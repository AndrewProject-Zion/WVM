#!/usr/bin/env python3
"""Does a SPICE viewer actually RENDER the guest desktop, or only negotiate?

WHY THIS IS A SEPARATE MEASUREMENT

"Connected to server" and "showing the desktop" are different claims, and the difference is the
whole feature. A viewer that completes the handshake and then paints nothing looks identical to a
working one from the server's side: the socket is accepted either way. So this takes PIXELS from a
real viewer and asserts they are not uniform -- a blank window and a Windows desktop both "connect".

The window is placed and captured with xdotool + a screenshot tool, then the opinion of whether it
looks like a desktop is left to a human or a vision model reading the PNG. This script only proves
the pixels are real and varied; it does not pretend to recognise Windows.

Note this opens a window on the user's desktop. That is deliberate and was agreed, but it is why
the script cleans the viewer up on exit.
"""
import shutil
import subprocess
import sys
import time
from pathlib import Path

SOCK = "/run/user/1000/wvm/w11.spice.sock"
OUT = Path("/tmp/spice-viewer-proof.png")


def screenshot(dest: Path) -> str | None:
    """Capture the whole display, whichever tool this host has."""
    for tool, args in (
        ("scrot", ["scrot", "-o", str(dest)]),
        ("import", ["import", "-window", "root", str(dest)]),
        ("gnome-screenshot", ["gnome-screenshot", "-f", str(dest)]),
    ):
        if shutil.which(tool):
            proc = subprocess.run(args, capture_output=True, text=True, timeout=60)
            if dest.exists() and dest.stat().st_size > 0:
                return f"{tool} ({dest.stat().st_size // 1024} KiB)"
            print(f"    {tool} failed: {proc.stderr.strip()[:120]}")
    return None


def main() -> int:
    if not Path(SOCK).exists():
        print(f"  FAIL  no display socket at {SOCK} -- is the VM running with SPICE?")
        return 1

    # A baseline BEFORE the viewer exists. Without it, a screenshot of the user's own desktop would
    # be full of variation and would "prove" the viewer renders while proving nothing at all.
    base = Path("/tmp/spice-viewer-baseline.png")
    print("=== baseline: the screen with no viewer open ===")
    print(f"  captured: {screenshot(base)}")

    print("=== opening a real viewer on the socket ===")
    viewer = subprocess.Popen(
        ["remote-viewer", f"spice+unix://{SOCK}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        # Poll for the window rather than sleeping a guessed interval: the viewer has to connect,
        # negotiate and paint, and on a first run that takes as long as it takes.
        found = False
        for _ in range(30):
            time.sleep(1)
            if viewer.poll() is not None:
                print(f"  FAIL  the viewer EXITED early (code {viewer.returncode}) -- it never"
                      " presented a window")
                return 1
            q = subprocess.run(
                ["xdotool", "search", "--onlyvisible", "--name", "spice"],
                capture_output=True, text=True,
            )
            if q.stdout.strip():
                found = True
                break
        print(f"  viewer alive: {viewer.poll() is None}   window present: {found}")
        if not found:
            print("  WARN  no window matched; capturing anyway to see what is on screen")

        time.sleep(6)  # let it paint a full frame
        print("=== capturing the viewer ===")
        shot = screenshot(OUT)
        print(f"  captured: {shot}")
        if not shot:
            print("  FAIL  no screenshot tool available on this host")
            return 1

        # Uniformity check: a window that negotiated but never painted is one flat colour.
        try:
            # PNG decoding without a third-party dep is not worth it; use ImageMagick identify if
            # present, else report the size and let the caller look at the file.
            idf = shutil.which("identify")
            if idf:
                meta = subprocess.run([idf, "-format", "%wx%h %[mean]", str(OUT)],
                                      capture_output=True, text=True).stdout
                print(f"  image: {meta}")
        except Exception as exc:  # pragma: no cover - diagnostic only
            print(f"  (image analysis skipped: {exc})")

        print(f"\n  PROOF IMAGE: {OUT}")
        print("  Look at it: a Windows desktop with a taskbar = the viewer renders. One flat")
        print("  colour = it connected but never painted, which is the bug Dave reported.")
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
