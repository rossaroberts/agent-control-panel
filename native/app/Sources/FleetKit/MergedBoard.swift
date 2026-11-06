// One board from several machines (Rulings D-R3, A-R1): a `Board` per endpoint, each fed by its own `BoardClient`,
// mirrored into `output`, an ordinary `Board` that `BoardModel` draws unchanged. Keys carry their machine, so rows never
// collide. A machine that cannot be reached keeps its last rows (spec §7): after `unreachableAfter` without a live
// connection each of its rows is mirrored as `Unreachable`, greyed, with the reason "<machine> unreachable since
// <hh:mm> UTC; was <state>: <reason>" (as the hub does, Ruling P5-R1), and nothing disappears for being out of reach.
//
// The mirror does no work it can skip (ALL-R1): only the sources that changed since the last pass are diffed, each
// against its own last mirrored copy, and any number of source changes in one main-actor turn become one `.delta` to
// `output` on the next turn (a `Coalescer` with no interval: one pass per turn, no frame wait, no timer).
//
// Lifted from lane D's FleetPhoneKit (the phone switches to this one), with: sources added and removed while it runs;
// the unreachable rows themselves (the phone greyed them in its view only); the merged Meta's epics and groups from
// the first source (the Mac's own), accounts unioned by (engine, name) (Ruling P5-R1).

import Combine
import Foundation

/// One machine's line on the board: its connection, the time of the cached rows shown while it is not live, and its
/// health (per machine; the merged Meta carries none).
public struct MachineLine: Equatable, Sendable {
    public var machine: String
    public var status: BoardClient.Status
    public var cachedAt: Date?
    public var health: Health?
    /// When it was last known to be reachable (its rows are as of then), while it is not live; nil while live.
    public var downSince: Date?
    /// Not live for `unreachableAfter` or more: its rows are shown as unreachable.
    public var unreachable: Bool

    public init(
        machine: String, status: BoardClient.Status, cachedAt: Date?, health: Health?, downSince: Date? = nil,
        unreachable: Bool = false
    ) {
        self.machine = machine
        self.status = status
        self.cachedAt = cachedAt
        self.health = health
        self.downSince = downSince
        self.unreachable = unreachable
    }
}

@MainActor
public final class MergedBoard: ObservableObject {
    public private(set) var sources: [Board]
    /// The merged board: run 1, its seq counting its own deltas.
    public let output = Board()

    /// One line per source, in source order. `machine` is the source's machine name, "" until it has one.
    @Published public private(set) var machines: [MachineLine]

    /// How long a machine may be out of reach before its rows say so: a fleetd restart, or a dropped connection that
    /// comes straight back, never greys the board.
    public let unreachableAfter: Duration

    /// How many passes changed `output` (the coalescing test), and how long the last pass took on the main actor
    /// (the ALL-R1 bench: merging adds ≤ 1 ms per delta).
    public private(set) var passes = 0
    public private(set) var lastPass: Duration = .zero

    /// Each source's sessions as last mirrored into `output` (unreachable ones as shown).
    private var mirrored: [[SessionKey: Session]]
    private var dirty = Set<ObjectIdentifier>()
    private var coalescer: Coalescer!
    private var watches: [ObjectIdentifier: AnyCancellable] = [:]
    /// Rows of removed sources still to take out of `output` on the next pass.
    private var dropped: [SessionKey] = []
    private let clock: () -> Date
    /// Per source, the generation of its last transition to down (the grace check it armed).
    private var downs: [ObjectIdentifier: Int] = [:]

    public init(sources: [Board], unreachableAfter: Duration = .seconds(3), clock: @escaping () -> Date = Date.init) {
        self.sources = []
        self.unreachableAfter = unreachableAfter
        self.clock = clock
        mirrored = []
        machines = []
        _ = output.apply(.snapshot(run: 1, seq: 0, machine: "", sessions: []))
        coalescer = Coalescer(interval: .zero) { [weak self] in self?.pass() }
        for b in sources { attach(b) }
        // What the sources already hold (a preloaded cache) is in `output` now, before the first frame is drawn.
        pass()
    }

