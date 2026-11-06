// The last board, kept on disk so the next launch draws it before it has connected (spec §9.3: launch to the last
// board ≤ 50 ms): `~/Library/Application Support/Fleet/board.cache` (FLEET_BOARD_CACHE overrides), written at most
// once a second, atomically (a temp file renamed over it).
//
// The format is this version's own wire: "FLEETBC1", the PROTO as u16 LE, the time it was saved (epoch ms, i64 LE),
// then a Snapshot frame (run and seq 0: a cache resumes nothing) and a Meta frame, each a u32 LE length and a postcard
// body. A file of another PROTO, cut short, or with anything after the Meta reads as no cache.

import Foundation

@MainActor
public final class BoardCache {
    public struct Contents: Equatable, Sendable {
        public var savedAt: Date
        public var machine: String
        public var sessions: [Session]
        public var meta: Meta

        public init(savedAt: Date, machine: String, sessions: [Session], meta: Meta) {
            self.savedAt = savedAt
            self.machine = machine
            self.sessions = sessions
            self.meta = meta
        }
    }

    public nonisolated static let minInterval: Duration = .seconds(1)
    private nonisolated static let magic = Data("FLEETBC1".utf8)

    public nonisolated static var defaultURL: URL {
        if let p = ProcessInfo.processInfo.environment["FLEET_BOARD_CACHE"], !p.isEmpty { return URL(fileURLWithPath: p) }
        return FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/Fleet/board.cache")
    }

    public let url: URL
    /// How many times the file was written (the throttle's test).
    public private(set) var writes = 0

    private var lastWrite: ContinuousClock.Instant?
    private var dirty: Board?
    private var trailing = false
    private let disk = DispatchQueue(label: "fleet.board-cache", qos: .utility)

    public init(url: URL = BoardCache.defaultURL) {
        self.url = url
    }

    /// Reads the file: nil when there is none, or it is not a whole cache of this version.
    public nonisolated func load() -> Contents? {
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
        return Self.decode(data)
    }

    /// The board changed: write it now if a second has passed since the last write, else once when it has.
    public func note(_ board: Board) {
        guard board.run != nil, board.machine != nil else { return }  // only a live board is worth keeping
        dirty = board
        guard !trailing else { return }
        let wait = lastWrite.map { Self.minInterval - (ContinuousClock.now - $0) } ?? .zero
        if wait <= .zero {
            write()
            return
        }
        trailing = true
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wait)
            guard let self else { return }
            self.trailing = false
            self.write()
        }
    }

    /// Writes a pending change now (on quit), and waits for it to be on disk.
    public func flush() {
        write()
        disk.sync {}
    }

    private func write() {
        guard let board = dirty, let machine = board.machine else { return }
        dirty = nil
        lastWrite = .now
        writes += 1
        let data = Self.encode(Contents(
            savedAt: Date(), machine: machine,
            sessions: board.sessions.values.sorted { ($0.key.profile, $0.key.sessionId) < ($1.key.profile, $1.key.sessionId) },
            meta: board.meta))
        let url = url
        disk.async { Self.writeFile(data, to: url) }
    }

    /// The folder is set to 0700 on every write (fleetd may have made it first, under the default umask), and the
    /// temp file is created 0600 and exclusive before a byte goes in, then renamed over the cache: the titles in it
    /// are never readable by anyone else, and a reader sees the old file or the new one, never half of one.
    nonisolated static func writeFile(_ data: Data, to url: URL) {
        let fm = FileManager.default
        let dir = url.deletingLastPathComponent()
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try? fm.setAttributes([.posixPermissions: 0o700], ofItemAtPath: dir.path)
        let tmp = dir.appendingPathComponent(".\(url.lastPathComponent).\(getpid()).tmp")
        unlink(tmp.path)  // one left by a crash of this pid number
        let fd = open(tmp.path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0o600)
        guard fd >= 0 else { return }
        let whole = data.withUnsafeBytes { buf -> Bool in
            var off = 0
            while off < buf.count {
                let n = Darwin.write(fd, buf.baseAddress! + off, buf.count - off)
                if n <= 0 { return false }
                off += n
            }
            return true
        }
        close(fd)
        if !whole || rename(tmp.path, url.path) != 0 { unlink(tmp.path) }
    }

    public nonisolated static func encode(_ c: Contents) -> Data {
        var out = magic
        appendLE(&out, UInt64(Frame.proto), bytes: 2)
        appendLE(&out, UInt64(bitPattern: Int64((c.savedAt.timeIntervalSince1970 * 1000).rounded())), bytes: 8)
        // Neither frame can pass maxFrame for any board a daemon would send (the daemon's own limit is the same).
        out.append((try? Frame.snapshot(run: 0, seq: 0, machine: c.machine, sessions: c.sessions).encode()) ?? Data())
        out.append((try? Frame.meta(c.meta).encode()) ?? Data())
        return out
    }

    /// Reads in place: the mapped file is indexed, and each frame decoded from its slice, with no copy of either.
    public nonisolated static func decode(_ d: Data) -> Contents? {
        var at = d.startIndex
        func take(_ n: Int) -> Data? {
            guard n >= 0, d.endIndex - at >= n else { return nil }
            defer { at += n }
            return d[at..<(at + n)]
        }
        func le(_ n: Int) -> UInt64? {
            guard let b = take(n) else { return nil }
            return b.reversed().reduce(0) { $0 << 8 | UInt64($1) }
        }
        func frame() -> Frame? {
            guard let n = le(4), n <= UInt64(Frame.maxFrame), let body = take(Int(n)) else { return nil }
            return try? Frame.decode(body)
        }
        guard take(magic.count) == magic, le(2) == UInt64(Frame.proto), let ms = le(8) else { return nil }
        guard case let .snapshot(_, _, machine, sessions)? = frame(), case let .meta(meta)? = frame(), at == d.endIndex
        else { return nil }
        return Contents(
            savedAt: Date(timeIntervalSince1970: Double(Int64(bitPattern: ms)) / 1000), machine: machine,
            sessions: sessions, meta: meta)
    }

    private nonisolated static func appendLE(_ out: inout Data, _ v: UInt64, bytes: Int) {
        for i in 0..<bytes { out.append(UInt8(truncatingIfNeeded: v >> (8 * UInt64(i)))) }
    }
}
