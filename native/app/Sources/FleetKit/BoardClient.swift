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

    /// Starts (or restarts, on another socket) the connection loop.
    public func start(socket url: URL) {
        stop()
        task = Task { await self.run(url) }
    }

    public func stop() {
        task?.cancel()
        task = nil
        socket?.close()
        socket = nil
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

    private func run(_ url: URL) async {
        var backoff = Self.backoffFloor
        // A Hello with no since asks for a Snapshot: after the board lost step it must not resume from where it was.
        var fresh = false
        while !Task.isCancelled {
            let sock: FrameSocket
            do {
                sock = try FrameSocket.connect(url)
            } catch {
                set(error as? SocketError == .absent ? .absent : .reconnecting)
                try? await Task.sleep(for: backoff)
                backoff = min(backoff * 2, Self.backoffCeiling)
                continue
            }
            socket = sock
            if Task.isCancelled { break }  // stop() ran while connecting
            let hello: Frame
            if fresh {
                hello = .hello(proto: Frame.proto, since: nil, run: nil, role: .board)
            } else {
                hello = await board.hello()
            }
            fresh = false
            do {
                try await sock.send(hello)
                for try await frame in sock.frames {
                    if case let .error(message) = frame {
                        lastError = message
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
            set(.reconnecting)
            try? await Task.sleep(for: backoff)
            backoff = min(backoff * 2, Self.backoffCeiling)
        }
    }
}
