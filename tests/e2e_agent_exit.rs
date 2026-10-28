//! An agent `taw agent start` started that exits, end to end through the built binary, a real
//! `taw watch run` and a real tmux server.
//!
//! The pane of such an agent is named by its kind hint alone when the agent's process is a script
//! or a wrapper, and the hint outlives the agent. When the agent died mid-turn its last frame stayed
//! on screen, the rules of its kind read that frame as a turn under way, and the exception that
//! keeps a live turn from being cut off by a frame caught mid-redraw kept the pane `working` for
//! good: no `done` was announced, and every wait for the agent ran to its `--timeout`, or for ever.
//!
//! The agent is the fake agent of the other suites: a script that draws a Claude's idle screen, goes
//! to its working screen on `SIGUSR1`, and ends on `SIGTERM`. A watcher is attached to its pane, as
//! the plugin's hooks attach one.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{
    FakeAgent, NotifyRecorder, Server, ServerOptions, tmux_named, wait_for, write_script,
};

/// Ceiling for "the watcher noticed and wrote the new state": a new agent reads `unknown` for 3 s
/// and one more tick first, and `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(25);

/// Notifications delivered through `$TAW_NOTIFY_CMD`, the done one after two seconds: long enough
/// for the pane to have moved on from the exit's `done` when the delay is up.
const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"system\"
delay_seconds = 0
done_delay_seconds = 2

[ui.sound]
enabled = false
";

/// The environment of every process of a server whose shells are typed into: a home of their own
/// and no history file, since a shell saves what it was typed when the server is killed, to
/// `$HISTFILE`, which is `~/.bash_history` where nothing says otherwise.
fn private_shells(home: &std::path::Path) -> Vec<(String, String)> {
    std::fs::create_dir_all(home).unwrap();
    vec![
        ("HISTFILE".to_string(), "/dev/null".to_string()),
        ("HOME".to_string(), home.display().to_string()),
    ]
}

/// A server whose panes find the fake agent as `claude`, and whose notifier is a recorder.
fn server(tmp: &std::path::Path) -> (Server, FakeAgent, NotifyRecorder) {
    let fake = FakeAgent::new(tmp);
    let recorder = NotifyRecorder::new(tmp);
    let mut env = private_shells(&tmp.join("home"));
    env.push((
        "TAW_NOTIFY_CMD".to_string(),
        recorder.script.display().to_string(),
    ));
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env,
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    (server, fake, recorder)
}

/// Attaches a watcher to `pane` and waits for it to have recorded its pid.
fn attach_watcher(server: &Server, pane: &str) {
    server.ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || server
                .pane_opt(pane, "@agent_watch_pid")
                .parse::<u32>()
                .is_ok(),
            SETTLE
        ),
        "the watcher never recorded its pid"
    );
}

/// A pane with a watcher, and the fake agent started in it as `rev` and doing a turn.
fn pane_with_an_agent_at_work(server: &Server, fake: &FakeAgent) -> String {
    let pane = server.pane();
    attach_watcher(server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    fake.wait_until_drawn(server, &pane, SETTLE);
    fake.signal("-USR1");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "working",
            SETTLE
        ),
        "the watcher never read the fake as working: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    pane
}

/// What a wait answered: the status it returned, or the code of the error it ended with.
fn answer(code: i32, envelope: &serde_json::Value) -> String {
    if code == 0 {
        format!(
            "status {}",
            envelope["result"]["agent"]["agent_status"]
                .as_str()
                .unwrap_or("?")
        )
    } else {
        format!(
            "error {}",
            envelope["error"]["code"].as_str().unwrap_or("?")
        )
    }
}

/// The turn ends with the process: the pane leaves `working` within a few seconds, reads `unknown`
/// once it has, as a pane whose agent has left does while its hint stays, and the end of the turn
/// is announced as the done it is, though the pane is no longer `done` when the notice's delay is
/// up. Before, it stayed `working` and nothing was announced.
#[test]
fn an_agent_started_by_agent_start_that_dies_mid_turn_does_not_read_working_for_good() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server(tmp.path());
    let pane = pane_with_an_agent_at_work(&server, &fake);

    fake.signal("-TERM");

    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the pane never left working after the agent died: {:?} (rule {:?})\n{}",
        server.pane_opt(&pane, "@agent_state"),
        server.pane_opt(&pane, "@agent_rule"),
        server.capture(&pane)
    );
    // The hint outlives the agent, so the pane still says who belongs here.
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "claude");
    assert!(
        wait_for(|| recorder.matching("done").len() == 1, SETTLE),
        "the end of the turn was never announced: {:?}",
        recorder.lines()
    );
    assert!(
        recorder.matching("done")[0].starts_with("rev done \u{b7} one:0|"),
        "unexpected title: {:?}",
        recorder.matching("done")[0]
    );
    // What it left with stays on the pane for a wait to read, with the second it left.
    let left = taw::store::Final::parse(&server.pane_opt(&pane, "@agent_final_state"))
        .expect("the exit left no status behind");
    assert_eq!(left.state, taw::store::AgentState::Done);
}

/// An agent that was at rest when its process went is `unknown` at the next read, and leaves with
/// the status it had: a wait that was for it asks for that, and the pane says nothing of the agent.
#[test]
fn an_agent_that_dies_at_rest_leaves_with_the_status_it_had() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server(tmp.path());
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    fake.wait_until_drawn(&server, &pane, SETTLE);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "idle", SETTLE),
        "the watcher never read the fake as idle: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );

    fake.signal("-TERM");

    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the pane never read unknown after the agent died: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    let left = taw::store::Final::parse(&server.pane_opt(&pane, "@agent_final_state"))
        .expect("the exit left no status behind");
    assert_eq!(left.state, taw::store::AgentState::Idle);
    // An agent at rest earned no notification by going.
    assert_eq!(recorder.lines(), Vec::<String>::new());
}

/// A wait for the agent ends when it dies, with the status it left with when that is one the wait
/// was for (herdr's `wait_for_resolved_agent` does as much for an agent released with a final
/// status), and `agent_not_running` when it is not.
#[test]
fn a_wait_for_an_agent_that_dies_mid_turn_ends_with_what_it_left_or_not_running() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    pane_with_an_agent_at_work(&server, &fake);

    let began = Instant::now();
    let ((settled_code, settled), (blocked_code, blocked)) = std::thread::scope(|scope| {
        // The default wait is for the agent to settle: `idle`, `done` or `blocked`.
        let settling = scope.spawn(|| server.taw(&["agent", "wait", "rev", "--timeout", "20000"]));
        let for_a_question = scope.spawn(|| {
            server.taw(&[
                "agent",
                "wait",
                "rev",
                "--state",
                "blocked",
                "--timeout",
                "20000",
            ])
        });
        std::thread::sleep(Duration::from_millis(800));
        fake.signal("-TERM");
        (settling.join().unwrap(), for_a_question.join().unwrap())
    });

    assert_eq!(
        answer(settled_code, &settled),
        "status done",
        "the end of the turn is what the wait was for ({settled})"
    );
    assert_eq!(
        answer(blocked_code, &blocked),
        "error agent_not_running",
        "the agent will not be blocked any more ({blocked})"
    );
    assert!(
        began.elapsed() < Duration::from_secs(19),
        "the waits ran to their timeouts"
    );
}

/// With no watcher nobody takes the record off the pane, and the dead agent's record says what it
/// said. A wait must not run on for an agent whose recorded process is gone.
#[test]
fn a_wait_for_an_agent_whose_process_is_gone_ends_even_with_no_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tmux_named(&bin, "claude");
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        env: private_shells(&tmp.path().join("home")),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    let socket = server.socket().to_string();
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-gone",
    ]);
    assert_eq!(code, 0, "{envelope}");

    let (code, envelope) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "wait",
                "rev",
                "--state",
                "idle",
                "--timeout",
                "20000",
            ])
        });
        std::thread::sleep(Duration::from_millis(800));
        assert!(
            !waiting.is_finished(),
            "the agent is running, and the wait ended"
        );
        server.wake("taw-fake-claude-gone");
        waiting.join().unwrap()
    });

    assert_eq!(
        answer(code, &envelope),
        "error agent_not_running",
        "{envelope}"
    );
}

