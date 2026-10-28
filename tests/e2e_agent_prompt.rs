//! What `taw agent prompt --wait` waits for, end to end through the built binary and a real tmux
//! server: herdr's rule (`src/api/wait.rs`, `prompt_agent`, the same in v0.9.1, v0.9.3 and master).
//!
//! The verb polled `@agent_state` and returned the first `idle`, `done` or `blocked` it read, so a
//! prompt sent to an agent at rest returned `idle` at once, with no turn seen, and an agent that
//! never took the prompt up looked as finished as one that had done its work. The rule is that
//! after the prompt is sent the agent has to be seen working or blocked within 5 seconds (else
//! `agent_prompt_stalled`, or `timeout` when `--timeout` is the shorter), and then the wait is for
//! the turn's end, within what is left of `--timeout` as one budget. An agent that is working
//! when the prompt is sent has no such gate: the end of the turn it is in satisfies the wait.
//!
//! The fakes are programs of the kind an agent's own hook is: they read the line the verb types
//! and say what they are doing with `taw pane report-agent`, which writes the state the verb
//! reads, so nothing here needs a watcher. A fake leaves a file for each thing it did, and a test
//! says what the verb did by which of them were there when it returned; a fake leaves its file
//! before it reports the state that follows it, so the file is there whenever the verb can have
//! seen that state. Nothing sleeps to wait for the product: a time is only asserted from below.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Server, ServerOptions, taw_bin, wait_for, write_script};

/// Ceiling for a fake to be up and a state to have been written.
const SETTLE: Duration = Duration::from_secs(20);

/// The gate herdr gives a prompt to show an agent at work.
const GATE: Duration = Duration::from_secs(5);

struct Fixture {
    server: Server,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        // The fakes are typed into the shell of a pane: no history file, since a shell saves what it
        // was typed to `$HISTFILE` when the server is killed, which is `~/.bash_history` where nothing
        // says otherwise, and a home of its own.
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let server = Server::start_with(ServerOptions {
            env: vec![
                ("HISTFILE".to_string(), "/dev/null".to_string()),
                ("HOME".to_string(), home.display().to_string()),
            ],
            ..ServerOptions::default()
        });
        server.write_config("[update]\nmanifest_check = false\n");
        Fixture { server, dir }
    }

    /// A new pane whose shell runs a fake agent as a command, in front of the shell as an agent a user
    /// starts is, and not as a script that stands in the shell's place: a pane whose own shell is idle at
    /// its prompt hosts no agent, and the verbs that type refuse it. The script is named for its agent,
    /// as an agent's own program is, since what the verbs that type look for in front of a pane is a
    /// process that names the agent (`sh <dir>/claude` does, by its arguments). `body` runs after the
    /// helpers `report <state>` (what an
    /// agent's hook does), `mark <name>` (leaves a file by that name), `leave` (takes the agent's
    /// record off the pane, as the watcher does when the agent exits) and `leave_as <state>` (the
    /// same, after the agent's last state, with the status it leaves with recorded as the watcher's
    /// clear records it, all in one tmux call, so that no look can find the agent between its last
    /// report and its leaving). It marks `ready` when it is up and the pane is an agent pane, and the
    /// test goes on from there.
    fn agent(&self, body: &str) -> String {
        let script = self.dir.path().join("bin").join("claude");
        let tmux = taw::tmux::Tmux::with_socket_name("unused").bin;
        write_script(
            &script,
            &format!(
                "TAW='{}'\nTMUX_BIN='{}'\nreport() {{ \"$TAW\" pane report-agent \"$TMUX_PANE\" --source test \
                 --agent claude --state \"$1\" > /dev/null 2>&1; }}\nmark() {{ : > '{}'/\"$1\"; }}\n\
                 leave() {{ for option in @agent_kind @agent_state @agent_state_since @agent_rule \
                 @agent_source @agent_turn; do \"$TMUX_BIN\" set-option -u -p -t \"$TMUX_PANE\" \"$option\"; \
                 done; }}\nleave_as() {{ \"$TMUX_BIN\" set-option -p -t \"$TMUX_PANE\" @agent_state \"$1\" \\; \
                 set-option -p -F -t \"$TMUX_PANE\" @agent_final_state \"#{{@agent_state}} $(perl -MTime::HiRes=time -e 'printf q(%d), time * 1000')\" \\; \
                 set-option -u -p -t \"$TMUX_PANE\" @agent_kind \\; set-option -u -p -t \"$TMUX_PANE\" @agent_state \\; \
                 set-option -u -p -t \"$TMUX_PANE\" @agent_state_since \\; set-option -u -p -t \"$TMUX_PANE\" @agent_rule \\; \
                 set-option -u -p -t \"$TMUX_PANE\" @agent_source \\; set-option -u -p -t \"$TMUX_PANE\" @agent_turn; }}\n{body}\n",
                taw_bin(),
                tmux.display(),
                self.dir.path().display()
            ),
        );
        let pane =
            self.server
                .tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
        self.server.tmux_ok(&[
            "send-keys",
            "-t",
            &pane,
            &format!("sh '{}'", script.display()),
            "Enter",
        ]);
        assert!(
            wait_for(|| self.marked("ready"), SETTLE),
            "the fake agent never came up:\n{}",
            self.server.capture(&pane)
        );
        pane
    }

    fn marked(&self, name: &str) -> bool {
        self.dir.path().join(name).exists()
    }

    /// `taw agent prompt <pane> hello --wait` and whatever else `args` adds, with how long it took.
    fn prompt(&self, pane: &str, args: &[&str]) -> (i32, serde_json::Value, Duration) {
        let mut argv = vec!["agent", "prompt", pane, "hello", "--wait"];
        argv.extend_from_slice(args);
        let began = Instant::now();
        let (code, envelope) = self.server.taw(&argv);
        (code, envelope, began.elapsed())
    }
}

