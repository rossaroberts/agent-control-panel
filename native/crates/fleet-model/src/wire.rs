//! The socket protocol between `fleetd` and its clients. A frame is a `u32` little-endian length, then that many bytes of
//! postcard `Frame`. Postcard is positional: append variants at the end, and bump `PROTO` when a shape changes.

use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::types::{Delta, Session};

pub const PROTO: u16 = 1;

/// The biggest frame a reader accepts, so a corrupt length cannot ask for gigabytes.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Frame {
    /// Client to server, first and only: `since` is the `seq` of the last delta the client holds.
    Hello {
        proto: u16,
        since: Option<u64>,
    },
    Snapshot {
        seq: u64,
        machine: String,
        sessions: Vec<Session>,
    },
    Delta(Delta),
    Error {
        message: String,
    },
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let body = postcard::to_stdvec(frame).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    w.write_all(&out).await?;
    w.flush().await
}

/// The next frame. `UnexpectedEof` when the peer closed between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
