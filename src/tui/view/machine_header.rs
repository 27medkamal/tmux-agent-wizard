//! A machine's header line in the list: a coloured rule with a dot, the name and the counts on it.
//!
//! ```text
//! ━━ ● this Mac ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ ⏸1 ⏵2 ━━
//! ━━ ● pi  not answering, last seen 2m ago ━━━━━━━━━━━━━━
//! ```
//!
//! The rule is in the theme's muted colour, the dot green for a machine that is connected (and
//! always for this one), yellow while it connects, red when it does not answer, and hollow and
//! muted for a machine of the table that no app keeps. The name is bold in the accent colour, the
//! words of a machine that is not live follow it in the muted colour, and the counts keep the
//! rows' own icons, colours and order. No cell has a background of its own.

use super::*;

/// The rule's character.
const RULE: &str = "\u{2501}";
/// The two rule characters that start the line and, after the counts, end it.
const END_PIECE: &str = "\u{2501}\u{2501}";
/// A name is cut no shorter than this many columns.
const NAME_MIN: usize = 8;
/// The fewest rule characters that make a fill: fewer would not read as a line.
const FILL_MIN: usize = 2;

/// What a header's line is made of, before it is fitted to a width.
struct Parts {
    dot: &'static str,
    dot_colour: Color,
    name: String,
    /// Empty, or what the header says of a machine that is not live.
    words: String,
    counts: Vec<(String, Color)>,
}

/// The dot of `machine`'s header and its colour.
fn dot_of(app: &App, machine: Option<usize>) -> (&'static str, Color) {
    let theme = &app.theme;
    let Some(header) = machine.and_then(|at| app.snap.machines.get(at)) else {
        return (
            "\u{25cf}",
            if machine.is_none() {
                theme.green
            } else {
                theme.red
            },
        );
    };
    if header.no_keeper {
        ("\u{25cb}", theme.dim)
    } else if header.this_server || header.live {
        ("\u{25cf}", theme.green)
    } else if words::is_connecting(&header.words) {
        ("\u{25cf}", theme.yellow)
    } else {
        ("\u{25cf}", theme.red)
    }
}

fn parts_of(app: &App, machine: Option<usize>) -> Parts {
    let indicators = app.cfg.ui.status_indicators;
    let (dot, dot_colour) = dot_of(app, machine);
    let (words, counts) = match machines::header_tail(&app.snap, machine) {
        machines::HeaderTail::Counts(counts) => (
            String::new(),
            counts
                .into_iter()
                .map(|(state, count)| {
                    (
                        format!("{}{count}", data::state_icon(Some(state), indicators)),
                        app.theme.state_color(Some(state)),
                    )
                })
                .collect(),
        ),
        machines::HeaderTail::Words(words) => (words, Vec::new()),
    };
    Parts {
        dot,
        dot_colour,
        name: machines::header_name(&app.snap, machine),
        words,
        counts,
    }
}

/// `machine`'s header, `width` columns wide when given, else with the rule at its shortest (the
/// line the panel is measured by).
///
/// Too narrow for all of it, the line gives up its parts in this order: the rule at the end, then
/// the rule altogether, then the counts, then the words from their end, and last the name, which is
/// never cut below [`NAME_MIN`] columns (the panel cuts what is still too wide).
pub(super) fn machine_line(app: &App, machine: Option<usize>, width: Option<u16>) -> Line<'static> {
    compose(&app.theme, &parts_of(app, machine), width.map(usize::from))
}

