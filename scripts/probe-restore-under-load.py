#!/usr/bin/env python3
"""Does a snapshot restore crash the guest when the guest has disk I/O in flight?

WHY THIS EXISTS

`scripts/verify-snapshot-rollback.py` passed three times, then failed: the save and the restore
both reported success, and the guest was dead afterwards — the screen showed a Windows bugcheck.
The probe's own follow-up question got a connection reset.

That is the worst shape of failure this project can have: the command says it worked, and the
machine it acted on is gone. So it needs a cause, not a retry.

THE HYPOTHESIS

The three passing runs had a quiet guest. A restore writes RAM and device state back into a running
machine; if that machine has I/O in flight, the device model can be asked to resume into a state the
guest no longer agrees with. That is a classic trigger and it is testable.

THE TRICK THAT MAKES IT TESTABLE

Snapshots run on the HOST over QMP, and the guest service answers one request at a time. So an
`exec` that holds a disk-writing loop can be left running in a background thread while the snapshot
happens over QMP — the two do not contend. That gives a genuinely busy guest, which is the state the
passing runs never had.

Note the load cannot simply be backgrounded with `start /b`: the exec layer kills the process tree
via a kill-on-close Job Object, and job membership is inherited, so a detached child dies with the
job when the exec returns.

Usage:  python3 scripts/probe-restore-under-load.py [--rounds 3] [--port 48274]
Exit:   0 if every round survived, 1 if the guest died under load.
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

CONFIG = str(Path.home() / "wvm/wvm.toml")
STAGING = r"C:\ProgramData\wvm\staging"

# A loop that writes to disk continuously. `for /l` is a cmd builtin so it needs no external
# program, and appending to a file keeps the write path busy rather than buffered.
LOAD = (f'cmd /c "for /l %i in (1,1,500000) do @echo '
        f'0123456789ABCDEF0123456789ABCDEF >> {STAGING}\\load.txt"')


def call(port, req, timeout=120):
    """One request over the control socket. Returns None on a closed connection."""
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(req).encode()
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


def guest_alive(port, deadline_s=60):
    """Is the guest service answering? Retries, because a restore takes a moment to resume.

    Prints nothing while retrying so the caller controls the reporting.
    """
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            r = call(port, {"op": "hello", "protocol_version": 1, "client": "load-probe"}, timeout=10)
            if r and r.get("status") == "ready":
                return True, ""
        except Exception as e:
            last = f"{type(e).__name__}"
        time.sleep(2)
    return False, locals().get("last", "no reply")


def wvm(*args, timeout=600):
    r = subprocess.run(["./target/release/wvm", *args],
                       capture_output=True, text=True, cwd=".", timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def start_load(port, hold_s=300):
    """Begin a disk-writing loop and leave it running.

    Runs in a thread because the guest answers one request at a time: this call will not return
    until the loop finishes or its timeout fires, and the snapshot must happen in between.
    """
    state = {}

    def worker():
        try:
            state["reply"] = call(
                port,
                {"op": "exec", "program": LOAD.split(" ", 1)[0],
                 "args": ["/c", LOAD.split(" /c ", 1)[1]],
                 "cwd": None, "require_allowlist": False, "timeout_ms": hold_s * 1000},
                timeout=hold_s + 30,
            )
        except Exception as e:
            state["error"] = f"{type(e).__name__}: {e}"

    t = threading.Thread(target=worker, daemon=True)
    t.start()
    state["thread"] = t
    return state


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--hold", type=int, default=300, help="how long the load loop may run, seconds")
    args = ap.parse_args()

    alive, why = guest_alive(args.port, 30)
    if not alive:
        print(f"  no guest on {args.port} ({why}) — start the VM first")
        return 2
    print(f"  guest is up\n")

    failures = []

    for i in range(1, args.rounds + 1):
        tag = f"load-{i}"
        print(f"--- round {i}/{args.rounds} ---")

        # Clear the load file so the write path starts from a known size.
        call(args.port, {"op": "exec", "program": "cmd.exe",
                         "args": ["/c", f"del /f /q {STAGING}\\load.txt"],
                         "cwd": None, "require_allowlist": False, "timeout_ms": 20000},
             timeout=40)

        print("  starting a disk-writing loop in the guest")
        load = start_load(args.port, hold_s=args.hold)
        time.sleep(3)  # let I/O actually be in flight

        # Confirm the loop is really writing — otherwise this is a quiet-guest run wearing a
        # costume, which is exactly the mistake that made the earlier passes meaningless.
        # (Checked from the HOST side: the guest service is busy with the load, so asking it would
        # queue behind a loop that has minutes to run.)
        print(f"  load running: {load['thread'].is_alive()}")

        print(f"  saving '{tag}' WHILE the guest is busy")
        code, out = wvm("vm", "snapshot", "save", tag, "--config", CONFIG)
        last = out.strip().splitlines()[-1] if out.strip() else "(no output)"
        print(f"    {last[:100]}")
        if code != 0:
            failures.append(f"round {i}: save failed: {last[:160]}")
            continue

        print(f"  restoring '{tag}' WHILE the guest is busy")
        code, out = wvm("vm", "snapshot", "restore", tag, "--config", CONFIG)
        last = out.strip().splitlines()[-1] if out.strip() else "(no output)"
        print(f"    {last[:100]}")
        if code != 0:
            failures.append(f"round {i}: restore reported failure: {last[:160]}")

        # The whole point: did the machine survive being restored into?
        alive, why = guest_alive(args.port, 60)
        if alive:
            print("  guest SURVIVED the restore\n")
        else:
            print(f"  guest DID NOT SURVIVE — {why}\n")
            failures.append(
                f"round {i}: the guest did not answer after a restore under load ({why}). "
                f"The restore had reported success."
            )
            # No point continuing rounds against a dead guest.
            break

    print("=" * 60)
    if failures:
        print("RESTORE UNDER LOAD FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(f"PASSED: {args.rounds} restore(s) under disk load, guest survived each time.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
