// MergedBoard: one Board per machine merged into one (Rulings D-R3, A-R1). Lane D's tests, lifted with the merge, and
// the Mac's: a machine out of reach greyed (unreachable, with its reason) and never removed, a resync of one machine
// that leaves the other alone, sources added and removed, the merged Meta, and the cost of a pass (ALL-R1).

import Combine
import Foundation
import Testing

@testable import FleetKit

private func session(
    _ machine: String, _ id: String, state: SessionState = .working, live: Bool = true, reason: String = "fixture",
    epic: String? = nil
) -> Session {
    Session(
        key: SessionKey(machine: machine, profile: "p", sessionId: id), engine: .claude, name: nil, cwd: "/w",
        state: state, reason: reason, live: live, pid: nil, stage: nil, model: nil, effort: nil, contextTokens: nil,
        lastAt: nil, said: nil, doing: nil, lastAsk: nil, firstAsk: nil, startedAt: nil, hookAt: nil, writtenAt: nil,
        jobId: nil, title: id, epic: epic, archived: false, account: nil)
}

/// Lets the main actor run what is queued on it (the merge's one pass per turn).
@MainActor private func turn() async {
    for _ in 0..<4 { await Task.yield() }
}

@MainActor private func source(_ machine: String, _ ids: [String], run: UInt64 = 7) -> Board {
    let b = Board()
    _ = b.apply(.snapshot(run: run, seq: 0, machine: machine, sessions: ids.map { session(machine, $0) }))
    return b
}

private func key(_ m: String, _ id: String) -> SessionKey { SessionKey(machine: m, profile: "p", sessionId: id) }

@MainActor @Suite struct MergedBoardTests {
    @Test func twoMachinesMergeIntoOne() {
        let merged = MergedBoard(sources: [source("m1", ["a", "b"]), source("m2", ["c"])])
        #expect(merged.output.sessions.count == 3)
        #expect(merged.output.run == 1)
        #expect(merged.machines.map(\.machine) == ["m1", "m2"])
    }

    @Test func aSourceDeltaIsFollowedAfterOneTurn() async {
        let m1 = source("m1", ["a", "b"])
        let m2 = source("m2", ["c"])
        let merged = MergedBoard(sources: [m1, m2])
        let before = merged.output.seq
        let c = merged.output.sessions[key("m2", "c")]
        _ = m1.apply(.delta(Delta(
            seq: 1, upsert: [session("m1", "d"), session("m1", "a", state: .needsYou)], remove: [key("m1", "b")])))
        await turn()
        #expect(Set(merged.output.sessions.keys.map(\.sessionId)) == ["a", "c", "d"])
        #expect(merged.output.sessions[key("m1", "a")]?.state == .needsYou)
        #expect(merged.output.sessions[key("m2", "c")] == c)
        #expect(merged.output.seq == (before ?? 0) + 1)
    }

    /// One machine's resync (a new run: its daemon restarted) is mirrored as a diff of that machine only: the other's
    /// rows are not re-sent, and the output keeps its own run.
    @Test func aResyncOfOneMachineDoesNotDisturbTheOther() async {
        let m1 = source("m1", ["a", "b"])
        let m2 = source("m2", ["c"])
        let merged = MergedBoard(sources: [m1, m2])
        var upserts: [Session] = []
        var removes: [SessionKey] = []
        let seqBefore = merged.output.seq ?? 0
        // The output's deltas, as a client of it would see them: what the pass sent.
        let before = merged.output.sessions
        _ = m1.apply(.snapshot(run: 8, seq: 0, machine: "m1", sessions: [session("m1", "a"), session("m1", "e")]))
        await turn()
        let after = merged.output.sessions
        for (k, s) in after where before[k] != s { upserts.append(s) }
        for k in before.keys where after[k] == nil { removes.append(k) }
        #expect(Set(after.keys.map(\.sessionId)) == ["a", "c", "e"])
        #expect(upserts.map(\.key) == [key("m1", "e")])
        #expect(removes == [key("m1", "b")])
        #expect(after[key("m2", "c")] == before[key("m2", "c")])
        #expect(merged.output.run == 1)  // the output's own run: a source's resync is not one of its own
        #expect(merged.output.seq == seqBefore + 1)
    }

