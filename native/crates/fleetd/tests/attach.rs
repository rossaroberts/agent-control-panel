//! `fleetd attach` as a person uses it: the real binary on a pty, against a daemon (in this process) whose terminals run
//! the stand-in `claude`. Every test checks the tty it ran on is back as it was (`stty -g` before and after).

mod terms_home;

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleet_model::types::{Engine, Session, SessionKey, State};
use fleetd::serve::{bind, serve, SocketGuard};
use fleetd::store::Store;
use terms_home::Home;

/// `$0` is the binary. The shell's own status is attach's, and it reports the tty (`stty -g`) as it found it or not.
const WRAPPER: &str = r#"B=$(stty -g); "$0" "$@"; rc=$?; A=$(stty -g)
if [ "$A" = "$B" ]; then echo "TTY=same"; else echo "TTY=changed"; fi
exit $rc"#;

const BIN: &str = env!("CARGO_BIN_EXE_fleetd");

fn sess(
    profile: &str,
    id: &str,
    engine: Engine,
    name: &str,
    state: State,
    job: Option<&str>,
) -> Session {
    Session {
        key: SessionKey {
            machine: "fixture-mac".into(),
            profile: profile.into(),
            session_id: id.into(),
        },
        engine,
        name: Some(name.into()),
        cwd: Some("~/work".into()),
        state,
        reason: "fixture".into(),
        live: true,
        pid: None,
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: None,
        doing: None,
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
        job_id: job.map(String::from),
        title: None,
        epic: None,
        archived: false,
        account: None,
    }
}

struct Daemon {
    home: Home,
    _sock: tempfile::TempDir,
    path: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
    terms: Arc<fleetd::terms::Terms>,
    _guard: SocketGuard,
}

impl Daemon {
    fn start() -> Daemon {
        let home = Home::new();
        let sock = tempfile::tempdir().unwrap();
        let path = sock.path().join("fleetd.sock");
        let store = Arc::new(Store::new());
        store.upsert(vec![
            sess(
                "default",
                "aaaaaaaa-0000-4000-8000-000000000001",
                Engine::Claude,
                "alpha job",
                State::Waiting,
                Some("0000000a"),
            ),
            sess(
                "work",
                "aaaaaaaa-0000-4000-8000-000000000002",
                Engine::Claude,
                "beta job",
                State::Working,
                Some("0000000b"),
            ),
            sess(
                "default",
                "aaaaaaaa-0000-4000-8000-000000000003",
                Engine::Claude,
                "interactive one",
                State::Working,
                None,
            ),
            sess(
                "codex",
                "0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                Engine::Codex,
                "gamma thread",
                State::Done,
                None,
            ),
        ]);
        let terms = Arc::new(home.terms());
        let (listener, guard) = bind(&path).unwrap();
        let task = tokio::spawn(serve(listener, store, "fixture-mac".into(), terms.clone()));
        Daemon {
            home,
            _sock: sock,
            path,
            task,
            terms,
            _guard: guard,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A pty of 100x30 as (master, slave), both close-on-exec from the start: a descriptor `openpty` hands out stays
/// inheritable until a later `fcntl`, and a process another test (or the in-process daemon) forks in that window would
/// hold this pty's master, so that closing it here would never hang the pty up.
fn pty() -> (OwnedFd, OwnedFd) {
    let ws = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: posix_openpt/grantpt/unlockpt/ptsname/open on a master we own; the name is copied before any other call.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        assert!(m >= 0, "posix_openpt");
        let master = OwnedFd::from_raw_fd(m);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m)).to_owned();
        let s = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        assert!(s >= 0, "open {name:?}");
        let slave = OwnedFd::from_raw_fd(s);
        assert_eq!(libc::ioctl(m, libc::TIOCSWINSZ as _, &ws), 0);
        (master, slave)
    }
}

/// `fleetd attach ...` on a pty of its own.
struct Tty {
    master: OwnedFd,
    slave: OwnedFd,
    child: Child,
    seen: Vec<u8>,
}

