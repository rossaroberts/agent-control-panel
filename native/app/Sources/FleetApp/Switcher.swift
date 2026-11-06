// ⌘K, the quick switcher (fleet.html `finder`): a box over the window, a field and the matching sessions as rail rows.
// Matching is on title, name and job (Rank: exact prefix, then a word start, then the letters in order; ties in the
// board's order). The fields are lowered once when it opens, and each keystroke ranks and reloads a list of at most
// 40 rows: well inside a frame. ↑/↓ move, Return opens the session into its terminal, Esc closes it.

import AppKit
import FleetKit

@MainActor
final class Switcher: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate {
    static let shownMax = 40

    let box = NSVisualEffectView()
    let field = NSTextField()
    private let table = NSTableView()
    private let scroll = NSScrollView()
    private var index = Rank.Index<Session>([], fields: { _ in [] })
    private(set) var found: [Session] = []
    /// The session chosen (Return or a click), and the switcher closing without one (Esc).
    var onChoose: ((Session) -> Void)?
    var onCancel: (() -> Void)?
    var isOpen: Bool { !box.isHidden && box.superview != nil }

    override init() {
        super.init()
        box.material = .popover
        box.blendingMode = .withinWindow
        box.state = .active
        box.wantsLayer = true
        box.layer?.cornerRadius = 8
        box.layer?.borderWidth = 1
        box.layer?.borderColor = NSColor.separatorColor.cgColor
        box.isHidden = true
        field.placeholderString = "Find a session"
        field.font = .systemFont(ofSize: 15)
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.delegate = self
        field.cell?.usesSingleLineMode = true
        let col = NSTableColumn(identifier: .init("found"))
        col.resizingMask = .autoresizingMask
        table.addTableColumn(col)
        table.headerView = nil
        table.style = .plain
        table.rowSizeStyle = .custom
        table.backgroundColor = .clear
        table.intercellSpacing = .zero
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(clicked)
        table.refusesFirstResponder = true
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        box.addSubview(field)
        box.addSubview(scroll)
    }

    /// Made ready in `host` without showing (after launch, so the first ⌘K builds nothing).
    func install(in host: NSView) {
        guard box.superview !== host else { return }
        host.addSubview(box, positioned: .above, relativeTo: nil)
        place()
    }

    /// Made and drawn once out of sight (its first showing otherwise costs ~75 ms of first-time setup).
    func warm(in host: NSView) {
        install(in: host)
        box.alphaValue = 0
        box.isHidden = false
        _ = host.window?.fieldEditor(true, for: field)
        table.reloadData()
        box.displayIfNeeded()
        box.isHidden = true
        box.alphaValue = 1
    }

    /// Centred near the top of its host, at most 560 wide.
    func place() {
        guard let host = box.superview else { return }
        let w = min(560, host.bounds.width - 32), h = min(420, host.bounds.height - 80)
        box.frame = NSRect(x: (host.bounds.width - w) / 2, y: host.bounds.height - h - 60, width: w, height: h)
        field.frame = NSRect(x: 14, y: h - 38, width: w - 28, height: 24)
        scroll.frame = NSRect(x: 0, y: 6, width: w, height: h - 50)
    }

    /// Opens over `host` with every session of the board, in its order.
    func open(in host: NSView, sessions: [Session]) {
        install(in: host)
        place()
        index = Rank.Index(sessions) { s in [Words.title(s), s.name, s.jobId].compactMap { $0 } }
        field.stringValue = ""
        box.isHidden = false
        filter()
        host.window?.makeFirstResponder(field)
    }

    func close() {
        guard !box.isHidden else { return }
        box.isHidden = true
        found = []
    }

    /// One keystroke's work: rank, reload, select the first.
    func filter() {
        found = Array(index.order(query: field.stringValue).prefix(Self.shownMax))
        table.reloadData()
        if !found.isEmpty { table.selectRowIndexes([0], byExtendingSelection: false) }
        table.scrollRowToVisible(0)
    }

    private func move(_ by: Int) {
        guard !found.isEmpty else { return }
        let r = max(0, min(found.count - 1, table.selectedRow + by))
        table.selectRowIndexes([r], byExtendingSelection: false)
        table.scrollRowToVisible(r)
    }

    private func choose(_ row: Int) {
        guard row >= 0, row < found.count else { return }
        let s = found[row]
        close()
        onChoose?(s)
    }

    @objc private func clicked() { choose(table.clickedRow) }

    // MARK: - field

    func controlTextDidChange(_ obj: Notification) { filter() }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy sel: Selector) -> Bool {
        switch sel {
        case #selector(NSResponder.moveUp(_:)): move(-1)
        case #selector(NSResponder.moveDown(_:)): move(1)
        case #selector(NSResponder.insertNewline(_:)): choose(table.selectedRow)
        case #selector(NSResponder.cancelOperation(_:)):
            close()
            onCancel?()
        default: return false
        }
        return true
    }

    // MARK: - table

    func numberOfRows(in tableView: NSTableView) -> Int { found.count }
    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat { RowLayout.height }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let c = (tableView.makeView(withIdentifier: RowCell.id, owner: nil) as? RowCell) ?? RowCell()
        c.show(found[row], cached: false)
        return c
    }
}
