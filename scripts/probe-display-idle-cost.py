#!/usr/bin/env python3
"""How much CPU does QEMU burn while idle, and how much does a SPICE server add?

WHY THIS GATES THE DESIGN

The display-on-request plan has two shapes and this number picks between them:

  A. An always-on display server (SPICE on a unix socket). Simple, standard, and the viewer
     attaches instantly -- but QEMU runs a display server whether or not anyone is watching, and
     the whole premise of this project is that an ignored VM should be cheap.

  B. Attach-on-demand only (`-display dbus` + `display-reload`, both present in this build).
     Genuinely zero cost until asked, but viewer support is unproven here.

The source sketch claimed the extra cost was "zero" and cited QMP screendump as the evidence. That
evidence is wrong -- screendump is independent of any display server and works with `-display none`
today, which is how every existing probe captures the desktop. So the claim is unverified, and this
probe is the verification.

WHAT IT MEASURES, PRECISELY

Total CPU time consumed by the QEMU process (utime + stime from /proc/<pid>/stat, which covers all
its threads) divided by wall time, over a sample window. That is the honest definition of "what this
costs to leave running".

Deliberately NOT measured: VRAM, since the framebuffer is allocated either way.

THE GUARD THAT MATTERS

A number is only meaningful if the machine under test was the same machine in both runs. The guest
is sampled after it has settled and the sample window is fixed, but the guest itself is not
controlled -- Windows does background work on its own schedule, and one run that happens to land on
a Windows Update check would look like SPICE costing 20% CPU. So the guest's own CPU is reported
alongside as a sanity check: if the two runs differ wildly in GUEST cpu, the comparison is not
valid and the honest answer is to re-run rather than to report a delta.
"""

import argparse
import pathlib
import time


def qemu_pids():
    """Pids of running qemu-system processes. Read from /proc rather than shelling out, so this
    cannot match the invoking shell's own command line (a trap that has produced a false 'process
    still running' result in this project before)."""
    out = []
    for p in pathlib.Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            comm = (p / "comm").read_text().strip()
        except OSError:
            continue
        if comm.startswith("qemu-system"):
            out.append(int(p.name))
    return out


def cpu_ticks(pid):
    """utime + stime in clock ticks, for the whole process (all threads)."""
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    # Field 2 is the comm and can contain spaces and parentheses, so split after the LAST ')':
    # taking a fixed field index here is how this kind of parser gets written wrong.
    fields = stat.rsplit(")", 1)[1].split()
    # After the comm, field 14 (utime) and 15 (stime) are at 0-indexed 11 and 12.
    return int(fields[11]) + int(fields[12])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=int, default=60, help="sample window")
    ap.add_argument("--label", default="run")
    args = ap.parse_args()

    pids = qemu_pids()
    if not pids:
        print("  no qemu-system process is running — nothing to measure")
        return 2
    if len(pids) > 1:
        print(f"  {len(pids)} qemu processes running; measuring the first only ({pids[0]})")
    pid = pids[0]

    hz = 100  # CLK_TCK on Linux; every mainstream config is 100
    start = cpu_ticks(pid)
    if start is None:
        print(f"  pid {pid} vanished before the sample started")
        return 1
    t0 = time.monotonic()
    time.sleep(args.seconds)
    end = cpu_ticks(pid)
    if end is None:
        print(f"  pid {pid} vanished during the sample — it was stopped or it exited")
        return 1
    wall = time.monotonic() - t0

    cpu_seconds = (end - start) / hz
    pct = 100.0 * cpu_seconds / wall

    print(f"  [{args.label}] qemu pid {pid}: {cpu_seconds:.2f} cpu-seconds over {wall:.0f}s "
          f"= {pct:.2f}% of one core")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
