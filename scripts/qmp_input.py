#!/usr/bin/env python3
"""Drive the Windows installer through QMP input events.

This is the first real exercise of what the project exists for: controlling a Windows guest as a
tool, headless, through the protocol rather than by clicking on a display.

It is a probe, not a finished feature. It exists to answer two questions:

  1. Does input injection actually reach the guest through QMP?
  2. Can it be driven reliably enough to complete an interactive install?

The honest answer to (2) may well be "not comfortably", which is worth knowing — a display during
installation and injection for automation afterwards may be the right split.

Usage:
    python3 scripts/qmp_input.py <qmp-socket> <command> [args]

Commands:
    key   <name> [name...]   send key presses (one keydown+keyup per name)
    text  <string>           type an ASCII string
    click <x> <y>            move the pointer and click left
    move  <x> <y>            move the pointer only
    shot  <path>             capture the framebuffer to a PPM
"""

import json
import os
import socket
import sys
import time


class Qmp:
    """A one-shot QMP conversation."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(20)
        self.sock.connect(path)
        self.f = self.sock.makefile("rw")
        # The greeting arrives unprompted and must be consumed first.
        self.f.readline()
        self.execute("qmp_capabilities")

    def execute(self, command, arguments=None):
        payload = {"execute": command}
        if arguments is not None:
            payload["arguments"] = arguments
        self.f.write(json.dumps(payload) + "\n")
        self.f.flush()

        # Skip asynchronous events until the reply carrying our result arrives.
        while True:
            line = self.f.readline()
            if not line:
                raise RuntimeError(f"QMP closed the connection during {command}")
            message = json.loads(line)
            if "error" in message:
                raise RuntimeError(f"QMP error for {command}: {message['error']}")
            if "return" in message:
                return message["return"]

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# --- key name mapping ---------------------------------------------------------------------------
#
# QOM key names are not the same as USB HID codes or Windows virtual-key codes. These are the
# names QEMU accepts in `send-key`. Anything absent is sent as-is, so a caller can pass a raw QOM
# name when it knows one.

SPECIAL = {
    "enter": "ret",
    "return": "ret",
    "esc": "esc",
    "escape": "esc",
    "tab": "tab",
    "space": "spc",
    "backspace": "backspace",
    "delete": "delete",
    "up": "up",
    "down": "down",
    "left": "left",
    "right": "right",
    "home": "home",
    "end": "end",
    "pgup": "pgup",
    "pgdn": "pgdn",
    "f1": "f1", "f2": "f2", "f3": "f3", "f4": "f4",
    "f5": "f5", "f6": "f6", "f7": "f7", "f8": "f8",
    "f9": "f9", "f10": "f10", "f11": "f11", "f12": "f12",
}

# Characters that need shift, mapped to (unshifted QOM name, needs shift).
SHIFTED = {
    "!": ("1", True), "@": ("2", True), "#": ("3", True), "$": ("4", True),
    "%": ("5", True), "^": ("6", True), "&": ("7", True), "*": ("8", True),
    "(": ("9", True), ")": ("0", True), "_": ("minus", True), "+": ("equal", True),
    "{": ("bracket_left", True), "}": ("bracket_right", True), "|": ("backslash", True),
    ":": ("semicolon", True), '"': ("apostrophe", True), "<": ("comma", True),
    ">": ("dot", True), "?": ("slash", True), "~": ("grave_accent", True),
}

PLAIN = {
    " ": "spc", "-": "minus", "=": "equal", "[": "bracket_left", "]": "bracket_right",
    "\\": "backslash", ";": "semicolon", "'": "apostrophe", ",": "comma",
    ".": "dot", "/": "slash", "`": "grave_accent",
}


def send_key(qmp, qom_name, shift=False, ctrl=False, alt=False):
    """Send one key press. `send-key` does the down+up for us."""
    keys = []
    if ctrl:
        keys.append({"type": "qcode", "data": "ctrl"})
    if alt:
        keys.append({"type": "qcode", "data": "alt"})
    if shift:
        keys.append({"type": "qcode", "data": "shift"})
    keys.append({"type": "qcode", "data": qom_name})
    qmp.execute("send-key", {"keys": keys, "hold-time": 60})


def key(qmp, name):
    name = name.lower()
    if name in SPECIAL:
        send_key(qmp, SPECIAL[name])
    elif len(name) == 1:
        type_char(qmp, name)
    else:
        # Pass it through: the caller may know a QOM name we do not.
        send_key(qmp, name)


def type_char(qmp, ch):
    """Type a single character, handling shift where needed."""
    if ch.isalpha():
        if ch.isupper():
            send_key(qmp, ch.lower(), shift=True)
        else:
            send_key(qmp, ch)
    elif ch.isdigit():
        send_key(qmp, ch)
    elif ch in SHIFTED:
        base, needs_shift = SHIFTED[ch]
        send_key(qmp, base, shift=needs_shift)
    elif ch in PLAIN:
        send_key(qmp, PLAIN[ch])
    elif ch == "\n":
        send_key(qmp, "ret")
    else:
        raise ValueError(f"no mapping for character {ch!r}")


def type_text(qmp, text):
    for ch in text:
        type_char(qmp, ch)
        # A small gap between keys. Not strictly necessary, but a burst of events with no delay
        # can arrive faster than the guest's input stack drains, and the symptom is dropped
        # characters rather than an error.
        time.sleep(0.02)


def move_pointer(qmp, x, y, absolute_size=(1024, 768)):
    """
    Move the pointer.

    The VM has only a *relative* PS/2 mouse, so absolute positioning is not available. Moving to
    an absolute point means computing a delta from where the pointer is believed to be and sending
    that, which requires tracking state across calls — a real limitation worth knowing about
    before building on it.

    QEMU can provide an absolute tablet instead (`-device usb-tablet`), which makes this exact.
    Adding it is a one-line change to the generated command line and is the right fix.
    """
    raise NotImplementedError(
        "absolute pointer positioning needs a USB tablet; this VM has only a relative PS/2 mouse. "
        "Add -device usb-tablet to the VM command line."
    )


def main(argv):
    if len(argv) < 3:
        print(__doc__)
        return 2

    socket_path = argv[1]
    command = argv[2]
    args = argv[3:]

    qmp = Qmp(socket_path)
    try:
        if command == "key":
            for name in args:
                key(qmp, name)
                time.sleep(0.05)
            print(f"sent {len(args)} key(s)")
            return 0

        if command == "text":
            if not args:
                print("text needs a string", file=sys.stderr)
                return 2
            type_text(qmp, " ".join(args))
            print(f"typed {len(' '.join(args))} character(s)")
            return 0

        if command in ("click", "move"):
            if len(args) != 2:
                print(f"{command} needs x and y", file=sys.stderr)
                return 2
            move_pointer(qmp, int(args[0]), int(args[1]))
            return 0

        if command == "shot":
            path = args[0] if args else "/tmp/qmp-shot.ppm"
            qmp.execute("screendump", {"filename": path})
            # The dump is written asynchronously; wait for the file to appear and settle.
            for _ in range(50):
                if os.path.exists(path) and os.path.getsize(path) > 1024:
                    break
                time.sleep(0.1)
            print(f"wrote {path} ({os.path.getsize(path)} bytes)")
            return 0

        print(f"unknown command: {command}", file=sys.stderr)
        return 2
    finally:
        qmp.close()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
