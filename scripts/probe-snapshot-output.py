#!/usr/bin/env python3
"""Dump the raw `info snapshots` output, exactly as the bytes arrive.

Written because the Rust parser said "no snapshots" while a snapshot demonstrably existed, and the
test sample I wrote from memory did not match what QEMU actually prints. The quickest way to fix a
parser is to look at the input rather than reason about it.

Usage:  python3 scripts/probe-snapshot-output.py
"""
import json
import socket
import sys

SOCK = "/home/andy/.local/state/wvm/w11/qmp.sock"


def cmd(f, execute, **kwargs):
    f.write(json.dumps({"execute": execute, **({"arguments": kwargs} if kwargs else {})}) + "\n")
    f.flush()
    for _ in range(60):
        m = json.loads(f.readline())
        if "return" in m or "error" in m:
            return m
    return None


def main():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(60)
    try:
        s.connect(SOCK)
    except Exception as e:
        print(f"cannot connect to {SOCK}: {e}")
        return 2
    f = s.makefile("rw")
    f.readline()
    cmd(f, "qmp_capabilities")

    r = cmd(f, "human-monitor-command", **{"command-line": "info snapshots"})
    raw = (r or {}).get("return", "")

    print("=== raw string, as JSON gives it ===")
    print(repr(raw))
    print()
    print("=== line by line, with the fields split the way the parser splits them ===")
    for i, line in enumerate(raw.splitlines()):
        stripped = line.strip()
        if not stripped:
            print(f"  {i:2}: (blank)")
            continue
        fields = stripped.split()
        first = fields[0] if fields else ""
        is_digit = first.isdigit()
        print(f"  {i:2}: {stripped!r}")
        print(f"      fields={fields}")
        print(f"      first={first!r} is_digit={is_digit} -> {'ROW' if is_digit else 'skipped'}")
    s.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
