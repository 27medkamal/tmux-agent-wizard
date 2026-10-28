//! The OpenCode integration, end to end through the built `taw` binary: the install writes
//! the plugins, and the plugins, run under node against a real tmux pane, leave in the pane's
//! options what a watcher, the popup and `taw agent get` read.
//!
//! `taw integration install opencode` runs as a subprocess with `$HOME` and both XDG
//! directories under a scratch directory, so nothing here can reach a real OpenCode config. The
//! plugins it writes are then loaded by node with a small stand-in for the TUI (see
//! `tests/fixtures/opencode/drive-*.mjs`) and run the real `taw`, which talks to a throwaway
//! tmux server; the test reads the options back from tmux itself. This is what proves that the
//! arguments the plugins build are the ones the two `pane report-*` verbs accept, and that the
//! reports reach the pane in the order they were made.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use common::{Server, taw_bin, wait_for};

/// Ceiling for one step's reports to have reached the pane.
const SETTLE: Duration = Duration::from_secs(20);

/// Session ids as OpenCode makes them: `ses_`, twelve hexadecimal digits (its clock and a
/// counter) and fourteen letters and digits (its random part). The drivers are handed them, and
/// the assertions read them back from the pane, so what the plugins pass through `taw pane
/// report-*` and its session id check is the shape they meet in use.
const SESSION: &str = "ses_3e1a04b21ffeQ7mZk2VtXc9LwH";
/// A child agent's session, which the TUI plugin reports for its root.
const CHILD_SESSION: &str = "ses_3e1a05c8dffeN4wRt8LbKe2YpA";
/// The session of an `opencode run`, which has no TUI.
const RUN_SESSION: &str = "ses_3e1a0c6f9ffeH2kR8pYd4NwVs3";

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

fn have_node() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/opencode")
        .join(name)
}

/// A scratch OpenCode with the integration installed by the real binary.
struct Installed {
    _root: tempfile::TempDir,
    config: PathBuf,
}

fn installed() -> Installed {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("xdg/opencode");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(root.path().join("home")).unwrap();
    let output = Command::new(taw_bin())
        .args(["integration", "install", "opencode"])
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("xdg"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run taw integration install opencode");
    assert!(
        output.status.success(),
        "install failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Installed {
        _root: root,
        config,
    }
}

/// A node driver that says `done <step>` after each step and waits for a line before the next.
struct Driver {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

fn drive(server: &Server, driver: &str, plugin: &Path, pane: &str) -> Driver {
    let mut child = Command::new("node")
        .arg(fixture(driver))
        .env("TAW_PLUGIN", plugin)
        .env("TAW_SESSION", SESSION)
        .env("TAW_CHILD_SESSION", CHILD_SESSION)
        .env("TAW_RUN_SESSION", RUN_SESSION)
        .env("TMUX_PANE", pane)
        .env("TAW_TMUX_SOCKET_NAME", server.socket())
        .env("TAW_CONFIG_DIR", server.config_dir())
        .env("TAW_STATE_DIR", server.state_dir())
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn node");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    Driver {
        child,
        stdin,
        lines,
    }
}

impl Driver {
    /// Waits for the driver to finish `step`.
    fn finished(&self, step: &str) {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|err| panic!("the driver never finished {step}: {err}"));
        assert_eq!(line, format!("done {step}"));
    }

    /// Lets it go on to the next step.
    fn go_on(&mut self) {
        writeln!(self.stdin).unwrap();
        self.stdin.flush().unwrap();
    }

    fn end(mut self) {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for the driver");
        assert!(status.success(), "the driver failed: {status}");
    }
}

/// Waits for the pane's `@agent_state` to be `state`, and says what else it reads.
fn expect_state(server: &Server, pane: &str, state: &str) {
    assert!(
        wait_for(|| server.pane_opt(pane, "@agent_state") == state, SETTLE),
        "wanted {state:?}, got {:?}",
        server.pane_opt(pane, "@agent_state")
    );
}

