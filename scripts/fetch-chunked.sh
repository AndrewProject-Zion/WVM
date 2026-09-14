#!/usr/bin/env bash
#
# Fetch a large file in chunks with resume, verifying each chunk.
#
# Written because a single long connection to a throttling mirror keeps dying. The failure is not
# size or time — it is that the connection has a limited life, and one request trying to stream
# the whole file will outlive it. Chunked range requests sidestep that entirely: each request is
# short, and progress is durable because it is on disk between requests.
#
# Each chunk is verified before being accepted:
#   * the response is 206 (partial content), not 200 (the server ignoring Range)
#   * the byte count matches what was asked for, unless it is the final chunk
# A server that ignores Range would stream from zero and corrupt the tail — which is exactly the
# failure mode to guard against, so it is a hard error rather than a warning.
#
# Usage:
#   scripts/fetch-chunked.sh <url> <dest> [chunk_mib] [total_bytes]
#
# Example:
#   scripts/fetch-chunked.sh https://example.com/big.iso ~/wvm-images/big.iso 16 877373440

set -uo pipefail

URL="${1:?usage: $0 <url> <dest> [chunk_mib] [total_bytes]}"
DEST="${2:?usage: $0 <url> <dest> [chunk_mib] [total_bytes]}"
CHUNK_MIB="${3:-16}"
EXPECTED_TOTAL="${4:-}"

CHUNK_BYTES=$((CHUNK_MIB * 1024 * 1024))
MAX_ATTEMPTS_PER_CHUNK=5

mkdir -p "$(dirname "$DEST")"
touch "$DEST"

# --- discover the total size if not given ------------------------------------------------------

if [ -z "$EXPECTED_TOTAL" ]; then
    EXPECTED_TOTAL=$(curl -sIL "$URL" 2>/dev/null \
        | awk 'BEGIN{IGNORECASE=1} /^content-length:/ {gsub(/\r/,""); print $2}' \
        | tail -1)

    if [ -z "$EXPECTED_TOTAL" ]; then
        echo "ERROR: could not determine the size; pass it as the fourth argument." >&2
        exit 1
    fi
fi

echo "url       $URL"
echo "dest      $DEST"
echo "total     $EXPECTED_TOTAL bytes ($(( EXPECTED_TOTAL / 1024 / 1024 )) MiB)"
echo "chunk     ${CHUNK_MIB} MiB"
echo

# --- fetch -------------------------------------------------------------------------------------
#
# Resume from whatever is already on disk. An earlier version always started at zero, which meant
# a re-run silently re-fetched everything already present — burning the same bandwidth again on a
# host that is already slow. Starting from `stat -c %s` makes a re-run cheap, which matters
# because a re-run is the expected outcome of a flaky mirror.

OFFSET=$(stat -c %s "$DEST" 2>/dev/null || echo 0)

if [ "$OFFSET" -gt 0 ]; then
    if [ "$OFFSET" -ge "$EXPECTED_TOTAL" ]; then
        echo "already complete: $OFFSET bytes on disk"
    else
        echo "resuming from $OFFSET bytes ($(( OFFSET * 100 / EXPECTED_TOTAL ))%)"
    fi
    echo
fi

ATTEMPT=0

# A partial chunk left by an interrupted run would be re-fetched from OFFSET anyway, so clear it
# rather than let it confuse the next size check.
rm -f "$DEST.part"

while [ "$OFFSET" -lt "$EXPECTED_TOTAL" ]; do
    END=$(( OFFSET + CHUNK_BYTES - 1 ))
    if [ "$END" -ge "$EXPECTED_TOTAL" ]; then
        END=$(( EXPECTED_TOTAL - 1 ))
    fi
    WANT=$(( END - OFFSET + 1 ))

    printf '  [%3d%%] %d -> %d (%d bytes) ' \
        $(( OFFSET * 100 / EXPECTED_TOTAL )) "$OFFSET" "$END" "$WANT"

    # A short-lived request per chunk. No --limit-rate: the connection is bounded by the chunk
    # size instead, which is the real fix. Rate limiting was masking the problem, not solving it.
    CODE=$(curl -sL \
        --max-time 180 \
        --connect-timeout 20 \
        --retry 3 --retry-delay 2 --retry-connrefused \
        -r "${OFFSET}-${END}" \
        -o "$DEST.part" \
        -w '%{http_code}' \
        "$URL" 2>/dev/null)

    GOT=$(stat -c %s "$DEST.part" 2>/dev/null || echo 0)

    if [ "$CODE" != "206" ]; then
        echo "FAIL (http $CODE)"
        echo "    Expected 206 Partial Content. A 200 means the server ignored Range and" >&2
        echo "    streamed from zero, which would corrupt the file. Refusing to continue." >&2
        rm -f "$DEST.part"
        exit 1
    fi

    if [ "$GOT" -ne "$WANT" ]; then
        ATTEMPT=$(( ATTEMPT + 1 ))
        echo "short ($GOT of $WANT, attempt $ATTEMPT)"
        if [ "$ATTEMPT" -ge "$MAX_ATTEMPTS_PER_CHUNK" ]; then
            echo "Giving up on this chunk after $MAX_ATTEMPTS_PER_CHUNK attempts." >&2
            echo "Resume later: re-run this command; it continues from the current size." >&2
            rm -f "$DEST.part"
            exit 1
        fi
        sleep 2
        continue
    fi

    # Append the verified chunk.
    #
    # The offset MUST be in bytes, and `dd seek` with `bs=1` is the only correct way to say that.
    # An earlier version used `bs=1M seek=$((OFFSET / 1024 / 1024))`, which looks right and is
    # badly wrong: integer division moves the write point EARLIER whenever the resume offset is
    # not a whole-mebibyte multiple, so each chunk overwrote bytes that were already correct and
    # shifted the file. The size check still passed, because the file grew to the right length —
    # only a byte comparison against an independent fetch caught it.
    #
    # `bs=1M` with a byte offset is not expressible; `bs=1` is slower but exact, and correctness
    # here matters more than the copy speed of a few MiB.
    dd if="$DEST.part" of="$DEST" bs=1 seek="$OFFSET" conv=notrunc status=none
    rm -f "$DEST.part"

    OFFSET=$(( OFFSET + GOT ))
    ATTEMPT=0
    echo "ok"
done

# --- verify ------------------------------------------------------------------------------------

ACTUAL=$(stat -c %s "$DEST")
echo
if [ "$ACTUAL" -ne "$EXPECTED_TOTAL" ]; then
    echo "FAIL: got $ACTUAL bytes, expected $EXPECTED_TOTAL" >&2
    exit 1
fi

echo "done      $ACTUAL bytes"
echo "sha256    $(sha256sum "$DEST" | cut -d' ' -f1)"
echo
echo "The digest above is what to compare against the publisher's if they list one."
echo "A size match alone does not prove integrity."
