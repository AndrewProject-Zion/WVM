//! Connection transport.
//!
//! `Connection` is the seam between the wire protocol and the underlying byte channel. The
//! default implementation is TCP over the guest's private link to the host; the trait exists so
//! that a `vsock` backend can be added later without touching dispatch.
//!
//! Why TCP is the default rather than virtio-vsock, in one line: Windows has no native
//! `AF_VSOCK`, the virtio-win `viosock` driver only entered the release at build 285, and the
//! two shipping projects in this space both rejected it. Full reasoning in
//! `docs/DECISIONS.md` D-002.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use anyhow::{Context, Result};
use wvm_ipc::{read_frame, write_frame};

/// Anything that can carry length-prefixed frames.
pub trait Connection: Send {
    fn recv(&mut self) -> Result<Vec<u8>>;
    fn send(&mut self, payload: &[u8]) -> Result<()>;
}

/// TCP implementation, used as the default.
pub struct TcpConnection {
    stream: TcpStream,
}

impl TcpConnection {
    pub fn new(stream: TcpStream) -> Self {
        TcpConnection { stream }
    }
}

impl Connection for TcpConnection {
    fn recv(&mut self) -> Result<Vec<u8>> {
        Ok(read_frame(&mut self.stream)?)
    }

    fn send(&mut self, payload: &[u8]) -> Result<()> {
        Ok(write_frame(&mut self.stream, payload)?)
    }
}

/// A listener that yields connections.
pub enum Listener {
    Tcp(TcpListener),
}

impl Listener {
    pub fn incoming(&self) -> Box<dyn Iterator<Item = Result<TcpConnection>> + '_> {
        match self {
            Listener::Tcp(l) => Box::new(l.incoming().map(|r| {
                r.map(TcpConnection::new)
                    .context("accepting a connection from the host")
            })),
        }
    }
}

/// Default bind address: loopback on a fixed port, reachable only over the host↔guest link.
///
/// Loopback rather than `0.0.0.0` on purpose. The control channel has no business being
/// reachable from the guest's own network neighbours, and the host's forward reaches loopback
/// fine.
pub fn default_bind() -> String {
    "127.0.0.1:48273".to_string()
}

pub fn listen(addr: &str) -> Result<Listener> {
    let listener = TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    Ok(Listener::Tcp(listener))
}

/// Report framing failures as a distinct type, so a truncated frame is not mistaken for a
/// protocol violation. The platform layer maps to this once it is written (M4).
#[derive(Debug)]
#[allow(dead_code)]
pub enum TransportError {
    Closed,
    Frame(wvm_ipc::FrameError),
    Io(std::io::Error),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Closed => write!(f, "peer closed the connection"),
            TransportError::Frame(e) => write!(f, "framing: {e}"),
            TransportError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Write then read one frame. Used by the host's handshake and by tests.
///
/// Kept public rather than test-only: the handshake in M2 needs exactly this shape, and a
/// helper that only exists under `cfg(test)` tends to get reimplemented instead of reused.
#[allow(dead_code)]
pub fn round_trip<S: Read + Write>(stream: &mut S, payload: &[u8]) -> Result<Vec<u8>> {
    write_frame(stream, payload)?;
    Ok(read_frame(stream)?)
}
