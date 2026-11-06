//! The watcher and the store over a temp home (`FLEETD_HOME`) whose `claude` is a stand-in shell script: hook writes,
//! transcript and rollout appends and roster runs in, sequenced deltas out. One test at a time: the home is process-wide.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

use fleet_model::types::{Delta, Engine, Limit, Session, SessionKey, State};
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
        fs::create_dir_all(root.join("panel")).unwrap();
        std::env::set_var("FLEETD_PANEL_DIR", root.join("panel"));
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

    /// The Node panel's folder (`FLEETD_PANEL_DIR`).
    fn panel(&self) -> PathBuf {
        self.root.join("panel")
    }

    /// A panel file, written as the panel writes them: a temp file renamed over it.
    fn panel_file(&self, name: &str, body: &str) {
        let d = self.panel();
        fs::write(d.join(".tmp"), body).unwrap();
        fs::rename(d.join(".tmp"), d.join(name)).unwrap();
    }

    /// `~/.claude.json`: the account the default profile is logged in as.
    fn login(&self, email: &str) {
        fs::write(
            self.root.join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": email}}).to_string(),
        )
        .unwrap();
    }

    /// A status-line snapshot of session `n` (written `at`), with these plan-limit windows: (used %, resets in s).
    fn status_limits(&self, n: usize, at: i64, five: (f64, i64), seven: (f64, i64)) {
        let d = self.profile().join("fleet-status");
        let snap = json!({"at": at, "session_id": sid(n),
            "rate_limits": {"five_hour": {"used_percentage": five.0, "resets_at": five.1},
                            "seven_day": {"used_percentage": seven.0, "resets_at": seven.1}}});
        fs::write(d.join(".tmp"), snap.to_string()).unwrap();
        fs::rename(d.join(".tmp"), d.join(format!("{}.json", sid(n)))).unwrap();
    }

    fn stand_in(&self) -> PathBuf {
        self.root.join("stand-in")
    }

    /// A hook event, written as hook.sh writes it: a temp file renamed over `<session>.json`. Its `at`.
    fn hook(&self, n: usize, e: &str, t: Option<&str>) -> i64 {
        self.hook_at(n, e, t, now_ms())
    }

    /// `hook`, saying it was written at `at` (a hook written a while ago, without the wait).
    fn hook_at(&self, n: usize, e: &str, t: Option<&str>, at: i64) -> i64 {
        let d = self.profile().join("fleet-state");
        let tmp = d.join(format!(".{}.{}", sid(n), std::process::id()));
        let ev = json!({"e": e, "t": t, "n": null, "r": null, "i": null, "at": at});
        fs::write(&tmp, format!("{ev}\n")).unwrap();
        fs::rename(&tmp, d.join(format!("{}.json", sid(n)))).unwrap();
        at
    }

    /// What the stand-in roster says from now on: one interactive session `sid(0)` with `pid` and `status`, listed as
    /// running when `running`.
    fn roster(&self, pid: u32, status: &str, running: bool) {
        let s = self.stand_in();
        let rows = json!([{"sessionId": sid(0), "pid": pid, "kind": "interactive", "status": status,
                           "name": "fixture 0", "cwd": CWD, "startedAt": STARTED}]);
        let run = if running {
            json!([{"sessionId": sid(0)}])
        } else {
            json!([])
        };
        // Renamed into place, so a run never reads half a file.
        for (name, v) in [("all.json", rows), ("running.json", run)] {
            fs::write(s.join(".tmp"), v.to_string()).unwrap();
            fs::rename(s.join(".tmp"), s.join(name)).unwrap();
        }
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
        job_id: None,
        title: None,
        epic: None,
        archived: false,
        account: None,
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_scan_answers_its_ticket_and_only_a_later_scan_does() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (_store, h, _rx) = home.start();
    let runs = h.scan_runs();
    let ticket = h.request_scan();
    assert!(
        h.wait_scan(ticket, Duration::from_secs(10)),
        "the scan it asked for is done"
    );
    assert!(h.scan_runs() > runs);
    assert!(
        !h.wait_scan(ticket + 1, Duration::from_millis(100)),
        "a ticket no scan has begun for is not answered"
    );
    let next = h.request_scan();
    assert!(next > ticket);
    assert!(h.wait_scan(next, Duration::from_secs(10)));
}

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
        at: Some(at),
        model: None,
        window,
        used_tokens: used,
        used_pct: pct,
        limits: None,
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
    // P1: no `at` is never fresh (Node: `undefined >= x` is false), even with no transcript time.
    let no_at = StatusSnap {
        at: None,
        ..sl(0, Some(5), None, None)
    };
    assert_eq!(context_tokens(Some(&no_at), None, Some(7), None), Some(7));
    assert_eq!(context_tokens(Some(&no_at), None, None, None), None);
}