/// A pane that only its kind hint names, with no pid recorded for what it runs (a start that did not
/// get as far as recording one), is told gone by its own shell: at its prompt with nothing running
/// in it, nothing is running an agent.
#[test]
fn a_wait_for_an_agent_whose_pane_has_its_shell_back_and_no_recorded_pid_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start_with(ServerOptions {
        env: private_shells(&tmp.path().join("home")),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    for (option, value) in [
        ("@agent_kind_hint", "claude"),
        ("@agent_kind", "claude"),
        ("@agent_state", "working"),
    ] {
        server.tmux_ok(&["set-option", "-p", "-t", &pane, option, value]);
    }

    let began = Instant::now();
    let (code, envelope) = server.taw(&[
        "agent",
        "wait",
        &pane,
        "--state",
        "idle",
        "--timeout",
        "20000",
    ]);

    assert_eq!(
        answer(code, &envelope),
        "error agent_not_running",
        "{envelope}"
    );
    assert!(began.elapsed() < Duration::from_secs(15), "the wait ran on");
}

/// An agent the user started by hand has no kind hint and no pid recorded for it, so a wait for it had
/// nothing to tell it gone: with no watcher to take its record off, the wait ran on after the agent
/// had exited, to its `--timeout` or for ever. The pane's own shell back at its prompt, with nothing
/// in front that names the agent, says it has gone, as it does for an agent only a hint names. And an
/// agent that is there is not taken for one that has gone: the wait goes on for longer than the grace
/// a record gets, and ends only when the agent does.
#[test]
fn a_wait_for_a_hand_started_agent_that_exited_ends_with_no_hint_and_no_watcher() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &["claude"]);
    let pane = server.pane();
    server.tmux_ok(&["send-keys", "-t", &pane, "claude", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the agent never took the terminal"
    );
    report_and_name(&server, &pane, "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), "");

    let (code, envelope, ended) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "wait",
                "rev",
                "--state",
                "done",
                "--timeout",
                "30000",
            ])
        });
        // Longer than the grace a record that stands over a process that is gone is given.
        std::thread::sleep(Duration::from_millis(3500));
        assert!(
            !waiting.is_finished(),
            "the agent is running, and the wait ended"
        );
        server.wake("taw-fake-claude-never");
        let woke = Instant::now();
        let (code, envelope) = waiting.join().unwrap();
        (code, envelope, woke.elapsed())
    });

    assert_eq!(
        answer(code, &envelope),
        "error agent_not_running",
        "{envelope}"
    );
    assert!(
        ended < Duration::from_secs(15),
        "the wait took {ended:?} to end after the agent had"
    );
}

/// What the last agent of a pane left with is the last agent's, and `agent start` clears the record of
/// the last agent off the pane to start the next: that clear says nothing of what it left with (the
/// watcher's write at its exit does, with the time it left at), or a wait for the new agent that began
/// in the same second would take it for the new agent's own.
#[test]
fn a_start_leaves_no_final_status_of_the_last_agent_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &["claude"]);
    let pane = server.pane();
    // What an agent that has exited with no watcher leaves: its record, in the state it was in.
    for (option, value) in [("@agent_kind", "claude"), ("@agent_state", "working")] {
        server.tmux_ok(&["set-option", "-p", "-t", &pane, option, value]);
    }
    let socket = server.socket().to_string();

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "next",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-never",
    ]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(
        server.pane_opt(&pane, "@agent_final_state"),
        "",
        "the start recorded what the last agent left with"
    );
    server.wake("taw-fake-claude-never");
}

/// A `tmux` that writes every call it is asked for to a log, a line each, and runs it: what a verb
/// did to a pane is what is in the log.
fn tmux_that_logs(dir: &Path) -> (PathBuf, PathBuf) {
    let (wrapper, log) = (dir.join("tmux-logging"), dir.join("tmux-calls.txt"));
    write_script(
        &wrapper,
        &format!(
            "{{ printf '%s' \"$*\" | tr '\\n\\t\\r' '   '; echo; }} >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            taw::tmux::Tmux::with_socket_name("unused").bin.display()
        ),
    );
    (wrapper, log)
}

/// What a typing verb is asked to type into the pane of a `rev` whose agent has gone: a command
/// that leaves a mark on the screen if a shell is handed it, and keys that do the same.
const SENTINEL: &str = "SENTINEL-TYPED-INTO-THE-SHELL";

/// The two verbs that type, run through the logging `tmux`, at `target`: `agent prompt` and `agent
/// send-keys`.
fn typing_verbs(server: &Server, wrapper: &Path, target: &str) -> [(i32, serde_json::Value); 2] {
    typing_verbs_of(server, wrapper, target, &format!("echo {SENTINEL}"))
}

/// [`typing_verbs`] with `text` as the prompt.
fn typing_verbs_of(
    server: &Server,
    wrapper: &Path,
    target: &str,
    text: &str,
) -> [(i32, serde_json::Value); 2] {
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];
    [
        server.taw_with_env(&["agent", "prompt", target, text], &env),
        server.taw_with_env(
            &["agent", "send-keys", target, "z", "q", "z", "Enter"],
            &env,
        ),
    ]
}

/// What herdr says to a verb that types into the pane of an agent that has gone (`queue_agent_prompt`
/// and `handle_agent_send_keys`, `src/app/api/agents.rs` at v0.9.3): `agent_not_ready`, for a prompt
/// that the agent is no longer the pane's foreground process, and for keys that it is not an active
/// named agent.
fn gone_as_herdr_says(target: &str) -> [(&'static str, String); 2] {
    [
        (
            "agent_not_ready",
            format!("agent {target} is no longer the pane foreground process"),
        ),
        (
            "agent_not_ready",
            format!("agent {target} is not an active named agent"),
        ),
    ]
}

/// A prompt or keys sent to a pane whose agent has gone land at the shell's prompt, and a shell may
/// run them as a command. The verbs refuse, in herdr's words and before anything is typed: each
/// answered with the code and the message `expected` holds for it, no `send-keys` call is in the log of
/// the `tmux` they ran through, and nothing of what they were to type is on the screen of `pane`.
fn assert_nothing_was_typed(
    server: &Server,
    pane: &str,
    log: &Path,
    answers: [(i32, serde_json::Value); 2],
    expected: [(&str, String); 2],
) {
    for ((verb, (code, envelope)), (want_code, want_message)) in ["agent prompt", "agent send-keys"]
        .iter()
        .zip(&answers)
        .zip(&expected)
    {
        assert_eq!(*code, 1, "{verb}: {envelope}");
        assert_eq!(
            envelope["error"]["code"].as_str(),
            Some(*want_code),
            "{verb}: {envelope}"
        );
        assert_eq!(
            envelope["error"]["message"].as_str(),
            Some(want_message.as_str()),
            "{verb}: {envelope}"
        );
    }
    let calls = std::fs::read_to_string(log).unwrap_or_default();
    let typed: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("send-keys"))
        .collect();
    assert!(typed.is_empty(), "the verbs typed into the pane: {typed:?}");
    let screen = server.capture(pane);
    assert!(
        !screen.contains(SENTINEL) && !screen.contains("zqz"),
        "what was typed reached the pane:\n{screen}"
    );
}

/// With no watcher nobody takes the record off the pane of an agent that has exited, and the pane
/// is a shell at its prompt that the verbs would type into.
#[test]
fn nothing_is_typed_into_the_shell_an_agent_left_behind_when_no_watcher_cleared_its_record() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tmux_named(&bin, "claude");
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        env: private_shells(&tmp.path().join("home")),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    let socket = server.socket().to_string();
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-typed",
    ]);
    assert_eq!(code, 0, "{envelope}");
    server.wake("taw-fake-claude-typed");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "claude",
            SETTLE
        ),
        "the agent never ended"
    );
    // The record stands, and the shell is back.
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// The same with a watcher, which has written the end of the turn and says `unknown`: the pane still
/// names the agent by its hint, and the shell is back.
#[test]
fn nothing_is_typed_into_the_shell_an_agent_that_a_watcher_saw_die_left_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let pane = pane_with_an_agent_at_work(&server, &fake);
    fake.signal("-TERM");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the pane never read unknown after the agent died: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// And an agent that is there still gets what it is sent.
#[test]
fn a_live_agent_still_gets_its_prompt_and_its_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let pane = pane_with_an_agent_at_work(&server, &fake);
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let [prompt, keys] = typing_verbs(&server, &wrapper, "rev");

    assert_eq!(prompt.0, 0, "{}", prompt.1);
    assert_eq!(keys.0, 0, "{}", keys.1);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls
            .lines()
            .filter(|call| call.contains("send-keys"))
            .count()
            >= 3,
        "the text, its Enter and the keys were all to be typed:\n{calls}"
    );
    assert!(
        wait_for(|| server.capture(&pane).contains("zqz"), SETTLE),
        "the keys never reached the agent's pane:\n{}",
        server.capture(&pane)
    );
}

/// A target that resolves to no pane (a name nobody has: the agent's record, with its name, was taken
/// off) is `agent_not_found` in herdr, which says so before it sends anything
/// (`agent_target_error_body`, `src/app/agents.rs` at v0.9.3).
#[test]
fn a_target_that_no_longer_resolves_is_agent_not_found_and_nothing_is_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _fake, _recorder) = server(tmp.path());
    let pane = server.pane();
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    let not_found = || ("agent_not_found", "agent target rev not found".to_string());
    assert_nothing_was_typed(&server, &pane, &log, answers, [not_found(), not_found()]);
}

