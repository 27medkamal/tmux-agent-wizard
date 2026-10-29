//! `taw open [TERMS]...`: tmux-session-wizard's `t <path or zoxide term>`, for someone at a
//! shell. It switches to, or creates, the session for a directory or for what zoxide finds,
//! and takes the terminal there.
//!
//! The terms, joined with single spaces, are tried as a directory first (a leading `~`
//! expanded, the rest absolute or relative to where the shell is, as `cd` would read it),
//! and otherwise handed to zoxide, one keyword each, as `z foo bar` does. The directory is
//! opened through the very code `taw session open` runs
//! ([`crate::session::create::open_or_create`]), so the create hooks, the naming, `zoxide
//! add` and a session that is already running all behave the same.
//!
//! Then it goes there. Inside tmux it switches this client, as `session open` does. Outside
//! tmux it replaces itself with `tmux attach-session`, so the terminal it was typed in
//! becomes the client. With no terms, inside tmux, it opens the popup on its new-session
//! picker, the way the plugin's key opens the popup.
//!
//! It is for people, so it prints nothing when it works, and a plain line on stderr when it
//! does not. `--json` is for scripts: the envelope `session open` prints, and nothing
//! attached; its `dir` is the folder that was opened, absolute. `--no-switch` makes or finds
//! the session and moves no client and attaches no terminal, with or without `--json`: for a
//! caller that is not the one looking at this machine (another machine's app, over ssh).

use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use serde_json::Value;

use super::envelope::{self, CliError};
use super::{Ctx, common, session, ui_open};
use crate::session::create::{HookFailure, Outcome};
use crate::session::naming::expand_tilde;
use crate::tmux::{CommandFailed, Tmux};

/// Why the terms did not lead to a directory. Each message is what `taw open` says after
/// its own name, so a terminal reads `taw open: no directory matches api`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Unresolved {
    /// Not a directory, and there is no zoxide to ask.
    #[error("{0} is not a directory, and zoxide is not installed to look it up")]
    NoZoxide(String),
    /// zoxide was asked and knows nothing like it.
    #[error("no directory matches {0}")]
    NoMatch(String),
    /// zoxide named a directory that is not there.
    #[error("zoxide matched {0}, which is not a directory")]
    Gone(String),
    /// zoxide would only name the directory the shell is in, which `--exclude` refuses: no
    /// other directory matches.
    #[error("no other directory matches {0}")]
    OnlyHere(String),
    /// zoxide could not answer, for a reason that is not "no match".
    #[error("zoxide failed: {0}")]
    ZoxideFailed(String),
    /// The directory this runs in cannot be read (it was removed under the shell, say), and
    /// what was typed is a relative path, which starts from it.
    #[error("the current directory cannot be read")]
    NoCwd,
}

/// What is left to do once the session exists.
pub(crate) enum Done {
    /// Nothing, and nothing to print.
    Quiet,
    /// The envelope `--json` prints.
    Envelope(Value),
    /// Take this terminal there: the process is to be replaced by this command.
    Attach(Command),
}

/// What `taw open` needs to know about where it runs, gathered once.
pub(crate) struct Surroundings {
    pub home: PathBuf,
    /// The current directory as the shell names it ([`logical_from`]), `None` when it cannot
    /// be read: only a relative path, or zoxide, needs it.
    pub cwd: Option<PathBuf>,
    /// The `zoxide` on `$PATH`, if there is one.
    pub zoxide: Option<PathBuf>,
    /// Whether this process runs inside tmux (`$TMUX` is set).
    pub in_tmux: bool,
    /// The pane this process was started in (`$TMUX_PANE`), which is how tmux tells which
    /// client a command comes from.
    pub pane: Option<String>,
    /// Whether this process's input and output are both a terminal: only then can it
    /// become a tmux client.
    pub terminal: bool,
}

impl Surroundings {
    pub(crate) fn from_env() -> anyhow::Result<Surroundings> {
        Ok(Surroundings {
            home: dirs::home_dir().context("resolving the home directory")?,
            cwd: std::env::current_dir()
                .ok()
                .map(|physical| logical_from(std::env::var_os("PWD"), physical)),
            zoxide: which::which("zoxide").ok(),
            in_tmux: std::env::var_os("TMUX").is_some_and(|tmux| !tmux.is_empty()),
            pane: common::current_pane().ok(),
            terminal: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        })
    }
}

/// `taw open`: see the module comment. `Some` is the envelope for `--json`, `None` means
/// there is nothing to print. Returns only when the terminal did not have to be attached:
/// attaching replaces the process, and only its failure comes back.
pub(crate) fn run(
    ctx: &Ctx,
    terms: &[String],
    json: bool,
    no_switch: bool,
) -> anyhow::Result<Option<Value>> {
    let here = Surroundings::from_env()?;
    match open_with(ctx, terms, json, no_switch, &here)? {
        Done::Quiet => Ok(None),
        Done::Envelope(envelope) => Ok(Some(envelope)),
        Done::Attach(command) => Err(attach(command)),
    }
}

/// [`run`] up to the moment the terminal is attached, which is left to the caller so that
/// everything before it can be tested.
pub(crate) fn open(
    ctx: &Ctx,
    terms: &[String],
    json: bool,
    here: &Surroundings,
) -> anyhow::Result<Done> {
    open_with(ctx, terms, json, false, here)
}

/// [`open`], with `no_switch` for a run that makes or finds the session and moves no client
/// and attaches no terminal: nothing is printed without `--json`, and the envelope says
/// `switched: false`.
pub(crate) fn open_with(
    ctx: &Ctx,
    terms: &[String],
    json: bool,
    no_switch: bool,
    here: &Surroundings,
) -> anyhow::Result<Done> {
    if typed(terms).trim().is_empty() {
        if no_switch {
            return Err(CliError::invalid_argument(
                "--no-switch needs a directory or a zoxide term",
            ));
        }
        return picker(ctx, json, here.in_tmux);
    }
    // Outside tmux the open ends by attaching this terminal, which needs one. That is asked
    // before anything is looked up or made, so a run that cannot attach leaves no session
    // behind.
    if !json && !no_switch && !here.in_tmux && !here.terminal {
        return Err(CliError::invalid_argument(
            "this is not a terminal, use --json to open without attaching",
        ));
    }
    let dir = resolve(
        terms,
        &here.home,
        here.cwd.as_deref(),
        here.zoxide.as_deref(),
    )
    .map_err(unresolved)?;
    let path = dir
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", dir.display()))?;

    if json {
        let mut envelope =
            session::open_at(ctx, path, &here.home, here.pane.as_deref(), !no_switch)?;
        // The folder the session is rooted at, as the open read it (symlinks resolved).
        let opened = std::fs::canonicalize(&dir).unwrap_or(dir);
        envelope["dir"] = Value::String(opened.to_string_lossy().into_owned());
        return Ok(Done::Envelope(envelope));
    }
    let outcome = session::open_session(ctx, path, &here.home)?;
    if no_switch {
        Ok(Done::Quiet)
    } else if here.in_tmux {
        switch(ctx, &outcome, here.pane.as_deref())?;
        Ok(Done::Quiet)
    } else {
        Ok(Done::Attach(attach_command(
            &ctx.tmux,
            &outcome.session_name,
        )))
    }
}

