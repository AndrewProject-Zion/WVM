#!/usr/bin/env bash
# Stop every QEMU VM this user owns, cleanly, and report what happened.
#
# Written as a script because the inline version kept tripping the agent's command parser, and
# because `pgrep -f qemu` matches its own invoking shell — a trap that has cost time more than
# once. Matching on the process NAME (`comm`) instead of the command line cannot self-match.

set -uo pipefail

echo "QEMU processes before:"
found=0
while read -r pid ppid etimes; do
    [ -z "$pid" ] && continue
    found=$((found + 1))
    printf '  pid %-8s ppid %-8s up %ss\n' "$pid" "$ppid" "$etimes"
done < <(ps -eo pid,ppid,etimes,comm --no-headers | awk '$4 == "qemu-system-x86" {print $1, $2, $3}')

if [ "$found" -eq 0 ]; then
    echo "  (none)"
fi

echo
echo "Stopping:"
while read -r pid; do
    [ -z "$pid" ] && continue
    if kill "$pid" 2>/dev/null; then
        echo "  asked $pid to stop (SIGTERM)"
    else
        echo "  $pid was already gone"
    fi
done < <(ps -eo pid,comm --no-headers | awk '$2 == "qemu-system-x86" {print $1}')

# Give them a moment to exit cleanly, then escalate only if something is stuck.
sleep 3

stubborn=$(ps -eo pid,comm --no-headers | awk '$2 == "qemu-system-x86" {print $1}')
if [ -n "$stubborn" ]; then
    echo
    echo "Still running after SIGTERM, escalating to SIGKILL:"
    while read -r pid; do
        [ -z "$pid" ] && continue
        kill -9 "$pid" 2>/dev/null && echo "  killed $pid"
    done <<< "$stubborn"
    sleep 2
fi

echo
remaining=$(ps -eo comm --no-headers | awk '$1 == "qemu-system-x86"' | wc -l)
echo "QEMU processes after: $remaining"

# A stale QMP socket outlives its daemon and looks identical to a live one. Remove it so the next
# start cannot be confused by it — and so a client does not get "connection refused" from a file
# that appears to exist.
SOCK="$HOME/.local/state/wvm/w11/qmp.sock"
PIDFILE="$HOME/.local/state/wvm/w11/qemu.pid"

for f in "$SOCK" "$PIDFILE"; do
    if [ -e "$f" ]; then
        rm -f "$f" && echo "removed $f"
    fi
done

echo
echo "Done. Start the VM with:"
echo "  cd /home/andy/LSW"
echo "  python3 scripts/vm-with-display.py --display gtk --cpu max /home/andy/wvm/install.toml"
