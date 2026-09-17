#!/usr/bin/env python3
"""Play audio in the guest through a live SPICE viewer, and WAIT for it to finish.

WHY THIS EXISTS

The previous probe killed its own experiment. It launched the guest's playback, captured for a
while, then called `terminate()` on the host-side exec wrapper -- and D-013 says the guest kills the
process TREE when the control channel closes (`KILL_ON_JOB_CLOSE`). So the playback died mid-loop,
never wrote its final line, and the probe then reported "GUEST DID NOT PLAY" while audio was
audibly reaching the viewer at that exact moment.

A probe must not destroy the thing it is measuring. This one waits for the guest's script to exit on
its own, and only then asks what happened.

It also LEAVES THE VIEWER OPEN, deliberately: the point is for a human at the machine to listen.
"""
import subprocess
import sys
import time
from pathlib import Path

REPO = Path("/home/andy/LSW")
SOCK = "/run/user/1000/wvm/w11.spice.sock"
CFG = str(Path.home() / "wvm/wvm.toml")
GUEST_PS1 = r"C:\ProgramData\wvm\staging\audio-listen-test.ps1"
GUEST_LOG = r"C:\ProgramData\wvm\staging\audio-listen-test.txt"

PS1 = r"""# Play a loud, repeating sound so a human at the screen has time to hear it, and record the
# outcome to a FILE. Written to a file because this runs while other things happen; a reply lost
# with its channel is indistinguishable from a hang.
$log = 'C:\ProgramData\wvm\staging\audio-listen-test.txt'
$p = 'C:\Windows\Media\Alarm01.wav'
if (-not (Test-Path $p)) { "MISSING" | Out-File -Encoding ascii $log -Append; exit 1 }
$err = ''
$played = $false
try {
    $sp = New-Object Media.SoundPlayer $p
    1..10 | ForEach-Object { $sp.PlaySync() }
    $played = $true
} catch { $err = $_.Exception.Message }
"played=$played err=$err" | Out-File -Encoding ascii $log -Append
"done" | Out-File -Encoding ascii $log -Append
"""


def guest(cmd: str, timeout: int = 200) -> str:
    r = subprocess.run(["python3", "scripts/guest-exec.py", cmd], cwd=str(REPO),
                       capture_output=True, text=True, timeout=timeout)
    return (r.stdout or "") + (r.stderr or "")


def main() -> int:
    if not Path(SOCK).exists():
        print(f"  FAIL  no socket at {SOCK} -- is the VM up with SPICE?")
        return 1

    print("=== the VM's live arguments ===")
    args = subprocess.run(["ps", "-eo", "args", "--no-headers"],
                          capture_output=True, text=True).stdout
    line = next((l for l in args.splitlines() if "qemu-system" in l and "awk" not in l), "")
    for token in ("-spice", "audiodev", "hda-output", "-display"):
        if token in line:
            print(f"  has {token}")
    print(f"  spice in args: {'-spice' in line}   audio in args: {'audiodev' in line}")

    print("\n=== push the listen script ===")
    ps1 = Path("/tmp/audio-listen-test.ps1")
    ps1.write_text(PS1)
    push = subprocess.run(
        ["./target/release/wvm", "vm", "transfer", "push", "--config", CFG,
         str(ps1), GUEST_PS1, "--overwrite"],
        cwd=str(REPO), capture_output=True, text=True, timeout=120)
    print(f"  {(push.stdout or push.stderr).strip()[:80]}")

    print("\n=== open a viewer and LEAVE IT OPEN so you can listen ===")
    viewer = subprocess.Popen(["remote-viewer", f"spice+unix://{SOCK}"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(8)
    print(f"  viewer alive: {viewer.poll() is None}")

    print("\n=== play ~10 alarms through the sound card (this is the audible part) ===")
    print("  >>> LISTEN NOW <<<")
    # WAIT for it, do not terminate it: killing the wrapper kills the guest's process tree.
    proc = subprocess.run(
        ["python3", "scripts/guest-exec.py",
         f"powershell.exe -NoProfile -ExecutionPolicy Bypass -File {GUEST_PS1}"],
        cwd=str(REPO), capture_output=True, text=True, timeout=240)
    out = (proc.stdout or "") + (proc.stderr or "")
    print(f"  guest exec returned: {out.strip().splitlines()[-1][:80] if out.strip() else '(no output)'}")

    print("\n=== did the guest finish the playback? (read AFTER it exited) ===")
    log = guest(f"type {GUEST_LOG}")
    for line in log.strip().splitlines()[-4:]:
        print(f"    {line.strip()[:100]}")

    played = "played=True" in log
    print("\n=== verdict ===")
    print(f"  guest reports playback complete : {played}")
    print(f"  viewer still open for listening : {viewer.poll() is None}")
    if played:
        print("  The guest played. If you heard it, the whole path is confirmed end to end:")
        print("  Windows -> HDA -> QEMU -> SPICE playback -> viewer -> this machine's speakers.")
    else:
        print("  The guest did NOT finish playing -- ask why before reading anything into it.")
    print("\n  (the viewer is deliberately left open; close it whenever you like)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
