//! What the daemon holds about each session between reads, and the one `Session` built from it: for Claude Code, the
//! profile's last roster row, the transcript's `Tail`, the last hook event and the status-line snapshot, decided by
//! `decide` (port of the session row `claudeProfile` builds in fleet/machine.mjs); for Codex, each rollout's
//! `RolloutTail` and whether a running Codex holds it open; for Muse, its running processes.
//!
//! The `Fleet` methods do no file I/O: they are what runs under the watcher's board lock. Reads happen before, unlocked
//! (`read_hook_file`, `reconcile_read`, `Tail::open`), and come in as values. A tail sits behind its own lock, so it is
//! advanced without the board. Two reads of one small file that race keep the newer one (`newer`): a slow read never
//! overwrites what a faster one already put in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_model::decide::{decide, hh_mm_utc, RosterRow};
use fleet_model::hook::hook_state;
use fleet_model::types::{Engine, HookEvent, Session, SessionKey};
use fleet_read::codex::{muse_session, recent_rollouts, RolloutTail, RECENT_CODEX_MS};
use fleet_read::hookfile::read_hook;
use fleet_read::profiles::Profile;
use fleet_read::roster::{pid_alive, RosterError, RosterRead};
use fleet_read::statusfile::{context_tokens, read_status, StatusSnap};
use fleet_read::transcript::Tail;

use crate::procscan::{MuseProc, Scan};

/// Rollouts followed at most from the recent list (`codexHome` slices to 60); logs a running Codex holds open come on top.
const CODEX_RECENT: usize = 60;
/// Files the hot path follows at most (kqueue fds): the live sessions' and the recently written ones', newest first.
pub const MAX_FOLLOWED: usize = 256;
/// A closed session's transcript is still followed this long after its last write.
const FOLLOW_RECENT_MS: i64 = 60 * 60_000;

/// A followed file's tail behind its own lock, with the file's path readable without it.
pub struct TailCell<T> {
    path: PathBuf,
    cell: Mutex<T>,
}

