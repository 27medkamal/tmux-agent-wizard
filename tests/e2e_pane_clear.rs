// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// These tests follow herdr's `clear_pane_preserves_alternate_screen_and_primary_history` and
// `clear_pane_preserves_wrapped_input_and_unfinished_vt_sequence` (src/pane.rs, v0.9.3), and its
// `api_clear_pane_mutates_endpoint_owned_history` (src/app/api/panes.rs, v0.9.3). herdr
// clears the terminal's own screen, so its tests read the terminal back; these run the built
// binary against a real tmux pane and read the pane back through tmux, with a real shell or a
// program of their own in front. herdr's clear sends the program nothing, where this one types
// Ctrl-L at a shell that is at its prompt, so a program in front logs every byte it is sent.

//! `taw pane clear`, end to end: the built binary against a throwaway tmux server, with a real
//! shell, or a program that logs every byte it reads, in the pane.
//!
//! herdr's clear drops a pane's screen and history, keeps the cursor's line, sends the program
//! nothing, and leaves a full-screen program alone. tmux can clear history but not the screen,
//! and the one way to a clean screen without the program is to ask the shell, so the screen is
//! cleared only where a shell is at its prompt and the history everywhere but on the alternate
//! screen.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Server, ServerOptions, make_executable, wait_for, write_script};
use serde_json::Value;

/// Ends a test that cannot run on this machine, saying why, as the neighbouring tests do.
macro_rules! skip {
    ($($why:tt)*) => {{
        eprintln!("skipping: {}", format!($($why)*));
        return;
    }};
}

