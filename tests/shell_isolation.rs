//! No test touches the shell history or the shell startup files of whoever runs it.
//!
//! A shell that one of the throwaway tmux servers runs saves what it was typed in `$HISTFILE` when
//! its server is killed, which is `~/.bash_history` where nothing says otherwise, and a shell reads
//! its startup files from the home it has: the runner's own, where nothing says otherwise. So every
//! server a test or a harness starts has `HISTFILE=/dev/null` and a `HOME` and a `ZDOTDIR` of its own
//! in the environment it is started with (`tests/common/shell_env.rs`), and so has the first pane,
//! which a `set-environment` sent after the start cannot reach.
//!
//! The first half of this file starts the ways the suite has of starting a server and reads what the
//! first pane of each has: the values of `HISTFILE` and `HOME`, and whether `ZDOTDIR` is `HOME`,
//! and nothing else of its environment, which holds secrets. The second half reads the sources, for
//! a server that some file starts in a way of its own, and tests that reading on lines that are
//! written to be wrong.

mod clients;
mod common;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use common::harness::harness_up;
use common::shell_env::{
    PINNED, RECORDER_LOG_VARIABLES, SSH_REFUSAL, SSH_SEAMS, ShellHome, is_a_test_socket,
    take_the_socket_away,
};
use common::{Server, ServerOptions, repo_root, wait_for};
use regex::Regex;
use taw::tmux::{Socket, Tmux};

/// What a shell has: the two values asked for, and whether its `$ZDOTDIR` is its `$HOME`.
#[derive(Debug)]
struct Vars {
    histfile: String,
    home: String,
    zdotdir_is_home: bool,
}

/// Types into the shell of `target`, through `tmux`, a command that writes its `HISTFILE` and `HOME`
/// to `file` and says whether `ZDOTDIR` is `HOME`, and reads that back. The shell is the one the pane
/// has run since it began, so this is what that process has and not what tmux would give a new one.
fn typed_read(tmux: &dyn Fn(&[&str]) -> Result<String, String>, target: &str, file: &Path) -> Vars {
    let command = format!(
        "{{ printf '%s\\n%s\\n' \"$HISTFILE\" \"$HOME\"; \
         if [ \"$ZDOTDIR\" = \"$HOME\" ]; then echo same; else echo differs; fi; }} \
         > '{0}.part' && mv '{0}.part' '{0}'",
        file.display()
    );
    // A pane that a `run-shell` has written to is in view mode, which takes the keys for itself.
    // Leaving a mode a pane is not in is an error, and no matter.
    let _ = tmux(&["send-keys", "-t", target, "-X", "cancel"]);
    tmux(&["send-keys", "-t", target, "-l", &command]).unwrap();
    tmux(&["send-keys", "-t", target, "Enter"]).unwrap();
    assert!(
        wait_for(|| file.exists(), Duration::from_secs(30)),
        "the shell of {target} never ran what was typed to it"
    );
    let text = std::fs::read_to_string(file).unwrap();
    let mut lines = text.lines().map(str::to_string);
    Vars {
        histfile: lines.next().unwrap_or_default(),
        home: lines.next().unwrap_or_default(),
        zdotdir_is_home: lines.next().as_deref() == Some("same"),
    }
}

/// The home of whoever runs the tests, when they have one.
fn runner_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok())
}

/// Whether `home` is a directory that is not the runner's own.
fn assert_a_home_of_its_own(home: &str, what: &str) {
    assert!(
        !home.is_empty() && Path::new(home).is_dir(),
        "{what}: $HOME is {home:?}, which is no directory"
    );
    assert_ne!(
        std::fs::canonicalize(home).ok(),
        runner_home(),
        "{what}: $HOME is the home of whoever runs the tests"
    );
}

fn assert_isolated(vars: &Vars, what: &str) {
    assert_eq!(
        vars.histfile, "/dev/null",
        "{what}: a shell saves what it was typed to $HISTFILE"
    );
    assert_a_home_of_its_own(&vars.home, what);
    assert!(
        vars.zdotdir_is_home,
        "{what}: a zsh reads its startup files from $ZDOTDIR, which is not $HOME"
    );
}

/// `t.run` with the error as the text `typed_read` takes.
fn through(t: &Tmux) -> impl Fn(&[&str]) -> Result<String, String> + '_ {
    move |args| t.run(args).map_err(|err| format!("{err:#}"))
}

fn scratch_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

/// What `tmux show-environment -g <name>` says the server's global environment holds for `name`,
/// `None` where it holds none: the one variable asked for, never the environment. A server's global
/// environment is the one its panes are made from.
fn global_value(t: &Tmux, name: &str) -> Option<String> {
    let line = t.run(&["show-environment", "-g", name]).ok()?;
    line.strip_prefix(&format!("{name}=")).map(str::to_string)
}

/// The same check as [`assert_isolated`], for a server that nothing is typed into (the one a
/// client's terminal comes from, whose pane runs the attach), read off its environment.
fn assert_server_isolated(t: &Tmux, what: &str) {
    assert_eq!(
        global_value(t, "HISTFILE").as_deref(),
        Some("/dev/null"),
        "{what}: a shell in this server saves what it was typed to $HISTFILE"
    );
    let home = global_value(t, "HOME").unwrap_or_default();
    assert_a_home_of_its_own(&home, what);
    assert!(
        global_value(t, "ZDOTDIR").as_deref() == Some(home.as_str()),
        "{what}: a zsh reads its startup files from $ZDOTDIR, which is not $HOME"
    );
}

#[test]
fn the_first_pane_of_a_shared_server_has_a_home_of_its_own_and_no_history_file() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();

    let vars = typed_read(&|a| server.tmux(a), "one:0.0", &scratch_file(&dir, "first"));

    assert_isolated(&vars, "the first pane of Server::start()");
}

#[test]
fn the_panes_a_shared_server_makes_later_have_the_same() {
    let server = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let new_window = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]);
    let split = server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        "one:0.0",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]);
    let other = server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "two",
        "-P",
        "-F",
        "#{pane_id}",
        "/bin/sh",
    ]);

    for (what, pane) in [
        ("a new window", new_window),
        ("a split", split),
        ("a new session", other),
    ] {
        let vars = typed_read(&|a| server.tmux(a), &pane, &scratch_file(&dir, &pane));
        assert_isolated(&vars, what);
    }
}

#[test]
fn each_shared_server_has_a_home_of_its_own() {
    let (one, two) = (Server::start(), Server::start());
    let dir = tempfile::tempdir().unwrap();

    let first = typed_read(&|a| one.tmux(a), "one:0.0", &scratch_file(&dir, "one"));
    let second = typed_read(&|a| two.tmux(a), "one:0.0", &scratch_file(&dir, "two"));

    assert_ne!(
        first.home, second.home,
        "two servers share a home, and so a history"
    );
}

/// A test that names the home its shells have wins over the default, and `HISTFILE` stays: several
/// tests of the agent verbs do (`ServerOptions::env`), and the `export PATH=...; exec /bin/sh` of a
/// `path_prefix` is another way for the first pane to begin.
#[test]
fn a_server_with_options_of_its_own_keeps_no_history_file() {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin.path().to_path_buf()),
        env: vec![("HOME".to_string(), home.path().display().to_string())],
        ..ServerOptions::default()
    });
    let dir = tempfile::tempdir().unwrap();

    let vars = typed_read(&|a| server.tmux(a), "one:0.0", &scratch_file(&dir, "first"));

    assert_eq!(vars.histfile, "/dev/null", "the first pane's $HISTFILE");
    assert_eq!(
        vars.home,
        home.path().display().to_string(),
        "a home a test names is the one its shells have"
    );
}

/// A server a file starts for itself, through `ShellHome::start_server`.
struct Throwaway {
    tmux: Tmux,
    home: ShellHome,
}

impl Throwaway {
    fn start() -> Throwaway {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmux =
            Tmux::with_socket_name(&format!("taw-test-shell-env-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        home.start_server(
            &tmux,
            &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"],
        );
        Throwaway { tmux, home }
    }
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.tmux.run_ok(&["kill-server"]);
    }
}

#[test]
fn a_server_started_through_the_helper_has_them_in_its_first_pane() {
    let server = Throwaway::start();
    let dir = tempfile::tempdir().unwrap();

    let vars = typed_read(
        &through(&server.tmux),
        "one:0.0",
        &scratch_file(&dir, "first"),
    );

    assert_isolated(&vars, "the first pane of ShellHome::start_server");
    assert_eq!(
        vars.home,
        server.home.path().display().to_string(),
        "the home the helper made"
    );
}

/// A server started the way a test file could start one: the environment of whoever runs
/// the tests, whatever it is, but for a history file. Its pane runs `sleep` and no shell, so nothing
/// here reads or saves the history of whoever runs the tests either.
///
/// `HISTFILE` is taken out of it: a runner that has one of `/dev/null` (a probe should) would hand
/// the server the very value the check looks for, and the check would pass for the runner's reason.
struct OldWay(Tmux, PathBuf);

