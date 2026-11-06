//! The macOS hot path (Ruling R15): kqueue `EVFILT_VNODE` on each profile's `fleet-state/` and `fleet-status/` folders
//! (a hook or status file replaced by a rename is a write to its folder) and on each followed transcript and rollout
//! (`NOTE_WRITE | NOTE_EXTEND | NOTE_DELETE | NOTE_RENAME`). The kernel says so at once; FSEvents, which waits ~11 ms
//! under steady activity, stays for discovery. A file renamed or deleted is let go and, when its path names a file
//! again, followed anew. The folders and files are opened `O_EVTONLY`, so nothing is held against a volume. Each live
//! session's process is watched for its exit (`EVFILT_PROC` `NOTE_EXIT`, one shot), so a session that crashes (no
//! `SessionEnd`) shows ended at once rather than at the next reconcile.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_int, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What a kevent was about.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A watched folder's entries changed.
    Dir(PathBuf),
    /// A followed file was written to, grew, or was replaced.
    File(PathBuf),
    /// A watched process exited (or was gone when it was to be watched).
    Pid(u32),
}

const FFLAGS: u32 = libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME;
const WAKE: usize = 0;

#[derive(Default)]
struct Inner {
    by_fd: HashMap<c_int, Target>,
    files: HashMap<PathBuf, c_int>,
    dirs: HashMap<PathBuf, c_int>,
    pids: HashSet<u32>,
}

pub struct Kq {
    kq: c_int,
    inner: Mutex<Inner>,
}

