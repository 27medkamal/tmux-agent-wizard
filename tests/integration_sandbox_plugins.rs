//! The Kilo, Pi, OMP and Hermes plugins inside a sandbox, rendered as an install renders them and
//! run where their runtime is here: node (20.6 and up for module hooks, 22.7 and up to strip Pi's
//! and OMP's types: a `.ts` with `import` in a folder with no package.json needs module detection,
//! on by default from 22.7) and python3. A run whose runtime is missing says it skipped. Each runs
//! with its environment cleared and a home of its own: nothing of the developer's reaches it.

use std::path::{Path, PathBuf};
use std::process::Command;

const TAW: &str = "/opt/taw/bin/taw";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

/// `template` rendered for `agent` into `dir`, its taw a path nothing runs.
fn rendered(dir: &Path, template: &str, agent: &str, name: &str) -> PathBuf {
    let text = std::fs::read_to_string(repo("src/integration/templates").join(template)).unwrap();
    let out = dir.join(name);
    std::fs::write(
        &out,
        // The placeholder stands for the whole expression: a shell word, or a string literal.
        text.replace(
            "{{TAW_BIN}}",
            &if template.ends_with(".sh") {
                format!("'{TAW}'")
            } else {
                format!("\"{TAW}\"")
            },
        )
        .replace("{{AGENT}}", agent),
    )
    .unwrap();
    out
}

fn node_at_least(major: u32, minor: u32) -> bool {
    let Ok(out) = Command::new("node").arg("--version").output() else {
        eprintln!("skipping: node is not on PATH");
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text
        .trim()
        .trim_start_matches('v')
        .split('.')
        .filter_map(|p| p.parse::<u32>().ok());
    let found = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if found < (major, minor) {
        eprintln!(
            "skipping: node {} is older than {major}.{minor}",
            text.trim()
        );
        return false;
    }
    true
}

/// Runs `program args` with the plugin at `plugin`, `TAW_REPORT_FILE` in a folder of its own, no
/// `TMUX_PANE`, and nothing of the developer's: the program is found on this machine's `PATH` and run
/// by its path, with its environment cleared and a home, agents' folders and XDG folders of its own.
fn run(program: &str, args: &[&Path], plugin: &Path, extra: &[(&str, &str)]) {
    let folder = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let mut cmd = Command::new(which::which(program).unwrap());
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", h)
        .env("HISTFILE", "/dev/null")
        .env("CLAUDE_CONFIG_DIR", h.join(".claude"))
        .env("CODEX_HOME", h.join(".codex"))
        .env("XDG_CONFIG_HOME", h.join(".config"))
        .env("XDG_STATE_HOME", h.join(".local/state"))
        .env("XDG_CACHE_HOME", h.join(".cache"))
        .env("TAW_PLUGIN", plugin)
        .env("TAW_BIN_EXPECTED", TAW)
        .env("TAW_REPORT_FILE", folder.path().join("session.json"));
    for (name, value) in extra {
        cmd.env(name, value);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn kilos_plugin_writes_its_session_to_the_report_file() {
    if !node_at_least(20, 6) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plugin = rendered(dir.path(), "taw-agent-state-kilo.js", "kilo", "kilo.js");
    let register = repo("tests/fixtures/opencode/register-fake-child-process.mjs");
    run(
        "node",
        &[
            Path::new("--import"),
            &register,
            &repo("tests/fixtures/sandbox/plugins/kilo.test.mjs"),
        ],
        &plugin,
        &[],
    );
}

#[test]
fn pis_and_omps_plugins_write_their_session_file_to_the_report_file() {
    if !node_at_least(22, 7) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let register = repo("tests/fixtures/opencode/register-fake-child-process.mjs");
    for (template, agent) in [
        ("taw-agent-state-pi.ts", "pi"),
        ("taw-agent-state-omp.ts", "omp"),
    ] {
        let plugin = rendered(dir.path(), template, agent, &format!("{agent}.ts"));
        run(
            "node",
            &[
                Path::new("--experimental-strip-types"),
                Path::new("--import"),
                &register,
                &repo("tests/fixtures/sandbox/plugins/pi.test.mjs"),
            ],
            &plugin,
            &[("TAW_AGENT", agent)],
        );
    }
}

#[test]
fn hermess_plugin_writes_its_session_to_the_report_file() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let plugin = rendered(
        dir.path(),
        "taw-hermes-plugin.py",
        "hermes",
        "taw_hermes_plugin.py",
    );
    run(
        "python3",
        &[&repo("tests/fixtures/sandbox/plugins/hermes_test.py")],
        &plugin,
        &[("PYTHONDONTWRITEBYTECODE", "1")],
    );
}
