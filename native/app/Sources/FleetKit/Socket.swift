// One framed connection to fleetd's Unix socket (native/PROTOCOL.md "Transport"). Reads and writes run on independent
// paths, as the protocol requires of a Terminals client: a thread of its own blocks in read(2) and hands each frame on
// at once (no polling, so an idle connection costs nothing), and writes go through a serial queue of their own, so a
// write that waits on the daemon's backpressure never stops output being read.
//
// Raw sockets rather than Network.framework: a blocking read on a dedicated thread is the shortest path from the
// kernel to the decoder (no event-loop hop), and a Unix endpoint needs nothing NWConnection adds.

import Darwin
import Foundation
import Synchronization

public enum SocketError: Error, Equatable, Sendable {
    /// No daemon: the socket file is not there, or nothing listens on it (a stale file).
    case absent
    /// The path does not fit a `sockaddr_un`.
    case pathTooLong
    /// The connection is closed (by `close()` or by the daemon).
    case closed
    /// Any other failure, by errno.
    case failed(errno: Int32)
}

/// A file descriptor closed exactly once, and never used after (its number may be handed out again).
private final class Descriptor: Sendable {
    private let fd: Mutex<Int32?>

    init(_ fd: Int32) { self.fd = Mutex(fd) }

    /// Ends both directions: a blocked read returns 0, a blocked write fails. The fd stays open until `close`.
    func shutdown() { fd.withLock { if let f = $0 { _ = Darwin.shutdown(f, SHUT_RDWR) } } }

    func close() {
        fd.withLock {
            if let f = $0 { _ = Darwin.close(f) }
            $0 = nil
        }
    }

    /// The fd, or nil once closed. Held only for the call: a blocking call made with it does not hold the lock.
    var value: Int32? { fd.withLock { $0 } }
}

/// Holds the reader thread before its next read(2) while the consumer is behind (`FrameSocket.pause`).
private final class ReadGate: Sendable {
    private let cond = NSCondition()
    private nonisolated(unsafe) var paused = false
    private nonisolated(unsafe) var shut = false

    func set(paused p: Bool) {
        cond.lock()
        paused = p
        cond.broadcast()
        cond.unlock()
    }

    func shutDown() {
        cond.lock()
        shut = true
        cond.broadcast()
        cond.unlock()
    }

    /// Waits while paused (and not shut down).
    func pass() {
        cond.lock()
        while paused && !shut { cond.wait() }
        cond.unlock()
    }
}

public final class FrameSocket: Sendable {
    /// Every frame the daemon sends, in order. Finishes when the connection ends (cleanly at EOF, else with the error).
    public let frames: AsyncThrowingStream<Frame, Error>

    private let fd: Descriptor
    private let gate = ReadGate()
    private let writes = DispatchQueue(label: "FleetKit.FrameSocket.write", qos: .userInteractive)

    private init(fd raw: Int32) {
        let fd = Descriptor(raw)
        self.fd = fd
        let (stream, sink) = AsyncThrowingStream<Frame, Error>.makeStream()
        frames = stream
        let writes = self.writes
        let gate = self.gate
        let reader = Thread {
            Self.readLoop(raw, gate, sink)
            // Closed on the write queue, behind any write still queued (they fail fast once the socket is shut).
            fd.shutdown()
            writes.async { fd.close() }
        }
        reader.name = "FleetKit.FrameSocket.read"
        reader.qualityOfService = .userInteractive
        reader.start()
    }

    deinit {
        fd.shutdown()
        gate.shutDown()
    }

