#!/usr/bin/env python3
"""Does the guest's audio reach a SPICE viewer? One decisive test, with every instrument proven.

WHAT WENT WRONG BEFORE, SO IT DOES NOT AGAIN

Three separate instruments lied in a row on this question:
  1. A capture whose parser returned 0.0 for any sample width but 16-bit -- a 32-bit recording read
     as PERFECT SILENCE.
  2. A "known non-silent signal" chosen by file size and never checked for content.
  3. A PowerShell command passed inline through the exec layer, which ECHOED THE COMMAND BACK
     instead of running it -- output that looks like evidence and is not.

So this test: forces the capture format, generates its own tone to prove the capture path, pushes a
.ps1 and runs it with -File (never inline), and has the guest write its result to a FILE so a
dropped channel cannot turn "played" into "unknown". It also reports whether the guest actually
played, because "the guest was silent" and "the guest never played" are different findings.
"""
import array
import math
import shutil
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

REPO = Path("/home/andy/LSW")
SOCK = "/run/user/1000/wvm/w11.spice.sock"
GUEST_PS1 = r"C:\ProgramData\wvm\staging\audio-play-test.ps1"
GUEST_LOG = r"C:\ProgramData\wvm\staging\audio-play-test.txt"

PS1 = r"""# Play a sound through the audio device repeatedly, and record what happened to a FILE.
# A file, because the viewer's audio and this script run concurrently and a dropped control channel
# must not be able to turn a completed playback into an unknown.
$log = 'C:\ProgramData\wvm\staging\audio-play-test.txt'
"start  $(Get-Date -Format o)" | Out-File -Encoding ascii $log
$p = 'C:\Windows\Media\Alarm01.wav'
if (-not (Test-Path $p)) {
    "MISSING $p" | Out-File -Encoding ascii $log -Append
    exit 1
}
"found  $p ($((Get-Item $p).Length) bytes)" | Out-File -Encoding ascii $log -Append
$err = ''
$played = $false
try {
    $sp = New-Object Media.SoundPlayer $p
    1..8 | ForEach-Object { $sp.PlaySync() }
    $played = $true
} catch {
    $err = $_.Exception.Message
}
"played=$played err=$err" | Out-File -Encoding ascii $log -Append
"end    $(Get-Date -Format o)" | Out-File -Encoding ascii $log -Append
"""


def write_tone(path: Path, seconds: float = 3.0, rate: int = 48000, freq: int = 1000) -> None:
    frames = int(seconds * rate)
    amp = int(0.9 * 32767)
    samples = array.array("h", (int(amp * math.sin(2 * math.pi * freq * i / rate)) for i in range(frames)))
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(rate)
        s = array.array("h")
        for v in samples:
            s.append(v)
            s.append(v)
        w.writeframes(s.tobytes())


def peak_of(path: Path) -> tuple[float, int, int]:
    with wave.open(str(path), "rb") as w:
        width, frames, rate = w.getsampwidth(), w.getnframes(), w.getframerate()
        raw = w.readframes(frames)
    if not raw:
        return 0.0, frames, rate
    if width != 2:
        raise ValueError(f"expected 16-bit, got {width * 8}-bit -- do NOT read as silence")
    s = array.array("h")
    s.frombytes(raw)
    return (max(abs(v) for v in s) / 32768.0 if s else 0.0), frames, rate


