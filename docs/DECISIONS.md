# Decision log

Format: `D-NNN` — decision, context, alternatives, rationale, reversal cost.
Append-only. Superseding a decision means adding a new entry that references the old one.

---

## D-001 — Do not port the original LSW architecture

**Date:** 2026-09-14
**Status:** accepted

**Context.** ne0YT's *Linux Subsystem for Windows* was reviewed as a candidate to rewrite in Rust.
Its code was removed from the tree before this repository went public — the attribution and the
reasoning both live on, in `docs/ORIGIN.md` and below. The entire system is ~420 lines of shell. Its
core is a 38-line `windows.sh` that does four things: `VBoxManage` VM boot/resume, mount host `/`
into the guest as `Z:\`, translate a POSIX path to a `Z:/` path, and run
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

## D-009 — Capture runs on the host against the framebuffer, not inside the guest

**Date:** 2026-09-15
**Status:** accepted

**Context.** Capture was implemented first inside the guest, via Win32 GDI (`GetDC`, `BitBlt`,
`GetDIBits`), returning a PNG base64-encoded through the typed protocol. This was the natural
choice: it goes through the same grant check and journal as every other operation, and it was
written and tested (a full PNG encoder with CRC and Adler-32, plus a base64 codec).

Against the live guest it failed at `BitBlt`:

```
capture: BitBlt failed (error 6)
```

**Finding.** The service runs in **session 0**. Measured in the guest:

```
SESSIONNAME   USERNAME    ID  STATE
>services                  0  Disc
 console      zion-win     1  Active
```

The user's desktop is session 1. Session 0 is isolated and has no desktop to copy from — this is a
deliberate Windows security boundary dating from Vista, and `BitBlt` from session 0 is supposed to
fail. It cannot be worked around: reaching the interactive desktop requires
`WTSQueryUserToken` + `CreateProcessAsUser` with `lpDesktop = "winsta0\default"` and
`SE_TCB_NAME`, i.e. a privilege-elevating process spawned into session 1 per request.

**Decision.** Capture runs on the **host**, issuing QMP `screendump` and converting the PPM to PNG
in `wvm-host/src/image.rs`. The operation still passes through the capability boundary and the
journal, which was the actual point of routing it through the protocol — the pixels are simply
taken from the framebuffer that QEMU owns rather than from a session that structurally cannot see
the desktop.

**Alternatives rejected.**

- `CreateProcessAsUser` into session 1 (~200 lines of Win32, `SeTcbPrivilege`, a process spawned per
  request). Correct, and the right answer if a *hostile guest* ever needs to be prevented from
  misreporting its own screen. Not warranted for a tool driving a VM the operator already controls,
  and it is a large amount of privilege-moving code to maintain.
- Running the service as a session-1 logon task. Gives it a desktop, and gives up SCM-managed
  auto-restart and service lifecycle — trading a real property for a cosmetic one.

**Consequence.** `capture.rs` and `base64.rs` in the guest are **kept**: they are correct, tested,
and exactly what the session-1 variant needs if that is ever built. They are not wired into
dispatch, and the dead-code allowance on them records why.

**A guest with no display cannot be captured at all.** That is inherent to reading the framebuffer,
and it is honest: a black image from a headless guest would be indistinguishable from a dark screen,
which is the same silent-failure shape this project keeps recording.

---

## D-010 — Input goes through emulated hardware, and the pointer is absolute

**Date:** 2026-09-15
**Status:** accepted

**Context.** Input was expected to need the same `CreateProcessAsUser` machinery as D-009, since a
session-0 service cannot call `SendInput` into session 1.

**Finding.** Session 0 isolation applies to the **Windows input API**, not to **emulated hardware**.
QMP input arrives as PS/2 and USB device events, which the kernel delivers to whichever session owns
the active console — session 1. No Windows API is involved, so the isolation boundary never applies.

The evidence was already in the repository: the entire Windows installation had been driven by QMP
keyboard injection, with no guest-side component. Checking that before writing code avoided a second
large piece of privilege-elevating Win32.

**Decision.** Input is issued from the host via QMP, through `wvm vm input`.

Two supporting decisions:

- **A `usb-tablet` is attached, on a `qemu-xhci` controller declared before it.** Without an
  absolute pointing device QEMU exposes only a relative mouse, whose deltas accumulate — a
  coordinate cannot be expressed at all. `qemu-xhci` is required on q35, which provides no USB bus
  by default, and the device must name its bus (`bus=xhci0.0`).
- **The keymap is UK and verified by typing into the guest, not derived.** Keycodes are physical
  positions, so what they produce depends on the guest's layout. Three entries were wrong and each
  produced a symptom that looked like a different fault entirely (see D-011).

**Consequence.** Input requires QEMU to be running, like capture, and cannot be used with a guest
whose display device is absent. A backslash is **refused** rather than mapped to its nearest
neighbour, with the workaround named, because silently substituting `#` is what turned a correct
path into `E:#NetKVM#w11#...` and sent a debugging session in the wrong direction.

