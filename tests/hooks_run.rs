//! `[hooks]` commands, run for real: what a hook is given, that it never delays the caller,
//! that one that runs too long is stopped, and that a failure is written down.
//!
//! The dispatcher starts a supervisor, which is the built `taw` (`taw hooks exec`), so these
//! run the binary. No tmux server is involved: a hook needs none, and the socket named here
//! is one nothing listens on, so the names tmux would have supplied stay empty.

#[path = "common/exec.rs"]
mod exec;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use taw::config::Config;
use taw::hooks;
use taw::paths::Paths;
use taw::plugin::PluginCtx;
use taw::tmux::Tmux;

/// A config, its state directory and a scratch directory for what a hook writes, all under
/// one temp dir. Nothing here reads or writes the real config or state.
struct Rig {
    dir: tempfile::TempDir,
    cfg: Config,
    paths: Paths,
    tmux: Tmux,
}

/// The paths of a rig rooted at `root`: everything under it, nothing of the machine's own.
fn paths_under(root: &Path) -> Paths {
    let state = root.join("state");
    let plugins_dir = state.join("plugins");
    let agent_detection = state.join("agent-detection");
    Paths {
        config_dir: root.join("config"),
        config_file: root.join("config").join("config.toml"),
        state_dir: state.clone(),
        override_dir: root.join("config").join("agent-detection"),
        remote_manifest_dir: agent_detection.join("remote"),
        manifest_marker: agent_detection.join("last-check"),
        sessions_dir: state.join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: state.join("logs"),
    }
}

impl Rig {
    /// A rig whose config is `toml`.
    fn new(toml: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let paths = paths_under(&root);
        std::fs::create_dir_all(root.join("out")).unwrap();
        let cfg: Config = toml::from_str(&toml.replace("$OUT", root.join("out").to_str().unwrap()))
            .expect("the test config parses");
        Rig {
            dir,
            cfg,
            paths,
            tmux: Tmux::with_socket_name("taw-hooks-run-no-server"),
        }
    }

    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    /// Where a hook is told to write (`$OUT` in the config).
    fn out(&self, name: &str) -> PathBuf {
        self.root().join("out").join(name)
    }

    fn ctx(&self) -> PluginCtx<'_> {
        self.ctx_with(Path::new(env!("CARGO_BIN_EXE_taw")))
    }

    /// A context whose supervisor is `taw_bin`: what a long-lived watcher has when the `taw`
    /// it started from has since been moved or replaced.
    fn ctx_with<'a>(&'a self, taw_bin: &'a Path) -> PluginCtx<'a> {
        PluginCtx {
            tmux: &self.tmux,
            cfg: &self.cfg,
            paths: &self.paths,
            taw_bin,
            server_env: None,
        }
    }

    /// An executable script of the given body, in the scratch directory.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root().join("out").join(name);
        std::fs::write(&path, body).unwrap();
        crate::exec::make_executable(&path);
        path
    }

    fn log(&self) -> String {
        std::fs::read_to_string(hooks::log::path(&self.paths)).unwrap_or_default()
    }
}

