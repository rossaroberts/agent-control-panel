// The app's pane and navigator, in-process: a real window (never shown) with the rail, the pane and the switcher, a
// synthetic board, and a backend that records what would reach a program (Review I3, I4, M5).

import AppKit
import FleetKit
import SwiftTerm
import Testing

@testable import FleetApp

@MainActor final class RecordingBackend: TermBackend {
    var opened: [UInt32] = []
    var sent: [UInt32: [UInt8]] = [:]
    var onConnection: ((Bool) -> Void)?
    func open(_ term: PaneTerm, target: AttachTarget, cols: UInt16, rows: UInt16) { opened.append(term.ref) }
    func input(_ ref: UInt32, _ data: Data) { sent[ref, default: []] += Array(data) }
    func size(_ ref: UInt32, cols: UInt16, rows: UInt16) {}
    func close(_ ref: UInt32) {}
}

private let machine = "rig-mac"

private func session(_ n: Int, job: String?, state: SessionState = .waiting, live: Bool = true, ageMs: Int64 = 60_000)
    -> Session
{
    let now = Int64(Date().timeIntervalSince1970 * 1000)
    return Session(
        key: SessionKey(machine: machine, profile: "rig", sessionId: "rig-\(n)"), engine: .claude, name: "rig \(n)",
        cwd: "/tmp", state: state, reason: "fixture", live: live, pid: nil, stage: nil, model: nil, effort: nil,
        contextTokens: nil, lastAt: now - ageMs, said: nil, doing: nil, lastAsk: nil, firstAsk: nil, startedAt: nil,
        hookAt: nil, writtenAt: nil, jobId: job, title: "rig session \(n)", epic: nil, archived: false, account: nil)
}

@MainActor final class Rig {
    let board = Board()
    let model: BoardModel
    let backend = RecordingBackend()
    let rail: BoardController
    let pane: TerminalPaneController
    let nav: Navigator
    let window: NSWindow
    let split = NSSplitViewController()

    init(_ sessions: [Session]) async {
        model = BoardModel(board: board)
        model.status = .live
        rail = BoardController(model: model)
        pane = TerminalPaneController(model: model, backend: backend)
        split.addSplitViewItem(NSSplitViewItem(sidebarWithViewController: rail))
        split.addSplitViewItem(NSSplitViewItem(viewController: pane))
        nav = Navigator(model: model, rail: rail, pane: pane)
        nav.split = split
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1200, height: 700), styleMask: [.titled, .resizable],
            backing: .buffered, defer: false)
        window.contentViewController = split
        pane.build()
        nav.prepare()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: machine, sessions: sessions))
        await settle()
    }

    func settle() async {
        try? await Task.sleep(for: .milliseconds(60))
    }

    var responder: NSResponder? { window.firstResponder }
}

@MainActor @Suite(.serialized) struct NavigatorFocusTests {
    /// I3: chosen from ⌘K, a session with no terminal leaves the keyboard in the rail, not in the hidden switcher.
    @Test func aSessionWithNoTerminalChosenFromTheSwitcherPutsTheKeyboardInTheRail() async {
        let rig = await Rig([session(1, job: nil), session(2, job: "0000000b")])
        rig.nav.quickSwitcher(nil)
        #expect(rig.nav.switcherOpen)
        let interactive = rig.board.sessions.values.first { $0.jobId == nil }!
        rig.nav.switcher.close()
        rig.nav.switcher.onChoose?(interactive)
        await rig.settle()
        #expect(rig.responder === rig.rail.table)
        #expect(rig.pane.shownTerm == nil)
    }

    /// I3: from a terminal, opening (with focus) a session that has none gives the rail the keyboard, and ⌃` there
    /// (nothing to focus) stays in or comes back to the rail.
    @Test func fromATerminalToASessionWithNoneTheRailHasTheKeyboard() async {
        let rig = await Rig([session(1, job: nil), session(2, job: "0000000b")])
        let job = rig.board.sessions.values.first { $0.jobId != nil }!.key
        let none = rig.board.sessions.values.first { $0.jobId == nil }!.key
        rig.nav.open(job, focus: true)
        await rig.settle()
        #expect(rig.pane.terminalHasFocus)
        rig.nav.open(none, focus: true)
        await rig.settle()
        #expect(rig.responder === rig.rail.table)
        rig.window.makeFirstResponder(nil)
        rig.nav.toggleFocus(nil)
        #expect(rig.responder === rig.rail.table)
        // Back to the one with a terminal: ⌃` gives it the keyboard, and again takes it back to the rail.
        rig.nav.open(job, focus: false)
        rig.nav.toggleFocus(nil)
        #expect(rig.pane.terminalHasFocus)
        rig.nav.toggleFocus(nil)
        #expect(rig.responder === rig.rail.table)
    }

