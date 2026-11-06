// The main area (Ruling P3-R8: "the terminal pane is the key"): a slim header strip (the dot, the title, the state word
// and the reason; on the right, "connecting…", "reconnecting…" or how it ended) over the session's terminal, a
// SwiftTerm `TerminalView` fed by the app's one Terminals connection (TermClient).
//
// - Keys go to the pty; the pty is resized when the view is, at most once per frame (the latest size).
// - Output is fed on the main actor as it comes; AppKit draws at most once per frame, and a flood gives the run loop a
//   turn every half frame so it never starves drawing or the keyboard. Taking output promptly is what keeps the
//   connection reading (Ruling P3-R5).
// - Warm views: the last 6 Claude terminals opened stay alive out of sight (still taking in what they print), so going
//   back to one is a view swap; the daemon's warm pool is the second layer. A Codex terminal closes when you leave it:
//   there the terminal is the agent itself (fleet.html `trimTerms`, plan 2).
// - A cold open shows the pane and "connecting…" in the same frame, and builds the terminal's view on the next turn;
//   the UI never waits on `claude attach`.
// - Only the terminal on screen follows the pane's size as it changes; those out of sight catch up one per turn
//   once it settles (so ⌘⇧F or a window resize reflows one terminal in the frame, not seven).
// - When fleetd restarts the header says "reconnecting…", and TermClient opens every terminal again on the next
//   connection.

import AppKit
import FleetKit
import SwiftTerm

/// Where a terminal's bytes go to and come from: TermClient in the app, an in-process echo in the bench.
@MainActor protocol TermBackend: AnyObject {
    /// Opens `term.ref` on `target`; output goes to `term.received`, the end to `term.ended`.
    func open(_ term: PaneTerm, target: AttachTarget, cols: UInt16, rows: UInt16)
    func input(_ ref: UInt32, _ data: Data)
    func size(_ ref: UInt32, cols: UInt16, rows: UInt16)
    func close(_ ref: UInt32)
    /// Told whether a machine's Terminals connection is up, now and at every change: the machine (nil when the
    /// backend has one connection for every terminal), and whether it is up.
    var onConnection: ((_ machine: String?, _ up: Bool) -> Void)? { get set }
}

/// The app's backend: one TermClient per machine (Ruling A-R1: a remote session's terminal goes through its machine's
/// endpoint, over TCP, with nothing added between the keystroke and the socket). Every operation goes through one
/// ordered queue, so keystrokes, sizes and closes reach each daemon in the order they were made.
@MainActor final class LiveBackend: TermBackend {
    private enum Op {
        case open(UInt32, AttachTarget, UInt16, UInt16, PaneTerm, TermClient)
        case input(UInt32, Data, TermClient)
        case size(UInt32, UInt16, UInt16, TermClient)
        case close(UInt32, TermClient)
    }

    private let ops: AsyncStream<Op>.Continuation
    /// The client of the machine whose session a terminal is: refs are the pane's, unique across every client.
    private let route: (String) -> TermClient?
    private var clientOf: [UInt32: TermClient] = [:]
    private var watched: Set<ObjectIdentifier> = []
    /// Bytes sent to programs: keystrokes and the terminal's replies to live queries (the live check counts them).
    private(set) var bytesTyped = 0
    var onConnection: ((_ machine: String?, _ up: Bool) -> Void)?

    /// One client for every terminal.
    convenience init(client: TermClient) {
        self.init { _ in client }
        watch(client) { nil }
    }

    /// `route` gives the client of a machine; `watch` each client to hear of its connection.
    init(route: @escaping (String) -> TermClient?) {
        self.route = route
        let (stream, ops) = AsyncStream<Op>.makeStream()
        self.ops = ops
        Task { @MainActor in
            for await op in stream {
                switch op {
                case let .open(ref, target, cols, rows, term, client):
                    let t = await client.open(ref, target: target, cols: cols, rows: rows)
                    term.drain(t)
                case let .input(ref, data, client):
                    // Refused while the connection is down and 64 KiB is held already (A-R2): said, not dropped
                    // silently.
                    if await client.input(ref, data) == .inputFull { NSSound.beep() }
                case let .size(ref, cols, rows, client):
                    await client.size(ref, cols: cols, rows: rows)
                case let .close(ref, client):
                    await client.close(ref)
                }
            }
        }
    }

