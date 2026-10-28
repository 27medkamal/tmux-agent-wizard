//! Switch to, or create, a tmux session for a directory: canonicalises the path, runs the
//! configured pre/post create hooks, and best-effort teaches `zoxide` about the directory.
//! Backs the popup's "new session" project picker and `taw session open <path>`.
//!
//! Order mirrors tmux-session-wizard: pre-create hook (on every open, before a running
//! session is looked for), `new-session -d -s <name> -c <path>`, `zoxide add <path>` when
//! `[sessions] zoxide` is on, post-create hook (only for a session just created).
//! `switch-client` is deliberately not run here — the caller decides whether, and to what,
//! it switches.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::config::Config;
use crate::tmux::{SessionInfo, Tmux};

use super::{naming, projects, worktree};

/// What [`open_or_create`] did: the session's real name (tmux's own answer, after any
/// pre-hook rename and tmux's `.`/`:` folding), whether it was newly created, and the
/// human-readable collision note [`naming::session_name_for`] produced, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub session_name: String,
    pub created: bool,
    pub collision: Option<String>,
}

/// Switches to, or creates, the session for `path` (tilde or absolute, resolved against
/// `home`).
///
/// `path` is canonicalised first (a leading `~` expanded via [`naming::expand_tilde`], then
/// `std::fs::canonicalize`); a directory that does not exist is refused with `no such
/// directory: <path>` (`path` as given, not the expanded form). A name is proposed
/// ([`naming::session_name_for`]) and the pre-create hook, when there is one, is run with it
/// and the directory. It runs on every open, before any session is looked for, so it is
/// asked whether or not one is already running there. What it prints replaces the name
/// (first line) and the directory (second line, which has to exist); a non-zero exit stops
/// the open, with nothing created and nothing looked for, and is reported as a
/// [`HookFailure`]. So is a hook that runs longer than `[sessions] hook_timeout_seconds`
/// ([`hook_limit`]): it is stopped, and the open is refused.
///
/// Then a running session is looked for. When the hook left the name and the directory as
/// they were, it is the one whose `session_path` names the directory ([`find_running`]),
/// whatever it is called, so a session renamed by hand is still found. When the hook chose a
/// name or a directory, it is the session called that, as tmux-session-wizard finds it: the
/// hook's answer wins over where a session happens to run. A session found is reported
/// as-is: no `new-session`, no `zoxide add`, no post-create hook.
///
/// Otherwise `new-session -d -s <name> -c <dir>` creates it; `zoxide add <path>` runs when
/// `[sessions] zoxide` is on; the post-create hook runs, told the name and directory the
/// session was made with. A failing post-create hook is logged, not propagated: the
/// session it ran after stays created either way, and that includes one that was stopped for
/// running too long. Never runs `switch-client`; that is always the caller's call.
///
/// With no tmux server running there is no session to find, so the open goes on to the
/// `new-session`, which starts the server ([`running_sessions`]).
pub fn open_or_create(t: &Tmux, cfg: &Config, path: &str, home: &Path) -> anyhow::Result<Outcome> {
    let expanded = naming::expand_tilde(path, home);
    if !expanded.is_dir() {
        anyhow::bail!("no such directory: {path}");
    }
    let abs_path = std::fs::canonicalize(&expanded)
        .with_context(|| format!("canonicalizing {}", expanded.display()))?;

    let mut sessions = running_sessions(t)?;
    let tilde_path = naming::tilde(&abs_path, home);
    let existing = existing_from_sessions(&sessions, home);
    let proposed = naming::session_name_for(&tilde_path, cfg.sessions.name, &existing);

    // `[sessions] pre_create_hook`/`post_create_hook` are kept as written by config.rs; a
    // leading `~` is expanded against `home` when the hook is run (see [`hook_script`]).
    // The pre hook is given the name a new session here would get, and the directory that
    // was picked, every time, and its answer is what the rest of the open goes by. Each of the
    // two hooks has the same time to finish.
    let limit = hook_limit(cfg.sessions.hook_timeout_seconds);
    let mut name = proposed.name.clone();
    let mut dir = abs_path.clone();
    if let Some(command) = hook_command(cfg.sessions.pre_create_hook.as_ref()) {
        let reply = run_hook(Hook::Pre, command, home, &name, &abs_path, limit)?;
        if let Some(line) = reply.name {
            name = naming::tmux_name(&line);
        }
        if let Some(line) = reply.dir {
            dir = hook_dir(&line, home)?;
        }
        // A hook is free to make or remove sessions itself, so look again: tmux-session-wizard
        // asks tmux whether the session exists only after its hook has answered.
        sessions = running_sessions(t)?;
    }

    let hook_chose = name != proposed.name || dir != abs_path;
    let running = if hook_chose {
        sessions.iter().find(|s| s.session_name == name)
    } else {
        find_running(&sessions, &dir)
    };
    if let Some(found) = running {
        return Ok(Outcome {
            session_name: found.session_name.clone(),
            created: false,
            collision: None,
        });
    }

    let dir_str = worktree::path_str(&dir)?;
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        &crate::tmux::new_name(&name),
        "-c",
        &crate::tmux::start_directory(dir_str),
    ])?;

    // The directory that was picked, not the one a hook moved the session to: what
    // zoxide ranks is where the person went, as tmux-session-wizard does.
    if cfg.sessions.zoxide {
        projects::zoxide_add(&abs_path);
    }

    if let Some(command) = hook_command(cfg.sessions.post_create_hook.as_ref())
        && let Err(err) = run_hook(Hook::Post, command, home, &name, &dir, limit)
    {
        log::error!("{err:#}; session '{name}' was created all the same");
    }

    // The collision note explains the name taw proposed; a name the hook chose is not it.
    let collision = if name == proposed.name {
        proposed.collision
    } else {
        None
    };
    Ok(Outcome {
        session_name: name,
        created: true,
        collision,
    })
}

/// The sessions tmux has, or none when there is no tmux server to ask: the `new-session`
/// that creates this one is what starts it. `taw open` from a plain terminal, with the last
/// session closed, is the first to need that; the popup and `taw session open` always find a
/// server. Any other reason the listing failed still stops the open.
fn running_sessions(t: &Tmux) -> anyhow::Result<Vec<SessionInfo>> {
    match t.list_sessions() {
        Err(err) if crate::tmux::no_server(&err) => Ok(Vec::new()),
        listed => listed,
    }
}

/// The hook command line to actually run, if any.
///
/// `[sessions] pre_create_hook = ""` is the form the key table documents as the default,
/// and an empty command is not a program: run, it would fail, and for the pre-create hook
/// that failure aborts the session the user asked for. So an empty, or all-whitespace,
/// value means "no hook", exactly as leaving the key out does. A value that has content is
/// used as written; only the emptiness test trims.
fn hook_command(hook: Option<&String>) -> Option<&str> {
    hook.map(String::as_str)
        .filter(|command| !command.trim().is_empty())
}

/// The shell a hook value is run by: an absolute path, so which shell it is does not
/// depend on `$PATH`.
const SHELL: &str = "/bin/sh";

/// Which hook is being run: what a failure calls it, and whether what it prints is an
/// answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    /// `[sessions] pre_create_hook`: runs before the session is made, and may rename it or
    /// move it to another directory.
    Pre,
    /// `[sessions] post_create_hook`: runs once the session exists; what it prints is dropped.
    Post,
    /// `[hooks] pre_close`: runs before taw closes a session, a window or a pane. What it
    /// prints is dropped, and a failure refuses the close.
    PreClose,
    /// `[hooks] pre_worktree_remove`: runs before taw removes a worktree, and a failure
    /// refuses the removal.
    PreWorktreeRemove,
}

impl Hook {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Hook::Pre => "pre-create hook",
            Hook::Post => "post-create hook",
            Hook::PreClose => "pre-close hook",
            Hook::PreWorktreeRemove => "pre-worktree-remove hook",
        }
    }
}

/// Why a hook did not finish well. A [`Hook::Pre`]'s failure stops the open; a
/// [`Hook::Post`]'s is logged and changes nothing; a [`Hook::PreClose`]'s or a
/// [`Hook::PreWorktreeRemove`]'s refuses the close or the removal. `taw session open` (and
/// the verbs that close things) report it with the error code `hook_refused` (see
/// `cli::envelope::code_for`), which is why it is a type of its own and not a message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HookFailure {
    /// The hook exited non-zero, or was ended by a signal. `stderr` is what it wrote there,
    /// trimmed (the last [`HELD_BYTES`] of it, for a `[hooks]` command that wrote more):
    /// empty when it wrote nothing, and for a post hook, whose output goes to /dev/null.
    #[error("{}", exit_message(*.hook, .command, *.status, .stderr))]
    Exited {
        hook: Hook,
        command: String,
        status: ExitStatus,
        stderr: String,
    },
    /// The hook, or a process it started that still held its output, was running when
    /// `[sessions] hook_timeout_seconds` ran out, and was stopped.
    #[error("{} took longer than {} s", .hook.label(), .limit.as_secs_f64())]
    TimedOut { hook: Hook, limit: Duration },
}

