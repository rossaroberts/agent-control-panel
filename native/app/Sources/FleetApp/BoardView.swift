// The board's sidebar: a view-based NSTableView with reused cells (Ruling P3-R6), grouped as `public/fleet.html` groups
// it (BoardModel). Each coalesced render (at most one per display frame) is turned into a row edit (BoardRows) and
// applied as removals, insertions and in-place reloads, never a reloadData per frame. The selection is a SessionKey:
// a row keeps it wherever it moves. Done or ended rows older than 48 h sit behind an "N older" line whose open state
// is kept per group, as fleet.html keeps it.

import AppKit
import Combine
import FleetKit

@MainActor
final class BoardController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    let model: BoardModel
    let table = NSTableView()
    private let scroll = NSScrollView()
    private let empty = label(Fonts.small, align: .center)
    private(set) var rows: [BoardRow] = []
    private var applying = false
    private var watches: [AnyCancellable] = []
    private static let openKey = "openOlder"
    private var openOlder: Set<String>
    /// Called after every applied render (the bench draws and times it); and when the selection changes.
    var onApplied: ((_ cached: Bool) -> Void)?
    var onSelect: (() -> Void)?

    init(model: BoardModel) {
        self.model = model
        openOlder = Set(UserDefaults.standard.stringArray(forKey: Self.openKey) ?? [])
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let col = NSTableColumn(identifier: .init("board"))
        col.resizingMask = .autoresizingMask
        table.addTableColumn(col)
        table.headerView = nil
        table.style = .sourceList
        table.rowSizeStyle = .custom
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.allowsEmptySelection = true
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(clicked)
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        empty.maximumNumberOfLines = 3
        empty.usesSingleLineMode = false
        empty.lineBreakMode = .byWordWrapping
        // Frames, not constraints: no Auto Layout engine starts for the board at launch.
        empty.frame = NSRect(x: 20, y: 0, width: 500, height: 60)
        empty.autoresizingMask = [.width, .minYMargin, .maxYMargin]
        scroll.frame = NSRect(x: 0, y: 0, width: 540, height: 400)
        empty.frame.origin.y = (scroll.bounds.height - 60) / 2
        empty.frame.size.width = scroll.bounds.width - 40
        scroll.addSubview(empty)
        view = scroll
        model.onRender { [weak self] cached in self?.apply(cached: cached) }
        // A status change is a render of its own (the note under "from <time>", the empty board's words).
        watches.append(model.$status.receive(on: RunLoop.main).sink { [weak self] _ in self?.apply(cached: nil) })
        apply(cached: nil)
    }

    /// The notes above the groups: the cached board's time, and why fleetd is not answering while it shows.
    private func notes() -> [BoardRow] {
        guard let at = model.cachedAt else { return [] }
        var out: [BoardRow] = [.note(id: "from", text: "from \(Words.time(at))")]
        switch model.status {
        case .absent: out.append(.note(id: "why", text: "fleetd not running. Install it with: fleetd install-service"))
        case let .refused(why): out.append(.note(id: "why", text: "fleetd refused: \(why)"))
        case .live, .reconnecting: break
        }
        return out
    }

    func apply(cached: Bool?) {
        let next = notes() + BoardRows.flatten(model.groups, openOlder: openOlder, cached: model.cachedAt != nil)
        applying = true
        defer { applying = false }
        if rows.isEmpty || next.isEmpty {
            rows = next
            table.reloadData()
        } else {
            let edit = BoardRows.edit(from: rows, to: next)
            if !edit.isEmpty {
                rows = next
                if !edit.removed.isEmpty || !edit.inserted.isEmpty {
                    table.beginUpdates()
                    table.removeRows(at: edit.removed, withAnimation: [])
                    table.insertRows(at: edit.inserted, withAnimation: [])
                    table.endUpdates()
                }
                if !edit.reloaded.isEmpty { table.reloadData(forRowIndexes: edit.reloaded, columnIndexes: [0]) }
            }
        }
        // The selection follows its key to wherever the row is now.
        let at = model.selection.flatMap { k in rows.firstIndex { $0.session?.key == k } }
        if let at {
            if table.selectedRow != at { table.selectRowIndexes([at], byExtendingSelection: false) }
        } else if table.selectedRow >= 0 {
            table.deselectAll(nil)
        }
        empty.isHidden = !rows.isEmpty
        if rows.isEmpty { empty.stringValue = emptyWords() }
        if let cached { onApplied?(cached) }
        onSelect?()
    }

    private func emptyWords() -> String {
        switch model.status {
        case .absent: "fleetd not running\nFleet shows what fleetd sees. Install it with: fleetd install-service"
        case let .refused(why): "fleetd refused the connection\n\(why)"
        case .reconnecting: "connecting…"
        case .live: "No sessions"
        }
    }

    /// The 30 s tick: the 48 h fold is re-read (through the model, coalesced), and the visible rows' ages move.
    /// Nothing else is redrawn.
    func tick() {
        model.refresh()
        let now = Date()
        let visible = table.rows(in: table.visibleRect)
        for r in visible.lowerBound..<visible.upperBound {
            (table.view(atColumn: 0, row: r, makeIfNecessary: false) as? RowCell)?.tick(now: now)
        }
    }

    /// Lays out and draws the table now and hands it to the window server (the bench's "on screen").
    func paint() {
        table.layoutSubtreeIfNeeded()
        table.displayIfNeeded()
        view.window?.displayIfNeeded()
        CATransaction.flush()
    }

    @objc private func clicked() {
        let r = table.clickedRow
        guard r >= 0, r < rows.count, case let .older(g, _, open) = rows[r] else { return }
        if open { openOlder.remove(g) } else { openOlder.insert(g) }
        UserDefaults.standard.set(openOlder.sorted(), forKey: Self.openKey)
        apply(cached: nil)
    }

    // MARK: - table

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        switch rows[row] {
        case .session: 30
        case .header: 34
        case .older: 26
        case .note: 20
        }
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        func reuse<T: NSTableCellView>(_ id: NSUserInterfaceItemIdentifier, _ make: () -> T) -> T {
            (tableView.makeView(withIdentifier: id, owner: nil) as? T) ?? make()
        }
        switch rows[row] {
        case let .session(s, cached):
            let c = reuse(RowCell.id, RowCell.init)
            c.show(s, cached: cached)
            return c
        case let .header(_, name, tally):
            let c = reuse(HeaderCell.id, HeaderCell.init)
            c.show(name: name, tally: tally)
            c.needsLayout = true
            return c
        case let .older(_, n, open):
            let c = reuse(LineCell.id, LineCell.init)
            c.text.stringValue = "\(open ? "▾" : "▸") \(n) older"
            c.text.textColor = .secondaryLabelColor
            c.toolTip = "Done or ended more than 48 hours ago"
            return c
        case let .note(id, text):
            let c = reuse(LineCell.id, LineCell.init)
            c.text.stringValue = text
            c.text.textColor = id == "why" ? Palette.red : .secondaryLabelColor
            c.toolTip = id == "from" ? "The board as it was then, until fleetd answers" : nil
            return c
        }
    }

    func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool { rows[row].session != nil }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !applying else { return }
        let r = table.selectedRow
        model.selection = r >= 0 && r < rows.count ? rows[r].session?.key : nil
        onSelect?()
    }
}
