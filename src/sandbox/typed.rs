//! The line taw types into a pane to start an agent in its sandbox: the `taw sandbox` a person
//! types, so the pane's history shows how its agent was started, and up arrow then Enter starts it
//! again. Restore and `agent start --sandbox` type it, each once the pane's
//! shell is at its prompt.
//!
//! The pane's shell reads it, and that is often zsh: each word is written so that sh, bash 3.2 and 5
//! and zsh all read it back as the one word it is ([`word`]), never with bash's `printf %q`, which
//! leaves a leading `~` bare in bash 3.2, and a leading `=` in both, which zsh reads as a command's
//! path.

use std::path::{Path, PathBuf};

use crate::tmux::Tmux;

/// `<taw> sandbox <agent>`, then `--profile <name>` when one is given, then `--` and the agent's own
/// arguments when it has some, each word written so the pane's shell reads it as the one argument it
/// is ([`word`]). `taw` is written as [`taw_word`] gives it, already fit for a shell.
pub fn line(taw: &str, agent: &str, profile: Option<&str>, args: &[String]) -> String {
    let mut words = vec![taw.to_string(), "sandbox".to_string(), word(agent)];
    if let Some(profile) = profile {
        words.push("--profile".to_string());
        words.push(word(profile));
    }
    if !args.is_empty() {
        words.push("--".to_string());
        words.extend(args.iter().map(|arg| word(arg)));
    }
    words.join(" ")
}