impl HookFailure {
    /// The failure in one line, for a place with room for one: the popup's toast. That is
    /// the last line the hook wrote to stderr, since a script says its verdict last, behind
    /// the name of the hook; or, when it wrote nothing, its exit status. A hook that ran out
    /// of time has one line to say already. What the error prints ([`std::fmt::Display`]) is
    /// longer for a hook that exited: it names the command and carries the stderr that was kept.
    pub fn summary(&self) -> String {
        match self {
            HookFailure::Exited {
                hook,
                status,
                stderr,
                ..
            } => {
                let label = hook.label();
                match stderr
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                {
                    Some(line) => format!("{label}: {line}"),
                    None => format!("{label} refused ({})", status_words(*status)),
                }
            }
            HookFailure::TimedOut { .. } => self.to_string(),
        }
    }
}

/// The message of [`HookFailure::Exited`]: the hook and its command, how it ended, and what
/// it wrote to stderr when it wrote anything.
fn exit_message(hook: Hook, command: &str, status: ExitStatus, stderr: &str) -> String {
    let label = hook.label();
    if stderr.is_empty() {
        format!("{label} {command} failed ({status})")
    } else {
        format!("{label} {command} failed ({status}): {stderr}")
    }
}

/// How a hook ended, in the few words a one-line summary has room for.
pub(crate) fn status_words(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// The command line a hook value stands for: the value as written and trimmed, with `home`
/// in place of a leading `~/`. The home directory is quoted, so one with a space or a quote
/// in it is still one word; a tilde anywhere else is the shell's own to expand. The value is
/// trimmed because a trailing newline would end the command early. Shared by the create
/// hooks, which add arguments to it ([`hook_script`]), and by the `[hooks]` commands, which
/// take none ([`run_command`]).
pub(crate) fn command_line(value: &str, home: &Path) -> String {
    let value = value.trim();
    match (value.strip_prefix("~/"), home.to_str()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", shell_words::quote(home)),
        _ => value.to_string(),
    }
}

/// The script `sh -c` runs for a hook value: the command line ([`command_line`]), then
/// `"$@"`, which the shell replaces with the two arguments taw passes after the script.
///
/// Those arguments are handed to the shell as arguments of its own and never spliced into
/// the script, so however the session or the directory is spelled, it reaches the hook as
/// exactly those characters: a directory called `$(rm -rf ~)` is a directory called that.
fn hook_script(value: &str, home: &Path) -> String {
    format!("{} \"$@\"", command_line(value, home))
}

/// What a pre-create hook printed: a session name on its first line and a directory on its
/// second, each only when its line has something on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookReply {
    pub name: Option<String>,
    pub dir: Option<String>,
}

/// The two lines a pre-create hook may print, exactly as printed: only the line ending is
/// dropped, so a hook that prints back what it was given, spaces and all, says nothing
/// new. A line with nothing on it, or nothing but whitespace, says nothing either, so a
/// hook that means to change only the directory can leave the name line blank. Anything
/// after the second line is ignored.
fn parse_reply(stdout: &str) -> HookReply {
    let mut lines = stdout.lines();
    let mut next = || {
        lines
            .next()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
    };
    HookReply {
        name: next(),
        dir: next(),
    }
}

/// Runs a hook: `command`, a command line (a path, or a path with flags of its own), through
/// `sh -c '<command> "$@"' sh <session> <dir>`, expanded the way a shell expands a command
/// typed at a prompt, as tmux-session-wizard's `eval` does (see [`hook_script`]). A hook gets
/// no standard input, and taw shows nothing it prints except a refusal's stderr. A hook that
/// opens the terminal itself, as fzf does, can prompt from `taw session open` but not inside
/// the popup.
///
/// A non-zero exit is an `Err` holding a [`HookFailure`], for either hook. What each one
/// prints is the difference between them (`kind`). A [`Hook::Pre`]'s stdout is its answer,
/// returned as a [`HookReply`] ([`parse_reply`]), and its stderr is kept for the failure a
/// non-zero exit raises. A [`Hook::Post`]'s stdout and stderr go to `/dev/null`, as
/// tmux-session-wizard sends them, and are not captured: a process the hook leaves running
/// inherits them, so pipes that were read to the end would keep the open waiting, and the
/// switch that follows it from ever happening, for as long as that process lived.
///
/// `limit` is how long the hook may take (`None` for as long as it likes). A hook still
/// running when it is up is stopped, along with what it started, and is an `Err` holding a
/// [`HookFailure::TimedOut`]. A pre hook is not done until the pipes it writes to are closed
/// as well as the hook ended, so a process it leaves behind that holds them counts against
/// the limit too; the caller is not held up by that process past it.
pub fn run_hook(
    kind: Hook,
    command: &str,
    home: &Path,
    session: &str,
    dir: &Path,
    limit: Option<Duration>,
) -> anyhow::Result<HookReply> {
    let label = kind.label();
    let mut shell = Command::new(SHELL);
    shell
        .arg("-c")
        .arg(hook_script(command, home))
        .arg("sh")
        .arg(session)
        .arg(dir);
    let streams = if kind == Hook::Pre {
        Streams::ANSWER
    } else {
        Streams::NEITHER
    };
    let ran = run_within(shell, streams, limit, None)
        .with_context(|| format!("running the {label} {command}"))?;
    let ended = require_success(kind, command, ran)?;
    Ok(match kind {
        Hook::Pre => parse_reply(&String::from_utf8_lossy(&ended.stdout)),
        Hook::Post | Hook::PreClose | Hook::PreWorktreeRemove => HookReply::default(),
    })
}

/// What a `[hooks]` command is started with, on top of the environment the process that starts
/// it has: variables to set, and variables to take out. tmux leaves a variable out of a new
/// pane's environment when it is told to (`set-environment -r`), and a process that fired an
/// event may still have it.
///
/// The values may be secrets: [`std::fmt::Debug`] shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HookEnv {
    /// Variables to set, replacing any of the same name the process has.
    pub set: Vec<(String, String)>,
    /// Variables to take out of what the process has.
    pub unset: Vec<String>,
}

impl std::fmt::Debug for HookEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEnv")
            .field(
                "set",
                &self.set.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("unset", &self.unset)
            .finish()
    }
}

impl HookEnv {
    /// Sets and takes out these on `command`, on top of what it inherits, which is without
    /// [`crate::plugin::env::PANE_CONTEXT_VARS`]: a hook's pane is named by its own `TAW_PANE`
    /// and the rest, and `HERDR_PANE_ID=%calling` from tmux's global environment (what
    /// `@agent-wizard-herdr-env` sets) is no pane of the event's.
    pub fn apply(&self, command: &mut Command) {
        crate::plugin::env::strip_pane_context_env(command);
        command.envs(self.set.iter().map(|(name, value)| (name, value)));
        for name in &self.unset {
            command.env_remove(name);
        }
    }

    /// The value of `name`, when it is one of the variables set.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.set
            .iter()
            .find(|(own, _)| own == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Runs a `[hooks]` command: `command` through `sh -c` as a command line ([`command_line`]),
/// with no arguments, `env` applied to the environment it inherits, `input` on its standard
/// input, and `dir` as its working directory when there is one. The time limit, and what
/// happens to a hook that outlives it, are [`run_hook`]'s: stopped along with what it
/// started, reported as [`Ran::OutOfTime`]. The streams `streams` keeps are read to their
/// ends, so a process the command leaves running that still holds one counts against the
/// limit; one that is kept nowhere goes to `/dev/null` and is waited for by nobody.
///
/// Nothing the environment or the input carries is part of the command line, which is the
/// point of them: a title that says `$(rm -rf ~)` reaches the hook as a title.
///
/// A hook that never reads its input is no problem, however much there is: the input is
/// written on a thread of its own, which ends when the hook does.
///
/// A `dir` the shell cannot be started in (one it may not enter, or one that went away since it
/// was looked at) does not lose the hook: it starts in `home` instead, and where this process is
/// when that cannot be entered either ([`starting_places`]).
pub(crate) fn run_command(
    command: &str,
    home: &Path,
    env: &HookEnv,
    input: &[u8],
    dir: Option<&Path>,
    limit: Option<Duration>,
    streams: Streams,
) -> std::io::Result<Ran> {
    let shell = |dir: Option<&Path>| {
        let mut shell = Command::new(SHELL);
        shell.arg("-c").arg(command_line(command, home));
        env.apply(&mut shell);
        if let Some(dir) = dir {
            shell.current_dir(dir);
        }
        shell
    };
    starting_places(dir, home)
        .into_iter()
        .find_map(
            |place| match run_within(shell(Some(place)), streams, limit, Some(input)) {
                Err(err) if cannot_start_in_a_directory(&err) => None,
                other => Some(other),
            },
        )
        .unwrap_or_else(|| run_within(shell(None), streams, limit, Some(input)))
}

/// The directories a hook that was asked to start in `dir` is tried in, best first: `dir`, then
/// `home` when `dir` cannot be entered or is gone. A caller that has no directory for it starts
/// it where its own process is, and so does one whose places all fail. So the same hook starts
/// in the same place whichever process fired the event, and not in the working directory of
/// whichever it was.
pub(crate) fn starting_places<'a>(dir: Option<&'a Path>, home: &'a Path) -> Vec<&'a Path> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let mut places = vec![dir];
    if home != dir && home.is_dir() {
        places.push(home);
    }
    places
}

/// Whether `err`, from starting a process with a working directory set, is about the directory:
/// tmux panes and worktrees can be left in places a hook may not enter, or that are gone.
pub(crate) fn cannot_start_in_a_directory(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
    )
}

