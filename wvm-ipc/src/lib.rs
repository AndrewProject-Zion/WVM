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

pub use framing::{read_frame, set_frame_debug, write_frame, FrameError, MAX_FRAME_LEN};

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
    /// Put the VM's desktop on screen on demand, and take it away again.
    Display,
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
            Verb::Display => "display",
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
        /// How long the process may run before it is killed, in milliseconds.
        ///
        /// `None` means the guest's default, which is **ten minutes**. That default is a real
        /// problem for a caller that does not think about it, and it was found the hard way: the
        /// guest answers ONE request at a time, so a single command that hangs holds the entire
        /// control channel for ten minutes. The host's own read timeout fires first, and the
        /// symptom is "the guest accepted the connection but did not answer" — which reads like a
        /// dead service, not like a slow command.
        ///
        /// So the caller states what it is willing to wait for. A short timeout turns a hung command
        /// into a `timed_out` reply the host can act on, instead of an unexplained silence.
        #[serde(default)]
        timeout_ms: Option<u64>,
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
        /// Whether an existing file at the destination may be replaced.
        ///
        /// Defaults, at the caller, to false. A transfer that silently overwrites is a data-loss bug
        /// waiting for a caller that retried — and the refusal must happen BEFORE the destination is
        /// opened, or a rejected transfer leaves a truncated file behind.
        #[serde(default)]
        overwrite: bool,
    },

    /// One chunk of a transfer's bytes, plus the framing needed to reassemble them.
    ///
    /// # Why this is a separate verb from `Transfer`
    ///
    /// `Transfer` names WHAT is being moved. This carries the bytes. Keeping them apart means the
    /// guest can validate and open the destination once, on `Transfer`, and then write chunk after
    /// chunk without re-resolving a path it has already checked.
    ///
    /// # Why the offset is sent rather than assumed
    ///
    /// The obvious design is to treat chunks as a stream and append in arrival order, with the host
    /// trusting the transport to preserve order. TCP does preserve order — within one connection.
    /// But a transfer that spans a reconnect, or a host that retries a chunk whose acknowledgement
    /// it did not see, would silently corrupt the file with the append-only shape, and the
    /// corruption would be undetectable until the hash was compared.
    ///
    /// Sending the offset makes every chunk self-describing: the guest can refuse a chunk that does
    /// not land where it expected, which turns a silent corruption into a loud error.
    TransferChunk {
        /// Byte offset this chunk begins at.
        offset: u64,
        /// The chunk's bytes, base64-encoded because JSON strings cannot carry arbitrary binary.
        ///
        /// The cost is a 33% larger payload. At 256 KiB per chunk that is about 341 KiB on the wire,
        /// measured working as a single frame (`wvm-ipc`'s `a_transfer_sized_frame_round_trips`).
        /// The alternative — multiplexed binary frames — would need a length-prefix-plus-type-header
        /// parser in Rust twice and in the Python client once, and three implementations of a
        /// framing rule is where desync bugs live.
        data_base64: String,
        /// True on the last chunk, so the guest knows to check the total and close the file.
        ///
        /// The length is not sent in advance deliberately: a sender that declares a size and then
        /// dies leaves the receiver unable to distinguish "complete" from "truncated" without a
        /// timeout. An explicit end marker cannot be ambiguous.
        eof: bool,
    },

    /// Request one chunk of a file the guest is serving.
    ///
    /// The mirror of `TransferChunk`, and deliberately a separate verb rather than a direction flag
    /// on it. The two carry different things: a push chunk carries DATA, and this carries a REQUEST
    /// for data. Overloading one verb would mean a variant whose payload fields are half-unused
    /// depending on direction, which is how a reader ends up misreading which side is which.
    ///
    /// `offset` and `length` are named rather than implied. A "send me the next chunk" protocol
    /// would need the guest to remember how far it had served, and a retry after a lost reply would
    /// then serve the wrong bytes. A self-describing request makes a replay harmless.
    PullChunk {
        /// Byte offset to read from.
        offset: u64,
        /// Maximum bytes wanted. The guest returns fewer at the end of the file, never more.
        length: u64,
    },

    /// VM lifecycle. Exercises [`Verb::Lifecycle`].
    Lifecycle { action: LifecycleAction },

    /// Put the desktop on screen, or take it away. Exercises [`Verb::Display`].
    Display { action: DisplayAction },
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

