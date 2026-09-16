#!/usr/bin/env python3
"""Verify the control channel survives a wedged request, without waiting out the deadline.

THE PROBLEM WITH TESTING THIS

The backstop is fifteen minutes. A probe cannot wait fifteen minutes, and the previous attempt at
this tested the wrong process entirely — it started a console instance with a short deadline while
the Windows SERVICE kept ownership of the port, so every measurement was of a process running the
default. Five attempts went into that.

WHAT THIS DOES INSTEAD

It does not try to make the deadline fire. It tests the property that actually matters and that can
be observed immediately: **while a long request is in flight, does a second connection still get
answered?**

That is the question the whole change is about. The guest serves connections on their own threads, so
a request that blocks one connection must not block the listener. If a second connection answers
promptly while the first is busy, the channel is not hostage to a single request — which is exactly
what was broken before, when a second connection got nothing for thirty seconds.

This distinguishes the two failure modes:
  - The accept loop blocks on a busy connection  -> second connection times out   (FAIL)
  - The accept loop stays reachable              -> second connection answers     (PASS)

The deadline itself is covered by unit tests (`wvm-guest/src/deadline.rs`) and by
`scripts/test-request-deadline.py`, which needs a guest started with a short deadline and says so.

The long request used here is deliberately bounded well under the default backstop, so the probe
leaves nothing wedged behind it. It is a busy request, not an abandoned one.

Usage:  python3 scripts/verify-request-deadline.py [--port 48274]

Exit:   0 the channel stayed reachable   1 it did not   2 could not run
"""
import argparse
import json
import socket
import struct
import sys
import threading
import time


def one(port, req, timeout):
    """Send one request. Returns (elapsed, reply_or_reason). Never raises."""
    t0 = time.monotonic()
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        s.settimeout(timeout)
        b = json.dumps(req).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
        header = b""
        while len(header) < 4:
            part = s.recv(4 - len(header))
            if not part:
                return (time.monotonic() - t0, "CLOSED")
            header += part
        (n,) = struct.unpack(">I", header)
        body = b""
        while len(body) < n:
            part = s.recv(n - len(body))
            if not part:
                break
            body += part
        s.close()
        return (time.monotonic() - t0, json.loads(body))
    except socket.timeout:
        return (time.monotonic() - t0, "TIMEOUT")
    except Exception as e:
        return (time.monotonic() - t0, type(e).__name__)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--busy-seconds", type=int, default=45,
                    help="how long the blocking request runs; well under the 15-minute backstop")
    args = ap.parse_args()

    hello = one(args.port, {"op": "hello", "protocol_version": 1, "client": "deadline-probe"}, 15)
    _, reply = hello
    if not isinstance(reply, dict) or reply.get("status") != "ready":
        print(f"  no guest on {args.port}: {reply}")
        return 2
    print(f"  hello -> {reply.get('status')}")

    # Occupies one connection for a bounded time. `ping -n N` counts packets, so this is roughly N
    # seconds and it works under the redirected stdin this harness always provides — `timeout /t`
    # does not, and exits in milliseconds (D-013).
    busy = {"op": "exec", "program": "cmd.exe",
            "args": ["/c", f"ping -n {args.busy_seconds} 127.0.0.1"],
            "cwd": None, "require_allowlist": False,
            "timeout_ms": (args.busy_seconds + 30) * 1000}

    result = {}

    def hold():
        result["elapsed"], result["reply"] = one(args.port, busy, timeout=args.busy_seconds + 60)

    t = threading.Thread(target=hold, daemon=True)
    t.start()
    time.sleep(3)  # let the busy request actually reach the guest

    # Now the question that matters: is a NEW connection still served?
    print(f"  polling a second connection while the first is busy:")
    worst = 0.0
    answered = 0
    for i in range(6):
        el, rep = one(args.port, {"op": "inspect"}, timeout=10)
        ok = isinstance(rep, dict) and rep.get("status") == "ok"
        worst = max(worst, el)
        if ok:
            answered += 1
        print(f"    t+{(i + 1) * 4:>2}s  {el:5.1f}s  {'ok' if ok else rep}")
        time.sleep(1)

    print()
    print(f"  {answered}/6 second-connection requests were answered (slowest {worst:.1f}s)")

    problems = []
    if answered < 6:
        problems.append(
            f"only {answered}/6 requests on a second connection were answered while the first was "
            f"busy — the accept loop is blocked by a busy connection"
        )
    if worst > 3.0:
        problems.append(
            f"the slowest second-connection reply took {worst:.1f}s; a listener that is reachable "
            f"answers promptly"
        )

    t.join(timeout=10)

    # The busy request should complete normally — this is a BUSY request, not an abandoned one, and
    # conflating the two would mean the probe itself leaves the guest in a state it should not.
    r = result.get("reply")
    if isinstance(r, dict) and r.get("status") == "ok":
        print(f"  the busy request completed normally after {result.get('elapsed', 0):.0f}s")
    else:
        print(f"  the busy request ended as {r} (allowed: the deadline may have fired first)")

    print()
    if problems:
        print("CHANNEL WEDGE PROBE FAILED:")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("PASSED: a busy request does not block the listener; the channel stays reachable.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
