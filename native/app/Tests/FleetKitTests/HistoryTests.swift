// The terminal pane's navigation, without a window (plan 3 Task 6): the back/forward history of opened sessions
// (⌘[ ⌘]), the warm set of Claude terminals kept alive out of sight, the quick switcher's ranking (⌘K), and which
// sessions a terminal can be opened on.

import Foundation
import Testing

@testable import FleetKit

@Suite struct HistoryTests {
    @Test func pushBackAndForward() {
        var h = History<String>()
        #expect(h.current == nil && !h.canGoBack && !h.canGoForward)
        h.push("a")
        h.push("b")
        h.push("c")
        #expect(h.entries == ["a", "b", "c"] && h.current == "c")
        #expect(h.back() == "b")
        #expect(h.back() == "a")
        #expect(h.back() == nil && h.current == "a")  // nothing before the first: stays
        #expect(h.forward() == "b")
        #expect(h.forward() == "c")
        #expect(h.forward() == nil && h.current == "c")
    }

    @Test func consecutiveDuplicatesAreOneEntry() {
        var h = History<String>()
        h.push("a")
        h.push("a")
        h.push("b")
        h.push("b")
        h.push("a")
        #expect(h.entries == ["a", "b", "a"])
    }

    @Test func aPushAfterGoingBackDropsWhatWasAhead() {
        var h = History<String>()
        for k in ["a", "b", "c"] { h.push(k) }
        _ = h.back()
        _ = h.back()
        h.push("d")
        #expect(h.entries == ["a", "d"] && !h.canGoForward)
        // Pushing where you already are (after going back) is not a new entry, and keeps what is ahead.
        var g = History<String>()
        for k in ["a", "b"] { g.push(k) }
        _ = g.back()
        g.push("a")
        #expect(g.entries == ["a", "b"] && g.forward() == "b")
    }

    @Test func goingBackSkipsSessionsNoLongerThere() {
        var h = History<String>()
        for k in ["a", "gone", "b"] { h.push(k) }
        #expect(h.back { $0 != "gone" } == "a")
        #expect(h.forward { $0 != "gone" } == "b")
        // None left that way: it stays where it is.
        #expect(h.forward { $0 != "gone" } == nil && h.current == "b")
        #expect(h.back { $0 == "nowhere" } == nil && h.current == "b")
    }

    @Test func itIsBounded() {
        var h = History<Int>(limit: 5)
        for i in 0..<12 { h.push(i) }
        #expect(h.entries == [7, 8, 9, 10, 11] && h.current == 11)
        #expect(h.back() == 10)
    }
}

@Suite struct WarmSetTests {
    @Test func keepsTheLastSixAndLetsGoOfTheOldest() {
        var w = WarmSet<String>(limit: 6)
        var let_go: [String] = []
        for k in ["a", "b", "c", "d", "e", "f"] { let_go += w.use(k) }
        #expect(let_go.isEmpty && w.keys.count == 6)
        let_go = w.use("g")
        #expect(let_go == ["a"])
        // Using one again makes it the newest: the next to go is the oldest of the rest.
        _ = w.use("b")
        #expect(w.use("h") == ["c"])
        #expect(Set(w.keys) == ["b", "d", "e", "f", "g", "h"])
    }

    /// M1: the one on screen is never let go of, even when it is the oldest (it was neither closed nor kept before).
    @Test func theOneOnScreenIsNeverLetGoOf() {
        var w = WarmSet<String>(limit: 2)
        _ = w.use("shown")
        _ = w.use("a")
        #expect(w.use("b", keeping: "shown") == ["a"])
        #expect(Set(w.keys) == ["shown", "b"])
        #expect(w.use("c", keeping: "shown") == ["b"])
        #expect(w.contains("shown"))
    }

    @Test func removeForgetsOne() {
        var w = WarmSet<String>(limit: 2)
        _ = w.use("a")
        _ = w.use("b")
        w.remove("a")
        #expect(w.use("c").isEmpty && Set(w.keys) == ["b", "c"])
    }
}

