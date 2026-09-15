#!/usr/bin/env python3
"""Isolate why a 341 KB request frame produced no reply.

The first framing probe sent a large JSON body and got nothing back. Two candidate causes, and they
need separating before either can be acted on:

  A. **Size.** The frame is too large for the read path, so the guest rejects it or desynchronises
     before dispatch. This would be a real transport problem and would sink Option 1.
  B. **Content.** The body contained a field (`chunk_base64`) that does not exist on the `Transfer`
     variant yet. serde's default is to IGNORE unknown fields, but a strict configuration would
     reject — and either way the guest should REPLY with an error rather than close silently, so a
     silent close would itself be worth knowing.

The way to tell them apart is to vary ONE thing at a time:

  1. small valid request              -> expect a reply (baseline)
  2. small request + unknown field    -> expect a reply (proves unknown fields are tolerated)
  3. LARGE valid request              -> expect a reply (proves the size is fine)
  4. LARGE request + unknown field    -> the original failing case
  5. large request of PURE PADDING    -> proves the size alone, with no plausible field in it

If 3 replies and 4 does not, the cause is (B). If neither 3 nor 4 replies, it is (A).

Usage:  python3 scripts/probe-frame-size.py [--port 48274]
"""
import argparse
import json
import socket
import struct
import sys

MAX_FRAME_LEN = 64 * 1024 * 1024


def attempt(port, payload: bytes, timeout=25):
    """Send one frame, return (reply_or_None, why)."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    except OSError as e:
        return None, f"connect failed: {e}"
    s.settimeout(timeout)
    try:
        s.sendall(struct.pack(">I", len(payload)) + payload)
        header = b""
        while len(header) < 4:
            c = s.recv(4 - len(header))
            if not c:
                return None, "peer closed without replying"
            header += c
        (n,) = struct.unpack(">I", header)
        body = b""
        while len(body) < n:
            c = s.recv(min(65536, n - len(body)))
            if not c:
                return None, f"truncated reply: {len(body)} of {n} bytes"
            body += c
        try:
            return json.loads(body), "ok"
        except json.JSONDecodeError as e:
            return None, f"reply was not JSON: {e}; first 200 bytes: {body[:200]!r}"
    except socket.timeout:
        return None, f"timed out after {timeout}s"
    finally:
        s.close()


def show(label, payload, port):
    obj, why = attempt(port, payload)
    size = len(payload)
    if obj is None:
        print(f"  {label:<34} {size:>8} bytes   NO REPLY  ({why})")
    else:
        # Print the shape of the reply, not the whole thing.
        summary = obj.get("message") or obj.get("status")
        print(f"  {label:<34} {size:>8} bytes   reply: {str(summary)[:60]}")
    return obj


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    args = ap.parse_args()

    print("Separating size from content. Each line varies ONE thing.\n")

    # 1. baseline: a small, valid request
    small_ok = json.dumps({"op": "inspect"}).encode()
    show("1. small, valid", small_ok, args.port)

    # 2. small, with a field the struct does not have
    small_unknown = json.dumps({"op": "inspect", "chunk_base64": "AAAA"}).encode()
    show("2. small + unknown field", small_unknown, args.port)

    # 3. LARGE, structurally valid: a transfer request with real fields only.
    #
    # `transfer` is implemented enough to be refused properly, so a reply here is a clean signal.
    big_valid = json.dumps(
        {
            "op": "transfer",
            "direction": "host_to_guest",
            "host_path": "/tmp/x",
            "guest_path": r"C:\ProgramData\wvm\staging\x",
        }
    ).encode()
    # Pad a VALID field rather than inventing one: guest_path can legally be long.
    padding = "A" * (350 * 1024)
    big_valid = json.dumps(
        {
            "op": "transfer",
            "direction": "host_to_guest",
            "host_path": "/tmp/x",
            "guest_path": r"C:\ProgramData\wvm\staging" + "\\" + padding + ".bin",
        }
    ).encode()
    show("3. large, valid fields only", big_valid, args.port)

    # 4. LARGE, with the unknown field the original probe used
    big_unknown = json.dumps(
        {
            "op": "transfer",
            "direction": "host_to_guest",
            "host_path": "/tmp/x",
            "guest_path": r"C:\ProgramData\wvm\staging\x",
            "chunk_base64": "A" * (340 * 1024),
        }
    ).encode()
    show("4. large + unknown field", big_unknown, args.port)

    # 5. LARGE, but a different verb entirely, so nothing transfer-specific can be the cause.
    #
    # `exec` with a long argument list is the cleanest large-request case: no new fields, no paths
    # to refuse, and the reply is small.
    big_exec = json.dumps(
        {
            "op": "exec",
            "program": "cmd.exe",
            "args": ["/c", "echo", "B" * (350 * 1024)],
            "cwd": None,
            "require_allowlist": False,
        }
    ).encode()
    show("5. large exec (verb is live)", big_exec, args.port)

    print()
    print("Reading it:")
    print("  If 1 and 2 reply and 4 does not  -> the size is the problem (A)")
    print("  If 3 and 5 reply and 4 does not  -> the unknown field is the problem (B)")
    print("  If 3 or 5 also fail              -> the size breaks the read path for any verb")
    return 0


if __name__ == "__main__":
    sys.exit(main())
