// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! A one-line text input with readline-style editing, shared by the filter and taw's
//! prompts: `/` opens a one-line input on the hint line (`tui-input`:
//! readline editing — Ctrl-W, Ctrl-A/E, Ctrl-U/K, Alt-b/f).
//!
//! It takes the rest of herdr's text-field keys as well: Ctrl-B/F, Ctrl-D, Ctrl-Y, Alt-D and
//! the word-wise Backspaces.
//!
//! Wraps `tui_input::Input` rather than reusing its crossterm backend:
//! `tui_input::backend::crossterm::to_input_request` takes a whole `Event` where this
//! module only ever sees a `KeyEvent`, maps Alt-b/f only under `KeyModifiers::META` (a
//! terminal's Alt key arrives as `ALT`, not `META`, so those two would never fire), and
//! sends Ctrl-U to `DeleteLine` — the *whole* buffer — rather than readline's "delete back
//! to the start". [`to_request`] drives the crate's own [`tui_input::InputRequest`]
//! variants directly instead, one key at a time, against the exact key set below.
//!
//! [`Input::cursor`] counts codepoints, matching `tui_input`'s own convention — not bytes —
//! so code that slices a prompt's text by cursor position can index it by `char`.
//!
//! The text of the last cut is kept here, not in `tui_input`: its own buffer joins cuts made
//! one straight after another and forgets itself on a cut that takes nothing, where herdr's
//! fields (and fzf) keep the last cut that took something, whole, for Ctrl-Y.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_input::InputRequest;

/// Where the keys that move or cut by a word (Alt-B, Alt-F, Alt-D) draw the line between
/// words. herdr's rule is the modal preset's, in its filter, its prompts and its picker; the
/// search preset's query line is a pure fzf search, so it is fzf's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WordBoundary {
    /// herdr's `word_boundary` (`text_editor.rs`, v0.9.1): the space there is, then one run of
    /// the same kind of character, a word (letters, digits and `_`) or a stretch of punctuation,
    /// so that a path or a name with a dot is several.
    #[default]
    Herdr,
    /// fzf's `forward-word`, `backward-word` and `kill-word`: whatever is not a letter or a
    /// digit is skipped, then one run of letters and digits, so `_`, `-`, `/` and `.` all
    /// separate words.
    Fzf,
}

/// A one-line text input: the `/` filter and taw's prompts (rename, a branch name, a
/// worktree's project path) all edit through one of these.
#[derive(Debug, Clone, Default)]
pub struct Input {
    inner: tui_input::Input,
    boundary: WordBoundary,
    /// The text of the last cut (Ctrl-U/K/W, Alt-D, the word-wise Backspaces), which
    /// Ctrl-Y puts back. One per field: it never leaves the input it was cut from.
    killed: String,
}

impl Input {
    /// A new input holding `initial`, cursor at its end.
    pub fn new(initial: &str) -> Self {
        Input {
            inner: tui_input::Input::new(initial.to_string()),
            boundary: WordBoundary::default(),
            killed: String::new(),
        }
    }

    /// This input with `boundary` for Alt-B, Alt-F and Alt-D.
    pub fn with_word_boundary(mut self, boundary: WordBoundary) -> Self {
        self.boundary = boundary;
        self
    }

    /// The current text.
    pub fn value(&self) -> &str {
        self.inner.value()
    }

    /// The cursor's position, in codepoints from the start of [`Input::value`].
    pub fn cursor(&self) -> usize {
        self.inner.cursor()
    }

    /// The text before the cursor and the text after it: what a view draws the cursor between.
    pub fn around_cursor(&self) -> (&str, &str) {
        let value = self.inner.value();
        let at = value
            .char_indices()
            .nth(self.inner.cursor())
            .map_or(value.len(), |(byte, _)| byte);
        value.split_at(at)
    }

    /// Replaces the text, cursor moving to its end — [`Input::new`], on an input already
    /// in use.
    pub fn set(&mut self, v: &str) {
        self.inner = tui_input::Input::new(v.to_string());
    }

    /// Empties the input.
    pub fn clear(&mut self) {
        self.inner.reset();
    }

