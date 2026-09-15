//! File transfer, and the containment rules that go with it.
//!
//! Two halves, deliberately separated:
//!
//! * **Planning** — decide whether a transfer is permissible and where it lands. Pure, no I/O,
//!   fully testable on the host.
//! * **Execution** — actually move the bytes. Platform-specific, and gated behind a trait so the
//!   planning logic can be exercised without a Windows machine.
//!
//! The split exists because the containment rules are the security-relevant part and they must be
//! testable without a VM. If the check and the copy live in the same function, the check only
//! ever gets tested by actually performing the transfer.
//!
//! ## Rules
//!
//! A transfer is planned only if **both ends** are inside their declared roots. Checking one end
//! leaves the other as an unguarded path, which is how a sandbox becomes a suggestion.

#![allow(dead_code)] // Wired into the request dispatch in M4; see docs/BUILD-PLAN.md.

use std::fmt;

use anyhow::{bail, Result};

use crate::paths;

/// Which way the bytes move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Host file → guest path.
    HostToGuest,
    /// Guest file → host path.
    GuestToHost,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The wire spelling, matching the serde rename, so the payload's `direction` field is the
        // same string the request used. A caller comparing them should not have to translate.
        match self {
            Direction::HostToGuest => write!(f, "host_to_guest"),
            Direction::GuestToHost => write!(f, "guest_to_host"),
        }
    }
}

/// What the guest was asked to do, after planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPlan {
    pub direction: Direction,
    /// Guest-side path, in native separators.
    pub guest_path: String,
    /// Host-side path, as the host gave it.
    pub host_path: String,
    /// Whether existing files at the destination may be replaced.
    ///
    /// Defaults to false. A transfer that silently overwrites is a data-loss bug waiting for a
    /// caller that retried.
    pub overwrite: bool,
}

impl fmt::Display for TransferPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arrow = match self.direction {
            Direction::HostToGuest => "->",
            Direction::GuestToHost => "<-",
        };
        write!(f, "{} {} {}", self.host_path, arrow, self.guest_path)
    }
}

/// Plan a transfer, or refuse it.
///
/// `guest_root` and `host_root` come from the capability grant. Passing an empty root is not
/// "no restriction" — it is a refusal, because a grant with no roots is a caller that has not
/// been given a place to put anything.
pub fn plan(
    direction: Direction,
    guest_root: &str,
    host_root: &str,
    guest_path: &str,
    host_path: &str,
) -> Result<TransferPlan> {
    if guest_root.trim().is_empty() {
        bail!("refusing transfer: the grant declares no guest root");
    }
    if host_root.trim().is_empty() {
        bail!("refusing transfer: the grant declares no host root");
    }

    // Guest side: containment checked with the shared Windows path rules.
    let resolved_guest = paths::resolve_within(guest_root, guest_path)?;

    // Host side: the host has already applied its own rules, but refusing to take an obviously
    // outside-the-root path here means a bug on the host cannot turn into a write anywhere on
    // the guest's view of the host filesystem.
    if !host_path.starts_with(host_root.trim_end_matches('/')) {
        bail!(
            "refusing transfer: host path '{}' is outside '{}'",
            host_path,
            host_root
        );
    }

    if guest_path.trim().is_empty() {
        bail!("refusing transfer: empty guest path");
    }

    Ok(TransferPlan {
        direction,
        guest_path: resolved_guest,
        host_path: host_path.to_string(),
        overwrite: false,
    })
}

/// Moves bytes. Implemented per platform.
///
/// Note the shape: every method takes `&self` and the write methods return the data rather than
/// mutating. That is not an accident — it keeps the containment logic testable without a mutable
/// store, and it means the platform layer has a small, visibly total surface. The in-memory
/// implementation in the tests satisfies the same trait the Windows one will.
pub trait TransferIo {
    /// Read the whole host-side file. Bounded by the caller's message-size limit.
    fn read_host(&self, path: &str) -> Result<Vec<u8>>;

    /// Write the whole host-side file.
    fn write_host(&self, path: &str, data: &[u8]) -> Result<()>;

    /// Read a guest-side file.
    fn read_guest(&self, path: &str) -> Result<Vec<u8>>;

    /// Write a guest-side file.
    fn write_guest(&self, path: &str, data: &[u8]) -> Result<()>;

    /// Does a file exist at this guest path?
    fn guest_exists(&self, path: &str) -> Result<bool>;

    /// Does a file exist at this host path?
    fn host_exists(&self, path: &str) -> Result<bool>;
}

