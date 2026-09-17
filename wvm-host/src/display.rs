//! Putting the guest's desktop on screen, on request, and taking it away again.
//!
//! The VM always HAS a display server (D-021: an ignored one measures at zero cost), so "showing"
//! the desktop is really "attaching a viewer to a socket that is already there". That is what makes
//! the window removable, which is the actual gap: `scripts/start-windows.sh` currently starts QEMU
//! with `-display gtk`, and a GTK display lives INSIDE the QEMU process -- the only way to close
//! that window is to kill the VM. A viewer on a socket is a separate process, so it can be closed
//! and reopened at will, and the guest never notices.
//!
//! Everything here MEASURES rather than assumes (D-005). In particular the availability check does
//! not stat() the socket: QEMU unlinks its socket on a clean exit but NOT on a crash, so a stale
//! socket and a live one look identical on the filesystem. The only honest test is to connect.
//!
//! The path-taking functions are the real ones and the `&VmConfig` ones are thin wrappers. That is
//! deliberate: it keeps the testable logic free of a fixture, and the tests below exercise the
//! socket states that matter instead of a config object.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A viewer we started, so we can tell ours from someone else's.
const VIEWER_HINT: &str = "remote-viewer";

/// How to reach a viewer for this VM.
pub fn socket_path(config: &crate::vm::VmConfig) -> PathBuf {
    config.spice_socket()
}

/// Is a display server actually LISTENING, as opposed to a socket file merely existing?
///
/// The D-005 rule: a diagnostic attempts the operation. A `stat()` succeeds on a socket left behind
/// by a crashed QEMU, and reporting "available" there is exactly the kind of confident wrong answer
/// this project keeps paying for.
pub fn is_listening_at(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Does this process's command line identify it as a VIEWER for this display?
///
/// A pure function so it can be tested without spawning anything, and it is the piece worth
/// testing: the failure it guards against is invisible until it produces a phantom. A search for
/// the socket path alone also matches the SHELL that is running the search -- the path is in that
/// shell's own command line -- and counting that would report a viewer that does not exist, after
/// which `hide` would signal an innocent process. Requiring the viewer's own name as well cannot
/// match the searcher, whose argv is the search string itself.
pub(crate) fn is_viewer_argv(argv: &str, socket: &str) -> bool {
    argv.contains(socket) && argv.contains(VIEWER_HINT)
}

/// Processes whose command line identifies them as a viewer for this socket.
///
/// Read from /proc rather than shelled out to `pgrep -f`, for the reason in `is_viewer_argv`.
fn readers_of(socket: &str) -> Vec<u32> {
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
        if is_viewer_argv(&argv, socket) {
            found.push(pid);
        }
    }
    found
}

/// How many viewers are attached to this VM's display right now.
pub fn viewer_count(config: &crate::vm::VmConfig) -> usize {
    readers_of(&socket_path(config).to_string_lossy()).len()
}

/// Attach a viewer. Returns the viewer's pid.
///
/// Detached deliberately: `wvm vm display show` should return control to the caller, and the viewer
/// should outlive the command that opened it -- that is the whole point of a window you can leave
/// open while doing something else.
pub fn show(config: &crate::vm::VmConfig) -> Result<u32> {
    if !is_listening_at(&socket_path(config)) {
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
                "could not start `{VIEWER_HINT}`. Install virt-viewer (it provides remote-viewer), \
                 which speaks spice+unix:// natively -- vncviewer and xtightvncviewer cannot open \
                 a unix socket at all."
            )
        })?;
    Ok(child.id())
}

/// Close the viewers we can identify for this VM. Returns how many were asked to stop.
///
/// Safe to call when there is nothing to stop, and it deliberately does NOT touch the VM or the
/// QEMU process: closing a window must never be able to take the machine down, which is the
/// property the GTK display lacks and the reason this verb exists.
pub fn hide(config: &crate::vm::VmConfig) -> Result<usize> {
    let pids = readers_of(&socket_path(config).to_string_lossy());
    for pid in &pids {
        // SIGTERM, so the viewer shuts its connection down cleanly and QEMU sees an orderly
        // disconnect rather than a half-open channel.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
    }
    Ok(pids.len())
}

