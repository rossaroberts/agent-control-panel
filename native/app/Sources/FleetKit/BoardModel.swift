// What the board window shows, kept apart from the window so it can be tested without one: the groups (re-read from
// the Board at most once per display frame, Coalescer), the selection (by SessionKey, so a row whose title, epic or
// label changes stays selected wherever it moves), the Needs-you count for the Dock badge, and the cached board shown
// before the first Snapshot, marked with the time it was saved.

import Combine
import Foundation

@MainActor
public final class BoardModel: ObservableObject {
    public let board: Board
    public let cache: BoardCache?

    @Published public private(set) var groups: [BoardGroup] = []
    /// The Needs you group's rows: live sessions that need you or stopped at a limit (the Dock badge).
    @Published public private(set) var needsYou = 0
    @Published public private(set) var meta = Meta()
    @Published public private(set) var machine: String?
    /// The time of the cached board while it is the one shown; nil from the first Snapshot on.
    @Published public private(set) var cachedAt: Date?
    @Published public var status: BoardClient.Status = .reconnecting
    @Published public var selection: SessionKey?

    /// How many renders ran (the coalescing test, and the bench).
    public private(set) var renders = 0
    /// Called after each render, with whether it showed the cached board (the bench marks "on screen" from it).
    public var onRender: (@MainActor (_ cached: Bool) -> Void)?

    private var cached: Board?
    private var coalescer: Coalescer!
    private var watch: AnyCancellable?

    public init(board: Board, cache: BoardCache? = nil, cached contents: BoardCache.Contents? = nil) {
        self.board = board
        self.cache = cache
        if let c = contents {
            let b = Board()
            _ = b.apply(.snapshot(run: 0, seq: 0, machine: c.machine, sessions: c.sessions))
            _ = b.apply(.meta(c.meta))
            cached = b
            cachedAt = c.savedAt
        }
        coalescer = Coalescer { [weak self] in self?.render() }
        // objectWillChange fires before the change lands; the coalesced render runs on a later turn, after it.
        watch = board.objectWillChange.sink { [weak self] _ in self?.coalescer.mark() }
        if cached != nil { render() }  // the cached board is drawn at once, with the window's first frame
    }

    /// The board the window shows: the live one once it has a Snapshot, the cached one until then.
    private var shown: Board? { board.run != nil ? board : cached }

    /// The selected session as the board has it now; nil while it is not on the board.
    public var selected: Session? {
        guard let k = selection else { return nil }
        return shown?.sessions[k]
    }

    /// Re-reads the board (the ages and the 48 h fold move with the clock).
    public func refresh() { coalescer.mark() }

    private func render() {
        renders += 1
        let live = board.run != nil
        if live { cached = nil }
        let source = shown
        let next = source?.groups(now: Date()) ?? []
        if next != groups { groups = next }
        let urgent = next.first { $0.kind == .needsYou }?.rows.count ?? 0
        if urgent != needsYou { needsYou = urgent }
        let m = source?.meta ?? Meta()
        if m != meta { meta = m }
        if source?.machine != machine { machine = source?.machine }
        if live && cachedAt != nil { cachedAt = nil }
        if live { cache?.note(board) }
        onRender?(!live)
    }
}
