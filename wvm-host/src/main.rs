//! WVM host daemon.
//!
//! Three responsibilities, in order of importance:
//!
//! 1. **Decide what a caller may do** — capability grants, checked at request construction.
//! 2. **Record what happened** — an append-only journal of every request, decision and result,
//!    including denials.
//! 3. **Supervise QEMU** — lifecycle, snapshots, health.
//!
//! Nothing here trusts the guest. The guest executes what it is told because the host has
//! already decided the request was permissible; a compromised guest gains no authority it was
//! not already granted.

mod capabilities;
mod display;
mod doctor;
mod guestclient;
mod image;
mod input;
mod journal;
mod lifecycle;
mod policy;
mod pull;
mod push;
mod server;
mod supervisor;
mod vm;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::time::Duration;
use wvm_ipc::{RequestKind, Verb, PROTOCOL_VERSION};

/// Windows VM control plane for autonomous agents.
#[derive(Debug, Parser)]
#[command(name = "wvm", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print what this build can do, as text or JSON.
    ///
    /// The same list `WVM.md` is generated from, so an agent can ask the binary rather than trust a
    /// document that may have been written against a different version.
    Capabilities {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Emit the markdown that WVM.md is generated from.
        #[arg(long)]
        markdown: bool,
    },

    /// Check the host environment and report anything missing.
    ///
    /// Reproduces the checks recorded in docs/VERIFIED-ENVIRONMENT.md so that "works on my
    /// machine" claims are falsifiable on any machine.
    Doctor {
        /// Exit non-zero if any check fails.
        #[arg(long)]
        strict: bool,
    },

    /// Run the control socket and serve callers.
    Serve {
        /// Socket path. Defaults to $XDG_RUNTIME_DIR/wvm/control.sock.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,

        /// Verbs to grant the caller, comma-separated. Defaults to all of them, confined to
        /// roots under the state directory.
        #[arg(long)]
        verbs: Option<String>,
    },

    /// Send one request to a running control socket.
    Call {
        /// Socket path. Defaults to $XDG_RUNTIME_DIR/wvm/control.sock.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,

        #[command(subcommand)]
        request: CallRequest,
    },

    /// Print the journal, oldest first.
    Journal {
        /// Maximum number of records to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Manage a VM defined by a TOML file.
    ///
    /// The config path is a global argument, so it can be given either side of the action:
    /// `wvm vm --config x validate` and `wvm vm validate --config x` both work. Putting it only
    /// on the parent looks correct in the derive and is rejected by the parser, which is exactly
    /// the kind of thing only running the binary finds.
    Vm {
        #[command(subcommand)]
        action: VmAction,
    },
}

#[derive(Debug, Subcommand)]
enum VmAction {
    /// Check the definition is valid and report what would run.
    Validate {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,
    },

    /// Print the QEMU command line without running it.
    ///
    /// A command line you can read before executing is worth more than one assembled invisibly
    /// at spawn time.
    Cmdline {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,
    },

    /// Show whether the VM is running, and how that was determined.
    Status {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,
    },

    /// Create the state directory and disk image; make no other changes.
    Prepare {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,
    },

