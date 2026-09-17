#!/usr/bin/env python3
"""Does a guest TRIM crash the machine? A one-minute test for the strongest current hypothesis.

THE HYPOTHESIS

The guest disk is declared with `-blockdev ...,discard=unmap`, which advertises discard to Windows:
when Windows trims, QEMU unmaps blocks, punching holes in the qcow2 while the disk is live. Every
bugcheck on this machine post-dates that declaration — including one that happened BEFORE any
snapshot had ever been saved — so the snapshot may be incidental and the trigger may be the discard
path all along.

A TRIM is guest-initiated and rare, which fits a fault that appears occasionally rather than always.

WHY THIS TEST IS WORTH MORE THAN A SAVE A/B

Saves crash roughly one time in five, so testing a fix by saving needs fifteen or more samples per
arm and a written-down decision rule, or it produces a confident wrong answer. A TRIM can be issued
on demand, so a single round takes about a minute. If retrimming crashes the guest, the hypothesis
is confirmed in one run and the fix — dropping `discard=unmap` — can be verified the same way.

THE VERDICT IS THE DUMP SET, not the guest's silence. A bugcheck here triggers an automatic reboot,
so the guest can be answering again two minutes later and look perfectly healthy. Only a new file in
the dump directory shows that anything happened.

Usage: python3 scripts/probe-trim-crash.py [--rounds 3]
Exit:  0 if every retrim was survived, 1 if the guest bugchecked, 2 if the probe could not run.
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
PORT = 48274
STAGE = r"C:\ProgramData\wvm\staging"
SCRIPT = rf"{STAGE}\guest-retrim.ps1"


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


def alive(deadline_s=200):
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            if guest({"op": "hello", "protocol_version": 1, "client": "trimprobe"}, timeout=10):
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=3)
    args = ap.parse_args()

    if not alive(30):
        print("  no guest — start the VM first")
        return 2

    p = subprocess.run(["./target/release/wvm", "vm", "transfer", "push", "--overwrite",
                        "--config", CONFIG, "./scripts/guest-retrim.ps1", SCRIPT],
                       capture_output=True, text=True, cwd=".", timeout=120)
    if p.returncode != 0:
        print(f"  could not place the retrim script: {(p.stderr or p.stdout).strip()[:120]}")
        return 2
    print("  retrim script in place")

    before = dumps()
    if before is None:
        print("  could not read the dump directory — cannot reach a verdict")
        return 2
    print(f"  {len(before)} crash dump(s) before this run")

    crashes = 0
    for i in range(1, args.rounds + 1):
        print(f"\n--- retrim {i}/{args.rounds} ---")
        t0 = time.time()
        r = guest({"op": "exec", "program": "powershell.exe",
                   "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", SCRIPT],
                   "cwd": None, "require_allowlist": False, "timeout_ms": 300000},
                  timeout=330)
        elapsed = time.time() - t0

        payload = (r or {}).get("payload", {})
        out = (payload.get("stdout") or "").replace("\x00", "")
        code = payload.get("code")

        # Report the interesting lines only; the script prints a lot of PowerShell noise.
        for line in out.splitlines():
            low = line.strip()
            if any(k in low for k in ("DisableDeleteNotify", "retrim completed", "retrim FAILED",
                                      "DUMP COUNT", "RETRIM SCRIPT FINISHED", "SizeRemaining")):
                print(f"    {low[:100]}")

        print(f"  exec returned code={code} after {elapsed:.0f}s")

        if not alive(200):
            print("  the guest is not answering after the retrim")

        after = dumps()
        if after is None:
            print("  could not read the dump directory after the retrim")
            return 2
        new = after - before
        if new:
            crashes += 1
            print(f"  *** BUGCHECK: new dump(s) {sorted(new)} ***")
            before = after
        elif code != 0 or "RETRIM SCRIPT FINISHED" not in out:
            print("  the retrim did not complete cleanly — result is inconclusive for this round")
        else:
            print("  survived, no new dump")

    print()
    if crashes:
        print(f"  {crashes} crash(es) in {args.rounds} retrim(s).")
        print("  -> the DISCARD path is implicated. Next: drop `discard=unmap` and re-run this.")
        return 1
    print(f"  no crash in {args.rounds} retrim(s).")
    print("  -> a forced TRIM alone is not enough to trigger it. The discard path is NOT confirmed;")
    print("     the snapshot's device-state save remains the leading candidate.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
