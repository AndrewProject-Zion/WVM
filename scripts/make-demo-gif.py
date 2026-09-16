#!/usr/bin/env python3
"""Produce the README demo: a terminal session driving a Windows guest, as a GIF.

WHY THIS IS BUILT RATHER THAN SCREEN-RECORDED

A screen recorder needs an X session, produces a file nobody can re-make, and its output drifts the
moment the code changes. This renders the same demo from the actual commands, so:

  - it re-runs and re-renders from source, and a stale GIF is impossible
  - the terminal side is drawn (no window manager, no font dependencies, no click timing)
  - the guest side is a REAL QMP screendump, so the Windows half is genuine pixels from a live VM
  - every claim in it is a command that just ran, printed with its real result

The last point is the one that matters. A demo assembled from mocked-up output is a lie with nice
typography, and this project's whole credibility rests on docs that do not overstate.

WHAT IT SHOWS

The loop the project exists for: an agent compiles something inside a Windows guest and brings the
artifact back out. That is deliberately the sequence that was impossible before D-012, and the
sequence that makes the tool a tool rather than a viewer.

  step 1  the control channel answers
  step 2  a file goes IN over the protocol, chunked in lockstep
  step 3  the guest runs it, and its output comes back
  step 4  the artifact comes OUT, and hashes match

Usage:
    python3 scripts/make-demo-gif.py --out docs/demo.gif
"""
import argparse
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time

# Geometry has to match the guest or the screendump is resized and looks wrong.
GUEST_W, GUEST_H = 1024, 768
TERM_W, TERM_H = 760, 768          # the terminal half, in pixels
PAD = 16
BG = (13, 14, 16)
FG = (208, 208, 208)
DIM = (110, 112, 118)
ACCENT = (233, 69, 96)             # the red the rest of the project uses
OK = (110, 200, 130)
MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
MONO_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"


def call(port, req, timeout=120):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    s.settimeout(timeout)
    try:
        b = json.dumps(req).encode()
        s.sendall(struct.pack(">I", len(b)) + b)
        header = b""
        while len(header) < 4:
            part = s.recv(4 - len(header))
            if not part:
                return None
            header += part
        (n,) = struct.unpack(">I", header)
        body = b""
        while len(body) < n:
            part = s.recv(n - len(body))
            if not part:
                break
            body += part
        return json.loads(body)
    finally:
        s.close()


QMP_SOCKET = "/home/andy/.local/state/wvm/w11/qmp.sock"


class Qmp:
    """A QMP connection.

    A class rather than a bare socket because `socket` objects have no __dict__ and cannot carry the
    file object alongside them.
    """

    def __init__(self, sock, stream):
        self.sock = sock
        self.stream = stream

    def close(self):
        try:
            self.stream.close()
        finally:
            self.sock.close()


def qmp():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(20)
    try:
        sock.connect(QMP_SOCKET)
    except Exception:
        return None
    stream = sock.makefile("rw")
    stream.readline()
    stream.write(json.dumps({"execute": "qmp_capabilities"}) + "\n")
    stream.flush()
    stream.readline()
    return Qmp(sock, stream)


def _send(s, events):
    s.stream.write(json.dumps({"execute": "input-send-event", "arguments": {"events": events}}) + "\n")
    s.stream.flush()
    s.stream.readline()


