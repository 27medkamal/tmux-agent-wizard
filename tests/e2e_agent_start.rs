//! What `taw agent start` says of an agent that never started, end to end through the built
//! binary and a real tmux server.
//!
//! The verb must not name the agent with the kind it was given as its own hint, or its wait for
//! the agent passed on the first look whatever had happened: with no `claude` installed, the pane
//! showed "command not found" and the verb reported success. It passes only when the typed
//! command really started the agent: the pane's own shell is not in front, and either the
//! process in front names the agent, or the agent's own integration reported it, or the watcher
//! has read a state for it off the screen. A shell back at its prompt before any of that is the
//! agent not starting, and the verb says so at once; the budget running out is `timeout`.
//!
//! Nothing here sleeps to wait for the product. A wait is a condition with a ceiling, and a time
//! is only ever asserted from below (the product cannot be quicker than what it waits for) or
//! from a causal bound: `--timeout` is one budget, and the message of the `timeout` it ends with
//! says how much of it was left, which is exactly what the shell's wait before it left over.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{
    FakeAgent, NotifyRecorder, Server, ServerOptions, tmux_named, wait_for, write_script,
};

/// Ceiling for the watcher to have read an agent: a new agent reads `unknown` for 3 s and one
/// more tick first, and `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(25);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// A new window whose shell has no `claude` to find: a `/bin/sh` that is the pane's own process,
/// with a `PATH` of the system's directories only.
fn pane_without_any_agent(server: &Server) -> String {
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        "exec env PATH=/usr/bin:/bin /bin/sh",
    ])
}

/// The message of the error of an `agent start` that failed.
fn error_of(envelope: &serde_json::Value) -> (&str, &str) {
    (
        envelope["error"]["code"].as_str().unwrap_or_default(),
        envelope["error"]["message"].as_str().unwrap_or_default(),
    )
}

#[test]
fn agent_start_fails_at_once_when_the_command_is_not_found_and_leaves_no_agent_behind() {
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    // A hint the pane had before: what the verb sets for its own purpose goes again, and what
    // was there comes back.
    server.set_pane_opt(&pane, "@agent_kind_hint", "codex");

    // The default timeout is thirty seconds, which a verb that went on waiting would sit out and
    // end with `timeout`: a different code is the proof that it did not.
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["id"], "cli:agent:start", "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "agent_not_started", "{envelope}");
    assert!(
        message.contains("did not start") && message.contains(&pane),
        "the message must say which pane the agent did not start in: {message}"
    );
    // The command was typed and the shell said what it thought of it.
    assert!(
        server.capture(&pane).contains("not found"),
        "the shell never said the command was not found:\n{}",
        server.capture(&pane)
    );
    // Nothing of the failed start is left on the pane, the line its command was typed on included.
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_typed_line"), "");
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind_hint"),
        "codex",
        "a failed start must put back the hint the pane had"
    );
}

/// A `tmux` that writes each command it is given to the returned log, one line each, and runs it.
fn tmux_that_logs(dir: &Path) -> (PathBuf, PathBuf) {
    let (wrapper, log) = (dir.join("tmux-that-logs"), dir.join("tmux-calls.txt"));
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

/// A pane in copy mode takes the keys sent to it as the mode's own (with nobody attached, tmux drops
/// them), and one whose input is off (`select-pane -d`) takes none: the command never reached the shell,
/// and the verb waited out its whole budget for an agent that was never started, leaving its hint and the
/// mark of an agent being taken on the pane. It refuses such a pane at once, with `shell_not_ready` and
/// the reason, before it types or marks anything, and leaves it as it was: still in its mode (a person may
/// be reading the scrollback), and with nothing of a start on it.
#[test]
fn agent_start_refuses_a_pane_that_takes_no_input_at_once_and_leaves_it_as_it_was() {
    for (state, why) in [
        (
            &["copy-mode"][..],
            "is in copy mode; leave it and try again",
        ),
        (
            &["select-pane", "-d"][..],
            "takes no input; turn its input back on and try again",
        ),
    ] {
        let server = Server::start();
        server.write_config(CONFIG);
        let pane = pane_without_any_agent(&server);
        assert!(
            wait_for(|| server.capture(&pane).contains('$'), SETTLE),
            "the shell never drew a prompt:\n{}",
            server.capture(&pane)
        );
        let mut put = state.to_vec();
        put.extend(["-t", pane.as_str()]);
        server.tmux_ok(&put);
        let before = server.display(&pane, "#{pane_in_mode} #{pane_input_off}");
        let tmp = tempfile::tempdir().unwrap();
        let (logging, log) = tmux_that_logs(tmp.path());

        let (code, envelope) = server.taw_with_env(
            &[
                "agent",
                "start",
                "rev",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "20000",
            ],
            &[("TAW_TMUX_BIN", logging.to_str().unwrap())],
        );

        assert_eq!(code, 1, "{envelope}");
        assert_eq!(
            error_of(&envelope),
            ("shell_not_ready", format!("pane {pane} {why}").as_str()),
            "{envelope}"
        );
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let typed_or_marked = calls.lines().any(|call| {
            let words: Vec<&str> = call.split_whitespace().collect();
            words.contains(&"send-keys") || words.contains(&"set-option")
        });
        assert!(
            !typed_or_marked,
            "{why}: the verb typed or marked something before it refused:\n{calls}"
        );
        assert_eq!(
            server.display(&pane, "#{pane_in_mode} #{pane_input_off}"),
            before,
            "the pane was not left as it was"
        );
        for option in [
            "@agent_kind_hint",
            "@agent_acquiring",
            "@agent_typed_line",
            "@agent_kind",
            "@agent_name",
        ] {
            assert_eq!(server.pane_opt(&pane, option), "", "{why}: {option}");
        }
    }
}

/// A `tmux` that puts the pane in copy mode just before the verb sends the command's text, as a person
/// scrolling back with the mouse at that moment does, and notes in `text-sent` the call that carries the
/// text and in `enter-sent` any Enter the verb sends. The text goes in a guarded call, `if-shell -F -t
/// <pane> <condition> 'send-keys -l -t <pane> -- <text>' <else>` (`cli::common::send_text_if_in_front`):
/// its pane is the seventh word, after `-L <socket> -u if-shell -F -t`, and its send the ninth.
fn tmux_that_enters_copy_mode_before_the_text(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-enters-copy-mode");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             if [ \"$4\" = if-shell ]; then\n\
             \x20 case \"$9\" in\n\
             \x20   'send-keys -l '*) : > '{dir}/text-sent'; \"$real\" -L \"$2\" copy-mode -t \"$7\" ;;\n\
             \x20   'send-keys -t '*' Enter') : > '{dir}/enter-sent' ;;\n\
             \x20 esac\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            dir = dir.display(),
        ),
    );
    wrapper
}

/// A pane put in copy mode after its prompt was seen and before the command was typed is typed nothing: the
/// text goes only while the pane takes input, which tmux is asked in the call that sends it, so the mode is
/// given none of it. The verb sends no Enter, refuses as it does a pane in copy mode from the start, and
/// leaves nothing of a start on it.
#[test]
fn agent_start_refuses_a_pane_put_in_copy_mode_while_the_command_was_typed() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let stand_in = tmux_that_enters_copy_mode_before_the_text(tmp.path());

    let (code, envelope) = server.taw_with_env(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--timeout",
            "20000",
        ],
        &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
    );

    assert_eq!(code, 1, "{envelope}");
    assert!(
        tmp.path().join("text-sent").exists(),
        "the stand-in never saw the text sent"
    );
    assert_eq!(
        error_of(&envelope),
        (
            "shell_not_ready",
            format!("pane {pane} is in copy mode; leave it and try again").as_str()
        ),
        "{envelope}"
    );
    assert!(
        !tmp.path().join("enter-sent").exists(),
        "Enter was sent after the text"
    );
    assert_eq!(server.display(&pane, "#{pane_in_mode}"), "1");
    server.tmux_ok(&["copy-mode", "-q", "-t", &pane]);
    assert!(
        !server.capture(&pane).contains("claude"),
        "the command reached the shell's line:\n{}",
        server.capture(&pane)
    );
    for option in [
        "@agent_kind_hint",
        "@agent_acquiring",
        "@agent_typed_line",
        "@agent_kind",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
}

/// What `agent start` says of pane `pane` once it is found in copy mode after the command was typed.
fn typed_then_refused(pane: &str) -> String {
    format!(
        "pane {pane} is in copy mode, and the command typed into it may be left on its line; leave the \
         mode, clear the line and try again"
    )
}

/// The pane of a `/bin/sh` at its prompt, with no `claude` to find, and a `tmux` for the verb that puts
/// the pane in copy mode at `when`: `text` once the command's text has reached the shell's line (a person
/// scrolling back with the mouse at that moment), `enter` just before the verb sends Enter.
fn copy_mode_while_a_start_types(server: &Server, dir: &Path, when: &str) -> (String, PathBuf) {
    let pane = pane_without_any_agent(server);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-enters-copy-mode");
    // The text goes in a guarded call, `if-shell ... 'send-keys -l -t <pane> -- <text>' ...`, and so does
    // the Enter.
    let trigger = match when {
        "text" => {
            "*' if-shell '*' send-keys -l -t '*)\n\
             \x20   \"$real\" \"$@\"; status=$?\n\
             \x20   i=0\n\
             \x20   while [ $i -lt 100 ] && ! \"$real\" -L \"$2\" capture-pane -p -t \"$pane\" | grep -q 'claude'; do\n\
             \x20     sleep 0.02; i=$((i + 1))\n\
             \x20   done\n\
             \x20   \"$real\" -L \"$2\" copy-mode -t \"$pane\"\n\
             \x20   exit $status ;;"
        }
        _ => {
            "*' if-shell '*'send-keys -t '*' Enter'*) \"$real\" -L \"$2\" copy-mode -t \"$pane\" ;;"
        }
    };
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             pane=''\n\
             previous=''\n\
             for word in \"$@\"; do\n\
             \x20 [ \"$previous\" = -t ] && [ -z \"$pane\" ] && pane=\"$word\"\n\
             \x20 previous=\"$word\"\n\
             done\n\
             case \"$*\" in\n\
             \x20 {trigger}\n\
             esac\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
        ),
    );
    (pane, wrapper)
}

/// A person puts the pane in copy mode just after the command's text reached the shell's line: the verb,
/// waiting for the echo, refuses, and says the command may be left on its line, which it is: the verb does
/// not take the pane out of its mode to clear it.
#[test]
fn copy_mode_after_the_command_reached_its_line_is_refused_and_said_to_leave_it_there() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let (pane, stand_in) = copy_mode_while_a_start_types(&server, tmp.path(), "text");

    let (code, envelope) = server.taw_with_env(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--timeout",
            "20000",
        ],
        &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
    );

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        error_of(&envelope),
        ("shell_not_ready", typed_then_refused(&pane).as_str()),
        "{envelope}"
    );
    assert_eq!(server.display(&pane, "#{pane_in_mode}"), "1");
    server.tmux_ok(&["copy-mode", "-q", "-t", &pane]);
    assert!(
        server.capture(&pane).trim_end().ends_with("$ claude"),
        "the command is not on the shell's line, as the message says it may be:\n{}",
        server.capture(&pane)
    );
    for option in ["@agent_kind_hint", "@agent_acquiring", "@agent_typed_line"] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
}

/// A person puts the pane in copy mode once the shell has echoed the command, just before the verb sends
/// Enter: the mode had the Enter, the command stayed on the shell's line, and the verb, finding the shell
/// idle with something typed, said the agent did not start. The Enter is sent only while the pane takes
/// input, in the tmux call that asks: the verb refuses with `shell_not_ready` and says the command may be
/// left on its line, which it is, and leaves nothing of a start on the pane.
#[test]
fn copy_mode_just_before_the_enter_is_refused_and_said_to_leave_the_command_on_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let (pane, stand_in) = copy_mode_while_a_start_types(&server, tmp.path(), "enter");

    let (code, envelope) = server.taw_with_env(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--timeout",
            "20000",
        ],
        &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
    );

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        error_of(&envelope),
        ("shell_not_ready", typed_then_refused(&pane).as_str()),
        "{envelope}"
    );
    assert_eq!(server.display(&pane, "#{pane_in_mode}"), "1");
    server.tmux_ok(&["copy-mode", "-q", "-t", &pane]);
    assert!(
        server.capture(&pane).trim_end().ends_with("$ claude"),
        "the command is not on the shell's line, as the message says it may be:\n{}",
        server.capture(&pane)
    );
    for option in [
        "@agent_kind_hint",
        "@agent_acquiring",
        "@agent_typed_line",
        "@agent_kind",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
}

/// A `tmux` for one `agent start` that holds its first call matching `pattern` (a `case` pattern over its
/// arguments) until `go` is in `dir`, saying so by making `held` there first: the moment between the
/// verb's look at the pane and what it sends there.
fn tmux_holding(dir: &Path, pattern: &str) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding");
    write_script(
        &wrapper,
        &format!(
            "case \"$*\" in\n\
             \x20 {pattern})\n\
             \x20   if mkdir '{dir}/held' 2>/dev/null; then\n\
             \x20     i=0\n\
             \x20     while [ ! -e '{dir}/go' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i + 1)); done\n\
             \x20   fi ;;\n\
             esac\n\
             exec '{real}' \"$@\"\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// What takes a pane from its shell while the command of a start is on its way to it.
#[derive(Clone, Copy, PartialEq)]
enum Taken {
    /// A program the shell runs, before the command is typed.
    BeforeTheCommand,
    /// The same, once the command is typed and before its Enter: the line is cleared with C-c first.
    BeforeTheEnter,
}

/// The command, and its Enter, go to the pane only while the shell the start found at its prompt is still
/// in front of it, as a restore types its own: a program started there in between (`cat`) is given none of
/// it, and the start says so (`shell_not_ready`) and puts the pane back as it found it, the hint it had
/// included.
fn a_program_in_front_of_the_shell_gets_nothing_of_a_start(taken: Taken) {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    server.set_pane_opt(&pane, "@agent_kind_hint", "codex");
    let hold = tmp.path().join("hold");
    std::fs::create_dir_all(&hold).unwrap();
    let pattern = match taken {
        Taken::BeforeTheCommand => "*'send-keys -l -t '*",
        Taken::BeforeTheEnter => "*'send-keys -t '*' Enter'*",
    };
    let wrapper = tmux_holding(&hold, pattern);
    let typed_into_cat = tmp.path().join("typed-into-cat");

    let (code, envelope) = std::thread::scope(|scope| {
        let start = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    "15000",
                ],
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        let held = wait_for(|| hold.join("held").exists(), SETTLE);
        if held {
            if taken == Taken::BeforeTheEnter {
                assert!(
                    wait_for(|| server.capture(&pane).contains("$ claude"), SETTLE),
                    "the command was never typed:\n{}",
                    server.capture(&pane)
                );
                server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
            }
            server.tmux_ok(&[
                "send-keys",
                "-t",
                &pane,
                "-l",
                &format!("cat > '{}'", typed_into_cat.display()),
            ]);
            server.tmux_ok(&["send-keys", "-t", &pane, "Enter"]);
            assert!(
                wait_for(
                    || server.display(&pane, "#{pane_current_command}") == "cat",
                    SETTLE
                ),
                "cat never came up:\n{}",
                server.capture(&pane)
            );
        }
        std::fs::write(hold.join("go"), "").unwrap();
        let outcome = start.join().unwrap();
        assert!(held, "the start never sent the command: {outcome:?}");
        outcome
    });

    assert_eq!(code, 1, "{envelope}");
    let said = match taken {
        Taken::BeforeTheCommand => format!(
            "pane {pane} had another program in front of its shell when the command was to be typed, so \
             nothing was typed. Start the agent again once the shell is at its prompt."
        ),
        Taken::BeforeTheEnter => format!(
            "pane {pane} had another program in front of its shell when the command was to be run, and the \
             command typed into it may be left on its line; clear the line and start the agent again once the \
             shell is at its prompt."
        ),
    };
    assert_eq!(
        error_of(&envelope),
        ("shell_not_ready", said.as_str()),
        "{envelope}"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "codex");
    for option in [
        "@agent_acquiring",
        "@agent_typed_line",
        "@agent_kind",
        "@agent_name",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
    // cat writes each line it is given, and the end of its input ends it.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-d"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "cat",
            SETTLE
        ),
        "cat never ended"
    );
    assert_eq!(
        std::fs::read_to_string(&typed_into_cat).unwrap_or_default(),
        "",
        "the start typed into cat"
    );
}