impl Tty {
    fn spawn(d: &Daemon, args: &[&str]) -> Tty {
        Tty::spawn_at(&d.path, d.home.path().as_path(), args)
    }

    fn spawn_at(sock: &Path, home: &Path, args: &[&str]) -> Tty {
        let (master, slave) = pty();
        let dup = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        // As a person runs it: from a shell, which keeps the tty and so can say whether it came back as it was.
        let mut c = Command::new("/bin/sh");
        c.args(["-c", WRAPPER, BIN, "attach"])
            .args(args)
            .env("FLEETD_SOCK", sock)
            .env("FLEETD_HOME", home)
            .stdin(dup(&slave))
            .stdout(dup(&slave))
            .stderr(dup(&slave));
        // SAFETY: setsid and TIOCSCTTY in the child before exec; the slave is its stdin.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = c.spawn().unwrap();
        Tty {
            master,
            slave,
            child,
            seen: vec![],
        }
    }

    fn type_bytes(&self, b: &[u8]) {
        // SAFETY: writes our slice to the master.
        let n = unsafe { libc::write(self.master.as_raw_fd(), b.as_ptr().cast(), b.len()) };
        assert_eq!(n as usize, b.len());
    }

    /// Reads the screen until it holds `needle` (looking at everything since the start), for `secs` at most.
    fn expect(&mut self, needle: &str, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            if self
                .seen
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
            {
                return true;
            }
            if Instant::now() >= end || !self.pump(50) {
                return false;
            }
        }
    }

    /// Takes what is on the screen, waiting `ms` at most for it. False if the pty is broken. (A terminal emulator
    /// always reads: a process closing a pty whose output nobody reads can hang in exit.)
    fn pump(&mut self, ms: i32) -> bool {
        {
            let mut p = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll and read on our own master fd.
            unsafe {
                if libc::poll(&mut p, 1, ms) < 0 {
                    return false;
                }
                if p.revents & libc::POLLIN != 0 {
                    let mut buf = [0u8; 4096];
                    let n = libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len());
                    if n > 0 {
                        self.seen.extend_from_slice(&buf[..n as usize]);
                    } else if n < 0 {
                        return false;
                    }
                }
            }
            true
        }
    }

    /// `stty -g` of this pty.
    fn stty(&self) -> String {
        let o = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::from(self.slave.try_clone().unwrap()))
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    }

    fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ on our master makes the kernel send SIGWINCH to the foreground group.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
    }

    fn exit_code(&mut self, secs: f64) -> Option<i32> {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code();
            }
            self.pump(10);
        }
        None
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        // The client under the shell first (the shell is the pid this test started; the client its only child), so a
        // failed test leaves no attach behind.
        let _ = Command::new("pkill")
            .args(["-KILL", "-P", &self.child.id().to_string()])
            .status();
        let _ = self.child.kill();
        // A process cannot finish exiting while its pty output sits unread: keep reading until it is gone.
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end && !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.pump(10);
        }
    }
}

