#!/usr/bin/env python3
"""Host side of the transport probe.

Verifies that the guest's control port is reachable through the QEMU host forward, before any
guest service exists to talk to.

Pair it with scripts/probe-transport.ps1 running inside the guest.

Usage:
    python3 scripts/probe-host-transport.py [--port 48274] [--timeout 60]
"""

from __future__ import annotations

import argparse
import socket
import sys
import time


def main(argv):
    parser = argparse.ArgumentParser(description="Host side of the WVM transport probe.")
    parser.add_argument(
        "--port",
        type=int,
        default=48274,
        help="the host port the VM forwards to the guest's control port",
    )
    parser.add_argument("--host", default="127.0.0.1", help="host to connect to")
    parser.add_argument("--timeout", type=int, default=60, help="seconds to keep trying")
    parser.add_argument(
        "--once",
        action="store_true",
        help="try once and exit, rather than retrying until the timeout",
    )
    args = parser.parse_args(argv[1:])

    target = (args.host, args.port)
    print(f"probe: connecting to {target[0]}:{target[1]}")
    print()

    # Retry rather than connect once: the guest-side probe may not have started listening yet, and
    # a single refusal would be indistinguishable from a genuinely broken forward.
    deadline = time.time() + args.timeout
    attempt = 0
    last_error = None

    while True:
        attempt += 1
        try:
            sock = socket.create_connection(target, timeout=5)
            break
        except OSError as e:
            last_error = e
            if args.once or time.time() >= deadline:
                print(f"  FAILED after {attempt} attempt(s): {e}")
                print()
                print("  What this means:")
                print("    * nothing is listening in the guest on the guest_port, or")
                print("    * the ports do not line up between the VM config and the guest, or")
                print("    * the guest firewall dropped it")
                print()
                print("  Check the guest side is running:  .\\probe-transport.ps1")
                return 1
            time.sleep(1)

    print(f"  CONNECTED after {attempt} attempt(s)")

    try:
        sock.settimeout(10)
        sock.sendall(b"ping")
        reply = sock.recv(1024)
        print(f"  sent:     ping")
        print(f"  received: {reply.decode(errors='replace')!r}")
    except OSError as e:
        print(f"  connected but the exchange failed: {e}")
        return 1
    finally:
        sock.close()

    print()
    print("  TRANSPORT VERIFIED: host -> guest -> host round trip completed.")
    print("  The guest service can now be built against this path.")
    print()
    print("  Note on TLS: this channel is plaintext on loopback. It is not exposed beyond the host")
    print("  because the forward binds 127.0.0.1, but it carries no authentication of its own —")
    print("  which is why the protocol assumes the host is trusted and the capability boundary")
    print("  lives on the host side rather than in the transport.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