impl<T> TailCell<T> {
    pub fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, T>> {
        self.cell.lock()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub type SharedTail = Arc<TailCell<Tail>>;
pub type SharedRollout = Arc<TailCell<RolloutTail>>;

fn new_tail(t: Tail) -> SharedTail {
    Arc::new(TailCell {
        path: t.path().to_path_buf(),
        cell: Mutex::new(t),
    })
}

fn new_rollout(t: RolloutTail) -> SharedRollout {
    Arc::new(TailCell {
        path: t.path().to_path_buf(),
        cell: Mutex::new(t),
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `~` for the home folder, as `tilde` does.
fn tilde(p: &str) -> String {
    let home = fleet_read::home::home_dir().to_string_lossy().into_owned();
    match p.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// Where a session's transcript is (`transcriptOf`): `projects/<its folder, every character but A-Za-z0-9 a dash>/<id>.jsonl`,
/// else the first project folder that has `<id>.jsonl`. JS replaces per UTF-16 unit, so a character outside the BMP is
/// two dashes.
pub fn transcript_of(p: &Profile, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    let root = p.dir.join("projects");
    let file = format!("{session_id}.jsonl");
    if let Some(cwd) = cwd {
        let mut d = String::with_capacity(cwd.len());
        for c in cwd.chars() {
            if c.is_ascii_alphanumeric() {
                d.push(c);
            } else {
                d.extend(std::iter::repeat_n('-', c.len_utf16()));
            }
        }
        let f = root.join(d).join(&file);
        if f.is_file() {
            return Some(f);
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|f| f.is_file())
}

/// What `jobs/*/state.json` look like (name, modified, length): a change the watcher missed shows as a different one.
pub fn jobs_signature(p: &Profile) -> Vec<(String, i128, u64)> {
    let Ok(rd) = std::fs::read_dir(p.dir.join("jobs")) else {
        return vec![];
    };
    let mut sig: Vec<(String, i128, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let s = sig_of(&e.path().join("state.json"))?;
            Some((
                e.file_name().to_string_lossy().into_owned(),
                s.mtime_ns,
                s.len,
            ))
        })
        .collect();
    sig.sort();
    sig
}

// ---------------------------------------------------------------- small files

/// Which version of a file a read saw: a file replaced by a rename has a new inode; one written in place a new time or
/// length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sig {
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
}

pub fn sig_of(f: &Path) -> Option<Sig> {
    let m = std::fs::metadata(f).ok()?;
    Some(Sig {
        ino: m.ino(),
        len: m.len(),
        mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
    })
}

/// A read of a small file: the version seen (stat first, so a file replaced mid-read shows as changed next time) and
/// what it said.
#[derive(Clone, Debug)]
pub struct FileRead<T> {
    pub sig: Option<Sig>,
    pub v: Option<T>,
}

pub fn hook_path(p: &Profile, sid: &str) -> PathBuf {
    p.dir.join("fleet-state").join(format!("{sid}.json"))
}

pub fn status_path(p: &Profile, sid: &str) -> PathBuf {
    p.dir.join("fleet-status").join(format!("{sid}.json"))
}

pub fn read_hook_file(p: &Profile, sid: &str) -> FileRead<HookEvent> {
    let sig = sig_of(&hook_path(p, sid));
    FileRead {
        sig,
        v: sig.and_then(|_| read_hook(p, sid)),
    }
}

pub fn read_status_file(p: &Profile, sid: &str) -> FileRead<StatusSnap> {
    let sig = sig_of(&status_path(p, sid));
    FileRead {
        sig,
        v: sig.and_then(|_| read_status(p, sid)),
    }
}

/// Whether a read of version `new` replaces what came from `old`: another version, not older than it. A file gone
/// replaces anything.
pub fn newer(old: Option<Sig>, new: Option<Sig>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(o), Some(n)) => n != o && n.mtime_ns >= o.mtime_ns,
    }
}

// ---------------------------------------------------------------- the board

pub struct ClaudeSession {
    pub row: RosterRow,
    pub tail: Option<SharedTail>,
    pub hook: Option<HookEvent>,
    pub hook_sig: Option<Sig>,
    pub status: Option<StatusSnap>,
    pub status_sig: Option<Sig>,
    /// The folder a transcript was looked for under and not found: not looked for again until that changes or a
    /// `projects/` event names it.
    pub no_transcript: Option<Option<String>>,
}

impl ClaudeSession {
    fn new(row: RosterRow) -> ClaudeSession {
        ClaudeSession {
            row,
            tail: None,
            hook: None,
            hook_sig: None,
            status: None,
            status_sig: None,
            no_transcript: None,
        }
    }

    fn take_hook(&mut self, r: FileRead<HookEvent>) -> bool {
        if !newer(self.hook_sig, r.sig) {
            return false;
        }
        self.hook_sig = r.sig;
        self.hook = r.v;
        true
    }

    fn take_status(&mut self, r: FileRead<StatusSnap>) -> bool {
        if !newer(self.status_sig, r.sig) {
            return false;
        }
        self.status_sig = r.sig;
        self.status = r.v;
        true
    }
}

pub struct ProfileState {
    pub profile: Profile,
    /// By session id, as the last good roster read listed them.
    pub sessions: BTreeMap<String, ClaudeSession>,
    /// What the last read that could tell said runs (Node's `runningWas`; Ruling R13).
    running_was: Option<HashSet<String>>,
    /// Since when (epoch ms) the roster has failed, while it does.
    pub stale_since: Option<i64>,
    /// `jobs_signature` when the last roster read started.
    pub jobs_sig: Vec<(String, i128, u64)>,
}

impl ProfileState {
    fn new(profile: Profile) -> ProfileState {
        ProfileState {
            profile,
            sessions: BTreeMap::new(),
            running_was: None,
            stale_since: None,
            jobs_sig: vec![],
        }
    }
}

#[derive(Default)]
pub struct CodexState {
    pub home: PathBuf,
    pub tails: BTreeMap<PathBuf, SharedRollout>,
    /// The rollouts running Codex processes hold open, by the last scan that worked (Ruling R14).
    pub open: HashSet<PathBuf>,
}

/// What a hook read did.
#[derive(Debug, Default)]
pub struct Hooked {
    /// The roster lists this session.
    pub known: bool,
    /// The file was another version than the one held.
    pub changed: bool,
    /// What the hook says moved (or the session is not listed): the roster's word on it may now be out of date (R16).
    pub moved: bool,
}

pub struct Fleet {
    pub machine: String,
    pub profiles: Vec<ProfileState>,
    pub codex: CodexState,
    /// Muse is installed and signed in (`museHome`).
    pub muse_on: bool,
    /// The running Muse processes, by the last scan that worked.
    pub muse: Vec<MuseProc>,
}

impl Fleet {
    pub fn new(machine: String, codex_home: PathBuf) -> Fleet {
        Fleet {
            machine,
            profiles: vec![],
            codex: CodexState {
                home: codex_home,
                ..CodexState::default()
            },
            muse_on: false,
            muse: vec![],
        }
    }

    /// Takes in the profiles found: the indexes of the new ones.
    pub fn add_profiles(&mut self, found: Vec<Profile>) -> Vec<usize> {
        let mut added = vec![];
        for p in found {
            if !self.profiles.iter().any(|s| s.profile.dir == p.dir) {
                self.profiles.push(ProfileState::new(p));
                added.push(self.profiles.len() - 1);
            }
        }
        added
    }

    /// The profile whose folder is `home/<name>`.
    pub fn profile_named(&self, name: &str) -> Option<usize> {
        self.profiles
            .iter()
            .position(|s| s.profile.dir.file_name().is_some_and(|n| n == name))
    }

    // ------------------------------------------------------------ sessions out

    pub fn claude_session(&self, pi: usize, sid: &str, now: i64) -> Option<Session> {
        let ps = &self.profiles[pi];
        let c = ps.sessions.get(sid)?;
        let tail = c.tail.as_ref().map(|t| t.lock().unwrap());
        let facts = tail.as_ref().map(|t| t.facts()).unwrap_or_default();
        let (state, live, reason) = decide(&c.row, &facts, c.hook.as_ref(), now);
        let reason = match ps.stale_since {
            Some(at) => format!("roster stale since {}; {reason}", hh_mm_utc(at)),
            None => reason,
        };
        let t = tail.as_ref().map(|t| t.summary());
        let of =
            |f: fn(&fleet_read::transcript::TranscriptSummary) -> Option<String>| t.and_then(f);
        let model = of(|t| t.model.clone());
        Some(Session {
            key: SessionKey {
                machine: self.machine.clone(),
                profile: ps.profile.id.clone(),
                session_id: sid.to_string(),
            },
            engine: Engine::Claude,
            name: c.row.name.clone(),
            cwd: c.row.cwd.as_deref().map(tilde),
            state,
            reason,
            live,
            pid: c.row.pid,
            stage: of(|t| t.stage.clone()),
            context_tokens: context_tokens(
                c.status.as_ref(),
                t.and_then(|t| t.last_at),
                t.and_then(|t| t.context_tokens),
                model.as_deref(),
            ),
            model,
            effort: of(|t| t.effort.clone()),
            last_at: t.and_then(|t| t.last_at),
            said: of(|t| t.said.clone()),
            doing: of(|t| t.doing.clone()),
            last_ask: of(|t| t.last_ask.clone()),
            first_ask: of(|t| t.first_ask.clone()),
            started_at: c.row.started_at,
            hook_at: c.hook.as_ref().map(|h| h.at),
            written_at: facts.written_at,
        })
    }

    pub fn profile_sessions(&self, pi: usize, now: i64) -> Vec<Session> {
        self.profiles[pi]
            .sessions
            .keys()
            .filter_map(|sid| self.claude_session(pi, sid, now))
            .collect()
    }

    /// Whether a session is this profile's: what a roster read replaces.
    pub fn in_profile(&self, pi: usize) -> impl Fn(&Session) -> bool + 'static {
        let (machine, id) = (self.machine.clone(), self.profiles[pi].profile.id.clone());
        move |s| s.engine == Engine::Claude && s.key.machine == machine && s.key.profile == id
    }

    pub fn codex_sessions(&self, now: i64) -> Vec<Session> {
        // One session per thread id: two logs of one thread show the newer.
        let mut by_id: BTreeMap<String, Session> = BTreeMap::new();
        for (f, t) in &self.codex.tails {
            let mut s = t.lock().unwrap().session(self.codex.open.contains(f), now);
            s.key.machine = self.machine.clone();
            let newer = by_id
                .get(&s.key.session_id)
                .is_none_or(|o| s.written_at > o.written_at);
            if newer {
                by_id.insert(s.key.session_id.clone(), s);
            }
        }
        by_id.into_values().collect()
    }

    pub fn muse_sessions(&self) -> Vec<Session> {
        if !self.muse_on {
            return vec![];
        }
        self.muse
            .iter()
            .map(|m| {
                let mut s = muse_session(m.pid, m.cwd.as_deref(), m.since, &m.args);
                s.key.machine = self.machine.clone();
                s
            })
            .collect()
    }

    /// Codex and Muse: what a process scan or a rollout event replaces.
    pub fn others(&self, now: i64) -> Vec<Session> {
        let mut v = self.codex_sessions(now);
        v.extend(self.muse_sessions());
        v
    }

    pub fn in_others(&self) -> impl Fn(&Session) -> bool + 'static {
        let machine = self.machine.clone();
        move |s| matches!(s.engine, Engine::Codex | Engine::Muse) && s.key.machine == machine
    }

    pub fn all_sessions(&self, now: i64) -> Vec<Session> {
        let mut all: Vec<Session> = (0..self.profiles.len())
            .flat_map(|pi| self.profile_sessions(pi, now))
            .collect();
        all.extend(self.others(now));
        all
    }

    pub fn in_machine(&self) -> impl Fn(&Session) -> bool + 'static {
        let machine = self.machine.clone();
        move |s| s.key.machine == machine
    }