#[test]
fn a_program_in_front_of_the_shell_before_the_command_is_typed_gets_none_of_it() {
    a_program_in_front_of_the_shell_gets_nothing_of_a_start(Taken::BeforeTheCommand);
}

#[test]
fn a_program_in_front_of_the_shell_before_the_enter_is_pressed_gets_no_enter() {
    a_program_in_front_of_the_shell_gets_nothing_of_a_start(Taken::BeforeTheEnter);
}

/// A `tmux` that, in the guarded call carrying the second piece of a long text, first takes the pane from its
/// shell (`respawn-pane` running `cat`) and waits until `cat` is in front: the first piece has reached the
/// shell's line by then, and the second is held back. The text goes in guarded calls,
/// `if-shell -F -t <pane> <condition> 'send-keys -l -t <pane> -- <text>' <else>`: its pane is the seventh
/// word, after `-L <socket> -u if-shell -F -t`, and its send the ninth.
fn tmux_that_takes_the_pane_after_the_first_piece(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-takes-the-pane");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             if [ \"$4\" = if-shell ]; then\n\
             \x20 case \"$9\" in\n\
             \x20   'send-keys -l '*)\n\
             \x20     echo piece >> '{dir}/pieces'\n\
             \x20     if [ \"$(wc -l < '{dir}/pieces')\" -ge 2 ]; then\n\
             \x20       \"$real\" -L \"$2\" respawn-pane -k -t \"$7\" cat\n\
             \x20       i=0\n\
             \x20       while [ $i -lt 200 ] && [ \"$(\"$real\" -L \"$2\" display -p -t \"$7\" '#{{pane_current_command}}')\" != cat ]; do\n\
             \x20         sleep 0.02; i=$((i + 1))\n\
             \x20       done\n\
             \x20     fi ;;\n\
             \x20 esac\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            dir = dir.display(),
        ),
    );
    wrapper
}

/// A command longer than one piece of a guarded send (4 KiB) is typed a piece at a time, and a program that
/// comes in front of the shell between the pieces is given the rest of none of it: the start refuses and
/// says, as it does of a program that came before the Enter, that what was typed may be left on its line,
/// which the first piece is. "So nothing was typed" would be untrue.
#[test]
fn a_program_in_front_of_the_shell_after_part_of_a_long_command_is_said_to_leave_it_on_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let stand_in = tmux_that_takes_the_pane_after_the_first_piece(tmp.path());
    let long = "a".repeat(5000);

    let (code, envelope) = server.taw_with_env(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            &pane,
            "--timeout",
            "20000",
            "--",
            &long,
        ],
        &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
    );

    assert_eq!(code, 1, "{envelope}");
    let pieces = std::fs::read_to_string(tmp.path().join("pieces")).unwrap_or_default();
    assert_eq!(
        pieces.lines().count(),
        2,
        "the stand-in did not see exactly two pieces sent: {pieces:?}"
    );
    let said = format!(
        "pane {pane} had another program in front of its shell when the command was to be run, and the \
         command typed into it may be left on its line; clear the line and start the agent again once the \
         shell is at its prompt."
    );
    assert_eq!(
        error_of(&envelope),
        ("shell_not_ready", said.as_str()),
        "{envelope}"
    );
    for option in ["@agent_acquiring", "@agent_typed_line", "@agent_kind"] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
}

/// The command is typed, and the line it is typed on written, as words of a tmux command, and tmux takes a
/// `;` at the end of one for the end of the command and drops it. A command that ends in `;` was typed
/// without it, and its line stored without it: with the one fixed and not the other, the row the command is
/// on does not start as the stored line does, the watcher misses the line twice, takes it for gone and reads
/// the frame the last agent left above it. Both are written as tmux reads them back whole.
#[test]
fn a_command_that_ends_in_a_semicolon_is_typed_and_its_line_stored_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = common::named_agent(tmp.path(), "claude", None);
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) = server.taw(&[
        "agent", "start", "rev", "--kind", "claude", "--pane", &pane, "--", "--x;",
    ]);

    assert_eq!(code, 0, "{envelope}");
    let screen = server.capture(&pane);
    server.wake("taw-fake-claude-never");
    assert!(
        screen
            .lines()
            .any(|row| row.trim_end().ends_with("$ claude --x;")),
        "the command was not typed whole:\n{screen}"
    );
    let line = server.pane_opt(&pane, "@agent_typed_line");
    assert!(
        line.contains(" 11 claude --x; ") && line.ends_with("$ claude --x;"),
        "the line was not stored as the verb wrote it: {line:?}"
    );
}

/// What the last agent left on the pane is taken off in one tmux invocation, the record, the pid and
/// the profile mark of an agent that ran in a sandbox together: a watcher that reads the pane between
/// two of fifteen calls can write a kind or a state back, and the next agent then has no grace, and a
/// mark left behind would have the agent this starts taken for a sandboxed one. A `tmux` that logs
/// what it is asked says how many calls take the state off, and what else goes in the same one.
#[test]
fn what_the_last_agent_left_is_taken_off_the_pane_in_one_tmux_call() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("tmux-calls.txt");
    let wrapper = tmp.path().join("tmux-logging");
    write_script(
        &wrapper,
        &format!(
            "{{ printf '%s' \"$*\" | tr '\\n\\t\\r' '   '; echo; }} >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            taw::tmux::Tmux::with_socket_name("unused").bin.display()
        ),
    );
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    report_a_claude_session(&server, &pane);
    server.set_pane_opt(&pane, "@agent_pid", "999999");
    server.set_pane_opt(&pane, "@agent_sandbox", "default");
    server.set_pane_opt(&pane, "@agent_sandbox_pid", "999998");
    server.set_pane_opt(&pane, "@agent_sandbox_resume", "{\"kind\":\"claude\"}");
    std::fs::write(&log, "").unwrap();

    let (code, envelope) = server.taw_with_env(
        &["agent", "start", "rev", "--kind", "claude", "--pane", &pane],
        &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
    );

    assert_eq!(code, 1, "{envelope}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let names = |call: &str, word: &str| call.split_whitespace().any(|w| w == word);
    let takes_the_state_off: Vec<&str> = calls
        .lines()
        .filter(|call| {
            names(call, "set-option") && names(call, "-u") && names(call, "@agent_state")
        })
        .collect();
    assert_eq!(
        takes_the_state_off.len(),
        1,
        "the state was taken off by {} calls:\n{calls}",
        takes_the_state_off.len()
    );
    for option in [
        "@agent_kind",
        "@agent_reported_kind",
        "@agent_source",
        "@agent_session_id",
        "@agent_pid",
        "@agent_sandbox",
        "@agent_sandbox_pid",
        "@agent_sandbox_resume",
    ] {
        assert!(
            names(takes_the_state_off[0], option),
            "the call that takes the state off leaves {option} to another: {}",
            takes_the_state_off[0]
        );
    }
    for option in [
        "@agent_sandbox",
        "@agent_sandbox_pid",
        "@agent_sandbox_resume",
    ] {
        assert_eq!(
            server.pane_opt(&pane, option),
            "",
            "the start leaves {option}"
        );
    }
}

#[test]
fn agent_start_clears_the_hint_it_set_when_the_pane_had_none() {
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);

    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_started", "{envelope}");
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind_hint"),
        "",
        "a pane that holds no agent is not marked as one"
    );
}

#[test]
fn a_watcher_that_took_the_hint_for_an_agent_lets_go_of_it_when_the_start_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (began, go) = (tmp.path().join("began"), tmp.path().join("go"));
    // The command is found, and fails once the test says so: until then the shell works in
    // builtins with the hint standing, which is long enough for the watcher to take it for an
    // agent. (A command that failed at once would have the hint gone before any watcher looked,
    // and the test would say nothing of what the watcher does with it.)
    let rc = tmp.path().join("rc");
    std::fs::write(
        &rc,
        format!(
            "claude() {{\n  : > '{}'\n  while [ ! -e '{}' ] && [ -d '{}' ] && [ \"${{SECONDS:-0}}\" -lt 300 ]; do :; done\n  echo 'claude: cannot start'\n  return 1\n}}\n",
            began.display(),
            go.display(),
            tmp.path().display()
        ),
    )
    .unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_whose_shell_reads(&server, &rc);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    attach_watcher(&server, &pane);

    let (code, envelope) = std::thread::scope(|scope| {
        let go = OpensTheGate(go.clone());
        let starting = scope
            .spawn(|| server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]));
        assert!(
            wait_for(|| began.exists(), SETTLE),
            "the function never ran:\n{}",
            server.capture(&pane)
        );
        // The watcher has taken the hint for an agent that nothing has said is there: a Claude it
        // knows nothing of (`unknown`).
        assert!(
            wait_for(
                || server.pane_opt(&pane, "@agent_kind") == "claude"
                    && server.pane_opt(&pane, "@agent_state") == "unknown",
                SETTLE
            ),
            "the watcher never took the hint for an agent: {:?} {:?}",
            server.pane_opt(&pane, "@agent_kind"),
            server.pane_opt(&pane, "@agent_state")
        );
        go.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_started", "{envelope}");
    // With the hint gone and the shell at its prompt the watcher has nothing to go on, and the
    // record goes at its next look, which is no more than its recheck of a quiet pane away.
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_kind").is_empty()
                && server.pane_opt(&pane, "@agent_state").is_empty(),
            SETTLE
        ),
        "the watcher still holds an agent in a pane whose agent never started: {:?} {:?}",
        server.pane_opt(&pane, "@agent_kind"),
        server.pane_opt(&pane, "@agent_state")
    );
}

#[test]
fn a_shell_that_has_not_taken_the_line_yet_is_not_an_agent_that_failed_to_start() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    // A program called `sh` that draws a prompt and reads nothing: alone in front of its terminal,
    // a shell by its name, and drawn, so the wait for the shell is satisfied, and what is typed to
    // it sits in the terminal's queue with nothing echoed. The screen is the one that was there
    // before the line was typed, exactly as it is for a real shell that has not got to the line
    // yet (which is too quick to catch at it); the same screen with a prompt on it, once the line
    // has been run and found wanting, is the shell back.
    let copy = tmux_named(tmp.path(), "sh");
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "printf 'deaf$ '; stty -echo; exec '{}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-deaf-shell-never",
            copy.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains("deaf$"), SETTLE),
        "the shell never drew its prompt:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "3000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        error_of(&envelope).0,
        "timeout",
        "a shell that has done nothing with the line is not the agent not starting: {envelope}"
    );
    server.wake("taw-deaf-shell-never");
}

#[test]
fn a_shell_function_that_works_a_while_before_it_runs_the_agent_is_not_a_start_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmux_named(tmp.path(), "claude");
    let (began, go) = (tmp.path().join("began"), tmp.path().join("go"));
    // What a version manager's loader does: `claude` is a function of the shell, which works in
    // builtins for a while, with nothing drawn and no process of its own, and then runs the agent.
    // The process table says the shell is alone in front of its terminal for all of that, and the
    // screen has the line on it and the cursor at the left edge of the next one. The work is a
    // loop that ends when this test says so, so that how long it takes is not a matter of how
    // fast the shell counts.
    let rc = tmp.path().join("rc");
    std::fs::write(
        &rc,
        format!(
            "claude() {{\n  : > '{}'\n  while [ ! -e '{}' ] && [ -d '{}' ] && [ \"${{SECONDS:-0}}\" -lt 300 ]; do :; done\n  '{}' -L \
             \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-slow-claude-never\n}}\n",
            began.display(),
            go.display(),
            tmp.path().display(),
            agent.display()
        ),
    )
    .unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("export ENV='{}'; exec /bin/sh", rc.display()),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope
            .spawn(|| server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]));
        assert!(
            wait_for(|| began.exists(), SETTLE),
            "the function never ran:\n{}",
            server.capture(&pane)
        );
        // The shell has been busy, and nothing but busy, for longer than a shell that was back
        // at its prompt would take to be given up on (300 ms of a screen held still, looked at
        // every 250 ms). Slowness only makes this longer, never wrong.
        std::thread::sleep(Duration::from_millis(2000));
        assert!(
            !starting.is_finished(),
            "agent start gave up on a shell that was working through the line:\n{}",
            server.capture(&pane)
        );
        std::fs::write(&go, "").unwrap();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(envelope["result"]["agent"]["agent"], "claude", "{envelope}");
    assert_eq!(server.display(&pane, "#{pane_current_command}"), "claude");
    server.wake("taw-slow-claude-never");
}

/// A pane whose shell, a `/bin/sh` with no `claude` to find and a `PATH` of the system's
/// directories only, reads `rc` first (what `ENV` names), and then draws its prompt.
fn pane_whose_shell_reads(server: &Server, rc: &Path) -> String {
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("exec env PATH=/usr/bin:/bin ENV='{}' /bin/sh", rc.display()),
    ])
}

/// What the docs say of the shells the verb cannot tell: a prompt that starts at the left edge of
/// the screen is a shell that has not drawn one, so the failed command is never seen to have come
/// back to it, and the verb ends in `timeout`.
#[test]
fn a_prompt_at_the_left_edge_of_the_screen_ends_in_timeout_and_not_in_a_start_that_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let rc = tmp.path().join("rc");
    std::fs::write(&rc, "echo drawn\nPS1=''\n").unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_whose_shell_reads(&server, &rc);
    assert!(
        wait_for(|| server.capture(&pane).contains("drawn"), SETTLE),
        "the shell never read its startup file:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "3000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "timeout", "{envelope}");
    assert!(
        server.capture(&pane).contains("not found"),
        "the command was never typed:\n{}",
        server.capture(&pane)
    );
}

/// The other: a question the shell asks of its own after the command was typed (a function that
/// reads an answer) is a shell alone in front of its terminal with a line drawn and nothing
/// running, which is what the shell back at its prompt looks like too. The verb says the agent
/// did not start, as the docs say it takes the question for the prompt.
#[test]
fn a_question_the_shell_asks_after_the_command_is_taken_for_its_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let rc = tmp.path().join("rc");
    std::fs::write(
        &rc,
        "claude() {\n  printf 'Update first? '\n  read answer\n}\n",
    )
    .unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_whose_shell_reads(&server, &rc);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_started", "{envelope}");
    assert!(
        server.capture(&pane).contains("Update first?"),
        "the question never came up:\n{}",
        server.capture(&pane)
    );
}

#[test]
fn agent_start_fails_at_once_when_the_agent_exits_the_moment_it_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("claude"),
        "echo 'claude: unknown option' >&2\nexit 2\n",
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "agent_not_started", "{envelope}");
    assert!(message.contains("did not start"), "{message}");
    assert!(
        server.capture(&pane).contains("unknown option"),
        "the agent never ran:\n{}",
        server.capture(&pane)
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "");
}

#[test]
fn agent_start_returns_once_the_process_in_front_of_the_pane_is_named_for_the_agent() {
    let tmp = tempfile::tempdir().unwrap();
    // The server's socket is not known until it is started, and the pane's `PATH` has to name
    // the directory of the fake before that: it is made in a directory known beforehand, and
    // the arguments are read off the server afterwards.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tmux_named(&bin, "claude");
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    let socket = server.socket().to_string();

    // No watcher is attached: the process in front names the agent, and that is enough.
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
        "taw-fake-claude-never",
    ]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(envelope["result"]["agent"]["agent"], "claude", "{envelope}");
    assert_eq!(envelope["result"]["agent"]["name"], "rev", "{envelope}");
    assert_eq!(server.display(&pane, "#{pane_current_command}"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "rev");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "claude");
    // The agent it started is being taken (herdr's managed launch): its first idle, a trust
    // question answered, is no finished turn. With no watcher, only the verb can have said so.
    assert_eq!(server.pane_opt(&pane, "@agent_acquiring"), "taken");
    server.wake("taw-fake-claude-never");
}

