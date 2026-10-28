// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! The command an agent reports for itself that resumes its current session (herdr 0.9.2,
//! `pane report-agent ... -- <command...>`), which `taw session restore` types into the restored
//! pane in place of taw's own resume command for that agent.
//!
//! The one place it is kept and read back: the pane options that hold it while its agent holds the
//! pane ([`OPTIONS`]), what a snapshot saves of it ([`ReportedResume`], `PaneSnap::agent_resume`), the
//! checks it must pass ([`validate_argv`]), the line a restore types ([`ReportedResume::line`]) and the
//! key a restore resumes it once by ([`ReportedResume::key`]). Who may set it, and when it goes, is the
//! report verbs' and the watcher's (`cli::pane::report`, `store`, `watch`).

use serde::{Deserialize, Serialize};

/// The source that reported the pane's resume command (`--source`, as given).
pub const SOURCE_OPTION: &str = "@agent_resume_source";
/// The agent it resumes (`--agent`).
pub const AGENT_OPTION: &str = "@agent_resume_agent";
/// The command, as the line a restore types ([`command_line`]).
pub const ARGV_OPTION: &str = "@agent_resume_argv";
/// The three pane options that hold a reported resume command, which go together.
pub const OPTIONS: [&str; 3] = [SOURCE_OPTION, AGENT_OPTION, ARGV_OPTION];

/// The most words a resume command may have.
const MAX_ARGS: usize = 64;
/// The most bytes its words may hold, all together.
const MAX_BYTES: usize = 8192;

/// A resume command an agent reported for itself: who reported it, for which agent, and its words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedResume {
    pub source: String,
    pub agent: String,
    pub argv: Vec<String>,
}

impl ReportedResume {
    /// The line a restore types into the pane's shell ([`command_line`]), which is also what the pane
    /// keeps it as.
    pub fn line(&self) -> String {
        command_line(&self.argv)
    }

    /// What a restore resumes once, whatever number of panes saved it: the source, the agent, the
    /// directory the pane is in and the command. The same command can name different sessions in
    /// different directories (`agent --continue`), so the directory is part of what it resumes.
    pub fn key(&self, cwd: &str) -> String {
        format!(
            "{}\u{0}{}\u{0}{cwd}\u{0}argv\u{0}{}",
            self.source,
            self.agent,
            self.argv.join("\u{0}")
        )
    }

    /// The command a pane's three options ([`OPTIONS`]) hold, when they hold one: all three set, and
    /// the line one [`command_line`] writes.
    pub fn from_options(source: &str, agent: &str, line: &str) -> Option<ReportedResume> {
        if source.is_empty() || agent.is_empty() {
            return None;
        }
        Some(ReportedResume {
            source: source.to_string(),
            agent: agent.to_string(),
            argv: parse_line(line)?,
        })
    }
}

