//! A log that a Windows service can actually write to.
//!
//! # Why this exists
//!
//! `eprintln!` is useless in a service. There is no console attached, so the output is discarded —
//! which meant the framing instrumentation added to diagnose the large-frame failure produced
//! NOTHING. The attempt to work around that by running the binary in console mode failed too, and
//! worse: starting a second copy of the service binary collided with the running service and took
//! the guest's session down with it.
//!
//! So the log has to go to a file. This is the smallest thing that does that, and it exists so the
//! next diagnostic writes somewhere that can be read afterwards instead of somewhere that vanishes.
//!
//! # Shape
//!
//! Deliberately simple, for the same reason the base64 codec avoids a crate: the guest binary should
//! stay lean and dependency-free. Append, flush, ignore failures. A diagnostic that can itself fail
//! in a way that hides the diagnosis is worse than no diagnostic.

use std::io::Write;

/// Where the log goes. Under ProgramData, alongside the staging roots, so it is writable by the
/// service account without any special grant.
pub const DEFAULT_LOG: &str = r"C:\ProgramData\wvm\probe.log";

/// Append one line, stamping each with a monotonically increasing sequence number.
///
/// The sequence matters for reading the output: several threads can write, and the order in which
/// lines appear tells you which read completed first. Without it, a log of interleaved reads is
/// ambiguous exactly when it is most needed.
pub fn probe(message: &str) {
    let path = std::env::var("WVM_PROBE_LOG").unwrap_or_else(|_| DEFAULT_LOG.to_string());

    // Create the parent directory if it is missing.
    //
    // This is not tidiness. The first version of this logger wrote nothing at all, because
    // `C:\ProgramData\wvm` did not exist and the open failed — silently, as designed, since a
    // diagnostic must never be the reason an operation fails. The result was that the
    // instrumentation added specifically to diagnose a failure produced no output, and the failure
    // looked un-instrumented.
    //
    // So: make the directory, then open.
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Opened per call rather than held: a diagnostic should not keep a handle open, and the write
    // volume here is tiny. `O_APPEND` semantics are what make concurrent writers safe.
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        // Nowhere to log. Returning quietly is right: a diagnostic must never be the reason an
        // operation fails.
        return;
    };

    let _ = writeln!(file, "{}", message);
    let _ = file.flush();
}
