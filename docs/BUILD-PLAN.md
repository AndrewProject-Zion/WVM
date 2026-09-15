# Build plan

Milestones are ordered so that each one is verifiable before the next depends on it. No milestone
is "done" without a check that fails when it is broken.

## M0 — Workspace skeleton  ← COMPLETE

- [x] Cargo workspace: `wvm-ipc`, `wvm-host`, `wvm-guest`
- [x] Git repository, reference material archived read-only
- [x] Design docs (README, DECISIONS, VERIFIED-ENVIRONMENT, BUILD-PLAN)
- [x] `cargo check` green on host target for all three crates
- [x] `cargo check --target x86_64-pc-windows-gnu` green for `wvm-guest`
- [x] `cargo test` green for `wvm-ipc` protocol round-trips

**Verification (recorded 2026-09-14):**

```
cargo test --workspace            43 passed, 0 failed
cargo clippy --workspace --all-targets   0 warnings
cargo build --release -p wvm-host        ELF 64-bit, 824 KB
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest
                                  PE32+ x86-64, 447 KB
                                  imports: KERNEL32, msvcrt, ntdll, WS2_32,
                                           api-ms-win-core-synch-l1-2-0
./target/release/wvm doctor        all checks pass, exit 0
```

The Windows binary depends only on core Windows DLLs — no VC++ redistributable to install on the
guest, which is the point of cross-compiling with the GNU toolchain.

## M1 — Protocol  ← largely complete (folded into M0)

- [x] `wvm-ipc`: `Request` / `Response` enums as closed types, versioned
- [x] Framing: length-prefixed, explicit maximum message size, truncated frames rejected
- [x] Capability grant type with verbs as a closed enum
- [x] Round-trip tests including malformed and oversized frames

**Verification:** 18 tests in `wvm-ipc`, covering empty payloads, sequential frames staying
aligned, clean-EOF-vs-truncation, an oversized header refused *before* allocation, and a
`deny_all` grant refusing every verb.

## M2 — Host daemon, no VM  ← COMPLETE

- [x] `wvm-host` CLI: `doctor`, `serve`, `call`, `journal`
- [x] `doctor` reproduces the checks in `VERIFIED-ENVIRONMENT.md`
- [x] Unix socket server speaking `wvm-ipc`
- [x] Journal: append-only
- [x] Journal wired into the request path — requests, completions **and denials**
- [x] Capability denial path exercised end to end

**Verification (live, 2026-09-14):**

```
cargo test --workspace              57 passed, 0 failed
cargo clippy --workspace --all-targets    0 warnings

./target/release/wvm serve --socket /tmp/wvm.sock &
./target/release/wvm call --socket /tmp/wvm.sock hello
  -> protocol 1; peer: no guest channel yet (M3 not started)
./target/release/wvm call --socket /tmp/wvm.sock inspect
  -> os: "host (no guest channel yet)", drives: [], apps: []
stat -c '%a' /tmp/wvm.sock        -> 600
```

The boundary was exercised against a daemon started with `--verbs inspect`:

```
lifecycle  -> denied (VerbNotGranted): lifecycle is not permitted for local-dev
capture    -> denied (VerbNotGranted): capture is not permitted for local-dev
```

Both refusals appear in the journal with their reason, alongside the request that triggered them:

```
{"event":{"kind":"request","verb":"lifecycle","op":"lifecycle"}}
{"event":{"kind":"denied","verb":"lifecycle","reason":"VerbNotGranted"}}
```

Also verified live: rebinding over a socket file left behind by a `kill -9` (where `Drop` cannot
run) succeeds. The stale file proves nothing about whether anything is listening.

**Bug found by reading live output rather than by a test.** The first version classified any
response that was not `Ok` as a failure, so every `Ready` handshake was journalled as `ok:false`.
An audit log that reports false errors on the happy path trains its readers to ignore it.
Handshakes are now not journalled at all — one per connection would bury the events that matter —
and `a_handshake_is_not_journalled_as_a_failure` is the regression test that would have caught it.



## M3 — QEMU supervision  ← COMPLETE

- [x] Launch a VM from a config file with a verified command line
- [x] Lifecycle: start / suspend / shutdown, plus prepare and status
- [x] Health: `Running` / `Unresponsive` / `Stopped`, distinguished rather than collapsed
- [x] `wvm vm cmdline` prints the argument vector without launching anything
- [x] `wvm vm log` tails the serial console
- [ ] Snapshot restore (`snapshot-load`) — suspend works, load is not yet wired
- [ ] Idle auto-suspend

**Verification (live, 2026-09-14).** A 1 GiB demo VM was created, booted and exercised:

```
wvm vm validate --config demo.toml      -> valid, all fields reported
wvm vm cmdline  --config demo.toml      -> full readable qemu-system-x86_64 command line
wvm vm prepare  --config demo.toml      -> disk.qcow2 + per-instance OVMF_VARS.fd created
wvm vm start    --config demo.toml      -> "qemu started, pid 1530190" / "responsive: running"
wvm vm status   --config demo.toml      -> running, with pid-file and qmp-socket state
wvm vm log      --config demo.toml      -> see below
wvm vm suspend  --config demo.toml      -> suspended
```

**The VM genuinely booted.** The serial console — real UEFI firmware output from inside the guest —
proves it, and proves the generated command line is one QEMU accepts:

```
BdsDxe: failed to load Boot0002 "UEFI Misc Device" from PciRoot(0x0)/Pci(0x2,0x0): Not Found
BdsDxe: No bootable option or device was found.
BdsDxe: Press any key to enter the Boot Manager Menu.
```

OVMF ran, probed PCI, tried the virtio-blk disk and found no OS (correct: the disk is empty and no
installer is attached), then entered the boot manager. That is the correct behaviour for an empty
disk, and it is the right place to be before an installer is attached.

**Suspend verified independently of our code.** `qemu-img` reads the snapshot straight out of the
qcow2 image with QEMU not running:

```
$ qemu-img snapshot -l disk.qcow2
ID   TAG            VM_SIZE     DATE                  VM_CLOCK     ICOUNT
1    wvm-suspend    48.9 MiB    2026-09-14 13:44:18   0000:00:43.543    --
```

**Two bugs found by running it, not by tests:**

1. **`--config` was rejected after the action.** `wvm vm validate --config x` failed with
   `unexpected argument '--config'` — the field was on the parent subcommand, which the derive
   accepts and the parser refuses. The flag is now `global = true` on each action. A test would
   not have caught this; running the binary for one second did.

2. **`savevm` is not a QMP command.** Suspend failed with
   `CommandNotFound: The command savevm has not been found`. `savevm`/`loadvm` are HMP commands;
   the QMP equivalents are `snapshot-save`/`snapshot-load`, confirmed by `query-commands` against
   the live QEMU. The device to snapshot is now read from `query-block` rather than assumed to be
   `disk0`. See D-006 — QMP and HMP are different protocols and must not be written from memory.

**Shutdown behaved correctly when there was nothing to shut down.** With no OS installed, the
guest cannot honour ACPI, so `system_powerdown` timed out and the command reported:

```
Error: the guest did not shut down within 10s; it may be ignoring ACPI
       (use `wvm vm kill` to force it)
```

That is the honest result, and it stays running rather than silently reporting a shutdown that did
not happen. `kill` is the documented escalation, not yet implemented as a subcommand.

**Not yet required: TPM.** Windows 11 nominally wants TPM 2.0 and Secure Boot; tiny11 removes both
requirements, so no `swtpm` process is needed and the generated command line deliberately omits
one. See `docs/VM-CONFIG.md`.


## M4 — Guest service  ← COMPLETE

- [x] `wvm-guest` cross-compiled from Linux (550 KB PE32+, core DLLs only)
- [x] Length-prefixed framing matching the host, transport behind a trait
- [x] Windows path canonicalisation with containment rules
- [x] Transfer planning and execution, with overwrite refusal
- [x] Installer script for registering the service in the guest
- [x] Install as a service in the running guest
- [x] Verify the guest can reach the host over the forwarded port
- [x] Process launch with stdout/stderr capture and exit code
- [x] Wire the transport, paths and transfer modules into `dispatch`
- [x] Input injection and framebuffer capture  ← done in M5

**Verified end to end**, driven from Linux with no console and no keyboard:

```
hello  ->  {"status":"ready","protocol_version":1,"guest":"Windows (wvm-guest 0.1.0)"}

exec   ->  {"result":"process_output","outcome":"exited","code":0,
            "stdout":"\r\nMicrosoft Windows [Version 10.0.22631.2715]\r\n",
            "elapsed_ms":37}
```

A process launched inside Windows, its output captured, returned to the host.

**What made it a service, and the distinction that mattered.** `sc.exe create` registers a binary
path; it does not make a program a service. The SCM starts the process and waits for it to call
`StartServiceCtrlDispatcher` and report `SERVICE_RUNNING`. A console program does neither, so the
SCM waits out its timeout and marks the service failed — while `create` reports SUCCESS and `query`
shows an entry that exists but never runs. `wvm-guest/src/service.rs` owns that conversation and
nothing else, delegating the work to the same `serve_loop` console mode uses.

Console mode is kept deliberately: running the binary by hand is what makes a service install
diagnosable, and was the only way to see this failure.