    /// Adds a source (a machine added in Settings). Its rows are in `output` on the next turn.
    public func add(_ source: Board) {
        attach(source)
        coalescer.mark()
    }

    private func attach(_ b: Board) {
        guard !sources.contains(where: { $0 === b }) else { return }
        sources.append(b)
        mirrored.append([:])
        // Not live until its client says so: out of reach from now, unless it is live within `unreachableAfter`.
        machines.append(MachineLine(
            machine: b.machine ?? "", status: .reconnecting, cachedAt: nil, health: b.health, downSince: clock()))
        let id = ObjectIdentifier(b)
        // objectWillChange fires before the change lands; the pass runs on a later turn, after it.
        watches[id] = b.objectWillChange.sink { [weak self] _ in self?.mark(id) }
        dirty.insert(id)
        armGrace(b)
    }

    /// Removes a source (a machine removed in Settings): its rows leave `output` on the next turn. The one way a row
    /// leaves the board other than its own daemon removing it.
    public func remove(_ source: Board) {
        guard let i = index(of: source) else { return }
        dropped += mirrored[i].keys
        sources.remove(at: i)
        mirrored.remove(at: i)
        machines.remove(at: i)
        watches[ObjectIdentifier(source)] = nil
        downs[ObjectIdentifier(source)] = nil
        dirty.remove(ObjectIdentifier(source))
        coalescer.mark()
    }

    /// A source's connection changed. A source that is not live for `unreachableAfter` is unreachable; its rows stay.
    /// `cachedAt` is the time its rows are as of while it is not live (its cache's, or when it was last live).
    public func set(status: BoardClient.Status, cachedAt: Date?, forSource source: Board) {
        guard let i = index(of: source) else { return }
        var line = machines[i]
        let wasLive = line.status == .live
        line.status = status
        line.cachedAt = cachedAt
        if status == .live {
            line.downSince = nil
            line.unreachable = false
            downs[ObjectIdentifier(source)] = (downs[ObjectIdentifier(source)] ?? 0) &+ 1
        } else if wasLive || line.downSince == nil {
            line.downSince = cachedAt ?? clock()
            armGrace(source)
        } else if let c = cachedAt, let d = line.downSince, c < d {
            line.downSince = c  // the rows shown are older than when it was first seen down
        }
        guard line != machines[i] else { return }
        let rowsChange = line.unreachable != machines[i].unreachable || line.downSince != machines[i].downSince
        machines[i] = line
        if rowsChange { mark(ObjectIdentifier(source)) }
    }

    /// Whether `machine`'s rows show as unreachable now.
    public func isUnreachable(_ machine: String) -> Bool {
        machines.first(where: { $0.machine == machine })?.unreachable ?? false
    }

    /// The line of the source that holds `machine`'s rows.
    public func line(of machine: String) -> MachineLine? { machines.first { $0.machine == machine } }

    /// Puts a cached board in a source before it connects: the cached Snapshot at run 0, which no daemon's run is
    /// (PROTOCOL.md: random at each start), so the first live Hello is sent a whole Snapshot.
    public static func preload(_ board: Board, from c: BoardCache.Contents) {
        _ = board.apply(.snapshot(run: 0, seq: 0, machine: c.machine, sessions: c.sessions))
        _ = board.apply(.meta(c.meta))
    }

