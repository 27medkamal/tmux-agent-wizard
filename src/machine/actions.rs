//! The words an action on a machine's row says to the machine: the `taw` verb the popup's action
//! mirrors, as the arguments of `taw`, for the ssh command to run there ([`super::ssh::command`] with
//! `Mode::Command`, which quotes each word for the shell on the far side).
//!
//! Read off each far verb's own definition and `--help` (all of them print a JSON envelope, and
//! none takes `--json`):
//!
//! - `pane close <PANE_ID>`, `workspace close <WORKSPACE_ID> [--group]`: raw tmux ids, `%N` and `$N`.
//! - `tab rename <TAB_ID> <LABEL>...`, `workspace rename <WORKSPACE_ID> <LABEL>...`: `@N` and `$N`; the
//!   label words are joined with one space, so one word with spaces in it reads back as itself.
//! - `agent rename <TARGET> [NAME]`: `%N`; `NAME` is one word.
//! - `worktree create --workspace <ID> --branch <NAME> --no-focus`: `$N`; `--focus` is the default, and
//!   it would switch whichever client is attached over there.
//! - `worktree remove --workspace <ID> [--force]`: the worktree's `workspace_id` as its record holds
//!   it (`w5`: the verb finds the record by that, its session name or its branch, and not by `$N`).
//!
//! A value that could be read as an option goes after `--` where the verb has a positional word for
//! it (the three renames), and as `--branch=<name>` for `--branch`, whose value clap will not take as a
//! separate word when it starts with a dash. The far verb then refuses such a branch in its own
//! words (`invalid_argument`), which is what a person is told.

use std::time::Duration;

use super::bounded::Ran;
use super::clean::plain;
use super::words;
use crate::tui::actions::Action;

/// The arguments of the far `taw` for `action`, or nothing when it has none: an action with no verb
/// over there (going somewhere is the app's own business), or a target that is not an id of the form
/// the verb takes, or a word that cannot be an argument at all (it holds a NUL).
///
/// The fields of `action` hold the machine's own ids, `%16`, `$3`, `@5` (a closed worktree's `w5`),
/// as the row the action came from carries them; `studio:%16`, a name, a path or an id cut short by
/// the cleaner is not one of them and gives no argv. A name or a branch is always one word, whatever
/// it holds. There is no window-close action.
pub fn remote_argv(action: &Action) -> Vec<String> {
    let argv = match action {
        Action::KillPane { pane } => id('%', pane).map(|pane| args(&["pane", "close", pane])),
        Action::KillWorkspace { session } => {
            id('$', session).map(|session| args(&["workspace", "close", session]))
        }
        Action::KillGroup { session } => {
            id('$', session).map(|session| args(&["workspace", "close", session, "--group"]))
        }
        Action::RenameAgent { pane, name } => id('%', pane)
            .zip(word(name))
            .map(|(pane, name)| renamed(&["agent", "rename", pane], name)),
        Action::RenameWorkspace { session, name } => id('$', session)
            .zip(word(name))
            .map(|(session, name)| renamed(&["workspace", "rename", session], name)),
        Action::RenameWindow { window, name } => id('@', window)
            .zip(word(name))
            .map(|(window, name)| renamed(&["tab", "rename", window], name)),
        Action::NewWorktree { workspace, branch } => {
            id('$', workspace)
                .zip(word(branch))
                .map(|(workspace, branch)| {
                    let mut argv = args(&["worktree", "create", "--workspace", workspace]);
                    if branch.starts_with('-') {
                        argv.push(format!("--branch={branch}"));
                    } else {
                        argv.extend(args(&["--branch", branch]));
                    }
                    argv.push("--no-focus".to_string());
                    argv
                })
        }
        Action::RemoveWorktree { workspace, force } => workspace_id(workspace).map(|workspace| {
            let mut argv = args(&["worktree", "remove", "--workspace", workspace]);
            if *force {
                argv.push("--force".to_string());
            }
            argv
        }),
        // `--json` for the envelope, `--no-switch` so no client over there moves (someone may be attached on
        // that machine), and `--` so a term that starts with a dash stays a term. The words of the term are an
        // argument each, as `taw open code api` gets them at a shell: that taw tries them joined with single
        // spaces as a folder and hands zoxide each as its own keyword, which one argument with a space in it
        // would not be.
        Action::NewRemoteSession { term, .. } => word(term)
            .filter(|term| !term.trim().is_empty())
            .map(|term| {
                let mut argv = args(&["open", "--json", "--no-switch", "--"]);
                argv.extend(term.split_whitespace().map(str::to_string));
                argv
            }),
        // No verb over there: going somewhere is the app's own business, and the rest are about
        // this machine's own tmux (there is no window-close action to send either).
        Action::Jump { .. }
        | Action::SwitchWorkspace { .. }
        | Action::OpenProject { .. }
        | Action::NextUp { .. }
        | Action::OpenWorktree { .. }
        | Action::Doorway { .. }
        // The image is sent by `taw image paste --machine`, which streams it there itself.
        | Action::PasteImage { .. } => None,
    };
    argv.unwrap_or_default()
}

/// The most digits an id may have: a `u64`'s. An id the cleaner cut at its limit is a long run of
/// digits that looks like an id, and is not one.
const MOST_DIGITS: usize = 20;

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// `id` when it is `sigil` and digits, which is what the verb takes (`valid_tmux_id` is the check
/// for the form, and each verb takes its own kind: a session's id is no use to `pane close`).
fn id(sigil: char, id: &str) -> Option<&str> {
    let digits = id.strip_prefix(sigil)?;
    (super::valid_tmux_id(id) && digits.len() <= MOST_DIGITS).then_some(id)
}

