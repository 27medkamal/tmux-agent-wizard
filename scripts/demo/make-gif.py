#!/usr/bin/env python3
"""Makes the README's GIF (docs/assets/demo.gif) from the storyboard scripts/demo/story.txt.

  python3 scripts/demo/make-gif.py tape       write dist/demo.tape and dist/demo-captions.json, nothing else
  python3 scripts/demo/make-gif.py captions   draw the captions and the toast's highlight over dist/frames/, write the GIF
  python3 scripts/demo/make-gif.py            tape, then vhs on it, then captions

Run from the repository root with a built taw (TAW_DEMO_BIN), vhs, ffmpeg, ImageMagick (magick), the JetBrainsMono
Nerd Font, and a tmux that is first on the PATH (Homebrew's, 3.8, as the other recordings). The world is start.sh's
with TAW_DEMO_STORY set. The captions are set in Inter (SIL Open Font License; put Inter-Medium.ttf and
Inter-SemiBold.ttf in dist/fonts/, or name a folder holding them in TAW_CAPTION_FONTS); without it they fall back to
Helvetica Neue, whose O and 0 differ as well.

The GIF is recorded at about twice the size the README shows it (830 to 880 px wide there, 1736 here), so nothing is
scaled afterwards. vhs writes every frame as a PNG (dist/frames/), so no video codec smears the coloured text; ffmpeg makes
one 256-colour palette from the whole video and writes the GIF with no dithering, which would speckle the text.

How the captions stay in step. The tape is generated from the storyboard, and nothing after its Show waits for
the screen: every key is followed by a fixed Sleep (the step's hold), so the recording is a clock the tape owns.
This script adds up every pause the tape makes (each key and each typed character costs the tape's TypingSpeed,
each Sleep its length) and writes the time of each step's key into dist/demo-captions.json. Each caption is a
floating pill drawn once as a transparent PNG (magick) and laid over the frames from its step's key to the next
step's (ffmpeg), fading in and out. Waiting happens only before the Show (the world, the keeper app and the popup
are there before the first frame), where a slow machine costs nothing the viewer sees. Re-recording prints the
recording's length next to the length the storyboard adds up to, and a difference is a sign the machine was too
slow to keep the clock: look at the frames before trusting the captions.

The captions sit in a band under the terminal, the pill centred in it, the same place for every step, so a caption
never covers the terminal and never moves.

The highlights. Every step outlines the part of the screen it is about in the toast's yellow (a rounded #f9e2af
outline with a faint glow; nothing is dimmed). Where each outline goes is read from the screen itself: the tape
attaches its tmux client through `script -r`, which keeps every byte tmux drew with its time, and this script plays
those bytes back into a pane of a private tmux server of the same size (taw-demo-replay), a few times a second of
each step, and reads the pane's text there (dist/captures/). Each step's target (see TARGETS) is found in that text,
and its cells become the outline's pixels, so the outline follows the layout. The toast's time comes from the
frames (the status line turns yellow), and the same highlight goes on docs/assets/toast.png.
"""
import glob
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import time
import unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
DIST = os.path.join(REPO, "dist")
TAPE = os.path.join(DIST, "demo.tape")
FRAMES = os.path.join(DIST, "frames")
CAPS = os.path.join(DIST, "demo-captions.json")
SIZE_FILE = os.path.join(DIST, "term-size.txt")
WORK = os.path.join(DIST, "caption-work")
GIF = os.path.join(REPO, "docs", "assets", "demo.gif")
TOAST_PNG = os.path.join(REPO, "docs", "assets", "toast.png")
REC = os.path.join(DIST, "client.rec")  # the tmux client's output with its times (script -r)
CAPDIR = os.path.join(DIST, "captures")
REPLAY = "taw-demo-replay"  # the private tmux server the client's output is played back into