/// Polls `ready` until it holds or `timeout` passes.
fn wait_for(mut ready: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Every wait for a hook to have run: a supervisor is a process of its own, started under
/// whatever load the rest of the suite puts on the machine.
const SETTLE: Duration = Duration::from_secs(20);

/// Whether `pid` is still running.
fn is_running(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A pane's event payload, `pane.updated`'s: the record is what the hook reads its
/// environment from.
fn pane_payload(dir: &Path, title: &str) -> Value {
    json!({ "pane": {
        "pane_id": "w3:p31",
        "workspace_id": "w3",
        "tab_id": "w3:t7",
        "terminal_id": "%31",
        "cwd": dir,
        "agent": "claude",
        "agent_status": "done",
        "terminal_title": title,
    }})
}

/// `KEY=value` lines as a map.
fn env_of(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn a_hook_is_given_its_environment_and_the_event_json_and_a_hostile_title_stays_a_title() {
    let rig = Rig::new(
        r#"
[hooks]
"pane.updated" = "cat > $OUT/stdin.json; env | grep '^TAW_' | sort > $OUT/env.txt; touch $OUT/finished"
"#,
    );
    let marker = rig.out("pwned");
    let title = format!(
        "$(touch {m}) `touch {m}` ; touch {m} \"'",
        m = marker.display()
    );
    let workdir = rig.root().join("out");

    let launched = hooks::fire(&rig.ctx(), "pane.updated", &pane_payload(&workdir, &title));

    assert_eq!(launched, 1);
    assert!(
        wait_for(|| rig.out("finished").exists(), SETTLE),
        "the hook never ran; log: {:?}",
        rig.log()
    );
    let env = env_of(&std::fs::read_to_string(rig.out("env.txt")).unwrap());
    assert_eq!(env["TAW_EVENT"], "pane.updated");
    assert_eq!(env["TAW_SESSION_ID"], "$3");
    assert_eq!(env["TAW_WINDOW"], "@7");
    assert_eq!(env["TAW_PANE"], "%31");
    assert_eq!(env["TAW_AGENT"], "claude");
    assert_eq!(env["TAW_STATE"], "done");
    assert_eq!(env["TAW_DIR"], workdir.to_str().unwrap());
    assert_eq!(
        env["TAW_TITLE"], title,
        "the title arrived whole and literal"
    );
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.out("stdin.json")).unwrap()).unwrap();
    assert_eq!(stdin["event"], "pane.updated");
    assert_eq!(stdin["pane"]["terminal_title"], title);
    assert_eq!(stdin["pane"]["terminal_id"], "%31");
    assert!(!marker.exists(), "the title was run as a command");
}

#[test]
fn a_hook_that_sleeps_does_not_delay_its_caller() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "sleep 3; touch $OUT/woke"
"#,
    );

    let started = Instant::now();
    let launched = hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));
    let took = started.elapsed();

    assert_eq!(launched, 1);
    assert!(took < Duration::from_secs(2), "the caller waited {took:?}");
    assert!(!rig.out("woke").exists(), "the hook is still asleep");
    // It does run, in its own time: waiting for it also leaves no sleep behind.
    assert!(
        wait_for(|| rig.out("woke").exists(), SETTLE),
        "the hook never woke; log: {:?}",
        rig.log()
    );
}

#[test]
fn every_command_of_a_list_runs() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = ["touch $OUT/first", "touch $OUT/second"]
"#,
    );

    let launched = hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert_eq!(launched, 2);
    assert!(wait_for(
        || rig.out("first").exists() && rig.out("second").exists(),
        SETTLE
    ));
}

#[test]
fn an_event_with_no_hook_starts_nothing() {
    let rig = Rig::new("[hooks]\n\"tab.closed\" = \"touch $OUT/never\"\n");

    let launched = hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert_eq!(launched, 0);
    assert!(rig.log().is_empty());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!rig.out("never").exists());
}

#[test]
fn a_hook_starts_in_the_directory_the_event_is_about() {
    let rig = Rig::new(
        r#"
[hooks]
"pane.updated" = "pwd > $OUT/pwd"
"#,
    );
    let there = rig.root().join("out");

    hooks::fire(&rig.ctx(), "pane.updated", &pane_payload(&there, "t"));

    assert!(wait_for(|| rig.out("pwd").exists(), SETTLE));
    assert!(wait_for(
        || !std::fs::read_to_string(rig.out("pwd"))
            .unwrap_or_default()
            .is_empty(),
        SETTLE
    ));
    let pwd = std::fs::read_to_string(rig.out("pwd")).unwrap();
    assert_eq!(
        std::fs::canonicalize(pwd.trim()).unwrap(),
        std::fs::canonicalize(&there).unwrap()
    );
}

#[test]
fn the_json_is_written_whole_however_large_it_is() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "cat > $OUT/stdin.json; touch $OUT/finished"
"#,
    );
    let big = "x".repeat(300_000);

    let started = Instant::now();
    hooks::fire(
        &rig.ctx(),
        "tab.focused",
        &json!({"tab_id": "w3:t7", "layout": big}),
    );

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(wait_for(|| rig.out("finished").exists(), SETTLE));
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.out("stdin.json")).unwrap()).unwrap();
    assert_eq!(stdin["layout"].as_str().unwrap().len(), 300_000);
}