/// A pane that hosts no agent is not an active named agent: `agent_not_ready` in herdr, for a prompt
/// and for keys alike (`queue_agent_prompt` and `handle_agent_send_keys`), and what is typed would
/// reach the shell the pane runs.
#[test]
fn a_pane_that_hosts_no_agent_is_agent_not_ready_and_nothing_is_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _fake, _recorder) = server(tmp.path());
    let pane = server.pane();
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, &pane);

    let not_active = || {
        (
            "agent_not_ready",
            format!("agent {pane} is not an active named agent"),
        )
    };
    assert_nothing_was_typed(&server, &pane, &log, answers, [not_active(), not_active()]);
}

/// A server whose panes find the agents `names` as processes tmux reports under those names (a copy
/// of tmux each, see `common::named_agent`), with no watcher on any pane.
fn server_with_named_agents(tmp: &Path, names: &[&str]) -> Server {
    let mut bin = None;
    for name in names {
        bin = Some(common::named_agent(tmp, name, None));
    }
    let server = Server::start_with(ServerOptions {
        path_prefix: bin,
        env: private_shells(&tmp.join("home")),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// What an agent's integration does when the agent that the user started by hand reports itself, and
/// what the user does to name it: the pane holds a record, and `rev` finds the pane.
fn report_and_name(server: &Server, pane: &str, kind: &str) {
    let (code, envelope) = server.taw(&[
        "pane",
        "report-agent",
        pane,
        "--source",
        "test",
        "--agent",
        kind,
        "--state",
        "idle",
    ]);
    assert_eq!(code, 0, "{envelope}");
    let (code, envelope) = server.taw(&["agent", "rename", pane, "rev"]);
    assert_eq!(code, 0, "{envelope}");
}

/// Waits for the shell of `pane` to have drawn a prompt as the last line of its screen: whatever was
/// running in it has ended, and the shell is back with nothing in front of it.
fn wait_for_the_shell_prompt(server: &Server, pane: &str) {
    assert!(
        wait_for(
            || server
                .capture(pane)
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .is_some_and(|line| line.trim_end().ends_with('$')),
            SETTLE
        ),
        "the shell never drew a prompt:\n{}",
        server.capture(pane)
    );
}

/// A pane in which a hand-started `claude` ran, reported itself, was named `rev` and exited, with no
/// watcher to take its record off: the shell is back at its prompt and the record stands. An agent
/// the user started by hand has no kind hint and no pid recorded for it, so what `agent start` leaves
/// to tell it gone is not there.
fn pane_a_hand_started_agent_left(tmp: &Path) -> (Server, String) {
    let server = server_with_named_agents(tmp, &["claude"]);
    let pane = server.pane();
    server.tmux_ok(&["send-keys", "-t", &pane, "claude", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the agent never took the terminal"
    );
    report_and_name(&server, &pane, "claude");
    server.wake("taw-fake-claude-never");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "claude",
            SETTLE
        ),
        "the agent never ended"
    );
    wait_for_the_shell_prompt(&server, &pane);
    // Nothing but a hand's start: no hint, no pid, and the record stands.
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    (server, pane)
}

/// With no watcher the record of an agent that has exited stands, and the pane is a shell at its
/// prompt that the verbs would type into. The pane's live foreground job has to be the agent, as in
/// herdr (`runtime_hosts_agent`), and a shell at its prompt is not.
#[test]
fn nothing_is_typed_into_the_shell_a_hand_started_agent_left_behind_when_no_watcher_cleared_its_record()
 {
    let tmp = tempfile::tempdir().unwrap();
    let (server, pane) = pane_a_hand_started_agent_left(tmp.path());
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// The process in front of `pane`'s shell is another than the shell: the terminal's foreground
/// process group is not the shell's own.
fn a_job_is_in_front(server: &Server, pane: &str) -> bool {
    let shell = server.display(pane, "#{pane_pid}");
    let tpgid = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &shell])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    !tpgid.is_empty() && tpgid != shell
}

/// A hand-started agent has gone, the record stands, and the user has started a shell of their own
/// in the pane (`sh`, `ssh`, `nix develop`, `sudo -s`): not the agent, and a shell runs what it is
/// typed. A prompt whose text is a command is refused, with nothing typed, and the command never runs.
#[test]
fn nothing_runs_in_a_nested_shell_the_user_started_after_a_hand_started_agent_left() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, pane) = pane_a_hand_started_agent_left(tmp.path());
    server.tmux_ok(&["send-keys", "-t", &pane, "sh", "Enter"]);
    assert!(
        wait_for(|| a_job_is_in_front(&server, &pane), SETTLE),
        "the nested shell never took the terminal"
    );
    let ran = tmp.path().join("ran-in-the-nested-shell");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs_of(
        &server,
        &wrapper,
        "rev",
        &format!("touch '{}' ; echo {SENTINEL}", ran.display()),
    );

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
    assert!(
        !wait_for(|| ran.exists(), Duration::from_secs(2)),
        "the nested shell ran what the prompt said"
    );
}

/// The same for a program that reads what it is typed, `cat`: the text would reach it.
#[test]
fn nothing_reaches_a_program_the_user_started_after_a_hand_started_agent_left() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, pane) = pane_a_hand_started_agent_left(tmp.path());
    server.tmux_ok(&["send-keys", "-t", &pane, "cat", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "cat",
            SETTLE
        ),
        "cat never took the terminal"
    );
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// And a pager, whose `!` runs the rest of a line as a command: a prompt of `!touch <file>` is a
/// command to it.
#[test]
fn nothing_runs_through_the_shell_escape_of_a_pager_the_user_opened_after_a_hand_started_agent_left()
 {
    let tmp = tempfile::tempdir().unwrap();
    let (server, pane) = pane_a_hand_started_agent_left(tmp.path());
    let notes = tmp.path().join("notes.txt");
    std::fs::write(&notes, "notes\n".repeat(200)).unwrap();
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("LESSHISTFILE=- less '{}'", notes.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "less",
            SETTLE
        ),
        "the pager never took the terminal"
    );
    assert!(
        wait_for(|| server.capture(&pane).contains("notes\nnotes"), SETTLE),
        "the pager never drew:\n{}",
        server.capture(&pane)
    );
    let ran = tmp.path().join("ran-from-the-pager");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs_of(
        &server,
        &wrapper,
        "rev",
        &format!("!touch '{}'", ran.display()),
    );

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
    assert!(
        !wait_for(|| ran.exists(), Duration::from_secs(2)),
        "the pager ran what the prompt said"
    );
}

/// The same for an agent no process name gives away (an npm agent under `node`, a wrapper), which
/// only its own report and the pane's shell say anything of.
#[test]
fn nothing_is_typed_into_the_shell_a_hand_started_agent_no_process_names_left_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &[]);
    let pane = server.pane();
    let exit = tmp.path().join("exit");
    let script = tmp.path().join("agent.sh");
    write_script(
        &script,
        &format!(
            "'{}' pane report-agent \"$TMUX_PANE\" --source test --agent claude --state idle >/dev/null 2>&1\n\
             while [ ! -e '{}' ]; do sleep 0.1; done\n",
            common::taw_bin(),
            exit.display()
        ),
    );
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("sh '{}'", script.display()),
        "Enter",
    ]);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind") == "claude", SETTLE),
        "the agent never reported:\n{}",
        server.capture(&pane)
    );
    let (code, envelope) = server.taw(&["agent", "rename", &pane, "rev"]);
    assert_eq!(code, 0, "{envelope}");
    std::fs::write(&exit, "").unwrap();
    wait_for_the_shell_prompt(&server, &pane);
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// A `tmux` that ends the agent in `pane` just before it passes on the first call that types: the
/// agent exits after the verb has checked that it is there and before the verb sends, and the wrapper
/// waits for the shell to have the terminal back before the call goes through. It leaves `ended` behind
/// when it has done so, so a test cannot pass for a wrapper that never ran.
fn tmux_that_ends_the_agent_before_it_types(
    dir: &Path,
    server: &Server,
    pane: &str,
    channel: &str,
) -> (PathBuf, PathBuf) {
    let (wrapper, ended) = (dir.join("tmux-ending-the-agent"), dir.join("ended"));
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *send-keys*)\n    '{real}' -L '{socket}' wait-for -S '{channel}'\n    i=0\n    \
             while [ \"$('{real}' -L '{socket}' display-message -p -t '{pane}' '#{{pane_current_command}}')\" = claude ] \
             && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n    : > '{ended}'\n    ;;\nesac\nexec '{real}' \"$@\"\n",
            real = real.display(),
            socket = server.socket(),
            ended = ended.display(),
        ),
    );
    (wrapper, ended)
}

