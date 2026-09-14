# Installing Windows into a WVM guest

The first real installation on this machine, written down as it happens so the next one is a
runbook rather than a rediscovery.

## Inputs

| Item | Path | Verified how |
|---|---|---|
| tiny11 2311 x64 | `/home/andy/Downloads/tiny11_2311_x64-ff.iso` | mounted read-only; label `TINY11_2311`, `install.esd` 2724 MiB with `MSWIM` magic, `boot.wim` 639 MiB, EFI boot files present |
| virtio-win drivers | `/home/andy/wvm-images/virtio-win.iso` | fetched in chunks; size checked against `Content-Length` |
| VM config | `/home/andy/wvm/install.toml` | `wvm vm validate` |

## Why the driver ISO is not optional

Windows ships no virtio driver. The VM is given a **virtio-blk** disk because it is far faster
than emulated IDE — but Windows Setup cannot see a virtio disk without a driver, and reports:

```
No drives were found. Click Load Driver to provide a mass storage driver.
```

That message looks like a fault and is the expected state before step 4 below.

## Procedure

```sh
# 1. Create the disk and the per-instance UEFI variable store.
wvm vm prepare --config ~/wvm/install.toml

# 2. Read the command line before running it. This is the whole point of `cmdline`.
wvm vm cmdline --config ~/wvm/install.toml

# 3. Boot. This waits until QEMU actually answers, rather than returning when the process spawns.
wvm vm start --config ~/wvm/install.toml --timeout 300

# 4. Watch the serial console. It exists whether or not a display is attached.
wvm vm log --config ~/wvm/install.toml --lines 60
```

### In Windows Setup

The guest is headless — `-display none` — so Setup is driven by its answer file defaults only as
far as the first prompt. Where it cannot proceed unattended:

1. Setup reaches **"Where do you want to install Windows?"** and reports **no drives**.
2. Choose **Load driver**.
3. Browse to the driver CD (the second optical drive) → `viostor` → `w11` → `amd64`.
4. Select **Red Hat VirtIO SCSI controller**. The 64 GiB disk appears.
5. Select it and continue.

Step 3's path is not obvious from a headless guest. If a display is needed to click through this,
attach one temporarily:

```sh
# A one-off run with a window, rather than editing the config.
qemu-system-x86_64 ... -display gtk
```

`wvm vm cmdline` prints the full argument list, so a single `-display none` can be substituted for
`-display gtk` and the rest reused verbatim. That is what `cmdline` is for.

## After installation

```sh
# Boot from disk with no installer attached.
wvm vm start --config ~/wvm/wvm.toml
```

Then install the guest service (M4), from an elevated PowerShell inside Windows:

```powershell
# Copy wvm-guest.exe in, then:
.\install-guest-service.ps1 -Port 48273
Start-Service wvm-guest
```

## Verifying the guest control channel

With the service running, from the host:

```sh
# The forward is loopback-only by design.
wvm call --socket /tmp/wvm.sock inspect
```

A refusal here is informative rather than mysterious:

| Symptom | Meaning |
|---|---|
| connection refused | the service is not running, or `guest_port` does not match |
| firewall drop | the rule is scoped to `LocalSubnet`; check the guest is on its own subnet |
| protocol mismatch | host and guest were built with different `PROTOCOL_VERSION` |

## Notes

**Suspend writes into the image.** `snapshot-save` puts VM state inside the qcow2, so a suspended
VM is self-contained — but it also means the disk grows by the guest's RAM size when suspended.
With 8 GiB of guest memory, expect the image to grow by roughly that much.

**Shutdown needs an installed OS.** `system_powerdown` is ACPI; a guest with no OS ignores it and
the command times out and says so. Before Windows is installed, stop the VM with `kill` on the
QEMU PID instead. This is expected, not a fault.
