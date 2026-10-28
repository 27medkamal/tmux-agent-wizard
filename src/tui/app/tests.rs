//! Every key, the cursor's own rules, the mouse and the filter, against the sample
//! world.
//!
//! The snapshot is [`crate::tui::fixtures`], so the rows these tests index into are:
//!
//! ```text
//!  0  # mobile        6  # web             13 # infra          19 # api
//!  1  claude  1:zsh     7  zsh     1:zsh      14 claude 1:zsh.1   20 zsh   1:zsh
//!  2  claude  2:review  8  claude  2:zsh.1    15 zsh    1:zsh.2   21 # notes
//!  3  nvim    3:nvim    9  gemini  2:zsh.2    16 nvim   2:docs    22 nvim  1:nvim
//!  4  zsh     4:zsh.1   10 codex   3:tests    17 tail   3:logs    23 zsh   2:zsh
//!  5  htop    4:zsh.2   11 node    4:server   18 claude 4:zsh
//!                       12 feat/pdf worktree
//! ```
//!
//! Two agents are in tiers 0–2 and so "need you": web's unseen `codex` (row 10) and
//! infra's blocked `claude` (row 18), in the order they are drawn — the order `Tab` walks.
//!
//! That row table is the `all` depth, which these tests pin explicitly ([`config`] and
//! [`search_config`]): the shipped default is `agents`, and every row index written
//! here would move under it.
//!
//! The popup was opened from mobile's `claude` (`%111`, row 1), which is therefore "your
//! own row": where the cursor opens, and the row `Enter` just closes the popup on.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::*;
use crate::config::{KeySpec, KeysPreset};
use crate::session::worktree::WorktreeRecord;
use crate::store::AgentState;
use crate::tui::data::AgentRow;
use crate::tui::fixtures::{snapshot, space, worktrees};
use crate::tui::overlays::{Answers, Confirm, Item, Prompt, PromptKind, RowAction};
use crate::tui::prefix::parse_tmux_key;
use crate::tui::preview;
use crate::tui::theme;

// ------------------------------------------------------------------ helpers

/// The default config at the `all` depth: the module comment's row table, which every
/// index in this file is read off. The shipped default is `agents`, so a
/// test that wants the shells drawn has to say so.
pub(super) fn config() -> Config {
    let mut cfg = Config::default();
    cfg.ui.depth = Depth::All;
    cfg
}

pub(super) fn search_config() -> Config {
    let mut cfg = config();
    cfg.keys.preset = KeysPreset::Search;
    cfg
}

pub(super) fn app(cfg: &Config) -> App<'_> {
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(cfg, theme, "settings".to_string(), snapshot());
    // The run loop hands the tracked worktrees over on its first refresh, the same way.
    app.set_worktrees(worktrees());
    app
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn click(column: u16, row: u16, button: MouseButton) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(button),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

pub(super) fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(ch(c));
    }
}

/// The pane id the cursor is on.
fn pane_at<'a>(app: &'a App<'a>) -> &'a str {
    match app.current().expect("a row under the cursor") {
        Row::Pane(pane) => &app.snap.panes[*pane].pane_id,
        other => panic!("expected a pane row, got {other:?}"),
    }
}

/// The session name the cursor's header is for.
fn header_at<'a>(app: &'a App<'a>) -> &'a str {
    match app.current().expect("a row under the cursor") {
        Row::Session(space) => &app.snap.spaces[*space].session_name,
        other => panic!("expected a session header, got {other:?}"),
    }
}

fn confirm_message<'a>(app: &'a App<'a>) -> &'a str {
    match &app.overlay {
        Overlay::Confirm(confirm) => &confirm.message,
        other => panic!("expected a confirmation, got {other:?}"),
    }
}

fn prompt<'a>(app: &'a App<'a>) -> &'a Prompt {
    match &app.overlay {
        Overlay::Prompt(prompt) => prompt,
        other => panic!("expected a prompt, got {other:?}"),
    }
}

fn menu_items<'a>(app: &'a App<'a>) -> &'a [Item] {
    match &app.overlay {
        Overlay::Menu(menu) => &menu.items,
        other => panic!("expected a menu, got {other:?}"),
    }
}

/// Puts the cursor on the row whose pane is `pane_id`.
pub(super) fn cursor_to_pane(app: &mut App, pane_id: &str) {
    let at = app
        .rows
        .rows
        .iter()
        .position(|row| match row {
            Row::Pane(pane) => app.snap.panes[*pane].pane_id == pane_id,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no row for pane {pane_id}"));
    app.sel = at;
}

/// Puts the cursor on `session`'s header.
pub(super) fn cursor_to_header(app: &mut App, session: &str) {
    let at = app
        .rows
        .rows
        .iter()
        .position(|row| match row {
            Row::Session(space) => app.snap.spaces[*space].session_name == session,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no header for {session}"));
    app.sel = at;
}

pub(super) fn cursor_to_worktree(app: &mut App) {
    let at = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::ClosedWorktree { .. }))
        .expect("the parked worktree has a row");
    app.sel = at;
}

// ------------------------------------------------------------------- cursor

/// The cursor opens on the pane the popup
/// was opened from, not on the most urgent agent (`Tab` is how you reach that). This is
/// the rest of [`App::home_row`]'s fallback chain — the row itself is asserted by
/// `the_cursor_opens_on_the_pane_the_popup_was_opened_from`.
#[test]
fn the_popup_opens_on_your_own_row_rather_than_on_what_needs_you_most() {
    let cfg = config();
    let app = app(&cfg);
    // infra's blocked agent is still tier 1, and the cursor is still not on it.
    assert_eq!(pane_at(&app), "%111");
    assert_eq!(app.hot_count(), 2);

    // The pane the popup was opened from is not drawn: its session's header instead.
    let mut snap = snapshot();
    snap.own_pane = Some("%999".to_string());
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let app = App::new(&cfg, theme, String::new(), snap);
    assert_eq!(header_at(&app), "mobile");

    // Neither a pane nor a session to fall back on: the first row.
    let mut snap = snapshot();
    snap.own_pane = None;
    snap.current_session = None;
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let app = App::new(&cfg, theme, String::new(), snap);
    assert_eq!(app.own_row(), None);
    assert_eq!(app.home_row(), None);
    assert_eq!(app.sel, 0);
}

/// Your own row is a pane of tmux's, agent or not, so an agentless client
/// still opens on the pane it was opened from rather than at the top of the list.
#[test]
fn with_no_agents_at_all_the_cursor_still_starts_on_your_own_pane() {
    let cfg = config();
    let mut snap = snapshot();
    snap.agents.clear();
    for pane in &mut snap.panes {
        pane.agent = None;
    }
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let app = App::new(&cfg, theme, String::new(), snap);

    assert_eq!(pane_at(&app), "%111");
    assert_eq!(app.sel, 1);
    assert_eq!(app.hot_count(), 0);
}

/// The walk is the order the rows are drawn in, top to bottom, not the attention order
/// (what needs you most first, then the newest transition): read here by pane rather than
/// by row index.
#[test]
fn tab_walks_the_agents_that_need_you_and_wraps() {
    let cfg = config();
    let mut app = app(&cfg);

    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%231", "codex, done and not looked at");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        pane_at(&app),
        "%341",
        "then infra's blocked agent, below it"
    );
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%231", "and round again");
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(pane_at(&app), "%341", "Shift-Tab walks the other way");
}

#[test]
fn with_no_agents_tab_walks_the_session_headers() {
    let cfg = config();
    let mut snap = snapshot();
    snap.agents.clear();
    for pane in &mut snap.panes {
        pane.agent = None;
    }
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);

    app.on_key(key(KeyCode::Tab));
    assert_eq!(header_at(&app), "web");
}

#[test]
fn j_and_k_move_one_row() {
    let cfg = config();
    let mut app = app(&cfg);
    let start = app.sel;

    app.on_key(ch('j'));
    assert_eq!(app.sel, start + 1);
    app.on_key(ch('k'));
    assert_eq!(app.sel, start);
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.sel, start + 1);
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.sel, start);
}

#[test]
fn shift_j_and_shift_k_hop_between_session_headers() {
    let cfg = config();
    let mut app = app(&cfg);
    // The cursor opens on your own row, which is inside mobile.
    app.on_key(ch('J'));
    assert_eq!(header_at(&app), "web");
    app.on_key(ch('K'));
    assert_eq!(header_at(&app), "mobile");
    app.on_key(ch('K'));
    assert_eq!(header_at(&app), "notes", "and wraps to the last");
}

#[test]
fn a_snapshot_refresh_keeps_the_cursor_on_the_same_pane() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%341");

    // mobile gains a window; every row below it shifts down.
    let mut next = snapshot();
    let mut extra = next.panes[0].clone();
    extra.pane_id = "%151".to_string();
    extra.window_id = "@15".to_string();
    extra.window_index = 5;
    next.panes.insert(5, extra);
    app.set_snapshot(next);

    assert_eq!(
        pane_at(&app),
        "%341",
        "the cursor followed the pane, not the index"
    );
}

/// The run loop reads tmux and `worktrees.json` separately and hands both over at once,
/// so one tick rebuilds the tree once instead of twice.
#[test]
fn one_tick_takes_the_snapshot_and_the_worktrees_together() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%341");
    let before = app.rows.rows.len();

    let mut next = snapshot();
    next.spaces.pop();
    let mut fewer = worktrees();
    fewer.pop();
    app.set_data(Some(next), Some(fewer));

    assert!(
        app.rows.rows.len() < before,
        "both halves landed in one rebuild"
    );
    assert_eq!(
        pane_at(&app),
        "%341",
        "and the cursor still followed its pane"
    );
}

/// What the run loop captures for the preview panel is whatever the cursor is on: an
/// agent pane and a plain pane are themselves, a session header stands in for the pane
/// that session has focused, and a parked worktree — which has no session yet, let alone
/// a pane — has nothing to show, so the panel says "no preview" instead.
#[test]
fn the_preview_follows_the_cursor_row() {
    let cfg = config();
    let mut app = app(&cfg);

    cursor_to_pane(&mut app, "%111");
    assert_eq!(app.preview_pane().as_deref(), Some("%111"), "an agent pane");

    cursor_to_pane(&mut app, "%131");
    assert_eq!(app.preview_pane().as_deref(), Some("%131"), "a plain pane");

    cursor_to_header(&mut app, "web");
    assert_eq!(
        app.preview_pane().as_deref(),
        Some("%211"),
        "a header shows the session's own active pane"
    );

    cursor_to_worktree(&mut app);
    assert_eq!(app.preview_pane(), None, "a parked worktree has no pane");
}

/// A tick where both reads failed changes nothing at all: the popup goes on showing the
/// last good answer rather than emptying itself because tmux was busy for 500 ms.
#[test]
fn a_tick_that_read_nothing_leaves_the_rows_and_the_cursor_alone() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%341");
    let rows = app.rows.rows.clone();
    let sel = app.sel;

    app.set_data(None, None);

    assert_eq!(app.rows.rows, rows);
    assert_eq!(app.sel, sel);
}

// ------------------------------------------------------------------- filter

#[test]
fn the_filter_narrows_the_tree_and_settles_on_the_best_match() {
    let cfg = config();
    let mut app = app(&cfg);

    app.on_key(ch('/'));
    assert!(app.typing());
    // "claude" alone also names mobile's and infra's agents; "web" narrows to this
    // session.
    type_str(&mut app, "web claude");

    let texts: Vec<String> = app
        .rows
        .rows
        .iter()
        .map(|row| tree::row_text(&app.snap, &app.worktrees, row, true))
        .collect();
    assert!(
        texts.iter().all(|t| t.starts_with("web")),
        "only web's rows survive: {texts:?}"
    );
    assert_eq!(pane_at(&app), "%221", "the cursor is on web's 2:zsh.1");
}

/// A number typed into the filter is text like any other character, matched against what each
/// row draws, its labels included, so a digit can name a session as well as match a window
/// index. A lone `3` is session 3's number exactly, so it
/// names infra: its header keeps all of infra's rows in view, the claude in window 4 included,
/// and the cursor is on the header; web's codex in window 3 stays too, by its `3:tests`.
#[test]
fn a_lone_digit_in_the_filter_names_its_session() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    app.on_key(ch('3'));
    assert_eq!(app.filter.as_ref().map(|i| i.value()), Some("3"));

    let shown = |id: &str| {
        app.rows
            .rows
            .iter()
            .any(|r| matches!(r, Row::Pane(p) if app.snap.panes[*p].pane_id == id))
    };
    assert!(shown("%231"), "web's codex, by its 3:tests");
    assert!(shown("%341"), "infra's claude, under its session's header");
    assert_eq!(header_at(&app), "infra");
}

#[test]
fn enter_acts_on_the_cursor_row_from_inside_the_filter() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    type_str(&mut app, "web claude");
    assert_eq!(pane_at(&app), "%221");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Jump {
            session: "$2".to_string(),
            window: "@22".to_string(),
            pane: "%221".to_string(),
        }),
        "Enter goes to what the query found, without leaving the filter first"
    );
}

/// `/blocked`: the filter matches the state word a row draws, so in
/// either preset the list is infra's header and its blocked claude, and the cursor is on the
/// claude, the agent that needs you.
#[test]
fn the_filter_finds_the_blocked_agent_by_its_state_word() {
    for cfg in [config(), search_config()] {
        let mut app = app(&cfg);
        if app.preset == KeysPreset::Modal {
            app.on_key(ch('/'));
        }
        type_str(&mut app, "blocked");
        assert_eq!(
            app.rows.rows.len(),
            2,
            "{:?}: {:?}",
            app.preset,
            app.rows.rows
        );
        assert_eq!(app.rows.rows[0], Row::Session(2), "{:?}: infra", app.preset);
        assert_eq!(pane_at(&app), "%341", "{:?}", app.preset);
    }
}

#[test]
fn the_filter_moves_the_cursor_without_ever_losing_a_letter_to_a_command() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));

    // `j` and `k` are move_down/move_up on the tree; in a query they are letters.
    type_str(&mut app, "jk");
    assert_eq!(app.filter.as_ref().map(|i| i.value()), Some("jk"));

    // The arrows, Tab and the ctrl-j/ctrl-k pair still move — ctrl-k moves rather than
    // deleting to the end of the line, which is what the query line is for.
    app.on_key(ctrl('u'));
    type_str(&mut app, "zsh");
    let start = app.sel;
    app.on_key(key(KeyCode::Down));
    assert_ne!(app.sel, start);
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.sel, start);
    app.on_key(ctrl('j'));
    assert_ne!(app.sel, start);
    app.on_key(ctrl('k'));
    assert_eq!(app.sel, start);
    assert_eq!(
        app.filter.as_ref().map(|i| i.value()),
        Some("zsh"),
        "no movement key ate part of the query"
    );
}

/// fzf's own keys move through the matches too: `ctrl-n` down and `ctrl-p` up,
/// beside `ctrl-j` and `ctrl-k`, and none of the four is typed into the query.
#[test]
fn ctrl_n_and_ctrl_p_move_through_the_matches_beside_ctrl_j_and_ctrl_k() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    type_str(&mut app, "zsh");
    let start = app.sel;
    let next = step(start, app.rows.rows.len(), 1);
    assert!(
        app.rows.rows.len() > 1,
        "more than one row matches, so there is somewhere to move"
    );

    app.on_key(ctrl('n'));
    assert_eq!(app.sel, next, "ctrl-n: the next row");
    app.on_key(ctrl('p'));
    assert_eq!(app.sel, start, "ctrl-p: back where it was");
    app.on_key(ctrl('j'));
    assert_eq!(app.sel, next, "ctrl-j still moves down");
    app.on_key(ctrl('k'));
    assert_eq!(app.sel, start, "ctrl-k still moves up");
    assert_eq!(
        app.filter.as_ref().map(|i| i.value()),
        Some("zsh"),
        "no movement key ate part of the query"
    );
}

#[test]
fn rebinding_an_action_away_from_a_key_stops_that_key_inside_the_filter_too() {
    let mut cfg = config();
    // An override replaces the action's keys, so this unbinds Tab entirely. (Not ctrl-n,
    // which the query line keeps for moving down, as fzf does.)
    cfg.keys.bindings.insert(
        "next_attention".to_string(),
        crate::config::KeySpec::One("ctrl+t".to_string()),
    );
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    let start = app.sel;

    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.sel, start, "Tab is nobody's key, so it does nothing");
    assert_eq!(
        app.filter.as_ref().map(|i| i.value()),
        Some(""),
        "and it typed nothing either"
    );

    app.on_key(ctrl('t'));
    assert_eq!(
        pane_at(&app),
        "%231",
        "the key the action moved to walks the list"
    );
}

#[test]
fn a_key_the_keymap_gave_to_a_non_movement_action_does_nothing_inside_the_filter() {
    let mut cfg = config();
    // Both halves are needed: the first action to claim a key keeps it, so `quit` only
    // gets Tab once `next_attention` has let go of it. `[keys]` is applied in the config's
    // own (alphabetical) order, and "next_attention" sorts before "quit".
    cfg.keys.bindings.insert(
        "next_attention".to_string(),
        crate::config::KeySpec::One("ctrl+n".to_string()),
    );
    cfg.keys.bindings.insert(
        "quit".to_string(),
        crate::config::KeySpec::One("tab".to_string()),
    );
    let mut app = app(&cfg);
    assert_eq!(
        app.keymap.lookup(key(KeyCode::Tab)),
        Some(KeyAction::Quit),
        "the override really took Tab"
    );
    app.on_key(ch('/'));
    let start = app.sel;

    app.on_key(key(KeyCode::Tab));

    assert!(
        !app.quit,
        "the filter runs no command that would close the popup"
    );
    assert_eq!(
        app.sel, start,
        "and Tab does not fall back to another meaning"
    );
    assert!(app.filter.is_some());
}

#[test]
fn a_rebound_movement_key_still_moves_while_the_filter_has_focus() {
    let mut cfg = config();
    // A chord, not a letter: a letter is text in a query whatever it is bound to.
    cfg.keys.bindings.insert(
        "next_attention".to_string(),
        crate::config::KeySpec::One("ctrl+g".to_string()),
    );
    let mut app = app(&cfg);
    app.on_key(ch('/'));

    app.on_key(ctrl('g'));

    assert_eq!(
        pane_at(&app),
        "%231",
        "the override walks the attention list"
    );
    assert_eq!(
        app.filter.as_ref().map(|i| i.value()),
        Some(""),
        "and typed nothing"
    );
}

#[test]
fn esc_clears_the_filter_before_it_quits() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    type_str(&mut app, "web");

    app.on_key(key(KeyCode::Esc));
    assert!(app.filter.is_none());
    assert!(!app.quit);

    app.on_key(key(KeyCode::Esc));
    assert!(app.quit);
}

/// docs/keys.md: the cursor stays where you put it, and `Esc` clears. The filter found web's
/// 2:zsh.1 and the cursor followed it there; clearing the query leaves it on that row, not
/// back on the row the popup opened on.
#[test]
fn esc_in_the_filter_leaves_the_cursor_on_the_row_the_filter_found() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(pane_at(&app), "%111", "the popup opens on your own row");
    app.on_key(ch('/'));
    type_str(&mut app, "web claude");
    assert_eq!(pane_at(&app), "%221");

    app.on_key(key(KeyCode::Esc));

    assert!(app.filter.is_none());
    assert_eq!(pane_at(&app), "%221", "the cursor stayed on the found row");
    assert!(
        !app.rows.rows.is_empty() && app.rows.rows.len() > 3,
        "the whole tree is back"
    );
}

/// A query that matches nothing selects no row: `Esc` then leaves the cursor where it was
/// before the filter opened, which is not the row the popup opened on once it has been moved.
#[test]
fn esc_after_a_query_that_found_nothing_leaves_the_cursor_where_it_was_before_the_filter() {
    let cfg = config();
    let mut app = app(&cfg);
    let own = app.sel;
    app.move_by(2);
    let moved = app.sel;
    assert_ne!(moved, own, "the cursor was moved off your own row");
    app.on_key(ch('/'));
    type_str(&mut app, "zzzzzz");
    assert!(app.rows.rows.is_empty(), "nothing matches");

    app.on_key(key(KeyCode::Esc));

    assert!(app.filter.is_none());
    assert_eq!(app.sel, moved);
}

#[test]
fn a_query_matching_nothing_leaves_no_rows_and_no_panic() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    type_str(&mut app, "zzzzzz");

    assert!(app.rows.rows.is_empty());
    assert_eq!(app.sel, 0);
    assert_eq!(app.on_key(key(KeyCode::Enter)), None);
}

// ---------------------------------------------------------------------- go

#[test]
fn enter_on_a_pane_jumps_to_it() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Jump {
            session: "$2".to_string(),
            window: "@22".to_string(),
            pane: "%221".to_string(),
        })
    );
}

#[test]
fn enter_on_a_header_switches_to_the_session() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "api");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::SwitchWorkspace {
            session: "$4".to_string()
        })
    );
}

#[test]
fn enter_on_a_closed_worktree_opens_it() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_worktree(&mut app);

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::OpenWorktree {
            workspace: "w6".to_string()
        })
    );
}

// ------------------------------------------------------------------- close

#[test]
fn x_on_an_agent_pane_confirms_the_kill_pane_it_would_run() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");

    assert_eq!(app.on_key(ch('x')), None, "the confirmation comes first");
    assert!(
        confirm_message(&app).starts_with("kill-pane web:2.1 (claude)?"),
        "{}",
        confirm_message(&app)
    );

    // y runs it.
    assert_eq!(
        app.on_key(ch('y')),
        Some(Action::KillPane {
            pane: "%221".to_string()
        })
    );
    assert!(!app.overlay.is_open());
}

#[test]
fn x_on_the_last_pane_of_a_window_says_the_window_goes_with_it() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%341"); // infra 4:zsh, one pane

    app.on_key(ch('x'));

    assert_eq!(
        confirm_message(&app),
        "kill-pane infra:4 (claude)? It is the window's last pane, so the window closes too."
    );
}

#[test]
fn x_on_a_header_confirms_kill_session() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "api");

    app.on_key(ch('x'));

    assert_eq!(confirm_message(&app), "kill-session api?");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::KillWorkspace {
            session: "$4".to_string()
        })
    );
}

/// The client's own session is `current`, and the one tmux calls its last is `previous` (ids).
fn client_is_on(app: &mut App, current: &str, previous: Option<&str>) {
    let mut snap = app.snap.clone();
    snap.current_session = Some(current.to_string());
    snap.previous_session = previous.map(str::to_string);
    app.set_snapshot(snap);
}

/// A close that takes the session the popup is on says where it leaves you: tmux's last session
/// when that one survives, else the first session that does.
#[test]
fn closing_your_own_session_says_where_you_go() {
    let cfg = config();
    let mut app = app(&cfg);
    // The fixture: on mobile ($1), last session api ($4).
    cursor_to_header(&mut app, "mobile");
    app.on_key(ch('x'));
    assert_eq!(confirm_message(&app), "kill-session mobile? You go to api.");
    app.on_key(key(KeyCode::Esc));

    // The last session is gone or unknown: the first session tmux lists that survives.
    client_is_on(&mut app, "$1", None);
    app.on_key(ch('x'));
    let first = app.session_name_of("$2");
    assert_eq!(
        confirm_message(&app),
        format!("kill-session mobile? You go to {first}.")
    );
    app.on_key(key(KeyCode::Esc));

    // A session you are not on is closed as it always was.
    client_is_on(&mut app, "$4", Some("$1"));
    app.on_key(ch('x'));
    assert_eq!(confirm_message(&app), "kill-session mobile?");
}

/// With nothing else running, closing the session you are on ends tmux, and the question says so.
#[test]
fn closing_your_only_session_says_tmux_ends() {
    let cfg = config();
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    snap.spaces.retain(|s| s.session_id == "$1");
    snap.panes.clear();
    snap.agents.clear();
    snap.previous_session = None;
    app.set_snapshot(snap);
    cursor_to_header(&mut app, "mobile");
    app.on_key(ch('x'));
    assert_eq!(
        confirm_message(&app),
        "kill-session mobile? It is your only session: tmux ends."
    );
}

/// The last pane of your own session closes the session: same words.
#[test]
fn closing_the_last_pane_of_your_own_session_says_where_you_go() {
    let cfg = config();
    let mut app = app(&cfg);
    api_has_one_window(&mut app);
    client_is_on(&mut app, "$4", Some("$1"));
    cursor_to_pane(&mut app, "%411");
    app.on_key(ch('x'));
    assert_eq!(
        confirm_message(&app),
        format!(
            "kill-pane api:1 (zsh)? It is the session's last pane, so the session closes with it. You go to {}.",
            app.session_name_of("$1")
        )
    );
}

/// The two answers of the group question can leave you in different places: the question says both.
#[test]
fn the_group_question_says_where_each_answer_leaves_you() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a"]);
    // On the worktree session ($6): `y` closes it with api, Enter only api.
    client_is_on(&mut app, "$6", Some("$1"));
    cursor_to_header(&mut app, "api");
    app.on_key(ch('x'));
    assert_eq!(
        confirm_message(&app),
        "kill-session api? It has 1 worktree session open (api--fix-a). Close them too? \
         If you close them too, you go to mobile; if not, you stay."
    );
    app.on_key(key(KeyCode::Esc));

    // On api itself both answers take your session, and both go to mobile: said once.
    client_is_on(&mut app, "$4", Some("$1"));
    app.on_key(ch('x'));
    assert_eq!(
        confirm_message(&app),
        "kill-session api? It has 1 worktree session open (api--fix-a). Close them too? You go to mobile."
    );
}

#[test]
fn x_on_a_closed_worktree_removes_it_and_keeps_the_branch() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_worktree(&mut app);

    app.on_key(ch('x'));

    assert_eq!(
        confirm_message(&app),
        "git worktree remove feat/pdf? (the branch is kept)"
    );
    assert_eq!(
        app.on_key(ch('y')),
        Some(Action::RemoveWorktree {
            workspace: "w6".to_string(),
            force: false,
        })
    );
}

// ------------------------------------------- a session with worktree sessions open

/// The sample world with `names` running as worktree sessions cut from `api` (`$4`, workspace
/// `w4`): each one a session of its own in the snapshot and a record in the tracked worktrees,
/// next to the one parked worktree the world already has.
pub(super) fn with_api_worktrees(app: &mut App, names: &[&str]) {
    let mut snap = app.snap.clone();
    let mut records = worktrees();
    for (i, name) in names.iter().enumerate() {
        let n = 6 + i as u32;
        snap.spaces.push(space(
            n,
            &format!("~/code/{name}"),
            &format!("fix/{i}"),
            (0, 0, 0, 0),
        ));
        records.push(WorktreeRecord {
            workspace_id: format!("w{}", 20 + i),
            session_name: (*name).to_string(),
            path: std::path::PathBuf::from(format!("~/code/{name}")),
            branch: format!("fix/{i}"),
            base: None,
            repo_root: std::path::PathBuf::from("~/code/api"),
            source_workspace_id: "w4".to_string(),
            created_unix_ms: i as u64,
            machine: None,
            real_path: None,
            real_repo_root: None,
        });
    }
    app.set_snapshot(snap);
    app.set_worktrees(records);
}

fn confirmation<'a>(app: &'a App<'a>) -> &'a Confirm {
    match &app.overlay {
        Overlay::Confirm(confirm) => confirm,
        other => panic!("expected a confirmation, got {other:?}"),
    }
}

fn kill_session(id: &str) -> Action {
    Action::KillWorkspace {
        session: id.to_string(),
    }
}

/// `x` on a session that has worktree sessions open asks whether to close those too (herdr's
/// group close, #2874), and names them. `y` closes the lot, Enter and `n` only this session,
/// and Esc nothing.
#[test]
fn x_on_a_session_with_worktree_sessions_open_asks_whether_to_close_them_too() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
    cursor_to_header(&mut app, "api");

    assert_eq!(app.on_key(ch('x')), None, "the question comes first");

    let question = confirmation(&app);
    assert_eq!(
        question.message,
        "kill-session api? It has 2 worktree sessions open (api--fix-a, api--fix-b). Close them too?"
    );
    assert_eq!(
        question.action,
        Action::KillGroup {
            session: "$4".to_string()
        }
    );
    assert_eq!(
        question.answers,
        Answers::Choice {
            otherwise: kill_session("$4")
        }
    );
    assert_eq!(
        app.overlay.hints(),
        "y close them too  \u{b7}  n or Enter just this one  \u{b7}  Esc cancel"
    );
}

#[test]
fn each_answer_to_the_group_question_does_what_it_says() {
    let cfg = config();
    let ask = |answer: KeyEvent| {
        let mut app = app(&cfg);
        with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
        cursor_to_header(&mut app, "api");
        app.on_key(ch('x'));
        let ran = app.on_key(answer);
        (ran, app.overlay.is_open(), app.quit)
    };
    let group = Some(Action::KillGroup {
        session: "$4".to_string(),
    });

    assert_eq!(ask(ch('y')), (group.clone(), false, false));
    assert_eq!(ask(ch('Y')), (group, false, false));
    // Enter is the default, and what `n` says: just this one, the way `x` always was.
    assert_eq!(
        ask(key(KeyCode::Enter)),
        (Some(kill_session("$4")), false, false)
    );
    assert_eq!(ask(ch('n')), (Some(kill_session("$4")), false, false));
    // Esc, and `q`, are neither: nothing runs, the popup stays open.
    assert_eq!(ask(key(KeyCode::Esc)), (None, false, false));
    assert_eq!(ask(ch('q')), (None, false, false));
    // Another key is not an answer.
    assert_eq!(ask(ch('j')), (None, true, false));
}

