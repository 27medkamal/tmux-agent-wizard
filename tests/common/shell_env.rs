//! The shell environment of a throwaway tmux server: what keeps a shell that one of its panes runs
//! away from the home of whoever runs the tests.
//!
//! A shell that is typed into saves what it was typed when its server is killed, to `$HISTFILE`,
//! which is `~/.bash_history` where nothing says otherwise: every run of the suite would add its
//! commands to the history of the person who ran it. A zsh reads `.zshenv` (for a command it is
//! given with `-c` as well) and, at a prompt, `.zshrc` from `$ZDOTDIR`, or from `$HOME` where that
//! is unset, and `/etc/zshrc` on macOS sets its history file there and overrides `$HISTFILE`. So a
//! server gets all three, a `HISTFILE` of `/dev/null` and a `HOME` and a `ZDOTDIR` that are an
//! empty directory of its own, in the environment of the command that starts it. That environment
//! becomes the server's, and so every pane's, the first pane's too, which a `set-environment` sent
//! afterwards cannot reach.
//!
//! A server is started through [`ShellHome::start_server`] (the servers that give a client a
//! terminal included: a command given to `new-session` is run by the server's shell, whose
//! startup files are read first), and `tests/shell_isolation.rs` fails for one that is not.
//!
//! Five more things go with a server: four for the same reason (what it starts takes its environment
//! from the command that starts it), and one at its end:
//!
//! * **Nothing on the desktop of whoever runs the tests.** taw's `system` toast runs the desktop's
//!   notifier (`osascript`, `notify-send`) and its sound a player (`afplay`, `paplay`, `aplay`), unless
//!   `$TAW_NOTIFY_CMD` and `$TAW_SOUND_CMD` name a program in their place ([`DESKTOP_SEAMS`]). A test
//!   that records them sets its own; for one that does not, the server has a refusing recorder of the
//!   home's for each, which notes the call and refuses it, so that no watcher and no `taw` it runs puts
//!   a notification on the screen of whoever runs the tests (one did: "claude done" in a session named
//!   `one`). `tests/desktop_net.rs` plants one.
//! * **No ssh.** taw runs every ssh through `$TAW_SSH_BIN` (src/machine/ssh.rs), which is `ssh` on the
//!   `PATH` where nothing says otherwise: a `taw --machine` that a test ran by mistake would log in to a
//!   real machine. A server has a refusing recorder of the home's for it ([`SSH_SEAMS`]), which notes the
//!   call and refuses it as an ssh that cannot connect, and a test that wants a machine puts the stand-in
//!   of `tests/common/machine.rs` there instead.
//! * **None of the runner's own files.** The XDG directories, zoxide's database and the config
//!   directory of every agent whose integration taw installs or reads are found by variables of
//!   their own, and a variable the runner has set wins over a home of the server's. [`PINNED`] puts
//!   each inside the home, and `tests/shell_isolation.rs` fails for a variable the product reads that
//!   is not in it, and for a helper that does not pin one.
//! * **No network.** A watcher refreshes the cached agent manifests from herdr.dev, and so does the
//!   plugin script's startup one-shot, unless the marker of the last check in the state directory
//!   is fresh (or `[update] manifest_check` is off). [`quiet_state`] makes the marker, for the
//!   state directory the product finds by itself in this home (`$XDG_STATE_HOME`, which the
//!   server's environment puts in the home too), and [`OFFLINE`] puts a proxy that nothing listens
//!   on in the environment, which the HTTP client of the product reads: what a test starts without
//!   a marker cannot reach the network. `tests/offline.rs` proves both with a listener.
//! * **Its socket file.** tmux leaves the file of a server that has ended where it was, so the home
//!   remembers the exact path of each server it starts (what the server says it listens on) and
//!   takes the file away when the home is dropped, which is after the fixture that holds it has
//!   killed the server ([`take_the_socket_away`]). `tests/socket_files.rs` looks.

// Each test file that includes this module uses a subset of it; unused-item warnings would
// otherwise fail `clippy --all-targets -- -D warnings` for the other files' sake.
#![allow(dead_code)]

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use taw::tmux::{Socket, Tmux};

use super::wait_for;

/// Whether `name` is the file name of a socket that a throwaway server of these tests listens on:
/// `taw-` and something after it. `default` is never one: it is the socket of the tmux server of
/// whoever runs the tests.
pub fn is_a_test_socket(name: &str) -> bool {
    name != "default" && name.starts_with("taw-")
}