/// A closed worktree's `workspace_id` as its record holds it: `w` and digits, nothing the verb would
/// read as another kind of id (`w5:t7`) or as a session name, a branch or a path.
fn workspace_id(id: &str) -> Option<&str> {
    let digits = id.strip_prefix('w')?;
    (!digits.is_empty()
        && digits.len() <= MOST_DIGITS
        && digits.bytes().all(|b| b.is_ascii_digit()))
    .then_some(id)
}

/// `word` unless it cannot be an argument at all (a NUL ends a C string). Whatever else it holds
/// stays: it is one word, and the far verb says what it thinks of it.
fn word(word: &str) -> Option<&str> {
    (!word.contains('\0')).then_some(word)
}

/// `head` and then `name` as one word, after `--` when it could be read as an option.
fn renamed(head: &[&str], name: &str) -> Vec<String> {
    let mut argv = args(head);
    if name.starts_with('-') {
        argv.push("--".to_string());
    }
    argv.push(name.to_string());
    argv
}

/// How long one verb on a machine may take before its ssh is ended (R12-5): twenty seconds.
pub const VERB_BOUND: Duration = Duration::from_secs(20);

/// The bound of `action`: [`VERB_BOUND`], and for closing a group of sessions together
/// (`workspace close --group`) [`VERB_BOUND`] for each session in it, `sessions` being how many the
/// group has (a count of nothing is one: the verb closes at least the workspace itself).
pub fn bound(action: &Action, sessions: usize) -> Duration {
    match action {
        Action::KillGroup { .. } => VERB_BOUND * u32::try_from(sessions.max(1)).unwrap_or(u32::MAX),
        _ => VERB_BOUND,
    }
}

/// What a verb run on a machine came to, as the app tells a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The verb exited zero: what happened shows in the machine's next state.
    Done,
    /// A hook on the machine said no (`hook_refused`). `label` is the hook's own name (`pre-close hook`),
    /// `why` its last line, as [`super::words::hook_refused`] shows them; nothing was closed.
    HookRefused { label: String, why: String },
    /// The workspace has worktree workspaces open (`workspace_group_close_required`): `--group` closes them
    /// together with it, which is a question for the person.
    GroupCloseRequired,
    /// git refused to remove a worktree for changes in it (`worktree_dirty`): `--force` would, which is a
    /// question for the person.
    Dirty,
    /// The target is not there any more (`not_found`).
    Gone,
    /// The far taw's own clap refused `open --no-switch` (or `open` itself): the exit was 2, with no envelope,
    /// and the stderr the run kept shows clap naming that option or subcommand. A taw that knows `open
    /// --no-switch` answers every failure with an envelope.
    OldTaw,
    /// The far command ended with status 2 and no envelope for any other reason (a wrapper script, a shell's
    /// syntax error): the first line of the stderr tail the run kept, cleaned, or empty when it kept none.
    Usage(String),
    /// Any other error the machine's taw reported: its own message, with the machine named
    /// ([`super::words::refused`]).
    Refused(String),
    /// ssh did not get an answer from the machine (it ended with 255 or by a signal, or it could not be
    /// run), or the far end ended with no answer taw can read.
    NoAnswer,
    /// The bound passed with no answer, so ssh was ended: the verb may still have run over there.
    TimedOut { secs: u64 },
}

/// ssh's own exit status when it could not talk to the machine.
const SSH_FAILED: i32 = 255;

/// What a verb that ran on `machine` for at most `bound` came to. `ran` is how the bounded run ended: ssh
/// ending with 255, a signal, a run that could not start (and a run that was called off, which only an app
/// that is ending does) is [`Reply::NoAnswer`]; the bound is [`Reply::TimedOut`]; an exit of zero is
/// [`Reply::Done`] whatever else was written. Any other exit reads the envelope, the last non-empty line of
/// what was written (a login shell may have said something before it), by its error code.
pub fn classify(machine: &str, ran: Ran, bound: Duration) -> Reply {
    match ran {
        Ran::Failed(_) | Ran::Ended => Reply::NoAnswer,
        Ran::TimedOut => Reply::TimedOut {
            secs: bound.as_secs(),
        },
        Ran::Done {
            code: Some(0),
            signal: None,
            ..
        } => Reply::Done,
        Ran::Done {
            signal: Some(_), ..
        }
        | Ran::Done {
            code: Some(SSH_FAILED),
            ..
        } => Reply::NoAnswer,
        Ran::Done {
            code,
            stdout,
            stderr,
            ..
        } => match error_of(&stdout) {
            Some((code, message)) => by_code(machine, &code, &message),
            None if code == Some(2)
                && (clap_refused_open(&stderr) || clap_refused_candidates(&stderr)) =>
            {
                Reply::OldTaw
            }
            None if code == Some(2) => Reply::Usage(first_line(&stderr)),
            None => Reply::Refused(words::refused(
                machine,
                &match code {
                    Some(code) => format!("it ended with status {code} and no answer taw can read"),
                    None => "it ended with no answer taw can read".to_string(),
                },
            )),
        },
    }
}

/// The `result` of the envelope a verb that exited zero wrote: the last non-empty line of `stdout`.
fn result_of(stdout: &[u8]) -> Option<serde_json::Value> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut reply: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(reply.get_mut("result")?.take())
}

/// How many worktree sessions a `workspace close --group` closed with the session, as its reply names them
/// (`worktree_workspace_ids`); `None` when the reply does not name any.
pub fn worktree_sessions_closed(stdout: &[u8]) -> Option<usize> {
    Some(
        result_of(stdout)?["worktree_workspace_ids"]
            .as_array()?
            .len(),
    )
}