/// Why a transfer was refused, as a value rather than a string.
///
/// The caller journals this, so the reason needs to be matchable rather than parsed back out of
/// prose. The `Display` impl exists for the human-readable side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The destination exists and overwrite was not requested.
    WouldOverwrite { side: &'static str, path: String },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::WouldOverwrite { side, path } => write!(
                f,
                "refusing to overwrite an existing file on the {side} side: {path}"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// Execute a planned transfer against any implementation.
///
/// The overwrite check happens **before** any bytes are read, so a refusal cannot leave a
/// half-written file behind.
pub fn execute<I: TransferIo>(io: &I, plan: &TransferPlan) -> Result<u64> {
    check_destination(io, plan)?;

    let bytes = match plan.direction {
        Direction::HostToGuest => {
            let data = io.read_host(&plan.host_path)?;
            io.write_guest(&plan.guest_path, &data)?;
            data.len()
        }
        Direction::GuestToHost => {
            let data = io.read_guest(&plan.guest_path)?;
            io.write_host(&plan.host_path, &data)?;
            data.len()
        }
    };

    Ok(bytes as u64)
}

/// Refuse before reading, so a refusal is side-effect free.
fn check_destination(io: &dyn TransferIo, plan: &TransferPlan) -> Result<()> {
    let (exists, side, path) = match plan.direction {
        Direction::HostToGuest => (
            io.guest_exists(&plan.guest_path)?,
            "guest",
            &plan.guest_path,
        ),
        Direction::GuestToHost => (io.host_exists(&plan.host_path)?, "host", &plan.host_path),
    };

    if exists && !plan.overwrite {
        return Err(Refusal::WouldOverwrite {
            side,
            path: path.clone(),
        }
        .into());
    }
    Ok(())
}

/// In-memory `TransferIo`, used by the tests and by `wvm` for a dry run.
///
/// Deliberately not a mock framework: it is a real, working implementation over two maps, so the
/// tests exercise the same `execute` path production does rather than a substitute for it.
///
/// Interior mutability is used for the writes so the type can satisfy the `&self` trait. That is
/// the honest shape here — a real filesystem write is also `&self` from the caller's point of
/// view, and pretending otherwise in the test double would make the double easier to satisfy than
/// the real thing.
#[derive(Debug, Default)]
pub struct MemoryIo {
    host: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    guest: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl MemoryIo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_host_file(self, path: &str, data: &[u8]) -> Self {
        self.host
            .lock()
            .expect("host map")
            .insert(path.to_string(), data.to_vec());
        self
    }

    pub fn with_guest_file(self, path: &str, data: &[u8]) -> Self {
        self.guest
            .lock()
            .expect("guest map")
            .insert(path.to_string(), data.to_vec());
        self
    }

    /// Read back a guest-side file, for assertions.
    pub fn guest_contents(&self, path: &str) -> Option<Vec<u8>> {
        self.guest.lock().expect("guest map").get(path).cloned()
    }

    /// Read back a host-side file, for assertions.
    pub fn host_contents(&self, path: &str) -> Option<Vec<u8>> {
        self.host.lock().expect("host map").get(path).cloned()
    }
}

impl TransferIo for MemoryIo {
    fn read_host(&self, path: &str) -> Result<Vec<u8>> {
        self.host
            .lock()
            .expect("host map")
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such host file: {path}"))
    }

    fn write_host(&self, path: &str, data: &[u8]) -> Result<()> {
        self.host
            .lock()
            .expect("host map")
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn read_guest(&self, path: &str) -> Result<Vec<u8>> {
        self.guest
            .lock()
            .expect("guest map")
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such guest file: {path}"))
    }

    fn write_guest(&self, path: &str, data: &[u8]) -> Result<()> {
        self.guest
            .lock()
            .expect("guest map")
            .insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn guest_exists(&self, path: &str) -> Result<bool> {
        Ok(self.guest.lock().expect("guest map").contains_key(path))
    }

    fn host_exists(&self, path: &str) -> Result<bool> {
        Ok(self.host.lock().expect("host map").contains_key(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_ROOT: &str = "C:\\wvm";
    const HOST_ROOT: &str = "/srv/wvm/in";

    #[test]
    fn a_transfer_inside_both_roots_is_planned() {
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\file.txt",
            "/srv/wvm/in/file.txt",
        )
        .expect("should be permitted");
        assert_eq!(p.guest_path, "C:\\wvm\\file.txt");
        assert_eq!(p.direction, Direction::HostToGuest);
    }

    #[test]
    fn a_guest_path_outside_its_root_is_refused() {
        let err = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\Windows\\System32\\evil.dll",
            "/srv/wvm/in/evil.dll",
        )
        .expect_err("must be refused");
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn a_host_path_outside_its_root_is_refused() {
        // The host should never send this, but if it does, refusing here means a host bug cannot
        // become a write anywhere on the guest's view of the host filesystem.
        let err = plan(
            Direction::GuestToHost,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\file.txt",
            "/etc/passwd",
        )
        .expect_err("must be refused");
        assert!(err.to_string().contains("outside"));
    }

    #[test]
    fn traversal_on_the_guest_side_is_refused() {
        let err = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\..\\Windows\\System32\\config\\SAM",
            "/srv/wvm/in/sam",
        )
        .expect_err("must be refused");
        assert!(err.to_string().contains("refusing"));
    }

    #[test]
    fn an_empty_guest_root_refuses_everything() {
        let err = plan(
            Direction::HostToGuest,
            "",
            HOST_ROOT,
            "C:\\wvm\\x",
            "/srv/wvm/in/x",
        )
        .expect_err("an empty root is not 'no restriction'");
        assert!(err.to_string().contains("no guest root"));
    }

    #[test]
    fn an_empty_host_root_refuses_everything() {
        let err = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            "",
            "C:\\wvm\\x",
            "/srv/wvm/in/x",
        )
        .expect_err("an empty root is a refusal");
        assert!(err.to_string().contains("no host root"));
    }

    #[test]
    fn overwrite_defaults_to_false() {
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\f",
            "/srv/wvm/in/f",
        )
        .unwrap();
        assert!(!p.overwrite, "a plan must not clobber by default");
    }

    #[test]
    fn a_full_host_to_guest_transfer_moves_the_bytes() {
        let io = MemoryIo::new().with_host_file("/srv/wvm/in/a.bin", b"hello");
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\a.bin",
            "/srv/wvm/in/a.bin",
        )
        .unwrap();

        let n = execute(&io, &p).expect("transfer");
        assert_eq!(n, 5);
        assert_eq!(
            io.guest_contents("C:\\wvm\\a.bin").as_deref(),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn a_full_guest_to_host_transfer_moves_the_bytes() {
        let io = MemoryIo::new().with_guest_file("C:\\wvm\\out.log", b"line\n");
        let p = plan(
            Direction::GuestToHost,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\out.log",
            "/srv/wvm/in/out.log",
        )
        .unwrap();

        let n = execute(&io, &p).expect("transfer");
        assert_eq!(n, 5);
        assert_eq!(
            io.host_contents("/srv/wvm/in/out.log").as_deref(),
            Some(&b"line\n"[..])
        );
    }

    #[test]
    fn an_existing_destination_is_refused_not_clobbered() {
        let io = MemoryIo::new()
            .with_host_file("/srv/wvm/in/a.bin", b"new")
            .with_guest_file("C:\\wvm\\a.bin", b"PRECIOUS");
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\a.bin",
            "/srv/wvm/in/a.bin",
        )
        .unwrap();

        let err = execute(&io, &p).expect_err("must not clobber");
        assert!(err.to_string().contains("refusing to overwrite"));
        // The refusal must be the typed value, not a string the caller has to parse.
        let refusal = err.downcast_ref::<Refusal>();
        assert_eq!(
            refusal,
            Some(&Refusal::WouldOverwrite {
                side: "guest",
                path: "C:\\wvm\\a.bin".into()
            })
        );
        // The existing content must be untouched.
        assert_eq!(
            io.guest_contents("C:\\wvm\\a.bin").as_deref(),
            Some(&b"PRECIOUS"[..])
        );
    }

    #[test]
    fn overwrite_when_explicitly_requested_does_replace() {
        let io = MemoryIo::new()
            .with_host_file("/srv/wvm/in/a.bin", b"new")
            .with_guest_file("C:\\wvm\\a.bin", b"old");
        let mut p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\a.bin",
            "/srv/wvm/in/a.bin",
        )
        .unwrap();
        p.overwrite = true;

        execute(&io, &p).expect("explicit overwrite should proceed");
        assert_eq!(
            io.guest_contents("C:\\wvm\\a.bin").as_deref(),
            Some(&b"new"[..])
        );
    }

    #[test]
    fn a_missing_source_is_an_error_not_a_silent_empty_file() {
        let io = MemoryIo::new();
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\missing.bin",
            "/srv/wvm/in/missing.bin",
        )
        .unwrap();

        assert!(execute(&io, &p).is_err());
        assert!(
            io.guest_contents("C:\\wvm\\missing.bin").is_none(),
            "a failed transfer must not leave an empty file behind"
        );
    }

    #[test]
    fn the_plan_renders_readably_for_the_journal() {
        let p = plan(
            Direction::HostToGuest,
            GUEST_ROOT,
            HOST_ROOT,
            "C:\\wvm\\f",
            "/srv/wvm/in/f",
        )
        .unwrap();
        let text = p.to_string();
        assert!(text.contains("->"), "{text}");
        assert!(text.contains("C:\\wvm\\f"), "{text}");
    }
}
