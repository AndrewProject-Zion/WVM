#!/usr/bin/env python3
"""Does merely STOPPING and RESUMING the guest crash it, with no snapshot involved?

WHY THIS IS THE EXPERIMENT TO RUN FIRST

The guest has bugchecked three times with *identical* parameters:

    0x50 (0xfffffffffffffff8, 0x44, 0xfffff8072cc54277, ...)
    0x50 (0xfffffffffffffff8, 0x44, 0xfffff80158a54277, ...)
    0x50 (0xfffffffffffffff8, 0x44, 0xfffff80452854277, ...)

Same stop code, same first and second parameters, and a faulting address that differs only in its
top bits — which is where KASLR randomises a load base. Identical everywhere else means a
DETERMINISTIC fault at a fixed code offset, not a race. Deterministic faults are findable.

Every crash so far happened around a snapshot. But a snapshot does two things at once:

  1. it STOPS the vCPUs and later resumes them
  2. it writes the machine state — RAM and device state — into the qcow2

If the fault is in the device layer's stop/resume handling, then a bare stop/cont reproduces it and
snapshots are merely how we kept triggering it. If a bare stop/cont is clean, the fault needs the
state write and the search moves there.

No theory substitutes for that distinction, and this test costs one minute.

The outcome is measured by the guest's OWN record rather than by a timeout: a new minidump, or a
newer Event 1001, means it crashed. Absence of a new dump after a clean stop/cont is meaningful
precisely because the dump mechanism is proven to work here — it has fired four times.

Usage: python3 scripts/probe-stop-cont-crash.py [--hold 60]
Exit:  0 if the guest survived, 1 if it bugchecked.
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

QMP = Path.home() / ".local/state/wvm/w11/qmp.sock"
PORT = 48274
STAGING = r"C:\ProgramData\wvm\staging"


def qmp(command, arguments=None, timeout=60):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(str(QMP))
    try:
        f = s.makefile("rw")
        f.readline()  # greeting
        def send(name, args=None):
            payload = {"execute": name}
            if args:
                payload["arguments"] = args
            f.write(json.dumps(payload) + "\n")
            f.flush()
            while True:
                msg = json.loads(f.readline())
                if "return" in msg or "error" in msg:
                    return msg
        send("qmp_capabilities")
        return send(command, arguments)
    finally:
        s.close()


def guest(request, timeout=90):
    s = socket.create_connection(("127.0.0.1", PORT), timeout=timeout)
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


def minidumps():
    """The guest's crash dumps, as a set of names. A new name means a new crash."""
    p = run_on_guest(r"dir /b C:\Windows\Minidump")
    out = (p.get("stdout") or "")
    return {ln.strip() for ln in out.splitlines() if ln.strip().lower().endswith(".dmp")}


def guest_answers(deadline_s=180):
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            r = guest({"op": "hello", "protocol_version": 1, "client": "stopcont"}, timeout=10)
            if r and r.get("status") == "ready":
                return True
        except Exception:
            pass
        time.sleep(2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hold", type=int, default=60, help="seconds to keep the vCPUs stopped")
    args = ap.parse_args()

    if not guest_answers(30):
        print("  no guest on the control port — start the VM first")
        return 2

    before = minidumps()
    print(f"  guest is healthy; {len(before)} existing crash dump(s)")

    print(f"  stopping the vCPUs for {args.hold}s — NO snapshot involved")
    r = qmp("stop")
    if "error" in r:
        print(f"  could not stop: {r['error']}")
        return 2

    # Confirm it really is stopped, or the test proves nothing.
    status = qmp("query-status").get("return", {}) or qmp("query-status")["return"]
    print(f"    status during hold: {status.get('status', status)}")

    time.sleep(args.hold)

    print("  resuming")
    qmp("cont")

    answered = guest_answers(180)
    after = minidumps() if answered else set()

    print()
    if not answered:
        print("  The guest did NOT come back after a bare stop/cont.")
        print("  -> the stop/resume path alone is enough to break it")
        return 1

    new = after - before
    if new:
        print(f"  THE GUEST BUGCHECKED on a bare stop/cont: {sorted(new)}")
        print("  -> the fault is in the stop/resume handling, NOT in the snapshot's state write")
        return 1

    print(f"  The guest survived: no new crash dump (still {len(after)}).")
    print("  -> a bare stop/cont is CLEAN, so the fault needs the snapshot's state write")
    return 0


if __name__ == "__main__":
    sys.exit(main())
