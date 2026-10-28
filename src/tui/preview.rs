//! The highlighted pane's screen, inside the popup.
//!
//! A `display-popup` covers the panes it is describing, so the sidebar alone would leave
//! the user unable to see what an agent is actually doing. The preview stands in for
//! that: `capture-pane -p -e` for the pane's visible screen
//! with its colours, rendered through `ansi-to-tui`, bottom-aligned the way a terminal
//! is — newest output against the bottom edge.

use ansi_to_tui::IntoText;
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthChar;

use crate::tmux::Tmux;

/// Captures `pane`'s visible screen, with escapes, as styled text.
pub fn capture(t: &Tmux, pane: &str) -> anyhow::Result<Text<'static>> {
    Ok(to_text(&t.capture_pane(pane, true)?))
}

/// Parses `capture-pane -e` output into styled text, falling back to the raw characters
/// if the escape sequences cannot be parsed — an unreadable preview is worth more than
/// no preview.
pub fn to_text(raw: &str) -> Text<'static> {
    raw.into_text()
        .unwrap_or_else(|_| Text::from(raw.to_string()))
}

/// [`to_text`] without the fallback: `None` when the escape sequences cannot be parsed, so that the caller
/// chooses what to show instead (the preview of a pane on another machine shows plain lines, never the bytes
/// as they came).
pub fn try_to_text(raw: &str) -> Option<Text<'static>> {
    raw.into_text().ok()
}

/// `text` re-flowed to `width` columns by display width: a line wider than the panel
/// continues on the next row, each span keeping its style, and a double-width character
/// never straddles the edge. Blank and short lines pass through as they are.
pub fn wrap(text: &Text<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::new();
    for line in &text.lines {
        let mut current: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for span in &line.spans {
            let mut buf = String::new();
            for c in span.content.chars() {
                let w = UnicodeWidthChar::width(c).unwrap_or(0);
                if used + w > width && used > 0 {
                    if !buf.is_empty() {
                        current.push(Span::styled(std::mem::take(&mut buf), span.style));
                    }
                    out.push(Line::from(std::mem::take(&mut current)).style(line.style));
                    used = 0;
                }
                buf.push(c);
                used += w;
            }
            if !buf.is_empty() {
                current.push(Span::styled(buf, span.style));
            }
        }
        out.push(Line::from(current).style(line.style));
    }
    out
}

/// `lines` without the blank rows at their end: `capture-pane` pads its output to the
/// pane's height, and a mostly idle pane would otherwise show its prompt at the top of the
/// preview with empty space below it.
///
/// The view runs this once, on the lines the panel will draw — after the wrap, which can
/// split a styled run of trailing spaces into rows that are blank too — and counts the
/// wheel's limit from what is left, so the limit and [`window`] see the same lines and no
/// notch of the wheel moves nothing.
pub fn trim_trailing_blank(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    while lines
        .last()
        .is_some_and(|line| plain(line).trim().is_empty())
    {
        lines.pop();
    }
    lines
}

/// How far up the wheel can go: the rows above a full panel.
pub fn max_offset(lines: usize, height: u16) -> usize {
    lines.saturating_sub(usize::from(height))
}

