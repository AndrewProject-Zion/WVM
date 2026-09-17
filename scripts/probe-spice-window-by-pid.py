#!/usr/bin/env python3
"""Capture the SPICE VIEWER'S OWN window, identified by process id -- not by a title guess.

WHY THIS EXISTS

The first attempt screenshotted the whole screen and searched window titles for "spice". The search
found nothing, and a whole-screen capture cannot tell the difference between "the viewer is
rendering a Windows desktop" and "the viewer never opened and this is the user's own desktop". A
screenshot of a busy desktop proves nothing on its own -- that is the same false-positive shape as a
detector that fires on a blank screen because blank and BSOD are both uniform.

So: bind the window to the process. `remote-viewer` runs as one pid; its window is the one whose
_XNET_WM_PID matches. Then capture THAT window, and nothing else.
"""
import subprocess
import sys
import time
from pathlib import Path

SOCK = "/run/user/1000/wvm/w11.spice.sock"
OUT = Path("/tmp/spice-viewer-window.png")


def run(cmd: list[str]) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=20).stdout.strip()
    except Exception:
        return ""


def windows_for_pid(pid: int) -> list[str]:
    """Visible window ids belonging to this exact process."""
    ids = run(["xdotool", "search", "--onlyvisible", "--pid", str(pid)]).split()
    return [i for i in ids if i]


def main() -> int:
    if not Path(SOCK).exists():
        print(f"  FAIL  no socket at {SOCK}")
        return 1

    viewer = subprocess.Popen(
        ["remote-viewer", f"spice+unix://{SOCK}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        found: list[str] = []
        for _ in range(25):
            time.sleep(1)
            if viewer.poll() is not None:
                print(f"  FAIL  viewer exited early (code {viewer.returncode})")
                return 1
            found = windows_for_pid(viewer.pid)
            if found:
                break

        if not found:
            # remote-viewer is a wrapper: it may exec the real viewer under a different pid. Fall
            # back to any visible window whose title mentions viewer/spice, and SAY so.
            titled = run(["xdotool", "search", "--onlyvisible", "--name", "iewer"]).split()
            titled += run(["xdotool", "search", "--onlyvisible", "--name", "pice"]).split()
            found = sorted(set(titled))
            print(f"  (no window owned by pid {viewer.pid}; matched by title instead: {found})")

        if not found:
            print("  FAIL  the viewer ran but owns no visible window -- it connected and presented"
                  " NOTHING, which is the bug being tested for")
            return 1

        print(f"  viewer pid {viewer.pid} owns {len(found)} window(s): {found}")
        for wid in found:
            name = run(["xdotool", "getwindowname", wid])
            geo = run(["xdotool", "getwindowgeometry", wid]).replace("\n", " ")
            print(f"    {wid}  title={name!r}")
            print(f"          {geo}")

        time.sleep(6)  # let it paint a full frame
        wid = found[0]
        proc = subprocess.run(["import", "-window", wid, str(OUT)],
                              capture_output=True, text=True, timeout=60)
        if not OUT.exists() or OUT.stat().st_size == 0:
            print(f"  FAIL  capture failed: {proc.stderr.strip()[:150]}")
            return 1
        size = OUT.stat().st_size // 1024
        print(f"\n  captured the viewer's OWN window: {OUT} ({size} KiB)")

        # A uniform image is the signature of "connected but never painted". Report the spread so a
        # flat window is visible in the numbers, without pretending to recognise Windows.
        ident = run(["identify", "-format", "%wx%h mean=%[mean] stddev=%[standard-deviation]", str(OUT)])
        print(f"  image: {ident}")
        print("  (stddev near 0 = one flat colour = it never painted; a large stddev = real content)")
        return 0
    finally:
        if viewer.poll() is None:
            viewer.terminate()
            try:
                viewer.wait(timeout=10)
            except subprocess.TimeoutExpired:
                viewer.kill()
        print("  (viewer closed)")


if __name__ == "__main__":
    sys.exit(main())
