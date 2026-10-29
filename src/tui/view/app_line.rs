//! The app's top line: what it is and how many sessions and agents it lists on the left, the clock on
//! the right. A toast takes the left's place. What needs you is on the list's border, and each machine
//! that is away says so on its own header; only an away machine no header names (a flat list, a
//! filter or a view that hides it) is named here, before the clock; on a narrow line the clock goes
//! first, then the left's counts are cut, and the away words stay.

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

/// The line's own name, the start of its left.
const NAME: &str = " taw app";

/// `1 session`, `2 sessions`.
fn counted(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The line itself, `width` columns. The right is the clock after any away machine words, and the
/// clock goes first when the line is narrow (the right's priorities are below); the left is cut with
/// an ellipsis to what is left.
pub(super) fn app_top_line(app: &App, width: u16) -> Line<'static> {
    let theme = &app.theme;
    let fg = Style::default().fg(theme.fg);
    let dim = Style::default().fg(theme.dim);
    let counts = app.bar_counts();

    // The right, by priority: the away machines that no header of the list names (none in the plain
    // tree, where every away machine has a header) with the clock, when they fit in half the line;
    // else the away words alone when they fit in half; else, with no toast, the away words anyway,
    // up to what the left's name leaves, cut with an ellipsis only when even that is too narrow. With
    // none away it is the clock alone, shown while it fits. The clock goes before the away words,
    // never the other way round, so a machine is named wherever the line has the room.
    let away = app.away_unheaded();
    let toast = app.live_toast();
    let half = width / 2;
    let clock_span = Span::styled(clock(app), dim);
    // The clock alone, or nothing when it would take more than half the line.
    let clock_alone = Line::from(clock_span.clone());
    let clock_alone = if line_width(&clock_alone) + 2 <= half {
        clock_alone
    } else {
        Line::default()
    };
    let right = if away.is_empty() {
        clock_alone
    } else {
        let words_span = Span::styled(words::away_names(&away), Style::default().fg(theme.yellow));
        let words_only = Line::from(words_span.clone());
        let both = Line::from(vec![
            words_span,
            Span::styled("  \u{2502}  ", dim),
            clock_span,
        ]);
        if line_width(&both) + 2 <= half {
            both
        } else if line_width(&words_only) + 2 <= half {
            words_only
        } else if toast.is_none() {
            let most = width.saturating_sub(line_width(&Line::from(NAME)) + 2);
            truncate_line(words_only, most, theme.dim)
        } else {
            clock_alone
        }
    };
    let right_width = line_width(&right);

    // The left: a toast, or the name and the counts.
    let left = if let Some(toast) = toast {
        Line::from(Span::styled(
            format!(" {toast}"),
            Style::default().fg(theme.yellow),
        ))
    } else {
        let mut spans = vec![Span::styled(
            NAME,
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

    let show_right = right_width > 0;
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
    use crate::tui::data::{MachineHeader, Snapshot};
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

    /// The sample world with studio (live, no session) and pi, which is away: with `rows` its stale
    /// rows are web's session, panes and agents; without, it never answered and has none.
    fn away_world(rows: bool) -> Snapshot {
        let mut snap = snapshot();
        if rows {
            snap.spaces[1].machine = Some(1);
            for pane in snap.panes.iter_mut().filter(|p| p.session_id == "$2") {
                pane.machine = Some(1);
            }
            for agent in snap.agents.iter_mut().filter(|a| a.session_id == "$2") {
                agent.machine = Some(1);
            }
        }
        snap.machines = vec![
            MachineHeader {
                name: "studio".into(),
                live: true,
                stated: true,
                ..MachineHeader::default()
            },
            MachineHeader {
                name: "pi".into(),
                live: false,
                words: "not answering, trying again in 30s".into(),
                ..MachineHeader::default()
            },
        ];
        snap
    }

    /// How often the line says `pi not connected`.
    fn named(line: &str) -> usize {
        line.matches("pi not connected").count()
    }

    /// An away machine is named once and never zero times: the plain tree names nothing, since pi's header
    /// says it is away, with its rows or without.
    #[test]
    fn the_plain_tree_names_no_away_machine_on_the_line() {
        let cfg = cfg();
        for rows in [true, false] {
            let mut a = app(&cfg);
            a.set_snapshot(away_world(rows));
            assert!(
                a.rows.rows.contains(&Row::Machine(Some(1))),
                "pi has its header (rows {rows})"
            );
            let line = text(&app_top_line(&a, 150));
            assert!(
                !line.contains("not connected") && !line.contains("pi"),
                "{line:?}"
            );
            assert!(a.bar_counts().away.is_empty());
        }
    }

    /// The flat layout draws no header, so a machine that never answered is named on the line, once.
    #[test]
    fn the_flat_layout_with_a_machine_that_never_answered_names_it_once() {
        let mut cfg = cfg();
        cfg.ui.layout = crate::config::Layout::Flat;
        let mut a = app(&cfg);
        a.set_snapshot(away_world(false));
        assert!(
            !a.rows.rows.iter().any(|r| matches!(r, Row::Machine(_))),
            "a flat list has no header"
        );
        let line = text(&app_top_line(&a, 150));
        assert_eq!(named(&line), 1, "{line:?}");
        assert!(
            !line.contains("studio"),
            "a live machine is not named: {line:?}"
        );
        // Before the clock, which is last.
        let clock = clock_text(
            a.wall_ms(),
            crate::session::restore::local_offset(a.wall_ms() / 1000),
        );
        assert!(line.trim_end().ends_with(&clock), "{line:?}");
        assert!(
            line.find("pi not connected") < line.find(&clock),
            "{line:?}"
        );
        assert_eq!(a.bar_counts().away, ["pi"]);
    }

    /// A filter that leaves pi no row takes its header away, so the line names it.
    #[test]
    fn a_filter_that_hides_the_header_names_the_machine_on_the_line() {
        let mut cfg = cfg();
        cfg.keys.preset = crate::config::KeysPreset::Search;
        let mut a = app(&cfg);
        a.set_snapshot(away_world(false));
        // Positive first: with nothing typed the header is there and the line names nothing.
        assert!(a.rows.rows.contains(&Row::Machine(Some(1))));
        assert_eq!(named(&text(&app_top_line(&a, 150))), 0);
        for c in "zzzz".chars() {
            a.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(a.query().map(|q| q.text), Some("zzzz".to_string()));
        assert!(
            !a.rows.rows.contains(&Row::Machine(Some(1))),
            "the filter took the header"
        );
        let line = text(&app_top_line(&a, 150));
        assert_eq!(named(&line), 1, "{line:?}");
    }

    /// A view that matches none of pi's agents leaves it no session, so the line names it.
    #[test]
    fn a_view_that_empties_the_machine_names_it_on_the_line() {
        // The agents depth draws agent rows alone, so a view can leave a session with none.
        let mut cfg = cfg();
        cfg.ui.depth = Depth::Agents;
        let mut a = app(&cfg);
        a.set_snapshot(away_world(true));
        assert!(a.rows.rows.contains(&Row::Machine(Some(1))));
        assert_eq!(named(&text(&app_top_line(&a, 150))), 0);
        let mut snap = away_world(true);
        snap.view = crate::tui::agent_view::parse(Some(
            r#"{"source":"cli","filter":{"op":"eq","field":"status","value":"blocked"}}"#,
        ))
        .expect("a query taw would install");
        assert!(snap.view.is_some());
        a.set_snapshot(snap);
        assert!(
            !a.rows.rows.contains(&Row::Machine(Some(1))),
            "the view took the header"
        );
        let line = text(&app_top_line(&a, 150));
        assert_eq!(named(&line), 1, "{line:?}");
        assert_eq!(a.bar_counts().away, ["pi"]);
    }

    fn flat_app<'a>(cfg: &'a mut Config, snap: Snapshot) -> App<'a> {
        cfg.ui.layout = crate::config::Layout::Flat;
        let mut a = app(cfg);
        a.set_snapshot(snap);
        a
    }

    /// The sample world with two machines that never answered, long names both.
    fn two_long_away() -> Snapshot {
        let mut snap = snapshot();
        snap.machines = ["mac-studio", "raspberry-pi"]
            .iter()
            .map(|name| MachineHeader {
                name: (*name).into(),
                live: false,
                words: "not answering, trying again in 30s".into(),
                ..MachineHeader::default()
            })
            .collect();
        snap
    }

    fn clock_of(a: &App) -> String {
        clock_text(
            a.wall_ms(),
            crate::session::restore::local_offset(a.wall_ms() / 1000),
        )
    }

    /// A narrow flat line still names the machine that never answered, once: at 60 columns beside the
    /// clock, at 40 the clock has gone and the words stay.
    #[test]
    fn a_narrow_line_drops_the_clock_before_the_away_machines_name() {
        let mut cfg = cfg();
        let a = flat_app(&mut cfg, away_world(false));
        let clock = clock_of(&a);
        let sixty = text(&app_top_line(&a, 60));
        assert_eq!(named(&sixty), 1, "{sixty:?}");
        assert_eq!(sixty.chars().count(), 60, "{sixty:?}");
        assert!(sixty.trim_end().ends_with(&clock), "{sixty:?}");
        assert!(sixty.contains(" taw app"), "{sixty:?}");
        let forty = text(&app_top_line(&a, 40));
        assert_eq!(named(&forty), 1, "{forty:?}");
        assert_eq!(forty.chars().count(), 40, "{forty:?}");
        assert!(!forty.contains(&clock), "no clock at 40: {forty:?}");
        assert!(forty.contains(" taw app"), "{forty:?}");
    }

    /// Two long names do not fit in half of 80 columns, even without the clock: both are still named,
    /// and the clock gives way, not the names.
    #[test]
    fn two_long_away_names_at_eighty_columns_are_both_named() {
        let mut cfg = cfg();
        let a = flat_app(&mut cfg, two_long_away());
        let line = text(&app_top_line(&a, 80));
        assert_eq!(line.chars().count(), 80, "{line:?}");
        assert!(
            line.contains("mac-studio, raspberry-pi not connected"),
            "{line:?}"
        );
        assert!(!line.contains(&clock_of(&a)), "{line:?}");
        assert!(line.starts_with(" taw app"), "{line:?}");
        assert!(line.contains(" agents"), "{line:?}");
    }

    /// One long name at 60 columns: the words and the clock do not fit in half, the words alone do, so
    /// the clock goes and the name stays.
    #[test]
    fn a_long_away_name_at_sixty_columns_keeps_its_name_and_loses_the_clock() {
        let mut cfg = cfg();
        let mut snap = two_long_away();
        snap.machines.truncate(1);
        let a = flat_app(&mut cfg, snap);
        let line = text(&app_top_line(&a, 60));
        assert_eq!(line.chars().count(), 60, "{line:?}");
        assert!(line.contains("mac-studio not connected"), "{line:?}");
        assert!(!line.contains(&clock_of(&a)), "{line:?}");
        // The left's counts are cut to make room, its name is not.
        assert!(line.starts_with(" taw app"), "{line:?}");
    }

    /// Too narrow even for the words, they are cut with an ellipsis and the line keeps its width.
    #[test]
    fn away_words_wider_than_the_line_are_cut_with_an_ellipsis() {
        let mut cfg = cfg();
        let a = flat_app(&mut cfg, two_long_away());
        let line = text(&app_top_line(&a, 40));
        assert_eq!(line.chars().count(), 40, "{line:?}");
        assert!(line.starts_with(" taw app"), "{line:?}");
        assert!(line.contains("mac-studio, raspberry-"), "{line:?}");
        assert!(line.trim_end().ends_with('\u{2026}'), "{line:?}");
    }

    /// The plain tree has a header for every away machine, so the narrow line is as it was: the clock alone.
    #[test]
    fn the_plain_tree_at_thirty_columns_keeps_the_clock_alone() {
        let cfg = cfg();
        let mut a = app(&cfg);
        a.set_snapshot(away_world(false));
        assert!(a.rows.rows.contains(&Row::Machine(Some(1))));
        let line = text(&app_top_line(&a, 30));
        assert_eq!(line.chars().count(), 30, "{line:?}");
        assert!(line.trim_end().ends_with(&clock_of(&a)), "{line:?}");
        assert!(
            !line.contains("not connected") && !line.contains("pi"),
            "{line:?}"
        );
    }

    /// A toast keeps the left whole; the away words sit beside it while they fit in half, and are back
    /// once the toast has gone.
    #[test]
    fn a_toast_stays_whole_beside_the_away_words() {
        let mut cfg = cfg();
        let mut a = flat_app(&mut cfg, away_world(false));
        a.toast("studio is read only");
        let line = text(&app_top_line(&a, 60));
        assert!(line.starts_with(" studio is read only "), "{line:?}");
        assert_eq!(named(&line), 1, "{line:?}");
        assert_eq!(line.chars().count(), 60, "{line:?}");
        // Words that do not fit in half leave the toast the line, with the clock.
        let mut cfg = crate::config::Config::default();
        cfg.ui.depth = Depth::All;
        let mut a = flat_app(&mut cfg, two_long_away());
        a.toast("studio is read only");
        let line = text(&app_top_line(&a, 60));
        assert!(line.starts_with(" studio is read only "), "{line:?}");
        assert!(!line.contains("not connected"), "{line:?}");
        assert!(line.trim_end().ends_with(&clock_of(&a)), "{line:?}");
        // The toast gone, the words are back.
        let mut cfg = crate::config::Config::default();
        cfg.ui.depth = Depth::All;
        let a = flat_app(&mut cfg, two_long_away());
        let line = text(&app_top_line(&a, 60));
        assert!(line.contains("not connected"), "{line:?}");
    }

    /// A machine nobody keeps, or this tmux server's own, is never named as away.
    #[test]
    fn a_machine_nobody_keeps_and_this_server_are_not_named() {
        let mut cfg = cfg();
        cfg.ui.layout = crate::config::Layout::Flat;
        let mut a = app(&cfg);
        let mut snap = away_world(false);
        snap.machines[1].no_keeper = true;
        snap.machines[0].live = false;
        snap.machines[0].this_server = true;
        a.set_snapshot(snap);
        assert!(a.bar_counts().away.is_empty());
        assert!(!text(&app_top_line(&a, 150)).contains("not connected"));
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