    /// Applies one key: a character, a cursor move, or an edit. Returns whether `key` was
    /// one of those — `false` for anything else (Enter, Esc, Tab, an unmapped chord, ...),
    /// so the caller can treat an unconsumed key as a command instead.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        match to_request(key) {
            Some(req) => {
                self.apply(req);
                true
            }
            None => false,
        }
    }

    fn apply(&mut self, req: InputRequest) {
        match req {
            InputRequest::Yank => {
                for c in self.killed.clone().chars() {
                    self.inner.handle(InputRequest::InsertChar(c));
                }
            }
            InputRequest::DeleteFromStart
            | InputRequest::DeleteTillEnd
            | InputRequest::DeletePrevWord
            | InputRequest::DeleteNextWord => self.cut(req),
            // The word moves and the next word's cut agree on where a word ends, which
            // `tui_input`'s own rule does not (it goes to the start of the next word).
            InputRequest::GoToNextWord => self.go_by_word(true),
            InputRequest::GoToPrevWord => self.go_by_word(false),
            other => {
                self.inner.handle(other);
            }
        }
    }

    /// Runs a cut and keeps what it took, for Ctrl-Y. A cut that took nothing (Ctrl-K at
    /// the end of the line) leaves the last real one alone.
    ///
    /// What went is read off the request and the cursor, never by comparing the text before
    /// and after: a line that repeats itself (`abcab`, cut at 3) is ambiguous that way.
    fn cut(&mut self, req: InputRequest) {
        let before = self.inner.value().to_string();
        let was = before.chars().count();
        let at = self.inner.cursor();
        match req {
            // `tui_input`'s own cut of the next word goes to the start of the one after it, and
            // takes the space with it; the word boundaries here stop at the end of the word.
            InputRequest::DeleteNextWord => {
                for _ in 0..self.word_steps(true) {
                    self.inner.handle(InputRequest::DeleteNextChar);
                }
            }
            _ => {
                self.inner.handle(req);
            }
        }
        let (start, len) = match req {
            InputRequest::DeleteFromStart => (0, at),
            InputRequest::DeleteTillEnd => (at, was - at),
            // The cursor lands where the cut began.
            InputRequest::DeletePrevWord => {
                let now = self.inner.cursor();
                (now, at - now)
            }
            // The cursor stays; the text shrank by what was taken.
            InputRequest::DeleteNextWord => (at, was - self.inner.value().chars().count()),
            _ => return,
        };
        let taken: String = before.chars().skip(start).take(len).collect();
        if !taken.is_empty() {
            self.killed = taken;
        }
    }
}

impl Input {
    /// Moves the cursor to the end of the next word, or the start of the previous one.
    fn go_by_word(&mut self, forward: bool) {
        let step = if forward {
            InputRequest::GoToNextChar
        } else {
            InputRequest::GoToPrevChar
        };
        for _ in 0..self.word_steps(forward) {
            self.inner.handle(step);
        }
    }

    /// How many characters (graphemes, the unit the cursor and Delete step by) lie between the
    /// cursor and the end of the next word, going forward, or the start of the previous one,
    /// going back: what Alt-F, Alt-B and Alt-D pass over. From the cursor, whatever
    /// [`WordBoundary::kind`] calls a separator, then one run of one kind of character; what
    /// follows the run, a space included, stays.
    fn word_steps(&self, forward: bool) -> usize {
        let chars: Vec<char> = self.inner.value().chars().collect();
        let mut probe =
            tui_input::Input::new(self.inner.value().to_string()).with_cursor(self.inner.cursor());
        let step = if forward {
            InputRequest::GoToNextChar
        } else {
            InputRequest::GoToPrevChar
        };
        let (mut run, mut steps) = (0u8, 0);
        loop {
            let here = probe.cursor();
            probe.handle(step);
            let there = probe.cursor();
            if there == here {
                break;
            }
            // The character passed starts at the smaller of the two cursors.
            let kind = self.boundary.kind(chars[here.min(there)]);
            if run != 0 && kind != run {
                break;
            }
            run = kind;
            steps += 1;
        }
        steps
    }
}

impl WordBoundary {
    /// The kind of `c` for [`Input::word_steps`]: `0` is a separator, skipped before the run of
    /// one other kind that is the word. herdr's kinds are space, a word's own characters
    /// (letters, digits, `_`) and anything else; fzf's are letters and digits and the rest.
    fn kind(self, c: char) -> u8 {
        match self {
            WordBoundary::Herdr if c.is_whitespace() => 0,
            WordBoundary::Herdr if c.is_alphanumeric() || c == '_' => 1,
            WordBoundary::Herdr => 2,
            WordBoundary::Fzf => u8::from(c.is_alphanumeric()),
        }
    }
}

