//! `taw app`: the list as the whole terminal, for a person whose terminal does not start tmux.
//!
//! Inside tmux there is nothing to supervise and the popup is the list ([`run`]). Outside it, `taw app` is
//! the terminal's program and starts nothing of its own on the screen: it starts the app child
//! (`taw app --child`) and waits for it.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;

use super::{Ctx, Output, ui_open};
use crate::machine::files;
use crate::tui::GoTarget;

/// What `taw app` says inside tmux, before it opens the popup.
pub const IN_TMUX: &str = "taw app: this terminal is already in tmux; the popup is the list here";

/// What `taw app --child` says when it is not run on a terminal.
pub const NO_TERMINAL: &str = "taw app: the app needs a terminal";

/// What the supervisor says when it cannot tell whether its child still runs, before it ends 1.
pub const LOST: &str = "taw app: lost track of the app child";

/// What `taw app --back` says of a value that is no session id.
pub const BACK_USAGE: &str = "--back takes the id of a tmux session, as in $3";
/// What a `--back-last` that is not a session id is told.
pub const BACK_LAST_USAGE: &str = "--back-last takes the id of a tmux session, as in $2";

/// `taw app`. Inside tmux (`$TMUX` is set) it says so on standard error and opens the popup on the client
/// it is run from, as `taw ui open` does, ending 0 when the popup opened and with its error when not.
/// Outside tmux it is the supervisor, or with `--child` the screen the supervisor started.
pub(super) fn run(
    ctx: &Ctx,
    go: Option<&str>,
    child: bool,
    back: Option<&str>,
    back_last: Option<&str>,
) -> anyhow::Result<Output> {
    // `--go` takes `machine:id`, a pane id of this Mac (`%7`) or `last`; anything else is refused before
    // anything starts, as a usage error (2), so that no terminal is taken for a target that cannot be had.
    let target = match go.map(GoTarget::parse).transpose() {
        Ok(target) => target,
        Err(words) => {
            eprintln!("taw app: {words}");
            std::process::exit(2);
        }
    };
    if back.is_some_and(|session| !crate::machine::valid_session_id(session)) {
        eprintln!("taw app: {BACK_USAGE}");
        std::process::exit(2);
    }
    if back_last.is_some_and(|session| !crate::machine::valid_session_id(session)) {
        eprintln!("taw app: {BACK_LAST_USAGE}");
        std::process::exit(2);
    }
    if child {
        // The screen is drawn on this terminal and read from it: with none, say so now, before any of the
        // start (crossterm would open /dev/tty and get far before something failed).
        // SAFETY: `isatty` takes a descriptor number and touches no memory.
        if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
            eprintln!("{NO_TERMINAL}");
            std::process::exit(1);
        }
        // A test seam, compiled only with debug assertions (a release build never reads it): with the
        // variable set the child ends 1 at once with one line of its own, as a crashing start would. It exists
        // for the test of the supervisor giving up after three quick failures, which needs the child alone to
        // fail while the supervisor runs on a terminal.
        #[cfg(debug_assertions)]
        if std::env::var_os("TAW_TEST_APP_CHILD_FAILS").is_some() {
            eprintln!("taw app: the test made this start fail");
            std::process::exit(1);
        }
        // The screen the supervisor started. Going back and leaving both end it 0: what the supervisor
        // does next is decided by whether the child is still there, never by how it ended.
        crate::tui::run_app(
            &ctx.tmux,
            &ctx.cfg,
            &ctx.paths,
            &ctx.taw_bin,
            None,
            target,
            back,
        )
        .context("running the app")?;
        return Ok(Output::Nothing);
    }
    if in_tmux() {
        return popup(ctx);
    }
    // The terminal's program starts the screen on this terminal: with none (a script, `ssh` with no
    // `-t`) it says so at once, as the screen itself would, not after three failed starts.
    put_terminal_on_stdout();
    // SAFETY: `isatty` takes a descriptor number and touches no memory.
    if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
        eprintln!("{NO_TERMINAL}");
        std::process::exit(1);
    }
    // The terminal's program: it ends the process with its own status, which is the terminal's.
    std::process::exit(supervise(ctx, go, back, back_last));
}

