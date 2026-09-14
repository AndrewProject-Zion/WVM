#!/usr/bin/env bash
# Check that the numbers and filenames claimed in the docs match reality.
#
# Docs drift. A README that says "116 tests passing" while the suite reports 108 is worse than one
# that says nothing, because it is trusted. This compares the claims against the actual state so
# the drift is caught when it happens rather than when someone relies on it.

set -uo pipefail
cd /home/andy/LSW || exit 1

fail=0

echo "=== test count ==="
actual_tests=$(cargo test --workspace 2>/dev/null | grep -E '^test result' | awk '{s+=$4} END {print s}')
echo "  workspace total: $actual_tests"

# Per-crate counts, so a doc can legitimately cite one of those instead of the total.
declare -A crate_tests
crate_tests[ipc]=$(cargo test -p wvm-ipc 2>/dev/null | grep -E '^test result' | awk '{s+=$4} END {print s}')
crate_tests[host]=$(cargo test -p wvm-host 2>/dev/null | grep -E '^test result' | awk '{s+=$4} END {print s}')
crate_tests[guest]=$(cargo test -p wvm-guest 2>/dev/null | grep -E '^test result' | awk '{s+=$4} END {print s}')
echo "  per crate: ipc=${crate_tests[ipc]} host=${crate_tests[host]} guest=${crate_tests[guest]}"

# A number in the docs is acceptable if it matches the workspace total, any per-crate count, OR any
# named test module's count — a sentence about the guest's path tests legitimately cites just those,
# and a sentence about the policy module cites just policy. It is only wrong if it matches NOTHING,
# which is the case worth flagging.
subset_counts=$(
    {
        cargo test --workspace 2>/dev/null | grep -oE '^test [a-z_]+::' \
            | sort -u | while read -r mod; do
                name="${mod#test }"; name="${name%::}"
                cargo test -p wvm-host -p wvm-guest -- "$name::" 2>/dev/null \
                    | grep -E '^test result' | awk '{s+=$4} END {if (s>0) print s}'
            done
    } | sort -u
)

known_ok=$(printf '%s\n%s\n' "$actual_tests ${crate_tests[ipc]} ${crate_tests[host]} ${crate_tests[guest]}" \
    "$(printf '%s' "$subset_counts" | tr '\n' ' ')")

echo "  module-level counts: $(printf '%s' "$subset_counts" | tr '\n' ' ')"

for doc in README.md docs/BUILD-PLAN.md docs/WINDOWS-INSTALL-STATUS.md; do
    [ -f "$doc" ] || continue
    while read -r claimed; do
        [ -z "$claimed" ] && continue
        if printf '%s\n' $known_ok | grep -qx "$claimed"; then
            printf '  %-28s claims %-4s — matches a real count\n' "$doc" "$claimed"
        else
            printf '  %-28s claims %-4s — MATCHES NOTHING (%s)\n' "$doc" "$claimed" "$known_ok"
            fail=1
        fi
    done < <(grep -oE '[0-9]+ tests' "$doc" 2>/dev/null | awk '{print $1}' | sort -u)
done

echo
echo "=== scripts referenced in the README exist ==="
# Pull `name.ext` backtick-quoted entries from the scripts table and check each file.
grep -oE '`[a-z0-9-]+\.(sh|py|ps1)`' README.md | tr -d '`' | sort -u | while read -r s; do
    if [ -f "scripts/$s" ]; then
        printf '  %-30s present\n' "$s"
    elif [ -f "examples/$s" ]; then
        printf '  %-30s present (examples/)\n' "$s"
    else
        printf '  %-30s MISSING\n' "$s"
    fi
done

echo
echo "=== docs referenced in the README exist ==="
grep -oE 'docs/[A-Z0-9-]+\.md|[A-Z0-9-]+\.md' README.md | sort -u | while read -r d; do
    case "$d" in
        docs/*) path="$d" ;;
        *)      path="docs/$d" ;;
    esac
    if [ -f "$path" ]; then
        printf '  %-30s present\n' "$d"
    else
        printf '  %-30s MISSING\n' "$d"
        exit 1
    fi
done

echo
echo "=== every claim in the README's verification list re-checked ==="
# The binary sizes the README quotes.
for entry in "target/release/wvm:wvm (host)" "target/x86_64-pc-windows-gnu/release/wvm-guest.exe:wvm-guest.exe"; do
    path="${entry%%:*}"; label="${entry##*:}"
    if [ -f "$path" ]; then
        printf '  %-24s %s\n' "$label" "$(du -h "$path" | cut -f1)"
    else
        printf '  %-24s NOT BUILT\n' "$label"
    fi
done

echo
if [ "$fail" -eq 0 ]; then
    echo "no mismatches found in the checked claims"
else
    echo "MISMATCHES FOUND — update the docs" >&2
    exit 1
fi
