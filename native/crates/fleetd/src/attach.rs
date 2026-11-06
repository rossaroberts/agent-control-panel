//! `fleetd attach`: the first terminal client. It puts the local tty into raw mode, opens one terminal on the daemon's
//! socket (a `role: Terminals` connection) and relays stdin and stdout through it, forwarding SIGWINCH as `TermSize`.
//! Ctrl-] (0x1d, as telnet's escape) detaches: it releases the terminal (the session itself is untouched) and restores
//! the tty. The tty is restored on every way out: a normal return, SIGINT/SIGTERM/SIGHUP, a panic.
//!
//! `fleetd attach --list` prints what can be attached, from a board connection.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleet_model::types::{AttachTarget, Engine, Session};
use fleet_model::wire::{Frame, TERM_IN_MAX};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot, Notify};

use crate::attach_cmd::{job_ok, profile_ok, uuid_ok};
use crate::client::{term_connect, Client};
use crate::out::state_word;
use crate::serve::socket_path;

pub const USAGE: &str = "usage: fleetd attach --list\n       fleetd attach <profile>/<job>\n       fleetd attach codex/<session-id>\n       fleetd attach --trust <profile> <dir>\n(Ctrl-] detaches)";

/// The escape byte: Ctrl-].
const DETACH: u8 = 0x1d;
/// The `ref` of the one terminal this connection holds.
const REF: u32 = 1;

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Req {
    List,
    /// `<profile>/<8-hex job>`
    Claude {
        profile: String,
        job: String,
    },
    /// `<profile>/<session uuid>`: the folder to resume in comes from the board.
    Codex {
        profile: String,
        session_id: String,
    },
    Trust {
        profile: String,
        dir: String,
    },
}

pub fn parse(args: &[String]) -> Result<Req, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["--list"] => Ok(Req::List),
        ["--trust", profile, dir] => {
            if !profile_ok(profile) {
                return Err(format!("not a profile name: {profile}"));
            }
            Ok(Req::Trust {
                profile: (*profile).into(),
                dir: (*dir).into(),
            })
        }
        [target] if !target.starts_with('-') => {
            let (profile, id) = target
                .split_once('/')
                .ok_or_else(|| format!("{target}: expected <profile>/<job>"))?;
            if !profile_ok(profile) {
                return Err(format!("not a profile name: {profile}"));
            }
            if job_ok(id) {
                Ok(Req::Claude {
                    profile: profile.into(),
                    job: id.into(),
                })
            } else if uuid_ok(id) {
                Ok(Req::Codex {
                    profile: profile.into(),
                    session_id: id.into(),
                })
            } else {
                Err(format!(
                    "{id}: neither an 8-hex job id nor a Codex session id"
                ))
            }
        }
        _ => Err(USAGE.into()),
    }
}

/// The attachable sessions as `<profile>/<id>  <state>  <name>` lines: Claude background jobs (they have a `job_id`)
/// and Codex threads. Sorted, so the output is the same run to run. Plain text always: a line is for a person or `cut`.
pub fn list_lines(sessions: &[Session]) -> Vec<String> {
    let mut rows: Vec<(String, String)> = sessions
        .iter()
        .filter_map(|s| {
            let id = match s.engine {
                Engine::Claude => s.job_id.as_deref()?,
                Engine::Codex => s.key.session_id.as_str(),
                Engine::Muse => return None,
            };
            let target = format!("{}/{id}", s.key.profile);
            let name = s.name.as_deref().unwrap_or(&s.key.session_id);
            let line = format!(
                "{target:<46} {:<11} {}",
                state_word(&s.state),
                name.replace(['\n', '\r'], " ")
            );
            Some((target, line))
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, l)| l).collect()
}

/// `data` as `TermIn` frames of at most 64 KiB each, in order.
pub fn input_frames(data: &[u8]) -> Vec<Frame> {
    data.chunks(TERM_IN_MAX)
        .map(|c| Frame::TermIn {
            r#ref: REF,
            data: c.to_vec(),
        })
        .collect()
}

/// What to send before detaching, and whether `data` holds a Ctrl-] (the first one ends it; what follows is dropped).
pub fn split_detach(data: &[u8]) -> (&[u8], bool) {
    match data.iter().position(|b| *b == DETACH) {
        Some(i) => (&data[..i], true),
        None => (data, false),
    }
}

// ---- the tty

