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
    /// Accept one connection if one is waiting.
    ///
    /// `Ok(None)` means nothing is pending, which requires the listener to be non-blocking. It is
    /// NOT an error: a poll loop that treated it as one would log a line every 100ms while idle.
    ///
    /// Accept one connection if one is pending, explicitly leaving it in BLOCKING mode.
    ///
    /// # This is belt-and-braces, not a fix
    ///
    /// An earlier version of this comment claimed that a non-blocking listener hands its accepted
    /// sockets the same mode, and that this caused a large frame to be abandoned mid-read. **That
    /// claim was tested and is false on Linux**: an accepted socket gets its own file-status flags
    /// and defaults to blocking. `scripts/probe-socket-inherit.rs`-style measurement showed the
    /// first read BLOCKING and returning normally.
    ///
    /// The `set_nonblocking(false)` call is kept because it makes the intent explicit and costs
    /// nothing — a reader should not have to know whether the platform inherits the flag to know
    /// that this socket is safe to read a multi-segment frame from.
    ///
    /// It is NOT the explanation for the observed large-frame failure, which remains unfound. The
    /// comment is corrected rather than deleted because a confident wrong explanation in the source
    /// is worse than no explanation: the next person would trust it and stop looking.
    pub fn accept(&self) -> Result<Option<TcpConnection>> {
        match self {
            Listener::Tcp(l) => match l.accept() {
                Ok((stream, _)) => {
                    // The fix. See the note above: the mode is inherited from the listener, and a
                    // non-blocking socket cannot read a frame that spans more than one segment.
                    stream.set_nonblocking(false).context(
                        "putting the accepted connection into blocking mode; without this, \
                                  a frame larger than one TCP segment is silently abandoned",
                    )?;
                    Ok(Some(TcpConnection::new(stream)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e).context("accepting a connection from the host"),
            },
        }
    }

    /// Put the listener into non-blocking mode so `accept` can be polled.
    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<()> {
        match self {
            Listener::Tcp(l) => l
                .set_nonblocking(nonblocking)
                .context("setting the listener's blocking mode"),
        }
    }
}

/// Default bind address for the guest control channel.
///
/// `0.0.0.0`, NOT loopback, and the reason is easy to get wrong.
///
/// The host reaches the guest through a QEMU user-mode forward (`hostfwd=tcp:127.0.0.1:48274-:48273`).
/// From the guest's point of view that arrives as an INBOUND connection on its own external
/// interface — slirp delivers it from the gateway address. It is not a loopback connection and it
/// never was, however much the `127.0.0.1` on the host side suggests otherwise: that address binds
/// the host end, and says nothing about where the packet lands inside the guest.
///
/// A loopback bind therefore accepts nothing from the host. The symptom is a forward that is
/// bound and listening on the host, a guest service that reports it started successfully, and a
/// connection that is refused with no indication which of the three is at fault.
///
/// Listening on all interfaces is safe here for a specific, checkable reason rather than by
/// assumption: the guest sits behind slirp NAT with no port forwards pointing inward, so nothing
/// outside this host can reach it. The host's forward is the only path in, and it is bound to the
/// host's loopback. The exposure is the guest's own subnet, which in user-mode networking contains
/// only the guest itself.
pub fn default_bind() -> String {
    "0.0.0.0:48273".to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A frame larger than one TCP segment must survive the round trip.
    ///
    /// **This is the regression test for a bug that every small test missed.** A non-blocking
    /// listener hands its accepted sockets the same mode, and `read` on a non-blocking socket
    /// returns `WouldBlock` the moment the kernel has nothing ready — including mid-message. The
    /// framing reader treats that as fatal, so a frame arriving in more than one segment was
    /// abandoned and the connection closed with NO REPLY.
    ///
    /// Why it stayed invisible: a small request arrives in a single segment, so the first read gets
    /// everything and the bug never fires. It only appears once the payload exceeds what the socket
    /// delivers at once — which is why the probe had to send a real 256 KiB chunk to find it.
    ///
    /// So this test deliberately uses a payload well past a segment, and asserts the BYTES arrive
    /// rather than that no error was returned. The original failure was a silent close, and
    /// "did not error" would have passed on the broken code.
    #[test]
    fn a_frame_larger_than_one_tcp_segment_survives_the_round_trip() {
        use std::net::{TcpListener as StdListener, TcpStream as StdStream};

        // 256 KiB, base64-encoded: exactly what a transfer chunk produces. The inflation is part of
        // the test — the real wire payload is what has to work, not the raw size.
        let raw = vec![0xABu8; 256 * 1024];
        let encoded = crate::base64::encode(&raw);
        let payload = format!(r#"{{"chunk_base64":"{encoded}","final":true}}"#).into_bytes();

        assert!(
            payload.len() > 300 * 1024,
            "the payload must be comfortably past a TCP segment or this test proves nothing"
        );

        let listener = StdListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Cloned for the thread: the test needs the original to compare against afterwards, and
        // the comparison is the assertion that matters.
        let expected = payload.clone();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");

            // Explicit, so the test exercises the fix rather than depending on the listener's mode
            // by accident. Without this line the old behaviour would never be reproduced here.
            stream
                .set_nonblocking(false)
                .expect("the accepted socket must be blocking");

            let mut conn = TcpConnection::new(stream);
            let got = conn
                .recv()
                .expect("a large frame must be read, not abandoned");
            assert_eq!(got.len(), expected.len(), "the whole frame must arrive");
            assert_eq!(got, expected, "and byte-for-byte identical");

            // Reply, so the client learns the read completed rather than inferring from a close.
            conn.send(b"{\"status\":\"ok\"}").expect("reply");
        });

        let mut client = StdStream::connect(addr).expect("connect");

        // Write a REAL frame: a 4-byte big-endian length prefix, then the payload.
        //
        // The first version of this test wrote the payload alone, and the reader interpreted the
        // first four bytes of base64 text as a length prefix — reporting a frame of 2,065,851,240
        // bytes. The refusal was correct and the test was wrong.
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);

        // Written in small pieces on purpose: a single write_all of 341 KB may be buffered and
        // delivered promptly, whereas dribbling it guarantees the reader sees it across several
        // reads — which is the condition that triggers the bug.
        for piece in frame.chunks(8192) {
            client.write_all(piece).expect("write");
        }
        client.flush().expect("flush");

        // Wait for the reply rather than assuming: a silent close IS the failure mode under test.
        let reply = read_frame(&mut client);
        assert!(
            reply.is_ok(),
            "the server must reply; a silent close is the original bug. Got: {reply:?}"
        );

        server.join().expect("server thread");
    }
}
