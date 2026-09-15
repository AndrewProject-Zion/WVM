#!/usr/bin/env python3
"""Measure what is actually slow in the WVM-01 regression test, rather than guessing.

The test passes but takes 60+ seconds. Two candidate causes:

  A. `std::io::pipe()` read/write with small internal buffers, so 4 MiB is many syscalls.
  B. Something in the drain path is not concurrent after all.

These are distinguishable by timing the writer and the reader separately. If the writer finishes
fast and the reader is slow, it is (A) — transfer cost. If the writer blocks until the reader
starts (or vice versa), it is (B) — a sequencing problem.

Run from the repo root:  python3 scripts/measure-pipe-throughput.py
"""
import time

# Written as a standalone Rust program because the point is to time the primitives under the exact
# conditions the test uses, not to reason about them.
SOURCE = r"""
use std::io::{Read, Write};
use std::time::Instant;

fn main() {
    const CHUNK: usize = 256 * 1024;
    const CHUNKS: usize = 16;

    let (mut reader, mut writer) = std::io::pipe().expect("pipe");

    // Writer on its own thread, exactly as the test does it, so a block shows up as a long time
    // rather than as a deadlock that tells us nothing.
    let w = std::thread::spawn(move || {
        let start = Instant::now();
        for _ in 0..CHUNKS {
            writer.write_all(&vec![b'x'; CHUNK]).expect("write");
        }
        start.elapsed()
    });

    let r_start = Instant::now();
    let mut total = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }
    }
    let r_elapsed = r_start.elapsed();

    let w_elapsed = w.join().expect("writer");
    println!("bytes:   {}", total);
    println!("writer:  {:?}", w_elapsed);
    println!("reader:  {:?}", r_elapsed);
}
"""


def main() -> int:
    import pathlib
    import subprocess
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        src = pathlib.Path(tmp) / "measure.rs"
        src.write_text(SOURCE)
        binary = pathlib.Path(tmp) / "measure"

        print("compiling the measurement...")
        build = subprocess.run(
            ["rustc", "-O", "-o", str(binary), str(src)],
            capture_output=True, text=True,
        )
        if build.returncode != 0:
            print(build.stderr[:800])
            return 2

        print("running...")
        started = time.monotonic()
        run = subprocess.run([str(binary)], capture_output=True, text=True, timeout=240)
        wall = time.monotonic() - started

    if run.returncode != 0:
        print("failed:", run.stderr[:400])
        return 2

    print(run.stdout.strip())
    print(f"wall:    {wall:.2f}s")

    # Interpret it, so the number leads somewhere instead of being another datum to hold.
    writer_line = next((l for l in run.stdout.splitlines() if l.startswith("writer:")), "")
    reader_line = next((l for l in run.stdout.splitlines() if l.startswith("reader:")), "")
    print()
    if "ms" in writer_line and "s" not in writer_line.replace("ms", ""):
        print("  Writer completed in milliseconds, so it never blocked past the pipe buffer.")
        print("  That rules out (B): the drain IS concurrent. The cost is "
              "transfer cost (A).")
        print("  The test's 4 MiB is therefore measuring throughput, not the bug.")
        print("  It should be sized just past the 64 KiB boundary instead.")
    else:
        print("  Writer did not finish quickly, so the drain may not be concurrent after all.")
        print("  That is (B), and it means the fix is not doing what it claims.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
