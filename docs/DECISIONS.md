# Decision log

Format: `D-NNN` — decision, context, alternatives, rationale, reversal cost.
Append-only. Superseding a decision means adding a new entry that references the old one.

---

## D-001 — Do not port the original LSW architecture

**Date:** 2026-09-14
**Status:** accepted

**Context.** `reference/lsw-original` (ne0YT's *Linux Subsystem for Windows*) was reviewed as a
candidate to rewrite in Rust. The entire system is ~420 lines of shell. Its core is a 38-line
`windows.sh` that does four things: `VBoxManage` VM boot/resume, mount host `/` into the guest as
`Z:\`, translate a POSIX path to a `Z:/` path, and run
`VBoxManage guestcontrol run --username admin --password $(cat vm_password.txt)`.

**Findings.**

- The headline "seamless mode" is **VirtualBox's own feature**, not the project's. Guest Additions
  is the display driver; the repo just toggles `GUI/Seamless on`.
- Host `/` is mounted read/write into the guest. Any guest process can rewrite host binaries and
  dotfiles. The author's mitigation is a script to unmount it *while running*.
- The guest administrator password is stored in plaintext in the repo directory.
- `savewindows.sh` is a one-line `vboxmanage controlvm savestate`.
- There is nothing to port. A Rust rewrite of `VBoxManage guestcontrol` would be slower, less
  maintained, and would drop the only feature users notice.

**Decision.** Do not port it. Treat it as a design reference for the *UX contract* only
("a Linux-side action produces a Windows application window") and build the missing piece instead.

**Reversal cost.** None — no code was written.

---

## D-002 — TCP control channel as the default transport, not virtio-vsock

**Date:** 2026-09-14
**Status:** accepted

**Context.** virtio-vsock is the canonical host↔guest transport on Linux and looked like the
obvious choice. Research into Windows guest support changed the assessment.

**Findings.**

- Windows has no native `AF_VSOCK`. The virtio-win driver presents sockets through `AF_HYPERV`;
  obtaining `AF_VSOCK` requires including `<vio_sockets.h>` from the driver tree.
- `virtio-win` issue #534: "the latest virtio-win.iso still does not contain socket driver …
  I would not recommend to release socket driver in its current state." The thread stays open
  from 2021 to Nov 2025.
- Issue #1185 (closed Nov 2025): viosock becomes part of the release **"starting from virtio-win
  build 285."**
- Issue #789 reports the practical failure mode: host→guest send fails with `errno 54`,
  connection reset by peer.
- Both shipping projects in this space rejected it. **WinBoat** (22.8k★, MIT) runs a Go guest
  server as a Windows service exposing an HTTP API. **WinPodX** (2.0k★, MIT) uses container
  provisioning over the guest's network.

**Alternatives considered.**

1. `virtio-vsock` — rejected. Requires a recently-added, historically-withheld driver; adds an
   install step that can fail opaquely on a user's machine.
2. `virtio-serial` on a raw character device (`\\.\Global\vport0`) — rejected. Gives a byte
   stream with no framing, no multiplexing, no reconnection, and no lifecycle. Building a trust
   boundary on a raw port is strictly more work than a TCP service.
3. `virtio-9p` / `virtio-fs` for transport — rejected. These are filesystems, not message
   transports; virtio-fs has no Windows client driver (the project points at WinFsp, built from
   source).
4. **TCP control channel over the guest's private link** — accepted.

**Decision.** Transport is a trait in `wvm-ipc`. Default implementation is TCP with explicit
length-prefixed framing. `vsock` ships as an opt-in backend. Any transport must implement
framing, max-message enforcement, and reconnect — not merely read/write.

**Consequence.** The transport is swappable, so a future change of mind costs one module, not a
rewrite. This also keeps the door open to contributing a vsock backend upstream later.

---

## D-003 — Capability boundary enforced at construction time

**Date:** 2026-09-14
**Status:** accepted

**Context.** `hcode`'s harness work established that a runtime check on an immutable resource is
a check a refactor can drop, and that a model (or caller) under pressure will route around a
boundary by putting the same intent in a permitted field.

**Decision.** A request that exceeds the caller's capability grant cannot be *constructed*. The
grant is a field on the request type; the constructor validates it. Policy denials are journalled
with the same weight as successes.

**Consequence.** Callers cannot express an unauthorised action, so there is no code path that
needs to remember to check. Verb lists are closed enums, not strings.

---

## D-004 — Headless by default, display is opt-in

**Date:** 2026-09-14
**Status:** accepted

**Context.** The competing projects exist to put windows on a desktop. WVM exists to be driven by
a program. Coupling the control plane to a display server would make it unusable in CI and would
reintroduce the brittleness (focus stealing, decoration, DPI, per-WM breakage) that the original
project suffered from.

**Decision.** All operations work with no display attached. Screen capture reads the framebuffer
directly. A viewer is available behind an explicit flag for human debugging.

**Consequence.** The agent-facing use case is the default path and the one that gets tested.
Desktop integration is explicitly out of scope for v1.

---

## D-005 — Health checks attempt the operation; they do not inspect metadata

**Date:** 2026-09-14
**Status:** accepted

**Context.** The first draft of `wvm doctor` inferred KVM access from `/dev/kvm`'s mode and the
user's membership of the `kvm` group. On the development host that reported **FAIL** — and was
wrong. The device carries an ACL:

```
# getfacl /dev/kvm
user::rw-
user:andy:rw-      <- an ACL grant, invisible to `id -nG`
group::rw-
mask::rw-
```

`open("/dev/kvm", O_RDWR)` succeeds. Two independent tools (a shell `groups` check and a Python
`open()` probe) disagreed, and the metadata-based check was the one that lied.

**Decision.** A diagnostic attempts the real operation and reports the observed result. Where a
metadata check cannot be avoided, it may add explanation but never decide the verdict.

**Consequence.** Fewer false negatives on hosts that use ACLs, capabilities, or container-mapped
device nodes — all of which are invisible to the obvious permission checks. This applies to every
check added later, not just KVM.

---

## D-006 — Suspend uses QMP `snapshot-save`, not the familiar `savevm`

**Date:** 2026-09-14
**Status:** accepted

**Context.** The first implementation of suspend called `savevm` over QMP, because that is the
command most documentation and every tutorial names. Against a live QEMU it failed:

```
QMP error for 'savevm': {"class":"CommandNotFound","desc":"The command savevm has not been found"}
```

**Findings.**

- `savevm` / `loadvm` are **HMP** (human monitor) commands. QMP has a different and explicitly
  versioned surface, where the equivalents are `snapshot-save` and `snapshot-load`.
- Querying `query-commands` on this QEMU (11.1.0) confirms it: `snapshot-save`, `snapshot-load`,
  `snapshot-delete`, `blockdev-snapshot-internal-sync` are all present; `savevm` is not.
- `snapshot-save` wants the device's **node name** as QEMU knows it, not an assumed `disk0`. It
  is therefore read back from `query-block` at call time, skipping read-only nodes and the
  `pflash` firmware devices, which are not valid snapshot targets.

**Alternatives considered.**

1. Shell out to `qemu-monitor-command` with an HMP string — rejected. It reintroduces the string
   protocol this project removed, and would make the HMP command set a runtime dependency.
2. Hardcode the device name from the config — rejected. Nothing guarantees QEMU's node name
   matches the `id=` given on the command line, and a change to the generated arguments would
   silently break suspend.

**Decision.** Use `snapshot-save`, discovering the target device from `query-block` at call time.
The command set is verified against the running QEMU rather than assumed.

**Consequence.** A general lesson, recorded because it cost a debugging round: **QMP and HMP are
different protocols with different command names.** Any QMP implementation must be validated
against `query-commands` on the actual QEMU build, not written from memory of the HMP console.

---
