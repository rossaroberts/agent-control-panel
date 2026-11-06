// The toolbar, in AppKit (Ruling P3-R6): over the sidebar, the accounts' limit meters (fleet.html `renderAccounts`,
// lines 719-735: the window, a thin line, the percent); over the detail, the machine's health (fleet.html `vitals`,
// lines 1028-1037: dev, db, cpu, mem, disk) and the connection ("live", "reconnecting…", "fleetd not running", or the
// daemon's refusal).

import AppKit
import FleetKit

@MainActor
final class BoardToolbar: NSObject, NSToolbarDelegate {
    private static let accountsId = NSToolbarItem.Identifier("accounts")
    private static let vitalsId = NSToolbarItem.Identifier("vitals")
    private static let connectionId = NSToolbarItem.Identifier("connection")

    let toolbar = NSToolbar(identifier: "FleetBoard")
    private let accounts = RunsView()
    private let vitals = RunsView()
    private let connection = RunsView()
    private var shown: (Meta, String?, BoardClient.Status)?

    override init() {
        super.init()
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        toolbar.allowsUserCustomization = false
    }

    /// The room each side has (the sidebar's width less the window buttons; the detail's less its title and the
    /// connection), from the split view; the meters that do not fit are left out from the right.
    private var room: (accounts: CGFloat, vitals: CGFloat) = (.greatestFiniteMagnitude, .greatestFiniteMagnitude)

    func fit(sidebar: CGFloat, detail: CGFloat) {
        // The window buttons and the toolbar's own padding take ~150 pt of the sidebar; the title ~150 pt of the
        // detail, and each item ~30 pt of padding.
        let r = (max(0, sidebar - 150), max(0, detail - 150 - connection.intrinsicContentSize.width - 70))
        guard r != room else { return }
        room = r
        if let s = shown { draw(s.0, s.1, s.2) }
    }

    func show(meta: Meta, machine: String?, status: BoardClient.Status) {
        if let s = shown, s.0 == meta, s.1 == machine, s.2 == status { return }
        shown = (meta, machine, status)
        draw(meta, machine, status)
    }

    private func draw(_ meta: Meta, _ machine: String?, _ status: BoardClient.Status) {
        accounts.fit(Self.accounts(meta.accounts), maxWidth: room.accounts, gap: 14)
        accounts.toolTip = meta.accounts.map { "\($0.name) · \($0.engine) · \($0.profile)" }.joined(separator: "\n")
        vitals.fit(Self.vitals(machine, meta.health), maxWidth: room.vitals, gap: 12)
        vitals.toolTip = meta.health.map { "\($0.dev) dev servers, \($0.db) database stacks" }
        // The connection always shows, whole.
        let (word, colour, help) = Self.connection(status)
        connection.runs = [.gap(10), .dot(colour, ring: false), .gap(5), .text(word, .labelColor, Fonts.small)]
        connection.toolTip = help
    }

    /// One piece per account: its name and its meters.
    private static func accounts(_ list: [Account]) -> [[RunsView.Run]] {
        list.map { a in
            var out: [RunsView.Run] = [.text(a.name, .labelColor, Fonts.small)]
            if a.limits.isEmpty { out += [.gap(6), .text("·", .secondaryLabelColor, Fonts.small)] }
            for l in a.limits {
                let pct = Double(l.usedPct), c = Palette.level(pct)
                out += [.gap(6), .text(l.window, .secondaryLabelColor, Fonts.small), .gap(5),
                        .bar(pct / 100, c ?? Palette.grey, 28), .gap(5),
                        .text("\(l.usedPct)%", c ?? .secondaryLabelColor, Fonts.small)]
            }
            return out
        }
    }

    /// One piece per vital, the machine's name first.
    private static func vitals(_ machine: String?, _ h: Health?) -> [[RunsView.Run]] {
        var out: [[RunsView.Run]] = []
        if let machine { out.append([.text(machine, .secondaryLabelColor, Fonts.small)]) }
        guard let h else { return out }
        let items: [(String, String, Double?)] = [
            ("dev", "\(h.dev)", nil), ("db", "\(h.db)", nil), ("cpu", "\(h.cpuPct)%", Double(h.cpuPct)),
            ("mem", "\(h.memPct)%", Double(h.memPct)), ("disk", "\(h.diskPct)%", Double(h.diskPct)),
        ]
        for (k, v, pct) in items {
            out.append([.text(k, .secondaryLabelColor, Fonts.small), .gap(4),
                        .text(v, pct.flatMap(Palette.level) ?? .labelColor, Fonts.small)])
        }
        return out
    }

    private static func connection(_ s: BoardClient.Status) -> (String, NSColor, String) {
        switch s {
        case .live: ("live", Palette.green, "Connected to fleetd")
        case .reconnecting: ("reconnecting…", Palette.orange, "The connection to fleetd was lost; trying again")
        case .absent: ("fleetd not running", Palette.red, "No fleetd at its socket. Install it with: fleetd install-service")
        case let .refused(why): ("fleetd refused", Palette.red, why)
        }
    }

    // MARK: - NSToolbarDelegate

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [Self.accountsId, .sidebarTrackingSeparator, .flexibleSpace, Self.vitalsId, Self.connectionId]
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        toolbarDefaultItemIdentifiers(toolbar)
    }

    func toolbar(
        _ toolbar: NSToolbar, itemForItemIdentifier id: NSToolbarItem.Identifier, willBeInsertedIntoToolbar: Bool
    ) -> NSToolbarItem? {
        let view: RunsView
        switch id {
        case Self.accountsId: view = accounts
        case Self.vitalsId: view = vitals
        case Self.connectionId: view = connection
        default: return nil
        }
        let item = NSToolbarItem(itemIdentifier: id)
        item.view = view
        item.label = id.rawValue
        return item
    }
}
