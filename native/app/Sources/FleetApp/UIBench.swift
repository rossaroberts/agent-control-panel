// Bench mode for the interactions (Ruling ALL-R1: every user-visible action ≤ 16 ms, one frame, from the trigger to
// the result on screen, median of 3). Each action is triggered as you would (a synthesized key event through
// NSApp.sendEvent: the menus, the responder chain, the views), then the window is drawn and flushed to the window
// server, and the time between is the action's.
//
//   FleetApp --ui-bench out.json
//       a synthetic board (no fleetd, no cache, no socket) and an in-process backend that answers an open with a
//       synthetic screen and echoes keys as a pty in echo mode would: so "echo" is what the app adds over the pty.
//       Times, 5 times each: a cold open's pane and "connecting…" on screen; ⌘[ ⌘] between two warm terminals; ↑/↓
//       in the rail between two warm terminals; ⌘1; Return into the terminal; ⌃` back to the rail; a key's echo;
//       ⌘K opening, each keystroke filtering, Esc closing it; ⌘⇧F there and back; a delta on screen. Also checks
//       that Return in a terminal reaches its program (the Go menu's Return must not take it).
//   FleetApp --ui-bench out.json --idle
//       the same synthetic board with one terminal shown and focused, then idle until killed (the idle-CPU check)
//   FleetApp --live-switch <session-or-job> <session-or-job> out.json
//       against the running fleetd: opens the two (the keyboard stays in the rail; nothing is typed), lets each draw,
//       then times ⌘[ and ⌘] between them 3 times each, and writes the times and the bytes typed (0).

import AppKit
import FleetKit

/// The bench's backend: no daemon. An open is answered with a synthetic screen; keys come back as they went.
@MainActor final class EchoBackend: TermBackend {
    private var terms: [UInt32: PaneTerm] = [:]
    private(set) var typed: [UInt32: [UInt8]] = [:]
    var onConnection: ((_ machine: String?, _ up: Bool) -> Void)?

    func open(_ term: PaneTerm, target: AttachTarget, cols: UInt16, rows: UInt16) {
        terms[term.ref] = term
        Task { @MainActor [weak term] in term?.received(Self.screen(cols: Int(cols), rows: Int(rows))) }
    }

    func input(_ ref: UInt32, _ data: Data) {
        typed[ref, default: []] += Array(data)
        Task { @MainActor [weak self] in self?.terms[ref]?.received(data) }
    }

    func size(_ ref: UInt32, cols: UInt16, rows: UInt16) {}
    func close(_ ref: UInt32) { terms[ref] = nil }

    /// A full synthetic screen: a frame, coloured lines of text, a prompt.
    static func screen(cols: Int, rows: Int) -> Data {
        var s = "\u{1b}[2J\u{1b}[H\u{1b}[38;5;39m╭" + String(repeating: "─", count: max(0, cols - 2)) + "╮\u{1b}[0m\r\n"
        let words = ["fixture", "terminal", "line", "with", "some", "text", "and", "colour", "to", "draw"]
        for r in 0..<max(0, rows - 4) {
            var line = ""
            var i = r
            while line.count < cols - 8 {
                line += words[i % words.count] + " "
                i += 3
            }
            s += "\u{1b}[3\(1 + r % 6)m\(String(line.prefix(cols - 4)))\u{1b}[0m\r\n"
        }
        s += "\u{1b}[38;5;39m╰" + String(repeating: "─", count: max(0, cols - 2)) + "╯\u{1b}[0m\r\n> "
        return Data(s.utf8)
    }
}

/// Times actions: trigger, wait for what it shows, draw and flush.
@MainActor struct Stopwatch {
    let window: NSWindow

    /// A key as you would press it, through the app's own event dispatch.
    func key(_ chars: String, _ code: UInt16, _ mods: NSEvent.ModifierFlags = []) {
        for type in [NSEvent.EventType.keyDown, .keyUp] {
            guard let e = NSEvent.keyEvent(
                with: type, location: .zero, modifierFlags: mods, timestamp: ProcessInfo.processInfo.systemUptime,
                windowNumber: window.windowNumber, context: nil, characters: chars,
                charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code)
            else { continue }
            NSApp.sendEvent(e)
        }
    }

