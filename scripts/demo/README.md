# The demo recording and the cost measurement

`docs/assets/demo.gif`, the animation at the top of the README, and
`docs/assets/social-preview.png`, the card a link to the repository shows, are made here, and so are the
landing page's two stills (`docs/assets/picker.png` and `worktree.png`).
`measure.sh` measures what taw costs while it watches your agents.

## What is real and what is a stand-in

taw, the plugin, tmux and every key press are real (Ctrl+G and Ctrl+Y are the demo's own
hidden keys, which move the stand-ins along). The agents and the sandbox are stand-ins:

- The agents (`agent.sh`) are small scripts that draw the screen and set the terminal
  title the way Claude Code, Codex, OpenCode and Gemini CLI do, so taw's detection rules
  read them like the real thing, and that change state when the recording asks them to.
  Claude Code's and Codex's screens are drawn whole by `screen.py`, at the pane's size (read at
  start and again whenever the pane is resized, lines cut to the width, never wrapped): an earlier
  part of the conversation at the top (the request, tool calls with their results, a short diff)
  and the live part at the bottom (a permission or approval prompt, the spinner over the input
  box, or the idle input box and its footer), in the words, glyphs and dark-theme colours of the
  real programs. Gemini CLI's and OpenCode's short screens are in `screens/`.
  Every word they draw fits the popup's preview (84 columns at the recording's 150), so the preview
  never wraps a stand-in's line: short sentences, commands and diff lines, a longer sentence wrapped
  at a word the way the real programs wrap in a narrow terminal (`python3 scripts/demo/screen.py
  check` lists any line that does not fit). The stand-ins draw on the terminal's alternate screen,
  which the real programs do not: taw's preview cuts such a pane's lines at its edge instead of
  wrapping them, so the input box's rules and the background of a user turn or a diff line, the
  only things wider than the preview, end at its edge. taw itself is unchanged.
- The OpenCode stand-in reports its state through `taw pane report-agent`, exactly as
  OpenCode's plugin does once `taw integration install opencode` is in place. taw takes
  such a report only for a pane whose program is called `opencode`, so it runs under a
  program of that name (`standin.c`, built when the world starts).
- The agent in a sandbox is the same kind of stand-in, run by a program called `docker`
  (also `standin.c`), started as `docker run ... claude-sandbox:latest claude`. taw's
  watcher reads that command line the way it reads a real container's, and the pane
  carries the option `taw sandbox` sets (`@agent_sandbox`), so the popup draws its
  package. No container, image or Docker is involved.

The GIF's second computer is a stand-in too. The machine called home-server (`far.sh`) is a second tmux
server on the same computer, with a home folder, config, state and stand-in agents of its own (one
of them stopped at a permission prompt), listed in the demo config's `[machines]` as `home-server` and
reached through a stand-in `ssh`: a script placed first on the PATH that runs the far taw against
that second server where a real ssh would log in, as `tests/common/machine.rs` does for the tests.
The machine called work-laptop is listed too and never answers (the stand-in ssh knows no other
destination). Nothing reaches a real ssh or a real host.

Everything runs in a throwaway tmux server with its own `$HOME`, config and state, so
your own tmux and taw are never touched. Six projects live under that home folder
(`code/api`, `web`, `docs`, `infra`, `mobile`, `data`), with eight agents among them,
a few plain panes (dev servers, shells) and a parked worktree.

## Re-recording the GIF

