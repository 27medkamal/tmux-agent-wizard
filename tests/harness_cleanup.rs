//! What a run of the shell harness leaves behind: nothing it started and did not wait for, and no
//! tmux server it did not mean to start.
//!
//! The acceptance run attaches a client from outside tmux in a terminal that `script` makes, whose
//! input is held open by a `cat` of a fifo (`e2e_terminal`, in tests/e2e/harness.sh). Nothing ever
//! ended that `cat`: it was started inside a shell that held the fifo open for writing, so it held
//! it open too and could never see the end of the file, and the run never knew its pid. A run that
//! leaves them leaves three behind for good (ppid 1, doing nothing, a day old and more), and they
//! pile up: 192 of them on one machine.
//!
//! Two things end it, and each is shown on its own here. The `cat` has no descriptor of the
//! fifo's but its own, so that it leaves by itself when the run lets go; and the run notes its pid
//! (`e2e_track`) for `e2e_down`, which ends it, pass or fail, if the process still has the command
//! line it was noted with and not otherwise (a pid that has since been given to some other process
//! is none of this run's). None of that starts a tmux server: the harness is sourced and not brought
//! up.
//!
//! A test after that brings it up. A run has a tmux socket directory of its own
//! (`scripts/private-tmux-dir.sh`), so that a tmux that names no socket, which would reach the server
//! of whoever runs it, starts a server of its own there instead, and `e2e_down` finds it (and ends
//! it, and removes the directory). That tmux is planted, as a regression would run one, and run only
//! when the environment has no `$TMUX` and its `$TMUX_TMPDIR` is the directory the harness made, which
//! is there: were any of that not so, the command would be skipped, the test would fail, and the
//! server of whoever runs the tests would still be untouched (the script's `$TMUX_TMPDIR` is a base of
//! the test's own to begin with).
//!
//! Every script here runs in a tmux base of the test's own ([`TmuxBase`]): its `$TMUX_TMPDIR`, and the
//! place where `e2e_up` makes the run's directory. A run that died before its `e2e_down` (stopped on an
//! error, or killed) left its two servers up for good, on the default socket directory where nothing
//! else was there to keep them off it; six were found so, days later. Its EXIT trap runs `e2e_down`
//! now, whatever happens, and the base ends what a run killed outright left in it.

mod common;

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::harness::{BashRun, TmuxBase, harness_up, run_bash};
use common::shell_env::ShellHome;
use common::{Server, Stop, repo_root, taw_bin, wait_for, write_script};
use taw::tmux::{Socket, Tmux};

/// Runs `body` in a bash that has sourced the harness, in `tmp` as the run's temp directory and `base`
/// for its tmux, and returns what it printed and how it ended. The shell runs `e2e_down` as it exits,
/// whatever happens (that ends whatever was noted with `e2e_track` too, and redoes nothing `body` did
/// already), so that a failure leaves nothing behind. (`run_bash` says why what it prints goes to
/// files.)
fn in_the_harness(base: &TmuxBase, tmp: &Path, body: &str) -> BashRun {
    let script = format!(
        "source '{}/tests/e2e/harness.sh'\n\
         trap 'e2e_down > /dev/null 2>&1' EXIT\n\
         fifo=\"$E2E_TMP/tty-in\"\n\
         # How many processes are `cat <fifo>`, once there are $1 of them or five seconds are up.\n\
         wait_count() {{ local n=0 want=\"$1\"; while [ \"$(e2e_running \"cat $fifo\")\" -ne \"$want\" ] && [ \"$n\" -lt 50 ]; do command sleep 0.1; n=$((n+1)); done; e2e_running \"cat $fifo\"; }}\n\
         {body}",
        repo_root().display()
    );
    let mut env = vec![
        ("E2E_TMP", tmp.display().to_string()),
        ("TAW_BIN", taw_bin().to_string()),
    ];
    env.extend(base.env());
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let run = run_bash(tmp, &script, &env);
    assert!(
        run.status.success(),
        "the script failed ({}): {}{}",
        run.status,
        run.stdout,
        run.stderr
    );
    run
}

#[test]
fn the_cat_of_a_terminal_leaves_by_itself_when_the_run_lets_go_of_the_fifo() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let seen = in_the_harness(
        &base,
        tmp.path(),
        "mkfifo \"$fifo\"; exec 7<>\"$fifo\"\n\
         e2e_terminal \"$fifo\" sleep 5 || echo failed=terminal\n\
         echo \"up=$(wait_count 1)\"\n\
         exec 7>&-\n\
         echo \"left=$(wait_count 0)\"\n\
         kill \"$E2E_TERM_PID\" 2>/dev/null || true\n",
    )
    .values();

    assert_eq!(
        seen.get("failed"),
        None,
        "e2e_terminal did not make its terminal"
    );
    assert_eq!(
        seen["up"], "1",
        "the terminal has one `cat` of the fifo while the run holds it"
    );
    assert_eq!(
        seen["left"], "0",
        "the `cat` was still there five seconds after the run let go of the fifo: it holds the \
         fifo open itself"
    );
}

