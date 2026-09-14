//! Policy: the decisions the host makes before anything reaches the guest.
//!
//! Two rules are enforced here, both of which the original project this was inspired by got
//! wrong in ways that mattered (see `docs/DECISIONS.md` D-001):
//!
//! * **No root exposure.** Transfers are confined to declared roots. A path that resolves
//!   outside them is refused, with `..` traversal resolved before the check rather than
//!   string-matched.
//! * **No unvetted execution.** A request that asks for the allowlist is only dispatched if the
//!   program appears on it.
//!
//! Every refusal produces a [`Denial`] which the caller journals. Nothing here silently
//! downgrades a request to something permissible — that would hide the intent.

#![allow(dead_code)] // Wired into the request path in M2; see docs/BUILD-PLAN.md.

use std::path::{Component, Path, PathBuf};

use wvm_ipc::{DenialReason, Denied, Grant, Verb};

/// The host's execution allowlist.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    /// Exact program paths permitted to run, compared case-insensitively because Windows paths
    /// are case-insensitive and a caller should not have to guess the canonical casing.
    entries: Vec<String>,
}

impl Allowlist {
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Allowlist {
            entries: entries
                .into_iter()
                .map(|s| s.into().to_lowercase())
                .collect(),
        }
    }

    /// The allowlist every install starts with: enough to be useful, narrow enough to be a real
    /// boundary. Anything else is a deliberate edit by the operator.
    pub fn default_for_windows() -> Self {
        Self::new([
            "c:\\windows\\system32\\cmd.exe",
            "c:\\windows\\system32\\windowspowershell\\v1.0\\powershell.exe",
            "c:\\windows\\system32\\notepad.exe",
        ])
    }

    pub fn permits(&self, program: &str) -> bool {
        let needle = program.to_lowercase();
        self.entries.contains(&needle)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A refusal produced by policy, with enough context to journal it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub subject: String,
    pub verb: Verb,
    pub reason: DenialReason,
    /// Path or program that triggered it, for the audit trail.
    pub subject_detail: Option<String>,
}

impl Denial {
    pub fn to_ipc(&self) -> Denied {
        Denied {
            subject: self.subject.clone(),
            verb: self.verb,
            reason: self.reason,
        }
    }
}

/// Resolve a path lexically and confirm it sits inside one of `roots`.
///
/// Resolution happens *before* the comparison, so `root/../../etc/passwd` is refused rather than
/// passing a naive prefix match. This is lexical rather than filesystem-based on purpose: the
/// path may not exist yet (a write target), and we must not follow symlinks out of the root.
pub fn path_within_roots(path: &Path, roots: &[String]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let resolved = lexical_resolve(path);
    roots.iter().any(|root| {
        let root_path = lexical_resolve(Path::new(root));
        resolved.starts_with(&root_path)
    })
}

/// Resolve `.` and `..` without touching the filesystem.
fn lexical_resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop, but never above the root of the path we were given.
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Check a host-side transfer against the grant.
///
/// Note the asymmetry: reads use `read_roots`, writes use `write_roots`. A grant may sensibly
/// allow reading a directory without permitting writes to it.
pub fn check_host_transfer(
    grant: &Grant,
    direction: wvm_ipc::TransferDirection,
    host_path: &Path,
) -> Result<(), Denial> {
    let (roots, ok) = match direction {
        wvm_ipc::TransferDirection::HostToGuest => (&grant.write_roots, true),
        wvm_ipc::TransferDirection::GuestToHost => (&grant.read_roots, true),
    };

    if !ok {
        unreachable!()
    }

    if roots.is_empty() {
        return Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Transfer,
            reason: DenialReason::NoTransferRoot,
            subject_detail: Some(host_path.display().to_string()),
        });
    }

    if !path_within_roots(host_path, roots) {
        return Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Transfer,
            reason: DenialReason::PathOutsideRoots,
            subject_detail: Some(host_path.display().to_string()),
        });
    }

    Ok(())
}

/// Check a guest-side path against the grant's guest root.
///
/// Guest paths are Windows-style, so traversal and separators are handled separately from the
/// host check. Both `\` and `/` are treated as separators: Windows accepts either, and a check
/// that only understands one of them is trivially bypassed.
pub fn check_guest_path(grant: &Grant, guest_path: &str) -> Result<(), Denial> {
    if grant.guest_root.trim().is_empty() {
        return Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Transfer,
            reason: DenialReason::NoTransferRoot,
            subject_detail: Some(guest_path.to_string()),
        });
    }

    let normalise = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let root = normalise(&grant.guest_root);
    if root.is_empty() {
        return Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Transfer,
            reason: DenialReason::NoTransferRoot,
            subject_detail: Some(guest_path.to_string()),
        });
    }

    let normalised = guest_path.replace('/', "\\");
    let mut parts: Vec<&str> = Vec::new();
    for segment in normalised.split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let resolved = parts.join("\\").to_lowercase();

    // Compare on segment boundaries: "C:\wvmX" must not satisfy root "C:\wvm".
    if resolved == root || resolved.starts_with(&format!("{root}\\")) {
        Ok(())
    } else {
        Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Transfer,
            reason: DenialReason::PathOutsideRoots,
            subject_detail: Some(guest_path.to_string()),
        })
    }
}

