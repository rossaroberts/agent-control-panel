//! The process scan (what Node's `processes()` gets from `ps`/`lsof` on macOS and `/proc` on Linux), for the two things the
//! board needs from it: which Codex logs a running Codex holds open, and the running Muse processes. No program is run: on
//! macOS it asks the kernel (`proc_listallpids`, `proc_pidinfo(PROC_PIDTBSDINFO)` for each process's name, parent and
//! start; then, for Codex and Muse only, `PROC_PIDLISTFDS` + `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`, the working folder
//! and `sysctl(KERN_PROCARGS2)`), about a millisecond; on Linux it reads `/proc`.
//!
//! A process is Codex or Muse by its program's name (`codex`, `codex-exec`, `muse`: Node's `ENGINE` on `argv[0]`). On macOS
//! that is the kernel's name for the executable, so a link under another name to one of them is not seen (Node would).
//!
//! `None` when the scan itself fails (or `FLEET_PROC_SCAN=0` turns it off): the caller then keeps its last good result
//! (Ruling R14). A process that exits or cannot be inspected mid-scan is skipped.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A running Muse, as `sortProcesses` keeps it among `others`.
#[derive(Clone, Debug, PartialEq)]
pub struct MuseProc {
    pub pid: u32,
    pub cwd: Option<String>,
    /// Epoch ms it started.
    pub since: i64,
    /// Its arguments after the program, on one line (`oneLine(args.slice(1).join(' '), 160)`).
    pub args: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scan {
    pub codex_open: HashSet<PathBuf>,
    pub muse: Vec<MuseProc>,
}

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    Codex,
    Muse,
}

fn engine_of(name: &str) -> Option<Engine> {
    match name {
        "codex" | "codex-exec" => Some(Engine::Codex),
        "muse" => Some(Engine::Muse),
        _ => None,
    }
}

/// One process of interest, as the platform half reads it.
struct Proc {
    pid: u32,
    engine: Engine,
    args: Vec<String>,
    cwd: Option<String>,
    since: i64,
    open: Vec<PathBuf>,
}

/// Every process's parent, for the "helper of another Muse" test.
type Parents = HashMap<u32, u32>;

pub fn scan() -> Option<Scan> {
    if std::env::var("FLEET_PROC_SCAN").as_deref() == Ok("0") {
        return None;
    }
    let (procs, parents) = imp::procs()?;
    Some(sorted(procs, &parents))
}

/// `oneLine`: whitespace runs to one space, trimmed, cut to `n` UTF-16 units.
fn one_line(s: &str, n: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut units = 0;
    joined
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect()
}