/// The branch of the worktree a `worktree remove` removed, as its reply names it. Text from the far side is
/// cleaned here and in [`by_code`] only: these two functions are the one place the app's words read it.
pub fn worktree_branch_removed(stdout: &[u8]) -> Option<String> {
    let result = result_of(stdout)?;
    Some(plain(result["worktree"]["branch"].as_str()?))
}

/// The error of an envelope: the `code` and `message` of the last non-empty line of `stdout`, when it is a
/// JSON object holding an `error`.
fn error_of(stdout: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let reply: serde_json::Value = serde_json::from_str(line).ok()?;
    let error = reply.get("error")?;
    Some((
        error.get("code")?.as_str()?.to_string(),
        error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_default()
            .to_string(),
    ))
}

/// Whether `stderr` is a taw's own clap refusing `open --no-switch` (`error: unexpected argument '--no-switch'
/// found`) or `open` (`error: unrecognized subcommand 'open'`): what a taw older than `open --no-switch` says.
/// Any other text, a wrapper's or a shell's, is not that taw speaking.
fn clap_refused_open(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("error:"))
        .any(|said| {
            said.contains("unexpected argument '--no-switch'")
                || said.contains("unrecognized subcommand 'open'")
        })
}

/// Whether `stderr` is a taw's own clap refusing `session candidates` (`error: unrecognized subcommand
/// 'candidates'`): what a taw older than that verb says when another machine's picker asks it for its folders.
fn clap_refused_candidates(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("error:"))
        .any(|said| said.contains("unrecognized subcommand 'candidates'"))
}

/// The first non-empty line of `stderr`, cleaned (the far side's text reaches a toast only through the cleaner).
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| plain(line.trim()))
        .unwrap_or_default()
}

/// What a [`Reply::Usage`] says: the far command ended with status 2 and no answer, and `said`, the first line
/// of its stderr when there is one.
pub fn usage_words(machine: &str, said: &str) -> String {
    if said.is_empty() {
        return words::refused(machine, "it ended with status 2 and no answer taw can read");
    }
    words::refused(machine, &format!("it ended with status 2 and said: {said}"))
}

/// The far verb another machine's picker asks with: what the picker there would list, as JSON.
pub fn candidates_argv() -> Vec<String> {
    args(&["session", "candidates", "--json"])
}

/// The folders `session candidates --json` listed, as its envelope's `result.candidates` names them, in its order,
/// with its optional `truncated` (a missing key is false) and `sessions_error` (tmux's own words there). Text from
/// the far side is cleaned here; a row with no name or no folder is left out. `None` when the answer is not that
/// envelope.
pub fn candidates_offered(stdout: &[u8]) -> Option<crate::tui::picker::FarList> {
    let result = result_of(stdout)?;
    let rows = result["candidates"].as_array()?;
    let folders = Some(
        rows.iter()
            .filter_map(|row| {
                let name = plain(row["name"].as_str()?);
                let dir = plain(row["dir"].as_str()?);
                (!name.is_empty() && !dir.is_empty()).then(|| crate::tui::picker::FarFolder {
                    kind: plain(row["kind"].as_str().unwrap_or_default()),
                    name,
                    dir,
                    session: row["session"].as_str().map(plain),
                })
            })
            .collect(),
    )?;
    Some(crate::tui::picker::FarList {
        folders,
        truncated: result["truncated"].as_bool().unwrap_or(false),
        sessions_error: result["sessions_error"]
            .as_str()
            .map(plain)
            .filter(|said| !said.trim().is_empty()),
    })
}

/// The session `open --json` made or found, as its envelope names it: the name, whether it was made, and the
/// folder. Text from the far side is cleaned here.
pub fn session_opened(stdout: &[u8]) -> Option<(String, bool, String)> {
    let result = result_of(stdout)?;
    let name = plain(result["session"].as_str()?);
    let created = result["created"].as_bool().unwrap_or(false);
    let dir = plain(result["dir"].as_str().unwrap_or_default());
    (!name.is_empty()).then_some((name, created, dir))
}

/// The message of the error envelope in `stdout`, cleaned: what a far verb said when it failed.
pub fn error_message(stdout: &[u8]) -> Option<String> {
    error_of(stdout).map(|(_, message)| plain(&message))
}

fn by_code(machine: &str, code: &str, message: &str) -> Reply {
    match code {
        "hook_refused" => {
            let (label, why) = hook_words(message);
            // What the far side sent reaches a toast only through the cleaner (no escape sequence, one line, cut).
            Reply::HookRefused {
                label: plain(&label),
                why: plain(&why),
            }
        }
        "workspace_group_close_required" => Reply::GroupCloseRequired,
        "worktree_dirty" => Reply::Dirty,
        "not_found" => Reply::Gone,
        _ => Reply::Refused(words::refused(machine, &plain(message))),
    }
}

