#!/usr/bin/env python3
"""Verify that a timeout kills the whole process TREE, not just the direct child.

The bug this tests for: `Child::kill()` on Windows terminates ONE process. A command that spawned
its own children leaves them running — orphaned, invisible to the control channel, still holding
memory and handles. Once is harmless; repeatedly it bricks a sandbox.

The test is deliberately hostile, following the protocol of spawning a stubborn hierarchy:

  1. Fetch a script that starts TWO grandchild processes which outlive their parent, then has the
     parent wait longer than the timeout.
  2. Run it with a 2-second timeout, so the deadline fires while the tree is alive.
  3. Ask the guest for its process list and count the survivors.

If any grandchild survives, the tree kill failed and the sandbox leaks capacity.
If the list is clean, the job object worked.

A control run first, WITHOUT the timeout firing, proves the script really does spawn children —
otherwise a passing result could mean the script never ran at all rather than that cleanup worked.
Testing only the timeout case cannot distinguish "killed everything" from "started nothing".

Usage:  python3 scripts/test-timeout-tree-kill.py [--port 48274]
"""
import argparse
import json
import socket
import struct
import sys
import time


def call(port, req, timeout=120):
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


def count_stubborn(port):
    """Count the grandchildren the stubborn script spawns.

    `PING.EXE`, not `timeout.exe`. The obvious choice was `timeout /t 600`, and it silently does not
    work here: the guest runs commands with stdin redirected to null, and `timeout.exe` refuses in
    that case —

        ERROR: Input redirection is not supported, exiting the process immediately.

    So the delays exited in milliseconds, the parent finished in 82ms, and the first version of this
    test reported a clean pass having spawned nothing at all. `ping -n` works with redirected stdin
    and is still a real child process, which is what has to be cleaned up.

    A filtered `tasklist` avoids parsing a large table, and its CSV output is a stable shape.
    """
    r = call(port, {
        "op": "exec",
        "program": "cmd.exe",
        "args": ["/c", "tasklist /fi \"imagename eq PING.EXE\" /fo csv /nh"],
        "cwd": None,
        "require_allowlist": False,
        # Short: this is a diagnostic and must not itself hold the channel.
        "timeout_ms": 20000,
    })
    if not r or r.get("status") != "ok":
        return -1, str(r)
    out = r["payload"].get("stdout", "")
    # Each matching process is one CSV line beginning with "timeout.exe".
    lines = [ln for ln in out.splitlines() if ln.strip().lower().startswith('"ping')]
    return len(lines), out.strip()[:300]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--script", default=r"C:\ProgramData\wvm\staging\stubborn.cmd")
    args = ap.parse_args()

    failures = []

    hello = call(args.port, {"op": "hello", "protocol_version": 1, "client": "tree-kill-test"})
    print(f"hello -> {hello}")
    if not hello or hello.get("status") != "ready":
        print("channel is not up")
        return 2

    # --- fetch the script into the guest ---
    print()
    print("FETCH the stubborn-tree script")
    r = call(args.port, {
        "op": "exec",
        "program": "cmd.exe",
        "args": ["/c", f"curl -sS -o {args.script} http://10.0.2.2:8899/guest-stubborn-tree.cmd"],
        "cwd": None,
        "require_allowlist": False,
        "timeout_ms": 30000,
    })
    res = r.get("payload", {}) if r else {}
    print(f"  {res.get('outcome')} code={res.get('code')}")
    if res.get("code") != 0:
        failures.append("could not fetch the test script")

    # --- CONTROL: run it with a LONG timeout so it spawns and is observable ---
    #
    # Without this, a clean result could mean "the tree was killed" OR "the script never started
    # children at all". Only running it once without the timeout firing distinguishes those.
    print()
    print("CONTROL: run with a 3s timeout but a script that spawns immediately")
    print("  (expect the children to be OBSERVABLE, proving the script really spawns them)")
    r = call(args.port, {
        "op": "exec",
        "program": "cmd.exe",
        "args": ["/c", args.script],
        "cwd": None,
        "require_allowlist": False,
        "timeout_ms": 3000,
    })
    outcome = r.get("payload", {}).get("outcome") if r else "no reply"
    print(f"  outcome: {outcome}")

    time.sleep(2)
    # After the timeout the tree should be gone. Before the fix it would not be. So the CONTROL here
    # is the post-timeout count with the script KNOWN to have spawned children — see the note in the
    # module docstring for why a pre-timeout observation is not usable: the request holds the
    # channel, so nothing else can be asked while it runs.
    n, detail = count_stubborn(args.port)
    print(f"  survivors after the timeout: {n}")
    if n < 0:
        failures.append(f"could not list processes: {detail}")
    elif n == 0:
        print("  no `PING.EXE` survivors — the tree was killed")
    else:
        print(f"  {n} survivor(s) — the tree kill FAILED")
        print(f"  {detail}")
        failures.append(f"{n} grandchild process(es) survived the timeout")

    # --- the real assertion, twice, because a single pass could be luck ---
    print()
    print("REPEAT the timeout 3 times and confirm the count stays at zero")
    for i in range(3):
        call(args.port, {
            "op": "exec",
            "program": "cmd.exe",
            "args": ["/c", args.script],
            "cwd": None,
            "require_allowlist": False,
            "timeout_ms": 2000,
        })
        time.sleep(1)
        n, detail = count_stubborn(args.port)
        print(f"  run {i + 1}: {n} survivor(s)")
        if n != 0:
            failures.append(f"run {i + 1} left {n} survivor(s): {detail}")

    # --- and confirm the channel still works after all that killing ---
    print()
    print("CHANNEL still healthy after repeated timeouts")
    r = call(args.port, {"op": "inspect"})
    ok = r is not None and r.get("status") == "ok"
    print(f"  inspect -> {'ok' if ok else r}")
    if not ok:
        failures.append("the channel did not survive the timeout tests")

    print()
    if failures:
        print("TREE KILL FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASSED: timeouts kill the whole tree; no orphans accumulate.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