    /// The files the hot path should follow: each live or recently written session's transcript or rollout, newest
    /// first, at most `MAX_FOLLOWED`.
    pub fn follows(&self, now: i64) -> Vec<PathBuf> {
        // A tail busy being read (its lock held) counts as just written: it is not let go for being busy.
        let mut want: Vec<(i64, PathBuf)> = vec![];
        for ps in &self.profiles {
            for c in ps.sessions.values() {
                let Some(t) = &c.tail else { continue };
                let w = t
                    .cell
                    .try_lock()
                    .map_or(now, |t| t.facts().written_at.unwrap_or(0));
                let live = c.row.active || c.row.pid.is_some_and(|_| c.row.pid_alive);
                if live || now - w < FOLLOW_RECENT_MS {
                    want.push((w, t.path().to_path_buf()));
                }
            }
        }
        for (f, t) in &self.codex.tails {
            let (live, w) = t.cell.try_lock().map_or((true, now), |t| {
                let s = t.session(self.codex.open.contains(f), now);
                (s.live, s.written_at.unwrap_or(0))
            });
            if live || now - w < FOLLOW_RECENT_MS {
                want.push((w, f.clone()));
            }
        }
        want.sort_by_key(|w| std::cmp::Reverse(w.0));
        want.into_iter()
            .take(MAX_FOLLOWED)
            .map(|(_, f)| f)
            .collect()
    }

