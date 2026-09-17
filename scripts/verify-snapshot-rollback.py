#!/usr/bin/env python3
"""Prove a snapshot restore actually rolls the machine back, not just that the command succeeds.

THE DISTINCTION THIS TESTS

`snapshot-load` returning without error says the API accepted the request. It does NOT say the guest
went back in time. Those are different claims, and only the second is what a caller relies on when
they snapshot before letting an agent do something risky.

This test makes the difference observable:

  1. save a snapshot                      ("the machine as it is now")
  2. write a MARKER FILE inside the guest ("the agent did something")
  3. confirm the marker exists            (the write really happened)
  4. restore the snapshot                 ("undo it")
  5. confirm the marker is GONE           (the rollback really happened)

Step 3 matters as much as step 5. Without it, a restore that silently did nothing would report
success, and the test would agree with it — the marker would be absent because it was never created.

That is the same shape as every other measurement mistake in this project: a check that cannot fail
proves nothing. Here both halves are asserted, so the test can only pass if the write happened AND
the rollback undid it.

Usage:  python3 scripts/verify-snapshot-rollback.py [--port 48274] [--tag wvm-rollback-test]
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import time

STAGING = r"C:\ProgramData\wvm\staging"
MARKER = STAGING + r"\rollback-marker.txt"


def call(port, req, timeout=120):
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


def run_on_guest(port, arg, timeout=180):
    r = call(port, {"op": "exec", "program": "cmd.exe", "args": ["/c", arg],
                    "cwd": None, "require_allowlist": False, "timeout_ms": timeout * 1000},
             timeout=timeout + 20)
    return (r or {}).get("payload", {})


def marker_exists(port):
    """Ask the guest whether the marker file is there.

    `dir` rather than a file-existence API: it reports what the filesystem actually holds, and its
    exit code is reliable. Note the path is NOT quoted — quoting breaks argument handling through
    this exec layer (D-014), and it fails in a way that looks like the file is missing.
    """
    p = run_on_guest(port, f"dir /b {MARKER}")
    out = (p.get("stdout") or "").strip()
    return "rollback-marker.txt" in out, out


def wait_for_guest(port, deadline_s=240):
    """Wait until the guest answers, or give up. Returns True if it came back.

    A RESTORE FREEZES THE vCPUs while machine state is written, and no QMP message arrives for the
    whole of that write — measured at 57 seconds on a disk carrying nine snapshots. A fixed
    five-second sleep therefore turned a working restore into a reported failure, twice, and sent
    the investigation looking for corruption that was not there.

    Waiting for the answer is both correct and self-documenting: if the guest never returns, that is
    a real failure, because the freeze has been measured at under a minute.
    """
    end = time.time() + deadline_s
    while time.time() < end:
        try:
            if call(port, {"op": "hello", "protocol_version": 1, "client": "rollback"},
                    timeout=10) is not None:
                return True
        except Exception:
            pass
        time.sleep(3)
    return False


def wvm(*args):
    """Run the host binary and return (exit_code, stdout+stderr)."""
    r = subprocess.run(["./target/release/wvm", *args],
                       capture_output=True, text=True, cwd=".")
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--tag", default="wvm-rollback-test")
    ap.add_argument("--config", default=str(__import__("pathlib").Path.home() / "wvm/wvm.toml"))
    args = ap.parse_args()

    failures = []

    hello = call(args.port, {"op": "hello", "protocol_version": 1, "client": "rollback"})
    if not hello or hello.get("status") != "ready":
        print(f"  no guest on {args.port}: {hello}")
        return 2
    print(f"  guest: {hello.get('guest')}")

    # --- 1. clear any leftover marker, so step 5 cannot pass for the wrong reason ---
    run_on_guest(args.port, f"del /f /q {MARKER}")
    present, out = marker_exists(args.port)
    if present:
        failures.append(f"could not clear the marker before starting: {out}")
        print(f"  FAIL: a marker from a previous run is still there: {out}")
    else:
        print("  marker absent at the start (so its later return would be meaningful)")

    # --- 2. snapshot the machine ---
    print(f"  saving snapshot '{args.tag}'")
    code, out = wvm("vm", "snapshot", "save", args.tag, "--config", args.config)
    if code != 0:
        failures.append(f"save failed: {out.strip()[:200]}")
        print(f"  FAIL: {out.strip()[:200]}")
        return 1
    print(f"    {out.strip().splitlines()[-1]}")

    # --- 3. do something that changes the disk ---
    #
    # WAIT for the guest before acting on it. A save freezes the vCPUs while machine state is
    # written, and after a large one the guest can stay starved for longer still: a verification
    # round timed out writing this marker (180s) and then the guest answered normally moments later.
    # Waiting is correct; treating that pause as a failure is what made a working save look broken.
    if not wait_for_guest(args.port):
        failures.append("the guest did not answer after the save, before the marker was written")
        print("  FAIL: the guest never answered after the save")
    print("  writing a marker file inside the guest")
    p = run_on_guest(args.port, f"echo rollback-test-marker > {MARKER}")
    if p.get("code") != 0:
        failures.append(f"could not write the marker: {(p.get('stderr') or '')[:120]}")
        print(f"  FAIL: marker write returned {p.get('code')}")

    # --- 4. prove the write happened, or step 5 proves nothing ---
    present, out = marker_exists(args.port)
    if not present:
        failures.append("the marker was not created, so the rollback cannot be measured")
        print(f"  FAIL: the marker was not created: {out[:120]}")
    else:
        print("  marker confirmed present (the write really happened)")

    # --- 5. restore ---
    print(f"  restoring '{args.tag}'")
    code, out = wvm("vm", "snapshot", "restore", args.tag, "--config", args.config)
    if code != 0:
        failures.append(f"restore failed: {out.strip()[:200]}")
        print(f"  FAIL: {out.strip()[:200]}")
    else:
        print(f"    {out.strip().splitlines()[-1]}")

    # --- 6. the guest should be back, and the write should be gone ---
    #
    # WAIT for it rather than sleeping a token amount. See `wait_for_guest`: the restore freezes
    # the vCPUs for as long as the machine state takes to write, which has been measured at nearly
    # a minute. A five-second sleep here reported a working restore as a failure.
    if not wait_for_guest(args.port):
        failures.append(
            "the guest did not answer within 240s of the restore. The freeze while state is "
            "written has been measured at under a minute, so this is a real failure, not slowness."
        )
        print("  FAIL: the guest never came back after the restore")
    else:
        print("  guest answering again (the freeze while state is written is expected)")

    present, out = marker_exists(args.port)
    if present:
        failures.append(
            "the marker is STILL THERE after a restore — the snapshot loaded but the disk was not "
            "rolled back"
        )
        print(f"  FAIL: the marker survived the restore: {out[:120]}")
    else:
        print("  marker is GONE — the restore rolled the disk back")

    # --- 7. and the machine is still usable afterwards ---
    p = run_on_guest(args.port, "ver")
    if p.get("code") == 0:
        print(f"  guest still usable: {(p.get('stdout') or '').strip()[:60]}")
    else:
        failures.append(f"the guest did not answer after the restore: {p.get('code')}")
        print(f"  FAIL: guest unresponsive after restore")

    print()
    if failures:
        print("SNAPSHOT ROLLBACK FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASSED: a snapshot restore really does roll the machine back.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