/// `height` rows of `lines`, ending `from_bottom` rows above the last non-blank line,
/// padded at the top so the content sits on the bottom edge; `from_bottom` is clamped so
/// the window never runs past the top. Trailing blank lines are dropped first
/// ([`trim_trailing_blank`]) — a no-op for the view, which has dropped them already.
pub fn window(lines: Vec<Line<'static>>, height: u16, from_bottom: usize) -> Text<'static> {
    let mut lines = trim_trailing_blank(lines);
    let height = usize::from(height);
    let total = lines.len();
    let from_bottom = from_bottom.min(total.saturating_sub(height));
    let end = total - from_bottom;
    let start = end.saturating_sub(height);
    let mut shown: Vec<Line<'static>> = lines.drain(start..end).collect();
    if shown.len() < height {
        let mut padded = vec![Line::default(); height - shown.len()];
        padded.append(&mut shown);
        shown = padded;
    }
    Text::from(shown)
}

/// The last `height` lines of `text`, on the bottom edge: [`window`] with no offset.
pub fn bottom_aligned(text: &Text<'static>, height: u16) -> Text<'static> {
    window(text.lines.clone(), height, 0)
}

/// One line's characters, without styling.
pub(crate) fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn plain_lines(text: &Text) -> Vec<String> {
        text.lines.iter().map(plain).collect()
    }

    #[test]
    fn a_long_line_continues_on_the_next_row_with_its_style() {
        let text = Text::from(vec![Line::from(vec![
            Span::styled("abcdefgh", Style::default().fg(Color::Red)),
            Span::raw("ij"),
        ])]);
        let wrapped = wrap(&text, 4);
        let plain: Vec<String> = wrapped.iter().map(plain).collect();
        assert_eq!(plain, ["abcd", "efgh", "ij"]);
        assert_eq!(
            wrapped[1].spans[0].style.fg,
            Some(Color::Red),
            "the style survives the break"
        );
    }

    #[test]
    fn a_double_width_character_never_straddles_the_edge() {
        let text = Text::from("ab漢字c");
        let plain: Vec<String> = wrap(&text, 3).iter().map(plain).collect();
        assert_eq!(plain, ["ab", "漢", "字c"]);
    }

    #[test]
    fn short_and_blank_lines_pass_through_untouched() {
        let text = Text::from("one\n\nthree");
        let plain: Vec<String> = wrap(&text, 10).iter().map(plain).collect();
        assert_eq!(plain, ["one", "", "three"]);
    }

    #[test]
    fn the_window_shows_the_tail_then_scrolls_up_and_clamps() {
        let lines: Vec<Line<'static>> = (1..=6).map(|n| Line::from(format!("l{n}"))).collect();
        assert_eq!(
            plain_lines(&window(lines.clone(), 3, 0)),
            ["l4", "l5", "l6"]
        );
        assert_eq!(
            plain_lines(&window(lines.clone(), 3, 2)),
            ["l2", "l3", "l4"]
        );
        assert_eq!(
            plain_lines(&window(lines.clone(), 3, 99)),
            ["l1", "l2", "l3"],
            "never past the top"
        );
        assert_eq!(max_offset(6, 3), 3);
        assert_eq!(max_offset(2, 3), 0);
    }

    /// The limit and the window must count the same lines. A styled run of spaces at the
    /// end of the last line (a background colour keeps it in a `capture-pane -e` capture)
    /// wraps into rows that are blank too; trimmed after the wrap, they go with the
    /// padding, and every offset up to the limit moves the window.
    #[test]
    fn the_trim_after_the_wrap_also_drops_a_blank_tail_the_wrap_split_off() {
        let text = to_text(&format!(
            "one\ntwo\n\u{1b}[44mprompt$ {}\u{1b}[0m\n\n\n",
            " ".repeat(30)
        ));
        let lines = trim_trailing_blank(wrap(&text, 10));
        assert_eq!(
            plain_lines(&Text::from(lines.clone())),
            ["one", "two", "prompt$   "]
        );
        assert_eq!(max_offset(lines.len(), 2), 1);
        assert_eq!(
            plain_lines(&window(lines.clone(), 2, 0)),
            ["two", "prompt$   "]
        );
        assert_eq!(plain_lines(&window(lines, 2, 1)), ["one", "two"]);
    }

    #[test]
    fn trailing_blank_lines_are_dropped_before_the_window_is_taken() {
        let lines = vec![Line::from("a"), Line::from(""), Line::from("   ")];
        assert_eq!(plain_lines(&window(lines, 2, 0)), ["", "a"]);
    }

    #[test]
    fn to_text_keeps_the_characters_and_decodes_sgr_colours() {
        let raw = "plain \u{1b}[31mred\u{1b}[0m end";
        let text = to_text(raw);
        assert_eq!(text.lines.len(), 1);
        let line = &text.lines[0];
        assert_eq!(plain(line), "plain red end");
        let red = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "red")
            .expect("a span for the coloured word");
        assert_eq!(red.style.fg, Some(Color::Red));
    }

    #[test]
    fn to_text_reads_a_multi_line_capture() {
        let text = to_text("one\ntwo\nthree");
        assert_eq!(text.lines.len(), 3);
        assert_eq!(plain(&text.lines[2]), "three");
    }

    #[test]
    fn bottom_aligned_drops_the_blank_padding_and_keeps_the_last_lines() {
        let text = to_text("one\ntwo\nthree\n   \n\n");
        let aligned = bottom_aligned(&text, 2);
        assert_eq!(aligned.lines.len(), 2);
        assert_eq!(plain(&aligned.lines[0]), "two");
        assert_eq!(plain(&aligned.lines[1]), "three");
    }

    #[test]
    fn bottom_aligned_pads_a_short_capture_at_the_top() {
        let aligned = bottom_aligned(&to_text("prompt$"), 4);
        assert_eq!(aligned.lines.len(), 4);
        assert_eq!(plain(&aligned.lines[0]), "");
        assert_eq!(plain(&aligned.lines[2]), "");
        assert_eq!(plain(&aligned.lines[3]), "prompt$");
    }

    #[test]
    fn bottom_aligned_of_an_empty_capture_is_all_blank() {
        let aligned = bottom_aligned(&to_text(""), 3);
        assert_eq!(aligned.lines.len(), 3);
        assert!(aligned.lines.iter().all(|l| plain(l).is_empty()));
    }
}
