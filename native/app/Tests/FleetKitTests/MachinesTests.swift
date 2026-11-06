// Two machines on one board against two real `fleetd serve`s (built from this repo, Ruling A-R1): one over its Unix
// socket (the Mac's own path) and one over its TCP listener on loopback, reached with its key preamble (PROTOCOL.md
// "Network"; on macOS a loopback peer always needs the key). Each daemon runs over a synthetic home of its own, with
// its own machine name, a profile listing three sessions of this test process, and a stand-in `claude` that answers
// the roster and echoes a raw-mode attach. Only the pids this test started are ever signalled.

import Darwin
import Foundation
import Testing

@testable import FleetKit

private let nativeDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    .deletingLastPathComponent().deletingLastPathComponent()

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

private func sid(_ n: Int) -> String { "bbbbbbbb-0000-4000-8000-\(String(format: "%012d", n))" }

private let rawJob = "e0000002"

/// A free loopback port (bound and let go), for a daemon that must come back on the same one.
private func freePort() throws -> UInt16 {
    let l = socket(AF_INET, SOCK_STREAM, 0)
    defer { Darwin.close(l) }
    var addr = sockaddr_in()
    addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    addr.sin_family = sa_family_t(AF_INET)
    addr.sin_addr.s_addr = inet_addr("127.0.0.1")
    let ok = withUnsafePointer(to: &addr) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(l, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0 }
    }
    guard ok else { throw SocketError.failed(errno: errno) }
    var bound = sockaddr_in()
    var len = socklen_t(MemoryLayout<sockaddr_in>.size)
    _ = withUnsafeMutablePointer(to: &bound) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(l, $0, &len) }
    }
    return UInt16(bigEndian: bound.sin_port)
}

/// A synthetic machine: its home, and the `fleetd serve` over it (optionally with its loopback listener).
private final class Machine {
    let name: String
    let root: URL
    let socket: URL
    let config: URL
    private var daemon: Process?