/// The agent exits between the verb's check and its send: the check passed, the shell is back by the time
/// tmux is asked to type, and what is typed would land at its prompt. The send is guarded in the tmux
/// invocation that makes it, on the command in front of the pane being the one the check saw, so tmux
/// types nothing, and the verb answers as it does for an agent that had gone before it looked.
#[test]
fn an_agent_that_exits_between_the_check_and_the_send_is_typed_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, pane) = {
        let server = server_with_named_agents(tmp.path(), &["claude"]);
        let pane = server.pane();
        server.tmux_ok(&["send-keys", "-t", &pane, "claude", "Enter"]);
        assert!(
            wait_for(
                || server.display(&pane, "#{pane_current_command}") == "claude",
                SETTLE
            ),
            "the agent never took the terminal"
        );
        report_and_name(&server, &pane, "claude");
        (server, pane)
    };
    let (wrapper, ended) = tmux_that_ends_the_agent_before_it_types(
        tmp.path(),
        &server,
        &pane,
        "taw-fake-claude-never",
    );
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];

    let (code, envelope) = server.taw_with_env(
        &["agent", "prompt", "rev", &format!("echo {SENTINEL}")],
        &env,
    );

    assert!(ended.exists(), "the agent was never ended before the send");
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        envelope["error"]["code"].as_str(),
        Some("agent_not_ready"),
        "{envelope}"
    );
    assert_eq!(
        envelope["error"]["message"].as_str(),
        Some("agent rev is no longer the pane foreground process"),
        "{envelope}"
    );
    wait_for_the_shell_prompt(&server, &pane);
    // Nothing was typed: the screen does not show the text, and nothing ran.
    assert!(
        !wait_for(
            || server.capture(&pane).contains(SENTINEL),
            Duration::from_secs(2)
        ),
        "what the prompt said reached the shell:\n{}",
        server.capture(&pane)
    );
}

/// The same for the keys of `agent send-keys`.
#[test]
fn an_agent_that_exits_between_the_check_and_the_keys_is_sent_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &["claude"]);
    let pane = server.pane();
    server.tmux_ok(&["send-keys", "-t", &pane, "claude", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the agent never took the terminal"
    );
    report_and_name(&server, &pane, "claude");
    let (wrapper, ended) = tmux_that_ends_the_agent_before_it_types(
        tmp.path(),
        &server,
        &pane,
        "taw-fake-claude-never",
    );
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];

    let (code, envelope) =
        server.taw_with_env(&["agent", "send-keys", "rev", "z", "q", "z", "Enter"], &env);

    assert!(ended.exists(), "the agent was never ended before the send");
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        envelope["error"]["code"].as_str(),
        Some("agent_not_ready"),
        "{envelope}"
    );
    assert_eq!(
        envelope["error"]["message"].as_str(),
        Some("agent rev is not an active named agent"),
        "{envelope}"
    );
    wait_for_the_shell_prompt(&server, &pane);
    assert!(
        !wait_for(
            || server.capture(&pane).contains("zqz"),
            Duration::from_secs(2)
        ),
        "the keys reached the shell:\n{}",
        server.capture(&pane)
    );
}

/// An agent in front that is another than the one the pane records is not it: herdr compares the
/// agent the live job identifies as with the one the pane hosts, and a codex that the user started
/// where a claude was is no claude.
#[test]
fn nothing_is_typed_into_a_pane_whose_foreground_is_another_agent_than_the_one_it_records() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &["codex"]);
    let pane = server.pane();
    server.tmux_ok(&["send-keys", "-t", &pane, "codex", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "codex",
            SETTLE
        ),
        "the agent never took the terminal"
    );
    report_and_name(&server, &pane, "claude");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// The files of a hand-started agent that runs under `node`, as an npm package installs one
/// (`node <dir>/bin/claude`): the line that starts it, and the file where it keeps every line it is typed.
/// It reports itself as a claude. `node` is a shell under that name, so that what runs has the arguments
/// of an npm agent, and the script is the agent.
fn node_agent(tmp: &Path) -> (String, PathBuf) {
    let node = tmp.join("node-bin").join("node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/bin/sh", &node).unwrap();
    let typed = tmp.join("typed");
    let script = tmp.join("package").join("bin").join("claude");
    write_script(
        &script,
        &format!(
            "'{}' pane report-agent \"$TMUX_PANE\" --source test --agent claude --state idle >/dev/null 2>&1\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done\n",
            common::taw_bin(),
            typed.display()
        ),
    );
    (
        format!("'{}' '{}'", node.display(), script.display()),
        typed,
    )
}

/// A pane holding a hand-started agent that runs under `node` ([`node_agent`]), reported and named `rev`.
fn pane_with_an_agent_under_node(tmp: &Path) -> (Server, String, PathBuf) {
    let server = server_with_named_agents(tmp, &[]);
    let pane = server.pane();
    let (command, typed) = node_agent(tmp);
    server.tmux_ok(&["send-keys", "-t", &pane, &command, "Enter"]);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind") == "claude", SETTLE),
        "the agent never reported:\n{}",
        server.capture(&pane)
    );
    let (code, envelope) = server.taw(&["agent", "rename", &pane, "rev"]);
    assert_eq!(code, 0, "{envelope}");
    (server, pane, typed)
}

/// An agent that runs under `node` is in front of the pane as `node`, and only its arguments give it
/// away (`node /path/to/bin/claude`, as an npm package installs one): herdr reads the argv of the
/// foreground job, so such an agent still gets what it is sent.
#[test]
fn a_live_hand_started_agent_under_node_still_gets_its_prompt_and_its_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _pane, typed) = pane_with_an_agent_under_node(tmp.path());
    let (wrapper, _log) = tmux_that_logs(tmp.path());

    let [prompt, keys] = typing_verbs_of(&server, &wrapper, "rev", "hello under node");

    assert_eq!(prompt.0, 0, "{}", prompt.1);
    assert_eq!(keys.0, 0, "{}", keys.1);
    assert!(
        wait_for(
            || std::fs::read_to_string(&typed)
                .is_ok_and(|got| got.contains("hello under node") && got.contains("zqz")),
            SETTLE
        ),
        "what was sent never reached the agent: {:?}",
        std::fs::read_to_string(&typed)
    );
}

/// The send is guarded in the tmux invocation that makes it, with the text written as a word of a tmux
/// command line, and what a prompt says reaches the agent as it was said: quotes, a backslash, `$` and
/// `~` (which tmux's parser reads for a meaning), a `#` and a `;`, a tab, and lines of which one is
/// indented and one starts with a `#`.
#[test]
fn what_a_prompt_says_reaches_the_agent_as_it_was_said() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _pane, typed) = pane_with_an_agent_under_node(tmp.path());
    let (wrapper, _log) = tmux_that_logs(tmp.path());
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];
    let lines = [
        r#"say "hi" and 'bye' \ \ 
 A $HOME ${USER} ~ ~/x"#,
        "# not a comment ; nor is this; {braces} #{pane_id} 100%",
        "    indented four spaces\tand a tab",
        "caf\u{e9} \u{65e5}\u{672c} \u{1f600}",
    ]
    .join("\n");

    let (code, envelope) = server.taw_with_env(&["agent", "prompt", "rev", &lines], &env);

    assert_eq!(code, 0, "{envelope}");
    let expected = format!("{lines}\n");
    assert!(
        wait_for(
            || std::fs::read_to_string(&typed).is_ok_and(|got| got == expected),
            SETTLE
        ),
        "what the agent was typed is not what was said:\n{:?}\nwanted\n{expected:?}",
        std::fs::read_to_string(&typed)
    );
}

/// A text longer than a tmux command line can be (about 16 KiB) is sent in pieces, each guarded, and
/// arrives whole and in order.
#[test]
fn a_prompt_longer_than_a_tmux_command_line_is_sent_in_pieces_and_arrives_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _pane, typed) = pane_with_an_agent_under_node(tmp.path());
    let (wrapper, log) = tmux_that_logs(tmp.path());
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];
    let lines: Vec<String> = (0..400)
        .map(|n| format!("line {n:04} \"quoted\" $x {}", "w".repeat(30)))
        .collect();
    let text = lines.join("\n");
    assert!(text.len() > 16 * 1024, "{}", text.len());

    let (code, envelope) = server.taw_with_env(&["agent", "prompt", "rev", &text], &env);

    assert_eq!(code, 0, "{envelope}");
    let expected = format!("{text}\n");
    assert!(
        wait_for(
            || std::fs::read_to_string(&typed).is_ok_and(|got| got == expected),
            SETTLE
        ),
        "the text did not arrive whole: {} of {} bytes",
        std::fs::read_to_string(&typed).map_or(0, |got| got.len()),
        expected.len()
    );
    let sends = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|call| call.contains("send-keys -l"))
        .count();
    assert!(sends >= 5, "{sends} sends for {} bytes", text.len());
}