/// A session with nothing running from it, and a worktree session itself, are asked nothing:
/// the confirmation is the ordinary one.
#[test]
fn a_session_with_no_worktree_sessions_open_is_asked_as_before() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a"]);

    // mobile is the session the popup is on, so the question says where you go.
    cursor_to_header(&mut app, "mobile");
    app.on_key(ch('x'));
    assert_eq!(
        confirmation(&app).message,
        "kill-session mobile? You go to api."
    );
    assert_eq!(confirmation(&app).answers, Answers::Plain);
    app.on_key(key(KeyCode::Esc));

    // web has a worktree too, but it is parked: no session of it is running.
    cursor_to_header(&mut app, "web");
    app.on_key(ch('x'));
    assert_eq!(confirmation(&app).message, "kill-session web?");
    app.on_key(key(KeyCode::Esc));

    // And a worktree session has no group, even with the session it was cut from running.
    cursor_to_header(&mut app, "api--fix-a");
    app.on_key(ch('x'));
    assert_eq!(confirmation(&app).message, "kill-session api--fix-a?");
    assert_eq!(confirmation(&app).answers, Answers::Plain);
}

#[test]
fn the_question_counts_what_is_running_and_names_three_at_most() {
    let cfg = config();
    let ask = |names: &[&str]| {
        let mut app = app(&cfg);
        with_api_worktrees(&mut app, names);
        cursor_to_header(&mut app, "api");
        app.on_key(ch('x'));
        confirmation(&app).message.clone()
    };

    assert_eq!(
        ask(&["api--a"]),
        "kill-session api? It has 1 worktree session open (api--a). Close them too?"
    );
    assert_eq!(
        ask(&["api--a", "api--b", "api--c"]),
        "kill-session api? It has 3 worktree sessions open (api--a, api--b, api--c). Close them too?"
    );
    assert_eq!(
        ask(&["api--a", "api--b", "api--c", "api--d", "api--e"]),
        "kill-session api? It has 5 worktree sessions open (api--a, api--b, api--c and 2 more). Close them too?"
    );
}

/// `[ui] confirm_close = false` asks nothing of a close, and this one is a close: the default
/// answer, just this session, runs at once, as it always has.
#[test]
fn with_confirmations_off_x_closes_just_the_session() {
    let mut cfg = config();
    cfg.ui.confirm_close = false;
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
    cursor_to_header(&mut app, "api");

    assert_eq!(app.on_key(ch('x')), Some(kill_session("$4")));
    assert!(!app.overlay.is_open());
}

/// The right-click menu's close is the same close.
#[test]
fn the_menus_close_asks_the_group_question_too() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
    cursor_to_header(&mut app, "api");
    app.open_context_menu(5, 3);

    assert_eq!(app.on_key(ch('x')), None);

    assert_eq!(
        confirmation(&app).action,
        Action::KillGroup {
            session: "$4".to_string()
        }
    );
}

/// The sample world has every session four windows wide; api, which has the one window with
/// the one pane, is given the `windows` tmux would report for it.
pub(super) fn api_has_one_window(app: &mut App) {
    api_has_windows(app, 1);
}

/// api with the `windows` windows tmux would report for a session that wide.
pub(super) fn api_has_windows(app: &mut App, windows: u32) {
    let mut snap = app.snap.clone();
    for space in snap.spaces.iter_mut().filter(|s| s.session_id == "$4") {
        space.windows = windows;
    }
    app.set_snapshot(snap);
}

/// api in a tmux session group of `size` sessions, itself included: the sessions of a group share
/// their windows. `0` is a session in no group, as tmux reports it.
pub(super) fn api_is_in_a_group_of(app: &mut App, size: u32) {
    let mut snap = app.snap.clone();
    for space in snap.spaces.iter_mut().filter(|s| s.session_id == "$4") {
        space.group_size = size;
    }
    app.set_snapshot(snap);
}

fn kill_pane(pane: &str) -> Action {
    Action::KillPane {
        pane: pane.to_string(),
    }
}

/// The last pane of a session's last window takes the session with it, so `x` on it is a
/// session close, and asks what a session close asks: whether the worktree sessions open go
/// too.
#[test]
fn x_on_a_sessions_last_pane_asks_whether_to_close_its_worktree_sessions_too() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
    api_has_one_window(&mut app);
    cursor_to_pane(&mut app, "%411");

    assert_eq!(app.on_key(ch('x')), None, "the question comes first");

    let question = confirmation(&app);
    assert_eq!(
        question.message,
        "kill-pane api:1 (zsh)? It is the session's last pane, so the session closes with it. It has 2 worktree sessions open (api--fix-a, api--fix-b). Close them too?"
    );
    assert_eq!(
        question.action,
        Action::KillGroup {
            session: "$4".to_string()
        }
    );
    assert_eq!(
        question.answers,
        Answers::Choice {
            otherwise: kill_pane("%411")
        }
    );
}

/// `y` closes the lot, Enter and `n` just this pane (and with it the session, as they always
/// did), Esc and `q` nothing.
#[test]
fn each_answer_to_the_last_pane_question_does_what_it_says() {
    let cfg = config();
    let ask = |answer: KeyEvent| {
        let mut app = app(&cfg);
        with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
        api_has_one_window(&mut app);
        cursor_to_pane(&mut app, "%411");
        app.on_key(ch('x'));
        let ran = app.on_key(answer);
        (ran, app.overlay.is_open(), app.quit)
    };
    let group = Some(Action::KillGroup {
        session: "$4".to_string(),
    });

    assert_eq!(ask(ch('y')), (group, false, false));
    assert_eq!(
        ask(key(KeyCode::Enter)),
        (Some(kill_pane("%411")), false, false)
    );
    assert_eq!(ask(ch('n')), (Some(kill_pane("%411")), false, false));
    assert_eq!(ask(key(KeyCode::Esc)), (None, false, false));
    assert_eq!(ask(ch('q')), (None, false, false));
}

/// Only a close that ends the session is one: with another window in it, or another pane in
/// the window, `x` on a pane is the pane close it was, and is asked as before.
#[test]
fn a_pane_that_does_not_end_its_session_is_asked_as_before() {
    let cfg = config();

    // api still has four windows in the snapshot.
    let mut four_windows = app(&cfg);
    with_api_worktrees(&mut four_windows, &["api--fix-a"]);
    cursor_to_pane(&mut four_windows, "%411");
    four_windows.on_key(ch('x'));
    assert_eq!(
        confirmation(&four_windows).message,
        "kill-pane api:1 (zsh)? It is the window's last pane, so the window closes too."
    );
    assert_eq!(confirmation(&four_windows).answers, Answers::Plain);

    let mut split = app(&cfg);
    with_api_worktrees(&mut split, &["api--fix-a"]);
    api_has_one_window(&mut split);
    let mut snap = split.snap.clone();
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%411") {
        pane.window_panes = 2;
    }
    split.set_snapshot(snap);
    cursor_to_pane(&mut split, "%411");
    split.on_key(ch('x'));
    assert_eq!(confirmation(&split).message, "kill-pane api:1.1 (zsh)?");
    assert_eq!(confirmation(&split).answers, Answers::Plain);
}

/// With nothing cut from the session the last pane of its last window is still a session close,
/// and the question says what closes: the session goes with the pane, not only the window (the
/// window's sentence was true but not the whole of it). It is the sentence the group question
/// opens with, and it is asked plainly: `y` and Enter run it, `n`, `q` and Esc leave it.
#[test]
fn the_last_pane_of_a_session_with_nothing_cut_from_it_says_the_session_closes_with_it() {
    let cfg = config();
    let asked = || {
        let mut popup = app(&cfg);
        api_has_one_window(&mut popup);
        cursor_to_pane(&mut popup, "%411");
        assert_eq!(popup.on_key(ch('x')), None, "the question comes first");
        popup
    };

    let alone = asked();
    assert_eq!(
        confirmation(&alone).message,
        "kill-pane api:1 (zsh)? It is the session's last pane, so the session closes with it."
    );
    assert_eq!(confirmation(&alone).answers, Answers::Plain);

    for answer in [ch('y'), key(KeyCode::Enter)] {
        let mut popup = asked();
        assert_eq!(popup.on_key(answer), Some(kill_pane("%411")), "{answer:?}");
        assert!(!popup.overlay.is_open(), "{answer:?}");
    }
    for answer in [ch('n'), ch('q'), key(KeyCode::Esc)] {
        let mut popup = asked();
        assert_eq!(popup.on_key(answer), None, "{answer:?}");
        assert!(!popup.overlay.is_open(), "{answer:?}");
        assert!(!popup.quit, "{answer:?}");
    }
}

/// The sessions of a tmux session group (`new-session -t`) share their windows, so the last pane
/// of the last window of one of them closes every one of them, and the question says so.
#[test]
fn the_last_pane_of_a_grouped_session_says_the_sessions_grouped_with_it_close_too() {
    let cfg = config();
    let mut grouped = app(&cfg);
    api_has_one_window(&mut grouped);
    api_is_in_a_group_of(&mut grouped, 3);
    cursor_to_pane(&mut grouped, "%411");

    assert_eq!(grouped.on_key(ch('x')), None, "the question comes first");

    assert_eq!(
        confirmation(&grouped).message,
        "kill-pane api:1 (zsh)? It is the session's last pane, so the session and the sessions grouped with it close."
    );
    assert_eq!(confirmation(&grouped).answers, Answers::Plain);
    assert_eq!(grouped.on_key(ch('y')), Some(kill_pane("%411")));
}

/// With worktree sessions open as well, the group question opens with the same sentence.
#[test]
fn a_grouped_session_with_worktree_sessions_open_opens_its_question_with_the_grouped_sentence() {
    let cfg = config();
    let mut grouped = app(&cfg);
    with_api_worktrees(&mut grouped, &["api--fix-a"]);
    api_has_one_window(&mut grouped);
    api_is_in_a_group_of(&mut grouped, 2);
    cursor_to_pane(&mut grouped, "%411");

    assert_eq!(grouped.on_key(ch('x')), None, "the question comes first");

    assert_eq!(
        confirmation(&grouped).message,
        "kill-pane api:1 (zsh)? It is the session's last pane, so the session and the sessions grouped with it close. It has 1 worktree session open (api--fix-a). Close them too?"
    );
    assert_eq!(
        confirmation(&grouped).answers,
        Answers::Choice {
            otherwise: kill_pane("%411")
        }
    );
}

/// What the sentence says follows how many windows the session has and how many sessions share
/// them: the session's own, with one window and no one grouped with it (a group of one, which
/// tmux still reports as a group, closes only itself), the grouped one with one window and
/// others in the group, and the window's with a second window, grouped or not. Two windows is
/// the edge that a count of one window against two decides.
#[test]
fn the_sentence_follows_the_window_count_and_the_group_size() {
    const SESSION: &str = "It is the session's last pane, so the session closes with it.";
    const GROUPED: &str =
        "It is the session's last pane, so the session and the sessions grouped with it close.";
    const WINDOW: &str = "It is the window's last pane, so the window closes too.";
    let cfg = config();

    for (windows, group, said) in [
        (1, 0, SESSION),
        (1, 1, SESSION),
        (1, 2, GROUPED),
        (1, 3, GROUPED),
        (2, 0, WINDOW),
        (2, 1, WINDOW),
        (2, 2, WINDOW),
        (4, 3, WINDOW),
    ] {
        let mut popup = app(&cfg);
        api_has_windows(&mut popup, windows);
        api_is_in_a_group_of(&mut popup, group);
        cursor_to_pane(&mut popup, "%411");
        popup.on_key(ch('x'));
        assert_eq!(
            confirmation(&popup).message,
            format!("kill-pane api:1 (zsh)? {said}"),
            "{windows} windows, a group of {group}"
        );
    }
}

/// The sentence says what tmux would close and no more: the window, for a last pane with another
/// window beside it in its session, and nothing at all for a pane that has another in its window.
#[test]
fn the_last_pane_sentence_names_what_closes_with_it_and_nothing_more() {
    let cfg = config();

    // A session with a second window: only the window closes.
    let mut two_windows = app(&cfg);
    cursor_to_pane(&mut two_windows, "%341");
    two_windows.on_key(ch('x'));
    assert_eq!(
        confirmation(&two_windows).message,
        "kill-pane infra:4 (claude)? It is the window's last pane, so the window closes too."
    );

    // Exactly two windows, the closest a session comes to its last: the one pane's window goes
    // and the session stays, so it does not say the session goes. A count of one against two
    // windows decides it, which the fixtures' four windows do not reach.
    let mut pair = app(&cfg);
    api_has_windows(&mut pair, 2);
    cursor_to_pane(&mut pair, "%411");
    pair.on_key(ch('x'));
    assert_eq!(
        confirmation(&pair).message,
        "kill-pane api:1 (zsh)? It is the window's last pane, so the window closes too."
    );

    // One window, but with a split: the pane closes and nothing with it.
    let mut split = app(&cfg);
    api_has_one_window(&mut split);
    let mut snap = split.snap.clone();
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%411") {
        pane.window_panes = 2;
    }
    split.set_snapshot(snap);
    cursor_to_pane(&mut split, "%411");
    split.on_key(ch('x'));
    let message = confirmation(&split).message.clone();
    assert_eq!(message, "kill-pane api:1.1 (zsh)?");
    assert!(!message.contains("last pane"), "{message}");
}

/// `[ui] confirm_close = false` asks nothing of a close, and this one is a close: the default
/// answer, just this pane, runs at once, as it always has.
#[test]
fn with_confirmations_off_x_on_a_last_pane_closes_just_the_pane() {
    let mut cfg = config();
    cfg.ui.confirm_close = false;
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a", "api--fix-b"]);
    api_has_one_window(&mut app);
    cursor_to_pane(&mut app, "%411");

    assert_eq!(app.on_key(ch('x')), Some(kill_pane("%411")));
    assert!(!app.overlay.is_open());
}

/// A worktree session's own last pane has no group, even with the session it was cut from
/// running.
#[test]
fn a_worktree_sessions_own_last_pane_is_asked_as_before() {
    let cfg = config();
    let mut app = app(&cfg);
    with_api_worktrees(&mut app, &["api--fix-a"]);
    let mut snap = app.snap.clone();
    for space in snap.spaces.iter_mut() {
        space.windows = 1;
    }
    let worktree = snap
        .spaces
        .iter()
        .position(|s| s.session_name == "api--fix-a");
    snap.panes.push(crate::tui::data::PaneRow {
        pane_id: "%991".to_string(),
        session_id: snap.spaces[worktree.unwrap()].session_id.clone(),
        window_id: "@991".to_string(),
        window_index: 1,
        window_name: "zsh".to_string(),
        window_panes: 1,
        pane_index: 1,
        pane_current_command: "zsh".to_string(),
        pane_active: true,
        alternate_on: false,
        agent: None,
        machine: None,
    });
    app.set_snapshot(snap);
    cursor_to_pane(&mut app, "%991");

    app.on_key(ch('x'));

    assert_eq!(confirmation(&app).answers, Answers::Plain);
    assert!(
        confirmation(&app)
            .message
            .starts_with("kill-pane api--fix-a:1"),
        "{}",
        confirmation(&app).message
    );
    // It is a session close all the same, so it says the session goes with it.
    assert!(
        confirmation(&app)
            .message
            .ends_with("It is the session's last pane, so the session closes with it."),
        "{}",
        confirmation(&app).message
    );
}

/// Closing the group is a close: Enter rests afterwards like after any other.
#[test]
fn a_group_close_makes_enter_rest_like_any_other_close() {
    let cfg = config();
    let mut app = app(&cfg);
    assert!(app.after_action(
        &Action::KillGroup {
            session: "$4".to_string()
        },
        Instant::now()
    ));
}

// ------------------------------------ a removal git refused, and the question it raises

fn forced_removal() -> Action {
    Action::RemoveWorktree {
        workspace: "w6".to_string(),
        force: true,
    }
}

/// What the run loop does with a question an action came back with: it is open, in place of
/// nothing, and is answered like any confirmation.
#[test]
fn a_question_an_action_raised_is_put_to_the_user() {
    let cfg = config();
    let mut app = app(&cfg);
    app.ask(Confirm::strict(
        "git will not remove feat/pdf: it has modified or untracked files. remove anyway? uncommitted changes are lost",
        forced_removal(),
    ));

    assert!(app.overlay.is_open());
    assert_eq!(
        app.overlay.hints(),
        "y confirm  \u{b7}  n,\u{a0}Enter\u{a0}or\u{a0}Esc cancel"
    );
    assert_eq!(app.on_key(ch('y')), Some(forced_removal()));
    assert!(!app.overlay.is_open());
}

/// Nothing but `y` runs a removal that loses work: not Enter, not `n`, not `q` and not Esc.
#[test]
fn a_stray_key_does_not_remove_anyway() {
    let cfg = config();
    for key in [
        key(KeyCode::Enter),
        ch('n'),
        ch('N'),
        ch('q'),
        key(KeyCode::Esc),
        ch(' '),
        ch('j'),
    ] {
        let mut app = app(&cfg);
        app.ask(Confirm::strict("remove anyway?", forced_removal()));

        assert_eq!(app.on_key(key), None, "{key:?}");
    }
}

/// The `y` a typist puts behind the one that confirmed the first removal, in the moment after
/// it ran, is not an answer to the question it raised: nothing is lost to a double tap.
#[test]
fn a_y_in_the_moment_after_the_action_that_asked_is_ignored() {
    let cfg = config();
    let first = Action::RemoveWorktree {
        workspace: "w6".to_string(),
        force: false,
    };
    let mut popup = app(&cfg);
    popup.after_action(&first, Instant::now());
    popup.ask(Confirm::strict("remove anyway?", forced_removal()));

    assert_eq!(popup.on_key(ch('y')), None, "too soon to be an answer");
    assert!(popup.overlay.is_open(), "and the question is still there");
    // The others are answers at once, for they lose nothing.
    assert_eq!(popup.on_key(key(KeyCode::Enter)), None);
    assert!(!popup.overlay.is_open());

    let mut popup = app(&cfg);
    popup.ask(Confirm::strict("remove anyway?", forced_removal()));
    popup.settle_until = Some(Instant::now() - std::time::Duration::from_millis(1));
    assert_eq!(
        popup.on_key(ch('y')),
        Some(forced_removal()),
        "after the moment it is"
    );
}

#[test]
fn confirm_close_off_runs_the_close_straight_away() {
    let mut cfg = config();
    cfg.ui.confirm_close = false;
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");

    assert_eq!(
        app.on_key(ch('x')),
        Some(Action::KillPane {
            pane: "%221".to_string()
        })
    );
    assert!(!app.overlay.is_open());
}

/// A close moves the cursor onto whichever row slid into the closed one's place, and the
/// Enter a typist puts behind the confirmation's `y` — or a second Enter — reaches the tree
/// only once the confirmation has gone. For a moment after the close that Enter goes nowhere
/// and quits nothing, while every other key acts at once; a moment later Enter goes again.
#[test]
fn an_enter_right_after_a_confirmed_kill_neither_goes_nor_closes_the_popup() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "api");
    app.on_key(ch('x'));
    let killed = Action::KillWorkspace {
        session: "$4".to_string(),
    };
    assert_eq!(app.on_key(ch('y')), Some(killed.clone()));

    // The run loop's refresh: api and everything in it are gone, and notes' header has
    // taken its place under the cursor.
    let mut gone = snapshot();
    gone.spaces.retain(|s| s.session_id != "$4");
    gone.panes.retain(|p| p.session_id != "$4");
    gone.agents.retain(|a| a.session_id != "$4");
    app.set_snapshot(gone);
    app.after_action(&killed, Instant::now());
    assert_eq!(header_at(&app), "notes");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        None,
        "the Enter behind the y goes nowhere"
    );
    assert!(!app.quit, "and does not close the popup");

    app.on_key(ch('j'));
    assert_eq!(pane_at(&app), "%511", "j moves at once");
    app.on_key(ch('k'));
    assert_eq!(header_at(&app), "notes", "and k");

    app.after_action(&killed, Instant::now() - SETTLE);
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::SwitchWorkspace {
            session: "$5".to_string()
        }),
        "once the moment has passed, Enter goes to the row under the cursor"
    );
}

/// The same after a parked worktree's `git worktree remove`, which can take seconds on a
/// checkout with a build directory: an Enter pressed again while the confirmation is still
/// on screen is read once the remove has finished.
#[test]
fn an_enter_right_after_a_confirmed_remove_neither_goes_nor_closes_the_popup() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_worktree(&mut app);
    app.on_key(ch('x'));
    let removed = Action::RemoveWorktree {
        workspace: "w6".to_string(),
        force: false,
    };
    assert_eq!(app.on_key(key(KeyCode::Enter)), Some(removed.clone()));

    app.set_worktrees(Vec::new());
    app.after_action(&removed, Instant::now());
    assert_eq!(
        header_at(&app),
        "infra",
        "infra's header took the parked row's place"
    );

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        None,
        "the second Enter goes nowhere"
    );
    assert!(!app.quit, "and does not close the popup");

    app.after_action(&removed, Instant::now() - SETTLE);
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::SwitchWorkspace {
            session: "$3".to_string()
        }),
        "once the moment has passed, Enter goes to the row under the cursor"
    );
}

// ------------------------------------------------------------------ rename

#[test]
fn r_on_an_agent_row_asks_the_window_or_the_agent() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");

    app.on_key(ch('r'));

    let keys: Vec<Option<char>> = menu_items(&app).iter().map(|i| i.key).collect();
    assert_eq!(keys, vec![Some('w'), Some('a')]);

    // `a` opens the agent-label prompt, straight away.
    app.on_key(ch('a'));
    assert_eq!(
        prompt(&app).kind,
        PromptKind::RenameAgent {
            pane: "%221".to_string()
        }
    );
}

#[test]
fn r_on_a_plain_pane_renames_its_window() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%211"); // web 1:zsh, no agent

    app.on_key(ch('r'));

    let prompt = prompt(&app);
    assert_eq!(
        prompt.kind,
        PromptKind::RenameWindow {
            window: "@21".to_string()
        }
    );
    assert_eq!(prompt.title, "rename-window web:1");
    assert_eq!(
        prompt.input.value(),
        "zsh",
        "prefilled with the window's name"
    );
}

#[test]
fn r_on_a_header_renames_the_session() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "web");

    app.on_key(ch('r'));

    assert_eq!(
        prompt(&app).kind,
        PromptKind::RenameSession {
            session: "$2".to_string()
        }
    );
    assert_eq!(prompt(&app).input.value(), "web");
}

#[test]
fn r_on_a_closed_worktree_says_its_branch_is_its_name() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_worktree(&mut app);

    app.on_key(ch('r'));

    assert!(!app.overlay.is_open());
    assert_eq!(
        app.live_toast(),
        Some("a closed worktree keeps its branch name")
    );
}

#[test]
fn renaming_a_session_refuses_a_duplicate_and_tmuxs_forbidden_characters() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "web");
    app.on_key(ch('r'));

    for (typed, reason) in [
        ("infra", "duplicate session"),
        ("a.b", "tmux would rewrite"),
        ("", "a name is needed"),
    ] {
        let Overlay::Prompt(p) = &mut app.overlay else {
            panic!("the prompt closed")
        };
        p.input.set(typed);
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(
            prompt(&app)
                .error
                .as_deref()
                .is_some_and(|e| e.contains(reason)),
            "{:?} should mention {reason}",
            prompt(&app).error
        );
    }

    // Its own name is not a duplicate of itself.
    let Overlay::Prompt(p) = &mut app.overlay else {
        panic!("the prompt closed")
    };
    p.input.set("web");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::RenameWorkspace {
            session: "$2".to_string(),
            name: "web".to_string(),
        })
    );
}

#[test]
fn renaming_an_agent_follows_herdrs_rule_and_stays_unique() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");
    app.snap.agents[4].name = Some("reviewer".to_string()); // %231, web's codex
    app.on_key(ch('r'));
    app.on_key(ch('a'));

    for (typed, reason) in [
        ("Reviewer", "lowercase"),
        ("reviewer", "already named"),
        ("", "must not be empty"),
    ] {
        let Overlay::Prompt(p) = &mut app.overlay else {
            panic!("the prompt closed")
        };
        p.input.set(typed);
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert!(
            prompt(&app)
                .error
                .as_deref()
                .is_some_and(|e| e.contains(reason)),
            "{:?} should mention {reason}",
            prompt(&app).error
        );
    }

    let Overlay::Prompt(p) = &mut app.overlay else {
        panic!("the prompt closed")
    };
    p.input.set("planner");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::RenameAgent {
            pane: "%221".to_string(),
            name: "planner".to_string(),
        })
    );
}

/// Opens the agent-rename prompt on `%221`, types `typed` and submits it.
fn submit_agent_rename(app: &mut App, typed: &str) -> Option<Action> {
    cursor_to_pane(app, "%221");
    app.on_key(ch('r'));
    app.on_key(ch('a'));
    let Overlay::Prompt(p) = &mut app.overlay else {
        panic!("the prompt closed")
    };
    p.input.set(typed);
    app.on_key(key(KeyCode::Enter))
}

fn renamed_to(name: &str) -> Option<Action> {
    Some(Action::RenameAgent {
        pane: "%221".to_string(),
        name: name.to_string(),
    })
}

/// The rule for agent names, as `taw agent rename` keeps it: a pane kept after its program ended
/// (`remain-on-exit`) holds no live agent, so the name its record still carries is free.
#[test]
fn renaming_an_agent_may_take_a_name_only_a_dead_pane_records() {
    let cfg = config();
    let mut app = app(&cfg);
    app.snap.agents[4].name = Some("reviewer".to_string()); // %231, web's codex
    app.snap.agents[4].pane_dead = true;

    assert_eq!(
        submit_agent_rename(&mut app, "reviewer"),
        renamed_to("reviewer")
    );
}

/// A pane whose own shell is back at its prompt with nothing running in it holds no live agent
/// either, so its name is free. The look at the shell is the seam: `idle_at` says which row's shell
/// is idle, where the real one asks the process table by pid.
#[test]
fn renaming_an_agent_may_take_a_name_only_a_pane_with_an_idle_shell_records() {
    let cfg = config();
    let mut app = app(&cfg);
    app.snap.agents[4].name = Some("reviewer".to_string()); // %231, web's codex

    assert!(
        app.check_unique_by("%221", "reviewer", |a| a.pane_id == "%231")
            .is_ok()
    );
    // The same name, with that shell busy: the pane holds it.
    assert_eq!(
        app.check_unique_by("%221", "reviewer", |_| false),
        Err("another agent is already named 'reviewer'".to_string())
    );
}

/// A real shell on a terminal of its own (`script`), for the tests that need the process table's
/// own answer. Dropping it ends the shell's process group and the `script` this started, so a
/// failed assertion leaves nothing running, and removes its directory.
struct RealShell {
    script: std::process::Child,
    // Held open so a `read` in the shell waits for input that never comes.
    _stdin: Option<std::process::ChildStdin>,
    pidfile: std::path::PathBuf,
    /// The shell's pid, once it has written it; the shell leads its own session under `script`,
    /// so its process group is the same number.
    pid: Option<u32>,
    _dir: tempfile::TempDir,
}