/// NB2: a tail whose lock is held by a slow read (500 ms) does not hold up a hook delta for its own session: the board
/// never waits on a tail's lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_is_not_held_up_by_a_busy_tail() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    append(&home.transcript(0), &assistant("Reading the loader."));
    let (_store, h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    assert!(h.hold_tail("default", &sid(0), Duration::from_millis(500)));
    let t0 = Instant::now();
    let at = home.hook(0, "UserPromptSubmit", None);
    let (s, got) = wait_for(&mut rx, Duration::from_secs(2), |s| s.hook_at == Some(at))
        .await
        .expect("a hook delta");
    let ms = got.duration_since(t0).as_secs_f64() * 1000.0;
    eprintln!("hook beside a busy tail -> delta: {ms:.2} ms");
    assert_eq!(s.state, State::Working);
    assert_eq!(
        s.said.as_deref(),
        Some("Reading the loader."),
        "the tail's last facts still"
    );
    assert!(ms < 50.0, "{ms:.2} ms");
}

/// NB1 (c): a hook file that cannot be read (here: too many open files) keeps the hook held; only a file that is gone
/// clears it.
#[test]
fn a_failed_hook_read_keeps_the_last_hook() {
    use fleet_model::decide::RosterRow;
    use fleet_read::profiles::Profile;
    use fleet_read::roster::RosterRead;
    use fleetd::sessions::{read_hook_file, read_hook_file_with, Fleet, RosterGot};
    let dir = tempfile::tempdir().unwrap();
    let pdir = dir.path().join(".claude");
    fs::create_dir_all(pdir.join("fleet-state")).unwrap();
    let p = Profile {
        id: "default".into(),
        dir: pdir.clone(),
        plain: true,
        config: dir.path().join(".claude.json"),
    };
    let mut f = Fleet::new("fixture-mac".into(), dir.path().join(".codex"));
    f.add_profiles(vec![p.clone()]);
    let row = RosterRow {
        session_id: Some(sid(0)),
        pid: Some(std::process::id()),
        pid_alive: true,
        status: Some("idle".into()),
        ..RosterRow::default()
    };
    let read = RosterRead {
        rows: vec![row],
        active_known: true,
    };
    f.apply_roster(0, Ok(read), RosterGot::default(), now_ms());
    let hook = |at: i64| {
        fs::write(
            pdir.join("fleet-state/.t"),
            json!({"e": "UserPromptSubmit", "at": at}).to_string(),
        )
        .unwrap();
        fs::rename(
            pdir.join("fleet-state/.t"),
            pdir.join(format!("fleet-state/{}.json", sid(0))),
        )
        .unwrap();
    };
    let (t1, t2) = (now_ms(), now_ms() + 1);
    hook(t1);
    f.merge_hook(0, &sid(0), read_hook_file(&p, &sid(0)));
    assert_eq!(
        f.claude_session(0, &sid(0), now_ms()).unwrap().hook_at,
        Some(t1)
    );

    hook(t2);
    let emfile = |_: &Path| -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::from_raw_os_error(libc::EMFILE))
    };
    f.merge_hook(0, &sid(0), read_hook_file_with(&p, &sid(0), &emfile));
    let s = f.claude_session(0, &sid(0), now_ms()).unwrap();
    assert_eq!(s.hook_at, Some(t1), "a failed read keeps what was held");
    assert_eq!(s.state, State::Working);
    // The next good read still takes the new version.
    f.merge_hook(0, &sid(0), read_hook_file(&p, &sid(0)));
    assert_eq!(
        f.claude_session(0, &sid(0), now_ms()).unwrap().hook_at,
        Some(t2)
    );
    // Gone clears it.
    fs::remove_file(pdir.join(format!("fleet-state/{}.json", sid(0)))).unwrap();
    f.merge_hook(0, &sid(0), read_hook_file(&p, &sid(0)));
    assert_eq!(
        f.claude_session(0, &sid(0), now_ms()).unwrap().hook_at,
        None
    );
}

