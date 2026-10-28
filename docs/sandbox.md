---
title: Agents in a sandbox
description: Run a coding agent in a container through a profile you can read. It limits accidents such as an rm -rf ~; it is not a wall. In beta.
---

# Agents in a sandbox

An agent in a sandbox runs in a container, so that what it can reach on your machine is what the
container is given. `taw sandbox`, typed in a pane, starts one there through a profile: a plain
script, of which taw ships one, `default`, that you can read line by line and copy. A sandbox
limits what an agent can reach by accident: an `rm -rf ~`, a write outside the project, a call to
a service on your machine. It is not a wall against an agent that is trying to get out, and
nothing taw prints says it is.

!!! note "The sandbox is in beta"
    It has been tried in Docker, but no real coding agent has been run inside it yet: the Tried
    column of the table in [Logging in, agent by agent](#logging-in-agent-by-agent) says so for
    each one.

## Starting one

```sh
taw sandbox [<agent>] [--profile <name>] [-- <the agent's own arguments>]
```

- `<agent>` is any agent taw knows (`taw manifest list`). Left out, it is `claude`.
- `--profile <name>` picks the sandbox: one of yours, or `default`. Left out, it is the agent's
    line in `[sandbox.agents]`, else `[sandbox] profile`, else `default`.

- The agent's own arguments go after `--`: `taw sandbox claude -- --model opus`. Everything
    before `--` is taw's.

It runs the agent in the pane you typed it in, in the foreground, until the agent exits, and you
are back at your shell in the same pane. It prints nothing of its own when it works (the profile
says what it is doing), and one plain line on stderr, with exit status 1, when it starts nothing:

- `taw sandbox: no profile called team (yours: default, max, in ~/.config/tmux-agent-wizard/sandbox/)`
- `taw sandbox: taw knows no agent called claud (taw manifest list names them)`
- `taw sandbox: the profile max is not executable (chmod +x <its path>)`, and the other faults of a
    file of yours in the words that name what to fix

- `taw sandbox: not typed at the prompt of pane %3 (it is not the job in front of that pane), so
    nothing was started: type it at the prompt of the pane it should take over (agents and scripts
    start one with taw agent start --sandbox)`, when it was not typed at the pane's prompt: an
    agent's tool call, a job sent to the background, or a terminal that took a pane's environment
    along. Marked, that pane would lose the session of the agent in front of it.

- `taw sandbox: config <file> did not load (<why>), so taw cannot tell which profile you chose:
    fix it (taw config check says what is wrong), or name one with --profile`, with no `--profile`
    and a config taw cannot read whole (also when it has a line in `[sandbox]` taw drops, or is a
    link to a file that is not there). With `--profile`, the config is not asked.

- A few name a broken setup in their own words: a `TMUX_PANE` that is no pane, a `TAW_STATE_DIR`
    that is not a full path.

A placement flag (`--split`, `--window`, `--session`, `--worktree`, `--here`, `--no-focus`) is not
one of those lines. It is refused as a usage error, in several lines (`Usage: taw sandbox ...`)
with exit status 2, and the tip `taw sandbox runs the agent in the pane it is typed in: make the
place first (prefix % or prefix " for a pane beside this one, prefix c for a window, the popup's n
or w for a session or a worktree), then type taw sandbox there`.

taw makes no pane, window or session for it. Make the place first, and type `taw sandbox` there:

| For the agent in | First | Then, in that pane |
|---|---|---|
| this pane | nothing | `taw sandbox` |
| a pane beside this one | tmux's split, `prefix + %` or `prefix + "` | `taw sandbox` |
| a new window | `prefix + c` | `taw sandbox` |
| a project's session | the popup's `n`, or `taw open <dir>` | `taw sandbox` |
| a branch of its own | the popup's `w`: a worktree and its session | `taw sandbox`, with git working inside |

The last is the one for an agent you leave to work alone: its changes land on a branch you
review, and two sandboxed agents never edit one checkout. In `~/work/api`, with the default
profile, the first time on this machine:

```
~/work/api $ taw sandbox claude
taw sandbox default: claude in docker, in ~/work/api
  reaches the internet and your local network, not this machine; no docker inside
  uses your own claude settings; runs commands without asking
building the image taw-sandbox-default (the first start only, a minute or two)
installing claude 2.1.286 and the tools in mise.toml (once for each version)
```

Then Claude's own screen. From the second start on, the three lines about the sandbox and then
Claude, within a few seconds. Outside tmux, `taw sandbox` runs the agent in that terminal anyway,
after one line: `taw sandbox: not in tmux, so this agent gets no popup row and no restore`; in tmux
but in no pane (a `run-shell`, say), the line is `taw sandbox: not in a tmux pane, so this agent
gets no popup row and no restore`. After a restart tmux can be back before Docker Desktop is, so
the default profile waits up to 60 seconds: `taw sandbox default: waiting for docker to answer
(start Docker Desktop, OrbStack or Colima)`.

The default also starts nothing in a folder the sandbox would share too much of, with a line of
its own:

- your home folder, or a folder that holds it: `taw sandbox default: ~ is your home folder, not a
    project, and the sandbox would share all of it; cd into a project first`;

- a folder in a git checkout of your home folder: `taw sandbox default: this folder is in a git
    checkout of your whole home folder, and the sandbox would share all of it; cd into a project with
    a git checkout of its own first`;

- tmux's socket folder, taw's state folder or taw's config folder, a folder inside one, or a folder
    that holds one: `taw sandbox default: ~/.config/tmux-agent-wizard is taw's config folder, and the
    sandbox could change your profiles and the hooks taw runs here; open a project outside it`;

- a folder that is or holds the folders of your programs and their settings (`~/.config`, `~/.local`).

For agents and scripts, `taw agent start <name> --kind <agent> --pane <id> --sandbox [--profile
<name>] [-- <args>]` types the same line into a pane at its prompt, waits until taw sees the agent
running in the container (300 seconds at most by default: a first start builds an image), names
it and prints its envelope. A profile that runs something taw cannot see fails with `timeout` and
says that a profile must end by running the runtime, with the agent as the container's program.

## In the popup

An agent that runs in a container has 📦 after its name, in the tree, the flat list and the
preview's title: `preview · ~/work/api · 📦 in a sandbox: docker, profile default`, or `docker, not
started by taw` for a container you started yourself, such as a shell function of yours. An agent
taw started through a profile, with no container runtime taw knows in front of it, has no 📦 and
no claim of one: the title names the profile, `sandbox profile <name>, no container seen`. 📦 is
there exactly while taw sees the container runtime run the agent in the pane (`@agent_container`):
nothing a program in the pane writes, a title or a window's name, ever draws it. It is a picture
and no word, so `/sandbox` finds nothing; its session's name finds it as it finds any row. It says all it claims, in the legend's words: in a container that taw started, or saw start;
nothing about how the container is set up. The popup offers nothing else for sandboxes: `n`, `w`
and `o` are as they are.

📦 is two columns wide, in tmux and in the popup. With the space before it, it takes 3 of the
name column's 10, so a name of eight letters or more loses its end (`openco… 📦`), and the place
column stays where it is on every row. A row too wide for the panel loses its end first: its
title, then, in the flat list, its session's name, then its state, then its place; the name and
📦 go last, and a cut through the emoji draws a space. A preview title too wide for the panel ends
in an ellipsis. Every Mac has the emoji; on Linux it needs an emoji font. A terminal that draws it
one column wide, where tmux counts two, leaves a blank column after it on that row.

## After a restart

`taw session restore` types the same command with the agent's resume arguments, in the common
case `taw sandbox claude -- --resume <id>`, with `--profile` when the profile was not the one the
config picks. The line starts `taw` when the pane's `PATH` finds this taw, else taw's path:
written `~/...` when it is under the pane's home, else in full. A word with a space or a shell
character in it is written in single quotes, so the pane's shell reads it as the one word it is.
A control character in a word (a newline, a tab, an escape) is written as a `$'\n'` escape after
the quote closes, here and in the line `taw agent start --sandbox` types, so such a word needs
bash, zsh or ksh in the pane: dash, the `sh` of Debian and Ubuntu, passes on `$\n` in its place.

An agent comes back in its conversation only when its own integration is installed (`taw
integration install <agent>`): the profile mounts a folder of the pane's, and the integration's
hook writes the session there from inside. taw reads it from outside, takes it only under that
agent's own rules, and never logs what it holds; nothing inside can reach tmux. Without an id the
pane comes back as a shell, as for any agent.

When restore cannot start the agent through its profile, the pane stays a shell and the plan says
why:

- `skip resume in pane %31: its sandbox profile team is gone`
- `skip resume in pane %31: its sandbox profile team is not executable (chmod +x <its path>)`, and
    any other fault of the profile in the words `taw sandbox` says it in

- `skip resume in pane %31: its claude ran in a docker container taw did not start, so it is not
    started again outside it`

- `skip resume in pane %31: its reported resume command does not start claude, and a sandboxed
    agent starts only through its profile`

A sandboxed agent is never started again outside its sandbox, and neither is an agent that ran in
a container taw did not start. When the typed line starts nothing, restore says so, `claude did
not start in its sandbox in pane %31: its shell is back at its prompt (taw pane read %31 shows what
the profile said); resume it with: <the line>`, and the pane is a shell like any other: up arrow
then Enter types the line again. While a restored sandbox is still starting (Docker coming up
after a reboot, an image building), a save keeps the conversation it is resuming, so a second
restart in that time still brings it back.

`sandbox.started` and `sandbox.stopped` (see "Hooks") fire when a pane's agent is seen running in a
container and when the container has left the pane; `TAW_SANDBOX` and `TAW_RUNTIME` are set on
every event about such a pane. The JSON of a pane, `taw agent list` and `taw agent get` carry
`"sandbox": {"profile": "default", "runtime": "docker"}`, and `null` outside one. `profile` is
`null` for a container taw did not start, and `runtime` is `null` while no container is seen.

## Profiles are plain scripts

Yours are executable files in `~/.config/tmux-agent-wizard/sandbox/` (`$TAW_CONFIG_DIR/sandbox/`
when that is set), each named for its profile: a letter, then letters, digits, `-` and `_`, ending
in a letter or digit. taw's own, `default`, is written to
`~/.local/state/tmux-agent-wizard/sandbox/default` whenever it runs it, so you can read exactly
what ran; a file of yours called `default` is used instead. Nothing is ever read from the project:
a project that could name its own sandbox could mount your home.
`taw sandbox new <name> [--from <profile>]` copies a profile, `default` unless named, into your
folder, executable, and prints its path; `taw sandbox list` prints the ones taw can run, with the
agents the config gives each, and the profiles it would run but refuses, with why: a `default` of
yours that cannot run (not executable, not a file, or a link to a file that is not there), or an
executable file whose `#!` line names its program by a path that is not a full one, or names one
you cannot run. Any other file that is not executable or has a bad name is in neither part of the
list: `taw config check` says it.
`taw config check` warns of a profile that `[sandbox]` names and that is not there or not
executable, of an agent it names that taw does not know or names by its command, and of a file in
your profiles folder that is not executable or whose name is no profile's name, in lines such as
`sandbox.profile: no profile called team (yours: default, max, in ~/.config/tmux-agent-wizard/sandbox/)`,
`sandbox.agents.claud: taw knows no agent called claud (taw manifest list names them)` and
`sandbox.agents.cursor-agent: taw calls this agent cursor, so its line is cursor = "max"`.

taw runs a profile in the agent's pane, in the project directory, as
`<profile> <the agent's command> [<its arguments>...]`, with five variables (three outside a tmux
pane, with no report folder and no pane):

| Variable | What it is |
|---|---|
| `TAW_SANDBOX_DIR` | the project directory, which the profile runs in |
| `TAW_SANDBOX_AGENT` | the agent as taw names it (`cursor` for `cursor-agent`) |
| `TAW_SANDBOX_REPORT` | a folder on this machine for the session report, emptied by taw before every start |
| `TAW_SANDBOX_PROFILE` | the profile's own name |
| `TAW_SANDBOX_PANE` | the tmux pane, for naming or labelling a container |

What follows the profile's name is the command line taw would have run on your machine
(`claude --resume <id>`, `codex resume <id>`, `cursor-agent`). A profile:

1. runs the agent with the arguments it was given; it may add its own, and never drops or changes
    them, because restore's `--resume <id>` arrives that way;

2. ends by becoming the runtime (`exec docker run ...`), with the agent as the program the
    container runs: that process is what taw sees and what draws 📦 (a profile that keeps a shell of
    its own in front gets no 📦 and no agent row, because taw sees `bash` there);

3. mounts the report folder and names the file and the agent inside, or restore brings back a
    shell: `-v "$TAW_SANDBOX_REPORT:/run/taw" -e TAW_REPORT_FILE=/run/taw/session.json -e
    TAW_REPORT_AGENT="$TAW_SANDBOX_AGENT"`; without the agent's name, any agent's hook inside (a
    copy of another agent, or one run as a tool) may write the session;

4. never gives the sandbox the tmux socket, `TMUX` or `TMUX_PANE`: whoever holds the socket can
    type into every pane you have;

5. ends its container when the pane closes (the default starts a small watcher for that);
6. says in a line or two what it is doing, before the agent starts.

A profile is a program of its own too: `~/.config/tmux-agent-wizard/sandbox/mine claude` runs
Claude in it from any shell, without a report folder, so without restore.
`TAW_SANDBOX_PRINT=1 <profile> <agent>` prints the command the default would run, and runs nothing.

## What `default` does

It runs the agent as you, in a container of a small image it builds once (`taw-sandbox-default`,
from mise's), with the agent installed inside at the version your machine runs (the latest, for an
agent with its own installer) and the tools of the project's `mise.toml`. An agent that mise
installs with npm (codex, gemini, copilot, amp, qwen, pi, letta) gets node's latest long-term
release when the project's `mise.toml` gives no node of its own, installed once and shared by every
project; the project's node, when it has one, is used. The project (in git, the whole checkout the folder is in) is mounted
read-write at its own path, with `.git/hooks` and `.git/config` read-only over it: a hook or a
config line the agent wrote there would run on your machine the next time you used git, and the
cost is that `git config` writes fail inside, so `git push -u` pushes but cannot record the
upstream. In a worktree, the repository's `.git` is mounted too, so git works. `node_modules` and
`.venv` get volumes of their own, at the project's top level always and wherever they are found
below it, five folders down at most: packages built inside are Linux builds, and yours are this
machine's. Docker makes the two folders at the top level, empty, in a checkout that has none; add
them to `.gitignore` if it does not list them already.

- **It runs without asking**, with each agent's own switch for that. Inside, a command it runs
    reaches only what the container is given; what it can still do without asking is change or
    delete anything in the project (git keeps the history), use your agent's settings and its login,
    and reach the internet and your local network.

- **Your agent's settings are shared**, at their paths, read-write: your settings, plugins, skills,
    instructions, history and memory are there inside, and a conversation started on one side resumes
    on the other. It also means the sandboxed agent can change them, and your agent outside uses them
    next: a hook or an MCP server it adds runs on your machine, outside any container.

- **The network: this machine is blocked, nothing else.** The agent reaches its own API, package
    registries, GitHub and your local network; it cannot reach anything running on this machine, by
    `localhost` or by its address. The rules are set inside before the agent starts, and if they
    cannot be set, the agent does not start.

- **No Docker inside**: `docker ps` there cannot connect. The project's Compose services are
    reachable over their own network (`psql -h db`).

Each of those is a recipe line in the script, commented out; uncomment it in your copy:

| Line | What it changes |
|---|---|
| `ask_first=yes` | the agent asks before each command, as it does outside |
| `settings=own` | a separate set of settings and a login of its own; for Claude, yours are there read-only (settings, instructions, skills, plugins and hooks) and your conversations read-write, and for another agent nothing of yours is |
| `block_local_network=yes` | your local network is blocked too, so it reaches only the internet |
| `open_ports="8317"` | those ports of this machine are open to it, a local model proxy on 8317 here |
| `docker=read-only` | `docker ps`, `logs` and `inspect` work, through a proxy beside the sandbox; `run`, `exec`, `stop` and `rm` are refused. Reading shows a lot: `docker inspect` shows every container's environment |
| `docker=full` | all of Docker. This is the same as giving it this whole machine: it can start a container that reads and writes your entire disk, so the sandbox then keeps nothing out. Inside, the agent gets the group the socket belongs to beside its own, and no other |
| `runtime=podman` | Podman instead of Docker |
| `tools="node@22 uv"` | tools beyond what the project's `mise.toml` installs |
| `pass="OPENAI_API_KEY GH_TOKEN"` | variables of yours, by name; their values come from your environment at start and are never written in the file |

## Logging in, agent by agent

Where an agent keeps its login decides its first start in a sandbox. One that keeps it as a file in
its shared folders is logged in inside as soon as you are outside. One that keeps it in the macOS
Keychain, as Claude does on a Mac, is not, because a container cannot read the Keychain: log in
once inside, and that login is then a file in your shared folder, while your agent outside keeps
its own. A key passed by name (`pass`) works for any agent.

| Agent | Installed inside by | Its folders, shared | To log in inside | Or pass | Runs without asking with | Tried |
|---|---|---|---|---|---|---|
| claude | mise (`claude`) | `~/.claude` and `~/.claude.json`, or `$CLAUDE_CONFIG_DIR` | `/login` once on a Mac (on Linux your login is already there) | `CLAUDE_CODE_OAUTH_TOKEN` (`claude setup-token`) or `ANTHROPIC_API_KEY` | `--dangerously-skip-permissions` | untested |
| codex | mise (`npm:@openai/codex`) | `$CODEX_HOME` or `~/.codex` | `codex login --device-auth` (device codes allowed in your ChatGPT security settings) | `OPENAI_API_KEY` | `--dangerously-bypass-approvals-and-sandbox` | untested |
| gemini | mise (`npm:@google/gemini-cli`) | `~/.gemini` | log in on your machine first; Google's login needs a browser to come back to the container | `GEMINI_API_KEY` | `--yolo` | untested |
| opencode | mise (`opencode`) | `~/.config/opencode` and `~/.local/share/opencode` | `opencode auth login` | the provider's own | `--auto` | untested |
| copilot | mise (`npm:@github/copilot`) | `$COPILOT_HOME` or `~/.copilot` | `/login`, a code you type at github.com/login/device | `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN` | `--allow-all` | untested |
| cursor | Cursor's installer | `$CURSOR_CONFIG_DIR` or `~/.cursor` | `cursor-agent login` | `CURSOR_API_KEY` | `--force` | untested |
| amp | mise (`npm:@sourcegraph/amp`) | `~/.config/amp` | `amp login` | `AMP_API_KEY` | `--dangerously-allow-all` | untested |
| qwen | mise (`npm:@qwen-code/qwen-code`) | `$QWEN_HOME` or `~/.qwen` | a key: Qwen's own login needs a browser | `OPENAI_API_KEY`, with `OPENAI_BASE_URL` and `OPENAI_MODEL` | `--yolo` | untested |
| droid | Factory's installer | `~/.factory` | the key | `FACTORY_API_KEY` | `--auto high` | untested |
| kimi | Kimi Code's installer | `$KIMI_CODE_HOME` or `~/.kimi-code` | `kimi login`, a code you type in a browser | | `--auto` | untested |
| pi | mise (`npm:@mariozechner/pi-coding-agent`) | `$PI_CODING_AGENT_DIR` or `~/.pi/agent` | `/login` | the provider's own | nothing to add: pi never asks | untested |
| letta | mise (`npm:@letta-ai/letta-code`) | `~/.letta` | the key | `LETTA_API_KEY` | `--yolo` | untested |

The default refuses any other agent with its line, `taw sandbox default: no recipe for qodercli;
add one to a copy (taw sandbox new)`; a recipe is one entry of the script, and `taw skill` tells an
agent how to add one with you. Two accounts, as a shell function of yours may keep: a copy of the
default named after one sets `CLAUDE_CONFIG_DIR=~/.claude-work` in a line beside its recipe lines,
and `taw sandbox --profile work` starts that account's Claude. Podman is the `runtime=podman` line.
Apple's `container` and Docker's `sbx` are not shipped: a profile of yours that ends in
`exec container run ... claude` or `exec sbx run claude ...` gets 📦 all the same. sbx brings its
own small VM and its own credential proxy and shares only the project, so restore can start it
again but has no conversation id to resume.

## Limits

- The internet is open. Anything inside, the agent's login included, can be read and sent out.
- With shared settings, the sandboxed agent can change what your agent outside runs.
- The default needs Docker (Docker Desktop, OrbStack, Colima or Docker Engine), or Podman by its
    line, and the first start builds an image and installs the agent, which takes a minute or two.

- 📦 says a container was seen. It does not say the container is set up well: a profile can mount
    `/`.