/// Puts the terminal this process was started from on its standard output when its standard input is one and its
/// standard output is not. `detach-client -E` runs its command in the tmux client's own process, which keeps the
/// client's standard output: for a client the app started that is the pipe the app copies the command's output
/// from (or /dev/null on the far side), and the app draws on a terminal, so it takes the controlling terminal
/// (`/dev/tty`) as its output, or, with none, the terminal standard input is, opened again to be written (a
/// duplicate of standard input would be the same open file, which may be read only). It runs before the
/// terminal check and before the supervisor starts its child, which then has the terminal too. A process with no
/// terminal on standard input, or one that cannot be written, is left as it is and is refused.
fn put_terminal_on_stdout() {
    // SAFETY: `isatty`, `ttyname`, `open`, `dup2` and `close` take descriptor numbers and NUL-ended paths, and touch
    // no memory of this process; `ttyname` returns a pointer into static storage, read at once.
    unsafe {
        if libc::isatty(0) != 1 || libc::isatty(1) == 1 {
            return;
        }
        let mut tty = libc::open(c"/dev/tty".as_ptr(), libc::O_WRONLY | libc::O_NOCTTY);
        if tty < 0 {
            let name = libc::ttyname(0);
            if !name.is_null() {
                tty = libc::open(name, libc::O_WRONLY | libc::O_NOCTTY);
            }
        }
        if tty < 0 {
            return;
        }
        // With standard output closed the open took its number, and there is nothing to move.
        if tty != 1 {
            libc::dup2(tty, 1);
            libc::close(tty);
        }
    }
}

/// Inside tmux: the line that says so, then the popup on the client. A refusal comes with the words
/// [`ui_open::open`] gives it (they already say the popup was being opened).
fn popup(ctx: &Ctx) -> anyhow::Result<Output> {
    eprintln!("{IN_TMUX}");
    ui_open::open(ctx, None, false)?;
    Ok(Output::Nothing)
}

/// The signal a supervisor was asked to pass on, set by its handler: 0 while there is none.
static PENDING: AtomicI32 = AtomicI32::new(0);
/// Whether the pending signal has been passed to the child.
static PASSED: AtomicBool = AtomicBool::new(false);

extern "C" fn pass_on(signal: libc::c_int) {
    PENDING.store(signal, Ordering::SeqCst);
}

/// How long a child has to have lived for its end not to count as a failure to start.
const QUICK: Duration = Duration::from_secs(1);
/// How many failures to start, one after another, the supervisor puts up with.
const GIVE_UP_AFTER: u32 = 3;
/// How big `app.log` may be when a child starts.
const LOG_LIMIT: u64 = 256 * 1024;
/// How often the supervisor looks at its child and at the client it waits for.
const LOOK: Duration = Duration::from_millis(100);