#[test]
fn e2e_down_ends_the_cat_of_a_terminal_whose_fifo_is_still_held() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let seen = in_the_harness(
        &base,
        tmp.path(),
        "mkfifo \"$fifo\"; exec 7<>\"$fifo\"\n\
         e2e_terminal \"$fifo\" sleep 5 || echo failed=terminal\n\
         echo \"up=$(wait_count 1)\"\n\
         e2e_down > /dev/null\n\
         echo \"left=$(wait_count 0)\"\n\
         kill \"$E2E_TERM_PID\" 2>/dev/null || true\n",
    )
    .values();

    assert_eq!(
        seen.get("failed"),
        None,
        "e2e_terminal did not make its terminal"
    );
    assert_eq!(seen["up"], "1", "the terminal has one `cat` of the fifo");
    assert_eq!(
        seen["left"], "0",
        "e2e_down left the `cat` of the terminal running, with the fifo still held by the run"
    );
}

#[test]
fn e2e_down_ends_only_a_process_that_still_has_the_command_line_it_was_noted_with() {
    let tmp = tempfile::tempdir().unwrap();
    // Two processes of the script's own (the lengths of the sleeps are theirs: nothing here goes
    // by a name). One is noted with the command line it has, and one with a line that is not its
    // own, the way a pid that has been given to another process is.
    let base = TmuxBase::new();
    let seen = in_the_harness(
        &base,
        tmp.path(),
        "sleep 611 & noted=$!\n\
         sleep 612 & other=$!\n\
         trap 'kill \"$noted\" \"$other\" 2>/dev/null' EXIT\n\
         e2e_track \"$noted\" 'sleep 611'\n\
         e2e_track \"$other\" 'cat /not/this/one'\n\
         e2e_down > /dev/null\n\
         alive() { [ \"$(e2e_command_of \"$1\")\" = \"$2\" ] && echo yes || echo no; }\n\
         n=0; while [ \"$(alive \"$noted\" 'sleep 611')\" = yes ] && [ \"$n\" -lt 50 ]; do command sleep 0.1; n=$((n+1)); done\n\
         echo \"noted=$(alive \"$noted\" 'sleep 611')\"\n\
         echo \"other=$(alive \"$other\" 'sleep 612')\"\n",
    )
    .values();

    assert_eq!(
        seen["noted"], "no",
        "the process noted with its own command line was not ended"
    );
    assert_eq!(
        seen["other"], "yes",
        "a process whose command line is not the one it was noted with was ended"
    );
}

#[test]
fn a_run_makes_a_tmux_directory_of_its_own_and_finds_a_tmux_that_named_no_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let run = in_the_harness(
        &base,
        tmp.path(),
        "e2e_up net > /dev/null || echo failed=up\n\
         dir=\"$TMUX_TMPDIR\"\n\
         echo \"dir=$dir\"\n\
         case \"$dir\" in \"$E2E_TMUX_BASE\"/taw-e2e-tmux.*) made=yes ;; *) made=no ;; esac\n\
         if [ \"$made\" = yes ] && [ \"$(printenv TMUX_TMPDIR)\" = \"$dir\" ] && [ -d \"$dir\" ] && [ -z \"$(printenv TMUX)\" ]; then\n\
           HISTFILE=/dev/null HOME=\"$dir\" ZDOTDIR=\"$dir\" tmux -f /dev/null new-session -d -s bare sleep 60; echo ran=yes\n\
         else echo ran=no; fi\n\
         echo \"names=$(private_tmux_names | tr '\\n' ' ')\"\n\
         pid=$(tmux -S \"$dir/tmux-$(id -u)/default\" display-message -p '#{pid}' 2>/dev/null); echo \"pid=$pid\"\n\
         e2e_down > /dev/null\n\
         echo \"gone=$([ -e \"$dir\" ] && echo no || echo yes)\"\n\
         n=0; while [ -n \"$pid\" ] && kill -0 \"$pid\" 2>/dev/null && [ \"$n\" -lt 50 ]; do command sleep 0.1; n=$((n+1)); done\n\
         echo \"ended=$([ -n \"$pid\" ] && kill -0 \"$pid\" 2>/dev/null && echo no || echo yes)\"\n",
    );
    let seen = run.values();

    assert_eq!(seen.get("failed"), None, "the harness did not come up");
    assert_ne!(
        Path::new(&seen["dir"]),
        base.tmux_dir(),
        "the harness did not make a directory of its own"
    );
    assert!(
        seen["dir"].starts_with(&format!("{}/taw-e2e-tmux.", base.path().display())),
        "the run's tmux directory is not a short private one: {}",
        seen["dir"]
    );
    assert_eq!(seen["ran"], "yes", "the planted command was skipped");
    assert!(!seen["pid"].is_empty(), "the planted server said no pid");
    assert!(
        seen["names"].starts_with("default "),
        "a tmux that names no socket did not start its server on the default socket of the run's \
         directory: {}",
        seen["names"]
    );
    assert!(
        run.stderr.contains("names no socket"),
        "e2e_down did not say that a tmux named no socket: {}",
        run.stderr
    );
    assert_eq!(
        seen["ended"], "yes",
        "the server it started was left running"
    );
    assert_eq!(seen["gone"], "yes", "the directory it made was kept");
}

