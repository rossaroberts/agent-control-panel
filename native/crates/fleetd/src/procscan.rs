//! Which Codex logs a running Codex holds open (what Node's `processes()` gets from `lsof` on macOS and `/proc/<pid>/fd` on
//! Linux): the `.jsonl` paths open in any process whose `argv[0]` ends in `codex` or `codex-exec` (Node's `ENGINE`). No
//! program is run: on macOS it asks the kernel (`proc_listallpids`, `sysctl(KERN_PROCARGS2)`, `proc_pidinfo(PROC_PIDLISTFDS)`,
//! `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`), about a millisecond where `lsof` takes hundreds; on Linux it reads `/proc`.
//!
//! `None` when the scan itself fails (or `FLEET_PROC_SCAN=0` turns it off): the caller then keeps its last good set
//! (Ruling R14). A process that exits or cannot be inspected mid-scan is skipped.

use std::collections::HashSet;
use std::path::PathBuf;

fn is_codex(name: &str) -> bool {
    matches!(name, "codex" | "codex-exec")
}

pub fn codex_open() -> Option<HashSet<PathBuf>> {
    if std::env::var("FLEET_PROC_SCAN").as_deref() == Ok("0") {
        return None;
    }
    imp::codex_open()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::{c_int, c_void, CStr};
    use std::os::unix::ffi::OsStrExt;

    /// `PROC_PIDFDVNODEPATHINFO` and the size and path offset of `struct vnode_fdinfowithpath` (sys/proc_info.h: 1200
    /// bytes, `pvip.vip_path` at 176, `MAXPATHLEN` long). libc does not bind this one struct.
    const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    const VNODE_FDINFOWITHPATH: usize = 1200;
    const VIP_PATH: usize = 176;

    fn pids() -> Option<Vec<c_int>> {
        // SAFETY: a null buffer asks for the count only.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return None;
        }
        let mut buf: Vec<c_int> = vec![0; n as usize + 64];
        let bytes = (buf.len() * std::mem::size_of::<c_int>()) as c_int;
        // SAFETY: `buf` is `bytes` long; the kernel writes at most that.
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr() as *mut c_void, bytes) };
        if got <= 0 {
            return None;
        }
        buf.truncate((got as usize).min(buf.len()));
        Some(buf)
    }

    /// The name of a process's program as Node's `ENGINE` sees it: the last part of its `argv[0]` (from `KERN_PROCARGS2`:
    /// `argc`, the executable's path, NUL padding, then the arguments). `buf` is reused across processes.
    fn arg0_name(pid: c_int, buf: &mut Vec<u8>) -> Option<String> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut len = buf.len();
        // SAFETY: `buf` is `len` bytes; the kernel writes at most that and sets `len` to what it wrote.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 || len < 4 {
            return None;
        }
        let b = &buf[4..len];
        let exec_end = b.iter().position(|&c| c == 0)?;
        let rest = &b[exec_end..];
        let start = rest.iter().position(|&c| c != 0)?;
        let arg0 = &rest[start..];
        let arg0 = &arg0[..arg0.iter().position(|&c| c == 0).unwrap_or(arg0.len())];
        let base = arg0.rsplit(|&c| c == b'/').next().unwrap_or(arg0);
        Some(String::from_utf8_lossy(base).into_owned())
    }

    /// `KERN_ARGMAX`: the most a process's arguments and environment can take.
    fn arg_max() -> usize {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut v: c_int = 0;
        let mut len = std::mem::size_of::<c_int>();
        // SAFETY: `v` is a `c_int` and `len` says so.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut v as *mut c_int as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 && v > 0 {
            v as usize
        } else {
            1 << 20
        }
    }

    fn open_jsonl(pid: c_int, out: &mut HashSet<PathBuf>) {
        let fd_size = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks for the size the fd list needs.
        let need =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return;
        }
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(need as usize / fd_size + 16);
        let cap = (fds.capacity() * fd_size) as c_int;
        // SAFETY: the buffer holds `cap` bytes of `proc_fdinfo`, plain data the kernel fills.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut c_void,
                cap,
            )
        };
        if got <= 0 {
            return;
        }
        // SAFETY: the kernel wrote `got` bytes of whole entries.
        unsafe { fds.set_len(got as usize / fd_size) };
        let mut info = vec![0u64; VNODE_FDINFOWITHPATH / 8];
        for fd in fds
            .iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            // SAFETY: `info` is 1200 bytes, the size of `vnode_fdinfowithpath`, and the call is told so.
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    info.as_mut_ptr() as *mut c_void,
                    VNODE_FDINFOWITHPATH as c_int,
                )
            };
            if n as usize != VNODE_FDINFOWITHPATH {
                continue;
            }
            // SAFETY: `info` lives and is 1200 bytes; the path field is NUL-terminated within its 1024 bytes.
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(info.as_ptr() as *const u8, VNODE_FDINFOWITHPATH)
            };
            let Ok(path) = CStr::from_bytes_until_nul(&bytes[VIP_PATH..]) else {
                continue;
            };
            let path = path.to_bytes();
            if path.ends_with(b".jsonl") {
                out.insert(PathBuf::from(std::ffi::OsStr::from_bytes(path)));
            }
        }
    }

    pub(super) fn codex_open() -> Option<HashSet<PathBuf>> {
        let mut out = HashSet::new();
        let mut buf = vec![0u8; arg_max()];
        for pid in pids()?.into_iter().filter(|&p| p > 0) {
            // Another user's process (or one gone since the list) cannot be read: skipped, as `ps`/`lsof` would.
            if arg0_name(pid, &mut buf).is_some_and(|n| is_codex(&n)) {
                open_jsonl(pid, &mut out);
            }
        }
        Some(out)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs;
    use std::path::Path;

    pub(super) fn codex_open() -> Option<HashSet<PathBuf>> {
        let mut out = HashSet::new();
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let name = e.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let arg0 = cmd.split(|&b| b == 0).next().unwrap_or_default();
            let exe = Path::new(std::ffi::OsStr::from_bytes(arg0))
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if !is_codex(exe) {
                continue;
            }
            let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    if target.as_os_str().as_bytes().ends_with(b".jsonl") {
                        out.insert(target);
                    }
                }
            }
        }
        Some(out)
    }

    use std::os::unix::ffi::OsStrExt;
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;
    pub(super) fn codex_open() -> Option<HashSet<PathBuf>> {
        None
    }
}
