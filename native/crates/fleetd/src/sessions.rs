//! What the daemon holds about each session between reads, and the one `Session` built from it: for Claude Code, the
//! profile's last roster row, the transcript's last view, the last hook event and the status-line snapshot, decided by
//! `decide` (port of the session row `claudeProfile` builds in fleet/machine.mjs); for Codex, each rollout's last view
//! and whether a running Codex holds it open; for Muse, its running processes.
//!
//! The `Fleet` methods do no file I/O and never wait on a tail's lock: they are what runs under the watcher's board
//! lock. Reads happen before, without it (`reads.rs`, `reconcile_read`), and come in as values: a tail's facts and
//! summary are published into the board as a numbered `Seen` view after each of its reads. Of two racing reads the
//! older never replaces the newer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_model::decide::{decide, due, hh_mm_utc, RosterRow};
use fleet_model::hook::hook_state;
use fleet_model::types::{Engine, HookEvent, HookSays, Session, SessionKey};
use fleet_read::codex::{muse_session, recent_rollouts, RolloutTail, RolloutView, RECENT_CODEX_MS};
use fleet_read::profiles::Profile;
use fleet_read::roster::{pid_alive, RosterError, RosterRead};
use fleet_read::statusfile::{context_tokens, StatusSnap};
use fleet_read::transcript::Tail;
use std::sync::Arc;

use crate::board_meta::{BoardMeta, Reading};
use crate::procscan::{MuseProc, Scan};
pub use crate::reads::{
    gone, hook_path, jobs_signature, new_rollout, new_tail, read_capped, read_hook_file,
    read_hook_file_with, read_status_file, reconcile_hook, reconcile_status, sig_of, status_path,
    take, transcript_of, FileRead, Reader, Seen, SharedRollout, SharedTail, Sig, TailView,
};

/// Rollouts followed at most from the recent list (`codexHome` slices to 60); logs a running Codex holds open come on top.
const CODEX_RECENT: usize = 60;
/// Files the hot path follows at most (kqueue fds), before the open-files limit has its say (`follow_cap`).
pub const MAX_FOLLOWED: usize = 256;
/// A closed session's transcript is still followed this long after its last write.
const FOLLOW_RECENT_MS: i64 = 60 * 60_000;

/// How many files to follow under an open-files soft limit: a quarter of it, at most `MAX_FOLLOWED`, so that following
/// never takes the descriptors every other read needs.
pub fn follow_cap(soft_limit: u64) -> usize {
    (soft_limit / 4).min(MAX_FOLLOWED as u64) as usize
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

// ---------------------------------------------------------------- the board

pub struct ClaudeSession {
    pub row: RosterRow,
    pub tail: Option<SharedTail>,
    /// What the tail last read (none without a tail).
    pub view: Option<Seen<TailView>>,
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
            view: None,
            hook: None,
            hook_sig: None,
            status: None,
            status_sig: None,
            no_transcript: None,
        }
    }

    fn take_hook(&mut self, r: FileRead<HookEvent>) -> bool {
        take(&mut self.hook, &mut self.hook_sig, r)
    }

    fn take_status(&mut self, r: FileRead<StatusSnap>) -> bool {
        take(&mut self.status, &mut self.status_sig, r)
    }

    fn set_tail(&mut self, t: (SharedTail, Seen<TailView>)) {
        self.tail = Some(t.0);
        self.view = Some(t.1);
        self.no_transcript = None;
    }

    /// Takes a view of this session's tail `t`, if it is still the one followed and the view is newer.
    fn take_view(&mut self, t: &SharedTail, seen: Seen<TailView>) -> bool {
        let ours = self.tail.as_ref().is_some_and(|x| Arc::ptr_eq(x, t));
        if ours && self.view.as_ref().is_none_or(|v| seen.gen > v.gen) {
            self.view = Some(seen);
            return true;
        }
        false
    }

    fn drop_tail(&mut self, t: &SharedTail) {
        if self.tail.as_ref().is_some_and(|x| Arc::ptr_eq(x, t)) {
            self.tail = None;
            self.view = None;
        }
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
    pub tails: BTreeMap<PathBuf, (SharedRollout, Seen<RolloutView>)>,
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
    /// Files the hot path follows at most (`follow_cap` of the open-files limit).
    pub max_followed: usize,
    /// Titles, epics, archive marks, accounts and limits: laid over every session this builds.
    pub board: BoardMeta,
}