    /// The source's own cache file: `BoardCache.defaultURL` with the endpoint's `host-port` in its name
    /// (`board-<host>-<port>.cache`; anything but letters, digits, `.` and `-` becomes `_`).
    public nonisolated static func cacheURL(for endpoint: Endpoint) -> URL {
        let tag: String
        switch endpoint {
        case let .tcp(host, port), let .keyedTCP(host, port, _): tag = "\(host)-\(port)"
        case let .unix(url): tag = url.lastPathComponent
        }
        let safe = String(tag.map { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "." || $0 == "-") ? $0 : "_" })
        let base = BoardCache.defaultURL
        let ext = base.pathExtension
        let name = base.deletingPathExtension().lastPathComponent + "-" + safe + (ext.isEmpty ? "" : "." + ext)
        return base.deletingLastPathComponent().appendingPathComponent(name)
    }

    /// A row of a machine out of reach: `Unreachable`, not live, the reason saying since when and what it was; every
    /// other field kept. A row that is already unreachable (a hub's) is left as it is.
    public nonisolated static func unreachable(_ s: Session, since: Date) -> Session {
        guard s.state != .unreachable else { return s }
        var u = s
        u.state = .unreachable
        u.live = false
        u.reason = "\(s.key.machine) unreachable since \(hhmmUTC(since)) UTC; was \(s.state.word): \(s.reason)"
        return u
    }

    /// `fleet_model::decide::hh_mm_utc`: the time of day in UTC as "hh:mm".
    public nonisolated static func hhmmUTC(_ d: Date) -> String {
        let minutes = Int((d.timeIntervalSince1970 / 60).rounded(.down)) % (24 * 60)
        let m = minutes < 0 ? minutes + 24 * 60 : minutes
        return String(format: "%02d:%02d", m / 60, m % 60)
    }

    private func index(of source: Board) -> Int? { sources.firstIndex { $0 === source } }

    /// One check, `unreachableAfter` from now: a source that has not been live since is shown unreachable. Each
    /// transition to down arms a check of its own; a later transition (live, or down again) makes the older one moot.
    private func armGrace(_ source: Board) {
        let id = ObjectIdentifier(source)
        let gen = (downs[id] ?? 0) &+ 1
        downs[id] = gen
        let after = unreachableAfter
        Task { @MainActor [weak self, weak source] in
            if after > .zero { try? await Task.sleep(for: after) }
            guard let self, let source, self.downs[id] == gen, let i = self.index(of: source) else { return }
            var line = self.machines[i]
            guard line.status != .live, !line.unreachable else { return }
            line.unreachable = true
            self.machines[i] = line
            self.mark(id)
        }
    }

    private func mark(_ id: ObjectIdentifier) {
        dirty.insert(id)
        coalescer.mark()
    }

    private func pass() {
        let start = ContinuousClock.now
        var upsert: [Session] = []
        var remove = dropped
        dropped = []
        if !dirty.isEmpty {
            for i in sources.indices where dirty.contains(ObjectIdentifier(sources[i])) {
                let line = machines[i]
                let since = line.downSince ?? clock()
                let raw = sources[i].sessions
                let now = line.unreachable ? raw.mapValues { Self.unreachable($0, since: since) } : raw
                let was = mirrored[i]
                for (k, s) in now where was[k] != s { upsert.append(s) }
                for k in was.keys where now[k] == nil { remove.append(k) }
                mirrored[i] = now

                var next = line
                if let m = sources[i].machine { next.machine = m }
                next.health = sources[i].health
                if next != machines[i] { machines[i] = next }
            }
            dirty.removeAll()
        }
        // A key still held by another source (a machine listed twice) is not removed from under it.
        if !remove.isEmpty {
            let held = Set(mirrored.flatMap(\.keys))
            remove.removeAll { held.contains($0) }
        }
        if !upsert.isEmpty || !remove.isEmpty {
            _ = output.apply(.delta(Delta(seq: (output.seq ?? 0) &+ 1, upsert: upsert, remove: remove)))
            passes += 1
        }
        _ = output.apply(.meta(mergedMeta()))  // publishes only when it differs
        lastPass = ContinuousClock.now - start
    }

    /// Ruling P5-R1: epics and groups from the first source that has a board (the Mac's own, first in the list);
    /// accounts unioned by (engine, name), in source order; no health (it is per machine, in `machines`).
    private func mergedMeta() -> Meta {
        let primary = sources.first { $0.run != nil }
        var accounts: [Account] = []
        for b in sources {
            for a in b.meta.accounts where !accounts.contains(where: { $0.name == a.name && $0.engine == a.engine }) {
                accounts.append(a)
            }
        }
        return Meta(
            epics: primary?.meta.epics ?? [], groups: primary?.meta.groups ?? [], accounts: accounts, health: nil)
    }
}
