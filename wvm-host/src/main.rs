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

use anyhow::Result;
use clap::{Parser, Subcommand};

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

    /// Print the journal, oldest first.
    Journal {
        /// Maximum number of records to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
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
        Command::Journal { limit } => {
            let j = journal::Journal::open_default()?;
            for record in j.tail(limit)? {
                println!("{}", serde_json::to_string(&record)?);
            }
            Ok(())
        }
    }
}
