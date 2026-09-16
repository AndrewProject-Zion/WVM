#!/usr/bin/env python3
"""Remove the request-deadline override from the guest, properly.

WHY A SCRIPT FOR THIS

`setx /M VAR ""` does NOT unset a variable — it sets the value to two literal quote characters. So
the "clear" step of the deadline test left `WVM_REQUEST_DEADLINE_SECS=""` in the machine
environment. The guest's parser rejects `""` and falls back to the fifteen-minute default, so the
observable behaviour was right, but right by accident: the override was still present and only a
parsing failure kept it inert.

That is worth a script rather than a one-liner because the failure mode is invisible — the correct
behaviour appeared for the wrong reason, which is the exact shape of mistake this project keeps
finding. Removing the value from the registry is the operation that actually removes it.

Also restores the service failure policy, because a deadline test that leaves `restart/3600000` set
turns every future crash into an hour of downtime, silently.

Run from the host. Exits non-zero if anything could not be confirmed.

Usage:  python3 scripts/clear-guest-deadline.py [--port 48274]
"""
import argparse
import json
import socket
import struct
import sys
import time

VAR = "WVM_REQUEST_DEADLINE_SECS"
REG_PATH = r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
ORIGINAL_POLICY = "restart/5000/restart/10000/restart/30000"

# What the override is left at. Larger than the fifteen-minute default, so a leftover override can
# never make the backstop fire EARLIER than intended on real work — the only dangerous direction.
SAFE_SECONDS = 3600


def call(port, req, timeout=45):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(req).encode()
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


def run(port, arg, timeout=40):
    r = call(port, {"op": "exec", "program": "cmd.exe", "args": ["/c", arg],
                    "cwd": None, "require_allowlist": False, "timeout_ms": timeout * 1000},
             timeout=timeout + 20)
    return (r.get("payload") or {}) if r else {}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    args = ap.parse_args()
    problems = []

    # Set the override to an explicit large value rather than deleting it.
    #
    # Deleting would be cleaner and it does not work, for a reason worth recording: the key path is
    # `...\Control\Session Manager\Environment`, and `reg delete` cannot be given that space
    # through this exec layer. Passing it quoted makes `reg` treat the quote characters as part of
    # the key name ("Invalid syntax"); passing it unquoted splits on the space. The 8.3 short name
    # for `Session Manager` does not exist, so that route is closed too. All three measured.
    #
    # Setting a large explicit value gets the same outcome and is arguably better: an override that
    # says what it is beats one that is absent and indistinguishable from never having been set.
    # The guest's parser rejects anything unparseable and falls back to the default, so a wrong
    # value here is inert rather than dangerous.
    print("1. set the override back to a large explicit value")
    r = run(args.port, f"setx /M {VAR} {SAFE_SECONDS}")
    if "success" in ((r.get("stdout") or "") + (r.get("stderr") or "")).lower():
        print(f"   set to {SAFE_SECONDS}s ({SAFE_SECONDS // 60} minutes)")
    else:
        print(f"   setx said: {((r.get('stdout') or '') + (r.get('stderr') or '')).strip()[:100]}")

    print("2. restart the service so it re-reads the environment")
    # taskkill rather than `sc stop`: the service refuses control messages (1061) while a request is
    # in flight, and the SCM brings it back within five seconds under the restored policy.
    run(args.port, "taskkill /f /im wvm-guest.exe", timeout=20)
    time.sleep(12)

    print("3. confirm the running service sees a safe value")
    ok = False
    for attempt in range(5):
        r = run(args.port, f"set {VAR}")
        env = (r.get("stdout") or "").strip()
        if str(SAFE_SECONDS) in env:
            print(f"   the service reads {env!r} — a backstop that cannot fire early on real work")
            ok = True
            break
        print(f"   attempt {attempt + 1}: {env!r}")
        time.sleep(5)
    if not ok:
        problems.append(
            "the running service does not report the expected override value; it may be running "
            "with a short deadline, which WOULD cut off legitimate long operations"
        )

    print("4. restore the service failure policy")
    run(args.port, f"sc failure wvm-guest reset= 0 actions= {ORIGINAL_POLICY}")
    r = run(args.port, "sc qfailure wvm-guest")
    out = r.get("stdout") or ""
    for expected in ("5000", "10000", "30000"):
        if expected not in out:
            problems.append(f"the failure policy is missing the {expected}ms action")
    if not problems:
        print("   restart/5000/10000/30000 restored")

    print()
    if problems:
        print("NOT FULLY CLEANED UP:")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(
        f"CLEAN: override parked at {SAFE_SECONDS}s (a backstop that cannot fire early on real "
        f"work), service policy restored to restart/5000/10000/30000."
    )
    print(
        "       Note: the variable is SET, not absent. Deleting it is unreachable through the exec "
        "layer — the key path contains a space that reg cannot be given. See the comment at step 1."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
