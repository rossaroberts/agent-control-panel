// The quick switcher's ranking (⌘K, plan 3 Task 6): a session matches a query on any of its fields (title, name, job),
// by its best one, in three tiers: the field starts with the query, a word of it does, or the query's letters are in
// it in order. Ties keep the board's order. Case is ignored. An `Index` lowers every field once, when the switcher
// opens, so each keystroke is one pass of byte compares (a board of hundreds ranks in well under a millisecond).

public enum Rank {
    public enum Tier: Int, Comparable, Sendable {
        case prefix, wordStart, subsequence

        public static func < (a: Tier, b: Tier) -> Bool { a.rawValue < b.rawValue }
    }

    /// How `query` matches `text`, or nil.
    public static func tier(_ query: String, in text: String) -> Tier? {
        tier(Array(query.lowercased().utf8), in: Array(text.lowercased().utf8))
    }

    static func tier(_ q: [UInt8], in t: [UInt8]) -> Tier? {
        if q.isEmpty { return .prefix }
        if q.count > t.count { return nil }
        if starts(q, t, at: 0) { return .prefix }
        // A word starts after anything that is not a letter or a digit (space, "-", "/", "_", "."…).
        for i in stride(from: 1, through: t.count - q.count, by: 1) where !isWord(t[i - 1]) && isWord(t[i]) && starts(q, t, at: i) {
            return .wordStart
        }
        // In order, the query's spaces not needed.
        var j = 0
        for c in t where j < q.count {
            while j < q.count, q[j] == 0x20 { j += 1 }
            if j < q.count, c == q[j] { j += 1 }
        }
        while j < q.count, q[j] == 0x20 { j += 1 }
        return j == q.count ? .subsequence : nil
    }

    private static func starts(_ q: [UInt8], _ t: [UInt8], at i: Int) -> Bool {
        guard i + q.count <= t.count else { return false }
        for k in 0..<q.count where t[i + k] != q[k] { return false }
        return true
    }

    /// ASCII letters and digits, and every byte of a multi-byte character.
    private static func isWord(_ c: UInt8) -> Bool {
        (c >= 0x30 && c <= 0x39) || (c >= 0x61 && c <= 0x7a) || (c >= 0x41 && c <= 0x5a) || c >= 0x80
    }

    /// The items that match, best tier first, the input order within a tier.
    public static func order<T>(_ items: [T], query: String, fields: (T) -> [String]) -> [T] {
        Index(items, fields: fields).order(query: query)
    }

    /// The items with their fields lowered once, for ranking at every keystroke.
    public struct Index<T> {
        private let items: [T]
        private let lowered: [[[UInt8]]]

        public init(_ items: [T], fields: (T) -> [String]) {
            self.items = items
            lowered = items.map { fields($0).map { Array($0.lowercased().utf8) } }
        }

        public var count: Int { items.count }

        public func order(query: String) -> [T] {
            let q = Array(query.lowercased().utf8)
            if q.isEmpty { return items }
            var tiers: [[T]] = [[], [], []]
            for (i, fs) in lowered.enumerated() {
                var best: Tier?
                for f in fs {
                    guard let t = Rank.tier(q, in: f) else { continue }
                    if best == nil || t < best! { best = t }
                    if t == .prefix { break }
                }
                if let best { tiers[best.rawValue].append(items[i]) }
            }
            return tiers[0] + tiers[1] + tiers[2]
        }
    }
}