/// The pid a shell wrote to its pid file, if it has written one.
fn read_pid(pidfile: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

impl Drop for RealShell {
    fn drop(&mut self) {
        // Only while `script` is still running has its child, the shell, not been reaped, so the
        // pid is still ours; once it has exited the number may belong to another process.
        if matches!(self.script.try_wait(), Ok(None))
            && let Some(pid) = self
                .pid
                .or_else(|| read_pid(&self.pidfile))
                .and_then(|pid| libc::pid_t::try_from(pid).ok())
                .filter(|pid| *pid > 1)
        {
            // SAFETY: `kill` takes two integers and touches no memory of this process; a negative
            // pid names the process group of the shell this test started.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = self.script.kill();
        let _ = self.script.wait();
    }
}

/// How long a real shell, or the process table's look at it, may take on a loaded machine.
const REAL_SHELL_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Starts `/bin/sh -c 'echo $$ > pidfile; body'` on a terminal and returns it with the shell's pid,
/// or `None` (say so, and skip) when no terminal could be had: `script` is missing or exits at
/// once. A `script` that stays up while its shell never writes a pid fails the test.
fn real_shell(body: &str) -> Option<(RealShell, u32)> {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let line = format!("echo $$ > '{}'; {body}", pidfile.display());
    let started = crate::watch::tests_support::script_command(&line)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut script = match started {
        Ok(script) => script,
        Err(e) => {
            eprintln!("skipped: no `script` to give a shell a terminal ({e})");
            return None;
        }
    };
    let stdin = script.stdin.take();
    let mut shell = RealShell {
        script,
        _stdin: stdin,
        pidfile: pidfile.clone(),
        pid: None,
        _dir: dir,
    };
    let begun = std::time::Instant::now();
    loop {
        if let Some(pid) = read_pid(&pidfile) {
            shell.pid = Some(pid);
            return Some((shell, pid));
        }
        if let Ok(Some(status)) = shell.script.try_wait() {
            // It may have written its pid just before it ended.
            if let Some(pid) = read_pid(&pidfile) {
                shell.pid = Some(pid);
                return Some((shell, pid));
            }
            eprintln!("skipped: no terminal could be had, `script` exited at once ({status})");
            return None;
        }
        assert!(
            begun.elapsed() < REAL_SHELL_WAIT,
            "the shell on its terminal never wrote its pid within {REAL_SHELL_WAIT:?}, \
             though `script` is still running"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Waits until the process table says the shell `pid` is `idle` (a positive answer, so the check
/// after it is not run against a shell whose state is still unknown), and fails after
/// [`REAL_SHELL_WAIT`].
fn wait_until_shell_is(pid: u32, idle: bool) {
    let begun = std::time::Instant::now();
    while crate::watch::pane_shell_is_idle(pid) != Some(idle) {
        assert!(
            begun.elapsed() < REAL_SHELL_WAIT,
            "shell {pid} never read as idle={idle}: {:?}",
            crate::watch::pane_shell_is_idle(pid)
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The popup's rename check asks the process table about the row's own shell, by the row's pid:
/// a shell at its prompt with nothing running in it frees the name its pane records, and a shell
/// with a command running in front keeps it. A real shell on a terminal of its own, the real idle
/// look and no stand-in, so the one line that joins the popup to a real shell is held.
#[test]
fn renaming_an_agent_reads_the_idle_look_of_the_rows_own_real_shell() {
    let cfg = config();
    // Idle: `read` waits on the terminal's input, which no one writes to.
    if let Some((_shell, pid)) = real_shell("read x") {
        wait_until_shell_is(pid, true);
        let mut app = app(&cfg);
        app.snap.agents[4].name = Some("reviewer".to_string()); // %231, web's codex
        app.snap.agents[4].pane_pid = pid;
        assert_eq!(app.check_unique("%221", "reviewer"), Ok(()));
    }
    // Busy: `; :` keeps the shell from replacing itself with `sleep`, which shares its group.
    if let Some((_shell, pid)) = real_shell("sleep 60; :") {
        wait_until_shell_is(pid, false);
        let mut app = app(&cfg);
        app.snap.agents[4].name = Some("reviewer".to_string());
        app.snap.agents[4].pane_pid = pid;
        assert_eq!(
            app.check_unique("%221", "reviewer"),
            Err("another agent is already named 'reviewer'".to_string())
        );
    }
}

/// A shell that cannot be told (the process table has no answer, `pane_pid` 0 here) is not idle, so
/// the pane keeps the name and the popup refuses it, with the text it always had.
#[test]
fn renaming_an_agent_refuses_a_name_whose_pane_shell_cannot_be_told() {
    let cfg = config();
    let mut app = app(&cfg);
    app.snap.agents[4].name = Some("reviewer".to_string());
    assert_eq!(app.snap.agents[4].pane_pid, 0);

    assert_eq!(submit_agent_rename(&mut app, "reviewer"), None);
    assert_eq!(
        prompt(&app).error.as_deref(),
        Some("another agent is already named 'reviewer'")
    );
}

/// Typing a name no other row carries never reads the process table: the idle look panics if asked,
/// and so does one asked of a row that does not carry the name.
#[test]
fn renaming_an_agent_to_a_free_name_never_asks_the_process_table() {
    let cfg = config();
    let mut app = app(&cfg);
    app.snap.agents[4].name = Some("reviewer".to_string());
    let never = |a: &AgentRow| -> bool { panic!("asked about {}", a.pane_id) };

    assert!(app.check_unique_by("%221", "planner", never).is_ok());
    // A row's own name is no collision, and is not asked about either.
    assert!(app.check_unique_by("%231", "reviewer", never).is_ok());
}

/// Only the rows that carry the name are asked, and the look stops at the first that holds it.
#[test]
fn renaming_an_agent_asks_only_the_rows_that_carry_the_name_and_stops_at_a_holder() {
    let cfg = config();
    let mut app = app(&cfg);
    app.snap.agents[3].name = Some("reviewer".to_string());
    app.snap.agents[4].name = Some("reviewer".to_string());
    let asked = std::cell::RefCell::new(Vec::new());

    let result = app.check_unique_by("%221", "reviewer", |a| {
        asked.borrow_mut().push(a.pane_id.clone());
        false
    });

    assert!(result.is_err());
    assert_eq!(asked.borrow().len(), 1, "asked {:?}", asked.borrow());
    assert_eq!(asked.borrow()[0], app.snap.agents[3].pane_id);
}

// ------------------------------------------------------------- make things

/// Closing a window, splitting and a new window are tmux's own keys, so
/// the popup does not spend `X`, `s`, `v` or `c` on them. `X`, `s` and `v` are
/// nothing at all (no action, no overlay, no toast) on any row.
/// `c` is a letter: the cursor's session's third row,
/// which it moves the cursor to and nothing more.
#[test]
fn the_removed_keys_do_nothing_in_the_modal_preset() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");
    let start = app.sel;
    for c in ['X', 's', 'v'] {
        let action = app.on_key(ch(c));
        assert_eq!(action, None, "{c} is unbound");
        assert!(!app.overlay.is_open(), "{c} opened nothing");
        assert_eq!(app.live_toast(), None, "{c} says nothing either");
        assert!(!app.quit, "{c} does not leave the popup");
        assert_eq!(app.sel, start, "{c} does not move the cursor");
    }
    assert_eq!(app.on_key(ch('c')), None, "c runs nothing for tmux");
    assert!(!app.overlay.is_open(), "c opened nothing");
    assert_eq!(app.live_toast(), None, "c says nothing either");
    assert_eq!(pane_at(&app), "%222", "c is web's third row, its gemini");
    for c in ['X', 's', 'v', 'c'] {
        assert_eq!(app.keymap.lookup(ch(c)), None, "{c} is bound to nothing");
    }
}

/// A `[keys]` table can still name the four actions the popup does not have.
/// Each gets the diagnostic any unknown name gets — on the first frame, as a toast — and
/// the rest of the table still applies.
#[test]
fn a_config_naming_a_removed_action_gets_the_unknown_action_diagnostic() {
    let mut cfg: Config = toml::from_str(
        "[keys]\n\
         close_window = \"X\"\n\
         new_window = \"c\"\n\
         rename = \"R\"\n\
         split_below = \"s\"\n\
         split_right = \"v\"\n",
    )
    .expect("parses");
    cfg.ui.depth = Depth::All;
    let mut app = app(&cfg);

    // `[keys]` is read in table order, which is alphabetical.
    assert_eq!(
        app.key_diagnostics,
        vec![
            "keys: unknown action \"close_window\"".to_string(),
            "keys: unknown action \"new_window\"".to_string(),
            "keys: unknown action \"split_below\"".to_string(),
            "keys: unknown action \"split_right\"".to_string(),
        ]
    );
    app.on_first_frame();
    assert_eq!(
        app.live_toast(),
        Some("keys: unknown action \"close_window\" (+3 more)"),
        "the first frame says so"
    );

    // Otherwise honoured: `R` is rename, and the keys the old names asked for are
    // nobody's.
    cursor_to_pane(&mut app, "%221");
    for c in ['X', 'c', 's', 'v'] {
        assert_eq!(app.keymap.lookup(ch(c)), None, "{c}");
    }
    assert_eq!(app.keymap.lookup(ch('r')), None, "the override replaced r");
    app.on_key(ch('R'));
    let keys: Vec<Option<char>> = menu_items(&app).iter().map(|i| i.key).collect();
    assert_eq!(keys, vec![Some('w'), Some('a')], "R asks window or agent");
}

#[test]
fn n_opens_the_project_picker() {
    let cfg = config();
    let mut app = app(&cfg);

    assert_eq!(app.on_key(ch('n')), None);
    assert!(
        matches!(app.overlay, Overlay::Project(_)),
        "{:?}",
        app.overlay
    );
}

/// A popup started on the picker (`taw ui --new-session`) is the picker `n` opens, in either
/// preset (the search preset has no key for it), with zoxide's directories already listed.
#[test]
fn the_popup_can_start_on_the_new_session_picker() {
    for cfg in [config(), search_config()] {
        let zoxide = vec!["~/z/zebra-crossing".to_string()];
        let mut started = app(&cfg);
        assert_eq!(started.overlay, Overlay::None);
        started.set_zoxide(zoxide.clone());
        started.start_on_new_session_picker();

        let mut opened = app(&cfg);
        opened.set_zoxide(zoxide);
        opened.open_project_picker();

        assert_eq!(started.overlay, opened.overlay, "{:?}", cfg.keys.preset);
        let Overlay::Project(picker) = &started.overlay else {
            panic!("no picker: {:?}", started.overlay)
        };
        assert!(
            picker
                .candidates
                .iter()
                .any(|c| c.path == "~/z/zebra-crossing"),
            "zoxide's history is listed: {:?}",
            picker.candidates
        );
    }
}

/// A popup started on the picker has no tree anyone asked to see behind it: Esc closes the
/// popup at once, in either preset, as tmux-session-wizard's `t` does.
#[test]
fn esc_closes_a_popup_that_started_on_the_picker() {
    for cfg in [config(), search_config()] {
        let mut popup = app(&cfg);
        popup.start_on_new_session_picker();
        assert!(!popup.quit);

        assert_eq!(popup.on_key(key(KeyCode::Esc)), None);

        assert!(popup.quit, "{:?}: the popup stays open", cfg.keys.preset);
        assert_eq!(popup.overlay, Overlay::None);
    }
}

/// The picker `n` opens inside a popup keeps its own Esc: back to the tree, the popup stays.
#[test]
fn esc_on_the_picker_that_n_opens_goes_back_to_the_tree() {
    let cfg = config();
    let mut popup = app(&cfg);
    popup.on_key(ch('n'));
    assert!(matches!(popup.overlay, Overlay::Project(_)));

    assert_eq!(popup.on_key(key(KeyCode::Esc)), None);

    assert!(!popup.quit, "the popup closed");
    assert_eq!(popup.overlay, Overlay::None);
}

/// Only the picker the popup started on closes it. Once that one has been answered with
/// Enter, and the open did not close the popup (a hook refused, say), a picker opened
/// later goes back to the tree like any other.
#[test]
fn a_picker_opened_after_the_first_was_answered_goes_back_to_the_tree() {
    // A directory that is really there, under a scratch `$HOME`: Enter refuses one that is not.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("code/api")).unwrap();
    for cfg in [config(), search_config()] {
        let mut popup = app(&cfg);
        popup.home = home.path().to_path_buf();
        popup.start_on_new_session_picker();
        type_str(&mut popup, "~/code/api");
        assert_eq!(
            popup.on_key(key(KeyCode::Enter)),
            Some(Action::OpenProject {
                path: "~/code/api".to_string()
            }),
            "{:?}",
            cfg.keys.preset
        );
        assert_eq!(popup.overlay, Overlay::None);

        popup.open_project_picker();
        popup.on_key(key(KeyCode::Esc));

        assert!(!popup.quit, "{:?}: the popup closed", cfg.keys.preset);
        assert_eq!(popup.overlay, Overlay::None);
    }
}

#[test]
fn w_asks_for_a_branch_for_a_worktree_of_the_rows_session() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");

    app.on_key(ch('w'));

    assert_eq!(
        prompt(&app).kind,
        PromptKind::NewWorktree {
            workspace: "$2".to_string()
        }
    );
    assert_eq!(prompt(&app).title, "new worktree in web: branch");

    let Overlay::Prompt(p) = &mut app.overlay else {
        panic!("the prompt closed")
    };
    p.input.set("feat/pdf");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::NewWorktree {
            workspace: "$2".to_string(),
            branch: "feat/pdf".to_string(),
        })
    );
}

// ---------------------------------------------------------------- overlays

#[test]
fn o_and_question_mark_open_the_options_and_help_overlays() {
    let cfg = config();
    let mut app = app(&cfg);

    app.on_key(ch('o'));
    assert!(matches!(app.overlay, Overlay::Settings(_, _)));
    app.on_key(key(KeyCode::Esc));
    assert!(!app.overlay.is_open());

    app.on_key(ch('?'));
    assert!(matches!(app.overlay, Overlay::Help(_)));
    app.on_key(ch('q'));
    assert!(!app.overlay.is_open());
    assert!(!app.quit, "q inside an overlay closes it, it does not quit");
}

#[test]
fn q_quits() {
    let cfg = config();
    let mut app = app(&cfg);

    app.on_key(ch('q'));
    assert!(app.quit);
}

// --------------------------------------------------------------- the chord

fn chord() -> Chord {
    Chord::new(
        parse_tmux_key("C-b").expect("C-b parses"),
        parse_tmux_key("O"),
    )
}

#[test]
fn the_prefix_then_the_notification_key_goes_next_up() {
    let cfg = config();
    let mut app = app(&cfg);
    app.chord = Some(chord());

    assert_eq!(app.on_key(ctrl('b')), None, "the prefix alone does nothing");
    assert_eq!(
        app.on_key(ch('O')),
        // The action carries the pane the popup was opened from: `exec` has no
        // `$TMUX_PANE` of its own to walk from inside a `display-popup`.
        Some(Action::NextUp {
            own: Some("%111".to_string()),
        })
    );
}

/// In the app the walk starts from the pane you came out of, which is the one that is there: the app has
/// no `own_pane` and no client for tmux to guess one from. With none to start from it is as before.
#[test]
fn in_the_app_the_walk_starts_from_the_pane_you_came_out_of() {
    let cfg = config();
    let mut app = app(&cfg);
    app.chord = Some(chord());
    app.enter_app_mode(Some("%121".to_string()), None);
    assert_eq!(
        app.came_from(),
        Some("%121"),
        "the pane is one the list draws"
    );

    assert_eq!(app.on_key(ctrl('b')), None);
    assert_eq!(
        app.on_key(ch('O')),
        Some(Action::NextUp {
            own: Some("%121".to_string()),
        })
    );

    // With no pane to come from the walk has no start of its own.
    app.snap.own_pane = None;
    app.forget_came_from();
    app.on_key(ctrl('b'));
    assert_eq!(app.on_key(ch('O')), Some(Action::NextUp { own: None }));
}

/// The chord knows only the notification key, not `prefix+C` (the popup key, `A` by
/// default), so the key that opens the popup is swallowed like any other
/// second key, and the popup stays open.
#[test]
fn prefix_then_the_popup_key_is_ignored_not_a_close() {
    let cfg = config();
    let mut app = app(&cfg);
    app.chord = Some(chord());
    let start = app.sel;

    assert_eq!(app.on_key(ctrl('b')), None);
    assert_eq!(app.on_key(ch('A')), None);

    assert!(!app.quit, "the popup stays open");
    assert!(!app.overlay.is_open());
    assert_eq!(app.sel, start);
    // And the chord is spent: the next key is an ordinary key again.
    app.on_key(ch('j'));
    assert_eq!(app.sel, start + 1);
}

#[test]
fn a_key_after_the_prefix_that_means_nothing_is_swallowed_not_run() {
    let cfg = config();
    let mut app = app(&cfg);
    app.chord = Some(chord());
    let start = app.sel;

    app.on_key(ctrl('b'));
    app.on_key(ch('j'));

    assert_eq!(app.sel, start, "prefix+j is not a movement");
    // The chord is one-shot: the next `j` moves again.
    app.on_key(ch('j'));
    assert_eq!(app.sel, start + 1);
}

/// tmux's `prefix2` starts the chord exactly as `prefix` does (herdr v0.9.2 takes several prefix
/// keys, #4653): either one, then the notification key, goes next up from the pane the popup was
/// opened from, and whichever key follows either of them is the chord's to swallow.
#[test]
fn either_prefix_then_the_notification_key_goes_next_up() {
    let cfg = config();
    for prefix in [ctrl('b'), ctrl('a')] {
        let mut app = app(&cfg);
        app.chord = Chord::from_options("C-b", "C-a", "O");
        let start = app.sel;

        assert_eq!(app.on_key(prefix), None, "{prefix:?} alone does nothing");
        assert_eq!(
            app.on_key(ch('O')),
            Some(Action::NextUp {
                own: Some("%111".to_string()),
            }),
            "{prefix:?} then O"
        );

        app.on_key(prefix);
        app.on_key(ch('j'));
        assert_eq!(app.sel, start, "{prefix:?} then j is not a movement");
        app.on_key(ch('j'));
        assert_eq!(app.sel, start + 1, "and the chord is spent");
    }
}

/// tmux prints `None` for a `prefix2` nobody set: it is no key, so Ctrl-A is no chord and goes
/// nowhere in the tree, and `O` after it is no way to go next up.
#[test]
fn a_prefix2_of_none_starts_no_chord() {
    let cfg = config();
    let mut app = app(&cfg);
    app.chord = Chord::from_options("C-b", "None", "O");
    let start = app.sel;

    assert_eq!(app.on_key(ctrl('a')), None);
    assert_eq!(app.on_key(ch('O')), None);
    assert_eq!(app.sel, start);
    assert!(!app.overlay.is_open());
}

// -------------------------------------------- the prefix against herdr's text keys

/// A key with Alt held.
fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

/// Backspace with `modifiers` held, as a terminal that tells them apart sends it.
fn backspace_with(modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Backspace, modifiers)
}

/// The chord of a tmux whose `prefix` is `prefix`, and whose notification key is `O`.
fn chord_on(prefix: &str) -> Chord {
    Chord::new(
        parse_tmux_key(prefix).expect("a tmux key"),
        parse_tmux_key("O"),
    )
}

/// The chord of a tmux whose `prefix` is `C-x`, which no check here uses, and whose `prefix2` is
/// `prefix2`: the same keys as [`chord_on`], given by tmux's other option.
fn chord_on_prefix2(prefix2: &str) -> Chord {
    Chord::from_options("C-x", prefix2, "O").expect("two prefixes")
}

/// What the filter holds and where its cursor is.
fn query(app: &App) -> (String, usize) {
    let filter = app.filter.as_ref().expect("a filter is open");
    (filter.value().to_string(), filter.cursor())
}

/// The popup in `cfg`'s preset with `abc` typed into its filter, tmux's prefix `prefix`.
fn filtering_under<'a>(cfg: &'a Config, prefix: &str) -> App<'a> {
    filtering_with(cfg, chord_on(prefix))
}

/// The popup in `cfg`'s preset with `abc` typed into its filter, under `chord`.
fn filtering_with(cfg: &Config, chord: Chord) -> App<'_> {
    let mut popup = app(cfg);
    popup.chord = Some(chord);
    if cfg.keys.preset == KeysPreset::Modal {
        popup.on_key(ch('/'));
    }
    type_str(&mut popup, "abc");
    assert_eq!(query(&popup), ("abc".to_string(), 3));
    popup
}

/// With tmux's own prefix, Ctrl-B stays the prefix: in the filter of either preset it starts
/// the chord and moves no cursor, and the key after it is the chord's to swallow, not text.
#[test]
fn with_tmuxs_default_prefix_ctrl_b_stays_the_prefix_in_the_filter() {
    for cfg in [config(), search_config()] {
        let mut popup = filtering_under(&cfg, "C-b");

        assert_eq!(popup.on_key(ctrl('b')), None);
        assert_eq!(
            query(&popup),
            ("abc".to_string(), 3),
            "{:?}",
            cfg.keys.preset
        );

        assert_eq!(popup.on_key(ch('z')), None);
        assert_eq!(
            query(&popup),
            ("abc".to_string(), 3),
            "the key after the prefix is the chord's"
        );

        // The chord is spent, and Ctrl-F, which is not the prefix, moves as it does.
        popup.on_key(ctrl('a'));
        assert_eq!(popup.on_key(ctrl('f')), None);
        assert_eq!(query(&popup), ("abc".to_string(), 1));
    }
}

/// With any other prefix Ctrl-B and Ctrl-F are the filter's: one character left and right.
#[test]
fn with_another_prefix_ctrl_b_and_ctrl_f_move_the_cursor_in_the_filter() {
    for cfg in [config(), search_config()] {
        let mut popup = filtering_under(&cfg, "C-a");

        assert_eq!(popup.on_key(ctrl('b')), None);
        assert_eq!(
            query(&popup),
            ("abc".to_string(), 2),
            "{:?}",
            cfg.keys.preset
        );
        popup.on_key(ctrl('b'));
        assert_eq!(query(&popup), ("abc".to_string(), 1));
        popup.on_key(ctrl('f'));
        assert_eq!(query(&popup), ("abc".to_string(), 2));
        type_str(&mut popup, "X");
        assert_eq!(query(&popup), ("abXc".to_string(), 3));
    }
}

/// A prefix of Ctrl-F keeps Ctrl-F, and Ctrl-B, not being the prefix, is a cursor key.
#[test]
fn a_prefix_of_ctrl_f_keeps_ctrl_f_in_the_filter() {
    for cfg in [config(), search_config()] {
        let mut popup = filtering_under(&cfg, "C-f");
        popup.on_key(ctrl('a'));

        popup.on_key(ctrl('f'));
        assert_eq!(
            query(&popup),
            ("abc".to_string(), 0),
            "{:?}",
            cfg.keys.preset
        );
        popup.on_key(ch('z'));
        assert_eq!(
            query(&popup),
            ("abc".to_string(), 0),
            "swallowed by the chord"
        );

        popup.on_key(ctrl('f'));
        popup.on_key(ch('z'));
        popup.on_key(ctrl('e'));
        popup.on_key(ctrl('b'));
        assert_eq!(query(&popup), ("abc".to_string(), 2));
    }
}

/// tmux's `prefix2` is yielded to in the filter as `prefix` is: the chord is asked before the
/// query line sees a key, so the key is the prefix whatever it is. Ctrl-A, which moves the cursor
/// to the start of the line, and Ctrl-B and Ctrl-F, which move it a character, start the chord and
/// move nothing; the key after is the chord's, and the field is the field's again after that.
#[test]
fn a_second_prefix_starts_the_chord_in_the_filter_whatever_key_it_is() {
    for cfg in [config(), search_config()] {
        for prefix2 in ["C-a", "C-b", "C-f"] {
            let mut popup = filtering_with(&cfg, chord_on_prefix2(prefix2));
            let what = format!("{:?}, prefix2 {prefix2}", cfg.keys.preset);

            let second = parse_tmux_key(prefix2).expect("a tmux key");
            assert_eq!(popup.on_key(second), None, "{what}");
            assert_eq!(
                query(&popup),
                ("abc".to_string(), 3),
                "{what}: no cursor moved"
            );

            assert_eq!(popup.on_key(ch('z')), None);
            assert_eq!(
                query(&popup),
                ("abc".to_string(), 3),
                "{what}: the key after the prefix is the chord's"
            );

            popup.on_key(key(KeyCode::Left));
            assert_eq!(
                query(&popup),
                ("abc".to_string(), 2),
                "{what}: the chord is spent, the field edits again"
            );
        }
    }
}

/// And its chord goes next up from inside the filter, in both presets, with the query as it was.
#[test]
fn a_second_prefix_then_the_notification_key_goes_next_up_from_the_filter() {
    for cfg in [config(), search_config()] {
        let mut popup = filtering_with(&cfg, chord_on_prefix2("C-a"));

        assert_eq!(popup.on_key(ctrl('a')), None);
        assert_eq!(
            popup.on_key(ch('O')),
            Some(Action::NextUp {
                own: Some("%111".to_string()),
            }),
            "{:?}",
            cfg.keys.preset
        );
        assert_eq!(query(&popup), ("abc".to_string(), 3), "typed nothing");
    }
}

/// The filter takes every one of herdr's keys, in both presets: Ctrl-D, Alt-D, Ctrl-Y, the
/// word-wise Backspaces, all of them fzf's own but for Ctrl-Backspace (below).
#[test]
fn the_filter_takes_herdrs_text_keys_in_both_presets() {
    for cfg in [config(), search_config()] {
        let preset = cfg.keys.preset;
        let mut popup = app(&cfg);
        if preset == KeysPreset::Modal {
            popup.on_key(ch('/'));
        }
        type_str(&mut popup, "one two three");

        // Alt-Backspace cuts "three", Ctrl-Y puts it back.
        popup.on_key(backspace_with(KeyModifiers::ALT));
        assert_eq!(query(&popup).0, "one two ", "{preset:?}");
        popup.on_key(ctrl('y'));
        assert_eq!(query(&popup).0, "one two three", "{preset:?}");

        // Alt-D cuts the word after the cursor and leaves the space after it (Alt-F went to the
        // end of "one", so the space before "two" goes with it); Ctrl-D deletes the character
        // under the cursor, which a cut would have kept: the space that is left.
        popup.on_key(ctrl('a'));
        popup.on_key(alt('f'));
        popup.on_key(alt('d'));
        assert_eq!(query(&popup).0, "one three", "{preset:?}");
        popup.on_key(ctrl('d'));
        assert_eq!(query(&popup).0, "onethree", "{preset:?}");
        popup.on_key(ctrl('y'));
        assert_eq!(
            query(&popup).0,
            "one twothree",
            "{preset:?}: Ctrl-Y put back the cut, where the cursor was"
        );
    }
}

/// Where Alt-D, Alt-B and Alt-F draw the line between words depends on the field: herdr's rule
/// in the modal preset's filter, its prompts and its picker; fzf's in the search preset's query
/// line, which is a pure fzf search (`order-tracking-app` cut from its start by fzf's Alt-D
/// leaves `-tracking-app`, then `-app`, then nothing). A prompt of the search preset is not
/// that query line and keeps herdr's.
#[test]
fn the_search_presets_query_cuts_by_fzfs_word_boundary_and_the_other_fields_by_herdrs() {
    let cut_thrice = |popup: &mut App| {
        popup.on_key(ctrl('a'));
        (0..3)
            .map(|_| {
                popup.on_key(alt('d'));
                query(popup).0
            })
            .collect::<Vec<_>>()
    };

    let cfg = search_config();
    let mut popup = app(&cfg);
    type_str(&mut popup, "order-tracking-app");
    assert_eq!(cut_thrice(&mut popup), ["-tracking-app", "-app", ""]);

    let cfg = config();
    let mut popup = app(&cfg);
    popup.on_key(ch('/'));
    type_str(&mut popup, "order-tracking-app");
    assert_eq!(
        cut_thrice(&mut popup),
        ["-tracking-app", "tracking-app", "-app"]
    );

    // A prompt of the search preset keeps herdr's boundary.
    let cfg = search_config();
    let mut popup = app(&cfg);
    cursor_to_header(&mut popup, "web");
    popup.open_context_menu(5, 3);
    popup.on_key(ch('r'));
    popup.on_key(ctrl('u'));
    type_str(&mut popup, "order-tracking-app");
    popup.on_key(ctrl('a'));
    popup.on_key(alt('d'));
    assert_eq!(prompt(&popup).input.value(), "-tracking-app");
    popup.on_key(alt('d'));
    assert_eq!(prompt(&popup).input.value(), "tracking-app");
}

/// fzf's ctrl-d is `delete-char/eof`, and the search preset is a pure fzf search: with text it
/// deletes the character under the cursor (and finds none at the end of the line), on an empty
/// query it quits, as Esc does. The press that empties the query only deletes: the next one
/// is the one that finds it empty.
#[test]
fn ctrl_d_in_the_search_preset_deletes_a_character_and_quits_on_an_empty_query() {
    let cfg = search_config();
    let mut popup = app(&cfg);
    type_str(&mut popup, "ab");

    popup.on_key(ctrl('d'));
    assert_eq!(
        query(&popup),
        ("ab".to_string(), 2),
        "nothing after the cursor"
    );
    assert!(!popup.quit, "a query with text is not at its end");

    popup.on_key(ctrl('a'));
    popup.on_key(ctrl('d'));
    assert_eq!(query(&popup), ("b".to_string(), 0));
    popup.on_key(ctrl('d'));
    assert_eq!(query(&popup), (String::new(), 0));
    assert!(!popup.quit, "the press that emptied the query only deleted");

    assert_eq!(popup.on_key(ctrl('d')), None);
    assert!(
        popup.quit,
        "on an empty query ctrl-d quits, as fzf's delete-char/eof does"
    );
}

/// Only ctrl-d is the end of input: a plain `d` is the first letter of a query, alt-d is fzf's
/// `kill-word` (which finds nothing to kill in an empty query), and ctrl-alt-d is no key of
/// fzf's at all.
#[test]
fn only_ctrl_d_is_the_end_of_input_on_an_empty_query() {
    let cfg = search_config();
    let mut popup = app(&cfg);

    popup.on_key(alt('d'));
    popup.on_key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    assert!(!popup.quit, "neither alt-d nor ctrl-alt-d is the end");
    assert_eq!(query(&popup), (String::new(), 0));

    popup.on_key(ch('d'));
    assert!(!popup.quit, "a plain d is text");
    assert_eq!(query(&popup), ("d".to_string(), 1));
}

/// Esc's path: nothing else about the popup changes, and a fresh popup's empty query quits at
/// the first press.
#[test]
fn ctrl_d_on_the_search_presets_untouched_query_quits_like_esc_does() {
    let cfg = search_config();
    let mut esc = app(&cfg);
    esc.on_key(key(KeyCode::Esc));
    let mut eof = app(&cfg);
    eof.on_key(ctrl('d'));

    assert!(esc.quit && eof.quit);
    assert_eq!(eof.sel, esc.sel);
    assert!(!eof.overlay.is_open());
}

/// A `[keys]` entry bound to ctrl-d still wins over fzf's end of input.
#[test]
fn a_keys_entry_bound_to_ctrl_d_beats_the_search_presets_end_of_input() {
    let mut cfg = search_config();
    cfg.keys
        .bindings
        .insert("help".to_string(), KeySpec::One("ctrl+d".to_string()));
    let mut popup = app(&cfg);

    popup.on_key(ctrl('d'));

    assert!(!popup.quit);
    assert!(
        matches!(popup.overlay, Overlay::Help(_)),
        "{:?}",
        popup.overlay
    );
}

/// tmux's prefix keeps its meaning over fzf's end of input: with a prefix of Ctrl-D the key
/// starts the chord, which is asked before the query line sees a key, so an empty query does
/// not take it for the end of the search and the popup stays open however often it is pressed.
/// With another prefix the same empty query does end there, which is what the first half
/// would otherwise be passing for nothing.
#[test]
fn a_prefix_of_ctrl_d_is_the_prefix_and_not_the_search_presets_end_of_input() {
    let cfg = search_config();
    // As `prefix`, and as `prefix2`: the key is the prefix either way.
    for chord in [chord_on("C-d"), chord_on_prefix2("C-d")] {
        let mut popup = app(&cfg);
        popup.chord = Some(chord);

        for press in 1..=2 {
            assert_eq!(popup.on_key(ctrl('d')), None);
            assert!(!popup.quit, "press {press} of the prefix");
        }
        assert_eq!(query(&popup), (String::new(), 0), "and it is no text");
    }

    let mut other = app(&cfg);
    other.chord = Some(chord_on("C-b"));
    other.on_key(ctrl('d'));
    assert!(other.quit, "with a prefix of Ctrl-B it is the end of input");
}

/// Only the search preset's query is fzf's: in the modal preset ctrl-d never closes anything,
/// with the filter open and empty, with none open, and in a prompt with nothing in it.
#[test]
fn ctrl_d_never_closes_anything_in_the_modal_preset() {
    let cfg = config();
    let mut popup = app(&cfg);

    popup.on_key(ch('/'));
    popup.on_key(ctrl('d'));
    assert!(!popup.quit, "an empty filter");
    assert_eq!(query(&popup), (String::new(), 0), "and it stays open");
    type_str(&mut popup, "x");
    popup.on_key(ctrl('d'));
    assert!(!popup.quit);
    assert_eq!(query(&popup), ("x".to_string(), 1));

    popup.on_key(key(KeyCode::Esc));
    assert!(popup.filter.is_none());
    popup.on_key(ctrl('d'));
    assert!(!popup.quit, "with no filter at all");
    // The one state the routing above never makes, an empty filter that is open but not
    // focused: there the preset alone says ctrl-d is not fzf's.
    popup.filter = Some(Input::new(""));
    popup.on_key(ctrl('d'));
    assert!(!popup.quit, "an open filter that is not focused");

    cursor_to_header(&mut popup, "web");
    popup.on_key(ch('r'));
    popup.on_key(ctrl('u'));
    assert_eq!(prompt(&popup).input.value(), "");
    popup.on_key(ctrl('d'));
    assert!(!popup.quit, "in an empty prompt");
    assert!(popup.overlay.is_open(), "which stays open");

    let mut popup = app(&cfg);
    popup.on_key(ch('n'));
    assert!(matches!(popup.overlay, Overlay::Project(_)), "the picker");
    popup.on_key(ctrl('d'));
    assert!(!popup.quit, "in an empty picker");
    assert!(
        matches!(popup.overlay, Overlay::Project(_)),
        "which stays open"
    );
}

/// Nor does it close a prompt or the picker in the search preset: the query line is fzf's,
/// an overlay's text field is not.
#[test]
fn ctrl_d_in_an_overlay_of_the_search_preset_edits_and_closes_nothing() {
    let cfg = search_config();
    let mut popup = app(&cfg);
    // The menu's rename, on a header with its name cleared.
    cursor_to_header(&mut popup, "web");
    popup.open_context_menu(5, 3);
    popup.on_key(ch('r'));
    popup.on_key(ctrl('u'));
    assert_eq!(prompt(&popup).input.value(), "");

    popup.on_key(ctrl('d'));

    assert!(!popup.quit);
    assert!(popup.overlay.is_open());
}

/// Ctrl-Backspace is the one key herdr and fzf disagree on: herdr cuts a word, fzf deletes a
/// character (`backward-delete-char`). The modal preset's filter is herdr's; the search
/// preset is a pure fzf search, so there it stays fzf's.
#[test]
fn ctrl_backspace_cuts_a_word_in_the_modal_filter_and_deletes_a_character_in_fzfs() {
    let cfg = config();
    let mut popup = app(&cfg);
    popup.on_key(ch('/'));
    type_str(&mut popup, "foo bar");
    popup.on_key(backspace_with(KeyModifiers::CONTROL));
    assert_eq!(query(&popup).0, "foo ");

    let cfg = search_config();
    let mut popup = app(&cfg);
    type_str(&mut popup, "foo bar");
    popup.on_key(backspace_with(KeyModifiers::CONTROL));
    assert_eq!(query(&popup).0, "foo ba");
    // Alt-Backspace is fzf's backward-kill-word, the same as herdr's.
    popup.on_key(backspace_with(KeyModifiers::ALT));
    assert_eq!(query(&popup).0, "foo ");
}

/// The search preset binds nothing new for these keys (it is a pure
/// fzf search): none of them is in either preset's keymap.
#[test]
fn neither_preset_binds_herdrs_text_keys() {
    for cfg in [config(), search_config()] {
        let popup = app(&cfg);
        for key in [
            ctrl('b'),
            ctrl('f'),
            ctrl('d'),
            ctrl('y'),
            alt('d'),
            backspace_with(KeyModifiers::ALT),
            backspace_with(KeyModifiers::CONTROL),
        ] {
            assert_eq!(popup.keymap.lookup(key), None, "{key:?}");
        }
    }
}

/// A `[keys]` entry still wins over a text key: the filter moves the cursor for a movement
/// action bound to Ctrl-F, and never sees the key.
#[test]
fn a_keys_entry_bound_to_a_text_key_still_wins() {
    let mut cfg = config();
    cfg.keys
        .bindings
        .insert("move_down".to_string(), KeySpec::One("ctrl+f".to_string()));
    let mut popup = app(&cfg);
    popup.on_key(ch('/'));
    type_str(&mut popup, "a");
    popup.on_key(ctrl('b'));
    assert_eq!(
        query(&popup),
        ("a".to_string(), 0),
        "unbound keys are text keys"
    );
    let before = popup.sel;
    popup.on_key(ctrl('f'));
    assert_eq!(
        query(&popup),
        ("a".to_string(), 0),
        "Ctrl-F went to move_down"
    );
    assert_ne!(popup.sel, before, "and the cursor moved a row");
}

/// The rename prompt and the new session picker are text fields too, and the overlay owns
/// the keyboard whole, so their keys are the input's own: with tmux's prefix Ctrl-B the new
/// Ctrl-B does nothing there (it stays the prefix's), with another prefix it moves the cursor.
#[test]
fn a_prompt_and_the_picker_leave_the_new_keys_to_a_prefix_bound_to_one() {
    for (prefix, moves) in [("C-b", false), ("C-a", true), ("C-f", true)] {
        a_prompt_and_the_picker_under(prefix, moves, chord_on);
    }
}

/// tmux's `prefix2` is yielded to exactly as `prefix` is: the same keys, the same answers.
#[test]
fn a_prompt_and_the_picker_leave_the_new_keys_to_a_second_prefix_bound_to_one() {
    for (prefix2, moves) in [("C-b", false), ("C-a", true), ("C-f", true)] {
        a_prompt_and_the_picker_under(prefix2, moves, chord_on_prefix2);
    }
}

/// The checks of the two tests above: a prompt and the picker, under the chord `chord` builds
/// for `prefix`, where Ctrl-B moves the cursor back a character only when `moves`.
fn a_prompt_and_the_picker_under(prefix: &str, moves: bool, chord: fn(&str) -> Chord) {
    let cfg = config();
    let mut popup = app(&cfg);
    popup.chord = Some(chord(prefix));
    cursor_to_header(&mut popup, "web");
    popup.on_key(ch('r'));
    assert_eq!(prompt(&popup).input.cursor(), 3);

    popup.on_key(ctrl('b'));
    assert_eq!(
        prompt(&popup).input.cursor(),
        if moves { 2 } else { 3 },
        "the rename prompt, prefix {prefix}, ctrl-b"
    );
    popup.on_key(ctrl('a'));
    popup.on_key(ctrl('f'));
    let after_f = prompt(&popup).input.cursor();
    assert_eq!(
        after_f,
        if prefix == "C-f" { 0 } else { 1 },
        "the rename prompt, prefix {prefix}, ctrl-f"
    );

    // The picker: type a path, then the same two keys.
    let mut popup = app(&cfg);
    popup.chord = Some(chord(prefix));
    popup.on_key(ch('n'));
    type_str(&mut popup, "~/abc");
    let cursor = |popup: &App| match &popup.overlay {
        Overlay::Project(picker) => picker.input.cursor(),
        other => panic!("expected the picker, got {other:?}"),
    };
    assert_eq!(cursor(&popup), 5);
    popup.on_key(ctrl('b'));
    assert_eq!(
        cursor(&popup),
        if moves { 4 } else { 5 },
        "the picker, prefix {prefix}, ctrl-b"
    );
}

/// What a text field takes stays the same whatever the prefix is: a prefix of
/// Ctrl-A (screen's) does not turn Ctrl-A in a prompt into nothing, as `prefix` or as `prefix2`.
#[test]
fn a_prefix_does_not_take_back_a_key_a_prompt_always_had() {
    let cfg = config();
    for chord in [chord_on("C-a"), chord_on_prefix2("C-a")] {
        let mut popup = app(&cfg);
        popup.chord = Some(chord);
        cursor_to_header(&mut popup, "web");
        popup.on_key(ch('r'));

        popup.on_key(ctrl('a'));

        assert_eq!(prompt(&popup).input.cursor(), 0);
    }
}

/// The prompt takes the rest of the keys as well: Ctrl-Y puts back what Ctrl-W cut.
#[test]
fn a_prompt_cuts_and_yanks() {
    let cfg = config();
    let mut popup = app(&cfg);
    popup.chord = Some(chord_on("C-x"));
    cursor_to_header(&mut popup, "web");
    popup.on_key(ch('r'));

    popup.on_key(ctrl('w'));
    assert_eq!(prompt(&popup).input.value(), "");
    popup.on_key(ctrl('y'));
    popup.on_key(ctrl('y'));
    assert_eq!(prompt(&popup).input.value(), "webweb");
    popup.on_key(ctrl('b'));
    popup.on_key(ctrl('d'));
    assert_eq!(prompt(&popup).input.value(), "webwe");
}

// ------------------------------------------------------------ depth, flat

#[test]
fn h_and_l_step_the_depth_and_stop_at_the_ends() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(app.depth, Depth::All);

    app.on_key(ch('h'));
    assert_eq!(app.depth, Depth::Agents);
    app.on_key(ch('h'));
    assert_eq!(app.depth, Depth::Sessions);
    app.on_key(ch('h'));
    assert_eq!(app.depth, Depth::Sessions, "shallowest is a floor");
    assert_eq!(app.rows.rows.len(), 5, "headers only");

    for _ in 0..3 {
        app.on_key(ch('l'));
    }
    assert_eq!(app.depth, Depth::All, "deepest is a ceiling");
}