/// Check an execution request against the allowlist.
pub fn check_exec(grant: &Grant, program: &str, allowlist: &Allowlist) -> Result<(), Denial> {
    if !allowlist.permits(program) {
        return Err(Denial {
            subject: grant.subject.clone(),
            verb: Verb::Exec,
            reason: DenialReason::ProgramNotAllowlisted,
            subject_detail: Some(program.to_string()),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wvm_ipc::TransferDirection;

    fn grant_with_roots() -> Grant {
        Grant {
            subject: "agent".into(),
            verbs: vec![Verb::Transfer, Verb::Exec],
            read_roots: vec!["/srv/wvm/in".into()],
            write_roots: vec!["/srv/wvm/out".into()],
            guest_root: "C:\\wvm".into(),
        }
    }

    #[test]
    fn path_inside_root_is_permitted() {
        assert!(path_within_roots(
            Path::new("/srv/wvm/in/a.txt"),
            &["/srv/wvm/in".into()]
        ));
    }

    #[test]
    fn path_outside_root_is_refused() {
        assert!(!path_within_roots(
            Path::new("/etc/passwd"),
            &["/srv/wvm/in".into()]
        ));
    }

    #[test]
    fn traversal_out_of_root_is_refused() {
        // The naive prefix check would pass this. Resolution must not.
        assert!(!path_within_roots(
            Path::new("/srv/wvm/in/../../../etc/passwd"),
            &["/srv/wvm/in".into()]
        ));
    }

    #[test]
    fn sibling_with_shared_prefix_is_refused() {
        // "/srv/wvm/input" must not satisfy root "/srv/wvm/in" without a separator check.
        // Path::starts_with works on components, so this is the regression guard for that.
        assert!(!path_within_roots(
            Path::new("/srv/wvm/input/x"),
            &["/srv/wvm/in".into()]
        ));
    }

    #[test]
    fn empty_roots_permit_nothing() {
        assert!(!path_within_roots(Path::new("/srv/wvm/in/a.txt"), &[]));
    }

    #[test]
    fn host_transfer_direction_uses_the_right_root() {
        let g = grant_with_roots();

        // Writing to the read root must fail: it is not a write root.
        let err = check_host_transfer(
            &g,
            TransferDirection::HostToGuest,
            Path::new("/srv/wvm/in/x"),
        )
        .expect_err("write into read-only root must be refused");
        assert_eq!(err.reason, DenialReason::PathOutsideRoots);

        // Writing to the write root is fine.
        assert!(check_host_transfer(
            &g,
            TransferDirection::HostToGuest,
            Path::new("/srv/wvm/out/x")
        )
        .is_ok());
    }

    #[test]
    fn host_transfer_with_no_roots_is_refused() {
        let mut g = grant_with_roots();
        g.write_roots.clear();
        let err = check_host_transfer(
            &g,
            TransferDirection::HostToGuest,
            Path::new("/srv/wvm/out/x"),
        )
        .expect_err("no write root must be a refusal, not a pass");
        assert_eq!(err.reason, DenialReason::NoTransferRoot);
    }

    #[test]
    fn guest_path_inside_root_is_permitted() {
        let g = grant_with_roots();
        assert!(check_guest_path(&g, "C:\\wvm\\file.txt").is_ok());
        assert!(check_guest_path(&g, "c:/wvm/sub/file.txt").is_ok());
    }

    #[test]
    fn guest_path_traversal_is_refused() {
        let g = grant_with_roots();
        assert!(check_guest_path(&g, "C:\\wvm\\..\\Windows\\System32\\config").is_err());
    }

    #[test]
    fn guest_path_sibling_with_shared_prefix_is_refused() {
        let g = grant_with_roots();
        // "C:\wvmdata" must not satisfy root "C:\wvm".
        assert!(check_guest_path(&g, "C:\\wvmdata\\x").is_err());
    }

    #[test]
    fn guest_path_with_empty_root_permits_nothing() {
        let mut g = grant_with_roots();
        g.guest_root = String::new();
        assert!(check_guest_path(&g, "C:\\wvm\\x").is_err());
    }

    #[test]
    fn allowlist_permits_only_listed_programs() {
        let a = Allowlist::default_for_windows();
        assert!(a.permits("C:\\Windows\\System32\\cmd.exe"));
        // Case-insensitive, because Windows paths are.
        assert!(a.permits("c:\\windows\\system32\\CMD.EXE"));
        assert!(!a.permits("C:\\Users\\admin\\Downloads\\payload.exe"));
    }

    #[test]
    fn exec_outside_allowlist_is_refused() {
        let g = grant_with_roots();
        let a = Allowlist::default_for_windows();
        let err = check_exec(&g, "C:\\evil.exe", &a).expect_err("unlisted program must be refused");
        assert_eq!(err.reason, DenialReason::ProgramNotAllowlisted);
    }

    #[test]
    fn denial_converts_to_the_wire_type() {
        let g = grant_with_roots();
        let a = Allowlist::default_for_windows();
        let err = check_exec(&g, "C:\\evil.exe", &a).unwrap_err();
        let wire = err.to_ipc();
        assert_eq!(wire.subject, "agent");
        assert_eq!(wire.verb, Verb::Exec);
    }
}
