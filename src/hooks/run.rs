//! Running a `[hooks]` command: started in the background when an event fires, or to the end
//! when one is being tested.
//!
//! An event hook is started by a **supervisor**, one process for each command: `taw hooks
//! exec`, which runs the command under its time limit and writes what went wrong to the
//! hooks log. The process that fired the event is often one that ends a moment later (a CLI
//! verb, a tmux hook's `taw plugin event`), so nothing in it could enforce a limit, and
//! nothing in it could report a failure. The supervisor is that process's stand-in, and the
//! only thing it waits for is the reaper thread that keeps a long-lived caller (the watcher,
//! the popup) from collecting zombies.
//!
//! The supervisor is started through a shell ([`START`]) that ignores the hangup a popup closing
//! would send to everything it started, and so do the supervisor and what it runs: a hook fired
//! by the popup (a new worktree, say) must outlive the popup that asked for it. Nothing in that
//! script is the person's text: the binary, the log, the event and the command are all arguments
//! of it. The command is the person's own configuration, and what happened is in the
//! environment and on standard input, never in a command line.
//!
//! A supervisor that never gets to run the hook is noticed twice over. A `taw` that is gone or
//! is not an executable file (a watcher outlives the binary it started from, which an upgrade
//! moves) is refused before anything is started, and that is the whole of what the process that
//! fires the event logs: it may end a moment later, and a line it meant to write when the
//! supervisor ended would go with it. One that starts and then ends badly (the shell's `exec`
//! failing, a `taw` that does not take these arguments, one that is killed) is logged by the
//! shell that started it, which is a process of its own and is there when it ends, since `taw
//! hooks exec` ends well whether or not the hook did.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use super::context::Built;
use super::log as hook_log;
use crate::plugin::PluginCtx;
use crate::session::create::{self, HookEnv, Ran, Streams};

/// What starts the supervisor, and says how it ended when it ended badly. The arguments are `$0`,
/// the `taw` binary; `$1`, `$2` and `$3`, the hooks log, the event and the command, for that line;
/// and the rest, which are the binary's own.
///
/// Hangups are ignored, and then the binary is run as a child and not `exec`ed, so that this
/// shell is still there when it ends. `taw hooks exec` ends with status 0 whatever became of the
/// hook, which it logs itself, so any other end means it never ran the hook: the binary is not
/// one, does not take these arguments, or was killed. The line is the one [`hook_log::record`]
/// writes, character for character but for the time: one line, the command flattened (a control
/// character is a space, C1 controls included, which are two bytes in UTF-8), the ends trimmed of
/// what Unicode calls white space, the excerpt cut at 300 characters with `...`, the log's
/// directory made and the log moved aside when it is past 256 KiB (`find -size +512` is 512-byte
/// blocks, so exactly `hooks::log`'s limit). `one_line` does the text in `awk` under `LC_ALL=C`,
/// so that what it does is the same wherever the hook was fired from and never depends on a locale;
/// a character is a byte that is not a UTF-8 continuation byte, with the ones that follow it. The
/// event and the command are only ever data: arguments of `printf` and input of `awk`, so
/// nothing in the line is run.
const START: &str = r#"trap '' HUP
log=$1 event=$2 command=$3
shift 3
"$0" "$@"
status=$?
[ "$status" -eq 0 ] && exit 0
if [ "$status" -gt 128 ]; then
  how="was killed by signal $((status - 128))"
else
  how="ended with exit status $status"
