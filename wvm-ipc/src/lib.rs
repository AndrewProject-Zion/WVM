//! Wire protocol shared by the WVM host daemon and the Windows guest service.
//!
//! Both sides depend on this crate, so the contract cannot silently desync: if a variant
//! changes shape, one of the two binaries stops compiling.
//!
//! Design rules held here:
//!
//! * **Closed enums, not strings.** A verb is a variant. A caller cannot invent one, and a
//!   parser cannot accept one.
//! * **Capability at construction time.** [`Request::new`] refuses to build a request the
//!   caller's [`Grant`] does not permit, so there is no code path that has to remember to
//!   check before acting (see `docs/DECISIONS.md` D-003).
//! * **Explicit framing.** Length-prefixed, with a hard maximum. A truncated or oversized
//!   frame is an error, never a panic.

use serde::{Deserialize, Serialize};

pub mod framing;

pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_LEN};

/// Protocol version. Bump on any breaking change to the types below.
pub const PROTOCOL_VERSION: u16 = 1;

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// A verb a caller may be granted.
///
/// This is deliberately narrow and closed. Adding a verb is a protocol change, because it
/// widens what a grant can authorise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    /// Read guest state: OS build, drives, installed applications.
    Inspect,
    /// Launch a process in the guest.
    Exec,
    /// Capture the guest framebuffer.
    Capture,
    /// Inject keyboard and mouse input.
    Input,
    /// Move files between host and guest, within declared roots.
    Transfer,
    /// Change VM lifecycle: start, suspend, snapshot, restore.
    Lifecycle,
}

impl Verb {
    /// Human-readable name, used in journals and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Inspect => "inspect",
            Verb::Exec => "exec",
            Verb::Capture => "capture",
            Verb::Input => "input",
            Verb::Transfer => "transfer",
            Verb::Lifecycle => "lifecycle",
        }
    }
}

/// What an authenticated caller is allowed to do.
///
/// Held by the host, never sent to the guest. The guest executes what it is told because the
/// host has already decided the request was permissible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Caller identity, for the journal.
    pub subject: String,
    /// Permitted verbs.
    pub verbs: Vec<Verb>,
    /// Host paths the caller may read from. Empty means none.
    pub read_roots: Vec<String>,
    /// Host paths the caller may write to. Empty means none. Never the home directory.
    pub write_roots: Vec<String>,
    /// Guest-side root that transfers are confined to.
    pub guest_root: String,
}

impl Grant {
    /// A grant that permits nothing. The safe default: a misconfigured caller can do no harm.
    pub fn deny_all(subject: impl Into<String>) -> Self {
        Grant {
            subject: subject.into(),
            verbs: Vec::new(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            guest_root: String::new(),
        }
    }

    pub fn permits(&self, verb: Verb) -> bool {
        self.verbs.contains(&verb)
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A host → guest request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Protocol and identity handshake. Always the first message on a connection.
    Hello {
        protocol_version: u16,
        /// Opaque client identifier, recorded by the guest for its own log.
        client: String,
    },

    /// Guest inventory. Exercises [`Verb::Inspect`].
    Inspect,

    /// Launch a process. Exercises [`Verb::Exec`].
    Exec {
        /// Absolute path to the executable inside the guest.
        program: String,
        args: Vec<String>,
        /// Working directory, or `None` for the guest's default.
        cwd: Option<String>,
        /// Whether `program` must appear in the host's allowlist before dispatch.
        #[serde(default)]
        require_allowlist: bool,
    },

    /// Capture the guest framebuffer as PNG. Exercises [`Verb::Capture`].
    Capture { monitor: u8 },

    /// Inject input. Exercises [`Verb::Input`].
    Input { event: InputEvent },

    /// Move a file. Exercises [`Verb::Transfer`].
    Transfer {
        direction: TransferDirection,
        /// Path on the host side, or `None` when the host resolves it from its own roots.
        host_path: Option<String>,
        /// Path in the guest, relative to the grant's `guest_root`.
        guest_path: String,
    },

    /// VM lifecycle. Exercises [`Verb::Lifecycle`].
    Lifecycle { action: LifecycleAction },
}

/// Input events, expressed as protocol data rather than synthetic X11/Windows messages so the
/// meaning does not depend on whichever display layer happens to be present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    KeyPress {
        scancode: u16,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    KeyRelease {
        scancode: u16,
        shift: bool,
        ctrl: bool,
        alt: bool,
    },
    /// Type a literal string. The guest translates it to scancodes itself.
    Text {
        value: String,
    },
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    HostToGuest,
    GuestToHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    /// Return resources to the host without losing guest state.
    Suspend,
    Resume,
    Snapshot,
    Restore,
    Shutdown,
}

impl Request {
    /// Construct a request, refusing anything the grant does not permit.
    ///
    /// This is the capability boundary. It lives in the constructor because a boundary that is
    /// a separate check can be dropped by a refactor; a boundary in the type cannot.
    pub fn new(grant: &Grant, kind: RequestKind) -> Result<Request, Denied> {
        let verb = kind.required_verb();
        if !grant.permits(verb) {
            return Err(Denied {
                subject: grant.subject.clone(),
                verb,
                reason: DenialReason::VerbNotGranted,
            });
        }
        Ok(kind.into_request())
    }