/// How a hook ended, as a result: what it wrote when it ended well, and a [`HookFailure`]
/// when it exited non-zero, was ended by a signal, or was stopped for running out of time.
pub(crate) fn require_success(kind: Hook, command: &str, ran: Ran) -> Result<Ended, HookFailure> {
    match ran {
        Ran::OutOfTime(limit) => Err(HookFailure::TimedOut { hook: kind, limit }),
        Ran::Ended(ended) if !ended.status.success() => Err(HookFailure::Exited {
            hook: kind,
            command: command.to_string(),
            status: ended.status,
            stderr: String::from_utf8_lossy(&ended.stderr).trim().to_string(),
        }),
        Ran::Ended(ended) => Ok(ended),
    }
}

/// `[sessions] hook_timeout_seconds` as a limit: `None` for `0`, which means no limit, and for
/// a number that is no length of time (a negative one, or one too big to be a time), which
/// `taw config check` reports as out of range.
pub(crate) fn hook_limit(seconds: f64) -> Option<Duration> {
    if seconds > 0.0 {
        Duration::try_from_secs_f64(seconds).ok()
    } else {
        None
    }
}

/// A hook's shell after it ended: how, and what it wrote when that was captured.
#[derive(Debug)]
pub(crate) struct Ended {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether what came before `stdout` was dropped: the stream is longer than [`Keep::Last`]
    /// holds.
    pub stdout_cut: bool,
    /// The same for `stderr`.
    pub stderr_cut: bool,
}

/// What became of a hook's shell: it ended on its own, or it was still going when the limit
/// (carried here) ran out and it was stopped.
#[derive(Debug)]
pub(crate) enum Ran {
    Ended(Ended),
    OutOfTime(Duration),
}

/// How much of what a hook writes to one of its streams is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// None of it: the stream goes to `/dev/null`, so nothing reads it and nothing waits for it
    /// to close.
    Nothing,
    /// All of it, however much there is: a pre-create hook's stdout is its answer.
    All,
    /// The last so many bytes. What came before is read and dropped as it arrives, so a hook that
    /// writes without end fills no memory and is never left waiting for a reader; the end is
    /// where a verdict is.
    Last(usize),
}

impl Keep {
    fn stdio(self) -> Stdio {
        match self {
            Keep::Nothing => Stdio::null(),
            Keep::All | Keep::Last(_) => Stdio::piped(),
        }
    }
}

/// What is kept of each of a hook's two streams, and how long they are waited for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Streams {
    pub stdout: Keep,
    pub stderr: Keep,
    /// How long the streams that are kept are still waited for once the shell has ended. They
    /// close when the last process holding them ends, and a process the shell left running holds
    /// them for as long as it lives. `None` waits for as long as the time limit allows, which
    /// is what a background hook and a create hook do; `Some` answers that long after the shell
    /// ends with what has been read, for a caller that has somebody waiting.
    pub after_exit: Option<Duration>,
}

/// How long a hook that can refuse is still read for after its shell has ended: long enough for
/// what the shell wrote to be read, short enough that a process it left behind costs the person
/// waiting a moment and not the whole time limit.
const AFTER_EXIT: Duration = Duration::from_millis(300);

/// How much of a `[hooks]` command's output is kept when a caller has a use for it: what a
/// refusal quotes, what the hooks log excerpts and what `taw hooks test` shows.
pub(crate) const HELD_BYTES: usize = 16 * 1024;

impl Streams {
    /// A pre-create hook's: stdout is its answer, and stderr the reason its refusal gives.
    const ANSWER: Streams = Streams {
        stdout: Keep::All,
        stderr: Keep::All,
        after_exit: None,
    };
    /// A post-create hook's: both go to `/dev/null`, as tmux-session-wizard sends them.
    const NEITHER: Streams = Streams {
        stdout: Keep::Nothing,
        stderr: Keep::Nothing,
        after_exit: None,
    };
    /// The end of stderr, where a failure says why, and nothing of stdout: a background hook,
    /// whose supervisor has all the time the limit gives it.
    pub(crate) const BACKGROUND: Streams = Streams {
        stdout: Keep::Nothing,
        stderr: Keep::Last(HELD_BYTES),
        after_exit: None,
    };
    /// The same for a hook that can refuse, which somebody is waiting for: what its shell wrote
    /// is read, and a process it left running is not waited for ([`AFTER_EXIT`]).
    pub(crate) const VETO: Streams = Streams {
        stdout: Keep::Nothing,
        stderr: Keep::Last(HELD_BYTES),
        after_exit: Some(AFTER_EXIT),
    };
    /// The end of both, for the caller that shows what a hook printed: `taw hooks test`, which
    /// waits as a background hook's supervisor does.
    pub(crate) const SHOWN: Streams = Streams {
        stdout: Keep::Last(HELD_BYTES),
        stderr: Keep::Last(HELD_BYTES),
        after_exit: None,
    };
    /// [`Streams::SHOWN`] for a hook that can refuse, which `taw hooks test` reads as a real close
    /// does ([`Streams::VETO`]): it is answered when its shell ends.
    pub(crate) const SHOWN_VETO: Streams = Streams {
        after_exit: Some(AFTER_EXIT),
        ..Streams::SHOWN
    };
}

/// How long [`run_within`] waits before it looks again: this at first, doubling to
/// [`LONGEST_NAP`], so a hook that ends in a moment is not kept waiting for a look.
const FIRST_NAP: Duration = Duration::from_millis(1);
const LONGEST_NAP: Duration = Duration::from_millis(20);

/// Runs `shell` with `input` on its standard input (none at all when there is no input), and
/// waits for it to end for at most `limit`. The streams `streams` keeps have their ends read,
/// and waited for as well as the shell: they close when the last process holding them ends,
/// which may be later than the shell. The rest go to `/dev/null`, and only the shell is waited
/// for.
///
/// Out of time, the shell and what it started are stopped ([`stop`]) and nothing more is
/// waited for, however long a process that escaped that keeps the pipes open. A shell that has
/// ended is not stopped: when the streams are still open at the limit only because a process it
/// left running holds them, that is reported as out of time too, and the process is left
/// running, unless `streams` says to stop waiting for them sooner ([`Streams::after_exit`]).
///
/// The input is written on a thread of its own and the pipe closed after it, so a shell that
/// reads none of it, or all of it late, does not hold this up: the thread ends when the shell
/// has read it, or when the shell is gone.
fn run_within(
    mut shell: Command,
    streams: Streams,
    limit: Option<Duration>,
    input: Option<&[u8]>,
) -> std::io::Result<Ran> {
    shell.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    shell
        .stdout(streams.stdout.stdio())
        .stderr(streams.stderr.stdio());
    let mut child = shell.spawn()?;
    if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
        let input = input.to_vec();
        std::thread::spawn(move || {
            // A hook that ended, or never reads, closes the pipe under this: not an error.
            let _ = pipe.write_all(&input);
        });
    }
    let stdout = Drain::start(child.stdout.take(), streams.stdout);
    let stderr = Drain::start(child.stderr.take(), streams.stderr);
    let deadline = limit.map(|limit| (Instant::now() + limit, limit));
    let mut status = None;
    let mut ended_at = None;
    let mut nap = FIRST_NAP;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
            if status.is_some() {
                ended_at = Some(Instant::now());
            }
        }
        if let Some(status) = status {
            let closed = stdout.is_read() && stderr.is_read();
            // The shell's own output was written before it ended, and is read at once: what
            // is still open after the grace is a process it left behind.
            let given_up = streams
                .after_exit
                .zip(ended_at)
                .is_some_and(|(grace, ended)| ended.elapsed() >= grace);
            if closed || given_up {
                return Ok(Ran::Ended(Ended {
                    status,
                    stdout: stdout.kept(),
                    stderr: stderr.kept(),
                    stdout_cut: stdout.was_cut(),
                    stderr_cut: stderr.was_cut(),
                }));
            }
        }
        if let Some((deadline, limit)) = deadline
            && Instant::now() >= deadline
        {
            // A shell that has ended is already reaped, and its pid may be somebody else's by
            // now: only one that has not is stopped.
            if status.is_none() {
                stop(child);
            }
            return Ok(Ran::OutOfTime(limit));
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(LONGEST_NAP);
    }
}

/// A pipe being read on a thread of its own, so that a hook that writes more than a pipe holds
/// is not left waiting for somebody to read it. What is kept of it is [`Keep`]'s.
struct Drain {
    kept: Arc<Mutex<Vec<u8>>>,
    /// Whether the pipe has been read to its end, or was never one.
    done: Arc<AtomicBool>,
    /// Whether something was dropped: there was more than [`Keep::Last`] keeps.
    cut: Arc<AtomicBool>,
}

/// Sets a flag when it goes, whichever way its thread ends.
struct SetWhenDropped(Arc<AtomicBool>);

