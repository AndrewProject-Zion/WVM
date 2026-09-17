#!/usr/bin/env python3
"""Prove scripts/read-minidump.py can actually name a module — before trusting it on a real dump.

WHY

read-minidump.py has so far only been run against PAGEDU64 kernel dumps, which it correctly
rejected with "not a minidump". A tool that has only ever said "no" has not been shown to be able to
say "yes". If the next real crash produces an MDMP and the parser prints something wrong — or
nothing — that would be discovered at the worst moment, and a wrong module name would send the
investigation after an innocent driver.

So: build a minidump with a KNOWN answer and check the parser recovers it exactly.

The synthetic dump contains two modules and an exception address inside the second one:

    ntoskrnl.exe   base 0xfffff80000000000  size 0x100000
    viostor.sys    base 0xfffff80100000000  size 0x20000

    exception address 0xfffff80100000140  ->  viostor.sys + 0x140

The parser must report viostor.sys with offset 0x140. If it names ntoskrnl, or reports the wrong
offset, the parser is wrong and the real dump would mislead.

Also checks the negative case: an address outside every module must be reported as such rather than
attributed to the nearest one.

Usage: python3 scripts/test-read-minidump.py
Exit:  0 if the parser behaves, 1 otherwise.
"""
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

# --- build a minidump with a known layout -------------------------------------------------------

MODULES = [
    (0xFFFFF80000000000, 0x100000, "ntoskrnl.exe"),
    (0xFFFFF80100000000, 0x20000, "viostor.sys"),
]
FAULT = 0xFFFFF80100000140          # inside viostor.sys
OUTSIDE = 0xFFFFF90000000000        # inside nothing

MODULE_LIST_STREAM = 4
EXCEPTION_STREAM = 6


def build(fault_address: int) -> bytes:
    """Lay the file out by hand: header, directory, exception stream, module list, names."""
    HEADER_LEN = 32
    DIR_LEN = 2 * 12
    dir_rva = HEADER_LEN
    exc_rva = HEADER_LEN + DIR_LEN
    exc_len = 64
    mod_rva = exc_rva + exc_len
    mod_len = 4 + 108 * len(MODULES)
    names_start = mod_rva + mod_len

    # Module name strings first, so their RVAs are known when the records are written.
    names = []
    off = names_start
    for _, _, name in MODULES:
        raw = name.encode("utf-16-le")
        names.append((off, raw))
        off += 4 + len(raw)

    total = off
    buf = bytearray(total)

    # Header
    struct.pack_into("<IIIIIIQ", buf, 0,
                     0x504D444D,      # 'MDMP'
                     0x0000A793,      # version
                     2,               # NumberOfStreams
                     dir_rva,
                     0,               # CheckSum
                     0x68C40000,      # TimeDateStamp
                     0)

    # Directory: [StreamType, DataSize, Rva]
    struct.pack_into("<III", buf, dir_rva + 0, EXCEPTION_STREAM, exc_len, exc_rva)
    struct.pack_into("<III", buf, dir_rva + 12, MODULE_LIST_STREAM, mod_len, mod_rva)

    # Exception stream: ThreadId(4) pad(4) then EXCEPTION_RECORD whose ExceptionAddress is at +16.
    struct.pack_into("<I", buf, exc_rva, 0x1234)
    struct.pack_into("<I", buf, exc_rva + 8, 0x00000050)          # ExceptionCode = the bugcheck
    struct.pack_into("<Q", buf, exc_rva + 8 + 16, fault_address)  # ExceptionAddress

    # Module list
    struct.pack_into("<I", buf, mod_rva, len(MODULES))
    for i, ((base, size, _), (name_rva, _)) in enumerate(zip(MODULES, names)):
        rec = mod_rva + 4 + i * 108
        struct.pack_into("<Q", buf, rec, base)
        struct.pack_into("<I", buf, rec + 8, size)
        struct.pack_into("<I", buf, rec + 20, name_rva)

    for name_rva, raw in names:
        struct.pack_into("<I", buf, name_rva, len(raw))   # length in BYTES
        buf[name_rva + 4: name_rva + 4 + len(raw)] = raw

    return bytes(buf)


def main():
    tmp = Path(tempfile.mkdtemp(prefix="wvm-mdmp-"))
    failures = []

    # --- case 1: the address is inside viostor.sys ---
    p1 = tmp / "known-viostor.dmp"
    p1.write_bytes(build(FAULT))
    r = subprocess.run([sys.executable, "scripts/read-minidump.py", str(p1)],
                       capture_output=True, text=True, cwd=".")
    out = r.stdout + r.stderr
    if "viostor.sys" in out and "+0x140" in out:
        print("  PASS: named viostor.sys at the right offset")
    else:
        failures.append("did not name viostor.sys +0x140")
        print("  FAIL: expected viostor.sys +0x140")
        print("   ", out.strip().replace("\n", "\n    ")[:500])

    # A wrong attribution is worse than none, so check it did not blame the kernel.
    if "ntoskrnl.exe" in out.split("FAULT IN:")[-1][:40]:
        failures.append("attributed the fault to ntoskrnl.exe")
        print("  FAIL: blamed ntoskrnl.exe")

    # --- case 2: an address inside no module must be refused, not guessed ---
    p2 = tmp / "known-outside.dmp"
    p2.write_bytes(build(OUTSIDE))
    r2 = subprocess.run([sys.executable, "scripts/read-minidump.py", str(p2)],
                        capture_output=True, text=True, cwd=".")
    out2 = r2.stdout + r2.stderr
    if "not inside any listed module" in out2:
        print("  PASS: an address in no module is reported as such")
    else:
        failures.append("did not report an unattributable address as unattributable")
        print("  FAIL: expected 'not inside any listed module'")
        print("   ", out2.strip().replace("\n", "\n    ")[:400])

    print()
    if failures:
        print("PARSER IS NOT TRUSTWORTHY:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASS: the parser names the right module and refuses an unknown address.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
