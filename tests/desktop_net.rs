//! No test shows a notification or plays a sound to whoever runs the suite.
//!
//! taw's `system` toast runs `osascript` (macOS) or `notify-send` (Linux), and its sound `afplay` (macOS)
//! or `paplay` or `aplay` (Linux), unless `$TAW_NOTIFY_CMD` and `$TAW_SOUND_CMD` name a program in their
//! place, which is how a test records them. A test that has neither puts a real notification on the screen
//! of whoever runs it: one once put "claude done · one:1" on a desktop. The gate
//! (scripts/check.sh) and the acceptance harness put a net of refusing stubs first on PATH for that
//! (scripts/desktop-net.sh): one for every name in scripts/desktop-net-names.txt, which notes the call in
//! the net's log and refuses it, and the gate fails when the log has a line. The two variables name
//! refusing recorders of the net's there, and a server a test starts (tests/common) has refusing recorders
//! of its own for them, which note what they refuse in the same log.
//!
//! These tests prove the net. Under the gate, which says where its net is (`TAW_DESKTOP_NET`), a bare
//! `osascript` and a bare `afplay` planted here, as a test with no recorder would run them, find the net's
//! stubs first and are refused and noted; the notes go to a log of the test's own
//! (`TAW_DESKTOP_NET_LOG`), so that the gate's stays empty. And, under the gate or not, the net a script
//! makes has a stub for every name, refuses and notes each call, and fails its check when one came; and a
//! real watcher planted under a server of the tests', with a `system` toast and a sound to deliver, is
//! caught: by its server's recorders when its test sets none, by the net's stubs when it has no recorder
//! at all, each named by the net's check, which fails, as the gate's last step does. Nothing here runs a
//! stub before it has seen that the stub is what the name finds first.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::harness::run_bash;
use common::repo_root;

/// Where `name` is found first on `path`, as a shell finds a command.
fn found_first(name: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The lines of `log`, each a call the net noted: what was run, its arguments, its directory and its
/// pane.
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
fn under_the_gate_a_bare_notifier_and_a_bare_player_find_the_nets_stubs_and_are_refused() {
    let Ok(net) = std::env::var("TAW_DESKTOP_NET") else {
        // In the gate a missing net is the gate's own: a check.sh that stopped making it fails here.
        assert!(
            !in_the_gates_tmux_directory(),
            "the gate runs this without its net: scripts/check.sh made no TAW_DESKTOP_NET"
        );
        eprintln!("not under the gate's net (no TAW_DESKTOP_NET): scripts/check.sh runs this");
        return;
    };
    let path = std::env::var("PATH").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("log");
    for name in ["osascript", "afplay"] {
        // Looked up first, and not run unless it is the net's: the real one is not to be run here.
        assert_eq!(
            found_first(name, &path),
            Some(Path::new(&net).join("bin").join(name)),
            "{name} is not the net's first on the gate's PATH"
        );
        let status = Command::new(name)
            .arg("taw-net-guard")
            .current_dir(tmp.path())
            .env("TAW_DESKTOP_NET_LOG", &log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(127), "{name}");
    }
    // And what taw runs in their place, in every process of the gate's that sets no recorder of its
    // own: the net's refusing recorders.
    for (variable, args) in [
        ("TAW_NOTIFY_CMD", &["claude done", "taw-net-guard"][..]),
        ("TAW_SOUND_CMD", &["/tmp/taw-net-guard.wav"][..]),
    ] {
        let recorder = Path::new(&net).join("seam").join(variable);
        assert_eq!(
            std::env::var_os(variable).map(PathBuf::from),
            Some(recorder.clone()),
            "${variable} is not the net's recorder under the gate"
        );
        let status = Command::new(&recorder)
            .args(args)
            .current_dir(tmp.path())
            .env("TAW_DESKTOP_NET_LOG", &log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1), "{variable}");
    }
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        noted(&log),
        [
            format!("osascript\ttaw-net-guard\t{dir}\t"),
            format!("afplay\ttaw-net-guard\t{dir}\t"),
            format!("TAW_NOTIFY_CMD\tclaude done taw-net-guard\t{dir}\t"),
            format!("TAW_SOUND_CMD\t/tmp/taw-net-guard.wav\t{dir}\t"),
        ]
    );
}

