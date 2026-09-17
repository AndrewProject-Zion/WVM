#!/usr/bin/env python3
"""What does a SPICE display server cost when nobody is watching? Measured on controlled QEMUs.

WHY NOT MEASURE THE REAL VM

First attempt measured the live Windows guest: QEMU consumed 136% of one core. The guest does its
own background work on its own schedule, so any A/B there is dominated by Windows rather than by the
display server -- and "the delta is lost in the noise" is not an answer to a design question.

So the subject is controlled: throwaway QEMUs with no disk, identical memory, differing ONLY in the
display arguments. Same binary, same host, same session. Any difference is the display server's.

THE ARM THAT MAKES THIS TRUSTWORTHY, AND WHY IT IS FIRST

Every arm below halts the vCPU (`-S`), so the honest expected answer is "about zero" from all of
them. That is precisely the shape of result that a BROKEN probe also produces -- three zeros look
like a perfect finding whether the probe works or not. This already happened once: the first version
of this script reported 0.00% for every arm including one with a viewer attached, and there was no
way to tell a real zero from a dead instrument.

So arm D is a positive control: the same QEMU with the vCPU RUNNING, which must burn CPU. If the
control reads zero, the probe cannot measure anything and the script says so instead of printing
numbers. A measurement that cannot fail is not a measurement.

VIEWER ARM IS OPT-IN

Spawning `remote-viewer` puts a window on the operator's desktop. Doing that unannounced is rude and
was reported as such, so it only runs with --with-viewer.
"""

import argparse
import pathlib
import signal
import subprocess
import sys
import time

SOCK = "/tmp/wvm-spice-probe.sock"
QEMU = "qemu-system-x86_64"
CLK_TCK = 100


def cpu_ticks(pid):
    """utime + stime for the whole process. Parsed from after the LAST ')' because the comm field
    can contain spaces and parentheses -- a fixed field index is how this parser gets written wrong."""
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    fields = stat.rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def measure(argv, label, seconds, with_viewer):
    pathlib.Path(SOCK).unlink(missing_ok=True)
    proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                            stdin=subprocess.DEVNULL)
    viewer = None
    try:
        time.sleep(2.0)
        if proc.poll() is not None:
            pipe = proc.stderr
            text = pipe.read().decode(errors="replace").strip() if pipe else ""
            last = text.splitlines()[-1] if text else "no output"
            print(f"  {label:<34} qemu exited immediately: {last}")
            return None

        if with_viewer:
            viewer = subprocess.Popen(["remote-viewer", f"spice+unix://{SOCK}"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                      stdin=subprocess.DEVNULL)
            time.sleep(4.0)
            if viewer.poll() is not None:
                print(f"  {label:<34} viewer exited without attaching — arm void")
                return None
            # Confirm a peer is actually on the socket. "A process started" is not "a viewer is
            # attached", and only the second one makes this arm mean anything.
            ss = subprocess.run(["ss", "-x"], capture_output=True, text=True).stdout
            if SOCK not in ss:
                print(f"  {label:<34} no peer on the socket — viewer did not connect")
                return None

        start = cpu_ticks(proc.pid)
        t0 = time.monotonic()
        time.sleep(seconds)
        end = cpu_ticks(proc.pid)
        wall = time.monotonic() - t0
        if start is None or end is None:
            print(f"  {label:<34} qemu died during the sample")
            return None
        ticks = end - start
        return ticks, 100.0 * ((ticks / CLK_TCK) / wall)
    finally:
        if viewer is not None and viewer.poll() is None:
            viewer.terminate()
            try:
                viewer.wait(timeout=5)
            except subprocess.TimeoutExpired:
                viewer.kill()
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        pathlib.Path(SOCK).unlink(missing_ok=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=int, default=30)
    ap.add_argument("--with-viewer", action="store_true",
                    help="also run the attached-viewer arm (opens a window on the desktop)")
    args = ap.parse_args()

    common = [QEMU, "-display", "none", "-nodefaults", "-monitor", "none",
              "-m", "256", "-machine", "q35"]
    halted = common + ["-S"]
    halted_spice = halted + ["-spice", f"unix=on,addr={SOCK},disable-ticketing=on"]

    arms = [
        ("D  POSITIVE CONTROL: vCPU running", common, False),
        ("A  no display server", halted, False),
        ("B  spice, nobody watching", halted_spice, False),
    ]
    if args.with_viewer:
        arms.append(("C  spice, viewer attached", halted_spice, True))

    print(f"  {args.seconds}s per arm, throwaway QEMUs differing only in display args\n")
    res = {}
    for label, argv, viewer in arms:
        got = measure(argv, label, args.seconds, viewer)
        res[label] = got
        if got is not None:
            ticks, pct = got
            print(f"  {label:<34} {ticks:6d} ticks  {pct:8.3f}% of one core")

    control = res.get("D  POSITIVE CONTROL: vCPU running")
    if control is None or control[0] <= 0:
        print("\n  INSTRUMENT FAILED — the positive control showed no CPU, so every number above")
        print("  is meaningless. Fix the probe; do not use this run as evidence.")
        return 3
    print("\n  positive control burned CPU as expected, so the probe can discriminate")

    a = res.get("A  no display server")
    b = res.get("B  spice, nobody watching")
    if a and b:
        print(f"  an ignored SPICE server costs {b[1] - a[1]:+8.3f}% of one core vs no server")
    c = res.get("C  spice, viewer attached")
    if b and c:
        print(f"  an attached viewer adds       {c[1] - b[1]:+8.3f}% of one core")
    return 0


if __name__ == "__main__":
    sys.exit(main())
