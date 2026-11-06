// The machines of one board (Ruling A-R1): per endpoint, in order (this machine's own Unix socket first, then other
// machines' fleetd over the tailnet), a source Board preloaded from its own cache, the BoardClient that keeps it live,
// and the TermClient its terminals open on; the MergedBoard over the sources. Everything a launch can show without a
// connection is read in `init`, before the first frame (ALL-R1: the cached board is in it). The list can change while
// it runs (Settings): a new endpoint's rows join the board, a removed one's leave it.
//
// The phone has the same machinery in its AppModel (lane D); this is the Mac's, in FleetKit so the two can share it.

import Combine
import Foundation

@MainActor
public final class Machines {
    /// One endpoint's machinery.
    @MainActor
    public final class Node {
        public let endpoint: Endpoint
        public let source = Board()
        public let client: BoardClient
        public let terms = TermClient()
        let cache: BoardCache?
        public fileprivate(set) var status: BoardClient.Status = .reconnecting
        /// The time of the rows shown while the machine is not live: the cache's, or when it was last live.
        public fileprivate(set) var cachedAt: Date?
        fileprivate var notePending = false
        fileprivate var watch: Task<Void, Never>?
        fileprivate var changes: AnyCancellable?
        fileprivate var started = false

        init(endpoint: Endpoint, cache: BoardCache?) {
            self.endpoint = endpoint
            self.cache = cache
            client = BoardClient(board: source)
        }
    }

    public let merged: MergedBoard
    public private(set) var nodes: [Node] = []
    /// Told of every status change, after the merged board has it (the app: the toolbar, the pane's terminals).
    public var onStatus: ((_ node: Node, _ was: BoardClient.Status) -> Void)?

    private let cacheURL: (Endpoint) -> URL?
    private var running = false

    /// `cacheURL` gives each endpoint's cache file, or nil for none (a test, a bench).
    public init(
        endpoints: [Endpoint], cacheURL: @escaping (Endpoint) -> URL? = { MergedBoard.cacheURL(for: $0) },
        unreachableAfter: Duration = .seconds(3)
    ) {
        self.cacheURL = cacheURL
        let made = Self.unique(endpoints).map { Self.node($0, cacheURL) }
        nodes = made
        // Its first pass is synchronous: the preloaded rows are in `output` now.
        merged = MergedBoard(sources: made.map(\.source), unreachableAfter: unreachableAfter)
        for n in made { merged.set(status: .reconnecting, cachedAt: n.cachedAt, forSource: n.source) }
    }

    /// The time of the first machine's cached rows (this Mac's), while they are what is shown.
    public var cachedAt: Date? { nodes.first?.cachedAt }

    /// Starts every client (board and terminals) of every endpoint, and those added later.
    public func start() {
        running = true
        for n in nodes { launch(n) }
    }

    /// Stops every client and writes each cache.
    public func stop() {
        running = false
        for n in nodes { halt(n) }
        flush()
    }

    /// The list changed: endpoints kept are left as they are; new ones join (started at once when running); removed
    /// ones are stopped and their rows leave the board.
    public func set(endpoints: [Endpoint]) {
        let want = Self.unique(endpoints)
        for n in nodes where !want.contains(n.endpoint) {
            halt(n)
            n.cache?.flush()
            merged.remove(n.source)
        }
        var next: [Node] = []
        for e in want {
            if let n = nodes.first(where: { $0.endpoint == e }) {
                next.append(n)
                continue
            }
            let n = Self.node(e, cacheURL)
            merged.add(n.source)
            merged.set(status: .reconnecting, cachedAt: n.cachedAt, forSource: n.source)
            next.append(n)
            if running { launch(n) }
        }
        nodes = next
    }

    /// The node whose board holds `machine`'s rows.
    public func node(for machine: String) -> Node? { nodes.first { $0.source.machine == machine } }

    public func node(for endpoint: Endpoint) -> Node? { nodes.first { $0.endpoint == endpoint } }

    /// Writes every pending cache now (on quit).
    public func flush() {
        for n in nodes { n.cache?.flush() }
    }

    private static func unique(_ list: [Endpoint]) -> [Endpoint] {
        var seen = Set<Endpoint>()
        return list.filter { seen.insert($0).inserted }
    }

    private static func node(_ e: Endpoint, _ cacheURL: (Endpoint) -> URL?) -> Node {
        let n = Node(endpoint: e, cache: cacheURL(e).map { BoardCache(url: $0) })
        if let c = n.cache?.load() {
            MergedBoard.preload(n.source, from: c)
            n.cachedAt = c.savedAt
        }
        return n
    }

    private func launch(_ n: Node) {
        guard !n.started else { return }
        n.started = true
        let client = n.client, terms = n.terms, endpoint = n.endpoint
        n.watch = Task { @MainActor [weak self, weak n] in
            for await s in await client.statuses() {
                guard let self, let n else { return }
                self.statusChanged(n, s)
            }
        }
        // objectWillChange fires before the change lands: note on the next turn, once per turn.
        n.changes = n.source.objectWillChange.sink { [weak n] _ in
            guard let n, n.status == .live, n.cache != nil, !n.notePending else { return }
            n.notePending = true
            Task { @MainActor [weak n] in
                guard let n else { return }
                n.notePending = false
                if n.status == .live { n.cache?.note(n.source) }
            }
        }
        Task {
            await client.start(endpoint: endpoint)
            await terms.start(endpoint: endpoint)
        }
    }

    private func halt(_ n: Node) {
        guard n.started else { return }
        n.started = false
        n.watch?.cancel()
        n.watch = nil
        n.changes = nil
        let client = n.client, terms = n.terms
        Task {
            await client.stop()
            await terms.stop()
        }
    }

    private func statusChanged(_ n: Node, _ s: BoardClient.Status) {
        let was = n.status
        guard s != was else { return }
        n.status = s
        if s == .live {
            n.cachedAt = nil
            n.cache?.note(n.source)  // the Snapshot that made it live landed before this status did
        } else if was == .live {
            n.cachedAt = Date()  // the rows kept are as of now
        }
        merged.set(status: s, cachedAt: n.cachedAt, forSource: n.source)
        onStatus?(n, was)
    }
}
