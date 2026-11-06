// The keyboard, and the menus that carry every key with its shortcut (plan 3 Task 6). Navigator is the one place that
// moves between sessions: the rail's selection, explicit opens, history (⌘[ ⌘]), the Nth needs-you (⌘1–⌘9), the
// quick switcher (⌘K) and the full-window terminal (⌘⇧F).
//
//   ↑ ↓            the rail (the table's own keys); a row settled on for 300 ms opens, passing through opens nothing
//   Return, click  opens the session's terminal and gives it the keyboard
//   ⌃`             terminal ↔ rail (fleet.html's key). Esc is the program's inside a terminal (Claude Code's interrupt,
//                  as in fleet.html: "Esc is Claude Code's there"); in the switcher it closes it
//   ⌘↑ ⌘↓          the previous / next session, from anywhere (the terminal keeps the keyboard if it had it)
//   ⌘1–⌘9          the Nth "Needs you"
//   ⌘K             the quick switcher
//   ⌘[ ⌘]          back / forward through the sessions opened
//   ⌘⇧F            the terminal takes the whole window (the rail folds away), and back

import AppKit
import FleetKit

@MainActor
final class Navigator: NSObject, NSMenuItemValidation {
    let model: BoardModel
    let rail: BoardController
    let pane: TerminalPaneController
    /// Made after launch (`prepare`), never on the launch path.
    lazy var switcher: Switcher = {
        let s = Switcher()
        s.onChoose = { [weak self] s in self?.open(s.key, focus: true) }
        s.onCancel = { [weak self] in
            guard let self else { return }
            self.window?.makeFirstResponder(self.before ?? self.rail.table)
        }
        return s
    }()
    private var switcherMade = false
    weak var split: NSSplitViewController?
    private(set) var history = History<SessionKey>()
    private var settle: DispatchWorkItem?
    /// Where the keyboard was when the switcher opened (Esc gives it back).
    private weak var before: NSResponder?
    /// The menu items with a modified key, which `keyDown` handles ahead of the menus.
    private var keyed: [NSMenuItem] = []
    private var monitor: Any?

    init(model: BoardModel, rail: BoardController, pane: TerminalPaneController) {
        self.model = model
        self.rail = rail
        self.pane = pane
        super.init()
        rail.onUserSelect = { [weak self] in self?.railSelected() }
        rail.onOpen = { [weak self] in self?.enter() }
        rail.table.onReturn = { [weak self] in self?.enter() }
    }

    private var window: NSWindow? { rail.view.window }

