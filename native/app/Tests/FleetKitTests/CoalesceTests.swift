// The board window's model, without a window: renders coalesced to one per display frame under a burst of deltas
// (Review Focus 3), the selection kept by SessionKey while a row's title and epic change (Review Focus 4), and the
// board cache (round trip, a truncated file, at most one write a second).

import Foundation
import Testing

@testable import FleetKit

private func session(_ sid: String, _ state: SessionState = .working, title: String? = nil, epic: String? = nil)
    -> Session
{
    Session(
        key: SessionKey(machine: "fixture-mac", profile: "default", sessionId: sid), engine: .claude, name: sid,
        cwd: nil, state: state, reason: "fixture", live: true, pid: nil, stage: nil, model: nil, effort: nil,
        contextTokens: nil, lastAt: 1_793_966_000_000, said: nil, doing: nil, lastAsk: nil, firstAsk: nil,
        startedAt: nil, hookAt: nil, writtenAt: nil, jobId: nil, title: title ?? sid, epic: epic, archived: false,
        account: nil)
}

/// Every field set, so a round trip that drops one shows.
private let full = Session(
    key: SessionKey(machine: "fixture-mac", profile: "work", sessionId: "s-full"), engine: .codex, name: "a name",
    cwd: "/tmp/fixture", state: .needsYou, reason: "asked a question", live: true, pid: 4242, stage: "plan",
    model: "fixture-model", effort: "high", contextTokens: 142_000, lastAt: 1_793_966_100_000, said: "said",
    doing: "doing", lastAsk: "last ask", firstAsk: "first ask", startedAt: 1_793_960_000_000,
    hookAt: 1_793_966_090_000, writtenAt: 1_793_966_095_000, jobId: "0a1b2c3d", title: "A title", epic: "Epic A",
    archived: false, account: "acct-a")

private let meta = Meta(
    epics: ["Epic A", "Epic B"], groups: ["Mine"],
    accounts: [Account(name: "acct-a", engine: .claude, profile: "default", limits: [
        Limit(window: "5h", usedPct: 42, resetsAt: 1_793_970_000_000), Limit(window: "7d", usedPct: 71, resetsAt: nil),
    ])],
    health: Health(cpuPct: 12, memPct: 61, diskPct: 40, dev: 2, db: 1))

/// Waits (on the main actor, so renders run) until `done` holds, or `limit` passes.
@MainActor private func settle(_ limit: Duration = .seconds(1), until done: () -> Bool) async {
    let end = ContinuousClock.now + limit
    while !done() && ContinuousClock.now < end { try? await Task.sleep(for: .milliseconds(5)) }
}

