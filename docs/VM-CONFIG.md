# VM configuration reference

A VM is defined by a TOML file. `wvm vm` subcommands take `--config <path>` (default
`wvm.toml`), so nothing is implicit — the command line names the definition it acts on.

A starting point lives at `wvm.toml.example` in the repository root.

## Fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | *required* | Display name. Used in the journal and as QEMU's `-name`. |
| `disk` | path | *required* | The Windows disk image. Created by `vm prepare` if absent. |
| `disk_gib` | integer | `64` | Size to create the disk at, when it does not exist. |
| `memory_mib` | integer | `4096` | Guest memory. Below 1024 is refused. |
| `cpus` | integer | `4` | vCPUs. Zero is refused. |
| `install_iso` | path | none | Installer ISO. Must exist if set. |
| `driver_iso` | path | none | virtio-win driver ISO. Must exist if set. |
| `firmware` | `"uefi"` \| `"bios"` | `"uefi"` | Firmware to boot. |
| `guest_port` | integer | `48273` | Port the guest service listens on. |
| `forward_port` | integer | `48274` | Host port forwarded to `guest_port`. |
| `state_dir` | path | derived | Where PID, QMP socket and logs live. |

## The two ISOs, and why both matter

**`install_iso`** is the installer. It is bootable. Once Windows is installed, comment it out so
the VM boots from disk — leaving it attached and bootable risks re-entering the installer.

**`driver_iso`** is the virtio-win driver disc. It is **not** bootable and never will be: it is
attached read-only as a second CD-ROM and exists so Windows can see the virtio disk and network
card during installation. Windows has no in-box virtio drivers; boot the installer without this
attached and the disk you created is invisible, which presents as "no drives found" rather than as
a configuration error.

Download it from:
`https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso`

During installation, when the Windows setup asks where to install:

1. Choose **Load driver**
2. Browse to the driver CD → `viostor` → `w11` → `amd64`
3. Select the **Red Hat VirtIO SCSI controller** driver
4. The disk appears

The same driver disc covers `NetKVM` (the network card) once Windows is up.

## State directory

Default: `$XDG_STATE_HOME/wvm/<name>/`, falling back to `~/.local/state/wvm/<name>/`.

| File | Purpose |
|---|---|
| `disk.qcow2` (if `disk` is inside it) | the disk, when the default path is used |
| `qemu.pid` | the running QEMU's PID |
| `qmp.sock` | QEMU Machine Protocol socket — lifecycle control |
| `OVMF_VARS.fd` | per-instance UEFI variable store |
| `serial.log` | guest serial console |
| `qemu.log` | QEMU's own stdout/stderr |

`serial.log` is the first place to look when a VM does not boot. `wvm vm log` prints its tail.

## Why UEFI needs a writable variable store

UEFI firmware reads a code image and a variable store. The code image is shared and mounted
read-only; the variable store must be **per-instance and writable**, or settings do not persist
and multiple VMs collide. `vm prepare` copies `OVMF_VARS_4M.fd` into the state directory for this
reason. If you see a VM that boots but forgets its boot order every time, that copy is the thing
to check.

## TPM is not required

Windows 11 nominally requires TPM 2.0 and Secure Boot. **tiny11 removes both requirements**, which
is its main attraction for this use case. No `swtpm` process is needed, and the generated command
line deliberately does not include one.

If you later use a stock Windows 11 image, you will need `swtpm` and the Secure Boot OVMF variant,
and the config will need a TPM section. That is a deliberate omission, not an oversight.

## Networking

User-mode networking with an explicit loopback forward:

```
-netdev user,id=net0,hostfwd=tcp:127.0.0.1:48274-:48273
```

Consequences worth knowing:

- The guest is **not** reachable from your LAN, and cannot be reached from off-box.
- The guest can reach the internet through host NAT, but runs no inbound services.
- The control channel is `127.0.0.1:48274` on the host.

Deliberately not bridged. A Windows VM with no patch discipline is not something to put on a
network with a route to your other machines.