    /// Follows `client`'s connection, told as `machine()`'s (its name once its board has one).
    func watch(_ client: TermClient, machine: @escaping @MainActor () -> String?) {
        guard watched.insert(ObjectIdentifier(client)).inserted else { return }
        Task { @MainActor [weak self] in
            for await up in await client.connections() { self?.onConnection?(machine(), up) }
        }
    }

    func open(_ term: PaneTerm, target: AttachTarget, cols: UInt16, rows: UInt16) {
        guard let client = route(term.key.machine) else {
            term.ended("\(term.key.machine) is not one of this Mac's machines")
            return
        }
        clientOf[term.ref] = client
        ops.yield(.open(term.ref, target, cols, rows, term, client))
    }

    func input(_ ref: UInt32, _ data: Data) {
        guard let client = clientOf[ref] else { return }
        bytesTyped += data.count
        ops.yield(.input(ref, data, client))
    }

    func size(_ ref: UInt32, cols: UInt16, rows: UInt16) {
        guard let client = clientOf[ref] else { return }
        ops.yield(.size(ref, cols, rows, client))
    }

    func close(_ ref: UInt32) {
        guard let client = clientOf.removeValue(forKey: ref) else { return }
        ops.yield(.close(ref, client))
    }
}

@MainActor enum TermTheme {
    /// fleet.html `TERM_THEME`: the terminal is dark in both appearances.
    static let background = NSColor(srgbRed: 0x16 / 255, green: 0x18 / 255, blue: 0x1a / 255, alpha: 1)
    static let foreground = NSColor(srgbRed: 0xdc / 255, green: 0xdf / 255, blue: 0xe1 / 255, alpha: 1)
    static let cursor = NSColor(srgbRed: 0x3f / 255, green: 0xb6 / 255, blue: 0xee / 255, alpha: 1)
    static let selection = NSColor(srgbRed: 0x3f / 255, green: 0xb6 / 255, blue: 0xee / 255, alpha: 0.3)
    static let font = NSFont.monospacedSystemFont(ofSize: 12.5, weight: .regular)
}

/// One terminal of the pane: its view, its ref on the connection, and where it is in its life.
@MainActor final class PaneTerm: NSObject, @preconcurrency TerminalViewDelegate {
    enum Phase: Equatable {
        case connecting
        case live
        /// The daemon's words, or nil for a plain end (the program exited, or it was let go of).
        case ended(String?)
    }

    let key: SessionKey
    let engine: Engine
    let target: AttachTarget
    let ref: UInt32
    let view: TerminalView
    private(set) var phase: Phase = .connecting
    /// The pane redraws its header when the phase moves.
    var onPhase: (() -> Void)?
    /// Called after each feed (the bench waits on it).
    var onFed: (() -> Void)?
    private weak var backend: TermBackend?
    private var drainTask: Task<Void, Never>?
    private var sizing: Coalescer!
    private var sent: (UInt16, UInt16)?
    private var closed = false
    /// The replay window (Review I4): fleetd answers an open of a kept-warm terminal with the end of its ring, and that
    /// ring holds the program's old queries (DA, DSR, OSC 11, XTVERSION, DECRQM). SwiftTerm answers every query it
    /// parses, and an answer to a replayed one would reach the live program as input. So the terminal's replies made
    /// while feeding the replay are dropped. The replay is what arrives within `replayWindow` of the open (or of a
    /// reconnect's reopen) and the burst that runs on from it (chunks under `burstGap` apart): fleetd queues the whole
    /// ring at once, before anything live. A fresh `claude attach` takes well over 50 ms to print, so its startup
    /// queries are answered. Keystrokes are never dropped: they do not come from feeding.
    static let replayWindow: Duration = .milliseconds(50)
    static let burstGap: Duration = .milliseconds(10)
    private var replayFrom: ContinuousClock.Instant?
    private var lastChunk: ContinuousClock.Instant?
    private var feeding = false
    /// Bytes of replies dropped (the tests).
    private(set) var repliesDropped = 0