@MainActor @Suite(.serialized) struct CoalesceTests {
    /// Review Focus 3: 500 deltas in 1 s render at most once per display frame (≤ 61 in that second), and the board
    /// on screen ends as the last delta left it.
    @Test func aBurstOf500DeltasInOneSecondRendersAtMostOncePerFrame() async {
        let board = Board()
        let model = BoardModel(board: board)
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [session("a"), session("b")]))
        await settle { model.renders > 0 }
        #expect(model.groups.flatMap(\.rows).count == 2)
        try? await Task.sleep(for: .milliseconds(50))
        let before = model.renders

        let start = ContinuousClock.now
        for i in 1...500 {
            // Paced by deadline, 2 ms apart: 500 in one second however long each sleep oversleeps.
            let due = start + .milliseconds(2 * (i - 1))
            if ContinuousClock.now < due { try? await Task.sleep(until: due, clock: .continuous) }
            var a = session("a")
            a.reason = "r\(i)"
            #expect(board.apply(.delta(Delta(seq: UInt64(i), upsert: [a], remove: []))) == .applied)
        }
        let burst = ContinuousClock.now - start
        await settle { model.groups.flatMap(\.rows).first { $0.key.sessionId == "a" }?.reason == "r500" }
        try? await Task.sleep(for: .milliseconds(50))
        let renders = model.renders - before
        print("500 deltas in \(burst): \(renders) renders")

        #expect(burst < .milliseconds(1100))
        #expect(renders <= 61)
        // It kept up all through the burst, not once at the end.
        #expect(renders >= 20)
        #expect(model.groups.flatMap(\.rows).first { $0.key.sessionId == "a" }?.reason == "r500")
        #expect(model.groups.flatMap(\.rows).count == 2)
    }

    /// A delta after a quiet spell is drawn on the next turn of the run loop, not a frame later.
    @Test func aLoneDeltaRendersAtOnce() async {
        let board = Board()
        let model = BoardModel(board: board)
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [session("a")]))
        await settle { model.renders > 0 }
        try? await Task.sleep(for: .milliseconds(50))
        var drawn: ContinuousClock.Instant?
        model.onRender { _ in drawn = drawn ?? .now }
        let t = ContinuousClock.now
        _ = board.apply(.delta(Delta(seq: 1, upsert: [session("b")], remove: [])))
        await settle { drawn != nil }
        #expect(model.groups.flatMap(\.rows).count == 2)
        #expect(drawn.map { $0 - t } ?? .seconds(1) < .milliseconds(16))
    }

    /// Review Focus 4: the selected session's title and epic change (it moves to another group): it stays selected,
    /// and what the detail shows is its new self.
    @Test func aSelectedSessionWhoseTitleAndEpicChangeStaysSelected() async {
        let board = Board()
        let model = BoardModel(board: board)
        _ = board.apply(.meta(Meta(epics: ["Epic A", "Epic B"])))
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [
            session("a", epic: "Epic A"), session("b", epic: "Epic A"),
        ]))
        await settle { model.renders > 0 }
        let key = session("a").key
        model.selection = key

        _ = board.apply(.delta(Delta(seq: 1, upsert: [session("a", title: "renamed", epic: "Epic B")], remove: [])))
        await settle { model.groups.count == 2 }

        #expect(model.selection == key)
        #expect(model.selected?.title == "renamed" && model.selected?.epic == "Epic B")
        #expect(model.groups.map(\.name) == ["Epic A", "Epic B"])
        #expect(model.groups.first { $0.name == "Epic B" }?.rows.map(\.key) == [key])

        // Gone from the board: the selection is kept (it comes back if the session does), and nothing is shown.
        _ = board.apply(.delta(Delta(seq: 2, upsert: [], remove: [key])))
        await settle { model.selected == nil }
        #expect(model.selection == key && model.selected == nil)
    }

    /// The Needs-you count (the Dock badge) is the Needs you group's rows.
    @Test func needsYouCountsTheNeedsYouGroup() async {
        let board = Board()
        let model = BoardModel(board: board)
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [
            session("a", .needsYou), session("b", .capped), session("c", .working),
        ]))
        await settle { model.renders > 0 }
        #expect(model.needsYou == 2)
    }
}

@MainActor @Suite(.serialized) final class BoardCacheTests {
    private var dirs: [URL] = []

    deinit { for d in dirs { try? FileManager.default.removeItem(at: d) } }

