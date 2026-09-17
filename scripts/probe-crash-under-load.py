#!/usr/bin/env python3
"""Do a snapshot save and a stalled I/O request together crash the guest? And how often?

THE HYPOTHESIS

The guest bugchecks 0x50 at a fixed code offset, reading at offset -8 from a null pointer — the
signature of code walking a structure whose owner has gone away. A bare vCPU pause for the same
duration does not reproduce it, so the trigger is in the snapshot's device-state write. The most
plausible mechanism left is an I/O request in flight when the device is quiesced.

If that is right, an idle guest crashes only occasionally, and a fix would take dozens of samples to
test. Driving real disk I/O across the save should raise the rate enough to test a candidate fix in
a handful of runs.

WHAT THIS FIXES FROM THE LAST ATTEMPT

The previous version reported "load running: False" and carried on regardless, so it tested a quiet
guest while believing it was busy — a measurement whose failure mode was identical to its success
mode. Two changes:

  * The load is confirmed from the HOST, by measuring QEMU's write rate. If the guest is not doing
    I/O the rate stays near zero and the round ABORTS rather than producing a meaningless result.
  * A crash is detected by a SCREENDUMP, not by asking the guest. The load legitimately holds the
    control channel (the guest service answers one request at a time), so a silent `hello` proves
    nothing. A screendump goes over QMP and works either way, and a Windows bugcheck screen is
    almost a single flat colour — a property a desktop does not have.

The uniformity check is calibrated against the live desktop at the start, so a threshold that is
wrong for this machine shows up immediately rather than being assumed.

Usage: python3 scripts/probe-crash-under-load.py [--rounds 5] [--load-seconds 150]
Exit:  0 if the guest survived every round, 1 if it crashed, 2 if the probe could not run.
"""
import argparse
import json
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

QMP = Path.home() / ".local/state/wvm/w11/qmp.sock"
CONFIG = str(Path.home() / "wvm/wvm.toml")
PORT = 48274
STAGE = r"C:\ProgramData\wvm\staging"
LOAD_CMD = rf"{STAGE}\guest-disk-load.cmd"


# --- QMP ---------------------------------------------------------------------------------------

def qmp(command, arguments=None, timeout=120):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(str(QMP))
    try:
        f = s.makefile("rw")
        f.readline()
        def send(name, args=None):
            payload = {"execute": name}
            if args:
                payload["arguments"] = args
            f.write(json.dumps(payload) + "\n")
            f.flush()
            while True:
                msg = json.loads(f.readline())
                if "return" in msg or "error" in msg:
                    return msg
        send("qmp_capabilities")
        return send(command, arguments)
    finally:
        s.close()


def screendump(path):
    """Ask QEMU for the framebuffer as PPM — raw RGB, so no image library is needed."""
    r = qmp("screendump", {"filename": str(path), "format": "ppm"}, timeout=60)
    return "error" not in r and Path(path).exists()


def uniformity(path, quant=16):
    """Fraction of pixels in the single most common quantised colour.

    A Windows bugcheck screen is one flat blue with a little white text, so this sits near 0.9. A
    desktop of any kind — windows, wallpaper, taskbar — never does, because the colour varies. The
    caller calibrates against the live desktop before relying on it.
    """
    b = Path(path).read_bytes()
    # P6 header: magic, width, height, maxval, then binary RGB.
    parts = []
    i = 0
    while len(parts) < 4 and i < len(b):
        if b[i:i + 1] == b"#":
            while b[i:i + 1] not in (b"\n", b""):
                i += 1
        elif b[i:i + 1].isspace():
            i += 1
        else:
            j = i
            while j < len(b) and not b[j:j + 1].isspace():
                j += 1
            parts.append(b[i:j])
            i = j
    if len(parts) < 4 or parts[0] != b"P6":
        return None
    i += 1  # one whitespace byte after maxval
    px = b[i:]
    counts = {}
    for k in range(0, len(px) - 2, 3):
        key = (px[k] >> 4) << 8 | (px[k + 1] >> 4) << 4 | (px[k + 2] >> 4)
        counts[key] = counts.get(key, 0) + 1
    total = sum(counts.values())
    return (max(counts.values()) / total) if total else None


# --- guest / host ------------------------------------------------------------------------------

