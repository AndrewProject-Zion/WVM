//! The machine-readable capability list, and the markdown generated from it.
//!
//! WHY THIS EXISTS
//!
//! A tool for agents needs a contract an agent can load and act on, and that contract must not be
//! able to lie. Written by hand in a markdown file it drifts the first time a verb changes — the
//! same way this project's README claimed `lifecycle` was a stub for a day after it was implemented.
//!
//! So the list below is the single source and `WVM.md` is GENERATED from it. `scripts/check-docs.sh`
//! regenerates and compares, which means a verb that changes without the doc changing fails the
//! build rather than quietly misleading whoever reads it next.
//!
//! Adding a verb means adding it here. That is the point: one edit, and the doc, the `--json`
//! output and the check all follow.

use serde::Serialize;

/// One verb, as an agent needs to know it.
#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    /// The protocol operation name, as it appears on the wire.
    pub op: &'static str,
    /// The host CLI invocation, if there is one.
    pub cli: &'static str,
    /// Where it runs. `host` means the guest refuses it — see `note`.
    pub runs_on: &'static str,
    /// A one-line description of what it does.
    pub summary: &'static str,
    /// The traps. These are the whole value of the file: each one cost a real debugging session.
    pub traps: &'static [&'static str],
}

/// Every verb in the protocol, in the order a caller would meet them.
pub const CAPABILITIES: &[Capability] = &[
    Capability {
        op: "hello",
        cli: "—",
        runs_on: "guest",
        summary: "Handshake. Reports the guest's protocol version and OS.",
        traps: &[
            "Do this first. It is how you find out whether a guest is there at all, rather than \
             inferring it from a connection that succeeds.",
        ],
    },
    Capability {
        op: "exec",
        cli: "wvm vm exec --config <cfg> \"<command>\"",
        runs_on: "guest",
        summary: "Run a program in the guest and return its stdout, stderr and exit code.",
        traps: &[
            "`&&` does not survive this layer. `cd /d X && cmd` exits 1 with EMPTY stdout and no \
             error — the command simply does not run. Run one thing at a time.",
            "`&&` is one of several characters that break quoting. If a command returns nothing \
             where output was expected, suspect the quoting before the program.",
            "The guest runs every command with stdin redirected to null, and `timeout.exe` refuses \
             that — it prints 'Input redirection is not supported' and exits in MILLISECONDS. A \
             test that uses it as a delay will count zero survivors for the absence of anything to \
             count. Use `ping -n <secs> 127.0.0.1`.",
            "The default working directory is C:\\Windows\\System32, so a relative filename lands \
             somewhere you did not intend.",
            "The service runs as `nt authority\\system`, so anything launched is already SYSTEM-\
             level. Elevation is not a problem here, and `setx /M` needs no help.",
            "A timeout kills the whole process TREE, not just the direct child, via a Windows Job \
             Object. A caller can rely on that.",
        ],
    },
    Capability {
        op: "capture",
        cli: "wvm vm capture --config <cfg> --out shot.png",
        runs_on: "host",
        summary: "Screenshot the live desktop.",
        traps: &[
            "This runs on the HOST over QMP, not in the guest. A Windows service runs in session 0 \
             which has no desktop, so BitBlt from inside can never work — that is architectural, \
             not a gap.",
            "It captures the whole desktop, not one window. There is no per-window targeting.",
        ],
    },
    Capability {
        op: "input",
        cli: "wvm vm input --config <cfg> ...",
        runs_on: "host",
        summary: "Move the pointer, click, type text and send key chords.",
        traps: &[
            "Also host-side, for the same reason: session-0 isolation blocks the SendInput API but \
             NOT emulated hardware, so the host injects through QMP and the kernel delivers it to \
             the interactive session.",
            "Typing is paced deliberately. Sending keys too fast makes the guest REORDER them, and \
             the symptom is not dropped characters but stray characters prefixed to the line — \
             which looks like a line-length limit and sends you looking in the wrong place.",
            "Control characters and chords need explicit down/up events. A press-and-release pair \
             arrives as the modifier released before the key, so Ctrl+R opens the Start menu \
             instead of Run.",
            "Driving an elevation (UAC) prompt this way is unreliable. If a human is at the screen, \
             ask them — it takes a second and cannot silently half-succeed.",
        ],
    },
    Capability {
        op: "transfer",
        cli: "wvm vm transfer push|pull --config <cfg> <SOURCE> <GUEST_PATH>",
        runs_on: "guest",
        summary: "Move a file in either direction, in chunks confined to a guest staging root.",
        traps: &[
            "`pull` INVERTS the positional names. They are written for a push, so for a pull SOURCE \
             is the GUEST file being fetched and GUEST_PATH is the HOST destination. Swap them and \
             the guest refuses a path it cannot see — an error that reads like a boundary problem \
             rather than a swapped argument.",
            "Do NOT quote a path passed to a guest command. The quote characters reach the process \
             as part of the argument, and `certutil -hashfile \"C:\\path\"` fails with \
             FILE_NOT_FOUND — the file is there; the quotes are the problem.",
            "Everything lands under one staging root (C:\\ProgramData\\wvm\\staging by default). \
             Paths outside it are refused, and that refusal is a feature.",
            "Chunks travel in lockstep with an offset checked on every one. Do not try to stream \
             them; a gap or an overlap would otherwise produce a right-length, wrong-content file.",
        ],
    },
    Capability {
        op: "lifecycle",
        cli: "wvm vm snapshot save|restore|list|delete --config <cfg> <tag>",
        runs_on: "host",
        summary: "Snapshot the machine, restore it, list snapshots, delete one.",
        traps: &[
            "A SAVE CAN CRASH THE GUEST on this Windows 11 build — roughly one save in five ends in \
             a 0x50 PAGE_FAULT_IN_NONPAGED_AREA bugcheck. The faulting instruction is in Windows' \
             own kernel, at one fixed code offset across four crashes, and pausing the vCPUs for \
             the same length of time does NOT reproduce it, so the trigger is the device-state \
             write rather than the freeze. Reproduced, localised, NOT fixed. Do not snapshot \
             anything you cannot afford to lose, and do not put a snapshot in the path of an \
             unattended agent run.",
            "The guest REFUSES this verb, permanently. Snapshots are a hypervisor capability and \
             the guest has no view of its own hypervisor. The refusal names the command that works \
             — it is not a stub awaiting an upgrade.",
            "A save FREEZES the guest CPUs while the machine state is written, and NO QMP message \
             arrives for the whole of that write. Measured here at 57 seconds with nine snapshots \
             on the disk, 6.5 seconds when it was fresh — the pause GROWS as snapshots accumulate. \
             A client read timeout shorter than the pause reports failure for work that is still \
             succeeding, and makes a frozen guest look like a dead one.",
            "A snapshot holds the machine's MEMORY as well as its disk, so it is roughly the RAM \
             size rather than a delta. That is what makes a restore a rollback instead of a disk \
             revert — and why snapshots are worth deleting.",
            "Restoring while the guest is running is supported and it survives. It is still a \
             rollback: anything written since the snapshot is gone, including files the agent \
             created.",
        ],
    },
];

