//! Request dispatch inside the guest.
//!
//! The guest is a **dumb executor**. It does exactly what the request says and reports what
//! happened. It does not decide whether an action is allowed — the host already did that, and a
//! second opinion here would only create a place for the two to disagree (`docs/DECISIONS.md`
//! D-003).
//!
//! Operations that are not yet implemented return an explicit `Error` saying so. Returning a
//! plausible-looking success from a stub is the failure mode this project is most keen to avoid:
//! it makes an incomplete system indistinguishable from a broken one.

#![allow(dead_code)] // The platform layer below this is not written yet; see docs/BUILD-PLAN.md M4.

use anyhow::{bail, Result};
use wvm_ipc::{Payload, Request, Response, PROTOCOL_VERSION};

use crate::transport::Connection;

/// Serve one connection until the peer disconnects.
pub fn serve<C: Connection>(mut conn: C) -> Result<()> {
    loop {
        let bytes = match conn.recv() {
            Ok(b) => b,
            // A clean close at a frame boundary is a normal end of session.
            Err(_) => return Ok(()),
        };

        let request: Request = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                // A malformed request is answered, not ignored: silence leaves the host waiting.
                respond(
                    &mut conn,
                    &Response::Error {
                        message: format!("malformed request: {e}"),
                    },
                )?;
                continue;
            }
        };

        let response = handle(&request);
        respond(&mut conn, &response)?;

        // A handshake that names a different protocol version ends the session: continuing
        // would mean interpreting fields under a contract neither side agreed to.
        if let Request::Hello {
            protocol_version, ..
        } = request
        {
            if protocol_version != PROTOCOL_VERSION {
                return Ok(());
            }
        }
    }
}

fn respond<C: Connection>(conn: &mut C, response: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(response)?;
    conn.send(&bytes)
}

/// Execute one request. Pure with respect to I/O so it can be unit-tested on any platform,
/// including the host during development.
pub fn handle(request: &Request) -> Response {
    match request {
        Request::Hello {
            protocol_version,
            client,
        } => {
            if *protocol_version != PROTOCOL_VERSION {
                return Response::Error {
                    message: format!(
                        "protocol mismatch: guest speaks {PROTOCOL_VERSION}, host speaks {protocol_version}"
                    ),
                };
            }
            let _ = client;
            Response::Ready {
                protocol_version: PROTOCOL_VERSION,
                guest: guest_description(),
            }
        }

        Request::Inspect => Response::Ok {
            payload: Payload::Inventory {
                os: guest_description(),
                // Filled in by the platform implementation. Empty is honest: the inventory is
                // not fabricated when the platform layer is absent.
                drives: Vec::new(),
                apps: Vec::new(),
            },
        },

        Request::Exec { .. } => not_yet("exec"),
        Request::Capture { .. } => not_yet("capture"),
        Request::Input { .. } => not_yet("input"),
        Request::Transfer { .. } => not_yet("transfer"),
        Request::Lifecycle { .. } => not_yet("lifecycle"),
    }
}

/// An honest refusal for an operation whose platform layer is not written yet.
fn not_yet(op: &str) -> Response {
    Response::Error {
        message: format!("{op}: not implemented in this build"),
    }
}

/// Describe the guest OS. On Windows this reads the real version; elsewhere it says so plainly
/// rather than pretending.
pub fn guest_description() -> String {
    #[cfg(windows)]
    {
        format!("Windows (wvm-guest {})", env!("CARGO_PKG_VERSION"))
    }
    #[cfg(not(windows))]
    {
        format!(
            "non-Windows host build (wvm-guest {}) — platform layer absent",
            env!("CARGO_PKG_VERSION")
        )
    }
}

/// Validate that a request is well-formed before dispatch. Reserved for checks that are
/// protocol-level (field sanity) rather than policy-level (which belongs on the host).
pub fn validate(request: &Request) -> Result<()> {
    match request {
        Request::Exec { program, .. } if program.trim().is_empty() => {
            bail!("exec: program must not be empty")
        }
        Request::Transfer { guest_path, .. } if guest_path.trim().is_empty() => {
            bail!("transfer: guest_path must not be empty")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wvm_ipc::{InputEvent, LifecycleAction, TransferDirection};

    #[test]
    fn handshake_with_matching_version_is_ready() {
        let resp = handle(&Request::Hello {
            protocol_version: PROTOCOL_VERSION,
            client: "test".into(),
        });
        match resp {
            Response::Ready {
                protocol_version, ..
            } => assert_eq!(protocol_version, PROTOCOL_VERSION),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn handshake_with_wrong_version_is_refused() {
        let resp = handle(&Request::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            client: "test".into(),
        });
        match resp {
            Response::Error { message } => assert!(message.contains("protocol mismatch")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn inspect_returns_an_inventory() {
        match handle(&Request::Inspect) {
            Response::Ok {
                payload: Payload::Inventory { .. },
            } => {}
            other => panic!("expected Inventory, got {other:?}"),
        }
    }

    #[test]
    fn unimplemented_operations_report_honestly() {
        // The critical property: a stub must never look like a success.
        let cases = vec![
            Request::Exec {
                program: "c:\\windows\\system32\\cmd.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: true,
            },
            Request::Capture { monitor: 0 },
            Request::Input {
                event: InputEvent::MouseMove { x: 0, y: 0 },
            },
            Request::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: None,
                guest_path: "x".into(),
            },
            Request::Lifecycle {
                action: LifecycleAction::Suspend,
            },
        ];

        for req in cases {
            match handle(&req) {
                Response::Error { message } => {
                    assert!(
                        message.contains("not implemented"),
                        "unimplemented op must say so, said: {message}"
                    );
                }
                other => panic!("a stub must not report success; got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_empty_program() {
        let req = Request::Exec {
            program: "   ".into(),
            args: vec![],
            cwd: None,
            require_allowlist: false,
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_rejects_empty_guest_path() {
        let req = Request::Transfer {
            direction: TransferDirection::GuestToHost,
            host_path: None,
            guest_path: String::new(),
        };
        assert!(validate(&req).is_err());
    }

    #[test]
    fn validate_accepts_well_formed_requests() {
        assert!(validate(&Request::Inspect).is_ok());
    }
}
