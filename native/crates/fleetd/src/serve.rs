//! The socket: one Unix listener, mode 0600. A client says `Hello` and gets the board (or what it missed), then every
//! delta. Each connection is its own task over its own broadcast receiver, so a slow client costs the store and the
//! others nothing: if it falls more than the ring behind it is sent a fresh snapshot.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fleet_model::wire::{read_frame, write_frame, Frame, PROTO};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::store::Store;

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

/// Binds the socket with mode 0600. A socket file left by a dead daemon is replaced; one that answers is not.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("fleetd already listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
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
    let l = moved?;
    Ok(l)
}

/// Accepts clients until the task is dropped.
pub async fn serve(listener: UnixListener, store: Arc<Store>, machine: String) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (store, machine) = (store.clone(), machine.clone());
                tokio::spawn(async move {
                    let _ = handle(stream, store, machine).await;
                });
            }
            Err(e) => {
                eprintln!("fleetd: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, f: &Frame) -> io::Result<()> {
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
            seq,
            machine,
            sessions,
        },
    )
}

async fn handle(stream: UnixStream, store: Arc<Store>, machine: String) -> io::Result<()> {
    let (mut r, mut w) = stream.into_split();
    let hello = timeout(HELLO_WITHIN, read_frame(&mut r))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello"))??;
    let since = match hello {
        Frame::Hello { proto, since } if proto == PROTO => since,
        Frame::Hello { proto, .. } => {
            let message = format!("protocol {proto} not supported; this is {PROTO}");
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
        Some((n, d)) => {
            if d.seq > n {
                let seq = d.seq;
                send(&mut w, &Frame::Delta(d)).await?;
                seq
            } else {
                n
            }
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
