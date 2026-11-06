//! File events in, sessions re-read, deltas out.
//!
//! - **Hot path** (macOS, Ruling R15): kqueue on each profile's `fleet-state/` and `fleet-status/` folders and on each
//!   followed transcript and rollout (`kq.rs`), handled on its own thread the moment the kernel says so. At most
//!   `MAX_FOLLOWED` files are followed: the live sessions' and the recently written ones'. On Linux inotify (notify's
//!   backend) is already immediate and carries all of it.
//! - **Discovery**: FSEvents (notify's backend: latency 0, `kFSEventStreamCreateFlagNoDefer`) over each profile's
//!   `fleet-state/`, `fleet-status/`, `jobs/` and `projects/` and `~/.codex/sessions/`: new transcripts and rollouts,
//!   jobs, hooks of sessions not listed yet. Its events for files kqueue follows are dropped; a hook or status file it
//!   names is re-read only when its version differs from the one held, so the hot path's work is not done twice.
//!
//! Routing, with no debounce anywhere (what arrives while one pass runs is taken together in the next):
//! - a hook file re-decides that one session, from the roster rows already held; a status file the same;
//! - a transcript advances that one `Tail`; a rollout its `RolloutTail`, and when it is not known to be held open the
//!   process scan runs again on a thread of its own (one in flight, one follow-up);
//! - a `jobs/` change, or a hook that says something new or names a session not listed (R16), re-reads that profile's
//!   roster on a thread of its own: at most one run in flight per profile, and any number of requests during it make
//!   exactly one more run after it (R4);
//! - every 30 s a reconcile pass re-reads files only (hooks, status files, tails against their fingerprints, pid
//!   liveness, the jobs' records, the process scan: Codex's open logs and Muse) and emits only real differences; every 5
//!   minutes it also re-reads each roster (R4);
//! - a transcript write to a session the board shows as needing you also asks for its profile's roster (at most once a
//!   second per profile): a permission given goes on with the tool and no hook, and only the roster says it is busy;
//! - what changes with the clock alone (`decide`'s quiet thresholds, a Codex log's age) is re-decided by one one-shot
//!   timer armed to the nearest such time across sessions (`Fleet::dues`), re-deciding only the sessions whose time
//!   came; a hook that needs you and has been quiet for 3 s also asks for its roster then. With nothing due it sleeps;
//! - on macOS each live session's process is watched for its exit (kqueue `NOTE_EXIT`): a session that crashes is
//!   re-decided at once. On Linux that waits for the reconcile's liveness check.
//!
//! The board (`Fleet`) sits behind one lock, held only to look up handles, to merge values read before and to write the
//! store. Once `watch_with` returns, no file is read and no tail's lock is waited on while it is held: a tail is read
//! under its own lock and publishes its view into the board after, and every other read (hooks, status files, the
//! jobs' signature, transcript lookups, the process scan) is done before taking it. So neither a roster run, a
//! reconcile pass nor a slow tail holds up a hook event. The one exception is the first read inside `watch_with`
//! (the recent Codex logs), made under the lock before any other thread of the watcher runs.
//!
//! A read that fails for any reason but "no such file" (too many open files, permissions) proves nothing: what is held
//! from it stays (`reads::FileRead::Failed`, `reads::gone`), and a transcript that could not be opened is not
//! remembered as missing. The soft open-files limit is raised at start, and the files followed are capped at a
//! quarter of it.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fleet_model::types::State;
use fleet_read::codex::{is_yours, muse_installed, RolloutTail};
use fleet_read::profiles::claude_profiles;
use fleet_read::roster::{default_source, RosterSource};
use fleet_read::transcript::Tail;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::machine::machine_name;
use crate::procscan::scan;
use crate::sessions::{
    follow_cap, gone, jobs_signature, new_rollout, new_tail, now_ms, read_hook_file,
    read_status_file, reconcile_read, sig_of, transcript_of, DueKey, Fleet, RosterGot,
};
use crate::store::Store;

pub const RECONCILE_EVERY: Duration = Duration::from_secs(30);
pub const ROSTER_SAFETY_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long `Drop` waits for roster and scan runs under way (a roster run gives up after 8 s).
const WORKER_GRACE: Duration = Duration::from_secs(10);
/// A transcript write asks for a needing-you session's roster at most this often per profile.
const NUDGE_EVERY: Duration = Duration::from_secs(1);
/// A due time this far in the past is still taken as just come (once): the timer woke late, or the time passed between
/// a session's last decide and the timer's look at it.
const DUE_SLACK_MS: i64 = 5_000;