/// With no terms: inside tmux, the popup opens on its new-session picker, by the same
/// launcher the plugin's key runs. It returns when the popup has closed, and whatever was
/// picked was opened by the popup itself.
fn picker(ctx: &Ctx, json: bool, in_tmux: bool) -> anyhow::Result<Done> {
    if json {
        return Err(CliError::invalid_argument(
            "--json needs a directory or a zoxide term",
        ));
    }
    if !in_tmux {
        return Err(CliError::invalid_argument(
            "name a directory or a zoxide term (the picker needs tmux)",
        ));
    }
    ui_open::open(ctx, None, true)?;
    Ok(Done::Quiet)
}

/// The terms as they were typed, one line: separated by single spaces.
fn typed(terms: &[String]) -> String {
    terms.join(" ")
}

/// The directory `terms` lead to: the terms joined with single spaces name one (with `~`
/// expanded, and a relative one read from `cwd`), or zoxide, given `zoxide`'s path and
/// each term as its own keyword, finds one.
///
/// `cwd` is `None` when the directory this runs in cannot be read. A path from the root or
/// from `~` can still be named then, and a term is still zoxide's to match, asked with no
/// directory to leave out; a relative path starts from `cwd`, and is [`Unresolved::NoCwd`].
pub(crate) fn resolve(
    terms: &[String],
    home: &Path,
    cwd: Option<&Path>,
    zoxide: Option<&Path>,
) -> Result<PathBuf, Unresolved> {
    let words = typed(terms);
    if let Some(dir) = named_directory(&words, home, cwd) {
        return Ok(dir);
    }
    // A relative path starts from the directory this runs in, and with it unreadable there
    // is nowhere to start. A term needs nothing of it: zoxide is asked to leave that
    // directory out of its answer, and with it unreadable there is nothing to leave out.
    if cwd.is_none() && names_a_path(&words) && !expand_tilde(&words, home).is_absolute() {
        return Err(Unresolved::NoCwd);
    }
    let Some(zoxide) = zoxide else {
        return Err(Unresolved::NoZoxide(words));
    };
    let found = ask_zoxide(zoxide, cwd, terms, &words)?;
    let dir = PathBuf::from(&found);
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(Unresolved::Gone(found))
    }
}

/// `words` as a directory, if it names one: `~` expanded, made absolute against `cwd` (which
/// a path from the root does not need), and its `.` and `..` read as text ([`clean`]).
/// Symlinks stay as typed.
fn named_directory(words: &str, home: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let expanded = expand_tilde(words, home);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd?.join(expanded)
    };
    let dir = clean(&absolute);
    dir.is_dir().then_some(dir)
}

/// Whether `words` are written as a path (`.`, `..`, or anything with a `/` in it) and not as
/// a term for zoxide. Whether that path is relative is for the caller to tell: only a
/// relative one needs the current directory.
fn names_a_path(words: &str) -> bool {
    words == "." || words == ".." || words.contains('/')
}

/// `zoxide query --exclude <cwd> -- <term> <term>...`: what `z foo bar` asks, so zoxide's
/// own matching decides, and the directory the shell is in is never the answer. The first
/// line of what it prints is the directory. With no `cwd` (the directory the shell is in
/// cannot be read) there is nothing to leave out, and `--exclude` is left off; nor is zoxide
/// run from that directory, which a zoxide that is a shell script would complain of on its
/// stderr before it answered: it is run from `/`, which it does not look at.
fn ask_zoxide(
    zoxide: &Path,
    cwd: Option<&Path>,
    terms: &[String],
    words: &str,
) -> Result<String, Unresolved> {
    let mut command = Command::new(zoxide);
    command.arg("query");
    match cwd {
        Some(cwd) => {
            command.arg("--exclude").arg(cwd);
        }
        None => {
            command.current_dir("/");
        }
    }
    let output = command.arg("--").args(terms).output().map_err(|err| {
        Unresolved::ZoxideFailed(format!("could not run {}: {err}", zoxide.display()))
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next().filter(|line| !line.trim().is_empty());
    if output.status.success() {
        return first
            .map(str::to_string)
            .ok_or_else(|| Unresolved::NoMatch(words.to_string()));
    }
    // zoxide says `zoxide: no match found` and exits 1 when nothing matches, and `zoxide:
    // you are already in the only match` when `--exclude` leaves nothing else. Anything
    // else it says is a failure of its own, and is shown as it said it.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if output.status.code().is_none() {
        Err(Unresolved::ZoxideFailed(
            "it was stopped by a signal".to_string(),
        ))
    } else if said.contains("already in the only match") {
        Err(Unresolved::OnlyHere(words.to_string()))
    } else if said.is_empty() || said.contains("no match") {
        Err(Unresolved::NoMatch(words.to_string()))
    } else {
        Err(Unresolved::ZoxideFailed(said.to_string()))
    }
}

/// The error a failed [`resolve`] is reported as: something the person named that is not
/// there is `not_found`, a relative path with nowhere to start from is `invalid_argument`
/// (and so the exit code 2 of a command line that cannot be run as typed), and zoxide
/// failing is an error of its own.
fn unresolved(err: Unresolved) -> anyhow::Error {
    match err {
        Unresolved::ZoxideFailed(_) => anyhow::Error::new(err),
        Unresolved::NoCwd => CliError::invalid_argument(err.to_string()),
        other => CliError::not_found(other.to_string()),
    }
}

/// The current directory the way a shell names it: `$PWD`, symlinks as they were typed,
/// while it is absolute, has no `.` or `..` in it, and still names the directory this
/// process is in; otherwise the physical one. It is what zoxide's `z` reads with `pwd -L`,
/// and zoxide keeps its directories the same way, so excluding the physical path of a
/// symlinked directory would exclude nothing.
fn logical_from(pwd: Option<OsString>, physical: PathBuf) -> PathBuf {
    let Some(pwd) = pwd.map(PathBuf::from) else {
        return physical;
    };
    let plain = pwd.is_absolute()
        && pwd
            .components()
            .all(|part| !matches!(part, Component::CurDir | Component::ParentDir));
    let same = match (std::fs::metadata(&pwd), std::fs::metadata(&physical)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if plain && same { pwd } else { physical }
}

/// `path` with its `.` and `..` read as text, the way `cd` and `pwd` do: `link/..` is the
/// directory that holds `link`, not the parent of what it points at. `path` is absolute, so
/// a `..` at the root stays at the root.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Puts this client on the new session, as `session open` does, and says why when it
/// could not: the session is there either way.
fn switch(ctx: &Ctx, outcome: &Outcome, pane: Option<&str>) -> anyhow::Result<()> {
    let name = &outcome.session_name;
    common::switch_client_to(&ctx.tmux, &crate::tmux::session_target(name), pane).map_err(|err| {
        anyhow::anyhow!(
            "session {name} is ready, but switching to it failed: {}",
            why(&err)
        )
    })
}

/// `attach-session` for `session`, by its exact name, aimed at the server `tmux` talks to.
fn attach_command(tmux: &Tmux, session: &str) -> Command {
    attach_to(tmux, &crate::tmux::session_target(session))
}

/// `attach-session` for the target `target` as tmux reads it (a session id, `=name`), aimed at the server
/// `tmux` talks to. Not run here: `taw app` runs it as a child with its terminal and waits, where `taw
/// open` replaces itself with it ([`attach`]).
pub(crate) fn attach_to(tmux: &Tmux, target: &str) -> Command {
    let mut command = tmux.command();
    command.args(["attach-session", "-t"]).arg(target);
    command
}

/// [`attach_to`] for `target` as a client that has `came_from` as its last session, so that prefix + L (`switch-client
/// -l`) goes back: tmux gives a new client no last session, so the client is attached to the session the person came
/// from and switched to `target` in the same command line. With `None` it is [`attach_to`]. Use
/// [`usable_came_from`] for the session to name.
pub(crate) fn attach_from(tmux: &Tmux, came_from: Option<&str>, target: &str) -> Command {
    let Some(came_from) = came_from else {
        return attach_to(tmux, target);
    };
    let mut command = attach_to(tmux, came_from);
    command.args([";", "switch-client", "-t"]).arg(target);
    command
}

/// `came_from` when it names a session that is there and is not `target` (the session id of each, so that `$3` and
/// `=name` of one session are one), else `None`: the attach is then a plain one, as before.
pub(crate) fn usable_came_from<'a>(
    tmux: &Tmux,
    came_from: Option<&'a str>,
    target: &str,
) -> Option<&'a str> {
    let came_from = came_from?;
    let id_of = |session: &str| {
        tmux.run(&[
            "display-message",
            "-p",
            "-t",
            &format!("{session}:"),
            "#{session_id}",
        ])
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
    };
    let from = id_of(came_from)?;
    (id_of(target).as_deref() != Some(from.as_str())).then_some(came_from)
}

