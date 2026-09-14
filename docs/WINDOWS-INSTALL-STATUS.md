# Windows install status

What is installed, how it was verified, and how to run it. Kept current because the state of the
guest is difficult to infer from the outside — an installed VM and a broken one look identical
from the host.

Last updated: 2026-09-14.

## What is installed

| | |
|---|---|
| Guest OS | Windows 11, **tiny11 2311 x64** (NTLite-built, debloated) |
| Installed to | `~/wvm/w11/disk.qcow2` — 8.9 GiB real, 64 GiB virtual |
| Firmware | **BIOS (SeaBIOS)** |
| Disk interface | `virtio-blk` (Windows needs the driver from the driver disc) |
| Optical | installer + driver disc on separate AHCI ports during install |
| User | `zion-win` |
| Windows Terminal | present (useful: it is how the guest service gets installed) |

## How the image was verified

Not by trusting a filename. The ISO was mounted read-only and inspected:

```
volume label   TINY11_2311
install.esd    2724.3 MiB, magic 'MSWIM'   (the Windows image itself)
boot.wim        639.1 MiB
EFI files      efi/boot/bootx64.efi, boot/bootmgr.efi, bcd, boot.sdi
El Torito      boot record present
```

The driver ISO was verified the same way: `viostor/w11/amd64/viostor.inf` is present, which is the
exact path Windows Setup needs.

## Two facts worth knowing before touching this

### 1. The image is BIOS-only, and the install must stay BIOS

Parsing the El Torito boot catalog gives:

```
VALIDATION ENTRY
  platform:  0x00 (x86 BIOS)
SECTION ENTRIES
  BOOTABLE  media=0x00  load_seg=0x0000  sys_type=0x08  sectors=1443
  [terminator]
```

There is **no EFI boot entry**. That is why the first attempt failed: the config said `firmware =
"uefi"`, OVMF found the disc, tried to start it, timed out, and dropped to its boot menu — with no
error message, because an unbootable disc is a normal condition to firmware.

**Boot the installed guest under BIOS.** Booting a BIOS-installed Windows under UEFI finds no EFI
System Partition and produces the same silent boot menu. `wvm vm prepare` now detects this and
refuses rather than letting you discover it from a blank screen.

### 2. The disk is on virtio-blk, so it is invisible until the driver loads

Setup cannot see the OS disk until `viostor` is loaded from the driver disc. That is the expected
sequence, not a fault:

1. Setup reports **"No device drivers were found"** — expected
2. **Load driver** → the driver CD (`E:`) → `viostor` → `w11` → `amd64`
3. *Red Hat VirtIO SCSI controller* — the disk then appears
4. Select it and continue

## Running it

```sh
cd /home/andy/LSW

./scripts/start-windows.sh              # with a window (gtk)
./scripts/start-windows.sh --headless   # no display
./scripts/stop-vms.sh                   # stop everything, cleanly
./scripts/vm-health.sh                  # is anything actually wrong?
```

`start-windows.sh` **refuses to start a second instance** while one is running. That guard exists
because overlapping starts caused a real problem: a VM left listening on a QMP socket the
filesystem no longer pointed at, which is confusing to diagnose and was entirely avoidable.

### Configuration

Two configs, deliberately separate:

| File | Purpose |
|---|---|
| `~/wvm/install.toml` | boots the installer; ISO attached. Not used now the install is done |
| `~/wvm/wvm.toml` | **the one to use** — boots from disk, no installer attached |

Both must agree on `firmware = "bios"`.

## Verifying the guest from the host

The guest is headless by default. To see what it is doing without a display:

```sh
SOCK=~/.local/state/wvm/w11/qmp.sock
python3 scripts/qmp_input.py $SOCK shot /tmp/screen.ppm
python3 -c "from PIL import Image; Image.open('/tmp/screen.ppm').save('/tmp/screen.png')"
```

`qmp_input.py` also drives the guest by keyboard — `key`, `text`, `shot`. **Input injection is
verified working**: the entire install was driven through it (licence acceptance, driver
selection, install), with no mouse. The VM exposes only a relative PS/2 pointer, so `click` is
deliberately unimplemented; absolute positioning needs `-device usb-tablet`.

## Known quirks

**A failed start reporting a disk lock is not a fault.**

```
Failed to get "write" lock
Is another process using the image [.../disk.qcow2]?
```

That is QEMU preventing two writers. The disk is intact *because* that check fired.

**A stale QMP socket can accept nothing while appearing present.** If `qmp_input.py` reports
`Connection refused` but the socket file exists, run `scripts/diagnose-qmp-socket.py`. It compares
the file's inode against the listening socket's, which is the only way to tell a stale path from a
live one.

## What is installed inside Windows

Nothing of ours yet. M4 installs `wvm-guest.exe` as a service; the installer script exists at
`scripts/install-guest-service.ps1` and has been parse-checked but not yet run against this guest.