#[test]
fn the_net_has_a_stub_for_every_name_and_its_check_names_each_call_it_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_bash(
        tmp.path(),
        &format!(
            "source '{}/scripts/desktop-net.sh'\n\
             desktop_net_make \"$PWD/net\" || echo failed=make\n\
             desktop_net_check; echo empty=$?\n\
             while IFS= read -r name; do\n\
             \x20 case \"$name\" in '' | '#'*) continue ;; esac\n\
             \x20 [ \"$(command -v \"$name\")\" = \"$PWD/net/bin/$name\" ] || echo \"not-first=$name\"\n\
             done < \"$DESKTOP_NET_NAMES\"\n\
             [ \"$(command -v osascript)\" = \"$PWD/net/bin/osascript\" ] || exit 9\n\
             [ \"$(command -v paplay)\" = \"$PWD/net/bin/paplay\" ] || exit 9\n\
             osascript -e 'display notification \"x\"'; echo osascript=$?\n\
             TMUX_PANE=%7 paplay /tmp/done.wav; echo paplay=$?\n\
             [ \"$TAW_NOTIFY_CMD\" = \"$PWD/net/seam/TAW_NOTIFY_CMD\" ] || exit 8\n\
             [ \"$TAW_SOUND_CMD\" = \"$PWD/net/seam/TAW_SOUND_CMD\" ] || exit 8\n\
             \"$TAW_NOTIFY_CMD\" 'claude done' 'a body'; echo notify=$?\n\
             TMUX_PANE=%8 \"$TAW_SOUND_CMD\" /tmp/request.wav; echo sound=$?\n\
             desktop_net_check 2> check.err; echo check=$?\n",
            repo_root().display()
        ),
        &[
            ("TAW_DESKTOP_NET_LOG", "/no/such/log"),
            ("TAW_NOTIFY_CMD", "/no/such/notifier"),
            ("TAW_SOUND_CMD", "/no/such/player"),
        ],
    );
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        run.stdout, "empty=0\nosascript=127\npaplay=127\nnotify=1\nsound=1\ncheck=1\n",
        "{}",
        run.stderr
    );
    let said = std::fs::read_to_string(tmp.path().join("check.err")).unwrap();
    for line in [
        format!("osascript\t-e display notification \"x\"\t{dir}\t\n"),
        format!("paplay\t/tmp/done.wav\t{dir}\t%7\n"),
        format!("TAW_NOTIFY_CMD\tclaude done a body\t{dir}\t\n"),
        format!("TAW_SOUND_CMD\t/tmp/request.wav\t{dir}\t%8\n"),
    ] {
        assert!(said.contains(&line), "{line:?} is not in\n{said}");
    }
}

/// A server a test starts (tests/common) has a refusing recorder for each of the two variables in the
/// environment it starts with, which every watcher and hook of it inherits, where the test sets none of
/// its own: the home's, which notes the call and refuses it. A test that records its notifications keeps
/// its own recorder.
#[test]
fn a_test_server_starts_with_a_refusing_recorder_for_each_variable_its_test_sets_none_of() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("log");
    let server = common::Server::start();
    for variable in common::shell_env::DESKTOP_SEAMS {
        let said = server.tmux_ok(&["show-environment", "-g", variable]);
        let recorder = said
            .strip_prefix(&format!("{variable}="))
            .unwrap_or_else(|| panic!("the server started with no {variable}: {said:?}"));
        assert_ne!(
            std::env::var(variable).ok().as_deref(),
            Some(recorder),
            "the server has this process's {variable}, not a recorder of its own"
        );
        let status = Command::new(recorder)
            .arg("taw-net-guard")
            .current_dir(tmp.path())
            .env("TAW_DESKTOP_NET_LOG", &log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(1), "{variable}");
    }
    let dir = tmp.path().canonicalize().unwrap().display().to_string();
    assert_eq!(
        noted(&log),
        [
            format!("TAW_NOTIFY_CMD\ttaw-net-guard\t{dir}\t"),
            format!("TAW_SOUND_CMD\ttaw-net-guard\t{dir}\t"),
        ]
    );

    let recorder = common::NotifyRecorder::new(tmp.path());
    let recording = common::Server::start_with(common::ServerOptions {
        env: vec![(
            "TAW_NOTIFY_CMD".to_string(),
            recorder.script.display().to_string(),
        )],
        ..common::ServerOptions::default()
    });
    assert_eq!(
        recording.tmux_ok(&["show-environment", "-g", "TAW_NOTIFY_CMD"]),
        format!("TAW_NOTIFY_CMD={}", recorder.script.display()),
        "a test's own recorder is not the one its server has"
    );
}

