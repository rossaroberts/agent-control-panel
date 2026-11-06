// Pages: a short list of named web pages Fleet.app opens in windows of their own (Vitals first). The list is plain
// data in UserDefaults (names and URLs only: a page's credentials never come near it, see PageAuth.swift). Nothing
// here touches WebKit, so the model costs nothing at launch.

import Foundation

struct Page: Equatable, Codable {
    var name: String
    var url: String

    static let vitals = Page(name: "Vitals", url: "https://vitals.clintonhealthaccess.org:8780/")

    /// The URL, when it is a usable http(s) address with a host.
    var address: URL? { Pages.parseURL(url) }
}

struct Pages {
    static let key = "FleetPages"
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    /// The saved list, or the default (Vitals) while none was saved. A list emptied by removing every page stays empty.
    var all: [Page] {
        get {
            guard let d = defaults.data(forKey: Self.key), let pages = try? JSONDecoder().decode([Page].self, from: d)
            else { return [.vitals] }
            return pages
        }
        set { defaults.set(try? JSONEncoder().encode(newValue), forKey: Self.key) }
    }

    enum AddResult: Equatable { case added, replaced, invalid }

    /// Adds a page (a page with the same name, any case, is replaced).
    @discardableResult mutating func add(name: String, url typed: String) -> AddResult {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty, let u = Self.parseURL(typed) else { return .invalid }
        var list = all
        let page = Page(name: n, url: u.absoluteString)
        if let i = list.firstIndex(where: { $0.name.caseInsensitiveCompare(n) == .orderedSame }) {
            list[i] = page
            all = list
            return .replaced
        }
        list.append(page)
        all = list
        return .added
    }

    mutating func remove(name: String) {
        all = all.filter { $0.name != name }
    }

    /// What was typed as an address: http or https with a host; a bare host.name gets https.
    static func parseURL(_ typed: String) -> URL? {
        var t = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return nil }
        if !t.contains("://") { t = "https://" + t }
        guard let u = URL(string: t), let s = u.scheme?.lowercased(), s == "http" || s == "https",
            let h = u.host, !h.isEmpty
        else { return nil }
        return u
    }
}
