//! A Letta whose command taw cannot read keeps its session id, end to end through the built
//! `taw` binary, and `session restore` resumes it.
//!
//! npm's Letta runs under `node`, so tmux reports that interpreter for its pane and nothing
//! about the command says Letta. The one thing that does is Letta's own `SessionStart` hook,
//! which `taw integration install letta` writes and which reports the conversation with
//! `taw pane report-agent-session`. That report sets `@agent_kind` and `@agent_session_id`
//! and no state, so a watcher could take the pane for one whose agent had gone once the
//! command had failed to name Letta six times, about 25 s later, and clear both. `session
//! restore` then found no conversation to resume.
//!
//! herdr keeps an agent it cannot identify by its process until the pane's own shell is idle,
//! and taw does the same, so what a pane like this survives is asked here: no command in front
//! of it ends it, and neither does a shell the user started inside the pane, only the pane's
//! own shell at its prompt. And what takes the pane over afterwards, a Claude taw does name
//! from its process, follows its own rules and inherits nothing of the Letta's record.
//!
//! The fake is a program run by `node`, so the pane's command is `node` and not an agent's or
//! a shell's; it draws the composer Letta draws at rest and draws it again on each SIGUSR1,
//! which is the output that makes the watcher read a pane it has not identified. The hook is
//! the real one, installed under a scratch `$HOME`, and run as Letta runs it: with `$TMUX_PANE`
//! set and the event on its standard input. The watcher is a real `taw watch run` on a
//! throwaway tmux server, and everything is read back from tmux's own options.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{
    Server, ServerOptions, make_executable, read_pid, taw_bin, tmux_named, wait_for, write_script,
};

/// Ceiling for "the watcher noticed and wrote the new state", well above what an idle machine
/// needs because `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(20);

/// How many more reads of a pane it holds as an agent the test waits for before it believes
/// the watcher would have cleared it. The read that follows the report counts the first miss,
/// each read 5 s after the last counts another, and the sixth clears: five reads after that
/// first one, and one more is the margin.
const READS_TO_OUTLAST_THE_MISSES: usize = 6;

/// Reads are 5 s apart on a quiet pane, so this is the wait for them with a wide margin for
/// a machine that is busy with other tests.
const READS_CEILING: Duration = Duration::from_secs(150);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The fake Letta. Its only argument is where it records its pid.
const FAKE_LETTA: &str = r#"#!/usr/bin/env node
const fs = require("fs");

const COMPOSER =
  "\x1b]2;Tutor\x07\x1b[2J\x1b[H" +
  "────────────────\n" +
  "› Try \"debug this error\"\n" +
  "────────────────\n" +
  "Tutor · No model selected\n";

const draw = () => process.stdout.write(COMPOSER);

process.on("SIGUSR1", draw);
process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
draw();
fs.writeFileSync(process.argv[2], String(process.pid));
setInterval(() => {}, 200);
"#;

