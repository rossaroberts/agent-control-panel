// The frames of the fleetd socket (native/PROTOCOL.md, PROTO 4), mirroring `crates/fleet-model/src/types.rs` and
// `wire.rs` field for field and variant for variant: postcard is positional, so the order of every `init(postcard:)`
// and `write(postcard:)` below is the format. The golden frames of `crates/fleet-model/tests/golden/` pin it
// (GoldenTests). Any change here is a PROTO bump on both sides.

import Foundation

public enum Engine: UInt32, Hashable, Sendable, CaseIterable {
    case claude, codex, muse
}

public enum State: UInt32, Hashable, Sendable, CaseIterable {
    case needsYou, working, waiting, capped, done, ended, unreachable
}

/// Which connection a `Hello` opens.
public enum Role: UInt32, Hashable, Sendable {
    case board, terminals
}

public struct SessionKey: Hashable, Sendable {
    public var machine: String
    public var profile: String
    public var sessionId: String

    public init(machine: String, profile: String, sessionId: String) {
        self.machine = machine
        self.profile = profile
        self.sessionId = sessionId
    }
}

/// One row of the board, as `types.rs` `Session` (see its doc comments for each field).
public struct Session: Equatable, Sendable {
    public var key: SessionKey
    public var engine: Engine
    public var name: String?
    public var cwd: String?
    public var state: State
    public var reason: String
    public var live: Bool
    public var pid: UInt32?
    public var stage: String?
    public var model: String?
    public var effort: String?
    public var contextTokens: UInt64?
    /// Epoch milliseconds, as every `…At` below.
    public var lastAt: Int64?
    public var said: String?
    public var doing: String?
    public var lastAsk: String?
    public var firstAsk: String?
    public var startedAt: Int64?
    public var hookAt: Int64?
    public var writtenAt: Int64?
    /// The roster's 8-hex id of a background job (what `claude attach` takes); nil for an interactive session.
    public var jobId: String?
    public var title: String?
    public var epic: String?
    /// Archived and not started again nor needing you since: not shown.
    public var archived: Bool
    public var account: String?

    public init(
        key: SessionKey, engine: Engine, name: String?, cwd: String?, state: State, reason: String, live: Bool,
        pid: UInt32?, stage: String?, model: String?, effort: String?, contextTokens: UInt64?, lastAt: Int64?,
        said: String?, doing: String?, lastAsk: String?, firstAsk: String?, startedAt: Int64?, hookAt: Int64?,
        writtenAt: Int64?, jobId: String?, title: String?, epic: String?, archived: Bool, account: String?
    ) {
        self.key = key
        self.engine = engine
        self.name = name
        self.cwd = cwd
        self.state = state
        self.reason = reason
        self.live = live
        self.pid = pid
        self.stage = stage
        self.model = model
        self.effort = effort
        self.contextTokens = contextTokens
        self.lastAt = lastAt
        self.said = said
        self.doing = doing
        self.lastAsk = lastAsk
        self.firstAsk = firstAsk
        self.startedAt = startedAt
        self.hookAt = hookAt
        self.writtenAt = writtenAt
        self.jobId = jobId
        self.title = title
        self.epic = epic
        self.archived = archived
        self.account = account
    }
}

/// What a terminal attaches to.
public enum AttachTarget: Equatable, Sendable {
    case claude(profile: String, job: String)
    case codex(profile: String, sessionId: String, cwd: String)
    case trust(profile: String, dir: String)
}

public struct Delta: Equatable, Sendable {
    public var seq: UInt64
    public var upsert: [Session]
    public var remove: [SessionKey]

    public init(seq: UInt64, upsert: [Session], remove: [SessionKey]) {
        self.seq = seq
        self.upsert = upsert
        self.remove = remove
    }
}

/// One plan-limit window of an account, as last reported (a passed `resetsAt` reads as 0 used).
public struct Limit: Equatable, Sendable {
    public var window: String
    public var usedPct: UInt8
    public var resetsAt: Int64?

