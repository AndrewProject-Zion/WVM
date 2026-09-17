#!/usr/bin/env python3
"""Run one command in the guest and print what came back.

A small helper rather than a curl-shaped one-liner, because every ad-hoc invocation of this was
re-deriving the same framing, the same JSON, and the same quoting rules — and getting them subtly
wrong at least once.

Usage:
    python3 scripts/guest-exec.py "ver"
    python3 scripts/guest-exec.py "dir /b C:\\ProgramData\\wvm\\staging" --json
    python3 scripts/guest-exec.py "ping -n 3 127.0.0.1" --timeout 30

Paths should be passed unquoted where possible: quotes reach the guest as part of the argument and
`certutil -hashfile "C:\\path"` fails with FILE_NOT_FOUND for that reason, not a missing file.
"""
import argparse
import json
import socket
import struct
import sys


def call(port, req, timeout=120):
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("command", help="the command line to run in the guest")
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--timeout", type=int, default=60, help="guest-side timeout, seconds")
    ap.add_argument("--json", action="store_true", help="print the raw response")
    ap.add_argument("--program", default="cmd.exe")
    args = ap.parse_args()

    reply = call(
        args.port,
        {
            "op": "exec",
            "program": args.program,
            "args": ["/c", args.command],
            "cwd": None,
            "require_allowlist": False,
            "timeout_ms": args.timeout * 1000,
        },
        timeout=args.timeout + 30,
    )

    if reply is None:
        print("no reply — the guest did not answer", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(reply, indent=2))
        return 0

    payload = reply.get("payload", {})
    if reply.get("status") == "error":
        print(f"error: {reply.get('message')}", file=sys.stderr)
        return 1

    out = (payload.get("stdout") or "").rstrip()
    err = (payload.get("stderr") or "").rstrip()
    if out:
        print(out)
    if err:
        print(err, file=sys.stderr)
    return int(payload.get("code") or 0)


if __name__ == "__main__":
    sys.exit(main())