/// Names are unique among live agents: `agent rename` refuses one that another pane's agent has,
/// and SKILL.md tells an agent so. `agent start` named an agent whatever the others were called, so
/// two panes could answer to one name, and a verb given the name acted on whichever it found first.
/// It refuses before it waits for the shell or types anything.
#[test]
fn agent_start_refuses_a_name_that_a_live_agent_already_has_and_types_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tmux_named(&bin, "claude");
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let first = server.pane();
    let socket = server.socket().to_string();
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &first,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-never",
    ]);
    assert_eq!(code, 0, "{envelope}");
    let first_public = server.public_pane_id(&first);
    let second = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
    assert!(
        wait_for(|| server.capture(&second).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&second)
    );

    let (code, envelope) = server.taw(&[
        "agent", "start", "rev", "--kind", "claude", "--pane", &second,
    ]);

    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "invalid_argument", "{envelope}");
    assert!(
        message.contains("'rev'") && message.contains(&first_public),
        "the message must name the name and the pane that has it: {message}"
    );
    assert!(
        !server.capture(&second).contains("claude"),
        "the command was typed:\n{}",
        server.capture(&second)
    );
    assert_eq!(server.pane_opt(&second, "@agent_kind_hint"), "");
    assert_eq!(server.pane_opt(&second, "@agent_name"), "");
    // The first agent keeps its name.
    assert_eq!(server.pane_opt(&first, "@agent_name"), "rev");
    server.wake("taw-fake-claude-never");
}

/// Without a watcher nobody takes the record off the pane of an agent that has exited, so its name
/// stays on the pane, and must not be held against a start in another pane for as long as the pane
/// lives. A pane whose own shell is at its prompt with nothing running in it holds no live agent,
/// whatever it records, and the name then finds the agent started with it.
#[test]
fn a_name_the_exited_agent_of_another_pane_left_behind_can_be_used_again() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    tmux_named(&bin, "claude");
    // No history file and a home of its own: the shells here are typed into.
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        env: vec![
            ("HISTFILE".to_string(), "/dev/null".to_string()),
            ("HOME".to_string(), home.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let first = server.pane();
    let socket = server.socket().to_string();
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &first,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-first",
    ]);
    assert_eq!(code, 0, "{envelope}");

    // The agent ends; the shell is back, and the record, with no watcher, stays.
    server.wake("taw-fake-claude-first");
    assert!(
        wait_for(
            || server.display(&first, "#{pane_current_command}") != "claude",
            SETTLE
        ),
        "the agent never ended"
    );
    assert_eq!(server.pane_opt(&first, "@agent_name"), "rev");
    assert_eq!(server.pane_opt(&first, "@agent_kind"), "claude");

    let second = server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"]);
    assert!(
        wait_for(|| server.capture(&second).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&second)
    );
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &second,
        "--",
        "-L",
        &socket,
        "wait-for",
        "taw-fake-claude-second",
    ]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(server.pane_opt(&second, "@agent_name"), "rev");

    // Both panes carry the name now, the one listed first only on its stale record: the name
    // finds the agent that holds it.
    assert!(
        wait_for(
            || server.display(&second, "#{pane_current_command}") == "claude",
            SETTLE
        ),
        "the second agent never came up"
    );
    let got = server.ok(&["agent", "get", "rev"], "cli:agent:get", "agent");
    assert_eq!(
        got["agent"]["pane_id"],
        server.public_pane_id(&second).as_str(),
        "{got}"
    );
    server.wake("taw-fake-claude-second");
}

#[test]
fn an_agent_no_process_name_gives_away_is_not_started_until_the_watcher_has_read_it() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(
            || server
                .pane_opt(&pane, "@agent_watch_pid")
                .parse::<u32>()
                .is_ok(),
            SETTLE
        ),
        "the watcher never recorded its pid"
    );

    // The fake is a script, so tmux names its interpreter and not Claude: the watcher is the one
    // that can say the agent is there, from its screen, after the 3 seconds a new agent is given.
    let began = Instant::now();
    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);
    let took = began.elapsed();

    assert_eq!(code, 0, "{envelope}");
    assert_ne!(
        server.display(&pane, "#{pane_current_command}"),
        "claude",
        "the fake must read to tmux as an interpreter, or this proves nothing"
    );
    let status = envelope["result"]["agent"]["agent_status"].as_str();
    assert!(
        matches!(status, Some("idle" | "working" | "blocked" | "done")),
        "the verb returned before the watcher had read a state for the agent: {envelope}"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "manifest");
    assert!(
        took >= Duration::from_secs(3),
        "a new agent is `unknown` for 3 seconds and the verb waited only {took:?}"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "rev");
}

#[test]
fn an_agent_nobody_has_read_runs_the_budget_out_and_the_pane_keeps_its_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    // No watcher is attached, and the process in front of the pane is the fake's interpreter:
    // nothing says the agent is there, and the verb says so as the budget runs out. It is
    // `timeout` and not "did not start": the fake is running, and may well be an agent.
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "3000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "timeout", "{envelope}");
    assert!(message.contains("agent 'claude'"), "{message}");
    // The fake is running all the while: only nobody has said so.
    fake.wait_until_drawn(&server, &pane, SETTLE);
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind_hint"),
        "claude",
        "the agent may be coming up: the hint stays"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "");
}

#[test]
fn an_agent_that_reports_itself_is_started_without_a_watcher_or_a_name_to_read_it_by() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let started = tmp.path().join("started");
    // A program no name gives away that stays: it says it has run, and sleeps.
    write_script(
        &bin.join("claude"),
        &format!(": > '{}'\nexec sleep 600\n", started.display()),
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();

    std::thread::scope(|scope| {
        let starting = scope
            .spawn(|| server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]));
        assert!(
            wait_for(|| started.exists(), SETTLE),
            "the fake never ran:\n{}",
            server.capture(&pane)
        );
        // It has run, and nothing has said it is an agent yet: the verb is still waiting. A
        // verb that was done by now took the typed command for the agent. Slowness only ever
        // makes this later, never wrong.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !starting.is_finished(),
            "agent start was done with nothing to say the agent was there"
        );

        // What Claude's own hook does when a session starts.
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
                "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42",
            ],
            "cli:pane:report-agent-session",
            "ok",
        );
        let (code, envelope) = starting.join().unwrap();
        assert_eq!(code, 0, "{envelope}");
        assert_eq!(envelope["result"]["agent"]["name"], "rev", "{envelope}");
    });
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "claude");
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "rev");
}

/// A program that asks a question before anything else, as mise does when a folder's config is
/// new to it, and a `claude` the shell can run once it is answered.
struct Asking {
    dir: tempfile::TempDir,
}

impl Asking {
    fn new() -> Asking {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        write_script(
            &bin.join("ask"),
            "printf 'Trust them? Yes No All '\nread answer\n",
        );
        write_script(&bin.join("claude"), "exec sleep 600\n");
        Asking { dir }
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// A new pane whose shell asks its question first and is a `/bin/sh` after it.
    fn pane(&self, server: &Server) -> String {
        server.tmux_ok(&[
            "set-option",
            "-g",
            "default-command",
            &format!(
                "export PATH={}:$PATH; '{}'; exec /bin/sh",
                self.bin().display(),
                self.bin().join("ask").display()
            ),
        ]);
        server.tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"])
    }
}

/// A `tmux` that notes the first time it is run and runs the real one: for a test that has to
/// know that a verb has begun.
fn tmux_that_notes_its_first_run(dir: &Path, marker: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-noting");
    write_script(
        &wrapper,
        &format!(
            "[ -e '{}' ] || : > '{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            marker.display(),
            real.display()
        ),
    );
    wrapper
}

#[test]
fn the_wait_for_the_shell_is_taken_out_of_the_one_budget_the_agent_has_what_is_left() {
    let asking = Asking::new();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = asking.pane(&server);
    assert!(
        wait_for(
            || server.capture(&pane).contains("Trust them?"),
            Duration::from_secs(10)
        ),
        "the new pane never showed the question:\n{}",
        server.capture(&pane)
    );
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("verb-began");
    let tmux = tmux_that_notes_its_first_run(tmp.path(), &marker);

    let budget_ms: u64 = 7000;
    let shell_waits = Duration::from_secs(2);
    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    &budget_ms.to_string(),
                ],
                &[("TAW_TMUX_BIN", tmux.to_str().unwrap())],
            )
        });
        // The verb starts its clock before it asks tmux anything, so by the time tmux has been
        // asked, `shell_waits` more is a wait for the shell that is at least that long, whatever
        // the machine is doing: the shell cannot be at its prompt before it is answered.
        assert!(
            wait_for(|| marker.exists(), SETTLE),
            "the verb never asked tmux anything"
        );
        std::thread::sleep(shell_waits);
        assert!(!starting.is_finished(), "agent start did not wait");
        server.tmux_ok(&["send-keys", "-t", &pane, "y", "Enter"]);
        starting.join().unwrap()
    });

    // Nothing says the fake is an agent (no watcher, a `sleep` named by no one), so the budget
    // runs out, and what it says it ran out of is what the shell's wait left of it.
    assert_eq!(code, 1, "{envelope}");
    let (error_code, message) = error_of(&envelope);
    assert_eq!(error_code, "timeout", "{envelope}");
    let left_ms: u64 = message
        .strip_prefix("timed out after ")
        .and_then(|rest| rest.split("ms").next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("the timeout does not say how long it waited: {message}"));
    let most_left = budget_ms - u64::try_from(shell_waits.as_millis()).unwrap();
    assert!(
        left_ms <= most_left,
        "the agent had {left_ms} ms of a {budget_ms} ms budget after a wait for the shell of at \
         least {} ms: the two waits do not share the budget ({message})",
        shell_waits.as_millis()
    );
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

/// What Claude's own hook does when a session starts.
fn report_a_claude_session(server: &Server, pane: &str) {
    server.ok(
        &[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "integration:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
}

/// A Claude with taw's integration ran in the pane and has gone, and what it reported stays on the
/// pane: its hook reports only when a session starts, and only the watcher takes a record away.
/// The next `agent start --kind claude` there types `claude`, and what runs is something tmux does
/// not call Claude that dies (a failed install, a wrapper that cannot find what it wraps). What
/// the pane recorded before the command was typed is the last agent's, and says nothing of this
/// one: the verb has to say the agent did not start.
#[test]
fn what_the_last_agent_reported_is_not_this_agent_saying_it_started() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    write_script(
        &bin.join("claude"),
        "sleep 1\necho 'Error: Cannot find module' >&2\nexit 1\n",
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    report_a_claude_session(&server, &pane);
    assert_eq!(server.pane_opt(&pane, "@agent_reported_kind"), "claude");

    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_started", "{envelope}");
    assert!(
        server.capture(&pane).contains("Cannot find module"),
        "the command never ran:\n{}",
        server.capture(&pane)
    );
    // Nothing of the old record is left on a pane that holds no agent.
    for option in [
        "@agent_kind",
        "@agent_reported_kind",
        "@agent_source",
        "@agent_session_id",
        "@agent_name",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }
}

/// What the pane's last occupant left of its pid and its hint is what tells the watcher that the
/// agent is gone, so a start that fails puts both back with the hint, as it found them.
#[test]
fn a_failed_start_puts_back_the_pid_the_pane_had_as_well_as_its_hint() {
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_without_any_agent(&server);
    server.set_pane_opt(&pane, "@agent_kind_hint", "codex");
    // The pid of an agent that has gone: no process has a number this big.
    server.set_pane_opt(&pane, "@agent_pid", "999999");

    let (code, envelope) =
        server.taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "agent_not_started", "{envelope}");
    assert_eq!(server.pane_opt(&pane, "@agent_kind_hint"), "codex");
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), "999999");
    // No agent started, so none is being taken.
    assert_eq!(server.pane_opt(&pane, "@agent_acquiring"), "");
}

/// The pid `agent start` recorded for the last agent says that agent has gone, and would say it of
/// the next as well, so it is taken off the pane before the command is typed. A new agent that has
/// no process to find a pid for (a function of the shell, which works in builtins until it runs the
/// agent) leaves the pane with none, and not with the pid of the one that left.
#[test]
fn a_start_takes_the_last_agents_pid_off_the_pane_before_it_types() {
    let tmp = tempfile::tempdir().unwrap();
    let (began, go) = (tmp.path().join("began"), tmp.path().join("go"));
    let rc = tmp.path().join("rc");
    std::fs::write(
        &rc,
        format!(
            "claude() {{\n  : > '{}'\n  while [ ! -e '{}' ] && [ -d '{}' ] && [ \"${{SECONDS:-0}}\" -lt 300 ]; do :; done\n}}\n",
            began.display(),
            go.display(),
            tmp.path().display()
        ),
    )
    .unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = pane_whose_shell_reads(&server, &rc);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    server.set_pane_opt(&pane, "@agent_pid", "999999");

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "2500",
    ]);
    std::fs::write(&go, "").unwrap();

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "timeout", "{envelope}");
    assert!(began.exists(), "the function never ran");
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        "",
        "the pid of the agent that left was left for the next"
    );
}

/// The pid of the process in front of `pane`'s shell, as the process table has it.
fn child_of_the_shell(server: &Server, pane: &str) -> String {
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

/// The watcher tells an agent that has exited by the pid `agent start` recorded for it. A verb that
/// gives up while the agent is coming up (nothing has said yet that it is one) leaves that process
/// running, and its pid with it, or the watcher could never tell when it exits. What runs is a shell
/// function's `/bin/sleep`, which nothing names as the agent, so it is the pid the last two looks of the
/// wait agreed on that is left: a script called `claude` is named by its path for the moment before its
/// `exec`, and a look that caught it there recorded it as the wait went. The budget is 4 s: at a timeout
/// the last two looks of the wait decide alone, and on a loaded machine a 2 s budget often ran out
/// before two looks had found what came up, so nothing was left.
#[test]
fn a_start_that_runs_out_of_its_budget_leaves_the_pid_of_what_came_up() {
    let tmp = tempfile::tempdir().unwrap();
    let rc = tmp.path().join("rc.sh");
    std::fs::write(&rc, "claude() { /bin/sleep 600; }\n").unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH=/usr/bin:/bin ENV='{}' HOME='{}' HISTFILE=/dev/null /bin/sh",
            rc.display(),
            home.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "4000",
    ]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(error_of(&envelope).0, "timeout", "{envelope}");
    let running = child_of_the_shell(&server, &pane);
    assert!(!running.is_empty(), "nothing is running in the pane");
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        running,
        "the pid of what is running was not left for the watcher"
    );
}

