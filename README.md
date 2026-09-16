# WVM — a headless execution sandbox for AI agents

Drive Windows 11 from a Linux terminal over a typed, hostile-peer-guarded JSON protocol.

A Rust control plane that lets a program or an agent drive a Windows VM as a typed tool: launch
processes, capture screenshots, inject input, move files, snapshot and restore. Headless by
default, every operation through a capability boundary and an append-only journal.

Give your agent a Windows guest instead of your host machine.

It is **not** a desktop-integration layer. That space is well served — WinBoat (22.8k stars),
WinPodX, winapps, LinOffice all put individual Windows windows on a Linux desktop via FreeRDP and
RemoteApp. WVM exists for the case they do not cover: **an agent or a program driving a Windows
guest over a typed protocol, headless, with an auditable capability boundary.**

**Status: M5 in progress.** `exec`, `capture`, `input`, `transfer push` and `transfer pull` are
implemented and verified against a live guest. `lifecycle` is an honest stub that reports
`not implemented in this build`.

## What works, verified

Driven from Linux, over the control channel, with no console, no keyboard and no guest-side agent:

```
hello    ->  {"status":"ready","protocol_version":1,"guest":"Windows (wvm-guest 0.1.0)"}

exec     ->  {"result":"process_output","outcome":"exited","code":0,
              "stdout":"\r\nMicrosoft Windows [Version 10.0.22631.2715]\r\n",
              "elapsed_ms":37}

capture  ->  a 1024x768 PNG of the live desktop, 608 distinct colours

input    ->  clicked (336,397) on the Firefox new tab page; Wikipedia loaded

transfer ->  1 MiB pushed in and pulled back, byte-identical both ways
```

A process launched inside Windows, its output captured, returned to the host. A screenshot of the
real desktop. A click that lands where you aimed. **And an artifact the guest produced, extracted
and hash-verified** — which is the difference between a sandbox and a roach motel.

| | |
|---|---|
| Protocol + framing | length-prefixed JSON, 4-byte big-endian, hostile-peer guard |
| Capability boundary | enforced at request **construction**, denials journalled |
| Audit journal | append-only JSON Lines, requests and denials at equal weight |
| QEMU supervision | start, suspend, shutdown, status, serial log |
| Windows install | tiny11, driven by keyboard injection alone, headless |
| Guest networking | e1000e (in-box driver), `10.0.2.15` |
| Guest service | Windows service, `StartServiceCtrlDispatcher`, auto-restart |
| Exec | launch, capture stdout/stderr, caller-supplied timeout, exit status |
| Exec cleanup | a timeout kills the whole process **tree**, not just the direct child |
| Capture | PNG of the live desktop, geometry assertion |
| Input | absolute pointer moves and clicks, UK-correct keys and text |
| Transfer | push and pull, chunked in lockstep, confined to a guest staging root |

## Not yet built

The protocol has verbs the guest answers honestly rather than optimistically. This still returns
`not implemented in this build`:

- **lifecycle** — snapshot and restore from the guest side

It has its plumbing in place and a test asserting a stub never reports success.

## Verifying it yourself

```
./scripts/verify-all.sh              # everything, against a live guest
./scripts/verify-all.sh --offline    # only what needs no guest
./scripts/verify-all.sh --list       # what runs, and why
```

Eleven probes, and a result with three states rather than two: **passed**, **failed**, and **could
not run**. That last one is not a pass. A probe that never executed has verified nothing, and
conflating the two is how a build ships on the strength of a check that did not happen.

Every bug in this project's history was found by one of these, and every false claim was caught by
one — so they are wired together rather than left to whoever remembers to run them.

## Known gaps, recorded rather than hidden

- **A request that ignores its deadline still holds the channel.** The guest answers one request at a
  time, and `exec` takes a caller-supplied timeout, so a hung command is bounded. But the general
  case is not solved: a request that neither returns nor respects its own deadline occupies the
  control channel until something kills it. A watchdog able to abandon a request without restarting
  the service is the follow-up (D-013).
- **The tree-kill has a microseconds-wide window.** The child is assigned to its job object
  immediately after `spawn`, so there is a moment where a process exists outside the job. Closing it
  needs `CREATE_SUSPENDED` + `AssignProcessToJobObject` + `ResumeThread`, which `std::process` does
  not expose. Named rather than described away — see D-013.