/// A run that crashed leaves its tmux socket directory behind (the gate's, the harness's, a bats
/// test's, the sweep's: scripts/private-tmux-dir.sh), out of reach of the default directory that
/// tests/e2e/cleanup.sh sweeps. It looks in them too, and takes away only what is dead: a socket that
/// refuses a connection. A socket that a server answers on is a run still going, perhaps somebody
/// else's gate: it is named and left alone, never killed. And it removes no directory, empty or not:
/// a run that is going has none of its servers up at times (the gate's, while it formats and lints),
/// and tmux takes /tmp for a `$TMUX_TMPDIR` that is gone, which would turn that run's net off.
///
/// The directories are in a base of the test's own (`$TAW_CLEANUP_BASE`), and the default socket
/// directory the script sweeps first is an empty one of the test's own too (`$TMUX_TMPDIR`, made
/// here), so that there is nothing else for the script to look at. It runs under the `bash` on the
/// `PATH` and under `/bin/bash`, which is bash 3.2 on a Mac (and stops at an empty array under `set -u`).
#[test]
fn cleanup_takes_away_dead_sockets_in_the_private_tmux_directories_and_nothing_else() {
    for shell in ["bash", "/bin/bash"] {
        sweep_dead_sockets_with(shell);
    }
}

fn sweep_dead_sockets_with(shell: &str) {
    let base = tempfile::Builder::new()
        .prefix("taw-hc-")
        .tempdir_in("/tmp")
        .unwrap();
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let sockets_of = |private: &str| {
        base.path()
            .join(private)
            .join(format!("tmux-{}", uid.trim()))
    };
    let (dead, live, empty, library) = (
        sockets_of("taw-check.dead01"),
        sockets_of("taw-e2e-tmux.live01"),
        sockets_of("taw-check.empty1"),
        sockets_of("taw-test-tmux.dead02"),
    );
    std::fs::create_dir_all(&dead).unwrap();
    // What the tests of the library itself make (tests/private_tmux_dir.rs), with a dead socket in it.
    std::fs::create_dir_all(&library).unwrap();
    drop(UnixListener::bind(library.join("taw-gone-too")).unwrap());
    std::fs::create_dir_all(&live).unwrap();
    // A gate's directory while it formats and lints: no server is up in it yet.
    std::fs::create_dir_all(&empty).unwrap();
    // What a server that has ended leaves: a socket file that nothing listens on any more.
    drop(UnixListener::bind(dead.join("taw-gone")).unwrap());
    let server = Tmux {
        socket: Some(Socket::Path(live.join("taw-cleanup-live"))),
        ..Tmux::with_socket_name("taw-unused")
    };
    let home = ShellHome::new();
    home.start_server(&server, &["-d", "-s", "one", "sleep", "600"]);
    let tmux_tmp = base.path().join("tmux-tmp");
    std::fs::create_dir_all(tmux_tmp.join(format!("tmux-{}", uid.trim()))).unwrap();

    let run = run_bash(
        base.path(),
        &format!("{shell} '{}/tests/e2e/cleanup.sh'", repo_root().display()),
        &[
            ("TAW_CLEANUP_BASE", &base.path().display().to_string()),
            ("TMUX_TMPDIR", &tmux_tmp.display().to_string()),
        ],
    );

    assert!(
        run.status.success(),
        "cleanup.sh failed under {shell} ({}): {}{}",
        run.status,
        run.stdout,
        run.stderr
    );
    assert!(
        !dead.join("taw-gone").exists(),
        "the dead socket is still there: {}",
        run.stdout
    );
    assert!(
        !library.join("taw-gone-too").exists(),
        "the dead socket in a directory of the library's tests is still there: {}",
        run.stdout
    );
    assert!(
        dead.is_dir() && empty.is_dir(),
        "a private tmux directory was removed (the dead socket's: {}; the empty one's: {}): {}",
        dead.is_dir(),
        empty.is_dir(),
        run.stdout
    );
    assert!(
        server.run(&["list-sessions"]).is_ok(),
        "the live server was ended: {}",
        run.stdout
    );
    assert!(
        run.stdout.contains("taw-cleanup-live"),
        "the live server was not named: {}",
        run.stdout
    );
}