/// Once the agent is there, `agent start` looks a while more for the process of an agent nothing names
/// (two looks have to agree on it), within what is left of `--timeout`, its one budget: looking for its
/// own 1.5 s whatever was left, a start that found its agent near the deadline ran past it. The agent is
/// a shell function whose children come and go faster than the verb looks, so no two looks agree, until
/// the test lets it run one that stays, once the budget has run out: a verb that kept looking past its
/// deadline recorded that one. The deadline is told from the verb's first tmux call, which comes after its
/// clock starts, so the child that stays comes after it however slow the verb was to start; the verb takes
/// no look after its deadline, and the one it may be taking then cannot agree with an earlier one. The
/// report that starts the agent comes 1 s before the deadline, so that 1.5 s run well past it.
#[test]
fn the_last_looks_for_the_pid_of_an_agent_nothing_names_end_with_the_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let (started, stable) = (tmp.path().join("started"), tmp.path().join("stable"));
    let runtime = tmux_named(tmp.path(), "agent-runtime");
    let rc = tmp.path().join("rc.sh");
    std::fs::write(
        &rc,
        format!(
            "claude() {{ : > '{}'; while [ ! -e '{}' ]; do /bin/sleep 0.01; done; '{}' -L \
             \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-runtime-never; }}\n",
            started.display(),
            stable.display(),
            runtime.display()
        ),
    )
    .unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH=/usr/bin:/bin ENV='{}' HOME='{}' HISTFILE=/dev/null /bin/sh",
            rc.display(),
            home.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let marker = tmp.path().join("verb-began");
    let noting = tmux_that_notes_its_first_run(tmp.path(), &marker);
    const BUDGET: Duration = Duration::from_millis(3000);

    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    &BUDGET.as_millis().to_string(),
                ],
                &[("TAW_TMUX_BIN", noting.to_str().unwrap())],
            )
        });
        let _stable = OpensTheGate(stable.clone());
        assert!(
            wait_for(|| marker.exists(), SETTLE),
            "the verb never asked tmux anything"
        );
        // The verb's clock started before its first tmux call, so its deadline is no later than this.
        let deadline = Instant::now() + BUDGET;
        assert!(
            wait_for(|| started.exists(), SETTLE),
            "the agent never ran:\n{}",
            server.capture(&pane)
        );
        let report_at = deadline - Duration::from_millis(1000);
        std::thread::sleep(report_at.saturating_duration_since(Instant::now()));
        report_a_claude_session(&server, &pane);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        std::fs::write(&stable, "").unwrap();
        starting.join().unwrap()
    });

    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the agent never ran what stays:\n{}",
        server.capture(&pane)
    );
    let stayed = child_of_the_shell(&server, &pane);
    let recorded = server.pane_opt(&pane, "@agent_pid");
    server.wake("taw-fake-runtime-never");
    assert_ne!(
        recorded, stayed,
        "the verb looked for the agent's pid past its budget ({code}: {envelope})"
    );
}

/// A `pgrep` first on `agent start`'s `$PATH` that, once `pane` holds `@agent_name` (the verb sets it once
/// the agent is there, right before its last looks for the agent's pid; a report of the agent's sets its
/// kind, and not its name), makes each look for the child in front of the pane (`pgrep -P`, which only
/// those looks run) as slow as a busy machine does, longer than the verb gives them all: it notes in
/// `looking` that it has begun, sleeps 2.5 s, and answers then, or with `until`, once a process of that
/// name is in front. Every other `pgrep`, and every look before, runs as it is. The returned directory
/// goes first on the verb's `$PATH`.
fn pgrep_that_looks_slowly_once_the_agent_is_there(
    dir: &Path,
    pane: &str,
    looking: &Path,
    until: Option<&str>,
) -> PathBuf {
    let bin = dir.join("pgrep-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let wait = until.map_or_else(String::new, |name| {
        format!(
            "\x20 i=0\n\
             \x20 while [ \"$i\" -lt 200 ]; do\n\
             \x20   for child in $(/usr/bin/pgrep -P \"$2\"); do\n\
             \x20     case \"$(/bin/ps -o comm= -p \"$child\")\" in *{name}) break 2 ;; esac\n\
             \x20   done\n\
             \x20   /bin/sleep 0.05; i=$((i + 1))\n\
             \x20 done\n"
        )
    });
    write_script(
        &bin.join("pgrep"),
        &format!(
            "[ \"$1\" = -P ] || exec /usr/bin/pgrep \"$@\"\n\
             if [ -n \"$('{tmux}' -L \"$TAW_TMUX_SOCKET_NAME\" show-options -pqv -t '{pane}' @agent_name)\" ]; then\n\
             \x20 : > '{looking}'\n\
             \x20 /bin/sleep 2.5\n\
             {wait}\
             fi\n\
             exec /usr/bin/pgrep \"$@\"\n",
            tmux = taw::tmux::Tmux::with_socket_name("unused").bin.display(),
            looking = looking.display()
        ),
    );
    bin
}

/// On a machine so busy that a look for the child in front of the pane takes longer than the 1.5 s
/// `agent start` gives its last looks for the pid of an agent nothing names, the verb ended them at that
/// bound after one, which found the agent while the look before it, in the wait, had found a child the
/// agent's shell function ran on its way; the two disagree, and it recorded nothing. It makes three looks
/// at the least, two of which find the agent, and records it. The agent is a shell function whose children
/// come and go until the test lets it run one that stays, once the verb has begun its slow looks; the
/// test's `pgrep` makes them slow ([`pgrep_that_looks_slowly_once_the_agent_is_there`]).
#[test]
fn an_agent_nothing_names_is_recorded_however_slow_the_last_looks_for_its_pid_are() {
    let tmp = tempfile::tempdir().unwrap();
    let (started, stable) = (tmp.path().join("started"), tmp.path().join("stable"));
    let runtime = tmux_named(tmp.path(), "agent-runtime");
    let rc = tmp.path().join("rc.sh");
    std::fs::write(
        &rc,
        format!(
            "claude() {{ : > '{}'; while [ ! -e '{}' ]; do /bin/sleep 0.01; done; '{}' -L \
             \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-runtime-never; }}\n",
            started.display(),
            stable.display(),
            runtime.display()
        ),
    )
    .unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH=/usr/bin:/bin ENV='{}' HOME='{}' HISTFILE=/dev/null /bin/sh",
            rc.display(),
            home.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let looking = tmp.path().join("looking-slowly");
    let pgrep = pgrep_that_looks_slowly_once_the_agent_is_there(
        tmp.path(),
        &pane,
        &looking,
        Some("agent-runtime"),
    );
    let path = format!(
        "{}:{}",
        pgrep.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            server.taw_with_env(
                &["agent", "start", "rev", "--kind", "claude", "--pane", &pane],
                &[("PATH", &path)],
            )
        });
        let _stable = OpensTheGate(stable.clone());
        assert!(
            wait_for(|| started.exists(), SETTLE),
            "the agent never ran:\n{}",
            server.capture(&pane)
        );
        report_a_claude_session(&server, &pane);
        assert!(
            wait_for(|| looking.exists(), SETTLE),
            "the verb never looked for the agent's pid once the agent was there"
        );
        std::fs::write(&stable, "").unwrap();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "agent-runtime",
            SETTLE
        ),
        "the agent never ran what stays:\n{}",
        server.capture(&pane)
    );
    let stayed = child_of_the_shell(&server, &pane);
    let recorded = server.pane_opt(&pane, "@agent_pid");
    server.wake("taw-fake-runtime-never");
    assert_eq!(
        recorded, stayed,
        "the verb recorded no pid for the agent, or another's"
    );
}

/// With slow looks and little of `--timeout` left once the agent is there, `agent start` begins no look for
/// the pid of an agent nothing names once that has run out, and returns within it but for the one look it
/// may be in then (a loose bound, under any load): its floor and its cap of looks, six of 2.5 s, ran it
/// some 11 s past. Its last two looks disagree, so it records nothing, the answer when it cannot tell: the
/// agent's shell function always has a child in front, a new one every 0.2 s, shorter than the 250 ms
/// between two looks of the wait, so that no two looks ever find the same one, and slow looks run on to
/// the cap. The deadline is told from the verb's first tmux call, which comes up to a second or more after
/// its clock starts under load, and the agent is reported 4 s before it, room enough for the verb's poll
/// and its calls to bring it to its slow looks with time left.
#[test]
fn agent_start_with_slow_last_looks_ends_within_its_budget_and_records_nothing_it_cannot_tell() {
    let tmp = tempfile::tempdir().unwrap();
    let started = tmp.path().join("started");
    let rc = tmp.path().join("rc.sh");
    std::fs::write(
        &rc,
        format!(
            "claude() {{ : > '{}'; while :; do /bin/sleep 0.2; done; }}\n",
            started.display()
        ),
    )
    .unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH=/usr/bin:/bin ENV='{}' HOME='{}' HISTFILE=/dev/null /bin/sh",
            rc.display(),
            home.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let looking = tmp.path().join("looking-slowly");
    let pgrep = pgrep_that_looks_slowly_once_the_agent_is_there(tmp.path(), &pane, &looking, None);
    let path = format!(
        "{}:{}",
        pgrep.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let marker = tmp.path().join("verb-began");
    let noting = tmux_that_notes_its_first_run(tmp.path(), &marker);
    const BUDGET: Duration = Duration::from_millis(8000);

    let (code, envelope, returned, deadline) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            let (code, envelope) = server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    &BUDGET.as_millis().to_string(),
                ],
                &[("TAW_TMUX_BIN", noting.to_str().unwrap()), ("PATH", &path)],
            );
            (code, envelope, Instant::now())
        });
        assert!(
            wait_for(|| marker.exists(), SETTLE),
            "the verb never asked tmux anything"
        );
        // The verb's clock started before its first tmux call, so its deadline is no later than this.
        let deadline = Instant::now() + BUDGET;
        assert!(
            wait_for(|| started.exists(), SETTLE),
            "the agent never ran:\n{}",
            server.capture(&pane)
        );
        let report_at = deadline - Duration::from_millis(4000);
        std::thread::sleep(report_at.saturating_duration_since(Instant::now()));
        report_a_claude_session(&server, &pane);
        let (code, envelope, returned) = starting.join().unwrap();
        (code, envelope, returned, deadline)
    });

    assert_eq!(code, 0, "{envelope}");
    assert!(looking.exists(), "the verb began no slow look for the pid");
    assert!(
        returned <= deadline + Duration::from_secs(4),
        "the verb ran {:?} past its budget",
        returned.saturating_duration_since(deadline)
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_pid"),
        "",
        "a pid was recorded for an agent whose last looks disagreed"
    );
}

/// A start in a pane whose shell runs a helper before every command, as the preexec hooks of atuin,
/// starship and bash-preexec do (here a DEBUG trap whose helper adds its pid to a file and becomes
/// `/bin/sleep 0.6`), with a scratch home and no history file. The helper is the first thing in front of
/// the pane once the command is entered, and is gone a moment later: the verb recorded its pid as the
/// agent's, and the watcher took that pid, dead, for the agent's exit, at its first read of the new agent.
/// The verb records a pid only for a child of a job in front that names the agent, or, for an agent
/// nothing names (`named` false: a shell function `claude` that runs a program called `agent-runtime`,
/// which its own report says is Claude), the child the last two looks of the wait both found: every pid
/// it writes is the agent's, never a helper's.
fn a_start_after_a_helper_records_only_the_agent(named: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let started = tmp.path().join("started");
    let helper_pids = tmp.path().join("helper.pids");
    let mut rc = format!(
        "trap 'sh -c \"echo \\$\\$ >> {}; exec /bin/sleep 0.6\"' DEBUG\n",
        helper_pids.display()
    );
    let bin = if named {
        common::named_agent(tmp.path(), "claude", None)
    } else {
        // A function, so that no process the shell runs for `claude` is ever called that, not even for
        // the moment before a script's `exec`.
        let runtime = tmux_named(tmp.path(), "agent-runtime");
        rc.push_str(&format!(
            "claude() {{ : > '{}'; '{}' -L \"$TAW_TMUX_SOCKET_NAME\" wait-for taw-fake-runtime-never; }}\n",
            started.display(),
            runtime.display()
        ));
        tmp.path().join("bin")
    };
    let rc_file = tmp.path().join("rc.sh");
    std::fs::write(&rc_file, rc).unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH='{}':/usr/bin:/bin ENV='{}' HOME='{}' HISTFILE=/dev/null /bin/sh",
            bin.display(),
            rc_file.display(),
            home.display()
        ),
    ]);
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let (logging, log) = tmux_that_logs(tmp.path());

    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "rev",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    "20000",
                ],
                &[("TAW_TMUX_BIN", logging.to_str().unwrap())],
            )
        });
        if !named {
            assert!(
                wait_for(|| started.exists(), SETTLE),
                "the agent never ran:\n{}",
                server.capture(&pane)
            );
            report_a_claude_session(&server, &pane);
        }
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    let helpers = std::fs::read_to_string(&helper_pids).unwrap_or_default();
    let helpers: Vec<&str> = helpers.split_whitespace().collect();
    assert!(
        !helpers.is_empty(),
        "the shell ran no helper before the agent"
    );
    let agent = child_of_the_shell(&server, &pane);
    assert!(
        !helpers.contains(&agent.as_str()),
        "a helper is still running"
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let written: Vec<&str> = calls
        .lines()
        .filter_map(|call| {
            let words: Vec<&str> = call.split_whitespace().collect();
            words
                .windows(2)
                .find(|pair| pair[0] == "@agent_pid" && pair[1].parse::<u32>().is_ok())
                .map(|pair| pair[1])
        })
        .collect();
    assert!(
        written.iter().all(|pid| !helpers.contains(pid)),
        "the pid of a helper ({helpers:?}) was recorded for the agent ({agent}): {written:?}"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_pid"), agent);
    server.wake(if named {
        "taw-fake-claude-never"
    } else {
        "taw-fake-runtime-never"
    });
}

#[test]
fn a_helper_the_shell_runs_before_an_agent_its_job_names_is_never_recorded_for_it() {
    a_start_after_a_helper_records_only_the_agent(true);
}

#[test]
fn a_helper_the_shell_runs_before_an_agent_nothing_names_is_never_recorded_for_it() {
    a_start_after_a_helper_records_only_the_agent(false);
}

/// The agent dies and is started again in its pane, as a person does when it has crashed. Its pid
/// stood in the pane's record for the agent that was there, so the watcher took the new one for the
/// old and went on saying `unknown` for the agent that was running: the verb timed out.
#[test]
fn a_second_start_in_the_pane_of_an_agent_that_died_is_read_by_the_watcher_again() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    attach_watcher(&server, &pane);

    let (code, envelope) = server.taw(&[
        "agent", "start", "first", "--kind", "claude", "--pane", &pane,
    ]);
    assert_eq!(code, 0, "{envelope}");
    let first = fake.pid().expect("the first fake recorded no pid");
    fake.signal("-TERM");
    // The hint outlives the agent, so the watcher says `unknown` for it and clears nothing.
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never noticed the agent had gone: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );

    // A budget well under the default: a verb that cannot read the agent should say so soon.
    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "second",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "15000",
    ]);

    assert_eq!(code, 0, "{envelope}");
    assert_ne!(
        fake.pid().as_deref(),
        Some(first.as_str()),
        "the second fake never ran"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_name"), "second");
    let status = envelope["result"]["agent"]["agent_status"].as_str();
    assert!(
        matches!(status, Some("idle" | "working" | "blocked" | "done")),
        "the verb returned before the watcher had read a state for the new agent: {envelope}"
    );
}

/// A pane whose agent has died: the fake is started with `agent start`, left in the state `signal`
/// puts it in (`SIGUSR1` working, `SIGUSR2` blocked, nothing for idle) and killed, and the watcher
/// has said `unknown` for it, as it does for an agent that has gone while its hint stays.
///
/// A blocked agent earns a request, which the watcher delivers on a thread of its own a moment after
/// it reads the state, so it can still be on its way when the pane has gone `unknown`. A test that
/// counts what a person is told from here on passes the `recorder`: the agent is not killed until its
/// request has been delivered, and the count that test takes next is a settled one.
fn a_pane_whose_agent_died(
    server: &Server,
    fake: &FakeAgent,
    signal: Option<(&str, &str)>,
    recorder: Option<&NotifyRecorder>,
) -> String {
    let pane = server.pane();
    attach_watcher(server, &pane);
    let (code, envelope) = server.taw(&[
        "agent", "start", "first", "--kind", "claude", "--pane", &pane,
    ]);
    assert_eq!(code, 0, "{envelope}");
    if let Some((sig, state)) = signal {
        fake.signal(sig);
        assert!(
            wait_for(|| server.pane_opt(&pane, "@agent_state") == state, SETTLE),
            "the watcher never read the fake as {state}: {:?}",
            server.pane_opt(&pane, "@agent_state")
        );
        if let (Some(recorder), "blocked") = (recorder, state) {
            assert!(
                wait_for(|| recorder.matching("blocked").len() == 1, SETTLE),
                "the request the blocked agent earned was never delivered: {:?}",
                recorder.lines()
            );
        }
    }
    fake.signal("-TERM");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never noticed the agent had gone: {:?}",
        server.pane_opt(&pane, "@agent_state")
    );
    pane
}