pub struct Options {
    pub roster: Box<dyn RosterSource + Send + Sync>,
    pub reconcile_every: Duration,
    pub roster_safety_every: Duration,
    /// The machine's name on the board; `None`: Ruling R3's.
    pub machine: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            roster: default_source(),
            reconcile_every: RECONCILE_EVERY,
            roster_safety_every: ROSTER_SAFETY_EVERY,
            machine: None,
        }
    }
}

#[derive(Default)]
struct Runner {
    in_flight: bool,
    again: bool,
}

/// What a background run is for.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Job {
    Roster(usize),
    Scan,
}

struct Shared {
    /// The home folder as the file events name it (symlinks resolved).
    home: PathBuf,
    store: Arc<Store>,
    fleet: Mutex<Fleet>,
    roster: Box<dyn RosterSource + Send + Sync>,
    runners: Mutex<HashMap<Job, Runner>>,
    roster_runs: AtomicU64,
    scan_runs: AtomicU64,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched: Mutex<HashSet<PathBuf>>,
    #[cfg(target_os = "macos")]
    kq: crate::kq::Kq,
    /// Per folder kqueue watches: each entry's inode as last seen, so a rename over one shows as a change.
    #[cfg(target_os = "macos")]
    entries: Mutex<HashMap<PathBuf, HashMap<String, u64>>>,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
    /// The one-shot timer's wait, poked when the board changed in a way that may move a due time.
    alarm: crate::alarm::Alarm,
    /// When each profile's roster was last asked for by a transcript write (F3's nudge).
    nudged: Mutex<HashMap<usize, Instant>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// The running watcher. Dropping it stops the streams and the timer, and waits for its threads.
pub struct WatchHandle {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl WatchHandle {
    /// Runs a reconcile pass now and returns when it is done.
    pub fn reconcile(&self) {
        reconcile(&self.shared, false);
    }

    /// Roster runs started since the watcher began (the first reads included).
    pub fn roster_runs(&self) -> u64 {
        self.shared.roster_runs.load(Ordering::SeqCst)
    }

    /// Process scans run by events (not the reconcile's or the first one).
    pub fn scan_runs(&self) -> u64 {
        self.shared.scan_runs.load(Ordering::SeqCst)
    }

    /// Test seam: holds the lock of a session's tail for `d` on a thread of its own, as a slow read would. Whether the
    /// session has a tail.
    #[doc(hidden)]
    pub fn hold_tail(&self, profile: &str, sid: &str, d: Duration) -> bool {
        let t = {
            let f = self.shared.fleet.lock().unwrap();
            f.profiles
                .iter()
                .find(|p| p.profile.id == profile)
                .and_then(|p| p.sessions.get(sid))
                .and_then(|c| c.tail.clone())
        };
        let Some(t) = t else { return false };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(());
            t.hold(d);
        });
        let _ = rx.recv();
        thread::sleep(Duration::from_millis(5)); // let it take the lock
        true
    }

    /// A running Codex holds thread `session_id` open (the open set of the last process scan that worked, R14). Takes
    /// the board lock only for an in-memory look.
    pub fn codex_held(&self, session_id: &str) -> bool {
        // Asked under the terminals' lock: a poisoned board must not take the terminals down with it.
        self.shared
            .fleet
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .codex_held(session_id)
    }

    /// Processes watched for their exit now (macOS; 0 elsewhere).
    pub fn watched_pids(&self) -> usize {
        #[cfg(target_os = "macos")]
        return self.shared.kq.pids();
        #[cfg(not(target_os = "macos"))]
        0
    }

    /// Files the hot path follows now.
    pub fn followed(&self) -> usize {
        #[cfg(target_os = "macos")]
        return self.shared.kq.followed();
        #[cfg(not(target_os = "macos"))]
        0
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        {
            // Under the timer's lock, so its wait cannot miss it.
            let _g = self.shared.wake.0.lock().unwrap();
            self.shared.stop.store(true, Ordering::SeqCst);
            self.shared.wake.1.notify_all();
        }
        self.shared.alarm.poke();
        // Dropping the watcher closes the event channel, which ends the dispatcher.
        self.shared.watcher.lock().unwrap().take();
        #[cfg(target_os = "macos")]
        self.shared.kq.wake();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let workers: Vec<_> = self.shared.workers.lock().unwrap().drain(..).collect();
        let end = Instant::now() + WORKER_GRACE;
        for w in workers {
            while !w.is_finished() && Instant::now() < end {
                thread::sleep(Duration::from_millis(5));
            }
            if w.is_finished() {
                let _ = w.join();
            }
        }
    }
}

