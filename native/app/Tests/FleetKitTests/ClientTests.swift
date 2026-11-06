// The connections against a real `fleetd serve` (built from this repo) over a synthetic home: a profile listing three
// interactive sessions run by this test process, and a stand-in `claude` that answers the roster and, for `attach`,
// prints a banner and echoes what it is sent (in raw mode, byte for byte). The daemon is started by the test with
// FLEETD_HOME / FLEETD_SOCK in a temp folder, and only that pid is ever signalled.

import Darwin
import Foundation
import Testing

@testable import FleetKit

/// `native/`, from this file's place in the tree.
private let nativeDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    .deletingLastPathComponent().deletingLastPathComponent()

/// The debug `fleetd`, built once per test run (a no-op when it is current).
private let fleetdBinary: URL = {
    let cargo = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".cargo/bin/cargo")
    let p = Process()
    p.executableURL = cargo
    p.arguments = ["build", "-q", "-p", "fleetd"]
    p.currentDirectoryURL = nativeDir
    try? p.run()
    p.waitUntilExit()
    precondition(p.terminationStatus == 0, "cargo build -p fleetd failed")
    return nativeDir.appendingPathComponent("target/debug/fleetd")
}()

private func sid(_ n: Int) -> String { "aaaaaaaa-0000-4000-8000-\(String(format: "%012d", n))" }

private func nowMs() -> Int64 { Int64((Date().timeIntervalSince1970 * 1000).rounded()) }

/// The job the stand-in serves in raw mode (any job starting with `e`).
private let rawJob = "e0000001"

/// A job whose stand-in floods (raw mode): after its banner, `floodWords` little-endian `u32`s counting from 0, then
/// it exits.
private let floodJob = "f0000001"
private let floodWords = 50 * 1024 * 1024 / 4
private let floodProgram =
    "import struct,sys\nw=sys.stdout.buffer\nfor i in range(0,\(floodWords),16384):"
    + " w.write(struct.pack(\"<16384I\",*range(i,i+16384)))\nw.flush()"

/// A synthetic home and the `fleetd serve` running over it.
private final class Fixture {
    let root: URL
    let socket: URL
    private var daemon: Process?

    init() throws {
        // Under /tmp, not $TMPDIR: a socket path must fit in 104 bytes.
        var tmpl = Array("/tmp/fleetkit-XXXXXX".utf8CString)
        guard let made = mkdtemp(&tmpl) else { throw SocketError.failed(errno: errno) }
        root = URL(fileURLWithPath: String(cString: made)).resolvingSymlinksInPath()
        socket = root.appendingPathComponent("fleetd.sock")
        let fm = FileManager.default
        let profile = root.appendingPathComponent(".claude")
        for d in ["fleet-state", "fleet-status", "jobs", "projects"] {
            try fm.createDirectory(at: profile.appendingPathComponent(d), withIntermediateDirectories: true)
        }
        for d in [".local/bin", "stand-in", "panel", "work"] {
            try fm.createDirectory(at: root.appendingPathComponent(d), withIntermediateDirectories: true)
        }
        try "{}".write(to: profile.appendingPathComponent("settings.json"), atomically: true, encoding: .utf8)
        let pid = getpid()
        let cwd = root.appendingPathComponent("work").path
        let rows = (0..<3).map {
            "{\"sessionId\":\"\(sid($0))\",\"pid\":\(pid),\"kind\":\"interactive\",\"status\":\"idle\","
                + "\"name\":\"fixture \($0)\",\"cwd\":\"\(cwd)\",\"startedAt\":1793900000000}"
        }
        let running = (0..<3).map { "{\"sessionId\":\"\(sid($0))\"}" }
        let standIn = root.appendingPathComponent("stand-in")
        try "[\(rows.joined(separator: ","))]".write(
            to: standIn.appendingPathComponent("all.json"), atomically: true, encoding: .utf8)
        try "[\(running.joined(separator: ","))]".write(
            to: standIn.appendingPathComponent("running.json"), atomically: true, encoding: .utf8)
        let script = """
            #!/bin/sh
            S='\(standIn.path)'
            FLOOD='\(floodProgram)'
            case "$1" in
              agents) case "$*" in *--all*) cat "$S/all.json";; *) cat "$S/running.json";; esac;;
              attach) case "$2" in
                  f*) stty raw -echo; printf 'BANNER %s\\r\\n' "$2"; exec /usr/bin/python3 -c "$FLOOD";;
                  e*) stty raw -echo;;
                esac
                printf 'BANNER %s\\r\\n' "$2"; exec cat;;
              *) exit 1;;
            esac

            """
        let bin = root.appendingPathComponent(".local/bin/claude")
        try script.write(to: bin, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bin.path)
    }

    deinit {
        stopDaemon()
        try? FileManager.default.removeItem(at: root)
    }

