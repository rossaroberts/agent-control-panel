// The other machines on the board (Ruling A-R1), besides this Mac's own fleetd, which is always there over its Unix
// socket (never TCP to itself). By default: every Tailscale peer that may run a fleetd (Tailscale.peers: Linux or
// macOS, this user's, untagged, not this Mac) at port 47470. Once Ross adds or removes one in the Machines menu, the
// list is his, kept in UserDefaults, until "Use Tailscale Peers" hands it back to the tailnet.
//
// The tailnet is read off the main thread with a time limit, after the first paint. The last peers it gave are kept,
// so a launch connects to them at once (ALL-R1: nothing waits on the CLI).

import AppKit
import FleetKit

@MainActor
final class MachineList: NSObject, NSMenuDelegate {
    /// One machine: what the menu calls it, and where its fleetd listens.
    struct Entry: Equatable, Codable {
        var name: String
        var host: String
        var port: UInt16

        var endpoint: Endpoint { .tcp(host: host, port: port) }
    }

    private static let chosenKey = "FleetMachines"
    private static let discoveredKey = "FleetDiscoveredMachines"

    private let defaults: UserDefaults
    /// Told when the list changes (the board's machines follow it).
    var onChange: (() -> Void)?
    /// The machine names as the board knows them, by endpoint (for the menu; "" until known).
    var nameOf: ((Endpoint) -> String?)?

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// Ross's own list, or nil while the tailnet's is used.
    private var chosen: [Entry]? {
        get { Self.read(defaults.data(forKey: Self.chosenKey)) }
        set {
            if let newValue { defaults.set(try? JSONEncoder().encode(newValue), forKey: Self.chosenKey) } else {
                defaults.removeObject(forKey: Self.chosenKey)
            }
        }
    }

    private var discovered: [Entry] {
        get { Self.read(defaults.data(forKey: Self.discoveredKey)) ?? [] }
        set { defaults.set(try? JSONEncoder().encode(newValue), forKey: Self.discoveredKey) }
    }

    private static func read(_ d: Data?) -> [Entry]? { d.flatMap { try? JSONDecoder().decode([Entry].self, from: $0) } }

    var usesTailscale: Bool { chosen == nil }

    /// The machines now (besides this Mac).
    var entries: [Entry] { chosen ?? discovered }
    var endpoints: [Endpoint] { entries.map(\.endpoint) }

    /// Reads the tailnet (off the main thread); when its peers differ from the last, and the list is the tailnet's,
    /// the board follows.
    func discover() {
        Task { @MainActor [weak self] in
            guard let peers = await Tailscale.discover() else { return }
            guard let self else { return }
            let found = peers.map { Entry(name: $0.name, host: $0.address, port: Tailscale.fleetdPort) }
            guard found != self.discovered else { return }
            self.discovered = found
            if self.usesTailscale { self.onChange?() }
        }
    }

    /// "host", "host:port" or "[v6]:port" as typed: the entry, or nil when it is not one.
    static func parse(_ typed: String) -> Entry? {
        let t = typed.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty, !t.contains(" ") else { return nil }
        var host = t, port = Tailscale.fleetdPort
        if t.hasPrefix("["), let close = t.firstIndex(of: "]") {
            host = String(t[t.index(after: t.startIndex)..<close])
            let rest = t[t.index(after: close)...]
            if rest.hasPrefix(":") {
                guard let p = UInt16(rest.dropFirst()) else { return nil }
                port = p
            } else if !rest.isEmpty {
                return nil
            }
        } else if t.filter({ $0 == ":" }).count == 1, let colon = t.lastIndex(of: ":") {
            host = String(t[..<colon])
            guard let p = UInt16(t[t.index(after: colon)...]), p > 0 else { return nil }
            port = p
        }
        guard !host.isEmpty else { return nil }
        return Entry(name: host, host: host, port: port)
    }

    func add(_ e: Entry) {
        var list = entries
        guard !list.contains(where: { $0.host == e.host && $0.port == e.port }) else { return }
        list.append(e)
        chosen = list
        onChange?()
    }

    func remove(at i: Int) {
        var list = entries
        guard list.indices.contains(i) else { return }
        list.remove(at: i)
        chosen = list
        onChange?()
    }

    func useTailscale() {
        chosen = nil
        onChange?()
        discover()
    }

    // MARK: - the Machines menu

    /// "Machines": this Mac, then each machine (choosing one removes it, after asking), "Add Machine…", and "Use
    /// Tailscale Peers". Rebuilt each time it opens.
    func menuItem() -> NSMenuItem {
        let m = NSMenu(title: "Machines")
        m.delegate = self
        let top = NSMenuItem(title: "Machines", action: nil, keyEquivalent: "")
        top.submenu = m
        return top
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let here = NSMenuItem(title: "This Mac (its own fleetd)", action: nil, keyEquivalent: "")
        here.isEnabled = false
        menu.addItem(here)
        for (i, e) in entries.enumerated() {
            let known = nameOf?(e.endpoint).flatMap { $0.isEmpty ? nil : $0 }
            let title = "Remove \(known ?? e.name) (\(e.host):\(e.port))…"
            let item = NSMenuItem(title: title, action: #selector(removeChosen(_:)), keyEquivalent: "")
            item.target = self
            item.tag = i
            menu.addItem(item)
        }
        menu.addItem(.separator())
        let add = NSMenuItem(title: "Add Machine…", action: #selector(addMachine(_:)), keyEquivalent: "")
        add.target = self
        menu.addItem(add)
        let ts = NSMenuItem(title: "Use Tailscale Peers", action: #selector(useTailscalePeers(_:)), keyEquivalent: "")
        ts.target = self
        ts.state = usesTailscale ? .on : .off
        menu.addItem(ts)
    }

    @objc private func addMachine(_ sender: Any?) {
        let alert = NSAlert()
        alert.messageText = "Add a machine"
        alert.informativeText = "Its Tailscale name or address, and the port if not 47470 (host or host:port). Its fleetd must listen on the tailnet."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        field.placeholderString = "host or host:port"
        alert.accessoryView = field
        alert.addButton(withTitle: "Add")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        guard let e = Self.parse(field.stringValue) else {
            NSSound.beep()
            return
        }
        add(e)
    }

    @objc private func removeChosen(_ sender: NSMenuItem) {
        guard entries.indices.contains(sender.tag) else { return }
        let e = entries[sender.tag]
        let alert = NSAlert()
        alert.messageText = "Remove \(e.name) from the board?"
        alert.informativeText = "Its rows leave the board. Nothing on \(e.name) changes; add it again any time."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        remove(at: sender.tag)
    }

    @objc private func useTailscalePeers(_ sender: Any?) { useTailscale() }
}
