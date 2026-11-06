//! The socket: one Unix listener, mode 0600. A client says `Hello` and gets the board (or what it missed), then every
//! delta. Each connection is its own task over its own broadcast receiver, so a slow client costs the store and the
//! others nothing: if it falls more than the ring behind it is sent a fresh snapshot. A `role: Terminals` connection
//! drives `Terms` instead (see `terminals`); the two kinds share nothing but the listener.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fleet_model::wire::{
    decode, hello_proto, read_body, write_frame, Frame, Role, PROTO, TERM_CLIENT_MAX, TERM_IN_MAX,
};

/// The most a Hello can weigh.
const HELLO_MAX: u32 = 4096;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncReadExt;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::store::Store;
use crate::terms::{term_sink, ConnId, TermSink, Terms};

/// How long a client has to say Hello, and how long one write may wait on a client that reads nothing.
const HELLO_WITHIN: Duration = Duration::from_secs(10);
const WRITE_WITHIN: Duration = Duration::from_secs(30);

/// Where the socket lives (`FLEETD_SOCK`, else the platform's place; see Global Constraints).
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("FLEETD_SOCK").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = fleet_read::home::home_dir();
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Fleet/fleetd.sock");
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
        Some(d) => PathBuf::from(d).join("fleetd.sock"),
        None => home.join(".local/state/fleet/fleetd.sock"),
    }
}

