//! `taw machine`: the machines of `[machines]`. `list` prints each one with what is known of it.
//!
//! Not `taw --machine <name> <verb>`: that is the option, handled before this parser sees the line
//! ([`crate::machine::forward`]). This is the verb group of the same name, which runs here.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Subcommand;
use serde_json::{Value, json};

use super::envelope::{CliError, UsageRefusal};
use super::{Ctx, Output, Sub};
use crate::machine::Machine;
use crate::machine::config::{config_file_word, load_machines};
use crate::machine::edit;
use crate::machine::ssh::{self, Mode, Ssh};
use crate::machine::wire::{self, Down, Hello};
use crate::machine::words::{self, Heard, Outcome};
use crate::machine::{files, forward};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// The machines in [machines], in the order the config lists them, with whether each is live
    List,
    /// Add a machine to [machines] in the config file, keeping every comment and line of it (a new file
    /// when there is none). A running app picks it up within a couple of seconds
    Add {
        /// What the app, `--machine` and the headers call the machine: lowercase letters, digits, - and _,
        /// starting with a letter, at most 32 characters
        name: String,
        /// What you type after `ssh`: a Host from your ssh config, or user@host
        ssh_host: String,
        /// Where taw is on that machine, when it is not on the PATH that ssh gives (~/ is its home folder)
        #[arg(long, value_name = "PATH")]
        taw: Option<String>,
    },
    /// Remove a machine from [machines] in the config file: its line, or its [machines.<name>] table, and
    /// nothing else. A running app stops showing it within a couple of seconds
    Remove {
        /// The machine to remove, as `taw machine list` names it
        name: String,
    },
    /// Try each machine of [machines] once, with a connection of its own, and say how each answered
    Check {
        /// The machine to check (every machine of [machines] when none is named)
        name: Option<String>,
        /// Print one JSON envelope instead of a line for each machine
        #[arg(long)]
        json: bool,
        /// How long to wait for a machine, in milliseconds
        #[arg(long, hide = true, default_value_t = 15_000)]
        deadline_ms: u64,
    },
    /// What the app runs on a machine to list it: this machine's tmux state as lines of JSON on standard
    /// output, until standard input ends
    Feed {
        /// The protocol the app speaks (the first line says which this taw speaks)
        #[arg(long, default_value_t = 1)]
        protocol: u32,
        /// Say hello and end
        #[arg(long)]
        once: bool,
    },
    /// What the app runs on a machine, over ssh with a terminal, to put that machine's tmux on the app's
    /// terminal: attach to a pane (%N), a session ($N) or a window (@N) of this machine
    Attach {
        /// The pane, session or window to attach to, by its tmux id
        #[arg(required_unless_present = "new", conflicts_with = "new")]
        target: Option<String>,
        /// Start a new tmux session here, in your home folder, and attach to it, as this machine's own shell
        /// would (for a machine with no tmux running)
        #[arg(long)]
        new: bool,
        /// The lease holder of the app that runs this: what the key back is addressed to
        #[arg(long, hide = true)]
        holder: String,
    },
}

/// What `taw machine attach` says without a terminal on standard input.
pub const ATTACH_NO_TERMINAL: &str =
    "taw machine attach: needs a terminal (the app runs it over ssh -t)";

/// What `taw machine attach` says of a target that is no id of tmux's or names nothing on this machine.
pub fn attach_no_such(target: &str) -> String {
    format!("taw machine attach: {target} is no pane, session or window here")
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List => ("list", list(ctx)),
        Cmd::Add {
            name,
            ssh_host,
            taw,
        } => ("add", add(ctx, &name, &ssh_host, taw.as_deref())),
        Cmd::Remove { name } => ("remove", remove(ctx, &name)),
        // `direct` answers these two before this is called, so no other word is ever told for them.
        Cmd::Feed { .. } | Cmd::Check { .. } | Cmd::Attach { .. } => {
            unreachable!("told apart in `direct`")
        }
    }
}

