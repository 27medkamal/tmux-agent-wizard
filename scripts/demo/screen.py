#!/usr/bin/env python3
"""Draws one whole screen of a stand-in agent for the demo (agent.sh calls it on every change of state and of size).

  screen.py <kind> <role> <state> <columns> <rows>

prints exactly <rows> lines, each cut to <columns> (never wrapped): an earlier part of the conversation at the top,
the live part at the bottom (a permission or approval prompt, the spinner line over the input box, or the idle
input box with its footer). {SPIN} and {SECS} are left in the spinner line for agent.sh to fill on every tick.

Every word fits the popup's preview (TEXT_WIDTH columns at the recording's 110), so the preview never wraps a line
of a stand-in: sentences, commands and diff lines are short, and a longer sentence is wrapped here, at a word, the
way the real programs wrap in a narrow terminal. Only the input box's rules and the background of a user turn or a
diff line reach past that width; agent.sh keeps the stand-ins on the terminal's alternate screen, where the preview
cuts a line at its edge instead of wrapping it. `screen.py check` prints every line that does not fit.

The look follows the real programs. Claude Code: the glyphs, words and dark-theme colours of its installed binary
(claude orange 215,119,87, permission 177,185,249, prompt border 136,136,136, success 78,186,101, the diff
backgrounds 34,92,43 and 122,41,54, the user message background 55,55,55, auto-accept 175,135,255). Codex: its TUI
(codex-rs/tui), the approval overlay and status line words of its snapshots. Gemini CLI and OpenCode keep their
short screens in screens/<role>-<state>.txt, drawn at the top of the pane.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ESC = "\033["
RESET = ESC + "0m"


def rgb(r, g, b):
    return "%s38;2;%d;%d;%dm" % (ESC, r, g, b)


def on(r, g, b):
    return "%s48;2;%d;%d;%dm" % (ESC, r, g, b)


BOLD, DIM, ITAL = ESC + "1m", ESC + "2m", ESC + "3m"
WHITE = rgb(255, 255, 255)
# Claude Code's dark theme.
C_ORANGE, C_PERM, C_BORDER = rgb(215, 119, 87), rgb(177, 185, 249), rgb(136, 136, 136)
C_GREY, C_SUCCESS, C_AUTO = rgb(153, 153, 153), rgb(78, 186, 101), rgb(175, 135, 255)
C_ADD, C_DEL, C_USER = on(34, 92, 43), on(122, 41, 54), on(55, 55, 55)
# Codex: plain ANSI colours, as its TUI uses, and a tint over the terminal's background for the user's turn.
X_GREEN, X_RED, X_CYAN, X_MAGENTA = ESC + "32m", ESC + "31m", ESC + "36m", ESC + "35m"
X_USER = on(36, 36, 52)

# ---- The conversations. One list per role: ("code", text) pairs, drawn by draw_claude or draw_codex. ----
# Claude:  U user turn   A answer (first line)   . its next line   T tool done (name|args)   P tool running
#          R result (first line)   r its next line   M "… +n lines"   + - = a diff line (number|text)
# Codex:   U user turn   A answer   . its next line   T Ran (command)   E Explored   R └ line   r its next line
#          H Edited (file|added|removed)   + - = a diff line   M "… +n lines"   W "Worked for" rule
# U, A, ., R and r are wrapped at a word to fit TEXT_WIDTH; a tool line, a diff line or a file name is written to fit.
CONVERSATIONS = {
    "web": [
        ("U", "Add validation to the signup form: a real email, a password of 12 characters or more, and an error under each field"),
        ("", ""),
        ("E", "Read SignupForm.tsx, api.ts"),
        ("", ""),
        ("A", "I'll add a schema, check the form on submit, and show each error under its field."),
        ("", ""),
        ("H", "src/signup/schema.ts|18|0"),
        ("+", "1|import { z } from \"zod\";"),
        ("+", "2|"),
        ("+", "3|export const schema = z.object({"),
        ("+", "4|  email: z.string().email(),"),
        ("+", "5|  password: z.string().min(12),"),
        ("M", "+13 lines"),
        ("", ""),
        ("H", "src/signup/SignupForm.tsx|5|1"),
        ("=", "14|  const send = useSignup();"),
        ("-", "15|  const submit = (f) => send(f);"),
        ("+", "15|  const submit = (f) => {"),
        ("+", "16|    const r = schema.safeParse(f);"),
        ("+", "17|    if (r.success) send(f);"),
        ("+", "18|    else setErrors(r.error);"),
        ("+", "19|  };"),
        ("", ""),
        ("A", "Each error now shows under its field. Next, the signup tests."),
    ],
    "web+idle": [
        ("", ""),
        ("T", "npm test -- src/signup"),
        ("R", "Tests: 12 passed, 12 total"),
        ("", ""),
        ("A", "Done: the form checks the email and the password, and the tests pass."),
        ("W", "3m 05s"),
    ],
    "api": [
        ("U", "The auth test fails about one run in twenty on CI. Find out why and fix it"),
        ("", ""),
        ("T", "Bash|npm test -- src/auth --repeat 50"),
        ("R", "Tests: 2 failed, 48 passed"),
        ("r", "session.test.ts: session not valid"),
        ("", ""),
        ("T", "Read|src/auth/session.ts"),
        ("R", "Read 64 lines"),
        ("", ""),
        ("A", "The flake is a race: the check reads the clock twice, so a session can expire between the two reads. I'll read it once."),
        ("", ""),
        ("T", "Update|src/auth/session.ts"),
        ("R", "Updated src/auth/session.ts with 3 additions and 2 removals"),
        ("=", "21|function isValid(s: Session) {"),
        ("-", "22|  return Date.now() < s.exp &&"),
        ("-", "23|    Date.now() - s.iat < MAX_AGE;"),
        ("+", "22|  const now = Date.now();"),
        ("+", "23|  return now < s.exp &&"),
        ("+", "24|    now - s.iat < MAX_AGE;"),
        ("=", "25|}"),
        ("", ""),
        ("P", "Bash|npm test -- src/auth --repeat 200"),
        ("R", "Running…"),
    ],
    "data": [
        ("U", "The nightly metrics job takes four hours. Make it read only what changed"),
        ("", ""),
        ("T", "Read|jobs/metrics.py"),
        ("R", "Read 142 lines"),
        ("", ""),
        ("A", "The job reads every partition again. I'll read only the days that changed since the last run."),
        ("", ""),
        ("T", "Update|jobs/metrics.py"),
        ("R", "Updated jobs/metrics.py with 3 additions and 1 removal"),
        ("=", "40|def partitions(store, since):"),
        ("-", "41|    return store.partitions()"),
        ("+", "41|    new = store.since(since)"),
        ("+", "42|    log.info(\"%d new\", len(new))"),
        ("+", "43|    return new"),
        ("", ""),
        ("P", "Bash|python -m jobs.metrics --backfill"),
        ("R", "Running…"),
    ],
    "release": [
        ("U", "Cut the 0.9 release: bump the version, tag it and push the tag"),
        ("", ""),
        ("T", "Update|Cargo.toml"),
        ("R", "Updated Cargo.toml with 1 addition and 1 removal"),
        ("=", "2|name = \"bench-runner\""),
        ("-", "3|version = \"0.8.4\""),
        ("+", "3|version = \"0.9.0\""),
        ("", ""),
        ("T", "Bash|git tag -a v0.9.0 -m \"v0.9.0\""),
        ("R", "(No content)"),
        ("", ""),
        ("A", "Tagged v0.9.0. Now the push."),
    ],
    "release+blocked": [
        ("", ""),
        ("P", "Bash|git push origin v0.9.0"),
        ("R", "Running…"),
    ],
    "release+working": [
        ("", ""),
        ("P", "Bash|git push origin v0.9.0"),
        ("R", "Running…"),
    ],
    "claude": [
        ("A", "Done. The tests pass and the change is committed."),
    ],
    "changelog": [
        ("U", "Draft the v2 changelog from the pull requests merged since v1.9.0"),
        ("", ""),
        ("A", "I'll list what landed since v1.9.0, group it by area, and write CHANGELOG.md."),
        ("", ""),
        ("T", "Bash|git log v1.9.0.. --oneline"),
        ("R", "4f2c1ab Sign in with a passkey (#412)"),
        ("r", "9be07d3 Renew sessions early (#409)"),
        ("M", "+21 lines (ctrl+o to expand)"),
        ("", ""),
        ("T", "Update|CHANGELOG.md"),
        ("R", "Updated CHANGELOG.md with 6 additions"),
        ("=", "1|# Changelog"),
        ("=", "2|"),
        ("+", "3|## v2.0.0"),
        ("+", "4|"),
        ("+", "5|### Added"),
        ("+", "6|- Sign in with a passkey (#412)"),
        ("+", "7|- Sessions renew early (#409)"),
        ("+", "8|"),
        ("", ""),
        ("A", "Before I date the release, I'll check which v2 tags exist."),
    ],
    "changelog+blocked": [
        ("", ""),
        ("P", "Bash|git tag --list 'v2*'"),
        ("R", "Running…"),
    ],
    "changelog+working": [
        ("", ""),
        ("T", "Bash|git tag --list 'v2*'"),
        ("R", "v2.0.0-rc.1"),
    ],
    "changelog+idle": [
        ("", ""),
        ("T", "Bash|git tag --list 'v2*'"),
        ("R", "v2.0.0-rc.1"),
        ("", ""),
        ("A", "CHANGELOG.md lists the v2 changes under Added, Changed and Removed, each linked to its pull request."),
    ],
    "infra": [
        ("U", "Review the terraform plan for staging and flag anything risky"),
        ("", ""),
        ("T", "Bash|terraform plan -out plan.bin"),
        ("R", "Plan: 3 to add, 1 to change, 0 to destroy."),
        ("", ""),
        ("A", "Only the node group change touches running machines. The rest are new security group rules."),
    ],
    "infra+idle": [
        ("", ""),
        ("A", "Safe to apply after one fix: the node group change replaces all three nodes at once, so staging would be down for about four minutes."),
        ("", ""),
        (".", "modules/eks/nodes.tf"),
        ("=", "12|  update_config {"),
        ("-", "13|    max_unavailable = 3"),
        ("+", "13|    max_unavailable = 1"),
        ("=", "14|  }"),
        ("", ""),
        (".", "With that, staging stays up during apply."),
    ],
    "review": [
        ("U", "Review the retry change in the payments client"),
        ("", ""),
        ("E", "Read client.ts, retry.ts"),
        ("", ""),
        ("T", "npm test -- src/payments"),
        ("R", "Tests: 31 passed, 31 total"),
        ("", ""),
        ("A", "The retry loop is sound, with one fix: it retries on every 4xx, so a declined card uses up the retry budget. Retry only 408, 429 and 5xx."),
        ("", ""),
        (".", "src/payments/retry.ts"),
        ("-", "18|  if (s >= 400) return true;"),
        ("+", "18|  if (s === 408) return true;"),
        ("+", "19|  if (s === 429) return true;"),
        ("+", "20|  if (s >= 500) return true;"),
        ("W", "1m 02s"),
    ],
    "bench": [
        ("U", "Find out why the nightly build got slower"),
        ("", ""),
        ("T", "cargo build --release --timings"),
        ("R", "Finished release in 6m 12s"),
        ("", ""),
        ("E", "Read Cargo.toml, build.rs"),
        ("r", "Read cargo-timing.html"),
        ("", ""),
        ("A", "Most of the time goes to one crate: build.rs makes the bindings again on every build, even when the headers have not changed."),
    ],
}

# The live part of each role: what Claude Code asks (Bash command, its description, the prefix of option 2) or what
# its spinner says; what Codex asks (reason, command, prefix) or its status word. A spinner's words stay short, so
# the line fits the preview with its seconds and "esc to interrupt".
LIVE = {
    "web": {"ask": ("check the new validation", "npm test -- src/signup", "npm test"), "spin": "Working"},
    "api": {"spin": "Testing auth"},
    "data": {"spin": "Backfilling"},
    "release": {"ask": ("git push origin v0.9.0", "Push the release tag", "git push"), "spin": "Pushing the tag"},
    "claude": {"spin": "Working"},
    "changelog": {"ask": ("git tag --list 'v2*'", "List the v2 release tags", "git tag"), "spin": "Checking tags"},
    "infra": {"spin": "Reviewing"},
    "review": {"spin": "Working"},
    "bench": {"spin": "Working"},
}
CWD = {"web": "web", "api": "api", "data": "data", "release": "release", "changelog": "docs", "infra": "infra",
       "review": "api", "bench": "bench", "claude": "api"}

# The popup's preview at the recording's 110 columns: the tree takes 64, the preview's frame 46, inside it 44.
TEXT_WIDTH = 44


def wrap(text, first, rest):
    """text in lines of at most TEXT_WIDTH columns, the first after a prefix `first` columns wide and the rest after
    one `rest` wide, broken at a word."""
    lines, line, room = [], "", TEXT_WIDTH - first
    for word in text.split(" "):
        if line and len(line) + 1 + len(word) > room:
            lines.append(line)
            line, room = word, TEXT_WIDTH - rest
        else:
            line = word if not line else line + " " + word
    return lines + [line]


# ---- Lines: a line is a list of (style, text) parts; it is cut to the width, and a filled line pads to it. ----
def cut(parts, width, fill=""):
    out, used = [], 0
    for style, text in parts:
        if used >= width:
            break
        text = text[: width - used]
        used += len(text)
        out.append(style + text + RESET)
    if fill:
        out.append(fill + " " * (width - used) + RESET)
    return "".join(out) + ESC + "K"


def draw_claude(code, text, width):
    if code == "U":
        return [cut([(C_USER + C_GREY, "❯ " if i == 0 else "  "), (C_USER + WHITE, part)], width, fill=C_USER)
                for i, part in enumerate(wrap(text, 2, 2))]
    if code == "A":
        return [cut([(WHITE, "⏺ ") if i == 0 else ("", "  "), ("", part)], width) for i, part in enumerate(wrap(text, 2, 2))]
    if code == ".":
        return [cut([("", "  " + part)], width) for part in wrap(text, 2, 2)]
    if code in ("T", "P"):
        name, _, args = text.partition("|")
        dot = C_SUCCESS if code == "T" else C_GREY
        return [cut([(dot, "⏺ "), (BOLD, name), ("", "(" + args + ")")], width)]
    if code == "R":
        return [cut([(C_GREY, "  ⎿  " if i == 0 else "     "), (C_GREY, part)], width) for i, part in enumerate(wrap(text, 5, 5))]
    if code == "r":
        return [cut([(C_GREY, "     " + part)], width) for part in wrap(text, 5, 5)]
    if code == "M":
        return [cut([(C_GREY, "     … " + text)], width)]
    if code in "+-=":
        num, _, body = text.partition("|")
        sign = {"+": "+", "-": "-", "=": " "}[code]
        if code == "=":
            return [cut([(C_GREY, "     %4s  " % num), ("", body)], width)]
        bg = C_ADD if code == "+" else C_DEL
        return [cut([("", "     "), (bg + WHITE, "%4s %s" % (num, sign)), (bg + WHITE, body)], min(width, 96), fill=bg)]
    return [cut([("", text)], width)]


def draw_codex(code, text, width):
    if code == "U":
        pad = cut([], width, fill=X_USER)
        return [pad] + [cut([(X_USER + BOLD + DIM, "› " if i == 0 else "  "), (X_USER, part)], width, fill=X_USER)
                        for i, part in enumerate(wrap(text, 2, 2))] + [pad]
    if code == "A":
        return [cut([("", ("• " if i == 0 else "  ") + part)], width) for i, part in enumerate(wrap(text, 2, 2))]
    if code == ".":
        return [cut([("", "  " + part)], width) for part in wrap(text, 2, 2)]
    if code == "T":
        return [cut([(X_GREEN + BOLD, "• "), (BOLD, "Ran "), ("", text)], width)]
    if code == "E":
        return [cut([(DIM, "• "), (BOLD, "Explored")], width), cut([(DIM, "  └ "), ("", text)], width)]
    if code == "R":
        return [cut([(DIM, ("  └ " if i == 0 else "    ") + part)], width) for i, part in enumerate(wrap(text, 4, 4))]
    if code == "r":
        return [cut([(DIM, "    " + part)], width) for part in wrap(text, 4, 4)]
    if code == "M":
        return [cut([(DIM, "    … " + text)], width)]
    if code == "H":
        name, add, rem = text.split("|")
        return [cut([(X_GREEN + BOLD, "• "), (BOLD, "Edited "), ("", name + " ("), (X_GREEN, "+" + add),
                     ("", " "), (X_RED, "-" + rem), ("", ")")], width)]
    if code in "+-=":
        num, _, body = text.partition("|")
        colour = {"+": X_GREEN, "-": X_RED, "=": ""}[code]
        sign = {"+": "+", "-": "-", "=": " "}[code]
        return [cut([(DIM, "    %3s " % num), (colour, sign + body)], width)]
    if code == "W":
        label = "─ Worked for %s " % text
        return [cut([(DIM, label + "─" * max(0, width - len(label)))], width)]
    return [cut([("", text)], width)]


def live_claude(role, state, width):
    live = LIVE.get(role, LIVE["claude"])
    rule = cut([(C_BORDER, "─" * width)], width)
    if state == "blocked":
        command, desc, prefix = live["ask"]
        # Claude Code's own words, wrapped where it wraps them in a terminal as narrow as the preview.
        return [
            "",
            cut([(C_PERM + BOLD, "  Bash command")], width),
            "",
            cut([("", "    " + command)], width),
            cut([(C_GREY, "    " + desc)], width),
            "",
            cut([("", "  Do you want to proceed?")], width),
            cut([(C_PERM, "  ❯ 1. Yes")], width),
            cut([("", "    2. Yes, and don't ask again for")], width),
            cut([("", "       "), (BOLD, prefix), ("", " commands in this project")], width),
            cut([("", "    3. No, and tell Claude what to do")], width),
            cut([("", "       differently "), (BOLD, "(esc)")], width),
            "",
            cut([(C_GREY, "  Esc to cancel · Tab to amend")], width),
            cut([(C_GREY, "  ctrl+e to explain")], width),
        ]
    box = [rule, cut([(C_GREY, "❯ "), (ESC + "7m", " ")], width), rule]
    if state == "working":
        spin = [cut([(C_ORANGE, "{SPIN} " + live["spin"] + "… "), (C_GREY, "({SECS}s · esc to interrupt)")], width)]
        return ["", *spin, ""] + box + [cut([(C_AUTO, "  ⏵⏵ accept edits on "), (C_GREY, "(shift+tab to cycle)")], width)]
    return [""] + box + [cut([(C_GREY, "  ? for shortcuts")], width)]


def live_codex(role, state, width):
    live = LIVE.get(role, LIVE["infra"])
    if state == "blocked":
        reason, command, prefix = live["ask"]
        # Codex's own words, wrapped where it wraps them in a terminal as narrow as the preview.
        return [
            "",
            cut([(BOLD, "  Would you like to run the following")], width),
            cut([(BOLD, "  command?")], width),
            "",
            cut([("", "  "), (ITAL, "Reason: " + reason)], width),
            "",
            cut([(DIM, "  $ "), ("", command)], width),
            "",
            "",
            cut([(X_CYAN, "› 1. Yes, proceed "), (DIM, "(y)")], width),
            cut([("", "  2. Yes, and don't ask again for commands")], width),
            cut([("", "     that start with "), (X_CYAN, "`%s`" % prefix), (DIM, " (p)")], width),
            cut([("", "  3. No, and tell Codex what to do")], width),
            cut([("", "     differently "), (DIM, "(esc)")], width),
            "",
            cut([(DIM, "  Press enter to confirm or esc to cancel")], width),
        ]
    footer = cut([(DIM, "  gpt-5-codex high · ~/code/" + CWD.get(role, role))], width)
    composer = ["", cut([(BOLD, "› "), (DIM, "Ask Codex to do anything")], width), "", footer]
    if state == "working":
        return ["", cut([(DIM, "• "), (BOLD, live["spin"]), (DIM, " ({SECS}s • esc to interrupt)")], width)] + composer
    return composer


BLANK_TOP = 2


def screen(kind, role, state, width, rows):
    if kind not in ("claude", "codex"):
        name = "%s/screens/%s-%s.txt" % (HERE, role, state)
        if not os.path.exists(name):
            name = "%s/screens/%s-%s.txt" % (HERE, kind, state)
        lines = [cut([("", l.rstrip("\n"))], width) for l in open(name, encoding="utf-8")]
        return (lines + [ESC + "K"] * rows)[:rows]
    draw = draw_claude if kind == "claude" else draw_codex
    body = []
    convo = CONVERSATIONS.get(role, CONVERSATIONS["claude"]) + CONVERSATIONS.get("%s+%s" % (role, state), [])
    for code, text in convo:
        body += draw(code, text, width) if code else [ESC + "K"]
    live = (live_claude if kind == "claude" else live_codex)(role, state, width)
    live = [l or ESC + "K" for l in live]
    room = max(0, rows - len(live))
    # The first two rows stay blank (the README's captions float over them and the terminal's top padding, so they
    # never cover a word); a conversation shorter than the room starts under them, a longer one shows its end.
    blank = min(BLANK_TOP, room)
    keep = room - blank
    top = [ESC + "K"] * blank + (body[-keep:] if keep else [])
    top += [ESC + "K"] * (room - len(top))
    return (top + live)[-rows:]


def check():
    """Every screen of every stand-in at the recording's size, each line's words within TEXT_WIDTH columns (the
    spinner's {SPIN} and {SECS} filled as agent.sh fills them, with three-digit seconds). Past that width a line may
    hold only the rules and backgrounds the alternate screen's preview cuts off. Prints each line that does not fit;
    exits 1 if there is one."""
    import re
    import unicodedata
    roles = {"claude": ["api", "data", "release", "changelog", "infra", "claude"], "codex": ["web", "review", "bench"]}
    bad = 0
    for kind, names in roles.items():
        for role in names:
            for state in ("working", "blocked", "idle"):
                if state == "blocked" and "ask" not in LIVE.get(role, {}):
                    continue
                for line in screen(kind, role, state, 110, 33):
                    plain = re.sub(r"\033\[[0-9;?]*[A-Za-z]", "", line).replace("{SPIN}", "·").replace("{SECS}", "100")
                    plain = plain.rstrip(" ─")
                    wide = sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in plain)
                    if wide > TEXT_WIDTH:
                        bad += 1
                        print("%s %s %s: %d columns: %s" % (kind, role, state, wide, plain))
    for name in sorted(os.listdir(os.path.join(HERE, "screens"))):
        for line in open(os.path.join(HERE, "screens", name), encoding="utf-8"):
            plain = line.rstrip("\n").replace("{SECS}", "100").rstrip()
            if len(plain) > TEXT_WIDTH:
                bad += 1
                print("screens/%s: %d columns: %s" % (name, len(plain), plain))
    return 1 if bad else 0


def main():
    if sys.argv[1:] == ["check"]:
        sys.exit(check())
    kind, role, state, cols, rows = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
    out = screen(kind, role, state, max(cols, 20), max(rows, 5))
    sys.stdout.write("\n".join(out))


if __name__ == "__main__":
    main()