/// Panics for a `Tmux` that would reach the server of whoever runs the tests: one with no socket at
/// all (a tmux with neither `-L` nor `-S` goes to the socket in `$TMUX`, else to the default one),
/// or with a socket that is not a test socket ([`is_a_test_socket`]).
pub fn assert_private(t: &Tmux) {
    let name = match &t.socket {
        Some(Socket::Name(name)) => name.clone(),
        Some(Socket::Path(path)) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        None => panic!(
            "this tmux has neither -L nor -S, so it would reach the default server of whoever \
             runs the tests"
        ),
    };
    assert!(
        is_a_test_socket(&name),
        "the socket {name:?} is not a test server's (taw-...): it could be the one of whoever \
         runs the tests"
    );
}

/// Ends the server `t` names, if it is still there, waits for it to be gone, and removes the file it
/// listened on, `path`: the exact one the server said so itself (`#{socket_path}`). tmux leaves that
/// file behind when a server ends, one for every server a run of the tests started.
///
/// Nothing else is touched, and never by pattern: a path whose name is not a test server's
/// ([`is_a_test_socket`]), one that is not a socket file, one that is not the file of the server `t`
/// names, and the file of a server that is still there ten seconds after it was told to end (the file
/// is how it is reached) are all left alone.
pub fn take_the_socket_away(t: &Tmux, path: &Path) {
    let name_is_a_test_servers = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_a_test_socket);
    let is_a_socket =
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket());
    let is_the_file_of_t = match &t.socket {
        Some(Socket::Name(name)) => path.file_name().is_some_and(|file| file == name.as_str()),
        Some(Socket::Path(own)) => own == path,
        None => false,
    };
    if !(name_is_a_test_servers && is_a_socket && is_the_file_of_t) {
        return;
    }
    // `list-sessions` starts nothing: it fails once the server is gone, which is already so for a
    // fixture that ended its server before its home was dropped (one command, then).
    let mut up = t.run(&["list-sessions"]).is_ok();
    if up {
        let _ = t.run_ok(&["kill-server"]);
        up = !wait_for(
            || t.run(&["list-sessions"]).is_err(),
            Duration::from_secs(10),
        );
    }
    if !up {
        let _ = std::fs::remove_file(path);
    }
}

/// What goes in the environment of every process a test server starts, so that none of them reaches
/// the network: a proxy on a port nothing listens on, for the HTTP client of the product (it reads
/// `ALL_PROXY`, and `NO_PROXY` for the hosts to go round it, which is empty so that none is) and for
/// the tools that read the lower-case names. A fetch that is attempted fails at once, at home.
pub const OFFLINE: [(&str, &str); 4] = [
    ("ALL_PROXY", "http://127.0.0.1:1"),
    ("all_proxy", "http://127.0.0.1:1"),
    ("NO_PROXY", ""),
    ("no_proxy", ""),
];

/// Where the product, and the programs a test runs, find a person's own files besides the home, by the
/// variable that says so, and where each goes in a server's home: the XDG base directories (taw's
/// config and state where `TAW_CONFIG_DIR` and `TAW_STATE_DIR` do not say, OpenCode's and devin's
/// config), zoxide's database, and the config directory of each agent whose integration taw installs
/// or reads (`env_dir` in src/integration). Each is the place the product would take with the
/// variable unset and the home of the server's, so it does what it does for anybody, and nothing of
/// the runner's is read or written, whatever the runner has them set to.
///
/// `PI_CODING_AGENT_DIR` is pinned empty, which the product takes for unset: set, it is the one
/// directory of Pi and of OMP both, which changes what OMP does.
pub const PINNED: [(&str, &str); 16] = [
    ("XDG_CONFIG_HOME", ".config"),
    ("XDG_DATA_HOME", ".local/share"),
    ("XDG_STATE_HOME", ".local/state"),
    ("_ZO_DATA_DIR", ".local/share/zoxide"),
    ("ANTIGRAVITY_CLI_CONFIG_DIR", ".gemini/config"),
    ("CLAUDE_CONFIG_DIR", ".claude"),
    ("CODEX_HOME", ".codex"),
    ("COPILOT_HOME", ".copilot"),
    ("CURSOR_CONFIG_DIR", ".cursor"),
    ("GROK_HOME", ".grok"),
    ("HERMES_HOME", ".hermes"),
    ("KIMI_CODE_HOME", ".kimi-code"),
    ("PI_CODING_AGENT_DIR", ""),
    ("PI_CONFIG_DIR", ".omp"),
    ("QODER_CONFIG_DIR", ".qoder"),
    ("QWEN_HOME", ".qwen"),
];