/// The verbs of this group that do not answer with one envelope: `feed` writes JSON lines itself, and
/// `check` a line for each machine (or the envelope with `--json`, and a status of its own).
pub(super) fn direct(ctx: &Ctx, cmd: &Cmd) -> Option<(Sub, anyhow::Result<Output>)> {
    match cmd {
        Cmd::Feed { protocol, once } => Some(("feed", feed(ctx, *protocol, *once))),
        Cmd::Attach {
            target,
            new,
            holder,
        } => Some((
            "attach",
            attach(ctx, target.as_deref().filter(|_| !*new), holder),
        )),
        Cmd::Check {
            name,
            json,
            deadline_ms,
        } => Some((
            "check",
            check(
                ctx,
                name.as_deref(),
                *json,
                Duration::from_millis(*deadline_ms),
            ),
        )),
        Cmd::List | Cmd::Add { .. } | Cmd::Remove { .. } => None,
    }
}

/// A failure of a verb that is not an envelope, told as one line on standard error.
pub(super) fn report(err: &anyhow::Error) -> i32 {
    eprintln!("taw machine: {err:#}");
    1
}

fn feed(ctx: &Ctx, protocol: u32, once: bool) -> anyhow::Result<Output> {
    let options = crate::machine::feed::Options { protocol, once };
    // A closed output ends the feed quietly; any other failure is told on standard error.
    crate::machine::feed::run(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        options,
        std::io::stdin(),
        std::io::stdout(),
    )?;
    Ok(Output::Exit(0))
}

/// `taw machine attach`: this terminal becomes a client of this machine's tmux on `target`. The client is
/// recorded first (`attached/<terminal>.json`, see [`files::AttachedRecord`]; the attach is made even when
/// the record cannot be written) and the process is then
/// replaced by `tmux attach-session` (with `TERM=xterm-256color` when this machine's terminfo does not have
/// the terminal's own type, see [`crate::machine::terminfo`]), so nothing is left to take the record away when the client ends: the
/// key that reads it ([`files::app_client`]) does that. Returns only when the attach could not be made.
///
/// With no `target` (`--new`) it is `tmux new-session` with no name, in the home folder, that is run: tmux
/// numbers the session, starts the server when none runs, and the shell in it is the user's own.
fn attach(ctx: &Ctx, target: Option<&str>, holder: &str) -> anyhow::Result<Output> {
    // A refusal is about what was asked, so it comes before anything else is looked at: first the id, then
    // whether this machine has what it names. tmux does not fail for a target it cannot find: it prints an
    // empty pane id (exit 0), so an empty answer is the refusal.
    if let Some(target) = target {
        let named = crate::machine::valid_tmux_id(target)
            && ctx
                .tmux
                .run(&["display-message", "-t", target, "-p", "#{pane_id}"])
                .is_ok_and(|pane| !pane.trim().is_empty());
        if !named {
            return Err(UsageRefusal(attach_no_such(target)).into());
        }
    }
    let Some(tty) = terminal_name() else {
        return Err(UsageRefusal(ATTACH_NO_TERMINAL.to_string()).into());
    };
    // The record only serves the key back: one that cannot be written (a state folder that is not writable,
    // a full disk) loses that and nothing else, so the attach goes ahead and the key opens the popup.
    let recorded = files::write_attached(
        &ctx.paths,
        &files::AttachedRecord {
            tty: tty.clone(),
            holder: holder.to_string(),
            at: files::now_ms(),
            // `exec` keeps the process, so this is the tmux client's own.
            pid: std::process::id(),
        },
    );
    if let Err(err) = &recorded {
        log::debug!("could not record the client on {tty}: {err}");
    }
    let mut command = far_command(&ctx.tmux, target, home_dir().as_deref());
    // A terminal type this machine's terminfo lacks (Ghostty's, Kitty's) would end the client with
    // `missing or unsuitable terminal`, so it runs with one every machine has. A known one is kept.
    if let Some(term) = crate::machine::terminfo::replacement_here() {
        command.env("TERM", term);
    }
    // The client's exit line (`[detached (from session 3)]`) would go through ssh to the app's terminal and show
    // until the app's list is drawn again. The client draws through its standard input and says what went wrong
    // on its standard error, so only that line goes.
    command.stdout(Stdio::null());
    let err = command.exec();
    if recorded.is_ok() {
        files::remove_attached(&ctx.paths, &tty);
    }
    anyhow::bail!(
        "could not run {}: {err}",
        command.get_program().to_string_lossy()
    )
}