#[test]
fn a_hook_that_runs_past_its_time_limit_is_stopped_with_what_it_started_and_the_log_says_so() {
    let rig = Rig::new(
        r#"
[hooks]
timeout_seconds = 1
"tab.focused" = "sleep 60 & echo $! > $OUT/sleeper.pid; wait"
"#,
    );

    hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert!(
        wait_for(|| rig.log().contains("timed out"), SETTLE),
        "nothing was logged: {:?}",
        rig.log()
    );
    let log = rig.log();
    assert!(
        log.contains(" tab.focused timed out after 1 s: sleep 60 & echo "),
        "{log}"
    );
    let pid = std::fs::read_to_string(rig.out("sleeper.pid")).unwrap();
    let pid = pid.trim();
    let gone = wait_for(|| !is_running(pid), Duration::from_secs(10));
    let _ = Command::new("kill")
        .args(["-KILL", pid])
        .stderr(Stdio::null())
        .status();
    assert!(gone, "the sleep the hook started was left running");
}

#[test]
fn a_hook_that_fails_is_logged_with_the_last_thing_it_said() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "echo connecting >&2; echo 'curl: (6) Could not resolve host' >&2; exit 3"
"#,
    );

    hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert!(
        wait_for(|| rig.log().contains("failed"), SETTLE),
        "nothing was logged: {:?}",
        rig.log()
    );
    let log = rig.log();
    let entry = log.lines().next().unwrap();
    assert!(
        entry.contains(" tab.focused failed (exit status 3): echo connecting"),
        "{entry}"
    );
    assert!(
        entry.ends_with(" | curl: (6) Could not resolve host"),
        "{entry}"
    );
}

#[test]
fn a_hook_that_works_leaves_the_log_alone() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/finished"
"#,
    );

    hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert!(wait_for(|| rig.out("finished").exists(), SETTLE));
    std::thread::sleep(Duration::from_millis(300));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

/// A `pane.agent_status_changed` payload, as the watcher emits it.
fn status_payload(from: Option<&str>, to: Option<&str>, kind_changed: bool) -> Value {
    json!({
        "pane_id": "w3:p31",
        "workspace_id": "w3",
        "agent": "claude",
        "agent_status": to,
        "previous_status": from,
        "kind_changed": kind_changed,
        "pane": { "pane_id": "w3:p31", "workspace_id": "w3", "terminal_id": "%31", "agent": "claude" },
    })
}

/// Hooks for the status event and each agent event, each writing a file of its own name.
const STATUS_HOOKS: &str = r#"
[hooks]
"pane.agent_status_changed" = "touch $OUT/status"
"agent.blocked" = "touch $OUT/blocked"
"agent.done" = "echo $TAW_EVENT $TAW_STATE $TAW_PREVIOUS_STATE > $OUT/done"
"agent.idle" = "touch $OUT/idle"
"agent.working" = "touch $OUT/working"
"#;

/// Whether `name` is written, given that `fired` hooks have been waited for: the ones that
/// must not fire get a moment to prove it.
fn settled_without(rig: &Rig, names: &[&str]) -> bool {
    std::thread::sleep(Duration::from_millis(500));
    names.iter().all(|name| !rig.out(name).exists())
}

#[test]
fn a_state_change_fires_the_status_event_and_the_agent_event_for_the_state_it_went_into() {
    let rig = Rig::new(STATUS_HOOKS);

    let launched = hooks::fire(
        &rig.ctx(),
        "pane.agent_status_changed",
        &status_payload(Some("working"), Some("done"), false),
    );

    assert_eq!(launched, 2);
    assert!(wait_for(
        || rig.out("status").exists() && rig.out("done").exists(),
        SETTLE
    ));
    assert!(settled_without(&rig, &["blocked", "idle", "working"]));
    assert!(
        wait_for(
            || std::fs::read_to_string(rig.out("done")).is_ok_and(|text| !text.is_empty()),
            SETTLE
        ),
        "the hook wrote nothing"
    );
    // The agent event is told it is the agent event, with the states it moved between.
    assert_eq!(
        std::fs::read_to_string(rig.out("done")).unwrap().trim(),
        "agent.done done working"
    );
}

#[test]
fn a_new_occupant_of_the_pane_fires_the_status_event_and_no_agent_event() {
    let rig = Rig::new(STATUS_HOOKS);

    let launched = hooks::fire(
        &rig.ctx(),
        "pane.agent_status_changed",
        &status_payload(Some("idle"), Some("idle"), true),
    );

    assert_eq!(launched, 1);
    assert!(wait_for(|| rig.out("status").exists(), SETTLE));
    assert!(settled_without(
        &rig,
        &["blocked", "done", "idle", "working"]
    ));
}