    /// The verb this request exercises. Used for journalling and for re-checks on the host.
    pub fn required_verb(&self) -> Verb {
        match self {
            Request::Hello { .. } => Verb::Inspect,
            Request::Inspect => Verb::Inspect,
            Request::Exec { .. } => Verb::Exec,
            Request::Capture { .. } => Verb::Capture,
            Request::Input { .. } => Verb::Input,
            Request::Transfer { .. } => Verb::Transfer,
            Request::Lifecycle { .. } => Verb::Lifecycle,
        }
    }
}

/// Request payloads before capability checking. Keeps the check in one place rather than
/// duplicated across every call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Hello {
        client: String,
    },
    Inspect,
    Exec {
        program: String,
        args: Vec<String>,
        cwd: Option<String>,
        require_allowlist: bool,
    },
    Capture {
        monitor: u8,
    },
    Input {
        event: InputEvent,
    },
    Transfer {
        direction: TransferDirection,
        host_path: Option<String>,
        guest_path: String,
    },
    Lifecycle {
        action: LifecycleAction,
    },
}

impl RequestKind {
    fn required_verb(&self) -> Verb {
        match self {
            RequestKind::Hello { .. } | RequestKind::Inspect => Verb::Inspect,
            RequestKind::Exec { .. } => Verb::Exec,
            RequestKind::Capture { .. } => Verb::Capture,
            RequestKind::Input { .. } => Verb::Input,
            RequestKind::Transfer { .. } => Verb::Transfer,
            RequestKind::Lifecycle { .. } => Verb::Lifecycle,
        }
    }

    fn into_request(self) -> Request {
        match self {
            RequestKind::Hello { client } => Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client,
            },
            RequestKind::Inspect => Request::Inspect,
            RequestKind::Exec {
                program,
                args,
                cwd,
                require_allowlist,
            } => Request::Exec {
                program,
                args,
                cwd,
                require_allowlist,
            },
            RequestKind::Capture { monitor } => Request::Capture { monitor },
            RequestKind::Input { event } => Request::Input { event },
            RequestKind::Transfer {
                direction,
                host_path,
                guest_path,
            } => Request::Transfer {
                direction,
                host_path,
                guest_path,
            },
            RequestKind::Lifecycle { action } => Request::Lifecycle { action },
        }
    }
}

// ---------------------------------------------------------------------------
// Denials
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    /// The verb is not in the grant.
    VerbNotGranted,
    /// The path falls outside the declared roots.
    PathOutsideRoots,
    /// The program is not in the allowlist.
    ProgramNotAllowlisted,
    /// A transfer root was empty, so nothing can be transferred.
    NoTransferRoot,
}

/// A refusal, recorded in the journal with the same weight as a success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denied {
    pub subject: String,
    pub verb: Verb,
    pub reason: DenialReason,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "denied: subject={} verb={} reason={:?}",
            self.subject,
            self.verb.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for Denied {}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A guest → host response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Handshake accepted.
    Ready {
        protocol_version: u16,
        /// Guest OS description, for the journal.
        guest: String,
    },
    /// The request was understood and completed.
    Ok { payload: Payload },
    /// The request was refused, or the operation failed.
    Error { message: String },
}

