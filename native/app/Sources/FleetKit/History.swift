// The terminal pane's memory of where you have been (plan 3 Task 6): the sessions you opened, in order, for ⌘[ and ⌘]
// (a browser's back and forward), and the few Claude terminals kept alive out of sight so going back to one is a view
// swap (fleet.html `trimTerms`, there 3; here 6, the daemon's warm pool being the second layer).

/// Back and forward through the sessions opened. Opening one where you are is not a new entry (consecutive entries are
/// never the same); opening one after going back drops what was ahead, as a browser does. At most `limit` are kept.
public struct History<Key: Hashable & Sendable>: Sendable {
    public private(set) var entries: [Key] = []
    /// Where you are in `entries`; -1 while empty.
    public private(set) var index = -1
    public let limit: Int

    public init(limit: Int = 100) {
        self.limit = max(1, limit)
    }

    public var current: Key? { entries.indices.contains(index) ? entries[index] : nil }
    public var canGoBack: Bool { index > 0 }
    public var canGoForward: Bool { index >= 0 && index < entries.count - 1 }

    public mutating func push(_ k: Key) {
        if current == k { return }
        if index < entries.count - 1 { entries.removeSubrange((index + 1)...) }
        entries.append(k)
        if entries.count > limit { entries.removeFirst(entries.count - limit) }
        index = entries.count - 1
    }

    /// The nearest earlier entry that is still `there` (a session gone from the board is passed over), now current;
    /// nil, and nothing moves, when there is none.
    public mutating func back(where there: (Key) -> Bool = { _ in true }) -> Key? {
        move(by: -1, there)
    }

    public mutating func forward(where there: (Key) -> Bool = { _ in true }) -> Key? {
        move(by: 1, there)
    }

    private mutating func move(by step: Int, _ there: (Key) -> Bool) -> Key? {
        var i = index + step
        while entries.indices.contains(i) {
            if there(entries[i]) {
                index = i
                return entries[i]
            }
            i += step
        }
        return nil
    }
}

/// The keys used most recently, at most `limit`: `use` makes one the newest and says which fell out (the oldest).
public struct WarmSet<Key: Hashable & Sendable>: Sendable {
    /// Oldest first.
    public private(set) var keys: [Key] = []
    public let limit: Int

    public init(limit: Int) {
        self.limit = max(1, limit)
    }

    /// `k` is the newest now; returns the keys let go of to stay within the limit, oldest first.
    public mutating func use(_ k: Key) -> [Key] {
        keys.removeAll { $0 == k }
        keys.append(k)
        guard keys.count > limit else { return [] }
        let out = Array(keys.prefix(keys.count - limit))
        keys.removeFirst(out.count)
        return out
    }

    public mutating func remove(_ k: Key) {
        keys.removeAll { $0 == k }
    }

    public func contains(_ k: Key) -> Bool { keys.contains(k) }
}