/// What herdr gives the panes it runs, and so what a test run from inside one has: the variables that tell a
/// program it is inside herdr (`HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`), the
/// binary to call back (`HERDR_BIN_PATH`) and its socket (`HERDR_SOCKET_PATH`). A test server is started
/// without them and so is everything it starts: a test of what `@agent-wizard-herdr-env` gives a pane, or
/// of a pane that has none, would otherwise see the runner's. A test that wants one puts it in
/// `ServerOptions::env`, which comes after.
pub const HERDR_RUNNER_VARIABLES: [&str; 6] = [
    "HERDR_ENV",
    "HERDR_BIN_PATH",
    "HERDR_PANE_ID",
    "HERDR_SOCKET_PATH",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
];

/// The two variables taw runs a program of a test's own by, in place of one that reaches the desktop of
/// whoever runs the tests: the notifier of a `system` toast (`osascript` on macOS, `notify-send` on Linux:
/// src/notify/system.rs) and the player of a sound (`afplay`, or `paplay` or `aplay`: src/notify/sound.rs).
pub const DESKTOP_SEAMS: [&str; 2] = ["TAW_NOTIFY_CMD", "TAW_SOUND_CMD"];

/// The variable taw runs ssh by (src/machine/ssh.rs): the program a test puts in its place, as it does
/// for the notifier and the player. Left alone, taw runs `ssh` from the `PATH`.
pub const SSH_SEAMS: [&str; 1] = ["TAW_SSH_BIN"];

/// What a runner could export that moves what a refusing recorder writes: each recorder, the ssh one and
/// the desktop ones, notes its calls in the file this names when it is set (the tests of the nets use
/// that), else in its own log. The fixture takes these out of every command it starts (a [`ShellHome`]'s
/// and a `Server`'s), so that a call still lands where the fixture looks.
pub const RECORDER_LOG_VARIABLES: [&str; 2] = ["TAW_SSH_NET_LOG", "TAW_DESKTOP_NET_LOG"];

/// What the refusing recorder of [`SSH_SEAMS`] says, as an ssh that could not connect says its last line.
pub const SSH_REFUSAL: &str = "ssh: refused by the test seam";

/// A refusing recorder for each of [`DESKTOP_SEAMS`], in `dir`, called by the variable's name: what a
/// server started through a [`ShellHome`] has for them where its test sets none. Each notes the call (the
/// variable, what taw gave it, the directory and the pane) and refuses it, with the status of a notifier
/// that failed: in `$TAW_DESKTOP_NET_LOG` when that is set (a test of the net's), else in the log of the
/// gate's desktop net when this runs under the gate (scripts/desktop-net.sh, which names it in
/// `$TAW_DESKTOP_NET`), so that the gate fails and names the call, else in `dir`'s own log.
fn write_refusing_recorders(dir: &Path) {
    let log = match std::env::var_os("TAW_DESKTOP_NET") {
        Some(net) => PathBuf::from(net).join("log"),
        None => dir.join("log"),
    };
    for name in DESKTOP_SEAMS {
        super::write_script(
            &dir.join(name),
            &format!(
                "# The refusing recorder of a test server's (tests/common/shell_env.rs) that ${name}\n\
                 # names: taw was to show a notification or play a sound, for a test that records none.\n\
                 printf '%s\\t%s\\t%s\\t%s\\n' {name} \"$*\" \"$PWD\" \"${{TMUX_PANE:-}}\" >> \"${{TAW_DESKTOP_NET_LOG:-{log}}}\"\n\
                 echo '{name}: refused: a test that records no notification or sound has none' >&2\n\
                 exit 1\n",
                log = log.display()
            ),
        );
    }
}

