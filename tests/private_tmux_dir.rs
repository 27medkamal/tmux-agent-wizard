//! A tmux that names no socket reaches nothing of whoever runs the tests.
//!
//! A tmux that is run with neither `-L` nor `-S` goes to the server in `$TMUX` and, with none, to the
//! default socket: the server of the person who runs the suite. A pattern over the sources cannot
//! find every way a command can be spelled, so the scripts that run a suite (the gate, the acceptance
//! harness, the bats run, the sweep) take `scripts/private-tmux-dir.sh` first. It unsets `$TMUX` and
//! puts `$TMUX_TMPDIR` on a fresh short directory of their own, so that such a tmux can start a
//! server only there, and when they are done they look: a default socket is a tmux that named none,
//! and every other file is a server that did not end with its socket.
//!
//! These tests run the library in a bash, and plant, as a regression would, a tmux that names no
//! socket. That tmux is run only when the `$TMUX_TMPDIR` of the environment (not a shell variable of
//! the same name, which no tmux sees) is the directory the library made, the directory is there, and no
//! `$TMUX` is in the environment, which each script checks first: were the library to leave any of that
//! as it was, the planted command would be skipped and the test would fail. And each bash has a
//! `$TMUX_TMPDIR` of its own to begin with (`run_bash`'s decoy), so that even a check gone wrong could
//! not reach the server of whoever runs the tests.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::harness::run_bash;
use common::{LoneProcess, Stop, repo_root, tmux_giving_the_pid, wait_for, write_script};

/// `script`, run in a bash that has the library, with the three lines every test below begins with.
fn with_the_library(script: &str) -> String {
    format!(
        "source '{}/scripts/private-tmux-dir.sh'\n\
         private_tmux_make /tmp/taw-test-tmux || echo failed=make\n\
         sockets=\"$TMUX_TMPDIR/tmux-$(id -u)\"\n\
         # Whether a process is still there, once it has had a moment to go (no pid is no answer: a\n\
         # `kill -0 0` asks about the whole process group).\n\
         gone() {{ [ -n \"$1\" ] || {{ echo unknown; return; }}; local n=0; while kill -0 \"$1\" 2>/dev/null && [ \"$n\" -lt 50 ]; do command sleep 0.1; n=$((n+1)); done; kill -0 \"$1\" 2>/dev/null && echo no || echo yes; }}\n\
         {script}",
        repo_root().display()
    )
}

#[test]
fn a_script_takes_a_directory_of_its_own_and_loses_the_tmux_it_was_run_in() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/private-tmux-dir.sh'\n\
             export TMUX=/outer/socket,1,0 TMUX_PANE=%1 TMUX_TMPDIR=/outer/dir\n\
             private_tmux_make /tmp/taw-test-tmux || echo failed=make\n\
             dir=\"$TMUX_TMPDIR\"\n\
             echo \"tmpdir=$dir\"\n\
             echo \"tmux=${{TMUX-unset}}\"\n\
             echo \"pane=${{TMUX_PANE-unset}}\"\n\
             echo \"is_dir=$([ -d \"$dir\" ] && echo yes || echo no)\"\n\
             private_tmux_down; echo \"status=$?\"\n\
             echo \"gone=$([ -e \"$dir\" ] && echo no || echo yes)\"\n",
            repo_root().display()
        ),
        &[],
    );
    let seen = run.values();

    assert_eq!(seen.get("failed"), None, "{}", run.stderr);
    assert!(
        seen["tmpdir"].starts_with("/tmp/taw-test-tmux."),
        "the directory is not a short private one: {}",
        seen["tmpdir"]
    );
    assert_eq!(seen["is_dir"], "yes");
    assert_eq!(
        seen["tmux"], "unset",
        "the tmux the script was run in stays"
    );
    assert_eq!(seen["pane"], "unset");
    assert_eq!(seen["status"], "0", "nothing was in it: {}", run.stderr);
    assert_eq!(seen["gone"], "yes", "the directory it made was kept");
}

/// A script that runs with no `$TMUX_TMPDIR` (as the gate does, where whoever runs it has none) gets
/// the directory in its environment, for every tmux it runs: a shell variable alone would reach no
/// tmux at all, and they would all go to `/tmp`.
#[test]
fn a_script_with_no_tmux_directory_has_the_one_the_library_makes_in_its_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/private-tmux-dir.sh'\n\
             unset TMUX_TMPDIR\n\
             private_tmux_make /tmp/taw-test-tmux || echo failed=make\n\
             echo \"dir=$TMUX_TMPDIR\"\n\
             echo \"exported=$(printenv TMUX_TMPDIR)\"\n\
             private_tmux_down; echo \"status=$?\"\n",
            repo_root().display()
        ),
        &[],
    );
    let seen = run.values();

    assert_eq!(seen.get("failed"), None, "{}", run.stderr);
    assert!(
        seen["dir"].starts_with("/tmp/taw-test-tmux."),
        "the directory is not a short private one: {}",
        seen["dir"]
    );
    assert_eq!(
        seen["exported"], seen["dir"],
        "the directory is not in the environment, where a tmux would find it"
    );
    assert_eq!(seen["status"], "0", "{}", run.stderr);
}