    /// Epics and groups are the first machine's (the Mac's own); accounts are unioned by (engine, name); no health,
    /// which is per machine.
    @Test func metaMerges() async {
        let m1 = source("m1", [])
        let m2 = source("m2", [])
        let acct = { (n: String, p: String) in Account(name: n, engine: .claude, profile: p, limits: []) }
        let health = Health(cpuPct: 1, memPct: 2, diskPct: 3, dev: 0, db: 0)
        _ = m1.apply(.meta(Meta(
            epics: ["e1", "e2"], groups: ["alpha", "zeta"], accounts: [acct("x", "p"), acct("y", "p")], health: health)))
        let merged = MergedBoard(sources: [m1, m2])
        _ = m2.apply(.meta(Meta(epics: ["e3"], groups: ["mid"], accounts: [acct("y", "other"), acct("z", "p")])))
        await turn()
        #expect(merged.output.meta.epics == ["e1", "e2"])
        #expect(merged.output.meta.groups == ["alpha", "zeta"])
        #expect(merged.output.meta.accounts.map(\.name) == ["x", "y", "z"])
        #expect(merged.output.meta.health == nil)
        #expect(merged.machines[0].health == health)
        #expect(merged.machines[1].health == nil)
    }

    /// A machine that drops out: its rows stay, as Unreachable (greyed, not live), the reason saying since when and
    /// what each was; the other machine's rows are untouched. When it is back its rows are its own again.
    @Test func aMachineDroppingOutIsGreyedNotRemoved() async throws {
        let m1 = source("m1", ["a"])
        let m2 = Board()
        _ = m2.apply(.snapshot(run: 7, seq: 0, machine: "m2", sessions: [
            session("m2", "c", state: .working, reason: "a turn is running"),
            session("m2", "d", state: .needsYou, reason: "a permission"),
        ]))
        let at = Date(timeIntervalSince1970: 1_793_923_200 + 754 * 60)  // 12:34 UTC
        let merged = MergedBoard(sources: [m1, m2], unreachableAfter: .zero)
        merged.set(status: .live, cachedAt: nil, forSource: m1)
        merged.set(status: .live, cachedAt: nil, forSource: m2)
        await turn()
        let a = merged.output.sessions[key("m1", "a")]
        #expect(merged.output.sessions[key("m2", "c")]?.state == .working)

        merged.set(status: .absent, cachedAt: at, forSource: m2)
        try await Task.sleep(for: .milliseconds(5))
        await turn()
        #expect(merged.isUnreachable("m2"))
        #expect(!merged.isUnreachable("m1"))
        #expect(merged.machines[1].downSince == at)
        #expect(merged.output.sessions.count == 3)
        let c = try #require(merged.output.sessions[key("m2", "c")])
        #expect(c.state == .unreachable)
        #expect(!c.live)
        #expect(c.reason == "m2 unreachable since 12:34 UTC; was working: a turn is running")
        #expect(merged.output.sessions[key("m2", "d")]?.reason == "m2 unreachable since 12:34 UTC; was needs you: a permission")
        #expect(merged.output.sessions[key("m1", "a")] == a)
        // A delta of its own while it is out of reach (from its cache, say) still shows as unreachable.
        _ = m2.apply(.delta(Delta(seq: 1, upsert: [session("m2", "e")], remove: [])))
        await turn()
        #expect(merged.output.sessions[key("m2", "e")]?.state == .unreachable)

        merged.set(status: .live, cachedAt: nil, forSource: m2)
        await turn()
        #expect(!merged.isUnreachable("m2"))
        #expect(merged.output.sessions[key("m2", "c")] == m2.sessions[key("m2", "c")])
        #expect(merged.output.sessions[key("m2", "d")]?.state == .needsYou)
    }

