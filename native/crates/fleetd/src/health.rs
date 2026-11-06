//! This machine's health for the board's Meta: load, memory, disk, and how many dev servers and database stacks run. A
//! port of `machineStatus` in `fleet/machine.mjs`, with syscalls where Node spawns `vm_stat` and `df`.
//!
//! Sampling runs only while at least one Board client is connected (`Store::board_client`), every 5 s: the numbers
//! that a syscall gives (load, memory, disk) are read each time; the dev servers (`lsof`, or `ss` on Linux) and the
//! database stacks (`docker ps`) cost a subprocess each, so they are asked at most every 30 s and never while no client
//! is connected. With no client the task waits on the client count and does nothing else.
//!
//! Where it differs from Node, deliberately:
//! - `cpu_pct` is `round(100 * load1 / cpus)` as the page shows it (Node's `vitals`), held to 100 to fit a percent.
//! - `dev` counts listening ports of `node`, `next*`, `vite`, `bun` and `deno` processes (by command-name prefix, one
//!   per port, not fleetd's own pid), the way Node does. The Node panel itself therefore counts as one dev server here:
//!   Node leaves out only its own process.
//! - `db` is the number of `supabase_db_*` containers in `docker ps` (Node's `stacks.length`); 0 where docker is
//!   absent or fails.
//! - `disk_pct` is what `df -k $HOME` prints, run as Node runs it, in the slow path (every 30 s). Until its first answer
//!   lands it is the last value held, or on the very first sample `statfs`'s figure (on APFS that reads a few points
//!   high: the free count is the container's, shared by every volume). `df` itself is the oracle: `statfs` is not it.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleet_model::types::Health;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, MissedTickBehavior};

use crate::store::Store;

/// How often the cheap numbers are read while a client is connected.
pub const EVERY: Duration = Duration::from_secs(5);
/// How often the subprocess answers (dev servers, database stacks) are asked again.
pub const SLOW_EVERY: Duration = Duration::from_secs(30);
/// How long a sample waits for a subprocess answer that is due before sending what it has; the answer follows when it
/// comes.
const SLOW_WAIT: Duration = Duration::from_millis(1000);
/// How long `lsof` and `docker` get before they are killed (Node: 8 s, and 6 s for docker).
const LSOF_WITHIN: Duration = Duration::from_secs(8);
const DOCKER_WITHIN: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub every: Duration,
    pub slow_every: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            every: EVERY,
            slow_every: SLOW_EVERY,
        }
    }
}

/// The running sampler; dropping it stops it.
pub struct HealthTask {
    task: JoinHandle<()>,
    samples: Arc<AtomicU64>,
    slow_runs: Arc<AtomicU64>,
}

impl HealthTask {
    /// How many times the cheap numbers were read.
    pub fn samples(&self) -> u64 {
        self.samples.load(Ordering::SeqCst)
    }

    /// How many times the subprocesses were run.
    pub fn slow_runs(&self) -> u64 {
        self.slow_runs.load(Ordering::SeqCst)
    }
}

impl Drop for HealthTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the sampler over `store`. Needs a tokio runtime.
pub fn spawn(store: Arc<Store>, opts: Options) -> HealthTask {
    let samples = Arc::new(AtomicU64::new(0));
    let slow_runs = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(sampler(store, opts, samples.clone(), slow_runs.clone()));
    HealthTask {
        task,
        samples,
        slow_runs,
    }
}

