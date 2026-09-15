#!/usr/bin/env python3
"""Prove the WVM-01 regression test actually catches the bug it was written for.

A regression test that passes on the broken code measures nothing. This temporarily restores the
ORIGINAL sequencing — read the pipe only after the writer has finished — and asserts the test
FAILS. Then it puts the real file back.

Run from the repo root:  python3 scripts/verify-wvm-01-regression.py
"""
import pathlib
import shutil
import subprocess
import sys

SRC = pathlib.Path("wvm-guest/src/win32.rs")
BACKUP = pathlib.Path("/tmp/win32.rs.verify-backup")

BROKEN = """        // BROKEN SEQUENCING REINTRODUCED BY THE VERIFIER: write everything first, then read.
        let finisher = std::thread::spawn(move || {
            for _ in 0..CHUNKS {
                writer.write_all(&vec![b'x'; CHUNK]).expect("write");
            }
            let _ = writer;
        });
        finisher.join().expect("writer thread");

        let handle = spawn_pipe_reader(Some(reader));
        let drained = join_pipe_reader(handle);
"""

CORRECT = """        let finisher = std::thread::spawn(move || {
            for _ in 0..CHUNKS {
                writer.write_all(&vec![b'x'; CHUNK]).expect("write");
            }
            // Dropping the writer closes it, which is what lets the reader reach EOF. Without this
            // the reader blocks forever — the same deadlock, from the other direction.
        });

        let handle = spawn_pipe_reader(Some(reader));
        let drained = join_pipe_reader(handle);
        finisher.join().expect("writer thread");
"""


def main() -> int:
    if not SRC.exists():
        print("run me from the repo root", file=sys.stderr)
        return 2

    original = SRC.read_text()
    if CORRECT not in original:
        print("could not find the correct sequencing to swap out", file=sys.stderr)
        return 2

    shutil.copy2(SRC, BACKUP)
    try:
        print("reintroducing the original sequencing (write all, then read)...")
        SRC.write_text(original.replace(CORRECT, BROKEN))

        print("running the regression test against the BROKEN code (expect FAILURE)...")
        try:
            result = subprocess.run(
                [
                    "cargo", "test", "-p", "wvm-guest",
                    "a_pipe_larger_than_the_pipe_buffer_is_drained_concurrently",
                    "--", "--nocapture",
                ],
                capture_output=True,
                text=True,
                timeout=90,
            )
        except subprocess.TimeoutExpired:
            # This is the bug reproducing itself, and it is the strongest possible evidence.
            #
            # With the original sequencing the writer fills the 64 KiB pipe buffer, nobody is
            # reading, so the writer BLOCKS FOREVER on its next write — and the test hangs rather
            # than failing. That is precisely the failure mode WVM-01 describes: not "output is
            # truncated past 64 KiB" but "the process wedges and is reported as timed_out".
            #
            # So a hang here is a PASS for the verifier: the test is exercising the real deadlock.
            print()
            print("  VERDICT: the test HUNG on broken code — which is WVM-01 reproducing itself.")
            print("  The writer blocked on a full pipe nobody was draining. That is the bug.")
            return 0

        combined = result.stdout + result.stderr

        if result.returncode == 0:
            print()
            print("  VERDICT: the test PASSED on broken code — it does not catch WVM-01.")
            print("  A regression test that cannot fail is decoration.")
            return 1

        print("  VERDICT: it FAILED on broken code, as it must.")
        for line in combined.splitlines():
            if "drained" in line.lower() or "assertion" in line.lower():
                print("   ", line.strip()[:100])
                break
        return 0
    finally:
        # Always restore, including on the failure path — leaving the broken version on disk would
        # be a worse outcome than any verdict this script can produce.
        shutil.copy2(BACKUP, SRC)
        print("  restored the real file")


if __name__ == "__main__":
    sys.exit(main())