    /// A blip shorter than the grace (a fleetd restart) never greys a row.
    @Test func aShortDropDoesNotGrey() async throws {
        let m1 = source("m1", ["a"])
        let merged = MergedBoard(sources: [m1], unreachableAfter: .milliseconds(200))
        merged.set(status: .live, cachedAt: nil, forSource: m1)
        merged.set(status: .reconnecting, cachedAt: Date(), forSource: m1)
        try await Task.sleep(for: .milliseconds(50))
        merged.set(status: .live, cachedAt: nil, forSource: m1)
        try await Task.sleep(for: .milliseconds(300))
        await turn()
        #expect(!merged.isUnreachable("m1"))
        #expect(merged.output.sessions[key("m1", "a")]?.state == .working)

        // Down for longer than the grace: greyed.
        merged.set(status: .reconnecting, cachedAt: Date(), forSource: m1)
        try await Task.sleep(for: .milliseconds(300))
        await turn()
        #expect(merged.isUnreachable("m1"))
        #expect(merged.output.sessions[key("m1", "a")]?.state == .unreachable)
    }

    /// A machine never reached since launch (its cached rows only) goes unreachable after the grace, as of its cache.
    @Test func aCachedMachineNeverReachedGoesUnreachable() async throws {
        let b = Board()
        let saved = Date(timeIntervalSince1970: 1_793_923_200)  // 00:00 UTC
        MergedBoard.preload(b, from: BoardCache.Contents(
            savedAt: saved, machine: "m2", sessions: [session("m2", "c")], meta: Meta()))
        let merged = MergedBoard(sources: [b], unreachableAfter: .milliseconds(20))
        merged.set(status: .absent, cachedAt: saved, forSource: b)
        #expect(merged.output.sessions[key("m2", "c")]?.state == .working)
        try await Task.sleep(for: .milliseconds(80))
        await turn()
        #expect(merged.output.sessions[key("m2", "c")]?.reason == "m2 unreachable since 00:00 UTC; was working: fixture")
    }

    @Test func sourcesAreAddedAndRemoved() async {
        let m1 = source("m1", ["a"])
        let merged = MergedBoard(sources: [m1])
        let m2 = source("m2", ["c", "d"])
        merged.add(m2)
        await turn()
        #expect(merged.output.sessions.count == 3)
        #expect(merged.machines.map(\.machine) == ["m1", "m2"])
        merged.remove(m2)
        await turn()
        #expect(Set(merged.output.sessions.keys) == [key("m1", "a")])
        #expect(merged.machines.map(\.machine) == ["m1"])
        // A removed source's changes no longer reach the output.
        _ = m2.apply(.delta(Delta(seq: 1, upsert: [session("m2", "x")], remove: [])))
        await turn()
        #expect(merged.output.sessions.count == 1)
    }

    @Test func preloadPutsTheCacheOnScreenAndAsksForASnapshot() {
        let b = Board()
        let contents = BoardCache.Contents(
            savedAt: Date(timeIntervalSince1970: 2_000), machine: "m1",
            sessions: [session("m1", "a"), session("m1", "b")], meta: Meta(epics: ["e1"]))
        MergedBoard.preload(b, from: contents)
        let merged = MergedBoard(sources: [b])
        // There before any live frame and before any turn of the main actor (ALL-R1: the first frame).
        #expect(merged.output.sessions.count == 2)
        #expect(merged.output.meta.epics == ["e1"])
        #expect(merged.machines.map(\.machine) == ["m1"])
        // Run 0 is never a daemon's run (PROTOCOL.md: random each start), so the first Hello is sent a Snapshot.
        guard case let .hello(_, since, run, role) = b.hello() else {
            Issue.record("not a hello")
            return
        }
        #expect(run == 0)
        #expect(since == 0)
        #expect(role == .board)
    }