/// Raises the soft open-files limit to `min(hard, 8192)` (launchd starts a daemon at 256): every followed file holds a
/// descriptor, and the reads the board depends on need their own. The soft and hard limits in force after.
pub fn raise_open_files() -> (u64, u64) {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `r` is a plain struct the call fills.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return (MAX_FOLLOWED_FALLBACK, MAX_FOLLOWED_FALLBACK);
    }
    let want = r.rlim_max.min(8192);
    if r.rlim_cur < want {
        let up = libc::rlimit {
            rlim_cur: want,
            rlim_max: r.rlim_max,
        };
        // SAFETY: as above; a refusal leaves the limit as it was.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &up) } == 0 {
            r.rlim_cur = want;
        }
    }
    (r.rlim_cur, r.rlim_max)
}

/// The limit assumed when it cannot be read: launchd's default.
const MAX_FOLLOWED_FALLBACK: u64 = 256;

pub fn watch(home: &Path, store: Arc<Store>) -> io::Result<WatchHandle> {
    watch_with(home, store, Options::default())
}

/// Starts watching, and returns once the board holds the first full read (each profile's roster, in parallel, the
/// recent Codex logs and the running Muse), so a reader connecting now gets a whole snapshot.
pub fn watch_with(home: &Path, store: Arc<Store>, opts: Options) -> io::Result<WatchHandle> {
    let home = home.canonicalize()?;
    let machine = opts
        .machine
        .unwrap_or_else(|| machine_name(&fleet_read::home::home_dir()));
    let mut fleet = Fleet::new(machine, home.join(".codex"));
    fleet.muse_on = muse_installed(&home);
    let (soft, hard) = raise_open_files();
    fleet.max_followed = follow_cap(soft);
    eprintln!(
        "fleetd: open files limit {soft} (hard {}), following at most {} files",
        if hard == libc::RLIM_INFINITY {
            "unlimited".to_string()
        } else {
            hard.to_string()
        },
        fleet.max_followed
    );
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    let watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev {
            if !matches!(ev.kind, EventKind::Access(_)) {
                let _ = tx.send(ev.paths);
            }
        }
    })
    .map_err(io::Error::other)?;
    let shared = Arc::new(Shared {
        home,
        store,
        fleet: Mutex::new(fleet),
        roster: opts.roster,
        runners: Mutex::new(HashMap::new()),
        roster_runs: AtomicU64::new(0),
        scan_runs: AtomicU64::new(0),
        watcher: Mutex::new(Some(watcher)),
        watched: Mutex::new(HashSet::new()),
        #[cfg(target_os = "macos")]
        kq: crate::kq::Kq::new()?,
        #[cfg(target_os = "macos")]
        entries: Mutex::new(HashMap::new()),
        stop: AtomicBool::new(false),
        wake: (Mutex::new(()), Condvar::new()),
        alarm: crate::alarm::Alarm::new()?,
        nudged: Mutex::new(HashMap::new()),
        workers: Mutex::new(vec![]),
    });

    // Watch first, then read: nothing written in between is missed.
    let added = shared
        .fleet
        .lock()
        .unwrap()
        .add_profiles(claude_profiles(&shared.home));
    arm(&shared);
    thread::scope(|s| {
        for &pi in &added {
            let sh = &shared;
            s.spawn(move || run_roster(sh, pi));
        }
        let found = scan();
        let now = now_ms();
        let mut f = shared.fleet.lock().unwrap();
        f.open_codex(found, now);
        let os = f.others(now);
        shared.store.replace(f.in_others(), os);
    });
    sync_follows(&shared);

    let mut threads = vec![];
    let sh = shared.clone();
    threads.push(
        thread::Builder::new()
            .name("fleetd-events".into())
            .spawn(move || dispatch(&sh, rx))?,
    );
    #[cfg(target_os = "macos")]
    {
        let sh = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("fleetd-kqueue".into())
                .spawn(move || kq_loop(&sh))?,
        );
    }
    let sh = shared.clone();
    let (every, safety) = (opts.reconcile_every, opts.roster_safety_every);
    threads.push(
        thread::Builder::new()
            .name("fleetd-reconcile".into())
            .spawn(move || timer(&sh, every, safety))?,
    );
    let sh = shared.clone();
    threads.push(
        thread::Builder::new()
            .name("fleetd-due".into())
            .spawn(move || due_loop(&sh))?,
    );
    Ok(WatchHandle { shared, threads })
}

