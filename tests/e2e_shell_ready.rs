//! `taw agent start` types the agent's command only once the pane's shell is at its prompt,
//! end to end through the built binary.
//!
//! A shell can ask a question before it gets to its prompt: mise asks "Trust them? Yes No
//! All" when a folder's config is new to it. Whatever is typed then goes to whatever reads
//! the terminal first, and `claude` holds an `a`. The question here is a program of its own
//! that reads the terminal, as mise is, and not the shell; it writes down the line it read so
//! a test can say who got what.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, ServerOptions, named_agent, wait_for, write_script};

/// A fake `claude` that notes its arguments, and a question to be asked before the prompt.
struct Fixture {
    server: Server,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        // The fake reads to tmux as `claude` for as long as it runs, which is what `agent start`
        // takes for the agent being there, with no watcher on the pane to say so.
        let bin = named_agent(
            dir.path(),
            "claude",
            Some(&dir.path().join("claude-argv.txt")),
        );
        write_script(
            &bin.join("ask"),
            &format!(
                "printf 'Trust them? Yes No All '\nread answer\nprintf '%s' \"$answer\" > '{}'\n",
                dir.path().join("answer.txt").display()
            ),
        );
        let server = Server::start_with(ServerOptions {
            path_prefix: Some(bin),
            ..ServerOptions::default()
        });
        Fixture { server, dir }
    }

    /// What the question read, once it has been answered.
    fn answer(&self) -> PathBuf {
        self.dir.path().join("answer.txt")
    }

    /// What the fake `claude` was run with, once it has been.
    fn argv(&self) -> PathBuf {
        self.dir.path().join("claude-argv.txt")
    }

    /// A new pane whose shell asks its question first and is a `/bin/sh` after it.
    fn asking_pane(&self) -> String {
        let bin = self.dir.path().join("bin");
        self.server.tmux_ok(&[
            "set-option",
            "-g",
            "default-command",
            &format!(
                "export PATH={}:$PATH; '{}'; exec /bin/sh",
                bin.display(),
                bin.join("ask").display()
            ),
        ]);
        self.server
            .tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"])
    }

    /// A new pane that is a real zsh, as the user's is, whose startup file runs the question
    /// before the first prompt (mise asks from a hook zsh runs before it draws one). zsh puts a
    /// program it runs in a process group of its own and gives it the terminal, which is not
    /// what a `/bin/sh -c` does; the question is up in a different way than it is in the
    /// other tests, and the shell has to be told not at its prompt in both.
    fn asking_zsh_pane(&self, zdotdir: &Path) -> String {
        self.zsh_pane_asking_after(zdotdir, 0.0)
    }

    /// [`Fixture::asking_zsh_pane`] whose startup file first works for `seconds`, in builtins,
    /// with no program run and nothing drawn. For that long the shell is alone in front of its
    /// terminal, and a shell, and the screen is blank: idle by the process table, and not at a
    /// prompt, with the question it has still to ask coming after.
    fn zsh_pane_asking_after(&self, zdotdir: &Path, seconds: f64) -> String {
        let bin = self.dir.path().join("bin");
        std::fs::create_dir_all(zdotdir).unwrap();
        let silence = if seconds > 0.0 {
            format!(
                "zmodload zsh/datetime\nuntil=$(( EPOCHREALTIME + {seconds} ))\n\
                 while (( EPOCHREALTIME < until )); do :; done\n"
            )
        } else {
            String::new()
        };
        std::fs::write(
            zdotdir.join(".zshrc"),
            format!(
                "{silence}export PATH='{}':$PATH\nPS1='READY> '\n'{}'\n",
                bin.display(),
                bin.join("ask").display()
            ),
        )
        .unwrap();
        self.server
            .tmux_ok(&["set-option", "-g", "default-shell", "/bin/zsh"]);
        // The server's own variable, so the pane's zsh reads this directory's startup files and
        // not the ones in the home directory of whoever runs the suite.
        self.server.tmux_ok(&[
            "set-environment",
            "-g",
            "ZDOTDIR",
            &zdotdir.display().to_string(),
        ]);
        self.server
            .tmux_ok(&["set-option", "-gu", "default-command"]);
        self.server
            .tmux_ok(&["new-window", "-d", "-t", "one:", "-P", "-F", "#{pane_id}"])
    }

    fn asking(&self, pane: &str) -> bool {
        self.server.capture(pane).contains("Trust them?")
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn agent_start_types_nothing_while_the_shell_is_asking_and_the_command_once_it_is_answered() {
    let f = Fixture::start();
    let pane = f.asking_pane();
    agent_start_waits_for_the_question_to_be_answered(&f, &pane, false);
}

/// The same as above in a real zsh. Skipped where there is none: the `/bin/sh` panes of the
/// other tests are what every machine has.
#[test]
fn agent_start_types_nothing_while_a_zsh_is_asking_and_the_command_once_it_is_answered() {
    if !Path::new("/bin/zsh").exists() {
        return;
    }
    let f = Fixture::start();
    let zdotdir = f.dir.path().join("zdotdir");
    let pane = f.asking_zsh_pane(&zdotdir);
    agent_start_waits_for_the_question_to_be_answered(&f, &pane, false);
}

/// The real zsh again, started the moment the pane exists, a second before it asks: its startup
/// works in silence first, and a shell that is alone in front of its terminal, and a shell, and
/// has drawn nothing, is not at its prompt. A wait that took it for one would type into the
/// question that comes after the silence, and fail here.
#[test]
fn agent_start_types_nothing_into_a_zsh_that_works_in_silence_before_it_asks_its_question() {
    if !Path::new("/bin/zsh").exists() {
        return;
    }
    let f = Fixture::start();
    let zdotdir = f.dir.path().join("zdotdir-silent");
    let pane = f.zsh_pane_asking_after(&zdotdir, 1.2);
    agent_start_waits_for_the_question_to_be_answered(&f, &pane, true);
}

/// `agent start` in `pane`, which asks a question; nothing is typed while it is up, and the
/// command is typed once it is answered. `at_once` starts the verb before the question is on
/// the pane, as soon as the pane exists.
fn agent_start_waits_for_the_question_to_be_answered(f: &Fixture, pane: &str, at_once: bool) {
    if !at_once {
        assert!(
            wait_for(|| f.asking(pane), Duration::from_secs(10)),
            "the new pane never showed the question:\n{}",
            f.server.capture(pane)
        );
    }

    std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            f.server
                .taw(&["agent", "start", "rev", "--kind", "claude", "--pane", pane])
        });
        if at_once {
            assert!(
                wait_for(|| f.asking(pane), Duration::from_secs(20)),
                "the new pane never showed the question:\n{}",
                f.server.capture(pane)
            );
        }

        // The question is up, and `agent start` is waiting on it, not typing into it: not at
        // once, and not a little later either.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !f.answer().exists(),
            "the question was answered before anyone did: {:?}",
            read(&f.answer())
        );
        assert!(!f.argv().exists(), "claude ran with the question still up");
        assert!(
            !starting.is_finished(),
            "agent start was done while the shell was still asking: it typed into the question"
        );
        assert_eq!(
            f.server.pane_opt(pane, "@agent_kind_hint"),
            "",
            "the pane was marked as an agent's before anything was typed"
        );

        // Answered, the shell goes on to its prompt, and the command is typed there.
        f.server.tmux_ok(&["send-keys", "-t", pane, "y", "Enter"]);
        let (code, envelope) = starting.join().unwrap();
        assert_eq!(code, 0, "agent start failed: {envelope}");
        assert_eq!(envelope["result"]["agent"]["name"], "rev", "{envelope}");
    });

    assert!(
        wait_for(|| f.argv().exists(), Duration::from_secs(10)),
        "claude was never run once the question was answered"
    );
    assert_eq!(
        read(&f.answer()),
        "y",
        "the question must have read what was typed to answer it, and only that"
    );
    assert_eq!(f.server.pane_opt(pane, "@agent_kind_hint"), "claude");
    assert_eq!(f.server.pane_opt(pane, "@agent_name"), "rev");
}