fn error_of(envelope: &serde_json::Value) -> (&str, &str) {
    (
        envelope["error"]["code"].as_str().unwrap_or_default(),
        envelope["error"]["message"].as_str().unwrap_or_default(),
    )
}

fn status_of(envelope: &serde_json::Value) -> &str {
    envelope["result"]["agent"]["agent_status"]
        .as_str()
        .unwrap_or_default()
}

#[test]
fn a_prompt_sent_to_an_agent_at_rest_returns_at_the_end_of_the_turn_it_started() {
    let f = Fixture::start();
    let pane = f.agent(
        "report idle\nmark ready\nread line\nmark read\nmark working\nreport working\nsleep 1\n\
         mark ended\nreport idle\nsleep 600",
    );

    let (code, envelope, _) = f.prompt(&pane, &[]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
    // It is idle at the start too, and the verb returned the idle that came after the turn.
    assert!(
        f.marked("working") && f.marked("ended"),
        "the verb returned the idle the agent was at rest in, not the one that ended its turn \
         (working: {}, ended: {}): {envelope}",
        f.marked("working"),
        f.marked("ended")
    );
}

#[test]
fn an_agent_that_never_takes_the_prompt_up_is_a_stalled_prompt_after_five_seconds() {
    let f = Fixture::start();
    let pane = f.agent("report idle\nmark ready\nread line\nmark read\nsleep 600");

    let (code, envelope, took) = f.prompt(&pane, &[]);

    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "agent_prompt_stalled", "{envelope}");
    assert!(
        took >= GATE - Duration::from_millis(200),
        "the gate is five seconds, and the verb gave up after {took:?}"
    );
    // The prompt was delivered, as the message says it may have been.
    assert!(f.marked("read"), "the fake never read the prompt");
    assert!(
        message.contains("may still have been delivered"),
        "the message must say that the prompt may have been delivered: {message}"
    );
    assert!(
        message.contains("idle") && message.contains(&pane),
        "and say which agent and what it was doing: {message}"
    );
}