async fn sampler(
    store: Arc<Store>,
    opts: Options,
    samples: Arc<AtomicU64>,
    slow_runs: Arc<AtomicU64>,
) {
    let mut clients = store.subscribe_clients();
    let in_flight = Arc::new(AtomicBool::new(false));
    let mut last_slow: Option<Instant> = None;
    loop {
        // No client: nothing runs until one connects.
        if clients.wait_for(|n| *n > 0).await.is_err() {
            return;
        }
        let mut tick = interval(opts.every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    samples.fetch_add(1, Ordering::SeqCst);
                    let first = samples.load(Ordering::SeqCst) == 1;
                    let fast = sample_fast();
                    let due = last_slow.is_none_or(|t| t.elapsed() >= opts.slow_every);
                    let mut slow = None;
                    if due && !in_flight.swap(true, Ordering::SeqCst) {
                        last_slow = Some(Instant::now());
                        slow_runs.fetch_add(1, Ordering::SeqCst);
                        let flag = in_flight.clone();
                        let mut job = tokio::task::spawn_blocking(move || {
                            let r = sample_slow();
                            flag.store(false, Ordering::SeqCst);
                            r
                        });
                        match timeout(SLOW_WAIT, &mut job).await {
                            Ok(Ok(r)) => slow = Some(r),
                            Ok(Err(_)) => {}
                            Err(_) => {
                                // Late: the answer is laid in when it comes.
                                let store = store.clone();
                                tokio::spawn(async move {
                                    if let Ok(r) = job.await {
                                        apply(&store, |h| r.lay_in(h));
                                    }
                                });
                            }
                        }
                    }
                    apply(&store, |h| {
                        h.cpu_pct = fast.cpu_pct;
                        h.mem_pct = fast.mem_pct;
                        if first {
                            h.disk_pct = fast.disk_pct;
                        }
                        if let Some(r) = slow {
                            r.lay_in(h);
                        }
                    });
                }
                changed = clients.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *clients.borrow() == 0 {
                        break;
                    }
                }
            }
        }
    }
}

/// Changes the health held, starting from the last one (or zeros); a Meta goes out only if it moved.
fn apply(store: &Store, f: impl FnOnce(&mut Health)) {
    store.modify_meta(|m| {
        let mut h = m.health.unwrap_or_default();
        f(&mut h);
        let changed = m.health != Some(h);
        m.health = Some(h);
        changed
    });
}

/// What the syscalls give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fast {
    pub cpu_pct: u8,
    pub mem_pct: u8,
    pub disk_pct: u8,
}

pub fn sample_fast() -> Fast {
    Fast {
        cpu_pct: cpu_pct(),
        mem_pct: mem_pct().unwrap_or(0),
        disk_pct: disk_pct(&fleet_read::home::home_dir()).unwrap_or(0),
    }
}

/// A percent held to 0-100.
pub fn pct(x: f64) -> u8 {
    x.round().clamp(0.0, 100.0) as u8
}

/// One-minute load over the cores, as a percent (the page's `cpu`).
pub fn cpu_pct() -> u8 {
    let mut l = [0f64; 3];
    // SAFETY: `l` has room for the 1 sample asked.
    if unsafe { libc::getloadavg(l.as_mut_ptr(), 1) } < 1 {
        return 0;
    }
    // SAFETY: sysconf has no preconditions.
    let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as f64;
    cpu_from(l[0], cpus)
}

/// The percent for a one-minute `load` on `cpus` cores: Node rounds the load to a tenth before it divides.
pub fn cpu_from(load: f64, cpus: f64) -> u8 {
    pct(100.0 * ((load * 10.0).round() / 10.0) / cpus)
}

/// Memory in use (active + wired + compressor on macOS; total less available on Linux) as a percent of the total.
pub fn mem_pct() -> Option<u8> {
    mem_used_total().map(|(used, total)| pct(100.0 * used as f64 / total as f64))
}

#[cfg(target_os = "macos")]
fn mem_used_total() -> Option<(u64, u64)> {
    use std::sync::OnceLock;
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    static TOTAL: OnceLock<u64> = OnceLock::new();
    let total = *TOTAL.get_or_init(|| {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: `v` is 8 bytes and `len` says so; the name is NUL-terminated.
        let r = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut v as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 {
            v
        } else {
            0
        }
    });
    if total == 0 {
        return None;
    }
    // `mach_host_self` adds a reference to the port at each call: once is enough.
    #[allow(deprecated)]
    // SAFETY: no preconditions.
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    // SAFETY: all-zero is a valid `vm_statistics64`; `count` is its size in words, and the call fills at most that.
    let info = unsafe {
        let mut info: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let r = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut info as *mut libc::vm_statistics64 as *mut libc::integer_t,
            &mut count,
        );
        if r != 0 {
            return None;
        }
        info
    };
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let pages =
        info.active_count as u64 + info.wire_count as u64 + info.compressor_page_count as u64;
    (pages > 0).then_some((pages * page, total))
}

