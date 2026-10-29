//! `taw open [TERMS]...` end to end: the built binary, run from a shell the way someone at a
//! terminal runs it, against a real (throwaway) tmux server.
//!
//! `taw open` is tmux-session-wizard's `t <path or zoxide term>`. These tests check what a
//! person sees: a directory, `~/x`, `../x` or a zoxide term goes to its session, made as
//! `taw session open` makes it and named as it names it; inside tmux the client moves and
//! nothing is printed; outside tmux `--json` says what happened and attaches nothing; and
//! every way it can fail says so in one plain line, with the exit code a script can read.
//! (Attaching a terminal outside tmux, and the popup with no terms, need a real terminal
//! and are in `tests/e2e/acceptance.sh`.)
//!
//! Nothing here reaches beyond its own tempdirs and its own `-L` socket. `$HOME` is a
//! tempdir, so `~` is one; zoxide is a script that records how it is called and answers from
//! a file, and `_ZO_DATA_DIR` names a tempdir as well, so not one path a test opens reaches
//! anybody's real zoxide database.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::shell_env::ShellHome;
use common::{Server, taw_bin, wait_for};
use serde_json::Value;

/// What one run of the binary printed and how it ended.
#[derive(Debug)]
struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Holds a real attached client on the server, so a switch can be seen: `switch-client`
/// does nothing with nobody attached. Control mode (`-C`) needs no terminal, and the
/// client stays attached for as long as its stdin is open.
struct AttachedClient(std::process::Child);

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The second tmux server that gives the terminals of one test their ptys: each terminal is
/// a window of it, running the attach of a client of the test server, as a person's
/// terminal does. Its server is killed on drop, which is what ends those clients.
struct Outer(String);

impl Drop for Outer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output();
    }
}

/// One real terminal attached to the test server: its window on the outer server, and the
/// tty the test server knows its client by.
struct Terminal {
    window: String,
    tty: String,
}