/// A fresh pty's `stty -g`: what a shell's tty looks like before any client touches it.
fn fresh_stty() -> String {
    let (mut m, mut s) = (0, 0);
    // SAFETY: openpty fills two fds we then own.
    let (_m, s) = unsafe {
        assert_eq!(
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            0
        );
        (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
    };
    let o = Command::new("stty")
        .arg("-g")
        .stdin(Stdio::from(s))
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_shows_the_banner_echoes_typing_and_ctrl_bracket_detaches() {
    let d = Daemon::start();
    let mut t = Tty::spawn(&d, &["default/0000000a"]);
    assert!(
        t.expect("BANNER", terms_home::BOOT),
        "banner: {:?}",
        String::from_utf8_lossy(&t.seen)
    );
    let raw = t.stty();
    assert_ne!(raw, fresh_stty(), "the tty is raw while attached");
    t.type_bytes(b"hi there\r");
    assert!(
        t.expect("GOT hi there", 5.0),
        "echo: {:?}",
        String::from_utf8_lossy(&t.seen)
    );
    t.type_bytes(&[0x1d]);
    assert_eq!(t.exit_code(5.0), Some(0));
    assert!(
        t.expect("TTY=same", 1.0),
        "{:?}",
        String::from_utf8_lossy(&t.seen)
    );
    // Detaching typed nothing else into the program, and started it once.
    assert_eq!(d.home.starts_of("0000000a").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_changes_reach_the_program_as_redraw() {
    let d = Daemon::start();
    let mut t = Tty::spawn(&d, &["default/0000000b"]);
    assert!(t.expect("BANNER", terms_home::BOOT));
    t.resize(120, 40);
    assert!(
        t.expect("REDRAW 120x40", 5.0),
        "{:?}",
        String::from_utf8_lossy(&t.seen)
    );
    t.type_bytes(&[0x1d]);
    assert_eq!(t.exit_code(5.0), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_open_prints_the_reason_exits_1_and_restores_the_tty() {
    let d = Daemon::start();
    let mut t = Tty::spawn(&d, &["--trust", "default", "/no/such/folder"]);
    assert_eq!(t.exit_code(10.0), Some(1));
    assert!(
        t.expect("Pick a background session", 1.0),
        "{:?}",
        String::from_utf8_lossy(&t.seen)
    );
    assert!(
        t.expect("TTY=same", 1.0),
        "{:?}",
        String::from_utf8_lossy(&t.seen)
    );
}

#[test]
fn without_a_daemon_it_says_so_and_exits_1() {
    let home = tempfile::tempdir().unwrap();
    let mut t = Tty::spawn_at(
        &home.path().join("none.sock"),
        home.path(),
        &["default/0000000a"],
    );
    assert_eq!(t.exit_code(10.0), Some(1));
    assert!(t.expect("is `fleetd serve` running?", 1.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_shows_the_attachable_sessions_without_colour() {
    let d = Daemon::start();
    let o = tokio::task::block_in_place(|| {
        Command::new(BIN)
            .args(["attach", "--list"])
            .env("FLEETD_SOCK", &d.path)
            .env("FLEETD_HOME", d.home.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    });
    assert!(o.status.success());
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(!out.contains('\x1b'), "no colour: {out:?}");
    let rows: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(rows.len(), 3, "{out}");
    assert_eq!(
        rows[0][..3],
        [
            "codex/0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            "done",
            "gamma"
        ]
    );
    assert_eq!(
        rows[1][..4],
        ["default/0000000a", "waiting", "alpha", "job"]
    );
    assert_eq!(rows[2][..4], ["work/0000000b", "working", "beta", "job"]);
    assert!(
        !out.contains("interactive"),
        "a session with no job id is not attachable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signal_restores_the_tty_too() {
    for (sig, code) in [("TERM", 143), ("HUP", 129), ("INT", 130)] {
        let d = Daemon::start();
        let mut t = Tty::spawn(&d, &["default/0000000c"]);
        assert!(t.expect("BANNER", terms_home::BOOT), "{sig}");
        // The shell's one child is the client.
        let k = Command::new("pkill")
            .args([&format!("-{sig}"), "-P", &t.child.id().to_string()])
            .status()
            .unwrap();
        assert!(k.success(), "{sig}: pkill");
        assert_eq!(t.exit_code(5.0), Some(code), "{sig}");
        assert!(
            t.expect("TTY=same", 1.0),
            "{sig}: {:?}",
            String::from_utf8_lossy(&t.seen)
        );
    }
}

/// Types `byte` until `max` bytes are in or the pty stops taking them for half a second; the count typed.
fn type_until_stalled(t: &Tty, byte: u8, max: usize) -> usize {
    // Whole lines: a pty queues only complete lines for a program that is not reading, and then stops taking more.
    let line: Vec<u8> = std::iter::repeat_n(byte, 79).chain(*b"\n").collect();
    let chunk = line.repeat(200);
    let mut at = 0;
    while at < max {
        let mut p = libc::pollfd {
            fd: t.master.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: poll, then write from our slice to the master.
        let n = unsafe {
            if libc::poll(&mut p, 1, 500) <= 0 {
                break;
            }
            libc::write(t.master.as_raw_fd(), chunk.as_ptr().cast(), chunk.len())
        };
        if n <= 0 {
            break;
        }
        at += n as usize;
    }
    at
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_bracket_detaches_even_when_the_daemon_has_stopped_reading_input() {
    let d = Daemon::start();
    let mut t = Tty::spawn(&d, &["default/0000000d"]);
    assert!(t.expect("BANNER", terms_home::BOOT));
    let pid = d.home.starts_of("0000000d")[0];
    // The program stops reading: the pty fills, then the daemon's 1 MiB per terminal, then it stops reading the socket,
    // then the socket's buffer fills and the client's send waits.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    tokio::task::block_in_place(|| type_until_stalled(&t, b'x', 32 << 20));
    std::thread::sleep(Duration::from_millis(800));
    let t0 = Instant::now();
    t.type_bytes(&[0x1d]);
    let code = t.exit_code(3.0);
    let took = t0.elapsed();
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(code, Some(0), "{:?}", String::from_utf8_lossy(&t.seen));
    assert!(took < Duration::from_secs(1), "detached in {took:?}");
    assert!(t.expect("TTY=same", 1.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_drop_is_a_close_pooled_for_claude_stopped_for_the_rest() {
    let d = Daemon::start();
    // A signal: the client sends TermClose if it can, and the socket drops either way. A claude attach goes to the pool.
    let mut t = Tty::spawn(&d, &["default/0000000e"]);
    assert!(t.expect("BANNER", terms_home::BOOT));
    assert_eq!(d.terms.live_count(), 1);
    let k = Command::new("kill")
        .args(["-TERM"])
        .arg(client_pid(&t).to_string())
        .status()
        .unwrap();
    assert!(k.success());
    assert_eq!(t.exit_code(5.0), Some(143));
    assert!(
        terms_home::wait_for(Duration::from_secs(3), || d.terms.warm_count() == 1),
        "pooled"
    );
    // A kill that gives the client no chance to say anything: a trust-folder terminal is stopped, nothing leaks.
    let mut u = Tty::spawn(&d, &["--trust", "default", "~/work"]);
    assert!(u.expect("BANNER", terms_home::BOOT));
    assert_eq!(d.terms.live_count(), 2);
    let k = Command::new("kill")
        .args(["-KILL"])
        .arg(client_pid(&u).to_string())
        .status()
        .unwrap();
    assert!(k.success());
    assert!(
        terms_home::wait_for(Duration::from_secs(3), || d.terms.live_count() == 1),
        "stopped"
    );
    assert_eq!(d.terms.warm_count(), 1);
}

/// The pid of the `fleetd attach` under the test's wrapper shell (the shell's only child), found with `pgrep -P`.
fn client_pid(t: &Tty) -> u32 {
    let o = Command::new("pgrep")
        .args(["-P", &t.child.id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(o.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .expect("a client under the shell")
        .parse()
        .unwrap()
}

/// `fleetd attach` itself on a pty (no shell between): the test is its parent and sees its exit. The test keeps a slave
/// descriptor, as the shell that ran the client would: the client's exit is then not the pty's last close (a last close
/// waits for queued output to drain, for as long as nobody reads it).
struct Direct {
    master: Option<OwnedFd>,
    _slave: OwnedFd,
    child: Child,
}

impl Direct {
    fn spawn(d: &Daemon, args: &[&str]) -> Direct {
        let (master, slave) = pty();
        let dup = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        let mut c = Command::new(BIN);
        c.arg("attach")
            .args(args)
            .env("FLEETD_SOCK", &d.path)
            .env("FLEETD_HOME", d.home.path())
            .stdin(dup(&slave))
            .stdout(dup(&slave))
            .stderr(dup(&slave));
        // A session of its own, but the pty is not made its controlling terminal: a session leader with one waits in
        // its exit for the pty's queued output to drain (for as long as nobody reads it, past any signal). From a
        // shell, the shell is that leader, not the client.
        // SAFETY: setsid in the child before exec.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = c.spawn().unwrap();
        Direct {
            master: Some(master),
            _slave: slave,
            child,
        }
    }

    /// Reads the screen until it holds `needle`.
    fn expect(&self, needle: &str, secs: f64) -> bool {
        let fd = self.master.as_ref().unwrap().as_raw_fd();
        let end = Instant::now() + Duration::from_secs_f64(secs);
        let mut seen = vec![];
        while Instant::now() < end {
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll and read on our own master fd.
            unsafe {
                if libc::poll(&mut p, 1, 50) > 0 && p.revents & libc::POLLIN != 0 {
                    let mut buf = [0u8; 4096];
                    let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
                    if n <= 0 {
                        return false;
                    }
                    seen.extend_from_slice(&buf[..n as usize]);
                }
            }
            if seen.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                return true;
            }
        }
        false
    }

    fn type_bytes(&self, b: &[u8]) {
        let fd = self.master.as_ref().unwrap().as_raw_fd();
        // SAFETY: writes our slice to the master.
        assert_eq!(
            unsafe { libc::write(fd, b.as_ptr().cast(), b.len()) } as usize,
            b.len()
        );
    }

    /// How long the client took to exit, waiting `secs` at most (reading nothing meanwhile).
    fn exited_within(&mut self, secs: f64) -> Option<Duration> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs_f64(secs) {
            if self.child.try_wait().unwrap().is_some() {
                return Some(t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn signal(&self, sig: i32) {
        // SAFETY: a signal to the pid this test started.
        unsafe { libc::kill(self.child.id() as i32, sig) };
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        let _ = self.child.kill(); // the pid this test started
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_pty_ends_attach_within_2s() {
    let d = Daemon::start();
    let mut t = Direct::spawn(&d, &["default/00000010"]);
    assert!(tokio::task::block_in_place(
        || t.expect("BANNER", terms_home::BOOT)
    ));
    t.master = None; // the terminal window closes: its pty goes
    let took = tokio::task::block_in_place(|| t.exited_within(2.0));
    assert!(
        took.is_some(),
        "attach still running 2 s after its pty went"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_or_sighup_ends_attach_within_2s_while_its_output_is_stuck() {
    // Nobody reads the pty (a window that froze): the client blocks writing the program's output to it, and a signal
    // still ends it (the orderly way out cannot run while that write waits).
    for (sig, code) in [(libc::SIGTERM, 143), (libc::SIGHUP, 129)] {
        let d = Daemon::start();
        let mut t = Direct::spawn(&d, &["default/00000011"]);
        assert!(tokio::task::block_in_place(
            || t.expect("BANNER", terms_home::BOOT)
        ));
        t.type_bytes(b"flood\r");
        std::thread::sleep(Duration::from_millis(800));
        t.signal(sig);
        let took = tokio::task::block_in_place(|| t.exited_within(2.0));
        assert!(
            took.is_some(),
            "attach ignored signal {sig} with its output stuck"
        );
        assert_eq!(t.child.wait().unwrap().code(), Some(code), "signal {sig}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_ends_attach_within_2s_after_its_pty_is_gone() {
    // Stuck writing output to a pty nobody reads, then the pty goes: whichever comes first, SIGTERM or EIO, it ends.
    let d = Daemon::start();
    let mut t = Direct::spawn(&d, &["default/00000012"]);
    assert!(tokio::task::block_in_place(
        || t.expect("BANNER", terms_home::BOOT)
    ));
    t.type_bytes(b"flood\r");
    std::thread::sleep(Duration::from_millis(800));
    t.master = None;
    t.signal(libc::SIGTERM);
    let took = tokio::task::block_in_place(|| t.exited_within(2.0));
    assert!(
        took.is_some(),
        "attach still running 2 s after SIGTERM with its pty gone"
    );
}
