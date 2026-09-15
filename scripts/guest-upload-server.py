#!/usr/bin/env python3
"""Receive files POSTed from the Windows guest, so its output can be read on the host.

Why this exists
---------------

The blocker throughout M4 has not been the guest service — it has been reading the guest's output.
Text printed in an elevated Windows console cannot be captured once that window closes, and driving
a console through an emulated keyboard is unreliable past ~75 characters. Several failures were
diagnosed by inference when the actual message was sitting on a screen nobody could read.

The guest can already reach the host over HTTP (proven: it fetched a 505 KB binary). HTTP has
methods in both directions, so turning a one-way download channel into a two-way one needs only a
receiver on this side.

This is that receiver. It is deliberately not a general-purpose file server: it accepts PUT/POST
bodies into a single directory, with a size cap and a name that cannot escape that directory. It is
a diagnostic channel, not a transport.

Usage
-----

    python3 scripts/guest-upload-server.py [--port 8900] [--dir ~/.local/state/wvm/inbox]

From the guest:

    curl -T C:\\Users\\zion-win\\wvm\\out.txt http://10.0.2.2:8900/

Or pipe a command's output straight in:

    sc.exe query wvm-guest 2>&1 | curl -T - http://10.0.2.2:8900/wvm-guest-query.txt

Files land in the inbox directory named by their URL path, and every upload is logged with the time
and size so a missing upload is obvious rather than silent.
"""

from __future__ import annotations

import argparse
import datetime
import http.server
import os
import pathlib
import socketserver
import sys

MAX_UPLOAD_BYTES = 8 * 1024 * 1024