/// An agent's pane in copy mode takes the keys sent to it as the mode's own (with nobody attached, tmux
/// drops them), so a prompt or keys sent to it never reach the agent, and `agent prompt` returned as if
/// they had. Both verbs refuse such a pane at once, with `shell_not_ready` and the reason, as `agent
/// start` does, and leave it in its mode: a person may be reading the scrollback.
#[test]
fn a_prompt_or_keys_for_an_agent_whose_pane_is_in_copy_mode_are_refused_and_the_mode_is_left_alone()
{
    let f = Fixture::start();
    let pane = f.agent("report idle\nmark ready\nread line\nmark read\nsleep 600");
    f.server.tmux_ok(&["copy-mode", "-t", &pane]);
    let refused = format!("pane {pane} is in copy mode; leave it and try again");

    for args in [
        &["agent", "prompt", &pane, "hello"][..],
        &["agent", "send-keys", &pane, "enter"][..],
    ] {
        let (code, envelope) = f.server.taw(args);

        assert_eq!(code, 1, "{args:?}: {envelope}");
        assert_eq!(
            error_of(&envelope),
            ("shell_not_ready", refused.as_str()),
            "{args:?}: {envelope}"
        );
        assert_eq!(
            f.server.display(&pane, "#{pane_in_mode}"),
            "1",
            "{args:?}: the pane was taken out of copy mode"
        );
    }
    // Out of the mode, the agent takes the prompt.
    f.server.tmux_ok(&["copy-mode", "-q", "-t", &pane]);
    let (code, envelope) = f.server.taw(&["agent", "prompt", &pane, "hello"]);
    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(|| f.marked("read"), SETTLE),
        "the agent never read the prompt"
    );
}

/// A `tmux` for a verb that types which, when the verb comes to the send whose call names `when`
/// (`send-keys` for its first send, `Enter` for the Enter after a prompt's text), first runs `put` on the
/// pane (`copy-mode`, `select-pane -d`), once: as a person does who scrolls back with the mouse just
/// after the verb has looked at the pane and found it taking input.
fn tmux_that_stops_the_input_before(dir: &Path, put: &str, when: &str) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-stops-the-input");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             case \"$*\" in\n\
             \x20 *' if-shell '*'{when}'*)\n\
             \x20   if [ ! -e '{stopped}' ]; then : > '{stopped}'; \"$real\" -L \"$2\" {put} -t \"$7\"; fi ;;\n\
             esac\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            stopped = dir.join("stopped").display(),
        ),
    );
    wrapper
}

/// A pane put in copy mode, or with its input turned off, after the verb's look found it taking input
/// and before tmux sends what the verb types: tmux gives the text and the keys to the mode, which drops
/// them with nobody attached, or sends them nowhere, and the verbs said they had sent them. They are
/// sent only while the pane takes input, in the same tmux call that asks: the verbs refuse with
/// `shell_not_ready` and the reason, and leave the pane as it is. A prompt whose text went, and whose
/// Enter was held back, says the prompt may be left on the agent's line.
#[test]
fn a_prompt_or_keys_for_a_pane_that_stops_taking_input_before_the_send_are_refused() {
    for (put, take_back, state, plain, after_typing) in [
        (
            "copy-mode",
            &["copy-mode", "-q"][..],
            "1 0",
            "is in copy mode; leave it and try again",
            "is in copy mode, and the prompt typed into it may be left on its line; leave the mode, \
             clear the line and try again",
        ),
        (
            "select-pane -d",
            &["select-pane", "-e"][..],
            "0 1",
            "takes no input; turn its input back on and try again",
            "takes no input, and the prompt typed into it may be left on its line; turn its input \
             back on, clear the line and try again",
        ),
    ] {
        let f = Fixture::start();
        let pane = f.agent("report idle\nmark ready\nread line\nmark read\nsleep 600");
        for (args, when, refused) in [
            (["agent", "prompt", &pane, "hello"], "send-keys", plain),
            (["agent", "send-keys", &pane, "enter"], "send-keys", plain),
            (["agent", "prompt", &pane, "hello"], "Enter", after_typing),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let stand_in = tmux_that_stops_the_input_before(dir.path(), put, when);

            let (code, envelope) = f
                .server
                .taw_with_env(&args, &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())]);

            assert!(
                dir.path().join("stopped").exists(),
                "{put} before {when}, {args:?}: the stand-in never ran"
            );
            assert_eq!(code, 1, "{put} before {when}, {args:?}: {envelope}");
            assert_eq!(
                error_of(&envelope),
                ("shell_not_ready", format!("pane {pane} {refused}").as_str()),
                "{put} before {when}, {args:?}: {envelope}"
            );
            assert_eq!(
                f.server.display(&pane, "#{pane_in_mode} #{pane_input_off}"),
                state,
                "{put} before {when}, {args:?}: the pane was not left as it was"
            );
            let mut back = take_back.to_vec();
            back.extend(["-t", pane.as_str()]);
            f.server.tmux_ok(&back);
        }
        // Taking input again, the agent takes the prompt.
        let (code, envelope) = f.server.taw(&["agent", "prompt", &pane, "hello"]);
        assert_eq!(code, 0, "{put}: {envelope}");
        assert!(
            wait_for(|| f.marked("read"), SETTLE),
            "{put}: the agent never read the prompt"
        );
    }
}

