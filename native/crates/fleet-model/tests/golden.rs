//! The wire format's bytes, pinned. Postcard is positional and both ends of every other test are Rust, so nothing else
//! notices a reordered field. Each frame here is encoded and compared with its golden file under `tests/golden/` (hex,
//! whitespace ignored), and each golden file is decoded and compared with the frame. A change to any of these bytes is a
//! `PROTO` bump (`wire.rs`). To write new golden files after a bump: `FLEET_BLESS=1 cargo test -p fleet-model --test
//! golden`, then review the diff.

use std::path::PathBuf;

use fleet_model::types::{
    Account, AttachTarget, Delta, Engine, Health, Limit, Meta, Session, SessionKey, State,
};
use fleet_model::wire::{decode, encode, hello_proto, Frame, Role, PROTO};

fn key(sid: &str) -> SessionKey {
    SessionKey {
        machine: "fixture-mac".into(),
        profile: "default".into(),
        session_id: sid.into(),
    }
}

/// Every `Option` set.
fn full() -> Session {
    Session {
        key: key("sess-0001-fixture"),
        engine: Engine::Codex,
        name: Some("fixture one".into()),
        cwd: Some("~/work/fixture".into()),
        state: State::NeedsYou,
        reason: "waiting on you: PermissionRequest".into(),
        live: true,
        pid: Some(4242),
        stage: Some("review".into()),
        model: Some("claude-fixture-2".into()),
        effort: Some("high".into()),
        context_tokens: Some(31_200),
        last_at: Some(1_793_966_401_000),
        said: Some("The loader retries now.".into()),
        doing: Some("Bash".into()),
        last_ask: Some("Fix the loader".into()),
        first_ask: Some("Look at the fixture".into()),
        started_at: Some(1_793_900_000_000),
        hook_at: Some(1_793_966_400_500),
        written_at: Some(1_793_966_401_250),
        job_id: Some("a1b2c3d4".into()),
        title: Some("Fix the fixture loader".into()),
        epic: Some("Fixtures".into()),
        archived: true,
        account: Some("work".into()),
    }
}

/// Every `Option` empty.
fn bare() -> Session {
    Session {
        key: key("sess-0002-fixture"),
        engine: Engine::Muse,
        name: None,
        cwd: None,
        state: State::Ended,
        reason: "ended, reason unknown".into(),
        live: false,
        pid: None,
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: None,
        doing: None,
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
        job_id: None,
        title: None,
        epic: None,
        archived: false,
        account: None,
    }
}

