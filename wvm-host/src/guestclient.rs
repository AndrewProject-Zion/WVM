//! A minimal host-side client for the guest control channel.
//!
//! # Why this exists
//!
//! The host had no way to talk to the guest from the `wvm` binary. Everything so far went through
//! Python scripts (`scripts/talk-to-guest.py`), which is fine for probing and wrong for a shipped
//! verb: a user typing `wvm vm transfer` should not need a Python interpreter in the loop.
//!
//! # What it is not
//!
//! Not a session, not a connection pool, not reconnect logic. It opens a socket to the guest's
//! forwarded port, sends one request, reads one response, and closes. The guest service answers one
//! request at a time, so a persistent connection would buy nothing and would need a reconnection
//! policy for the case where the guest restarts underneath it.
//!
//! # The forward, and why a connect proves nothing
//!
//! The port reached here is the host side of a QEMU user-mode (slirp) forward. slirp completes the
//! TCP handshake **locally** and only then attempts delivery to the guest, so a successful connect
//! says nothing about whether anything is listening. This module therefore treats the framed reply
//! as the only evidence of reachability — see D-008. A connect that times out on the first frame is
//! reported as "the guest did not answer", not as a connection failure.

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result};
use wvm_ipc::{read_frame, write_frame, Request, Response};

/// How long to wait for the guest to reply.
///
/// Generous relative to a local forward, because the guest service handles one request at a time and
/// a transfer of a large file is genuinely slow. The value bounds a *stalled* guest, not a busy one.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Send one request and return the reply.
///
/// Errors distinguish the two failure modes that otherwise look identical to a caller:
///
/// - **the connect failed** — nothing is listening on the host side of the forward
/// - **the reply did not arrive** — the forward accepted, but the guest never answered
///
/// The second is the interesting one, because slirp makes it look like success.
pub fn request(addr: &str, req: &Request) -> Result<Response> {
    let stream = TcpStream::connect(addr)
        .with_context(|| format!("connecting to the guest control channel at {addr}"))?;

    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .context("setting a read timeout on the control channel")?;
    stream
        .set_write_timeout(Some(REPLY_TIMEOUT))
        .context("setting a write timeout on the control channel")?;

    let mut writer = &stream;
    let encoded = serde_json::to_vec(req).context("encoding the request")?;
    write_frame(&mut writer, &encoded).context("sending the request")?;
    writer.flush().context("flushing the request")?;

    let mut reader = &stream;
    match read_frame(&mut reader) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parsing the guest's reply as wvm-ipc"),
        Err(e) => Err(anyhow::anyhow!(
            "the guest accepted the connection but did not answer: {e}.\n\
             The connect succeeding is not evidence the guest is up — QEMU's user-mode networking \
             completes the handshake on the host's behalf before attempting delivery (D-008).\n\
             Check the guest service is running, and that a harness is not holding the port."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_to_a_closed_port_is_reported_as_a_connect_failure() {
        // Port 1 on loopback: nothing listens there, and it needs no privileges to attempt.
        let err = request("127.0.0.1:1", &Request::Inspect).expect_err("must fail");
        let text = format!("{err:#}");
        assert!(
            text.contains("connecting to the guest control channel"),
            "the error must name the connect, not the reply: {text}"
        );
    }
}