/// A run killed between `e2e_up` and `e2e_down`, where no trap of its own can run, leaves no server
/// running: its two servers are in the test's tmux base, which ends each of them by its exact socket
/// when it is dropped, so that they do not stay up for good.
#[test]
fn a_run_killed_between_e2e_up_and_e2e_down_leaves_no_server_running() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let printed = tmp.path().join("killed.out");
    let mut shell = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "source '{}/tests/e2e/harness.sh'\n\
             trap 'e2e_down > /dev/null 2>&1' EXIT\n\
             e2e_up killed > /dev/null\n\
             echo \"inner=$E2E_SOCK_FILE\"\n\
             echo \"outer=$E2E_OUTER_FILE\"\n\
             exec sleep 600\n",
            repo_root().display()
        ))
        .env("E2E_TMP", tmp.path())
        .env("TAW_BIN", taw_bin())
        .envs(base.env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket_of = |label: &str| -> Option<String> {
        std::fs::read_to_string(&printed)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    };
    let up = wait_for(|| socket_of("outer=").is_some(), Duration::from_secs(60));
    // Each server by its exact socket, and its pid while it answers.
    let servers: Vec<(String, Option<String>)> = ["inner=", "outer="]
        .iter()
        .map(|label| {
            let path = socket_of(label).unwrap_or_default();
            let tmux = Tmux {
                socket: Some(Socket::Path(PathBuf::from(&path))),
                ..Tmux::with_socket_name("taw-unused")
            };
            let pid = tmux.run(&["display-message", "-p", "#{pid}"]).ok();
            (path, pid)
        })
        .collect();
    // SIGKILL, which no trap of the script's can answer: what a run killed outright leaves.
    let _ = shell.kill();
    let _ = shell.wait();
    assert!(up, "e2e_up never said where its servers are");
    // As tmux says a socket's path: /tmp is /private/tmp on a Mac.
    let base_path = std::fs::canonicalize(base.path()).unwrap();

    drop(base);

    for (path, pid) in &servers {
        let pid: libc::pid_t = pid
            .as_deref()
            .unwrap_or_else(|| panic!("the server on {path} did not answer after e2e_up"))
            .parse()
            .unwrap();
        // SAFETY: `kill` with signal 0 sends nothing; it takes two integers.
        let alive = || unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            wait_for(|| !alive(), Duration::from_secs(10)),
            "the server on {path} (pid {pid}) still runs after its run was killed (the run's \
             servers, by socket and pid: {servers:?})"
        );
    }
    for (path, _) in &servers {
        assert!(
            Path::new(path).starts_with(&base_path),
            "a server of the run was not in the test's tmux base: {path}"
        );
    }
}

/// A job for `server` to run (`run-shell -b`, as the plugin's hooks are), which leaves a process behind
/// in the server's process group, as theirs do: it notes its pid, and then looks for its `$TMUX_TMPDIR`
/// every tenth of a second for five seconds, noting "gone" as soon as it finds it gone, or "there" at
/// the end. Its pid, once it runs.
fn a_job_that_looks_for_its_tmux_directory(server: &Tmux, tmp: &Path) -> libc::pid_t {
    let noted = tmp.join("looker.pid");
    let job = tmp.join("job.sh");
    write_script(
        &job,
        &format!(
            "sh -c 'echo $$ > \"$1.part\" && mv \"$1.part\" \"$1\"; n=0; \
             while [ \"$n\" -lt 50 ]; do [ -d \"$TMUX_TMPDIR\" ] || {{ echo gone > \"$2\"; exit 0; }}; \
             sleep 0.1; n=$((n + 1)); done; echo there > \"$2\"' sh '{}' '{}' > /dev/null 2>&1 &\n",
            noted.display(),
            tmp.join("looked.txt").display()
        ),
    );
    server
        .run_ok(&["run-shell", "-b", &format!("'{}'", job.display())])
        .unwrap();
    wait_for(|| noted.exists(), Duration::from_secs(10));
    std::fs::read_to_string(&noted)
        .ok()
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or(0)
}

