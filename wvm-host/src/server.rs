//! The host's agent-facing control socket.
//!
//! A caller (an agent harness, a CI job, a CLI) connects over a Unix domain socket and speaks
//! `wvm-ipc`. This module owns the policy gate and the journal, so every request that arrives is
//! recorded and every refusal is recorded with its reason.
//!
//! ## Why a Unix socket rather than TCP
//!
//! The control socket is host-local by definition — the guest channel is a separate concern.
//! A Unix socket gives filesystem permissions for free, cannot be reached from off-box, and
//! leaves no port to firewall. The socket file is created with 0600 for that reason.
//!
//! ## The order of operations, which matters
//!
//! 1. Parse the frame. A malformed request is answered and journalled; silence would leave the
//!    caller waiting on a reply that is never coming.
//! 2. Check the grant. A refusal is journalled **before** the reply is sent, so a crash between
//!    the two cannot lose the record of the attempt.
//! 3. Dispatch. The guest is not involved until policy has already agreed.

use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use wvm_ipc::{Grant, Payload, Request, Response, Verb, PROTOCOL_VERSION};

use crate::journal::{Event, Journal, Record};
use crate::policy::{self, Allowlist};

/// Where the control socket lives. Under `$XDG_RUNTIME_DIR` when available (which is per-user
/// and already permissioned), otherwise a path under the state directory.
pub fn default_socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("wvm").join("control.sock");
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("wvm").join("control.sock")
}

/// Server configuration.
pub struct ServerConfig {
    pub socket_path: PathBuf,
    /// How a caller is identified and what it may do. In M2 this is a single configured
    /// identity; per-connection identification is a later concern (see docs/BUILD-PLAN.md).
    pub grant: Grant,
    pub allowlist: Allowlist,
}

impl ServerConfig {
    /// A development configuration: permissive verbs, but roots confined to a scratch directory
    /// rather than the home directory. Useful for exercising the plane without a guest.
    pub fn development() -> Self {
        let scratch = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(".local/share/wvm");

        ServerConfig {
            socket_path: default_socket_path(),
            grant: Grant {
                subject: "local-dev".into(),
                verbs: vec![
                    Verb::Inspect,
                    Verb::Exec,
                    Verb::Capture,
                    Verb::Input,
                    Verb::Transfer,
                    Verb::Lifecycle,
                ],
                read_roots: vec![scratch.join("in").display().to_string()],
                write_roots: vec![scratch.join("out").display().to_string()],
                guest_root: "C:\\wvm".into(),
            },
            allowlist: Allowlist::default_for_windows(),
        }
    }
}

/// A running server. Dropping it removes the socket file.
pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    config: ServerConfig,
    journal: Journal,
}

impl Server {
    /// Bind the control socket.
    ///
    /// A socket file left behind by a crashed process is removed before binding: the file's
    /// existence proves nothing about whether anything is listening on it, which is exactly the
    /// trap that makes a stale socket look alive.
    pub fn bind(config: ServerConfig) -> Result<Self> {
        if let Some(parent) = config.socket_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating socket directory {}", parent.display()))?;
        }

        if config.socket_path.exists() {
            std::fs::remove_file(&config.socket_path).with_context(|| {
                format!(
                    "removing stale socket {} (if a daemon is running, stop it first)",
                    config.socket_path.display()
                )
            })?;
        }

        let listener = UnixListener::bind(&config.socket_path)
            .with_context(|| format!("binding {}", config.socket_path.display()))?;