    public init(window: String, usedPct: UInt8, resetsAt: Int64?) {
        self.window = window
        self.usedPct = usedPct
        self.resetsAt = resetsAt
    }
}

public struct Account: Equatable, Sendable {
    public var name: String
    public var engine: Engine
    public var profile: String
    public var limits: [Limit]

    public init(name: String, engine: Engine, profile: String, limits: [Limit]) {
        self.name = name
        self.engine = engine
        self.profile = profile
        self.limits = limits
    }
}

public struct Health: Equatable, Sendable {
    public var cpuPct: UInt8
    public var memPct: UInt8
    public var diskPct: UInt8
    public var dev: UInt16
    public var db: UInt16

    public init(cpuPct: UInt8, memPct: UInt8, diskPct: UInt8, dev: UInt16, db: UInt16) {
        self.cpuPct = cpuPct
        self.memPct = memPct
        self.diskPct = diskPct
        self.dev = dev
        self.db = db
    }
}

/// The board besides its sessions: the epics (fleet.json order), the groups you made (sorted), accounts and health.
public struct Meta: Equatable, Sendable {
    public var epics: [String]
    public var groups: [String]
    public var accounts: [Account]
    public var health: Health?

    public init(epics: [String] = [], groups: [String] = [], accounts: [Account] = [], health: Health? = nil) {
        self.epics = epics
        self.groups = groups
        self.accounts = accounts
        self.health = health
    }
}

/// A frame of the socket, by its `wire.rs` variant index (0 Hello … 10 Meta).
public enum Frame: Equatable, Sendable {
    case hello(proto: UInt16, since: UInt64?, run: UInt64?, role: Role)
    case snapshot(run: UInt64, seq: UInt64, machine: String, sessions: [Session])
    case delta(Delta)
    case error(message: String)
    case termOpen(ref: UInt32, target: AttachTarget, cols: UInt16, rows: UInt16)
    case termIn(ref: UInt32, data: Data)
    case termSize(ref: UInt32, cols: UInt16, rows: UInt16)
    case termClose(ref: UInt32)
    case termOut(ref: UInt32, data: Data)
    case termEnd(ref: UInt32, error: String?)
    case meta(Meta)

    /// `wire.rs` `PROTO`.
    public static let proto: UInt16 = 4
    /// The most a `TermIn` carries.
    public static let termInMax = 64 * 1024
    /// The biggest frame body a reader accepts.
    public static let maxFrame: UInt32 = 64 * 1024 * 1024

    /// A frame's body (what follows the length), as `wire.rs` `decode`. Throws a `PostcardError` on anything that is
    /// not exactly one frame of this version.
    public static func decode(_ body: Data) throws -> Frame {
        var r = PostcardReader(body)
        let f = try r.value(Frame.self)
        try r.finish()
        return f
    }

    /// The frame as it goes on the wire, as `wire.rs` `encode`: a `u32` little-endian body length, then the body.
    public func encode() -> Data {
        let body = body()
        let n = UInt32(body.count)
        var out = Data(capacity: 4 + body.count)
        out.append(contentsOf: [UInt8(n & 0xff), UInt8(n >> 8 & 0xff), UInt8(n >> 16 & 0xff), UInt8(n >> 24)])
        out.append(contentsOf: body)
        return out
    }

    /// The postcard body alone.
    public func body() -> [UInt8] {
        var w = PostcardWriter()
        w.value(self)
        return w.bytes
    }
}

// MARK: - postcard forms, in declaration order

extension Postcard where Self: RawRepresentable, RawValue == UInt32 {
    public init(postcard r: inout PostcardReader) throws {
        let i = try r.variant()
        guard let v = Self(rawValue: i) else { throw PostcardError.badVariant(type: "\(Self.self)", index: i) }
        self = v
    }