    /// Milliseconds from `act` to its result on screen: `until` holds (polled, so an asynchronous result counts its
    /// hops), then the window is drawn and handed to the window server. nil if `until` never held within a second.
    func time(_ act: () -> Void, until: () -> Bool = { true }) async -> Double? {
        let t0 = ContinuousClock.now
        act()
        let end = t0 + .seconds(1)
        while !until() {
            if ContinuousClock.now > end { return nil }
            try? await Task.sleep(for: .microseconds(100))
        }
        window.displayIfNeeded()
        CATransaction.flush()
        let d = ContinuousClock.now - t0
        return Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15
    }

    static func median(_ v: [Double]) -> Double? {
        guard !v.isEmpty else { return nil }
        let s = v.sorted()
        return s.count % 2 == 1 ? s[s.count / 2] : (s[s.count / 2 - 1] + s[s.count / 2]) / 2
    }
}

/// `--ui-bench`.
@MainActor final class UIBench {
    let out: URL
    static let machine = "bench-mac"

    init(out: URL) {
        self.out = out
    }

    static func fromArguments() -> UIBench? {
        let a = CommandLine.arguments
        guard let i = a.firstIndex(of: "--ui-bench"), i + 1 < a.count else { return nil }
        return UIBench(out: URL(fileURLWithPath: a[i + 1]))
    }

    /// A synthetic board: needs-you, waiting and working background jobs, Codex threads, done rows.
    static func sessions(now: Date = Date()) -> [Session] {
        let ms = Int64(now.timeIntervalSince1970 * 1000)
        func s(_ n: Int, _ state: SessionState, engine: Engine = .claude, live: Bool = true, age: Int64) -> Session {
            Session(
                key: SessionKey(machine: machine, profile: "bench", sessionId: String(format: "bench-%04d", n)),
                engine: engine, name: "bench-\(n)", cwd: "/tmp/bench", state: state, reason: "synthetic \(state.word)",
                live: live, pid: nil, stage: nil, model: "fixture-model", effort: nil, contextTokens: 42_000,
                lastAt: ms - age, said: nil, doing: nil, lastAsk: nil, firstAsk: nil, startedAt: ms - age - 60_000,
                hookAt: nil, writtenAt: nil, jobId: engine == .claude ? String(format: "%08x", 0xb000 + n) : nil,
                title: "fixture session \(n) \(["alpha", "bravo", "charlie", "delta"][n % 4])", epic: nil,
                archived: false, account: "acct-\(n % 3)")
        }
        var out: [Session] = []
        for n in 0..<4 { out.append(s(n, .needsYou, age: Int64(n) * 60_000 + 30_000)) }
        // The two most recent waiting ones sit next to each other in the rail.
        for n in 4..<20 { out.append(s(n, .waiting, age: Int64(n) * 120_000)) }
        for n in 20..<26 { out.append(s(n, .working, age: Int64(n) * 10_000)) }
        for n in 26..<30 { out.append(s(n, .done, live: false, age: Int64(n) * 3_600_000)) }
        return out
    }

    private var results: [String: [Double]] = [:]
    private var misses: [String] = []

    private func record(_ name: String, _ v: Double?) {
        if let v { results[name, default: []].append(v) } else { misses.append(name) }
    }

