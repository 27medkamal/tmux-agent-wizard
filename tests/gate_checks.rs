//! Text tests that hold the gate and the tests' own nets to what they promise, where a test of the
//! behaviour would only fail once somebody ran it on a machine that shows it.
//!
//! The nets (scripts/agent-net.sh, scripts/desktop-net.sh, scripts/git-decoy.sh) are proved by tests of
//! their own (tests/agent_net.rs, tests/desktop_net.rs, tests/git_decoy_gate.rs), but a net does nothing
//! the day the line that makes it, or the line that reads its log, is edited out of the script that runs
//! a suite. These look for those lines.

mod common;

use std::path::{Path, PathBuf};

use common::repo_root;

/// Every Rust test file under tests/, with its text.
fn test_sources() -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let mut folders = vec![repo_root().join("tests")];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                found.push((path, text));
            }
        }
    }
    found.sort();
    found
}

fn named(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// A test that sets `$TAW_NOTIFY_CMD` or `$TAW_SOUND_CMD` in the test process, for its watcher or its
/// toast, puts the value it found back (`common::EnvVar`), and does not remove the variable: the gate
/// exports a refusing recorder of each, and a `remove_var` after one test leaves every test after it with
/// the desktop's own `osascript` and `afplay`.
#[test]
fn no_test_removes_the_recorders_the_gate_gave_every_test() {
    let remove = concat!("env::remove_", "var(");
    let mut found = Vec::new();
    for (path, text) in test_sources() {
        for (n, line) in text.lines().enumerate() {
            if line.contains(remove)
                && (line.contains("TAW_NOTIFY_CMD") || line.contains("TAW_SOUND_CMD"))
            {
                found.push(format!("{}:{}", named(&path), n + 1));
            }
        }
    }
    assert!(
        found.is_empty(),
        "these take a recorder out of the process's environment (use common::EnvVar, which puts it back): {found:?}"
    );
}

/// The lines of the script at `path` under the repository, each with its number (from 1).
fn lines_of(path: &str) -> Vec<(usize, String)> {
    std::fs::read_to_string(repo_root().join(path))
        .unwrap_or_else(|err| panic!("{path}: {err}"))
        .lines()
        .enumerate()
        .map(|(n, line)| (n + 1, line.to_string()))
        .collect()
}

/// The number of the first line of `lines` that is `wanted` once trimmed.
fn line_of(lines: &[(usize, String)], wanted: &str) -> Option<usize> {
    lines
        .iter()
        .find(|(_, line)| line.trim() == wanted)
        .map(|(n, _)| *n)
}

/// The number of the first line of `lines` after `after` that starts with `start`, once trimmed.
fn line_starting(lines: &[(usize, String)], after: usize, start: &str) -> Option<usize> {
    lines
        .iter()
        .find(|(n, line)| *n > after && line.trim_start().starts_with(start))
        .map(|(n, _)| *n)
}

/// The gate makes each net before its first step, and looks in each when its last step has ended, and
/// fails on what it finds: the agent net's and the desktop net's checks, which no test of the nets can
/// hold (they run the script's functions, not the script that is the gate), and the decoy's, after each
/// of the two steps that run on it.
#[test]
fn the_gate_makes_each_net_first_and_checks_each_when_its_steps_are_over() {
    let gate = lines_of("scripts/check.sh");
    let first_step = gate
        .iter()
        .find(|(_, line)| line.trim_start().starts_with("run_step \""))
        .map(|(n, _)| *n)
        .expect("scripts/check.sh runs no step");
    let last_step = gate
        .iter()
        .rev()
        .find(|(_, line)| line.trim_start().starts_with("run_step \""))
        .map(|(n, _)| *n)
        .unwrap();
    for (script, make) in [
        ("agent-net.sh", "agent_net_make \"$PRIVATE_TMUX_DIR/net\""),
        (
            "desktop-net.sh",
            "desktop_net_make \"$PRIVATE_TMUX_DIR/desktop-net\"",
        ),
        (
            "git-decoy.sh",
            "git_decoy_make \"$PRIVATE_TMUX_DIR/git-decoy\"",
        ),
    ] {
        let source = format!("source \"$CURRENT_DIR/scripts/{script}\"");
        assert!(
            line_of(&gate, &source).is_some(),
            "scripts/check.sh does not source scripts/{script}"
        );
        let made = line_starting(&gate, 0, make)
            .unwrap_or_else(|| panic!("scripts/check.sh does not call {make}"));
        assert!(
            made < first_step,
            "scripts/check.sh makes the net of {script} (line {made}) after its first step (line {first_step})"
        );
    }
    // The agent net's and the desktop net's checks: after the last step, and each fails the gate.
    for check in ["agent_net_check", "desktop_net_check"] {
        let wanted = format!("if ! {check}; then");
        let at = line_of(&gate, &wanted)
            .unwrap_or_else(|| panic!("scripts/check.sh does not run {check} as `{wanted}`"));
        assert!(
            at > last_step,
            "{check} runs (line {at}) before the last step (line {last_step})"
        );
        let sets_status = gate
            .iter()
            .any(|(n, line)| (at + 1..=at + 3).contains(n) && line.trim() == "status=1");
        assert!(
            sets_status,
            "scripts/check.sh does not fail the gate when {check} does"
        );
    }
    // The decoy's, after each of the two steps that run on it: the step runs through `git_decoy_run`,
    // and the next line looks at the decoy and fails the gate.
    for (step, what) in [
        (
            "run_step \"cargo test\" git_decoy_run cargo test --no-fail-fast",
            "cargo test",
        ),
        (
            "run_step \"bats tests/bats\" git_decoy_run \"${bats_cmd[@]}\" tests/bats",
            "bats tests/bats",
        ),
    ] {
        let at = line_of(&gate, step)
            .unwrap_or_else(|| panic!("scripts/check.sh does not run `{step}`"));
        let check = format!("git_decoy_check \"{what}\" || status=1");
        assert!(
            gate.iter()
                .any(|(n, line)| *n == at + 1 && line.trim() == check),
            "the line after `{step}` is not `{check}`"
        );
    }
}

/// The acceptance run brings its servers up with both nets (`e2e_up` makes them) and, when its last
/// section is over, checks that neither logged a call.
#[test]
fn the_acceptance_run_makes_both_nets_and_checks_both_logs() {
    let harness = lines_of("tests/e2e/harness.sh");
    for make in [
        "agent_net_make \"$PRIVATE_TMUX_DIR/net\" || return 1",
        "desktop_net_make \"$PRIVATE_TMUX_DIR/desktop-net\" || return 1",
    ] {
        assert!(
            line_of(&harness, make).is_some(),
            "tests/e2e/harness.sh (e2e_up) does not run `{make}`"
        );
    }
    let acceptance = lines_of("tests/e2e/acceptance.sh");
    let end = line_of(&acceptance, "[ \"$fail\" -eq 0 ]")
        .expect("tests/e2e/acceptance.sh ends with no test of its failures");
    for (what, hits) in [
        (
            "no agent, container runtime or ssh ran from the run's own PATH",
            "agent_net_hits",
        ),
        (
            "no notification or sound reached the desktop of whoever runs it",
            "desktop_net_hits",
        ),
    ] {
        let wanted = format!("check \"{what}\" \"\" \"$({hits})\"");
        let at = line_of(&acceptance, &wanted)
            .unwrap_or_else(|| panic!("tests/e2e/acceptance.sh does not run `{wanted}`"));
        assert!(
            at < end,
            "the acceptance run checks {hits} (line {at}) after the line that ends it (line {end})"
        );
    }
}

/// Whether `path` is a file anyone can run.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A `cargo test` run, in the gate or on its own, gives every test a notifier and a player that refuse:
/// `.cargo/config.toml` names the repository's refusing script for both variables, which the gate
/// replaces by recorders of its own. Cargo puts `[env]` in the environment of the test binaries it runs,
/// which this shows: the variables are set and name a program that is there. Only the repository's own
/// script is run, to show that it refuses, never the program the variables name: in the gate that is the
/// gate's recorder, which notes every call it gets, and the gate fails on any (scripts/desktop-net.sh).
#[test]
fn cargo_gives_every_test_a_notifier_and_a_player_that_refuse() {
    for name in ["TAW_NOTIFY_CMD", "TAW_SOUND_CMD"] {
        let value = std::env::var_os(name)
            .unwrap_or_else(|| panic!("{name} is not set: cargo did not apply .cargo/config.toml"));
        let program = PathBuf::from(&value);
        assert!(
            program.is_absolute() && is_executable(&program),
            "{name} does not name a program that is there: {program:?}"
        );
    }
    let script = repo_root().join("scripts").join("refuse-desktop.sh");
    let status = std::process::Command::new(&script)
        .arg("title")
        .arg("body")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1), "{script:?} does not refuse");
}