/// The first test fails, and does not pass by skipping, when it runs in the gate's own tmux directory
/// (`taw-check.*`) with no `TAW_DESKTOP_NET`, as it would under a scripts/check.sh that had stopped making
/// the net. It is run here as the gate would run it, by this test binary, with the environment so.
#[test]
fn the_gate_only_test_fails_in_the_gates_directory_without_the_net() {
    let tmp = tempfile::tempdir().unwrap();
    let gates = tmp.path().join("taw-check.test");
    std::fs::create_dir_all(&gates).unwrap();
    let run = Command::new(std::env::current_exe().unwrap())
        .args([
            "under_the_gate_a_bare_notifier_and_a_bare_player_find_the_nets_stubs_and_are_refused",
            "--exact",
            "--test-threads=1",
        ])
        .env_remove("TAW_DESKTOP_NET")
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

/// What a planted watcher shows and plays: a notification through `system` and a sound, each at once,
/// for an agent that is blocked in a session nobody is attached to.
const PLANTED: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"system\"
delay_seconds = 0
done_delay_seconds = 0

[ui.sound]
enabled = true
request_path = \"planted.wav\"
done_path = \"planted.wav\"
";

/// Ceiling for a watcher to have noticed the agent and done what it does: a new agent reads `unknown`
/// for 3 s and a tick, and `cargo test` runs the suite's files at once.
const SETTLE: Duration = Duration::from_secs(25);

/// The `PATH` a planted watcher's server starts with: under the gate the one this process has, with the
/// gate's net first; elsewhere that one behind a net the script makes in `dir`. Every program of the
/// desktop's is a stub of that net on it, which is looked at before anything runs, since the real ones
/// are not to be run here. And the folder of the stubs.
fn planted_path(dir: &Path) -> (String, PathBuf) {
    let path = std::env::var("PATH").unwrap();
    let (path, bin) = match std::env::var("TAW_DESKTOP_NET") {
        Ok(net) => (path, Path::new(&net).join("bin")),
        Err(_) => {
            let made = run_bash(
                dir,
                &format!(
                    "source '{}/scripts/desktop-net.sh'\ndesktop_net_make \"$PWD/net\" && echo made\n",
                    repo_root().display()
                ),
                &[],
            );
            assert_eq!(made.stdout, "made\n", "{}", made.stderr);
            let bin = dir.join("net").join("bin");
            (format!("{}:{path}", bin.display()), bin)
        }
    };
    for name in ["osascript", "notify-send", "afplay", "paplay", "aplay"] {
        assert_eq!(
            found_first(name, &path),
            Some(bin.join(name)),
            "{name} is not the net's first on the planted watcher's PATH"
        );
    }
    (path, bin)
}

/// A real watcher, under a server of tests/common, on a fake Claude that gets blocked, with the toast
/// and the sound of [`PLANTED`]: the calls they made, as the net noted them in `log`
/// (`TAW_DESKTOP_NET_LOG`, which every stub and recorder of a net writes to when it is set). `without`
/// is taken out of the environment the server starts with.
fn planted_watcher(dir: &Path, log: &Path, without: &[&str]) -> Vec<String> {
    let (path, bin) = planted_path(dir);
    let fake = common::FakeAgent::new(dir);
    let server = common::Server::start_with(common::ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env: vec![
            ("PATH".to_string(), path),
            ("TAW_DESKTOP_NET_LOG".to_string(), log.display().to_string()),
        ],
        env_remove: without.iter().map(|name| name.to_string()).collect(),
        ..common::ServerOptions::default()
    });
    // The PATH the server really started with (what a watcher's `osascript` is looked up on), and not
    // only the one this test computed for it: each stub of the net is first on it.
    let started_with = server.tmux_ok(&["show-environment", "-g", "PATH"]);
    let started_with = started_with
        .strip_prefix("PATH=")
        .unwrap_or_else(|| panic!("the server started with no PATH: {started_with:?}"));
    for name in ["osascript", "notify-send", "afplay", "paplay", "aplay"] {
        assert_eq!(
            found_first(name, started_with),
            Some(bin.join(name)),
            "{name} is not the net's first on the PATH the planted watcher's server started with"
        );
    }
    server.write_config(PLANTED);
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        common::wait_for(|| server.piped(&pane) == "1", SETTLE),
        "the watcher never started"
    );
    server.ok(
        &[
            "agent", "start", "planted", "--kind", "claude", "--pane", &pane,
        ],
        "cli:agent:start",
        "agent",
    );
    fake.wait_until_drawn(&server, &pane, SETTLE);
    for (signal, want) in [("", "idle"), ("-USR1", "working"), ("-USR2", "blocked")] {
        if !signal.is_empty() {
            fake.signal(signal);
        }
        assert!(
            common::wait_for(|| server.pane_opt(&pane, "@agent_state") == want, SETTLE),
            "the planted agent never read {want} (it reads {:?})",
            server.pane_opt(&pane, "@agent_state")
        );
    }
    assert!(
        common::wait_for(|| noted(log).len() >= 2, SETTLE),
        "the blocked agent's toast and sound never reached the net: {:?}\n{}",
        noted(log),
        server.watcher_logs()
    );
    noted(log)
}

