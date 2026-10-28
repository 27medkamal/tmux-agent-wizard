// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! `taw pane clear`: herdr's `pane.clear` over a tmux pane.
//!
//! herdr's clear drops a pane's screen and its scrollback, keeps the line the cursor is on at the top,
//! sends the program in the pane nothing, and does nothing on the alternate screen, where a full-screen
//! program such as Vim keeps its own. herdr has no command line verb for it, only the socket method and
//! the `keys.clear_pane` key, which is unbound until it is set.
//!
//! tmux can clear a pane's history (`clear-history`) and cannot clear its screen: that belongs to the
//! program in the pane, and the one way to a clean screen without resetting the pane's terminal, which
//! would undo the modes the program set, is to ask. So:
//!
//! - On the alternate screen (`#{alternate_on}`) nothing is done.
//! - Otherwise the history is cleared.
//! - And where bash, zsh or fish is at its prompt, which is [`prompt::at_prompt`]'s test with the shell
//!   in front being the pane's own, the shell is sent Ctrl-L. It clears its screen and draws its prompt
//!   and the line being typed at the top, which is the line herdr keeps.
//! - With anything else in front, a program that would take Ctrl-L for input or a shell with no line
//!   editing to answer it (a plain `sh`), the screen is left as it is and no key is sent. Nor is one
//!   to a pane in a window with `synchronize-panes` on, where tmux copies a key sent to one pane to
//!   all the others, whatever is in front of them.
//!
//! The order matters: when a shell clears its screen, tmux moves what was on it into the pane's
//! history (`scroll-on-clear`, on by default), so the history is cleared after the shell has redrawn.
//! The verb sends the key, waits until the pane has changed and held still, and clears the history
//! then. It clears it before as well, which takes a pane out of copy mode, where the key would go to
//! the mode and not to the shell. Every clear of the history is one command the server runs, which
//! looks at `#{alternate_on}` and clears or not and says which, so a program that goes to the
//! alternate screen between the verb's look at the pane and its clear keeps the history under it.
//!
//! A screen that is clear already, the prompt at the top and nothing else on it, is not asked for
//! anything: it is what a clear leaves, a Ctrl-L would show no change (with scroll-on-clear off, none
//! at all), and the shell it might be sent to could take it for text.
//!
//! Some shells take Ctrl-L for text: bash with readline in vi mode inserts a literal `^L` at the
//! cursor (zsh keeps clear-screen on Ctrl-L in its vi mode; fish has not been tried), and a zsh busy
//! in a builtin of its own such as `read` has the terminal echo it as `^L` and read it as a form feed.
//! Nothing outside the shell says which, so the verb watches what the key did, and a `^L` that is on
//! the screen now and was not is the key taken as text, wherever it landed: the verb takes it out
//! again with one Backspace, leaves the screen alone, and says so. It says the shell redrew only on
//! evidence of a clear (the old screen in the history, the cursor's row moved up the screen), and
//! that it did not answer when there is none.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::cli::Ctx;
use crate::cli::common;
use crate::cli::envelope::CliError;
use crate::session::prompt::{self, AtPrompt, Timing};
use crate::tmux::{Tmux, ids};

/// What the reply says when the pane is on the alternate screen, where nothing is done.
const ON_THE_ALTERNATE_SCREEN: &str = "the pane is on the alternate screen, as a full screen program leaves it, so nothing was changed";

/// What the reply says when the screen was left alone because no shell that answers Ctrl-L is at its
/// prompt.
const HISTORY_ONLY: &str = "the history was cleared and the screen was left as it is, because it is cleared only when bash, zsh or fish is at its prompt";

/// What the reply says when the screen was left alone because the pane is synchronized with the others
/// in its window, which a key sent to it would reach as well.
const SYNCHRONIZED: &str = "the history was cleared and the screen was left as it is, because the pane is synchronized with the others in its window and a key sent to it would reach them too";