/// The keys of `agent send-keys` are guarded the same way, and a key that tmux's parser reads for a
/// meaning (`;`, `$`, `~`, `"`, `\`) is the key it names.
#[test]
fn the_keys_an_agent_is_sent_reach_it_as_named() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _pane, typed) = pane_with_an_agent_under_node(tmp.path());
    let (wrapper, _log) = tmux_that_logs(tmp.path());
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];

    let (code, envelope) = server.taw_with_env(
        &[
            "agent",
            "send-keys",
            "rev",
            ";",
            "$",
            "~",
            "\"",
            "\\",
            "#",
            "{",
            "}",
            "a",
            "Enter",
        ],
        &env,
    );

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || std::fs::read_to_string(&typed).is_ok_and(|got| got == ";$~\"\\#{}a\n"),
            SETTLE
        ),
        "the keys did not arrive as named: {:?}",
        std::fs::read_to_string(&typed)
    );
}

/// And a hand-started agent that is there still gets what it is sent, whether the pane's command names
/// it or only the arguments of the process do (a script under the shell, named for the agent).
#[test]
fn a_live_hand_started_agent_still_gets_its_prompt_and_its_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &["claude"]);
    let named = server.pane();
    server.tmux_ok(&["send-keys", "-t", &named, "claude", "Enter"]);
    assert!(
        wait_for(
            || server.display(&named, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the agent never took the terminal"
    );
    report_and_name(&server, &named, "claude");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let [prompt, keys] = typing_verbs(&server, &wrapper, "rev");

    assert_eq!(prompt.0, 0, "{}", prompt.1);
    assert_eq!(keys.0, 0, "{}", keys.1);
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls
            .lines()
            .filter(|call| call.contains("send-keys"))
            .count()
            >= 3,
        "the text, its Enter and the keys were all to be typed:\n{calls}"
    );

    // The same for an agent that is a script under the shell, which the pane's command (`sh`) does not
    // name and its arguments (`sh <dir>/codex`) do.
    let unnamed = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
    let script = tmp.path().join("scripts").join("codex");
    write_script(
        &script,
        &format!(
            "'{}' pane report-agent \"$TMUX_PANE\" --source test --agent codex --state idle >/dev/null 2>&1\n\
             while :; do sleep 0.1; done\n",
            common::taw_bin()
        ),
    );
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &unnamed,
        &format!("sh '{}'", script.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || server.pane_opt(&unnamed, "@agent_kind") == "codex",
            SETTLE
        ),
        "the agent never reported:\n{}",
        server.capture(&unnamed)
    );
    let (code, envelope) = server.taw(&["agent", "rename", &unnamed, "other"]);
    assert_eq!(code, 0, "{envelope}");
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];
    let (code, envelope) = server.taw_with_env(&["agent", "prompt", "other", "hello"], &env);
    assert_eq!(code, 0, "{envelope}");
    let (code, envelope) =
        server.taw_with_env(&["agent", "send-keys", "other", "z", "q", "z"], &env);
    assert_eq!(code, 0, "{envelope}");
}

/// The pid of what the shell of `pane` started last: its newest child, as `agent start` records it.
fn newest_child(server: &Server, pane: &str) -> String {
    let shell = server.display(pane, "#{pane_pid}");
    let out = std::process::Command::new("pgrep")
        .args(["-P", &shell])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string()
}

/// An agent called `name` that a pane's shell can run, and that no process name gives away: a script that
/// becomes a copy of tmux called `runtime`, which waits on the channel [`Server::wake`] signals for
/// `taw-fake-<name>-never`. What is in front of the pane is `runtime`, and nothing in its arguments names
/// the agent. The returned directory goes first on the pane shell's `$PATH`.
fn unnamed_agent(dir: &Path, name: &str, runtime: &str) -> PathBuf {
    let bin = dir.join("bin");
    let copy_dir = dir.join("copy");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let copy = tmux_named(&copy_dir, runtime);
    write_script(
        &bin.join(name),
        &format!(
            "exec '{}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-{name}-never\n",
            copy.display()
        ),
    );
    bin
}

/// What the tests that restore a session run on: nothing is announced, and nothing is fetched.
const QUIET: &str = "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n";

/// A server of its own whose first session held a claude named `rev` when it was saved, and was
/// restored: `taw session restore` made the session again and typed `claude --resume abc` into its pane,
/// in a shell whose `$PATH` starts with `bin`, where the `claude` that runs is. The pane it made, with
/// the agent resumed in it, which holds what restore marks it with (the hint, the kind, the name).
fn pane_a_restore_resumed_a_claude_in(tmp: &Path, bin: PathBuf) -> (Server, String) {
    pane_a_restore_resumed_a_claude_in_shells_with(tmp, bin, &[], &[])
}

/// [`pane_a_restore_resumed_a_claude_in`], with `env` in the environment of the server and its shells
/// besides, and `restore_env` in the environment of the restore.
fn pane_a_restore_resumed_a_claude_in_shells_with(
    tmp: &Path,
    bin: PathBuf,
    env: &[(&str, &str)],
    restore_env: &[(&str, &str)],
) -> (Server, String) {
    let mut server_env = private_shells(&tmp.join("home"));
    server_env.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let server = Server::start_with(ServerOptions {
        session: "alpha".to_string(),
        width: 200,
        height: 50,
        path_prefix: Some(bin),
        env: server_env,
        ..ServerOptions::default()
    });
    server.write_config(QUIET);
    server.tmux_ok(&["new-session", "-d", "-s", "beta", "-x", "200", "-y", "50"]);
    let pane = server.pane_of("alpha");
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_session_id", "abc"),
        ("@agent_name", "rev"),
    ] {
        server.set_pane_opt(&pane, option, value);
    }
    let (code, envelope) = server.taw(&["session", "save", "--name", "resume"]);
    assert_eq!(code, 0, "{envelope}");
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    let (code, envelope) =
        server.taw_with_env(&["session", "restore", "--name", "resume"], restore_env);
    assert_eq!(code, 0, "{envelope}");
    let pane = server.pane_of("alpha");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "rev");
    (server, pane)
}

/// The user starts a shell of their own in the pane after the agent has gone (`sh`, `ssh`, `nix develop`),
/// and `agent prompt` and `agent send-keys` are asked to type: a shell runs what it is typed, so both
/// are refused with nothing typed, and the command a prompt carries never runs.
fn assert_a_nested_shell_runs_nothing(server: &Server, pane: &str, tmp: &Path) {
    server.tmux_ok(&["send-keys", "-t", pane, "sh", "Enter"]);
    assert!(
        wait_for(|| a_job_is_in_front(server, pane), SETTLE),
        "the nested shell never took the terminal"
    );
    let ran = tmp.join("ran-in-the-nested-shell");
    let (wrapper, log) = tmux_that_logs(&tmp.join("after-the-agent"));

    let answers = typing_verbs_of(
        server,
        &wrapper,
        "rev",
        &format!("touch '{}' ; echo {SENTINEL}", ran.display()),
    );

    assert_nothing_was_typed(server, pane, &log, answers, gone_as_herdr_says("rev"));
    assert!(
        !wait_for(|| ran.exists(), Duration::from_secs(2)),
        "the nested shell ran what the prompt said"
    );
}

/// A pane a session restore resumed an agent in has the hint, the kind and the name, and `agent start`'s
/// record of the process it started was not made: with no pid, the agent that exits left a pane no
/// process names, and what the verbs that type took for the agent was the user's own shell, or anything
/// the user started in it, so that `touch <file>` ran in a nested `sh`. The restore records the pid of
/// what it started, and with none recorded the hint does not stand in for the agent.
#[test]
fn nothing_runs_in_a_nested_shell_after_a_restored_agent_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = common::named_agent(tmp.path(), "claude", None);
    let (server, pane) = pane_a_restore_resumed_a_claude_in(tmp.path(), bin);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the resumed agent never took the terminal"
    );
    let agent = newest_child(&server, &pane);
    server.wake("taw-fake-claude-never");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "claude",
            SETTLE
        ),
        "the agent never ended"
    );
    wait_for_the_shell_prompt(&server, &pane);

    assert_a_nested_shell_runs_nothing(&server, &pane, tmp.path());

    // The restore recorded what it had started, as `agent start` does.
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), agent);
}

/// The same with a watcher on the pane.
#[test]
fn nothing_runs_in_a_nested_shell_after_a_restored_agent_a_watcher_saw_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = common::named_agent(tmp.path(), "claude", None);
    let (server, pane) = pane_a_restore_resumed_a_claude_in(tmp.path(), bin);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the resumed agent never took the terminal"
    );
    attach_watcher(&server, &pane);
    server.wake("taw-fake-claude-never");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "claude",
            SETTLE
        ),
        "the agent never ended"
    );
    wait_for_the_shell_prompt(&server, &pane);
    // The watcher has read the exit: the hint keeps the pane's kind, and the agent's state is gone.
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never read the exit: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );

    assert_a_nested_shell_runs_nothing(&server, &pane, tmp.path());
}

