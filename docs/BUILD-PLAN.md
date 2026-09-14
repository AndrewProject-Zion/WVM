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

**Verification:** 17 tests in `wvm-ipc`, covering empty payloads, sequential frames staying
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


## M4 — Guest service

- [ ] `wvm-guest` cross-compiled from Linux, installed as a Windows service
- [ ] Implement the transport trait; length-prefixed framing matching the host
- [ ] Process launch with stdout/stderr capture and exit code
- [ ] Input injection and framebuffer capture
- [ ] File transfer restricted to declared roots

**Verification:** from the host, launch a process in the guest and read its real output; capture a
screenshot and confirm it is a valid PNG with a plausible size; refuse a transfer outside the
declared root.

## M5 — Agent surface

- [ ] Stable request/response contract documented for harnesses
- [ ] Reference client
- [ ] Example: drive an installed Windows application end to end, headless

**Verification:** the example runs unattended and produces a journal that reconstructs what
happened.

---

## Non-goals for v1

Explicitly out of scope, to keep the boundary between this and the desktop-integration projects
sharp:

- window compositing / RemoteApp / per-application windows on a Linux desktop
- a graphical configuration UI
- GPU acceleration for guests
- desktop-environment integration (`.desktop` handlers, MIME associations, taskbar pinning)

Projects that already do these well: `WinBoat`, `WinPodX`, `winapps`.
