#!/usr/bin/env python3
"""Is the RESTORE the trigger, rather than the save?

WHY THIS IS THE TEST TO RUN NOW

Two results, side by side:

  * four save-only rounds under heavy disk load, each saving and then deleting the snapshot
    -> ZERO crashes
  * every crash observed this session happened inside a run that saved AND restored

That is a real difference and it was hiding in plain sight. The save writes device state into the
qcow2; the restore writes it BACK INTO THE DEVICE. A device-state restore is the more likely place
for a driver to be handed something it does not expect — the guest's storage stack resumes with
queues and outstanding requests as they were at save time.

A save-only test therefore cannot see this, which is exactly why the previous probes came back
clean. Testing save and restore separately is the whole point.

The test saves ONCE and then restores many times, so N restore attempts cost N x ~40 s rather than
N x (save + restore). Each round checks the crash-dump directory rather than the guest's silence,
because a bugcheck triggers an automatic reboot and a recovered guest looks healthy.

Usage: python3 scripts/probe-restore-crash.py [--restores 8] [--config ~/wvm/wvm-test.toml]
Exit:  0 if every restore was survived, 1 if the guest bugchecked, 2 if the probe could not run.
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

CONFIG_DEFAULT = str(Path.home() / "wvm/wvm-test.toml")
PORT = 48274


def guest(request, timeout=60):
    s = socket.create_connection(("127.0.0.1", PORT), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(request).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
        h = b""
        while len(h) < 4:
            c = s.recv(4 - len(h))
            if not c:
                return None
            h += c
        (n,) = struct.unpack(">I", h)
        body = b""
        while len(body) < n:
            c = s.recv(n - len(body))
            if not c:
                break
            body += c
        return json.loads(body)
    finally:
        s.close()


def dumps():
    r = guest({"op": "exec", "program": "cmd.exe", "args": ["/c", r"dir /b C:\Windows\Minidump"],
               "cwd": None, "require_allowlist": False, "timeout_ms": 60000}, timeout=90)
    if not r:
        return None
    return {ln.strip() for ln in (r.get("payload", {}).get("stdout") or "").splitlines()
            if ln.strip().endswith(".dmp")}


def alive(deadline_s=240):
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            if guest({"op": "hello", "protocol_version": 1, "client": "restoreprobe"}, timeout=10):
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


def wvm(*args, timeout=900):
    r = subprocess.run(["./target/release/wvm", *args], capture_output=True, text=True,
                       cwd=".", timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--restores", type=int, default=8)
    ap.add_argument("--config", default=CONFIG_DEFAULT)
    ap.add_argument("--tag", default="restore-loop")
    args = ap.parse_args()

    if not alive(60):
        print("  no guest — start the VM first")
        return 2

    before = dumps()
    if before is None:
        print("  cannot read the dump directory — no way to detect a crash")
        return 2
    print(f"  {len(before)} crash dump(s) before this run")

    code, out = wvm("vm", "snapshot", "save", args.tag, "--config", args.config)
    if code != 0:
        print(f"  save failed: {out.strip()[:140]}")
        return 2
    print(f"  saved '{args.tag}' once")

    crashes = 0
    for i in range(1, args.restores + 1):
        t0 = time.time()
        code, out = wvm("vm", "snapshot", "restore", args.tag, "--config", args.config)
        last = out.strip().splitlines()[-1] if out.strip() else "(no output)"

        answered = alive(240)
        after = dumps()
        new: set = set()
        if after is not None:
            new = {d for d in after if d not in before}

        status = "survived"
        if new:
            crashes += 1
            status = f"*** BUGCHECK: {sorted(new)} ***"
            before = after
        elif not answered:
            crashes += 1
            status = "*** unresponsive, no new dump ***"
        elif code != 0:
            status = f"restore reported failure: {last[:70]}"

        print(f"  restore {i}/{args.restores}: {time.time()-t0:5.0f}s  {status}")

        if crashes and not answered:
            print("  waiting for the guest to come back before continuing...")
            alive(300)

    print()
    if crashes:
        print(f"  {crashes} crash(es) in {args.restores} restore(s).")
        print("  -> the RESTORE is implicated, not the save. The save-only probes could never")
        print("     have seen this, which is why they came back clean.")
        return 1
    print(f"  no crash in {args.restores} restore(s).")
    print("  -> a restore on a quiet guest is not enough. Next variable: restore while the guest")
    print("     has I/O in flight, or with a larger vmstate.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
