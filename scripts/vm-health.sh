#!/usr/bin/env bash
# Report the health of the WVM Windows VM: how many QEMU processes exist, whether the control
# channel answers, and whether the disk image is intact.
#
# Exists as a script because the one-liner kept tripping the command parser, and because these are
# the four checks worth running after any confusing background notification.

set -uo pipefail

echo "1. QEMU processes"
count=0
while read -r pid etimes; do
    [ -z "$pid" ] && continue
    count=$((count + 1))
    printf '   pid %s  up %ss\n' "$pid" "$etimes"
done < <(ps -eo pid,etimes,comm --no-headers | awk '$3 == "qemu-system-x86" {print $1, $2}')

if [ "$count" -eq 0 ]; then
    echo "   (none running)"
elif [ "$count" -eq 1 ]; then
    echo "   exactly one — correct"
else
    echo "   WARNING: $count instances. Only one may hold the disk; the others will have failed"
    echo "   to acquire the write lock, which is QEMU protecting the image rather than a fault."
fi
echo

echo "2. Control channel"
SOCK="$HOME/.local/state/wvm/w11/qmp.sock"
if python3 - "$SOCK" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(4)
try:
    s.connect(sys.argv[1])
    s.close()
    sys.exit(0)
except OSError:
    sys.exit(1)
PY
then
    echo "   socket accepts connections"
else
    echo "   socket refuses connections"
fi
echo

echo "3. Screen capture"
if python3 /home/andy/LSW/scripts/qmp_input.py "$SOCK" shot /tmp/wvm-health.ppm >/dev/null 2>&1; then
    size=$(stat -c %s /tmp/wvm-health.ppm 2>/dev/null || echo 0)
    echo "   captured $size bytes"
else
    echo "   capture failed (no QMP connection, or the guest is not rendering)"
fi
echo

echo "4. Disk"
DISK="$HOME/wvm/w11/disk.qcow2"
if [ -f "$DISK" ]; then
    du -h "$DISK" | awk '{print "   real size: " $1 " (64 GiB virtual)"}'

    # `qemu-img check` needs the write lock, which a running VM holds. Rather than print a raw
    # error, distinguish the two cases and say which one we are in: "cannot check while running"
    # is not a problem, and reporting it as one trains the reader to ignore this output.
    #
    # The lock error is the expected outcome for a running VM, so it is reported as such.
    if output=$(qemu-img check "$DISK" 2>&1); then
        printf '%s\n' "$output" | tail -3 | sed 's/^/   /'
    elif printf '%s' "$output" | grep -q 'Failed to get shared "write" lock'; then
        echo "   integrity check skipped: the running VM holds the write lock"
        echo "   (stop the VM and re-run to verify the image)"
    else
        printf '%s\n' "$output" | tail -3 | sed 's/^/   /'
    fi
else
    echo "   no disk at $DISK"
fi
