#!/usr/bin/env bash
#
# Install the wvm CLI on this host.
#
# Usage:
#   scripts/install.sh [--check] [--prefix <dir>] [--link] [--with-guest]
#
# WHAT THIS DOES
#
# Builds the release host binary, puts it where your shell will find it, and proves the INSTALLED
# copy runs from outside this repository. That last step is the point of the script: a binary that
# works only from the directory it was built in is the mistake this exists to catch, and it is not
# a hypothetical -- `wvm vm display show` failed for a real user with "Command 'wvm' not found"
# because nothing here ever put it on a PATH.
#
# WHAT THIS DOES NOT DO
#
# It does not install Windows. That takes about an hour, it needs an ISO you supply, and it is
# scripts/prepare-install.sh's job. This prints the next step when it is done rather than implying
# the job is finished.
#
# THREE STATES, NOT TWO
#
# Every check reports ok / MISSING / COULD NOT RUN, and COULD NOT RUN is treated as a failure. A
# check that cannot run has not passed -- collapsing it into "ok" is how a machine that was never
# tested comes to look like one that works.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_NAME="wvm"
PREFIX=""
LINK=0
CHECK_ONLY=0
WITH_GUEST=0

FAILED=0
declare -a MISSING=()

# --- output ------------------------------------------------------------------------------------

if [ -t 1 ]; then
    B=$'\033[1m'; R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; N=$'\033[0m'
else
    B=""; R=""; G=""; Y=""; N=""
fi

ok()      { printf '  %s%-24s%s %s\n' "$G" "$1" "$N" "$2"; }
missing() { printf '  %s%-24s%s %s\n' "$R" "$1" "$N" "$2"; FAILED=1; MISSING+=("$1"); }
could_not_run() { printf '  %s%-24s%s %s\n' "$Y" "$1" "$N" "$2"; FAILED=1; MISSING+=("$1"); }
note()    { printf '  %s\n' "$1"; }

usage() {
    sed -n '3,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
}

# --- arguments ---------------------------------------------------------------------------------

while [ $# -gt 0 ]; do
    case "$1" in
        --check)      CHECK_ONLY=1; shift ;;
        --prefix)     PREFIX="$2"; shift 2 ;;
        --link)       LINK=1; shift ;;
        --with-guest) WITH_GUEST=1; shift ;;
        -h|--help)    usage ;;
        *)            echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

echo
printf '%sWVM install%s\n' "$B" "$N"
note "repository: $REPO_ROOT"
echo

# --- 1. what is present ------------------------------------------------------------------------

printf '%sPrerequisites%s\n' "$B" "$N"

if command -v cargo >/dev/null 2>&1; then
    ok "cargo" "$(cargo --version 2>/dev/null | head -1)"
else
    missing "cargo" "install Rust: https://rustup.rs"
fi

# Needed to RUN a guest, not to build the CLI. Missing these is reported and fails the run, because
# an install that cannot then run a VM has not delivered anything the user asked for.
if command -v qemu-system-x86_64 >/dev/null 2>&1; then
    ok "qemu-system-x86_64" "$(qemu-system-x86_64 --version 2>/dev/null | head -1)"
else
    missing "qemu-system-x86_64" "needed to run a guest: apt install qemu-system-x86"
fi

if [ -e /dev/kvm ]; then
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
        ok "/dev/kvm" "readable and writable"
    else
        # D-005: attempt the operation, never decide from metadata. Group membership is the wrong
        # test -- this rig has an ACL that grants access without the user being in the kvm group,
        # and a name-based check would report a false failure.
        could_not_run "/dev/kvm" "exists but is not readable+writable by $(id -un)"
    fi
else
    missing "/dev/kvm" "no hardware acceleration; enable VT-x/AMD-V in firmware"
fi

if [ "$WITH_GUEST" = "1" ]; then
    if rustup target list --installed 2>/dev/null | grep -qx 'x86_64-pc-windows-gnu'; then
        ok "windows target" "x86_64-pc-windows-gnu"
    else
        missing "windows target" "rustup target add x86_64-pc-windows-gnu"
    fi
    if command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
        ok "mingw-w64" "$(x86_64-w64-mingw32-gcc --version 2>/dev/null | head -1)"
    else
        missing "mingw-w64" "apt install gcc-mingw-w64-x86-64"
    fi
fi

echo

if [ "$FAILED" = "1" ]; then
    printf '%sCannot continue%s -- %d prerequisite(s) unmet: %s\n\n' \
        "$R" "$N" "${#MISSING[@]}" "${MISSING[*]}"
    note "Fix the above and re-run. Nothing has been installed or changed."
    exit 1
fi

if [ "$CHECK_ONLY" = "1" ]; then
    printf '%sAll prerequisites present%s (--check: nothing built or installed)\n\n' "$G" "$N"
    exit 0
fi

# --- 2. build ----------------------------------------------------------------------------------

printf '%sBuild%s\n' "$B" "$N"
BUILT="$REPO_ROOT/target/release/$BIN_NAME"
if cargo build --release -p wvm-host --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null 2>&1; then
    if [ -x "$BUILT" ]; then
        ok "host binary" "$(du -h "$BUILT" | cut -f1), built"
    else
        could_not_run "host binary" "cargo reported success but $BUILT is not executable"
        exit 1
    fi
