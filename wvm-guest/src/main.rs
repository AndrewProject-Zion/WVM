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

mod dispatch;
mod paths;
mod transfer;
mod transport;
mod win32;

#[cfg(windows)]
mod service;

use anyhow::Result;

fn main() -> Result<()> {
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
    serve_loop(&addr, move |port| {
        eprintln!("wvm-guest: listening on {port}");
    })
}

/// Bind, accept, and serve, until the process is asked to stop.
///
/// Shared by console mode and the Windows service, so there is one implementation of the actual
/// behaviour rather than two that drift. `on_ready` is called once the socket is bound, which is
/// where console mode prints and where a service would report its state.
pub fn serve_loop<F>(addr: &str, on_ready: F) -> Result<()>
where
    F: FnOnce(&str),
{
    let listener = transport::listen(addr)?;
    on_ready(addr);

    for stream in listener.incoming() {
        match stream {
            Ok(conn) => {
                if let Err(e) = dispatch::serve(conn) {
                    eprintln!("wvm-guest: connection ended: {e}");
                }
            }
            Err(e) => eprintln!("wvm-guest: accept failed: {e}"),
        }
    }

    Ok(())
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
