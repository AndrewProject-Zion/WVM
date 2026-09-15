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

        Request::Exec {
            program,
            args,
            cwd,
            require_allowlist: _,
        } => {
            // `require_allowlist` is carried to the host, not acted on here. The guest does not
            // check authority — see the module comment: a second, weaker check creates a place
            // for the two to disagree.
            match crate::win32::run(
                program,
                args,
                cwd.as_deref(),
                crate::win32::DEFAULT_TIMEOUT_MS,
            ) {
                Ok(execution) => {
                    let (outcome, code) = match execution.outcome {
                        crate::win32::Outcome::Exited { code } => ("exited".to_string(), code),
                        crate::win32::Outcome::TimedOut { .. } => ("timed_out".to_string(), -1),
                    };
                    Response::Ok {
                        payload: Payload::ProcessOutput {
                            outcome,
                            code,
                            stdout: execution.stdout,
                            stderr: execution.stderr,
                            elapsed_ms: execution.elapsed_ms,
                        },
                    }
                }
                // A failure to launch is an Error, not an Ok with a non-zero code: "there is no
                // such program" and "the program returned 1" are different answers.
                Err(e) => Response::Error {
                    message: format!("exec: {e}"),
                },
            }
        }

        Request::Capture { monitor } => {
            // The guest captures its own screen rather than the host reaching into the
            // framebuffer over QMP. Both work; the guest route goes through the grant check and
            // the journal, which is the whole point of this design. See capture.rs.
            match crate::capture::capture(*monitor) {
                Ok(frame) => Response::Ok {
                    payload: Payload::Frame {
                        png_base64: crate::base64::encode(&frame.png),
                        width: frame.width,
                        height: frame.height,
                    },
                },
                // A headless guest genuinely cannot be screenshotted, and saying so is the point:
                // a black image would be indistinguishable from a dark screen.
                Err(e) => Response::Error {
                    message: format!("capture: {e}"),
                },
            }
        }

        Request::Input { .. } => not_yet("input"),

        Request::Transfer {
            direction,
            host_path,
            guest_path,
        } => {
            // The guest-side transfer is real, and it is the side that enforces where bytes land.
            //
            // This is deliberate: the path containment rules live here, so routing a transfer
            // through the guest means the guest — not the caller — decides what it is willing to
            // write and where. A host-side copy would be faster and would bypass exactly the
            // boundary that exists to stop a transfer writing outside its staging root.
            //
            // See fsio.rs for why this is chunked rather than one frame per file.
            let plan = match crate::fsio::plan_from(
                direction,
                &crate::fsio::roots(),
                guest_path,
                host_path.as_deref().unwrap_or(""),
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Response::Error {
                        message: format!("transfer: {e}"),
                    };
                }
            };

            #[cfg(windows)]
            {
                let io = crate::fsio::FilesystemIo::new();
                match crate::transfer::execute(&io, &plan) {
                    Ok(bytes) => Response::Ok {
                        payload: Payload::Transferred {
                            bytes,
                            direction: plan.direction.to_string(),
                            guest_path: plan.guest_path.clone(),
                        },
                    },
                    Err(e) => Response::Error {
                        message: format!("transfer: {e}"),
                    },
                }
            }

            #[cfg(not(windows))]
            {
                let _ = plan;
                Response::Error {
                    message: "transfer: the filesystem layer is only present in a Windows build \
                              of wvm-guest; this binary was built for the host, where it exists \
                              to be type-checked and tested rather than run"
                        .to_string(),
                }
            }
        }

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
    fn transfer_refuses_a_path_outside_the_staging_root() {
        // The property that matters most about transfer: the guest decides where bytes may land,
        // and refuses everything else. A transfer to an arbitrary path would be the host-`/`-as-`Z:`
        // mistake this project exists to avoid.
        //
        // Traversal is the specific case, because it is the one a naive `starts_with` check passes.
        let roots = crate::fsio::Roots::default_staging();

        for escaped in [
            "../../Windows/System32/drivers/etc/hosts",
            r"C:\Windows\System32\config\SAM",
            r"..\..\..\Windows\win.ini",
        ] {
            let planned = crate::fsio::plan_from(
                &TransferDirection::HostToGuest,
                &roots,
                escaped,
                "C:/staging/source.bin",
            );
            assert!(
                planned.is_err(),
                "'{escaped}' must be refused, not resolved into the staging root"
            );
        }

        // And a path INSIDE the root is accepted, so the check is not simply refusing everything —
        // a boundary that refuses all input is not a boundary, it is a wall, and it would pass the
        // loop above while making transfer useless.
        let ok = crate::fsio::plan_from(
            &TransferDirection::HostToGuest,
            &roots,
            r"C:\ProgramData\wvm\staging\payload.bin",
            r"C:\ProgramData\wvm\staging-host\payload.bin",
        );
        assert!(
            ok.is_ok(),
            "a path inside the staging root must be allowed: {ok:?}"
        );
    }

    #[test]
    fn transfer_on_a_host_build_refuses_rather_than_pretending() {
        // The filesystem layer is Windows-only. On a host build the honest answer is that the
        // platform half is absent — not an Ok with a fabricated byte count, and not the
        // "not implemented" stub message, which would imply the verb is unwritten.
        //
        // Both paths must be INSIDE the staging roots here, or the containment check refuses the
        // request first and this test never reaches the platform branch it is about. (The first
        // version of this test used an arbitrary host path and asserted the wrong thing failed —
        // which is itself evidence the containment check runs before anything else.)
        let roots = crate::fsio::Roots::default_staging();

        let response = handle(&Request::Transfer {
            direction: TransferDirection::HostToGuest,
            host_path: Some(format!(r"{}\file.bin", roots.host)),
            guest_path: format!(r"{}\file.bin", roots.guest),
        });

        match response {
            Response::Error { message } => {
                if cfg!(windows) {
                    // On Windows the transfer may genuinely succeed or fail on the filesystem; this
                    // test is about the host build's honesty, so assert only that we did not panic.
                    return;
                }
                assert!(
                    message.contains("Windows build"),
                    "a host build must name the missing platform, said: {message}"
                );
                assert!(
                    !message.contains("not implemented"),
                    "transfer is implemented; the message must not claim otherwise"
                );
            }
            other => panic!("a host build must refuse, got {other:?}"),
        }
    }

    #[test]
    fn unimplemented_operations_report_honestly() {
        // The critical property: a stub must never look like a success.
        //
        // `exec`, `capture` and `transfer` are no longer here — all three have real
        // implementations. On a host build their platform halves cannot run, and each reports that
        // explicitly rather than claiming to be unwritten; see the tests below.
        let cases = vec![
            Request::Input {
                event: InputEvent::MouseMove { x: 0, y: 0 },
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
    fn exec_on_a_host_build_refuses_rather_than_pretending() {
        // `exec` has a real implementation, but its execution half exists only in a Windows
        // build. On the host it must say so — an error naming the platform, never an `Ok` with a
        // fabricated exit code, and never a `not implemented` message that would suggest the
        // feature is unwritten when it is merely unbuildable here.
        let req = Request::Exec {
            program: "c:/windows/system32/cmd.exe".into(),
            args: vec!["/c".into(), "echo".into(), "hi".into()],
            cwd: None,
            require_allowlist: true,
        };

        match handle(&req) {
            Response::Error { message } => {
                if cfg!(windows) {
                    // In a real guest this should have run; reaching an error here means something
                    // is wrong with the command, not with the platform.
                    panic!("exec failed inside a Windows guest: {message}");
                }
                assert!(
                    message.contains("Win32 execution layer"),
                    "the refusal must name what is missing: {message}"
                );
                assert!(
                    !message.contains("not implemented"),
                    "exec is implemented; only its platform half is absent here: {message}"
                );
            }
            other => panic!("a platform-absent exec must not report success; got {other:?}"),
        }
    }

    #[test]
    fn capture_on_a_host_build_refuses_rather_than_returning_a_blank_image() {
        // A capture stub that returned a fabricated black PNG would be far worse than one that
        // refuses: it would look like a successful screenshot of a dark screen, and there is no
        // way to tell those apart from the caller's side.
        match handle(&Request::Capture { monitor: 0 }) {
            Response::Error { message } => {
                if cfg!(windows) {
                    panic!("capture failed inside a Windows guest: {message}");
                }
                assert!(
                    message.contains("Windows build"),
                    "the refusal must name the platform: {message}"
                );
                assert!(
                    !message.contains("not implemented"),
                    "capture is implemented; only its platform half is absent here: {message}"
                );
            }
            other => panic!("a platform-absent capture must not report success; got {other:?}"),
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