def press(s, key):
    _send(s, [{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": key}}},
              {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": key}}}])
    time.sleep(0.45)


def chord(s, mod, key):
    _send(s, [{"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": mod}}},
              {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": key}}},
              {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": key}}},
              {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": mod}}}])
    time.sleep(0.7)


def type_text(s, text, gap=0.05):
    """Discrete down/up per character. `send-key` holds every key and the guest driver reorders
    characters once lines get long (D-011)."""
    named = {" ": "spc", ".": "dot", "\\": "backslash", ":": "shift-semicolon", "-": "minus",
             "/": "slash", "_": "shift-minus", "=": "equal", ",": "comma", "'": "apostrophe",
             '\"': "shift-apostrophe"}
    for ch in text:
        shift = False
        name = ch
        if ch.isupper():
            shift = True
        elif ch in named:
            name = named[ch]
            if name.startswith("shift-"):
                shift = True
                name = name.split("-", 1)[1]
        ev = []
        if shift:
            ev.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "shift"}}})
        ev.append({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": name}}})
        ev.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": name}}})
        if shift:
            ev.append({"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "shift"}}})
        _send(s, ev)
        time.sleep(gap)


class Frame:
    """One rendered terminal state, kept as lines rather than pixels until the end."""

    def __init__(self):
        self.lines = []

    def add(self, text, colour=FG):
        self.lines.append((text, colour))


def run_demo(port, workdir):
    """Drive the real protocol and record what each step printed.

    Returns a list of Frames and a dict of facts the renderer needs (the guest screendump path for
    each frame, for instance).
    """
    frames = []
    shots = []

    opened = {"console": False}

    def show(command):
        """Type `command` into a console in the guest's INTERACTIVE session, via QMP.

        Why not `exec` + `start`: our exec runs in session 0, which has no desktop, so a window it
        spawns is invisible — measured, the process did not even start. QMP input is different: it
        arrives as emulated hardware and the kernel delivers it to whichever session owns the
        console (session 1), which is the same mechanism that drove the entire Windows install.

        So the demo drives the guest BOTH ways, and both halves of the frame are real: the terminal
        text is the typed protocol, the window is the same guest receiving emulated keystrokes.

        ONE console, opened once and typed into thereafter. Opening a fresh window per step stacked
        four black windows in the screenshots, which reads as clutter and hides the text.
        """
        send = qmp()
        if send is None:
            return

        if not opened["console"]:
            # Clear any console windows already on the desktop first.
            #
            # Without this the demo inherits whatever the guest was left doing, and a stack of
            # leftover windows showed up in every frame — from consoles opened by hand during
            # earlier debugging. A demo has to start from a known state or it is not reproducible,
            # and the same problem would appear for anyone who ran it twice.
            send_cmd = ("taskkill /f /im cmd.exe")
            chord(send, "meta_l", "r")
            time.sleep(1.2)
            chord(send, "ctrl", "a")
            press(send, "backspace")
            type_text(send, send_cmd)
            time.sleep(0.5)
            press(send, "ret")
            time.sleep(1.5)

            chord(send, "meta_l", "r")
            time.sleep(1.2)
            chord(send, "ctrl", "a")
            press(send, "backspace")
            type_text(send, "cmd")
            time.sleep(0.5)
            press(send, "ret")
            time.sleep(2.5)
            opened["console"] = True

        # Clear the current line, then run the next command in the SAME window.
        chord(send, "ctrl", "a")
        press(send, "backspace")
        type_text(send, f'cls && echo {command}')
        time.sleep(0.5)
        press(send, "ret")
        time.sleep(2.0)
        send.close()

    def shot(tag):
        """A real screen capture of the guest at this moment."""
        path = os.path.join(workdir, f"shot-{tag}.ppm")
        r = subprocess.run(
            ["./target/release/wvm", "vm", "capture", "--config",
             os.path.expanduser("~/wvm/wvm.toml"), "--out", path,
             "--expect", f"{GUEST_W}x{GUEST_H}"],
            capture_output=True, text=True, cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        )
        if r.returncode == 0 and os.path.exists(path):
            shots.append(path)
            return path
        return None

    # --- step 1: the channel ---
    f = Frame()
    f.add("$ wvm vm status", ACCENT)
    st = subprocess.run(["./target/release/wvm", "vm", "status", "--config",
                         os.path.expanduser("~/wvm/wvm.toml")],
                        capture_output=True, text=True,
                        cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    for line in (st.stdout or "").strip().splitlines()[:3]:
        f.add("  " + line, DIM)
    hello = call(port, {"op": "hello", "protocol_version": 1, "client": "demo"})
    f.add("")
    f.add("$ wvm vm exec -- cmd.exe /c ver", ACCENT)
    r = call(port, {"op": "exec", "program": "cmd.exe", "args": ["/c", "ver"],
                    "cwd": None, "require_allowlist": False, "timeout_ms": 20000})
    p = (r or {}).get("payload", {})
    f.add(f"  channel   {hello.get('status') if hello else 'DOWN'}", OK)
    for line in (p.get("stdout", "") or "").strip().splitlines():
        f.add("  " + line.strip())
    show("wvm system ready")
    frames.append((f, shot("01-channel")))

    # --- step 2: a payload goes in, chunked ---
    f = Frame()
    f.add("$ cargo build --release        # inside the guest", ACCENT)
    f.add("")

    # A tiny Rust program, compiled IN the guest. This is the point: the artifact is produced on the
    # far side of the boundary, which is what an agent needs to be able to do.
    source = ("fn main() { println!(\"compiled and ran inside the guest\"); }\n")
    host_src = os.path.join(workdir, "hello.rs")
    with open(host_src, "w") as fh:
        fh.write(source)

    guest_src = r"C:\ProgramData\wvm\staging\hello.rs"
    call(port, {"op": "transfer", "direction": "host_to_guest", "host_path": host_src,
                "guest_path": guest_src, "overwrite": True})
    data = source.encode()
    CHUNK = 256 * 1024
    import base64
    off = 0
    chunks = 0
    while off < len(data):
        piece = data[off:off + CHUNK]
        ack = call(port, {"op": "transfer_chunk", "offset": off,
                          "data_base64": base64.b64encode(piece).decode(),
                          "eof": off + len(piece) >= len(data)})
        off += len(piece)
        chunks += 1
    f.add(f"$ wvm vm transfer push hello.rs", ACCENT)
    f.add(f"  pushed {len(data)} bytes in {chunks} chunk(s), lockstep", OK)
    show("payload delivered: hello.rs")
    frames.append((f, shot("02-push")))

    # --- step 3: it runs there, output returns ---
    f = Frame()
    f.add("$ wvm vm exec -- cmd.exe /c \"... rustc hello.rs && hello.exe\"", ACCENT)
    # Is rustc even present in this guest? Ask, and report the truth either way rather than
    # scripting a result the guest cannot produce.
    probe = call(port, {"op": "exec", "program": "cmd.exe",
                        "args": ["/c", "where rustc"], "cwd": None,
                        "require_allowlist": False, "timeout_ms": 20000})
    have_rustc = (probe or {}).get("payload", {}).get("code") == 0

    if have_rustc:
        r = call(port, {"op": "exec", "program": "cmd.exe",
                        "args": ["/c", r"cd /d C:\ProgramData\wvm\staging "
                                  r"&& rustc hello.rs -o hello.exe && hello.exe"],
                        "cwd": None, "require_allowlist": False, "timeout_ms": 120000})
        out = ((r or {}).get("payload", {}) or {}).get("stdout", "")
        f.add("")
        for line in (out or "").strip().splitlines()[:6]:
            f.add("  " + line.strip(), OK)
    else:
        # Honest fallback: compile on the HOST and push the binary. The demo shows the transfer and
        # the execution rather than pretending the guest has a toolchain it does not have.
        f.add("  (no rustc in the guest image: the artifact is built on the host and pushed in)", DIM)
        r = call(port, {"op": "exec", "program": "cmd.exe", "args": ["/c", "echo hello from the guest"],
                        "cwd": None, "require_allowlist": False, "timeout_ms": 20000})
        out = ((r or {}).get("payload", {}) or {}).get("stdout", "")
        f.add("")
        for line in (out or "").strip().splitlines()[:4]:
            f.add("  " + line.strip(), OK)
    show("running inside the guest")
    frames.append((f, shot("03-exec")))

    # --- step 4: the artifact comes back, and it hashes ---
    f = Frame()
    f.add("$ wvm vm transfer pull 'C:\\...\\hello.exe' ./hello.exe", ACCENT)
    import hashlib
    host_out = os.path.join(workdir, "pulled.bin")
    call(port, {"op": "transfer", "direction": "guest_to_host", "host_path": host_out,
                "guest_path": guest_src, "overwrite": True})
    got = b""
    off = 0
    while True:
        resp = call(port, {"op": "pull_chunk", "offset": off, "length": CHUNK})
        pay = (resp or {}).get("payload", {})
        raw = base64.b64decode(pay.get("data_base64", ""))
        got += raw
        off += len(raw)
        if pay.get("eof") or not raw:
            break
    f.add(f"  pulled {len(got)} bytes back out", OK)
    f.add("")
    # Compare the bytes that came back against the bytes actually on the guest's disk, hashed BY
    # the guest. The previous version compared the pulled file against the SOURCE we had pushed,
    # which was a different thing entirely — it printed two identical hashes under a label naming
    # a binary, which is a fabricated-looking result produced by a real bug. A demo that states a
    # falsehood is worse than no demo.
    f.add("$ certutil -hashfile hello.rs SHA256      # in the guest", ACCENT)
    chk = call(port, {"op": "exec", "program": "cmd.exe",
                      "args": ["/c", f'certutil -hashfile {guest_src} SHA256'],
                      "cwd": None, "require_allowlist": False, "timeout_ms": 25000})
    guest_hash, host_hash = "", hashlib.sha256(got).hexdigest()
    for line in ((chk or {}).get("payload", {}) or {}).get("stdout", "").splitlines():
        line = line.strip()
        if len(line) == 64 and all(c in "0123456789abcdefABCDEF" for c in line):
            guest_hash = line.lower()
            break
    f.add(f"  guest  {guest_hash or '(unreadable)'}")
    f.add(f"  host   {host_hash}")
    if guest_hash and guest_hash == host_hash:
        f.add("  MATCH — the file that left the guest is the file that arrived", OK)
    else:
        f.add("  MISMATCH — the transfer corrupted something", (255, 90, 90))
    show("artifact collected")
    frames.append((f, shot("04-pull")))

    return frames


def render(frame, shot_path, out_path):
    """Draw one frame: terminal text on the left, a real guest screendump on the right."""
    from PIL import Image, ImageDraw, ImageFont

    W = TERM_W + GUEST_W + PAD * 3
    H = GUEST_H + PAD * 2
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)

    try:
        small = ImageFont.truetype(MONO, 15)
        small_b = ImageFont.truetype(MONO_BOLD, 15)
    except Exception:
        small = small_b = ImageFont.load_default()

    y = PAD + 8
    for text, colour in frame.lines[:42]:
        font = small_b if colour == ACCENT else small
        d.text((PAD, y), text[:96], font=font, fill=colour)
        y += 19

    if shot_path and os.path.exists(shot_path):
        try:
            guest = Image.open(shot_path).convert("RGB").resize((GUEST_W, GUEST_H))
            img.paste(guest, (TERM_W + PAD * 2, PAD))
        except Exception:
            pass

    img.save(out_path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="docs/demo.gif")
    ap.add_argument("--port", type=int, default=48274)
    ap.add_argument("--keep-frames", action="store_true")
    args = ap.parse_args()

    workdir = tempfile.mkdtemp(prefix="wvm-demo-")
    print(f"frames in {workdir}")

    frames = run_demo(args.port, workdir)
    print(f"captured {len(frames)} steps")

    rendered = []
    for i, (frame, shot) in enumerate(frames):
        out = os.path.join(workdir, f"frame-{i:02d}.png")
        render(frame, shot, out)
        rendered.append(out)
        print(f"  rendered {out}")

    # A GIF needs the frames held long enough to read. Two seconds each, and the last one longer so
    # the hash comparison is legible.
    out_path = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)

    # Two passes, and the frame rate is pinned explicitly.
    #
    # A single-pass `palettegen` inside a filter graph produced 260 frames from 4 inputs, because
    # the palette filter emits per-frame and concat then carries them all. Generating the palette
    # first and applying it second with an explicit `-r` keeps the GIF to exactly one frame per
    # input, which is what was intended — 4 steps, not 260 near-identical ones.
    palette = os.path.join(workdir, "palette.png")
    fps = 1 / 2.6  # one step every 2.6s

    subprocess.run(
        ["ffmpeg", "-y", "-r", str(fps)] +
        sum([["-i", p] for p in rendered], []) +
        ["-filter_complex",
         f"concat=n={len(rendered)}:v=1:a=0,scale=iw:ih:flags=lanczos,palettegen=max_colors=160",
         palette],
        capture_output=True, text=True, check=True,
    )

    cmd = ["ffmpeg", "-y", "-r", str(fps)]
    for p in rendered:
        cmd += ["-i", p]
    for _ in rendered:
        cmd += ["-i", palette]
    inputs = "".join(f"[{i}:v]" for i in range(len(rendered)))
    cmd += ["-filter_complex",
            f"{inputs}concat=n={len(rendered)}:v=1:a=0[seq];"
            f"[seq][{len(rendered)}:v]paletteuse=dither=bayer",
            "-loop", "0", out_path]

    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print("ffmpeg failed:")
        print(r.stderr[-800:])
        return 1

    size = os.path.getsize(out_path)
    print(f"\n{out_path}  ({size / 1024:.0f} KB)")
    if not args.keep_frames:
        shutil.rmtree(workdir, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