    /// After the first paint, out of sight: the switcher is made and drawn once, and the text input system (whose
    /// first key otherwise costs 100–600 ms, measured) takes a throwaway key, so the first ⌘K and the first keystroke
    /// in a terminal are as quick as the rest.
    func prepare() {
        guard let host = split?.view else { return }
        switcherMade = true
        switcher.warm(in: host)
        let scratch = NSTextView(frame: NSRect(x: 0, y: 0, width: 10, height: 10))
        if let ctx = scratch.inputContext, let e = NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
            windowNumber: 0, context: nil, characters: "a", charactersIgnoringModifiers: "a", isARepeat: false,
            keyCode: 0)
        {
            ctx.activate()
            _ = ctx.handleEvent(e)
            ctx.deactivate()
        }
    }

    // MARK: - moving

    /// The rail's selection moved (↑/↓ or a click): the session shows; settled on, it goes into the history.
    private func railSelected() {
        settle?.cancel()
        let s = model.selected
        pane.show(s, open: false, focus: false)
        guard let k = s?.key else { return }
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.model.selection == k else { return }
            self.history.push(k)
        }
        settle = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3, execute: work)
    }

    /// Opens `key` now, selected in the rail; `focus` gives its terminal the keyboard.
    func open(_ key: SessionKey, focus: Bool, push: Bool = true) {
        settle?.cancel()
        model.selection = key
        rail.select(key)
        pane.show(model.selected, open: true, focus: focus)
        if !focus, pane.terminalHasFocus == false, window?.firstResponder !== rail.table, !switcherOpen {
            window?.makeFirstResponder(rail.table)
        }
        if push { history.push(key) }
    }

    /// Return in the rail, a click on a row, or ⌃` from the rail: the shown session's terminal, opened if need be,
    /// takes the keyboard.
    @objc func enter() {
        settle?.cancel()
        if let k = model.selection, pane.shownKey != k { pane.show(model.selected, open: true, focus: true) }
        pane.activate()
        if let k = model.selection { history.push(k) }
    }

    /// The keyboard to the rail, on the session shown.
    @objc func toRail() {
        window?.makeFirstResponder(rail.table)
    }

    @objc func toggleFocus(_ sender: Any?) {
        if pane.terminalHasFocus { toRail() } else { enter() }
    }

    @objc func back(_ sender: Any?) {
        if let k = history.back(where: { self.exists($0) }) { open(k, focus: pane.terminalHasFocus, push: false) }
    }

    @objc func forward(_ sender: Any?) {
        if let k = history.forward(where: { self.exists($0) }) { open(k, focus: pane.terminalHasFocus, push: false) }
    }

    @objc func needsYou(_ sender: NSMenuItem) {
        let rows = model.groups.first { $0.kind == .needsYou }?.rows ?? []
        let n = sender.tag
        guard n >= 1, n <= rows.count else { return }
        open(rows[n - 1].key, focus: pane.terminalHasFocus)
    }

    @objc func previous(_ sender: Any?) { step(-1) }
    @objc func next(_ sender: Any?) { step(1) }

    private func step(_ by: Int) {
        let keys = rail.rows.compactMap { $0.session?.key }
        guard !keys.isEmpty else { return }
        let at = model.selection.flatMap { keys.firstIndex(of: $0) }
        let i = at.map { max(0, min(keys.count - 1, $0 + by)) } ?? (by > 0 ? 0 : keys.count - 1)
        guard keys[i] != model.selection else { return }
        open(keys[i], focus: pane.terminalHasFocus)
    }

    @objc func quickSwitcher(_ sender: Any?) {
        guard let host = split?.view else { return }
        switcherMade = true
        if switcher.isOpen {
            switcher.close()
            window?.makeFirstResponder(before ?? rail.table)
            return
        }
        before = window?.firstResponder
        if let fr = before as? NSTextView, fr.isFieldEditor { before = rail.table }
        // The board's order: Needs you first, then the groups, folded rows last.
        let all = model.groups.flatMap { $0.rows } + model.groups.flatMap { $0.older }
        switcher.open(in: host, sessions: all)
    }

    /// ⌘⇧F: the rail folds away and the terminal has the window; again, and the rail is back.
    @objc func fullWindow(_ sender: Any?) {
        guard let side = split?.splitViewItems.first else { return }
        let folding = !side.isCollapsed
        side.isCollapsed = folding
        if folding { pane.focusTerminal() } else if !pane.terminalHasFocus { toRail() }
    }

    var isFullWindow: Bool { split?.splitViewItems.first?.isCollapsed ?? false }

    private func exists(_ k: SessionKey) -> Bool { model.board.sessions[k] != nil }

    // MARK: - keys

    /// The app's own modified keys are done here, before AppKit offers the key to the menus: a menu key equivalent
    /// first flashes its menu's title, which held the action back ~95 ms (measured), six frames. The menus still list
    /// every key with its shortcut, and the same validation decides whether it acts. Unmodified keys (Return) and the
    /// standard ones (⌘C, ⌘Q…) go the usual way.
    func installKeys() {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            // Local monitors run on the main thread, inside sendEvent.
            nonisolated(unsafe) let event = e
            let done = MainActor.assumeIsolated { self?.keyDown(event) == true }
            return done ? nil : e
        }
    }

    private func keyDown(_ e: NSEvent) -> Bool {
        let mods = e.modifierFlags.intersection([.command, .shift, .control, .option])
        guard !mods.isEmpty, let chars = e.charactersIgnoringModifiers?.lowercased(), e.window === window else {
            return false
        }
        for item in keyed where item.keyEquivalentModifierMask == mods && item.keyEquivalent.lowercased() == chars {
            guard let action = item.action, validateMenuItem(item) else { return false }
            NSApp.sendAction(action, to: self, from: item)
            return true
        }
        return false
    }

    // MARK: - menus

    /// Whether the switcher is up (false before it was ever made).
    var switcherOpen: Bool { switcherMade && switcher.isOpen }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        let busy = switcherOpen
        switch item.action {
        case #selector(back(_:)): return !busy && history.canGoBack
        case #selector(forward(_:)): return !busy && history.canGoForward
        case #selector(enter): return !busy && window?.firstResponder === rail.table && model.selection != nil
        case #selector(previous(_:)), #selector(next(_:)), #selector(toggleFocus(_:)): return !busy
        case #selector(needsYou(_:)):
            let rows = model.groups.first { $0.kind == .needsYou }?.rows ?? []
            if item.tag <= rows.count {
                let t = Words.title(rows[item.tag - 1])
                item.title = "\(item.tag)  \(t.count > 48 ? String(t.prefix(47)) + "…" : t)"
            } else {
                item.title = "Needs You \(item.tag)"
            }
            return !busy && item.tag <= rows.count
        case #selector(fullWindow(_:)):
            item.title = isFullWindow ? "Show the Sessions" : "Full-Window Terminal"
            return true
        default: return true
        }
    }

    func mainMenu() -> NSMenu {
        let bar = NSMenu()
        func item(_ t: String, _ a: Selector?, _ k: String = "", _ mods: NSEvent.ModifierFlags = .command,
                  target: AnyObject? = nil, tag: Int = 0) -> NSMenuItem
        {
            let i = NSMenuItem(title: t, action: a, keyEquivalent: k)
            i.keyEquivalentModifierMask = mods
            i.target = target
            i.tag = tag
            return i
        }
        @discardableResult func menu(_ title: String, _ items: [NSMenuItem]) -> NSMenu {
            let m = NSMenu(title: title)
            items.forEach(m.addItem)
            let top = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            top.submenu = m
            bar.addItem(top)
            return m
        }
        let arrowUp = String(UnicodeScalar(NSUpArrowFunctionKey)!), arrowDown = String(UnicodeScalar(NSDownArrowFunctionKey)!)
        menu("Fleet", [
            item("About Fleet", #selector(NSApplication.orderFrontStandardAboutPanel(_:))), .separator(),
            item("Hide Fleet", #selector(NSApplication.hide(_:)), "h"),
            item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]),
            item("Show All", #selector(NSApplication.unhideAllApplications(_:))), .separator(),
            item("Quit Fleet", #selector(NSApplication.terminate(_:)), "q"),
        ])
        menu("Edit", [
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
        ])
        menu("View", [
            item("Full-Window Terminal", #selector(fullWindow(_:)), "f", [.command, .shift], target: self),
            item("Show Sidebar", #selector(NSSplitViewController.toggleSidebar(_:)), "s", [.command, .control]),
            item("Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]),
        ])
        var go: [NSMenuItem] = [
            item("Quick Switcher…", #selector(quickSwitcher(_:)), "k", target: self), .separator(),
            item("Back", #selector(back(_:)), "[", target: self),
            item("Forward", #selector(forward(_:)), "]", target: self), .separator(),
            item("Previous Session", #selector(previous(_:)), arrowUp, target: self),
            item("Next Session", #selector(next(_:)), arrowDown, target: self),
            // Return opens only while the rail has the keyboard (disabled anywhere else, so a terminal's Return is
            // its program's); the bench checks a terminal still gets it.
            item("Open in Terminal", #selector(enter), "\r", [], target: self),
            item("Terminal / Sessions", #selector(toggleFocus(_:)), "`", .control, target: self),
            .separator(),
        ]
        let head = NSMenuItem(title: "Needs You", action: nil, keyEquivalent: "")
        head.isEnabled = false
        go.append(head)
        for n in 1...9 { go.append(item("Needs You \(n)", #selector(needsYou(_:)), "\(n)", target: self, tag: n)) }
        menu("Go", go)
        keyed = bar.items.compactMap(\.submenu).flatMap(\.items).filter {
            ($0.target as AnyObject?) === self && !$0.keyEquivalentModifierMask.intersection([.command, .control]).isEmpty
        }
        NSApp.windowsMenu = menu("Window", [
            item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"),
            item("Zoom", #selector(NSWindow.performZoom(_:))),
            item("Close", #selector(NSWindow.performClose(_:)), "w"),
        ])
        return bar
    }
}