#[test]
fn f_flattens_the_tree_and_back() {
    let cfg = config();
    let mut app = app(&cfg);
    let tree_rows = app.rows.rows.len();

    app.on_key(ch('f'));
    assert_eq!(app.layout, Layout::Flat);
    assert_eq!(
        app.rows.rows.len(),
        tree_rows - 5,
        "the five headers are gone"
    );
    // `f` keeps your row, so it is still the pane you came from.
    assert_eq!(pane_at(&app), "%111", "the cursor is still your own row");

    app.on_key(ch('f'));
    assert_eq!(app.layout, Layout::Tree);
}

#[test]
fn f_at_the_sessions_depth_flips_the_layout_without_changing_the_screen() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('h'));
    app.on_key(ch('h')); // config() starts at the `all` depth: two steps to sessions
    assert_eq!(app.depth, Depth::Sessions);
    let before = app.rows.rows.clone();
    let sel = app.sel;

    app.on_key(ch('f'));

    assert_eq!(app.layout, Layout::Flat, "the layout flipped");
    assert!(
        !app.flat_now(),
        "but at the sessions depth the list is not flat"
    );
    assert_eq!(app.rows.rows, before, "the headers are still there");
    assert_eq!(app.sel, sel);

    app.on_key(ch('l'));
    assert!(app.flat_now(), "l shows the flat agents");
    assert!(app.rows.rows.iter().all(|r| !matches!(r, Row::Session(_))));
}

#[test]
fn a_flat_list_carried_into_the_sessions_depth_shows_the_headers_and_comes_back_with_l() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('f'));
    assert_eq!(app.layout, Layout::Flat);
    app.on_key(ch('h')); // all -> agents
    app.on_key(ch('h')); // agents -> sessions
    assert!(
        app.rows.rows.iter().all(|r| matches!(r, Row::Session(_))),
        "at the sessions depth a flat layout draws the headers"
    );
    assert!(!app.flat_now());
    app.on_key(ch('l'));
    assert!(app.flat_now(), "l returns to the flat agents");
    assert!(app.rows.rows.iter().all(|r| !matches!(r, Row::Session(_))));
}

/// web and infra are the only sessions with a hot agent (module doc table); api and notes
/// have none. Starting the cursor on api means the row `Tab` reaches first by plain tmux
/// order (notes) is not hot, so this only passes when `Tab` truly walks `next_up_rows`, not
/// when it happens to land on an adjacent header that is coincidentally hot.
#[test]
fn tab_still_stops_at_a_folded_hot_header_when_a_flat_layout_is_carried_into_the_sessions_depth() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('f')); // flat
    app.on_key(ch('h')); // all -> agents
    app.on_key(ch('h')); // agents -> sessions: headers only, layout still Flat
    assert!(!app.flat_now());
    cursor_to_header(&mut app, "api");

    app.on_key(key(KeyCode::Tab));

    let Some(Row::Session(space)) = app.current().cloned() else {
        panic!(
            "Tab lands on a folded header that needs you, got {:?}",
            app.current()
        );
    };
    assert!(
        app.session_hot_agent(space).is_some(),
        "the header Tab stopped at has an agent that needs you"
    );
}

// ------------------------------------------------------------------- mouse

#[test]
fn a_click_selects_and_a_double_click_goes() {
    let cfg = config();
    let mut app = app(&cfg);
    let row = app
        .rows
        .rows
        .iter()
        .position(|r| matches!(r, Row::Pane(p) if app.snap.panes[*p].pane_id == "%221"))
        .expect("web 2:zsh.1 has a row");
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 40, 30),
        rows: vec![(5, row)],
        ..PanelHit::default()
    };

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(app.on_mouse(click), None);
    assert_eq!(app.sel, row);
    assert_eq!(
        app.on_mouse(click),
        Some(Action::Jump {
            session: "$2".to_string(),
            window: "@22".to_string(),
            pane: "%221".to_string(),
        })
    );
}

/// A double-click on your own row closes the popup and runs nothing, as Enter there does
/// (your own row is where you already are). The first click only selects it.
/// The same in both presets.
#[test]
fn a_double_click_on_your_own_row_just_closes_the_popup_like_enter() {
    for cfg in [config(), search_config()] {
        let mut app = app(&cfg);
        let preset = app.preset;
        let own = app.own_row().expect("your own row is drawn");
        app.hits.tree = PanelHit {
            area: Rect::new(0, 0, 40, 30),
            rows: vec![(5, own)],
            ..PanelHit::default()
        };

        assert_eq!(app.on_mouse(click(1, 5, MouseButton::Left)), None);
        assert_eq!(app.sel, own, "{preset:?}");
        assert!(!app.quit, "{preset:?}: one click only selects");

        assert_eq!(
            app.on_mouse(click(1, 5, MouseButton::Left)),
            None,
            "{preset:?}: nothing for tmux, you are already there"
        );
        assert!(app.quit, "{preset:?}: the second click closes the popup");
    }
}

#[test]
fn a_right_click_opens_the_same_actions_as_a_lettered_list() {
    let cfg = config();
    let mut app = app(&cfg);
    let row = app
        .rows
        .rows
        .iter()
        .position(|r| matches!(r, Row::Pane(p) if app.snap.panes[*p].pane_id == "%221"))
        .expect("web 2:zsh.1 has a row");
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 40, 30),
        rows: vec![(5, row)],
        ..PanelHit::default()
    };

    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: 1,
        row: 5,
        modifiers: KeyModifiers::NONE,
    });

    let choices: Vec<RowAction> = menu_items(&app).iter().map(|i| i.choice).collect();
    assert_eq!(
        choices,
        vec![
            RowAction::Go,
            RowAction::Rename,
            RowAction::Close,
            RowAction::NewWorktree,
            RowAction::PasteImage
        ]
    );

    // Its letters are the keyboard's letters: `x` confirms the same close.
    app.on_key(ch('x'));
    assert!(
        confirm_message(&app).starts_with("kill-pane web:2.1 (claude)?"),
        "{}",
        confirm_message(&app)
    );
}

/// The menu offers the row's own actions and nothing tmux already has a
/// key for — no splits, no windows — and no options, which is `o`. It is the same entries
/// in both presets, and an agent row's has paste image after the four (the search preset's
/// only way to it). A header's menu is the four; a parked worktree has no name of its own to
/// change, so it has no rename.
#[test]
fn the_right_click_menu_lists_go_rename_close_new_worktree_and_on_an_agent_paste_image() {
    for cfg in [config(), search_config()] {
        let mut app = app(&cfg);
        let preset = app.preset;
        cursor_to_pane(&mut app, "%221");
        app.on_mouse(click(5, 3, MouseButton::Right));
        let keys: Vec<Option<char>> = menu_items(&app).iter().map(|i| i.key).collect();
        assert_eq!(
            keys,
            vec![None, Some('r'), Some('x'), Some('w'), Some('v')],
            "{preset:?}"
        );
        let labels: Vec<&str> = menu_items(&app).iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "go",
                "rename (window or agent)",
                "close this pane (kill-pane)",
                "new worktree\u{2026}",
                "paste image",
            ],
            "{preset:?}"
        );

        app.on_key(key(KeyCode::Esc));
        cursor_to_header(&mut app, "infra");
        app.on_mouse(click(5, 3, MouseButton::Right));
        let keys: Vec<Option<char>> = menu_items(&app).iter().map(|i| i.key).collect();
        assert_eq!(
            keys,
            vec![None, Some('r'), Some('x'), Some('w')],
            "{preset:?}"
        );

        app.on_key(key(KeyCode::Esc));
        cursor_to_worktree(&mut app);
        app.on_mouse(click(5, 3, MouseButton::Right));
        let keys: Vec<Option<char>> = menu_items(&app).iter().map(|i| i.key).collect();
        assert_eq!(keys, vec![None, Some('x'), Some('w')], "{preset:?}");
    }
}

#[test]
fn a_click_outside_an_open_menu_closes_it_and_a_click_on_its_row_outside_the_box_does_not_run_an_item()
 {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_mouse(click(5, 3, MouseButton::Right));
    assert!(
        matches!(app.overlay, Overlay::Menu(_)),
        "right-click opens the menu"
    );
    // The view records the box; stand in for it here.
    app.hits.overlay = OverlayHit {
        area: Rect::new(5, 3, 30, 8),
        rows: vec![(4, 0), (5, 1), (6, 2)],
    };

    let action = app.on_mouse(click(120, 4, MouseButton::Left)); // same row as item 0, far outside
    assert_eq!(action, None, "nothing ran");
    assert!(!app.overlay.is_open(), "the menu is gone");
}

/// A right-click outside an open menu is a right-click on whatever it lands on: the old
/// menu closes and the row under the pointer opens its own, as a context menu does.
/// Inside the box, a right-click still does nothing.
#[test]
fn a_right_click_outside_an_open_menu_opens_the_menu_of_the_row_under_the_pointer() {
    let cfg = config();
    let mut app = app(&cfg);
    let web = app
        .rows
        .rows
        .iter()
        .position(|r| matches!(r, Row::Pane(p) if app.snap.panes[*p].pane_id == "%221"))
        .expect("web 2:zsh.1 has a row");
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 40, 30),
        rows: vec![(1, app.sel), (8, web)],
        ..PanelHit::default()
    };
    app.on_mouse(click(3, 1, MouseButton::Right));
    let Overlay::Menu(first) = app.overlay.clone() else {
        panic!("right-click opens a menu, got {:?}", app.overlay);
    };
    assert!(first.title.starts_with("mobile:"), "{}", first.title);
    // The view records the box; stand in for it here, under the first click.
    app.hits.overlay = OverlayHit {
        area: Rect::new(3, 2, 30, 5),
        rows: vec![(3, 0), (4, 1), (5, 2)],
    };

    app.on_mouse(click(10, 4, MouseButton::Right));
    assert_eq!(
        app.overlay,
        Overlay::Menu(first),
        "inside the box nothing changes"
    );

    assert_eq!(app.on_mouse(click(3, 8, MouseButton::Right)), None);
    assert_eq!(app.sel, web, "the cursor went to the row under the pointer");
    let Overlay::Menu(second) = &app.overlay else {
        panic!("a menu for that row is open, got {:?}", app.overlay);
    };
    assert!(second.title.starts_with("web:2:zsh.1"), "{}", second.title);
    assert_eq!(second.anchor, (3, 8), "drawn under the new click");
}

#[test]
fn a_click_on_a_menu_item_inside_the_box_runs_it() {
    let cfg = config();
    let mut app = app(&cfg);
    // Off your own row: "go" there is a real tmux action, not just a close.
    app.on_key(ch('3'));
    app.on_mouse(click(5, 3, MouseButton::Right));
    app.hits.overlay = OverlayHit {
        area: Rect::new(5, 3, 30, 8),
        rows: vec![(4, 0), (5, 1), (6, 2)],
    };
    let action = app.on_mouse(click(10, 4, MouseButton::Left)); // item 0 is "go"
    assert_eq!(
        action,
        Some(Action::SwitchWorkspace {
            session: "$3".to_string()
        }),
        "infra's header, not the own row's no-op close"
    );
}

#[test]
fn esc_closes_an_open_menu_and_leaves_the_popup_open() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_mouse(click(5, 3, MouseButton::Right));
    app.on_key(key(KeyCode::Esc));
    assert!(!app.overlay.is_open());
    assert!(!app.quit, "Esc closed the menu, not the popup");
}

#[test]
fn the_wheel_scrolls_without_dragging_the_cursor_back() {
    let cfg = config();
    let mut app = app(&cfg);
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 40, 30),
        rows: Vec::new(),
        ..PanelHit::default()
    };

    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(app.scroll, 1);
    assert!(!app.follow_sel);
}

#[test]
fn the_wheel_over_the_preview_scrolls_the_preview_not_the_tree() {
    let cfg = config();
    let mut app = app(&cfg);
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 60, 30),
        rows: Vec::new(),
        ..PanelHit::default()
    };
    app.hits.preview = Rect::new(60, 0, 80, 30);
    app.preview_max_scroll = 5;

    let wheel = |kind, column| MouseEvent {
        kind,
        column,
        row: 3,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(wheel(MouseEventKind::ScrollUp, 70));
    app.on_mouse(wheel(MouseEventKind::ScrollUp, 70));
    assert_eq!(app.preview_scroll, 2, "two notches up from the tail");
    assert_eq!(app.scroll, 0, "the tree did not move");

    app.on_mouse(wheel(MouseEventKind::ScrollDown, 70));
    assert_eq!(app.preview_scroll, 1);
    for _ in 0..10 {
        app.on_mouse(wheel(MouseEventKind::ScrollUp, 70));
    }
    assert_eq!(app.preview_scroll, 5, "clamped at the top of the content");
    for _ in 0..10 {
        app.on_mouse(wheel(MouseEventKind::ScrollDown, 70));
    }
    assert_eq!(app.preview_scroll, 0, "and at the tail");

    app.on_mouse(wheel(MouseEventKind::ScrollDown, 10));
    assert_eq!(
        app.scroll, 1,
        "over the tree the wheel still scrolls the tree"
    );
    assert_eq!(app.preview_scroll, 0);
}

#[test]
fn the_preview_offset_survives_a_recapture_and_resets_when_the_pane_changes() {
    let cfg = config();
    let mut app = app(&cfg);
    app.set_preview(Some("%111".to_string()), Some(preview::to_text("a\nb\nc")));
    app.preview_scroll = 2;
    app.set_preview(
        Some("%111".to_string()),
        Some(preview::to_text("a\nb\nc\nd")),
    );
    assert_eq!(
        app.preview_scroll, 2,
        "same pane, fresh capture: you keep your place"
    );
    app.set_preview(Some("%16".to_string()), Some(preview::to_text("x")));
    assert_eq!(app.preview_scroll, 0, "another pane: back to the tail");
    app.preview_scroll = 1;
    app.set_preview(None, None);
    assert_eq!(app.preview_scroll, 0);
}

/// `n` notches of the wheel up over the preview, the way the view lays it out: the panel
/// to the right of the tree, with room to scroll.
fn scroll_preview_up(app: &mut App, notches: usize) {
    app.hits.preview = Rect::new(60, 0, 80, 30);
    app.preview_max_scroll = 10;
    for _ in 0..notches {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 70,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
    }
}

/// The offset belongs to the row it was scrolled on (it resets to the tail whenever
/// the row under the cursor changes). A header previews its session's active pane, so the
/// row below mobile's header shows the very same capture — and moving onto it still
/// returns the preview to the tail. A recapture with the cursor where it was keeps your
/// place.
#[test]
fn moving_to_another_row_returns_the_preview_to_the_tail_even_when_it_shows_the_same_pane() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('1'));
    assert_eq!(header_at(&app), "mobile");
    assert_eq!(app.preview_pane().as_deref(), Some("%111"));
    app.set_preview(app.preview_pane(), Some(preview::to_text("a\nb\nc\nd")));
    scroll_preview_up(&mut app, 2);
    assert_eq!(app.preview_scroll, 2);

    // The next tick recaptures the same pane, the cursor still on the header.
    app.set_preview(app.preview_pane(), Some(preview::to_text("a\nb\nc\nd\ne")));
    assert_eq!(app.preview_scroll, 2, "a recapture keeps your place");

    app.on_key(ch('j'));
    assert_eq!(
        pane_at(&app),
        "%111",
        "the header's active pane, on its own row"
    );
    assert_eq!(app.preview_pane().as_deref(), Some("%111"));
    assert_eq!(app.preview_scroll, 0, "another row: back to the tail");

    // And by the mouse: a click on another row is a move like any other.
    scroll_preview_up(&mut app, 3);
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 60, 30),
        rows: vec![(2, 2)],
        ..PanelHit::default()
    };
    app.on_mouse(click(5, 2, MouseButton::Left));
    assert_eq!(pane_at(&app), "%121");
    assert_eq!(app.preview_scroll, 0);
}

/// A depth or layout change, or a refresh, keeps the offset only while the cursor stays on
/// the row it was scrolled on; folding that row into its header is a move to another row.
#[test]
fn a_depth_change_or_a_refresh_keeps_the_preview_offset_only_on_the_same_row() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(pane_at(&app), "%111");
    app.set_preview(app.preview_pane(), Some(preview::to_text("a\nb\nc\nd")));
    scroll_preview_up(&mut app, 2);

    app.on_key(ch('h')); // all -> agents: your claude is still drawn, and still under you
    assert_eq!(pane_at(&app), "%111");
    assert_eq!(app.preview_scroll, 2);
    app.set_snapshot(app.snap.clone()); // the half-second refresh
    assert_eq!(app.preview_scroll, 2);

    app.on_key(ch('h')); // agents -> sessions: folded into mobile's header
    assert_eq!(header_at(&app), "mobile");
    assert_eq!(app.preview_scroll, 0);
}

/// Until a capture of the new row's pane is set, the text on screen is the last capture, so
/// the wrap rule is that capture's pane's, not the new row's: moving onto nvim keeps an
/// agent's prose wrapped until nvim's own screen is set, and moving back keeps nvim's
/// screen cut until the agent's is.
#[test]
fn the_wrap_rule_is_the_captured_panes_until_the_next_capture() {
    let cfg = config();
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    snap.panes
        .iter_mut()
        .find(|p| p.pane_id == "%131")
        .expect("mobile's nvim")
        .alternate_on = true;
    app.set_snapshot(snap);
    assert_eq!(pane_at(&app), "%111");
    app.set_preview(
        app.preview_pane(),
        Some(preview::to_text("an agent's prose")),
    );
    assert!(app.preview_wraps());

    app.on_key(ch('j'));
    app.on_key(ch('j'));
    assert_eq!(pane_at(&app), "%131");
    assert!(app.preview_wraps(), "still the agent's capture on screen");
    app.set_preview(app.preview_pane(), Some(preview::to_text("nvim's screen")));
    assert!(!app.preview_wraps(), "nvim's own screen is cut");

    app.on_key(ch('k'));
    app.on_key(ch('k'));
    assert_eq!(pane_at(&app), "%111");
    assert!(!app.preview_wraps(), "still nvim's capture on screen");
    app.set_preview(
        app.preview_pane(),
        Some(preview::to_text("an agent's prose")),
    );
    assert!(app.preview_wraps());
}

// ------------------------------------------------------------- hint line

#[test]
fn the_hint_line_names_the_keys_that_are_actually_bound() {
    let cfg = config();
    let app = app(&cfg);

    // The six, in order: the numbers lead, `Enter` always says
    // `go`, and `q quit` is not on the line (it is in `?`). The letters of the cursor's
    // session come right after the numbers:
    // the cursor opens on mobile's claude, and mobile has five rows.
    let hints = app.hint_line();
    assert_eq!(
        hints,
        vec![
            ("1-9".to_string(), "session".to_string()),
            ("a-e".to_string(), "rows".to_string()),
            ("Tab".to_string(), "next up".to_string()),
            ("Enter".to_string(), "go".to_string()),
            ("j/k".to_string(), "move".to_string()),
            ("/".to_string(), "filter".to_string()),
            ("?".to_string(), "help".to_string()),
        ]
    );
}