/// The terminal's program outside tmux: starts the app child on this terminal and waits, and starts it
/// again when it ends any way but cleanly. Never draws and never reads the keyboard.
///
/// It ignores `SIGINT`, `SIGQUIT` and `SIGTSTP`, so a stray key never ends the terminal's program, and
/// passes `SIGHUP` and `SIGTERM` on to the child and ends after it, with 128 and the signal. The child
/// exiting 0 ends it 0. After any other end it waits for the client the child's record names
/// (`client_pid`) to end, which is where the terminal is not the app's, and starts a new child without
/// `--go`. Three failures to start in a row end it 1, saying where the log is.
pub(super) fn supervise(
    ctx: &Ctx,
    go: Option<&str>,
    back: Option<&str>,
    back_last: Option<&str>,
) -> i32 {
    reset_sigchld();
    // SAFETY: `signal` with a handler that only stores to an atomic, or with `SIG_IGN`.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            libc::signal(signal, libc::SIG_IGN);
        }
        for signal in [libc::SIGHUP, libc::SIGTERM] {
            libc::signal(
                signal,
                pass_on as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
    let log = ctx.paths.logs_dir.join("app.log");
    let mut go = go.map(str::to_string);
    let mut failures = Failures::default();
    loop {
        let started = Instant::now();
        let child = match start_child(ctx, go.take().as_deref(), back, &log) {
            Ok(child) => child,
            Err(err) => {
                eprintln!("taw app: could not start the app: {err:#}");
                return 1;
            }
        };
        let pid = child.id();
        let status = loop {
            match wait_a_little(pid) {
                Waited::Ended(status) => break status,
                Waited::Running => {}
                // Starting a second child while the first may still run would put two on one terminal.
                Waited::Lost => {
                    eprintln!("{LOST}");
                    return 1;
                }
            }
            pass_pending_on(pid);
        };
        // A signal passed on ends the child, however it ends (it puts the terminal away and leaves 0), and
        // this process after it; the record is the child's no more.
        if let Some(signal) = pending() {
            files::remove_app_record(&ctx.paths, pid);
            return 128 + signal;
        }
        if status.success() {
            return back_to_tmux(ctx, back, back_last, 0);
        }
        if failures.ended_after(started.elapsed()) {
            eprintln!(
                "taw app: the app keeps failing; its log is {}",
                log.display()
            );
            return back_to_tmux(ctx, back, back_last, 1);
        }
        // Whatever the child's client is, the terminal is its until it ends.
        let client = files::read_app_record(&ctx.paths, pid).map_or(0, |record| record.client_pid);
        while files::app_alive(client) {
            if let Some(signal) = pending() {
                files::remove_app_record(&ctx.paths, pid);
                return 128 + signal;
            }
            std::thread::sleep(LOOK);
        }
        files::remove_app_record(&ctx.paths, pid);
    }
}

/// The terminal is put back in tmux when an app that the doorway started (`back` is the session the popup was opened
/// from) ends: this process is replaced by `tmux attach-session -t <that session>` on the server the app is pinned
/// to, so the window whose program the tmux client was does not close with the app. A session that is gone is
/// replaced by tmux's own pick (`attach-session` with no target). Nothing is done, and `code` is what the
/// supervisor ends with as it always did, when there is no `back`, when the terminal is gone (a hung-up tty has
/// nothing to attach), when the server is gone, or when the exec fails. Never called for a signal passed on: a
/// terminal that is closing is not given a client. `last` is the session the doorway's client was in before
/// `back` (its `#{client_last_session}`): the new client is given it as its last session, so that prefix + L goes
/// back to it.
fn back_to_tmux(ctx: &Ctx, back: Option<&str>, last: Option<&str>, code: i32) -> i32 {
    let Some(session) = back else {
        return code;
    };
    // `isatty` alone says yes on macOS for a terminal whose master closed: a client started there would attach
    // for a moment before it read the hang-up. The screen's watchdog asks the same question.
    if !crate::tui::terminal_is_there(libc::STDIN_FILENO) {
        return code;
    }
    let Some(mut command) = attach_back(&ctx.tmux, session, last) else {
        return code;
    };
    // What this process ignores would be ignored by the client it becomes.
    // SAFETY: `signal` with `SIG_DFL` touches no memory of ours.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    let err = command.exec();
    eprintln!("taw app: could not go back to tmux: {err}");
    code
}

/// The command that attaches this terminal to `session` on the server `tmux` talks to: its own session, else (the
/// session is gone but the server answers) tmux's most recent one, else `None` (no server). With `last`, the
/// session the doorway's client came to `session` from, that is still there, the client is attached to it and
/// switched to `session`, so that prefix + L goes back to it ([`super::open::attach_from`]).
fn attach_back(tmux: &crate::tmux::Tmux, session: &str, last: Option<&str>) -> Option<Command> {
    if tmux.run(&["has-session", "-t", session]).is_ok() {
        let last = super::open::usable_came_from(tmux, last, session);
        return Some(super::open::attach_from(tmux, last, session));
    }
    tmux.run(&["list-sessions"]).ok()?;
    let mut command = tmux.command();
    command.arg("attach-session");
    Some(command)
}

/// Sets `SIGCHLD` to its default. A parent that ignores it leaves it ignored across `exec`, and where an ignored
/// `SIGCHLD` has the children reaped for the parent (Linux, not macOS) `waitpid` of the child would fail with
/// ECHILD: every end, `q` included, would be taken for a lost child.
pub(crate) fn reset_sigchld() {
    // SAFETY: `signal` with `SIG_DFL` touches no memory of ours.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }
}

/// How many children in a row have failed to start.
#[derive(Default)]
struct Failures(u32);

impl Failures {
    /// A child that lived `lived` has ended badly. Says whether that was the last failure the supervisor
    /// puts up with: a child that lived [`QUICK`] or more was not one, and starts the count again.
    fn ended_after(&mut self, lived: Duration) -> bool {
        if lived < QUICK {
            self.0 += 1;
            self.0 >= GIVE_UP_AFTER
        } else {
            self.0 = 0;
            false
        }
    }
}

/// The signal waiting to be passed on, if one is.
fn pending() -> Option<i32> {
    Some(PENDING.load(Ordering::SeqCst)).filter(|signal| *signal != 0)
}

/// Passes a pending `SIGHUP` or `SIGTERM` to the child, by its pid, once.
fn pass_pending_on(pid: u32) {
    let signal = PENDING.load(Ordering::SeqCst);
    if signal != 0 && !PASSED.swap(true, Ordering::SeqCst) {
        // SAFETY: `kill` of a pid this process started and has not yet reaped.
        unsafe { libc::kill(pid as libc::pid_t, signal) };
    }
}

/// What a look at the child found.
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    /// The child has ended, and is reaped.
    Ended(ExitStatus),
    /// The child is still there (it may just have been set going again).
    Running,
    /// The child cannot be asked about: whether it runs is not known.
    Lost,
}

