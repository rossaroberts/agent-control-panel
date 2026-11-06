//! The watcher and the store over a temp home (`FLEETD_HOME`) whose `claude` is a stand-in shell script: hook writes,
//! transcript and rollout appends and roster runs in, sequenced deltas out. One test at a time: the home is process-wide.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

use fleet_model::types::{Delta, Engine, Session, SessionKey, State};
use fleetd::store::Store;
use fleetd::watch::{watch, WatchHandle};
use serde_json::json;
use tokio::sync::broadcast::{error::RecvError, Receiver};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CWD: &str = "/tmp/fleetd-fixture";
const STARTED: i64 = 1_793_900_000_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn sid(n: usize) -> String {
    format!("sess-{n:04}-fixture")
}

/// A temp home with one profile (`~/.claude`) listing `n` interactive sessions, all run by this test's own (live) pid and
/// idle at their prompt, and a stand-in `claude` in `~/.local/bin` (where `CliRoster` looks first) that answers from
/// `stand-in/`: `fail` there makes it exit 1, `fail-running` makes only `agents --json` (what runs) exit 1, `slow` holds
/// the seconds it sleeps first, `calls.log` lists its calls.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new(n: usize) -> Home {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let p = root.join(".claude");
        for d in [
            "fleet-state",
            "fleet-status",
            "jobs",
            "projects",
            "../.local/bin",
            "../stand-in",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::create_dir_all(root.join(".codex/sessions/2026/11/06")).unwrap();
        fs::write(p.join("settings.json"), "{}").unwrap();
        let rows: Vec<_> = (0..n)
            .map(|i| {
                json!({"sessionId": sid(i), "pid": std::process::id(), "kind": "interactive", "status": "idle",
                       "name": format!("fixture {i}"), "cwd": CWD, "startedAt": STARTED})
            })
            .collect();
        let running: Vec<_> = (0..n).map(|i| json!({"sessionId": sid(i)})).collect();
        let s = root.join("stand-in");
        fs::write(s.join("all.json"), serde_json::to_string(&rows).unwrap()).unwrap();
        fs::write(
            s.join("running.json"),
            serde_json::to_string(&running).unwrap(),
        )
        .unwrap();
        let script = format!(
            "#!/bin/sh\nS='{}'\necho \"$*\" >> \"$S/calls.log\"\n[ -f \"$S/slow\" ] && sleep \"$(cat \"$S/slow\")\"\n\
             [ -f \"$S/fail\" ] && {{ echo 'stand-in: roster broke' >&2; exit 1; }}\n\
             [ -f \"$S/fail-running\" ] && [ \"$*\" = 'agents --json' ] && {{ echo 'stand-in: list broke' >&2; exit 1; }}\n\
             case \"$*\" in\n  'agents --json --all') cat \"$S/all.json\" ;;\n  'agents --json') cat \"$S/running.json\" ;;\n\
             \x20 *) echo \"stand-in: $*\" >&2; exit 1 ;;\nesac\n",
            s.display()
        );
        let bin = root.join(".local/bin/claude");
        fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("FLEETD_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("FLEETD_MACHINE", "fixture-mac");
        std::env::set_var("FLEETD_ROSTER", "cli");
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var(
            "PATH",
            format!("{}:{path}", root.join(".local/bin").display()),
        );
        Home { _dir: dir, root }
    }

    fn profile(&self) -> PathBuf {
        self.root.join(".claude")
    }

    fn stand_in(&self) -> PathBuf {
        self.root.join("stand-in")
    }

    /// A hook event, written as hook.sh writes it: a temp file renamed over `<session>.json`. Its `at`.
    fn hook(&self, n: usize, e: &str, t: Option<&str>) -> i64 {
        let at = now_ms();
        let d = self.profile().join("fleet-state");
        let tmp = d.join(format!(".{}.{}", sid(n), std::process::id()));
        let ev = json!({"e": e, "t": t, "n": null, "r": null, "i": null, "at": at});
        fs::write(&tmp, format!("{ev}\n")).unwrap();
        fs::rename(&tmp, d.join(format!("{}.json", sid(n)))).unwrap();
        at
    }

    fn transcript(&self, n: usize) -> PathBuf {
        let d = self.profile().join("projects").join(CWD.replace('/', "-"));
        fs::create_dir_all(&d).unwrap();
        d.join(format!("{}.jsonl", sid(n)))
    }

    fn calls(&self, what: &str) -> usize {
        fs::read_to_string(self.stand_in().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == what)
            .count()
    }

    fn start(&self) -> (Arc<Store>, WatchHandle, Receiver<Delta>) {
        let store = Arc::new(Store::new());
        let h = watch(&self.root, store.clone()).unwrap();
        let rx = store.subscribe();
        (store, h, rx)
    }
}

