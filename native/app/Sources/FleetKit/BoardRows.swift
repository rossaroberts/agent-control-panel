// The board as the window's table draws it: one flat list of rows (notes, group headers, sessions, "N older" lines),
// each with a stable id, and the edit that turns one list into the next: rows removed, rows inserted, and rows whose
// content changed in place. The table applies the edit (removeRows / insertRows / reloadData(forRowIndexes:)) instead
// of reloading everything, so a delta costs the rows it touched (Ruling P3-R6).

import Foundation

public enum BoardRow: Equatable, Sendable {
    public struct Tally: Equatable, Sendable {
        public var state: SessionState
        public var count: Int
    }

    /// A line above the groups (the cached board's time, why fleetd is not answering).
    case note(id: String, text: String)
    /// A group's name and its tally of live rows (fleet.html `tally`).
    case header(id: String, name: String, tally: [Tally])
    case session(Session, cached: Bool)
    /// The done or ended rows older than 48 h, folded or open.
    case older(groupId: String, count: Int, open: Bool)

    public var id: String {
        switch self {
        case let .note(id, _): "n:\(id)"
        case let .header(id, _, _): "h:\(id)"
        case let .session(s, _): "s:\(s.key.machine)\u{1f}\(s.key.profile)\u{1f}\(s.key.sessionId)"
        case let .older(g, _, _): "o:\(g)"
        }
    }

    public var session: Session? {
        if case let .session(s, _) = self { return s }
        return nil
    }
}

public enum BoardRows {
    /// The tally order of fleet.html `tally`.
    static let tallied: [SessionState] = [.needsYou, .working, .capped, .waiting]

    /// The groups as rows: each group's header, its rows, and its "N older" line (and those rows, when open).
    public static func flatten(_ groups: [BoardGroup], openOlder: Set<String>, cached: Bool) -> [BoardRow] {
        var out: [BoardRow] = []
        for g in groups {
            let tally = tallied.compactMap { st -> BoardRow.Tally? in
                let n = g.rows.filter { $0.live && $0.state == st }.count
                return n > 0 ? BoardRow.Tally(state: st, count: n) : nil
            }
            out.append(.header(id: g.id, name: g.name, tally: tally))
            out += g.rows.map { .session($0, cached: cached) }
            if !g.older.isEmpty {
                let open = openOlder.contains(g.id)
                out.append(.older(groupId: g.id, count: g.older.count, open: open))
                if open { out += g.older.map { .session($0, cached: cached) } }
            }
        }
        return out
    }

    public struct Edit: Equatable, Sendable {
        /// Indexes in the old list, removed first (all at once).
        public var removed: IndexSet
        /// Indexes in the new list, inserted after the removals.
        public var inserted: IndexSet
        /// Indexes in the new list of rows kept whose content changed.
        public var reloaded: IndexSet

        public var isEmpty: Bool { removed.isEmpty && inserted.isEmpty && reloaded.isEmpty }
    }

    public static func edit(from old: [BoardRow], to new: [BoardRow]) -> Edit {
        let oldIds = old.map(\.id), newIds = new.map(\.id)
        var removed = IndexSet(), inserted = IndexSet(), reloaded = IndexSet()
        if oldIds != newIds {
            for change in newIds.difference(from: oldIds) {
                switch change {
                case let .remove(offset, _, _): removed.insert(offset)
                case let .insert(offset, _, _): inserted.insert(offset)
                }
            }
        }
        // A row neither removed nor inserted keeps its place in order: pair the survivors up.
        let keptOld = old.indices.filter { !removed.contains($0) }
        let keptNew = new.indices.filter { !inserted.contains($0) }
        for (o, n) in zip(keptOld, keptNew) where old[o] != new[n] { reloaded.insert(n) }
        return Edit(removed: removed, inserted: inserted, reloaded: reloaded)
    }

    /// What applying `edit` to `old` gives, the way NSTableView applies it (the test's model of the table).
    public static func apply(_ edit: Edit, to old: [BoardRow], from new: [BoardRow]) -> [BoardRow] {
        var rows = old
        for i in edit.removed.reversed() { rows.remove(at: i) }
        for i in edit.inserted { rows.insert(new[i], at: i) }
        for i in edit.reloaded { rows[i] = new[i] }
        return rows
    }
}