    private func temp() -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("fleetkit-cache-\(UUID().uuidString)")
        dirs.append(dir)
        return dir.appendingPathComponent("board.cache")
    }

    @Test func roundTrips() {
        let c = BoardCache.Contents(
            savedAt: Date(timeIntervalSince1970: 1_793_966_400), machine: "fixture-mac",
            sessions: [full, session("b", .done)], meta: meta)
        let back = BoardCache.decode(BoardCache.encode(c))
        #expect(back == c)
    }

    /// The cache inside a bigger buffer (a slice whose indexes do not start at 0) reads the same, in place.
    @Test func decodesASliceInPlace() {
        let c = BoardCache.Contents(
            savedAt: Date(timeIntervalSince1970: 1_793_966_400), machine: "fixture-mac", sessions: [full], meta: meta)
        let buffer = Data([1, 2, 3]) + BoardCache.encode(c) + Data([9, 9])
        let slice = buffer[3..<(buffer.count - 2)]
        #expect(slice.startIndex == 3)
        #expect(BoardCache.decode(slice) == c)
    }

    /// The file and its folder are the user's alone: it holds session titles.
    @Test func theFileIs0600InA0700Folder() async throws {
        let url = temp()
        let cache = BoardCache(url: url)
        let board = Board()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [full]))
        cache.note(board)
        cache.flush()
        let fm = FileManager.default
        let file = try fm.attributesOfItem(atPath: url.path)[.posixPermissions] as? Int
        let dir = try fm.attributesOfItem(atPath: url.deletingLastPathComponent().path)[.posixPermissions] as? Int
        #expect(file == 0o600 && dir == 0o700)
        let left = try fm.contentsOfDirectory(atPath: url.deletingLastPathComponent().path)
        #expect(left == ["board.cache"])  // no temp file left behind

        // A folder someone else made 0755 first (fleetd, under the default umask) is tightened on the next write.
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: url.deletingLastPathComponent().path)
        _ = board.apply(.delta(Delta(seq: 1, upsert: [session("b")], remove: [])))
        BoardCache.writeFile(Data("x".utf8), to: url)
        let again = try fm.attributesOfItem(atPath: url.deletingLastPathComponent().path)[.posixPermissions] as? Int
        #expect(again == 0o700)
    }

    /// I2: a quiet board ticking (the 30 s refresh, a render that changes nothing) writes nothing.
    @Test func aTickWithNoChangeWritesNothing() async {
        let cache = BoardCache(url: temp())
        let board = Board()
        let model = BoardModel(board: board, cache: cache)
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [session("a")]))
        _ = board.apply(.meta(meta))
        await settle { cache.writes == 1 }
        try? await Task.sleep(for: .milliseconds(1100))  // past the throttle, so a write would show at once
        let before = (cache.writes, model.renders)
        for _ in 0..<5 {
            model.refresh()
            try? await Task.sleep(for: .milliseconds(30))
        }
        // A health reading alone is not a change to the board worth a write either.
        var m = meta
        m.health = Health(cpuPct: 99, memPct: 1, diskPct: 2, dev: 0, db: 0)
        _ = board.apply(.meta(m))
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.renders >= before.1 + 5)
        #expect(cache.writes == before.0)
        // A real change still writes.
        _ = board.apply(.delta(Delta(seq: 1, upsert: [session("b")], remove: [])))
        await settle { cache.writes == before.0 + 1 }
        #expect(cache.writes == before.0 + 1)
    }

    /// A file cut short anywhere (a crash mid-write that bypassed the atomic rename, a full disk) reads as no cache.
    @Test func aTruncatedOrForeignFileIsNoCache() {
        let bytes = BoardCache.encode(BoardCache.Contents(
            savedAt: Date(timeIntervalSince1970: 1_793_966_400), machine: "fixture-mac", sessions: [full], meta: meta))
        for n in 0..<bytes.count { #expect(BoardCache.decode(bytes.prefix(n)) == nil, "cut at \(n)") }
        #expect(BoardCache.decode(Data("not a cache at all, just some bytes".utf8)) == nil)
        var extra = bytes
        extra.append(0)
        #expect(BoardCache.decode(extra) == nil)
    }

    @Test func savesThroughTheFileAndLoadsBack() async {
        let url = temp()
        let cache = BoardCache(url: url)
        #expect(cache.load() == nil)
        let board = Board()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [full]))
        _ = board.apply(.meta(meta))
        cache.note(board)
        await settle { cache.load() != nil }
        let back = cache.load()
        #expect(back?.sessions == [full] && back?.meta == meta && back?.machine == "fixture-mac")

        // A truncated file on disk: no cache, no crash.
        let data = try! Data(contentsOf: url)
        try! data.prefix(data.count / 2).write(to: url)
        #expect(cache.load() == nil)
    }

    /// At most one write a second: a stream of changes writes once now, and once more (the last state) a second later.
    @Test func writesAtMostOnceASecond() async {
        let url = temp()
        let cache = BoardCache(url: url)
        let board = Board()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [session("a")]))
        for i in 1...20 {
            _ = board.apply(.delta(Delta(seq: UInt64(i), upsert: [session("s\(i)")], remove: [])))
            cache.note(board)
            try? await Task.sleep(for: .milliseconds(10))
        }
        #expect(cache.writes == 1)
        await settle(.seconds(2)) { cache.writes == 2 }
        #expect(cache.writes == 2)
        await settle { cache.load()?.sessions.count == 21 }
        #expect(cache.load()?.sessions.count == 21)
        try? await Task.sleep(for: .milliseconds(1200))
        #expect(cache.writes == 2)  // nothing new, nothing written
    }

    /// The model shows the cached board, marked with its time, until the first Snapshot; then the live one.
    @Test func theModelShowsTheCacheUntilTheFirstSnapshot() async {
        let board = Board()
        let saved = Date(timeIntervalSince1970: 1_793_966_400)
        let model = BoardModel(board: board, cached: BoardCache.Contents(
            savedAt: saved, machine: "fixture-mac", sessions: [session("old")], meta: meta))
        #expect(model.cachedAt == saved)
        #expect(model.groups.flatMap(\.rows).map(\.key.sessionId) == ["old"])
        // A Meta alone is not the live board yet.
        _ = board.apply(.meta(Meta()))
        await settle(.milliseconds(100)) { false }
        #expect(model.cachedAt == saved)
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: [session("new")]))
        await settle { model.cachedAt == nil }
        #expect(model.groups.flatMap(\.rows).map(\.key.sessionId) == ["new"])
    }
}

