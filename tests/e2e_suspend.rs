//! An agent suspended with Ctrl+Z keeps its identity and its session (herdr v0.9.4, #4964): end to
//! end through the built `taw` binary and a real `taw watch run` on a throwaway tmux server.
//!
//! The Claude is a copy of `sleep` called `claude`, so that tmux reports `claude` as the pane's
//! command for as long as it runs in front, and so that Ctrl+Z stops it as it stops any program
//! that leaves the terminal's signals alone (a copy of tmux, the other fixtures' stand-in, ignores
//! the stop). A script on the pane's `$PATH` records its pid and then becomes that copy. Its hook's
//! report is the real hook's call, with Claude's shape of session id.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::owned::{alive, args_of, signal};
use common::{Server, ServerOptions, read_pid, taw_bin, wait_for, write_script};
use taw::store::{PROCESS_OPTION, SUSPENDED_RULE, SUSPENDED_WORD};

/// Ceiling for "the watcher noticed and wrote the new state": `cargo test` runs these files in
/// parallel, and the gate runs them on a loaded machine.
const SETTLE: Duration = Duration::from_secs(20);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// Claude's session ids are UUIDs.
const SESSION: &str = "5b2e9c1a-3f47-4d8e-a6b0-7c19e2d4f863";

/// The fake's process, killed by pid when the test ends however it ends: a stopped job outlives
/// the server that ran it unless something sends it a signal it cannot ignore.
struct Fake {
    pid: u32,
    copy: String,
}

impl Drop for Fake {
    fn drop(&mut self) {
        // Only the process the test started: alive, and still running the copy.
        if alive(self.pid) && args_of(self.pid).contains(&self.copy) {
            signal(self.pid, libc::SIGKILL);
        }
    }
}

/// `bin/claude` in `dir`: records its pid in `pid_file`, then becomes a copy of `sleep` called
/// `claude`. Returns the `bin` directory, for the pane shell's `$PATH`, and the copy's path.
fn fake_claude(dir: &Path, pid_file: &Path) -> (std::path::PathBuf, String) {
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let copy = copy_dir.join("claude");
    std::fs::copy("/bin/sleep", &copy).unwrap();
    common::make_executable(&copy);
    // macOS kills a copy of one of its own programs on its first instruction (the copy is no longer
    // where its signature says it lives); signed again for itself, it runs.
    if cfg!(target_os = "macos") {
        let signed = Command::new("codesign")
            .args(["-s", "-", "-f"])
            .arg(&copy)
            .output()
            .unwrap();
        assert!(signed.status.success(), "codesign: {signed:?}");
    }
    let bin = dir.join("bin");
    write_script(
        &bin.join("claude"),
        &format!(
            "echo $$ > '{}'\nexec '{}' 600\n",
            pid_file.display(),
            copy.display()
        ),
    );
    (bin, copy.display().to_string())
}

/// What `ps` says of the process's state (`T` for stopped), empty once it has gone.
fn stat_of(pid: u32) -> String {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The record the watcher and the hook leave: kind, state, rule and session id, in that order.
fn record(server: &Server, pane: &str) -> [String; 4] {
    [
        "@agent_kind",
        "@agent_state",
        "@agent_rule",
        "@agent_session_id",
    ]
    .map(|name| server.pane_opt(pane, name))
}

fn command_of(server: &Server, pane: &str) -> String {
    server.display(pane, "#{pane_current_command}")
}

/// The popup, run in a window of its own on the same server, until its screen holds `word`.
fn popup_shows(server: &Server, word: &str) -> String {
    let env: String = server
        .taw_env()
        .iter()
        .map(|(name, value)| format!("'{name}={value}' "))
        .collect();
    let line = format!("env -u TMUX -u TMUX_PANE {env}'{}' app", taw_bin());
    let popup = server
        .tmux_ok(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-n",
            "popup",
            &line,
        ])
        .trim()
        .to_string();
    let found = wait_for(|| server.capture(&popup).contains(word), SETTLE);
    let screen = server.capture(&popup);
    server.tmux_ok(&["kill-window", "-t", "popup"]);
    assert!(found, "the popup never drew {word:?}:\n{screen}");
    screen
}