/// A fixture: the server, and a directory for the programs and logs of one test.
struct Fixture {
    server: Server,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Fixture {
        Fixture {
            server: Server::start(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// A new window's pane running `command`, which tmux hands to its shell.
    fn pane_running(&self, command: &str) -> String {
        self.server.tmux_ok(&[
            "new-window",
            "-d",
            "-t",
            "one:",
            "-P",
            "-F",
            "#{pane_id}",
            command,
        ])
    }

    /// A program that prints 60 numbered lines, so the pane has history, then does `rest`.
    fn program(&self, name: &str, rest: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        write_script(
            &path,
            &format!("i=0\nwhile [ $i -lt 60 ]; do echo \"line$i\"; i=$((i+1)); done\n{rest}"),
        );
        path
    }

    /// How many lines of history tmux holds for the pane.
    fn history(&self, pane: &str) -> usize {
        self.server
            .display(pane, "#{history_size}")
            .parse()
            .unwrap()
    }

    /// The pane's screen.
    fn screen(&self, pane: &str) -> String {
        self.server.capture(pane)
    }

    /// The pane's history and screen together.
    fn everything(&self, pane: &str) -> String {
        self.server
            .tmux_ok(&["capture-pane", "-p", "-S", "-", "-t", pane])
    }

    fn alternate(&self, pane: &str) -> bool {
        self.server.display(pane, "#{alternate_on}") == "1"
    }

    fn wait_for_screen(&self, pane: &str, needle: &str) {
        assert!(
            wait_for(
                || self.screen(pane).contains(needle),
                Duration::from_secs(15)
            ),
            "{needle:?} never came up on the pane:\n{}",
            self.screen(pane)
        );
    }

    /// `taw pane clear <pane>`, which has to work: its `result`.
    fn clear(&self, pane: &str) -> Value {
        self.server
            .ok(&["pane", "clear", pane], "cli:pane:clear", "ok")
    }
}

/// The prompt every shell here is given, so a screen can be read by it: tmux leaves off a row's
/// trailing space, so a row with nothing typed on it is `READY>`.
const PROMPT: &str = "READY>";

/// The bash binaries to try, the one macOS ships first.
const BASHES: [&str; 4] = [
    "/bin/bash",
    "/usr/bin/bash",
    "/opt/homebrew/bin/bash",
    "/usr/local/bin/bash",
];

fn bash_path() -> Option<&'static str> {
    BASHES.into_iter().find(|path| Path::new(path).exists())
}

impl Fixture {
    /// A pane whose shell is the command `exec`s into, waited for until it shows its prompt.
    fn shell_pane(&self, command: &str) -> String {
        let pane = self.pane_running(command);
        self.wait_for_row(&pane, PROMPT);
        pane
    }

    /// A zsh with none of the user's startup files, or `None` where there is no zsh.
    fn zsh_pane(&self) -> Option<String> {
        if !Path::new("/bin/zsh").exists() {
            return None;
        }
        Some(self.shell_pane(&format!(
            "exec env HOME='{}' PS1='{PROMPT} ' /bin/zsh -f -i",
            self.dir.path().display()
        )))
    }

    /// A pane running `bash` (a path) with none of the user's startup files, reading `inputrc` for
    /// its line editing, which a user's own `~/.inputrc` would otherwise decide: it is vi mode on
    /// the machine this was written on. The name it runs under is the file's.
    fn bash_pane(&self, bash: &str, inputrc: &str) -> String {
        self.shell_pane(&format!(
            "exec env HOME='{}' HISTFILE=/dev/null BASH_SILENCE_DEPRECATION_WARNING=1 \
             INPUTRC='{inputrc}' PS1='{PROMPT} ' '{bash}' --norc --noprofile -i",
            self.dir.path().display()
        ))
    }

    /// A copy of a bash under another name, for a shell that tmux and `ps` call by that name. macOS
    /// kills a copy of a binary it ships on launch, so each bash is tried by running it, and `None`
    /// is where none runs.
    fn bash_copy(&self, name: &str) -> Option<String> {
        let copy = self.dir.path().join(name);
        for bash in BASHES.into_iter().filter(|path| Path::new(path).exists()) {
            if std::fs::copy(bash, &copy).is_err() {
                continue;
            }
            make_executable(&copy);
            let runs = Command::new(&copy)
                .args(["--norc", "--noprofile", "-c", "true"])
                .status()
                .is_ok_and(|status| status.success());
            if runs {
                return Some(copy.display().to_string());
            }
            let _ = std::fs::remove_file(&copy);
        }
        None
    }

    /// Types `text` at the pane as it is, with no Enter.
    fn type_text(&self, pane: &str, text: &str) {
        self.server.tmux_ok(&["send-keys", "-t", pane, "-l", text]);
    }

    fn press(&self, pane: &str, key: &str) {
        self.server.tmux_ok(&["send-keys", "-t", pane, key]);
    }

    /// Waits for a row of the pane's screen that is exactly `row`.
    fn wait_for_row(&self, pane: &str, row: &str) {
        assert!(
            wait_for(
                || self.screen(pane).lines().any(|line| line == row),
                Duration::from_secs(15)
            ),
            "no row of the pane is {row:?}:\n{}",
            self.screen(pane)
        );
    }

    /// Has the shell print 60 numbered lines, so the pane has history, and waits for its prompt.
    fn fill(&self, pane: &str) {
        self.fill_for(pane, PROMPT);
    }

    /// [`Fixture::fill`] for a shell whose prompt row, with nothing typed, is `prompt`.
    fn fill_for(&self, pane: &str, prompt: &str) {
        self.type_text(pane, "for i in $(seq 0 59); do echo \"line$i\"; done");
        self.press(pane, "Enter");
        assert!(
            wait_for(
                || {
                    let screen = self.screen(pane);
                    let rows: Vec<&str> = screen.lines().collect();
                    rows.iter()
                        .position(|row| *row == "line59")
                        .is_some_and(|at| rows.get(at + 1) == Some(&prompt))
                },
                Duration::from_secs(15)
            ),
            "the shell never printed its lines and came back to its prompt:\n{}",
            self.screen(pane)
        );
        assert!(self.history(pane) > 0);
    }

    /// Waits for a screen of nothing but `row` at the top, which is what a shell's redraw leaves.
    fn wait_for_the_top(&self, pane: &str, row: &str) {
        assert!(
            wait_for(
                || {
                    let screen = self.screen(pane);
                    let mut rows = screen.lines();
                    rows.next() == Some(row) && rows.all(str::is_empty)
                },
                Duration::from_secs(10)
            ),
            "the screen is not {row:?} at the top and nothing else:\n{}",
            self.screen(pane)
        );
    }
}

impl Fixture {
    /// A fixture whose server, and so every pane and every command, runs under `locale`.
    fn start_in(locale: &str) -> Fixture {
        Fixture {
            server: Server::start_with(ServerOptions {
                env: vec![
                    ("LC_ALL".to_string(), locale.to_string()),
                    ("LANG".to_string(), locale.to_string()),
                ],
                ..ServerOptions::default()
            }),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// A zsh that reads `rc` as its `.zshrc`, from a directory of its own, and none of the user's
    /// files, or `None` where there is no zsh.
    fn zsh_pane_with_rc(&self, rc: &str) -> Option<String> {
        if !Path::new("/bin/zsh").exists() {
            return None;
        }
        let zdotdir = self.dir.path().join("zdotdir");
        std::fs::create_dir_all(&zdotdir).unwrap();
        std::fs::write(zdotdir.join(".zshrc"), rc).unwrap();
        Some(self.shell_pane(&format!(
            "exec env HOME='{}' ZDOTDIR='{}' /bin/zsh -i",
            self.dir.path().display(),
            zdotdir.display()
        )))
    }

    /// [`Fixture::bash_pane`] with a prompt of `ps1`, waited for until a row is `ready`.
    fn bash_pane_prompt(&self, bash: &str, inputrc: &str, ps1: &str, ready: &str) -> String {
        let pane = self.pane_running(&format!(
            "exec env HOME='{}' HISTFILE=/dev/null BASH_SILENCE_DEPRECATION_WARNING=1 \
             INPUTRC='{inputrc}' PS1='{ps1}' '{bash}' --norc --noprofile -i",
            self.dir.path().display()
        ));
        self.wait_for_row(&pane, ready);
        pane
    }

    /// A readline config for bash in vi mode, whose insert mode takes Ctrl-L for text.
    fn vi_inputrc(&self) -> String {
        let inputrc = self.dir.path().join("vi.inputrc");
        std::fs::write(&inputrc, "set editing-mode vi\n").unwrap();
        inputrc.display().to_string()
    }
}

/// A UTF-8 locale this machine has, for a test with wide characters in it.
fn utf8_locale() -> Option<String> {
    let listed = Command::new("locale").arg("-a").output().ok()?;
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    ["en_US.UTF-8", "C.UTF-8", "en_US.utf8", "C.utf8"]
        .into_iter()
        .find(|wanted| listed.lines().any(|line| line.trim() == *wanted))
        .map(str::to_string)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A program that logs every byte it is sent, in a pane: the terminal raw, so nothing waits for
/// a newline and nothing is echoed. `LOGGING` is on the screen once it reads.
fn logger(f: &Fixture, name: &str, log: &Path) -> PathBuf {
    f.program(
        name,
        &format!(
            "stty raw -echo\nprintf 'LOGGING\\r\\n'\nexec cat >> '{}'\n",
            log.display()
        ),
    )
}

#[test]
fn clear_leaves_a_full_screen_program_alone_and_clears_the_history_under_it_once_it_leaves() {
    let f = Fixture::start();
    let flag = f.dir.path().join("leave");
    let program = f.program(
        "fullscreen",
        &format!(
            "printf '\\033[?1049h\\033[2J\\033[Halt app text'\n\
             while [ ! -e '{}' ]; do sleep 0.1; done\n\
             printf '\\033[?1049l'\nexec cat\n",
            flag.display()
        ),
    );
    let pane = f.pane_running(&format!("exec '{}'", program.display()));
    f.wait_for_screen(&pane, "alt app text");
    assert!(f.alternate(&pane), "the program is on the alternate screen");
    let history = f.history(&pane);
    assert!(history > 0, "the pane has history under the program");
    let (screen, everything) = (f.screen(&pane), f.everything(&pane));

    // On the alternate screen nothing is touched: not the screen, and not the history of the
    // screen the program left behind, which is what herdr's own test asserts.
    let result = f.clear(&pane);
    assert_eq!(result["history_cleared"], false, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("alternate screen"),
        "{result}"
    );
    assert!(f.alternate(&pane));
    assert_eq!(f.history(&pane), history, "the history is as it was");
    assert_eq!(f.screen(&pane), screen);
    assert_eq!(f.everything(&pane), everything);

    // The program leaves, and what was under it is there again: now the history goes.
    std::fs::write(&flag, "").unwrap();
    assert!(
        wait_for(|| !f.alternate(&pane), Duration::from_secs(15)),
        "the program never left the alternate screen"
    );
    assert!(f.screen(&pane).contains("line59"), "{}", f.screen(&pane));
    assert!(f.history(&pane) > 0);
    let screen = f.screen(&pane);

    let result = f.clear(&pane);
    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(f.history(&pane), 0);
    assert_eq!(
        f.screen(&pane),
        screen,
        "a program in front keeps its screen"
    );
    assert!(
        !f.everything(&pane).lines().any(|line| line == "line0"),
        "the old lines are gone from the history:\n{}",
        f.everything(&pane)
    );
}

#[test]
fn clear_empties_the_history_of_a_pane_with_a_program_in_front_and_sends_it_nothing() {
    let f = Fixture::start();
    let log = f.dir.path().join("keys.log");
    let program = logger(&f, "logger", &log);
    let pane = f.pane_running(&format!("exec '{}'", program.display()));
    f.wait_for_screen(&pane, "LOGGING");

    // A byte the program is known to log, so that a log that is empty after the clear says
    // nothing was sent, and not that the program reads nothing.
    f.server.tmux_ok(&["send-keys", "-t", &pane, "-l", "x"]);
    assert!(
        wait_for(|| read(&log) == "x", Duration::from_secs(15)),
        "the program never logged the byte it was sent: {:?}",
        read(&log)
    );
    assert!(f.history(&pane) > 0);
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(result["note"].is_string(), "{result}");
    assert_eq!(f.history(&pane), 0, "the history is gone");
    assert_eq!(f.screen(&pane), screen, "the screen stays");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(read(&log), "x", "the program was sent nothing");
}

#[test]
fn clear_prints_the_usual_envelope_and_names_the_pane_by_its_public_id() {
    let f = Fixture::start();
    let pane = f.server.pane();
    let public = f.server.public_pane_id(&pane);

    let (code, envelope) = f.server.taw(&["pane", "clear", &pane]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(envelope["id"], "cli:pane:clear");
    assert!(envelope.get("error").is_none(), "{envelope}");
    assert_eq!(envelope["result"]["type"], "ok");
    assert_eq!(envelope["result"]["pane_id"], public.as_str());
    assert!(envelope["result"]["history_cleared"].is_boolean());
    assert!(envelope["result"]["screen_cleared"].is_boolean());
}

#[test]
fn clear_takes_the_pane_it_runs_in_when_none_is_named() {
    let f = Fixture::start();
    let program = f.program("busy", "exec cat\n");
    let command = format!("exec '{}'", program.display());
    let (first, second) = (f.pane_running(&command), f.pane_running(&command));
    for pane in [&first, &second] {
        f.wait_for_screen(pane, "line59");
        assert!(f.history(pane) > 0);
    }

    let (code, envelope) = f.server.taw_in_pane(&second, &["pane", "clear"]);

    assert_eq!(code, 0, "{envelope}");
    assert_eq!(
        envelope["result"]["pane_id"],
        f.server.public_pane_id(&second).as_str()
    );
    assert_eq!(f.history(&second), 0, "the pane it ran in is cleared");
    assert!(f.history(&first) > 0, "and no other");
}

#[test]
fn clear_of_a_pane_that_is_not_there_is_not_found_and_exits_one() {
    let f = Fixture::start();

    let (code, envelope) = f.server.taw(&["pane", "clear", "w9:p99"]);

    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["id"], "cli:pane:clear");
    assert_eq!(envelope["error"]["code"], "not_found", "{envelope}");
}

/// The shell in `pane` has printed lines and has `echo kept` typed at its prompt: clearing it
/// redraws the prompt and the line at the top, and the history goes.
fn redraws_at_the_top(f: &Fixture, pane: &str) {
    f.fill(pane);
    f.type_text(pane, "echo kept");
    f.wait_for_row(pane, "READY> echo kept");

    let result = f.clear(pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], true, "{result}");
    assert!(result.get("note").is_none(), "{result}");
    assert_eq!(
        f.history(pane),
        0,
        "the old screen did not stay in the history"
    );
    f.wait_for_the_top(pane, "READY> echo kept");
    // The old screen is not in the history after the redraw either: tmux moves it there when the
    // shell clears it, and a history emptied before that is full again.
    assert_eq!(
        f.history(pane),
        0,
        "the redraw left the old screen in the history"
    );

    // The line is still the shell's own, as it was typed.
    f.press(pane, "Enter");
    f.wait_for_row(pane, "kept");
}

/// The shell in `pane` is one `taw pane clear` types nothing at: the same, and the screen, the
/// line and the history stay as they were, bar the history that goes.
fn leaves_the_screen_alone(f: &Fixture, pane: &str) {
    f.fill(pane);
    f.type_text(pane, "echo kept");
    f.wait_for_row(pane, "READY> echo kept");
    let screen = f.screen(pane);

    let result = f.clear(pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(result["note"].is_string(), "{result}");
    assert_eq!(f.history(pane), 0);
    // A key typed at the shell would have been drawn by now.
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(f.screen(pane), screen);
    f.press(pane, "Enter");
    f.wait_for_row(pane, "kept");
}

#[test]
fn clear_redraws_a_zsh_at_its_prompt_at_the_top_and_keeps_the_line_being_typed() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    redraws_at_the_top(&f, &pane);
}

#[test]
fn clear_redraws_a_zsh_when_tmux_does_not_scroll_a_cleared_screen_into_the_history() {
    let f = Fixture::start();
    // Older tmux has no such option and always scrolls, which only makes the evidence plentier.
    let _ = f
        .server
        .tmux(&["set-option", "-g", "scroll-on-clear", "off"]);
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    // The history does not grow when the shell clears, so the verb has the cursor's row moving up
    // the screen to go by.
    redraws_at_the_top(&f, &pane);
}

#[test]
fn clear_redraws_a_zsh_in_vi_mode_as_well() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.type_text(&pane, "bindkey -v");
    f.press(&pane, "Enter");
    f.wait_for_row(&pane, PROMPT);
    redraws_at_the_top(&f, &pane);
}

#[test]
fn clear_redraws_a_bash_at_its_prompt_at_the_top_and_keeps_the_line_being_typed() {
    let f = Fixture::start();
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    let pane = f.bash_pane(bash, "/dev/null");
    redraws_at_the_top(&f, &pane);
}

#[test]
fn clear_keeps_a_command_line_that_wraps_over_two_rows() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.fill(&pane);
    // 112 columns on an 80 column pane: the shell breaks the line itself, so it is two rows.
    let line = format!("echo {}", "a".repeat(100));
    let both = format!("{PROMPT} {line}");
    let (head, tail) = both.split_at(80);
    f.type_text(&pane, &line);
    f.wait_for_row(&pane, tail);

    f.clear(&pane);

    assert_eq!(f.history(&pane), 0);
    assert!(
        wait_for(
            || {
                let screen = f.screen(&pane);
                let mut rows = screen.lines();
                rows.next() == Some(head) && rows.next() == Some(tail) && rows.all(str::is_empty)
            },
            Duration::from_secs(10)
        ),
        "the whole line, over both its rows, is not at the top and alone:\n{}",
        f.screen(&pane)
    );
    // And it is one line to the shell: it runs as it was typed. The echo wraps at the pane's
    // edge, which the terminal does, so what it prints is 80 and then 20.
    f.press(&pane, "Enter");
    assert!(
        wait_for(
            || {
                let screen = f.screen(&pane);
                let rows: Vec<&str> = screen.lines().collect();
                rows.windows(2)
                    .any(|pair| pair[0] == "a".repeat(80) && pair[1] == "a".repeat(20))
            },
            Duration::from_secs(15)
        ),
        "the line did not run as it was typed:\n{}",
        f.screen(&pane)
    );
}

#[test]
fn clear_empties_the_history_after_a_slow_shell_has_redrawn_and_not_before() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    // The shell answers Ctrl-L after most of a second. tmux moves the old screen into the history
    // when the shell clears it, so a history emptied before that is full again.
    f.type_text(
        &pane,
        "slow() { sleep 0.8; zle clear-screen }; zle -N slow; bindkey '^L' slow",
    );
    f.press(&pane, "Enter");
    f.wait_for_row(&pane, PROMPT);
    f.fill(&pane);

    let began = Instant::now();
    let result = f.clear(&pane);
    let took = began.elapsed();

    assert_eq!(result["screen_cleared"], true, "{result}");
    assert!(
        took >= Duration::from_millis(700),
        "the verb came back after {took:?}, before the shell had answered"
    );
    f.wait_for_the_top(&pane, PROMPT);
    assert_eq!(
        f.history(&pane),
        0,
        "the slow redraw left the screen in the history"
    );
}

#[test]
fn clear_takes_a_pane_out_of_copy_mode_and_redraws_the_shell_under_it() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.fill(&pane);
    f.server.tmux_ok(&["copy-mode", "-t", &pane]);
    f.server
        .tmux_ok(&["send-keys", "-t", &pane, "-X", "page-up"]);
    assert_eq!(f.server.display(&pane, "#{pane_in_mode}"), "1");