From the repository root, with a built taw (`mise run build`), a C compiler (`cc`), python3,
[vhs](https://github.com/charmbracelet/vhs) 0.12 or newer, ffmpeg, ImageMagick (`magick`), a tmux first on the PATH
(3.8), the JetBrainsMono Nerd Font installed, and Inter for the captions (SIL Open Font License: put
`Inter-Medium.ttf` and `Inter-SemiBold.ttf` from [its release](https://github.com/rsms/inter/releases) in the
git-ignored `dist/fonts/`; without them the captions fall back to Helvetica Neue):

```sh
PATH=/opt/homebrew/bin:$PATH python3 scripts/demo/make-gif.py
```

The GIF is told by a storyboard, `story.txt`: one line per step, with the keys pressed, the caption (the key,
then what it does in plain words), how long the step lasts, a still to take, and what the step's highlight goes
around. `make-gif.py` writes `dist/demo.tape` from it and runs vhs on it, which writes every frame as a PNG into
`dist/frames/` (no video codec in between, so coloured text stays sharp). It is recorded 2350 px wide, 150 columns
by 34 rows of JetBrainsMono at 24 px, Catppuccin Mocha on its darkest base (`#11111b`), 15 frames a second: a wide
screen, so the list keeps the width taw gives it and the preview beside it is big. GitHub draws a README image at
most as wide as its column (about 830 to 880 px), so the README shows the text smaller than a 110-column recording
would; the video in full size shows it large. Edit the storyboard, never the tape.

The captions sit in a band under the terminal: each is a rounded dark pill with keycaps for the keys (`prefix + A`
is a `prefix` key and an `A` key) and the words in Inter, drawn once as a transparent PNG by `magick` and laid over
the frames by ffmpeg from its step's key to the next one's, fading in and out, always centred in the band, so a
caption never covers the terminal and never moves.

Every step outlines what it is about in the toast's yellow: a rounded `#f9e2af` outline with a faint glow, nothing
dimmed (a highlight is on nearly the whole time, so dimming would darken most of the GIF). The targets: the popup's
`need you` count, the row the cursor lands on, the agent's permission or approval prompt, its `1. Yes` line as it
is chosen, tmux's window tabs, the toast, the finished agent's last answer. Each is found in the screen's own text:
the tape attaches its tmux client through `script -r`, which keeps every byte tmux draws with its time, and the
script plays that back into a pane of a private tmux server (`taw-demo-replay`) of the client's size, reads the
pane a few times a second of each step (`dist/captures/`), finds the target's text there and turns its cells into
pixels, so the outline follows the layout. The toast's moment comes from the frames (the status line turns yellow).
Then one 256-colour palette is made from the whole video and the GIF is written with no dithering and only the
changed rectangle of each frame, which keeps it sharp and under 8 MB.

The look is `tmux.conf`'s: the status line at the bottom on the terminal's background, the session in blue,
window tabs in brackets with a small Nerd Font icon (a robot for an agent's window), the current one in magenta,
taw's summary on the right, the popup covering the whole window above the status line (taw draws a full-size popup with no frame of its
own, so nothing of the window under it shows at its edges), and a yellow toast.

The story opens on the popup (`prefix + A`) in the world of `start.sh` with `TAW_DEMO_STORY` set: this
machine's six sessions and their agents, listed with agents only, no dev server or shell in any of them, and
under them two other machines, `home-server` (reached through the stand-in ssh, its agents listed) and
`work-laptop` (one that does not answer). The popup lists another machine only through a running `taw app`, so the
tape starts one hidden, in a tmux server of its own. Then `Tab` to the agent that needs you (docs' Claude Code,
asking permission to run a command), `Enter` into its pane, `1` to answer it, `prefix + A` back to the list,
`Tab` and `Enter` to the next one (web's Codex, asking approval to run the tests), back without answering, a
toast when another agent finishes (infra's Claude Code, the toast showing its last line), `prefix + O` to it, and the list again. Every `Enter` goes into
a pane of this machine.

How the captions stay in step: nothing after the tape's `Show` waits for the screen. Every key is followed by a
fixed `Sleep`, so the tape owns the clock; the script adds up every pause (each key costs the typing speed) and
draws each caption from the moment its key is pressed to the next one's. Everything that needs a wait (the world,
the keeper app, the popup with the other machines in it) happens before `Show`. The script prints the video's
length beside the storyboard's total, so a machine too slow to keep the clock shows. The same run takes
`docs/assets/popup.png`, `docs/assets/toast.png` (with the toast's highlight) and `dist/popup-frame.png`, the
picture of the link card (steps marked `popup` and `toast` in the storyboard). Re-record whenever the popup
changes, so the README never shows a popup taw no longer has. `TAW_DEMO_ROOT` (default `/tmp/taw-demo`) and
`TAW_DEMO_SOCKET` (default `taw-demo`) say where the world lives and which tmux socket it uses; `TAW_DEMO_BIN`
names the taw to use.

## Re-recording the stills

```sh
PATH=/opt/homebrew/bin:$PATH bash scripts/demo/stills.sh
```

Same needs as the GIF. It builds the world with this machine alone, so the popup draws no machine headers, as
for anyone with one machine, and writes `docs/assets/picker.png` (the popup's project picker, opened with `n`,
over directories zoxide remembers and the project folders) and `docs/assets/worktree.png` (above, the parked
worktree's row in the tree; below, the session `Enter` reopens it into; stacked by `magick` once the tape has
ended, since vhs writes a tape's pictures only when its recording ends). Size, font, theme and tmux look are the
GIF's. The world's server, `taw-demo`, is ended when the recording ends. Re-record whenever the picker or a
worktree row changes.

## The link card

`scripts/demo/social-preview.sh [picture.png]` renders `docs/assets/social-preview.png`
(1280x640), the card GitHub, X and Reddit show for a link to the repository:
`social-preview.html` beside the picture the tape took (`dist/popup-frame.png` by default):
the whole tree, the cursor on the agent that needs you. It needs
that picture, so make the GIF first (`make-gif.py`), and a Chrome or Chromium to render with.

To look around the demo world yourself instead: `scripts/demo/start.sh` builds it and
attaches (socket `taw-demo`). Alt-b stops web's Codex at an approval prompt,
Alt-f makes infra's Claude Code finish, Alt-r sets both working again. Leave with
`tmux -L taw-demo kill-server`.

## Measuring the cost

```sh
scripts/demo/measure.sh        # 60 seconds; pass another number of seconds if you like
```

Ten agent panes (five Claude Code stand-ins mid-turn, redrawing their spinner line as
the real one does, three idle, two Codex mid-turn), then the memory and CPU time of
taw's own processes over the window: the one watcher tmux runs per pane. It needs
bash 4 or newer (Homebrew's bash on macOS).
