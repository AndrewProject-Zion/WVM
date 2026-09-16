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
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import tempfile

GUEST_STAGING = r"C:\ProgramData\wvm\staging"

# The guest reads at most this much per chunk. Kept in step with `wvm-guest/src/fsio.rs`.
CHUNK = 256 * 1024


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


def parse_certutil(reply):
    """Pull the hex digest out of a `certutil -hashfile` reply, or "" if there isn't one.

    certutil prints the digest on its own line as 64 hex characters. Matching that shape rather
    than a fixed line number survives the localisation of the surrounding prose.
    """
    if not reply or reply.get("status") != "ok":
        return ""
    for line in reply.get("payload", {}).get("stdout", "").splitlines():
        line = line.strip()
        if len(line) == 64 and all(c in "0123456789abcdefABCDEF" for c in line):
            return line.lower()
    return ""


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
    #
    # `transfer` no longer moves anything by itself: it OPENS a transfer (validating the path and
    # refusing to clobber) and the bytes then travel as `transfer_chunk` messages, one acknowledged
    # chunk at a time. This probe asserts the open succeeds, because a stub guest still answers
    # "not implemented in this build" and every test below would then be measuring a refusal.
    probe = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": None, "guest_path": f"{GUEST_STAGING}\\probe.bin",
        "overwrite": True,
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

    # --- PUSH: host -> guest, in lockstep ---
    #
    # `transfer` opens the destination; `transfer_chunk` carries the bytes. The host sends one
    # chunk and WAITS for the guest's acknowledgement before sending the next, so the transfer
    # advances at the speed of the guest's disk rather than the speed of the host's socket. That
    # is deliberate (D-012): without it the host packs the TCP buffers faster than a virtualised
    # NTFS write can drain them, and the guest runs out of memory.
    print()
    print("PUSH  host -> guest  (chunked, lockstep)")
    r = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": host_out, "guest_path": remote,
        "overwrite": True,
    })
    if not r or r.get("status") != "ok":
        failures.append(f"could not open the destination for a push: {r}")
    else:
        print(f"  opened: {r.get('payload', {}).get('guest_path')}")
        offset = 0
        sent = 0
        while offset < len(payload):
            piece = payload[offset:offset + CHUNK]
            eof = offset + len(piece) >= len(payload)
            ack = call(args.port, {
                "op": "transfer_chunk", "offset": offset,
                "data_base64": base64.b64encode(piece).decode(),
                "eof": eof,
            }, timeout=180)
            if not ack or ack.get("status") != "ok":
                failures.append(f"chunk at offset {offset} was refused: {ack}")
                break
            got = ack.get("payload", {})
            # The guest echoes the offset so a reply can be matched to its chunk, and reports the
            # running total so a short write is caught here rather than at the final hash.
            if got.get("offset") != offset:
                failures.append(f"ACK for the wrong offset: sent {offset}, acked {got.get('offset')}")
                break
            sent += len(piece)
            offset += len(piece)
        print(f"  sent {sent} bytes in {(sent + CHUNK - 1) // CHUNK} chunk(s)")
        if sent != args.size:
            failures.append(f"push sent {sent} bytes of {args.size}")

    # --- prove the guest received the right CONTENT, by asking the guest to hash it ---
    #
    # Not "the transfer said ok" — that is the guest's own claim about its own write. certutil
    # reads the file back off the filesystem, so this is independent of the transfer path.
    #
    # The path is passed UNQUOTED. That is not a style choice — quoting it breaks the command.
    #
    # Measured, after four wrong hypotheses (write timing, NTFS flush, directory metadata, then
    # Defender, which an exclusion test falsified):
    #
    #   certutil -hashfile C:\...\file.bin SHA256     -> works
    #   certutil -hashfile "C:\...\file.bin" SHA256   -> FILE_NOT_FOUND
    #
    #   1 MiB into a 2 MiB file, it still reported FILE_NOT_FOUND with no mention of the path.
    #
    # The quote characters reach certutil as part of the argument and it treats them as part of the
    # filename, then reports that the file does not exist — indistinguishable from a file that
    # genuinely is not there. `attrib` on the same path printed the doubled form, which is what
    # finally pointed at argument handling rather than the filesystem.
    #
    # Two other traps in the same family:
    #   `cd /d X && certutil ...` returns code 1 with EMPTY stdout. `&&` does not survive this exec
    #   layer, and it fails silently — no error, the command simply does not run.
    #   A relative filename fails too, because the guest's cwd is C:\Windows\System32.
    print()
    print("VERIFY (inside the guest, reading the file back)")
    r = call(args.port, {
        "op": "exec", "program": "cmd.exe",
        "args": ["/c", f"certutil -hashfile {remote} SHA256"],
        "cwd": None, "require_allowlist": False, "timeout_ms": 20000,
    })
    guest_hash = parse_certutil(r)
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
        "overwrite": True,
    })
    if not r or r.get("status") != "ok":
        failures.append(f"could not open the source for a pull: {r}")
    else:
        # Ask for one chunk at a time and append what arrives. The loop ends when the guest
        # reports fewer bytes than requested — the file is exhausted — or immediately on an
        # empty chunk, so a zero-length source cannot spin forever.
        offset = 0
        with open(host_in, "wb") as f:
            while True:
                resp = call(args.port, {"op": "pull_chunk", "offset": offset, "length": CHUNK},
                            timeout=180)
                if not resp or resp.get("status") != "ok":
                    failures.append(f"pull_chunk at offset {offset} was refused: {resp}")
                    break
                p = resp.get("payload", {})
                raw = base64.b64decode(p.get("data_base64", ""))
                f.write(raw)
                offset += len(raw)
                # Terminate on the guest's end-of-file flag, NOT on "this chunk was short".
                # A file whose size is an exact multiple of CHUNK ends with a FULL final chunk,
                # so a length test never fires and the next request is refused because the
                # transfer has already closed. That is a real off-by-one, and it only shows up
                # on sizes that divide evenly — a 1 MiB payload is exactly 4 chunks.
                if p.get("eof"):
                    break
                if not raw:
                    # A zero-length chunk that is not flagged eof would spin forever otherwise.
                    failures.append("pull returned an empty chunk without setting eof")
                    break
        print(f"  received {offset} bytes")
        if not os.path.exists(host_in):
            failures.append("pull reported success but no file appeared on the host")
        else:
            back = sha256_file(host_in)
            size = os.path.getsize(host_in)
            print(f"  host file {size} bytes, sha256 {back[:16]}…")
            if size != args.size:
                failures.append(f"pulled {size} bytes, expected {args.size}")
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
        "overwrite": False,
    })
    if r and r.get("status") == "error" and "overwrite" in str(r.get("message", "")).lower():
        print(f"  refused, as intended: {r['message'][:90]}")
        # The refusal must not have truncated anything: prove the file is still intact.
        chk = call(args.port, {
            "op": "exec", "program": "cmd.exe",
            "args": ["/c", f"certutil -hashfile {remote} SHA256"],
            "cwd": None, "require_allowlist": False, "timeout_ms": 20000,
        })
        still = parse_certutil(chk)
        if still and still != want:
            failures.append("the refused overwrite DAMAGED the existing file")
        elif still == want:
            print("  the existing file is untouched — the refusal happened before any write")
    else:
        failures.append(f"a second push to the same path was not refused: {r}")

    # --- containment: a path outside the staging root must be refused ---
    print()
    print("CONTAINMENT")
    r = call(args.port, {
        "op": "transfer", "direction": "host_to_guest",
        "host_path": host_out, "guest_path": r"C:\Windows\Temp\escape.bin",
        "overwrite": True,
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