@Suite struct RankTests {
    private struct Item {
        var title: String
        var name: String?
        var job: String?
    }

    private let fixture = [
        Item(title: "a deploy check", name: "s1", job: "0a1b2c3d"),  // "dep" at a word start
        Item(title: "deploy pipeline", name: "s2", job: nil),  // "dep" as the prefix
        Item(title: "the dashboard export", name: "s3", job: nil),  // d…e…p as a subsequence
        Item(title: "unrelated", name: "s4", job: "deadbeef"),  // no d…e…p in order anywhere
        Item(title: "Deploy notes", name: "s5", job: nil),  // prefix, any case
    ]

    private func fields(_ i: Item) -> [String] { [i.title, i.name, i.job].compactMap { $0 } }

    @Test func tiers() {
        #expect(Rank.tier("dep", in: "deploy pipeline") == .prefix)
        #expect(Rank.tier("dep", in: "a deploy check") == .wordStart)
        #expect(Rank.tier("dep", in: "fix-deploy") == .wordStart)
        #expect(Rank.tier("dep", in: "the dashboard export") == .subsequence)
        #expect(Rank.tier("dep", in: "unrelated") == nil)
        #expect(Rank.tier("DEP", in: "Deploy notes") == .prefix)
        #expect(Rank.tier("", in: "anything") == .prefix)
    }

    @Test func exactPrefixThenWordStartThenSubsequenceThenTheBoardsOrder() {
        let ranked = Rank.order(fixture, query: "dep", fields: fields).map(\.name)
        #expect(ranked == ["s2", "s5", "s1", "s3"])
    }

    @Test func theBestFieldCounts() {
        // "0a1b" is s1's job's prefix: it outranks a subsequence in a title.
        let ranked = Rank.order(fixture, query: "0a1b", fields: fields).map(\.name)
        #expect(ranked == ["s1"])
    }

    @Test func anEmptyQueryKeepsEveryoneInOrder() {
        #expect(Rank.order(fixture, query: "", fields: fields).map(\.name) == ["s1", "s2", "s3", "s4", "s5"])
    }

    @Test func aBoardFullRanksWellInsideAFrame() {
        let many = (0..<500).map { Item(title: "session number \($0) of the fixture board", name: "n\($0)", job: nil) }
        let index = Rank.Index(many, fields: fields)
        // Each keystroke over 500 rows inside one 16 ms frame, even unoptimised (the app's bench times the release
        // build's whole keystroke, drawing included).
        // The best of 5 per keystroke: the work, not the scheduler (this suite runs beside a 50 MB terminal flood).
        for q in ["s", "se", "ses", "sess", "num 4", "fixture b", "zzz"] {
            let best = (0..<5).map { _ -> Duration in
                let start = ContinuousClock.now
                _ = index.order(query: q)
                return ContinuousClock.now - start
            }.min()!
            #expect(best < .milliseconds(16), "\(q): \(best)")
        }
    }
}

@Suite struct AttachTests {
    private func s(_ engine: Engine, live: Bool = true, job: String? = nil, sid: String = "s", cwd: String? = nil,
                   machine: String = "fixture-mac") -> Session
    {
        Session(
            key: SessionKey(machine: machine, profile: engine == .codex ? "codex" : "work", sessionId: sid),
            engine: engine, name: nil, cwd: cwd, state: .waiting, reason: "fixture", live: live, pid: nil, stage: nil,
            model: nil, effort: nil, contextTokens: nil, lastAt: nil, said: nil, doing: nil, lastAsk: nil,
            firstAsk: nil, startedAt: nil, hookAt: nil, writtenAt: nil, jobId: job, title: nil, epic: nil,
            archived: false, account: nil)
    }

    private let uuid = "0a1b2c3d-0000-4000-8000-00000000000a"