---

## D-011 — Measure the keymap by typing into the guest; never derive it from the host's layout

**Date:** 2026-09-15
**Status:** accepted

**Context.** Three separate bugs across two sessions, all one root cause: the input map was written
for a US keyboard on a guest configured UK.

| Intended | Was mapped to | Produced | Presented as |
|---|---|---|---|
| `"` | SHIFT + apostrophe | `@` | `sc.exe` rejecting its own quoting — misread as a bad path or permissions |
| `\` | `backslash` | `#` | `pnputil` reporting a missing driver for a mangled path |
| `@` | SHIFT + 2 | `"` | swapped with `"`, since UK and US differ on both |

A fourth, separate fault looked like a fifth: `send-key` with `hold-time: 60` left every key held
while the next arrived, so the guest's driver **reordered** characters. At 90+ characters, stray
characters appeared at the *start* of the line, which read as a line-length limit. It was
disproved by measuring — 110 characters arrived intact once explicit press/release events replaced
the held keys, where 90 had been mangled before.

**Decision.** Every entry in `wvm-host/src/input.rs` is verified by **typing into the guest and
reading the echo back**. The map is a measurement, not a derivation, and each entry that was once
wrong carries a comment naming the symptom it caused.

Two rules fall out of it:

1. **Never derive a keymap from the host's layout.** The host's layout is irrelevant; the guest's is
   the only one that matters, and they can differ silently.
2. **Refuse what cannot be produced rather than substituting.** No keycode yields a literal
   backslash on a UK layout. Mapping it to `backslash` (which yields `#`) is how a correct path
   became a mangled one, and the resulting error named the wrong problem entirely.

**Consequence.** Adding a character to the map without typing it into a guest is a guess wearing a
lookup table's clothes. The tests assert the mapping, but only a live guest confirms the guest's
layout is still UK.

---

## D-012 — Bulk bytes travel as base64 chunks inside JSON, in lockstep

**Date:** 2026-09-15
**Status:** accepted (push verified live; pull built, not yet verified)