impl OldWay {
    fn start() -> OldWay {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmux =
            Tmux::with_socket_name(&format!("taw-test-shell-old-{}-{n}", std::process::id()));
        let started = tmux
            .command()
            .args(["-u", "-f", "/dev/null", "new-session", "-d", "-s", "one"])
            .args(["-x", "80", "-y", "24", "sleep", "600"])
            .env_remove("HISTFILE")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .status()
            .unwrap();
        assert!(started.success(), "the server did not start");
        let file = PathBuf::from(
            tmux.run(&["display-message", "-p", "#{socket_path}"])
                .unwrap(),
        );
        OldWay(tmux, file)
    }
}

impl Drop for OldWay {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
        take_the_socket_away(&self.0, &self.1);
    }
}

/// The check can fail: a server started that way has the history file and the home of whoever
/// runs the tests, and is refused for the history file first.
#[test]
#[should_panic(expected = "a shell in this server saves what it was typed to $HISTFILE")]
fn a_server_started_the_old_way_is_found_out() {
    let server = OldWay::start();

    assert_server_isolated(
        &server.0,
        "a server started with the environment it was run in",
    );
}

/// The server that gives a client its terminal runs the attach with its own shell, and a shell
/// reads its startup files whatever it is run for.
#[test]
fn the_server_that_gives_a_client_a_terminal_has_them_too() {
    let server = Server::start();
    let inner = Tmux::with_socket_name(server.socket());
    let client = clients::Client::attach(&inner, "one");

    assert_server_isolated(client.outer(), "the terminal server of a client");
}

#[test]
fn the_shell_harness_starts_both_its_servers_with_them() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = harness_up(tmp.path(), &[]);
    let dir = tempfile::tempdir().unwrap();

    let vars = typed_read(
        &through(&harness.inner),
        "main:0.0",
        &scratch_file(&dir, "main"),
    );
    assert_isolated(&vars, "main:0 of the harness");

    assert_server_isolated(&harness.outer, "the outer server of the harness");
}

// ------------------------------------------------------------------ the runner's own files

/// The variables the product finds a directory by (`env_dir("...")` and `xdg_env_dir("...")` in
/// `src/`), read off its sources, so that one an integration starts to read is known here at once.
fn directory_variables_of_the_product() -> Vec<String> {
    static READ: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\b(?:xdg_)?env_dir\("([A-Z_][A-Z0-9_]*)"\)"#).unwrap());
    let mut names: Vec<String> = sources()
        .iter()
        .filter(|(path, _)| path.starts_with("src/"))
        .flat_map(|(_, text)| {
            READ.captures_iter(text)
                .map(|found| found[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort();
    names.dedup();
    assert!(
        names.len() >= 12,
        "the sources name {} directory variables, where the product reads more than a dozen",
        names.len()
    );
    names
}

#[test]
fn every_directory_the_product_reads_from_the_environment_is_pinned() {
    let missing: Vec<String> = directory_variables_of_the_product()
        .into_iter()
        .filter(|name| !PINNED.iter().any(|(pinned, _)| pinned == name))
        .collect();

    assert!(
        missing.is_empty(),
        "the product finds a directory by each of these, and a test server would have the runner's: \
         pin them in shell_env::PINNED: {missing:?}"
    );
}

/// The variables of [`PINNED`] that the server `t` does not have where its home `home` is: a value
/// inside the home, or an empty one for a variable pinned empty. Names only: the value of one that
/// is not pinned would be the runner's.
fn unpinned(t: &Tmux, home: &Path) -> Vec<&'static str> {
    PINNED
        .iter()
        .filter(|(name, place)| {
            let value = global_value(t, name);
            let pinned = if place.is_empty() {
                value.as_deref() == Some("")
            } else {
                value.is_some_and(|value| Path::new(&value).starts_with(home))
            };
            !pinned
        })
        .map(|(name, _)| *name)
        .collect()
}

/// A server of each helper has them all, inside its own home: whatever the runner has them set to
/// (or has not set at all), nothing the server runs, and nothing the helper runs, reads or writes
/// the runner's own files. The shell harness's two servers have them inside the run's directory (the
/// shell home, and zoxide's beside it).
#[test]
fn each_helper_pins_the_directories_of_the_runners_own_files_inside_its_home() {
    let shared = Server::start();
    let throwaway = Throwaway::start();
    let tmp = tempfile::tempdir().unwrap();
    let harness = harness_up(tmp.path(), &[]);

    for (what, tmux, home) in [
        (
            "common::Server",
            Tmux::with_socket_name(shared.socket()),
            shared.home_dir(),
        ),
        (
            "a server started by ShellHome::start_server",
            throwaway.tmux.clone(),
            throwaway.home.path(),
        ),
        (
            "the inner server of the harness",
            harness.inner.clone(),
            harness.dir.as_path(),
        ),
        (
            "the outer server of the harness",
            harness.outer.clone(),
            harness.dir.as_path(),
        ),
    ] {
        let unpinned = unpinned(&tmux, home);
        assert!(
            unpinned.is_empty(),
            "{what}: the server has these of the runner's, or none, where its own home should be: \
             {unpinned:?}"
        );
    }
}

// ------------------------------------------------------------------ ssh

/// A server of each helper has `$TAW_SSH_BIN` naming a recorder that refuses: what taw runs as ssh in
/// anything the server starts (a hook, a watcher, a pane's `taw`) is never a real ssh, whatever the
/// runner has the variable set to. The recorder is run here, with a log of this test's own, and notes
/// its arguments and exits as an ssh that cannot connect does (255, with one line on standard error).
/// The shell harness's two servers (tests/e2e/harness.sh) are not here: its sections run no machine, and
/// a bare `ssh` there is the net's stub (`e2e_up` makes the net).
#[test]
fn each_helper_gives_its_servers_a_refusing_recorder_for_ssh() {
    let shared = Server::start();
    let throwaway = Throwaway::start();
    let tmp = tempfile::tempdir().unwrap();
    let mut n = 0;
    for (what, tmux) in [
        ("common::Server", Tmux::with_socket_name(shared.socket())),
        (
            "a server started by ShellHome::start_server",
            throwaway.tmux.clone(),
        ),
    ] {
        for name in SSH_SEAMS {
            let bin = global_value(&tmux, name)
                .unwrap_or_else(|| panic!("{what}: the server has no {name}"));
            assert!(
                Path::new(&bin).is_absolute() && Path::new(&bin).is_file(),
                "{what}: {name} names {bin:?}, which is not a recorder of the helper's"
            );
            n += 1;
            let log = tmp.path().join(format!("ssh-log-{n}"));
            let out = std::process::Command::new(&bin)
                .args(["-o", "BatchMode=yes", "studio.local"])
                .env("TAW_SSH_NET_LOG", &log)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(255), "{what}");
            assert_eq!(
                String::from_utf8_lossy(&out.stderr),
                format!("{SSH_REFUSAL}\n"),
                "{what}"
            );
            assert_eq!(
                std::fs::read_to_string(&log).unwrap(),
                format!("{name}\t-o BatchMode=yes studio.local\n"),
                "{what}"
            );
        }
    }
}

#[test]
fn a_taw_that_reaches_for_ssh_from_a_plain_server_reaches_the_refusing_recorder() {
    let server = Server::start();
    server.write_config("[machines]\nstudio = \"studio.local\"\n");
    assert_eq!(server.ssh_calls().len(), 0);

    let (code, envelope) = server.taw(&["--machine", "studio", "workspace", "list"]);

    // The recorder exits 255 as ssh does when it cannot connect, and it noted the call: once.
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["id"], "cli:machine");
    assert_eq!(envelope["error"]["code"], "machine_unreachable");
    assert_eq!(
        envelope["error"]["message"],
        "studio: ssh: refused by the test seam"
    );
    let calls = server.ssh_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("TAW_SSH_BIN\t"), "{calls:?}");
    assert!(calls[0].contains(" studio.local "), "{calls:?}");
}

/// A runner that exports `$TAW_SSH_NET_LOG` does not move the recorder's log: the fixture takes it out of
/// every command it starts, so the call is still noted where the fixture looks. The test above is run by
/// this test binary, as it would be run by such a runner.
#[test]
fn an_exported_ssh_log_variable_does_not_move_the_recorders_log() {
    let tmp = tempfile::tempdir().unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    let run = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "a_taw_that_reaches_for_ssh_from_a_plain_server_reaches_the_refusing_recorder",
            "--exact",
            "--test-threads=1",
        ])
        .env("TAW_SSH_NET_LOG", &elsewhere)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    // The positive half: the test did run, and found its call in the log it looks in.
    assert!(
        run.status.success() && said.contains("1 passed"),
        "the inner test did not pass:\n{said}"
    );
    assert!(
        !elsewhere.exists(),
        "the recorder wrote to the runner's log"
    );
}

