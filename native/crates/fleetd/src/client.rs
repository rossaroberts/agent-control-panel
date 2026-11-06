//! A client of the socket, for `fleetd watch` and the tests. The Swift app mirrors this: connect, say Hello, read frames.

use std::io;
use std::path::Path;

use fleet_model::wire::{read_frame, write_frame, Frame, Role, MAX_FRAME, PROTO};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub struct Client {
    r: OwnedReadHalf,
    /// Kept open: the server treats a closed client as gone.
    _w: OwnedWriteHalf,
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
        let hello = Frame::Hello {
            proto,
            since: resume.map(|r| r.1),
            run: resume.map(|r| r.0),
            role: Role::Board,
        };
        write_frame(&mut w, &hello).await?;
        Ok(Client { r, _w: w })
    }

    /// The next frame; `UnexpectedEof` when the server closed.
    pub async fn next(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.r, MAX_FRAME).await
    }
}

/// The send half of a Terminals connection (`fleetd attach`, the terminal bench).
pub struct TermTx {
    w: OwnedWriteHalf,
}

/// The receive half: `TermOut` and `TermEnd` frames, and an `Error` before the server hangs up.
pub struct TermRx {
    r: OwnedReadHalf,
}

/// Connects with `role: Terminals`. The two halves are separate so a caller can read output while its own write waits
/// on a server that has stopped reading input (the server's backpressure): a single task doing both could deadlock.
pub async fn term_connect(path: &Path) -> io::Result<(TermTx, TermRx)> {
    let (r, mut w) = UnixStream::connect(path).await?.into_split();
    let hello = Frame::Hello {
        proto: PROTO,
        since: None,
        run: None,
        role: Role::Terminals,
    };
    write_frame(&mut w, &hello).await?;
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