fn compose(theme: &Theme, parts: &Parts, width: Option<usize>) -> Line<'static> {
    let rule = Style::default().fg(theme.dim);
    let plain = Style::default();
    let name_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let words_style = Style::default().fg(theme.dim);
    let counts_width = if parts.counts.is_empty() {
        0
    } else {
        parts.counts.iter().map(|(c, _)| c.width()).sum::<usize>() + parts.counts.len() - 1
    };
    let words_width = if parts.words.is_empty() {
        0
    } else {
        2 + parts.words.width()
    };
    // The dot, its space, the name and the words: what every line has.
    let body = |name: &str, words: &str| -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::styled(parts.dot, Style::default().fg(parts.dot_colour)),
            Span::styled(" ", plain),
            Span::styled(name.to_string(), name_style),
        ];
        if !words.is_empty() {
            spans.push(Span::styled(format!("  {words}"), words_style));
        }
        spans
    };
    let counts = |spans: &mut Vec<Span<'static>>| {
        for (at, (text, colour)) in parts.counts.iter().enumerate() {
            if at > 0 {
                spans.push(Span::styled(" ", plain));
            }
            spans.push(Span::styled(text.clone(), Style::default().fg(*colour)));
        }
    };
    let name_width = parts.name.width();
    let body_width = 2 + name_width + words_width;
    let lead_width = END_PIECE.chars().count() + 1;
    let has_counts = !parts.counts.is_empty();

    // The rule's two ends and its fill, then the same without the end after the counts.
    for with_end in [true, false] {
        if !has_counts && !with_end {
            break;
        }
        let right_width = if has_counts {
            1 + counts_width
                + if with_end {
                    1 + END_PIECE.chars().count()
                } else {
                    0
                }
        } else {
            0
        };
        let fixed = lead_width + body_width + 1 + right_width;
        let fill = match width {
            None => FILL_MIN,
            Some(width) if width >= fixed + FILL_MIN => width - fixed,
            Some(_) => continue,
        };
        let mut spans = vec![Span::styled(format!("{END_PIECE} "), rule)];
        spans.extend(body(&parts.name, &parts.words));
        spans.push(Span::styled(format!(" {}", RULE.repeat(fill)), rule));
        if has_counts {
            spans.push(Span::styled(" ", plain));
            counts(&mut spans);
            if with_end {
                spans.push(Span::styled(format!(" {END_PIECE}"), rule));
            }
        }
        return Line::from(spans);
    }

    // No rule: the name and its words, the counts after them when they fit.
    let width = width.unwrap_or(usize::MAX);
    if has_counts && body_width + 2 + counts_width <= width {
        let mut spans = body(&parts.name, &parts.words);
        spans.push(Span::styled("  ", plain));
        counts(&mut spans);
        return Line::from(spans);
    }
    if body_width <= width {
        return Line::from(body(&parts.name, &parts.words));
    }
    // The words give way before the name does.
    let room = width.saturating_sub(2 + name_width);
    if room > 2 + 1 {
        let words = super::truncate(&parts.words, (room - 2) as u16);
        return Line::from(body(&parts.name, &words));
    }
    let name = super::truncate(&parts.name, width.saturating_sub(2).max(NAME_MIN) as u16);
    Line::from(body(&name, ""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Depth};
    use crate::machine::this_machine_word;
    use crate::tui::data::{MachineHeader, Snapshot};
    use crate::tui::fixtures::snapshot;
    use crate::tui::theme;

    const PI_WORDS: &str = "not answering, last seen 2m ago, trying again in 30s";

    /// The sample world with four more machines: studio, live, with a working and a done agent
    /// (so its header has counts); pi, which does not answer; build-box, connecting; and
    /// far, a machine of the table that no app keeps.
    fn world() -> Snapshot {
        let mut snap = snapshot();
        snap.agents[2].machine = Some(0);
        snap.agents[4].machine = Some(0);
        snap.spaces[2].machine = Some(0);
        let header = |name: &str, live: bool, words: &str, no_keeper: bool| MachineHeader {
            name: name.to_string(),
            live,
            words: words.to_string(),
            no_keeper,
            ..MachineHeader::default()
        };
        snap.machines = vec![
            header("studio", true, "", false),
            header("pi", false, PI_WORDS, false),
            header("build-box", false, "connecting", false),
            header("far", false, crate::machine::words::NO_KEEPER, true),
        ];
        snap
    }

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.ui.depth = Depth::All;
        cfg
    }

    fn app(cfg: &Config) -> App<'_> {
        App::new(
            cfg,
            theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
            "settings".to_string(),
            world(),
        )
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The style of the `at`th character of `line`.
    fn style_at(line: &Line, at: usize) -> Style {
        line.spans
            .iter()
            .flat_map(|s| s.content.chars().map(|_| s.style))
            .nth(at)
            .expect("a character there")
    }

    fn rule(n: usize) -> String {
        "\u{2501}".repeat(n)
    }

    /// The header of `machine` drawn in `width` columns.
    fn drawn(app: &App, machine: Option<usize>, width: u16) -> Line<'static> {
        machine_line(app, machine, Some(width))
    }

    #[test]
    fn this_machines_header_is_a_rule_with_a_dot_the_name_and_the_counts_at_80_columns() {
        let cfg = cfg();
        let a = app(&cfg);
        let line = drawn(&a, None, 80);
        let name = this_machine_word();
        // This machine's own agents, blocked, working, idle, in the rows' own icons and order.
        let counts = format!(
            "{}1 {}2 {}2",
            data::state_icon(Some(AgentState::Blocked), cfg.ui.status_indicators),
            data::state_icon(Some(AgentState::Working), cfg.ui.status_indicators),
            data::state_icon(Some(AgentState::Idle), cfg.ui.status_indicators),
        );
        let left = format!("{} \u{25cf} {name}", rule(2));
        let right = format!(" {counts} {}", rule(2));
        let fill = 80 - left.width() - 1 - right.width();
        assert_eq!(text(&line), format!("{left} {}{right}", rule(fill)));
        assert_eq!(text(&line).width(), 80);
    }

    #[test]
    fn the_five_states_at_80_columns_say_the_same_at_the_same_places() {
        let cfg = cfg();
        let a = app(&cfg);
        let on = |machine| text(&drawn(&a, machine, 80));
        for (machine, name, words) in [
            (Some(0), "studio", String::new()),
            (Some(1), "pi", format!("  {PI_WORDS}")),
            (Some(2), "build-box", "  connecting".to_string()),
            (
                Some(3),
                "far",
                format!("  {}", crate::machine::words::NO_KEEPER),
            ),
        ] {
            let text = on(machine);
            assert_eq!(text.width(), 80, "{text:?}");
            assert!(text.starts_with("\u{2501}\u{2501} "), "{text:?}");
            let dot = if machine == Some(3) {
                '\u{25cb}'
            } else {
                '\u{25cf}'
            };
            assert!(
                text.contains(&format!(" {dot} {name}{words} \u{2501}\u{2501}")),
                "{text:?}"
            );
        }
        // A live machine ends on the rule after its counts; one that is not live has none but the fill.
        let studio = on(Some(0));
        assert!(studio.ends_with(&format!(" {}", rule(2))), "{studio:?}");
        assert!(studio.contains(" \u{2713}1 "), "{studio:?}");
        let pi = on(Some(1));
        assert!(pi.ends_with('\u{2501}'), "{pi:?}");
    }

    #[test]
    fn the_dot_is_green_yellow_red_or_hollow_dim_and_the_name_is_bold_accent_on_a_rule_in_dim() {
        let cfg = cfg();
        let a = app(&cfg);
        let t = a.theme;
        for (machine, dot, colour) in [
            (None, '\u{25cf}', t.green),
            (Some(0), '\u{25cf}', t.green),
            (Some(1), '\u{25cf}', t.red),
            (Some(2), '\u{25cf}', t.yellow),
            (Some(3), '\u{25cb}', t.dim),
        ] {
            let line = drawn(&a, machine, 80);
            let chars: Vec<char> = text(&line).chars().collect();
            assert_eq!(chars[3], dot, "{machine:?}");
            assert_eq!(style_at(&line, 3).fg, Some(colour), "{machine:?}");
            // The name, bold in the accent colour; the rule in the muted one; nothing has a background.
            assert_eq!(style_at(&line, 5).fg, Some(t.accent), "{machine:?}");
            assert!(style_at(&line, 5).add_modifier.contains(Modifier::BOLD));
            assert_eq!(style_at(&line, 0).fg, Some(t.dim), "{machine:?}");
            assert_eq!(style_at(&line, 79).fg, Some(t.dim), "{machine:?}");
            for at in 0..80 {
                assert_eq!(style_at(&line, at).bg, None, "{machine:?} column {at}");
            }
        }
        // The words of a machine that is not live are dim, and its counts keep their state colours.
        let pi = drawn(&a, Some(1), 80);
        assert_eq!(style_at(&pi, 5 + "pi".len() + 2).fg, Some(t.dim));
        let studio = drawn(&a, Some(0), 80);
        let at = text(&studio).chars().position(|c| c == '\u{2713}').unwrap();
        assert_eq!(
            style_at(&studio, at).fg,
            Some(t.state_color(Some(AgentState::Done)))
        );
    }

    #[test]
    fn a_narrow_width_drops_the_rule_pieces_then_the_counts_and_never_cuts_the_name_below_8() {
        let cfg = cfg();
        let a = app(&cfg);
        let name = this_machine_word();
        let counts = text(&drawn(&a, None, 80));
        let counts = counts[counts.find(" \u{23f8}").unwrap()..]
            .trim_end_matches('\u{2501}')
            .trim()
            .to_string();
        let full = 3 + 2 + name.width() + 1 + 2 + 1 + counts.width() + 1 + 2;
        // Just enough for the whole line, the rule at its shortest.
        let line = text(&drawn(&a, None, full as u16));
        assert_eq!(line.width(), full, "{line:?}");
        assert!(
            line.ends_with(&format!(" {counts} {}", rule(2))),
            "{line:?}"
        );
        // One short: the rule at the end goes first, the counts stay.
        let line = text(&drawn(&a, None, full as u16 - 1));
        assert_eq!(line.width(), full - 1, "{line:?}");
        assert!(line.ends_with(&counts), "{line:?}");
        assert!(line.starts_with("\u{2501}\u{2501} "), "{line:?}");
        // Short of the rule at all: no rule, the counts after the name.
        let line = text(&drawn(
            &a,
            None,
            (2 + name.width() + 2 + counts.width()) as u16,
        ));
        assert_eq!(line, format!("\u{25cf} {name}  {counts}"));
        // Then the counts go.
        let line = text(&drawn(&a, None, (2 + name.width()) as u16));
        assert_eq!(line, format!("\u{25cf} {name}"));
        // A name is cut to the width but never below 8 columns.
        let line = text(&drawn(&a, Some(2), 9));
        assert_eq!(line, "\u{25cf} build-b\u{2026}", "{line:?}");
        let line = text(&drawn(&a, Some(2), 4));
        assert_eq!(line, "\u{25cf} build-b\u{2026}", "{line:?}");
        // The words are cut before the name is.
        let line = text(&drawn(&a, Some(1), 30));
        assert_eq!(line.width(), 30, "{line:?}");
        assert!(line.starts_with("\u{25cf} pi  not answering"), "{line:?}");
        assert!(line.ends_with('\u{2026}'), "{line:?}");
    }

    #[test]
    fn with_no_width_the_rule_is_at_its_shortest() {
        let cfg = cfg();
        let a = app(&cfg);
        let line = text(&machine_line(&a, Some(0), None));
        assert!(
            line.starts_with(&format!("{} \u{25cf} studio {} ", rule(2), rule(2))),
            "{line:?}"
        );
        assert!(line.ends_with(&format!(" {}", rule(2))), "{line:?}");
    }
}