    public func write(postcard w: inout PostcardWriter) { w.variant(rawValue) }
}

extension Engine: Postcard {}
extension State: Postcard {}
extension Role: Postcard {}

extension SessionKey: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(machine: try r.string(), profile: try r.string(), sessionId: try r.string())
    }

    public func write(postcard w: inout PostcardWriter) {
        w.string(machine)
        w.string(profile)
        w.string(sessionId)
    }
}

private func optString(_ r: inout PostcardReader) throws -> String? { try r.option { try $0.string() } }
private func optI64(_ r: inout PostcardReader) throws -> Int64? { try r.option { try $0.i64() } }

extension PostcardWriter {
    fileprivate mutating func optString(_ v: String?) { option(v) { $0.string($1) } }
    fileprivate mutating func optI64(_ v: Int64?) { option(v) { $0.i64($1) } }
}

extension Session: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        // Swift evaluates arguments left to right: this is the read order.
        self.init(
            key: try r.value(),
            engine: try r.value(),
            name: try optString(&r),
            cwd: try optString(&r),
            state: try r.value(),
            reason: try r.string(),
            live: try r.bool(),
            pid: try r.option { try $0.u32() },
            stage: try optString(&r),
            model: try optString(&r),
            effort: try optString(&r),
            contextTokens: try r.option { try $0.u64() },
            lastAt: try optI64(&r),
            said: try optString(&r),
            doing: try optString(&r),
            lastAsk: try optString(&r),
            firstAsk: try optString(&r),
            startedAt: try optI64(&r),
            hookAt: try optI64(&r),
            writtenAt: try optI64(&r),
            jobId: try optString(&r),
            title: try optString(&r),
            epic: try optString(&r),
            archived: try r.bool(),
            account: try optString(&r)
        )
    }

    public func write(postcard w: inout PostcardWriter) {
        w.value(key)
        w.value(engine)
        w.optString(name)
        w.optString(cwd)
        w.value(state)
        w.string(reason)
        w.bool(live)
        w.option(pid) { $0.u32($1) }
        w.optString(stage)
        w.optString(model)
        w.optString(effort)
        w.option(contextTokens) { $0.u64($1) }
        w.optI64(lastAt)
        w.optString(said)
        w.optString(doing)
        w.optString(lastAsk)
        w.optString(firstAsk)
        w.optI64(startedAt)
        w.optI64(hookAt)
        w.optI64(writtenAt)
        w.optString(jobId)
        w.optString(title)
        w.optString(epic)
        w.bool(archived)
        w.optString(account)
    }
}

extension AttachTarget: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        switch try r.variant() {
        case 0: self = .claude(profile: try r.string(), job: try r.string())
        case 1: self = .codex(profile: try r.string(), sessionId: try r.string(), cwd: try r.string())
        case 2: self = .trust(profile: try r.string(), dir: try r.string())
        case let i: throw PostcardError.badVariant(type: "AttachTarget", index: i)
        }
    }

    public func write(postcard w: inout PostcardWriter) {
        switch self {
        case let .claude(profile, job):
            w.variant(0)
            w.string(profile)
            w.string(job)
        case let .codex(profile, sessionId, cwd):
            w.variant(1)
            w.string(profile)
            w.string(sessionId)
            w.string(cwd)
        case let .trust(profile, dir):
            w.variant(2)
            w.string(profile)
            w.string(dir)
        }
    }
}

extension Delta: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(seq: try r.u64(), upsert: try r.vec { try $0.value() }, remove: try r.vec { try $0.value() })
    }

    public func write(postcard w: inout PostcardWriter) {
        w.u64(seq)
        w.vec(upsert) { $0.value($1) }
        w.vec(remove) { $0.value($1) }
    }
}

extension Limit: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(window: try r.string(), usedPct: try r.u8(), resetsAt: try optI64(&r))
    }

    public func write(postcard w: inout PostcardWriter) {
        w.string(window)
        w.u8(usedPct)
        w.optI64(resetsAt)
    }
}