    func startDaemon() throws {
        let p = Process()
        p.executableURL = fleetdBinary
        p.arguments = ["serve"]
        p.environment = [
            "HOME": root.path, "FLEETD_HOME": root.path, "FLEETD_SOCK": socket.path,
            "FLEETD_PANEL_DIR": root.appendingPathComponent("panel").path, "FLEETD_MACHINE": "fixture-mac",
            "FLEETD_ROSTER": "cli", "PATH": root.appendingPathComponent(".local/bin").path + ":/usr/bin:/bin",
        ]
        let log = root.appendingPathComponent("fleetd.log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let h = try FileHandle(forWritingTo: log)
        h.seekToEndOfFile()
        p.standardError = h
        p.standardOutput = h
        try p.run()
        daemon = p
    }

    /// SIGTERM to the daemon this fixture started (by its pid), and its exit.
    func stopDaemon() {
        guard let p = daemon else { return }
        daemon = nil
        kill(p.processIdentifier, SIGTERM)
        p.waitUntilExit()
    }

    /// A hook event for session `n`, written as hook.sh writes it (a temp file renamed over `<session>.json`). Its `at`.
    func hook(_ n: Int, _ e: String) throws -> Int64 {
        let at = nowMs()
        let d = root.appendingPathComponent(".claude/fleet-state")
        let tmp = d.appendingPathComponent(".\(sid(n)).tmp")
        try "{\"e\":\"\(e)\",\"t\":null,\"n\":null,\"r\":null,\"i\":null,\"at\":\(at)}\n".write(
            to: tmp, atomically: false, encoding: .utf8)
        guard rename(tmp.path, d.appendingPathComponent("\(sid(n)).json").path) == 0 else {
            throw SocketError.failed(errno: errno)
        }
        return at
    }
}

/// Polls `cond` (on the main actor) every half millisecond until it holds or `seconds` pass; whether it held.
@MainActor
private func until(_ seconds: Double, _ cond: @MainActor () async -> Bool) async -> Bool {
    let end = ContinuousClock.now + .milliseconds(Int(seconds * 1000))
    while ContinuousClock.now < end {
        if await cond() { return true }
        try? await Task.sleep(for: .microseconds(500))
    }
    return await cond()
}

/// Collects a terminal's output, from its own task.
private final class Screen: @unchecked Sendable {
    private let lock = NSLock()
    private var bytes = Data()
    private var endedWith: String??

    init(_ t: Terminal) {
        Task { for await d in t.output { self.lock.withLock { self.bytes.append(d) } } }
        Task { for await e in t.end { self.lock.withLock { self.endedWith = .some(e) } } }
    }

    var data: Data { lock.withLock { bytes } }
    var ended: String?? { lock.withLock { endedWith } }

    func has(_ s: String) -> Bool { data.range(of: Data(s.utf8)) != nil }
}

/// Runs `body`, then `cleanup` whether or not it threw.
@MainActor
private func cleaning(
    _ body: @MainActor () async throws -> Void, then cleanup: @MainActor () async -> Void
) async throws {
    do {
        try await body()
    } catch {
        await cleanup()
        throw error
    }
    await cleanup()
}

/// This process's resident memory, in bytes.
private func residentBytes() -> UInt64 {
    var info = mach_task_basic_info()
    var count = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<natural_t>.size)
    let r = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &count)
        }
    }
    return r == KERN_SUCCESS ? info.resident_size : 0
}

/// A stand-in daemon that takes each connection, answers it with an `Error` (as fleetd does a Hello of another
/// version) and closes it, noting when each came.
private final class RefusingServer: @unchecked Sendable {
    private let fd: Int32
    private let lock = NSLock()
    private var times: [ContinuousClock.Instant] = []
    private var stopped = false
    private let done = DispatchSemaphore(value: 0)
    static let words = "protocol 9 not supported; this is proto 4"

    init(at url: URL) throws {
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let path = Array(url.path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path) }
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) == 0 && listen(fd, 16) == 0
            }
        }
        guard ok else { throw SocketError.failed(errno: errno) }
        let refusal = try Frame.error(message: Self.words).encode()
        Thread { [self] in
            while !lock.withLock({ stopped }) {
                var p = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
                guard poll(&p, 1, 20) == 1 else { continue }
                let c = accept(fd, nil, nil)
                guard c >= 0 else { continue }
                lock.withLock { times.append(.now) }
                _ = refusal.withUnsafeBytes { write(c, $0.baseAddress!, $0.count) }
                Darwin.close(c)
            }
            done.signal()
        }.start()
    }

    var attempts: [ContinuousClock.Instant] { lock.withLock { times } }

    func stop() {
        lock.withLock { stopped = true }
        done.wait()
        Darwin.close(fd)
    }
}

/// Watches only a terminal's end (its output is left unread).
private final class EndWatch: @unchecked Sendable {
    private let lock = NSLock()
    private var endedWith: String??

    init(_ t: Terminal) {
        let end = t.end
        Task { for await e in end { self.lock.withLock { self.endedWith = .some(e) } } }
    }

    var ended: String?? { lock.withLock { endedWith } }
}

/// This process's CPU time (user + system), in milliseconds.
private func cpuMs() -> Double {
    var u = rusage()
    getrusage(RUSAGE_SELF, &u)
    let t = { (v: timeval) in Double(v.tv_sec) * 1000 + Double(v.tv_usec) / 1000 }
    return t(u.ru_utime) + t(u.ru_stime)
}

/// A loopback TCP relay to a Unix socket, standing in for the path a phone takes to a Mac's fleetd: it accepts on
/// 127.0.0.1 (a port the kernel picks) and, for each connection, pumps bytes both ways to the socket at `target` on two
/// threads of its own. Only its own fds and threads are ever stopped.
private final class TCPRelay: @unchecked Sendable {
    let port: UInt16
    private let listener: Int32
    private let target: URL
    private let lock = NSLock()
    private var stopped = false
    private var conns: [Int32] = []
    private let threads = DispatchGroup()