#[test]
fn a_rebound_key_shows_its_own_key_on_the_hint_line() {
    let mut cfg = config();
    // `a` is free in the modal preset, so the override takes it outright.
    cfg.keys.bindings.insert(
        "next_attention".to_string(),
        crate::config::KeySpec::One("a".to_string()),
    );
    let app = app(&cfg);

    // The numbers lead the line; the walk's entry names its own key.
    assert!(
        app.hint_line()
            .contains(&("a".to_string(), "next up".to_string())),
        "{:?}",
        app.hint_line()
    );
    let mut app = app;
    app.on_key(ch('a'));
    assert_eq!(pane_at(&app), "%231", "and the key itself walks the list");
}

// ---------------------------------------------------------- search preset

#[test]
fn the_search_preset_types_straight_into_the_filter() {
    let cfg = search_config();
    let mut app = app(&cfg);
    assert!(app.typing(), "the input is open from the first frame");

    type_str(&mut app, "web");

    assert_eq!(app.filter.as_ref().map(|i| i.value()), Some("web"));
    let texts: Vec<String> = app
        .rows
        .rows
        .iter()
        .map(|row| tree::row_text(&app.snap, &app.worktrees, row, true))
        .collect();
    assert!(
        texts.iter().all(|t| t.contains("web")),
        "only web's rows survive: {texts:?}"
    );
}

/// The search preset is fzf's own keys: with a query typed, ctrl-n and ctrl-p
/// move down and up through what it left, as do ctrl-j / ctrl-k and the arrows, none of
/// them typed into the query; Enter goes to the row under the cursor.
#[test]
fn the_search_preset_moves_with_fzfs_keys_and_enter_goes() {
    let cfg = search_config();
    let mut app = app(&cfg);
    type_str(&mut app, "web");
    let start = app.sel;
    let next = step(start, app.rows.rows.len(), 1);
    for (down, up) in [
        (ctrl('n'), ctrl('p')),
        (ctrl('j'), ctrl('k')),
        (key(KeyCode::Down), key(KeyCode::Up)),
    ] {
        app.on_key(down);
        assert_eq!(app.sel, next, "{down:?} moves down one row");
        app.on_key(up);
        assert_eq!(app.sel, start, "{up:?} moves back up");
    }
    assert_eq!(app.filter.as_ref().map(|i| i.value()), Some("web"));

    cursor_to_pane(&mut app, "%221");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Jump {
            session: "$2".to_string(),
            window: "@22".to_string(),
            pane: "%221".to_string(),
        })
    );
}

/// A chord beyond fzf's own keys does nothing in the search preset: no overlay,
/// no menu, no move, no change to the depth or the layout, nothing typed, and the popup
/// stays open. The cursor is on an agent row.
///
/// ctrl-Left and ctrl-Right are not among them: unbound, they reach the query line, which
/// takes them for Left and Right, as it takes the arrows. They do not change the depth.
#[test]
fn the_search_presets_removed_chords_do_nothing() {
    let cfg = search_config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");
    let ctrl_key = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
    for sideways in [ctrl_key(KeyCode::Left), ctrl_key(KeyCode::Right)] {
        let (depth, layout) = (app.depth, app.layout);
        app.on_key(sideways);
        assert_eq!((app.depth, app.layout), (depth, layout), "{sideways:?}");
        assert!(!app.overlay.is_open(), "{sideways:?}");
    }
    cursor_to_pane(&mut app, "%221");
    for gone in [
        ctrl('o'),
        ctrl('t'),
        ctrl('r'),
        ctrl('x'),
        key(KeyCode::Tab),
        key(KeyCode::BackTab),
        ctrl_key(KeyCode::Up),
        ctrl_key(KeyCode::Down),
        ctrl('/'),
    ] {
        let (sel, depth, layout) = (app.sel, app.depth, app.layout);
        assert_eq!(app.on_key(gone), None, "{gone:?}");
        assert!(!app.overlay.is_open(), "{gone:?} opened {:?}", app.overlay);
        assert!(!app.quit, "{gone:?} quit");
        assert_eq!(
            (app.sel, app.depth, app.layout),
            (sel, depth, layout),
            "{gone:?}"
        );
        assert_eq!(app.filter.as_ref().map(|i| i.value()), Some(""), "{gone:?}");
    }
}

/// Options and help have no key in the search preset, but `[keys]` still gives them one,
/// and then that key opens the overlay itself; options is not a menu there.
#[test]
fn a_keys_entry_still_opens_help_and_options_in_the_search_preset() {
    let mut cfg = search_config();
    cfg.keys
        .bindings
        .insert("help".to_string(), KeySpec::One("ctrl+/".to_string()));
    cfg.keys
        .bindings
        .insert("options".to_string(), KeySpec::One("ctrl+o".to_string()));
    let mut app = app(&cfg);
    assert!(app.key_diagnostics.is_empty(), "{:?}", app.key_diagnostics);

    app.on_key(ctrl('/'));
    assert!(
        matches!(app.overlay, Overlay::Help(_)),
        "the help overlay: {:?}",
        app.overlay
    );
    app.on_key(key(KeyCode::Esc));
    assert!(!app.overlay.is_open());

    app.on_key(ctrl('o'));
    assert!(
        matches!(app.overlay, Overlay::Settings(_, _)),
        "the options overlay, not a menu: {:?}",
        app.overlay
    );
}

#[test]
fn the_modal_presets_o_opens_the_options_overlay_outright() {
    let cfg = config();
    let mut app = app(&cfg);

    app.on_key(ch('o'));

    assert!(matches!(app.overlay, Overlay::Settings(_, _)));
}

#[test]
fn esc_quits_the_search_preset_rather_than_clearing_its_input() {
    let cfg = search_config();
    let mut app = app(&cfg);
    type_str(&mut app, "web");

    app.on_key(key(KeyCode::Esc));

    assert!(app.quit);
}

/// The search preset's hint line is its whole keyboard, and the mouse's menu: exactly these
/// six, in this order, what typing a label does second.
#[test]
fn the_search_presets_hint_line_is_type_move_go_menu_and_quit() {
    let cfg = search_config();
    let app = app(&cfg);

    let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, m)| ((*k).to_string(), (*m).to_string()))
            .collect()
    };
    assert_eq!(
        app.hint_line(),
        pairs(&[
            ("type", "filter (fzf syntax)"),
            ("4a", "session 4 row a"),
            ("\u{2191}/\u{2193} ctrl-n/p", "move"),
            ("Enter", "go"),
            ("right-click", "menu"),
            ("Esc", "quit"),
        ])
    );
}

/// Enter with nothing under the cursor says why in one line, and names the key that makes a
/// session only when one is bound: `n` in the modal preset, a `[keys]` entry's own key, and
/// none in the search preset, where `n` is a letter of the query and new session has no key.
#[test]
fn enter_on_an_empty_list_names_the_key_that_makes_a_session_only_when_one_is_bound() {
    // `[keys]` entries, as (action, key).
    type Bindings = &'static [(&'static str, &'static str)];
    let cases: [(Config, Bindings, &str); 4] = [
        (config(), &[], "nothing here; n creates a session"),
        (
            config(),
            &[("new_session", "N")],
            "nothing here; N creates a session",
        ),
        (search_config(), &[], "nothing here"),
        (
            search_config(),
            &[("new_session", "ctrl+g")],
            "nothing here; ctrl-g creates a session",
        ),
    ];
    for (mut cfg, bindings, said) in cases {
        for (action, key) in bindings {
            cfg.keys
                .bindings
                .insert((*action).to_string(), KeySpec::One((*key).to_string()));
        }
        let mut app = app(&cfg);
        if app.preset == KeysPreset::Modal {
            app.on_key(ch('/'));
        }
        type_str(&mut app, "zzzzzz");
        assert!(app.current().is_none(), "{bindings:?}: nothing is listed");

        assert_eq!(app.on_key(key(KeyCode::Enter)), None, "{bindings:?}");
        assert_eq!(app.live_toast(), Some(said), "{bindings:?}");
        assert!(!app.quit, "{bindings:?}");
    }
}

/// A `[keys]` entry for moving puts its own keys on the search preset's hint line, as it does
/// in `?`. The line does not say `↑/↓ ctrl-n/p` whatever is bound: after a rebind ctrl-n and
/// ctrl-p move nothing, so the line names the keys the help's key column names, down then
/// up, the way the modal preset's line does; the preset's own keys keep the line's own words.
#[test]
fn a_rebound_move_key_shows_on_the_search_presets_hint_line() {
    let cases: [(&[(&str, &str)], &str); 3] = [
        (&[], "\u{2191}/\u{2193} ctrl-n/p"),
        (
            &[("move_down", "ctrl+d"), ("move_up", "ctrl+u")],
            "ctrl-d/ctrl-u",
        ),
        (&[("move_down", "ctrl+d")], "ctrl-d/Up"),
    ];
    for (bindings, keys) in cases {
        let mut cfg = search_config();
        for (action, key) in bindings {
            cfg.keys
                .bindings
                .insert((*action).to_string(), KeySpec::One((*key).to_string()));
        }
        let app = app(&cfg);
        assert_eq!(
            app.hint_line()[2],
            (keys.to_string(), "move".to_string()),
            "{bindings:?}"
        );
    }
}

// -------------------------------------------------------------- attention

#[test]
fn an_agent_that_has_been_looked_at_stops_needing_you() {
    let cfg = config();
    let mut snap = snapshot();
    // infra's blocked agent is looked at and goes idle; only codex is left hot.
    let infra = snap
        .agents
        .iter()
        .position(|a| a.pane_id == "%341")
        .expect("infra's blocked agent");
    snap.agents[infra].state = Some(AgentState::Idle);
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);

    assert_eq!(app.hot_count(), 1);
    // The cursor opens on your own row whatever is hot; `Tab` is what goes
    // to the one agent that is left, and stays there because it is the only one.
    assert_eq!(pane_at(&app), "%111");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%231");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%231");
}

/// With nothing that needs you, `Tab` walks every agent row, here all working, in the order
/// drawn, not the whole list by recency.
#[test]
fn with_every_agent_working_tab_walks_them_all_in_the_order_drawn() {
    let cfg = config();
    let mut snap = snapshot();
    for agent in &mut snap.agents {
        agent.state = Some(AgentState::Working);
        agent.seen = true;
    }
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);

    assert_eq!(app.hot_count(), 0);
    assert_eq!(pane_at(&app), "%111", "your own row, hot or not");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%121", "mobile's second agent, just below");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%221", "then web's first");
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(pane_at(&app), "%121", "and back up");
}

// ------------------------------------------------------------------ next up

/// The sample world with these agents given a state, each with whether it has been looked
/// at; `None` is an agent that has reported no state.
fn world_with(states: &[(&str, Option<AgentState>, bool)]) -> Snapshot {
    let mut snap = snapshot();
    for &(pane, state, seen) in states {
        let agent = snap
            .agents
            .iter_mut()
            .find(|a| a.pane_id == pane)
            .unwrap_or_else(|| panic!("no agent in {pane}"));
        agent.state = state;
        agent.seen = seen;
    }
    snap
}

/// An app over `snap`, with the tracked worktrees handed over as the run loop does.
fn app_over(cfg: &Config, snap: Snapshot) -> App<'_> {
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(cfg, theme, String::new(), snap);
    app.set_worktrees(worktrees());
    app
}

/// What the cursor is on, as a test reads it: a pane's id, or a header's session name.
fn on(app: &App) -> String {
    match app.current().expect("a row under the cursor") {
        Row::Pane(pane) => app.snap.panes[*pane].pane_id.clone(),
        Row::Session(space) => app.snap.spaces[*space].session_name.clone(),
        other => panic!("expected a pane or a header, got {other:?}"),
    }
}

/// Where the cursor is after each of `n` presses of `code`.
fn visited(app: &mut App, code: KeyCode, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            app.on_key(key(code));
            on(app)
        })
        .collect()
}

/// `Tab` is "next up": while an agent in view needs you it walks only
/// those, and with none it walks every agent row, one row a press,
/// wrapping. The sample world has two agents that need you (`%231` and `%341`, rows 10
/// and 18), two idle (`%121` and `%222`, rows 2 and 9) and three working (`%111`, `%221`
/// and `%311`, rows 1, 8 and 14).
#[test]
fn tab_walks_only_what_needs_you_while_anything_does() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 5),
        ["%231", "%341", "%231", "%341", "%231"],
        "the idle and the working agents around them are never a stop"
    );
    assert_eq!(
        visited(&mut app, KeyCode::BackTab, 3),
        ["%341", "%231", "%341"]
    );
}

#[test]
fn one_agent_that_needs_you_keeps_tab_on_it_whatever_else_is_idle_or_working() {
    let cfg = config();
    // infra's blocked agent is answered and working again: only web's codex is left.
    let mut app = app_over(
        &cfg,
        world_with(&[("%341", Some(AgentState::Working), true)]),
    );
    assert_eq!(visited(&mut app, KeyCode::Tab, 3), ["%231", "%231", "%231"]);
    assert_eq!(visited(&mut app, KeyCode::BackTab, 2), ["%231", "%231"]);
}

/// With nothing that needs you, `Tab` walks every agent row, idle and working alike, in the
/// order drawn, not the idle ones alone.
#[test]
fn with_nothing_that_needs_you_tab_walks_every_agent_row() {
    let cfg = config();
    // Web's codex is looked at, so it is idle now, and infra's blocked agent is working
    // again: three idle agents (rows 2, 9 and 10) and four working ones (1, 8, 14 and 18).
    let mut app = app_over(
        &cfg,
        world_with(&[
            ("%231", Some(AgentState::Done), true),
            ("%341", Some(AgentState::Working), true),
        ]),
    );
    assert_eq!(app.hot_count(), 0);
    assert_eq!(pane_at(&app), "%111", "your own row, working or not");
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 7),
        ["%121", "%221", "%222", "%231", "%311", "%341", "%111"],
        "every agent row, and round again"
    );
    assert_eq!(visited(&mut app, KeyCode::BackTab, 2), ["%341", "%311"]);
}

/// The sweep holds as states change under it, being the rows in the order drawn: an idle
/// agent that starts working is still a stop, one that blocks takes `Tab` to itself, and once
/// it is answered the sweep goes on from the cursor, after it.
#[test]
fn the_sweep_of_every_agent_holds_as_states_change_under_it() {
    let cfg = config();
    let calm = [
        ("%231", Some(AgentState::Done), true),
        ("%341", Some(AgentState::Working), true),
    ];
    let with = |more: &[(&'static str, Option<AgentState>, bool)]| {
        let mut states = calm.to_vec();
        states.extend_from_slice(more);
        world_with(&states)
    };
    let mut app = app_over(&cfg, with(&[]));
    assert_eq!(visited(&mut app, KeyCode::Tab, 1), ["%121"]);
    // mobile's review claude starts working: the next stop is the next agent row.
    app.set_snapshot(with(&[("%121", Some(AgentState::Working), true)]));
    assert_eq!(pane_at(&app), "%121", "the cursor stays on its row");
    assert_eq!(visited(&mut app, KeyCode::Tab, 1), ["%221"]);
    // web's gemini blocks: Tab goes to it and stays.
    app.set_snapshot(with(&[
        ("%121", Some(AgentState::Working), true),
        ("%222", Some(AgentState::Blocked), false),
    ]));
    assert_eq!(visited(&mut app, KeyCode::Tab, 2), ["%222", "%222"]);
    // Answered, the sweep goes on after it.
    app.set_snapshot(with(&[
        ("%121", Some(AgentState::Working), true),
        ("%222", Some(AgentState::Working), true),
    ]));
    assert_eq!(visited(&mut app, KeyCode::Tab, 2), ["%231", "%311"]);
}

#[test]
fn with_nothing_that_needs_you_tab_walks_unknown_and_stateless_agents_too() {
    let cfg = config();
    // Nothing needs you: every agent is a stop, working, unknown, and one that has
    // reported no state at all alike.
    let mut app = app_over(
        &cfg,
        world_with(&[
            ("%121", Some(AgentState::Unknown), false),
            ("%222", Some(AgentState::Working), true),
            ("%231", Some(AgentState::Working), true),
            ("%341", None, false),
        ]),
    );
    assert_eq!(app.hot_count(), 0);
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 8),
        [
            "%121", "%221", "%222", "%231", "%311", "%341", "%111", "%121"
        ],
        "every agent row, in the order drawn, and round again"
    );
    assert_eq!(visited(&mut app, KeyCode::BackTab, 2), ["%111", "%341"]);
}

/// Once the agent that needs you is looked at, it is idle and nothing needs you, so the next
/// `Tab` moves on to the next agent row of any kind.
#[test]
fn once_the_agent_that_needs_you_is_looked_at_the_next_tab_moves_on_to_the_next_agent() {
    let cfg = config();
    let mut app = app_over(
        &cfg,
        world_with(&[("%341", Some(AgentState::Working), true)]),
    );
    assert_eq!(visited(&mut app, KeyCode::Tab, 1), ["%231"]);

    // The popup's next refresh reads codex as looked at: a finished agent you have seen is
    // idle, like mobile's and web's other agents.
    app.set_snapshot(world_with(&[
        ("%341", Some(AgentState::Working), true),
        ("%231", Some(AgentState::Done), true),
    ]));
    assert_eq!(pane_at(&app), "%231", "the cursor stays on its row");
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 3),
        ["%311", "%341", "%111"],
        "on to infra's working agents below it, and round"
    );
}

/// The walk is over the rows in view: what a query hides is not there to be walked to, and
/// what needs you out of sight does not keep the agents in sight out of it.
#[test]
fn a_filter_narrows_the_walk_to_the_rows_in_view() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('/'));
    type_str(&mut app, "mobile");
    let agents_in_view: Vec<&str> = app
        .rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Pane(pane) => app.snap.panes[*pane]
                .agent
                .map(|_| app.snap.panes[*pane].pane_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        agents_in_view,
        ["%111", "%121"],
        "mobile's working agent and its idle one"
    );
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 2),
        ["%121", "%111"],
        "nothing in view needs you: both agents, the working one too"
    );
}

/// A folded header stands for its session's agents: while one in view needs you, a header
/// with such an agent is a stop; with none, every header with an agent in its session is.
#[test]
fn folded_headers_with_an_agent_are_stops_when_nothing_needs_you() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    // Codex looked at (idle), infra's blocked agent working: mobile and web each have an
    // idle agent, infra has only working ones, api and notes have none.
    let mut app = app_over(
        &cfg,
        world_with(&[
            ("%231", Some(AgentState::Done), true),
            ("%341", Some(AgentState::Working), true),
        ]),
    );
    assert_eq!(header_at(&app), "mobile", "your own session, folded");
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 4),
        ["web", "infra", "mobile", "web"],
        "infra's working agents make it a stop; api and notes have none"
    );
    assert_eq!(visited(&mut app, KeyCode::BackTab, 1), ["mobile"]);
}

#[test]
fn folded_headers_with_only_working_agents_are_stops_when_nothing_needs_you() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    // Every agent working: mobile, web and infra have some, api and notes have none.
    let every_agent_working: Vec<(&str, Option<AgentState>, bool)> =
        ["%111", "%121", "%221", "%222", "%231", "%311", "%341"]
            .into_iter()
            .map(|pane| (pane, Some(AgentState::Working), true))
            .collect();
    let mut app = app_over(&cfg, world_with(&every_agent_working));
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 4),
        ["web", "infra", "mobile", "web"],
        "the sessions with no agent at all are not a stop"
    );
}

/// Under a filter a folded header is found only by the words it draws, and stands for the agent whose
/// title it draws, not for all of its session's: infra holds a blocked agent and an idle one, and
/// `zebra` is in the title of both, but the header draws the blocked one's alone. Mobile's idle agent
/// has `zebra` in its title too, and its header draws none: it is not found.
#[test]
fn under_a_filter_a_folded_header_stands_only_for_the_agent_whose_title_it_draws() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    let mut snap = world_with(&[("%311", Some(AgentState::Idle), true)]);
    for agent in &mut snap.agents {
        match agent.pane_id.as_str() {
            "%311" => agent.pane_title = "zebra tracks".to_string(),
            "%341" => agent.pane_title = "zebra errors".to_string(),
            "%121" => agent.pane_title = "zebra herd".to_string(),
            _ => {}
        }
    }
    let mut app = app_over(&cfg, snap);
    let walk = |app: &App| -> Vec<String> {
        app.next_up_rows()
            .into_iter()
            .map(|at| match &app.rows.rows[at] {
                Row::Session(space) => app.snap.spaces[*space].session_name.clone(),
                other => panic!("a header, not {other:?}"),
            })
            .collect()
    };
    // Unfiltered, web's unseen finished agent and infra's blocked one need you: the only stops.
    assert_eq!(walk(&app), ["web", "infra"]);

    app.on_key(ch('/'));
    type_str(&mut app, "zebra");
    let shown: Vec<String> = app
        .rows
        .rows
        .iter()
        .map(|row| match row {
            Row::Session(space) => app.snap.spaces[*space].session_name.clone(),
            other => panic!("a header, not {other:?}"),
        })
        .collect();
    assert_eq!(shown, ["infra"], "found by the title its header draws");
    assert_eq!(walk(&app), ["infra"]);
    let infra = app
        .snap
        .spaces
        .iter()
        .position(|s| s.session_name == "infra");
    let standing: Vec<&str> = app.rows.stands_for[&infra.expect("infra")]
        .iter()
        .map(|&agent| app.snap.agents[agent].pane_id.as_str())
        .collect();
    assert_eq!(standing, ["%341"], "the blocked agent, not the idle ones");
}

/// A header found by its own words is a stop of `Tab` and a target of `Enter` exactly as it is with no
/// filter: nothing about the query changes where they land.
#[test]
fn a_header_found_by_its_name_lands_enter_and_tab_where_they_land_unfiltered() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    // Nothing needs you (web's codex looked at, infra's blocked agent working), so every header with
    // an agent in its session is a stop; api and notes have none.
    let world = || {
        world_with(&[
            ("%231", Some(AgentState::Done), true),
            ("%341", Some(AgentState::Working), true),
        ])
    };
    let names = ["mobile", "web", "infra", "api", "notes"];
    let stops = |app: &App| -> Vec<String> {
        app.next_up_rows()
            .into_iter()
            .map(|at| match &app.rows.rows[at] {
                Row::Session(space) => app.snap.spaces[*space].session_name.clone(),
                other => panic!("a header, not {other:?}"),
            })
            .collect()
    };
    let unfiltered = app_over(&cfg, world());
    let unfiltered_stops = stops(&unfiltered);
    assert_eq!(unfiltered_stops, ["mobile", "web", "infra"]);
    for name in names {
        let mut plain = app_over(&cfg, world());
        cursor_to_header(&mut plain, name);
        let unfiltered_enter = plain.on_key(key(KeyCode::Enter));
        // Enter on the session the popup was opened from only closes the popup.
        assert_eq!(
            matches!(unfiltered_enter, Some(Action::SwitchWorkspace { .. })),
            name != "mobile",
            "{name}: {unfiltered_enter:?}"
        );

        let mut app = app_over(&cfg, world());
        app.on_key(ch('/'));
        type_str(&mut app, &format!("^{name}"));
        assert_eq!(header_at(&app), name, "the filter found the header");
        assert_eq!(
            stops(&app),
            unfiltered_stops
                .iter()
                .filter(|stop| stop.as_str() == name)
                .cloned()
                .collect::<Vec<_>>(),
            "{name}: a stop exactly as it is unfiltered"
        );
        assert_eq!(app.on_key(key(KeyCode::Enter)), unfiltered_enter, "{name}");
    }
}

/// With no query too, a folded header stands only for the agents the Agents view shows: a view
/// hides an agent from the walk at every depth. Infra holds a blocked agent and an idle one; a view
/// that leaves out the blocked ones makes infra's header stand for the idle one alone, so it is no
/// stop while web's unseen finished agent still is.
#[test]
fn a_folded_header_does_not_stand_for_an_agent_the_view_hides() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    let walk = |snap: Snapshot| -> Vec<String> {
        let app = app_over(&cfg, snap);
        app.next_up_rows()
            .into_iter()
            .map(|at| match &app.rows.rows[at] {
                Row::Session(space) => app.snap.spaces[*space].session_name.clone(),
                other => panic!("a header, not {other:?}"),
            })
            .collect()
    };
    let world = || world_with(&[("%311", Some(AgentState::Idle), true)]);
    assert_eq!(walk(world()), ["web", "infra"], "the blocked agent counts");

    let mut hiding = world();
    hiding.view = crate::tui::agent_view::parse(Some(
        r#"{"source":"cli","filter":{"op":"not","filter":{"op":"eq","field":"status","value":"blocked"}}}"#,
    ))
    .expect("a query taw would install");
    assert!(hiding.view.is_some());
    assert_eq!(walk(hiding), ["web"], "the hidden blocked agent is no stop");
}

/// A row a configured `rows` template draws is found by the template's own words too, though the
/// default row has none of them.
#[test]
fn a_row_a_template_draws_is_found_by_the_words_of_the_template() {
    let mut snap = snapshot();
    snap.agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .expect("an agent")
        .tokens
        .insert("ticket".to_string(), "zorblat".to_string());
    let found = |cfg: &Config| -> Vec<String> {
        let mut app = app_over(cfg, snap.clone());
        app.on_key(ch('/'));
        type_str(&mut app, "zorblat");
        app.rows
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pane(pane) => Some(app.snap.panes[*pane].pane_id.clone()),
                _ => None,
            })
            .collect()
    };
    // Drawn the default way the row has no such word, and nothing is found.
    assert!(found(&config()).is_empty());

    let mut cfg = config();
    cfg.ui.sidebar =
        toml::from_str::<Config>("[ui.sidebar.agents]\nrows = [[\"agent\", \"$ticket\"]]\n")
            .unwrap()
            .ui
            .sidebar;
    assert_eq!(found(&cfg), ["%121"]);
}

/// The words of a template's row are matched in one run, as a title's are: `zorb` finds the row, and
/// `zrbt`, scattered over `zorblat`, finds nothing.
#[test]
fn a_templates_words_are_matched_in_one_run_not_scattered() {
    let mut snap = snapshot();
    snap.agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .expect("an agent")
        .tokens
        .insert("ticket".to_string(), "zorblat".to_string());
    let mut cfg = config();
    cfg.ui.sidebar =
        toml::from_str::<Config>("[ui.sidebar.agents]\nrows = [[\"agent\", \"$ticket\"]]\n")
            .unwrap()
            .ui
            .sidebar;
    let found = |query: &str| -> Vec<String> {
        let mut app = app_over(&cfg, snap.clone());
        app.on_key(ch('/'));
        type_str(&mut app, query);
        app.rows
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pane(pane) => Some(app.snap.panes[*pane].pane_id.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(found("zorb"), ["%121"]);
    assert!(found("zrbt").is_empty());
}

/// A row carries exactly the words its header draws: with a `[ui.sidebar.spaces] rows` template
/// that leaves the branch out, `release claude` finds none of mobile's rows, as the header shows
/// no `release` to read it from; drawn the default way (the control) it finds both.
#[test]
fn a_row_carries_the_words_a_spaces_template_draws_of_its_header() {
    let mut snap = snapshot();
    snap.spaces[0].git.as_mut().expect("git").branch = Some("release".to_string());
    let found = |cfg: &Config| -> usize {
        let mut app = app_over(cfg, snap.clone());
        app.on_key(ch('/'));
        type_str(&mut app, "release claude");
        app.rows
            .rows
            .iter()
            .filter(|row| match row {
                Row::Pane(pane) => app.snap.panes[*pane].session_id == "$1",
                _ => false,
            })
            .count()
    };
    assert_eq!(found(&config()), 2);

    let mut cfg = config();
    cfg.ui.sidebar = toml::from_str::<Config>("[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n")
        .unwrap()
        .ui
        .sidebar;
    assert_eq!(found(&cfg), 0);
    // A template that draws the branch carries it.
    cfg.ui.sidebar =
        toml::from_str::<Config>("[ui.sidebar.spaces]\nrows = [[\"workspace\", \"branch\"]]\n")
            .unwrap()
            .ui
            .sidebar;
    assert_eq!(found(&cfg), 2);
}

/// The panes the popup lists for `query` under `cfg` (the sidebar's TOML text, or none), in `snap`.
fn panes_found(snap: &Snapshot, sidebar: &str, query: &str) -> Vec<String> {
    let mut cfg = config();
    if !sidebar.is_empty() {
        cfg.ui.sidebar = toml::from_str::<Config>(sidebar).unwrap().ui.sidebar;
    }
    let mut app = app_over(&cfg, snap.clone());
    app.on_key(ch('/'));
    type_str(&mut app, query);
    app.rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Pane(pane) => Some(app.snap.panes[*pane].pane_id.clone()),
            _ => None,
        })
        .collect()
}

/// A row its template draws shows the template's words and none of the default row's: with
/// `rows = [["agent"]]` the window `zsh`, the state `working` and the title are not drawn, so they
/// find nothing, drawn the default way (the control) they find the row, and the agent's name, which
/// the template draws, and the session's, which the header above draws, still do.
#[test]
fn a_row_a_template_draws_is_not_found_by_the_default_words_it_leaves_out() {
    let mut snap = snapshot();
    let agent = snap
        .agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .expect("an agent");
    agent.pane_title = "Mend zorblat".to_string();
    let drawn = "[ui.sidebar.agents]\nrows = [[\"agent\"]]\n";
    // The words the default row draws: its window, its state and its title.
    for word in ["review", "idle", "zorblat"] {
        let control = panes_found(&snap, "", word);
        let templated = panes_found(&snap, drawn, word);
        assert!(
            control.contains(&"%121".to_string()),
            "{word}: the default row has the word: {control:?}"
        );
        assert!(
            !templated.contains(&"%121".to_string()),
            "{word}: the template does not draw it: {templated:?}"
        );
    }
    assert!(panes_found(&snap, drawn, "claude").contains(&"%121".to_string()));
    assert!(
        panes_found(&snap, drawn, "mobile").contains(&"%121".to_string()),
        "the session's name is on the header above"
    );
}

