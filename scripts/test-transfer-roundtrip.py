#!/usr/bin/env python3
"""Transfer round trip: push a file, pull it back, compare SHA-256.

This is the test that matters for the transfer verb. The unit tests cover the chunk arithmetic and
the containment refusals; only this proves the wiring end to end — that the host's bytes reach the
guest intact, land inside the staging root, and come back byte-identical.

Why the hash comparison is the whole point: a transfer that silently drops a chunk, truncates the
tail, or corrupts through a bad encode produces a file that LOOKS fine. Length alone would not catch
a byte swap. This compares content.

Usage:
    python3 scripts/test-transfer-roundtrip.py [--port 48274] [--size 1048576]
"""
import argparse
import hashlib
import json
import os
import socket
import struct
import sys
import tempfile

GUEST_STAGING = r"C:\ProgramData\wvm\staging"


def call(port, req, timeout=180):
    """One request, one response. No session — matching the guest's one-at-a-time model."""
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(req).encode()
        s.sendall(struct.pack(">I", len(b)) + b)

        header = b""
        while len(header) < 4:
            c = s.recv(4 - len(header))
            if not c:
                return None
            header += c
        (n,) = struct.unpack(">I", header)

        body = b""
        while len(body) < n:
            c = s.recv(n - len(body))
            if not c:
                break
            body += c
        return json.loads(body)
    finally:
        s.close()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(65536), b""):
            h.update(block)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--size", type=int, default=1024 * 1024, help="payload size in bytes")
    args = ap.parse_args()

    failures = []

    # --- handshake, so a later failure is known to be about transfer and not the channel ---
    hello = call(args.port, {"op": "hello", "protocol_version": 1, "client": "roundtrip"})
    print(f"hello    -> {hello}")
    if not hello or hello.get("status") != "ready":
        print("the channel is not up; nothing below would mean anything")
        return 2

    if hello.get("guest") and "0.1.0" in str(hello.get("guest")):
        pass

    # --- is transfer implemented at all? ask before testing it ---
    probe = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": None, "guest_path": f"{GUEST_STAGING}\\probe.bin",
    })
    if probe and "not implemented" in str(probe.get("message", "")):
        print()
        print("  transfer is STILL THE STUB — the guest is running the old binary.")
        print("  Nothing was tested. Deploy the new build first.")
        return 2

    # --- build a payload whose bytes are not all the same, so corruption cannot hide ---
    # A run of identical bytes would survive a truncation-plus-padding bug. This does not.
    payload = os.urandom(args.size)
    host_out = os.path.join(tempfile.gettempdir(), "wvm-roundtrip-out.bin")
    with open(host_out, "wb") as f:
        f.write(payload)
    want = hashlib.sha256(payload).hexdigest()
    print(f"payload  -> {args.size} bytes, sha256 {want[:16]}…")

    remote = f"{GUEST_STAGING}\\roundtrip.bin"

    # --- PUSH: host -> guest ---
    print()
    print("PUSH  host -> guest")
    r = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": host_out, "guest_path": remote,
    })
    print(f"  {r}")
    if not r or r.get("status") != "ok":
        failures.append(f"push did not succeed: {r}")
    else:
        got = r.get("payload", {}).get("bytes")
        if got != args.size:
            failures.append(f"push reported {got} bytes, sent {args.size}")

    # --- prove the guest received the right CONTENT, by asking the guest to hash it ---
    #
    # Not "the transfer said ok" — that is the guest's own claim about its own write. certutil
    # reads the file back off the filesystem, so this is independent of the transfer path.
    print()
    print("VERIFY (inside the guest, reading the file back)")
    r = call(args.port, {
        "op": "exec", "program": "cmd.exe",
        "args": ["/c", f'certutil -hashfile "{remote}" SHA256'],
        "cwd": None, "require_allowlist": False,
    })
    guest_hash = ""
    if r and r.get("status") == "ok":
        out = r["payload"].get("stdout", "")
        for line in out.splitlines():
            line = line.strip()
            # certutil prints the hash on its own line: 64 hex chars, no spaces.
            if len(line) == 64 and all(ch in "0123456789abcdefABCDEF" for ch in line):
                guest_hash = line.lower()
                break
    print(f"  guest says {guest_hash or '(no hash parsed)'}")

    if not guest_hash:
        failures.append("could not read a hash back from the guest")
    elif guest_hash != want:
        failures.append(f"PUSH CORRUPTED: guest {guest_hash[:16]}… != host {want[:16]}…")
    else:
        print("  MATCH — the pushed file is byte-identical inside the guest")

    # --- PULL: guest -> host, into a different local file ---
    print()
    print("PULL  guest -> host")
    host_in = os.path.join(tempfile.gettempdir(), "wvm-roundtrip-back.bin")
    if os.path.exists(host_in):
        os.remove(host_in)

    r = call(args.port, {
        "op": "transfer", "direction": "guest_to_host",
        "host_path": host_in, "guest_path": remote,
    })
    print(f"  {r}")
    if not r or r.get("status") != "ok":
        failures.append(f"pull did not succeed: {r}")
    else:
        if not os.path.exists(host_in):
            failures.append("pull reported success but no file appeared on the host")
        else:
            back = sha256_file(host_in)
            size = os.path.getsize(host_in)
            print(f"  host file {size} bytes, sha256 {back[:16]}…")
            if back != want:
                failures.append(f"PULL CORRUPTED: {back[:16]}… != {want[:16]}…")
            else:
                print("  MATCH — the round trip is byte-exact")

    # --- refuse to overwrite an existing destination ---
    #
    # The default must be refuse-not-clobber: a transfer that silently overwrites is a data-loss bug
    # waiting for a caller that retried.
    print()
    print("OVERWRITE GUARD")
    r = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": host_out, "guest_path": remote,
    })
    if r and r.get("status") == "error" and "overwrite" in str(r.get("message", "")).lower():
        print(f"  refused, as intended: {r['message'][:90]}")
    else:
        failures.append(f"a second push to the same path was not refused: {r}")

    # --- containment: a path outside the staging root must be refused ---
    print()
    print("CONTAINMENT")
    r = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": host_out, "guest_path": r"C:\Windows\Temp\escape.bin",
    })
    if r and r.get("status") == "error":
        print(f"  refused: {r['message'][:90]}")
    else:
        failures.append(f"a path outside the staging root was not refused: {r}")

    print()
    if failures:
        print("ROUND TRIP FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1

    print(f"ROUND TRIP PASSED: {args.size} bytes pushed, pulled and hashed identically.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
