#!/usr/bin/env bash
# Which display backends can QEMU bind to a UNIX socket, versus only TCP?
#
# WHY THIS MATTERS
#
# The plan is "always-on headless display server + on-demand viewer". The security property that
# makes that acceptable is that the server is reachable ONLY through the filesystem: a Unix socket
# under a directory owned by the user, gated by POSIX permissions. A backend that can only bind TCP
# would put a Windows desktop behind a host port, which is a different risk profile entirely.
#
# So the claim "use a unix socket" has to be tested per backend rather than assumed. It is easy to
# get wrong: with no client connected, QEMU still prints nothing about where it bound, and it
# UNLINKS the socket when it exits — so checking after termination shows nothing either way.
# That mistake was made once already; this script checks while the process is alive.
#
# Usage: bash scripts/probe-display-sockets.sh

set -u
QEMU=${QEMU:-qemu-system-x86_64}

probe() {
    local args="$1" sock="$2" label="$3"
    rm -f "$sock"
    # shellcheck disable=SC2086
    "$QEMU" -display none $args -S -nodefaults -monitor none -m 64 </dev/null >/tmp/ds.log 2>&1 &
    local pid=$!
    sleep 2.5

    if [ -S "$sock" ]; then
        printf '  %-28s socket CREATED  (%s)\n' "$label" "$(stat -c '%A' "$sock")"
    else
        local note
        note=$(head -1 /tmp/ds.log 2>/dev/null | cut -c1-70)
        printf '  %-28s NO socket      %s\n' "$label" "${note:-}"
    fi

    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    rm -f "$sock"
}

echo "=== display backends, checked WHILE qemu is running ==="
probe "-vnc unix:/tmp/vnct.sock"                      /tmp/vnct.sock "vnc  unix:"
probe "-spice unix=on,addr=/tmp/spct.sock,disable-ticketing=on" /tmp/spct.sock "spice unix="

echo
echo "=== and what a TCP bind looks like, for contrast ==="
# No unix socket is expected here; the point is that it starts and binds a TCP port instead.
probe "-vnc 127.0.0.1:5"                              /tmp/never.sock "vnc  127.0.0.1:5"

echo
echo "=== installed viewers ==="
for v in remote-viewer spicy virt-viewer remmina vncviewer xtightvncviewer; do
    p=$(command -v "$v" 2>/dev/null) && printf '  %-16s %s\n' "$v" "$p"
done

echo
echo "=== does this qemu have the spice-app / dbus display paths? ==="
"$QEMU" -display help 2>&1 | tr '\n' ' ' | sed 's/^/  /'
echo