    @Test func fiveHundredSourceDeltasInOneTurnAreOneOutputDelta() async {
        let m1 = source("m1", [])
        let merged = MergedBoard(sources: [m1, source("m2", ["c"])])
        let start = merged.output.seq ?? 0
        let passes = merged.passes
        var willChange = 0
        let watch = merged.output.objectWillChange.sink { _ in willChange += 1 }
        for i in 1...500 {
            _ = m1.apply(.delta(Delta(seq: UInt64(i), upsert: [session("m1", "s\(i)")], remove: [])))
        }
        #expect(merged.output.sessions.count == 1)  // nothing yet: the pass is on the next turn
        await turn()
        #expect(merged.output.sessions.count == 501)
        #expect(merged.output.seq == start + 1)
        #expect(merged.passes == passes + 1)
        #expect(willChange == 1)
        withExtendedLifetime(watch) {}
    }

    /// ALL-R1: a pass for one delta of one machine costs ≤ 1 ms on the main actor, with a full board on both (150
    /// rows each, more than Ross runs). Median of 3 runs of the p95 of 200 deltas.
    @Test func aPassCostsUnderAMillisecond() async {
        var p95s: [Double] = []
        for _ in 0..<3 {
            let m1 = source("m1", (0..<150).map { "a\($0)" })
            let m2 = source("m2", (0..<150).map { "b\($0)" })
            let merged = MergedBoard(sources: [m1, m2])
            var times: [Double] = []
            for i in 1...200 {
                _ = m2.apply(.delta(Delta(
                    seq: UInt64(i), upsert: [session("m2", "b\(i % 150)", state: i % 2 == 0 ? .waiting : .working)],
                    remove: [])))
                await turn()
                times.append(Double(merged.lastPass.components.attoseconds) / 1e15
                    + Double(merged.lastPass.components.seconds) * 1000)
            }
            times.sort()
            p95s.append(times[Int(Double(times.count) * 0.95)])
        }
        p95s.sort()
        let median = p95s[1]
        print("MergedBoard pass p95 (median of 3): \(String(format: "%.3f", median)) ms")
        #expect(median <= 1.0)
    }

    @Test func eachEndpointHasItsOwnCacheFile() {
        let base = BoardCache.defaultURL
        let a = MergedBoard.cacheURL(for: .tcp(host: "m1.example", port: 47470))
        let b = MergedBoard.cacheURL(for: .tcp(host: "::1", port: 9))
        #expect(a.deletingLastPathComponent() == base.deletingLastPathComponent())
        #expect(a.lastPathComponent == "board-m1.example-47470.cache")
        #expect(b.lastPathComponent == "board-__1-9.cache")
        #expect(a != b)
    }

    /// The board model over a merged board: cached until a machine is live, then live; the machine count.
    @Test func theModelIsCachedUntilAMachineIsLive() async {
        let m1 = source("m1", ["a"])
        let merged = MergedBoard(sources: [m1, source("m2", ["c"])])
        let saved = Date(timeIntervalSince1970: 5_000)
        let model = BoardModel(merged: merged, cachedAt: saved)
        #expect(model.cachedAt == saved)
        #expect(model.groups.flatMap(\.rows).count == 2)  // the rows are there at once
        #expect(model.machineCount == 2)
        merged.set(status: .live, cachedAt: nil, forSource: m1)
        try? await Task.sleep(for: .milliseconds(5))
        await turn()
        #expect(model.cachedAt == nil)
        #expect(model.status(of: "m1") == .live)
        #expect(model.status(of: "m2") == .reconnecting)
    }
}

