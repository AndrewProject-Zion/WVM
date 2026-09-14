//! Host environment checks.
//!
//! Every check here corresponds to a row in `docs/VERIFIED-ENVIRONMENT.md`. The point is that a
//! user on an unknown machine can run one command and find out exactly what is missing, rather
//! than discovering it as an opaque failure three steps later.
//!
//! The `kvm` group check exists because it is the single most common "installed fine, VM never
//! starts" cause: `/dev/kvm` is mode 0660 owned by `root:kvm`, so a user not in that group gets
//! a permission error that looks like a missing feature.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Pass => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// What the user should do about it, when there is something to do.
    pub hint: Option<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn push(
        &mut self,
        name: &'static str,
        status: Status,
        detail: impl Into<String>,
        hint: Option<&str>,
    ) {
        self.checks.push(Check {
            name,
            status,
            detail: detail.into(),
            hint: hint.map(str::to_string),
        });
    }

    /// True if nothing failed. Warnings are permitted — they represent optional capability.
    pub fn all_passed(&self) -> bool {
        !self.checks.iter().any(|c| c.status == Status::Fail)
    }

    pub fn render(&self) -> String {
        let mut out = String::from("wvm doctor\n\n");
        for c in &self.checks {
            out.push_str(&format!(
                "  [{}] {:<22} {}\n",
                c.status.label(),
                c.name,
                c.detail
            ));
            if let Some(hint) = &c.hint {
                out.push_str(&format!("         -> {}\n", hint));
            }
        }

        let fails = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let warns = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        out.push('\n');
        if fails == 0 {
            out.push_str(&format!("passed with {} warning(s)\n", warns));
        } else {
            out.push_str(&format!(
                "{} check(s) failed, {} warning(s)\n",
                fails, warns
            ));
        }
        out
    }
}

/// Run every check. Never panics and never exits: reporting is the caller's job.
pub fn run() -> Report {
    let mut r = Report::default();
    check_kvm(&mut r);
    check_qemu(&mut r);
    check_vsock(&mut r);
    check_rust_targets(&mut r);
    check_resources(&mut r);
    r
}

/// `/dev/kvm` present AND openable by the current user.
fn check_kvm(r: &mut Report) {
    let path = Path::new("/dev/kvm");
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            r.push(
                "kvm device",
                Status::Fail,
                "/dev/kvm does not exist",
                Some("Enable AMD-V/VT-x in firmware, then: sudo modprobe kvm_amd (or kvm_intel)"),
            );
            return;
        }
    };

    // Mode and group are informative, but the authoritative test is whether we can open it.
    let mode = meta.mode() & 0o777;
    let gid = meta.gid();

    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(_) => {
            // Access can come from group membership *or* from an ACL. Report which, because it
            // changes what an operator needs to know: an ACL grant is invisible to `groups` and
            // surprises people debugging "why does this work on my machine and not yours".
            let group = group_name_for_gid(gid).unwrap_or_else(|| gid.to_string());
            let in_group = user_in_group(&group);
            let how = if in_group {
                format!("in the '{group}' group")
            } else {
                format!("via an ACL (not a member of '{group}')")
            };
            r.push(
                "kvm access",
                Status::Pass,
                format!("/dev/kvm openable, mode {:o}, {}", mode, how),
                None,
            );
        }
        Err(e) => {
            // Name the group explicitly: this is the fix nine times out of ten.
            let group = group_name_for_gid(gid).unwrap_or_else(|| gid.to_string());
            r.push(
                "kvm access",
                Status::Fail,
                format!("/dev/kvm exists but is not openable: {}", e),
                Some(&format!(
                    "Add yourself to the '{group}' group: sudo usermod -aG {group} $USER  (then log out and back in)"
                )),
            );
        }
    }
}

