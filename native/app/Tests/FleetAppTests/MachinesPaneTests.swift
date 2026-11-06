// The pane on a board of two machines (Ruling A-R1, fix round I1): each machine's Terminals connection is known by its
// link from the moment it is made, before its machine has a name; a reconnect reopens (replays, P3-R11) only that
// connection's terminals. And the Machines list (M1, M3, M7): what can be typed, what is kept, this Mac refused.

import AppKit
import FleetKit
import Testing

@testable import FleetApp

/// Two connections, one per machine, each known by a token fixed at creation; a machine maps to its connection only
/// once its board has named it (as LiveBackend's route does through Machines.node(for:)).
@MainActor final class TwoLinkBackend: TermBackend {
    final class Token {}
    let tokens: [Token]
    let sources: [Board]
    var opened: [UInt32] = []
    var onConnection: ((_ link: TermLink?, _ up: Bool) -> Void)?

    init(sources: [Board]) {
        self.sources = sources
        tokens = sources.map { _ in Token() }
    }

    func link(_ i: Int) -> TermLink { ObjectIdentifier(tokens[i]) }
    func link(for machine: String) -> TermLink? {
        sources.firstIndex { $0.machine == machine }.map(link)
    }

    func open(_ term: PaneTerm, target: AttachTarget, cols: UInt16, rows: UInt16) { opened.append(term.ref) }
    func input(_ ref: UInt32, _ data: Data) {}
    func size(_ ref: UInt32, cols: UInt16, rows: UInt16) {}
    func close(_ ref: UInt32) {}
}

private func job(_ machine: String, _ n: Int) -> Session {
    Session(
        key: SessionKey(machine: machine, profile: "rig", sessionId: "\(machine)-\(n)"), engine: .claude, name: nil,
        cwd: "/tmp", state: .waiting, reason: "fixture", live: true, pid: nil, stage: nil, model: nil, effort: nil,
        contextTokens: nil, lastAt: Int64(Date().timeIntervalSince1970 * 1000), said: nil, doing: nil, lastAsk: nil,
        firstAsk: nil, startedAt: nil, hookAt: nil, writtenAt: nil, jobId: "0000000\(n)", title: "\(machine) \(n)",
        epic: nil, archived: false, account: nil)
}

@MainActor @Suite(.serialized) struct MachinesPaneTests {
    @Test func aReconnectReopensOnlyThatMachinesTerminalsEvenWhenItConnectedBeforeItHadAName() async throws {
        let local = Board(), remote = Board()
        _ = local.apply(.snapshot(run: 1, seq: 0, machine: "rig-mac", sessions: [job("rig-mac", 1)]))
        let merged = MergedBoard(sources: [local, remote])
        merged.set(status: .live, cachedAt: nil, forSource: local)
        let model = BoardModel(merged: merged, cachedAt: nil)
        let backend = TwoLinkBackend(sources: [local, remote])
        let pane = TerminalPaneController(model: model, backend: backend)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1000, height: 600), styleMask: [.titled], backing: .buffered,
            defer: false)
        window.contentViewController = pane
        pane.build()

        // Both connections come up before the remote's board has said its name.
        #expect(remote.machine == nil)
        backend.onConnection?(backend.link(0), true)
        backend.onConnection?(backend.link(1), true)

        _ = remote.apply(.snapshot(run: 2, seq: 0, machine: "rig-remote", sessions: [job("rig-remote", 2)]))
        merged.set(status: .live, cachedAt: nil, forSource: remote)
        try await Task.sleep(for: .milliseconds(60))
        let l = try #require(merged.output.sessions[SessionKey(machine: "rig-mac", profile: "rig", sessionId: "rig-mac-1")])
        let r = try #require(merged.output.sessions[SessionKey(machine: "rig-remote", profile: "rig", sessionId: "rig-remote-2")])
        pane.show(l, open: true, focus: false)
        try await Task.sleep(for: .milliseconds(60))
        pane.show(r, open: true, focus: false)
        try await Task.sleep(for: .milliseconds(60))
        let lt = try #require(pane.term(for: l.key)), rt = try #require(pane.term(for: r.key))
        lt.received(Data("local".utf8))
        rt.received(Data("remote".utf8))
        #expect(lt.phase == .live && rt.phase == .live)

        // The remote's connection drops and comes back: its terminal is a replay again; the local one carries on.
        backend.onConnection?(backend.link(1), false)
        #expect(!pane.termConnected)
        backend.onConnection?(backend.link(1), true)
        #expect(rt.phase == .connecting)
        #expect(lt.phase == .live)
        #expect(pane.termConnected)

        // And the local one's drop touches only the local terminal.
        rt.received(Data("again".utf8))
        backend.onConnection?(backend.link(0), false)
        backend.onConnection?(backend.link(0), true)
        #expect(lt.phase == .connecting)
        #expect(rt.phase == .live)
    }
}

