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

## Requirements

- Linux with KVM (`/dev/kvm` reachable by your user — `sudo usermod -aG kvm $USER`)
- QEMU (`qemu-system-x86_64`) with `vhost-vsock-pci` only if you enable the vsock transport
- Rust (stable) + `rustup target add x86_64-pc-windows-gnu` + `x86_64-w64-mingw32-gcc`
- A Windows 11 image (tiny11 or equivalent recommended)

## Build

```sh
# host daemon
cargo build --release -p wvm-host

# guest service — cross-compiled from Linux, no Windows toolchain required
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest
```

## License

MIT — to be confirmed before first public push.