/// The refusing recorder of [`SSH_SEAMS`], in `dir`, called by the variable's name: what a server started
/// through a [`ShellHome`] has for `$TAW_SSH_BIN` where its test sets none. It notes the call (its
/// arguments, one line) in `$TAW_SSH_NET_LOG` when that is set, else in `ssh-log` in `dir`, and refuses
/// it as ssh does when it cannot connect: [`SSH_REFUSAL`] on standard error and status 255.
fn write_ssh_recorder(dir: &Path) {
    for name in SSH_SEAMS {
        super::write_script(
            &dir.join(name),
            &format!(
                "# The refusing recorder of a test server's (tests/common/shell_env.rs) that ${name}\n\
                 # names: taw was to run ssh, for a test that has no stand-in machine.\n\
                 printf '%s\\t%s\\n' {name} \"$*\" >> \"${{TAW_SSH_NET_LOG:-{log}}}\"\n\
                 echo '{SSH_REFUSAL}' >&2\n\
                 exit 255\n",
                log = dir.join("ssh-log").display()
            ),
        );
    }
}

/// Says that the manifests were checked a moment ago, in the state directory `state_dir`: the marker
/// the watcher and the plugin's startup one-shot look at before they reach out to herdr.dev, which
/// they do for no marker or one older than six hours.
pub fn quiet_state(state_dir: &Path) {
    let marker_dir = state_dir.join("agent-detection");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(marker_dir.join("last-check"), "").unwrap();
}

/// A directory that goes with one throwaway server, the `$HOME` and the `$ZDOTDIR` of every shell
/// the server runs, removed when this is dropped, and the servers started with it, whose socket
/// files go when it does.
pub struct ShellHome {
    dir: tempfile::TempDir,
    /// The refusing recorders of [`DESKTOP_SEAMS`] ([`write_refusing_recorders`]) and of [`SSH_SEAMS`]
    /// ([`write_ssh_recorder`]), beside the home and not in it, where no shell's `~` lists them.
    desktop: tempfile::TempDir,
    /// The servers this home was used to start, each with the exact socket file it said it listens
    /// on.
    sockets: Mutex<Vec<(Tmux, PathBuf)>>,
}

