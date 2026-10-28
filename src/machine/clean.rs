//! The one cleaner for text that comes from another machine: names, titles, paths, toasts and the
//! captured pane of a preview.
//!
//! What a far machine sends is shown on this terminal, so it reaches the screen only through here:
//! [`plain`] for a line of text, [`preview`] for a captured pane (colours kept, nothing else that is
//! not text), and [`state`] for a whole state (the rows whose ids are not tmux's dropped).

use super::wire;

/// The most characters [`plain`] keeps.
pub const MAX_TEXT: usize = 256;

/// `s` as one line of plain text: without control characters (C0 with tab and newline, DEL and C1),
/// without a whole escape sequence of any kind, without the characters that reorder text, and cut at
/// [`MAX_TEXT`] characters.
pub fn plain(s: &str) -> String {
    strip(s, Keep::Nothing, Some(MAX_TEXT))
}

/// `s` as one line of plain text exactly as [`plain`] makes it, but not cut: for a caller that looks at
/// the whole line (to classify it) before it shows any of it, and then cuts with [`cut`].
pub(crate) fn plain_uncut(s: &str) -> String {
    strip(s, Keep::Nothing, None)
}

/// The first [`MAX_TEXT`] characters of `s`: what is shown of a line that [`plain_uncut`] made.
pub(crate) fn cut(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

/// `s` as a captured pane: the colours (`ESC [`, digits, `;` or `:`, `m`) and the newlines are kept, and
/// every other sequence, control character and reordering character is taken out. Not cut.
pub fn preview(s: &str) -> String {
    strip(s, Keep::Colours, None)
}

/// `s` with the rows that hold an id tmux does not make taken out. Each id field has its own sigil: a
/// pane's ids (`pane_id`, `active_pane_id`) start with `%`, a session's with `$` and a window's
/// (`window_id`, `active_window_id`) with `@`, each followed by digits (at most 20 of them). An id with
/// another sigil is dropped with its row: a later verb sends these ids back as `-t <id>`, and `@5` in
/// place of a pane id would name a window. The ids a row points at may be none. A worktree's
/// `workspace_id` is not a tmux id and is left as it is. Every string of a line is already clean
/// (`wire::Down::cleaned`, ids included, so `%1` and a bell is `%1` here) before a state comes here; this
/// checks what that cannot, which strings are ids of their field.
pub fn state(s: wire::State) -> wire::State {
    let pane = |id: &str| is_id(id, '%');
    let session = |id: &str| is_id(id, '$');
    let window = |id: &str| is_id(id, '@');
    // An id a row points at: none is fine, a bad one is not.
    let pane_or_none = |id: &str| id.is_empty() || pane(id);
    let window_or_none = |id: &str| id.is_empty() || window(id);
    wire::State {
        spaces: s
            .spaces
            .into_iter()
            .filter(|row| {
                session(&row.session_id)
                    && window_or_none(&row.active_window_id)
                    && row.active_pane_id.as_deref().is_none_or(pane_or_none)
            })
            .collect(),
        panes: s
            .panes
            .into_iter()
            .filter(|row| pane(&row.pane_id) && session(&row.session_id) && window(&row.window_id))
            .collect(),
        agents: s
            .agents
            .into_iter()
            .filter(|row| pane(&row.pane_id) && session(&row.session_id) && window(&row.window_id))
            .collect(),
        worktrees: s.worktrees,
        open_worktrees: s.open_worktrees,
    }
}

/// `toast` with its `pane` kept only when it is empty or a pane id (`%` and digits, the same rule as the
/// rows of a [`state`]); any other string becomes empty. The toast itself, its text and its kind stay: a
/// later verb sends the pane back as `-t <pane>`, and `@5` in its place would name a window.
pub fn toast(toast: wire::Toast) -> wire::Toast {
    wire::Toast {
        pane: pane_or_none(toast.pane),
        ..toast
    }
}

/// `left` with its `pane` kept only when it is empty or a pane id, as [`toast`] does.
pub fn left(left: wire::Left) -> wire::Left {
    wire::Left {
        pane: pane_or_none(left.pane),
        ..left
    }
}

/// `id` when it is empty or a pane id, and the empty string otherwise.
fn pane_or_none(id: String) -> String {
    if id.is_empty() || is_id(&id, '%') {
        id
    } else {
        String::new()
    }
}

/// The most characters an id may have: tmux counts ids up from 0 in a `u64`, which is 20 digits and a
/// sign character. A longer one is not tmux's, and is dropped with its row, not cut.
const MAX_ID: usize = 21;

/// Whether `id` is a tmux id that starts with `sigil`.
fn is_id(id: &str, sigil: char) -> bool {
    id.len() <= MAX_ID && id.starts_with(sigil) && super::valid_tmux_id(id)
}

/// What [`strip`] leaves of the sequences and controls.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Nothing but text.
    Nothing,
    /// Text, newlines and colour sequences.
    Colours,
}

