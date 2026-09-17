#!/usr/bin/env python3
"""Does the guest's audio actually REACH the viewer? Measured, with a silent baseline.

WHY A BASELINE

The temptation is to open a viewer, play a sound in the guest, and if a playback stream exists on
the host, call it proven. That check cannot fail: remote-viewer brings its playback stream up when
the SPICE channel is negotiated, so the stream exists whether or not any audio ever flows through
it. It would report success on a completely broken audio path.

So this records the SAME sink twice -- once with the guest silent, once with the guest playing -- and
compares. Silence in both means no audio is arriving, whatever the channel state says.

The sound is a real WAV played through the audio device, NOT `[console]::beep`, which uses the PC
speaker and would bypass the emulated sound card entirely -- a test that could pass with the audio
path torn out.

Caveat stated plainly: this records the host's DEFAULT sink monitor, so any other audio the user is
playing would contaminate it. That is why both captures are reported as numbers rather than reduced
to a yes/no.
"""
import shutil
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path

REPO = Path("/home/andy/LSW")
SOCK = "/run/user/1000/wvm/w11.spice.sock"


def peak_of(path: Path) -> tuple[float, int]:
    """Peak amplitude (0..1) and frame count.

    RAISES on a sample width it was not built for. That matters: the first version returned 0.0 for
    anything wider than 16-bit, so a perfectly good 32-bit recording read as PERFECT SILENCE. A
    parser that cannot read its input must not answer "no sound" -- that is the same mistake as a
    detector that cannot tell a blank screen from a BSOD.
    """
    with wave.open(str(path), "rb") as w:
        width, frames = w.getsampwidth(), w.getnframes()
        raw = w.readframes(frames)
    if not raw:
        return 0.0, frames
    if width != 2:
        raise ValueError(
            f"{path.name}: expected 16-bit samples, got {width * 8}-bit. "
            "Record with --format=s16le; do NOT read this as silence."
        )
    import array
    samples = array.array("h")
    samples.frombytes(raw)
    return (max(abs(s) for s in samples) / 32768.0 if samples else 0.0), frames



def capture(seconds: int, dest: Path) -> bool:
    """Record the default sink's monitor."""
    cmd = ["parec", "--file-format=wav", "--format=s16le", "--rate=48000", "--channels=2", "--device=@DEFAULT_MONITOR@", str(dest)]
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(seconds)
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    return dest.exists() and dest.stat().st_size > 44


def main() -> int:
    for tool in ("parec", "remote-viewer"):
        if not shutil.which(tool):
            print(f"  FAIL  {tool} is not installed")
            return 1
    if not Path(SOCK).exists():
        print(f"  FAIL  no display socket at {SOCK}")
        return 1

    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)
        silent_wav, loud_wav = tdp / "silent.wav", tdp / "playing.wav"

        viewer = subprocess.Popen(["remote-viewer", f"spice+unix://{SOCK}"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            time.sleep(10)  # connect, negotiate the playback channel, settle

            print("=== 1. is a viewer playback stream present at all? ===")
            si = subprocess.run(["pactl", "list", "sink-inputs"], capture_output=True, text=True).stdout
            viewer_streams = [b for b in si.split("Sink Input #") if "viewer" in b.lower() or "spice" in b.lower()]
            print(f"  sink-inputs mentioning the viewer/spice: {len(viewer_streams)}")
            for b in viewer_streams:
                name = [ln.strip() for ln in b.splitlines() if "application.name" in ln]
                state = [ln.strip() for ln in b.splitlines() if ln.strip().startswith("State:")]
                print(f"    {name} {state}")

            print("\n=== 2. BASELINE: guest silent ===")
            if capture(4, silent_wav):
                pk, frames = peak_of(silent_wav)
                print(f"  peak={pk:.5f}  frames={frames}")
            else:
                print("  FAIL  could not record")
                return 1

            print("\n=== 3. guest plays a real WAV through the sound card ===")
            # PlaySync with a real media file: this goes through the emulated HDA device, through
            # QEMU's audio subsystem, down the SPICE playback channel, to the viewer.
            ps = (
                "1..6 | ForEach-Object { "
                "(New-Object Media.SoundPlayer 'C:\\Windows\\Media\\Alarm01.wav').PlaySync() }"
            )
            player = subprocess.Popen(
                ["python3", "scripts/guest-exec.py",
                 f'powershell.exe -NoProfile -Command "{ps}"'],
                cwd=str(REPO), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            )
            time.sleep(4)  # let the first PlaySync start
            if capture(8, loud_wav):
                pk, frames = peak_of(loud_wav)
                print(f"  peak={pk:.5f}  frames={frames}")
            else:
                print("  FAIL  could not record while playing")
                return 1
            player.terminate()

            print("\n=== verdict ===")
            s_peak = peak_of(silent_wav)[0]
            l_peak = peak_of(loud_wav)[0]
            print(f"  silent  peak = {s_peak:.5f}")
            print(f"  playing peak = {l_peak:.5f}")
            if l_peak > s_peak + 0.02:
                print("  AUDIO ARRIVES IN THE VIEWER: the sink got measurably louder when the guest")
                print("  played. That is the whole path: guest -> HDA -> QEMU -> SPICE -> viewer.")
            elif l_peak < 0.005:
                print("  NOT VERIFIED as audible: both captures are near silence. The device and the")
                print("  channel exist, but nothing is reaching the host sink from the guest.")
            else:
                print("  INCONCLUSIVE: there is host audio in both captures (the user's own sound?)")
                print("  and it is not separable from the guest's. Reported as numbers above.")
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