/// What the reply says when the shell took Ctrl-L as text.
const TOOK_IT_AS_TEXT: &str = "the shell took Ctrl-L as text, which was taken out of its line again, so the screen is as it was";

/// What the reply says when the shell took Ctrl-L as text and a Backspace did not take it out.
const TEXT_STAYED: &str = "the shell took Ctrl-L as text and a Backspace did not take it out of its line again, so a ^L may be left in it";

/// What the reply says when the shell showed no sign of having answered.
const NO_ANSWER: &str = "the shell did not answer Ctrl-L, so the screen is as it was";

/// How long a shell gets to be at its prompt. A shell that is idle and has not held its screen still
/// yet is looked at again for this long; one with a program in front of it is not waited for.
const PROMPT_BOUND: Duration = Duration::from_secs(2);

/// How long the shell gets to answer Ctrl-L: the longest the verb waits for a shell that shows no
/// sign, such as one whose screen was clear already and that tmux was told not to scroll.
const ANSWER_BOUND: Duration = Duration::from_secs(3);

/// How often the pane is looked at while the shell answers.
const ANSWER_POLL: Duration = Duration::from_millis(20);

/// How long the pane has to stay as the answer left it for the answer to be over: a shell clears the
/// screen and then draws its prompt, and may do it in more than one write.
const ANSWER_QUIET: Duration = Duration::from_millis(100);

/// What `pane clear` did to a pane, which the reply tells.
#[derive(Debug, PartialEq, Eq)]
struct Done {
    /// Whether the pane's history was cleared.
    history: bool,
    /// Whether the pane's screen was cleared, with its prompt redrawn at the top.
    screen: bool,
    /// Why not, when something was left alone.
    note: Option<&'static str>,
}

impl Done {
    fn untouched() -> Done {
        Done {
            history: false,
            screen: false,
            note: Some(ON_THE_ALTERNATE_SCREEN),
        }
    }

    /// Only the history was cleared, for the reason `note` gives.
    fn history_only(note: &'static str) -> Done {
        Done {
            history: true,
            screen: false,
            note: Some(note),
        }
    }

    fn redrawn() -> Done {
        Done {
            history: true,
            screen: true,
            note: None,
        }
    }

    /// What the shell's answer left, `history` being whether the history was cleared at the end, which
    /// it is not when the pane went to the alternate screen while the shell answered.
    fn after(answer: Answer, history: bool) -> Done {
        let (screen, note) = match answer {
            Answer::Redrawn => (true, None),
            Answer::Text => (false, Some(TOOK_IT_AS_TEXT)),
            Answer::TextStayed => (false, Some(TEXT_STAYED)),
            Answer::None => (false, Some(NO_ANSWER)),
        };
        if history {
            Done {
                history,
                screen,
                note,
            }
        } else {
            Done {
                history,
                screen,
                note: Some(ON_THE_ALTERNATE_SCREEN),
            }
        }
    }
}

