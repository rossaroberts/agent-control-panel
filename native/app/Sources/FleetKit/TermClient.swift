// The app's one Terminals connection (native/PROTOCOL.md "Terminals connections"): every terminal the app shows is a
// `ref` on it. Output is read on the socket's own read path while writes wait on theirs (a write may block for as long
// as a program does not read its input), input goes in `TermIn`s of at most 64 KiB (a bigger one closes the connection
// and every terminal on it), and a lost connection is reconnected (50 ms up to 2 s) with every terminal that was open
// opened again at its last size: a kept-warm `claude attach` comes back with what it last printed.
//
// Output is bounded (Ruling P3-R5): a terminal's output not yet taken by its view is held up to `outputLimit` (1 MiB).
// Past that the client stops reading the socket until that terminal is down to half, so fleetd's own backpressure
// reaches the program, and nothing is dropped or reordered. So a live terminal's view must keep draining its output:
// one that stops holds every terminal of the connection behind it, by design (as fleetd holds a connection whose reader
// stops). A terminal that is closed, replaced, or whose view is cancelled or let go of is retired at once: its held
// output and anything more it is sent are discarded, so it holds nothing up.

import Foundation
import Synchronization

/// A terminal's held output, the client's side of it. What is held and not yet taken is `unread` bytes. Past `limit`
/// unread it is over, until a take brings it under half (then `onDrain` runs once). Once discarded it holds nothing and
/// takes nothing more.
final class OutputBuffer: Sendable {
    private struct State {
        var chunks: [Data] = []
        var head = 0
        var unread = 0
        var finished = false
        var over = false
        var discarded = false
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
    var unread: Int { state.withLock { $0.unread } }

    var isOver: Bool { state.withLock { $0.over } }