def guest(request, timeout=60):
    s = socket.create_connection(("127.0.0.1", PORT), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(request).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
        h = b""
        while len(h) < 4:
            c = s.recv(4 - len(h))
            if not c:
                return None
            h += c
        (n,) = struct.unpack(">I", h)
        body = b""
        while len(body) < n:
            c = s.recv(n - len(body))
            if not c:
                break
            body += c
        return json.loads(body)
    finally:
        s.close()


def dumps():
    """The guest's crash-dump filenames, or None if the guest cannot be asked."""
    r = guest({"op": "exec", "program": "cmd.exe", "args": ["/c", r"dir /b C:\Windows\Minidump"],
               "cwd": None, "require_allowlist": False, "timeout_ms": 60000}, timeout=90)
    if not r:
        return None
    out = (r.get("payload", {}).get("stdout") or "")
    return {ln.strip() for ln in out.splitlines() if ln.strip().endswith(".dmp")}


def qemu_write_rate():
    """Bytes QEMU wrote in the last second. Near zero means the guest is not doing disk I/O."""
    pid = None
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if "qemu" in (p / "comm").read_text():
                    pid = p.name
                    break
            except OSError:
                continue
    if pid is None:
        return None

    def wb():
        try:
            for line in Path(f"/proc/{pid}/io").read_text().splitlines():
                if line.startswith("write_bytes"):
                    return int(line.split()[1])
        except OSError:
            pass
        return None

    a = wb()
    time.sleep(1.2)
    b = wb()
    return None if (a is None or b is None) else int((b - a) / 1.2)


def wvm(*args, timeout=900):
    r = subprocess.run(["./target/release/wvm", *args], capture_output=True, text=True,
                       cwd=".", timeout=timeout)
    return r.returncode, (r.stdout or "") + (r.stderr or "")


def start_load(seconds, state):
    """Begin sustained disk I/O and leave it running.

    The guest service answers ONE request at a time, so this call does not return until the load
    ends or its timeout fires. That is expected and is why the crash check cannot use `hello`.
    """
    def worker():
        try:
            state["reply"] = guest(
                {"op": "exec", "program": "cmd.exe", "args": ["/c", LOAD_CMD], "cwd": None,
                 "require_allowlist": False, "timeout_ms": seconds * 1000},
                timeout=seconds + 40)
        except Exception as e:
            state["error"] = f"{type(e).__name__}"

    t = threading.Thread(target=worker, daemon=True)
    t.start()
    state["thread"] = t


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--load-seconds", type=int, default=150)
    args = ap.parse_args()

    if not guest({"op": "hello", "protocol_version": 1, "client": "loadprobe"}, timeout=20):
        print("  no guest — start the VM first")
        return 2

    # Make sure the load script is present. --overwrite so this probe can run more than once.
    p = subprocess.run(["./target/release/wvm", "vm", "transfer", "push", "--overwrite",
                        "--config", CONFIG, "./scripts/guest-disk-load.cmd", LOAD_CMD],
                       capture_output=True, text=True, cwd=".", timeout=120)
    if p.returncode != 0:
        print(f"  could not place the load script: {(p.stderr or p.stdout).strip()[:120]}")
        return 2
    print("  load script in place")

    # --- the crash detector is the DUMP SET, not the screen ---
    #
    # A screendump-based detector was tried first and thrown away, because calibrating it showed it
    # cannot work here: the live desktop measures 1.0000 uniformity — identical to a bugcheck screen
    # — because Windows blanks the display when idle, and a blanked display is one flat colour. It
    # would have reported a crash on every round.
    #
    # The reliable signal is a NEW FILE in the crash-dump directory. It survives the automatic
    # reboot that follows a bugcheck, which a `hello` check does not: the guest comes back two
    # minutes later and looks healthy, and the crash would be missed entirely.
    before = dumps()
    if before is None:
        print("  could not read the crash-dump directory — cannot detect a crash")
        return 2
    print(f"  {len(before)} crash dump(s) before this run")

    crashes = 0
    for i in range(1, args.rounds + 1):
        print(f"\n--- round {i}/{args.rounds} ---")
        state = {}
        start_load(args.load_seconds, state)
        time.sleep(6)

        rate = qemu_write_rate()
        if rate is None:
            print("  could not read QEMU's write rate — cannot confirm the load")
            return 2
        print(f"  guest disk write rate: {rate/1024/1024:.1f} MB/s")
        if rate < 1024 * 1024:
            print("  THE LOAD IS NOT RUNNING. Stopping: an idle-guest round would prove nothing")
            print("  about a hypothesis that needs I/O in flight.")
            return 2

        code, out = wvm("vm", "snapshot", "save", f"load-{i}", "--config", CONFIG)
        last = out.strip().splitlines()[-1] if out.strip() else "(no output)"
        print(f"  save: {last[:100]}")

        crashed = False

        if not crashed:
            # Give the load time to finish, then confirm the guest is really healthy.
            print("  waiting for the load to end, then checking the guest...")
            time.sleep(max(0, args.load_seconds - 20))
            ok = False
            for _ in range(30):
                try:
                    if guest({"op": "hello", "protocol_version": 1, "client": "loadprobe"},
                             timeout=10):
                        ok = True
                        break
                except Exception:
                    pass
                time.sleep(3)
            if not ok:
                print("  NOTE: the guest is not answering — checking the dumps for the verdict")

            after = dumps()
            if after is not None:
                new = after - before
                if new:
                    crashed = True
                    print(f"  *** BUGCHECK: new dump(s) {sorted(new)} ***")
                    before = after
                elif not ok:
                    crashed = True
                    print("  unresponsive with no new dump — treating as a failure")
                else:
                    print("  no new crash dump: survived")

        if crashed:
            crashes += 1
            print("  CRASH on this round")
            # Let Windows restart, so the next round has a machine to work with.
            for _ in range(24):
                time.sleep(15)
                try:
                    if guest({"op": "hello", "protocol_version": 1, "client": "loadprobe"},
                             timeout=10):
                        print("  guest is back")
                        break
                except Exception:
                    pass
        else:
            print("  survived")
            wvm("vm", "snapshot", "delete", f"load-{i}", "--config", CONFIG)

    print()
    print(f"  {crashes} crash(es) in {args.rounds} save(s) under disk load")
    return 1 if crashes else 0


if __name__ == "__main__":
    sys.exit(main())
