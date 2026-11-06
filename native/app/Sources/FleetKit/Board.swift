// The board model: the sessions of one fleetd, kept in step by its frames (native/PROTOCOL.md "Rules a client relies
// on"), and grouped and ordered as `public/fleet.html` `renderMachines` does (lines 2085-2118), with plan 3's rule for
// done and ended rows (Global Constraints: those older than 48 h fold behind a count; archived rows are not shown).

import Combine
import Foundation

/// What a frame did to the board.
public enum BoardChange: Equatable, Sendable {
    case applied
    /// The board lost step (a `seq` gap, a delta with nothing to apply it to, or a frame a board connection never
    /// carries): reconnect with `hello()` and take what is sent. The board is left as it was until then.
    case needsResync
}

/// One section of the board.
public struct BoardGroup: Equatable, Sendable, Identifiable {
    public enum Kind: Equatable, Sendable {
        /// Live sessions that need you or stopped at a limit, above every other group.
        case needsYou
        /// An epic of `fleet.json`.
        case epic
        /// A group you made.
        case user
        /// Everything else.
        case other
    }

    public var name: String
    public var kind: Kind
    /// The rows shown.
    public var rows: [Session]
    /// Done or ended rows older than 48 h, behind the group's "N older" count; same order as `rows`.
    public var older: [Session]

    public var id: String { "\(kind):\(name)" }
}

extension SessionState {
    /// The state as a word on the board: `fleet.html` `WORD` (line 620), "closed" being done or ended.
    public var word: String {
        switch self {
        case .needsYou: "needs you"
        case .working: "working"
        case .capped: "at limit"
        case .waiting: "waiting"
        case .done: "done"
        case .ended: "ended"
        case .unreachable: "unreachable"
        }
    }

    /// `fleet.html` `ORDER` (line 619): needs-you 0, working 1, capped 2, ready 3, closed 4. Done, ended, and
    /// unreachable (which fleet.html has no word for) sort as closed.
    var order: Int {
        switch self {
        case .needsYou: 0
        case .working: 1
        case .capped: 2
        case .waiting: 3
        case .done, .ended, .unreachable: 4
        }
    }
}

@MainActor
public final class Board: ObservableObject {
    @Published public private(set) var sessions: [SessionKey: Session] = [:]
    @Published public private(set) var meta = Meta()
    /// The run of the last Snapshot, and the `seq` of the last frame applied: what `hello()` resumes from. Not
    /// published: no view shows them, and a delta that changes no row then fires no `objectWillChange`.
    public private(set) var run: UInt64?
    public private(set) var seq: UInt64?
    /// The daemon's machine name, from the last Snapshot.
    @Published public private(set) var machine: String?

    public var health: Health? { meta.health }

    /// A Hello was just sent with a `since`: the next Delta is the resume, whose `seq` may jump ahead.
    private var resuming = false

    /// How old a done or ended row is before it folds.
    public static let foldAfter: TimeInterval = 48 * 3600

    public init() {}

    /// The Hello for a board connection: resume from this board's run and seq when it has them, else ask for a
    /// Snapshot. Send it as the first frame of each connection; it readies the board for the resume delta.
    public func hello() -> Frame {
        resuming = run != nil && seq != nil
        return .hello(proto: Frame.proto, since: run == nil ? nil : seq, run: run, role: .board)
    }

    public func apply(_ frame: Frame) -> BoardChange {
        switch frame {
        case let .snapshot(run, seq, machine, list):
            resuming = false
            sessions = Dictionary(list.map { ($0.key, $0) }, uniquingKeysWith: { _, last in last })
            self.run = run
            self.seq = seq
            if self.machine != machine { self.machine = machine }
            return .applied

        case let .delta(d):
            let wasResuming = resuming
            resuming = false
            guard run != nil, let held = seq else { return .needsResync }
            // The resume delta carries the daemon's current seq (or ours, when caught up); every other one is next.
            guard wasResuming ? d.seq >= held : d.seq == held &+ 1 else { return .needsResync }
            var next = sessions
            for s in d.upsert { next[s.key] = s }
            for k in d.remove { next[k] = nil }
            if next != sessions { sessions = next }
            seq = d.seq
            return .applied

        case let .meta(m):
            if m != meta { meta = m }
            return .applied

        case .hello, .error, .termOpen, .termIn, .termSize, .termClose, .termOut, .termEnd:
            resuming = false
            return .needsResync
        }
    }