**Context.** A transfer has the host as the reader for a push and the guest as the reader for a pull,
and there is no shared filesystem between them by design — the original project mounted host `/` into
the guest as a writable `Z:\` and this project exists partly to avoid that. So the bytes must travel
over the protocol.

**Decision, carrier.** Base64 inside JSON, **256 KiB of binary per chunk** (~341 KiB on the wire).
The 33% inflation is a non-argument at this size, and the property that matters was measured before
anything was built on it: a 341 KiB frame round-trips intact, reproducibly, five times out of five
(`scripts/probe-frame-size.py`, `wvm-ipc`'s `a_transfer_sized_frame_round_trips`).

**Alternatives rejected.**

- **Multiplexed binary frames** (a 1-byte type header before the length prefix). Zero inflation and
  more speed. Rejected because it needs a hand-written parser in the Rust host, the Rust guest *and*
  the Python reference client — and three implementations of one framing rule is where desync bugs
  live. `examples/wvm_client.py` gaining five lines instead of a state machine is the whole argument.
- **A side-channel data socket.** Rejected immediately: a second ephemeral port per transfer breaks
  the single-port boundary and widens the firewall surface for a control plane. Called out in the
  research as a discard.

**Decision, pacing.** Lockstep, one chunk per round trip:

```
host: read 256 KiB -> base64 -> send -> WAIT -> verify the byte count -> next
```

Not an optimisation. The host reads from a bare-metal disk and base64-encodes far faster than the
guest can deserialize JSON, decode base64, and flush through virtio-blk to a virtualised NTFS volume.
Streaming ahead fills the TCP buffers and then the guest's receive queue, and the failure is an
out-of-memory kill or a torn socket rather than anything legible. Round-tripping per chunk makes
network speed into disk speed and keeps memory flat at any file size.

**Decision, the offset is on every chunk.** The obvious design is append-in-arrival-order. It would
let a gap or an overlap produce a file of the **right length with the wrong contents** — and an
end-of-transfer length check cannot catch that, because by then the bytes are on disk and the count is
correct. Only a hash comparison would notice, after rewriting gigabytes to find out where it went
wrong. Naming the offset per chunk turns that into a loud error on the chunk that caused it.

**Decision, `Transfer` and the chunk verbs are separate.** `Transfer` names *what* is moving and opens
the destination or source; `TransferChunk` / `PullChunk` carry or request the bytes. Splitting them
means the guest validates a path once and then writes or reads chunks without re-resolving a path it
has already checked.

**Decision, the two directions are asymmetric code, not one mirrored function.** An earlier version of
`transfer::plan` treated push and pull as one operation, and that assumption hid a bug: it validated
the host's path against a root the guest cannot see, so every honest transfer was refused with a
correct-looking error. A pull has no host path at all, and a design that assumes symmetry cannot
express that.

**Consequence, and an open hole this work surfaced.** The guest answers one request at a time, so any
single request that blocks holds the entire control channel. `exec` defaulted to a **ten-minute**
timeout and the protocol did not expose it, so a hung command presented as a dead service rather than
a slow one — the host's own read timeout fires first. `timeout_ms` is now on `Exec` and the caller
decides. The general problem is not solved: a request that ignores its timeout would still hold the
channel, and a future revision should consider a watchdog that can abandon a request without killing
the service.

---

## D-013 — A timeout kills the process tree, via a Job Object on Windows

**Date:** 2026-09-15
**Status:** accepted, verified both ways against a live guest

**Context.** `exec` supports a caller-supplied timeout. When it fired, the guest called
`Child::kill()` — which on Windows is `TerminateProcess` on **one** process. Anything that process
had spawned survived it.

A command that shells out is the normal case, not the exotic one: a build script, a pipeline, an
agent running `cmd /c something.bat`. Timing out killed the shell and left its children running —
orphaned, invisible to the control channel, still holding memory and handles. Once is harmless;
repeatedly it exhausts the guest's resources, and the failure surfaces much later as an unrelated
problem. This is how a worker node gets bricked by its own workload.

**Finding.** The POSIX answer does not port. On Linux the recipe is `setpgid` at spawn plus
`kill(-pgid)`: signal the negative PID and the kernel delivers it to every member of the group.
Windows has no negative PID and no signal that reaches a tree, so there is nothing to translate —
the mechanism itself is absent.

**Decision.** A **Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, created before the child
and with the child assigned immediately after spawn. Dropping the last handle terminates every
process in the job, and **job membership is inherited** — which is what makes grandchildren die and
not merely children.

**Known window, recorded rather than described away.** Between `spawn` returning and
`AssignProcessToJobObject` succeeding there is a moment where the process exists outside the job. If
the timeout fired in that window the kill would miss, and the resulting failure would be rare and
unreproducible. `CREATE_SUSPENDED` + `AssignProcessToJobObject` + `ResumeThread` closes it
completely, and `std::process` does not expose that. The window is microseconds and the child
executes nothing meaningful inside it, so this is accepted for now and named here so the next person
can judge it rather than discover it.

**Verification, in both directions.** `scripts/test-timeout-tree-kill.py` spawns a script that
starts two grandchildren which outlive their parent, times it out, and counts survivors by asking the
guest for its process list.

- Job object active: `timed_out` reported, **zero survivors**, three repeat runs clean, channel
  still healthy afterwards.
- Job kill disabled: the channel **wedges outright**. The orphans hold the pipe handles open, so the
  drain threads never see EOF and the host times out entirely.

That second result is the point worth keeping: the leak is not merely "memory fills up eventually".
**A single hostile command can hang the control plane**, which is a far more immediate failure than
the resource exhaustion that prompted the fix.

**A test that nearly passed for the wrong reason.** The first version reported a clean pass having
tested nothing — `outcome: exited` rather than `timed_out`, in 82ms, with
`ERROR: Input redirection is not supported, exiting the process immediately.` on stderr. The obvious
Windows stand-in for `sleep 600` is `timeout /t 600`, and it **refuses to run when stdin is
redirected**, which the guest always does. Every delay exited instantly, no grandchild ever existed,
and every survivor count was zero for the absence of anything to count. `ping -n` works under
redirected stdin and is still a real child process.

**Consequence.** Any future guest-side operation that spawns processes must put them in a job. The
pattern is in `wvm-guest/src/job.rs` and is not specific to `exec`.

---

## D-014 — Quote nothing you pass to the guest; and if a claim is unverifiable, say so

**Date:** 2026-09-16
**Status:** accepted

**Context.** `scripts/test-transfer-roundtrip.py` hashes the pushed file inside the guest with
`certutil`, so the content check does not depend on the transfer path vouching for itself. On a
freshly pushed file it reported:

```
CertUtil: -hashfile command FAILED: 0x80070002 (WIN32: 2 ERROR_FILE_NOT_FOUND)
```

`dir` listed the file. `type` printed its contents. `copy` copied it. Only `certutil` could not open
it, and only for some files.

**Four hypotheses, all wrong, all falsified by measurement rather than argument:**

1. *Write timing* — a delay sweep from 0s to 10s changed nothing.
2. *NTFS flush* — the `eof` path already calls `flush()` then `sync_all()`, and the failure did not
   move with size or delay.
3. *Directory metadata not yet visible* — `dir` saw the file immediately; `copy` to a second path
   worked instantly.
4. *Defender scanning a freshly written file* — the most plausible-sounding, and it was killed by an
   exclusion test: `Add-MpPreference -ExclusionPath 'C:\ProgramData\wvm\staging'` changed **nothing**.
   A convincing mechanism that an experiment refutes is worse than no mechanism, because it arrives
   with confidence.

**The actual cause.** The path was passed **quoted**:

```
certutil -hashfile C:\...\file.bin SHA256      -> works
certutil -hashfile "C:\...\file.bin" SHA256    -> FILE_NOT_FOUND
```

The quote characters reach the process as part of the argument, and `certutil` treats them as part of
the filename. It then reports that the file does not exist, which is indistinguishable from the file
genuinely not being there. The clue that broke it open was `attrib` printing a doubled path
(`C:\C:\...`) — evidence about argument handling, not about the filesystem.

**Decision.** Paths passed to guest commands are **not quoted**. Where a path needs quoting for a
shell, that is a different problem and needs its own test.

**Related traps in the same family, each found the same way:**

- `&&` does not survive the exec layer: `cd /d X && certutil ...` returns exit code 1 with **empty
  stdout**. No error message. The command simply does not run.
- A relative filename fails, because the guest's default cwd is `C:\Windows\System32`.
- `timeout /t 600` refuses to run under the redirected stdin this harness always provides, and exits
  in milliseconds — which made a process-cleanup test count zero survivors for the absence of
  anything to count (D-013).

**The general rule this belongs to.** A negative result from a probe must be distinguishable from the
probe's own failure. `FILE_NOT_FOUND` looked like a fact about a file; it was a fact about an
argument. Four hypotheses were entertained before the instrument itself was suspected, and the
session's own running lesson says the instrument should have been first.

**Verification.** `scripts/verify-all.sh` runs eleven probes against a live guest and reports three
states — passed, failed, and **could not run** — because collapsing "could not run" into "passed" is
the defect this whole entry is about. All eleven pass.

---

## D-015 — No single request may hold the control channel

**Date:** 2026-09-16
**Status:** accepted, verified both ways against a live guest

**Context.** `exec` takes a caller-supplied timeout, so a hung COMMAND is bounded. That is one verb,
not the channel. The guest chose to serve one connection at a time, inline in the accept loop, so a
request that never returned for any other reason — a bug, a blocking path, a verb added later with no
timeout — held the connection, and the host could not tell a busy guest from a wedged one from a dead
one, because all three look like silence.

**Decision, in two halves — and the first half alone was not enough.**

1. **A request deadline.** Requests run on a worker and are abandoned if they overrun
   (`wvm-guest/src/deadline.rs`, default fifteen minutes, `WVM_REQUEST_DEADLINE_SECS` to override).
   The reply says plainly that the abandoned work was **not** cancelled, because Rust cannot safely
   cancel arbitrary code and it continues running. Claiming a clean stop would be a claim the guest
   cannot support.

2. **A thread per connection.** The first half fired correctly and answered the host — and the channel
   was *still* unavailable, because `dispatch::serve` then loops back to read the next request on that
   connection while the accept loop is still inside it. Measured: the first connection received a
   well-formed timeout reply, and a second connection got **nothing for thirty seconds.**

Bounded by `MAX_CONNECTIONS = 8`. "One thread per connection" with no ceiling is a
resource-exhaustion bug waiting for a peer that opens sockets in a loop.

**The measurement mistake worth recording.** Five attempts tested the WRONG PROCESS. A console guest
was started with a short deadline on the assumption it would take over the port; the Windows service
kept ownership, so every measurement was of a process running the default. The guest's own
environment eventually said so:

```
guest env WVM_REQUEST_DEADLINE_SECS: ''          <- never set
CommandLine : "...wvm-guest.exe" --service       <- the service, not the instance under test
```

Checking which process was being measured should have been the first step, not the fifth. The
override is now applied with `setx /M` and the service restarted, so the process under test is
provably the process serving the port.

**Verification, both directions.** `scripts/verify-request-deadline.py` holds one connection busy and
checks whether a second is still served — the property that matters, observable immediately, with no
fifteen-minute wait:

- Thread per connection: **6/6 second-connection requests answered, slowest 0.0s.**
- Inline serving (the fix reverted): **2 timeouts, slowest 10.0s, 4/6** — the listener is blocked by
  the connection it is serving.

`scripts/test-request-deadline.py` additionally exercises the deadline itself end to end against a
guest started with a short one, and asserts the reply names the deadline and states the work was not
cancelled.

**Consequence.** `scripts/clear-guest-deadline.py` exists to park the override somewhere harmless and
restore the service failure policy, because a test that leaves `restart/3600000` turns every future
crash into an hour of downtime, silently. It reports honestly when it cannot do what it was asked —
which is how the `reg delete` limitation below was found.

**Limit recorded.** The override variable cannot be **deleted** through the exec layer: the registry
key is `...\Control\Session Manager\Environment` and its space does not survive. Quoted, `reg` takes
the quote characters as part of the key name ("Invalid syntax"); unquoted, the path splits on the
space; and `Session Manager` has no 8.3 short name. All three measured. The variable is therefore
parked at a large explicit value instead, which is a better outcome anyway — an override that says
what it is beats one that is absent and indistinguishable from never having been set.

---

## D-016 — Why `lifecycle` is still a stub, and what the two options cost

**Date:** 2026-09-16
**Status:** resolved — Option B taken, and the vmstate parameter was misunderstood

**RESOLVED 2026-09-16.** Option B was implemented: the disk is declared with `-blockdev` and named
nodes, and the guest boots with 12/12 probes passing.

**The mistake that made this look impossible.** `vmstate` was assumed to be a SEPARATE block node
holding RAM, so one was created (file node, then qcow2 node, then a pre-sized file — three
variations). Every one failed with `vmstate block device 'X' does not exist`, *including for a node
that `query-named-block-nodes` plainly listed as present and writable*. That contradiction is the
tell that the parameter means something other than it appears to.

**The actual signature, from QEMU's own QAPI documentation in `qapi/migration.json`:**

```
-> { "execute": "snapshot-save",
     "arguments": {
        "job-id": "snapsave0",
        "tag": "my-snap",
        "vmstate": "disk0",           <-- the DISK node itself
        "devices": ["disk0", "disk1"]
     }
   }