    // ------------------------------------------------------------ Claude Code in

    /// The versions held of these sessions' hook files (`None` inside: none held; outer `None`: not listed).
    pub fn hook_sigs(&self, pi: usize, sids: &[String]) -> Vec<Option<Option<Sig>>> {
        let ps = &self.profiles[pi];
        sids.iter()
            .map(|s| ps.sessions.get(s).map(|c| c.hook_sig))
            .collect()
    }

    pub fn status_sigs(&self, pi: usize, sids: &[String]) -> Vec<Option<Option<Sig>>> {
        let ps = &self.profiles[pi];
        sids.iter()
            .map(|s| ps.sessions.get(s).map(|c| c.status_sig))
            .collect()
    }

    /// Takes in a hook file read.
    pub fn merge_hook(&mut self, pi: usize, sid: &str, r: FileRead<HookEvent>) -> Hooked {
        let Some(c) = self.profiles[pi].sessions.get_mut(sid) else {
            return Hooked {
                known: false,
                changed: false,
                moved: r.v.is_some(),
            };
        };
        let before = c.hook.as_ref().and_then(hook_state);
        let changed = c.take_hook(r);
        Hooked {
            known: true,
            changed,
            moved: changed && c.hook.as_ref().and_then(hook_state) != before,
        }
    }

    /// Takes in a status file read: whether it changed what is held.
    pub fn merge_status(&mut self, pi: usize, sid: &str, r: FileRead<StatusSnap>) -> bool {
        self.profiles[pi]
            .sessions
            .get_mut(sid)
            .is_some_and(|c| c.take_status(r))
    }

