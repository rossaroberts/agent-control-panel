//! File events in, sessions re-read, deltas out. One FSEvents stream (macOS: latency 0, `kFSEventStreamCreateFlagNoDefer`,
//! file events, which is what notify's FSEvents backend sets) or inotify (Linux) over each profile's `fleet-state/`, `jobs/`
//! and `projects/`, and `~/.codex/sessions/`. There is no debounce: each event is handled the moment it comes, and events
//! that arrive while one is handled are taken together in the next pass (a burst coalesces, a single event waits on
//! nothing).
//!
//! - a hook file re-decides that one session, from the roster rows already held;
//! - a transcript advances that one `Tail`; a rollout its `RolloutTail` (and, when it is not known to be held open, the
//!   Codex open set is scanned again);
//! - a `jobs/` change, or a hook that says something new, re-reads that profile's roster on a thread of its own: at most
//!   one run in flight per profile, and any number of requests during it make exactly one more run after it (R4);
//! - every 30 s a reconcile pass re-reads files only (hooks, tails against their fingerprints, pid liveness, the jobs'
//!   records, the open set) and emits only real differences; every 5 minutes it also re-reads each roster (R4).
//!
//! The board (`Fleet`) sits behind one lock that is held only for file reads of a session or two and for the store's
//! write, so a slow roster run (hundreds of ms, outside the lock) never holds up a hook event.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fleet_read::profiles::claude_profiles;
use fleet_read::roster::{default_source, RosterSource};
use fleet_read::transcript::Tail;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::machine::machine_name;
use crate::procscan::codex_open;
use crate::sessions::{jobs_signature, now_ms, transcript_of, Fleet};
use crate::store::Store;

pub const RECONCILE_EVERY: Duration = Duration::from_secs(30);
pub const ROSTER_SAFETY_EVERY: Duration = Duration::from_secs(5 * 60);

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

struct Shared {
    /// The home folder as the file events name it (symlinks resolved).
    home: PathBuf,
    store: Arc<Store>,
    fleet: Mutex<Fleet>,
    roster: Box<dyn RosterSource + Send + Sync>,
    runners: Mutex<HashMap<usize, Runner>>,
    roster_runs: AtomicU64,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched: Mutex<HashSet<PathBuf>>,
    stop: AtomicBool,
    wake: (Mutex<()>, Condvar),
}

/// The running watcher. Dropping it stops the event stream and the reconcile timer.
pub struct WatchHandle {
    shared: Arc<Shared>,
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
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.1.notify_all();
        // Dropping the watcher closes the event channel, which ends the dispatcher.
        self.shared.watcher.lock().unwrap().take();
    }
}

pub fn watch(home: &Path, store: Arc<Store>) -> io::Result<WatchHandle> {
    watch_with(home, store, Options::default())
}

/// Starts watching, and returns once the board holds the first full read (each profile's roster, in parallel, and the
/// recent Codex logs), so a reader connecting now gets a whole snapshot.
pub fn watch_with(home: &Path, store: Arc<Store>, opts: Options) -> io::Result<WatchHandle> {
    let home = home.canonicalize()?;
    let machine = opts
        .machine
        .unwrap_or_else(|| machine_name(&fleet_read::home::home_dir()));
    let fleet = Fleet::new(machine, home.join(".codex"));
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
        watcher: Mutex::new(Some(watcher)),
        watched: Mutex::new(HashSet::new()),
        stop: AtomicBool::new(false),
        wake: (Mutex::new(()), Condvar::new()),
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
        let open = codex_open();
        let now = now_ms();
        let mut f = shared.fleet.lock().unwrap();
        f.open_codex(open, now);
        let cs = f.codex_sessions(now);
        shared.store.replace(f.in_codex(), cs);
    });

    let sh = shared.clone();
    thread::Builder::new()
        .name("fleetd-events".into())
        .spawn(move || dispatch(&sh, rx))?;
    let sh = shared.clone();
    let (every, safety) = (opts.reconcile_every, opts.roster_safety_every);
    thread::Builder::new()
        .name("fleetd-reconcile".into())
        .spawn(move || timer(&sh, every, safety))?;
    Ok(WatchHandle { shared })
}

