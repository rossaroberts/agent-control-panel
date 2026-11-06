//! A pseudo-terminal with a program on it: the relay `fleet/term.mjs` runs in Python, as a library. The child gets its own
//! session with the pty as its controlling terminal and nothing else (fds 0, 1, 2 and no more). Stopping hangs up on the
//! whole process group, then kills what is left. No session or warm-pool logic lives here.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::attach_cmd::Cmd;

/// How long a program gets to leave after SIGHUP before the group is killed.
const HANGUP_GRACE: Duration = Duration::from_millis(1500);

/// `openpty` hands out descriptors that are inheritable until `set_cloexec` runs; spawns are serialised so no other
/// thread's fork can see them in between.
static SPAWN: Mutex<()> = Mutex::new(());

/// What outlives the `Pty` handle: the pid and what is known of its exit. Shared with the detached thread `Drop` may start.
struct Inner {
    pid: i32,
    /// `Some(status)` once the leader has been reaped (by `wait` or `stop`). Held across a reap, so a thread that finds
    /// the child already reaped (ECHILD) reads the status only after the reaper stored it.
    reaped: Mutex<Option<ExitStatus>>,
    /// The leader is reaped and the group was seen empty. After that the pid may belong to anyone, so nothing is signalled.
    finished: AtomicBool,
}

pub struct Pty {
    master: OwnedFd,
    inner: Arc<Inner>,
}

fn set_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// One past the highest descriptor this process has open (plus slack for another thread opening one meanwhile), so the
/// child's close loop does not walk the whole RLIMIT_NOFILE (8192 in the daemon) on every spawn.
fn fd_ceiling() -> RawFd {
    let top = std::fs::read_dir("/dev/fd").ok().map(|d| {
        d.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<RawFd>().ok())
            .max()
            .unwrap_or(0)
    });
    match top {
        Some(top) => top + 64,
        None => (unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }).clamp(256, 65536) as RawFd,
    }
}

impl Pty {
    /// Run `cmd` on a new pty of `cols` x `rows`.
    pub fn spawn(cmd: &Cmd, cols: u16, rows: u16) -> io::Result<Pty> {
        let (program, args) = cmd
            .argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
        let _guard = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        let (mut m, mut s) = (-1, -1);
        let mut ws = winsize(cols, rows);
        if unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        set_cloexec(m);
        set_cloexec(s);
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let ceiling = fd_ceiling();
        let mut c = Command::new(program);
        c.args(args)
            .env_clear()
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&cmd.cwd);
        c.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // Runs in the child between fork and exec (async-signal-safe calls only).
        unsafe {
            c.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Whatever else the daemon holds open and did not mark close-on-exec stays out of the child. Close-on-exec
                // ones (including std's own exec-error pipe) are left for exec to close.
                for fd in 3..ceiling {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                        libc::close(fd);
                    }
                }
                Ok(())
            });
        }
        let child = c.spawn()?;
        let pid = child.id() as i32;
        drop(child); // dropping a Child neither kills nor reaps it; `wait` and `stop` reap by pid
        Ok(Pty {
            master,
            inner: Arc::new(Inner {
                pid,
                reaped: Mutex::new(None),
                finished: AtomicBool::new(false),
            }),
        })
    }

    /// A new handle on the master, for reading what the program prints. It shares the master's open file, so setting it
    /// non-blocking sets it for the writer too (`write` copes).
    pub fn reader(&self) -> io::Result<OwnedFd> {
        self.master.try_clone()
    }

    /// Keystrokes to the program, all of them.
    pub fn write(&self, mut data: &[u8]) -> io::Result<()> {
        let fd = self.master.as_raw_fd();
        while !data.is_empty() {
            let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
            if n >= 0 {
                data = &data[n as usize..];
                continue;
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => {
                    let mut p = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut p, 1, 1000) };
                }
                _ => return Err(err),
            }
        }
        Ok(())
    }

    /// Tell the program its window is now `cols` x `rows` (it gets SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
    }

    /// Hang up on the program's whole process group, as closing a terminal window does; whatever is still there after 1.5 s
    /// is killed. Returns when the group is gone. Once the leader is reaped and the group seen empty this does nothing
    /// (the pid could be someone else's by then), so it is safe to call again, or after `wait`.
    pub fn stop(&self) {
        self.inner.stop();
    }

    /// Wait for the program to exit; its status. None only if the child was reaped without a status being recorded.
    pub fn wait(&self) -> Option<ExitStatus> {
        self.inner.wait()
    }
}

impl Inner {
    fn group_alive(&self) -> bool {
        unsafe { libc::killpg(self.pid, 0) == 0 }
    }

    /// Latch `finished` if the leader is reaped and nothing is left in its group. While any member lives its pgid cannot
    /// be reused, so a group seen alive is safe to signal; a reaped leader with an empty group is not.
    fn check_finished(&self, reaped: bool) -> bool {
        if reaped && !self.group_alive() {
            self.finished.store(true, Ordering::SeqCst);
        }
        self.finished.load(Ordering::SeqCst)
    }

    /// Non-blocking reap; true once the leader is reaped (by anyone).
    fn reap_now(&self) -> bool {
        let mut st = self.reaped.lock().unwrap_or_else(|e| e.into_inner());
        if st.is_some() {
            return true;
        }
        let mut raw = 0;
        let r = unsafe { libc::waitpid(self.pid, &mut raw, libc::WNOHANG) };
        if r == self.pid {
            *st = Some(ExitStatus::from_raw(raw));
            return true;
        }
        r < 0 // ECHILD: gone without a status we know of
    }

    fn settled(&self) -> bool {
        let reaped = self.reap_now();
        self.check_finished(reaped)
    }

    fn stop(&self) {
        if self.settled() {
            return;
        }
        unsafe { libc::killpg(self.pid, libc::SIGHUP) };
        let end = Instant::now() + HANGUP_GRACE;
        while Instant::now() < end {
            if self.settled() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Only a group seen alive just now is signalled (settled() latches the empty case).
        if !self.settled() {
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
        }
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            if self.settled() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait(&self) -> Option<ExitStatus> {
        let mut raw = 0;
        loop {
            let r = unsafe { libc::waitpid(self.pid, &mut raw, 0) };
            if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // Under the lock: a `stop` that reaped first has stored its status before releasing it.
            let mut st = self.reaped.lock().unwrap_or_else(|e| e.into_inner());
            if r == self.pid && st.is_none() {
                *st = Some(ExitStatus::from_raw(raw));
            }
            let out = *st;
            drop(st);
            self.check_finished(true);
            return out;
        }
    }
}

impl Drop for Pty {
    /// Letting go of the handle ends the program as closing the window would: reap if it is already gone, otherwise hang up
    /// now and finish the job (grace, then kill) on a detached thread, so dropping never blocks.
    fn drop(&mut self) {
        if self.inner.settled() {
            return;
        }
        unsafe { libc::killpg(self.inner.pid, libc::SIGHUP) };
        let inner = Arc::clone(&self.inner);
        let _ = std::thread::Builder::new()
            .name("pty-stop".into())
            .spawn(move || inner.stop());
    }
}
