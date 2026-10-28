//! The line `taw agent start` typed an agent's command on, and the rows of the screen below it.
//!
//! The pane still shows what its last occupant left when the next agent is started: an agent that died
//! at a permission prompt leaves the prompt, and one that died mid-turn leaves its turn. The rules of
//! the new agent's kind read that frame as well as they read one the new agent draws, so the frame was
//! read as the new agent's state: `blocked`, with a notification that a person was needed, for an agent
//! that was only starting. Whatever the command started can have drawn below the line it was typed on
//! and nowhere else, so the verb, and the watcher while the line is on the screen, read those rows only.
//!
//! The line moves: each row the screen scrolls by moves it up one, and a clear can take it off. The row
//! it was typed on and the size of the pane's history then say where it should be (the history grows by
//! a row for each row that leaves the top of the screen), and it is looked for from there upwards: a
//! shell that rewrites its prompt rows when a command is entered (a transient prompt) can move the
//! command up, and a history at its limit stops growing as the screen scrolls, or drops a tenth of its
//! rows at once, so the search can start well below where the line is. A row is the line only when it
//! has the line's shape: what the prompt left of the cursor (or a transient prompt put in its place, no
//! wider than it or than 16 columns), the whole command, and after it a right prompt, only when the
//! prompt row showed one before the command was typed ([`TypedLine::is`]). An agent's frame has rows that
//! hold the command's text (a status row naming the model, `claude-opus-4-1`, a tip to run `claude
//! --continue`, a footer that says to `Run: mise upgrade claude`, a box row with the word in it), and a
//! row that only held it was taken for the line, with nothing below it. A line that is not on the screen
//! any more has gone off it, and the screen, all of it, is newer than the command.
//!
//! So the line is recorded only once the shell has echoed the command on it ([`TypedLine::echoed`]):
//! recorded before, a read that came between the record and the echo would find it nowhere, and take
//! a line that was about to be drawn for one that had gone.

/// How many columns a transient prompt can take before the command on the row it rewrote, however few
/// the prompt it replaced took: the last line of a prompt of two lines can be as short as `\u{276f} `, and
/// a prompt that ends with a line break leaves nothing before the cursor, while a transient prompt in
/// their place can show more (`14:32 \u{276f} `, oh-my-posh's `zsh> `). A row of the agent's own with more
/// than the prompt or this before the command is not the line.
const TRANSIENT_PROMPT_ROOM: usize = 16;

/// Where a command was typed: the row of the screen its last row was on, when the pane's history held
/// `history` rows, the column it was typed at, the command, and what the row starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedLine {
    row: u32,
    history: u64,
    /// The column the command was typed at: how wide the prompt before it was. `None` for a line taw
    /// recorded before it recorded the column, whose shape is then not known: anything can be before the
    /// command, and a right prompt after it, as that taw read it ([`TypedLine::shape_unknown`]).
    column: Option<u32>,
    /// The command, whole, as a row reads it (its tabs and line breaks spaces).
    command: String,
    /// What the row starts with ([`start_of`]): the prompt left of where the command was typed and the
    /// command, or the part of a command that wrapped on its last row; `None` when that was not known.
    start: Option<String>,
    /// Whether the row showed something right of the cursor before the command was typed: a right
    /// prompt, which can stay after the command once it is entered, as zsh's does unless
    /// `TRANSIENT_RPROMPT` is set ([`right_of`]).
    right: bool,
}

/// What a screen shows below a [`TypedLine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Below<'a> {
    /// The rows below the line, which is on the screen, as the screen's own text.
    Rows(&'a str),
    /// The line is not on the screen: it has scrolled off the top, or a clear took it away, so the screen
    /// is newer than the command.
    Gone,
}

impl TypedLine {
    /// The line that `typed`, typed at the cursor (`cursor_x`, `cursor_y`) of a pane `width` columns wide
    /// whose history held `history` rows, ends on: the row of its last character, and what that row holds
    /// of the command. A character is taken to be a column wide, as the command line of an agent is.
    pub fn at(cursor_x: u32, cursor_y: u32, width: u32, history: u64, typed: &str) -> TypedLine {
        let (rows_on, _) = last_part(cursor_x, width, typed);
        TypedLine {
            row: cursor_y.saturating_add(u32::try_from(rows_on).unwrap_or(u32::MAX)),
            history,
            column: Some(cursor_x),
            command: folded(typed.trim()),
            start: None,
            right: false,
        }
    }

    /// The same line, told by what its row starts with as well ([`start_of`]).
    pub fn starting_with(self, start: Option<String>) -> TypedLine {
        TypedLine { start, ..self }
    }

    /// The same line, on a row that showed a right prompt before the command was typed, or not
    /// ([`right_of`]).
    pub fn with_a_right_prompt(self, right: bool) -> TypedLine {
        TypedLine { right, ..self }
    }

    /// The line `typed`, typed at column `start_x` of a pane `width` columns wide, has been echoed on,
    /// once the shell has echoed all of it: the cursor (`cursor_x`, `cursor_y`) is where the command
    /// ends, and the row it ends on holds what [`TypedLine::at`] looks for. `history` is the size of the
    /// pane's history and `screen` the screen, read with the cursor. `None` while the shell has not.
    ///
    /// Where the cursor is says it, and not where the line was expected to go: a command typed on the
    /// last row that wraps scrolls the screen as it is echoed. A command that ends at the right edge
    /// leaves the cursor at the start of the next row (bash's line editor moves it there itself), or at
    /// the edge with the wrap still to come.
    pub fn echoed(
        start_x: u32,
        width: u32,
        typed: &str,
        cursor: (u32, u32),
        history: u64,
        screen: &str,
    ) -> Option<TypedLine> {
        let width = width.max(1) as usize;
        let end = start_x as usize + typed.trim().chars().count();
        let (cursor_x, cursor_y) = (cursor.0 as usize, cursor.1 as usize);
        let row = if !end.is_multiple_of(width) {
            (cursor_x == end % width).then_some(cursor_y)?
        } else if cursor_x == 0 {
            cursor_y.checked_sub(1)?
        } else if cursor_x + 1 >= width {
            cursor_y
        } else {
            return None;
        };
        let (_, last) = last_part(start_x, u32::try_from(width).unwrap_or(u32::MAX), typed);
        if !folded(screen.lines().nth(row)?).contains(last.as_str()) {
            return None;
        }
        Some(TypedLine {
            row: u32::try_from(row).ok()?,
            history,
            column: Some(start_x),
            command: folded(typed.trim()),
            start: None,
            right: false,
        })
    }