    init(_ name: String) throws {
        self.name = name
        var tmpl = Array("/tmp/fleetmm-XXXXXX".utf8CString)
        guard let made = mkdtemp(&tmpl) else { throw SocketError.failed(errno: errno) }
        root = URL(fileURLWithPath: String(cString: made)).resolvingSymlinksInPath()
        socket = root.appendingPathComponent("fleetd.sock")
        config = root.appendingPathComponent("cfg/fleetd.json")
        let fm = FileManager.default
        let profile = root.appendingPathComponent(".claude")
        for d in ["fleet-state", "fleet-status", "jobs", "projects"] {
            try fm.createDirectory(at: profile.appendingPathComponent(d), withIntermediateDirectories: true)
        }
        for d in [".local/bin", "stand-in", "panel", "work", "cfg"] {
            try fm.createDirectory(at: root.appendingPathComponent(d), withIntermediateDirectories: true)
        }
        try "{}".write(to: profile.appendingPathComponent("settings.json"), atomically: true, encoding: .utf8)
        let pid = getpid()
        let cwd = root.appendingPathComponent("work").path
        let rows = (0..<3).map {
            "{\"sessionId\":\"\(sid($0))\",\"pid\":\(pid),\"kind\":\"interactive\",\"status\":\"idle\","
                + "\"name\":\"\(name) \($0)\",\"cwd\":\"\(cwd)\",\"startedAt\":1793900000000}"
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
            case "$1" in
              agents) case "$*" in *--all*) cat "$S/all.json";; *) cat "$S/running.json";; esac;;
              attach) stty raw -echo; printf 'BANNER %s\\r\\n' "$2"; exec cat;;
              *) exit 1;;
            esac

            """
        let bin = root.appendingPathComponent(".local/bin/claude")
        try script.write(to: bin, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bin.path)
    }

    deinit {
        stop()
        try? FileManager.default.removeItem(at: root)
    }

    /// Starts the daemon; with `tcpPort`, its loopback listener on that port too.
    func start(tcpPort: UInt16? = nil) throws {
        let p = Process()
        p.executableURL = fleetdBinary
        p.arguments = ["serve", "--config", config.path]
            + (tcpPort.map { ["--listen", "loopback", "--port", String($0)] } ?? [])
        p.environment = [
            "HOME": root.path, "FLEETD_HOME": root.path, "FLEETD_SOCK": socket.path,
            "FLEETD_PANEL_DIR": root.appendingPathComponent("panel").path, "FLEETD_MACHINE": name,
            "FLEETD_ROSTER": "cli", "PATH": root.appendingPathComponent(".local/bin").path + ":/usr/bin:/bin",
            // Never the real Tailscale CLI.
            "FLEETD_TAILSCALE": root.appendingPathComponent("no-tailscale-here").path,
        ]
        let log = root.appendingPathComponent("fleetd.log")
        if !FileManager.default.fileExists(atPath: log.path) { FileManager.default.createFile(atPath: log.path, contents: nil) }
        let h = try FileHandle(forWritingTo: log)
        h.seekToEndOfFile()
        p.standardError = h
        p.standardOutput = h
        try p.run()
        daemon = p
    }

    /// SIGTERM to the daemon this test started (by its pid), and its exit.
    func stop() {
        guard let p = daemon else { return }
        daemon = nil
        kill(p.processIdentifier, SIGTERM)
        p.waitUntilExit()
    }

    var log: String { (try? String(contentsOf: root.appendingPathComponent("fleetd.log"), encoding: .utf8)) ?? "" }

    /// The key beside the config (64 hex characters), once the daemon has made it.
    func key() -> Data? {
        guard let hex = try? String(contentsOf: config.deletingLastPathComponent().appendingPathComponent("fleetd.key"),
                                    encoding: .utf8)
        else { return nil }
        let h = Array(hex.trimmingCharacters(in: .whitespacesAndNewlines).utf8)
        guard h.count == 64 else { return nil }
        func nibble(_ c: UInt8) -> UInt8? {
            switch c {
            case UInt8(ascii: "0")...UInt8(ascii: "9"): c - UInt8(ascii: "0")
            case UInt8(ascii: "a")...UInt8(ascii: "f"): c - UInt8(ascii: "a") + 10
            default: nil
            }
        }
        var out = Data()
        for i in stride(from: 0, to: 64, by: 2) {
            guard let a = nibble(h[i]), let b = nibble(h[i + 1]) else { return nil }
            out.append(a << 4 | b)
        }
        return out
    }

    /// A hook event for session `n`, as hook.sh writes it.
    @discardableResult func hook(_ n: Int, _ e: String) throws -> Int64 {
        let at = Int64((Date().timeIntervalSince1970 * 1000).rounded())
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

@MainActor
private func until(_ seconds: Double, _ cond: @MainActor () async -> Bool) async -> Bool {
    let end = ContinuousClock.now + .milliseconds(Int(seconds * 1000))
    while ContinuousClock.now < end {
        if await cond() { return true }
        try? await Task.sleep(for: .milliseconds(1))
    }
    return await cond()
}

private final class Screen: @unchecked Sendable {
    private let lock = NSLock()
    private var bytes = Data()

    init(_ t: Terminal) {
        Task { for await d in t.output { self.lock.withLock { self.bytes.append(d) } } }
    }

    func has(_ s: String) -> Bool { lock.withLock { bytes.range(of: Data(s.utf8)) != nil } }
}

private func ms(_ d: Duration) -> Double {
    Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15
}

@MainActor @Suite(.serialized) struct MachinesTests {
    /// The Mac's own board over its socket and another machine's over TCP with the key, merged: six rows, each under
    /// its machine; a hook on the remote reaches the board with a pass ≤ 1 ms (ALL-R1, median of 3 p95s); a terminal
    /// of the remote's opens on that endpoint's TermClient and echoes; the remote stopped greys its rows (never
    /// removes them) and leaves the local ones alone; started again, its rows are its own again.
    @Test func twoFleetdsOnOneBoard() async throws {
        let local = try Machine("fixture-local")
        let remote = try Machine("fixture-remote")
        let port = try freePort()
        try local.start()
        try remote.start(tcpPort: port)
        defer {
            local.stop()
            remote.stop()
        }
        #expect(await until(20) { remote.log.contains("listening tcp 127.0.0.1:\(port)") && remote.key() != nil },
                "\(remote.log)")
        #expect(await until(20) { FileManager.default.fileExists(atPath: local.socket.path) })
        let key = try #require(remote.key())
        let remoteEndpoint = Endpoint.keyedTCP(host: "127.0.0.1", port: port, key: key)
        let machines = Machines(
            endpoints: [.unix(local.socket), remoteEndpoint], cacheURL: { _ in nil },
            unreachableAfter: .milliseconds(300))
        let out = machines.merged.output
        machines.start()
        defer { machines.stop() }

        #expect(await until(20) {
            out.sessions.count == 6 && machines.merged.machines.allSatisfy { $0.status == .live }
        }, "\(machines.merged.machines) \(remote.log)")
        #expect(machines.merged.machines.map(\.machine) == ["fixture-local", "fixture-remote"])
        #expect(Set(out.sessions.keys.map(\.machine)) == ["fixture-local", "fixture-remote"])
        let localRows = out.sessions.filter { $0.key.machine == "fixture-local" }

        // ALL-R1: each hook on the remote, through its TCP board, costs the main actor one pass of ≤ 1 ms.
        var p95s: [Double] = []
        for run in 0..<3 {
            var times: [Double] = []
            for i in 0..<20 {
                let n = i % 3
                let k = SessionKey(machine: "fixture-remote", profile: "default", sessionId: sid(n))
                let at = try remote.hook(n, (run + i) % 2 == 0 ? "UserPromptSubmit" : "Stop")
                let arrived = await until(5) { out.sessions[k]?.hookAt == at }
                #expect(arrived, "hook \(run).\(i) never reached the merged board")
                times.append(ms(machines.merged.lastPass))
                try await Task.sleep(for: .milliseconds(20))
            }
            times.sort()
            p95s.append(times[min(times.count - 1, Int(Double(times.count) * 0.95))])
        }
        p95s.sort()
        print("two-fleetd merge pass p95 (median of 3): \(String(format: "%.3f", p95s[1])) ms")
        #expect(p95s[1] <= 1.0)

        // A remote session's terminal goes through the remote's endpoint.
        let node = try #require(machines.node(for: "fixture-remote"))
        #expect(node.endpoint == remoteEndpoint)
        let screen = Screen(await node.terms.open(1, target: .claude(profile: "default", job: rawJob), cols: 80, rows: 24))
        #expect(await until(15) { screen.has("BANNER \(rawJob)") }, "\(remote.log)")
        _ = await node.terms.input(1, Data("over tcp".utf8))
        #expect(await until(5) { screen.has("over tcp") })
        await node.terms.close(1)

        // The remote goes away: greyed, not removed; the local rows untouched.
        remote.stop()
        #expect(await until(10) { machines.merged.isUnreachable("fixture-remote") })
        #expect(await until(2) {
            out.sessions.filter { $0.key.machine == "fixture-remote" }.values.allSatisfy { $0.state == .unreachable }
        })
        #expect(out.sessions.count == 6)
        let grey = out.sessions.values.first { $0.key.machine == "fixture-remote" }
        #expect(grey?.live == false)
        #expect(grey?.reason.hasPrefix("fixture-remote unreachable since ") == true)
        #expect(grey?.reason.contains(" UTC; was ") == true)
        let localNow = out.sessions.filter { $0.key.machine == "fixture-local" }
        #expect(Set(localNow.keys) == Set(localRows.keys))
        #expect(localNow.values.allSatisfy { $0.state != .unreachable && $0.live })
        #expect(machines.merged.line(of: "fixture-local")?.status == .live)

        // Back on the same port: its rows are its own again.
        try remote.start(tcpPort: port)
        #expect(await until(20) {
            machines.merged.line(of: "fixture-remote")?.status == .live
                && out.sessions.values.allSatisfy { $0.state != .unreachable } && out.sessions.count == 6
        }, "\(machines.merged.machines)")
    }

    /// An endpoint added while running joins the board; removed, its rows leave.
    @Test func machinesAddedAndRemovedWhileRunning() async throws {
        let a = try Machine("fixture-a")
        let b = try Machine("fixture-b")
        try a.start()
        try b.start()
        defer {
            a.stop()
            b.stop()
        }
        #expect(await until(20) {
            FileManager.default.fileExists(atPath: a.socket.path) && FileManager.default.fileExists(atPath: b.socket.path)
        })
        let machines = Machines(endpoints: [.unix(a.socket)], cacheURL: { _ in nil })
        machines.start()
        defer { machines.stop() }
        let out = machines.merged.output
        #expect(await until(20) { out.sessions.count == 3 })
        machines.set(endpoints: [.unix(a.socket), .unix(b.socket)])
        #expect(await until(20) { out.sessions.count == 6 })
        machines.set(endpoints: [.unix(a.socket)])
        #expect(await until(5) { out.sessions.count == 3 && out.sessions.keys.allSatisfy { $0.machine == "fixture-a" } })
        #expect(machines.nodes.count == 1)
    }
}