/// The tmux command `taw machine attach` becomes: `attach-session` on `target`, or with none (`--new`)
/// `new-session` with no name, run in `home` when it is known (tmux starts it in the folder it is run from,
/// which is the home folder after an ssh login, and `home` makes that certain), and then `display-message` of
/// [`words::BACK_HINT`].
fn far_command(tmux: &crate::tmux::Tmux, target: Option<&str>, home: Option<&Path>) -> Command {
    let mut command = match target {
        Some(target) => super::open::attach_to(tmux, target),
        None => {
            let mut command = tmux.command();
            command.arg("new-session");
            if let Some(home) = home {
                command.current_dir(home);
            }
            command
        }
    };
    // The same tmux invocation goes on once the client is attached (`attach-session ... \; display-message`): the
    // way back to the list, on the status line for a few seconds.
    command.args([
        ";",
        "display-message",
        "-d",
        words::BACK_HINT_MS,
        words::BACK_HINT,
    ]);
    command
}

/// The home folder of the user this runs as, from `$HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
}

/// The terminal on standard input as `ttyname` gives it, or `None` when standard input is not one.
fn terminal_name() -> Option<String> {
    // SAFETY: `isatty` and `ttyname` take a descriptor number; `ttyname` returns a pointer into static
    // storage or null, and the string is copied before anything else can call it.
    unsafe {
        if libc::isatty(0) != 1 {
            return None;
        }
        let name = libc::ttyname(0);
        if name.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned();
        (!name.is_empty()).then_some(name)
    }
}

/// What `add` and `remove` say of the app: a running app looks at the config file and picks the edit up.
const APP_NOTE: &str = "A running app picks this up within a couple of seconds.";

/// The config file as it is now: the file that is written (the target of a link, so that a config kept
/// elsewhere stays a link), its text ("" for a file that is not there), and its permissions.
fn read_config(ctx: &Ctx) -> anyhow::Result<(PathBuf, String, Option<std::fs::Permissions>)> {
    let path = &ctx.paths.config_file;
    let unread = |why: String| {
        CliError::invalid_argument(format!(
            "config {} could not be read ({})",
            config_file_word(&ctx.paths),
            crate::config::one_line(&why)
        ))
    };
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((path.clone(), String::new(), None));
        }
        Err(err) => return Err(unread(err.to_string())),
        Ok(_) => {}
    }
    let target = std::fs::canonicalize(path).map_err(|err| unread(err.to_string()))?;
    let text = std::fs::read_to_string(&target).map_err(|err| unread(err.to_string()))?;
    let permissions = std::fs::metadata(&target).ok().map(|m| m.permissions());
    Ok((target, text, permissions))
}

/// Writes the new text of the config whole, as the file had its permissions.
fn write_config(
    ctx: &Ctx,
    target: &Path,
    text: &str,
    permissions: Option<std::fs::Permissions>,
) -> anyhow::Result<()> {
    crate::atomic_file::write_whole(target, text, "config.toml").map_err(|err| {
        anyhow::anyhow!(
            "config {} could not be written ({err})",
            config_file_word(&ctx.paths)
        )
    })?;
    if let Some(permissions) = permissions {
        // The file keeps the permissions it had; a failure here leaves the ones the atomic write
        // gave it, readable by you alone.
        let _ = std::fs::set_permissions(target, permissions);
    }
    Ok(())
}

fn add(ctx: &Ctx, name: &str, ssh_host: &str, taw: Option<&str>) -> anyhow::Result<Value> {
    let (target, text, permissions) = read_config(ctx)?;
    let text = edit::add(&text, name, ssh_host, taw)
        .map_err(|err| CliError::invalid_argument(err.to_string()))?;
    write_config(ctx, &target, &text, permissions)?;
    Ok(json!({
        "machine": { "name": name, "destination": ssh_host, "taw": taw },
        "config": config_file_word(&ctx.paths),
        "note": APP_NOTE,
    }))
}

fn remove(ctx: &Ctx, name: &str) -> anyhow::Result<Value> {
    let (target, text, permissions) = read_config(ctx)?;
    let text = edit::remove(&text, name).map_err(|err| match err {
        edit::EditError::Unknown(_) => CliError::not_found(err.to_string()),
        err => CliError::invalid_argument(err.to_string()),
    })?;
    write_config(ctx, &target, &text, permissions)?;
    Ok(json!({
        "removed": name,
        "config": config_file_word(&ctx.paths),
        "note": APP_NOTE,
    }))
}

