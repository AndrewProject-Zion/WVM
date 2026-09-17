#!/usr/bin/env python3
"""Verify the audio/SPICE coupling rules before writing them into vm.rs.

The recipe came from a sub-agent. Its claims are about intent until something exercises them, and
these two are load-bearing: if they are wrong, the VM either refuses to boot (traps 1 and 2) or
silently has no audio. Each case below must FAIL IN A SPECIFIC WAY -- a case that merely exits
non-zero would also pass if the script itself were broken, so the expected text is asserted.

Throwaway QEMUs only. No disk, no display, no VM touched.
"""
import subprocess
import sys
import tempfile
import time
from pathlib import Path

QEMU = "qemu-system-x86_64"


def attempt(label: str, extra: list[str], expect: str) -> bool:
    """Start a throwaway QEMU. Return True if it behaved as the recipe predicts."""
    proc = subprocess.Popen(
        [QEMU, "-machine", "q35,accel=kvm", "-m", "64", "-display", "none", "-nodefaults", *extra],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    time.sleep(2.5)
    alive = proc.poll() is None
    out = ""
    if alive:
        proc.terminate()
        try:
            out = proc.communicate(timeout=10)[0] or ""
        except subprocess.TimeoutExpired:
            proc.kill()
    else:
        out = proc.communicate()[0] or ""

    ok_text = expect.lower() in out.lower()
    if ok_text:
        print(f"  PASS  {label}")
        print("        " + " ".join(out.split())[:150])
        return True
    print(f"  FAIL  {label}")
    print(f"        alive={alive}  wanted {expect!r} in output")
    print("        got: " + " ".join(out.split())[:220])
    return False


def main() -> int:
    with tempfile.TemporaryDirectory() as td:
        sock = str(Path(td) / "s.sock")
        spice = [f"-spice", f"unix=on,addr={sock},disable-ticketing=on"]
        hda = ["-device", "ich9-intel-hda", "-device", "hda-duplex,audiodev=snd0"]
        audiodev = ["-audiodev", "spice,id=snd0"]

        print("=== the coupling rules, each expected to fail in its own way ===")
        results = []

        # TRAP 1: an hda device with no -audiodev. The recipe says QEMU 11.1 refuses to start.
        results.append(
            attempt(
                "hda device with NO -audiodev  -> refuses to start",
                spice + ["-device", "ich9-intel-hda", "-device", "hda-duplex"],
                "no default audio driver available",
            )
        )

        # TRAP 2: -audiodev spice with no -spice. This is why the audio args must be gated on the
        # SAME condition as the display server, or a SPICE-off config will not boot at all.
        results.append(
            attempt(
                "-audiodev spice with NO -spice -> refuses to start",
                audiodev + hda,
                "Cannot use spice audio without -spice",
            )
        )

        # THE RECIPE: both together must be accepted. Without this a broken script would "pass".
        results.append(
            attempt(
                "spice + audiodev spice + hda  -> starts clean",
                spice + audiodev + hda,
                "",
            )
        )

        # The positive control for case 3: it must be RUNNING, not merely quiet. A QEMU that died
        # instantly would produce empty output and also "contain" the empty expected string.
        proc = subprocess.Popen(
            [QEMU, "-machine", "q35,accel=kvm", "-m", "64", "-display", "none", "-nodefaults",
             *spice, *audiodev, *hda],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        time.sleep(3)
        alive = proc.poll() is None
        proc.terminate()
        proc.communicate(timeout=10)
        print(f"  {'PASS' if alive else 'FAIL'}  the full recipe is ALIVE after 3s (not merely silent)")
        results.append(alive)
        print(f"        socket created: {Path(sock).exists()}")

    print()
    if all(results):
        print("VERDICT: all coupling rules reproduce. The gating in vm.rs must couple audio to spice.")
        return 0
    print("VERDICT: at least one rule did NOT reproduce — do not write it into vm.rs as fact.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
