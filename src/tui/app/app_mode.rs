//! The app's mode: how it ends, and where the cursor opens.

use crossterm::event::{KeyCode, KeyModifiers};

use super::tests::{app, ch, config, key, search_config};
use super::*;

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

#[test]
fn ctrl_c_leaves_and_the_quit_keys_go_back() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    assert_eq!(a.end, None);
    a.on_key(ctrl('c'));
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Leave));

    for quit_key in [ch('q'), key(KeyCode::Esc)] {
        let mut a = app(&cfg);
        a.enter_app_mode(None, None);
        a.on_key(quit_key);
        assert!(a.quit, "{quit_key:?} did not quit");
        assert_eq!(a.end, Some(AppEnd::Back), "{quit_key:?}");
    }

    // The search preset ends on ctrl-d with nothing typed, and that is going back too.
    let cfg = search_config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ctrl('d'));
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Back));
}

#[test]
fn a_popup_is_a_popup_until_it_is_made_the_app() {
    let cfg = config();
    let mut a = app(&cfg);
    assert_eq!(a.mode, Mode::Popup);
    assert!(!a.is_app());
    a.enter_app_mode(Some("%121".into()), None);
    assert!(a.is_app());
    assert_eq!(
        a.mode,
        Mode::App {
            came_from: Some("%121".into())
        }
    );
}

#[test]
fn the_cursor_opens_on_the_pane_you_came_from_else_the_last_one_else_it_stays() {
    let cfg = config();
    let row_of = |a: &App, pane: &str| {
        a.rows
            .rows
            .iter()
            .position(|row| matches!(row, Row::Pane(p) if a.snap.panes[*p].pane_id == pane))
            .unwrap()
    };

    let mut a = app(&cfg);
    // The sample popup opens on its own pane, %111; the app has no pane of its own.
    a.enter_app_mode(Some("%121".into()), Some("%111"));
    assert_eq!(
        a.sel,
        row_of(&a, "%121"),
        "came from beats the last session"
    );

    let mut a = app(&cfg);
    a.enter_app_mode(None, Some("%121"));
    assert_eq!(a.sel, row_of(&a, "%121"), "the last session's active pane");

    let mut a = app(&cfg);
    a.sel = 5;
    a.enter_app_mode(Some("%nope".into()), Some("%gone"));
    assert_eq!(a.sel, 5, "no drawn row to open on: the cursor stays");
    let mut a = app(&cfg);
    a.sel = 5;
    a.enter_app_mode(None, None);
    assert_eq!(a.sel, 5);

    // A pane that is not drawn is not a row to move to.
    let mut a = app(&cfg);
    let before = a.sel;
    assert!(!a.cursor_to_pane("%nope"));
    assert_eq!(a.sel, before);
}

/// A pane that is gone (the attach failed, or the last pane of its session exited) is not kept as the pane you
/// came from: the cursor stays where it was and Back has nowhere to go in, so `q` ends the app at once.
#[test]
fn a_pane_that_is_gone_is_not_kept_as_the_pane_you_came_from() {
    let cfg = config();
    let mut a = app(&cfg);
    a.sel = 5;
    a.enter_app_mode(Some("%gone".into()), None);
    assert_eq!(a.sel, 5, "the cursor stays");
    assert_eq!(a.mode, Mode::App { came_from: None });
    assert_eq!(a.came_from(), None);
    a.on_key(ch('q'));
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Back));

    // Positive half: a pane that is drawn is kept, and the cursor moves to it.
    let mut a = app(&cfg);
    a.sel = 5;
    a.enter_app_mode(Some("%121".into()), None);
    assert_eq!(a.came_from(), Some("%121"));
    assert_ne!(a.sel, 5);
}

/// A filter typed before going in is gone on the way back, so a pane it would hide is still the one the cursor
/// opens on (and Esc then goes back instead of clearing a filter).
#[test]
fn a_filter_typed_before_going_in_does_not_hide_the_pane_you_come_back_to() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('/'));
    for c in ['w', 'e', 'b'] {
        a.on_key(ch(c));
    }
    let hidden = !a
        .rows
        .rows
        .iter()
        .any(|row| matches!(row, Row::Pane(p) if a.snap.panes[*p].pane_id == "%121"));
    assert!(hidden, "the filter hides the pane (positive half)");
    assert!(a.filter.is_some());

    a.enter_app_mode(Some("%121".into()), None);

    assert!(a.filter.is_none(), "the filter is empty");
    assert_eq!(a.came_from(), Some("%121"));
    let at = a
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(p) if a.snap.panes[*p].pane_id == "%121"));
    assert_eq!(Some(a.sel), at, "the cursor is on the pane you were in");
}

