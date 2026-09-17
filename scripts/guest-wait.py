#!/usr/bin/env python3
"""Wait for the guest to start answering again, and report how long it was gone.

WHY THIS EXISTS

A snapshot save appeared to kill the guest: the save timed out, `hello` timed out, QMP itself timed
out, and the guest stayed silent — while QEMU was alive with an 8.5 GB RSS.

Two explanations fit that evidence and they are not similar:

  1. THE GUEST CRASHED. Something in the snapshot path is corrupting machine state. Serious.
  2. THE GUEST IS FROZEN, CORRECTLY, AND FOR A LONG TIME. A snapshot stops the vCPUs while it
     writes the machine state, and the write turned out to run at ~28 MB/s rather than disk speed.
     3.4 GB at that rate is minutes. The client's 60-second read timeout gives up long before the
     write finishes, so it reports a failure for something that is still working.

The distinguishing measurement is trivial and does not require any theory: **does the guest come
back on its own?** If it does, and the machine state is intact, explanation 2 is confirmed and there
is no corruption to chase. If it never comes back, explanation 1 stands.

Waiting is the experiment. This script does only that, and prints the elapsed time, so the freeze
duration becomes a number rather than an impression.

Usage: python3 scripts/guest-wait.py [--port 48274] [--timeout 900]
Exit:  0 if the guest answered, 2 if it did not within the timeout.
"""
import argparse
import json
import socket
import struct
import sys
import time


def hello(port, timeout=8):
    """One hello attempt. Returns True/False; never raises."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        s.settimeout(timeout)
        try:
            body = json.dumps({"op": "hello", "protocol_version": 1, "client": "wait"}).encode()
            s.sendall(struct.pack(">I", len(body)) + body)
            header = b""
            while len(header) < 4:
                part = s.recv(4 - len(header))
                if not part:
                    return False
                header += part
            (n,) = struct.unpack(">I", header)
            body = b""
            while len(body) < n:
                part = s.recv(n - len(body))
                if not part:
                    break
                body += part
            reply = json.loads(body)
            return reply.get("status") == "ready"
        finally:
            s.close()
    except Exception:
        return False


def qemu_writing():
    """Bytes written by qemu in the last second, or None when there is no qemu."""
    from pathlib import Path

    pid = None
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if "qemu" in (p / "comm").read_text():
                    pid = p.name
                    break
            except OSError:
                continue
    if pid is None:
        return None

    def wb():
        try:
            for line in Path(f"/proc/{pid}/io").read_text().splitlines():
                if line.startswith("write_bytes"):
                    return int(line.split()[1])
        except OSError:
            pass
        return None

    a = wb()
    time.sleep(1.0)
    b = wb()
    if a is None or b is None:
        return None
    return b - a


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--timeout", type=int, default=900)
    args = ap.parse_args()

    start = time.time()
    if hello(args.port):
        print("  the guest is already answering")
        return 0

    print(f"  guest not answering; waiting up to {args.timeout}s for it to come back")
    print("  (watching qemu's write rate too, so 'busy' and 'stuck' are distinguishable)")
    print()
    print(f"  {'elapsed':>8}  {'qemu writing':>14}  guest")

    last_report = 0.0
    while time.time() - start < args.timeout:
        elapsed = time.time() - start
        w = qemu_writing()
        writing = "n/a" if w is None else f"{w / 1024 / 1024:8.1f} MB/s"

        if hello(args.port):
            total = time.time() - start
            print(f"  {total:7.0f}s  {writing:>14}  ANSWERED")
            print()
            print(f"  The guest was unresponsive for {total:.0f} seconds and then RECOVERED on its own.")
            if total > 60:
                print("  A save that takes longer than a minute with the vCPUs frozen will look like")
                print("  a crash to any client whose read timeout is under a minute.")
            return 0

        # Report roughly every 15s, plus immediately when the write rate changes sharply.
        if elapsed - last_report >= 15:
            print(f"  {elapsed:7.0f}s  {writing:>14}  silent")
            last_report = elapsed
        time.sleep(2)

    print()
    print(f"  The guest did NOT answer within {args.timeout}s — that is a real failure, not slowness.")
    return 2


if __name__ == "__main__":
    sys.exit(main())