/// A one-line summary of the display for `status`.
pub fn describe_at(socket: &Path) -> String {
    if !socket.exists() {
        return "no display server: the socket does not exist (is the VM running?)".to_string();
    }
    if !is_listening_at(socket) {
        return format!(
            "socket {} exists but NOTHING is listening -- this is a stale socket, most likely left \
             by a QEMU that crashed rather than exited. It does not mean the display is available.",
            socket.display()
        );
    }
    match readers_of(&socket.to_string_lossy()).len() {
        0 => format!(
            "display available on {}; no viewer attached",
            socket.display()
        ),
        1 => format!(
            "display available on {}; 1 viewer attached",
            socket.display()
        ),
        n => format!(
            "display available on {}; {n} viewers attached",
            socket.display()
        ),
    }
}

/// A one-line summary of the display for `status`.
pub fn describe(config: &crate::vm::VmConfig) -> String {
    describe_at(&socket_path(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A socket path unique to this test, so tests cannot collide with each other or with a real VM.
    fn temp_socket(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wvm-display-test-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("display.sock")
    }

    #[test]
    fn a_socket_nobody_is_listening_on_is_not_a_display() {
        // The stale-socket case, which is the reason this module connects instead of stat-ing.
        // A crashed QEMU leaves the file behind, and `exists()` is true for it.
        let path = temp_socket("stale");
        let _ = std::fs::remove_file(&path);
        {
            // Bind and drop: the file survives, the listener does not. That is a stale socket.
            let listener = UnixListener::bind(&path).expect("bind");
            drop(listener);
        }
        assert!(
            path.exists(),
            "the socket file must still be on disk for this test to mean anything"
        );
        assert!(
            !is_listening_at(&path),
            "a dropped listener must not read as available"
        );
        let described = describe_at(&path);
        assert!(
            described.contains("stale"),
            "must say stale, not available: {described}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_live_listener_is_reported_as_available() {
        // THE POSITIVE CONTROL. Without this, `is_listening_at` could return false unconditionally
        // and the stale-socket test above would still pass -- a check that cannot succeed is as
        // worthless as one that cannot fail.
        let path = temp_socket("live");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        assert!(
            is_listening_at(&path),
            "a bound listener must read as available"
        );
        assert!(
            describe_at(&path).contains("display available"),
            "{}",
            describe_at(&path)
        );
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_socket_is_reported_as_missing() {
        let path = temp_socket("absent");
        let _ = std::fs::remove_file(&path);
        assert!(!is_listening_at(&path));
        assert!(
            describe_at(&path).contains("does not exist"),
            "{}",
            describe_at(&path)
        );
    }

    #[test]
    fn a_shell_merely_mentioning_the_socket_is_not_a_viewer() {
        // The phantom this guards against. `pgrep -f <socket-path>` matches the shell RUNNING the
        // search, because the path is in that shell's own command line -- this project has already
        // produced a phantom "1 viewer" that way. Counting it would make `hide` signal an innocent
        // process, and would make `show` refuse because of a viewer that does not exist.
        let sock = "/run/user/1000/wvm/w11.spice.sock";
        let shell = format!("bash -c pgrep -f {sock}");
        assert!(
            !is_viewer_argv(&shell, sock),
            "a shell quoting the socket path must NOT count as a viewer"
        );
        // And the genuine article, so the function is not simply always false.
        let real = format!("remote-viewer spice+unix://{sock}");
        assert!(
            is_viewer_argv(&real, sock),
            "a real viewer must be recognised"
        );
        // A viewer for a DIFFERENT VM must not be counted for this one.
        let other = "remote-viewer spice+unix:///run/user/1000/wvm/other.spice.sock";
        assert!(
            !is_viewer_argv(other, sock),
            "another VM's viewer must not be counted"
        );
        // And nothing at all.
        assert!(!is_viewer_argv("", sock));
    }
}