@Suite struct RemoteAttachTests {
    /// A remote machine's job opens through its own fleetd (the caller names its machine as local); while it cannot be
    /// reached it says so instead of offering a terminal.
    @Test func aRemoteJobOpensAndAnUnreachableOneSaysWhy() {
        var s = session("m2", "c")
        s.jobId = "0a1b2c3d"
        #expect(Attach.of(s, local: "m2") == .open(.claude(profile: "p", job: "0a1b2c3d")))
        guard case .none = Attach.of(s, local: "m1") else {
            Issue.record("a machine with no endpoint opens nothing")
            return
        }
        let u = MergedBoard.unreachable(s, since: Date(timeIntervalSince1970: 0))
        #expect(Attach.of(u, local: "m2") == .none("m2 cannot be reached now: its terminal opens here when it is back."))
    }
}

@Suite struct TailscalePeersTests {
    /// Synthetic `tailscale status --json`: this node, a Linux and a macOS node of the same user, an iOS node, a
    /// tagged node, another user's node.
    private let status = """
        {"Self": {"HostName": "this-mac", "OS": "macOS", "UserID": 11, "TailscaleIPs": ["100.64.0.1", "fd7a::1"]},
         "Peer": {
           "k1": {"HostName": "linux-box", "OS": "linux", "UserID": 11, "TailscaleIPs": ["100.64.0.2", "fd7a::2"]},
           "k2": {"HostName": "other-mac", "OS": "macOS", "UserID": 11, "Tags": [], "TailscaleIPs": ["fd7a::3", "100.64.0.3"]},
           "k3": {"HostName": "phone", "OS": "iOS", "UserID": 11, "TailscaleIPs": ["100.64.0.4"]},
           "k4": {"HostName": "tagged", "OS": "linux", "UserID": 11, "Tags": ["tag:server"], "TailscaleIPs": ["100.64.0.5"]},
           "k5": {"HostName": "theirs", "OS": "linux", "UserID": 22, "TailscaleIPs": ["100.64.0.6"]}
         }}
        """

    @Test func onlyLinuxAndMacNodesOfThisUserUntaggedAndNotSelf() throws {
        let peers = try #require(Tailscale.peers(fromStatus: Data(status.utf8)))
        #expect(peers == [
            Tailscale.Peer(name: "linux-box", address: "100.64.0.2"),
            Tailscale.Peer(name: "other-mac", address: "100.64.0.3"),
        ])
        #expect(peers[0].endpoint == .tcp(host: "100.64.0.2", port: 47470))
    }

    @Test func notAStatusIsNil() {
        #expect(Tailscale.peers(fromStatus: Data("[]".utf8)) == nil)
        #expect(Tailscale.peers(fromStatus: Data("{}".utf8)) == nil)
        #expect(Tailscale.peers(fromStatus: Data(#"{"Self": {"UserID": 1}}"#.utf8)) == [])
    }

    @Test func aMissingCLIIsNil() async {
        #expect(await Tailscale.discover(cli: ["/nonexistent/tailscale"]) == nil)
    }

    /// A CLI that hangs is stopped at the time limit.
    @Test func aSlowCLIIsStopped() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ts-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let cli = dir.appendingPathComponent("tailscale")
        try "#!/bin/sh\nexec sleep 10\n".write(to: cli, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: cli.path)
        let t0 = ContinuousClock.now
        #expect(await Tailscale.discover(timeout: .milliseconds(200), cli: [cli.path]) == nil)
        #expect(ContinuousClock.now - t0 < .seconds(2))
    }

    @Test func aCLIThatAnswersIsRead() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ts-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try status.write(to: dir.appendingPathComponent("status.json"), atomically: true, encoding: .utf8)
        let cli = dir.appendingPathComponent("tailscale")
        try "#!/bin/sh\ncat '\(dir.path)/status.json'\n".write(to: cli, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: cli.path)
        let peers = await Tailscale.discover(cli: [cli.path])
        #expect(peers?.map(\.name) == ["linux-box", "other-mac"])
    }
}
