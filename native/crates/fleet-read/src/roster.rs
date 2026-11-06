//! A profile's roster of sessions: what `claude agents --json --all` lists (`CliRoster`), or what Claude Code's own records
//! under the profile say (`FileRoster`). Read-only: nothing here writes to Claude Code's records.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fleet_model::decide::RosterRow;
use serde_json::Value;

use crate::json::read_json;
use crate::profiles::Profile;

#[derive(Debug, PartialEq)]
pub enum RosterError {
    Timeout,
    Spawn(String),
    Failed(String),
    Parse(String),
}

/// One read of a profile's roster.
#[derive(Debug, PartialEq)]
pub struct RosterRead {
    pub rows: Vec<RosterRow>,
    /// Whether `active` was decided. False when the list of what runs could not be read: every row then has `active`
    /// false, and the caller keeps its last good running set (as Node's `runningWas` does).
    pub active_known: bool,
}

pub trait RosterSource {
    fn read(&self, p: &Profile) -> Result<RosterRead, RosterError>;
    /// `cli` or `files`.
    fn kind(&self) -> &'static str;
}

/// A pid of 0 is no pid (Node's `!r.pid`).
fn pid_of(v: &Value) -> Option<u32> {
    v.get("pid")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0)
}

/// Whether a process with this pid exists (`kill(pid, 0)`; a process we may not signal still exists).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn is_job_id(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(str::to_string)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The record of a background job, `<profile>/jobs/<id>/state.json`.
fn job_record(p: &Profile, id: &str) -> Option<Value> {
    if !is_job_id(id) {
        return None;
    }
    read_json(&p.dir.join("jobs").join(id).join("state.json"), 1 << 20)
}