/// A look at the child `pid`: its end if it has ended, else after a short look, [`Waited::Running`]. A
/// child that is stopped (a key typed at the terminal before the child's screen is raw can stop it) is set
/// going again at once: nothing else would, and the terminal would hang.
fn wait_a_little(pid: u32) -> Waited {
    let pid = pid as libc::pid_t;
    let mut status: libc::c_int = 0;
    // SAFETY: `waitpid` of a pid this process started and has not reaped, with a status of its own to
    // write to. A child that has ended is reaped here, and `Child` is never asked to wait for it again.
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
    if waited == pid {
        if libc::WIFSTOPPED(status) {
            // SAFETY: `kill` of the child that has just been seen stopped, so not yet reaped.
            unsafe { libc::kill(pid, libc::SIGCONT) };
            return Waited::Running;
        }
        return Waited::Ended(ExitStatus::from_raw(status));
    }
    if waited == 0 {
        std::thread::sleep(LOOK);
        return Waited::Running;
    }
    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
        return Waited::Running;
    }
    Waited::Lost
}

/// `taw app --child [--go <target>]` on this terminal, its standard error appended to the log.
fn start_child(
    ctx: &Ctx,
    go: Option<&str>,
    back: Option<&str>,
    log: &Path,
) -> anyhow::Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).context("making the logs folder")?;
    }
    // Over the limit it starts again from empty; the child's words are appended below.
    if std::fs::metadata(log).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        std::fs::File::create(log).context("emptying app.log")?;
    }
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let mut command = Command::new(&ctx.taw_bin);
    command.args(["app", "--child"]);
    if let Some(go) = go {
        command.args(["--go", go]);
    }
    // The child only needs to know, for what its keys say; the supervisor is the one that goes back.
    if let Some(back) = back {
        command.args(["--back", back]);
    }
    command.stderr(stderr);
    // The signals this process ignores are the child's to handle as any program does.
    // SAFETY: the closure only calls `signal`, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    command.spawn().context("starting the app")
}

