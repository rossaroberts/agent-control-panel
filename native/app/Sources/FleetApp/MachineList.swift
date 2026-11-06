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
    /// The machine an endpoint duplicates, when its fleetd reports another's name (for the menu).
    var duplicateOf: ((Endpoint) -> String?)?

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

    /// This Mac's names and addresses on the tailnet (from the last status read); with `localNames`, what is never
    /// dialled over TCP (M1): this Mac's board is its Unix socket.
    var tailnetSelf: Set<String> = []

    /// This Mac's own names and loopback: gethostname(2), which asks no resolver (launch must not wait on DNS).
    static let localNames: Set<String> = {
        var out: Set<String> = ["localhost", "127.0.0.1", "::1"]
        var buf = [CChar](repeating: 0, count: 256)
        if gethostname(&buf, buf.count) == 0 {
            let host = String(cString: buf).lowercased()
            out.insert(host)
            if let short = host.split(separator: ".").first {
                out.insert(String(short))
                out.insert("\(short).local")
            }
        }
        return out
    }()

    /// Whether `e` is this Mac itself (its Tailscale address or name, its host name, loopback).
    func isSelf(_ e: Entry) -> Bool {
        let h = e.host.lowercased()
        let bare = h.hasSuffix(".") ? String(h.dropLast()) : h
        return tailnetSelf.contains(bare) || Self.localNames.contains(bare) || bare.hasPrefix("127.")
    }

    /// The machines now (besides this Mac).
    var entries: [Entry] { (chosen ?? discovered).filter { !isSelf($0) } }
    var endpoints: [Endpoint] { entries.map(\.endpoint) }

    /// Reads the tailnet (off the main thread); when its peers differ from the last, and the list is the tailnet's,
    /// the board follows.
    func discover() {
        Task { @MainActor [weak self] in
            guard let status = await Tailscale.discover() else { return }
            guard let self else { return }
            self.tailnetSelf = status.me
            let found = status.peers.map { Entry(name: $0.name, host: $0.address, port: Tailscale.fleetdPort) }
                .filter { !self.isSelf($0) }
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

    enum AddResult: Equatable { case added, already, isThisMac }

    /// Adds `e`, unless it is already listed or is this Mac (whose board is its own socket, never TCP).
    @discardableResult func add(_ e: Entry) -> AddResult {
        guard !isSelf(e) else { return .isThisMac }
        var list = entries
        guard !list.contains(where: { $0.host == e.host && $0.port == e.port }) else { return .already }
        list.append(e)
        chosen = list
        onChange?()
        return .added
    }

    /// Removes the machine at `host`:`port` (whatever its place in the list now, M3).
    func remove(host: String, port: UInt16) {
        var list = entries
        guard let i = list.firstIndex(where: { $0.host == host && $0.port == port }) else { return }
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
        for e in entries {
            let known = nameOf?(e.endpoint).flatMap { $0.isEmpty ? nil : $0 }
            let twin = duplicateOf?(e.endpoint).map { ", duplicate of \($0)" } ?? ""
            let title = "Remove \(known ?? e.name) (\(e.host):\(e.port)\(twin))…"
            let item = NSMenuItem(title: title, action: #selector(removeChosen(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = [e.name, e.host, String(e.port)]
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
        if add(e) == .isThisMac {
            let no = NSAlert()
            no.messageText = "That is this Mac"
            no.informativeText = "This Mac's board is always on, through its own fleetd. Add another machine."
            no.runModal()
        }
    }

    @objc private func removeChosen(_ sender: NSMenuItem) {
        // The entry the item was made for, by host and port: never a place in a list that may have changed since.
        guard let r = sender.representedObject as? [String], r.count == 3, let port = UInt16(r[2]) else { return }
        let e = Entry(name: r[0], host: r[1], port: port)
        let alert = NSAlert()
        alert.messageText = "Remove \(e.name) from the board?"
        alert.informativeText = "Its rows leave the board. Nothing on \(e.name) changes; add it again any time."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        remove(host: e.host, port: e.port)
    }

    @objc private func useTailscalePeers(_ sender: Any?) { useTailscale() }
}
