//! A per-request deadline, so no single request can hold the control channel forever.
//!
//! # The gap this closes
//!
//! `exec` takes a caller-supplied timeout, so a hung COMMAND is bounded. That is a real fix and it
//! was worth making (see the timeout work in `win32.rs`). It is also narrower than it looks: it
//! bounds one verb, not the channel.
//!
//! The guest serves one request at a time on the connection's thread. If a request never returns —
//! a verb with a bug, a path that blocks, an operation added later that nobody gave a timeout to —
//! then the connection is held for as long as that request decides, and the host waits. The host
//! cannot tell the difference between a guest that is busy, a guest that is wedged, and a guest
//! that is dead, because all three look like silence.
//!
//! That is the same envelope as the two availability bugs already fixed here: a single misbehaving
//! input taking out more than itself.
//!
//! # What this does and does not do
//!
//! It enforces a deadline at the REQUEST level: if `handle` has not returned in time, the loop
//! stops waiting for it and answers the host with a timeout response. The channel stays usable.
//!
//! It does NOT stop the abandoned work. The thread running `handle` keeps running until it finishes
//! or the process exits; there is no safe way to cancel arbitrary code in Rust. So this is a
//! liveness fix for the CHANNEL, not a resource fix for the guest. Both matter, and conflating them
//! would be the mistake this project keeps making — so the timeout response says plainly that the
//! work may still be running, rather than implying a clean cancellation that did not happen.
//!
//! # Why the timer is a thread rather than a select
//!
//! The serving loop is not async and the guest deliberately carries no async runtime: it is a small
//! Windows service whose whole job is to be predictable, and `tokio` is a large dependency to add
//! for one timeout. A channel with a `recv_timeout` gives the same semantics with nothing new.

use std::sync::mpsc;
use std::time::Duration;

/// How long a request may occupy the loop before the host is told it is taking too long.
///
/// Generous on purpose. This is a backstop for a WEDGED request, not a performance limit, and it
/// must sit well above the longest legitimate operation. `exec` carries its own caller-supplied
/// timeout (up to ten minutes) and a full 4 GiB transfer takes minutes, so the deadline has to
/// clear both or this would start severing work that is proceeding normally.
///
/// Fifteen minutes: longer than any bounded operation the protocol currently permits, short enough
/// that a wedged request does not hold a host agent indefinitely.
pub const DEFAULT_REQUEST_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// The deadline actually in force.
///
/// `WVM_REQUEST_DEADLINE_SECS` overrides it, and exists for one reason: a fifteen-minute backstop
/// cannot be tested by waiting fifteen minutes, and a mechanism nobody can exercise is a mechanism
/// nobody can trust. The override makes the failure path reachable in a test — set it to a couple of
/// seconds, send a request that blocks, and check that the channel answers again.
///
/// An override that is absent, unparseable, or zero falls back to the default rather than failing.
/// A malformed environment variable must not be able to remove the backstop, because the failure
/// mode of "no deadline at all" is exactly the wedge this exists to prevent.
pub fn request_deadline() -> Duration {
    match std::env::var("WVM_REQUEST_DEADLINE_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) | Err(_) => DEFAULT_REQUEST_DEADLINE,
            Ok(secs) => Duration::from_secs(secs),
        },
        Err(_) => DEFAULT_REQUEST_DEADLINE,
    }
}

/// Run `f` on a worker thread and wait at most `deadline` for it.
///
/// Returns `Ok(value)` if it finished in time, or `Err(elapsed)` with the time waited if it did not.
/// The worker is detached rather than joined on the timeout path: joining would block the loop for
/// exactly as long as the timeout was supposed to prevent, which would make the whole mechanism
/// pointless.
pub fn with_deadline<T, F>(deadline: Duration, f: F) -> Result<T, Duration>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // A send error means the receiver was dropped, which happens when the deadline fired first.
        // Nothing to do about it: the value is discarded along with the abandoned work.
        let _ = tx.send(f());
    });
    rx.recv_timeout(deadline).map_err(|_| deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_that_finishes_in_time_returns_its_value() {
        let got = with_deadline(Duration::from_secs(5), || 42).expect("finished in time");
        assert_eq!(got, 42);
    }

    #[test]
    fn a_request_that_overruns_is_abandoned_rather_than_waited_for() {
        // The point of the mechanism: the caller gets control back. If this blocked for the full
        // sleep, the deadline would be doing nothing.
        let start = std::time::Instant::now();
        let result = with_deadline(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            "too late"
        });
        let waited = start.elapsed();

        assert!(
            result.is_err(),
            "an overrunning request must not yield a value"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the deadline must return promptly, not after the work finishes; waited {waited:?}"
        );
    }

    #[test]
    fn a_panicking_request_does_not_wedge_the_loop() {
        // A panic on the worker drops the sender, so the receive fails immediately rather than
        // waiting out the full deadline. Treating that as a timeout is safe — the loop stays
        // responsive, which is the property under test.
        let result = with_deadline(Duration::from_secs(5), || -> i32 {
            panic!("a request panicked");
        });
        assert!(
            result.is_err(),
            "a panicking request must not leave the loop waiting"
        );
    }

    #[test]
    fn a_malformed_override_cannot_remove_the_backstop() {
        // The failure mode of "no deadline" is the wedge this exists to prevent, so a bad value has
        // to fall back rather than disable. Zero is the dangerous one: `recv_timeout(0)` fails
        // immediately, which would time out EVERY request instead of none.
        let key = "WVM_REQUEST_DEADLINE_SECS";
        let previous = std::env::var(key).ok();

        for bad in ["", "0", "not-a-number", "-5", "  "] {
            std::env::set_var(key, bad);
            assert_eq!(
                request_deadline(),
                DEFAULT_REQUEST_DEADLINE,
                "override {bad:?} must fall back to the default, not disable the deadline"
            );
        }

        // And a sane value is honoured, or the test above would pass with the override ignored.
        std::env::set_var(key, "2");
        assert_eq!(request_deadline(), Duration::from_secs(2));

        match previous {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn the_deadline_clears_every_bounded_operation_in_the_protocol() {
        // A backstop that fires during normal work is worse than no backstop: it would sever
        // legitimate transfers. The longest bounded operation is `exec` at its maximum, so the
        // request deadline has to sit above it.
        const EXEC_MAX: Duration = Duration::from_secs(10 * 60);
        assert!(
            DEFAULT_REQUEST_DEADLINE > EXEC_MAX,
            "the request deadline must exceed the longest per-verb timeout, or it would cut off \
             work that is proceeding correctly"
        );
    }
}