impl Drop for SetWhenDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Drain {
    /// Starts reading `pipe`; `None`, a stream that was not captured, is already read and empty.
    fn start<R: Read + Send + 'static>(pipe: Option<R>, keep: Keep) -> Drain {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(pipe.is_none()));
        let cut = Arc::new(AtomicBool::new(false));
        if let Some(mut pipe) = pipe {
            let (kept, done, cut) = (
                Arc::clone(&kept),
                SetWhenDropped(Arc::clone(&done)),
                Arc::clone(&cut),
            );
            std::thread::spawn(move || {
                let _done = done;
                let mut chunk = [0u8; 8192];
                loop {
                    let read = match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    let mut kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
                    kept.extend_from_slice(&chunk[..read]);
                    if let Keep::Last(most) = keep
                        && kept.len() > most
                    {
                        let excess = kept.len() - most;
                        kept.drain(..excess);
                        cut.store(true, Ordering::Release);
                    }
                }
            });
        }
        Drain { kept, done, cut }
    }

    /// Whether the pipe has been read to its end.
    fn is_read(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Whether some of it was dropped, so far.
    fn was_cut(&self) -> bool {
        self.cut.load(Ordering::Acquire)
    }

    /// What is kept of it so far: all there is once [`Drain::is_read`].
    fn kept(&self) -> Vec<u8> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// How many times [`stop`] looks again, after stopping what it found, for what the hook
/// started in the meantime.
const FREEZE_ROUNDS: usize = 5;

/// Stops a hook's shell and what it started, which are still running.
///
/// The shell is not in a process group of its own, so there is no one call that ends its tree:
/// a group would no longer be in the foreground of the terminal, and a hook that opens the
/// terminal to ask a question, as fzf does, would be stopped the moment it read from it. The
/// tree is listed instead, and a tree that is being listed can grow: a process that forks
/// after the list is taken escapes the kill. So each process found is frozen (SIGSTOP), which
/// cannot fork, the shell first as it is the one that starts the rest, and the list is taken
/// again until it has nothing new. Only then is everything killed.
///
/// A process that left the shell's tree before this (one it started and left running, once
/// the shell itself had gone) is not found. The shell is reaped on a thread of its own, so a
/// process that will not die cannot hold the caller up.
fn stop(mut shell: Child) {
    let root = shell.id();
    signal(libc::SIGSTOP, &[root]);
    let mut below: Vec<u32> = Vec::new();
    for _ in 0..FREEZE_ROUNDS {
        let found: Vec<u32> = descendants(root)
            .into_iter()
            .filter(|pid| !below.contains(pid))
            .collect();
        if found.is_empty() {
            break;
        }
        signal(libc::SIGSTOP, &found);
        below.extend(found);
    }
    let _ = shell.kill();
    signal(libc::SIGKILL, &below);
    std::thread::spawn(move || {
        let _ = shell.wait();
    });
}

/// The pids of everything below `root`: its children, theirs, and so on, as `ps` lists them.
fn descendants(root: u32) -> Vec<u32> {
    let Ok(listing) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&listing.stdout).lines() {
        let mut ids = line
            .split_whitespace()
            .filter_map(|id| id.parse::<u32>().ok());
        if let (Some(pid), Some(parent)) = (ids.next(), ids.next()) {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut below = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            if seen.insert(child) {
                below.push(child);
                parents.push(child);
            }
        }
    }
    below
}

/// Sends `signal` (`SIGSTOP`, `SIGKILL`) to every pid given, by the system call itself. Starting
/// `kill(1)` for it would fail on a machine short of processes for a moment: a tree
/// that was frozen and then not killed stayed frozen for good, taken in by launchd once its shell was
/// killed (a hook that timed out on a loaded machine left one). A pid that is already gone is no
/// error, and the rest are sent to all the same; a pid that would name a process group, init or
/// launchd (`0`, `1`, what is no `pid_t`) is never sent to.
fn signal(signal: libc::c_int, pids: &[u32]) {
    for &pid in pids {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        // SAFETY: kill(2) takes a pid and a signal number by value and touches no memory of ours.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// The directory a pre-create hook printed, resolved the way the path an open is given is:
/// a leading `~` against `home`, then canonicalised. A line that names no directory is the
/// hook's mistake, and stops the open with a message that says whose it is.
fn hook_dir(line: &str, home: &Path) -> anyhow::Result<PathBuf> {
    let expanded = naming::expand_tilde(line, home);
    if !expanded.is_dir() {
        anyhow::bail!("no such directory: {line} (the pre-create hook chose it)");
    }
    std::fs::canonicalize(&expanded)
        .with_context(|| format!("canonicalizing {}", expanded.display()))
}

/// The already-running session whose directory is `abs_path`: canonicalises both sides
/// (`std::fs::canonicalize`) so a symlink, or a redundant `.`/`..` segment, does not stop a
/// real match, falling back to comparing the paths exactly as given when either side no
/// longer exists to canonicalise (most commonly, a session left pointing at a directory
/// that was since removed).
pub fn find_running<'a>(sessions: &'a [SessionInfo], abs_path: &Path) -> Option<&'a SessionInfo> {
    let target_canon = std::fs::canonicalize(abs_path).ok();
    sessions.iter().find(|s| {
        let session_path = Path::new(&s.session_path);
        match (&target_canon, std::fs::canonicalize(session_path).ok()) {
            (Some(target), Some(candidate)) => *target == candidate,
            _ => abs_path.to_string_lossy().as_ref() == s.session_path.as_str(),
        }
    })
}