impl ShellHome {
    pub fn new() -> ShellHome {
        let dir = tempfile::tempdir().unwrap();
        // Where the product looks for its state when the environment does not say: in this home.
        quiet_state(
            &dir.path()
                .join(".local")
                .join("state")
                .join("tmux-agent-wizard"),
        );
        let desktop = tempfile::tempdir().unwrap();
        write_refusing_recorders(desktop.path());
        write_ssh_recorder(desktop.path());
        ShellHome {
            dir,
            desktop,
            sockets: Mutex::new(Vec::new()),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The three variables, by name and value: the history file nothing is written to, and the
    /// home and the zsh startup directory that are this one.
    pub fn vars(&self) -> [(&'static str, String); 3] {
        let home = self.path().display().to_string();
        [
            ("HISTFILE", "/dev/null".to_string()),
            ("HOME", home.clone()),
            ("ZDOTDIR", home),
        ]
    }

    /// What a server started through this home has in its environment: [`ShellHome::vars`], the
    /// directories of [`PINNED`] (inside this home, whatever the person who runs the tests has them
    /// set to), [`OFFLINE`], [`ShellHome::desktop_seams`] and [`ShellHome::ssh_seams`]. A caller's own value for one of them
    /// (a test's recorder of its notifications, say) goes on after these, and wins.
    pub fn server_vars(&self) -> Vec<(&'static str, String)> {
        let mut vars: Vec<(&'static str, String)> = self.vars().into();
        vars.extend(PINNED.iter().map(|(name, place)| {
            let value = if place.is_empty() {
                String::new()
            } else {
                self.path().join(place).display().to_string()
            };
            (*name, value)
        }));
        vars.extend(
            OFFLINE
                .iter()
                .map(|(name, value)| (*name, value.to_string())),
        );
        vars.extend(self.desktop_seams());
        vars.extend(self.ssh_seams());
        vars
    }

    /// Each of [`DESKTOP_SEAMS`], naming this home's refusing recorder of it.
    pub fn desktop_seams(&self) -> [(&'static str, String); 2] {
        DESKTOP_SEAMS.map(|name| (name, self.desktop.path().join(name).display().to_string()))
    }

    /// Each of [`SSH_SEAMS`], naming this home's refusing recorder of it.
    pub fn ssh_seams(&self) -> [(&'static str, String); 1] {
        SSH_SEAMS.map(|name| (name, self.desktop.path().join(name).display().to_string()))
    }

    /// The calls this home's refusing recorder of ssh has noted, one line each: the variable and the
    /// arguments, apart by a tab.
    pub fn ssh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.desktop.path().join("ssh-log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Puts [`ShellHome::server_vars`] in the environment of `command`, and takes out of it each of
    /// [`HERDR_RUNNER_VARIABLES`], [`RECORDER_LOG_VARIABLES`] and `taw::git::REDIRECTING_VARIABLES`: a
    /// server that `command` starts, and every pane, hook and watcher of it, would otherwise run a git
    /// on the repository that a `GIT_DIR` of the test process's (which git gives the command of a
    /// `rebase -x`) names, and a recorder would write to the file a runner's log variable names.
    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        for name in taw::git::REDIRECTING_VARIABLES
            .iter()
            .chain(&HERDR_RUNNER_VARIABLES)
            .chain(&RECORDER_LOG_VARIABLES)
        {
            command.env_remove(name);
        }
        for (name, value) in self.server_vars() {
            command.env(name, value);
        }
        command
    }

    /// `tmux -L <socket> -u <args>`, as `Tmux::run` runs it, with [`ShellHome::server_vars`] in its
    /// environment. A server that command starts keeps them as its own.
    pub fn run(&self, t: &Tmux, args: &[&str]) -> Result<String, String> {
        self.run_with(t, &[], args)
    }

    /// [`ShellHome::run`] with variables of the caller's as well. `$TMUX` and `$TMUX_PANE` are
    /// taken out, so that nothing here addresses the tmux this suite may itself be run inside, and
    /// a `Tmux` that has no socket of a test's own is refused ([`assert_private`]): every command
    /// here, `kill-server` included, names its server with `-L` or `-S`.
    pub fn run_with(
        &self,
        t: &Tmux,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Result<String, String> {
        assert_private(t);
        let mut command = t.command();
        command.arg("-u").args(args);
        command
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE");
        self.apply(&mut command);
        command.envs(env.iter().copied());
        let out = command
            .output()
            .map_err(|err| format!("could not run tmux: {err}"))?;
        if out.status.success() {
            let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            if stdout.ends_with('\n') {
                stdout.pop();
            }
            Ok(stdout)
        } else {
            Err(format!(
                "tmux {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Starts the server `t` names, with no tmux.conf (a fresh `-L` socket still loads the host's)
    /// and a first session made with `new_session`, the arguments `new-session` takes, and returns
    /// what the command printed. The first command a server gets is the one that starts it, which
    /// is why this is not a `run` like the rest. The socket file of the server goes with this home.
    pub fn start_server(&self, t: &Tmux, new_session: &[&str]) -> String {
        self.start_server_with(t, &[], new_session)
    }

    /// [`ShellHome::start_server`] with variables of the caller's in the server's environment too:
    /// the ones the code under test finds the server and its own directories by.
    pub fn start_server_with(
        &self,
        t: &Tmux,
        env: &[(&str, &str)],
        new_session: &[&str],
    ) -> String {
        let mut args = vec!["-f", "/dev/null", "new-session"];
        args.extend_from_slice(new_session);
        let printed = self
            .run_with(t, env, &args)
            .unwrap_or_else(|err| panic!("the test server did not start: {err}"));
        self.adopt(t);
        printed
    }

    /// Makes the socket file of the running server `t` names one that goes with this home, for a
    /// server that something else started (a fixture that had its own way to, or the code under
    /// test, on a cold socket).
    pub fn adopt(&self, t: &Tmux) {
        if let Ok(path) = self.run(t, &["display-message", "-p", "#{socket_path}"]) {
            self.sockets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((t.clone(), PathBuf::from(path)));
        }
    }
}

impl Drop for ShellHome {
    /// After the fixture that holds the home has ended its server (a field drops once the `Drop` of
    /// what holds it has run), takes the socket file of each server it started away.
    fn drop(&mut self) {
        let sockets =
            std::mem::take(&mut *self.sockets.lock().unwrap_or_else(PoisonError::into_inner));
        for (tmux, path) in sockets {
            take_the_socket_away(&tmux, &path);
        }
    }
}

impl Default for ShellHome {
    fn default() -> Self {
        ShellHome::new()
    }
}
