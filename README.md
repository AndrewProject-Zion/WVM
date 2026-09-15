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

**Status: M5 in progress.** `exec`, `capture` and `input` are implemented and verified against a
live guest. `transfer` and `lifecycle` are honest stubs that report
`not implemented in this build` — see [Not yet built](#not-yet-built).

## What works, verified

Driven from Linux, over the control channel, with no console, no keyboard and no guest-side agent:

```
hello   ->  {"status":"ready","protocol_version":1,"guest":"Windows (wvm-guest 0.1.0)"}

exec    ->  {"result":"process_output","outcome":"exited","code":0,
             "stdout":"\r\nMicrosoft Windows [Version 10.0.22631.2715]\r\n",
             "elapsed_ms":37}

capture ->  a 1024x768 PNG of the live desktop, 608 distinct colours

input   ->  clicked (336,397) on the Firefox new tab page; Wikipedia loaded
```

A process launched inside Windows, its output captured, returned to the host. A screenshot of the
real desktop. A click that lands where you aimed.

| | |
|---|---|
| Protocol + framing | length-prefixed JSON, 4-byte big-endian, hostile-peer guard |
| Capability boundary | enforced at request **construction**, denials journalled |
| Audit journal | append-only JSON Lines, requests and denials at equal weight |
| QEMU supervision | start, suspend, shutdown, status, serial log |
| Windows install | tiny11, driven by keyboard injection alone, headless |
| Guest networking | e1000e (in-box driver), `10.0.2.15` |
| Guest service | Windows service, `StartServiceCtrlDispatcher`, auto-restart |
| Exec | launch, capture stdout/stderr, timeout, exit status |
| Capture | PNG of the live desktop, geometry assertion |
| Input | absolute pointer moves and clicks, UK-correct keys and text |

## Not yet built

The protocol has verbs the guest answers honestly rather than optimistically. These still return
`not implemented in this build`:

- **transfer** — move files between host and guest
- **lifecycle** — snapshot and restore from the guest side

Each has its plumbing in place and a test asserting a stub never reports success.

## Known gaps, recorded rather than hidden

- **`transfer` payloads are unchunked.** When this verb lands, a large file over a raw TCP socket
  needs chunking and backpressure, or it will meet the same buffer exhaustion that WVM-01 caused in
  `exec`. The framing carries one JSON message per frame, so a multi-megabyte file cannot travel as
  a single `Payload`.
- **A guest without a display cannot be captured.** Capture runs against the QEMU framebuffer on the
  host, so it needs QEMU to be rendering. That is inherent, not a bug — and it is why capture is
  host-side at all (see D-009).

## Quickstart

Requires QEMU, KVM, Rust, and a Windows ISO the guest can be installed from.

```sh
# 1. Check the host can run this at all
cargo run --release -p wvm-host -- doctor

# 2. Cross-compile the Windows guest from Linux
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest

# 3. Generate the VM configs (writes install.toml and wvm.toml)
./scripts/prepare-install.sh --iso /path/to/windows.iso

# 4. Install Windows. Read the driver steps below first.
wvm vm cmdline --config ~/wvm/install.toml    # read before running
wvm vm start   --config ~/wvm/install.toml --timeout 300
wvm vm log     --config ~/wvm/install.toml --lines 60

# 5. After install, boot from disk
./scripts/start-windows.sh

# 6. Install the guest service (inside the guest, elevated)
#    see docs/WINDOWS-INSTALL-STATUS.md

# 7. Drive it
python3 scripts/talk-to-guest.py hello
python3 scripts/talk-to-guest.py exec -- cmd.exe /c ver

# or with the host binary, which also does capture and input
wvm vm capture --config ~/wvm/wvm.toml --out /tmp/screen.png --expect 1024x768
wvm vm input click --config ~/wvm/wvm.toml 336 397
wvm vm input text  --config ~/wvm/wvm.toml 'hello from the host'
```

## Two things that will bite you

**The OS disk is on virtio-blk, so Windows Setup cannot see it** until the virtio storage driver is
loaded from a second disc. "No device drivers were found" is the expected state before that step,
not a fault. The installer must point at `viostor` → `w11` → `amd64` on the driver disc.

**The driver disc must be on a bus Windows can already read.** This was a genuine circular
dependency during development: the disc carrying the virtio driver was attached to a virtio
controller, making it unreadable to the very Setup that needed the driver. Both discs now sit on
separate AHCI ports; the OS disk stays on virtio-blk for speed.

Details, including the firmware-mismatch trap, in `docs/WINDOWS-INSTALL-STATUS.md`.

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
