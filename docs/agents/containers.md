---
title: Agents in containers
description: Coding agents started in Docker, Podman, nerdctl, Apple's container or sbx from a tmux pane are recognised by tmux-agent-wizard with nothing set.
---

# Agents in containers

An agent started inside a container from a pane is recognised. When the pane's foreground
program is Docker, Podman, nerdctl, Apple's `container` or `sbx`, `taw` reads the arguments
it was started with, once for each process that comes to the front of the pane, and looks
for the agent as the program the container runs: the word after the image
(`docker run ... image claude`, `podman run ... image codex`), the word after `run` in
`sbx run claude`, or a command of the script a shell in the container runs, as in
`--entrypoint bash image -lc 'mise install; claude --effort medium'`. It has to be the
agent's own executable, and only there. An image called `claude`, a container or a mounted
directory that happens to be called `claude`, the value of a flag (`--name claude`,
`-e AGENT=claude`), `.claude` in a path, an argument of another program
(`npm install claude-code`), `echo "claude"` and a comment (`# then claude`) in a script are
not the agent. A script is read as a shell reads it: a comment holds no command, whatever
quote or `;` is in it, and a line that ends in a backslash goes on with the next.

The arguments are read exactly as the process was given them, from the system itself on
macOS and Linux, so a script over several lines, a database URL with an `&` in a flag or a
value with spaces in it cannot hide the agent. Where the system will not say (a process of
another user, another operating system), `taw` reads the line `ps` prints instead. `ps`
joins the arguments with spaces, so there a flag's value with a space in it can hide the
agent.

From then on the pane is that agent's pane. Its state is read from the screen with the
agent's own rules, so the popup row, the toasts and next up work as they do for an agent on
the host. The pane also carries `@agent_container`, set to `docker`, `podman`, `nerdctl`,
`container` or `sbx`, for a status line or a pane border that wants to say where the agent
runs (`#{@agent_container}`). The mark goes when the runtime is no longer in front of the
pane, which is when the container stops. The agent's row goes when the shell is back,
usually at the same moment. The runtime's arguments name the agent as its program would
if it ran from a prompt, so it follows the same exit rules as that one (see above): the shell
back in front of it ends its record, and so does another program in front of it, after
about half a minute, whatever its hook reported. The arguments can hold credentials, so
`taw` reads them only to find that name, and keeps and logs nothing of them.

A pane `taw sandbox` started comes back through its profile when you run `taw session restore`:
it types `taw sandbox` with the agent's resume arguments, so the agent resumes its conversation in
a new container, never outside one (see "Agents in a sandbox"). A save while that restored sandbox
is still starting (its profile waiting for the runtime, or building the image) keeps the
conversation it is resuming.

Three limits. An agent in a container taw did not start (one you typed by hand) comes back as a
plain shell, its conversation not resumed, and you start the container again. Only the command
line is read: an agent you start by typing `claude` in a
shell you entered with `docker exec -it box bash` is not found, because nothing in the
arguments names it. And a container whose script carries on after its agent quits (`claude;
bash`, or an entrypoint that keeps the container up) keeps reading as that agent, idle, until
the runtime exits, since the arguments still name it and the screen is all that is left to
read.