/// What the process of [`a_job_that_looks_for_its_tmux_directory`] found: "gone", "there", or
/// nothing, when it was ended before it found anything. It is waited for until it has gone (seven
/// seconds at most, and then ended by its pid).
fn what_the_looker_found(pid: libc::pid_t, tmp: &Path) -> String {
    // SAFETY: `kill` takes two integers and touches no memory of this process; signal 0 sends nothing.
    let alive = || pid > 1 && unsafe { libc::kill(pid, 0) } == 0;
    if !wait_for(|| !alive(), Duration::from_secs(7)) {
        // SAFETY: as above; the pid is the looker's, which it wrote itself, and is more than 1.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    std::fs::read_to_string(tmp.join("looked.txt"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A tmux base ends the servers still running in it when it is dropped (a run killed before its
/// `e2e_down` leaves them), and then their process groups, before its directory goes: a job that a
/// server ran outlives the server, and one that ran on would find its `$TMUX_TMPDIR` gone (and a tmux
/// of it would take /tmp for it). The run is killed as in the test above, with a job of the test's
/// running on its inner server.
#[test]
fn a_dropped_tmux_base_ends_the_jobs_of_the_servers_in_it_before_its_directory_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let printed = tmp.path().join("killed.out");
    let mut shell = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "source '{}/tests/e2e/harness.sh'\n\
             trap 'e2e_down > /dev/null 2>&1' EXIT\n\
             e2e_up jobs > /dev/null\n\
             echo \"inner=$E2E_SOCK_FILE\"\n\
             exec sleep 600\n",
            repo_root().display()
        ))
        .env("E2E_TMP", tmp.path())
        .env("TAW_BIN", taw_bin())
        .envs(base.env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let inner = || -> Option<String> {
        std::fs::read_to_string(&printed)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("inner="))
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    };
    let up = wait_for(|| inner().is_some(), Duration::from_secs(60));
    let looker = match inner() {
        Some(path) => a_job_that_looks_for_its_tmux_directory(
            &Tmux {
                socket: Some(Socket::Path(PathBuf::from(path))),
                ..Tmux::with_socket_name("taw-unused")
            },
            tmp.path(),
        ),
        None => 0,
    };
    // SIGKILL, which no trap of the script's can answer: what a run killed outright leaves.
    let _ = shell.kill();
    let _ = shell.wait();

    drop(base);

    let found = what_the_looker_found(looker, tmp.path());
    assert!(
        up && looker > 1,
        "the run never came up, or its job never ran"
    );
    assert_eq!(
        found, "",
        "a job of a server in the dropped base ran on, and found its tmux directory {found}"
    );
}

/// The same for a harness whose shell died before its `e2e_down`: its drop ends the two servers, and
/// then their process groups, before the base it is in goes.
#[test]
fn a_dropped_harness_whose_shell_died_ends_the_jobs_of_its_servers_before_its_directory_goes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut harness = harness_up(tmp.path(), &[]);
    let looker = a_job_that_looks_for_its_tmux_directory(&harness.inner, tmp.path());
    harness.kill_the_shell();

    drop(harness);

    let found = what_the_looker_found(looker, tmp.path());
    assert!(looker > 1, "the job never ran");
    assert_eq!(
        found, "",
        "a job of a server of the dropped harness ran on, and found its tmux directory {found}"
    );
}

/// Every socket in `base` that a server still answers on.
fn live_sockets(base: &TmuxBase) -> Vec<PathBuf> {
    base.sockets()
        .into_iter()
        .filter(|socket| {
            Tmux {
                socket: Some(Socket::Path(socket.clone())),
                ..Tmux::with_socket_name("taw-unused")
            }
            .run(&["list-sessions"])
            .is_ok()
        })
        .collect()
}