/// A restored agent that no process name gives away (a wrapper, an npm install) is in front of its pane
/// as another program, and what says it is there is the pid the restore recorded: with the pid, it gets
/// what it is sent while it runs, and nothing once it has exited and the user has started a shell.
#[test]
fn a_restored_agent_no_process_names_gets_its_prompt_while_it_runs_and_nothing_once_it_has_exited()
{
    let tmp = tempfile::tempdir().unwrap();
    let bin = unnamed_agent(tmp.path(), "claude", "agent-runtime");
    let (server, pane) = pane_a_restore_resumed_a_claude_in(tmp.path(), bin);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the resumed agent never took the terminal"
    );
    let (wrapper, _log) = tmux_that_logs(tmp.path());
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];

    let (code, envelope) = server.taw_with_env(&["agent", "prompt", "rev", "hello restored"], &env);

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(|| server.capture(&pane).contains("hello restored"), SETTLE),
        "the prompt never reached the agent's pane:\n{}",
        server.capture(&pane)
    );
    server.wake("taw-fake-claude-never");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "agent-runtime",
            SETTLE
        ),
        "the agent never ended"
    );
    wait_for_the_shell_prompt(&server, &pane);

    assert_a_nested_shell_runs_nothing(&server, &pane, tmp.path());
}

/// A restore in a pane whose shell runs a helper before every command, as the preexec hooks of
/// starship, atuin and bash-preexec do (here a DEBUG trap whose helper writes its pid and becomes
/// `/bin/sleep 0.6`), resuming an agent no process name gives away. The shell's first child is the
/// helper, which has gone by the time the agent runs; the pid the restore records is the agent's, so
/// that what tells the agent's exit is the agent and not a helper that had ended before it started.
#[test]
fn a_restore_records_the_agent_and_not_the_helper_its_shell_ran_before_it() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = unnamed_agent(tmp.path(), "claude", "agent-runtime");
    let helper_pid = tmp.path().join("helper.pid");
    let rc = tmp.path().join("rc.sh");
    std::fs::write(
        &rc,
        format!(
            "trap 'sh -c \"echo \\$\\$ > {}; exec /bin/sleep 0.6\"' DEBUG\n",
            helper_pid.display()
        ),
    )
    .unwrap();
    let (server, pane) = pane_a_restore_resumed_a_claude_in_shells_with(
        tmp.path(),
        bin,
        &[("ENV", rc.to_str().unwrap())],
        &[],
    );
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the resumed agent never took the terminal:\n{}",
        server.capture(&pane)
    );
    let agent = newest_child(&server, &pane);
    let helper = common::read_pid(&helper_pid).expect("the shell ran no helper before the agent");
    assert_ne!(helper, agent);

    let recorded = server.pane_opt(&pane, "@agent_pid");

    assert_eq!(
        recorded, agent,
        "the restore recorded the pid of the helper ({helper}) and not the agent's"
    );
}

/// A `pgrep` first on a restore's `$PATH` that notes in `looks` each look for the process a resume command
/// started (`pgrep -P`, which only those looks run) and answers it as the real one does, and runs every
/// other `pgrep` as it is. With `gate`, it makes the looks as slow as a busy machine does, two of them
/// longer than the restore's whole bound: the first answers what is in front then (the helper the shell
/// runs before the command, which waits for `gate`) and takes 2.2 s more; the second lets the helper go
/// (`gate`) and answers once the agent, `agent-runtime`, is in front; the rest answer at once. So the two
/// looks a restore made disagreed, whatever the machine is doing. The returned directory goes first on the
/// restore's `$PATH`.
fn pgrep_that_notes_its_looks(dir: &Path, looks: &Path, gate: Option<&Path>) -> PathBuf {
    let bin = dir.join("pgrep-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let slow = gate.map_or_else(String::new, |gate| {
        format!(
            "if [ \"$n\" -eq 1 ]; then\n\
             \x20 /usr/bin/pgrep \"$@\"; s=$?\n\
             \x20 /bin/sleep 2.2\n\
             \x20 exit \"$s\"\n\
             fi\n\
             if [ \"$n\" -eq 2 ]; then\n\
             \x20 : > '{gate}'\n\
             \x20 i=0\n\
             \x20 while [ \"$i\" -lt 200 ]; do\n\
             \x20   for child in $(/usr/bin/pgrep -P \"$2\"); do\n\
             \x20     case \"$(/bin/ps -o comm= -p \"$child\")\" in *agent-runtime) break 2 ;; esac\n\
             \x20   done\n\
             \x20   /bin/sleep 0.05; i=$((i + 1))\n\
             \x20 done\n\
             fi\n",
            gate = gate.display()
        )
    });
    write_script(
        &bin.join("pgrep"),
        &format!(
            "[ \"$1\" = -P ] || exec /usr/bin/pgrep \"$@\"\n\
             echo look >> '{looks}'\n\
             n=$(($(wc -l < '{looks}')))\n\
             {slow}\
             exec /usr/bin/pgrep \"$@\"\n",
            looks = looks.display()
        ),
    );
    bin
}

/// How many looks [`pgrep_that_notes_its_looks`] noted.
fn looks_made(looks: &Path) -> usize {
    std::fs::read_to_string(looks)
        .unwrap_or_default()
        .lines()
        .count()
}

/// A restore on a machine so busy that a look for the process a resume command started takes longer than
/// the restore's whole bound: it ended the looks at its bound after the two it always makes, the first of
/// which caught the helper the shell runs before the command (here one that waits to be let go) and the
/// second the agent, which disagree, and it recorded nothing. It makes three looks at the least, and the
/// last two find the agent. The test's `pgrep` makes the looks that slow ([`pgrep_that_notes_its_looks`]).
#[test]
fn a_restore_whose_looks_outlast_its_bound_records_the_agent_and_not_the_helper() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = unnamed_agent(tmp.path(), "claude", "agent-runtime");
    let (helper_pid, gate) = (tmp.path().join("helper.pid"), tmp.path().join("gate"));
    let rc = tmp.path().join("rc.sh");
    std::fs::write(
        &rc,
        format!(
            "trap 'case $BASH_COMMAND in claude*) sh -c \"echo \\$\\$ > {}; i=0; while [ ! -e {} ] && \
             [ \\$i -lt 600 ]; do sleep 0.05; i=\\$((i + 1)); done\";; esac' DEBUG\n",
            helper_pid.display(),
            gate.display()
        ),
    )
    .unwrap();
    let looks = tmp.path().join("looks");
    let pgrep = pgrep_that_notes_its_looks(tmp.path(), &looks, Some(&gate));
    let path = format!(
        "{}:{}",
        pgrep.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (server, pane) = pane_a_restore_resumed_a_claude_in_shells_with(
        tmp.path(),
        bin,
        &[("ENV", rc.to_str().unwrap())],
        &[("PATH", &path)],
    );
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the resumed agent never took the terminal:\n{}",
        server.capture(&pane)
    );
    let agent = newest_child(&server, &pane);
    let helper = common::read_pid(&helper_pid).expect("the shell ran no helper before the agent");
    assert_ne!(helper, agent);

    let recorded = server.pane_opt(&pane, "@agent_pid");

    assert_eq!(
        recorded,
        agent,
        "the restore recorded no pid, or the helper's ({helper}), and not the agent's, in {} looks",
        looks_made(&looks)
    );
}

/// The floor of three looks is for slow ones: a restore whose looks are quick ends them when its bound
/// runs out, after as many as its two seconds leave room for at a look every 50 ms at the most, and the
/// floor adds none. The test's `pgrep` counts them ([`pgrep_that_notes_its_looks`]), and the agent is the
/// one recorded, as at any restore.
#[test]
fn a_restore_whose_looks_are_quick_makes_no_more_than_its_bound_leaves_room_for() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = unnamed_agent(tmp.path(), "claude", "agent-runtime");
    let looks = tmp.path().join("looks");
    let pgrep = pgrep_that_notes_its_looks(tmp.path(), &looks, None);
    let path = format!(
        "{}:{}",
        pgrep.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (server, pane) =
        pane_a_restore_resumed_a_claude_in_shells_with(tmp.path(), bin, &[], &[("PATH", &path)]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the resumed agent never took the terminal:\n{}",
        server.capture(&pane)
    );

    let made = looks_made(&looks);

    assert!(
        (3..=2000 / 50 + 2).contains(&made),
        "the restore made {made} looks"
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        newest_child(&server, &pane)
    );
}

/// A pane that a hint, a kind and a name say holds an agent, with no pid recorded for it (what a restore
/// left before it recorded one, or an agent whose pid `pgrep` could not find): the hint stands for
/// nothing, so a shell the user started in the pane is not taken for the agent, and what a prompt carries
/// never runs in it.
#[test]
fn nothing_runs_in_a_nested_shell_in_a_pane_only_a_hint_and_a_name_say_holds_an_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &[]);
    let pane = server.pane();
    for (option, value) in [
        ("@agent_kind_hint", "claude"),
        ("@agent_kind", "claude"),
        ("@agent_name", "rev"),
    ] {
        server.set_pane_opt(&pane, option, value);
    }
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), "");

    assert_a_nested_shell_runs_nothing(&server, &pane, tmp.path());
}