#[test]
fn a_shell_that_is_not_drawn_puts_the_cursor_on_its_sessions_header() {
    // The default depth draws agents only: web's plain shell (`%211`) has no row of its own.
    let cfg = Config::default();
    let mut a = app(&cfg);
    assert!(
        !a.rows
            .rows
            .iter()
            .any(|row| matches!(row, Row::Pane(p) if a.snap.panes[*p].pane_id == "%211"))
    );
    assert!(a.cursor_to_pane("%211"));
    assert!(matches!(a.rows.rows[a.sel], Row::Session(_)));
    assert_eq!(a.row_session_id(&a.rows.rows[a.sel]).as_deref(), Some("$2"));
}

fn upper_q() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT)
}

/// `Q` quits the app at once, whatever the cursor is on; the popup has no such key.
#[test]
fn capital_q_leaves_the_app_and_is_nobodys_key_in_the_popup() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(Some("%121".into()), None);
    a.on_key(upper_q());
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Leave));
    // A plain Q (no shift flag, as some terminals send it) is the same key.
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('Q'));
    assert_eq!(a.end, Some(AppEnd::Leave));

    let mut popup = app(&cfg);
    popup.on_key(upper_q());
    assert!(!popup.quit, "the popup has no Q");
    assert_eq!(popup.end, None);
}

/// In a filter, Q is a letter of the query, in either preset; Ctrl-C still ends the app.
#[test]
fn q_typed_in_a_filter_is_text_and_ctrl_c_from_it_leaves() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('/'));
    a.on_key(upper_q());
    assert!(!a.quit);
    assert_eq!(a.filter.as_ref().map(|f| f.value()), Some("Q"));
    a.on_key(ctrl('c'));
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Leave));

    let cfg = search_config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(upper_q());
    assert!(!a.quit, "in the search preset Q is text");
    assert_eq!(a.filter.as_ref().map(|f| f.value()), Some("Q"));
    a.on_key(ctrl('c'));
    assert_eq!(a.end, Some(AppEnd::Leave));
}

/// Ctrl-C ends the app from an open overlay too, and Q inside an overlay is the overlay's.
#[test]
fn ctrl_c_leaves_from_an_overlay_and_q_belongs_to_it() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('?'));
    assert!(a.overlay.is_open());
    a.on_key(upper_q());
    assert!(!a.quit, "Q does not reach past an open help");
    a.on_key(ctrl('c'));
    assert!(a.quit);
    assert_eq!(a.end, Some(AppEnd::Leave));
}

/// `[keys]` still decides: a rebound quit key goes back, and a key given to another action stays
/// that action's, Q included.
#[test]
fn a_keys_table_still_rebinds_quit_and_keeps_its_own_q() {
    let mut cfg = config();
    cfg.keys.bindings.insert(
        "quit".to_string(),
        crate::config::KeySpec::One("ctrl+x".to_string()),
    );
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('q'));
    assert!(!a.quit, "q is nobody's key once quit moved to ctrl-x");
    a.on_key(ctrl('x'));
    assert_eq!(a.end, Some(AppEnd::Back));

    let mut cfg = config();
    cfg.keys.bindings.insert(
        "help".to_string(),
        crate::config::KeySpec::One("shift+q".to_string()),
    );
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(upper_q());
    assert!(!a.quit, "Q went to the table's action");
    assert!(matches!(a.overlay, Overlay::Help(_)));
}

/// An app the doorway started (`back_to_tmux`) says that its quit keys go back to tmux, in both presets, and not
/// that they quit; any other app keeps saying quit.
#[test]
fn an_app_the_doorway_started_says_q_goes_back_to_tmux() {
    for (cfg, key) in [(config(), "Q"), (search_config(), "Ctrl-C")] {
        let mut a = app(&cfg);
        a.enter_app_mode(None, None);
        let said = |a: &App| {
            a.hint_line()
                .into_iter()
                .find(|(k, _)| k == key)
                .map(|(_, meaning)| meaning)
        };
        assert_eq!(
            said(&a).as_deref(),
            Some("quit"),
            "{key}: an app of its own"
        );
        a.back_to_tmux = true;
        assert_eq!(
            said(&a).as_deref(),
            Some(crate::tui::app::BACK_TO_TMUX),
            "{key}: started by the doorway"
        );
        let text = crate::tui::view::hint_text(&a);
        assert!(text.contains(&format!("{key} back to tmux")), "{text}");
        assert!(!text.contains(&format!("{key} quit")), "{text}");
    }
}

