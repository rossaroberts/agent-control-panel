//! Terminal connections over the real socket: a `role: Terminals` client opens, types, floods, stalls and hangs up, and
//! a board client on the same daemon is unaffected. Its own test binary, and every test here runs alone (`SERIAL`): some
//! count the process's fds and resident memory, which another test's ptys would move.

mod terms_home;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleet_model::types::{AttachTarget, Engine, Session, SessionKey, State};
use fleet_model::wire::{read_frame, write_frame, Frame, Role, MAX_FRAME, PROTO, TERM_IN_MAX};
use fleetd::client::Client;
use fleetd::serve::{bind, serve, SocketGuard};
use fleetd::store::Store;
use fleetd::terms::Terms;
use terms_home::{claude, codex, has, Home, BOOT, SID};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

/// Held by each test for its whole run (an async lock: each test has its own runtime, and a panicking test frees it).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

fn fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

/// This process's resident memory, in bytes.
fn rss() -> u64 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    assert_eq!(n, size);
    info.pti_resident_size
}

fn sess(n: u64) -> Session {
    Session {
        key: SessionKey {
            machine: "fixture-mac".into(),
            profile: "default".into(),
            session_id: "sess-0001".into(),
        },
        engine: Engine::Claude,
        name: Some("fixture".into()),
        cwd: Some("/tmp/fleetd-fixture".into()),
        state: State::Working,
        reason: "working".into(),
        live: true,
        pid: Some(4242),
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: None,
        doing: Some(format!("step {n}")),
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
        job_id: None,
    }
}

struct Daemon {
    home: Home,
    _sock: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    terms: Arc<Terms>,
    task: JoinHandle<()>,
    _guard: SocketGuard,
}

impl Daemon {
    fn start() -> Daemon {
        let home = Home::new();
        let sock = tempfile::tempdir().unwrap();
        let path = sock.path().join("fleetd.sock");
        let store = Arc::new(Store::new());
        let terms = Arc::new(home.terms());
        let (listener, _guard) = bind(&path).unwrap();
        let task = tokio::spawn(serve(
            listener,
            store.clone(),
            "fixture-mac".into(),
            terms.clone(),
        ));
        Daemon {
            home,
            _sock: sock,
            path,
            store,
            terms,
            task,
            _guard,
        }
    }

