//! What the daemon holds about each session between reads, and the one `Session` built from it: for Claude Code, the
//! profile's last roster row, the transcript's `Tail` and the last hook event, decided by `decide` (port of the session
//! row `claudeProfile` builds in fleet/machine.mjs); for Codex, each rollout's `RolloutTail` and whether a running Codex
//! holds it open. Plain data and file reads only; the watcher decides when each is re-read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_model::decide::{decide, RosterRow, TailFacts};
use fleet_model::hook::hook_state;
use fleet_model::types::{Engine, HookEvent, Session, SessionKey};
use fleet_read::codex::{is_yours, recent_rollouts, RolloutTail, RECENT_CODEX_MS};
use fleet_read::hookfile::read_hook;
use fleet_read::profiles::Profile;
use fleet_read::roster::{pid_alive, RosterError, RosterRead};
use fleet_read::transcript::Tail;

/// Rollouts followed at most from the recent list (`codexHome` slices to 60); logs a running Codex holds open come on top.
const CODEX_RECENT: usize = 60;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `hh:mm` in UTC of an epoch-millisecond time, as `decide` writes times in reasons.
pub fn hh_mm_utc(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
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
            let m = std::fs::metadata(e.path().join("state.json")).ok()?;
            let at = m
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as i128);
            Some((e.file_name().to_string_lossy().into_owned(), at, m.len()))
        })
        .collect();
    sig.sort();
    sig
}

pub struct ClaudeSession {
    pub row: RosterRow,
    pub tail: Option<Tail>,
    pub hook: Option<HookEvent>,
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
    pub tails: BTreeMap<PathBuf, RolloutTail>,
    /// The rollouts running Codex processes hold open, by the last scan that worked (Ruling R14).
    pub open: HashSet<PathBuf>,
}

/// What a hook event did.
pub struct Hooked {
    /// The roster lists this session.
    pub known: bool,
    /// What the hook says moved (or the session is new): the roster's word on it may now be out of date.
    pub moved: bool,
}

pub struct Fleet {
    pub machine: String,
    pub profiles: Vec<ProfileState>,
    pub codex: CodexState,
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

    pub fn claude_session(&self, pi: usize, sid: &str, now: i64) -> Option<Session> {
        let ps = &self.profiles[pi];
        let c = ps.sessions.get(sid)?;
        let facts = c
            .tail
            .as_ref()
            .map(Tail::facts)
            .unwrap_or(TailFacts::default());
        let (state, live, reason) = decide(&c.row, &facts, c.hook.as_ref(), now);
        let reason = match ps.stale_since {
            Some(at) => format!("roster stale since {}; {reason}", hh_mm_utc(at)),
            None => reason,
        };
        let t = c.tail.as_ref().map(Tail::summary);
        let of =
            |f: fn(&fleet_read::transcript::TranscriptSummary) -> Option<String>| t.and_then(f);
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
            model: of(|t| t.model.clone()),
            effort: of(|t| t.effort.clone()),
            context_tokens: t.and_then(|t| t.context_tokens),
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
        for t in self.codex.tails.values() {
            let mut s = t.session(self.codex.open.contains(t.path()), now);
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

    pub fn in_codex(&self) -> impl Fn(&Session) -> bool + 'static {
        let machine = self.machine.clone();
        move |s| s.engine == Engine::Codex && s.key.machine == machine
    }

    pub fn all_sessions(&self, now: i64) -> Vec<Session> {
        let mut all: Vec<Session> = (0..self.profiles.len())
            .flat_map(|pi| self.profile_sessions(pi, now))
            .collect();
        all.extend(self.codex_sessions(now));
        all
    }

    pub fn in_machine(&self) -> impl Fn(&Session) -> bool + 'static {
        let machine = self.machine.clone();
        move |s| s.key.machine == machine
    }

    // ------------------------------------------------------------ Claude Code

    /// A hook file changed: the session's last event, re-read.
    pub fn on_hook(&mut self, pi: usize, sid: &str) -> Hooked {
        let ps = &mut self.profiles[pi];
        let Some(c) = ps.sessions.get_mut(sid) else {
            return Hooked {
                known: false,
                moved: true,
            };
        };
        let h = read_hook(&ps.profile, sid);
        let moved = c.hook.as_ref().and_then(hook_state) != h.as_ref().and_then(hook_state);
        c.hook = h;
        Hooked { known: true, moved }
    }

    /// A transcript changed: its `Tail` reads what was appended (or opens, for a session whose transcript is new). Whether
    /// the session is listed.
    pub fn on_transcript(&mut self, pi: usize, sid: &str, path: &Path) -> bool {
        let Some(c) = self.profiles[pi].sessions.get_mut(sid) else {
            return false;
        };
        match &mut c.tail {
            Some(t) if t.path() == path => {
                if t.advance().is_err() {
                    c.tail = None;
                }
            }
            Some(_) => {} // another file of the same id: the one found first stays
            None => c.tail = Tail::open(path).ok(),
        }
        true
    }

