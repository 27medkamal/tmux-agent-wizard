---
name: An agent shows the wrong state
about: working when it is idle, done when it is still busy, not recognised at all
labels: detection
---

**Which agent, and which version** (for example Claude Code 2.1.241)

**What taw shows, and what the agent is really doing**

**What matched** (run this on the pane and paste the output):

```sh
taw agent explain <pane>
```

**The bottom of the pane's screen**, as text if you can (`tmux capture-pane -p -t <pane> | tail -20`), with anything private removed.

- taw version (`taw --version`):
- tmux version (`tmux -V`):