    let result = f.clear(&pane);

    assert_eq!(result["screen_cleared"], true, "{result}");
    assert_eq!(f.server.display(&pane, "#{pane_in_mode}"), "0");
    assert_eq!(f.history(&pane), 0);
    f.wait_for_the_top(&pane, PROMPT);
}

#[test]
fn clear_redraws_a_shell_named_fish_and_sends_the_ones_named_sh_dash_and_ksh_nothing() {
    let f = Fixture::start();
    // Every one of these is a bash with its line editing, so a key typed at one would clear its
    // screen: what tells them apart is the name tmux and `ps` know them by. A plain `sh` has no
    // line editing to answer Ctrl-L with, so it is not sent one.
    let names = ["fish", "sh", "dash", "ksh"];
    let copies: Vec<Option<String>> = names.iter().map(|name| f.bash_copy(name)).collect();
    for (name, copy) in names.iter().zip(copies) {
        let Some(copy) = copy else {
            skip!("no bash on this machine runs under another name")
        };
        let pane = f.bash_pane(&copy, "/dev/null");
        assert_eq!(f.server.display(&pane, "#{pane_current_command}"), *name);
        if *name == "fish" {
            redraws_at_the_top(&f, &pane);
        } else {
            leaves_the_screen_alone(&f, &pane);
        }
    }
}