#[test]
fn a_tmux_that_names_no_socket_is_found_on_the_default_socket_and_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &with_the_library(
            "dir=\"$TMUX_TMPDIR\"\n\
             echo \"dir=$dir\"\n\
             # The planted command: a tmux with neither -L nor -S, run only in the directory the library made.\n\
             case \"$dir\" in /tmp/taw-test-tmux.*) made=yes ;; *) made=no ;; esac\n\
             if [ \"$made\" = yes ] && [ \"$(printenv TMUX_TMPDIR)\" = \"$dir\" ] && [ -d \"$dir\" ] && [ -z \"$(printenv TMUX)\" ]; then\n\
               HISTFILE=/dev/null HOME=\"$dir\" ZDOTDIR=\"$dir\" tmux -f /dev/null new-session -d -s bare sleep 60; echo ran=yes\n\
             else echo ran=no; fi\n\
             echo \"default=$([ -S \"$sockets/default\" ] && echo yes || echo no)\"\n\
             pid=$(tmux -S \"$sockets/default\" display-message -p '#{pid}' 2>/dev/null)\n\
             private_tmux_down; echo \"status=$?\"\n\
             echo \"ended=$(gone \"$pid\")\"\n\
             echo \"gone=$([ -e \"$dir\" ] && echo no || echo yes)\"\n",
        ),
        &[],
    );
    let seen = run.values();

    assert_ne!(
        Path::new(&seen["dir"]),
        run.decoy(),
        "the library did not make a directory of its own"
    );
    assert_eq!(seen["ran"], "yes", "the planted command was skipped");
    assert_eq!(
        seen["default"], "yes",
        "a tmux that names no socket did not start its server on the default socket of the \
         directory"
    );
    assert_eq!(
        seen["status"], "1",
        "a default socket in the directory was not found: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("names no socket") && run.stderr.contains("/default"),
        "the default socket is not named: {}",
        run.stderr
    );
    assert_eq!(
        seen["ended"], "yes",
        "the server it started was left running"
    );
    assert_eq!(seen["gone"], "yes");
}

#[test]
fn a_server_that_was_left_running_is_found_and_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &with_the_library(
            "HISTFILE=/dev/null HOME=\"$TMUX_TMPDIR\" ZDOTDIR=\"$TMUX_TMPDIR\" tmux -L leftover -f /dev/null new-session -d -s one sleep 60\n\
             pid=$(tmux -L leftover display-message -p '#{pid}')\n\
             echo \"names=$(private_tmux_names)\"\n\
             private_tmux_down; echo \"status=$?\"\n\
             echo \"ended=$(gone \"$pid\")\"\n",
        ),
        &[],
    );
    let seen = run.values();

    assert_eq!(seen["names"], "leftover");
    assert_eq!(seen["status"], "1", "{}", run.stderr);
    assert!(
        run.stderr.contains("left running") && run.stderr.contains("/leftover"),
        "the server is not named: {}",
        run.stderr
    );
    assert_eq!(seen["ended"], "yes", "the server was left running");
}

/// A server found left running in the directory is ended by its socket, and then its process group
/// (its jobs), once it has gone: as a group alone, since by then its pid may be another process's. A
/// stand-in tmux first on the script's `PATH` stands for a server there whose pid a lone sleep of the
/// script's has: it answers on the socket until it is told to end, and gives the sleep's pid as its
/// own. No tmux is run.
#[test]
fn a_server_left_running_has_its_group_ended_as_a_group_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let ended = tmp.path().join("ended");
    let lone = tmp.path().join("lone.pid");
    write_script(
        &bin.join("tmux"),
        &format!(
            "case \"$*\" in\n\
               *kill-server*) : > '{ended}' ;;\n\
               *'#{{pid}}'*) cat '{lone}' ;;\n\
               *list-sessions*) [ ! -e '{ended}' ] ;;\n\
               *) exit 1 ;;\n\
             esac\n",
            ended = ended.display(),
            lone = lone.display()
        ),
    );
    let run = run_bash(
        tmp.path(),
        &with_the_library(&format!(
            "sleep 30 & echo $! > '{lone}'\n\
             mkdir -p \"$sockets\" && : > \"$sockets/taw-stand-in\"\n\
             PATH='{bin}':$PATH private_tmux_down; echo \"status=$?\"\n\
             echo \"lone=$(kill -0 \"$(cat '{lone}')\" 2>/dev/null && echo alive || echo ended)\"\n\
             kill \"$(cat '{lone}')\" 2>/dev/null\n",
            lone = lone.display(),
            bin = bin.display()
        )),
        &[],
    );
    let seen = run.values();

    assert_eq!(seen["status"], "1", "{}", run.stderr);
    assert!(
        run.stderr.contains("left running") && ended.exists(),
        "the stand-in was not found running and told to end, so this proves nothing: {}",
        run.stderr
    );
    assert_eq!(
        seen["lone"], "alive",
        "a process that has the pid the server had, and leads no group, was ended: {}",
        run.stderr
    );
}

