// The tailnet peers that may run a fleetd, this machine's own names, and the CLI read with a time limit.

import Foundation
import Testing

@testable import FleetKit

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

    /// M1: this machine's own names and addresses, so it is never dialled as a peer.
    @Test func thisMachinesNamesAreKnown() throws {
        let withDNS = status.replacingOccurrences(
            of: #""HostName": "this-mac", "OS""#, with: #""HostName": "This-Mac", "DNSName": "this-mac.tailnet.example.", "OS""#)
        let st = try #require(Tailscale.status(from: Data(withDNS.utf8)))
        #expect(st.me == ["100.64.0.1", "fd7a::1", "this-mac", "this-mac.tailnet.example"])
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
        let st = await Tailscale.discover(cli: [cli.path])
        #expect(st?.peers.map(\.name) == ["linux-box", "other-mac"])
        #expect(st?.me.contains("this-mac") == true)
    }
}
