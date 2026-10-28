//! The e2e shell harness (`tests/e2e/harness.sh`) brought up from a test: its two servers, and the
//! shell that did it, which waits on its standard input to take them down again with `e2e_down`.
//!
//! What a harness run leaves behind (its servers, their socket files) is `e2e_down`'s to take away,
//! so a test that wants to see it does is better off with the shell that did both: [`Harness::down`].

// Each test file that includes `common` uses a subset of it; unused-item warnings would otherwise
// fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::collections::HashMap;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use taw::tmux::{Socket, Tmux};

use super::shell_env::assert_private;
use super::{end_what_a_server_left, repo_root, taw_bin, wait_for};

/// A tmux base of a test's own, made in /tmp (a socket's path has to be short) and removed when it is
/// dropped: the `$TMUX_TMPDIR` of the scripts the test runs (`tmux` in it), and the place their runs
/// make their tmux directories in (`$E2E_TMUX_BASE`). Whatever a script leaves running is in it, then,
/// and on no socket of anybody else's, the default directory least of all, whether or not a net is
/// around the test; and when it is dropped, every server that still listens on a socket in it is
/// ended, by that exact socket, and then its process group ([`end_the_server`]). A script killed before
/// its `e2e_down`, where its EXIT trap cannot run, leaves nothing behind.
pub struct TmuxBase(tempfile::TempDir);

impl TmuxBase {
    pub fn new() -> TmuxBase {
        let base = tempfile::Builder::new()
            .prefix("taw-hcb-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(base.path().join("tmux")).unwrap();
        TmuxBase(base)
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// The `$TMUX_TMPDIR` of a script run in it.
    pub fn tmux_dir(&self) -> PathBuf {
        self.path().join("tmux")
    }

    /// What a script run in it has in its environment.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("TMUX_TMPDIR", self.tmux_dir().display().to_string()),
            ("E2E_TMUX_BASE", self.path().display().to_string()),
        ]
    }

    /// Every socket file in it, at any depth (a link is not followed out of it).
    pub fn sockets(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for path in entries.flatten().map(|entry| entry.path()) {
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    walk(&path, found);
                } else if meta.file_type().is_socket() {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(self.path(), &mut found);
        found
    }
}

impl Drop for TmuxBase {
    fn drop(&mut self) {
        for socket in self.sockets() {
            end_the_server(&Tmux {
                socket: Some(Socket::Path(socket)),
                ..Tmux::with_socket_name("taw-unused")
            });
        }
    }
}

/// Ends the server that `tmux` reaches, if one answers there, and then its process group: the jobs it
/// ran (the plugin's hooks and one-shots, the watchers) outlive it, and one that ran on after the
/// directories of the test went would find them gone (a tmux of it would take /tmp for its
/// `$TMUX_TMPDIR`). The pid is read before the server is told to end.
fn end_the_server(tmux: &Tmux) {
    if tmux.run(&["list-sessions"]).is_err() {
        return;
    }
    let pid = tmux
        .run(&["display-message", "-p", "#{pid}"])
        .ok()
        .and_then(|pid| pid.trim().parse().ok());
    let _ = tmux.run_ok(&["kill-server"]);
    wait_for(
        || tmux.run(&["list-sessions"]).is_err(),
        Duration::from_secs(10),
    );
    end_what_a_server_left(pid, None);
}

/// What a script run by [`run_bash`] printed, and how it ended.
pub struct BashRun {
    pub stdout: String,
    pub stderr: String,
    pub status: ExitStatus,
    /// The tmux base the script had for its `$TMUX_TMPDIR`, unless it was given one of its own.
    decoy: TmuxBase,
}

impl BashRun {
    /// The `$TMUX_TMPDIR` that [`run_bash`] gave the script: a directory of its own, not the one of
    /// whoever runs the tests, nor the one a script under test makes for itself.
    pub fn decoy(&self) -> PathBuf {
        self.decoy.tmux_dir()
    }