/// The `bats` that check.sh runs: the one on the `PATH`, or else mise's (`mise x bats@1.11.0`), asked
/// from `dir`, outside the checkout, whose config is the gate's to trust. mise is only asked where
/// bats is: run under `mise x`, bats would have mise's tools first on its `PATH`, a tmux among them,
/// ahead of a stand-in of the test's. With neither, the test fails: one that was skipped would pass
/// whatever the teardown did.
fn the_bats(dir: &Path) -> PathBuf {
    if let Ok(bats) = which::which("bats") {
        return bats;
    }
    let mise = which::which("mise").unwrap_or_else(|_| {
        panic!(
            "neither bats nor mise is on the PATH: `mise install` provides bats, as the gate has it"
        )
    });
    let out = Command::new(mise)
        .args(["x", "bats@1.11.0", "--", "sh", "-c", "command -v bats"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let bats = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success() && !bats.is_empty(),
        "mise has no bats ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(bats)
}

/// The bats teardown ends its server's process group (the plugin's jobs) once the server has gone, as
/// a group alone: by then the server's pid may be another process's, which is not ended. One test of
/// the real plugin.bats is run (its own setup and teardown with it), with a tmux first on its `PATH`
/// that gives the pid of a lone sleep of the test's as the server's when the teardown asks, and
/// passes everything else to the real tmux (`common::tmux_giving_the_pid`). That test loads no plugin,
/// so its server runs no job that would be left without the end of its group.
#[test]
fn the_bats_teardown_hands_its_server_over_as_a_group_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let bats = the_bats(tmp.path());
    let decoy = a_decoy();
    let lone = LoneProcess::start();
    let asked = tmp.path().join("asked.txt");
    let wrap = tmp.path().join("wrap");
    tmux_giving_the_pid(&wrap, lone.pid(), &asked);
    let printed = tmp.path().join("bats.out");

    let mut run = Command::new(bats)
        .arg("-f")
        .arg("^the tmux directory of a test is a private one")
        .arg(repo_root().join("tests").join("bats").join("plugin.bats"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                wrap.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TMUX_TMPDIR", decoy.path())
        .env("HISTFILE", "/dev/null")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(std::fs::File::create(tmp.path().join("bats.err")).unwrap())
        .spawn()
        .unwrap();
    let ended = wait_for(
        || matches!(run.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    if !ended {
        let _ = run.kill();
    }
    let status = run.wait().unwrap();
    let survived = lone.alive();
    let said = std::fs::read_to_string(&printed).unwrap_or_default()
        + &std::fs::read_to_string(tmp.path().join("bats.err")).unwrap_or_default();

    assert!(
        std::fs::read_to_string(&asked).is_ok_and(|asked| !asked.is_empty()),
        "the teardown never asked its server's pid, so this proves nothing ({status}): {said}"
    );
    assert!(
        survived,
        "the teardown ended a process that has a pid its server had, which leads no group: {said}"
    );
    assert!(
        ended && status.success() && said.contains("1..1"),
        "the one test of plugin.bats did not run and pass ({status}): {said}"
    );
}

#[test]
fn a_socket_file_that_was_left_behind_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &with_the_library(
            "HISTFILE=/dev/null HOME=\"$TMUX_TMPDIR\" ZDOTDIR=\"$TMUX_TMPDIR\" tmux -L dead -f /dev/null new-session -d -s one sleep 60\n\
             tmux -L dead kill-server\n\
             n=0; while tmux -L dead list-sessions >/dev/null 2>&1 && [ \"$n\" -lt 50 ]; do command sleep 0.1; n=$((n+1)); done\n\
             echo \"names=$(private_tmux_names)\"\n\
             private_tmux_down; echo \"status=$?\"\n",
        ),
        &[],
    );
    let seen = run.values();

    assert_eq!(
        seen["names"], "dead",
        "tmux took its socket file with the server, so this proves nothing"
    );
    assert_eq!(seen["status"], "1", "{}", run.stderr);
    assert!(
        run.stderr.contains("left behind") && run.stderr.contains("/dead"),
        "the file is not named: {}",
        run.stderr
    );
}

/// The gate, `scripts/check.sh`, started with the stand-ins in `tmp/bin` first on its `PATH` and a
/// decoy `$TMUX_TMPDIR` of the test's own (were the gate not to make a directory of its own, its tmux
/// would reach no server of anybody's). What it prints goes to `tmp/gate.out` and `tmp/gate.err`
/// (`run_bash` says why files and not pipes).
fn start_the_gate(tmp: &Path, decoy: &Path) -> Child {
    start_the_gate_with(tmp, decoy, "bash", &[])
}

/// [`start_the_gate`] under `shell` (`bash` on the `PATH`, or `/bin/bash`, which is bash 3.2 on a
/// Mac), with `env` added.
fn start_the_gate_with(tmp: &Path, decoy: &Path, shell: &str, env: &[(&str, &Path)]) -> Child {
    let path = format!(
        "{}:{}",
        tmp.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(shell)
        .arg(repo_root().join("scripts").join("check.sh"))
        .env("PATH", path)
        .env("TMUX_TMPDIR", decoy)
        .envs(env.iter().copied())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(tmp.join("gate.out")).unwrap())
        .stderr(std::fs::File::create(tmp.join("gate.err")).unwrap())
        .spawn()
        .unwrap()
}

fn a_decoy() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("taw-decoy-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Whether a process is there (never asked of 0 or 1, which would be this process's own group, or
/// every process).
fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: `kill` takes two integers and touches no memory of this process; signal 0 sends nothing.
    pid > 1 && unsafe { libc::kill(pid, 0) } == 0
}

/// A trap set before private_tmux_make, which takes the directory down, finds it named however soon
/// after it is made a signal comes: its name goes into `PRIVATE_TMUX_DIR` in the very command that
/// makes it. A DEBUG trap from `$BASH_ENV` sends the script TERM just before the first command that
/// runs once the directory is there (an EXIT trap runs on a TERM that no trap of its own answers).
#[test]
fn a_signal_right_after_the_directory_is_made_finds_it_named() {
    let tmp = tempfile::tempdir().unwrap();
    let prefix = tmp.path().join("made");
    let bash_env = tmp.path().join("stop.sh");
    std::fs::write(
        &bash_env,
        format!(
            "set -T\n\
             __prefix='{}'\n\
             trap 'if [ -z \"${{__stopped:-}}\" ]; then for __made in \"$__prefix\".*; do if [ -e \"$__made\" ]; then __stopped=1; kill -TERM $$; fi; break; done; fi' DEBUG\n",
            prefix.display()
        ),
    )
    .unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/private-tmux-dir.sh'\n\
             trap 'private_tmux_down > /dev/null' EXIT\n\
             private_tmux_make '{}'\n\
             echo made=all\n",
            repo_root().display(),
            prefix.display()
        ),
        &[("BASH_ENV", bash_env.to_str().unwrap())],
    );
    let left: Vec<PathBuf> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("made."))
        })
        .collect();

    assert_eq!(
        run.values().get("made"),
        None,
        "the script was not stopped once the directory was there, so this proves nothing: {}",
        run.stderr
    );
    assert_eq!(
        left,
        Vec::<PathBuf>::new(),
        "a signal right after the directory was made left it behind: {}",
        run.stderr
    );
}

