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



## M3 — QEMU supervision

- [ ] Launch a VM from a config file with a verified command line
- [ ] Lifecycle: start / suspend / resume / snapshot / restore / destroy
- [ ] Health: detect a guest that has stopped responding and report it
- [ ] Idle suspend to return resources to the host

**Verification:** VM boots from a scripted command line; suspend and resume round-trip; snapshot
restore returns the guest to the recorded state.

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