class InboxHandler(http.server.BaseHTTPRequestHandler):
    """Accept an upload body and write it to the inbox directory."""

    # Set by the server before it starts handling.
    inbox: pathlib.Path

    def _safe_name(self, raw_path: str) -> str | None:
        """Turn a URL path into a filename that cannot escape the inbox.

        Only the final path component is used, and it is checked for the two ways a name can climb
        out of a directory: a path separator, and a `..` component. A request for `/../../etc/passwd`
        must land as `passwd` in the inbox, or be refused.
        """
        # Strip the query string, then take the last path segment.
        path = raw_path.split("?", 1)[0]
        name = path.rstrip("/").rsplit("/", 1)[-1]

        if not name:
            return None

        # A name that is only dots is a directory traversal attempt in disguise.
        if set(name) <= {"."}:
            return None

        # No separators, and no parent-directory components once split.
        if "/" in name or "\\" in name:
            return None
        if ".." in pathlib.PurePosixPath(name).parts:
            return None

        # A Windows-y name is fine, but a path is not: reject anything that looks like one.
        if name.count(":") > 1:
            return None

        return name

    def _store(self, body: bytes, name: str) -> None:
        self.inbox.mkdir(parents=True, exist_ok=True)
        target = self.inbox / name
        target.write_bytes(body)

        stamp = datetime.datetime.now().strftime("%H:%M:%S")
        size = len(body)
        preview = body[:400].decode("utf-8", errors="replace").replace("\r\n", "\n")
        if size > 400:
            preview += f"\n... (+{size - 400} more bytes; full text in the file)"

        print(f"[{stamp}] received {name} ({size} bytes) -> {target}")
        if preview.strip():
            print("  ---8<---")
            for line in preview.splitlines():
                print(f"  {line}")
            print("  ---8<---")
        sys.stdout.flush()

    def _read_body(self) -> bytes | None:
        """Read the request body, handling both chunked and content-length framing.

        `curl -T -` does NOT send a Content-Length: it uses chunked transfer-encoding, where the
        body is wrapped in hex-length lines and terminated by a zero-length chunk. Reading with
        `rfile.read()` returns that framing verbatim, so the stored file began with `14\\r\\n` and
        ended with `0\\r\\n\\r\\n` — a text file that looks corrupt for no visible reason.

        Returns None if the body is over the cap (the caller should respond 413).
        """
        encoding = (self.headers.get("Transfer-Encoding") or "").lower()

        if "chunked" in encoding:
            body = bytearray()
            while True:
                size_line = self.rfile.readline(64).strip()
                if not size_line:
                    break
                # A chunk size may carry extensions after a semicolon; ignore them.
                size_hex = size_line.split(b";", 1)[0].strip()
                try:
                    size = int(size_hex, 16)
                except ValueError:
                    # Malformed framing: stop rather than guess at the remainder.
                    break
                if size == 0:
                    # Consume the trailing CRLF after the final chunk.
                    self.rfile.readline(2)
                    break
                if len(body) + size > MAX_UPLOAD_BYTES:
                    return None
                body += self.rfile.read(size)
                # Each chunk is followed by its own CRLF.
                self.rfile.readline(2)
            return bytes(body)

        length_header = self.headers.get("Content-Length")
        if length_header and length_header.isdigit():
            declared = int(length_header)
            if declared > MAX_UPLOAD_BYTES:
                return None
            return self.rfile.read(declared)

        # No framing information at all: nothing to read safely, so treat it as an empty body
        # rather than blocking on a read that may never complete.
        return b""

    def do_PUT(self):  # noqa: N802 - the name is fixed by BaseHTTPRequestHandler
        # A path that does not reduce to a plain filename is REFUSED, not silently rewritten.
        # Rewriting a traversal attempt to a safe name in the inbox is tempting but wrong: it
        # hides an attack behind a successful response, and the caller has no way to know their
        # path was not honoured. An explicit refusal is both honest and actionable.
        raw = self.path.split("?", 1)[0]
        if raw.startswith("/../../") or ".." in raw or "\\" in raw:
            self.send_error(400, "path must be a plain filename with no traversal")
            return

        name = self._safe_name(self.path)
        if name is None:
            self.send_error(400, "path must be a plain filename")
            return

        body = self._read_body()
        if body is None:
            self.send_error(413, f"body larger than {MAX_UPLOAD_BYTES} bytes")
            return

        self._store(body, name)

        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def do_POST(self):  # noqa: N802
        self.do_PUT()

    def do_GET(self):  # noqa: N802
        """Report what is in the inbox, so a client can confirm the channel is alive."""
        files = sorted(p.name for p in self.inbox.glob("*")) if self.inbox.exists() else []
        listing = "\n".join(files) + ("\n" if files else "")
        payload = (
            "wvm guest upload inbox\n"
            f"directory: {self.inbox}\n"
            f"files: {len(files)}\n"
            f"{listing}"
        ).encode()

        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, format, *args):  # noqa: A002 - name fixed by the base class
        """Quiet the default per-request logging; _store prints what matters.

        The default handler logs a line per request to stderr, which would interleave with the
        useful output and make the interesting part hard to spot.
        """
        return


class Server(socketserver.ThreadingTCPServer):
    # Allow a quick restart without waiting for the TIME_WAIT socket to clear.
    allow_reuse_address = True
    daemon_threads = True


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8900)
    parser.add_argument(
        "--dir",
        default=os.path.expanduser("~/.local/state/wvm/inbox"),
        help="where uploads are written",
    )
    parser.add_argument("--bind", default="0.0.0.0")
    args = parser.parse_args(argv[1:])

    inbox = pathlib.Path(args.dir).expanduser().resolve()
    inbox.mkdir(parents=True, exist_ok=True)

    InboxHandler.inbox = inbox

    with Server((args.bind, args.port), InboxHandler) as httpd:
        print(f"wvm guest upload inbox")
        print(f"  listening  {args.bind}:{args.port}")
        print(f"  writing to {inbox}")
        print()
        print("  from the guest:")
        print(f"    curl -T C:\\path\\to\\file.txt http://10.0.2.2:{args.port}/")
        print(f"    some-command 2>&1 | curl -T - http://10.0.2.2:{args.port}/output.txt")
        print()
        print("  confirm the channel from the host:")
        print(f"    curl http://127.0.0.1:{args.port}/")
        print()
        sys.stdout.flush()
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\nstopped")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