fn have(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `taw integration install letta` with `$HOME` moved into `root`, so nothing can reach the
/// developer's own `~/.letta`. Returns the installed hook script.
fn install_the_hook(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join(".letta")).unwrap();
    let output = Command::new(taw_bin())
        .args(["integration", "install", "letta"])
        .env("HOME", root)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run taw integration install letta");
    assert!(
        output.status.success(),
        "taw integration install letta failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let script = root.join(".letta/hooks/taw-agent-session.sh");
    assert!(script.is_file(), "{} was not installed", script.display());
    script
}

/// Letta running `hook` for a `SessionStart` event: `$TMUX_PANE` set to `pane`, the event on
/// its standard input, and the throwaway server's socket and paths in the environment so the
/// `taw` the hook calls talks to that server.
fn fire_the_hook(server: &Server, hook: &Path, pane: &str, event: &str) {
    let mut child = Command::new("sh")
        .arg(hook)
        .arg("session")
        .env("TMUX_PANE", pane)
        .env("TAW_TMUX_SOCKET_NAME", server.socket())
        .env("TAW_CONFIG_DIR", server.config_dir())
        .env("TAW_STATE_DIR", server.state_dir())
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.as_bytes())
        .expect("write the event");
    let output = child.wait_with_output().expect("wait for the hook");
    assert!(
        output.status.success(),
        "the hook exited non-zero: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Letta puts a SessionStart hook's output in front of the model, so it has none.
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "the hook printed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn signal(pid: &str) {
    let status = Command::new("kill").args(["-USR1", pid]).status().unwrap();
    assert!(status.success(), "kill -USR1 {pid} failed");
}

fn terminate(pid: &str) {
    let status = Command::new("kill").args(["-TERM", pid]).status().unwrap();
    assert!(status.success(), "kill -TERM {pid} failed");
}

/// A shell the user started inside the pane: `bash` typed at its prompt, the shape of `nix
/// develop` or `sudo -s`. It has a prompt of its own to wait for.
fn nest_a_shell(server: &Server, pane: &str) {
    for line in ["bash --norc --noprofile", "PS1='nested$ '"] {
        server.tmux_ok(&["send-keys", "-t", pane, line, "Enter"]);
    }
    assert!(
        wait_for(|| at_the_nested_prompt(server, pane), SETTLE),
        "the nested shell never prompted:\n{}",
        server.capture(pane)
    );
}

fn at_the_nested_prompt(server: &Server, pane: &str) -> bool {
    server.capture(pane).trim_end().ends_with("nested$")
}

/// A pane running the fake, with a watcher on it, and its pid.
struct Wrapped {
    pane: String,
    pid: String,
}

/// Starts the fake in `pane`, waits for it to draw, and attaches a watcher.
fn start_the_fake(server: &Server, fake: &Path, pid_file: &Path, pane: &str) -> Wrapped {
    server.tmux_ok(&[
        "send-keys",
        "-t",
        pane,
        &format!("'{}' '{}'", fake.display(), pid_file.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || server.capture(pane).contains("No model selected") && read_pid(pid_file).is_some(),
            SETTLE
        ),
        "the fake never drew its composer:\n{}",
        server.capture(pane)
    );
    let command = server.display(pane, "#{pane_current_command}");
    assert!(
        !command.to_lowercase().contains("letta")
            && !taw::watch::state::SHELLS.contains(&command.trim_start_matches('-')),
        "the pane's command must name neither Letta nor a shell, it is {command:?}"
    );
    server.ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
    Wrapped {
        pane: pane.to_string(),
        pid: read_pid(pid_file).unwrap(),
    }
}

#[test]
fn a_letta_run_under_an_interpreter_keeps_its_session_id_and_restore_resumes_it() {
    if !have("python3") || !have("node") || !have("bash") {
        eprintln!(
            "skipping: python3 (the letta hook parses JSON with it), node (the fake runs under \
             it) and bash (the nested shell) are all needed"
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let resumed = tmp.path().join("resumed.txt");
    // What `session restore` types: `letta --conversation ...`. It records its argument
    // list and returns, so the assertion is about what was *executed* in the pane, not about
    // text left on a prompt line.
    write_script(
        &bin_dir.join("letta"),
        &format!("printf '%s\\n' \"letta $*\" >> '{}'\n", resumed.display()),
    );
    let fake = tmp.path().join("letta-under-node");
    std::fs::write(&fake, FAKE_LETTA).unwrap();
    make_executable(&fake);

    let server = Server::start_with(ServerOptions {
        width: 200,
        height: 50,
        path_prefix: Some(bin_dir),
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    // Four panes that hold a Letta, in four sessions of their own, and one session that is
    // never killed, so the server has one to resolve against while the others are restored.
    // The last runs it inside a shell of the user's own.
    for name in ["two", "three", "nested", "keepalive"] {
        server.tmux_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "200",
            "-y",
            "50",
            "-c",
            server.work_dir().to_str().unwrap(),
        ]);
    }
    let hook = install_the_hook(&tmp.path().join("home"));

    let conversation = start_the_fake(&server, &fake, &tmp.path().join("one.pid"), &server.pane());
    let default = start_the_fake(
        &server,
        &fake,
        &tmp.path().join("two.pid"),
        &server.pane_of("two"),
    );
    let leaver = start_the_fake(
        &server,
        &fake,
        &tmp.path().join("three.pid"),
        &server.pane_of("three"),
    );
    let nested = server.pane_of("nested");
    nest_a_shell(&server, &nested);
    let nested = start_the_fake(&server, &fake, &tmp.path().join("four.pid"), &nested);

    // Letta reports its conversation. The first is a conversation of its own; the second is
    // the agent's default one, which the hook writes as `default:<agent id>`; the third
    // will end at the pane's own shell, and the last in a shell of the user's.
    for (wrapped, event) in [
        (
            &conversation,
            r#"{"conversation_id":"conv-node-1","agent_id":"agent-1","is_new_session":true}"#,
        ),
        (
            &default,
            r#"{"conversation_id":"default","agent_id":"agent-2","is_new_session":false}"#,
        ),
        (
            &leaver,
            r#"{"conversation_id":"conv-leaver","agent_id":"agent-3","is_new_session":true}"#,
        ),
        (
            &nested,
            r#"{"conversation_id":"conv-nested","agent_id":"agent-4","is_new_session":true}"#,
        ),
    ] {
        fire_the_hook(&server, &hook, &wrapped.pane, event);
        // Letta draws again as it goes, which is what makes the watcher read the pane.
        signal(&wrapped.pid);
    }
    for (wrapped, id) in [
        (&conversation, "conv-node-1"),
        (&default, "default:agent-2"),
        (&leaver, "conv-leaver"),
        (&nested, "conv-nested"),
    ] {
        assert_eq!(
            server.pane_opt(&wrapped.pane, "@agent_session_id"),
            id,
            "the hook's report did not land"
        );
        assert_eq!(server.pane_opt(&wrapped.pane, "@agent_kind"), "letta");
        // The watcher reads such a pane as the Letta it says it is: its state comes from
        // the screen, by Letta's own rules.
        assert!(
            wait_for(
                || server.pane_opt(&wrapped.pane, "@agent_state") == "idle"
                    && server.pane_opt(&wrapped.pane, "@agent_rule") == "composer_idle",
                SETTLE
            ),
            "the composer was not read as idle: {:?} by {:?}\n--- log ---\n{}",
            server.pane_opt(&wrapped.pane, "@agent_state"),
            server.pane_opt(&wrapped.pane, "@agent_rule"),
            server.watcher_logs(),
        );
        assert_eq!(server.pane_opt(&wrapped.pane, "@agent_source"), "manifest");
    }

    // The watcher goes on reading, and the record stays: by the seventh read a command that
    // never named Letta would have cost the pane its agent.
    let kept = [&conversation, &default];
    let before = kept.map(|wrapped| server.watcher_reads(&wrapped.pane));
    let waited = wait_for(
        || {
            let gone = kept.iter().any(|wrapped| {
                server
                    .pane_opt(&wrapped.pane, "@agent_session_id")
                    .is_empty()
            });
            let outlasted = kept.iter().zip(before).all(|(wrapped, before)| {
                server.watcher_reads(&wrapped.pane) >= before + READS_TO_OUTLAST_THE_MISSES
            });
            gone || outlasted
        },
        READS_CEILING,
    );
    assert!(
        waited,
        "the watcher did not read the panes {READS_TO_OUTLAST_THE_MISSES} times in {READS_CEILING:?}\n{}",
        server.watcher_logs()
    );
    for (wrapped, id) in [
        (&conversation, "conv-node-1"),
        (&default, "default:agent-2"),
    ] {
        assert_eq!(
            server.pane_opt(&wrapped.pane, "@agent_session_id"),
            id,
            "the watcher cleared the session id of a Letta it had been told about\n{}",
            server.watcher_logs()
        );
        assert_eq!(server.pane_opt(&wrapped.pane, "@agent_kind"), "letta");
    }

    // Keeping it is not keeping it for ever: when Letta ends and the shell is back, the
    // record goes, as it does for every agent.
    server.tmux_ok(&["send-keys", "-t", &leaver.pane, "C-c"]);
    assert!(
        wait_for(
            || server.pane_opt(&leaver.pane, "@agent_kind").is_empty()
                && server
                    .pane_opt(&leaver.pane, "@agent_session_id")
                    .is_empty(),
            SETTLE
        ),
        "the shell came back and the record stayed: kind {:?}, session {:?}\n{}",
        server.pane_opt(&leaver.pane, "@agent_kind"),
        server.pane_opt(&leaver.pane, "@agent_session_id"),
        server.watcher_logs()
    );

    // A Letta that quits into a shell the user started inside the pane. That shell is not the
    // pane's own, and nothing else says the agent has gone, so the record stays, session id
    // and all, through as many reads as the ordinary rule would have needed to end it.
    terminate(&nested.pid);
    assert!(
        wait_for(|| at_the_nested_prompt(&server, &nested.pane), SETTLE),
        "the nested shell never came back:\n{}",
        server.capture(&nested.pane)
    );
    let before = server.watcher_reads(&nested.pane);
    assert!(
        wait_for(
            || server.watcher_reads(&nested.pane) >= before + 3,
            READS_CEILING
        ),
        "the watcher did not read the pane 3 more times\n{}",
        server.watcher_logs()
    );
    assert_eq!(
        server.pane_opt(&nested.pane, "@agent_session_id"),
        "conv-nested",
        "a shell the user started ended a Letta taw cannot name\n{}",
        server.watcher_logs()
    );
    assert_eq!(server.pane_opt(&nested.pane, "@agent_kind"), "letta");

    // The nested shell ends, and the pane's own is at its prompt: now the record goes.
    server.tmux_ok(&["send-keys", "-t", &nested.pane, "exit", "Enter"]);
    assert!(
        wait_for(
            || server.pane_opt(&nested.pane, "@agent_kind").is_empty()
                && server
                    .pane_opt(&nested.pane, "@agent_session_id")
                    .is_empty(),
            SETTLE
        ),
        "the pane's own shell came back and the record stayed: kind {:?}, session {:?}\n{}",
        server.pane_opt(&nested.pane, "@agent_kind"),
        server.pane_opt(&nested.pane, "@agent_session_id"),
        server.watcher_logs()
    );

    // What a restart of the machine would do: save, lose the two sessions, restore.
    server.ok(&["session", "save"], "cli:session:save", "session_saved");
    server.tmux_ok(&["kill-session", "-t", "one"]);
    server.tmux_ok(&["kill-session", "-t", "two"]);
    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        restored["resumed"].as_array().unwrap().len(),
        2,
        "both saved Letta panes should be resumed: {restored}"
    );
    let mut want = vec![
        "letta --conversation conv-node-1".to_string(),
        "letta --conversation default --agent agent-2".to_string(),
    ];
    want.sort();
    assert!(
        wait_for(
            || {
                let mut got: Vec<String> = std::fs::read_to_string(&resumed)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                got.sort();
                got == want
            },
            SETTLE
        ),
        "wanted {want:?} to have been run in the restored panes, got {:?}",
        std::fs::read_to_string(&resumed)
    );
}

/// A Letta that quit into a shell the user started inside its pane is still on the pane, kept for
/// the pane's own shell, and a Claude started in that shell is a different agent: taw names it
/// from its process, so the record follows it, and carries nothing of the Letta's. Not the
/// conversation, which `session restore` would resume under Claude, and not the Letta's word that
/// it was there.
#[test]
fn a_claude_that_takes_a_pane_from_a_letta_carries_nothing_of_its_record() {
    if !have("python3") || !have("node") || !have("bash") {
        eprintln!(
            "skipping: python3 (the letta hook parses JSON with it), node (the fake runs under \
             it) and bash (the nested shell) are all needed"
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("letta-under-node");
    std::fs::write(&fake, FAKE_LETTA).unwrap();
    make_executable(&fake);
    let claude = tmux_named(tmp.path(), "claude");
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let hook = install_the_hook(&tmp.path().join("home"));

    let pane = server.pane();
    nest_a_shell(&server, &pane);
    let letta = start_the_fake(&server, &fake, &tmp.path().join("letta.pid"), &pane);
    fire_the_hook(
        &server,
        &hook,
        &pane,
        r#"{"conversation_id":"conv-successor","agent_id":"agent-5","is_new_session":true}"#,
    );
    signal(&letta.pid);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "idle", SETTLE),
        "the composer was not read as idle\n{}",
        server.watcher_logs()
    );

    // Letta quits into the nested shell, and is kept there through a few reads.
    terminate(&letta.pid);
    assert!(
        wait_for(|| at_the_nested_prompt(&server, &pane), SETTLE),
        "the nested shell never came back:\n{}",
        server.capture(&pane)
    );
    let before = server.watcher_reads(&pane);
    assert!(
        wait_for(|| server.watcher_reads(&pane) >= before + 2, READS_CEILING),
        "the watcher did not read the pane 2 more times\n{}",
        server.watcher_logs()
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_session_id"),
        "conv-successor"
    );

    // A Claude starts in the nested shell.
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &server.wait_line(&claude, "quit"),
        "Enter",
    ]);
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind") == "claude"
                && server.pane_opt(&pane, "@agent_session_id").is_empty()
                && server.pane_opt(&pane, "@agent_reported_kind").is_empty(),
            SETTLE
        ),
        "the Claude that took the pane carries the Letta's record: kind {:?}, session {:?}, \
         reported {:?}\n{}",
        server.pane_opt(&pane, "@agent_kind"),
        server.pane_opt(&pane, "@agent_session_id"),
        server.pane_opt(&pane, "@agent_reported_kind"),
        server.watcher_logs()
    );

    // And it ends as an agent taw names does: with its process, at the shell in front of it.
    server.wake("quit");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind").is_empty(), SETTLE),
        "Claude quit into the nested shell and the record stayed\n{}",
        server.watcher_logs()
    );
}