/// Whether `argv` can be kept as a resume command and typed at a restore, and why not: it has a first
/// word, a plain command name (letters, digits, `_`, `-` and `.`, not starting with `-`) and not a path,
/// since shells disagree on how to run a quoted path; at most 64 words and 8 KiB; and no word holds a
/// control character or an apostrophe, so that the line a restore types quotes every word the same way
/// in every shell.
pub fn validate_argv(argv: &[String]) -> Result<(), String> {
    let Some(command) = argv.first() else {
        return Err("the resume command after -- must not be empty".to_string());
    };
    if argv.len() > MAX_ARGS {
        return Err(format!(
            "the resume command may have at most {MAX_ARGS} words"
        ));
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_BYTES {
        return Err(format!(
            "the resume command may hold at most {MAX_BYTES} bytes"
        ));
    }
    if argv.iter().any(|word| word.chars().any(char::is_control)) {
        return Err("no word of the resume command may hold a control character".to_string());
    }
    if argv.iter().any(|word| word.contains('\'')) {
        return Err("no word of the resume command may hold an apostrophe".to_string());
    }
    let plain = !command.is_empty()
        && !command.starts_with('-')
        && command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if !plain {
        return Err(format!(
            "the resume command must start with a plain command name, not a path: {command:?}"
        ));
    }
    Ok(())
}

/// The line that runs `argv` in a POSIX shell, each word written by taw's one rule for a typed word
/// ([`crate::sandbox::typed::word`]): as it is when it is made of letters, digits and
/// `_ - . / : @ % +`, and in single quotes otherwise, with the words a space apart. An apostrophe in
/// a word, which [`validate_argv`] refuses, is written `'\''`. A word that holds `=` is quoted: zsh
/// reads one that starts with it as the path of a command, and with MAGIC_EQUAL_SUBST expands what
/// follows an `=` anywhere in a word.
pub fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| crate::sandbox::typed::word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The words of a line [`command_line`] wrote for a command with no apostrophe in it, `None` for any
/// other line: words a space apart, each either bare or in single quotes.
fn parse_line(line: &str) -> Option<Vec<String>> {
    if line.is_empty() {
        return None;
    }
    let mut words = Vec::new();
    let mut rest = line;
    loop {
        let (word, after) = match rest.strip_prefix('\'') {
            Some(quoted) => {
                let end = quoted.find('\'')?;
                (quoted[..end].to_string(), &quoted[end + 1..])
            }
            None => {
                let end = rest.find(' ').unwrap_or(rest.len());
                let word = &rest[..end];
                if word.is_empty() || crate::sandbox::typed::word(word) != word {
                    return None;
                }
                (word.to_string(), &rest[end..])
            }
        };
        words.push(word);
        if after.is_empty() {
            return Some(words);
        }
        rest = after.strip_prefix(' ').filter(|next| !next.is_empty())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    // The cases of herdr's src/agent_resume.rs
    // `reported_resume_argv_requires_a_plain_command_name`.
    #[test]
    fn reported_resume_argv_requires_a_plain_command_name() {
        assert!(
            validate_argv(&argv(&[
                "prime-agent",
                "--resume",
                "01a0de21",
                "--model",
                "gpt 6"
            ]))
            .is_ok()
        );
        assert!(validate_argv(&argv(&["cursor-agent.cmd", "--resume", "id"])).is_ok());

        for invalid in [
            argv(&[]),
            argv(&[""]),
            argv(&["/usr/bin/prime-agent", "--resume", "id"]),
            argv(&["C:\\Program Files\\Prime\\prime.exe"]),
            argv(&["prime agent"]),
            argv(&["-prime"]),
            argv(&["prime-agent", "bad\nline"]),
            argv(&["prime-agent", "--name", "can's session"]),
            vec!["prime-agent".to_string(); MAX_ARGS + 1],
            argv(&["prime-agent", &"x".repeat(MAX_BYTES)]),
        ] {
            assert!(validate_argv(&invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn the_limits_are_herdrs_and_inclusive() {
        assert!(validate_argv(&vec!["a".to_string(); MAX_ARGS]).is_ok());
        assert!(validate_argv(&argv(&["a", &"x".repeat(MAX_BYTES - 1)])).is_ok());
        assert!(validate_argv(&argv(&["a", "tab\there"])).is_err());
        assert!(validate_argv(&argv(&["a", "del\u{7f}"])).is_err());
    }

    // Follows herdr's src/app/agent_resume.rs,
    // `shell_command_from_argv_quotes_resume_arguments`. Differences: a word that holds `=` is quoted
    // as well.
    #[test]
    fn the_typed_line_quotes_only_the_words_that_need_it() {
        assert_eq!(
            command_line(&argv(&["claude", "--resume", "session with ' quote"])),
            "claude --resume 'session with '\\'' quote'"
        );
        assert_eq!(
            command_line(&argv(&[
                "prime-agent",
                "--resume",
                "01a0",
                "--model",
                "gpt 6",
                "",
                "--dir=/a/b",
                "=zsh",
                "~/x",
                "$HOME",
                "a;b",
            ])),
            "prime-agent --resume 01a0 --model 'gpt 6' '' '--dir=/a/b' '=zsh' '~/x' '$HOME' 'a;b'"
        );
    }

    /// What a pane keeps is the typed line, and it reads back as the words it was made from, for every
    /// command the checks let through.
    #[test]
    fn the_kept_line_reads_back_as_the_words_it_was_made_from() {
        for words in [
            argv(&["a"]),
            argv(&["prime-agent", "--resume", "01a0", "--model", "gpt 6"]),
            argv(&["a", "", "b", "", ""]),
            argv(&[
                "a",
                "two  spaces",
                " lead",
                "trail ",
                "\"q\" $x ~y `z` #{f} ;&|<>()*?\\",
            ]),
            argv(&[
                "a",
                "=x",
                "%1",
                "--k=v",
                "x==y",
                "a=b",
                "caf\u{e9} \u{1f600}",
            ]),
        ] {
            assert!(validate_argv(&words).is_ok(), "{words:?}");
            assert_eq!(parse_line(&command_line(&words)), Some(words.clone()));
        }
        for line in [
            "", " a", "a ", "a  b", "'a", "a'b", "'a'b", "a $x", "a 'b' 'c", "=x", "a=b",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn a_pane_holds_a_command_only_with_its_source_its_agent_and_its_line() {
        let resume =
            ReportedResume::from_options("prime-agent", "prime-agent", "prime-agent --resume a")
                .unwrap();
        assert_eq!(resume.argv, argv(&["prime-agent", "--resume", "a"]));
        assert_eq!(resume.line(), "prime-agent --resume a");
        assert_eq!(ReportedResume::from_options("", "a", "a"), None);
        assert_eq!(ReportedResume::from_options("s", "", "a"), None);
        assert_eq!(ReportedResume::from_options("s", "a", ""), None);
        assert_eq!(ReportedResume::from_options("s", "a", "a 'b"), None);
    }

    /// The same command in two directories is two sessions; the same in one is one.
    #[test]
    fn the_key_a_restore_resumes_once_by_holds_the_panes_directory() {
        let resume = ReportedResume {
            source: "prime-agent".to_string(),
            agent: "prime-agent".to_string(),
            argv: argv(&["prime-agent", "--continue"]),
        };
        assert_eq!(resume.key("/project-a"), resume.key("/project-a"));
        assert_ne!(resume.key("/project-a"), resume.key("/project-b"));
        let other = ReportedResume {
            source: "custom".to_string(),
            ..resume.clone()
        };
        assert_ne!(resume.key("/project-a"), other.key("/project-a"));
    }
}
