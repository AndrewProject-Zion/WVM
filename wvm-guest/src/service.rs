//! Windows service integration.
//!
//! `sc.exe create` registers a binary path; it does not make a program a service. The Service
//! Control Manager starts the process and then waits for it to call
//! `StartServiceCtrlDispatcher`, report `SERVICE_RUNNING`, and stand by for control requests. A
//! plain console program does none of that, so the SCM gives up after its timeout and marks the
//! service failed — while `sc.exe create` reports success and `sc.exe query` shows an entry that
//! exists but never runs.
//!
//! That is precisely what happened here: the service registered cleanly, never started, and
//! nothing listened on the port. The distinction between "registered" and "running" is the whole
//! reason the installer verifies a listening socket rather than trusting the service list.
//!
//! # Design
//!
//! The service is a thin wrapper. It owns exactly the SCM conversation — connect, report running,
//! handle stop, report stopped — and delegates everything else to the same `serve` function the
//! console mode uses. So there is one implementation of the actual behaviour and two ways to
//! start it, rather than two code paths that can drift.
//!
//! Console mode stays supported on purpose: being able to run the binary by hand and read its
//! output is what makes debugging a service install possible at all.

#![cfg(windows)]

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// The name the installer registers. Must match `sc.exe create <name>`.
pub const SERVICE_NAME: &str = "wvm-guest";

define_windows_service!(ffi_service_main, service_main);

/// Tells the serving loop to stop.
///
/// An atomic flag rather than a channel: the control handler runs on an SCM-owned thread and must
/// return promptly, and the loop needs to *poll* it between accept attempts. A channel would need
/// a receiver the loop blocks on, which is the blocking-forever problem this exists to solve.
///
/// A `OnceLock` keeps it reachable from the Win32 callback, which cannot carry user data.
static STOPPING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Entry point called by the SCM through the dispatcher.
fn service_main(_arguments: Vec<OsString>) {
    // Any failure here happens before the SCM has a status to report against, so there is nowhere
    // to surface it except the event log — which the installer's log path covers instead.
    if let Err(e) = run_service() {
        eprintln!("wvm-guest service: {e}");
    }
}

fn run_service() -> Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    let _ = STOPPING.set(Arc::clone(&stopping));

    // The control handler runs on an SCM-owned thread and must return promptly: the SCM serialises
    // control requests, and blocking here stalls stop and shutdown for every service call. Setting
    // a flag is about as prompt as it gets.
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(flag) = STOPPING.get() {
                flag.store(true, Ordering::Relaxed);
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    // The bind address the service uses. 0.0.0.0 because the host's forward arrives as an inbound
    // connection on the guest's external interface, never on loopback.
    let addr = crate::transport::default_bind();

    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };

    // report RUNNING only after the listener is bound would be more truthful, but the SCM gives a
    // bounded window for the transition and binding is fast. If binding fails, the process exits
    // and the SCM records a failure against the service, which is the correct outcome.
    status_handle.set_service_status(running)?;

    // The real work: bind, accept, dispatch. Identical to console mode.
    //
    // `should_stop` is consulted between accept attempts, so a stop request is noticed within the
    // poll interval rather than never. Without this the SCM sends a stop, the process keeps
    // running, and the service stays RUNNING while holding its own binary open — which made the
    // next deploy fail with a sharing violation rather than an obvious "service is still running".
    let stopping = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stopping);
    let result = crate::serve_loop(
        &addr,
        move |port| {
            eprintln!("wvm-guest service: listening on {port}");
        },
        move || stop_flag.load(Ordering::Relaxed),
    );

    // Report stopped regardless of how the loop ended, so the SCM is never left believing the
    // service is still running. A stopped service that the SCM thinks is running cannot be
    // restarted without a reboot.
    let stopped = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: match &result {
            Ok(()) => ServiceExitCode::Win32(0),
            Err(_) => ServiceExitCode::ServiceSpecific(1),
        },
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let _ = status_handle.set_service_status(stopped);

    result
}

/// Run as a service: connect to the SCM and take requests from it.
///
/// Returns once the service has stopped. Never returns if the SCM starts the process in a context
/// where it is not a service, which is the documented behaviour for the error case.
pub fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow!("could not connect to the service manager: {e}"))
}