    /// Review (b): under Metal a blinking cursor is a full redraw every 0.7 s (SwiftTerm's renderer timer), 180 ms of
    /// CPU a minute, measured, against an idle budget of 50. So after `blinkRest` with nothing typed or printed a
    /// blinking cursor holds steady (as many terminals stop blinking when idle), and the program's own style is back at
    /// the next key or output.
    static let blinkRest: Duration = .seconds(10)
    private var restingFrom: CursorStyle?
    private var rest: DispatchWorkItem?

    private func active() {
        if let style = restingFrom {
            restingFrom = nil
            view.getTerminal().setCursorStyle(style)
        }
        rest?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.resting() }
        rest = work
        DispatchQueue.main.asyncAfter(deadline: .now() + .seconds(Int(Self.blinkRest.components.seconds)), execute: work)
    }

    func resting() {
        guard !closed else { return }
        let t = view.getTerminal()
        let steady: CursorStyle
        switch t.options.cursorStyle {
        case .blinkBlock: steady = .steadyBlock
        case .blinkBar: steady = .steadyBar
        case .blinkUnderline: steady = .steadyUnderline
        case .steadyBlock, .steadyBar, .steadyUnderline: return
        }
        restingFrom = t.options.cursorStyle
        t.setCursorStyle(steady)
    }

    /// Whether this terminal has the keyboard of the key window (what a program that asks for focus reports, DECSET
    /// 1004, is told). SwiftTerm assumes focused until told; out of sight it is not.
    private(set) var focused = false

    init(key: SessionKey, engine: Engine, target: AttachTarget, ref: UInt32, frame: NSRect, backend: TermBackend) {
        self.key = key
        self.engine = engine
        self.target = target
        self.ref = ref
        self.backend = backend
        view = TerminalView(frame: frame, font: TermTheme.font, options: TerminalOptions(scrollback: 5000))
        super.init()
        view.nativeBackgroundColor = TermTheme.background
        view.nativeForegroundColor = TermTheme.foreground
        view.caretColor = TermTheme.cursor
        view.selectedTextBackgroundColor = TermTheme.selection
        view.optionAsMetaKey = true
        view.terminalDelegate = self
        // SwiftTerm's Metal renderer draws a full screen in ~1 ms where CoreText takes ~10 (measured in the bench):
        // that is what keeps a switch and ⌘⇧F inside a frame. It draws on demand only (paused, set-needs-display),
        // so an idle terminal costs nothing. CoreText stays as the fallback, and FLEET_TERM_RENDERER=cg forces it.
        if ProcessInfo.processInfo.environment["FLEET_TERM_RENDERER"] != "cg" { try? view.setUseMetal(true) }
        sizing = Coalescer { [weak self] in self?.sendSize() }
        view.getTerminal().setTerminalFocus(false)
    }

    func setFocused(_ f: Bool) {
        guard f != focused, !closed else { return }
        focused = f
        view.getTerminal().setTerminalFocus(f)
    }

    /// Opens it on the connection, at the view's size: the program draws its screen once, at the size you see.
    func open() {
        guard !closed, let backend else { return }
        phase = .connecting
        replayFrom = .now
        lastChunk = nil
        let t = view.getTerminal()
        sent = (UInt16(clamping: t.cols), UInt16(clamping: t.rows))
        backend.open(self, target: target, cols: sent!.0, rows: sent!.1)
        onPhase?()
    }

    /// Takes the terminal's output for as long as it runs (the Terminal is held by this task: letting go of it would
    /// close it), then its end.
    func drain(_ t: FleetKit.Terminal) {
        drainTask?.cancel()
        drainTask = Task { @MainActor [weak self] in
            var slice = ContinuousClock.now
            for await d in t.output {
                guard let self, !self.closed else { return }
                self.received(d)
                // A flood: half a frame of parsing, then the run loop gets a turn to draw and take keys.
                if ContinuousClock.now - slice > .milliseconds(8) {
                    try? await Task.sleep(for: .milliseconds(1))
                    slice = .now
                }
            }
            for await e in t.end {
                self?.ended(e)
                break
            }
        }
    }

    /// The connection is back and TermClient has opened this again: what comes first is a replay again.
    func reopened() {
        guard !closed else { return }
        if case .ended = phase { return }
        replayFrom = .now
        lastChunk = nil
        phase = .connecting
        onPhase?()
    }

    /// Whether output arriving now is still the replay.
    private func inReplay(_ now: ContinuousClock.Instant) -> Bool {
        guard let from = replayFrom else { return false }
        if let last = lastChunk, now - last < Self.burstGap { return true }
        if now - from < Self.replayWindow { return true }
        replayFrom = nil
        return false
    }

    func received(_ d: Data) {
        let now = ContinuousClock.now
        let replay = inReplay(now)
        lastChunk = now
        active()
        if phase != .live {
            phase = .live
            onPhase?()
        }
        feeding = replay
        d.withUnsafeBytes { raw in
            view.feed(byteArray: ArraySlice(raw.bindMemory(to: UInt8.self)))
        }
        feeding = false
        onFed?()
    }

    func ended(_ error: String?) {
        guard !closed else { return }
        if case .ended = phase { return }
        phase = .ended(error)
        view.feed(text: "\r\n\u{1b}[2m\(error ?? "detached") · return to attach again\u{1b}[0m\r\n")
        onPhase?()
    }

    /// Let go of: its ref is closed (a Claude attach stays warm in the daemon; a Codex one ends).
    func close() {
        guard !closed else { return }
        closed = true
        drainTask?.cancel()
        drainTask = nil
        rest?.cancel()
        backend?.close(ref)
        view.terminalDelegate = nil
        view.removeFromSuperview()
    }

    private func sendSize() {
        guard !closed else { return }
        if case .ended = phase { return }
        let t = view.getTerminal()
        let now = (UInt16(clamping: t.cols), UInt16(clamping: t.rows))
        if let s = sent, s == now { return }
        sent = now
        backend?.size(ref, cols: now.0, rows: now.1)
    }

    // MARK: - TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        if case .ended = phase {
            // Return in an ended terminal attaches it again, as in fleet.html.
            if data.count == 1, data.first == 13 { open() }
            return
        }
        // Sent while the replay is being fed: the terminal's answer to an old query, not a key.
        if feeding {
            repliesDropped += data.count
            return
        }
        active()
        backend?.input(ref, Data(data))
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) { sizing.mark() }
    func setTerminalTitle(source: TerminalView, title: String) {}
    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}

    /// A program's OSC 52 copy (Claude Code's own selection): written to the clipboard only while its window is key
    /// and this terminal has the keyboard, never from out of sight.
    func clipboardCopy(source: TerminalView, content: Data) {
        guard source.window?.isKeyWindow == true, source.window?.firstResponder === source,
            let text = String(data: content, encoding: .utf8)
        else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        guard let url = URL(string: link), ["http", "https"].contains(url.scheme?.lowercased() ?? "") else { return }
        NSWorkspace.shared.open(url)
    }
}

