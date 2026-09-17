#!/usr/bin/env python3
"""Can a display be attached to a RUNNING QEMU, or must it exist from startup?

WHY THIS DECIDES THE ARCHITECTURE

There are two ways to give a headless VM a window on request, and they have different costs:

  A. ALWAYS-ON SERVER. Start QEMU with a VNC or SPICE server bound to a unix socket. `display show`
     spawns a viewer that connects; `display hide` kills it. The display server runs the whole time,
     whether or not anyone is watching. Standard, well-trodden, and the cost is whatever an idle
     display server costs — which is a number to measure, not to assume.

  B. ATTACH ON DEMAND. Start with `-display none` as today, and add a display when asked. Zero cost
     until the request. This is only possible if QEMU exposes a command for it.

The difference matters because this project's whole pitch is a headless sandbox: if the display
machinery costs anything when nobody is looking, that is a tax on every agent run to pay for a
feature used occasionally.

So: ask QEMU. Start one instance with a QMP socket and list the commands that mention display.

Usage: python3 scripts/probe-qmp-display.py
"""
import json
import os
import socket
import subprocess
import sys
import tempfile
import time

QEMU = os.environ.get("QEMU", "qemu-system-x86_64")


def main():
    tmp = tempfile.mkdtemp(prefix="wvm-qmp-probe-")
    qmp_path = os.path.join(tmp, "qmp.sock")

    proc = subprocess.Popen(
        [QEMU, "-display", "none", "-S", "-nodefaults", "-m", "64",
         "-qmp", f"unix:{qmp_path},server=on,wait=off"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
    )

    try:
        # Wait for the socket to be bound. Checking for the FILE would be wrong: a stale socket
        # outlives its process and looks identical, so connect instead.
        sock = None
        for _ in range(50):
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.settimeout(5)
                sock.connect(qmp_path)
                break
            except OSError:
                if sock:
                    sock.close()
                sock = None
                time.sleep(0.1)

        if sock is None:
            print("  could not connect to the QMP socket — is qemu installed?")
            return 2

        f = sock.makefile("rw")
        f.readline()  # greeting

        def cmd(name):
            f.write(json.dumps({"execute": name}) + "\n")
            f.flush()
            while True:
                msg = json.loads(f.readline())
                if "return" in msg or "error" in msg:
                    return msg

        cmd("qmp_capabilities")
        reply = cmd("query-commands")
        names = [c["name"] for c in reply.get("return", [])]

        interesting = sorted(
            n for n in names
            if any(k in n.lower() for k in ("display", "vnc", "spice", "screendump", "screendump"))
        )
        print(f"  {len(names)} QMP commands in this build")
        print("  display-related:")
        for n in interesting:
            print(f"    {n}")

        # The decisive question, asked rather than inferred from a name.
        print()
        for candidate in ("display-update", "display-reload"):
            present = candidate in names
            print(f"  {candidate:16} {'present' if present else 'NOT present'}")

        print()
        print("  VERDICT:")
        can_attach = any(n.startswith("display-") for n in names)
        if can_attach:
            print("    a display-related runtime command exists — test whether it can ADD a backend")
        else:
            print("    no runtime command can add a display, so the server must exist from startup")
        sock.close()
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
