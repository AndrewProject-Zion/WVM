#!/usr/bin/env bash
#
# Run every verification probe against a live guest, and refuse to report success
# unless each one actually passed.
#
# WHY THIS EXISTS
#
# Every real bug in this project was found by one of these probes, and every false
# claim was caught by one. But they were not wired to anything: they ran when someone
# remembered to run them, which means a claim could ship on the strength of a probe
# that was never re-run after the code changed underneath it.
#
# A verification step that depends on remembering is not a verification step. This is
# the thing that makes it mechanical.
#
# THREE STATES, NOT TWO
#
# A probe exits 0 (passed), 1 (failed), or 2 (could not run — VM down, port closed).
# Collapsing "could not run" into "passed" is exactly the failure this project keeps
# hitting: an empty grep, a blank screenshot, a test that passes on broken code. So
# exit 2 is reported loudly and makes the whole run UNAVAILABLE rather than green.
#
# A green run means every probe ran and passed. Anything else is not green.
#
# Usage:
#   ./scripts/verify-all.sh                 # everything, against the default port
#   ./scripts/verify-all.sh --offline       # skip the probes needing a live guest
#   ./scripts/verify-all.sh --port 48274
#   ./scripts/verify-all.sh --list          # show what would run, and why
#
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

PORT=48274
OFFLINE=0
LIST=0

while [ $# -gt 0 ]; do
    case "$1" in
        --offline) OFFLINE=1; shift ;;
        --port)    PORT="$2"; shift 2 ;;
        --list)    LIST=1; shift ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Each entry: name | needs-guest | command
#
# `needs-guest` decides whether --offline skips it. Everything marked `no` is
# static and must pass on any machine that can build the project.
PROBES=(
    "build                        | no  | cargo build --workspace --quiet"
    "unit tests                   | no  | cargo test --workspace --quiet"
    "clippy, zero warnings        | no  | cargo clippy --workspace --all-targets --quiet"
    "windows cross-compile        | no  | cargo build --release --target x86_64-pc-windows-gnu -p wvm-guest --quiet"
    "docs match the build         | no  | ./scripts/check-docs.sh"
    "framing: 341 KB round trip   | no  | cargo test -p wvm-ipc a_transfer_sized --quiet"
    "guest hello                  | yes | python3 scripts/talk-to-guest.py hello --port $PORT"
    "framing: 5 sizes, live       | yes | python3 scripts/probe-frame-size.py"
    "carrier: base64 in one frame | yes | python3 scripts/probe-transfer-framing.py"
    "transfer: push and pull      | yes | python3 scripts/test-transfer-roundtrip.py --port $PORT"
    "exec: timeout kills the tree | yes | python3 scripts/test-timeout-tree-kill.py --port $PORT"
    "channel survives a wedge     | yes | python3 scripts/verify-request-deadline.py --port $PORT"
    "snapshot rolls the machine back | yes | python3 scripts/verify-snapshot-rollback.py --port $PORT"
)

if [ "$LIST" -eq 1 ]; then
    printf '%-30s %s\n' "PROBE" "NEEDS GUEST"
    printf '%-30s %s\n' "-----" "-----------"
    for entry in "${PROBES[@]}"; do
        IFS='|' read -r name needs _ <<< "$entry"
        printf '%-30s %s\n' "$(echo "$name" | xargs)" "$(echo "$needs" | xargs)"
    done
    exit 0
fi

if [ "$OFFLINE" -eq 0 ]; then
    # Fail fast with a clear reason rather than letting eleven probes each time out
    # against a closed port and produce a wall of noise.
    if ! python3 - "$PORT" <<'PY'
import socket, sys
try:
    s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
    s.close()
except Exception:
    raise SystemExit(1)
PY
    then
        echo "the guest control channel on 127.0.0.1:$PORT is not answering."
        echo "start the VM (./scripts/start-windows.sh --headless), or use --offline"
        echo "to run only the probes that need no guest."
        exit 2
    fi
fi

pass=0; fail=0; skip=0; unavailable=0
# Declared empty and appended to. Under `set -u`, referencing an array that was
# never appended to is an unbound-variable error — which crashed this script on its
# first real run, after the failures had been collected but before they were printed.
# The failure list is the most important output, so losing it to a crash was the
# worst possible time to die.
FAILED=()
NAMES=()
UNAVAIL=()

echo "=== verification, $(date '+%Y-%m-%d %H:%M:%S') ==="
[ "$OFFLINE" -eq 1 ] && echo "    (offline mode: probes needing a live guest are skipped)"
echo

for entry in "${PROBES[@]}"; do
    IFS='|' read -r name needs cmd <<< "$entry"
    name="$(echo "$name" | xargs)"
    needs="$(echo "$needs" | xargs)"
    cmd="$(echo "$cmd" | xargs)"

    if [ "$needs" = "yes" ] && [ "$OFFLINE" -eq 1 ]; then
        printf '  %-30s SKIP (needs a guest)\n' "$name"
        skip=$((skip + 1))
        continue
    fi

    start=$(date +%s)
    output=$(bash -c "$cmd" 2>&1)
    rc=$?
    secs=$(( $(date +%s) - start ))

    case $rc in
        0)
            printf '  %-30s PASS  (%ss)\n' "$name" "$secs"
            pass=$((pass + 1))
            ;;
        2)
            # Could not run. This is NOT a pass and must not be counted as one.
            printf '  %-30s UNAVAILABLE  (%ss)\n' "$name" "$secs"
            unavailable=$((unavailable + 1))
            NAMES+=("$name"); UNAVAIL+=("$output")
            ;;
        *)
            printf '  %-30s FAIL  (%ss)\n' "$name" "$secs"
            fail=$((fail + 1))
            NAMES+=("$name"); FAILED+=("$output")
            ;;
    esac
done

echo
echo "  passed $pass   failed $fail   unavailable $unavailable   skipped $skip"

if [ ${#FAILED[@]} -gt 0 ]; then
    echo
    echo "=== FAILURES ==="
    for i in "${!FAILED[@]}"; do
        echo
        echo "--- ${NAMES[$i]}"
        # Tail: the interesting part is usually the end, and a full probe log buries it.
        echo "${FAILED[$i]}" | tail -25 | sed 's/^/    /'
    done
fi

if [ ${#UNAVAIL[@]} -gt 0 ]; then
    echo
    echo "=== COULD NOT RUN (these are NOT passes) ==="
    for i in "${!UNAVAIL[@]}"; do
        echo
        echo "--- ${NAMES[$i]}"
        echo "${UNAVAIL[$i]}" | tail -12 | sed 's/^/    /'
    done
fi

echo
if [ "$fail" -gt 0 ]; then
    echo "RESULT: FAILED — $fail probe(s) rejected the build."
    exit 1
fi
if [ "$unavailable" -gt 0 ]; then
    echo "RESULT: INCOMPLETE — $unavailable probe(s) could not run, so nothing is verified."
    echo "        A probe that did not run is not a probe that passed."
    exit 2
fi
echo "RESULT: PASSED — every probe ran and passed."
exit 0
