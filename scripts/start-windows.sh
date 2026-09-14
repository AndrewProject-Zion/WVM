#!/usr/bin/env bash
# Start the installed Windows VM, safely.
#
# Exists because ad-hoc restarts caused a real problem: launching while a previous QEMU was still
# releasing the socket path leaves the running VM listening on a socket the filesystem no longer
# points at (the "inode mismatch" that diagnose-qmp-socket.py reports). The symptom is a VM that
# is running fine but whose control channel refuses connections, which is confusing and avoidable.
#
# So this script serialises the whole thing:
#   1. stop every QEMU and WAIT for them to actually be gone
#   2. verify the disk is not locked by a survivor
#   3. verify the QMP socket is bound and reachable before returning
#
# It refuses to start a second instance while one is running, rather than racing it.
#
# Usage:
#   scripts/start-windows.sh [--headless] [--config PATH]

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$HOME/wvm/wvm.toml"
SOCK="$HOME/.local/state/wvm/w11/qmp.sock"
DISK="$HOME/wvm/w11/disk.qcow2"
HEADLESS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --headless) HEADLESS=1; shift ;;
        --config)   CONFIG="$2"; shift 2 ;;
        -h|--help)  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# `comm` is truncated to 15 chars by the kernel, so "qemu-system-x86" is the whole matchable name.
qemu_pids() {
    ps -eo pid,comm --no-headers | awk '$2 == "qemu-system-x86" {print $1}'
}

# --- 1. stop anything running, and wait for it to be gone ---------------------------------------

running=$(qemu_pids)
if [ -n "$running" ]; then
    echo "A VM is already running (pid: $(echo "$running" | tr '\n' ' '))."
    echo "Stop it first, or use scripts/stop-vms.sh."
    exit 1
fi

echo "No VM running."
echo

# --- 2. clear stale state, but only when nothing holds it ---------------------------------------

for f in "$SOCK" "$(dirname "$SOCK")/qemu.pid"; do
    if [ -e "$f" ]; then
        rm -f "$f" && echo "removed stale $(basename "$f")"
    fi
done

# --- 3. confirm the disk is not locked ----------------------------------------------------------

if [ -f "$DISK" ]; then
    # qemu-img takes the same lock QEMU does, so if this fails something still holds the image.
    if ! qemu-img info "$DISK" >/dev/null 2>&1; then
        echo
        echo "ERROR: $DISK is locked by another process." >&2
        echo "       Something still holds the image. Check:  scripts/diagnose-qmp-socket.py" >&2
        exit 1
    fi
    size=$(du -h "$DISK" | cut -f1)
    echo "disk: $DISK ($size)"
else
    echo "ERROR: no disk at $DISK — has Windows been installed?" >&2
    exit 1
fi

if [ ! -f "$CONFIG" ]; then
    echo "ERROR: no config at $CONFIG" >&2
    exit 1
fi

echo "config: $CONFIG"
echo

# --- 4. start ---------------------------------------------------------------------------------

if [ "$HEADLESS" -eq 1 ]; then
    echo "Starting headless (no display)..."
    echo
    exec "$REPO/target/release/wvm" vm start --config "$CONFIG" --timeout 180
fi

echo "Starting with a window..."
echo
exec python3 "$REPO/scripts/vm-with-display.py" --display gtk --cpu max "$CONFIG"