/// The hook's name and what it said, out of the message of a `hook_refused` error. The messages are
/// `<label> <command> failed (<status>): <stderr>` (a hook that exited), `<label> took longer than <n> s`
/// (a hook that ran out of time), and the `<label>` is `pre-close hook`, `pre-worktree-remove hook` or
/// `pre-create hook`. What it said is the last non-empty line after the `): ` that ends the status (the first
/// one after ` failed (`: the command, which comes before, may hold one); a hook that wrote nothing says its
/// status (`refused (exit status: 3)`), and a hook that ran out of time says how.
fn hook_words(message: &str) -> (String, String) {
    let label = match message.split_once(" hook") {
        Some((name, _)) if !name.is_empty() && !name.contains(char::is_whitespace) => {
            format!("{name} hook")
        }
        _ => "hook".to_string(),
    };
    let after_label = message
        .strip_prefix(label.as_str())
        .map_or(message, str::trim_start);
    // The status is the `failed (` followed by what a process's status says (`exit status: 3`, `signal: 9`): the
    // hook's own command, which comes before it, may hold a ` failed (` of its own.
    let at = message
        .match_indices(" failed (")
        .map(|(at, found)| at + found.len())
        .find(|at| {
            let rest = &message[*at..];
            rest.starts_with("exit status: ") || rest.starts_with("signal: ")
        })
        .or_else(|| message.find(" failed (").map(|at| at + " failed (".len()));
    let Some(at) = at else {
        return (label, after_label.to_string());
    };
    let status = &message[at..];
    let (status, said) = match status.split_once("): ") {
        Some((status, said)) => (status, said),
        None => {
            let status = status.trim_end().trim_end_matches(':');
            (status.strip_suffix(')').unwrap_or(status), "")
        }
    };
    match said
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
    {
        Some(why) => (label, why.to_string()),
        None => (label, format!("refused ({status})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(action: Action) -> Vec<String> {
        remote_argv(&action)
    }

    fn words(of: &[&str]) -> Vec<String> {
        of.iter().map(|w| w.to_string()).collect()
    }

    /// Words that hold everything a shell, a clap or a person could trip on: each must still be one word.
    const HOSTILE: [&str; 7] = [
        "two words",
        "it's \"quoted\"",
        "$(touch /tmp/x)",
        "a; rm -rf ~",
        "back`tick`",
        "line\nbreak",
        "tab\there",
    ];

    #[test]
    fn every_action_with_a_verb_gives_its_exact_argv() {
        assert_eq!(
            argv(Action::KillPane { pane: "%16".into() }),
            words(&["pane", "close", "%16"])
        );
        assert_eq!(
            argv(Action::KillWorkspace {
                session: "$3".into()
            }),
            words(&["workspace", "close", "$3"])
        );
        assert_eq!(
            argv(Action::KillGroup {
                session: "$3".into()
            }),
            words(&["workspace", "close", "$3", "--group"])
        );
        assert_eq!(
            argv(Action::RenameAgent {
                pane: "%16".into(),
                name: "scout".into()
            }),
            words(&["agent", "rename", "%16", "scout"])
        );
        assert_eq!(
            argv(Action::RenameWorkspace {
                session: "$3".into(),
                name: "garden".into()
            }),
            words(&["workspace", "rename", "$3", "garden"])
        );
        assert_eq!(
            argv(Action::RenameWindow {
                window: "@5".into(),
                name: "logs".into()
            }),
            words(&["tab", "rename", "@5", "logs"])
        );
        assert_eq!(
            argv(Action::NewWorktree {
                workspace: "$3".into(),
                branch: "feat/rain".into()
            }),
            words(&[
                "worktree",
                "create",
                "--workspace",
                "$3",
                "--branch",
                "feat/rain",
                "--no-focus"
            ])
        );
        assert_eq!(
            argv(Action::RemoveWorktree {
                workspace: "w5".into(),
                force: false
            }),
            words(&["worktree", "remove", "--workspace", "w5"])
        );
        assert_eq!(
            argv(Action::RemoveWorktree {
                workspace: "w5".into(),
                force: true
            }),
            words(&["worktree", "remove", "--workspace", "w5", "--force"])
        );
    }

    #[test]
    fn the_far_n_gives_the_far_taw_each_word_of_the_term() {
        let open = |term: &str| {
            argv(Action::NewRemoteSession {
                machine: "studio".into(),
                term: term.into(),
            })
        };
        assert_eq!(
            open("code api"),
            words(&["open", "--json", "--no-switch", "--", "code", "api"])
        );
        // Any run of white space is one split, and the ends are nothing.
        assert_eq!(
            open("  code \t api  "),
            words(&["open", "--json", "--no-switch", "--", "code", "api"])
        );
        assert_eq!(
            open("api"),
            words(&["open", "--json", "--no-switch", "--", "api"])
        );
        // A word that starts with a dash is still a term, after `--`.
        assert_eq!(
            open("-odd api"),
            words(&["open", "--json", "--no-switch", "--", "-odd", "api"])
        );
        assert_eq!(
            open("--help"),
            words(&["open", "--json", "--no-switch", "--", "--help"])
        );
        assert!(open("   ").is_empty());
    }

    #[test]
    fn a_new_worktree_never_focuses_a_client_over_there() {
        let made = argv(Action::NewWorktree {
            workspace: "$3".into(),
            branch: "x".into(),
        });
        assert!(made.contains(&"--no-focus".to_string()), "{made:?}");
        assert!(!made.contains(&"--focus".to_string()), "{made:?}");
    }

    #[test]
    fn a_name_or_a_branch_is_one_word_whatever_it_holds() {
        for hostile in HOSTILE {
            let name = hostile.to_string();
            assert_eq!(
                argv(Action::RenameAgent {
                    pane: "%1".into(),
                    name: name.clone()
                }),
                words(&["agent", "rename", "%1", hostile])
            );
            assert_eq!(
                argv(Action::RenameWorkspace {
                    session: "$1".into(),
                    name: name.clone()
                }),
                words(&["workspace", "rename", "$1", hostile])
            );
            assert_eq!(
                argv(Action::RenameWindow {
                    window: "@1".into(),
                    name: name.clone()
                }),
                words(&["tab", "rename", "@1", hostile])
            );
            assert_eq!(
                argv(Action::NewWorktree {
                    workspace: "$1".into(),
                    branch: name
                }),
                words(&[
                    "worktree",
                    "create",
                    "--workspace",
                    "$1",
                    "--branch",
                    hostile,
                    "--no-focus"
                ])
            );
        }
    }

    #[test]
    fn a_name_that_starts_with_a_dash_follows_a_double_dash_in_the_three_renames() {
        assert_eq!(
            argv(Action::RenameAgent {
                pane: "%1".into(),
                name: "-x".into()
            }),
            words(&["agent", "rename", "%1", "--", "-x"])
        );
        assert_eq!(
            argv(Action::RenameWorkspace {
                session: "$1".into(),
                name: "--clear".into()
            }),
            words(&["workspace", "rename", "$1", "--", "--clear"])
        );
        assert_eq!(
            argv(Action::RenameWindow {
                window: "@1".into(),
                name: "-".into()
            }),
            words(&["tab", "rename", "@1", "--", "-"])
        );
    }

    #[test]
    fn a_name_that_is_exactly_a_double_dash_is_one_word_after_the_verbs_own() {
        for action in [
            Action::RenameWorkspace {
                session: "$1".into(),
                name: "--".into(),
            },
            Action::RenameWindow {
                window: "@1".into(),
                name: "--".into(),
            },
            Action::RenameAgent {
                pane: "%1".into(),
                name: "--".into(),
            },
        ] {
            let words = argv(action.clone());
            let dashes: Vec<usize> = words
                .iter()
                .enumerate()
                .filter(|(_, word)| *word == "--")
                .map(|(at, _)| at)
                .collect();
            assert_eq!(
                dashes,
                [words.len() - 2, words.len() - 1],
                "the verb's own `--`, then the name as one word: {words:?} for {action:?}"
            );
        }
    }

    #[test]
    fn a_branch_that_starts_with_a_dash_is_joined_to_its_flag() {
        assert_eq!(
            argv(Action::NewWorktree {
                workspace: "$1".into(),
                branch: "--base".into()
            }),
            words(&[
                "worktree",
                "create",
                "--workspace",
                "$1",
                "--branch=--base",
                "--no-focus"
            ])
        );
    }

    #[test]
    fn an_id_the_verb_does_not_take_gives_no_argv() {
        let bad = [
            "",
            "%",
            "$",
            "@",
            "16",
            "%x",
            "%1 2",
            "studio:%16",
            "w5",
            "%1;ls",
            "%-1",
            "%1\n",
            "%\u{661}",
            "-%1",
            "%1%2",
            " %1",
        ];
        for id in bad {
            assert!(
                argv(Action::KillPane { pane: id.into() }).is_empty(),
                "pane {id:?}"
            );
            assert!(
                argv(Action::RenameAgent {
                    pane: id.into(),
                    name: "n".into()
                })
                .is_empty(),
                "agent {id:?}"
            );
        }
        // A run of digits as long as the cleaner's cut is not an id.
        let long = format!("%{}", "7".repeat(256));
        assert!(argv(Action::KillPane { pane: long }).is_empty());
        assert_eq!(
            argv(Action::KillPane {
                pane: format!("%{}", "7".repeat(20))
            })
            .len(),
            3
        );
        // Each verb takes its own sigil and no other: a session id is not a pane's.
        assert!(argv(Action::KillPane { pane: "$3".into() }).is_empty());
        assert!(argv(Action::KillPane { pane: "@3".into() }).is_empty());
        assert!(
            argv(Action::KillWorkspace {
                session: "%3".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::KillGroup {
                session: "@3".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::RenameWorkspace {
                session: "%3".into(),
                name: "n".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::RenameWindow {
                window: "$3".into(),
                name: "n".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::NewWorktree {
                workspace: "%3".into(),
                branch: "b".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::NewWorktree {
                workspace: "w3".into(),
                branch: "b".into()
            })
            .is_empty()
        );
    }

    #[test]
    fn a_closed_worktree_is_named_by_its_workspace_id_alone() {
        for id in ["w5", "w0", "w1234567"] {
            assert_eq!(
                argv(Action::RemoveWorktree {
                    workspace: id.into(),
                    force: false
                })
                .len(),
                4,
                "{id}"
            );
        }
        let long = format!("w{}", "9".repeat(300));
        let bad = [
            "",
            "w",
            "5",
            "$5",
            "%5",
            "w5:t7",
            "w5:p31",
            "wx",
            "w-5",
            "w5 ",
            "w5\n",
            "W5",
            "studio:w5",
            "/home/me/code/garden",
            "feat/rain",
            "repo--feat-rain",
            long.as_str(),
        ];
        for id in bad {
            for force in [false, true] {
                assert!(
                    argv(Action::RemoveWorktree {
                        workspace: id.into(),
                        force
                    })
                    .is_empty(),
                    "{id:?}"
                );
            }
        }
    }

    #[test]
    fn a_word_that_cannot_be_an_argument_gives_no_argv() {
        assert!(
            argv(Action::RenameWindow {
                window: "@1".into(),
                name: "a\0b".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::RenameAgent {
                pane: "%1".into(),
                name: "a\0b".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::RenameWorkspace {
                session: "$1".into(),
                name: "\0".into()
            })
            .is_empty()
        );
        assert!(
            argv(Action::NewWorktree {
                workspace: "$1".into(),
                branch: "a\0b".into()
            })
            .is_empty()
        );
    }

    #[test]
    fn an_action_with_no_verb_over_there_gives_no_argv() {
        let none = [
            Action::Jump {
                session: "$1".into(),
                window: "@1".into(),
                pane: "%1".into(),
            },
            Action::SwitchWorkspace {
                session: "$1".into(),
            },
            Action::OpenProject {
                path: "/home/me/code".into(),
            },
            Action::NextUp { own: None },
            Action::OpenWorktree {
                workspace: "w5".into(),
            },
            Action::Doorway {
                machine: "studio".into(),
                id: "%16".into(),
            },
        ];
        for action in none {
            assert!(remote_argv(&action).is_empty(), "{action:?}");
        }
    }

    #[test]
    fn the_ids_the_verbs_take_are_the_ones_the_far_taw_parses() {
        use crate::tmux::ids::{Target, parse};
        assert!(matches!(parse("%16"), Some(Target::Pane(_))));
        assert!(matches!(parse("@5"), Some(Target::Tab(_))));
        assert!(matches!(parse("$3"), Some(Target::Workspace(_))));
        // Every id this module lets through is one `ids::parse` reads as the kind the verb wants.
        for action in [
            Action::KillPane { pane: "%16".into() },
            Action::KillWorkspace {
                session: "$3".into(),
            },
            Action::RenameWindow {
                window: "@5".into(),
                name: "n".into(),
            },
        ] {
            let args = remote_argv(&action);
            assert!(parse(&args[2]).is_some(), "{args:?}");
        }
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    // The envelope lines below are what the far `taw` printed for each verb, run once in a private tmux
    // server with a scratch home (the far verbs `pane close`, `workspace close`, `worktree create`
    // and `worktree remove`, with `[hooks]` set for the refusals). Only the scratch folder in a path is
    // replaced, by `/home/studio`.
    const NOT_FOUND: &str = r#"{"id":"cli:pane:close","error":{"code":"not_found","message":"pane '%9999' not found"}}"#;
    const GROUP: &str = r#"{"id":"cli:workspace:close","error":{"code":"workspace_group_close_required","message":"workspace '$0' has worktree workspaces still open: w2 (repo--feat-x); pass --group to close them together with it"}}"#;
    const DIRTY: &str = r#"{"id":"cli:worktree:remove","error":{"code":"worktree_dirty","message":"git [\"worktree\", \"remove\", \"--\", \"/home/studio/co/repo--feat-x\"] failed (exit status: 128): fatal: '/home/studio/co/repo--feat-x' contains modified or untracked files, use --force to delete it"}}"#;
    const NOT_A_REPO: &str = r#"{"id":"cli:worktree:create","error":{"code":"not_a_git_repo","message":"not_a_git_repo: '/home/studio/notes' is not inside a git repository"}}"#;
    const PRE_CLOSE: &str = r#"{"id":"cli:pane:close","error":{"code":"hook_refused","message":"pre-close hook echo noise >&2; echo uncommitted changes in ~/code/garden >&2; exit 1 failed (exit status: 1): noise\nuncommitted changes in /home/studio/code/garden"}}"#;
    const PRE_REMOVE: &str = r#"{"id":"cli:worktree:remove","error":{"code":"hook_refused","message":"pre-worktree-remove hook echo has a dirty tree: do not >&2; exit 1 failed (exit status: 1): has a dirty tree: do not"}}"#;
    const SILENT: &str = r#"{"id":"cli:pane:close","error":{"code":"hook_refused","message":"pre-close hook exit 3 failed (exit status: 3)"}}"#;
    const SLOW: &str = r#"{"id":"cli:pane:close","error":{"code":"hook_refused","message":"pre-close hook took longer than 1 s"}}"#;
    const CLOSED: &str = r#"{"id":"cli:pane:close","result":{"terminal_id":"%2","type":"ok"}}"#;

    fn done(code: i32, stdout: &str) -> Ran {
        Ran::Done {
            code: Some(code),
            signal: None,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// An old taw's own clap error for `open --json --no-switch -- x` (a real run's stderr), exit 2.
    const OLD_TAW_STDERR: &str = "error: unexpected argument '--no-switch' found\n\n  tip: to pass '--no-switch' as a value, use '-- --no-switch'\n\nUsage: taw open --json [TERMS]...\n\nFor more information, try '--help'.\n";

    fn ended_with(code: i32, stderr: &str) -> Reply {
        classify(
            "studio",
            Ran::Done {
                code: Some(code),
                signal: None,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            },
            VERB_BOUND,
        )
    }

    /// taw 0.1.2's own clap error for `session candidates --json` (master's release build under `env -i` with a
    /// scratch HOME: exit 2, nothing on stdout) is an old taw; a wrapper naming the word is not.
    /// Mutation: `clap_refused_candidates` answers false.
    #[test]
    fn an_old_taws_own_error_for_session_candidates_is_an_old_taw() {
        let said = "error: unrecognized subcommand 'candidates'\n\nUsage: taw session [OPTIONS] <COMMAND>\n\nFor more information, try '--help'.\n";
        assert_eq!(ended_with(2, said), Reply::OldTaw);
        assert_eq!(
            ended_with(2, "wrapper: candidates is not allowed\n"),
            Reply::Usage("wrapper: candidates is not allowed".into())
        );
        assert_eq!(candidates_argv(), ["session", "candidates", "--json"]);
    }

    /// The far list reads its folders in order, and the optional `truncated` and `sessions_error` when present;
    /// missing keys are false and none, and an answer that is not the envelope is none.
    #[test]
    fn the_far_list_reads_its_folders_and_its_optional_keys() {
        let plain = br#"{"id":"cli:session:candidates","result":{"type":"session_candidates","candidates":[{"kind":"session","name":"~/api","dir":"/home/me/api","session":"api"},{"kind":"zoxide","name":"~/web","dir":"/home/me/web"},{"kind":"zoxide","name":"","dir":"/x"}]}}"#;
        let list = candidates_offered(plain).expect("the envelope");
        assert_eq!(
            list.folders
                .iter()
                .map(|f| (
                    f.kind.as_str(),
                    f.name.as_str(),
                    f.dir.as_str(),
                    f.session.as_deref()
                ))
                .collect::<Vec<_>>(),
            [
                ("session", "~/api", "/home/me/api", Some("api")),
                ("zoxide", "~/web", "/home/me/web", None)
            ]
        );
        assert!(!list.truncated);
        assert_eq!(list.sessions_error, None);
        let both = br#"{"id":"cli:session:candidates","result":{"type":"session_candidates","candidates":[{"kind":"zoxide","name":"~/web","dir":"/home/me/web"}],"truncated":true,"sessions_error":"error connecting to /tmp/tmux-501/default (Permission denied)"}}"#;
        let list = candidates_offered(both).expect("the envelope");
        assert!(list.truncated);
        assert_eq!(
            list.sessions_error.as_deref(),
            Some("error connecting to /tmp/tmux-501/default (Permission denied)")
        );
        assert_eq!(candidates_offered(b"not json"), None);
        assert_eq!(candidates_offered(br#"{"result":{"type":"ok"}}"#), None);
    }

    #[test]
    fn status_2_is_an_old_taw_only_when_its_own_clap_refused_open() {
        assert_eq!(ended_with(2, OLD_TAW_STDERR), Reply::OldTaw);
        assert_eq!(
            ended_with(2, "error: unrecognized subcommand 'open'\n"),
            Reply::OldTaw
        );
        // A wrapper's message that names the option is not clap speaking.
        assert_eq!(
            ended_with(2, "wrapper: no --no-switch here\n"),
            Reply::Usage("wrapper: no --no-switch here".into())
        );
        assert_eq!(ended_with(2, ""), Reply::Usage(String::new()));
        // Another status with the same text is no usage error.
        assert!(matches!(ended_with(1, OLD_TAW_STDERR), Reply::Refused(_)));
        // The first line only, cleaned.
        assert_eq!(
            ended_with(2, "\n  \x1b]0;x\x07oops\nmore\n"),
            Reply::Usage("oops".into())
        );
    }

    fn reply(code: i32, line: &str) -> Reply {
        classify("studio", done(code, &format!("{line}\n")), VERB_BOUND)
    }

    fn hook(label: &str, why: &str) -> Reply {
        Reply::HookRefused {
            label: label.into(),
            why: why.into(),
        }
    }

    #[test]
    fn an_exit_of_zero_is_done_whatever_was_written() {
        assert_eq!(reply(0, CLOSED), Reply::Done);
        assert_eq!(
            classify("studio", done(0, "Last login: Mon\n"), VERB_BOUND),
            Reply::Done
        );
        assert_eq!(classify("studio", done(0, ""), VERB_BOUND), Reply::Done);
    }

    #[test]
    fn each_code_the_far_taw_reports_is_its_own_reply() {
        assert_eq!(reply(1, GROUP), Reply::GroupCloseRequired);
        assert_eq!(reply(1, DIRTY), Reply::Dirty);
        assert_eq!(reply(1, NOT_FOUND), Reply::Gone);
        assert_eq!(
            reply(1, NOT_A_REPO),
            Reply::Refused(
                "studio: not_a_git_repo: '/home/studio/notes' is not inside a git repository"
                    .into()
            )
        );
    }

    #[test]
    fn a_hook_that_refused_is_named_and_says_its_last_line() {
        assert_eq!(
            reply(1, PRE_CLOSE),
            hook(
                "pre-close hook",
                "uncommitted changes in /home/studio/code/garden"
            )
        );
        assert_eq!(
            reply(1, PRE_REMOVE),
            hook("pre-worktree-remove hook", "has a dirty tree: do not")
        );
        // The words the app shows are the shared ones, with the machine named.
        let Reply::HookRefused { label, why } = reply(1, PRE_CLOSE) else {
            panic!("not a hook refusal");
        };
        assert_eq!(
            words::hook_refused("studio", &label, &why),
            "pre-close hook on studio: uncommitted changes in /home/studio/code/garden"
        );
    }

    #[test]
    fn a_hook_that_wrote_nothing_or_ran_out_of_time_still_says_something() {
        assert_eq!(
            reply(1, SILENT),
            hook("pre-close hook", "refused (exit status: 3)")
        );
        assert_eq!(
            reply(1, SLOW),
            hook("pre-close hook", "took longer than 1 s")
        );
    }

    #[test]
    fn a_command_that_holds_the_separator_does_not_move_the_reason() {
        // The command comes before ` failed (`, so a `): ` in it is not the end of the status; blank lines
        // and a trailing newline in what the hook wrote are skipped.
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-close hook if (x): then fi failed (exit status: 1): first\nreason: not now\n\n"}}"#;
        assert_eq!(reply(1, line), hook("pre-close hook", "reason: not now"));
        // The same command, with nothing written: the reason is the status, not the rest of the command.
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-close hook if (x): exit 1 fi failed (exit status: 1)"}}"#;
        assert_eq!(
            reply(1, line),
            hook("pre-close hook", "refused (exit status: 1)")
        );
        // A status with parentheses of its own.
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-close hook run failed (signal: 9 (SIGKILL))"}}"#;
        assert_eq!(
            reply(1, line),
            hook("pre-close hook", "refused (signal: 9 (SIGKILL))")
        );
    }

    #[test]
    fn a_command_that_holds_failed_paren_does_not_spoil_the_reason_of_a_silent_hook() {
        // The person's own command has ` failed (` in it and the hook wrote nothing.
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-close hook grep -q 'build failed (' log && exit 1 failed (exit status: 1)"}}"#;
        assert_eq!(
            reply(1, line),
            hook("pre-close hook", "refused (exit status: 1)")
        );
        // And with something written, the last line of it.
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-close hook grep -q 'build failed (' log && exit 1 failed (exit status: 1): the log says no"}}"#;
        assert_eq!(reply(1, line), hook("pre-close hook", "the log says no"));
    }

    #[test]
    fn the_far_text_that_reaches_a_toast_is_cleaned_and_cut() {
        // An escape sequence is stripped whole, in a refusal and in a hook's reason and label.
        // As JSON writes them (the envelope is JSON): \u001b is ESC, \u0007 is BEL.
        let esc = r"\u001b]0;owned\u0007\u001b[31mred\u001b[0m";
        let line =
            format!(r#"{{"id":"x","error":{{"code":"internal","message":"bad {esc} end"}}}}"#);
        assert_eq!(
            reply(1, &line),
            Reply::Refused("studio: bad red end".into())
        );
        let line = format!(
            r#"{{"id":"x","error":{{"code":"hook_refused","message":"pre-close hook run failed (exit status: 1): {esc} no"}}}}"#
        );
        assert_eq!(reply(1, &line), hook("pre-close hook", "red no"));
        // An over-long message is cut at the cleaner's cap, not carried whole.
        let long = "x".repeat(5_000);
        let line = format!(r#"{{"id":"x","error":{{"code":"internal","message":"{long}"}}}}"#);
        let Reply::Refused(said) = reply(1, &line) else {
            panic!("not a refusal");
        };
        assert!(
            said.chars().count() <= "studio: ".len() + 256,
            "{}",
            said.len()
        );
        let line = format!(
            r#"{{"id":"x","error":{{"code":"hook_refused","message":"pre-close hook run failed (exit status: 1): {long}"}}}}"#
        );
        let Reply::HookRefused { why, .. } = reply(1, &line) else {
            panic!("not a hook refusal");
        };
        assert!(why.chars().count() <= 256);
    }

    #[test]
    fn a_create_hook_is_read_by_the_same_rule() {
        let line = r#"{"id":"x","error":{"code":"hook_refused","message":"pre-create hook ./check failed (exit status: 2): no branch named wip"}}"#;
        assert_eq!(
            reply(1, line),
            hook("pre-create hook", "no branch named wip")
        );
    }

    #[test]
    fn the_envelope_is_the_last_line_after_whatever_a_login_shell_said() {
        let said = format!("Welcome to studio\n\n{NOT_FOUND}\n\n");
        assert_eq!(classify("studio", done(1, &said), VERB_BOUND), Reply::Gone);
    }

    #[test]
    fn ssh_that_could_not_talk_is_no_answer() {
        assert_eq!(
            classify("studio", done(255, ""), VERB_BOUND),
            Reply::NoAnswer
        );
        // Even when something that looks like an envelope came with it.
        assert_eq!(
            classify("studio", done(255, NOT_FOUND), VERB_BOUND),
            Reply::NoAnswer
        );
        let killed = Ran::Done {
            code: None,
            signal: Some(9),
            stdout: NOT_FOUND.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(classify("studio", killed, VERB_BOUND), Reply::NoAnswer);
        assert_eq!(
            classify("studio", Ran::Failed("no ssh".into()), VERB_BOUND),
            Reply::NoAnswer
        );
        assert_eq!(classify("studio", Ran::Ended, VERB_BOUND), Reply::NoAnswer);
    }

    #[test]
    fn the_bound_that_passed_is_a_timeout_naming_its_seconds() {
        assert_eq!(
            classify("studio", Ran::TimedOut, VERB_BOUND),
            Reply::TimedOut { secs: 20 }
        );
        assert_eq!(
            classify("studio", Ran::TimedOut, Duration::from_secs(60)),
            Reply::TimedOut { secs: 60 }
        );
    }

    #[test]
    fn an_exit_with_no_envelope_is_a_refusal_in_words_not_a_success() {
        assert_eq!(
            classify(
                "studio",
                done(127, "bash: taw: command not found\n"),
                VERB_BOUND
            ),
            Reply::Refused("studio: it ended with status 127 and no answer taw can read".into())
        );
        // An envelope with a result and a failing exit, or an error with no code, is no answer either.
        for line in [CLOSED, r#"{"error":{"message":"x"}}"#, "[1,2]"] {
            assert!(
                matches!(reply(1, line), Reply::Refused(_)),
                "{line}: {:?}",
                reply(1, line)
            );
        }
    }

    #[test]
    fn a_verb_has_twenty_seconds_and_a_group_twenty_for_each_session() {
        assert_eq!(VERB_BOUND, Duration::from_secs(20));
        let pane = Action::KillPane { pane: "%1".into() };
        assert_eq!(bound(&pane, 3), Duration::from_secs(20));
        let group = Action::KillGroup {
            session: "$1".into(),
        };
        assert_eq!(bound(&group, 1), Duration::from_secs(20));
        assert_eq!(bound(&group, 3), Duration::from_secs(60));
        assert_eq!(bound(&group, 0), Duration::from_secs(20));
    }
}