/// The characters that reorder or direct text, which a name or a title has no use for.
fn reorders(c: char) -> bool {
    matches!(
        c as u32,
        0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

/// Where the string that starts at `from` (the first character after its introducer) ends: after a BEL
/// or after `ESC \`. A string that never ends takes the rest.
fn string_end(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            '\u{7}' => return i + 1,
            '\u{1b}' if chars.get(i + 1) == Some(&'\\') => return i + 2,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Where the escape sequence that starts at `at` (an ESC) ends, and whether it is a colour (`ESC [`,
/// digits, `;` or `:`, `m`).
fn sequence_end(chars: &[char], at: usize) -> (usize, bool) {
    let Some(&next) = chars.get(at + 1) else {
        return (at + 1, false);
    };
    match next {
        // A control sequence: parameters and intermediates, then its final character. A character that
        // is none of these ends it unfinished, and is left for the caller to look at.
        '[' => {
            let mut i = at + 2;
            while chars
                .get(i)
                .is_some_and(|c| ('\u{20}'..='\u{3f}').contains(c))
            {
                i += 1;
            }
            match chars.get(i) {
                Some(&last) if ('\u{40}'..='\u{7e}').contains(&last) => {
                    let colour = last == 'm'
                        && chars[at + 2..i]
                            .iter()
                            .all(|c| c.is_ascii_digit() || *c == ';' || *c == ':');
                    (i + 1, colour)
                }
                _ => (i, false),
            }
        }
        // A string: operating system command, device control, start of string, privacy message,
        // application program command.
        ']' | 'P' | 'X' | '^' | '_' => (string_end(chars, at + 2), false),
        // Any other: intermediates, then one final character.
        _ => {
            let mut i = at + 1;
            while chars
                .get(i)
                .is_some_and(|c| ('\u{20}'..='\u{2f}').contains(c))
            {
                i += 1;
            }
            match chars.get(i) {
                Some(&last) if ('\u{30}'..='\u{7e}').contains(&last) => (i + 1, false),
                _ => (i.max(at + 1), false),
            }
        }
    }
}

fn strip(s: &str, keep: Keep, cut: Option<usize>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len().min(1024));
    let mut kept = 0usize;
    let mut i = 0;
    while i < chars.len() {
        if cut.is_some_and(|most| kept >= most) {
            break;
        }
        let c = chars[i];
        if c == '\u{1b}' {
            let (end, colour) = sequence_end(&chars, i);
            if colour && keep == Keep::Colours {
                out.extend(&chars[i..end]);
                kept += end - i;
            }
            i = end;
        } else {
            if (!c.is_control() && !reorders(c)) || (c == '\n' && keep == Keep::Colours) {
                out.push(c);
                kept += 1;
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(code: u32) -> char {
        char::from_u32(code).unwrap()
    }

    #[test]
    fn the_pane_of_a_toast_or_a_left_is_a_pane_id_or_empty() {
        let toast_with = |pane: &str| wire::Toast {
            kind: "blocked".to_string(),
            pane: pane.to_string(),
            text: "garden needs you".to_string(),
            title: "t".to_string(),
            lease: true,
            ..wire::Toast::default()
        };
        let left_with = |pane: &str| wire::Left {
            holder: "mac-1".to_string(),
            pane: pane.to_string(),
            go: Some("%2".to_string()),
        };
        // A pane id is kept; the 20-digit one is the longest.
        let longest = format!("%{}", "9".repeat(20));
        for good in ["%31", "%0", longest.as_str()] {
            assert_eq!(toast(toast_with(good)), toast_with(good), "{good}");
            assert_eq!(left(left_with(good)), left_with(good), "{good}");
        }
        // Empty stays empty.
        assert_eq!(toast(toast_with("")), toast_with(""));
        assert_eq!(left(left_with("")), left_with(""));
        // Another sigil, a trailing letter, a bare sigil, a 25-digit id and a plain word are emptied,
        // and the rest of the toast and of the left is as it was.
        let too_long = format!("%{}", "1".repeat(25));
        for bad in [
            "@5",
            "$3",
            "%1x",
            "%",
            too_long.as_str(),
            "31",
            "%-1",
            "%1 ",
        ] {
            assert_eq!(toast(toast_with(bad)), toast_with(""), "{bad:?}");
            assert_eq!(left(left_with(bad)), left_with(""), "{bad:?}");
        }
    }

    #[test]
    fn plain_removes_c0_controls_tab_and_newline_del_and_c1() {
        assert_eq!(plain("a\tb\nc\rd\x07e\x00f\x7fg"), "abcdefg");
        let c1: String = (0x80..=0x9f).map(ch).collect();
        assert_eq!(plain(&format!("x{c1}y")), "xy");
        // The characters on both sides of C1 stay.
        assert_eq!(
            plain(&format!("{}{}", ch(0x7e), ch(0xa0))),
            format!("{}{}", ch(0x7e), ch(0xa0))
        );
    }

    #[test]
    fn plain_removes_each_escape_sequence_whole() {
        for (text, left) in [
            ("a\x1b[31mb", "ab"),
            ("a\x1b[1;38:5:196mb", "ab"),
            ("a\x1b[2Jb", "ab"),
            ("a\x1b[?25lb", "ab"),
            ("a\x1b]0;a title\x07b", "ab"),
            ("a\x1b]52;c;SGVsbG8=\x1b\\b", "ab"),
            ("a\x1b]8;;http://x\x07link\x1b]8;;\x07b", "alinkb"),
            ("a\x1bPq#0;2;0;0;0\x1b\\b", "ab"),
            ("a\x1bXsos text\x1b\\b", "ab"),
            ("a\x1b^pm text\x1b\\b", "ab"),
            ("a\x1b_apc text\x1b\\b", "ab"),
            ("a\x1bcb", "ab"),
            ("a\x1b7b", "ab"),
            ("a\x1b(Bb", "ab"),
            ("a\x1b\\b", "ab"),
        ] {
            assert_eq!(plain(text), left, "{text:?}");
        }
    }

    #[test]
    fn plain_takes_the_rest_when_a_string_never_ends_and_keeps_what_follows_a_cut_short_sequence() {
        assert_eq!(plain("a\x1b]0;never ends"), "a");
        assert_eq!(plain("a\x1bPnever ends"), "a");
        // An escape that ends the text, or is followed by a control, leaves only itself out.
        assert_eq!(plain("a\x1b"), "a");
        assert_eq!(plain("a\x1b\nb"), "ab");
        // A second escape starts its own sequence.
        assert_eq!(plain("a\x1b\x1b[31mb"), "ab");
        // A control cuts a control sequence short: it is not whole, so what follows is text.
        assert_eq!(plain("a\x1b[1\nb"), "ab");
    }

    #[test]
    fn plain_removes_the_characters_that_reorder_text() {
        for code in [
            0x061c, 0x200e, 0x200f, 0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067, 0x2068,
            0x2069,
        ] {
            assert_eq!(plain(&format!("a{}b", ch(code))), "ab", "{code:x}");
        }
        // Next to them: the zero width joiner and a letter of the right-to-left scripts stay.
        let kept = format!("{}{}", ch(0x200d), ch(0x05d0));
        assert_eq!(plain(&kept), kept);
    }

    #[test]
    fn plain_cuts_at_256_characters_after_what_it_removed() {
        let long = "a".repeat(300);
        assert_eq!(plain(&long).chars().count(), 256);
        assert_eq!(plain(&"a".repeat(256)).chars().count(), 256);
        assert_eq!(plain(&"a".repeat(255)).chars().count(), 255);
        // Characters, not bytes.
        let wide: String = std::iter::repeat_n(ch(0x4e2d), 300).collect();
        assert_eq!(plain(&wide).chars().count(), 256);
        // What is removed does not count: 100 escapes then 256 letters.
        let text = format!("{}{}", "\x1b[1m".repeat(100), "b".repeat(300));
        assert_eq!(plain(&text), "b".repeat(256));
    }

    #[test]
    fn the_uncut_step_keeps_what_plain_cuts_and_cut_is_the_rest_of_plain() {
        let long = "a".repeat(300);
        assert_eq!(plain_uncut(&long), long);
        assert_eq!(plain_uncut("a\x1b[31mb\x07"), "ab");
        assert_eq!(cut(&long), "a".repeat(256));
        assert_eq!(cut("short"), "short");
        let text = format!("{}{}", "\x1b[1m".repeat(100), "b".repeat(300));
        assert_eq!(cut(&plain_uncut(&text)), plain(&text));
    }

    #[test]
    fn a_preview_keeps_colours_and_newlines() {
        assert_eq!(
            preview("\x1b[1;31mred\x1b[0m\nnext"),
            "\x1b[1;31mred\x1b[0m\nnext"
        );
        assert_eq!(preview("\x1b[mplain"), "\x1b[mplain");
        assert_eq!(preview("\x1b[38:5:196mx"), "\x1b[38:5:196mx");
        assert_eq!(
            preview("\x1b[38;2;1;2;3mx\x1b[49m"),
            "\x1b[38;2;1;2;3mx\x1b[49m"
        );
    }

    #[test]
    fn a_preview_removes_every_other_sequence_and_control() {
        for (text, left) in [
            ("a\x1b]52;c;SGVsbG8=\x07b", "ab"),
            ("a\x1b]0;title\x07b", "ab"),
            ("a\x1b]8;;http://x\x1b\\b", "ab"),
            ("a\x1b[2Jb", "ab"),
            ("a\x1b[Hb", "ab"),
            ("a\x1b[?25lb", "ab"),
            ("a\x1b[1;31Hb", "ab"),
            ("a\x1b[1;31 mb", "ab"),
            ("a\x1b[<31mb", "ab"),
            ("a\rb", "ab"),
            ("a\tb", "ab"),
            ("a\x07\x7fb", "ab"),
            ("a\x1bPdcs\x1b\\b", "ab"),
            ("a\x1b_apc\x1b\\b", "ab"),
            ("a\x1bcb", "ab"),
        ] {
            assert_eq!(preview(text), left, "{text:?}");
        }
        assert_eq!(preview(&format!("a{}b{}c", ch(0x202e), ch(0x85))), "abc");
        // A preview is not cut at 256.
        assert_eq!(preview(&"a".repeat(1000)).len(), 1000);
    }

    /// A seeded generator, so that a failure can be run again.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Characters that make sequences often, and the ones the cleaner has to take out or count: escapes,
    /// the brackets, digits, separators and terminators, the C1 controls (U+0080 to U+009F), the
    /// characters that reorder text and a few of more than one byte.
    fn alphabet() -> Vec<char> {
        let mut all: Vec<char> = "\u{1b}\u{1b}\u{1b}[[]];:0123456789\u{7}\\mmPXX^_\n\r\t HJ?<abcdefghijklmnopqrstuvwxyzabcdefghijklm"
            .chars()
            .collect();
        all.extend((0x80..=0x9f).map(ch));
        all.extend(
            [
                0x061c, 0x200e, 0x200f, 0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067,
                0x2068, 0x2069,
            ]
            .map(ch),
        );
        all.extend([ch(0xe9), ch(0x4e2d), ch(0x05d0), ch(0x1f600), ch(0x200d)]);
        all
    }

    /// A string of up to about 700 characters from [`alphabet`]: long enough that the cut at 256 is
    /// reached by what is left after the removals.
    fn random_text(rng: &mut Rng, alphabet: &[char]) -> String {
        let len = match rng.next() % 4 {
            0 => (rng.next() % 40) as usize,
            1 => (rng.next() % 300) as usize,
            _ => 300 + (rng.next() % 400) as usize,
        };
        (0..len)
            .map(|_| alphabet[(rng.next() >> 8) as usize % alphabet.len()])
            .collect()
    }

    #[test]
    fn ten_thousand_random_strings_never_leave_a_control_character() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let alphabet = alphabet();
        let mut cut_reached = 0;
        for _ in 0..10_000 {
            let text = random_text(&mut rng, &alphabet);
            let shown = plain(&text);
            assert!(
                shown.chars().all(|c| !c.is_control() && !reorders(c)),
                "{text:?} gave {shown:?}"
            );
            // Exactly the first 256 characters of what is left when the removals are done.
            let whole = plain_uncut(&text);
            assert_eq!(
                shown.chars().count(),
                whole.chars().count().min(256),
                "{text:?}"
            );
            assert!(whole.starts_with(&shown), "{text:?}");
            cut_reached += usize::from(whole.chars().count() > 256);
            let shown = preview(&text);
            assert_only_newlines_and_colours(&text, &shown);
            assert!(
                shown.chars().all(|c| !reorders(c)),
                "{text:?} gave {shown:?}"
            );
        }
        // The cut was really exercised: a good share of the strings were longer than it.
        assert!(
            cut_reached > 500,
            "only {cut_reached} strings reached the cut"
        );
    }

    /// Re-parses `shown`: no control character but `\n`, and every escape is one whole `ESC [ digits ; : m`.
    fn assert_only_newlines_and_colours(text: &str, shown: &str) {
        let chars: Vec<char> = shown.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\x1b' {
                assert_eq!(chars.get(i + 1), Some(&'['), "{text:?} gave {shown:?}");
                let mut j = i + 2;
                while chars
                    .get(j)
                    .is_some_and(|c| c.is_ascii_digit() || *c == ';' || *c == ':')
                {
                    j += 1;
                }
                assert_eq!(chars.get(j), Some(&'m'), "{text:?} gave {shown:?}");
                i = j + 1;
            } else {
                assert!(c == '\n' || !c.is_control(), "{text:?} gave {shown:?}");
                i += 1;
            }
        }
    }

    fn space(id: &str) -> wire::WireSpace {
        wire::WireSpace {
            session_id: id.to_string(),
            session_name: "garden".to_string(),
            ..wire::WireSpace::default()
        }
    }

    fn pane(id: &str, session: &str, window: &str) -> wire::WirePane {
        wire::WirePane {
            pane_id: id.to_string(),
            session_id: session.to_string(),
            window_id: window.to_string(),
            ..wire::WirePane::default()
        }
    }

    fn agent(id: &str, session: &str, window: &str) -> wire::WireAgent {
        wire::WireAgent {
            pane_id: id.to_string(),
            session_id: session.to_string(),
            window_id: window.to_string(),
            ..wire::WireAgent::default()
        }
    }

    #[test]
    fn a_state_loses_the_rows_whose_ids_are_not_tmuxs_and_keeps_the_rest_as_it_was() {
        let long = format!("%{}", "1".repeat(300));
        let widest = format!("%{}", "9".repeat(20));
        let input = wire::State {
            spaces: vec![space("$1"), space("studio:$2"), space("$x"), space("")],
            panes: vec![
                pane("%1", "$1", "@1"),
                pane("%1;rm", "$1", "@1"),
                pane("%2", "1", "@1"),
                pane("%3", "$1", "w1"),
                pane(&long, "$1", "@1"),
                pane(&widest, "$1", "@1"),
            ],
            agents: vec![
                agent("%1", "$1", "@1"),
                agent("%1 ", "$1", "@1"),
                agent("%4", "$1", ""),
                agent("p1", "$1", "@1"),
            ],
            worktrees: vec![wire::WireWorktree {
                workspace_id: "w-not-a-tmux-id".to_string(),
                ..wire::WireWorktree::default()
            }],
            open_worktrees: Vec::new(),
        };
        let out = state(input);
        assert_eq!(
            out.spaces
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["$1"]
        );
        assert_eq!(
            out.panes,
            vec![pane("%1", "$1", "@1"), pane(&widest, "$1", "@1")]
        );
        assert_eq!(out.agents, vec![agent("%1", "$1", "@1")]);
        // A worktree record's id is not a tmux id: it stays.
        assert_eq!(out.worktrees.len(), 1);
    }

    #[test]
    fn an_id_with_the_sigil_of_another_kind_drops_its_row() {
        let mut bad_active_window = space("$6");
        bad_active_window.active_window_id = "%2".to_string();
        let mut bad_active_pane = space("$7");
        bad_active_pane.active_pane_id = Some("@2".to_string());
        let out = state(wire::State {
            // A session row with a pane id as its id, one with a window id, and a good one.
            spaces: vec![
                space("%1"),
                space("@1"),
                space("$1"),
                bad_active_window,
                bad_active_pane,
            ],
            panes: vec![
                pane("@5", "$1", "@1"),
                pane("$5", "$1", "@1"),
                pane("%5", "@1", "@1"),
                pane("%5", "$1", "%1"),
                pane("%5", "$1", "@1"),
            ],
            agents: vec![
                agent("%1", "$1", "$2"),
                agent("@1", "$1", "@1"),
                agent("%1", "%1", "@1"),
                agent("%1", "$1", "@2"),
            ],
            worktrees: Vec::new(),
            open_worktrees: Vec::new(),
        });
        assert_eq!(
            out.spaces
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["$1"]
        );
        assert_eq!(out.panes, vec![pane("%5", "$1", "@1")]);
        assert_eq!(out.agents, vec![agent("%1", "$1", "@2")]);
    }

    #[test]
    fn the_ids_a_row_points_at_are_checked_too_when_there_is_one() {
        let mut ok = space("$1");
        ok.active_window_id = "@2".to_string();
        ok.active_pane_id = Some("%3".to_string());
        let mut bad_window = space("$2");
        bad_window.active_window_id = "@2;x".to_string();
        let mut bad_pane = space("$3");
        bad_pane.active_pane_id = Some("%y".to_string());
        // No active window or pane at all is not a bad id.
        let none = space("$4");
        let out = state(wire::State {
            spaces: vec![ok, bad_window, bad_pane, none],
            ..wire::State::default()
        });
        assert_eq!(
            out.spaces
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["$1", "$4"]
        );
    }
}
