//! Codex rollouts against Node's `readRollout`, `recentRollouts` + `yours` and `codexAccount` (golden from
//! native/parity/gen-codex-golden.mjs over the synthetic rollouts in native/parity/fixtures/codex): each read whole (its log
//! held by a process or not) and grown by chunks; the cap and the order of the recent list; the account's email; and the
//! states and reasons the Rust side adds (`Done`, `Ended · ended, reason unknown`).

use fleet_model::types::{Engine, Session, State};
use fleet_read::codex::{
    codex_account, codex_session_at, muse_installed_with, muse_session, recent_rollouts,
    RolloutTail,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const DAY: i64 = 86_400_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Golden {
    now_ms: i64,
    fixtures: Vec<Fixture>,
    recent: Recent,
    cap: Vec<String>,
    account: Account,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    rel: String,
    size: usize,
    mtime_ms: i64,
    cuts: [usize; 2],
    held: Value,
    not_held: Value,
    chunks: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recent {
    seven_days: Vec<String>,
    one_day: Vec<String>,
}

#[derive(Deserialize)]
struct Account {
    fixture: String,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity")
}

fn golden() -> Golden {
    serde_json::from_str(&fs::read_to_string(root().join("golden/codex.json")).expect("golden"))
        .expect("parse golden")
}

fn set_mtime(f: &Path, ms: i64) {
    let t = UNIX_EPOCH + Duration::from_millis(ms as u64);
    let file = OpenOptions::new().write(true).open(f).unwrap();
    file.set_times(FileTimes::new().set_modified(t)).unwrap();
}

fn append(f: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(f)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// A Codex home holding a copy of the fixture rollouts, each written at the time the golden used.
fn home_with_fixtures(g: &Golden) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for fx in &g.fixtures {
        let to = dir.path().join("sessions").join(&fx.rel);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(root().join("fixtures/codex/sessions").join(&fx.rel), &to).unwrap();
        set_mtime(&to, fx.mtime_ms);
    }
    dir
}

/// Node's row state word for a Session.
fn node_state(s: &Session) -> &'static str {
    if !s.live {
        return "closed";
    }
    match s.state {
        State::NeedsYou => "needs-you",
        State::Working => "working",
        State::Waiting => "ready",
        ref other => panic!("a live codex thread is never {other:?}"),
    }
}

/// The fields the golden holds, from a Session.
fn as_node(s: &Session) -> Value {
    json!({
        "id": s.key.session_id, "live": s.live, "state": node_state(s), "said": s.said, "doing": s.doing,
        "model": s.model, "effort": s.effort, "lastAt": s.last_at, "contextTokens": s.context_tokens,
        "lastAsk": s.last_ask, "firstAsk": s.first_ask, "startedAt": s.started_at, "cwd": s.cwd,
        "name": s.name, "writtenAt": s.written_at,
    })
}

fn assert_node(s: &Session, want: &Value, what: &str) {
    let got = as_node(s);
    if &got != want {
        let (g, w) = (got.as_object().unwrap(), want.as_object().unwrap());
        let diff: Vec<_> = w
            .iter()
            .filter(|(k, v)| g.get(*k) != Some(v))
            .map(|(k, v)| format!("{k}: rust {} node {v}", g[k]))
            .collect();
        panic!("{what}:\n  {}", diff.join("\n  "));
    }
}

#[test]
fn whole_reads_match_node() {
    let g = golden();
    assert!(g.fixtures.len() >= 15);
    assert!(
        g.fixtures.iter().any(|f| f.size > 640 * 1024),
        "one rollout bigger than the head and the tail together"
    );
    let home = home_with_fixtures(&g);
    for fx in &g.fixtures {
        let f = home.path().join("sessions").join(&fx.rel);
        assert_eq!(
            fs::metadata(&f).unwrap().len() as usize,
            fx.size,
            "{}",
            fx.rel
        );
        let s = codex_session_at(&f, true, g.now_ms);
        assert_node(&s, &fx.held, &format!("{} held open", fx.rel));
        let s = codex_session_at(&f, false, g.now_ms);
        assert_node(&s, &fx.not_held, &format!("{} not held", fx.rel));
    }
}

#[test]
fn chunked_reads_match_node() {
    let g = golden();
    let dir = tempfile::tempdir().unwrap();
    for fx in &g.fixtures {
        let buf = fs::read(root().join("fixtures/codex/sessions").join(&fx.rel)).unwrap();
        let file = dir.path().join("grow.jsonl");
        let bounds = [0, fx.cuts[0], fx.cuts[1], buf.len()];
        fs::write(&file, b"").unwrap();
        let mut tail: Option<RolloutTail> = None;
        for i in 0..3 {
            append(&file, &buf[bounds[i]..bounds[i + 1]]);
            set_mtime(&file, fx.mtime_ms);
            match tail.as_mut() {
                None => tail = Some(RolloutTail::open(&file).unwrap()),
                Some(t) => {
                    t.advance().unwrap();
                }
            }
            let s = tail.as_ref().unwrap().session(true, g.now_ms);
            // Ruling R9: a tail remembers all it read since it opened, where Node reads a fresh 384 KiB window each time.
            // Once the file is longer than the window the two may differ in what the window slid past (checked below).
            if bounds[i + 1] > 384 * 1024 {
                continue;
            }
            assert_node(
                &s,
                &fx.chunks[i],
                &format!("{} after chunk {}", fx.rel, i + 1),
            );
        }
        if buf.len() > 384 * 1024 {
            // the turn_context that Node's final window no longer holds is still known to a tail that read it as it came
            let s = tail.as_ref().unwrap().session(true, g.now_ms);
            assert_eq!(
                fx.chunks[2]["model"],
                Value::Null,
                "{}: the window forgot the model",
                fx.rel
            );
            assert_eq!(
                s.model.as_deref(),
                Some("fixture-codex-1"),
                "{}: R9",
                fx.rel
            );
        }
    }
}

#[test]
fn recent_list_matches_node() {
    let g = golden();
    let home = home_with_fixtures(&g);
    let rel = |v: Vec<PathBuf>| -> Vec<String> {
        v.iter()
            .map(|p| {
                p.strip_prefix(home.path().join("sessions"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    };
    let seven = rel(recent_rollouts(home.path(), 7 * DAY, g.now_ms));
    assert_eq!(seven, g.recent.seven_days);
    assert_eq!(
        rel(recent_rollouts(home.path(), DAY, g.now_ms)),
        g.recent.one_day
    );
    // Threads Codex started itself are not in it.
    assert!(!seven
        .iter()
        .any(|r| r.contains("subagent") || r.contains("review")));
    assert!(
        seven.iter().any(|r| r.contains("user-thread")),
        "thread_source user is yours"
    );
    assert!(
        seven.iter().any(|r| r.contains("working")),
        "thread_source absent is yours"
    );
}

#[test]
fn recent_list_is_capped_at_200_newest() {
    let g = golden();
    let dir = tempfile::tempdir().unwrap();
    let day = dir.path().join("sessions/2026/11/06");
    fs::create_dir_all(&day).unwrap();
    for i in 0..205 {
        let f = day.join(format!("rollout-cap-{i:03}.jsonl"));
        fs::write(
            &f,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n",
        )
        .unwrap();
        set_mtime(&f, g.now_ms - (i + 1) * 60_000);
    }
    let got: Vec<String> = recent_rollouts(dir.path(), 7 * DAY, g.now_ms)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(got.len(), 200);
    assert_eq!(got, g.cap);
}

#[test]
fn account_email_is_read_from_the_id_token() {
    let g = golden();
    let fixtures = root().join("fixtures/codex");
    assert_eq!(
        codex_account(&fixtures).as_deref(),
        Some(g.account.fixture.as_str())
    );
    assert_eq!(g.account.fixture, "fixture@example.test");

    let write = |body: &str| {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("auth.json"), body).unwrap();
        d
    };
    // an API key alone, a token that is not a JWT, a payload that is not JSON, no email in it, no file at all
    assert_eq!(
        codex_account(write(r#"{"OPENAI_API_KEY":"sk-fixture"}"#).path()),
        None
    );
    assert_eq!(
        codex_account(write(r#"{"tokens":{"id_token":"nodots"}}"#).path()),
        None
    );
    assert_eq!(
        codex_account(write(r#"{"tokens":{"id_token":"a.!!!.c"}}"#).path()),
        None
    );
    assert_eq!(
        codex_account(write(r#"{"tokens":{"id_token":"a.e30.c"}}"#).path()),
        None
    );
    assert_eq!(codex_account(write("not json").path()), None);
    assert_eq!(codex_account(tempfile::tempdir().unwrap().path()), None);
    // padded and standard-alphabet base64 are read as Node reads them: {"email":"p@example.test"}
    assert_eq!(
        codex_account(
            write(r#"{"tokens":{"id_token":"h.eyJlbWFpbCI6InBAZXhhbXBsZS50ZXN0In0=.s"}}"#).path()
        )
        .as_deref(),
        Some("p@example.test")
    );
}

fn session_of(g: &Golden, name: &str, open: bool) -> Session {
    let fx = g
        .fixtures
        .iter()
        .find(|f| f.rel.ends_with(&format!("-{name}.jsonl")))
        .unwrap();
    let home = home_with_fixtures(g);
    codex_session_at(&home.path().join("sessions").join(&fx.rel), open, g.now_ms)
}

#[test]
fn states_and_reasons() {
    let g = golden();
    let pick = |name: &str, open: bool| {
        let s = session_of(&g, name, open);
        (s.state.clone(), s.live, s.reason.clone())
    };
    assert_eq!(
        pick("working", true),
        (State::Working, true, "working: user_message".into())
    );
    assert_eq!(
        pick("asking", false),
        (
            State::NeedsYou,
            true,
            "waiting on you: request_user_input".into()
        )
    );
    assert_eq!(
        pick("aborted", true),
        (State::Waiting, true, "idle at prompt".into())
    );
    // closed, and its last record says the turn finished
    assert_eq!(
        pick("finished", false),
        (State::Done, false, "finished".into())
    );
    assert_eq!(pick("exec", false), (State::Done, false, "finished".into()));
    // closed with no record of why
    assert_eq!(
        pick("aborted", false),
        (State::Ended, false, "ended, reason unknown".into())
    );
    assert!(
        pick("working", false).1,
        "written a minute ago: open without a process"
    );
    assert_eq!(pick("old", false), (State::Done, false, "finished".into()));
    assert_eq!(
        pick("asking-stale", false),
        (State::Ended, false, "ended, reason unknown".into())
    );
    assert_eq!(
        pick("break", false),
        (State::Ended, false, "ended, reason unknown".into())
    );
    assert_eq!(pick("no-meta", false).0, State::Done);

    let s = session_of(&g, "working", true);
    assert_eq!(s.key.profile, "codex");
    assert_eq!(s.key.machine, "", "the store fills the machine in");
    assert_eq!(s.key.session_id, "00000000-0000-4000-8000-000000000001");
    assert_eq!(s.engine, Engine::Codex);
    assert_eq!(s.name.as_deref(), Some("codex_cli_rs"));
    assert_eq!(s.cwd.as_deref(), Some("/work/fixture-app"));
    assert_eq!(s.pid, None);
    // no id in the first line: the file's name
    let s = session_of(&g, "no-meta", true);
    assert_eq!(s.key.session_id, "rollout-2026-11-04T09-00-00-no-meta");
}

#[test]
fn an_append_reads_only_the_new_bytes() {
    let g = golden();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("r.jsonl");
    let first = fs::read(
        root()
            .join("fixtures/codex/sessions")
            .join(&g.fixtures[0].rel),
    )
    .unwrap();
    fs::write(&file, &first).unwrap();
    let mut t = RolloutTail::open(&file).unwrap();
    let before = t.bytes_read();
    assert_eq!(before, first.len() as u64);
    assert!(!t.advance().unwrap(), "nothing new");
    assert_eq!(t.bytes_read(), before);

    let more = b"{\"timestamp\":\"2026-11-06T10:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Appended words.\"}}\n";
    append(&file, more);
    assert!(t.advance().unwrap());
    assert_eq!(t.bytes_read(), before + more.len() as u64);
    assert_eq!(
        t.session(true, g.now_ms).said.as_deref(),
        Some("Appended words.")
    );
}

#[test]
fn a_partial_line_waits_for_its_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("r.jsonl");
    fs::write(
        &file,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"t1\"}}\n",
    )
    .unwrap();
    let mut t = RolloutTail::open(&file).unwrap();
    let line = "{\"timestamp\":\"2026-11-06T10:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Whole json.\"}}";
    append(&file, line.as_bytes());
    t.advance().unwrap();
    assert_eq!(t.session(true, 0).said, None, "not read until its newline");
    append(&file, b"\n");
    t.advance().unwrap();
    assert_eq!(t.session(true, 0).said.as_deref(), Some("Whole json."));
}

#[test]
fn a_replaced_or_truncated_rollout_is_read_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("r.jsonl");
    let say = |id: &str, t: &str| {
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n{{\"timestamp\":\"2026-11-06T10:00:00.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\",\"message\":\"{t}\"}}}}\n")
    };
    fs::write(&file, say("a", "first words, long enough to be longer")).unwrap();
    let mut t = RolloutTail::open(&file).unwrap();
    assert_eq!(
        t.session(true, 0).said.as_deref(),
        Some("first words, long enough to be longer")
    );
    // the same inode, shorter
    File::create(&file)
        .unwrap()
        .write_all(say("b", "short").as_bytes())
        .unwrap();
    t.advance().unwrap();
    let s = t.session(true, 0);
    assert_eq!(
        (s.key.session_id.as_str(), s.said.as_deref()),
        ("b", Some("short"))
    );
    // another inode
    let other = dir.path().join("other.jsonl");
    fs::write(
        &other,
        say("c", "replaced words, now much longer than before"),
    )
    .unwrap();
    fs::rename(&other, &file).unwrap();
    t.advance().unwrap();
    let s = t.session(true, 0);
    assert_eq!(
        (s.key.session_id.as_str(), s.said.as_deref()),
        ("c", Some("replaced words, now much longer than before"))
    );
}

#[test]
fn muse_is_where_its_cli_is_installed_and_signed_in() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    let dirs = [bin.clone()];
    assert!(!muse_installed_with(home.path(), &dirs));
    fs::create_dir_all(home.path().join(".config/muse")).unwrap();
    fs::write(home.path().join(".config/muse/auth.json"), "{}").unwrap();
    assert!(
        !muse_installed_with(home.path(), &dirs),
        "signed in, but no muse"
    );
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("muse"), "#!/bin/sh\n").unwrap();
    assert!(!muse_installed_with(home.path(), &dirs), "not executable");
    fs::set_permissions(bin.join("muse"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(muse_installed_with(home.path(), &dirs));
    fs::remove_file(home.path().join(".config/muse/auth.json")).unwrap();
    assert!(!muse_installed_with(home.path(), &dirs), "not signed in");
}

#[test]
fn a_running_muse_is_a_working_session_of_its_pid() {
    let s = muse_session(
        4242,
        Some("/work/fixture-app"),
        1_793_955_000_000,
        "exec --yolo",
    );
    assert_eq!(s.engine, Engine::Muse);
    assert_eq!(
        (s.key.profile.as_str(), s.key.session_id.as_str()),
        ("muse", "4242")
    );
    assert_eq!(
        (s.state.clone(), s.live, s.pid),
        (State::Working, true, Some(4242))
    );
    assert_eq!(s.name.as_deref(), Some("muse"));
    assert_eq!(s.doing.as_deref(), Some("exec --yolo"));
    assert_eq!(
        (s.started_at, s.last_at),
        (Some(1_793_955_000_000), Some(1_793_955_000_000))
    );
    assert_eq!(s.reason, "working: running process");
}

/// `RolloutView::due` is when the clock alone may change a Codex session: over every fixture, held open or not, the
/// state, liveness and reason are the same at every time from `now` up to (not including) `due`, and with no `due` at
/// every time at all.
#[test]
fn a_rollout_holds_still_until_due() {
    let g = golden();
    let home = home_with_fixtures(&g);
    let (min, day) = (60_000i64, 86_400_000i64);
    let mut armed = 0;
    for fx in &g.fixtures {
        let v = RolloutTail::open(&home.path().join("sessions").join(&fx.rel))
            .unwrap()
            .view();
        for open in [true, false] {
            let now = g.now_ms;
            let d = v.due(open, now);
            assert!(d.is_none_or(|d| d > now));
            armed += d.is_some() as usize;
            let at = |t: i64| {
                let s = v.session(open, t);
                (s.state, s.live, s.reason)
            };
            let w = v.written_at().unwrap_or(0);
            let mut times = vec![now + 1, now + 1_000 * day];
            for off in [10 * min, 3 * day] {
                times.extend([w + off - 1, w + off, w + off + 1]);
            }
            for t in times.into_iter().filter(|t| *t >= now) {
                if d.is_none_or(|d| t < d) {
                    assert_eq!(
                        at(t),
                        at(now),
                        "{} open {open}: changed at {t} before {d:?}",
                        fx.rel
                    );
                }
            }
            if let Some(d) = d {
                assert_ne!(at(d), at(now), "{} open {open}: armed for nothing", fx.rel);
            }
        }
    }
    assert!(armed > 0, "some fixture is live by age");
}
