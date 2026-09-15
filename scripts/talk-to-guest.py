#!/usr/bin/env python3
"""Talk to the guest service over the host forward, and prove it by sending a real frame.

A TCP connect is NOT proof. QEMU's user-mode networking completes the handshake locally and only
then tries to deliver to the guest, so a connect succeeds even when nothing is listening — verified
by connecting to this same forward before the guest service existed. Success and failure looked
identical.

Proof requires a round trip: send a length-prefixed JSON request, and receive a length-prefixed
JSON response that could only have come from a working guest.

Usage:
    python3 scripts/talk-to-guest.py [--port 48274] [--host 127.0.0.1] [request]
    python3 scripts/talk-to-guest.py hello
    python3 scripts/talk-to-guest.py exec -- cmd.exe /c echo hello
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import time

MAX_FRAME = 16 * 1024 * 1024


def send_frame(sock: socket.socket, payload: bytes) -> None:
    """4-byte big-endian length, then the payload. Matches wvm-ipc's framing."""
    sock.sendall(struct.pack(">I", len(payload)) + payload)


def recv_frame(sock: socket.socket, timeout: float = 30.0) -> bytes:
    """Read one frame, or raise with a specific reason."""
    sock.settimeout(timeout)

    header = b""
    while len(header) < 4:
        chunk = sock.recv(4 - len(header))
        if not chunk:
            raise ConnectionError(
                "the peer closed the connection before sending a response header — "
                "something accepted the connection but is not the guest service"
            )
        header += chunk

    (length,) = struct.unpack(">I", header)
    if length > MAX_FRAME:
        raise ValueError(f"frame of {length} bytes exceeds the {MAX_FRAME} limit")

    body = b""
    while len(body) < length:
        chunk = sock.recv(length - len(body))
        if not chunk:
            raise ConnectionError(
                f"truncated frame: expected {length} bytes, received {len(body)}"
            )
        body += chunk

    return body


def main(argv):
    description = (__doc__ or "").strip().split("\n")[0]
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=48274)
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("command", nargs="?", default="hello")
    parser.add_argument("rest", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv[1:])

    target = (args.host, args.port)
    print(f"connecting to {target[0]}:{target[1]}")

    started = time.time()
    try:
        sock = socket.create_connection(target, timeout=10)
    except OSError as e:
        print(f"  could not connect: {e}")
        return 1

    # Report the connect, but do NOT treat it as the result — see the module docstring.
    print(f"  connected in {(time.time() - started) * 1000:.0f}ms (not yet proof of anything)")

    try:
        if args.command == "hello":
            request = {"kind": "hello", "client": "talk-to-guest", "protocol_version": 1}

        elif args.command == "inspect":
            request = {"kind": "inspect"}

        elif args.command == "exec":
            # Everything after the subcommand, split on '--'.
            words = [w for w in args.rest if w != "--"]
            if not words:
                print("exec needs a program, e.g. exec -- cmd.exe /c echo hi")
                return 2
            request = {
                "kind": "exec",
                "program": words[0],
                "args": words[1:],
                "cwd": None,
                "require_allowlist": True,
            }

        else:
            # Allow a raw JSON request for anything not covered above.
            request = json.loads(args.command)

        body = json.dumps(request).encode()
        print(f"  sending {len(body)} bytes: {args.command}")
        send_frame(sock, body)

        print("  waiting for a response...")
        reply = recv_frame(sock, timeout=args.timeout)
        print(f"  received {len(reply)} bytes:")
        print()

        # Pretty-print when it parses, so a large payload stays readable.
        try:
            print(json.dumps(json.loads(reply), indent=2))
        except ValueError:
            print(reply.decode(errors="replace"))

        print()
        print("  ROUND TRIP COMPLETE: the guest service answered.")
        return 0

    except Exception as e:
        print()
        print(f"  FAILED: {e}")
        print()
        print("  A connect that succeeds but exchanges no frames means the forward is")
        print("  accepting on the guest's behalf (slirp behaviour). Check inside the guest:")
        print("    netstat -an | findstr 48273")
        return 1
    finally:
        sock.close()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
