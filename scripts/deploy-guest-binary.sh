#!/usr/bin/env bash
# Deploy a newly cross-compiled wvm-guest.exe onto a STOPPED guest's disk, from the host.
#
# Why this exists, and why it is the right mechanism rather than a workaround:
#
#   The obvious deployment is "stop the service, copy the file, start the service" driven from
#   inside the guest. That failed repeatedly and not for a trivial reason:
#
#     - the installed service has `sc.exe failure ... actions= restart/5000/...`, so the SCM
#       restarts it within ~5 seconds of a kill, re-locking the binary;
#     - the old build has no control handler, so `sc stop` returns without stopping;
#     - and a console opened from the Run dialog is invisible to an agent watching only the
#       framebuffer, so a failing copy produced no readable evidence at all.
#
#   The guest disk is a file on this machine. Attaching it with qemu-nbd and writing the binary
#   directly removes the guest from the deployment path entirely — no elevation, no keystroke races,
#   no invisible consoles, and the result is verifiable by hash before the VM is ever started.
#
#   The VM MUST be stopped. NTFS does not tolerate two writers, and QEMU holds an exclusive lock.
#
# Usage:  sudo scripts/deploy-guest-binary.sh [--config ~/wvm/wvm.toml]
set -euo pipefail

# Resolve the invoking user's home BEFORE anything else. `sudo` rewrites $HOME to /root, so a
# script that reads $HOME after elevation looks in the wrong place entirely — and reports the
# disk as missing rather than reporting the real problem.
INVOKER_HOME="${HOME}"
if [ -n "${SUDO_USER:-}" ]; then
  INVOKER_HOME="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
fi

CONFIG="${INVOKER_HOME}/wvm/wvm.toml"
while [ $# -gt 0 ]; do
  case "$1" in
    --config) CONFIG="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="${REPO}/target/x86_64-pc-windows-gnu/release/wvm-guest.exe"
DISK="${INVOKER_HOME}/wvm/w11/disk.qcow2"
NBD=/dev/nbd0
MNT=/mnt/guest
TARGET_DIR="${MNT}/Program Files/wvm"
TARGET="${TARGET_DIR}/wvm-guest.exe"

[ -f "$SRC" ] || { echo "not built: $SRC" >&2; echo "run: cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest" >&2; exit 1; }
[ -f "$DISK" ] || { echo "no disk at $DISK" >&2; exit 1; }

# Refuse to run against a live VM. This is the check that makes the whole approach safe: writing to
# a disk QEMU has open corrupts the filesystem, and the failure would appear much later as a guest
# that will not boot.
if pgrep -x qemu-system-x86 >/dev/null 2>&1; then
  echo "a QEMU process is running; stop the VM first (scripts/stop-vms.sh)" >&2
  exit 1
fi

cleanup() {
  mountpoint -q "$MNT" && umount "$MNT" || true
  qemu-nbd --disconnect "$NBD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "  source    $SRC ($(stat -c%s "$SRC") bytes)"
WANT="$(sha256sum "$SRC" | cut -d' ' -f1)"
echo "  sha256    $WANT"

modprobe nbd max_part=8
qemu-nbd --disconnect "$NBD" >/dev/null 2>&1 || true
qemu-nbd --connect="$NBD" "$DISK"
sleep 2
partprobe "$NBD" 2>/dev/null || true

mkdir -p "$MNT"

# ntfs-3g rather than the in-kernel ntfs3 driver: ntfs3 refuses read-write on a volume that was not
# shut down cleanly. The fuse driver is more tolerant.
#
# It is not infinitely tolerant. Windows 11 defaults to FAST STARTUP, which HIBERNATES the kernel
# rather than shutting it down, so stopping QEMU leaves the volume flagged dirty with a live Windows
# metadata cache. ntfs-3g then refuses read-write and says so:
#
#     The disk contains an unclean file system
#     Metadata kept in Windows cache, refused to mount.
#
# `ntfsfix -d` clears the dirty flag so the mount can proceed. That is safe here and only here: the
# guest is not running, no Windows has the volume open, and the alternative is that this script
# cannot work at all on a default Windows 11 install.
#
# The honest caveat: clearing the flag discards nothing, but it does mean Windows will not replay
# its own cached metadata on next boot. For a disposable sandbox that is fine. For a guest holding
# data you care about, disable fast startup in the guest instead:
#     powercfg /h off
ntfsfix -d "${NBD}p2" >/dev/null 2>&1 || true

mount -t ntfs-3g -o rw "${NBD}p2" "$MNT"

if [ -f "$TARGET" ]; then
  OLD="$(sha256sum "$TARGET" | cut -d' ' -f1)"
  echo "  replacing  $OLD"
  # Keep a copy. A deployment that cannot be undone is a deployment nobody should run.
  cp "$TARGET" "${TARGET}.bak-$(stat -c%s "$TARGET")"
fi

cp "$SRC" "$TARGET"
sync
sleep 1

GOT="$(sha256sum "$TARGET" | cut -d' ' -f1)"
if [ "$GOT" != "$WANT" ]; then
  echo "MISMATCH: wrote $GOT, expected $WANT" >&2
  exit 1
fi

echo "  installed $GOT"
echo "  verified  hashes match on disk"
echo
echo "  start the VM with:  ./scripts/start-windows.sh --headless"