    /// The tail a listed session follows, and whether one is wanted at `path`: `Some(None)` when the session is listed
    /// and follows nothing yet.
    pub fn tail_for(&self, pi: usize, sid: &str, path: &Path) -> Option<Option<SharedTail>> {
        let c = self.profiles[pi].sessions.get(sid)?;
        match &c.tail {
            Some(t) if t.path() == path => Some(Some(t.clone())),
            Some(_) => None, // another file of the same id: the one found first stays
            None => Some(None),
        }
    }

    /// Puts in a tail opened without the board, unless the session went or one came in meanwhile.
    pub fn install_tail(&mut self, pi: usize, sid: &str, t: Tail) -> bool {
        match self.profiles[pi].sessions.get_mut(sid) {
            Some(c) if c.tail.is_none() => {
                c.tail = Some(new_tail(t));
                c.no_transcript = None;
                true
            }
            _ => false,
        }
    }

    /// Lets go of a tail whose file is gone (only the one that failed: another may have come in since).
    pub fn drop_tail(&mut self, pi: usize, sid: &str, t: &SharedTail) {
        if let Some(c) = self.profiles[pi].sessions.get_mut(sid) {
            if c.tail.as_ref().is_some_and(|x| Arc::ptr_eq(x, t)) {
                c.tail = None;
            }
        }
    }

    /// The sessions of a roster read whose transcript is not followed yet and still worth a look, with their folder:
    /// their tails are opened without holding the board.
    pub fn untailed(&self, pi: usize, read: &RosterRead) -> Vec<(String, Option<String>)> {
        let ps = &self.profiles[pi];
        read.rows
            .iter()
            .filter_map(|r| {
                let sid = r.session_id.clone()?;
                let skip = ps
                    .sessions
                    .get(&sid)
                    .is_some_and(|c| c.tail.is_some() || c.no_transcript.as_ref() == Some(&r.cwd));
                (!skip).then(|| (sid, r.cwd.clone()))
            })
            .collect()
    }

    /// Takes in a roster read and what was read for it without the board: the new tails, the transcripts looked for and
    /// not found, and each listed session's hook and status. A good read is now the profile's list: sessions it no
    /// longer lists are gone. When it could not tell what runs, the last good running set stands (R13). A failed one
    /// keeps every session as it was and marks the profile stale from the first failure on (R1).
    pub fn apply_roster(
        &mut self,
        pi: usize,
        read: Result<RosterRead, RosterError>,
        mut got: RosterGot,
        now: i64,
    ) {
        let ps = &mut self.profiles[pi];
        let read = match read {
            Ok(r) => r,
            Err(_) => {
                ps.stale_since.get_or_insert(now);
                return;
            }
        };
        ps.stale_since = None;
        let running: HashSet<String> = if read.active_known {
            let r: HashSet<String> = read
                .rows
                .iter()
                .filter(|r| r.active)
                .filter_map(|r| r.session_id.clone())
                .collect();
            ps.running_was = Some(r.clone());
            r
        } else {
            ps.running_was.clone().unwrap_or_default()
        };
        let mut next = BTreeMap::new();
        for mut row in read.rows {
            let Some(sid) = row.session_id.clone() else {
                continue;
            };
            row.active = running.contains(&sid);
            let mut c = ps
                .sessions
                .remove(&sid)
                .unwrap_or_else(|| ClaudeSession::new(RosterRow::default()));
            if c.tail.is_none() {
                if let Some(t) = got.opened.remove(&sid) {
                    c.tail = Some(new_tail(t));
                    c.no_transcript = None;
                } else if got.missed.contains(&sid) {
                    c.no_transcript = Some(row.cwd.clone());
                }
            }
            if let Some(h) = got.hooks.remove(&sid) {
                c.take_hook(h);
            }
            if let Some(s) = got.statuses.remove(&sid) {
                c.take_status(s);
            }
            c.row = row;
            next.insert(sid, c);
        }
        ps.sessions = next;
    }

    // ------------------------------------------------------------ Codex and Muse in

    pub fn under_sessions(&self, f: &Path) -> bool {
        f.starts_with(self.codex.home.join("sessions"))
    }

    pub fn rollout(&self, f: &Path) -> Option<SharedRollout> {
        self.codex.tails.get(f).cloned()
    }

