#!/usr/bin/env bash
# Probe: can the installer and driver discs both be visible to Windows Setup without drivers?
#
# The problem this solves: Windows Setup cannot read the driver disc when the driver disc is on
# the virtio-scsi controller, because reading virtio-scsi requires the very driver that disc
# provides. A circular dependency. The disc must therefore live on a bus Windows understands
# natively — IDE/SATA — while the OS disk stays on virtio-blk for speed.
#
# Each configuration is started with a short timeout and its stderr examined. QEMU reports
# attachment conflicts immediately, so a clean start with no output means the topology is valid.

set -uo pipefail

TINY=/home/andy/Downloads/tiny11_2311_x64-ff.iso
DRV=/home/andy/wvm-images/virtio-win.iso

probe() {
    local name="$1"; shift
    # A successful start is killed by `timeout`, which writes "terminating on signal 15" to stderr.
    # That message is therefore a SUCCESS signal, not a failure — the first version of this probe
    # treated it as an error and reported every valid topology as broken. Only QEMU's own argument
    # diagnostics ("can't create", "invalid", "does not support") indicate a real problem.
    local out
    out=$(timeout 6 qemu-system-x86_64 -machine q35,accel=kvm -cpu host -m 512 \
            -display none -nodefaults -serial none "$@" 2>&1 | head -3)

    # Strip the timeout notice; anything left is QEMU's own diagnostics.
    local real
    real=$(printf '%s\n' "$out" | grep -viE 'terminating on signal|^$' || true)

    if [ -z "$real" ]; then
        printf '  %-52s OK (started, then stopped by the probe)\n' "$name"
        return 0
    fi
    printf '  %-52s REJECTED BY QEMU\n' "$name"
    printf '      %s\n' "$real"
    return 1
}

echo "Probing optical-drive topologies on q35"
echo

# Baseline: the configuration already in the tree, which Setup cannot read the second disc on.
probe "both discs on virtio-scsi (current)" \
    -device virtio-scsi-pci,id=scsi0 \
    -drive "file=$TINY,media=cdrom,readonly=on,if=none,id=cd0" \
    -device scsi-cd,drive=cd0,bus=scsi0.0 \
    -drive "file=$DRV,media=cdrom,readonly=on,if=none,id=cd1" \
    -device scsi-cd,drive=cd1,bus=scsi0.0
echo "      ^ attaches, but Windows has no virtio-scsi driver at install time, so cd1 is invisible"
echo

# Candidate: separate ports on the q35 AHCI bus.
probe "installer ide.0 + driver disc ide.1" \
    -drive "file=$TINY,media=cdrom,readonly=on,if=none,id=cd0" \
    -device ide-cd,drive=cd0,bus=ide.0 \
    -drive "file=$DRV,media=cdrom,readonly=on,if=none,id=cd1" \
    -device ide-cd,drive=cd1,bus=ide.1
echo "      ^ separate ports, not separate units. This is the difference from the earlier failure."
echo

# The earlier failure, reproduced for contrast: two devices on the SAME unit.
probe "both on the SAME ide unit (the known failure)" \
    -drive "file=$TINY,media=cdrom,readonly=on,if=none,id=cd0" \
    -device ide-cd,drive=cd0,bus=ide.0 \
    -drive "file=$DRV,media=cdrom,readonly=on,if=none,id=cd1" \
    -device ide-cd,drive=cd1,bus=ide.0
echo

# Candidate: OS disk on virtio-blk for speed, both discs on AHCI for Setup's benefit.
probe "virtio-blk OS disk + both discs on AHCI" \
    -drive file=/tmp/probe-disk.qcow2,if=none,id=disk0,format=qcow2 \
    -device virtio-blk-pci,drive=disk0,bootindex=1 \
    -drive "file=$TINY,media=cdrom,readonly=on,if=none,id=cd0" \
    -device ide-cd,drive=cd0,bus=ide.0,bootindex=2 \
    -drive "file=$DRV,media=cdrom,readonly=on,if=none,id=cd1" \
    -device ide-cd,drive=cd1,bus=ide.1
echo "      ^ the OS disk keeps virtio performance; only the discs are on AHCI."
echo "        During install Windows cannot see the virtio-blk disk until the driver loads,"
echo "        which is expected — that is what Load Driver is for."