/// NB1 (a, b): launchd's 256 soft limit is raised to min(hard, 8192), and the files followed are a quarter of what is
/// obtained, at most 256.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_open_files_limit_is_raised_and_bounds_the_follows() {
    use fleetd::sessions::follow_cap;
    use fleetd::watch::raise_open_files;
    let _g = SERIAL.lock().await;
    let mut was = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut was) }, 0);
    let low = libc::rlimit {
        rlim_cur: 256,
        rlim_max: was.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
    let (soft, hard) = raise_open_files();
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &was) };
    assert_eq!(hard, was.rlim_max);
    assert_eq!(soft, was.rlim_max.min(8192));
    assert_eq!(follow_cap(256), 64);
    assert_eq!(follow_cap(100), 25);
    assert_eq!(follow_cap(8192), 256);
}

/// Ruling R20: a reconcile of an unchanged home stats each hook file and reads no body; a hook that moved is read once.
#[test]
fn a_reconcile_of_an_unchanged_home_reads_no_hook_file_bodies() {
    use fleet_model::decide::RosterRow;
    use fleet_read::profiles::Profile;
    use fleet_read::roster::RosterRead;
    use fleetd::sessions::{read_hook_file, reconcile_read_with, Fleet, RosterGot};
    use std::cell::Cell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let pdir = dir.path().join(".claude");
    fs::create_dir_all(pdir.join("fleet-state")).unwrap();
    let p = Profile {
        id: "default".into(),
        dir: pdir.clone(),
        plain: true,
        config: dir.path().join(".claude.json"),
    };
    let mut f = Fleet::new("fixture-mac".into(), dir.path().join(".codex"));
    f.add_profiles(vec![p.clone()]);
    let rows: Vec<_> = (0..20)
        .map(|i| RosterRow {
            session_id: Some(sid(i)),
            pid: Some(std::process::id()),
            pid_alive: true,
            status: Some("idle".into()),
            ..RosterRow::default()
        })
        .collect();
    let read = RosterRead {
        rows,
        active_known: true,
    };
    f.apply_roster(0, Ok(read), RosterGot::default(), now_ms());
    let hook = |n: usize, at: i64| {
        fs::write(
            pdir.join("fleet-state/.t"),
            json!({"e": "Stop", "at": at}).to_string(),
        )
        .unwrap();
        fs::rename(
            pdir.join("fleet-state/.t"),
            pdir.join(format!("fleet-state/{}.json", sid(n))),
        )
        .unwrap();
    };
    // Hook files for 10 of the 20 sessions, read once as the watcher does.
    for n in 0..10 {
        hook(n, now_ms());
        f.merge_hook(0, &sid(n), read_hook_file(&p, &sid(n)));
    }
    let reads = Rc::new(Cell::new(0usize));
    let counter = reads.clone();
    let count = move |path: &Path| -> std::io::Result<Vec<u8>> {
        counter.set(counter.get() + 1);
        fs::read(path)
    };
    let plan = f.reconcile_plan(|_| false);
    reconcile_read_with(&plan, None, false, now_ms(), &count);
    assert_eq!(reads.get(), 0, "an unchanged home reads no hook body");

    // One hook moves; one session that had none gets one: two bodies, and no more.
    hook(3, now_ms() + 5);
    hook(15, now_ms());
    reconcile_read_with(&plan, None, false, now_ms(), &count);
    assert_eq!(reads.get(), 2);
}