    /// The sessions of a roster read whose transcript is not followed yet, with their folder: their tails are opened
    /// without holding the board.
    pub fn untailed(&self, pi: usize, read: &RosterRead) -> Vec<(String, Option<String>)> {
        let ps = &self.profiles[pi];
        read.rows
            .iter()
            .filter_map(|r| {
                let sid = r.session_id.clone()?;
                let followed = ps.sessions.get(&sid).is_some_and(|c| c.tail.is_some());
                (!followed).then(|| (sid, r.cwd.clone()))
            })
            .collect()
    }

    /// Takes in a roster read. A good one is now the profile's list: sessions it no longer lists are gone. When it could
    /// not tell what runs, the last good running set stands (R13). A failed one keeps every session as it was and marks
    /// the profile stale from the first failure on (R1).
    pub fn apply_roster(
        &mut self,
        pi: usize,
        read: Result<RosterRead, RosterError>,
        mut opened: HashMap<String, Tail>,
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
            let mut c = ps.sessions.remove(&sid).unwrap_or(ClaudeSession {
                row: RosterRow::default(),
                tail: None,
                hook: None,
            });
            if c.tail.is_none() {
                c.tail = opened.remove(&sid);
            }
            c.hook = read_hook(&ps.profile, &sid);
            c.row = row;
            next.insert(sid, c);
        }
        ps.sessions = next;
    }

    /// The reconcile's re-read of a profile, files only (R4): each hook, each pid's liveness, each tail checked against
    /// its fingerprint then advanced (R11), and a transcript looked for where there was none.
    pub fn reconcile_profile(&mut self, pi: usize) {
        let ps = &mut self.profiles[pi];
        for (sid, c) in ps.sessions.iter_mut() {
            c.hook = read_hook(&ps.profile, sid);
            c.row.pid_alive = c.row.pid.is_some_and(pid_alive);
            match &mut c.tail {
                Some(t) => {
                    if t.reconcile().is_err() {
                        c.tail = None;
                    }
                }
                None => {
                    c.tail = transcript_of(&ps.profile, sid, c.row.cwd.as_deref())
                        .and_then(|f| Tail::open(&f).ok());
                }
            }
        }
    }

    // ------------------------------------------------------------ Codex

    fn under_sessions(&self, f: &Path) -> bool {
        f.starts_with(self.codex.home.join("sessions"))
    }

    /// The first read: the recent rollouts that are yours (at most 60) and every log a running Codex holds open.
    pub fn open_codex(&mut self, open: Option<HashSet<PathBuf>>, now: i64) {
        if let Some(o) = open {
            self.codex.open = o;
        }
        let mut files: Vec<PathBuf> = recent_rollouts(&self.codex.home, RECENT_CODEX_MS, now)
            .into_iter()
            .take(CODEX_RECENT)
            .collect();
        let held: Vec<PathBuf> = self
            .codex
            .open
            .iter()
            .filter(|f| self.under_sessions(f) && !files.contains(f))
            .cloned()
            .collect();
        files.extend(held);
        for f in files {
            if let Ok(t) = RolloutTail::open(&f) {
                self.codex.tails.insert(f, t);
            }
        }
    }

    /// A rollout changed: its tail reads what was appended, or a new log of yours is followed.
    pub fn on_codex(&mut self, f: &Path, open: Option<HashSet<PathBuf>>) {
        if let Some(o) = open {
            self.codex.open = o;
        }
        if let Some(t) = self.codex.tails.get_mut(f) {
            if t.advance().is_err() {
                self.codex.tails.remove(f);
            }
        } else if self.under_sessions(f) && f.is_file() && is_yours(f) {
            if let Ok(t) = RolloutTail::open(f) {
                self.codex.tails.insert(f.to_path_buf(), t);
            }
        }
    }

    /// The reconcile's re-read of Codex: the open set (kept when the scan failed), each tail checked and advanced, logs
    /// newly held open followed, and logs neither open nor written in the recent window let go.
    pub fn reconcile_codex(&mut self, open: Option<HashSet<PathBuf>>, now: i64) {
        if let Some(o) = open {
            self.codex.open = o;
        }
        let open = &self.codex.open;
        self.codex.tails.retain(|f, t| {
            t.reconcile().is_ok()
                && (open.contains(f)
                    || t.session(false, now)
                        .written_at
                        .is_some_and(|w| now - w < RECENT_CODEX_MS))
        });
        let held: Vec<PathBuf> = open
            .iter()
            .filter(|f| self.under_sessions(f) && !self.codex.tails.contains_key(*f))
            .cloned()
            .collect();
        for f in held {
            if let Ok(t) = RolloutTail::open(&f) {
                self.codex.tails.insert(f, t);
            }
        }
    }
}