/// What the popup lists for `query` under `sidebar` (TOML text, or none), in `snap`: a header as
/// `# <session>`, a pane by its id, a parked worktree as `worktree <branch>`.
fn rows_found(snap: &Snapshot, sidebar: &str, query: &str) -> Vec<String> {
    let mut cfg = config();
    if !sidebar.is_empty() {
        cfg.ui.sidebar = toml::from_str::<Config>(sidebar).unwrap().ui.sidebar;
    }
    let mut app = app_over(&cfg, snap.clone());
    app.on_key(ch('/'));
    type_str(&mut app, query);
    app.rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Session(space) => Some(format!("# {}", app.snap.spaces[*space].session_name)),
            Row::Pane(pane) => Some(app.snap.panes[*pane].pane_id.clone()),
            Row::ClosedWorktree { record, .. } => {
                Some(format!("worktree {}", app.worktrees[*record].branch))
            }
            _ => None,
        })
        .collect()
}

/// A row is the template's when a template applies to it, whatever it draws: a spaces template of
/// `branch` draws a session with no git an empty line, which no word finds. The control in the
/// same test: the session that has a branch is found by it, and by its name drawn the default way.
#[test]
fn a_header_a_template_draws_empty_is_found_by_nothing() {
    let mut snap = snapshot();
    snap.spaces[4].git = None;
    let branch = "[ui.sidebar.spaces]\nrows = [[\"branch\"]]\n";
    assert!(
        rows_found(&snap, "", "notes").contains(&"# notes".to_string()),
        "drawn by default the name finds it"
    );
    assert!(
        rows_found(&snap, branch, "invoices").contains(&"# web".to_string()),
        "the control: a header that draws its branch is found by it"
    );
    let named = rows_found(&snap, branch, "notes");
    assert!(
        !named.contains(&"# notes".to_string()),
        "the header draws an empty line, so its name finds nothing: {named:?}"
    );
}

/// The same for an agent row: with an agents template of `$task` and the token unset, the row
/// draws an empty line, so its kind, its state, its window and its title find nothing. The control
/// in the same test: the token set, the row is found by it.
#[test]
fn an_agent_row_a_template_draws_empty_is_found_by_nothing() {
    let mut snap = snapshot();
    let agent = snap
        .agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .expect("an agent");
    agent.pane_title = "Mend zorblat".to_string();
    let task = "[ui.sidebar.agents]\nrows = [[\"$task\"]]\n";
    for word in ["claude", "idle", "review", "zorblat"] {
        assert!(
            panes_found(&snap, "", word).contains(&"%121".to_string()),
            "{word}: drawn by default the row is found by it"
        );
    }
    let mut set = snap.clone();
    set.agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .expect("an agent")
        .tokens
        .insert("task".to_string(), "polish".to_string());
    assert_eq!(
        panes_found(&set, task, "polish"),
        ["%121"],
        "the control: the token set, the row is found by it"
    );
    for word in ["claude", "idle", "review", "zorblat"] {
        let found = panes_found(&snap, task, word);
        assert!(
            !found.contains(&"%121".to_string()),
            "{word}: the row draws an empty line: {found:?}"
        );
    }
}

/// Under a header a template draws, a row no template draws (a plain shell, a parked worktree) has
/// no session name of its own either: the header draws the name only if its template does. With
/// `rows = [["branch"]]`, web's name is on no line of the screen, so `web` finds nothing at the
/// default depth of the test's config; its branch, which the header draws, finds the session and
/// its rows.
#[test]
fn a_row_under_a_template_header_does_not_keep_its_sessions_name() {
    let snap = snapshot();
    let branch = "[ui.sidebar.spaces]\nrows = [[\"branch\"]]\n";
    let by_branch = rows_found(&snap, branch, "invoices");
    for want in ["# web", "worktree feat/pdf"] {
        assert!(
            by_branch.contains(&want.to_string()),
            "the control, {want}: {by_branch:?}"
        );
    }
    assert!(by_branch.len() > 2, "its plain shells too: {by_branch:?}");
    let named = rows_found(&snap, branch, "web");
    assert!(
        named.is_empty(),
        "web is drawn nowhere, so it finds nothing: {named:?}"
    );
    // Drawn the default way (the control), the name finds the session, its shells and its worktree.
    let default = rows_found(&snap, "", "web");
    assert!(
        default.contains(&"worktree feat/pdf".to_string()),
        "{default:?}"
    );
    // A header that draws the name keeps it for its rows.
    let drawn = rows_found(
        &snap,
        "[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n",
        "web",
    );
    assert!(
        drawn.contains(&"worktree feat/pdf".to_string()),
        "{drawn:?}"
    );
}

/// The rows of the popup after `query` is typed under `sidebar` (flat when `flat`), as
/// `(row, scored)`: whether each row has a match of its own, the cursor and the lit text going by
/// it. A row shown only because its header matched has none.
fn rows_scored(snap: &Snapshot, sidebar: &str, flat: bool, query: &str) -> Vec<(Row, bool)> {
    let mut cfg = config();
    if !sidebar.is_empty() {
        cfg.ui.sidebar = toml::from_str::<Config>(sidebar).unwrap().ui.sidebar;
    }
    let mut app = app_over(&cfg, snap.clone());
    if flat {
        app.on_key(ch('f'));
        assert!(app.flat_now());
    }
    app.on_key(ch('/'));
    type_str(&mut app, query);
    app.rows
        .rows
        .iter()
        .cloned()
        .zip(app.rows.scores.iter().map(Option::is_some))
        .collect()
}

/// A header that matched shows all its rows, but a row of its own is found only by what that row
/// draws. Web's header draws its branch `web-v2` (`rows = [["branch"]]`), so `web` finds the
/// header and, under it, no shell, no parked worktree and no agent: the tree draws the session's
/// name on none of them. The controls in the same test: the header is found by `web`, the branch
/// finds the same, the default header lets the rows keep the name, and the flat list, which has no
/// header and draws the name on each row, finds them by it.
#[test]
fn a_row_under_a_matched_template_header_does_not_keep_its_sessions_name() {
    let mut snap = snapshot();
    snap.spaces[1].git.as_mut().expect("git").branch = Some("web-v2".to_string());
    let branch = "[ui.sidebar.spaces]\nrows = [[\"branch\"]]\n";
    let kids = |found: &[(Row, bool)]| -> Vec<(Row, bool)> {
        found
            .iter()
            .filter(|(row, _)| !matches!(row, Row::Session(_)))
            .cloned()
            .collect()
    };
    let in_web = |snap: &Snapshot, row: &Row| match row {
        Row::Pane(pane) => snap.panes[*pane].session_id == "$2",
        Row::ClosedWorktree { space, .. } => *space == 1,
        _ => false,
    };
    let found = rows_scored(&snap, branch, false, "web");
    let header = found
        .iter()
        .find(|(row, _)| matches!(row, Row::Session(1)))
        .expect("the header of web is listed");
    assert!(
        header.1,
        "the control: the branch finds the header: {found:?}"
    );
    let shown: Vec<_> = kids(&found)
        .into_iter()
        .filter(|(row, _)| in_web(&snap, row))
        .collect();
    assert!(
        shown
            .iter()
            .any(|(row, _)| matches!(row, Row::ClosedWorktree { .. }))
            && shown.iter().any(|(row, _)| matches!(row, Row::Pane(_))),
        "the control: its worktree and its panes are shown under the matched header: {shown:?}"
    );
    assert!(
        shown.len() >= 6 && shown.iter().all(|(_, scored)| !scored),
        "no row of web has a match of its own from the name: {shown:?}"
    );
    // Drawn the default way the name is on the header, and the rows keep it.
    let default = rows_scored(&snap, "", false, "web");
    assert!(
        default
            .iter()
            .any(|(row, scored)| *scored && matches!(row, Row::ClosedWorktree { .. })),
        "the control: the default header lets the worktree keep the name: {default:?}"
    );
    // The flat list draws the name on the row, so the row has it.
    let flat = rows_scored(&snap, branch, true, "web");
    assert!(
        flat.iter()
            .any(|(row, scored)| *scored && matches!(row, Row::Pane(_)) && in_web(&snap, row)),
        "the control: a flat row draws its session's name: {flat:?}"
    );
}

/// A header a `[ui.sidebar.spaces] rows` template draws is found by what the template draws: with
/// `rows = [["workspace"]]` the branch `release` is not on the screen, so it finds nothing; drawn
/// the default way it finds the session, and a template with the `branch` token finds it again.
#[test]
fn a_header_a_template_draws_is_not_found_by_the_words_it_leaves_out() {
    let mut snap = snapshot();
    snap.spaces[0].git.as_mut().expect("git").branch = Some("release".to_string());
    let sessions = |sidebar: &str| -> usize {
        let mut cfg = config();
        if !sidebar.is_empty() {
            cfg.ui.sidebar = toml::from_str::<Config>(sidebar).unwrap().ui.sidebar;
        }
        let mut app = app_over(&cfg, snap.clone());
        app.on_key(ch('/'));
        type_str(&mut app, "release");
        app.rows
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Session(_)))
            .count()
    };
    assert_eq!(sessions(""), 1);
    assert_eq!(
        sessions("[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n"),
        0
    );
    assert_eq!(
        sessions("[ui.sidebar.spaces]\nrows = [[\"workspace\", \"branch\"]]\n"),
        1
    );
}

/// The flat list is unchanged (it puts what needs you first, idle above working) and `Tab`
/// works by the same rule there as everywhere else: what needs you, else every agent, in the
/// order drawn.
#[test]
fn in_the_flat_list_tab_walks_by_the_same_rule_in_the_order_drawn() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('f'));
    assert!(app.flat_now());
    // Drawn most urgent first: infra's blocked agent, web's codex, then the idle ones and
    // the working ones. The two that need you are the walk.
    assert_eq!(visited(&mut app, KeyCode::Tab, 3), ["%341", "%231", "%341"]);

    let mut app = app_over(
        &cfg,
        world_with(&[
            ("%231", Some(AgentState::Done), true),
            ("%341", Some(AgentState::Working), true),
        ]),
    );
    app.on_key(ch('f'));
    assert_eq!(pane_at(&app), "%111", "the cursor keeps your row");
    // Nothing needs you now: every agent is the walk, in the flat list's order, idle first
    // (newest first: %231, %121, %222), then working (%221, %111, %311, %341), from your own
    // row down and round.
    assert_eq!(
        visited(&mut app, KeyCode::Tab, 4),
        ["%311", "%341", "%231", "%121"]
    );
}

// ------------------------------------------------------------ agent views

/// The sample world with `json` installed in `@agent_wizard_view`.
fn snapshot_with_view(json: &str) -> Snapshot {
    let mut snap = snapshot();
    snap.view = crate::tui::agent_view::parse(Some(json)).expect("a query taw would install");
    assert!(snap.view.is_some(), "{json} should install a projection");
    snap
}

/// The cursor may never sit on a row the view took out. The most urgent agent is infra's
/// blocked claude; a view that keeps only codex leaves it no row, so the cursor settles on
/// a drawn row instead and `Tab` walks only what is left.
#[test]
fn the_cursor_lands_on_a_visible_row_when_a_view_hides_the_most_urgent_agent() {
    let cfg = config();
    let snap = snapshot_with_view(
        r#"{"source":"cli","label":"codex","filter":{"op":"eq","field":"agent","value":"codex"}}"#,
    );
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snap);
    app.set_worktrees(worktrees());

    let visible: Vec<String> = app
        .rows
        .rows
        .iter()
        .map(|row| tree::row_text(&app.snap, &app.worktrees, row, false))
        .collect();
    assert!(
        !visible.iter().any(|row| row.contains("claude")),
        "the view hid every claude: {visible:?}"
    );
    assert!(
        app.sel < app.rows.rows.len(),
        "the cursor is at {} of {} rows",
        app.sel,
        app.rows.rows.len()
    );
    assert!(app.current().is_some(), "the cursor is on a drawn row");
    // The chip still says a projection is installed, hidden agents and all.
    assert_eq!(app.view_label(), Some("codex"));

    // Tab walks the agents that need you — and only the one the view kept.
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        pane_at(&app),
        "%231",
        "web's codex, not infra's blocked claude"
    );
    app.on_key(key(KeyCode::Tab));
    assert_eq!(pane_at(&app), "%231", "it is the only one left to walk");
}

// ------------------------------------------------------------ the cursor on opening

#[test]
fn the_cursor_opens_on_the_pane_the_popup_was_opened_from() {
    let cfg = config();
    let app = app(&cfg);
    assert_eq!(app.own_row(), Some(1));
    assert_eq!(app.sel, 1);
    assert_eq!(pane_at(&app), "%111");
}

#[test]
fn the_cursor_opens_on_the_header_when_the_own_pane_is_folded() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    let app = app(&cfg);
    assert_eq!(header_at(&app), "mobile");
}

#[test]
fn a_number_moves_the_cursor_to_that_session_and_nothing_else() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(app.on_key(ch('3')), None, "a number never leaves the popup");
    assert_eq!(header_at(&app), "infra");
    assert!(!app.quit);
    app.on_key(ch('9'));
    assert_eq!(
        header_at(&app),
        "infra",
        "a number with no session does nothing"
    );
    assert!(app.live_toast().is_some());
}

/// The modal filter runs the movement keys and swallows the rest, so a `select_session`
/// rebound to a key the filter cannot type — the digits themselves are text there — has to
/// be one of the movements it runs. Otherwise the binding works on the tree and silently
/// does nothing the moment a query is open. The search preset's filter is always open, so
/// the same override has to move the cursor there too, straight off the keymap rather than
/// through the modal filter's own swallow-the-rest logic.
#[test]
fn a_non_text_select_session_key_moves_the_cursor_inside_the_filter() {
    for cfg in [config(), search_config()] {
        let mut cfg = cfg;
        let preset = cfg.keys.preset;
        cfg.keys.bindings.insert(
            "select_session".to_string(),
            KeySpec::Many(vec![
                "ctrl+1".to_string(),
                "ctrl+2".to_string(),
                "ctrl+3".to_string(),
            ]),
        );
        let mut app = app(&cfg);
        if preset == KeysPreset::Modal {
            app.on_key(ch('/'));
        }
        assert!(app.typing(), "{preset:?}: the filter is open");

        assert_eq!(
            app.on_key(ctrl('3')),
            None,
            "{preset:?}: a number never leaves the popup"
        );

        assert_eq!(header_at(&app), "infra", "{preset:?}");
        assert!(
            app.typing(),
            "{preset:?}: and the filter is still open behind it"
        );
    }
}

#[test]
fn labels_follow_tmux_order_and_survive_a_filter() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(app.row_label(0).as_deref(), Some("1"));
    assert_eq!(app.row_label(13).as_deref(), Some("3"));
    assert_eq!(
        app.row_label(1),
        None,
        "agent rows carry no label in the tree"
    );
    app.on_key(ch('/'));
    type_str(&mut app, "infra");
    let header = app
        .rows
        .rows
        .iter()
        .position(|r| matches!(r, Row::Session(2)))
        .expect("infra's header survived its own name");
    assert_eq!(
        app.row_label(header).as_deref(),
        Some("3"),
        "filtering does not renumber"
    );
}

#[test]
fn tab_walks_the_rows_that_need_you_top_to_bottom_and_wraps() {
    let cfg = config();
    let mut app = app(&cfg);
    // From your own row (mobile's claude, row 1): codex (row 10, done unseen) comes first
    // in the order drawn, then infra's blocked claude (row 18), then back to codex.
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.sel, 10);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.sel, 18);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.sel, 10);
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(app.sel, 18);
}

#[test]
fn tab_from_a_header_walks_down_through_that_session_first() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3')); // infra's header, row 13
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        app.sel, 18,
        "infra's own blocked agent, not web's codex above"
    );
}

#[test]
fn folded_headers_with_a_hot_agent_are_tab_stops() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Sessions;
    let mut app = app(&cfg);
    // Rows: 0 mobile*, 1 web, 2 infra, 3 api, 4 notes. Own row is mobile.
    app.on_key(key(KeyCode::Tab));
    assert_eq!(header_at(&app), "web");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(header_at(&app), "infra");
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        header_at(&app),
        "web",
        "wraps past api and notes, which need nothing"
    );
}

#[test]
fn enter_on_a_header_is_switch_client_only() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    let action = app.on_key(key(KeyCode::Enter));
    assert_eq!(
        action,
        Some(Action::SwitchWorkspace {
            session: "$3".to_string()
        })
    );
}

#[test]
fn enter_on_your_own_row_just_closes_whatever_needs_you_elsewhere() {
    let cfg = config();
    let mut app = app(&cfg);
    // The fixture has a blocked agent in infra and a done one in web: plenty to walk to.
    let mut snap = app.snap.clone();
    snap.previous_session = Some("$4".to_string());
    app.set_snapshot(snap);
    assert_eq!(app.enter_meaning(), "go");
    let action = app.on_key(key(KeyCode::Enter));
    assert_eq!(action, None, "nothing for tmux: you are already there");
    assert!(app.quit);
}

#[test]
fn enter_on_a_folded_header_reached_by_tab_is_still_the_session_as_you_left_it() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('h')); // all → agents: still drawn
    app.on_key(ch('h')); // agents → sessions: the headers fold their agents
    app.on_key(key(KeyCode::Tab)); // onto the header of a session that needs you
    let Some(Row::Session(space)) = app.current().cloned() else {
        panic!("Tab lands on a folded header at the sessions depth");
    };
    let session = app.snap.spaces[space].session_id.clone();
    let action = app.on_key(key(KeyCode::Enter));
    assert_eq!(action, Some(Action::SwitchWorkspace { session }));
}

#[test]
fn the_hint_line_says_enter_go_on_every_row() {
    let cfg = config();
    let mut app = app(&cfg);
    for _ in 0..app.rows.rows.len() {
        let hint = app.hint_line();
        let enter = hint
            .iter()
            .find(|(k, _)| k == "Enter")
            .expect("Enter is on the hint line");
        assert_eq!(enter.1, "go");
        app.on_key(ch('j'));
    }
}

#[test]
fn enter_elsewhere_is_plain_go() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('j'));
    assert_eq!(app.enter_meaning(), "go");
    app.on_key(ch('3'));
    assert_eq!(app.enter_meaning(), "go");
}

#[test]
fn depth_changes_keep_the_row_and_expanding_returns_to_the_leaf() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    app.on_key(key(KeyCode::Tab)); // infra's blocked claude
    assert_eq!(pane_at(&app), "%341");
    app.on_key(ch('h')); // agents: still drawn
    assert_eq!(pane_at(&app), "%341");
    app.on_key(ch('h')); // sessions: folded → its header
    assert_eq!(header_at(&app), "infra");
    app.on_key(ch('l')); // back on the agent
    assert_eq!(pane_at(&app), "%341");
    app.on_key(ch('l'));
    assert_eq!(pane_at(&app), "%341");
}

#[test]
fn a_header_you_moved_to_yourself_stays_a_header_through_a_depth_change() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    app.on_key(key(KeyCode::Tab));
    // Back up onto infra's header by hand: five rows at the `all` depth, 18 up to 13.
    for _ in 0..5 {
        app.on_key(ch('k'));
    }
    assert_eq!(header_at(&app), "infra");
    app.on_key(ch('h'));
    app.on_key(ch('h'));
    app.on_key(ch('l'));
    assert_eq!(header_at(&app), "infra");
}

#[test]
fn flat_toggle_keeps_the_row() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    app.on_key(key(KeyCode::Tab));
    app.on_key(ch('f'));
    assert_eq!(pane_at(&app), "%341");
    app.on_key(ch('f'));
    assert_eq!(pane_at(&app), "%341");
}

#[test]
fn the_hint_line_leads_with_the_numbers_and_says_enter_go() {
    let cfg = config();
    let app = app(&cfg);
    let hints = app.hint_line();
    assert_eq!(hints[0], ("1-9".to_string(), "session".to_string()));
    assert_eq!(hints[1], ("a-e".to_string(), "rows".to_string()));
    assert_eq!(hints[2].0, "Tab");
    assert_eq!(hints[3], ("Enter".to_string(), "go".to_string()));
    assert!(
        !hints.iter().any(|(k, _)| k == "q"),
        "quit left the hint line"
    );
}

/// The hint line names the number keys exactly while they are keys, as it names the letters
/// while they show. With `/` open a digit is text for the query, and typing a number works
/// as pressing it does, so the numbers still
/// work and the line says what typing a label does, `4a session 4 row a`; Esc gives the tree's
/// `1-9 session` back. With `[ui] labels` off there is none. `number_works` is what decides it,
/// number by number.
#[test]
fn the_hint_line_names_the_numbers_only_while_they_are_keys() {
    let numbers = ("1-9".to_string(), "session".to_string());
    let typed = ("4a".to_string(), "session 4 row a".to_string());
    let cfg = config();
    let mut app = app(&cfg);
    assert!((1..=9).all(|n| app.number_works(n)));
    assert_eq!(app.hint_line()[0], numbers);

    app.on_key(ch('/'));
    assert!(app.typing());
    assert!((1..=9).all(|n| app.number_works(n)));
    assert_eq!(app.hint_line()[0], typed, "{:?}", app.hint_line());
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.hint_line()[0], numbers, "Esc, and they are keys again");

    let mut off = config();
    off.ui.labels = false;
    let mut app = self::app(&off);
    assert!(!app.number_works(1));
    assert!(!app.hint_line().contains(&numbers));
    app.on_key(ch('/'));
    assert!(!app.number_works(1));
    assert!(!app.hint_line().contains(&typed), "{:?}", app.hint_line());
}

/// The search preset's hint line names the number keys exactly while they act: its digits are
/// text for the query and nothing is bound, so it names none, but
/// typing a label works and the line says so right after `type`; with
/// `[keys] select_session` bound its keymap is asked before its query, the keys act, and the hint
/// names them, right after `type`, before what typing does.
#[test]
fn the_search_presets_hint_names_the_number_keys_only_while_a_binding_makes_them_act() {
    let numbers = ("1-9".to_string(), "session".to_string());
    let typed = ("4a".to_string(), "session 4 row a".to_string());
    let mut search = config();
    search.keys.preset = KeysPreset::Search;
    let app = self::app(&search);
    assert!(app.numbers_work(), "typing a number works");
    assert!(!app.hint_line().contains(&numbers), "{:?}", app.hint_line());
    assert_eq!(app.hint_line()[1], typed, "{:?}", app.hint_line());

    let mut bound = config();
    bound.keys.preset = KeysPreset::Search;
    bound.keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many((1..=9).map(|n| n.to_string()).collect()),
    );
    let app = self::app(&bound);
    assert!(app.numbers_work());
    let hints = app.hint_line();
    assert_eq!(hints[0].0, "type", "{hints:?}");
    assert_eq!(hints[1], numbers, "{hints:?}");
    assert_eq!(hints[2], typed, "{hints:?}");
}

/// While the modal filter has focus every plain character is text for the query, so `j`, `k`,
/// `/` and `?` do not act there, and the hint line names only what does, as the filter's hint
/// does: what typing a label does, Esc clears,
/// Tab goes next up, the arrows move, Enter goes. Esc gives the tree's line back.
#[test]
fn the_filters_hint_line_names_only_the_keys_that_work_in_it() {
    let entry = |key: &str, meaning: &str| (key.to_string(), meaning.to_string());
    let cfg = config();
    let mut app = app(&cfg);
    let tree = app.hint_line();
    app.on_key(ch('/'));
    assert!(app.typing());
    assert_eq!(
        app.hint_line(),
        vec![
            entry("4a", "session 4 row a"),
            entry("Esc", "clear"),
            entry("Tab", "next up"),
            entry("\u{2191}/\u{2193}", "move"),
            entry("Enter", "go"),
        ]
    );
    app.on_key(ch('t'));
    assert_eq!(
        app.hint_line()[1],
        entry("Esc", "clear"),
        "with a query typed"
    );
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.hint_line(), tree, "Esc: the tree's line again");

    // Next up named by a key of its that is not text: `z` is text for the query, Tab is not; and
    // with `z` alone next up does nothing from the filter, and the line leaves it out.
    for (keys, named) in [(vec!["z", "tab"], Some("Tab")), (vec!["z"], None)] {
        let mut rebound = config();
        rebound.keys.bindings.insert(
            "next_attention".to_string(),
            KeySpec::Many(keys.iter().map(|k| k.to_string()).collect()),
        );
        let mut app = self::app(&rebound);
        app.on_key(ch('/'));
        let next_up = app
            .hint_line()
            .into_iter()
            .find(|(_, meaning)| meaning == "next up")
            .map(|(key, _)| key);
        assert_eq!(next_up.as_deref(), named, "next_attention = {keys:?}");
    }

    // With moving rebound to j and k alone, both text in the filter, the arrows are bound to
    // nothing and move nothing there; the line names fzf's ctrl-p and ctrl-n, which always do.
    let mut letters = config();
    for (action, key) in [("move_down", "j"), ("move_up", "k")] {
        letters
            .keys
            .bindings
            .insert(action.to_string(), KeySpec::One(key.to_string()));
    }
    let mut app = self::app(&letters);
    app.on_key(ch('/'));
    let hints = app.hint_line();
    assert!(hints.contains(&entry("ctrl-p/n", "move")), "{hints:?}");
    assert!(
        !hints.iter().any(|(key, _)| key.contains('\u{2191}')),
        "{hints:?}"
    );
}

#[test]
fn hot_count_ignores_a_finished_agent_you_have_looked_at() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(app.hot_count(), 2);
    let mut snap = app.snap.clone();
    snap.agents
        .iter_mut()
        .find(|a| a.pane_id == "%231")
        .expect("web's codex")
        .seen = true;
    app.set_snapshot(snap);
    assert_eq!(app.hot_count(), 1);
}

/// An app opened from mobile's nvim (`%131`): a plain pane, which the `agents` depth
/// does not draw.
fn opened_from_a_plain_shell(cfg: &Config) -> App<'_> {
    let mut snap = snapshot();
    snap.own_pane = Some("%131".to_string());
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    App::new(cfg, theme, String::new(), snap)
}

/// mobile's idle claude, which the flat list's attention order puts first of mobile's
/// rows (idle above working).
fn jump_to_mobile_idle_claude() -> Option<Action> {
    Some(Action::Jump {
        session: "$1".to_string(),
        window: "@12".to_string(),
        pane: "%121".to_string(),
    })
}

/// A flat list has no headers, so with your own pane not drawn the cursor opens on the
/// first row of your session — wherever the flat sort put it. That row is another pane's
/// agent, not where you are: Enter goes to it (your own row is your
/// pane's row or your session's header, and Enter goes to the row under the cursor).
#[test]
fn in_a_flat_list_opened_from_a_plain_shell_enter_on_the_opening_row_goes_to_that_agent() {
    let mut cfg = config();
    cfg.ui.layout = Layout::Flat;
    cfg.ui.depth = Depth::Agents;
    let mut app = opened_from_a_plain_shell(&cfg);

    let first = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(p) if app.snap.panes[*p].session_id == "$1"))
        .expect("mobile still has a row");
    assert_eq!(app.home_row(), Some(first));
    assert_eq!(app.sel, first, "the cursor opens on mobile's first row");
    assert_eq!(pane_at(&app), "%121");
    assert_eq!(
        app.own_row(),
        None,
        "neither your pane nor your header is drawn"
    );

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        jump_to_mobile_idle_claude()
    );
    assert!(!app.quit, "the jump closes the popup, not the key");
}

/// The same row reached the other way: the tree opens on your header (your pane is not
/// drawn), `f` keeps the cursor in your session on its first flat row, and Enter there
/// goes to that agent rather than closing on it.
#[test]
fn after_f_from_your_own_header_enter_goes_to_the_row_the_cursor_kept() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Agents;
    let mut app = opened_from_a_plain_shell(&cfg);
    assert_eq!(header_at(&app), "mobile");

    app.on_key(ch('f'));
    assert!(app.flat_now());
    assert_eq!(pane_at(&app), "%121", "f kept the cursor in mobile");

    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        jump_to_mobile_idle_claude()
    );
    assert!(!app.quit);
}

/// In the tree your session's header stands in for a pane it does not draw: that header
/// is your own row, so Enter on it just closes the popup.
#[test]
fn in_the_tree_your_header_stands_in_for_an_undrawn_pane_and_enter_just_closes() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Agents;
    let mut app = opened_from_a_plain_shell(&cfg);
    assert_eq!(header_at(&app), "mobile");
    assert_eq!(app.own_row(), Some(app.sel));

    assert_eq!(app.on_key(key(KeyCode::Enter)), None, "nothing for tmux");
    assert!(app.quit);
}

/// A flat list has no headers to number, so the first nine rows that are not parked
/// worktrees carry the labels instead, and a number puts the cursor on that row.
#[test]
fn a_flat_list_numbers_its_first_nine_rows_instead_of_its_sessions() {
    let mut cfg = config();
    cfg.ui.layout = Layout::Flat;
    let mut app = app(&cfg);

    assert_eq!(app.row_label(0).as_deref(), Some("1"));
    assert_eq!(app.row_label(8).as_deref(), Some("9"));
    assert_eq!(app.row_label(9), None, "nine labels, no more");
    let worktree = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::ClosedWorktree { .. }))
        .expect("the parked worktree has a row");
    assert_eq!(
        app.row_label(worktree),
        None,
        "a parked worktree is not one"
    );

    app.on_key(ch('2'));
    assert_eq!(app.sel, 1);
}