**The bind address that could never have worked.** `default_bind()` was `127.0.0.1:48273`. The
host's forward arrives as an INBOUND connection on the guest's external interface; the `127.0.0.1`
binds the host end and says nothing about where the packet lands inside the guest. The service
would have listened and accepted nothing, forever, with every host-side check reporting healthy.
Now `0.0.0.0`, with the reasoning recorded.

**Three faults in the input tooling, one root cause — the keymap was written for a US keyboard and
the guest is UK.** Each presented as a different plausible fault, and each was found by measuring
rather than reasoning:

| Fault | Symptom | Misdiagnosed as |
|---|---|---|
| `"` → SHIFT+apostrophe (yields `@`) | `sc.exe` rejected its own quoting | a bad path / permissions |
| `backslash` → `#` (US physical position) | pnputil: "missing driver package" | a bad driver disc |
| `send-key` held each key 60ms | characters reordered to the line start | a line-length limit |

The reordering one was measured with `scripts/probe-typing-limit.py`, which disproved the
length-limit theory: nothing was dropped or split, and 110-character lines arrived intact once the
hold was replaced with explicit press/release events.

**Also learned:** a TCP connect to a forwarded port is NOT proof of anything. slirp completes the
handshake locally and only then tries to deliver, so a connect succeeds against a guest with no
service listening. Success and failure look identical. Proof requires a round trip that exchanges
real frames.

**Tests:** 131 across the workspace, 0 clippy warnings, Windows cross-compile clean.

See `docs/WINDOWS-INSTALL-STATUS.md` for what is installed in the guest and how to drive it.


## M5 — Agent surface  ← IN PROGRESS

- [x] Reference client (`examples/wvm_client.py`, dependency-free, stdlib only)
- [x] Capability boundary demonstrable (`wvm_client.py boundary`, exits non-zero on a leak)
- [x] `capture` — PNG of the live desktop, from the host framebuffer (D-009)
- [x] `input` — absolute pointer moves and clicks, UK-correct keys and text (D-010, D-011)
- [x] WVM-01 fixed — execution pipes drained concurrently, not after exit
- [x] `transfer push` — host to guest, chunked in lockstep (D-012)
- [x] `transfer pull` — guest to host, so an agent can extract what it produced
- [x] `exec` timeout kills the process TREE, not just the direct child (D-013)
- [ ] `lifecycle` — snapshot and restore
- [ ] Stable request/response contract documented as a standalone reference
- [ ] Example: drive an installed Windows application end to end, headless

**Verified against a live guest**, not asserted:

```
click --no-click 336 397   -> pointer landed on the Wikipedia tile, hover active
click 336 397              -> Wikipedia loaded; tab title and URL correct
text abc123                -> appeared in the address bar, autocomplete fired
text 'q"w@e#r$t%y&u*i:o/p-end'
                           -> came back character for character
```

That last line is the one that matters: `"` and `@` are the two characters that stalled M4, and
both now round-trip.

**WVM-01 — `exec` reported healthy commands as `timed_out`.**

`run` waited for the process to exit and only then read stdout and stderr. A Windows pipe holds
about 64 KiB, so a process emitting more than that blocked on its next write, never exited, and
`cmd /c dir /s C:\` came back as `"outcome":"timed_out"` with its already-produced output
discarded. The command was fine; we had wedged it ourselves.

Reporting a healthy command as hung is the worst failure mode a control channel can have — the
caller cannot tell it from a genuinely hung command. Both pipes are now drained on their own
threads, started before the wait loop and joined after, so the deadline measures the command rather
than our own greed.

`scripts/verify-wvm-01-regression.py` re-introduces the original sequencing and asserts the
regression test **fails** on it. It does — it *hangs*, because the writer blocks on a full pipe
nobody is draining. That is the bug reproducing itself, and it is stronger evidence than a failed
assertion would have been.

**Verified so far (client):** the Python client speaks the same wire format as the Rust host and the
`boundary` subcommand starts its own daemon with an inspect-only grant, then attempts to exceed it:

```
hello      -> handshake accepted
inspect    -> inventory: os='host (no guest channel yet)', 0 drive(s), 0 app(s)
capture    -> REFUSED
lifecycle  -> REFUSED
exec       -> REFUSED
```

Two independent implementations agreeing on framing is what makes this a protocol rather than an
implementation detail — if Python and Rust ever disagree, the documentation is wrong.


---

## Non-goals for v1

Explicitly out of scope, to keep the boundary between this and the desktop-integration projects
sharp:

- window compositing / RemoteApp / per-application windows on a Linux desktop
- a graphical configuration UI
- GPU acceleration for guests
- desktop-environment integration (`.desktop` handlers, MIME associations, taskbar pinning)

Projects that already do these well: `WinBoat`, `WinPodX`, `winapps`.