    /// Adds a chunk; whether this made it go over the limit.
    func append(_ d: Data) -> Bool {
        let (waiter, crossed): (CheckedContinuation<Data?, Never>?, Bool) = state.withLock { s in
            if s.discarded { return (nil, false) }
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

    /// Drops what is held and anything sent later; the consumer's next take ends it.
    func discard() {
        let waiter = state.withLock { s -> CheckedContinuation<Data?, Never>? in
            s.discarded = true
            s.finished = true
            s.over = false
            s.chunks = []
            s.head = 0
            s.unread = 0
            defer { s.waiter = nil }
            return s.waiter
        }
        waiter?.resume(returning: nil)
    }

    /// A take that is waiting gives up (its consumer was cancelled).
    func cancelWait() {
        let waiter = state.withLock { s -> CheckedContinuation<Data?, Never>? in
            defer { s.waiter = nil }
            return s.waiter
        }
        waiter?.resume(returning: nil)
    }

    func take() async -> Data? {
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

}

/// A terminal's output, in order, never dropped: an `AsyncSequence` of chunks for one consumer, which finishes when the
/// terminal ends. Taking it is what lets the client read on (`unread` is what is held). A consumer that is cancelled,
/// or lets go of this (and of its `Terminal`), closes the terminal: an abandoned view must not hold the connection.
public final class TermOutput: AsyncSequence, Sendable {
    public typealias Element = Data

    let buffer: OutputBuffer
    private let onAbandon: @Sendable () -> Void

    init(_ buffer: OutputBuffer, onAbandon: @escaping @Sendable () -> Void) {
        self.buffer = buffer
        self.onAbandon = onAbandon
    }

    deinit { onAbandon() }

    /// Bytes held that the consumer has not taken.
    public var unread: Int { buffer.unread }

    public struct AsyncIterator: AsyncIteratorProtocol {
        let output: TermOutput

        public mutating func next() async -> Data? {
            while true {
                if Task.isCancelled {
                    output.onAbandon()
                    return nil
                }
                let buffer = output.buffer
                let d = await withTaskCancellationHandler {
                    await buffer.take()
                } onCancel: {
                    buffer.cancelWait()
                }
                guard let d else {
                    if Task.isCancelled { output.onAbandon() }
                    return nil
                }
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
    /// The most input one terminal holds while the connection is down (Ruling A-R2); past it `input` refuses.
    public static let heldMax = 64 * 1024

    /// What `input` did with the keys.
    public enum InputResult: Equatable, Sendable {
        /// On the socket.
        case sent
        /// The connection is down: held, and sent after the terminal is opened again on the next connection.
        case held
        /// Refused, nothing of it taken: `heldMax` is held already (the client's InputFull; type again once back).
        case inputFull
        /// No such terminal open.
        case notOpen
    }

    private final class Open {
        let id: UInt64
        var target: AttachTarget
        var cols: UInt16
        var rows: UInt16
        let output: OutputBuffer
        let end: AsyncStream<String?>.Continuation
        /// Keystrokes typed while the connection was down (Ruling A-R2, P2-R4: never dropped), sent right after this
        /// terminal's TermOpen on the next connection. At most `heldMax`.
        var held = Data()

        init(id: UInt64, target: AttachTarget, cols: UInt16, rows: UInt16, output: OutputBuffer,
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

    private var watchers: [UUID: AsyncStream<Bool>.Continuation] = [:]

    /// Whether the Terminals connection is up: now, then at every change (a view shows "reconnecting…" while it is
    /// down; the terminals are opened again when it is back).
    public func connections() -> AsyncStream<Bool> {
        let (stream, c) = AsyncStream<Bool>.makeStream()
        let id = UUID()
        watchers[id] = c
        c.yield(connected)
        c.onTermination = { [weak self] _ in
            guard let self else { return }
            Task { await self.unwatch(id) }
        }
        return stream
    }

    private func unwatch(_ id: UUID) { watchers[id] = nil }

    private func setConnected(_ up: Bool) {
        guard up != connected else { return }
        connected = up
        for c in watchers.values { c.yield(up) }
    }

    /// `data` as `TermIn` frames of at most `Frame.termInMax` (64 KiB) each, in order.
    public static func inputFrames(ref: UInt32, data: Data) -> [Frame] {
        stride(from: data.startIndex, to: data.endIndex, by: Frame.termInMax).map {
            .termIn(ref: ref, data: Data(data[$0..<min($0 + Frame.termInMax, data.endIndex)]))
        }
    }

    public func start(socket url: URL) {
        start(endpoint: .unix(url))
    }

    /// As `start(socket:)`, on any endpoint: a Unix socket, or another machine's fleetd over TCP.
    public func start(endpoint: Endpoint) {
        stop()
        task = Task { await self.run(endpoint) }
    }

    /// Ends the connection and every terminal (each `end` yields nil). The daemon releases them as at a hang-up.
    public func stop() {
        task?.cancel()
        task = nil
        socket?.close()
        socket = nil
        setConnected(false)
        dropAll()
    }

    /// Opens terminal `ref` (> 0) on `target`. Sent now if connected, else when the connection comes up. A `ref` that
    /// is open already is closed first (its `end` yields when the daemon answers), and the new terminal takes its place.
    public func open(_ ref: UInt32, target: AttachTarget, cols: UInt16, rows: UInt16) -> Terminal {
        retire(ref)
        nextId += 1
        let id = nextId
        let output = OutputBuffer(limit: Self.outputLimit) { [weak self] in
            guard let self else { return }
            Task { await self.drained(id) }
        }
        let view = TermOutput(output) { [weak self] in
            guard let self else { return }
            Task { await self.abandoned(id) }
        }
        let (end, ends) = AsyncStream<String?>.makeStream()
        terms[ref] = Open(id: id, target: target, cols: cols, rows: rows, output: output, end: ends)
        post([.termOpen(ref: ref, target: target, cols: cols, rows: rows)])
        return Terminal(ref: ref, output: view, end: end)
    }

    /// Keystrokes or a paste, split into `TermIn`s of at most 64 KiB that go out back to back. Returns once they are on
    /// the socket (which waits while the program does not read). Typed while the connection is down, they are held
    /// (Ruling A-R2: never dropped, P2-R4) and sent, in order, right after the terminal's TermOpen on the next
    /// connection; past `heldMax` held, the input is refused whole (`inputFull`).
    @discardableResult
    public func input(_ ref: UInt32, _ data: Data) async -> InputResult {
        guard let t = terms[ref] else { return .notOpen }
        if data.isEmpty { return .sent }
        guard let sock = socket, t.held.isEmpty else { return hold(t, data) }
        do {
            try await sock.send(Self.inputFrames(ref: ref, data: data))
            return .sent
        } catch {
            // The connection went while these were on their way: held for the next one rather than lost.
            guard terms[ref] === t else { return .notOpen }
            return hold(t, data)
        }
    }

    private func hold(_ t: Open, _ data: Data) -> InputResult {
        guard t.held.count + data.count <= Self.heldMax else { return .inputFull }
        t.held.append(data)
        return .held
    }

    /// Closes the connection now and connects again (the usual backoff from its floor), every open terminal opened
    /// again at its last size, with no end reaching its view: as when the daemon restarts (Ruling A-R2, for a phone
    /// back from the background). Not `stop()`: the terminals are kept.
    public func reconnect() {
        socket?.close()
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
    /// Its output is discarded at once (what is held, and what the daemon sends before its end): a closed terminal's
    /// bytes are not wanted, and its view no longer reads, so it must not hold the connection.
    private func retire(_ ref: UInt32) {
        guard let t = terms.removeValue(forKey: ref) else { return }
        t.output.discard()
        if over.remove(t.id) != nil, over.isEmpty { socket?.resume() }
        if socket == nil {
            end(t, nil)
        } else {
            closing[ref, default: []].append(t)
            post([.termClose(ref: ref)])
        }
    }

    /// Terminal `id`'s view was cancelled or let go of: it is closed, if it is still open.
    private func abandoned(_ id: UInt64) {
        guard let ref = terms.first(where: { $0.value.id == id })?.key else { return }
        retire(ref)
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

    private func run(_ endpoint: Endpoint) async {
        var backoff = BoardClient.backoffFloor
        while !Task.isCancelled {
            // Off the actor: while a dead host takes its time, input, open and stop() still answer.
            guard let sock = try? await FrameSocket.connectOffActor(endpoint) else {
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
            // Each terminal's held keys go right behind its TermOpen (the daemon opens it before reading on).
            let reopen = terms.sorted { $0.key < $1.key }.flatMap { ref, t -> [Frame] in
                let held = t.held
                t.held = Data()
                return [Frame.termOpen(ref: ref, target: t.target, cols: t.cols, rows: t.rows)]
                    + Self.inputFrames(ref: ref, data: held)
            }
            sock.enqueue([hello] + reopen)
            socket = sock
            setConnected(true)
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
                setConnected(false)
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
