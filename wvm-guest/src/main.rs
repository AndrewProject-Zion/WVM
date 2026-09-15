//! WVM guest service.
//!
//! Runs inside the Windows guest and executes requests the host has already authorised. It
//! performs **no capability checks of its own** and must not grow any: authority is decided on
//! the host, and a second, weaker check here would only create a place for the two to disagree.
//!
//! The transport is deliberately abstract. [`transport::Connection`] is the seam where the
//! default TCP channel lives and where a `vsock` backend can be added later without touching the
//! dispatch logic (see `docs/DECISIONS.md` D-002).
//!
//! Compile status: the request/response plumbing and framing are complete and unit-tested. The
//! Win32 execution, capture, input and file-transfer implementations are stubbed with explicit
//! `NotImplemented` responses rather than optimistic ones — a stub that pretends to work is
//! worse than a stub that says it does not.

mod base64;
mod capture;
mod chunk;
mod dispatch;
mod fsio;
mod log;
mod paths;
mod transfer;
mod transport;
mod win32;

#[cfg(windows)]
mod service;

use anyhow::Result;

fn main() -> Result<()> {
    // Route the shared crate's framing diagnostics into this binary's log file.
    //
    // Without this, wvm-ipc's read loop has nowhere to report and stays silent — which is how a
    // large frame disappeared with no evidence at all.
    wvm_ipc::set_frame_debug(crate::log::probe);

    let args: Vec<String> = std::env::args().collect();

    // Service mode: hand over to the SCM dispatcher, which calls back into `service::service_main`
    // on its own thread. Without this the process is just a console program, and the SCM will
    // start it, wait for a status report that never comes, and mark it failed — which is exactly
    // what happened before this existed.
    #[cfg(windows)]
    if args.iter().any(|a| a == "--service") {
        return service::run();
    }

    // Console mode. Kept deliberately: running the binary by hand and reading its output is what
    // makes a service install diagnosable.
    let addr = parse_bind(&args).unwrap_or_else(transport::default_bind);
    serve_loop(
        &addr,
        move |port| {
            eprintln!("wvm-guest: listening on {port}");
        },
        // Console mode runs until interrupted. Ctrl+C terminates the process directly, so there is
        // nothing to poll for.
        || false,
    )
}

/// Bind, accept, and serve, until the process is asked to stop.
///
/// Shared by console mode and the Windows service, so there is one implementation of the actual
/// behaviour rather than two that drift. `on_ready` is called once the socket is bound, which is
/// where console mode prints and where a service would report its state.
///
/// # Stopping
///
/// The obvious loop — `for stream in listener.incoming()` — blocks indefinitely in `accept`, which
/// means a stop request is never noticed. That was not a theoretical problem: `sc.exe stop` would
/// report the service still RUNNING, the process would keep the binary open, and the next deploy
/// failed with "the process cannot access the file because it is being used by another process".
///
/// So the listener is non-blocking and the loop polls. `should_stop` is consulted between attempts,
/// which bounds how long a stop takes to roughly the poll interval rather than never.
pub fn serve_loop<F, S>(addr: &str, on_ready: F, should_stop: S) -> Result<()>
where
    F: FnOnce(&str),
    S: Fn() -> bool,
{
    let listener = transport::listen(addr)?;
    // Non-blocking so the loop can check for a stop between connection attempts.
    listener.set_nonblocking(true)?;
    on_ready(addr);

    /// How long to wait between polls.
    ///
    /// Short enough that a stop feels immediate to a human, long enough not to spin a core. 100ms
    /// costs nothing while idle and makes a stop take at most a tenth of a second longer than it
    /// otherwise would.
    const POLL: std::time::Duration = std::time::Duration::from_millis(100);

    loop {
        if should_stop() {
            return Ok(());
        }

        match listener.accept() {
            Ok(Some(conn)) => {
                if let Err(e) = dispatch::serve(conn) {
                    eprintln!("wvm-guest: connection ended: {e}");
                }
            }
            // Nothing pending: normal for a non-blocking listener.
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                eprintln!("wvm-guest: accept failed: {e}");
                std::thread::sleep(POLL);
            }
        }
    }
}

/// Parse `--bind <addr>` without pulling in a full argument parser: the guest binary should stay
/// as small as possible so it is cheap to deploy into a debloated image.
fn parse_bind(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--bind" {
            return it.next().cloned();
        }
    }
    None
}