fn parse_iso_ms(s: &str) -> Option<i64> {
    // 2026-11-06T12:34:56.789Z
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let ms = if b.get(19) == Some(&b'.') {
        let frac: String = s[20..]
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .collect();
        format!("{frac:0<3}").parse::<i64>().ok()?
    } else {
        0
    };
    // days from civil
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

/// Whether a background job with no process of its own can still be starting: its record was written in the last ten
/// minutes (`jobFresh`). One written long ago and without a process died with its machine.
pub fn job_fresh(p: &Profile, id: &str, now: i64) -> bool {
    let Some(s) = job_record(p, id) else {
        return false;
    };
    let at = ["updatedAt", "createdAt"]
        .iter()
        .find_map(|k| s.get(k).and_then(Value::as_str).and_then(parse_iso_ms))
        .unwrap_or(0);
    at > 0 && now - at < 10 * 60_000
}

// ---------------------------------------------------------------- CliRoster

/// The environment a child of ours gets: everything but `CLAUDE*` and `FLEET_*` markers (a Claude Code session that starts
/// us must not hand its own to the agents we read), a PATH that finds `claude`, and `CLAUDE_CONFIG_DIR` unless `plain`.
pub fn child_env(
    vars: impl Iterator<Item = (String, String)>,
    home: &Path,
    p: &Profile,
) -> Vec<(String, String)> {
    let mut path = vec![
        home.join(".local/bin").to_string_lossy().into_owned(),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
    ];
    let mut env = Vec::new();
    for (k, v) in vars {
        if k == "PATH" {
            path.push(v);
        } else if !k.starts_with("CLAUDE") && !k.starts_with("FLEET_") {
            env.push((k, v));
        }
    }
    if path.len() == 3 {
        path.push("/usr/bin:/bin".into());
    }
    env.push(("PATH".into(), path.join(":")));
    if !p.plain {
        env.push((
            "CLAUDE_CONFIG_DIR".into(),
            p.dir.to_string_lossy().into_owned(),
        ));
    }
    env
}

pub struct CliRoster {
    claude: PathBuf,
    home: PathBuf,
    timeout: Duration,
}

impl Default for CliRoster {
    fn default() -> Self {
        Self::new()
    }
}

impl CliRoster {
    pub fn new() -> Self {
        let home = crate::home::home_dir();
        let claude = [
            home.join(".local/bin"),
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
            "/usr/bin".into(),
        ]
        .into_iter()
        .map(|d| d.join("claude"))
        .find(|f| f.is_file())
        .unwrap_or_else(|| "claude".into());
        Self {
            claude,
            home,
            timeout: Duration::from_secs(8),
        }
    }

    /// Run this `claude` instead of looking for one (tests).
    pub fn with_claude(claude: PathBuf) -> Self {
        Self {
            claude,
            ..Self::new()
        }
    }

    fn run(&self, p: &Profile, args: &[&str], timeout: Duration) -> Result<String, RosterError> {
        let out = run_child(
            &self.claude,
            &child_env(std::env::vars(), &self.home, p),
            args,
            timeout,
        )?;
        if out.success {
            Ok(String::from_utf8_lossy(&out.stdout).into())
        } else {
            Err(RosterError::Failed(
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .chars()
                    .take(200)
                    .collect(),
            ))
        }
    }

    /// `claude logs <id>` says "job not found" for a job that is gone. A timeout or any other failure proves nothing and
    /// is not kept; a definite answer is kept for 30 s.
    fn job_gone(&self, p: &Profile, id: &str) -> bool {
        static KEPT: OnceLock<Mutex<GoneCache>> = OnceLock::new();
        let kept = KEPT.get_or_init(Default::default);
        let key = (p.dir.clone(), id.to_string());
        if let Some((at, gone)) = kept.lock().unwrap().get(&key) {
            if at.elapsed() < Duration::from_secs(30) {
                return *gone;
            }
        }
        let Ok(out) = run_child(
            &self.claude,
            &child_env(std::env::vars(), &self.home, p),
            &["logs", id],
            Duration::from_secs(5),
        ) else {
            return false;
        };
        let gone = !out.success
            && String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("job not found");
        if out.success || gone {
            let mut k = kept.lock().unwrap();
            if k.len() > 500 {
                k.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
            }
            k.insert(key, (Instant::now(), gone));
        }
        gone
    }
}

/// `claude logs` answers kept 30 s, by (profile folder, job id): when asked, and whether the job is gone.
type GoneCache = HashMap<(PathBuf, String), (Instant, bool)>;

struct ChildOut {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

const MAX_STDOUT: u64 = 8_000_000;

fn drain(mut r: impl Read + Send + 'static, cap: u64) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.by_ref().take(cap + 1).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Run `bin` in its own process group with `env` only, and give up after `timeout`: the whole group is killed (a
/// grandchild must not outlive us) and the child reaped. Output beyond 8 MB is an error, as Node's `maxBuffer`.
fn run_child(
    bin: &Path,
    env: &[(String, String)],
    args: &[&str],
    timeout: Duration,
) -> Result<ChildOut, RosterError> {
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| RosterError::Spawn(e.to_string()))?;
    let pgid = child.id() as libc::pid_t;
    let out_rx = drain(child.stdout.take().unwrap(), MAX_STDOUT);
    let err_rx = drain(child.stderr.take().unwrap(), 64 * 1024);
    let kill_group = |child: &mut std::process::Child| {
        // SAFETY: the group was created by this child and the child is not yet reaped, so its id cannot be reused.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        let _ = child.wait();
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                kill_group(&mut child);
                return Err(RosterError::Failed(e.to_string()));
            }
        }
        if Instant::now() >= deadline {
            kill_group(&mut child);
            return Err(RosterError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    // Anything the child left running in its group goes with it; the readers then see EOF.
    // SAFETY: as above; the child was reaped by try_wait, but a group id stays unused while any member lives, and a
    // group with no members makes this a harmless ESRCH.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    let grace = Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    if stdout.len() as u64 > MAX_STDOUT {
        return Err(RosterError::Failed("output over 8 MB".into()));
    }
    Ok(ChildOut {
        success: status.success(),
        stdout,
        stderr,
    })
}

fn row_of(v: &Value) -> RosterRow {
    let pid = pid_of(v);
    RosterRow {
        id: str_of(v, "id"),
        session_id: str_of(v, "sessionId"),
        pid,
        pid_alive: pid.is_some_and(pid_alive),
        state: str_of(v, "state"),
        status: str_of(v, "status"),
        waiting_for: str_of(v, "waitingFor"),
        active: false,
        kind: str_of(v, "kind"),
        name: str_of(v, "name"),
        cwd: str_of(v, "cwd"),
        started_at: v.get("startedAt").and_then(Value::as_i64),
    }
}

impl RosterSource for CliRoster {
    fn kind(&self) -> &'static str {
        "cli"
    }

    fn read(&self, p: &Profile) -> Result<RosterRead, RosterError> {
        let (all, running) = std::thread::scope(|s| {
            let a = s.spawn(|| self.run(p, &["agents", "--json", "--all"], self.timeout));
            let r = s.spawn(|| self.run(p, &["agents", "--json"], self.timeout));
            (a.join().unwrap(), r.join().unwrap())
        });
        let all = all?;
        let list: Vec<Value> =
            serde_json::from_str(&all).map_err(|e| RosterError::Parse(e.to_string()))?;
        // What runs, by the list without --all. When that can't be read, `active` is not known.
        let running = running
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok());
        let active_known = running.is_some();
        let mut active: HashSet<String> = running
            .iter()
            .flatten()
            .filter_map(|r| str_of(r, "sessionId").or_else(|| str_of(r, "id")))
            .collect();
        let mut rows: Vec<RosterRow> = list
            .iter()
            .filter(|v| {
                v.get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            .map(row_of)
            .collect();
        // Listed as running but with no process: only a job whose screen Claude Code can still show is live.
        let now = now_ms();
        let doubtful: Vec<(&str, &str)> = rows
            .iter()
            .filter_map(|r| Some((r.session_id.as_deref()?, r.id.as_deref()?, r.pid)))
            .filter(|(sid, id, pid)| pid.is_none() && active.contains(*sid) && is_job_id(id))
            .map(|(sid, id, _)| (sid, id))
            .collect();
        let dropped: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = doubtful
                .iter()
                .map(|&(sid, id)| {
                    s.spawn(move || {
                        (!job_fresh(p, id, now) || self.job_gone(p, id)).then(|| sid.to_string())
                    })
                })
                .collect();
            hs.into_iter().filter_map(|h| h.join().unwrap()).collect()
        });
        for sid in dropped {
            active.remove(&sid);
        }
        for r in &mut rows {
            r.active = r.session_id.as_ref().is_some_and(|s| active.contains(s));
        }
        Ok(RosterRead { rows, active_known })
    }
}