    func run(window: NSWindow, model: BoardModel, nav: Navigator, pane: TerminalPaneController, backend: EchoBackend) async {
        let sw = Stopwatch(window: window)
        let pause = { try? await Task.sleep(for: .milliseconds(60)) }
        // The board on screen.
        while nav.rail.rows.isEmpty { try? await Task.sleep(for: .milliseconds(5)) }
        // `--idle`: one terminal shown with the keyboard in it (its caret on), then nothing, for the idle-CPU check
        // (measured from outside; the caller ends it).
        if CommandLine.arguments.contains("--idle") {
            nav.open(Self.sessions()[4].key, focus: true)
            try? await Task.sleep(for: .seconds(1))
            // FLEET_IDLE_BLINK=1: the cursor blinks as in a key window with the keyboard in it (which a bench launched
            // beside someone at work cannot always get), the worst case for idle.
            if ProcessInfo.processInfo.environment["FLEET_IDLE_BLINK"] == "1", let v = pane.shownTerm?.view {
                v.caretViewTracksFocus = false
                v.getTerminal().setCursorStyle(.blinkBlock)
            }
            try? await Task.sleep(for: .seconds(1))
            let state: [String: Any] = ["key": window.isKeyWindow, "terminalFocused": pane.terminalHasFocus]
            if let data = try? JSONSerialization.data(withJSONObject: state) { try? data.write(to: out) }
            return
        }
        await pause()
        let all = Self.sessions()
        // ⌘1 is the first of Needs you as the board orders it (the oldest wait first).
        guard let n1 = model.groups.first(where: { $0.kind == .needsYou })?.rows.first?.key else { return }
        let a = all[4].key, b = all[5].key
        var fed = 0
        pane.onFed = { fed += 1 }

        // Cold opens: the pane and "connecting…" in the frame of the open (the synthetic screen comes a turn later).
        for i in 0..<5 {
            let k = all[20 + i].key
            record("coldOpenShown", await sw.time({ nav.open(k, focus: false) }))
            await pause()
        }
        // Warm: N1, then A and B (in that order, so history is …, A, B).
        for k in [n1, a, b] {
            nav.open(k, focus: false)
            let f = fed
            while fed == f { try? await Task.sleep(for: .milliseconds(2)) }
            await pause()
        }
        for _ in 0..<5 {
            record("back", await sw.time({ sw.key("[", 33, .command) }, until: { pane.shownKey == a }))
            await pause()
            record("forward", await sw.time({ sw.key("]", 30, .command) }, until: { pane.shownKey == b }))
            await pause()
        }
        // ↑/↓ in the rail between A and B (adjacent), both warm.
        window.makeFirstResponder(nav.rail.table)
        let up = String(UnicodeScalar(NSUpArrowFunctionKey)!), down = String(UnicodeScalar(NSDownArrowFunctionKey)!)
        for _ in 0..<5 {
            record("railUp", await sw.time({ sw.key(up, 126, [.numericPad, .function]) }, until: { pane.shownKey == a }))
            await pause()
            record("railDown", await sw.time({ sw.key(down, 125, [.numericPad, .function]) }, until: { pane.shownKey == b }))
            await pause()
        }
        // ⌘1 (warm), then back to B.
        for _ in 0..<5 {
            record("needsYou1", await sw.time({ sw.key("1", 18, .command) }, until: { pane.shownKey == n1 }))
            await pause()
            nav.open(b, focus: false)
            await pause()
        }
        // Return into the terminal; ⌃` back to the rail.
        for _ in 0..<5 {
            window.makeFirstResponder(nav.rail.table)
            await pause()
            record("returnFocus", await sw.time({ sw.key("\r", 36) }, until: { pane.terminalHasFocus }))
            await pause()
            record("toRail", await sw.time({ sw.key("`", 50, .control) }, until: { window.firstResponder === nav.rail.table }))
            await pause()
        }
        // A key's echo in the terminal, and Return reaching the program.
        nav.enter()
        await pause()
        let ref = pane.shownTerm?.ref ?? 0
        for _ in 0..<5 {
            let f = fed
            record("echo", await sw.time({ sw.key("x", 7) }, until: { fed > f }))
            await pause()
        }
        let before = backend.typed[ref]?.count ?? 0
        sw.key("\r", 36)
        await pause()
        let returnReaches = (backend.typed[ref] ?? []).dropFirst(before).contains(13)
        // ⌘K: open, three keystrokes, Esc.
        for _ in 0..<5 {
            record("switcherOpen", await sw.time({ sw.key("k", 40, .command) }, until: { nav.switcher.isOpen }))
            await pause()
            for (c, code) in [("f", UInt16(3)), ("i", 34), ("x", 7)] {
                let q = nav.switcher.field.stringValue
                record("switcherKey", await sw.time({ sw.key(c, code) }, until: { nav.switcher.field.stringValue != q }))
                await pause()
            }
            record("switcherClose", await sw.time({ sw.key("\u{1b}", 53) }, until: { !nav.switcher.isOpen }))
            await pause()
        }
        // ⌘⇧F there and back.
        for _ in 0..<5 {
            record("fullWindow", await sw.time({ sw.key("f", 3, [.command, .shift]) }, until: { nav.isFullWindow }))
            await pause()
            record("fullWindowBack", await sw.time({ sw.key("f", 3, [.command, .shift]) }, until: { !nav.isFullWindow }))
            await pause()
        }
        // A delta on screen: a row changes state and title.
        for i in 0..<5 {
            var s = all[10]
            s.title = "fixture changed \(i)"
            s.state = i % 2 == 0 ? .working : .waiting
            let r = model.renders
            let seq = (model.board.seq ?? 0) + 1
            record("delta", await sw.time({
                _ = model.board.apply(.delta(Delta(seq: seq, upsert: [s], remove: [])))
            }, until: { model.renders > r }))
            await pause()
        }

        var json: [String: Any] = [
            "returnReachesTerminal": returnReaches, "misses": misses,
            "renderer": pane.shownTerm.map { $0.view.isUsingMetalRenderer ? "metal" : "coregraphics" } ?? "none",
        ]
        for (k, v) in results {
            json[k] = Stopwatch.median(v).map { ($0 * 100).rounded() / 100 }
            json[k + "Runs"] = v.map { ($0 * 100).rounded() / 100 }
        }
        if let data = try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys, .prettyPrinted]) {
            try? data.write(to: out)
        }
        NSApp.terminate(nil)
    }
}