/// A `claude` that has drawn nothing: it says it has run in `waiting`, waits at `gate`, and then
/// draws as the fake agent does.
fn a_claude_that_waits_at_a_gate(fake: &FakeAgent, dir: &Path) -> (PathBuf, PathBuf) {
    let (waiting, gate) = (dir.join("waiting"), dir.join("gate"));
    write_script(
        &fake.bin_dir.join("claude"),
        &format!(
            ": > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\nexec '{}'\n",
            waiting.display(),
            gate.display(),
            common::fake_agent_fixture().display()
        ),
    );
    (waiting, gate)
}

/// Opens `gate` when it goes out of scope, so that a test that fails while a verb waits at the
/// gate does not leave the verb to run out its budget before the failure is reported.
struct OpensTheGate(PathBuf);

impl OpensTheGate {
    fn open(&self) {
        std::fs::write(&self.0, "").unwrap();
    }
}

impl Drop for OpensTheGate {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

/// Notifications delivered through `$TAW_NOTIFY_CMD`, a request at once.
const NOTIFYING: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"system\"
delay_seconds = 0
done_delay_seconds = 2

[ui.sound]
enabled = false
";

/// A server for a second start in a pane whose agent died: the fake agent first on its shells' `$PATH`
/// and `height` rows to a pane, notifications recorded, every decision of a watcher logged (so that a
/// test can tell a read made after a new agent's grace), and a home of the shells' own.
fn server_for_a_second_start(tmp: &Path, height: u32) -> (Server, FakeAgent, NotifyRecorder) {
    let fake = FakeAgent::new(tmp);
    let recorder = NotifyRecorder::new(tmp);
    let home = tmp.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        height,
        path_prefix: Some(fake.bin_dir.clone()),
        env: vec![
            ("HOME".to_string(), home.display().to_string()),
            (
                "TAW_NOTIFY_CMD".to_string(),
                recorder.script.display().to_string(),
            ),
            ("TAW_LOG".to_string(), "debug".to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(NOTIFYING);
    (server, fake, recorder)
}

/// How long a new agent reads `unknown` before its screen is believed: 3 s, and one more tick.
const GRACE_AND_A_TICK: Duration = Duration::from_millis(3600);

/// Waits until the watcher of `pane` has decided on a read made after a new agent's grace: two reads more
/// than `reads` (the read at the grace's end and the one after it, which decides on the screen, are owed
/// whether or not the pane prints anything), and the grace and a tick since `since`. A check that the pane
/// says nothing new after the grace says nothing before that. The watcher logs a decision before it
/// writes it, so the wait ends only once a second has gone by with no new decision, far longer than the
/// write after one takes: a check made the moment the count grew could run before a wrong write landed.
fn wait_for_a_read_after_the_grace(server: &Server, pane: &str, reads: usize, since: Instant) {
    assert!(
        wait_for(
            || since.elapsed() >= GRACE_AND_A_TICK && server.watcher_reads(pane) >= reads + 2,
            SETTLE
        ),
        "the watcher never read the pane after the grace ({} reads, {:?} since)",
        server.watcher_reads(pane),
        since.elapsed()
    );
    let mut last = (server.watcher_reads(pane), Instant::now());
    assert!(
        wait_for(
            || {
                let now = server.watcher_reads(pane);
                if now != last.0 {
                    last = (now, Instant::now());
                }
                last.1.elapsed() >= Duration::from_secs(1)
            },
            SETTLE
        ),
        "the watcher never stopped reading the pane ({} reads)",
        server.watcher_reads(pane)
    );
}

/// What the pane says of its agent, and why: `@agent_state` and `@agent_rule`.
fn what_the_pane_says(server: &Server, pane: &str) -> String {
    format!(
        "{}/{}",
        server.pane_opt(pane, "@agent_state"),
        server.pane_opt(pane, "@agent_rule")
    )
}

/// How much of the end of the watcher's log a failure message carries.
const FAILURE_LOG_LINES: usize = 120;

/// What the pane holds that would explain a wait on it that ran out, for that wait's failure message: the
/// screen; whether the pane is in a mode or has its input off (either takes the keys a verb types away
/// from the shell), where its cursor is and how big its history is; the pane's `@agent_*` options; the
/// processes on its terminal and the end of its watcher's log. The processes say what is in front of the
/// pane: `TPGID` is the group the terminal gives its input to, and the pane's shell is at its prompt when
/// it is that group's only process. Nothing here may panic: it is built while a test is failing, and a
/// panic would replace the failure it is there to explain.
fn what_the_pane_holds(server: &Server, pane: &str) -> String {
    let asked = |args: &[&str]| {
        server
            .tmux(args)
            .unwrap_or_else(|err| format!("(tmux said: {err})"))
    };
    let screen = asked(&["capture-pane", "-p", "-t", pane]);
    let state = asked(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "in a mode #{pane_in_mode} (#{pane_mode}), input off #{pane_input_off}, cursor \
         #{cursor_x},#{cursor_y}, history #{history_size}",
    ]);
    let options = asked(&["show-options", "-p", "-t", pane])
        .lines()
        .filter(|line| line.starts_with("@agent"))
        .collect::<Vec<_>>()
        .join("\n");
    let tty = asked(&["display-message", "-p", "-t", pane, "#{pane_tty}"]);
    let processes = match tty.strip_prefix("/dev/") {
        Some(name) => match std::process::Command::new("ps")
            .args(["-t", name, "-o", "pid,ppid,pgid,tpgid,stat,etime,command"])
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            Err(err) => format!("(ps could not run: {err})"),
        },
        None => "(the pane has no terminal)".to_string(),
    };
    let log = server.watcher_logs();
    let lines: Vec<&str> = log.lines().collect();
    let end = lines[lines.len().saturating_sub(FAILURE_LOG_LINES)..].join("\n");
    format!(
        "--- the screen:\n{screen}\n\
         --- the pane: {state}\n\
         --- the pane's @agent options:\n{options}\n\
         --- the processes on its terminal ({tty}):\n{processes}\n\
         --- the end of the watcher's log:\n{end}"
    )
}

/// The pane still shows what the last agent left when the next is started, and a new agent that has not
/// drawn yet leaves it there. Read with the new agent's rules, the last agent's idle screen was the new
/// one's idle. The watcher reads only the rows below the line the command was typed on, where nothing is
/// yet: the pane says `unknown` after the grace too, and the verb waits for the agent to draw. Once it
/// has drawn (the fake clears the screen as it draws), the line is off the screen, and the pane's record
/// of it goes.
#[test]
fn a_second_start_reads_nothing_of_the_screen_the_last_agent_left() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = a_pane_whose_agent_died(&server, &fake, None, None);
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "start",
                "second",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "60000",
            ])
        });
        assert!(
            wait_for(|| waiting.exists(), SETTLE),
            "the second agent never ran:\n{}",
            server.capture(&pane)
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "the new agent was read off the screen the last agent left:\n{}",
            server.capture(&pane)
        );
        assert!(
            !starting.is_finished(),
            "agent start was done before the new agent drew anything:\n{}",
            server.capture(&pane)
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_typed_line").is_empty(),
            SETTLE
        ),
        "the line the command was typed on stayed after the agent cleared the screen: {:?}",
        server.pane_opt(&pane, "@agent_typed_line")
    );
    let screen = server.capture(&pane);
    assert!(
        screen.contains("? for shortcuts"),
        "no agent had drawn when the verb was done:\n{screen}"
    );
    assert!(
        !screen.contains("claude"),
        "the screen is the one the last agent left, with the command typed under it:\n{screen}"
    );
}

/// The agent died at a permission prompt, and its screen is still on the pane when the next is started.
/// Read as the new agent's, it would be a `blocked` agent, and the person would be told that an agent
/// that is only starting needs them. The first thing the pane says of the new agent is the grace's
/// `unknown`, and after the grace it says `unknown` still, with no notification, until the agent draws.
#[test]
fn a_second_start_is_not_blocked_for_the_prompt_the_last_agent_died_at() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = a_pane_whose_agent_died(&server, &fake, Some(("-USR2", "blocked")), Some(&recorder));
    assert!(
        server.capture(&pane).contains("Do you want"),
        "the last agent's permission prompt is not on the screen:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let old_pid = server.pane_opt(&pane, "@agent_pid");
    assert!(!old_pid.is_empty(), "the first start recorded no pid");
    let asked_before = recorder.matching("blocked").len();
    // What the verb logs of each look it takes at the pane, as it takes it, for the failure message of
    // a wait that runs out while the verb still waits.
    let verb_log = tmp.path().join("verb.log");

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            server.taw_logging_to(
                &[
                    "agent",
                    "start",
                    "second",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    "60000",
                ],
                &verb_log,
            )
        });
        // The verb takes the old record off the pane before it types, and records the pid of what
        // comes up in its place. From then on what the pane says of the agent is the watcher's of
        // the new one, and the first it says is the grace's `unknown`.
        assert!(
            wait_for(
                || {
                    let pid = server.pane_opt(&pane, "@agent_pid");
                    !pid.is_empty() && pid != old_pid
                },
                SETTLE
            ),
            "nothing came up in the pane: its @agent_pid was {old_pid} before the verb ran\n{}\n\
             --- the verb's log:\n{}",
            what_the_pane_holds(&server, &pane),
            std::fs::read_to_string(&verb_log).unwrap_or_default()
        );
        let mut first = String::new();
        assert!(
            wait_for(
                || {
                    first = server.pane_opt(&pane, "@agent_state");
                    !first.is_empty()
                },
                SETTLE
            ),
            "the pane never said anything of the new agent"
        );
        assert_eq!(
            first, "unknown",
            "the new agent was read off the screen the last agent left"
        );
        assert!(wait_for(|| waiting.exists(), SETTLE));
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "after the grace, the new agent was read off the prompt the last agent died at:\n{}",
            server.capture(&pane)
        );
        assert_eq!(
            recorder.matching("blocked").len(),
            asked_before,
            "a person was told an agent that is only starting needed them: {:?}",
            recorder.lines()
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
}

/// A `tmux` for the verb only that lets the pane's watcher read the pane once before the verb types the
/// agent's command: when the verb comes to send the command's text, it makes the pane print something
/// that changes no row (a title), which the watcher reads, and waits for the watcher to have decided on
/// one more read (its log is in `state`) before it sends the text. That read is the watcher's first since
/// the verb took the last agent's record off the pane, so no grace has begun and it captures the screen,
/// on which the command is nowhere yet. The text goes in a guarded call, `-L <socket> -u if-shell -F -t
/// <pane> <condition> 'send-keys -l -t <pane> -- <text>' <else>`: the pane is its seventh word, and the
/// send its ninth.
fn tmux_that_lets_the_watcher_read_before_the_text(dir: &Path, state: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-lets-the-watcher-read");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             if [ \"$4\" = if-shell ] && case \"$9\" in 'send-keys -l '*) true ;; *) false ;; esac; then\n\
             \x20 pane=\"$7\"\n\
             \x20 log='{state}/logs/watch-'\"$pane\"'.log'\n\
             \x20 before=$(grep -c ' decision: ' \"$log\" 2>/dev/null)\n\
             \x20 tty=$(\"$real\" -L \"$2\" display-message -p -t \"$pane\" '#{{pane_tty}}')\n\
             \x20 printf '\\033]2;a title\\007' > \"$tty\"\n\
             \x20 i=0\n\
             \x20 while [ \"$i\" -lt 200 ]; do\n\
             \x20   if [ \"$(grep -c ' decision: ' \"$log\" 2>/dev/null)\" -gt \"${{before:-0}}\" ]; then\n\
             \x20     : > '{read}'; break\n\
             \x20   fi\n\
             \x20   sleep 0.05; i=$((i + 1))\n\
             \x20 done\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            state = state.display(),
            read = dir.join("the-watcher-read-before-the-text").display(),
        ),
    );
    wrapper
}

/// The verb takes the last agent's record off the pane, and a watcher reads the pane before the shell has
/// echoed the command the verb types: its first read of the new agent, before any grace. Had the verb
/// recorded the line it types the command on before the echo, that read would find the line nowhere on
/// the screen, take it for gone and take it off the pane, and after the grace read the permission prompt
/// the last agent died at as the new agent's: `blocked`, with a request telling a person an agent that is
/// only starting needed them. The line is recorded once the shell has echoed the command, so the pane says
/// `unknown` after the grace, nobody is told anything, and the line is still the pane's.
#[test]
fn a_watcher_that_reads_the_pane_before_the_command_is_echoed_still_reads_below_the_line() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = a_pane_whose_agent_died(&server, &fake, Some(("-USR2", "blocked")), Some(&recorder));
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let asked_before = recorder.matching("blocked").len();
    let stand_in = tmux_that_lets_the_watcher_read_before_the_text(tmp.path(), server.state_dir());

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "agent",
                    "start",
                    "second",
                    "--kind",
                    "claude",
                    "--pane",
                    &pane,
                    "--timeout",
                    "60000",
                ],
                &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
            )
        });
        assert!(
            wait_for(|| waiting.exists(), SETTLE),
            "the second agent never ran:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        assert!(
            tmp.path().join("the-watcher-read-before-the-text").exists(),
            "the stand-in never let the watcher read before the text"
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "after the grace, the new agent was read off the prompt the last agent died at:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        assert_eq!(
            recorder.matching("blocked").len(),
            asked_before,
            "a person was told an agent that is only starting needed them: {:?}",
            recorder.lines()
        );
        assert!(
            !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
            "the line the command was typed on was taken off the pane while it was on the screen:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        assert!(
            !starting.is_finished(),
            "agent start was done before the new agent drew anything"
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
}

/// Makes the watcher of `pane` read it once, with something printed that changes no row (a title), and
/// waits for that read's decision.
fn a_read_of(server: &Server, pane: &str) {
    let before = server.watcher_reads(pane);
    let tty = server.display(pane, "#{pane_tty}");
    std::fs::write(&tty, "\u{1b}]2;a title\u{7}").unwrap();
    assert!(
        wait_for(|| server.watcher_reads(pane) > before, SETTLE),
        "the watcher never read the pane:\n{}",
        what_the_pane_holds(server, pane)
    );
}

/// The watcher's own guard against a read that lands while the line is not on the screen yet: a line
/// recorded before the shell has echoed the command (as `agent start` did, and any writer that records
/// one early still could), and a read before the echo. That read finds the line nowhere; taking it for
/// gone, the watcher took it off the pane, and after the grace read the permission prompt above it as the
/// agent's `blocked`, with a request telling a person an agent that was only starting needed them. One
/// read that does not find the line reads nothing of the screen and keeps the line; only a second read in
/// a row that does not find it takes it for gone. Here the echo comes before that, so the pane says
/// `unknown` after the grace, nobody is told anything, and the line is still the pane's.
#[test]
fn one_read_that_does_not_find_the_line_keeps_it_and_reads_nothing_of_the_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want") && screen.trim_end().ends_with('$')
            },
            SETTLE
        ),
        "the last agent's frame and the prompt never came:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let at = server.display(&pane, "#{cursor_y} #{history_size}");
    // What `agent start` leaves before it types: the hint, the mark of an agent being taken, and here the
    // line as it would be once `claude` is typed at the prompt, written before it is.
    server.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_kind_hint",
        "claude",
        ";",
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_acquiring",
        "taken",
        ";",
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_typed_line",
        &format!("{at} 8 6 claude"),
    ]);
    a_read_of(&server, &pane);
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "one read that did not find the line took it off the pane:\n{}",
        what_the_pane_holds(&server, &pane)
    );
    let _gate = OpensTheGate(gate);
    server.tmux_ok(&["send-keys", "-t", &pane, "-l", "claude"]);
    server.tmux_ok(&["send-keys", "-t", &pane, "Enter"]);
    assert!(
        wait_for(|| waiting.exists(), SETTLE),
        "the agent never ran:\n{}",
        server.capture(&pane)
    );
    let (reads, since) = (server.watcher_reads(&pane), Instant::now());
    wait_for_a_read_after_the_grace(&server, &pane, reads, since);
    assert_eq!(
        what_the_pane_says(&server, &pane),
        "unknown/",
        "after the grace, the agent was read off the frame above its line:\n{}",
        what_the_pane_holds(&server, &pane)
    );
    assert_eq!(
        recorder.matching("blocked").len(),
        0,
        "a person was told an agent that is only starting needed them: {:?}",
        recorder.lines()
    );
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "the line was taken off the pane while it was on the screen:\n{}",
        what_the_pane_holds(&server, &pane)
    );
}