/// Adds a watch on each folder that exists and is not watched yet (a folder made later is picked up by the reconcile).
fn arm(sh: &Shared) {
    let mut want: Vec<PathBuf> = {
        let f = sh.fleet.lock().unwrap();
        f.profiles
            .iter()
            .flat_map(|p| ["fleet-state", "jobs", "projects"].map(|d| p.profile.dir.join(d)))
            .collect()
    };
    want.push(sh.home.join(".codex").join("sessions"));
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

/// What a changed path is.
enum Route {
    Hook(usize, String),
    Transcript(usize, String),
    Jobs(usize),
    Codex,
}

fn route(sh: &Shared, f: &Fleet, path: &Path) -> Option<Route> {
    let rel = path.strip_prefix(&sh.home).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [".codex", "sessions", .., name] if name.ends_with(".jsonl") => Some(Route::Codex),
        [p, rest @ ..] => {
            let pi = f.profile_named(p)?;
            match rest {
                ["fleet-state", name] => {
                    Some(Route::Hook(pi, name.strip_suffix(".json")?.to_string()))
                }
                ["jobs", ..] => Some(Route::Jobs(pi)),
                ["projects", _, name] => Some(Route::Transcript(
                    pi,
                    name.strip_suffix(".jsonl")?.to_string(),
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
        let mut seen = HashSet::new();
        paths.retain(|p| seen.insert(p.clone()));
        handle(sh, &paths);
    }
}

fn handle(sh: &Arc<Shared>, paths: &[PathBuf]) {
    let routes: Vec<(Route, &PathBuf)> = {
        let f = sh.fleet.lock().unwrap();
        paths
            .iter()
            .filter_map(|p| Some((route(sh, &f, p)?, p)))
            .collect()
    };
    if routes.is_empty() {
        return;
    }
    // A rollout not known to be held open may be one a Codex just opened: scan again, outside the lock (about 1 ms).
    let rescan = {
        let f = sh.fleet.lock().unwrap();
        routes
            .iter()
            .any(|(r, p)| matches!(r, Route::Codex) && !f.codex.open.contains(*p))
    };
    let open = if rescan { codex_open() } else { None };

    let mut rosters = HashSet::new();
    let mut f = sh.fleet.lock().unwrap();
    let now = now_ms();
    let mut touched: Vec<(usize, String)> = vec![];
    let mut codex = false;
    let mut open = open;
    for (r, p) in &routes {
        match r {
            Route::Hook(pi, sid) => {
                let h = f.on_hook(*pi, sid);
                if h.known {
                    touched.push((*pi, sid.clone()));
                }
                if h.moved {
                    rosters.insert(*pi);
                }
            }
            Route::Transcript(pi, sid) => {
                if f.on_transcript(*pi, sid, p) {
                    touched.push((*pi, sid.clone()));
                }
            }
            Route::Jobs(pi) => {
                rosters.insert(*pi);
            }
            Route::Codex => {
                f.on_codex(p, open.take());
                codex = true;
            }
        }
    }
    let mut out: Vec<_> = touched
        .iter()
        .filter_map(|(pi, sid)| f.claude_session(*pi, sid, now))
        .collect();
    if codex {
        let cs = f.codex_sessions(now);
        sh.store.replace(f.in_codex(), cs);
    }
    out.dedup_by(|a, b| a.key == b.key);
    sh.store.upsert(out);
    drop(f);
    for pi in rosters {
        request_roster(sh, pi);
    }
}

/// Asks for a roster read of a profile: one starts now unless one is under way, in which case exactly one more runs after it.
fn request_roster(sh: &Arc<Shared>, pi: usize) {
    {
        let mut rs = sh.runners.lock().unwrap();
        let r = rs.entry(pi).or_default();
        if r.in_flight {
            r.again = true;
            return;
        }
        r.in_flight = true;
    }
    let sh = sh.clone();
    let spawned = thread::Builder::new().name("fleetd-roster".into()).spawn({
        let sh = sh.clone();
        move || loop {
            run_roster(&sh, pi);
            let mut rs = sh.runners.lock().unwrap();
            let r = rs.entry(pi).or_default();
            if std::mem::take(&mut r.again) && !sh.stop.load(Ordering::SeqCst) {
                continue;
            }
            r.in_flight = false;
            break;
        }
    });
    if spawned.is_err() {
        sh.runners.lock().unwrap().entry(pi).or_default().in_flight = false;
    }
}

/// One roster read of a profile: the read and the new transcripts' first reads outside the lock, then the board updated.
fn run_roster(sh: &Shared, pi: usize) {
    sh.roster_runs.fetch_add(1, Ordering::SeqCst);
    let profile = {
        let mut f = sh.fleet.lock().unwrap();
        let ps = &mut f.profiles[pi];
        ps.jobs_sig = jobs_signature(&ps.profile);
        ps.profile.clone()
    };
    let read = sh.roster.read(&profile);
    let need = match &read {
        Ok(r) => sh.fleet.lock().unwrap().untailed(pi, r),
        Err(_) => vec![],
    };
    let opened: HashMap<String, Tail> = need
        .into_iter()
        .filter_map(|(sid, cwd)| {
            let t = Tail::open(&transcript_of(&profile, &sid, cwd.as_deref())?).ok()?;
            Some((sid, t))
        })
        .collect();
    let mut f = sh.fleet.lock().unwrap();
    let now = now_ms();
    f.apply_roster(pi, read, opened, now);
    let ss = f.profile_sessions(pi, now);
    sh.store.replace(f.in_profile(pi), ss);
}

/// The reconcile pass: files only, then one write of everything that really changed. `rosters`: also re-read each roster
/// (the 5-minute safety run).
fn reconcile(sh: &Arc<Shared>, rosters: bool) {
    let found = claude_profiles(&sh.home);
    let open = codex_open();
    let mut wanted = HashSet::new();
    {
        let mut f = sh.fleet.lock().unwrap();
        wanted.extend(f.add_profiles(found));
        let now = now_ms();
        for pi in 0..f.profiles.len() {
            f.reconcile_profile(pi);
            let ps = &f.profiles[pi];
            if rosters || jobs_signature(&ps.profile) != ps.jobs_sig {
                wanted.insert(pi);
            }
        }
        f.reconcile_codex(open, now);
        let all = f.all_sessions(now);
        sh.store.replace(f.in_machine(), all);
    }
    arm(sh);
    for pi in wanted {
        request_roster(sh, pi);
    }
}

fn timer(sh: &Arc<Shared>, every: Duration, safety: Duration) {
    let mut last_safety = Instant::now();
    loop {
        let g = sh.wake.0.lock().unwrap();
        let (_g, _) = sh
            .wake
            .1
            .wait_timeout_while(g, every, |_| !sh.stop.load(Ordering::SeqCst))
            .unwrap();
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
