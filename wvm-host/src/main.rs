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

use anyhow::Result;
use clap::{Parser, Subcommand};
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