/// The header strip: drawn, not built from fields (one view, a few strings), so a switch redraws it in microseconds.
final class HeaderStrip: NSView {
    static let height: CGFloat = 30
    var session: Session? { didSet { needsDisplay = true } }
    /// Right-aligned: "connecting…", "reconnecting…", "ended", or nothing.
    var status: (String, NSColor)? { didSet { needsDisplay = true } }

    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        bounds.fill()
        Palette.rule.setFill()
        NSRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
        let mid = bounds.height / 2
        var right = bounds.width - 12
        if let (word, colour) = status {
            let a: [NSAttributedString.Key: Any] = [.font: Fonts.small, .foregroundColor: colour]
            let w = ceil((word as NSString).size(withAttributes: a).width)
            right -= w
            (word as NSString).draw(at: NSPoint(x: right, y: mid - 7), withAttributes: a)
            right -= 14
        }
        guard let s = session else { return }
        let dot = NSBezierPath(ovalIn: NSRect(x: 12, y: mid - 3.5, width: 7, height: 7).insetBy(dx: 0.75, dy: 0.75))
        if s.state == .waiting {
            Palette.quiet.setStroke()
            dot.lineWidth = 1.5
            dot.stroke()
        } else {
            Palette.of(s.state).setFill()
            dot.fill()
        }
        let p = NSMutableParagraphStyle()
        p.lineBreakMode = .byTruncatingTail
        let line = NSMutableAttributedString(string: Words.title(s), attributes: [
            .font: NSFont.systemFont(ofSize: 13, weight: .medium), .foregroundColor: NSColor.labelColor, .paragraphStyle: p,
        ])
        line.append(NSAttributedString(string: "   \(s.state.word)", attributes: [
            .font: Fonts.word, .foregroundColor: Palette.of(s.state), .paragraphStyle: p,
        ]))
        if !s.reason.isEmpty {
            line.append(NSAttributedString(string: " · \(s.reason)", attributes: [
                .font: Fonts.small, .foregroundColor: NSColor.secondaryLabelColor, .paragraphStyle: p,
            ]))
        }
        let r = NSRect(x: 28, y: mid - 8, width: max(0, right - 28), height: 17)
        NSGraphicsContext.saveGraphicsState()
        r.clip()
        line.draw(with: r, options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
        NSGraphicsContext.restoreGraphicsState()
    }