#[test]
fn an_agent_that_leaves_its_pane_fires_the_status_event_and_no_agent_event() {
    let rig = Rig::new(STATUS_HOOKS);

    let launched = hooks::fire(
        &rig.ctx(),
        "pane.agent_status_changed",
        &status_payload(Some("working"), None, false),
    );

    assert_eq!(launched, 1);
    assert!(wait_for(|| rig.out("status").exists(), SETTLE));
    assert!(settled_without(
        &rig,
        &["blocked", "done", "idle", "working"]
    ));
}

#[test]
fn emit_starts_the_hooks_of_the_config_beside_the_plugins() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/focused"
"#,
    );

    let launched =
        taw::plugin::events::emit(&rig.ctx(), "tab.focused", json!({"tab_id": "w3:t7"})).unwrap();

    assert_eq!(launched, 1);
    assert!(wait_for(|| rig.out("focused").exists(), SETTLE));
}

/// The directory an event is about may be one a hook cannot enter: a checkout another user
/// owns, a pane left in a directory whose permissions changed. The hook is not lost over it,
/// it starts where the process that fired the event was.
#[test]
fn a_hook_whose_directory_cannot_be_entered_still_runs() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(
        r#"
[hooks]
"pane.updated" = "touch $OUT/ran"
"#,
    );
    let locked = rig.root().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let launched = hooks::fire(&rig.ctx(), "pane.updated", &pane_payload(&locked, "t"));

    let ran = wait_for(|| rig.out("ran").exists(), SETTLE);
    // So that the temp dir can be removed.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(launched, 1);
    assert!(
        ran,
        "the hook was lost over its directory; log: {:?}",
        rig.log()
    );
}

/// The `taw` a long-lived watcher started from can be moved, or replaced, while it runs. The
/// hook is then not run, and the hooks log says so: it is where the README promises a line for
/// a hook that cannot be started.
#[test]
fn a_hook_is_not_started_and_is_logged_when_the_taw_that_would_run_it_is_gone() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let gone = rig.root().join("out").join("no-such-taw");

    let launched = hooks::fire(
        &rig.ctx_with(&gone),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    assert_eq!(launched, 0, "a hook that could not be started was counted");
    let log = rig.log();
    assert!(
        log.contains(" tab.focused could not start: touch "),
        "{log}"
    );
    assert!(
        log.contains("no-such-taw"),
        "the log does not say which taw: {log}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!rig.out("ran").exists());
}

#[test]
fn a_taw_that_is_not_an_executable_file_is_no_supervisor_either() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let plain = rig.root().join("out").join("plain-file");
    std::fs::write(&plain, "#!/bin/sh\ntouch out/ran\n").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    let directory = rig.root().join("out");

    for bin in [&plain, &directory] {
        let launched = hooks::fire(
            &rig.ctx_with(bin),
            "tab.focused",
            &json!({"tab_id": "w3:t7"}),
        );

        assert_eq!(launched, 0, "{}", bin.display());
    }
    let log = rig.log();
    assert_eq!(
        log.matches("could not start").count(),
        2,
        "one line for each: {log}"
    );
    assert!(log.contains("is not an executable file"), "{log}");
}

/// A `taw` that starts and then ends without having run the hook (one whose `hooks exec` takes
/// other arguments, say) is only found out when it ends. The shell that started it is what says
/// so, in the hooks log, with how it ended.
#[test]
fn a_supervisor_that_ends_badly_is_logged_with_how_it_ended() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let old = rig.script("old-taw", "#!/bin/sh\nexit 2\n");

    let launched = hooks::fire(
        &rig.ctx_with(&old),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    // It was started, so it is counted; what went wrong is known when it ends.
    assert_eq!(launched, 1);
    assert!(
        wait_for(|| rig.log().contains("could not start"), SETTLE),
        "nothing was logged: {:?}",
        rig.log()
    );
    let log = rig.log();
    let entry = log.lines().next().unwrap();
    assert!(
        entry.contains(" tab.focused could not start: touch "),
        "{entry}"
    );
    assert!(
        entry.ends_with(&format!(" | {} ended with exit status 2", old.display())),
        "{entry}"
    );
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(!rig.out("ran").exists());
}