/// An agent `taw agent start` started, suspended with C-z (its process lives, the shell has the terminal
/// back), and a shell the user starts in front of it: what the record holds is a pid that is alive, and
/// that says nothing of what is in front. A recorded pid counts only while its process is in the
/// foreground of the terminal, so the nested shell is refused what it would run.
#[test]
fn nothing_runs_in_a_nested_shell_in_front_of_a_suspended_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    let agent = fake.wait_until_drawn(&server, &pane, SETTLE);
    server.tmux_ok(&["send-keys", "-t", &pane, "C-z"]);
    assert!(
        wait_for(|| process_is_stopped(&agent), SETTLE),
        "the agent was never suspended:\n{}",
        server.capture(&pane)
    );
    assert!(
        wait_for(|| !a_job_is_in_front(&server, &pane), SETTLE),
        "the shell never took the terminal back"
    );

    assert_a_nested_shell_runs_nothing(&server, &pane, tmp.path());

    // It is the agent's process that stands in the record, and it lives.
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), agent);
    assert!(
        process_is_stopped(&agent),
        "the agent was not left suspended"
    );
}

/// Whether the process `pid` is stopped (its state is `T`).
fn process_is_stopped(pid: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim_start()
                .starts_with('T')
        })
}

/// A plain C-z: the agent is suspended and its shell is back at its prompt with nothing in front. The
/// pid lives, and the shell is at its prompt, which hosts no agent.
#[test]
fn nothing_is_typed_into_the_shell_a_suspended_agent_left_in_front() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    let agent = fake.wait_until_drawn(&server, &pane, SETTLE);
    server.tmux_ok(&["send-keys", "-t", &pane, "C-z"]);
    assert!(
        wait_for(|| process_is_stopped(&agent), SETTLE),
        "the agent was never suspended"
    );
    wait_for_the_shell_prompt(&server, &pane);
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs(&server, &wrapper, "rev");

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
}

/// A pane `taw agent start` started a claude in, which has exited and been read as exited by the watcher
/// (the pane keeps the hint, and the pid `agent start` recorded, which is dead), and in which the user
/// then starts an agent by hand under `node` ([`node_agent`]), which reports itself. The file the hand-
/// started agent keeps what it is typed in, and the pane.
fn pane_a_started_agent_left_and_a_hand_started_one_reported(
    tmp: &Path,
) -> (Server, FakeAgent, String, PathBuf) {
    let (server, fake, _recorder) = server(tmp);
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    let started = fake.wait_until_drawn(&server, &pane, SETTLE);
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), started);
    fake.signal("-TERM");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never read the exit: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    // What stays of it: the hint, and a pid that is dead.
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), started);
    let (command, typed) = node_agent(tmp);
    server.tmux_ok(&["send-keys", "-t", &pane, &command, "Enter"]);
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_source") == "hook:test",
            SETTLE
        ),
        "the hand-started agent never reported:\n{}",
        server.capture(&pane)
    );
    (server, fake, pane, typed)
}

/// The pid `agent start` recorded for the agent that left is dead, and stays with the hint. A report from
/// an agent the user started by hand afterwards is not that pid's agent: it takes the pid off, so that
/// the pane's hint and a pid that is dead do not say the live agent has gone (a wait for it ended with
/// `agent_not_running` two seconds in).
#[test]
fn a_wait_for_a_hand_started_agent_in_a_pane_a_started_agent_left_does_not_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _fake, pane, _typed) =
        pane_a_started_agent_left_and_a_hand_started_one_reported(tmp.path());

    let began = Instant::now();
    let (code, envelope) = server.taw(&[
        "agent",
        "wait",
        &pane,
        "--state",
        "blocked",
        "--timeout",
        "4000",
    ]);

    assert_eq!(answer(code, &envelope), "error timeout", "{envelope}");
    assert!(
        began.elapsed() >= Duration::from_millis(3500),
        "the wait ended after {:?}",
        began.elapsed()
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        "",
        "the report left the dead pid on the pane"
    );
}

/// And the agent the user started gets what it is sent: with the dead pid off the pane, what is in front
/// is told by its arguments, as it is for any agent started by hand.
#[test]
fn a_hand_started_agent_in_a_pane_a_started_agent_left_gets_its_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _fake, pane, typed) =
        pane_a_started_agent_left_and_a_hand_started_one_reported(tmp.path());

    let (code, envelope) = server.taw(&["agent", "prompt", &pane, "typed after the hand start"]);

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || std::fs::read_to_string(&typed)
                .is_ok_and(|got| got.contains("typed after the hand start")),
            SETTLE
        ),
        "what was sent never reached the agent: {:?}",
        std::fs::read_to_string(&typed)
    );
}

/// Runs `taw <args>` from the job in front of `pane`, as an agent's hook runs it: the pane's shell is
/// typed a `sh -c` that runs taw, with `env` before it, and notes in `dir` when taw has ended. taw's
/// parent is that `sh`, which leads the job in front of the pane's terminal. Waits for the note and for
/// the shell's prompt, and returns what taw printed.
fn report_from_the_job_in_front(
    server: &Server,
    pane: &str,
    args: &[&str],
    env: &[(&str, &str)],
    dir: &Path,
) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let (printed, done) = (dir.join("printed"), dir.join("done"));
    let env: String = env.iter().map(|(k, v)| format!("{k}='{v}' ")).collect();
    let args: String = args.iter().map(|arg| format!(" '{arg}'")).collect();
    let line = format!(
        "sh -c \"env {env}'{}'{args} > '{}' 2>&1; : > '{}'\"",
        common::taw_bin(),
        printed.display(),
        done.display()
    );
    server.tmux_ok(&["send-keys", "-t", pane, &line, "Enter"]);
    assert!(
        wait_for(|| done.exists(), SETTLE),
        "the report never ran:\n{}",
        server.capture(pane)
    );
    wait_for_the_shell_prompt(server, pane);
    let printed = std::fs::read_to_string(&printed).unwrap_or_default();
    assert!(
        printed.contains("\"result\""),
        "the report failed: {printed}"
    );
    printed
}

/// The two verbs an integration reports with, for `pane`.
fn the_two_reports(pane: &str) -> [Vec<&str>; 2] {
    [
        vec![
            "pane",
            "report-agent",
            pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ],
        vec![
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            "abc",
        ],
    ]
}

/// The pid of a process that has ended.
fn a_dead_pid() -> String {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id().to_string();
    child.wait().unwrap();
    pid
}

/// A report from the job in front of the pane, as an agent's hook makes it, is what a live agent says:
/// both of the verbs an integration reports with take a pid that is dead off the pane, and leave one that
/// lives, or none, as it is. A report from anywhere else (this test's own process here, the hook of an
/// agent that died in the next test) leaves a dead pid where it is.
#[test]
fn a_report_from_the_job_in_front_takes_a_dead_pid_off_the_pane_and_leaves_a_live_one() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &[]);
    let pane = server.pane();
    wait_for_the_shell_prompt(&server, &pane);
    let dead = a_dead_pid();
    // This test's own process lives for as long as it runs.
    let live = std::process::id().to_string();
    for (n, report) in the_two_reports(&pane).iter().enumerate() {
        let dir = |what: &str| tmp.path().join(format!("{n}-{what}"));
        server.set_pane_opt(&pane, "@agent_pid", &dead);
        let (code, envelope) = server.taw(report);
        assert_eq!(code, 0, "{envelope}");
        assert_eq!(
            server.pane_opt(&pane, "@agent_pid"),
            dead,
            "{}: a report from no job in front of the pane took the dead pid off",
            report[1]
        );

        // Typed at the pane's own prompt: the report is the job in front, and its parent, the shell, is not
        // in it. No agent is there to say anything.
        let line: String = std::iter::once(format!("'{}'", common::taw_bin()))
            .chain(report.iter().map(|arg| format!("'{arg}'")))
            .collect::<Vec<_>>()
            .join(" ");
        server.tmux_ok(&[
            "send-keys",
            "-t",
            &pane,
            &format!("{line} > /dev/null 2>&1; : > '{}'", dir("typed").display()),
            "Enter",
        ]);
        assert!(
            wait_for(|| dir("typed").exists(), SETTLE),
            "the typed report never ran"
        );
        wait_for_the_shell_prompt(&server, &pane);
        assert_eq!(
            server.pane_opt(&pane, "@agent_pid"),
            dead,
            "{}: a report typed at the pane's prompt took the dead pid off",
            report[1]
        );

        report_from_the_job_in_front(&server, &pane, report, &[], &dir("dead"));
        assert_eq!(
            server.pane_opt(&pane, "@agent_pid"),
            "",
            "{}: a dead pid stayed on the pane",
            report[1]
        );

        server.set_pane_opt(&pane, "@agent_pid", &live);
        report_from_the_job_in_front(&server, &pane, report, &[], &dir("live"));
        assert_eq!(
            server.pane_opt(&pane, "@agent_pid"),
            live,
            "{}: a pid that lives was taken off the pane",
            report[1]
        );

        // None recorded: nothing to take off, and nothing is made up.
        server.tmux_ok(&["set-option", "-u", "-p", "-t", &pane, "@agent_pid"]);
        report_from_the_job_in_front(&server, &pane, report, &[], &dir("none"));
        assert_eq!(server.pane_opt(&pane, "@agent_pid"), "");
    }
}

