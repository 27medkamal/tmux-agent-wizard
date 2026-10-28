//! `session::prompt`: when a pane's shell can be said to be at its prompt, against a real (throwaway)
//! tmux server. Never touches the default tmux server.
//!
//! What is typed at a shell that is still starting goes to whatever reads the terminal first, so
//! `taw session restore` and `taw agent start` look at the pane before they type. The restore and
//! `agent start` tests are in `tmux_session.rs` and `e2e_shell_ready.rs`; these are the looks
//! themselves.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::shell_env::ShellHome;
use taw::session::prompt::{self, AtPrompt, Outcome, Timing, Waiter};
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. See tests/tmux_session.rs
/// for why the socket name has a counter and why `-f /dev/null` and `/bin/sh` are named. Its panes
/// have a home of their own (the second field, a directory that goes with the server) and no
/// history file: a shell that is typed into saves what it was typed when the server is killed,
/// in `$HISTFILE`, which is `~/.bash_history` where nothing says otherwise, and the home it would
/// be in is that of whoever runs the suite.
struct Server(Tmux, ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Tmux::with_socket_name(&format!("taw-test-prompt-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        // The first pane's too: the variables are in the environment of the command that starts the
        // server, which `new-session` makes it from before a `set-environment` could reach the pane.
        home.start_server(
            &t,
            &["-d", "-s", "main", "-x", "200", "-y", "50", "/bin/sh"],
        );
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        t.run_ok(&["set-option", "-g", "exit-empty", "off"])
            .unwrap();
        Server(t, home)
    }

    /// The home its panes have.
    fn home(&self) -> &Path {
        self.1.path()
    }

    /// The pane the server's first window began with.
    fn first_pane(&self) -> String {
        self.0.display("main:0.0", "#{pane_id}").unwrap()
    }

    /// A new window's pane: a `/bin/sh` that has just been started.
    fn pane(&self) -> String {
        self.0
            .run(&["new-window", "-d", "-t", "main:", "-P", "-F", "#{pane_id}"])
            .unwrap()
    }

    /// A new window's pane that asks a question of its own before it becomes a `/bin/sh`, the way
    /// mise does: a program that reads the terminal, writing down what it read.
    fn asking_pane(&self, dir: &Path) -> (String, std::path::PathBuf) {
        let answer = dir.join("answer.txt");
        let ask = dir.join("ask");
        std::fs::write(
            &ask,
            format!(
                "#!/bin/sh\nprintf 'Trust them? Yes No All '\nread answer\nprintf '%s' \"$answer\" > '{}'\n",
                answer.display()
            ),
        )
        .unwrap();
        crate::common::make_executable(&ask);
        let pane = self
            .0
            .run(&[
                "new-window",
                "-d",
                "-t",
                "main:",
                "-P",
                "-F",
                "#{pane_id}",
                &format!("'{}'; exec /bin/sh", ask.display()),
            ])
            .unwrap();
        (pane, answer)
    }

    fn screen(&self, pane: &str) -> String {
        self.0.capture_pane(pane, false).unwrap_or_default()
    }

    fn wait_until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// A bound short enough for a test that expects to be refused.
fn bound(ms: u64) -> Timing {
    Timing {
        bound: Duration::from_millis(ms),
        ..Timing::default()
    }
}

#[test]
fn a_shell_that_has_sat_at_its_prompt_for_a_while_is_ready_on_the_first_look() {
    let s = Server::start();
    let pane = s.pane();
    s.wait_until("the prompt", || !s.screen(&pane).trim().is_empty());
    // tmux says when a window last drew, to the second: a window quiet for the settle and a
    // second more needs no second look to show nothing is moving.
    std::thread::sleep(Duration::from_millis(2500));

    // Ready on the first look: no second look, and so no settle to sit out, which is as much
    // time as a busy machine takes to answer, not a number worth asserting.
    let mut waiter = Waiter::new(&pane, Instant::now(), Timing::default());
    assert_eq!(
        waiter.look(&s.0).unwrap(),
        Some(Outcome::Ready),
        "a pane at its prompt for 2.5 seconds is typed into at once"
    );
}

#[test]
fn a_shell_that_has_just_come_to_its_prompt_is_ready_once_its_screen_has_held_still_for_the_settle()
{
    let s = Server::start();
    let pane = s.pane();

    let began = Instant::now();
    let outcome = prompt::wait(&s.0, &pane, Timing::default()).unwrap();
    let took = began.elapsed();

    assert_eq!(outcome, Outcome::Ready);
    assert!(
        took >= prompt::SETTLE - Duration::from_millis(10),
        "ready after {took:?}, with the settle not over"
    );
    assert!(took < Duration::from_secs(14), "ready after {took:?}");
}

#[test]
fn a_shell_running_a_command_is_not_at_its_prompt() {
    let s = Server::start();
    let pane = s.pane();
    s.0.run_ok(&["send-keys", "-t", &pane, "sleep 30", "Enter"])
        .unwrap();
    s.wait_until("sleep to be the pane's program", || {
        s.0.display(&pane, "#{pane_current_command}")
            .is_ok_and(|command| command == "sleep")
    });

    assert_eq!(
        prompt::wait(&s.0, &pane, bound(800)).unwrap(),
        Outcome::TimedOut
    );
}

#[test]
fn a_program_alone_in_the_terminal_that_is_not_a_shell_is_not_a_prompt() {
    let s = Server::start();
    let pane =
        s.0.run(&[
            "new-window",
            "-d",
            "-t",
            "main:",
            "-P",
            "-F",
            "#{pane_id}",
            "exec cat",
        ])
        .unwrap();
    s.wait_until("cat to be the pane's program", || {
        s.0.display(&pane, "#{pane_current_command}")
            .is_ok_and(|command| command == "cat")
    });

    assert_eq!(
        prompt::wait(&s.0, &pane, bound(800)).unwrap(),
        Outcome::TimedOut
    );
}

#[test]
fn a_shell_asking_a_question_is_not_ready_until_it_is_answered() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let (pane, answer) = s.asking_pane(dir.path());
    s.wait_until("the question", || s.screen(&pane).contains("Trust them?"));

    // Up for as long as it takes: the settle runs out of a still screen many times over.
    assert_eq!(
        prompt::wait(&s.0, &pane, bound(1500)).unwrap(),
        Outcome::TimedOut,
        "a question on a still screen is not a prompt"
    );
    assert!(!answer.exists(), "the wait itself typed something");

    s.0.run_ok(&["send-keys", "-t", &pane, "y", "Enter"])
        .unwrap();
    assert_eq!(
        prompt::wait(&s.0, &pane, Timing::default()).unwrap(),
        Outcome::Ready,
        "answered, the shell comes to its prompt"
    );
    assert_eq!(std::fs::read_to_string(&answer).unwrap(), "y");
}

#[test]
fn a_pane_that_was_closed_is_gone() {
    let s = Server::start();
    let pane = s.pane();
    s.0.run_ok(&["kill-pane", "-t", &pane]).unwrap();

    assert_eq!(
        prompt::wait(&s.0, &pane, bound(800)).unwrap(),
        Outcome::Gone
    );
}

#[test]
fn a_pane_whose_shell_has_ended_is_gone_even_while_tmux_keeps_it() {
    let s = Server::start();
    s.0.run_ok(&["set-option", "-g", "remain-on-exit", "on"])
        .unwrap();
    let pane = s.pane();
    s.0.run_ok(&["send-keys", "-t", &pane, "exit", "Enter"])
        .unwrap();
    s.wait_until("the pane to be dead", || {
        s.0.display(&pane, "#{pane_dead}")
            .is_ok_and(|dead| dead == "1")
    });

    assert_eq!(
        prompt::wait(&s.0, &pane, bound(800)).unwrap(),
        Outcome::Gone
    );
}

#[test]
fn a_server_that_is_not_there_is_an_error_and_not_a_pane_gone() {
    let t = Tmux::with_socket_name(&format!("taw-test-prompt-none-{}", std::process::id()));
    let err = prompt::wait(&t, "%1", bound(800)).unwrap_err();
    assert!(!prompt::is_gone(&err), "{err:#}");
}

#[test]
fn a_deadline_that_has_passed_still_lets_a_pane_that_is_ready_through() {
    let s = Server::start();
    let pane = s.pane();
    s.wait_until("the prompt", || !s.screen(&pane).trim().is_empty());
    std::thread::sleep(Duration::from_millis(2500));

    // Made long ago, so its bound ran out long ago: a restore that came to this pane late
    // (behind others it had to pace) still finds its shell at its prompt.
    let long_ago = Instant::now() - Duration::from_secs(60);
    let mut waiter = Waiter::new(&pane, long_ago, Timing::default());
    assert_eq!(waiter.look(&s.0).unwrap(), Some(Outcome::Ready));
}

/// A `tmux` like `t` whose first answer to a look (`#{pane_dead}` ... `#{pane_current_command}`) names
/// `helper` for what is in front of the pane, as a look that came just before a helper a shell's prompt
/// runs had ended would read it, and is the real tmux's for everything else.
fn tmux_whose_first_look_names_a_helper(t: &Tmux, dir: &Path) -> Tmux {
    let script = dir.join("tmux-naming-a-helper");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             out=$('{real}' \"$@\"); status=$?\n\
             case \"$*\" in\n\
             \x20 *'#{{pane_dead}}'*'#{{pane_current_command}}'*)\n\
             \x20   if mkdir '{dir}/answered' 2>/dev/null; then\n\
             \x20     out=$(printf '%s\\n' \"$out\" | awk -F '\\t' 'BEGIN {{ OFS = \"\\t\" }} {{ $NF = \"helper\"; print }}')\n\
             \x20   fi ;;\n\
             esac\n\
             printf '%s\\n' \"$out\"\n\
             exit $status\n",
            real = t.bin.display(),
            dir = dir.display(),
        ),
    )
    .unwrap();
    crate::common::make_executable(&script);
    Tmux {
        bin: script,
        socket: t.socket.clone(),
    }
}