/// `key` as one [`InputRequest`], for the exact key set the filter and prompts use: a
/// plain character inserts; Backspace/Delete and Left/Right/Home/End edit or move by one
/// character; Ctrl-A/E jump to the edges; Ctrl-U deletes back to the start; Ctrl-K deletes
/// to the end; Ctrl-W deletes one word back; Alt-b/f move by a word; Ctrl-H is
/// Backspace's other spelling (some terminals send it for the same key). On top of those,
/// herdr's other text-field keys ([`extra_request`]). `None` for everything else, notably a
/// plain character combined with Ctrl or Alt outside that set (e.g. Ctrl-C), which the
/// caller is left free to treat as a command.
fn to_request(key: KeyEvent) -> Option<InputRequest> {
    base_request(key).or_else(|| extra_request(key))
}

/// The keys this input has always taken, each one a single [`InputRequest`].
///
/// A control chord's letter always arrives lower-case in practice — Ctrl clears a
/// character's case bits at the terminal, so Ctrl-A and Ctrl-Shift-A are the same chord —
/// but `to_ascii_lowercase` is cheap insurance against a terminal that reports it
/// upper-case anyway.
fn base_request(key: KeyEvent) -> Option<InputRequest> {
    use InputRequest::*;

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if let KeyCode::Char(c) = key.code {
        if ctrl {
            return match c.to_ascii_lowercase() {
                'a' => Some(GoToStart),
                'e' => Some(GoToEnd),
                'u' => Some(DeleteFromStart),
                'k' => Some(DeleteTillEnd),
                'w' => Some(DeletePrevWord),
                'h' => Some(DeletePrevChar),
                _ => None,
            };
        }
        if alt {
            return match c.to_ascii_lowercase() {
                'b' => Some(GoToPrevWord),
                'f' => Some(GoToNextWord),
                _ => None,
            };
        }
        return Some(InsertChar(c));
    }
    match key.code {
        KeyCode::Backspace if !ctrl && !alt => Some(DeletePrevChar),
        KeyCode::Delete => Some(DeleteNextChar),
        KeyCode::Left => Some(GoToPrevChar),
        KeyCode::Right => Some(GoToNextChar),
        KeyCode::Home => Some(GoToStart),
        KeyCode::End => Some(GoToEnd),
        _ => None,
    }
}

/// The rest of herdr's text-field keys (`keyboard.mdx`, "Editing text fields"): Ctrl-B and
/// Ctrl-F move one character, Ctrl-D deletes the next one, Alt-D cuts the next word,
/// Alt-Backspace and Ctrl-Backspace cut the previous word, and Ctrl-Y puts back the last cut.
fn extra_request(key: KeyEvent) -> Option<InputRequest> {
    use InputRequest::*;

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'b' => Some(GoToPrevChar),
            'f' => Some(GoToNextChar),
            'd' => Some(DeleteNextChar),
            'y' => Some(Yank),
            _ => None,
        },
        KeyCode::Char(c) if alt => match c.to_ascii_lowercase() {
            'd' => Some(DeleteNextWord),
            _ => None,
        },
        KeyCode::Backspace if ctrl || alt => Some(DeletePrevWord),
        _ => None,
    }
}

