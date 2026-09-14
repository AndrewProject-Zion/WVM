# Verified environment — 2026-09-14

Every row below was checked by running a command on the host, not inferred from documentation.
This file exists so that "works on my machine" claims are falsifiable.

Host: Kali GNU/Linux Rolling, kernel 6.19.14+kali-amd64.

## Virtualisation

| Check | Command | Result |
|---|---|---|
| CPU virtualisation | `grep -o -m1 -E 'vmx\|svm' /proc/cpuinfo` | `svm` (AMD-V) |
| KVM module | `lsmod \| grep kvm` | `kvm_amd` loaded; `kvm` loaded |
| Nested virt | `cat /sys/module/kvm_amd/parameters/nested` | `1` |
| `/dev/kvm` exists | `ls -la /dev/kvm` | `crw-rw----+ root kvm 10,232` |
| **User access** | open for read+write | **openable — via an ACL, not group membership** |
| `kvm` group | `groups` | **andy is NOT in `kvm`** (and does not need to be) |

**Note — this is a trap worth recording.** `/dev/kvm` is mode `0660 root:kvm`, so the obvious
check (membership of the `kvm` group) reports a failure on this host. It is wrong: the trailing
`+` in the mode is an ACL granting `user:andy:rw-` directly:

```
# getfacl /dev/kvm
user::rw-
user:andy:rw-      <- this is why access works
group::rw-
mask::rw-
other::---
```

`wvm doctor` therefore tests by **actually opening the device**, not by inspecting group
membership. A permission check that reads metadata instead of attempting the operation reports
false negatives on any host using ACLs — see `docs/DECISIONS.md` D-005.


## QEMU

| Check | Result |
|---|---|
| Version | QEMU emulator 11.1.0 (Debian 1:11.1.0+ds-2) |
| Machine | `q35` available |
| `vhost-vsock-pci` device | present in `-device help` |
| `vhost-user-vsock-pci` | present |
| virtio-gpu family | `virtio-vga-gl`, `virtio-gpu-gl-pci` present |
| Display backends | `gtk`, `sdl`, `spice-app`, `dbus`, `egl-headless`, `none` |

## Host↔guest transport

| Check | Result |
|---|---|
| `/dev/vhost-vsock` | **present** (`crw-rw---- root kvm 10,241`) |
| `vhost_vsock` module | available at `/lib/modules/6.19.14+kali-amd64/kernel/drivers/vhost/vhost_vsock.ko.xz`, auto-loads on demand |
| Windows guest driver | **viosock only from virtio-win build 285**; exposes `AF_HYPERV`, needs `<vio_sockets.h>` for `AF_VSOCK` |

Conclusion recorded in `docs/DECISIONS.md` (D-002): TCP control channel is the default transport;
vsock is an opt-in backend.

## Rust toolchain

| Check | Result |
|---|---|
| cargo / rustc | 1.95.0 (2026-03-21) |
| Windows GNU target | `x86_64-pc-windows-gnu` available via rustup |
| MinGW linker | `x86_64-w64-mingw32-gcc` already installed |
| Package source | `gcc-mingw-w64-x86-64` candidate 15.2.0-12+28.3 |

Cross-compiling the guest service from Linux needs **no Windows toolchain** — the linker is
already present on this host.

## Graphics (optional path, not on the critical path)

| Check | Result |
|---|---|
| GPU | NVIDIA GA102 (RTX 3080), `10de:2206` |
| libvirglrenderer | `1.11.0` present |
| VirtIO Venus / DRM native context | Linux-guest only; **not usable by a Windows guest** |

## Host resources

| Resource | Value |
|---|---|
| RAM | 31 GiB total (24 GiB in use at time of check) |
| Free disk on `/` | 413 GiB |
| Display session | `XDG_SESSION_TYPE=tty`, `DISPLAY=:1.0` (X11) |

The display session is `tty`, so any GUI attach path must not assume a desktop session is
inherited from the systemd user environment.