def start_capture(dest: Path):
    """Begin recording and return the process. Caller decides when to stop.

    Explicit start/stop rather than a sleep-based window, because parec needs a second or two to
    actually begin. A capture window that opens before the recorder is ready records SILENCE, which
    is indistinguishable from a guest that produced no sound -- this probe reported exactly that
    false result twice before the ordering was fixed.
    """
    return subprocess.Popen(
        ["parec", "--file-format=wav", "--format=s16le", "--rate=48000", "--channels=2",
         "--device=@DEFAULT_MONITOR@", str(dest)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def stop_capture(proc) -> bool:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    dest = Path(proc.args[-1])
    return dest.exists() and dest.stat().st_size > 44


def guest(cmd: str, timeout: int = 90) -> str:
    r = subprocess.run(["python3", "scripts/guest-exec.py", cmd], cwd=str(REPO),
                       capture_output=True, text=True, timeout=timeout)
    return (r.stdout or "") + (r.stderr or "")


def main() -> int:
    if not Path(SOCK).exists():
        print(f"  FAIL  no display socket at {SOCK}")
        return 1
    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)
        tone, back = tdp / "tone.wav", tdp / "guest-audio.wav"

        print("=== 0. prove the capture path with a tone I generate ===")
        write_tone(tone)
        rec = start_capture(tdp / "selftest.wav")
        time.sleep(2.5)                      # let parec actually begin
        print("  (recorder running; now playing the tone)")
        play = subprocess.Popen(["paplay", str(tone)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(3.0)
        play.terminate()
        stop_capture(rec)
        pk, _, _ = peak_of(tdp / "selftest.wav")
        print(f"  self-test peak = {pk:.5f} {'(capture works)' if pk > 0.05 else '(CAPTURE IS BROKEN)'}")
        if pk <= 0.05:
            print("  Refusing to test the guest with a capture that cannot hear the host.")
            return 1

        print("\n=== 1. push and start the guest's playback (as a .ps1, never inline) ===")
        ps1 = tdp / "audio-play-test.ps1"
        ps1.write_text(PS1)
        push = subprocess.run(
            ["./target/release/wvm", "vm", "transfer", "push", "--config",
             str(Path.home() / "wvm/wvm.toml"), str(ps1), GUEST_PS1, "--overwrite"],
            cwd=str(REPO), capture_output=True, text=True, timeout=120)
        print(f"  push: {(push.stdout or push.stderr).strip()[:90]}")

        print("\n=== 2. open a viewer, then play and capture at the same time ===")
        viewer = subprocess.Popen(["remote-viewer", f"spice+unix://{SOCK}"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            time.sleep(9)
            si = subprocess.run(["pactl", "list", "sink-inputs"], capture_output=True, text=True).stdout
            print(f"  viewer sink-inputs BEFORE playback: "
                  f"{len([b for b in si.split('Sink Input #') if 'viewer' in b.lower() or 'spice' in b.lower()])}")

            # Fire the guest playback without waiting for it; capture while it runs.
            rec = start_capture(back)
            time.sleep(2.5)                  # parec ready BEFORE any sound is made
            guest_started = subprocess.Popen(
                ["python3", "scripts/guest-exec.py",
                 f"powershell.exe -NoProfile -ExecutionPolicy Bypass -File {GUEST_PS1}"],
                cwd=str(REPO), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            time.sleep(16)                   # the guest loops the wav; capture across it
            ok = stop_capture(rec)
            si2 = subprocess.run(["pactl", "list", "sink-inputs"], capture_output=True, text=True).stdout
            n2 = len([b for b in si2.split("Sink Input #") if "viewer" in b.lower() or "spice" in b.lower()])
            print(f"  viewer sink-inputs WHILE playing:  {n2}")
            guest_started.terminate()

            pk2, frames, rate = peak_of(back) if ok else (0.0, 0, 0)
            print(f"  captured: {frames} frames at {rate} Hz, peak = {pk2:.5f}")

            print("\n=== 3. did the guest actually play? (its own log, from a file) ===")
            log = guest(f"type {GUEST_LOG}")
            for line in log.strip().splitlines()[-5:]:
                print(f"    {line.strip()[:120]}")

            print("\n=== verdict ===")
            played = "played=True" in log
            if not played:
                print("  GUEST DID NOT PLAY -- the silence says nothing about the SPICE audio path.")
                print("  Fix the playback first, then re-run.")
            elif pk2 > 0.05:
                print("  AUDIO ARRIVES IN THE VIEWER. Measured, not assumed.")
            else:
                print("  GUEST PLAYED but nothing reached the host sink: the guest's audio is NOT")
                print("  audible in the viewer. Device and driver are fine, so this is the")
                print("  QEMU -> SPICE -> viewer leg. An honest, bounded finding.")
            return 0
        finally:
            if viewer.poll() is None:
                viewer.terminate()
                try:
                    viewer.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    viewer.kill()
            print("  (viewer closed)")


if __name__ == "__main__":
    sys.exit(main())