/// Every machine of the config in its order. A table taw cannot read whole is refused in the words
/// `taw --machine` uses for it ([`crate::machine::config::MachinesDoubt`]).
fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    let machines =
        load_machines(&ctx.paths).map_err(|doubt| CliError::invalid_argument(doubt.to_string()))?;
    let keeper = files::keeper_alive(&ctx.paths);
    let now = files::now_ms();
    let rows: Vec<Value> = machines
        .iter()
        .map(|machine| {
            let file = files::read(&ctx.paths, &machine.name).unwrap_or_default();
            json!({
                "name": machine.name,
                "destination": machine.ssh,
                "taw": machine.taw,
                "live": files::is_live(&file, keeper, now),
                "last_seen": file.last_seen,
                "taw_version": file.taw_version,
                "protocol": file.protocol,
                "words": files::words_shown(&file, keeper),
            })
        })
        .collect();
    Ok(json!({ "machines": rows }))
}

/// How one machine answered, and what it said.
struct Checked {
    machine: Machine,
    outcome: Outcome,
    /// The version of the taw there, when the answer said it.
    taw_version: Option<String>,
    protocol: Option<u32>,
    /// The last line ssh or the shell said on standard error.
    ssh_said: String,
}

impl Checked {
    fn connected(&self) -> bool {
        matches!(self.outcome, Outcome::Connected { .. })
    }

    /// The sentence for the outcome, without what is added after it.
    fn words(&self) -> String {
        let theirs = self.taw_version.clone().unwrap_or_default();
        match &self.outcome {
            Outcome::Connected { taw } => words::connected(taw),
            Outcome::NoTaw => words::no_taw(),
            Outcome::NoMachines => words::no_machines(&theirs),
            Outcome::Newer { taw } => words::newer(taw, env!("CARGO_PKG_VERSION")),
            Outcome::Login => words::login(),
            Outcome::HostKey => words::host_key(&self.machine.ssh),
            Outcome::NotAnswering => words::not_answering_once(),
            Outcome::Garbage => words::garbage_header(),
        }
    }
}

/// What one ssh run said.
struct Attempt {
    /// `None` when it was ended by a signal, which is also how it ends at the deadline.
    exit: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// How long the streams of a run that has ended are given, together, to reach their end.
const DRAIN_WAIT: Duration = Duration::from_secs(1);

/// The most of a stream that is kept.
const KEPT: u64 = 1 << 20;

/// A stream being read on a thread: what has come in so far, and a signal that the end was reached.
struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: Receiver<()>,
}

/// Reads `pipe` to its end or to [`KEPT`] bytes, whichever is first, then lets go of it.
fn drain(mut pipe: impl Read + Send + 'static) -> Drain {
    let (tx, done) = std::sync::mpsc::channel();
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&bytes);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let want = chunk
                .len()
                .min(KEPT as usize - kept.lock().map_or(0, |held| held.len()));
            if want == 0 {
                break;
            }
            match pipe.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut held) = kept.lock() {
                        held.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        let _ = tx.send(());
    });
    Drain { bytes, done }
}

impl Drain {
    /// What was read, once the end came or `until` has passed: a stream that a child of ssh still holds
    /// open is not waited for, and what it had said by then is kept. The caller gives every stream of one
    /// run the same instant, so that the waits do not add up.
    fn finish(self, until: Instant) -> Vec<u8> {
        let _ = self
            .done
            .recv_timeout(until.saturating_duration_since(Instant::now()));
        self.bytes
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }
}

/// How long ssh is given to end its own helpers after SIGTERM before the group is killed.
const GRACE: Duration = Duration::from_millis(300);