/// `sessions` as [`naming::Existing`] (each path in tilde form against `home`) — what
/// [`naming::session_name_for`] checks a proposed name against.
fn existing_from_sessions(sessions: &[SessionInfo], home: &Path) -> Vec<naming::Existing> {
    sessions
        .iter()
        .map(|s| naming::Existing {
            name: s.session_name.clone(),
            path: naming::tilde(Path::new(&s.session_path), home),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn session(name: &str, path: &str) -> SessionInfo {
        SessionInfo {
            session_id: "$1".to_string(),
            session_name: name.to_string(),
            session_path: path.to_string(),
            session_attached: 0,
            session_windows: 1,
            session_group_size: 0,
            active_window_id: "@1".to_string(),
            session_last_attached: 0,
        }
    }

    fn executable_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        crate::test_exec::make_executable(&script);
        script
    }

    // ---- find_running ----

    #[test]
    fn find_running_matches_a_noisy_path_via_canonicalization() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        std::fs::create_dir(&real).unwrap();
        // Same real directory, spelled with a redundant `.` component: canonicalize
        // resolves both to the same path even though the strings differ, proving this
        // isn't a plain string compare.
        let noisy = tmp.path().join(".").join("proj");
        let sessions = vec![session("proj", &real.display().to_string())];

        let found = find_running(&sessions, &noisy).expect("should match by canonical path");
        assert_eq!(found.session_name, "proj");
    }

    #[test]
    fn find_running_none_when_no_session_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = vec![session("other", "/definitely/not/it")];
        assert!(find_running(&sessions, tmp.path()).is_none());
    }

    #[test]
    fn find_running_falls_back_to_the_given_strings_when_canonicalize_fails() {
        // Neither side exists on disk, so canonicalize fails for both — the exact-string
        // fallback is what has to catch this, not a filesystem lookup.
        let sessions = vec![session("gone", "/no/such/directory")];
        assert!(find_running(&sessions, Path::new("/no/such/directory")).is_some());
        assert!(find_running(&sessions, Path::new("/no/such/other")).is_none());
    }

    // ---- hook_command ----

    /// `[sessions] pre_create_hook = ""` is the form the key table documents as the
    /// default. Run, it would abort session creation: an empty command is "no hook", the
    /// same as leaving the key out.
    #[test]
    fn an_empty_hook_command_is_no_hook_at_all() {
        assert_eq!(hook_command(None), None);
        assert_eq!(hook_command(Some(&String::new())), None);
        assert_eq!(hook_command(Some(&"   ".to_string())), None);
        assert_eq!(hook_command(Some(&"\t\n".to_string())), None);
    }

    #[test]
    fn a_configured_hook_command_is_used_exactly_as_written() {
        let hook = "/opt/taw/pre-create.sh --layout dev".to_string();
        assert_eq!(hook_command(Some(&hook)), Some(hook.as_str()));
    }

    // ---- hook_script ----

    const HOME: &str = "/Users/x";

    #[test]
    fn a_hook_script_is_the_value_then_the_two_arguments_as_they_came() {
        assert_eq!(
            hook_script("/opt/pre.sh", Path::new(HOME)),
            "/opt/pre.sh \"$@\""
        );
        assert_eq!(
            hook_script("/opt/pre.sh --layout dev", Path::new(HOME)),
            "/opt/pre.sh --layout dev \"$@\""
        );
        assert_eq!(
            hook_script("LABEL=a /opt/pre.sh", Path::new(HOME)),
            "LABEL=a /opt/pre.sh \"$@\""
        );
    }

    #[test]
    fn a_hook_script_drops_the_whitespace_that_would_end_the_command_early() {
        assert_eq!(
            hook_script("  /opt/pre.sh\n", Path::new(HOME)),
            "/opt/pre.sh \"$@\""
        );
    }

    #[test]
    fn a_leading_tilde_slash_is_the_home_directory() {
        assert_eq!(
            hook_script("~/bin/pre.sh --layout dev", Path::new(HOME)),
            "/Users/x/bin/pre.sh --layout dev \"$@\""
        );
    }

    #[test]
    fn a_tilde_that_does_not_start_the_value_is_left_to_the_shell() {
        for value in ["~other/pre.sh", "/opt/pre.sh ~/x", "~", "~pre.sh"] {
            assert_eq!(
                hook_script(value, Path::new(HOME)),
                format!("{value} \"$@\""),
                "{value}"
            );
        }
    }

    // ---- parse_reply ----

    /// A reply with a name and nothing else.
    fn named(name: &str) -> HookReply {
        HookReply {
            name: Some(name.to_string()),
            dir: None,
        }
    }

    fn reply(name: &str, dir: &str) -> HookReply {
        HookReply {
            name: Some(name.to_string()),
            dir: Some(dir.to_string()),
        }
    }

    #[test]
    fn nothing_printed_is_no_answer() {
        assert_eq!(parse_reply(""), HookReply::default());
        assert_eq!(parse_reply("\n"), HookReply::default());
    }

    #[test]
    fn the_first_line_is_the_name_and_the_second_the_directory() {
        assert_eq!(
            parse_reply("work-api\n/srv/api\n"),
            reply("work-api", "/srv/api")
        );
        assert_eq!(
            parse_reply("work-api\n/srv/api"),
            reply("work-api", "/srv/api")
        );
        assert_eq!(parse_reply("work-api\n"), named("work-api"));
        assert_eq!(parse_reply("work-api"), named("work-api"));
    }

    #[test]
    fn a_line_with_nothing_on_it_is_no_answer_for_what_it_stands_for() {
        let dir_only = HookReply {
            name: None,
            dir: Some("/srv/api".to_string()),
        };
        assert_eq!(parse_reply("\n/srv/api\n"), dir_only);
        assert_eq!(parse_reply("  \t\n/srv/api\n"), dir_only);
        assert_eq!(parse_reply("work-api\n\n"), named("work-api"));
        assert_eq!(parse_reply("work-api\n   \n"), named("work-api"));
    }

    #[test]
    fn lines_after_the_second_are_ignored() {
        assert_eq!(
            parse_reply("work-api\n/srv/api\nnotes to self\nmore\n"),
            reply("work-api", "/srv/api")
        );
    }

    /// Only the line ending is dropped, so a hook that prints back what it was given, a
    /// trailing space included, has printed nothing new.
    #[test]
    fn a_line_is_kept_as_printed_apart_from_its_ending() {
        assert_eq!(
            parse_reply("my proj \n/srv/a b \n"),
            reply("my proj ", "/srv/a b ")
        );
        assert_eq!(parse_reply("api\r\n/srv/api\r\n"), reply("api", "/srv/api"));
    }

    // ---- hook_dir ----

    #[test]
    fn a_directory_the_hook_prints_is_resolved_like_the_path_an_open_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let real = home.join("code").join("api");
        std::fs::create_dir_all(&real).unwrap();

        assert_eq!(
            hook_dir(real.to_str().unwrap(), Path::new("/nowhere")).unwrap(),
            real
        );
        assert_eq!(hook_dir("~/code/api", &home).unwrap(), real);
        assert_eq!(
            hook_dir(
                &format!("{}/code/../code/api", home.display()),
                Path::new("/nowhere")
            )
            .unwrap(),
            real
        );
    }

    #[test]
    fn a_directory_the_hook_prints_that_is_not_there_names_the_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, "").unwrap();

        for line in ["/no/such/dir", file.to_str().unwrap()] {
            let err = hook_dir(line, tmp.path()).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("no such directory: {line} (the pre-create hook chose it)")
            );
        }
    }

    // ---- run_hook ----

    /// Runs `command` as a pre-create hook against a home that does not matter.
    fn run_pre(command: &str, session: &str, dir: &Path) -> anyhow::Result<HookReply> {
        run_hook(Hook::Pre, command, Path::new(HOME), session, dir, None)
    }

    #[test]
    fn run_hook_captures_a_renamed_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "pre.sh", "#!/bin/sh\necho renamed\n");

        let out = run_pre(script.to_str().unwrap(), "proposed", Path::new("/tmp/dir")).unwrap();
        assert_eq!(out, named("renamed"));
    }

    #[test]
    fn run_hook_returns_both_lines_of_a_pre_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "pre.sh",
            "#!/bin/sh\necho renamed\necho /srv/api\n",
        );

        let out = run_pre(script.to_str().unwrap(), "proposed", Path::new("/tmp/dir")).unwrap();
        assert_eq!(out, reply("renamed", "/srv/api"));
    }

    #[test]
    fn run_hook_passes_session_and_dir_as_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let recorded = tmp.path().join("recorded.txt");
        let script = executable_script(
            tmp.path(),
            "record.sh",
            &format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$1\" \"$2\" > {}\n",
                recorded.display()
            ),
        );

        run_hook(
            Hook::Post,
            script.to_str().unwrap(),
            Path::new(HOME),
            "my-session",
            Path::new("/some/dir"),
            None,
        )
        .unwrap();

        let contents = std::fs::read_to_string(&recorded).unwrap();
        assert_eq!(contents, "my-session\n/some/dir\n");
    }

    #[test]
    fn run_hook_puts_the_flags_of_the_value_before_the_two_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "flags.sh",
            "#!/bin/sh\nprintf '%s|%s|%s|%s\\n' \"$1\" \"$2\" \"$3\" \"$4\"\n",
        );
        let command = format!("{} --layout 'dev tools'", script.display());

        let out = run_pre(&command, "proposed", Path::new("/tmp/dir")).unwrap();

        assert_eq!(out, named("--layout|dev tools|proposed|/tmp/dir"));
    }

    /// The point of `"$@"`: a name that a shell would act on reaches the hook unread.
    #[test]
    fn run_hook_never_lets_a_shell_read_the_session_or_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "echo.sh", "#!/bin/sh\nprintf '%s\\n' \"$1\"\n");
        let hostile = "a'b\"c $(echo hi) `echo hi` $HOME * ; exit 7 &";

        let out = run_pre(script.to_str().unwrap(), hostile, Path::new("/tmp/dir")).unwrap();

        assert_eq!(out, named(hostile));
    }

    #[test]
    fn run_hook_expands_a_leading_tilde_against_the_home_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        // A home with a space and a quote in it: the tilde still has to come out as one word.
        let home = tmp.path().join("o'neil home");
        std::fs::create_dir(&home).unwrap();
        executable_script(&home, "pre.sh", "#!/bin/sh\necho from-home\n");

        let out = run_hook(
            Hook::Pre,
            "~/pre.sh",
            &home,
            "proposed",
            Path::new("/tmp/dir"),
            None,
        )
        .unwrap();

        assert_eq!(out, named("from-home"));
    }

    #[test]
    fn run_hook_ignores_stdout_when_it_is_the_post_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "post.sh", "#!/bin/sh\necho renamed\n");

        let out = run_hook(
            Hook::Post,
            script.to_str().unwrap(),
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            None,
        )
        .unwrap();
        assert_eq!(out, HookReply::default());
    }

    #[test]
    fn run_hook_empty_first_line_is_no_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "pre.sh", "#!/bin/sh\nexit 0\n");

        let out = run_pre(script.to_str().unwrap(), "proposed", Path::new("/tmp/dir")).unwrap();
        assert_eq!(out, HookReply::default());
    }

    #[test]
    fn run_hook_nonzero_exit_is_err_naming_the_hook_with_its_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "fail.sh",
            "#!/bin/sh\necho 'boom' >&2\nexit 3\n",
        );
        let command = script.to_str().unwrap();

        let err = run_pre(command, "proposed", Path::new("/tmp/dir")).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("pre-create hook {command} failed (exit status: 3): boom")
        );

        let err = run_hook(
            Hook::Post,
            command,
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("post-create hook "), "{err}");
    }

    /// A post hook's output goes to /dev/null, so there is no stderr to carry.
    #[test]
    fn a_failing_post_hook_is_named_with_its_status_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "fail.sh",
            "#!/bin/sh\necho 'boom' >&2\nexit 3\n",
        );
        let command = script.to_str().unwrap();

        let err = run_hook(
            Hook::Post,
            command,
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("post-create hook {command} failed (exit status: 3)")
        );
    }

    #[test]
    fn run_hook_nonzero_exit_with_nothing_on_stderr_ends_at_the_status() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "quiet.sh", "#!/bin/sh\nexit 1\n");
        let command = script.to_str().unwrap();

        let err = run_pre(command, "proposed", Path::new("/tmp/dir")).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("pre-create hook {command} failed (exit status: 1)")
        );
    }

    // ---- hook_limit ----

    #[test]
    fn a_number_of_seconds_is_a_limit_and_zero_is_none() {
        assert_eq!(hook_limit(10.0), Some(Duration::from_secs(10)));
        assert_eq!(hook_limit(0.5), Some(Duration::from_millis(500)));
        assert_eq!(hook_limit(0.0), None);
    }

    /// `taw config check` reports these; a hook that is asked to stop before it starts, or
    /// a panic building a `Duration` from them, would be worse than none.
    #[test]
    fn a_number_that_is_no_length_of_time_is_no_limit() {
        for seconds in [-1.0, -0.0, f64::NAN, f64::INFINITY, 1e30] {
            assert_eq!(hook_limit(seconds), None, "{seconds}");
        }
    }

    // ---- run_hook: the limit ----

    /// Whether the process `pid` is still there (`kill -0`).
    fn is_running(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Whether the process `pid` is gone within a few seconds: whatever reaps it may take a
    /// moment after it is killed.
    fn is_gone_soon(pid: &str) -> bool {
        for _ in 0..40 {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Whether the process `pid` is still running what one of `marks` names: a number the system has
    /// given to another process since is none of a test's, and is never ended by one.
    fn is_ours(pid: &str, marks: &[&str]) -> bool {
        Command::new("ps")
            .args(["-o", "command=", "-p", pid])
            .output()
            .is_ok_and(|out| {
                let command = String::from_utf8_lossy(&out.stdout);
                marks.iter().any(|mark| command.contains(mark))
            })
    }

    /// Ends `pid` if it is still there, so a test that failed leaves no `sleep 30` behind.
    fn stop_pid(pid: &str) {
        let _ = Command::new("kill")
            .args(["-KILL", pid])
            .stderr(Stdio::null())
            .status();
    }

    /// A limit that a hook which sleeps 30 seconds is far past, and that a test can wait for.
    const SHORT: Duration = Duration::from_millis(300);

    /// Well over what stopping a hook takes, and well under the 30 seconds it sleeps.
    const MARGIN: Duration = Duration::from_secs(8);

    #[test]
    fn a_hook_that_outlives_its_limit_is_stopped_and_fails_as_timed_out() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "slow.sh", "#!/bin/sh\nsleep 30\n");
        let command = script.to_str().unwrap();

        for kind in [Hook::Pre, Hook::Post] {
            let started = Instant::now();
            let err = run_hook(
                kind,
                command,
                Path::new(HOME),
                "proposed",
                Path::new("/tmp/dir"),
                Some(SHORT),
            )
            .unwrap_err();
            let took = started.elapsed();

            assert_eq!(
                err.downcast_ref::<HookFailure>(),
                Some(&HookFailure::TimedOut {
                    hook: kind,
                    limit: SHORT
                })
            );
            assert!(
                took >= SHORT && took < SHORT + MARGIN,
                "{kind:?}: took {took:?} against a limit of {SHORT:?}"
            );
        }
    }

    #[test]
    fn a_timed_out_hook_says_so_by_name_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "slow.sh", "#!/bin/sh\nsleep 30\n");
        let command = script.to_str().unwrap();

        let pre = run_hook(
            Hook::Pre,
            command,
            Path::new(HOME),
            "p",
            Path::new("/tmp/dir"),
            Some(SHORT),
        )
        .unwrap_err();
        assert_eq!(pre.to_string(), "pre-create hook took longer than 0.3 s");

        let post = run_hook(
            Hook::Post,
            command,
            Path::new(HOME),
            "p",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(1)),
        )
        .unwrap_err();
        assert_eq!(post.to_string(), "post-create hook took longer than 1 s");
    }

    /// The summary of a timeout is the whole message: a toast has no more to say.
    #[test]
    fn the_summary_of_a_timed_out_hook_is_its_message() {
        let failure = HookFailure::TimedOut {
            hook: Hook::Pre,
            limit: Duration::from_secs(10),
        };
        assert_eq!(failure.summary(), "pre-create hook took longer than 10 s");
        assert_eq!(failure.to_string(), failure.summary());
    }

    /// A shell that is stopped takes its children with it, and theirs: the process a hook
    /// runs is rarely the shell itself.
    #[test]
    fn stopping_a_hook_stops_what_it_started_at_every_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("deep.pid");
        let subshell_file = tmp.path().join("subshell.pid");
        // The sleep is the child of a subshell, which is the child of the hook.
        let script = executable_script(
            tmp.path(),
            "deep.sh",
            &format!(
                "#!/bin/sh\n( sleep 30 & echo $! > '{}'; wait ) &\necho $! > '{}'\nwait\n",
                pid_file.display(),
                subshell_file.display()
            ),
        );

        // The first run of a script that was just written can take a second to start on a
        // machine that scans new executables, so the limit leaves the tree time to grow.
        let err = run_hook(
            Hook::Pre,
            script.to_str().unwrap(),
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(3)),
        )
        .unwrap_err();

        let pid = std::fs::read_to_string(&pid_file)
            .expect("the hook started the sleep")
            .trim()
            .to_string();
        let gone = is_gone_soon(&pid);
        // Nothing of the hook is left by this test, whatever it says: the sleep, and the subshell
        // above it, which a stop that froze the tree and did not kill it would leave frozen.
        let subshell = std::fs::read_to_string(&subshell_file).unwrap_or_default();
        for pid in [pid.as_str(), subshell.trim()] {
            if !pid.is_empty() && is_ours(pid, &["sleep 30", "deep.sh"]) {
                stop_pid(pid);
            }
        }
        assert!(err.downcast_ref::<HookFailure>().is_some(), "{err}");
        assert!(gone, "the sleep two levels down was left running");
    }

    /// A tree that grows while it is stopped: a subshell that starts a new long sleep every
    /// few milliseconds. Listing it and then killing what was listed would leave the sleeps
    /// started in between; stopping what is found first, and listing again, does not. The
    /// subshell and every sleep note their pids, and each sleep sleeps a length of this run's own:
    /// what is left is what still runs what the hook started there, noted or not, and this test ends
    /// it by pid, never by a name, whatever happens.
    #[test]
    fn stopping_a_hook_that_is_still_starting_processes_leaves_none_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("ready");
        let pids = tmp.path().join("pids");
        // A length of this run's own, 31.7 and the number of this process, which no other test, no one's
        // shell and no other run of this test on the machine sleeps for: what is counted below, and
        // what is ended, is found by it and by nothing a name like `sleep 31.7` would match as well.
        let length = format!("31.7{}", std::process::id());
        let script = executable_script(
            tmp.path(),
            "forking.sh",
            &format!(
                "#!/bin/sh
( while :; do /bin/sleep {length} & echo $! >> '{pids}'; : > '{ready}'; /bin/sleep 0.02; done ) &
echo $! >> '{pids}'
wait
",
                pids = pids.display(),
                ready = ready.display()
            ),
        );

        let err = run_hook(
            Hook::Pre,
            script.to_str().unwrap(),
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(3)),
        )
        .unwrap_err();

        let recorded: Vec<String> = std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.trim().to_string())
            .filter(|pid| !pid.is_empty())
            .collect();
        // What is left of the hook, by pid: every process sleeping this run's length, and every pid the
        // hook noted that still runs what it started there (the looping subshell as well).
        let sleeping = format!("sleep {length}");
        let left = || -> Vec<String> {
            let ps = Command::new("ps")
                .args(["-A", "-o", "pid=,command="])
                .output()
                .unwrap();
            let mut left: Vec<String> = String::from_utf8_lossy(&ps.stdout)
                .lines()
                .filter(|line| line.trim_end().ends_with(&sleeping))
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_string)
                .collect();
            for pid in &recorded {
                if !left.contains(pid)
                    && is_running(pid)
                    && is_ours(pid, &[&sleeping, "forking.sh"])
                {
                    left.push(pid.clone());
                }
            }
            left
        };
        let mut remaining = left();
        for _ in 0..40 {
            if remaining.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            remaining = left();
        }
        for pid in &remaining {
            stop_pid(pid);
        }
        assert!(
            ready.exists(),
            "the hook never got as far as starting a sleep"
        );
        assert!(err.downcast_ref::<HookFailure>().is_some(), "{err}");
        assert!(
            remaining.is_empty(),
            "processes the hook started were left running: {remaining:?}"
        );
    }

    #[test]
    fn a_hook_that_ends_inside_its_limit_answers_as_usual() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "quick.sh", "#!/bin/sh\necho renamed\n");

        let out = run_hook(
            Hook::Pre,
            script.to_str().unwrap(),
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(30)),
        )
        .unwrap();

        assert_eq!(out, named("renamed"));
    }

    #[test]
    fn with_no_limit_a_hook_is_waited_for_however_long_it_takes() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(tmp.path(), "slowish.sh", "#!/bin/sh\nsleep 1\necho slow\n");

        let out = run_pre(script.to_str().unwrap(), "proposed", Path::new("/tmp/dir")).unwrap();

        assert_eq!(out, named("slow"));
    }

    /// Both pipes are read while the hook runs. A reader that took one to its end before the
    /// other would leave a hook that fills the second waiting for it, and this would end only
    /// when the limit stopped it.
    #[test]
    fn a_hook_that_fills_both_pipes_is_read_to_the_end_of_each() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "loud.sh",
            "#!/bin/sh\n( yes out | head -n 200000 ) &\n( yes err | head -n 200000 ) >&2\nwait\n",
        );
        let command = script.to_str().unwrap();

        let out = run_hook(
            Hook::Pre,
            command,
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(out, reply("out", "out"));

        let script = executable_script(
            tmp.path(),
            "loud-and-failing.sh",
            "#!/bin/sh\n( yes out | head -n 200000 ) &\n( yes oops | head -n 200000 ) >&2\nwait\nexit 1\n",
        );
        let err = run_hook(
            Hook::Pre,
            script.to_str().unwrap(),
            Path::new(HOME),
            "proposed",
            Path::new("/tmp/dir"),
            Some(Duration::from_secs(30)),
        )
        .unwrap_err();
        assert_eq!(
            err.downcast_ref::<HookFailure>()
                .map(HookFailure::summary)
                .as_deref(),
            Some("pre-create hook: oops")
        );
    }

    /// A hook that writes without end must fill no memory: a caller keeps the last part of each
    /// stream, where a verdict is, and the rest is read and dropped as it arrives, so the hook is
    /// never left waiting for a reader either.
    #[test]
    fn a_command_that_writes_a_great_deal_is_held_to_the_last_of_it() {
        let ran = run_command(
            "head -c 3000000 /dev/zero | tr '\\0' x; echo; \
             head -c 3000000 /dev/zero | tr '\\0' e >&2; echo >&2; echo 'the verdict' >&2",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            None,
            Some(Duration::from_secs(30)),
            Streams::SHOWN,
        )
        .unwrap();

        let Ran::Ended(ended) = ran else {
            panic!("the command ran out of time");
        };
        assert!(ended.status.success());
        assert!(ended.stdout.len() <= HELD_BYTES, "{}", ended.stdout.len());
        assert!(ended.stderr.len() <= HELD_BYTES, "{}", ended.stderr.len());
        // It is the end that is kept.
        assert!(said(&ended.stdout).ends_with("xxx\n"));
        assert!(said(&ended.stderr).ends_with("eee\nthe verdict\n"));
    }

    /// A stream is cut only when more was written than is kept: what fills the space exactly is
    /// all there is, and one byte more drops the first. A stream kept whole or not at all is
    /// never cut.
    #[test]
    fn a_stream_is_cut_only_when_more_was_written_than_is_kept() {
        let of = |bytes: usize, stdout: Keep, stderr: Keep| {
            let ran = run_command(
                &format!("head -c {bytes} /dev/zero | tr '\\0' x; head -c {bytes} /dev/zero | tr '\\0' e >&2"),
                Path::new(HOME),
                &HookEnv::default(),
                b"",
                None,
                Some(Duration::from_secs(30)),
                Streams {
                    stdout,
                    stderr,
                    after_exit: None,
                },
            )
            .unwrap();
            let Ran::Ended(ended) = ran else {
                panic!("the command ran out of time");
            };
            ended
        };

        for (bytes, cut) in [(999, false), (1000, false), (1001, true)] {
            let ended = of(bytes, Keep::Last(1000), Keep::Last(1000));
            assert_eq!(
                (ended.stdout_cut, ended.stderr_cut),
                (cut, cut),
                "{bytes} bytes against 1000"
            );
            assert_eq!(ended.stdout.len(), bytes.min(1000));
        }
        let ended = of(3000, Keep::All, Keep::Nothing);
        assert_eq!((ended.stdout_cut, ended.stderr_cut), (false, false));
        assert_eq!(ended.stdout.len(), 3000);
        assert!(ended.stderr.is_empty());
    }

    /// The Hooks page and the compatibility notes say how much of a hook's output is kept, in KiB:
    /// the words follow the constant.
    #[test]
    fn the_docs_say_how_much_of_a_hooks_output_is_kept() {
        let kept = format!("the last {} KiB", HELD_BYTES / 1024);
        for (name, text) in [
            ("docs/hooks.md", include_str!("../../docs/hooks.md")),
            ("docs/COMPAT.md", include_str!("../../docs/COMPAT.md")),
        ] {
            assert!(text.contains(&kept), "{name} never says {kept:?}");
        }
    }

    /// The Hooks page says how long a hook that can refuse is still read after its shell has ended:
    /// the words follow the constant.
    #[test]
    fn the_readme_says_how_long_a_veto_is_read_after_its_shell_ends() {
        let said = format!("for {} s more", AFTER_EXIT.as_secs_f64());
        assert!(
            include_str!("../../docs/hooks.md").contains(&said),
            "the Hooks page never says {said:?}"
        );
    }

    // ---- command_line ----

    #[test]
    fn a_command_line_is_the_value_alone_with_no_arguments_after_it() {
        assert_eq!(
            command_line("/opt/pre.sh --layout dev", Path::new(HOME)),
            "/opt/pre.sh --layout dev"
        );
        assert!(!command_line("/opt/pre.sh", Path::new(HOME)).contains("$@"));
    }

    #[test]
    fn a_command_line_starts_with_the_home_directory_for_a_leading_tilde_slash() {
        assert_eq!(
            command_line("~/bin/setup --x", Path::new(HOME)),
            "/Users/x/bin/setup --x"
        );
        // A tilde anywhere else is the shell's own to expand, and the value is trimmed.
        assert_eq!(
            command_line("  cat ~/notes\n", Path::new(HOME)),
            "cat ~/notes"
        );
        assert_eq!(
            command_line("hook_script_is_this_plus_the_arguments", Path::new(HOME)),
            "hook_script_is_this_plus_the_arguments"
        );
    }

    // ---- run_command ----

    /// Runs `command` the way an event hook is run, and returns how it ended: it must have
    /// ended, not run out of time.
    fn run_event(
        command: &str,
        env: &[(&str, &str)],
        input: &[u8],
        limit: Option<Duration>,
    ) -> Ended {
        let env = HookEnv {
            set: env
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            unset: Vec::new(),
        };
        match run_command(
            command,
            Path::new(HOME),
            &env,
            input,
            None,
            limit,
            Streams::SHOWN,
        )
        .unwrap()
        {
            Ran::Ended(ended) => ended,
            Ran::OutOfTime(limit) => panic!("{command} ran past {limit:?}"),
        }
    }

    fn said(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn a_command_is_given_its_environment_and_its_input_and_no_arguments() {
        let ended = run_event(
            r#"printf '%s|%s|' "$#" "$TAW_TITLE"; cat"#,
            &[("TAW_TITLE", "a $(echo hi) title")],
            b"{\"event\":\"agent.done\"}",
            None,
        );

        assert!(ended.status.success());
        assert_eq!(
            said(&ended.stdout),
            "0|a $(echo hi) title|{\"event\":\"agent.done\"}"
        );
    }

    /// What tmux tells a new pane to leave out, a hook is started without, whatever the process
    /// that starts it has: a removal is a variable taken out of the command's environment, and
    /// not a variable set to nothing. What a hook's environment holds is never shown, only the
    /// names.
    #[test]
    fn a_hook_environment_sets_what_it_sets_and_takes_out_what_it_removes() {
        let env = HookEnv {
            set: vec![("PUSH_TOKEN".to_string(), "hunter2".to_string())],
            unset: vec!["STALE_AGENT".to_string()],
        };

        let mut command = Command::new("true");
        env.apply(&mut command);

        let applied: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .filter(|(name, _)| !crate::plugin::env::PANE_CONTEXT_VARS.contains(&name.as_str()))
            .collect();
        assert_eq!(
            applied,
            [
                ("PUSH_TOKEN".to_string(), Some("hunter2".to_string())),
                ("STALE_AGENT".to_string(), None),
            ]
        );
        assert_eq!(env.get("PUSH_TOKEN"), Some("hunter2"));
        assert_eq!(env.get("STALE_AGENT"), None);
        let shown = format!("{env:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(
            shown.contains("PUSH_TOKEN") && shown.contains("STALE_AGENT"),
            "{shown}"
        );
    }

    /// `@agent-wizard-herdr-env` puts `HERDR_PANE_ID=%calling` in tmux's global environment, which a
    /// `run-shell` job has and a hook then inherits: a hook is told its pane by `TAW_PANE`, and
    /// never has a pane, tab or workspace id of herdr's from the process that started it. What the
    /// environment itself sets (here, `HERDR_TAB_ID`) is the caller's choice and stays.
    #[test]
    fn a_hook_starts_without_the_pane_ids_its_process_has_and_keeps_what_it_is_given() {
        let env = HookEnv {
            set: vec![("HERDR_TAB_ID".to_string(), "given".to_string())],
            unset: Vec::new(),
        };

        let mut command = Command::new("true");
        env.apply(&mut command);

        let value = |name: &str| {
            command
                .get_envs()
                .find(|(own, _)| own.to_string_lossy() == name)
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        for name in crate::plugin::env::PANE_CONTEXT_VARS {
            if name == "HERDR_TAB_ID" {
                continue;
            }
            assert_eq!(value(name), Some(None), "{name} must be taken out");
        }
        assert_eq!(value("HERDR_TAB_ID"), Some(Some("given".to_string())));
    }

    /// The point of the environment: what a shell would act on in a title reaches the hook
    /// as the characters it is, and runs nothing.
    #[test]
    fn a_value_in_the_environment_is_never_run_as_part_of_the_command() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("ran");
        let hostile = format!(
            "x; touch {} `touch {}` $(touch {})",
            marker.display(),
            marker.display(),
            marker.display()
        );

        let ended = run_event(
            r#"printf '%s' "$TAW_TITLE""#,
            &[("TAW_TITLE", &hostile)],
            b"",
            None,
        );

        assert_eq!(said(&ended.stdout), hostile);
        assert!(!marker.exists(), "the title was run as a command");
    }

    #[test]
    fn input_larger_than_a_pipe_is_written_whole_to_a_command_that_reads_it() {
        let input = vec![b'x'; 300_000];

        let ended = run_event("wc -c", &[], &input, Some(Duration::from_secs(30)));

        assert_eq!(said(&ended.stdout).trim(), "300000");
    }

    #[test]
    fn a_command_that_never_reads_its_input_ends_all_the_same() {
        let input = vec![b'x'; 300_000];

        let ended = run_event("true", &[], &input, Some(Duration::from_secs(30)));

        assert!(ended.status.success());
    }

    #[test]
    fn a_command_runs_in_the_directory_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();

        let ran = run_command(
            "pwd",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            Some(&dir),
            None,
            Streams::SHOWN,
        )
        .unwrap();

        let Ran::Ended(ended) = ran else {
            panic!("pwd ran out of time");
        };
        assert_eq!(said(&ended.stdout).trim(), dir.to_str().unwrap());
    }

    /// A directory that was there when the hook was looked up and is gone by the time it starts:
    /// the hook starts in the home directory, which is where it would have started had the
    /// directory never been there, and not wherever this process is.
    #[test]
    fn a_command_whose_directory_is_gone_starts_in_the_home_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let gone = home.join("no-such-directory");

        let ran = run_command(
            "pwd -P",
            &home,
            &HookEnv::default(),
            b"",
            Some(&gone),
            None,
            Streams::SHOWN,
        )
        .unwrap();

        let Ran::Ended(ended) = ran else {
            panic!("pwd ran out of time");
        };
        assert_eq!(said(&ended.stdout).trim(), home.to_str().unwrap());
    }

    #[test]
    fn a_hook_is_tried_in_its_directory_and_then_in_the_home_directory_and_never_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let dir = Path::new("/work/api");

        assert_eq!(starting_places(Some(dir), home), [dir, home]);
        assert_eq!(starting_places(Some(home), home), [home]);
        // No directory asked for is where the process is; a home that is not a directory is no
        // place to fall back to.
        assert!(starting_places(None, home).is_empty());
        assert_eq!(
            starting_places(Some(dir), Path::new("/no/such/home")),
            [dir]
        );
    }

    #[test]
    fn a_command_expands_a_leading_tilde_against_the_home_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        // A home with a space and a quote in it: the tilde still has to come out as one word.
        let home = tmp.path().join("o'neil home");
        std::fs::create_dir(&home).unwrap();
        executable_script(&home, "hook.sh", "#!/bin/sh\necho from-home\n");

        let ran = run_command(
            "~/hook.sh",
            &home,
            &HookEnv::default(),
            b"",
            None,
            None,
            Streams::SHOWN,
        )
        .unwrap();

        let Ran::Ended(ended) = ran else {
            panic!("the hook ran out of time");
        };
        assert_eq!(said(&ended.stdout).trim(), "from-home");
    }

    #[test]
    fn a_command_that_outlives_its_limit_is_stopped_like_a_create_hook() {
        let started = Instant::now();

        let ran = run_command(
            "sleep 30",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            None,
            Some(SHORT),
            Streams::SHOWN,
        )
        .unwrap();

        assert!(matches!(ran, Ran::OutOfTime(limit) if limit == SHORT));
        assert!(
            started.elapsed() < SHORT + MARGIN,
            "took {:?} against a limit of {SHORT:?}",
            started.elapsed()
        );
    }

    // ---- require_success ----

    #[test]
    fn a_command_that_exits_non_zero_is_a_refusal_by_the_hook_that_ran_it() {
        let ran = run_command(
            "echo 'looking' >&2; echo 'not now' >&2; exit 2",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            None,
            None,
            Streams::BACKGROUND,
        )
        .unwrap();

        let err = require_success(Hook::PreClose, "the command", ran).unwrap_err();

        assert_eq!(
            err,
            HookFailure::Exited {
                hook: Hook::PreClose,
                command: "the command".to_string(),
                status: ExitStatus::from_raw(2 << 8),
                stderr: "looking\nnot now".to_string(),
            }
        );
        assert_eq!(err.summary(), "pre-close hook: not now");
    }

    #[test]
    fn a_command_that_runs_out_of_time_is_a_refusal_that_says_so() {
        let ran = run_command(
            "sleep 30",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            None,
            Some(SHORT),
            Streams::SHOWN,
        )
        .unwrap();

        let err = require_success(Hook::PreWorktreeRemove, "sleep 30", ran).unwrap_err();

        assert_eq!(
            err.to_string(),
            "pre-worktree-remove hook took longer than 0.3 s"
        );
    }

    #[test]
    fn a_command_that_exits_zero_is_no_refusal() {
        let ran = run_command(
            "exit 0",
            Path::new(HOME),
            &HookEnv::default(),
            b"",
            None,
            None,
            Streams::SHOWN,
        )
        .unwrap();
        assert!(require_success(Hook::PreClose, "exit 0", ran).is_ok());
    }

    #[test]
    fn the_two_vetoes_are_named_for_the_keys_that_configure_them() {
        assert_eq!(
            refused(Hook::PreClose, 1, "keep it open").summary(),
            "pre-close hook: keep it open"
        );
        assert_eq!(
            refused(Hook::PreWorktreeRemove, 1, "").summary(),
            "pre-worktree-remove hook refused (exit status 1)"
        );
    }

    // ---- descendants ----

    #[test]
    fn descendants_finds_a_processs_children_and_theirs_and_no_one_else() {
        let mut shell = Command::new(SHELL)
            // Two commands, so that the subshell stays a process of its own above the sleep.
            .args(["-c", "( sleep 30; true ) & wait"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();

        let mut below = Vec::new();
        for _ in 0..50 {
            below = descendants(shell.id());
            if below.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        signal(libc::SIGKILL, &below);
        let _ = shell.kill();
        let _ = shell.wait();

        assert!(
            below.len() >= 2,
            "a subshell and its sleep should be found: {below:?}"
        );
        assert!(!below.contains(&shell.id()));
        assert!(!below.contains(&std::process::id()));
    }

    // ---- HookFailure ----

    /// A failure as `run_hook` raises it, built without running anything: `code` is the
    /// exit code the hook left with.
    fn refused(hook: Hook, code: i32, stderr: &str) -> HookFailure {
        HookFailure::Exited {
            hook,
            command: "/opt/pre.sh".to_string(),
            status: ExitStatus::from_raw(code << 8),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn a_refusing_hook_fails_with_a_hook_failure_that_holds_what_it_said() {
        let tmp = tempfile::tempdir().unwrap();
        let script = executable_script(
            tmp.path(),
            "fail.sh",
            "#!/bin/sh\necho 'first' >&2\necho 'not today' >&2\nexit 3\n",
        );
        let command = script.to_str().unwrap();

        let err = run_pre(command, "proposed", Path::new("/tmp/dir")).unwrap_err();

        assert_eq!(
            err.downcast_ref::<HookFailure>(),
            Some(&HookFailure::Exited {
                hook: Hook::Pre,
                command: command.to_string(),
                status: ExitStatus::from_raw(3 << 8),
                stderr: "first\nnot today".to_string(),
            })
        );
    }

    #[test]
    fn the_summary_of_a_refusal_is_the_hook_and_the_last_line_it_wrote() {
        assert_eq!(
            refused(Hook::Pre, 1, "not on a Friday").summary(),
            "pre-create hook: not on a Friday"
        );
        assert_eq!(
            refused(Hook::Pre, 1, "looking at the calendar\nnot on a Friday").summary(),
            "pre-create hook: not on a Friday"
        );
        // A trailing blank line, or indentation, is not what the hook meant to say.
        assert_eq!(
            refused(Hook::Pre, 1, "not on a Friday  \n\n   \n").summary(),
            "pre-create hook: not on a Friday"
        );
        assert_eq!(
            refused(Hook::Post, 1, "disk full").summary(),
            "post-create hook: disk full"
        );
    }

    #[test]
    fn the_summary_of_a_refusal_that_said_nothing_is_the_exit_status() {
        assert_eq!(
            refused(Hook::Pre, 3, "").summary(),
            "pre-create hook refused (exit status 3)"
        );
        assert_eq!(
            refused(Hook::Pre, 1, " \n\t\n").summary(),
            "pre-create hook refused (exit status 1)"
        );
        assert_eq!(
            refused(Hook::Post, 127, "").summary(),
            "post-create hook refused (exit status 127)"
        );
    }

    /// A hook ended by a signal has no exit code to give.
    #[test]
    fn the_summary_of_a_hook_a_signal_ended_names_the_signal() {
        let killed = HookFailure::Exited {
            hook: Hook::Pre,
            command: "/opt/pre.sh".to_string(),
            status: ExitStatus::from_raw(9),
            stderr: String::new(),
        };
        assert_eq!(
            killed.summary(),
            "pre-create hook refused (killed by signal 9)"
        );
    }

    /// The message an error prints keeps the command and all of stderr: the summary is for
    /// a toast, and this is for whoever reads a log or a JSON error.
    #[test]
    fn the_message_of_a_refusal_keeps_the_command_and_all_of_stderr() {
        assert_eq!(
            refused(Hook::Pre, 3, "first\nnot today").to_string(),
            "pre-create hook /opt/pre.sh failed (exit status: 3): first\nnot today"
        );
        assert_eq!(
            refused(Hook::Post, 3, "").to_string(),
            "post-create hook /opt/pre.sh failed (exit status: 3)"
        );
    }

    // ---- name proposal (naming::session_name_for via existing_from_sessions) ----

    #[test]
    fn existing_from_sessions_feeds_the_collision_suffix_logic() {
        let home = Path::new("/Users/x");
        let sessions = vec![session("api", "/Users/x/work/api")];

        let existing = existing_from_sessions(&sessions, home);
        assert_eq!(
            existing,
            vec![naming::Existing {
                name: "api".to_string(),
                path: "~/work/api".to_string(),
            }]
        );

        let proposed =
            naming::session_name_for("~/oss/api", crate::config::NameMode::Directory, &existing);
        assert_eq!(proposed.name, "oss/api");
        assert!(proposed.collision.unwrap().contains("~/work/api"));
    }
}