/// `--live-switch A B out.json`: ⌘[ ⌘] between two real sessions' warm terminals, typing nothing.
@MainActor enum LiveSwitch {
    static func arguments() -> (String, String, URL)? {
        let a = CommandLine.arguments
        guard let i = a.firstIndex(of: "--live-switch"), i + 3 < a.count else { return nil }
        return (a[i + 1], a[i + 2], URL(fileURLWithPath: a[i + 3]))
    }

    static func find(_ id: String, in model: BoardModel) -> SessionKey? {
        model.board.sessions.values.first { $0.key.sessionId == id || $0.jobId == id }?.key
    }

    static func run(_ ids: (String, String), out: URL, window: NSWindow, model: BoardModel, nav: Navigator,
                    pane: TerminalPaneController, backend: LiveBackend) async
    {
        var json: [String: Any] = [:]
        defer {
            json["bytesTyped"] = backend.bytesTyped
            if let data = try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys, .prettyPrinted]) {
                try? data.write(to: out)
            }
            NSApp.terminate(nil)
        }
        let deadline = ContinuousClock.now + .seconds(10)
        while model.board.run == nil || model.cachedAt != nil, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(20))
        }
        guard let a = find(ids.0, in: model), let b = find(ids.1, in: model) else {
            json["error"] = "session not on the board"
            return
        }
        let sw = Stopwatch(window: window)
        for k in [a, b] {
            nav.open(k, focus: false)
            let end = ContinuousClock.now + .seconds(10)
            while pane.term(for: k)?.phase != .live, ContinuousClock.now < end {
                try? await Task.sleep(for: .milliseconds(20))
            }
            json["phase \(k.sessionId)"] = "\(String(describing: pane.term(for: k)?.phase))"
            try? await Task.sleep(for: .seconds(1.5))  // let it draw its screen
        }
        window.makeFirstResponder(nav.rail.table)  // the keyboard stays out of the terminals
        var back: [Double] = [], forward: [Double] = []
        for _ in 0..<3 {
            if let v = await sw.time({ sw.key("[", 33, .command) }, until: { pane.shownKey == a }) { back.append(v) }
            try? await Task.sleep(for: .milliseconds(400))
            if let v = await sw.time({ sw.key("]", 30, .command) }, until: { pane.shownKey == b }) { forward.append(v) }
            try? await Task.sleep(for: .milliseconds(400))
        }
        json["backMs"] = back
        json["forwardMs"] = forward
        json["medianMs"] = Stopwatch.median(back + forward)
        json["renderer"] = pane.shownTerm.map { $0.view.isUsingMetalRenderer ? "metal" : "coregraphics" } ?? "none"
        // The pair as it ended, on screen (B, after the last ⌘]): the swapped-in view drew.
        if let (png, viewDrawn) = windowPNG(window, rail: nav.rail) {
            try? png.write(to: out.deletingPathExtension().appendingPathExtension("png"))
            json["shot"] = viewDrawn ? "view-drawn" : "window server"
        }
    }
}