    /// Start the VM and wait until QEMU answers.
    Start {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        /// How long to wait for QEMU to become responsive.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },

    /// Suspend the guest, returning resources to the host.
    Suspend {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,
    },

    /// Ask the guest to shut down, waiting for it to go.
    Shutdown {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },

    /// Print the tail of the serial console — the first place to look when a VM will not boot.
    Log {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        #[arg(long, default_value_t = 40)]
        lines: usize,
    },

    /// Move a file between the host and the guest.
    ///
    /// Both paths must be inside the guest's staging roots. The guest checks that, not the host —
    /// which is the point of the staging design: the side that writes decides where writes may land.
    Transfer {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        /// `push` (host to guest) or `pull` (guest to host).
        #[arg(value_parser = ["push", "pull"])]
        action: String,

        /// The file to move.
        ///
        /// For `push` this is a host path. For `pull` it is a **guest** path — the file being
        /// fetched. The name is unhelpful for pull, which is why `--help` spells out both cases
        /// rather than relying on the argument name to carry the meaning.
        source: std::path::PathBuf,

        /// Where to put it.
        ///
        /// For `push` this is a **guest** path, inside the guest's staging root. For `pull` it is a
        /// **host** destination.
        guest_path: String,

        /// Replace an existing file at the destination.
        ///
        /// Off by default: a transfer that silently overwrites is a data-loss bug waiting for a
        /// caller that retried.
        #[arg(long)]
        overwrite: bool,
    },

    /// Snapshot, restore and list VM state.
    ///
    /// A snapshot writes the machine's RAM and device state INTO the disk's own qcow2 as an
    /// internal snapshot. It is not a disk-only revert point: restoring returns the guest to the
    /// running state it was in, which is what makes this useful before letting an agent do
    /// something it might need to undo.
    ///
    /// Requires the VM to be running — QEMU holds the state, so this is a QMP operation against a
    /// live hypervisor.
    /// Put the guest's desktop on screen, on request: `show`, `hide` or `status`.
    ///
    /// Separate from `start` on purpose. The display server always exists (D-021), so this only
    /// attaches or detaches a VIEWER -- a separate process, which means closing the window cannot
    /// take the VM down with it.
    Display {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        /// `show`, `hide` or `status`.
        #[arg(value_parser = ["show", "hide", "status"])]
        action: String,
    },

    Snapshot {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        /// `save`, `restore`, `list` or `delete`.
        #[arg(value_parser = ["save", "restore", "list", "delete"])]
        action: String,

        /// The snapshot name. Not used by `list`.
        tag: Option<String>,
    },

    /// Capture the guest's screen as a PNG.
    ///
    /// Runs against the QEMU framebuffer rather than inside the guest. The guest cannot do this at
    /// all: a Windows service runs in session 0, which has no desktop. See
    /// `supervisor::Qmp::screendump`, and `docs/DECISIONS.md` D-009.
    Capture {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        /// Where to write the PNG. `-` writes to stdout.
        #[arg(long, short)]
        out: String,

        /// Reject a capture whose dimensions do not match this, as `WIDTHxHEIGHT`.
        ///
        /// A screendump can succeed and be the wrong thing — a stale framebuffer, a resized
        /// display, a VM that has not finished booting. Asserting the expected geometry turns
        /// "a file appeared" into a check that means something.
        #[arg(long)]
        expect: Option<String>,
    },

    /// Send input to the guest: keys, text, or a click at a coordinate.
    ///
    /// Delivered as emulated HARDWARE through QEMU, not through Windows input APIs. That matters:
    /// a Windows service runs in session 0 and `SendInput` from there cannot reach the interactive
    /// session, but a device event is delivered by the kernel to whichever session owns the active
    /// console. See `docs/DECISIONS.md` D-010.
    Input {
        #[arg(long, default_value = "wvm.toml", global = true)]
        config: std::path::PathBuf,

        #[command(subcommand)]
        action: InputAction,
    },
}

/// What to send to the guest.
#[derive(Debug, Subcommand)]
enum InputAction {
    /// Type a literal string.
    ///
    /// Each character is translated to the key events that produce it on a UK layout, which is the
    /// guest's layout. A map written for a US keyboard sends `@` where `"` was intended — the bug
    /// that stalled M4 for most of a session.
    Text {
        /// The string to type.
        value: String,
    },

    /// Press a named key or a `+`-separated chord, e.g. `ret`, `esc`, `ctrl+c`.
    Key {
        /// QOM key names, joined with `+` for a chord.
        spec: String,
    },

    /// Move the pointer to an absolute coordinate and optionally click.
    ///
    /// Requires the VM to have a `usb-tablet`, which the generated command line includes. Without
    /// it the guest sees only a relative mouse and an absolute move is not expressible.
    Click {
        x: i32,
        y: i32,

        /// Which button.
        #[arg(long, default_value = "left")]
        button: String,

        /// Move without clicking, to see where the pointer lands first.
        #[arg(long)]
        no_click: bool,
    },
}

/// The requests `wvm call` can send. Raw JSON is available via the client library for anything
/// not covered here.
#[derive(Debug, Subcommand)]
enum CallRequest {
    /// Handshake and report what is on the other end.
    Hello,
    /// Ask for guest inventory.
    Inspect,
}

/// Is the guest's service answering? A one-shot hello over the control channel, nothing more.
///
/// WHY THIS EXISTS — a snapshot can crash the guest while reporting success (D-018). The snapshot
/// genuinely did succeed; it is the machine that is now blue-screening. A caller who reads "saved"
/// has no reason to look any further, so the worst possible outcome is the one the tool currently
/// produces: a confident success message over a dead guest.
///
/// The comparison is what makes it meaningful. A guest that was already silent — stopped for
/// maintenance, service not running — is not evidence about this save, so the caller asks twice and
/// only reports a change.
fn guest_answering(addr: &str) -> bool {
    matches!(
        guestclient::request(
            addr,
            &wvm_ipc::Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: "wvm-liveness".to_string(),
            }
        ),
        Ok(wvm_ipc::Response::Ready { .. })
    )
}