/// The time `f` was last written, epoch ms (what the tail's `written_at` is).
fn mtime_ms(f: &Path) -> i64 {
    fs::metadata(f)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// F4: a session whose process dies with no `SessionEnd` (a crash) is not live within 100 ms: its pid is watched for
/// its exit (kqueue `NOTE_EXIT`), not left for the 30 s reconcile.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_whose_process_dies_is_not_live_within_100ms() {
    let _g = SERIAL.lock().await;
    let home = Home::new(0);
    let work = home.root.join("work");
    fs::create_dir_all(&work).unwrap();
    let held = common::Held::start(&home.root.join("claude-session"), &[], &work);
    home.roster(held.pid(), "idle", true);
    let (store, h, mut rx) = home.start();
    let s = &store.snapshot().1[0];
    assert_eq!(
        (s.state.clone(), s.live),
        (State::Waiting, true),
        "{}",
        s.reason
    );
    assert!(h.watched_pids() >= 1, "its pid is watched");
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain(&mut rx, Duration::from_millis(100)).await;

    // It crashes: Claude Code no longer lists it as running, and nothing writes a hook or a jobs/ record.
    home.roster(held.pid(), "idle", false);
    let t0 = Instant::now();
    // SAFETY: the helper this test started.
    unsafe { libc::kill(held.pid() as libc::pid_t, libc::SIGKILL) };
    let (s, at) = wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.key.session_id == sid(0) && !s.live
    })
    .await
    .expect("a delta that it is not live");
    let ms = at.duration_since(t0).as_secs_f64() * 1000.0;
    eprintln!("process exit -> not live: {ms:.2} ms ({})", s.reason);
    assert_eq!(s.state, State::Ended);
    assert!(ms < 100.0, "{ms:.2} ms");
    drop(held);
}

/// F4: what the clock alone changes comes at its time, not at the next reconcile. A turn stopped with Esc fires
/// nothing: a working hook with the roster idle is waiting once it has been quiet 60 s (`decide`'s own threshold). The
/// hook says it was written 59.5 s ago, so the threshold comes half a second in; the delta must come within 100 ms of
/// it, and not before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_working_hook_with_the_roster_idle_is_waiting_at_60s_within_100ms() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let (_store, _h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    let at = home.hook_at(0, "PreToolUse", Some("Bash"), now_ms() - 59_500);
    let (s, _) = wait_for(&mut rx, Duration::from_secs(1), |s| s.hook_at == Some(at))
        .await
        .expect("the hook's delta");
    assert_eq!(s.state, State::Working, "{}", s.reason);
    let due = at + 60_001;
    let (s, _) = wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.key.session_id == sid(0) && s.state == State::Waiting
    })
    .await
    .expect("waiting at its threshold");
    let late = now_ms() - due;
    eprintln!("60 s threshold -> delta: {late} ms after it");
    assert_eq!(s.reason, "idle at prompt");
    assert!((0..100).contains(&late), "{late} ms after the threshold");
}

/// F3, the grant: a permission prompt (hook `PermissionRequest`, roster `waiting`) needs you. Ross grants; the tool
/// runs with no hook and no jobs/ change, and the roster now says `busy`. A transcript write to the session asks for
/// its roster at once (within 500 ms), so the stale `waiting` row goes; the session is working as soon as `decide`'s
/// own rule allows (a hook that needs you, quiet for 3 s, beside a busy roster), within 100 ms of that time. The write
/// itself restarts the quiet (Node's `quiet` counts from the last write too), so "working" cannot come sooner than 3 s
/// after it without changing the oracle's rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_permission_reads_working_after_a_transcript_write() {
    let _g = SERIAL.lock().await;
    let home = Home::new(0);
    home.roster(std::process::id(), "waiting", true);
    let f = home.transcript(0);
    append(&f, &assistant("I need to run the build."));
    let (_store, _h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    let at = home.hook(0, "PermissionRequest", Some("Bash"));
    let (s, _) = wait_for(&mut rx, Duration::from_secs(1), |s| s.hook_at == Some(at))
        .await
        .unwrap();
    assert_eq!(s.state, State::NeedsYou, "{}", s.reason);

    // Granted: the roster says busy, and the session writes to its transcript.
    tokio::time::sleep(Duration::from_millis(300)).await;
    home.roster(std::process::id(), "busy", true);
    let calls = home.calls("agents --json --all");
    let t0 = Instant::now();
    append(
        &f,
        &json!({"type":"progress","timestamp":"2026-11-06T12:00:01.000Z"}),
    );
    let w = mtime_ms(&f);
    while home.calls("agents --json --all") == calls {
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "no roster run within 500 ms of the write"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    eprintln!(
        "transcript write on a needing-you session -> roster run: {:.2} ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    let (s, _) = wait_for(&mut rx, Duration::from_secs(5), |s| {
        s.key.session_id == sid(0) && s.state == State::Working
    })
    .await
    .expect("working once quiet");
    let late = now_ms() - (w + 3_001);
    eprintln!("granted -> working: {late} ms after quiet passed 3 s");
    assert_eq!(s.reason, "working: roster busy");
    assert!((0..100).contains(&late), "{late} ms");
}

