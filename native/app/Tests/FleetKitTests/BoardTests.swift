// The board model: what a Snapshot, a Delta and a Meta do to it (PROTOCOL.md "Rules a client relies on"), and how it
// groups and orders its rows (the rules of `public/fleet.html`, cited by line, with plan 3's done/ended folding).

import Foundation
import Testing

@testable import FleetKit

private let hour: Int64 = 3_600_000
private let now = Date(timeIntervalSince1970: 1_793_966_400)  // a fixed "now"
private let nowMs = Int64(now.timeIntervalSince1970 * 1000)

private func row(
    _ sid: String, _ state: SessionState, live: Bool = true, epic: String? = nil, lastAt: Int64? = nil,
    startedAt: Int64? = nil, archived: Bool = false, machine: String = "fixture-mac"
) -> Session {
    Session(
        key: SessionKey(machine: machine, profile: "default", sessionId: sid), engine: .claude, name: sid, cwd: nil,
        state: state, reason: "fixture", live: live, pid: nil, stage: nil, model: nil, effort: nil, contextTokens: nil,
        lastAt: lastAt, said: nil, doing: nil, lastAsk: nil, firstAsk: nil, startedAt: startedAt, hookAt: nil,
        writtenAt: nil, jobId: nil, title: sid, epic: epic, archived: archived, account: nil)
}

/// Minutes / hours before `now`, in epoch milliseconds.
private func minsAgo(_ m: Int64) -> Int64 { nowMs - m * 60_000 }
private func hoursAgo(_ h: Int64) -> Int64 { nowMs - h * hour }

private func ids(_ s: [Session]) -> [String] { s.map(\.key.sessionId) }

@MainActor @Suite struct BoardApplyTests {
    let a = row("a", .working), b = row("b", .waiting), c = row("c", .needsYou), d = row("d", .done, live: false)

    /// Review Focus 1: Snapshot, then consecutive Deltas, then a gap.
    @Test func snapshotThenDeltasThenAGapAsksForResync() {
        let board = Board()
        #expect(board.apply(.snapshot(run: 7, seq: 10, machine: "fixture-mac", sessions: [a, b])) == .applied)
        #expect(board.run == 7 && board.seq == 10 && board.machine == "fixture-mac")
        #expect(Set(board.sessions.keys) == [a.key, b.key])

        var b2 = b
        b2.state = .working
        #expect(board.apply(.delta(Delta(seq: 11, upsert: [c, b2], remove: []))) == .applied)
        #expect(board.sessions[b.key]?.state == .working)
        #expect(board.apply(.delta(Delta(seq: 12, upsert: [], remove: [a.key]))) == .applied)
        #expect(Set(board.sessions.keys) == [b.key, c.key] && board.seq == 12)

        // seq 13 is lost: 14 is a gap, and is not applied.
        #expect(board.apply(.delta(Delta(seq: 14, upsert: [d], remove: [b.key]))) == .needsResync)
        #expect(Set(board.sessions.keys) == [b.key, c.key] && board.seq == 12)
        // A repeat of one already applied is out of step too.
        #expect(board.apply(.delta(Delta(seq: 12, upsert: [d], remove: []))) == .needsResync)
        #expect(board.sessions[d.key] == nil)
    }

    /// Review Focus 1: a Snapshot of a new run (fleetd restarted) replaces the board whole, and seq starts over.
    @Test func snapshotOfANewRunReplacesEverythingWithNoGhosts() {
        let board = Board()
        _ = board.apply(.snapshot(run: 7, seq: 500, machine: "fixture-mac", sessions: [a, b, c]))
        _ = board.apply(.delta(Delta(seq: 501, upsert: [d], remove: [])))
        #expect(board.sessions.count == 4)

        var b3 = b
        b3.reason = "after the restart"
        #expect(board.apply(.snapshot(run: 9, seq: 0, machine: "fixture-mac", sessions: [b3])) == .applied)
        #expect(board.run == 9 && board.seq == 0)
        #expect(Array(board.sessions.keys) == [b.key], "no ghost of a, c or d")
        #expect(board.sessions[b.key]?.reason == "after the restart")
        #expect(board.apply(.delta(Delta(seq: 1, upsert: [a], remove: []))) == .applied, "deltas continue from 0")
        // A Snapshot may arrive at any time, of the same run too, and it is the whole board.
        #expect(board.apply(.snapshot(run: 9, seq: 4100, machine: "fixture-mac", sessions: [c])) == .applied)
        #expect(Array(board.sessions.keys) == [c.key] && board.seq == 4100)
    }

    @Test func aDeltaBeforeAnySnapshotAsksForResync() {
        let board = Board()
        #expect(board.apply(.delta(Delta(seq: 1, upsert: [a], remove: []))) == .needsResync)
        #expect(board.sessions.isEmpty && board.seq == nil)
    }