/// One test's world: a server with its first session `one`, a home directory, a directory
/// of fakes on the front of `$PATH`, and (once asked for) an attached client.
struct World {
    server: Server,
    _tmp: tempfile::TempDir,
    /// Canonical: a temp directory on macOS is reached through the `/var` symlink, and tmux
    /// reports the resolved path.
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    zoxide_log: PathBuf,
    zoxide_answer: PathBuf,
    client: Option<AttachedClient>,
    /// Declared after the server, so the terminals are dropped once it is gone.
    outer: Option<Outer>,
    /// The home and the history file of the shells the outer server runs, which it needs from
    /// the moment it starts (`common::shell_env`). Last, so it goes after the server does.
    outer_home: ShellHome,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let world = World {
            // Whatever the server starts (a popup, a key binding's `run-shell`) keeps its
            // zoxide database here too, never the one of whoever runs the tests.
            server: Server::start_with(common::ServerOptions {
                env: vec![(
                    "_ZO_DATA_DIR".to_string(),
                    root.join("zoxide-data").display().to_string(),
                )],
                ..common::ServerOptions::default()
            }),
            home: root.join("home"),
            bin: root.join("bin"),
            zoxide_log: root.join("zoxide.log"),
            zoxide_answer: root.join("zoxide.answer"),
            client: None,
            outer: None,
            outer_home: ShellHome::new(),
            root,
            _tmp: tmp,
        };
        std::fs::create_dir_all(&world.home).unwrap();
        std::fs::create_dir_all(world.root.join("zoxide-data")).unwrap();
        std::fs::create_dir_all(&world.bin).unwrap();
        // zoxide as the real one behaves: every call is recorded (one line, each argument
        // followed by `\x1f`); `query` prints what the test put in the answer file, one
        // absolute path a line, or says `zoxide: no match found` and exits 1, or, when
        // the file holds `only-here`, `zoxide: you are already in the only match` and
        // exits 1 (both are what zoxide 0.9.9 says); `add` is silent.
        common::write_script(
            &world.bin.join("zoxide"),
            &format!(
                "{{ for a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\n'; }} >> '{log}'\n\
                 case \"$1\" in\n\
                   query)\n\
                     if [ \"$(cat '{answer}' 2>/dev/null)\" = only-here ]; then echo 'zoxide: you are already in the only match' >&2; exit 1; fi\n\
                     if [ -s '{answer}' ]; then cat '{answer}'; else echo 'zoxide: no match found' >&2; exit 1; fi ;;\n\
                 esac\n\
                 exit 0\n",
                log = world.zoxide_log.display(),
                answer = world.zoxide_answer.display(),
            ),
        );
        // A tmux for the one test that starts a server: no config file, `/bin/sh`, and no history
        // file; the home is the world's own, as it is for everything else here.
        common::write_script(
            &world.bin.join("tmux-quiet"),
            &format!(
                "HISTFILE=/dev/null HOME='{home}' ZDOTDIR='{home}' SHELL=/bin/sh exec tmux -f /dev/null \"$@\"\n",
                home = world.home.display()
            ),
        );
        world
    }

    /// `mkdir -p` under the root, and its canonical path.
    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The paths zoxide answers `query` with, from now on.
    fn zoxide_says(&self, paths: &[&Path]) {
        let lines: Vec<String> = paths.iter().map(|p| format!("{}\n", p.display())).collect();
        std::fs::write(&self.zoxide_answer, lines.concat()).unwrap();
    }

    /// Every call zoxide has had, in order, as its arguments.
    fn zoxide_calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.zoxide_log)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                line.trim_end_matches('\u{1f}')
                    .split('\u{1f}')
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    fn attach(&mut self, session: &str) {
        let child = Command::new("tmux")
            .arg("-L")
            .arg(self.server.socket())
            .args(["-C", "attach-session", "-t", session])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn a control-mode tmux client");
        // Owned before the wait below, so `Drop` reaps it if that panics.
        self.client = Some(AttachedClient(child));
        let attached = wait_for(
            || {
                self.server
                    .tmux(&["list-clients", "-F", "#{client_name}"])
                    .is_ok_and(|clients| !clients.is_empty())
            },
            Duration::from_secs(5),
        );
        assert!(attached, "no tmux client attached within 5s");
    }

    /// Moves the attached client to `session`, as if it had been switched by hand.
    fn move_client_to(&self, session: &str) {
        let client = self
            .server
            .tmux_ok(&["list-clients", "-F", "#{client_name}"]);
        self.server
            .tmux_ok(&["switch-client", "-c", &client, "-t", session]);
    }

    /// The ttys of every client the test server has, in the order it lists them, which is
    /// the order they attached in.
    fn client_ttys(&self) -> Vec<String> {
        self.server
            .tmux_ok(&["list-clients", "-F", "#{client_tty}"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A real terminal attached to `session`: a client with a pty of its own that takes
    /// keys as a person's does, so that which of several clients was used last is something
    /// tmux knows. Several can be on one session, the first attached first.
    fn terminal(&mut self, session: &str) -> Terminal {
        let before = self.client_ttys();
        let tmux = which::which("tmux").expect("tmux is on PATH");
        let attach = format!(
            "'{}' -L '{}' attach-session -t '{session}'",
            tmux.display(),
            self.server.socket()
        );
        let outer_socket = format!("{}-outer", self.server.socket());
        let outer_tmux = |args: &[&str]| -> String {
            let out = Command::new("tmux")
                .args(["-L", &outer_socket])
                .args(args)
                .env_remove("TMUX")
                .env_remove("TMUX_PANE")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "tmux {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let window = if self.outer.is_some() {
            outer_tmux(&["new-window", "-d", "-P", "-F", "#{window_id}", &attach])
        } else {
            let window = self.outer_home.start_server(
                &taw::tmux::Tmux::with_socket_name(&outer_socket),
                &[
                    "-d",
                    "-s",
                    "host",
                    "-x",
                    "120",
                    "-y",
                    "30",
                    "-P",
                    "-F",
                    "#{window_id}",
                    &attach,
                ],
            );
            // The next terminals are windows of a session nobody is attached to, which get
            // the default size, not this one's.
            outer_tmux(&["set-option", "-g", "default-size", "120x30"]);
            self.outer = Some(Outer(outer_socket.clone()));
            window
        };
        let tty = {
            let mut found = None;
            let attached = wait_for(
                || {
                    found = self
                        .client_ttys()
                        .into_iter()
                        .find(|tty| !before.contains(tty));
                    found.is_some()
                },
                Duration::from_secs(10),
            );
            assert!(attached, "no terminal attached to {session} within 10s");
            found.unwrap()
        };
        Terminal { window, tty }
    }

    /// What a terminal shows: the screen of the window that holds its client, popups and all.
    fn screen_of(&self, terminal: &Terminal) -> String {
        let outer = self.outer.as_ref().expect("a terminal was made first");
        let out = Command::new("tmux")
            .args(["-L", &outer.0, "capture-pane", "-p", "-t", &terminal.window])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Sends keys to a terminal as a person's typing does (`send-keys`, so they reach the
    /// client's own input): a popup that is up takes them, and so does the pane otherwise.
    fn send_keys(&self, terminal: &Terminal, keys: &[&str]) {
        let outer = self.outer.as_ref().expect("a terminal was made first");
        let out = Command::new("tmux")
            .args(["-L", &outer.0, "send-keys", "-t", &terminal.window])
            .args(keys)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Waits until `text` is on a terminal's screen, and fails with the screen when it is not.
    fn expect_on_screen(&self, terminal: &Terminal, text: &str) {
        let seen = wait_for(
            || self.screen_of(terminal).contains(text),
            Duration::from_secs(30),
        );
        assert!(
            seen,
            "{text:?} never showed on {}:\n{}",
            terminal.tty,
            self.screen_of(terminal)
        );
    }

    /// Types `text` into a terminal, as a person does, and waits for it to show at the
    /// prompt of the pane the terminal is looking at, which is when tmux has had the keys.
    fn type_in(&self, terminal: &Terminal, text: &str, pane: &str, shown: &str) {
        let outer = self.outer.as_ref().expect("a terminal was made first");
        let out = Command::new("tmux")
            .args([
                "-L",
                &outer.0,
                "send-keys",
                "-t",
                &terminal.window,
                "-l",
                text,
            ])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seen = wait_for(
            || self.server.capture(pane).contains(shown),
            Duration::from_secs(10),
        );
        assert!(
            seen,
            "{shown:?} did not show in {pane}: {:?}",
            self.server.capture(pane)
        );
    }

    /// The session a terminal's client is looking at.
    fn terminal_session(&self, terminal: &Terminal) -> String {
        self.server
            .tmux_ok(&["list-clients", "-F", "#{client_tty} #{client_session}"])
            .lines()
            .find_map(|line| {
                line.strip_prefix(&format!("{} ", terminal.tty))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("no client on {}", terminal.tty))
    }

    /// Puts every one of `terminals` on `session`, as if its person had gone there.
    fn put_on(&self, session: &str, terminals: &[&Terminal]) {
        for terminal in terminals {
            self.server
                .tmux_ok(&["switch-client", "-c", &terminal.tty, "-t", session]);
        }
    }

    /// The session the attached client is looking at.
    fn client_session(&self) -> String {
        self.server
            .tmux_ok(&["list-clients", "-F", "#{client_session}"])
    }

    /// Every session's name, sorted.
    fn sessions(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .server
            .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }

    /// The binary run from `cwd` with `args`, the environment a shell would give it:
    /// `$PWD` the directory it is in, the scratch `$HOME`, the fakes first on `$PATH`,
    /// and `$TMUX` set only when `in_tmux`.
    fn run(&self, cwd: &Path, args: &[&str], in_tmux: bool) -> Ran {
        self.run_with(cwd, args, in_tmux, &[])
    }

    fn run_with(&self, cwd: &Path, args: &[&str], in_tmux: bool, extra: &[(&str, &str)]) -> Ran {
        let mut cmd = Command::new(taw_bin());
        cmd.args(args).current_dir(cwd).env("PWD", cwd);
        self.finish(cmd, in_tmux, extra)
    }

    /// The binary run by a shell that has changed into `dir` and removed it, so that the
    /// directory the binary is in cannot be read (`getcwd` fails), as a shell is left in when
    /// the worktree it stands in is deleted.
    fn run_from_a_removed_directory(&self, dir: &Path, args: &[&str], in_tmux: bool) -> Ran {
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "cd \"$1\" && rmdir \"$1\" && shift && exec \"$@\"",
            "sh",
        ])
        .arg(dir)
        .arg(taw_bin())
        .args(args)
        .current_dir(&self.root)
        .env("PWD", &self.root);
        self.finish(cmd, in_tmux, &[])
    }

    /// The binary started in `cwd` with `args`, as [`World::run_with`] does, and left running:
    /// for a run that waits on something the test does, such as a popup that is closed.
    fn spawn(
        &self,
        cwd: &Path,
        args: &[&str],
        in_tmux: bool,
        extra: &[(&str, &str)],
    ) -> std::process::Child {
        let mut cmd = Command::new(taw_bin());
        cmd.args(args).current_dir(cwd).env("PWD", cwd);
        self.prepare(&mut cmd, in_tmux, extra);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Gives `cmd` the environment a shell would, and runs it.
    fn finish(&self, mut cmd: Command, in_tmux: bool, extra: &[(&str, &str)]) -> Ran {
        self.prepare(&mut cmd, in_tmux, extra);
        let out = cmd.output().unwrap();
        Ran {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    /// The environment of a shell: `$HOME` the scratch one, the fakes first on `$PATH`, the
    /// test server's socket and directories, and `$TMUX` set only when `in_tmux`.
    fn prepare(&self, cmd: &mut Command, in_tmux: bool, extra: &[(&str, &str)]) {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&inherited)),
        )
        .unwrap();
        cmd.env("HOME", &self.home)
            .env("PATH", path)
            .env("_ZO_DATA_DIR", self.root.join("zoxide-data"))
            .env("TAW_TMUX_SOCKET_NAME", self.server.socket())
            .env("TAW_CONFIG_DIR", self.server.config_dir())
            .env("TAW_STATE_DIR", self.server.state_dir())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_TMUX_SOCKET_PATH")
            .env_remove("TAW_TMUX_BIN");
        if in_tmux {
            // Only that it is set, and the session it names (`$0`, the first), matter: the
            // socket is the `-L` one above.
            cmd.env("TMUX", "/tmp/taw-open-test,1,0");
        }
        for (key, value) in extra {
            cmd.env(key, value);
        }
    }

    /// The envelope `taw <args>` printed, which has to be one line of JSON.
    fn envelope(&self, cwd: &Path, args: &[&str], in_tmux: bool) -> (i32, Value) {
        let ran = self.run(cwd, args, in_tmux);
        let value = serde_json::from_str(&ran.stdout)
            .unwrap_or_else(|e| panic!("not an envelope ({e}): {ran:?}"));
        (ran.code, value)
    }
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

// ------------------------------------------------------- a directory, inside tmux

/// The plain case, inside tmux: a directory that has no session gets one, this client is
/// switched to it, and nothing at all is printed.
#[test]
fn inside_tmux_a_directory_gets_a_session_and_the_client_moves_to_it() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/my-project");

    let ran = w.run(&w.root, &["open", s(&project)], true);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(ran.stdout, "", "nothing printed on success");
    assert_eq!(ran.stderr, "", "nothing printed on success");
    assert_eq!(w.client_session(), "my-project");
    assert_eq!(
        w.server.display("my-project", "#{session_path}"),
        s(&project)
    );
}

/// A second `taw open` of the same directory switches to the session that is there: it does
/// not make another, and it finds it however the directory is spelled.
#[test]
fn a_second_open_of_the_same_directory_switches_and_does_not_create() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/reopened");
    w.dir("code/reopened/sub");
    w.run(&w.root, &["open", s(&project)], true);
    let below = project.join("sub");
    for (cwd, spelling) in [
        (&project, "."),
        (&project, "sub/.."),
        (&below, ".."),
        (&below, "../../reopened"),
    ] {
        w.move_client_to("one");
        assert_eq!(w.client_session(), "one");

        let ran = w.run(cwd, &["open", spelling], true);

        assert_eq!(ran.code, 0, "{spelling}: {ran:?}");
        assert_eq!(w.client_session(), "reopened", "{spelling}: back on it");
    }
    assert_eq!(
        w.sessions().iter().filter(|n| *n == "reopened").count(),
        1,
        "{:?}",
        w.sessions()
    );
    let (_, envelope) = w.envelope(&w.root, &["open", "--json", s(&project)], true);
    assert_eq!(envelope["result"]["created"], false, "{envelope}");
}

/// Sessions are made and named as `taw session open` makes and names them: the directory's
/// name, and when that is taken by another directory, the shortest path that is not.
#[test]
fn a_session_is_named_as_session_open_names_it() {
    let mut w = World::new();
    w.attach("one");
    let work_api = w.dir("work/api");
    let oss_api = w.dir("oss/api");
    let lone = w.dir("code/lonely-project");

    let (_, opened) = w.envelope(&w.root, &["session", "open", s(&work_api)], true);
    assert_eq!(opened["result"]["session"], "api", "{opened}");
    let ran = w.run(&w.root, &["open", s(&oss_api)], true);
    let ran_alone = w.run(&w.root, &["open", s(&lone)], true);

    assert_eq!((ran.code, ran_alone.code), (0, 0), "{ran:?} {ran_alone:?}");
    assert_eq!(
        w.sessions(),
        vec!["api", "lonely-project", "one", "oss/api"],
        "the second api gets the collision suffix, the way session open gives it"
    );
    assert_eq!(w.server.display("oss/api", "#{session_path}"), s(&oss_api));
}

// ------------------------------------------------------------ which client moves

/// `taw open ~/x` typed in a pane moves the client the person typed it in. With two clients
/// on one session, tmux tells which is theirs (the one whose keys came last), and taw asks
/// it, not the first client the server lists, which is the other one when
/// the person is at the second. Two real terminals, each with a pty, so that "used last" is
/// something tmux has.
#[test]
fn with_two_clients_on_a_session_the_one_typed_in_is_the_one_that_moves() {
    let mut w = World::new();
    let first = w.terminal("one");
    let second = w.terminal("one");
    let pane = w.server.pane_of("one");
    let project = w.dir("code/two-screens");
    let mut typed = String::new();

    // The person is at the second terminal, then at the first: their keys are the last.
    for (at, elsewhere, key) in [(&second, &first, "X"), (&first, &second, "Y")] {
        w.put_on("one", &[&first, &second]);
        typed.push_str(key);
        w.type_in(at, key, &pane, &typed);

        let ran = w.run_with(
            &w.root,
            &["open", s(&project)],
            true,
            &[("TMUX_PANE", &pane)],
        );

        assert_eq!(ran.code, 0, "typed in {}: {ran:?}", at.tty);
        assert_eq!(ran.stderr, "");
        assert_eq!(
            w.terminal_session(at),
            "two-screens",
            "the client typed in ({}) did not move",
            at.tty
        );
        assert_eq!(
            w.terminal_session(elsewhere),
            "one",
            "the client not typed in ({}) moved",
            elsewhere.tty
        );
    }
}

/// The same for `--json` and for `session open`, which every script and agent uses: the
/// client that moves is the one at the pane they run in.
#[test]
fn json_and_session_open_move_the_client_typed_in_too() {
    let mut w = World::new();
    let first = w.terminal("one");
    let second = w.terminal("one");
    let pane = w.server.pane_of("one");
    let project = w.dir("code/scripted-two");
    let pane_of_the_shell = [("TMUX_PANE", pane.as_str())];
    let mut typed = String::new();

    for (args, key) in [
        (vec!["open", "--json", s(&project)], "X"),
        (vec!["session", "open", s(&project)], "Y"),
    ] {
        w.put_on("one", &[&first, &second]);
        typed.push_str(key);
        w.type_in(&second, key, &pane, &typed);

        let ran = w.run_with(&w.root, &args, true, &pane_of_the_shell);

        let envelope: Value = serde_json::from_str(&ran.stdout)
            .unwrap_or_else(|e| panic!("not an envelope ({e}): {ran:?}"));
        assert_eq!(ran.code, 0, "{args:?}: {envelope}");
        assert_eq!(envelope["result"]["switched"], true, "{args:?}: {envelope}");
        assert_eq!(w.terminal_session(&second), "scripted-two", "{args:?}");
        assert_eq!(w.terminal_session(&first), "one", "{args:?}");
    }
}

/// A pane in a session that nobody is looking at (an agent working in the background) does
/// not tell which client is the person's. Left to itself tmux would move whichever client
/// was used last anywhere on the server, which is one nobody asked to move, so taw goes on
/// as it does with no pane at all: with several clients and none on the pane's session, it
/// says it cannot tell, and moves nobody.
#[test]
fn a_pane_nobody_is_looking_at_moves_no_client_that_someone_is() {
    let mut w = World::new();
    w.server
        .tmux_ok(&["new-session", "-d", "-s", "agents", "/bin/sh"]);
    let first = w.terminal("one");
    let second = w.terminal("one");
    let pane = w.server.pane_of("agents");
    let session = w.server.display("agents", "#{session_id}");
    let tmux_var = format!("/tmp/taw-open-test,1,{}", session.trim_start_matches('$'));
    let project = w.dir("code/from-the-background");

    let ran = w.run_with(
        &w.root,
        &["open", s(&project)],
        true,
        &[("TMUX_PANE", &pane), ("TMUX", &tmux_var)],
    );

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(
        ran.stderr,
        "taw open: session from-the-background is ready, but switching to it failed: \
         cannot tell which of 2 attached clients to switch\n"
    );
    assert_eq!(w.terminal_session(&first), "one");
    assert_eq!(w.terminal_session(&second), "one");
}

// ------------------------------------------------------------- --no-switch and dir

/// `--no-switch` makes the session and moves nobody: the one client stays where it was, with
/// or without `--json`, while the same open without the flag moves that client (a lone client
/// is the one a run with no pane moves). The envelope says `switched: false` and names the
/// folder it opened.
#[test]
fn no_switch_makes_the_session_and_the_client_stays_where_it_was() {
    let mut w = World::new();
    w.attach("one");
    let plain = w.dir("code/stays-plain");
    let scripted = w.dir("code/stays-json");
    let moved = w.dir("code/moves-it");

    let ran = w.run(&w.root, &["open", "--no-switch", s(&plain)], true);

    assert_eq!(
        (ran.code, ran.stdout.as_str(), ran.stderr.as_str()),
        (0, "", ""),
        "{ran:?}"
    );
    assert_eq!(w.client_session(), "one", "the client moved");
    assert_eq!(
        w.server.display("stays-plain", "#{session_path}"),
        s(&plain)
    );

    let (code, envelope) = w.envelope(
        &w.root,
        &["open", "--json", "--no-switch", "--", s(&scripted)],
        true,
    );

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(envelope["result"]["session"], "stays-json", "{envelope}");
    assert_eq!(envelope["result"]["created"], true, "{envelope}");
    assert_eq!(envelope["result"]["switched"], false, "{envelope}");
    assert_eq!(envelope["result"]["dir"], s(&scripted), "{envelope}");
    assert_eq!(w.client_session(), "one", "the client moved");

    // Without the flag the same open still moves that client.
    let (_, envelope) = w.envelope(&w.root, &["open", "--json", s(&moved)], true);
    assert_eq!(envelope["result"]["switched"], true, "{envelope}");
    assert_eq!(w.client_session(), "moves-it");
}

/// `--no-switch` on a directory that already has its session finds that session, makes no
/// other and switches nothing.
#[test]
fn no_switch_on_a_session_that_exists_reuses_it_and_switches_nothing() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/already-there");
    w.run(&w.root, &["open", s(&project)], true);
    assert_eq!(w.client_session(), "already-there");
    w.move_client_to("one");

    let (code, envelope) = w.envelope(
        &w.root,
        &["open", "--json", "--no-switch", s(&project)],
        true,
    );

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(envelope["result"]["session"], "already-there", "{envelope}");
    assert_eq!(envelope["result"]["created"], false, "{envelope}");
    assert_eq!(envelope["result"]["switched"], false, "{envelope}");
    assert_eq!(envelope["result"]["dir"], s(&project), "{envelope}");
    assert_eq!(w.client_session(), "one", "the client moved");
    assert_eq!(
        w.sessions()
            .iter()
            .filter(|n| *n == "already-there")
            .count(),
        1,
        "{:?}",
        w.sessions()
    );
}

/// `dir` is the folder that was opened, absolute, after `~`, a relative path and a zoxide
/// term are resolved; it is there with and without `--no-switch`.
#[test]
fn json_names_the_folder_for_a_path_a_tilde_and_a_zoxide_term() {
    let mut w = World::new();
    w.attach("one");
    let near = w.dir("code/near");
    let under_home = w.dir("home/code/under-home");
    let found = w.dir("elsewhere/found-by-zoxide");
    w.zoxide_says(&[&found]);

    for (args, want, label) in [
        (
            vec!["open", "--json", s(&near)],
            s(&near),
            "an absolute path",
        ),
        (
            vec!["open", "--json", "code/near"],
            s(&near),
            "a relative path",
        ),
        (
            vec!["open", "--json", "~"],
            s(&w.home),
            "the home directory",
        ),
        (
            vec!["open", "--json", "~/code/under-home"],
            s(&under_home),
            "a path under ~",
        ),
        (vec!["open", "--json", "zed"], s(&found), "a zoxide term"),
        (
            vec!["open", "--json", "--no-switch", "zed"],
            s(&found),
            "a zoxide term, no switch",
        ),
    ] {
        let (code, envelope) = w.envelope(&w.root, &args, true);
        assert_eq!(code, 0, "{label}: {envelope}");
        assert_eq!(envelope["result"]["dir"], want, "{label}: {envelope}");
    }
}

// ----------------------------------------------------------- the home directory

/// The home directory's session is called `~`, which tmux reads as the marked pane unless it
/// is asked for by exact name: a plain `switch-client -t ~` said `no marked target`, and the
/// open failed with the session made and nobody in it.
#[test]
fn the_home_directory_opens_and_the_client_goes_to_its_session() {
    let mut w = World::new();
    w.attach("one");

    let ran = w.run(&w.root, &["open", "~"], true);

    assert_eq!(
        (ran.code, ran.stdout.as_str(), ran.stderr.as_str()),
        (0, "", ""),
        "{ran:?}"
    );
    assert_eq!(w.client_session(), "~");
    assert_eq!(w.server.display("=~:", "#{session_path}"), s(&w.home));

    // Its envelope says it went, from `taw open` and from `session open`.
    for args in [vec!["open", "--json", "~"], vec!["session", "open", "~"]] {
        w.move_client_to("one");
        let (code, envelope) = w.envelope(&w.root, &args, true);
        assert_eq!(code, 0, "{args:?}: {envelope}");
        assert_eq!(envelope["result"]["session"], "~", "{args:?}: {envelope}");
        assert_eq!(envelope["result"]["created"], false, "{args:?}: {envelope}");
        assert_eq!(envelope["result"]["switched"], true, "{args:?}: {envelope}");
        assert_eq!(w.client_session(), "~", "{args:?}");
    }
}

/// With a pane marked somewhere else, a bare `~` is that pane, and the client does not land
/// in the marked pane's session. It goes to the home directory's session, and the mark stays
/// where it was.
#[test]
fn the_home_directory_is_not_the_marked_pane() {
    let mut w = World::new();
    w.attach("one");
    w.server
        .tmux_ok(&["new-session", "-d", "-s", "elsewhere", "/bin/sh"]);
    w.server.tmux_ok(&["select-pane", "-m", "-t", "elsewhere"]);
    let marked = w.server.pane_of("elsewhere");

    for args in [
        vec!["open", "~"],
        vec!["open", "--json", "~"],
        vec!["session", "open", "~"],
    ] {
        w.move_client_to("one");

        let ran = w.run(&w.root, &args, true);

        assert_eq!(ran.code, 0, "{args:?}: {ran:?}");
        assert_eq!(w.client_session(), "~", "{args:?}");
    }
    assert_eq!(
        w.server
            .tmux_ok(&["display-message", "-p", "-t", &marked, "#{pane_marked}"]),
        "1",
        "the mark was not touched"
    );
}

// ------------------------------------------------- the ways a directory can be typed

/// `~/x` is the home directory's, and a relative path is the current directory's, read as
/// a shell reads it (`..` and `.` included).
#[test]
fn tilde_and_relative_paths_name_directories() {
    let mut w = World::new();
    w.attach("one");
    let in_home = w.dir("home/from-home");
    let sibling = w.dir("work/sibling");
    let here = w.dir("work/here");
    let below = w.dir("work/here/sub/deep-dir");

    let by_tilde = w.run(&w.root, &["open", "~/from-home"], true);
    let by_dots = w.run(&here, &["open", "../sibling"], true);
    let by_below = w.run(&here, &["open", "sub/deep-dir"], true);
    let by_dot = w.run(&here, &["open", "."], true);

    for ran in [&by_tilde, &by_dots, &by_below, &by_dot] {
        assert_eq!((ran.code, ran.stdout.as_str()), (0, ""), "{ran:?}");
    }
    let path_of = |name: &str| w.server.display(name, "#{session_path}");
    assert_eq!(path_of("from-home"), s(&in_home));
    assert_eq!(path_of("sibling"), s(&sibling));
    assert_eq!(path_of("deep-dir"), s(&below));
    assert_eq!(path_of("here"), s(&here));
}

/// A path with a space in it, typed without quotes, is the terms joined with a space.
#[test]
fn unquoted_words_can_name_a_directory_with_a_space() {
    let mut w = World::new();
    w.attach("one");
    let spaced = w.dir("work/my app");

    let ran = w.run(&w.root.join("work"), &["open", "my", "app"], true);

    assert_eq!((ran.code, ran.stdout.as_str()), (0, ""), "{ran:?}");
    assert_eq!(w.server.display("my app", "#{session_path}"), s(&spaced));
    assert_eq!(
        w.zoxide_calls().iter().filter(|c| c[0] == "query").count(),
        0
    );
}

/// A shell can be standing in a directory that was removed under it (the worktree it works
/// in was deleted). The directory it is in cannot be read then. A path from the root, or from
/// `~`, needs nothing of it and opens as ever, and so does a zoxide term, which is asked with
/// no directory to leave out; a relative path starts from it, and says so in one line, exit 1,
/// with nothing made and zoxide not asked.
#[test]
fn from_a_directory_that_was_removed_a_path_from_the_root_still_opens() {
    let mut w = World::new();
    w.attach("one");
    let gone = w.root.join("work/standing-here");
    let project = w.dir("code/from-nowhere");
    let in_home = w.dir("home/home-project");
    let found = w.dir("code/found-by-zoxide");
    w.dir("work/standing-here");

    for (typed, session) in [
        (s(&project), "from-nowhere"),
        ("~/home-project", "home-project"),
    ] {
        w.move_client_to("one");
        w.dir("work/standing-here");

        let ran = w.run_from_a_removed_directory(&gone, &["open", typed], true);

        assert_eq!(
            (ran.code, ran.stdout.as_str(), ran.stderr.as_str()),
            (0, "", ""),
            "{typed}: {ran:?}"
        );
        assert_eq!(w.client_session(), session, "{typed}");
    }
    assert_eq!(
        w.server.display("home-project", "#{session_path}"),
        s(&in_home)
    );

    let sessions = w.sessions();
    for terms in [vec!["../code/from-nowhere"], vec!["code/from-nowhere"]] {
        w.dir("work/standing-here");
        let mut args = vec!["open"];
        args.extend(terms.iter());

        let ran = w.run_from_a_removed_directory(&gone, &args, true);

        assert_eq!(ran.code, 2, "{terms:?}: {ran:?}");
        assert_eq!(ran.stdout, "");
        assert_eq!(
            ran.stderr, "taw open: the current directory cannot be read\n",
            "{terms:?}"
        );
    }
    // Under `--json` it is an envelope, with the code of an argument that cannot be used.
    w.dir("work/standing-here");
    let ran =
        w.run_from_a_removed_directory(&gone, &["open", "--json", "../code/from-nowhere"], true);
    let envelope: Value = serde_json::from_str(&ran.stdout)
        .unwrap_or_else(|e| panic!("not an envelope ({e}): {ran:?}"));
    assert_eq!(ran.code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument", "{envelope}");
    assert_eq!(
        envelope["error"]["message"],
        "the current directory cannot be read"
    );
    assert_eq!(w.sessions(), sessions, "nothing was made");
    assert!(
        w.zoxide_calls().iter().all(|call| call[0] != "query"),
        "zoxide was asked: {:?}",
        w.zoxide_calls()
    );

    // A term is zoxide's to match, asked without a directory to leave out.
    w.zoxide_says(&[&found]);
    w.move_client_to("one");
    w.dir("work/standing-here");

    let ran = w.run_from_a_removed_directory(&gone, &["open", "found", "by"], true);

    assert_eq!(
        (ran.code, ran.stdout.as_str(), ran.stderr.as_str()),
        (0, "", ""),
        "{ran:?}"
    );
    assert_eq!(w.client_session(), "found-by-zoxide");
    let query = w
        .zoxide_calls()
        .into_iter()
        .find(|call| call[0] == "query")
        .expect("zoxide was asked");
    assert_eq!(query[..], ["query", "--", "found", "by"], "{query:?}");
}

/// A path from the root or from `~` that is not there is not the current directory's fault,
/// which is gone too: it is looked up as a term is, with no directory to leave out, and said
/// to match nothing, as it would be from a directory that can be read.
#[test]
fn from_a_directory_that_was_removed_a_missing_absolute_path_is_not_blamed_on_it() {
    let mut w = World::new();
    w.attach("one");
    let gone = w.root.join("work/standing-here");
    let typos = [
        format!("{}/code/typo", w.root.display()),
        "~/typo".to_string(),
    ];

    for typed in &typos {
        w.dir("work/standing-here");

        let ran = w.run_from_a_removed_directory(&gone, &["open", typed], true);

        assert_eq!(ran.code, 1, "{typed}: {ran:?}");
        assert_eq!(ran.stdout, "");
        assert_eq!(
            ran.stderr,
            format!("taw open: no directory matches {typed}\n")
        );
    }
    let queries: Vec<Vec<String>> = w
        .zoxide_calls()
        .into_iter()
        .filter(|call| call[0] == "query")
        .collect();
    let asked: Vec<Vec<String>> = typos
        .iter()
        .map(|typed| vec!["query".to_string(), "--".to_string(), typed.clone()])
        .collect();
    assert_eq!(
        queries, asked,
        "asked of zoxide, with no directory to leave out"
    );
    assert_eq!(w.sessions(), vec!["one"], "nothing was made");
}

// ------------------------------------------------------- what zoxide finds

/// A term that is not a directory goes to zoxide, which is asked the way `z` asks it:
/// `query --exclude <where you are> -- <term>`. What it answers is opened like any
/// directory, and told to zoxide again (`add`), which promotes it.
#[test]
fn a_zoxide_term_goes_to_the_directory_zoxide_finds() {
    let mut w = World::new();
    w.attach("one");
    let cwd = w.dir("work/here");
    let found = w.dir("code/gateway");
    w.zoxide_says(&[&found]);

    let ran = w.run(&cwd, &["open", "gate"], true);

    assert_eq!(
        (ran.code, ran.stdout.as_str(), ran.stderr.as_str()),
        (0, "", ""),
        "{ran:?}"
    );
    assert_eq!(w.client_session(), "gateway");
    assert_eq!(w.server.display("gateway", "#{session_path}"), s(&found));
    assert_eq!(
        w.zoxide_calls(),
        vec![
            vec![
                "query".to_string(),
                "--exclude".into(),
                s(&cwd).into(),
                "--".into(),
                "gate".into()
            ],
            vec!["add".to_string(), s(&found).into()],
        ]
    );
}

/// Each term is a keyword of its own, as `z foo bar` has it, and the first line of the
/// answer is the directory.
#[test]
fn each_term_is_a_keyword_and_the_first_answer_wins() {
    let mut w = World::new();
    w.attach("one");
    let first = w.dir("code/first-hit");
    let second = w.dir("code/second-hit");
    w.zoxide_says(&[&first, &second]);

    let ran = w.run(&w.root, &["open", "work", "api"], true);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(w.sessions(), vec!["first-hit", "one"]);
    let query = &w.zoxide_calls()[0];
    assert_eq!(query[3..], ["--", "work", "api"], "{query:?}");
}

/// A term zoxide does not know is said so in one line, exits 1, and creates nothing.
#[test]
fn a_term_zoxide_does_not_know_says_so() {
    let mut w = World::new();
    w.attach("one");

    let ran = w.run(&w.root, &["open", "nothing", "like", "it"], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "taw open: no directory matches nothing like it\n"
    );
    assert_eq!(w.sessions(), vec!["one"]);
}

/// When the only directory that matches is the one you are in, zoxide (asked to leave that
/// one out, as `z` asks) says so, and that is said in one line: it is no other directory
/// matching, and nobody moves.
#[test]
fn when_the_only_match_is_where_you_are_it_says_so() {
    let mut w = World::new();
    w.attach("one");
    std::fs::write(&w.zoxide_answer, "only-here\n").unwrap();

    let ran = w.run(&w.root, &["open", "proj"], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.stderr, "taw open: no other directory matches proj\n");
    assert_eq!(w.sessions(), vec!["one"]);
    let (code, envelope) = w.envelope(&w.root, &["open", "--json", "proj"], true);
    assert_eq!(code, 1);
    assert_eq!(envelope["error"]["code"], "not_found", "{envelope}");
}

/// zoxide can name a directory that has gone since. That is said plainly, and no session is
/// made for it.
#[test]
fn a_directory_zoxide_names_that_is_gone_makes_no_session() {
    let mut w = World::new();
    w.attach("one");
    let gone = w.root.join("code/removed-since");
    w.zoxide_says(&[&gone]);

    let ran = w.run(&w.root, &["open", "removed"], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        format!(
            "taw open: zoxide matched {}, which is not a directory\n",
            gone.display()
        )
    );
    assert_eq!(w.sessions(), vec!["one"]);
}

/// With no zoxide installed, a term that is not a directory can only be said not to be one.
#[test]
fn without_zoxide_a_term_that_is_not_a_directory_says_so() {
    let mut w = World::new();
    w.attach("one");
    // A `$PATH` of one directory that holds tmux and nothing else: whichever of the
    // machine's own directories has zoxide in it is not on it.
    let only_tmux = w.dir("only-tmux");
    let tmux = which::which("tmux").expect("tmux is on PATH");
    std::os::unix::fs::symlink(tmux, only_tmux.join("tmux")).unwrap();

    let ran = w.run_with(
        &w.root,
        &["open", "api", "gateway"],
        true,
        &[("PATH", s(&only_tmux))],
    );

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "taw open: api gateway is not a directory, and zoxide is not installed to look it up\n"
    );
    assert_eq!(w.sessions(), vec!["one"]);
}

// ------------------------------------------------------------ nothing to open

/// With no terms and no tmux there is no popup to open: the way to use it is named, and the
/// exit code is the one for a command line that cannot be run as typed. A blank term is no
/// term.
#[test]
fn with_no_terms_and_no_tmux_it_asks_for_a_directory_or_a_term() {
    let w = World::new();
    for args in [vec!["open"], vec!["open", ""], vec!["open", "", "  "]] {
        let ran = w.run(&w.root, &args, false);
        assert_eq!(ran.code, 2, "{args:?}: {ran:?}");
        assert_eq!(ran.stdout, "");
        assert_eq!(
            ran.stderr, "taw open: name a directory or a zoxide term (the picker needs tmux)\n",
            "{args:?}"
        );
    }
    assert_eq!(w.sessions(), vec!["one"]);
}

/// With no terms inside tmux the popup is opened on the client the person is at, and with no
/// client attached there is nothing to open it on: said in one line, not as a
/// size that does not parse. The launcher the plugin's key runs says the same in its envelope.
#[test]
fn with_no_terms_and_no_client_attached_there_is_no_popup_to_open() {
    let w = World::new();

    let ran = w.run(&w.root, &["open"], true);
    let (code, envelope) = w.envelope(&w.root, &["ui", "open"], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "taw open: no attached client to open the popup on\n"
    );
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "tmux_error", "{envelope}");
    assert_eq!(
        envelope["error"]["message"],
        "no attached client to open the popup on"
    );
    assert_eq!(w.sessions(), vec!["one"]);
}