#[test]
fn clear_takes_back_the_character_a_bash_in_vi_mode_takes_ctrl_l_for() {
    let f = Fixture::start();
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    // In readline's vi insert mode Ctrl-L is not clear-screen: it inserts a ^L into the line.
    let pane = f.bash_pane(bash, &f.vi_inputrc());
    f.fill(&pane);
    f.type_text(&pane, "echo kept");
    f.wait_for_row(&pane, "READY> echo kept");
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(
        result["note"].as_str().unwrap().contains("as text"),
        "{result}"
    );
    assert_eq!(f.history(&pane), 0);
    // The line is back as it was typed, with no ^L in it, and it runs as typed.
    assert_eq!(f.screen(&pane), screen);
    f.press(&pane, "Enter");
    f.wait_for_row(&pane, "kept");
}

#[test]
fn clear_says_when_a_backspace_did_not_take_the_ctrl_l_it_was_sent_out_of_a_bash_line() {
    let f = Fixture::start();
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    let pane = f.bash_pane(bash, &f.vi_inputrc());
    f.fill(&pane);
    f.type_text(&pane, "echo kept");
    f.wait_for_row(&pane, "READY> echo kept");
    // A tmux that drops the Backspace the verb sends: the ^L the shell took for text stays, as it
    // would for a shell that does nothing with the key.
    let tmux = taw::tmux::Tmux::with_socket_name(f.server.socket()).bin;
    let shim = f.dir.path().join("tmux-shim");
    write_script(
        &shim,
        &format!(
            "case \"$*\" in\n*send-keys*BSpace*) exit 0 ;;\nesac\nexec '{}' \"$@\"\n",
            tmux.display()
        ),
    );

    let began = Instant::now();
    let (code, envelope) = f.server.taw_with_env(
        &["pane", "clear", &pane],
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );
    let took = began.elapsed();

    assert_eq!(code, 0, "{envelope}");
    let result = &envelope["result"];
    assert_eq!(result["history_cleared"], true, "{envelope}");
    assert_eq!(result["screen_cleared"], false, "{envelope}");
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("did not take it out"),
        "{envelope}"
    );
    assert!(
        took < Duration::from_secs(10),
        "the verb waited {took:?} for a ^L that was not going to go"
    );
    assert!(
        f.screen(&pane).contains("^L"),
        "the ^L is not there to be told of:\n{}",
        f.screen(&pane)
    );
}