/// Holds the socket's lock for the daemon's lifetime; dropping it removes the socket (only ever while the lock is
/// held: the file closes after `drop` runs).
pub struct SocketGuard {
    path: PathBuf,
    _lock: std::fs::File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Binds the socket with mode 0600 under an exclusive lock on `<path>.lock` (`AddrInUse` if another daemon holds it).
/// A leftover socket is replaced only when it is a socket and nothing answers (refused or gone); any other file at the
/// path, or any other connect error, is left alone and reported.
pub fn bind(path: &Path) -> io::Result<(UnixListener, SocketGuard)> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_name)?;
    // SAFETY: flock on an fd we own; the lock lives as long as `lock` stays open.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another fleetd holds {}", Path::new(&lock_name).display()),
            )
        } else {
            e
        });
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket; left alone", path.display()),
            ))
        }
        Ok(_) => match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("fleetd already listening on {}", path.display()),
                ))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path)?
            }
            Err(e) => return Err(e),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Bound inside a private folder and renamed into place once it is 0600, so the socket is never reachable with a looser
    // mode (a umask would do it, but that is the whole process's).
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let tmp = path.with_file_name(format!(".fleetd-bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::DirBuilder::new().mode(0o700).create(&tmp)?;
    let staged = tmp.join("s");
    let moved: io::Result<UnixListener> = (|| {
        let l = UnixListener::bind(&staged)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&staged, path)?;
        Ok(l)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    Ok((
        moved?,
        SocketGuard {
            path: path.to_path_buf(),
            _lock: lock,
        },
    ))
}

/// While a `TermIn` waits on a full input queue, how often it is offered again.
const STALL_POLL: Duration = Duration::from_millis(5);

/// Accepts clients until the task is dropped: board connections over `store`, terminal connections over `terms`.
pub async fn serve(listener: UnixListener, store: Arc<Store>, machine: String, terms: Arc<Terms>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (store, machine, terms) = (store.clone(), machine.clone(), terms.clone());
                tokio::spawn(async move {
                    let _ = handle(stream, store, machine, terms).await;
                });
            }
            Err(e) => {
                eprintln!("fleetd: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn send(w: &mut OwnedWriteHalf, f: &Frame) -> io::Result<()> {
    timeout(WRITE_WITHIN, write_frame(w, f))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client not reading"))?
}

fn snapshot(store: &Store, machine: &str) -> (u64, Frame) {
    let (seq, sessions) = store.snapshot();
    let machine = machine.to_string();
    (
        seq,
        Frame::Snapshot {
            run: store.run(),
            seq,
            machine,
            sessions,
        },
    )
}

async fn handle(
    stream: UnixStream,
    store: Arc<Store>,
    machine: String,
    terms: Arc<Terms>,
) -> io::Result<()> {
    let (mut r, mut w) = stream.into_split();
    let body = timeout(HELLO_WITHIN, read_body(&mut r, HELLO_MAX))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello"))??;
    // A Hello of another version may not decode as this one's: its `proto` is still read (wire.rs), to say so.
    let hello = decode(&body);
    let proto = match &hello {
        Ok(Frame::Hello { proto, .. }) => Some(*proto),
        _ => hello_proto(&body),
    };
    let since = match (hello, proto) {
        (
            Ok(Frame::Hello {
                role: Role::Terminals,
                ..
            }),
            Some(PROTO),
        ) => return terminals(r, w, terms).await,
        // `since` counts within one run only (R24): from another run, or with no run, it gets a snapshot.
        (Ok(Frame::Hello { since, run, .. }), Some(PROTO)) => {
            since.filter(|_| run == Some(store.run()))
        }
        // This version's Hello, but its bytes are not: not a version problem.
        (Err(_), Some(PROTO)) => {
            let message = "malformed Hello".to_string();
            return send(&mut w, &Frame::Error { message }).await;
        }
        (_, Some(proto)) => {
            let message = format!("protocol {proto} not supported; this is proto {PROTO}");
            return send(&mut w, &Frame::Error { message }).await;
        }
        _ => {
            let message = "expected Hello".to_string();
            return send(&mut w, &Frame::Error { message }).await;
        }
    };

    // Subscribe before reading the board: a delta between the two is then in the receiver, not lost; the `seq` check
    // below drops the ones the board already holds.
    let mut rx = store.subscribe();
    let mut last = match since.and_then(|n| store.since(n).map(|d| (n, d))) {
        Some((_, d)) => {
            // Caught up gets the same empty delta as an acknowledgement (Ruling R18).
            let seq = d.seq;
            send(&mut w, &Frame::Delta(d)).await?;
            seq
        }
        None => {
            let (seq, f) = snapshot(&store, &machine);
            send(&mut w, &f).await?;
            seq
        }
    };

    let mut scratch = [0u8; 64];
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Ok(d) => {
                    if d.seq > last {
                        last = d.seq;
                        send(&mut w, &Frame::Delta(d)).await?;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    let (seq, f) = snapshot(&store, &machine);
                    last = seq;
                    send(&mut w, &f).await?;
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            // The client sends nothing after Hello; this only notices it hanging up.
            n = r.read(&mut scratch) => if n? == 0 { return Ok(()) },
        }
    }
}

// ------------------------------------------------------------------------------------------------- terminals

/// Numbers terminal connections for `Terms`.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// A `role: Terminals` connection: frames from the client drive `terms`; the connection's sink (pty output and ends) and
/// this layer's own answers go back. No write timeout: a client that reads slowly only stops its own ptys (backpressure in
/// `Terms`), so it is kept. Nothing here takes the board's lock.
async fn terminals(
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    terms: Arc<Terms>,
) -> io::Result<()> {
    let conn: ConnId = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (sink, source) = term_sink();
    // This layer's answers (a refused open, an `Error`). Bounded: a client that sends opens it cannot all be answered
    // for, because it does not read, stops being read.
    let (ctl, mut ctl_rx) = mpsc::channel::<Frame>(16);
    // In a block of its own: both halves (and their borrow of `source`) are gone before the cleanup.
    let res = {
        let reader = read_terms(&mut r, &terms, conn, sink, ctl);
        let writer = async {
            loop {
                let f = tokio::select! {
                    biased;
                    c = ctl_rx.recv() => match c {
                        Some(f) => f,
                        None => return io::Result::Ok(()), // the reader has finished, and its last word is out
                    },
                    f = source.recv() => f,
                };
                write_frame(&mut w, &f).await?;
            }
        };
        tokio::pin!(reader, writer);
        tokio::select! {
            r = &mut reader => {
                // The reader's last word (an `Error`) still goes out, if the client takes it soon.
                let _ = timeout(Duration::from_secs(1), &mut writer).await;
                r
            }
            w = &mut writer => w,
        }
    };
    let t = terms.clone();
    let _ = tokio::task::spawn_blocking(move || t.drop_conn(conn)).await;
    drop(source);
    res
}

/// Reads the client's frames until it hangs up or breaks a rule (then its `Error` is the last thing queued).
async fn read_terms(
    r: &mut OwnedReadHalf,
    terms: &Arc<Terms>,
    conn: ConnId,
    sink: TermSink,
    ctl: mpsc::Sender<Frame>,
) -> io::Result<()> {
    let refuse = |message: String| {
        let ctl = ctl.clone();
        async move {
            let _ = ctl.send(Frame::Error { message }).await;
        }
    };
    loop {
        let body = match read_body(r, TERM_CLIENT_MAX).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                refuse(e.to_string()).await;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let Ok(frame) = decode(&body) else {
            refuse("malformed frame".into()).await;
            return Ok(());
        };
        match frame {
            Frame::TermOpen {
                r#ref,
                target,
                cols,
                rows,
            } => {
                // A spawn blocks (~1 ms, under the terminals' lock): off the runtime. The next frame waits for it, so a
                // `TermIn` right behind its `TermOpen` finds the terminal open.
                let (t, s) = (terms.clone(), sink.clone());
                let opened =
                    tokio::task::spawn_blocking(move || t.open(conn, r#ref, target, cols, rows, s))
                        .await
                        .map_err(io::Error::other)?;
                if let Err(error) = opened {
                    let end = Frame::TermEnd {
                        r#ref,
                        error: Some(error),
                    };
                    if ctl.send(end).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Frame::TermIn { r#ref, data } => {
                if data.len() > TERM_IN_MAX {
                    let n = data.len();
                    refuse(format!("TermIn of {n} bytes; at most {TERM_IN_MAX}")).await;
                    return Ok(());
                }
                if !offer(r, terms, conn, r#ref, &data).await {
                    return Ok(());
                }
            }
            Frame::TermSize { r#ref, cols, rows } => terms.size(conn, r#ref, cols, rows),
            Frame::TermClose { r#ref } => terms.close(conn, r#ref),
            _ => {
                refuse("unexpected frame on a Terminals connection".into()).await;
                return Ok(());
            }
        }
    }
}

/// Hands `data` to terminal `ref`, holding it (and reading nothing more from this client) while the terminal's input queue
/// is full: it is offered again every 5 ms until it is taken, or the terminal has ended (then it is let go). Keystrokes are
/// never dropped (Ruling P2-R4). Re-offering is what "until `input_pending` drops" comes to for this one terminal: the
/// connection-wide `input_pending` is the fullest of its terminals, which may be another one. False when the client hung
/// up meanwhile.
async fn offer(r: &OwnedReadHalf, terms: &Terms, conn: ConnId, r#ref: u32, data: &[u8]) -> bool {
    while terms.input(conn, r#ref, data).is_err() {
        tokio::time::sleep(STALL_POLL).await;
        if hung_up(r) {
            return false;
        }
    }
    true
}

/// The client has closed its end (read without taking anything: a frame waiting is left for the reader).
fn hung_up(r: &OwnedReadHalf) -> bool {
    use std::os::fd::AsRawFd;
    let fd = r.as_ref().as_raw_fd();
    let mut b = 0u8;
    // SAFETY: a one-byte peek into a local, on an fd the stream owns for as long as `r` lives.
    let n = unsafe {
        libc::recv(
            fd,
            (&mut b as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}