/// The popup `taw open` opens is closed from outside (`taw popup close`) while it is up. It
/// was shown and it is gone, which is a way a popup ends: `taw open` returns, exit 0, and
/// prints nothing. tmux ends `display-popup` with 129 and says nothing when that happens, and
/// that is not reported as a failure with the whole `display-popup` command line in it.
/// A real terminal, and the real popup on it.
#[test]
fn a_popup_closed_from_outside_ends_taw_open_quietly() {
    let mut w = World::new();
    let terminal = w.terminal("one");
    let pane = w.server.pane_of("one");
    let opening = w.spawn(&w.root, &["open"], true, &[("TMUX_PANE", &pane)]);

    let up = wait_for(
        || w.screen_of(&terminal).contains("new session"),
        Duration::from_secs(30),
    );
    assert!(up, "the picker never came up:\n{}", w.screen_of(&terminal));
    let closed = w.run(&w.root, &["popup", "close"], true);
    let ended = opening.wait_with_output().unwrap();

    assert_eq!(closed.code, 0, "{closed:?}");
    assert_eq!(
        (
            ended.status.code(),
            String::from_utf8_lossy(&ended.stdout).as_ref(),
            String::from_utf8_lossy(&ended.stderr).as_ref(),
        ),
        (Some(0), "", "")
    );
    assert_eq!(w.sessions(), vec!["one"]);
}