    pub fn install_rollout(&mut self, f: PathBuf, t: RolloutTail) {
        self.codex.tails.entry(f).or_insert_with(|| new_rollout(t));
    }

    pub fn drop_rollout(&mut self, f: &Path, t: &SharedRollout) {
        if self.codex.tails.get(f).is_some_and(|x| Arc::ptr_eq(x, t)) {
            self.codex.tails.remove(f);
        }
    }

    /// Takes in a process scan that worked (a failed one is `None` and changes nothing: R14).
    pub fn take_scan(&mut self, s: Option<Scan>) {
        if let Some(s) = s {
            self.codex.open = s.codex_open;
            self.muse = s.muse;
        }
    }

    /// The logs a running Codex holds open, under `sessions/`, that are not followed yet.
    pub fn held_untracked(&self, open: &HashSet<PathBuf>) -> Vec<PathBuf> {
        open.iter()
            .filter(|f| self.under_sessions(f) && !self.codex.tails.contains_key(*f))
            .cloned()
            .collect()
    }

    /// The first read of Codex: the recent rollouts that are yours (at most 60) and every log held open. Reads files:
    /// for startup only, before anything else holds the board.
    pub fn open_codex(&mut self, scan: Option<Scan>, now: i64) {
        self.take_scan(scan);
        let mut files: Vec<PathBuf> = recent_rollouts(&self.codex.home, RECENT_CODEX_MS, now)
            .into_iter()
            .take(CODEX_RECENT)
            .collect();
        for f in self.held_untracked(&self.codex.open.clone()) {
            if !files.contains(&f) {
                files.push(f);
            }
        }
        for f in files {
            if let Ok(t) = RolloutTail::open(&f) {
                self.install_rollout(f, t);
            }
        }
    }

    // ------------------------------------------------------------ the reconcile

    /// What the reconcile re-reads, taken under the board lock (handles and names only, no reads).
    pub fn reconcile_plan(&self) -> Plan {
        Plan {
            profiles: self
                .profiles
                .iter()
                .map(|ps| PlanProfile {
                    profile: ps.profile.clone(),
                    sessions: ps
                        .sessions
                        .iter()
                        .map(|(sid, c)| PlanSession {
                            sid: sid.clone(),
                            pid: c.row.pid,
                            cwd: c.row.cwd.clone(),
                            tail: c.tail.clone(),
                            look: c.tail.is_none() && c.no_transcript.as_ref() != Some(&c.row.cwd),
                        })
                        .collect(),
                    jobs_sig: ps.jobs_sig.clone(),
                })
                .collect(),
            rollouts: self
                .codex
                .tails
                .iter()
                .map(|(f, t)| (f.clone(), t.clone()))
                .collect(),
            codex_home: self.codex.home.clone(),
        }
    }

    /// Takes in what the reconcile read without the board. Each value replaces what is held only if nothing newer came
    /// in meanwhile. The indexes of the profiles whose `jobs/` changed unseen.
    pub fn merge_reconcile(&mut self, got: Reconciled) -> Vec<usize> {
        let mut rosters = vec![];
        for (pi, gp) in got.profiles.into_iter().enumerate() {
            let ps = &mut self.profiles[pi];
            if gp.jobs_changed {
                rosters.push(pi);
            }
            for g in gp.sessions {
                let Some(c) = ps.sessions.get_mut(&g.sid) else {
                    continue;
                };
                c.take_hook(g.hook);
                c.take_status(g.status);
                if let Some(alive) = g.pid_alive {
                    if c.row.pid == g.pid {
                        c.row.pid_alive = alive;
                    }
                }
                if let Some(t) = &g.failed_tail {
                    if c.tail.as_ref().is_some_and(|x| Arc::ptr_eq(x, t)) {
                        c.tail = None;
                    }
                }
                match g.looked {
                    Some(Some(t)) if c.tail.is_none() => {
                        c.tail = Some(new_tail(t));
                        c.no_transcript = None;
                    }
                    Some(None) if c.tail.is_none() => c.no_transcript = Some(g.cwd),
                    _ => {}
                }
            }
        }
        self.muse_on = got.muse_on;
        self.take_scan(got.scan);
        for (f, t) in got.dropped_rollouts {
            self.drop_rollout(&f, &t);
        }
        for (f, t) in got.held {
            self.install_rollout(f, t);
        }
        rosters
    }
}