    async fn term_client(&self) -> TClient {
        TClient::connect(&self.path).await
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A Terminals client: what it has been sent, per ref.
struct TClient {
    r: OwnedReadHalf,
    w: OwnedWriteHalf,
    outs: HashMap<u32, Vec<u8>>,
    ends: Vec<(u32, Option<String>)>,
    errors: Vec<String>,
    /// Each output or end, in arrival order: (ref, None) for output, (ref, Some(error)) for an end.
    log: Vec<(u32, Option<Option<String>>)>,
}

impl TClient {
    async fn connect(path: &std::path::Path) -> TClient {
        let (r, mut w) = UnixStream::connect(path).await.unwrap().into_split();
        let hello = Frame::Hello {
            proto: PROTO,
            since: None,
            run: None,
            role: Role::Terminals,
        };
        write_frame(&mut w, &hello).await.unwrap();
        TClient {
            r,
            w,
            outs: HashMap::new(),
            ends: vec![],
            errors: vec![],
            log: vec![],
        }
    }

    async fn send(&mut self, f: Frame) {
        write_frame(&mut self.w, &f).await.unwrap();
    }

    async fn open(&mut self, r: u32, target: AttachTarget) {
        self.send(Frame::TermOpen {
            r#ref: r,
            target,
            cols: 100,
            rows: 30,
        })
        .await;
    }

    async fn type_in(&mut self, r: u32, data: &[u8]) {
        self.send(Frame::TermIn {
            r#ref: r,
            data: data.to_vec(),
        })
        .await;
    }

    fn output(&self, r: u32) -> &[u8] {
        self.outs.get(&r).map_or(&[], |v| v.as_slice())
    }

    fn ended(&self, r: u32) -> bool {
        self.ends.iter().any(|(x, _)| *x == r)
    }

    /// One frame, waiting until `end` at most. `None`: none came in time; `Some(false)`: the server closed.
    async fn take(&mut self, end: Instant) -> Option<bool> {
        let left = end.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, read_frame(&mut self.r, MAX_FRAME)).await {
            Err(_) => None,
            Ok(Err(_)) => Some(false),
            Ok(Ok(Frame::TermOut { r#ref, data })) => {
                self.log.push((r#ref, None));
                self.outs.entry(r#ref).or_default().extend(data);
                Some(true)
            }
            Ok(Ok(Frame::TermEnd { r#ref, error })) => {
                self.log.push((r#ref, Some(error.clone())));
                self.ends.push((r#ref, error));
                Some(true)
            }
            Ok(Ok(Frame::Error { message })) => {
                self.errors.push(message);
                Some(true)
            }
            Ok(Ok(f)) => panic!("unexpected {f:?}"),
        }
    }

    /// Reads until `done`, the server closes, or `secs` pass; whether it was done.
    async fn until(&mut self, secs: f64, done: impl Fn(&TClient) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while !done(self) {
            if self.take(end).await != Some(true) {
                return done(self);
            }
        }
        true
    }

    /// Reads until `ref`'s output holds `needle` (looking only at what is new each time).
    async fn until_has(&mut self, r: u32, needle: &str, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        let mut from = 0;
        loop {
            let out = self.output(r);
            if has(&out[from..], needle) {
                return true;
            }
            from = out.len().saturating_sub(needle.len());
            if self.take(end).await != Some(true) {
                return has(&self.output(r)[from..], needle);
            }
        }
    }

    /// Reads until the server closes; false if it has not within `secs`.
    async fn until_closed(&mut self, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            match self.take(end).await {
                Some(true) => {}
                Some(false) => return true,
                None => return false,
            }
        }
    }

    /// Reads for `secs`, whatever comes.
    async fn drain_for(&mut self, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while self.take(end).await == Some(true) {}
    }
}

/// `lines` lines of exactly `width` bytes (newline included), numbered `<tag><n>` and padded with dots.
fn numbered(tag: char, from: usize, lines: usize, width: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(lines * width);
    for n in from..from + lines {
        let mut l = format!("{tag}{n:06}").into_bytes();
        l.resize(width - 1, b'.');
        l.push(b'\n');
        v.extend(l);
    }
    v
}

fn pid_of(h: &Home, word: &str) -> i32 {
    *h.starts_of(word).last().expect("a start")
}

/// (a) Open, the banner, type, the echo: over the real socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_type_and_see_the_echo_over_the_socket() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000a1")).await;
    assert!(c.until_has(1, "BANNER attach 000000a1", BOOT).await);
    c.type_in(1, b"hello there\n").await;
    assert!(c.until_has(1, "GOT hello there", 5.0).await);
    // A size and a close: the close is answered with the ref's end.
    c.send(Frame::TermSize {
        r#ref: 1,
        cols: 120,
        rows: 40,
    })
    .await;
    assert!(c.until_has(1, "REDRAW 120x40", 5.0).await);
    c.send(Frame::TermClose { r#ref: 1 }).await;
    assert!(c.until(5.0, |c| c.ended(1)).await);
    assert_eq!(c.ends, vec![(1, None)]);
    assert!(c.errors.is_empty());
    assert_eq!(d.home.starts_of("000000a1").len(), 1);
    // A refused open is a TermEnd with term.mjs's words; the connection goes on.
    c.open(
        2,
        AttachTarget::Claude {
            profile: "default".into(),
            job: "not a job".into(),
        },
    )
    .await;
    assert!(c.until(5.0, |c| c.ended(2)).await);
    assert_eq!(
        c.ends[1],
        (2, Some(fleetd::terms::ERR_PICK.to_string())),
        "{:?}",
        c.ends
    );
    c.open(3, claude("000000a1")).await;
    assert!(c.until_has(3, "BANNER", 5.0).await, "warm reopen");
}

/// Board latency while `during` runs: deltas are upserted one by one and timed to the board client.
async fn board_latencies(store: &Store, board: &mut Client, from: u64, n: u64) -> Vec<Duration> {
    let none = std::sync::atomic::AtomicBool::new(false);
    let count = std::sync::atomic::AtomicUsize::new(0);
    board_pings(store, board, from, Some(n), &none, &count)
        .await
        .into_iter()
        .map(|(_, d)| d)
        .collect()
}

/// Board pings (when each started, how long its delta took): `n` of them, or until `stop` is set. `count` counts them
/// as they complete.
async fn board_pings(
    store: &Store,
    board: &mut Client,
    from: u64,
    n: Option<u64>,
    stop: &std::sync::atomic::AtomicBool,
    count: &std::sync::atomic::AtomicUsize,
) -> Vec<(Instant, Duration)> {
    let mut out = vec![];
    for i in from.. {
        if n.is_some_and(|n| i >= from + n) || stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let t = Instant::now();
        store.upsert(vec![sess(i)]);
        loop {
            match tokio::time::timeout(Duration::from_secs(5), board.next())
                .await
                .expect("a delta within 5 s")
                .unwrap()
            {
                Frame::Delta(x) if !x.upsert.is_empty() => break,
                _ => {}
            }
        }
        out.push((t, t.elapsed()));
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    out
}

fn median(v: &[Duration]) -> Duration {
    let mut v = v.to_vec();
    v.sort();
    v[v.len() / 2]
}

fn p95(v: &[Duration]) -> Duration {
    let mut v = v.to_vec();
    v.sort();
    v[(v.len() * 95 / 100).min(v.len() - 1)]
}

/// (b) Review Focus 2: the client drops in the middle of a 50 MB flood. Within 2 s its pty is back in the warm pool (a
/// `claude attach`) or stopped (Codex), the daemon's fds are back to where they were, and a board client meanwhile keeps
/// getting deltas with under 10 ms added.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_dropping_mid_flood_frees_its_pty_and_the_board_never_notices() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut board = Client::connect(&d.path, None).await.unwrap();
    assert!(matches!(
        board.next().await.unwrap(),
        Frame::Snapshot { .. }
    ));
    let quiet = board_latencies(&d.store, &mut board, 0, 40).await;
    let base = median(&quiet);

    // Claude: warm it once, so the baseline holds its pty, then flood it from a second client that drops mid-way.
    {
        let mut c = d.term_client().await;
        c.open(1, claude("000000b1")).await;
        assert!(c.until_has(1, "BANNER", BOOT).await);
        c.send(Frame::TermClose { r#ref: 1 }).await;
        assert!(c.until(5.0, |c| c.ended(1)).await);
    }
    assert!(terms_home::wait_for(Duration::from_secs(2), || d
        .terms
        .warm_count()
        == 1));
    let fd_warm = {
        let mut last = fds();
        // Settled: the first client's socket has closed.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let now = fds();
            if now == last {
                break;
            }
            last = now;
        }
        last
    };

    let mut c = d.term_client().await;
    c.open(1, claude("000000b1")).await;
    assert!(c.until_has(1, "BANNER", 5.0).await);
    // Some of it, then gone; the board is timed throughout, and for a while after.
    let flooding = {
        // Its own task, so the test's own reading and waiting cannot delay it.
        // It runs from before the flood until after the post-drop check.
        let store = d.store.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (stop2, count2) = (stop.clone(), count.clone());
        let pinger = tokio::spawn(async move {
            board_pings(&store, &mut board, 1000, None, &stop2, &count2).await
        });
        let pinged = || count.load(std::sync::atomic::Ordering::Relaxed);
        while pinged() < 5 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        c.type_in(1, b"flood50\n").await;
        let dropped = {
            assert!(c.until(30.0, |c| c.output(1).len() > 4 << 20).await);
            let dropped = Instant::now();
            drop(c);
            let terms = d.terms.clone();
            let back = tokio::task::spawn_blocking(move || {
                terms_home::wait_for(Duration::from_secs(2), || {
                    terms.warm_count() == 1 && terms.live_count() == 1 && fds() == fd_warm
                })
            })
            .await
            .unwrap();
            assert!(
                back,
                "after {:?}: warm {}, live {}, fds {} (baseline {fd_warm})",
                dropped.elapsed(),
                d.terms.warm_count(),
                d.terms.live_count(),
                fds()
            );
            dropped
        };
        // And on for a while after the drop and the check.
        let at = pinged();
        while pinged() < at + 20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let pings = pinger.await.unwrap();
        assert!(
            pings.iter().any(|(t, _)| *t < dropped) && pings.iter().any(|(t, _)| *t > dropped),
            "the board was pinged before and after the drop"
        );
        pings.into_iter().map(|(_, d)| d).collect::<Vec<_>>()
    };
    let worst = *flooding.iter().max().unwrap();
    eprintln!(
        "board delta latency: quiet median {base:?}; while flooding and dropping: p95 {:?}, worst {worst:?}",
        p95(&flooding)
    );
    assert!(
        worst.saturating_sub(base) < Duration::from_millis(10),
        "board delta latency: quiet median {base:?}, worst while flooding {worst:?}"
    );
    assert_eq!(
        d.home.starts_of("000000b1").len(),
        1,
        "it was warm, not respawned"
    );

    // Codex: stopped when its client drops, and every fd it had is gone.
    let fd0 = fds();
    let mut c = d.term_client().await;
    c.open(2, codex(SID)).await;
    assert!(c.until_has(2, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, SID);
    c.type_in(2, b"flood50\n").await;
    assert!(c.until(30.0, |c| c.output(2).len() > 4 << 20).await);
    drop(c);
    let dropped = Instant::now();
    assert!(terms_home::gone_within(pid, Duration::from_secs(2)));
    assert!(
        terms_home::wait_for(
            Duration::from_secs(2).saturating_sub(dropped.elapsed()),
            || fds() == fd0
        ),
        "fds {} (baseline {fd0})",
        fds()
    );
    assert_eq!(d.terms.warm_count(), 1);
}

/// (c) A client reading 1 KiB/s while the program floods: the daemon's memory stays bounded (the pty is not read past
/// what the connection holds).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_client_costs_bounded_memory() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, codex(SID)).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, SID);
    let before = rss();
    c.type_in(1, b"flood50\n").await;
    let TClient { mut r, w, .. } = c;
    use tokio::io::AsyncReadExt;
    let mut peak = before;
    let mut buf = [0u8; 1024];
    let mut got = 0usize;
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end {
        got += r.read(&mut buf).await.unwrap();
        peak = peak.max(rss());
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(got <= 8 * 1024, "read {got}");
    assert!(peak < 64 << 20, "resident {} MB while flooding", peak >> 20);
    assert!(
        peak.saturating_sub(before) < 8 << 20,
        "resident grew {} KiB while flooding (from {} MB)",
        peak.saturating_sub(before) >> 10,
        before >> 20
    );
    assert!(terms_home::alive(pid), "the program is blocked, not gone");
    drop((r, w));
    assert!(terms_home::gone_within(pid, Duration::from_secs(3)));
}

/// (d) A TermIn (or size, or close) for a ref that is not open is ignored: no error, no end, the connection goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_for_an_unknown_ref_is_ignored() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.type_in(99, b"nobody\n").await;
    c.send(Frame::TermSize {
        r#ref: 99,
        cols: 80,
        rows: 24,
    })
    .await;
    c.send(Frame::TermClose { r#ref: 99 }).await;
    c.open(1, claude("000000d1")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    c.type_in(1, b"still here\n").await;
    assert!(c.until_has(1, "GOT still here", 5.0).await);
    c.drain_for(0.2).await;
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert!(c.ends.is_empty(), "{:?}", c.ends);
    assert!(!c.outs.contains_key(&99));
}

/// P2-R2: a TermIn of exactly 64 KiB is taken; one of 64 KiB + 1 is refused with an `Error`, and the connection closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_term_in_over_64_kib_is_refused() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000e1")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let full = numbered('L', 0, TERM_IN_MAX / 64, 64);
    assert_eq!(full.len(), TERM_IN_MAX);
    c.type_in(1, &full).await;
    let last = format!("GOT L{:06}", TERM_IN_MAX / 64 - 1);
    assert!(c.until_has(1, &last, 10.0).await, "all 64 KiB arrived");
    assert!(c.errors.is_empty());

    let mut over = numbered('M', 0, TERM_IN_MAX / 64, 64);
    over.push(b'\n');
    assert_eq!(over.len(), TERM_IN_MAX + 1);
    c.type_in(1, &over).await;
    assert!(
        c.until_closed(5.0).await,
        "the server closed the connection"
    );
    assert_eq!(
        c.errors,
        vec![format!(
            "TermIn of {} bytes; at most {TERM_IN_MAX}",
            TERM_IN_MAX + 1
        )]
    );
    assert!(
        !has(c.output(1), "M000000"),
        "none of it reached the program"
    );
    // The attach ended with the connection; the session's pty is warm, not stopped.
    assert!(terms_home::wait_for(Duration::from_secs(2), || d
        .terms
        .warm_count()
        == 1));
    // Its ring holds everything the program printed and echoed: none of the refused frame is there.
    let mut c = d.term_client().await;
    c.open(1, claude("000000e1")).await;
    assert!(c.until_has(1, "REDRAW", 5.0).await, "replay, then the kick");
    assert!(
        has(c.output(1), &last),
        "the replay holds the accepted input"
    );
    assert!(
        !has(c.output(1), "M000000"),
        "none of the refused frame reached the program"
    );
    assert_eq!(d.home.starts_of("000000e1").len(), 1);
}

/// P2-R4: input past the program's 1 MiB queue is held, not dropped, and nothing more is read from that client until it
/// goes in. A frame behind the held one (an open on another ref) waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_to_a_stopped_program_holds_the_connection_and_loses_nothing() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000f1")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, "000000f1");
    unsafe { libc::kill(pid, libc::SIGSTOP) };

    // 24 frames of 64 KiB (1.5 MiB, in 512-byte lines), then an open of ref 2: from a task, as the socket fills.
    let frames = 24;
    let per = TERM_IN_MAX / 512;
    // The client's write half goes to the task; a stand-in takes its place meanwhile.
    let (_, spare) = UnixStream::pair().unwrap().0.into_split();
    let mut w = std::mem::replace(&mut c.w, spare);
    let writer = tokio::spawn(async move {
        for i in 0..frames {
            let data = numbered('L', i * per, per, 512);
            let f = Frame::TermIn { r#ref: 1, data };
            write_frame(&mut w, &f).await.unwrap();
        }
        let f = Frame::TermOpen {
            r#ref: 2,
            target: claude("000000f2"),
            cols: 100,
            rows: 30,
        };
        write_frame(&mut w, &f).await.unwrap();
        w
    });
    // Held: the open behind the input does not happen while the program is stopped.
    c.drain_for(1.5).await;
    assert!(
        d.home.starts_of("000000f2").is_empty(),
        "read past a held TermIn"
    );
    assert!(!c.outs.contains_key(&2));
    unsafe { libc::kill(pid, libc::SIGCONT) };
    let last = format!("GOT L{:06}", frames * per - 1);
    assert!(c.until_has(1, &last, 30.0).await, "the last line arrived");
    let out = c.output(1);
    let mut at = 0;
    for n in 0..frames * per {
        let needle = format!("GOT L{n:06}");
        let p = terms_home::pos(&out[at..], &needle)
            .unwrap_or_else(|| panic!("{needle} missing or out of order"));
        at += p + needle.len();
    }
    assert!(
        c.until_has(2, "BANNER", BOOT).await,
        "the open behind it, once it went in"
    );
    c.w = writer.await.unwrap();
    assert!(c.errors.is_empty());
}

/// P2-R4: a terminal that ends while its input is held lets the held frames go, and the connection reads on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_ending_while_held_releases_the_connection() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000f3")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, "000000f3");
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let per = TERM_IN_MAX / 512;
    // From a task: the client's writes block once the server holds a frame and stops reading.
    let (_, spare) = UnixStream::pair().unwrap().0.into_split();
    let mut w = std::mem::replace(&mut c.w, spare);
    let writer = tokio::spawn(async move {
        for i in 0..20 {
            let data = numbered('L', i * per, per, 512);
            write_frame(&mut w, &Frame::TermIn { r#ref: 1, data })
                .await
                .unwrap();
        }
        w
    });
    // The 1 MiB queue is full and a frame is held; the program then dies.
    c.drain_for(1.0).await;
    assert!(!writer.is_finished(), "the server stopped reading");
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert!(c.until(10.0, |c| c.ended(1)).await);
    c.w = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("the held frames were let go and the rest read")
        .unwrap();
    c.open(2, claude("000000f4")).await;
    assert!(c.until_has(2, "BANNER", BOOT).await);
    assert!(c.errors.is_empty());
}

/// Keystroke echo through the socket while cold opens run on another connection, against a direct pty. Recorded for
/// Task 5's budget (added p95 <= 1 ms); asserted here only loosely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keystroke_echo_through_the_socket_during_cold_opens() {
    let _s = serial().await;
    let d = Daemon::start();
    let samples = 300;

    // Direct: the stand-in on a pty of our own.
    let direct = {
        use std::io::{Read, Write};
        let cmd = fleetd::attach_cmd::command(&claude("0000e0d1"), &d.home.path()).unwrap();
        let pty = fleetd::pty::Pty::spawn(&cmd, 100, 30).unwrap();
        let mut rd = std::fs::File::from(pty.reader().unwrap());
        let mut buf = [0u8; 4096];
        let mut seen = vec![];
        while !has(&seen, "BANNER") {
            let n = rd.read(&mut buf).unwrap();
            seen.extend(&buf[..n]);
        }
        let mut v = vec![];
        for _ in 0..samples {
            let t = Instant::now();
            pty.write(b"k").unwrap();
            loop {
                let n = rd.read(&mut buf).unwrap();
                if buf[..n].contains(&b'k') {
                    break;
                }
            }
            v.push(t.elapsed());
            pty.write(b"\x7f").unwrap();
            let _ = rd.read(&mut buf).unwrap();
        }
        let _ = rd.flush();
        pty.stop();
        v
    };

    let mut c = d.term_client().await;
    c.open(1, claude("0000e0d2")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let path = d.path.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opened = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let opener = {
        let (stop, opened) = (stop.clone(), opened.clone());
        tokio::spawn(async move {
            let mut o = TClient::connect(&path).await;
            let mut n = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n += 1;
                let sid = format!("0199aaaa-bbbb-4ccc-8ddd-{n:012x}");
                o.open(n, codex(&sid)).await;
                tokio::time::sleep(Duration::from_millis(15)).await;
                o.send(Frame::TermClose { r#ref: n }).await;
                o.drain_for(0.005).await;
                opened.store(n, std::sync::atomic::Ordering::Relaxed);
            }
            n
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // At least `samples`, and for as long as it takes 30 cold opens to run alongside.
    let mut socket = vec![];
    while socket.len() < samples || opened.load(std::sync::atomic::Ordering::Relaxed) < 30 {
        let before = c.output(1).len();
        let t = Instant::now();
        c.type_in(1, b"k").await;
        assert!(
            c.until(5.0, |c| c.output(1)[before..].contains(&b'k'))
                .await
        );
        socket.push(t.elapsed());
        let n = c.output(1).len();
        c.type_in(1, b"\x7f").await;
        assert!(c.until(5.0, |c| c.output(1).len() > n).await);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let opens = opener.await.unwrap();
    let (dp, sp) = (p95(&direct), p95(&socket));
    eprintln!(
        "echo p95: direct {dp:?}, through fleetd {sp:?} (added {:?}); medians {:?} / {:?}; {opens} cold opens meanwhile",
        sp.saturating_sub(dp),
        median(&direct),
        median(&socket)
    );
    assert!(opens >= 30, "cold opens ran meanwhile: {opens}");
    assert!(sp < Duration::from_millis(50), "{sp:?}");
}

/// Sets a test-only environment variable for the life of the guard (the tests here run one at a time).
struct EnvVar(&'static str);

impl EnvVar {
    fn set(k: &'static str, v: &str) -> EnvVar {
        std::env::set_var(k, v);
        EnvVar(k)
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        std::env::remove_var(self.0);
    }
}

/// P2-R5: a client whose sink is full and that takes nothing for the stall time (30 s; 300 ms here) is closed, and its
/// terminal released. One that reads slowly but steadily is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_takes_nothing_with_a_full_sink_is_closed() {
    let _s = serial().await;
    let _stall = EnvVar::set("FLEETD_TEST_TERM_STALL_MS", "300");
    let d = Daemon::start();

    let mut c = d.term_client().await;
    c.open(1, codex(SID)).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, SID);
    c.type_in(1, b"flood50\n").await;
    // Nothing read: the program is stopped, so the stall closes the connection and the release stops it.
    assert!(
        terms_home::gone_within(pid, Duration::from_secs(5)),
        "released"
    );
    assert!(
        c.until_closed(10.0).await,
        "the server closed the connection"
    );

    // Slow but steady (4 KiB every 50 ms): kept.
    let mut c = d.term_client().await;
    let sid = "0199aaaa-bbbb-4ccc-8ddd-000000000005";
    c.open(1, codex(sid)).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, sid);
    c.type_in(1, b"flood50\n").await;
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        assert!(
            c.r.read(&mut buf).await.unwrap() > 0,
            "closed while reading"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(terms_home::alive(pid), "a slow reader is kept");
}

/// M1: an open still running when the connection ends (its writer failed first) is waited for, and released with the
/// rest: nothing it registers late is left attached to a gone connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_in_flight_when_the_connection_dies_is_released() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000c1")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let _delay = EnvVar::set("FLEETD_TEST_OPEN_DELAY_MS", "800");
    c.type_in(1, b"flood\n").await;
    c.open(2, claude("000000c2")).await;
    // Output is flowing for ref 1; the open of ref 2 is held on the blocking pool. Gone: the writer fails first.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(c);
    let terms = d.terms.clone();
    let both_warm = tokio::task::spawn_blocking(move || {
        terms_home::wait_for(Duration::from_secs(5), || terms.warm_count() == 2)
    })
    .await
    .unwrap();
    assert!(
        both_warm,
        "warm {}, live {} (ref 2 left attached to a gone connection)",
        d.terms.warm_count(),
        d.terms.live_count()
    );
}

/// M2: a refused open of a reused `ref` is answered behind that ref's earlier output and its close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_reopen_of_a_ref_comes_after_its_old_output() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000c3")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    c.type_in(1, b"flood\n").await;
    // Not read for a while: the sink fills with ref 1's output. Then close it and reopen ref 1 on a bad target.
    tokio::time::sleep(Duration::from_millis(500)).await;
    c.send(Frame::TermClose { r#ref: 1 }).await;
    c.open(
        1,
        AttachTarget::Claude {
            profile: "default".into(),
            job: "not a job".into(),
        },
    )
    .await;
    assert!(c.until(20.0, |c| c.ends.len() == 2).await, "{:?}", c.ends);
    c.drain_for(0.3).await;
    let ends: Vec<usize> = (0..c.log.len()).filter(|&i| c.log[i].1.is_some()).collect();
    assert_eq!(ends.len(), 2);
    assert_eq!(c.log[ends[0]], (1, Some(None)), "the close's end first");
    assert_eq!(
        c.log[ends[1]],
        (1, Some(Some(fleetd::terms::ERR_PICK.to_string()))),
        "then the refusal"
    );
    assert!(
        ends[0] > 0 && c.log[..ends[0]].iter().all(|e| *e == (1, None)),
        "output before"
    );
    assert_eq!(ends[1], c.log.len() - 1, "nothing after the refusal");
    assert_eq!(ends[1], ends[0] + 1, "nothing between the two ends");
}

/// M3: a client that hangs up while its input is held, with frames it sent still unread, is noticed: the stall ends and
/// its terminal is released, though the program stays stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hang_up_while_input_is_held_releases_the_terminal() {
    let _s = serial().await;
    let d = Daemon::start();
    let mut c = d.term_client().await;
    c.open(1, claude("000000c4")).await;
    assert!(c.until_has(1, "BANNER", BOOT).await);
    let pid = pid_of(&d.home, "000000c4");
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let per = TERM_IN_MAX / 512;
    let (_, spare) = UnixStream::pair().unwrap().0.into_split();
    let mut w = std::mem::replace(&mut c.w, spare);
    let writer = tokio::spawn(async move {
        for i in 0..24 {
            let data = numbered('L', i * per, per, 512);
            let _ = write_frame(&mut w, &Frame::TermIn { r#ref: 1, data }).await;
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    c.drain_for(1.0).await;
    assert_eq!(d.terms.warm_count(), 0, "attached while held");
    // Gone, with frames it sent still in the server's socket buffer.
    writer.abort();
    let _ = writer.await;
    drop(c);
    let terms = d.terms.clone();
    let released = tokio::task::spawn_blocking(move || {
        terms_home::wait_for(Duration::from_secs(2), || terms.warm_count() == 1)
    })
    .await
    .unwrap();
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(released, "the held stall ended when the client went");
}
