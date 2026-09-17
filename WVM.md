<!-- GENERATED FILE — do not edit by hand.

   Source:    wvm-host/src/capabilities.rs
   Regenerate: wvm capabilities --markdown > WVM.md
   Checked by: scripts/check-docs.sh (a change without a regenerate FAILS the build)

   Editing this file directly will be overwritten. Add or change a verb in the source
   list instead, and the markdown, the --json output and the check all follow.
-->

# WVM for agents

You are driving a Windows 11 machine from a Linux terminal. This file is the contract: the
verbs, the exact responses, and the traps that cost real time to find. It is generated from
the code, so it cannot describe a verb that does not exist.

## The one command to run first

Before anything else, find out what the sandbox will and will not do. The refusals are as
important as the capabilities, and they arrive as structured data rather than a timeout:

```sh
python3 examples/wvm_client.py boundary
```

A refusal is the boundary working. It names what it refused and why, so you can route
around it deliberately instead of discovering the limit as a failure halfway through a job.

## Verbs

### `hello`

Handshake. Reports the guest's protocol version and OS.

- **Runs on:** guest

Traps:

- Do this first. It is how you find out whether a guest is there at all, rather than inferring it from a connection that succeeds.

### `exec`

Run a program in the guest and return its stdout, stderr and exit code.

- **Runs on:** guest
- **CLI:** `wvm vm exec --config <cfg> "<command>"`

Traps:

- `&&` does not survive this layer. `cd /d X && cmd` exits 1 with EMPTY stdout and no error — the command simply does not run. Run one thing at a time.
- `&&` is one of several characters that break quoting. If a command returns nothing where output was expected, suspect the quoting before the program.
- The guest runs every command with stdin redirected to null, and `timeout.exe` refuses that — it prints 'Input redirection is not supported' and exits in MILLISECONDS. A test that uses it as a delay will count zero survivors for the absence of anything to count. Use `ping -n <secs> 127.0.0.1`.
- The default working directory is C:\Windows\System32, so a relative filename lands somewhere you did not intend.
- The service runs as `nt authority\system`, so anything launched is already SYSTEM-level. Elevation is not a problem here, and `setx /M` needs no help.
- A timeout kills the whole process TREE, not just the direct child, via a Windows Job Object. A caller can rely on that.

### `capture`

Screenshot the live desktop.

- **Runs on:** host
- **CLI:** `wvm vm capture --config <cfg> --out shot.png`

Traps:

- This runs on the HOST over QMP, not in the guest. A Windows service runs in session 0 which has no desktop, so BitBlt from inside can never work — that is architectural, not a gap.
- It captures the whole desktop, not one window. There is no per-window targeting.

### `input`

Move the pointer, click, type text and send key chords.

- **Runs on:** host
- **CLI:** `wvm vm input --config <cfg> ...`

Traps:

- Also host-side, for the same reason: session-0 isolation blocks the SendInput API but NOT emulated hardware, so the host injects through QMP and the kernel delivers it to the interactive session.
- Typing is paced deliberately. Sending keys too fast makes the guest REORDER them, and the symptom is not dropped characters but stray characters prefixed to the line — which looks like a line-length limit and sends you looking in the wrong place.
- Control characters and chords need explicit down/up events. A press-and-release pair arrives as the modifier released before the key, so Ctrl+R opens the Start menu instead of Run.
- Driving an elevation (UAC) prompt this way is unreliable. If a human is at the screen, ask them — it takes a second and cannot silently half-succeed.

### `transfer`

Move a file in either direction, in chunks confined to a guest staging root.

- **Runs on:** guest
- **CLI:** `wvm vm transfer push|pull --config <cfg> <SOURCE> <GUEST_PATH>`

Traps:

- `pull` INVERTS the positional names. They are written for a push, so for a pull SOURCE is the GUEST file being fetched and GUEST_PATH is the HOST destination. Swap them and the guest refuses a path it cannot see — an error that reads like a boundary problem rather than a swapped argument.
- Do NOT quote a path passed to a guest command. The quote characters reach the process as part of the argument, and `certutil -hashfile "C:\path"` fails with FILE_NOT_FOUND — the file is there; the quotes are the problem.
- Everything lands under one staging root (C:\ProgramData\wvm\staging by default). Paths outside it are refused, and that refusal is a feature.
- Chunks travel in lockstep with an offset checked on every one. Do not try to stream them; a gap or an overlap would otherwise produce a right-length, wrong-content file.

### `lifecycle`

Snapshot the machine, restore it, list snapshots, delete one.

- **Runs on:** host
- **CLI:** `wvm vm snapshot save|restore|list|delete --config <cfg> <tag>`

Traps:

- A SAVE CAN CRASH THE GUEST on this Windows 11 build — roughly one save in five ends in a 0x50 PAGE_FAULT_IN_NONPAGED_AREA bugcheck. The faulting instruction is in Windows' own kernel, at one fixed code offset across four crashes, and pausing the vCPUs for the same length of time does NOT reproduce it, so the trigger is the device-state write rather than the freeze. Reproduced, localised, NOT fixed. Do not snapshot anything you cannot afford to lose, and do not put a snapshot in the path of an unattended agent run.
- The guest REFUSES this verb, permanently. Snapshots are a hypervisor capability and the guest has no view of its own hypervisor. The refusal names the command that works — it is not a stub awaiting an upgrade.
- A save FREEZES the guest CPUs while the machine state is written, and NO QMP message arrives for the whole of that write. Measured here at 57 seconds with nine snapshots on the disk, 6.5 seconds when it was fresh — the pause GROWS as snapshots accumulate. A client read timeout shorter than the pause reports failure for work that is still succeeding, and makes a frozen guest look like a dead one.
- A snapshot holds the machine's MEMORY as well as its disk, so it is roughly the RAM size rather than a delta. That is what makes a restore a rollback instead of a disk revert — and why snapshots are worth deleting.
- Restoring while the guest is running is supported and it survives. It is still a rollback: anything written since the snapshot is gone, including files the agent created.

## What the guest will refuse, and why that is the answer

`lifecycle` is refused from inside the guest, permanently. Snapshots are a hypervisor
capability and the guest has no view of its own hypervisor, so this can never work from that
side.

The distinction matters to you as a caller: a verb reported as "not implemented" would be
worth retrying after an upgrade, and this one never will be. The refusal names the command
that does work.

## Verification

Every claim in this file is backed by a probe that runs against a live guest:

```sh
./scripts/verify-all.sh
```

It reports three states, not two — `passed`, `failed`, and `COULD NOT RUN`. The third
exists because collapsing it into `passed` is how a machine that was never tested comes to
look like one that works.

## Machine-readable

The same list, as JSON, for a caller that would rather not parse markdown:

```sh
wvm capabilities --json
```