else
    missing "host build" "cargo build failed -- run 'cargo build --release -p wvm-host' to see why"
    exit 1
fi

if [ "$WITH_GUEST" = "1" ]; then
    if cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest \
            --manifest-path "$REPO_ROOT/Cargo.toml" >/dev/null 2>&1; then
        ok "guest binary" "target/x86_64-pc-windows-gnu/release/wvm-guest.exe"
    else
        missing "guest build" "cross-compile failed; see the guest section of WVM.md"
        exit 1
    fi
fi
echo

# --- 3. where it goes --------------------------------------------------------------------------

# Explicit rather than magic: the choice is printed, along with why. A user-writable directory that
# is already on PATH is preferred because it needs no sudo.
if [ -n "$PREFIX" ]; then
    DEST_DIR="$PREFIX"
    WHY="given with --prefix"
elif [ -d "$HOME/.local/bin" ] && case ":$PATH:" in *":$HOME/.local/bin:"*) true ;; *) false ;; esac; then
    DEST_DIR="$HOME/.local/bin"
    WHY="already on your PATH, and needs no sudo"
elif [ -w /usr/local/bin ]; then
    DEST_DIR="/usr/local/bin"
    WHY="/usr/local/bin is writable"
else
    DEST_DIR="/usr/local/bin"
    WHY="the conventional location; this step will need sudo"
fi

printf '%sInstall%s\n' "$B" "$N"
note "destination: $DEST_DIR/  ($WHY)"
if [ ! -d "$DEST_DIR" ]; then
    mkdir -p "$DEST_DIR" 2>/dev/null || sudo -n install -d "$DEST_DIR"
fi

DEST="$DEST_DIR/$BIN_NAME"
if [ "$LINK" = "1" ]; then
    # Deliberate, and the trade is real: a symlink always runs the current build, which is what you
    # want while working on the repository; a copy can silently go stale after a rebuild. That
    # exact staleness bit this project once already.
    ln -sfn "$BUILT" "$DEST" || { missing "symlink" "could not link $DEST"; exit 1; }
    ok "installed (link)" "$DEST -> $BUILT"
else
    if install -m 0755 "$BUILT" "$DEST" 2>/dev/null || sudo -n install -m 0755 "$BUILT" "$DEST"; then
        ok "installed (copy)" "$DEST"
    else
        missing "install" "could not write $DEST -- pass --prefix ~/.local/bin instead"
        exit 1
    fi
fi
echo

# --- 4. prove the INSTALLED copy runs, from outside the repository ------------------------------

# Two separate claims, so two separate checks. `command -v` alone is not enough: it resolves
# whatever is first on PATH, so installing to a prefix that is NOT on PATH would verify some other
# wvm entirely and report success for a file this script never ran.
printf '%sVerify%s\n' "$B" "$N"

# 1. The file we just wrote. By full path, from a directory that is not this repository -- running
#    it from here would pass even if the install had never happened.
if OUT="$( cd /tmp && "$DEST" capabilities --json 2>&1 )"; then
    # `|| true` because grep exits 1 when it matches nothing, and under `set -o pipefail` that
    # aborts the whole script -- silently. That is exactly how this step first failed: a wrong
    # field name killed the run with no message whatsoever, which is indistinguishable from the
    # script deciding there was nothing to say.
    COUNT="$(printf '%s' "$OUT" | grep -o '"op"' | wc -l || true)"
    if [ "${COUNT:-0}" -gt 0 ]; then
        ok "installed binary runs" "$COUNT capabilities, from /tmp"
    else
        # Zero is a FAILURE, never a pass with a small number. It means the binary produced output
        # this script could not read, which is precisely what a broken or wrong build looks like.
        could_not_run "installed binary" "ran but reported no capabilities: $(printf '%s' "$OUT" | head -1)"
        exit 1
    fi
else
    could_not_run "installed binary" "failed: $(printf '%s' "$OUT" | head -1)"
    exit 1
fi

# 2. Whether a bare `wvm` actually reaches it. A different wvm winning is not a broken install, but
#    it does mean the command the docs tell people to type runs something else -- worth refusing
#    loudly now rather than leaving to be discovered later.
RESOLVED="$(command -v "$BIN_NAME" 2>/dev/null || true)"
if [ "$RESOLVED" = "$DEST" ]; then
    ok "on PATH" "$RESOLVED"
elif [ -z "$RESOLVED" ]; then
    could_not_run "on PATH" "$DEST_DIR is not on your PATH; add it, then re-run"
    exit 1
else
    could_not_run "on PATH" "a different wvm wins: $RESOLVED (installed $DEST)"
    exit 1
fi
echo

# --- 5. what is next ---------------------------------------------------------------------------

printf '%sInstalled. Two things remain, and this script did neither.%s\n' "$G" "$N"
note "1. Create the VM definition and disk:   scripts/prepare-install.sh --iso <tiny11.iso>"
note "2. Install Windows into it:             scripts/start-windows.sh --headless"
note ""
note "Neither is optional and neither is quick -- the Windows install is most of an hour."
note "Full walkthrough: the Quickstart in README.md."
echo
