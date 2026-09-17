#!/usr/bin/env python3
"""Name the module that faulted, by reading the module list out of a Windows minidump.

WHY THIS EXISTS

The guest bugchecked three times with identical parameters:

    0x50 (0xfffffffffffffff8, 0x44, 0xfffff8072cc54277, 0xe)
    0x50 (0xfffffffffffffff8, 0x44, 0xfffff80158a54277, 0xe)
    0x50 (0xfffffffffffffff8, 0x44, 0xfffff80452854277, 0xe)

Same code, same first two parameters, and a faulting address that differs only in the bits KASLR
randomises. A bare stop/cont for 60 seconds does NOT reproduce it, so it is tied to the snapshot's
state write rather than to pausing the machine.

That much is measured. What was still unknown is WHICH DRIVER faults, and every extra guess about it
is a guess. The minidump answers it directly: it carries the loaded-module list with base addresses,
so the faulting instruction address can be attributed to a named image with an offset — which is the
thing you need before looking anything up.

No debugger and no Windows required. The format is documented and stable:

    MINIDUMP_HEADER       32 bytes: 'MDMP', version, NumberOfStreams, StreamDirectoryRva, ...
    directory entry       12 bytes: StreamType, DataSize, Rva
    stream 4              ModuleList: u32 count, then 108-byte MINIDUMP_MODULE records
    MINIDUMP_MODULE       u64 BaseOfImage, u32 SizeOfImage, ..., rva ModuleNameRva, ...
    string                u32 length IN BYTES, then UTF-16

Usage: python3 scripts/read-minidump.py <dump.dmp> [...]
       python3 scripts/read-minidump.py --address 0xfffff8072cc54277 <dump.dmp>
"""
import argparse
import struct
import sys
from pathlib import Path

MODULE_LIST_STREAM = 4
EXCEPTION_STREAM = 6


def u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def u64(b, o):
    return struct.unpack_from("<Q", b, o)[0]


def read_string(buf, rva):
    """MINIDUMP_STRING: a byte length followed by UTF-16 code units."""
    n = u32(buf, rva)
    raw = buf[rva + 4: rva + 4 + n]
    return raw.decode("utf-16-le", errors="replace")


def modules(buf, rva, count):
    out = []
    for i in range(count):
        rec = rva + 4 + i * 108
        base = u64(buf, rec)
        size = u32(buf, rec + 8)
        name_rva = u32(buf, rec + 20)
        try:
            name = read_string(buf, name_rva)
        except Exception:
            name = "?"
        out.append((base, size, name))
    return out


def exception_address(buf, rva):
    """The address the CPU was executing when the fault happened, from the dump itself.

    EXCEPTION_STREAM is ThreadId(4), alignment(4), then EXCEPTION_RECORD whose ExceptionAddress
    sits at offset 16 — after ExceptionCode(4), ExceptionFlags(4) and the nested record pointer(8).
    """
    return u64(buf, rva + 8 + 16), u32(buf, rva + 8)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dumps", nargs="+")
    ap.add_argument("--address", help="override the address to attribute (from Event 1001 param 3)")
    args = ap.parse_args()

    override = int(args.address, 16) if args.address else None

    for path in args.dumps:
        data = Path(path).read_bytes()
        print(f"\n=== {Path(path).name} ({len(data)} bytes) ===")

        if data[:4] != b"MDMP":
            print("  not a minidump (no 'MDMP' signature)")
            continue

        nstreams = u32(data, 8)
        dir_rva = u32(data, 12)
        stamp = u32(data, 20)
        import datetime
        when = datetime.datetime.utcfromtimestamp(stamp).isoformat() if stamp else "?"
        print(f"  {nstreams} streams, written {when} UTC")

        mods = None
        fault = None
        code = None
        for i in range(nstreams):
            entry = dir_rva + i * 12
            stype = u32(data, entry)
            srva = u32(data, entry + 8)
            if stype == MODULE_LIST_STREAM:
                mods = modules(data, srva, u32(data, srva))
            elif stype == EXCEPTION_STREAM:
                try:
                    fault, code = exception_address(data, srva)
                except Exception:
                    pass

        if fault:
            print(f"  exception address in the dump: 0x{fault:016x}   (code 0x{code:08x})")
        addr = override or fault
        if addr is None:
            print("  no faulting address available (pass --address)")
            continue
        if override and fault and override != fault:
            print(f"  NOTE: --address 0x{override:016x} differs from the dump's 0x{fault:016x}")

        if not mods:
            print("  no module list in this dump")
            continue

        print(f"  {len(mods)} modules loaded")
        hit = [(b, s, n) for (b, s, n) in mods if b <= addr < b + s]
        if not hit:
            print(f"  0x{addr:016x} is not inside any listed module")
            continue

        base, size, name = hit[0]
        print()
        print(f"  ########################################################")
        print(f"  # FAULT IN: {name}")
        print(f"  #   image base 0x{base:016x}   size 0x{size:x}")
        print(f"  #   offset    +0x{addr - base:x}")
        print(f"  ########################################################")


if __name__ == "__main__":
    sys.exit(main())