/// The acceptance run and the smoke test set their EXIT trap before `e2e_up`: one told to stop while
/// e2e_up brings its servers up (or whose e2e_up fails then, its client not attached in time) takes
/// them down all the same. They set it after, and left both servers running for good, which nothing
/// would end (cleanup.sh leaves a server that answers alone). Stopped ([`Stop`]) once both servers
/// are up, in a tmux base of the test's own.
#[test]
fn the_acceptance_run_and_the_smoke_test_take_down_what_e2e_up_started_when_stopped_in_it() {
    for script in ["acceptance.sh", "smoke.sh"] {
        let tmp = tempfile::tempdir().unwrap();
        let base = TmuxBase::new();
        let stop = Stop::at(tmp.path(), "E2E_SOCK_FILE=$(");
        let printed = tmp.path().join("run.out");
        let mut run = Command::new("bash")
            .arg(repo_root().join("tests").join("e2e").join(script))
            .env("E2E_TMP", tmp.path())
            .env("TAW_BIN", taw_bin())
            .env("BASH_ENV", &stop.bash_env)
            .envs(base.env())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&printed).unwrap())
            .stderr(std::fs::File::create(tmp.path().join("run.err")).unwrap())
            .spawn()
            .unwrap();
        let ended = wait_for(
            || matches!(run.try_wait(), Ok(Some(_))),
            Duration::from_secs(60),
        );
        if !ended {
            let _ = run.kill();
        }
        let _ = run.wait();
        let noted = stop.noted().unwrap_or_default();
        let left = live_sockets(&base);
        let said = std::fs::read_to_string(&printed).unwrap_or_default();

        assert!(
            ended && stop.noted().is_some(),
            "{script} never got to its servers' socket files: {said}"
        );
        assert!(
            noted
                .tmux_tmpdir
                .starts_with(base.path().join("taw-e2e-tmux.").to_str().unwrap()),
            "{script} did not run in the test's tmux base, so this proves nothing: {said}"
        );
        assert_eq!(
            left,
            Vec::<PathBuf>::new(),
            "{script}, stopped while e2e_up brought its servers up, left them running: {said}"
        );
        assert!(
            !Path::new(&noted.tmux_tmpdir).exists(),
            "{script}, stopped while e2e_up brought its servers up, left its tmux directory: {said}"
        );
    }
}

/// e2e_up sets its signal traps (a signal lets the command that is running finish, then exits through
/// the EXIT trap) in a script only. In an interactive shell, the README's way to explore, `trap 'exit
/// 130' INT` made the next Ctrl-C at the prompt exit the developer's shell, with no e2e_down. An
/// interactive bash with no startup files, in a pane of a server of the test's, sources the harness,
/// brings it up in a tmux base of the test's, gets C-c at its prompt, and is still there to take the
/// harness down.
#[test]
fn an_interactive_shell_that_brought_the_harness_up_outlives_a_ctrl_c() {
    let server = Server::start();
    let base = TmuxBase::new();
    let tmp = tempfile::tempdir().unwrap();
    let pane = server
        .tmux_ok(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-e",
            &format!("E2E_TMP={}", tmp.path().display()),
            "-e",
            &format!("TAW_BIN={}", taw_bin()),
            "-e",
            &format!("E2E_TMUX_BASE={}", base.path().display()),
            "-e",
            &format!("TMUX_TMPDIR={}", base.tmux_dir().display()),
            "bash --norc --noprofile -i",
        ])
        .trim()
        .to_string();
    let shows = |text: &str| {
        server
            .tmux(&["capture-pane", "-p", "-t", &pane])
            .is_ok_and(|screen| screen.contains(text))
    };
    let typed = |text: &str| {
        let _ = server.tmux(&["send-keys", "-t", &pane, "-l", text]);
        let _ = server.tmux(&["send-keys", "-t", &pane, "Enter"]);
    };
    wait_for(|| shows("$"), Duration::from_secs(10));
    typed(&format!(
        "source '{}/tests/e2e/harness.sh' && e2e_up interactive",
        repo_root().display()
    ));
    let up = wait_for(|| shows("up: inner="), Duration::from_secs(60));

    let _ = server.tmux(&["send-keys", "-t", &pane, "C-c"]);
    typed("echo still-$((6 * 7))");
    let still = wait_for(|| shows("still-42"), Duration::from_secs(10));
    if still {
        typed("e2e_down");
        wait_for(|| shows("down: "), Duration::from_secs(30));
    }
    let screen = server
        .tmux(&["capture-pane", "-p", "-t", &pane])
        .unwrap_or_else(|err| format!("(no pane: {err})"));

    assert!(
        up,
        "the harness never came up in the interactive shell: {screen}"
    );
    assert!(
        still,
        "the interactive shell that brought the harness up did not outlive a Ctrl-C: {screen}"
    );
}