/// Outside tmux `taw open` ends by attaching this terminal, and a run with no terminal (piped,
/// from a script) has none to attach: it says so before it makes anything, and how to open a
/// session without attaching. It does not make the session and leave tmux's own `open
/// terminal failed: not a terminal` behind.
#[test]
fn outside_tmux_with_no_terminal_it_says_so_and_makes_nothing() {
    let w = World::new();
    let project = w.dir("code/nobody-to-attach");

    let ran = w.run(&w.root, &["open", s(&project)], false);

    assert_eq!(ran.code, 2, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "taw open: this is not a terminal, use --json to open without attaching\n"
    );
    assert_eq!(w.sessions(), vec!["one"], "nothing was made");
    assert!(
        w.zoxide_calls().is_empty(),
        "zoxide was told: {:?}",
        w.zoxide_calls()
    );
}

/// The popup `taw open` opens is shown on the client the person is at, and Enter on a directory
/// in its picker moves that client and no other, with two clients on the session: the
/// launcher says which client the popup is on and the popup switches that one, not
/// the first client the server lists, which is the other screen when the person is at the
/// second. Two real terminals, and the real popup on the second.
#[test]
fn the_popup_taw_open_opens_moves_the_client_it_is_shown_on() {
    let mut w = World::new();
    w.server.write_config("[sessions]\nzoxide = false\n");
    let first = w.terminal("one");
    let second = w.terminal("one");
    let pane = w.server.pane_of("one");
    let project = w.dir("code/from-the-popup");
    // The person is at the second terminal: its keys are the last of any.
    w.type_in(&second, "X", &pane, "X");

    let opening = w.spawn(&w.root, &["open"], true, &[("TMUX_PANE", &pane)]);
    w.expect_on_screen(&second, "new session");
    w.send_keys(&second, &["-l", s(&project)]);
    w.expect_on_screen(&second, "new-session -s \"from-the-popup\"");
    w.send_keys(&second, &["Enter"]);
    let ended = opening.wait_with_output().unwrap();

    assert_eq!(
        (
            ended.status.code(),
            String::from_utf8_lossy(&ended.stdout).as_ref(),
            String::from_utf8_lossy(&ended.stderr).as_ref(),
        ),
        (Some(0), "", "")
    );
    assert_eq!(
        (w.terminal_session(&first), w.terminal_session(&second)),
        ("one".to_string(), "from-the-popup".to_string()),
        "the popup did not move the client it is shown on (the second), and it must move no other"
    );
}

