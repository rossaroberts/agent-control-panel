// The wire's bytes, as fleetd sends them. Every golden frame of `native/crates/fleet-model/tests/golden/` decodes to the
// value `tests/golden.rs` builds it from (`full()`, `bare()`, `frames()`, reproduced here field for field), and encodes
// back to the same bytes. Postcard is positional: a field or variant out of order on this side shows up here first.

import Foundation
import Testing

@testable import FleetKit

private func key(_ sid: String) -> SessionKey {
    SessionKey(machine: "fixture-mac", profile: "default", sessionId: sid)
}

/// `golden.rs` `full()`: every `Option` set.
private func full() -> Session {
    Session(
        key: key("sess-0001-fixture"),
        engine: .codex,
        name: "fixture one",
        cwd: "~/work/fixture",
        state: .needsYou,
        reason: "waiting on you: PermissionRequest",
        live: true,
        pid: 4242,
        stage: "review",
        model: "claude-fixture-2",
        effort: "high",
        contextTokens: 31_200,
        lastAt: 1_793_966_401_000,
        said: "The loader retries now.",
        doing: "Bash",
        lastAsk: "Fix the loader",
        firstAsk: "Look at the fixture",
        startedAt: 1_793_900_000_000,
        hookAt: 1_793_966_400_500,
        writtenAt: 1_793_966_401_250,
        jobId: "a1b2c3d4",
        title: "Fix the fixture loader",
        epic: "Fixtures",
        archived: true,
        account: "work"
    )
}

/// `golden.rs` `bare()`: every `Option` empty.
private func bare() -> Session {
    Session(
        key: key("sess-0002-fixture"),
        engine: .muse,
        name: nil,
        cwd: nil,
        state: .ended,
        reason: "ended, reason unknown",
        live: false,
        pid: nil,
        stage: nil,
        model: nil,
        effort: nil,
        contextTokens: nil,
        lastAt: nil,
        said: nil,
        doing: nil,
        lastAsk: nil,
        firstAsk: nil,
        startedAt: nil,
        hookAt: nil,
        writtenAt: nil,
        jobId: nil,
        title: nil,
        epic: nil,
        archived: false,
        account: nil
    )
}

/// `golden.rs` `frames()`, in its order. `client` marks the frames a client sends (Hello, TermOpen/In/Size/Close).
private let frames: [(name: String, frame: Frame, client: Bool)] = [
    ("snapshot", .snapshot(run: 0x0123_4567_89ab_cdef, seq: 300, machine: "fixture-mac", sessions: [full(), bare()]), false),
    ("delta", .delta(Delta(seq: 301, upsert: [full()], remove: [key("sess-0002-fixture")])), false),
    ("error", .error(message: "protocol 3 not supported; this is proto 4"), false),
    ("hello", .hello(proto: Frame.proto, since: 300, run: 0x0123_4567_89ab_cdef, role: .board), true),
    ("hello-terminals", .hello(proto: Frame.proto, since: nil, run: nil, role: .terminals), true),
    ("term-open-claude", .termOpen(ref: 7, target: .claude(profile: "default", job: "a1b2c3d4"), cols: 100, rows: 30), true),
    (
        "term-open-codex",
        .termOpen(
            ref: 8,
            target: .codex(profile: "codex", sessionId: "0192f3a4-5b6c-7d8e-9f01-23456789abcd", cwd: "/tmp/fleetd-fixture"),
            cols: 120, rows: 40),
        true
    ),
    ("term-open-trust", .termOpen(ref: 9, target: .trust(profile: "default", dir: "/tmp/fleetd-fixture"), cols: 80, rows: 24), true),
    ("term-in", .termIn(ref: 7, data: Data("ls\r".utf8)), true),
    ("term-size", .termSize(ref: 7, cols: 132, rows: 43), true),
    (
        "meta",
        .meta(
            Meta(
                epics: ["Fixtures", "Other work"],
                groups: ["Experiments"],
                accounts: [
                    Account(
                        name: "work", engine: .claude, profile: "default",
                        limits: [
                            Limit(window: "5h", usedPct: 37, resetsAt: 1_793_970_000_000),
                            Limit(window: "7d", usedPct: 100, resetsAt: nil),
                        ]),
                    Account(name: "codex", engine: .codex, profile: "codex", limits: []),
                ],
                health: Health(cpuPct: 12, memPct: 64, diskPct: 81, dev: 3, db: 1))),
        false
    ),
    ("meta-bare", .meta(Meta(epics: [], groups: [], accounts: [], health: nil)), false),
    ("term-close", .termClose(ref: 7), true),
    ("term-out", .termOut(ref: 7, data: Data("\u{1b}[32mok\u{1b}[0m\r\n".utf8)), false),
    ("term-end", .termEnd(ref: 7, error: nil), false),
    ("term-end-error", .termEnd(ref: 8, error: "It is open in another Fleet terminal already."), false),
]