@MainActor @Suite(.serialized) struct MachineListTests {
    private func list() -> (MachineList, UserDefaults, String) {
        let suite = "fleet.tests.\(UUID().uuidString)"
        let d = UserDefaults(suiteName: suite)!
        return (MachineList(defaults: d), d, suite)
    }

    @Test func parseForms() {
        typealias E = MachineList.Entry
        #expect(MachineList.parse("redbox") == E(name: "redbox", host: "redbox", port: 47470))
        #expect(MachineList.parse("  box.example:9000 ") == E(name: "box.example", host: "box.example", port: 9000))
        #expect(MachineList.parse("100.64.0.9") == E(name: "100.64.0.9", host: "100.64.0.9", port: 47470))
        #expect(MachineList.parse("[fd7a::9]:47471") == E(name: "fd7a::9", host: "fd7a::9", port: 47471))
        #expect(MachineList.parse("[fd7a::9]") == E(name: "fd7a::9", host: "fd7a::9", port: 47470))
        #expect(MachineList.parse("fd7a::9") == E(name: "fd7a::9", host: "fd7a::9", port: 47470))  // bare v6: no port
        for bad in ["", "  ", "a b", "box:", "box:0", "box:70000", "box:x", ":47470", "[fd7a::9]x", "[fd7a::9]:"] {
            #expect(MachineList.parse(bad) == nil, "\(bad)")
        }
    }

    @Test func addedMachinesAreKeptAndRemovedByHostAndPort() {
        let (l, d, suite) = list()
        defer { d.removePersistentDomain(forName: suite) }
        var changes = 0
        l.onChange = { changes += 1 }
        #expect(l.usesTailscale)
        #expect(l.add(MachineList.Entry(name: "a", host: "box-a", port: 47470)) == .added)
        #expect(l.add(MachineList.Entry(name: "b", host: "box-b", port: 47470)) == .added)
        #expect(l.add(MachineList.Entry(name: "a again", host: "box-a", port: 47470)) == .already)
        #expect(!l.usesTailscale)
        // A second list over the same defaults reads what the first kept.
        let again = MachineList(defaults: d)
        #expect(again.entries.map(\.host) == ["box-a", "box-b"])
        l.remove(host: "box-a", port: 47470)
        #expect(MachineList(defaults: d).entries.map(\.host) == ["box-b"])
        l.remove(host: "box-a", port: 47470)  // gone already: nothing
        #expect(changes == 3)
        l.useTailscale()
        #expect(l.usesTailscale)
        #expect(l.entries.isEmpty)
    }

    /// A kept list that does not decode (another version's, damaged) falls back to the tailnet's.
    @Test func anUnreadableListFallsBackToTheTailnets() {
        let (l, d, suite) = list()
        defer { d.removePersistentDomain(forName: suite) }
        d.set(Data("not json".utf8), forKey: "FleetMachines")
        d.set(try? JSONEncoder().encode([MachineList.Entry(name: "peer", host: "100.64.0.2", port: 47470)]),
              forKey: "FleetDiscoveredMachines")
        #expect(l.usesTailscale)
        #expect(l.entries.map(\.host) == ["100.64.0.2"])
        d.set(Data("also not json".utf8), forKey: "FleetDiscoveredMachines")
        #expect(l.entries.isEmpty)
    }

    /// M1: this Mac (its Tailscale address and names, its host name, loopback) is never added, nor dialled from a
    /// kept list.
    @Test func thisMacIsRefused() {
        let (l, d, suite) = list()
        defer { d.removePersistentDomain(forName: suite) }
        l.tailnetSelf = ["100.64.0.1", "this-mac", "this-mac.tailnet.example"]
        for host in ["100.64.0.1", "This-Mac", "this-mac.tailnet.example.", "localhost", "127.0.0.1", "::1"] {
            #expect(l.add(MachineList.Entry(name: host, host: host, port: 47470)) == .isThisMac, "\(host)")
        }
        #expect(l.entries.isEmpty)
        d.set(try? JSONEncoder().encode([
            MachineList.Entry(name: "me", host: "100.64.0.1", port: 47470),
            MachineList.Entry(name: "box", host: "box", port: 47470),
        ]), forKey: "FleetMachines")
        #expect(l.entries.map(\.host) == ["box"])
    }
}