/// Render `WVM.md` from the list above.
///
/// Deliberately not a template file. A template is a second thing to keep in step; generating from
/// the same data the `--json` output uses means there is one source and no way for the two to
/// disagree.
pub fn render_markdown() -> String {
    let mut out = String::new();

    out.push_str(
        "<!-- GENERATED FILE — do not edit by hand.\n\
         \n\
         \x20  Source:    wvm-host/src/capabilities.rs\n\
         \x20  Regenerate: wvm capabilities --markdown > WVM.md\n\
         \x20  Checked by: scripts/check-docs.sh (a change without a regenerate FAILS the build)\n\
         \n\
         \x20  Editing this file directly will be overwritten. Add or change a verb in the source\n\
         \x20  list instead, and the markdown, the --json output and the check all follow.\n\
         -->\n\n",
    );

    out.push_str("# WVM for agents\n\n");
    out.push_str(
        "You are driving a Windows 11 machine from a Linux terminal. This file is the contract: the\n\
         verbs, the exact responses, and the traps that cost real time to find. It is generated from\n\
         the code, so it cannot describe a verb that does not exist.\n\n",
    );

    out.push_str("## The one command to run first\n\n");
    out.push_str(
        "Before anything else, find out what the sandbox will and will not do. The refusals are as\n\
         important as the capabilities, and they arrive as structured data rather than a timeout:\n\n\
         ```sh\n\
         python3 examples/wvm_client.py boundary\n\
         ```\n\n\
         A refusal is the boundary working. It names what it refused and why, so you can route\n\
         around it deliberately instead of discovering the limit as a failure halfway through a job.\n\n",
    );

    out.push_str("## Verbs\n\n");
    for c in CAPABILITIES {
        out.push_str(&format!("### `{}`\n\n", c.op));
        out.push_str(&format!("{}\n\n", c.summary));
        out.push_str(&format!("- **Runs on:** {}\n", c.runs_on));
        if c.cli != "—" {
            out.push_str(&format!("- **CLI:** `{}`\n", c.cli));
        }
        out.push('\n');
        if !c.traps.is_empty() {
            out.push_str("Traps:\n\n");
            for t in c.traps {
                out.push_str(&format!("- {t}\n"));
            }
            out.push('\n');
        }
    }

    out.push_str("## What the guest will refuse, and why that is the answer\n\n");
    out.push_str(
        "`lifecycle` is refused from inside the guest, permanently. Snapshots are a hypervisor\n\
         capability and the guest has no view of its own hypervisor, so this can never work from that\n\
         side.\n\n\
         The distinction matters to you as a caller: a verb reported as \"not implemented\" would be\n\
         worth retrying after an upgrade, and this one never will be. The refusal names the command\n\
         that does work.\n\n",
    );

    out.push_str("## Verification\n\n");
    out.push_str(
        "Every claim in this file is backed by a probe that runs against a live guest:\n\n\
         ```sh\n\
         ./scripts/verify-all.sh\n\
         ```\n\n\
         It reports three states, not two — `passed`, `failed`, and `COULD NOT RUN`. The third\n\
         exists because collapsing it into `passed` is how a machine that was never tested comes to\n\
         look like one that works.\n\n",
    );

    out.push_str("## Machine-readable\n\n");
    out.push_str(
        "The same list, as JSON, for a caller that would rather not parse markdown:\n\n\
         ```sh\n\
         wvm capabilities --json\n\
         ```\n\n",
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_has_a_summary_and_a_home() {
        for c in CAPABILITIES {
            assert!(!c.summary.is_empty(), "{} has no summary", c.op);
            assert!(
                c.runs_on == "host" || c.runs_on == "guest",
                "{} has an unknown home: {}",
                c.op,
                c.runs_on
            );
        }
    }

    #[test]
    fn the_markdown_covers_every_verb() {
        // The property that makes the file trustworthy: a verb added to the list cannot be missing
        // from the generated doc. Without this the two could drift and the doc would be the one
        // that lies.
        let md = render_markdown();
        for c in CAPABILITIES {
            assert!(
                md.contains(&format!("### `{}`", c.op)),
                "{} is in the capability list but missing from the generated markdown",
                c.op
            );
        }
    }

    #[test]
    fn the_markdown_says_it_is_generated() {
        // A generated file that does not say so invites exactly the hand-edit that then gets
        // overwritten, which is a bad way to find out.
        let md = render_markdown();
        assert!(md.starts_with("<!-- GENERATED FILE"));
        assert!(md.contains("capabilities.rs"));
    }

    #[test]
    fn traps_are_not_decoration() {
        // Each trap is a licence to skip a debugging session. If a verb has none, either it is
        // genuinely trivial or nobody has written down what it cost — and the second is more
        // likely. `hello` is the one deliberate exception.
        for c in CAPABILITIES {
            if c.op == "hello" {
                continue;
            }
            assert!(!c.traps.is_empty(), "{} has no recorded traps", c.op);
        }
    }
}