#[test]
fn a_prompt_that_turns_the_agent_blocked_returns_blocked_at_once() {
    let f = Fixture::start();
    let pane =
        f.agent("report idle\nmark ready\nread line\nmark blocked\nreport blocked\nsleep 600");

    let (code, envelope, _) = f.prompt(&pane, &[]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "blocked", "{envelope}");
    assert!(
        f.marked("blocked"),
        "the verb returned before the agent was blocked: {envelope}"
    );
}

#[test]
fn an_agent_that_is_working_when_the_prompt_is_sent_is_waited_for_to_the_end_of_its_turn() {
    let f = Fixture::start();
    let pane = f.agent(
        "report working\nmark ready\nread line\nmark read\nsleep 1\nmark ended\nreport idle\n\
         sleep 600",
    );

    let (code, envelope, _) = f.prompt(&pane, &[]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
    assert!(
        f.marked("ended"),
        "the verb did not wait for the turn that was under way: {envelope}"
    );
}

#[test]
fn a_blocked_state_the_agent_was_already_in_is_not_the_activity_the_prompt_caused() {
    // taw sends the text to a blocked agent (answering a dialog is often what the caller wants), and
    // the `blocked` that was there before the prompt says nothing of what the prompt did.
    let f = Fixture::start();
    let pane = f.agent(
        "report blocked\nmark ready\nread line\nmark read\nsleep 1\nreport working\nsleep 1\n\
         mark ended\nreport idle\nsleep 600",
    );

    let (code, envelope, _) = f.prompt(&pane, &[]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
    assert!(
        f.marked("ended"),
        "the verb returned the blocked state the agent was in before the prompt: {envelope}"
    );
}

/// A `tmux` that lets the verb send what it was asked to and then holds its next look at the pane
/// (the call that reads `@agent_turn`) until the fake has marked `reported`, which it does after
/// its last report: a look that lands between two of them sees the state of the first, and the
/// turn is then seen by the state alone. It says in `gave-up` when it ran out of time, ten seconds,
/// so a test cannot pass for a hold that never held.
fn tmux_that_holds_the_verbs_look_until_the_fake_has_reported(
    dir: &std::path::Path,
) -> std::path::PathBuf {
    let wrapper = dir.join("tmux-holding-the-look");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n  *send-keys*) : > '{o}/sent' ;;\n  *\"@agent_turn\"*)\n    if [ -e '{o}/sent' ]; then\n      i=0\n      while [ ! -e '{o}/reported' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n      [ -e '{o}/reported' ] || : > '{o}/gave-up'\n    fi ;;\nesac\nexec '{real}' \"$@\"\n",
            o = dir.display(),
            real = taw::tmux::Tmux::with_socket_name("unused").bin.display(),
        ),
    );
    wrapper
}