/// Leaves the popup's world with nothing left to do: every agent looked at and working,
/// and no previous session either.
fn nowhere_to_go(app: &mut App) {
    let mut snap = app.snap.clone();
    for a in &mut snap.agents {
        a.seen = true;
        if a.state == Some(AgentState::Blocked) {
            a.state = Some(AgentState::Working);
        }
    }
    snap.previous_session = None;
    app.set_snapshot(snap);
}

/// The anchor is the row *you* last moved to. A refresh that shifts every index below an
/// inserted session still puts the cursor back on its own row, but it must not touch the
/// anchor: expanding again owes you the agent you collapsed from, not the header the
/// refresh found you on.
#[test]
fn a_refresh_that_moves_the_rows_leaves_the_anchor_alone() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    app.on_key(key(KeyCode::Tab)); // infra's blocked claude
    assert_eq!(pane_at(&app), "%341");
    app.on_key(ch('h'));
    app.on_key(ch('h')); // sessions only: infra's header stands in for it
    assert_eq!(header_at(&app), "infra");
    let before = app.sel;

    // A session appears above infra, so every header below it moves down a row.
    let mut next = snapshot();
    next.spaces
        .insert(2, space(6, "~/code/extra", "main", (0, 0, 0, 0)));
    app.set_snapshot(next);

    assert_eq!(header_at(&app), "infra", "the cursor followed its header");
    assert_ne!(app.sel, before, "and the header really did move");
    app.on_key(ch('l'));
    assert_eq!(pane_at(&app), "%341", "expanding returns to the agent");
}

/// `[ui] labels = false` takes the numbers off the rows, so the number keys do nothing at
/// all — not even complain about a session the reader can see but cannot name.
#[test]
fn a_number_does_nothing_at_all_when_the_labels_are_off() {
    let mut cfg = config();
    cfg.ui.labels = false;
    let mut app = app(&cfg);
    let before = app.sel;

    assert_eq!(app.on_key(ch('3')), None);

    assert_eq!(app.sel, before, "the cursor stayed where it was");
    assert!(app.live_toast().is_none(), "and nothing was said about it");
    assert!(!app.quit);
}

#[test]
fn enter_on_your_own_row_with_nowhere_to_go_just_closes_the_popup() {
    let cfg = config();
    let mut app = app(&cfg);
    nowhere_to_go(&mut app);

    assert_eq!(app.on_key(key(KeyCode::Enter)), None, "nothing for tmux");
    assert!(app.quit);
}

/// [`App::keep_cursor`]'s last rule, since the sessions depth is never empty (`f` is inert
/// there, so a flat config still shows the headers). Stepping a
/// flat-configured app down to the sessions depth lands the cursor on your own session's
/// header, and stepping back out returns to the flat list on a legal row.
#[test]
fn a_flat_config_stepped_to_the_sessions_depth_lands_on_your_own_header() {
    let mut cfg = config();
    cfg.ui.layout = Layout::Flat;
    let mut app = app(&cfg);

    app.on_key(ch('h')); // all -> agents
    app.on_key(ch('h')); // agents -> sessions

    assert_eq!(app.depth, Depth::Sessions);
    assert!(!app.flat_now());
    assert_eq!(
        app.current().cloned(),
        Some(Row::Session(0)),
        "the cursor finds mobile's header, the session the popup was opened from"
    );

    app.on_key(ch('l'));
    assert!(app.flat_now(), "back to the flat agents");
    assert!(
        app.sel < app.rows.rows.len(),
        "the cursor is on a drawn row"
    );
}

// ------------------------------------------ the letters

// The letters `a`–`e` are always on for the session the cursor is in, however it got there. At the `all`
// depth mobile's rows are 1–5, web's 7–12 (12 its parked worktree), infra's 14–18, api's
// 20 and notes' 22–23.

/// `a`, `b`, … on `rows`, in order: what [`App::lettered_rows`] returns for a session
/// whose rows those are.
fn letters_on(rows: std::ops::RangeInclusive<usize>) -> Vec<(usize, char)> {
    rows.zip(JUMP_LETTERS.chars()).collect()
}

/// Whichever way the cursor moves, the letters go with it to the session it lands in: the
/// popup's first frame, `j`, `J`, `K`, `Tab`, a number, `k` onto a parked worktree, and a
/// click. A header is its own session's; a pane or a parked worktree is its session's.
#[test]
fn the_letters_follow_the_cursor_into_whichever_session_it_is_in() {
    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(pane_at(&app), "%111", "the popup opens on mobile's claude");
    assert_eq!(app.lettered_rows(), letters_on(1..=5), "no number needed");

    app.on_key(ch('j'));
    assert_eq!(pane_at(&app), "%121");
    assert_eq!(app.lettered_rows(), letters_on(1..=5), "j, within mobile");
    app.on_key(ch('J'));
    assert_eq!(header_at(&app), "web");
    assert_eq!(
        app.lettered_rows(),
        letters_on(7..=11),
        "J: web's first five rows of six"
    );
    app.on_key(ch('K'));
    assert_eq!(header_at(&app), "mobile");
    assert_eq!(
        app.lettered_rows(),
        letters_on(1..=5),
        "K: back on a header"
    );
    app.on_key(key(KeyCode::Tab));
    assert_eq!(
        pane_at(&app),
        "%231",
        "Tab: web's codex, the first that needs you"
    );
    assert_eq!(app.lettered_rows(), letters_on(7..=11), "Tab");
    app.on_key(ch('3'));
    assert_eq!(header_at(&app), "infra");
    assert_eq!(app.lettered_rows(), letters_on(14..=18), "a number");
    app.on_key(ch('k'));
    assert!(
        matches!(app.current(), Some(Row::ClosedWorktree { .. })),
        "k from infra's header: web's parked worktree"
    );
    assert_eq!(
        app.lettered_rows(),
        letters_on(7..=11),
        "a parked worktree is its session's row"
    );
    app.hits.tree = PanelHit {
        area: Rect::new(0, 0, 60, 30),
        rows: vec![(4, 20)],
        ..PanelHit::default()
    };
    app.on_mouse(click(3, 4, MouseButton::Left));
    assert_eq!(pane_at(&app), "%411", "a click on api's shell");
    assert_eq!(app.lettered_rows(), vec![(20, 'a')], "a click");
}

/// Every session's rows wear their letters: at the `all` depth mobile's
/// rows 1 to 5 are `a` to `e`, web's 7 to 11 (its sixth, the parked worktree, has none),
/// infra's 14 to 18, api's one row `a` and notes' two `a` and `b`. A letter is still a key of
/// the cursor's session only: `c` from mobile is mobile's third row, and from web it is
/// web's. None show where the letters do not (a flat list).
#[test]
fn every_session_wears_its_letters_and_a_letter_acts_on_the_cursors_session_only() {
    let cfg = config();
    let mut app = app(&cfg);
    let mut every = letters_on(1..=5);
    every.extend(letters_on(7..=11));
    every.extend(letters_on(14..=18));
    every.extend(letters_on(20..=20));
    every.extend(letters_on(22..=23));
    assert_eq!(app.row_letters(), every);
    assert_eq!(
        app.lettered_rows(),
        letters_on(1..=5),
        "the cursor's are mobile's"
    );

    app.on_key(ch('c'));
    assert_eq!(pane_at(&app), "%131", "c: mobile's third row, its nvim");
    app.on_key(ch('J'));
    app.on_key(ch('c'));
    assert_eq!(
        pane_at(&app),
        "%222",
        "c from web's header: web's third row, its gemini"
    );
    assert_eq!(
        app.row_letters(),
        every,
        "the same letters wherever the cursor is"
    );

    app.on_key(ch('f'));
    assert!(app.row_letters().is_empty(), "flat: no letters");
}

/// A letter is an ordinary key: it moves the cursor to its row and does nothing else, and
/// the letters stay out, with nothing to put away. At the shipped `agents` depth infra's
/// rows are its two claudes, so `3 b Enter` reaches its blocked one, and once the cursor is anywhere in infra, `b` alone does.
#[test]
fn a_letter_moves_the_cursor_to_its_row_and_nothing_else() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Agents;
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    assert_eq!(app.on_key(ch('b')), None, "a letter only moves the cursor");
    assert_eq!(
        pane_at(&app),
        "%341",
        "b: infra's second row, its blocked claude"
    );
    assert_eq!(app.lettered_rows().len(), 2, "the letters are still out");
    app.on_key(ch('a'));
    assert_eq!(pane_at(&app), "%311", "a: infra's first row");
    app.on_key(ch('b'));
    assert!(!app.overlay.is_open());
    assert!(app.live_toast().is_none());
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::Jump {
            session: "$3".to_string(),
            window: "@34".to_string(),
            pane: "%341".to_string(),
        }),
        "Enter is then the ordinary go"
    );
}

/// infra has two rows at the `agents` depth, so `c`, `d` and `e` name nothing there. The hint
/// names `a-e` in every session, so pressing one says so in one plain line,
/// and does nothing else: no move, no overlay. api, with no row at that depth, says so for
/// `a`. A letter a `[keys]` entry gives to an action is that action's, row or not.
#[test]
fn a_letter_with_no_row_says_so() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Agents;
    let mut popup = app(&cfg);
    popup.on_key(ch('3'));
    for letter in ['c', 'd', 'e'] {
        assert_eq!(popup.on_key(ch(letter)), None, "{letter}");
        assert_eq!(header_at(&popup), "infra", "{letter}: the cursor stayed");
        assert!(!popup.overlay.is_open(), "{letter}");
        let said = format!("no row {letter} in this session");
        assert_eq!(popup.live_toast(), Some(said.as_str()), "{letter}");
        assert!(!popup.quit, "{letter}");
    }
    popup.on_key(ch('4'));
    assert_eq!(
        header_at(&popup),
        "api",
        "a shell and no agent: no row here"
    );
    popup.on_key(ch('a'));
    assert_eq!(popup.live_toast(), Some("no row a in this session"));

    let mut bound = config();
    bound.ui.depth = Depth::Agents;
    bound
        .keys
        .bindings
        .insert("rename".to_string(), KeySpec::One("c".to_string()));
    let mut bound_popup = app(&bound);
    bound_popup.on_key(ch('3'));
    bound_popup.on_key(ch('c'));
    assert!(
        matches!(bound_popup.overlay, Overlay::Prompt(_)),
        "c renames infra, as bound"
    );
}

/// Five letters and no more: web's sixth row, its parked worktree, wears none, and `f`
/// onwards are action keys — `f` flips the layout as it always did.
#[test]
fn five_letters_and_no_more_and_f_is_never_one() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('2'));
    let letters: Vec<Option<char>> = (7..=12)
        .map(|row| letter_among(&app.lettered_rows(), row))
        .collect();
    assert_eq!(
        letters,
        vec![Some('a'), Some('b'), Some('c'), Some('d'), Some('e'), None],
        "the parked worktree, sixth, has no letter"
    );
    app.on_key(ch('e'));
    assert_eq!(pane_at(&app), "%241", "e: the fifth row, the node server");
    app.on_key(ch('f'));
    assert_eq!(app.layout, Layout::Flat, "f is the flat list, not a letter");
}

/// No letters where they would mean nothing or get in the way: a flat list (its numbers
/// already name rows), the sessions depth (nothing under a header), and a session with no
/// rows under its header (api at the `agents` depth: a shell, no agent). A letter there does
/// nothing.
#[test]
fn no_letters_in_a_flat_list_at_the_sessions_depth_or_for_a_session_without_rows() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('f'));
    assert!(app.flat_now());
    assert!(app.lettered_rows().is_empty(), "flat");
    let sel = app.sel;
    app.on_key(ch('b'));
    assert_eq!(app.sel, sel, "b does nothing in a flat list");

    app.on_key(ch('f'));
    app.on_key(ch('h')); // all -> agents
    app.on_key(ch('4'));
    assert_eq!(header_at(&app), "api");
    assert!(
        app.lettered_rows().is_empty(),
        "a header with nothing under it"
    );
    app.on_key(ch('h')); // agents -> sessions
    app.on_key(ch('3'));
    assert_eq!(header_at(&app), "infra");
    assert!(app.lettered_rows().is_empty(), "the sessions depth");
}

/// `[ui] labels = false` takes the numbers away, and the letters with them.
#[test]
fn no_letters_with_labels_off() {
    let mut cfg = config();
    cfg.ui.labels = false;
    let mut app = app(&cfg);
    assert!(app.lettered_rows().is_empty());
    let sel = app.sel;
    app.on_key(ch('b'));
    assert_eq!(app.sel, sel);
}

/// While keystrokes are text — the modal preset's open filter, the search preset's
/// always-open one — no row wears a letter, and a letter is typed into the query.
#[test]
fn no_letters_while_typing_and_a_letter_is_text() {
    for cfg in [config(), search_config()] {
        let mut app = app(&cfg);
        if app.preset == KeysPreset::Modal {
            assert!(!app.lettered_rows().is_empty(), "the tree has its letters");
            app.on_key(ch('/'));
        }
        assert!(app.typing());
        assert!(app.lettered_rows().is_empty(), "{:?}", app.preset);
        app.on_key(ch('b'));
        assert_eq!(
            app.filter.as_ref().map(|f| f.value()),
            Some("b"),
            "{:?}: the letter was typed",
            app.preset
        );
    }
}

/// With the letters out, Esc quits.
#[test]
fn esc_quits_with_the_letters_out() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));
    assert!(!app.lettered_rows().is_empty());
    app.on_key(key(KeyCode::Esc));
    assert!(app.quit);
}

/// The letters are worked out from the cursor, so a refresh needs nothing of its own: a
/// session appearing above infra moves infra's rows and the letters with them, and a
/// refresh without infra shows the letters of wherever the cursor lands — api's shell.
#[test]
fn a_refresh_moves_the_letters_with_the_cursor() {
    let cfg = config();
    let mut app = app(&cfg);
    app.on_key(ch('3'));

    let mut next = snapshot();
    next.spaces
        .insert(2, space(6, "~/code/extra", "main", (0, 0, 0, 0)));
    app.set_snapshot(next);
    assert_eq!(header_at(&app), "infra", "the cursor is kept on infra");
    assert_eq!(
        app.lettered_rows(),
        letters_on(15..=19),
        "one row further down"
    );
    let (first, _) = app.lettered_rows()[0];
    assert!(
        matches!(app.rows.rows[first], Row::Pane(p) if app.snap.panes[p].pane_id == "%311"),
        "a is still infra's first row"
    );

    let mut gone = snapshot();
    gone.spaces.retain(|s| s.session_id != "$3");
    gone.panes.retain(|p| p.session_id != "$3");
    gone.agents.retain(|a| a.session_id != "$3");
    app.set_snapshot(gone);
    assert_eq!(
        pane_at(&app),
        "%411",
        "infra has gone: the cursor is on api's shell"
    );
    assert_eq!(
        app.lettered_rows(),
        vec![(14, 'a')],
        "and the letters are api's"
    );
}

/// The hint line names the letters right after the numbers whenever they show, and the same
/// way whatever session the cursor is in, so the entry is always `a-e rows`, in a session of
/// three rows, five, one, six or none. Not at all where they do not show; the numbers are never
/// replaced.
#[test]
fn the_hint_lines_letter_entry_is_the_same_whatever_the_session() {
    let entry = |key: &str, meaning: &str| (key.to_string(), meaning.to_string());
    let letters = entry("a-e", "rows");
    let mut agents = config();
    agents.ui.depth = Depth::Agents;
    let mut web = app(&agents);
    web.set_worktrees(Vec::new());
    web.on_key(ch('2'));
    assert_eq!(web.hint_line()[1], letters, "web's three agents");
    web.on_key(ch('4'));
    assert_eq!(web.hint_line()[1], letters, "api, no row at this depth");

    let cfg = config();
    let mut app = app(&cfg);
    assert_eq!(
        app.hint_line()[..3],
        [
            entry("1-9", "session"),
            letters.clone(),
            entry("Tab", "next up"),
        ],
        "mobile, five rows"
    );
    app.on_key(ch('4'));
    assert_eq!(app.hint_line()[1], letters, "api, one row");
    app.on_key(ch('2'));
    assert_eq!(app.hint_line()[1], letters, "web, six rows");
    app.on_key(ch('f'));
    assert_eq!(
        app.hint_line()[..2],
        [entry("1-9", "session"), entry("Tab", "next up")],
        "flat: no letters, nothing named"
    );
    app.on_key(ch('f'));
    app.on_key(ch('h'));
    app.on_key(ch('h'));
    assert_eq!(app.depth, Depth::Sessions);
    assert_eq!(
        app.hint_line()[1],
        entry("Tab", "next up"),
        "the sessions depth: no letters"
    );
}

/// A letter a `[keys]` entry gives to an action stays that action's: the key does what the
/// entry says, the row it would have named wears no letter, and the rest keep theirs.
#[test]
fn a_letter_bound_in_keys_keeps_its_binding_and_its_row_goes_without() {
    let mut cfg = config();
    cfg.keys
        .bindings
        .insert("next_attention".to_string(), KeySpec::One("a".to_string()));
    let mut app = app(&cfg);
    assert_eq!(
        app.lettered_rows(),
        vec![(2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')],
        "mobile's first row wears no letter"
    );
    assert_eq!(app.hint_line()[1], ("b-e".to_string(), "rows".to_string()));
    app.on_key(ch('a'));
    assert_eq!(
        pane_at(&app),
        "%231",
        "a walks the attention list, as bound"
    );
    app.on_key(ch('b'));
    assert_eq!(pane_at(&app), "%221", "b is web's second row");
}

/// A `[keys]` letter taken from the *middle* of the run, not the end, leaves a gap: the hint
/// must not claim the missing one is still among them. `rename` bound to `c`, mobile's
/// third row's own letter, leaves a, b, d, e -- `a-e` would say c jumps too.
#[test]
fn the_hint_line_lists_the_letters_instead_of_a_run_across_a_gap() {
    let mut cfg = config();
    cfg.keys
        .bindings
        .insert("rename".to_string(), KeySpec::One("c".to_string()));
    let app = app(&cfg);
    let letters: Vec<char> = app.lettered_rows().into_iter().map(|(_, c)| c).collect();
    assert_eq!(
        letters,
        vec!['a', 'b', 'd', 'e'],
        "the third row wears no letter"
    );
    assert_eq!(
        app.hint_line()[1],
        ("a b d e".to_string(), "rows".to_string())
    );
}

/// The tmux prefix starts its chord and leaves the letters as they are. A letter right after
/// it is the chord's to swallow, never a jump; and the chord still goes to what needs you. Ctrl-B
/// is tmux's `prefix` in the first run and its `prefix2` in the second.
#[test]
fn a_letter_after_the_prefix_is_the_chords_not_a_jump() {
    let mut cfg = config();
    cfg.ui.depth = Depth::Agents;
    for chord in [chord(), chord_on_prefix2("C-b")] {
        let mut app = app(&cfg);
        app.chord = Some(chord);
        app.on_key(ch('3')); // infra's header: a and b
        let letters = app.lettered_rows();

        assert_eq!(
            app.on_key(ctrl('b')),
            None,
            "the prefix alone does nothing else"
        );
        assert_eq!(app.lettered_rows(), letters, "the letters stay");
        assert_eq!(app.on_key(ch('b')), None);
        assert_eq!(header_at(&app), "infra", "prefix then b does not jump");

        app.on_key(ctrl('b'));
        assert_eq!(
            app.on_key(ch('O')),
            Some(Action::NextUp {
                own: Some("%111".to_string()),
            }),
            "the chord still goes next up"
        );
    }
}

// ------------------------------------- the numbers and letters while you type

// Searching by `4A` goes to session 4's agent A: the filter searches everything you can
// see on your screen, labels included. At the `all` depth session 2 is
// web (rows a zsh `%211`, b claude `%221`, c gemini `%222`, d codex `%231`, e node `%241`) and
// session 4 is api (row a zsh `%411`).

/// One app per preset, each with its filter being typed into: `/` in the modal preset, the
/// search preset's always open.
fn typing_apps(configs: &[Config; 2]) -> Vec<App<'_>> {
    configs
        .iter()
        .map(|cfg| {
            let mut app = app(cfg);
            if app.preset == KeysPreset::Modal {
                app.on_key(ch('/'));
            }
            assert!(app.typing(), "{:?}", app.preset);
            app
        })
        .collect()
}

#[test]
fn typing_a_number_lands_on_its_header_and_a_letter_after_it_on_that_row() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        let preset = app.preset;
        type_str(&mut app, "4");
        assert_eq!(header_at(&app), "api", "{preset:?}: 4 is api's header");
        type_str(&mut app, "a");
        assert_eq!(pane_at(&app), "%411", "{preset:?}: 4a is api's row a");
        assert_eq!(
            app.filter.as_ref().map(|input| input.value()),
            Some("4a"),
            "{preset:?}: the letter is text of the query, not a key"
        );
    }
    for typed in ["2c", "2C"] {
        for mut app in typing_apps(&configs) {
            type_str(&mut app, typed);
            assert_eq!(
                pane_at(&app),
                "%222",
                "{:?}: {typed} is web's gemini",
                app.preset
            );
        }
    }
}

/// A row whose address the query is exactly keeps the cursor, whatever else scores as high:
/// `4` scores api's row a (`4a api zsh 1:zsh`) as high as api's header (`4 api main`), and a
/// child wins a tie, so only the label rule puts the cursor on the header.
#[test]
fn a_query_that_is_a_label_wins_over_a_row_that_scores_as_high() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        type_str(&mut app, "4");
        let labels = &app.labels;
        let text = |row: Row| tree::match_text(&app.snap, &app.worktrees, &row, true, labels);
        let header = text(Row::Session(3));
        let child = text(Row::Pane(15));
        assert!(header.starts_with("4 api"), "{header:?}");
        assert!(child.starts_with("4a api zsh"), "{child:?}");
        let query = Query {
            text: "4".to_string(),
        };
        let score = |text: &str| tree::score(&query, text).expect("it matches").0;
        assert!(
            score(&child) >= score(&header),
            "the precondition: the row scores at least as high as the header"
        );
        assert_eq!(header_at(&app), "api", "{:?}", app.preset);
    }
}

#[test]
fn enter_after_a_typed_address_goes_to_that_row() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        type_str(&mut app, "2c");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Jump {
                session: "$2".to_string(),
                window: "@22".to_string(),
                pane: "%222".to_string(),
            }),
            "{:?}",
            app.preset
        );
    }
}

/// With `[ui] labels = false` no number or letter is drawn, so none is matched: `4a` finds
/// nothing (no row has a 4 followed by an a), and `4` does not name api's header.
#[test]
fn with_the_labels_off_a_typed_address_names_nothing() {
    let mut modal = config();
    modal.ui.labels = false;
    let mut search = search_config();
    search.ui.labels = false;
    let configs = [modal, search];
    for mut app in typing_apps(&configs) {
        type_str(&mut app, "4");
        assert_ne!(app.current(), Some(&Row::Session(3)), "{:?}", app.preset);
        type_str(&mut app, "a");
        assert!(
            !app.rows.rows.contains(&Row::Pane(15)),
            "{:?}: {:?}",
            app.preset,
            app.rows.rows
        );
        assert!(app.row_letters().is_empty(), "and no letter is drawn");
    }
}

/// A tenth session has no number, so its rows' addresses are their letters alone: `^a`
/// (text that starts with an a) finds its row a and no numbered session's row by its label,
/// whose rows start with their numbers; `api` is found by its own name, as `^name` always does.
#[test]
fn a_tenth_sessions_row_is_found_by_its_letter_alone() {
    let mut snap = snapshot();
    for n in 6..=10u32 {
        let name = ["kilo", "lima", "mike", "oscar", "zulu"][n as usize - 6];
        snap.spaces
            .push(space(n, &format!("~/x/{name}"), "main", (0, 0, 0, 0)));
        let mut pane = snap.panes[15].clone();
        pane.session_id = format!("${n}");
        pane.pane_id = format!("%{n}11");
        pane.window_id = format!("@{n}1");
        snap.panes.push(pane);
    }
    let configs = [config(), search_config()];
    for cfg in &configs {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(cfg, theme, "settings".to_string(), snap.clone());
        if app.preset == KeysPreset::Modal {
            app.on_key(ch('/'));
        }
        let zulu = snap.panes.len() - 1;
        assert_eq!(
            tree::match_text(
                &app.snap,
                &app.worktrees,
                &Row::Pane(zulu),
                true,
                &app.labels
            ),
            "a zulu zsh 1:zsh main"
        );
        type_str(&mut app, "^a");
        let api = app.snap.spaces.iter().position(|s| s.session_name == "api");
        let rows = &app.rows.rows;
        assert!(rows.contains(&Row::Pane(zulu)), "{:?}", app.preset);
        assert!(
            rows.contains(&Row::Session(api.unwrap())),
            "api by its name"
        );
        assert!(
            !rows.iter().any(|row| matches!(row, Row::Session(1))),
            "web starts with a 2, not an a"
        );
    }
}

/// A filter hides rows and moves no label: hiding web's first two rows leaves gemini its
/// `c`, drawn while you type.
#[test]
fn a_query_that_hides_earlier_rows_leaves_the_later_rows_letters() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        type_str(&mut app, "gemini");
        assert_eq!(app.rows.rows, vec![Row::Session(1), Row::Pane(7)]);
        assert_eq!(app.row_letters(), vec![(1, 'c')], "{:?}", app.preset);
        assert_eq!(app.label_index(0), Some(2), "web keeps its number");
    }
}

/// A flat list's numbers are the rows' places with nothing typed: a query that hides the rows
/// before one leaves it its number, and typing that number lands on it.
#[test]
fn a_flat_lists_numbers_stay_under_a_query() {
    let mut modal = config();
    modal.ui.layout = Layout::Flat;
    let mut search = search_config();
    search.ui.layout = Layout::Flat;
    let configs = [modal, search];
    for mut app in typing_apps(&configs) {
        let at = app
            .rows
            .rows
            .iter()
            .position(|row| *row == Row::Pane(7))
            .expect("gemini is drawn");
        let n = app.label_index(at).expect("gemini is numbered");
        assert!(n > 1, "rows come before gemini: {n}");
        type_str(&mut app, "gemini");
        assert_eq!(app.rows.rows, vec![Row::Pane(7)]);
        assert_eq!(app.label_index(0), Some(n), "{:?}", app.preset);
        for _ in 0.."gemini".len() {
            app.on_key(key(KeyCode::Backspace));
        }
        type_str(&mut app, &n.to_string());
        assert_eq!(pane_at(&app), "%222", "{:?}: typing {n}", app.preset);
    }
}

/// A tenth session's row a has the address `a`, a letter alone, which is text like any other
/// and names no row: typing a bare `a` scores that row as any match, never as [`tree::NAMED`],
/// and leaves the cursor where the fuzzy match puts it, not on that row.
#[test]
fn a_bare_letter_does_not_name_a_tenth_sessions_row() {
    let mut snap = snapshot();
    for n in 6..=10u32 {
        let name = ["kilo", "lima", "mike", "oscar", "zulu"][n as usize - 6];
        snap.spaces
            .push(space(n, &format!("~/x/{name}"), "main", (0, 0, 0, 0)));
        let mut pane = snap.panes[15].clone();
        pane.session_id = format!("${n}");
        pane.pane_id = format!("%{n}11");
        pane.window_id = format!("@{n}1");
        snap.panes.push(pane);
    }
    let zulu = Row::Pane(snap.panes.len() - 1);
    for cfg in [config(), search_config()] {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, "settings".to_string(), snap.clone());
        if app.preset == KeysPreset::Modal {
            app.on_key(ch('/'));
        }
        let at = app.rows.rows.iter().position(|row| *row == zulu);
        assert_eq!(app.row_address(at.expect("zulu is drawn")), "a");
        type_str(&mut app, "a");
        let at = app.rows.rows.iter().position(|row| *row == zulu);
        let score = app.rows.scores[at.expect("zulu matches an a")].as_ref();
        assert!(
            score.is_some_and(|(score, _)| *score != tree::NAMED),
            "{:?}: {score:?}",
            app.preset
        );
        assert_ne!(app.current(), Some(&zulu), "{:?}", app.preset);
    }
}

/// While you type where no letters are drawn, a flat list and the sessions depth, the hint
/// line says what typing a number does, `1-9 session`, and never `4a session 4 row a`, in both
/// presets.
#[test]
fn where_no_letters_are_drawn_the_typing_hint_names_the_numbers() {
    let numbers = ("1-9".to_string(), "session".to_string());
    let typed = ("4a".to_string(), "session 4 row a".to_string());
    for (layout, depth) in [(Layout::Flat, Depth::All), (Layout::Tree, Depth::Sessions)] {
        let mut modal = config();
        let mut search = search_config();
        for cfg in [&mut modal, &mut search] {
            cfg.ui.layout = layout;
            cfg.ui.depth = depth;
        }
        let configs = [modal, search];
        for app in typing_apps(&configs) {
            let hints = app.hint_line();
            assert!(hints.contains(&numbers), "{layout:?} {depth:?}: {hints:?}");
            assert!(!hints.contains(&typed), "{layout:?} {depth:?}: {hints:?}");
        }
    }
}