/// The config is as the reason for it needs: both variables, relative to the repository, and not forced
/// (a forced value would replace the gate's recorders and a test's own), naming a script that is there.
#[test]
fn the_cargo_config_names_a_refusing_script_for_both_recorders_and_forces_neither() {
    let text = std::fs::read_to_string(repo_root().join(".cargo").join("config.toml")).unwrap();
    let config: toml::Value = toml::from_str(&text).unwrap();
    let env = config.get("env").and_then(|env| env.as_table()).unwrap();
    for name in ["TAW_NOTIFY_CMD", "TAW_SOUND_CMD"] {
        let entry = env
            .get(name)
            .and_then(|entry| entry.as_table())
            .unwrap_or_else(|| panic!(".cargo/config.toml has no [env] {name} table"));
        assert_eq!(
            entry.get("relative").and_then(|v| v.as_bool()),
            Some(true),
            "{name} is not relative to the repository"
        );
        assert!(
            !entry
                .get("force")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            "{name} is forced: it would replace the gate's recorder and a test's own"
        );
        let script = entry.get("value").and_then(|v| v.as_str()).unwrap();
        assert!(
            is_executable(&repo_root().join(script)),
            "{name} names {script}, which is not an executable file"
        );
    }
    assert_eq!(
        env["TAW_NOTIFY_CMD"]["value"], env["TAW_SOUND_CMD"]["value"],
        "one refusing script serves both"
    );
}