/// The part of `sortProcesses` that decides which Muse processes are sessions: not this process's own children, not
/// Claude Code's machinery words, not a backend (`--sdk-url`, `app-server`, `mcp-server`), not the panel's own titles,
/// and not a helper of another Muse (up to 12 parents up).
fn sorted(procs: Vec<Proc>, parents: &Parents) -> Scan {
    let me = std::process::id();
    let muse_pids: HashSet<u32> = procs
        .iter()
        .filter(|p| p.engine == Engine::Muse)
        .map(|p| p.pid)
        .collect();
    let mut out = Scan::default();
    for p in procs {
        match p.engine {
            Engine::Codex => out.codex_open.extend(p.open),
            Engine::Muse => {
                let a1 = p.args.get(1).map(String::as_str).unwrap_or("");
                const MACHINERY: [&str; 11] = [
                    "agents",
                    "auth",
                    "attach",
                    "stop",
                    "rm",
                    "daemon",
                    "bg-pty-host",
                    "bg-spare",
                    "--bg-pty-host",
                    "--bg-spare",
                    "--version",
                ];
                let has = |a: &str| p.args.iter().any(|x| x == a);
                let mut up = parents.get(&p.pid).copied();
                let mut helper = false;
                for _ in 0..12 {
                    match up {
                        Some(pp) if pp > 1 => {
                            if muse_pids.contains(&pp) {
                                helper = true;
                                break;
                            }
                            up = parents.get(&pp).copied();
                        }
                        _ => break,
                    }
                }
                let skip = parents.get(&p.pid) == Some(&me)
                    || MACHINERY.contains(&a1)
                    || (has("--json-schema") && has("--no-session-persistence"))
                    || has("--sdk-url")
                    || a1 == "app-server"
                    || has("mcp-server")
                    || helper;
                if !skip {
                    out.muse.push(MuseProc {
                        pid: p.pid,
                        cwd: p.cwd,
                        since: p.since,
                        args: one_line(&p.args[1.min(p.args.len())..].join(" "), 160),
                    });
                }
            }
        }
    }
    out.muse.sort_by_key(|m| m.pid);
    out
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::{c_int, c_void, CStr};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::{Mutex, OnceLock};

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

    fn bsdinfo(pid: c_int) -> Option<libc::proc_bsdinfo> {
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: plain data, zeroed, and the call is told its size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut c_void,
                size,
            )
        };
        (n == size).then_some(info)
    }

    fn cstr(b: &[libc::c_char]) -> String {
        let b: Vec<u8> = b
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&b).into_owned()
    }

    /// The buffer `KERN_PROCARGS2` is read into, `KERN_ARGMAX` long: made once.
    fn argbuf() -> &'static Mutex<Vec<u8>> {
        static BUF: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
        BUF.get_or_init(|| {
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
            Mutex::new(vec![0; if r == 0 && v > 0 { v as usize } else { 1 << 20 }])
        })
    }

    /// A process's arguments (`KERN_PROCARGS2`: `argc`, the executable's path, NUL padding, then `argc` arguments).
    fn args(pid: c_int) -> Vec<String> {
        let mut buf = argbuf().lock().unwrap();
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
            return vec![];
        }
        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
        let b = &buf[4..len];
        let Some(exec_end) = b.iter().position(|&c| c == 0) else {
            return vec![];
        };
        let rest = &b[exec_end..];
        let Some(start) = rest.iter().position(|&c| c != 0) else {
            return vec![];
        };
        rest[start..]
            .split(|&c| c == 0)
            .take(argc)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }

    fn cwd(pid: c_int) -> Option<String> {
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as c_int;
        // SAFETY: plain data, zeroed, and the call is told its size.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut c_void,
                size,
            )
        };
        (n == size)
            .then(|| cstr(info.pvi_cdir.vip_path.as_flattened()))
            .filter(|s| !s.is_empty())
    }

    fn open_jsonl(pid: c_int) -> Vec<PathBuf> {
        let mut out = vec![];
        let fd_size = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks for the size the fd list needs.
        let need =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return out;
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
            return out;
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
                out.push(PathBuf::from(std::ffi::OsStr::from_bytes(path)));
            }
        }
        out
    }

    pub(super) fn procs() -> Option<(Vec<Proc>, Parents)> {
        let mut parents = Parents::new();
        let mut out = vec![];
        for pid in pids()?.into_iter().filter(|&p| p > 0) {
            // Another user's process (or one gone since the list) cannot be read: skipped, as `ps`/`lsof` would.
            let Some(info) = bsdinfo(pid) else { continue };
            parents.insert(pid as u32, info.pbi_ppid);
            let name = cstr(&info.pbi_name);
            let name = if name.is_empty() {
                cstr(&info.pbi_comm)
            } else {
                name
            };
            let Some(engine) = engine_of(&name) else {
                continue;
            };
            out.push(Proc {
                pid: pid as u32,
                engine,
                args: if engine == Engine::Muse {
                    args(pid)
                } else {
                    vec![]
                },
                cwd: if engine == Engine::Muse {
                    cwd(pid)
                } else {
                    None
                },
                since: info.pbi_start_tvsec as i64 * 1000 + info.pbi_start_tvusec as i64 / 1000,
                open: if engine == Engine::Codex {
                    open_jsonl(pid)
                } else {
                    vec![]
                },
            });
        }
        Some((out, parents))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::time::UNIX_EPOCH;

    pub(super) fn procs() -> Option<(Vec<Proc>, Parents)> {
        let mut parents = Parents::new();
        let mut out = vec![];
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some(ppid) = stat
                    .rfind(')')
                    .and_then(|i| stat[i + 2..].split(' ').nth(1))
                    .and_then(|s| s.parse().ok())
                {
                    parents.insert(pid, ppid);
                }
            }
            let Ok(cmd) = fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let args: Vec<String> = cmd
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            let exe = args
                .first()
                .and_then(|a| Path::new(a).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let Some(engine) = engine_of(exe) else {
                continue;
            };
            let mut open = vec![];
            if engine == Engine::Codex {
                if let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) {
                    for fd in fds.flatten() {
                        if let Ok(t) = fs::read_link(fd.path()) {
                            if t.as_os_str().as_bytes().ends_with(b".jsonl") {
                                open.push(t);
                            }
                        }
                    }
                }
            }
            let since = fs::metadata(format!("/proc/{pid}"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            out.push(Proc {
                pid,
                engine,
                args,
                cwd: fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned()),
                since,
                open,
            });
        }
        Some((out, parents))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::*;
    pub(super) fn procs() -> Option<(Vec<Proc>, Parents)> {
        None
    }
}