extension Account: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(name: try r.string(), engine: try r.value(), profile: try r.string(), limits: try r.vec { try $0.value() })
    }

    public func write(postcard w: inout PostcardWriter) {
        w.string(name)
        w.value(engine)
        w.string(profile)
        w.vec(limits) { $0.value($1) }
    }
}

extension Health: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(cpuPct: try r.u8(), memPct: try r.u8(), diskPct: try r.u8(), dev: try r.u16(), db: try r.u16())
    }

    public func write(postcard w: inout PostcardWriter) {
        w.u8(cpuPct)
        w.u8(memPct)
        w.u8(diskPct)
        w.u16(dev)
        w.u16(db)
    }
}

extension Meta: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        self.init(
            epics: try r.vec { try $0.string() },
            groups: try r.vec { try $0.string() },
            accounts: try r.vec { try $0.value() },
            health: try r.option { try $0.value() }
        )
    }

    public func write(postcard w: inout PostcardWriter) {
        w.vec(epics) { $0.string($1) }
        w.vec(groups) { $0.string($1) }
        w.vec(accounts) { $0.value($1) }
        w.option(health) { $0.value($1) }
    }
}

extension Frame: Postcard {
    public init(postcard r: inout PostcardReader) throws {
        switch try r.variant() {
        case 0:
            self = .hello(
                proto: try r.u16(), since: try r.option { try $0.u64() }, run: try r.option { try $0.u64() },
                role: try r.value())
        case 1:
            self = .snapshot(run: try r.u64(), seq: try r.u64(), machine: try r.string(), sessions: try r.vec { try $0.value() })
        case 2: self = .delta(try r.value())
        case 3: self = .error(message: try r.string())
        case 4: self = .termOpen(ref: try r.u32(), target: try r.value(), cols: try r.u16(), rows: try r.u16())
        case 5: self = .termIn(ref: try r.u32(), data: Data(try r.bytesValue()))
        case 6: self = .termSize(ref: try r.u32(), cols: try r.u16(), rows: try r.u16())
        case 7: self = .termClose(ref: try r.u32())
        case 8: self = .termOut(ref: try r.u32(), data: Data(try r.bytesValue()))
        case 9: self = .termEnd(ref: try r.u32(), error: try optString(&r))
        case 10: self = .meta(try r.value())
        case let i: throw PostcardError.badVariant(type: "Frame", index: i)
        }
    }

    public func write(postcard w: inout PostcardWriter) {
        switch self {
        case let .hello(proto, since, run, role):
            w.variant(0)
            w.u16(proto)
            w.option(since) { $0.u64($1) }
            w.option(run) { $0.u64($1) }
            w.value(role)
        case let .snapshot(run, seq, machine, sessions):
            w.variant(1)
            w.u64(run)
            w.u64(seq)
            w.string(machine)
            w.vec(sessions) { $0.value($1) }
        case let .delta(d):
            w.variant(2)
            w.value(d)
        case let .error(message):
            w.variant(3)
            w.string(message)
        case let .termOpen(ref, target, cols, rows):
            w.variant(4)
            w.u32(ref)
            w.value(target)
            w.u16(cols)
            w.u16(rows)
        case let .termIn(ref, data):
            w.variant(5)
            w.u32(ref)
            w.bytesValue(data)
        case let .termSize(ref, cols, rows):
            w.variant(6)
            w.u32(ref)
            w.u16(cols)
            w.u16(rows)
        case let .termClose(ref):
            w.variant(7)
            w.u32(ref)
        case let .termOut(ref, data):
            w.variant(8)
            w.u32(ref)
            w.bytesValue(data)
        case let .termEnd(ref, error):
            w.variant(9)
            w.u32(ref)
            w.optString(error)
        case let .meta(m):
            w.variant(10)
            w.value(m)
        }
    }
}
