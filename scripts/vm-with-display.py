#!/usr/bin/env python3
"""Launch a WVM VM with a display, for interacting with an installer.

The normal `wvm vm start` is headless (`-display none`), which is right for automation and wrong
for installing Windows, where Setup needs a human to click through the disk-driver step.

This script takes the command line the daemon would generate and swaps the display argument. It
does not reimplement argument generation — that would be a second source of truth for the same
command line, and the two would drift. It asks the host binary, mutates one argument, runs it.

Usage:
    python3 scripts/vm-with-display.py <config.toml> [--gtk|--vnc PORT] [--cpu <model>]
"""

from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
from pathlib import Path


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def host_binary() -> Path:
    return repo_root() / "target" / "release" / "wvm"


def main(argv):
    parser = argparse.ArgumentParser(
        description="Launch a WVM VM with a display, for interacting with an installer."
    )
    parser.add_argument("config", help="VM definition (TOML)")
    parser.add_argument(
        "--display",
        default="gtk",
        choices=["gtk", "sdl", "vnc"],
        help="how to show the VM (default: gtk)",
    )
    parser.add_argument(
        "--vnc-port", type=int, default=5900, help="port for --display vnc"
    )
    parser.add_argument(
        "--cpu",
        default=None,
        help=(
            "override the CPU model. 'host' passes the host's full feature set through; "
            "'max' is a safer superset; 'qemu64' is the most conservative. Windows may stall "
            "under 'host' on some AMD parts before its CPU driver is installed."
        ),
    )
    parser.add_argument("--dry-run", action="store_true", help="print the command, do not run it")
    args = parser.parse_args(argv[1:])

    binary = host_binary()
    if not binary.exists():
        print(f"host binary not found: {binary}", file=sys.stderr)
        print("build it first:  cargo build --release -p wvm-host", file=sys.stderr)
        return 1

    config = Path(args.config).expanduser()
    if not config.exists():
        print(f"config not found: {config}", file=sys.stderr)
        return 1

    # Ask the daemon for the command line it would use.
    result = subprocess.run(
        [str(binary), "vm", "cmdline", "--config", str(config)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"could not generate a command line:\n{result.stderr}", file=sys.stderr)
        return 1

    cmdline = result.stdout.strip()
    if not cmdline.startswith("qemu-system-x86_64"):
        print(f"unexpected command line: {cmdline[:200]}", file=sys.stderr)
        return 1

    # Parse and mutate. shlex handles the single-quoted arguments the daemon emits for values
    # containing commas or spaces.
    parts = shlex.split(cmdline)

    # Replace `-display none` with the requested display.
    try:
        i = parts.index("-display")
    except ValueError:
        print("no -display argument in the generated command line", file=sys.stderr)
        return 1

    if args.display == "vnc":
        parts[i + 1] = f"vnc=127.0.0.1:{args.vnc_port - 5900}"
    else:
        parts[i + 1] = args.display

    # Optionally override the CPU model. Reported rather than silent, because it changes what the
    # guest sees and could mask or cause a stall.
    if args.cpu:
        try:
            j = parts.index("-cpu")
            previous = parts[j + 1]
            parts[j + 1] = args.cpu
            print(f"cpu: {previous} -> {args.cpu}")
        except (ValueError, IndexError):
            print("no -cpu argument to override", file=sys.stderr)

    # A serial console alongside the display: the boot log is often more informative than the
    # screen when something goes wrong early.
    if "-serial" in parts:
        k = parts.index("-serial")
        # Keep whatever the config asked for; note it so the operator knows where to look.
        print(f"serial: {parts[k + 1]}")

    print(f"display: {args.display}")
    print()
    print("command:")
    print("  " + " ".join(shlex.quote(p) for p in parts))
    print()

    if args.dry_run:
        print("--dry-run: not executing")
        return 0

    if args.display == "vnc":
        print(f"connect a VNC client to 127.0.0.1:{args.vnc_port}")
        print("(vnc is bound to loopback; forward it over ssh rather than exposing it)")
    else:
        print("a window should appear. Close it or press Ctrl-C here to stop the VM.")
    print()

    # Run in the foreground: this mode exists precisely so a human can interact, and a
    # background VM with a window nobody is watching is how an install stalls unnoticed.
    try:
        return subprocess.call(parts)
    except KeyboardInterrupt:
        print("\ninterrupted")
        return 130


if __name__ == "__main__":
    sys.exit(main(sys.argv))