- **A guest without a display cannot be captured.** Capture runs against the QEMU framebuffer on the
  host, so it needs QEMU to be rendering. That is inherent, not a bug — and it is why capture is
  host-side at all (see D-009).
- **`lifecycle` is not built.** Snapshot and restore from the guest side still return
  `not implemented in this build`. The host-side equivalents work — see `docs/VM-CONFIG.md`.
- **Input cannot reach every widget.** A page-level overlay that fights synthetic focus may take
  focus and still ignore injected keys. Ordinary Windows controls and browser chrome accept input
  reliably; this is recorded as an observed boundary rather than explained away.

## Quickstart

This walks from a bare Linux host to driving a Windows guest. It takes about an hour, most of it
the Windows install.

### 0. What you need

- **Linux host** with KVM. `/dev/kvm` must be openable by your user. On many distributions this
  needs you in the `kvm` group; check with `test -r /dev/kvm && test -w /dev/kvm && echo ok`.
- **QEMU** 8 or newer, **Rust** 1.70+, and **cargo**.
- **A Windows ISO.** Tiny11 is recommended and is what this project is developed against — see
  [Why Tiny11](#why-tiny11) for the reasoning and where to get it.
- **The virtio-win driver ISO**, from
  `https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso`.
  You need this even though the guest ends up needing no virtio drivers at runtime: it is what
  makes the OS disk visible to Windows Setup.
- **~40 GB free disk** (a 64 GiB sparse disk and a Windows install).

### 1. Check the host can run this

```sh
cargo run --release -p wvm-host -- doctor
```

This reports KVM, QEMU, the cross-compile target and the toolchain, and it **attempts** each
operation rather than inspecting metadata. If it says something is missing, fix that first — every
later step assumes it.

### 2. Cross-compile the guest agent

```sh
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest
```

You need `x86_64-w64-mingw32-gcc` installed as the linker. On Debian/Ubuntu:
`sudo apt install gcc-mingw-w64-x86-64`. The result is
`target/x86_64-pc-windows-gnu/release/wvm-guest.exe`, a little under 600 KB. It links only against
core Windows DLLs — `KERNEL32`, `msvcrt`, `ntdll`, `WS2_32` — so it needs no Visual C++
redistributable in the guest.

### 3. Generate the VM configuration

```sh
./scripts/prepare-install.sh --iso /path/to/tiny11.iso
```

This writes two files under `~/wvm/`: `install.toml` (the installer ISO attached) and `wvm.toml`
(the long-lived profile, no installer). **Read them.** They are the whole VM definition and they are
short.

### 4. Install Windows, headless

```sh
wvm vm cmdline --config ~/wvm/install.toml   # read the command line before running it
wvm vm start   --config ~/wvm/install.toml --timeout 300
wvm vm log     --config ~/wvm/install.toml --lines 60
```

**Windows Setup will not see the disk on the first screen.** That is expected and it is the
[catch-22](#the-virtio-storage-catch-22): the OS disk is on `virtio-blk` for speed, and Setup needs
a driver it can only load from a disc on a bus it can already read. When Setup offers no disk,
click **Load driver → Browse → the driver disc → `viostor` → `w11` → `amd64`**. The disk appears.

Complete the install through the GUI. Watch it with a window if you prefer:

```sh
python3 scripts/vm-with-display.py --display gtk /home/andy/wvm/install.toml
```

### 5. Boot the installed guest

```sh
./scripts/start-windows.sh              # windowed
./scripts/start-windows.sh --headless   # no display
./scripts/vm-health.sh                  # is anything actually wrong?
```

### 6. Install the guest service

Inside the guest, **in an elevated PowerShell**:

```powershell
sc.exe create wvm-guest binPath= "C:\Program Files\wvm\wvm-guest.exe" --service start= auto
sc.exe start  wvm-guest
sc.exe query  wvm-guest
```

`scripts/install-guest-service.ps1` does this plus the firewall rule, and is the path to prefer —
copy the binary and the script into the guest, then run the script elevated. Full detail, including
how to get files into the guest, is in `docs/WINDOWS-INSTALL-STATUS.md`.

Note `sc.exe`, not `sc`: in PowerShell `sc` is an alias for `Set-Content` and will fail with a
confusing "positional parameter" error.

### 7. Drive it

```sh
python3 scripts/talk-to-guest.py hello
python3 scripts/talk-to-guest.py exec -- cmd.exe /c ver

# the host binary also does capture, input and file transfer
wvm vm capture  --config ~/wvm/wvm.toml --out /tmp/screen.png --expect 1024x768
wvm vm input    click --config ~/wvm/wvm.toml 336 397
wvm vm input    text  --config ~/wvm/wvm.toml 'hello from the host'

# move a file in, and get an artifact back out
wvm vm transfer push --config ~/wvm/wvm.toml ./payload.bin 'C:\ProgramData\wvm\staging\payload.bin'
wvm vm transfer pull --config ~/wvm/wvm.toml 'C:\ProgramData\wvm\staging\result.bin' ./result.bin
```

Note the argument order for `pull`: the names are written for a push, so they **invert**. `SOURCE`
is the **guest** file being fetched and `GUEST_PATH` is the **host** destination. Getting it
backwards sends your local path to the guest, which refuses it as outside its staging root — an
error that looks like a boundary problem rather than a swapped argument.

## Why Tiny11

**Tiny11** is Windows 11 Pro with the consumer surface stripped out. Around 3 GB installed, versus
20 GB+ for a stock ISO once telemetry, Edge components and the background service set are included.

Tiny11 was chosen for an agent sandbox specifically, not for novelty:

- **Everything the control plane needs is present.** Win32 APIs, the Service Control Manager, and
  the interactive Session 1 desktop subsystem. Those are the parts that matter, and none of them
  are removed.
- **Less to move and less to watch.** For an ephemeral sandbox, baseline memory footprint and I/O
  latency matter more than features nobody will use.
- **Fewer background services means fewer things that can fire mid-operation**, and a shorter boot.

The trade is that Tiny11 is a community build rather than an official Microsoft image. For a
disposable sandbox that is the right trade; for anything holding data you care about, install from
an official ISO and expect a larger, slower guest.

**Verified against:** `Microsoft Windows [Version 10.0.22631.2715]` — the string the guest returns
from `exec -- cmd.exe /c ver`, which is the honest way to confirm what you actually built.

## The virtio storage catch-22

Windows Setup cannot see the OS disk, because that disk is on `virtio-blk` for speed and Setup has
no virtio driver yet. So a second disc carries the driver.

**The driver disc must be on a bus Setup can already read — AHCI, not virtio.** Attaching the driver
disc to a virtio controller is a genuine circular dependency: Setup cannot read the disc that
contains the driver that would let it read a disc. The symptom is "No device drivers were found"
with nothing but `Boot(X:)` to browse, which looks like a broken image rather than a bus mismatch.

```
OS disk          virtio-blk          fast, invisible to Setup until the driver loads
driver ISO       AHCI port 0         readable by Setup, because AHCI is in-box
installer ISO    AHCI port 1         a separate port, not a second unit on the first
```

Both discs need **separate AHCI ports**. One IDE unit cannot carry two CD-ROMs on q35.

Full detail, including how each of these was found, is in `docs/WINDOWS-INSTALL-STATUS.md`.

## Execution modes

### Headless (the default)

No display is attached and the framebuffer is not instantiated. This is the primitive: a typed,
auditable channel with no UI in the loop.

```sh
./scripts/start-windows.sh --headless
python3 scripts/talk-to-guest.py exec -- cmd.exe /c ver
```

Lower memory footprint, faster boot, and nothing in the loop that a human is expected to read. Use
this for agent execution. Capture still works here — it reads the framebuffer QEMU is rendering even
when no window is shown.

### Windowed (visual telemetry)

Attach a display when you want to watch what the agent is doing — absolute pointer positioning, a UI
that did not respond as expected, a guest mid-boot.

```sh
./scripts/start-windows.sh                    # GTK window on the host
python3 scripts/vm-with-display.py --display vnc /home/andy/wvm/wvm.toml
```

For VNC, point Remmina (or any viewer) at `127.0.0.1:5900` — no password, so bind it to loopback
only. `Ctrl+Alt+G` releases the mouse and keyboard from a GTK window.

You will see the exact desktop the agent is manipulating, because input arrives as **emulated USB
hardware** (`usb-tablet`) rather than through any Windows API, and the guest's own Session 1 desktop
renders it. That indirection is what makes headless `input` work at all — see
[why input is emulated hardware](#why-input-is-emulated-hardware).

## Two more things that will bite you

Both are covered above; they are repeated here because they cost the most time and each presents as
something else entirely.

**A firmware mismatch is silent.** An unbootable disk under the wrong firmware produces no error —
the firmware just shows its boot menu, which looks like "the install failed". `wvm vm start` reads
the partition table and refuses a mismatch by name rather than letting you debug a symptom.

**A TCP connect to the guest port proves nothing.** QEMU's user-mode networking completes the
handshake on the host's behalf and only then attempts delivery, so a connect succeeds against a
guest with nothing listening. Use `scripts/talk-to-guest.py`, which exchanges real frames and says
so in its output.

## Why input is emulated hardware

This is the design decision that makes most of the tool possible, so it is worth stating plainly.

**A Windows service cannot inject input into the desktop.** Services run in **session 0**, an
isolated session with no desktop, and `SendInput` from there cannot reach session 1 where the user
(and any GUI application) lives. The only supported route is `CreateProcessAsUser` with
`lpDesktop = "winsta0\default"`, which needs `SE_TCB_NAME` and spawns a process per event.

**Emulated hardware has no such restriction.** Input sent over QMP arrives as PS/2 and USB device
events, and the Windows kernel delivers those to whichever session owns the **active console** —
session 1. No Windows API is involved, so the isolation boundary never applies.

The same reasoning applies to screen capture. The guest cannot screenshot its own desktop from
session 0, so capture reads QEMU's framebuffer on the host instead (D-009).

Practical consequences:

- `input` and `capture` need QEMU running. They are host-side operations.
- A guest with no display device cannot be captured at all — honestly reported, rather than
  returning a black image that looks like a dark screen.
- Absolute pointer positioning needs a `usb-tablet` on a USB controller. Without one QEMU exposes
  only a relative mouse, and a coordinate cannot be expressed at all.

Full reasoning in `docs/DECISIONS.md` D-009, D-010 and D-011.

## Why the odd decisions

Recorded so they are not re-litigated. Full reasoning in `docs/DECISIONS.md`.

- **TCP, not virtio-vsock** (D-002). Windows has no native `AF_VSOCK`; the virtio-win `viosock`
  driver was withheld for years and only entered the release at build 285, with a field failure
  mode of `Connection reset by peer`. Both shipping projects in this space rejected it.
- **Headless by default** (D-004). The plane must work with no display attached.
- **A diagnostic attempts the operation** (D-005). `/dev/kvm` reported as inaccessible because the
  user was not in the `kvm` group — while an ACL made it openable. Metadata lied; attempting the
  open told the truth.
- **e1000e, not virtio-net.** virtio-net is faster and needs a driver installed *inside* the guest
  before any adapter exists. On a fresh install the guest enumerated no network interface while
  every host-side check reported healthy — the forward was bound, the device was attached, and a
  TCP connect succeeded. e1000e uses the Windows in-box driver.
- **Capture on the host, not in the guest** (D-009). Session 0 isolation, as above.
- **Transfers are chunked at 256 KiB** (see `wvm-guest/src/fsio.rs`). Not because the frame limit
  requires it — frames carry up to 64 MiB — but because one frame per file means the whole file in
  memory on both sides at once, and a transfer that dies halfway would be indistinguishable from a
  small successful one.
- **The keymap is measured, never derived** (D-011). It was written for a US keyboard on a UK guest,
  and each wrong entry presented as a different fault: a bad quote looked like a permissions
  problem, a bad backslash looked like a missing driver.

## Layout

```
wvm-ipc/     wire protocol and framing — shared by host and guest, so they cannot desync
wvm-host/    the Linux daemon: CLI, QEMU supervision, control socket, policy gate, journal
wvm-guest/   the Windows service: transport, dispatch, Win32 execution, path containment
docs/        design record — decisions, verified environment, build plan, install status
scripts/     tooling, each written for a specific failure that cost time
examples/    a dependency-free Python client, and an executable demonstration of the gate
reference/   the original project this was seeded from, read-only
```

## The client

`examples/wvm_client.py` is a second, independent implementation of the wire format — stdlib only.
A protocol with one implementation has no way to notice that its own encoder and decoder
disagree; two implementations surface that immediately.

```sh
python3 examples/wvm_client.py boundary   # starts a daemon, then tries to exceed its grant
```

## Licence

MIT.
