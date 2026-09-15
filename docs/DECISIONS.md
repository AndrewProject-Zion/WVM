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

## D-007 — The guest is a Windows service, with console mode kept alongside it

**Date:** 2026-09-15
**Status:** accepted

**Context.** The guest binary was a console program that bound a socket and looped. It was installed
with `sc.exe create`, which reported SUCCESS, and the service then never ran: `sc.exe query` showed
an entry that existed, nothing listened on the port, and the host round trip timed out.

`sc.exe create` registers a binary **path**. It does not make a program a service. The Service
Control Manager starts the process and waits for it to call `StartServiceCtrlDispatcher`, report
`SERVICE_RUNNING`, and stand by for control requests. A console program does none of that, so the
SCM waits out its timeout and marks the service failed.

**Decision.** Implement the SCM conversation in a dedicated module (`wvm-guest/src/service.rs`)
that owns nothing else, and delegate the actual work to the same `serve_loop` the console path uses.
Console mode is retained, selected by an explicit `--service` flag rather than inferred from the
environment.

**Rejected alternatives.**

*Inferring service context* (checking for arguments, or probing for an SCM dispatcher) is guesswork
where a flag is a fact. The installer knows which mode it wants; it says so.

*Two implementations, one per mode.* They drift. A service and a console run are the same behaviour
started differently, so there is one `serve_loop` and two entry points into it.

*Dropping console mode once the service worked.* Running the binary by hand and reading its output
was the only way to see this failure at all. A service whose only interface is the event log is
markedly harder to diagnose, and the debugging value is worth the small amount of extra code.

**Consequence, and a known gap.** The control handler signals a channel and returns immediately —
it must, because the SCM serialises control requests and blocking in the handler stalls stop for
every service call. But the serving loop blocks in `accept`, so a stop request does not interrupt
it and the SCM eventually kills the process. Closing this needs a non-blocking accept with a poll
timeout. It is recorded here rather than half-implemented: **an advertised graceful stop that does
not stop is worse than one that honestly waits.**

**Generalisation.** "Registered" and "running" are different claims, and only the second one
matters. This is the same shape as D-005: a check that cannot distinguish success from failure is
not a check. The installer now verifies a **listening socket**, not the service list.

---

## D-008 — A TCP connect is not proof that the guest is reachable

**Date:** 2026-09-15
**Status:** accepted

**Context.** During M4 development, a TCP connect to the forwarded port (`127.0.0.1:48274`)
succeeded consistently while the guest service was not running, was not installed, and — on one
occasion — while the guest had no network adapter at all. It would have been easy to record a
passing transport on a guest that could not receive anything.

The cause is QEMU's user-mode networking. slirp completes the TCP handshake **locally, on the
host's behalf**, and only then attempts to deliver to the guest. The guest's answer to that attempt
arrives later, or never. So a `connect()` returning success says nothing about whether anything is
listening on the other side.

**Decision.** Any check of the guest channel must **exchange frames**. A round trip that sends a
length-prefixed request and receives a length-prefixed response is the smallest thing that
distinguishes a working channel from a plausible one. `scripts/talk-to-guest.py` exists for this and
prints an explicit warning about the connect being meaningless, so the number cannot be misread.

**Rejected alternative.** Treating `connect()` as a liveness check. It is faster and it is wrong:
its success and failure modes are indistinguishable in exactly the case that matters — when
something is listening on the host side and nothing on the guest side.

**Consequence.** This is the fourth instance of one lesson, which is why it is written down
separately from the others (D-005 on metadata, the held-key reordering, and the `100`-token
abliteration budget are the family). **A check whose failure mode cannot be told apart from its
success mode is not a check — it is a coin that always lands heads.**

---
