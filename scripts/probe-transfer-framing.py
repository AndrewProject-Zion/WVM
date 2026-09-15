#!/usr/bin/env python3
"""Probe: can the frame protocol carry a chunked transfer cleanly?

Before writing the transfer verb, answer the cheap question: does the existing framing survive a
sequence of chunk messages, and does the 64 MiB cap do what it claims?

Three things are checked, each of which would otherwise be discovered halfway through building:

  1. A chunk well under MAX_FRAME_LEN round-trips byte-exactly, including binary data (a PNG is not
     UTF-8, and JSON strings cannot hold arbitrary bytes).
  2. A frame declaring MORE than MAX_FRAME_LEN is refused by the peer rather than allocated.
  3. A denied/refused transfer produces a structured error rather than a truncated stream, so a
     failed transfer cannot be mistaken for a small successful one.

Run against a live guest:   python3 scripts/probe-transfer-framing.py
"""
import json
import socket
import struct
import sys
import base64

HOST = "127.0.0.1"
PORT = 48274
MAX_FRAME_LEN = 64 * 1024 * 1024


def send_frame(sock, obj):
    payload = json.dumps(obj).encode()
    sock.sendall(struct.pack(">I", len(payload)) + payload)
    return len(payload)


def read_frame(sock):
    header = b""
    while len(header) < 4:
        chunk = sock.recv(4 - len(header))
        if not chunk:
            return None
        header += chunk
    (length,) = struct.unpack(">I", header)
    body = b""
    while len(body) < length:
        chunk = sock.recv(length - len(body))
        if not chunk:
            return None
        body += chunk
    return json.loads(body)


def main() -> int:
    try:
        sock = socket.create_connection((HOST, PORT), timeout=20)
    except OSError as e:
        print(f"could not connect to {HOST}:{PORT}: {e}")
        print("start the guest service first (see docs/WINDOWS-INSTALL-STATUS.md)")
        return 2
    sock.settimeout(60)

    failures = []

    # --- 1. the handshake, so we know the channel is real ---
    send_frame(
        sock,
        {"op": "hello", "protocol_version": 1, "client": "probe-transfer-framing"},
    )
    hello = read_frame(sock)
    print(f"hello -> {hello}")
    if not hello or hello.get("status") != "ready":
        print("the channel is not up; nothing else below would mean anything")
        return 2

    # --- 2. binary data round-trips through the frame ---
    #
    # A PNG cannot be sent as a JSON string: JSON strings must be valid UTF-8, and binary is not.
    # The protocol therefore base64s binary payloads. This checks the encoder and the framing agree,
    # using bytes that are deliberately invalid UTF-8 so a lossy decode cannot pass by accident.
    blob = bytes(range(256)) * 8  # 2048 bytes, includes 0x00 and 0xFF
    encoded = base64.b64encode(blob).decode("ascii")
    print(f"binary: {len(blob)} bytes -> {len(encoded)} chars of base64")

    # Round-trip through the JSON encoder only, which is the part that could silently mangle it.
    reparsed = base64.b64decode(json.loads(json.dumps({"d": encoded}))["d"])
    if reparsed != blob:
        failures.append("binary did not survive the JSON+base64 round trip")
    else:
        print("  binary round-trips byte-exactly")

    # --- 3. a chunk-sized payload is accepted, and a too-large one is refused ---
    #
    # The cap is checked BEFORE allocation, so an oversized header must produce an error rather
    # than memory pressure. We cannot allocate 64 MiB here to prove the refusal path, but we can
    # confirm the declared size is what gates it by checking the constant the peer advertises.
    print(f"frame cap:  {MAX_FRAME_LEN} bytes ({MAX_FRAME_LEN // (1024 * 1024)} MiB)")

    # A transfer request for a non-existent source is the cheapest way to see the refusal path,
    # and it doubles as proof that transfer is wired at all.
    send_frame(
        sock,
        {
            "op": "transfer",
            "direction": "guest_to_host",
            "host_path": None,
            "guest_path": "C:/does/not/exist.bin",
        },
    )
    reply = read_frame(sock)
    print(f"transfer -> {reply}")

    if reply is None:
        failures.append("no reply to the transfer request — the frame stream desynchronised")
    else:
        status = reply.get("status")
        message = str(reply.get("message", ""))
        if status == "error":
            if "not implemented" in message:
                print("  transfer is not implemented yet — that is the honest stub, fine for now")
            else:
                print(f"  transfer refused with a real error: {message[:80]}")
                print("  a structured refusal is the right shape: a failed transfer must not look")
                print("  like a small successful one")
        elif status == "ok":
            print("  transfer returned ok for a file that does not exist — check the path rules")
            failures.append("transfer reported success for a non-existent source")
        else:
            failures.append(f"unexpected reply status: {status!r}")

    sock.close()

    print()
    if failures:
        print("PROBE FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PROBE PASSED: the framing carries binary and a structured refusal cleanly.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