/// F3/F4, the grant with no write at all (a long tool run): when the hook that needs you has been quiet 3 s, the timer
/// asks for the roster once, and the busy row makes it working within 500 ms of that time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_permission_reads_working_once_quiet_with_no_write() {
    let _g = SERIAL.lock().await;
    let home = Home::new(0);
    home.roster(std::process::id(), "waiting", true);
    let (_store, _h, mut rx) = home.start();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain(&mut rx, Duration::from_millis(100)).await;
    let at = home.hook(0, "PermissionRequest", Some("Bash"));
    wait_for(&mut rx, Duration::from_secs(1), |s| {
        s.hook_at == Some(at) && s.state == State::NeedsYou
    })
    .await
    .unwrap();
    home.roster(std::process::id(), "busy", true);
    let (s, _) = wait_for(&mut rx, Duration::from_secs(5), |s| {
        s.key.session_id == sid(0) && s.state == State::Working
    })
    .await
    .expect("working once quiet");
    let late = now_ms() - (at + 3_001);
    eprintln!("granted, no write -> working: {late} ms after quiet passed 3 s");
    assert_eq!(s.reason, "working: roster busy");
    assert!((0..500).contains(&late), "{late} ms");
}

/// Ruling R25: a granted permission leaves the session NeedsYou by its hook while the roster says busy, and its
/// transcript grows every 200 ms: those writes must not ask the roster again and again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_roster_row_is_not_nudged_by_transcript_writes() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let f = home.transcript(0);
    append(
        &f,
        &json!({"type":"user","timestamp":"2026-11-06T11:59:00.000Z",
                "message":{"role":"user","content":"Run the long tool"}}),
    );
    home.roster(std::process::id(), "busy", true);
    home.hook(0, "PermissionRequest", None);
    let (_store, h, _rx) = home.start();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = h.roster_runs();
    for i in 0..50 {
        append(&f, &assistant(&format!("progress {i}")));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let runs = h.roster_runs() - before;
    assert!(runs <= 2, "the roster ran {runs} times during the tool run");
}

/// A background job's Session carries its roster id (what `claude attach` takes); an interactive session's has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_job_carries_its_roster_id_as_job_id() {
    let _g = SERIAL.lock().await;
    let home = Home::new(2);
    let s = home.stand_in();
    let rows = json!([
        {"id": "a1b2c3d4", "sessionId": sid(0), "pid": std::process::id(), "kind": "background", "status": "idle",
         "name": "fixture 0", "cwd": CWD, "startedAt": STARTED},
        {"id": "a1b2c3d5", "sessionId": sid(1), "pid": std::process::id(), "kind": "interactive", "status": "idle",
         "name": "fixture 1", "cwd": CWD, "startedAt": STARTED},
    ]);
    fs::write(s.join("all.json"), rows.to_string()).unwrap();
    let (store, _h, _rx) = home.start();
    let (_, sessions) = store.snapshot();
    let by = |n: usize| {
        sessions
            .iter()
            .find(|x| x.key.session_id == sid(n))
            .unwrap()
    };
    assert_eq!(by(0).job_id.as_deref(), Some("a1b2c3d4"));
    assert_eq!(by(1).job_id, None);
}

// ------------------------------------------------------------------------------------------------- board metadata

fn key(n: usize) -> String {
    format!("fixture-mac:{}", sid(n))
}

fn session(store: &Store, n: usize) -> Session {
    store
        .snapshot()
        .1
        .into_iter()
        .find(|s| s.key.session_id == sid(n))
        .expect("the session is on the board")
}

