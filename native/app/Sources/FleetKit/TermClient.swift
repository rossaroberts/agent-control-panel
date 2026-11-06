// The app's one Terminals connection (native/PROTOCOL.md "Terminals connections"): every terminal the app shows is a
// `ref` on it. Output is read on the socket's own read path while writes wait on theirs (a write may block for as long
// as a program does not read its input), input goes in `TermIn`s of at most 64 KiB (a bigger one closes the connection
// and every terminal on it), and a lost connection is reconnected (50 ms up to 2 s) with every terminal that was open
// opened again at its last size: a kept-warm `claude attach` comes back with what it last printed.
//
// Output is bounded (Ruling P3-R5): a terminal's output not yet taken by its view is held up to `outputLimit` (1 MiB).
// Past that the client stops reading the socket until that terminal is down to half, so fleetd's own backpressure
// reaches the program, and nothing is dropped or reordered.

import Foundation
import Synchronization

/// A terminal's output, in order, never dropped: an `AsyncSequence` of chunks for one consumer, which finishes when the
/// terminal ends. What is held and not yet taken is `unread` bytes; taking it is what lets the client read on. Past
/// `limit` unread it is over, until a take brings it under half (then `onDrain` runs once).
public final class TermOutput: AsyncSequence, Sendable {
    public typealias Element = Data

    private struct State {
        var chunks: [Data] = []
        var head = 0
        var unread = 0
        var finished = false
        var over = false
        var waiter: CheckedContinuation<Data?, Never>?
    }

    private let state = Mutex(State())
    private let limit: Int
    private let onDrain: @Sendable () -> Void

    init(limit: Int, onDrain: @escaping @Sendable () -> Void) {
        self.limit = limit
        self.onDrain = onDrain
    }

    /// Bytes held that the consumer has not taken.
    public var unread: Int { state.withLock { $0.unread } }

    var isOver: Bool { state.withLock { $0.over } }

    /// Adds a chunk; whether this made it go over the limit.
    func append(_ d: Data) -> Bool {
        let (waiter, crossed): (CheckedContinuation<Data?, Never>?, Bool) = state.withLock { s in
            if let w = s.waiter {
                s.waiter = nil
                return (w, false)
            }
            s.chunks.append(d)
            s.unread += d.count
            guard !s.over, s.unread > limit else { return (nil, false) }
            s.over = true
            return (nil, true)
        }
        waiter?.resume(returning: d)
        return crossed
    }

    func finish() {
        let waiter = state.withLock { s -> CheckedContinuation<Data?, Never>? in
            s.finished = true
            defer { s.waiter = nil }
            return s.waiter
        }
        waiter?.resume(returning: nil)
    }

    private func take() async -> Data? {
        let (got, drained): (Data??, Bool) = state.withLock { s in
            guard s.head < s.chunks.count else { return (s.finished ? .some(nil) : nil, false) }
            let d = s.chunks[s.head]
            s.head += 1
            s.unread -= d.count
            // Taken chunks are let go of: all at once when it is empty, else in batches (a view that keeps up with a
            // steady flow may never see it empty).
            if s.head == s.chunks.count {
                s.chunks.removeAll(keepingCapacity: true)
                s.head = 0
            } else if s.head >= 256 {
                s.chunks.removeFirst(s.head)
                s.head = 0
            }
            guard s.over, s.unread < limit / 2 else { return (.some(d), false) }
            s.over = false
            return (.some(d), true)
        }
        if let got {
            if drained { onDrain() }
            return got
        }
        return await withCheckedContinuation { c in
            let now: Data?? = state.withLock { s in
                if s.head < s.chunks.count || s.finished { return .some(nil) }  // raced: take again
                s.waiter = c
                return nil
            }
            if now != nil { c.resume(returning: Data()) }  // an empty chunk: the iterator takes again
        }
    }

    public struct AsyncIterator: AsyncIteratorProtocol {
        let output: TermOutput

        public mutating func next() async -> Data? {
            while true {
                guard let d = await output.take() else { return nil }
                if !d.isEmpty { return d }
            }
        }
    }

    public func makeAsyncIterator() -> AsyncIterator { AsyncIterator(output: self) }
}