/// A run told to stop (TERM, to its shell alone) lets the command it is running finish before its tmux
/// directory goes, and then takes the directory down with its servers: tmux takes /tmp for a
/// `$TMUX_TMPDIR` that is gone, and the command would have run on with no net. The command notes its
/// pid and runs for two seconds; the test watches the directory until it is gone, and the command has
/// to be over by then.
#[test]
fn a_run_told_to_stop_keeps_its_tmux_directory_until_the_command_it_runs_has_ended() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let printed = tmp.path().join("told.out");
    let step = tmp.path().join("step.pid");
    let mut shell = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "source '{}/tests/e2e/harness.sh'\n\
             trap 'e2e_down > /dev/null 2>&1' EXIT\n\
             e2e_up told > /dev/null\n\
             echo \"dir=$TMUX_TMPDIR\"\n\
             sh -c 'echo $$ > \"$1\".part && mv \"$1\".part \"$1\"; exec sleep 2' sh '{}'\n\
             echo after=yes\n",
            repo_root().display(),
            step.display()
        ))
        .env("E2E_TMP", tmp.path())
        .env("TAW_BIN", taw_bin())
        .envs(base.env())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&printed).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let up = wait_for(|| step.exists(), Duration::from_secs(60));
    let dir = std::fs::read_to_string(&printed)
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("dir="))
        .unwrap_or_default()
        .to_string();
    let step_pid: libc::pid_t = std::fs::read_to_string(&step)
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(0);
    // SAFETY: `kill` takes two integers and touches no memory of this process; signal 0 sends nothing.
    let alive = |pid: libc::pid_t| pid > 1 && unsafe { libc::kill(pid, 0) } == 0;

    // SAFETY: as above; the pid is the run's shell, a child of this test.
    unsafe { libc::kill(shell.id() as libc::pid_t, libc::SIGTERM) };
    let mut went_too_soon = false;
    let gone = wait_for(
        || {
            let gone = !dir.is_empty() && !Path::new(&dir).exists();
            went_too_soon |= gone && alive(step_pid);
            gone
        },
        Duration::from_secs(30),
    );
    let ended = wait_for(
        || matches!(shell.try_wait(), Ok(Some(_))),
        Duration::from_secs(30),
    );
    if !ended {
        let _ = shell.kill();
    }
    let _ = shell.wait();
    if alive(step_pid) {
        // SAFETY: as above; the pid is the command's, which wrote it itself.
        unsafe { libc::kill(step_pid, libc::SIGKILL) };
    }

    assert!(up && step_pid > 1, "the run never got to its command");
    assert!(
        !went_too_soon,
        "the run's tmux directory went while the command it was running went on without it"
    );
    assert!(gone, "the run's tmux directory was never taken down");
    assert!(ended, "the run did not end when it was told to stop");
}

/// `e2e_down` a second time (a script that takes the harness down itself, and again from its EXIT
/// trap) runs no tmux: the first call forgets the servers, so that the second cannot reach for them
/// after the run's tmux directory is gone, where tmux would take /tmp for it. A stand-in tmux first on
/// the script's `PATH` notes every call and runs the real one.
#[test]
fn e2e_down_a_second_time_runs_no_tmux() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let log = tmp.path().join("tmux.log");
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("tmux"),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            which::which("tmux").unwrap().display()
        ),
    );
    let mut env = vec![
        ("E2E_TMP", tmp.path().display().to_string()),
        ("TAW_BIN", taw_bin().to_string()),
        (
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        ),
    ];
    env.extend(base.env());
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/tests/e2e/harness.sh'\n\
             e2e_up twice > /dev/null || echo failed=up\n\
             e2e_down > /dev/null\n\
             echo '--- the second e2e_down' >> '{}'\n\
             e2e_down > /dev/null\n",
            repo_root().display(),
            log.display()
        ),
        &env,
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let (before, after) = calls
        .split_once("--- the second e2e_down\n")
        .unwrap_or((&calls, ""));

    assert_eq!(run.values().get("failed"), None, "{}", run.stderr);
    assert!(
        before.lines().any(|call| call.contains("kill-server")),
        "the stand-in saw no tmux of the first e2e_down, so this proves nothing: {before}"
    );
    assert_eq!(
        after, "",
        "the second e2e_down ran tmux, after the run's tmux directory was gone"
    );
}