```

**`vmstate` is the disk node.** The VM state is written INTO the disk's own qcow2 snapshot. There is
no separate vmstate device to create, and creating one produces an error that describes a node as
missing while it is visibly present.

With the documented form it works on the first attempt:

```
job: created -> running -> waiting -> pending -> concluded
error: None

info snapshots:
  ID   TAG          VM_SIZE      DATE
  2    my-snap      3.37 GiB     2026-09-16 12:57:50      <- RAM included
```

Compare with the internal snapshot attempted before the disk was renamed, which reported `0 B` and
could not be reverted while the VM was running. **`3.37 GiB` versus `0 B` is the whole difference
between a disk revert point and a restorable machine.**

**Process note.** Five forms were tried before reading the API documentation, and the first thing
that should have been consulted was the QAPI definition — it contains a complete worked example. The
correct order is: read the vendor's signature, then implement. Guessing at a parameter's meaning from
its name cost more time here than any code in the project so far.

**Context.** `lifecycle` is the last unimplemented verb. The assumption going in was that it is
plumbing: open the QMP socket that `wvm-host` already supervises and dispatch the snapshot commands.
Measurement says otherwise, so the findings are recorded here before any code is written.

**Finding 1 — `savevm`/`loadvm` are not QMP commands.** Queried against the live socket:
242 commands available, and neither appears. They are HMP commands. D-006 recorded this and chose
`snapshot-save`/`snapshot-load` instead; that decision holds.

**Finding 2 — `snapshot-save` needs a NAMED block node, and our disk has none.** It takes
`job-id`, `tag`, `vmstate`, `devices`, where `devices` is a list of node names. The disk is declared
with the legacy `-drive` interface:

```
-drive file=.../disk.qcow2,if=none,id=disk0,format=qcow2,...
virtio-blk-pci,drive=disk0,bootindex=1
```

`-drive` produces an anonymous node — QEMU generates `#block172`. Passing the device id fails with
`No block device node 'disk0'`; passing the generated name gets further but then fails with
`vmstate block device '...' does not exist`, because `vmstate` must itself be a block node rather
than a file path.

