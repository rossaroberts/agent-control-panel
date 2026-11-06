//! A client of the socket, for `fleetd watch` and the tests. The Swift app mirrors this: connect, say Hello, read frames.
//! Over the Unix socket, or over TCP (`net`): the same conversation, after the key preamble when one is given.

use std::io;
use std::path::Path;
use std::time::Duration;

use fleet_model::wire::{encode, read_frame, write_frame, Frame, Role, MAX_FRAME, PROTO};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

/// How long a TCP connect may take before it is given up (the hub's remotes need to fail within 5 s).
pub const CONNECT_WITHIN: Duration = Duration::from_secs(5);

type Rd = Box<dyn AsyncRead + Unpin + Send>;
type Wr = Box<dyn AsyncWrite + Unpin + Send>;

pub struct Client {
    r: Rd,
    /// Kept open: the server treats a closed client as gone.
    _w: Wr,
}

impl Client {
    /// Connects and says Hello: `None` for the board, or `Some((run, seq))` (the run of the last snapshot, the `seq` of
    /// the last frame) to be sent only what came after, when the daemon is still that run.
    pub async fn connect(path: &Path, resume: Option<(u64, u64)>) -> io::Result<Client> {
        Self::connect_proto(path, PROTO, resume).await
    }

    pub async fn connect_proto(
        path: &Path,
        proto: u16,
        resume: Option<(u64, u64)>,
    ) -> io::Result<Client> {
        let (r, mut w) = UnixStream::connect(path).await?.into_split();
        write_frame(&mut w, &board_hello(proto, resume)).await?;
        Ok(Client {
            r: Box::new(r),
            _w: Box::new(w),
        })
    }

    /// The same over TCP (`host:port`), within `CONNECT_WITHIN`, with keepalive and `TCP_NODELAY` (`net::keepalive`).
    /// `key` is sent as the preamble first: needed on loopback on macOS (Ruling P5-R2); a Tailscale peer sends none.
    pub async fn connect_tcp(
        addr: &str,
        key: Option<[u8; 32]>,
        resume: Option<(u64, u64)>,
    ) -> io::Result<Client> {
        let (r, w) = tcp(addr, key, &board_hello(PROTO, resume)).await?;
        Ok(Client { r, _w: w })
    }

    /// The next frame; `UnexpectedEof` when the server closed.
    pub async fn next(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.r, MAX_FRAME).await
    }
}

fn board_hello(proto: u16, resume: Option<(u64, u64)>) -> Frame {
    Frame::Hello {
        proto,
        since: resume.map(|r| r.1),
        run: resume.map(|r| r.0),
        role: Role::Board,
    }
}

fn term_hello() -> Frame {
    Frame::Hello {
        proto: PROTO,
        since: None,
        run: None,
        role: Role::Terminals,
    }
}

/// Dials `addr`, then writes the preamble (if any) and `hello` in one write.
async fn tcp(addr: &str, key: Option<[u8; 32]>, hello: &Frame) -> io::Result<(Rd, Wr)> {
    let s = tokio::time::timeout(CONNECT_WITHIN, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {addr}: timed out"),
            )
        })??;
    crate::net::keepalive(&s)?;
    let (r, mut w) = s.into_split();
    let mut out = Vec::with_capacity(36 + 16);
    if let Some(k) = key {
        out.extend_from_slice(crate::net::FKEY);
        out.extend_from_slice(&k);
    }
    out.extend_from_slice(&encode(hello)?);
    w.write_all(&out).await?;
    w.flush().await?;
    Ok((Box::new(r), Box::new(w)))
}

/// The send half of a Terminals connection (`fleetd attach`, the terminal bench).
pub struct TermTx {
    w: Wr,
}

/// The receive half: `TermOut` and `TermEnd` frames, and an `Error` before the server hangs up.
pub struct TermRx {
    r: Rd,
}

/// Connects with `role: Terminals`. The two halves are separate so a caller can read output while its own write waits
/// on a server that has stopped reading input (the server's backpressure): a single task doing both could deadlock.
pub async fn term_connect(path: &Path) -> io::Result<(TermTx, TermRx)> {
    let (r, mut w) = UnixStream::connect(path).await?.into_split();
    write_frame(&mut w, &term_hello()).await?;
    Ok((TermTx { w: Box::new(w) }, TermRx { r: Box::new(r) }))
}

/// The same over TCP, as `Client::connect_tcp`.
pub async fn term_connect_tcp(addr: &str, key: Option<[u8; 32]>) -> io::Result<(TermTx, TermRx)> {
    let (r, w) = tcp(addr, key, &term_hello()).await?;
    Ok((TermTx { w }, TermRx { r }))
}

impl TermTx {
    pub async fn send(&mut self, frame: &Frame) -> io::Result<()> {
        write_frame(&mut self.w, frame).await
    }
}

impl TermRx {
    /// The next frame; `UnexpectedEof` when the server closed.
    pub async fn next(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.r, MAX_FRAME).await
    }
}
