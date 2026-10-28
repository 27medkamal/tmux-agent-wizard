//! The app's top line: what it is, what it is listing and what needs you on the left, the counts,
//! the machines that are away and the clock on the right. A toast takes the left's place.

use super::*;

/// `HH:MM` of `unix_ms` in the zone `offset_s` seconds from UTC.
pub(crate) fn clock_text(unix_ms: u64, offset_s: i64) -> String {
    let seconds = (unix_ms as i64 / 1000 + offset_s).rem_euclid(86_400);
    format!("{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
}

/// The clock of the line: the app's wall clock in this machine's zone.
fn clock(app: &App) -> String {
    let ms = app.wall_ms();
    clock_text(ms, crate::session::restore::local_offset(ms / 1000))
}

/// `1 session`, `2 sessions`.
fn counted(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The names of the machines that are not live, as the right of the line says them: `pi offline`.
fn offline_words(app: &App) -> String {
    let names: Vec<&str> = app
        .snap
        .machines
        .iter()
        .filter(|m| !m.this_server && !m.no_keeper && !m.live)
        .map(|m| m.name.as_str())
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("{} offline", names.join(", "))
    }
}

/// The line itself, `width` columns. The right is dropped when it would leave the left under half
/// the line, and the left is cut with an ellipsis to what is left.
pub(super) fn app_top_line(app: &App, width: u16) -> Line<'static> {
    let theme = &app.theme;
    let fg = Style::default().fg(theme.fg);
    let dim = Style::default().fg(theme.dim);
    let indicators = app.cfg.ui.status_indicators;
    let counts = app.bar_counts();
    let glyph = |state: AgentState| data::state_icon(Some(state), indicators);
    let colour = |state: AgentState| {
        Style::default()
            .fg(theme.state_color(Some(state)))
            .add_modifier(Modifier::BOLD)
    };

    // The right: every state that has an agent in it, who is away, the clock.
    let mut states: Vec<(AgentState, usize)> = Vec::new();
    for state in COUNTS {
        let n = app
            .snap
            .agents
            .iter()
            .filter(|a| !app.snap.greyed(a.machine) && a.state == Some(state))
            .count();
        if n > 0 {
            states.push((state, n));
        }
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    for (state, n) in &states {
        right.push(Span::styled(
            format!("{}{n}", glyph(*state)),
            colour(*state),
        ));
        right.push(Span::styled(" ", dim));
    }
    let offline = offline_words(app);
    if !offline.is_empty() {
        right.push(Span::styled(offline, fg));
    } else {
        right.pop();
    }
    if !right.is_empty() {
        right.push(Span::styled("  \u{2502}  ", dim));
    }
    right.push(Span::styled(clock(app), dim));
    let right = Line::from(right);
    let right_width = line_width(&right);

    // The left: a toast, or the name and the counts.
    let left = if let Some(toast) = app.live_toast() {
        Line::from(Span::styled(
            format!(" {toast}"),
            Style::default().fg(theme.yellow),
        ))
    } else {
        let mut spans = vec![Span::styled(
            " taw app",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )];
        let mut part = |text: String| {
            spans.push(Span::styled(SEPARATOR, dim));
            spans.push(Span::styled(text, fg));
        };
        let machines = machines::machines_part(&app.snap);
        if !machines.is_empty() {
            part(machines);
        }
        part(counted(counts.sessions, "session"));
        part(counted(counts.agents, "agent"));
        let need: usize = counts.needs.iter().map(|&(_, n)| n).sum();
        if need > 0 {
            spans.push(Span::styled(SEPARATOR, dim));
            for (at, (state, n)) in counts.needs.iter().enumerate() {
                if at > 0 {
                    spans.push(Span::styled(" ", dim));
                }
                spans.push(Span::styled(
                    format!("{}{n}", glyph(*state)),
                    colour(*state),
                ));
            }
            let words = if need == 1 { "needs you" } else { "need you" };
            spans.push(Span::styled(format!(" {words}"), fg));
        }
        Line::from(spans)
    };

    let show_right = right_width + 2 <= width / 2;
    let room = if show_right {
        width.saturating_sub(right_width + 1)
    } else {
        width
    };
    let mut spans = truncate_line(left, room, theme.dim).spans;
    if show_right {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let gap = (width as usize).saturating_sub(used + right_width as usize + 1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right.spans);
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Depth};
    use crate::tui::data::MachineHeader;
    use crate::tui::fixtures::{snapshot, worktrees};
    use crate::tui::theme;

    fn app(cfg: &Config) -> App<'_> {
        let mut app = App::new(
            cfg,
            theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
            "settings".to_string(),
            snapshot(),
        );
        app.set_worktrees(worktrees());
        app.enter_app_mode(None, None);
        app
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.ui.depth = Depth::All;
        cfg
    }

    #[test]
    fn the_clock_is_hours_and_minutes_in_the_zone_given() {
        // 2026-10-19 17:58:30 UTC.
        let ms = 1_792_432_710_000;
        assert_eq!(clock_text(ms, 0), "17:58");
        assert_eq!(clock_text(ms, 3600 * 10), "03:58");
        assert_eq!(clock_text(ms, -3600 * 18), "23:58");
        assert_eq!(clock_text(0, 0), "00:00");
    }

    #[test]
    fn the_line_says_what_it_lists_and_what_needs_you_with_the_counts_and_the_clock_beside() {
        let cfg = cfg();
        let mut a = app(&cfg);
        a.clock_ms = Some(1_792_432_710_000);
        let line = text(&app_top_line(&a, 150));
        let sessions = a.bar_counts().sessions;
        let agents = a.bar_counts().agents;
        assert!(line.starts_with(" taw app  \u{b7}  "), "{line:?}");
        assert!(line.contains(&format!("{sessions} sessions")), "{line:?}");
        assert!(line.contains(&format!("{agents} agents")), "{line:?}");
        // Two agents of the sample world need you.
        assert!(line.contains("need you"), "{line:?}");
        // No machines: no machines part and nobody offline.
        assert!(!line.contains("machine"), "{line:?}");
        assert!(!line.contains("offline"), "{line:?}");
        assert_eq!(line.chars().count(), 150, "{line:?}");
        // The clock ends it, in this machine's zone, after the counts of each state that is there.
        let ms = a.wall_ms();
        let clock = clock_text(ms, crate::session::restore::local_offset(ms / 1000));
        assert!(line.ends_with(&format!("\u{2502}  {clock} ")), "{line:?}");
        assert!(line.contains("\u{23f5}3"), "three agents work: {line:?}");
    }

    #[test]
    fn the_machines_part_and_the_offline_names_come_with_machines() {
        let cfg = cfg();
        let mut a = app(&cfg);
        let mut snap = a.snap.clone();
        snap.machines = vec![
            MachineHeader {
                name: "studio".into(),
                live: true,
                ..MachineHeader::default()
            },
            MachineHeader {
                name: "pi".into(),
                live: false,
                ..MachineHeader::default()
            },
        ];
        a.set_snapshot(snap);
        let line = text(&app_top_line(&a, 150));
        assert!(line.contains("3 machines, pi offline"), "{line:?}");
        // And once more on the right, with the counts: the names of who is away.
        assert_eq!(line.matches("pi offline").count(), 2, "{line:?}");
    }

    #[test]
    fn a_machine_no_app_keeps_is_neither_counted_nor_named_offline() {
        let cfg = cfg();
        let mut a = app(&cfg);
        let mut snap = a.snap.clone();
        snap.machines = vec![
            MachineHeader {
                name: "studio".into(),
                live: true,
                ..MachineHeader::default()
            },
            MachineHeader {
                name: "pi".into(),
                no_keeper: true,
                ..MachineHeader::default()
            },
        ];
        a.set_snapshot(snap);
        let line = text(&app_top_line(&a, 150));
        assert!(line.contains("2 machines"), "the positive half: {line:?}");
        assert!(!line.contains("offline"), "{line:?}");
        let mut snap = a.snap.clone();
        snap.machines.remove(0);
        a.set_snapshot(snap);
        let line = text(&app_top_line(&a, 150));
        assert!(
            !line.contains("machine") && !line.contains("offline"),
            "{line:?}"
        );
    }

    #[test]
    fn a_toast_takes_the_left_and_a_narrow_line_drops_the_right() {
        let cfg = cfg();
        let mut a = app(&cfg);
        a.toast("studio is read only");
        let line = text(&app_top_line(&a, 100));
        assert!(line.starts_with(" studio is read only"), "{line:?}");
        assert!(!line.contains("taw app"), "{line:?}");
        assert!(line.ends_with(' '), "{line:?}");
        assert_eq!(line.chars().count(), 100, "{line:?}");

        // Twelve columns: no room for the right beside the left; the left is cut, never wider.
        let narrow = text(&app_top_line(&a, 12));
        assert_eq!(narrow.chars().count(), 12, "{narrow:?}");
        assert!(narrow.ends_with('\u{2026}'), "{narrow:?}");
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// On a narrow terminal, where the hints keep two lines and the rest are dropped, the app's two ways out
    /// (the way back and the quit) stay on the line: the modal line at 57 columns, the search preset's from
    /// 64 to 70.
    #[test]
    fn a_narrow_app_keeps_the_ways_out_on_its_hint_line() {
        use crate::config::KeysPreset;
        let hints = |preset: KeysPreset, width: u16| {
            let mut cfg = cfg();
            cfg.keys.preset = preset;
            let mut a = app(&cfg);
            let lines = screen(&mut a, width, 30);
            // The hint line is the last two rows of the screen.
            lines[lines.len() - 2..].join("\n")
        };
        for width in [57u16, 50] {
            let said = hints(KeysPreset::Modal, width);
            assert!(
                said.contains("go in"),
                "{width} columns: the line is the app's: {said}"
            );
            assert!(
                said.contains("Q quit") && said.contains("Esc back"),
                "{width} columns: {said}"
            );
        }
        for width in [64u16, 67, 70] {
            let said = hints(KeysPreset::Search, width);
            assert!(
                said.contains("go in"),
                "{width} columns: the line is the app's: {said}"
            );
            assert!(
                said.contains("Esc back") && said.contains("Ctrl-C quit"),
                "{width} columns: {said}"
            );
        }
    }

    #[test]
    fn the_app_draws_its_line_above_the_body_and_keeps_its_toast_off_the_hint_line() {
        let cfg = cfg();
        let mut a = app(&cfg);
        a.toast("pi is not connected");
        let lines = screen(&mut a, 150, 30);
        assert!(lines[0].contains("pi is not connected"), "{:?}", lines[0]);
        assert!(lines[0].contains('\u{2502}'), "{:?}", lines[0]);
        let last = lines.last().unwrap();
        assert!(!last.contains("not connected"), "{last:?}");
        assert!(
            last.contains("move"),
            "the hint line is still the keys: {last:?}"
        );

        // The popup has no such line: the first line is the tree's own border.
        let mut popup = App::new(
            &cfg,
            theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
            "settings".to_string(),
            snapshot(),
        );
        popup.toast("a toast");
        let lines = screen(&mut popup, 150, 30);
        assert!(!lines[0].contains("taw app"), "{:?}", lines[0]);
        assert!(lines.last().unwrap().contains("a toast"));
    }
}
