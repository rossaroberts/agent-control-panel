// The other machines of the tailnet that may run a fleetd (Ruling A-R1's default list): every peer that
// `tailscale status --json` lists as a Linux or macOS node of this machine's own Tailscale user, untagged, not this
// machine, at fleetd's port (Ruling ALL-R2). Those are exactly the peers a fleetd listener lets in with no key
// (PROTOCOL.md "Who is allowed"). Read off the main thread, with a time limit: the CLI of the Mac app can be slow.

import Foundation

public enum Tailscale {
    /// fleetd's TCP port on every machine (Ruling ALL-R2).
    public static let fleetdPort: UInt16 = 47470

    public struct Peer: Equatable, Sendable, Codable {
        /// The node's host name, as Tailscale shows it.
        public var name: String
        /// Its Tailscale IPv4 address (a fleetd listener binds IPv4 only).
        public var address: String

        public init(name: String, address: String) {
            self.name = name
            self.address = address
        }

        public var endpoint: Endpoint { .tcp(host: address, port: Tailscale.fleetdPort) }
    }

    /// The peers in a `tailscale status --json` document that may run a fleetd, in host-name order. Nil when it is not
    /// such a document.
    public static func peers(fromStatus data: Data) -> [Peer]? {
        guard let doc = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let me = doc["Self"] as? [String: Any]
        else { return nil }
        let user = (me["UserID"] as? NSNumber)?.int64Value
        let mine = Set((me["TailscaleIPs"] as? [String]) ?? [])
        let list = (doc["Peer"] as? [String: Any])?.values.compactMap { $0 as? [String: Any] } ?? []
        var out: [Peer] = []
        for p in list {
            guard let os = p["OS"] as? String, os == "linux" || os == "macOS",
                let u = (p["UserID"] as? NSNumber)?.int64Value, u == user,
                ((p["Tags"] as? [Any]) ?? []).isEmpty,
                let ip = ((p["TailscaleIPs"] as? [String]) ?? []).first(where: isIPv4), !mine.contains(ip)
            else { continue }
            let name = (p["HostName"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? ip
            out.append(Peer(name: name, address: ip))
        }
        return out.sorted { ($0.name, $0.address) < ($1.name, $1.address) }
    }

    private static func isIPv4(_ s: String) -> Bool {
        let parts = s.split(separator: ".", omittingEmptySubsequences: false)
        return parts.count == 4 && parts.allSatisfy { UInt8($0) != nil }
    }

#if os(macOS)
    /// Where the CLI is: the Mac app's own binary, else one on the usual paths.
    public static let cliPaths = [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale",
    ]

    /// Runs `tailscale status --json` on a thread of its own (never the main one), killed after `timeout`; the peers,
    /// or nil when there is no CLI, it fails, or it is too slow.
    public static func discover(timeout: Duration = .seconds(2), cli: [String] = cliPaths) async -> [Peer]? {
        guard let path = cli.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) else { return nil }
        return await withCheckedContinuation { (c: CheckedContinuation<[Peer]?, Never>) in
            let t = Thread { c.resume(returning: run(path, timeout: timeout)) }
            t.name = "FleetKit.Tailscale"
            t.qualityOfService = .utility
            t.start()
        }
    }

    private static func run(_ path: String, timeout: Duration) -> [Peer]? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: path)
        p.arguments = ["status", "--json"]
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        p.standardInput = FileHandle.nullDevice
        let done = DispatchSemaphore(value: 0)
        p.terminationHandler = { _ in done.signal() }
        do { try p.run() } catch { return nil }
        // Read while it runs (a big status would fill the pipe), and stop it if it takes too long.
        let box = Box()
        let read = DispatchSemaphore(value: 0)
        let reader = Thread {
            box.set(out.fileHandleForReading.readDataToEndOfFile())
            read.signal()
        }
        reader.start()
        let secs = Double(timeout.components.seconds) + Double(timeout.components.attoseconds) / 1e18
        if done.wait(timeout: .now() + secs) == .timedOut {
            p.terminate()  // our own child, by its pid
            _ = done.wait(timeout: .now() + 1)
            return nil
        }
        _ = read.wait(timeout: .now() + 1)
        guard p.terminationStatus == 0 else { return nil }
        return peers(fromStatus: box.get())
    }

    private final class Box: @unchecked Sendable {
        private let lock = NSLock()
        private var data = Data()
        func set(_ d: Data) { lock.withLock { data = d } }
        func get() -> Data { lock.withLock { data } }
    }
#endif
}