impl Fleet {
    pub fn new(machine: String, codex_home: PathBuf) -> Fleet {
        Fleet {
            board: BoardMeta::new(machine.clone()),
            machine,
            profiles: vec![],
            codex: CodexState {
                home: codex_home,
                ..CodexState::default()
            },
            muse_on: false,
            muse: vec![],
            max_followed: MAX_FOLLOWED,
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
        let view = c.view.as_ref().map(|v| &v.v);
        let facts = view.map(|v| v.facts.clone()).unwrap_or_default();
        let (state, live, reason) = decide(&c.row, &facts, c.hook.as_ref(), now);
        let reason = match ps.stale_since {
            Some(at) => format!("roster stale since {}; {reason}", hh_mm_utc(at)),
            None => reason,
        };
        let t = view.map(|v| &v.summary);
        let of =
            |f: fn(&fleet_read::transcript::TranscriptSummary) -> Option<String>| t.and_then(f);
        let model = of(|t| t.model.clone());
        let mut s = Session {
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
            job_id: job_id(&c.row),
            title: None,
            epic: None,
            archived: false,
            account: None,
        };
        self.board.overlay(&mut s);
        Some(s)
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
        for (f, (_, seen)) in &self.codex.tails {
            let mut s = seen.v.session(self.codex.open.contains(f), now);
            s.key.machine = self.machine.clone();
            self.board.overlay(&mut s);
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
                self.board.overlay(&mut s);
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
    /// first, at most `max_followed`.
    pub fn follows(&self, now: i64) -> Vec<PathBuf> {
        let mut want: Vec<(i64, PathBuf)> = vec![];
        for ps in &self.profiles {
            for c in ps.sessions.values() {
                let (Some(t), Some(v)) = (&c.tail, &c.view) else {
                    continue;
                };
                let w = v.v.facts.written_at.unwrap_or(0);
                let live = c.row.active || c.row.pid.is_some_and(|_| c.row.pid_alive);
                if live || now - w < FOLLOW_RECENT_MS {
                    want.push((w, t.path().to_path_buf()));
                }
            }
        }
        for (f, (_, seen)) in &self.codex.tails {
            let s = seen.v.session(self.codex.open.contains(f), now);
            let w = s.written_at.unwrap_or(0);
            if s.live || now - w < FOLLOW_RECENT_MS {
                want.push((w, f.clone()));
            }
        }
        want.sort_by_key(|w| std::cmp::Reverse(w.0));
        want.into_iter()
            .take(self.max_followed)
            .map(|(_, f)| f)
            .collect()
    }

    // ------------------------------------------------------------ the board's groups and accounts

    /// Codex's newest plan-limit reading among the logs held (`codexHome`'s: the newest log that reported any).
    fn codex_reading(&self) -> Option<Reading> {
        self.codex
            .tails
            .values()
            .filter_map(|(_, seen)| seen.v.limits())
            .max_by_key(|(at, _)| *at)
            .map(|(at, l)| (at, l.to_vec()))
    }

    /// The groups and accounts as they are now (no health: that is the store's).
    pub fn meta(&self) -> fleet_model::types::Meta {
        let profiles: Vec<String> = self.profiles.iter().map(|p| p.profile.id.clone()).collect();
        self.board.meta(&profiles, self.codex_reading().as_ref())
    }

    /// A status file's limits, as read for one session: kept as the profile's when no older than what it holds. Whether
    /// the profile's limits changed.
    pub fn note_limits(&mut self, pi: usize, snap: &StatusSnap) -> bool {
        let (Some(at), Some(l)) = (snap.at, snap.limits.as_ref()) else {
            return false;
        };
        let id = self.profiles[pi].profile.id.clone();
        match self.board.claude_limits.get(&id) {
            Some((was, old)) if *was > at || (*was == at && old == l) => false,
            _ => {
                self.board.claude_limits.insert(id, (at, l.clone()));
                true
            }
        }
    }

    // ------------------------------------------------------------ the clock and the processes

    /// Each session whose state waits on the clock, and when (`decide::due`, `RolloutView::due`): what the one-shot timer
    /// is armed to. `roster`: a hook that needs you is quiet at that time, so the roster's word on it is wanted again (a
    /// permission given runs the tool with no event: only the roster says it is busy).
    pub fn dues(&self, now: i64) -> Vec<Due> {
        let mut out = vec![];
        for (pi, ps) in self.profiles.iter().enumerate() {
            for (sid, c) in &ps.sessions {
                let facts = c
                    .view
                    .as_ref()
                    .map(|v| v.v.facts.clone())
                    .unwrap_or_default();
                if let Some(at) = due(&c.row, &facts, c.hook.as_ref(), now) {
                    let roster = c.hook.as_ref().and_then(hook_state) == Some(HookSays::NeedsYou);
                    out.push(Due {
                        key: DueKey::Claude(pi, sid.clone()),
                        at,
                        roster,
                    });
                }
            }
        }
        for (f, (_, seen)) in &self.codex.tails {
            if let Some(at) = seen.v.due(self.codex.open.contains(f), now) {
                out.push(Due {
                    key: DueKey::Codex(f.clone()),
                    at,
                    roster: false,
                });
            }
        }
        out
    }

    /// The pids of the Claude Code sessions held as alive: what is watched for its exit.
    pub fn live_pids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .profiles
            .iter()
            .flat_map(|ps| ps.sessions.values())
            .filter(|c| c.row.pid_alive)
            .filter_map(|c| c.row.pid)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// A process exited: each session held as alive on that pid is not any more. Those sessions.
    pub fn pid_exited(&mut self, pid: u32) -> Vec<(usize, String)> {
        let mut out = vec![];
        for (pi, ps) in self.profiles.iter_mut().enumerate() {
            for (sid, c) in ps.sessions.iter_mut() {
                if c.row.pid == Some(pid) && c.row.pid_alive {
                    c.row.pid_alive = false;
                    out.push((pi, sid.clone()));
                }
            }
        }
        out
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
                moved: r.value().is_some(),
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
    pub fn install_tail(&mut self, pi: usize, sid: &str, t: (SharedTail, Seen<TailView>)) -> bool {
        match self.profiles[pi].sessions.get_mut(sid) {
            Some(c) if c.tail.is_none() => {
                c.set_tail(t);
                true
            }
            _ => false,
        }
    }

    /// Takes the view a read of tail `t` published (only if `t` is still followed and the view is newer).
    pub fn take_view(&mut self, pi: usize, sid: &str, t: &SharedTail, seen: Seen<TailView>) {
        if let Some(c) = self.profiles[pi].sessions.get_mut(sid) {
            c.take_view(t, seen);
        }
    }

    /// Lets go of a tail whose file is gone (only the one that failed: another may have come in since).
    pub fn drop_tail(&mut self, pi: usize, sid: &str, t: &SharedTail) {
        if let Some(c) = self.profiles[pi].sessions.get_mut(sid) {
            c.drop_tail(t);
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
                    c.set_tail(new_tail(t));
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
        self.codex.tails.get(f).map(|(t, _)| t.clone())
    }

    pub fn install_rollout(&mut self, f: PathBuf, t: (SharedRollout, Seen<RolloutView>)) {
        self.codex.tails.entry(f).or_insert(t);
    }

    /// Takes the view a read of rollout `t` published (only if `t` is still followed and the view is newer).
    pub fn take_rollout_view(&mut self, f: &Path, t: &SharedRollout, seen: Seen<RolloutView>) {
        if let Some((x, held)) = self.codex.tails.get_mut(f) {
            if Arc::ptr_eq(x, t) && seen.gen > held.gen {
                *held = seen;
            }
        }
    }

    pub fn drop_rollout(&mut self, f: &Path, t: &SharedRollout) {
        if self
            .codex
            .tails
            .get(f)
            .is_some_and(|(x, _)| Arc::ptr_eq(x, t))
        {
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

    /// A running Codex holds thread `session_id` open, by the last scan that worked: one of the open logs is its rollout
    /// (`rollout-<time>-<session_id>.jsonl`). In memory only, so it is safe to ask under the board lock.
    pub fn codex_held(&self, session_id: &str) -> bool {
        let tail = format!("-{session_id}");
        self.codex.open.iter().any(|f| {
            f.extension().is_some_and(|e| e == "jsonl")
                && f.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with(&tail))
        })
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
                self.install_rollout(f, new_rollout(t));
            }
        }
    }

    // ------------------------------------------------------------ the reconcile

    /// What the reconcile re-reads, taken under the board lock (handles and names only, no reads).
    pub fn reconcile_plan(&self, followed: impl Fn(&Path) -> bool) -> Plan {
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
                            followed: c.tail.as_ref().is_some_and(|t| followed(t.path())),
                            hook_sig: c.hook_sig,
                            status_sig: c.status_sig,
                        })
                        .collect(),
                    jobs_sig: ps.jobs_sig.clone(),
                })
                .collect(),
            rollouts: self
                .codex
                .tails
                .iter()
                .map(|(f, (t, _))| (f.clone(), t.clone()))
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
                match g.tail {
                    Some((t, TailRead::Gone)) => c.drop_tail(&t),
                    Some((t, TailRead::Seen(seen))) => {
                        c.take_view(&t, *seen);
                    }
                    None => {}
                }
                match g.looked {
                    Look::Found(t) if c.tail.is_none() => c.set_tail(*t),
                    Look::Missed if c.tail.is_none() => c.no_transcript = Some(g.cwd),
                    _ => {}
                }
            }
        }
        self.muse_on = got.muse_on;
        self.take_scan(got.scan);
        for (f, t, seen) in got.rollout_views {
            self.take_rollout_view(&f, &t, seen);
        }
        for (f, t) in got.dropped_rollouts {
            self.drop_rollout(&f, &t);
        }
        for (f, t) in got.held {
            self.install_rollout(f, t);
        }
        rosters
    }
}