    /// What `@agent_typed_line` holding `value` says: `<row> <history> <column> <length> <command>`, with the
    /// word `right` before `<length>` when the row showed a right prompt, and ` <start>` after it when what
    /// the row starts with is known, where `<length>` is how many characters the command has. A value an
    /// earlier taw wrote, with no `<column>`, is read as it was written, with the column not known. `None`
    /// for anything else.
    pub fn parse(value: &str) -> Option<TypedLine> {
        let (row, rest) = value.split_once(' ')?;
        let (history, rest) = rest.split_once(' ')?;
        let line = |column: Option<u32>, rest: &str| {
            let (word, rest) = rest.split_once(' ')?;
            let (right, (length, rest)) = match word {
                "right" => (true, rest.split_once(' ')?),
                length => (false, (length, rest)),
            };
            let length: usize = length.parse().ok()?;
            let cut = rest
                .char_indices()
                .nth(length)
                .map_or(rest.len(), |(at, _)| at);
            let (command, start) = rest.split_at(cut);
            if length == 0 || command.chars().count() != length || command.trim().is_empty() {
                return None;
            }
            let start = match start {
                "" => None,
                start => match start.strip_prefix(' ')? {
                    "" => return None,
                    start => Some(start.to_string()),
                },
            };
            Some(TypedLine {
                row: row.parse().ok()?,
                history: history.parse().ok()?,
                column,
                command: command.to_string(),
                start,
                right,
            })
        };
        rest.split_once(' ')
            .and_then(|(column, rest)| line(Some(column.parse().ok()?), rest))
            .or_else(|| line(None, rest))
    }

    /// The value `@agent_typed_line` is written with ([`TypedLine::parse`]).
    pub fn value(&self) -> String {
        let mut value = format!("{} {} ", self.row, self.history);
        if let Some(column) = self.column {
            value.push_str(&format!("{column} "));
        }
        if self.right {
            value.push_str("right ");
        }
        value.push_str(&format!(
            "{} {}",
            self.command.chars().count(),
            self.command
        ));
        if let Some(start) = &self.start {
            value.push(' ');
            value.push_str(start);
        }
        value
    }

    /// Whether `row`, a row of the screen, has the line's shape: what the prompt left of the cursor, or a
    /// transient prompt put in its place ([`TRANSIENT_PROMPT_ROOM`]), then the whole command, then a right
    /// prompt only when the prompt row showed one before the command was typed. So it is the line when:
    ///
    /// * it reads as the line's row did ([`start_of`]): the prompt and the command, the command alone at
    ///   the left edge (a prompt that ends with a line break leaves the cursor there), or the last row's
    ///   part of a command that wrapped;
    /// * it starts with the prompt and the command and goes on past a space, where the prompt row showed a
    ///   right prompt, which is what follows (however far it is, a clock in it that has moved on);
    /// * or it holds the whole command as a command line does ([`TypedLine::holds_the_command`]): a
    ///   transient prompt rewrote what came before it, kept a right prompt after it, as zsh does, or took a
    ///   command that had wrapped back to one row.
    ///
    /// A row of the agent's own that holds the command's text is not it: a status row naming the model
    /// (`claude-opus-4-1`), a tip to run `claude --continue`, one that ends `run claude` or a footer that
    /// ends `Run: mise upgrade claude` (more before the command than a prompt), a box row `\u{2502} claude
    /// ... \u{2502}`, a status row `claude  \u{b7}  opus 4.1` (more after it, with no right prompt), or a
    /// menu row `\u{276f} claude-code-guide` after a prompt `\u{276f} ` (the command's text, and more of a
    /// word).
    fn is(&self, row: &str) -> bool {
        let row = folded(row);
        let row = row.trim_end();
        let command = self.command.as_str();
        match self.start.as_deref() {
            Some(start) if row == start => true,
            Some(start)
                if (self.right || self.shape_unknown())
                    && start.len() > command.len()
                    && start.ends_with(command)
                    && row
                        .strip_prefix(start)
                        .is_some_and(|after| after.starts_with(' ')) =>
            {
                true
            }
            _ => self.holds_the_command(row),
        }
    }

    /// Whether `row`, its end's spaces trimmed, holds the command as a command line does: after the row's
    /// start or a space, with no more before it than the prompt it was typed after took, or than
    /// [`TRANSIENT_PROMPT_ROOM`] columns when that took fewer, and with nothing after it but, past a run of
    /// two or more spaces, a right prompt, where the prompt row showed one. Text after a single space is more of another command (a tip to run
    /// `claude --continue`), and text joined to it is another word (`claude-opus-4-1`). For a line whose
    /// shape was not recorded ([`TypedLine::shape_unknown`]), whatever is before the command is taken for
    /// a prompt, and a right prompt can follow it.
    fn holds_the_command(&self, row: &str) -> bool {
        let command = self.command.as_str();
        let room = self
            .column
            .map(|column| (column as usize).max(TRANSIENT_PROMPT_ROOM));
        let right = self.right || self.shape_unknown();
        row.match_indices(command).any(|(at, _)| {
            let (before, after) = (&row[..at], &row[at + command.len()..]);
            (before.is_empty() || before.ends_with(' '))
                && room.is_none_or(|room| unicode_width::UnicodeWidthStr::width(before) <= room)
                && (after.is_empty() || (right && after.starts_with("  ")))
        })
    }

    /// Whether the line was recorded by a taw that did not record its shape (the column it was typed at,
    /// and whether a right prompt was beyond it): what is before the command on its row is then taken
    /// for a prompt however long it is, and what is after it for a right prompt, as that taw took them.
    fn shape_unknown(&self) -> bool {
        self.column.is_none()
    }