/// Resolve a gid to a name using /etc/group, without pulling in a dependency.
fn group_name_for_gid(gid: u32) -> Option<String> {
    let contents = std::fs::read_to_string("/etc/group").ok()?;
    for line in contents.lines() {
        let mut parts = line.split(':');
        let name = parts.next()?;
        let _passwd = parts.next()?;
        if let Some(g) = parts.next() {
            if g.parse::<u32>().ok() == Some(gid) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Is the current user a member of `group`, by primary gid or by /etc/group membership?
///
/// Used only to *explain* how access is obtained. Access itself is decided by attempting the
/// open, never by this check.
fn user_in_group(group: &str) -> bool {
    let username = std::env::var("USER").unwrap_or_default();
    if username.is_empty() {
        return false;
    }
    let contents = match std::fs::read_to_string("/etc/group") {
        Ok(c) => c,
        Err(_) => return false,
    };
    for line in contents.lines() {
        let mut parts = line.split(':');
        let name = parts.next().unwrap_or_default();
        if name != group {
            continue;
        }
        let _passwd = parts.next();
        let _gid = parts.next();
        if let Some(members) = parts.next() {
            return members.split(',').any(|m| m.trim() == username);
        }
    }
    false
}

/// QEMU present and new enough.
fn check_qemu(r: &mut Report) {
    match Command::new("qemu-system-x86_64").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("unknown")
                .to_string();
            r.push("qemu", Status::Pass, first, None);
        }
        Ok(_) => {
            r.push(
                "qemu",
                Status::Fail,
                "qemu-system-x86_64 ran but exited non-zero",
                Some("Reinstall QEMU for your distribution"),
            );
        }
        Err(_) => {
            r.push(
                "qemu",
                Status::Fail,
                "qemu-system-x86_64 not found on PATH",
                Some("Install qemu-system-x86 (Debian/Ubuntu) or qemu-full (Arch)"),
            );
        }
    }
}

/// virtio-vsock availability. Warn-only: TCP is the default transport.
fn check_vsock(r: &mut Report) {
    let dev = Path::new("/dev/vhost-vsock");
    if dev.exists() {
        r.push(
            "vhost-vsock",
            Status::Pass,
            "/dev/vhost-vsock present (optional transport available)",
            None,
        );
    } else {
        r.push(
            "vhost-vsock",
            Status::Warn,
            "not present; the vsock transport will be unavailable",
            Some("Optional. The default TCP transport works without it."),
        );
    }
}

/// Windows cross-compilation prerequisites, for building the guest service from Linux.
fn check_rust_targets(r: &mut Report) {
    match Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let installed = String::from_utf8_lossy(&out.stdout);
            if installed.contains("x86_64-pc-windows-gnu") {
                r.push(
                    "windows target",
                    Status::Pass,
                    "x86_64-pc-windows-gnu installed",
                    None,
                );
            } else {
                r.push(
                    "windows target",
                    Status::Warn,
                    "x86_64-pc-windows-gnu not installed",
                    Some("Needed to build the guest service: rustup target add x86_64-pc-windows-gnu"),
                );
            }
        }
        Ok(_) | Err(_) => {
            r.push(
                "windows target",
                Status::Warn,
                "rustup not found; cannot verify cross-compilation target",
                Some("Install rustup if you intend to build the guest service"),
            );
        }
    }

    let linker_ok = Command::new("x86_64-w64-mingw32-gcc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if linker_ok {
        r.push(
            "mingw linker",
            Status::Pass,
            "x86_64-w64-mingw32-gcc available",
            None,
        );
    } else {
        r.push(
            "mingw linker",
            Status::Warn,
            "x86_64-w64-mingw32-gcc not found",
            Some("Install gcc-mingw-w64-x86-64 to link the guest service"),
        );
    }
}

/// Memory and disk headroom. A Windows guest needs real memory; report rather than assume.
fn check_resources(r: &mut Report) {
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        let total_kb = contents
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok());

        if let Some(kb) = total_kb {
            let gib = kb as f64 / 1024.0 / 1024.0;
            let status = if gib >= 8.0 {
                Status::Pass
            } else {
                Status::Warn
            };
            r.push(
                "memory",
                status,
                format!("{:.1} GiB total", gib),
                if status == Status::Warn {
                    Some("8 GiB or more recommended for a Windows 11 guest")
                } else {
                    None
                },
            );
        }
    }
}