    /// PROTOCOL.md "The conversation" 2: a Hello with this run's `since` is answered by one Delta holding everything
    /// since, whose `seq` is the daemon's current one (`store.rs` `since`), or by the empty caught-up Delta whose `seq`
    /// is the client's own (Ruling R18).
    @Test func helloResumesAndTheFirstDeltaMayJump() {
        let board = Board()
        #expect(board.hello() == .hello(proto: 4, since: nil, run: nil, role: .board), "a new board asks for a Snapshot")
        _ = board.apply(.snapshot(run: 7, seq: 10, machine: "fixture-mac", sessions: [a, b]))
        #expect(board.hello() == .hello(proto: 4, since: 10, run: 7, role: .board))
        #expect(board.apply(.delta(Delta(seq: 25, upsert: [c], remove: [a.key]))) == .applied, "the resume delta")
        #expect(Set(board.sessions.keys) == [b.key, c.key] && board.seq == 25)
        // After it, deltas are consecutive again.
        #expect(board.apply(.delta(Delta(seq: 27, upsert: [], remove: []))) == .needsResync)

        _ = board.hello()
        #expect(board.apply(.delta(Delta(seq: 25, upsert: [], remove: []))) == .applied, "caught up")
        #expect(board.seq == 25)
        _ = board.hello()
        #expect(board.apply(.delta(Delta(seq: 24, upsert: [], remove: []))) == .needsResync, "behind what we hold")
        // Or the daemon answers the resume with a Snapshot (another run, or too far behind).
        _ = board.hello()
        #expect(board.apply(.snapshot(run: 8, seq: 3, machine: "fixture-mac", sessions: [d])) == .applied)
        #expect(Array(board.sessions.keys) == [d.key])
        #expect(board.apply(.delta(Delta(seq: 9, upsert: [], remove: []))) == .needsResync, "no resume after it")
    }

    @Test func metaCarriesTheGroupsAccountsAndHealth() {
        let board = Board()
        #expect(board.health == nil && board.meta.epics.isEmpty)
        let health = Health(cpuPct: 12, memPct: 64, diskPct: 81, dev: 3, db: 1)
        let meta = Meta(epics: ["Alpha"], groups: ["Mine"], accounts: [], health: health)
        #expect(board.apply(.meta(meta)) == .applied)
        #expect(board.meta == meta && board.health == health)
        #expect(board.apply(.meta(Meta(epics: [], groups: [], accounts: [], health: nil))) == .applied)
        #expect(board.health == nil)
    }

    /// A frame that has no place on a board connection (an Error, which closes it; a terminal frame; a Hello) means the
    /// connection is done for: reconnect.
    @Test func otherFramesAskForResyncAndChangeNothing() {
        let board = Board()
        _ = board.apply(.snapshot(run: 7, seq: 10, machine: "fixture-mac", sessions: [a]))
        for f: Frame in [.error(message: "x"), .termOut(ref: 1, data: Data()), .termEnd(ref: 1, error: nil), board.hello()] {
            #expect(board.apply(f) == .needsResync)
        }
        #expect(Array(board.sessions.keys) == [a.key] && board.seq == 10 && board.run == 7)
    }
}

@MainActor @Suite struct BoardGroupTests {
    /// The fixture board. Meta: epics Alpha, Beta (fleet.json order), the group you made: Mine.
    static let sessions: [Session] = [
        // Needs you: live and needs-you or capped (fleet.html:2087).
        row("n1", .needsYou, epic: "Beta", lastAt: minsAgo(10)),
        row("n2", .needsYou, lastAt: minsAgo(60)),
        row("c1", .capped, epic: "Alpha", lastAt: minsAgo(120)),
        row("c2", .capped, epic: "Alpha", lastAt: nil, startedAt: minsAgo(5)),  // waits since its start (`since`)
        // Not live, so not in Needs you: it stays in its epic, under the live rows (fleet.html:2111), first by ORDER.
        row("nl", .needsYou, live: false, epic: "Alpha", lastAt: minsAgo(3)),
        row("a1", .working, epic: "Alpha", lastAt: minsAgo(5)),
        row("a2", .waiting, epic: "Alpha", lastAt: minsAgo(1)),
        row("a3", .working, epic: "Alpha", lastAt: minsAgo(1)),
        row("a4", .done, live: false, epic: "Alpha", lastAt: hoursAgo(47)),  // under 48 h: shown
        row("a5", .ended, live: false, epic: "Alpha", lastAt: hoursAgo(49)),  // folded
        row("a6", .done, live: false, epic: "Alpha", lastAt: nil, startedAt: hoursAgo(72)),  // folded, by its start
        row("a7", .waiting, epic: "Alpha", lastAt: hoursAgo(200)),  // old but not done/ended: shown
        // Archived: never shown. Beta then holds only n1, which is in Needs you, so Beta is not shown at all.
        row("b1", .working, epic: "Beta", lastAt: minsAgo(2), archived: true),
        row("b2", .needsYou, epic: "Beta", lastAt: minsAgo(500), archived: true),
        // Mine holds only a folded row: the group shows, with nothing but its "1 older".
        row("m1", .ended, live: false, epic: "Mine", lastAt: hoursAgo(100)),
        // Other: an epic no longer in fleet.json, and none (fleet.html:2096).
        row("o1", .waiting, epic: "Gone epic", lastAt: minsAgo(30)),
        row("o2", .working, lastAt: minsAgo(40)),
        row("o3", .ended, live: false),  // no time known: not provably older than 48 h, so shown
        row("o4", .working, lastAt: minsAgo(40), machine: "fixture-mac-2"),  // ties with o2: by key
        // Times off the wire at the edges of Int64: folding and sorting must not overflow.
        row("o5", .ended, live: false, lastAt: .min),  // folded
        row("o6", .done, live: false, lastAt: .max),  // "in the future": shown, newest first
    ]