**Finding 3 — the job abstraction hides the reason.** `snapshot-save` returns immediately and reports
its outcome asynchronously as `JOB_STATUS_CHANGE` events. The failure sequence is
`created → running → aborting → concluded`, and **none of those events carries the error**. The
reason is only visible from `query-jobs`, which returns `error: "No block device node 'disk0'"`.
Anyone reading the event stream alone would see a job abort with no explanation. This cost four
attempts and a guessed device name before the error was found.

**Finding 4 — internal snapshots work today, but are disk-only.** `blockdev-snapshot-internal-sync`
takes a `device` rather than a node name and succeeded on the current VM:

```
info snapshots:
  ID      TAG               VM_SIZE    DATE                 VM_CLOCK
  --      wvm-internal-1       0 B     2026-09-16 12:39:37  0000:55:20.440
```

`VM_SIZE 0 B` is the tell, and attempting to revert to it confirms it:

```
loadvm wvm-internal-1
  -> "Error: This is a disk-only snapshot. Revert to it offline using qemu-img"
```

So it captures **disk state only, with no RAM**, and cannot be restored while the VM is running. It
is a revert point for a stopped guest, not a save-state.

**Finding 5 — `loadvm` is reachable through `human-monitor-command`.** HMP is available over QMP with
that one escape hatch, so the HMP-only verbs are not unreachable, only indirect. Worth knowing for
any future verb in the same position.

**The two options.**

*Option A — internal snapshots, as they work today.* No change to the VM's disk declaration. Creates
and deletes cleanly. **Cannot be restored live** and does not capture RAM, so "snapshot before
something risky, restore after" does not work: it would mean stopping the guest to revert, which
defeats the point of a control plane that exists to drive a running guest.

*Option B — declare the disk with `-blockdev` instead of `-drive`.* Nodes become explicitly named, and
full VM-state snapshots (`snapshot-save`/`snapshot-load`) become available with live restore and RAM
captured. This is the modern QEMU interface and the one the async job API is built around. **The cost
is that it changes how the guest's disk is declared, which touches the boot path** — the same path
that took the virtio-net and two-CD-on-one-IDE deadlocks to get right. It needs a regression check
that the guest still boots and that the existing 12 probes still pass, and it should be done as its
own change with the VM stopped, not folded into a verb implementation.

**Recommendation: Option B**, as a separate commit before `lifecycle` is written, because a
lifecycle verb that cannot restore a running VM is not the thing the verb exists for. But it is a
change to a working boot path, and that decision is Dave's rather than something to slip in.

---
