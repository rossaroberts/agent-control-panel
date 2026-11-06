// The board connection (native/PROTOCOL.md "The conversation" and "Rules a client relies on"): connect, say the Board's
// own Hello (its run and since, so a reconnect to the same daemon is sent only what changed), and apply every frame to
// the Board on the main actor. A lost connection, or a frame the board cannot apply (a seq gap), reconnects, backing off
// from 50 ms up to 2 s. The app never starts fleetd: with no socket the status is `.absent` and the client keeps
// trying, at most every 2 s, at no cost in between.

import Foundation

public actor BoardClient {
    public enum Status: Equatable, Sendable {
        /// Connected, and the board is current.
        case live
        /// The connection was lost (or refused a frame), and the client is reconnecting.
        case reconnecting
        /// No daemon at the socket ("fleetd not running").
        case absent
        /// The daemon answered with an `Error` and closed (its words: a version mismatch, say). The client keeps
        /// trying, every 2 s at most, in case a matching daemon takes its place.
        case refused(String)
    }

    /// The first wait before a reconnect, and the longest.
    public static let backoffFloor: Duration = .milliseconds(50)
    public static let backoffCeiling: Duration = .seconds(2)

    public nonisolated let board: Board
    public private(set) var status: Status = .reconnecting
    /// The words of the last `Error` frame the daemon sent (a version mismatch, say), until the next live connection.
    public private(set) var lastError: String?

    private var task: Task<Void, Never>?
    private var socket: FrameSocket?
    private var watchers: [UUID: AsyncStream<Status>.Continuation] = [:]

    public init(board: Board) {
        self.board = board
    }

    deinit {
        for w in watchers.values { w.finish() }
        socket?.close()
    }

    /// Starts (or restarts, on another socket) the connection loop.
    public func start(socket url: URL) {
        start(endpoint: .unix(url))
    }

    /// Starts (or restarts, on another endpoint) the connection loop: a Unix socket, or another machine's fleetd over
    /// TCP. The conversation, the backoff and the statuses are the same for both.
    public func start(endpoint: Endpoint) {
        halt()
        task = Task { await self.run(endpoint) }
    }

    /// Ends the connection, and every `statuses()` stream.
    public func stop() {
        halt()
        for w in watchers.values { w.finish() }
        watchers = [:]
    }

    private func halt() {
        task?.cancel()
        task = nil
        socket?.close()
        socket = nil
    }

    /// Closes the connection now and connects again, resuming from the board's run and seq (only what changed is sent):
    /// as after a lost connection (Ruling A-R2). Not `stop()`.
    public func reconnect() {
        socket?.close()
    }

    /// The status now, then each change.
    public func statuses() -> AsyncStream<Status> {
        let (stream, sink) = AsyncStream<Status>.makeStream()
        let id = UUID()
        sink.yield(status)
        watchers[id] = sink
        sink.onTermination = { [weak self] _ in Task { await self?.unwatch(id) } }
        return stream
    }

    private func unwatch(_ id: UUID) { watchers[id] = nil }

    private func set(_ s: Status) {
        guard s != status else { return }
        status = s
        for w in watchers.values { w.yield(s) }
    }

    private func run(_ endpoint: Endpoint) async {
        var backoff = Self.backoffFloor
        // A Hello with no since asks for a Snapshot: after the board lost step it must not resume from where it was.
        var fresh = false
        while !Task.isCancelled {
            let sock: FrameSocket
            do {
                // Off the actor: while a dead host takes its time, status, stop() and start(endpoint:) still answer.
                sock = try await FrameSocket.connectOffActor(endpoint)
            } catch {
                // A loop stopped (or replaced by a new start) while connecting says nothing more: the status is no
                // longer its to set.
                if Task.isCancelled { break }
                set(error as? SocketError == .absent ? .absent : .reconnecting)
                try? await Task.sleep(for: backoff)
                backoff = min(backoff * 2, Self.backoffCeiling)
                continue
            }
            if Task.isCancelled {  // stop() ran while connecting
                sock.close()
                break
            }
            socket = sock
            let hello: Frame
            if fresh {
                hello = .hello(proto: Frame.proto, since: nil, run: nil, role: .board)
            } else {
                hello = await board.hello()
            }
            fresh = false
            var refusal: String?
            // Queued, not awaited: a daemon that refuses may close before the Hello is all written, and its Error is
            // still there to read.
            sock.enqueue([hello])
            do {
                for try await frame in sock.frames {
                    if case let .error(message) = frame {
                        lastError = message
                        refusal = message
                        break
                    }
                    if await board.apply(frame) == .needsResync {
                        fresh = true
                        break
                    }
                    if status != .live {
                        lastError = nil
                        backoff = Self.backoffFloor
                        set(.live)
                    }
                }
            } catch {}
            sock.close()
            if socket === sock { socket = nil }
            if Task.isCancelled { break }
            set(refusal.map(Status.refused) ?? .reconnecting)
            try? await Task.sleep(for: backoff)
            backoff = min(backoff * 2, Self.backoffCeiling)
        }
    }
}
