//! The socket protocol between `fleetd` and its clients (`native/PROTOCOL.md` is the contract the Swift app reads). A
//! frame is a `u32` little-endian length, then that many bytes of postcard `Frame`.
//!
//! Postcard is positional: field order and variant order are the format. The bytes are pinned by the golden frames in
//! `crates/fleet-model/tests/golden/`; **any change to those bytes is a `PROTO` bump** (and new golden files). Two
//! things never move, so that a client of any version can be told it is out of date: `Hello` is variant 0 and starts
//! with `proto`, and `Error` is variant 3 with one `message` string.

use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::types::{AttachTarget, Delta, Session};

/// 2: `run` in `Hello` and `Snapshot` (Ruling R24). 3: `role` in `Hello`, the terminal frames, `job_id` in `Session`.
pub const PROTO: u16 = 3;

/// What a connection is for, said in its `Hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The board: a snapshot, then deltas.
    Board,
    /// Terminals: the client opens and drives ptys with the `Term*` frames.
    Terminals,
}

/// The most a `TermIn` carries.
pub const TERM_IN_MAX: usize = 64 * 1024;

/// The most a frame of a Terminals connection's client can weigh: a full `TermIn` plus its framing, and a `TermOpen`
/// with room to spare.
pub const TERM_CLIENT_MAX: u32 = TERM_IN_MAX as u32 + 1024;

/// The biggest frame a reader accepts, so a corrupt length cannot ask for gigabytes.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Client to server, first and only: `since` is the `seq` of the last delta the client holds, and `run` the run of
    /// the daemon it came from. `since` is honoured only when `run` is this daemon's; otherwise the client gets a
    /// snapshot.
    Hello {
        proto: u16,
        since: Option<u64>,
        run: Option<u64>,
        role: Role,
    },
    /// The whole board. `run` is the daemon run (random at each start): a `seq` means something only within one run.
    Snapshot {
        run: u64,
        seq: u64,
        machine: String,
        sessions: Vec<Session>,
    },
    Delta(Delta),
    Error {
        message: String,
    },
    /// Client to server on a Terminals connection: open a terminal on `target`, named `ref` by the client.
    TermOpen {
        r#ref: u32,
        target: AttachTarget,
        cols: u16,
        rows: u16,
    },
    /// Client to server: keystrokes for terminal `ref` (at most `TERM_IN_MAX` bytes).
    TermIn {
        r#ref: u32,
        data: Vec<u8>,
    },
    /// Client to server: the terminal's window changed size.
    TermSize {
        r#ref: u32,
        cols: u16,
        rows: u16,
    },
    /// Client to server: release terminal `ref` (ends the attach only).
    TermClose {
        r#ref: u32,
    },
    /// Server to client: output of terminal `ref`.
    TermOut {
        r#ref: u32,
        data: Vec<u8>,
    },
    /// Server to client: terminal `ref` is over (`error` says why when it could not open or broke).
    TermEnd {
        r#ref: u32,
        error: Option<String>,
    },
}

/// A frame as it goes on the wire: length, then body.
pub fn encode(frame: &Frame) -> io::Result<Vec<u8>> {
    let body = postcard::to_stdvec(frame).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// A frame's body (what follows the length).
pub fn decode(body: &[u8]) -> io::Result<Frame> {
    postcard::from_bytes(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The `proto` of a body that starts as a `Hello` of any version does (variant 0, then `proto`), even when the rest is
/// another version's shape: what lets an old client be told which version this is.
pub fn hello_proto(body: &[u8]) -> Option<u16> {
    match postcard::take_from_bytes::<(u32, u16)>(body) {
        Ok(((0, proto), _)) => Some(proto),
        _ => None,
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    w.write_all(&encode(frame)?).await?;
    w.flush().await
}

/// The next frame, whose body may be at most `max` bytes (a peer's length is not trusted: memory grows only as bytes
/// arrive). `UnexpectedEof` when the peer closed between frames or mid-frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: u32) -> io::Result<Frame> {
    decode(&read_body(r, max).await?)
}

/// The next frame's body, undecoded (as `read_frame`).
pub async fn read_body<R: AsyncRead + Unpin>(r: &mut R, max: u32) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len);
    if len > max.min(MAX_FRAME) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
    }
    let mut body = Vec::new();
    let got = r.take(len as u64).read_to_end(&mut body).await?;
    if got < len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(body)
}