#[cfg(not(target_os = "macos"))]
fn mem_used_total() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let (total, avail) = (kb("MemTotal")?, kb("MemAvailable")?);
    (total > 0).then_some((total.saturating_sub(avail) * 1024, total * 1024))
}

/// The first sample's disk figure, before `df` has answered: used over used plus available over `statfs`, rounded up.
/// On APFS it is a few points above `df -k` (the free count is the container's).
pub fn disk_pct(dir: &Path) -> Option<u8> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: all-zero is a valid `statfs`; the path is NUL-terminated.
    let s = unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(path.as_ptr(), &mut s) != 0 {
            return None;
        }
        s
    };
    #[allow(clippy::unnecessary_cast)]
    let (blocks, bfree, bavail) = (s.f_blocks as u64, s.f_bfree as u64, s.f_bavail as u64);
    let used = blocks.saturating_sub(bfree);
    let denom = used + bavail;
    (denom > 0).then(|| {
        (100.0 * used as f64 / denom as f64)
            .ceil()
            .clamp(0.0, 100.0) as u8
    })
}

// ------------------------------------------------------------------------------------------------ subprocess answers

/// What the subprocesses answer: dev servers, database stacks, and the disk as `df` prints it (`None`: no answer).
#[derive(Clone, Copy, Debug)]
struct Slow {
    dev: u16,
    db: u16,
    disk: Option<u8>,
}

impl Slow {
    fn lay_in(self, h: &mut Health) {
        h.dev = self.dev;
        h.db = self.db;
        if let Some(d) = self.disk {
            h.disk_pct = d;
        }
    }
}

/// One `lsof` (or `ss`), one `docker ps`, one `df`.
fn sample_slow() -> Slow {
    Slow {
        dev: dev_servers(),
        db: db_stacks(),
        disk: df_pct(&fleet_read::home::home_dir()),
    }
}

/// Node's disk read: the first `NN%` on the second line of `df -k <dir>`; none when it is absent or 0.
pub fn parse_df_pct(out: &str) -> Option<u8> {
    let line = out.lines().nth(1)?;
    let at = line.find('%')?;
    let digits = line[..at]
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|d| !d.is_empty())?;
    digits.parse::<u8>().ok().filter(|n| *n > 0)
}

/// `df -k <dir>`'s capacity.
pub fn df_pct(dir: &Path) -> Option<u8> {
    let d = dir.to_string_lossy();
    let (_, out) = run_cmd(&bin("df"), &["-k", &d], &[], LSOF_WITHIN);
    parse_df_pct(&out)
}

fn dev_servers() -> u16 {
    let own = std::process::id();
    let n = if cfg!(target_os = "linux") {
        let (_, out) = run_cmd(&bin("ss"), &["-ltnpH"], &[], LSOF_WITHIN);
        count_ss_dev(&out, own)
    } else {
        let (_, out) = run_cmd(
            &bin("lsof"),
            &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"],
            &[],
            LSOF_WITHIN,
        );
        count_lsof_dev(&out, own)
    };
    n.min(u16::MAX as usize) as u16
}

fn db_stacks() -> u16 {
    let mut env = vec![];
    // Rootless Docker answers on the user's own socket; Docker Desktop and OrbStack on the default one.
    if let Some(sock) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("docker.sock"))
        .filter(|s| s.exists())
    {
        let mut v = OsString::from("unix://");
        v.push(sock);
        env.push(("DOCKER_HOST".to_string(), v));
    }
    let (ok, out) = run_cmd(
        &bin("docker"),
        &["ps", "--format", "{{.Names}}"],
        &env,
        DOCKER_WITHIN,
    );
    if !ok {
        return 0;
    }
    count_db_stacks(&out).min(u16::MAX as usize) as u16
}