/// What to do with the VM's display. Host-side only: the guest cannot see its own hypervisor, so
/// the guest answers this with a refusal that names the command that works (see dispatch.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayAction {
    /// Attach a viewer window to the running VM.
    Show,
    /// Detach the viewer, returning to pure headless operation.
    Hide,
    /// Report whether a viewer is attached. Never an error when one is not: "nobody is watching" is
    /// the normal state and the one this verb is called in most.
    Status,
}

impl std::fmt::Display for DisplayAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The wire spelling, matching the serde rename, so a refusal and the request that caused it
        // use the same word.
        match self {
            DisplayAction::Show => write!(f, "show"),
            DisplayAction::Hide => write!(f, "hide"),
            DisplayAction::Status => write!(f, "status"),
        }
    }
}

impl std::fmt::Display for LifecycleAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The wire spelling, matching the serde rename, so an error message and the request that
        // caused it use the same word. A caller comparing them should not have to translate.
        match self {
            LifecycleAction::Suspend => write!(f, "suspend"),
            LifecycleAction::Resume => write!(f, "resume"),
            LifecycleAction::Snapshot => write!(f, "snapshot"),
            LifecycleAction::Restore => write!(f, "restore"),
            LifecycleAction::Shutdown => write!(f, "shutdown"),
        }
    }
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
            Request::Transfer { .. }
            | Request::TransferChunk { .. }
            | Request::PullChunk { .. } => Verb::Transfer,
            Request::Lifecycle { .. } => Verb::Lifecycle,
            Request::Display { .. } => Verb::Display,
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
        /// How long the process may run, or `None` for the guest's default.
        timeout_ms: Option<u64>,
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
        /// Whether an existing destination may be replaced.
        overwrite: bool,
    },
    Lifecycle {
        action: LifecycleAction,
    },
    Display {
        action: DisplayAction,
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
            RequestKind::Display { .. } => Verb::Display,
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
                timeout_ms,
            } => Request::Exec {
                program,
                args,
                cwd,
                require_allowlist,
                timeout_ms,
            },
            RequestKind::Capture { monitor } => Request::Capture { monitor },
            RequestKind::Input { event } => Request::Input { event },
            RequestKind::Transfer {
                direction,
                host_path,
                guest_path,
                overwrite,
            } => Request::Transfer {
                direction,
                host_path,
                guest_path,
                overwrite,
            },
            RequestKind::Lifecycle { action } => Request::Lifecycle { action },
            RequestKind::Display { action } => Request::Display { action },
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
    /// Acknowledgement of one transfer chunk.
    ///
    /// This is what closes the lockstep loop: the host will not send the next chunk until it sees
    /// this, so the transfer advances at the speed of the guest's disk rather than the host's.
    ///
    /// It carries the running total so the host can detect a lost or short write on the chunk it
    /// just sent, instead of discovering it from a hash mismatch at the end of a 10 GB file.
    ChunkWritten {
        /// Echoed, so a reply can be matched to the chunk that caused it.
        offset: u64,
        /// Bytes in this chunk.
        bytes: u64,
        /// Total bytes written to the destination so far.
        total: u64,
        /// Echoed end-of-file flag.
        eof: bool,
    },
    /// One chunk of a file the guest served, plus enough to reconstruct the whole.
    ///
    /// Carries the running `total` so the receiver can confirm it got what the file actually is,
    /// rather than discovering a truncated read only from a hash mismatch at the end.
    ChunkRead {
        /// The offset this chunk begins at, echoed so a reply can be matched to its request.
        offset: u64,
        /// The chunk's bytes, base64-encoded — JSON strings cannot carry arbitrary binary.
        data_base64: String,
        /// True when this chunk reaches the end of the file.
        eof: bool,
        /// Total size of the file being served, so the caller can check its own accounting.
        total: u64,
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

/// Internal helper so the framing tests can build a realistic base64 body without pulling a
/// dependency into this crate.
#[doc(hidden)]
pub fn base64_encode_for_test(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Standard base64 decoding, for payloads that arrive from a peer.
///
/// Strict about the alphabet and the padding, deliberately. Input arrives from the network, and a
/// lenient decoder is how malformed data becomes silently wrong data: a byte dropped by a lenient
/// skip would shift everything after it and the file would still look plausibly sized.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    fn value(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(format!("length {} is not a multiple of 4", bytes.len()));
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, group) in bytes.chunks(4).enumerate() {
        let mut vals = [0u8; 4];
        let mut pad = 0;
        for (j, &c) in group.iter().enumerate() {
            if c == b'=' {
                // Padding is only legal in the last group, and only in the last two positions.
                if i != bytes.len() / 4 - 1 || j < 2 {
                    return Err("padding in an invalid position".into());
                }
                pad += 1;
                vals[j] = 0;
            } else {
                // A non-padding character AFTER padding is malformed.
                if pad > 0 {
                    return Err("data after padding".into());
                }
                vals[j] = value(c).ok_or_else(|| format!("invalid base64 character {c:?}"))?;
            }
        }

        let n = (u32::from(vals[0]) << 18)
            | (u32::from(vals[1]) << 12)
            | (u32::from(vals[2]) << 6)
            | u32::from(vals[3]);

        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }

    Ok(out)
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
                Verb::Display,
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
                timeout_ms: None,
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
                timeout_ms: None,
            },
            RequestKind::Capture { monitor: 0 },
            RequestKind::Input {
                event: InputEvent::MouseMove { x: 1, y: 2 },
            },
            RequestKind::Transfer {
                direction: TransferDirection::HostToGuest,
                host_path: None,
                guest_path: "x".into(),
                overwrite: false,
            },
            RequestKind::Lifecycle {
                action: LifecycleAction::Suspend,
            },
            RequestKind::Display {
                action: DisplayAction::Show,
            },
        ];
        // THIS LIST IS MAINTAINED BY HAND, so it is only as good as the person editing it. The
        // canary below deliberately encodes the number of Verb variants: adding a verb without
        // adding a kind here turns this test red, which is the only reason a missing entry gets
        // noticed. It has already been the case once that a variant was added and this list was
        // not, and the test passed while covering nothing.
        assert_eq!(
            kinds.len(),
            7,
            "one kind per Verb variant (inspect, exec, capture, input, transfer, lifecycle, \
display) — if you added a verb, add its kind to this list and bump this number"
        );
        for k in kinds {
            let verb = k.required_verb();
            let r = Request::new(&g, k).expect("granted");
            assert_eq!(r.required_verb(), verb);
        }
    }

    #[test]
    fn a_display_request_says_what_it_is_on_the_wire() {
        // Both halves matter and neither is visible from the type alone:
        //  - the wire spelling is what a caller types and what a refusal quotes back, so "show"
        //    has to survive serialisation unchanged;
        //  - the verb mapping is what the capability gate authorises against, so a Display request
        //    that mapped to, say, Lifecycle would be granted by the wrong grant.
        assert_eq!(DisplayAction::Show.to_string(), "show");
        assert_eq!(DisplayAction::Hide.to_string(), "hide");
        assert_eq!(DisplayAction::Status.to_string(), "status");

        let kind = RequestKind::Display {
            action: DisplayAction::Show,
        };
        assert_eq!(kind.required_verb(), Verb::Display);

        let encoded = serde_json::to_string(&kind.into_request()).unwrap();
        assert!(
            encoded.contains("\"action\":\"show\""),
            "wire spelling drifted: {encoded}"
        );
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.required_verb(), Verb::Display);
        assert!(matches!(
            decoded,
            Request::Display {
                action: DisplayAction::Show
            }
        ));
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
                timeout_ms: None,
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
