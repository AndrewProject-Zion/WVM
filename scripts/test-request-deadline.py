#!/usr/bin/env python3
"""Prove the guest abandons a wedged request instead of holding the channel forever.

THE GAP THIS TESTS

`exec` takes a caller-supplied timeout, so a hung COMMAND is bounded. That bounds one verb, not the
channel. The guest serves one request at a time, so a request that never returns for any other reason
— a bug, a blocking path, a verb added later with no timeout — holds the connection, and the host
cannot tell a busy guest from a wedged one from a dead one.

THE TRICK: MAKE THE DEADLINE REACHABLE

The production deadline is fifteen minutes, which cannot be tested by waiting. The guest reads
`WVM_REQUEST_DEADLINE_SECS` for exactly this reason, so this probe runs a guest with a 5-second
deadline, sends a request that will take far longer, and checks two things:

  1. The host gets an ANSWER rather than silence — a timeout response naming what happened.
  2. A LATER request on a fresh connection still works, which is the actual property: the channel
     survived the wedged request.

Point 2 is the one that matters. Point 1 could be satisfied by a guest that answers and then dies.

The wedging request is `exec` running a delay of 600 seconds with its OWN timeout set to 600 too, so
`exec`'s internal bounding does not rescue it. That is deliberate: the probe must exercise the
request-level deadline, not the per-verb one that already existed.

WHAT THIS DOES NOT PROVE

The abandoned work is not cancelled. Rust cannot safely cancel arbitrary code, so the worker thread
keeps running until it finishes. This probe asserts the CHANNEL is released, and the response says so
in as many words rather than implying a clean stop.

Usage:  python3 scripts/test-request-deadline.py [--port 48276]
"""
import argparse
import json
import socket
import struct
import sys
import time

HOST = "127.0.0.1"


def call(port, req, timeout=60):
    """One request/response. Returns the parsed reply, or None if the peer went away."""
    s = socket.create_connection((HOST, port), timeout=timeout)
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48276,
                    help="a guest started with a SHORT WVM_REQUEST_DEADLINE_SECS")
    ap.add_argument("--deadline", type=int, default=5,
                    help="the deadline the guest under test was started with")
    args = ap.parse_args()

    failures = []

    hello = call(args.port, {"op": "hello", "protocol_version": 1, "client": "deadline-probe"})
    print(f"hello -> {hello}")
    if not hello or hello.get("status") != "ready":
        print("no guest on this port. Start one with a short deadline first:")
        print(f"  WVM_REQUEST_DEADLINE_SECS={args.deadline} wvm-guest.exe --port {args.port}")
        return 2

    # --- 1. send a request that will outlast the deadline -------------------------------------
    #
    # `exec` with its own timeout set far above the request deadline, so the per-verb bound cannot
    # be what saves us. If the request-level deadline is the thing that works, this proves it.
    print()
    print(f"SEND a request that takes ~600s, against a {args.deadline}s deadline")
    long_cmd = {"op": "exec", "program": "cmd.exe",
                "args": ["/c", "ping -n 600 127.0.0.1"],
                "cwd": None, "require_allowlist": False,
                "timeout_ms": 600_000}

    t0 = time.monotonic()
    reply = call(args.port, long_cmd, timeout=args.deadline + 60)
    waited = time.monotonic() - t0
    print(f"  answered after {waited:.1f}s: {reply}")

    if reply is None:
        failures.append("the guest never answered and the connection closed — the channel was held")
    else:
        msg = str(reply.get("message", ""))
        if reply.get("status") != "error":
            failures.append(f"expected a timeout error, got {reply}")
        elif "did not finish" not in msg and "deadline" not in msg.lower():
            failures.append(f"the reply does not name a deadline: {msg[:120]}")
        else:
            print("  the guest answered instead of holding the connection")
        # It must NOT claim the work was cancelled, because it was not.
        if "NOT cancelled" in msg:
            print("  and it says plainly that the abandoned work was not cancelled")
        else:
            failures.append(
                "the timeout reply must state that the work was not cancelled; implying a clean "
                "stop would be a claim the guest cannot support"
            )
        if waited > args.deadline * 3:
            failures.append(
                f"the deadline took {waited:.1f}s to fire against a {args.deadline}s setting — the "
                "mechanism is not respecting its own value"
            )

    # --- 2. the property that actually matters -------------------------------------------------
    #
    # The channel must be usable again. A guest that answers and then dies would satisfy step 1.
    print()
    print("THE CHANNEL still works on a NEW connection")
    ok = False
    for attempt in range(3):
        r = call(args.port, {"op": "inspect"}, timeout=20)
        if r and r.get("status") == "ok":
            ok = True
            print(f"  attempt {attempt + 1}: the guest answered structurally")
            break
        print(f"  attempt {attempt + 1}: {r}")
        time.sleep(1)
    if not ok:
        failures.append("the guest did not answer a later request — the channel was not released")

    # --- 3. a NORMAL request still completes ---------------------------------------------------
    print()
    print("A NORMAL request still completes (the deadline must not fire on real work)")
    r = call(args.port, {"op": "exec", "program": "cmd.exe", "args": ["/c", "ver"],
                         "cwd": None, "require_allowlist": False, "timeout_ms": 20000}, timeout=40)
    if r and r.get("status") == "ok":
        print("  exec answered normally")
    else:
        failures.append(f"a short exec failed: {r}")

    print()
    if failures:
        print("DEADLINE FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASSED: a wedged request is abandoned, the channel survives, and normal work still runs.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