/// Adds a watch on each folder that exists and is not watched yet (a folder made later is picked up by the reconcile),
/// and on macOS the kqueue watch of each hook and status folder.
fn arm(sh: &Shared) {
    let profiles: Vec<PathBuf> = {
        let f = sh.fleet.lock().unwrap();
        f.profiles.iter().map(|p| p.profile.dir.clone()).collect()
    };
    let mut want: Vec<PathBuf> = profiles
        .iter()
        .flat_map(|d| ["fleet-state", "fleet-status", "jobs", "projects"].map(|s| d.join(s)))
        .collect();
    want.push(sh.home.join(".codex").join("sessions"));
    #[cfg(target_os = "macos")]
    for d in profiles
        .iter()
        .flat_map(|d| ["fleet-state", "fleet-status"].map(|s| d.join(s)))
    {
        let new = !sh.entries.lock().unwrap().contains_key(&d);
        if sh.kq.watch_dir(&d) && new {
            let seen = entries_of(&d);
            sh.entries.lock().unwrap().entry(d).or_insert(seen);
        }
    }
    let mut watched = sh.watched.lock().unwrap();
    let new: Vec<PathBuf> = want
        .into_iter()
        .filter(|d| !watched.contains(d) && d.is_dir())
        .collect();
    if new.is_empty() {
        return;
    }
    let mut w = sh.watcher.lock().unwrap();
    let Some(w) = w.as_mut() else { return };
    let mut paths = w.paths_mut();
    for d in new {
        if paths.add(&d, RecursiveMode::Recursive).is_ok() {
            watched.insert(d);
        }
    }
    let _ = paths.commit();
}

/// A folder's `*.json` entries and their inodes (from the directory itself: no stat per file).
#[cfg(target_os = "macos")]
fn entries_of(dir: &Path) -> HashMap<String, u64> {
    use std::os::unix::fs::DirEntryExt;
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            (n.ends_with(".json") && !n.starts_with('.')).then(|| (n, e.ino()))
        })
        .collect()
}

/// What a changed path is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Route {
    Hook(usize, String),
    Status(usize, String),
    Transcript(usize, String, PathBuf),
    Jobs(usize),
    Codex(PathBuf),
}

fn route(home: &Path, f: &Fleet, path: &Path) -> Option<Route> {
    let rel = path.strip_prefix(home).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [".codex", "sessions", .., name] if name.ends_with(".jsonl") => {
            Some(Route::Codex(path.to_path_buf()))
        }
        [p, rest @ ..] => {
            let pi = f.profile_named(p)?;
            let json = |n: &str| {
                n.strip_suffix(".json")
                    .filter(|s| !s.starts_with('.'))
                    .map(str::to_string)
            };
            match rest {
                ["fleet-state", name] => Some(Route::Hook(pi, json(name)?)),
                ["fleet-status", name] => Some(Route::Status(pi, json(name)?)),
                ["jobs", ..] => Some(Route::Jobs(pi)),
                ["projects", _, name] => Some(Route::Transcript(
                    pi,
                    name.strip_suffix(".jsonl")?.to_string(),
                    path.to_path_buf(),
                )),
                _ => None,
            }
        }
        _ => None,
    }
}

fn dispatch(sh: &Arc<Shared>, rx: mpsc::Receiver<Vec<PathBuf>>) {
    while let Ok(first) = rx.recv() {
        // Whatever else is already waiting goes in the same pass: no waiting for more.
        let mut paths = first;
        while let Ok(more) = rx.try_recv() {
            paths.extend(more);
        }
        let routes: Vec<Route> = {
            let f = sh.fleet.lock().unwrap();
            paths
                .iter()
                .filter_map(|p| route(&sh.home, &f, p))
                .collect()
        };
        // kqueue has these: FSEvents is only discovery for them.
        #[cfg(target_os = "macos")]
        let routes: Vec<Route> = routes
            .into_iter()
            .filter(|r| match r {
                Route::Transcript(_, _, p) | Route::Codex(p) => !sh.kq.is_followed(p),
                _ => true,
            })
            .collect();
        handle(sh, routes);
    }
}

/// One pass over routed events: hooks and status files first (the hot path), then transcripts, rollouts, rosters.
fn handle(sh: &Arc<Shared>, routes: Vec<Route>) {
    let mut seen = HashSet::new();
    let mut hooks: HashMap<usize, Vec<String>> = HashMap::new();
    let mut statuses: HashMap<usize, Vec<String>> = HashMap::new();
    let mut transcripts = vec![];
    let mut rollouts = vec![];
    for r in routes {
        if !seen.insert(r.clone()) {
            continue;
        }
        match r {
            Route::Hook(pi, sid) => hooks.entry(pi).or_default().push(sid),
            Route::Status(pi, sid) => statuses.entry(pi).or_default().push(sid),
            Route::Transcript(pi, sid, p) => transcripts.push((pi, sid, p)),
            Route::Jobs(pi) => request(sh, Job::Roster(pi)),
            Route::Codex(p) => rollouts.push(p),
        }
    }
    for (pi, sids) in hooks {
        on_hooks(sh, pi, sids);
    }
    for (pi, sids) in statuses {
        on_statuses(sh, pi, sids);
    }
    if !transcripts.is_empty() {
        on_transcripts(sh, transcripts);
    }
    if !rollouts.is_empty() {
        on_rollouts(sh, rollouts);
    }
}