    /// The rows of `screen` below the line, when the pane's history holds `history` rows and `alternate`
    /// says whether the pane is on its alternate screen, which holds nothing of what the main screen
    /// showed, so all of it counts.
    pub fn below<'a>(&self, screen: &'a str, history: u64, alternate: bool) -> Below<'a> {
        if alternate {
            return Below::Rows(screen);
        }
        // The rows that have left the top of the screen since the command was typed: at least that many
        // when the history has only grown, since a history at its limit stops growing; and a history
        // that shrank (it dropped its oldest rows at its limit, or was cleared) says nothing of it.
        let expected = match history.checked_sub(self.history) {
            Some(scrolled) => i64::from(self.row) - i64::try_from(scrolled).unwrap_or(i64::MAX),
            None => i64::from(self.row),
        };
        let Ok(expected) = usize::try_from(expected) else {
            return Below::Gone;
        };
        let mut rows = Vec::new();
        let mut start = 0;
        for row in screen.split_inclusive('\n') {
            rows.push((start, row));
            start += row.len();
        }
        let Some(last) = rows.len().checked_sub(1) else {
            return Below::Gone;
        };
        for index in (0..=expected.min(last)).rev() {
            let (at, row) = rows[index];
            if self.is(row) {
                return Below::Rows(&screen[at + row.len()..]);
            }
        }
        Below::Gone
    }

    /// Whether `screen` shows anything below the line ([`TypedLine::below`]): a row there with something on
    /// it, or, when the line is not on the screen, anything at all.
    pub fn drawn_below(&self, screen: &str, history: u64, alternate: bool) -> bool {
        let shown = match self.below(screen, history, alternate) {
            Below::Rows(rows) => rows,
            Below::Gone => screen,
        };
        shown.lines().any(|row| !row.trim().is_empty())
    }
}

/// What a read of a pane that records a [`TypedLine`] makes of its screen ([`Misses::read`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read<'a> {
    /// The line is on the screen: the rows below it, as [`Below::Rows`].
    Rows(&'a str),
    /// The line is not on the screen, and the read before this one found it, or did not look: this read
    /// reads nothing of the screen.
    Nothing,
    /// The line is not on the screen for the second read in a row: it has gone, and the screen, all of
    /// it, is newer than the command.
    Gone,
}

/// The line the last read of a pane looked for and did not find. One read can miss a line that is there
/// to be drawn: one recorded before the shell has echoed the command, as `taw agent start` once recorded
/// it, or caught while the shell redraws it. Taken for gone at once, the line was taken off the pane and
/// the whole screen read, the frame the last agent left with it. So a read that misses it reads nothing of
/// the screen, the next read is owed at once ([`Misses::owe_a_look`]), and only a second read in a row
/// that misses it takes it for gone: with nothing printed in between, that read sees the same screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Misses(Option<TypedLine>);

impl Misses {
    /// What a read of `screen`, in a pane that records `line`, makes of it ([`TypedLine::below`] for
    /// `history` and `alternate`), counting this read in.
    pub fn read<'a>(
        &mut self,
        line: &TypedLine,
        screen: &'a str,
        history: u64,
        alternate: bool,
    ) -> Read<'a> {
        match line.below(screen, history, alternate) {
            Below::Rows(rows) => {
                self.0 = None;
                Read::Rows(rows)
            }
            Below::Gone if self.0.as_ref() == Some(line) => {
                self.0 = None;
                Read::Gone
            }
            Below::Gone => {
                self.0 = Some(line.clone());
                Read::Nothing
            }
        }
    }

    /// A read that did not look for the line (it captured no screen, or the pane records none): the next
    /// that misses it is the first in a row.
    pub fn looked_away(&mut self) {
        self.0 = None;
    }

    /// Whether the last read missed the line, so that the next is owed at once rather than when the pane
    /// next prints, which a pane whose agent has drawn and gone quiet may not do for a while.
    pub fn owe_a_look(&self) -> bool {
        self.0.is_some()
    }
}

/// What the row the command `typed` ends on starts with, `typed` being typed at the cursor (`cursor_x`,
/// `cursor_y`) of a pane `width` columns wide that showed `screen` just before: for a command that fits
/// on the row it is typed on, the prompt as drawn left of the cursor, with the command after it; for one
/// that wraps, the part of it on its last row, which starts that row. Whatever the shell draws after the
/// command (a suggestion after the cursor, a right prompt) is no part of it, nor what it changes to when
/// the command is entered (a suggestion goes, a clock moves on). The prompt is cut at `cursor_x` columns,
/// measured as tmux draws it ([`fitting`]), and made up to them with the spaces a capture leaves out at
/// the end of a row. A row `screen` does not have is empty: the last row of a screen, empty, has no line
/// of its own in a capture read with its last line break taken off, which is where a prompt that ends
/// with a line break leaves the cursor in a pane full to its last row.
pub fn start_of(screen: &str, cursor_x: u32, cursor_y: u32, width: u32, typed: &str) -> String {
    let (rows_on, last) = last_part(cursor_x, width, typed);
    if rows_on > 0 {
        return last;
    }
    let row = screen
        .lines()
        .nth(cursor_y as usize)
        .map(folded)
        .unwrap_or_default();
    let (prompt, columns) = fitting(&row, cursor_x as usize);
    let mut start = prompt.to_string();
    start.extend(std::iter::repeat_n(
        ' ',
        (cursor_x as usize).saturating_sub(columns),
    ));
    start.push_str(&last);
    start
}

/// The longest start of `row` that takes no more than `columns` columns, and how many it takes, measured
/// as the string it is, as tmux draws it: a sequence tmux draws as one cell two columns wide (a symbol
/// with the variation selector that asks for its emoji form, a keycap, a sequence joined with a zero
/// width joiner, an emoji with a skin tone) counts as that cell, where its characters one at a time come
/// to one column, or to four.
fn fitting(row: &str, columns: usize) -> (&str, usize) {
    let mut fits = (0, 0);
    for (at, c) in row.char_indices() {
        let end = at + c.len_utf8();
        let width = unicode_width::UnicodeWidthStr::width(&row[..end]);
        if width > columns {
            break;
        }
        fits = (end, width);
    }
    (&row[..fits.0], fits.1)
}

/// Whether row `cursor_y` of `screen` shows something right of the cursor at column `cursor_x`: a right
/// prompt, on a screen read before anything is typed at the cursor.
pub fn right_of(screen: &str, cursor_x: u32, cursor_y: u32) -> bool {
    screen.lines().nth(cursor_y as usize).is_some_and(|row| {
        let row = folded(row);
        let (prompt, _) = fitting(&row, cursor_x as usize);
        !row[prompt.len()..].trim().is_empty()
    })
}

