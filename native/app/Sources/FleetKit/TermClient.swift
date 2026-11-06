// The app's one Terminals connection (native/PROTOCOL.md "Terminals connections"): every terminal the app shows is a
// `ref` on it. Output is read on the socket's own read path while writes wait on theirs (a write may block for as long
// as a program does not read its input), input goes in `TermIn`s of at most 64 KiB (a bigger one closes the connection
// and every terminal on it), and a lost connection is reconnected (50 ms up to 2 s) with every terminal that was open
// opened again at its last size: a kept-warm `claude attach` comes back with what it last printed.

import Foundation

/// One open terminal, as `TermClient.open` hands it out.
public struct Terminal: Sendable {
    public let ref: UInt32
    /// The program's output, in order, never dropped. Finishes when the terminal ends.
    public let output: AsyncStream<Data>
    /// Yields once when the terminal ends, then finishes: nil for a plain end (the program exited, or `close`), else
    /// the daemon's words (a refused open, or "The session has ended." for a warm terminal whose program ended).
    public let end: AsyncStream<String?>
}

public actor TermClient {
    private struct Open {
        var target: AttachTarget
        var cols: UInt16
        var rows: UInt16
        let output: AsyncStream<Data>.Continuation
        let end: AsyncStream<String?>.Continuation

        func finish(_ error: String?) {
            output.finish()
            end.yield(error)
            end.finish()
        }
    }

    /// The words of the last `Error` frame the daemon sent before closing the connection.
    public private(set) var lastError: String?
    /// Whether the connection is up now.
    public private(set) var connected = false

    private var terms: [UInt32: Open] = [:]
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
        for t in terms.values { t.finish(nil) }
        terms = [:]
    }

    /// Opens terminal `ref` (> 0, unique among the open ones) on `target`. Sent now if connected, else when the
    /// connection comes up. Opening a `ref` that is open already hands back nothing new: it is closed first.
    public func open(_ ref: UInt32, target: AttachTarget, cols: UInt16, rows: UInt16) -> Terminal {
        if let old = terms.removeValue(forKey: ref) { old.finish(nil) }
        let (output, out) = AsyncStream<Data>.makeStream()
        let (end, ends) = AsyncStream<String?>.makeStream()
        terms[ref] = Open(target: target, cols: cols, rows: rows, output: out, end: ends)
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
        guard terms[ref] != nil else { return }
        terms[ref]?.cols = cols
        terms[ref]?.rows = rows
        post([.termSize(ref: ref, cols: cols, rows: rows)])
    }

    /// Releases terminal `ref`: its `end` yields when the daemon answers (at once when the connection is down).
    public func close(_ ref: UInt32) {
        guard terms[ref] != nil else { return }
        if socket == nil {
            terms.removeValue(forKey: ref)?.finish(nil)
        } else {
            post([.termClose(ref: ref)])
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
            if Task.isCancelled { sock.close(); break }
            // The Hello and every open terminal's TermOpen, in one write, ahead of anything sent from here on.
            let hello = Frame.hello(proto: Frame.proto, since: nil, run: nil, role: .terminals)
            let reopen = terms.sorted { $0.key < $1.key }.map {
                Frame.termOpen(ref: $0.key, target: $0.value.target, cols: $0.value.cols, rows: $0.value.rows)
            }
            sock.enqueue([hello] + reopen)
            socket = sock
            connected = true
            backoff = BoardClient.backoffFloor
            do {
                for try await frame in sock.frames {
                    switch frame {
                    case let .termOut(ref, data):
                        terms[ref]?.output.yield(data)
                    case let .termEnd(ref, error):
                        terms.removeValue(forKey: ref)?.finish(error)
                    case let .error(message):
                        lastError = message
                    default:
                        break
                    }
                }
            } catch {}
            sock.close()
            if socket === sock {
                socket = nil
                connected = false
            }
            if Task.isCancelled { break }
            try? await Task.sleep(for: backoff)
            backoff = min(backoff * 2, BoardClient.backoffCeiling)
        }
    }
}
