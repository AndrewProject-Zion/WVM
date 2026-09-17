//! Putting the guest's desktop on screen, on request, and taking it away again.
//!
//! The VM always HAS a display server (D-021: an ignored one measures at zero cost), so "showing"
//! the desktop is really "attaching a viewer to a socket that is already there". That is what makes
//! the window removable, which is the actual gap: `scripts/start-windows.sh` currently starts QEMU
//! with `-display gtk`, and a GTK display lives INSIDE the QEMU process -- the only way to close
//! that window is to kill the VM. A viewer on a socket is a separate process, so it can be closed
//! and reopened at will, and the guest never notices.
//!
//! Everything here MEASURES rather than assumes (D-005). In particular `status` does not stat() the
//! socket to decide whether the display is available: QEMU unlinks its socket on a clean exit but
//! NOT on a crash, so a stale socket and a live one look identical on the filesystem. The only
//! honest test is to try to connect.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How to reach a viewer for this VM.
pub fn socket_path(config: &crate::vm::VmConfig) -> PathBuf {
    config.spice_socket()
}

/// A viewer we started, so we can tell ours from someone else's.
///
/// Identified by the socket path in the command line rather than by a marker file: a marker file is
/// a second source of truth that goes stale the moment a viewer is killed by hand or the machine
/// reboots, and then "hide" would try to stop something that is not there while missing one that
/// is.
const VIEWER_HINT: &str = "remote-viewer";

/// Is a display server actually listening, as opposed to a socket file existing?
///
/// This is the D-005 rule: a diagnostic attempts the operation. A `stat()` succeeds on a socket
/// left behind by a crashed QEMU, and reporting "available" there is exactly the kind of confident
/// wrong answer this project keeps paying for.
pub fn is_listening(config: &crate::vm::VmConfig) -> bool {
    let path = socket_path(config);
    if !Path::new(&path).exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// How many viewers are attached to this VM's display right now.
pub fn viewer_count(config: &crate::vm::VmConfig) -> usize {
    let path = socket_path(config).to_string_lossy().to_string();
    readers_of_us(&path).len()
}

/// Processes whose command line references this VM's display socket.
///
/// Read from /proc rather than `pgrep -f`, because a pattern match is also satisfied by the shell
/// that is running the search itself -- which has produced a phantom "1 viewer" in this project
/// before. Reading each process's own argv cannot match the searcher, since the searcher's argv is
/// the search string, not the socket path.
fn readers_of_us(socket: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv = String::from_utf8_lossy(&raw).replace('\0', " ");
        // Both must be true: the socket identifies the VM, the hint identifies it as a viewer and
        // not, say, this very process asking the question.
        if argv.contains(socket) && argv.contains(VIEWER_HINT) {
            found.push(pid);
        }
    }
    found
}

/// Attach a viewer. Returns the viewer's pid.
///
/// Detached deliberately: `wvm vm display show` should return control to the caller, and the viewer
/// should outlive the command that opened it -- that is the whole point of a window you can leave
/// open while doing something else.
pub fn show(config: &crate::vm::VmConfig) -> Result<u32> {
    if !is_listening(config) {
        anyhow::bail!(
            "the VM has no display server listening on {}. Start it with `wvm vm start`; the \
             server is created at launch and cannot be attached to a running VM (D-021).",
            socket_path(config).display()
        );
    }
    // Idempotent on purpose. A SECOND attachment to the same SPICE server is the unreliable case:
    // measured here, `show` took the count 1 -> 2 and one of them then exited, which is the exact
    // symptom reported as "the other viewer says connected to server but never makes it". Rather
    // than open a second window that may never paint, say the desktop is already on screen.
    let attached = viewer_count(config);
    if attached > 0 {
        anyhow::bail!(
            "a viewer is already attached to this display ({attached}); nothing to do. Close that \
             window first if you meant to reopen it. A second simultaneous SPICE viewer is the \
             known-unreliable path."
        );
    }

    let uri = format!("spice+unix://{}", socket_path(config).display());
    let child = Command::new(VIEWER_HINT)
        .arg(&uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "could not start `{VIEWER_HINT}`. Install virt-viewer (it provides \
                     remote-viewer), which speaks spice+unix:// natively -- vncviewer does not."
            )
        })?;
    Ok(child.id())
}

/// Close the viewers we can identify for this VM. Returns how many were asked to stop.
///
/// This is safe to call when there is nothing to stop, and it deliberately does NOT touch the VM or
/// the QEMU process: closing a window must never be able to take the machine down, which is the
/// property the GTK display lacks.
pub fn hide(config: &crate::vm::VmConfig) -> Result<usize> {
    let pids = readers_of_us(&socket_path(config).to_string_lossy());
    let mut stopped = 0;
    for pid in &pids {
        // SIGTERM, so the viewer can shut its connection down cleanly and QEMU sees an orderly
        // disconnect rather than a half-open channel.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
        stopped += 1;
    }
    Ok(stopped)
}

/// A one-line summary of the display for `status`.
pub fn describe(config: &crate::vm::VmConfig) -> String {
    let path = socket_path(config);
    if !path.exists() {
        return "no display server: the socket does not exist (is the VM running?)".to_string();
    }
    if !is_listening(config) {
        return format!(
            "socket {} exists but NOTHING is listening -- this is a stale socket, likely left by \
             a QEMU that crashed rather than exited. It does not mean the display is available.",
            path.display()
        );
    }
    let n = viewer_count(config);
    match n {
        0 => format!(
            "display available on {}; no viewer attached",
            path.display()
        ),
        1 => format!("display available on {}; 1 viewer attached", path.display()),
        _ => format!(
            "display available on {}; {n} viewers attached",
            path.display()
        ),
    }
}
