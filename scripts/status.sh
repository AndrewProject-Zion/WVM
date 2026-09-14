#!/usr/bin/env bash
# Get up to speed on WVM: what is committed, what is uncommitted, and is the VM running.
#
# A script because the equivalent one-liner keeps tripping the agent's command parser (it is the
# nested command substitution and the grep pipeline that do it, not the operations themselves).

set -uo pipefail

cd /home/andy/LSW || exit 1

echo "=== git: recent commits ==="
git log --oneline | head -8
echo

echo "=== git: working tree ==="
if [ -z "$(git status --porcelain)" ]; then
    echo "  clean — nothing uncommitted"
else
    git status --short
fi
echo

echo "=== VM: running? ==="
count=0
while read -r pid etimes; do
    [ -z "$pid" ] && continue
    count=$((count + 1))
    printf '  qemu pid %s  up %ss\n' "$pid" "$etimes"
done < <(ps -eo pid,etimes,comm --no-headers | awk '$3 == "qemu-system-x86" {print $1, $2}')

case "$count" in
    0) echo "  (not running)" ;;
    1) echo "  exactly one — correct" ;;
    *) echo "  WARNING: $count instances" ;;
esac
echo

echo "=== artefacts ==="
for f in "$HOME/wvm/w11/disk.qcow2" "$HOME/Downloads/tiny11_2311_x64-ff.iso" \
         "$HOME/wvm-images/virtio-win.iso"; do
    if [ -f "$f" ]; then
        printf '  %-52s %s\n' "$(basename "$f")" "$(du -h "$f" | cut -f1)"
    else
        printf '  %-52s MISSING\n' "$(basename "$f")"
    fi
done
echo

echo "=== build state ==="
for b in target/release/wvm target/x86_64-pc-windows-gnu/release/wvm-guest.exe; do
    if [ -f "$b" ]; then
        printf '  %-52s %s\n' "$b" "$(du -h "$b" | cut -f1)"
    else
        printf '  %-52s not built\n' "$b"
    fi
done