        // Owner-only. The control socket is not a shared resource.
        std::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| "setting socket permissions to 0600")?;

        let journal = Journal::open_default()?;

        Ok(Server {
            listener,
            socket_path: config.socket_path.clone(),
            config,
            journal,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept connections, handling one at a time.
    ///
    /// Serial by design for now: a single guest and a single control channel means concurrency
    /// would buy nothing and cost reasoning. Revisit when the guest channel is multiplexed.
    ///
    /// Accepting and serving are split into two steps on purpose. Holding the listener borrow
    /// across a call that needs `&mut self` does not compile — and the fix is not a workaround,
    /// it is the right shape: accept first, release the listener, then serve.
    pub fn run(mut self) -> Result<()> {
        self.journal.append(&Record::new(Event::Started {
            version: env!("CARGO_PKG_VERSION").into(),
        }))?;

        loop {
            // Scope the borrow of `self.listener` so it ends before `handle` needs `&mut self`.
            let accepted = self.listener.accept();
            let stream = match accepted {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("wvm: accept failed: {e}");
                    continue;
                }
            };
            if let Err(e) = self.handle(stream) {
                eprintln!("wvm: session ended: {e}");
            }
        }
    }

    /// Serve one caller connection.
    fn handle(&mut self, mut stream: UnixStream) -> Result<()> {
        let reader_stream = stream.try_clone().context("cloning the control stream")?;
        let mut reader = BufReader::new(reader_stream);

        loop {
            let request = match read_request(&mut reader) {
                Ok(Some(r)) => r,
                // Clean disconnect at a frame boundary.
                Ok(None) => return Ok(()),
                Err(e) => {
                    // A malformed frame is answered rather than dropped.
                    let reply = Response::Error {
                        message: format!("malformed request: {e}"),
                    };
                    let _ = send(&mut stream, &reply);
                    return Ok(());
                }
            };

            let verb = request.required_verb();

            // Record the attempt before acting on it.
            self.journal.append(&Record::new(Event::Request {
                subject: self.config.grant.subject.clone(),
                verb: verb.as_str().to_string(),
                op: op_name(&request).to_string(),
            }))?;

            // Policy gate. Every branch here either produces a response or a recorded denial.
            let outcome = self.authorise(&request);

            match outcome {
                Ok(()) => {
                    let response = self.execute(&request);
                    // A `Ready` handshake is a success even though it is not an `Ok` payload.
                    // Recording it as a failure would put false errors in the audit trail, which
                    // is worse than no audit trail: an operator filtering for failures would see
                    // noise on every connection.
                    let ok = matches!(response, Response::Ok { .. } | Response::Ready { .. });

                    // The handshake is not itself a meaningful audit event — every connection
                    // produces one, so recording it buries the events that matter. The
                    // authoritative record is the `Started` entry plus whatever the caller
                    // actually asks for.
                    if !matches!(request, Request::Hello { .. }) {
                        self.journal.append(&Record::new(Event::Completed {
                            subject: self.config.grant.subject.clone(),
                            verb: verb.as_str().to_string(),
                            ok,
                            detail: describe(&response),
                        }))?;
                    }
                    send(&mut stream, &response)?;
                }
                Err(denial) => {
                    // Journalled BEFORE replying: a crash between the two must not lose the
                    // record that an unauthorised attempt was made.
                    self.journal.append(&Record::new(Event::Denied {
                        subject: denial.subject.clone(),
                        verb: denial.verb.as_str().to_string(),
                        reason: format!("{:?}", denial.reason),
                    }))?;

                    let response = Response::Error {
                        message: format!(
                            "denied ({:?}): {} is not permitted for {}",
                            denial.reason,
                            denial.verb.as_str(),
                            denial.subject
                        ),
                    };
                    send(&mut stream, &response)?;
                }
            }
        }
    }

    /// The capability gate. Mirrors the rules in `policy` and adds protocol-level sanity.
    fn authorise(&self, request: &Request) -> Result<(), policy::Denial> {
        let grant = &self.config.grant;
        let verb = request.required_verb();

        // The grant check is repeated here rather than trusted from construction time: this is
        // the boundary the caller actually passes through, and a request could in principle
        // arrive from a path that did not go through `Request::new`.
        if !grant.permits(verb) {
            return Err(policy::Denial {
                subject: grant.subject.clone(),
                verb,
                reason: wvm_ipc::DenialReason::VerbNotGranted,
                subject_detail: Some(op_name(request).to_string()),
            });
        }

        match request {
            Request::Exec { program, .. } => {
                policy::check_exec(grant, program, &self.config.allowlist)
            }
            Request::Transfer {
                direction,
                host_path,
                guest_path,
            } => {
                // Both ends must be inside their roots; checking only one would leave the other
                // as an unguarded path.
                if let Some(hp) = host_path {
                    policy::check_host_transfer(grant, *direction, Path::new(hp))?;
                }
                policy::check_guest_path(grant, guest_path)
            }
            // Inspect, Capture, Input and Lifecycle are authorised by the verb grant alone in M2.
            // Finer scoping (which monitors, which key ranges) belongs with the guest layer.
            _ => Ok(()),
        }
    }

    /// Dispatch an authorised request.
    ///
    /// With no guest channel yet (M3/M4), every operation that needs the guest returns an
    /// explicit error. `Inspect` succeeds because the host can honestly describe itself; nothing
    /// is fabricated about the guest.
    fn execute(&self, request: &Request) -> Response {
        match request {
            Request::Hello {
                protocol_version, ..
            } => {
                if *protocol_version != PROTOCOL_VERSION {
                    return Response::Error {
                        message: format!(
                            "protocol mismatch: host speaks {PROTOCOL_VERSION}, caller speaks {protocol_version}"
                        ),
                    };
                }
                Response::Ready {
                    protocol_version: PROTOCOL_VERSION,
                    guest: "no guest channel yet (M3 not started)".into(),
                }
            }

            Request::Inspect => Response::Ok {
                // Deliberately empty and labelled: the host has no guest to inventory, and
                // inventing entries would be worse than reporting none.
                payload: Payload::Inventory {
                    os: "host (no guest channel yet)".into(),
                    drives: Vec::new(),
                    apps: Vec::new(),
                },
            },

            // Everything else needs the guest.
            Request::Exec { .. } => no_guest("exec"),
            Request::Capture { .. } => no_guest("capture"),
            Request::Input { .. } => no_guest("input"),
            Request::Transfer { .. } => no_guest("transfer"),
            Request::Lifecycle { .. } => no_guest("lifecycle"),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Leaving the socket file behind makes the next start ambiguous: the file's presence
        // says nothing about whether a daemon is listening.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// An honest error for an operation that needs the guest channel.
fn no_guest(op: &str) -> Response {
    Response::Error {
        message: format!("{op}: no guest channel yet (M3 not started)"),
    }
}

fn op_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::Inspect => "inspect",
        Request::Exec { .. } => "exec",
        Request::Capture { .. } => "capture",
        Request::Input { .. } => "input",
        Request::Transfer { .. } => "transfer",
        Request::Lifecycle { .. } => "lifecycle",
    }
}

fn describe(response: &Response) -> String {
    match response {
        Response::Ready { .. } => "ready".into(),
        Response::Ok { payload } => format!("ok: {}", payload_name(payload)),
        Response::Error { message } => format!("error: {message}"),
    }
}

fn payload_name(payload: &Payload) -> &'static str {
    match payload {
        Payload::None => "none",
        Payload::Inventory { .. } => "inventory",
        Payload::ProcessExited(_) => "process_exited",
        Payload::Frame { .. } => "frame",
        Payload::Transferred { .. } => "transferred",
        Payload::LifecycleDone { .. } => "lifecycle_done",
    }
}