/// Ends the group `child` leads (what ssh started, a ProxyCommand or a ProxyJump, is in it): SIGTERM, a
/// short grace, SIGKILL, and the child waited for so that it is no zombie.
fn end_group(child: &mut Child) {
    let pid = child.id() as i32;
    debug_assert!(pid > 1, "a child's pid is more than 1: {pid}");
    let group = -pid;
    // SAFETY: signals the process group of a child this run started with `process_group(0)`: its pid is
    // the group's, by pid and never by name.
    unsafe { libc::kill(group, libc::SIGTERM) };
    let until = Instant::now() + GRACE;
    while Instant::now() < until && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: as above; a group with no one left in it makes this a no-op. When the leader was reaped in
    // the loop, the group's id is still its pid, and pids are given out in turn, so no other process has
    // it within the grace.
    unsafe { libc::kill(group, libc::SIGKILL) };
    // The child itself, in case ssh left the group and the signals above missed it: the wait cannot hang.
    let _ = child.kill();
    let _ = child.wait();
}

/// How many machines' groups the signal handler can end at once. A machine beyond this many is simply
/// not covered: its ssh is still ended at the deadline, but not when taw itself is signalled.
const GROUP_SLOTS: usize = 128;

/// The ids of the process groups of the ssh runs that are live, for the signal handler. 0 is an empty
/// slot; any other value is a child's pid, which is the id of the group it leads.
static GROUPS: [AtomicI32; GROUP_SLOTS] = [const { AtomicI32::new(0) }; GROUP_SLOTS];

/// A place in [`GROUPS`] held for one live ssh run, and given back on every path, a panic included.
struct GroupSlot(usize);

impl GroupSlot {
    /// Records the group `leader` leads, or `None` when every slot is taken.
    fn hold(leader: u32) -> Option<GroupSlot> {
        let group = i32::try_from(leader).ok().filter(|group| *group > 1)?;
        GROUPS.iter().enumerate().find_map(|(at, slot)| {
            slot.compare_exchange(0, group, Ordering::SeqCst, Ordering::SeqCst)
                .ok()
                .map(|_| GroupSlot(at))
        })
    }
}

impl Drop for GroupSlot {
    fn drop(&mut self) {
        GROUPS[self.0].store(0, Ordering::SeqCst);
    }
}

/// The handler of SIGINT, SIGTERM and SIGHUP for `check`: SIGTERM to each live group, then the signal
/// it got with its default effect, so that taw still ends by it (^C still reads as an interrupted command
/// to the shell). Each ssh runs in a group of its own, so a ^C at the terminal reaches taw only and
/// would leave ssh and its ProxyCommand running if taw did not end them here. It does only what a
/// handler may: atomic loads, `kill`, `sigaction` and `raise`. A SIGKILL of taw cannot be caught: ssh
/// then ends by EPIPE at its next write, by `ConnectTimeout` or by ServerAlive.
extern "C" fn end_groups_and_die(signal: libc::c_int) {
    for slot in &GROUPS {
        let group = slot.load(Ordering::SeqCst);
        // Never pid 0, pid 1 or a negative id: a slot holds a child's pid, which is more than 1.
        if group > 1 {
            // SAFETY: signals the group a child of this run leads, by its id.
            unsafe { libc::kill(-group, libc::SIGTERM) };
        }
    }
    // SAFETY: a zeroed `sigaction` with the default handler puts the default effect back; `raise` then
    // sends the signal to this thread, delivered when the handler returns.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        libc::raise(signal);
    }
}

/// Installs [`end_groups_and_die`] for the signals that end a command at the terminal or its shell.
fn end_groups_on_signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed `sigaction` is the empty one; the handler is an `extern "C"` function that
        // does only what a handler may.
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction =
                end_groups_and_die as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        if installed != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Runs `cmd` with no input, and ends it and what it started when `deadline` has passed. A stream that
/// a child of it still holds open is given a second, not waited for.
fn attempt(mut cmd: Command, deadline: Duration) -> Attempt {
    // In a group of its own, so that the end at the deadline reaches what it started too.
    cmd.process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Attempt {
                exit: None,
                stdout: Vec::new(),
                stderr: format!("ssh could not be run ({err})").into_bytes(),
            };
        }
    };
    // Its group is on the list for as long as it runs, for the handler of a signal to taw.
    let slot = GroupSlot::hold(child.id());
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let end = Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                end_group(&mut child);
                break None;
            }
        }
    };
    // Waited for, so the group is no longer one to signal.
    drop(slot);
    // One second in all for both streams, from now.
    let until = Instant::now() + DRAIN_WAIT;
    Attempt {
        exit: status,
        stdout: out.map(|drain| drain.finish(until)).unwrap_or_default(),
        stderr: err.map(|drain| drain.finish(until)).unwrap_or_default(),
    }
}

