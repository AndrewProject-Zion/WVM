#!/usr/bin/env python3
"""
Reference client for the WVM control plane.

This exists to demonstrate the thing the project claims: that a program can drive a Windows VM as
a typed tool. It speaks the same protocol the Rust host does — length-prefixed JSON over a Unix
socket — so it doubles as an executable specification of the wire format.

It is deliberately dependency-free: standard library only, no pip install, no build step. If this
script cannot talk to the daemon, the protocol documentation is wrong, and that is worth knowing.

Usage:
    python3 examples/wvm_client.py hello
    python3 examples/wvm_client.py inspect
    python3 examples/wvm_client.py raw '{"op":"inspect"}'
    python3 examples/wvm_client.py boundary          # demonstrate the capability gate

The `boundary` subcommand is the interesting one: it starts a daemon with a narrow grant in a
private socket and then tries to exceed it, printing what the journal recorded.
"""

from __future__ import annotations

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# Must match wvm_ipc::MAX_FRAME_LEN. Refusing early here means a client bug surfaces as a clear
# local error rather than as a hang waiting for a reply the daemon will never send.
MAX_FRAME_LEN = 64 * 1024 * 1024

PROTOCOL_VERSION = 1

# Where the host binary lives, relative to this script's repository root.
REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = REPO_ROOT / "target" / "release" / "wvm"


class ProtocolError(RuntimeError):
    """The peer said something that does not match the protocol."""


def send_frame(sock: socket.socket, payload: bytes) -> None:
    """Write one length-prefixed message. Length is big-endian u32, payload only."""
    if len(payload) > MAX_FRAME_LEN:
        raise ProtocolError(
            f"refusing to send {len(payload)} bytes; the maximum frame is {MAX_FRAME_LEN}"
        )
    sock.sendall(struct.pack(">I", len(payload)) + payload)


def recv_exactly(sock: socket.socket, n: int) -> bytes:
    """Read exactly n bytes, or raise. A short read is a fault, not a partial success."""
    chunks = []
    remaining = n
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ProtocolError(f"connection closed with {remaining} bytes still expected")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def recv_frame(sock: socket.socket) -> bytes:
    """Read one length-prefixed message."""
    header = recv_exactly(sock, 4)
    (length,) = struct.unpack(">I", header)
    if length > MAX_FRAME_LEN:
        # Refuse before allocating. A hostile or corrupt peer must not be able to make this
        # process reserve arbitrary memory.
        raise ProtocolError(f"peer declared {length} bytes, over the {MAX_FRAME_LEN} limit")
    return recv_exactly(sock, length)


class Client:
    """One connection to a WVM control socket."""

    def __init__(self, path: str | os.PathLike):
        self.path = str(path)
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)
        # A daemon that has wedged should not hang the caller forever.
        self.sock.settimeout(30.0)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def call(self, request: dict) -> dict:
        """Send one request, return the parsed response."""
        send_frame(self.sock, json.dumps(request).encode())
        reply = recv_frame(self.sock)
        try:
            return json.loads(reply)
        except json.JSONDecodeError as e:
            raise ProtocolError(f"the reply was not JSON: {reply[:200]!r} ({e})") from None

    def hello(self, name: str = "python-client") -> dict:
        """Handshake. Must be the first call on a connection."""
        return self.call({"op": "hello", "protocol_version": PROTOCOL_VERSION, "client": name})


# --- pretty output ------------------------------------------------------------------------------


def show(label: str, value: object) -> None:
    if isinstance(value, (dict, list)):
        body = json.dumps(value, indent=2)
    else:
        body = str(value)
    print(f"{label}: {body}")


def explain(response: dict) -> str:
    """
    Turn a response into one line of plain English.

    The point of the reference client is to make the protocol legible, so a response is never
    printed without saying what it means.
    """
    status = response.get("status")
    if status == "ready":
        return f"handshake accepted; peer reports: {response.get('guest')}"
    if status == "error":
        message = response.get("message", "")
        if "denied" in message:
            # A gate refusal is a deliberate boundary: the verb was not in the daemon's grant. This
            # is different from a verb that is granted but has nothing to talk to, and a caller
            # needs to tell them apart — one is policy, the other is state.
            return f"REFUSED by the capability gate - {message}"
        if "not implemented" in message:
            return f"understood, but this build cannot do it yet - {message}"
        if "no guest channel" in message:
            return f"understood; needs a running VM - {message}"
        return f"error - {message}"

    payload = response.get("payload", {})
    kind = payload.get("result")
    if kind == "inventory":
        return (
            f"inventory: os={payload.get('os')!r}, "
            f"{len(payload.get('drives', []))} drive(s), {len(payload.get('apps', []))} app(s)"
        )
    return f"ok ({kind})"


# --- subcommands --------------------------------------------------------------------------------