#[test]
fn an_agent_suspended_with_ctrl_z_keeps_its_kind_and_session_through_the_stop_and_fg() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("claude.pid");
    let (bin, copy) = fake_claude(tmp.path(), &pid_file);
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        path_prefix: Some(bin),
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    server.tmux_ok(&["send-keys", "-t", &pane, "claude", "Enter"]);
    assert!(
        wait_for(|| command_of(&server, &pane) == "claude", SETTLE),
        "the fake never took the terminal (the pane runs {:?})",
        command_of(&server, &pane)
    );
    let mut pid = None;
    assert!(
        wait_for(
            || {
                pid = read_pid(&pid_file).and_then(|text| text.trim().parse::<u32>().ok());
                pid.is_some()
            },
            SETTLE
        ),
        "the fake never wrote its pid"
    );
    let fake = Fake {
        pid: pid.unwrap(),
        copy,
    };

    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind") == "claude"
                && server.pane_opt(&pane, "@agent_state") == "idle",
            SETTLE
        ),
        "the watcher never took Claude for idle: {:?}\n{}",
        record(&server, &pane),
        server.watcher_logs()
    );
    server.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            SESSION,
        ],
        "cli:pane:report-agent-session",
        "ok",
    );

    // Ctrl+Z: the process stops and the shell takes the terminal back.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-z"]);
    assert!(
        wait_for(
            || stat_of(fake.pid).starts_with('T') && command_of(&server, &pane) != "claude",
            SETTLE
        ),
        "the fake never stopped behind the shell (stat {:?}, the pane runs {:?})",
        stat_of(fake.pid),
        command_of(&server, &pane)
    );
    // The positive wait: the watcher has read the stop and says so.
    let suspended = ["claude", "idle", SUSPENDED_RULE, SESSION].map(str::to_string);
    assert!(
        wait_for(|| record(&server, &pane) == suspended, SETTLE),
        "after Ctrl+Z the record is {:?}\n{}",
        record(&server, &pane),
        server.watcher_logs()
    );

    // The popup's row says so.
    popup_shows(&server, SUSPENDED_WORD);

    // A save taken meanwhile holds the session, so a restore resumes it.
    let save = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    let snapshot = std::fs::read_to_string(save["path"].as_str().unwrap()).unwrap();
    assert!(
        snapshot.contains(SESSION) && snapshot.contains("\"claude\""),
        "the save taken while the agent was suspended lost it:\n{snapshot}"
    );

    // A watcher started while the agent is stopped (a restart after an update, a plugin reload)
    // takes back the note the last one kept on the pane, and keeps the agent.
    assert_eq!(
        server.pane_opt(&pane, PROCESS_OPTION),
        format!("{} claude claude", fake.pid),
        "the note on the pane"
    );
    server.ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    assert!(
        wait_for(|| server.piped(&pane) == "0", SETTLE),
        "the watcher never let go of the pane"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    // The positive wait: the new watcher has taken the note back. Only a watcher that starts with a
    // note on the pane says so, and the record check below cannot pass on the old watcher's write.
    assert!(
        wait_for(
            || server
                .watcher_logs()
                .contains("noted before this watcher started"),
            SETTLE
        ),
        "the new watcher never took the note back\n{}",
        server.watcher_logs()
    );
    assert_eq!(
        record(&server, &pane),
        suspended,
        "a watcher started during the suspend took the agent for gone\n{}",
        server.watcher_logs()
    );

    // `fg`: the same agent again, with the same session, and no longer suspended.
    server.tmux_ok(&["send-keys", "-t", &pane, "fg", "Enter"]);
    assert!(
        wait_for(
            || command_of(&server, &pane) == "claude" && !stat_of(fake.pid).starts_with('T'),
            SETTLE
        ),
        "fg never brought the fake back (the pane runs {:?})",
        command_of(&server, &pane)
    );
    assert!(
        wait_for(
            || {
                let [kind, state, rule, session] = record(&server, &pane);
                kind == "claude" && state == "idle" && rule != SUSPENDED_RULE && session == SESSION
            },
            SETTLE
        ),
        "after fg the record is {:?}\n{}",
        record(&server, &pane),
        server.watcher_logs()
    );

    // The agent exits: the note on the pane goes with it. It is there while the agent runs, and the
    // wait for the exit comes first, so a note that is simply not written yet is not taken for gone.
    assert_eq!(
        server.pane_opt(&pane, PROCESS_OPTION),
        format!("{} claude claude", fake.pid),
        "the note on the pane while the agent runs"
    );
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(
            || !alive(fake.pid)
                && command_of(&server, &pane) != "claude"
                && server.pane_opt(&pane, "@agent_kind").is_empty(),
            SETTLE
        ),
        "the agent never exited (the pane runs {:?}, record {:?})\n{}",
        command_of(&server, &pane),
        record(&server, &pane),
        server.watcher_logs()
    );
    assert!(
        wait_for(|| server.pane_opt(&pane, PROCESS_OPTION).is_empty(), SETTLE),
        "the note of the agent's process outlived the agent: {:?}\n{}",
        server.pane_opt(&pane, PROCESS_OPTION),
        server.watcher_logs()
    );
    drop(fake);
}