/// The recorders of a home that has no server of the fixture's (`ShellHome::apply`, which a test that
/// starts its own server through the home uses) are not moved by a runner's log variables either: each
/// recorder, the ssh one and the desktop ones, notes its calls in the file such a variable names.
#[test]
fn a_homes_start_environment_takes_out_the_variables_that_move_a_recorders_log() {
    // The list holds the log variable of each kind of recorder (scripts/ssh-net-names.txt and the
    // desktop net's stubs), so that the loop below is not over nothing.
    assert_eq!(
        RECORDER_LOG_VARIABLES,
        ["TAW_SSH_NET_LOG", "TAW_DESKTOP_NET_LOG"]
    );
    let home = ShellHome::new();
    let mut command = std::process::Command::new("true");
    home.apply(&mut command);
    for name in RECORDER_LOG_VARIABLES {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()),
            "the home's start environment does not take {name} out"
        );
    }
}

// ------------------------------------------------------------------ the sockets

/// The file name of the socket the server `t` reaches says it listens on.
fn socket_name_of(t: &Tmux) -> String {
    let path = t
        .run(&["display-message", "-p", "#{socket_path}"])
        .unwrap_or_else(|err| panic!("the server does not say its socket: {err:#}"));
    Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn assert_a_test_socket(name: &str, what: &str) {
    assert!(
        is_a_test_socket(name),
        "{what}: the server listens on {name:?}, which is not a test server's socket (taw-...): it \
         could be the default server of whoever runs the tests"
    );
}

/// Nothing the helpers start can be the server of whoever runs the tests: each is reached by a
/// socket of its own (`-L taw-...`), which the server itself says it listens on, and none is the
/// `default` one.
#[test]
fn every_server_the_helpers_start_listens_on_a_socket_of_its_own() {
    let shared = Server::start();
    let throwaway = Throwaway::start();
    let terminal_of = Server::start();
    let client = clients::Client::attach(&Tmux::with_socket_name(terminal_of.socket()), "one");
    let tmp = tempfile::tempdir().unwrap();
    let harness = harness_up(tmp.path(), &[]);

    assert_a_test_socket(
        &socket_name_of(&Tmux::with_socket_name(shared.socket())),
        "common::Server",
    );
    assert_a_test_socket(
        &socket_name_of(&throwaway.tmux),
        "a server started by ShellHome::start_server",
    );
    assert_a_test_socket(
        &socket_name_of(client.outer()),
        "the second server of a Client",
    );
    assert_a_test_socket(
        &socket_name_of(&harness.inner),
        "the inner server of the harness",
    );
    assert_a_test_socket(
        &socket_name_of(&harness.outer),
        "the outer server of the harness",
    );
}

/// The helper runs nothing against a tmux that has no socket (which would be the server in `$TMUX`,
/// else the default one), or the socket called `default`. The binary named here does not exist, so
/// nothing could run whatever happened.
#[test]
#[should_panic(expected = "neither -L nor -S")]
fn the_helper_refuses_a_tmux_with_no_socket() {
    let bare = Tmux {
        bin: PathBuf::from("/nonexistent/tmux"),
        socket: None,
    };

    let _ = ShellHome::new().run(&bare, &["list-sessions"]);
}

#[test]
#[should_panic(expected = "is not a test server's")]
fn the_helper_refuses_the_socket_called_default() {
    let default = Tmux {
        bin: PathBuf::from("/nonexistent/tmux"),
        socket: Some(Socket::Name("default".to_string())),
    };

    let _ = ShellHome::new().run(&default, &["list-sessions"]);
}

// ------------------------------------------------------------------ the sources

/// Every file that may start a tmux server, by its path from the repository root and its text, but
/// this one (which holds the lines it tests the reading on): the Rust, the shell and the bats files
/// of the tests, the scripts, the library's own unit tests and the examples.
fn sources() -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, files);
                continue;
            }
            let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(extension, "rs" | "sh" | "bats") {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.push((relative, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["tests", "scripts", "src", "examples"] {
        walk(&root, &root.join(dir), &mut files);
    }
    files.retain(|(path, _)| path != "tests/shell_isolation.rs");
    assert!(
        files.len() > 150,
        "the scan found {} files, where the repository has more than 150",
        files.len()
    );
    files
}

/// Whether `line` is a comment of a Rust or a shell file.
fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("//") || line.starts_with('#')
}

/// `line` without a `//` comment at its end, for a line of Rust: the comment starts at a `//` that is
/// not inside a string (an even number of quotes before it).
fn without_comment(line: &str) -> &str {
    let mut quotes = 0;
    let bytes = line.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' if i == 0 || bytes[i - 1] != b'\\' => quotes += 1,
            b'/' if quotes % 2 == 0 && bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
    }
    line
}

// ------------------------------------------------------------------ reading a shell line

/// One word of a shell command line, as the shell splits it: what was written (`raw`), and what it
/// says once its quotes and backslashes are taken off (`value`).
#[derive(Debug)]
struct Word {
    raw: String,
    value: String,
}

/// What a shell line is made of, as far as finding the commands on it goes.
#[derive(Debug)]
enum Token {
    Word(Word),
    /// `;` `&` `&&` `|` `||` `(` `)`: where a command ends, and the next one starts.
    Separator,
    /// `<` `>` `>>` `2>&1` `<<` and the like: the word after it is where the redirection goes.
    Redirect,
}

/// The index of the `)` that ends the `$(` whose body starts at `start`, or the end of the line.
fn substitution_end(chars: &[char], start: usize) -> usize {
    let (mut depth, mut i) = (1, start);
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// The index of the backtick that ends the one before `start`, or the end of the line.
fn backtick_end(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() && chars[i] != '`' {
        i += if chars[i] == '\\' { 2 } else { 1 };
    }
    i.min(chars.len())
}

/// A shell line split the way the shell splits it, and the command lines inside it: the body of each
/// `$(...)` and of each pair of backticks, which the shell runs wherever it stands (in a word, in an
/// assignment, in double quotes, but not in single quotes, where nothing is special). A `#` that
/// starts a word starts a comment.
fn shell_split(line: &str) -> (Vec<Token>, Vec<String>) {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut inner = Vec::new();
    let (mut raw, mut value, mut in_word) = (String::new(), String::new(), false);
    let end_word =
        |tokens: &mut Vec<Token>, raw: &mut String, value: &mut String, in_word: &mut bool| {
            if *in_word {
                tokens.push(Token::Word(Word {
                    raw: std::mem::take(raw),
                    value: std::mem::take(value),
                }));
                *in_word = false;
            }
        };
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            ' ' | '\t' => {
                end_word(&mut tokens, &mut raw, &mut value, &mut in_word);
                i += 1;
            }
            '#' if !in_word => break,
            '\'' => {
                in_word = true;
                let close = chars[i + 1..]
                    .iter()
                    .position(|&d| d == '\'')
                    .map_or(n, |p| i + 1 + p);
                raw.extend(&chars[i..(close + 1).min(n)]);
                value.extend(&chars[i + 1..close]);
                i = close + 1;
            }
            '"' => {
                in_word = true;
                raw.push('"');
                i += 1;
                while i < n && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < n => {
                            raw.extend(&chars[i..i + 2]);
                            value.push(chars[i + 1]);
                            i += 2;
                        }
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            let close = substitution_end(&chars, i + 2);
                            inner.push(chars[i + 2..close].iter().collect());
                            raw.extend(&chars[i..(close + 1).min(n)]);
                            i = close + 1;
                        }
                        '`' => {
                            let close = backtick_end(&chars, i + 1);
                            inner.push(chars[i + 1..close].iter().collect());
                            raw.extend(&chars[i..(close + 1).min(n)]);
                            i = close + 1;
                        }
                        d => {
                            raw.push(d);
                            value.push(d);
                            i += 1;
                        }
                    }
                }
                if i < n {
                    raw.push('"');
                    i += 1;
                }
            }
            '\\' => {
                in_word = true;
                raw.extend(&chars[i..(i + 2).min(n)]);
                value.extend(&chars[(i + 1).min(n)..(i + 2).min(n)]);
                i += 2;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                in_word = true;
                let close = substitution_end(&chars, i + 2);
                inner.push(chars[i + 2..close].iter().collect());
                raw.extend(&chars[i..(close + 1).min(n)]);
                i = close + 1;
            }
            '`' => {
                in_word = true;
                let close = backtick_end(&chars, i + 1);
                inner.push(chars[i + 1..close].iter().collect());
                raw.extend(&chars[i..(close + 1).min(n)]);
                i = close + 1;
            }
            '&' if chars.get(i + 1) == Some(&'>') => {
                end_word(&mut tokens, &mut raw, &mut value, &mut in_word);
                tokens.push(Token::Redirect);
                i += 2;
                while i < n && chars[i] == '>' {
                    i += 1;
                }
            }
            ';' | '&' | '|' | '(' | ')' => {
                end_word(&mut tokens, &mut raw, &mut value, &mut in_word);
                tokens.push(Token::Separator);
                i += 1;
                // `&&`, `||`, `;;` and `|&` are one separator each.
                if i < n
                    && ((chars[i] == c && matches!(c, '&' | '|' | ';'))
                        || (c == '|' && chars[i] == '&'))
                {
                    i += 1;
                }
            }
            '<' | '>' => {
                // A word of digits right before is the descriptor the redirection is of: no word.
                if in_word && !raw.is_empty() && raw.chars().all(|d| d.is_ascii_digit()) {
                    raw.clear();
                    value.clear();
                    in_word = false;
                }
                end_word(&mut tokens, &mut raw, &mut value, &mut in_word);
                tokens.push(Token::Redirect);
                i += 1;
                while i < n && matches!(chars[i], '<' | '>' | '&' | '-' | '|') {
                    i += 1;
                }
            }
            d => {
                in_word = true;
                raw.push(d);
                value.push(d);
                i += 1;
            }
        }
    }
    end_word(&mut tokens, &mut raw, &mut value, &mut in_word);
    (tokens, inner)
}

