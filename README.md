# WVM — Windows VM control plane for autonomous agents

**A Rust control plane that lets a program drive a Windows VM as a typed tool: launch processes,
screenshot, inject input, move files, snapshot and restore.**

Not a desktop-integration toy. WVM has no GUI, no taskbar icon, no Electron shell. It is a
machine-facing control plane for agents and automation, built on QEMU/KVM, with a typed,
versioned protocol and a hard capability boundary.

---

## Why this exists

Windows-only software is a capability gap for anything running on Linux — including autonomous
agents. The existing projects in this space (`WinBoat` 22.8k★, `WinPodX` 2.0k★, `winapps`) all
solve a **human** problem: put a Windows application window on a Linux desktop. They are excellent
at it and you should use them if that is what you want.

None of them solve the **automation** problem, because their interfaces are built for a person:

- window compositing assumes a display server and a session
- guest agents expose data, not *control* (`WinBoat`'s guest server is a read-mostly HTTP API)
- no typed request/response contract, no lifecycle guarantees, no audit trail
- no notion of a capability boundary — an agent driving one has ambient authority over the VM

WVM is that missing control plane. It is designed to be driven by a program, by an agent harness,
or by a CI job — headless by default, display optional.

---

## What it does

| Capability | Notes |
|---|---|
| VM lifecycle | create / start / suspend / resume / snapshot / restore / destroy |
| Process control | launch, wait, capture stdout/stderr, return exit code |
| File transfer | host → guest, guest → host, with explicit, sandboxed roots |
| Screen capture | PNG framebuffer grab, no display server required on the host |
| Input injection | keyboard and mouse at the protocol level (not X11 scraping) |
| Guest inventory | installed applications, drives, OS build |
| Audit | every request, response, and VM state transition is journalled |

Display is **optional**. The plane runs fully headless; `--display` attaches a viewer when a human
needs to look.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Agent / CI job / your code                                 │
└───────────────────────────┬─────────────────────────────────┘
                            │  typed request (JSON over Unix socket)
                            │  + capability grant
┌───────────────────────────▼─────────────────────────────────┐
│  wvm-host        (Rust, Linux)                              │
│    • capability boundary  — what this caller may do         │
│    • policy engine        — sandbox roots, process allowlist │
│    • journal              — every action, replayable         │
│    • QEMU supervisor      — lifecycle, snapshots, health     │
└───────────────────────────┬─────────────────────────────────┘
                            │  framed JSON  (transport is pluggable)
┌───────────────────────────▼─────────────────────────────────┐
│  QEMU/KVM guest: Windows                                     │
│    wvm-guest     (Rust, Windows, cross-compiled from Linux)  │
│      • control service (installed once, runs at boot)        │
│      • Win32 process launch, capture, input, file ops        │
└─────────────────────────────────────────────────────────────┘
```

### Cargo workspace

| Crate | Target | Responsibility |
|---|---|---|
| `wvm-ipc` | any | Protocol types. Shared by host and guest — the contract cannot desync. |
| `wvm-host` | Linux | CLI + daemon: capability boundary, policy, journal, QEMU supervision. |
| `wvm-guest` | Windows | Guest service: Win32 execution, capture, input, file transfer. |

### The transport decision (read this before proposing VSOCK)

`virtio-vsock` looks like the obvious choice for host↔guest and it is **not the right default**:

- Windows has no native `AF_VSOCK`. The virtio-win driver exposes sockets via `AF_HYPERV`; callers
  must include `<vio_sockets.h>` to get an `AF_VSOCK` shim.
- The `viosock` driver only became part of the virtio-win release at **build 285** (late 2025),
  after a long history of being withheld as not release-ready.
- Reported failure mode in the field (`Connection reset by peer`, `errno 54`) is exactly the kind
  of thing that makes a public project un-installable on someone else's machine.
- Both shipping projects in this space rejected it: `WinBoat` uses a plain TCP/HTTP service,
  `WinPodX` uses container provisioning.

**Decision:** the transport is an explicit, swappable trait behind `wvm-ipc`. The default is a
TCP control channel on the guest's private link. `vsock` is available as an opt-in backend for
users who want it and have the driver. Any new transport must implement the whole trait, including
framing, length limits, and reconnect — not just the read/write calls.

---

## Security model

The original project this was inspired by mounted the host's `/` into the guest as a read/write
share and stored the guest administrator password in a plaintext file next to the launcher. Both
are treated here as design errors to avoid, not features to port.

| Concern | Approach |
|---|---|
| Host filesystem exposure | **Never** a root mount. Explicit, per-request, sandboxed roots only. |
| Credentials | No plaintext secrets on disk. Guest control is authenticated by a per-install key. |
| Agent authority | Capability grants. A caller gets only the verbs it was issued. |
| Process execution | Allowlist policy; unlisted binaries are refused before reaching the guest. |
| Audit | Append-only journal: request, decision, result. Denials are recorded too. |
| Guest → host | Transfer roots are separate from read roots, and neither is the home directory. |

The capability boundary is enforced at **construction time** in the host (a request that exceeds
its grant cannot be built), not by a check a later refactor can bypass. This mirrors the approach
used in `hcode`'s harness boundary.

---

## Status

**Pre-alpha / design complete.** Nothing is wired end-to-end yet. See `docs/BUILD-PLAN.md` for
the milestone sequence and `docs/VERIFIED-ENVIRONMENT.md` for exactly what has been checked on a
real host.

## Quickstart

```sh
# 1. Check the host. This performs the checks, rather than reading metadata about them.
cargo build --release -p wvm-host
./target/release/wvm doctor

# 2. Build the guest service — cross-compiled from Linux, no Windows toolchain needed.
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest

# 3. Locate your ISOs, write the configs, and create the disk.
#    Reports an in-flight browser download as such rather than "not found".
./scripts/prepare-install.sh

# 4. Read the command line before running it.
./target/release/wvm vm cmdline --config ~/wvm/install.toml

# 5. Install Windows.
./target/release/wvm vm start --config ~/wvm/install.toml --timeout 300
./target/release/wvm vm log   --config ~/wvm/install.toml --lines 60
```

Two ISOs are needed and both matter:

| ISO | Why |
|---|---|
| a Windows image | tiny11 recommended — it needs neither TPM 2.0 nor Secure Boot |
| **virtio-win drivers** | Windows has no in-box virtio driver. **Without this, Setup reports no disks.** |

In Windows Setup, when it asks where to install: **Load driver** → browse the driver CD →
`viostor` → `w11` → `amd64` → *Red Hat VirtIO SCSI controller*. The disk then appears.

After installation, use the other config so the installer is no longer attached:

```sh
./target/release/wvm vm shutdown --config ~/wvm/install.toml
./target/release/wvm vm start    --config ~/wvm/wvm.toml
```

Two configs, not one, because a bootable installer left attached to a working VM drops it back into
Windows Setup.

## Running without a VM

The control plane, the policy gate and the journal are all exercisable with no guest:

```sh
./target/release/wvm serve --socket /tmp/wvm.sock &
./target/release/wvm call --socket /tmp/wvm.sock hello
./target/release/wvm call --socket /tmp/wvm.sock inspect

# Narrow the grant and watch the boundary hold.
./target/release/wvm serve --socket /tmp/wvm-ro.sock --verbs inspect

./target/release/wvm journal --limit 20
```

Denials are journalled alongside successes, with the reason:

```
{"event":{"kind":"request","verb":"lifecycle","op":"lifecycle"}}
{"event":{"kind":"denied","verb":"lifecycle","reason":"VerbNotGranted"}}
```

### A reference client, and a demonstration of the gate

`examples/wvm_client.py` is a dependency-free Python client — standard library only, no build step.
It speaks the same protocol the Rust host does, which makes it both a usage example and a second
independent implementation of the wire format.

```sh
# Start a daemon with only the Inspect verb, then try to exceed it.
python3 examples/wvm_client.py boundary
```

```
  hello      -> handshake accepted; peer reports: no guest channel yet
  inspect    -> inventory: os='host (no guest channel yet)', 0 drive(s), 0 app(s)
  capture    -> REFUSED
  lifecycle  -> REFUSED
  exec       -> REFUSED

Every ungranted verb was refused. The grant held.
```

That output is from a real run against the real daemon. It exits non-zero if anything the grant
forbids is permitted, so it works as a check rather than only as a demo.

## Status

**Windows 11 is installed and running** in a guest this project created and booted. The control
plane, capability boundary and journal are working and verified. The guest service is part-written:
its path and transfer logic is complete and tested, and what remains is the Win32 layer.

| Milestone | State |
|---|---|
| M0 workspace, protocol, capability boundary | done |
| M1 protocol and framing | done |
| M2 control socket, policy gate, journal | done |
| M3 QEMU supervision — boots VMs | done |
| M4 guest service | part-written; Win32 layer outstanding |
| M5 agent surface | reference client done |

Verified on a real host, not asserted:

- a VM boots from a tiny11 ISO and Windows Setup runs (driven entirely through QMP input)
- Windows 11 installs, reboots, and reaches a desktop
- `snapshot-save` writes VM state into the image (`qemu-img snapshot -l` reads it back)
- the capability gate refuses ungranted verbs, and journals the refusals
- 116 tests passing, 0 clippy warnings; the guest cross-compiles to a 440 KB PE32+ binary

See `docs/BUILD-PLAN.md` for the milestone detail and `docs/DECISIONS.md` for the reasoning.
`docs/WINDOWS-INSTALL-STATUS.md` records the state of the installed guest.

## docs/

| File | Content |
|---|---|
| `BUILD-PLAN.md` | the milestone sequence, with a recorded verification for each |
| `DECISIONS.md` | D-001..D-006 — what was decided, what was rejected, and why |
| `VERIFIED-ENVIRONMENT.md` | every host check, with the command that produced it |
| `WINDOWS-INSTALL-STATUS.md` | what is installed in the guest, and how to run it |
| `VM-CONFIG.md` | the VM definition reference, including the two-ISO requirement |
| `INSTALL-WINDOWS.md` | the install runbook |
| `ORIGINAL-NOTES.txt` | the notes that started the project, preserved as received |

## scripts/

| Script | Purpose |
|---|---|
| `start-windows.sh` | start the installed VM; refuses to double-start |
| `stop-vms.sh` | stop all QEMU, matching on process name so it cannot self-match |
| `vm-health.sh` | one command answering "is anything actually wrong?" |
| `diagnose-qmp-socket.py` | stale socket vs live one, by inode comparison |
| `qmp_input.py` | drive the guest by keyboard and capture the screen |
| `vm-with-display.py` | run the daemon's command line with a window |
| `prepare-install.sh` | locate ISOs, write both configs, create the disk |
| `install-guest-service.ps1` | install the guest service inside Windows |
| `fetch-chunked.sh` | chunked download with verified resume |
| `probe-drive-topology.sh` | test QEMU argument topologies against real QEMU |
| `status.sh` | repository and VM state at a glance |

Two of these earn their place from specific failures, and are worth reading if you touch this code:

- **`fetch-chunked.sh`** exists because a resume that is wrong produces a file of the right *size*
  containing the wrong *bytes*. Every size-based check passes. Its test suite compares against an
  independently fetched reference, and is mutation-tested.
- **`diagnose-qmp-socket.py`** separates a stale socket from a live one. A socket file outlives its
  daemon, and a process can be listening on an inode the filesystem no longer points at —
  file present, listener present, connections refused. Only an inode comparison explains that.


## Requirements

- Linux with KVM (`/dev/kvm` openable by your user — group membership or an ACL)
- QEMU (`qemu-system-x86_64`, `qemu-img`) with OVMF for UEFI
- Rust (stable) + `rustup target add x86_64-pc-windows-gnu` + `x86_64-w64-mingw32-gcc`
- A Windows image (tiny11 recommended) and the virtio-win driver ISO

`wvm doctor` checks all of these, and tests by performing each operation rather than inspecting it —
a metadata check reports false failures against ACLs. See `docs/DECISIONS.md` D-005.

## License

MIT — to be confirmed before first public push.