/// Every read the watcher of `pane` has logged, in order: whether it did not find the line an agent's
/// command was typed on, for the first time in a row (it logs that it reads nothing of the screen), and
/// what it decided (the server needs `TAW_LOG=debug`).
fn reads_and_decisions(server: &Server, pane: &str) -> Vec<(bool, String)> {
    let log = std::fs::read_to_string(
        server
            .state_dir()
            .join("logs")
            .join(format!("watch-{pane}.log")),
    )
    .unwrap_or_default();
    let reading = format!("reading pane {pane} (");
    let mut missed = false;
    let mut reads = Vec::new();
    for line in log.lines() {
        if line.contains(reading.as_str()) {
            missed = false;
        } else if line.contains("is not on the screen; reading nothing") {
            missed = true;
        } else if let Some((_, decided)) = line.split_once(" decision: ") {
            reads.push((missed, decided.to_string()));
        }
    }
    reads
}

/// After the grace, the line is on the screen and the agent below it has drawn nothing: `unknown`, with
/// the frame the last agent left above the line. A read that does not find the line (here its row was
/// erased, as a shell's redraw can leave it for a moment) decides on nothing: it keeps what the pane says,
/// where reading the whole screen would have read the frame above the line as the agent's `blocked`.
#[test]
fn a_read_that_does_not_find_the_line_after_the_grace_keeps_what_the_pane_says() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want") && screen.trim_end().ends_with('$')
            },
            SETTLE
        ),
        "the last agent's frame and the prompt never came:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let gate = OpensTheGate(gate);
    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "start",
                "second",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "60000",
            ])
        });
        assert!(
            wait_for(|| waiting.exists(), SETTLE),
            "the agent never ran:\n{}",
            server.capture(&pane)
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "{}",
            what_the_pane_holds(&server, &pane)
        );
        let line = server.pane_opt(&pane, "@agent_typed_line");
        let row: u32 = line
            .split(' ')
            .next()
            .and_then(|row| row.parse().ok())
            .unwrap_or_else(|| panic!("the pane records no line: {line:?}"));
        let before = reads_and_decisions(&server, &pane).len();
        let tty = server.display(&pane, "#{pane_tty}");
        std::fs::write(
            &tty,
            format!("\u{1b}7\u{1b}[{};1H\u{1b}[2K\u{1b}8", row + 1),
        )
        .unwrap();
        // The first read that did not find the line: the one the erasing earned, or one that was under
        // way and captured the screen after it.
        let mut missed = None;
        assert!(
            wait_for(
                || {
                    missed = reads_and_decisions(&server, &pane)
                        .into_iter()
                        .skip(before)
                        .find(|(missed, _)| *missed)
                        .map(|(_, decided)| decided);
                    missed.is_some()
                },
                SETTLE
            ),
            "no read after the line was erased missed it:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        assert_eq!(
            missed.as_deref(),
            Some("Keep"),
            "the read that did not find the line decided on the screen:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        // Let the verb go: the agent draws, and what is on the screen is its own.
        gate.open();
        starting.join().unwrap()
    });
    assert_eq!(code, 0, "{envelope}");
}

/// A `tmux` that notes, when the verb sends the command's text, what `@agent_typed_line` holds then (in
/// `line-when-the-text-was-sent`); when the verb writes the line, what the pane shows then (in
/// `screen-when-the-line-was-written`); and when the verb sends Enter, that it has (`enter-sent`), so the
/// order of the three can be told. The text and the Enter each go in a guarded call, `-L <socket> -u
/// if-shell -F -t <pane> ...`, whose pane is its seventh word.
fn tmux_that_notes_the_line_the_text_and_enter(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-that-notes-the-order");
    write_script(
        &wrapper,
        &format!(
            "real='{real}'\n\
             case \"$*\" in\n\
             \x20 *' if-shell '*' send-keys -l -t '*)\n\
             \x20   \"$real\" -L \"$2\" show-options -pqv -t \"$7\" @agent_typed_line > '{dir}/line-when-the-text-was-sent' ;;\n\
             \x20 *' set-option -p -t '*' @agent_typed_line '*)\n\
             \x20   [ -e '{dir}/enter-sent' ] && : > '{dir}/enter-before-the-line'\n\
             \x20   \"$real\" -L \"$2\" capture-pane -p -t \"$7\" > '{dir}/screen-when-the-line-was-written' ;;\n\
             \x20 *' if-shell '*'send-keys -t '*' Enter'*)\n\
             \x20   : > '{dir}/enter-sent' ;;\n\
             esac\n\
             exec \"$real\" \"$@\"\n",
            real = real.display(),
            dir = dir.display(),
        ),
    );
    wrapper
}

/// `agent start` types the command, waits for the shell to echo it, records the line it is on, and only
/// then sends Enter: the line is never on the pane while the command is not on the screen, and the row it
/// records is the one the command was echoed on.
#[test]
fn the_line_is_recorded_once_the_shell_has_echoed_the_command_and_before_enter() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = common::named_agent(tmp.path(), "claude", None);
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    let notes = tmp.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let stand_in = tmux_that_notes_the_line_the_text_and_enter(&notes);

    let (code, envelope) = server.taw_with_env(
        &["agent", "start", "rev", "--kind", "claude", "--pane", &pane],
        &[("TAW_TMUX_BIN", stand_in.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{envelope}");
    server.wake("taw-fake-claude-never");
    let read = |name: &str| std::fs::read_to_string(notes.join(name)).unwrap_or_default();
    assert!(
        notes.join("line-when-the-text-was-sent").exists(),
        "the stand-in never saw the text sent"
    );
    assert_eq!(
        read("line-when-the-text-was-sent").trim(),
        "",
        "the line was on the pane before the command was typed"
    );
    let screen = read("screen-when-the-line-was-written");
    assert!(
        screen
            .lines()
            .rev()
            .find(|row| !row.trim().is_empty())
            .is_some_and(|row| row.trim_end().ends_with("$ claude")),
        "the line was written before the shell had echoed the command:\n{screen}"
    );
    assert!(
        notes.join("enter-sent").exists() && !notes.join("enter-before-the-line").exists(),
        "Enter was sent before the line was written"
    );
}

/// A pane whose history is full, as a pane that has been in use for a while has (`history_limit` rows,
/// none for a history of none): its history stops growing as the screen scrolls (it drops a tenth of its
/// rows at its limit, and grows again), so how far the screen has scrolled since the command was typed
/// cannot be told from it, and the line is looked for from the row it was typed on, upwards. The agent
/// prints forty lines and draws its working frame under them, with the row `above` above the frame and the
/// row `below` under it (none for an empty one), rows of its own that hold the command's text. `ps1`, when
/// it is given, is the shell's prompt. The line has scrolled off, the whole screen is read, and the pane
/// says `working`, with the verb done.
fn a_full_pane_reads_the_agents_working_frame(
    history_limit: u32,
    ps1: Option<&str>,
    above: &str,
    below: &str,
) {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server_for_a_second_start(tmp.path(), 24);
    server.tmux_ok(&[
        "set-option",
        "-g",
        "history-limit",
        &history_limit.to_string(),
    ]);
    let pane = server.tmux_ok(&["new-window", "-d", "-P", "-F", "#{pane_id}"]);
    if let Some(ps1) = ps1 {
        server.tmux_ok(&["send-keys", "-t", &pane, &format!("PS1='{ps1}'"), "Enter"]);
    }
    server.tmux_ok(&["send-keys", "-t", &pane, "seq 1 200", "Enter"]);
    let prompt_end = ps1.map_or("$", |ps1| ps1.trim_end_matches("\\n"));
    assert!(
        wait_for(
            || {
                let history: u32 = server
                    .display(&pane, "#{history_size}")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                history >= history_limit * 4 / 5
                    && server.capture(&pane).trim_end().ends_with(prompt_end)
            },
            SETTLE
        ),
        "the history never filled:\n{}",
        what_the_pane_holds(&server, &pane)
    );
    attach_watcher(&server, &pane);
    let working = common::repo_root().join("tests/fixtures/screens/claude-2.1.227-working.txt");
    let rows = |name: &str, row: &str| {
        let file = tmp.path().join(name);
        let text = if row.is_empty() {
            String::new()
        } else {
            format!("{row}\n")
        };
        std::fs::write(&file, text).unwrap();
        file
    };
    let (above, below) = (rows("above", above), rows("below", below));
    write_script(
        &fake.bin_dir.join("claude"),
        &format!(
            "i=0; while [ $i -lt 40 ]; do echo \"output line $i\"; i=$((i + 1)); done\n\
             cat '{}'\n\
             cat '{}'\n\
             cat '{}'\n\
             n=0; while [ $n -lt 600 ]; do sleep 0.1; n=$((n + 1)); done\n",
            above.display(),
            working.display(),
            below.display()
        ),
    );

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "20000",
    ]);

    assert_eq!(
        code,
        0,
        "{envelope}\n{}",
        what_the_pane_holds(&server, &pane)
    );
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "working",
            SETTLE
        ),
        "the working agent was not read as working: {}\n{}",
        what_the_pane_says(&server, &pane),
        what_the_pane_holds(&server, &pane)
    );
}

/// The agent's rows that name its model are set in two columns, and the prompt is the shell's own:
/// taking a row for the line because it holds the command read the pane `unknown` for good, with
/// nothing below that row, and the verb ran out its budget though the agent was working. A row is the
/// line only when it has the line's shape: the prompt, or another in its place no wider than it or than
/// 16 columns, and the whole command, with a right prompt after it only where the prompt row showed one.
#[test]
fn in_a_pane_whose_history_is_full_a_row_holding_the_commands_text_is_not_the_line() {
    a_full_pane_reads_the_agents_working_frame(
        50,
        None,
        "  claude-opus-4-1 . ~/work",
        "  claude-opus-4-1 . ~/work . ctx 41%",
    );
}

/// A prompt that ends with a line break leaves the cursor at the left edge, so the line starts with the
/// command and nothing before it, and the agent's rows that name its model are flush left: the status row
/// under the frame starts with the command's text, and taken for the line, it left nothing below it to
/// read, and the verb ran out its budget. A line that starts with no prompt is a row that reads the
/// command and nothing else. The pane keeps no history, so the search for the line starts at the row the
/// command was typed on, the last, wherever the history at its limit stood.
#[test]
fn a_flush_left_row_that_starts_with_the_command_is_not_a_line_typed_at_the_left_edge() {
    a_full_pane_reads_the_agents_working_frame(
        0,
        Some("~/work\\n"),
        "claude-opus-4-1 . ~/work",
        "claude-opus-4-1 . ~/work . ctx 41%",
    );
}

/// Claude Code draws in its prompt's footer, right-aligned, that an update is ready and how to take it,
/// and for an agent installed with mise that ends with the command (`Run: mise upgrade claude`, the text
/// of Claude Code 2.1.293). At the bottom of a pane whose search for the line starts at the row it was
/// typed on, the footer was taken for the line, with nothing below it: the pane said `unknown` for good,
/// and the verb ran out its budget. There is more before the command than the prompt it was typed after.
#[test]
fn claude_codes_footer_that_says_to_run_an_upgrade_of_claude_is_not_the_line() {
    a_full_pane_reads_the_agents_working_frame(
        0,
        None,
        "",
        "  ? for shortcuts                    Update available! Run: mise upgrade claude",
    );
}

/// A tip of the agent's that ends with the command (`run claude`): more before it than a prompt.
#[test]
fn a_tip_that_ends_with_the_command_is_not_the_line() {
    a_full_pane_reads_the_agents_working_frame(
        0,
        None,
        "",
        "  \u{23bf}  Tip: to resume later, run claude",
    );
}

/// A box row with the command's word in it, padded to the box's edge: more after the command than nothing,
/// and the prompt row showed no right prompt for it to be.
#[test]
fn a_box_row_that_holds_the_command_padded_is_not_the_line() {
    a_full_pane_reads_the_agents_working_frame(
        0,
        None,
        "",
        "\u{2502} claude                                         \u{2502}",
    );
}

/// A status row that starts with the command's word and goes on past two spaces: the same.
#[test]
fn a_status_row_that_starts_with_the_command_and_pads_it_is_not_the_line() {
    a_full_pane_reads_the_agents_working_frame(
        0,
        None,
        "",
        "claude  \u{b7}  opus 4.1  \u{b7}  ctx 41%",
    );
}

/// How the zsh of [`a_zsh_with_a_right_prompt_keeps_its_line`] draws the line a command is typed on: its
/// prompt, with a right prompt that stays on an accepted line (zsh's default, no `TRANSIENT_RPROMPT`),
/// and what it does besides.
#[derive(Clone, Copy)]
struct Zsh {
    /// The prompt, left of the cursor, as zsh's `$'...'` reads it (`\n` a line break).
    prompt: &'static str,
    /// The right prompt, none when it is empty.
    rprompt: &'static str,
    /// A suggestion is shown after the cursor once `claude` is typed, as zsh-autosuggestions does
    /// (`POSTDISPLAY`, cleared when the line is accepted).
    suggestion: bool,
    /// It is slow to take the first key of a line (1.4 s, longer than `agent start` looks for the echo),
    /// in a wait of its own (`zselect`, a builtin): the shell stays in front of the pane all along, as a
    /// shell slow to echo on a busy machine does. A `sleep` there would be a program in front of the shell
    /// when the verb comes to press Enter, which it then does not press.
    slow: bool,
    /// What the prompt is rewritten to when a line is accepted (a transient prompt, in
    /// `zle-line-finish`).
    transient: Option<&'static str>,
    /// The transient prompt drops the right prompt as well, as powerlevel10k's does.
    drops_rprompt: bool,
    /// What `agent start` passes the agent.
    args: &'static [&'static str],
}

impl Zsh {
    const PLAIN: Zsh = Zsh {
        prompt: "~/work > ",
        rprompt: "10:32",
        suggestion: false,
        slow: false,
        transient: None,
        drops_rprompt: false,
        args: &[],
    };

    /// The prompt's last line, the one the command is typed on, as it is drawn.
    fn last_line(&self) -> &'static str {
        self.prompt.rsplit("\\n").next().unwrap_or_default()
    }

    /// The row the command was entered on, as far as it starts: the prompt it has then, and the command.
    fn entered(&self) -> String {
        let mut entered = format!(
            "{} claude",
            self.transient.unwrap_or(self.last_line()).trim_end()
        );
        for arg in self.args {
            entered.push(' ');
            entered.push_str(arg);
        }
        entered.trim_start().to_string()
    }
}

/// The config of the zsh `how` says, in a directory of its own, never the user's: the returned
/// `ZDOTDIR`.
fn zsh_with_a_right_prompt(dir: &Path, how: Zsh) -> PathBuf {
    let zdot = dir.join("zdot");
    std::fs::create_dir_all(&zdot).unwrap();
    let wait = if how.slow {
        "[[ -z $BUFFER ]] && zselect -t 140; "
    } else {
        ""
    };
    let suggest = if how.suggestion {
        "if [[ $BUFFER == claude ]]; then POSTDISPLAY=' --resume 3f0c2a9e'; else POSTDISPLAY=''; fi"
    } else {
        ":"
    };
    let transient = match how.transient {
        Some(short) => format!(
            "_probe_finish() {{ PROMPT='{short}'; {drop}zle reset-prompt; }}\n\
             zle -N zle-line-finish _probe_finish\n\
             precmd() {{ PROMPT=$'{prompt}'; RPROMPT='{rprompt}'; }}\n",
            drop = if how.drops_rprompt {
                "RPROMPT=''; "
            } else {
                ""
            },
            prompt = how.prompt,
            rprompt = how.rprompt,
        ),
        None => String::new(),
    };
    std::fs::write(
        zdot.join(".zshrc"),
        format!(
            "zmodload zsh/zselect\n\
             PROMPT=$'{prompt}'\n\
             RPROMPT='{rprompt}'\n\
             unsetopt TRANSIENT_RPROMPT\n\
             _probe_insert() {{ {wait}zle .self-insert; {suggest}; }}\n\
             zle -N self-insert _probe_insert\n\
             _probe_accept() {{ POSTDISPLAY=''; zle .accept-line; }}\n\
             zle -N accept-line _probe_accept\n\
             {transient}",
            prompt = how.prompt,
            rprompt = how.rprompt,
        ),
    )
    .unwrap();
    zdot
}