/// A supervisor killed by a signal is told as that, and not as an exit status the shell made up
/// by adding 128.
#[test]
fn a_supervisor_that_is_killed_is_logged_as_killed() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let killed = rig.script("killed-taw", "#!/bin/sh\nkill -KILL $$\n");

    hooks::fire(
        &rig.ctx_with(&killed),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    assert!(
        wait_for(|| rig.log().contains("could not start"), SETTLE),
        "nothing was logged: {:?}",
        rig.log()
    );
    let log = rig.log();
    assert!(
        log.trim_end()
            .ends_with(&format!(" | {} was killed by signal 9", killed.display())),
        "{log}"
    );
}

/// A verb ends a moment after it fires an event, and nothing that lives in it can say how a
/// supervisor ended. This runs one in a process of its own (the test below started as a helper),
/// that ends the instant it has started the hook, over a `taw` that takes two seconds to fail:
/// the line is written all the same, since the shell that started the supervisor outlives it.
#[test]
fn the_line_for_a_supervisor_that_ends_badly_survives_a_verb_that_ends_first() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let slow = rig.script("slow-taw", "#!/bin/sh\nsleep 2\nexit 2\n");
    // The verb reads its config from the file, as a real one does.
    std::fs::create_dir_all(&rig.paths.config_dir).unwrap();
    std::fs::write(
        &rig.paths.config_file,
        format!(
            "[hooks]\n\"tab.focused\" = \"touch {}\"\n",
            rig.out("ran").display()
        ),
    )
    .unwrap();

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_verb_that_fires_a_hook_and_ends_at_once",
            "--test-threads=1",
        ])
        .env("TAW_TEST_VERB_ROOT", rig.root())
        .env("TAW_TEST_VERB_BIN", &slow)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    assert!(status.success(), "the verb did not fire the hook");
    assert!(
        wait_for(|| rig.log().contains("could not start"), SETTLE),
        "nothing was logged after the verb ended: {:?}",
        rig.log()
    );
    assert!(
        rig.log()
            .trim_end()
            .ends_with(&format!(" | {} ended with exit status 2", slow.display())),
        "{}",
        rig.log()
    );
}