/// `--term-check <job> <seconds> out.json`, against a throwaway fleetd (FLEETD_SOCK): shows a live background job of the
/// daemon's own machine in the pane and records, every 10 ms, what the header says and the terminal's phase, as they
/// change, then the screen's last lines. The fleetd restart check (Review (a)) kills and restarts that daemon while it
/// runs.
@MainActor enum TermCheck {
    static func arguments() -> (String, Double, URL)? {
        let a = CommandLine.arguments
        guard let i = a.firstIndex(of: "--term-check"), i + 3 < a.count, let secs = Double(a[i + 2]) else { return nil }
        return (a[i + 1], secs, URL(fileURLWithPath: a[i + 3]))
    }

    static func run(job: String, seconds: Double, out: URL, model: BoardModel, pane: TerminalPaneController) async {
        let start = ContinuousClock.now
        while model.machine == nil, ContinuousClock.now - start < .seconds(10) { try? await Task.sleep(for: .milliseconds(10)) }
        let s = Session(
            key: SessionKey(machine: model.machine ?? "?", profile: "default", sessionId: "term-check"), engine: .claude,
            name: nil, cwd: nil, state: .waiting, reason: "term check", live: true, pid: nil, stage: nil, model: nil,
            effort: nil, contextTokens: nil, lastAt: nil, said: nil, doing: nil, lastAsk: nil, firstAsk: nil,
            startedAt: nil, hookAt: nil, writtenAt: nil, jobId: job, title: "term check", epic: nil, archived: false,
            account: nil)
        pane.show(s, open: true, focus: false)
        var events: [[String: Any]] = []
        var last = ""
        let end = ContinuousClock.now + .milliseconds(Int(seconds * 1000))
        while ContinuousClock.now < end {
            let phase = pane.shownTerm.map { "\($0.phase)" } ?? "none"
            let now = "\(pane.statusText ?? "-") | \(phase)"
            if now != last {
                let ms = Double((ContinuousClock.now - start).components.attoseconds) / 1e15
                    + Double((ContinuousClock.now - start).components.seconds) * 1000
                events.append(["ms": (ms * 10).rounded() / 10, "status": pane.statusText ?? "-", "phase": phase])
                last = now
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        var lines: [String] = []
        if let t = pane.shownTerm?.view.getTerminal() {
            for r in 0..<t.rows {
                let l = t.getLine(row: r)?.translateToString(trimRight: true) ?? ""
                if !l.isEmpty { lines.append(l) }
            }
        }
        let json: [String: Any] = ["events": events, "screen": lines, "repliesDropped": pane.shownTerm?.repliesDropped ?? -1]
        if let data = try? JSONSerialization.data(withJSONObject: json, options: [.sortedKeys, .prettyPrinted]) {
            try? data.write(to: out)
        }
        NSApp.terminate(nil)
    }
}
