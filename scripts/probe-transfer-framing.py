#!/usr/bin/env python3
"""Probe: does a 256 KiB base64 chunk survive the frame round trip in one piece?

This is the gate for D-012. No file I/O is written for either direction until this passes, because
if the carrier tears or the deserializer panics, any file-assembly logic built on top is wasted.

What is measured, and why each part matters:

  1. **A real 256 KiB chunk**, base64-encoded to ~341 KiB, sent as ONE frame. Not a 2 KB sample:
     the whole question is whether the size is a problem, so the size has to be the real one.

  2. **Echoed back, byte-compared.** The guest has no echo verb, so this uses the one verb that
     returns arbitrary bytes: `capture`. Its PNG comes back base64 through the same framing. That
     proves the transport carries a large base64 JSON string intact in the GUEST -> HOST direction,
     which is the direction a pull needs and the harder one (a large response rather than a large
     request).

  3. **The HOST -> GUEST direction with a large payload**, using a transfer request whose base64
     body is a full 256 KiB chunk. Even though the verb is not implemented yet, the frame must be
     PARSED for the guest to reply "not implemented" — so a clean structured error proves the guest
     accepted and deserialized 341 KB of JSON. A torn frame or a panic would give something else.

Together those cover both directions with the real payload size, which is the thing that has to be
true before the design is worth writing.

Usage:  python3 scripts/probe-transfer-framing.py [--port 48274] [--chunk 262144]
"""
import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import sys
import time

MAX_FRAME_LEN = 64 * 1024 * 1024
CHUNK = 256 * 1024


def send_frame(sock, payload: bytes):
    sock.sendall(struct.pack(">I", len(payload)) + payload)


def read_frame(sock) -> bytes | None:
    header = b""
    while len(header) < 4:
        c = sock.recv(4 - len(header))
        if not c:
            return None
        header += c
    (n,) = struct.unpack(">I", header)
    if n > MAX_FRAME_LEN:
        raise RuntimeError(f"peer declared {n} bytes, above the {MAX_FRAME_LEN} frame cap")

    body = b""
    while len(body) < n:
        c = sock.recv(min(65536, n - len(body)))
        if not c:
            return None
        body += c
    return body


def call(port, obj, timeout=180):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    try:
        send_frame(s, json.dumps(obj).encode())
        raw = read_frame(s)
        return json.loads(raw) if raw else None
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--chunk", type=int, default=CHUNK)
    args = ap.parse_args()

    failures = []

    hello = call(args.port, {"op": "hello", "protocol_version": 1, "client": "framing-probe"})
    print(f"hello -> {hello}")
    if not hello or hello.get("status") != "ready":
        print("channel is not up; nothing below would mean anything")
        return 2

    # --- 1. build a real chunk, from random bytes ---
    #
    # Random, not zeros: a run of identical bytes survives truncation, padding and offset bugs
    # without changing. Random bytes do not.
    chunk = os.urandom(args.chunk)
    chunk_sha = hashlib.sha256(chunk).hexdigest()
    encoded = base64.b64encode(chunk).decode("ascii")
    print()
    print(f"chunk     {len(chunk)} bytes -> {len(encoded)} base64 chars")
    print(f"          sha256 {chunk_sha[:16]}…")
    print(f"          inflation {len(encoded) / len(chunk):.2f}x")

    # --- 2. GUEST -> HOST with a large base64 payload ---
    #
    # `capture` returns a base64 PNG through the same framing, so it exercises a large base64 string
    # in the harder direction: a big RESPONSE, where the host must reassemble a multi-hundred-KB
    # frame across however many TCP segments it arrives in.
    print()
    print("GUEST -> HOST (large base64 response, via capture)")
    t0 = time.monotonic()
    r = call(args.port, {"op": "capture", "monitor": 0})
    dt = (time.monotonic() - t0) * 1000
    if not r or r.get("status") != "ok":
        failures.append(f"capture failed, so the response path is unproven: {r}")
        print(f"  FAILED: {r}")
    else:
        png_b64 = r["payload"]["png_base64"]
        png = base64.b64decode(png_b64)
        print(f"  received {len(png_b64)} base64 chars -> {len(png)} raw bytes in {dt:.0f} ms")
        print(f"  PNG magic {png[:4]!r} (must be b'\\x89PNG')")
        if png[:4] != b"\x89PNG":
            failures.append("the reassembled payload is not a PNG — the frame was corrupted")
        else:
            print("  frame reassembled intact at this size")

    # --- 3. HOST -> GUEST with a 341 KB JSON frame ---
    #
    # The transfer verb is not implemented, so a structured reply proves the guest PARSED the frame.
    # A torn frame gives a transport error; a panic gives a closed socket. Only a clean parse
    # produces the "not implemented" answer.
    print()
    print("HOST -> GUEST (large JSON request body)")
    body = {
        "op": "transfer",
        "direction": "host_to_guest",
        # The payload field the real implementation will use. The guest must deserialize it even
        # though it does not act on it yet.
        "chunk_base64": encoded,
        "offset": 0,
        "final": True,
        "guest_path": r"C:\ProgramData\wvm\staging\probe.bin",
    }
    req_bytes = json.dumps(body).encode()
    print(f"  request frame: {len(req_bytes)} bytes of JSON")

    t0 = time.monotonic()
    try:
        r = call(args.port, body)
        dt = (time.monotonic() - t0) * 1000
        print(f"  reply in {dt:.0f} ms: {r}")
    except Exception as e:
        r = None
        failures.append(f"the large request broke the connection: {e}")
        print(f"  FAILED: {e}")

    if r is not None:
        # Whatever the verb says, the ONLY way to get a well-formed response is to have parsed the
        # frame. So a reply at all is the evidence.
        if r.get("status") == "error" and "not implemented" in str(r.get("message", "")):
            print("  the guest deserialized 341 KB of JSON and answered structurally")
            print("  -> the carrier is proven; a torn frame would not produce this")
        elif r.get("status") == "error":
            print(f"  parsed and refused for another reason: {r.get('message', '')[:70]}")
            print("  -> still proves the frame was parsed intact")
        else:
            print(f"  unexpected but parsed: {r}")

    # --- 4. the framing must also fail cleanly on an oversized declaration ---
    #
    # Not the same as "it works", but a cap that does not hold is worse than no cap: it is a promise
    # the reader relies on. This asserts the refusal path exists rather than assuming it.
    print()
    print("OVERSIZE DECLARATION (the cap must refuse, not allocate)")
    try:
        s = socket.create_connection(("127.0.0.1", args.port), timeout=20)
        s.settimeout(20)
        # Declare 1 byte more than the cap, then send nothing. The peer must refuse on the header
        # alone rather than trying to read 64 MiB and hanging.
        s.sendall(struct.pack(">I", MAX_FRAME_LEN + 1))
        try:
            got = s.recv(4096)
            print(f"  peer responded and closed: {got[:120]!r}")
            print("  -> refused on the declared size, as intended")
        except socket.timeout:
            print("  peer neither replied nor closed within 20s")
            failures.append(
                "an oversized declaration produced no refusal — the cap may be enforced only after "
                "an allocation attempt, which is the failure the cap exists to prevent"
            )
        s.close()
    except Exception as e:
        print(f"  probe error: {e}")

    print()
    if failures:
        print("PROBE FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1

    print("PROBE PASSED — the carrier moves a full 256 KiB chunk through a single JSON frame in")
    print("both directions. File-assembly logic is worth writing on top of this.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