/// A look reads what tmux names in front of the pane before it asks the process table whether the shell
/// is idle, and again after: a look whose first read named a program that had gone by the second (a
/// helper the prompt ran, an answer just typed to a question) is no look at a shell at rest, even when
/// tmux's clock vouches for the window. The command the wait keeps is the shell's, which a caller that
/// types sends only while it is still in front: kept from the first read, it would be the helper's, and
/// every send would be held back.
#[test]
fn a_look_whose_first_read_named_a_helper_is_no_look_at_a_shell_at_rest() {
    let s = Server::start();
    let pane = s.pane();
    s.wait_until("the prompt", || !s.screen(&pane).trim().is_empty());
    std::thread::sleep(Duration::from_millis(2500));
    let dir = tempfile::tempdir().unwrap();
    let naming = tmux_whose_first_look_names_a_helper(&s.0, dir.path());
    let shell = s.0.display(&pane, "#{pane_current_command}").unwrap();

    let mut waiter = Waiter::new(&pane, Instant::now(), Timing::default());
    assert_eq!(
        waiter.look(&naming).unwrap(),
        None,
        "the look named a helper"
    );
    assert!(
        dir.path().join("answered").exists(),
        "the stand-in never named the helper"
    );
    let mut outcome = None;
    for _ in 0..100 {
        outcome = waiter.look(&naming).unwrap();
        if outcome.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(outcome, Some(Outcome::Ready));
    assert_eq!(waiter.command(), shell);
}

#[test]
fn a_deadline_that_has_passed_gives_up_on_a_pane_that_is_not() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let (pane, _answer) = s.asking_pane(dir.path());
    s.wait_until("the question", || s.screen(&pane).contains("Trust them?"));

    let long_ago = Instant::now() - Duration::from_secs(60);
    let mut waiter = Waiter::new(&pane, long_ago, Timing::default());
    assert_eq!(waiter.look(&s.0).unwrap(), Some(Outcome::TimedOut));
}