fn change(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

impl Kq {
    pub fn new() -> io::Result<Kq> {
        // SAFETY: no arguments; the fd is ours to close.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let k = Kq {
            kq,
            inner: Mutex::new(Inner::default()),
        };
        k.submit(change(
            WAKE,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
        ))?;
        Ok(k)
    }

    fn submit(&self, ev: libc::kevent) -> io::Result<()> {
        // SAFETY: one change, no event list.
        let r = unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open(&self, path: &Path, target: Target) -> Option<c_int> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: a NUL-terminated path; the fd is closed by `close_fd` or `Drop`.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let ev = change(
            fd as usize,
            libc::EVFILT_VNODE,
            libc::EV_ADD | libc::EV_CLEAR,
            FFLAGS,
        );
        if self.submit(ev).is_err() {
            // SAFETY: ours, just opened.
            unsafe { libc::close(fd) };
            return None;
        }
        self.inner.lock().unwrap().by_fd.insert(fd, target);
        Some(fd)
    }

    fn close_fd(&self, fd: c_int) {
        self.inner.lock().unwrap().by_fd.remove(&fd);
        // SAFETY: an fd this queue opened; closing it drops its knote.
        unsafe { libc::close(fd) };
    }

    /// Watches a folder's entries. Whether it is watched (it exists).
    pub fn watch_dir(&self, dir: &Path) -> bool {
        if self.inner.lock().unwrap().dirs.contains_key(dir) {
            return true;
        }
        let Some(fd) = self.open(dir, Target::Dir(dir.to_path_buf())) else {
            return false;
        };
        let mut i = self.inner.lock().unwrap();
        if i.dirs.contains_key(dir) {
            drop(i);
            self.close_fd(fd);
        } else {
            i.dirs.insert(dir.to_path_buf(), fd);
        }
        true
    }

    /// Follows a file. Whether it is followed (it exists).
    pub fn follow(&self, file: &Path) -> bool {
        if self.inner.lock().unwrap().files.contains_key(file) {
            return true;
        }
        let Some(fd) = self.open(file, Target::File(file.to_path_buf())) else {
            return false;
        };
        let mut i = self.inner.lock().unwrap();
        if i.files.contains_key(file) {
            drop(i);
            self.close_fd(fd);
        } else {
            i.files.insert(file.to_path_buf(), fd);
        }
        true
    }

    pub fn unfollow(&self, file: &Path) {
        let fd = self.inner.lock().unwrap().files.remove(file);
        if let Some(fd) = fd {
            self.close_fd(fd);
        }
    }

    pub fn is_followed(&self, file: &Path) -> bool {
        self.inner.lock().unwrap().files.contains_key(file)
    }

    pub fn followed(&self) -> usize {
        self.inner.lock().unwrap().files.len()
    }

    /// Follows exactly these files: the ones no longer wanted are let go.
    pub fn set_follows(&self, want: &[PathBuf]) {
        let want_set: HashSet<&PathBuf> = want.iter().collect();
        let drop: Vec<PathBuf> = self
            .inner
            .lock()
            .unwrap()
            .files
            .keys()
            .filter(|f| !want_set.contains(f))
            .cloned()
            .collect();
        for f in drop {
            self.unfollow(&f);
        }
        for f in want {
            self.follow(f);
        }
    }

    /// Watches exactly these processes for their exit. The ones that are already gone are returned: as good as exited.
    pub fn set_pids(&self, want: &[u32]) -> Vec<u32> {
        let want: HashSet<u32> = want.iter().copied().filter(|p| *p > 0).collect();
        let (add, drop): (Vec<u32>, Vec<u32>) = {
            let i = self.inner.lock().unwrap();
            (
                want.difference(&i.pids).copied().collect(),
                i.pids.difference(&want).copied().collect(),
            )
        };
        for p in drop {
            // A one-shot that fired, or a process gone, has no knote left: the error says only that.
            let _ = self.submit(change(p as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0));
            self.inner.lock().unwrap().pids.remove(&p);
        }
        let mut gone = vec![];
        for p in add {
            let ev = change(
                p as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_ONESHOT,
                libc::NOTE_EXIT,
            );
            match self.submit(ev) {
                Ok(()) => {
                    self.inner.lock().unwrap().pids.insert(p);
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => gone.push(p),
                // Not ours to watch (another user's): the reconcile's liveness check stands for it.
                Err(_) => {}
            }
        }
        gone
    }

    /// Processes watched for their exit now.
    pub fn pids(&self) -> usize {
        self.inner.lock().unwrap().pids.len()
    }

    /// Makes a `wait` under way return (empty).
    pub fn wake(&self) {
        let _ = self.submit(change(WAKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER));
    }

    /// Blocks until something happens: what it was about. A file replaced or removed is let go, and followed again
    /// when its path names a file again (a rename over it). Empty on `wake`.
    pub fn wait(&self) -> Vec<Target> {
        let mut evs: Vec<libc::kevent> = (0..64).map(|_| change(0, 0, 0, 0)).collect();
        // SAFETY: room for 64 events; no changes; no timeout (blocks until an event or `wake`).
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                evs.as_mut_ptr(),
                evs.len() as c_int,
                std::ptr::null(),
            )
        };
        if n <= 0 {
            return vec![];
        }
        let mut out = vec![];
        for ev in &evs[..n as usize] {
            if ev.filter == libc::EVFILT_USER {
                continue;
            }
            if ev.filter == libc::EVFILT_PROC {
                let pid = ev.ident as u32;
                self.inner.lock().unwrap().pids.remove(&pid);
                out.push(Target::Pid(pid));
                continue;
            }
            let fd = ev.ident as c_int;
            let Some(t) = self.inner.lock().unwrap().by_fd.get(&fd).cloned() else {
                continue;
            };
            if ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME) != 0 {
                match &t {
                    Target::File(f) => {
                        self.unfollow(f);
                        self.follow(f);
                    }
                    Target::Dir(d) => {
                        let fd = self.inner.lock().unwrap().dirs.remove(d);
                        if let Some(fd) = fd {
                            self.close_fd(fd);
                        }
                    }
                    Target::Pid(_) => {}
                }
            }
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }
}

impl Drop for Kq {
    fn drop(&mut self) {
        let i = self.inner.get_mut().unwrap();
        for fd in i.by_fd.keys() {
            // SAFETY: fds this queue opened.
            unsafe { libc::close(*fd) };
        }
        // SAFETY: ours.
        unsafe { libc::close(self.kq) };
    }
}
