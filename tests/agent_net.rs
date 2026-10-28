//! No test runs an agent or a container runtime from the PATH of whoever runs the suite.
//!
//! A test that types an agent's command into a pane, or runs a container runtime, has to find a fake of
//! its own first: on the runner's PATH, `claude --resume` starts the Claude Code installed there, and
//! `docker info` asks the real Docker. The gate (scripts/check.sh) and the acceptance harness put a net of
//! refusing stubs first on PATH for that (scripts/agent-net.sh): one for every name in
//! scripts/agent-net-names.txt, which notes the call in the net's log and refuses it, and the gate fails
//! when the log has a line.
//!
//! These tests prove the net. Under the gate, which says where its net is (`TAW_AGENT_NET`), a bare
//! `claude` and a bare `docker` planted here, as a test that forgot its fakes would run them, find the
//! net's stubs first and are refused and noted; the notes go to a log of the test's own
//! (`TAW_AGENT_NET_LOG`), so that the gate's stays empty. And, under the gate or not, the net a script
//! makes has a stub for every name, refuses and notes each call, and fails its check when one came.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::harness::run_bash;
use common::repo_root;

/// Where `name` is found first on `path`, as a shell finds a command.
fn found_first(name: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The lines of `log`, each a call a stub noted: its name, its arguments, its directory and its pane.
fn noted(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Whether this runs in the gate's own tmux directory, which scripts/check.sh makes as
/// `/tmp/taw-check.XXXXXX` and names in `TMUX_TMPDIR`.
fn in_the_gates_tmux_directory() -> bool {
    std::env::var_os("TMUX_TMPDIR")
        .map(PathBuf::from)
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().starts_with("taw-check."))
        })
        .unwrap_or(false)
}

#[test]
fn under_the_gate_a_bare_agent_and_a_bare_runtime_find_the_nets_stubs_and_are_refused() {
    let Ok(net) = std::env::var("TAW_AGENT_NET") else {
        // In the gate a missing net is the gate's own: a check.sh that stopped making it fails here.
        assert!(
            !in_the_gates_tmux_directory(),
            "the gate runs this without its net: scripts/check.sh made no TAW_AGENT_NET"
        );
        eprintln!("not under the gate's net (no TAW_AGENT_NET): scripts/check.sh runs this");
        return;
    };
    let path = std::env::var("PATH").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("log");
    for name in ["claude", "docker"] {
        // Looked up first, and not run unless it is the net's: a real one is not to be run here either.
        assert_eq!(
            found_first(name, &path),
            Some(Path::new(&net).join("bin").join(name)),
            "{name} is not the net's first on the gate's PATH"
        );
        let status = Command::new(name)
            .arg("taw-net-guard")
            .current_dir(tmp.path())
            .env("TAW_AGENT_NET_LOG", &log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(127), "{name}");
    }
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        noted(&log),
        [
            format!("claude\ttaw-net-guard\t{dir}\t"),
            format!("docker\ttaw-net-guard\t{dir}\t"),
        ]
    );
}

#[test]
fn the_net_has_a_stub_for_every_name_and_its_check_names_each_call_it_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/agent-net.sh'\n\
             agent_net_make \"$PWD/net\" || echo failed=make\n\
             agent_net_check; echo empty=$?\n\
             while IFS= read -r name; do\n\
             \x20 case \"$name\" in '' | '#'*) continue ;; esac\n\
             \x20 [ \"$(command -v \"$name\")\" = \"$PWD/net/bin/$name\" ] || {{ echo \"not-first=$name\"; exit 9; }}\n\
             done < \"$AGENT_NET_NAMES\"\n\
             [ \"$(command -v claude)\" = \"$PWD/net/bin/claude\" ] || exit 9\n\
             [ \"$(command -v docker)\" = \"$PWD/net/bin/docker\" ] || exit 9\n\
             claude --resume x; echo claude=$?\n\
             TMUX_PANE=%7 docker info; echo docker=$?\n\
             agent_net_check 2> check.err; echo check=$?\n",
            repo_root().display()
        ),
        &[("TAW_AGENT_NET_LOG", "/no/such/log")],
    );
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        run.stdout, "empty=0\nclaude=127\ndocker=127\ncheck=1\n",
        "{}",
        run.stderr
    );
    let said = std::fs::read_to_string(tmp.path().join("check.err")).unwrap();
    assert!(
        said.contains(&format!("claude\t--resume x\t{dir}\t\n"))
            && said.contains(&format!("docker\tinfo\t{dir}\t%7\n")),
        "{said}"
    );
}

/// The test above fails, and does not pass by skipping, when it runs in the gate's own tmux directory
/// (`taw-check.*`) with no `TAW_AGENT_NET`, as it would under a scripts/check.sh that had stopped making
/// the net. It is run here as the gate would run it, by this test binary, with the environment so.
#[test]
fn the_gate_only_test_fails_in_the_gates_directory_without_the_net() {
    let tmp = tempfile::tempdir().unwrap();
    let gates = tmp.path().join("taw-check.test");
    std::fs::create_dir_all(&gates).unwrap();
    let run = Command::new(std::env::current_exe().unwrap())
        .args([
            "under_the_gate_a_bare_agent_and_a_bare_runtime_find_the_nets_stubs_and_are_refused",
            "--exact",
            "--test-threads=1",
        ])
        .env_remove("TAW_AGENT_NET")
        .env("TMUX_TMPDIR", &gates)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!run.status.success(), "it passed without the net:\n{said}");
    assert!(
        said.contains("the gate runs this without its net"),
        "it failed for another reason:\n{said}"
    );
}