/// A second start in a zsh with a right prompt, under the permission prompt the last agent died at. The
/// row the command is on changes when it is entered: the suggestion after the cursor goes (or the shell,
/// slow to echo, had not echoed it when the verb looked), and the right prompt stays, so the row neither
/// reads as the shell echoed it nor ends with the command. Told only by those, the line was missed twice,
/// taken for gone, and the frame above it read as the new agent's `blocked`, with a request telling a
/// person an agent that was only starting needed them. The row starts as the line did, with the prompt
/// left of the cursor and the command: the pane says `unknown` after the grace, nobody is told anything,
/// and the line is still the pane's.
fn a_zsh_with_a_right_prompt_keeps_its_line(how: Zsh) {
    if !Path::new("/bin/zsh").exists() {
        eprintln!("skipping: there is no zsh at /bin/zsh");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let zdot = zsh_with_a_right_prompt(tmp.path(), how);
    let prompt = how
        .prompt
        .split("\\n")
        .next()
        .unwrap_or_default()
        .trim_end();
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH='{}':/usr/bin:/bin ZDOTDIR='{}' HOME='{}' HISTFILE=/dev/null /bin/zsh -i",
            fake.bin_dir.display(),
            zdot.display(),
            tmp.path().join("home").display()
        ),
    ]);
    attach_watcher(&server, &pane);
    assert!(
        wait_for(|| server.capture(&pane).contains(prompt), SETTLE),
        "zsh never drew its prompt:\n{}",
        server.capture(&pane)
    );
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want")
                    && screen.trim_end().lines().last().is_some_and(|row| {
                        row.starts_with(how.last_line().trim_end()) && row.ends_with(how.rprompt)
                    })
            },
            SETTLE
        ),
        "the last agent's frame and the next prompt never came:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let asked_before = recorder.matching("blocked").len();

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            let mut args = vec![
                "agent",
                "start",
                "second",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "60000",
                "--",
            ];
            args.extend(how.args);
            server.taw(&args)
        });
        assert!(
            wait_for(|| waiting.exists(), SETTLE),
            "the agent never ran:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        let screen = server.capture(&pane);
        let entered = how.entered();
        assert!(
            screen.lines().any(|row| row.starts_with(&entered)
                && (how.drops_rprompt || row.trim_end().ends_with(how.rprompt))),
            "the command's row is not the prompt, the command and the right prompt:\n{screen}"
        );
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "after the grace, the new agent was read off the prompt the last agent died at:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        assert_eq!(
            recorder.matching("blocked").len(),
            asked_before,
            "a person was told an agent that is only starting needed them: {:?}",
            recorder.lines()
        );
        assert!(
            !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
            "the line was taken off the pane while it was on the screen:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
}

/// While a new agent starts, a pid can stand for it that is dead before the agent has a process of its own:
/// the one `agent start` leaves when its budget runs out with the agent still coming up is what was in
/// front of the pane then, which can be a helper the shell ran before the command (a slow preexec hook's),
/// gone a moment later. The watcher took a dead pid for the agent's exit and the line its command was
/// typed on for nobody's, at its very first read of the new agent, so that once the grace was over it read
/// the frame above the line, the permission prompt the last agent died at, as the new agent's `blocked`,
/// with a request. Nothing is believed of an agent in its grace, its exit included: the line stays, the
/// agent's own pid is recorded, and after the grace the pane says `unknown`.
#[test]
fn a_dead_pid_recorded_while_the_agent_starts_does_not_take_its_line_away() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want") && screen.trim_end().ends_with('$')
            },
            SETTLE
        ),
        "the last agent's frame and the prompt never came:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let _gate = OpensTheGate(gate);
    server.tmux_ok(&["send-keys", "-t", &pane, "-l", "claude"]);
    assert!(
        wait_for(
            || server.capture(&pane).trim_end().ends_with("$ claude"),
            SETTLE
        ),
        "the command was never echoed:\n{}",
        server.capture(&pane)
    );
    let at = server.display(&pane, "#{cursor_y} #{history_size}");
    // What `agent start` leaves once the command is echoed: the hint, the mark of an agent being taken,
    // the line, and the pid of a helper the shell ran, which has died (no process has a number this big).
    server.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_kind_hint",
        "claude",
        ";",
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_acquiring",
        "taken",
        ";",
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_pid",
        "999999",
        ";",
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_typed_line",
        &format!("{at} 8 6 claude"),
    ]);
    a_read_of(&server, &pane);
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "the new agent's first read took a dead pid for its exit and its line away:\n{}",
        what_the_pane_holds(&server, &pane)
    );
    server.tmux_ok(&["send-keys", "-t", &pane, "Enter"]);
    assert!(
        wait_for(|| waiting.exists(), SETTLE),
        "the agent never ran:\n{}",
        server.capture(&pane)
    );
    // The verb's next look records the agent's own pid.
    let agent = child_of_the_shell(&server, &pane);
    assert!(!agent.is_empty(), "nothing is running in the pane");
    server.set_pane_opt(&pane, "@agent_pid", &agent);
    let (reads, since) = (server.watcher_reads(&pane), Instant::now());
    wait_for_a_read_after_the_grace(&server, &pane, reads, since);
    assert_eq!(
        what_the_pane_says(&server, &pane),
        "unknown/",
        "after the grace, the agent was read off the frame above its line:\n{}",
        what_the_pane_holds(&server, &pane)
    );
    assert_eq!(
        recorder.matching("blocked").len(),
        0,
        "a person was told an agent that is only starting needed them: {:?}",
        recorder.lines()
    );
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "the line was taken off the pane while it was on the screen:\n{}",
        what_the_pane_holds(&server, &pane)
    );
}

#[test]
fn a_zsh_that_shows_a_suggestion_and_a_right_prompt_keeps_its_line_after_enter() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        suggestion: true,
        ..Zsh::PLAIN
    });
}

#[test]
fn a_zsh_slow_to_echo_with_a_right_prompt_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        slow: true,
        ..Zsh::PLAIN
    });
}

/// A transient prompt that rewrites the row the command was entered on to a short prompt, and keeps the
/// right prompt on it, as zsh does unless `TRANSIENT_RPROMPT` is set: the row neither started as the line
/// did nor ended with the command, and the frame above it was read as the new agent's after the grace.
/// The row holds the command as a command line does, after the short prompt and before a run of spaces
/// and the right prompt.
#[test]
fn a_zsh_transient_prompt_that_keeps_its_right_prompt_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/work on main \u{276f} ",
        transient: Some("\u{276f} "),
        ..Zsh::PLAIN
    });
}

/// The same with a right prompt that has two spaces inside it: cut at its last run of two spaces, what
/// was left of the row did not end with the command, and the frame above it was read as the new agent's.
#[test]
fn a_zsh_transient_prompt_that_keeps_a_right_prompt_with_two_spaces_in_it_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/work on main \u{276f} ",
        rprompt: "main  10:32",
        transient: Some("\u{276f} "),
        ..Zsh::PLAIN
    });
}

/// powerlevel10k's way: a long one-line prompt with a right prompt, rewritten to a short one with no right
/// prompt when a command is entered, and a command long enough to wrap after the long prompt. Its line
/// was told by its last row's part alone, which is all that row holds, and on Enter the short prompt took
/// the command back to one row: no row read as the last part, and the frame above was read as the new
/// agent's after the grace. The rewritten row holds the whole command at its end.
#[test]
fn a_zsh_transient_prompt_that_unwraps_a_long_command_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/src/github/tmux-agent-wizard on main \u{276f} ",
        transient: Some("\u{276f} "),
        drops_rprompt: true,
        args: &["--resume", "3f0c2a9e-7d41-4b6e-9c55-2b1e8a6d0f42"],
        ..Zsh::PLAIN
    });
}

/// A prompt that ends with a line break, which leaves the cursor at the left edge, rewritten to a short
/// prompt on the row the command was entered on: the line, told by the command alone, was the row that
/// read the command and nothing else, which the rewritten row is not, and the frame above it was read as
/// the new agent's after the grace. The row holds the command after the short prompt.
#[test]
fn a_zsh_transient_prompt_over_a_prompt_that_ends_with_a_line_break_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/work\\n",
        transient: Some("\u{276f} "),
        ..Zsh::PLAIN
    });
}

/// A prompt of two lines whose last line is short (`\u{276f} `, as pure's and starship's are), with no right
/// prompt, rewritten when a command is entered by a transient prompt that shows more than that last line
/// did (`14:32 \u{276f} `, as a transient prompt of one's own or oh-my-posh's can): held to the width of the
/// last line, the rewritten row was not the line, and the frame above it was read as the new agent's
/// after the grace. What comes before the command can take 16 columns, whatever the prompt took.
#[test]
fn a_zsh_transient_prompt_wider_than_the_last_line_of_a_two_line_prompt_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/work on main\\n\u{276f} ",
        rprompt: "",
        transient: Some("14:32 \u{276f} "),
        ..Zsh::PLAIN
    });
}

/// The same over a prompt that ends with a line break, which leaves the cursor at the left edge.
#[test]
fn a_zsh_transient_prompt_with_the_time_over_a_prompt_that_ends_with_a_line_break_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "~/work\\n",
        rprompt: "",
        transient: Some("14:32 \u{276f} "),
        ..Zsh::PLAIN
    });
}

/// A prompt with a symbol tmux draws as one cell two columns wide, which its characters one at a time make
/// one (a cloud with U+FE0F, as starship's cloud modules show), and a right prompt: the prompt was cut a
/// column off where the cursor was, the row the command was entered on neither started as the line did
/// nor ended with the command, and the frame above it was read as the new agent's after the grace.
#[test]
fn a_zsh_whose_prompt_has_an_emoji_two_columns_wide_and_a_right_prompt_keeps_its_line() {
    a_zsh_with_a_right_prompt_keeps_its_line(Zsh {
        prompt: "\u{2601}\u{fe0f} ~/work > ",
        ..Zsh::PLAIN
    });
}

/// A `claude` that prints an install's progress before the agent it fetches starts, as `npx` does, notes
/// in `printed` that it has, waits at `gate`, and then becomes the fake agent. It gives up after a minute,
/// so that nothing is left running when a test fails before it opens the gate.
fn a_claude_that_installs_first(fake: &FakeAgent, dir: &Path) -> (PathBuf, PathBuf) {
    let (printed, gate) = (dir.join("printed"), dir.join("gate"));
    write_script(
        &fake.bin_dir.join("claude"),
        &format!(
            "printf 'Need to install the following packages:\\n  @anthropic-ai/claude-code@2.1.300\\n'\n\
             printf 'added 3 packages in 2s\\n'\n\
             : > '{}'\n\
             i=0\n\
             while [ ! -e '{}' ] && [ \"$i\" -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done\n\
             exec '{}'\n",
            printed.display(),
            gate.display(),
            common::fake_agent_fixture().display()
        ),
    );
    (printed, gate)
}

/// A new process that prints over the last agent's frame has drawn something: an install's progress,
/// below the line the command was typed on, with the last agent's permission prompt still above it. What
/// the pane says of the new agent is read off what it printed, which no rule of Claude's knows: `unknown`,
/// with no notification; and the verb waits for the agent itself.
#[test]
fn a_successor_that_prints_an_install_over_the_last_frame_is_not_read_off_that_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = a_pane_whose_agent_died(&server, &fake, Some(("-USR2", "blocked")), Some(&recorder));
    let (printed, gate) = a_claude_that_installs_first(&fake, tmp.path());
    let asked_before = recorder.matching("blocked").len();

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "start",
                "second",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "60000",
            ])
        });
        assert!(
            wait_for(|| printed.exists(), SETTLE),
            "the install never printed:\n{}",
            server.capture(&pane)
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        let screen = server.capture(&pane);
        assert!(
            screen.contains("Do you want") && screen.contains("added 3 packages"),
            "the install did not print under the last agent's prompt:\n{screen}"
        );
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "the new agent was read off the frame the install printed under:\n{screen}"
        );
        assert_eq!(
            recorder.matching("blocked").len(),
            asked_before,
            "a person was told an agent that is installing needed them: {:?}",
            recorder.lines()
        );
        assert!(
            !starting.is_finished(),
            "agent start was done on the last agent's frame, while the install ran:\n{screen}"
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
    let status = envelope["result"]["agent"]["agent_status"].as_str();
    assert!(
        matches!(status, Some("idle" | "working")),
        "the verb returned before the agent was read: {envelope}"
    );
}

/// A shell that rewrites its prompt rows when a command is entered, as powerlevel10k's transient prompt
/// does: a two-row prompt (a row of where it is, and `❯`) becomes one row with the command on it. Run as
/// a script, it gives the commands it runs its own process group.
fn a_shell_with_a_transient_prompt(dir: &Path) -> PathBuf {
    let shell = dir.join("transient-shell");
    write_script(
        &shell,
        "while :; do\n\
         \x20 printf '~/work on main\\n\\342\\235\\257 '\n\
         \x20 IFS= read -r line || exit 0\n\
         \x20 printf '\\033[2A\\r\\033[J\\342\\235\\257 %s\\n' \"$line\"\n\
         \x20 eval \"$line\"\n\
         done\n",
    );
    shell
}

/// A shell whose prompt rewrites its rows on Enter, under the frame of an agent that died at a permission
/// prompt: the rows above the line, and the line itself, change when the command is entered, and nothing
/// is drawn below it. That is no agent having drawn: the pane says `unknown` after the grace, with no
/// notification, and the verb waits for the agent to draw.
#[test]
fn a_transient_prompt_is_not_an_agent_that_drew() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, recorder) = server_for_a_second_start(tmp.path(), 24);
    let shell = a_shell_with_a_transient_prompt(tmp.path());
    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        "-P",
        "-F",
        "#{pane_id}",
        &format!(
            "exec env PATH='{}':/usr/bin:/bin sh '{}'",
            fake.bin_dir.display(),
            shell.display()
        ),
    ]);
    attach_watcher(&server, &pane);
    // What the last agent left: its permission prompt.
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    assert!(
        wait_for(|| server.capture(&pane).contains('\u{276f}'), SETTLE),
        "the shell never drew its prompt:\n{}",
        server.capture(&pane)
    );
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want") && screen.trim_end().ends_with("main\n\u{276f}")
            },
            SETTLE
        ),
        "the last agent's frame and the next prompt never came:\n{}",
        server.capture(&pane)
    );
    let (waiting, gate) = a_claude_that_waits_at_a_gate(&fake, tmp.path());
    let asked_before = recorder.matching("blocked").len();

    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| {
            server.taw(&[
                "agent",
                "start",
                "second",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--timeout",
                "60000",
            ])
        });
        assert!(
            wait_for(|| waiting.exists(), SETTLE),
            "the agent never ran:\n{}",
            server.capture(&pane)
        );
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        let screen = server.capture(&pane);
        assert!(
            screen.contains("\u{276f} claude")
                && !screen.contains("~/work on main\n\u{276f} claude"),
            "the shell did not rewrite its prompt:\n{screen}"
        );
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "the new agent was read off the last agent's frame:\n{screen}"
        );
        assert_eq!(
            recorder.matching("blocked").len(),
            asked_before,
            "a person was told an agent that is only starting needed them: {:?}",
            recorder.lines()
        );
        assert!(
            !starting.is_finished(),
            "agent start took the rewritten prompt rows for the agent having drawn:\n{screen}"
        );
        gate.open();
        starting.join().unwrap()
    });

    assert_eq!(code, 0, "{envelope}");
}