/// The same from the plugin's own route: a key bound to `run-shell "taw ui open"`, pressed at
/// the second terminal. The popup opens on the client that pressed the key, and its Enter moves
/// that one.
#[test]
fn the_popup_a_key_opens_moves_the_client_the_key_was_pressed_in() {
    let mut w = World::new();
    w.server.write_config("[sessions]\nzoxide = false\n");
    w.server.tmux_ok(&[
        "bind-key",
        "-n",
        "F5",
        "run-shell",
        &format!("'{}' ui open --new-session >/dev/null 2>&1", taw_bin()),
    ]);
    let first = w.terminal("one");
    let second = w.terminal("one");
    let project = w.dir("code/from-the-key");

    w.send_keys(&second, &["F5"]);
    w.expect_on_screen(&second, "new session");
    w.send_keys(&second, &["-l", s(&project)]);
    w.expect_on_screen(&second, "new-session -s \"from-the-key\"");
    w.send_keys(&second, &["Enter"]);

    // The popup closes as it moves the client, and `run-shell` returns with it.
    wait_for(
        || w.terminal_session(&first) != "one" || w.terminal_session(&second) != "one",
        Duration::from_secs(30),
    );
    assert_eq!(
        (w.terminal_session(&first), w.terminal_session(&second)),
        ("one".to_string(), "from-the-key".to_string()),
        "the popup did not move the client the key was pressed in (the second), and it must move no other"
    );
}