/// `-<pid>` is a process group alone: what a script hands over for a leader that has ended and been
/// waited for, whose pid may be another process's by now. A process that leads no group is not ended
/// by that form, and is by the plain one; a group is ended by either.
#[test]
fn a_pid_given_as_a_group_alone_never_ends_a_process_that_leads_none() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &with_the_library(
            "sleep 30 & lone=$!\n\
             set -m; sleep 30 & leader=$!; set +m\n\
             echo \"leads=$(ps -o pgid= -p \"$leader\" | tr -d ' ')\"\n\
             echo \"leader=$leader\"\n\
             private_tmux_end \"-$lone\"\n\
             echo \"lone_as_a_group=$(kill -0 \"$lone\" 2>/dev/null && echo alive || echo ended)\"\n\
             private_tmux_end \"-$leader\"\n\
             echo \"group_as_a_group=$(gone \"$leader\")\"\n\
             private_tmux_end \"$lone\"\n\
             echo \"lone_as_a_pid=$(gone \"$lone\")\"\n\
             kill \"$lone\" \"$leader\" 2>/dev/null\n\
             private_tmux_down; echo \"status=$?\"\n",
        ),
        &[],
    );
    let seen = run.values();

    assert_eq!(
        seen["leads"], seen["leader"],
        "the second sleep leads no group of its own, so this proves nothing: {}",
        run.stderr
    );
    assert_eq!(
        seen["lone_as_a_group"], "alive",
        "a process that leads no group was ended by the form for a group alone"
    );
    assert_eq!(
        seen["group_as_a_group"], "yes",
        "a group was not ended by the form for a group alone"
    );
    assert_eq!(
        seen["lone_as_a_pid"], "yes",
        "a process was not ended by its pid"
    );
    assert_eq!(seen["status"], "0", "{}", run.stderr);
}