    /// The `key=value` lines of what it printed.
    pub fn values(&self) -> HashMap<String, String> {
        self.stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }
}

/// Runs `script` in a bash, in `dir`, with `env` added and nothing of `$TMUX`, `$TMUX_PANE` and
/// `$TMUX_TMPDIR` that the runner has, and waits for it for a minute, which is long enough for any
/// script of these tests: a script that is not over by then is ended, and this fails.
///
/// Its `$TMUX_TMPDIR` is a decoy, a [`TmuxBase`] of its own, and not none: with none, a tmux the script
/// ran with no socket named would reach `/tmp/tmux-<uid>/default`, the server of whoever runs the
/// tests (and a script that is to make a directory of its own is told from one that did not by the
/// decoy: [`BashRun::decoy`]). `env` can still name another.
///
/// What the script prints goes to files and not to pipes. A process it leaves behind (which is what
/// some of these tests are about) holds a pipe open, and a test that waited for the end of a pipe would
/// hang for as long as that process lives instead of failing.
pub fn run_bash(dir: &Path, script: &str, env: &[(&str, &str)]) -> BashRun {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let (printed, complained) = (
        dir.join(format!("bash-{n}.out")),
        dir.join(format!("bash-{n}.err")),
    );
    let decoy = TmuxBase::new();
    let mut shell = Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TAW_REPORT_FILE")
        .env("TMUX_TMPDIR", decoy.tmux_dir())
        .envs(env.iter().copied())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(std::fs::File::create(&complained).unwrap())
        .spawn()
        .unwrap();
    let ended = wait_for(
        || matches!(shell.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    if !ended {
        stop(&mut shell);
    }
    let status = shell.wait().unwrap();
    let run = BashRun {
        stdout: std::fs::read_to_string(&printed).unwrap_or_default(),
        stderr: std::fs::read_to_string(&complained).unwrap_or_default(),
        status,
        decoy,
    };
    assert!(
        ended,
        "the script did not end in a minute: {}{}",
        run.stdout, run.stderr
    );
    run
}

/// The two servers `e2e_up` starts, which `smoke.sh` and `acceptance.sh` run on.
pub struct Harness {
    pub inner: Tmux,
    pub outer: Tmux,
    /// The run's directory (`$DIR`): its state, its config and the home of its shells.
    pub dir: PathBuf,
    /// The shell that brought the servers up, waiting to take them down.
    shell: Option<Child>,
    /// The tmux base the run is in, which ends whatever is still under it once the rest of the
    /// harness has gone (a field drops after the `Drop` of what holds it).
    base: TmuxBase,
}

impl Harness {
    /// Lets the shell go on to `e2e_down` (it waits for its standard input to end) and waits for it to
    /// be done: the servers are down by then, and so is whatever `e2e_down` takes with them.
    pub fn down(&mut self) {
        let Some(mut shell) = self.shell.take() else {
            return;
        };
        drop(shell.stdin.take());
        let ended = wait_for(
            || matches!(shell.try_wait(), Ok(Some(_))),
            Duration::from_secs(60),
        );
        if !ended {
            stop(&mut shell);
        }
    }

    /// Kills the shell that brought the servers up, with SIGKILL, which no trap answers: a run that
    /// died before its `e2e_down`, whose servers are left up for the drop to end.
    pub fn kill_the_shell(&mut self) {
        if let Some(mut shell) = self.shell.take() {
            let _ = shell.kill();
            let _ = shell.wait();
        }
    }
}

/// Stops a shell of these tests: TERM first, which one that brought the harness up answers with
/// `e2e_down` from its EXIT trap, and a few seconds to do so, then KILL, which no trap answers.
fn stop(shell: &mut Child) {
    // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the shell's,
    // a child of this process that has not been waited for.
    unsafe { libc::kill(shell.id() as libc::pid_t, libc::SIGTERM) };
    let ended = wait_for(
        || matches!(shell.try_wait(), Ok(Some(_))),
        Duration::from_secs(10),
    );
    if !ended {
        let _ = shell.kill();
    }
    let _ = shell.wait();
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.down();
        // What `e2e_down` should have done, for a harness that did not: the servers, by the paths of
        // their own sockets (checked to be a test's when the harness was made), and their groups.
        end_the_server(&self.outer);
        end_the_server(&self.inner);
    }
}

/// A `Tmux` that reaches the server listening on the socket file `path`, by that path (`-S`).
fn by_path(path: &str) -> Tmux {
    Tmux {
        socket: Some(Socket::Path(PathBuf::from(path))),
        ..Tmux::with_socket_name("taw-unused")
    }
}

/// `e2e_up`, run by a bash that stays until it is told to take the servers down. What it prints goes
/// to a file: the servers it starts are not children it can wait for, and a pipe they inherited would
/// keep this waiting for as long as they live. Whether a client attached in time is no matter here,
/// which is why the sockets are printed whatever `e2e_up` returned: the servers are up either way.
/// `env` is put in the environment of the shell, and so of the servers.
///
/// The servers are reached by the exact paths of their socket files, which `e2e_up` notes: they are
/// in the tmux directory the run makes for itself (`scripts/private-tmux-dir.sh`), which is not the
/// `$TMUX_TMPDIR` of this process, and `e2e_down` looks in it and removes it. The shell runs
/// `e2e_down` from its EXIT trap as well, so that one stopped by a signal, or by an error before its
/// last line, takes the servers down all the same; a shell this gives up on is stopped with TERM
/// before KILL ([`stop`]); and the run is in a [`TmuxBase`] of its own, whose drop ends whatever is
/// still under it, so that not even a shell that had to be killed leaves a server running.
pub fn harness_up(tmp: &Path, env: &[(&str, &str)]) -> Harness {
    let base = TmuxBase::new();
    let printed = tmp.join("e2e_up.out");
    let file = std::fs::File::create(&printed).unwrap();
    let mut shell = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "source '{}/tests/e2e/harness.sh'; trap 'e2e_down > /dev/null 2>&1' EXIT; \
             e2e_up guard > /dev/null; \
             printf 'sockets inner=%s outer=%s dir=%s\\n' \"$E2E_SOCK_FILE\" \"$E2E_OUTER_FILE\" \"$DIR\"; \
             read -r _; e2e_down > /dev/null",
            repo_root().display()
        ))
        .env("E2E_TMP", tmp)
        .env("TAW_BIN", taw_bin())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE").env_remove("TAW_REPORT_FILE")
        .envs(base.env())
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let printed_sockets = || {
        std::fs::read_to_string(&printed)
            .unwrap_or_default()
            .lines()
            .any(|line| line.starts_with("sockets "))
    };
    wait_for(
        || printed_sockets() || matches!(shell.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    let text = std::fs::read_to_string(&printed).unwrap_or_default();
    let Some(sockets) = text.lines().find_map(|line| line.strip_prefix("sockets ")) else {
        stop(&mut shell);
        panic!("e2e_up printed no sockets: {text}");
    };
    let word = |label: &str| {
        sockets
            .split_whitespace()
            .find_map(|word| word.strip_prefix(label))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let (Some(inner), Some(outer), Some(dir)) = (word("inner="), word("outer="), word("dir="))
    else {
        stop(&mut shell);
        panic!("e2e_up printed no sockets and directory worth trusting: {text}");
    };
    let (inner, outer) = (by_path(&inner), by_path(&outer));
    // Before anything is run against them (a kill-server on drop included): a name that is not a
    // test server's panics here, and no `Harness` is made to be dropped.
    for tmux in [&inner, &outer] {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_private(tmux))).is_err()
        {
            stop(&mut shell);
            panic!("e2e_up named a server that is not a test's: {text}");
        }
    }
    Harness {
        inner,
        outer,
        dir: PathBuf::from(dir),
        shell: Some(shell),
        base,
    }
}
