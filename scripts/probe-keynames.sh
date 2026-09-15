#!/usr/bin/env bash
# Discover which key names this QEMU accepts, by asking it.
#
# `send-key` validates its argument and returns a clear error for an unknown name:
#   Parameter 'data' does not accept value 'ctrl_l'
# So validity is directly testable without reading the QAPI schema — which turned out to have a
# shape I could not reliably parse, and probing is both simpler and more authoritative anyway.
#
# Usage: scripts/probe-keynames.sh <qmp-socket>

set -uo pipefail

SOCK="${1:-$HOME/.local/state/wvm/w11/qmp.sock}"

python3 - "$SOCK" <<'PY'
import json
import socket
import sys

sock_path = sys.argv[1]

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(15)
s.connect(sock_path)
f = s.makefile("rw")
f.readline()
f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n")
f.flush()
f.readline()


def try_key(name):
    """Return True if QEMU accepts this key name."""
    f.write(json.dumps({
        "execute": "send-key",
        "arguments": {"keys": [{"type": "qcode", "data": name}], "hold-time": 1},
    }) + "\n")
    f.flush()
    resp = f.readline()
    return '"error"' not in resp


# The names a driver actually needs. Tested rather than assumed.
candidates = [
    # modifiers -- what to actually call them
    "ctrl", "ctrl_r", "shift", "shift_r", "alt", "alt_r", "meta_l", "meta_r",
    # rejected names, to confirm the mechanism reports failures and is not accepting everything
    "ctrl_l", "enter", "control",
    # essentials
    "ret", "esc", "tab", "spc", "backspace", "delete",
    "up", "down", "left", "right", "home", "end", "pgup", "pgdn",
    # letters and digits
    "a", "z", "0", "9",
    # punctuation likely needed for a command line
    "minus", "equal", "slash", "backslash", "dot", "comma", "semicolon", "apostrophe",
    "bracket_left", "bracket_right", "grave_accent",
]

accepted, rejected = [], []
for name in candidates:
    (accepted if try_key(name) else rejected).append(name)

print(f"accepted ({len(accepted)}):")
for i in range(0, len(accepted), 6):
    print("   ", "  ".join(f"{n:<14}" for n in accepted[i:i + 6]))
print()
print(f"rejected ({len(rejected)}): {' '.join(rejected) or 'none'}")
print()

# The sanity check that matters: if the rejected list is empty, this probe is not testing anything
# and would have "confirmed" a wrong name.
if not rejected:
    print("WARNING: nothing was rejected, so this probe cannot distinguish valid names from")
    print("         invalid ones. Treat the accepted list as unverified.")
    sys.exit(1)
print("The probe rejects invalid names, so the accepted list is meaningful.")

s.close()
PY