// `at_prompt` is the same look for a caller that acts only on a pane that has a prompt now: it does
// not wait on a program, so a shell that is not idle is not at its prompt at once.

#[test]
fn a_shell_that_has_just_come_to_its_prompt_is_at_it_once_its_screen_has_held_still() {
    let s = Server::start();
    let pane = s.pane();

    let began = Instant::now();
    let answer = prompt::at_prompt(&s.0, &pane, Timing::default()).unwrap();
    let took = began.elapsed();

    assert_eq!(answer, AtPrompt::Yes);
    assert!(
        took >= prompt::SETTLE - Duration::from_millis(10),
        "at its prompt after {took:?}, with the settle not over"
    );
}

#[test]
fn a_shell_running_a_command_is_not_at_a_prompt_and_the_look_does_not_wait_on_it() {
    let s = Server::start();
    let pane = s.pane();
    s.0.run_ok(&["send-keys", "-t", &pane, "sleep 30", "Enter"])
        .unwrap();
    s.wait_until("sleep to be the pane's program", || {
        s.0.display(&pane, "#{pane_current_command}")
            .is_ok_and(|command| command == "sleep")
    });

    // A bound far longer than the look takes: a look that waited on the program would use it.
    let began = Instant::now();
    let answer = prompt::at_prompt(&s.0, &pane, bound(8000)).unwrap();

    assert_eq!(answer, AtPrompt::No);
    assert!(
        began.elapsed() < Duration::from_secs(4),
        "the look waited {:?} on a program",
        began.elapsed()
    );
}

