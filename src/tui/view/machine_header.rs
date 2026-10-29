//! A machine's header line in the list: a coloured rule with a dot, the name and the counts on it.
//!
//! ```text
//! ━━ ● this Mac ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ ⏸1 ⏵2 ━━
//! ━━ ● pi  not answering, last seen 2m ago ━━━━━━━━━━━━━━
//! ```
//!
//! The counts show only when two or more machine groups of the drawn list have agents: they say where
//! things are, and with one group or none they would repeat the rows.
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

/// How many of the machine groups in the drawn list have agents: a header whose tail is counts, and
/// under which an agent row is drawn or, at the sessions depth, where the agents are folded into the
/// session rows, a drawn session row has agents (a view or a filter hides agents from the list, and
/// an agent it hides is not there to count).
fn groups_with_agents(app: &App) -> usize {
    let folded = app.depth == Depth::Sessions;
    let mut groups: Vec<Option<usize>> = Vec::new();
    let mut with_agents: Vec<Option<usize>> = Vec::new();
    for row in &app.rows.rows {
        if let Row::Machine(machine) = row {
            groups.push(*machine);
            continue;
        }
        let Some(&machine) = groups.last() else {
            continue;
        };
        let agent_drawn = match row {
            Row::Pane(pane) => app.snap.panes.get(*pane).and_then(|p| p.agent).is_some(),
            Row::Session(space) => folded && !tree::shown_agents(&app.snap, *space).is_empty(),
            _ => false,
        };
        if agent_drawn && !with_agents.contains(&machine) {
            with_agents.push(machine);
        }
    }
    with_agents
        .into_iter()
        .filter(|&machine| {
            matches!(
                machines::header_tail(&app.snap, machine),
                machines::HeaderTail::Counts(counts) if !counts.is_empty()
            )
        })
        .count()
}

