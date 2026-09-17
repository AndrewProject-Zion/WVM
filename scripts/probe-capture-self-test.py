#!/usr/bin/env python3
"""Prove the capture path hears a signal WHOSE CONTENT I CONTROLLED.

WHY THE PREVIOUS ATTEMPT WAS WORTHLESS

It played /usr/share/sounds/sound-icons/percussion-10.wav -- chosen because it was the SMALLEST file
on the host. Nothing verified it contained any audio, so "the capture heard nothing" had two
possible causes and no way to tell them apart: a broken capture path, or a silent source file. A
test whose two failure modes are indistinguishable is not a test.

So this GENERATES the signal: a full-scale 1 kHz sine, written here, played through the host's audio
stack, and captured back. Peak amplitude is then a fact about the capture path and nothing else.

If this reads near zero, the capture path is the problem and every silence reading taken through it
must be thrown away.
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


def write_tone(path: Path, seconds: float = 3.0, rate: int = 48000, freq: int = 1000) -> None:
    """A full-scale sine. Loud by construction, so the capture has something to find."""
    frames = int(seconds * rate)
    amp = int(0.9 * 32767)
    samples = array.array("h", (
        int(amp * math.sin(2.0 * math.pi * freq * i / rate)) for i in range(frames)
    ))
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(rate)
        stereo = array.array("h")
        for s in samples:
            stereo.append(s)
            stereo.append(s)
        w.writeframes(stereo.tobytes())
    print(f"  generated {path.name}: {frames} frames, {seconds}s, peak {amp / 32767:.2f} (by construction)")


def peak_of(path: Path) -> tuple[float, int, int]:
    """Peak, frames, rate. Raises rather than reporting silence for a format it cannot read."""
    with wave.open(str(path), "rb") as w:
        width, frames, rate = w.getsampwidth(), w.getnframes(), w.getframerate()
        raw = w.readframes(frames)
    if not raw:
        return 0.0, frames, rate
    if width != 2:
        raise ValueError(f"expected 16-bit, got {width * 8}-bit -- do NOT read as silence")
    samples = array.array("h")
    samples.frombytes(raw)
    return (max(abs(s) for s in samples) / 32768.0 if samples else 0.0), frames, rate


def capture(seconds: float, dest: Path) -> bool:
    proc = subprocess.Popen(
        ["parec", "--file-format=wav", "--format=s16le", "--rate=48000", "--channels=2",
         "--device=@DEFAULT_MONITOR@", str(dest)],
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
    player = next((p for p in ("paplay", "pw-play", "aplay") if shutil.which(p)), None)
    if not (player and shutil.which("parec")):
        print(f"  FAIL  need a player and parec; player={player}")
        return 1
    print(f"  player: {player}")

    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)
        tone, back = tdp / "tone.wav", tdp / "captured.wav"

        print("=== 1. generate a signal whose content is known ===")
        write_tone(tone)

        print("\n=== 2. play it into the host's audio stack ===")
        play = subprocess.Popen([player, str(tone)],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        time.sleep(0.5)

        print("=== 3. capture the default sink's monitor ===")
        ok = capture(2.5, back)
        out = play.communicate(timeout=20)[0] if play.poll() is None else ""
        play.terminate()

        if not ok:
            print("  FAIL  parec produced nothing")
            return 1
        pk, frames, rate = peak_of(back)

        print("\n=== result ===")
        print(f"  player said     : {out.strip()[:90]!r}")
        print(f"  captured frames : {frames} at {rate} Hz = {frames / max(rate, 1):.2f}s")
        print(f"  captured peak   : {pk:.5f}")

        if pk > 0.05:
            print("\n  INSTRUMENT OK. The capture path hears a signal I generated and verified, so")
            print("  a silent reading while the guest plays WOULD be meaningful.")
            return 0
        print("\n  INSTRUMENT STILL BROKEN: a full-scale tone played by the host came back silent.")
        print("  Every silence reading taken through this capture is worthless. The guest's audio")
        print("  is therefore UNKNOWN -- not broken, and not verified.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
