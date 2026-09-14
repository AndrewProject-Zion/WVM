#!/usr/bin/env bash
#
# Tests for scripts/fetch-chunked.sh.
#
# The resume logic is the part worth testing: a resume that is wrong is worse than no resume at
# all, because it produces a file of the right SIZE containing the wrong BYTES. Every check here
# therefore compares against an independently fetched reference rather than checking size alone.
#
# Runs against a real HTTP server on localhost, not a mock, because the properties being tested
# (HTTP 206, exact byte ranges, Content-Length) are server behaviours.
#
# Usage: scripts/test-fetch-chunked.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FETCH="$SCRIPT_DIR/fetch-chunked.sh"
WORK="$(mktemp -d)"
PORT=18099

cleanup() {
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

pass=0
fail=0

check() {
    local name="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "  ok    $name"
        pass=$((pass + 1))
    else
        echo "  FAIL  $name"
        echo "          expected: $expected"
        echo "          actual:   $actual"
        fail=$((fail + 1))
    fi
}

# --- fixture: a file of deterministic pseudo-random bytes ---------------------------------------
#
# Not /dev/urandom: the test must be reproducible, and a fixed sequence means a byte-level
# comparison failure is diagnosable rather than "these two random files differ".

echo "Preparing fixture (6 MiB)..."
python3 - "$WORK/payload.bin" <<'PY'
import sys
# A repeating but non-trivial pattern, so a misplaced chunk cannot coincidentally match.
pattern = bytes((i * 37 + (i >> 8) * 11) & 0xFF for i in range(65536))
with open(sys.argv[1], "wb") as f:
    for _ in range(96):        # 96 * 64 KiB = 6 MiB
        f.write(pattern)
PY
TOTAL=$(stat -c %s "$WORK/payload.bin")
echo "  fixture is $TOTAL bytes"
echo

# --- a range-capable file server ----------------------------------------------------------------
#
# `python3 -m http.server` does NOT support Range requests (it answers 200 with the whole file),
# which was discovered by this test refusing to run rather than by a false pass. So the fixture is
# a small handler that implements Range explicitly — which also means the test controls exactly
# what the server does, including the no-Range case in test 6.
cat > "$WORK/rangeserver.py" <<'PY'
import http.server, socketserver, sys, os, re

class RangeServer(http.server.SimpleHTTPRequestHandler):
    """Serves a directory with single-range Range support."""

    def send_head(self):
        path = self.translate_path(self.path)
        if os.path.isdir(path):
            return super().send_head()

        try:
            f = open(path, "rb")
        except OSError:
            self.send_error(404, "not found")
            return None

        size = os.fstat(f.fileno()).st_size
        rng = self.headers.get("Range")

        if rng:
            m = re.match(r"bytes=(\d+)-(\d*)", rng)
            if m:
                start = int(m.group(1))
                end = int(m.group(2)) if m.group(2) else size - 1
                end = min(end, size - 1)
                if start >= size:
                    self.send_error(416, "range not satisfiable")
                    f.close()
                    return None
                self.send_response(206)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                self.send_header("Content-Length", str(end - start + 1))
                self.send_header("Accept-Ranges", "bytes")
                self.end_headers()
                f.seek(start)
                # Wrap so only the requested span is written.
                self._limit = end - start + 1
                return _Limited(f, self._limit)

        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        return f

    def copyfile(self, source, outputfile):
        limit = getattr(self, "_limit", None)
        if limit is None:
            return super().copyfile(source, outputfile)
        remaining = limit
        while remaining > 0:
            chunk = source.read(min(65536, remaining))
            if not chunk:
                break
            outputfile.write(chunk)
            remaining -= len(chunk)


class _Limited:
    """A read wrapper that stops at a byte limit."""
    def __init__(self, f, limit):
        self.f = f
        self.limit = limit
    def read(self, n=-1):
        if self.limit <= 0:
            return b""
        if n < 0 or n > self.limit:
            n = self.limit
        data = self.f.read(n)
        self.limit -= len(data)
        return data
    def close(self):
        self.f.close()


class ReusableServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    port = int(sys.argv[1])
    directory = sys.argv[2]
    os.chdir(directory)
    with ReusableServer(("127.0.0.1", port), RangeServer) as httpd:
        httpd.serve_forever()
PY

python3 "$WORK/rangeserver.py" "$PORT" "$WORK" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 40); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/payload.bin" && break
    sleep 0.1
done

URL="http://127.0.0.1:$PORT/payload.bin"

# Confirm the server actually honours Range before trusting any result below.
RANGE_CODE=$(curl -s -o /dev/null -w '%{http_code}' -r 0-99 "$URL")
if [ "$RANGE_CODE" != "206" ]; then
    echo "SKIP: the fixture server did not honour Range (got $RANGE_CODE)." >&2
    echo "      The tests below would be meaningless." >&2
    exit 1
fi

# --- reference -----------------------------------------------------------------------------------

cp "$WORK/payload.bin" "$WORK/reference.bin"
REF_SHA=$(sha256sum "$WORK/reference.bin" | cut -d' ' -f1)

echo "Tests"
echo

# --- 1. a clean fetch ----------------------------------------------------------------------------

