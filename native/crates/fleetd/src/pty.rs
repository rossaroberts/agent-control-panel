//! A pseudo-terminal with a program on it: the relay `fleet/term.mjs` runs in Python, as a library. The child gets its own
//! session with the pty as its controlling terminal and nothing else (fds 0, 1, 2 and no more). Stopping hangs up on the
//! whole process group, then kills what is left. No session or warm-pool logic lives here.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::attach_cmd::Cmd;

/// How long a program gets to leave after SIGHUP before the group is killed.
const HANGUP_GRACE: Duration = Duration::from_millis(1500);

/// `openpty` hands out descriptors that are inheritable until `set_cloexec` runs; spawns are serialised so no other
/// thread's fork can see them in between.
static SPAWN: Mutex<()> = Mutex::new(());

pub struct Pty {
    master: OwnedFd,
    pid: i32,
    status: Mutex<Option<ExitStatus>>,
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
            c.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Whatever else the daemon holds open and did not mark close-on-exec stays out of the child. Close-on-exec
                // ones (including std's own exec-error pipe) are left for exec to close.
                let max = libc::sysconf(libc::_SC_OPEN_MAX).clamp(256, 65536) as RawFd;
                for fd in 3..max {
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
            pid,
            status: Mutex::new(None),
        })
    }

    /// A new handle on the master, for reading what the program prints. It shares the master's open file, so setting it
    /// non-blocking sets it for the writer too (`write` copes).
    pub fn reader(&self) -> OwnedFd {
        self.master.try_clone().expect("dup of a pty master")
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

    /// Reap the child if it has exited (`block` waits for it). `Err` means it is already gone (reaped elsewhere).
    fn reap(&self, block: bool) -> Option<ExitStatus> {
        let mut raw = 0;
        loop {
            let r =
                unsafe { libc::waitpid(self.pid, &mut raw, if block { 0 } else { libc::WNOHANG }) };
            if r == self.pid {
                let st = ExitStatus::from_raw(raw);
                *self.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(st);
                return Some(st);
            }
            if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return *self.status.lock().unwrap_or_else(|e| e.into_inner());
        }
    }

    fn group_alive(&self) -> bool {
        unsafe { libc::killpg(self.pid, 0) == 0 }
    }

    /// Hang up on the program's whole process group, as closing a terminal window does; whatever is still there after 1.5 s
    /// is killed. Returns when the group is gone.
    pub fn stop(&self) {
        if self.reap(false).is_none() || self.group_alive() {
            unsafe { libc::killpg(self.pid, libc::SIGHUP) };
            let end = Instant::now() + HANGUP_GRACE;
            while Instant::now() < end {
                self.reap(false);
                if !self.group_alive() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe { libc::killpg(self.pid, libc::SIGKILL) };
        }
        // The kill is on its way; reap the leader so no zombie keeps the group listed.
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            self.reap(false);
            if !self.group_alive() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait for the program to exit; its status, or None if someone else already reaped it without recording one.
    pub fn wait(&self) -> Option<ExitStatus> {
        self.reap(true)
    }
}
