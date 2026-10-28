//! No test runs ssh from the PATH of whoever runs the suite.
//!
//! taw logs in to other machines by ssh (src/machine/ssh.rs), and a test that reached a real one would
//! log in to a real machine. Every test goes through `$TAW_SSH_BIN`, which tests/common sets to a
//! refusing recorder (tests/shell_isolation.rs holds that), and where a test forgot it, `ssh` is found on
//! the PATH (and under the gate, `TAW_SSH_BIN` names the net's stub, so a test that forgot it runs that
//! stub). The gate (scripts/check.sh) and the acceptance harness put the agent net first on PATH
//! (scripts/agent-net.sh), which has a refusing stub for each name in scripts/ssh-net-names.txt as well as
//! for the agents: `ssh`, `scp`, `sftp`, `ssh-add` and `ssh-agent`. The stub notes the call in the net's
//! log and refuses it, and the gate fails when the log has a line.
//!
//! These tests prove the ssh half of that net. Under the gate, which says where its net is
//! (`TAW_AGENT_NET`), a bare `ssh` and a bare `ssh-add` find the net's stubs first and are refused and
//! noted; the notes go to a log of the test's own (`TAW_AGENT_NET_LOG`), so that the gate's stays empty.
//! And, under the gate or not, the net a script makes has a stub for each of the five names, refuses and
//! notes each call, and the list holds exactly those names. Nothing here runs a stub before it has seen
//! that the stub is what the name finds first: each call goes through one function that looks up its own
//! name first, so a name left out of the list and the file cannot run a real program.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::harness::run_bash;
use common::repo_root;

/// The names scripts/ssh-net-names.txt lists, and the only ones it may.
const NAMES: [&str; 5] = ["ssh", "scp", "sftp", "ssh-add", "ssh-agent"];

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
fn the_list_holds_exactly_the_five_ssh_names() {
    let text = std::fs::read_to_string(repo_root().join("scripts/ssh-net-names.txt")).unwrap();
    let names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(names, NAMES);
}

#[test]
fn under_the_gate_a_bare_ssh_and_a_bare_ssh_add_find_the_nets_stubs_and_are_refused() {
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
    for name in ["ssh", "ssh-add"] {
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
            format!("ssh\ttaw-net-guard\t{dir}\t"),
            format!("ssh-add\ttaw-net-guard\t{dir}\t"),
        ]
    );
}

#[test]
fn the_net_has_a_stub_for_each_ssh_name_and_refuses_and_notes_each_call() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/agent-net.sh'\n\
             agent_net_make \"$PWD/net\" || echo failed=make\n\
             for name in {}; do\n\
             \x20 [ \"$(command -v \"$name\")\" = \"$PWD/net/bin/$name\" ] || {{ echo \"not-first=$name\"; exit 9; }}\n\
             done\n\
             call() {{\n\
             \x20 [ \"$(command -v \"$1\")\" = \"$PWD/net/bin/$1\" ] || {{ echo \"not-first=$1\"; exit 9; }}\n\
             \x20 \"$@\"\n\
             }}\n\
             call ssh -o BatchMode=yes studio.local taw; echo ssh=$?\n\
             call scp a b:c; echo scp=$?\n\
             call sftp x; echo sftp=$?\n\
             call ssh-add -l; echo ssh-add=$?\n\
             call ssh-agent -k; echo ssh-agent=$?\n\
             agent_net_check 2> check.err; echo check=$?\n",
            repo_root().display(),
            NAMES.join(" ")
        ),
        &[("TAW_AGENT_NET_LOG", "/no/such/log")],
    );
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        run.stdout, "ssh=127\nscp=127\nsftp=127\nssh-add=127\nssh-agent=127\ncheck=1\n",
        "{}",
        run.stderr
    );
    let said = std::fs::read_to_string(tmp.path().join("check.err")).unwrap();
    for call in [
        format!("ssh\t-o BatchMode=yes studio.local taw\t{dir}\t\n"),
        format!("scp\ta b:c\t{dir}\t\n"),
        format!("sftp\tx\t{dir}\t\n"),
        format!("ssh-add\t-l\t{dir}\t\n"),
        format!("ssh-agent\t-k\t{dir}\t\n"),
    ] {
        assert!(said.contains(&call), "{call:?} is not in:\n{said}");
    }
}

#[test]
fn the_net_names_its_ssh_stub_as_the_ssh_taw_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/agent-net.sh'\n\
             unset TAW_SSH_BIN\n\
             agent_net_make \"$PWD/net\" || echo failed=make\n\
             [ \"$TAW_SSH_BIN\" = \"$PWD/net/bin/ssh\" ] && echo same\n\
             [ -x \"$TAW_SSH_BIN\" ] && echo runs\n",
            repo_root().display()
        ),
        &[],
    );
    assert_eq!(run.stdout, "same\nruns\n", "{}", run.stderr);
}

#[test]
fn a_net_whose_names_file_is_missing_is_not_made() {
    let tmp = tempfile::tempdir().unwrap();
    // The positive half: with its files, the same call makes the net and says 0.
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/agent-net.sh'\nagent_net_make \"$PWD/net-ok\"; echo made=$?\n",
            repo_root().display()
        ),
        &[],
    );
    assert_eq!(run.stdout, "made=0\n", "{}", run.stderr);
    for names in ["SSH_NET_NAMES", "AGENT_NET_NAMES"] {
        let run = run_bash(
            tmp.path(),
            &format!(
                "source '{}/scripts/agent-net.sh'\n\
                 {names}=/no/such/names\n\
                 agent_net_make \"$PWD/net-{names}\" 2>/dev/null; echo made=$?\n",
                repo_root().display()
            ),
            &[],
        );
        assert_eq!(run.stdout, "made=1\n", "{names}: {}", run.stderr);
    }
}

/// The gate-only test above fails, and does not pass by skipping, when it runs in the gate's own tmux
/// directory (`taw-check.*`) with no `TAW_AGENT_NET`, as it would under a scripts/check.sh that had
/// stopped making the net. It is run here as the gate would run it, by this test binary, with the
/// environment so.
#[test]
fn the_gate_only_test_fails_in_the_gates_directory_without_the_net() {
    let tmp = tempfile::tempdir().unwrap();
    let gates = tmp.path().join("taw-check.test");
    std::fs::create_dir_all(&gates).unwrap();
    let run = Command::new(std::env::current_exe().unwrap())
        .args([
            "under_the_gate_a_bare_ssh_and_a_bare_ssh_add_find_the_nets_stubs_and_are_refused",
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