TYPING = 0.05  # seconds a key or a typed character costs in the tape (Set TypingSpeed)
# The terminal: about twice the size the README shows it, 110 columns of 15 px cells (JetBrainsMono at 24 px) by
# about 34 rows, wide enough for taw to draw the preview beside the tree (100 columns or more), narrow enough that
# the text stays readable at the 830 to 880 px GitHub draws the README GIF at (every column more makes it smaller).
WIDTH, HEIGHT, FONT_SIZE, PADDING = 1736, 1124, 24, 24
# The caption band under the terminal: the GIF is WIDTH by HEIGHT + BAND, the pill centred in the band.
BAND = 104
FPS = 15  # vhs records at this rate, and the GIF keeps it (lower it, never the size, if the GIF passes 8 MB)
FONT = "JetBrainsMono Nerd Font"  # not the Mono variant: its icons are drawn at full size
# Catppuccin Mocha, on its darkest base (crust, #11111b) instead of #1e1e2e.
THEME = {
    "name": "Catppuccin Mocha Crust", "background": "#11111b", "foreground": "#cdd6f4", "cursor": "#f5e0dc",
    "selection": "#585b70", "black": "#45475a", "red": "#f38ba8", "green": "#a6e3a1", "yellow": "#f9e2af",
    "blue": "#89b4fa", "magenta": "#f5c2e7", "cyan": "#94e2d5", "white": "#bac2de", "brightBlack": "#585b70",
    "brightRed": "#f38ba8", "brightGreen": "#a6e3a1", "brightYellow": "#f9e2af", "brightBlue": "#89b4fa",
    "brightMagenta": "#f5c2e7", "brightCyan": "#94e2d5", "brightWhite": "#a6adc8",
}
# The caption pill (full size): Catppuccin's mantle at 90%, a surface1 hairline, keycaps in surface0.
PILL_H, PILL_PAD, PILL_GAP = 66, 22, 18
KEY_H, KEY_PAD, KEY_GAP, KEY_PT, WORD_PT = 44, 14, 10, 25, 30
FADE = 0.15
MAGICK = shutil.which("magick") or "/opt/homebrew/bin/magick"


def tape_settings():
    """The vhs settings the GIF and the stills share (stills.tape repeats them)."""
    return [
        'Set Shell "bash"',
        'Set FontFamily "%s"' % FONT,
        "Set FontSize %d" % FONT_SIZE,
        "Set Width %d" % WIDTH,
        "Set Height %d" % HEIGHT,
        "Set Padding %d" % PADDING,
        "Set Framerate %d" % FPS,
        "Set Theme %s" % json.dumps(THEME),
        "Set TypingSpeed %dms" % round(TYPING * 1000),
        "Set CursorBlink false",
    ]


def steps():
    out = []
    for raw in open(os.path.join(HERE, "story.txt"), encoding="utf-8"):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = [p.strip() for p in line.split("|")]
        while len(parts) < 6:
            parts.append("")
        keys, key, words, hold, shot, target = parts[:6]
        out.append(
            {
                "keys": [k.strip() for k in keys.split(";") if k.strip()],
                "key": key,
                "words": words,
                "hold": float(hold),
                "shot": shot,
                "target": target,
            }
        )
    return out


def vhs_key(cmd):
    """One storyboard key as a vhs command, with the seconds it costs."""
    if cmd.startswith("Type "):
        text = cmd[5:]
        return 'Type "%s"' % text, TYPING * len(text)
    return cmd, TYPING


# The stills are taken from the frames themselves (see write_gif), so they match the GIF pixel for pixel.
STILLS = {"popup": [os.path.join(REPO, "docs", "assets", "popup.png"), os.path.join(DIST, "popup-frame.png")]}