/// One open terminal, as `TermClient.open` hands it out.
public struct Terminal: Sendable {
    public let ref: UInt32
    /// The program's output (see `TermOutput`). Finishes when the terminal ends.
    public let output: TermOutput
    /// Yields once when the terminal ends, then finishes: nil for a plain end (the program exited, or `close`), else
    /// the daemon's words (a refused open, or "The session has ended." for a warm terminal whose program ended).
    public let end: AsyncStream<String?>
}

public actor TermClient {
    /// The most output one terminal holds unread before the client stops reading the socket (Ruling P3-R5); it reads
    /// again once that terminal is down to half.
    public static let outputLimit = 1024 * 1024

    private final class Open {
        let id: UInt64
        var target: AttachTarget
        var cols: UInt16
        var rows: UInt16
        let output: TermOutput
        let end: AsyncStream<String?>.Continuation

        init(id: UInt64, target: AttachTarget, cols: UInt16, rows: UInt16, output: TermOutput,
             end: AsyncStream<String?>.Continuation) {
            (self.id, self.target, self.cols, self.rows, self.output, self.end) = (id, target, cols, rows, output, end)
        }

        func finish(_ error: String?) {
            output.finish()
            end.yield(error)
            end.finish()
        }
    }

    /// The words of the last `Error` frame the daemon sent before closing the connection; cleared by a connection that
    /// works (one that the daemon has sent a terminal frame on).
    public private(set) var lastError: String?
    /// Whether the connection is up now.
    public private(set) var connected = false

    /// The open terminals by ref.
    private var terms: [UInt32: Open] = [:]
    /// Per ref, the terminals closed (or replaced) whose `TermEnd` the daemon still owes, oldest first. The daemon
    /// sends all of an earlier terminal of a ref (its output, then its end) before anything of the next one, so output
    /// and ends for a ref go to the oldest of these first.
    private var closing: [UInt32: [Open]] = [:]
    /// The terminals (by `Open.id`) holding more than `outputLimit` unread, until each is down to half.
    private var over: Set<UInt64> = []
    private var nextId: UInt64 = 0
    private var socket: FrameSocket?
    private var task: Task<Void, Never>?

    public init() {}

    /// `data` as `TermIn` frames of at most `Frame.termInMax` (64 KiB) each, in order.
    public static func inputFrames(ref: UInt32, data: Data) -> [Frame] {
        stride(from: data.startIndex, to: data.endIndex, by: Frame.termInMax).map {
            .termIn(ref: ref, data: Data(data[$0..<min($0 + Frame.termInMax, data.endIndex)]))
        }
    }

    public func start(socket url: URL) {
        stop()
        task = Task { await self.run(url) }
    }

    /// Ends the connection and every terminal (each `end` yields nil). The daemon releases them as at a hang-up.
    public func stop() {
        task?.cancel()
        task = nil
        socket?.close()
        socket = nil
        connected = false
        dropAll()
    }

    /// Opens terminal `ref` (> 0) on `target`. Sent now if connected, else when the connection comes up. A `ref` that
    /// is open already is closed first (its `end` yields when the daemon answers), and the new terminal takes its place.
    public func open(_ ref: UInt32, target: AttachTarget, cols: UInt16, rows: UInt16) -> Terminal {
        retire(ref)
        nextId += 1
        let id = nextId
        let output = TermOutput(limit: Self.outputLimit) { [weak self] in
            guard let self else { return }
            Task { await self.drained(id) }
        }
        let (end, ends) = AsyncStream<String?>.makeStream()
        terms[ref] = Open(id: id, target: target, cols: cols, rows: rows, output: output, end: ends)
        post([.termOpen(ref: ref, target: target, cols: cols, rows: rows)])
        return Terminal(ref: ref, output: output, end: end)
    }

    /// Keystrokes or a paste, split into `TermIn`s of at most 64 KiB that go out back to back. Returns once they are on
    /// the socket (which waits while the program does not read). Input for a terminal that is not open, or typed while
    /// the connection is down, is not sent: the terminal is opened again when the connection is back.
    public func input(_ ref: UInt32, _ data: Data) async {
        guard terms[ref] != nil, !data.isEmpty, let sock = socket else { return }
        try? await sock.send(Self.inputFrames(ref: ref, data: data))
    }

    public func size(_ ref: UInt32, cols: UInt16, rows: UInt16) {
        guard let t = terms[ref] else { return }
        t.cols = cols
        t.rows = rows
        post([.termSize(ref: ref, cols: cols, rows: rows)])
    }

    /// Releases terminal `ref`: its `end` yields when the daemon answers (at once when the connection is down).
    public func close(_ ref: UInt32) {
        retire(ref)
    }

    /// The open terminal of `ref`, if any, is closed: with a `TermClose` whose `TermEnd` is then owed (connected), or
    /// at once.
    private func retire(_ ref: UInt32) {
        guard let t = terms.removeValue(forKey: ref) else { return }
        if socket == nil {
            end(t, nil)
        } else {
            closing[ref, default: []].append(t)
            post([.termClose(ref: ref)])
        }
    }

    private func end(_ t: Open, _ error: String?) {
        t.finish(error)
        if over.remove(t.id) != nil, over.isEmpty { socket?.resume() }
    }

    /// Every terminal ends (the connection is gone or stopped); the closing ones were released with it.
    private func dropAll() {
        for t in terms.values { end(t, nil) }
        for list in closing.values { for t in list { end(t, nil) } }
        terms = [:]
        closing = [:]
        over = []
    }

    /// Terminal `id`'s view took output and it is down to half the limit. (Told from a task: by now it may have gone
    /// over again, and then it stays held.)
    private func drained(_ id: UInt64) {
        let t = terms.values.first { $0.id == id } ?? closing.values.lazy.flatMap { $0 }.first { $0.id == id }
        if let t, t.output.isOver { return }
        if over.remove(id) != nil, over.isEmpty { socket?.resume() }
    }

    /// Whom a frame for `ref` is for: the oldest terminal of it still owed an end, else the open one.
    private func target(of ref: UInt32) -> Open? { closing[ref]?.first ?? terms[ref] }

    private func deliver(_ ref: UInt32, _ data: Data, on sock: FrameSocket) {
        guard let t = target(of: ref) else { return }
        if t.output.append(data) {
            over.insert(t.id)
            sock.pause()
        }
    }

    private func ended(_ ref: UInt32, _ error: String?) {
        if var list = closing[ref], !list.isEmpty {
            let t = list.removeFirst()
            closing[ref] = list.isEmpty ? nil : list
            end(t, error)
        } else if let t = terms.removeValue(forKey: ref) {
            end(t, error)
        }
    }

    /// Queues frames on the connection, in call order with every other write of this client.
    private func post(_ frames: [Frame]) {
        socket?.enqueue(frames)
    }

    private func run(_ url: URL) async {
        var backoff = BoardClient.backoffFloor
        while !Task.isCancelled {
            guard let sock = try? FrameSocket.connect(url) else {
                try? await Task.sleep(for: backoff)
                backoff = min(backoff * 2, BoardClient.backoffCeiling)
                continue
            }
            if Task.isCancelled {
                sock.close()
                break
            }
            // The Hello and every open terminal's TermOpen, in one write, ahead of anything sent from here on.
            let hello = Frame.hello(proto: Frame.proto, since: nil, run: nil, role: .terminals)
            let reopen = terms.sorted { $0.key < $1.key }.map {
                Frame.termOpen(ref: $0.key, target: $0.value.target, cols: $0.value.cols, rows: $0.value.rows)
            }
            sock.enqueue([hello] + reopen)
            socket = sock
            connected = true
            // A view still behind from before holds this connection too, until it catches up.
            over = Set(terms.values.filter { $0.output.isOver }.map(\.id))
            if !over.isEmpty { sock.pause() }
            do {
                for try await frame in sock.frames {
                    switch frame {
                    case let .error(message):
                        lastError = message
                        continue
                    case let .termOut(ref, data):
                        deliver(ref, data, on: sock)
                    case let .termEnd(ref, error):
                        ended(ref, error)
                    default:
                        continue
                    }
                    // A terminal frame: the connection works (the daemon took our Hello).
                    backoff = BoardClient.backoffFloor
                    lastError = nil
                }
            } catch {}
            sock.close()
            if socket === sock {
                socket = nil
                connected = false
                // The daemon released this connection's terminals: the owed ends will not come, and the open ones are
                // opened again on the next connection.
                for list in closing.values { for t in list { end(t, nil) } }
                closing = [:]
            }
            if Task.isCancelled { break }
            try? await Task.sleep(for: backoff)
            backoff = min(backoff * 2, BoardClient.backoffCeiling)
        }
    }
}