#[test]
fn clear_sends_a_script_that_bash_runs_in_front_of_a_zsh_nothing() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    let log = f.dir.path().join("keys.log");
    // Not `exec`ed: bash stays in front, as the program tmux calls `bash`, though it is not the
    // pane's own shell. The zsh is.
    let script = f.program(
        "keys",
        &format!(
            "stty raw -echo\nprintf 'LOGGING\\r\\n'\ncat >> '{}'\n",
            log.display()
        ),
    );
    f.type_text(&pane, &format!("'{bash}' '{}'", script.display()));
    f.press(&pane, "Enter");
    f.wait_for_screen(&pane, "LOGGING");
    assert_eq!(f.server.display(&pane, "#{pane_current_command}"), "bash");
    f.type_text(&pane, "x");
    assert!(
        wait_for(|| read(&log) == "x", Duration::from_secs(15)),
        "the script never logged the byte it was sent: {:?}",
        read(&log)
    );
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert_eq!(f.history(&pane), 0);
    assert_eq!(f.screen(&pane), screen);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(read(&log), "x", "the script was sent a key");
}

#[test]
fn clear_sends_a_zsh_that_is_running_a_command_nothing_that_it_would_act_on_later() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.fill(&pane);
    f.type_text(&pane, "sleep 3");
    f.press(&pane, "Enter");
    assert!(
        wait_for(
            || f.server.display(&pane, "#{pane_current_command}") == "sleep",
            Duration::from_secs(15)
        ),
        "sleep never took the terminal"
    );
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["screen_cleared"], false, "{result}");
    assert_eq!(f.history(&pane), 0);
    assert_eq!(f.screen(&pane), screen);
    // A key typed at the terminal while the command ran would wait for the shell and clear its
    // screen the moment the prompt came back.
    f.wait_for_row(&pane, "READY> sleep 3");
    assert!(
        wait_for(
            || f.server.display(&pane, "#{pane_current_command}") == "zsh",
            Duration::from_secs(15)
        ),
        "sleep never ended"
    );
    f.wait_for_screen(&pane, "line59");
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        f.screen(&pane).contains("line59"),
        "the shell cleared its screen when the command was over:\n{}",
        f.screen(&pane)
    );
}