/// Node's `DEV`: a command name that starts with one of these.
fn is_dev_command(cmd: &str) -> bool {
    ["node", "next-server", "next", "vite", "bun", "deno"]
        .iter()
        .any(|p| cmd.starts_with(p))
}

/// The ports `lsof -nP -iTCP -sTCP:LISTEN -Fpcn` shows a dev server listening on, other than `own`'s: one per port.
pub fn count_lsof_dev(out: &str, own: u32) -> usize {
    let mut seen = std::collections::HashSet::new();
    let (mut pid, mut cmd) = (None::<u32>, "");
    for l in out.lines() {
        let Some(rest) = l.get(1..) else { continue };
        match l.as_bytes()[0] {
            b'p' => pid = rest.parse().ok(),
            b'c' => cmd = rest,
            b'n' => {
                let port: Option<u32> = rest.rsplit(':').next().and_then(|p| p.parse().ok());
                if let (Some(port), Some(pid)) = (port.filter(|p| *p != 0), pid) {
                    if pid != own && is_dev_command(cmd) {
                        seen.insert(port);
                    }
                }
            }
            _ => {}
        }
    }
    seen.len()
}

/// The same from `ss -ltnpH` lines (Linux).
pub fn count_ss_dev(out: &str, own: u32) -> usize {
    let mut seen = std::collections::HashSet::new();
    for l in out.lines() {
        let port: Option<u32> = l
            .split_whitespace()
            .nth(3)
            .and_then(|a| a.rsplit(':').next())
            .and_then(|p| p.parse().ok());
        let who = l.split("users:((\"").nth(1).and_then(|r| {
            let (name, rest) = r.split_once('"')?;
            let pid = rest.split("pid=").nth(1)?;
            let pid: u32 = pid
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()?;
            Some((name.to_string(), pid))
        });
        if let (Some(port), Some((name, pid))) = (port.filter(|p| *p != 0), who) {
            if pid != own && is_dev_command(&name) {
                seen.insert(port);
            }
        }
    }
    seen.len()
}

/// The Supabase database containers in `docker ps --format {{.Names}}`.
pub fn count_db_stacks(out: &str) -> usize {
    out.lines()
        .filter(|n| {
            n.strip_prefix("supabase_db_")
                .is_some_and(|r| !r.is_empty())
        })
        .count()
}

/// Where Node's `bin()` finds a tool: the user's, Homebrew's, then the system's; else the bare name for the PATH.
fn bin(name: &str) -> String {
    let home = fleet_read::home::home_dir();
    for d in [
        home.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/usr/sbin"),
    ] {
        let f = d.join(name);
        if is_executable(&f) {
            return f.to_string_lossy().into_owned();
        }
    }
    name.to_string()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Runs `cmd` with no marker of a session in its environment, for at most `within`, then kills it (our own child only).
/// Whether it exited 0, and what it wrote to stdout (whatever came before a kill too).
pub fn run_cmd(
    cmd: &str,
    args: &[&str],
    env: &[(String, OsString)],
    within: Duration,
) -> (bool, String) {
    let home = fleet_read::home::home_dir();
    let mut path = format!(
        "{}:/opt/homebrew/bin:/usr/local/bin",
        home.join(".local/bin").display()
    );
    path.push(':');
    path.push_str(
        &std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into()),
    );
    let mut c = Command::new(cmd);
    c.args(args)
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| {
            let k = k.to_string_lossy();
            !k.starts_with("CLAUDE") && !k.starts_with("FLEET_") && k != "PATH"
        }))
        .env("PATH", path)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = c.spawn() else {
        return (false, String::new());
    };
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let mut buf = vec![];
        let _ = stdout.read_to_end(&mut buf);
        s.push_str(&String::from_utf8_lossy(&buf));
        s
    });
    let end = Instant::now() + within;
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    (ok, reader.join().unwrap_or_default())
}