#[test]
fn agent_start_fails_with_shell_not_ready_and_types_nothing_when_the_prompt_never_comes() {
    let f = Fixture::start();
    let pane = f.asking_pane();
    assert!(
        wait_for(|| f.asking(&pane), Duration::from_secs(10)),
        "the new pane never showed the question:\n{}",
        f.server.capture(&pane)
    );

    // Nobody answers. `--timeout` caps the wait for the shell as it caps the wait for the
    // agent, so the 15 seconds a shell is given do not have to be sat through here.
    let began = std::time::Instant::now();
    let (code, envelope) = f.server.taw(&[
        "agent",
        "start",
        "rev",
        "--kind",
        "claude",
        "--pane",
        &pane,
        "--timeout",
        "2000",
    ]);
    let took = began.elapsed();

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["id"], "cli:agent:start", "{envelope}");
    assert_eq!(envelope["error"]["code"], "shell_not_ready", "{envelope}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&pane)
            && message.contains("prompt")
            && message.contains("nothing was typed"),
        "the message must say which pane, what it was waiting for and that nothing was typed: {message}"
    );
    assert!(
        took >= Duration::from_millis(2000),
        "agent start gave up after {took:?}, before the wait it was given"
    );
    // The wait it was given by `--timeout`, and not the 15 seconds a shell gets otherwise.
    assert!(took < Duration::from_secs(12), "agent start took {took:?}");

    assert!(
        !f.answer().exists(),
        "something was typed into the question: {:?}",
        read(&f.answer())
    );
    assert!(!f.argv().exists());
    assert_eq!(
        f.server.pane_opt(&pane, "@agent_kind_hint"),
        "",
        "a pane nothing was typed into is not marked as holding an agent"
    );
    assert_eq!(f.server.pane_opt(&pane, "@agent_name"), "");
    assert_eq!(f.server.pane_opt(&pane, "@agent_kind"), "");
}

#[test]
fn agent_start_says_the_pane_was_closed_when_it_is_closed_while_the_shell_is_still_asking() {
    let f = Fixture::start();
    let pane = f.asking_pane();
    assert!(
        wait_for(|| f.asking(&pane), Duration::from_secs(10)),
        "the new pane never showed the question:\n{}",
        f.server.capture(&pane)
    );

    let (code, envelope) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| {
            f.server
                .taw(&["agent", "start", "rev", "--kind", "claude", "--pane", &pane])
        });
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!starting.is_finished(), "agent start did not wait");
        f.server.tmux_ok(&["kill-pane", "-t", &pane]);
        starting.join().unwrap()
    });

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "not_found", "{envelope}");
    // On a machine too busy for `agent start` to have got as far as its wait in the time given,
    // it finds the pane already gone, and says that instead; it is the wait that says this.
    let message = envelope["error"]["message"].as_str().unwrap();
    if message.contains("closed") {
        assert!(
            message.contains("nothing was typed"),
            "the message must say that nothing was typed: {message}"
        );
    }
    // The question's own program ended with the pane, having read nothing.
    assert_eq!(
        read(&f.answer()),
        "",
        "something was typed into the question"
    );
    assert!(!f.argv().exists());
}