pub(super) fn clear(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<Value> {
    let t = &ctx.tmux;
    let pane = common::pane_target(t, positional, pane_flag)?;
    let done = clear_pane(t, &pane)?;
    let info = common::pane_info(t, &pane)?;
    let mut reply = json!({
        "pane_id": ids::pane_id(&info.session_id, &info.pane_id),
        "history_cleared": done.history,
        "screen_cleared": done.screen,
    });
    if let Some(note) = done.note {
        reply["note"] = json!(note);
    }
    Ok(reply)
}

/// What is in front of a pane, as far as this verb cares.
struct Front {
    /// Whether a full-screen program has the pane on the alternate screen.
    alternate: bool,
    /// Whether the pane's program has ended and tmux keeps the pane (`remain-on-exit`).
    dead: bool,
    /// Whether the pane is synchronized with the others in its window (`synchronize-panes`), where a
    /// key sent to one pane reaches them all, a program in front of one of them included.
    synchronized: bool,
    /// `#{pane_current_command}`: the program that has the terminal, as tmux names it.
    command: String,
}

/// Reads what is in front of `pane`. The pane's own id is read with it, because `display-message`
/// answers for a pane that has just been closed with a line of empty fields and does not fail.
fn front(t: &Tmux, pane: &str) -> anyhow::Result<Front> {
    let line = t
        .display(
            pane,
            "#{pane_id}\t#{alternate_on}\t#{pane_dead}\t#{pane_synchronized}\t#{pane_current_command}",
        )
        .map_err(|e| CliError::tmux(format!("reading pane '{pane}': {e:#}")))?;
    let mut fields = line.splitn(5, '\t');
    if fields.next().is_none_or(str::is_empty) {
        return Err(not_found(pane));
    }
    let alternate = fields.next() == Some("1");
    let dead = fields.next() == Some("1");
    let synchronized = fields.next() == Some("1");
    let command = fields.next().unwrap_or_default().to_string();
    Ok(Front {
        alternate,
        dead,
        synchronized,
        command,
    })
}

/// Whether `command`, the name tmux gives the program in front of a pane, is a shell that answers
/// Ctrl-L by clearing the screen and redrawing its prompt: bash, zsh or fish, which tmux names by the
/// last part of the path it was run by, without the `-` a login shell is run with. `sh`, and `dash`
/// and `ksh` with it, have no line editing to answer with, or none that is told apart by name: a plain
/// `sh` reads the key as a character.
fn answers_ctrl_l(command: &str) -> bool {
    let name = command.trim().rsplit('/').next().unwrap_or_default();
    matches!(
        name.trim_start_matches('-').to_ascii_lowercase().as_str(),
        "bash" | "zsh" | "fish"
    )
}

/// Clears `pane` as far as tmux can, as the module's documentation says.
fn clear_pane(t: &Tmux, pane: &str) -> anyhow::Result<Done> {
    let front = front(t, pane)?;
    if front.alternate {
        return Ok(Done::untouched());
    }
    if front.dead || !answers_ctrl_l(&front.command) {
        return history_only(t, pane, HISTORY_ONLY);
    }
    if front.synchronized {
        return history_only(t, pane, SYNCHRONIZED);
    }
    let timing = Timing {
        bound: PROMPT_BOUND,
        ..Timing::default()
    };
    match prompt::at_prompt(t, pane, timing)? {
        AtPrompt::Yes => redraw(t, pane),
        AtPrompt::No => history_only(t, pane, HISTORY_ONLY),
        AtPrompt::Gone => Err(not_found(pane)),
    }
}

/// Clears the history and leaves the screen, for the reason `note` gives; or, if the pane has gone to
/// the alternate screen since it was looked at, nothing.
fn history_only(t: &Tmux, pane: &str, note: &'static str) -> anyhow::Result<Done> {
    Ok(if clear_history(t, pane)? {
        Done::history_only(note)
    } else {
        Done::untouched()
    })
}

/// Sends the shell at its prompt Ctrl-L, and clears the history once it has answered.
fn redraw(t: &Tmux, pane: &str) -> anyhow::Result<Done> {
    // Before the key, so the pane is out of copy mode, where the key would go to the mode and not to
    // the shell. After it is the clear that counts.
    if !clear_history(t, pane)? {
        return Ok(Done::untouched());
    }
    let before = look(t, pane)?;
    // The prompt at the top of a screen with nothing else on it is what a clear leaves. A Ctrl-L would
    // change nothing to be seen (with scroll-on-clear off, nothing at all), so there is no answer to
    // wait for, and nothing to ask of a shell that might take the key for text.
    if already_clear(&before) {
        return Ok(Done::redrawn());
    }
    t.run_ok(&["send-keys", "-t", pane, "C-l"])
        .map_err(|e| gone_or_tmux(pane, "sending Ctrl-L to", &e))?;
    let answer = wait_for_the_answer(t, pane, &before)?;
    let cleared = clear_history(t, pane)?;
    Ok(Done::after(answer, cleared))
}

/// Whether the screen is what a clear leaves: the cursor on the first row and nothing on any row
/// below it.
fn already_clear(look: &Look) -> bool {
    look.y == 0 && !look.rows.is_empty() && look.rows.iter().skip(1).all(String::is_empty)
}

/// How the shell answered Ctrl-L.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// It cleared its screen and drew its prompt and line at the top.
    Redrawn,
    /// It put a `^L` into the line, which has been taken out again.
    Text,
    /// It put a `^L` into the line, which a Backspace did not take out.
    TextStayed,
    /// Nothing changed in the pane while it was given.
    None,
}