/// Wait for the guest to answer, up to `deadline`.
///
/// A save freezes the vCPUs while state is written, so the guest does NOT answer immediately
/// afterwards — measured at 57 seconds on a disk carrying snapshots. Reporting a crash on the first
/// silent probe would turn every large save into a false alarm, which is worse than no check.
fn wait_for_guest(addr: &str, deadline: std::time::Duration) -> std::time::Duration {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if guest_answering(addr) {
            return start.elapsed();
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    start.elapsed()
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Capabilities { json, markdown } => {
            if markdown {
                // Straight to stdout with no trailing banner, so `> WVM.md` produces exactly the
                // file `scripts/check-docs.sh` regenerates and compares.
                print!("{}", capabilities::render_markdown());
            } else if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&capabilities::CAPABILITIES)?
                );
            } else {
                for c in capabilities::CAPABILITIES {
                    println!("{:10} {:6}  {}", c.op, c.runs_on, c.summary);
                }
            }
            Ok(())
        }
        Command::Doctor { strict } => {
            let report = doctor::run();
            print!("{}", report.render());
            if strict && !report.all_passed() {
                std::process::exit(1);
            }
            Ok(())
        }

        Command::Serve { socket, verbs } => {
            let mut config = server::ServerConfig::development();
            if let Some(path) = socket {
                config.socket_path = path;
            }
            // Narrowing the grant from the command line is how an operator gives a caller less
            // than everything without editing code.
            if let Some(list) = verbs {
                config.grant.verbs = list.split(',').filter_map(parse_verb).collect();
                eprintln!(
                    "wvm: granting {} verb(s) to {}",
                    config.grant.verbs.len(),
                    config.grant.subject
                );
            }

            let server = server::Server::bind(config)?;
            eprintln!("wvm: listening on {}", server.socket_path().display());
            server.run()
        }

        Command::Call { socket, request } => {
            let path = socket.unwrap_or_else(server::default_socket_path);
            let mut client = server::client::Client::connect(&path)?;

            match request {
                CallRequest::Hello => {
                    let guest = client.hello("wvm-cli")?;
                    println!("protocol {PROTOCOL_VERSION}; peer: {guest}");
                }
                CallRequest::Inspect => {
                    // Handshake first: a server that has moved to a different protocol version
                    // should say so before it is asked to interpret anything.
                    client.hello("wvm-cli")?;

                    // Inspect needs no more than the Inspect verb; the grant may still refuse it.
                    let grant = wvm_ipc::Grant {
                        subject: "wvm-cli".into(),
                        verbs: vec![Verb::Inspect],
                        read_roots: Vec::new(),
                        write_roots: Vec::new(),
                        guest_root: String::new(),
                    };
                    let request = wvm_ipc::Request::new(&grant, RequestKind::Inspect)?;
                    let response = client.send(&request)?;
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
            }
            Ok(())
        }

        Command::Journal { limit } => {
            let j = journal::Journal::open_default()?;
            for record in j.tail(limit)? {
                println!("{}", serde_json::to_string(&record)?);
            }
            Ok(())
        }

        Command::Vm { action } => run_vm(action),
    }
}