/// The titles, epics, archive marks and accounts are in the first snapshot, and so are the groups and accounts of the
/// Meta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_panel_s_files_are_laid_over_the_first_snapshot() {
    let _g = SERIAL.lock().await;
    let home = Home::new(4);
    home.login("ada@example.com");
    home.panel_file(
        "fleet.json",
        &json!({"self": "fixture-mac", "names": {"claude:ada@example.com": "work"},
                "epics": [{"name": "Fixtures", "about": "the fixtures"}, {"name": "Other work"}]})
        .to_string(),
    );
    home.panel_file(
        "fleet-labels.json",
        &json!({key(0): {"title": "My title", "group": "Experiments"}, key(2): {"group": "Fixtures"}}).to_string(),
    );
    home.panel_file(
        "fleet-titles.json",
        &json!({key(1): {"title": "Written", "stage": "building", "epic": "Other work"}})
            .to_string(),
    );
    home.panel_file("fleet-archived.json", &json!({key(3): STARTED}).to_string());
    home.status_limits(0, now_ms(), (37.4, 1_793_970_000), (100.0, 1_794_400_000));
    let (store, _h, _rx) = home.start();

    let s0 = session(&store, 0);
    assert_eq!(s0.title.as_deref(), Some("My title"), "yours first");
    assert_eq!(s0.epic.as_deref(), Some("Experiments"));
    assert_eq!(s0.account.as_deref(), Some("work"));
    assert!(!s0.archived);
    let s1 = session(&store, 1);
    assert_eq!(
        s1.title.as_deref(),
        Some("fixture 1"),
        "Claude Code's own name beats the written title"
    );
    assert_eq!(s1.epic.as_deref(), Some("Other work"), "the written epic");
    assert_eq!(session(&store, 2).epic.as_deref(), Some("Fixtures"));
    assert_eq!(session(&store, 2).title.as_deref(), Some("fixture 2"));
    // Hidden at STARTED, live, and started at STARTED (not after): archived.
    assert!(session(&store, 3).archived);

    let m = store.meta();
    assert_eq!(m.epics, ["Fixtures", "Other work"]);
    assert_eq!(m.groups, ["Experiments"]);
    assert_eq!(m.accounts.len(), 1);
    let a = &m.accounts[0];
    assert_eq!(
        (a.name.as_str(), a.engine.clone(), a.profile.as_str()),
        ("work", Engine::Claude, "default")
    );
    assert_eq!(
        a.limits,
        [
            Limit {
                window: "5h".into(),
                used_pct: 37,
                resets_at: Some(1_793_970_000_000)
            },
            Limit {
                window: "7d".into(),
                used_pct: 100,
                resets_at: Some(1_794_400_000_000)
            },
        ]
    );
}

/// Without a name in `fleet.json`, an account is its email's local part.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_without_a_name_is_its_email_s_local_part() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    home.login("bob.smith@example.com");
    let (store, _h, _rx) = home.start();
    assert_eq!(session(&store, 0).account.as_deref(), Some("bob.smith"));
    assert_eq!(store.meta().accounts[0].name, "bob.smith");
    assert!(
        store.meta().accounts[0].limits.is_empty(),
        "no status line has reported any"
    );
}

/// Changing `fleet-labels.json` is a delta for that session within 50 ms, and the groups follow in a Meta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_change_is_a_delta_within_50ms() {
    let _g = SERIAL.lock().await;
    let home = Home::new(2);
    let (store, _h, mut rx) = home.start();
    let mut meta = store.subscribe_meta();
    let mut worst = Duration::ZERO;
    for i in 0..10 {
        let title = format!("Renamed {i}");
        let t0 = Instant::now();
        home.panel_file(
            "fleet-labels.json",
            &json!({key(0): {"title": title, "group": format!("Group {i}")}}).to_string(),
        );
        let (s, at) = wait_for(&mut rx, Duration::from_secs(2), |s| {
            s.key.session_id == sid(0) && s.title.as_deref() == Some(title.as_str())
        })
        .await
        .expect("a delta with the new title");
        assert_eq!(s.epic, Some(format!("Group {i}")));
        worst = worst.max(at.duration_since(t0));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!(
        "label write -> delta: worst {:.2} ms (10 writes)",
        worst.as_secs_f64() * 1000.0
    );
    assert!(worst < Duration::from_millis(50), "worst {worst:?}");
    // Only the changed session was sent: the other one has nothing new.
    assert_eq!(session(&store, 1).epic, None);
    meta.changed().await.unwrap();
    assert_eq!(store.meta().groups, ["Group 9"]);
}