/// A row's address is spelled as its session's number is drawn ([`Keymap::label`]): with
/// `[keys] select_session` on ctrl+1 to ctrl+9, api's header is `ctrl-4` and its row a
/// `ctrl-4a`, not `4a`.
#[test]
fn a_rows_address_is_spelled_as_its_number_is_drawn() {
    let mut cfg = config();
    cfg.keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many((1..=9).map(|n| format!("ctrl+{n}")).collect()),
    );
    let app = app(&cfg);
    let label = app.keymap.label(4);
    assert_ne!(label, "4", "the precondition: 4 is drawn another way");
    let at = |row: Row| app.rows.rows.iter().position(|r| *r == row).unwrap();
    assert_eq!(app.row_address(at(Row::Session(3))), label);
    assert_eq!(app.row_address(at(Row::Pane(15))), format!("{label}a"));
}

/// fzf's terms meet the labels one by one: each term is matched on its own
/// against a row's text with its label in front and without it. `!4` drops api's header and
/// its row a, labelled 4 and 4a, and every row whose words hold a 4, and keeps the rest;
/// `gemini !2c` drops web's gemini, labelled 2c; `!^api` drops api's row a, whose own words
/// start with api though its text with the label does not; `^api 4a` keeps api's row a, its words
/// starting with api and its label being 4a, and nothing whose words start otherwise.
#[test]
fn fzf_terms_meet_the_labels_one_by_one() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        let preset = app.preset;
        type_str(&mut app, "!4");
        let rows = app.rows.rows.clone();
        assert!(!rows.contains(&Row::Session(3)), "{preset:?}: {rows:?}");
        assert!(!rows.contains(&Row::Pane(15)), "{preset:?}: {rows:?}");
        assert!(
            rows.contains(&Row::Session(1)) && rows.contains(&Row::Pane(7)),
            "{preset:?}: web and its gemini stay: {rows:?}"
        );
        for _ in 0..2 {
            app.on_key(key(KeyCode::Backspace));
        }

        type_str(&mut app, "gemini");
        assert!(app.rows.rows.contains(&Row::Pane(7)), "{preset:?}");
        type_str(&mut app, " !2c");
        assert!(
            !app.rows.rows.contains(&Row::Pane(7)),
            "{preset:?}: {:?}",
            app.rows.rows
        );
        for _ in 0.."gemini !2c".len() {
            app.on_key(key(KeyCode::Backspace));
        }

        type_str(&mut app, "!^api");
        let rows = app.rows.rows.clone();
        assert!(!rows.contains(&Row::Pane(15)), "{preset:?}: {rows:?}");
        assert!(rows.contains(&Row::Pane(7)), "{preset:?}: {rows:?}");
        for _ in 0.."!^api".len() {
            app.on_key(key(KeyCode::Backspace));
        }

        type_str(&mut app, "^api 4a");
        let rows = &app.rows.rows;
        assert!(rows.contains(&Row::Pane(15)), "{preset:?}: {rows:?}");
        assert!(
            !rows.iter().any(|row| matches!(row, Row::Session(1))),
            "{preset:?}: web's words do not start with api: {rows:?}"
        );
        let at = rows.iter().position(|row| *row == Row::Pane(15)).unwrap();
        let hits = app.rows.scores[at].clone().expect("it matched").1;
        assert_eq!(app.row_address(at), "4a", "{preset:?}: the precondition");
        assert!(
            [0, 1, 3, 4, 5].iter().all(|i| hits.contains(i)),
            "{preset:?}: 4a lit from the label, api from the words: {hits:?}"
        );
    }
}

/// A plain query, one term, matches as it did before the terms met the labels one by one: `cl`
/// keeps the same rows with the same scores and positions as the better of the labelled and the
/// plain text, scored whole.
#[test]
fn a_one_term_query_matches_as_the_better_of_the_two_texts() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        type_str(&mut app, "cl");
        let query = Query {
            text: "cl".to_string(),
        };
        assert!(!app.rows.rows.is_empty());
        for (at, row) in app.rows.rows.iter().enumerate() {
            let labels = &app.labels;
            let with = tree::match_text(&app.snap, &app.worktrees, row, true, labels);
            let own = tree::row_text(&app.snap, &app.worktrees, row, true);
            let shift = with.chars().count() - own.chars().count();
            let plain = tree::score(&query, &own)
                .map(|(score, at)| (score, at.into_iter().map(|i| i + shift).collect::<Vec<_>>()));
            let expected = match (tree::score(&query, &with), plain) {
                (Some(label), Some(plain)) if plain.0 > label.0 => Some(plain),
                (Some(label), _) => Some(label),
                (None, plain) => plain,
            };
            assert_eq!(app.rows.scores[at], expected, "{:?}: {row:?}", app.preset);
        }
    }
}

/// With `[keys] select_session` on ctrl+1 to ctrl+9 the hint line while typing names the number
/// keys, which act in the filter, and no typed address: `ctrl-4a` cannot be typed, and an `a`
/// after ctrl-4 lands in the query as text. Bound to the digits backwards, session 4's label is
/// `6`, and `6a` can be typed and is named.
#[test]
fn the_typing_hint_names_an_address_only_while_a_query_can_hold_it() {
    for (keys, typed) in [
        (
            (1..=9).map(|n| format!("ctrl+{n}")).collect::<Vec<_>>(),
            None,
        ),
        (
            "987654321".chars().map(String::from).collect(),
            Some(("6a".to_string(), "session 4 row a".to_string())),
        ),
    ] {
        let mut modal = config();
        let mut search = search_config();
        for cfg in [&mut modal, &mut search] {
            cfg.keys
                .bindings
                .insert("select_session".to_string(), KeySpec::Many(keys.clone()));
        }
        let configs = [modal, search];
        for app in typing_apps(&configs) {
            let hints = app.hint_line();
            let named: Vec<_> = hints
                .iter()
                .filter(|(_, meaning)| meaning.contains("row a"))
                .cloned()
                .collect();
            assert_eq!(
                named,
                typed.clone().into_iter().collect::<Vec<_>>(),
                "{:?}: {hints:?}",
                app.preset
            );
        }
    }
}

/// While you type, every number works: typing it lands on its row ([`App::number_works`]).
/// The letters are drawn but are not keys, and the cursor's session's are lit while the query
/// is exactly that session's number ([`App::lit_letters`]).
#[test]
fn while_you_type_the_numbers_work_and_a_number_lights_its_sessions_letters() {
    let configs = [config(), search_config()];
    for mut app in typing_apps(&configs) {
        let preset = app.preset;
        assert!((1..=9).all(|n| app.number_works(n)), "{preset:?}");
        assert!(
            app.lettered_rows().is_empty(),
            "{preset:?}: no letter is a key"
        );
        assert!(
            !app.row_letters().is_empty(),
            "{preset:?}: but they are drawn"
        );
        assert!(app.lit_letters().is_empty(), "{preset:?}: nothing typed");
        type_str(&mut app, "2");
        let lit: Vec<(Row, char)> = app
            .lit_letters()
            .into_iter()
            .map(|(at, letter)| (app.rows.rows[at].clone(), letter))
            .collect();
        let web: Vec<(Row, char)> = (5..=9).map(Row::Pane).zip(JUMP_LETTERS.chars()).collect();
        assert_eq!(lit, web, "{preset:?}: 2 lights web's");
        type_str(&mut app, "c");
        assert!(
            app.lit_letters().is_empty(),
            "{preset:?}: 2c is a whole address"
        );
    }
}

#[test]
fn following_the_preview_with_the_preview_off_captures_nothing() {
    let mut cfg = config();
    cfg.ui.preview = false;
    let mut app = app(&cfg);
    assert!(!app.show_preview);
    assert!(app.preview_pane().is_some(), "the cursor row has a pane");
    // A tmux that is not there: a capture would fail and clear the text.
    let tmux = crate::tmux::Tmux::with_socket_name("taw-test-preview-off-no-server");
    app.preview = Some(preview::to_text("what was on show"));

    crate::tui::follow_preview(&tmux, &mut app);

    assert!(app.preview.is_some(), "nothing was captured");
    assert_eq!(app.preview_shows(), None);
}

// ------------------------------------------------ what only this machine's rows decide
//
// A row of another machine is read-only here, and a name, an id or a path it holds is that machine's
// own: it never decides what a command on this machine's rows says.

mod this_machine_only {
    use super::*;
    use crate::machine::MachineView;
    use crate::machine::wire::{State, WireAgent, WirePane, WireSpace, WireWorktree};
    use crate::tui::machines::{merge, merge_worktrees};

    /// A live machine called `studio` with `state`.
    pub(super) fn studio(state: State) -> MachineView {
        MachineView {
            name: "studio".into(),
            live: true,
            state: Some(state),
            last_seen: Some(5_000),
            ..MachineView::default()
        }
    }

    /// A session of studio's, `$id`, called `name` at `path`.
    pub(super) fn studio_space(id: u32, name: &str, path: &str) -> WireSpace {
        WireSpace {
            session_id: format!("${id}"),
            session_name: name.into(),
            session_path: path.into(),
            windows: 1,
            active_window_id: format!("@{id}"),
            active_pane_id: Some(format!("%{id}")),
            ..WireSpace::default()
        }
    }

    /// One shell pane of studio's session `$id`.
    pub(super) fn studio_pane(id: u32) -> WirePane {
        WirePane {
            pane_id: format!("%{id}"),
            session_id: format!("${id}"),
            window_id: format!("@{id}"),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 1,
            pane_index: 1,
            pane_current_command: "zsh".into(),
            pane_active: true,
            alternate_on: false,
        }
    }

    /// A closed worktree of studio's, cut from its session `source` (`w6`).
    pub(super) fn studio_record(name: &str, repo: &str, source: &str) -> WireWorktree {
        WireWorktree {
            workspace_id: "w20".into(),
            session_name: name.into(),
            path: format!("~/code/{name}"),
            branch: "rain".into(),
            base: None,
            repo_root: repo.into(),
            source_workspace_id: source.into(),
            created_unix_ms: 7,
            real_path: None,
            real_repo_root: None,
        }
    }

    /// An app over this machine's sample world and `view`, the records merged beside it.
    pub(super) fn app_with_studio<'a>(
        cfg: &'a Config,
        local: Snapshot,
        records: Vec<WorktreeRecord>,
        view: MachineView,
    ) -> App<'a> {
        let views = [view];
        let mut app = App::new(
            cfg,
            theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
            String::new(),
            merge(local, &views, 5_000),
        );
        app.set_worktrees(merge_worktrees(records, &views));
        app
    }

    /// This machine's sample world with `garden` (`$6`, workspace `w6`) and `garden--fix`, running
    /// at the path of the worktree this machine's own record says was cut from it.
    fn garden_here() -> (Snapshot, Vec<WorktreeRecord>) {
        let mut snap = snapshot();
        snap.spaces
            .push(space(6, "~/code/garden", "main", (0, 0, 0, 0)));
        snap.spaces
            .push(space(7, "~/code/garden--fix", "fix", (0, 0, 0, 0)));
        let mut records = worktrees();
        records.push(WorktreeRecord {
            workspace_id: "w21".into(),
            session_name: "garden--fix".into(),
            path: "~/code/garden--fix".into(),
            branch: "fix".into(),
            base: None,
            repo_root: "~/code/garden".into(),
            source_workspace_id: "w6".into(),
            created_unix_ms: 1,
            machine: None,
            real_path: None,
            real_repo_root: None,
        });
        (snap, records)
    }

    /// studio's `garden` (`$6`, the same path and the same id as this machine's) and `rain`, a
    /// worktree session of it that runs, with the records `records`.
    fn garden_there(records: Vec<WireWorktree>) -> MachineView {
        studio(State {
            spaces: vec![
                studio_space(6, "garden", "~/code/garden"),
                studio_space(7, "garden--rain", "~/code/garden--rain"),
            ],
            panes: vec![studio_pane(6), studio_pane(7)],
            worktrees: records,
            ..State::default()
        })
    }

    const ASKED: &str =
        "kill-session garden? It has 1 worktree session open (garden--fix). Close them too?";

    #[test]
    fn x_on_this_machines_session_names_only_this_machines_worktree_sessions() {
        // studio has the same `garden` at the same path, and a record whose source is `w6`, which
        // is this machine's `garden` too: its running `garden--rain` is not this machine's.
        let cfg = config();
        let (snap, records) = garden_here();
        let there = garden_there(vec![studio_record("garden--rain", "~/code/garden", "w6")]);
        let mut app = app_with_studio(&cfg, snap, records, there);
        cursor_to_header(&mut app, "garden");
        assert_eq!(app.on_key(ch('x')), None, "the question comes first");
        assert_eq!(confirmation(&app).message, ASKED);
    }

    #[test]
    fn a_record_of_another_machine_named_like_this_machines_session_does_not_skip_the_question() {
        // studio's own record of a worktree session called `garden`.
        let cfg = config();
        let (snap, records) = garden_here();
        let there = garden_there(vec![studio_record("garden", "/home/pi/other", "w9")]);
        let mut app = app_with_studio(&cfg, snap, records, there);
        cursor_to_header(&mut app, "garden");
        assert_eq!(app.on_key(ch('x')), None);
        assert_eq!(confirmation(&app).message, ASKED);
    }

    #[test]
    fn a_session_of_another_machine_called_like_a_closed_worktree_is_not_running_it() {
        // This machine's record of `garden--fix` has no session here; studio runs one of that name.
        let cfg = config();
        let (mut snap, records) = garden_here();
        snap.spaces.pop();
        let there = studio(State {
            spaces: vec![studio_space(7, "garden--fix", "~/code/garden--fix")],
            panes: vec![studio_pane(7)],
            ..State::default()
        });
        let mut app = app_with_studio(&cfg, snap, records, there);
        cursor_to_header(&mut app, "garden");
        assert_eq!(app.on_key(ch('x')), None);
        assert_eq!(confirmation(&app).message, "kill-session garden?");
    }

    /// The sample world with studio's agent `reviewer` in its session `garden`.
    fn reviewer_there() -> MachineView {
        let mut state = State {
            spaces: vec![studio_space(6, "garden", "~/code/garden")],
            panes: vec![studio_pane(6)],
            ..State::default()
        };
        state.agents.push(WireAgent {
            pane_id: "%6".into(),
            session_id: "$6".into(),
            session_name: "garden".into(),
            window_id: "@6".into(),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 1,
            pane_index: 1,
            kind: Some("claude".into()),
            state: Some("idle".into()),
            name: Some("reviewer".into()),
            // A pid that is this machine's own is no business of an agent over there.
            pane_pid: std::process::id(),
            ..WireAgent::default()
        });
        studio(state)
    }

    #[test]
    fn an_agent_of_another_machine_does_not_hold_the_name_a_local_agent_asks_for() {
        let cfg = config();
        let mut app = app_with_studio(&cfg, snapshot(), worktrees(), reviewer_there());
        assert!(
            app.snap
                .agents
                .iter()
                .any(|a| a.machine.is_some() && a.name.as_deref() == Some("reviewer"))
        );
        assert_eq!(
            submit_agent_rename(&mut app, "reviewer"),
            renamed_to("reviewer")
        );
        // And the idle look is never asked of a pid of another machine.
        let look = |a: &AgentRow| -> bool {
            assert!(a.machine.is_none(), "asked about {}", a.pane_id);
            false
        };
        assert_eq!(app.check_unique_by("%221", "reviewer", look), Ok(()));
    }

    #[test]
    fn a_local_agent_with_the_name_still_refuses_it_next_to_one_of_another_machine() {
        // The positive half: both machines have a `reviewer`, and this machine's decides.
        let cfg = config();
        let mut app = app_with_studio(&cfg, snapshot(), worktrees(), reviewer_there());
        app.snap.agents[4].name = Some("reviewer".to_string()); // %231, web's codex
        assert_eq!(submit_agent_rename(&mut app, "reviewer"), None);
        assert_eq!(
            prompt(&app).error.as_deref(),
            Some("another agent is already named 'reviewer'")
        );
    }

    /// Types `typed` into the rename prompt of this machine's session `session` and presses Enter.
    fn rename_session_to(app: &mut App, session: &str, typed: &str) -> Option<Action> {
        cursor_to_header(app, session);
        app.on_key(ch('r'));
        let Overlay::Prompt(p) = &mut app.overlay else {
            panic!("the prompt did not open")
        };
        p.input.set(typed);
        app.on_key(key(KeyCode::Enter))
    }

    #[test]
    fn renaming_a_session_may_take_a_name_only_another_machine_has() {
        let cfg = config();
        let mut app = app_with_studio(
            &cfg,
            snapshot(),
            worktrees(),
            studio(State {
                spaces: vec![studio_space(6, "garden", "~/code/garden")],
                panes: vec![studio_pane(6)],
                ..State::default()
            }),
        );
        assert_eq!(
            rename_session_to(&mut app, "web", "garden"),
            Some(Action::RenameWorkspace {
                session: "$2".to_string(),
                name: "garden".to_string(),
            })
        );
    }

    #[test]
    fn renaming_a_session_still_refuses_a_name_this_machine_has() {
        // The positive half, with a machine that has the same name listed.
        let cfg = config();
        let mut app = app_with_studio(
            &cfg,
            snapshot(),
            worktrees(),
            studio(State {
                spaces: vec![studio_space(6, "infra", "~/code/infra")],
                panes: vec![studio_pane(6)],
                ..State::default()
            }),
        );
        assert_eq!(rename_session_to(&mut app, "web", "infra"), None);
        assert!(
            prompt(&app)
                .error
                .as_deref()
                .is_some_and(|e| e.contains("duplicate session")),
            "{:?}",
            prompt(&app).error
        );
    }
}

// ------------------------------------------------ the walk across machines

mod walk_across_machines {
    use super::this_machine_only::{studio_pane, studio_space};
    use super::*;
    use crate::machine::MachineView;
    use crate::machine::wire::{State, WireAgent};
    use crate::tui::machines::merge;

    /// An agent of a machine in its session `$id`, pane `%id`, in `state` (`blocked`, `idle`, ...).
    fn wire_agent(id: u32, state: &str) -> WireAgent {
        WireAgent {
            pane_id: format!("%{id}"),
            session_id: format!("${id}"),
            session_name: format!("s{id}"),
            window_id: format!("@{id}"),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 1,
            pane_index: 1,
            pane_current_command: "claude".into(),
            kind: Some("claude".into()),
            state: Some(state.into()),
            seen: true,
            ..WireAgent::default()
        }
    }

    /// A machine called `name` with one session and one agent per `(id, state)`.
    pub(super) fn machine(name: &str, live: bool, agents: &[(u32, &str)]) -> MachineView {
        MachineView {
            name: name.into(),
            live,
            state: Some(State {
                spaces: agents
                    .iter()
                    .map(|&(id, _)| studio_space(id, name, "~/c"))
                    .collect(),
                panes: agents.iter().map(|&(id, _)| studio_pane(id)).collect(),
                agents: agents
                    .iter()
                    .map(|&(id, state)| wire_agent(id, state))
                    .collect(),
                worktrees: Vec::new(),
                open_worktrees: Vec::new(),
            }),
            last_seen: Some(5_000),
            ..MachineView::default()
        }
    }

    pub(super) fn app_over<'a>(cfg: &'a Config, views: &[MachineView]) -> App<'a> {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(cfg, theme, String::new(), merge(snapshot(), views, 5_000));
        app.set_worktrees(worktrees());
        app
    }

    #[test]
    fn the_walk_runs_over_every_live_machine_in_the_order_drawn() {
        let cfg = config();
        let views = [
            machine("studio", true, &[(8, "blocked")]),
            machine("pi", true, &[(9, "blocked")]),
        ];
        let mut app = app_over(&cfg, &views);
        assert_eq!(
            app.walk_stops(),
            ["%231", "%341", "studio:%8", "pi:%9"],
            "this machine first, then each machine in the config's order"
        );
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Tab));
            seen.push(pane_at(&app).to_string());
        }
        assert_eq!(seen, ["%231", "%341", "studio:%8", "pi:%9"]);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(pane_at(&app), "%231", "and round again");
    }

    #[test]
    fn a_greyed_agent_is_never_a_stop() {
        let cfg = config();
        // pi is away and its last state still holds a blocked agent: the row is drawn greyed.
        let views = [
            machine("studio", true, &[(8, "blocked")]),
            machine("pi", false, &[(9, "blocked")]),
        ];
        let mut app = app_over(&cfg, &views);
        assert_eq!(app.walk_stops(), ["%231", "%341", "studio:%8"]);
        // The positive half: the walk does reach studio, and the one after it is this machine's again.
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Tab));
            seen.push(pane_at(&app).to_string());
        }
        assert_eq!(seen, ["%231", "%341", "studio:%8", "%231"]);
        assert!(
            app.snap.panes.iter().any(|pane| pane.pane_id == "pi:%9"),
            "the greyed row is on the list: it is only not a stop"
        );
    }

    /// At the sessions-only depth a header stands for its session's agents, and a greyed machine's agents do not
    /// make its session a stop: Tab goes web's, infra's, studio's and round, never to pi's. The positive half is
    /// studio's session, which is reached; the greyed one is on the list all the same.
    #[test]
    fn a_greyed_machines_session_is_never_a_stop_at_the_sessions_depth() {
        let mut cfg = config();
        cfg.ui.depth = Depth::Sessions;
        let views = [
            machine("studio", true, &[(8, "blocked")]),
            machine("pi", false, &[(9, "blocked")]),
        ];
        let mut app = app_over(&cfg, &views);
        assert_eq!(app.depth, Depth::Sessions);
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Tab));
            seen.push(header_at(&app).to_string());
        }
        assert_eq!(seen, ["web", "infra", "studio", "web"]);
        assert!(
            app.snap
                .spaces
                .iter()
                .any(|space| space.session_name == "pi"),
            "the greyed machine's session is on the list: it is only not a stop"
        );
    }

    #[test]
    fn with_nothing_that_needs_you_every_live_agent_is_a_stop() {
        let mut snap = snapshot();
        snap.agents.retain(|agent| agent.pane_id == "%111");
        let views = [
            machine("studio", true, &[(8, "idle")]),
            machine("pi", false, &[(9, "idle")]),
        ];
        let snap = merge(snap, &views, 5_000);
        assert_eq!(super::walk::stops(&snap), ["%111", "studio:%8"]);
    }

    #[test]
    fn walk_stops_do_not_move_with_a_typed_filter() {
        let cfg = config();
        let views = [machine("studio", true, &[(8, "blocked")])];
        let mut app = app_over(&cfg, &views);
        let before = app.walk_stops();
        let shows = |app: &App, id: &str| {
            app.rows
                .rows
                .iter()
                .any(|row| matches!(row, Row::Pane(at) if app.snap.panes[*at].pane_id == id))
        };
        assert!(shows(&app, "%231"), "a stop is drawn before the filter");
        app.on_key(ch('/'));
        type_str(&mut app, "zsh");
        assert!(
            app.rows.rows.len() < app_over(&cfg, &views).rows.rows.len(),
            "the filter hid rows"
        );
        assert!(!shows(&app, "%231"), "the filter hid a stop, not only rows");
        assert_eq!(app.walk_stops(), before);
    }

    #[test]
    fn the_stop_after_a_pane_wraps_and_takes_the_first_after_a_pane_that_is_not_a_stop() {
        let views = [machine("studio", true, &[(8, "blocked")])];
        let snap = merge(snapshot(), &views, 5_000);
        let next = |own: Option<&str>| super::walk::next_after(&snap, own);
        assert_eq!(next(Some("%231")).as_deref(), Some("%341"));
        assert_eq!(next(Some("%341")).as_deref(), Some("studio:%8"));
        assert_eq!(next(Some("studio:%8")).as_deref(), Some("%231"), "wraps");
        // %311 is infra's first shell, drawn between the two stops of this machine: not a stop, in the middle.
        assert_eq!(next(Some("%311")).as_deref(), Some("%341"));
        // api's shell is drawn after both, and before studio.
        assert_eq!(next(Some("%411")).as_deref(), Some("studio:%8"));
        assert_eq!(next(None).as_deref(), Some("%231"));
        assert_eq!(
            next(Some("%999")).as_deref(),
            Some("%231"),
            "a pane no list has"
        );
    }
}

#[test]
fn ctrl_v_on_an_agent_row_pastes_the_image_into_that_agent() {
    let cfg = Config::default();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snapshot());
    let at = (0..app.rows.rows.len())
        .find(|&i| app.row_agent(&app.rows.rows[i].clone()).is_some())
        .expect("the sample world has an agent row");
    let pane = app
        .row_agent(&app.rows.rows[at].clone())
        .unwrap()
        .pane_id
        .clone();
    app.sel = at;
    assert_eq!(app.on_key(ctrl('v')), Some(Action::PasteImage { pane }));
}

#[test]
fn ctrl_v_on_a_row_that_is_not_an_agent_says_why() {
    let cfg = Config::default();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = App::new(&cfg, theme, String::new(), snapshot());
    let at = (0..app.rows.rows.len())
        .find(|&i| app.row_agent(&app.rows.rows[i].clone()).is_none())
        .expect("the sample world has a row with no agent");
    app.sel = at;
    assert_eq!(app.on_key(ctrl('v')), None);
    assert_eq!(app.live_toast(), Some(crate::image::NOT_AN_AGENT));
}

/// An agent that is waiting on a question (blocked) takes no image from the key or the menu: they say to answer it
/// first and start nothing. Working and idle agents take it. This machine's row.
#[test]
fn ctrl_v_on_a_local_agent_waiting_on_a_question_refuses() {
    let cfg = config();
    let mut app = app(&cfg);
    for (state, pasted) in [
        (AgentState::Blocked, false),
        (AgentState::Working, true),
        (AgentState::Idle, true),
    ] {
        let mut snap = app.snap.clone();
        for agent in &mut snap.agents {
            if agent.pane_id == "%221" {
                agent.state = Some(state);
            }
        }
        app.set_snapshot(snap);
        cursor_to_pane(&mut app, "%221");
        let got = app.on_key(ctrl('v'));
        if pasted {
            assert_eq!(
                got,
                Some(Action::PasteImage {
                    pane: "%221".to_string()
                }),
                "{state:?}"
            );
        } else {
            assert_eq!(got, None, "{state:?}");
            assert_eq!(
                app.live_toast(),
                Some(crate::image::WAITING_ON_A_QUESTION),
                "{state:?}"
            );
            assert_eq!(
                crate::image::WAITING_ON_A_QUESTION,
                "the agent is waiting on a question: answer it, then paste the image"
            );
            // The menu entry is the same verb and says the same.
            app.toast("");
            assert_eq!(app.row_action(RowAction::PasteImage), None);
            assert_eq!(
                app.live_toast(),
                Some(crate::image::WAITING_ON_A_QUESTION),
                "the menu entry refuses as the key does"
            );
        }
    }
}

/// The same on a row of another machine, as the list knows its agent: blocked refuses, working takes it (the
/// popup sends it over ssh itself).
#[test]
fn ctrl_v_on_an_agent_of_another_machine_waiting_on_a_question_refuses() {
    use walk_across_machines::{app_over, machine};
    let cfg = config();
    let views = [machine("studio", true, &[(8, "blocked"), (9, "working")])];
    let mut app = app_over(&cfg, &views);
    cursor_to_pane(&mut app, "studio:%9");
    assert_eq!(
        app.on_key(ctrl('v')),
        Some(Action::PasteImage {
            pane: "studio:%9".to_string()
        }),
        "a working agent of studio takes it"
    );
    cursor_to_pane(&mut app, "studio:%8");
    assert_eq!(app.on_key(ctrl('v')), None);
    assert_eq!(
        app.live_toast(),
        Some(crate::image::WAITING_ON_A_QUESTION),
        "a blocked agent of studio does not"
    );
}

/// `ctrl-v` is the default preset's key and is not the search preset's, which binds fzf's own keys and nothing
/// else: there, paste image is an entry of an agent row's right-click menu, and choosing it (by its letter or
/// by Enter on it) produces the same action the key does.
#[test]
fn the_search_preset_takes_paste_image_from_the_menu_and_not_from_a_key() {
    let cfg = search_config();
    let mut app = app(&cfg);
    cursor_to_pane(&mut app, "%221");
    let pane = "%221".to_string();
    assert_eq!(
        app.on_key(ctrl('v')),
        None,
        "no key of the search preset pastes"
    );
    assert_eq!(app.live_toast(), None, "and it says nothing");

    app.on_mouse(click(5, 3, MouseButton::Right));
    assert_eq!(
        app.on_key(ch('v')),
        Some(Action::PasteImage { pane: pane.clone() })
    );

    cursor_to_pane(&mut app, "%221");
    app.on_mouse(click(5, 3, MouseButton::Right));
    let last = menu_items(&app).len() - 1;
    for _ in 0..last {
        app.on_key(key(KeyCode::Down));
    }
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Action::PasteImage { pane })
    );
}

/// A row that is not an agent has no paste image in its menu, and the default preset's key on it says why.
#[test]
fn a_menu_without_an_agent_has_no_paste_image() {
    let cfg = config();
    let mut app = app(&cfg);
    cursor_to_header(&mut app, "infra");
    app.on_mouse(click(5, 3, MouseButton::Right));
    assert!(
        menu_items(&app)
            .iter()
            .all(|item| item.choice != RowAction::PasteImage),
        "{:?}",
        menu_items(&app)
    );
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.on_key(ctrl('v')), None);
    assert_eq!(app.live_toast(), Some(crate::image::NOT_AN_AGENT));
}

/// The default preset's hint line is the six hints it was: the paste key is in `?` and not on the line.
#[test]
fn the_hint_line_does_not_carry_the_paste_image_key() {
    for cfg in [config(), search_config()] {
        let app = app(&cfg);
        let line: String = app
            .hint_line()
            .iter()
            .map(|(key, meaning)| format!("{key} {meaning}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!line.contains("image"), "{line}");
        assert!(
            !line.contains("ctrl-v") && !line.contains("Ctrl-V"),
            "{line}"
        );
    }
}