    @Test func aLiveBackgroundJobAttaches() {
        #expect(Attach.of(s(.claude, job: "0a1b2c3d"), local: "fixture-mac") == .open(.claude(profile: "work", job: "0a1b2c3d")))
    }

    @Test func aCodexThreadThatIsClosedResumesOnlyWhenAsked() {
        let x = s(.codex, live: false, sid: uuid, cwd: "/tmp/fixture")
        guard case let .ask(t, _) = Attach.of(x, local: "fixture-mac") else { Issue.record("not ask"); return }
        #expect(t == .codex(profile: "codex", sessionId: uuid, cwd: "/tmp/fixture"))
    }

    @Test func everythingElseSaysWhy() {
        let cases = [
            s(.claude, job: nil),  // interactive
            s(.claude, live: false, job: "0a1b2c3d"),  // ended
            s(.claude, job: "not-hex!"),  // not a job id
            s(.claude, job: "0a1b2c3d", machine: "elsewhere"),  // another machine
            s(.codex, live: true, sid: uuid, cwd: "/tmp/fixture"),  // open in Codex
            s(.codex, live: false, sid: uuid, cwd: nil),  // no folder
            s(.codex, live: false, sid: "short", cwd: "/tmp/fixture"),  // not a thread id
            s(.muse),
        ]
        for c in cases {
            guard case let .none(why) = Attach.of(c, local: "fixture-mac") else {
                Issue.record("attachable: \(c)")
                continue
            }
            #expect(!why.isEmpty)
        }
    }
}

/// Ross saw the row text overlap at his window width: in the rail's two-line rows nothing shares a pixel, each part lies
/// inside its row, at any width.
@Suite struct RailLayoutTests {
    @Test func noPartsOverlapAndAllLieInsideTheRowAtAnyWidth() {
        for w in stride(from: CGFloat(30), through: 1600, by: 7) {
            let f = RowLayout.frames(width: w, height: RowLayout.height)
            let row = CGRect(x: 0, y: 0, width: w, height: RowLayout.height)
            let rects = Array(f.values)
            for r in rects { #expect(row.contains(r), "outside the row at \(w)") }
            for i in rects.indices {
                for j in rects.indices where j > i {
                    #expect(!rects[i].intersects(rects[j]), "overlap at \(w): \(rects[i]) \(rects[j])")
                }
            }
            if w >= 120 { #expect(f[.dot] != nil && f[.title] != nil && f[.info] != nil, "all parts at \(w)") }
        }
    }

    @Test func theSecondLineIsUnderTheFirst() {
        let f = RowLayout.frames(width: 360, height: RowLayout.height)
        #expect(f[.info]!.minY >= f[.title]!.maxY)
        #expect(f[.title]!.minX == f[.info]!.minX)
    }
}

@Suite struct CacheFolderTests {
    private func mode(_ path: String) -> Int {
        ((try? FileManager.default.attributesOfItem(atPath: path))?[.posixPermissions] as? NSNumber)?.intValue ?? -1
    }

    @Test func onlyAFolderNamedFleetThatIsOursIsMadePrivate() throws {
        let base = FileManager.default.temporaryDirectory.appendingPathComponent("fleet-cache-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: base) }
        let other = base.appendingPathComponent("elsewhere"), fleet = base.appendingPathComponent("Fleet")
        for d in [other, fleet] {
            try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
            try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: d.path)
        }
        BoardCache.writeFile(Data("x".utf8), to: other.appendingPathComponent("board.cache"))
        BoardCache.writeFile(Data("x".utf8), to: fleet.appendingPathComponent("board.cache"))
        #expect(mode(other.path) == 0o755)  // FLEET_BOARD_CACHE pointed somewhere else: left as it was
        #expect(mode(fleet.path) == 0o700)
        #expect(FileManager.default.fileExists(atPath: other.appendingPathComponent("board.cache").path))
    }
}