#[test]
fn a_turn_that_began_and_ended_between_two_looks_is_not_a_stalled_prompt() {
    // An agent's hook reports `working` and then `idle` before the verb has looked at the pane
    // again: the state is the one it was, and the turn stamp the report of work moved is what shows
    // that a turn was. The verb's look is held back until both reports have landed, so that it
    // cannot be the `working` between them that it sees.
    let f = Fixture::start();
    let pane = f.agent(
        "report idle\nmark ready\nread line\nreport working\nmark ended\nreport idle\n\
         mark reported\nsleep 600",
    );
    let wrapper = tmux_that_holds_the_verbs_look_until_the_fake_has_reported(f.dir.path());

    let began = Instant::now();
    let (code, envelope) = f.server.taw_with_env(
        &["agent", "prompt", &pane, "hello", "--wait"],
        &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
    );
    let took = began.elapsed();

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
    assert!(
        !f.marked("gave-up"),
        "the look was let go by the clock before the fake had reported: this run proves nothing"
    );
    assert!(
        took < GATE,
        "the verb waited out the gate for a turn that had already happened: {took:?}"
    );
}

#[test]
fn a_timeout_shorter_than_the_gate_is_a_timeout_and_not_a_stalled_prompt() {
    let f = Fixture::start();
    let pane = f.agent("report idle\nmark ready\nread line\nmark read\nsleep 600");

    let (code, envelope, took) = f.prompt(&pane, &["--timeout", "2000"]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "timeout", "{envelope}");
    assert!(
        took >= Duration::from_millis(1800),
        "it gave up after {took:?}, before the 2000 ms it was given"
    );
}

#[test]
fn the_gate_and_the_wait_for_the_end_of_the_turn_are_one_budget() {
    let f = Fixture::start();
    // The agent takes a second to show that it is working, and then works for good.
    let pane = f.agent(
        "report idle\nmark ready\nread line\nsleep 1\nmark working\nreport working\nsleep 600",
    );
    let budget_ms: u64 = 5000;

    let (code, envelope, _) = f.prompt(&pane, &["--timeout", &budget_ms.to_string()]);

    // The turn never ends, so the budget runs out in the wait for its end, and what it says it ran
    // out of is what the gate left of it: the gate took at least the second the agent slept.
    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "timeout", "{envelope}");
    let left_ms: u64 = message
        .strip_prefix("timed out after ")
        .and_then(|rest| rest.split("ms").next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("the timeout does not say how long it waited: {message}"));
    assert!(
        left_ms <= budget_ms - 1000,
        "the wait for the end of the turn had {left_ms} ms of a {budget_ms} ms budget after a gate \
         of at least 1000 ms: they do not share it ({message})"
    );
    assert!(f.marked("working"));
}