    /// M5: a folded "older" row chosen from ⌘K unfolds its group, so the rail shows it selected.
    @Test func aFoldedRowChosenFromTheSwitcherIsShownInTheRail() async {
        UserDefaults.standard.removeObject(forKey: "openOlder")
        defer { UserDefaults.standard.removeObject(forKey: "openOlder") }
        let old = session(9, job: nil, state: .done, live: false, ageMs: 72 * 3600 * 1000)
        let rig = await Rig([session(1, job: nil), old])
        #expect(!rig.rail.rows.contains { $0.session?.key == old.key })
        rig.nav.open(old.key, focus: true)
        await rig.settle()
        let at = rig.rail.rows.firstIndex { $0.session?.key == old.key }
        #expect(at != nil && rig.rail.table.selectedRow == at)
    }
}

@MainActor @Suite struct ReplayRepliesTests {
    private func term(_ b: RecordingBackend) -> PaneTerm {
        PaneTerm(
            key: SessionKey(machine: machine, profile: "rig", sessionId: "t"), engine: .claude,
            target: .claude(profile: "rig", job: "0000000a"), ref: 7, frame: NSRect(x: 0, y: 0, width: 800, height: 400),
            backend: b)
    }

    /// I4: a replayed ring holding a DA1 query: SwiftTerm's answer does not reach the program.
    @Test func aQueryInTheReplayIsNotAnswered() async {
        let b = RecordingBackend()
        let t = term(b)
        t.open()
        t.received(Data("old screen \u{1b}[c more old screen \u{1b}[6n".utf8))
        #expect(b.sent[7] == nil)
        #expect(t.repliesDropped > 0)
    }

    /// I4: a live query, after the replay, is answered.
    @Test func aLiveQueryAfterTheReplayIsAnswered() async throws {
        let b = RecordingBackend()
        let t = term(b)
        t.open()
        t.received(Data("replay \u{1b}[c".utf8))
        try await Task.sleep(for: PaneTerm.replayWindow + .milliseconds(30))
        t.received(Data("\u{1b}[c".utf8))
        let sent = b.sent[7] ?? []
        #expect(sent.starts(with: Array("\u{1b}[?".utf8)), "\(sent)")
    }

    /// I4: keys typed while the replay is coming in are never dropped.
    @Test func keysDuringTheReplayReachTheProgram() async {
        let b = RecordingBackend()
        let t = term(b)
        t.open()
        t.received(Data("replay part 1".utf8))
        t.send(source: t.view, data: ArraySlice(Array("abc".utf8)))
        t.received(Data("replay part 2 \u{1b}[c".utf8))
        #expect(b.sent[7] == Array("abc".utf8))
    }

    /// I4: a reconnect's reopen is a replay again.
    @Test func aReopenAfterAReconnectIsAReplayAgain() async throws {
        let b = RecordingBackend()
        let t = term(b)
        t.open()
        try await Task.sleep(for: PaneTerm.replayWindow + .milliseconds(30))
        t.received(Data("live".utf8))
        t.reopened()
        t.received(Data("ring again \u{1b}[c".utf8))
        #expect(b.sent[7] == nil)
    }

    /// Review (b): an idle blinking cursor holds steady (no Metal redraw every 0.7 s); the next output brings the
    /// program's style back.
    @Test func anIdleBlinkingCursorRestsAndComesBack() async {
        let b = RecordingBackend()
        let t = term(b)
        t.open()
        t.view.getTerminal().setCursorStyle(.blinkBar)
        t.resting()
        #expect(t.view.getTerminal().options.cursorStyle == .steadyBar)
        t.received(Data("x".utf8))
        #expect(t.view.getTerminal().options.cursorStyle == .blinkBar)
    }
}