    init(to target: URL) throws {
        self.target = target
        let l = socket(AF_INET, SOCK_STREAM, 0)
        guard l >= 0 else { throw SocketError.failed(errno: errno) }
        var one: Int32 = 1
        _ = setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        addr.sin_port = 0
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(l, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0 && listen(l, 16) == 0
            }
        }
        guard ok else { throw SocketError.failed(errno: errno) }
        var bound = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &bound) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(l, $0, &len) }
        }
        listener = l
        port = UInt16(bigEndian: bound.sin_port)
        threads.enter()
        Thread { [self] in
            acceptLoop()
            threads.leave()
        }.start()
    }

    private func acceptLoop() {
        while !lock.withLock({ stopped }) {
            var p = pollfd(fd: listener, events: Int16(POLLIN), revents: 0)
            guard poll(&p, 1, 20) == 1 else { continue }
            let c = accept(listener, nil, nil)
            guard c >= 0 else { continue }
            var one: Int32 = 1
            _ = setsockopt(c, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
            guard let u = connectUnix() else {
                Darwin.close(c)
                continue
            }
            let keep = lock.withLock { () -> Bool in
                if stopped { return false }
                conns += [c, u]
                return true
            }
            guard keep else {
                Darwin.close(c)
                Darwin.close(u)
                continue
            }
            for (from, to) in [(c, u), (u, c)] {
                threads.enter()
                Thread { [self] in
                    Self.pump(from, to)
                    threads.leave()
                }.start()
            }
        }
    }

    private func connectUnix() -> Int32? {
        let u = socket(AF_UNIX, SOCK_STREAM, 0)
        guard u >= 0 else { return nil }
        var one: Int32 = 1
        _ = setsockopt(u, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let path = Array(target.path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path) }
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(u, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if r != 0 {
            Darwin.close(u)
            return nil
        }
        return u
    }

    /// Copies `from` to `to` until either side ends, then ends both (so the other pump stops too).
    private static func pump(_ from: Int32, _ to: Int32) {
        var buf = [UInt8](repeating: 0, count: 64 * 1024)
        outer: while true {
            let n = buf.withUnsafeMutableBytes { read(from, $0.baseAddress!, $0.count) }
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { break }
            var at = 0
            while at < n {
                let w = buf.withUnsafeBytes { write(to, $0.baseAddress! + at, n - at) }
                if w < 0 && errno == EINTR { continue }
                guard w > 0 else { break outer }
                at += w
            }
        }
        _ = shutdown(from, SHUT_RDWR)
        _ = shutdown(to, SHUT_RDWR)
    }

    /// Stops accepting, ends every relayed connection, waits for its threads, and closes its fds.
    func stop() {
        let fds = lock.withLock { () -> [Int32] in
            stopped = true
            return conns
        }
        for f in fds { _ = shutdown(f, SHUT_RDWR) }
        threads.wait()
        for f in fds { Darwin.close(f) }
        Darwin.close(listener)
    }
}

/// A loopback port that was bound and then let go of: nothing listens on it now.
private func closedPort() throws -> UInt16 {
    let fd = socket(AF_INET, SOCK_STREAM, 0)
    guard fd >= 0 else { throw SocketError.failed(errno: errno) }
    defer { Darwin.close(fd) }
    var addr = sockaddr_in()
    addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0 }
    }
    guard ok else { throw SocketError.failed(errno: errno) }
    var bound = sockaddr_in()
    var len = socklen_t(MemoryLayout<sockaddr_in>.size)
    _ = withUnsafeMutablePointer(to: &bound) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) }
    }
    return UInt16(bigEndian: bound.sin_port)
}

/// A loopback port where a connect neither completes nor is refused: the network's black hole (a Mac asleep, a
/// tailnet path down), made on this machine. A listener that never accepts, its backlog filled with connections of
/// this helper's own: the kernel then drops every further SYN, and a connect waits as for a host that does not answer.
/// Backlog 1 rather than 0, as Darwin takes 0 to mean `kern.ipc.somaxconn` (128); the fill goes on until a probe stays
/// pending, so it holds whatever the kernel's limit is. Bound on 127.0.0.1, and on ::1 too when `v6` (one port for
/// both, as "localhost" names both). Only its own fds are ever closed.
private final class BlackHole {
    let port: UInt16
    private var fds: [Int32] = []

    init(v6: Bool = false) throws {
        var tries = 0
        while true {
            tries += 1
            fds = []
            guard let l4 = Self.listener(AF_INET, port: 0) else { throw SocketError.failed(errno: errno) }
            fds.append(l4)
            let p = Self.boundPort(l4)
            if v6 {
                guard let l6 = Self.listener(AF_INET6, port: p) else {
                    // That port is taken on ::1: another one.
                    for f in fds { Darwin.close(f) }
                    if tries < 20 { continue }
                    throw SocketError.failed(errno: errno)
                }
                fds.append(l6)
            }
            port = p
            break
        }
        try fill(AF_INET)
        if v6 { try fill(AF_INET6) }
    }

    deinit { for f in fds { Darwin.close(f) } }