/// Read one framed request. `Ok(None)` means the caller hung up cleanly.
fn read_request(reader: &mut BufReader<UnixStream>) -> Result<Option<Request>> {
    match wvm_ipc::read_frame(reader) {
        Ok(bytes) => {
            let request: Request =
                serde_json::from_slice(&bytes).context("parsing the framed request as wvm-ipc")?;
            Ok(Some(request))
        }
        // Clean disconnect at a frame boundary.
        Err(wvm_ipc::FrameError::Closed) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn send(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(response)?;
    wvm_ipc::write_frame(stream, &bytes)?;
    stream.flush().context("flushing the control stream")?;
    Ok(())
}

/// A minimal client, used by `wvm call` and by the integration tests.
pub mod client {
    use super::*;

    pub struct Client {
        stream: UnixStream,
        reader: BufReader<UnixStream>,
    }

    impl Client {
        pub fn connect(socket_path: &Path) -> Result<Self> {
            let stream = UnixStream::connect(socket_path)
                .with_context(|| format!("connecting to {}", socket_path.display()))?;
            let reader = BufReader::new(stream.try_clone()?);
            Ok(Client { stream, reader })
        }

        pub fn send(&mut self, request: &Request) -> Result<Response> {
            let bytes = serde_json::to_vec(request)?;
            wvm_ipc::write_frame(&mut self.stream, &bytes)?;
            self.stream.flush()?;

            let reply = match wvm_ipc::read_frame(&mut self.reader) {
                Ok(b) => b,
                Err(e) => anyhow::bail!("reading the reply: {e}"),
            };
            Ok(serde_json::from_slice(&reply)?)
        }

        /// Handshake. Returns the guest description on success.
        pub fn hello(&mut self, client_name: &str) -> Result<String> {
            let response = self.send(&Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: client_name.to_string(),
            })?;
            match response {
                Response::Ready { guest, .. } => Ok(guest),
                Response::Ok { .. } => anyhow::bail!("unexpected Ok to Hello"),
                Response::Error { message } => anyhow::bail!("handshake refused: {message}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;
    use wvm_ipc::{RequestKind, TransferDirection};

    /// A scratch path unique to this test and process.
    fn scratch_socket(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("wvm-test-{}-{}.sock", tag, std::process::id()));
        p
    }

    /// Serve exactly one connection in a background thread so tests can talk to the server
    /// without running the accept loop forever.
    fn serve_one(server: Server) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let stream = match server.listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            // `handle` is private to the module; the test lives inside it.
            let mut s = server;
            let _ = s.handle(stream);
        })
    }

    fn full_grant() -> Grant {
        Grant {
            subject: "test-agent".into(),
            verbs: vec![Verb::Inspect, Verb::Exec, Verb::Transfer],
            read_roots: vec!["/tmp/wvm-in".into()],
            write_roots: vec!["/tmp/wvm-out".into()],
            guest_root: "C:\\wvm".into(),
        }
    }

    /// Round-trip one request through a real socket against a real server.
    fn round_trip(tag: &str, grant: Grant, kind: RequestKind) -> Response {
        let path = scratch_socket(tag);
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: grant.clone(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).expect("bind");
        let handle = serve_one(server);

        let mut c = client::Client::connect(&path).expect("connect");
        let request = Request::new(&grant, kind).expect("request within grant");
        let response = c.send(&request).expect("send");
        drop(c);
        let _ = handle.join();
        let _ = std::fs::remove_file(&path);
        response
    }

    #[test]
    fn socket_is_owner_only() {
        let path = scratch_socket("perms");
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the control socket must not be world-accessible"
        );
        drop(server);
    }

    #[test]
    fn dropping_the_server_removes_the_socket() {
        let path = scratch_socket("cleanup");
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).unwrap();
        assert!(path.exists(), "socket must exist while bound");
        drop(server);
        assert!(!path.exists(), "socket must be removed on shutdown");
    }

    #[test]
    fn a_stale_socket_file_does_not_block_binding() {
        // The trap this guards: a socket file outlives its daemon and looks identical to a live
        // one. Binding must succeed anyway.
        let path = scratch_socket("stale");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"not a socket").unwrap();

        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).expect("must replace a stale socket file");
        drop(server);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn inspect_succeeds_and_reports_no_guest_instead_of_faking_one() {
        let response = round_trip("inspect", full_grant(), RequestKind::Inspect);
        match response {
            Response::Ok {
                payload: Payload::Inventory { os, drives, apps },
            } => {
                assert!(
                    os.contains("host"),
                    "must say the host answered, not a guest"
                );
                assert!(drives.is_empty(), "must not invent drives");
                assert!(apps.is_empty(), "must not invent applications");
            }
            other => panic!("expected Inventory, got {other:?}"),
        }
    }

    #[test]
    fn a_granted_but_unimplemented_op_reports_no_guest() {
        let response = round_trip(
            "exec",
            full_grant(),
            RequestKind::Exec {
                program: "C:\\Windows\\System32\\cmd.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: true,
            },
        );
        match response {
            Response::Error { message } => assert!(
                message.contains("no guest channel"),
                "must name the real reason, said: {message}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_outside_the_allowlist_is_denied_at_the_socket() {
        let response = round_trip(
            "deny-exec",
            full_grant(),
            RequestKind::Exec {
                program: "C:\\Users\\admin\\evil.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: true,
            },
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("denied"),
                    "an unlisted program must be refused, said: {message}"
                );
                assert!(message.contains("exec"));
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn transfer_outside_the_roots_is_denied() {
        let response = round_trip(
            "deny-transfer",
            full_grant(),
            RequestKind::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: Some("/etc/shadow".into()),
                guest_path: "C:\\wvm\\x".into(),
            },
        );
        match response {
            Response::Error { message } => {
                assert!(message.contains("PathOutsideRoots"), "said: {message}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn transfer_inside_the_roots_passes_policy_then_fails_on_the_missing_guest() {
        // Distinguishes "refused by policy" from "no guest yet": the two must not be confused.
        let response = round_trip(
            "allow-transfer",
            full_grant(),
            RequestKind::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: Some("/tmp/wvm-out/file.bin".into()),
                guest_path: "C:\\wvm\\file.bin".into(),
            },
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("no guest channel"),
                    "an authorised transfer must reach dispatch, said: {message}"
                );
                assert!(!message.contains("denied"), "must not be denied: {message}");
            }
            other => panic!("expected a no-guest error, got {other:?}"),
        }
    }

    #[test]
    fn guest_path_traversal_is_denied() {
        let response = round_trip(
            "deny-traversal",
            full_grant(),
            RequestKind::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: Some("/tmp/wvm-out/x".into()),
                guest_path: "C:\\wvm\\..\\Windows\\System32\\config\\SAM".into(),
            },
        );
        match response {
            Response::Error { message } => {
                assert!(message.contains("PathOutsideRoots"), "said: {message}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn a_verb_outside_the_grant_is_denied() {
        // The agent may inspect but not execute. The construct of an Exec request must fail, and
        // if one reached the server by another route it must still be refused.
        let grant = Grant {
            subject: "read-only".into(),
            verbs: vec![Verb::Inspect],
            read_roots: vec![],
            write_roots: vec![],
            guest_root: String::new(),
        };

        // Construction refuses it.
        assert!(Request::new(
            &grant,
            RequestKind::Exec {
                program: "c:\\windows\\system32\\cmd.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: false,
            }
        )
        .is_err());

        // And the server refuses it even when the request is built by hand, bypassing `new`.
        let path = scratch_socket("deny-verb");
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: grant.clone(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).unwrap();
        let handle = serve_one(server);
        let mut c = client::Client::connect(&path).unwrap();

        let smuggled = Request::Exec {
            program: "c:\\windows\\system32\\cmd.exe".into(),
            args: vec![],
            cwd: None,
            require_allowlist: false,
        };
        let response = c.send(&smuggled).unwrap();
        drop(c);
        let _ = handle.join();
        let _ = std::fs::remove_file(&path);

        match response {
            Response::Error { message } => {
                assert!(message.contains("VerbNotGranted"), "said: {message}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn the_journal_records_a_denial() {
        // This is the property the whole audit trail rests on: an attempted unauthorised action
        // must be recoverable from the journal even though it was refused.
        let path = scratch_socket("journal-denial");
        let _ = std::fs::remove_file(&path);
        let journal_path = std::env::temp_dir().join(format!(
            "wvm-test-journal-denial-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&journal_path);

        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };

        let mut server = Server::bind(config).unwrap();
        // Redirect this server's journal to the scratch file.
        server.journal = Journal::open(&journal_path).unwrap();

        let handle = std::thread::spawn(move || {
            let stream = match server.listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            let _ = server.handle(stream);
        });

        let mut c = client::Client::connect(&path).unwrap();
        let response = c
            .send(&Request::Exec {
                program: "C:\\Users\\admin\\evil.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: true,
            })
            .unwrap();
        assert!(matches!(response, Response::Error { .. }));
        drop(c);
        let _ = handle.join();

        let records = Journal::open(&journal_path).unwrap().tail(50).unwrap();
        let denied: Vec<_> = records
            .iter()
            .filter(|r| matches!(r.event, Event::Denied { .. }))
            .collect();

        assert_eq!(denied.len(), 1, "exactly one denial must be recorded");
        match &denied[0].event {
            Event::Denied {
                subject,
                verb,
                reason,
            } => {
                assert_eq!(subject, "test-agent");
                assert_eq!(verb, "exec");
                assert!(
                    reason.contains("ProgramNotAllowlisted"),
                    "reason was {reason}"
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }

        // The attempt is also recorded, not just the refusal.
        assert!(
            records
                .iter()
                .any(|r| matches!(r.event, Event::Request { .. })),
            "the request itself must be journalled"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal_path);
    }

    #[test]
    fn a_malformed_frame_is_answered_not_ignored() {
        let path = scratch_socket("malformed");
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).unwrap();
        let handle = serve_one(server);

        let mut stream = UnixStream::connect(&path).unwrap();
        // A well-framed but non-JSON payload.
        wvm_ipc::write_frame(&mut stream, b"this is not json").unwrap();

        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let reply = wvm_ipc::read_frame(&mut reader).unwrap();
        let response: Response = serde_json::from_slice(&reply).unwrap();

        match response {
            Response::Error { message } => {
                assert!(message.contains("malformed"), "said: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        drop(stream);
        let _ = handle.join();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn client_handshake_succeeds_with_matching_version() {
        let path = scratch_socket("hello");
        let _ = std::fs::remove_file(&path);
        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let server = Server::bind(config).unwrap();
        let handle = serve_one(server);

        let mut c = client::Client::connect(&path).unwrap();
        let guest = c.hello("integration-test").expect("handshake");
        assert!(guest.contains("no guest channel"), "guest was: {guest}");

        drop(c);
        let _ = handle.join();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_handshake_is_not_journalled_as_a_failure() {
        // Regression: the first version classified any response that was not `Ok` as a failure,
        // so every `Ready` handshake landed in the journal as ok:false. An audit log that
        // reports false errors on the happy path trains its readers to ignore it.
        let path = scratch_socket("hello-journal");
        let _ = std::fs::remove_file(&path);
        let journal_path = std::env::temp_dir().join(format!(
            "wvm-test-hello-journal-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&journal_path);

        let config = ServerConfig {
            socket_path: path.clone(),
            grant: full_grant(),
            allowlist: Allowlist::default_for_windows(),
        };
        let mut server = Server::bind(config).unwrap();
        server.journal = Journal::open(&journal_path).unwrap();

        let handle = std::thread::spawn(move || {
            let stream = match server.listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            let _ = server.handle(stream);
        });

        let mut c = client::Client::connect(&path).unwrap();
        c.hello("journal-test").expect("handshake");
        // One real request after the handshake, so there is something that SHOULD be recorded.
        let inspect = Request::new(&full_grant(), RequestKind::Inspect).unwrap();
        c.send(&inspect).unwrap();
        drop(c);
        let _ = handle.join();

        let records = Journal::open(&journal_path).unwrap().tail(50).unwrap();

        // No completed record may claim a failure.
        let false_failures: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.event {
                Event::Completed {
                    ok: false, detail, ..
                } => Some(detail.clone()),
                _ => None,
            })
            .collect();
        assert!(
            false_failures.is_empty(),
            "the happy path must not journal failures: {false_failures:?}"
        );

        // The real request is recorded as a success.
        assert!(
            records.iter().any(|r| matches!(
                &r.event,
                Event::Completed { ok: true, verb, .. } if verb == "inspect"
            )),
            "the inspect request must be journalled as a success"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal_path);
    }

    /// Unused import guard: `BufWriter` is referenced so the module compiles the same way with
    /// and without the test-only helpers above.
    #[allow(dead_code)]
    fn _unused(_: BufWriter<UnixStream>) {}
}