    override func viewDidChangeEffectiveAppearance() { needsDisplay = true }
}

@MainActor
final class TerminalPaneController: NSViewController {
    /// Claude terminals kept alive out of sight.
    static let warmLimit = 6

    private let model: BoardModel
    private let backend: TermBackend
    private let header = HeaderStrip()
    private let body = NSView()
    private let message = label(.systemFont(ofSize: 13), align: .center)
    private var terms: [SessionKey: PaneTerm] = [:]
    private var warm = WarmSet<SessionKey>(limit: TerminalPaneController.warmLimit)
    private var nextRef: UInt32 = 0
    /// The session in the pane, and its terminal when it has one on screen.
    private(set) var shownKey: SessionKey?
    var shownTerm: PaneTerm? { shownKey.flatMap { terms[$0] } }
    private var settle: DispatchWorkItem?
    /// Sessions whose terminal is being made (on the turn after the open), and whether to focus it when it is.
    private var pending: [SessionKey: Bool] = [:]
    private var catchUp: DispatchWorkItem?
    /// Called when a terminal's phase moves or output is fed (the bench).
    var onFed: (() -> Void)?
    /// Per machine (nil: the one connection of a single-connection backend), whether its Terminals connection is up
    /// (M2: "reconnecting…" follows it, not only the board's), whether it has been up, and whether it went down since.
    private var termUp: [String?: Bool] = [:]
    private var termEverUp: Set<String?> = []
    private var wasDown: Set<String?> = []

    init(model: BoardModel, backend: TermBackend) {
        self.model = model
        self.backend = backend
        super.init(nibName: nil, bundle: nil)
        backend.onConnection = { [weak self] machine, up in self?.connection(machine, up) }
    }

    /// Whether the shown terminal's connection is up (the checks).
    var termConnected: Bool { shownKey.map { termIsUp($0.machine) } ?? true }

    private func termIsUp(_ machine: String) -> Bool { termUp[machine] ?? termUp[nil] ?? true }
    private func termWasUp(_ machine: String) -> Bool { termEverUp.contains(machine) || termEverUp.contains(nil) }

    private func connection(_ machine: String?, _ up: Bool) {
        termUp[machine] = up
        if up { termEverUp.insert(machine) }
        if !up, termEverUp.contains(machine) { wasDown.insert(machine) }
        if up, wasDown.remove(machine) != nil {
            for t in terms.values where machine == nil || t.key.machine == machine { t.reopened() }
        }
        refreshStatus()
    }

