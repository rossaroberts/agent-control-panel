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

    /// The merged board of several machines, when the model draws one (`init(merged:cachedAt:)`).
    public let merged: MergedBoard?

    private var cached: Board?
    private var coalescer: Coalescer!
    private var watch: AnyCancellable?
    private var mergedWatch: AnyCancellable?
    private var observers: [@MainActor (_ cached: Bool) -> Void] = []
    /// What the cache last saw: a render that changes none of it (a clock tick, a health reading) writes nothing.
    private var noted: Noted?

    private struct Noted: Equatable {
        var run: UInt64?
        var seq: UInt64?
        var machine: String?
        var epics: [String]
        var groups: [String]
        var accounts: [Account]
    }

    public init(board: Board, cache: BoardCache? = nil, cached contents: BoardCache.Contents? = nil) {
        self.board = board
        self.cache = cache
        merged = nil
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
        if cached != nil { render() }  // the cached board is there before the window is
    }

    /// The board of several machines (Ruling A-R1): `merged.output`, which holds every source's rows (each preloaded
    /// from its own cache, so there is no cached board of the model's own). It counts as cached, with the time
    /// `cachedAt`, until one of its machines is live; the caches are the caller's to keep (one per source).
    public init(merged: MergedBoard, cachedAt: Date?) {
        board = merged.output
        cache = nil
        self.merged = merged
        self.cachedAt = cachedAt
        coalescer = Coalescer { [weak self] in self?.render() }
        watch = board.objectWillChange.sink { [weak self] _ in self?.coalescer.mark() }
        // A machine going live changes what is cached, and one added or named changes how rows read, without a row
        // changing. A health reading changes neither: the toolbar takes it, and the groups are not recomputed (M6).
        mergedWatch = merged.$machines
            .map { lines in Shape(live: lines.contains { $0.status == .live && $0.duplicateOf == nil },
                                  count: lines.count, first: lines.first?.machine) }
            .removeDuplicates()
            .dropFirst()
            .sink { [weak self] _ in self?.coalescer.mark() }
        render()  // the preloaded rows are there before the window is
    }

    /// What of a merged board's machines a render reads.
    private struct Shape: Equatable {
        var live: Bool
        var count: Int
        var first: String?
    }

    /// Whether the board shown is live: the board's own Snapshot, or for a merged board, a machine that is live.
    private var isLive: Bool {
        if let merged { return merged.machines.contains { $0.status == .live && $0.duplicateOf == nil } }
        return board.run != nil
    }

    /// The connection of the machine whose rows these are (the local status for a single board).
    public func status(of machine: String) -> BoardClient.Status {
        merged?.line(of: machine)?.status ?? status
    }

    /// How many machines the board is of (a row names its machine when there is more than one).
    public var machineCount: Int { merged?.machines.count ?? 1 }

    /// Called after each render (coalesced: at most once per display frame), with whether it showed the cached board.
    public func onRender(_ f: @escaping @MainActor (_ cached: Bool) -> Void) { observers.append(f) }

    /// The board the window shows: the live one once it has a Snapshot, the cached one until then.
    private var shown: Board? { merged != nil || board.run != nil ? board : cached }

    /// The selected session as the board has it now; nil while it is not on the board.
    public var selected: Session? {
        guard let k = selection else { return nil }
        return shown?.sessions[k]
    }

    /// Re-reads the board (the 48 h fold moves with the clock).
    public func refresh() { coalescer.mark() }

    private func render() {
        renders += 1
        let live = isLive
        if live { cached = nil }
        let source = shown
        let next = source?.groups(now: Date()) ?? []
        if next != groups { groups = next }
        let urgent = next.first { $0.kind == .needsYou }?.rows.count ?? 0
        if urgent != needsYou { needsYou = urgent }
        let m = source?.meta ?? Meta()
        if m != meta { meta = m }
        // A merged board's machine is its first source's (this Mac's own): the one its local actions are for.
        let here = merged.map { $0.machines.first.flatMap { $0.machine.isEmpty ? nil : $0.machine } } ?? source?.machine
        if here != machine { machine = here }
        if live && cachedAt != nil { cachedAt = nil }
        if live, let cache {
            let now = Noted(
                run: board.run, seq: board.seq, machine: board.machine, epics: m.epics, groups: m.groups,
                accounts: m.accounts)
            if now != noted {
                noted = now
                cache.note(board)
            }
        }
        for f in observers { f(!live) }
    }
}