    static func board() -> Board {
        let board = Board()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "fixture-mac", sessions: sessions))
        _ = board.apply(.meta(Meta(epics: ["Alpha", "Beta"], groups: ["Mine"], accounts: [], health: nil)))
        return board
    }

    @Test func groupsComeNeedsYouThenEpicsThenYoursThenOther() {
        let groups = Self.board().groups(now: now)
        // fleet.html:2091 Needs you on top; :2094 epics in fleet.json order, then your groups, then Other; :2099 a
        // group with nothing to show is left out (Beta).
        #expect(groups.map(\.name) == ["Needs you", "Alpha", "Mine", "Other"])
        #expect(groups.map(\.kind) == [.needsYou, .epic, .user, .other])
    }

    /// fleet.html:2087-2088: needs-you before capped (ORDER, :619), then the longest wait first (`lastAt ?? since`,
    /// oldest first).
    @Test func needsYouIsOldestWaitFirst() {
        let top = Self.board().groups(now: now)[0]
        #expect(ids(top.rows) == ["n2", "n1", "c1", "c2"])
        #expect(top.older.isEmpty)
    }

    /// fleet.html:2111 draws a group's live rows, and :2113-2117 its closed (not live) ones under them; within each,
    /// :2098 orders by ORDER (:619 needs-you 0, working 1, capped 2, ready 3, closed 4: here done and ended), then the
    /// most recent first (`lastAt ?? startedAt`). Plan 3 (Global Constraints) replaces :2113-2117's fold of every
    /// closed row: only done/ended rows older than 48 h are folded, behind the group's always-visible count.
    @Test func rowsFollowOrderThenMostRecentAndOldDoneEndedFold() {
        let groups = Self.board().groups(now: now)
        let alpha = groups[1], mine = groups[2], other = groups[3]
        #expect(ids(alpha.rows) == ["a3", "a1", "a2", "a7", "nl", "a4"])
        #expect(ids(alpha.older) == ["a5", "a6"])
        #expect(mine.rows.isEmpty && ids(mine.older) == ["m1"])
        #expect(ids(other.rows) == ["o2", "o4", "o1", "o6", "o3"])
        #expect(ids(other.older) == ["o5"])
    }

    @Test func archivedRowsAreNotShownAnywhere() {
        let shown = Self.board().groups(now: now).flatMap { $0.rows + $0.older }.map(\.key.sessionId)
        #expect(!shown.contains("b1") && !shown.contains("b2"))
        #expect(shown.count == Self.sessions.count - 2, "every other row is somewhere, once")
        #expect(Set(shown).count == shown.count)
    }

    /// The 48 h edge: exactly 48 h is not older than 48 h.
    @Test func foldingIsStrictlyOlderThan48Hours() {
        let board = Board()
        _ = board.apply(
            .snapshot(
                run: 1, seq: 0, machine: "m",
                sessions: [row("x", .done, live: false, lastAt: hoursAgo(48)), row("y", .ended, live: false, lastAt: hoursAgo(48) - 1)]))
        let other = board.groups(now: now)[0]
        #expect(ids(other.rows) == ["x"] && ids(other.older) == ["y"])
    }

    /// Without a Meta every session is under Other (Review Focus 5's app side).
    @Test func withoutMetaEverythingIsOther() {
        let board = Board()
        _ = board.apply(.snapshot(run: 1, seq: 0, machine: "m", sessions: [row("x", .working, epic: "Alpha")]))
        #expect(board.groups(now: now).map(\.name) == ["Other"])
    }

    /// fleet.html:620 `WORD`, with "closed" as done or ended (Global Constraints).
    @Test func stateWords() {
        #expect([SessionState.needsYou, .working, .capped, .waiting, .done, .ended, .unreachable].map(\.word) == [
            "needs you", "working", "at limit", "waiting", "done", "ended", "unreachable",
        ])
    }
}