// ------------------------------------------------------------------- --json

/// `--json` is for scripts: the envelope `session open` prints, for the directory found, and
/// nothing else on stdout. Inside tmux it switches the client, as `session open` does.
#[test]
fn json_prints_the_envelope_session_open_prints() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/scripted");

    let (code, opened) = w.envelope(&w.root, &["open", "--json", s(&project)], true);

    assert_eq!(code, 0, "{opened}");
    assert_eq!(opened["id"], "cli:open");
    assert_eq!(opened["result"]["type"], "session_open");
    assert_eq!(
        opened["result"],
        serde_json::json!({
            "type": "session_open",
            "session": "scripted",
            "created": true,
            "collision": null,
            "switched": true,
            "dir": s(&project),
        })
    );
    assert_eq!(w.client_session(), "scripted");

    // The same directory through `session open` says the same, with its own id.
    let (_, same) = w.envelope(&w.root, &["session", "open", s(&project)], true);
    assert_eq!(same["id"], "cli:session:open");
    assert_eq!(same["result"]["session"], "scripted");
    assert_eq!(same["result"]["created"], false);
    let mut keys: Vec<_> = same["result"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| *key != "dir")
        .cloned()
        .collect();
    let mut want: Vec<_> = opened["result"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| *key != "dir")
        .cloned()
        .collect();
    keys.sort();
    want.sort();
    assert_eq!(
        keys, want,
        "the same fields, bar the folder only open names"
    );
}