/// Whether `raw` is an assignment: `NAME=value`, which a command can start with.
fn is_assignment(raw: &str) -> bool {
    static ASSIGNMENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=").unwrap());
    ASSIGNMENT.is_match(raw)
}

/// The name of a variable an assignment sets.
fn assigned_name(raw: &str) -> String {
    raw.split(['=', '[', '+']).next().unwrap_or("").to_string()
}

/// The shell's own words that a command can follow, and that run nothing themselves.
const RESERVED: [&str; 15] = [
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "case", "esac", "!",
    "{", "}",
];

/// A simple command of a shell line: the variables assigned in front of it, and its words, from the
/// one that names what it runs (a redirection and where it goes are no words of it).
struct Command<'a> {
    assigned: Vec<&'a Word>,
    words: Vec<&'a Word>,
}

fn simple_commands(tokens: &[Token]) -> Vec<Command<'_>> {
    let mut commands = Vec::new();
    let mut current = Command {
        assigned: Vec::new(),
        words: Vec::new(),
    };
    let mut redirect_target = false;
    for token in tokens {
        match token {
            Token::Separator => {
                redirect_target = false;
                let done = std::mem::replace(
                    &mut current,
                    Command {
                        assigned: Vec::new(),
                        words: Vec::new(),
                    },
                );
                if !done.words.is_empty() {
                    commands.push(done);
                }
            }
            Token::Redirect => redirect_target = true,
            Token::Word(word) => {
                if std::mem::take(&mut redirect_target) {
                    continue;
                }
                if current.words.is_empty() {
                    if RESERVED.contains(&word.value.as_str()) {
                        continue;
                    }
                    if is_assignment(&word.raw) {
                        current.assigned.push(word);
                        continue;
                    }
                }
                current.words.push(word);
            }
        }
    }
    if !current.words.is_empty() {
        commands.push(current);
    }
    commands
}

/// The last part of a path: `tmux` for `/usr/bin/tmux`.
fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

/// An array given to `env` whole (`env "${pinned[@]}" tmux ...`): the assignments a script keeps in
/// one place for more than one command.
static ARRAY_OF_ASSIGNMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^"\$\{[A-Za-z_][A-Za-z0-9_]*\[@\]\}"$"#).unwrap());

/// Where the program a command runs is among its words, past the commands that run another program
/// (`env`, `timeout`, `nohup`, `nice`, `xargs`, `command`, `exec`, `time`, `builtin`, and the `run` of
/// bats) with their options, and the variables `env` sets on the way, which are returned too. `None`
/// for `command -v` and `command -V`, which ask where a program is and run nothing.
fn program_of<'a>(words: &[&'a Word]) -> Option<(usize, Vec<&'a Word>)> {
    let mut assigned = Vec::new();
    let mut i = 0;
    let skip_options = |i: &mut usize, with_argument: &[&str]| {
        while let Some(word) = words.get(*i) {
            if !word.value.starts_with('-') || word.value == "-" {
                break;
            }
            *i += 1;
            if word.value == "--" {
                break;
            }
            if with_argument.contains(&word.value.as_str()) {
                *i += 1;
            }
        }
    };
    loop {
        let word = words.get(i)?;
        match basename(&word.value) {
            "env" => {
                i += 1;
                while let Some(next) = words.get(i) {
                    if is_assignment(&next.raw) {
                        assigned.push(*next);
                        i += 1;
                    } else if ARRAY_OF_ASSIGNMENTS.is_match(&next.raw) {
                        i += 1;
                    } else if next.value.starts_with('-') && next.value != "-" {
                        i += if ["-u", "-C", "-P", "-S"].contains(&next.value.as_str()) {
                            2
                        } else {
                            1
                        };
                    } else {
                        break;
                    }
                }
            }
            "timeout" => {
                i += 1;
                skip_options(&mut i, &["-s", "-k"]);
                i += 1;
            }
            "nohup" | "builtin" => i += 1,
            "nice" => {
                i += 1;
                skip_options(&mut i, &["-n"]);
            }
            "xargs" => {
                i += 1;
                skip_options(&mut i, &["-n", "-I", "-L", "-P", "-s", "-E", "-d", "-a"]);
            }
            "command" => {
                i += 1;
                while let Some(next) = words.get(i) {
                    if !next.value.starts_with('-') {
                        break;
                    }
                    if next.value.contains(['v', 'V']) {
                        return None;
                    }
                    i += 1;
                }
            }
            "exec" => {
                i += 1;
                skip_options(&mut i, &["-a"]);
            }
            "time" => {
                i += 1;
                skip_options(&mut i, &[]);
            }
            "run" => {
                i += 1;
                while words
                    .get(i)
                    .is_some_and(|next| next.value.starts_with('-') || next.value == "!")
                {
                    i += 1;
                }
            }
            _ => return Some((i, assigned)),
        }
    }
}

/// The command line a shell is given to run with `-c` (`sh -c '...'`, `bash -lc "..."`), from the
/// words after the shell's name.
fn dash_c_script(args: &[&Word]) -> Option<String> {
    for (i, word) in args.iter().enumerate() {
        let value = &word.value;
        if !value.starts_with('-') || value.len() < 2 {
            return None;
        }
        if value[1..].chars().all(|c| c.is_ascii_alphabetic()) && value.contains('c') {
            return args.get(i + 1).map(|script| script.value.clone());
        }
    }
    None
}

/// A tmux that a shell line runs, as its options say: whether it names its server (`-L`, `-S`, `-V`,
/// which asks for the version and connects to nothing, or the `"$@"` of a wrapper that passes on the
/// arguments of a caller that names it), the socket it names (as the shell gives it), whether it is
/// given a config file (`-f`), the tmux command it runs, and the variables set on it (`VAR=value tmux`,
/// `env VAR=value tmux`).
#[derive(Debug)]
struct TmuxRun {
    names_its_server: bool,
    socket: Option<String>,
    config: bool,
    verb: Option<String>,
    assigned: Vec<String>,
}

/// Reads tmux's options from `args` the way tmux's getopt does (`-uL name` is `-u -L name`), up to
/// the tmux command.
fn read_tmux(args: &[&Word], assigned: Vec<String>) -> TmuxRun {
    let mut run = TmuxRun {
        names_its_server: args.iter().any(|word| word.raw.contains("$@")),
        socket: None,
        config: false,
        verb: None,
        assigned,
    };
    let mut i = 0;
    while let Some(word) = args.get(i) {
        let value = &word.value;
        if !value.starts_with('-') || value == "-" {
            break;
        }
        i += 1;
        if value == "--" {
            break;
        }
        let letters: Vec<char> = value.chars().skip(1).collect();
        for (k, &letter) in letters.iter().enumerate() {
            if letter == 'V' {
                run.names_its_server = true;
            }
            if matches!(letter, 'L' | 'S' | 'c' | 'f' | 'T') {
                let attached: String = letters[k + 1..].iter().collect();
                let argument = if attached.is_empty() {
                    i += 1;
                    args.get(i - 1).map(|next| next.value.clone())
                } else {
                    Some(attached)
                };
                match letter {
                    'L' | 'S' => {
                        run.names_its_server = true;
                        run.socket = argument;
                    }
                    'f' => run.config = true,
                    _ => {}
                }
                break;
            }
        }
    }
    run.verb = args.get(i).map(|word| word.value.clone());
    run
}