    required init?(coder: NSCoder) { fatalError() }

    /// Frames, not constraints: nothing here costs the launch an Auto Layout pass. The message line is made after the
    /// first paint (`build`).
    override func loadView() {
        let v = NSView(frame: NSRect(x: 0, y: 0, width: 900, height: 700))
        header.frame = NSRect(x: 0, y: v.bounds.height - HeaderStrip.height, width: v.bounds.width, height: HeaderStrip.height)
        body.frame = NSRect(x: 0, y: 0, width: v.bounds.width, height: v.bounds.height - HeaderStrip.height)
        body.wantsLayer = true
        body.layer?.backgroundColor = TermTheme.background.cgColor
        body.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(
            self, selector: #selector(bodyResized), name: NSView.frameDidChangeNotification, object: body)
        v.addSubview(body)
        v.addSubview(header)
        view = v
    }

    /// The window's content runs under the toolbar (full-size content view): the header goes below it.
    override func viewDidLayout() {
        super.viewDidLayout()
        let b = view.bounds, top = view.safeAreaInsets.top
        let h = NSRect(x: 0, y: b.height - top - HeaderStrip.height, width: b.width, height: HeaderStrip.height)
        if header.frame != h { header.frame = h }
        let r = NSRect(x: 0, y: 0, width: b.width, height: max(0, h.minY))
        if body.frame != r { body.frame = r }
    }

    private var focusWatch: NSKeyValueObservation?

    /// Each terminal is told whether it has the keyboard: on every first-responder change and key-window change.
    private func watchFocus() {
        guard focusWatch == nil, let w = view.window else { return }
        focusWatch = w.observe(\.firstResponder) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.focusMoved() }
        }
        for n in [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification] {
            NotificationCenter.default.addObserver(self, selector: #selector(focusMoved), name: n, object: w)
        }
    }

    @objc private func focusMoved() {
        let w = view.window, fr = w?.firstResponder, key = w?.isKeyWindow ?? false
        for t in terms.values { t.setFocused(key && fr === t.view) }
    }

    func build() {
        watchFocus()
        guard message.superview == nil else { return }
        message.maximumNumberOfLines = 4
        message.usesSingleLineMode = false
        message.lineBreakMode = .byWordWrapping
        message.textColor = TermTheme.foreground.withAlphaComponent(0.6)
        message.frame = NSRect(x: 40, y: body.bounds.midY - 40, width: max(0, body.bounds.width - 80), height: 80)
        message.autoresizingMask = [.width, .minYMargin, .maxYMargin]
        if message.stringValue.isEmpty, shownKey == nil { message.stringValue = "Select a session" }
        body.addSubview(message)
    }

    /// What `s` offers in the pane.
    /// A session of any machine on the board opens through that machine's own fleetd (Ruling A-R1).
    func attach(_ s: Session) -> Attach {
        Attach.of(s, local: model.merged?.line(of: s.key.machine) != nil ? s.key.machine : model.machine)
    }