static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// What a program may have switched on in the local terminal while attached: mouse reporting, bracketed paste,
/// application cursor keys, a hidden cursor, colours. Put back on the way out (a warm replay turns on again whatever the
/// next attach needs). The alternate screen is left only if the output entered it (`AltScreen`).
const RESET: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b[?25h\x1b[0m";
const ALT_ON: &[u8] = b"\x1b[?1049h";
const ALT_OFF: &[u8] = b"\x1b[?1049l";

/// Whether the output stream has the alternate screen on (read by `restore_tty`, which may run from a panic hook).
static ALT: AtomicBool = AtomicBool::new(false);

/// Follows `ESC[?1049h` / `ESC[?1049l` in the output across chunk boundaries (a sequence may be split).
#[derive(Default)]
struct AltScreen {
    tail: Vec<u8>,
}

impl AltScreen {
    fn feed(&mut self, data: &[u8]) {
        let mut all = std::mem::take(&mut self.tail);
        all.extend_from_slice(data);
        let mut state = None;
        for (i, w) in all.windows(ALT_ON.len()).enumerate() {
            if w == ALT_ON {
                state = Some((i, true));
            } else if w == ALT_OFF {
                state = Some((i, false));
            }
        }
        if let Some((_, on)) = state {
            ALT.store(on, Ordering::SeqCst);
        }
        let keep = all.len().min(ALT_ON.len() - 1);
        self.tail = all[all.len() - keep..].to_vec();
    }
}

/// Puts the tty back as it was, once. Safe to call from a panic hook and from `Drop`.
fn restore_tty() {
    let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(t) = saved {
        // SAFETY: plain syscalls on fds 0 and 1 with a termios we obtained from tcgetattr.
        unsafe {
            if libc::isatty(1) == 1 {
                libc::write(1, RESET.as_ptr().cast(), RESET.len());
                if ALT.load(Ordering::SeqCst) {
                    libc::write(1, ALT_OFF.as_ptr().cast(), ALT_OFF.len());
                }
            }
            libc::tcsetattr(0, libc::TCSAFLUSH, &t);
        }
    }
}

/// Raw mode for as long as it lives.
struct RawTty;

impl RawTty {
    fn enter() -> io::Result<RawTty> {
        // SAFETY: tcgetattr/cfmakeraw/tcsetattr on fd 0 with a zeroed termios we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                let e = io::Error::last_os_error();
                SAVED.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(e);
            }
        }
        Ok(RawTty)
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        restore_tty();
    }
}

