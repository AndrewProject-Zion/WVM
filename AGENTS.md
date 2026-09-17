# WVM — instructions for an agent working in this repo

If you are an agent about to *use* WVM to drive a Windows guest, read `WVM.md` instead — it is the
protocol contract. This file is for an agent about to *change* this codebase.

## Before you claim anything works

```sh
./scripts/verify-all.sh
```

It reports three states, not two: `passed`, `failed`, and `COULD NOT RUN`. Treat the third as a
failure. Collapsing it into `passed` is how a machine that was never tested comes to look like one
that works, and that mistake is the reason this project has the runner at all.

After a docs edit, also run:

```sh
./scripts/check-docs.sh
```

It checks the README's claims against the build, and regenerates `WVM.md` from the capability list
to catch drift. A stale `WVM.md` fails it.

## The house rules, learned expensively

1. **Never assert from the shape of the work — measure it.** Tell to watch for: a confident
   explanation written into a code comment. Several of those turned out to be false.

2. **Prove a check can fail before trusting it.** A test that has never been seen red is a claim,
   not evidence. Revert the fix, watch it fail, restore. Three tests in this repo's history passed
   on broken code and one of them was cited in a commit message before anyone noticed.

3. **State what you are measuring.** Which process, which variable, which state. Five separate
   "failures" in one session were the instrument, not the code.

4. **A refusal is architecturally permanent or it is not, and the difference is worth stating.**
   `lifecycle` from the guest can never work — it is not a stub. The message says so and names the
   command that does work.

5. **No plaintext credentials, ever.** See `docs/DECISIONS.md`. Paths are confined to a staging
   root by design; the host filesystem is never exposed to the guest.

## Where things live

| | |
|---|---|
| `wvm-ipc/` | the wire protocol — request/response types and framing |
| `wvm-host/` | the daemon and CLI; `capabilities.rs` is the source `WVM.md` is generated from |
| `wvm-guest/` | the Windows service — a deliberately dumb executor with no policy of its own |
| `docs/DECISIONS.md` | every architectural call, including the wrong turns |
| `scripts/verify-all.sh` | the probe runner |

## Building

```sh
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest
```

The guest binary is cross-compiled from Linux. Deploy it with `sudo scripts/deploy-guest-binary.sh`
while the VM is **stopped** — it attaches the disk and hash-verifies the copy.