/// Successful result bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Payload {
    None,
    /// Guest inventory.
    Inventory {
        os: String,
        drives: Vec<Drive>,
        apps: Vec<AppEntry>,
    },
    /// A finished process.
    ProcessExited(i32),
    /// A finished process, with what it printed.
    ///
    /// `ProcessExited` alone cannot describe the thing callers actually want: what a command
    /// produced. Without captured output an `exec` can report that something ran while giving no
    /// way to see what it said, which for a control plane is most of the value.
    ///
    /// `outcome` is a string rather than an enum so this crate stays independent of the guest's
    /// platform types, and so an unknown outcome from a newer guest is reported rather than
    /// rejected by an older host. The host treats anything it does not recognise as a failure it
    /// does not understand, which is the safe reading.
    ProcessOutput {
        /// `exited` or `timed_out`.
        outcome: String,
        /// Exit status when `outcome` is `exited`; -1 when the process ended without one.
        code: i32,
        stdout: String,
        stderr: String,
        elapsed_ms: u64,
    },
    /// A captured frame, base64-encoded PNG. Size is bounded by the frame limit.
    Frame {
        png_base64: String,
        width: u32,
        height: u32,
    },
    /// Bytes moved by a transfer.
    ///
    /// Carries the direction and the destination as well as the count, so a caller can verify which
    /// transfer completed rather than only that one did. "The request succeeded" and "the file I
    /// asked about arrived complete" are different claims, and a control channel that conflates
    /// them is how a truncated file ends up looking like a good one.
    Transferred {
        bytes: u64,
        /// `host_to_guest` or `guest_to_host`, echoing the request.
        direction: String,
        /// Where the bytes were written, as the guest resolved it.
        guest_path: String,
    },
    /// Lifecycle acknowledgement.
    LifecycleDone {
        action: LifecycleAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    pub letter: String,
    pub label: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    pub name: String,
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_grant() -> Grant {
        Grant {
            subject: "test".into(),
            verbs: vec![
                Verb::Inspect,
                Verb::Exec,
                Verb::Capture,
                Verb::Input,
                Verb::Transfer,
                Verb::Lifecycle,
            ],
            read_roots: vec!["/srv/wvm/in".into()],
            write_roots: vec!["/srv/wvm/out".into()],
            guest_root: "C:\\wvm".into(),
        }
    }

    #[test]
    fn deny_all_grant_permits_nothing() {
        let g = Grant::deny_all("nobody");
        for v in [
            Verb::Inspect,
            Verb::Exec,
            Verb::Capture,
            Verb::Input,
            Verb::Transfer,
            Verb::Lifecycle,
        ] {
            assert!(!g.permits(v), "{:?} should not be permitted", v);
        }
    }

    #[test]
    fn request_beyond_grant_is_refused_at_construction() {
        let g = Grant::deny_all("nobody");
        let err = Request::new(
            &g,
            RequestKind::Exec {
                program: "cmd.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: true,
            },
        )
        .expect_err("deny_all must refuse Exec");
        assert_eq!(err.verb, Verb::Exec);
        assert_eq!(err.reason, DenialReason::VerbNotGranted);
    }

    #[test]
    fn request_within_grant_is_built() {
        let g = full_grant();
        let r = Request::new(&g, RequestKind::Inspect).expect("inspect is granted");
        assert_eq!(r.required_verb(), Verb::Inspect);
    }

    #[test]
    fn every_kind_maps_to_a_verb_and_back() {
        // Guards against a new variant being added without a verb mapping.
        let g = full_grant();
        let kinds = vec![
            RequestKind::Inspect,
            RequestKind::Exec {
                program: "a.exe".into(),
                args: vec![],
                cwd: None,
                require_allowlist: false,
            },
            RequestKind::Capture { monitor: 0 },
            RequestKind::Input {
                event: InputEvent::MouseMove { x: 1, y: 2 },
            },
            RequestKind::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: None,
                guest_path: "x".into(),
            },
            RequestKind::Lifecycle {
                action: LifecycleAction::Suspend,
            },
        ];
        for k in kinds {
            let verb = k.required_verb();
            let r = Request::new(&g, k).expect("granted");
            assert_eq!(r.required_verb(), verb);
        }
    }

    #[test]
    fn round_trips_through_json() {
        let g = full_grant();
        let req = Request::new(
            &g,
            RequestKind::Exec {
                program: "C:\\Windows\\System32\\cmd.exe".into(),
                args: vec!["/c".into(), "echo hi".into()],
                cwd: Some("C:\\wvm".into()),
                require_allowlist: true,
            },
        )
        .unwrap();
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn responses_round_trip() {
        let resp = Response::Ok {
            payload: Payload::Inventory {
                os: "Windows 11".into(),
                drives: vec![Drive {
                    letter: "C:".into(),
                    label: "System".into(),
                    free_bytes: 1,
                    total_bytes: 2,
                }],
                apps: vec![AppEntry {
                    name: "notepad".into(),
                    path: "C:\\notepad.exe".into(),
                }],
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn denied_is_an_error_type() {
        let g = Grant::deny_all("nobody");
        let err = Request::new(&g, RequestKind::Capture { monitor: 0 }).unwrap_err();
        // Must be usable as a std error so callers can `?` it.
        let _: &dyn std::error::Error = &err;
        assert!(err.to_string().contains("capture"));
    }
}