/// The version a taw gives for `--version` (`taw 0.0.9`), when it does.
fn version_of(ssh: &Ssh, ctx: &Ctx, machine: &Machine, deadline: Duration) -> Option<String> {
    if deadline.is_zero() {
        return None;
    }
    let cmd = ssh::command(ssh, &ctx.paths, machine, Mode::Fresh, &["--version"]);
    let run = attempt(cmd, deadline);
    let text = String::from_utf8_lossy(&run.stdout).into_owned();
    // What the far machine printed reaches this terminal only as plain text, and short.
    let first = wire::clean_text(text.lines().next()?);
    let version = first.trim().strip_prefix("taw ")?.trim();
    (!version.is_empty()).then(|| shortened(version))
}

/// The most characters of a far machine's version the check shows.
const VERSION_SHOWN: usize = 64;

fn shortened(text: &str) -> String {
    text.chars().take(VERSION_SHOWN).collect()
}

fn check_one(ssh: &Ssh, ctx: &Ctx, machine: &Machine, deadline: Duration) -> Checked {
    let cmd = ssh::command(
        ssh,
        &ctx.paths,
        machine,
        Mode::Fresh,
        &["machine", "feed", "--protocol", "1", "--once"],
    );
    let started = Instant::now();
    let run = attempt(cmd, deadline);
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let first = stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let parsed = first.map(wire::parse_down);
    // The far machine's own words: through the cleaning before anything uses them.
    let hello: Option<Hello> = match parsed {
        Some(Ok(line @ Down::Hello(_))) => match line.cleaned() {
            Down::Hello(mut hello) => {
                hello.taw = shortened(&hello.taw);
                Some(hello)
            }
            _ => None,
        },
        _ => None,
    };
    let heard = match (&hello, first) {
        (Some(hello), _) => Heard::Hello(hello),
        (None, Some(_)) => Heard::Other,
        (None, None) => Heard::Nothing,
    };
    let ssh_said_whole = ssh::last_line(&run.stderr);
    let outcome = words::classify(run.exit, &ssh_said_whole, heard);
    let ssh_said = ssh::shown(&ssh_said_whole);
    let taw_version = match &outcome {
        Outcome::Connected { taw } | Outcome::Newer { taw } => Some(taw.clone()),
        // A taw that does not know `machine` does not say its version there: ask it.
        // What is left of this machine's one deadline, not a fresh one.
        Outcome::NoMachines => version_of(
            ssh,
            ctx,
            machine,
            deadline.saturating_sub(started.elapsed()),
        ),
        _ => None,
    };
    Checked {
        machine: machine.clone(),
        outcome,
        taw_version,
        protocol: hello.as_ref().map(|h| h.protocol),
        ssh_said,
    }
}