/// What a pane showed at one look: enough to see that it has changed, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Look {
    /// Lines of history.
    history: usize,
    /// The cursor's column and row on the screen.
    x: usize,
    y: usize,
    /// The screen's rows, without their trailing spaces, which tmux leaves off.
    rows: Vec<String>,
}

/// Looks at `pane`; an error when it has gone.
fn look(t: &Tmux, pane: &str) -> anyhow::Result<Look> {
    let line = t
        .display(
            pane,
            "#{pane_id}\t#{history_size}\t#{cursor_x}\t#{cursor_y}",
        )
        .map_err(|e| gone_or_tmux(pane, "reading", &e))?;
    let mut fields = line.split('\t');
    if fields.next().is_none_or(str::is_empty) {
        return Err(not_found(pane));
    }
    let mut number = || fields.next().and_then(|n| n.trim().parse().ok());
    let (history, x, y) = (
        number().unwrap_or(0),
        number().unwrap_or(0),
        number().unwrap_or(0),
    );
    let screen = t
        .capture_pane(pane, false)
        .map_err(|e| gone_or_tmux(pane, "reading", &e))?;
    Ok(Look {
        history,
        x,
        y,
        rows: rows_of(&screen),
    })
}

/// The rows of a capture of a screen. `capture-pane` ends every row with a newline, and `Tmux::run`
/// takes one off the end, so splitting on newlines gives as many rows as the pane has, the last of them
/// empty when nothing is on it; reading the text by lines would lose that row, and with it the cursor's
/// row when the cursor is on it. Without their trailing spaces, which tmux leaves off.
fn rows_of(screen: &str) -> Vec<String> {
    screen
        .split('\n')
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// Waits for the shell to answer a Ctrl-L sent when the pane looked as `before` says, for at most
/// [`ANSWER_BOUND`]: for the pane to change, and then to hold still for [`ANSWER_QUIET`], so a prompt
/// drawn in more than one write has been drawn. If the change was a `^L` in the line, takes it out.
fn wait_for_the_answer(t: &Tmux, pane: &str, before: &Look) -> anyhow::Result<Answer> {
    let deadline = Instant::now() + ANSWER_BOUND;
    let mut now = loop {
        std::thread::sleep(ANSWER_POLL);
        let seen = look(t, pane)?;
        if seen != *before {
            break seen;
        }
        if Instant::now() >= deadline {
            return Ok(Answer::None);
        }
    };
    let mut still_since = Instant::now();
    while still_since.elapsed() < ANSWER_QUIET && Instant::now() < deadline {
        std::thread::sleep(ANSWER_POLL);
        let seen = look(t, pane)?;
        if seen != now {
            now = seen;
            still_since = Instant::now();
        }
    }
    match judge(before, &now) {
        Answer::Text => take_it_back(t, pane, caret_ls(&before.rows)),
        answer => Ok(answer),
    }
}

/// How many `^L` the screen shows: what a Ctrl-L typed as text looks like, whether a shell put it in
/// its line or the terminal echoed it. The rows are joined with nothing between them, so a `^` that
/// ends a row and the `L` that begins the next, which is how two columns that do not fit look, count.
fn caret_ls(rows: &[String]) -> usize {
    rows.concat().matches("^L").count()
}

/// How the shell took the key, from the pane as it was and as it is. A `^L` that was not on the
/// screen is the key taken as text, whatever else changed with it (a line pushed past the right edge
/// wraps, and at the bottom of the screen scrolls it), and no clear of a screen makes one. Else the
/// shell has redrawn if there is something that only a clear does: the old screen gone into the
/// history, or the cursor's row moved up the screen. A pane that changed in neither way has not
/// answered, whatever else it did.
fn judge(before: &Look, after: &Look) -> Answer {
    if caret_ls(&after.rows) == caret_ls(&before.rows) + 1 {
        Answer::Text
    } else if after.history > before.history || after.y < before.y {
        Answer::Redrawn
    } else {
        Answer::None
    }
}

/// Takes the `^L` the shell took for text back out of its line: one Backspace, which deletes the one
/// character before the cursor in readline's vi insert mode as in any other, or the terminal's own
/// erase of what it echoed. Then waits for the screen to show as many `^L` as it did, `wanted`, for as
/// long as an answer is waited for.
fn take_it_back(t: &Tmux, pane: &str, wanted: usize) -> anyhow::Result<Answer> {
    t.run_ok(&["send-keys", "-t", pane, "BSpace"])
        .map_err(|e| gone_or_tmux(pane, "sending Backspace to", &e))?;
    let deadline = Instant::now() + ANSWER_BOUND;
    while Instant::now() < deadline {
        std::thread::sleep(ANSWER_POLL);
        if caret_ls(&look(t, pane)?.rows) <= wanted {
            return Ok(Answer::Text);
        }
    }
    Ok(Answer::TextStayed)
}

/// `clear-history`: drops the pane's scrollback, and takes the pane out of copy mode (tmux 3.6b does).
/// Not when the pane is on the alternate screen, which is asked and acted on in one command the server
/// runs, so that a program that goes to it between this verb's look at the pane and its clear is left
/// as it is, the history under it included. Says whether it cleared, which the server's own answer
/// tells: `display-message -p` in either branch prints what it did.
fn clear_history(t: &Tmux, pane: &str) -> anyhow::Result<bool> {
    let clear = format!("clear-history -t {pane} ; display-message -p cleared");
    let said = t
        .run(&[
            "if-shell",
            "-F",
            "-t",
            pane,
            "#{alternate_on}",
            "display-message -p alternate",
            &clear,
        ])
        .map_err(|e| gone_or_tmux(pane, "clearing the history of", &e))?;
    Ok(said.trim() == "cleared")
}

fn not_found(pane: &str) -> anyhow::Error {
    CliError::not_found(format!("pane '{pane}' not found"))
}

/// tmux failing a command that acted on `pane`: `not_found` when the pane has gone, since it was
/// there when the verb began, else a tmux error saying what was being done.
fn gone_or_tmux(pane: &str, doing: &str, err: &anyhow::Error) -> anyhow::Error {
    if prompt::is_gone(err) {
        not_found(pane)
    } else {
        CliError::tmux(format!("{doing} pane '{pane}': {err:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look_of(history: usize, at: (usize, usize), rows: &[&str]) -> Look {
        Look {
            history,
            x: at.0,
            y: at.1,
            rows: rows.iter().map(|row| row.to_string()).collect(),
        }
    }

    #[test]
    fn bash_zsh_and_fish_answer_ctrl_l_by_the_name_tmux_gives_them() {
        for command in [
            "bash",
            "zsh",
            "fish",
            "-zsh",
            "-bash",
            "/usr/bin/zsh",
            "FISH",
        ] {
            assert!(answers_ctrl_l(command), "{command}");
        }
    }

    #[test]
    fn a_plain_sh_other_shells_programs_and_nothing_are_sent_no_key() {
        for command in [
            "sh", "dash", "ksh", "csh", "tcsh", "nu", "elvish", "xonsh", "pwsh", "claude", "node",
            "vim", "cat", "ssh", "zshrc", "bashful", "fishy", "",
        ] {
            assert!(!answers_ctrl_l(command), "{command:?}");
        }
    }

    #[test]
    fn a_capture_is_split_into_as_many_rows_as_the_pane_has_and_a_blank_last_row_is_one() {
        // `capture-pane` ends every row with a newline and `Tmux::run` takes one off the end.
        assert_eq!(rows_of("a\nb"), ["a", "b"]);
        assert_eq!(rows_of("a\n"), ["a", ""]);
        assert_eq!(rows_of("a  \n  b "), ["a", "  b"]);
        assert_eq!(rows_of("\n\n"), ["", "", ""]);
    }

    #[test]
    fn a_caret_l_split_over_two_rows_is_counted_and_so_is_one_in_the_middle_of_a_row() {
        let rows =
            |rows: &[&str]| -> Vec<String> { rows.iter().map(|row| row.to_string()).collect() };
        assert_eq!(caret_ls(&rows(&["READY> echo^L", ""])), 1);
        assert_eq!(caret_ls(&rows(&["READY> echo aaaa^", "L"])), 1);
        assert_eq!(caret_ls(&rows(&["a^Lb^L", "^L"])), 3);
        assert_eq!(caret_ls(&rows(&["^", "x"])), 0);
        assert_eq!(caret_ls(&[]), 0);
    }

    #[test]
    fn a_new_caret_l_is_a_key_taken_as_text_wherever_it_landed() {
        // At the end of the line.
        let before = look_of(5, (19, 2), &["old", "", "READY> echo partial"]);
        let after = look_of(5, (21, 2), &["old", "", "READY> echo partial^L"]);
        assert_eq!(judge(&before, &after), Answer::Text);
        // On the cursor's row with the cursor on a blank last row: nothing before it to count.
        let before = look_of(0, (0, 2), &["old", "READY> read x", ""]);
        let after = look_of(0, (2, 2), &["old", "READY> read x", "^L"]);
        assert_eq!(judge(&before, &after), Answer::Text);
        // In the middle of the line.
        let before = look_of(0, (7, 0), &["READY> echo"]);
        let after = look_of(0, (9, 0), &["READY> ^Lecho"]);
        assert_eq!(judge(&before, &after), Answer::Text);
        // Past a wide character, where a column is not a character.
        let before = look_of(0, (16, 1), &["old", "READY> echo \u{65e5}\u{672c}"]);
        let after = look_of(0, (18, 1), &["old", "READY> echo \u{65e5}\u{672c}^L"]);
        assert_eq!(judge(&before, &after), Answer::Text);
        // Pushed to the right edge, which wraps the cursor to the next row and, at the bottom of
        // the screen, scrolls: the history grows, the rows move up, and the cursor stays low.
        let before = look_of(0, (78, 2), &["l1", "l2", "READY> aaaa"]);
        let after = look_of(1, (0, 2), &["l2", "READY> aaaa^L", ""]);
        assert_eq!(judge(&before, &after), Answer::Text);
        // Split over two rows: the `^` in the last column and the `L` on the next.
        let before = look_of(0, (79, 2), &["l1", "l2", "READY> aaaa"]);
        let after = look_of(1, (1, 2), &["l2", "READY> aaaa^", "L"]);
        assert_eq!(judge(&before, &after), Answer::Text);
    }

    #[test]
    fn a_cleared_screen_with_the_prompt_at_the_top_is_a_redraw() {
        let before = look_of(0, (19, 2), &["old", "old", "READY> echo partial"]);
        // The old screen in the history, the cursor's row moved up.
        let after = look_of(3, (19, 0), &["READY> echo partial", "", ""]);
        assert_eq!(judge(&before, &after), Answer::Redrawn);
        // The cursor's row moved up, and nothing in the history (scroll-on-clear off).
        let after = look_of(0, (19, 0), &["READY> echo partial", "", ""]);
        assert_eq!(judge(&before, &after), Answer::Redrawn);
        // The history grew and the cursor is where it was: a prompt of two lines at the top.
        let before = look_of(0, (7, 1), &["first", "READY>"]);
        let after = look_of(2, (7, 1), &["first", "READY>"]);
        assert_eq!(judge(&before, &after), Answer::Redrawn);
    }

    #[test]
    fn a_pane_that_changed_in_neither_way_has_not_answered() {
        let before = look_of(0, (19, 2), &["old", "", "READY> echo partial"]);
        assert_eq!(judge(&before, &before), Answer::None);
        // Something else was printed, or typed, and the cursor did not go up.
        let printed = look_of(0, (19, 2), &["old", "output", "READY> echo partial"]);
        assert_eq!(judge(&before, &printed), Answer::None);
        let typed = look_of(0, (20, 2), &["old", "", "READY> echo partialx"]);
        assert_eq!(judge(&before, &typed), Answer::None);
        // The cursor going down is not an answer either.
        let down = look_of(0, (0, 3), &["old", "", "READY> echo partial", ""]);
        assert_eq!(judge(&before, &down), Answer::None);
    }

    #[test]
    fn a_caret_l_that_was_there_already_is_not_a_key_taken_as_text() {
        // The line has a literal ^L in it, and the screen is cleared: the count does not rise.
        let before = look_of(0, (21, 2), &["old", "", "READY> echo partial^L"]);
        let after = look_of(3, (21, 0), &["READY> echo partial^L", "", ""]);
        assert_eq!(judge(&before, &after), Answer::Redrawn);
        // And one more arriving beside it is.
        let after = look_of(3, (23, 2), &["old", "", "READY> echo partial^L^L"]);
        assert_eq!(judge(&before, &after), Answer::Text);
    }

    #[test]
    fn a_screen_with_only_the_cursors_row_at_the_top_is_clear_already() {
        assert!(already_clear(&look_of(0, (7, 0), &["READY>", "", ""])));
        assert!(already_clear(&look_of(
            9,
            (12, 0),
            &["READY> echo kept", ""]
        )));
        // Anything below the cursor's row, or the cursor lower down, is not.
        assert!(!already_clear(&look_of(
            0,
            (7, 0),
            &["READY>", "", "output"]
        )));
        assert!(!already_clear(&look_of(0, (0, 1), &["READY>", ""])));
        assert!(!already_clear(&look_of(0, (7, 2), &["old", "", "READY>"])));
        assert!(!already_clear(&look_of(0, (0, 0), &[])));
    }

    #[test]
    fn what_the_reply_says_for_each_end_is_plain_ascii_text() {
        for text in [
            ON_THE_ALTERNATE_SCREEN,
            HISTORY_ONLY,
            SYNCHRONIZED,
            TOOK_IT_AS_TEXT,
            TEXT_STAYED,
            NO_ANSWER,
        ] {
            assert!(text.is_ascii(), "{text}");
            assert!(!text.contains(" - "), "{text}");
        }
    }

    #[test]
    fn each_end_says_what_was_done_and_only_an_answered_clear_has_no_note() {
        assert_eq!(Done::after(Answer::Redrawn, true), Done::redrawn());
        assert_eq!(Done::redrawn().note, None);
        assert!(Done::redrawn().screen);
        for done in [
            Done::untouched(),
            Done::history_only(HISTORY_ONLY),
            Done::history_only(SYNCHRONIZED),
            Done::after(Answer::Text, true),
            Done::after(Answer::TextStayed, true),
            Done::after(Answer::None, true),
        ] {
            assert!(done.note.is_some(), "{done:?}");
            assert!(!done.screen, "{done:?}");
        }
        assert!(!Done::untouched().history);
        assert!(Done::history_only(HISTORY_ONLY).history);
        assert!(Done::after(Answer::Text, true).history);
    }

    #[test]
    fn a_pane_that_went_to_the_alternate_screen_while_the_shell_answered_says_so() {
        let done = Done::after(Answer::Redrawn, false);
        assert!(!done.history);
        assert!(done.screen, "the shell did clear its screen first");
        assert_eq!(done.note, Some(ON_THE_ALTERNATE_SCREEN));
        assert_eq!(
            Done::after(Answer::None, false).note,
            Some(ON_THE_ALTERNATE_SCREEN)
        );
    }
}
