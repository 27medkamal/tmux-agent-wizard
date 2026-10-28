//! The OpenCode plugins taw installs, run under node with taw's own file for each: what they
//! report, and when, for the panes an OpenCode TUI is showing.
//!
//! The behaviour is herdr's, whose tests for the same plugins run under bun and mock its
//! socket; those are ported to `tests/fixtures/opencode/*.test.mjs`, which run under node with
//! the plugin's `node:child_process` answered by a fake (see `fake-child-process.mjs`), and this
//! file renders the plugins the way an install does and runs them. Every run is skipped with a
//! message, as the other tests that need a runtime are, where node is not there or is too old
//! for module hooks.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The fake taw the plugins are rendered with. Nothing runs it: the plugins' `execFile` is the
/// fake's, which only checks that this is the path it was asked to run.
const TAW: &str = "/opt/taw/bin/taw";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

fn template(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/integration/templates")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// What `render_template` makes of a JavaScript template: the taw path escaped for a double
/// quoted string, and the agent's name.
fn render(template: &str, taw_bin: &str) -> String {
    template
        .replace(
            "{{TAW_BIN}}",
            &format!("\"{}\"", taw_bin.replace('\\', "\\\\").replace('"', "\\\"")),
        )
        .replace("{{AGENT}}", "opencode")
}

/// Whether `node` is here and new enough to register module hooks (20.6).
fn node_with_module_hooks() -> bool {
    let Ok(output) = Command::new("node").arg("--version").output() else {
        eprintln!("skipping: node is not on PATH");
        return false;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) < (20, 6) {
        eprintln!(
            "skipping: node {} cannot register module hooks",
            version.trim()
        );
        return false;
    }
    true
}

/// Runs the node test file `name` from the fixtures against the rendered plugin at `plugin`.
fn run_node_tests(name: &str, plugin: &Path) {
    let register = fixtures().join("register-fake-child-process.mjs");
    let output = Command::new("node")
        .arg("--import")
        .arg(&register)
        .arg(fixtures().join(name))
        .env("TAW_PLUGIN", plugin)
        .env("TAW_BIN_EXPECTED", TAW)
        .env_remove("TMUX_PANE")
        .env_remove("TAW_REPORT_FILE")
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "{name} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The plugin files as an install lays them out: the TUI plugin beside the config, and the
/// version 2 directory entry that re-exports it.
struct Installed {
    _dir: tempfile::TempDir,
    server_plugin: PathBuf,
    tui_plugin: PathBuf,
    v2_entry: PathBuf,
}

fn installed() -> Installed {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("plugins");
    std::fs::create_dir(&plugins).unwrap();
    let server_plugin = plugins.join("taw-agent-state.js");
    std::fs::write(
        &server_plugin,
        render(&template("taw-agent-state-opencode.js"), TAW),
    )
    .unwrap();
    let tui_plugin = dir.path().join("taw-tui-session.js");
    std::fs::write(&tui_plugin, render(&template("taw-tui-session.js"), TAW)).unwrap();
    let v2_dir = dir.path().join("taw-opencode");
    std::fs::create_dir(&v2_dir).unwrap();
    let v2_entry = v2_dir.join("tui.js");
    std::fs::write(&v2_entry, render(&template("taw-opencode-tui.js"), TAW)).unwrap();
    Installed {
        _dir: dir,
        server_plugin,
        tui_plugin,
        v2_entry,
    }
}

#[test]
fn the_tui_plugin_reports_the_pane_the_tui_is_in_as_herdrs_does() {
    if !node_with_module_hooks() {
        return;
    }
    let files = installed();
    run_node_tests("tui-session.test.mjs", &files.tui_plugin);
}

#[test]
fn inside_a_sandbox_the_tui_plugin_writes_its_selection_to_the_report_file() {
    if !node_with_module_hooks() {
        return;
    }
    let files = installed();
    run_node_tests("tui-session-sandbox.test.mjs", &files.tui_plugin);
}

#[test]
fn the_server_plugin_reports_for_the_local_clients_that_have_no_tui_plugin_as_herdrs_does() {
    if !node_with_module_hooks() {
        return;
    }
    let files = installed();
    run_node_tests("server-plugin.test.mjs", &files.server_plugin);
}

#[test]
fn a_taw_path_that_needs_escaping_is_still_the_path_the_plugin_runs() {
    if !node_with_module_hooks() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("taw-tui-session.js");
    let odd = "/opt/taw \"quoted\" \\back/taw";
    std::fs::write(&plugin, render(&template("taw-tui-session.js"), odd)).unwrap();
    let register = fixtures().join("register-fake-child-process.mjs");
    let output = Command::new("node")
        .arg("--import")
        .arg(&register)
        .arg(fixtures().join("tui-session-path.mjs"))
        .env("TAW_PLUGIN", &plugin)
        .env("TAW_BIN_EXPECTED", odd)
        .env_remove("TAW_REPORT_FILE")
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_version_2_entry_is_the_same_plugin_as_the_version_1_file() {
    if !node_with_module_hooks() {
        return;
    }
    let files = installed();
    let register = fixtures().join("register-fake-child-process.mjs");
    let output = Command::new("node")
        .arg("--import")
        .arg(&register)
        .arg(fixtures().join("tui-session-entry.mjs"))
        .env("TAW_PLUGIN", &files.tui_plugin)
        .env("TAW_V2_ENTRY", &files.v2_entry)
        .env("TAW_BIN_EXPECTED", TAW)
        .env_remove("TAW_REPORT_FILE")
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