/// Which session a due time is for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DueKey {
    Claude(usize, String),
    Codex(PathBuf),
}

/// When a session's state may change with the clock alone (`Fleet::dues`).
#[derive(Clone, Debug, PartialEq)]
pub struct Due {
    pub key: DueKey,
    pub at: i64,
    pub roster: bool,
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
    /// The tail is on the hot path (kqueue): the reconcile verifies it whatever its stat says.
    pub followed: bool,
    pub hook_sig: Option<Sig>,
    pub status_sig: Option<Sig>,
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

/// What the reconcile's read of a followed tail came to.
pub enum TailRead {
    /// Its file is gone: let go.
    Gone,
    /// Read (or failed for another reason, which proves nothing): its view after.
    Seen(Box<Seen<TailView>>),
}

/// The reconcile's look for a transcript.
pub enum Look {
    NotLooked,
    Found(Box<(SharedTail, Seen<TailView>)>),
    /// There is none (a read that failed is not a miss).
    Missed,
}

pub struct GotSession {
    sid: String,
    pid: Option<u32>,
    cwd: Option<String>,
    hook: FileRead<HookEvent>,
    status: FileRead<StatusSnap>,
    pid_alive: Option<bool>,
    tail: Option<(SharedTail, TailRead)>,
    looked: Look,
}

pub struct GotProfile {
    sessions: Vec<GotSession>,
    jobs_changed: bool,
}

pub struct Reconciled {
    profiles: Vec<GotProfile>,
    scan: Option<Scan>,
    muse_on: bool,
    rollout_views: Vec<(PathBuf, SharedRollout, Seen<RolloutView>)>,
    dropped_rollouts: Vec<(PathBuf, SharedRollout)>,
    held: Vec<(PathBuf, (SharedRollout, Seen<RolloutView>))>,
}

/// The reconcile's reads, files only (R4), with no board lock: each hook and status file, each pid's liveness, each
/// tail checked against its fingerprint then advanced (R11; only that tail's own lock is held), a transcript looked for
/// where none is followed (and not already missed under the same folder), the jobs' records, and the process scan (its
/// result kept only when it worked: R14). A rollout neither held open nor written in the recent window is let go.
pub fn reconcile_read(plan: &Plan, scan: Option<Scan>, muse_on: bool, now: i64) -> Reconciled {
    reconcile_read_with(plan, scan, muse_on, now, &read_capped)
}

/// `reconcile_read` reading file bodies with `read` (the seam a test counts reads through).
pub fn reconcile_read_with(
    plan: &Plan,
    scan: Option<Scan>,
    muse_on: bool,
    now: i64,
    read: &Reader,
) -> Reconciled {
    let profiles = plan
        .profiles
        .iter()
        .map(|pp| GotProfile {
            jobs_changed: jobs_signature(&pp.profile) != pp.jobs_sig,
            sessions: pp
                .sessions
                .iter()
                .map(|s| {
                    let tail = s.tail.as_ref().map(|t| {
                        let (r, seen) = t.run(|t| t.reconcile_if(s.followed));
                        let read = if gone(&r) {
                            TailRead::Gone
                        } else {
                            TailRead::Seen(Box::new(seen))
                        };
                        (t.clone(), read)
                    });
                    let looked = if !s.look {
                        Look::NotLooked
                    } else {
                        match transcript_of(&pp.profile, &s.sid, s.cwd.as_deref()) {
                            None => Look::Missed,
                            Some(f) => Tail::open(&f)
                                .map_or(Look::NotLooked, |t| Look::Found(Box::new(new_tail(t)))),
                        }
                    };
                    GotSession {
                        sid: s.sid.clone(),
                        pid: s.pid,
                        cwd: s.cwd.clone(),
                        hook: reconcile_hook(&pp.profile, &s.sid, s.hook_sig, read),
                        status: reconcile_status(&pp.profile, &s.sid, s.status_sig, read),
                        pid_alive: s.pid.map(pid_alive),
                        tail,
                        looked,
                    }
                })
                .collect(),
        })
        .collect();
    let open = scan.as_ref().map(|s| &s.codex_open);
    let mut dropped_rollouts = vec![];
    let mut rollout_views = vec![];
    for (f, t) in &plan.rollouts {
        let held = open.is_some_and(|o| o.contains(f));
        let (r, seen) = t.run(|t| t.reconcile());
        let recent = seen
            .v
            .written_at()
            .is_some_and(|w| now - w < RECENT_CODEX_MS);
        if gone(&r) || !(held || recent) {
            dropped_rollouts.push((f.clone(), t.clone()));
        } else {
            rollout_views.push((f.clone(), t.clone(), seen));
        }
    }
    let tracked: HashSet<&PathBuf> = plan.rollouts.iter().map(|(f, _)| f).collect();
    let sessions = plan.codex_home.join("sessions");
    let held = open
        .into_iter()
        .flatten()
        .filter(|f| f.starts_with(&sessions) && !tracked.contains(f))
        .filter_map(|f| Some((f.clone(), new_rollout(RolloutTail::open(f).ok()?))))
        .collect();
    Reconciled {
        profiles,
        scan,
        muse_on,
        rollout_views,
        dropped_rollouts,
        held,
    }
}

/// The roster `id` of a background job (8 lowercase hex digits), which `claude attach` takes.
fn job_id(row: &RosterRow) -> Option<String> {
    let id = row.id.as_deref()?;
    let hex8 = id.len() == 8 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (hex8 && row.kind.as_deref() == Some("background")).then(|| id.to_string())
}

#[cfg(test)]
mod codex_held_tests {
    use super::*;

    #[test]
    fn a_thread_is_held_when_its_rollout_is_in_the_open_set() {
        let mut f = Fleet::new("m".into(), PathBuf::from("/h/.codex"));
        let sid = "0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        assert!(!f.codex_held(sid));
        f.codex.open.insert(PathBuf::from(format!(
            "/h/.codex/sessions/2026/11/06/rollout-2026-11-06T10-00-00-{sid}.jsonl"
        )));
        assert!(f.codex_held(sid));
        assert!(!f.codex_held("0199aaaa-bbbb-4ccc-8ddd-ffffffffffff"));
    }
}