@Suite struct BoardRowsTests {
    private func groups(_ spec: [(String, [Session], [Session])]) -> [BoardGroup] {
        spec.map { BoardGroup(name: $0.0, kind: .epic, rows: $0.1, older: $0.2) }
    }

    @Test func flattensHeadersRowsAndTheOlderLine() {
        let g = groups([("A", [session("a"), session("b", .waiting)], [session("c", .done)])])
        let shut = BoardRows.flatten(g, openOlder: [], cached: false).map(\.id)
        #expect(shut.count == 4 && shut.first == "h:epic:A" && shut.last == "o:epic:A")
        let open = BoardRows.flatten(g, openOlder: ["epic:A"], cached: false)
        #expect(open.count == 5 && open.last?.session?.key.sessionId == "c")
        guard case let .header(_, _, tally) = open[0] else { Issue.record("no header"); return }
        #expect(tally == [.init(state: .working, count: 1), .init(state: .waiting, count: 1)])
    }

    /// One row's content changes: one reload, nothing removed or inserted.
    @Test func aChangedRowIsReloadedInPlace() {
        var b = session("b")
        let old = BoardRows.flatten(groups([("A", [session("a"), b], [])]), openOlder: [], cached: false)
        b.reason = "changed"
        let new = BoardRows.flatten(groups([("A", [session("a"), b], [])]), openOlder: [], cached: false)
        let e = BoardRows.edit(from: old, to: new)
        #expect(e.removed.isEmpty && e.inserted.isEmpty && e.reloaded == [2])
        #expect(BoardRows.edit(from: new, to: new).isEmpty)
    }

    /// Rows moving between groups, coming and going, folding: the edit applied as the table applies it gives the new
    /// list exactly.
    @Test func editsReproduceTheNewListAcrossMovesInsertsAndRemovals() {
        let a = session("a"), b = session("b"), c = session("c"), d = session("d"), e = session("e", .done)
        var b2 = b
        b2.title = "renamed"
        let cases: [([BoardGroup], [BoardGroup])] = [
            (groups([("A", [a, b], []), ("B", [c], [])]), groups([("A", [a], []), ("B", [b2, c], [])])),
            (groups([("A", [a, b, c], [e])]), groups([("A", [d, c, a], [e])])),
            (groups([("A", [a], [])]), groups([("B", [b, c], []), ("A", [a], [])])),
            (groups([("A", [a, b], [e]), ("B", [c, d], [])]), []),
            ([], groups([("A", [a, b], [e])])),
        ]
        for (x, y) in cases {
            for open in [Set<String>(), ["epic:A"]] {
                let old = BoardRows.flatten(x, openOlder: open, cached: true)
                let new = BoardRows.flatten(y, openOlder: open, cached: false)
                let edit = BoardRows.edit(from: old, to: new)
                #expect(BoardRows.apply(edit, to: old, from: new) == new)
            }
        }
    }
}

/// Ross saw the row text overlap at his window width: no two columns ever share a pixel, each lies inside its row,
/// state and title always show, and the right-hand columns give way in their order.
@Suite struct RowLayoutTests {
    @Test func noColumnsOverlapAndAllLieInsideTheRowAtAnyWidth() {
        for w in stride(from: CGFloat(40), through: 1600, by: 7) {
            let f = RowLayout.frames(width: w, height: 30)
            let row = CGRect(x: 0, y: 0, width: w, height: 30)
            let rects = Array(f.values)
            for r in rects { #expect(row.contains(r), "outside the row at \(w)") }
            for i in rects.indices {
                for j in rects.indices where j > i {
                    #expect(!rects[i].intersects(rects[j]), "overlap at \(w): \(rects[i]) \(rects[j])")
                }
            }
            if w >= 140 { #expect(f[.state] != nil && f[.title] != nil, "state and title at \(w)") }
        }
    }

    @Test func columnsGiveWayInOrderAndTheTitleKeepsItsRoom() {
        let wide = RowLayout.frames(width: 680, height: 30)
        #expect(Set(wide.keys) == Set(RowColumn.allCases))
        var dropped: [RowColumn] = []
        for w in stride(from: CGFloat(680), through: 260, by: -1) {
            let f = RowLayout.frames(width: w, height: 30)
            for c in RowLayout.dropOrder where f[c] == nil && !dropped.contains(c) { dropped.append(c) }
            // While any column that can give way still shows, the title has its room.
            if RowLayout.dropOrder.contains(where: { f[$0] != nil }) {
                #expect(f[.title]!.width >= RowLayout.minTitle, "title room at \(w)")
            }
        }
        #expect(dropped == RowLayout.dropOrder)
    }
}