/// Replaces this process with `command`, so the terminal becomes the tmux client. Only a
/// failure to do that returns.
fn attach(mut command: Command) -> anyhow::Error {
    let err = command.exec();
    anyhow::anyhow!(
        "could not run {}: {err}",
        command.get_program().to_string_lossy()
    )
}

/// `taw open`'s failure for a person: one line on stderr, after its own name. Returns the
/// exit code.
pub(crate) fn report(err: &anyhow::Error) -> i32 {
    eprintln!("taw open: {}", why(err));
    exit_code(err)
}

/// The exit code for a failure without `--json`: 2 for a command line that cannot be run
/// as typed (clap's own code for usage), 1 for everything else.
fn exit_code(err: &anyhow::Error) -> i32 {
    if envelope::code_for(err) == "invalid_argument" {
        2
    } else {
        1
    }
}

/// One line saying why `err` stopped the open, in words a person reads: a create hook that
/// refused says so in its own summary, and the errors of the chain follow one another, each
/// of tmux's cut down to its reason. `taw sandbox` says its failures the same way.
pub(crate) fn why(err: &anyhow::Error) -> String {
    if let Some(hook) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<HookFailure>())
    {
        return hook.summary();
    }
    err.chain()
        .map(|cause| match cause.downcast_ref::<CommandFailed>() {
            Some(failed) => failed.reason(),
            None => reason(&cause.to_string()),
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// tmux's failures read `` `<binary> <args>` failed (<status>): <reason> `` (the shape
/// `Tmux::run` fails with), and a person wants the reason, on its first line. When tmux said
/// nothing, that is how it ended and never the command line, which is what the message of
/// a failure that has been spelled into text still holds.
fn reason(text: &str) -> String {
    match text.rsplit_once("): ") {
        Some((head, reason)) if head.contains("` failed (") => {
            let said = reason.lines().next().unwrap_or_default().trim();
            if said.is_empty() {
                let ended = head
                    .rsplit_once("` failed (")
                    .map_or("", |(_, ended)| ended);
                format!("tmux failed ({ended})")
            } else {
                said.to_string()
            }
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::cli::envelope::code_for;
    use crate::cli::test_support::{
        Stub, calls, sessions_tmux, stub_ctx, stub_tmux as shared_stub,
    };
    use crate::tmux::Socket;

    // ---------------------------------------------------------------- the client's last session

    fn argv(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// With a session to come from, the client is attached to it and switched to the target in one command
    /// line (a new client has no last session, and this gives it one); with none it is the plain attach.
    #[test]
    fn an_attach_from_a_session_goes_through_it_and_switches() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = sessions_tmux(dir.path(), &[]);
        assert_eq!(
            argv(&attach_from(&tmux, Some("$4"), "=api")),
            [
                "attach-session",
                "-t",
                "$4",
                ";",
                "switch-client",
                "-t",
                "=api"
            ]
        );
        assert_eq!(
            argv(&attach_from(&tmux, None, "=api")),
            ["attach-session", "-t", "=api"]
        );
    }

    /// A session to come from that is gone, or is the target (named by id or by name), is none: the plain attach.
    #[test]
    fn a_session_to_come_from_that_is_gone_or_is_the_target_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = sessions_tmux(
            dir.path(),
            &[("$4", "$4"), ("=other", "$4"), ("=api", "$5"), ("$5", "$5")],
        );
        assert_eq!(usable_came_from(&tmux, Some("$4"), "=api"), Some("$4"));
        assert_eq!(
            usable_came_from(&tmux, Some("=other"), "$5"),
            Some("=other")
        );
        assert_eq!(usable_came_from(&tmux, None, "=api"), None, "none given");
        assert_eq!(usable_came_from(&tmux, Some("$9"), "=api"), None, "gone");
        assert_eq!(usable_came_from(&tmux, Some("$5"), "=api"), None, "same id");
        assert_eq!(
            usable_came_from(&tmux, Some("=api"), "$5"),
            None,
            "same by name"
        );
    }

    // ---------------------------------------------------------------- helpers

    fn strings(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|t| (*t).to_string()).collect()
    }

    /// A scratch world, every path canonical (a temp directory on macOS is reached through
    /// the `/var` symlink):
    ///
    /// ```text
    /// home/x
    /// work/x
    /// work/here/sub/dir     the current directory is work/here
    /// work/here/my app
    /// work/here/link        a symlink to elsewhere/real
    /// work/here/notes.txt   a file
    /// elsewhere/real
    /// ```
    struct World {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        cwd: PathBuf,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        for dir in [
            "home/x",
            "work/x",
            "work/here/sub/dir",
            "work/here/my app",
            "elsewhere/real",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("elsewhere/real"), root.join("work/here/link"))
            .unwrap();
        fs::write(root.join("work/here/notes.txt"), "hi").unwrap();
        World {
            home: root.join("home"),
            cwd: root.join("work/here"),
            root,
            _tmp: tmp,
        }
    }

    impl World {
        fn resolve(&self, terms: &[&str]) -> Result<PathBuf, Unresolved> {
            resolve(&strings(terms), &self.home, Some(&self.cwd), None)
        }
    }

    /// A `zoxide` that records how it was run, one argument to a line, and the directory it
    /// was run from (a `cwd` file next to the arguments), then answers as the real one does:
    /// `stdout` (absolute paths, one to a line), `stderr`, and `code`. Returns the script and
    /// the file its arguments are written to.
    fn zoxide(w: &World, stdout: &str, stderr: &str, code: i32) -> (PathBuf, PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = w.root.join(format!("zoxide-{n}"));
        fs::create_dir_all(&dir).unwrap();
        let (argv, out, err, bin) = (
            dir.join("argv"),
            dir.join("out"),
            dir.join("err"),
            dir.join("zoxide"),
        );
        fs::write(&out, stdout).unwrap();
        fs::write(&err, stderr).unwrap();
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\npwd -P > '{}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\ncat '{}'\ncat '{}' >&2\nexit {code}\n",
                argv.with_file_name("cwd").display(),
                argv.display(),
                out.display(),
                err.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        (bin, argv)
    }

    fn asked(argv: &Path) -> Vec<String> {
        fs::read_to_string(argv)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    // ------------------------------------------------ terms that name a directory

    #[test]
    fn an_absolute_path_is_used_as_it_is() {
        let w = world();
        let dir = w.root.join("work/x");
        assert_eq!(w.resolve(&[dir.to_str().unwrap()]), Ok(dir));
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let w = world();
        assert_eq!(w.resolve(&["~/x"]), Ok(w.home.join("x")));
        assert_eq!(w.resolve(&["~"]), Ok(w.home.clone()));
    }

    #[test]
    fn dot_is_where_you_are() {
        let w = world();
        assert_eq!(w.resolve(&["."]), Ok(w.cwd.clone()));
    }

    #[test]
    fn dot_dot_is_the_directory_above() {
        let w = world();
        assert_eq!(w.resolve(&[".."]), Ok(w.root.join("work")));
    }

    #[test]
    fn a_relative_path_can_climb_and_come_down_again() {
        let w = world();
        assert_eq!(w.resolve(&["../x"]), Ok(w.root.join("work/x")));
    }

    #[test]
    fn a_relative_path_can_go_below_where_you_are() {
        let w = world();
        assert_eq!(w.resolve(&["sub/dir"]), Ok(w.cwd.join("sub/dir")));
    }

    #[test]
    fn dots_in_the_middle_and_a_trailing_slash_are_tidied_away() {
        let w = world();
        assert_eq!(w.resolve(&["./sub/./dir/"]), Ok(w.cwd.join("sub/dir")));
        assert_eq!(w.resolve(&["sub/dir/../dir"]), Ok(w.cwd.join("sub/dir")));
    }

    /// `taw open my app` types a path with a space in it without quoting it: the terms are
    /// joined with a single space, and that names the directory. Typed in one piece it does
    /// too.
    #[test]
    fn terms_joined_with_a_space_can_name_a_directory_with_a_space() {
        let w = world();
        assert_eq!(w.resolve(&["my", "app"]), Ok(w.cwd.join("my app")));
        assert_eq!(w.resolve(&["my app"]), Ok(w.cwd.join("my app")));
    }

    /// The way `cd` then `pwd` gives it: a symlink is kept as typed, not followed, and `..`
    /// after one is the directory that holds the link, not the parent of its target.
    #[test]
    fn a_symlinked_directory_is_kept_as_typed() {
        let w = world();
        assert_eq!(w.resolve(&["link"]), Ok(w.cwd.join("link")));
        assert_eq!(w.resolve(&["link/.."]), Ok(w.cwd.clone()));
        assert_ne!(
            w.resolve(&["link"]),
            Ok(w.root.join("elsewhere/real")),
            "the target's path is not what was typed"
        );
    }

    /// Only a directory counts: a file, or a path that is not there, is looked up as a
    /// term. With no zoxide to ask, that is the message.
    #[test]
    fn a_file_or_a_missing_path_is_not_a_directory() {
        let w = world();
        assert_eq!(
            w.resolve(&["notes.txt"]),
            Err(Unresolved::NoZoxide("notes.txt".to_string()))
        );
        assert_eq!(
            w.resolve(&["nowhere/at/all"]),
            Err(Unresolved::NoZoxide("nowhere/at/all".to_string()))
        );
    }

    // ----------------------------------------- a current directory that is gone

    /// A shell can be standing in a directory that was removed under it (a worktree deleted
    /// while it was the current directory), and the directory it is in cannot be read then.
    /// A path that starts at the root or at `~` needs nothing of it.
    #[test]
    fn a_path_from_the_root_or_the_home_needs_no_current_directory() {
        let w = world();
        let dir = w.root.join("work/x");
        for (typed, want) in [
            (dir.to_str().unwrap(), dir.clone()),
            ("~/x", w.home.join("x")),
            ("~", w.home.clone()),
        ] {
            assert_eq!(
                resolve(&strings(&[typed]), &w.home, None, None),
                Ok(want),
                "{typed}"
            );
        }
    }

    /// A relative path starts from the directory the shell is in, and with that unreadable
    /// there is nowhere to start from: it says so rather than guess. `.` and `..`, and
    /// anything with a `/` in it, are paths. zoxide is not run.
    #[test]
    fn a_relative_path_needs_the_current_directory() {
        let w = world();
        let (bin, argv) = zoxide(&w, &format!("{}\n", w.home.display()), "", 0);
        for terms in [
            &["."][..],
            &[".."],
            &["../x"],
            &["./x"],
            &["sub/dir"],
            &["my/app", "x"],
        ] {
            assert_eq!(
                resolve(&strings(terms), &w.home, None, Some(&bin)),
                Err(Unresolved::NoCwd),
                "{terms:?}"
            );
        }
        assert!(!argv.exists(), "zoxide was run");
    }

    /// A path from the root or from `~` that is not a directory is no relative path: it needs
    /// no current directory, so it is not blamed on it. It is looked up as a term is, and
    /// said to match nothing.
    #[test]
    fn a_missing_path_from_the_root_or_the_home_is_not_blamed_on_the_current_directory() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "zoxide: no match found\n", 1);
        for typed in ["/nowhere/x", "~/nowhere"] {
            assert_eq!(
                resolve(&strings(&[typed]), &w.home, None, Some(&bin)),
                Err(Unresolved::NoMatch(typed.to_string())),
                "{typed}"
            );
        }
    }

    /// A relative path with nowhere to start from is a path that cannot be used as typed, so
    /// `--json` reports it as `invalid_argument`, and a terminal as the exit code 2 of a
    /// command line that cannot be run as typed, like the picker outside tmux.
    #[test]
    fn a_relative_path_with_nowhere_to_start_is_an_invalid_argument() {
        let err = unresolved(Unresolved::NoCwd);

        assert_eq!(code_for(&err), "invalid_argument");
        assert_eq!(why(&err), "the current directory cannot be read");
        assert_eq!(exit_code(&err), 2);
    }

    /// With the directory this runs in gone, zoxide is not handed it as its own: a zoxide that
    /// is a shell script (a shim) says so on its stderr before it answers, and that, for a
    /// term it has no match for, would be taken for its answer. It is run from `/`, which it
    /// does not look at.
    #[test]
    fn with_the_current_directory_gone_zoxide_is_run_from_the_root() {
        let w = world();
        let (bin, argv) = zoxide(&w, &format!("{}\n", w.home.display()), "", 0);

        resolve(&strings(&["api"]), &w.home, None, Some(&bin)).unwrap();

        let ran_in = fs::read_to_string(argv.with_file_name("cwd")).unwrap();
        assert_eq!(ran_in.trim(), "/");
    }

    /// A term is zoxide's to match. zoxide is asked to leave out the directory the shell is
    /// in, which with that directory unreadable there is nothing to name: the question is
    /// the same without `--exclude`, and each term is still a keyword of its own.
    #[test]
    fn a_term_is_asked_of_zoxide_without_exclude_when_the_current_directory_is_gone() {
        let w = world();
        let found = w.root.join("work/x");
        let (bin, argv) = zoxide(&w, &format!("{}\n", found.display()), "", 0);

        let got = resolve(&strings(&["api"]), &w.home, None, Some(&bin));
        assert_eq!(got, Ok(found.clone()));
        assert_eq!(asked(&argv), ["query", "--", "api"]);

        let got = resolve(&strings(&["my", "app"]), &w.home, None, Some(&bin));
        assert_eq!(got, Ok(found));
        assert_eq!(asked(&argv), ["query", "--", "my", "app"]);
    }

    /// What zoxide says without a directory to leave out is what it says with one: a term it
    /// has no match for is no match, and no zoxide at all is said as it is.
    #[test]
    fn a_term_with_the_current_directory_gone_is_answered_as_ever() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "zoxide: no match found\n", 1);
        assert_eq!(
            resolve(&strings(&["api"]), &w.home, None, Some(&bin)),
            Err(Unresolved::NoMatch("api".to_string()))
        );
        assert_eq!(
            resolve(&strings(&["api"]), &w.home, None, None),
            Err(Unresolved::NoZoxide("api".to_string()))
        );
    }

    // ---------------------------------------------- terms that zoxide finds

    /// A term that is not a directory goes to zoxide, with the current directory excluded
    /// (as `z` does, so a second `z api` in the best match goes to the next one) and one
    /// argument per term.
    #[test]
    fn a_term_that_is_not_a_directory_goes_to_zoxide_with_the_current_directory_excluded() {
        let w = world();
        let found = w.root.join("work/x");
        let (bin, argv) = zoxide(&w, &format!("{}\n", found.display()), "", 0);

        let got = resolve(&strings(&["api"]), &w.home, Some(&w.cwd), Some(&bin));

        assert_eq!(got, Ok(found));
        assert_eq!(
            asked(&argv),
            ["query", "--exclude", w.cwd.to_str().unwrap(), "--", "api"]
        );
    }

    #[test]
    fn each_term_is_its_own_keyword() {
        let w = world();
        let (bin, argv) = zoxide(&w, &format!("{}\n", w.home.display()), "", 0);

        resolve(
            &strings(&["work", "api"]),
            &w.home,
            Some(&w.cwd),
            Some(&bin),
        )
        .unwrap();

        let asked = asked(&argv);
        assert_eq!(asked[3..], ["--", "work", "api"], "{asked:?}");
    }

    /// zoxide prints one path a line; the first is the directory.
    #[test]
    fn only_the_first_line_of_zoxides_answer_is_used() {
        let w = world();
        let out = format!(
            "{}\n{}\n",
            w.home.display(),
            w.root.join("work/x").display()
        );
        let (bin, _) = zoxide(&w, &out, "", 0);

        assert_eq!(
            resolve(&strings(&["x"]), &w.home, Some(&w.cwd), Some(&bin)),
            Ok(w.home.clone())
        );
    }

    /// A directory, or a path to one, never reaches zoxide: it would have nothing to say
    /// that the directory does not.
    #[test]
    fn terms_that_name_a_directory_do_not_ask_zoxide() {
        let w = world();
        let (bin, argv) = zoxide(&w, "/nowhere\n", "", 0);

        let got = resolve(&strings(&["sub/dir"]), &w.home, Some(&w.cwd), Some(&bin));

        assert_eq!(got, Ok(w.cwd.join("sub/dir")));
        assert!(!argv.exists(), "zoxide was run");
    }

    #[test]
    fn without_zoxide_the_terms_are_said_not_to_be_a_directory() {
        let w = world();
        let err = resolve(&strings(&["api", "gateway"]), &w.home, Some(&w.cwd), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "api gateway is not a directory, and zoxide is not installed to look it up"
        );
    }

    /// The real zoxide says `zoxide: no match found` on stderr and exits 1.
    #[test]
    fn zoxide_with_no_match_says_so() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "zoxide: no match found\n", 1);
        let err = resolve(
            &strings(&["api", "gateway"]),
            &w.home,
            Some(&w.cwd),
            Some(&bin),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no directory matches api gateway");
    }

    /// `--exclude` refuses the directory you are in, and when that is the only match the
    /// real zoxide says `zoxide: you are already in the only match` and exits 1 (0.9.9,
    /// found by running it). That is no other directory matching, not zoxide failing, so
    /// it is not reported as an error of its own.
    #[test]
    fn zoxide_saying_you_are_already_in_the_only_match_is_no_other_match() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "zoxide: you are already in the only match\n", 1);

        let err = resolve(&strings(&["api"]), &w.home, Some(&w.cwd), Some(&bin)).unwrap_err();

        assert_eq!(err, Unresolved::OnlyHere("api".to_string()));
        assert_eq!(err.to_string(), "no other directory matches api");
        assert_eq!(code_for(&unresolved(err)), "not_found");
    }

    #[test]
    fn zoxide_that_prints_nothing_has_no_match_either() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "", 0);
        let err = resolve(&strings(&["api"]), &w.home, Some(&w.cwd), Some(&bin)).unwrap_err();
        assert_eq!(err, Unresolved::NoMatch("api".to_string()));
    }

    /// The real zoxide filters out what is gone, but a directory can go between its
    /// answer and the open. It is said plainly, and nothing is created.
    #[test]
    fn a_match_that_is_gone_is_reported_by_name() {
        let w = world();
        let gone = w.root.join("work/removed");
        let (bin, _) = zoxide(&w, &format!("{}\n", gone.display()), "", 0);

        let err = resolve(&strings(&["removed"]), &w.home, Some(&w.cwd), Some(&bin)).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "zoxide matched {}, which is not a directory",
                gone.display()
            )
        );
    }

    /// A file is not a directory either.
    #[test]
    fn a_match_that_is_a_file_is_reported_too() {
        let w = world();
        let file = w.cwd.join("notes.txt");
        let (bin, _) = zoxide(&w, &format!("{}\n", file.display()), "", 0);
        let err = resolve(&strings(&["notes"]), &w.home, Some(&w.cwd), Some(&bin)).unwrap_err();
        assert_eq!(err, Unresolved::Gone(file.display().to_string()));
    }

    /// zoxide failing for a reason of its own is not "no match": its own words are shown.
    #[test]
    fn zoxide_failing_for_another_reason_is_reported_in_its_own_words() {
        let w = world();
        let (bin, _) = zoxide(&w, "", "zoxide: error: could not open the database\n", 1);
        let err = resolve(&strings(&["api"]), &w.home, Some(&w.cwd), Some(&bin)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "zoxide failed: zoxide: error: could not open the database"
        );
    }

    #[test]
    fn a_zoxide_that_cannot_be_run_is_reported() {
        let w = world();
        let missing = w.root.join("no-such-zoxide");
        let err = resolve(&strings(&["api"]), &w.home, Some(&w.cwd), Some(&missing)).unwrap_err();
        assert!(
            matches!(&err, Unresolved::ZoxideFailed(why) if why.contains("could not run")),
            "{err}"
        );
    }

    // ------------------------------------------------------ the current directory

    fn osstr(path: &Path) -> Option<std::ffi::OsString> {
        Some(path.as_os_str().to_os_string())
    }

    /// `$PWD` is where the shell thinks it is, symlinks and all, and is believed while it
    /// names the directory the process really is in.
    #[test]
    fn pwd_is_believed_while_it_names_the_directory_the_process_is_in() {
        let w = world();
        let physical = w.root.join("elsewhere/real");
        let logical = w.cwd.join("link");
        assert_eq!(logical_from(osstr(&logical), physical.clone()), logical);
    }

    #[test]
    fn a_pwd_that_is_stale_or_unusable_is_ignored() {
        let w = world();
        let physical = w.root.join("elsewhere/real");
        // Another directory, one that is not there, a relative one, one with dots in it,
        // and none at all.
        for pwd in [
            osstr(&w.root.join("work/x")),
            osstr(&w.root.join("gone")),
            osstr(Path::new("work/here/link")),
            osstr(&w.cwd.join("sub/../link")),
            None,
        ] {
            assert_eq!(
                logical_from(pwd.clone(), physical.clone()),
                physical,
                "{pwd:?}"
            );
        }
    }

    // ------------------------------------------------------------------ clean

    #[test]
    fn clean_reads_dots_as_text() {
        let cases = [
            ("/a/b/../c", "/a/c"),
            ("/a/./b/", "/a/b"),
            ("/a//b", "/a/b"),
            ("/a/b/../..", "/"),
            ("/..", "/"),
            ("/a/link/..", "/a"),
        ];
        for (typed, want) in cases {
            assert_eq!(clean(Path::new(typed)), Path::new(want), "{typed}");
        }
    }

    // ------------------------------------------------- what the terms are called

    /// Every message says what happened in a person's words, with none of the marks kept out
    /// of anything a user reads: an em-dash, an en-dash, an arrow or a curly quote.
    #[test]
    fn the_messages_hold_no_dash_arrow_or_curly_quote() {
        let all = [
            Unresolved::NoZoxide("a b".to_string()).to_string(),
            Unresolved::NoMatch("a b".to_string()).to_string(),
            Unresolved::Gone("/x".to_string()).to_string(),
            Unresolved::OnlyHere("a b".to_string()).to_string(),
            Unresolved::ZoxideFailed("it broke".to_string()).to_string(),
            Unresolved::NoCwd.to_string(),
        ];
        for message in all {
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(!message.contains(banned), "{message}");
            }
        }
    }

    // --------------------------------------------------- the whole open, on a stub

    /// The stand-in for `taw open`: no sessions running and no status line (so no bar), and
    /// a client attached when `clients` says so.
    fn stub_tmux(dir: &Path, clients: bool) -> (Tmux, PathBuf) {
        shared_stub(
            dir,
            &Stub {
                status: "off",
                clients,
                running: false,
                ..Stub::default()
            },
        )
    }

    /// The calls that changed something: what `open` did to the server.
    fn changes(log: &Path) -> Vec<Vec<String>> {
        crate::cli::test_support::changes(
            log,
            &[
                "new-session",
                "switch-client",
                "display-popup",
                "attach-session",
            ],
        )
    }

    fn here(w: &World, in_tmux: bool) -> Surroundings {
        Surroundings {
            home: w.home.clone(),
            cwd: Some(w.cwd.clone()),
            zoxide: None,
            in_tmux,
            pane: None,
            terminal: true,
        }
    }

    /// Inside tmux the client is switched to the session, which is made first; nothing is
    /// printed and nothing is attached.
    #[test]
    fn inside_tmux_a_directory_is_opened_and_the_client_switched() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);

        let done = open(&ctx, &strings(&["sub/dir"]), false, &here(&w, true)).unwrap();

        assert!(matches!(done, Done::Quiet));
        assert_eq!(
            changes(&log),
            vec![
                vec![
                    "new-session".to_string(),
                    "-d".to_string(),
                    "-s".to_string(),
                    "dir".to_string(),
                    "-c".to_string(),
                    w.cwd.join("sub/dir").display().to_string(),
                ],
                vec![
                    "switch-client".to_string(),
                    "-c".to_string(),
                    "/dev/ttys001".to_string(),
                    "-t".to_string(),
                    "=dir".to_string(),
                ],
            ]
        );
    }

    /// Started in a pane, the client that is moved is the one tmux says is the person's:
    /// no `-c`, so `open`, `--json` and `session open` all leave it to tmux.
    #[test]
    fn started_in_a_pane_the_client_is_left_for_tmux_to_pick() {
        let w = world();
        for json in [false, true] {
            let dir = w.root.join(format!("json-{json}"));
            fs::create_dir_all(&dir).unwrap();
            let (tmux, log) = stub_tmux(&dir, true);
            let ctx = stub_ctx(tmux);
            let mut surroundings = here(&w, true);
            surroundings.pane = Some("%5".to_string());

            open(&ctx, &strings(&["sub/dir"]), json, &surroundings).unwrap();

            let switches: Vec<Vec<String>> = changes(&log)
                .into_iter()
                .filter(|change| change[0] == "switch-client")
                .collect();
            assert_eq!(
                switches,
                vec![vec![
                    "switch-client".to_string(),
                    "-t".to_string(),
                    "=dir".to_string()
                ]],
                "json: {json}"
            );
        }
    }

    /// The home directory's session is called `~`, which tmux reads as the marked pane
    /// unless it is asked for by exact name: a switch that says `-t ~` fails with `no
    /// marked target`, or lands in whatever pane is marked. It is asked for by name,
    /// however the client is found.
    #[test]
    fn the_home_directory_is_switched_to_by_its_exact_name() {
        let w = world();
        for pane in [None, Some("%5".to_string())] {
            let dir = w.root.join(format!("pane-{}", pane.is_some()));
            fs::create_dir_all(&dir).unwrap();
            let (tmux, log) = stub_tmux(&dir, true);
            let ctx = stub_ctx(tmux);
            let mut surroundings = here(&w, true);
            surroundings.pane = pane.clone();

            open(&ctx, &strings(&["~"]), false, &surroundings).unwrap();

            let made = changes(&log).remove(0);
            assert_eq!(made[..4], ["new-session", "-d", "-s", "~"], "{pane:?}");
            let switched = changes(&log).pop().unwrap();
            assert_eq!(switched[0], "switch-client", "{pane:?}");
            assert_eq!(switched.last().unwrap(), "=~", "{pane:?}");
        }
    }

    /// Outside tmux the session is made, and the terminal is to be attached to it by exact
    /// name: nothing is switched, since there is no client of this terminal's to switch.
    #[test]
    fn outside_tmux_the_terminal_is_to_attach_to_the_new_session() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);

        let done = open(&ctx, &strings(&["sub/dir"]), false, &here(&w, false)).unwrap();

        let Done::Attach(command) = done else {
            panic!("not an attach")
        };
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), ctx.tmux.bin.as_os_str());
        assert_eq!(args, ["attach-session", "-t", "=dir"]);
        let made: Vec<String> = changes(&log).into_iter().map(|c| c[0].clone()).collect();
        assert_eq!(made, ["new-session"], "made, and no client switched");
    }

    /// `--json` prints what `session open` prints plus the folder it opened, and attaches nothing,
    /// inside tmux or not.
    #[test]
    fn json_gives_the_envelope_session_open_gives_and_attaches_nothing() {
        let w = world();
        for in_tmux in [true, false] {
            let (tmux, _) = stub_tmux(&w.root, true);
            let ctx = stub_ctx(tmux);

            let done = open(&ctx, &strings(&["sub/dir"]), true, &here(&w, in_tmux)).unwrap();

            let Done::Envelope(envelope) = done else {
                panic!("no envelope")
            };
            let dir = envelope["dir"].as_str().expect("dir").to_string();
            assert!(
                Path::new(&dir).is_absolute() && dir.ends_with("sub/dir"),
                "{dir}"
            );
            let mut without = envelope.clone();
            without.as_object_mut().unwrap().remove("dir");
            assert_eq!(
                without,
                serde_json::json!({
                    "session": "dir",
                    "created": true,
                    "collision": null,
                    "switched": true,
                })
            );
        }
    }

    /// With no client to switch, `session open` says `switched: false` and so does this.
    #[test]
    fn json_says_when_no_client_was_switched() {
        let w = world();
        let (tmux, _) = stub_tmux(&w.root, false);
        let ctx = stub_ctx(tmux);

        let Done::Envelope(envelope) =
            open(&ctx, &strings(&["sub/dir"]), true, &here(&w, false)).unwrap()
        else {
            panic!("no envelope")
        };

        assert_eq!(envelope["switched"], false);
        assert_eq!(envelope["created"], true);
    }

    /// Inside tmux with no client to switch, the session is there but the person is not: it
    /// is said, and it is a failure.
    #[test]
    fn a_switch_that_cannot_happen_is_a_failure_that_says_so() {
        let w = world();
        let (tmux, _) = stub_tmux(&w.root, false);
        let ctx = stub_ctx(tmux);

        let Err(err) = open(&ctx, &strings(&["sub/dir"]), false, &here(&w, true)) else {
            panic!("no failure")
        };

        assert_eq!(
            why(&err),
            "session dir is ready, but switching to it failed: no attached client to switch"
        );
        assert_eq!(exit_code(&err), 1);
    }

    /// Whatever zoxide names that is not there creates nothing.
    #[test]
    fn a_zoxide_match_that_is_gone_creates_no_session() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let gone = w.root.join("work/removed");
        let (bin, _) = zoxide(&w, &format!("{}\n", gone.display()), "", 0);
        let mut surroundings = here(&w, true);
        surroundings.zoxide = Some(bin);

        let Err(err) = open(&ctx, &strings(&["removed"]), false, &surroundings) else {
            panic!("no failure")
        };

        assert_eq!(
            why(&err),
            format!(
                "zoxide matched {}, which is not a directory",
                gone.display()
            )
        );
        assert_eq!(exit_code(&err), 1);
        assert_eq!(code_for(&err), "not_found");
        assert!(changes(&log).is_empty(), "{:?}", calls(&log));
    }

    #[test]
    fn a_zoxide_match_is_opened_like_a_directory_named_by_hand() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let found = w.root.join("work/x");
        let (bin, argv) = zoxide(&w, &format!("{}\n", found.display()), "", 0);
        let mut surroundings = here(&w, true);
        surroundings.zoxide = Some(bin);

        let done = open(&ctx, &strings(&["x"]), false, &surroundings).unwrap();

        assert!(matches!(done, Done::Quiet));
        assert_eq!(asked(&argv)[..2], ["query", "--exclude"]);
        let new_session = changes(&log).remove(0);
        assert_eq!(new_session[..4], ["new-session", "-d", "-s", "x"]);
        assert_eq!(new_session[5], found.display().to_string());
    }

    /// The shell may be standing in a directory that was removed under it. A directory typed
    /// from the root still opens; one typed relative to the directory it cannot read says
    /// so, in one line, and creates nothing.
    #[test]
    fn with_the_current_directory_gone_a_path_from_the_root_still_opens() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let mut surroundings = here(&w, true);
        surroundings.cwd = None;
        let dir = w.root.join("work/x");

        let done = open(
            &ctx,
            &strings(&[dir.to_str().unwrap()]),
            false,
            &surroundings,
        )
        .unwrap();

        assert!(matches!(done, Done::Quiet));
        let made = changes(&log).remove(0);
        assert_eq!(made[..4], ["new-session", "-d", "-s", "x"]);
        assert_eq!(made[5], dir.display().to_string());
    }

    #[test]
    fn with_the_current_directory_gone_a_relative_path_says_so_and_creates_nothing() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let mut surroundings = here(&w, true);
        surroundings.cwd = None;

        for terms in [&["sub/dir"][..], &["../x"]] {
            let Err(err) = open(&ctx, &strings(terms), false, &surroundings) else {
                panic!("{terms:?} opened something")
            };

            assert_eq!(why(&err), "the current directory cannot be read");
            assert_eq!(exit_code(&err), 2);
        }
        assert!(changes(&log).is_empty(), "{:?}", calls(&log));
    }

    /// A term needs no current directory: it is opened from what zoxide answers, which was
    /// asked without a directory to leave out.
    #[test]
    fn with_the_current_directory_gone_a_term_is_opened_from_zoxides_answer() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let found = w.root.join("work/x");
        let (bin, argv) = zoxide(&w, &format!("{}\n", found.display()), "", 0);
        let mut surroundings = here(&w, true);
        surroundings.cwd = None;
        surroundings.zoxide = Some(bin);

        let done = open(&ctx, &strings(&["x"]), false, &surroundings).unwrap();

        assert!(matches!(done, Done::Quiet));
        assert_eq!(asked(&argv), ["query", "--", "x"]);
        let made = changes(&log).remove(0);
        assert_eq!(made[..4], ["new-session", "-d", "-s", "x"]);
        assert_eq!(made[5], found.display().to_string());
    }

    /// The popup `taw open` opens is closed from outside (`taw popup close`) while it is up:
    /// it was shown and it is gone, and that is a normal end, nothing printed, exit 0. Any
    /// other status the popup's command ends with, with nothing said, is one line with that
    /// status in it.
    #[test]
    fn a_popup_closed_from_outside_ends_taw_open_quietly() {
        let w = world();
        let (tmux, _) = shared_stub(
            &w.root,
            &Stub {
                status: "off",
                popup_exit: 129,
                popup_says: "",
                ..Stub::default()
            },
        );

        let done = open(&stub_ctx(tmux), &[], false, &here(&w, true)).unwrap();

        assert!(matches!(done, Done::Quiet));
    }

    #[test]
    fn a_popup_that_ended_badly_is_one_line_with_its_status() {
        let w = world();
        let (tmux, _) = shared_stub(
            &w.root,
            &Stub {
                status: "off",
                popup_exit: 3,
                popup_says: "",
                ..Stub::default()
            },
        );

        let Err(err) = open(&stub_ctx(tmux), &[], false, &here(&w, true)) else {
            panic!("no failure")
        };

        assert_eq!(why(&err), "the popup ended with status 3");
        assert_eq!(exit_code(&err), 1);
    }

    /// A tmux that failed with nothing on its stderr is never shown as its command line and a
    /// colon with nothing behind it: it is how tmux ended.
    #[test]
    fn a_tmux_failure_with_nothing_said_is_never_the_command_line() {
        use std::os::unix::process::ExitStatusExt;
        let typed = crate::tmux::CommandFailed {
            command: "/usr/bin/tmux -u display-popup -E x".to_string(),
            status: std::process::ExitStatus::from_raw(2 << 8),
            stderr: String::new(),
        };
        assert_eq!(
            why(&anyhow::Error::new(typed).context("opening the popup")),
            "opening the popup: tmux ended with status 2"
        );
        // The same failure as text, after something has spelled it into a message.
        let text =
            anyhow::anyhow!("`/usr/bin/tmux -u display-popup -E x` failed (exit status: 2): ");
        assert_eq!(why(&text), "tmux failed (exit status: 2)");
    }

    /// Outside tmux the open ends by attaching this terminal, and tmux cannot attach one that
    /// is not a terminal (`open terminal failed: not a terminal`, after the session was made
    /// and left behind). So it is asked first, and when it is not one nothing is looked up or
    /// made.
    #[test]
    fn outside_tmux_with_no_terminal_to_attach_nothing_is_made() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);
        let mut surroundings = here(&w, false);
        surroundings.terminal = false;

        for terms in [&["sub/dir"][..], &["api"]] {
            let Err(err) = open(&ctx, &strings(terms), false, &surroundings) else {
                panic!("{terms:?} opened something")
            };

            assert_eq!(
                why(&err),
                "this is not a terminal, use --json to open without attaching"
            );
            assert_eq!(exit_code(&err), 2);
        }
        assert!(changes(&log).is_empty(), "{:?}", calls(&log));
    }

    /// A terminal is only needed to attach one: `--json` attaches nothing, and inside tmux the
    /// client is switched, not this terminal.
    #[test]
    fn a_terminal_is_needed_only_to_attach_one() {
        let w = world();
        for (json, in_tmux) in [(true, false), (true, true), (false, true)] {
            let (tmux, _) = stub_tmux(&w.root, true);
            let ctx = stub_ctx(tmux);
            let mut surroundings = here(&w, in_tmux);
            surroundings.terminal = false;

            let done = open(&ctx, &strings(&["sub/dir"]), json, &surroundings);

            assert!(done.is_ok(), "json {json}, in tmux {in_tmux}");
        }
    }

    /// Nothing named and nothing to look up, outside tmux: the picker is a popup, and
    /// there is none to open.
    #[test]
    fn no_terms_outside_tmux_asks_for_a_directory_or_a_term() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);

        let Err(err) = open(&ctx, &[], false, &here(&w, false)) else {
            panic!("no failure")
        };

        assert_eq!(
            why(&err),
            "name a directory or a zoxide term (the picker needs tmux)"
        );
        assert_eq!(exit_code(&err), 2);
        assert!(changes(&log).is_empty());
    }

    /// Blank terms are no terms, as tmux-session-wizard's `t ""` is a bare `t`: zoxide
    /// would match every directory it knows for an empty keyword.
    #[test]
    fn blank_terms_are_no_terms() {
        let w = world();
        for terms in [strings(&[""]), strings(&["", " "]), strings(&["  "])] {
            let (tmux, _) = stub_tmux(&w.root, true);
            let ctx = stub_ctx(tmux);
            let Err(err) = open(&ctx, &terms, false, &here(&w, false)) else {
                panic!("{terms:?} opened something")
            };
            assert_eq!(exit_code(&err), 2, "{terms:?}");
        }
    }

    /// The envelope is for a directory that was named: the picker has nothing to put in it.
    #[test]
    fn json_with_no_terms_is_refused() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);

        let Err(err) = open(&ctx, &[], true, &here(&w, true)) else {
            panic!("no failure")
        };

        assert_eq!(why(&err), "--json needs a directory or a zoxide term");
        assert_eq!(code_for(&err), "invalid_argument");
        assert!(changes(&log).is_empty());
    }

    /// `--no-switch` makes or finds a session and moves no client, so with no directory it has no picker
    /// to fall back on: it is refused, as a usage error (exit 2, `invalid_argument` under `--json`), and
    /// nothing is made.
    #[test]
    fn no_switch_with_no_terms_is_refused() {
        for json in [false, true] {
            for terms in [vec![], vec!["  ".to_string()]] {
                let w = world();
                let (tmux, log) = stub_tmux(&w.root, true);
                let ctx = stub_ctx(tmux);

                let Err(err) = open_with(&ctx, &terms, json, true, &here(&w, true)) else {
                    panic!("no failure: {terms:?} json {json}")
                };

                assert_eq!(
                    why(&err),
                    "--no-switch needs a directory or a zoxide term",
                    "{terms:?} json {json}"
                );
                assert_eq!(code_for(&err), "invalid_argument", "json {json}");
                assert_eq!(exit_code(&err), 2, "json {json}");
                assert!(changes(&log).is_empty(), "json {json}");
            }
        }
    }

    /// Inside tmux with nothing to open, the popup is opened the way the plugin's key opens
    /// it, told to start on the picker.
    #[test]
    fn no_terms_inside_tmux_opens_the_popup_on_the_picker() {
        let w = world();
        let (tmux, log) = stub_tmux(&w.root, true);
        let ctx = stub_ctx(tmux);

        let done = open(&ctx, &[], false, &here(&w, true)).unwrap();

        assert!(matches!(done, Done::Quiet));
        let popups: Vec<Vec<String>> = changes(&log);
        assert_eq!(popups.len(), 1, "{popups:?}");
        assert_eq!(popups[0][0], "display-popup");
        assert_eq!(
            popups[0].last().unwrap(),
            "/usr/local/bin/taw ui --from '%5' --client /dev/ttys001 --new-session"
        );
    }

    // ------------------------------------------------------------ failure lines

    #[test]
    fn a_usage_error_exits_two_and_everything_else_one() {
        assert_eq!(
            exit_code(&crate::cli::envelope::CliError::invalid_argument("x")),
            2
        );
        assert_eq!(
            exit_code(&crate::cli::envelope::CliError::not_found("x")),
            1
        );
        assert_eq!(exit_code(&anyhow::anyhow!("boom")), 1);
    }

    /// tmux's failures read `` `<binary> <args>` failed (<status>): <reason> ``, and a person
    /// wants the reason. A create hook that refused has a one-line summary of its own.
    #[test]
    fn a_failure_is_said_in_a_persons_words() {
        let tmux = anyhow::anyhow!(
            "`/usr/bin/tmux new-session -d -s api -c /x` failed (exit status: 1): duplicate session: api"
        );
        assert_eq!(why(&tmux), "duplicate session: api");
        assert_eq!(
            why(&crate::cli::envelope::CliError::not_found(
                "no such directory: /x"
            )),
            "no such directory: /x"
        );
        let hook = crate::session::create::HookFailure::TimedOut {
            hook: crate::session::create::Hook::Pre,
            limit: std::time::Duration::from_secs(10),
        };
        assert_eq!(
            why(&anyhow::Error::new(hook)),
            "pre-create hook took longer than 10 s"
        );
    }

    #[test]
    fn attach_names_the_session_exactly_and_the_socket_the_client_uses() {
        for (socket, prefix) in [
            (None, vec![]),
            (Some(Socket::Name("s".to_string())), vec!["-L", "s"]),
            (
                Some(Socket::Path(PathBuf::from("/tmp/t/sock"))),
                vec!["-S", "/tmp/t/sock"],
            ),
        ] {
            let tmux = Tmux {
                bin: PathBuf::from("/usr/bin/tmux"),
                socket,
            };
            let command = attach_command(&tmux, "my api");
            let args: Vec<String> = command
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let mut want: Vec<String> = prefix.iter().map(|s| (*s).to_string()).collect();
            want.extend(["attach-session", "-t", "=my api"].map(String::from));
            assert_eq!(args, want);
        }
        // A name that ends in `;` would end tmux's command: it goes with a `\` before it, as every
        // session target does, and a `#` as it is, since tmux expands no format in a target.
        let tmux = Tmux {
            bin: PathBuf::from("/usr/bin/tmux"),
            socket: None,
        };
        let command = attach_command(&tmux, "C#Sharp;");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["attach-session", "-t", "=C#Sharp\\;"]);
    }
}
