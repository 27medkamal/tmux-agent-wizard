# The landing page's two stills. scripts/demo/stills.sh runs this tape and then stacks worktree.png from its two
# pictures of the worktree (vhs writes a tape's Screenshots only when its recording ends, so no step of the tape can
# read them). With a built taw:
#
#   bash scripts/demo/stills.sh
#
# writes docs/assets/picker.png (the popup's project picker, opened with n) and docs/assets/worktree.png (a closed
# worktree row in the tree above, the session it reopens into below). The world is the GIF's (start.sh) with this
# machine alone, so the popup draws no machine headers, as for anyone with one machine. Size, font, theme and tmux
# look are the GIF's (make-gif.py's tape_settings, repeated here), so the stills match popup.png.

Output dist/stills.mp4

Set Shell "bash"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 24
Set Width 2350
Set Height 1124
Set Padding 24
Set Framerate 15
Set Theme {"name": "Catppuccin Mocha Crust", "background": "#11111b", "foreground": "#cdd6f4", "cursor": "#f5e0dc", "selection": "#585b70", "black": "#45475a", "red": "#f38ba8", "green": "#a6e3a1", "yellow": "#f9e2af", "blue": "#89b4fa", "magenta": "#f5c2e7", "cyan": "#94e2d5", "white": "#bac2de", "brightBlack": "#585b70", "brightRed": "#f38ba8", "brightGreen": "#a6e3a1", "brightYellow": "#f9e2af", "brightBlue": "#89b4fa", "brightMagenta": "#f5c2e7", "brightCyan": "#94e2d5", "brightWhite": "#a6adc8"}
Set TypingSpeed 50ms
Set CursorBlink false

Hide
Type "HISTFILE=/dev/null TAW_DEMO_ROOT=${TAW_DEMO_ROOT:-/tmp/taw-demo} bash scripts/demo/start.sh --detach && echo world-$((6*7))-ready"
Enter
Wait+Screen@60s /world-42-ready/
# Every watcher has read its pane before the first still.
Sleep 9s
Type "source ${TAW_DEMO_ROOT:-/tmp/taw-demo}/env.sh && clear"
Enter
Sleep 0.5s
Show

# The stills come from the popup, so the terminal is a tmux client of the world's own server.
Type "clear && tmux -L ${TAW_DEMO_SOCKET:-taw-demo} attach -t api"
Enter
Sleep 2s
Ctrl+B
Type "A"
Wait+Screen@20s /Enter go/
Sleep 1s

# Still 1: n opens the picker over the directories zoxide remembers and the project folders.
Type "n"
Wait+Screen@20s /type a project, or a path/
Wait+Screen@20s /project dir/
Sleep 1s
Screenshot docs/assets/picker.png
Sleep 0.5s
Escape
Sleep 1s

# Still 2: d puts the cursor on the closed worktree row (a picture is taken), Enter opens its session, and
# the popup again shows the session it reopened into, the preview beside the tree as in the GIF.
Type "d"
Wait+Screen@20s /worktree, closed/
Sleep 0.8s
Screenshot dist/worktree-closed.png
Sleep 0.5s
Enter
Wait+Screen@20s /api--fix-login \$/
Sleep 0.5s
Type "git status"
Enter
Wait+Screen@20s /On branch fix-login/
Sleep 0.5s
Ctrl+B
Type "A"
Wait+Screen@20s /Enter go/
Sleep 1s
Screenshot dist/worktree-open.png
Sleep 0.5s

Hide
Type "q"
Sleep 1.5s
Ctrl+B
Type "d"
Wait+Screen@20s /detached/
# The server ends here, and the tape waits until it no longer answers.
Type "tmux -L ${TAW_DEMO_SOCKET:-taw-demo} kill-server; sleep 2; tmux -L ${TAW_DEMO_SOCKET:-taw-demo} ls 2>&1 | head -1; echo servers-gone-$((6*7))"
Enter
Wait+Screen@20s /servers-gone-42/