    private static func addr(_ family: Int32, port: UInt16, _ body: (UnsafePointer<sockaddr>, socklen_t) -> Int32) -> Int32 {
        if family == AF_INET {
            var a = sockaddr_in()
            a.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
            a.sin_family = sa_family_t(AF_INET)
            a.sin_addr.s_addr = inet_addr("127.0.0.1")
            a.sin_port = port.bigEndian
            return withUnsafePointer(to: &a) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { body($0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
            }
        }
        var a = sockaddr_in6()
        a.sin6_len = UInt8(MemoryLayout<sockaddr_in6>.size)
        a.sin6_family = sa_family_t(AF_INET6)
        a.sin6_addr = in6addr_loopback
        a.sin6_port = port.bigEndian
        return withUnsafePointer(to: &a) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { body($0, socklen_t(MemoryLayout<sockaddr_in6>.size)) }
        }
    }

    private static func listener(_ family: Int32, port: UInt16) -> Int32? {
        let fd = socket(family, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        if family == AF_INET6 {
            var one: Int32 = 1
            _ = setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &one, socklen_t(MemoryLayout<Int32>.size))
        }
        guard addr(family, port: port, { bind(fd, $0, $1) }) == 0, listen(fd, 1) == 0 else {
            Darwin.close(fd)
            return nil
        }
        return fd
    }

    private static func boundPort(_ fd: Int32) -> UInt16 {
        var a = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &a) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) }
        }
        return UInt16(bigEndian: a.sin_port)
    }

    /// Connects to the listener until one connect stays pending for 200 ms: the backlog is full.
    private func fill(_ family: Int32) throws {
        for _ in 0..<512 {
            let c = socket(family, SOCK_STREAM, 0)
            guard c >= 0 else { throw SocketError.failed(errno: errno) }
            _ = fcntl(c, F_SETFL, fcntl(c, F_GETFL, 0) | O_NONBLOCK)
            _ = Self.addr(family, port: port) { Darwin.connect(c, $0, $1) }
            var p = pollfd(fd: c, events: Int16(POLLOUT), revents: 0)
            if poll(&p, 1, 200) == 0 {
                Darwin.close(c)  // the probe: never queued, nothing to keep
                return
            }
            fds.append(c)
        }
        throw SocketError.failed(errno: EAGAIN)
    }
}

