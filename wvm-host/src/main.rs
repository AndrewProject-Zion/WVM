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

mod doctor;
mod journal;
mod policy;
mod server;
mod supervisor;
mod vm;

use anyhow::Result;
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
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
        | VmAction::Log { config, .. } => config.clone(),
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
    }
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
