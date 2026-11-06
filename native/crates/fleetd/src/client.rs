//! A client of the socket, for `fleetd watch` and the tests. The Swift app mirrors this: connect, say Hello, read frames.

use std::io;
use std::path::Path;

use fleet_model::wire::{read_frame, write_frame, Frame, MAX_FRAME, PROTO};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

pub struct Client {
    r: OwnedReadHalf,
    /// Kept open: the server treats a closed client as gone.
    _w: OwnedWriteHalf,
}

impl Client {
    /// Connects and says Hello with `since`.
    pub async fn connect(path: &Path, since: Option<u64>) -> io::Result<Client> {
        Self::connect_proto(path, PROTO, since).await
    }

    pub async fn connect_proto(path: &Path, proto: u16, since: Option<u64>) -> io::Result<Client> {
        let (r, mut w) = UnixStream::connect(path).await?.into_split();
        write_frame(&mut w, &Frame::Hello { proto, since }).await?;
        Ok(Client { r, _w: w })
    }

    /// The next frame; `UnexpectedEof` when the server closed.
    pub async fn next(&mut self) -> io::Result<Frame> {
        read_frame(&mut self.r, MAX_FRAME).await
    }
}