#[test]
fn the_tui_plugin_reports_the_session_its_pane_shows_through_the_real_verbs() {
    if !have_node() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let files = installed();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.pane();
    let plugin = files.config.join("taw-tui-session.js");
    let mut driver = drive(&server, "drive-tui.mjs", &plugin, &pane);

    // Selecting a session reports it, and the state its family is in: nothing running.
    driver.finished("select");
    expect_state(&server, &pane, "idle");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "opencode");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), SESSION);
    // It is the agent's own report (a hook's), and it names itself.
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:integration:opencode"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_reported_kind"), "opencode");
    assert_eq!(
        server.pane_opt(&pane, "@agent_token_session_start_source"),
        "select"
    );
    driver.go_on();

    // A turn.
    driver.finished("working");
    expect_state(&server, &pane, "working");
    driver.go_on();

    // A child agent waiting for a permission blocks the pane, for the root the TUI shows.
    driver.finished("child-blocked");
    expect_state(&server, &pane, "blocked");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), SESSION);
    driver.go_on();

    // The answer sends it back to work, and the turn's end makes it idle.
    driver.finished("child-replied");
    expect_state(&server, &pane, "working");
    driver.go_on();
    driver.finished("idle");
    expect_state(&server, &pane, "idle");
    let got = server.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["agent"], "opencode", "{got}");
    assert_eq!(got["agent"]["agent_status"], "idle", "{got}");
    assert_eq!(got["agent"]["agent_session_id"], SESSION, "{got}");
    driver.go_on();

    // Leaving the session for the home screen settles the pane, and reports nothing new.
    driver.finished("home");
    expect_state(&server, &pane, "idle");
    driver.go_on();
    driver.end();
}

#[test]
fn the_server_plugin_reports_for_opencode_run_through_the_real_verbs() {
    if !have_node() {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let files = installed();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.pane();
    let plugin = files.config.join("plugins/taw-agent-state.js");
    let mut driver = drive(&server, "drive-server.mjs", &plugin, &pane);

    driver.finished("working");
    expect_state(&server, &pane, "working");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "opencode");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), RUN_SESSION);
    assert_eq!(
        server.pane_opt(&pane, "@agent_source"),
        "hook:integration:opencode"
    );
    driver.go_on();
    driver.finished("blocked");
    expect_state(&server, &pane, "blocked");
    driver.go_on();
    driver.finished("idle");
    expect_state(&server, &pane, "idle");
    driver.go_on();
    driver.end();
}

#[test]
fn the_install_it_ran_is_the_one_an_uninstall_takes_out_again() {
    // The same binary, the same scratch directories: what the real `taw` wrote is what the
    // real `taw` removes, and leaves the config directory as it found it.
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("xdg/opencode");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(root.path().join("home")).unwrap();
    let run = |verb: &str| {
        Command::new(taw_bin())
            .args(["integration", verb, "opencode"])
            .env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("xdg"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap()
    };
    let installed = run("install");
    assert!(installed.status.success(), "{installed:?}");
    let message: serde_json::Value = serde_json::from_slice(&installed.stdout).unwrap();
    let said = message["result"]["message"].as_str().unwrap_or_default();
    assert!(said.contains("restart OpenCode"), "{message}");
    assert!(config.join("taw-tui-session.js").is_file());
    assert!(config.join("tui.jsonc").is_file());

    let uninstalled = run("uninstall");
    assert!(uninstalled.status.success(), "{uninstalled:?}");
    // `plugins/` is OpenCode's own convention directory, so it may stay; nothing of taw's may.
    let mut left = Vec::new();
    let mut dirs = vec![config.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry.path().is_dir() {
                dirs.push(entry.path());
            } else {
                left.push(entry.path());
            }
        }
    }
    assert!(left.is_empty(), "left behind: {left:?}");
    assert!(!config.join("taw-opencode").exists());
}