/// Outside tmux `--json` opens the session and attaches nothing: a script has no terminal to
/// give it. With no client anywhere, `switched` is false, as it is for `session open`.
#[test]
fn json_outside_tmux_opens_the_session_and_attaches_nothing() {
    let w = World::new();
    let project = w.dir("code/headless");

    let (code, opened) = w.envelope(&w.root, &["open", "--json", s(&project)], false);

    assert_eq!(code, 0, "{opened}");
    assert_eq!(opened["result"]["created"], true);
    assert_eq!(opened["result"]["switched"], false);
    assert_eq!(w.sessions(), vec!["headless", "one"]);
}

/// A terminal that is not in a UTF-8 locale (`LC_ALL=C`, or no `LANG` at all) makes tmux
/// rewrite the tabs and the non-ASCII of every value a client reads back as `_`, and every
/// listing an open reads then fails to parse ("expected 7 tab-separated fields, got 1").
/// Outside tmux that is the client's own locale, so it is asked for UTF-8 whatever it is.
/// It needs a server that is already running: on a cold one the first reads come before
/// there is anything to list.
#[test]
fn outside_tmux_a_locale_that_is_not_utf8_still_opens() {
    let w = World::new();
    let project = w.dir("code/c-locale");

    let ran = w.run_with(
        &w.root,
        &["open", "--json", s(&project)],
        false,
        &[("LC_ALL", "C")],
    );

    let opened: Value = serde_json::from_str(&ran.stdout)
        .unwrap_or_else(|e| panic!("not an envelope ({e}): {ran:?}"));
    assert_eq!(ran.code, 0, "{opened}");
    assert_eq!(opened["result"]["created"], true, "{opened}");
    assert_eq!(opened["result"]["session"], "c-locale");
    assert_eq!(w.sessions(), vec!["c-locale", "one"]);
}