/// An agent's hook that says again what the agent already was (`blocked`, `blocked`) is not the
/// agent doing something with the prompt: the report moved `@agent_state_since` and the turn stamp
/// all the same, and the gate took it for activity, so a prompt to an agent that sat at a dialog came
/// back `blocked` at once whatever it did. Only a report of work, or a change of state, is the
/// agent's doing, and a prompt that none of them follows is stalled.
#[test]
fn a_report_of_the_state_the_agent_was_already_in_is_not_activity() {
    let f = Fixture::start();
    let pane =
        f.agent("report blocked\nmark ready\nread line\nmark read\nreport blocked\nsleep 600");

    let (code, envelope, took) = f.prompt(&pane, &[]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_prompt_stalled", "{envelope}");
    assert!(
        took >= GATE - Duration::from_millis(200),
        "the gate is five seconds, and the verb gave up after {took:?}"
    );
    assert!(f.marked("read"), "the fake never read the prompt");
}

/// The other side of it: a report that changes the state is activity, `blocked` among them, even
/// when the agent had been blocked before and has been something else since.
#[test]
fn a_report_that_changes_the_state_to_blocked_is_activity() {
    let f = Fixture::start();
    let pane = f.agent(
        "report blocked\nmark ready\nread line\nreport idle\nreport blocked\nmark again\nsleep 600",
    );

    let (code, envelope, took) = f.prompt(&pane, &[]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "blocked", "{envelope}");
    assert!(
        f.marked("again") && took < GATE,
        "the verb waited out the gate for a state that had changed ({took:?}): {envelope}"
    );
}

/// A prompt that ends the agent (`/exit`) takes the agent's record off the pane, the turn stamp with
/// it. The stamp that went was a change of the stamp, and the gate took it for the prompt having
/// started a turn: the verb went on to wait for a state that a pane with no agent never has, and
/// without `--timeout` that is for ever. The agent is gone, and the verb says so, as herdr does
/// (`agent_not_running`, from its `wait_for_resolved_agent`), at once.
#[test]
fn a_prompt_that_ends_the_agent_is_an_agent_that_is_not_running() {
    let f = Fixture::start();
    let pane = f.agent(
        "report working\nreport idle\nmark ready\nread line\nmark read\nmark leaving\nleave\n\
         sleep 600",
    );

    let (code, envelope, took) = f.prompt(&pane, &["--timeout", "8000"]);

    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "agent_not_running", "{envelope}");
    assert!(
        message.contains(&pane) && message.contains("no longer running"),
        "{message}"
    );
    assert!(
        f.marked("leaving"),
        "the verb ended before the fake began to leave: {envelope}"
    );
    assert!(
        took < GATE,
        "the verb waited {took:?} for an agent that had gone: {envelope}"
    );
}

/// The wait for the end of the turn ends when the agent goes: a state that a pane with no agent
/// does not have is not worth waiting for, with or without a budget.
#[test]
fn the_wait_for_the_end_of_the_turn_ends_when_the_agent_goes() {
    let f = Fixture::start();
    let pane = f.agent(
        "report idle\nmark ready\nread line\nreport working\nsleep 1\nmark leaving\nleave\nsleep 600",
    );

    let (code, envelope, took) = f.prompt(&pane, &["--timeout", "30000"]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
    assert!(
        f.marked("leaving"),
        "the verb ended before the fake began to leave: {envelope}"
    );
    assert!(
        took < Duration::from_secs(20),
        "the verb went on waiting for an agent that had gone ({took:?}): {envelope}"
    );
}

/// `agent wait` is the same wait, and ends the same way.
#[test]
fn agent_wait_ends_when_the_agent_goes() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 1\nmark leaving\nleave\nsleep 600");

    let began = Instant::now();
    let (code, envelope) = f.server.taw(&[
        "agent",
        "wait",
        &pane,
        "--state",
        "idle",
        "--timeout",
        "30000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "the wait went on for an agent that had gone: {envelope}"
    );
}

/// herdr's `wait_for_resolved_agent` answers a wait with the final status an agent is released with
/// when the wait was for it, and with `agent_not_running` only when it was not. An agent that ends
/// its turn and leaves in one step (a one-shot run) leaves no look a state to read, so what it left
/// with is the answer, and the pane records it where a wait can read it as the record is taken off.
#[test]
fn an_agent_that_ends_its_turn_and_leaves_at_once_is_answered_with_the_status_it_left() {
    let f = Fixture::start();
    let pane = f.agent(
        "report idle\nmark ready\nread line\nreport working\nsleep 1\nleave_as idle\nsleep 600",
    );

    let (code, envelope, _) = f.prompt(&pane, &["--timeout", "30000"]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
}

/// The same for `agent wait`, and for a status the wait did not ask for: the agent left, and the wait
/// is not for what it left with.
#[test]
fn an_agent_that_leaves_with_a_status_the_wait_was_not_for_is_not_running() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 1\nleave_as idle\nsleep 600");

    let (code, envelope) = f.server.taw(&[
        "agent",
        "wait",
        &pane,
        "--state",
        "blocked",
        "--timeout",
        "30000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
}

/// What the last agent of a pane left with is not the answer of a wait for the next: it is read only
/// when it was left after the wait began.
#[test]
fn a_status_the_last_agent_left_is_not_the_answer_of_a_wait_that_began_after_it() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 3\nleave\nsleep 600");
    f.server.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_final_state",
        "idle 1000000000",
    ]);

    let (code, envelope) = f
        .server
        .taw(&["agent", "wait", &pane, "--timeout", "30000"]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
}

/// The same for a status the last agent left a moment before the wait began, in the same second: the
/// time it was left at is in milliseconds, so it is told from the wait's own start, where whole seconds
/// could not.
#[test]
fn a_status_left_a_moment_before_a_wait_began_in_the_same_second_is_not_its_answer() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 4\nleave\nsleep 600");
    let millis = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    };
    // Early in a second, so that what is written and the wait that begins right after it share one.
    assert!(
        wait_for(|| millis() % 1000 < 250, Duration::from_secs(5)),
        "the clock never came to the start of a second"
    );
    f.server.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_final_state",
        &format!("done {}", millis()),
    ]);

    let (code, envelope) = f
        .server
        .taw(&["agent", "wait", &pane, "--timeout", "30000"]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
}