/// Whether `key` is one of the keys [`extra_request`] adds, which a tmux prefix (`prefix` or
/// `prefix2`) bound to the same key takes back: with tmux's default prefix, Ctrl-B stays the
/// prefix in every text field of the popup, and never moves a cursor. The keys the input has
/// always taken are not these, so a prefix that is one of them (Ctrl-A, say) behaves as it did.
pub fn is_extra(key: KeyEvent) -> bool {
    base_request(key).is_none() && extra_request(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn bare(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Backspace with `modifiers` held: a terminal that tells them apart (the kitty
    /// keyboard protocol, tmux's extended keys) sends these, others send Ctrl-H or ESC DEL.
    fn backspace_with(modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Backspace, modifiers)
    }

    /// An input holding `text` with the cursor on column `at`.
    fn input_at(text: &str, at: usize) -> Input {
        let mut input = Input::new(text);
        while input.cursor() > at {
            assert!(input.handle(bare(KeyCode::Left)));
        }
        input
    }

    /// Feeds `s`'s characters through `input.handle`, one `KeyEvent` at a time.
    fn type_str(input: &mut Input, s: &str) {
        for c in s.chars() {
            assert!(input.handle(ch(c)), "typing {c:?} should be consumed");
        }
    }

    #[test]
    fn types_a_and_b() {
        let mut input = Input::new("");
        type_str(&mut input, "ab");
        assert_eq!(input.value(), "ab");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn ctrl_w_deletes_the_word_left_of_the_cursor() {
        let mut input = Input::new("");
        type_str(&mut input, "~/dev/foo ");
        assert!(input.handle(ctrl('w')));
        assert_eq!(input.value(), "~/dev/");
        assert_eq!(input.cursor(), 6);
    }

    #[test]
    fn ctrl_u_deletes_back_to_the_start_not_the_whole_line() {
        let mut input = Input::new("");
        type_str(&mut input, "hello");
        assert!(input.handle(bare(KeyCode::Left)));
        assert!(input.handle(bare(KeyCode::Left)));
        assert_eq!(input.cursor(), 3);
        assert!(input.handle(ctrl('u')));
        // Only "hel" (up to the cursor) is gone; "lo" (after it) survives — unlike
        // `tui_input`'s own Ctrl-U, which is `DeleteLine` and would clear everything.
        assert_eq!(input.value(), "lo");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn ctrl_a_then_typing_inserts_at_the_start() {
        let mut input = Input::new("");
        type_str(&mut input, "bcd");
        assert!(input.handle(ctrl('a')));
        assert_eq!(input.cursor(), 0);
        type_str(&mut input, "a");
        assert_eq!(input.value(), "abcd");
        assert_eq!(input.cursor(), 1);
    }

    #[test]
    fn ctrl_e_moves_to_the_end() {
        let mut input = Input::new("");
        type_str(&mut input, "abc");
        assert!(input.handle(ctrl('a')));
        assert_eq!(input.cursor(), 0);
        assert!(input.handle(ctrl('e')));
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn alt_b_moves_back_a_word() {
        let mut input = Input::new("");
        type_str(&mut input, "foo bar");
        assert!(input.handle(alt('b')));
        assert_eq!(input.cursor(), 4);
        // The cursor really did land at the start of "bar", not merely somewhere earlier.
        type_str(&mut input, "X");
        assert_eq!(input.value(), "foo Xbar");
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn alt_f_moves_forward_a_word() {
        let mut input = Input::new("");
        type_str(&mut input, "foo bar");
        assert!(input.handle(ctrl('a')));
        assert_eq!(input.cursor(), 0);
        assert!(input.handle(alt('f')));
        // Lands on the end of "foo", where herdr's and fzf's word moves both stop: not on the
        // start of "bar", where `tui_input::InputRequest::GoToNextWord` would take it.
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn delete_removes_the_character_after_the_cursor() {
        let mut input = Input::new("");
        type_str(&mut input, "ab");
        assert!(input.handle(bare(KeyCode::Home)));
        assert!(input.handle(bare(KeyCode::Delete)));
        assert_eq!(input.value(), "b");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn home_and_end_move_to_the_edges() {
        let mut input = Input::new("");
        type_str(&mut input, "abc");
        assert!(input.handle(bare(KeyCode::Home)));
        assert_eq!(input.cursor(), 0);
        assert!(input.handle(bare(KeyCode::End)));
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn ctrl_k_deletes_from_the_cursor_to_the_end() {
        let mut input = Input::new("");
        type_str(&mut input, "hello world");
        assert!(input.handle(bare(KeyCode::Home)));
        for _ in 0..5 {
            assert!(input.handle(bare(KeyCode::Right)));
        }
        assert_eq!(input.cursor(), 5);
        assert!(input.handle(ctrl('k')));
        assert_eq!(input.value(), "hello");
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn ctrl_h_deletes_the_character_before_the_cursor_like_backspace() {
        let mut input = Input::new("");
        type_str(&mut input, "ab");
        assert!(input.handle(ctrl('h')));
        assert_eq!(input.value(), "a");
        assert_eq!(input.cursor(), 1);
    }

    #[test]
    fn backspace_deletes_the_character_before_the_cursor() {
        let mut input = Input::new("");
        type_str(&mut input, "ab");
        assert!(input.handle(bare(KeyCode::Backspace)));
        assert_eq!(input.value(), "a");
        assert_eq!(input.cursor(), 1);
    }

    #[test]
    fn new_starts_with_the_initial_value_and_cursor_at_its_end() {
        let input = Input::new("~/");
        assert_eq!(input.value(), "~/");
        assert_eq!(input.cursor(), 2);
    }

    /// What a view draws its cursor between. The cursor counts codepoints, so a multibyte
    /// character is one step, and the split is never inside one.
    #[test]
    fn around_cursor_is_the_text_either_side_of_the_cursor() {
        let mut input = Input::new("h\u{e9}llo w\u{f6}rld");
        assert_eq!(input.around_cursor(), ("h\u{e9}llo w\u{f6}rld", ""));

        input.handle(ctrl('a'));
        assert_eq!(input.around_cursor(), ("", "h\u{e9}llo w\u{f6}rld"));
        input.handle(ctrl('f'));
        input.handle(ctrl('f'));
        assert_eq!(input.around_cursor(), ("h\u{e9}", "llo w\u{f6}rld"));
        input.handle(alt('f'));
        assert_eq!(input.around_cursor(), ("h\u{e9}llo", " w\u{f6}rld"));
        input.handle(ctrl('f'));
        input.handle(ctrl('f'));
        input.handle(ctrl('f'));
        assert_eq!(input.around_cursor(), ("h\u{e9}llo w\u{f6}", "rld"));

        assert_eq!(Input::new("").around_cursor(), ("", ""));
    }

    #[test]
    fn set_replaces_the_value_and_moves_the_cursor_to_its_end() {
        let mut input = Input::new("");
        type_str(&mut input, "abc");
        input.set("xy");
        assert_eq!(input.value(), "xy");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn clear_empties_the_value_and_resets_the_cursor() {
        let mut input = Input::new("");
        type_str(&mut input, "abc");
        input.clear();
        assert_eq!(input.value(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn unhandled_keys_are_not_consumed_and_leave_the_input_untouched() {
        let mut input = Input::new("");
        type_str(&mut input, "ab");
        for key in [
            bare(KeyCode::Enter),
            bare(KeyCode::Esc),
            bare(KeyCode::Tab),
            ctrl('c'),
            alt('x'),
        ] {
            assert!(!input.handle(key), "{key:?} should not be consumed");
        }
        assert_eq!(input.value(), "ab");
        assert_eq!(input.cursor(), 2);
    }

    // ------------------------------------------------------------ herdr's text keys

    // Follows herdr's src/client/shell/text_editor.rs tests. Differences: the
    // table and the kill and yank chain run on this input's keys, and the word test is Alt-D's, cut
    // one run after another where herdr's walks Ctrl-W and Alt-F over the same line.

    /// herdr's table, row for row (`src/client/shell/text_editor.rs` at v0.9.1,
    /// `every_binding_edits_at_the_cursor`): every key of a text field, pressed in
    /// "one two" with the cursor before "two". `(key, text after, cursor after, last cut)`.
    #[test]
    fn every_key_edits_at_the_cursor() {
        let alt_backspace = backspace_with(KeyModifiers::ALT);
        let ctrl_backspace = backspace_with(KeyModifiers::CONTROL);
        for (key, text, cursor, killed) in [
            (bare(KeyCode::Left), "one two", 3, ""),
            (ctrl('b'), "one two", 3, ""),
            (bare(KeyCode::Right), "one two", 5, ""),
            (ctrl('f'), "one two", 5, ""),
            (bare(KeyCode::Home), "one two", 0, ""),
            (ctrl('a'), "one two", 0, ""),
            (bare(KeyCode::End), "one two", 7, ""),
            (ctrl('e'), "one two", 7, ""),
            (bare(KeyCode::Backspace), "onetwo", 3, ""),
            (ctrl('h'), "onetwo", 3, ""),
            (bare(KeyCode::Delete), "one wo", 4, ""),
            (ctrl('d'), "one wo", 4, ""),
            (alt('b'), "one two", 0, ""),
            (alt('f'), "one two", 7, ""),
            (ctrl('u'), "two", 0, "one "),
            (ctrl('k'), "one ", 4, "two"),
            (ctrl('w'), "two", 0, "one "),
            (alt_backspace, "two", 0, "one "),
            (ctrl_backspace, "two", 0, "one "),
            (alt('d'), "one ", 4, "two"),
            (ctrl('y'), "one two", 4, ""),
            (ch('X'), "one Xtwo", 5, ""),
        ] {
            let mut input = input_at("one two", 4);
            assert!(input.handle(key), "{key:?} is an editing key");
            assert_eq!(
                (input.value(), input.cursor(), input.killed.as_str()),
                (text, cursor, killed),
                "{key:?}"
            );
            // And on an empty field every one of them is still an editing key, which does
            // nothing and does not panic.
            let mut empty = Input::new("");
            assert!(empty.handle(key), "{key:?} on an empty field");
            if ch('X') != key {
                assert_eq!(empty.value(), "", "{key:?} on an empty field");
            }
        }
    }

    /// Ctrl-Y puts back the last cut text, wherever the cursor is by then. herdr's
    /// `suggestions_movement_kills_and_yank`: a cut that took nothing (Ctrl-K at the end of
    /// the line) leaves the buffer as it was, and Ctrl-U cuts the whole line.
    #[test]
    fn ctrl_y_puts_back_the_last_cut_text() {
        let mut input = Input::new("one two");
        assert!(input.handle(ctrl('w')));
        assert_eq!(input.value(), "one ");
        assert!(input.handle(ctrl('k')));
        assert!(input.handle(bare(KeyCode::Backspace)));
        assert_eq!(input.value(), "one");
        assert!(input.handle(ctrl('y')));
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "onetwotwo");
        assert_eq!(input.cursor(), 9);
        assert!(input.handle(ctrl('u')));
        assert_eq!(input.value(), "");
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "onetwotwo", "Ctrl-U cut the whole line");
    }

    /// A second cut replaces the first (herdr's and fzf's rule), even straight after it.
    #[test]
    fn a_cut_replaces_the_one_before_it() {
        let mut input = Input::new("a b c");
        assert!(input.handle(ctrl('w')));
        assert!(input.handle(ctrl('w')));
        assert_eq!(input.value(), "a ");
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "a b ", "only the word cut last comes back");
        assert_eq!(input.cursor(), 4);
    }

    /// An empty cut is not a cut: Ctrl-W at the start of the line, Ctrl-K at its end.
    #[test]
    fn a_cut_that_takes_nothing_keeps_what_was_cut_before() {
        let mut input = Input::new("one two");
        assert!(input.handle(ctrl('w')));
        assert!(input.handle(bare(KeyCode::Backspace)));
        assert!(input.handle(ctrl('k')), "at the end of the line");
        assert!(input.handle(ctrl('a')));
        assert!(input.handle(ctrl('w')), "at the start of the line");
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "twoone");
    }

    /// What a cut took is what the request says it took, not what the text happens to repeat:
    /// `abcab` cut back to column 3 took `abc`, which a comparison of the two texts would
    /// read as `cab`.
    #[test]
    fn a_cut_is_what_the_cursor_says_even_in_a_line_that_repeats_itself() {
        let mut input = input_at("abcab", 3);
        assert!(input.handle(ctrl('u')));
        assert_eq!(input.value(), "ab");
        assert_eq!(input.killed, "abc");
        let mut input = input_at("abab", 2);
        assert!(input.handle(ctrl('k')));
        assert_eq!(input.value(), "ab");
        assert_eq!(input.killed, "ab");
        let mut input = input_at("xyxy", 2);
        assert!(input.handle(alt('d')));
        assert_eq!(input.value(), "xy");
        assert_eq!(input.killed, "xy");
    }

    /// Cut text stays in the field it was cut from: nothing is shared, and nothing reaches
    /// a clipboard (herdr: "Cut text stays within the current field").
    #[test]
    fn the_last_cut_belongs_to_its_own_field() {
        let mut first = Input::new("one two");
        assert!(first.handle(ctrl('w')));
        let mut other = Input::new("");
        assert!(other.handle(ctrl('y')));
        assert_eq!(other.value(), "");
        assert_eq!(first.killed, "two");
    }

    /// Setting a field's text, as a completion does, keeps what was cut from it.
    #[test]
    fn replacing_the_text_keeps_the_last_cut() {
        let mut input = Input::new("one two");
        assert!(input.handle(ctrl('w')));
        input.set("~/projects/");
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "~/projects/two");
    }

    /// Alt-D cuts to the end of the word and leaves what separates it from the next, as herdr's
    /// `word_boundary` does (and fzf's `kill-word`, in the search preset): the space after "two"
    /// stays.
    #[test]
    fn alt_d_cuts_the_next_word_and_ctrl_y_puts_it_back_where_the_cursor_is() {
        let mut input = input_at("one two three", 4);
        assert!(input.handle(alt('d')));
        assert_eq!(input.value(), "one  three");
        assert_eq!(input.cursor(), 4);
        assert_eq!(input.killed, "two");
        assert!(input.handle(ctrl('a')));
        assert!(input.handle(ctrl('y')));
        assert_eq!(input.value(), "twoone  three");
    }

    /// What Alt-D takes is herdr's forward `word_boundary` (`text_editor.rs`, v0.9.1): the space
    /// in front of the cursor's word, if the cursor is in some, and then one run of the same
    /// kind of character, a word (letters, digits, `_`) or a stretch of punctuation, where a
    /// path or a name with a dot is several. A grapheme is one character, so the accent and the
    /// joined emoji are not cut in two. Cut one after another from the start of herdr's own line
    /// (`words_distinguish_paths_punctuation_and_whitespace`).
    #[test]
    fn alt_d_takes_a_word_a_punctuation_run_or_a_run_of_space_and_one_of_them() {
        let mut input = Input::new("src/foo_bar.rs  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}");
        input.handle(ctrl('a'));
        for (left, took) in [
            (
                "/foo_bar.rs  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}",
                "src",
            ),
            (
                "foo_bar.rs  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}",
                "/",
            ),
            (
                ".rs  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}",
                "foo_bar",
            ),
            ("rs  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}", "."),
            ("  e\u{301}\u{4e2d} \u{1f469}\u{200d}\u{1f4bb}", "rs"),
            (" \u{1f469}\u{200d}\u{1f4bb}", "  e\u{301}\u{4e2d}"),
            ("", " \u{1f469}\u{200d}\u{1f4bb}"),
        ] {
            assert!(input.handle(alt('d')));
            assert_eq!(input.value(), left);
            assert_eq!(input.killed, took);
            assert_eq!(input.cursor(), 0);
        }
        assert!(input.handle(alt('d')), "nothing is left to cut");
        assert_eq!(
            input.killed, " \u{1f469}\u{200d}\u{1f4bb}",
            "and the last cut is kept"
        );

        // In the space between two words it takes the space and the next word, and none of
        // what follows.
        let mut input = input_at("a  b c", 1);
        assert!(input.handle(alt('d')));
        assert_eq!(input.value(), "a c");
        assert_eq!(input.killed, "  b");
    }

    /// Cursor after each of `n` presses of `key`, from where `input` stands.
    fn cursors_after(input: &mut Input, key: KeyEvent, n: usize) -> Vec<usize> {
        (0..n)
            .map(|_| {
                assert!(input.handle(key));
                input.cursor()
            })
            .collect()
    }

    /// Alt-F and Alt-B walk herdr's word boundary in the modal preset's fields, the line herdr's
    /// own test walks (`words_distinguish_paths_punctuation_and_whitespace`): the space, then a
    /// run of one kind of character, a word with its `_` or punctuation, each a stop.
    #[test]
    fn alt_f_and_alt_b_walk_herdrs_word_boundaries() {
        let mut input = Input::new(" /tmp/foo_bar.rs");
        assert!(input.handle(ctrl('a')));
        assert_eq!(
            cursors_after(&mut input, alt('f'), 7),
            [2, 5, 6, 13, 14, 16, 16],
            "forward, and it stays at the end"
        );
        assert_eq!(
            cursors_after(&mut input, alt('b'), 8),
            [14, 13, 6, 5, 2, 1, 0, 0],
            "back again, the space before the first run a stop of its own"
        );
    }

    /// The search preset's query is a pure fzf search, so there the same keys follow fzf 0.73.1
    /// (read off a real fzf): what is not a letter or a digit is skipped and only letters and
    /// digits are a word, so `_`, `.` and `/` separate. Forward lands on the end of a word and
    /// back on the start of one.
    #[test]
    fn alt_f_and_alt_b_walk_fzfs_word_boundaries_in_the_search_preset() {
        let mut input = Input::new("src/foo_bar.rs  x").with_word_boundary(WordBoundary::Fzf);
        assert!(input.handle(ctrl('a')));
        assert_eq!(
            cursors_after(&mut input, alt('f'), 6),
            [3, 7, 11, 14, 17, 17]
        );
        assert_eq!(cursors_after(&mut input, alt('b'), 6), [16, 12, 8, 4, 0, 0]);
    }

    /// fzf's kill-word, from the start of the line (the examples are real fzf 0.73.1 runs).
    #[test]
    fn alt_d_in_the_search_preset_cuts_as_fzfs_kill_word_does() {
        for (text, cuts) in [
            ("tmux-agent-wizard", &["-agent-wizard", "-wizard", ""][..]),
            (
                "src/foo_bar.rs  x",
                &["/foo_bar.rs  x", "_bar.rs  x", ".rs  x", "  x", ""][..],
            ),
            ("a1b2 c3 d4e", &[" c3 d4e", " d4e", ""][..]),
        ] {
            let mut input = Input::new(text).with_word_boundary(WordBoundary::Fzf);
            assert!(input.handle(ctrl('a')));
            for left in cuts {
                assert!(input.handle(alt('d')));
                assert_eq!(input.value(), *left, "{text}");
                assert_eq!(input.cursor(), 0);
            }
        }
        // From inside a word, from a gap and with nothing but separators ahead.
        for (text, at, left, took) in [
            ("abcdef gh", 2, "ab gh", "cdef"),
            ("ab  cd ef", 2, "ab ef", "  cd"),
            ("abc   ", 3, "abc", "   "),
            ("abc ,;", 3, "abc", " ,;"),
        ] {
            let mut input = input_at(text, at).with_word_boundary(WordBoundary::Fzf);
            assert!(input.handle(alt('d')));
            assert_eq!(input.value(), left, "{text} at {at}");
            assert_eq!(input.killed, took, "{text} at {at}");
        }
    }

    /// The same text cut by herdr's rule, which is the other fields': `-` is a run of its own.
    #[test]
    fn alt_d_in_the_modal_preset_and_the_prompts_keeps_herdrs_boundary() {
        let mut input = Input::new("tmux-agent-wizard");
        assert!(input.handle(ctrl('a')));
        for left in ["-agent-wizard", "agent-wizard", "-wizard", "wizard", ""] {
            assert!(input.handle(alt('d')));
            assert_eq!(input.value(), left);
        }
    }

    /// Alt-B and Alt-D agree on what a word is, in each place, so going back over a word and
    /// cutting takes exactly that word, and Alt-F then Alt-B is back where it began.
    #[test]
    fn alt_b_then_alt_d_cuts_the_word_moved_back_over_in_each_place() {
        for boundary in [WordBoundary::Herdr, WordBoundary::Fzf] {
            let mut input = Input::new("one two three").with_word_boundary(boundary);
            input.handle(ctrl('a'));
            input.handle(alt('f'));
            input.handle(alt('f'));
            let there = input.cursor();
            assert!(input.handle(alt('b')));
            assert!(input.handle(alt('d')));
            assert_eq!(input.value(), "one  three", "{boundary:?}");
            assert_eq!(input.killed, "two", "{boundary:?}");
            let mut input = Input::new("one two three").with_word_boundary(boundary);
            input.handle(ctrl('a'));
            input.handle(alt('f'));
            input.handle(alt('f'));
            assert_eq!(input.cursor(), there);
            input.handle(alt('b'));
            assert_eq!(input.cursor(), 4, "{boundary:?}: the start of \"two\"");
        }
    }

    #[test]
    fn the_word_wise_backspaces_cut_the_previous_word_and_a_plain_one_a_character() {
        for modifiers in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
            let mut input = Input::new("foo bar");
            assert!(input.handle(backspace_with(modifiers)));
            assert_eq!(input.value(), "foo ", "{modifiers:?}+Backspace");
            assert_eq!(input.killed, "bar");
        }
        let mut input = Input::new("foo bar");
        assert!(input.handle(backspace_with(KeyModifiers::SHIFT)));
        assert_eq!(
            input.value(),
            "foo ba",
            "Shift+Backspace is still one character"
        );
        assert_eq!(input.killed, "");
    }

    #[test]
    fn ctrl_d_deletes_the_character_under_the_cursor_and_does_not_cut_it() {
        let mut input = input_at("abc", 1);
        assert!(input.handle(ctrl('d')));
        assert_eq!(input.value(), "ac");
        assert_eq!(input.cursor(), 1);
        assert_eq!(input.killed, "");
        assert!(input.handle(ctrl('e')));
        assert!(
            input.handle(ctrl('d')),
            "at the end there is nothing to delete"
        );
        assert_eq!(input.value(), "ac");
    }

    #[test]
    fn ctrl_b_and_ctrl_f_move_one_character() {
        let mut input = Input::new("abc");
        assert!(input.handle(ctrl('b')));
        assert!(input.handle(ctrl('b')));
        assert_eq!(input.cursor(), 1);
        assert!(input.handle(ctrl('f')));
        assert_eq!(input.cursor(), 2);
        assert!(input.handle(ctrl('f')));
        assert!(input.handle(ctrl('f')), "past the end it stays at the end");
        assert_eq!(input.cursor(), 3);
        assert!(input.handle(ctrl('a')));
        assert!(
            input.handle(ctrl('b')),
            "before the start it stays at the start"
        );
        assert_eq!(input.cursor(), 0);
    }

    /// The keys a tmux prefix can take back (see [`is_extra`]) are exactly the ones added
    /// on top of what the input always had.
    #[test]
    fn only_the_added_keys_are_extra() {
        for key in [
            ctrl('b'),
            ctrl('f'),
            ctrl('d'),
            ctrl('y'),
            alt('d'),
            backspace_with(KeyModifiers::ALT),
            backspace_with(KeyModifiers::CONTROL),
        ] {
            assert!(is_extra(key), "{key:?}");
        }
        for key in [
            ch('b'),
            ctrl('a'),
            ctrl('e'),
            ctrl('u'),
            ctrl('k'),
            ctrl('w'),
            ctrl('h'),
            alt('b'),
            alt('f'),
            bare(KeyCode::Backspace),
            bare(KeyCode::Delete),
            bare(KeyCode::Left),
            ctrl('c'),
            alt('x'),
        ] {
            assert!(!is_extra(key), "{key:?}");
        }
    }
}