/// `e2e_down` ends its servers' process groups (the plugin's jobs, which outlive the servers) once the
/// servers have gone, by which time a server's pid may be another process's: it hands them over as
/// groups alone, which a process that leads none is not ended by. A stand-in tmux first on the
/// script's `PATH` stands for that: it gives the pid of a lone sleep of the script's as each server's
/// pid, and has no server to end. No tmux is run.
#[test]
fn e2e_down_hands_its_servers_over_as_groups_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let base = TmuxBase::new();
    let lone = tmp.path().join("lone.pid");
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("tmux"),
        &format!(
            "case \"$*\" in *'#{{pid}}'*) cat '{}' ;; *) exit 1 ;; esac\n",
            lone.display()
        ),
    );
    let mut env = vec![
        ("E2E_TMP", tmp.path().display().to_string()),
        ("TAW_BIN", taw_bin().to_string()),
    ];
    env.extend(base.env());
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();

    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{repo}/tests/e2e/harness.sh'\n\
             sleep 30 & echo $! > '{lone}'\n\
             private_tmux_make \"$E2E_TMUX_BASE/taw-e2e-tmux\" || echo failed=make\n\
             SOCK=\"taw-e2e-gone-$$\" OUTER=\"taw-e2e-gone-$$-outer\"\n\
             PATH='{bin}':$PATH e2e_down > /dev/null\n\
             echo \"lone=$(kill -0 \"$(cat '{lone}')\" 2>/dev/null && echo alive || echo ended)\"\n\
             kill \"$(cat '{lone}')\" 2>/dev/null\n",
            repo = repo_root().display(),
            lone = lone.display(),
            bin = bin.display()
        ),
        &env,
    );
    let seen = run.values();

    assert_eq!(seen.get("failed"), None, "{}", run.stderr);
    assert_eq!(
        seen["lone"], "alive",
        "e2e_down ended a process that has a pid one of its servers had, which leads no group: {}",
        run.stderr
    );
}

/// `harness_up` giving up on a run leaves no server of it running. Here e2e_up says no socket path,
/// which a stand-in tmux first on the run's `PATH` makes it do (it answers that one question with
/// nothing and passes everything else to the real tmux, noting each call and the pid of each server it
/// starts), and `harness_up` gives up on the empty word. It does not stop the shell with SIGKILL,
/// which no trap answers, leaving the two servers running in the run's tmux directory for good. It
/// stops it with TERM first, which the shell answers with `e2e_down` (a `kill-server` by the
/// servers' names), and the run is in a tmux base of harness_up's own, whose drop ends whatever is
/// still under it.
#[test]
fn harness_up_giving_up_on_a_run_leaves_no_server_running() {
    let tmp = tempfile::tempdir().unwrap();
    let pids = tmp.path().join("servers.txt");
    let calls = tmp.path().join("calls.txt");
    let bin = tmp.path().join("bin");
    let real = which::which("tmux").unwrap();
    write_script(
        &bin.join("tmux"),
        &format!(
            "printf '%s in %s\\n' \"$*\" \"${{TMUX_TMPDIR:-}}\" >> '{calls}'\n\
             case \"$*\" in *'display-message -p #{{socket_path}}'*) exit 0 ;; esac\n\
             '{real}' \"$@\"; status=$?\n\
             case \"$*\" in *new-session*)\n\
               name=''; prev=''\n\
               for a in \"$@\"; do [ \"$prev\" = -L ] && name=\"$a\"; prev=\"$a\"; done\n\
               [ -n \"$name\" ] && '{real}' -L \"$name\" display-message -p '#{{pid}}' >> '{pids}'\n\
               ;;\n\
             esac\n\
             exit $status\n",
            real = real.display(),
            pids = pids.display(),
            calls = calls.display()
        ),
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let gave_up = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        harness_up(tmp.path(), &[("PATH", &path)])
    }));

    let started: Vec<libc::pid_t> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|pid| pid.trim().parse().ok())
        .collect();
    // SAFETY: `kill` takes two integers and touches no memory of this process; signal 0 sends nothing.
    let alive = |pid: libc::pid_t| pid > 1 && unsafe { libc::kill(pid, 0) } == 0;
    let left_running: Vec<libc::pid_t> = started
        .iter()
        .copied()
        .filter(|pid| !wait_for(|| !alive(*pid), Duration::from_secs(10)))
        .collect();
    assert!(
        gave_up.is_err(),
        "harness_up did not give up on a run that said no socket path"
    );
    assert_eq!(
        started.len(),
        2,
        "the stand-in did not see the run's two servers start: {started:?}"
    );
    assert_eq!(
        left_running,
        Vec::<libc::pid_t>::new(),
        "these servers of the run harness_up gave up on still run"
    );
    let called = std::fs::read_to_string(&calls).unwrap_or_default();
    assert!(
        called
            .lines()
            .filter(|call| call.contains(" new-session "))
            .all(|call| call.contains(" in /tmp/taw-hcb-")),
        "the run was not in a tmux base of harness_up's own:\n{called}"
    );
    assert!(
        called
            .lines()
            .any(|call| call.starts_with("-L taw-e2e-guard-") && call.contains(" kill-server")),
        "the shell did not take its servers down itself (e2e_down, on TERM):\n{called}"
    );
}