#[test]
fn a_program_alone_in_the_terminal_that_is_not_a_shell_is_not_at_a_prompt() {
    let s = Server::start();
    let pane =
        s.0.run(&[
            "new-window",
            "-d",
            "-t",
            "main:",
            "-P",
            "-F",
            "#{pane_id}",
            "exec cat",
        ])
        .unwrap();
    s.wait_until("cat to be the pane's program", || {
        s.0.display(&pane, "#{pane_current_command}")
            .is_ok_and(|command| command == "cat")
    });

    assert_eq!(
        prompt::at_prompt(&s.0, &pane, bound(800)).unwrap(),
        AtPrompt::No
    );
}

#[test]
fn a_shell_asking_a_question_is_not_at_its_prompt_and_the_look_types_nothing() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let (pane, answer) = s.asking_pane(dir.path());
    s.wait_until("the question", || s.screen(&pane).contains("Trust them?"));

    assert_eq!(
        prompt::at_prompt(&s.0, &pane, bound(1500)).unwrap(),
        AtPrompt::No,
        "a question on a still screen is not a prompt"
    );
    assert!(!answer.exists(), "the look itself typed something");
}

#[test]
fn a_pane_that_was_closed_has_no_prompt_to_be_at() {
    let s = Server::start();
    let pane = s.pane();
    s.0.run_ok(&["kill-pane", "-t", &pane]).unwrap();

    assert_eq!(
        prompt::at_prompt(&s.0, &pane, bound(800)).unwrap(),
        AtPrompt::Gone
    );
}

#[test]
fn a_pane_whose_shell_has_ended_has_no_prompt_to_be_at_while_tmux_keeps_it() {
    let s = Server::start();
    s.0.run_ok(&["set-option", "-g", "remain-on-exit", "on"])
        .unwrap();
    let pane = s.pane();
    s.0.run_ok(&["send-keys", "-t", &pane, "exit", "Enter"])
        .unwrap();
    s.wait_until("the pane to be dead", || {
        s.0.display(&pane, "#{pane_dead}")
            .is_ok_and(|dead| dead == "1")
    });

    assert_eq!(
        prompt::at_prompt(&s.0, &pane, bound(800)).unwrap(),
        AtPrompt::Gone
    );
}

#[test]
fn a_server_that_is_not_there_is_an_error_and_not_a_pane_without_a_prompt() {
    let t = Tmux::with_socket_name(&format!("taw-test-prompt-none-{}", std::process::id()));
    let err = prompt::at_prompt(&t, "%1", bound(800)).unwrap_err();
    assert!(!prompt::is_gone(&err), "{err:#}");
}

/// A shell that is typed into keeps what it was typed when its server is killed, in `$HISTFILE`,
/// which is `$HOME/.bash_history` where nothing says otherwise: the home of whoever runs the suite.
#[test]
fn the_panes_of_the_test_server_keep_no_history_in_the_home_of_whoever_runs_the_suite() {
    let s = Server::start();
    let dir = tempfile::tempdir().unwrap();
    let said = dir.path().join("env.txt");
    for pane in [s.pane(), s.first_pane()] {
        let _ = std::fs::remove_file(&said);
        s.0.run_ok(&[
            "send-keys",
            "-t",
            &pane,
            "-l",
            &format!(
                "printf '%s\\n%s\\n' \"$HISTFILE\" \"$HOME\" > '{}.part' && mv '{}.part' '{}'",
                said.display(),
                said.display(),
                said.display()
            ),
        ])
        .unwrap();
        s.0.run_ok(&["send-keys", "-t", &pane, "Enter"]).unwrap();
        s.wait_until("the shell to say where it keeps its history", || {
            said.exists()
        });
        let text = std::fs::read_to_string(&said).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("/dev/null"),
            "the history file of {pane}"
        );
        let home = lines.next().unwrap_or_default();
        assert_eq!(
            Path::new(home),
            s.home(),
            "the home of {pane} is not the one the server was given"
        );
        assert_ne!(
            Some(home),
            std::env::var("HOME").ok().as_deref(),
            "the home of {pane} is the runner's"
        );
    }
}