#[test]
fn clear_leaves_a_shell_that_is_on_the_alternate_screen_alone() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.fill(&pane);
    f.type_text(&pane, "printf '\\033[?1049h\\033[2J\\033[Halt app text'");
    f.press(&pane, "Enter");
    assert!(
        wait_for(|| f.alternate(&pane), Duration::from_secs(15)),
        "the pane never went to the alternate screen"
    );
    f.wait_for_screen(&pane, "alt app text");
    // The shell is at its prompt again, under the text: a key typed at it would clear the screen.
    f.wait_for_row(&pane, PROMPT);
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], false, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    std::thread::sleep(Duration::from_millis(600));
    assert!(f.alternate(&pane));
    assert_eq!(f.screen(&pane), screen);
}

#[test]
fn clear_sends_no_key_to_a_pane_whose_window_copies_keys_to_the_others() {
    let f = Fixture::start();
    let Some(shell) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    let log = f.dir.path().join("keys.log");
    let program = logger(&f, "logger", &log);
    let beside = f.server.tmux_ok(&[
        "split-window",
        "-d",
        "-t",
        &shell,
        "-P",
        "-F",
        "#{pane_id}",
        &format!("exec '{}'", program.display()),
    ]);
    f.wait_for_screen(&beside, "LOGGING");
    // With synchronize-panes on, tmux hands a key sent to one pane to every pane of the window, so
    // a Ctrl-L for the shell would reach the program beside it. What is typed at the shell here is
    // copied to the program too, which is how the test knows the program reads what it is sent.
    f.server
        .tmux_ok(&["set-option", "-w", "-t", &shell, "synchronize-panes", "on"]);
    f.fill(&shell);
    f.type_text(&shell, "echo kept");
    f.wait_for_row(&shell, "READY> echo kept");
    assert!(
        wait_for(
            || read(&log).ends_with("echo kept"),
            Duration::from_secs(15)
        ),
        "the program never logged the keys typed beside it: {:?}",
        read(&log)
    );
    let read_before = read(&log);
    let screen = f.screen(&shell);

    let result = f.clear(&shell);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(
        result["note"].as_str().unwrap().contains("synchronized"),
        "{result}"
    );
    assert_eq!(f.history(&shell), 0);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(f.screen(&shell), screen, "the shell's screen stays");
    assert_eq!(
        read(&log),
        read_before,
        "the program beside it was sent a key"
    );
}

/// A ^L taken as text is told by the ^L itself, wherever it lands, and not by where the cursor
/// went: these are the places the cursor and the rows give no help.
#[test]
fn clear_takes_a_form_feed_back_out_of_a_zsh_that_is_reading_with_the_cursor_on_the_last_row() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.fill(&pane);
    // `read` waits at the terminal with the cursor on a new row at the bottom of the screen, which
    // has nothing on it. It is a builtin, so the shell passes for one at its prompt, and a Ctrl-L
    // typed at it is echoed as ^L and read as a form feed.
    f.type_text(&pane, "read x; print -r -- \"x=[$x]\"");
    f.press(&pane, "Enter");
    assert!(
        wait_for(
            || f.server.display(&pane, "#{cursor_y}") == "23"
                && f.server.display(&pane, "#{cursor_x}") == "0",
            Duration::from_secs(15)
        ),
        "the cursor is not on the blank last row:\n{}",
        f.screen(&pane)
    );

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(
        result["note"].as_str().unwrap().contains("as text"),
        "{result}"
    );
    assert!(
        !f.screen(&pane).contains("^L"),
        "the ^L is still on the screen:\n{}",
        f.screen(&pane)
    );
    // What `read` reads is what was typed, with no form feed in front of it.
    f.type_text(&pane, "abc");
    f.press(&pane, "Enter");
    f.wait_for_row(&pane, "x=[abc]");
}

