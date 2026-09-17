#!/usr/bin/env python3
"""Reproduce the snapshot bugcheck deliberately, so a parseable dump is captured.

WHERE THIS STANDS

The guest bugchecks 0x50 PAGE_FAULT_IN_NONPAGED_AREA at a fixed code offset. Established so far, by
measurement rather than argument:

  * a bare stop/cont for 60 seconds does NOT reproduce it   -> not the freeze
  * vmstate SIZE is not the trigger (a 2.96 GiB save crashed; a 2.90 GiB save did not)
  * memory pressure is absent (12 GB available, PSI 0.00)
  * the storage driver is current (viostor 100.103.104.30200, July 2026)

The four existing dumps are PAGEDU64 kernel dumps, which carry no directly-readable module list.
`CrashDumpEnabled` is 3 (small memory dump), so a NEW crash should write an MDMP minidump, which
does carry the loaded-driver list with base addresses — and that names the faulting driver instead
of leaving it a guess.

So the job here is simply to make the crash happen again, on purpose, and stop.

Each iteration saves under a fresh tag and then deletes it, so the disk does not accumulate GB of
machine state — accumulated snapshots were measured to slow saves from 6.5 seconds to 57, and a slow
save makes every iteration take minutes.

Usage: python3 scripts/repro-snapshot-crash.py [--rounds 6] [--port 48274]
Exit:  0 if no crash occurred, 1 if one did (and the new dump is named).
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

CONFIG = str(Path.home() / "wvm/wvm.toml")
STAGE = r"C:\ProgramData\wvm\staging"


def guest(request, timeout=90):
    s = socket.create_connection(("127.0.0.1", 48274), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(request).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
        header = b""
        while len(header) < 4:
            part = s.recv(4 - len(header))
            if not part:
                return None
            header += part
        (n,) = struct.unpack(">I", header)
        body = b""
        while len(body) < n:
            part = s.recv(n - len(body))
            if not part:
                break
            body += part
        return json.loads(body)
    finally:
        s.close()


def run_on_guest(command, timeout=120):
    r = guest({"op": "exec", "program": "cmd.exe", "args": ["/c", command], "cwd": None,
               "require_allowlist": False, "timeout_ms": timeout * 1000}, timeout=timeout + 20)
    return (r or {}).get("payload", {})


def dumps():
    p = run_on_guest(r"dir /b C:\Windows\Minidump")
    return {ln.strip() for ln in (p.get("stdout") or "").splitlines() if ln.strip().endswith(".dmp")}


def alive(deadline_s=240):
    """Wait for the guest to answer. The freeze makes a slow answer normal; a dead one is not."""
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            r = guest({"op": "hello", "protocol_version": 1, "client": "repro"}, timeout=10)
            if r and r.get("status") == "ready":
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
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--port", type=int, default=48274)
    args = ap.parse_args()

    if not alive(60):
        print("  no guest — start the VM first")
        return 2

    before = dumps()
    print(f"  guest alive; {len(before)} existing dump(s)")
    print()

    for i in range(1, args.rounds + 1):
        tag = f"repro-{i}"
        print(f"--- round {i}/{args.rounds}: snapshot '{tag}' ---")

        code, out = wvm("vm", "snapshot", "save", tag, "--config", CONFIG)
        last = out.strip().splitlines()[-1] if out.strip() else "(no output)"
        print(f"    {last[:110]}")

        # The crash shows up as the guest failing to answer afterwards. Waiting is right: the freeze
        # alone can run to a minute, and calling that a crash is the mistake that started all this.
        if not alive(240):
            after = set()
            print(f"    GUEST DID NOT ANSWER after the save")
            print()
            print("  A crash is likely. Waiting for it to reboot so the dump can be read...")
            # Windows is configured to restart after a bugcheck; give it time.
            for _ in range(20):
                time.sleep(15)
                if alive(30):
                    after = dumps()
                    break
            new = after - before
            print()
            if new:
                print(f"  NEW DUMP(S): {sorted(new)}")
            if not after:
                print("  the guest did not come back within 5 minutes of the save")
            return 1

        print("    guest answering — deleting the snapshot to keep saves fast")
        wvm("vm", "snapshot", "delete", tag, "--config", CONFIG)

    print()
    print(f"  No crash in {args.rounds} consecutive saves. Not reproduced this time.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
