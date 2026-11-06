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

/// This process's CPU time (user + system), in milliseconds.
private func cpuMs() -> Double {
    var u = rusage()
    getrusage(RUSAGE_SELF, &u)
    let t = { (v: timeval) in Double(v.tv_sec) * 1000 + Double(v.tv_usec) / 1000 }
    return t(u.ru_utime) + t(u.ru_stime)
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
