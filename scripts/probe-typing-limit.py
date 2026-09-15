#!/usr/bin/env python3
"""Measure how long a typed line can be before the guest console splits it.

Typing into the guest has failed repeatedly at unpredictable points: a 93-character command split
after `protocol=TCP`, a 50-character command split before `RunAs`. Each time the settlement was
already generous, so the delay before Enter is not the variable I assumed it was.

Rather than keep guessing at a safe length, this sends lines of increasing length and reports which
arrive intact. The result is a usable limit and, more importantly, evidence for what the limit
depends on.

The probe uses `echo` with a counter of known characters, so a split is obvious: the echoed line
will differ from what was sent, and a second command line will appear.

Usage:
    python3 scripts/probe-typing-limit.py [--socket PATH] [--max 120]
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
import time

QMP_INPUT = "/home/andy/LSW/scripts/qmp_input.py"


def load_helper(path):
    spec = importlib.util.spec_from_file_location("qmpi", path)
    if spec is None or spec.loader is None:
        raise ImportError(f"could not load the QMP helper from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def screenshot_via(qmp, path):
    """Capture the screen so the caller can read it. Returns the file path."""
    qmp.execute("screendump", {"filename": path})
    return path


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", default="/home/andy/.local/state/wvm/w11/qmp.sock")
    parser.add_argument("--max", type=int, default=120, help="longest line to try")
    parser.add_argument("--delay", type=int, default=70, help="ms between keys")
    args = parser.parse_args(argv[1:])

    m = load_helper(QMP_INPUT)
    q = m.Qmp(args.socket)

    # Clear any partial line first.
    m.chord(q, m.CTRL, "c")
    time.sleep(0.5)

    # Lengths chosen to bracket the observed failures (50 split, 38 fine).
    lengths = [20, 30, 40, 45, 50, 60, 75, 90, 110]
    lengths = [n for n in lengths if n <= args.max]

    print(f"probing line lengths up to {args.max}, {args.delay}ms between keys")
    print()

    for n in lengths:
        # Build a line of exactly n characters: `echo ` takes 5, then a marker and padding.
        # The marker encodes the length so the echo can be matched to the attempt.
        prefix = "echo "
        marker = f"L{n}"
        padding = "x" * max(0, n - len(prefix) - len(marker) - 1)
        line = f"{prefix}{marker}{' '}{padding}"
        line = line[:n]

        print(f"  {n:>4} chars: sending...")
        m.type_text(q, line, delay_ms=args.delay)
        # A generous settle beyond what any real command would need, so that the measurement
        # reflects the typing path rather than the Enter timing.
        time.sleep(3.0)
        m.press(q, "ret")
        time.sleep(1.2)

    time.sleep(1.0)
    path = "/tmp/typing-limit.ppm"
    screenshot_via(q, path)
    q.close()

    print()
    print(f"  screenshot: {path}")
    print("  Read it and compare each echoed line against what was sent. A line that")
    print("  differs, or is followed by a stray fragment, split during typing.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
