#!/usr/bin/env python3
"""Drive the guest through QMP to restore the service after the deadline test.

WHY THIS EXISTS

Testing the request deadline required running the guest with a short deadline, which meant stopping
the Windows service that normally serves the control channel. The service has `sc failure ...
restart/3600000` configured by this test, so it does not come back on its own — deliberately, so the
console instance could own the port instead.

That console instance never started, which left the guest with no control channel at all. This script
is the way back: QMP input events reach the guest's interactive session regardless of what is running
inside it, so it works precisely when the channel does not.

It restores:
  - the original restart policy (restart/5000/restart/10000/restart/30000)
  - the service itself, started

Kept as a script rather than a one-off because "the channel is down, get it back" is a recurring
situation in a project whose whole purpose is driving a guest, and the recovery path should not have
to be reinvented each time.

Usage:  python3 scripts/restore-guest-service.py [--socket PATH]
"""
import argparse
import json
import socket
import sys
import time


def connect(path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(30)
    s.connect(path)
    f = s.makefile("rw")
    f.readline()  # greeting
    f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n")
    f.flush()
    f.readline()
    return s, f


def cmd(f, execute, **arguments):
    f.write(json.dumps({"execute": execute, "arguments": arguments}) + "\n")
    f.flush()
    f.readline()


def type_text(f, text, gap=0.06):
    """Type a string as discrete key events.

    `input-send-event` with explicit down/up rather than `send-key`, because `send-key` holds every
    key for its hold-time and the guest driver then REORDERS characters once lines get long (D-011).
    """
    for ch in text:
        keymap = {
            " ": "spc", "-": "minus", "/": "slash", "\\": "backslash", ":": "shift-semicolon",
            "=": "equal", ".": "dot", ",": "comma", "_": "shift-minus", '"': "shift-apostrophe",
            "(": "shift-9", ")": "shift-0",
        }
        if ch.isalnum():
            name = ch
            shift = ch.isupper()
        elif ch in keymap:
            name = keymap[ch]
            shift = name.startswith("shift-")
            if shift:
                name = name.split("-", 1)[1]
        else:
            continue

        events = []
        if shift:
            events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "shift"}}})
        events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": name}}})
        events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": name}}})
        if shift:
            events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "shift"}}})
        cmd(f, "input-send-event", events=events)
        time.sleep(gap)


def press(f, qcode, shift=False):
    events = []
    if shift:
        events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "shift"}}})
    events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": qcode}}})
    events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": qcode}}})
    if shift:
        events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "shift"}}})
    cmd(f, "input-send-event", events=events)


def chord(f, modifier, qcode):
    """A held modifier plus a key — `send-key` releases everything it presses, so Meta+R arrives as
    Meta-up then 'r' and opens Start instead of Run (D-011)."""
    cmd(f, "input-send-event", events=[
        {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": modifier}}},
        {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": qcode}}},
        {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": qcode}}},
        {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": modifier}}},
    ])


def run_dialog(f, command, settle=1.5):
    """Meta+R, clear the field, type, Enter.

    The Run field remembers its previous contents, so it is cleared first — a stale command from an
    earlier session has been typed into the guest before.
    """
    chord(f, "meta_l", "r")
    time.sleep(settle)
    # Select-all then delete, in case the box holds a previous command.
    chord(f, "ctrl", "a")
    time.sleep(0.2)
    press(f, "backspace")
    time.sleep(0.2)
    type_text(f, command)
    time.sleep(0.5)
    press(f, "ret")
    time.sleep(settle)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default="/home/andy/.local/state/wvm/w11/qmp.sock")
    args = ap.parse_args()

    s, f = connect(args.socket)
    print(f"connected to {args.socket}")

    steps = [
        ("restore the original restart policy",
         r"cmd /c sc failure wvm-guest reset= 0 actions= restart/5000/restart/10000/restart/30000"),
        ("start the service",
         r"cmd /c sc start wvm-guest"),
    ]
    for label, command in steps:
        print(f"  {label}")
        run_dialog(f, command)

    s.close()

    # The service takes a few seconds to come up and bind.
    print("waiting for the channel to answer...")
    deadline = time.time() + 60
    import struct
    while time.time() < deadline:
        time.sleep(3)
        try:
            c = socket.create_connection(("127.0.0.1", 48274), timeout=5)
            c.settimeout(10)
            b = json.dumps({"op": "hello", "protocol_version": 1, "client": "restore"}).encode()
            c.sendall(struct.pack(">I", len(b)) + b)
            hdr = b""
            while len(hdr) < 4:
                part = c.recv(4 - len(hdr))
                if not part:
                    break
                hdr += part
            if len(hdr) == 4:
                (n,) = struct.unpack(">I", hdr)
                body = b""
                while len(body) < n:
                    part = c.recv(n - len(body))
                    if not part:
                        break
                    body += part
                print("  channel is back:", json.loads(body).get("status"))
                c.close()
                return 0
            c.close()
        except Exception:
            pass
    print("  the channel did not come back within 60s")
    return 1


if __name__ == "__main__":
    sys.exit(main())
