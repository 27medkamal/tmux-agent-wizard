//! Shared harness for the end-to-end integration tests.
//!
//! Everything here exists so an `e2e_*` test can drive the **assembled product** — the
//! real `taw` binary and the real plugin script — against a throwaway tmux server,
//! without ever touching the developer's own tmux server, config or state:
//!
//! * [`Server`] owns one `tmux -L <unique> -f /dev/null` server plus the three tempdirs
//!   `taw` is pointed at, and kills the server in `Drop`.
//! * [`Server::taw`] runs `env!("CARGO_BIN_EXE_taw")` against it and parses the JSON
//!   envelope, so assertions are about the product's documented output rather than its
//!   library internals.
//! * [`wait_for`] is the only way these tests wait: every asynchronous thing here (a
//!   `pipe-pane` watcher starting, a hook firing, a state transition) is polled with a
//!   timeout, never slept through.
//! * [`FakeAgent`] launches `tests/fixtures/fake-agents/claude` in a pane *through the
//!   product* (`taw agent start`) and knows the pid to signal.
//!
//! # Why each isolation measure is here
//!
//! `-f /dev/null`: a fresh `-L` socket still loads the *host's* `~/.config/tmux/tmux.conf`
//! — `-L` only picks the socket. A personal config (base-index, TPM autoload, hooks) would
//! break both the index assumptions and the "throwaway" premise.
//!
//! `default-shell /bin/sh` **and** `/bin/sh` as the first session's own command: `-f
//! /dev/null` isolates tmux's config but not the *shell's*. The host login shell may print
//! banners, run prompt tools, or open an interactive prompt of its own — on this repo, a
//! `mise` "trust this config?" question in an untrusted worktree — which swallows the
//! keystrokes `taw agent start` sends and makes every test flaky for reasons that have
//! nothing to do with `taw`. `/bin/sh` reads no rc file. It has to be named twice because
//! the server does not outlive a `start-server` with no sessions, so there is no earlier
//! moment at which to set the option for the first pane.
//!
//! `TAW_*` env on the **server-starting** tmux invocation, not only on `taw`: the
//! `pipe-pane` watcher `watch attach` opens is a child of the tmux *server*, not of any
//! `taw` process, and so is every `run-shell` the plugin script's hooks fire. Without the
//! variables in the server's own environment those processes would resolve
//! `Paths::from_env()` to the developer's real `~/.local/state` and write there.
//!
//! `HISTFILE`, `HOME` and `ZDOTDIR` on the **server-starting** invocation as well, for the
//! same reason as `TAW_*` (the first pane is made from the environment the server starts
//! with) and no other: a shell that is typed into saves what it was typed when its server
//! is killed, to `~/.bash_history` of whoever runs the tests, and a shell reads its startup
//! files from the home it has. See [`shell_env`], and `tests/shell_isolation.rs`, which
//! fails for a server that is started without them.
//!
//! A pre-touched manifest marker: `taw manifest update --if-stale` (which the plugin
//! script fires at startup) would otherwise reach out to `herdr.dev`. A fresh marker file
//! makes `detect::update::is_stale` say "no", so no test in this suite makes a network
//! request.
//!
//! `$TAW_NOTIFY_CMD` and `$TAW_SOUND_CMD` on the server-starting invocation too, naming
//! refusing recorders ([`shell_env::DESKTOP_SEAMS`]): a watcher that toasts through `system`,
//! or plays a sound, runs the desktop's own `osascript` or `afplay` without them, and a test
//! once put "claude done" on the screen of whoever ran it. A test that records its toasts
//! puts its own recorder in [`ServerOptions::env`], which wins.
//!
//! `$TAW_SSH_BIN` too, naming a refusing recorder ([`shell_env::SSH_SEAMS`]): taw runs every ssh
//! through it, and a `taw --machine` that a test ran by mistake would log in to a real machine
//! without it. A test of machines puts a stand-in ssh in its place.

// Each `e2e_*.rs` file includes this module and uses a subset of it; unused-item warnings
// would otherwise fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub mod exec;
pub mod fake_runtime;
pub mod git_decoy;
pub mod harness;
pub mod machine;
pub mod owned;
pub mod proxy;
pub mod shell_env;
pub mod terminal;

/// Marks a stand-in executable (0o755) so that Linux runs it at once: see [`exec`].
#[allow(unused_imports)]
pub use exec::{make_executable, make_executable_with_mode};

/// The built binary under test. Every command in these tests goes through this, never
/// through `taw`'s library API — the point is to prove the *assembled* product works.
pub fn taw_bin() -> &'static str {
    env!("CARGO_BIN_EXE_taw")
}

/// The repo root, from cargo.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `tests/fixtures/fake-agents/claude`: a shell script that renders the captured Claude
/// Code screens and switches between them on `SIGUSR1`/`SIGUSR2`.
pub fn fake_agent_fixture() -> PathBuf {
    repo_root()
        .join("tests")
        .join("fixtures")
        .join("fake-agents")
        .join("claude")
}

/// The real TPM entry point, `tmux-agent-wizard.tmux`.
pub fn plugin_script() -> PathBuf {
    repo_root().join("tmux-agent-wizard.tmux")
}