/// What herdr's `wait_for_resolved_agent` does when another agent takes the pane
/// (`agent_wait_identity_matches`): the wait ends with `agent_not_running`, not waiting for
/// the newcomer to reach the state it had been waiting for the agent to reach.
#[test]
fn a_wait_ends_when_another_kind_takes_the_pane() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 600");

    let (code, envelope, took) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let began = Instant::now();
            let (code, envelope) = f.server.taw(&[
                "agent",
                "wait",
                &pane,
                "--state",
                "idle",
                "--timeout",
                "20000",
            ]);
            (code, envelope, began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(800));
        // Another agent reports for the pane, in the very state the wait is for: it is not the
        // answer, since the agent that was waited for is not the one that is idle.
        let (code, envelope) = f.server.taw(&[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "codex",
            "--state",
            "idle",
        ]);
        assert_eq!(code, 0, "{envelope}");
        waiting.join().unwrap()
    });

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
    assert!(
        took < Duration::from_secs(15),
        "the wait went on for the newcomer ({took:?}): {envelope}"
    );
}

/// The same kind of agent, in another session of its own: its integration reports the session it
/// runs, and a different one is a different agent. A session an agent reports for the first time is
/// still the agent.
#[test]
fn another_session_of_the_same_kind_is_another_agent_and_a_first_session_is_not() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 600");
    let session = |id: &str| {
        let (code, envelope) = f.server.taw(&[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            id,
        ]);
        assert_eq!(code, 0, "{envelope}");
    };
    let report = |state: &str| {
        let (code, envelope) = f.server.taw(&[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            state,
        ]);
        assert_eq!(code, 0, "{envelope}");
    };
    let first = "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42";
    let second = "0b3c8e1d-52a4-4f0e-8a77-9d2c61e4b5aa";

    // An agent that reports its session after the wait began is the agent the wait began with.
    let (code, envelope) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            f.server.taw(&[
                "agent",
                "wait",
                &pane,
                "--state",
                "idle",
                "--timeout",
                "20000",
            ])
        });
        std::thread::sleep(Duration::from_millis(800));
        session(first);
        std::thread::sleep(Duration::from_millis(600));
        report("idle");
        waiting.join().unwrap()
    });
    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");

    // Another session of the kind, reported while the wait is on, is another agent.
    report("working");
    let (code, envelope, took) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let began = Instant::now();
            let (code, envelope) = f.server.taw(&[
                "agent",
                "wait",
                &pane,
                "--state",
                "idle",
                "--timeout",
                "20000",
            ]);
            (code, envelope, began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(800));
        session(second);
        waiting.join().unwrap()
    });
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
    assert!(took < Duration::from_secs(15), "{took:?}: {envelope}");
}