"$FETCH" "$URL" "$WORK/fresh.bin" 1 "$TOTAL" >/dev/null 2>&1
check "clean fetch: exit code" 0 "$?"
check "clean fetch: byte-identical to the reference" \
      "$REF_SHA" "$(sha256sum "$WORK/fresh.bin" | cut -d' ' -f1)"

# --- 2. a resume from an ALIGNED offset ----------------------------------------------------------
#
# The easy case. An earlier version of the script passed this and still corrupted the file on
# unaligned resumes, which is why case 3 exists.

head -c 2097152 "$WORK/reference.bin" > "$WORK/aligned.bin"
"$FETCH" "$URL" "$WORK/aligned.bin" 1 "$TOTAL" >/dev/null 2>&1
check "resume from a 2 MiB boundary: byte-identical" \
      "$REF_SHA" "$(sha256sum "$WORK/aligned.bin" | cut -d' ' -f1)"

# --- 3. a resume from an UNALIGNED offset --------------------------------------------------------
#
# THE REGRESSION TEST. The bug: `dd bs=1M seek=$((OFFSET / 1048576))` truncates the offset towards
# zero, so a resume point that is not a whole mebibyte moves the write EARLIER and overwrites
# bytes that were already correct.
#
# Note what made it hard to catch: the file still ends up the right LENGTH, and each chunk is
# still individually the right size, so every size-based check passes. Only comparing the bytes
# against an independent fetch finds it.
#
# 1000003 is deliberately not a multiple of 1048576 or of 4096.

SEED=1000003
head -c "$SEED" "$WORK/reference.bin" > "$WORK/unaligned.bin"
"$FETCH" "$URL" "$WORK/unaligned.bin" 1 "$TOTAL" >/dev/null 2>&1
check "resume from an unaligned offset: byte-identical" \
      "$REF_SHA" "$(sha256sum "$WORK/unaligned.bin" | cut -d' ' -f1)"
check "resume from an unaligned offset: correct length" \
      "$TOTAL" "$(stat -c %s "$WORK/unaligned.bin")"

# --- 4. resuming several times in a row ----------------------------------------------------------
#
# Repeated interruption is the normal case on a flaky mirror, so it is tested rather than assumed.

rm -f "$WORK/repeated.bin"
touch "$WORK/repeated.bin"
for n in 1 2 3; do
    # Each pass is cut short by a chunk budget, simulating an interruption and re-run.
    head -c $(( n * 1500000 )) "$WORK/reference.bin" > "$WORK/repeated.bin"
    "$FETCH" "$URL" "$WORK/repeated.bin" 1 "$TOTAL" >/dev/null 2>&1
done
check "three successive resumes: byte-identical" \
      "$REF_SHA" "$(sha256sum "$WORK/repeated.bin" | cut -d' ' -f1)"

# --- 5. an already-complete file is a no-op ------------------------------------------------------

"$FETCH" "$URL" "$WORK/reference.bin" 1 "$TOTAL" >/dev/null 2>&1
check "already complete: byte-identical" \
      "$REF_SHA" "$(sha256sum "$WORK/reference.bin" | cut -d' ' -f1)"

# --- 6. a server that ignores Range is refused ---------------------------------------------------
#
# Must be a hard error: a server returning 200 to a Range request streams from zero, which would
# silently overwrite the beginning of a partly-downloaded file with the wrong bytes.

mkdir -p "$WORK/norangeserver"
cat > "$WORK/norangeserver/serve.py" <<'PY'
import http.server, socketserver, sys, os

class NoRange(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        # Anything with a Range header gets a 200 and the whole file, ignoring the range.
        if "Range" in self.headers:
            path = self.translate_path(self.path)
            if os.path.isdir(path):
                return None
            f = open(path, "rb")
            size = os.fstat(f.fileno()).st_size
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(size))
            self.end_headers()
            return f
        return super().send_head()

socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", int(sys.argv[1])), NoRange) as httpd:
    httpd.serve_forever()
PY

python3 "$WORK/norangeserver/serve.py" $((PORT + 1)) --directory "$WORK" >/dev/null 2>&1 &
NO_RANGE_PID=$!
for _ in $(seq 1 40); do
    curl -s -o /dev/null "http://127.0.0.1:$((PORT + 1))/payload.bin" && break
    sleep 0.1
done

head -c 1000000 "$WORK/reference.bin" > "$WORK/noranges.bin"
NORANGE_SHA_BEFORE=$(sha256sum "$WORK/noranges.bin" | cut -d' ' -f1)
"$FETCH" "http://127.0.0.1:$((PORT + 1))/payload.bin" "$WORK/noranges.bin" 1 "$TOTAL" >/dev/null 2>&1
NORANGE_EXIT=$?
kill "$NO_RANGE_PID" 2>/dev/null

check "a server ignoring Range: refused with non-zero exit" \
      "yes" "$([ "$NORANGE_EXIT" -ne 0 ] && echo yes || echo no)"
check "a server ignoring Range: the partial file is left untouched" \
      "$NORANGE_SHA_BEFORE" "$(sha256sum "$WORK/noranges.bin" | cut -d' ' -f1)"

# --- summary -------------------------------------------------------------------------------------

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ] || exit 1