/// Whether a pane's screen ends at a shell's prompt: `sh-3.2$`, `$`, or `#` for root. The screen,
/// trimmed at its end, ends in `$` or `#`. A shell whose rc sets another `PS1` is not covered: a pane
/// like that needs its own readiness check.
pub fn at_a_prompt(screen: &str) -> bool {
    let last = screen.trim_end();
    last.ends_with('$') || last.ends_with('#')
}

/// Polls `ready` every 25 ms until it returns `true`, or `timeout` elapses.
///
/// Returns whether it became true, so a caller can attach a message describing the state
/// it gave up on. Nothing in these tests sleeps for a fixed duration waiting on the
/// product: a watcher's evaluation, a tmux hook and a notification thread all settle in
/// their own time, and a poll with a ceiling is both faster and less brittle than a sleep
/// long enough to be "safe".
pub fn wait_for(mut ready: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Writes an executable `/bin/sh` script at `path`. `body` is appended after the shebang.
pub fn write_script(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    make_executable(path);
}

/// A copy of this machine's `tmux` called `name`, made in `dir`: a program whose name is the
/// agent's, for a fake that has to read to tmux as that agent. tmux reports the name of the
/// process that has a pane's terminal as the pane's command, and on macOS that is the
/// interpreter for a `#!` script, so a script cannot stand in for `claude`; a copy of a real
/// binary can. Run as `<copy> -L <socket> wait-for <channel>` (see [`Server::wait_line`]) it
/// does nothing until the test signals the channel, and then ends.
pub fn tmux_named(dir: &Path, name: &str) -> PathBuf {
    let copy = dir.join(name);
    std::fs::copy(taw::tmux::Tmux::with_socket_name("unused").bin, &copy).unwrap();
    make_executable(&copy);
    copy
}

/// An agent called `name` that a pane's shell can run, and that tmux reports as `name` for as long
/// as it runs: what `taw agent start` takes for the agent without a watcher to read its screen.
/// `dir` is a directory of the test's (a tempdir); the returned directory, `dir/bin`, goes first
/// on the pane shell's `$PATH` (`ServerOptions::path_prefix`).
///
/// What the shell runs is a script, which records the arguments it was given in `argv` when that
/// is asked for and then becomes a copy of tmux called `name`, so that the pane's command is
/// `name` and not the script's interpreter. The copy waits on a channel nobody signals, and
/// reaches the test's server through `$TAW_TMUX_SOCKET_NAME`, which the panes of a [`Server`] have
/// from the server's own environment.
pub fn named_agent(dir: &Path, name: &str, argv: Option<&Path>) -> PathBuf {
    let bin = dir.join("bin");
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let copy = tmux_named(&copy_dir, name);
    let record = argv
        .map(|path| format!("printf '%s\\n' \"$@\" > '{}'\n", path.display()))
        .unwrap_or_default();
    write_script(
        &bin.join(name),
        &format!(
            "{record}exec '{}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-{name}-never\n",
            copy.display()
        ),
    );
    bin
}

/// The pid a fake agent's wrapper wrote to `path`, once it is there. The file exists from
/// the moment the shell opens it for writing and holds nothing until the write lands, so a
/// reader that only asks whether it exists can read an empty pid.
pub fn read_pid(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// An environment variable of this process, set for as long as the guard lives, and put back as it was
/// when it is dropped: the value it had (the gate's refusing recorder of `$TAW_NOTIFY_CMD`, say), or none.
/// A test that sets a process-global variable for itself and then removes it takes away what the gate
/// gave every test after it.
///
/// The environment is process-global: whoever holds one holds the lock that keeps every other thread of
/// the test binary from reading or writing the environment meanwhile (the `SERIAL` of
/// tests/tmux_watch.rs), declared before the guard so that it is dropped after it.
pub struct EnvVar {
    name: &'static str,
    before: Option<std::ffi::OsString>,
}

impl EnvVar {
    pub fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvVar {
        let before = std::env::var_os(name);
        // SAFETY: the caller holds the lock that serialises the tests that touch the environment
        // (see above), and no thread of ours is reading it: set before the watcher thread is spawned.
        unsafe { std::env::set_var(name, value) };
        EnvVar { name, before }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: as in `set`: the lock is still held (the guard is dropped first), and the threads
        // the test started have been joined.
        unsafe {
            match &self.before {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }
}

/// How a [`Server`]'s panes are started.
pub struct ServerOptions {
    /// The session name of the first session (default `one`).
    pub session: String,
    pub width: u32,
    pub height: u32,
    /// Prepended to every pane shell's `$PATH`.
    ///
    /// Set from *inside* the shell (`default-command` runs `export PATH=…; exec /bin/sh`)
    /// rather than handed in by tmux: neither `set-environment -g PATH` nor `new-session
    /// -e PATH=…` reliably reaches a freshly spawned pane's process (found out
    /// empirically), whereas `export` + `exec` depends only on POSIX shell
    /// semantics — `export` sets it in the current process, `exec` keeps that process's
    /// environment while replacing the program.
    pub path_prefix: Option<PathBuf>,
    /// Extra variables put into the tmux **server's** environment, and so inherited by
    /// every `pipe-pane` watcher and `run-shell` hook it spawns, as well as passed to
    /// every [`Server::taw`] invocation.
    pub env: Vec<(String, String)>,
    /// Variables taken out of the server's environment (and so out of everything it starts),
    /// whatever this fixture or the process running it has for them: for a test of what runs
    /// with none of them (`tests/desktop_net.rs` plants a watcher with no recorder at all).
    pub env_remove: Vec<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            session: "one".to_string(),
            width: 80,
            height: 24,
            path_prefix: None,
            env: Vec::new(),
            env_remove: Vec::new(),
        }
    }
}

/// A throwaway tmux server plus the tempdirs `taw` is pointed at, all torn down on drop.
pub struct Server {
    socket: String,
    config: tempfile::TempDir,
    state: tempfile::TempDir,
    /// The cwd every pane starts in — a tempdir, so no project config in the checkout
    /// (`mise.toml`, `.envrc`, direnv, …) can run inside a test pane.
    work: tempfile::TempDir,
    /// The `$HOME` and `$ZDOTDIR` of every shell the server runs. See [`shell_env`].
    home: shell_env::ShellHome,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
    /// The server's pid, read once it has started: the process group its jobs run in, for a `Drop`
    /// that can no longer ask the server (a test ended it itself).
    pid: Option<libc::pid_t>,
}

impl Server {
    /// A server whose panes are plain `/bin/sh`.
    pub fn start() -> Server {
        Server::start_with(ServerOptions::default())
    }

    pub fn start_with(mut options: ServerOptions) -> Server {
        // `cargo test` runs the tests in one file as concurrent threads of a single
        // process, so `std::process::id()` alone is the same for all of them: a socket
        // name built only from the pid would make every test race to create its session
        // on the very same server. The counter gives each `Server` its own socket.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("taw-e2e-{}-{n}", std::process::id());

        let mut server = Server {
            socket,
            config: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            home: shell_env::ShellHome::new(),
            env: std::mem::take(&mut options.env),
            env_remove: std::mem::take(&mut options.env_remove),
            pid: None,
        };

        // No test in this suite may reach the network: a fresh marker makes `taw manifest
        // update --if-stale` (fired by the plugin script's startup one-shots) a no-op, and a
        // watcher's refresh too. (`shell_env::OFFLINE`, in `base_env`, is the net under it.)
        shell_env::quiet_state(server.state_dir());

        server.start_the_session(&options);
        server
    }

    /// Starts the tmux server with `options`' first session, and sets what every later pane needs. The
    /// one place that spells the start, for [`Server::start_with`] and [`Server::crash_and_start_afresh`].
    fn start_the_session(&mut self, options: &ServerOptions) {
        let pane_command = match &options.path_prefix {
            Some(dir) => format!("export PATH={}:$PATH; exec /bin/sh", dir.display()),
            None => "/bin/sh".to_string(),
        };

        let width = options.width.to_string();
        let height = options.height.to_string();
        let new_session: [&str; 13] = [
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            &options.session,
            "-x",
            &width,
            "-y",
            &height,
            "-c",
            self.work_dir().to_str().unwrap(),
            &pane_command,
        ];
        // A start straight after `kill-server` ([`Server::crash_and_start_at_once`]) can meet the old server as
        // it goes: tmux's client then says the server exited, and the start is made again, for a second at most.
        // Any other failure is the test's to see.
        let mut tries = 0;
        loop {
            match self.tmux(&new_session) {
                Ok(_) => break,
                Err(err) if err.contains("server exited unexpectedly") && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("tmux new-session failed: {err}"),
            }
        }
        // Its socket file goes with the server's home, after `Drop` has killed the server.
        self.home
            .adopt(&taw::tmux::Tmux::with_socket_name(&self.socket));
        self.pid = self.server_pid();

        // Every pane created later (splits, new windows, new sessions, and the ones
        // `session restore` recreates) gets the same treatment.
        self.tmux(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        if let Some(dir) = &options.path_prefix {
            self.tmux(&[
                "set-option",
                "-g",
                "default-command",
                &format!("export PATH={}:$PATH; exec /bin/sh", dir.display()),
            ])
            .unwrap();
        }
        // tmux kills a server that runs out of sessions. Several tests kill a session on
        // purpose (so `session restore` has something to recreate), which would otherwise
        // take the whole throwaway server down mid-test.
        self.tmux(&["set-option", "-g", "exit-empty", "off"])
            .unwrap();
    }

    /// Kills the server as a crash would end it and starts a new one on the same socket, with one session
    /// `options.session` (its size and `path_prefix` too; its `env` is the first server's, kept), the way
    /// [`Server::start_with`] starts the first: `-f /dev/null`, the shell home, the socket file adopted,
    /// the new pid recorded, `default-shell` and `exit-empty off`, and `default-command` for a
    /// `path_prefix`. A test of restore after a crash needs this: only this fixture may start a server,
    /// and the new server must be the same kind as the one that went. It waits until the old server
    /// has gone, ends what the old one left in its process group ([`end_what_a_server_left`], as
    /// `Drop` does), then pauses two seconds: a snapshot's `taken_unix` and tmux's `#{start_time}` are
    /// whole seconds, and a new server that started in the same second as the save would look no
    /// newer than it.
    pub fn crash_and_start_afresh(&mut self, options: &ServerOptions) {
        let old = self.server_pid().or(self.pid);
        let _ = self.tmux(&["kill-server"]);
        assert!(
            wait_for(
                || self.tmux(&["list-sessions"]).is_err(),
                Duration::from_secs(20)
            ),
            "the server did not go"
        );
        end_what_a_server_left(old, self.pid);
        std::thread::sleep(Duration::from_secs(2));
        self.start_the_session(options);
    }

    /// Like [`Server::crash_and_start_afresh`] with nothing in between: the new server is started the
    /// moment the old one is told to go, with no wait for it to be gone and no pause, so that something
    /// that looks at the server every so often can see the new one without ever having seen none (the
    /// gap is a few tens of milliseconds, so a look that falls in it still sees none). What the old server
    /// left is ended afterwards.
    pub fn crash_and_start_at_once(&mut self, options: &ServerOptions) {
        let old = self.server_pid().or(self.pid);
        let _ = self.tmux(&["kill-server"]);
        self.start_the_session(options);
        end_what_a_server_left(old, None);
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// The pid of the server, as the server says it, while it answers.
    fn server_pid(&self) -> Option<libc::pid_t> {
        self.tmux(&["display-message", "-p", "#{pid}"])
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .filter(|pid| *pid > 1)
    }

    pub fn config_dir(&self) -> &Path {
        self.config.path()
    }

    pub fn state_dir(&self) -> &Path {
        self.state.path()
    }

    pub fn work_dir(&self) -> &Path {
        self.work.path()
    }

    /// The `$HOME` of the server's shells, and of everything it and this fixture run.
    pub fn home_dir(&self) -> &Path {
        self.home.path()
    }

    /// Writes `config.toml` into this server's `TAW_CONFIG_DIR`.
    pub fn write_config(&self, toml: &str) {
        std::fs::write(self.config_dir().join("config.toml"), toml).unwrap();
    }

    /// The env every child process of this fixture gets: the shell environment ([`shell_env`]), the
    /// socket and the two path roots, plus whatever [`ServerOptions::env`] added, which wins.
    ///
    /// The tmux server inherits the shell environment from the first command, and so does every
    /// pane's shell: a `HISTFILE` of `/dev/null`, since a shell that is typed into saves what it was
    /// typed when the server is killed, to `$HISTFILE`, which is `~/.bash_history` where nothing says
    /// otherwise; and a `HOME` and `ZDOTDIR` of the server's own, so that no shell reads the startup
    /// files of whoever runs the suite. The directories of [`shell_env::PINNED`] are in it, inside
    /// that home (the XDG ones, zoxide's, every agent's config), so that nothing the server starts or
    /// this fixture runs reads or writes the runner's own; [`shell_env::OFFLINE`], so that none of
    /// it reaches the network; and refusing recorders for [`shell_env::DESKTOP_SEAMS`] and
    /// [`shell_env::SSH_SEAMS`], so that none of it shows a notification, plays a sound or logs in
    /// to another machine for whoever runs the suite.
    fn base_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .home
            .server_vars()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect();
        env.extend([
            ("TAW_TMUX_SOCKET_NAME".to_string(), self.socket.clone()),
            (
                "TAW_CONFIG_DIR".to_string(),
                self.config_dir().display().to_string(),
            ),
            (
                "TAW_STATE_DIR".to_string(),
                self.state_dir().display().to_string(),
            ),
        ]);
        env.extend(self.env.iter().cloned());
        env
    }

    /// Puts [`Server::base_env`] in the environment of `cmd`, having taken out of it what a runner could
    /// export that moves a recorder's log ([`shell_env::RECORDER_LOG_VARIABLES`]). What the fixture's own
    /// options add comes with `base_env`, so it still wins.
    fn apply_base_env(&self, cmd: &mut Command) {
        for name in shell_env::RECORDER_LOG_VARIABLES {
            cmd.env_remove(name);
        }
        for (k, v) in self.base_env() {
            cmd.env(k, v);
        }
    }

    /// What every `taw` this fixture runs has in its environment, name and value: [`Server::base_env`], which
    /// holds the shell home, the pinned directories, the refusing recorders and the server's own socket
    /// name, config folder and state folder. For a `taw` that something else starts on this server's behalf
    /// (the far side of the stand-in ssh, `tests/common/machine.rs`): it adds to this and takes nothing away.
    pub fn taw_env(&self) -> Vec<(String, String)> {
        self.base_env()
    }

    /// The calls this fixture's refusing recorder of ssh has noted, one line each (`TAW_SSH_BIN`, a tab, the
    /// arguments): the `taw` it runs reached for ssh, and a test that sets no stand-in sees how often.
    pub fn ssh_calls(&self) -> Vec<String> {
        self.home.ssh_calls()
    }

    /// The built `taw` as a command to run against this fixture, with [`Server::taw_env`] set and what
    /// would point it at another tmux (`$TMUX`, `$TMUX_PANE`) and at a report file taken out. Its standard
    /// streams are the caller's to set.
    pub fn taw_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(taw_bin());
        cmd.args(args);
        self.apply_base_env(&mut cmd);
        cmd.env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE");
        cmd
    }

    /// Runs raw `tmux -L <socket> <args>` — fixture setup and independent verification
    /// only; the product itself is always exercised through [`Server::taw`].
    pub fn tmux(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new("tmux");
        cmd.arg("-L").arg(&self.socket).args(args);
        // Without git's variables (`GIT_DIR` and the rest of `taw::git::REDIRECTING_VARIABLES`), whatever
        // the test process has (a `rebase -x` gives it `GIT_DIR`): the server, and so every pane, hook
        // and watcher it starts, would run a git on the repository they name. A test that wants one
        // puts it in `ServerOptions::env`, which comes after.
        for name in taw::git::REDIRECTING_VARIABLES {
            cmd.env_remove(name);
        }
        // Nor what herdr gives its panes, which a runner inside herdr has ([`shell_env::HERDR_RUNNER_VARIABLES`]).
        for name in shell_env::HERDR_RUNNER_VARIABLES {
            cmd.env_remove(name);
        }
        self.apply_base_env(&mut cmd);
        // The first of these starts the server, whose environment goes without them.
        for name in &self.env_remove {
            cmd.env_remove(name);
        }
        // `$TMUX`/`$TMUX_PANE` are cleared so nothing here accidentally addresses the
        // tmux session this test suite may itself be running inside.
        cmd.env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE");
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// `tmux …`, panicking on failure — for setup steps that must have worked.
    pub fn tmux_ok(&self, args: &[&str]) -> String {
        self.tmux(args)
            .unwrap_or_else(|err| panic!("tmux {} failed: {err}", args.join(" ")))
    }

    /// Runs the built `taw` binary against this fixture: `(exit code, stdout, stderr)`.
    pub fn taw_raw(&self, args: &[&str]) -> (i32, String, String) {
        self.taw_raw_with_env(args, &[])
    }

    pub fn taw_raw_with_env(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, String, String) {
        // `taw_command` clears `$TMUX` and the rest before `extra` is applied, so that a caller who passes
        // `$TMUX_PANE` (as `taw_in_pane` does) has it: clearing after would take it out again.
        let mut cmd = self.taw_command(args);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// Runs `taw` and parses its stdout as the JSON envelope: `(exit code, envelope)`.
    pub fn taw(&self, args: &[&str]) -> (i32, Value) {
        let (code, stdout, stderr) = self.taw_raw(args);
        (code, parse_envelope(args, &stdout, &stderr))
    }

    pub fn taw_with_env(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, Value) {
        let (code, stdout, stderr) = self.taw_raw_with_env(args, extra);
        (code, parse_envelope(args, &stdout, &stderr))
    }

    /// Like [`Server::taw`], with what the binary logs to stderr written to `log` as it goes (it
    /// logs at the level `TAW_LOG` in [`ServerOptions::env`] names): for a test whose failure message,
    /// built while the verb still runs, says what the verb saw.
    pub fn taw_logging_to(&self, args: &[&str], log: &Path) -> (i32, Value) {
        let mut cmd = Command::new(taw_bin());
        cmd.args(args);
        self.apply_base_env(&mut cmd);
        cmd.env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE");
        cmd.stderr(std::fs::File::create(log).unwrap());
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        (
            out.status.code().unwrap_or(-1),
            parse_envelope(args, &stdout, ""),
        )
    }

    /// Starts the built `taw` against this fixture and leaves it running, its output thrown
    /// away: for a test that has to end a command in the middle of what it is doing.
    pub fn spawn_taw(&self, args: &[&str]) -> Child {
        let mut cmd = Command::new(taw_bin());
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.apply_base_env(&mut cmd);
        cmd.env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE");
        cmd.spawn().unwrap()
    }

    /// Like [`Server::taw`], with `$TMUX_PANE` set — for the verbs that read "the current
    /// pane" from the environment.
    pub fn taw_in_pane(&self, pane: &str, args: &[&str]) -> (i32, Value) {
        self.taw_with_env(args, &[("TMUX_PANE", pane)])
    }

    /// The envelope's `result`, asserting exit 0 and the expected `id` and `result.type`.
    pub fn ok(&self, args: &[&str], id: &str, result_type: &str) -> Value {
        let (code, env) = self.taw(args);
        assert_eq!(code, 0, "`taw {}` failed: {env}", args.join(" "));
        assert_eq!(env["id"], id, "wrong envelope id: {env}");
        assert!(env.get("error").is_none(), "unexpected error: {env}");
        let result = env["result"].clone();
        assert_eq!(result["type"], result_type, "wrong result.type: {env}");
        result
    }

    /// The raw tmux pane id (`%N`) of the first pane of `session`.
    pub fn pane_of(&self, session: &str) -> String {
        self.tmux_ok(&["display-message", "-p", "-t", session, "#{pane_id}"])
    }

    /// The raw tmux pane id (`%N`) of the server's single starting pane.
    pub fn pane(&self) -> String {
        self.pane_of("one")
    }

    /// `taw`'s own public pane id (`w<n>:p<n>`) for a raw tmux pane id.
    pub fn public_pane_id(&self, pane: &str) -> String {
        let got = self.ok(&["pane", "get", pane], "cli:pane:get", "pane");
        got["pane"]["pane_id"].as_str().unwrap().to_string()
    }

    /// One pane option read straight from tmux, `""` when unset — the shape every
    /// assertion here wants, and read independently of `taw`'s own read path so a test
    /// proves what the product actually wrote.
    pub fn pane_opt(&self, pane: &str, name: &str) -> String {
        self.tmux_ok(&["show-options", "-pqv", "-t", pane, name])
    }

    pub fn set_pane_opt(&self, pane: &str, name: &str, value: &str) {
        self.tmux_ok(&["set-option", "-p", "-t", pane, name, value]);
    }

    /// One global option, `""` when unset.
    pub fn global_opt(&self, name: &str) -> String {
        self.tmux_ok(&["show-options", "-gqv", name])
    }

    /// One `display-message -p` format expansion against `target`.
    pub fn display(&self, target: &str, format: &str) -> String {
        self.tmux_ok(&["display-message", "-p", "-t", target, format])
    }

    /// The watchers' own logs, for a failure message: a watcher is a child of the tmux
    /// server, so its `TAW_LOG` is the one the server was started with.
    pub fn watcher_logs(&self) -> String {
        let mut out = String::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir().join("logs")) {
            for entry in entries.flatten() {
                out.push_str(&format!("== {}\n", entry.path().display()));
                out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        out
    }

    /// How many times the watcher of `pane` has read it and reached a decision. Counted
    /// from its log, so the server has to have been started with `TAW_LOG=debug`.
    pub fn watcher_reads(&self, pane: &str) -> usize {
        std::fs::read_to_string(
            self.state_dir()
                .join("logs")
                .join(format!("watch-{pane}.log")),
        )
        .map(|log| log.matches(" decision: ").count())
        .unwrap_or(0)
    }

    /// `#{pane_pipe}` — whether `pipe-pane` currently has a watcher on this pane.
    pub fn piped(&self, pane: &str) -> String {
        self.display(pane, "#{pane_pipe}")
    }

    /// The pane's visible screen text.
    pub fn capture(&self, pane: &str) -> String {
        self.tmux_ok(&["capture-pane", "-p", "-t", pane])
    }

    /// Waits (20 s) until the shell of `pane` is at its prompt, and fails with the screen it last saw.
    ///
    /// A line typed with `send-keys -l` before the shell reads its terminal goes through the tty's own
    /// line buffer, which keeps 1024 bytes on macOS and drops the rest, Enter too, so the line never
    /// runs. [`Server::start_with`] does not wait for the first prompt (some tests give a pane's shell a
    /// slow rc or an odd `PS1` to test taw's own shell-readiness rules), so a test that types a shell
    /// line into a fresh pane calls this first. Never call it while a program other than the shell is in
    /// front of the pane.
    pub fn wait_for_prompt(&self, pane: &str) {
        let seen = |server: &Server| {
            server
                .tmux(&["capture-pane", "-p", "-t", pane])
                .unwrap_or_default()
        };
        assert!(
            wait_for(|| at_a_prompt(&seen(self)), Duration::from_secs(20)),
            "the shell of {pane} never came to its prompt:\n{}",
            seen(self)
        );
    }

    /// The line to type into a pane to run `waiter`, a [`tmux_named`] copy, until [`Server::wake`]
    /// signals `channel`.
    pub fn wait_line(&self, waiter: &Path, channel: &str) -> String {
        format!(
            "'{}' -L '{}' wait-for {channel}",
            waiter.display(),
            self.socket
        )
    }

    /// Lets the program that [`Server::wait_line`] started on `channel` end.
    pub fn wake(&self, channel: &str) {
        self.tmux_ok(&["wait-for", "-S", channel]);
    }

    /// Sources the real `tmux-agent-wizard.tmux` into this server with `$TAW_BIN` pointed
    /// at the binary under test.
    ///
    /// `run-shell` (not `source-file`): the entry point is a bash script, not a file of
    /// tmux commands, and this is how a user's TPM runs it. Without `-b` it blocks until
    /// the script returns, so the options, key bindings and hooks it installs are in place
    /// when this call comes back — the *startup one-shots* it fires with `run-shell -b`
    /// are still asynchronous and have to be waited for.
    pub fn source_plugin(&self) {
        let script = plugin_script();
        self.tmux_ok(&[
            "run-shell",
            &format!("TAW_BIN='{}' bash '{}'", taw_bin(), script.display()),
        ]);
    }
}

impl Drop for Server {
    /// Ends the server, and everything it set off, before the directories go (the fields drop after
    /// this). Killing the server closes every pane, and a `pipe-pane` watcher reads the end of its
    /// pipe and goes, but what a hook started with `run-shell -b` outlives it: the plugin's autosave
    /// on a closed session marks itself wanted in the state directory the moment its `taw` starts,
    /// and one that a test set off just before its end, still starting when the directories were
    /// removed, made them again. tmux runs those (and the watchers) in the process group of the
    /// server, which the server leads, and a pane's processes in groups of their own: so once the
    /// server has gone, its group is ended and waited for ([`end_what_a_server_left`]). The group
    /// is the server's own pid, read off the server (or, for one a test ended itself, at its start),
    /// and nothing outside it is touched.
    fn drop(&mut self) {
        let live = self.server_pid();
        let _ = self.tmux(&["kill-server"]);
        end_what_a_server_left(live, self.pid);
    }
}

/// Ends the process group of a tmux server that has been told to end, once the server has gone:
/// the jobs it ran (hooks, watchers), which outlive it. `live` is the pid the server gave just
/// before it was told to end, `started` the one it gave when it started, for a server that no
/// longer answered (a test ended it itself). A pid is given to another process once its own has
/// gone, so the group of `started` is signalled only once the process that has that pid is gone:
/// one that is still there is not the server (a server that a test ended is not), and its group is
/// not the server's. A group's own id, by contrast, is nobody else's while anything is in it.
pub fn end_what_a_server_left(live: Option<libc::pid_t>, started: Option<libc::pid_t>) {
    // Never 0 or 1, which would be this process's own group, or every process.
    let Some(group) = live.or(started).filter(|pid| *pid > 1) else {
        return;
    };
    // SAFETY: `kill` takes two integers and touches no memory of this process; signal 0 sends
    // nothing, and asks whether the process (or, for a negative pid, a process of the group) is
    // there.
    let there = |pid: libc::pid_t| unsafe { libc::kill(pid, 0) } == 0;
    let gone = wait_for(|| !there(group), Duration::from_secs(10));
    if !gone && live.is_none() {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        if !there(-group) {
            break;
        }
        // SAFETY: as above; the group is the server's, which only what it started is in.
        unsafe { libc::kill(-group, signal) };
        wait_for(|| !there(-group), Duration::from_secs(5));
    }
}

/// A `$BASH_ENV` file, made in `dir`, that stops a bash script at a moment of the test's choosing: a
/// DEBUG trap sends the script TERM, once, just before the first command whose text begins with
/// `stop_at` (bash runs a pending trap at the start of the next command, so that is where a signal
/// that lands at that moment is handled), in its functions and subshells as well (`set -T`). As it
/// does, it notes the script's `$TMUX_TMPDIR`, `$!` and `$SCRATCH` (each empty when there is none),
/// read in a subshell with `set +u`: what [`Stop::noted`] reads.
pub struct Stop {
    pub bash_env: PathBuf,
    noted: PathBuf,
}

/// What a [`Stop`] noted as it stopped the script, or nothing, when it never did.
#[derive(Debug, Default)]
pub struct Noted {
    pub tmux_tmpdir: String,
    pub last_background_pid: libc::pid_t,
    pub scratch: String,
}

impl Stop {
    pub fn at(dir: &Path, stop_at: &str) -> Stop {
        let bash_env = dir.join("stop.sh");
        let noted = dir.join("stopped.txt");
        std::fs::write(
            &bash_env,
            format!(
                "set -T\n\
                 __stop_at='{stop_at}'\n\
                 __noted='{}'\n\
                 trap 'if [ -z \"${{__stopped:-}}\" ]; then case \"$BASH_COMMAND\" in \"$__stop_at\"*) __stopped=1; ( set +u; printf \"%s\\n%s\\n%s\\n\" \"$TMUX_TMPDIR\" \"$!\" \"$SCRATCH\" ) > \"$__noted\"; kill -TERM $$ ;; esac; fi' DEBUG\n",
                noted.display()
            ),
        )
        .unwrap();
        Stop { bash_env, noted }
    }

    /// What the stop noted, once it has (None when the script never got to the command).
    pub fn noted(&self) -> Option<Noted> {
        let text = std::fs::read_to_string(&self.noted).ok()?;
        let mut lines = text.lines();
        Some(Noted {
            tmux_tmpdir: lines.next().unwrap_or_default().to_string(),
            last_background_pid: lines.next().and_then(|pid| pid.parse().ok()).unwrap_or(0),
            scratch: lines.next().unwrap_or_default().to_string(),
        })
    }
}

/// A `sleep` that leads no process group and is no child of this process: it is in this process's
/// group, and init has it as its child, so a signal that ends it leaves no zombie of it that would
/// still answer as if it were there. Ended by its pid when dropped, if it still is that sleep.
pub struct LoneProcess(libc::pid_t);

impl LoneProcess {
    const COMMAND: &'static str = "sleep 297";

    pub fn start() -> LoneProcess {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{} > /dev/null 2>&1 & echo $!", Self::COMMAND))
            .output()
            .unwrap();
        let pid: libc::pid_t = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert!(pid > 1, "no lone process: {out:?}");
        LoneProcess(pid)
    }

    pub fn pid(&self) -> libc::pid_t {
        self.0
    }

    /// Whether it is still there: the pid is there and its command line is still the sleep's.
    pub fn alive(&self) -> bool {
        Command::new("ps")
            .args(["-ww", "-o", "command=", "-p", &self.0.to_string()])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == Self::COMMAND)
    }
}

impl Drop for LoneProcess {
    fn drop(&mut self) {
        if self.alive() {
            // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the
            // sleep's, checked just now, and is more than 1.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
}

/// A `tmux` in `dir`, for the front of a script's `PATH`, which gives `pid` for the server's pid: it
/// answers `display-message -p '#{pid}'`, the question a script asks of a server it is about to end,
/// with `pid`, and notes in `asked` that it did, and it passes everything else to the real tmux. With
/// `pid` a lone process of the test's, which leads no group, that process stands for a pid that a
/// server which has gone had: what the script then ends as the server's group must not be it.
pub fn tmux_giving_the_pid(dir: &Path, pid: libc::pid_t, asked: &Path) -> PathBuf {
    let path = dir.join("tmux");
    write_script(
        &path,
        &format!(
            "for last; do :; done\n\
             if [ \"${{last:-}}\" = '#{{pid}}' ]; then\n\
               case \" $* \" in *' display-message '*) echo '{pid}'; echo \"$*\" >> '{asked}'; exit 0 ;; esac\n\
             fi\n\
             exec '{real}' \"$@\"\n",
            asked = asked.display(),
            real = which::which("tmux").unwrap().display()
        ),
    );
    path
}

/// Parses the CLI's stdout as the JSON envelope, panicking with both streams attached
/// when it is not valid JSON — that is always a bug worth seeing in full.
pub fn parse_envelope(args: &[&str], stdout: &str, stderr: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| {
        let cmd = args.join(" ");
        panic!(
            "`taw {cmd}` printed non-JSON stdout ({e}):\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        )
    })
}

/// A `claude` executable, on a pane shell's `$PATH`, that *is* the fake agent.
///
/// Two things make this necessary rather than pointing a pane straight at the fixture:
///
/// 1. `taw agent start … --kind claude` types the literal command `claude` into the pane
///    (herdr's contract: the canonical executable for the kind). So the fake has to be
///    reachable as `claude`.
/// 2. The fixture's own pid is not discoverable from tmux: on macOS a `#!` script's
///    `#{pane_current_command}` is the *interpreter*, and `#{pane_pid}` is the pane's
///    shell, not the agent it launched. The wrapper records `$$` and then `exec`s the
///    fixture — `exec` replaces the program while keeping the process, so the recorded pid
///    is the fixture's, and `kill -USR1` reaches the traps it installed.
pub struct FakeAgent {
    /// Directory to put on the pane shell's `$PATH` (holds the `claude` wrapper).
    pub bin_dir: PathBuf,
    pid_file: PathBuf,
}

impl FakeAgent {
    /// Creates the wrapper under `dir` (typically a tempdir).
    pub fn new(dir: &Path) -> FakeAgent {
        let bin_dir = dir.join("fake-bin");
        let pid_file = dir.join("fake-agent.pid");
        write_script(
            &bin_dir.join("claude"),
            &format!(
                "printf '%s\\n' \"$$\" > '{}'\nexec '{}' \"$@\"\n",
                pid_file.display(),
                fake_agent_fixture().display(),
            ),
        );
        FakeAgent { bin_dir, pid_file }
    }

    /// The fixture's pid, once it has recorded one.
    pub fn pid(&self) -> Option<String> {
        read_pid(&self.pid_file)
    }

    /// Waits for the fixture to have started and drawn its idle screen in `pane`, then
    /// returns its pid.
    ///
    /// Waiting for the *screen* matters: the agent's identity is recorded by `taw agent
    /// start` the moment the kind hint is set, but the watcher can only classify what the
    /// pane is showing, so a test that signals before the first screen is drawn is
    /// racing the redraw.
    pub fn wait_until_drawn(&self, server: &Server, pane: &str, timeout: Duration) -> String {
        assert!(
            wait_for(
                || self.pid().is_some() && server.capture(pane).contains("? for shortcuts"),
                timeout
            ),
            "the fake agent never drew its idle screen (pid {:?}, screen:\n{}\n)",
            self.pid(),
            server.capture(pane)
        );
        self.pid().unwrap()
    }

    /// Sends a signal to the fixture. `SIGUSR1` toggles idle <-> working, `SIGUSR2` moves
    /// it to the permission prompt, `SIGTERM` ends it.
    pub fn signal(&self, sig: &str) {
        let pid = self
            .pid()
            .expect("the fake agent has not recorded its pid yet");
        let status = Command::new("kill").args([sig, &pid]).status().unwrap();
        assert!(status.success(), "kill {sig} {pid} failed");
    }
}

/// A `$TAW_NOTIFY_CMD` script that appends `<title>|<body>` to a file, one line per
/// delivered notification — the documented test seam in `notify::system::send`.
pub struct NotifyRecorder {
    pub script: PathBuf,
    pub log: PathBuf,
}

impl NotifyRecorder {
    pub fn new(dir: &Path) -> NotifyRecorder {
        let log = dir.join("notifications.txt");
        let script = dir.join("record-notification.sh");
        write_script(
            &script,
            &format!("printf '%s|%s\\n' \"$1\" \"$2\" >> '{}'\n", log.display()),
        );
        NotifyRecorder { script, log }
    }

    /// Every delivered notification so far, as `<title>|<body>` lines.
    pub fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The delivered lines whose title carries `word` — `"blocked"` for a `request`
    /// notification, `"done"` for a `done` one (`notify::for_transition` builds the title
    /// as `"<agent> <word> · <session>:<window>"`).
    pub fn matching(&self, word: &str) -> Vec<String> {
        let marker = format!(" {word} \u{b7} ");
        self.lines()
            .into_iter()
            .filter(|line| line.contains(&marker))
            .collect()
    }
}