/// The pid `taw agent start` recorded names what it started, and another is another agent.
#[test]
fn another_recorded_pid_is_another_agent() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 600");
    f.server
        .tmux_ok(&["set-option", "-p", "-t", &pane, "@agent_pid", "4242"]);

    let (code, envelope, took) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            let began = Instant::now();
            let (code, envelope) = f.server.taw(&[
                "agent",
                "wait",
                &pane,
                "--state",
                "idle",
                "--timeout",
                "20000",
            ]);
            (code, envelope, began.elapsed())
        });
        std::thread::sleep(Duration::from_millis(800));
        f.server
            .tmux_ok(&["set-option", "-p", "-t", &pane, "@agent_pid", "4343"]);
        waiting.join().unwrap()
    });

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_running", "{envelope}");
    assert!(took < Duration::from_secs(15), "{took:?}: {envelope}");
}

/// Claude's `/clear` starts a new session in the same process, and `/resume` switches to another: its
/// integration reports the new session id, and the pid `taw agent start` recorded says it is the
/// process the wait began with. That is the agent the wait is for, whatever session it is in now.
#[test]
fn a_new_session_in_the_same_process_is_the_same_agent() {
    let f = Fixture::start();
    let pane = f.agent("report working\nmark ready\nsleep 600");
    // The pid of the fake agent, which lives: a report takes a pid that is dead off the pane.
    let shell = f.server.display(&pane, "#{pane_pid}");
    let children = std::process::Command::new("pgrep")
        .args(["-P", &shell])
        .output()
        .unwrap();
    let agent = String::from_utf8_lossy(&children.stdout)
        .split_whitespace()
        .last()
        .expect("the fake agent has no process")
        .to_string();
    f.server
        .tmux_ok(&["set-option", "-p", "-t", &pane, "@agent_pid", &agent]);
    let session = |id: &str| {
        let (code, envelope) = f.server.taw(&[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            id,
        ]);
        assert_eq!(code, 0, "{envelope}");
    };
    session("3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42");

    let (code, envelope) = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| {
            f.server.taw(&[
                "agent",
                "wait",
                &pane,
                "--state",
                "idle",
                "--timeout",
                "20000",
            ])
        });
        std::thread::sleep(Duration::from_millis(800));
        // `/clear`: the same process, a new session.
        session("0b3c8e1d-52a4-4f0e-8a77-9d2c61e4b5aa");
        std::thread::sleep(Duration::from_millis(800));
        assert!(!waiting.is_finished(), "a new session ended the wait");
        let (code, envelope) = f.server.taw(&[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ]);
        assert_eq!(code, 0, "{envelope}");
        waiting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
}

/// The state and the turn stamp are written by one call, as an agent's report is, and so they have
/// to be read by one: read by two, a look can pair the state from before a report with the stamp it
/// moved, and a prompt to an agent at rest comes back `idle` as if its turn were over before it began
/// (which this test's first form of the verb did, once in three runs). A `tmux` that logs what it
/// is asked says what the verb reads, and in how many calls.
#[test]
fn the_state_and_the_turn_stamp_are_read_in_one_tmux_call() {
    let f = Fixture::start();
    let pane = f.agent(
        "report idle\nmark ready\nread line\nmark working\nreport working\nsleep 1\nmark ended\n\
         report idle\nsleep 600",
    );
    let log = f.dir.path().join("tmux-calls.txt");
    let wrapper = f.dir.path().join("tmux-logging");
    write_script(
        &wrapper,
        &format!(
            "{{ printf '%s' \"$*\" | tr '\\n\\t\\r' '   '; echo; }} >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            taw::tmux::Tmux::with_socket_name("unused").bin.display()
        ),
    );

    let (code, envelope) = f.server.taw_with_env(
        &["agent", "prompt", &pane, "hello", "--wait"],
        &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(status_of(&envelope), "idle", "{envelope}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let reading_the_stamp: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("@agent_turn"))
        .collect();
    assert!(
        !reading_the_stamp.is_empty(),
        "the verb never read the turn stamp:\n{calls}"
    );
    for call in reading_the_stamp {
        assert!(
            call.contains("display-message") && call.contains("@agent_state"),
            "the turn stamp was read apart from the state it goes with: {call}"
        );
    }
}