def write_tape():
    os.makedirs(DIST, exist_ok=True)
    root = '${TAW_DEMO_ROOT:-/tmp/taw-demo}'
    sock = '${TAW_DEMO_SOCKET:-taw-demo}'
    lines = [
        "# Generated by scripts/demo/make-gif.py from scripts/demo/story.txt: edit the storyboard, not this file.",
        "Output dist/frames/",
        "",
        *tape_settings(),
        "",
        "# Before the first frame, hidden: the world, the keeper app that lets the popup list the other machines,",
        "# and the popup itself, opened from api's Claude Code.",
        "Hide",
        'Type "stty size > dist/term-size.txt; HISTFILE=/dev/null TAW_DEMO_STORY=1 TAW_DEMO_ROOT=%s bash scripts/demo/start.sh --detach && echo world-$((6*7))-ready"' % root,
        "Enter",
        "Wait+Screen@90s /world-42-ready/",
        "# Every watcher has read its pane, and OpenCode's hook has reported, before the recording starts.",
        "Sleep 9s",
        'Type "source %s/env.sh && clear"' % root,
        "Enter",
        "Sleep 0.5s",
        "# taw app outside tmux keeps the machines for the popup. It runs in a tmux server of its own, hidden,",
        "# in a pane with no TMUX, so it is the app and not the popup.",
        'Type "tmux -L %s-keeper new-session -d -x 120 -y 30 env -u TMUX -u TMUX_PANE taw app && echo keeper-$((6*7))-up"' % sock,
        "Enter",
        "Wait+Screen@20s /keeper-42-up/",
        "Sleep 8s",
        "# The client runs under script -r, which keeps what tmux draws with its times: the highlights are placed",
        "# from that (see replay).",
        'Type "clear && rm -f dist/client.rec && script -q -r dist/client.rec tmux -L %s attach -t api"' % sock,
        "Enter",
        "Sleep 2s",
        "Ctrl+B",
        'Type "A"',
        "Wait+Screen@30s /Enter go/",
        "# The other machines are listed, with their agents, before the recording starts.",
        "Wait+Screen@60s /not answering/",
        "Sleep 2s",
        "Show",
        "",
    ]
    t = 0.0
    caps = []
    for i, st in enumerate(steps()):
        lines.append("# step %d: %s %s" % (i + 1, st["key"], st["words"]))
        first = t  # the caption starts as the step's first key is pressed
        for cmd in st["keys"]:
            c, cost = vhs_key(cmd)
            lines.append(c)
            t += cost
        hold = st["hold"]
        lines.append("Sleep %.2fs" % hold)
        name, _, at = st["shot"].partition("@")
        shot_at = round(t + float(at or 0), 3)
        t += hold
        caps.append({"key": st["key"], "words": st["words"], "target": st["target"], "shot": name,
                     "shot_at": shot_at, "keys": st["keys"], "start": round(first, 3), "end": round(t, 3)})
        lines.append("")
    lines += [
        "# The tape's end: the three servers are ended, and it waits until none answers.",
        "Hide",
        "Escape",
        "Sleep 0.5s",
        "Ctrl+B",
        'Type "d"',
        "Sleep 1s",
        'Type "for s in %s-keeper %s-home %s; do tmux -L $s kill-server; done; sleep 2; tmux -L %s ls 2>&1 | head -1; echo servers-gone-$((6*7))"' % (sock, sock, sock, sock),
        "Enter",
        "Wait+Screen@30s /servers-gone-42/",
        "",
    ]
    with open(TAPE, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    json.dump({"total": round(t, 3), "captions": caps}, open(CAPS, "w"), indent=1)
    print("storyboard adds up to %.1f s (%d steps)" % (t, len(caps)))


# ---- Captions ----
def fonts():
    for d in (os.environ.get("TAW_CAPTION_FONTS", ""), os.path.join(DIST, "fonts")):
        if d and os.path.exists(os.path.join(d, "Inter-Medium.ttf")) and os.path.exists(os.path.join(d, "Inter-SemiBold.ttf")):
            return os.path.join(d, "Inter-Medium.ttf"), os.path.join(d, "Inter-SemiBold.ttf")
    print("Inter is not in dist/fonts: the captions fall back to Helvetica Neue")
    return "Helvetica-Neue-Medium", "Helvetica-Neue-Bold"


def label(font, pt, colour, text, out):
    subprocess.run([MAGICK, "-background", "none", "-fill", colour, "-font", font, "-pointsize", str(pt),
                    "label:" + text, "-trim", "+repage", out], check=True)
    w, h = subprocess.run([MAGICK, "identify", "-format", "%w %h", out], capture_output=True, text=True,
                          check=True).stdout.split()
    return int(w), int(h)


def keys_of(key):
    """`prefix + A` is two keys, a prefix key and an A key; anything else is one key."""
    return [k.strip() for k in key.split("+")] if key else []


def draw_pill(i, key, words, out):
    """One caption as a transparent PNG: a soft shadow, the rounded pill, its keycaps, then the words."""
    medium, semibold = fonts()
    parts = []  # (kind, png, w, h)
    for j, k in enumerate(keys_of(key)):
        png = os.path.join(WORK, "k%02d-%d.png" % (i, j))
        w, h = label(semibold, KEY_PT, "#cdd6f4", k, png)
        parts.append(("key", png, w, h))
    if words:
        png = os.path.join(WORK, "w%02d.png" % i)
        w, h = label(medium, WORD_PT, "#cdd6f4", words, png)
        parts.append(("words", png, w, h))
    inner = 0
    for n, (kind, _png, w, _h) in enumerate(parts):
        if n:
            inner += PILL_GAP if kind == "words" else KEY_GAP
        inner += max(w + 2 * KEY_PAD, KEY_H) if kind == "key" else w
    pill_w = inner + 2 * PILL_PAD
    margin = 28  # room for the shadow
    cw, ch = pill_w + 2 * margin, PILL_H + 2 * margin
    x0, y0, x1, y1 = margin, margin, margin + pill_w - 1, margin + PILL_H - 1
    r = PILL_H // 2
    cmd = [MAGICK, "-size", "%dx%d" % (cw, ch), "xc:none",
           "(", "-size", "%dx%d" % (cw, ch), "xc:none", "-fill", "rgba(0,0,0,0.6)",
           "-draw", "roundrectangle %d,%d,%d,%d,%d,%d" % (x0, y0 + 8, x1, y1 + 8, r, r), "-blur", "0x12", ")",
           "-composite",
           "-fill", "rgba(24,24,37,0.92)", "-stroke", "#45475a", "-strokewidth", "2",
           "-draw", "roundrectangle %d,%d,%d,%d,%d,%d" % (x0, y0, x1, y1, r, r), "-stroke", "none"]
    x = x0 + PILL_PAD
    ky0 = y0 + (PILL_H - KEY_H) // 2
    for n, (kind, png, w, h) in enumerate(parts):
        if n:
            x += PILL_GAP if kind == "words" else KEY_GAP
        if kind == "key":
            kw = max(w + 2 * KEY_PAD, KEY_H)
            # A keycap: a darker lip under a lighter face, so it reads as a key at half size too.
            cmd += ["-fill", "#11111b", "-draw", "roundrectangle %d,%d,%d,%d,9,9" % (x, ky0 + 3, x + kw - 1, ky0 + KEY_H + 2),
                    "-fill", "#313244", "-stroke", "#585b70", "-strokewidth", "1.5",
                    "-draw", "roundrectangle %d,%d,%d,%d,9,9" % (x, ky0, x + kw - 1, ky0 + KEY_H - 1), "-stroke", "none",
                    png, "-geometry", "+%d+%d" % (x + (kw - w) // 2, ky0 + (KEY_H - h) // 2 - 1), "-composite"]
            x += kw
        else:
            cmd += [png, "-geometry", "+%d+%d" % (x, y0 + (PILL_H - h) // 2), "-composite"]
            x += w
    cmd += [out]
    subprocess.run(cmd, check=True)
    return cw, ch, margin


def grid():
    """The terminal's grid and its canvas in the raw frames: vhs writes the text layer (frame-text-*) and the cursor
    layer (frame-cursor-*) of the terminal alone, at one pixel per pixel; the GIF centres that canvas in the
    terminal's part of the frame on the theme's background, so the margins are even and nothing is scaled.
    Returns rows, cols, cell width, cell height, and the canvas's left and top offsets in the GIF."""
    rows, cols = [int(v) for v in open(SIZE_FILE).read().split()]
    first = sorted(glob.glob(os.path.join(FRAMES, "frame-text-*.png")))[0]
    cw, ch = [int(v) for v in subprocess.run([MAGICK, "identify", "-format", "%w %h", first], capture_output=True,
                                             text=True, check=True).stdout.split()]
    return rows, cols, cw // cols, ch // rows, (WIDTH - cw) // 2, (HEIGHT - ch) // 2


def compose(k, out):
    """Raw frame k (from 0) as the GIF's terminal shows it: the cursor over the text, centred on the background."""
    subprocess.run([MAGICK, os.path.join(FRAMES, "frame-text-%05d.png" % (k + 1)),
                    os.path.join(FRAMES, "frame-cursor-%05d.png" % (k + 1)), "-composite", "-background", "#11111b",
                    "-gravity", "center", "-extent", "%dx%d" % (WIDTH, HEIGHT), "-strip", out], check=True)


def yellowness(png, rows, cell_h):
    """How much of the status line's left half is the toast's yellow (#f9e2af) in one raw frame, from 0 to 1."""
    y = (rows - 1) * cell_h
    out = subprocess.run([MAGICK, png, "-crop", "%dx%d+0+%d" % (WIDTH // 2, cell_h, y), "+repage",
                          "-fuzz", "18%", "-fill", "white", "-opaque", "#f9e2af", "-fill", "black", "+opaque", "white",
                          "-format", "%[fx:mean]", "info:"], capture_output=True, text=True, check=True).stdout
    return float(out)


# ---- The screen as text: the client's output played back into a private tmux server ----
KEY_BYTES = {"Tab": b"\t", "Enter": b"\r", "Escape": b"\x1b", "Ctrl+B": b"\x02", "Ctrl+Y": b"\x19"}


def key_bytes(cmd):
    return cmd[5:].encode() if cmd.startswith("Type ") else KEY_BYTES[cmd]


def records():
    """script -r's records: (seconds, direction, bytes), direction "i" for a key, "o" for what tmux drew."""
    data, i, out = open(REC, "rb").read(), 0, []
    while i + 24 <= len(data):
        n, sec, usec, d = struct.unpack_from("<QQII", data, i)
        i += 24
        out.append((sec + usec / 1e6, chr(d), data[i:i + n]))
        i += n
    return out


def anchors(recs, caps, scale):
    """For each step, the recording's clock minus the video's at its first key, so a moment of the video is found
    in script's record: the keys the client got are matched, in order, to the storyboard's."""
    keys = [(t, bytes([b])) for t, d, data in recs if d == "i" for b in data]
    pos = 0
    # The popup was opened, hidden, before the recording: prefix + A.
    while pos < len(keys) and keys[pos][1] != b"A":
        pos += 1
    pos += 1
    offs = []
    for c in caps:
        want = b"".join(key_bytes(k) for k in c["keys"])
        if not want:
            offs.append(None)
            continue
        while pos < len(keys) and keys[pos][1] != want[:1]:
            pos += 1
        if pos >= len(keys):
            sys.exit("the client's record is missing the keys of step %r" % c["words"])
        offs.append(keys[pos][0] - c["start"] * scale)
        pos += len(want)
    # The opening step pressed nothing: it shares the next step's clock.
    for i in range(len(offs) - 2, -1, -1):
        if offs[i] is None:
            offs[i] = offs[i + 1]
    return offs


def cells(line):
    """One row of capture-pane -e as cells: (character, background, reverse); a wide character takes two."""
    out, bg, rev = [], None, False
    for m in re.finditer(r"\x1b\[([0-9;:]*)m|([^\x1b])", line):
        if m.group(2) is not None:
            ch = m.group(2)
            out.append((ch, bg, rev))
            if unicodedata.east_asian_width(ch) in ("W", "F"):
                out.append(("", bg, rev))
            continue
        ps = [p for p in re.split("[;:]", m.group(1))] or ["0"]
        j = 0
        while j < len(ps):
            p = int(ps[j] or 0)
            if p == 0:
                bg, rev = None, False
            elif p == 7:
                rev = True
            elif p == 27:
                rev = False
            elif p == 49:
                bg = None
            elif 40 <= p <= 47 or 100 <= p <= 107:
                bg = str(p)
            elif p in (38, 48):
                mode = int(ps[j + 1] or 0) if j + 1 < len(ps) else 0
                val = ps[j + 2:j + 5] if mode == 2 else ps[j + 2:j + 3]
                if p == 48:
                    bg = ",".join(val)
                j += 1 + len(val)
            j += 1
    return out


RAW = []  # the captures with their colours, in the order taken (for a look when a target is not found)


def replay(times, rows, cols):
    """The screen at each moment of times (script's clock, ascending): the client's output is written into a pane of
    a private tmux server of the client's size, a little at a time, and the pane is read at each moment. Returns a list
    of screens, each a list of rows of cells."""
    recs = [(t, data) for t, d, data in records() if d == "o"]
    tm = ["tmux", "-L", REPLAY, "-f", "/dev/null"]
    subprocess.run(tm + ["kill-server"], stderr=subprocess.DEVNULL)
    fifo = os.path.join(WORK, "replay.fifo")
    if os.path.exists(fifo):
        os.remove(fifo)
    os.mkfifo(fifo)
    subprocess.run(tm + ["start-server", ";", "set", "-g", "exit-empty", "off", ";", "set", "-g", "status", "off", ";",
                         "set", "-g", "history-limit", "0", ";",
                         "new-session", "-d", "-s", "r", "-x", str(cols), "-y", str(rows), "cat '%s'; sleep 600" % fifo],
                   check=True, env={k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")})
    subprocess.run(tm + ["resize-window", "-t", "r", "-x", str(cols), "-y", str(rows)], check=True)
    out, i = [], 0
    try:
        with open(fifo, "wb", buffering=0) as w:
            for t in times:
                chunk = b""
                while i < len(recs) and recs[i][0] <= t:
                    chunk += recs[i][1]
                    i += 1
                if chunk:
                    w.write(chunk)
                    time.sleep(0.12)
                text = subprocess.run(tm + ["capture-pane", "-p", "-e", "-t", "r"], capture_output=True, text=True,
                                      check=True).stdout
                out.append([cells(l) for l in text.split("\n")[:rows]])
                RAW.append(text)
    finally:
        subprocess.run(tm + ["kill-server"], stderr=subprocess.DEVNULL)
        os.remove(fifo)
    return out


def text_of(row):
    return "".join(c[0] or "\0" for c in row)


def find(screen, pattern, rows=None, first=True):
    """The first (or last) match of a pattern in the screen's rows: (row, first column, last column), or None."""
    order = rows if rows is not None else range(len(screen))
    hits = []
    for r in order:
        if r >= len(screen):
            continue
        for m in re.finditer(pattern, text_of(screen[r])):
            hits.append((r, m.start(), m.end() - 1))
    if not hits:
        return None
    return hits[0] if first else hits[-1]


def tree_right(screen):
    """The last column of the popup's tree: the column of the frame's right edge on the row of its top border."""
    for row in screen[:3]:
        t = text_of(row)
        if "╭" in t:
            k = t.find("╮", t.find("╭"))
            if k > 0:
                return k
    return None


def target_count(screen):
    hit = find(screen, r"[^ ━]?\d+ need you")
    return hit and (hit[0], hit[1], hit[0], hit[2])


def target_cursor(screen):
    """The row the popup's cursor is on: the tree's row drawn on a background of its own, one the panel itself does
    not use (the panel's own background is the commonest one in the tree)."""
    right = tree_right(screen)
    if right is None:
        return None
    count = {}
    for row in screen[1:-2]:
        for c in row[1:right]:
            count[c[1]] = count.get(c[1], 0) + 1
    common = {bg for bg, n in count.items() if n * 4 > sum(count.values()) // 4} | {None}
    for r, row in enumerate(screen[1:-2], start=1):
        inner = row[1:right]
        lit = [k + 1 for k, c in enumerate(inner) if c[1] not in common or c[2]]
        if len(lit) > (right - 1) // 2:
            return (r, lit[0], r, lit[-1])
    return None


def target_prompt(screen):
    """An agent's permission or approval prompt: from its first line (Claude Code's "Bash command", Codex's question)
    to its last choice."""
    top = find(screen, r"Bash command|Would you like to run the following")
    end = find(screen, r"differently \(esc\)", first=False)
    if not (top and end) or end[0] < top[0]:
        return None
    left = min(top[1], end[1])
    width = max(len(text_of(screen[r]).rstrip()) for r in range(top[0], end[0] + 1)) - 1
    return (top[0], max(0, left - 1), end[0], width)


def target_yes(screen):
    hit = find(screen, r"[❯›] 1\. Yes[^\0]*?(?=\s{2,}|$)")
    return hit and (hit[0], hit[1], hit[0], hit[2])


def target_tabs(screen):
    """tmux's window tabs on the status line, from the first bracket to the last."""
    r = len(screen) - 1
    t = text_of(screen[r])
    a, b = t.find("["), t.rfind("]")
    return (r, a, r, b) if 0 <= a < b else None


def target_answer(screen):
    """The finished agent's last answer, from its first line to its last, wide enough for its diff lines' background
    as well as its words."""
    top = find(screen, r"Safe to apply after one fix")
    end = find(screen, r"staging stays up during apply\.", first=False)
    if not (top and end):
        return None
    rows = range(top[0], end[0] + 1)
    width = max(max([len(text_of(screen[r]).rstrip())] + [k + 1 for k, c in enumerate(screen[r]) if c[1]]) for r in rows) - 1
    return (top[0], max(0, top[1] - 2), end[0], width)


TARGETS = {"count": target_count, "cursor": target_cursor, "prompt": target_prompt, "yes": target_yes,
           "tabs": target_tabs, "answer": target_answer}


def status_box(png, rows, cell_h):
    """The box of what is drawn on tmux's status line in one raw frame: x0, y0, x1, y1."""
    y = (rows - 1) * cell_h
    # The left three quarters only: tmux keeps the status line's right part (taw's summary) beside a message.
    out = subprocess.run([MAGICK, png, "-crop", "%dx%d+0+%d" % (WIDTH * 3 // 4, cell_h, y), "+repage", "-fuzz", "12%",
                          "-trim", "-format", "%w %h %X %Y", "info:"], capture_output=True, text=True, check=True).stdout
    w, h, ox, oy = out.split()
    ox, oy = int(ox.lstrip("+")), int(oy.lstrip("+"))
    return ox, y + oy, ox + int(w), y + oy + int(h)


def highlight(box, out, pad_x=6, pad_y=3):
    """One highlight as a full-frame PNG: a rounded yellow outline with a faint glow around box (pixels), nothing
    dimmed. A one-row box is a pill; a taller box keeps a smaller radius."""
    x0, y0, x1, y1 = box
    x0, y0, x1, y1 = x0 - pad_x, y0 - pad_y, x1 + pad_x, y1 + pad_y
    r = min((y1 - y0) // 2, 22)
    rect = "roundrectangle %d,%d,%d,%d,%d,%d" % (x0, y0, x1, y1, r, r)
    size = "%dx%d" % (WIDTH, HEIGHT + BAND)
    subprocess.run([MAGICK, "-size", size, "xc:none",
                    "(", "-size", size, "xc:none", "-fill", "none", "-stroke", "rgba(249,226,175,0.55)",
                    "-strokewidth", "10", "-draw", rect, "-blur", "0x9", ")", "-composite",
                    "-fill", "none", "-stroke", "#f9e2af", "-strokewidth", "3", "-draw", rect, out], check=True)


def cell_box(c, grid_):
    """A box of cells (row, first column, last row, last column) as the pixels it covers in the GIF."""
    rows, cols, cw, ch, ox, oy = grid_
    r0, c0, r1, c1 = c
    return ox + c0 * cw, oy + r0 * ch, ox + (c1 + 1) * cw, oy + (r1 + 1) * ch


SAMPLE = 0.2  # seconds between two looks at the screen within a step
SETTLE = 0.2  # a step's highlight starts this long after its key, once the screen has changed
SETTLE_NOW = {"yes"}  # targets already on the screen when the key is pressed: their highlight starts with it


def find_targets(info, scale, grid_):
    """Each step's highlight segments: (start, end, cell box), from the screen as the client drew it."""
    rows, cols = grid_[0], grid_[1]
    offs = anchors(records(), info["captions"], scale)
    moments = []  # (step, video time)
    for i, c in enumerate(info["captions"]):
        if not TARGETS.get(c["target"]):
            continue
        t = c["start"] * scale + (0.0 if c["target"] in SETTLE_NOW else SETTLE)
        while t < c["end"] * scale - 0.05:
            moments.append((i, t))
            t += SAMPLE
    order = sorted(range(len(moments)), key=lambda k: moments[k][1] + offs[moments[k][0]])
    screens = replay([moments[k][1] + offs[moments[k][0]] for k in order], rows, cols)
    shot = [None] * len(moments)
    for n, k in enumerate(order):
        shot[k] = screens[n]
    shutil.rmtree(CAPDIR, ignore_errors=True)
    os.makedirs(CAPDIR)
    segs = {}
    for k, (i, t) in enumerate(moments):
        c = info["captions"][i]
        with open(os.path.join(CAPDIR, "step%02d-%05.2f.txt" % (i + 1, t)), "w", encoding="utf-8") as f:
            f.write("\n".join(text_of(row).replace("\0", "") for row in shot[k]))
        with open(os.path.join(CAPDIR, "step%02d-%05.2f.ansi" % (i + 1, t)), "w", encoding="utf-8") as f:
            f.write(RAW[order.index(k)])
        box = TARGETS[c["target"]](shot[k])
        s = segs.setdefault(i, [])
        if box and s and s[-1][2] == box and abs(s[-1][1] - t) < SAMPLE / 2:
            s[-1][1] = t + SAMPLE
        elif box:
            s.append([t, t + SAMPLE, box])
    for i, s in segs.items():
        if len(s) > 1:
            segs[i] = [x for x in s if x[1] - x[0] > SAMPLE * 1.5] or s
    for i, c in enumerate(info["captions"]):
        if TARGETS.get(c["target"]):
            found = segs.get(i, [])
            print("step %d (%s): %s" % (i + 1, c["target"], ", ".join(
                "%.1f-%.1f s at %s" % (a, min(b, c["end"] * scale), x) for a, b, x in found) or "NOT FOUND"))
    return {i: [(a, min(b, info["captions"][i]["end"] * scale), x) for a, b, x in s] for i, s in segs.items()}


LENGTH = [0.0]  # the recording's length in seconds, once write_gif knows it


def overlay(inputs, chain, prev, png, x, y, a, b):
    """One picture over the video from a to b seconds, fading in and out; returns the new last label."""
    n = inputs.count("-i") + 1
    inputs += ["-framerate", str(FPS), "-loop", "1", "-t", "%.3f" % LENGTH[0], "-i", png]
    chain.append("[%d:v]format=rgba,fade=t=in:st=%.3f:d=%.2f:alpha=1,fade=t=out:st=%.3f:d=%.2f:alpha=1[o%d]"
                 % (n, a, FADE, max(a, b - FADE), FADE, n))
    chain.append("[%s][o%d]overlay=%d:%d:enable='between(t,%.3f,%.3f)'[v%d]" % (prev, n, x, y, a, b, n))
    return "v%d" % n


def write_gif():
    info = json.load(open(CAPS))
    os.makedirs(WORK, exist_ok=True)
    files = sorted(glob.glob(os.path.join(FRAMES, "frame-text-*.png")))
    if not files:
        sys.exit("no frames in dist/frames: record first")
    length = len(files) / FPS
    print("recording is %.1f s, the storyboard says %.1f s" % (length, info["total"]))
    # vhs drops a little time at each key (its own overhead is shorter than the typing speed the tape counts): the
    # times are scaled by the recording's length over the storyboard's, so a caption is never late by the sum.
    scale = length / info["total"] if abs(length - info["total"]) < 3 else 1.0
    grid_ = grid()
    rows, cols, cw, ch, ox, oy = grid_
    print("terminal %d columns by %d rows, cells %dx%d px" % (cols, rows, cw, ch))
    targets = find_targets(info, scale, grid_)
    # Input 1 is the cursor layers; every other input is a picture laid over the video for a while.
    LENGTH[0] = length
    inputs = ["-framerate", str(FPS), "-i", os.path.join(FRAMES, "frame-cursor-%05d.png")]
    chain, prev = [], "base"
    hl_n = 0
    for i, c in enumerate(info["captions"]):
        t0, t1 = c["start"] * scale, c["end"] * scale
        if c["target"] == "toast":
            # The toast: every frame of the step whose status line is yellow.
            lit = [k for k in range(int(t0 * FPS), min(len(files), int(t1 * FPS) + 1))
                   if yellowness(files[k], rows, ch) > 0.04]
            if lit:
                mid = lit[len(lit) // 2]
                # Its columns from the frame, its height the status line's row, as every other highlight's.
                x0, _y0, x1, _y1 = status_box(files[mid], rows, ch)
                hl = os.path.join(WORK, "toast-highlight.png")
                highlight((x0 + ox, oy + (rows - 1) * ch, x1 + ox, oy + rows * ch), hl, pad_x=12)
                a, b = lit[0] / FPS, (lit[-1] + 1) / FPS
                prev = overlay(inputs, chain, prev, hl, 0, 0, a, b)
                print("step %d (toast): %.1f-%.1f s" % (i + 1, a, b))
                shot = os.path.join(WORK, "toast-frame.png")
                compose(mid, shot)
                subprocess.run([MAGICK, shot, "(", hl, "-crop", "%dx%d+0+0" % (WIDTH, HEIGHT), "+repage", ")",
                                "-composite", "-strip", TOAST_PNG], check=True)
            else:
                print("no toast found in the toast step's frames: look at them")
        for a, b, box in targets.get(i, []):
            hl_n += 1
            hl = os.path.join(WORK, "highlight%02d.png" % hl_n)
            highlight(cell_box(box, grid_), hl)
            prev = overlay(inputs, chain, prev, hl, 0, 0, a, b)
        if c["shot"] in STILLS:
            k = min(len(files) - 1, int(round(c["shot_at"] * scale * FPS)))
            for out in STILLS[c["shot"]]:
                compose(k, out)
        if not (c["key"] or c["words"]):
            continue
        png = os.path.join(WORK, "pill%02d.png" % i)
        pw, ph, _margin = draw_pill(i, c["key"], c["words"], png)
        # The band under the terminal, the pill centred in it: the same place for every step.
        prev = overlay(inputs, chain, prev, png, (WIDTH - pw) // 2, HEIGHT + (BAND - ph) // 2, t0, t1)
    # The palette from the whole video; no dithering (it speckles text); only the changed rectangle per frame.
    chain.append("[%s]split[a][b];[a]palettegen=max_colors=256:stats_mode=full[p];"
                 "[b][p]paletteuse=dither=none:diff_mode=rectangle" % prev)
    # Input 0 is the text layers, input 1 the cursor layers: the cursor goes over the text, the canvas is centred on
    # the background in the terminal's part of the frame, a hairline and the band go under it, then the captions and
    # the highlights go over that.
    base = ("[0:v][1:v]overlay=0:0,pad=%d:%d:%d:%d:color=0x11111b,"
            "drawbox=x=0:y=%d:w=%d:h=2:color=0x313244:t=fill[base]" % (WIDTH, HEIGHT + BAND, ox, oy, HEIGHT, WIDTH))
    chain = [base] + chain
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-framerate", str(FPS), "-i", os.path.join(FRAMES, "frame-text-%05d.png")]
                   + inputs + ["-filter_complex", ";".join(chain), "-loop", "0", GIF], check=True)
    print("%s: %.0f KiB, %dx%d, %.1f s" % (GIF, os.path.getsize(GIF) / 1024, WIDTH, HEIGHT + BAND, length))


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "all"
    os.chdir(REPO)
    if mode in ("tape", "all"):
        write_tape()
    if mode == "all":
        shutil.rmtree(FRAMES, ignore_errors=True)
        subprocess.run(["vhs", TAPE], check=True)
    if mode in ("captions", "all"):
        write_gif()
    if mode == "targets":
        # Only the highlights' places, from the last recording: dist/captures/ and one line per step.
        info = json.load(open(CAPS))
        files = glob.glob(os.path.join(FRAMES, "frame-text-*.png"))
        scale = len(files) / FPS / info["total"]
        os.makedirs(WORK, exist_ok=True)
        find_targets(info, scale if abs(scale - 1) < 0.1 else 1.0, grid())


if __name__ == "__main__":
    main()