@MainActor @Suite(.serialized) struct ClientTests {
    /// (a) Connect and get the Snapshot; a hook write arrives as a Delta in < 20 ms.
    @Test func connectsGetsTheSnapshotAndAHookWriteArrivesFast() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let board = Board()
        let client = BoardClient(board: board)
        await client.start(socket: fx.socket)
        try await cleaning {
            #expect(await until(15) { await client.status == .live && board.sessions.count == 3 })
            #expect(board.machine == "fixture-mac")
            #expect(board.run != nil)
            let key = SessionKey(machine: "fixture-mac", profile: "default", sessionId: sid(0))
            #expect(board.sessions[key]?.name == "fixture 0")

            var took: [Duration] = []
            for i in 0..<5 {
                let e = i % 2 == 0 ? "UserPromptSubmit" : "Stop"
                let t0 = ContinuousClock.now
                let at = try fx.hook(0, e)
                let arrived = await until(2) { board.sessions[key]?.hookAt == at }
                #expect(arrived, "hook \(i) never arrived")
                took.append(ContinuousClock.now - t0)
                try await Task.sleep(for: .milliseconds(100))
            }
            let median = took.sorted()[took.count / 2]
            print("hook write -> delta on the board: \(took)")
            #expect(median < .milliseconds(20), "hook -> delta median \(median)")
        } then: {
            await client.stop()
            fx.stopDaemon()
        }
    }

    /// (b) The daemon is killed and started again: the client reconnects, gets the new run, and resyncs.
    @Test func reconnectsToARestartedDaemonAndResyncs() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let board = Board()
        let client = BoardClient(board: board)
        await client.start(socket: fx.socket)
        try await cleaning {
            #expect(await until(15) { await client.status == .live && board.sessions.count == 3 })
            let firstRun = try #require(board.run)

            fx.stopDaemon()
            #expect(await until(5) { await client.status != .live })
            try fx.startDaemon()
            #expect(await until(15) { await client.status == .live && board.run != firstRun })
            #expect(board.sessions.count == 3)
            // And it is live: a hook on the new daemon reaches the board.
            let key = SessionKey(machine: "fixture-mac", profile: "default", sessionId: sid(1))
            let at = try fx.hook(1, "UserPromptSubmit")
            #expect(await until(2) { board.sessions[key]?.hookAt == at })
        } then: {
            await client.stop()
            fx.stopDaemon()
        }
    }

    /// (c) A terminal: the stand-in's banner, typing echoed, and 200 KiB of input arriving intact in ≤ 64 KiB frames.
    @Test func aTerminalShowsTheBannerEchoesAndTakesALargePaste() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let screen = Screen(await terms.open(1, target: .claude(profile: "default", job: rawJob), cols: 100, rows: 30))
            #expect(await until(15) { screen.has("BANNER \(rawJob)") })
            await terms.input(1, Data("hello fleet".utf8))
            #expect(await until(5) { screen.has("hello fleet") })

            let paste = Data((0..<(200 * 1024)).map { i in UInt8(ascii: "a") + UInt8(i % 26) })
            let frames = TermClient.inputFrames(ref: 1, data: paste)
            let lens = frames.map { f -> Int in
                guard case let .termIn(ref, d) = f, ref == 1 else { return -1 }
                return d.count
            }
            #expect(lens == [65536, 65536, 65536, 8192])
            #expect(frames.allSatisfy { (try? $0.encode()) != nil })
            let before = screen.data.count
            await terms.input(1, paste)
            #expect(await until(10) { screen.data.count >= before + paste.count })
            // A Bool, not two Data: swift-testing diffs collections it is shown, which for 200 KiB takes minutes.
            let same = screen.data.suffix(from: before) == paste
            #expect(same, "the paste came back altered")
            // Still the same connection: an oversized TermIn would have closed it, ending the terminal.
            #expect(screen.ended == nil)
            #expect(await terms.lastError == nil)

            await terms.close(1)
            #expect(await until(5) { screen.ended != nil })
            #expect(screen.ended == .some(nil))
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// I1: a ref closed and opened again at once. The old terminal's TermEnd comes after the new open, and is the old
    /// one's: the new terminal stays open and shows its banner.
    @Test func closeThenOpenOfARefKeepsTheNewTerminal() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let target = AttachTarget.claude(profile: "default", job: "0000000a")
            let old = Screen(await terms.open(1, target: target, cols: 100, rows: 30))
            #expect(await until(15) { old.has("BANNER 0000000a") })
            await terms.close(1)
            let new = Screen(await terms.open(1, target: target, cols: 100, rows: 30))
            #expect(await until(5) { old.ended != nil })
            #expect(old.ended == .some(nil))
            #expect(await until(15) { new.has("BANNER 0000000a") })
            try await Task.sleep(for: .milliseconds(300))
            #expect(new.ended == nil, "the old terminal's end was applied to the new one")
            await terms.input(1, Data("still here\r".utf8))
            #expect(await until(5) { new.has("still here") })
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// I1: opening a ref that is open, on another target: the old one is closed first, and the new program shows.
    @Test func openingAnOpenRefOnAnotherTargetSwitchesProgram() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let old = Screen(await terms.open(1, target: .claude(profile: "default", job: "0000000a"), cols: 100, rows: 30))
            #expect(await until(15) { old.has("BANNER 0000000a") })
            let new = Screen(await terms.open(1, target: .claude(profile: "default", job: "0000000b"), cols: 100, rows: 30))
            #expect(await until(15) { new.has("BANNER 0000000b") })
            #expect(await until(5) { old.ended != nil })
            #expect(!new.has("BANNER 0000000a"))
            try await Task.sleep(for: .milliseconds(300))
            #expect(new.ended == nil)
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// The daemon restarts under an open terminal: the client reconnects and opens it again.
    @Test func reconnectingReopensTheOpenTerminals() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let screen = Screen(await terms.open(3, target: .claude(profile: "default", job: "0000000a"), cols: 90, rows: 25))
            #expect(await until(15) { screen.has("BANNER 0000000a") })
            let first = screen.data.count

            fx.stopDaemon()
            try fx.startDaemon()
            #expect(await until(15) { screen.data.suffix(from: first).range(of: Data("BANNER 0000000a".utf8)) != nil })
            #expect(screen.ended == nil)
            await terms.input(3, Data("again\r".utf8))
            #expect(await until(5) { screen.data.suffix(from: first).range(of: Data("again".utf8)) != nil })
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// Ruling A-R2: `reconnect()` with two terminals open. Both are opened again on the new connection (each shows its
    /// banner again) and neither view sees an end.
    @Test func reconnectReattachesEveryOpenTerminalWithNoEnd() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let a = Screen(await terms.open(1, target: .claude(profile: "default", job: "e000000a"), cols: 90, rows: 25))
            let b = Screen(await terms.open(2, target: .claude(profile: "default", job: "e000000b"), cols: 90, rows: 25))
            #expect(await until(15) { a.has("BANNER e000000a") && b.has("BANNER e000000b") })
            let (fa, fb) = (a.data.count, b.data.count)
            let ups = await terms.connections()
            var seen: [Bool] = []
            let watch = Task { for await u in ups { seen.append(u) } }
            await terms.reconnect()
            // Kept warm by the daemon, each comes back with what it last printed (its banner) on the new connection.
            #expect(await until(15) {
                a.data.suffix(from: fa).range(of: Data("BANNER e000000a".utf8)) != nil
                    && b.data.suffix(from: fb).range(of: Data("BANNER e000000b".utf8)) != nil
            })
            #expect(a.ended == nil && b.ended == nil)
            #expect(await until(2) { seen.contains(false) && seen.last == true })
            watch.cancel()
            #expect(await terms.input(1, Data("after".utf8)) == .sent)
            #expect(await until(5) { a.data.suffix(from: fa).range(of: Data("after".utf8)) != nil })
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// Ruling A-R2 (P2-R4): keys typed while the daemon is away are held and arrive, in order, after the reopen.
    @Test func keysTypedDuringTheGapArriveInOrderAfterTheReopen() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let screen = Screen(await terms.open(4, target: .claude(profile: "default", job: rawJob), cols: 90, rows: 25))
            #expect(await until(15) { screen.has("BANNER \(rawJob)") })
            fx.stopDaemon()
            #expect(await until(5) { await !terms.connected })
            #expect(await terms.input(4, Data("first-".utf8)) == .held)
            #expect(await terms.input(4, Data("second-".utf8)) == .held)
            #expect(await terms.input(4, Data("third".utf8)) == .held)
            let before = screen.data.count
            try fx.startDaemon()
            #expect(await until(15) {
                screen.data.suffix(from: before).range(of: Data("first-second-third".utf8)) != nil
            })
            #expect(screen.ended == nil)
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// Ruling A-R2: past 64 KiB held during the gap, input is refused (InputFull), whole; what was held still arrives.
    @Test func typingPast64KiBDuringTheGapIsInputFull() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let screen = Screen(await terms.open(5, target: .claude(profile: "default", job: rawJob), cols: 90, rows: 25))
            #expect(await until(15) { screen.has("BANNER \(rawJob)") })
            fx.stopDaemon()
            #expect(await until(5) { await !terms.connected })
            // In lines: a fresh stand-in is still in cooked mode when the held keys land (before its `stty raw`), and
            // a canonical line longer than MAX_CANON (1024) never completes.
            var big = Data()
            while big.count + 64 <= TermClient.heldMax - 10 { big.append(Data((String(repeating: "x", count: 63) + "\n").utf8)) }
            big.append(Data(repeating: UInt8(ascii: "x"), count: TermClient.heldMax - 10 - big.count))
            #expect(big.count == TermClient.heldMax - 10)
            #expect(await terms.input(5, big) == .held)
            #expect(await terms.input(5, Data("0123456789".utf8)) == .held)  // exactly at the bound
            #expect(await terms.input(5, Data("!".utf8)) == .inputFull)
            #expect(await terms.input(5, Data(repeating: 0x79, count: 4096)) == .inputFull)
            let before = screen.data.count
            try fx.startDaemon()
            #expect(await until(15) { screen.data.suffix(from: before).range(of: Data("x0123456789".utf8)) != nil })
            let back = screen.data.suffix(from: before)
            #expect(back.range(of: Data("!".utf8)) == nil && back.range(of: Data("yyyy".utf8)) == nil)
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// Ruling A-R2: the board's `reconnect()` resumes from its run and seq: the board keeps its sessions throughout.
    @Test func boardReconnectResumes() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let board = Board()
        let client = BoardClient(board: board)
        await client.start(socket: fx.socket)
        try await cleaning {
            #expect(await until(15) { board.sessions.count == 3 })
            let run = board.run
            await client.reconnect()
            #expect(await until(5) { await client.status == .reconnecting || board.seq != nil })
            #expect(await until(15) { await client.status == .live })
            #expect(board.run == run && board.sessions.count == 3)
        } then: {
            await client.stop()
            fx.stopDaemon()
        }
    }

    /// P3-R5: a 50 MB flood into a view that reads slowly. The client holds about 1 MiB of it at most (then stops
    /// reading the socket, so fleetd's backpressure reaches the program), its memory grows < 8 MiB, and every byte
    /// arrives in order.
    @Test func aFloodIntoASlowViewIsBoundedAndArrivesInOrder() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let base = residentBytes()
            let t = await terms.open(1, target: .claude(profile: "default", job: floodJob), cols: 100, rows: 30)
            // Nobody reads for 2 s: the flood backs up into fleetd and the program.
            try await Task.sleep(for: .seconds(2))
            let held = t.output.unread
            print("held unread after 2 s: \(held) bytes")
            #expect(held > 0 && held <= TermClient.outputLimit + 512 * 1024, "\(held) bytes held")

            let banner = Data("BANNER \(floodJob)\r\n".utf8)
            var head = Data()
            var carry = Data()
            var next: UInt32 = 0
            var bad: Int?
            var peak = residentBytes()
            var chunks = 0
            for await chunk in t.output {
                var d = chunk
                if head.count < banner.count {
                    let n = min(banner.count - head.count, d.count)
                    head.append(d.prefix(n))
                    d = d.dropFirst(n)
                }
                carry.append(d)
                let whole = carry.count / 4 * 4
                carry.withUnsafeBytes { p in
                    for at in stride(from: 0, to: whole, by: 4) where bad == nil {
                        if UInt32(littleEndian: p.loadUnaligned(fromByteOffset: at, as: UInt32.self)) != next {
                            bad = Int(next)
                        }
                        next &+= 1
                    }
                }
                carry = Data(carry.suffix(from: carry.startIndex + whole))
                chunks += 1
                if chunks % 64 == 0 {
                    peak = max(peak, residentBytes())
                    try await Task.sleep(for: .milliseconds(1))  // a slow view
                }
            }
            peak = max(peak, residentBytes())
            #expect(head == banner)
            #expect(bad == nil, "out of order at word \(bad ?? -1)")
            #expect(Int(next) == floodWords && carry.isEmpty, "\(next) words of \(floodWords)")
            let growth = Int64(peak) - Int64(base)
            print("flood: \(chunks) chunks, resident growth \(growth / 1024) KiB")
            #expect(growth < 8 * 1024 * 1024, "resident memory grew \(growth) bytes")
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// NB1: a terminal flooded past its limit with nobody reading, then closed and its ref opened again: the close
    /// discards its output, so the connection reads on, the old terminal's end arrives, and the new one shows.
    @Test func closingAFloodedTerminalNobodyReadsFreesTheConnection() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let old = await terms.open(1, target: .claude(profile: "default", job: floodJob), cols: 100, rows: 30)
            let oldEnd = EndWatch(old)
            #expect(await until(15) { old.output.unread > TermClient.outputLimit }, "the flood never backed up")
            await terms.close(1)
            let t0 = ContinuousClock.now
            let new = Screen(await terms.open(1, target: .claude(profile: "default", job: rawJob), cols: 100, rows: 30))
            #expect(await until(2) { new.has("BANNER \(rawJob)") }, "the new terminal stalled behind the old one")
            #expect(await until(2) { oldEnd.ended != nil }, "the old terminal's end never came")
            #expect(oldEnd.ended == .some(nil))
            print("closed flood -> new banner in \(ContinuousClock.now - t0)")
            #expect(old.output.unread == 0)
            #expect(new.ended == nil)
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// NB1: a flooded terminal whose view is cancelled (or let go of) without a close: it is closed for it, and
    /// another terminal's output flows again.
    @Test func anAbandonedFloodedViewDoesNotHoldTheConnection() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let terms = TermClient()
        await terms.start(socket: fx.socket)
        try await cleaning {
            let other = Screen(await terms.open(2, target: .claude(profile: "default", job: rawJob), cols: 100, rows: 30))
            #expect(await until(15) { other.has("BANNER \(rawJob)") })

            // A view that takes one chunk and then stalls, until it is cancelled.
            let flood = await terms.open(1, target: .claude(profile: "default", job: floodJob), cols: 100, rows: 30)
            let view = Task {
                for await _ in flood.output { try? await Task.sleep(for: .seconds(600)) }
            }
            #expect(await until(15) { flood.output.unread > TermClient.outputLimit })
            // The connection is held: the other terminal's echo waits.
            await terms.input(2, Data("one".utf8))
            try await Task.sleep(for: .milliseconds(300))
            #expect(!other.has("one"), "the connection was not held by the stalled view")

            view.cancel()
            #expect(await until(2) { other.has("one") }, "the cancelled view still holds the connection")
            await terms.input(2, Data("two".utf8))
            #expect(await until(2) { other.has("two") })

            // And one let go of without iterating, past its limit: it is closed too, and holds nothing.
            do {
                let dropped = await terms.open(3, target: .claude(profile: "default", job: floodJob), cols: 100, rows: 30)
                #expect(await until(15) { dropped.output.unread > TermClient.outputLimit })
                await terms.input(2, Data("three".utf8))
                try await Task.sleep(for: .milliseconds(300))
                #expect(!other.has("three"))
            }  // the last reference to it goes here
            #expect(await until(2) { other.has("three") }, "a view let go of still holds the connection")
        } then: {
            await terms.stop()
            fx.stopDaemon()
        }
    }

    /// I2 and M4: a daemon that takes the connection and refuses it (an `Error`, then a close) is retried on the
    /// 50 ms -> 2 s schedule, not every 50 ms; the board client says it was refused.
    @Test func aRefusingDaemonIsRetriedWithBackoffAndShownAsRefused() async throws {
        let fx = try Fixture()
        let server = try RefusingServer(at: fx.socket)
        let terms = TermClient()
        let client = BoardClient(board: Board())
        try await cleaning {
            await terms.start(socket: fx.socket)
            try await Task.sleep(for: .seconds(3))
            await terms.stop()
            let termTries = server.attempts.count
            #expect(await terms.lastError == RefusingServer.words)
            print("terminals: \(termTries) attempts in 3 s")
            #expect(termTries >= 3 && termTries <= 8, "\(termTries) attempts in 3 s")

            let before = server.attempts.count
            await client.start(socket: fx.socket)
            #expect(await until(2) { await client.status == .refused(RefusingServer.words) })
            try await Task.sleep(for: .seconds(3))
            await client.stop()
            let boardTries = server.attempts.count - before
            print("board: \(boardTries) attempts in ~3 s")
            #expect(boardTries >= 3 && boardTries <= 9, "\(boardTries) attempts")
            #expect(await client.status == .refused(RefusingServer.words))
        } then: {
            await terms.stop()
            await client.stop()
            server.stop()
        }
    }

    /// M6: stop() ends every statuses() stream.
    @Test func stopEndsTheStatusStreams() async throws {
        let fx = try Fixture()
        let client = BoardClient(board: Board())
        await client.start(socket: fx.socket)
        let stream = await client.statuses()
        let reader = Task {
            var seen = 0
            for await _ in stream { seen += 1 }
            return seen
        }
        try await Task.sleep(for: .milliseconds(200))
        await client.stop()
        let seen = await reader.value  // returns only if the stream finished
        #expect(seen >= 1)
    }

    /// The board over TCP, through a loopback relay to the fixture's socket (the phone's path to a Mac): live, with the
    /// fixture's three sessions.
    @Test func boardOverTCP() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let relay = try TCPRelay(to: fx.socket)
        let board = Board()
        let client = BoardClient(board: board)
        try await cleaning {
            // The daemon may still be binding its socket: the relay's first connections can fail and the client retries.
            #expect(await until(15) { FileManager.default.fileExists(atPath: fx.socket.path) })
            await client.start(endpoint: .tcp(host: "127.0.0.1", port: relay.port))
            #expect(await until(5) { await client.status == .live && board.sessions.count == 3 })
            #expect(board.machine == "fixture-mac")
        } then: {
            await client.stop()
            relay.stop()
            fx.stopDaemon()
        }
    }

    /// Nothing listening on a TCP port is `.absent`, as a missing socket is.
    @Test func tcpRefusedIsAbsent() throws {
        let port = try closedPort()
        #expect(throws: SocketError.absent) { try FrameSocket.connect(.tcp(host: "127.0.0.1", port: port)) }
    }

    /// A host that never answers (a black hole: SYNs dropped, nothing refused) is `.absent` once the connect timeout
    /// is spent, not after the kernel's own ~75 s.
    @Test func tcpTimeoutIsAbsent() throws {
        let hole = try BlackHole()
        let t0 = ContinuousClock.now
        #expect(throws: SocketError.absent) { try FrameSocket.connect(.tcp(host: "127.0.0.1", port: hole.port)) }
        let took = ContinuousClock.now - t0
        #expect(took >= .milliseconds(2900) && took < .seconds(4), "took \(took)")
        #expect(FrameSocket.tcpConnectTimeout == .seconds(3))
        withExtendedLifetime(hole) {}  // held open until here: released early, it would refuse instead
    }

    /// The timeout is the whole connect's, not each address's: "localhost" names ::1 and 127.0.0.1, both black holes
    /// here, and it is `.absent` in 3 s, not 6.
    @Test func tcpDeadlineIsSharedAcrossAddresses() throws {
        let hole = try BlackHole(v6: true)
        var hints = addrinfo()
        hints.ai_family = AF_UNSPEC
        hints.ai_socktype = SOCK_STREAM
        var list: UnsafeMutablePointer<addrinfo>?
        try #require(getaddrinfo("localhost", String(hole.port), &hints, &list) == 0)
        var count = 0
        var at = list
        while let ai = at {
            count += 1
            at = ai.pointee.ai_next
        }
        freeaddrinfo(list)
        try #require(count >= 2, "localhost resolves to \(count) address(es) here")
        let t0 = ContinuousClock.now
        #expect(throws: SocketError.absent) { try FrameSocket.connect(.tcp(host: "localhost", port: hole.port)) }
        let took = ContinuousClock.now - t0
        #expect(took >= .milliseconds(2900) && took < .seconds(4), "took \(took) over \(count) addresses")
        withExtendedLifetime(hole) {}
    }

    /// A connect pending on a dead host does not hold either client's actor: status, input and stop() answer at once.
    @Test func aPendingConnectDoesNotHoldTheActor() async throws {
        let hole = try BlackHole()
        let endpoint = Endpoint.tcp(host: "127.0.0.1", port: hole.port)
        let client = BoardClient(board: Board())
        let terms = TermClient()
        await client.start(endpoint: endpoint)
        await terms.start(endpoint: endpoint)
        try await cleaning {
            // Well inside the first 3 s attempt: both loops are in their connect.
            try await Task.sleep(for: .milliseconds(300))
            var t0 = ContinuousClock.now
            _ = await client.status
            #expect(ContinuousClock.now - t0 < .milliseconds(100), "status took \(ContinuousClock.now - t0)")
            t0 = ContinuousClock.now
            await terms.input(1, Data("x".utf8))
            _ = await terms.connected
            #expect(ContinuousClock.now - t0 < .milliseconds(100), "terminals took \(ContinuousClock.now - t0)")
            t0 = ContinuousClock.now
            await client.stop()
            #expect(ContinuousClock.now - t0 < .milliseconds(100), "board stop took \(ContinuousClock.now - t0)")
            t0 = ContinuousClock.now
            await terms.stop()
            #expect(ContinuousClock.now - t0 < .milliseconds(100), "terminals stop took \(ContinuousClock.now - t0)")
        } then: {
            await client.stop()
            await terms.stop()
            // Held open until here (released early, it would refuse, and nothing would be pending). Closing it then
            // refuses the connects still pending on their threads, which end on their own.
            withExtendedLifetime(hole) {}
        }
    }

    /// A TCP connection turns Nagle off: a keystroke's frame goes out at once, not behind the last one's ack.
    @Test func tcpNoDelay() async throws {
        let fx = try Fixture()
        try fx.startDaemon()
        let relay = try TCPRelay(to: fx.socket)
        defer {
            relay.stop()
            fx.stopDaemon()
        }
        let sock = try FrameSocket.connect(.tcp(host: "127.0.0.1", port: relay.port))
        defer { sock.close() }
        let fd = try #require(sock.fdForTesting)
        var v: Int32 = 0
        var len = socklen_t(MemoryLayout<Int32>.size)
        #expect(getsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &v, &len) == 0)
        // Darwin reports the flag bit (TF_NODELAY, 4), not 1: on is any non-zero value.
        #expect(v != 0, "TCP_NODELAY is \(v)")
        // Blocking again after the timed connect: the read thread blocks in read(2), it does not spin on EAGAIN.
        #expect(fcntl(fd, F_GETFL, 0) & O_NONBLOCK == 0)
    }

    /// (d) No socket: `.absent`, and no busy loop (CPU over 5 s ≈ 0).
    @Test func anAbsentSocketIsAbsentAndCostsNothing() async throws {
        let fx = try Fixture()
        let client = BoardClient(board: Board())
        await client.start(socket: fx.socket)
        try await cleaning {
            #expect(await until(2) { await client.status == .absent })
            // The backoff reaches its 2 s ceiling within the first second or so; then measure.
            try await Task.sleep(for: .seconds(1))
            let t0 = cpuMs()
            try await Task.sleep(for: .seconds(5))
            let used = cpuMs() - t0
            print("CPU over 5 s with no daemon: \(used) ms")
            #expect(used < 50, "\(used) ms of CPU in 5 s")
            #expect(await client.status == .absent)
        } then: {
            await client.stop()
        }
    }
}
