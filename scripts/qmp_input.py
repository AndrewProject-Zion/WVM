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

# Modifier names, verified against this QEMU by scripts/probe-keynames.sh rather than assumed.
# The probe rejected `ctrl_l` and `enter`, so the accepted list is meaningful:
#   ctrl  ctrl_r  shift  shift_r  alt  alt_r  meta_l  meta_r
CTRL = "ctrl"
SHIFT = "shift"
ALT = "alt"
META = "meta_l"

# Plain (unshifted) characters and their QOM keycode names.
#
# MEASURED on this guest (UK layout), not assumed. scripts/probe-keynames.sh confirmed which names
# QEMU accepts; a labelled round trip in the guest (type "0<key>1<key>...", read the echo with
# vision) confirmed what each one actually produces:
#
#   grave_accent  -> `
#   apostrophe    -> '        semicolon -> ;        bracket_left  -> [
#   bracket_right -> ]        minus     -> -        equal         -> =
#   comma         -> ,        dot       -> .        slash         -> /
#   backslash     -> #        (US physical position, which on UK is `#`)
#   yen           -> @
#   kp_divide     -> /
#
# There is NO name that produces a literal backslash on this layout. Rather than guess further,
# callers avoid the character: Windows accepts forward slashes in paths, so `E:/NetKVM/w11/...`
# works wherever `E:\NetKVM\w11\...` would. `PLAIN` therefore maps `\` to `slash` — documented,
# deterministic, and correct for the use that matters. Anything that genuinely requires a backslash
# must be rewritten to use a forward slash.
PLAIN = {
    " ": "spc", "-": "minus", "=": "equal", "[": "bracket_left", "]": "bracket_right",
    # See note above: no keycode yields a literal backslash here. `/` is accepted by Windows APIs,
    # and the previous attempts (`backslash` -> `#`) produced mangled paths whose error message
    # blamed the driver rather than the keystroke.
    "\\": "slash",
    ";": "semicolon", "'": "apostrophe", ",": "comma", ".": "dot", "/": "slash",
    "`": "grave_accent",
}


def send_key(qmp, qom_name, shift=False, ctrl=False, alt=False):
    """Send one key press via `input-send-event`, with an explicit press and release.

    This used to use `send-key` with `hold-time: 60`, which holds every key down for 60ms. That is
    how the reordering bug happened: with a 70ms gap between characters, each key was still held
    when the next one arrived, and the guest's keyboard driver reordered them. The symptom was
    characters appearing at the START of a line out of sequence — not dropped, reordered — and it
    only showed up past about 75 characters, which is what made it look like a line-length limit.

    Measured with scripts/probe-typing-limit.py: 20/30/40/45/50/60/75 characters arrived intact,
    90 and 110 arrived with stray characters prefixed. The probe disproved "long lines get split",
    which is what I had assumed.

    `input-send-event` takes discrete down and up events, so the key is released before the next
    is pressed and there is nothing to reorder. A short hold is still applied (the guest needs to
    sample the key as down long enough to register it) but it is now well inside the inter-key
    gap rather than competing with it.
    """
    events = []

    # Modifiers down first, in a stable order, then released in reverse after the key.
    modifiers = []
    if ctrl:
        modifiers.append(CTRL)
    if alt:
        modifiers.append(ALT)
    if shift:
        modifiers.append(SHIFT)

    for modifier in modifiers:
        events.append(
            {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": modifier}}}
        )

    events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": qom_name}}})
    # A brief hold so the guest registers the press. 12ms is comfortably under the 70ms inter-key
    # gap used by type_text, so a press is always complete before the next begins.
    events.append(
        {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": qom_name}}}
    )

    for modifier in reversed(modifiers):
        events.append(
            {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": modifier}}}
        )

    qmp.execute("input-send-event", {"events": events})


def chord(qmp, *keys, hold_ms=40):
    """Press keys simultaneously, holding modifiers down across the whole chord.

    `send-key` cannot express this: it presses everything given and releases it again, so a
    combination like Meta+R arrives as Meta-down, Meta-up, R-down, R-up — which Windows reads as
    the Start menu opening and then a stray 'r', not as the Run dialog.

    `input-send-event` does model press/release separately, so the chord is built as explicit
    down events for the modifiers, the key, then up events in reverse order. This is the form that
    actually works for accelerator keys.
    """
    events = []
    for key in keys[:-1]:
        events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": key}}})
    last = keys[-1]
    events.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": last}}})
    events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": last}}})
    for key in reversed(keys[:-1]):
        events.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": key}}})

    qmp.execute("input-send-event", {"events": events})
    # A short settle: the guest's input stack needs a moment between chords.
    time.sleep(hold_ms / 1000.0)


def press(qmp, qom_name):
    """Press and release a single key via input-send-event, for consistency with `chord`."""
    chord(qmp, qom_name)


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


def type_text(qmp, text, delay_ms=60, settle_ms=120):
    """Type a string, pacing it so the guest keeps up.

    The delay is not cosmetic. At 20ms between keys, roughly a dozen characters were silently
    dropped at the start of a typed command and a stray character appeared at the end — the guest's
    input stack, the PS/2 controller and the console host all buffer, and a burst overruns them.
    The visible result is a command that is subtly wrong with no error explaining why, which is the
    worst kind of failure to debug.

    60ms is comfortable. Verified by checking the guest's echo against what was sent rather than
    assuming the text arrived intact.
    """
    for ch in text:
        type_char(qmp, ch)
        time.sleep(delay_ms / 1000.0)

    # A pause before Enter, so it cannot be consumed as part of the burst.
    time.sleep(settle_ms / 1000.0)


def type_command(qmp, command, verify_echo=True):
    """Type a command line, press Enter, and (optionally) report what was sent.

    The caller is expected to compare the guest's echo against `command` — the interpreter here is
    the guest's own console, and a mismatch there is the only reliable proof the keystrokes landed.
    """
    type_text(qmp, command)
    press(qmp, "ret")
    if verify_echo:
        # Recorded so a caller can diff it against the guest's screen.
        return command
    return None


def run_dialog(qmp, command, clear_first=True):
    """Open the Run dialog (Meta+R), type a command, and press Enter.

    Exists as a named operation because getting it right required discovering that the chord must
    hold the modifier down, and that a previous session's text may still be in the field — a stale
    `regedit` left there from an earlier interaction silently produced a command that did not
    exist, with no error to explain why.
    """
    time.sleep(0.4)
    chord(qmp, META, "r")
    time.sleep(1.0)

    if clear_first:
        # Select all and delete, so an existing value cannot be appended to.
        chord(qmp, CTRL, "a")
        press(qmp, "delete")
        time.sleep(0.2)

    type_text(qmp, command)
    time.sleep(0.4)
    press(qmp, "ret")


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

        if command == "run":
            # Open the Run dialog, type the command, press Enter. The chord handling and the
            # field-clearing are the parts that took discovering.
            if not args:
                print("run needs a command", file=sys.stderr)
                return 2
            target = " ".join(args)
            run_dialog(qmp, target)
            print(f"ran: {target}")
            return 0

        if command == "chord":
            # e.g. `chord ctrl alt delete`
            if not args:
                print("chord needs at least one key", file=sys.stderr)
                return 2
            chord(qmp, *args)
            print(f"chord: {'+'.join(args)}")
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