def cmd_hello(sock_path: str) -> int:
    with Client(sock_path) as c:
        response = c.hello()
    show("raw", response)
    print(f"  -> {explain(response)}")
    return 0 if response.get("status") == "ready" else 1


def cmd_inspect(sock_path: str) -> int:
    with Client(sock_path) as c:
        c.hello()
        response = c.call({"op": "inspect"})
    show("raw", response)
    print(f"  -> {explain(response)}")
    return 0


def cmd_raw(sock_path: str, payload: str) -> int:
    try:
        request = json.loads(payload)
    except json.JSONDecodeError as e:
        print(f"not valid JSON: {e}", file=sys.stderr)
        return 2

    with Client(sock_path) as c:
        c.hello()
        response = c.call(request)
    show("raw", response)
    print(f"  -> {explain(response)}")
    return 0


def cmd_boundary(binary: Path) -> int:
    """
    Demonstrate that the capability gate refuses what the grant does not permit.

    Starts a daemon with only `inspect` granted, on a private socket, then attempts operations
    that need other verbs. Prints what the daemon said and what it journalled.
    """
    if not binary.exists():
        print(f"host binary not found: {binary}", file=sys.stderr)
        print("build it first:  cargo build --release -p wvm-host", file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory() as tmp:
        sock_path = os.path.join(tmp, "control.sock")

        proc = subprocess.Popen(
            [str(binary), "serve", "--socket", sock_path, "--verbs", "inspect"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        try:
            # Wait for the socket to be *bound*, not merely present. A file on disk proves
            # nothing — the same trap the daemon itself guards against.
            deadline = time.time() + 10
            while time.time() < deadline:
                if os.path.exists(sock_path):
                    try:
                        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                        probe.connect(sock_path)
                        probe.close()
                        break
                    except OSError:
                        pass
                time.sleep(0.1)
            else:
                print("the daemon never bound its socket", file=sys.stderr)
                return 1

            print("Daemon started with the grant: inspect only")
            print()
            print("This exercises the CAPABILITY GATE, not a running VM. Refusals below come from")
            print("the grant, so they hold whether or not a guest is present — which is why this")
            print("is the one command worth running first.")
            print()

            with Client(sock_path) as c:
                hello = c.hello("boundary-demo")
                print(f"  hello      -> {explain(hello)}")

                # Permitted: inspect is in the grant.
                inspect = c.call({"op": "inspect"})
                print(f"  inspect    -> {explain(inspect)}")

                # Refused: none of these verbs were granted.
                for label, request in [
                    ("capture", {"op": "capture", "monitor": 0}),
                    ("lifecycle", {"op": "lifecycle", "action": "suspend"}),
                    (
                        "exec",
                        {
                            "op": "exec",
                            "program": "c:\\windows\\system32\\cmd.exe",
                            "args": [],
                            "cwd": None,
                            "require_allowlist": True,
                        },
                    ),
                ]:
                    response = c.call(request)
                    marker = "REFUSED" if response.get("status") == "error" else "ALLOWED"
                    print(f"  {label:<10} -> {marker}")
                    if marker != "REFUSED":
                        print(
                            f"\nFAIL: {label} should not have been permitted by an inspect-only grant",
                            file=sys.stderr,
                        )
                        return 1

            print()
            print("Every ungranted verb was refused. The grant held.")

        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    return 0


# --- entry point --------------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    import argparse

    parser = argparse.ArgumentParser(
        description="Reference client for the WVM control plane.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  %(prog)s boundary\n"
            "  %(prog)s hello\n"
            "  %(prog)s raw '{\"op\":\"inspect\"}'\n"
        ),
    )
    parser.add_argument(
        "--socket",
        default=os.environ.get("WVM_SOCKET", "/tmp/wvm/control.sock"),
        help="control socket path (default: %(default)s)",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("hello", help="handshake and report what is on the other end")
    sub.add_parser("inspect", help="ask for guest inventory")
    # `boundary` needs no socket: it starts its own daemon.
    sub.add_parser("boundary", help="demonstrate the capability gate refusing ungranted verbs")

    raw = sub.add_parser("raw", help="send a raw JSON request")
    raw.add_argument("payload", help="the request as JSON")

    args = parser.parse_args(argv)

    if args.command == "boundary":
        return cmd_boundary(DEFAULT_BINARY)

    sock_path = args.socket
    if not os.path.exists(sock_path):
        print(f"no control socket at {sock_path}", file=sys.stderr)
        print("start one with:  wvm serve", file=sys.stderr)
        return 1

    try:
        if args.command == "hello":
            return cmd_hello(sock_path)
        if args.command == "inspect":
            return cmd_inspect(sock_path)
        if args.command == "raw":
            return cmd_raw(sock_path, args.payload)
    except (ConnectionRefusedError, FileNotFoundError) as e:
        print(f"could not reach the daemon: {e}", file=sys.stderr)
        return 1
    except ProtocolError as e:
        print(f"protocol error: {e}", file=sys.stderr)
        return 1

    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