// ---------------------------------------------------------------- FileRoster

fn listed_status(s: String) -> String {
    if s == "shell" {
        "busy".into()
    } else {
        s
    }
}

/// The roster from Claude Code's own records: `jobs/<id>/state.json` for background jobs, `sessions/<pid>.json` for the
/// process each is (or was) run by, plus sessions with no job (interactive ones). No process is started.
#[derive(Default)]
pub struct FileRoster;

impl RosterSource for FileRoster {
    fn kind(&self) -> &'static str {
        "files"
    }

    fn read(&self, p: &Profile) -> Result<RosterRead, RosterError> {
        // Session records by pid, newest first is not needed: a job's own record names its session.
        let mut records: Vec<Value> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(p.dir.join("sessions")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".json") else {
                    continue;
                };
                let Ok(pid) = stem.parse::<u32>() else {
                    continue;
                };
                if let Some(v) = read_json::<Value>(&e.path(), 1 << 20) {
                    if v.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)) {
                        records.push(v);
                    }
                }
            }
        }
        let mut used: HashSet<usize> = HashSet::new();
        let mut rows = Vec::new();
        if let Ok(rd) = std::fs::read_dir(p.dir.join("jobs")) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                let Some(job) = job_record(p, &id) else {
                    continue;
                };
                let sid = str_of(&job, "sessionId");
                // Several records can name one job (a restarted process): the one with a live pid, then the newest.
                let matches: Vec<usize> = (0..records.len())
                    .filter(|&i| {
                        let r = &records[i];
                        str_of(r, "jobId").as_deref() == Some(id.as_str())
                            || (sid.as_deref().is_some_and(|s| !s.is_empty())
                                && str_of(r, "sessionId") == sid)
                    })
                    .collect();
                used.extend(&matches);
                let rec = matches
                    .into_iter()
                    .max_by_key(|&i| {
                        let r = &records[i];
                        (
                            pid_of(r).is_some_and(pid_alive),
                            r.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
                        )
                    })
                    .map(|i| &records[i]);
                let pid = rec.and_then(pid_of);
                // Claude Code lists a running job's state as `working` and a shell step as `busy`; the records say `running`/`shell`.
                let state =
                    str_of(&job, "state")
                        .map(|s| if s == "running" { "working".into() } else { s });
                let ended = matches!(state.as_deref(), Some("done" | "stopped" | "failed"));
                let active = !ended && (pid.is_some_and(pid_alive) || job_fresh(p, &id, now_ms()));
                rows.push(RosterRow {
                    session_id: sid,
                    pid,
                    pid_alive: pid.is_some_and(pid_alive),
                    id: Some(id),
                    status: rec.and_then(|r| str_of(r, "status")).map(listed_status),
                    waiting_for: None,
                    active,
                    state,
                    kind: Some("background".into()),
                    name: str_of(&job, "name")
                        .filter(|n| !n.is_empty())
                        .or_else(|| str_of(&job, "intent").filter(|n| !n.is_empty())),
                    cwd: str_of(&job, "cwd"),
                    started_at: rec
                        .and_then(|r| r.get("startedAt")?.as_i64())
                        .or_else(|| str_of(&job, "createdAt").and_then(|s| parse_iso_ms(&s))),
                });
            }
        }
        // A live process with no job: an interactive session.
        for (i, r) in records.iter().enumerate() {
            if used.contains(&i) {
                continue;
            }
            let Some(pid) = pid_of(r) else {
                continue;
            };
            if !pid_alive(pid) {
                continue;
            }
            rows.push(RosterRow {
                id: None,
                session_id: str_of(r, "sessionId"),
                pid: Some(pid),
                pid_alive: true,
                state: None,
                status: str_of(r, "status").map(listed_status),
                waiting_for: None,
                active: true,
                kind: str_of(r, "kind").map(|k| {
                    if k == "bg" {
                        "background".into()
                    } else {
                        "interactive".into()
                    }
                }),
                name: str_of(r, "name"),
                cwd: str_of(r, "cwd"),
                started_at: r.get("startedAt").and_then(Value::as_i64),
            });
        }
        rows.sort_by(|a, b| (&a.id, &a.session_id, a.pid).cmp(&(&b.id, &b.session_id, b.pid)));
        Ok(RosterRead {
            rows,
            active_known: true,
        })
    }
}

/// The roster source for a `FLEETD_ROSTER` value (Ruling R12): `claude agents` unless it is `files`. The files agreed with
/// the CLI over 10 rounds, but not yet with a pending permission prompt, a failed job or a respawned one.
pub fn source_for(choice: Option<&str>) -> Box<dyn RosterSource + Send + Sync> {
    match choice {
        Some("files") => Box::new(FileRoster),
        _ => Box::new(CliRoster::new()),
    }
}

/// The roster the daemon reads: `FLEETD_ROSTER=files` opts into the files, else the CLI.
pub fn default_source() -> Box<dyn RosterSource + Send + Sync> {
    source_for(std::env::var("FLEETD_ROSTER").ok().as_deref())
}