    /// The board as it is shown: Needs you, then the epics in `fleet.json` order, then the groups you made, then Other;
    /// a group with nothing in it is left out.
    public func groups(now: Date) -> [BoardGroup] {
        let nowMs = Int64((now.timeIntervalSince1970 * 1000).rounded())
        let cutoff = nowMs - Int64(Self.foldAfter * 1000)
        let list = sessions.values.filter { !$0.archived }

        // fleet.html:2087-2088: live and needing you or at a limit; needs-you first, then the longest wait
        // (`lastAt ?? since`, oldest first).
        let urgent = list.filter { $0.live && ($0.state == .needsYou || $0.state == .capped) }
            .sorted { a, b in
                if a.state.order != b.state.order { return a.state.order < b.state.order }
                let (ta, tb) = (a.lastAt ?? a.startedAt ?? 0, b.lastAt ?? b.startedAt ?? 0)
                return ta != tb ? ta < tb : keyOrder(a.key, b.key)
            }
        let urgentKeys = Set(urgent.map(\.key))

        // fleet.html:2094: the epics, then the groups you made, then Other (each once).
        var names: [(String, BoardGroup.Kind)] = []
        var seen: Set<String> = []
        for (n, k) in meta.epics.map({ ($0, BoardGroup.Kind.epic) }) + meta.groups.map({ ($0, .user) }) + [("Other", .other)]
        where seen.insert(n).inserted {
            names.append((n, k))
        }
        // fleet.html:2096: an epic that is not a group of the board is Other.
        let groupOf = { (s: Session) in s.epic.flatMap { seen.contains($0) ? $0 : nil } ?? "Other" }

        var byGroup: [String: [Session]] = [:]
        for s in list where !urgentKeys.contains(s.key) { byGroup[groupOf(s), default: []].append(s) }

        var out: [BoardGroup] = []
        if !urgent.isEmpty { out.append(BoardGroup(name: "Needs you", kind: .needsYou, rows: urgent, older: [])) }
        for (name, kind) in names {
            guard let members = byGroup[name], !members.isEmpty else { continue }  // fleet.html:2099
            // fleet.html:2098: by ORDER, then the most recent first (`lastAt ?? startedAt ?? since`); and the live
            // rows above the rest (:2111 draws `shown.filter(x => x.live)`, :2113-2117 the closed ones under them).
            let sorted = members.sorted { a, b in
                if a.live != b.live { return a.live }
                if a.state.order != b.state.order { return a.state.order < b.state.order }
                let (ta, tb) = (a.lastAt ?? a.startedAt ?? 0, b.lastAt ?? b.startedAt ?? 0)
                return ta != tb ? ta > tb : keyOrder(a.key, b.key)
            }
            // Plan 3 in place of fleet.html:2113-2117's fold of every closed row: done or ended, and last heard of
            // more than 48 h ago. A row with no time at all is not known to be old, so it shows.
            let folds = { (s: Session) -> Bool in
                guard s.state == .done || s.state == .ended, let t = s.lastAt ?? s.startedAt else { return false }
                // `t` comes off the wire: compare without arithmetic on it, so no value of it can overflow.
                return t < cutoff
            }
            out.append(BoardGroup(name: name, kind: kind, rows: sorted.filter { !folds($0) }, older: sorted.filter(folds)))
        }
        return out
    }
}

/// A stable last tie-break (fleet.html's sort is stable over the snapshot's order, which a dictionary does not keep).
private func keyOrder(_ a: SessionKey, _ b: SessionKey) -> Bool {
    (a.machine, a.profile, a.sessionId) < (b.machine, b.profile, b.sessionId)
}