/// bash in vi mode with a line that runs to the right edge of an 80 column pane: readline wraps
/// it, and the screen scrolls, when the two columns of ^L do not fit.
fn takes_back_a_line_that_runs_to_the_edge(a: usize) {
    let f = Fixture::start();
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    let pane = f.bash_pane(bash, &f.vi_inputrc());
    f.fill(&pane);
    let line = format!("echo {}", "a".repeat(a));
    let typed = format!("{PROMPT} {line}");
    f.type_text(&pane, &line);
    f.wait_for_row(&pane, &typed);
    let column = typed.chars().count();

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(
        result["screen_cleared"], false,
        "a line that ends at column {column}: {result}"
    );
    assert!(
        result["note"].as_str().unwrap().contains("as text"),
        "{result}"
    );
    assert!(
        !f.screen(&pane).contains("^L"),
        "the ^L is still on the screen:\n{}",
        f.screen(&pane)
    );
    assert!(
        f.screen(&pane).lines().any(|row| row == typed),
        "the line is not back as it was typed:\n{}",
        f.screen(&pane)
    );
    f.press(&pane, "Enter");
    f.wait_for_row(&pane, &"a".repeat(a));
}

#[test]
fn clear_takes_back_the_character_a_bash_in_vi_mode_took_for_text_at_column_78() {
    takes_back_a_line_that_runs_to_the_edge(66);
}

#[test]
fn clear_takes_back_the_character_a_bash_in_vi_mode_took_for_text_at_column_79() {
    takes_back_a_line_that_runs_to_the_edge(67);
}

#[test]
fn clear_takes_back_the_character_a_bash_in_vi_mode_took_for_text_after_wide_characters() {
    let Some(bash) = bash_path() else {
        skip!("there is no bash at any of {BASHES:?}")
    };
    let Some(locale) = utf8_locale() else {
        skip!("this machine lists no UTF-8 locale")
    };
    let f = Fixture::start_in(&locale);
    let inputrc = f.vi_inputrc();
    // Wide characters before the cursor, in what is typed and in the prompt: a column is not a
    // character.
    for (ps1, ready, typed) in [
        ("READY> ", "READY>", "echo \u{65e5}\u{672c}"),
        ("\u{1f680} ", "\u{1f680}", "echo hi"),
    ] {
        let pane = f.bash_pane_prompt(bash, &inputrc, ps1, ready);
        f.fill_for(&pane, ready);
        let row = format!("{ready} {typed}");
        f.type_text(&pane, typed);
        f.wait_for_row(&pane, &row);

        let result = f.clear(&pane);

        assert_eq!(result["screen_cleared"], false, "{ready}: {result}");
        assert!(
            result["note"].as_str().unwrap().contains("as text"),
            "{result}"
        );
        assert!(
            !f.screen(&pane).contains("^L"),
            "{ready}: the ^L is still on the screen:\n{}",
            f.screen(&pane)
        );
        assert!(
            f.screen(&pane).lines().any(|line| line == row),
            "{ready}: the line is not back as it was typed:\n{}",
            f.screen(&pane)
        );
    }
}

/// With scroll-on-clear off a Ctrl-L at a screen that is clear already shows nothing: no change to
/// wait for. The prompt at the top of an empty screen is what a clear leaves, so there is nothing
/// to ask the shell for.
#[test]
fn clear_of_a_pane_that_is_clear_already_sends_no_key_and_does_not_wait_for_an_answer() {
    let f = Fixture::start();
    // Older tmux has no such option and always scrolls, which only makes the test easier to pass.
    let _ = f
        .server
        .tmux(&["set-option", "-g", "scroll-on-clear", "off"]);
    let log = f.dir.path().join("ctrl-l.log");
    let rc = format!(
        "PS1='{PROMPT} '\nlogl() {{ print x >> '{}'; zle clear-screen }}\nzle -N logl\nbindkey '^L' logl\n",
        log.display()
    );
    let Some(pane) = f.zsh_pane_with_rc(&rc) else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.wait_for_the_top(&pane, PROMPT);

    let began = Instant::now();
    let result = f.clear(&pane);
    let took = began.elapsed();

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], true, "{result}");
    assert!(result.get("note").is_none(), "{result}");
    assert!(
        took < Duration::from_millis(2500),
        "the verb waited {took:?} for a shell to answer a key that changes nothing"
    );
    assert_eq!(
        read(&log),
        "",
        "a Ctrl-L was typed at a screen that was clear already"
    );
}

