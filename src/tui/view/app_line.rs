//! The app's top line: what it is and how many sessions and agents it lists on the left, the clock on
//! the right. A toast takes the left's place. What needs you is on the list's border, and each machine
//! that is away says so on its own header.

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

/// The line itself, `width` columns. The right is dropped when it would leave the left under half
/// the line, and the left is cut with an ellipsis to what is left.
pub(super) fn app_top_line(app: &App, width: u16) -> Line<'static> {
    let theme = &app.theme;
    let fg = Style::default().fg(theme.fg);
    let dim = Style::default().fg(theme.dim);
    let counts = app.bar_counts();

    // The right: the clock alone.
    let right = Line::from(Span::styled(clock(app), dim));
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
        part(counted(counts.sessions, "session"));
        part(counted(counts.agents, "agent"));
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
    fn the_line_says_how_many_sessions_and_agents_it_lists_with_only_the_clock_beside() {
        let cfg = cfg();
        let mut a = app(&cfg);
        a.clock_ms = Some(1_792_432_710_000);
        let line = text(&app_top_line(&a, 150));
        let sessions = a.bar_counts().sessions;
        let agents = a.bar_counts().agents;
        let ms = a.wall_ms();
        let clock = clock_text(ms, crate::session::restore::local_offset(ms / 1000));
        assert!(line.starts_with(" taw app  \u{b7}  "), "{line:?}");
        assert!(line.contains(&format!("{sessions} sessions")), "{line:?}");
        assert!(line.contains(&format!("{agents} agents")), "{line:?}");
        assert_eq!(line.chars().count(), 150, "{line:?}");
        // The left holds the name and the two counts and nothing else: nothing that needs you, no
        // machines, no state counts.
        let left_end =
            line.find(&format!("{agents} agents")).unwrap() + format!("{agents} agents").len();
        assert!(line[left_end..].trim().ends_with(&clock), "{line:?}");
        // The right is the clock alone.
        assert_eq!(line[left_end..].trim(), clock, "{line:?}");
        for word in [
            "need", "machine", "offline", "\u{2502}", "\u{23f5}", "\u{2713}", "\u{23f8}",
        ] {
            assert!(!line.contains(word), "{word:?} in {line:?}");
        }
    }

    /// With machines listed the line still says nothing of them: their count, who is away and their
    /// states are on the list's own headers.
    #[test]
    fn machines_add_nothing_to_the_line() {
        let cfg = cfg();
        let mut a = app(&cfg);
        let before = text(&app_top_line(&a, 150));
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
            MachineHeader {
                name: "far".into(),
                no_keeper: true,
                ..MachineHeader::default()
            },
        ];
        a.set_snapshot(snap);
        let line = text(&app_top_line(&a, 150));
        assert_eq!(line, before, "the same line with and without machines");
        assert!(
            !line.contains("machine") && !line.contains("offline") && !line.contains("pi"),
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
        // The clock is still on the right, beside the toast.
        let clock = lines[0].trim_end().rsplit(' ').next().unwrap();
        assert!(
            clock.len() == 5 && clock.as_bytes()[2] == b':',
            "{:?}",
            lines[0]
        );
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
