#!/usr/bin/env python3
"""Is the guest control channel answering? One question, one answer, no exceptions either way.

Written as a file rather than a one-liner because inline probing kept failing on shell parsing
rather than on the thing being probed, which wastes a round trip and tells you nothing about the
guest.

Exits 0 if the channel answered, 1 if it did not. Prints one line either way.
"""
import json
import socket
import struct
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 48274
WAIT = int(sys.argv[2]) if len(sys.argv) > 2 else 1


def probe():
    s = socket.create_connection(("127.0.0.1", PORT), timeout=8)
    s.settimeout(10)
    try:
        b = json.dumps({"op": "hello", "protocol_version": 1, "client": "health"}).encode()
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


last = None
for attempt in range(WAIT):
    try:
        r = probe()
        if r is not None:
            print(f"  CHANNEL: {r.get('status')} ({r.get('guest')})")
            sys.exit(0)
        last = "connection closed without a reply"
    except Exception as e:
        last = f"{type(e).__name__}"
    if attempt < WAIT - 1:
        time.sleep(5)

print(f"  channel DOWN: {last}")
sys.exit(1)