fi
one_line() {
  printf '%s' "$1" | LC_ALL=C awk -v max="$2" '
    { s = s (NR > 1 ? "\n" : "") $0 }
    END {
      gsub(/[\001-\037\177]/, " ", s)
      gsub(/\302[\200-\237]/, " ", s)
      ws = "( |\302\240|\341\232\200|\342\200[\200-\212\250\251\257]|\342\201\237|\343\200\200)+"
      sub("^" ws, "", s)
      sub(ws "$", "", s)
      if (max > 0) {
        n = 0
        for (i = 1; i <= length(s); i++) {
          c = substr(s, i, 1)
          if (c < "\200" || c > "\277") {
            if (++n > max) { s = substr(s, 1, i - 1) "..."; break }
          }
        }
      }
      printf "%s", s
    }'
}
case $log in */*) mkdir -p "${log%/*}" ;; esac
[ -n "$(find "$log" -size +512 2>/dev/null)" ] && mv -f "$log" "$log.1"
printf '%s %s could not start: %s | %s\n' "$(date -u '+%Y-%m-%d %H:%M:%S UTC')" "$(one_line "$event" 0)" "$(one_line "$command" 0)" "$(one_line "$0 $how" 300)" >> "$log"
exit "$status"
"#;

/// How a hook command ended.
#[derive(Debug)]
pub enum Outcome {
    /// It ended: how, and what it wrote to stdout and to stderr, when that was kept: only the end
    /// of each, so that what is kept says whether what came before was cut.
    Ended {
        status: ExitStatus,
        stdout: String,
        stderr: String,
        stdout_cut: bool,
        stderr_cut: bool,
    },
    /// It was still running when its time limit ran out, and was stopped with what it had
    /// started.
    TimedOut(Duration),
    /// It could not be started at all.
    NotStarted(String),
}

impl Outcome {
    /// What went wrong, in the words the hooks log uses; `None` for a hook that exited zero.
    pub fn failure(&self) -> Option<String> {
        match self {
            Outcome::Ended { status, .. } if status.success() => None,
            Outcome::Ended { status, .. } => {
                Some(format!("failed ({})", create::status_words(*status)))
            }
            Outcome::TimedOut(limit) => Some(format!("timed out after {} s", limit.as_secs_f64())),
            Outcome::NotStarted(_) => Some("could not start".to_string()),
        }
    }

    /// What the hook printed to stdout, which nothing but `taw hooks test` shows.
    pub fn stdout(&self) -> &str {
        match self {
            Outcome::Ended { stdout, .. } => stdout,
            _ => "",
        }
    }

    /// What the hook wrote to stderr, or why it could not start.
    pub fn stderr(&self) -> &str {
        match self {
            Outcome::Ended { stderr, .. } | Outcome::NotStarted(stderr) => stderr,
            Outcome::TimedOut(_) => "",
        }
    }

    /// [`Outcome::stdout`] as `taw hooks test` shows it: with [`cut_note`] first when the hook
    /// printed more than is kept, so that the end of it is not read as the whole.
    pub fn stdout_shown(&self) -> String {
        match self {
            Outcome::Ended {
                stdout, stdout_cut, ..
            } => shown(stdout, *stdout_cut),
            _ => String::new(),
        }
    }

    /// [`Outcome::stderr`] as `taw hooks test` shows it, as [`Outcome::stdout_shown`] does.
    pub fn stderr_shown(&self) -> String {
        match self {
            Outcome::Ended {
                stderr, stderr_cut, ..
            } => shown(stderr, *stderr_cut),
            Outcome::NotStarted(stderr) => stderr.clone(),
            Outcome::TimedOut(_) => String::new(),
        }
    }
}

/// The line `taw hooks test` puts first when what it shows of a hook's output is only the end of
/// it: what is kept is [`create::HELD_BYTES`].
pub fn cut_note() -> String {
    format!(
        "(earlier output cut; the last {} KiB follow)",
        create::HELD_BYTES / 1024
    )
}

/// `text`, with [`cut_note`] on a line before it when `cut`.
fn shown(text: &str, cut: bool) -> String {
    match (cut, text.is_empty()) {
        (false, _) => text.to_string(),
        (true, true) => cut_note(),
        (true, false) => format!("{}\n{text}", cut_note()),
    }
}

/// Runs `command` to the end, or to `limit`: `env` on top of the environment it inherits,
/// `input` on its standard input, `dir` as its working directory. Used by the supervisor and
/// by `taw hooks test`. What is kept of its output is `streams`: no more than the end of each
/// stream, and only the streams a caller has a use for.
pub(crate) fn run(
    command: &str,
    home: &Path,
    env: &HookEnv,
    input: &[u8],
    dir: Option<&Path>,
    limit: Option<Duration>,
    streams: Streams,
) -> Outcome {
    match create::run_command(command, home, env, input, dir, limit, streams) {
        Ok(Ran::Ended(ended)) => Outcome::Ended {
            status: ended.status,
            stdout: String::from_utf8_lossy(&ended.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&ended.stderr).trim().to_string(),
            stdout_cut: ended.stdout_cut,
            stderr_cut: ended.stderr_cut,
        },
        Ok(Ran::OutOfTime(limit)) => Outcome::TimedOut(limit),
        Err(err) => Outcome::NotStarted(format!("{err}")),
    }
}

/// What `taw hooks exec` does: runs `command` with the event's JSON as its input, and writes
/// a line to `log` when it fails. The environment is the one this process was started with,
/// which is where the dispatcher put the `TAW_*` variables, and the working directory is this
/// process's own. Nothing reads a background hook's stdout, so it goes to `/dev/null`, and of
/// its stderr only the end is held, for the last line the log quotes: a hook that is chatty
/// (`curl -v`, `tail -f`) fills nothing.
pub fn supervise(
    log: &Path,
    event: &str,
    command: &str,
    home: &Path,
    input: &[u8],
    limit: Option<Duration>,
) -> Outcome {
    let outcome = run(
        command,
        home,
        &HookEnv::default(),
        input,
        None,
        limit,
        Streams::BACKGROUND,
    );
    if let Some(failure) = outcome.failure() {
        hook_log::record(log, event, &failure, command, outcome.stderr());
    }
    outcome
}

/// Starts every command hooked to `event`, each in the background, and returns how many
/// started. A command that cannot be started (its `taw` is gone, or `/bin/sh` will not run) is
/// logged and not counted, and the rest are started all the same: one hook's trouble is not
/// another's.
pub fn dispatch(ctx: &PluginCtx, event: &str, built: &Built) -> usize {
    let log = hook_log::path(ctx.paths);
    let home = dirs::home_dir().unwrap_or_default();
    let dir = hook_dir(&built.env, &home);
    let env = super::environment(ctx, &built.env);
    let mut started = 0;
    for command in ctx.cfg.hooks.commands(event) {
        let spawned = spawn_supervisor(
            ctx.taw_bin,
            &Supervisor {
                event,
                limit: ctx.cfg.hooks.timeout_seconds,
                log: &log,
                command,
            },
            &env,
            &built.json,
            dir.as_deref(),
            &home,
        );
        match spawned {
            Ok(()) => started += 1,
            Err(err) => {
                ::log::warn!("{event} hook {command}: {err:#}");
                hook_log::record(&log, event, "could not start", command, &format!("{err:#}"));
            }
        }
    }
    started
}

/// Where a hook starts: the directory the event is about (`TAW_DIR`) when it is one that
/// exists, else the person's home directory, so the same hook starts in the same place
/// whichever process fired the event. One that exists and cannot be entered is found out when
/// the hook is started, and falls back to the home directory the same way
/// ([`create::starting_places`]).
pub fn hook_dir(env: &[(String, String)], home: &Path) -> Option<PathBuf> {
    let event_dir = env
        .iter()
        .find(|(name, _)| name == "TAW_DIR")
        .map(|(_, value)| Path::new(value))
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir());
    event_dir
        .or_else(|| home.is_dir().then_some(home))
        .map(Path::to_path_buf)
}

/// What a supervisor is told, beside the environment and the JSON: the arguments of `taw
/// hooks exec`.
struct Supervisor<'a> {
    event: &'a str,
    limit: f64,
    log: &'a Path,
    command: &'a str,
}

/// Whether `taw_bin` is something the shell can `exec`: a file with an execute bit. The shell
/// that would run it says nothing to anyone if it is not (its output goes nowhere), so a
/// binary that has gone is found out here.
fn check_runnable(taw_bin: &Path) -> anyhow::Result<()> {
    let meta = std::fs::metadata(taw_bin)
        .map_err(|err| anyhow::anyhow!("{}: {err}", taw_bin.display()))?;
    anyhow::ensure!(
        meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        "{} is not an executable file",
        taw_bin.display()
    );
    Ok(())
}

/// Starts the supervisor for one command, in `dir` when there is one and it can be entered, else
/// in the home directory, else where this process is ([`create::starting_places`]).
fn spawn_supervisor(
    taw_bin: &Path,
    supervisor: &Supervisor,
    env: &HookEnv,
    json: &str,
    dir: Option<&Path>,
    home: &Path,
) -> anyhow::Result<()> {
    check_runnable(taw_bin)?;
    // `--limit=` and `--log=` are one argument each, so a negative limit is a value and not
    // an option, and a path is never read as a flag.
    let mut log = OsString::from("--log=");
    log.push(supervisor.log);
    let command = |dir: Option<&Path>| {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", START])
            .arg(taw_bin)
            .arg(supervisor.log)
            .arg(supervisor.event)
            .arg(supervisor.command)
            .args(["hooks", "exec", "--event"])
            .arg(supervisor.event)
            .arg(format!("--limit={}", supervisor.limit))
            .arg(&log)
            .arg("--")
            .arg(supervisor.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        env.apply(&mut command);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        command
    };
    // A directory the hook may not enter, or that is gone, does not lose the hook: it starts in
    // the home directory, and failing that where this process is.
    let mut child = create::starting_places(dir, home)
        .into_iter()
        .find_map(|place| match command(Some(place)).spawn() {
            Err(err) if create::cannot_start_in_a_directory(&err) => None,
            other => Some(other),
        })
        .unwrap_or_else(|| command(None).spawn())?;
    if let Some(mut input) = child.stdin.take() {
        // Written here and not on a thread: a caller that ends a moment later would take a
        // thread down with it, halfway through the JSON. A supervisor reads it at once, so
        // this waits for nothing but its start when the JSON is larger than a pipe holds.
        let _ = input.write_all(json.as_bytes());
    }
    // Reaped on a thread of its own, so a watcher that lives for days collects none of them. A
    // supervisor that ends badly is logged by the shell that started it, not by this thread:
    // this process may end before it does.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dir: &str) -> Vec<(String, String)> {
        vec![("TAW_DIR".to_string(), dir.to_string())]
    }

    #[test]
    fn a_hook_starts_in_the_directory_the_event_is_about_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let there = tmp.path().to_str().unwrap();

        assert_eq!(
            hook_dir(&env(there), Path::new("/")),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn a_hook_starts_at_home_when_the_directory_is_gone_or_there_is_none() {
        let home = tempfile::tempdir().unwrap();

        for dir in ["/no/such/directory", ""] {
            assert_eq!(
                hook_dir(&env(dir), home.path()),
                Some(home.path().to_path_buf()),
                "{dir:?}"
            );
        }
        assert_eq!(hook_dir(&[], home.path()), Some(home.path().to_path_buf()));
        assert_eq!(hook_dir(&[], Path::new("/no/such/home")), None);
    }

    /// The supervisor is started where the hook is: in the directory the event is about, and when
    /// that is gone or cannot be entered, in the home directory, which is the same wherever the
    /// process that fires the event happens to be.
    #[test]
    fn a_supervisor_whose_directory_is_gone_is_started_in_the_home_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let wrote = root.join("pwd");
        let bin = root.join("taw");
        std::fs::write(&bin, format!("#!/bin/sh\npwd -P > '{}'\n", wrote.display())).unwrap();
        crate::test_exec::make_executable(&bin);
        let log = root.join("hooks.log");
        let supervisor = Supervisor {
            event: "tab.focused",
            limit: 30.0,
            log: &log,
            command: "true",
        };

        spawn_supervisor(
            &bin,
            &supervisor,
            &HookEnv::default(),
            "{}",
            Some(&root.join("gone")),
            &root,
        )
        .unwrap();

        let started_in = (0..200).find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::read_to_string(&wrote)
                .ok()
                .filter(|text| !text.trim().is_empty())
        });
        assert_eq!(
            started_in.as_deref().map(str::trim),
            Some(root.to_str().unwrap())
        );
    }

    /// What `taw hooks test` shows of a hook's output is only the end of it when the hook printed
    /// more than is kept, and the first line says so.
    #[test]
    fn output_that_was_cut_is_shown_with_the_note_first() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(cut_note(), "(earlier output cut; the last 16 KiB follow)");
        assert_eq!(shown("the end", false), "the end");
        assert_eq!(
            shown("the end", true),
            "(earlier output cut; the last 16 KiB follow)\nthe end"
        );
        // Whatever was kept was blank: the note stands alone, with no empty line after it.
        assert_eq!(shown("", true), cut_note());
        assert_eq!(shown("", false), "");

        let cut = Outcome::Ended {
            status: ExitStatus::from_raw(0),
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            stdout_cut: true,
            stderr_cut: false,
        };
        assert_eq!(
            cut.stdout_shown(),
            "(earlier output cut; the last 16 KiB follow)\nout"
        );
        assert_eq!(cut.stderr_shown(), "err");
        // The plain accessors are what the log and a refusal use, and carry no note.
        assert_eq!((cut.stdout(), cut.stderr()), ("out", "err"));
    }

    #[test]
    fn what_went_wrong_is_said_the_way_the_log_says_it() {
        use std::os::unix::process::ExitStatusExt;

        let ended = |code: i32| Outcome::Ended {
            status: ExitStatus::from_raw(code << 8),
            stdout: String::new(),
            stderr: "boom".to_string(),
            stdout_cut: false,
            stderr_cut: false,
        };
        assert_eq!(ended(0).failure(), None);
        assert_eq!(
            ended(3).failure().as_deref(),
            Some("failed (exit status 3)")
        );
        assert_eq!(ended(3).stderr(), "boom");
        assert_eq!(
            Outcome::Ended {
                status: ExitStatus::from_raw(9),
                stdout: String::new(),
                stderr: String::new(),
                stdout_cut: false,
                stderr_cut: false,
            }
            .failure()
            .as_deref(),
            Some("failed (killed by signal 9)")
        );
        assert_eq!(
            Outcome::TimedOut(Duration::from_secs(30))
                .failure()
                .as_deref(),
            Some("timed out after 30 s")
        );
        assert_eq!(
            Outcome::TimedOut(Duration::from_millis(2500))
                .failure()
                .as_deref(),
            Some("timed out after 2.5 s")
        );
        let missing = Outcome::NotStarted("no such file".to_string());
        assert_eq!(missing.failure().as_deref(), Some("could not start"));
        assert_eq!(missing.stderr(), "no such file");
    }
}