/// Archiving and un-archiving are deltas; so is a change of `fleet.json` (names and epics).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_marks_and_names_follow_their_files() {
    let _g = SERIAL.lock().await;
    let home = Home::new(2);
    home.login("ada@example.com");
    let (store, _h, mut rx) = home.start();
    assert!(!session(&store, 1).archived);
    home.panel_file("fleet-archived.json", &json!({key(1): STARTED}).to_string());
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.key.session_id == sid(1) && s.archived
    })
    .await
    .expect("archived");
    assert!(!session(&store, 0).archived);
    home.panel_file("fleet-archived.json", "{}");
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.key.session_id == sid(1) && !s.archived
    })
    .await
    .expect("no longer archived");
    home.panel_file(
        "fleet.json",
        &json!({"names": {"claude:ada@example.com": "renamed"}, "epics": [{"name": "Fresh"}]})
            .to_string(),
    );
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.account.as_deref() == Some("renamed")
    })
    .await
    .expect("the account's new name");
    assert_eq!(store.meta().epics, ["Fresh"]);
    assert_eq!(store.meta().accounts[0].name, "renamed");
}

/// Review Focus 5 at run time: a truncated or non-JSON panel file, or a missing folder, is empty: the board keeps
/// going, and the next good write is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_panel_file_is_empty_and_the_next_good_one_is_taken() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    home.panel_file(
        "fleet-labels.json",
        &json!({key(0): {"title": "Mine"}}).to_string(),
    );
    let (store, _h, mut rx) = home.start();
    assert_eq!(session(&store, 0).title.as_deref(), Some("Mine"));
    home.panel_file("fleet-labels.json", "{\"fixture-mac:sess-0000-fixt");
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.title.as_deref() == Some("fixture 0")
    })
    .await
    .expect("a truncated file is nothing: the name shows");
    home.panel_file("fleet.json", "not json at all");
    home.panel_file(
        "fleet-labels.json",
        &json!({key(0): {"title": "Mine again"}}).to_string(),
    );
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.title.as_deref() == Some("Mine again")
    })
    .await
    .expect("the next good write is taken");

    // A home with no panel folder at all: the board starts, with nothing from it.
    let bare = Home::new(1);
    fs::remove_dir_all(bare.panel()).unwrap();
    let (store, _h, _rx) = bare.start();
    let s = session(&store, 0);
    assert_eq!(
        (s.title.as_deref(), s.epic, s.archived),
        (Some("fixture 0"), None, false)
    );
    assert_eq!(store.meta().epics, Vec::<String>::new());
}

/// An account's limits follow its newest status-line snapshot, and a Meta is sent when they move, not when a snapshot
/// brings nothing new.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_status_line_limit_change_is_a_new_meta() {
    let _g = SERIAL.lock().await;
    let home = Home::new(2);
    home.login("ada@example.com");
    let at = now_ms();
    home.status_limits(0, at, (10.0, 1_793_970_000), (20.0, 1_794_400_000));
    let (store, _h, _rx) = home.start();
    let mut meta = store.subscribe_meta();
    meta.borrow_and_update();
    let used = |store: &Store| {
        store.meta().accounts[0]
            .limits
            .iter()
            .map(|l| l.used_pct)
            .collect::<Vec<_>>()
    };
    assert_eq!(used(&store), [10, 20]);

    // Another session's newer reading is the account's (the budget is shared), though that session is not the first.
    home.status_limits(1, at + 1000, (11.0, 1_793_970_000), (21.0, 1_794_400_000));
    tokio::time::timeout(Duration::from_secs(2), meta.changed())
        .await
        .expect("a Meta")
        .unwrap();
    assert_eq!(used(&store), [11, 21]);

    // An older reading changes nothing.
    home.status_limits(0, at - 5000, (90.0, 1_793_970_000), (90.0, 1_794_400_000));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), meta.changed())
            .await
            .is_err()
    );
    assert_eq!(used(&store), [11, 21]);
}