/// A group that private_tmux_left cannot end keeps the directory: private_tmux_down tries it again, and
/// keeps the directory, saying so, while it is there. The ending is a stand-in here that ends nothing
/// and notes what it was asked (a process that outlives a KILL cannot be made to order); the test ends
/// the group itself after.
#[test]
fn a_group_left_running_that_could_not_be_ended_keeps_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &with_the_library(
            "dir=\"$TMUX_TMPDIR\"\n\
             set -m; sh -c 'sleep 30 & exit 0' & step=$!; set +m\n\
             wait \"$step\"\n\
             private_tmux_end() { [ \"$#\" -gt 0 ] || return 0; asked=\"$asked $*\"; return 1; }\n\
             private_tmux_left \"$step\" 'the step'; echo \"left=$?\"\n\
             private_tmux_down; echo \"down=$?\"\n\
             echo \"asked=$asked\"\n\
             echo \"kept=$([ -d \"$dir\" ] && echo yes || echo no)\"\n\
             echo \"step=$step\"\n\
             command kill -KILL -- \"-$step\" 2>/dev/null\n\
             rm -rf \"$dir\"\n",
        ),
        &[],
    );
    let seen = run.values();
    let group = format!("-{}", seen["step"]);

    assert_eq!(
        seen["left"], "1",
        "nothing was found left in the step's group: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("sleep 30"),
        "what was left is not named: {}",
        run.stderr
    );
    assert_eq!(
        seen["asked"].split_whitespace().collect::<Vec<_>>(),
        [group.as_str(), group.as_str()],
        "the group was not asked to end, as a group alone, by private_tmux_left and again by \
         private_tmux_down"
    );
    assert_eq!(
        seen["kept"], "yes",
        "the directory went while the group was there"
    );
    assert_eq!(seen["down"], "1");
    assert!(
        run.stderr.contains("kept the tmux directory"),
        "the directory was kept without a word: {}",
        run.stderr
    );
}