/// How many rows below the one it was typed on `typed`, typed at column `start` of a pane `width` columns
/// wide, ends, and what that row holds of it: the whole command when it fits on the row it was typed on,
/// else the part of it on its last row.
fn last_part(start: u32, width: u32, typed: &str) -> (usize, String) {
    let typed = typed.trim();
    let chars: Vec<char> = typed.chars().collect();
    let width = width.max(1) as usize;
    let start = start as usize;
    let rows_on = (start + chars.len()).saturating_sub(1) / width;
    let first_on_last_row = (rows_on * width).saturating_sub(start);
    let tail: String = chars[first_on_last_row.min(chars.len())..].iter().collect();
    let last = match tail.trim() {
        "" => typed.to_string(),
        tail => tail.to_string(),
    };
    (rows_on, folded(&last))
}

/// `text` with every tab, carriage return and line feed in it a space, as the watcher reads a pane option
/// (one line, each column between tabs), so that what is compared is the same text whoever read it.
fn folded(text: &str) -> String {
    text.replace(['\t', '\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane `agent start` typed `claude` into: the last agent's frame, and a prompt on the fourth row
    /// of six, with nothing in the history.
    const LEFT_BEHIND: &str = "esc to interrupt\n? for shortcuts\n\nsh-3.2$ claude\n\n\n";

    fn typed_on_the_fourth_row() -> TypedLine {
        TypedLine::at(8, 3, 40, 0, "claude")
    }

    #[test]
    fn a_command_that_fits_its_row_ends_on_it_and_is_looked_for_whole() {
        let line = typed_on_the_fourth_row();
        assert_eq!(line.value(), "3 0 8 6 claude");
        assert_eq!(TypedLine::parse(&line.value()), Some(line));
        assert_eq!(
            TypedLine::at(8, 3, 40, 12, "  claude --resume abc ").value(),
            "3 12 8 19 claude --resume abc"
        );
        // Exactly to the last column is still one row.
        assert_eq!(
            TypedLine::at(34, 3, 40, 0, "claude").value(),
            "3 0 34 6 claude"
        );
    }

    #[test]
    fn a_command_that_wraps_ends_on_a_later_row_and_is_kept_whole() {
        // From column 30 of a 40-column pane: ten characters on the row it was typed on, the rest below.
        let line = TypedLine::at(30, 3, 40, 0, "claude --model the-longest-model-name");
        assert_eq!(
            line.value(),
            "4 0 30 37 claude --model the-longest-model-name"
        );
        // Two rows below.
        let command = format!("claude {}", "x".repeat(60));
        let line = TypedLine::at(30, 3, 40, 0, &command);
        assert_eq!(line.value(), format!("5 0 30 67 {command}"));
    }

    #[test]
    fn a_value_that_is_not_a_row_a_history_and_a_command_is_no_line() {
        for value in [
            "",
            "3",
            "3 0",
            "3 0 8",
            "3 0 8 6",
            "3 0 8 6 ",
            "3 0 8 claude",
            "3 0 9 0 claude",
            "3 0 9 7 claude",
            "3 0 8 6 claudesh-3.2$ claude",
            "3 0 8 6 claude ",
            "x 0 8 6 claude",
            "3 y 8 6 claude",
            "3 0 z 6 claude",
            "-1 0 8 6 claude",
            "3 0 8 right claude",
            "3 0 8 left 6 claude",
        ] {
            assert_eq!(TypedLine::parse(value), None, "{value:?}");
        }
        // The command keeps its spaces, and so does what the row starts with.
        let line =
            TypedLine::parse("3 7 9 19 claude --resume abc  ~/work > claude --resume abc").unwrap();
        assert_eq!(line.command, "claude --resume abc");
        assert_eq!(line.column, Some(9));
        assert_eq!(line.start.as_deref(), Some(" ~/work > claude --resume abc"));
        assert!(!line.right);
        assert_eq!(TypedLine::parse(&line.value()), Some(line));
        // A row that showed a right prompt says so before the length.
        let line = TypedLine::at(9, 1, 50, 0, "claude")
            .starting_with(Some("~/work > claude".to_string()))
            .with_a_right_prompt(true);
        assert_eq!(line.value(), "1 0 9 right 6 claude ~/work > claude");
        assert_eq!(TypedLine::parse(&line.value()), Some(line));
        // A command that wrapped is kept whole, and its row starts with its last row's part.
        let line = TypedLine::at(30, 3, 40, 0, "claude --model the-longest-model-name")
            .starting_with(Some("odel the-longest-model-name".to_string()));
        assert_eq!(
            line.value(),
            "4 0 30 37 claude --model the-longest-model-name odel the-longest-model-name"
        );
        assert_eq!(TypedLine::parse(&line.value()), Some(line));
    }

    /// A value an earlier taw wrote has no column, and says nothing of a right prompt unless it has the
    /// word: it is read as that taw read it, with whatever is before the command taken for a prompt, and a
    /// right prompt after it.
    #[test]
    fn a_value_an_earlier_taw_wrote_is_read_as_it_was_written() {
        let screen = "x\n~/work > claude --resume abc            10:32\nWelcome\n";
        for value in [
            "1 0 19 claude --resume abc",
            "1 0 19 claude --resume abc ~/work > claude --resume abc            10:32",
            "1 0 19 claude --resume abc ~/work > claude --resume abc",
            "1 0 right 19 claude --resume abc ~/work > claude --resume abc",
        ] {
            let line = TypedLine::parse(value).unwrap();
            assert_eq!(line.column, None, "{value:?}");
            assert_eq!(line.value(), value);
            assert_eq!(
                line.below(screen, 0, false),
                Below::Rows("Welcome\n"),
                "{value:?}"
            );
        }
        // However long the prompt a transient one put in place of the first was.
        let line = TypedLine::parse("1 0 6 claude").unwrap();
        assert_eq!(
            line.below("x\n~/work/project \u{276f} claude\nWelcome\n", 0, false),
            Below::Rows("Welcome\n")
        );
    }

    #[test]
    fn a_row_starts_with_the_prompt_as_drawn_left_of_the_cursor_and_the_command() {
        // A capture keeps no spaces at the end of a row: the prompt's own is made up to the cursor.
        let screen = "x\ny\nsh-3.2$\n\n";
        assert_eq!(start_of(screen, 8, 2, 40, "claude"), "sh-3.2$ claude");
        // A right prompt is right of the cursor, and no part of it.
        let screen = "x\n~/work >                    10:32\n\n";
        assert_eq!(
            start_of(screen, 9, 1, 40, "  claude --resume abc "),
            "~/work > claude --resume abc"
        );
        // The cursor counts columns, and a character drawn two columns wide takes two.
        let screen = "\u{1f680} ~/w \u{276f}\n\n";
        assert_eq!(
            start_of(screen, 9, 0, 40, "claude"),
            "\u{1f680} ~/w \u{276f} claude"
        );
        // A command that wraps starts its last row with what it has on it.
        assert_eq!(
            start_of(screen, 30, 0, 40, "claude --model the-longest-model-name"),
            "odel the-longest-model-name"
        );
        // A row the screen does not have is empty: the prompt is the cursor's columns.
        assert_eq!(start_of(screen, 8, 5, 40, "claude"), "        claude");
        assert_eq!(start_of("x\n~/work\n", 0, 2, 40, "claude"), "claude");
    }

    /// tmux draws some sequences of characters as one cell two columns wide: a symbol with the variation
    /// selector that asks for its emoji form (U+FE0F), a keycap, a sequence joined with U+200D, and an emoji
    /// with a skin tone. Measured a character at a time, the first two came to one column and the others to
    /// four, and the prompt was cut a column off where the cursor was, so that with a right prompt beyond it
    /// the row the command was entered on neither started as the line did nor ended with the command. The
    /// prompt is measured as the string it is, as tmux draws it.
    #[test]
    fn a_prompt_is_cut_where_tmux_draws_its_cells_whatever_sequence_makes_one() {
        for (symbol, name) in [
            ("\u{2601}\u{fe0f}", "a symbol and U+FE0F"),
            ("\u{2764}\u{fe0f}", "a heart and U+FE0F"),
            ("1\u{fe0f}\u{20e3}", "a keycap"),
            ("\u{1f468}\u{200d}\u{1f4bb}", "a ZWJ sequence"),
            ("\u{1f44d}\u{1f3fd}", "a skin tone"),
        ] {
            // Two columns of symbol and ten of ` ~/work > `, a right prompt beyond the cursor at twelve.
            let screen = format!("x\n{symbol} ~/work >                10:32\n\n");
            assert_eq!(
                start_of(&screen, 12, 1, 40, "claude"),
                format!("{symbol} ~/work > claude"),
                "{name}"
            );
        }
        // What was measured right by the character stays so: a wide emoji, a flag of two regional
        // indicators, CJK, a Nerd Font glyph (one column, from the private use area) and a combining mark.
        for (prompt, cursor_x) in [
            ("\u{1f680} ~ ", 5),
            ("\u{1f1fa}\u{1f1f8} ~ ", 5),
            ("\u{65e5}\u{672c} ~ ", 7),
            ("\u{e0a0} main ", 7),
            ("caf\u{e9}e\u{301} > ", 8),
        ] {
            let screen = format!("{prompt}       10:32\n\n");
            assert_eq!(
                start_of(&screen, cursor_x, 0, 40, "claude"),
                format!("{prompt}claude"),
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn a_row_is_the_line_when_it_starts_as_the_line_did_or_holds_the_command_as_a_command_line() {
        // zsh, with a suggestion after the cursor when the command was echoed and a right prompt: on
        // Enter the suggestion goes and the clock moves on, so the row neither reads as it was echoed nor
        // ends with the command. It starts as the line did.
        let before = "esc to interrupt\n~/work >                         10:32\n\n\n";
        let start = start_of(before, 9, 1, 50, "claude");
        let line = TypedLine::at(9, 1, 50, 0, "claude")
            .starting_with(Some(start))
            .with_a_right_prompt(right_of(before, 9, 1));
        let entered = "esc to interrupt\n~/work > claude                  10:33\n\n\n";
        assert_eq!(line.below(entered, 0, false), Below::Rows("\n\n"));
        // A right prompt one space after the command, in a pane too narrow for more: it starts as the line
        // did.
        let narrow = "esc to interrupt\n~/work > claude 10:33\n\n\n";
        assert_eq!(line.below(narrow, 0, false), Below::Rows("\n\n"));
        // A transient prompt rewrote the row, prompt and right prompt and all, and moved it up: it ends
        // with the command.
        let rewritten = "\u{276f} claude\n\n\n\n";
        assert_eq!(line.below(rewritten, 0, false), Below::Rows("\n\n\n"));
    }

    /// zsh's transient prompt rewrites the row the command is entered on, and keeps a right prompt on it
    /// unless `TRANSIENT_RPROMPT` is set: the row neither starts as the line did nor ends with the command,
    /// and holds it before a run of spaces and the right prompt, whatever that holds (two spaces inside it,
    /// a clock that moved on). p10k's drops the right prompt, and un-wraps a command that wrapped, whose
    /// line was told by its last row's part: the row holds the whole command at its end.
    #[test]
    fn a_row_a_transient_prompt_rewrote_holds_the_whole_command() {
        let before = "esc to interrupt\n~/work on main \u{276f}                 10:32\n\n\n";
        let line = TypedLine::at(17, 1, 50, 0, "claude")
            .starting_with(Some(start_of(before, 17, 1, 50, "claude")))
            .with_a_right_prompt(right_of(before, 17, 1));
        for rewritten in [
            "esc to interrupt\n\u{276f} claude                          10:32\n\n\n",
            "esc to interrupt\n\u{276f} claude                 main 10:33\n\n\n",
            "esc to interrupt\n\u{276f} claude               main  10:32\n\n\n",
            "esc to interrupt\n\u{276f} claude\n\n\n",
        ] {
            assert_eq!(
                line.below(rewritten, 0, false),
                Below::Rows("\n\n"),
                "{rewritten:?}"
            );
        }
        // Typed at column 41 of a 50-column pane, the command wrapped: its line is on the row below, told
        // by its last row's part, and on Enter the short prompt took it back to one row, a row higher.
        let command = "claude --resume 3f0c2a9e-7d41";
        let before = format!("x\n{}\n\n\n", "p".repeat(41));
        let line = TypedLine::at(41, 1, 50, 0, command)
            .starting_with(Some(start_of(&before, 41, 1, 50, command)))
            .with_a_right_prompt(right_of(&before, 41, 1));
        assert_eq!(line.start.as_deref(), Some("resume 3f0c2a9e-7d41"));
        let unwrapped = format!("x\n\u{276f} {command}\nWelcome\n\n");
        assert_eq!(line.below(&unwrapped, 0, false), Below::Rows("Welcome\n\n"));
        // Not rewritten, it reads as its last row did.
        let wrapped = format!("x\n{}claude --\nresume 3f0c2a9e-7d41\n\n", "p".repeat(41));
        assert_eq!(line.below(&wrapped, 0, false), Below::Rows("\n"));
    }

    /// A prompt that ends with a line break leaves the cursor at the left edge, and the line starts with the
    /// command: a row that only starts with its text (the agent's status row naming its model, flush left)
    /// is not the line, nor a row with more text after it; a row that holds it as a command line does
    /// (alone, or after the prompt a transient prompt rewrote the row to, of no more than 16 columns)
    /// is, and so is one with a right prompt after it, where the prompt row showed one.
    #[test]
    fn a_line_typed_at_the_left_edge_is_a_row_that_holds_the_command_as_a_command_line() {
        let before = "x\n~/work\n";
        let line = TypedLine::at(0, 2, 40, 0, "claude")
            .starting_with(Some(start_of(before, 0, 2, 40, "claude")))
            .with_a_right_prompt(right_of(before, 0, 2));
        for (screen, below) in [
            ("x\n~/work\nclaude\nWelcome\n", Below::Rows("Welcome\n")),
            ("x\n~/work\nclaude   \n\n", Below::Rows("\n")),
            ("x\n\u{276f} claude\n\n\n", Below::Rows("\n\n")),
            ("x\n$ \u{276f} claude\n\n\n", Below::Rows("\n\n")),
            ("x\n~/work \u{276f} claude\n\n\n", Below::Rows("\n\n")),
            ("x\n14:32 \u{276f} claude\n\n\n", Below::Rows("\n\n")),
            ("x\n~/work on main \u{276f} claude\n\n\n", Below::Gone),
            ("x\n~/work\nclaude                10:32\n\n", Below::Gone),
            ("x\n~/work\nclaude-opus-4-1 . ctx 41%\n\n", Below::Gone),
            ("x\n~/work\nclaude-opus-4-1  ctx 41%\n\n", Below::Gone),
            ("x\n~/work\nclaude --continue\n\n", Below::Gone),
            ("x\n~/work\nnotclaude\n\n", Below::Gone),
        ] {
            assert_eq!(line.below(screen, 0, false), below, "{screen:?}");
        }
        // The cursor's row showed a right prompt before the command was typed.
        let right = line.with_a_right_prompt(true);
        assert_eq!(
            right.below("x\n~/work\nclaude                10:33\n\n", 0, false),
            Below::Rows("\n")
        );
        assert_eq!(
            right.below("x\n\u{276f} claude          10:33\n\n\n", 0, false),
            Below::Rows("\n\n")
        );
        assert!(right_of("x\n~/work\n                    10:32\n", 0, 2));
    }

    /// What comes before the command on a row takes no more columns than the prompt it was typed after, or
    /// than 16 (a transient prompt in its place): Claude Code's own rows that end with the command, the
    /// footer that says an update is ready and a tip, have more, and are not the line.
    #[test]
    fn a_row_of_the_agents_own_that_ends_with_the_command_is_not_the_line() {
        let before = "x\nsh-3.2$\n";
        let line = TypedLine::at(8, 1, 80, 0, "claude")
            .starting_with(Some(start_of(before, 8, 1, 80, "claude")));
        for row in [
            "  ? for shortcuts                    Update available! Run: mise upgrade claude",
            "  \u{23bf}  Tip: to resume later, run claude",
            "  ? for shortcuts                    Update available! Run: brew upgrade claude-code",
            "  \u{23bf}  Tip: run claude --continue to resume",
        ] {
            assert_eq!(
                line.below(&format!("output\n{row}\n"), 0, false),
                Below::Gone,
                "{row:?}"
            );
        }
        // The shell's own rows are: its prompt and the command, and one in its place that takes no more than
        // 16 columns.
        for row in [
            "sh-3.2$ claude",
            "$ claude",
            "\u{276f} claude",
            "claude",
            "~/wrk \u{276f} claude",
            "sh-3.2$$ claude",
            "~/src on main \u{276f} claude",
        ] {
            assert_eq!(
                line.below(&format!("output\n{row}\nWelcome\n"), 0, false),
                Below::Rows("Welcome\n"),
                "{row:?}"
            );
        }
        // A column more is not.
        assert_eq!(
            line.below("output\n~/work on main \u{276f} claude\n", 0, false),
            Below::Gone
        );
        // A prompt wider than 16 columns gives the room: as wide as it, and no wider.
        let before = "x\n~/work/project on main \u{276f}\n";
        let wide = TypedLine::at(25, 1, 80, 0, "claude")
            .starting_with(Some(start_of(before, 25, 1, 80, "claude")));
        assert_eq!(
            wide.below(
                "output\n[14:32] ~/work/project \u{276f} claude\nWelcome\n",
                0,
                false
            ),
            Below::Rows("Welcome\n")
        );
        assert_eq!(
            wide.below(
                "output\n[14:32] ~/work/projects \u{276f} claude\n",
                0,
                false
            ),
            Below::Gone
        );
    }

    /// What comes after the command on a row, past two spaces or more, is a right prompt only where the
    /// prompt row showed one: Claude Code's own rows that hold the command and pad it, a box row and a status
    /// row, are not the line. With a right prompt, the rows that end with the command still are not.
    #[test]
    fn a_row_of_the_agents_own_that_pads_the_command_is_not_the_line_with_no_right_prompt() {
        let before = "x\nsh-3.2$\n";
        let line = TypedLine::at(8, 1, 80, 0, "claude")
            .starting_with(Some(start_of(before, 8, 1, 80, "claude")))
            .with_a_right_prompt(right_of(before, 8, 1));
        for row in [
            "\u{2502} claude                                         \u{2502}",
            "claude  \u{b7}  opus 4.1  \u{b7}  ctx 41%",
        ] {
            assert_eq!(
                line.below(&format!("output\n{row}\n"), 0, false),
                Below::Gone,
                "{row:?}"
            );
        }
        let right = line.with_a_right_prompt(true);
        for row in [
            "  ? for shortcuts                    Update available! Run: mise upgrade claude",
            "  \u{23bf}  Tip: to resume later, run claude",
        ] {
            assert_eq!(
                right.below(&format!("output\n{row}\n"), 0, false),
                Below::Gone,
                "{row:?}"
            );
        }
    }

    /// A row that starts as the line did and goes on is the line only where the prompt row showed a right
    /// prompt, and only past a space, which parts a right prompt from the command: a row of the agent's own
    /// that starts as a command line at a prompt like the shell's is not, a menu row naming an agent after
    /// its `\u{276f} `, or the command with more of it after it.
    #[test]
    fn a_row_that_starts_as_the_line_did_and_goes_on_is_the_line_only_before_a_right_prompt() {
        let before = "x\n\u{276f}\n";
        let line = TypedLine::at(2, 1, 80, 0, "claude")
            .starting_with(Some(start_of(before, 2, 1, 80, "claude")))
            .with_a_right_prompt(right_of(before, 2, 1));
        assert_eq!(line.start.as_deref(), Some("\u{276f} claude"));
        assert!(!line.right);
        let right = line.clone().with_a_right_prompt(true);
        let menu = "output\n\u{276f} claude-code-guide \u{b7} sonnet\n";
        assert_eq!(line.below(menu, 0, false), Below::Gone);
        assert_eq!(right.below(menu, 0, false), Below::Gone);
        let more = "output\n\u{276f} claude --continue\n";
        assert_eq!(line.below(more, 0, false), Below::Gone);
        // With a right prompt, what follows the command past a space is it, in a pane too narrow for more.
        assert_eq!(
            right.below("output\n\u{276f} claude 10:33\nWelcome\n", 0, false),
            Below::Rows("Welcome\n")
        );
    }

    #[test]
    fn a_row_that_only_holds_the_commands_text_is_not_the_line() {
        // The history was full, so the search starts at the row the command was typed on, the last one:
        // the line scrolled off long ago, and the agent's frame names its model in the rows left.
        let screen =
            "output 1\n  claude-opus-4-1 . ~/work\nesc to interrupt\n  claude-opus-4-1 . ctx 41%\n";
        let before = "x\ny\nz\nsh-3.2$\n";
        let line = TypedLine::at(8, 3, 40, 50, "claude")
            .starting_with(Some(start_of(before, 8, 3, 40, "claude")));
        assert_eq!(line.below(screen, 50, false), Below::Gone);
        assert!(line.drawn_below(screen, 50, false));
        // Nor by a line not told by what it starts with.
        let computed = TypedLine::at(8, 3, 40, 50, "claude");
        assert_eq!(computed.below(screen, 50, false), Below::Gone);
        // Nor by the same rows flush left, nor a tip with text after the command.
        let flush = "output 1\nclaude-opus-4-1 . ~/work\nTip: run claude --continue\nclaude-opus-4-1 . ctx 41%\n";
        assert_eq!(line.below(flush, 50, false), Below::Gone);
        // Nor with a right prompt on the prompt row.
        let right = line.with_a_right_prompt(true);
        assert_eq!(right.below(screen, 50, false), Below::Gone);
        assert_eq!(right.below(flush, 50, false), Below::Gone);
    }

    #[test]
    fn nothing_drawn_is_nothing_below_the_line_whatever_is_above_it() {
        let line = typed_on_the_fourth_row();
        assert_eq!(line.below(LEFT_BEHIND, 0, false), Below::Rows("\n\n"));
        assert!(!line.drawn_below(LEFT_BEHIND, 0, false));
        // Drawn below it.
        let drawn = "esc to interrupt\n? for shortcuts\n\nsh-3.2$ claude\nInstalling 1/3\n\n";
        assert_eq!(
            line.below(drawn, 0, false),
            Below::Rows("Installing 1/3\n\n")
        );
        assert!(line.drawn_below(drawn, 0, false));
    }

    #[test]
    fn a_transient_prompt_that_moves_the_command_up_has_drawn_nothing() {
        // The shell drew a two-row prompt, and on Enter it rewrote both rows as one with the command on it.
        let before = "esc to interrupt\n? for shortcuts\n~/work on main\n\u{276f} claude\n\n\n";
        let line = TypedLine::at(2, 3, 40, 0, "claude");
        assert!(!line.drawn_below(before, 0, false));
        let rewritten = "esc to interrupt\n? for shortcuts\n\u{276f} claude\n\n\n\n";
        assert_eq!(line.below(rewritten, 0, false), Below::Rows("\n\n\n"));
        assert!(!line.drawn_below(rewritten, 0, false));
        // What the command prints next is below it, on the row the command had been typed on.
        let drawn = "esc to interrupt\n? for shortcuts\n\u{276f} claude\nloading\n\n\n";
        assert!(line.drawn_below(drawn, 0, false));
    }

    #[test]
    fn a_line_the_screen_scrolled_is_found_where_the_history_says_it_went() {
        // The prompt was on the last row: Enter and three rows of output scrolled the screen four rows.
        let line = TypedLine::at(8, 5, 40, 10, "claude");
        let screen = "\nsh-3.2$ claude\n1\n2\n3\n\n";
        assert_eq!(line.below(screen, 14, false), Below::Rows("1\n2\n3\n\n"));
        // The old frame above it has the command too, from the last start: the line nearest below is it.
        let screen = "sh-3.2$ claude\nold frame\nsh-3.2$ claude\n1\n2\n\n";
        assert_eq!(line.below(screen, 13, false), Below::Rows("1\n2\n\n"));
        // What the agent drew below the line has the command in it too (Claude's tip to run `claude
        // --continue`): the history says where the line is, and every row below it is the agent's.
        let screen = "a\nb\nsh-3.2$ claude\nWelcome\nTip: run claude --continue\n> \n";
        assert_eq!(
            line.below(screen, 13, false),
            Below::Rows("Welcome\nTip: run claude --continue\n> \n")
        );
    }

    #[test]
    fn a_line_in_a_history_at_its_limit_is_looked_for_upwards() {
        // The history did not grow (it was full) though the screen scrolled two rows.
        let line = TypedLine::at(8, 5, 40, 2000, "claude");
        let screen = "a\nb\nsh-3.2$ claude\nout 1\nout 2\n\n";
        assert_eq!(
            line.below(screen, 2000, false),
            Below::Rows("out 1\nout 2\n\n")
        );
        // It dropped a tenth of its rows at its limit, and the screen scrolled one more.
        let screen = "b\nsh-3.2$ claude\nout 1\nout 2\nout 3\n\n";
        assert_eq!(
            line.below(screen, 1801, false),
            Below::Rows("out 1\nout 2\nout 3\n\n")
        );
    }

    #[test]
    fn a_line_that_scrolled_off_or_was_cleared_away_is_gone_and_the_screen_is_newer() {
        let line = TypedLine::at(8, 3, 40, 0, "claude");
        // Four rows left the top: the line was the fourth.
        let screen = "4\n5\n6\n7\n8\n\n";
        assert_eq!(line.below(screen, 4, false), Below::Gone);
        assert!(line.drawn_below(screen, 4, false));
        // A clear that took the screen into the history, then a frame drawn.
        let screen = "\u{273b} Welcome\n\n> \n\n\n\n";
        assert_eq!(line.below(screen, 6, false), Below::Gone);
        assert!(line.drawn_below(screen, 6, false));
        // A clear of the history as well (Claude's /clear): the history is smaller than it was, and the
        // command is nowhere on the screen.
        let line_over_a_history = TypedLine::at(8, 3, 40, 50, "claude");
        assert_eq!(line_over_a_history.below(screen, 0, false), Below::Gone);
        // Cleared, with nothing drawn yet.
        assert!(!line.drawn_below("\n\n\n\n\n\n", 6, false));
    }

    #[test]
    fn the_alternate_screen_is_all_below_the_line() {
        let line = typed_on_the_fourth_row();
        let alternate = "\n\n  agent ui\n\n\n\n";
        assert_eq!(line.below(alternate, 0, true), Below::Rows(alternate));
        assert!(line.drawn_below(alternate, 0, true));
        assert!(!line.drawn_below("\n\n\n", 0, true));
    }

    #[test]
    fn a_command_is_echoed_once_the_cursor_is_where_it_ends_and_its_row_holds_it() {
        // `claude` typed at column 8 of the fourth row: nothing echoed yet, then part of it, then all.
        let before = "x\ny\n\nsh-3.2$ \n\n\n";
        assert_eq!(TypedLine::echoed(8, 40, "claude", (8, 3), 0, before), None);
        let part = "x\ny\n\nsh-3.2$ cla\n\n\n";
        assert_eq!(TypedLine::echoed(8, 40, "claude", (11, 3), 0, part), None);
        let all = "x\ny\n\nsh-3.2$ claude\n\n\n";
        assert_eq!(
            TypedLine::echoed(8, 40, "claude", (14, 3), 0, all),
            Some(TypedLine::at(8, 3, 40, 0, "claude"))
        );
        // The cursor where the command ends, on a row that does not hold it, is no echo.
        assert_eq!(TypedLine::echoed(8, 40, "claude", (14, 4), 0, all), None);
        // Nor is a prompt that holds the command's text itself, with the cursor where it was.
        let in_the_prompt = "~/claude $ \n\n";
        assert_eq!(
            TypedLine::echoed(11, 40, "claude", (11, 0), 0, in_the_prompt),
            None
        );
    }

    #[test]
    fn a_command_echoed_on_the_last_row_is_on_the_row_the_cursor_is_on_whatever_scrolled() {
        // Typed at column 30 of the last row of six: it wraps, and the echo scrolls the screen a row,
        // so the line is the last row, with the history a row bigger than it was.
        let typed = "claude --model the-longest-model-name";
        let echoed =
            "b\nc\nd\ne\nsh-3.2$ x                     claude --m\nodel the-longest-model-name\n";
        let line = TypedLine::echoed(30, 40, typed, (27, 5), 11, echoed)
            .unwrap()
            .starting_with(Some(start_of("", 30, 4, 40, typed)));
        assert_eq!(
            line.value(),
            "5 11 30 37 claude --model the-longest-model-name odel the-longest-model-name"
        );
        assert_eq!(
            line.below(echoed, 11, false),
            Below::Rows(""),
            "nothing is below the last row"
        );
    }

    #[test]
    fn a_command_that_ends_at_the_right_edge_is_echoed_with_the_cursor_on_the_next_row_or_at_the_edge()
     {
        // Eight columns of prompt and thirty-two of command fill a row of forty.
        let typed = "claude --resume 0123456789abcdef";
        let echoed = "sh-3.2$ claude --resume 0123456789abcdef\n\n\n";
        let expected = Some(TypedLine::at(8, 0, 40, 0, typed));
        assert_eq!(TypedLine::echoed(8, 40, typed, (0, 1), 0, echoed), expected);
        assert_eq!(
            TypedLine::echoed(8, 40, typed, (39, 0), 0, echoed),
            expected
        );
        // Anywhere else on either row, the echo is not done.
        assert_eq!(TypedLine::echoed(8, 40, typed, (20, 0), 0, echoed), None);
        assert_eq!(TypedLine::echoed(8, 40, typed, (3, 1), 0, echoed), None);
    }

    #[test]
    fn a_read_that_misses_the_line_reads_nothing_and_the_second_in_a_row_takes_it_for_gone() {
        let line = TypedLine::at(8, 3, 40, 0, "claude");
        let before_the_echo = "esc to interrupt\n? for shortcuts\n\nsh-3.2$ \n\n\n";
        let echoed = "esc to interrupt\n? for shortcuts\n\nsh-3.2$ claude\n\n\n";
        let mut misses = Misses::default();
        assert_eq!(misses.read(&line, before_the_echo, 0, false), Read::Nothing);
        assert!(misses.owe_a_look());
        // The echo came: the line is there, and the count starts over.
        assert_eq!(misses.read(&line, echoed, 0, false), Read::Rows("\n\n"));
        assert!(!misses.owe_a_look());
        assert_eq!(misses.read(&line, before_the_echo, 0, false), Read::Nothing);
        // Missed again by the next read: gone, and nothing is owed after it.
        assert_eq!(misses.read(&line, before_the_echo, 0, false), Read::Gone);
        assert!(!misses.owe_a_look());
        assert_eq!(misses.read(&line, before_the_echo, 0, false), Read::Nothing);
    }

    #[test]
    fn a_read_that_does_not_look_or_another_line_starts_the_count_over() {
        let line = TypedLine::at(8, 3, 40, 0, "claude");
        let blank = "\n\n\n\n\n\n";
        let mut misses = Misses::default();
        assert_eq!(misses.read(&line, blank, 0, false), Read::Nothing);
        misses.looked_away();
        assert!(!misses.owe_a_look());
        assert_eq!(misses.read(&line, blank, 0, false), Read::Nothing);
        // The next start recorded a line of its own: its first miss is a first.
        let next = TypedLine::at(8, 4, 40, 0, "claude");
        assert_eq!(misses.read(&next, blank, 0, false), Read::Nothing);
        assert_eq!(misses.read(&next, blank, 0, false), Read::Gone);
    }

    #[test]
    fn a_screen_with_no_rows_shows_no_line() {
        assert_eq!(typed_on_the_fourth_row().below("", 0, false), Below::Gone);
        assert!(!typed_on_the_fourth_row().drawn_below("", 0, false));
    }
}