/// Codex's account is its id token's email; its limits are the newest `token_count` of its logs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_s_account_and_limits_come_from_its_login_and_its_logs() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    let codex = home.root.join(".codex");
    // A JWT whose payload is {"email":"cora@example.com"}; the signature is not read.
    fs::write(
        codex.join("auth.json"),
        json!({"tokens": {"id_token": "e30.eyJlbWFpbCI6ImNvcmFAZXhhbXBsZS5jb20ifQ.sig"}})
            .to_string(),
    )
    .unwrap();
    let f = codex.join("sessions/2026/11/06/rollout-2026-11-06T09-00-00-fixture.jsonl");
    let id = "00000000-0000-4000-8000-000000000002";
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
    let (store, _h, mut rx) = home.start();
    let codex_account = |store: &Store| {
        store
            .meta()
            .accounts
            .into_iter()
            .find(|a| a.engine == Engine::Codex)
    };
    let a = codex_account(&store).expect("the codex account");
    assert_eq!((a.name.as_str(), a.profile.as_str()), ("cora", "codex"));
    assert!(a.limits.is_empty());
    assert_eq!(
        store
            .snapshot()
            .1
            .iter()
            .find(|s| s.key.session_id == id)
            .unwrap()
            .account
            .as_deref(),
        Some("cora")
    );

    let mut meta = store.subscribe_meta();
    meta.borrow_and_update();
    append(
        &f,
        &line(
            "event_msg",
            json!({"type": "token_count", "info": null, "rate_limits": {
                "primary": {"used_percent": 12.0, "window_minutes": 300, "resets_at": 1_793_970_000},
                "secondary": {"used_percent": 55.0, "window_minutes": 10080, "resets_at": 1_794_400_000}}}),
        ),
    );
    wait_for(&mut rx, Duration::from_secs(2), |s| s.key.session_id == id).await;
    tokio::time::timeout(Duration::from_secs(2), meta.changed())
        .await
        .expect("a Meta with the limits")
        .unwrap();
    assert_eq!(
        codex_account(&store).unwrap().limits,
        [
            Limit {
                window: "5h".into(),
                used_pct: 12,
                resets_at: Some(1_793_970_000_000)
            },
            Limit {
                window: "7d".into(),
                used_pct: 55,
                resets_at: Some(1_794_400_000_000)
            },
        ]
    );
}

fn start_fast(home: &Home) -> (Arc<Store>, WatchHandle, Receiver<Delta>) {
    use fleetd::watch::Options;
    let store = Arc::new(Store::new());
    let opts = Options {
        reconcile_every: Duration::from_millis(200),
        roster_safety_every: Duration::from_millis(200),
        ..Options::default()
    };
    let h = fleetd::watch::watch_with(&home.root, store.clone(), opts).unwrap();
    let rx = store.subscribe();
    (store, h, rx)
}

/// A panel folder that is made after the start is read at once, with its files already in it, when the reconcile finds it
/// (no later event would come for files that were there before the watch).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panel_folder_made_after_the_start_is_read_when_found() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    fs::remove_dir_all(home.panel()).unwrap();
    let (store, _h, mut rx) = start_fast(&home);
    assert_eq!(session(&store, 0).title.as_deref(), Some("fixture 0"));
    // The folder arrives whole: made elsewhere, then renamed into place, so no event is for a file in it.
    let tmp = home.root.join("panel-new");
    fs::create_dir_all(&tmp).unwrap();
    fs::write(
        tmp.join("fleet-labels.json"),
        json!({key(0): {"title": "From the new folder"}}).to_string(),
    )
    .unwrap();
    fs::rename(&tmp, home.panel()).unwrap();
    wait_for(&mut rx, Duration::from_secs(3), |s| {
        s.title.as_deref() == Some("From the new folder")
    })
    .await
    .expect("the label in the folder that appeared");
    // And it is watched from then on.
    home.panel_file(
        "fleet-labels.json",
        &json!({key(0): {"title": "Changed after"}}).to_string(),
    );
    wait_for(&mut rx, Duration::from_secs(2), |s| {
        s.title.as_deref() == Some("Changed after")
    })
    .await
    .expect("the folder is watched");
}

/// A login that changes reaches the sessions' `account` (and the accounts) by the next reconcile's account read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_change_updates_the_sessions_account() {
    let _g = SERIAL.lock().await;
    let home = Home::new(1);
    home.login("first.person@example.com");
    let (store, _h, mut rx) = start_fast(&home);
    assert_eq!(session(&store, 0).account.as_deref(), Some("first.person"));
    home.login("second.person@example.com");
    wait_for(&mut rx, Duration::from_secs(3), |s| {
        s.account.as_deref() == Some("second.person")
    })
    .await
    .expect("the session's new account");
    assert_eq!(store.meta().accounts[0].name, "second.person");
}