/// Every tmux a shell line runs: as a command, wherever a command can stand on it, in its
/// substitutions, and in the command lines it gives a shell (`sh -c`) or `eval`.
fn tmux_runs(line: &str) -> Vec<TmuxRun> {
    fn collect(line: &str, depth: usize, found: &mut Vec<TmuxRun>) {
        if depth > 8 {
            return;
        }
        let (tokens, inner) = shell_split(line);
        for body in &inner {
            collect(body, depth + 1, found);
        }
        for command in simple_commands(&tokens) {
            let Some((at, through_env)) = program_of(&command.words) else {
                continue;
            };
            let args = &command.words[at + 1..];
            match basename(&command.words[at].value) {
                "sh" | "bash" | "zsh" | "dash" | "ksh" => {
                    if let Some(script) = dash_c_script(args) {
                        collect(&script, depth + 1, found);
                    }
                }
                "eval" => {
                    let script: Vec<&str> = args.iter().map(|word| word.value.as_str()).collect();
                    collect(&script.join(" "), depth + 1, found);
                }
                "tmux" => {
                    let assigned = command
                        .assigned
                        .iter()
                        .chain(through_env.iter())
                        .map(|word| assigned_name(&word.raw))
                        .collect();
                    found.push(read_tmux(args, assigned));
                }
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    collect(line, 0, &mut found);
    found
}

/// The lines of a shell file as the shell reads them: a line that ends in a backslash goes on in the
/// next one, and the two are one, numbered as the first. A comment line is left out.
fn shell_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let (number, mut joined) = pending.take().unwrap_or((index + 1, String::new()));
        if joined.is_empty() && is_comment(line) {
            continue;
        }
        joined.push_str(line);
        match joined.strip_suffix('\\') {
            Some(open) => pending = Some((number, format!("{open} "))),
            None => lines.push((number, joined)),
        }
    }
    lines.extend(pending);
    lines
}

fn is_shell(path: &str) -> bool {
    path.ends_with(".sh") || path.ends_with(".bats")
}

// ------------------------------------------------------------------ the rules

/// The arguments of a tmux server's start, spelled in Rust: `-f /dev/null` is what every throwaway
/// server in this repository is started with, and nothing else is.
static OWN_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""-f",\s*"/dev/null""#).unwrap());

/// A line that runs `tmux` with a config file, which is a start, whether it is a script's line or a
/// line of the script a Rust test writes (in a string, where a shell file's own reading does not look).
static TMUX_WITH_A_CONFIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\btmux\b[^\n]*\s-f\s").unwrap());

/// What a server's environment is given, as an assignment.
const THE_THREE: [&str; 3] = ["HISTFILE=", "HOME=", "ZDOTDIR="];

/// The tmux commands that start a server where none is running on the socket they name.
const STARTS: [&str; 4] = ["new-session", "new", "start-server", "start"];

/// The same, as a Rust test spells them in the arguments it gives a `Tmux` (the short forms are no
/// words of their own in Rust).
const RUST_STARTS: [&str; 2] = ["\"new-session\"", "\"start-server\""];

/// A `Tmux` made on a socket name the code makes itself (`Tmux::with_socket_name` of a `format!`, or of
/// a variable a `format!` made just above): a fresh socket, which no server listens on yet.
static FRESH_TMUX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"let\s+(?:mut\s+)?(\w+)\s*=\s*Tmux::with_socket_name\(\s*&\s*(?:format!\(|(\w+)\s*\))",
    )
    .unwrap()
});

/// The number of the line `offset` is on.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// The statement of Rust that starts at `offset`: up to the `;` that ends a line, comments left out.
fn statement_at(text: &str, offset: usize) -> String {
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r";[ \t]*(?:\n|$)").unwrap());
    let rest = &text[offset..];
    let end = END.find(rest).map_or(rest.len(), |m| m.end());
    rest[..end]
        .lines()
        .filter(|line| !is_comment(line))
        .map(without_comment)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The starts of a server in a Rust file that are not spelled with `-f /dev/null`: the first command
/// a fresh `Tmux` ([`FRESH_TMUX`]) runs, when it is one that makes a session. Each is the line it is
/// on and the statement. A `Tmux` that is handed to something first (`home.start_server(&t, ...)`,
/// `adopt(&t)`) or moved into a value is not followed further.
fn fresh_starts(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for captures in FRESH_TMUX.captures_iter(text) {
        let binding = captures.get(0).unwrap();
        if let Some(variable) = captures.get(2) {
            let above: Vec<&str> = text[..binding.start()].lines().rev().take(5).collect();
            let made_here = Regex::new(&format!(
                r"let\s+(?:mut\s+)?{}\s*=\s*format!\(",
                regex::escape(variable.as_str())
            ))
            .unwrap();
            if !above.iter().any(|line| made_here.is_match(line)) {
                continue;
            }
        }
        let name = regex::escape(captures.get(1).unwrap().as_str());
        let first_use = Regex::new(&format!(
            r"(?:^|[^.\w])(?P<call>{name}\s*\.\s*(?:run_ok|run|command)\s*\()|&\s*{name}\s*[,)]|[^.\w&]{name}\s*[,}}]"
        ))
        .unwrap();
        let after = binding.end() + text[binding.end()..].find(';').map_or(0, |at| at + 1);
        let Some(used) = first_use.captures(&text[after..]) else {
            continue;
        };
        let Some(call) = used.name("call") else {
            continue;
        };
        let statement = statement_at(text, after + call.start());
        if RUST_STARTS.iter().any(|verb| statement.contains(verb)) {
            found.push((line_of(text, after + call.start()), statement));
        }
    }
    found
}

/// Whether the text of a start statement, or a wrapper of tmux elsewhere in the file (a line of code
/// that sets the three and `exec`s), gives a server all three of `HISTFILE`, `HOME` and `ZDOTDIR`.
fn gives_the_three(statement: &str, text: &str) -> bool {
    let named = |chunk: &str| {
        THE_THREE
            .iter()
            .all(|name| chunk.contains(name.trim_end_matches('=')))
    };
    named(statement)
        || text.lines().any(|line| {
            !is_comment(line)
                && line.contains("exec")
                && THE_THREE.iter().all(|name| line.contains(name))
        })
}