#[test]
fn clear_clears_the_history_of_a_pane_kept_after_its_program_ended_and_leaves_the_rest() {
    let f = Fixture::start();
    f.server
        .tmux_ok(&["set-option", "-g", "remain-on-exit", "on"]);
    let program = f.program("ended", "exit 0\n");
    let pane = f.pane_running(&format!("exec '{}'", program.display()));
    assert!(
        wait_for(
            || f.server.display(&pane, "#{pane_dead}") == "1",
            Duration::from_secs(15)
        ),
        "the pane never ended"
    );
    f.wait_for_screen(&pane, "line59");
    assert!(f.history(&pane) > 0);
    let screen = f.screen(&pane);

    let result = f.clear(&pane);

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(result["note"].is_string(), "{result}");
    assert_eq!(f.history(&pane), 0);
    assert_eq!(f.screen(&pane), screen);
    assert_eq!(f.server.display(&pane, "#{pane_dead}"), "1");
}

#[test]
fn clear_gives_up_on_a_zsh_that_ignores_ctrl_l_within_a_few_seconds_and_says_so() {
    let f = Fixture::start();
    let Some(pane) = f.zsh_pane() else {
        skip!("there is no zsh at /bin/zsh")
    };
    f.type_text(&pane, "bindkey '^L' undefined-key");
    f.press(&pane, "Enter");
    f.fill(&pane);
    let screen = f.screen(&pane);

    let began = Instant::now();
    let result = f.clear(&pane);
    let took = began.elapsed();

    assert_eq!(result["history_cleared"], true, "{result}");
    assert_eq!(result["screen_cleared"], false, "{result}");
    assert!(
        result["note"].as_str().unwrap().contains("did not answer"),
        "{result}"
    );
    assert!(
        took < Duration::from_secs(8),
        "the verb waited {took:?} for a shell that was never going to answer"
    );
    assert_eq!(f.history(&pane), 0);
    assert_eq!(f.screen(&pane), screen);
}

/// A tmux that, before the first command naming clear-history, has the pane's program go to the
/// alternate screen (`cat` writes back the escape sequence it is typed) and waits until tmux shows
/// it there: the program goes between the verb's look at the pane and its clear.
#[test]
fn clear_leaves_the_history_alone_of_a_pane_that_goes_to_the_alternate_screen_just_before_the_clear()
 {
    let f = Fixture::start();
    let program = f.program("late", "exec cat\n");
    let pane = f.pane_running(&format!("exec '{}'", program.display()));
    f.wait_for_screen(&pane, "line59");
    let history = f.history(&pane);
    assert!(history > 0);
    let tmux = taw::tmux::Tmux::with_socket_name(f.server.socket()).bin;
    let shim = f.dir.path().join("tmux-shim");
    write_script(
        &shim,
        &format!(
            "case \"$*\" in\n\
             *clear-history*)\n\
             if [ ! -e '{triggered}' ]; then\n\
             : > '{triggered}'\n\
             '{tmux}' -L '{socket}' send-keys -t '{pane}' -l \"$(printf '\\033[?1049h')alt app text\"\n\
             '{tmux}' -L '{socket}' send-keys -t '{pane}' Enter\n\
             n=0\n\
             while [ \"$n\" -lt 200 ]; do\n\
             [ \"$('{tmux}' -L '{socket}' display-message -p -t '{pane}' '#{{alternate_on}}')\" = 1 ] && break\n\
             sleep 0.05; n=$((n + 1))\n\
             done\n\
             fi ;;\n\
             esac\n\
             exec '{tmux}' \"$@\"\n",
            triggered = f.dir.path().join("triggered").display(),
            tmux = tmux.display(),
            socket = f.server.socket(),
        ),
    );

    let (code, envelope) = f.server.taw_with_env(
        &["pane", "clear", &pane],
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{envelope}");
    let result = &envelope["result"];
    assert!(
        f.alternate(&pane),
        "the program never reached the alternate screen"
    );
    assert_eq!(result["history_cleared"], false, "{envelope}");
    assert_eq!(result["screen_cleared"], false, "{envelope}");
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("alternate screen"),
        "{envelope}"
    );
    // The line the shim typed scrolled the screen once before the program left it for the alternate
    // screen, so the history is what it was and a line more: not what a clear leaves.
    assert!(
        f.history(&pane) >= history,
        "the history under the program was cleared: {} lines of {history}",
        f.history(&pane)
    );
}

/// A shell the harness starts and that is typed into keeps what it was typed when the server is
/// killed, in `$HISTFILE`: the runner's own `~/.bash_history` where nothing says otherwise.
#[test]
fn the_harness_gives_the_shells_it_starts_a_history_file_of_dev_null() {
    let f = Fixture::start();
    let said = f.dir.path().join("histfile.txt");
    let first = f.server.pane();
    let later = f.pane_running("exec /bin/sh");
    for pane in [first, later] {
        let _ = std::fs::remove_file(&said);
        // Both shells are fresh, and the harness's own prompt is not in front: a line typed before the
        // shell reads its terminal is cut at the terminal's line buffer.
        f.server.wait_for_prompt(&pane);
        f.type_text(
            &pane,
            &format!(
                "echo \"$HISTFILE\" > '{}.part' && mv '{}.part' '{}'",
                said.display(),
                said.display(),
                said.display()
            ),
        );
        f.press(&pane, "Enter");
        assert!(
            wait_for(|| said.exists(), Duration::from_secs(15)),
            "the shell in {pane} never said where it keeps its history"
        );
        assert_eq!(
            read(&said).trim(),
            "/dev/null",
            "the history file of {pane}"
        );
    }
}