/// `session open` reads the same listings outside tmux, and had the same flaw.
#[test]
fn session_open_in_a_locale_that_is_not_utf8_still_opens() {
    let w = World::new();
    let project = w.dir("code/c-locale-too");

    let ran = w.run_with(
        &w.root,
        &["session", "open", s(&project)],
        false,
        &[("LC_ALL", "C")],
    );

    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(w.sessions(), vec!["c-locale-too", "one"]);
}

/// Failures under `--json` are envelopes too, with a code and exit 1, like every other
/// command: what was named is `not_found`.
#[test]
fn json_failures_are_envelopes() {
    let w = World::new();

    let (code, lookup) = w.envelope(&w.root, &["open", "--json", "no", "such", "thing"], false);
    assert_eq!(code, 1);
    assert_eq!(lookup["id"], "cli:open");
    assert_eq!(lookup["error"]["code"], "not_found", "{lookup}");
    assert_eq!(
        lookup["error"]["message"],
        "no directory matches no such thing"
    );

    let (code, bare) = w.envelope(&w.root, &["open", "--json"], false);
    assert_eq!(code, 1);
    assert_eq!(bare["error"]["code"], "invalid_argument", "{bare}");
    assert_eq!(
        bare["error"]["message"],
        "--json needs a directory or a zoxide term"
    );
}

// ------------------------------------------------------ the same open as session open

/// The create hooks are `session open`'s: the post hook runs once for a session that was
/// made and not for one that is found, and it is told the name and the directory.
#[test]
fn the_create_hooks_run_as_they_do_for_session_open() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/hooked");
    let log = w.root.join("post.log");
    let hook = w.root.join("post.sh");
    common::write_script(
        &hook,
        &format!("printf '%s %s\\n' \"$1\" \"$2\" >> '{}'\n", log.display()),
    );
    w.server.write_config(&format!(
        "[sessions]\npost_create_hook = \"{}\"\n",
        hook.display()
    ));

    w.run(&w.root, &["open", s(&project)], true);
    w.run(&w.root, &["open", s(&project)], true);

    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("hooked {}\n", project.display()),
        "once, for the session that was made"
    );
}

/// A pre-create hook that refuses stops the open, and it is said in one line that names the
/// hook and what it said; under `--json` it has its own error code.
#[test]
fn a_pre_create_hook_that_refuses_stops_the_open_in_one_line() {
    let mut w = World::new();
    w.attach("one");
    let project = w.dir("code/refused");
    let hook = w.root.join("pre.sh");
    common::write_script(&hook, "echo 'not today' >&2\nexit 3\n");
    w.server.write_config(&format!(
        "[sessions]\npre_create_hook = \"{}\"\n",
        hook.display()
    ));

    let ran = w.run(&w.root, &["open", s(&project)], true);
    let (code, envelope) = w.envelope(&w.root, &["open", "--json", s(&project)], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.stderr, "taw open: pre-create hook: not today\n");
    assert_eq!(code, 1);
    assert_eq!(envelope["error"]["code"], "hook_refused", "{envelope}");
    assert_eq!(w.sessions(), vec!["one"], "nothing was made");
}

/// The session is there, but nobody was switched to it: with no client attached, that is
/// said, and it is a failure; the session stays.
#[test]
fn a_switch_with_no_client_to_switch_is_said_so() {
    let w = World::new();
    let project = w.dir("code/unwatched");

    let ran = w.run(&w.root, &["open", s(&project)], true);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "taw open: session unwatched is ready, but switching to it failed: no attached client to switch\n"
    );
    assert_eq!(w.sessions(), vec!["one", "unwatched"]);
}

// ------------------------------------------------------------- a first open

/// `taw open` from a plain terminal, with tmux not running at all, is what starts it: the
/// session is made on a fresh server, which the open leaves running. (Through `--json`,
/// since attaching a terminal is not something a test can do.)
#[test]
fn with_no_tmux_server_running_an_open_starts_one() {
    let w = World::new();
    let project = w.dir("code/first-of-the-day");
    w.server.tmux_ok(&["kill-server"]);
    assert!(
        w.server.tmux(&["list-sessions"]).is_err(),
        "the server is gone before the open"
    );

    let quiet = w.bin.join("tmux-quiet");
    let ran = w.run_with(
        &w.root,
        &["open", "--json", s(&project)],
        false,
        &[("TAW_TMUX_BIN", s(&quiet))],
    );

    assert_eq!(ran.code, 0, "{ran:?}");
    let envelope: Value = serde_json::from_str(&ran.stdout).unwrap();
    assert_eq!(envelope["result"]["created"], true, "{envelope}");
    assert_eq!(envelope["result"]["session"], "first-of-the-day");
    assert_eq!(w.sessions(), vec!["first-of-the-day"]);
}