    /// Connects to the socket at `url`. Throws `SocketError.absent` when no daemon is there.
    public static func connect(_ url: URL) throws -> FrameSocket {
        var addr = sockaddr_un()
        let path = Array(url.path.utf8)
        guard path.count < MemoryLayout.size(ofValue: addr.sun_path) else { throw SocketError.pathTooLong }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw SocketError.failed(errno: errno) }
        // macOS has no SOCK_CLOEXEC: until this fcntl a process forked by another thread of the app could inherit the
        // socket, and keep the connection open after the app closed it (the daemon would not see the hang-up). The app
        // starts no processes of its own; anything that does fork while a client connects should use posix_spawn with
        // POSIX_SPAWN_CLOEXEC_DEFAULT.
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        var one: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path) }
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if r != 0 {
            let e = errno
            Darwin.close(fd)
            throw e == ENOENT || e == ECONNREFUSED ? SocketError.absent : SocketError.failed(errno: e)
        }
        return FrameSocket(fd: fd)
    }

    /// Queues `frames` to be written back to back (nothing queued meanwhile goes between them), behind everything
    /// queued before; `then` is told when they are on the socket, or why not. It returns at once: the order of the
    /// frames on the wire is the order of the `enqueue` calls, whatever thread or actor makes them.
    public func enqueue(_ frames: [Frame], then: (@Sendable (Error?) -> Void)? = nil) {
        var bytes = Data()
        do {
            for f in frames { bytes.append(try f.encode()) }
        } catch {
            then?(error)
            return
        }
        writes.async { [bytes] in
            do {
                try self.writeAll(bytes)
                then?(nil)
            } catch {
                then?(error)
            }
        }
    }

    /// Writes one frame and returns once it is on the socket, which may wait for as long as the daemon does not read
    /// (its backpressure); reading carries on meanwhile. Queued before the first suspension, so in call order from
    /// any one actor.
    public func send(_ frame: Frame, isolation: isolated (any Actor)? = #isolation) async throws {
        try await send([frame])
    }

    /// `frames` back to back, as `enqueue`, awaiting their write.
    public func send(_ frames: [Frame], isolation: isolated (any Actor)? = #isolation) async throws {
        try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
            enqueue(frames) { e in
                if let e { c.resume(throwing: e) } else { c.resume() }
            }
        }
    }

    /// Ends the connection: `frames` finishes and later sends throw `SocketError.closed`.
    public func close() {
        fd.shutdown()
        gate.shutDown()
    }

    /// Stops reading the socket (after the read in progress) until `resume`: the daemon's backpressure then reaches
    /// whatever it is sending. Frames already read are still delivered. Writes are not affected.
    public func pause() { gate.set(paused: true) }

    public func resume() { gate.set(paused: false) }

    private func writeAll(_ bytes: Data) throws {
        guard let f = fd.value else { throw SocketError.closed }
        try bytes.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            var at = 0
            while at < buf.count {
                let n = Darwin.write(f, buf.baseAddress! + at, buf.count - at)
                if n < 0 {
                    if errno == EINTR { continue }
                    throw errno == EPIPE || errno == ENOTCONN ? SocketError.closed : SocketError.failed(errno: errno)
                }
                at += n
            }
        }
    }

    /// Reads frames until EOF or an error: each `u32` little-endian length, then its body, decoded and yielded.
    /// The read buffer's size; it grows for a bigger frame and comes back to this once that frame is consumed.
    private static let readSize = 256 * 1024

    private static func readLoop(
        _ fd: Int32, _ gate: ReadGate, _ sink: AsyncThrowingStream<Frame, Error>.Continuation
    ) {
        var buf = [UInt8](repeating: 0, count: readSize)
        var have = 0  // bytes in buf[0..<have]
        var start = 0  // the first byte not yet parsed
        while true {
            // The bytes the frame at `start` needs, when it is not all here yet.
            var need = 0
            // Every whole frame in what has been read.
            while have - start >= 4 {
                let n = Int(buf[start]) | Int(buf[start + 1]) << 8 | Int(buf[start + 2]) << 16 | Int(buf[start + 3]) << 24
                guard n <= Int(Frame.maxFrame) else {
                    sink.finish(throwing: PostcardError.frameTooLarge(n))
                    return
                }
                guard have - start >= 4 + n else {
                    need = 4 + n
                    if 4 + n > buf.count { buf += [UInt8](repeating: 0, count: 4 + n - buf.count) }
                    break
                }
                do {
                    sink.yield(try Frame.decode(Data(buf[(start + 4)..<(start + 4 + n)])))
                } catch {
                    sink.finish(throwing: error)
                    return
                }
                start += 4 + n
            }
            // What is left of a frame moves to the front, so the next read has room behind it.
            if start > 0 {
                if have > start { buf.withUnsafeMutableBufferPointer { p in
                    _ = memmove(p.baseAddress!, p.baseAddress! + start, have - start)
                } }
                have -= start
                start = 0
            }
            if buf.count > readSize && need <= readSize && have <= readSize {
                buf = Array(buf[0..<have]) + [UInt8](repeating: 0, count: readSize - have)
            }
            gate.pass()
            let r = buf.withUnsafeMutableBufferPointer { p in Darwin.read(fd, p.baseAddress! + have, p.count - have) }
            if r > 0 {
                have += r
            } else if r == 0 {
                sink.finish()
                return
            } else if errno != EINTR {
                sink.finish(throwing: SocketError.failed(errno: errno))
                return
            }
        }
    }
}