/// An agent that draws its frame below the line it was typed on, with no clear (its idle prompt box,
/// under the permission prompt the last agent died at), is read off what it drew: `idle`. The line is
/// still on the screen, so the pane keeps it while the agent runs; and once the agent exits, the line is
/// nobody's, and the pane's record of it goes.
#[test]
fn an_agent_drawn_below_the_line_is_read_off_what_it_drew_and_lets_go_of_the_line_when_it_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, fake, _recorder) = server_for_a_second_start(tmp.path(), 50);
    let pane = a_pane_whose_agent_died(&server, &fake, Some(("-USR2", "blocked")), None);
    let idle = common::repo_root().join("tests/fixtures/screens/claude-idle.txt");
    write_script(
        &fake.bin_dir.join("claude"),
        &format!("cat '{}'\nexec sleep 600\n", idle.display()),
    );

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "second",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "60000",
    ]);

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "idle", SETTLE),
        "the agent was not read off what it drew: {}\n{}",
        what_the_pane_says(&server, &pane),
        server.capture(&pane)
    );
    let screen = server.capture(&pane);
    assert!(
        screen.contains("Do you want") && screen.contains("? for shortcuts"),
        "the agent did not draw under the last agent's frame:\n{screen}"
    );
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "the line went while it was on the screen"
    );
    let agent = server.pane_opt(&pane, "@agent_pid");
    assert!(!agent.is_empty(), "the start recorded no pid");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &agent])
        .status()
        .unwrap();
    assert!(status.success(), "kill {agent} failed");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown",
            SETTLE
        ),
        "the watcher never read the exit: {}",
        what_the_pane_says(&server, &pane)
    );
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_typed_line").is_empty(),
            SETTLE
        ),
        "the line stayed after its agent exited: {:?}",
        server.pane_opt(&pane, "@agent_typed_line")
    );
}

/// `/bin/sh` body of a stand-in OpenCode: a full-screen program, as the real one is, on the alternate
/// screen, so the line its command was typed on stays the pane's for its whole life. Its composer at
/// rest, which none of OpenCode's rules knows (it reads idle only as the fallback); SIGUSR1 turns a turn
/// on and off (its progress bar, `progress_bar_working`), and SIGUSR2 opens a model picker, which no rule
/// knows either. `PIDFILE` is replaced by where it records its pid.
const FAKE_OPENCODE: &str = r#"printf '%s\n' "$$" > 'PIDFILE'
draw() { printf '\033[2J\033[H%s\n' "$1"; }
step=rest
render() {
  case "$step" in
    rest) draw '┃  Ask anything...
┃
┃  Build  big-pickle OpenCode Zen

  tab agents  ctrl+p commands' ;;
    work) draw '┃  Ask anything...
┃
┃  Build  big-pickle OpenCode Zen

  ■■■■⬝⬝⬝⬝  esc interrupt' ;;
    picker) draw 'Select model

  Recent
  big-pickle OpenCode Zen
  gpt-5.6 OpenAI' ;;
  esac
}
trap 'if [ "$step" = work ]; then step=rest; else step=work; fi; render' USR1
trap 'step=picker; render' USR2
trap 'printf "\033[?1049l"; exit 0' TERM INT
printf '\033[?1049h\033]2;OpenCode\007'
render
while :; do sleep 0.2; done
"#;

/// An agent whose rules do not know its rest, started by `taw agent start`: OpenCode reads idle only as
/// the fallback, a frame no rule knows, and it is read below the line its command was typed on for its
/// whole life (the alternate screen keeps that line). Its composer at rest reads idle, a turn's end
/// `done`, and a frame no rule knows after that (a model picker) leaves the `done` as it is.
#[test]
fn an_agent_whose_rules_do_not_know_its_rest_reads_idle_and_keeps_the_done_of_its_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let pid_file = tmp.path().join("opencode.pid");
    write_script(
        &bin_dir.join("opencode"),
        &FAKE_OPENCODE.replace("PIDFILE", &pid_file.display().to_string()),
    );
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![
            ("HOME".to_string(), home.display().to_string()),
            ("TAW_LOG".to_string(), "debug".to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    attach_watcher(&server, &pane);

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "coder",
        "--kind",
        "opencode",
        "--pane",
        &pane,
        "--timeout",
        "60000",
    ]);
    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(|| common::read_pid(&pid_file).is_some(), SETTLE),
        "the stand-in never recorded its pid"
    );
    let pid = common::read_pid(&pid_file).unwrap();
    let signal = |sig: &str| {
        let status = std::process::Command::new("kill")
            .args([sig, &pid])
            .status()
            .unwrap();
        assert!(status.success(), "kill {sig} {pid} failed");
    };
    let expect = |wanted: &str| {
        assert!(
            wait_for(|| what_the_pane_says(&server, &pane) == wanted, SETTLE),
            "wanted {wanted}, the pane says {}\n{}",
            what_the_pane_says(&server, &pane),
            what_the_pane_holds(&server, &pane)
        );
    };

    expect("idle/");
    assert!(
        !server.pane_opt(&pane, "@agent_typed_line").is_empty(),
        "the line the command was typed on is not the pane's any more, so nothing here is read below it"
    );
    signal("-USR1");
    expect("working/progress_bar_working");
    signal("-USR1");
    expect("done/");

    // A frame no rule knows after the turn: once the watcher has read it, and read nothing more for a
    // second, the pane still says `done`.
    let reads = server.watcher_reads(&pane);
    signal("-USR2");
    assert!(
        wait_for(
            || server.capture(&pane).contains("Select model")
                && server.watcher_reads(&pane) > reads,
            SETTLE
        ),
        "the watcher never read the picker ({} reads)\n{}",
        server.watcher_reads(&pane),
        what_the_pane_holds(&server, &pane)
    );
    let mut last = (server.watcher_reads(&pane), Instant::now());
    assert!(
        wait_for(
            || {
                let now = server.watcher_reads(&pane);
                if now != last.0 {
                    last = (now, Instant::now());
                }
                last.1.elapsed() >= Duration::from_secs(1)
            },
            SETTLE
        ),
        "the watcher never stopped reading the pane"
    );
    assert_eq!(
        what_the_pane_says(&server, &pane),
        "done/",
        "a frame no rule knows took the turn's done away\n{}",
        what_the_pane_holds(&server, &pane)
    );
    signal("-TERM");
}

/// `/bin/sh` body of a stand-in Codex: it sets its folder's name as its title, as the real one does, and
/// draws its composer at rest, which taw's `taw_composer_idle` knows. When `SLOW` exists it first prints
/// a line no rule knows (an update's), says so in `WAITING`, adds a dot to it every half second for five
/// seconds, as an update's progress does (so the watcher reads it again after any grace), and waits for
/// `GATE` before it draws.
/// `PIDFILE` is replaced by where it records its pid.
const FAKE_CODEX: &str = r#"printf '%s\n' "$$" > 'PIDFILE'
trap 'exit 0' TERM INT
printf '\033]2;%s\007' "${PWD##*/}"
if [ -e 'SLOW' ]; then
  printf 'Updating Codex to the latest version...\n'
  : > 'WAITING'
  i=0
  while [ "$i" -lt 10 ]; do sleep 0.5; printf '.'; i=$((i + 1)); done
  while [ ! -e 'GATE' ]; do sleep 0.1; done
fi
printf '\n› Ask Codex to do anything\n\n  gpt-5.6-sol default · ~/work\n'
while :; do sleep 0.2; done
"#;

/// A Codex started again in the pane the last one exited in, with taw's supplement (the default), under
/// the same title the last one left there: what it prints before its composer is no idle of its own
/// (Codex's rules know its idle screen), and once it draws its composer it reads idle off it, not off the
/// title the last one left.
#[test]
fn a_codex_started_again_under_the_title_the_last_one_left_reads_idle_only_once_it_draws() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    let (pid_file, slow, waiting, gate) = (
        tmp.path().join("codex.pid"),
        tmp.path().join("slow"),
        tmp.path().join("waiting"),
        tmp.path().join("gate"),
    );
    write_script(
        &bin_dir.join("codex"),
        &FAKE_CODEX
            .replace("PIDFILE", &pid_file.display().to_string())
            .replace("SLOW", &slow.display().to_string())
            .replace("WAITING", &waiting.display().to_string())
            .replace("GATE", &gate.display().to_string()),
    );
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![
            ("HOME".to_string(), home.display().to_string()),
            ("TAW_LOG".to_string(), "debug".to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    attach_watcher(&server, &pane);
    let start = |name: &str| {
        server.taw(&[
            "agent",
            "start",
            name,
            "--kind",
            "codex",
            "--pane",
            &pane,
            "--timeout",
            "60000",
        ])
    };
    let title = || server.tmux_ok(&["display-message", "-p", "-t", &pane, "#{pane_title}"]);

    let (code, envelope) = start("first");
    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "idle", SETTLE),
        "the first codex never read idle: {}\n{}",
        what_the_pane_says(&server, &pane),
        what_the_pane_holds(&server, &pane)
    );
    let left = title();
    assert!(!left.trim().is_empty(), "the first codex set no title");
    let pid = common::read_pid(&pid_file).expect("the first codex recorded no pid");
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(status.success(), "kill {pid} failed");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_state") == "unknown"
                && server.pane_opt(&pane, "@agent_typed_line").is_empty(),
            SETTLE
        ),
        "the watcher never read the exit: {}\n{}",
        what_the_pane_says(&server, &pane),
        what_the_pane_holds(&server, &pane)
    );

    std::fs::write(&slow, "").unwrap();
    let (code, envelope) = std::thread::scope(|scope| {
        let gate = OpensTheGate(gate.clone());
        let starting = scope.spawn(|| start("second"));
        assert!(
            wait_for(
                || waiting.exists() && server.capture(&pane).contains("Updating Codex"),
                SETTLE
            ),
            "the second codex never printed its update:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        // Its progress is read after any grace a start has, and then nothing more is printed.
        let (reads, since) = (server.watcher_reads(&pane), Instant::now());
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert!(
            wait_for(|| server.capture(&pane).contains("\n.........."), SETTLE),
            "the update's progress never ended:\n{}",
            what_the_pane_holds(&server, &pane)
        );
        // Its last dot can come after that wait (the dots take five seconds and more, the wait as little as
        // the grace and a quiet second), so the checks wait for the watcher to decide on it too: the grace
        // is over and the reads are made, so this is a second with no new decision.
        wait_for_a_read_after_the_grace(&server, &pane, reads, since);
        assert_eq!(
            title(),
            left,
            "the second codex is not under the last one's title"
        );
        assert_eq!(
            what_the_pane_says(&server, &pane),
            "unknown/",
            "the second codex was read before it drew\n{}",
            what_the_pane_holds(&server, &pane)
        );
        gate.open();
        starting.join().unwrap()
    });
    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || what_the_pane_says(&server, &pane) == "idle/taw_composer_idle",
            SETTLE
        ),
        "the second codex was not read idle off its composer: {}\n{}",
        what_the_pane_says(&server, &pane),
        what_the_pane_holds(&server, &pane)
    );
    let pid = common::read_pid(&pid_file).expect("the second codex recorded no pid");
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status();
}

/// `agent explain` says what the watcher decides on: while the line an agent's command was typed on is on
/// the screen, the rows below it. With the last agent's permission prompt above the line and nothing
/// below it, a pane only the hint names explains as `unknown`, with no rule, as the watcher writes it,
/// and not as the `blocked` the prompt above would read as.
#[test]
fn agent_explain_reads_only_below_the_line_the_command_was_typed_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, _fake, _recorder) = server_for_a_second_start(tmp.path(), 24);
    let pane = server.pane();
    let frame = common::repo_root().join("tests/fixtures/screens/claude-permission.txt");
    server.tmux_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("cat '{}'", frame.display()),
        "Enter",
    ]);
    assert!(
        wait_for(
            || {
                let screen = server.capture(&pane);
                screen.contains("Do you want") && screen.trim_end().ends_with('$')
            },
            SETTLE
        ),
        "the last agent's frame and the prompt never came:\n{}",
        server.capture(&pane)
    );
    // A command typed at the prompt, the line `agent start` records, and the hint it sets.
    server.tmux_ok(&["send-keys", "-t", &pane, "-l", "claude --resume abc"]);
    assert!(
        wait_for(
            || server.capture(&pane).contains("claude --resume abc"),
            SETTLE
        ),
        "the command was never typed"
    );
    let at = server.display(&pane, "#{cursor_y} #{history_size}");
    server.set_pane_opt(
        &pane,
        "@agent_typed_line",
        &format!("{at} 8 19 claude --resume abc"),
    );
    server.set_pane_opt(&pane, "@agent_kind_hint", "claude");

    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );

    assert_eq!(explained["state"], "unknown", "{explained}");
    assert_eq!(
        explained["matched_rule"],
        serde_json::Value::Null,
        "{explained}"
    );
    // With the line gone, the whole screen is read, prompt and all.
    server.tmux_ok(&["set-option", "-p", "-u", "-t", &pane, "@agent_typed_line"]);
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(explained["state"], "blocked", "{explained}");
}

/// What tmux was asked, one invocation a line, by the callers whose command line holds `caller`: a `tmux`
/// that logs those and runs every call. A watcher is told by its command line, `taw watch run <pane>`.
fn tmux_that_logs_the_calls_of(dir: &Path, caller: &str) -> (PathBuf, PathBuf) {
    let (wrapper, log) = (dir.join("tmux-that-logs-some"), dir.join("tmux-calls.txt"));
    write_script(
        &wrapper,
        &format!(
            "case \"$(ps -o command= -p $PPID)\" in\n\
             \x20 *'{caller}'*) {{ printf '%s' \"$*\" | tr '\\n\\t\\r' '   '; echo; }} >> '{}' ;;\n\
             esac\n\
             exec '{}' \"$@\"\n",
            log.display(),
            taw::tmux::Tmux::with_socket_name("unused").bin.display()
        ),
    );
    (wrapper, log)
}

/// `agent explain` reads the screen and what says where the line is on it (the line, the size of the
/// history, the alternate screen) in one tmux invocation, as the verb and the watcher do: read in two,
/// output between them scrolls the screen, and the line is looked for a row off.
#[test]
fn agent_explain_reads_the_screen_and_the_history_in_one_tmux_call() {
    let tmp = tempfile::tempdir().unwrap();
    let (logging, log) = tmux_that_logs(tmp.path());
    let server = Server::start();
    server.write_config(CONFIG);
    let pane = server.pane();
    assert!(
        wait_for(|| server.capture(&pane).contains('$'), SETTLE),
        "the shell never drew a prompt:\n{}",
        server.capture(&pane)
    );
    server.set_pane_opt(&pane, "@agent_kind_hint", "claude");
    server.set_pane_opt(&pane, "@agent_typed_line", "0 0 8 6 claude");

    let (code, envelope) = server.taw_with_env(
        &["agent", "explain", &pane],
        &[("TAW_TMUX_BIN", logging.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{envelope}");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let captures: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("capture-pane"))
        .collect();
    assert!(!captures.is_empty(), "explain captured nothing:\n{calls}");
    for capture in captures {
        assert!(
            capture.contains("@agent_typed_line") && capture.contains("history_size"),
            "the screen was read apart from where the line is on it: {capture}\n{calls}"
        );
    }
}

/// The watcher reads the size of the history in the same tmux invocation as every screen it captures, a
/// capture that rides with the display and one that does not alike (the first read after a grace, which
/// captured nothing): read apart, output between the two scrolls the screen, and the line its agent's
/// command was typed on is looked for a row off.
#[test]
fn the_watcher_reads_the_history_with_every_screen_it_captures() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    let (logging, log) = tmux_that_logs_the_calls_of(tmp.path(), " watch run ");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env: vec![
            ("HOME".to_string(), home.display().to_string()),
            ("TAW_TMUX_BIN".to_string(), logging.display().to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    let pane = server.pane();
    attach_watcher(&server, &pane);

    let (code, envelope) = server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "30000",
    ]);

    assert_eq!(code, 0, "{envelope}");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let captures: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("capture-pane"))
        .collect();
    assert!(
        captures.len() >= 2,
        "the watcher captured too little to say:\n{calls}"
    );
    for capture in captures {
        assert!(
            capture.contains("history_size"),
            "a screen was read apart from the size of the history: {capture}\n{calls}"
        );
    }
}