/// Whether this terminal is inside tmux: `$TMUX` is set to something.
fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// A failure of `taw app`, told as one line on standard error (it prints no envelope).
pub(super) fn report(err: &anyhow::Error) -> i32 {
    eprintln!("taw app: {err:#}");
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::{Stub, stub_ctx, stub_tmux};

    /// Three children in a row that end before [`QUICK`] are given up on, no fewer; one that lived longer
    /// between them starts the count again.
    #[test]
    fn the_third_quick_failure_in_a_row_is_the_last() {
        let quick = Duration::from_millis(50);
        let long = Duration::from_secs(5);
        let mut failures = Failures::default();
        assert!(!failures.ended_after(quick), "one is not enough");
        assert!(!failures.ended_after(quick), "two is not enough");
        assert!(failures.ended_after(quick), "the third is the last");

        let mut failures = Failures::default();
        for round in 0..4 {
            assert!(!failures.ended_after(quick), "round {round}, first");
            assert!(!failures.ended_after(quick), "round {round}, second");
            assert!(!failures.ended_after(long), "round {round}, a long life");
        }
        assert!(!failures.ended_after(quick));
        assert!(!failures.ended_after(quick));
        assert!(failures.ended_after(quick), "and then three in a row");
    }

    /// A look at a process that is not this one's child cannot say whether it runs: that is
    /// [`Waited::Lost`], never an end; and a child that ended is reaped with its real status.
    #[test]
    fn a_look_at_a_process_that_is_not_ours_loses_track_and_an_ended_child_is_reaped() {
        let not_ours = std::os::unix::process::parent_id();
        assert_eq!(wait_a_little(not_ours), Waited::Lost);

        // `wait_a_little` is what reaps it, by its pid: the test is that it does.
        #[allow(clippy::zombie_processes)]
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut seen = Waited::Running;
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen == Waited::Running && Instant::now() < deadline {
            seen = wait_a_little(pid);
        }
        assert_eq!(seen, Waited::Ended(ExitStatus::from_raw(3 << 8)));
    }

    /// A `SIGCHLD` that was ignored is the default after the reset. Run in a forked child, so that the test
    /// process's other threads never see their `SIGCHLD` ignored. (An integration test cannot tell on macOS,
    /// where an ignored `SIGCHLD` still leaves the child to `waitpid`.)
    #[test]
    fn the_reset_makes_an_ignored_sigchld_the_default_again() {
        // SAFETY: the forked child calls only `signal`, `sigaction` and `_exit` (async-signal-safe) and ends;
        // the parent waits for it by its pid.
        let code = unsafe {
            let pid = libc::fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                let disposition = || {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action);
                    action.sa_sigaction
                };
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                if disposition() != libc::SIG_IGN {
                    libc::_exit(10);
                }
                reset_sigchld();
                libc::_exit(if disposition() == libc::SIG_DFL {
                    0
                } else {
                    11
                });
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            libc::WEXITSTATUS(status)
        };
        assert_eq!(
            code, 0,
            "10: not ignored to begin with, 11: not the default after the reset"
        );
    }

    /// Where the terminal goes back to when an app the doorway started ends: the session it names; with that
    /// session gone and the server answering, tmux's own pick (no target); with no server, nowhere.
    #[test]
    fn the_way_back_is_the_session_else_any_session_else_nowhere() {
        let args = |command: &Command| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let command = attach_back(&tmux, "$3", None).expect("the session is there");
        assert_eq!(args(&command), ["attach-session", "-t", "$3"]);

        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(
            dir.path(),
            &Stub {
                session_gone: true,
                ..Stub::default()
            },
        );
        let command = attach_back(&tmux, "$3", None).expect("the server is there");
        assert_eq!(args(&command), ["attach-session"]);

        let gone = crate::tmux::Tmux {
            bin: std::path::PathBuf::from("/nonexistent/taw-test-tmux"),
            socket: None,
        };
        assert!(
            attach_back(&gone, "$3", None).is_none(),
            "no server, nowhere"
        );
    }

    /// Going back to tmux, the new client has the session the doorway's client was in before as its last session
    /// (attached to it, then switched to the session in one command line), so prefix + L goes back to it. A last
    /// session that is gone, or is the session itself, is no one's: the plain attach.
    #[test]
    fn the_way_back_goes_through_the_last_session_when_it_is_there() {
        let args = |command: &Command| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) =
            crate::cli::test_support::sessions_tmux(dir.path(), &[("$3", "$3"), ("$2", "$2")]);
        let command = attach_back(&tmux, "$3", Some("$2")).expect("the session is there");
        assert_eq!(
            args(&command),
            [
                "attach-session",
                "-t",
                "$2",
                ";",
                "switch-client",
                "-t",
                "$3"
            ]
        );
        let command = attach_back(&tmux, "$3", Some("$9")).expect("the session is there");
        assert_eq!(args(&command), ["attach-session", "-t", "$3"], "gone");
        let command = attach_back(&tmux, "$3", Some("$3")).expect("the session is there");
        assert_eq!(args(&command), ["attach-session", "-t", "$3"], "itself");
    }

    /// With no session to go back to the supervisor ends with the code it was given and execs nothing.
    #[test]
    fn with_no_way_back_the_end_keeps_its_code() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let ctx = stub_ctx(tmux);
        assert_eq!(back_to_tmux(&ctx, None, None, 7), 7);
        assert!(std::fs::read_to_string(&log).unwrap_or_default().is_empty());
    }

    /// A popup tmux refuses is told once as "opening the popup: <tmux's words>": the verb adds nothing of
    /// its own to what `ui_open` says.
    #[test]
    fn a_refused_popup_says_opening_the_popup_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(
            dir.path(),
            &Stub {
                popup_exit: 129,
                popup_says: "no current client",
                ..Stub::default()
            },
        );
        let err = popup(&stub_ctx(tmux)).err().expect("the popup was refused");
        let said = format!("taw app: {err:#}");
        assert_eq!(said.matches("opening the popup").count(), 1, "{said}");
        assert!(said.ends_with("no current client"), "{said}");
    }
}
