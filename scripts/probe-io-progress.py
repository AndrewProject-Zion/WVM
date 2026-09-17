#!/usr/bin/env python3
"""Is QEMU's I/O actually progressing, or is it hung waiting for completions that never come?

WHY THIS EXISTS

A snapshot save timed out, then the guest stopped answering, and `query-status` over QMP also timed
out — while QEMU was still alive with an 8.5 GB RSS. The process wchan is `io_cqring_wait`, so it is
blocked on io_uring completions.

That is a completely different diagnosis depending on one measurement:

  * completions ARRIVING slowly  -> QEMU is busy writing 3.4 GB of machine state into a fragmented
    53 GB qcow2. Slow but healthy. The client's timeouts are simply too short.
  * completions NOT arriving     -> a kernel/storage I/O hang. Nothing in this project can cause or
    fix that, and no timeout value would help.

Guessing between those two would produce a confident wrong explanation, which is the failure this
project keeps having to correct. So measure it.

Usage: python3 scripts/probe-io-progress.py [pid] [--seconds 6]
"""
import argparse
import os
import subprocess
import sys
import time
from pathlib import Path


def read_int(path, key=None):
    try:
        text = Path(path).read_text()
    except OSError:
        return None
    if key is None:
        try:
            return int(text.strip())
        except ValueError:
            return None
    for line in text.splitlines():
        if line.startswith(key):
            parts = line.split()
            for p in parts[1:]:
                if p.isdigit():
                    return int(p)
    return None


def sample(pid, disk):
    """One reading of the things that would move if I/O is progressing."""
    out = {}
    out["write_bytes"] = read_int(f"/proc/{pid}/io", "write_bytes")
    try:
        out["disk_size"] = os.path.getsize(disk)
    except OSError:
        out["disk_size"] = None
    # The device's sectors-written counter, which moves even if QEMU's own accounting lags.
    try:
        dev = os.stat(disk).st_dev
        major, minor = os.major(dev), os.minor(dev)
    except OSError:
        major = minor = None
    out["dev"] = (major, minor)
    sectors = None
    if major is not None:
        real = Path("/sys/dev/block/%d:%d" % (major, minor))
        try:
            name = os.path.basename(os.path.realpath(real))
            for line in Path("/proc/diskstats").read_text().splitlines():
                parts = line.split()
                if len(parts) > 9 and parts[2] == name:
                    sectors = int(parts[9])  # sectors written
                    break
        except OSError:
            pass
    out["sectors_written"] = sectors
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("pid", nargs="?", type=int)
    ap.add_argument("--seconds", type=float, default=6.0)
    ap.add_argument("--disk", default=str(Path.home() / "wvm/w11/disk.qcow2"))
    args = ap.parse_args()

    pid = args.pid
    if pid is None:
        # Find it rather than asking, so the script works with no arguments.
        for p in Path("/proc").iterdir():
            if p.name.isdigit() and (p / "comm").exists():
                try:
                    if "qemu" in (p / "comm").read_text():
                        pid = int(p.name)
                        break
                except OSError:
                    continue
    if pid is None:
        print("  no qemu process found")
        return 2

    print(f"  qemu pid {pid}")
    print(f"  wchan: {read_int(f'/proc/{pid}/wchan') or Path(f'/proc/{pid}/wchan').read_text().strip()}")
    state = "?"
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("State:"):
                state = line.split(":", 1)[1].strip()
    except OSError:
        pass
    print(f"  state: {state}")

    a = sample(pid, args.disk)
    time.sleep(args.seconds)
    b = sample(pid, args.disk)

    def delta(k):
        if a.get(k) is None or b.get(k) is None:
            return "n/a"
        return b[k] - a[k]

    wb = delta("write_bytes")
    ds = delta("disk_size")
    sw = delta("sectors_written")

    print()
    print(f"  over {args.seconds:.0f}s:")
    if isinstance(wb, int):
        print(f"    qemu write_bytes   +{wb / 1024 / 1024:.1f} MB")
    else:
        print("    qemu write_bytes    n/a")
    if isinstance(ds, int):
        print(f"    disk.qcow2 grew    +{ds / 1024 / 1024:.1f} MB")
    else:
        print("    disk.qcow2 grew     n/a")
    if isinstance(sw, int):
        print(f"    device sectors     +{sw}  ({sw * 512 / 1024 / 1024:.1f} MB written)")
    else:
        print("    device sectors      n/a (diskstats unavailable)")

    print()
    moving = any(isinstance(v, int) and v > 0 for v in (wb, ds, sw))
    if moving:
        print("  VERDICT: I/O IS PROGRESSING. QEMU is busy, not hung.")
        print("    -> the client's read timeouts are too short for the work")
    else:
        print("  VERDICT: NOTHING IS MOVING. This is an I/O hang, not a slow write.")
        print("    -> check the kernel log for storage errors; no timeout value helps")

    # Kernel-side evidence either way.
    try:
        d = subprocess.run(["sudo", "-n", "dmesg", "-T"], capture_output=True, text=True, timeout=20)
        hits = [
            ln for ln in d.stdout.splitlines()
            if any(k in ln.lower() for k in ("i/o error", "nvme", "timeout", "reset", "hung"))
        ]
        if hits:
            print()
            print("  recent kernel storage messages:")
            for ln in hits[-8:]:
                print(f"    {ln[:150]}")
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