fn frames() -> Vec<(&'static str, Frame)> {
    vec![
        (
            "snapshot",
            Frame::Snapshot {
                run: 0x0123_4567_89ab_cdef,
                seq: 300,
                machine: "fixture-mac".into(),
                sessions: vec![full(), bare()],
            },
        ),
        (
            "delta",
            Frame::Delta(Delta {
                seq: 301,
                upsert: vec![full()],
                remove: vec![key("sess-0002-fixture")],
            }),
        ),
        (
            "error",
            Frame::Error {
                message: "protocol 3 not supported; this is proto 4".into(),
            },
        ),
        (
            "hello",
            Frame::Hello {
                proto: PROTO,
                since: Some(300),
                run: Some(0x0123_4567_89ab_cdef),
                role: Role::Board,
            },
        ),
        (
            "hello-terminals",
            Frame::Hello {
                proto: PROTO,
                since: None,
                run: None,
                role: Role::Terminals,
            },
        ),
        (
            "term-open-claude",
            Frame::TermOpen {
                r#ref: 7,
                target: AttachTarget::Claude {
                    profile: "default".into(),
                    job: "a1b2c3d4".into(),
                },
                cols: 100,
                rows: 30,
            },
        ),
        (
            "term-open-codex",
            Frame::TermOpen {
                r#ref: 8,
                target: AttachTarget::Codex {
                    profile: "codex".into(),
                    session_id: "0192f3a4-5b6c-7d8e-9f01-23456789abcd".into(),
                    cwd: "/tmp/fleetd-fixture".into(),
                },
                cols: 120,
                rows: 40,
            },
        ),
        (
            "term-open-trust",
            Frame::TermOpen {
                r#ref: 9,
                target: AttachTarget::Trust {
                    profile: "default".into(),
                    dir: "/tmp/fleetd-fixture".into(),
                },
                cols: 80,
                rows: 24,
            },
        ),
        (
            "term-in",
            Frame::TermIn {
                r#ref: 7,
                data: b"ls\r".to_vec(),
            },
        ),
        (
            "term-size",
            Frame::TermSize {
                r#ref: 7,
                cols: 132,
                rows: 43,
            },
        ),
        (
            "meta",
            Frame::Meta(Meta {
                epics: vec!["Fixtures".into(), "Other work".into()],
                groups: vec!["Experiments".into()],
                accounts: vec![
                    Account {
                        name: "work".into(),
                        engine: Engine::Claude,
                        profile: "default".into(),
                        limits: vec![
                            Limit {
                                window: "5h".into(),
                                used_pct: 37,
                                resets_at: Some(1_793_970_000_000),
                            },
                            Limit {
                                window: "7d".into(),
                                used_pct: 100,
                                resets_at: None,
                            },
                        ],
                    },
                    Account {
                        name: "codex".into(),
                        engine: Engine::Codex,
                        profile: "codex".into(),
                        limits: vec![],
                    },
                ],
                health: Some(Health {
                    cpu_pct: 12,
                    mem_pct: 64,
                    disk_pct: 81,
                    dev: 3,
                    db: 1,
                }),
            }),
        ),
        (
            "meta-bare",
            Frame::Meta(Meta {
                epics: vec![],
                groups: vec![],
                accounts: vec![],
                health: None,
            }),
        ),
        ("term-close", Frame::TermClose { r#ref: 7 }),
        (
            "term-out",
            Frame::TermOut {
                r#ref: 7,
                data: b"\x1b[32mok\x1b[0m\r\n".to_vec(),
            },
        ),
        (
            "term-end",
            Frame::TermEnd {
                r#ref: 7,
                error: None,
            },
        ),
        (
            "term-end-error",
            Frame::TermEnd {
                r#ref: 8,
                error: Some("It is open in another Fleet terminal already.".into()),
            },
        ),
    ]
}

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/{name}.hex"))
}

fn hex(b: &[u8]) -> String {
    let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let lines: Vec<&str> = s
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    lines.join("\n") + "\n"
}

fn unhex(s: &str) -> Vec<u8> {
    let h: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    h.chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn the_frames_encode_to_their_golden_bytes_and_back() {
    let bless = std::env::var_os("FLEET_BLESS").is_some();
    for (name, frame) in frames() {
        let bytes = encode(&frame).unwrap();
        if bless {
            std::fs::create_dir_all(golden(name).parent().unwrap()).unwrap();
            std::fs::write(golden(name), hex(&bytes)).unwrap();
        }
        let want = unhex(
            &std::fs::read_to_string(golden(name))
                .unwrap_or_else(|e| panic!("{}: {e}", golden(name).display())),
        );
        assert_eq!(
            hex(&bytes),
            hex(&want),
            "{name}: the encoding moved: that is a PROTO bump"
        );
        let len = u32::from_le_bytes(want[..4].try_into().unwrap()) as usize;
        assert_eq!(len, want.len() - 4, "{name}: the length prefix");
        assert_eq!(decode(&want[4..]).unwrap(), frame, "{name}: decodes back");
    }
}

/// What never moves: a Hello of any version is variant 0 starting with `proto`, so a proto-1 Hello (no `run`) is still
/// read as far as its version.
#[test]
fn a_proto_1_hello_is_read_as_far_as_its_version() {
    // Proto 1's `Hello { proto: 1, since: Some(5) }`: variant 0, proto 1, Some, 5.
    let v1 = [0x00, 0x01, 0x01, 0x05];
    assert!(decode(&v1).is_err(), "not this version's shape");
    assert_eq!(hello_proto(&v1), Some(1));
    let now = encode(&frames()[3].1).unwrap();
    assert_eq!(hello_proto(&now[4..]), Some(PROTO));
    // An Error is variant 3 with one string, as it was in proto 1.
    assert_eq!(
        &encode(&Frame::Error {
            message: "ab".into()
        })
        .unwrap()[4..],
        &[0x03, 0x02, b'a', b'b']
    );
    assert_eq!(hello_proto(&[0x03, 0x02, b'a', b'b']), None);
}