/// The local window as (cols, rows).
fn winsize() -> Option<(u16, u16)> {
    for fd in [0, 1] {
        // SAFETY: TIOCGWINSZ fills a winsize.
        let ws = unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            (libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0).then_some(ws)
        };
        if let Some(ws) = ws.filter(|w| w.ws_col > 0 && w.ws_row > 0) {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

/// What the stdin thread hands on.
enum In {
    Keys(Vec<u8>),
    /// A Ctrl-] was typed; what followed it is dropped.
    Detach,
}

/// Reads fd 0 on a thread of its own (a tty read cannot be cancelled; the process exits instead). Chunks are at most
/// 64 KiB. The Ctrl-] scan is done here, ahead of any send that can wait on the daemon: the channel is unbounded (it
/// holds only what a person typed or pasted) so this thread never blocks behind a stalled terminal, and `detach` is
/// notified at once so a send that is waiting can be given up.
fn read_stdin(tx: mpsc::UnboundedSender<In>, detach: Arc<Notify>) {
    let _ = std::thread::Builder::new()
        .name("attach-stdin".into())
        .spawn(move || {
            let mut buf = vec![0u8; TERM_IN_MAX];
            loop {
                // SAFETY: reads into our buffer.
                let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if n <= 0 {
                    detach.notify_one(); // end of input detaches, as Ctrl-] does
                    return;
                }
                let (keys, d) = split_detach(&buf[..n as usize]);
                if !keys.is_empty() && tx.send(In::Keys(keys.to_vec())).is_err() {
                    return;
                }
                if d {
                    let _ = tx.send(In::Detach);
                    detach.notify_one();
                    return;
                }
            }
        });
}

// ---- the session

struct Outcome {
    code: i32,
    message: Option<String>,
}

fn fail(code: i32, m: impl Into<String>) -> Outcome {
    Outcome {
        code,
        message: Some(m.into()),
    }
}

fn clean() -> Outcome {
    Outcome {
        code: 0,
        message: None,
    }
}

/// How long the daemon gets to answer a `TermClose` before the client leaves anyway.
const CLOSE_WAIT: Duration = Duration::from_millis(400);
/// How long the writer gets to put a last `TermClose` on the socket when a signal ends the client.
const FLUSH_WAIT: Duration = Duration::from_millis(100);

async fn attach(path: &Path, target: AttachTarget) -> Outcome {
    let (mut tx, mut rx) = match term_connect(path).await {
        Ok(c) => c,
        Err(e) => {
            return fail(
                1,
                format!("{}: {e} (is `fleetd serve` running?)", path.display()),
            )
        }
    };
    let (Ok(mut winch), Ok(mut int), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::window_change()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return fail(1, "cannot install signal handlers");
    };
    let (ftx, mut frx) = mpsc::channel::<Frame>(16);
    let writer = tokio::spawn(async move {
        while let Some(f) = frx.recv().await {
            if tx.send(&f).await.is_err() {
                break;
            }
        }
    });
    let (cols, rows) = winsize().unwrap_or((100, 30));
    let tty = match RawTty::enter() {
        Ok(t) => t,
        Err(e) => return fail(1, format!("the terminal cannot be put in raw mode: {e}")),
    };
    let open = Frame::TermOpen {
        r#ref: REF,
        target,
        cols,
        rows,
    };
    if ftx.send(open).await.is_err() {
        return fail(1, "connection to the daemon closed");
    }

    // The pump: keystrokes and window changes to the daemon, in order, awaiting the daemon (its backpressure). It is a
    // task of its own so that output keeps being read while a send waits. Every send is raced against the detach
    // notice: a stalled terminal (the daemon has stopped reading our input) must not trap Ctrl-] (raw mode has no
    // Ctrl-C either).
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<In>();
    let detach = Arc::new(Notify::new());
    read_stdin(stdin_tx, detach.clone());
    let (detach_tx, mut detach_rx) = oneshot::channel::<()>();
    let pump_tx = ftx.clone();
    let pump = tokio::spawn(async move {
        // False when the send was given up (a detach) or the writer is gone.
        let send = |f: Frame| {
            let (tx, detach) = (&pump_tx, &detach);
            async move {
                tokio::select! {
                    r = tx.send(f) => r.is_ok(),
                    _ = detach.notified() => false,
                }
            }
        };
        'pump: loop {
            tokio::select! {
                msg = stdin_rx.recv() => match msg {
                    Some(In::Keys(keys)) => {
                        for f in input_frames(&keys) {
                            if !send(f).await {
                                break 'pump;
                            }
                        }
                    }
                    Some(In::Detach) | None => break,
                },
                _ = detach.notified() => break,
                _ = winch.recv() => {
                    if let Some((cols, rows)) = winsize() {
                        if !send(Frame::TermSize { r#ref: REF, cols, rows }).await {
                            break;
                        }
                    }
                }
            }
        }
        // Queued input is dropped. The close goes if the channel has room; if the daemon is not reading, the hang-up
        // that follows the exit releases the terminal.
        let _ = pump_tx.try_send(Frame::TermClose { r#ref: REF });
        let _ = detach_tx.send(());
    });

    let mut stdout = io::stdout().lock();
    let mut alt = AltScreen::default();
    // Set once, when the detach starts: the daemon gets until then to answer, however much output arrives meanwhile.
    let mut deadline: Option<tokio::time::Instant> = None;
    let end = loop {
        let wait = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            f = rx.next() => match f {
                Ok(Frame::TermOut { data, .. }) => {
                    alt.feed(&data);
                    if stdout.write_all(&data).and_then(|_| stdout.flush()).is_err() {
                        break fail(1, "cannot write to the terminal");
                    }
                }
                Ok(Frame::TermEnd { error: Some(e), .. }) => break fail(1, e),
                Ok(Frame::TermEnd { error: None, .. }) => break clean(),
                Ok(Frame::Error { message }) => break fail(1, message),
                Ok(_) => {}
                Err(_) if deadline.is_some() => break clean(),
                Err(_) => break fail(1, "connection to the daemon closed"),
            },
            Ok(()) = &mut detach_rx, if deadline.is_none() => {
                deadline = Some(tokio::time::Instant::now() + CLOSE_WAIT);
            }
            _ = wait => break clean(),
            _ = int.recv() => break fail(130, "interrupted"),
            _ = term.recv() => break fail(143, "terminated"),
            _ = hup.recv() => break fail(129, "hung up"),
        }
    };
    if end.code >= 128 {
        let _ = ftx.try_send(Frame::TermClose { r#ref: REF });
    }
    // Give the writer a moment to put what is queued on the socket (a signal's TermClose), then leave: the daemon
    // treats a dropped socket as a close anyway.
    pump.abort();
    drop(ftx);
    let _ = tokio::time::timeout(FLUSH_WAIT, writer).await;
    drop(stdout);
    drop(tty);
    end
}

// ---- the commands

/// The board, once: the opening snapshot.
async fn board(path: &Path) -> io::Result<Vec<Session>> {
    let mut c = Client::connect(path, None).await?;
    loop {
        if let Frame::Snapshot { sessions, .. } = c.next().await? {
            return Ok(sessions);
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a tokio runtime")
}

/// Runs the command; the process exit code.
pub fn run(args: &[String]) -> i32 {
    let req = match parse(args) {
        Ok(r) => r,
        Err(m) => {
            eprintln!("{m}");
            return 2;
        }
    };
    let path = socket_path();
    let rt = runtime();
    let code = rt.block_on(async {
        let target = match req {
            Req::List => {
                return match board(&path).await {
                    Ok(s) => {
                        let mut o = io::stdout().lock();
                        for l in list_lines(&s) {
                            if writeln!(o, "{l}").is_err() {
                                break;
                            }
                        }
                        0
                    }
                    Err(e) => {
                        eprintln!("fleetd: {}: {e}", path.display());
                        1
                    }
                };
            }
            Req::Claude { profile, job } => AttachTarget::Claude { profile, job },
            Req::Trust { profile, dir } => AttachTarget::Trust { profile, dir },
            Req::Codex {
                profile,
                session_id,
            } => match board(&path).await {
                Ok(s) => {
                    let found = s.iter().find(|x| {
                        x.engine == Engine::Codex
                            && x.key.session_id == session_id
                            && x.key.profile == profile
                    });
                    match found.and_then(|x| x.cwd.clone()) {
                        Some(cwd) => AttachTarget::Codex {
                            profile,
                            session_id,
                            cwd,
                        },
                        None => {
                            eprintln!("fleetd: no Codex session {profile}/{session_id} with a folder on the board");
                            return 1;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("fleetd: {}: {e}", path.display());
                    return 1;
                }
            },
        };
        // SAFETY: isatty only inspects the fds.
        if unsafe { libc::isatty(0) != 1 || libc::isatty(1) != 1 } {
            eprintln!("fleetd: attach needs a terminal on stdin and stdout");
            return 2;
        }
        // A panic puts the tty back before the process ends (the hook in main ends it after this one).
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_tty();
            prev(info)
        }));
        let out = attach(&path, target).await;
        if let Some(m) = out.message {
            eprintln!("fleetd: {m}");
        }
        out.code
    });
    // The stdin thread may still be blocked in read(2); ending the process is what ends it.
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn it_parses_the_forms() {
        assert_eq!(parse(&a(&["--list"])), Ok(Req::List));
        assert_eq!(
            parse(&a(&["work/0a1b2c3d"])),
            Ok(Req::Claude {
                profile: "work".into(),
                job: "0a1b2c3d".into()
            })
        );
        assert_eq!(
            parse(&a(&["codex/0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"])),
            Ok(Req::Codex {
                profile: "codex".into(),
                session_id: "0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()
            })
        );
        assert_eq!(
            parse(&a(&["--trust", "work", "~/x"])),
            Ok(Req::Trust {
                profile: "work".into(),
                dir: "~/x".into()
            })
        );
        for bad in [
            vec![],
            vec!["nojob"],
            vec!["work/xyz"],
            vec!["bad name/0a1b2c3d"],
            vec!["--trust", "work"],
            vec!["--frob"],
            vec!["a/0a1b2c3d", "b"],
        ] {
            assert!(parse(&a(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn input_is_split_at_64_kib() {
        let n = TERM_IN_MAX * 2 + 5;
        let fr = input_frames(&vec![7u8; n]);
        let lens: Vec<usize> = fr
            .iter()
            .map(|f| match f {
                Frame::TermIn { data, .. } => data.len(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(lens, vec![TERM_IN_MAX, TERM_IN_MAX, 5]);
        assert!(input_frames(&[]).is_empty());
    }

    #[test]
    fn ctrl_right_bracket_detaches_and_drops_what_follows() {
        assert_eq!(split_detach(b"abc"), (&b"abc"[..], false));
        assert_eq!(split_detach(b"ab\x1dcd"), (&b"ab"[..], true));
        assert_eq!(split_detach(b"\x1d"), (&b""[..], true));
    }

    #[test]
    fn the_alternate_screen_is_followed_across_chunks() {
        let mut a = AltScreen::default();
        ALT.store(false, Ordering::SeqCst);
        a.feed(b"hello \x1b[?10");
        assert!(!ALT.load(Ordering::SeqCst));
        a.feed(b"49h screen");
        assert!(ALT.load(Ordering::SeqCst), "entered, the sequence split");
        a.feed(b"\x1b[?1049l and \x1b[?1049h");
        assert!(ALT.load(Ordering::SeqCst), "the last one wins");
        a.feed(b"x\x1b[?1049");
        a.feed(b"l");
        assert!(!ALT.load(Ordering::SeqCst), "left");
    }
}