/// The lines for the machines: names padded to the longest and two spaces, the words, the hint for the
/// outcomes that need you, and ssh's last line under the words.
fn check_lines(all: &[Checked]) -> String {
    let width = all
        .iter()
        .map(|c| c.machine.name.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let mut out = String::new();
    for checked in all {
        let name = &checked.machine.name;
        let mut line = format!("{name:<width$}{}", checked.words());
        if checked.outcome.needs_you() {
            line.push_str(&words::needs_you_hint());
        }
        out.push_str(&line);
        out.push('\n');
        if !checked.connected() && !checked.ssh_said.is_empty() {
            out.push_str(&format!("{:width$}{}\n", "", checked.ssh_said));
        }
    }
    out
}

fn check(ctx: &Ctx, name: Option<&str>, json: bool, deadline: Duration) -> anyhow::Result<Output> {
    let machines =
        load_machines(&ctx.paths).map_err(|doubt| CliError::invalid_argument(doubt.to_string()))?;
    let chosen: Vec<&Machine> = match name {
        Some(name) => match machines.iter().find(|m| m.name == name) {
            Some(machine) => vec![machine],
            None => {
                return Err(
                    UsageRefusal(forward::unknown_name(name, &machines, &ctx.paths)).into(),
                );
            }
        },
        None => machines.iter().collect(),
    };
    if chosen.is_empty() {
        if json {
            return Ok(Output::EnvelopeExit(json!({ "machines": [] }), 0));
        }
        println!(
            "no machines in [machines] ({})",
            config_file_word(&ctx.paths)
        );
        return Ok(Output::Exit(0));
    }

    // Before any thread or ssh: a signal to taw from here on ends what it started.
    end_groups_on_signals()?;
    // Each machine on its own connection, all at once: one that does not answer holds up only itself.
    let ssh = Ssh::from_env();
    let checked: Vec<Checked> = std::thread::scope(|scope| {
        let tries: Vec<_> = chosen
            .iter()
            .map(|machine| scope.spawn(|| check_one(&ssh, ctx, machine, deadline)))
            .collect();
        tries
            .into_iter()
            .zip(&chosen)
            .map(|(handle, machine)| {
                handle.join().unwrap_or_else(|_| Checked {
                    machine: (*machine).clone(),
                    outcome: Outcome::NotAnswering,
                    taw_version: None,
                    protocol: None,
                    ssh_said: String::new(),
                })
            })
            .collect()
    });
    let code = i32::from(!checked.iter().all(Checked::connected));
    if json {
        let rows: Vec<Value> = checked
            .iter()
            .map(|c| {
                json!({
                    "name": c.machine.name,
                    "destination": c.machine.ssh,
                    "connected": c.connected(),
                    "words": c.words(),
                    "taw_version": c.taw_version,
                    "protocol": c.protocol,
                    "ssh_said": c.ssh_said,
                })
            })
            .collect();
        return Ok(Output::EnvelopeExit(json!({ "machines": rows }), code));
    }
    print!("{}", check_lines(&checked));
    Ok(Output::Exit(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_streams_that_stay_open_share_one_instant_and_do_not_wait_one_after_the_other() {
        // Two pipes whose writing ends stay open: neither stream ever ends.
        let (first, _keep_first) = std::io::pipe().unwrap();
        let (second, _keep_second) = std::io::pipe().unwrap();
        let (out, err) = (drain(first), drain(second));
        let until = Instant::now() + Duration::from_millis(400);
        let _ = out.finish(until);
        // The first has waited until the instant; the second has none left to wait.
        let before = Instant::now();
        let _ = err.finish(until);
        let second_took = before.elapsed();
        assert!(
            second_took < Duration::from_millis(150),
            "the second stream waited {second_took:?} more"
        );
        assert!(
            Instant::now() >= until,
            "the first did not wait for the instant"
        );
    }
}

#[cfg(test)]
mod new_session_tests {
    use super::*;
    use crate::tmux::Tmux;

    fn words_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// `--new` runs `new-session` with no name and no target, in the home folder; a target runs `attach-session`.
    #[test]
    fn new_runs_new_session_with_no_name_in_the_home_folder() {
        let tmux = Tmux::from_env();
        let home = Path::new("/home/me");
        let new = far_command(&tmux, None, Some(home));
        let args = words_of(&new);
        assert!(args.contains(&"new-session".to_string()), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "-s" || a == "-t"),
            "no name, no target: {args:?}"
        );
        assert_eq!(new.get_current_dir(), Some(home));
        let attach = far_command(&tmux, Some("%3"), Some(home));
        let args = words_of(&attach);
        assert!(args.contains(&"attach-session".to_string()), "{args:?}");
        assert!(!args.contains(&"new-session".to_string()), "{args:?}");
        assert_eq!(attach.get_current_dir(), None);
    }

    /// Both ways of attaching end with the way back: `; display-message -d 4000 '<words>'`.
    #[test]
    fn both_commands_carry_the_way_back_to_the_list() {
        let tmux = Tmux::from_env();
        for target in [None, Some("%3")] {
            let args = words_of(&far_command(&tmux, target, None));
            let tail: Vec<&str> = args
                .iter()
                .rev()
                .take(5)
                .rev()
                .map(String::as_str)
                .collect();
            assert_eq!(
                tail,
                [";", "display-message", "-d", "4000", words::BACK_HINT],
                "{args:?}"
            );
        }
        assert!(words::BACK_HINT.ends_with(": back to the taw list"));
    }
}