/// Hook files changed: those whose version differs from the one held are read (without the board), merged, and their
/// sessions re-decided at once. A hook that moved, or of a session not listed, asks for a roster read (R16).
fn on_hooks(sh: &Arc<Shared>, pi: usize, sids: Vec<String>) {
    let (profile, held) = {
        let f = sh.fleet.lock().unwrap();
        (f.profiles[pi].profile.clone(), f.hook_sigs(pi, &sids))
    };
    let reads: Vec<_> = sids
        .into_iter()
        .zip(held)
        .filter(|(sid, h)| match h {
            Some(held) => sig_of(&crate::sessions::hook_path(&profile, sid)) != *held,
            None => true,
        })
        .map(|(sid, _)| {
            let r = read_hook_file(&profile, &sid);
            (sid, r)
        })
        .collect();
    if reads.is_empty() {
        return;
    }
    let mut roster = false;
    {
        let mut f = sh.fleet.lock().unwrap();
        let now = now_ms();
        let mut out = vec![];
        for (sid, r) in reads {
            let h = f.merge_hook(pi, &sid, r);
            roster |= h.moved;
            if h.changed {
                out.extend(f.claude_session(pi, &sid, now));
            }
        }
        sh.store.upsert(out);
    }
    poke(sh);
    if roster {
        request(sh, Job::Roster(pi));
    }
}

/// Status files changed: as hooks, without a roster read.
fn on_statuses(sh: &Arc<Shared>, pi: usize, sids: Vec<String>) {
    let (profile, held) = {
        let f = sh.fleet.lock().unwrap();
        (f.profiles[pi].profile.clone(), f.status_sigs(pi, &sids))
    };
    let reads: Vec<_> = sids
        .into_iter()
        .zip(held)
        .filter_map(|(sid, h)| {
            let held = h?; // a session not listed has no row to show it on
            (sig_of(&crate::sessions::status_path(&profile, &sid)) != held).then(|| {
                let r = read_status_file(&profile, &sid);
                (sid, r)
            })
        })
        .collect();
    if reads.is_empty() {
        return;
    }
    let mut f = sh.fleet.lock().unwrap();
    let now = now_ms();
    let mut out = vec![];
    for (sid, r) in reads {
        if f.merge_status(pi, &sid, r) {
            out.extend(f.claude_session(pi, &sid, now));
        }
    }
    sh.store.upsert(out);
}