/// The app's hint line says what the three ways out do, in words, and fits two lines of 98
/// columns; the popup's keeps its words.
#[test]
fn the_app_hint_line_says_go_in_quit_and_back() {
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    let text = crate::tui::view::hint_text(&a);
    for words in ["Enter go in", "Q quit", "Esc back"] {
        assert!(text.contains(words), "{words:?} not in {text:?}");
    }
    let drawn = bottom_lines(&mut a, 98);
    for words in ["Enter go in", "Q quit", "Esc back"] {
        assert!(
            drawn.contains(words),
            "{words:?} not drawn at 98 columns: {drawn:?}"
        );
    }
    let popup = crate::tui::view::hint_text(&app(&cfg));
    assert!(popup.contains("Enter go") && !popup.contains("go in") && !popup.contains("Q quit"));

    // The search preset has no Q key: Esc goes back and Ctrl-C quits.
    let cfg = search_config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    let text = crate::tui::view::hint_text(&a);
    assert!(
        text.contains("Enter go in") && text.contains("Esc back"),
        "{text}"
    );
    assert!(
        text.contains("Ctrl-C quit") && !text.contains("Q quit"),
        "{text}"
    );
    let drawn = bottom_lines(&mut a, 98);
    assert!(
        drawn.contains("Esc back") && drawn.contains("Ctrl-C quit"),
        "{drawn:?}"
    );

    // A focused filter's line is the filter's own: Esc clears there.
    let cfg = config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    a.on_key(ch('/'));
    let text = crate::tui::view::hint_text(&a);
    assert!(
        text.contains("Esc clear") && !text.contains("Esc back"),
        "{text}"
    );

    // A filter that holds text without the keyboard (no key does this today: a focused filter stays
    // focused until it is cleared): Esc would clear it before it goes back, and the line says so.
    a.on_key(ch('o'));
    a.filter_focused = false;
    let text = crate::tui::view::hint_text(&a);
    assert!(
        a.filter.is_some() && !a.filter_focused,
        "the filter is held"
    );
    assert!(
        text.contains("Esc clear") && !text.contains("Esc back") && text.contains("Q quit"),
        "{text}"
    );

    // `quit` moved to ctrl-x: Esc is nobody's key, so the line names ctrl-x (and Q still ends the app).
    let mut cfg = config();
    cfg.keys.bindings.insert(
        "quit".to_string(),
        crate::config::KeySpec::One("ctrl+x".to_string()),
    );
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    let text = crate::tui::view::hint_text(&a);
    assert!(
        text.contains("ctrl-x back") && !text.contains("Esc") && text.contains("Q quit"),
        "{text}"
    );

    // `help` took Q: Q opens the help, so the line does not say it quits.
    let mut cfg = config();
    cfg.keys.bindings.insert(
        "help".to_string(),
        crate::config::KeySpec::One("shift+q".to_string()),
    );
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    let text = crate::tui::view::hint_text(&a);
    assert!(
        !text.contains("Q quit") && text.contains("Esc back") && text.contains("Q help"),
        "{text}"
    );
}

/// The last two lines of a frame `width` columns wide: where the hint line is drawn.
fn bottom_lines(a: &mut App, width: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
    terminal.draw(|f| crate::tui::view::draw(f, a)).unwrap();
    let buf = terminal.backend().buffer().clone();
    (28..30)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The search preset's query keeps fzf's word rule after the app comes back from an attach with text in it: Alt-B
/// over `a_b` stops after the `_` (fzf takes it for a separator), where herdr's rule would pass it.
#[test]
fn the_search_query_keeps_fzfs_word_rule_after_a_return_from_an_attach() {
    let cfg = search_config();
    let mut a = app(&cfg);
    a.enter_app_mode(None, None);
    let typed_then_word_back = |a: &mut App| {
        for c in ['a', '_', 'b'] {
            a.on_key(ch(c));
        }
        a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        a.filter.as_ref().expect("the query line").cursor()
    };
    assert_eq!(typed_then_word_back(&mut a), 2, "fzf at the start");

    // Back from an attach with text typed in the query: it is emptied, and must be fzf's still.
    a.enter_app_mode(Some("%121".into()), None);
    assert_eq!(a.filter.as_ref().map(|f| f.value()), Some(""));
    assert_eq!(typed_then_word_back(&mut a), 2, "fzf after the return");
}