/// Opens `gate` when it goes out of scope, so that a test that fails before it opens it lets go of what
/// waits there.
struct OpensTheGate(PathBuf);

impl Drop for OpensTheGate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// The fake agent as `claude` (its pid noted where [`FakeAgent`] reads it), with a hook of its own that
/// reports `working` for the pane once `gate` is there, and notes in `reported` that it has. The hook is
/// the agent's child and in the agent's job, as Claude's hooks are, so once the agent is killed, launchd
/// takes it in, and what it reports then comes from no job in front of the pane. It gives up after a
/// minute, so that nothing is left running when a test fails before it opens the gate.
fn a_claude_whose_hook_reports_late(fake: &FakeAgent, tmp: &Path) -> (PathBuf, PathBuf) {
    let (gate, reported) = (tmp.join("gate"), tmp.join("reported"));
    write_script(
        &fake.bin_dir.join("claude"),
        &format!(
            "printf '%s\\n' \"$$\" > '{pid}'\n\
             (\n\
             \x20 i=0\n\
             \x20 while [ ! -e '{gate}' ]; do\n\
             \x20   [ \"$i\" -ge 1200 ] && exit 0\n\
             \x20   sleep 0.05; i=$((i + 1))\n\
             \x20 done\n\
             \x20 '{taw}' pane report-agent \"$TMUX_PANE\" --source test --agent claude --state working > '{log}' 2>&1\n\
             \x20 : > '{reported}'\n\
             ) &\n\
             exec '{fixture}'\n",
            pid = tmp.join("fake-agent.pid").display(),
            gate = gate.display(),
            taw = common::taw_bin(),
            log = tmp.join("late-report.log").display(),
            reported = reported.display(),
            fixture = common::fake_agent_fixture().display(),
        ),
    );
    (gate, reported)
}

/// An agent `taw agent start` started, at work and reported working by its hook, is killed; its hook
/// reports `working` after it has died, as a hook still on its way does. The pid `agent start` recorded
/// is what says the agent has gone, and a report from no job in front of the pane does not take it off:
/// the pane ends `unknown`, as the pane of an agent that has gone does while its hint stays, and not
/// `working` off the frame the dead agent left (a ghost that stayed for good, and that every wait for
/// the agent took for a turn under way).
#[test]
fn a_late_report_from_the_hook_of_an_agent_that_died_leaves_its_pane_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let (gate, reported) = a_claude_whose_hook_reports_late(&fake, tmp.path());
    let gate = OpensTheGate(gate);
    let pane = pane_with_an_agent_at_work(&server, &fake);
    let (code, envelope) = server.taw(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "test",
        "--agent",
        "claude",
        "--state",
        "working",
    ]);
    assert_eq!(code, 0, "{envelope}");
    let recorded = server.pane_opt(&pane, "@agent_pid");
    assert_eq!(fake.pid().as_deref(), Some(recorded.as_str()));
    fake.signal("-KILL");
    wait_for_the_shell_prompt(&server, &pane);
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never read the exit: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );

    std::fs::write(&gate.0, "").unwrap();
    assert!(
        wait_for(|| reported.exists(), SETTLE),
        "the dead agent's hook never reported"
    );

    let read = || {
        format!(
            "{}/{}/{}",
            server.pane_opt(&pane, "@agent_state"),
            server.pane_opt(&pane, "@agent_source"),
            server.pane_opt(&pane, "@agent_rule")
        )
    };
    assert!(
        wait_for(|| read() == "unknown/manifest/", SETTLE),
        "the dead agent came back: {} (state/source/rule), @agent_pid {:?}",
        read(),
        server.pane_opt(&pane, "@agent_pid")
    );
    // And it stays so: the watcher reads nothing of the frame the dead agent left.
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(read(), "unknown/manifest/");
    }
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        recorded,
        "the late report took the dead pid off"
    );
}

/// An agent `taw agent start` started has exited (its pane keeps the hint, the kind, the name and the pid
/// it recorded, which is dead), and the system has given that number to a program in front of ANOTHER
/// terminal (the program of another pane, its pid written in the record to stand for the reuse). That
/// program leads the foreground group of its own terminal, which says nothing of this pane's. With the
/// pane's shell at its prompt (`nested` false) or running a nested shell, the pid is no agent in front of
/// the pane, and what a prompt carries never runs.
fn assert_a_pid_reused_in_front_of_another_terminal_stands_for_nothing(nested: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server(tmp.path());
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    assert_eq!(code, 0, "{envelope}");
    let started = fake.wait_until_drawn(&server, &pane, SETTLE);
    fake.signal("-TERM");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never read the exit: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), started);
    let other = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        "exec sleep 600",
    ]);
    assert!(
        wait_for(
            || server.display(&other, "#{pane_current_command}") == "sleep",
            SETTLE
        ),
        "the other pane's program never took its terminal"
    );
    let reused = server.display(&other, "#{pane_pid}");
    server.set_pane_opt(&pane, "@agent_pid", &reused);
    if nested {
        server.tmux_ok(&["send-keys", "-t", &pane, "sh", "Enter"]);
        assert!(
            wait_for(|| a_job_is_in_front(&server, &pane), SETTLE),
            "the nested shell never took the terminal"
        );
    }
    let ran = tmp.path().join("ran-in-the-panes-shell");
    let (wrapper, log) = tmux_that_logs(tmp.path());

    let answers = typing_verbs_of(
        &server,
        &wrapper,
        "rev",
        &format!("touch '{}' ; echo {SENTINEL}", ran.display()),
    );

    assert_nothing_was_typed(&server, &pane, &log, answers, gone_as_herdr_says("rev"));
    assert!(
        !wait_for(|| ran.exists(), Duration::from_secs(2)),
        "the pane's shell ran what the prompt said"
    );
}

#[test]
fn a_pid_reused_in_front_of_another_terminal_is_no_agent_in_front_of_a_pane_at_its_prompt() {
    assert_a_pid_reused_in_front_of_another_terminal_stands_for_nothing(false);
}

#[test]
fn a_pid_reused_in_front_of_another_terminal_is_no_agent_in_front_of_a_pane_running_a_nested_shell()
{
    assert_a_pid_reused_in_front_of_another_terminal_stands_for_nothing(true);
}

/// A `tmux` that, right after it answers the read of `@agent_pid` of `pane`, makes the pane's record hold
/// `live` instead: another writer (`agent start` recording the pid of the agent it has just started) lands
/// between a reader's look at the dead pid and what the reader then does about it.
fn tmux_that_records_another_pid_after_the_read(dir: &Path, pane: &str, live: &str) -> PathBuf {
    let wrapper = dir.join("tmux-recording-another-pid");
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *display-message*'#{{@agent_pid}}'*)\n    out=$('{real}' \"$@\"); status=$?\n    '{real}' -L \"$TAW_TMUX_SOCKET_NAME\" set-option -p -t '{pane}' @agent_pid '{live}'\n    printf '%s\\n' \"$out\"; exit $status ;;\nesac\nexec '{real}' \"$@\"\n",
            real = real.display(),
        ),
    );
    wrapper
}

/// A report takes a pid that is dead off the pane, and only that one: read, found dead and unset are
/// three steps, and a pid another writer recorded in between, of an agent that lives, is not the dead
/// one. The unset is one tmux invocation that asks, in tmux, whether the record still holds the pid that
/// was found dead. The report comes from the job in front of the pane, as only such a report takes a
/// dead pid off at all.
#[test]
fn a_report_does_not_take_off_a_pid_that_was_recorded_after_it_found_the_old_one_dead() {
    let tmp = tempfile::tempdir().unwrap();
    let server = server_with_named_agents(tmp.path(), &[]);
    let pane = server.pane();
    wait_for_the_shell_prompt(&server, &pane);
    let dead = a_dead_pid();
    let live = std::process::id().to_string();
    let wrapper = tmux_that_records_another_pid_after_the_read(tmp.path(), &pane, &live);
    let env = [("TAW_TMUX_BIN", wrapper.to_str().unwrap())];
    for (n, report) in the_two_reports(&pane).iter().enumerate() {
        server.set_pane_opt(&pane, "@agent_pid", &dead);

        report_from_the_job_in_front(
            &server,
            &pane,
            report,
            &env,
            &tmp.path().join(format!("report-{n}")),
        );

        assert_eq!(
            server.pane_opt(&pane, "@agent_pid"),
            live,
            "{}: the pid recorded after the read was taken off",
            report[1]
        );
    }
}