/// `text` as one word of a line typed at a shell's prompt: as it is when it is made of letters, digits
/// and `_ - . / : @ % +`, which no shell expands, and in single quotes otherwise, with a quote in it
/// written `'\''`. A key a line editor acts on is written as an escape after the quote closes, a newline
/// `'$'\n''`, a tab `'$'\t''` and the rest by their number (`'$'\033''`): typed as itself, a newline
/// would end the line, a tab would ask for completions, and the others would move the cursor or delete
/// what came before. So the line is one line of plain text, which bash 3.2 and 5, zsh and ksh all
/// read back as these very words; dash, the `sh` of Debian and Ubuntu, has no `$'...'` and reads a
/// word with a control character in it wrong.
///
/// It is taw's one rule for a word of any line it types into a shell: restore's resume command
/// (`session::restore::resume_command`) and an agent's reported resume command
/// ([`crate::agent_resume::command_line`]) are written by it too.
pub fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+')
        });
    if plain {
        return text.to_string();
    }
    let mut quoted = String::from("'");
    for c in text.chars() {
        match c {
            '\'' => quoted.push_str("'\\''"),
            '\n' => quoted.push_str("'$'\\n''"),
            '\t' => quoted.push_str("'$'\\t''"),
            c if c.is_ascii_control() => quoted.push_str(&format!("'$'\\{:03o}''", u32::from(c))),
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// How the line names taw: `taw` when `found` (the `taw` the pane's `PATH` finds first) is this
/// binary, else this binary's own path. `home` is the pane's home ([`session_home`]), the one its
/// shell expands a `~` with, never this process's own: when it is a prefix of taw's path the line
/// writes `~/` and the rest quoted, and when tmux has none (or taw is not under it) the absolute
/// path, quoted.
pub fn taw_word(found: Option<&Path>, taw_bin: &Path, home: Option<&Path>) -> String {
    let this = std::fs::canonicalize(taw_bin).ok();
    if this.is_some() && found.and_then(|path| std::fs::canonicalize(path).ok()) == this {
        return "taw".to_string();
    }
    if let Some(rest) = home
        .filter(|home| home.is_absolute())
        .and_then(|home| taw_bin.strip_prefix(home).ok())
        .filter(|rest| !rest.as_os_str().is_empty())
    {
        return format!("~/{}", word(&rest.to_string_lossy()));
    }
    word(&taw_bin.to_string_lossy())
}

/// The `PATH` a new pane of `session` starts with, as tmux keeps it: the session's own, else the
/// server's; with no session (a restore, before its sessions exist), the server's. `None` when
/// tmux says neither.
pub fn session_path(t: &Tmux, session: Option<&str>) -> Option<String> {
    session_variable(t, session, "PATH")
}

/// The `HOME` a new pane of `session` starts with, as tmux keeps it (the same rule as
/// [`session_path`]): the home its shell expands a `~` with, which is not always the `HOME` of the
/// taw that types.
pub fn session_home(t: &Tmux, session: Option<&str>) -> Option<PathBuf> {
    session_variable(t, session, "HOME").map(PathBuf::from)
}

/// `name` as tmux keeps it for a new pane of `session`: the session's own, else the server's.
fn session_variable(t: &Tmux, session: Option<&str>, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let value = |args: &[&str]| {
        t.run(args)
            .ok()
            .and_then(|out| out.strip_prefix(&prefix).map(str::to_string))
    };
    session
        .and_then(|session| value(&["show-environment", "-t", session, name]))
        .or_else(|| value(&["show-environment", "-g", name]))
}

/// The `taw` a `PATH` finds first.
pub fn on_path(path: &str) -> Option<PathBuf> {
    which::which_in("taw", Some(path), "/").ok()
}

/// [`taw_word`] for a pane of `session` (or any new pane, with none), with the home tmux gives it.
pub fn taw_for(t: &Tmux, session: Option<&str>, taw_bin: &Path) -> String {
    let found = session_path(t, session).and_then(|path| on_path(&path));
    taw_word(
        found.as_deref(),
        taw_bin,
        session_home(t, session).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_is_taw_sandbox_the_agent_its_profile_and_its_arguments_after_a_double_dash() {
        assert_eq!(line("taw", "claude", None, &[]), "taw sandbox claude");
        assert_eq!(
            line("taw", "codex", Some("box"), &[]),
            "taw sandbox codex --profile box"
        );
        assert_eq!(
            line(
                "taw",
                "claude",
                None,
                &["--resume".to_string(), "4b1f2c".to_string()]
            ),
            "taw sandbox claude -- --resume 4b1f2c"
        );
    }

    /// An argument with spaces, quotes or `$(...)` stays one argument.
    #[test]
    fn every_word_stays_the_one_argument_it_is_for_the_panes_shell() {
        let args: Vec<String> = ["--resume", "4b1f 2c", "$(touch pwned)", "it's", "`id`"]
            .iter()
            .map(|a| a.to_string())
            .collect();

        let typed = line("~/'My Tools/taw'", "claude", Some("work"), &args);

        // The pane's shell reads the line: the taw word's home expands, and every other word is
        // the one argument it was.
        let place = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("set -- {typed}; printf '%s\\n' \"$@\"")])
            .current_dir(place.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/home/you")
            .env("HISTFILE", "/dev/null")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/home/you/My Tools/taw\nsandbox\nclaude\n--profile\nwork\n--\n--resume\n4b1f 2c\n$(touch pwned)\nit's\n`id`\n"
        );
        assert!(!place.path().join("pwned").exists(), "{typed}");
    }

    /// The words a shell reads from `typed`, run by `shell` with its `options` from a script, with
    /// `home` for its home (where a `~` it was left would lead) and `place` to run in (where a `$(...)`
    /// it ran would leave its file): each word ends with a NUL, an empty one too.
    fn read_back(
        (shell, options): &(PathBuf, Vec<&str>),
        typed: &str,
        home: &Path,
        place: &Path,
    ) -> Vec<String> {
        let script = place.join("read-back");
        std::fs::write(&script, format!("printf '%s\\0' {typed}\n")).unwrap();
        let out = std::process::Command::new(shell)
            .args(options)
            .arg(&script)
            .current_dir(place)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("ZDOTDIR", home)
            .env("HISTFILE", "/dev/null")
            .env("LANG", "en_AU.UTF-8")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{} {options:?} could not read {typed}: {}",
            shell.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        let mut words: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .map(str::to_string)
            .collect();
        words.pop();
        words
    }

    /// The shells a typed line is read back by: sh, macOS's bash 3.2, bash 5 when one is installed,
    /// zsh with no rc files, and zsh with MAGIC_EQUAL_SUBST, which expands what follows any `=` in a
    /// word.
    fn shells() -> Vec<(PathBuf, Vec<&'static str>)> {
        let mut shells = vec![
            (PathBuf::from("/bin/sh"), vec![]),
            (PathBuf::from("/bin/bash"), vec![]),
        ];
        if let Ok(bash) = which::which("bash")
            && std::fs::canonicalize(&bash).ok() != std::fs::canonicalize("/bin/bash").ok()
        {
            shells.push((bash, vec![]));
        }
        let zsh = which::which("zsh").unwrap_or_else(|_| PathBuf::from("/bin/zsh"));
        shells.push((zsh.clone(), vec!["-f"]));
        shells.push((zsh, vec!["-f", "-o", "magicequalsubst"]));
        shells
    }

    /// Whether `shell` reads `$'...'`, as bash, zsh and ksh do: dash, the `/bin/sh` of Debian and
    /// Ubuntu, does not, and reads `$'\n'` as the two characters `$\n`.
    fn reads_dollar_quotes(shell: &(PathBuf, Vec<&str>)) -> bool {
        let dir = tempfile::tempdir().unwrap();
        read_back(shell, "'a'$'\\n''b'", dir.path(), dir.path()) == ["a\nb"]
    }

    /// The line is read by the pane's shell, which is often zsh: sh, macOS's bash 3.2, bash 5 and zsh,
    /// with MAGIC_EQUAL_SUBST and without, all read back each word as it was, whatever it holds. A
    /// word that starts with `~` or `=`, one with `=` or `==` inside it, an assignment with a `~`, a
    /// quote, an empty word, what a shell globs, expands, comments or takes from its history, a
    /// four-byte character, and a newline, a tab and the other keys a line editor acts on, which are
    /// written as escapes so that the line is typed as one line of plain text.
    #[test]
    fn sh_bash_3_2_bash_5_and_zsh_read_back_every_word_as_it_was() {
        let words: Vec<String> = [
            "--model",
            "~",
            "~/x",
            "x=~/y",
            "=x",
            "x==y",
            "a=b",
            "--k=v",
            "x=~",
            "%1",
            "it's",
            "'",
            "end'",
            "",
            "a b",
            "$(touch pwned)",
            "`touch pwned`",
            "$HOME",
            "*",
            "[ab]",
            "{a,b}",
            "#x",
            "!x",
            "^x",
            "a;b",
            "a|b",
            "a&b",
            "<x>",
            "\\",
            "\"",
            "\u{1f4e6}",
            "line\nbreak",
            "\n",
            "tab\there",
            "cr\rhere",
            "esc\u{1b}[A",
            "kill\u{15}line",
            "del\u{7f}",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        let typed = line("taw", "claude", Some("work"), &words);
        assert!(
            !typed.chars().any(|c| c.is_ascii_control()),
            "the line holds a key a line editor acts on: {typed:?}"
        );

        let shells = shells();
        let dir = tempfile::tempdir().unwrap();
        let (home, place) = (dir.path().join("home"), dir.path().join("place"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&place).unwrap();
        let mut expected = vec!["sandbox", "claude", "--profile", "work", "--"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        expected.extend(words.iter().cloned());

        // macOS's sh is bash 3.2, which reads `$'...'`; a dash is read back the words it can read,
        // and every other shell reads them all.
        let sh_reads_them = reads_dollar_quotes(&shells[0]);
        assert!(
            sh_reads_them || !cfg!(target_os = "macos"),
            "macOS's /bin/sh does not read $'...'"
        );
        let plain: Vec<String> = words
            .iter()
            .filter(|word| !word.chars().any(|c| c.is_ascii_control()))
            .cloned()
            .collect();
        let mut expected_plain = expected[..5].to_vec();
        expected_plain.extend(plain.iter().cloned());
        let typed_plain = line("taw", "claude", Some("work"), &plain);

        for shell in &shells {
            let (typed, expected) = if sh_reads_them || shell.0 != Path::new("/bin/sh") {
                (&typed, &expected)
            } else {
                (&typed_plain, &expected_plain)
            };
            // The script's first word is `printf`'s format, and `taw` the first word it prints.
            let read = read_back(shell, typed, &home, &place);
            assert_eq!(read.first().map(String::as_str), Some("taw"), "{shell:?}");
            assert_eq!(read[1..], expected[..], "read by {shell:?}: {typed}");
        }
        assert!(!place.join("pwned").exists(), "a shell ran a $(...)");
    }

    /// The other lines taw types are written by the one rule too, and the same shells read them back
    /// as their words: an agent's reported resume command (`agent_resume::command_line`, whose words
    /// hold no apostrophe or control key) and restore's resume command, a `--flag=value` word
    /// included, whose value is quoted after its bare flag.
    #[test]
    fn the_reported_and_the_restore_resume_commands_read_back_as_their_words_in_every_shell() {
        let reported: Vec<String> = [
            "prime-agent",
            "--resume",
            "01a0",
            "x==y",
            "a=b",
            "--k=v",
            "=zsh",
            "x=~",
            "~/x",
            "$HOME",
            "a;b",
            "*",
            "#x",
            "a  b",
            "",
            "%1",
            "$(touch pwned)",
        ]
        .iter()
        .map(|word| word.to_string())
        .collect();
        crate::agent_resume::validate_argv(&reported).unwrap();
        let mut lines = vec![(crate::agent_resume::command_line(&reported), reported)];
        for (kind, id) in [
            ("claude", "a b"),
            ("codex", "it's"),
            ("omp", "x==y"),
            ("copilot", "~/x"),
            ("copilot", "=zsh"),
            ("pi", "$(touch pwned)"),
            ("letta", "default:a=b"),
        ] {
            let mut words = vec![crate::detect::executable(kind)];
            words.extend(crate::session::restore::resume_args(kind, id).unwrap());
            lines.push((
                crate::session::restore::resume_command(kind, id).unwrap(),
                words,
            ));
        }

        let dir = tempfile::tempdir().unwrap();
        let (home, place) = (dir.path().join("home"), dir.path().join("place"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&place).unwrap();
        for shell in &shells() {
            for (typed, words) in &lines {
                // The script's first word is `printf`'s format, and the agent's command the first
                // word it prints.
                let read = read_back(shell, typed, &home, &place);
                assert_eq!(&read, words, "read by {shell:?}: {typed}");
            }
        }
        assert!(!place.join("pwned").exists(), "a shell ran a $(...)");
    }

    #[test]
    fn a_plain_word_is_typed_as_it_is_and_any_other_in_single_quotes() {
        for plain in [
            "claude", "--model", "opus-4.1", "a_b", "x/y:z", "me@host", "50%", "+1",
        ] {
            assert_eq!(
                line("taw", "claude", None, &[plain.to_string()]),
                format!("taw sandbox claude -- {plain}")
            );
        }
        assert_eq!(
            line(
                "taw",
                "claude",
                None,
                &["it's".to_string(), "a\nb".to_string(), String::new()]
            ),
            "taw sandbox claude -- 'it'\\''s' 'a'$'\\n''b' ''"
        );
    }

    #[test]
    fn taw_is_typed_as_taw_when_the_panes_path_finds_this_binary_and_as_its_path_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("plugin/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let real = dir.path().join("plugin/target/release/taw");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "").unwrap();
        std::os::unix::fs::symlink(&real, bin.join("taw")).unwrap();
        let other = dir.path().join("other/taw");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "").unwrap();

        assert_eq!(taw_word(Some(&bin.join("taw")), &real, None), "taw");
        assert_eq!(
            taw_word(Some(&other), &real, None),
            real.display().to_string()
        );
        assert_eq!(
            taw_word(None, &real, Some(dir.path())),
            "~/plugin/target/release/taw"
        );
        let spaced = dir.path().join("My Tools/taw");
        assert_eq!(
            taw_word(None, &spaced, Some(dir.path())),
            "~/'My Tools/taw'"
        );
    }

    /// The `~` is the pane's home, which is not always the home of the taw that types: with the
    /// pane's home equal to the one taw's path lies under it is `~/`, with another it is the
    /// absolute path, and with none (tmux has no `HOME`, or an empty or relative one) the absolute
    /// path too, quoted as ever.
    #[test]
    fn the_tilde_is_the_panes_home_and_the_path_is_absolute_when_it_is_not_a_prefix_of_taws() {
        let dir = tempfile::tempdir().unwrap();
        let taw = dir.path().join("My Tools/taw");
        let elsewhere = dir.path().join("elsewhere");
        let absolute = format!("'{}'", taw.display());

        assert_eq!(taw_word(None, &taw, Some(dir.path())), "~/'My Tools/taw'");
        assert_eq!(taw_word(None, &taw, Some(&elsewhere)), absolute);
        assert_eq!(taw_word(None, &taw, None), absolute);
        assert_eq!(taw_word(None, &taw, Some(Path::new(""))), absolute);
        assert_eq!(taw_word(None, &taw, Some(Path::new("relative"))), absolute);
        // A home that is taw's own path, not a folder above it, leaves nothing to write after `~/`.
        assert_eq!(taw_word(None, &taw, Some(&taw)), absolute);
    }

    /// A new pane's `HOME` is the session's own, else the server's, which is also the one asked with
    /// no session, as `PATH` is.
    #[test]
    fn the_home_is_the_sessions_own_else_the_servers() {
        let dir = tempfile::tempdir().unwrap();
        let t = tmux_answering(
            dir.path(),
            "'-t $1 HOME') echo 'HOME=/own/home' ;;\n\
             '-t $2 HOME') echo 'unknown variable: HOME' >&2; exit 1 ;;\n\
             '-t $3 HOME') echo '-HOME' ;;\n\
             '-g HOME') echo 'HOME=/server home' ;;",
        );
        let home = |session| session_home(&t, session);
        assert_eq!(home(Some("$1")), Some(PathBuf::from("/own/home")));
        assert_eq!(home(Some("$2")), Some(PathBuf::from("/server home")));
        assert_eq!(home(Some("$3")), Some(PathBuf::from("/server home")));
        assert_eq!(home(None), Some(PathBuf::from("/server home")));
        let silent = tmux_answering(dir.path(), "");
        assert_eq!(session_home(&silent, Some("$1")), None);
    }

    #[test]
    fn a_path_finds_the_first_taw_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a"), dir.path().join("b"));
        for folder in [&first, &second] {
            std::fs::create_dir_all(folder).unwrap();
            let taw = folder.join("taw");
            std::fs::write(&taw, "#!/bin/sh\n").unwrap();
            crate::test_exec::make_executable(&taw);
        }
        let path = format!("{}:{}", first.display(), second.display());
        assert_eq!(on_path(&path), Some(first.join("taw")));
        assert_eq!(on_path("/nonexistent"), None);
    }

    /// A stand-in for tmux at `bin` that answers `show-environment` from `body`, a `case` on the
    /// words after it: `-t <session> PATH` or `-g PATH`.
    fn tmux_answering(dir: &Path, body: &str) -> Tmux {
        let bin = dir.join("tmux");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\n[ \"$1\" = -u ] && shift\n[ \"$1\" = show-environment ] || exit 1\nshift\ncase \"$*\" in\n{body}\n*) exit 1 ;;\nesac\n"),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        Tmux { bin, socket: None }
    }

    /// A session's own `PATH` is the one its new panes start with; one it does not have (tmux says
    /// `unknown variable`) or has taken out (`-PATH`) leaves the server's, which is also the one asked
    /// with no session.
    #[test]
    fn the_path_is_the_sessions_own_else_the_servers() {
        let dir = tempfile::tempdir().unwrap();
        let t = tmux_answering(
            dir.path(),
            "'-t $1 PATH') echo 'PATH=/own:/usr/bin' ;;\n\
             '-t $2 PATH') echo 'unknown variable: PATH' >&2; exit 1 ;;\n\
             '-t $3 PATH') echo '-PATH' ;;\n\
             '-g PATH') echo 'PATH=/server: with a space' ;;",
        );
        assert_eq!(
            session_path(&t, Some("$1")).as_deref(),
            Some("/own:/usr/bin")
        );
        assert_eq!(
            session_path(&t, Some("$2")).as_deref(),
            Some("/server: with a space")
        );
        assert_eq!(
            session_path(&t, Some("$3")).as_deref(),
            Some("/server: with a space")
        );
        assert_eq!(
            session_path(&t, None).as_deref(),
            Some("/server: with a space")
        );
        let silent = tmux_answering(dir.path(), "");
        assert_eq!(session_path(&silent, Some("$1")), None);
    }

    /// The `taw` the session's `PATH` finds is the one a pane of it runs: `taw` when it is this
    /// binary, by a link or not, and this binary's path when it is another or there is none.
    #[test]
    fn a_pane_of_a_session_whose_path_finds_this_binary_is_typed_taw() {
        let dir = tempfile::tempdir().unwrap();
        let this = dir.path().join("build/taw");
        std::fs::create_dir_all(this.parent().unwrap()).unwrap();
        std::fs::write(&this, "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable(&this);
        let links = dir.path().join("links");
        std::fs::create_dir_all(&links).unwrap();
        std::os::unix::fs::symlink(&this, links.join("taw")).unwrap();
        let t = tmux_answering(
            dir.path(),
            &format!(
                "'-t $1 PATH') echo 'PATH={}:/usr/bin' ;;\n'-g PATH') echo 'PATH=/usr/bin:/bin' ;;\n\
                 '-t $2 HOME') echo 'HOME=/elsewhere' ;;\n'-g HOME') echo 'HOME={}' ;;",
                links.display(),
                dir.path().display()
            ),
        );

        assert_eq!(taw_for(&t, Some("$1"), &this), "taw");
        // With no session the home is the server's, which taw's path lies under; with a session whose
        // own home it does not, the path is the absolute one.
        assert_eq!(taw_for(&t, None, &this), "~/build/taw");
        assert_eq!(
            taw_for(&t, Some("$2"), &this),
            word(&this.to_string_lossy())
        );
    }
}