fn append(f: &Path, line: &serde_json::Value) {
    use std::io::Write;
    let mut o = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(f)
        .unwrap();
    writeln!(o, "{line}").unwrap();
}

fn assistant(text: &str) -> serde_json::Value {
    json!({"type":"assistant","timestamp":"2026-11-06T12:00:00.000Z",
           "message":{"model":"claude-fixture-2","stop_reason":"end_turn",
                      "content":[{"type":"text","text":text}],"usage":{"input_tokens":7}}})
}

/// The first delta (within `within`) with a session that passes `want`, and when it came.
async fn wait_for(
    rx: &mut Receiver<Delta>,
    within: Duration,
    want: impl Fn(&Session) -> bool,
) -> Option<(Session, Instant)> {
    let end = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(end, rx.recv()).await {
            Err(_) => return None,
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => return None,
            Ok(Ok(d)) => {
                if let Some(s) = d.upsert.into_iter().find(|s| want(s)) {
                    return Some((s, Instant::now()));
                }
            }
        }
    }
}

/// Every delta that arrives until none has for `quiet`.
async fn drain(rx: &mut Receiver<Delta>, quiet: Duration) -> Vec<Delta> {
    let mut out = vec![];
    while let Ok(r) = tokio::time::timeout(quiet, rx.recv()).await {
        match r {
            Ok(d) => out.push(d),
            Err(RecvError::Lagged(n)) => panic!("lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
    out
}

/// Latencies in ms, sorted.
struct Lat(Vec<f64>);

impl Lat {
    fn at(&self, q: f64) -> f64 {
        self.0[((self.0.len() as f64 * q).ceil() as usize).clamp(1, self.0.len()) - 1]
    }
    fn p50(&self) -> f64 {
        self.at(0.5)
    }
    fn p95(&self) -> f64 {
        self.at(0.95)
    }
    fn max(&self) -> f64 {
        self.0[self.0.len() - 1]
    }
}

/// One hook write's latency to its delta, `n` times `gap` apart, flipping between working and waiting.
async fn hook_latency(home: &Home, rx: &mut Receiver<Delta>, n: usize, gap: Duration) -> Lat {
    let mut ms = vec![];
    for i in 0..n {
        let (e, want) = if i % 2 == 0 {
            ("UserPromptSubmit", State::Working)
        } else {
            ("Stop", State::Waiting)
        };
        let t0 = Instant::now();
        let at = home.hook(0, e, None);
        let got = wait_for(rx, Duration::from_millis(500), |s| {
            s.key.session_id == sid(0) && s.hook_at == Some(at)
        })
        .await
        .unwrap_or_else(|| panic!("no delta for hook write {i}"));
        assert_eq!(got.0.state, want);
        ms.push(got.1.duration_since(t0).as_secs_f64() * 1000.0);
        tokio::time::sleep(gap).await;
    }
    ms.sort_by(f64::total_cmp);
    Lat(ms)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_write_is_a_delta_within_50ms() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (store, _h, mut rx) = home.start();
    let (seq, snap) = store.snapshot();
    assert_eq!(snap.len(), 1, "the first read is in before watch returns");
    assert!(seq >= 1);
    let s = &snap[0];
    assert_eq!(
        s.key,
        SessionKey {
            machine: "fixture-mac".into(),
            profile: "default".into(),
            session_id: sid(0)
        }
    );
    assert_eq!(
        (s.engine.clone(), s.state.clone(), s.live),
        (Engine::Claude, State::Waiting, true)
    );
    assert_eq!(s.reason, "idle at prompt");
    assert_eq!(s.name.as_deref(), Some("fixture 0"));

    let l = hook_latency(&home, &mut rx, 30, Duration::from_millis(20)).await;
    eprintln!(
        "hook write -> delta: p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms (30 writes)",
        l.p50(),
        l.p95(),
        l.max()
    );
    assert!(l.max() < 50.0, "max {:.2} ms", l.max());
    let (_, snap) = store.snapshot();
    assert_eq!(snap[0].reason, "idle at prompt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transcript_append_is_a_delta_with_the_new_said() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let f = home.transcript(0);
    append(
        &f,
        &json!({"type":"user","timestamp":"2026-11-06T11:59:00.000Z",
                       "message":{"role":"user","content":"Fix the fixture loader"}}),
    );
    let (store, h, mut rx) = home.start();
    #[cfg(target_os = "macos")]
    assert_eq!(
        h.followed(),
        1,
        "the live session's transcript is on the hot path"
    );
    #[cfg(not(target_os = "macos"))]
    let _ = h;
    let (_, snap) = store.snapshot();
    assert_eq!(snap[0].first_ask.as_deref(), Some("Fix the fixture loader"));
    assert_eq!(snap[0].said, None);

    let t0 = Instant::now();
    append(&f, &assistant("The loader retries now."));
    let (s, at) = wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.said.as_deref() == Some("The loader retries now.")
    })
    .await
    .expect("a delta with the new said");
    eprintln!(
        "transcript append -> delta: {:.2} ms",
        at.duration_since(t0).as_secs_f64() * 1000.0
    );
    assert_eq!(s.model.as_deref(), Some("claude-fixture-2"));
    assert!(s.written_at.is_some());
}

/// Review Focus 4: a burst coalesces to the final state per session, with no trailing debounce on a single write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_200_hook_writes_coalesces() {
    let _g = SERIAL.lock().await;
    let home = Home::new(10);
    let (_store, h, mut rx) = home.start();
    let before = hook_latency(&home, &mut rx, 20, Duration::from_millis(20))
        .await
        .p50();
    drain(&mut rx, Duration::from_millis(200)).await;
    let runs = h.roster_runs();

    let mut last_at = [0i64; 10];
    let mut writing = Duration::ZERO;
    let t0 = Instant::now();
    for i in 0..200 {
        let (n, round) = (i % 10, i / 10);
        let (e, t) = match round {
            19 => ("PreToolUse", Some("Bash")),
            r if r % 2 == 0 => ("UserPromptSubmit", None),
            _ => ("Stop", None),
        };
        let w = Instant::now();
        last_at[n] = home.hook(n, e, t);
        writing += w.elapsed();
        // 200 in one second.
        let due = t0 + Duration::from_micros(5000 * (i as u64 + 1));
        tokio::time::sleep_until(due.into()).await;
    }
    let took = t0.elapsed();
    assert!(
        took < Duration::from_millis(1100),
        "the writes took {took:?}"
    );
    let deltas = drain(&mut rx, Duration::from_millis(500)).await;
    eprintln!(
        "burst: 200 writes in {:.0} ms ({:.0} ms writing) -> {} deltas, {} roster runs",
        took.as_secs_f64() * 1000.0,
        writing.as_secs_f64() * 1000.0,
        deltas.len(),
        h.roster_runs() - runs
    );
    assert!(deltas.len() <= 220, "{} deltas", deltas.len());
    for (n, at) in last_at.iter().enumerate() {
        let last = deltas
            .iter()
            .flat_map(|d| &d.upsert)
            .rfind(|s| s.key.session_id == sid(n))
            .unwrap_or_else(|| panic!("no delta for session {n}"));
        assert_eq!(last.state, State::Working, "session {n}");
        assert_eq!(last.reason, "working: hook PreToolUse", "session {n}");
        assert_eq!(
            last.hook_at,
            Some(*at),
            "session {n} ends on its last write"
        );
    }

    let l = hook_latency(&home, &mut rx, 20, Duration::from_millis(20)).await;
    let (after, max) = (l.p50(), l.max());
    eprintln!(
        "single write p50 before the burst {before:.2} ms, after {after:.2} ms (max {max:.2})"
    );
    assert!(max < 50.0, "after the burst: max {max:.2} ms");
    assert!(
        after <= before * 1.5 + 2.0,
        "after the burst p50 {after:.2} ms, before {before:.2} ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconcile_of_an_unchanged_home_emits_nothing() {
    let _g = SERIAL.lock().await;
    let home = Home::new(3);
    append(&home.transcript(1), &assistant("Settled."));
    home.hook(2, "PreToolUse", Some("Read"));
    let (store, h, mut rx) = home.start();
    drain(&mut rx, Duration::from_millis(300)).await;
    let (seq, snap) = store.snapshot();
    let runs = h.roster_runs();
    h.reconcile();
    h.reconcile();
    let got = drain(&mut rx, Duration::from_millis(300)).await;
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(store.snapshot(), (seq, snap));
    assert_eq!(h.roster_runs(), runs, "the reconcile re-reads files only");
}

#[test]
fn since_reaches_back_4096_deltas() {
    let store = Store::new();
    let session = |said: String| Session {
        key: SessionKey {
            machine: "fixture-mac".into(),
            profile: "default".into(),
            session_id: sid(0),
        },
        engine: Engine::Claude,
        name: None,
        cwd: None,
        state: State::Working,
        reason: "working: hook PreToolUse".into(),
        live: true,
        pid: None,
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: Some(said),
        doing: None,
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
    };
    assert_eq!(store.since(0), Some(Delta::default()));
    for i in 0..5000 {
        assert_eq!(
            store.upsert(vec![session(format!("step {i}"))]),
            Some(i + 1)
        );
        assert_eq!(
            store.upsert(vec![session(format!("step {i}"))]),
            None,
            "unchanged: no delta"
        );
    }
    assert_eq!(store.seq(), 5000);
    assert_eq!(store.since(1), None, "older than the ring: send a snapshot");
    assert_eq!(store.since(5000 - 4096 - 1), None);
    let oldest = store
        .since(5000 - 4096)
        .expect("the ring's oldest is reachable");
    assert_eq!(oldest.upsert.len(), 1, "one session, as it is now");
    assert_eq!(oldest.upsert[0].said.as_deref(), Some("step 4999"));
    assert_eq!(
        store.since(5000),
        Some(Delta {
            seq: 5000,
            ..Delta::default()
        })
    );
    assert_eq!(store.since(5001), None, "ahead of this board");

    // A removal in scope comes through `since` as a removal.
    assert_eq!(
        store.replace(|s| s.engine == Engine::Claude, vec![]),
        Some(5001)
    );
    let d = store.since(4999).unwrap();
    assert!(d.upsert.is_empty());
    assert_eq!(d.remove.len(), 1);
    assert!(store.snapshot().1.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_roster_keeps_sessions_stale() {
    let _g = SERIAL.lock().await;
    let home = Home::new(2);
    let (store, _h, mut rx) = home.start();
    assert_eq!(store.snapshot().1.len(), 2);

    fs::write(home.stand_in().join("fail"), "").unwrap();
    let job = home.profile().join("jobs/0000abcd");
    fs::create_dir_all(&job).unwrap();
    fs::write(job.join("state.json"), r#"{"state":"running"}"#).unwrap();
    wait_for(&mut rx, Duration::from_secs(3), |s| {
        s.reason.starts_with("roster stale since ")
    })
    .await
    .expect("a stale delta");
    drain(&mut rx, Duration::from_millis(200)).await;
    let (_, snap) = store.snapshot();
    assert_eq!(snap.len(), 2, "sessions stay");
    let stale = regex_like_stale();
    for s in &snap {
        assert!(stale(&s.reason), "{}", s.reason);
        assert!(s.reason.ends_with("; idle at prompt"), "{}", s.reason);
        assert!(s.live);
    }

    // A hook still re-decides a session of a stale profile at once, from the rows held.
    let at = home.hook(0, "UserPromptSubmit", None);
    let (s, _) = wait_for(&mut rx, Duration::from_secs(1), |s| s.hook_at == Some(at))
        .await
        .unwrap();
    assert_eq!(s.state, State::Working);
    assert!(
        s.reason.ends_with("; working: hook UserPromptSubmit"),
        "{}",
        s.reason
    );

    // Back: the next good read drops the mark.
    fs::remove_file(home.stand_in().join("fail")).unwrap();
    fs::write(job.join("state.json"), r#"{"state":"done"}"#).unwrap();
    wait_for(&mut rx, Duration::from_secs(3), |s| {
        s.reason == "idle at prompt"
    })
    .await
    .expect("fresh again");
}

/// `roster stale since hh:mm; …`
fn regex_like_stale() -> impl Fn(&str) -> bool {
    |r: &str| {
        let Some(rest) = r.strip_prefix("roster stale since ") else {
            return false;
        };
        let b = rest.as_bytes();
        b.len() > 7
            && b[..2].iter().chain(&b[3..5]).all(u8::is_ascii_digit)
            && b[2] == b':'
            && &b[5..7] == b"; "
    }
}

/// With a slow roster, any number of jobs/ changes during a run make exactly one more run after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roster_runs_one_at_a_time_with_one_follow_up() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (_store, h, _rx) = home.start();
    // Events of the home's own making (just before the stream began) may still come in: let them settle.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (runs, calls) = (h.roster_runs(), home.calls("agents --json --all"));
    fs::write(home.stand_in().join("slow"), "1").unwrap();
    let job = |i: u32| {
        let d = home.profile().join(format!("jobs/{i:08x}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("state.json"), "{}").unwrap();
    };
    job(1);
    // The run has begun (the stand-in logs its call before it sleeps): ten more changes while it runs.
    let t0 = Instant::now();
    while home.calls("agents --json --all") == calls {
        assert!(t0.elapsed() < Duration::from_secs(2), "no run started");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    for i in 2..12 {
        job(i);
    }
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "the changes came during the run"
    );
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert_eq!(
        h.roster_runs() - runs,
        2,
        "one run, then exactly one follow-up"
    );
    assert_eq!(home.calls("agents --json --all") - calls, 2);
}

/// A new rollout of yours is followed from its first event, and an append is a delta with its new `said`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollout_append_is_a_delta() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (store, _h, mut rx) = home.start();
    let f = home
        .root
        .join(".codex/sessions/2026/11/06/rollout-2026-11-06T09-00-00-fixture.jsonl");
    let id = "00000000-0000-4000-8000-000000000001";
    let line = |t: &str, p: serde_json::Value| json!({"timestamp": "2026-11-06T09:00:02.000Z", "type": t, "payload": p});
    append(
        &f,
        &line(
            "session_meta",
            json!({"id": id, "timestamp": "2026-11-06T09:00:00.000Z", "cwd": "/work/fixture-app",
                                             "originator": "codex_cli_rs", "source": "cli"}),
        ),
    );
    append(&f, &line("event_msg", json!({"type": "task_started"})));
    append(
        &f,
        &line(
            "event_msg",
            json!({"type": "user_message", "message": "Rename the fixture key"}),
        ),
    );
    wait_for(&mut rx, Duration::from_secs(2), |s| s.key.session_id == id)
        .await
        .expect("the new rollout is followed");
    append(
        &f,
        &line(
            "event_msg",
            json!({"type": "agent_message", "message": "Renamed in two files."}),
        ),
    );
    let (s, _) = wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.key.session_id == id && s.said.as_deref() == Some("Renamed in two files.")
    })
    .await
    .expect("a delta with the new said");
    assert_eq!(
        (s.engine, s.key.profile.as_str(), s.key.machine.as_str()),
        (Engine::Codex, "codex", "fixture-mac")
    );
    assert!(s.live, "written just now");
    assert_eq!(store.snapshot().1.len(), 2);
}

/// Ruling R13: when the roster cannot say what runs, the last good running set stands. A background job with no process
/// is open only while it is listed as running; a failed "what runs" read must not close it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_running_set_keeps_the_last_good_one() {
    let _g = SERIAL.lock().await;
    let home = Home::new(0);
    let (job, sid) = ("0000beef", "sess-job-fixture");
    let s = home.stand_in();
    let row = json!([{"id": job, "sessionId": sid, "kind": "background", "state": "working", "name": "fixture job",
                      "cwd": CWD, "startedAt": STARTED}]);
    fs::write(s.join("all.json"), row.to_string()).unwrap();
    fs::write(
        s.join("running.json"),
        json!([{"sessionId": sid}]).to_string(),
    )
    .unwrap();
    let record = |state: &str| {
        let d = home.profile().join("jobs").join(job);
        fs::create_dir_all(&d).unwrap();
        let now = std::process::Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%S.000Z"])
            .output()
            .unwrap();
        let at = String::from_utf8(now.stdout).unwrap();
        fs::write(
            d.join("state.json"),
            json!({"state": state, "updatedAt": at.trim()}).to_string(),
        )
        .unwrap();
    };
    record("running");
    let (store, _h, mut rx) = home.start();
    let (_, snap) = store.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(
        (snap[0].state.clone(), snap[0].live),
        (State::Working, true),
        "{}",
        snap[0].reason
    );

    fs::write(s.join("fail-running"), "").unwrap();
    let calls = home.calls("agents --json");
    record("running"); // a jobs/ change: the roster is read again
    let t0 = Instant::now();
    while home.calls("agents --json") == calls {
        assert!(t0.elapsed() < Duration::from_secs(3), "no roster run");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drain(&mut rx, Duration::from_millis(500)).await;
    let (_, snap) = store.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(
        (snap[0].state.clone(), snap[0].live),
        (State::Working, true),
        "{}",
        snap[0].reason
    );
    assert_eq!(
        snap[0].reason, "working: roster working",
        "a good --all read is not stale"
    );
}

/// Ruling R14: a failed process scan keeps the last good open set and Muse list; a good one replaces them.
#[test]
fn a_failed_open_scan_keeps_the_last_good_set() {
    use fleetd::procscan::{MuseProc, Scan};
    use fleetd::sessions::Fleet;
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fleet::new("fixture-mac".into(), dir.path().join(".codex"));
    let held: std::collections::HashSet<PathBuf> =
        [dir.path().join(".codex/sessions/rollout-a.jsonl")].into();
    let muse = vec![MuseProc {
        pid: 4242,
        cwd: None,
        since: STARTED,
        args: "exec".into(),
    }];
    f.take_scan(Some(Scan {
        codex_open: held.clone(),
        muse: muse.clone(),
    }));
    f.take_scan(None);
    assert_eq!(
        (&f.codex.open, &f.muse),
        (&held, &muse),
        "failed scan: kept"
    );
    f.take_scan(Some(Scan::default()));
    assert!(
        f.codex.open.is_empty() && f.muse.is_empty(),
        "a good scan replaces it"
    );
}

/// A hook written while a slow roster run (1 s) is under way is a delta at once: the run holds no lock the hook needs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_during_a_slow_roster_run_is_not_held_up() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (_store, _h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    fs::write(home.stand_in().join("slow"), "1").unwrap();
    let calls = home.calls("agents --json --all");
    let job = home.profile().join("jobs/0000cafe");
    fs::create_dir_all(&job).unwrap();
    fs::write(job.join("state.json"), "{}").unwrap();
    let t0 = Instant::now();
    while home.calls("agents --json --all") == calls {
        assert!(t0.elapsed() < Duration::from_secs(2), "no run started");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let t1 = Instant::now();
    let at = home.hook(0, "UserPromptSubmit", None);
    let (s, got) = wait_for(&mut rx, Duration::from_millis(900), |s| {
        s.hook_at == Some(at)
    })
    .await
    .expect("a delta while the roster runs");
    let ms = got.duration_since(t1).as_secs_f64() * 1000.0;
    eprintln!("hook during a slow roster run -> delta: {ms:.2} ms");
    assert_eq!(s.state, State::Working);
    assert!(ms < 50.0, "{ms:.2} ms");
    assert!(
        home.calls("agents --json --all") == calls + 1,
        "the run was still under way"
    );
}

/// The hot path's budget (R15): hook write -> delta p95 under 5 ms over 200 single writes. Timing-sensitive: run with
/// `cargo test --release -p fleetd --test store -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn hook_to_delta_p95_under_5ms() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (_store, _h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    let l = hook_latency(&home, &mut rx, 200, Duration::from_millis(10)).await;
    eprintln!(
        "hook write -> delta over 200 writes: p50 {:.3} ms, p95 {:.3} ms, max {:.3} ms",
        l.p50(),
        l.p95(),
        l.max()
    );
    assert!(l.p95() < 5.0, "p95 {:.3} ms", l.p95());
}

/// G3: a session's context comes from its status-line snapshot when that is fresh (`contextOf`), else the transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_line_gives_the_context() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    append(&home.transcript(0), &assistant("Reading the loader."));
    let (store, _h, mut rx) = home.start();
    assert_eq!(
        store.snapshot().1[0].context_tokens,
        Some(7),
        "the transcript's"
    );
    let d = home.profile().join("fleet-status");
    // Written after the transcript's last line (12:00:00Z), so fresh.
    let snap = json!({"at": 1_793_966_401_000i64, "session_id": sid(0), "model": {"id": "claude-fixture-2"},
                      "context_window": {"context_window_size": 200000,
                                         "current_usage": {"input_tokens": 1000, "cache_creation_input_tokens": 200,
                                                           "cache_read_input_tokens": 30000}}});
    let t0 = Instant::now();
    fs::write(d.join(".tmp"), snap.to_string()).unwrap();
    fs::rename(d.join(".tmp"), d.join(format!("{}.json", sid(0)))).unwrap();
    let (_, got) = wait_for(&mut rx, Duration::from_secs(1), |s| {
        s.context_tokens == Some(31_200)
    })
    .await
    .expect("a delta with the status line's context");
    eprintln!(
        "status write -> delta: {:.2} ms",
        got.duration_since(t0).as_secs_f64() * 1000.0
    );
}

#[test]
fn helper_hold() {
    common::hold();
}

/// The gap: a running Muse (installed and signed in) is a session on the board, found by the scan at startup and by
/// the reconcile, and gone in a delta when its process ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_muse_is_a_session() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    fs::create_dir_all(home.root.join(".config/muse")).unwrap();
    fs::write(home.root.join(".config/muse/auth.json"), "{}").unwrap();
    let work = home.root.join("work");
    fs::create_dir_all(&work).unwrap();
    // The program is the one installed: `muse_installed` wants an executable `~/.local/bin/muse`.
    let first = common::Held::start(&home.root.join(".local/bin/muse"), &[], &work);
    let (store, h, mut rx) = home.start();
    let muse = |pid: u32| {
        move |s: &Session| s.engine == Engine::Muse && s.key.session_id == pid.to_string()
    };
    let snap = store.snapshot().1;
    let s = snap
        .iter()
        .find(|s| muse(first.pid())(s))
        .expect("found at startup");
    assert_eq!(
        (s.state.clone(), s.live, s.key.profile.as_str()),
        (State::Working, true, "muse")
    );
    assert_eq!(s.cwd.as_deref(), Some("~/work"));
    assert_eq!(
        s.doing.as_deref(),
        Some("--exact helper_hold --nocapture --test-threads=1")
    );

    let second = common::Held::start(&home.root.join("muse"), &[], &work);
    h.reconcile();
    wait_for(&mut rx, Duration::from_secs(1), muse(second.pid()))
        .await
        .expect("a new one found by the reconcile");
    let pid = first.pid();
    first.stop();
    h.reconcile();
    let end = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut gone = false;
    while let Ok(Ok(d)) = tokio::time::timeout_at(end, rx.recv()).await {
        if d.remove.iter().any(|k| k.session_id == pid.to_string()) {
            gone = true;
            break;
        }
    }
    assert!(gone, "a delta removes it");
    assert!(store.snapshot().1.iter().any(|s| muse(second.pid())(s)));
    second.stop();
}

/// `contextOf`'s choice: the status line when fresh, else the transcript; a 0 just after a restart defers to the
/// transcript; a percentage without a size uses the guessed window.
#[test]
fn context_prefers_a_fresh_status_line() {
    use fleet_read::statusfile::{context_tokens, StatusSnap};
    let sl = |at: i64, used: Option<u64>, pct: Option<f64>, window: Option<u64>| StatusSnap {
        at,
        model: None,
        window,
        used_tokens: used,
        used_pct: pct,
    };
    let t = Some(100_000);
    assert_eq!(
        context_tokens(
            Some(&sl(100_000, Some(5), None, None)),
            Some(100_000),
            Some(7),
            None
        ),
        Some(5)
    );
    assert_eq!(
        context_tokens(
            Some(&sl(10_000, Some(5), None, None)),
            Some(100_000),
            Some(7),
            None
        ),
        Some(7),
        "stale"
    );
    assert_eq!(
        context_tokens(Some(&sl(100_000, Some(0), None, None)), t, Some(7), None),
        Some(7),
        "0 after a restart"
    );
    assert_eq!(
        context_tokens(Some(&sl(100_000, Some(0), None, None)), t, None, None),
        Some(0)
    );
    assert_eq!(
        context_tokens(Some(&sl(100_000, None, Some(10.0), None)), t, None, None),
        Some(20_000)
    );
    assert_eq!(
        context_tokens(
            Some(&sl(100_000, None, Some(10.0), None)),
            t,
            None,
            Some("m[1m]")
        ),
        Some(100_000)
    );
    assert_eq!(
        context_tokens(
            Some(&sl(100_000, None, Some(10.0), Some(400_000))),
            t,
            None,
            None
        ),
        Some(40_000)
    );
    assert_eq!(context_tokens(None, t, Some(7), None), Some(7));
}