/// VM subcommand dispatch.
///
/// Each action carries its own `config` argument. The duplication is deliberate: a single field
/// on the parent is rejected by the parser when the flag is given after the action, which is the
/// natural place to type it.
fn run_vm(action: VmAction) -> Result<()> {
    // Pull the config path out first, then act. This keeps the per-action handling below from
    // repeating the load-and-validate preamble.
    let config_path = match &action {
        VmAction::Validate { config }
        | VmAction::Cmdline { config }
        | VmAction::Status { config }
        | VmAction::Prepare { config }
        | VmAction::Start { config, .. }
        | VmAction::Suspend { config }
        | VmAction::Shutdown { config, .. }
        | VmAction::Log { config, .. }
        | VmAction::Transfer { config, .. }
        | VmAction::Capture { config, .. }
        | VmAction::Display { config, .. }
        | VmAction::Snapshot { config, .. }
        | VmAction::Input { config, .. } => config.clone(),
    };

    // Validate before constructing the supervisor: a bad definition should be reported as a
    // config problem, not as a mysterious launch failure.
    let config = vm::VmConfig::load(&config_path)?;
    let supervisor = supervisor::Supervisor::new(config);

    match action {
        VmAction::Validate { .. } => {
            println!("{} is valid", config_path.display());
            let c = supervisor.config();
            println!("  name        {}", c.name);
            println!("  disk        {} ({} GiB)", c.disk.display(), c.disk_gib);
            println!(
                "  disk exists {}",
                if c.disk.exists() {
                    "yes"
                } else {
                    "no — `wvm vm prepare` will create it"
                }
            );
            println!("  memory      {} MiB", c.memory_mib);
            println!("  cpus        {}", c.cpus);
            println!("  firmware    {:?}", c.firmware);
            println!(
                "  install iso {}",
                c.install_iso
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".into())
            );
            println!(
                "  driver iso  {}",
                c.driver_iso
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(
                        || "none — Windows will not see a virtio disk without it".into()
                    )
            );
            println!("  state dir   {}", c.state_dir().display());
            println!(
                "  forwards    127.0.0.1:{} -> guest:{}",
                c.forward_port, c.guest_port
            );
            Ok(())
        }

        VmAction::Cmdline { .. } => {
            println!("{}", supervisor.config().qemu_cmdline());
            Ok(())
        }

        VmAction::Status { .. } => {
            let state = supervisor.state();
            println!("{}", state.label());
            // Say how the answer was reached, so a surprising result is diagnosable.
            let c = supervisor.config();
            println!(
                "  pid file    {} ({})",
                c.pid_file().display(),
                if c.pid_file().exists() {
                    "present"
                } else {
                    "absent"
                }
            );
            println!(
                "  qmp socket  {} ({})",
                c.qmp_socket().display(),
                if c.qmp_socket().exists() {
                    "present"
                } else {
                    "absent"
                }
            );
            Ok(())
        }

        VmAction::Prepare { .. } => {
            supervisor.prepare()?;
            println!("prepared {}", supervisor.config().state_dir().display());
            Ok(())
        }

        VmAction::Start { timeout, .. } => {
            let pid = supervisor.start()?;
            println!("qemu started, pid {pid}");

            // Do not report success merely because a process exists. QEMU needs time to
            // initialise KVM and firmware, and "started" followed by "nothing happened" is the
            // least trustworthy thing a supervisor can do.
            let state = supervisor.wait_until_responsive(Duration::from_secs(timeout))?;
            match &state {
                supervisor::VmState::Running { status } => {
                    println!("responsive: {status}");
                    Ok(())
                }
                other => {
                    // Name the next diagnostic step rather than just failing.
                    eprintln!(
                        "warning: QEMU is {}. Check the serial log with `wvm vm log` and {}",
                        other.label(),
                        supervisor.config().monitor_log().display()
                    );
                    anyhow::bail!("the VM did not become responsive within {timeout}s")
                }
            }
        }

        VmAction::Suspend { .. } => {
            supervisor.suspend()?;
            println!("suspended");
            Ok(())
        }

        VmAction::Shutdown { timeout, .. } => {
            supervisor.shutdown(Duration::from_secs(timeout))?;
            println!("shut down");
            Ok(())
        }

        VmAction::Log { lines, .. } => {
            println!("{}", supervisor.serial_tail(lines)?);
            Ok(())
        }

        VmAction::Transfer {
            action,
            source,
            guest_path,
            overwrite,
            ..
        } => {
            let config = supervisor.config();

            if !supervisor.state().is_running() {
                anyhow::bail!(
                    "the VM is not running, so there is nothing to transfer to. \
                     Start it with `wvm vm start`"
                );
            }

            let addr = format!("127.0.0.1:{}", config.forward_port);

            match action.as_str() {
                "push" => {
                    // Lockstep: read a chunk, send it, wait for the acknowledgement, repeat. See
                    // `push.rs` for why the host must not stream ahead of the guest's disk.
                    let started = std::time::Instant::now();
                    let pushed = push::push(&addr, &source, &guest_path, overwrite)?;
                    let elapsed = started.elapsed();

                    println!(
                        "pushed {} bytes in {} chunk(s) to {}",
                        pushed.bytes, pushed.chunks, pushed.guest_path
                    );
                    println!(
                        "  {:.1} ms total, {:.0} KiB/s effective",
                        elapsed.as_secs_f64() * 1000.0,
                        (pushed.bytes as f64 / 1024.0) / elapsed.as_secs_f64().max(0.001)
                    );
                    if !overwrite {
                        println!("  (an existing file at the destination would have been refused)");
                    }
                    Ok(())
                }
                "pull" => {
                    // Lockstep in the other direction: ask for a chunk, write it, ask again. The
                    // guest is the reader here, which is why this is its own module rather than the
                    // push loop with the arguments swapped — see `pull.rs`.
                    //
                    // NOTE the argument order. `source` and `guest_path` are named for a push, and
                    // for a pull their meaning inverts: `source` is the GUEST file being fetched and
                    // `guest_path` is the HOST destination. Getting this backwards sends the local
                    // path to the guest, which refuses it as outside its staging root — a confusing
                    // error that looks like a boundary problem rather than a swapped argument.
                    let guest_source = source.to_string_lossy().to_string();
                    let host_destination = std::path::PathBuf::from(&guest_path);

                    let started = std::time::Instant::now();
                    let pulled = pull::pull(&addr, &guest_source, &host_destination, overwrite)?;
                    let elapsed = started.elapsed();

                    println!(
                        "pulled {} bytes in {} chunk(s) from {}",
                        pulled.bytes, pulled.chunks, guest_source
                    );
                    println!("  written to {}", pulled.host_path);
                    println!(
                        "  {:.1} ms total, {:.0} KiB/s effective",
                        elapsed.as_secs_f64() * 1000.0,
                        (pulled.bytes as f64 / 1024.0) / elapsed.as_secs_f64().max(0.001)
                    );
                    Ok(())
                }
                other => anyhow::bail!("unknown transfer action '{other}' (expected push or pull)"),
            }
        }

        VmAction::Display { action, .. } => {
            let config = supervisor.config();
            match action.as_str() {
                "show" => {
                    let pid = display::show(&config)?;
                    println!("  viewer started (pid {pid})");
                    println!("  {}", display::describe(&config));
                    println!();
                    println!("  Close that window whenever you like. It is a separate process, so");
                    println!(
                        "  closing it does NOT stop the VM -- which is the difference between"
                    );
                    println!(
                        "  this and the GTK display scripts/start-windows.sh uses by default."
                    );
                    Ok(())
                }
                "hide" => {
                    match display::hide(&config)? {
                        0 => println!("  no viewer attached; nothing to close"),
                        n => println!("  closed {n} viewer(s). The VM is untouched."),
                    }
                    Ok(())
                }
                "status" => {
                    println!("  {}", display::describe(&config));
                    Ok(())
                }
                other => anyhow::bail!("unknown display action '{other}'"),
            }
        }

        VmAction::Snapshot { action, tag, .. } => {
            let config = supervisor.config();

            // Snapshots need a live hypervisor: the machine state being saved lives in QEMU, so a
            // stopped VM has nothing to snapshot. Say so plainly rather than connecting and
            // producing a confusing socket error.
            if !supervisor.state().is_running() {
                anyhow::bail!(
                    "the VM is not running, so there is no machine state to work with. \
                     A snapshot captures RAM as well as disk, and RAM only exists while the guest \
                     is up. Start it with `wvm vm start`"
                );
            }

            let mut qmp = supervisor::Qmp::connect(&config.qmp_socket())?;

            match action.as_str() {
                "list" => {
                    let snapshots = lifecycle::list(&mut qmp)?;
                    if snapshots.is_empty() {
                        println!("no snapshots on this disk");
                    } else {
                        println!("snapshots on this disk:");
                        for s in snapshots {
                            println!(
                                "  {:<20} {:>10}   (id {})",
                                s.tag,
                                human_size(s.vm_size_bytes),
                                s.id
                            );
                        }
                    }
                }
                "save" => {
                    let tag = tag
                        .as_deref()
                        .context("a snapshot needs a name: `wvm vm snapshot save <tag>`")?;
                    println!(
                        "saving the machine state as '{tag}' — the guest FREEZES while this \
writes, and it can take a minute or more on a disk carrying snapshots. That freeze is expected; \
a timeout here does not mean the save failed."
                    );
                    let addr = format!("127.0.0.1:{}", config.forward_port);
                    let was_answering = guest_answering(&addr);

                    // Warn about an accumulating disk BEFORE the save, while the caller can still
                    // act on it. Two measured reasons: the guest freeze grows with the number of
                    // snapshots (6.5 seconds on a fresh disk, 57 seconds with nine), and every
                    // crash observed so far has been on a disk carrying many. Neither is guessed at
                    // — both are in D-017 and D-018.
                    if let Ok(existing) = lifecycle::list(&mut qmp) {
                        if existing.len() > 2 {
                            println!(
                                concat!(
                                    "  note: this disk already holds {} snapshots. Saves get slower ",
                                    "as they accumulate — the guest froze for 57s with nine, against ",
                                    "6.5s on a fresh disk — and that is also the state in which ",
                                    "crashes have been observed. `wvm vm snapshot delete <tag>` ",
                                    "reclaims one."
                                ),
                                existing.len()
                            );
                        }
                    }

                    let s = lifecycle::save(&mut qmp, tag)?;
                    println!(
                        "  saved '{tag}' — {} of machine state (RAM and devices)",
                        human_size(s.vm_size_bytes)
                    );

                    // Do not stop at "the snapshot succeeded". Ask the machine whether it is still
                    // there, because a save has been observed to bugcheck the guest while the
                    // snapshot itself completes perfectly (D-018).
                    if was_answering {
                        let waited = wait_for_guest(&addr, std::time::Duration::from_secs(240));
                        if waited >= std::time::Duration::from_secs(240) {
                            anyhow::bail!(concat!(
                                "the snapshot was written, BUT THE GUEST HAS NOT ANSWERED IN ",
                                "240 SECONDS. It may have bugchecked — this is the failure ",
                                "recorded in D-018, and an ordinary freeze does not look like ",
                                "this. Check C:\\Windows\\Minidump inside the guest before ",
                                "trusting the machine."
                            ));
                        }
                        if waited > std::time::Duration::from_secs(20) {
                            println!(
                                "  guest answering again after {:.0}s — that was the freeze ending",
                                waited.as_secs_f32()
                            );
                        }
                    }
                }
                "restore" => {
                    let tag = tag
                        .as_deref()
                        .context("name the snapshot to restore: `wvm vm snapshot restore <tag>`")?;
                    let addr = format!("127.0.0.1:{}", config.forward_port);
                    let was_answering = guest_answering(&addr);

                    println!("restoring '{tag}' — the guest will roll back to that moment");
                    lifecycle::restore(&mut qmp, tag)?;
                    println!("  restored '{tag}'");

                    // This used to ASSERT "the guest is still running". It was observed printing
                    // that sentence at a machine which was blue-screening at the time (D-018).
                    // Asserting a fact the code has not checked is how a caller ends up trusting a
                    // dead guest, so the line now reports a measurement instead.
                    if !was_answering {
                        println!(
                            "  the guest was not answering before the restore, so no claim is made \
                             about it now — anything written since the snapshot is still gone"
                        );
                    } else {
                        let waited = wait_for_guest(&addr, std::time::Duration::from_secs(240));
                        if waited >= std::time::Duration::from_secs(240) {
                            anyhow::bail!(concat!(
                                "the snapshot loaded, BUT THE GUEST HAS NOT ANSWERED IN 240 ",
                                "SECONDS. It may have bugchecked — see D-018. Check ",
                                "C:\\Windows\\Minidump inside the guest before trusting the ",
                                "machine."
                            ));
                        }
                        println!(
                            "  guest is running again ({:.0}s) and anything written since the \
                             snapshot is gone",
                            waited.as_secs_f32()
                        );
                    }
                }
                "delete" => {
                    let tag = tag
                        .as_deref()
                        .context("name the snapshot to delete: `wvm vm snapshot delete <tag>`")?;
                    lifecycle::delete(&mut qmp, tag)?;
                    println!("  deleted '{tag}'");
                }
                other => anyhow::bail!("unknown snapshot action '{other}'"),
            }

            Ok(())
        }

        VmAction::Capture { out, expect, .. } => {
            let config = supervisor.config();

            // The VM must be running: a capture from a stopped VM would either fail or, worse,
            // return a stale framebuffer left over from the last time it ran.
            if !supervisor.state().is_running() {
                anyhow::bail!(
                    "the VM is not running, so there is nothing to capture. \
                     Start it with `wvm vm start`"
                );
            }

            // QMP writes to a path, so go through a temporary file and read it back.
            let scratch =
                std::env::temp_dir().join(format!("wvm-capture-{}.ppm", std::process::id()));

            let mut qmp = supervisor::Qmp::connect(&config.qmp_socket())?;
            qmp.screendump(&scratch, None)?;

            let image = image::read_ppm(&scratch)?;
            // Clean up before the size check, so a rejected capture does not leave the scratch file
            // behind to be found later and mistaken for a real result.
            let _ = std::fs::remove_file(&scratch);

            if let Some(spec) = &expect {
                let (w, h) = parse_geometry(spec)?;
                if image.width != w || image.height != h {
                    anyhow::bail!(
                        "the capture is {}x{} but {spec} was expected. \n\
                         A screendump can succeed and still be the wrong thing: a stale \
                         framebuffer, a resized display, or a guest that has not finished booting.",
                        image.width,
                        image.height
                    );
                }
            }

            let png = image::encode_png(&image)?;

            if out == "-" {
                use std::io::Write;
                std::io::stdout().write_all(&png)?;
            } else {
                std::fs::write(&out, &png)?;
                eprintln!(
                    "captured {}x{} -> {out} ({} bytes)",
                    image.width,
                    image.height,
                    png.len()
                );
            }

            Ok(())
        }

        VmAction::Input { action, .. } => {
            let config = supervisor.config();

            if !supervisor.state().is_running() {
                anyhow::bail!(
                    "the VM is not running, so there is nothing to send input to. \
                     Start it with `wvm vm start`"
                );
            }

            let mut qmp = supervisor::Qmp::connect(&config.qmp_socket())?;

            match action {
                InputAction::Text { value } => {
                    let events = input::translate(&value)?;
                    for event in &events {
                        qmp.send_key_event(event)?;
                    }
                    println!("typed {} character(s)", value.chars().count());
                }

                InputAction::Key { spec } => {
                    let keys = input::parse_chord(&spec)?;
                    qmp.send_chord(&keys)?;
                    println!("sent {spec}");
                }

                InputAction::Click {
                    x,
                    y,
                    button,
                    no_click,
                } => {
                    qmp.send_pointer_move(x, y)?;
                    if no_click {
                        println!("moved the pointer to ({x}, {y})");
                    } else {
                        qmp.send_pointer_button(&button, true)?;
                        qmp.send_pointer_button(&button, false)?;
                        println!("clicked {button} at ({x}, {y})");
                    }
                }
            }

            Ok(())
        }
    }
}