/// Not a test: the verb the one above starts in a process of its own. It fires `tab.focused` at
/// the rig it is told of, over the `taw` it is told of, and ends without waiting for anything.
#[test]
fn a_verb_that_fires_a_hook_and_ends_at_once() {
    let (Some(root), Some(bin)) = (
        std::env::var_os("TAW_TEST_VERB_ROOT"),
        std::env::var_os("TAW_TEST_VERB_BIN"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    let bin = PathBuf::from(bin);
    let cfg: Config = toml::from_str(
        &std::fs::read_to_string(root.join("config").join("config.toml"))
            .expect("the parent wrote the config"),
    )
    .unwrap();
    let paths = paths_under(&root);
    let tmux = Tmux::with_socket_name("taw-hooks-run-no-server");
    let ctx = PluginCtx {
        tmux: &tmux,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &bin,
        server_env: None,
    };

    let launched = hooks::fire(&ctx, "tab.focused", &json!({"tab_id": "w3:t7"}));

    // A verb does not wait for what it started, and neither does this: no thread of it, the
    // one that reaps the supervisor included, gets to say anything.
    std::process::exit(if launched == 1 { 0 } else { 3 });
}

/// The line is one line whatever the command holds, and what is in the command is text: it is
/// only ever an argument of the shell that writes the line, so a command with a newline, a tab,
/// quotes, a `$(...)` and a backquote comes out flattened and runs nothing.
#[test]
fn the_line_the_shell_writes_is_one_line_and_runs_nothing_of_the_command() {
    let command =
        "echo a\n\tb $(touch $OUT/pwned) `touch $OUT/pwned` \"q\" 'r' ; touch $OUT/pwned\n";
    let rig = Rig::new(&format!(
        "[hooks]\n\"tab.focused\" = {}\n",
        toml::Value::String(command.to_string())
    ));
    let failing = rig.script("failing-taw", "#!/bin/sh\nexit 127\n");

    hooks::fire(
        &rig.ctx_with(&failing),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    assert!(
        wait_for(|| rig.log().ends_with('\n'), SETTLE),
        "nothing was logged: {:?}",
        rig.log()
    );
    let log = rig.log();
    assert_eq!(log.lines().count(), 1, "{log}");
    let expected = format!(
        " tab.focused could not start: echo a  b $(touch {0}) `touch {0}` \"q\" 'r' ; touch {0} | {1} ended with exit status 127",
        rig.out("pwned").display(),
        failing.display()
    );
    assert!(log.trim_end().ends_with(&expected), "{log}");
    assert!(!rig.out("pwned").exists(), "the command was run");
}

/// The log is moved aside to `hooks.log.1` when it passes 256 KiB, before the next line, whoever
/// writes it: the shell's line included.
#[test]
fn the_shell_moves_a_log_that_has_grown_past_its_size_aside_before_it_writes() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let log = hooks::log::path(&rig.paths);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "x".repeat(300_000)).unwrap();
    let failing = rig.script("failing-taw", "#!/bin/sh\nexit 1\n");

    hooks::fire(
        &rig.ctx_with(&failing),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    assert!(
        wait_for(|| rig.log().contains("could not start"), SETTLE),
        "nothing was logged: {:?}",
        rig.log().len()
    );
    assert_eq!(rig.log().lines().count(), 1, "{}", rig.log());
    let aside = log.with_extension("log.1");
    assert_eq!(std::fs::metadata(&aside).unwrap().len(), 300_000);
}

#[test]
fn a_supervisor_that_ends_well_is_not_logged() {
    let rig = Rig::new(
        r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
    );
    let fine = rig.script(
        "fine-taw",
        &format!(
            "#!/bin/sh\ntouch '{}'\nexit 0\n",
            rig.out("ended").display()
        ),
    );

    let launched = hooks::fire(
        &rig.ctx_with(&fine),
        "tab.focused",
        &json!({"tab_id": "w3:t7"}),
    );

    assert_eq!(launched, 1);
    assert!(wait_for(|| rig.out("ended").exists(), SETTLE));
    std::thread::sleep(Duration::from_millis(300));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

/// What a hook prints to stdout is thrown away, and nothing waits for it to close: a process the
/// hook leaves running that holds only its stdout (its stderr sent elsewhere) does not make the
/// hook late. Waited for, it would count against the limit, and the hook would be logged as timed
/// out though its own shell ended well.
#[test]
fn a_process_a_hook_leaves_holding_only_its_stdout_does_not_make_it_time_out() {
    let rig = Rig::new(
        r#"
[hooks]
timeout_seconds = 1
"tab.focused" = "sleep 30 2>/dev/null & echo $! > $OUT/holder.pid; touch $OUT/finished"
"#,
    );

    hooks::fire(&rig.ctx(), "tab.focused", &json!({"tab_id": "w3:t7"}));

    assert!(wait_for(|| rig.out("finished").exists(), SETTLE));
    // Long enough for the limit to have come and gone twice.
    std::thread::sleep(Duration::from_millis(2500));
    let log = rig.log();
    let pid = std::fs::read_to_string(rig.out("holder.pid")).unwrap_or_default();
    let _ = Command::new("kill")
        .args(["-KILL", pid.trim()])
        .stderr(Stdio::null())
        .status();
    assert!(log.is_empty(), "the hook was logged: {log}");
}

/// The README says a hook starts in the directory the event is about when it can, and else in
/// the home directory, "whichever process fired the event": a directory that exists and cannot
/// be entered is one it cannot start in, so it is the home directory and not wherever the process
/// that fired the event happens to be, which would differ from one process to the next.
#[test]
fn a_hook_whose_directory_cannot_be_entered_starts_in_the_home_directory() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new(
        r#"
[hooks]
"pane.updated" = "pwd -P > $OUT/pwd"
"#,
    );
    let locked = rig.root().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A user that may enter any directory (root) has nothing to fall back from.
    let enterable = std::fs::read_dir(&locked).is_ok();

    hooks::fire(&rig.ctx(), "pane.updated", &pane_payload(&locked, "t"));

    let wrote = wait_for(
        || std::fs::read_to_string(rig.out("pwd")).is_ok_and(|text| !text.trim().is_empty()),
        SETTLE,
    );
    // So that the temp dir can be removed.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(wrote, "the hook did not run; log: {:?}", rig.log());
    if enterable {
        return;
    }
    let started_in = std::fs::read_to_string(rig.out("pwd")).unwrap();
    let home = dirs::home_dir().expect("a home directory");
    assert_eq!(
        std::fs::canonicalize(started_in.trim()).unwrap(),
        std::fs::canonicalize(home).unwrap(),
        "the hook started in {}",
        started_in.trim()
    );
}

/// What the Rust side writes to the hooks log for `event`, `command` and a last line of stderr,
/// without its time: the line the shell that starts a supervisor has to write the same as.
fn line_the_log_writes(event: &str, command: &str, stderr: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("hooks.log");
    hooks::log::record(&log, event, "could not start", command, stderr);
    without_the_time(&std::fs::read_to_string(&log).unwrap())
}

/// A log line without its time (`2026-09-30 10:11:12 UTC`), which is when it was written.
fn without_the_time(line: &str) -> String {
    line.splitn(4, ' ').nth(3).unwrap_or_default().to_string()
}

/// The line the shell writes for a supervisor that ended badly is the line the Rust side writes,
/// character for character but for the time: what is control (C0, DEL and C1) is a space, the
/// ends lose what Unicode calls white space, a multi-byte character is one character, and the
/// command is kept whole however long it is.
#[test]
fn the_shell_writes_the_line_the_rust_log_writes() {
    let commands = [
        "touch plain".to_string(),
        "a\tb\nc\rd\u{1b}[31me\u{7f}f".to_string(),
        // C1 controls: U+0080, NEL, CSI and the last of them.
        "a\u{80}b\u{85}c\u{9b}d\u{9f}e".to_string(),
        // Characters of several widths, one whose second byte is one that C1 controls end in.
        "x\u{100}y \u{1f600} \u{e9}".to_string(),
        // White space at the ends is trimmed, and inside is kept.
        "\u{a0}\u{2003} spaced out \u{3000}\u{a0}".to_string(),
        "inner\u{a0}space stays".to_string(),
        format!("{}\u{e9}\u{100}{}", "x".repeat(700), "y".repeat(700)),
    ];
    for command in commands {
        let rig = Rig::new(&format!(
            "[hooks]\n\"tab.focused\" = {}\n",
            toml::Value::String(command.clone())
        ));
        let failing = rig.script("failing-taw", "#!/bin/sh\nexit 2\n");

        hooks::fire(
            &rig.ctx_with(&failing),
            "tab.focused",
            &json!({"tab_id": "w3:t7"}),
        );

        assert!(
            wait_for(|| rig.log().ends_with('\n'), SETTLE),
            "nothing was logged for {command:?}"
        );
        let excerpt = format!("{} ended with exit status 2", failing.display());
        assert_eq!(
            without_the_time(&rig.log()),
            line_the_log_writes("tab.focused", &command, &excerpt),
            "{command:?}"
        );
    }
}

/// The excerpt, which is where the `taw` is named, is cut at 300 characters and `...` follows, as
/// the Rust line cuts it: 300 characters and not 300 bytes, so that a cut inside a run of
/// two-byte characters leaves whole ones.
#[test]
fn the_shell_cuts_the_excerpt_at_300_characters_as_the_rust_line_does() {
    for (name, piece) in [("ascii", "a"), ("two-byte", "\u{e9}")] {
        let rig = Rig::new(
            r#"
[hooks]
"tab.focused" = "touch $OUT/ran"
"#,
        );
        // A path of well over 300 characters: two long directories, each named by one character.
        let deep = rig
            .root()
            .join("out")
            .join(piece.repeat(120))
            .join(piece.repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let failing = deep.join("taw");
        {
            std::fs::write(&failing, "#!/bin/sh\nexit 2\n").unwrap();
            crate::exec::make_executable(&failing);
        }
        let excerpt = format!("{} ended with exit status 2", failing.display());
        assert!(excerpt.chars().count() > 300, "{name}: {}", excerpt.len());

        hooks::fire(
            &rig.ctx_with(&failing),
            "tab.focused",
            &json!({"tab_id": "w3:t7"}),
        );

        assert!(
            wait_for(|| rig.log().ends_with('\n'), SETTLE),
            "{name}: nothing was logged"
        );
        let log = rig.log();
        let line = without_the_time(&log);
        assert_eq!(
            line,
            line_the_log_writes(
                "tab.focused",
                &format!("touch {}/ran", rig.root().join("out").display()),
                &excerpt
            ),
            "{name}"
        );
        let shown = line.trim_end().rsplit_once(" | ").unwrap().1;
        assert_eq!(
            shown.chars().count(),
            303,
            "{name}: 300 characters and the dots"
        );
        assert!(shown.ends_with("..."), "{name}");
    }
}