/// `native/crates/fleet-model/tests/golden/`, found from this file (`native/app/Tests/FleetKitTests/`).
private let goldenDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../../../crates/fleet-model/tests/golden")
    .standardizedFileURL

private func golden(_ name: String) throws -> Data {
    let text = try String(contentsOf: goldenDir.appendingPathComponent("\(name).hex"), encoding: .utf8)
    let digits = text.filter { !$0.isWhitespace }.map { UInt8(String($0), radix: 16)! }
    return Data(stride(from: 0, to: digits.count, by: 2).map { digits[$0] << 4 | digits[$0 + 1] })
}

private func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

@Suite struct GoldenTests {
    @Test func everyGoldenFileIsCovered() throws {
        let files = try FileManager.default.contentsOfDirectory(atPath: goldenDir.path)
            .filter { $0.hasSuffix(".hex") }.map { String($0.dropLast(4)) }
        #expect(Set(files) == Set(frames.map(\.name)), "a golden file without a Swift case, or the reverse")
        #expect(frames.count == 16)
    }

    @Test(arguments: frames.map(\.name))
    func goldenDecodesToItsValue(name: String) throws {
        let (_, want, _) = frames.first { $0.name == name }!
        let bytes = try golden(name)
        let len = bytes.prefix(4).enumerated().reduce(0) { $0 | Int($1.element) << (8 * $1.offset) }
        #expect(len == bytes.count - 4, "\(name): the length prefix")
        #expect(try Frame.decode(bytes.dropFirst(4)) == want, "\(name): decodes to golden.rs's value")
    }

    /// Client frames must encode to the golden bytes (fleetd reads them); the server frames are checked too, since a
    /// board cache (Task 5) writes them with the same encoder.
    @Test(arguments: frames.map(\.name))
    func valueEncodesToItsGolden(name: String) throws {
        let (_, frame, client) = frames.first { $0.name == name }!
        #expect(hex(frame.encode()) == hex(try golden(name)), "\(name)\(client ? " (client frame)" : ""): encoding moved")
    }

    @Test func clientFramesAreTheOnesTheBriefNames() {
        #expect(frames.filter(\.client).map(\.name) == [
            "hello", "hello-terminals", "term-open-claude", "term-open-codex", "term-open-trust", "term-in", "term-size",
            "term-close",
        ])
    }

    /// `golden.rs` `a_proto_1_hello_is_read_as_far_as_its_version`: an old Hello is not this version's shape, and an
    /// Error is variant 3 with one string.
    @Test func oldHelloDoesNotDecodeAndErrorKeepsItsShape() throws {
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x00, 0x01, 0x01, 0x05])) }
        #expect(Frame.error(message: "ab").encode().dropFirst(4) == Data([0x03, 0x02, 0x61, 0x62]))
    }

    @Test func malformedBodiesThrow() throws {
        let snap = try golden("snapshot").dropFirst(4)
        #expect(throws: PostcardError.self) { try Frame.decode(snap.dropLast()) }  // truncated
        #expect(throws: PostcardError.self) { try Frame.decode(snap + [0x00]) }  // trailing byte
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x0b])) }  // no variant 11
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x09, 0x07, 0x02])) }  // Option tag 2
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x03, 0x02, 0xff, 0xfe])) }  // not UTF-8
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x07, 0xff, 0xff, 0xff, 0xff, 0x7f])) }  // u32 overflow
        #expect(throws: PostcardError.self) { try Frame.decode(Data([0x03, 0xff, 0xff, 0xff, 0xff, 0x0f])) }  // huge length
    }

    @Test func varintsAndZigzagRoundTrip() throws {
        var w = PostcardWriter()
        let unsigned: [UInt64] = [0, 1, 127, 128, 300, UInt64(UInt32.max), UInt64.max]
        let signed: [Int64] = [0, -1, 1, -64, 64, 1_793_966_401_000, -1_793_966_401_000, Int64.min, Int64.max]
        for v in unsigned { w.u64(v) }
        for v in signed { w.i64(v) }
        var r = PostcardReader(w.bytes)
        for v in unsigned { #expect(try r.u64() == v) }
        for v in signed { #expect(try r.i64() == v) }
        try r.finish()
        // Postcard's own examples: 300 is ac 02, and -1 zigzags to 1.
        var w2 = PostcardWriter()
        w2.u64(300)
        w2.i64(-1)
        #expect(w2.bytes == [0xac, 0x02, 0x01])
    }
}
