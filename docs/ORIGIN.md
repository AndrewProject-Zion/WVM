# Where WVM came from, and what it is not

This project did not appear from nowhere. It started from a review of an existing tool, and the most
useful thing that review produced was a list of things **not** to do. That reasoning is in
`DECISIONS.md`; this document records the attribution, because the original code is no longer in the
tree and the debt should still be visible.

## The starting point

**[ne0YT / Linux-Subsystem-for-Windows](https://github.com/ne0YT/Linux-Subsystem-for-Windows)** —
"seamless Windows apps on Linux". Roughly 420 lines of shell whose central mechanism is:

```sh
VBoxManage guestcontrol run --username admin --password "$(cat vm_password.txt)"
```

plus mounting the host's `/` into the guest as a writable `Z:\`, and using VirtualBox's own seamless
mode to put individual Windows windows on the Linux desktop.

It is a genuinely neat trick and it works. It is also a **desktop-integration** tool, and its
famous feature — seamless windows — is VirtualBox's, not the project's.

## Why this is not a port of it

Two things made a port the wrong move, and both are recorded as decisions:

**A plaintext password file driving every guest command is indefensible.**
`echo "YOUR_PASSWORD" > vm_password.txt` then `$(cat vm_password.txt)` on the command line puts the
credential in the process table and leaves it on disk in the clear. There is no version of that
which belongs in a tool an agent runs unattended.

**Mounting the host's `/` into the guest as `Z:\` is a live grenade.** One `rm -rf` inside the guest
— by accident, by a compromised Windows program, or by a model that misreads its own command —
reaches the host's real filesystem. WVM has no shared filesystem at all: bytes move over an explicit
protocol into a **staging directory**, and the guest is the thing that enforces where they land.

There is also a scope difference that turned out to matter more than either. The original is for a
person clicking a file and choosing "open with Windows". WVM is for **a program driving a Windows
guest as a typed tool** — headless, with every operation passing a capability boundary and landing in
an append-only journal. Those are different products that happen to share a hypervisor.

## What was taken

Concepts, not code:

- **Tiny11 as the guest image.** The original's observation that a stripped Windows is the right
  base for a disposable sandbox is correct and is why this project uses it.
- **QEMU/KVM over VirtualBox.** A deliberate departure, but the same underlying instinct: use the
  hypervisor that is already on the machine rather than shipping a second one.
- **Snapshots as a safety net**, so a guest can be returned to a known state cheaply.

No source file was copied. The original's code was removed from the tree before the repository went
public; this document is what remains of it, and `DECISIONS.md` carries the reasoning.

## Credit

The original project is MIT-licensed and the authors' work is what made the problem visible: it
demonstrated that a Windows guest on a Linux host is genuinely useful, and by doing so it made the
gap obvious — nothing in that space gives an **agent** a bounded, auditable way to drive a guest.

That gap is the entire reason this exists.

**ne0YT/Linux-Subsystem-for-Windows:** https://github.com/ne0YT/Linux-Subsystem-for-Windows

If you want seamless Windows windows on your Linux desktop, that project — or WinBoat, or WinPodX —
is what you want. WVM is not trying to be those.