/// Parse a `WIDTHxHEIGHT` geometry string.
/// Bytes as a human would write them, for sizes the caller is judging at a glance.
///
/// A snapshot size is the number a caller uses to decide whether it is worth keeping, so "3.37 GiB"
/// is more useful than "3618814771". One decimal place: more precision implies a certainty the
/// measurement does not have, since QEMU reports it rounded already.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1024 * 1024 * 1024 * 1024),
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.2} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

fn parse_geometry(spec: &str) -> Result<(u32, u32)> {
    let (w, h) = spec
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("expected WIDTHxHEIGHT, got {spec:?}"))?;
    Ok((
        w.trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("bad width in {spec:?}: {e}"))?,
        h.trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("bad height in {spec:?}: {e}"))?,
    ))
}

/// Parse a verb name. Unknown names are reported rather than silently dropped: a grant that is
/// quietly narrower than the operator intended is a support ticket waiting to happen.
fn parse_verb(name: &str) -> Option<Verb> {
    let trimmed = name.trim();
    let verb = match trimmed {
        "inspect" => Verb::Inspect,
        "exec" => Verb::Exec,
        "capture" => Verb::Capture,
        "input" => Verb::Input,
        "transfer" => Verb::Transfer,
        "lifecycle" => Verb::Lifecycle,
        other => {
            eprintln!("wvm: unknown verb '{other}' — ignoring");
            return None;
        }
    };
    Some(verb)
}