    /// Shows `s` (nil: none). A terminal already open for it is swapped in now. Otherwise, a Claude job opens now when
    /// `open` (an explicit open: Return, a click, ⌘K, ⌘1–9, history), or after the selection settles for 300 ms (↑/↓
    /// passing through rows opens nothing on the way); a Codex thread only on `activate`. `focus` gives its terminal
    /// the keyboard.
    func show(_ s: Session?, open: Bool, focus: Bool) {
        settle?.cancel()
        settle = nil
        let key = s?.key
        if key != shownKey { leave() }
        shownKey = key
        header.session = s
        guard let s else {
            showMessage("Select a session")
            refreshStatus()
            return
        }
        if let t = terms[s.key] {
            swapIn(t, focus: focus)
            return
        }
        if pending[s.key] != nil {
            showMessage(nil)
            if focus { pending[s.key] = true }
            refreshStatus()
            return
        }
        switch attach(s) {
        case let .open(target):
            showMessage(nil)
            if open {
                start(s, target, focus: focus)
            } else {
                header.status = ("connecting…", .secondaryLabelColor)
                let work = DispatchWorkItem { [weak self] in
                    guard let self, self.shownKey == s.key, self.terms[s.key] == nil else { return }
                    self.start(s, target, focus: false)
                }
                settle = work
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.3, execute: work)
                return
            }
        case let .ask(_, words):
            showMessage(words)
        case let .none(why):
            showMessage(why)
        }
        refreshStatus()
    }

    /// Return in the rail, or a Codex thread asked for: opens what is shown if it can, and focuses its terminal.
    func activate() {
        guard let k = shownKey, let s = header.session else { return }
        if let t = terms[k] {
            swapIn(t, focus: true)
            return
        }
        if pending[k] != nil {
            pending[k] = true
            return
        }
        switch attach(s) {
        case let .open(target), let .ask(target, _):
            showMessage(nil)
            start(s, target, focus: true)
        case .none:
            break
        }
    }

    /// The header follows the board (state, reason) and the connection.
    func update(_ s: Session?) {
        if let s, s.key == shownKey, header.session != s { header.session = s }
        refreshStatus()
    }

    /// The keyboard into the shown terminal; false when there is none.
    @discardableResult func focusTerminal() -> Bool {
        guard let t = shownTerm, !t.view.isHidden else { return false }
        view.window?.makeFirstResponder(t.view)
        return true
    }

    /// Whether there is a terminal to give the keyboard to (shown, or being made, or one Return opens).
    var canFocus: Bool {
        guard let k = shownKey else { return false }
        if terms[k] != nil || pending[k] != nil { return true }
        guard let s = header.session else { return false }
        if case .none = attach(s) { return false }
        return true
    }

    var terminalHasFocus: Bool {
        guard let fr = view.window?.firstResponder as? NSView else { return false }
        return terms.values.contains { fr === $0.view || fr.isDescendant(of: $0.view) }
    }

    var warmKeys: [SessionKey] { warm.keys }
    /// What the header's right side says now (the checks).
    var statusText: String? { header.status?.0 }
    func term(for key: SessionKey) -> PaneTerm? { terms[key] }

    // MARK: -

    /// A cold open: the pane says "connecting…" now; the terminal's view is made, put in and opened on the next
    /// turn (making a TerminalView and its renderer takes a few ms that the frame of the open does not wait for).
    private func start(_ s: Session, _ target: AttachTarget, focus: Bool) {
        pending[s.key] = focus
        showMessage(nil)
        for o in terms.values where !o.view.isHidden { o.view.isHidden = true }
        header.status = ("connecting…", .secondaryLabelColor)
        DispatchQueue.main.async { [weak self] in self?.make(s, target) }
    }

    private func make(_ s: Session, _ target: AttachTarget) {
        guard let focus = pending.removeValue(forKey: s.key), terms[s.key] == nil else { return }
        // A Codex terminal is made only to be seen: left before it was, it is not started.
        if s.engine == .codex, shownKey != s.key { return }
        nextRef &+= 1
        if nextRef == 0 { nextRef = 1 }
        let t = PaneTerm(key: s.key, engine: s.engine, target: target, ref: nextRef, frame: termFrame, backend: backend)
        t.onPhase = { [weak self, weak t] in
            guard let self, let t, t.key == self.shownKey else { return }
            self.refreshStatus()
        }
        t.onFed = { [weak self] in self?.onFed?() }
        terms[s.key] = t
        t.view.isHidden = true
        body.addSubview(t.view)
        if s.engine != .codex {
            for k in warm.use(s.key, keeping: shownKey) { terms.removeValue(forKey: k)?.close() }
        }
        if shownKey == s.key { swapIn(t, focus: focus) }
        t.open()
    }

    /// The view swap: the one terminal shown and following the pane's size, the others hidden (they go on taking
    /// output, and draw nothing).
    private func swapIn(_ t: PaneTerm, focus: Bool) {
        showMessage(nil)
        for o in terms.values where o !== t && !o.view.isHidden {
            o.view.isHidden = true
            o.view.autoresizingMask = []
        }
        if t.view.frame != termFrame { t.view.frame = termFrame }
        t.view.autoresizingMask = [.width, .height]
        if t.view.isHidden { t.view.isHidden = false }
        if t.engine != .codex {
            for k in warm.use(t.key) where k != t.key { terms.removeValue(forKey: k)?.close() }
        }
        if focus { view.window?.makeFirstResponder(t.view) }
        refreshStatus()
    }

    /// A terminal's frame in the body: a little room at the edges, as a terminal window has.
    private var termFrame: NSRect { body.bounds.insetBy(dx: 8, dy: 4) }

    /// The pane changed size: the shown terminal followed it; the others catch up once it has settled for 150 ms,
    /// one per turn of the run loop, so no frame reflows more than one.
    @objc private func bodyResized() {
        catchUp?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.catchUpOne() }
        catchUp = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15, execute: work)
    }

    private func catchUpOne() {
        guard let t = terms.values.first(where: { $0.view.isHidden && $0.view.frame != termFrame }) else { return }
        t.view.frame = termFrame
        let work = DispatchWorkItem { [weak self] in self?.catchUpOne() }
        catchUp = work
        DispatchQueue.main.async(execute: work)
    }

    /// Leaving the shown session: its terminal goes out of sight; a Codex one closes (plan 2: the agent is the
    /// terminal, and it is not kept running unseen).
    private func leave() {
        guard let k = shownKey, let t = terms[k] else { return }
        if terminalHasFocus, let w = view.window { w.makeFirstResponder(nil) }
        if t.engine == .codex {
            terms.removeValue(forKey: k)
            t.close()
        } else {
            t.view.isHidden = true
        }
    }

    private func showMessage(_ text: String?) {
        if let text {
            for o in terms.values where !o.view.isHidden { o.view.isHidden = true }
            message.stringValue = text
            message.isHidden = false
        } else if !message.isHidden {
            message.isHidden = true
        }
    }

    private func refreshStatus() {
        let next: (String, NSColor)?
        // The connection of the shown session's machine: this Mac's fleetd, or another machine's over the tailnet.
        let machine = shownKey?.machine
        let remote = machine != nil && model.merged != nil && machine != model.machine
        let status = machine.map { model.status(of: $0) } ?? model.status
        switch status {
        case _ where shownTerm != nil && machine.map({ !termIsUp($0) && termWasUp($0) }) == true:
            next = ("reconnecting…", Palette.orange)
        // The board's status starts as reconnecting: before its first Snapshot it is "connecting…" (below).
        case .reconnecting where shownTerm != nil && model.cachedAt == nil && model.board.run != nil:
            next = ("reconnecting…", Palette.orange)
        case .absent where shownTerm != nil:
            next = (remote ? "\(machine ?? "") unreachable" : "fleetd not running", Palette.red)
        case let .refused(why) where shownTerm != nil: next = ("fleetd refused: \(why)", Palette.red)
        default:
            switch shownTerm?.phase {
            case .connecting: next = ("connecting…", .secondaryLabelColor)
            case let .ended(why): next = (why == nil ? "detached" : "ended", .secondaryLabelColor)
            case .live, nil: next = nil
            }
        }
        if header.status?.0 != next?.0 { header.status = next }
        if let s = header.session {
            header.toolTip = [
                ("machine", s.key.machine), ("account", s.account), ("profile", s.key.profile), ("folder", s.cwd),
                ("model", s.model.map { [$0, s.effort].compactMap { $0 }.joined(separator: " · ") }),
                ("context", s.contextTokens.map { _ in Words.tokens(s.contextTokens) }), ("job", s.jobId),
                ("why", s.reason),
            ].compactMap { k, v in v.flatMap { $0.isEmpty ? nil : "\(k)  \($0)" } }.joined(separator: "\n")
        }
    }
}

