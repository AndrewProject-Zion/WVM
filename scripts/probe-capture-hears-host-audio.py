#!/usr/bin/env python3
"""Prove the SOUND CAPTURE path works before believing any silence it reports.

WHY

`probe-spice-audio-arrives.py` returned peak=0.00000 for the guest playing AND for the guest silent.
Those two results are indistinguishable from a capture device that is recording nothing at all --
the same shape as the CPU probe that reported 0.00% on every arm including a busy one. A silence
reading is only evidence if the instrument has been shown to produce a non-silence reading.

So: play a known sound THROUGH THE HOST's audio stack and capture it. This exercises exactly the
capture half (parec + the monitor source) without involving QEMU, Windows or SPICE at all.

If this reports a peak, the capture works and the guest's silence is meaningful.
If this ALSO reports silence, then nothing measured earlier says anything about the guest.
"""
import array
import shutil
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path


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



def capture(seconds: float, dest: Path, device: str) -> bool:
    proc = subprocess.Popen(
        ["parec", "--file-format=wav", "--format=s16le", "--rate=48000", "--channels=2", f"--device={device}", str(dest)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    time.sleep(seconds)
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    return dest.exists() and dest.stat().st_size > 44


def main() -> int:
    print("=== what does the host say its default sink is? ===")
    info = subprocess.run(["pactl", "info"], capture_output=True, text=True).stdout
    for line in info.splitlines():
        if "Default Sink" in line or "Default Source" in line:
            print("  " + line.strip())

    print("\n=== does @DEFAULT_MONITOR@ resolve, and can it hear the host? ===")
    # Find a real sound file to play. Any of these is fine; the point is a known non-silent signal.
    candidates = list(Path("/usr/share/sounds").rglob("*.wav")) + \
                 list(Path("/usr/share/sounds").rglob("*.oga"))
    print(f"  sound files on this host: {len(candidates)}")
    if not candidates:
        print("  FAIL  no sound file to play with -- cannot prove the capture path this way")
        return 1

    # .oga is ogg; play through paplay which handles it. Take a small one to keep it quick.
    candidates.sort(key=lambda p: p.stat().st_size)
    sample = next((c for c in candidates if c.suffix == ".wav"), candidates[0])
    print(f"  playing: {sample}")

    with tempfile.TemporaryDirectory() as td:
        out = Path(td) / "host.wav"
        players = ["paplay", "aplay", "ffplay"]
        player = next((p for p in players if shutil.which(p)), None)
        if not player:
            print("  FAIL  no player (paplay/aplay/ffplay) available")
            return 1

        play = subprocess.Popen(
            [player, str(sample)] if player != "ffplay" else
            [player, "-nodisp", "-autoexit", str(sample)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        time.sleep(0.4)  # let it start
        ok = capture(3.0, out, "@DEFAULT_MONITOR@")
        pk, frames = peak_of(out) if ok else (0.0, 0)
        rc = play.poll()
        play.terminate()

        print("\n=== result ===")
        print(f"  captured frames : {frames}")
        print(f"  captured peak   : {pk:.5f}")
        print(f"  player exit     : {rc}")
        if pk > 0.01:
            print("\n  INSTRUMENT OK: the capture path hears host audio. So a silent reading while")
            print("  the guest plays is MEANINGFUL and means the guest's audio is not arriving.")
            return 0
        print("\n  INSTRUMENT BROKEN: the capture heard nothing even for a host sound. Every")
        print("  silence reading taken with it is worthless -- fix the capture before concluding")
        print("  anything about the guest's audio.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