/// Transcripts changed: each listed session's `Tail` advances under its own lock, or a new transcript's first read is
/// made without the board; then the board takes them in.
fn on_transcripts(sh: &Arc<Shared>, items: Vec<(usize, String, PathBuf)>) {
    let looked: Vec<_> = {
        let f = sh.fleet.lock().unwrap();
        items
            .into_iter()
            .filter_map(|(pi, sid, p)| Some((f.tail_for(pi, &sid, &p)?, pi, sid, p)))
            .collect()
    };
    let mut failed = vec![];
    let mut views = vec![];
    let mut opened = vec![];
    let mut touched = vec![];
    for (tail, pi, sid, p) in looked {
        match tail {
            Some(t) => {
                let (r, seen) = t.run(|t| t.advance());
                if gone(&r) {
                    failed.push((pi, sid.clone(), t));
                } else {
                    views.push((pi, sid.clone(), t, seen));
                }
                touched.push((pi, sid));
            }
            None => {
                if let Ok(t) = Tail::open(&p) {
                    opened.push((pi, sid.clone(), p, new_tail(t)));
                    touched.push((pi, sid));
                }
            }
        }
    }
    let mut follow = vec![];
    let mut needs_you = HashSet::new();
    {
        let mut f = sh.fleet.lock().unwrap();
        for (pi, sid, t) in &failed {
            f.drop_tail(*pi, sid, t);
        }
        for (pi, sid, t, seen) in views {
            f.take_view(pi, &sid, &t, seen);
        }
        for (pi, sid, p, t) in opened {
            if f.install_tail(pi, &sid, t) {
                follow.push(p);
            }
        }
        let now = now_ms();
        let mut out = vec![];
        for (pi, sid) in &touched {
            if let Some(s) = f.claude_session(*pi, sid, now) {
                // A roster that already says busy has the permission given (Ruling R25): nothing to ask it.
                let busy = f.profiles[*pi]
                    .sessions
                    .get(sid)
                    .is_some_and(|c| c.row.status.as_deref() == Some("busy"));
                if s.state == State::NeedsYou && !busy {
                    needs_you.insert(*pi);
                }
                out.push(s);
            }
        }
        sh.store.upsert(out);
    }
    #[cfg(target_os = "macos")]
    for p in follow {
        sh.kq.follow(&p);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = follow;
    // Written to while it needs you: the roster's word on it (waiting, or busy once a permission is given) may be old.
    for pi in needs_you {
        nudge(sh, pi);
    }
}

/// Asks for a profile's roster on a transcript write, at most once per `NUDGE_EVERY`; one asked for less than that
/// after the last is left to the due timer's run (a quiet hook that needs you asks again at 3 s).
fn nudge(sh: &Arc<Shared>, pi: usize) {
    {
        let mut n = sh.nudged.lock().unwrap();
        if n.get(&pi).is_some_and(|t| t.elapsed() < NUDGE_EVERY) {
            return;
        }
        n.insert(pi, Instant::now());
    }
    request(sh, Job::Roster(pi));
}

/// Rollouts changed: each followed one advances under its own lock, a new one of yours is opened without the board.
/// One not known to be held open asks for a process scan, which runs on its own thread: this pass does not wait for it.
fn on_rollouts(sh: &Arc<Shared>, paths: Vec<PathBuf>) {
    let (looked, unknown_open): (Vec<_>, bool) = {
        let f = sh.fleet.lock().unwrap();
        let unknown = paths.iter().any(|p| !f.codex.open.contains(p));
        (
            paths
                .into_iter()
                .filter(|p| f.under_sessions(p))
                .map(|p| (f.rollout(&p), p))
                .collect(),
            unknown,
        )
    };
    let mut failed = vec![];
    let mut views = vec![];
    let mut opened = vec![];
    for (t, p) in looked {
        match t {
            Some(t) => {
                let (r, seen) = t.run(|t| t.advance());
                if gone(&r) {
                    failed.push((p, t));
                } else {
                    views.push((p, t, seen));
                }
            }
            None => {
                if p.is_file() && is_yours(&p) {
                    if let Ok(t) = RolloutTail::open(&p) {
                        opened.push((p, new_rollout(t)));
                    }
                }
            }
        }
    }
    let mut follow = vec![];
    {
        let mut f = sh.fleet.lock().unwrap();
        for (p, t) in &failed {
            f.drop_rollout(p, t);
        }
        for (p, t, seen) in views {
            f.take_rollout_view(&p, &t, seen);
        }
        for (p, t) in opened {
            follow.push(p.clone());
            f.install_rollout(p, t);
        }
        let now = now_ms();
        let os = f.others(now);
        sh.store.replace(f.in_others(), os);
    }
    poke(sh);
    #[cfg(target_os = "macos")]
    for p in follow {
        sh.kq.follow(&p);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = follow;
    if unknown_open {
        request(sh, Job::Scan);
    }
}

#[cfg(target_os = "macos")]
fn kq_loop(sh: &Arc<Shared>) {
    use crate::kq::Target;
    while !sh.stop.load(Ordering::SeqCst) {
        let events = sh.kq.wait();
        let mut routes = vec![];
        let mut exited = vec![];
        for t in events {
            match t {
                Target::Dir(d) => {
                    // Which entries changed: a rename over a file is a new inode.
                    let now = entries_of(&d);
                    let changed: Vec<String> = {
                        let mut all = sh.entries.lock().unwrap();
                        let was = all.entry(d.clone()).or_default();
                        let ch = now
                            .iter()
                            .filter(|(n, ino)| was.get(*n) != Some(ino))
                            .map(|(n, _)| n.clone())
                            .collect();
                        *was = now;
                        ch
                    };
                    let f = sh.fleet.lock().unwrap();
                    routes.extend(
                        changed
                            .iter()
                            .filter_map(|n| route(&sh.home, &f, &d.join(n))),
                    );
                }
                Target::File(p) => {
                    let f = sh.fleet.lock().unwrap();
                    routes.extend(route(&sh.home, &f, &p));
                }
                Target::Pid(p) => exited.push(p),
            }
        }
        if !exited.is_empty() {
            on_exits(sh, exited);
        }
        if !routes.is_empty() {
            handle(sh, routes);
        }
    }
}

/// Processes that exited: each session held as alive on one is re-decided at once, and its profile's roster asked for
/// (a session Claude Code still lists as running stays open until the roster says it is not: R7).
fn on_exits(sh: &Arc<Shared>, pids: Vec<u32>) {
    let mut profiles = HashSet::new();
    {
        let mut f = sh.fleet.lock().unwrap();
        let now = now_ms();
        let mut out = vec![];
        for p in pids {
            for (pi, sid) in f.pid_exited(p) {
                out.extend(f.claude_session(pi, &sid, now));
                profiles.insert(pi);
            }
        }
        sh.store.upsert(out);
    }
    poke(sh);
    for pi in profiles {
        request(sh, Job::Roster(pi));
    }
}

/// Follows exactly the files the board says are worth it (live or recent sessions, newest first, capped), and watches
/// exactly the live sessions' processes; then lets the due timer look again (what was read may move a due time).
fn sync_follows(sh: &Arc<Shared>) {
    #[cfg(target_os = "macos")]
    {
        let (want, pids) = {
            let f = sh.fleet.lock().unwrap();
            (f.follows(now_ms()), f.live_pids())
        };
        sh.kq.set_follows(&want);
        // Gone before it could be watched; a zombie (still answering `kill 0`) is left to the reconcile, as before.
        let gone: Vec<u32> = sh
            .kq
            .set_pids(&pids)
            .into_iter()
            .filter(|p| !fleet_read::roster::pid_alive(*p))
            .collect();
        if !gone.is_empty() {
            on_exits(sh, gone);
        }
    }
    poke(sh);
}

/// Wakes the due timer to look again.
fn poke(sh: &Shared) {
    sh.alarm.poke();
}

/// The one-shot timer: sleeps until the nearest due time across sessions (or a poke), re-decides only the sessions whose
/// time came (each due time once), asks for the roster of a quiet hook that needs you, and arms again. With nothing due
/// it sleeps until poked: idle is idle.
fn due_loop(sh: &Arc<Shared>) {
    // The due times already acted on, so one in the slack window is acted on once.
    let mut handled: HashMap<DueKey, i64> = HashMap::new();
    while !sh.stop.load(Ordering::SeqCst) {
        let now = now_ms();
        let mut rosters = HashSet::new();
        let next = {
            let f = sh.fleet.lock().unwrap();
            let dues = f.dues(now - DUE_SLACK_MS);
            let (mut out, mut codex, mut next) = (vec![], false, None::<i64>);
            let mut keep = HashMap::new();
            for d in dues {
                if d.at > now {
                    next = Some(next.map_or(d.at, |n| n.min(d.at)));
                    continue;
                }
                if handled.get(&d.key) != Some(&d.at) {
                    match &d.key {
                        DueKey::Claude(pi, sid) => {
                            out.extend(f.claude_session(*pi, sid, now));
                            if d.roster {
                                rosters.insert(*pi);
                            }
                        }
                        DueKey::Codex(_) => codex = true,
                    }
                }
                keep.insert(d.key, d.at);
            }
            handled = keep;
            sh.store.upsert(out);
            if codex {
                let os = f.others(now);
                sh.store.replace(f.in_others(), os);
            }
            next
        };
        for pi in rosters {
            request(sh, Job::Roster(pi));
        }
        if sh.stop.load(Ordering::SeqCst) {
            return;
        }
        let after = next.map(|at| Duration::from_millis((at - now_ms()).max(0) as u64));
        sh.alarm.wait(after);
    }
}

/// Asks for a background run: one starts now unless one is under way, in which case exactly one more runs after it.
fn request(sh: &Arc<Shared>, job: Job) {
    {
        let mut rs = sh.runners.lock().unwrap();
        let r = rs.entry(job).or_default();
        if r.in_flight {
            r.again = true;
            return;
        }
        r.in_flight = true;
    }
    let s = sh.clone();
    let spawned = thread::Builder::new()
        .name("fleetd-worker".into())
        .spawn(move || loop {
            match job {
                Job::Roster(pi) => run_roster(&s, pi),
                Job::Scan => run_scan(&s),
            }
            let mut rs = s.runners.lock().unwrap();
            let r = rs.entry(job).or_default();
            if std::mem::take(&mut r.again) && !s.stop.load(Ordering::SeqCst) {
                continue;
            }
            r.in_flight = false;
            break;
        });
    match spawned {
        Ok(h) => {
            let mut w = sh.workers.lock().unwrap();
            w.retain(|h| !h.is_finished());
            w.push(h);
        }
        Err(_) => {
            sh.runners.lock().unwrap().entry(job).or_default().in_flight = false;
        }
    }
}

/// One roster read of a profile: the read, the new transcripts' first reads and every listed session's hook and status
/// file outside the lock, then the board updated.
fn run_roster(sh: &Arc<Shared>, pi: usize) {
    sh.roster_runs.fetch_add(1, Ordering::SeqCst);
    let profile = sh.fleet.lock().unwrap().profiles[pi].profile.clone();
    // The jobs' records as this run starts, read without the board: the reconcile compares against it.
    let jobs_sig = jobs_signature(&profile);
    sh.fleet.lock().unwrap().profiles[pi].jobs_sig = jobs_sig;
    let read = sh.roster.read(&profile);
    let mut got = RosterGot::default();
    if let Ok(r) = &read {
        let need = sh.fleet.lock().unwrap().untailed(pi, r);
        for (sid, cwd) in need {
            // Only "there is none" is remembered as a miss: a transcript that could not be opened is looked for again.
            match transcript_of(&profile, &sid, cwd.as_deref()) {
                Some(f) => {
                    if let Ok(t) = Tail::open(&f) {
                        got.opened.insert(sid, t);
                    }
                }
                None => {
                    got.missed.insert(sid);
                }
            }
        }
        for sid in r.rows.iter().filter_map(|r| r.session_id.as_deref()) {
            got.hooks.insert(sid.into(), read_hook_file(&profile, sid));
            got.statuses
                .insert(sid.into(), read_status_file(&profile, sid));
        }
    }
    {
        let mut f = sh.fleet.lock().unwrap();
        let now = now_ms();
        f.apply_roster(pi, read, got, now);
        let ss = f.profile_sessions(pi, now);
        sh.store.replace(f.in_profile(pi), ss);
    }
    sync_follows(sh);
}

/// One process scan (Codex's open logs, Muse), off the event threads: logs newly held open are opened without the
/// board, then Codex and Muse re-decided.
fn run_scan(sh: &Arc<Shared>) {
    sh.scan_runs.fetch_add(1, Ordering::SeqCst);
    let Some(found) = scan() else {
        return; // R14: keep the last good result
    };
    let new = sh.fleet.lock().unwrap().held_untracked(&found.codex_open);
    let opened: Vec<_> = new
        .into_iter()
        .filter_map(|f| Some((f.clone(), new_rollout(RolloutTail::open(&f).ok()?))))
        .collect();
    {
        let mut f = sh.fleet.lock().unwrap();
        for (p, t) in opened {
            f.install_rollout(p, t);
        }
        f.take_scan(Some(found));
        let now = now_ms();
        let os = f.others(now);
        sh.store.replace(f.in_others(), os);
    }
    sync_follows(sh);
}

/// The reconcile pass: a plan taken under the lock, every read without it, the results merged, then one write of
/// everything that really changed. `rosters`: also re-read each roster (the 5-minute safety run).
fn reconcile(sh: &Arc<Shared>, rosters: bool) {
    let found = claude_profiles(&sh.home);
    let added = sh.fleet.lock().unwrap().add_profiles(found);
    #[cfg(target_os = "macos")]
    let plan = sh
        .fleet
        .lock()
        .unwrap()
        .reconcile_plan(|p| sh.kq.is_followed(p));
    #[cfg(not(target_os = "macos"))]
    let plan = sh.fleet.lock().unwrap().reconcile_plan(|_| false);
    let got = reconcile_read(&plan, scan(), muse_installed(&sh.home), now_ms());
    let mut wanted: HashSet<usize> = added.into_iter().collect();
    {
        let mut f = sh.fleet.lock().unwrap();
        let now = now_ms();
        wanted.extend(f.merge_reconcile(got));
        if rosters {
            wanted.extend(0..f.profiles.len());
        }
        let all = f.all_sessions(now);
        sh.store.replace(f.in_machine(), all);
    }
    arm(sh);
    sync_follows(sh);
    for pi in wanted {
        request(sh, Job::Roster(pi));
    }
}

fn timer(sh: &Arc<Shared>, every: Duration, safety: Duration) {
    // Test seam, debug builds only: `FLEETD_TEST_PANIC=<ms>` makes this thread panic that long after start, holding the
    // board's lock, as a bug would (tests/panic.rs: the process must exit, not serve a poisoned board).
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("FLEETD_TEST_PANIC")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        thread::sleep(Duration::from_millis(ms));
        let _board = sh.fleet.lock().unwrap();
        panic!("FLEETD_TEST_PANIC");
    }
    let mut last_safety = Instant::now();
    loop {
        {
            let g = sh.wake.0.lock().unwrap();
            let _ = sh
                .wake
                .1
                .wait_timeout_while(g, every, |_| !sh.stop.load(Ordering::SeqCst))
                .unwrap();
        }
        if sh.stop.load(Ordering::SeqCst) {
            return;
        }
        let rosters = last_safety.elapsed() >= safety;
        if rosters {
            last_safety = Instant::now();
        }
        reconcile(sh, rosters);
    }
}