/// The net's check over `log`, as the gate's last step runs it over the gate's: it fails, and names each
/// of `calls`.
fn the_check_fails_naming(dir: &Path, log: &Path, calls: &[String]) {
    let run = run_bash(
        dir,
        &format!(
            "source '{}/scripts/desktop-net.sh'\n\
             mkdir -p checked && cp '{}' checked/log\n\
             DESKTOP_NET_DIR=\"$PWD/checked\"\n\
             desktop_net_check 2> check.err; echo check=$?\n",
            repo_root().display(),
            log.display()
        ),
        &[],
    );
    assert_eq!(run.stdout, "check=1\n", "{}", run.stderr);
    let said = std::fs::read_to_string(dir.join("check.err")).unwrap();
    for call in calls {
        assert!(said.contains(call.as_str()), "{call:?} is not in\n{said}");
    }
}

/// A real watcher whose test sets no recorder of its own, with a `system` toast and a sound for a blocked
/// agent: both go to the refusing recorders its server started with (tests/common), and nothing runs a
/// program of the desktop's, not even its stub. The net's check fails and names both, as the gate's last
/// step does.
#[test]
fn a_planted_watchers_toast_and_sound_go_to_its_servers_refusing_recorders_and_are_named() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("planted.log");
    let calls = planted_watcher(tmp.path(), &log, &[]);
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("TAW_NOTIFY_CMD\tplanted blocked \u{b7} one:0 ")),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("TAW_SOUND_CMD\t") && call.contains("/planted.wav\t")),
        "{calls:#?}"
    );
    assert!(
        calls.iter().all(|call| call.starts_with("TAW_")),
        "a program of the desktop's was run: {calls:#?}"
    );
    the_check_fails_naming(tmp.path(), &log, &calls);
}

/// The same watcher in a server started with neither variable, so that it has no recorder at all: it
/// runs the desktop's notifier and player by name, which are the net's stubs on its `PATH`, refused and
/// noted, and the net's check fails and names both.
#[test]
fn a_planted_watcher_with_no_recorder_at_all_reaches_the_nets_stubs_and_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("planted.log");
    let calls = planted_watcher(tmp.path(), &log, &common::shell_env::DESKTOP_SEAMS);
    let (notifier, player) = if cfg!(target_os = "macos") {
        ("osascript", "afplay")
    } else {
        ("notify-send", "paplay")
    };
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with(&format!("{notifier}\t"))
                && call.contains("planted blocked \u{b7} one:0")),
        "{calls:#?}"
    );
    assert!(
        calls.iter().any(
            |call| call.starts_with(&format!("{player}\t")) && call.contains("/planted.wav\t")
        ),
        "{calls:#?}"
    );
    the_check_fails_naming(tmp.path(), &log, &calls);
}