/// What a roster run read without the board.
#[derive(Default)]
pub struct RosterGot {
    pub opened: HashMap<String, Tail>,
    /// Sessions whose transcript was looked for and not found.
    pub missed: HashSet<String>,
    pub hooks: HashMap<String, FileRead<HookEvent>>,
    pub statuses: HashMap<String, FileRead<StatusSnap>>,
}

pub struct PlanSession {
    pub sid: String,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    pub tail: Option<SharedTail>,
    /// No transcript followed, and the last look (if any) was under another folder.
    pub look: bool,
}

pub struct PlanProfile {
    pub profile: Profile,
    pub sessions: Vec<PlanSession>,
    pub jobs_sig: Vec<(String, i128, u64)>,
}

pub struct Plan {
    pub profiles: Vec<PlanProfile>,
    pub rollouts: Vec<(PathBuf, SharedRollout)>,
    pub codex_home: PathBuf,
}

pub struct GotSession {
    sid: String,
    pid: Option<u32>,
    cwd: Option<String>,
    hook: FileRead<HookEvent>,
    status: FileRead<StatusSnap>,
    pid_alive: Option<bool>,
    failed_tail: Option<SharedTail>,
    /// `Some(found)` when a transcript was looked for.
    looked: Option<Option<Tail>>,
}

pub struct GotProfile {
    sessions: Vec<GotSession>,
    jobs_changed: bool,
}

pub struct Reconciled {
    profiles: Vec<GotProfile>,
    scan: Option<Scan>,
    muse_on: bool,
    dropped_rollouts: Vec<(PathBuf, SharedRollout)>,
    held: Vec<(PathBuf, RolloutTail)>,
}

/// The reconcile's reads, files only (R4), with no board lock: each hook and status file, each pid's liveness, each
/// tail checked against its fingerprint then advanced (R11; only that tail's own lock is held), a transcript looked for
/// where none is followed (and not already missed under the same folder), the jobs' records, and the process scan (its
/// result kept only when it worked: R14). A rollout neither held open nor written in the recent window is let go.
pub fn reconcile_read(plan: &Plan, scan: Option<Scan>, muse_on: bool, now: i64) -> Reconciled {
    let profiles = plan
        .profiles
        .iter()
        .map(|pp| GotProfile {
            jobs_changed: jobs_signature(&pp.profile) != pp.jobs_sig,
            sessions: pp
                .sessions
                .iter()
                .map(|s| {
                    let failed_tail = s
                        .tail
                        .as_ref()
                        .filter(|t| t.lock().unwrap().reconcile().is_err())
                        .cloned();
                    let looked = s.look.then(|| {
                        transcript_of(&pp.profile, &s.sid, s.cwd.as_deref())
                            .and_then(|f| Tail::open(&f).ok())
                    });
                    GotSession {
                        sid: s.sid.clone(),
                        pid: s.pid,
                        cwd: s.cwd.clone(),
                        hook: read_hook_file(&pp.profile, &s.sid),
                        status: read_status_file(&pp.profile, &s.sid),
                        pid_alive: s.pid.map(pid_alive),
                        failed_tail,
                        looked,
                    }
                })
                .collect(),
        })
        .collect();
    let open = scan.as_ref().map(|s| &s.codex_open);
    let mut dropped_rollouts = vec![];
    for (f, t) in &plan.rollouts {
        let mut g = t.lock().unwrap();
        let held = open.is_some_and(|o| o.contains(f));
        let keep = g.reconcile().is_ok()
            && (held
                || g.session(false, now)
                    .written_at
                    .is_some_and(|w| now - w < RECENT_CODEX_MS));
        if !keep {
            dropped_rollouts.push((f.clone(), t.clone()));
        }
    }
    let tracked: HashSet<&PathBuf> = plan.rollouts.iter().map(|(f, _)| f).collect();
    let sessions = plan.codex_home.join("sessions");
    let held = open
        .into_iter()
        .flatten()
        .filter(|f| f.starts_with(&sessions) && !tracked.contains(f))
        .filter_map(|f| Some((f.clone(), RolloutTail::open(f).ok()?)))
        .collect();
    Reconciled {
        profiles,
        scan,
        muse_on,
        dropped_rollouts,
        held,
    }
}