fn parts_of(app: &App, machine: Option<usize>) -> Parts {
    let indicators = app.cfg.ui.status_indicators;
    let (dot, dot_colour) = dot_of(app, machine);
    // The counts say where things are, so they show only when agents are in two groups or more;
    // over one group (or none) they would only repeat the rows.
    let counted = groups_with_agents(app) >= 2;
    let (words, counts) = match machines::header_tail(&app.snap, machine) {
        machines::HeaderTail::Counts(counts) => (
            String::new(),
            counts
                .into_iter()
                .filter(|_| counted)
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

/// `machine`'s header, `width` columns wide when given, else the line the panel is measured by:
/// the dot, the name, the counts and the rule at its shortest, with none of the words, so that
/// nothing an away machine says widens the list or moves it as the words change.
///
/// When the words do not fit beside the rule, they give up their clauses from the end, one at a
/// time, and the rule stays: a clause begins at `, ` or `: `, so `not answering, last seen 2m ago,
/// trying again in 30s` becomes `not answering, last seen 2m ago`, then `not answering`. Only when
/// the first clause alone does not fit with the rule does the line give up its parts in this
/// order: the rule at the end, then the rule altogether, then the counts, then the first clause
/// from its end, and last the name, which is never cut below [`NAME_MIN`] columns (the panel cuts
/// what is still too wide).
pub(super) fn machine_line(app: &App, machine: Option<usize>, width: Option<u16>) -> Line<'static> {
    let mut parts = parts_of(app, machine);
    if width.is_none() {
        parts.words.clear();
    }
    compose(&app.theme, &parts, width.map(usize::from))
}

/// `words` whole, then with each clause dropped from the end, down to the first clause alone. A
/// clause begins at `, ` or `: `.
fn clause_prefixes(words: &str) -> Vec<&str> {
    let mut cuts: Vec<usize> = words
        .match_indices([',', ':'])
        .map(|(at, _)| at)
        .filter(|&at| at > 0 && words[at + 1..].starts_with(' '))
        .collect();
    cuts.reverse();
    std::iter::once(words)
        .chain(cuts.into_iter().map(|at| &words[..at]))
        .collect()
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
    let words_width_of = |words: &str| {
        if words.is_empty() {
            0
        } else {
            2 + words.width()
        }
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
    let candidates = clause_prefixes(&parts.words);
    let first_clause = candidates.last().copied().unwrap_or("");
    let lead_width = END_PIECE.chars().count() + 1;
    let has_counts = !parts.counts.is_empty();

    // The rule's two ends and its fill, then the same without the end after the counts; the
    // words are tried whole first and then a clause shorter, so the rule outlives them.
    for words in &candidates {
        let body_width = 2 + name_width + words_width_of(words);
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
            spans.extend(body(&parts.name, words));
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
    }

    // No rule: the name and its words, the counts after them when they fit.
    let width = width.unwrap_or(usize::MAX);
    let body_width = 2 + name_width + words_width_of(first_clause);
    if has_counts && body_width + 2 + counts_width <= width {
        let mut spans = body(&parts.name, first_clause);
        spans.push(Span::styled("  ", plain));
        counts(&mut spans);
        return Line::from(spans);
    }
    if body_width <= width {
        return Line::from(body(&parts.name, first_clause));
    }
    // The words give way before the name does.
    let room = width.saturating_sub(2 + name_width);
    if room > 2 + 1 {
        let words = super::truncate(first_clause, (room - 2) as u16);
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

    /// The glyph-and-count pieces a header carries: any of the five state icons followed by a digit.
    fn has_counts(line: &str) -> bool {
        let indicators = Config::default().ui.status_indicators;
        COUNTS.iter().any(|&state| {
            let icon = data::state_icon(Some(state), indicators);
            line.match_indices(icon)
                .any(|(at, _)| line[at + icon.len()..].starts_with(|c: char| c.is_ascii_digit()))
        })
    }

    /// Rule 3 of the counts' round: the counts say where things are, so they show only when agents are
    /// in two machine groups of the drawn list or more; with one group, or none, no header has them,
    /// and the dot, the name and the words stay.
    #[test]
    fn the_counts_show_only_when_two_machine_groups_have_agents() {
        let cfg = cfg();
        let mut a = app(&cfg);
        let name = this_machine_word();
        // This machine and studio both have agents.
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(has_counts(&line), "two groups: {line:?}");
        }
        // Only this machine's agents are left: studio has sessions and no agent, and nothing shows counts.
        let mut one = a.snap.clone();
        for agent in &mut one.agents {
            agent.machine = None;
        }
        a.set_snapshot(one);
        for (machine, shown) in [(None, name.to_string()), (Some(0), "studio".to_string())] {
            let line = text(&drawn(&a, machine, 80));
            assert!(line.contains(&format!("\u{25cf} {shown}")), "{line:?}");
            assert!(!has_counts(&line), "one group: {line:?}");
        }
        // The words of a machine that is not live stay beside its name.
        let pi = text(&drawn(&a, Some(1), 80));
        assert!(pi.contains(PI_WORDS), "{pi:?}");
        // No agent anywhere: no counts either.
        let mut none = a.snap.clone();
        none.agents.clear();
        a.set_snapshot(none);
        let line = text(&drawn(&a, None, 80));
        assert!(line.contains(&format!("\u{25cf} {name}")), "{line:?}");
        assert!(!has_counts(&line), "no group: {line:?}");
    }

    /// The groups that count are those with agents in the drawn list: a view that hides every agent of one
    /// group leaves one group with agents, and no header shows counts, though the hidden agents are
    /// still in the snapshot.
    #[test]
    fn a_view_that_hides_one_groups_agents_leaves_one_group_with_agents() {
        let cfg = cfg();
        let mut a = app(&cfg);
        // Studio's session is web's (`$2`), with its agents; this machine keeps the rest.
        let mut snap = world();
        for agent in &mut snap.agents {
            agent.machine = if agent.session_id == "$2" {
                Some(0)
            } else {
                None
            };
        }
        for pane in &mut snap.panes {
            pane.machine = if pane.session_id == "$2" {
                Some(0)
            } else {
                None
            };
        }
        for space in &mut snap.spaces {
            space.machine = if space.session_id == "$2" {
                Some(0)
            } else {
                None
            };
        }
        a.set_snapshot(snap.clone());
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(has_counts(&line), "no view, two groups: {line:?}");
        }
        // A view that keeps blocked agents only: the one blocked agent is this machine's, studio's are hidden.
        snap.view = crate::tui::agent_view::parse(Some(
            r#"{"source":"cli","filter":{"op":"eq","field":"status","value":"blocked"}}"#,
        ))
        .expect("a query taw would install");
        assert!(snap.view.is_some());
        a.set_snapshot(snap);
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(
                !has_counts(&line),
                "the view hides studio's agents: {line:?}"
            );
        }
    }

    /// A filter is honoured as a view is: with `zsh` typed at the all depth, two machine groups are
    /// drawn with shell rows only, no agent row is on screen, and no header shows counts.
    #[test]
    fn a_filter_that_leaves_shell_rows_only_leaves_no_group_with_agents() {
        let mut cfg = cfg();
        cfg.keys.preset = crate::config::KeysPreset::Search;
        let mut a = app(&cfg);
        let mut snap = world();
        for agent in &mut snap.agents {
            agent.machine = (agent.session_id == "$2").then_some(0);
        }
        for pane in &mut snap.panes {
            pane.machine = (pane.session_id == "$2").then_some(0);
            if pane.agent.is_some() {
                // Nothing of an agent's pane says `zsh`, so the filter drops its row.
                pane.pane_current_command = "claude".into();
                pane.window_name = "work".into();
            }
        }
        for space in &mut snap.spaces {
            space.machine = (space.session_id == "$2").then_some(0);
        }
        a.set_snapshot(snap);
        // Positive first: unfiltered, two groups have agents and both headers show counts.
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(has_counts(&line), "no filter, two groups: {line:?}");
        }
        for c in "zsh".chars() {
            a.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(a.query().map(|q| q.text), Some("zsh".to_string()));
        let drawn_rows = &a.rows.rows;
        let headers: Vec<_> = drawn_rows
            .iter()
            .filter(|r| matches!(r, Row::Machine(_)))
            .collect();
        assert_eq!(
            headers.len(),
            2,
            "both machine groups drawn: {drawn_rows:?}"
        );
        let panes: Vec<usize> = drawn_rows
            .iter()
            .filter_map(|r| if let Row::Pane(p) = r { Some(*p) } else { None })
            .collect();
        assert!(!panes.is_empty(), "shell rows are drawn: {drawn_rows:?}");
        assert!(
            panes.iter().all(|&p| a.snap.panes[p].agent.is_none()),
            "the filter left no agent row: {drawn_rows:?}"
        );
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(
                !has_counts(&line),
                "the filter left shell rows only: {line:?}"
            );
        }
    }

    /// The sessions depth draws headers and session rows only, the agents folded into them: two
    /// groups whose session rows have agents show counts on both headers.
    #[test]
    fn at_the_sessions_depth_two_groups_with_agents_show_counts() {
        let mut cfg = cfg();
        cfg.ui.depth = Depth::Sessions;
        let a = app(&cfg);
        assert!(
            a.rows.rows.iter().all(|r| !matches!(r, Row::Pane(_))),
            "no pane row at the sessions depth: {:?}",
            a.rows.rows
        );
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(has_counts(&line), "two groups with agents: {line:?}");
        }
    }

    /// At the sessions depth a filter drops whole headers: with only studio's session kept, one group
    /// is drawn and no header shows counts, though this machine's agents are still in the snapshot.
    #[test]
    fn at_the_sessions_depth_a_filter_that_keeps_one_header_shows_no_counts() {
        let mut cfg = cfg();
        cfg.ui.depth = Depth::Sessions;
        cfg.keys.preset = crate::config::KeysPreset::Search;
        let mut a = app(&cfg);
        // Positive first: unfiltered, both headers count.
        for machine in [None, Some(0)] {
            let line = text(&drawn(&a, machine, 80));
            assert!(has_counts(&line), "no filter, two groups: {line:?}");
        }
        // Studio's session is infra (`$3`); no other session name has these letters.
        for c in "infra".chars() {
            a.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(a.query().map(|q| q.text), Some("infra".to_string()));
        let headers: Vec<_> = a
            .rows
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Machine(_)))
            .collect();
        assert_eq!(
            headers,
            [&Row::Machine(Some(0))],
            "only studio's header is drawn: {:?}",
            a.rows.rows
        );
        let line = text(&drawn(&a, Some(0), 80));
        assert!(
            line.contains("\u{25cf} studio") && !has_counts(&line),
            "one group: {line:?}"
        );
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
        // The words are cut before the name is: the first clause alone, from its end, once not
        // even that fits beside a rule.
        let line = text(&drawn(&a, Some(1), 14));
        assert_eq!(line.width(), 14, "{line:?}");
        assert!(line.starts_with("\u{25cf} pi  not ans"), "{line:?}");
        assert!(line.ends_with('\u{2026}'), "{line:?}");
    }

    /// The header of `machine` with its words as they are, and the rule it draws at `width`.
    fn pi_at(a: &App, width: u16) -> String {
        text(&drawn(a, Some(1), width))
    }

    /// The width of the line `pi  <words>` takes beside its rule, the rule at its shortest.
    fn beside_rule(words: &str) -> u16 {
        // The lead `\u{2501}\u{2501} `, the dot and its space, the name, two spaces, the words, a space and
        // the shortest fill.
        (3 + 2 + "pi".width() + 2 + words.width() + 1 + FILL_MIN) as u16
    }

    #[test]
    fn a_header_that_is_too_narrow_for_its_words_drops_their_clauses_from_the_end_and_keeps_its_rule()
     {
        let cfg = cfg();
        let a = app(&cfg);
        let seen = "not answering, last seen 2m ago";
        let first = "not answering";
        // Room for every clause: nothing is dropped.
        let line = pi_at(&a, beside_rule(PI_WORDS));
        assert!(
            line.contains(&format!("pi  {PI_WORDS} \u{2501}")),
            "{line:?}"
        );
        // One short of that: the retry goes, the rest stays with its rule.
        let line = pi_at(&a, beside_rule(PI_WORDS) - 1);
        assert!(line.contains(&format!("pi  {seen} \u{2501}")), "{line:?}");
        assert!(!line.contains("trying"), "{line:?}");
        assert!(line.ends_with('\u{2501}'), "{line:?}");
        // Just enough for it, and one short: last seen goes.
        let line = pi_at(&a, beside_rule(seen));
        assert!(line.contains(&format!("pi  {seen} \u{2501}")), "{line:?}");
        assert_eq!(line.width(), beside_rule(seen) as usize, "{line:?}");
        let line = pi_at(&a, beside_rule(seen) - 1);
        assert!(line.contains(&format!("pi  {first} \u{2501}")), "{line:?}");
        assert!(!line.contains("last seen"), "{line:?}");
        // Just enough for the first clause: it stays, with its rule; one short, today's order goes
        // on with that clause as the words: the rule is gone and the words are cut.
        let line = pi_at(&a, beside_rule(first));
        assert!(line.contains(&format!("pi  {first} \u{2501}")), "{line:?}");
        assert_eq!(line.width(), beside_rule(first) as usize, "{line:?}");
        let line = pi_at(&a, beside_rule(first) - 1);
        assert_eq!(line, format!("\u{25cf} pi  {first}"), "{line:?}");
        let line = pi_at(&a, 14);
        assert_eq!(line, "\u{25cf} pi  not ans\u{2026}", "{line:?}");
    }

    #[test]
    fn a_machine_no_app_keeps_says_not_connected_with_its_rule_where_its_sentence_does_not_fit() {
        let cfg = cfg();
        let a = app(&cfg);
        let sentence = crate::machine::words::NO_KEEPER;
        let whole = (3 + 2 + "far".width() + 2 + sentence.width() + 1 + FILL_MIN) as u16;
        let line = text(&drawn(&a, Some(3), whole));
        assert!(
            line.contains(&format!("far  {sentence} \u{2501}")),
            "{line:?}"
        );
        let line = text(&drawn(&a, Some(3), whole - 1));
        assert!(
            line.contains("\u{25cb} far  not connected \u{2501}"),
            "{line:?}"
        );
        assert!(!line.contains("run taw"), "{line:?}");
        assert!(line.ends_with('\u{2501}'), "{line:?}");
    }

    /// The list sized by its rows alone: no agent has a title, so the rows are short and the
    /// widest line is one a machine's header would make.
    fn short_rows_app(cfg: &Config) -> App<'_> {
        let mut a = app(cfg);
        let mut snap = world();
        for agent in &mut snap.agents {
            agent.pane_title.clear();
        }
        a.set_snapshot(snap);
        a
    }

    fn list_width(a: &App) -> u16 {
        tree_width(a, 150)
    }

    fn with_words(a: &mut App, machine: usize, words: &str) {
        let mut snap = a.snap.clone();
        snap.machines[machine].words = words.to_string();
        a.set_snapshot(snap);
    }

    #[test]
    fn a_long_away_header_does_not_widen_the_list() {
        let cfg = cfg();
        let mut a = short_rows_app(&cfg);
        assert!(
            a.rows
                .rows
                .iter()
                .any(|r| matches!(r, Row::Machine(Some(1)))),
            "pi has a header in the list: {:?}",
            a.rows.rows
        );
        assert!(
            a.rows
                .rows
                .iter()
                .any(|r| matches!(r, Row::Machine(Some(2)))),
            "build-box has a header in the list"
        );
        // Positive first: the words are long, and the rows leave the list room to be told apart.
        let pi_line = text(&machine_line(&a, Some(1), Some(150)));
        assert!(pi_line.contains(PI_WORDS), "{pi_line:?}");
        let none = {
            with_words(&mut a, 1, "");
            list_width(&a)
        };
        assert!(none < TREE_MAX, "the rows are short: {none}");
        with_words(&mut a, 1, PI_WORDS);
        assert_eq!(list_width(&a), none, "pi's words");
        // A machine's name and counts still count: a very long name widens the list.
        let mut snap = a.snap.clone();
        snap.machines[1].name = "a-machine-with-a-very-long-name-indeed".to_string();
        a.set_snapshot(snap);
        assert!(list_width(&a) > none);
    }

    #[test]
    fn the_list_does_not_move_as_an_away_machines_words_change() {
        let cfg = cfg();
        let mut a = short_rows_app(&cfg);
        with_words(&mut a, 1, "");
        let none = list_width(&a);
        assert!(none < TREE_MAX, "the rows are short: {none}");
        with_words(&mut a, 1, PI_WORDS);
        let long = text(&machine_line(&a, Some(1), Some(150)));
        assert!(
            long.contains(PI_WORDS),
            "positive: the words are drawn: {long:?}"
        );
        for words in [
            "connecting\u{2026}",
            "connecting\u{2026}, last seen 2m ago",
            "not answering, last seen 2m ago, trying again in 9s",
            "not answering, last seen 2m ago, trying again in 30s",
            PI_WORDS,
        ] {
            with_words(&mut a, 1, words);
            assert_eq!(list_width(&a), none, "{words:?}");
        }
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
