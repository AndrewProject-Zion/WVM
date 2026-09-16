#!/usr/bin/env python3
"""Diagnose why a QMP socket refuses connections despite appearing bound.

The interesting case: the file exists, the process holds a LISTEN socket, and connections are
still refused. The usual cause is an inode mismatch — the file on disk is not the socket the
process actually bound, because something replaced or recreated the path after binding.

Walking /proc/net/unix is the way to settle it, since it reports the real bound inode.
"""

from pathlib import Path
import os
import socket
import sys


def bound_inodes_for(path):
    """Find LISTEN unix sockets whose path matches, and return their inodes."""
    found = []
    with open("/proc/net/unix") as f:
        next(f)  # header
        for line in f:
            parts = line.split()
            if len(parts) < 8:
                continue
            # Format: Num RefCount Protocol Flags Type St Inode Path
            inode = parts[6]
            listed_path = parts[7] if len(parts) > 7 else ""
            state = parts[3]
            sock_type = parts[4]
            if listed_path == path or listed_path.endswith(os.path.basename(path)):
                found.append(
                    {
                        "inode": inode,
                        "path_shown": listed_path,
                        "flags": state,
                        "type": sock_type,
                        "refcount": parts[1],
                    }
                )
    return found


def main(path):
    print(f"diagnosing {path}")
    print()

    if not os.path.exists(path):
        print("  the path does not exist at all")
        return 1

    st = os.stat(path)
    is_socket = (st.st_mode & 0o170000) == 0o140000
    print(f"  exists:        yes")
    print(f"  is a socket:   {is_socket}")
    print(f"  disk inode:    {st.st_ino}")
    print(f"  mode:          {oct(st.st_mode & 0o777)}")
    print()

    bound = bound_inodes_for(path)
    if not bound:
        print("  NOT in /proc/net/unix -> nothing is listening at this path.")
        print("  The file is a leftover; safe to remove.")
        return 1

    print("  bound sockets at this path:")
    for b in bound:
        # Flags 00010000 = LISTEN for unix sockets.
        listening = b["flags"] == "00010000"
        print(
            f"    inode {b['inode']}  flags {b['flags']}"
            f"  {'(LISTENING)' if listening else '(connected)'}"
        )
    print()

    disk_inode = str(st.st_ino)
    bound_inodes = {b["inode"] for b in bound}

    if disk_inode not in bound_inodes:
        print("  MISMATCH")
        print(f"    the file on disk has inode {disk_inode},")
        print(f"    but the listening socket has inode {', '.join(bound_inodes)}.")
        print()
        print("  Meaning: something unlinked and recreated the path AFTER the process bound its")
        print("  socket. The process is listening on an inode no longer reachable by that name, so")
        print("  connecting by path is refused even though a listener exists.")
        print()
        print("  Cause seen in practice: a second QEMU start removed the stale socket file and")
        print("  recreated it, while the first QEMU was still holding open the original inode.")
        print("  The fix is to stop ALL QEMU processes, remove the path, and start once.")
        return 2

    print("  inode matches — the listener is reachable by this path.")
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(3)
        s.connect(path)
        print("  connect: SUCCEEDED")
        s.close()
        return 0
    except OSError as e:
        print(f"  connect: FAILED ({e})")
        print()
        print("  The inode matches and the socket is listening, but the connection is refused.")
        print("  Check the process is not stalled, and that the listen backlog is not full.")
        return 3


if __name__ == "__main__":
    target = sys.argv[1] if len(sys.argv) > 1 else str(Path.home() / ".local/state/wvm/w11/qmp.sock")
    sys.exit(main(target))