/// What is wrong with the way the file `path`, whose text is `text`, starts tmux servers, one line of
/// words each: a server started in a way that has nothing of the three in its environment, or that
/// loads the host's tmux.conf.
///
/// * A Rust file other than the two that own a start (`tests/common/shell_env.rs`, and
///   `tests/common/mod.rs`, whose `Server` puts the three in the environment of every command it
///   runs) starts a server through `ShellHome::start_server`, when it is a test. A start is spelled
///   with the arguments of one (`"-f", "/dev/null"`), or it is the first command of a fresh `Tmux`
///   that makes a session ([`fresh_starts`]). A unit test in `src/`, which cannot include the helper,
///   names all three on the start (`.env("HISTFILE", ...)`) or in a wrapper of tmux the file writes,
///   in code and not in a comment, and gives `-f /dev/null`.
/// * A tmux command in a shell file that makes a session (`new-session`, `start-server`) on a socket
///   the file has not started already is a start: it gives `-f` (a fresh socket loads the host's
///   tmux.conf without one) and sets all three, on its line or in an `export` in a line of code above
///   (another start's line above, that set them for itself, is no export).
/// * A line of any file that runs `tmux` with a `-f` (a start written as a script, a wrapper a test
///   writes in a string) sets all three on the line or, in a shell file, exports them above.
fn problems(path: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let is_rust = path.ends_with(".rs");
    let owns_a_start = path == "tests/common/shell_env.rs" || path == "tests/common/mod.rs";
    if is_rust && !owns_a_start {
        let mut starts: Vec<(usize, String, bool)> = OWN_START
            .find_iter(text)
            .map(|hit| {
                let line_start = text[..hit.start()].rfind('\n').map_or(0, |at| at + 1);
                (
                    line_of(text, hit.start()),
                    statement_at(text, line_start),
                    true,
                )
            })
            .collect();
        // A fresh start spelled with `-f /dev/null` is one of those already.
        starts.extend(
            fresh_starts(text)
                .into_iter()
                .filter(|(_, statement)| !OWN_START.is_match(statement))
                .map(|(line, statement)| (line, statement, false)),
        );
        starts.sort_by_key(|(line, ..)| *line);
        starts.dedup_by_key(|(line, ..)| *line);
        for (line, statement, with_a_config) in starts {
            if !path.starts_with("src/") {
                found.push(format!(
                    "{path}:{line}: starts a server of its own: use ShellHome::start_server"
                ));
                continue;
            }
            if !with_a_config {
                found.push(format!(
                    "{path}:{line}: starts a server without -f /dev/null, so it loads the host's tmux.conf"
                ));
            }
            if !gives_the_three(&statement, text) {
                found.push(format!(
                    "{path}:{line}: starts a server and names HISTFILE, HOME and ZDOTDIR neither on its start nor in a wrapper"
                ));
            }
        }
    }
    let mut exported: Vec<String> = Vec::new();
    let exported_all = |exported: &[String]| {
        THE_THREE
            .iter()
            .all(|name| exported.iter().any(|line| line.contains(name)))
    };
    if is_shell(path) {
        let mut started: Vec<Option<String>> = Vec::new();
        for (number, line) in shell_lines(text) {
            let runs = tmux_runs(&line);
            for run in &runs {
                let makes_a_session = run
                    .verb
                    .as_deref()
                    .is_some_and(|verb| STARTS.contains(&verb));
                if !(makes_a_session || run.config) {
                    continue;
                }
                if makes_a_session && run.socket.is_some() && started.contains(&run.socket) {
                    continue;
                }
                if makes_a_session {
                    started.push(run.socket.clone());
                    if !run.config {
                        found.push(format!(
                            "{path}:{number}: starts a server without -f, so it loads the host's tmux.conf: {}",
                            line.trim()
                        ));
                    }
                }
                let set_here = THE_THREE.iter().all(|name| {
                    run.assigned
                        .iter()
                        .any(|assigned| *assigned == name.trim_end_matches('='))
                });
                if !set_here && !exported_all(&exported) {
                    found.push(format!(
                        "{path}:{number}: starts a server without HISTFILE, HOME and ZDOTDIR in its environment: {}",
                        line.trim()
                    ));
                }
            }
            if runs.is_empty() && TMUX_WITH_A_CONFIG.is_match(&line) {
                let set_here = THE_THREE.iter().all(|name| line.contains(name));
                if !set_here && !exported_all(&exported) {
                    found.push(format!(
                        "{path}:{number}: starts a server without HISTFILE, HOME and ZDOTDIR in its environment: {}",
                        line.trim()
                    ));
                }
            }
            if line.trim_start().starts_with("export ") {
                exported.push(line);
            }
        }
    } else {
        for (number, line) in text.lines().enumerate() {
            if is_comment(line) || !TMUX_WITH_A_CONFIG.is_match(line) {
                continue;
            }
            if !THE_THREE.iter().all(|name| line.contains(name)) {
                found.push(format!(
                    "{path}:{}: starts a server without HISTFILE, HOME and ZDOTDIR in its environment: {}",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    found
}

#[test]
fn no_file_starts_a_server_with_the_runners_home_and_history() {
    let found: Vec<String> = sources()
        .iter()
        .flat_map(|(path, text)| problems(path, text))
        .collect();

    assert!(
        found.is_empty(),
        "a shell in a pane of these servers would write the history and read the startup files of \
         whoever runs the tests, or the server would load their tmux.conf:\n{}",
        found.join("\n")
    );
}

/// A spawn of tmux from Rust: of the name, of the path `which` finds, of a variable that holds tmux,
/// or of the binary of a `Tmux` (`t.bin`), which runs it with none of the socket the `Tmux` has.
static RUST_TMUX_SPAWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"Command::new\(\s*(?:"tmux"|&?\s*which::which\(\s*"tmux"\s*\)|&?\s*[\w.]*\btmux\b|&?\s*[\w.]*\.bin\b)"#,
    )
    .unwrap()
});

/// A `Tmux` with no socket, which runs a bare tmux: a struct literal's `socket: None`, or one set so.
static NO_SOCKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsocket\s*:\s*None\b|\.socket\s*=\s*None\b").unwrap());

/// The binary of a stand-in tmux that records what it is asked and runs no tmux, or of none at all:
/// a `Tmux` with no socket is one only with such a binary.
static STUB_BIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bbin\b.*(shim|stub|fake|nonexistent)").unwrap());

/// The scripts of the tests themselves, besides those under `tests/`: what runs the suite.
const TEST_SCRIPTS: [&str; 3] = [
    "scripts/check.sh",
    "scripts/cli-sweep.sh",
    "scripts/private-tmux-dir.sh",
];

/// A tmux that is run with neither `-L` nor `-S` goes to the server in `$TMUX` and, with none, to the
/// default one: the server of whoever runs the tests. So every spawn of tmux in a test says its socket:
///
/// * a spawn of tmux in a Rust file of the tests ([`RUST_TMUX_SPAWN`]) has `"-L"` or `"-S"` in code
///   among the next eight lines; a `Tmux` with no socket has a stand-in for a binary
///   ([`STUB_BIN`]); and no test makes one from the environment (`Tmux::from_env()`, whose socket is
///   none where the environment names none);
/// * in a shell or bats file of the tests (but the fixtures, hooks of the wizard's own that run inside
///   the server they are given) and in the scripts that run the suite ([`TEST_SCRIPTS`]), every tmux
///   that runs as a command ([`tmux_runs`]: after an assignment, `env` and its options, `if`, `while`,
///   `timeout`, `nohup`, `xargs`, `command`, `exec`, bats's `run`, in a substitution or backticks, in
///   the command line of an `sh -c` or an `eval`, quoted or with a backslash) names its server with
///   `-L` or `-S` among its options, or asks for the version (`-V`), or is a wrapper that passes on
///   `"$@"`, the arguments of a caller that names it.
///
/// No pattern finds every way to spell a tmux: a string of a hook, a `sh -c` that Rust runs, a wrapper
/// whose callers name no socket, a `taw` run with no socket variable. What the rules miss, the net
/// catches: the gate, the harness, the bats tests and the sweep run with no `$TMUX` and a tmux socket
/// directory of their own (`scripts/private-tmux-dir.sh`), where such a tmux reaches nothing else, and
/// they fail for a server it started there. The product's own code (`src/`) is not read here: it talks
/// to the server of the person who runs it, and the stubs of its unit tests have no socket on purpose.
fn socket_problems(path: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if path.starts_with("tests/") && path.ends_with(".rs") {
        let lines: Vec<&str> = text.lines().collect();
        for (number, line) in lines.iter().enumerate() {
            if is_comment(line) {
                continue;
            }
            let code = without_comment(line);
            let say = |what: &str| format!("{path}:{}: {what}: {}", number + 1, line.trim());
            if RUST_TMUX_SPAWN.is_match(code) {
                let near: Vec<&str> = lines[number..lines.len().min(number + 8)]
                    .iter()
                    .filter(|line| !is_comment(line))
                    .map(|line| without_comment(line))
                    .collect();
                let near = near.join("\n");
                if !near.contains("\"-L\"") && !near.contains("\"-S\"") {
                    found.push(say("runs tmux with neither -L nor -S"));
                }
            }
            if NO_SOCKET.is_match(code) {
                let around = &lines[number.saturating_sub(3)..lines.len().min(number + 4)];
                if !around.iter().any(|line| STUB_BIN.is_match(line)) {
                    found.push(say(
                        "a Tmux with no socket runs a bare tmux, unless its binary is a stand-in",
                    ));
                }
            }
            if code.contains("Tmux::from_env(") {
                found.push(say(
                    "a Tmux from the environment has no socket where the environment names none",
                ));
            }
        }
    } else if is_shell(path)
        && ((path.starts_with("tests/") && !path.starts_with("tests/fixtures/"))
            || TEST_SCRIPTS.contains(&path))
    {
        for (number, line) in shell_lines(text) {
            for run in tmux_runs(&line) {
                if !run.names_its_server {
                    found.push(format!(
                        "{path}:{number}: runs tmux with neither -L nor -S: {}",
                        line.trim()
                    ));
                }
            }
        }
    }
    found
}

#[test]
fn no_test_runs_a_tmux_without_the_socket_of_its_own_server() {
    let found: Vec<String> = sources()
        .iter()
        .flat_map(|(path, text)| socket_problems(path, text))
        .collect();

    assert!(
        found.is_empty(),
        "a tmux with neither -L nor -S would reach the server of whoever runs the tests:\n{}",
        found.join("\n")
    );
}

// What the reading above is tested on: lines that are wrong, and lines that are not.

#[test]
fn a_rust_test_that_starts_its_own_server_is_found() {
    let text = "fn start() {\n    t.run_ok(&[\"-f\", \"/dev/null\", \"new-session\", \"-d\"]).unwrap();\n}\n";

    let found = problems("tests/tmux_new.rs", text);

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/tmux_new.rs:2: "), "{found:?}");
}

#[test]
fn a_rust_test_that_starts_through_the_helper_is_not() {
    let text = "fn start() {\n    home.start_server(&t, &[\"-d\", \"-s\", \"one\"]);\n}\n";

    assert_eq!(problems("tests/tmux_new.rs", text), Vec::<String>::new());
}

#[test]
fn the_two_files_that_own_a_start_may_spell_it() {
    let text = "args.extend([\"-f\", \"/dev/null\", \"new-session\"]);\n";

    assert_eq!(problems("tests/common/mod.rs", text), Vec::<String>::new());
    assert_eq!(
        problems("tests/common/shell_env.rs", text),
        Vec::<String>::new()
    );
}

#[test]
fn a_unit_test_in_src_names_the_three_in_its_own_file_or_is_found() {
    let start = "t.command().args([\"-u\", \"-f\", \"/dev/null\", \"new-session\"])";
    let with = format!("{start}\n.env(\"HISTFILE\", x).env(\"HOME\", y).env(\"ZDOTDIR\", y)\n");

    assert_eq!(problems("src/cli/new.rs", &with), Vec::<String>::new());
    assert_eq!(problems("src/cli/new.rs", start).len(), 1);
}

#[test]
fn a_script_line_that_starts_a_server_sets_all_three_or_is_found() {
    let old = "tmux -L \"$SOCK\" -f /dev/null new-session -d\n";
    let new = "HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\" tmux -L \"$SOCK\" -f /dev/null new-session -d\n";
    let two_of_three =
        "HISTFILE=/dev/null HOME=\"$H\" tmux -L \"$SOCK\" -f /dev/null new-session -d\n";

    assert_eq!(problems("tests/e2e/new.sh", old).len(), 1);
    assert_eq!(problems("tests/e2e/new.sh", new), Vec::<String>::new());
    assert_eq!(problems("tests/e2e/new.sh", two_of_three).len(), 1);
    assert_eq!(problems("tests/bats/new.bats", old).len(), 1);
}

#[test]
fn a_script_may_assign_the_three_in_a_line_of_code_above_the_start() {
    let assigned = "export HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\"\n";
    let start = "tmux -L \"$SOCK\" -f /dev/null new-session -d\n";
    let in_a_comment = "# HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\"\n";
    let after = format!("{start}{assigned}");

    assert_eq!(
        problems("scripts/new.sh", &format!("{assigned}{start}")),
        Vec::<String>::new()
    );
    assert_eq!(
        problems("scripts/new.sh", &format!("{in_a_comment}{start}")).len(),
        1,
        "a comment that names them sets none"
    );
    assert_eq!(
        problems("scripts/new.sh", &after).len(),
        1,
        "set after the start is too late"
    );
}

#[test]
fn a_start_that_sets_them_does_not_stand_for_the_one_below_it() {
    let first = "HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\" tmux -L \"$A\" -f /dev/null new-session -d\n";
    let second = "tmux -L \"$B\" -f /dev/null new-session -d\n";

    let found = problems("tests/e2e/new.sh", &format!("{first}{second}"));

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/e2e/new.sh:2: "), "{found:?}");
}

#[test]
fn a_comment_that_talks_about_a_start_is_not_one() {
    let text = "# tmux -L \"$SOCK\" -f /dev/null new-session -d\n// tmux -f /dev/null\n";

    assert_eq!(problems("scripts/new.sh", text), Vec::<String>::new());
}

#[test]
fn a_script_a_rust_test_writes_is_read_like_any_other() {
    let old = "        \"#!/bin/sh\\nSHELL=/bin/sh exec tmux -f /dev/null \\\"$@\\\"\\n\",\n";
    let new = "        format!(\"#!/bin/sh\\nHISTFILE=/dev/null HOME={h} ZDOTDIR={h} exec tmux -f /dev/null \\\"$@\\\"\\n\"),\n";

    assert_eq!(problems("tests/tmux_new.rs", old).len(), 1);
    assert_eq!(problems("tests/tmux_new.rs", new), Vec::<String>::new());
}

#[test]
fn a_rust_tmux_with_no_socket_is_found() {
    let bare = "let out = Command::new(\"tmux\")\n    .args([\"kill-server\"])\n    .output();\n";
    let named = "let out = Command::new(\"tmux\")\n    .arg(\"-L\")\n    .arg(&socket)\n    .args([\"kill-server\"])\n    .output();\n";
    let by_path = "Command::new(\"tmux\").args([\"-S\", &path, \"kill-server\"])\n";

    let found = socket_problems("tests/tmux_new.rs", bare);

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/tmux_new.rs:1: "), "{found:?}");
    assert_eq!(
        socket_problems("tests/tmux_new.rs", named),
        Vec::<String>::new()
    );
    assert_eq!(
        socket_problems("tests/tmux_new.rs", by_path),
        Vec::<String>::new()
    );
}

#[test]
fn a_script_tmux_with_no_socket_is_found_wherever_a_command_stands() {
    let lines = [
        "tmux kill-server",
        "  tm=$(tmux list-sessions)",
        "ok \"x\" && tmux kill-session -t a",
        "run tmux source-file x.conf",
        "tmux -L \"$SOCK\" list-sessions; tmux kill-server",
        "printf 'x'; tmux new-session -d",
    ];
    for line in lines {
        let found = socket_problems("tests/e2e/new.sh", &format!("{line}\n"));
        assert_eq!(found.len(), 1, "{line}: {found:?}");
    }
}

#[test]
fn a_script_tmux_that_names_its_server_or_asks_for_the_version_is_not_found() {
    let lines = [
        "tmux -L \"$SOCK\" kill-server",
        "tm() { tmux -L \"$SOCK\" \"$@\"; }",
        "echo \"tmux=$(tmux -V)\"",
        "run tmux -L \"$SOCK\" source-file x.conf",
        "outer new-session -d \"tmux -L $SOCK attach -t main\"",
        "tmux -S \"$path\" list-sessions",
        "printf '#!/bin/sh\\nexec tmux -f /dev/null \"$@\"\\n' > \"$dir/tmux-quiet\"",
        "TAW_TMUX_BIN=\"$dir/tmux-quiet\" \"$TAW_BIN\" open x",
    ];
    for line in lines {
        let found = socket_problems("tests/e2e/new.sh", &format!("{line}\n"));
        assert_eq!(found, Vec::<String>::new(), "{line}");
    }
}

#[test]
fn prose_and_patterns_that_say_tmux_are_not_commands() {
    let lines = [
        "ok \"with no tmux running the open starts one\"",
        "ok \"tmux order has the working agent first\"",
        "run bash -c \"grep -c 'tmux run-shell -b' '$PLUGIN'\"",
        "printf '  echo \"tmux 3.1a\"\\n'",
        "fake_open_target '{\"message\":\"`tmux list-panes -a` failed\"}' 1",
        "# tmux kill-server",
    ];
    for line in lines {
        let found = socket_problems("tests/bats/new.bats", &format!("{line}\n"));
        assert_eq!(found, Vec::<String>::new(), "{line}");
    }
}

#[test]
fn the_products_own_sources_are_not_read_for_a_socket() {
    let bare = "let out = Command::new(\"tmux\")\n    .args([\"list-sessions\"])\n    .output();\n";

    assert_eq!(
        socket_problems("src/tmux/mod.rs", bare),
        Vec::<String>::new()
    );
    assert_eq!(
        socket_problems("examples/tui_demo.rs", bare),
        Vec::<String>::new()
    );
    assert_eq!(socket_problems("tests/tmux_new.rs", bare).len(), 1);
}

#[test]
fn the_fixtures_and_the_plugins_own_scripts_run_inside_the_server_they_are_given() {
    let bare = "tmux rename-window -t \"$session\" editor\n";

    assert_eq!(
        socket_problems("tests/fixtures/session-wizard-hooks/x.sh", bare),
        Vec::<String>::new()
    );
    assert_eq!(
        socket_problems("scripts/helpers.sh", bare),
        Vec::<String>::new()
    );
    assert_eq!(socket_problems("scripts/cli-sweep.sh", bare).len(), 1);
    assert_eq!(socket_problems("tests/bats/new.bats", bare).len(), 1);
}

#[test]
fn a_script_tmux_with_no_socket_is_found_however_the_command_is_spelled() {
    // Each runs tmux, with no socket of its own, once (twice for the last).
    let lines = [
        "if tmux has-session -t x; then",
        "while tmux list-sessions >/dev/null; do",
        "until tmux list-sessions; do",
        "elif tmux list-sessions; then",
        "HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\" tmux -f /dev/null new-session -d",
        "X=1 tmux kill-server",
        "env -u TMUX tmux kill-server",
        "env TMUX= tmux kill-server",
        "env -i PATH=\"$PATH\" tmux kill-server",
        "env \"${pinned[@]}\" tmux kill-server",
        "timeout 5 tmux kill-server",
        "timeout -s KILL 5 tmux kill-server",
        "nohup tmux kill-server >/dev/null 2>&1 &",
        "time tmux list-sessions",
        "printf '%s\\n' a | xargs -n1 tmux kill-session -t",
        "command tmux kill-server",
        "command -p tmux kill-server",
        "exec tmux kill-server",
        "exec -a x tmux kill-server",
        "sh -c 'tmux kill-server'",
        "bash -c \"tmux kill-server\"",
        "bash -lc 'tmux kill-server'",
        "x=`tmux list-sessions`",
        "echo \"`tmux list-sessions`\"",
        "echo \"$(tmux display-message -p x)\"",
        "run -0 tmux list-sessions",
        "run --separate-stderr tmux kill-server",
        "run ! tmux has-session -t x",
        "\"tmux\" kill-server",
        "'tmux' kill-server",
        "\\tmux kill-server",
        "tmux\tkill-server",
        "/usr/bin/tmux kill-server",
        "tmux -u kill-server",
        "eval \"tmux kill-server\"",
        "{ tmux kill-server; }",
        "(tmux kill-server)",
        "foo | tmux load-buffer -",
    ];
    for line in lines {
        let found = socket_problems("tests/e2e/new.sh", &format!("{line}\n"));
        assert_eq!(found.len(), 1, "{line}: {found:?}");
    }
    let two = "a && tmux kill-session -t x || tmux kill-server\n";
    assert_eq!(socket_problems("tests/e2e/new.sh", two).len(), 2);
    // A line that ends in a backslash goes on in the next, and the two are read as one.
    let continued = "tmux -L \"$SOCK\" list-sessions && \\\n  tmux kill-server\n";
    let found = socket_problems("tests/e2e/new.sh", continued);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/e2e/new.sh:1: "), "{found:?}");
}

#[test]
fn a_script_tmux_that_names_its_server_however_it_is_spelled_is_not_found() {
    let lines = [
        "HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\" tmux -L \"$SOCK\" -f /dev/null new-session -d",
        "env -u TMUX tmux -L \"$SOCK\" kill-server",
        "HISTFILE=/dev/null HOME=\"$h\" ZDOTDIR=\"$h\" env \"${pinned[@]}\" tmux -L \"$S\" -f /dev/null new-session -d",
        "if tmux -S \"$path\" has-session; then",
        "timeout 5 tmux -L x kill-server",
        "sh -c 'tmux -L x kill-server'",
        "x=`tmux -L x list-sessions`",
        "run -0 tmux -L \"$SOCK\" list-sessions",
        "tmux -uL x list-sessions",
        "tmux -u -L x list-sessions",
        "tmux -f /dev/null -L x new-session -d",
        "tmux -L\"$SOCK\" kill-server",
        "exec tmux -f /dev/null \"$@\"",
        "command -v tmux >/dev/null",
        "which tmux",
        "printf 'tmux kill-server'",
        "echo \"tmux kill-server\"",
        "grep -c 'tmux run-shell -b' x",
        "e2e_wait_screen '^ /tmux-agent-wizard' 2000 \\",
        "case $x in tmux) echo yes ;; esac",
        "tmux -L \"$SOCK\" new-session -d \\\n  -s two",
    ];
    for line in lines {
        let found = socket_problems("tests/e2e/new.sh", &format!("{line}\n"));
        assert_eq!(found, Vec::<String>::new(), "{line}");
    }
}

/// A line that starts a server in the shell harness or in the bats tests with its `-L` taken off
/// is found by the scan, at that line.
#[test]
fn a_server_start_without_its_socket_flag_is_found_in_the_shell_files() {
    for (path, start, planted_start) in [
        (
            "tests/e2e/harness.sh",
            "tmux -L \"$SOCK\" -f \"$DIR/tmux.conf\" new-session",
            "tmux -f \"$DIR/tmux.conf\" new-session",
        ),
        (
            "tests/e2e/harness.sh",
            "tmux -L \"$OUTER\" -f /dev/null new-session",
            "tmux -f /dev/null new-session",
        ),
        (
            "tests/bats/plugin.bats",
            "tmux -L \"$SOCK\" -f /dev/null new-session",
            "tmux -f /dev/null new-session",
        ),
    ] {
        let text = std::fs::read_to_string(repo_root().join(path)).unwrap();
        assert_eq!(text.matches(start).count(), 1, "{path}: {start}");
        assert_eq!(socket_problems(path, &text), Vec::<String>::new(), "{path}");
        let line = text[..text.find(start).unwrap()].matches('\n').count() + 1;
        let planted = text.replacen(start, planted_start, 1);

        let found = socket_problems(path, &planted);

        assert_eq!(found.len(), 1, "{path}: {found:?}");
        assert!(
            found[0].starts_with(&format!("{path}:{line}: ")),
            "{found:?}"
        );
    }
}

#[test]
fn a_rust_tmux_with_no_socket_is_found_however_it_is_made() {
    let found_in = |text: &str| socket_problems("tests/tmux_new.rs", text);
    let wrong = [
        "Command::new(which::which(\"tmux\").unwrap()).args([\"kill-server\"]).output();\n",
        "let out = Command::new(&tmux).args([\"kill-server\"]).output();\n",
        "let out = Command::new(&t.bin).args([\"kill-server\"]).output();\n",
        "let out = Command::new(\"tmux\")\n    // no \"-L\" here\n    .args([\"kill-server\"])\n    .output();\n",
        "let out = Command::new(\"tmux\") // \"-L\" some day\n    .output();\n",
        "let t = Tmux {\n    bin: which::which(\"tmux\").unwrap(),\n    socket: None,\n};\n",
        "let mut t = Tmux::with_socket_name(\"taw-x\");\nt.socket = None;\n",
        "let t = Tmux::from_env();\n",
    ];
    for text in wrong {
        assert_eq!(found_in(text).len(), 1, "{text}: {:?}", found_in(text));
    }
    let right = [
        "let t = Tmux {\n    bin: shim.clone(),\n    socket: None,\n};\n",
        "let bare = Tmux {\n    bin: PathBuf::from(\"/nonexistent/tmux\"),\n    socket: None,\n};\n",
        "Command::new(which::which(\"tmux\").unwrap()).args([\"-L\", &socket, \"kill-server\"]);\n",
        "let out = Command::new(\"tmux\")\n    // the socket of its own:\n    .arg(\"-L\")\n    .output();\n",
        "// let t = Tmux::from_env();\n",
    ];
    for text in right {
        assert_eq!(found_in(text), Vec::<String>::new(), "{text}");
    }
}

#[test]
fn a_script_start_without_a_config_file_is_found_and_a_second_session_is_not() {
    let three = "HISTFILE=/dev/null HOME=\"$H\" ZDOTDIR=\"$H\"";
    let without_f = format!("{three} tmux -L \"$A\" new-session -d\n");
    let with_f = format!("{three} tmux -L \"$A\" -f /dev/null new-session -d\n");
    let through_an_array =
        format!("{three} env \"${{pinned[@]}}\" tmux -L \"$A\" -f /dev/null new-session -d\n");
    let second_session = "tmux -L \"$A\" new-session -d -s two\n";
    let other_socket = format!("{three} tmux -L \"$B\" start-server\n");

    let found = problems("tests/e2e/new.sh", &without_f);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("without -f"), "{found:?}");
    assert_eq!(problems("tests/e2e/new.sh", &with_f), Vec::<String>::new());
    assert_eq!(
        problems("tests/e2e/new.sh", &through_an_array),
        Vec::<String>::new(),
        "assignments kept in an array for env are no command"
    );
    assert_eq!(
        problems("tests/e2e/new.sh", &format!("{with_f}{second_session}")),
        Vec::<String>::new(),
        "a session made on a server the file started is no start"
    );
    let found = problems("tests/e2e/new.sh", &format!("{with_f}{other_socket}"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/e2e/new.sh:2: "), "{found:?}");
    let bare_start = "tmux -L \"$A\" new-session -d\n";
    assert_eq!(
        problems("tests/bats/new.bats", bare_start).len(),
        2,
        "no -f and none of the three"
    );
}

#[test]
fn a_rust_start_on_a_fresh_socket_without_the_helper_is_found() {
    let fresh = "let t = Tmux::with_socket_name(&format!(\"taw-x-{}-{n}\", std::process::id()));\n";
    let started_by_hand =
        format!("{fresh}t.run_ok(&[\"new-session\", \"-d\", \"-s\", \"one\"])\n    .unwrap();\n");
    let through_the_helper = format!(
        "{fresh}home.start_server(&t, &[\"-d\", \"-s\", \"one\"]);\nt.run_ok(&[\"new-session\", \"-d\", \"-s\", \"two\"])\n    .unwrap();\n"
    );
    let named_first = "let name = format!(\"taw-x-{n}\");\nlet t = Tmux::with_socket_name(&name);\nlet started = t\n    .command()\n    .args([\"-u\", \"new-session\", \"-d\"])\n    .status()\n    .unwrap();\n";
    let not_fresh = "let t = Tmux::with_socket_name(&server.socket);\nt.run_ok(&[\"new-session\", \"-d\", \"-s\", \"two\"]).unwrap();\n";

    let found = problems("tests/tmux_new.rs", &started_by_hand);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/tmux_new.rs:2: "), "{found:?}");
    assert_eq!(
        problems("tests/tmux_new.rs", &through_the_helper),
        Vec::<String>::new()
    );
    assert_eq!(problems("tests/tmux_new.rs", named_first).len(), 1);
    assert_eq!(
        problems("tests/tmux_new.rs", not_fresh),
        Vec::<String>::new()
    );
    // In src/, a unit test's start of its own gives -f /dev/null and the three.
    let found = problems("src/cli/new.rs", named_first);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .any(|line| line.contains("without -f /dev/null")),
        "{found:?}"
    );
}

#[test]
fn a_unit_test_in_src_names_the_three_on_its_start_or_its_wrapper_and_not_in_a_comment() {
    let start = "let started = t\n    .command()\n    .args([\"-u\", \"-f\", \"/dev/null\", \"new-session\"])\n";
    let on_the_start = format!(
        "{start}    .env(\"HISTFILE\", \"/dev/null\")\n    .env(\"HOME\", home.path())\n    .env(\"ZDOTDIR\", home.path())\n    .status();\n"
    );
    let in_a_comment = format!(
        "{start}    .status();\n// The shells get HISTFILE, HOME and ZDOTDIR from somewhere else.\n"
    );
    let a_wrapper = "    \"#!/bin/sh\\nHISTFILE=/dev/null HOME={dir} ZDOTDIR={dir} exec {} -u \\\"$@\\\"\\n\",\n";
    let through_a_wrapper = format!("{start}    .status();\n{a_wrapper}");
    let a_wrapper_in_a_comment = format!("{start}    .status();\n// {}", a_wrapper.trim_start());

    assert_eq!(
        problems("src/cli/new.rs", &on_the_start),
        Vec::<String>::new()
    );
    assert_eq!(problems("src/cli/new.rs", &in_a_comment).len(), 1);
    assert_eq!(
        problems("src/cli/new.rs", &through_a_wrapper),
        Vec::<String>::new()
    );
    assert_eq!(problems("src/cli/new.rs", &a_wrapper_in_a_comment).len(), 1);
}