/// The gate removes its tmux directory only once the step it runs has ended: tmux takes /tmp for a
/// `$TMUX_TMPDIR` that is gone, so a step that ran on after it would have no net at all, and a signal
/// to the gate's shell alone leaves its child running. A stand-in `cargo`, first on the gate's `PATH`,
/// starts a child of its own (as cargo starts the test binaries), notes both pids and the directory it
/// runs with, and waits; the gate's shell is sent TERM; then the step and its child are gone, and so is
/// the directory, or the gate said that it kept it. Whatever happened, both are ended at the end by
/// their pids, so that a failure leaves nothing running.
#[test]
fn the_gate_told_to_stop_mid_step_ends_the_step_before_its_tmux_directory_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let decoy = a_decoy();
    let noted = tmp.path().join("step.txt");
    write_script(
        &tmp.path().join("bin").join("cargo"),
        &format!(
            "sleep 600 &\nprintf '%s %s %s\\n' \"$$\" \"$!\" \"$TMUX_TMPDIR\" > '{0}.part' && mv '{0}.part' '{0}'\nwait\n",
            noted.display()
        ),
    );
    let mut gate = start_the_gate(tmp.path(), decoy.path());
    let started = wait_for(|| noted.exists(), Duration::from_secs(60));
    let text = std::fs::read_to_string(&noted).unwrap_or_default();
    let words: Vec<&str> = text.split_whitespace().collect();
    let pids: Vec<libc::pid_t> = words
        .iter()
        .take(2)
        .map(|pid| pid.parse().unwrap_or(0))
        .collect();
    let dir = words.get(2).copied().unwrap_or("");

    // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the gate's
    // shell, a child of this test.
    unsafe { libc::kill(gate.id() as libc::pid_t, libc::SIGTERM) };
    let gate_ended = wait_for(
        || matches!(gate.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    let step_ended = wait_for(
        || pids.iter().all(|pid| !alive(*pid)),
        Duration::from_secs(5),
    );
    if !gate_ended {
        let _ = gate.kill();
    }
    let _ = gate.wait();
    for pid in pids.iter().filter(|pid| alive(**pid)) {
        // SAFETY: as above; the pids are the stand-in's and its child's, which it wrote itself.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    let said = std::fs::read_to_string(tmp.path().join("gate.err")).unwrap_or_default();

    assert!(started && pids.len() == 2, "the step never started: {said}");
    assert!(
        dir.starts_with("/tmp/taw-check.") && Path::new(dir) != decoy.path(),
        "the step did not run in the gate's own tmux directory: {dir:?}"
    );
    assert!(gate_ended, "the gate did not stop when it was told to");
    assert!(
        step_ended,
        "the step, or the child it started, ran on after the gate was told to stop: {said}"
    );
    assert!(
        !Path::new(dir).exists() || said.contains("kept the tmux directory"),
        "the gate kept its tmux directory and did not say so: {said}"
    );
    assert!(
        !said.contains("Terminated"),
        "bash said a line of its own about the step it ended: {said}"
    );
}

/// A step that a signal from outside ends (its leader alone, as a test runner's timeout would)
/// fails, and what it left in its group is named and ended; the gate goes on to the next steps. And
/// bash, whose job the step is, says nothing of its own about it ("[1]+ Terminated ..."): the gate
/// says what failed itself. The stand-in `cargo` waits in its fmt step with a child of its own, and
/// passes every other step.
#[test]
fn a_step_a_signal_ended_fails_with_what_it_left_and_no_word_from_bash() {
    let tmp = tempfile::tempdir().unwrap();
    let decoy = a_decoy();
    let noted = tmp.path().join("step.txt");
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("cargo"),
        &format!(
            "[ \"$1\" = fmt ] || exit 0\n\
             sleep 600 &\n\
             printf '%s %s\\n' \"$$\" \"$!\" > '{0}.part' && mv '{0}.part' '{0}'\n\
             wait\n",
            noted.display()
        ),
    );
    write_script(&bin.join("shellcheck"), "exit 0\n");
    write_script(&bin.join("bats"), "exit 0\n");

    let mut gate = start_the_gate(tmp.path(), decoy.path());
    let started = wait_for(|| noted.exists(), Duration::from_secs(60));
    let text = std::fs::read_to_string(&noted).unwrap_or_default();
    let pids: Vec<libc::pid_t> = text
        .split_whitespace()
        .map(|pid| pid.parse().unwrap_or(0))
        .collect();
    if let Some(&step) = pids.first().filter(|pid| **pid > 1) {
        // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the
        // stand-in's, which it wrote itself, and is more than 1.
        unsafe { libc::kill(step, libc::SIGTERM) };
    }
    let ended = wait_for(
        || matches!(gate.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    if !ended {
        let _ = gate.kill();
    }
    let status = gate.wait().unwrap();
    let child_ran_on = pids.get(1).is_some_and(|pid| alive(*pid));
    for pid in pids.iter().filter(|pid| alive(**pid)) {
        // SAFETY: as above; the pids are the stand-in's and its child's, which it wrote itself.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    let said = std::fs::read_to_string(tmp.path().join("gate.err")).unwrap_or_default();

    assert!(started && pids.len() == 2, "the step never started: {said}");
    assert!(ended, "the gate did not end: {said}");
    assert!(
        !status.success() && said.contains("FAILED: cargo fmt --check"),
        "the step that a signal ended did not fail the gate: {said}"
    );
    assert!(
        said.contains("sleep 600") && !child_ran_on,
        "what the step left was not named, or ran on: {said}"
    );
    assert!(
        !said.contains("Terminated"),
        "bash said a line of its own about the step a signal ended: {said}"
    );
}

/// The gate, told to stop at one moment of its run: just before the command that begins with
/// `stop_at`, a DEBUG trap from `$BASH_ENV` sends it TERM, once ([`Stop`]), which is where a signal that
/// lands at that moment is handled. Under `shell`, with the usual stand-ins: the stand-in cargo looks
/// for the gate's directory every hundredth of a second for three seconds, and notes it if it ever
/// finds it gone. What the gate said, how it ended, and what came of the step and the directory;
/// whatever happened, the step is ended by its pid at the end, and a directory the gate kept is
/// removed.
struct StoppedGate {
    status: std::process::ExitStatus,
    said: String,
    /// Whether the gate got to the command at all.
    stopped: bool,
    /// The gate's `$TMUX_TMPDIR` when it was stopped: its own directory, once it is made.
    dir: String,
    /// `$!` when the gate was stopped: the step it had just forked, or 0 for none.
    step: libc::pid_t,
    step_ran_on: bool,
    saw_its_directory_gone: bool,
    /// Whether the gate's own tmux directory was still there once it had ended.
    kept: bool,
}

fn the_gate_stopped_at(shell: &str, stop_at: &str) -> StoppedGate {
    let tmp = tempfile::tempdir().unwrap();
    let decoy = a_decoy();
    let gone = tmp.path().join("saw-the-directory-gone");
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("cargo"),
        &format!(
            "[ \"$1\" = fmt ] || exit 0\n\
             n=0\n\
             while [ \"$n\" -lt 300 ]; do\n\
               [ -d \"$TMUX_TMPDIR\" ] || {{ echo gone > '{gone}'; exit 0; }}\n\
               sleep 0.01; n=$((n + 1))\n\
             done\n",
            gone = gone.display()
        ),
    );
    write_script(&bin.join("shellcheck"), "exit 0\n");
    write_script(&bin.join("bats"), "exit 0\n");
    let stop = Stop::at(tmp.path(), stop_at);

    let mut gate = start_the_gate_with(
        tmp.path(),
        decoy.path(),
        shell,
        &[("BASH_ENV", stop.bash_env.as_path())],
    );
    let ended = wait_for(
        || matches!(gate.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    if !ended {
        let _ = gate.kill();
    }
    let status = gate.wait().unwrap();
    let noted = stop.noted();
    let step = noted.as_ref().map_or(0, |noted| noted.last_background_pid);
    // A step that ran on looks for the directory for up to three seconds, and leaves as soon as it
    // finds it gone.
    let step_ran_on = alive(step) && !wait_for(|| !alive(step), Duration::from_secs(4));
    if alive(step) {
        // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the
        // step's, which the gate's shell noted, and is more than 1.
        unsafe { libc::kill(step, libc::SIGKILL) };
    }
    let said = std::fs::read_to_string(tmp.path().join("gate.err")).unwrap_or_default();
    let dir = noted
        .as_ref()
        .map(|noted| noted.tmux_tmpdir.clone())
        .unwrap_or_default();
    let kept = dir.starts_with("/tmp/taw-check.") && Path::new(&dir).exists();
    if kept {
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(ended, "the gate did not end under {shell}: {said}");
    StoppedGate {
        status,
        said,
        stopped: noted.is_some(),
        dir,
        step,
        step_ran_on,
        saw_its_directory_gone: gone.exists(),
        kept,
    }
}

/// The gate told to stop at any moment of its start leaves no tmux directory behind. Its traps are set
/// before the directory is made, which is named in the command that makes it: a signal that came
/// between the two would end the gate outright (no trap of its own yet), with nothing to take the
/// directory down. Stopped before the traps are all set (no directory yet, or, before, one made) and
/// once the directory is made (taken down), under bash 5 and bash 3.2.
#[test]
fn the_gate_told_to_stop_while_it_starts_leaves_no_tmux_directory() {
    for shell in ["bash", "/bin/bash"] {
        for stop_at in ["step=\"\"", "trap 'exit 143' TERM", "status=0"] {
            let stopped = the_gate_stopped_at(shell, stop_at);
            let said = &stopped.said;

            assert!(
                stopped.stopped,
                "the gate under {shell} never got to {stop_at:?}: {said}"
            );
            assert!(
                !stopped.kept,
                "the gate under {shell}, stopped at {stop_at:?}, left its tmux directory behind: {said}"
            );
            if stop_at == "status=0" {
                assert!(
                    stopped.dir.starts_with("/tmp/taw-check."),
                    "the gate under {shell} had made no directory of its own by status=0, so this \
                     proves nothing: {said}"
                );
            }
        }
    }
}

/// A signal that lands while bash forks a step is handled before `step=$!`: the EXIT trap has the new
/// step in `$!` and not yet in `step`. It still ends that step before the directory goes, under bash
/// 5 and under bash 3.2. It must not remove its directory then and leave the step running
/// without it (a race that showed in 6 runs in 300).
#[test]
fn a_signal_while_the_gate_forks_a_step_ends_that_step_before_the_directory_goes() {
    for shell in ["bash", "/bin/bash"] {
        let stopped = the_gate_stopped_at(shell, "step=$!");
        let said = &stopped.said;

        assert!(
            stopped.dir.starts_with("/tmp/taw-check."),
            "the gate under {shell} was not stopped in its own directory, so this proves nothing: {said}"
        );
        assert!(
            stopped.step > 1,
            "the gate under {shell} had forked no step when it was stopped, so this proves nothing: \
             {said}"
        );
        assert_eq!(
            stopped.status.code(),
            Some(143),
            "the gate under {shell} did not exit as told to stop: {said}"
        );
        assert!(
            !stopped.saw_its_directory_gone,
            "under {shell}, the step the gate had just started found the gate's tmux directory gone: \
             {said}"
        );
        assert!(
            !stopped.step_ran_on,
            "under {shell}, the step the gate had just started ran on after the gate: {said}"
        );
        assert!(
            !stopped.kept || said.contains("kept the tmux directory"),
            "the gate under {shell} kept its tmux directory and did not say so: {said}"
        );
    }
}

/// The same signal a moment earlier, before bash forks the gate's first step: `$!` names nothing yet,
/// and the EXIT trap, which reads it, must not stop on that under `set -u` (bash 5 and bash 3.2 both
/// say "$!: unbound variable" and leave the trap there, with the directory still in place).
#[test]
fn a_signal_just_before_the_first_step_is_forked_leaves_nothing() {
    for shell in ["bash", "/bin/bash"] {
        let stopped = the_gate_stopped_at(shell, "\"$@\" < /dev/null");
        let said = &stopped.said;

        assert!(
            stopped.dir.starts_with("/tmp/taw-check."),
            "the gate under {shell} was not stopped in its own directory, so this proves nothing: {said}"
        );
        assert_eq!(
            stopped.step, 0,
            "the gate under {shell} had forked a step when it was stopped, so this proves nothing: \
             {said}"
        );
        assert_eq!(
            stopped.status.code(),
            Some(143),
            "the gate under {shell} did not exit as told to stop: {said}"
        );
        assert!(
            !said.contains("unbound variable"),
            "the gate's EXIT trap under {shell} stopped on a variable: {said}"
        );
        assert!(
            !stopped.kept,
            "the gate under {shell} left its tmux directory behind: {said}"
        );
    }
}

/// What a step left running when it ended, in its process group (a child of a test that nothing
/// waited for, which cargo leaves where it is when it exits), would run on after the gate's tmux
/// directory went, with no net: tmux takes /tmp for a `$TMUX_TMPDIR` that is gone. The gate must
/// not end with a pass and not a word about it. It ends it as soon as the step is over, says what
/// it was, and fails. Here the stand-in `cargo` leaves a script behind in its test step, which
/// notes its pid and the directory it runs with, and looks for that directory every tenth of a
/// second for half a minute, noting it if it ever finds it gone; the other tools are stand-ins that
/// pass at once. Whatever happened, the script is ended at the end by its pid, so that a failure
/// leaves nothing running.
#[test]
fn the_gate_ends_what_a_finished_step_left_running_says_what_it_was_and_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let decoy = a_decoy();
    let left = tmp.path().join("left-behind.sh");
    let noted = tmp.path().join("left-behind.txt");
    let saw_it_gone = tmp.path().join("saw-the-directory-gone");
    write_script(
        &left,
        &format!(
            "printf '%s %s\\n' \"$$\" \"$TMUX_TMPDIR\" > '{noted}.part' && mv '{noted}.part' '{noted}'\n\
             n=0\n\
             while [ \"$n\" -lt 300 ]; do\n\
               [ -d \"$TMUX_TMPDIR\" ] || {{ echo gone > '{gone}'; exit 0; }}\n\
               sleep 0.1; n=$((n + 1))\n\
             done\n",
            noted = noted.display(),
            gone = saw_it_gone.display()
        ),
    );
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("cargo"),
        &format!(
            "[ \"$1\" = test ] || exit 0\n\
             '{left}' &\n\
             n=0; while [ ! -e '{noted}' ] && [ \"$n\" -lt 100 ]; do sleep 0.1; n=$((n + 1)); done\n",
            left = left.display(),
            noted = noted.display()
        ),
    );
    write_script(&bin.join("shellcheck"), "exit 0\n");
    write_script(&bin.join("bats"), "exit 0\n");

    let mut gate = start_the_gate(tmp.path(), decoy.path());
    let ended = wait_for(
        || matches!(gate.try_wait(), Ok(Some(_))),
        Duration::from_secs(60),
    );
    if !ended {
        let _ = gate.kill();
    }
    let status = gate.wait().unwrap();
    let text = std::fs::read_to_string(&noted).unwrap_or_default();
    let mut words = text.split_whitespace();
    let pid: libc::pid_t = words.next().and_then(|pid| pid.parse().ok()).unwrap_or(0);
    let dir = words.next().unwrap_or("").to_string();
    let still_running = alive(pid);
    if still_running {
        // SAFETY: `kill` takes two integers and touches no memory of this process; the pid is the
        // script's, which it wrote itself, and is more than 1.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let said = std::fs::read_to_string(tmp.path().join("gate.err")).unwrap_or_default();

    assert!(ended, "the gate did not end: {said}");
    assert!(
        pid > 1,
        "the step left nothing running, so this proves nothing: {said}"
    );
    assert!(
        dir.starts_with("/tmp/taw-check.") && Path::new(&dir) != decoy.path(),
        "what the step left did not run in the gate's own tmux directory: {dir:?}"
    );
    assert!(
        !status.success(),
        "the gate passed although a step left something running: {said}"
    );
    assert!(
        said.contains("left-behind.sh"),
        "the gate did not say what the step left running: {said}"
    );
    assert!(
        !still_running,
        "what the step left was still running when the gate ended: {said}"
    );
    assert!(
        !saw_it_gone.exists(),
        "the gate's tmux directory went while what the step left was still running: {said}"
    );
    assert!(
        !Path::new(&dir).exists(),
        "the gate kept its tmux directory: {said}"
    );
}
