//! The frame, asserted against the sample world.
//!
//! Every expectation here is read off the frame drawn from [`fixtures::snapshot`], so a row's
//! columns, a chip's wording and the help's groups cannot drift without a test saying so.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;

use super::*;
use crate::config::{Config, Depth, Layout as TreeLayout};
use crate::tui::actions::Action;
use crate::tui::app::{App, JUMP_LETTERS};
use crate::tui::fixtures::{snapshot, worktrees};
use crate::tui::input::Input;
use crate::tui::overlays::{Confirm, Overlay, Scroll};
use crate::tui::theme;
use crate::tui::tree::Match;

/// The sample client: 150 columns, 39 rows of popup under a status line.
const W: u16 = 150;
const H: u16 = 39;

fn app(cfg: &Config) -> App<'_> {
    let mut app = App::new(
        cfg,
        theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
        "settings".to_string(),
        snapshot(),
    );
    app.set_worktrees(worktrees());
    app
}

fn config(toml: &str) -> Config {
    let mut cfg: Config = toml::from_str(toml).expect("the test config must parse");
    // Every frame asserted here draws the shells too; `agents` is
    // the default depth, so a test that does not ask for a depth gets `all`.
    if !toml.contains("depth") {
        cfg.ui.depth = Depth::All;
    }
    cfg
}

/// One frame, as a buffer — styles included, for the colour assertions.
fn frame(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|f| draw(f, app))
        .expect("drawing must not fail");
    terminal.backend().buffer().clone()
}

/// The drawn frame, as one string per screen line.
fn lines_of(buffer: &Buffer) -> Vec<String> {
    let area = *buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn drawn(app: &mut App, width: u16, height: u16) -> Vec<String> {
    lines_of(&frame(app, width, height))
}

/// The first screen line holding `needle`, and the column it starts at.
fn find(lines: &[String], needle: &str) -> (usize, usize) {
    lines
        .iter()
        .enumerate()
        .find_map(|(y, line)| {
            line.char_indices()
                .position(|(i, _)| line[i..].starts_with(needle))
                .map(|x| (y, x))
        })
        .unwrap_or_else(|| panic!("no line contains {needle:?}:\n{}", lines.join("\n")))
}

// ------------------------------------------------------------------ three columns

#[test]
fn a_session_header_shows_its_name_and_its_branch() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    // Colour and state live on the agents, so a header is its label, its
    // flag and its name — no rollup glyph.
    assert!(
        screen.iter().any(|l| l.contains("3   infra  main")),
        "{}",
        screen.join("\n")
    );
    // mobile is the session the popup was opened from, and carries its git flags after
    // the branch.
    assert!(
        screen
            .iter()
            .any(|l| l.contains("1 * mobile  master \u{b1}3 \u{2191}1")),
        "{}",
        screen.join("\n")
    );
}

#[test]
fn pane_rows_put_what_and_where_in_the_same_two_columns() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    // An agent pane and the plain pane beside it.
    let (_, agent_x) = find(&screen, "\u{23f5} claude     1:zsh.1");
    let (_, pane_x) = find(&screen, "\u{25a2} zsh        1:zsh.2");
    assert_eq!(agent_x, pane_x, "the glyph column must line up");
}

/// The *where* column pads but never cut, so a window renamed to something long pushed
/// the state and the title out of their columns on that row alone. A pane's *where* is
/// sized to the column now, cut with an `\u{2026}` so the cut is visible; a worktree's
/// `worktree, closed`, which is wider than the column by design, is still drawn whole.
#[test]
fn a_long_window_name_is_cut_so_the_columns_after_it_stay_aligned() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    // mobile's claude (%111) is alone in window 1, named "zsh". Rename that window to
    // seventeen columns: with its index that is nineteen, well past the eleven the column
    // allows.
    let long = "documentation-pad";
    assert_eq!(long.width(), 17);
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%111") {
        pane.window_name = long.to_string();
    }
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.window_name = long.to_string();
    }
    app.set_snapshot(snap);
    let screen = drawn(&mut app, W, H);

    // Cut to the column's own width, with the `\u{2026}` that says it was cut.
    let (cut_y, _) = find(&screen, "\u{23f5} claude     1:document\u{2026} ");
    // web's claude, whose *where* still fits: the state must start in the same column.
    let (whole_y, _) = find(&screen, "\u{23f5} claude     2:zsh.1");
    assert_eq!(
        column_of(&screen[cut_y], "working"),
        column_of(&screen[whole_y], "working"),
        "the state column moved:\n{}\n{}",
        screen[cut_y],
        screen[whole_y]
    );
}

#[test]
fn a_parked_worktree_is_a_row_of_its_own() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    assert!(
        screen
            .iter()
            .any(|l| l.contains("\u{25cc} feat/pdf   worktree, closed")),
        "{}",
        screen.join("\n")
    );
}

/// An agent whose state has not been read yet has nothing to write in the state column,
/// so the column is still drawn and reserved: otherwise its title would start twelve columns
/// early.
#[test]
fn an_agent_with_no_state_still_reserves_the_state_column() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.state = None;
    }
    app.set_snapshot(snap);
    let screen = drawn(&mut app, W, H);

    // mobile's claude has no state and web's is working: their titles line up anyway.
    let (stateless_y, _) = find(&screen, "Tidy page styles");
    let (stated_y, _) = find(&screen, "Invoice parser");
    assert_eq!(
        column_of(&screen[stateless_y], "Tidy page styles"),
        column_of(&screen[stated_y], "Invoice parser"),
        "the title column moved:\n{}\n{}",
        screen[stateless_y],
        screen[stated_y]
    );
}

#[test]
fn a_flat_layout_labels_its_rows_and_names_the_session_after_the_row() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.layout = TreeLayout::Flat;
    app.rebuild();
    let screen = drawn(&mut app, W, H);

    let (y, x) = find(&screen, "\u{23f5} claude     1:zsh.1");
    assert_eq!(
        x, 3,
        "a flat row leads with its label and a space, where a tree row has its connector"
    );
    assert!(screen[y].contains("infra"), "{}", screen[y]);
}

#[test]
fn the_sessions_depth_counts_each_state_on_the_header() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.depth = Depth::Sessions;
    app.rebuild();
    let screen = drawn(&mut app, W, H);

    // web has one done, one working and one idle agent, in the tree's order:
    // blocked, done, working, idle, unknown.
    assert!(
        screen
            .iter()
            .any(|l| l.contains("\u{2713}1  \u{23f5}1  \u{25cb}1")),
        "{}",
        screen.join("\n")
    );
}

#[test]
fn a_custom_sidebar_template_renders_herdrs_way_instead_of_the_three_columns() {
    let cfg = config("[ui.sidebar.spaces]\nrows = [[\"workspace\"]]\n");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    let (y, _) = find(&screen, "\u{2502}mobile");
    assert!(
        screen[y].starts_with("\u{2502}mobile"),
        "a configured template is drawn as herdr draws it: {:?}",
        screen[y]
    );
}

/// A token's `rules` in a real frame (herdr v0.9.1, `configuration.mdx`): a `$load` token
/// that reports `90` is red and bold, one that reports `12` is not, and a `$machine` token
/// hidden by `equals = "Local"` takes its whole row with it, so the agent that reports
/// `Local` is a line shorter than the one that reports anything else.
#[test]
fn token_rules_colour_a_token_and_a_hidden_token_removes_its_row_from_the_frame() {
    let cfg = config(
        r##"[ui.sidebar.agents]
rows = [["state_icon", "workspace", "tab"], [{ token = "$machine", rules = [{ equals = "Local", hide = true }] }], [{ token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true }] }]]
"##,
    );
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for (pane, machine, load) in [("%111", "Local", "90"), ("%121", "devbox", "12")] {
        let agent = snap
            .agents
            .iter_mut()
            .find(|a| a.pane_id == pane)
            .unwrap_or_else(|| panic!("no agent in {pane}"));
        agent
            .tokens
            .insert("machine".to_string(), machine.to_string());
        agent.tokens.insert("load".to_string(), load.to_string());
    }
    app.set_snapshot(snap);

    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let here = |needle: &str| {
        screen
            .iter()
            .position(|line| line.contains(needle))
            .map(|y| (y, find(&screen, needle).1))
    };

    // The row the rule hides is not drawn, for the one agent it hides it for.
    assert!(here("Local").is_none(), "{}", screen.join("\n"));
    let (devbox, _) = here("devbox").expect("the other agent's machine row is drawn");
    // mobile's two agents: each entry is its first row, then what is left of the others.
    let first = |pane_where: &str| {
        screen
            .iter()
            .position(|line| line.contains(pane_where))
            .unwrap_or_else(|| panic!("no row for {pane_where}:\n{}", screen.join("\n")))
    };
    let local_entry = first("mobile 1:zsh");
    assert!(
        screen[local_entry + 1].contains("90"),
        "the machine row is gone, so the load row is right under the first: {}",
        screen.join("\n")
    );
    let devbox_entry = first("mobile 2:review");
    assert_eq!(
        devbox,
        devbox_entry + 1,
        "an agent whose row is not hidden keeps it"
    );
    assert!(screen[devbox_entry + 2].contains("12"));

    // Colours: 90 is red and bold, 12 is the token's own white and plain.
    let (y, x) = here("90").expect("the load is drawn");
    let hot = &buffer[(x as u16, y as u16)];
    assert_eq!(hot.fg, ratatui::style::Color::Rgb(255, 85, 85));
    assert!(hot.modifier.contains(Modifier::BOLD));
    let (y, x) = here("12").expect("the other load is drawn");
    let calm = &buffer[(x as u16, y as u16)];
    assert_eq!(calm.fg, ratatui::style::Color::Rgb(255, 255, 255));
    assert!(!calm.modifier.contains(Modifier::BOLD));
}

// ---------------------------------------------------------------------- chrome

#[test]
fn the_border_says_how_many_agents_need_you_by_kind() {
    let cfg = config("");
    let mut app = app(&cfg);
    assert_eq!(app.hot_count(), 2);
    let screen = drawn(&mut app, W, H);

    // infra's blocked claude and web's finished codex: one of each.
    let (y, _) = find(&screen, "\u{23f8}1 \u{2713}1 need you");
    assert_eq!(y, 0, "the chip sits on the tree's top border");
}

/// The state of each agent named by pane id, `(pane, state, seen)`, in a copy of the
/// sample world: what each of the chip's cases below needs.
fn with_agents(app: &mut App, changes: &[(&str, AgentState, bool)]) {
    let mut snap = app.snap.clone();
    for &(pane, state, seen) in changes {
        let agent = snap
            .agents
            .iter_mut()
            .find(|a| a.pane_id == pane)
            .unwrap_or_else(|| panic!("no agent in {pane}"));
        agent.state = Some(state);
        agent.seen = seen;
    }
    app.set_snapshot(snap);
}

/// The chip is drawn as what needs you, by kind: each group is the state's own glyph and
/// count in the state's own colour, bold, and the words after them are the text colour, not
/// bold, so a green row never sits under a count in the blocked colour.
#[test]
fn a_blocked_agent_and_two_finished_ones_read_as_two_groups_in_their_own_colours() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    with_agents(&mut app, &[("%222", AgentState::Done, false)]);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, x) = find(&lines, "\u{23f8}1 \u{2713}2 need you");
    assert_eq!(y, 0, "on the tree's top border");
    let at = |column: usize| {
        let cell = &buffer[(column as u16, y as u16)];
        (cell.fg, cell.modifier.contains(Modifier::BOLD))
    };
    assert_eq!(
        at(x),
        (theme.attention, true),
        "the blocked glyph, in the attention colour"
    );
    assert_eq!(at(x + 1), (theme.attention, true), "and its count");
    assert_eq!(at(x + 3), (theme.green, true), "the done glyph, green");
    assert_eq!(at(x + 4), (theme.green, true), "and its count");
    assert_eq!(at(x + 6), (theme.fg, false), "the words: fg, not bold");
    assert_eq!(at(x + 13), (theme.fg, false));
}

/// One in all reads `needs you`; a lone finished agent is a green check.
#[test]
fn one_finished_agent_is_a_green_check_and_needs_you() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    // infra's blocked claude is working again, so only web's codex needs you.
    with_agents(&mut app, &[("%341", AgentState::Working, true)]);
    assert_eq!(app.hot_count(), 1);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, x) = find(&lines, "\u{2713}1 needs you");
    assert_eq!(y, 0);
    let cell = &buffer[(x as u16, y as u16)];
    assert_eq!(
        (cell.fg, cell.modifier.contains(Modifier::BOLD)),
        (theme.green, true)
    );
    assert!(
        !lines[0].contains("\u{23f8}"),
        "no blocked group when nothing is blocked: {}",
        lines[0]
    );
}

/// The dots setting gives every state the same glyph, so the groups differ by colour alone.
#[test]
fn the_dots_setting_draws_the_chip_as_dots_in_the_two_colours() {
    let cfg = config("[ui]\nstatus_indicators = \"dots\"\n");
    let mut app = app(&cfg);
    let theme = app.theme;
    with_agents(&mut app, &[("%222", AgentState::Done, false)]);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, x) = find(&lines, "\u{25cf}1 \u{25cf}2 need you");
    assert_eq!(y, 0);
    assert_eq!(buffer[(x as u16, y as u16)].fg, theme.attention, "blocked");
    assert_eq!(buffer[(x as u16 + 3, y as u16)].fg, theme.green, "done");
}

/// A border too narrow for the whole chip drops the words first, then groups from the least
/// urgent end, and never half of one: at every width the chip is one of the four whole
/// readings, and the most urgent group is the last to go.
#[test]
fn a_chip_too_wide_for_the_border_drops_its_words_and_then_whole_groups() {
    let cfg = config("");
    let mut app = app(&cfg);
    with_agents(&mut app, &[("%222", AgentState::Done, false)]);
    let title = " sessions ";
    let text = |width: u16| -> Option<String> {
        need_chip(&app, width, title).map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
    };
    // The room is the panel's width less its corners, its title and a column of air.
    assert_eq!(
        text(29).as_deref(),
        Some(" \u{23f8}1 \u{2713}2 need you "),
        "room 16"
    );
    assert_eq!(
        text(28).as_deref(),
        Some(" \u{23f8}1 \u{2713}2 "),
        "room 15: no words"
    );
    assert_eq!(text(20).as_deref(), Some(" \u{23f8}1 \u{2713}2 "), "room 7");
    assert_eq!(
        text(19).as_deref(),
        Some(" \u{23f8}1 "),
        "room 6: one group"
    );
    assert_eq!(text(17).as_deref(), Some(" \u{23f8}1 "), "room 4");
    assert_eq!(text(16), None, "room 3: not even one group");
    for width in 0..80 {
        if let Some(chip) = text(width) {
            assert!(
                [
                    " \u{23f8}1 \u{2713}2 need you ",
                    " \u{23f8}1 \u{2713}2 ",
                    " \u{23f8}1 "
                ]
                .contains(&chip.as_str()),
                "width {width}: {chip:?}"
            );
        }
    }
}

#[test]
fn the_bottom_border_names_the_layout_and_the_depth_when_they_are_not_the_default() {
    let cfg = config("");
    let mut app = app(&cfg);
    assert!(
        !drawn(&mut app, W, H).join("\n").contains(" flat "),
        "the default tree layout wears no chip"
    );

    app.layout = TreeLayout::Flat;
    app.rebuild();
    let width = tree_width(&app, W) as usize;
    let screen = drawn(&mut app, W, H);
    let (y, x) = find(&screen, " flat ");
    assert_eq!(
        y,
        screen.len() - 2,
        "the chip sits on the tree's bottom border"
    );
    assert_eq!(
        x + " flat ".chars().count(),
        width - 1,
        "and at its right end, just inside the corner: {:?}",
        screen[y]
    );

    app.layout = TreeLayout::Tree;
    app.depth = Depth::Agents;
    app.rebuild();
    assert!(drawn(&mut app, W, H).join("\n").contains(" agents "));
}

#[test]
fn the_chip_says_sessions_not_flat_when_a_flat_layout_sits_at_the_sessions_depth() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.layout = TreeLayout::Flat;
    app.depth = Depth::Sessions;
    app.rebuild();
    let screen = drawn(&mut app, W, H).join("\n");
    assert!(screen.contains(" sessions "), "{screen}");
    assert!(!screen.contains(" flat "), "{screen}");
}

/// The search preset's filter never closes, so it is a line of the panel rather than a
/// hint: it is drawn as the tree's own first row.
#[test]
fn the_search_preset_draws_its_query_inside_the_tree_panel() {
    let cfg = config("[keys]\npreset = \"search\"\n");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    assert!(
        screen[1].starts_with("\u{2502} > \u{258f}search\u{2026}"),
        "{:?}",
        screen[1]
    );
    let hints = screen.last().expect("a hint line").clone();
    assert!(
        !hints.contains('\u{258f}'),
        "and not on the hint line too, which keeps its keys: {hints}"
    );

    // What is typed shows there, filters the tree, and the rows start below it.
    for c in "gemini".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let screen = drawn(&mut app, W, H);
    assert!(
        screen[1].starts_with("\u{2502} > gemini\u{258f}"),
        "{:?}",
        screen[1]
    );
    assert!(!screen[1].contains("search\u{2026}"), "{:?}", screen[1]);
    assert!(screen[2].contains("web"), "{:?}", screen[2]);
    assert!(screen[3].contains("gemini"), "{:?}", screen[3]);
}

/// The query row is not a row, so a click on it must not select whatever row would
/// otherwise have landed at that line.
#[test]
fn the_search_presets_query_row_shifts_the_rows_without_shifting_the_clicks() {
    let cfg = config("[keys]\npreset = \"search\"\n");
    let mut app = app(&cfg);
    drawn(&mut app, W, H);

    let top = app.hits.tree.area.y;
    assert!(
        app.hits.tree.rows.iter().all(|(y, _)| *y > top),
        "no row is mapped to the query line"
    );
    let (y, index) = app.hits.tree.rows[3];
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: app.hits.tree.area.x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.sel, index);
}

#[test]
fn the_preview_title_carries_the_directory_of_the_row_the_cursor_is_on() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, W, H);

    // The cursor opens on your own row, the pane in mobile the popup was
    // opened from. The separator is the middle dot, as in the tree's `sessions · flat`.
    assert!(
        screen[0].contains("preview \u{b7} ~/code/mobile"),
        "{}",
        screen[0]
    );
}

/// tmux reports a session's path whole (`/home/you/code/api`), and a parked worktree's record
/// holds its checkout whole too: under the home folder the title writes either the way the
/// project picker does, `~/code/api`, and a path anywhere else stays whole.
#[test]
fn the_preview_title_writes_a_path_under_home_with_a_tilde() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.home = std::path::PathBuf::from("/home/you");
    let cursor = app
        .selected_space()
        .expect("the cursor opens in a session")
        .session_id
        .clone();
    let set_cursor_path = |app: &mut App, path: &str| {
        for space in &mut app.snap.spaces {
            if space.session_id == cursor {
                space.session_path = std::path::PathBuf::from(path);
            }
        }
    };

    set_cursor_path(&mut app, "/home/you/code/api");
    let screen = drawn(&mut app, W, H);
    assert!(
        screen[0].contains("preview \u{b7} ~/code/api "),
        "{}",
        screen[0]
    );

    set_cursor_path(&mut app, "/srv/code/api");
    let screen = drawn(&mut app, W, H);
    assert!(
        screen[0].contains("preview \u{b7} /srv/code/api "),
        "{}",
        screen[0]
    );

    let parked = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::ClosedWorktree { .. }))
        .expect("the fixture parks a worktree");
    let Row::ClosedWorktree { record, .. } = app.rows.rows[parked] else {
        unreachable!()
    };
    app.worktrees[record].path = std::path::PathBuf::from("/home/you/code/api--fix-login");
    app.sel = parked;
    let screen = drawn(&mut app, W, H);
    assert!(
        screen[0].contains("preview \u{b7} ~/code/api--fix-login "),
        "{}",
        screen[0]
    );
}

/// A wheel notch decrements `preview_scroll` against whatever `preview_max_scroll` was
/// last set to; if the pane's wrapped line count shrinks (a resize, or the content itself
/// shrinking) while scrolled up, a stale `preview_scroll` above the new max would eat
/// several notches before the wheel visibly moves anything. The next frame must clamp the
/// field itself, not just the value it windows with.
#[test]
fn a_stale_preview_offset_is_clamped_by_the_next_frame_so_the_wheel_never_sticks() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.preview = Some(preview::to_text("one\ntwo\nthree"));
    app.preview_scroll = 50; // scrolled far up on content that has since shrunk
    let _ = drawn(&mut app, W, H);
    assert!(
        app.preview_scroll <= app.preview_max_scroll,
        "the frame wrote the clamp back: {} <= {}",
        app.preview_scroll,
        app.preview_max_scroll
    );

    let before = app.preview_scroll;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: app.hits.preview.x + 1,
        row: app.hits.preview.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.preview_scroll,
        before.saturating_sub(1),
        "one notch down moves one row at once"
    );
}

/// `capture-pane` pads a pane's screen to its height with blank rows, and the window drops
/// them before it draws. The wheel's limit must not count them either: each one would be a
/// notch that moves nothing, at the top and again on the way back down.
#[test]
fn the_wheel_limit_counts_only_what_the_window_shows_not_the_blank_padding() {
    let cfg = config("");
    let mut app = app(&cfg);
    // Ten lines of output on a fifty-row pane, the shape `capture-pane -p` prints.
    let screen: Vec<String> = (1..=10)
        .map(|n| format!("line {n}"))
        .chain(std::iter::repeat_n(String::new(), 40))
        .collect();
    app.set_preview(
        app.preview_pane(),
        Some(preview::to_text(&screen.join("\n"))),
    );
    let wheel = |app: &App, kind| MouseEvent {
        kind,
        column: app.hits.preview.x + 1,
        row: app.hits.preview.y + 1,
        modifiers: KeyModifiers::NONE,
    };

    // A panel taller than the ten lines: nothing to scroll, and a notch up records nothing.
    let _ = frame(&mut app, W, H);
    assert!(app.hits.preview.height - 2 > 10, "{:?}", app.hits.preview);
    assert_eq!(app.preview_max_scroll, 0);
    app.on_mouse(wheel(&app, MouseEventKind::ScrollUp));
    assert_eq!(app.preview_scroll, 0);

    // A panel of four rows: six lines above it, and every notch moves the window.
    let mut before = drawn(&mut app, W, 7);
    assert_eq!(app.hits.preview.height - 2, 4, "{:?}", app.hits.preview);
    assert_eq!(app.preview_max_scroll, 6);
    for notch in 1..=6 {
        app.on_mouse(wheel(&app, MouseEventKind::ScrollUp));
        let now = drawn(&mut app, W, 7);
        assert_ne!(now, before, "notch {notch} up moved nothing");
        before = now;
    }
    assert!(
        before.iter().any(|l| l.contains("line 1 "))
            && !before.iter().any(|l| l.contains("line 10")),
        "six notches up is the top, lines 1 to 4:\n{}",
        before.join("\n")
    );
    app.on_mouse(wheel(&app, MouseEventKind::ScrollUp));
    assert_eq!(app.preview_scroll, 6, "never past the top");
    app.on_mouse(wheel(&app, MouseEventKind::ScrollDown));
    assert_ne!(
        drawn(&mut app, W, 7),
        before,
        "the first notch back down moves at once"
    );
}

#[test]
fn no_row_is_drawn_past_the_tree_panels_own_border() {
    let cfg = config("");
    let mut app = app(&cfg);
    let width = tree_width(&app, W);
    let screen = drawn(&mut app, W, H);

    for line in screen.iter().skip(1).take(app.rows.rows.len()) {
        let column = line.chars().nth(width as usize - 1);
        assert_eq!(
            column,
            Some('\u{2502}'),
            "a row spilled past column {width}: {line:?}"
        );
    }
}

// ------------------------------------------------------------------ hint line

/// The six default hints, and the letters of the
/// cursor's session right after the numbers: the cursor opens on mobile's claude, and
/// mobile has five rows.
#[test]
fn the_hint_line_carries_the_six_default_hints() {
    let cfg = config("");
    let mut app = app(&cfg);
    assert_eq!(app.hint_line().len(), 7);
    let screen = drawn(&mut app, W, H);
    let hints = screen.last().expect("a hint line").clone();
    assert!(
        hints
            .trim_start()
            .starts_with("1-9 session  \u{b7}  a-e rows"),
        "{hints}"
    );

    for expected in [
        "1-9 session",
        "a-e rows",
        "Tab next up",
        "j/k move",
        // `Enter` always goes to the row under the cursor, full stop.
        "Enter go",
        "/ filter",
        "? help",
    ] {
        assert!(hints.contains(expected), "missing {expected}: {hints}");
    }
}

#[test]
fn a_hint_line_too_long_for_the_popup_wraps_to_a_second_line() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, 44, 20);

    // The letters' entry follows the numbers, and the walk's after it; at 44 columns `Enter
    // go` is the first to go to the second line.
    let tail = &screen[screen.len() - 2..];
    assert!(tail[0].contains("1-9 session"), "{tail:?}");
    assert!(tail[0].contains("a-e rows"), "{tail:?}");
    assert!(tail[0].contains("Tab next up"), "{tail:?}");
    assert!(tail[1].starts_with(" Enter go"), "{tail:?}");
    assert!(tail[1].contains("move"), "{tail:?}");
}

/// The hint line is one line wherever it was before the letters' entry was made the same in
/// every session: `a-e rows` is shorter than `a-e its rows`, so the whole line, 91 columns with
/// its leading space, fits from a popup 92 wide, and the tree keeps every row (a longer
/// `a-e session's rows` would wrap at 96 to 101 columns and cost the tree a row).
#[test]
fn the_default_hint_line_is_one_line_from_92_columns() {
    let cfg = config("");
    for width in [92u16, 96, 100, 101] {
        let mut app = app(&cfg);
        let screen = drawn(&mut app, width, 30);
        let last = screen.last().expect("a hint line");
        assert!(
            last.contains("1-9 session") && last.contains("? help"),
            "{width} columns: {last}"
        );
    }
    let mut app = app(&cfg);
    let screen = drawn(&mut app, 91, 30);
    assert!(
        !screen.last().expect("a hint line").contains("1-9 session"),
        "91 columns is one short, and the line wraps"
    );
}

#[test]
fn the_filter_shows_the_query_with_a_cursor_and_a_placeholder_while_it_is_empty() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(app.typing());
    let screen = drawn(&mut app, W, H);
    let hints = screen.last().expect("a hint line").clone();

    assert!(hints.contains("/\u{258f}"), "{hints}");
    assert!(hints.contains("search\u{2026}"), "{hints}");
}

/// The hint line of a question only `y` answers names its second key whole, `n, Enter or Esc`,
/// in the key colour and bold, and what it does, `cancel`, dim: not `n,` alone as the key with
/// the rest of the key read as what it does.
#[test]
fn the_strict_questions_hint_keeps_its_keys_whole_in_the_key_colour() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.ask(Confirm::strict(
        "git will not remove feat/pdf: it has modified or untracked files. remove anyway? uncommitted changes are lost",
        Action::RemoveWorktree {
            workspace: "w6".to_string(),
            force: true,
        },
    ));
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "n, Enter or Esc cancel");
    let at = |x: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        (cell.fg, cell.modifier.contains(Modifier::BOLD))
    };

    for offset in 0.."n, Enter or Esc".len() {
        assert_eq!(
            at(x + offset),
            (app.theme.yellow, true),
            "column {offset} of its keys: {}",
            screen[y]
        );
    }
    let said = x + "n, Enter or Esc ".len();
    for offset in 0.."cancel".len() {
        assert_eq!(
            at(said + offset),
            (app.theme.dim, false),
            "column {offset} of what they do: {}",
            screen[y]
        );
    }
}

/// The cursor is drawn where it is, not at the end of the line whatever it is: Left, ctrl-b,
/// ctrl-f, ctrl-a and ctrl-e visibly move it, and what is typed goes in at it.
#[test]
fn the_filter_draws_its_cursor_where_it_is() {
    let cfg = config("");
    let mut app = app(&cfg);
    let press = |app: &mut App, code: KeyCode, modifiers: KeyModifiers| {
        app.on_key(KeyEvent::new(code, modifiers));
    };
    let hints = |app: &mut App| drawn(app, W, H).last().expect("a hint line").clone();
    press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "abcd".chars() {
        press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert!(
        hints(&mut app).contains("/abcd\u{258f}"),
        "{}",
        hints(&mut app)
    );

    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert!(
        hints(&mut app).contains("/abc\u{258f}d"),
        "{}",
        hints(&mut app)
    );
    press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(
        hints(&mut app).contains("/ab\u{258f}cd"),
        "{}",
        hints(&mut app)
    );
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert!(
        hints(&mut app).contains("/\u{258f}abcd"),
        "{}",
        hints(&mut app)
    );
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert!(
        hints(&mut app).contains("/a\u{258f}bcd"),
        "{}",
        hints(&mut app)
    );
    press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert!(
        hints(&mut app).contains("/abcd\u{258f}"),
        "{}",
        hints(&mut app)
    );

    // What is typed goes in at the cursor, and the cursor stays after it.
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('X'), KeyModifiers::SHIFT);
    assert!(
        hints(&mut app).contains("/abcX\u{258f}d"),
        "{}",
        hints(&mut app)
    );
}

/// The search preset's query line is the same input, and draws its cursor the same way.
#[test]
fn the_search_presets_query_line_draws_its_cursor_where_it_is() {
    let cfg = config("[keys]\npreset = \"search\"\n");
    let mut app = app(&cfg);
    for c in "gemini".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let query = |app: &mut App| drawn(app, W, H)[1].clone();
    assert!(query(&mut app).starts_with("\u{2502} > gemini\u{258f}"));

    app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(
        query(&mut app).starts_with("\u{2502} > gemi\u{258f}ni"),
        "{:?}",
        query(&mut app)
    );
    app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(
        query(&mut app).starts_with("\u{2502} > \u{258f}gemini"),
        "{:?}",
        query(&mut app)
    );
}

#[test]
fn a_toast_replaces_the_hint_line() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.toast("kill-pane -t \"web:2.1\"");
    let screen = drawn(&mut app, W, H);

    assert!(
        screen.last().expect("a hint line").contains("kill-pane -t"),
        "{screen:?}"
    );
}

#[test]
fn a_broken_keys_table_says_so_once_on_the_first_frame() {
    let cfg = config("[keys]\nnot_an_action = \"x\"\n");
    let mut app = app(&cfg);
    assert!(!app.key_diagnostics.is_empty());
    let screen = drawn(&mut app, W, H);

    assert!(
        screen
            .last()
            .expect("a hint line")
            .contains("unknown action"),
        "{screen:?}"
    );
}

// ------------------------------------------------------------- highlighting

#[test]
fn the_query_highlights_the_characters_it_matched() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.filter = Some(Input::new("gemini"));
    app.rebuild();
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let (y, x) = find(&screen, "gemini");
    let cell = &buffer[(x as u16, y as u16)];
    assert_eq!(cell.fg, theme.yellow, "a matched character is yellow");
    assert!(
        cell.modifier.contains(Modifier::BOLD),
        "a matched character is bold"
    );
    // The glyph two columns before the match keeps the row's own colour.
    assert_ne!(buffer[(x as u16 - 2, y as u16)].fg, theme.yellow);
}

/// A match in the end of a long window name, which the *where* column cuts away, would
/// leave the row on screen with nothing lit, and nothing would say why it was there. The `…` that
/// marks the cut is lit as a matched character is instead; a match in the part drawn lights
/// its own characters and leaves the `…` as the row draws it.
#[test]
fn a_match_in_the_cut_end_of_a_long_window_name_lights_the_cut() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    // mobile's claude (%111), alone in window 1: `1:documentation-pad` is drawn
    // `1:document…`, and `pad` is all in the part cut away.
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%111") {
        pane.window_name = "documentation-pad".to_string();
    }
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.window_name = "documentation-pad".to_string();
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    let lit = |buffer: &Buffer, x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
    };

    app.filter = Some(Input::new("pad"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "1:document\u{2026}");
    assert!(lit(&buffer, x + 10, y), "the cut is lit: {}", screen[y]);
    for at in 0..10 {
        assert!(
            !lit(&buffer, x + at, y),
            "column {at} is not: {}",
            screen[y]
        );
    }

    app.filter = Some(Input::new("docu"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "1:document\u{2026}");
    for at in 2..6 {
        assert!(lit(&buffer, x + at, y), "column {at} is lit: {}", screen[y]);
    }
    assert!(!lit(&buffer, x + 10, y), "the cut is not: {}", screen[y]);
}

/// The *what* column cut a long name silently: an agent named `documentation-writer` read
/// `documentat`, a name it does not have. It is cut as the *where*
/// column is, with an `\u{2026}`, on every row and whether or not a filter is on, and the place
/// column after it stays in line with every other row's. A plain pane's long program is cut
/// the same way.
#[test]
fn a_long_name_is_cut_with_an_ellipsis_whether_or_not_a_filter_is_on() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.name = Some("documentation-writer".to_string());
    }
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%131") {
        pane.pane_current_command = "docker-compose".to_string();
    }
    app.set_snapshot(snap);

    let screen = drawn(&mut app, W, H);
    // Both needles hold their row's name column whole and then its place: starting in one
    // column, their places are in line too.
    let (y, x) = find(&screen, "documenta\u{2026} 1:zsh");
    let (_, review) = find(&screen, "claude     2:review");
    assert_eq!(x, review, "the place column is in line: {}", screen[y]);
    find(&screen, "\u{25a2} docker-co\u{2026} 3:nvim");

    app.filter = Some(Input::new("docu"));
    app.rebuild();
    let screen = drawn(&mut app, W, H);
    find(&screen, "documenta\u{2026} 1:zsh");
}

/// A match in the cut end of a long name lights the `\u{2026}`, as one in a long window name's
/// does: `/writer` keeps `documentation-writer` on screen with nothing of `documenta` lit, so
/// the lit cut says why it is there. A match in the part drawn lights its own letters and
/// leaves the cut as the row draws it.
#[test]
fn a_match_in_the_cut_end_of_a_long_name_lights_the_cut() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.name = Some("documentation-writer".to_string());
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    let lit = |buffer: &Buffer, x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
    };

    app.filter = Some(Input::new("writer"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "documenta\u{2026}");
    assert!(lit(&buffer, x + 9, y), "the cut is lit: {}", screen[y]);
    for at in 0..9 {
        assert!(
            !lit(&buffer, x + at, y),
            "letter {at} is not: {}",
            screen[y]
        );
    }

    app.filter = Some(Input::new("docu"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "documenta\u{2026}");
    for at in 0..4 {
        assert!(lit(&buffer, x + at, y), "letter {at} is lit: {}", screen[y]);
    }
    assert!(!lit(&buffer, x + 9, y), "the cut is not: {}", screen[y]);
}

/// A match in an agent's title lights the title's own letters where the row draws it, the rest of
/// the title keeping its colour.
#[test]
fn the_query_highlights_the_characters_it_matched_in_a_title() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.filter = Some(Input::new("styles"));
    app.rebuild();
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let lit = |x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
    };

    let (y, x) = find(&screen, "Tidy page styles");
    for at in 0..16 {
        let word = (10..16).contains(&at);
        assert_eq!(lit(x + at, y), word, "letter {at}: {}", screen[y]);
    }
    assert!(!lit(x + 16, y), "the cut after it is not: {}", screen[y]);
}

/// What the row of `needle` shows lit: the symbols of its yellow bold cells, in order. The row's
/// jump letter `a` is lit while the query matches it, so it comes first.
fn lit_symbols(buffer: &Buffer, screen: &[String], needle: &str, theme: &Theme) -> String {
    let y = screen
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no row with {needle:?}: {screen:?}"));
    (0..W)
        .filter(|&x| {
            let cell = &buffer[(x, y as u16)];
            cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
        })
        .map(|x| buffer[(x, y as u16)].symbol().to_string())
        .collect()
}

/// The matcher counts a text that is not ASCII by grapheme and the row is drawn by character: a
/// title with a warning sign and its emoji selector (two characters, one position) before the
/// match lights the match's own letters, not the ones before them.
#[test]
fn a_title_with_a_multi_character_grapheme_before_the_match_lights_the_match() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.pane_title = "\u{26a0}\u{fe0f} fix flaky".to_string();
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    app.filter = Some(Input::new("flaky"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    assert_eq!(lit_symbols(&buffer, &screen, "fix flaky", &theme), "aflaky");
}

/// The matcher counts the row's own words in bytes when every grapheme of them starts with an
/// ASCII character (an accent or an emoji selector adds bytes, not positions), so the title
/// starts that many positions later: the title's first letters light the title's, never the end
/// of the row's own words (the state word's last letter).
fn the_title_lights_after_own_words(window: &str) {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%111") {
        pane.window_name = window.to_string();
    }
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.window_name = window.to_string();
        agent.pane_title = "fix flaky".to_string();
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    app.filter = Some(Input::new("fix"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    assert_eq!(
        lit_symbols(&buffer, &screen, "fix flaky", &theme),
        "afix",
        "the title's letters light and nothing else: {screen:?}"
    );
}

/// Two NFD accents in the row's own words (a window name `Re\u{301}sume\u{301}`) before a title.
#[test]
fn a_title_after_two_nfd_accents_lights_its_own_letters_and_not_the_state_word() {
    the_title_lights_after_own_words("Re\u{301}sume\u{301}");
}

/// A letter and U+FE0F in the row's own words (a window name `zsh` and a selector) before a title.
#[test]
fn a_title_after_a_letter_and_an_emoji_selector_lights_its_own_letters() {
    the_title_lights_after_own_words("zs\u{fe0f}h");
}

/// The same for a window name drawn in the *where* column, here a ZWJ sequence, three emoji
/// joined into one grapheme, in front of the match.
#[test]
fn a_window_name_with_a_multi_character_grapheme_before_the_match_lights_the_match() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    let name = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}docs";
    for pane in snap.panes.iter_mut().filter(|p| p.pane_id == "%111") {
        pane.window_name = name.to_string();
    }
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.window_name = name.to_string();
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    app.filter = Some(Input::new("docs"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    assert_eq!(lit_symbols(&buffer, &screen, "docs", &theme), "adocs");
}

/// A match in the end of a title the panel cuts away lights the `\u{2026}` that marks the cut, as a
/// long name's does: the row is on screen because of letters nobody can see. A match in the part
/// drawn lights its own letters and leaves the cut as the row draws it.
#[test]
fn a_match_in_the_cut_end_of_a_title_lights_the_cut() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.pane_title =
            "Reconcile the ledger against the bank statement for october".to_string();
    }
    app.set_snapshot(snap);
    let theme = app.theme;

    app.filter = Some(Input::new("october"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, _) = find(&screen, "Reconcile the");
    let end = screen[y].find('\u{2026}').expect("the title is cut");
    let x = screen[y][..end].chars().count();
    let cell = &buffer[(x as u16, y as u16)];
    assert_eq!(cell.fg, theme.yellow, "the cut is lit: {}", screen[y]);
    assert!(cell.modifier.contains(Modifier::BOLD), "{}", screen[y]);
    assert!(
        !screen[y].contains("october"),
        "the match is in what is cut: {}",
        screen[y]
    );

    // A match in the part drawn lights its letters, and the cut is as the row draws it.
    app.filter = Some(Input::new("recon"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "Reconcile the");
    for at in 0..9 {
        let cell = &buffer[((x + at) as u16, y as u16)];
        let lit = cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD);
        assert_eq!(lit, at < 5, "letter {at}: {}", screen[y]);
    }
    let end = screen[y].find('\u{2026}').expect("still cut");
    let cut = &buffer[(screen[y][..end].chars().count() as u16, y as u16)];
    assert_ne!(cut.fg, theme.yellow, "the cut is not lit: {}", screen[y]);
}

/// The cut counts the columns of a title by grapheme as the screen draws them: a warning sign and
/// its emoji selector are two columns, though their characters add up to one. A match at the
/// title's end lights the cut exactly when the letters are not drawn, however many of these
/// come before it.
#[test]
fn a_wide_grapheme_in_a_title_does_not_hide_the_cut_from_a_match_after_it() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let mut hidden = 0;
    let mut shown = 0;
    for signs in 0..10 {
        let mut snap = app.snap.clone();
        for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
            agent.pane_title = format!("{}fix zq", "\u{26a0}\u{fe0f}".repeat(signs));
        }
        app.set_snapshot(snap);
        app.filter = Some(Input::new("zq"));
        app.rebuild();
        let buffer = frame(&mut app, W, H);
        let screen = lines_of(&buffer);
        let (y, _) = find(&screen, "a \u{2514} \u{23f5} claude");
        let drawn = screen[y].contains("zq");
        let Some(x) = (0..W).find(|&x| buffer[(x, y as u16)].symbol() == "\u{2026}") else {
            assert!(drawn, "{signs}: no cut and no match: {}", screen[y]);
            shown += 1;
            continue;
        };
        let cell = &buffer[(x, y as u16)];
        let lit = cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD);
        assert_eq!(lit, !drawn, "{signs} signs: {}", screen[y]);
        if lit {
            hidden += 1;
        }
    }
    assert!(hidden > 0 && shown > 0, "{hidden} hidden, {shown} shown");
}

/// A row a configured template draws lights nothing, and its cut is not lit either: the positions
/// the matcher gives for the template's text are not the title's, though the template ends in the
/// title. Here the match is in a long token the template draws before the title, which the cut
/// takes the end of.
#[test]
fn a_templated_row_ending_in_the_title_does_not_light_its_cut() {
    let cfg = config(
        r##"[ui.sidebar.agents]
rows = [["state_icon", "$load", "terminal_title_stripped"]]
"##,
    );
    let mut app = app(&cfg);
    let theme = app.theme;
    let mut snap = app.snap.clone();
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.tokens.insert("load".to_string(), "q".repeat(58));
        agent.pane_title = "t".repeat(40);
    }
    app.set_snapshot(snap);
    app.filter = Some(Input::new("qq"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, _) = find(&screen, "qqqq");
    let x = (0..W)
        .find(|&x| buffer[(x, y as u16)].symbol() == "\u{2026}")
        .unwrap_or_else(|| panic!("the row is cut: {}", screen[y]));
    let cell = &buffer[(x, y as u16)];
    assert_ne!(cell.fg, theme.yellow, "the cut is not lit: {}", screen[y]);
}

/// A flat list writes each row's session name at its end, having no header to hang it under,
/// and the filter matches that name first in the row: a row matched by
/// it lights its letters there, so `/notes` shows why notes' two panes are listed. Under
/// `/nvim` the same row's name is not lit, the match being the program's.
#[test]
fn a_flat_row_matched_by_its_session_lights_the_name_at_its_end() {
    let cfg = config("[ui]\nlayout = \"flat\"\n");
    let mut app = app(&cfg);
    assert!(app.flat_now());
    let theme = app.theme;
    let lit = |buffer: &Buffer, x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
    };
    let row = format!("{:<11}  notes", "1:nvim");

    app.filter = Some(Input::new("notes"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, &row);
    for at in 0..5 {
        assert!(
            lit(&buffer, x + 13 + at, y),
            "letter {at} is lit: {}",
            screen[y]
        );
    }

    app.filter = Some(Input::new("nvim"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, &row);
    for at in 0..5 {
        assert!(
            !lit(&buffer, x + 13 + at, y),
            "letter {at} is not: {}",
            screen[y]
        );
    }
}

/// The filter matches the state word a row draws so the letters of it
/// the query matched are lit as any matched letters are: `/blocked` lights the whole word
/// on infra's blocked claude and not the time after it, and `/claude` lights the name and
/// leaves the state word as the row draws it.
#[test]
fn a_match_in_the_state_word_lights_its_letters() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let lit = |buffer: &Buffer, x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD)
    };

    app.filter = Some(Input::new("blocked"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "4:zsh       blocked ");
    let state = x + 12;
    for at in 0..7 {
        assert!(
            lit(&buffer, state + at, y),
            "letter {at} is lit: {}",
            screen[y]
        );
    }
    assert!(
        !lit(&buffer, state + 8, y),
        "the time is not: {}",
        screen[y]
    );

    app.filter = Some(Input::new("claude"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "4:zsh       blocked ");
    for at in 0..7 {
        assert!(
            !lit(&buffer, x + 12 + at, y),
            "letter {at} is not: {}",
            screen[y]
        );
    }
}

// ----------------------------------------------------------------- overlays

/// The help fits one screen. On the sample client all of it is up at
/// once, in either preset: the `prefix+O` line whole (in the modal preset the same next up
/// as the walk's line above it, the longest of the keys' lines; in the search preset, which
/// has no key for the walk, saying what next up is), the mouse, the legend's tree line note
/// and the legend's last line, with nothing scrolled.
#[test]
fn the_whole_help_fits_the_sample_client_without_scrolling() {
    for toml in ["", "[keys]\npreset = \"search\"\n"] {
        let cfg = config(toml);
        let mut app = app(&cfg);
        app.overlay = Overlay::Help(Scroll::default());
        let screen = drawn(&mut app, W, H).join("\n");
        if toml.is_empty() {
            assert!(
                screen
                    .contains("next up: who to deal with next (what needs you, else every agent)"),
                "the walk's line, whole:\n{screen}"
            );
        }

        for expected in [
            if toml.is_empty() {
                "the same next up from anywhere in tmux, straight there without the popup"
            } else {
                "next up from anywhere in tmux: what needs you, else every agent"
            },
            "scroll the tree, or the preview under the pointer",
            "tree line: white on each session's open window, blue down your own session",
            "window 2, named zsh, pane 1, the way tmux writes it",
            "in a container that taw started, or saw start",
        ] {
            assert!(
                screen.contains(expected),
                "{toml:?}: missing {expected:?}:\n{screen}"
            );
        }
        // And the box leaves the hint line under it, which says how to leave.
        let hint = screen.lines().last().unwrap_or_default();
        assert!(hint.contains("Esc close"), "{toml:?}: {hint:?}\n{screen}");
    }
}

#[test]
fn the_help_overlay_groups_the_keys_and_carries_the_legend() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.overlay = Overlay::Help(Scroll::default());
    let screen = drawn(&mut app, W, H).join("\n");

    // Each group's title, right inside the box's left border.
    for expected in [
        "\u{2502} move ",
        "\u{2502} go ",
        "\u{2502} act on the row ",
        "\u{2502} mouse ",
        "\u{2502} legend ",
    ] {
        assert!(
            screen.contains(expected),
            "missing group {expected}:\n{screen}"
        );
    }
    for expected in [
        "next up: who to deal with next (what needs you, else every agent)",
        "jump to that session (yellow: a key that works now)",
        "jump to a row of your session  \u{b7}  row a of session 4, in the search too",
        "flat: one list of what is in view (agents, or every pane), most urgent first",
        "the same next up from anywhere in tmux, straight there without the popup",
        "new session  \u{b7}  new worktree",
        "menu: go, rename, close, new worktree, paste image",
        "waiting on you",
        "a worktree whose session is closed",
    ] {
        assert!(screen.contains(expected), "missing {expected}:\n{screen}");
    }
}

/// prefix+O is drawn on the line under Tab's, so it reads as the row above's action. Drawn by the popup from its config: the line under Tab's
/// says it is the same next up, outside the popup; with `[keys]` taking both of next up's
/// keys away that row is gone, and the line stands where it was and says what next up is.
#[test]
fn the_help_says_prefix_o_is_the_same_next_up_on_the_line_under_tab() {
    let cfg = config("");
    let mut popup = app(&cfg);
    popup.overlay = Overlay::Help(Scroll::default());
    let screen = drawn(&mut popup, W, H);
    let (tab, _) = find(&screen, "Tab / Shift-Tab");
    let (chord, _) = find(&screen, "prefix+O");
    assert_eq!(chord, tab + 1, "under Tab's line:\n{}", screen.join("\n"));
    assert!(
        screen[chord]
            .contains("the same next up from anywhere in tmux, straight there without the popup"),
        "{}",
        screen[chord]
    );

    let cfg = config("[keys]\nnext_attention = []\nprevious_attention = []\n");
    let mut popup = app(&cfg);
    popup.overlay = Overlay::Help(Scroll::default());
    let screen = drawn(&mut popup, W, H);
    assert!(
        !screen.iter().any(|line| line.contains("the same next up")),
        "the same as nothing:\n{}",
        screen.join("\n")
    );
    let (chord, _) = find(&screen, "prefix+O");
    assert!(
        screen[chord].contains("next up from anywhere in tmux: what needs you, else every agent"),
        "{}",
        screen[chord]
    );
    assert!(
        screen[chord - 1].contains("row a of session 4, in the search too"),
        "where the next up line was: {}",
        screen[chord - 1]
    );
}

#[test]
fn the_help_overlay_names_the_keys_that_are_actually_bound() {
    let cfg = config("[keys]\nnew_session = \"N\"\n");
    let mut app = app(&cfg);
    app.overlay = Overlay::Help(Scroll::default());
    let screen = drawn(&mut app, W, H);

    let (y, _) = find(&screen, "new session  \u{b7}  new worktree");
    assert!(
        screen[y].contains('N'),
        "the overridden key must be named: {}",
        screen[y]
    );
}

#[test]
fn the_help_legend_colours_each_glyph_by_its_state() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.overlay = Overlay::Help(Scroll::default());
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let (y, x) = find(&screen, "\u{23f8} blocked");
    assert_eq!(buffer[(x as u16, y as u16)].fg, theme.attention);
    let (y, x) = find(&screen, "\u{23f5} working");
    assert_eq!(buffer[(x as u16, y as u16)].fg, theme.blue);
}

// -------------------------------------------------------------- the machinery

#[test]
fn the_recorded_geometry_lets_a_click_select_the_row_that_was_drawn() {
    let cfg = config("");
    let mut app = app(&cfg);
    drawn(&mut app, W, H);

    let (y, index) = *app
        .hits
        .tree
        .rows
        .get(3)
        .expect("the tree drew at least four rows");
    let x = app.hits.tree.area.x;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });

    assert_eq!(app.sel, index);
}

#[test]
fn the_whole_popup_draws_at_absurdly_small_sizes_with_an_overlay_open() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.overlay = Overlay::Help(Scroll::default());
    for (width, height) in [(1, 1), (4, 3), (20, 6), (40, 10), (200, 60)] {
        drawn(&mut app, width, height);
    }
}

#[test]
fn the_tree_widens_to_its_longest_row_within_the_spec_bounds() {
    let cfg = config("");
    let app = app(&cfg);
    let width = tree_width(&app, W);

    assert!(
        (TREE_MIN..=TREE_MAX).contains(&width),
        "{width} is outside {TREE_MIN}..={TREE_MAX}"
    );
    assert_eq!(tree_width(&app, 20), 20, "never past the popup itself");
}

#[test]
fn a_configured_sidebar_width_raises_the_trees_floor_and_its_ceiling_with_it() {
    // Under the floor the three columns stop lining up, so a small `sidebar_width` is
    // ignored and the panel fits its widest row, capped at `max(sidebar_width, TREE_MAX)`.
    // The rows carry a title, so the fixture's longest one reaches that cap:
    // only a `sidebar_width` above it widens the panel further, and then it is the floor
    // that decides.
    for (configured, expected) in [(26, TREE_MAX), (44, TREE_MAX), (99, 99)] {
        let cfg = config(&format!("[ui]\nsidebar_width = {configured}\n"));
        let mut app = app(&cfg);
        assert_eq!(
            tree_width(&app, W),
            expected,
            "sidebar_width = {configured}"
        );
        // And it draws: a floor above the popup's own width must not panic.
        drawn(&mut app, 40, 10);
    }
}

/// The tree's cap is 64 columns, which on the narrowest popup that shows both panels would
/// leave the preview 34 columns of pane to show. The preview has a floor of its own, and the
/// tree is what gives way for it.
#[test]
fn the_preview_keeps_its_floor_and_the_tree_gives_way_on_a_narrow_popup() {
    let cfg = config("");
    let mut app = app(&cfg);
    assert!(app.show_preview, "the preview is on by default");

    // 100 columns is `WIDE_COLUMNS` itself, the narrowest popup drawing both panels.
    let narrow = tree_width(&app, WIDE_COLUMNS);
    assert_eq!(
        narrow,
        WIDE_COLUMNS - PREVIEW_MIN,
        "the tree is capped so the preview keeps {PREVIEW_MIN} columns"
    );

    // 150 columns has room for both, so `TREE_MAX` is still what caps the tree there.
    assert_eq!(
        tree_width(&app, W),
        TREE_MAX,
        "a wide popup is unaffected by the preview's floor"
    );

    // Hide the preview and the tree has the whole narrow popup to widen into again.
    app.show_preview = false;
    assert_eq!(tree_width(&app, WIDE_COLUMNS), TREE_MAX);

    // And the narrow popup draws, with the preview beside the tree.
    app.show_preview = true;
    drawn(&mut app, WIDE_COLUMNS, 30);
}

/// `[ui] status_indicators = "dots"` is herdr's plainer alphabet: one `●` per agent,
/// coloured by state, rather than a glyph per state.
#[test]
fn the_dots_indicator_draws_one_glyph_for_every_state() {
    let cfg = config("[ui]\nstatus_indicators = \"dots\"\n");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let (y, x) = find(&screen, "\u{25cf} claude     1:zsh.1");
    assert_eq!(
        buffer[(x as u16, y as u16)].fg,
        theme.blue,
        "still by state"
    );
    assert!(
        !screen.iter().any(|l| l.contains('\u{23f5}')),
        "no symbol glyphs survive under dots:\n{}",
        screen.join("\n")
    );
}

#[test]
fn truncate_cuts_to_the_available_columns_with_an_ellipsis() {
    assert_eq!(truncate("abcdef", 10), "abcdef");
    assert_eq!(truncate("abcdef", 6), "abcdef");
    assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
    assert_eq!(truncate("abcdef", 0), "");
}

#[test]
fn pad_fills_short_text_and_cuts_long_text_to_the_column() {
    assert_eq!(pad("zsh", 10), "zsh       ");
    assert_eq!(pad("claude", 10), "claude    ");
    assert_eq!(pad("a-very-long-agent-name", 10), "a-very-lon");
}

#[test]
fn the_viewport_follows_the_cursor_without_pushing_the_list_off_the_bottom() {
    let heights = vec![1u16; 10];

    // The cursor below the fold drags the viewport the smallest distance.
    assert_eq!(keep_visible(&heights, 7, 0, 4, true), 4);
    // The wheel keeps its own offset while nothing moved.
    assert_eq!(keep_visible(&heights, 7, 2, 4, false), 2);
    // ...but never past the last legal offset.
    assert_eq!(
        keep_visible(&heights, 0, 99, 4, false),
        max_scroll(&heights, 4)
    );
    assert_eq!(max_scroll(&heights, 4), 6);
    assert_eq!(max_scroll(&heights, 20), 0);
}

/// The list comes first: in a very small terminal window the user should see the list of
/// agents, not the preview. A popup made too
/// narrow for both panels draws the tree across the whole width and no preview, whatever it
/// opened as: here it opens wide, with the preview beside the tree, and the window is made
/// small.
#[test]
fn a_popup_too_narrow_for_both_panels_draws_the_tree_alone() {
    for (width, height) in [(WIDE_COLUMNS - 1, 30), (60, 20)] {
        let cfg = config("");
        let mut app = app(&cfg);
        let wide = drawn(&mut app, W, H);
        assert!(
            wide[0].contains(" preview"),
            "the preview is beside the tree while both fit:\n{}",
            wide[0]
        );

        let screen = drawn(&mut app, width, height);
        let text = screen.join("\n");
        assert!(
            !text.contains(" preview"),
            "{width}x{height}: no preview panel, and not its placeholder:\n{text}"
        );
        // One panel, and it runs the whole width.
        assert!(
            screen[0].starts_with("\u{256d} sessions") && screen[0].ends_with('\u{256e}'),
            "{width}x{height}: {}",
            screen[0]
        );
        assert_eq!(screen[0].matches('\u{256d}').count(), 1, "{}", screen[0]);
        assert_eq!(
            screen[0].chars().count(),
            usize::from(width),
            "{}",
            screen[0]
        );
        // The rows to navigate, and the hint line under them.
        for row in ["mobile", "web"] {
            assert!(text.contains(row), "{width}x{height}: no {row}:\n{text}");
        }
        let hint = screen[screen.len() - 2..].join(" ");
        assert!(
            hint.contains("1-9 session") && hint.contains("Tab next up"),
            "{width}x{height}: {hint}"
        );
        assert_eq!(
            app.hits.tree.area.width,
            width - 2,
            "the tree's rows take the whole width, borders aside"
        );
        assert_eq!(app.hits.preview, Rect::default(), "no preview to hit");
    }
}

/// The narrowest popup that draws both panels, `WIDE_COLUMNS` itself.
#[test]
fn both_panels_show_once_both_fit() {
    let cfg = config("");
    let mut app = app(&cfg);
    let screen = drawn(&mut app, WIDE_COLUMNS, 30);

    assert_eq!(screen[0].chars().count(), usize::from(WIDE_COLUMNS));
    assert_eq!(screen[0].matches('\u{256d}').count(), 2, "{}", screen[0]);
    assert!(screen[0].contains(" preview"), "{}", screen[0]);
    assert!(screen.iter().any(|l| l.contains("mobile")));
    assert_ne!(
        app.hits.preview,
        Rect::default(),
        "the preview is there to hit"
    );
    assert!(app.hits.tree.area.width < WIDE_COLUMNS);
}

/// A click on a tree row in a narrow popup selects it: the tree is there to click.
#[test]
fn a_click_on_a_tree_row_in_a_narrow_popup_selects_it() {
    let cfg = config("");
    let mut app = app(&cfg);
    drawn(&mut app, W, H);
    drawn(&mut app, 60, 20);

    assert!(
        !app.hits.tree.rows.is_empty(),
        "the tree's rows are clickable"
    );
    let (y, index) = *app
        .hits
        .tree
        .rows
        .iter()
        .find(|&&(_, index)| index != app.sel)
        .expect("a row other than the cursor's");
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: app.hits.tree.area.x + 2,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.sel, index);
}

/// The preview's last area is not left behind when it stops being drawn: a wheel notch over
/// where it was scrolls the tree, not a preview nobody can see.
#[test]
fn the_wheel_does_not_scroll_a_preview_that_is_no_longer_drawn() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.preview = Some(preview::to_text(&"a line of the pane\n".repeat(80)));
    drawn(&mut app, W, H);
    let (column, row) = (app.hits.preview.x + 1, app.hits.preview.y + 1);
    assert!(
        app.preview_max_scroll > 0,
        "the preview has history to scroll"
    );

    // Just under `WIDE_COLUMNS`, so the spot where the preview was is still on the screen.
    drawn(&mut app, WIDE_COLUMNS - 1, 30);
    assert!(
        column < WIDE_COLUMNS - 1,
        "the old preview's corner is inside the narrow popup"
    );
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        app.preview_scroll, 0,
        "the preview is not what the wheel moved"
    );
}

/// Nothing about the size a popup opened at is remembered: made wide enough for both, it
/// shows the preview beside the tree.
#[test]
fn a_popup_opened_narrow_shows_the_preview_once_it_is_made_wide_enough() {
    let cfg = config("");
    let mut app = app(&cfg);
    let narrow = drawn(&mut app, 60, 20);
    assert!(!narrow.join("\n").contains(" preview"));

    let wide = drawn(&mut app, W, H);
    assert!(wide[0].contains(" preview"), "{}", wide[0]);
    assert_eq!(wide[0].matches('\u{256d}').count(), 2, "{}", wide[0]);
}

/// `[ui] preview = false` stays off at any width, and the tree then has the whole width.
#[test]
fn a_user_who_turned_the_preview_off_keeps_it_off_at_any_width() {
    let cfg = config("[ui]\npreview = false\n");
    let mut app = app(&cfg);
    for (width, height) in [(W, H), (WIDE_COLUMNS, 30), (60, 20)] {
        let screen = drawn(&mut app, width, height);
        assert!(
            !screen.join("\n").contains(" preview"),
            "{width}x{height}:\n{}",
            screen.join("\n")
        );
        assert_eq!(
            screen[0].chars().count(),
            usize::from(width),
            "{}",
            screen[0]
        );
    }
}

#[test]
fn an_empty_tree_says_what_would_fill_it() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.filter = Some(Input::new("qqq"));
    app.rebuild();
    let screen = drawn(&mut app, W, H).join("\n");

    assert!(screen.contains("nothing matches"), "{screen}");
    assert!(screen.contains("creates a session"), "{screen}");
}

/// The empty tree names the key that makes a session only when one is bound. The search
/// preset binds none, and its `n` is a letter of the query: the line said `n creates a
/// session` there, which typed an n. A `[keys]` entry brings the key back, its own.
#[test]
fn an_empty_tree_names_the_key_that_makes_a_session_only_when_one_is_bound() {
    for (toml, line) in [
        ("[keys]\npreset = \"search\"\n", " nothing matches \"qqq\""),
        (
            "[keys]\npreset = \"search\"\nnew_session = \"ctrl+g\"\n",
            " nothing matches \"qqq\"  \u{b7}  ctrl-g creates a session",
        ),
        ("", " nothing matches \"qqq\"  \u{b7}  n creates a session"),
    ] {
        let cfg = config(toml);
        let mut app = app(&cfg);
        app.filter = Some(Input::new("qqq"));
        app.rebuild();
        let screen = drawn(&mut app, W, H);
        let row = screen
            .iter()
            .find(|l| l.contains("nothing matches"))
            .unwrap_or_else(|| panic!("{toml:?}:\n{}", screen.join("\n")));
        // The line as the panel draws it: its border, the words, then the panel's padding.
        let words = row
            .trim_start_matches('\u{2502}')
            .split('\u{2502}')
            .next()
            .unwrap_or_default()
            .trim_end();
        assert_eq!(words, line, "{toml:?}");
    }
}

/// Colour is the only thing separating a row's three columns, so it is asserted rather
/// than left to the eye: the glyph is its state's colour, the agent is the plain
/// foreground (colour goes to the rows that need you), and where it lives
/// is dim. On the window its session has open the row is lit instead: the agent and its place are white and bold, and the rest is white.
#[test]
fn a_pane_row_colours_its_glyph_its_agent_and_its_target_differently() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);
    let at = |x: usize, y: usize| {
        let cell = &buffer[(x as u16, y as u16)];
        (cell.fg, cell.modifier.contains(Modifier::BOLD))
    };

    // mobile's active window is 1 and this idle claude sits in window 2: a quiet row.
    let (y, x) = find(&screen, "\u{25cb} claude     2:review");
    assert_eq!(
        at(x, y).0,
        theme.state_color(Some(AgentState::Idle)),
        "an idle agent's glyph"
    );
    assert_eq!(
        at(x + 2, y),
        (theme.fg, false),
        "a quiet agent column is the plain foreground"
    );
    assert_eq!(at(x + 13, y), (theme.dim, false), "where it lives is dim");

    // infra's active window is 1 (fixture `cur: 1`), exactly where this claude sits.
    let (y, x) = find(&screen, "\u{23f5} claude     1:zsh.1");
    assert_eq!(at(x, y).0, theme.blue, "a working agent's glyph is blue");
    assert_eq!(at(x + 2, y), (theme.fg, true), "the open window's agent");
    assert_eq!(at(x + 13, y), (theme.fg, true), "the open window's place");
    let tail = column_of(&screen[y], "working");
    assert_eq!(at(tail, y), (theme.fg, false), "the open window's state");
}

// ------------------------------------------------------------- the tree's columns

/// The column a needle starts at within one drawn line: its character index, which is its
/// screen column while every glyph the tree draws is one cell wide. [`find`] does the same
/// across a whole screen; this is for the second and third needle of one row.
fn column_of(line: &str, needle: &str) -> usize {
    line.char_indices()
        .position(|(i, _)| line[i..].starts_with(needle))
        .unwrap_or_else(|| panic!("no {needle:?} in {line:?}"))
}

#[test]
fn elapsed_labels_read_like_a_clock() {
    assert_eq!(elapsed_label(100, 130), "now");
    assert_eq!(elapsed_label(100, 100 + 38 * 60), "38m");
    assert_eq!(elapsed_label(100, 100 + 2 * 3600 + 5), "2h");
    assert_eq!(elapsed_label(100, 100 + 3 * 86_400), "3d");
    assert_eq!(
        elapsed_label(500, 100),
        "now",
        "a clock that went backwards is not an error"
    );
}

#[test]
fn a_header_is_label_flag_and_a_bold_accent_name_without_a_glyph() {
    let cfg = config("");
    let mut app = app(&cfg);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);

    // The label, then the flag: `*` on the session the popup was opened from, `-` on the
    // one before it, a space on every other. (A bare `mobile` also appears in the
    // preview's title, so each header is found by its own first three columns.)
    let (y, x) = find(&lines, "1 * mobile");
    find(&lines, "4 - api");
    let (infra, _) = find(&lines, "3   infra");
    for glyph in ["\u{23f8}", "\u{23f5}", "\u{2713}", "\u{25cb}"] {
        assert!(
            !lines[infra].contains(glyph) && !lines[y].contains(glyph),
            "headers carry no state glyph: {:?}",
            lines[infra]
        );
    }
    // Name colour: accent, bold.
    let cell = &buffer[((x + 4) as u16, y as u16)];
    assert_eq!(cell.fg, app.theme.accent);
    assert!(cell.modifier.contains(Modifier::BOLD));
}

#[test]
fn your_own_sessions_line_and_star_are_accent_coloured_and_the_others_are_dim() {
    let cfg = config("");
    let mut app = app(&cfg);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    // mobile is the session you came from (fixture: current_session = "$1"). Its claude in
    // window 2 is outside the window it has open, so its line is the accent, bold: the open
    // window's own line is white ([`the_open_windows_tree_line_is_white_in_every_session`]).
    let (y, _) = find(&lines, "Review PR");
    let x = column_of(&lines[y], "\u{251c}") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.accent, "own connector");
    assert!(buffer[(x, y as u16)].modifier.contains(Modifier::BOLD));
    // infra's blocked claude sits outside the window infra has open: dim, though it needs you.
    let (y, _) = find(&lines, "Error report");
    let x = column_of(&lines[y], "\u{2514}") as u16;
    assert_eq!(
        buffer[(x, y as u16)].fg,
        app.theme.dim,
        "another session's connector"
    );
    let (y, _) = find(&lines, "* mobile");
    let x = column_of(&lines[y], "*") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.accent, "the star");
}

/// The colour and weight of the connector, `\u{251c}` or `\u{2514}`, on screen line `y`: the
/// first one the line has, which is the tree's own.
fn connector_at(buffer: &Buffer, lines: &[String], y: usize) -> (Color, bool) {
    let x = lines[y]
        .chars()
        .position(|c| c == '\u{251c}' || c == '\u{2514}')
        .unwrap_or_else(|| panic!("no connector on line {y}: {:?}", lines[y]));
    let cell = &buffer[(x as u16, y as u16)];
    (cell.fg, cell.modifier.contains(Modifier::BOLD))
}

/// The tree line marks the open window: on every row of the window a
/// session has open, the connector is `fg`, bold, in every session, the one you came from
/// included. Every other row keeps its line as before, dim, and the accent, bold, in your own
/// session. Only the window decides: a row that needs you and a parked worktree take no
/// colour of their own there.
#[test]
fn the_open_windows_tree_line_is_white_in_every_session() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let white = (theme.fg, true);
    let dim = (theme.dim, false);
    let accent = (theme.accent, true);
    // The connectors of the `n` rows under the header that starts with `header`.
    let under = |header: &str, n: usize| -> Vec<(Color, bool)> {
        let (y, _) = find(&lines, header);
        (y + 1..=y + n)
            .map(|row| connector_at(&buffer, &lines, row))
            .collect()
    };
    // mobile, where you are, has window 1 open: its claude. The rest are the accent.
    assert_eq!(
        under("1 * mobile", 5),
        [white, accent, accent, accent, accent],
        "mobile"
    );
    // web has window 2 open, its claude and its gemini; its shell, its codex (which needs
    // you), its server and its parked worktree are outside it.
    assert_eq!(
        under("2   web", 6),
        [dim, white, white, dim, dim, dim],
        "web"
    );
    // infra has window 1 open, two panes; its blocked claude is in window 4.
    assert_eq!(
        under("3   infra", 5),
        [white, white, dim, dim, dim],
        "infra"
    );
    assert_eq!(under("4 - api", 1), [white], "api's one window is open");
    assert_eq!(under("5   notes", 2), [white, dim], "notes");
}

/// The mark follows the window a session has open, and the flat list, which has no
/// connectors, has none to light.
#[test]
fn the_white_line_moves_with_the_open_window_and_the_flat_list_has_none() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let mut snap = app.snap.clone();
    snap.spaces
        .iter_mut()
        .find(|s| s.session_id == "$2")
        .expect("web")
        .active_window_id = "@23".to_string();
    app.set_snapshot(snap);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    // web's window 3 is the open one now, and its codex needs you: the text stays green,
    // the line is white; gemini's window is not open.
    let (y, _) = find(&lines, "fix flaky");
    assert_eq!(connector_at(&buffer, &lines, y), (theme.fg, true), "codex");
    let (y, _) = find(&lines, "gemini");
    assert_eq!(
        connector_at(&buffer, &lines, y),
        (theme.dim, false),
        "gemini"
    );

    let cfg = config("[ui]\nlayout = \"flat\"\n");
    let mut app = self::app(&cfg);
    let lines = drawn(&mut app, W, H);
    assert!(
        !lines
            .iter()
            .any(|l| l.contains('\u{251c}') || l.contains('\u{2514}')),
        "a flat list has no connectors:\n{}",
        lines.join("\n")
    );
}

#[test]
fn agent_rows_hang_on_connectors_and_say_state_elapsed_and_title() {
    let cfg = config("");
    let mut app = app(&cfg);
    let lines = lines_of(&frame(&mut app, W, H));

    let (y, _) = find(&lines, "\u{2514} \u{23f8} claude");
    let blocked = &lines[y];
    assert!(blocked.contains("4:zsh"), "{blocked}");
    assert!(blocked.contains("blocked "), "the state word: {blocked}");
    // The title is the last column, so it is the one the panel's cap cuts: a long
    // title such as "Error report review and fixes" arrives here as its first sixteen columns, two fewer
    // because a child row starts two columns in.
    assert!(blocked.contains("Error report rev\u{2026}"), "{blocked}");
    assert!(blocked.contains('\u{2026}'), "cut, not dropped: {blocked}");

    let (y, _) = find(&lines, "Migrate auth");
    let working = &lines[y];
    assert!(
        working.contains("\u{251c} \u{23f5} claude"),
        "an inner child on \u{251c}: {working}"
    );
    assert!(working.contains("working "), "{working}");
}

#[test]
fn a_blocked_row_is_attention_coloured_and_bold_end_to_end_a_quiet_row_is_not() {
    let cfg = config("");
    let mut app = app(&cfg);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);

    let (y, _) = find(&lines, "\u{2514} \u{23f8} claude");
    for needle in ["claude", "4:zsh", "blocked", "Error report"] {
        let x = column_of(&lines[y], needle) as u16;
        let cell = &buffer[(x, y as u16)];
        assert_eq!(cell.fg, app.theme.attention, "{needle}");
        assert!(cell.modifier.contains(Modifier::BOLD), "{needle}");
    }

    // A quiet row outside its session's open window: mobile's idle claude, in window 2
    // (infra's "Migrate auth" claude sits in infra's open window, which lights its row).
    let (y, _) = find(&lines, "Review PR");
    let x = column_of(&lines[y], "claude") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.fg);
    assert!(!buffer[(x, y as u16)].modifier.contains(Modifier::BOLD));
    let x = column_of(&lines[y], "Review") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.dim);
}

#[test]
fn a_done_row_is_green_until_looked_at() {
    let cfg = config("");
    let mut app = app(&cfg);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, _) = find(&lines, "fix flaky tests");
    let x = column_of(&lines[y], "codex") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.green);

    let mut snap = app.snap.clone();
    snap.agents
        .iter_mut()
        .find(|a| a.pane_id == "%231")
        .unwrap()
        .seen = true;
    app.set_snapshot(snap);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, _) = find(&lines, "fix flaky tests");
    let x = column_of(&lines[y], "codex") as u16;
    assert_eq!(buffer[(x, y as u16)].fg, app.theme.fg);
}

#[test]
fn a_folded_header_shows_counts_and_the_title_of_its_agent_that_needs_you() {
    let cfg = config("[ui]\ndepth = \"sessions\"\n");
    let mut app = app(&cfg);
    let lines = lines_of(&frame(&mut app, W, H));

    let (y, _) = find(&lines, "infra");
    let infra = &lines[y];
    assert!(infra.contains("\u{23f8}1"), "{infra}");
    assert!(infra.contains("\u{23f5}1"), "{infra}");
    assert!(infra.contains("Error report review and fixes"), "{infra}");

    let (y, _) = find(&lines, "web");
    let web = &lines[y];
    // blocked, done, working, idle, unknown: done before working.
    let done = web.find("\u{2713}1").unwrap();
    let working = web.find("\u{23f5}1").unwrap();
    assert!(done < working, "{web}");
    assert!(web.contains("fix flaky tests"), "{web}");
}

#[test]
fn titles_and_labels_can_be_switched_off() {
    let cfg = config("[ui]\ntitles = false\nlabels = false\n");
    let mut app = app(&cfg);
    let lines = lines_of(&frame(&mut app, W, H));

    // With titles on, the blocked claude's row ends in its title, cut at the panel's cap:
    // `Error report rev\u{2026}` ([`agent_rows_hang_on_connectors_and_say_state_elapsed_and_title`]).
    assert!(!lines.iter().any(|l| l.contains("Error report")));
    let (y, _) = find(&lines, "infra");
    let infra = &lines[y];
    assert!(!infra.contains('3'), "{infra}");
    let hint = lines
        .iter()
        .find(|l| l.contains("Tab next up"))
        .expect("a hint line");
    assert!(!hint.contains("1-9"), "{hint}");
}

#[test]
fn the_need_chip_is_coloured_by_kind_and_the_hint_line_leads_with_the_numbers() {
    let cfg = config("");
    let mut app = app(&cfg);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);

    // The chip's blocked group is the attention colour, as the blocked row is; the words are
    // the text colour ([`a_blocked_agent_and_two_finished_ones_read_as_two_groups_in_their_own_colours`]).
    let (y, x) = find(&lines, "\u{23f8}1");
    assert_eq!(buffer[(x as u16, y as u16)].fg, app.theme.attention);
    let (y, x) = find(&lines, "need you");
    assert_eq!(buffer[(x as u16, y as u16)].fg, app.theme.fg);

    let hint = lines
        .iter()
        .rev()
        .find(|l| l.contains("Tab next up"))
        .expect("a hint line");
    assert!(hint.trim_start().starts_with("1-9 session"), "{hint}");
    assert!(hint.contains("Enter go"), "{hint}");
}

#[test]
fn a_flat_list_numbers_its_rows_and_skips_parked_worktrees() {
    let cfg = config("[ui]\nlayout = \"flat\"\nagent_panel_sort = \"sessions\"\n");
    let mut app = app(&cfg);
    let lines = lines_of(&frame(&mut app, W, H));

    let (y, _) = find(&lines, "Tidy page");
    assert!(lines[y].contains("1 \u{23f5} claude"), "{}", lines[y]);
    let (y, _) = find(&lines, "worktree, closed");
    assert!(
        !lines[y]
            .trim_start()
            .starts_with(|c: char| c.is_ascii_digit()),
        "{}",
        lines[y]
    );
}

// ------------------------------------------------------------- the preview's wrap

#[test]
fn the_preview_wraps_a_long_line_of_an_agent_pane_and_cuts_a_full_screen_one() {
    let cfg = config("");
    let mut app = app(&cfg);
    let long = "x".repeat(200);
    app.set_preview(app.preview_pane(), Some(preview::to_text(&long)));
    // The cursor opens on your own agent pane, which is not in the alternate screen.
    assert!(app.preview_wraps());
    let screen = drawn(&mut app, W, H);
    let rows_with_x = screen.iter().filter(|l| l.contains("xxxxxxxxxx")).count();
    assert!(
        rows_with_x >= 2,
        "200 columns wrap onto at least two preview rows:\n{}",
        screen.join("\n")
    );

    let mut snap = app.snap.clone();
    for p in &mut snap.panes {
        p.alternate_on = true;
    }
    app.set_snapshot(snap);
    app.set_preview(app.preview_pane(), Some(preview::to_text(&long)));
    assert!(!app.preview_wraps());
    let screen = drawn(&mut app, W, H);
    let rows_with_x = screen.iter().filter(|l| l.contains("xxxxxxxxxx")).count();
    assert_eq!(
        rows_with_x,
        1,
        "a full-screen program is cut, not wrapped:\n{}",
        screen.join("\n")
    );
}

/// The preview draws a pane in the pane's own colours. What the pane leaves at the terminal's
/// default colours, before a colour and after a reset alike, is drawn at the terminal's default
/// colours, so a translucent terminal shows through the preview as it does through the pane; a
/// colour the pane sets is kept; the panel's border keeps the theme's background.
#[test]
fn the_preview_leaves_what_the_pane_leaves_at_the_default_colours_to_the_terminal() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    assert_ne!(
        theme.panel_bg,
        Color::Reset,
        "the default theme paints its panels"
    );
    app.set_preview(
        app.preview_pane(),
        Some(preview::to_text("plain \u{1b}[41mboxed\u{1b}[0m after")),
    );
    let buffer = frame(&mut app, W, H);
    let (y, x) = find(&lines_of(&buffer), "plain boxed after");
    let cell = |column: usize| &buffer[(column as u16, y as u16)];
    for (column, what) in [
        (x, "text before any colour"),
        (x + "plain ".len() - 1, "the space before the colour"),
        (x + "plain boxed ".len(), "text after the reset"),
        (x + "plain boxed after ".len(), "the rest of the row"),
    ] {
        assert_eq!(
            (cell(column).fg, cell(column).bg),
            (Color::Reset, Color::Reset),
            "{what} is at the terminal's default colours"
        );
    }
    assert_eq!(
        cell(x + "plain ".len()).bg,
        Color::Red,
        "a colour the pane sets is kept"
    );
    assert_eq!(
        cell(x - 1).bg,
        theme.panel_bg,
        "the border keeps the theme's background"
    );
}

// -------------------------------------------------------------- the open window

/// The colour and weight of the cell where `needle` starts on screen line `y`.
fn style_of(buffer: &Buffer, lines: &[String], y: usize, needle: &str) -> (Color, bool) {
    let x = column_of(&lines[y], needle) as u16;
    let cell = &buffer[(x, y as u16)];
    (cell.fg, cell.modifier.contains(Modifier::BOLD))
}

/// A row of the window its session has open
/// is lit whole — its kind and its place in `fg`, bold, and its tail (the state word, the
/// elapsed time, the title) in `fg` at normal weight. Every other row keeps its quiet
/// colours: the kind `fg`, the place and the tail `dim`.
#[test]
fn the_open_windows_rows_are_lit_whole_and_the_others_stay_quiet() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    // web's active window is 2 (fixture `cur: 2`): its two agents sit in window 2, the
    // codex in window 3. gemini's row is quiet (idle, seen) so nothing else colours it.
    let (y, _) = find(&lines, "gemini");
    assert_eq!(
        style_of(&buffer, &lines, y, "gemini"),
        (theme.fg, true),
        "the open window's kind"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "2:zsh.2"),
        (theme.fg, true),
        "the open window's place"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "idle"),
        (theme.fg, false),
        "the open window's tail: fg, not bold"
    );
    // The codex is hot (done, unseen): it keeps the attention colour end to end.
    let (y, _) = find(&lines, "fix flaky");
    let x = column_of(&lines[y], "3:tests") as u16;
    assert_eq!(
        buffer[(x, y as u16)].fg,
        theme.state_color(Some(AgentState::Done))
    );
    // mobile's active window is 1; its idle claude sits in window 2: quiet.
    let (y, _) = find(&lines, "Review PR");
    assert_eq!(
        style_of(&buffer, &lines, y, "claude"),
        (theme.fg, false),
        "another window's kind: fg, not bold"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "2:review"),
        (theme.dim, false),
        "another window's place"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "idle"),
        (theme.dim, false),
        "another window's state"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "Review PR"),
        (theme.dim, false),
        "another window's title"
    );
}

#[test]
fn a_plain_pane_in_the_open_window_is_lit_whole_at_the_all_depth() {
    let cfg = config("[ui]\ndepth = \"all\"\n");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    // api ($4) has one window (index 1) with a plain shell; it is the open one. Its
    // header is found by name rather than by the plain-pane glyph, which also marks
    // mobile's and infra's own plain panes earlier in the tree.
    let (header, _) = find(&lines, "4 - api");
    let y = header + 1;
    // Every column that is dim on other rows is lit on the open window's: a plain pane's
    // glyph too.
    assert_eq!(
        style_of(&buffer, &lines, y, "\u{25a2}"),
        (theme.fg, false),
        "the open window's glyph"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "zsh"),
        (theme.fg, true),
        "the open window's program"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "1:zsh"),
        (theme.fg, true),
        "the open window's place"
    );
    // mobile's nvim sits in window 3, which mobile does not have open: a plain pane
    // there keeps its glyph, its program and its place dim, as every plain pane did before.
    let (y, _) = find(&lines, "3:nvim");
    assert_eq!(
        style_of(&buffer, &lines, y, "\u{25a2}"),
        (theme.dim, false),
        "another window's glyph"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "nvim"),
        (theme.dim, false),
        "another window's program"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "3:nvim"),
        (theme.dim, false),
        "another window's place"
    );
}

/// The flat list lights the same rows, the session name each flat row carries included:
/// every column that is dim on other rows is `fg` on the open window's rows, an agent's
/// and a plain pane's alike.
#[test]
fn a_flat_list_lights_the_open_windows_rows_the_same_way() {
    let cfg = config("[ui]\nlayout = \"flat\"\n");
    let mut app = app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let (y, _) = find(&lines, "gemini");
    assert_eq!(style_of(&buffer, &lines, y, "gemini"), (theme.fg, true));
    assert_eq!(style_of(&buffer, &lines, y, "2:zsh.2"), (theme.fg, true));
    assert_eq!(style_of(&buffer, &lines, y, "idle"), (theme.fg, false));
    assert_eq!(
        style_of(&buffer, &lines, y, "web"),
        (theme.fg, false),
        "the open window's session name"
    );
    let (y, _) = find(&lines, "Review PR");
    assert_eq!(style_of(&buffer, &lines, y, "claude"), (theme.fg, false));
    assert_eq!(style_of(&buffer, &lines, y, "2:review"), (theme.dim, false));
    assert_eq!(style_of(&buffer, &lines, y, "idle"), (theme.dim, false));
    assert_eq!(
        style_of(&buffer, &lines, y, "mobile"),
        (theme.dim, false),
        "another window's session name"
    );
    assert_eq!(
        style_of(&buffer, &lines, y, "Review PR"),
        (theme.dim, false)
    );

    // api's shell is its open window's one pane; web's shell sits in window 1, which web
    // does not have open.
    let pane_row = |session: &str| {
        lines
            .iter()
            .position(|l| {
                l.contains("\u{25a2} zsh        1:zsh ") && l.contains(&format!("  {session}"))
            })
            .unwrap_or_else(|| panic!("{session}'s shell row:\n{}", lines.join("\n")))
    };
    let y = pane_row("api");
    for (needle, bold) in [
        ("\u{25a2}", false),
        ("zsh", true),
        ("1:zsh", true),
        ("api", false),
    ] {
        assert_eq!(
            style_of(&buffer, &lines, y, needle),
            (theme.fg, bold),
            "api's open shell: {needle}"
        );
    }
    let y = pane_row("web");
    for needle in ["\u{25a2}", "zsh", "1:zsh", "web"] {
        assert_eq!(
            style_of(&buffer, &lines, y, needle),
            (theme.dim, false),
            "web's shell in another window: {needle}"
        );
    }
}

/// A row that needs you is drawn in its attention colour end to end, and being in the open
/// window adds nothing to it: web with its codex's window open instead, the codex (done,
/// unseen) stays green and at normal weight in every column.
#[test]
fn a_hot_row_in_the_open_window_keeps_its_colour_and_gains_nothing() {
    let cfg = config("");
    let mut app = app(&cfg);
    let theme = app.theme;
    let mut snap = app.snap.clone();
    snap.spaces
        .iter_mut()
        .find(|s| s.session_id == "$2")
        .expect("web")
        .active_window_id = "@23".to_string();
    app.set_snapshot(snap);
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    let green = theme.state_color(Some(AgentState::Done));
    let (y, _) = find(&lines, "fix flaky");
    for needle in ["codex", "3:tests", "done", "fix flaky"] {
        assert_eq!(
            style_of(&buffer, &lines, y, needle),
            (green, false),
            "{needle}"
        );
    }
    // And gemini's window is not the open one, so its row is quiet again.
    let (y, _) = find(&lines, "gemini");
    assert_eq!(style_of(&buffer, &lines, y, "gemini"), (theme.fg, false));
    assert_eq!(style_of(&buffer, &lines, y, "2:zsh.2"), (theme.dim, false));
}

/// A plain key, as the app tests' `ch` builds it: the numbers and letters these press.
fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// The letters show beside the first five rows of every session, and only the cursor's
/// session's are lit (so that you can think about the letter to
/// press before pressing the number, without going to the session to find out what it is). At the shipped `agents` depth the popup opens
/// on mobile's claude: mobile's two claudes wear `a` and `b` in the two columns before their
/// connectors, in the theme's yellow, bold; web's and infra's rows wear theirs too, dim. `3`
/// takes the cursor to infra and the light with it; a letter moves the cursor and the light
/// stays; `J` on to api, which has nothing under its header at this depth, lights none.
#[test]
fn every_sessions_rows_wear_their_letters_and_only_the_cursors_are_lit() {
    let cfg = config("[ui]\ndepth = \"agents\"\n");
    let mut app = app(&cfg);
    let theme = app.theme;
    let lit = (theme.yellow, true);
    let dim = (theme.dim, false);
    // The two columns before the row's connector: the letter, its colour and weight, and
    // whether the column after it is bold.
    let gutter = |buffer: &Buffer, lines: &[String], needle: &str| {
        let (y, _) = find(lines, needle);
        let x = lines[y]
            .chars()
            .position(|c| c == '\u{251c}' || c == '\u{2514}')
            .expect("a connector")
            - 2;
        let cell = &buffer[(x as u16, y as u16)];
        let after = &buffer[(x as u16 + 1, y as u16)];
        (
            cell.symbol().to_string(),
            (cell.fg, cell.modifier.contains(Modifier::BOLD)),
            after.modifier.contains(Modifier::BOLD),
        )
    };
    let letter = |buffer: &Buffer, lines: &[String], needle: &str| {
        let (key, style, _) = gutter(buffer, lines, needle);
        (key, style)
    };

    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    assert_eq!(letter(&buffer, &lines, "Tidy page"), ("a".to_string(), lit));
    // Only the letter is styled; the column after it is plain. A terminal writes a style
    // only where it changes, so the connector after a plain column has its own bold written
    // out again rather than carried over from the letter, which the acceptance suite,
    // reading escapes, would not see.
    let (_, _, bold_after) = gutter(&buffer, &lines, "Tidy page");
    assert!(!bold_after, "the column after the letter is plain");
    assert_eq!(letter(&buffer, &lines, "Review PR"), ("b".to_string(), lit));
    assert_eq!(
        letter(&buffer, &lines, "Invoice parser"),
        ("a".to_string(), dim),
        "web"
    );
    assert_eq!(
        letter(&buffer, &lines, "fix flaky"),
        ("c".to_string(), dim),
        "web"
    );
    assert_eq!(
        letter(&buffer, &lines, "Migrate auth"),
        ("a".to_string(), dim),
        "infra"
    );
    assert_eq!(
        letter(&buffer, &lines, "Error report"),
        ("b".to_string(), dim),
        "infra"
    );

    app.on_key(ch('3'));
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    assert_eq!(
        letter(&buffer, &lines, "Migrate auth"),
        ("a".to_string(), lit)
    );
    assert_eq!(
        letter(&buffer, &lines, "Error report"),
        ("b".to_string(), lit)
    );
    assert_eq!(
        letter(&buffer, &lines, "Tidy page"),
        ("a".to_string(), dim),
        "mobile"
    );

    app.on_key(ch('b'));
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    assert_eq!(
        letter(&buffer, &lines, "Migrate auth"),
        ("a".to_string(), lit),
        "after the jump the light stays on infra"
    );

    app.on_key(ch('J'));
    let buffer = frame(&mut app, W, H);
    let lines = lines_of(&buffer);
    for needle in [
        "Tidy page",
        "Invoice parser",
        "Migrate auth",
        "Error report",
    ] {
        let (_, style) = letter(&buffer, &lines, needle);
        assert_eq!(
            style, dim,
            "api has no rows at this depth, so none is lit: {needle}"
        );
    }
}

/// Every kind of row under a header takes a letter, in the order drawn: at the `all` depth
/// web's shell, its three agents and its node server wear `a` to `e`, and the sixth, the
/// parked worktree, wears none. The hint line names the letters after the numbers. With the
/// cursor in the next session its rows wear theirs, and web's keep theirs.
#[test]
fn every_kind_of_row_takes_a_letter_and_the_sixth_takes_none() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.on_key(ch('2'));
    let lines = drawn(&mut app, W, H);
    for row in [
        "a \u{251c} \u{25a2} zsh ",
        "b \u{251c} \u{23f5} claude ",
        "c \u{251c} \u{25cb} gemini ",
        "d \u{251c} \u{2713} codex ",
        "e \u{251c} \u{25a2} node ",
        "  \u{2514} \u{25cc} feat/pdf ",
    ] {
        find(&lines, row);
    }
    let hints = lines.last().expect("a hint line");
    assert!(hints.contains("1-9 session  \u{b7}  a-e rows"), "{hints}");

    app.on_key(ch('j'));
    let lines = drawn(&mut app, W, H);
    find(&lines, "a \u{251c} \u{25a2} zsh ");
    app.on_key(ch('J'));
    let lines = drawn(&mut app, W, H);
    // web's rows keep their letters, unlit now.
    find(&lines, "a \u{251c} \u{25a2} zsh        1:zsh ");
    for row in [
        "a \u{251c} \u{23f5} claude ",
        "b \u{251c} \u{25a2} zsh ",
        "c \u{251c} \u{25a2} nvim ",
        "d \u{251c} \u{25a2} tail ",
        "e \u{2514} \u{23f8} claude ",
    ] {
        find(&lines, row);
    }
}

// ------------------------------------------- the filter's directories

/// The popup over the sample world with `[ui] projects_in_filter` on, a scratch `$HOME`
/// holding the directories, and zoxide's history handed over as the run loop does. Two
/// zoxide directories and one project directory: three tags to draw.
fn projects_popup<'a>(cfg: &'a Config, home: &std::path::Path) -> App<'a> {
    let mut popup = app(cfg);
    popup.home = home.to_path_buf();
    popup.set_zoxide(vec!["~/oss/api".to_string(), "~/z/shop-pad".to_string()]);
    popup
}

fn projects_config(home: &std::path::Path, extra: &str) -> Config {
    for dir in ["oss/api", "z/shop-pad", "code/shop-notes"] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
    }
    config(&format!(
        "{extra}[ui]\nprojects_in_filter = true\n[sessions]\nproject_dirs = [\"{}\"]\n",
        home.join("code").display()
    ))
}

/// Under every session and agent, a dim `open a directory`, then the directories the
/// picker would list, each with where it came from at the end of its row: the tags line up
/// in a column after the longest path. The same under the search preset's query line.
#[test]
fn the_directories_are_drawn_under_the_tree_with_a_dim_header_and_a_tag_at_each_rows_end() {
    let home = tempfile::tempdir().unwrap();
    for preset in ["", "[keys]\npreset = \"search\"\n"] {
        let cfg = projects_config(home.path(), preset);
        let mut popup = projects_popup(&cfg, home.path());
        if preset.is_empty() {
            popup.on_key(ch('/'));
        }
        let theme = popup.theme;
        let buffer = frame(&mut popup, W, H);
        let screen = lines_of(&buffer);

        let (header_y, header_x) = find(&screen, "open a directory");
        assert_eq!(header_x, 2, "one column in from the border: {preset:?}");
        assert_eq!(
            style_of(&buffer, &screen, header_y, "open a directory"),
            (theme.dim, false),
            "dim, and not bold, like the quiet text of a group title"
        );
        // Below every session and agent row, and the directories follow it directly.
        let rows = &popup.rows.rows;
        let hit = |y: u16| {
            popup
                .hits
                .tree
                .rows
                .iter()
                .find(|(at, _)| *at == y)
                .map(|(_, i)| *i)
        };
        for (y, index) in &popup.hits.tree.rows {
            let is_dir = matches!(rows[*index], Row::Project(_));
            assert_eq!(
                (*y as usize) > header_y,
                is_dir,
                "row {index} at line {y}: only directories are under the header"
            );
        }
        let first = hit(header_y as u16 + 1).expect("a directory right under the header");
        assert!(matches!(rows[first], Row::Project(_)));
        assert_eq!(hit(header_y as u16), None, "the header maps to no row");

        // The paths, in the picker's order: zoxide's, then the project directory's.
        let order: Vec<usize> = ["~/oss/api", "~/z/shop-pad", "~/code/shop-notes"]
            .iter()
            .map(|path| find(&screen, path).0)
            .collect();
        assert_eq!(
            order,
            [header_y + 1, header_y + 2, header_y + 3],
            "{preset:?}"
        );
        // The paths start where the sessions' names do, and the tags share one column.
        let lines: Vec<&String> = order.iter().map(|y| &screen[*y]).collect();
        assert_eq!(column_of(lines[0], "~/oss/api"), 5, "under the names");
        let tags = [
            column_of(lines[0], "zoxide"),
            column_of(lines[1], "zoxide"),
            column_of(lines[2], "project dir"),
        ];
        assert_eq!(tags[0], tags[1], "{preset:?}");
        assert_eq!(tags[1], tags[2], "{preset:?}");
        let longest = "~/code/shop-notes";
        assert_eq!(
            tags[0],
            column_of(lines[2], longest) + longest.len() + 2,
            "the column starts two after the longest path"
        );
        for (line, tag) in lines.iter().zip(["zoxide", "zoxide", "project dir"]) {
            let y = screen.iter().position(|l| l == *line).unwrap();
            assert_eq!(
                style_of(&buffer, &screen, y, tag),
                (theme.dim, false),
                "the tag is dim"
            );
        }
    }
}

/// A directory nobody runs a session in is tagged for its source, and one somebody does
/// says `running`: the picker's word.
#[test]
fn a_directory_with_a_session_says_running_at_the_end_of_its_row() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    let mut snap = popup.snap.clone();
    snap.spaces[3].session_path = home.path().join("oss/api");
    popup.set_snapshot(snap);
    popup.on_key(ch('/'));

    let screen = drawn(&mut popup, W, H);

    let (y, _) = find(&screen, "~/oss/api");
    assert!(screen[y].contains("running"), "{}", screen[y]);
    assert!(!screen[y].contains("zoxide"), "{}", screen[y]);
}

/// The header is a line, not a row: a click on it selects nothing, and the highlight of the
/// row under the cursor never lights it, even when the cursor is on the first directory,
/// which is the row right under it.
#[test]
fn the_header_cannot_be_clicked_or_lit_and_a_directory_can() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    drawn(&mut popup, W, H);
    let first = popup.rows.first_project().expect("a section");
    let (header_y, _) = find(&drawn(&mut popup, W, H), "open a directory");
    let x = popup.hits.tree.area.x;
    let click = |popup: &mut App, y: usize| {
        popup.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y as u16,
            modifiers: KeyModifiers::NONE,
        })
    };

    let before = popup.sel;
    click(&mut popup, header_y);
    assert_eq!(popup.sel, before, "a click on the header moves nothing");

    click(&mut popup, header_y + 1);
    assert_eq!(
        popup.sel, first,
        "a click on the first directory selects it"
    );
    let theme = popup.theme;
    let buffer = frame(&mut popup, W, H);
    let bg = |y: usize| buffer[(x + 4, y as u16)].bg;
    assert_eq!(bg(header_y + 1), theme.selection_bg, "the directory is lit");
    assert_ne!(bg(header_y), theme.selection_bg, "the header is not");
    assert_ne!(bg(header_y + 2), theme.selection_bg, "nor the next row");
}

/// The last child of the last session still ends its tree line: the row after it is the
/// section's first directory, which is not a session, so it is the last of its session all
/// the same. (It is not the last row of the list, or a header, that ends it.)
#[test]
fn the_last_row_of_the_last_session_still_ends_its_tree_line() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));

    let screen = drawn(&mut popup, W, H);

    // notes' second window, the last pane of the tree: `└ ▢ zsh        2:zsh`.
    let (y, x) = find(&screen, "zsh        2:zsh ");
    let line: Vec<char> = screen[y].chars().collect();
    assert_eq!(line[x - 4], '\u{2514}', "{}", screen[y]);
}

/// With the cursor on a directory nothing has a pane to show, and the preview says so like
/// it does for a parked worktree; its title is the directory, so a long path cut at the
/// tree's edge can still be read whole.
#[test]
fn the_preview_of_a_directory_says_so_and_names_it() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    popup.sel = popup.rows.first_project().expect("a section");

    let screen = drawn(&mut popup, W, H);

    assert!(
        screen[0].contains("preview \u{b7} ~/oss/api"),
        "{}",
        screen[0]
    );
    assert!(
        screen.iter().any(|l| l.contains(PREVIEW_PLACEHOLDER)),
        "{}",
        screen.join("\n")
    );
}

/// The section scrolls with the cursor: on a short popup with the cursor on the first
/// directory, the header that introduces it is on screen with it, and the cursor's row too.
#[test]
fn the_header_scrolls_into_view_with_the_first_directory() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    popup.sel = popup.rows.first_project().expect("a section");
    popup.follow_sel = true;

    let screen = drawn(&mut popup, W, 12);

    let (header_y, _) = find(&screen, "open a directory");
    // (The preview's title names the directory too, so the row is looked for under the header.)
    assert!(
        screen[header_y + 1].contains("~/oss/api"),
        "the header is on screen with its first directory: {}",
        screen.join("\n")
    );
    assert!(
        header_y + 1 < 11,
        "inside the tree panel: {}",
        screen.join("\n")
    );
}

/// A popup too small for the tree, at every size the other frames are drawn at, still
/// draws with the section in it, and cuts a directory row like any other.
#[test]
fn the_section_draws_at_absurdly_small_sizes() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    for (width, height) in [(1, 1), (4, 3), (20, 6), (40, 10), (60, 24), (200, 60)] {
        let _ = drawn(&mut popup, width, height);
    }
    let screen = drawn(&mut popup, 60, 40);
    assert!(
        screen.iter().any(|l| l.contains("open a directory")),
        "{}",
        screen.join("\n")
    );
    assert!(
        screen.iter().all(|l| l.chars().count() <= 60),
        "nothing is drawn past the popup: {}",
        screen.join("\n")
    );
}

/// The characters the filter matched in a directory's path are highlighted as they are in a
/// session's name.
#[test]
fn the_query_highlights_the_characters_it_matched_in_a_directory() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    for c in "shop".chars() {
        popup.on_key(ch(c));
    }
    let theme = popup.theme;

    let buffer = frame(&mut popup, W, H);
    let screen = lines_of(&buffer);

    // (The preview's title may name the directory the cursor is on: look under the header.)
    let (header_y, _) = find(&screen, "open a directory");
    let y = (header_y + 1..screen.len())
        .find(|y| screen[*y].contains("~/z/shop-pad"))
        .expect("the directory's row");
    let x = column_of(&screen[y], "~/z/shop-pad");
    let shop = column_of(&screen[y], "shop");
    assert_eq!(shop, x + 4);
    let cell = &buffer[(shop as u16, y as u16)];
    assert_eq!(cell.fg, theme.yellow, "a matched character is yellow");
    assert!(cell.modifier.contains(Modifier::BOLD));
    assert_ne!(
        buffer[(x as u16 + 1, y as u16)].fg,
        theme.yellow,
        "`/` is not"
    );
}

/// A parked worktree's row draws `worktree, closed`, and `closed` finds it and lights those six
/// letters where they are drawn, past the branch column and the `worktree, ` before them.
#[test]
fn closed_lights_the_word_a_parked_worktree_draws() {
    let cfg = config("");
    let mut app = app(&cfg);
    app.filter = Some(Input::new("closed"));
    app.rebuild();
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let (y, x) = find(&screen, "worktree, closed");
    for at in 0..16 {
        let cell = &buffer[((x + at) as u16, y as u16)];
        let lit = cell.fg == theme.yellow && cell.modifier.contains(Modifier::BOLD);
        assert_eq!(lit, at >= 10, "letter {at}: {}", screen[y]);
    }
}

/// A directory's tag is drawn beside its path and is neither matched nor lit: a query that is
/// the tag word lights none of it.
#[test]
fn a_directory_tag_is_drawn_and_never_lit() {
    use crate::tui::picker::{Project, Tag};
    let cfg = config("");
    let mut app = app(&cfg);
    app.filter = Some(Input::new("zoxide"));
    app.projects = vec![Project {
        path: "~/z/shop-pad".to_string(),
        tag: Some(Tag::Zoxide),
    }];
    app.rebuild();
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let (header_y, _) = find(&screen, "open a directory");
    let y = (header_y + 1..screen.len())
        .find(|y| screen[*y].contains("~/z/shop-pad"))
        .expect("the directory's row");
    let x = column_of(&screen[y], "zoxide");
    for at in 0..6 {
        let cell = &buffer[((x + at) as u16, y as u16)];
        assert_ne!(cell.fg, theme.yellow, "letter {at}: {}", screen[y]);
    }
}

/// The help overlay, drawn by the popup itself, says the filter also lists directories when
/// the switch is on: from the config to the screen, in both presets (the search preset has
/// no key for help, so a `[keys]` entry gives it one).
#[test]
fn the_help_overlay_says_directories_show_below_when_the_switch_is_on() {
    let said = "filter (fzf syntax), by state too; directories to open show below";
    for (preset, help) in [
        ("", KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
        (
            "[keys]\npreset = \"search\"\nhelp = \"ctrl+g\"\n",
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        ),
    ] {
        for on in [false, true] {
            let cfg = config(&format!("{preset}[ui]\nprojects_in_filter = {on}\n"));
            let mut app = app(&cfg);
            app.on_key(help);
            assert!(matches!(app.overlay, Overlay::Help(_)), "{preset:?}");

            let screen = drawn(&mut app, W, H);

            assert_eq!(
                screen.iter().any(|line| line.contains(said)),
                on,
                "{preset:?}, switch {on}:\n{}",
                screen.join("\n")
            );
        }
    }
}

// -------------------------------- long paths keep their tag and their name

/// The worktrees of one repository, as deep as real ones get.
const WORKTREES: &str = "code/storefront/order-tracking-app/.tree/worktrees";

/// A project directory's child with a name of some length: 31 columns.
const LONG_CHILD: &str = "quarterly-report-generator-tool";

/// What each of the three directories the popup below lists ends in, and its tag.
const DIRECTORIES: [(&str, &str); 3] = [
    ("projects-filter", "running"),
    ("next-up-follow", "zoxide"),
    (LONG_CHILD, "project dir"),
];

/// A config whose project directory holds [`LONG_CHILD`].
fn long_paths_config(home: &std::path::Path) -> Config {
    let projects = home.join("clients/archived");
    std::fs::create_dir_all(projects.join(LONG_CHILD)).unwrap();
    config(&format!(
        "[ui]\nprojects_in_filter = true\n[sessions]\nproject_dirs = [\"{}\"]\n",
        projects.display()
    ))
}

/// The popup over the sample world listing three directories whose paths run to 50 and 68
/// columns, one for each tag: a worktree a session is running in, another that only zoxide
/// knows (the two differ in their last component alone), and a project directory's child.
fn long_paths_popup<'a>(cfg: &'a Config, home: &std::path::Path) -> App<'a> {
    let mut popup = app(cfg);
    popup.home = home.to_path_buf();
    let mut snap = popup.snap.clone();
    snap.spaces[3].session_path = home.join(format!("{WORKTREES}/projects-filter"));
    popup.set_snapshot(snap);
    popup.set_zoxide(vec![
        format!("~/{WORKTREES}/projects-filter"),
        format!("~/{WORKTREES}/next-up-follow"),
    ]);
    popup
}

/// What the tree panel drew on each directory row, its borders cut away: the indent, the
/// path, the padding and the tag.
fn directory_rows(popup: &mut App, width: u16, height: u16) -> Vec<String> {
    let screen = drawn(popup, width, height);
    let inner = popup.hits.tree.area;
    popup
        .hits
        .tree
        .rows
        .iter()
        .filter(|(_, index)| matches!(popup.rows.rows[*index], Row::Project(_)))
        .map(|(y, _)| {
            screen[*y as usize]
                .chars()
                .skip(inner.x as usize)
                .take(inner.width as usize)
                .collect()
        })
        .collect()
}

/// Deep worktree paths run past the panel, and a row cut at its edge would lose its tag and
/// its last component, so two worktrees would read the same. A path that does
/// not fit beside its tag loses its middle, and every row keeps its tag and its name. The
/// tags start in one column, the last one the widest still fits in.
#[test]
fn a_long_path_is_cut_from_the_middle_so_its_row_keeps_its_tag_and_its_last_component() {
    let home = tempfile::tempdir().unwrap();
    let cfg = long_paths_config(home.path());
    for (width, height) in [(150, 39), (120, 39), (100, 39)] {
        let mut popup = long_paths_popup(&cfg, home.path());
        popup.on_key(ch('/'));
        let rows = directory_rows(&mut popup, width, height);
        let inner = popup.hits.tree.area.width as usize;
        assert_eq!(rows.len(), 3, "{width} columns: {rows:?}");

        for (row, (leaf, tag)) in rows.iter().zip(DIRECTORIES) {
            let at = column_of(row, tag);
            let path: String = row.chars().take(at).collect();
            assert!(
                row.trim_end().ends_with(tag),
                "{width} columns, the tag is cut off: {row:?}"
            );
            assert!(
                path.trim_end().ends_with(leaf),
                "{width} columns, the name is cut off: {row:?}"
            );
            assert!(
                row.starts_with("    ~/") && row.contains('\u{2026}'),
                "{width} columns, the middle is what goes: {row:?}"
            );
            assert_eq!(
                at,
                inner - "project dir".len(),
                "{width} columns, the tags end at the panel's edge: {row:?}"
            );
        }
        assert_ne!(
            rows[0].split("running").next(),
            rows[1].split("zoxide").next(),
            "{width} columns: the two worktrees read differently"
        );
    }
}

/// A path that fits beside the widest tag is drawn whole, as it always was: the popup just
/// under the width that draws the preview has the tree to itself.
#[test]
fn a_path_that_fits_beside_its_tag_is_drawn_whole() {
    let home = tempfile::tempdir().unwrap();
    let cfg = long_paths_config(home.path());
    let mut popup = long_paths_popup(&cfg, home.path());
    popup.on_key(ch('/'));

    let rows = directory_rows(&mut popup, WIDE_COLUMNS - 1, 39);

    let longest = format!("~/{WORKTREES}/projects-filter");
    assert_eq!(longest.width(), 68);
    assert!(rows[0].contains(&longest), "{:?}", rows[0]);
    for (row, (_, tag)) in rows.iter().zip(DIRECTORIES) {
        assert!(!row.contains('\u{2026}'), "nothing is cut: {row:?}");
        assert_eq!(
            column_of(row, tag),
            4 + 68 + 2,
            "the tags start two after the longest path: {row:?}"
        );
    }
}

/// A row with no tag has nothing to keep room for, so it may use the whole width, and it is
/// cut from the middle too when it is longer than that, its last component kept whole.
#[test]
fn a_directory_with_no_tag_keeps_its_last_component_too() {
    let home = tempfile::tempdir().unwrap();
    let leaf = "a-worktree-with-a-rather-long-name-of-its-own";
    let deep = home.path().join(WORKTREES).join(leaf);
    std::fs::create_dir_all(&deep).unwrap();
    let cfg = long_paths_config(home.path());
    let mut popup = long_paths_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    for c in format!("~/{WORKTREES}/").chars() {
        popup.on_key(ch(c));
    }

    let rows = directory_rows(&mut popup, 150, 39);

    let inner = popup.hits.tree.area.width as usize;
    let bare = rows
        .iter()
        .find(|row| row.contains(leaf))
        .unwrap_or_else(|| panic!("the completion is listed: {rows:?}"));
    assert!(
        bare.trim_end().ends_with(leaf),
        "the name is not cut off: {bare:?}"
    );
    assert!(bare.contains('\u{2026}'), "the middle went: {bare:?}");
    assert!(
        bare.trim_end().chars().count() > inner - 13,
        "it is not held to the room a tag would leave: {bare:?}"
    );
}

/// The characters a query matched are lit in the part of a cut path that is drawn, at the
/// column they moved to, and the ellipsis is not one of them.
#[test]
fn the_matched_characters_of_a_cut_path_stay_lit_after_the_cut() {
    let home = tempfile::tempdir().unwrap();
    let cfg = long_paths_config(home.path());
    let mut popup = long_paths_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    for c in "projects-filter".chars() {
        popup.on_key(ch(c));
    }
    let theme = popup.theme;

    let buffer = frame(&mut popup, W, H);
    let screen = lines_of(&buffer);

    let (y, _) = find(&screen, "running");
    let x = column_of(&screen[y], "projects-filter");
    let cell = |x: usize| &buffer[(x as u16, y as u16)];
    for n in 0.."projects-filter".len() {
        assert_eq!(
            cell(x + n).fg,
            theme.yellow,
            "character {n} of the name is lit: {}",
            screen[y]
        );
        assert!(cell(x + n).modifier.contains(Modifier::BOLD));
    }
    let ellipsis = column_of(&screen[y], "\u{2026}");
    assert_eq!(cell(ellipsis).fg, theme.dim, "the ellipsis is quiet");
}

/// `shorten`'s answer as the row draws it.
fn shown(path: &str, columns: usize) -> String {
    match shorten(path, columns) {
        Some(cut) => format!("{}\u{2026}{}", cut.head, cut.tail),
        None => path.to_string(),
    }
}

/// The middle goes and the last component stays: the head is whole components, as many as
/// fit beside the tail, and the tail is the last component with the slash before it. The 30
/// columns are an example, `~/code/\u{2026}/projects-filter`.
#[test]
fn a_path_wider_than_its_room_loses_its_middle_and_keeps_its_last_component() {
    let path = format!("~/{WORKTREES}/projects-filter");
    assert_eq!(path.width(), 68);
    for (columns, expected) in [
        (68, path.as_str()),
        (
            67,
            "~/code/storefront/order-tracking-app/.tree/\u{2026}/projects-filter",
        ),
        (
            59,
            "~/code/storefront/order-tracking-app/\u{2026}/projects-filter",
        ),
        (50, "~/code/storefront/\u{2026}/projects-filter"),
        (30, "~/code/\u{2026}/projects-filter"),
        (20, "~/\u{2026}/projects-filter"),
        // No whole component fits before the ellipsis: it comes first.
        (17, "\u{2026}/projects-filter"),
        // The last component itself is too wide: its end is what is shown.
        (16, "\u{2026}projects-filter"),
        (12, "\u{2026}ects-filter"),
        (1, "\u{2026}"),
    ] {
        assert_eq!(shown(&path, columns), expected, "in {columns} columns");
    }
    // A path with no slash to cut at is cut from its start, and one that fits is left alone.
    assert_eq!(shown("quarterly-report", 9), "\u{2026}y-report");
    assert_eq!(shown("~/oss/api", 9), "~/oss/api");
}

/// However the path is built, wide characters and a lone component included, what is drawn
/// never takes more columns than it was given, and a path that fits is not touched.
#[test]
fn a_shortened_path_never_takes_more_columns_than_it_was_given() {
    let long = format!("~/{WORKTREES}/projects-filter");
    for path in [
        long.as_str(),
        "~/a/b",
        "~",
        "/",
        "/usr/local/very-long-name-indeed",
        "~/\u{65e5}\u{672c}\u{8a9e}/\u{30c7}\u{30a3}\u{30ec}\u{30af}\u{30c8}\u{30ea}/leaf",
        "~/proj/\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}",
        "no-slash-at-all-in-this-one",
    ] {
        for columns in 1..=path.width() + 2 {
            let drawn = shown(path, columns);
            assert!(
                drawn.width() <= columns,
                "{path:?} in {columns}: {drawn:?} is {} wide",
                drawn.width()
            );
            if path.width() <= columns {
                assert_eq!(drawn, path, "fits in {columns}");
            }
        }
    }
}

/// What the row lights is counted in the path's own characters: the tail says where it
/// starts, so a position in it is lit at the column it moved to.
#[test]
fn a_shortened_path_says_where_its_tail_starts() {
    let path = format!("~/{WORKTREES}/projects-filter");
    let cut = shorten(&path, 30).expect("cut");
    assert_eq!(cut.head, "~/code/");
    assert_eq!(cut.tail, "/projects-filter");
    assert_eq!(
        cut.tail_from,
        path.chars().count() - "/projects-filter".chars().count()
    );
    let end = shorten(&path, 12).expect("cut");
    assert_eq!(end.head, "");
    assert_eq!(
        end.tail_from,
        path.chars().count() - end.tail.chars().count()
    );
}

// ---------- the section under a short tree, pinned to the foot of a tall one

/// The tree panel's lines after a frame, top to bottom, cut to the panel's own columns. The
/// search preset's query line is the first of them.
fn panel_lines(popup: &App, screen: &[String]) -> Vec<String> {
    let inner = popup.hits.tree.area;
    (inner.y..inner.bottom())
        .map(|y| {
            screen[y as usize]
                .chars()
                .skip(inner.x as usize)
                .take(inner.width as usize)
                .collect()
        })
        .collect()
}

/// Which of the panel's lines the cursor's highlight is on, counted from the top of the panel.
fn lit_line(popup: &App, buffer: &Buffer) -> Option<usize> {
    let inner = popup.hits.tree.area;
    (inner.y..inner.bottom()).position(|y| buffer[(inner.x, y)].bg == popup.theme.selection_bg)
}

/// A frame of the popup as it stands: the panel's lines, and the line the cursor is on.
fn panel_now(popup: &mut App, width: u16, height: u16) -> (Vec<String>, Option<usize>) {
    let buffer = frame(popup, width, height);
    let screen = lines_of(&buffer);
    (panel_lines(popup, &screen), lit_line(popup, &buffer))
}

/// What the three directories `projects_popup` lists say, in order.
const THREE: [(&str, &str); 3] = [
    ("~/oss/api", "zoxide"),
    ("~/z/shop-pad", "zoxide"),
    ("~/code/shop-notes", "project dir"),
];

/// Right after `/` is pressed the recommended list is in view at the bottom. A tree taller
/// than the panel must not push the section off it, to be scrolled to, so it is pinned: with a tree that fills the panel, its header and its directories are the
/// panel's last lines, with the sessions and agents above them, before anything is typed. (A
/// tree that leaves room has the section right under it: the short-tree tests below.)
#[test]
fn right_after_slash_the_section_is_at_the_bottom_of_a_panel_the_tree_overflows() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));

    let (lines, lit) = panel_now(&mut popup, W, 20);

    let n = lines.len();
    assert_eq!(n, 17);
    assert!(
        popup.rows.first_project().unwrap() > n,
        "the tree alone is taller than the panel"
    );
    assert!(lines[n - 4].contains("open a directory"), "{lines:#?}");
    for (line, (path, tag)) in lines[n - 3..].iter().zip(THREE) {
        assert!(
            line.contains(path) && line.trim_end().ends_with(tag),
            "{line:?}"
        );
    }
    assert!(lines[0].contains("mobile"), "the tree above it: {lines:#?}");
    assert!(
        lit.is_some_and(|at| at < n - 4),
        "the cursor is on a row of the tree: {lit:?}"
    );
}

/// The config of [`projects_config`] at the app's own default depth, `agents`, where a
/// session with no agent in it is one line: a tree's lines are then easy to count.
fn agents_config(home: &std::path::Path, extra: &str) -> Config {
    let mut cfg = projects_config(home, extra);
    cfg.ui.depth = Depth::Agents;
    cfg
}

/// The sample world cut down to two sessions with nothing running in them, api and notes
/// (the popup was opened from api's shell), listing the same three directories.
fn two_sessions_popup<'a>(cfg: &'a Config, home: &std::path::Path) -> App<'a> {
    let mut popup = projects_popup(cfg, home);
    let mut snap = popup.snap.clone();
    snap.spaces
        .retain(|s| s.session_id == "$4" || s.session_id == "$5");
    snap.panes
        .retain(|p| p.session_id == "$4" || p.session_id == "$5");
    snap.agents.clear();
    for pane in &mut snap.panes {
        pane.agent = None;
    }
    snap.own_pane = Some("%411".into());
    snap.current_session = Some("$4".into());
    snap.previous_session = Some("$5".into());
    popup.set_snapshot(snap);
    popup
}

/// What the panel's line `at` says in its tree columns.
fn says(lines: &[String], at: usize) -> &str {
    lines.get(at).map_or("", |line| line.trim())
}

/// The filter's directories sit just below the sessions, not at the very bottom. Two sessions and three
/// directories on a tall panel: the header is on the second line after the last session's
/// line, one blank line between them, the directories under it in order, and the empty space
/// of the panel is under the section, not above it. In both presets (the search preset's
/// query is the panel's first line).
#[test]
fn a_short_tree_has_the_section_just_below_it_and_the_empty_space_under_the_section() {
    let home = tempfile::tempdir().unwrap();
    for (preset, top) in [("", 0), ("[keys]\npreset = \"search\"\n", 1)] {
        let cfg = agents_config(home.path(), preset);
        let mut popup = two_sessions_popup(&cfg, home.path());
        if top == 0 {
            popup.on_key(ch('/'));
        }

        let (lines, _) = panel_now(&mut popup, W, H);

        let n = lines.len();
        assert!(
            n >= 30,
            "a tall panel, or nowhere is far from the foot: {n}"
        );
        assert!(says(&lines, top).contains("api"), "{preset:?}: {lines:#?}");
        assert!(
            says(&lines, top + 1).contains("notes"),
            "{preset:?}: {lines:#?}"
        );
        assert_eq!(
            says(&lines, top + 2),
            "",
            "{preset:?}: one blank line: {lines:#?}"
        );
        assert!(
            says(&lines, top + 3).contains("open a directory"),
            "{preset:?}: the second line after the last session: {lines:#?}"
        );
        for (k, (path, tag)) in THREE.into_iter().enumerate() {
            let line = says(&lines, top + 4 + k);
            assert!(
                line.contains(path) && line.ends_with(tag),
                "{preset:?}: directory {k}: {lines:#?}"
            );
        }
        assert!(
            lines[top + 7..].iter().all(|line| line.trim().is_empty()),
            "{preset:?}: the empty space is under the section: {lines:#?}"
        );
    }
}

/// The blank line is a line and the sessions and the directories are the rows: with the
/// section right under the tree, the hit map has an entry for each session and each directory
/// and none for the header or the blank line, and the cursor lights the directory it is on.
#[test]
fn under_a_short_tree_the_hit_map_has_the_rows_and_neither_the_header_nor_the_blank_line() {
    let home = tempfile::tempdir().unwrap();
    let cfg = agents_config(home.path(), "");
    let mut popup = two_sessions_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().expect("a section");
    assert_eq!(first, 2, "two sessions above the directories");

    let screen = drawn(&mut popup, W, H);

    let (header, _) = find(&screen, "open a directory");
    let mapped: Vec<(u16, usize)> = popup.hits.tree.rows.clone();
    let panel_top = popup.hits.tree.area.y;
    assert_eq!(
        mapped,
        [
            (panel_top, 0),
            (panel_top + 1, 1),
            (header as u16 + 1, first),
            (header as u16 + 2, first + 1),
            (header as u16 + 3, first + 2),
        ]
    );
    assert_eq!(popup.hits.tree.header, Some(header as u16));
    assert_eq!(
        header as u16,
        panel_top + 3,
        "the blank line (line {}) is neither",
        panel_top + 2
    );
}

/// A tree that almost fills the panel: the sample's is 24 lines (five sessions, eighteen
/// panes and a parked worktree) and the section 4. From a panel with room for both and a blank
/// line between them the section goes as far down as the tree makes it, one blank line under
/// the tree's last line, and no further than the foot, where it stays while the tree is
/// scrolled. Between the two it goes to the foot as well and drops the blank line, but it
/// never covers a line of the tree: one panel line shorter than that and the tree scrolls
/// above it instead. At each size the header is where that puts it, the tree's lines run
/// unbroken down to it (its last row right above the blank line, or right above the header),
/// and the three directories are under it whole.
#[test]
fn a_tree_that_almost_fills_the_panel_pushes_the_section_down_and_never_over_a_tree_line() {
    let home = tempfile::tempdir().unwrap();
    for (preset, top) in [("", 0), ("[keys]\npreset = \"search\"\n", 1)] {
        let cfg = projects_config(home.path(), preset);
        for height in 29..=37 {
            let mut popup = projects_popup(&cfg, home.path());
            if top == 0 {
                popup.on_key(ch('/'));
            }
            let tree = popup.rows.first_project().expect("a section");
            assert_eq!(tree, 24, "the sample's tree is 24 lines");

            let (lines, _) = panel_now(&mut popup, W, height);

            let n = lines.len();
            let foot = n - 4;
            let room = foot - top;
            let expected = if tree < room { top + tree + 1 } else { foot };
            let header = lines
                .iter()
                .position(|line| line.contains("open a directory"))
                .unwrap_or_else(|| panic!("{preset:?} {n}: no header: {lines:#?}"));
            assert_eq!(
                header, expected,
                "{preset:?} panel of {n} lines, a tree of {tree} with {room} above the foot: {lines:#?}"
            );
            // The tree runs unbroken to the section, with its blank line only when it fits.
            let blank = usize::from(tree < room);
            assert_eq!(
                says(&lines, header - 1).is_empty(),
                blank == 1,
                "{lines:#?}"
            );
            assert!(
                lines[top..header - blank]
                    .iter()
                    .all(|line| !line.trim().is_empty()),
                "{preset:?} {n}: a tree line went missing: {lines:#?}"
            );
            if tree <= room {
                assert!(
                    says(&lines, header - blank - 1).contains("2:zsh"),
                    "{preset:?} {n}: the tree's last row is right above the gap: {lines:#?}"
                );
            }
            for (k, (path, tag)) in THREE.into_iter().enumerate() {
                let line = says(&lines, header + 1 + k);
                assert!(
                    line.contains(path) && line.ends_with(tag),
                    "{preset:?} {n}: directory {k}: {lines:#?}"
                );
            }
        }
    }
}

/// With no session or agent to show, the section is what the panel shows, from its top (under
/// the search preset's query line): there is no tree line to leave a blank line under, and
/// the section is not left at the foot of an empty panel.
#[test]
fn with_no_session_to_show_the_section_starts_at_the_top_of_the_panel() {
    let home = tempfile::tempdir().unwrap();
    for (preset, top) in [("", 0), ("[keys]\npreset = \"search\"\n", 1)] {
        let cfg = projects_config(home.path(), preset);
        let mut popup = projects_popup(&cfg, home.path());
        if top == 0 {
            popup.on_key(ch('/'));
        }
        // `shop` is in no session, agent or pane of the sample world, only in two directories.
        for c in "shop".chars() {
            popup.on_key(ch(c));
        }
        assert_eq!(
            popup.rows.first_project(),
            Some(0),
            "{preset:?}: no tree row"
        );

        let (lines, _) = panel_now(&mut popup, W, H);

        assert!(
            says(&lines, top).contains("open a directory"),
            "{preset:?}: {lines:#?}"
        );
        assert!(
            says(&lines, top + 1).contains("~/z/shop-pad"),
            "{preset:?}: {lines:#?}"
        );
        assert!(
            says(&lines, top + 2).contains("shop-notes"),
            "{preset:?}: {lines:#?}"
        );
        assert!(
            lines[top + 3..].iter().all(|line| line.trim().is_empty()),
            "{preset:?}: {lines:#?}"
        );
    }
}

/// A sidebar template can give a row more than one line, and the tree is worked out in lines:
/// two sessions with two-line headers are four lines, and the section starts on the sixth.
#[test]
fn a_short_tree_of_rows_of_two_lines_is_counted_in_lines() {
    let home = tempfile::tempdir().unwrap();
    let cfg = agents_config(
        home.path(),
        "[ui.sidebar.spaces]\nrows = [[\"workspace\"], [\"workspace\"]]\n",
    );
    let mut popup = two_sessions_popup(&cfg, home.path());
    popup.on_key(ch('/'));

    let (lines, _) = panel_now(&mut popup, W, H);

    assert!(
        says(&lines, 0).contains("api") && says(&lines, 1).contains("api"),
        "{lines:#?}"
    );
    assert!(
        says(&lines, 2).contains("notes") && says(&lines, 3).contains("notes"),
        "{lines:#?}"
    );
    assert_eq!(says(&lines, 4), "", "the blank line: {lines:#?}");
    assert!(says(&lines, 5).contains("open a directory"), "{lines:#?}");
    assert!(says(&lines, 6).contains("~/oss/api"), "{lines:#?}");
}

/// A click finds a directory where the section is drawn, wherever that is: under a short
/// tree, under a tree that almost fills the panel and at the foot of a tall one. The click
/// selects it, the second click of a double-click opens it, and a click on the header, or on
/// the blank line above it where there is one, selects nothing.
#[test]
fn a_click_on_a_directory_finds_it_under_a_short_tree_a_full_one_and_a_tall_one() {
    let home = tempfile::tempdir().unwrap();
    let short = agents_config(home.path(), "");
    let full = projects_config(home.path(), "");
    // (what it is, its config, two sessions only, the popup's height, and the header's line
    // in the panel: under the two sessions and a blank line; under the tree's 24 lines and a
    // blank line, with one line to spare below the section's four; and the panel's last four)
    let worlds: [(&str, &Config, bool, u16, usize); 3] = [
        ("a short tree", &short, true, H, 3),
        ("a tree that almost fills the panel", &full, false, 33, 25),
        ("a tall tree", &full, false, 20, 13),
    ];
    for (what, cfg, two, height, at) in worlds {
        let mut popup = if two {
            two_sessions_popup(cfg, home.path())
        } else {
            projects_popup(cfg, home.path())
        };
        popup.on_key(ch('/'));
        let first = popup.rows.first_project().expect("a section");
        let screen = drawn(&mut popup, W, height);
        let (header, _) = find(&screen, "open a directory");
        assert_eq!(
            header - popup.hits.tree.area.y as usize,
            at,
            "{what}: the section is where that puts it:\n{}",
            screen.join("\n")
        );
        let x = popup.hits.tree.area.x + 5;
        let click = |popup: &mut App, y: usize| {
            popup.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            })
        };

        let before = popup.sel;
        assert_eq!(click(&mut popup, header), None, "{what}: the header");
        assert_eq!(
            popup.sel, before,
            "{what}: a click on the header moves nothing"
        );
        if !popup
            .hits
            .tree
            .rows
            .iter()
            .any(|(y, _)| *y as usize == header - 1)
        {
            assert!(
                screen[header - 1].chars().skip(1).take(5).all(|c| c == ' '),
                "{what}: a blank line above the header: {}",
                screen[header - 1]
            );
            assert_eq!(
                click(&mut popup, header - 1),
                None,
                "{what}: the blank line"
            );
            assert_eq!(
                popup.sel, before,
                "{what}: a click on the blank line moves nothing"
            );
        }
        for (k, (path, _)) in THREE.into_iter().enumerate() {
            assert!(
                screen[header + 1 + k].contains(path),
                "{what}: directory {k} is where the frame drew it: {}",
                screen[header + 1 + k]
            );
            assert_eq!(click(&mut popup, header + 1 + k), None, "{what}");
            assert_eq!(popup.sel, first + k, "{what}: a click on directory {k}");
        }
        assert_eq!(
            click(&mut popup, header + 3),
            Some(crate::tui::actions::Action::OpenProject {
                path: THREE[2].0.to_string()
            }),
            "{what}: the second click of a double-click opens the directory"
        );
    }
}

/// The search preset has its query on the panel's first line, and with a tree that fills the
/// panel the section is pinned to the foot of whatever is left.
#[test]
fn the_search_presets_section_is_pinned_below_its_query_line() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "[keys]\npreset = \"search\"\n");
    let mut popup = projects_popup(&cfg, home.path());

    let (lines, _) = panel_now(&mut popup, W, 20);

    let n = lines.len();
    assert!(lines[0].contains('>'), "the query line: {lines:#?}");
    assert!(lines[n - 4].contains("open a directory"), "{lines:#?}");
    assert!(lines[n - 3].contains("~/oss/api"), "{lines:#?}");
    assert!(lines[n - 1].contains("shop-notes"), "{lines:#?}");
}

/// Both parts stay right as the cursor walks: down through a tree that scrolls above the
/// section, into the section's first directory and through it, past its end and back to the
/// top, then up again. The section never moves, the cursor's line is always on screen, and the
/// tree only scrolls when the cursor is in it.
#[test]
fn the_cursor_walks_the_tree_and_the_section_with_both_parts_in_place() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().unwrap();
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);

    let (start, _) = panel_now(&mut popup, W, 20);
    let n = start.len();
    let section: Vec<String> = start[n - 4..].to_vec();
    let mut tree = start[..n - 4].to_vec();

    // Down through the tree, to its last row.
    while popup.sel + 1 < first {
        popup.on_key(down());
        let (lines, lit) = panel_now(&mut popup, W, 20);
        assert_eq!(&lines[n - 4..], &section[..], "row {}", popup.sel);
        assert!(
            lit.is_some_and(|at| at < n - 4),
            "row {} is on screen, above the section: {lit:?}",
            popup.sel
        );
        tree = lines[..n - 4].to_vec();
    }
    assert_eq!(popup.sel, first - 1);
    assert!(
        tree[n - 5].contains("zsh") && !tree[n - 5].trim().is_empty(),
        "the tree's last row is the last line above the section: {tree:#?}"
    );

    // Into the section: its first directory is lit, and the tree is exactly as it was.
    popup.on_key(down());
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(lit, Some(n - 3), "{lines:#?}");
    assert_eq!(&lines[..n - 4], &tree[..], "the tree did not move");
    assert_eq!(&lines[n - 4..], &section[..]);

    // Through the section, and out of its end: the cursor wraps to the top of the tree, which
    // scrolls back for it.
    popup.on_key(down());
    popup.on_key(down());
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(lit, Some(n - 1), "the last directory");
    assert_eq!(&lines[..n - 4], &tree[..]);
    popup.on_key(down());
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(popup.sel, 0);
    assert_eq!(lit, Some(0), "the first row is lit at the top: {lines:#?}");
    assert_eq!(&lines[n - 4..], &section[..]);
    let top = lines[..n - 4].to_vec();

    // And back up, out of the top and into the last directory, the tree staying at its top.
    popup.on_key(up());
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(lit, Some(n - 1), "{lines:#?}");
    assert_eq!(&lines[..n - 4], &top[..], "the tree did not scroll for it");
    assert_eq!(&lines[n - 4..], &section[..]);
    popup.on_key(up());
    popup.on_key(up());
    popup.on_key(up());
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(popup.sel, first - 1);
    assert!(lit.is_some_and(|at| at < n - 4), "{lit:?}");
    assert_eq!(&lines[n - 4..], &section[..]);
}

/// Opening the filter with the cursor on the last row of a tall tree: the section takes lines
/// off the panel's foot, and the cursor's row must not be one of the lines it takes.
#[test]
fn opening_the_filter_keeps_the_cursor_on_screen_when_the_section_takes_the_bottom() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.sel = popup.rows.rows.len() - 1;
    popup.follow_sel = true;
    let (lines, lit) = panel_now(&mut popup, W, 20);
    assert_eq!(lit, Some(lines.len() - 1), "the last row, at the foot");

    popup.on_key(ch('/'));
    let (lines, lit) = panel_now(&mut popup, W, 20);

    let n = lines.len();
    assert!(lines[n - 4].contains("open a directory"), "{lines:#?}");
    assert!(
        lit.is_some_and(|at| at < n - 4),
        "the cursor's row is above the section: {lit:?}\n{lines:#?}"
    );
}

/// A wheel over the panel scrolls the tree above the section and leaves the section where it
/// is.
#[test]
fn the_wheel_scrolls_the_tree_and_never_the_section() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    let (before, _) = panel_now(&mut popup, W, 20);
    let n = before.len();
    let inner = popup.hits.tree.area;

    for _ in 0..3 {
        popup.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: inner.x,
            row: inner.y,
            modifiers: KeyModifiers::NONE,
        });
    }
    let (after, _) = panel_now(&mut popup, W, 20);

    assert_ne!(after[..n - 4], before[..n - 4], "the tree scrolled");
    assert_eq!(after[n - 4..], before[n - 4..], "the section did not");
}

/// The section a panel too short for all of it gets: never more than half of the panel's
/// lines, as many of them as that allows up to the header and the eight directories, and its
/// own scroll when the cursor goes where the lines do not show. It starts one blank line
/// under the tree's 24 lines when they fit above the foot, and at the foot when they do not.
#[test]
fn a_short_panel_gives_the_section_at_most_half_of_its_lines() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let history: Vec<String> = (0..12).map(|n| format!("~/z/p{n:02}")).collect();
    for height in 4..=45 {
        let mut popup = projects_popup(&cfg, home.path());
        popup.set_zoxide(history.clone());
        popup.on_key(ch('/'));
        assert_eq!(popup.projects.len(), 8);

        let (lines, _) = panel_now(&mut popup, W, height);

        let listed = lines.len();
        let directories = lines
            .iter()
            .filter(|line| line.starts_with("    ~/z/p"))
            .count();
        let header = lines.iter().any(|line| line.contains("open a directory"));
        let section = directories + usize::from(header);
        assert!(
            section <= listed / 2,
            "height {height}: {section} of {listed} lines: {lines:#?}"
        );
        assert_eq!(
            section,
            (listed / 2).min(9),
            "height {height}: as much as half allows: {lines:#?}"
        );
        assert_eq!(header, listed / 2 >= 2, "height {height}: {lines:#?}");
        if section > 0 {
            let tree = popup.rows.first_project().unwrap();
            let start = (tree + 1).min(listed - section);
            assert!(
                lines[start..start + section]
                    .iter()
                    .filter(|line| !line.contains("open a directory"))
                    .all(|line| line.starts_with("    ~/z/p")),
                "height {height}: the section starts on line {start}: {lines:#?}"
            );
            assert!(
                lines[start + section..]
                    .iter()
                    .all(|line| line.trim().is_empty()),
                "height {height}: nothing is under the section: {lines:#?}"
            );
        }
    }
}

/// A section that shows only some of its directories scrolls to the cursor, like the tree,
/// and the tree does not move while it does: on a panel of eight lines the section is a
/// header and three of the eight.
#[test]
fn a_section_cut_short_by_the_panel_scrolls_to_the_cursor_and_the_tree_stays() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let history: Vec<String> = (0..12).map(|n| format!("~/z/p{n:02}")).collect();
    let mut popup = projects_popup(&cfg, home.path());
    popup.set_zoxide(history);
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().unwrap();
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    let height = 11;

    let (lines, _) = panel_now(&mut popup, W, height);
    let n = lines.len();
    assert_eq!(n, 8);
    let tree = lines[..n - 4].to_vec();
    assert!(lines[n - 4].contains("open a directory"));
    assert!(lines[n - 3].contains("~/z/p00") && lines[n - 1].contains("~/z/p02"));

    popup.sel = first - 1;
    popup.follow_sel = true;
    let (lines, _) = panel_now(&mut popup, W, height);
    let tree_at_its_end = lines[..n - 4].to_vec();
    assert_ne!(tree_at_its_end, tree, "the cursor scrolled the tree");

    // Down through the eight directories: the cursor's line is on screen at every step, and
    // it is the window's last line once it has passed the third.
    for step in 0..8 {
        popup.on_key(down());
        let (lines, lit) = panel_now(&mut popup, W, height);
        assert_eq!(&lines[..n - 4], &tree_at_its_end[..], "step {step}");
        let want = format!("~/z/p{step:02}");
        let at = lit.unwrap_or_else(|| panic!("step {step}: nothing lit: {lines:#?}"));
        assert!(lines[at].contains(&want), "step {step}: {lines:#?}");
        assert!(at >= n - 3, "step {step}: inside the section: {lines:#?}");
        assert_eq!(at, (n - 3 + step).min(n - 1), "step {step}");
    }
    // Back up: the window stays until the cursor passes its top, then follows it.
    for (step, expected) in [(0, n - 2), (1, n - 3), (2, n - 3)] {
        popup.on_key(up());
        let (lines, lit) = panel_now(&mut popup, W, height);
        let cursor = 6 - step;
        assert!(
            lines[lit.unwrap()].contains(&format!("~/z/p{cursor:02}")),
            "{lines:#?}"
        );
        assert_eq!(lit, Some(expected), "up {step}: {lines:#?}");
    }
}

/// A new list starts at its top: after the cursor has scrolled a cut-short section to its
/// last directory, a keystroke that lists again shows the section from its first.
#[test]
fn a_new_list_shows_the_section_from_its_first_directory() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let history: Vec<String> = (0..12).map(|n| format!("~/z/p{n:02}")).collect();
    let mut popup = projects_popup(&cfg, home.path());
    popup.set_zoxide(history);
    popup.on_key(ch('/'));
    popup.sel = popup.rows.rows.len() - 1;
    popup.follow_sel = true;
    let (lines, _) = panel_now(&mut popup, W, 11);
    let n = lines.len();
    assert!(
        lines[n - 1].contains("~/z/p07"),
        "scrolled to the last: {lines:#?}"
    );

    // `z` matches the shells of the tree, so the cursor leaves the section, and it is in every
    // one of the same eight paths.
    popup.on_key(ch('z'));
    let (lines, lit) = panel_now(&mut popup, W, 11);

    assert!(
        lit.is_some_and(|at| at < n - 4),
        "the cursor is in the tree: {lit:?} {lines:#?}"
    );
    assert!(lines[n - 3].contains("~/z/p00"), "from its top: {lines:#?}");
    assert!(lines[n - 1].contains("~/z/p02"), "{lines:#?}");
}

/// The mouse finds a pinned directory where the frame drew it: a click selects it, and the
/// second click of a double-click opens it, with the tree scrolled to wherever it is.
#[test]
fn a_click_selects_a_pinned_directory_and_a_double_click_opens_it() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    popup.sel = popup.rows.rows.len() - 4;
    popup.follow_sel = true;
    let buffer = frame(&mut popup, W, 20);
    let screen = lines_of(&buffer);
    let (y, _) = find(&screen, "~/z/shop-pad");
    let x = popup.hits.tree.area.x + 5;
    let click = |popup: &mut App| {
        popup.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y as u16,
            modifiers: KeyModifiers::NONE,
        })
    };

    assert_eq!(click(&mut popup), None);
    let first = popup.rows.first_project().unwrap();
    assert_eq!(popup.sel, first + 1, "the directory the click landed on");

    assert_eq!(
        click(&mut popup),
        Some(crate::tui::actions::Action::OpenProject {
            path: "~/z/shop-pad".to_string()
        })
    );
}

// ------------------------------------------ the header's quirks

/// The header is a line, not a row, and a right-click on it opens nothing, and does not fall
/// through to the menu of whatever row the cursor is on. A row still has its menu, on the
/// mouse's own row or under the pointer, and a directory has none.
#[test]
fn a_right_click_on_the_header_does_nothing() {
    let home = tempfile::tempdir().unwrap();
    for preset in ["", "[keys]\npreset = \"search\"\n"] {
        let cfg = projects_config(home.path(), preset);
        let mut popup = projects_popup(&cfg, home.path());
        if preset.is_empty() {
            popup.on_key(ch('/'));
        }
        let screen = drawn(&mut popup, W, 20);
        let (header_y, _) = find(&screen, "open a directory");
        let x = popup.hits.tree.area.x + 3;
        let right_click = |popup: &mut App, y: usize| {
            popup.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                column: x,
                row: y as u16,
                modifiers: KeyModifiers::NONE,
            })
        };
        let cursor = popup.sel;
        assert!(
            !matches!(popup.rows.rows[cursor], Row::Project(_)),
            "the cursor is on a session or a pane, whose menu it would have opened: {preset:?}"
        );

        assert_eq!(right_click(&mut popup, header_y), None, "{preset:?}");

        assert!(
            !popup.overlay.is_open(),
            "{preset:?}: the header has no menu: {:?}",
            popup.overlay
        );
        assert_eq!(popup.sel, cursor, "{preset:?}: and the cursor stayed");

        // The rows around it are as they were: a session row's menu opens on it, and a
        // directory's does not open at all.
        let session = popup
            .hits
            .tree
            .rows
            .iter()
            .find(|(_, index)| matches!(popup.rows.rows[*index], Row::Session(_)))
            .map(|(y, _)| *y as usize)
            .expect("a session row on screen");
        right_click(&mut popup, session);
        assert!(
            matches!(popup.overlay, Overlay::Menu(_)),
            "{preset:?}: {:?}",
            popup.overlay
        );
        popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        right_click(&mut popup, header_y + 1);
        assert!(
            !popup.overlay.is_open(),
            "{preset:?}: a directory has no menu"
        );
    }
}

/// Whether the header has a line of its own to be right-clicked on: exactly when it is drawn.
#[test]
fn the_hit_map_knows_the_header_only_while_it_is_drawn() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(home.path(), "");
    let mut popup = projects_popup(&cfg, home.path());
    drawn(&mut popup, W, 20);
    assert_eq!(popup.hits.tree.header, None, "no filter, no section");

    popup.on_key(ch('/'));
    let screen = drawn(&mut popup, W, 20);
    let (header_y, _) = find(&screen, "open a directory");
    assert_eq!(popup.hits.tree.header, Some(header_y as u16));

    // On a panel with room for one line the section is a directory alone.
    let screen = drawn(&mut popup, W, 5);
    assert!(!screen.iter().any(|line| line.contains("open a directory")));
    assert_eq!(popup.hits.tree.header, None);
}

/// A header with nothing under it says nothing, and a panel that cuts the section can leave
/// it last: at every size from a one-line panel to a tall one, in both layouts of the tree,
/// whenever the header is drawn the line under it, still inside the panel, is a directory.
#[test]
fn the_header_is_never_drawn_without_a_directory_under_it() {
    let home = tempfile::tempdir().unwrap();
    for preset in ["", "[keys]\npreset = \"search\"\n"] {
        let cfg = projects_config(home.path(), preset);
        for height in 3..=45 {
            for width in [30, 60, 99, 100, 150] {
                let mut popup = projects_popup(&cfg, home.path());
                if preset.is_empty() {
                    popup.on_key(ch('/'));
                }
                let screen = drawn(&mut popup, width, height);
                let lines = panel_lines(&popup, &screen);
                for (at, line) in lines.iter().enumerate() {
                    if line.contains("open a directory") {
                        assert!(
                            lines
                                .get(at + 1)
                                .is_some_and(|next| next.starts_with("    ~/")),
                            "{width}x{height} {preset:?}: the header is last or over a row that is not a directory:\n{}",
                            lines.join("\n")
                        );
                    }
                }
            }
        }
    }
}

/// A `[ui.sidebar.*] rows` template can draw more than one line for a row, which the scroll
/// counts in lines: with two lines a session header, the tree still scrolls above the pinned
/// section, the section does not move, and at every step of a walk through all the rows the
/// cursor's line is on screen, in the part it belongs to.
#[test]
fn rows_of_two_lines_scroll_above_the_pinned_section() {
    let home = tempfile::tempdir().unwrap();
    let cfg = projects_config(
        home.path(),
        "[ui.sidebar.spaces]\nrows = [[\"workspace\"], [\"workspace\"]]\n",
    );
    let mut popup = projects_popup(&cfg, home.path());
    popup.on_key(ch('/'));
    let total = popup.rows.rows.len();
    let first = popup.rows.first_project().unwrap();

    let (start, _) = panel_now(&mut popup, W, 20);
    let n = start.len();
    let section = start[n - 4..].to_vec();
    assert!(section[0].contains("open a directory"), "{start:#?}");
    assert_eq!(
        start.iter().filter(|line| line.contains("mobile")).count(),
        2,
        "the session's header is two lines: {start:#?}"
    );

    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    for step in 0..total {
        let (lines, lit) = panel_now(&mut popup, W, 20);
        assert_eq!(&lines[n - 4..], &section[..], "step {step}");
        let at = lit.unwrap_or_else(|| panic!("step {step}: no lit line:\n{lines:#?}"));
        if popup.sel < first {
            assert!(
                at < n - 4,
                "step {step}: row {} above the section",
                popup.sel
            );
        } else {
            assert_eq!(at, n - 3 + (popup.sel - first), "step {step}");
        }
        popup.on_key(down);
    }
}

/// Down to the narrowest panel the tree ever has (`TREE_MIN`, a popup as wide as that) every
/// row keeps its tag and the end of its name. Below it, with no room for a tag beside a path,
/// the path is still held to [`PROJECT_PATH_MIN`] columns, an ellipsis and the end of its name,
/// and it is the tag that goes: the row is cut at the panel's edge like any other line, with
/// an ellipsis that says so.
#[test]
fn a_narrow_panel_keeps_every_tag_down_to_the_narrowest_tree_and_below_it_keeps_the_name() {
    let home = tempfile::tempdir().unwrap();
    let cfg = long_paths_config(home.path());
    for width in [TREE_MIN, TREE_MIN + 1, 40, 60] {
        let mut popup = long_paths_popup(&cfg, home.path());
        popup.on_key(ch('/'));
        let rows = directory_rows(&mut popup, width, 39);
        assert_eq!(rows.len(), 3, "{width} columns: {rows:?}");
        for (row, (leaf, tag)) in rows.iter().zip(DIRECTORIES) {
            let end = &leaf[leaf.len() - 10..];
            assert!(row.trim_end().ends_with(tag), "{width}: {row:?}");
            assert!(
                row[..row.rfind(tag).unwrap()].trim_end().ends_with(end),
                "{width}: the end of the name is kept: {row:?}"
            );
        }
    }
    for width in [12, 20, 26, 30] {
        let mut popup = long_paths_popup(&cfg, home.path());
        popup.on_key(ch('/'));
        let rows = directory_rows(&mut popup, width, 39);
        let room = popup.hits.tree.area.width as usize - PROJECT_INDENT.len();
        assert_eq!(rows.len(), 3, "{width} columns: {rows:?}");
        for (row, (leaf, tag)) in rows.iter().zip(DIRECTORIES) {
            assert!(
                row.trim_end().ends_with(tag) || row.ends_with('\u{2026}'),
                "{width}: whole, or cut and says so: {row:?}"
            );
            if room >= PROJECT_PATH_MIN {
                let held = format!("\u{2026}{}", &leaf[leaf.len() - (PROJECT_PATH_MIN - 1)..]);
                assert!(
                    row.strip_prefix(PROJECT_INDENT)
                        .is_some_and(|path| path.starts_with(&held)),
                    "{width}: the path is held at {PROJECT_PATH_MIN} columns, {held:?}: {row:?}"
                );
            }
        }
    }
}

// ------------------------------------- numbers and letters told apart

/// Every tree row drawn, as `(its index in the rows, what the panel shows on its line)`, the
/// borders cut away, in the order drawn.
fn tree_rows(popup: &App, screen: &[String]) -> Vec<(usize, String)> {
    let inner = popup.hits.tree.area;
    popup
        .hits
        .tree
        .rows
        .iter()
        .map(|&(y, index)| {
            let text = screen[y as usize]
                .chars()
                .skip(inner.x as usize)
                .take(inner.width as usize)
                .collect();
            (index, text)
        })
        .collect()
}

/// The text of the row `index` among `rows`, as [`tree_rows`] read them.
fn row_text(rows: &[(usize, String)], index: usize) -> &str {
    rows.iter()
        .find_map(|(at, text)| (*at == index).then_some(text.as_str()))
        .unwrap_or_else(|| panic!("row {index} was not drawn: {rows:?}"))
}

/// Whether row `index` of the popup is a session's child: a pane or a parked worktree.
fn is_child(popup: &App, index: usize) -> bool {
    matches!(
        popup.rows.rows[index],
        Row::Pane(_) | Row::ClosedWorktree { .. }
    )
}

/// The letters of a session's rows must not clutter the session numbers, so the two look
/// different. The first column is the
/// numbers' alone. A header is its number, its flag and its name, as it was (`1 * mobile`); a
/// child row starts two columns in, its letter under the header's `*` and its connector under
/// the session's name (`  a ├ ⏵ claude`), in the session the cursor is in (mobile) and in the
/// others (web), a parked worktree with no letter too.
#[test]
fn the_numbers_have_the_first_column_and_a_rows_letter_sits_under_its_headers_star() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let screen = drawn(&mut popup, W, H);
    let rows = tree_rows(&popup, &screen);
    let letters = popup.row_letters();

    let (mut headers, mut children) = (0, 0);
    for (index, text) in &rows {
        let columns: Vec<char> = text.chars().collect();
        match popup.rows.rows[*index] {
            Row::Session(space) => {
                headers += 1;
                let name = &popup.snap.spaces[space].session_name;
                assert_eq!(
                    columns[0],
                    char::from(b'1' + space as u8),
                    "the number: {text:?}"
                );
                assert_eq!(columns[1], ' ', "{text:?}");
                assert!(
                    text.chars()
                        .skip(4)
                        .collect::<String>()
                        .starts_with(name.as_str()),
                    "the name starts in column 4: {text:?}"
                );
            }
            _ => {
                children += 1;
                assert_eq!(
                    &columns[..2],
                    [' ', ' '],
                    "nothing in the numbers' column: {text:?}"
                );
                assert_eq!(
                    columns[2],
                    letter_among(&letters, *index).unwrap_or(' '),
                    "the letter in column 2: {text:?}"
                );
                assert_eq!(columns[3], ' ', "{text:?}");
                assert!(
                    matches!(columns[4], '\u{251c}' | '\u{2514}'),
                    "the connector in column 4: {text:?}"
                );
                assert_eq!(columns[5], ' ', "{text:?}");
                assert_ne!(columns[6], ' ', "the glyph in column 6: {text:?}");
            }
        }
    }
    assert_eq!(headers, 5, "{rows:?}");
    // Eighteen panes and web's parked worktree.
    assert_eq!(children, 19, "{rows:?}");

    // mobile, the cursor's session, and web, another: each header, its first row and its
    // last, word for word.
    let starts = |text: &str| rows.iter().any(|(_, row)| row.starts_with(text));
    for text in [
        "1 * mobile",
        "  a \u{251c} \u{23f5} claude     1:zsh",
        "  e \u{2514} \u{25a2} htop       4:zsh.2",
        "2   web",
        "  a \u{251c} \u{25a2} zsh        1:zsh",
        "    \u{2514} \u{25cc} feat/pdf   worktree, closed",
    ] {
        assert!(
            starts(text),
            "no row starts {text:?}:\n{}",
            screen.join("\n")
        );
    }
}

/// A child row has the same columns lettered or not, so the tree never shifts as the letters
/// come and go: with `[ui] labels` off every child row is the lettered one with its letter's
/// column blank, the rest where it was; a row past the fifth of its session (web's parked
/// worktree) has that column blank all along. While `/` is open the letters stay, so every row is drawn as it was before `/`.
#[test]
fn a_child_row_keeps_its_columns_with_its_letter_and_without_it() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let screen = drawn(&mut popup, W, H);
    let lettered = tree_rows(&popup, &screen);
    let letters = popup.row_letters();
    popup.on_key(ch('/'));
    assert_eq!(
        popup.row_letters(),
        letters,
        "the same letters while / is open"
    );
    let screen = drawn(&mut popup, W, H);
    let typing = tree_rows(&popup, &screen);
    let off = config("[ui]\nlabels = false\n");
    let mut plain = app(&off);
    let screen = drawn(&mut plain, W, H);
    let unlabelled = tree_rows(&plain, &screen);

    let mut blanked = 0;
    for (index, text) in &lettered {
        if !is_child(&popup, *index) {
            continue;
        }
        let columns: Vec<char> = text.chars().collect();
        if columns[2] != ' ' {
            blanked += 1;
        }
        let without: String = columns
            .iter()
            .enumerate()
            .map(|(at, &c)| if at == 2 { ' ' } else { c })
            .collect();
        assert_eq!(row_text(&typing, *index), *text, "while / is open");
        assert_eq!(row_text(&unlabelled, *index), without, "with labels off");
    }
    // mobile, web and infra five each, api one and notes two: eighteen letters blanked.
    assert_eq!(blanked, 18, "{lettered:?}");
}

/// A click selects the row on its line wherever on the line it lands, so moving the letters
/// moves nothing a click finds: a child row's letter, its connector and its text each select
/// it (web's gemini, from the cursor's row in mobile).
#[test]
fn a_click_on_a_child_rows_letter_its_connector_or_its_text_selects_it() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let screen = drawn(&mut popup, W, H);
    let start = popup.sel;
    let at = |popup: &App, needle: &str| -> (u16, u16, usize) {
        let (y, x) = find(&screen, needle);
        let index = popup
            .hits
            .tree
            .rows
            .iter()
            .find_map(|&(row, index)| (row as usize == y).then_some(index))
            .unwrap_or_else(|| panic!("{needle:?} is on no row of the hit map"));
        (x as u16, y as u16, index)
    };
    let (text, y, gemini) = at(&popup, "gemini");
    let (_, _, own) = at(&popup, "Tidy page");
    assert_eq!(own, start, "the cursor opens on mobile's claude");
    let left = popup.hits.tree.area.x;
    for (what, column) in [
        ("letter", left + 2),
        ("connector", left + 4),
        ("text", text),
    ] {
        assert_eq!(screen[y as usize].chars().nth(left as usize + 2), Some('c'));
        popup.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            popup.sel, gemini,
            "a click on gemini's {what}, column {column}"
        );
        popup.sel = start;
        drawn(&mut popup, W, H);
    }
}

/// The cut: two columns further in, a child row reaches the panel's edge
/// two columns sooner, and it is cut there as any line is, ending in an ellipsis; a row that
/// fits is drawn whole, and nothing spills past the border, down to a popup 20 columns wide.
#[test]
fn a_narrow_popup_cuts_a_child_row_at_the_panels_edge_and_nothing_spills() {
    let cfg = config("");
    for width in [60u16, 44, 30, 20] {
        let mut popup = app(&cfg);
        let screen = drawn(&mut popup, width, 30);
        let whole = tree_lines(&popup);
        let inner = popup.hits.tree.area;
        let rows = tree_rows(&popup, &screen);
        let mut cut = 0;
        for &(y, index) in &popup.hits.tree.rows {
            let line: Vec<char> = screen[y as usize].chars().collect();
            assert_eq!(
                line.get(inner.right() as usize),
                Some(&'\u{2502}'),
                "{width}: the border closes {:?}",
                screen[y as usize]
            );
            let text = row_text(&rows, index);
            let natural: String = whole[index][0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            if natural.width() > usize::from(inner.width) {
                cut += 1;
                assert!(
                    text.ends_with('\u{2026}'),
                    "{width}: cut and says so: {text:?}"
                );
                let kept = text.trim_end_matches('\u{2026}');
                assert!(
                    natural.starts_with(kept),
                    "{width}: {text:?} of {natural:?}"
                );
            } else {
                assert_eq!(text.trim_end(), natural.trim_end(), "{width}: drawn whole");
            }
        }
        assert!(cut > 0, "{width}: some row is cut");
    }
}

/// Yellow is a key you can press: every session's number is drawn in the
/// theme's yellow, bold, on every header, the session you came from or not, the cursor's
/// session or not, wherever the cursor goes, as the numbers always work; the letters stay
/// yellow, bold, in the cursor's session alone, and dim in every other. The column after a
/// number is plain, as the one after a letter is.
#[test]
fn every_sessions_number_is_yellow_and_bold_and_only_the_cursors_letters_are() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let theme = popup.theme;
    let key = (theme.yellow, true);
    let dim = (theme.dim, false);
    let look = |popup: &mut App| {
        let buffer = frame(popup, W, H);
        let screen = lines_of(&buffer);
        let inner = popup.hits.tree.area;
        let style = |y: u16, column: u16| {
            let cell = &buffer[(inner.x + column, y)];
            (cell.fg, cell.modifier.contains(Modifier::BOLD))
        };
        let mut numbers = Vec::new();
        let mut letters = Vec::new();
        for &(y, index) in &popup.hits.tree.rows {
            let row = &screen[y as usize];
            match popup.rows.rows[index] {
                Row::Session(space) => {
                    assert!(
                        !style(y, 1).1,
                        "the column after the number is plain: {row}"
                    );
                    numbers.push((space, style(y, 0)));
                }
                _ => {
                    let space = tree::space_of(&popup.snap, &popup.rows.rows[index]).unwrap();
                    if row.chars().nth(inner.x as usize + 2) != Some(' ') {
                        letters.push((space, style(y, 2)));
                    }
                }
            }
        }
        (numbers, letters)
    };

    // The popup opens on mobile's claude: mobile's letters are the keys.
    let (numbers, letters) = look(&mut popup);
    assert_eq!(numbers.len(), 5, "{numbers:?}");
    for (space, style) in &numbers {
        assert_eq!(*style, key, "session {}'s number", space + 1);
    }
    assert_eq!(letters.len(), 18, "{letters:?}");
    for (space, style) in &letters {
        assert_eq!(
            *style,
            if *space == 0 { key } else { dim },
            "a letter of session {}",
            space + 1
        );
    }

    // 3 takes the cursor to infra: its letters light, the numbers stay as they are.
    popup.on_key(ch('3'));
    let (numbers, letters) = look(&mut popup);
    for (space, style) in &numbers {
        assert_eq!(
            *style,
            key,
            "session {}'s number, the cursor on infra",
            space + 1
        );
    }
    for (space, style) in &letters {
        assert_eq!(
            *style,
            if *space == 2 { key } else { dim },
            "a letter of session {}",
            space + 1
        );
    }

    // Folded to the sessions, the headers alone, numbered the same way.
    popup.on_key(ch('h'));
    popup.on_key(ch('h'));
    assert_eq!(popup.depth, Depth::Sessions);
    let (numbers, letters) = look(&mut popup);
    assert_eq!(numbers.len(), 5, "{numbers:?}");
    assert!(letters.is_empty(), "{letters:?}");
    for (space, style) in &numbers {
        assert_eq!(
            *style,
            key,
            "session {}'s number at the sessions depth",
            space + 1
        );
    }
}

/// The colour and weight of every number the tree draws, in the order drawn: a header's or a
/// flat row's label, in the panel's first column.
fn number_styles(popup: &mut App) -> Vec<(Color, bool)> {
    let buffer = frame(popup, W, H);
    let inner = popup.hits.tree.area;
    popup
        .hits
        .tree
        .rows
        .iter()
        .filter(|&&(_, index)| popup.row_label(index).is_some())
        .map(|&(y, _)| {
            let cell = &buffer[(inner.x, y)];
            assert!(
                cell.symbol().chars().all(|c| c.is_ascii_digit()),
                "a number in the first column: {:?}",
                cell.symbol()
            );
            (cell.fg, cell.modifier.contains(Modifier::BOLD))
        })
        .collect()
}

/// A number is yellow and bold exactly while it does what the number says (yellow is a key that works). With `/` open a digit is text for the query,
/// and that text names its session: typing a number puts the
/// cursor on that session, so every number stays yellow and bold while you type, a query typed
/// or not, and the hint line says what to type (`4a session 4 row a`) where it named the
/// number keys.
#[test]
fn the_numbers_stay_yellow_while_a_digit_typed_in_the_filter_names_its_session() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let theme = popup.theme;
    let key = (theme.yellow, true);
    let hint = |popup: &mut App| drawn(popup, W, H).last().cloned().unwrap_or_default();

    assert_eq!(
        number_styles(&mut popup),
        vec![key; 5],
        "the tree, nothing typed"
    );
    assert!(hint(&mut popup).contains("1-9 session"));

    popup.on_key(ch('/'));
    assert!(popup.typing());
    assert_eq!(number_styles(&mut popup), vec![key; 5], "while / is open");
    let typing = hint(&mut popup);
    assert!(typing.contains("/\u{258f}"), "{typing}");
    assert!(
        typing.contains("4a session 4 row a") && !typing.contains("1-9"),
        "what to type, and no number key the filter takes as text: {typing}"
    );
    popup.on_key(ch('t'));
    assert!(
        number_styles(&mut popup).iter().all(|style| *style == key),
        "with a query typed"
    );

    popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!popup.typing());
    assert_eq!(number_styles(&mut popup), vec![key; 5], "Esc");
    assert!(hint(&mut popup).contains("1-9 session"));
}

/// A flat list's numbers are keys too (each puts the cursor on its row), so they follow the
/// same rule: yellow and bold while they work, `/` open too, where a typed number names its row
/// too. `f` never turns a working key dim: the tree's numbers and the flat list's are yellow
/// alike. The flat list's columns are as they were.
#[test]
fn a_flat_lists_numbers_are_yellow_while_they_work_as_the_trees_are() {
    let cfg = config("[ui]\nlayout = \"flat\"\n");
    let mut popup = app(&cfg);
    let theme = popup.theme;
    let key = (theme.yellow, true);
    assert!(popup.flat_now());

    let flat = number_styles(&mut popup);
    assert_eq!(flat.len(), 9, "the first nine rows are numbered");
    assert!(flat.iter().all(|style| *style == key), "{flat:?}");
    let screen = drawn(&mut popup, W, H);
    let (y, x) = find(&screen, "\u{2502}1 ");
    assert_eq!(
        x, 0,
        "the first row's number right after the border: {}",
        screen[y]
    );

    popup.on_key(ch('/'));
    let typing = number_styles(&mut popup);
    assert_eq!(typing, vec![key; 9], "while / is open");
    popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    popup.on_key(ch('f'));
    assert!(!popup.flat_now());
    assert_eq!(
        number_styles(&mut popup),
        vec![key; 5],
        "f: the tree's are yellow too"
    );
    popup.on_key(ch('f'));
    assert!(number_styles(&mut popup).iter().all(|style| *style == key));
}

/// In the search preset a digit is text for the query, and that text names its session: typing 3 puts the cursor on infra's header.
/// So its numbers are yellow and bold as the modal preset's are, and a `[keys] select_session`
/// entry, which the search preset asks before its query, keeps them so.
#[test]
fn the_search_presets_numbers_are_yellow_as_typing_one_reaches_its_session() {
    let cfg = config("[keys]\npreset = \"search\"\n");
    let mut popup = app(&cfg);
    let theme = popup.theme;
    assert_eq!(number_styles(&mut popup), vec![(theme.yellow, true); 5]);
    popup.on_key(ch('3'));
    assert_eq!(
        popup.current(),
        Some(&Row::Session(2)),
        "typed 3 names infra"
    );
    assert_eq!(
        popup.filter.as_ref().map(|input| input.value()),
        Some("3"),
        "as text for the query"
    );

    let cfg = config(
        "[keys]\npreset = \"search\"\nselect_session = [\"1\", \"2\", \"3\", \"4\", \"5\", \"6\", \"7\", \"8\", \"9\"]\n",
    );
    let mut popup = app(&cfg);
    popup.on_key(ch('3'));
    assert_eq!(
        popup.current(),
        Some(&Row::Session(2)),
        "a bound digit selects session 3 here"
    );
    assert_eq!(number_styles(&mut popup), vec![(theme.yellow, true); 5]);
}

/// The help's legend draws a session's number as the tree draws the numbers at that moment: in
/// the modal preset, where they are keys, in the yellow of a key, bold; in the search preset,
/// where a typed digit names its session, yellow and bold too; and in a search preset whose
/// `[keys]` gives the numbers keys of their own, yellow as well.
#[test]
fn the_help_legend_draws_a_sessions_number_as_the_tree_does() {
    for (toml, line, look) in [
        ("", "its number puts the cursor on it", (true, true)),
        (
            "[keys]\npreset = \"search\"\n",
            "its number puts the cursor on it",
            (true, true),
        ),
        (
            "[keys]\npreset = \"search\"\nselect_session = [\"1\", \"2\", \"3\"]\n",
            "its number puts the cursor on it",
            (true, true),
        ),
    ] {
        let cfg = config(toml);
        let mut popup = app(&cfg);
        popup.overlay = Overlay::Help(Scroll::default());
        let theme = popup.theme;
        let buffer = frame(&mut popup, W, H);
        let screen = lines_of(&buffer);
        let (y, x) = find(&screen, line);
        // The legend's 3 is the last one before its words, which start past the subject column.
        let before: Vec<char> = screen[y].chars().take(x).collect();
        let at = before
            .iter()
            .rposition(|&c| c == '3')
            .unwrap_or_else(|| panic!("{toml:?}: no 3 before the words: {}", screen[y]));
        let cell = &buffer[(at as u16, y as u16)];
        let (yellow, bold) = look;
        assert_eq!(
            (cell.fg, cell.modifier.contains(Modifier::BOLD)),
            (if yellow { theme.yellow } else { theme.dim }, bold),
            "{toml:?}: {}",
            screen[y]
        );
    }
}

/// The colour and weight of every lit-or-dim letter the tree draws, in the order drawn, with
/// the session it belongs to: a child row's letter in the panel's third column.
fn letter_styles(popup: &mut App) -> Vec<(usize, (Color, bool))> {
    let buffer = frame(popup, W, H);
    let inner = popup.hits.tree.area;
    let rows: Vec<(u16, usize)> = popup.hits.tree.rows.clone();
    rows.into_iter()
        .filter(|&(_, index)| is_child(popup, index))
        .filter_map(|(y, index)| {
            let cell = &buffer[(inner.x + 2, y)];
            let lettered = cell.symbol().chars().all(|c| c.is_ascii_lowercase());
            let space = tree::space_of(&popup.snap, &popup.rows.rows[index])?;
            lettered.then(|| (space, (cell.fg, cell.modifier.contains(Modifier::BOLD))))
        })
        .collect()
}

/// While an overlay is open every key goes to it: `3` in the
/// rename prompt types a 3, and with the close question up only its own keys act. So the tree's
/// numbers and the cursor's session's letters are dim then, and yellow again once it closes.
/// The help's legend keeps drawing its `3` as the tree looks once the help is closed: yellow.
#[test]
fn with_an_overlay_open_the_numbers_and_the_lit_letters_are_dim() {
    let cfg = config("");
    let mut popup = app(&cfg);
    let theme = popup.theme;
    let key = (theme.yellow, true);
    let dim = (theme.dim, false);
    assert_eq!(
        number_styles(&mut popup),
        vec![key; 5],
        "the tree, no overlay"
    );
    let lit = |styles: &[(usize, (Color, bool))]| -> Vec<(Color, bool)> {
        styles
            .iter()
            .filter(|(space, _)| *space == 0)
            .map(|(_, style)| *style)
            .collect()
    };
    assert_eq!(
        lit(&letter_styles(&mut popup)),
        vec![key; 5],
        "mobile's letters"
    );

    // The help: every key is the help's, and the legend shows its 3 as the tree will look.
    popup.on_key(ch('?'));
    assert!(matches!(popup.overlay, Overlay::Help(_)));
    assert_eq!(number_styles(&mut popup), vec![dim; 5], "under the help");
    assert_eq!(
        lit(&letter_styles(&mut popup)),
        vec![dim; 5],
        "under the help"
    );
    let buffer = frame(&mut popup, W, H);
    let screen = lines_of(&buffer);
    let (y, x) = find(&screen, "its number puts the cursor on it");
    let three = screen[y]
        .chars()
        .take(x)
        .collect::<Vec<_>>()
        .iter()
        .rposition(|&c| c == '3');
    let cell = &buffer[(three.expect("the legend's 3") as u16, y as u16)];
    assert_eq!(
        (cell.fg, cell.modifier.contains(Modifier::BOLD)),
        key,
        "the legend's 3"
    );
    popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(number_styles(&mut popup), vec![key; 5], "the help closed");

    // A question (x on the cursor's row) and a prompt (r on a header): only their keys act.
    popup.on_key(ch('x'));
    assert!(matches!(popup.overlay, Overlay::Confirm(_)));
    assert_eq!(
        number_styles(&mut popup),
        vec![dim; 5],
        "under the close question"
    );
    assert_eq!(
        lit(&letter_styles(&mut popup)),
        vec![dim; 5],
        "under the close question"
    );
    popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    popup.on_key(ch('1'));
    popup.on_key(ch('r'));
    assert!(
        matches!(popup.overlay, Overlay::Prompt(_)),
        "{:?}",
        popup.overlay.is_open()
    );
    assert_eq!(
        number_styles(&mut popup),
        vec![dim; 5],
        "under the rename prompt"
    );
    popup.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!popup.overlay.is_open());
    assert_eq!(number_styles(&mut popup), vec![key; 5], "the prompt closed");
    assert_eq!(
        lit(&letter_styles(&mut popup)),
        vec![key; 5],
        "and mobile's letters"
    );
}

/// A number key that the open filter does not take as text still works while it is open: with
/// `[keys] select_session` on alt+1 to alt+5 the numbers stay
/// yellow with `/` open, and the hint line names them.
#[test]
fn a_number_key_that_is_not_text_keeps_its_number_yellow_while_the_filter_is_open() {
    let cfg = config(
        "[keys]\nselect_session = [\"alt+1\", \"alt+2\", \"alt+3\", \"alt+4\", \"alt+5\"]\n",
    );
    let mut popup = app(&cfg);
    let theme = popup.theme;
    popup.on_key(ch('/'));
    assert!(popup.typing());
    assert!((1..=5).all(|n| popup.number_works(n)));
    let screen = drawn(&mut popup, W, H);
    let hint = screen.last().cloned().unwrap_or_default();
    assert!(hint.contains("alt-1-alt-5 session"), "{hint}");
    let buffer = frame(&mut popup, W, H);
    let inner = popup.hits.tree.area;
    let headers: Vec<(Color, bool)> = popup
        .hits
        .tree
        .rows
        .iter()
        .filter(|&&(_, index)| matches!(popup.rows.rows[index], Row::Session(_)))
        .map(|&(y, _)| {
            let cell = &buffer[(inner.x, y)];
            (cell.fg, cell.modifier.contains(Modifier::BOLD))
        })
        .collect();
    assert_eq!(headers, vec![(theme.yellow, true); 5], "with / open");
    popup.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT));
    assert_eq!(
        popup.current(),
        Some(&Row::Session(2)),
        "alt+3 works from the filter"
    );
}

/// The search preset's help names the number keys while a `[keys] select_session` entry binds
/// them, on a line of their own under the move line, dropped while nothing is bound as any key
/// line with no key is; its legend's words beside the 3 say a number reaches its session either
/// way, as a typed number names its session.
#[test]
fn the_search_presets_help_names_the_number_keys_while_they_are_bound() {
    let help = |toml: &str| -> String {
        let cfg = config(toml);
        let mut popup = app(&cfg);
        popup.overlay = Overlay::Help(Scroll::default());
        drawn(&mut popup, W, H).join("\n")
    };
    let unbound = help("[keys]\npreset = \"search\"\n");
    assert!(!unbound.contains("jump to that session"), "{unbound}");
    // A typed number names its session, so the legend says it reaches it, bound or not.
    assert!(
        unbound.contains("its number puts the cursor on it"),
        "{unbound}"
    );
    assert!(!unbound.contains("numbered in tmux order"), "{unbound}");

    let bound = help(
        "[keys]\npreset = \"search\"\nselect_session = [\"1\", \"2\", \"3\", \"4\", \"5\", \"6\", \"7\", \"8\", \"9\"]\n",
    );
    let line = bound
        .lines()
        .find(|line| line.contains("jump to that session"))
        .unwrap_or_else(|| panic!("no number line:\n{bound}"));
    assert!(
        line.contains("1-9")
            && line.contains("jump to that session (yellow: a key that works now)"),
        "{line}"
    );
    let (row, n) = (
        bound.find("next / previous row").expect("the move line"),
        bound.find("jump to that session").unwrap(),
    );
    assert!(row < n, "the number line comes after the move line");
    assert!(
        bound.contains("its number puts the cursor on it"),
        "{bound}"
    );
    assert!(!bound.contains("numbered in tmux order"), "{bound}");
}

/// The filter's hint line as drawn while the modal filter has
/// focus: the query, what to type to reach a row (`4a session 4 row a`), then `Esc clear`,
/// `Tab next up`, `\u{2191}/\u{2193} move` and `Enter go`, and none of the keys the query takes as
/// text (`j/k move`, `/ filter`, `? help`).
#[test]
fn the_filters_hint_line_is_the_filters_own_while_the_filter_has_focus() {
    let cfg = config("");
    let mut popup = app(&cfg);
    popup.on_key(ch('/'));
    let screen = drawn(&mut popup, W, H);
    let hint = screen.last().cloned().unwrap_or_default();
    assert_eq!(
        hint.trim_end(),
        " /\u{258f}search\u{2026}  \u{b7}  4a session 4 row a  \u{b7}  Esc clear  \u{b7}  Tab next up  \u{b7}  \u{2191}/\u{2193} move  \u{b7}  Enter go"
    );
    for gone in ["j/k", "/ filter", "? help"] {
        assert!(!hint.contains(gone), "{gone}: {hint}");
    }
}

/// With `[ui] labels` off no number is drawn and the number keys do nothing, so the help follows
/// the numbers as the hint line does: it names no number keys, and
/// its legend's 3, dim, says only how the sessions are numbered, promising no number moves the
/// cursor. Nor are the letters drawn or keys then, and the help names no letter keys either.
/// With labels on the same help names both (the control).
#[test]
fn with_labels_off_the_help_names_no_number_keys_and_its_legend_promises_nothing() {
    for (toml, numbers) in [("[ui]\nlabels = false\n", false), ("", true)] {
        let cfg = config(toml);
        let mut popup = app(&cfg);
        assert_eq!(popup.numbers_work(), numbers);
        popup.overlay = Overlay::Help(Scroll::default());
        let theme = popup.theme;
        let buffer = frame(&mut popup, W, H);
        let screen = lines_of(&buffer);
        let help = screen.join("\n");
        assert_eq!(
            help.contains("jump to that session"),
            numbers,
            "{toml:?}:\n{help}"
        );
        // The letters' line follows the letters, which show and act only with labels on.
        assert_eq!(
            help.contains("jump to a row of your session"),
            numbers,
            "{toml:?}:\n{help}"
        );
        let words = if numbers {
            "its number puts the cursor on it"
        } else {
            "session: bold, numbered in tmux order"
        };
        let (y, x) = find(&screen, words);
        assert_eq!(
            help.contains("its number puts the cursor on it"),
            numbers,
            "{toml:?}"
        );
        let before: Vec<char> = screen[y].chars().take(x).collect();
        let three = before
            .iter()
            .rposition(|&c| c == '3')
            .expect("the legend's 3");
        let cell = &buffer[(three as u16, y as u16)];
        assert_eq!(
            (cell.fg, cell.modifier.contains(Modifier::BOLD)),
            if numbers {
                (theme.yellow, true)
            } else {
                (theme.dim, false)
            },
            "{toml:?}: the legend's 3"
        );
    }
}

/// With labels on and no key for the numbers (`[keys] select_session = []` in the modal preset)
/// the letters still show and jump, so the help names the letter keys and no number keys: each
/// line follows its own keys, not the other's. Its legend's 3
/// then says only how the sessions are numbered.
#[test]
fn with_no_number_key_bound_the_help_names_the_letter_keys_and_no_number_keys() {
    let cfg = config("[keys]\nselect_session = []\n");
    let mut popup = app(&cfg);
    assert!(!popup.numbers_work());
    popup.overlay = Overlay::Help(Scroll::default());
    let help = drawn(&mut popup, W, H).join("\n");
    let letters = help
        .lines()
        .find(|line| line.contains("jump to a row of your session"))
        .unwrap_or_else(|| panic!("no letters line:\n{help}"));
    assert!(letters.contains("a-e"), "{letters}");
    assert!(!help.contains("jump to that session"), "{help}");
    assert!(
        help.contains("session: bold, numbered in tmux order"),
        "{help}"
    );
}

/// The help spells the letter keys as the hint line does, from the letters `[keys]` leaves them:
/// a run while none is taken from its middle (`a-e`, `b-e`), the letters listed across a gap
/// (`a b d e`), and no line at all once every letter is an action's key, as the hint line then
/// has no entry for them. The help must not say `a-e` whatever `[keys]` takes.
#[test]
fn the_help_spells_the_letter_keys_as_the_hint_line_does() {
    for (toml, letters) in [
        ("", Some("a-e")),
        ("[keys]\nnext_attention = \"a\"\n", Some("b-e")),
        ("[keys]\nrename = \"c\"\n", Some("a b d e")),
        (
            "[keys]\nrename = [\"a\", \"b\", \"c\", \"d\", \"e\"]\n",
            None,
        ),
    ] {
        let cfg = config(toml);
        let mut popup = app(&cfg);
        let hint = popup
            .hint_line()
            .into_iter()
            .find_map(|(key, meaning)| (meaning == "rows").then_some(key));
        assert_eq!(hint.as_deref(), letters, "{toml:?}: the hint line");
        popup.overlay = Overlay::Help(Scroll::default());
        let help = drawn(&mut popup, W, H).join("\n");
        // The key column: what the help box draws between its left border and the meaning.
        let keys = help.lines().find_map(|line| {
            line.split_once("jump to a row of your session")
                .map(|(before, _)| before.rsplit('\u{2502}').next().unwrap_or(before).trim())
        });
        // And beside them the address of session 4's first lettered row, `4a` or `4b`.
        let address = letters.map(|letters| format!("{letters}   4{}", &letters[..1]));
        assert_eq!(keys, address.as_deref(), "{toml:?}:\n{help}");
    }
}

// While you search, the numbers stay lit and the letters stay drawn, and the same
// addresses work.

/// A popup of each preset with its search open and nothing typed yet: the modal preset after
/// `/`, the search preset as it opens.
fn searching() -> Vec<(&'static str, App<'static>)> {
    let modal = Box::leak(Box::new(config("")));
    let search = Box::leak(Box::new(config("[keys]\npreset = \"search\"\n")));
    let mut out = vec![("modal", app(modal)), ("search", app(search))];
    out[0].1.on_key(ch('/'));
    for (preset, popup) in &out {
        assert!(popup.typing(), "{preset}");
    }
    out
}

/// Every letter the tree draws, with its row and the colour and weight it is drawn in.
fn letter_cells(popup: &mut App) -> Vec<(Row, char, (Color, bool))> {
    let buffer = frame(popup, W, H);
    let inner = popup.hits.tree.area;
    let rows: Vec<(u16, usize)> = popup.hits.tree.rows.clone();
    rows.into_iter()
        .filter(|&(_, index)| is_child(popup, index))
        .filter_map(|(y, index)| {
            let cell = &buffer[(inner.x + 2, y)];
            let letter = cell.symbol().chars().next()?;
            letter.is_ascii_lowercase().then(|| {
                (
                    popup.rows.rows[index].clone(),
                    letter,
                    (cell.fg, cell.modifier.contains(Modifier::BOLD)),
                )
            })
        })
        .collect()
}

/// While you type, in either preset, every row keeps its letter, drawn where it was and unlit:
/// a letter typed now is text for the query, so no letter is a key, but each still says how to
/// reach its row. The numbers stay yellow and bold, as typing one reaches its session.
#[test]
fn while_you_type_the_letters_are_drawn_unlit_and_the_numbers_stay_yellow() {
    for (preset, mut popup) in searching() {
        let theme = popup.theme;
        let cells = letter_cells(&mut popup);
        assert_eq!(cells.len(), 18, "{preset}: {cells:?}");
        for (row, letter, style) in &cells {
            assert_eq!(
                *style,
                (theme.dim, false),
                "{preset}: {row:?}'s {letter} is unlit"
            );
        }
        assert_eq!(
            number_styles(&mut popup),
            vec![(theme.yellow, true); 5],
            "{preset}"
        );
    }
}

/// While you type, a session's number alone lights that session's letters, as the cursor is on
/// its header and its number and a letter reach any of its rows: `2` lights web's five, and
/// nobody else's; a query that is no number (`t`) lights none.
#[test]
fn typing_a_sessions_number_lights_that_sessions_letters() {
    for (preset, mut popup) in searching() {
        let theme = popup.theme;
        popup.on_key(ch('2'));
        assert_eq!(popup.current(), Some(&Row::Session(1)), "{preset}");
        let lit: Vec<(Row, char)> = letter_cells(&mut popup)
            .into_iter()
            .filter(|(.., style)| *style == (theme.yellow, true))
            .map(|(row, letter, _)| (row, letter))
            .collect();
        let web: Vec<(Row, char)> = (5..=9).map(Row::Pane).zip(JUMP_LETTERS.chars()).collect();
        assert_eq!(lit, web, "{preset}: 2 lights web's letters");

        popup.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        popup.on_key(ch('t'));
        let cells = letter_cells(&mut popup);
        assert!(!cells.is_empty(), "{preset}");
        assert!(
            cells.iter().all(|(.., style)| *style == (theme.dim, false)),
            "{preset}: t lights none: {cells:?}"
        );
    }
}

/// The query matches a row's address before its own words, and the address it matched is lit
/// where it is drawn: `2c` lights gemini's `c` (web's third row), while `gemini`, which matches
/// the same row by its words alone, leaves that letter unlit.
#[test]
fn a_letter_the_query_matched_is_lit() {
    for (preset, mut popup) in searching() {
        let theme = popup.theme;
        for c in "2c".chars() {
            popup.on_key(ch(c));
        }
        let gemini = |popup: &mut App| {
            letter_cells(popup)
                .into_iter()
                .find(|(row, ..)| *row == Row::Pane(7))
                .map(|(_, letter, style)| (letter, style))
        };
        assert_eq!(
            gemini(&mut popup),
            Some(('c', (theme.yellow, true))),
            "{preset}: 2c"
        );

        for _ in 0..2 {
            popup.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        for c in "gemini".chars() {
            popup.on_key(ch(c));
        }
        assert_eq!(
            gemini(&mut popup),
            Some(('c', (theme.dim, false))),
            "{preset}: gemini"
        );
    }
}

/// Only the letter itself lights a letter, not its session's number: `2w` matches web's rows
/// by the `2` of their address and the `w` of `web`, and leaves every letter unlit; `cg`
/// matches gemini's row by its letter `c` and the `g` of `gemini`, and lights that `c`.
#[test]
fn a_letter_lights_when_the_query_matched_it_not_its_number() {
    for (preset, mut popup) in searching() {
        let theme = popup.theme;
        for c in "2w".chars() {
            popup.on_key(ch(c));
        }
        let at = popup.rows.rows.iter().position(|row| *row == Row::Pane(5));
        let hits = popup.rows.scores[at.expect("web's row a matches")].clone();
        assert!(
            hits.as_ref()
                .is_some_and(|(_, at)| at.contains(&0) && !at.contains(&1)),
            "{preset}: the precondition, the 2 matched and the a did not: {hits:?}"
        );
        let cells = letter_cells(&mut popup);
        assert!(
            cells.iter().any(|(row, ..)| *row == Row::Pane(5)),
            "{preset}: {cells:?}"
        );
        assert!(
            cells.iter().all(|(.., style)| *style == (theme.dim, false)),
            "{preset}: 2w lights none: {cells:?}"
        );

        for _ in 0..2 {
            popup.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        for c in "cg".chars() {
            popup.on_key(ch(c));
        }
        let at = popup.rows.rows.iter().position(|row| *row == Row::Pane(7));
        let hits = popup.rows.scores[at.expect("gemini matches")].clone();
        assert!(
            hits.as_ref()
                .is_some_and(|(_, at)| at.contains(&1) && !at.contains(&0)),
            "{preset}: the precondition, the c matched and the 2 did not: {hits:?}"
        );
        let gemini = letter_cells(&mut popup)
            .into_iter()
            .find(|(row, ..)| *row == Row::Pane(7))
            .map(|(_, letter, style)| (letter, style));
        assert_eq!(gemini, Some(('c', (theme.yellow, true))), "{preset}: cg");
    }
}

/// A fresh popup of each preset, search open and `text` typed, as a tree and as a flat list.
fn typed_in_each(text: &str) -> Vec<(String, App<'static>)> {
    let mut out = Vec::new();
    for flat in [false, true] {
        for (preset, mut popup) in searching() {
            if flat {
                popup.layout = TreeLayout::Flat;
                popup.rebuild();
            }
            for c in text.chars() {
                popup.on_key(ch(c));
            }
            out.push((format!("{preset}, flat {flat}"), popup));
        }
    }
    out
}

/// What a query shows: the rows, their matches, the cursor, and the frame, styles included.
type Shown = (Vec<Row>, Vec<Option<Match>>, Option<Row>, Buffer);

fn query_shows(popup: &mut App) -> Shown {
    let buffer = frame(popup, W, H);
    (
        popup.rows.rows.clone(),
        popup.rows.scores.clone(),
        popup.current().cloned(),
        buffer,
    )
}

/// The frames' cells that differ, but for those where `other` draws the typed `query` (the input
/// line, a "nothing matches" note): there a cell whose symbol differs only in case, in the same
/// style, is the query's own letter, counted apart.
fn frame_differences(one: &Buffer, other: &Buffer, query: &str) -> (usize, Vec<(u16, u16)>) {
    let area = *one.area();
    let width = query.chars().count() as u16;
    let mut by_case = 0;
    let mut other_cells = Vec::new();
    for y in 0..area.height {
        // The columns where `other` draws the query on this line, one cell per character.
        let drawn_at = |x: u16| {
            (0..=x).any(|from| {
                from + width <= area.width
                    && x < from + width
                    && (from..from + width)
                        .map(|c| other[(c, y)].symbol())
                        .collect::<String>()
                        == query
            })
        };
        for x in 0..area.width {
            let (a, b) = (&one[(x, y)], &other[(x, y)]);
            if a == b {
                continue;
            }
            let same_style = a.fg == b.fg && a.bg == b.bg && a.modifier == b.modifier;
            if same_style
                && a.symbol() != b.symbol()
                && a.symbol().to_lowercase() == b.symbol().to_lowercase()
                && drawn_at(x)
            {
                by_case += 1;
            } else {
                other_cells.push((x, y));
            }
        }
    }
    (by_case, other_cells)
}

// A term that is an address, digits
// and then one letter (after `!`, `^` or `'`, before `$`), is matched ignoring case; every other
// term keeps smart case.

/// A typed address in capitals finds what it does in small letters: the same rows, matches and
/// cursor, and the same frame (highlights and letter colours) but for the query's own letters,
/// in both presets, as a tree and as a flat list; `!2C` drops what `!2c` drops (`gemini !2c` drops
/// web's gemini).
#[test]
fn a_typed_address_ignores_case() {
    let pairs = [
        ("2a", "2A"),
        ("2c", "2C"),
        ("4a", "4A"),
        ("^2b", "^2B"),
        ("'2d", "'2D"),
        ("2e$", "2E$"),
        ("gemini 2c", "gemini 2C"),
        ("!2c", "!2C"),
        ("gemini !2c", "gemini !2C"),
    ];
    for (small, capital) in pairs {
        let smalls = typed_in_each(small);
        let capitals_typed = typed_in_each(capital);
        for ((which, mut one), (_, mut other)) in smalls.into_iter().zip(capitals_typed) {
            let (rows, scores, cursor, frame_one) = query_shows(&mut one);
            let (rows_c, scores_c, cursor_c, frame_other) = query_shows(&mut other);
            assert_eq!(
                rows_c, rows,
                "{which}: {capital:?} keeps the rows {small:?} does"
            );
            assert_eq!(
                scores_c, scores,
                "{which}: {capital:?} matches as {small:?} does"
            );
            assert_eq!(
                cursor_c, cursor,
                "{which}: {capital:?} puts the cursor where {small:?} does"
            );
            let (by_case, cells) = frame_differences(&frame_one, &frame_other, capital);
            assert!(by_case > 0, "{which}: {capital:?} is drawn as typed");
            assert_eq!(
                cells,
                vec![],
                "{which}: {capital:?} draws what {small:?} draws but its own letters"
            );
        }
    }
    // An operator keeps its kind: `'2C` is exact, so in a tree it matches web's gemini alone,
    // where a fuzzy `2c` would match web's other rows too (the 2 of the address, a c later).
    let tree = |(which, _): &(String, App)| which.ends_with("flat false");
    for (which, popup) in typed_in_each("'2C").into_iter().filter(tree) {
        let matched: Vec<&Row> = popup
            .rows
            .rows
            .iter()
            .zip(&popup.rows.scores)
            .filter_map(|(row, score)| score.as_ref().map(|_| row))
            .collect();
        assert_eq!(matched, vec![&Row::Pane(7)], "{which}: '2C");
    }
    for (which, popup) in typed_in_each("2C").into_iter().filter(tree) {
        assert!(
            popup.rows.rows.contains(&Row::Pane(5)),
            "{which}: the precondition, a fuzzy 2C matches web's row a too"
        );
    }
    // The fixture's precondition: `!2c` drops a row in a tree (a flat list has no letters), so
    // the `!2C` pairs above mean something.
    for (which, popup) in typed_in_each("gemini").into_iter().filter(tree) {
        assert!(
            popup.rows.rows.contains(&Row::Pane(7)),
            "{which}: gemini finds web's gemini"
        );
    }
    for (which, popup) in typed_in_each("gemini !2c").into_iter().filter(tree) {
        assert!(
            !popup.rows.rows.contains(&Row::Pane(7)),
            "{which}: !2c drops web's gemini, labelled 2c"
        );
    }
}

/// Every other term keeps smart case: `Api`, `A` (a letter alone is no address) and `1Cl` (two
/// letters after the number) are case-sensitive, so each finds less than its small letters do.
#[test]
fn a_term_that_is_no_address_keeps_smart_case() {
    for (small, capital) in [("api", "Api"), ("a", "A"), ("1cl", "1Cl")] {
        for ((which, one), (_, other)) in
            typed_in_each(small).into_iter().zip(typed_in_each(capital))
        {
            assert!(
                !one.rows.rows.is_empty(),
                "{which}: the precondition, {small:?} finds rows"
            );
            assert_ne!(
                other.rows.rows, one.rows.rows,
                "{which}: {capital:?} is case-sensitive, unlike {small:?}"
            );
        }
    }
}

/// An overlay takes every key, while you type too: with `2` typed, the numbers and web's lit
/// letters go dim under it, and come back once it closes.
#[test]
fn while_you_type_an_overlay_dims_the_numbers_and_the_lit_letters() {
    for (preset, mut popup) in searching() {
        let theme = popup.theme;
        popup.on_key(ch('2'));
        popup.overlay = Overlay::Help(Scroll::default());
        // The help's box covers some of the tree: every number and letter still in view is dim.
        let numbers = number_styles(&mut popup);
        assert!(
            !numbers.is_empty() && numbers.iter().all(|style| *style == (theme.dim, false)),
            "{preset}: {numbers:?}"
        );
        let cells = letter_cells(&mut popup);
        assert!(
            cells.iter().any(|(row, ..)| *row == Row::Pane(5))
                && cells.iter().all(|(.., style)| *style == (theme.dim, false)),
            "{preset}: {cells:?}"
        );
        popup.overlay = Overlay::None;
        let numbers = number_styles(&mut popup);
        assert!(
            !numbers.is_empty() && numbers.iter().all(|style| *style == (theme.yellow, true)),
            "{preset}: the headers 2 leaves in view: {numbers:?}"
        );
        assert_eq!(
            letter_cells(&mut popup)
                .iter()
                .filter(|(.., style)| *style == (theme.yellow, true))
                .count(),
            5,
            "{preset}: web's letters again"
        );
    }
}

// ------------------------------------------------------------------ the sandbox's 📦

const BOX: &str = "\u{1f4e6}";

/// The sample world with five sandboxed agents.
fn boxed_app(cfg: &Config) -> App<'_> {
    let mut app = app(cfg);
    let mut snap = app.snap.clone();
    crate::tui::fixtures::sandboxed(&mut snap);
    app.set_snapshot(snap);
    app.rebuild();
    app
}

/// The screen line of the row drawn with `name` and `place`.
fn row_of(screen: &[String], name: &str, place: &str) -> usize {
    screen
        .iter()
        .position(|line| line.contains(name) && line.contains(place))
        .unwrap_or_else(|| panic!("no row {name} at {place}:\n{}", screen.join("\n")))
}

#[test]
fn an_agent_in_a_container_has_the_box_after_its_name_and_the_place_column_stays() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let screen = drawn(&mut app, W, H);

    // The five, by their places: mobile 1:zsh and 2:review, web 2:zsh.1 and 3:tests, infra 4:zsh.
    for (name, place) in [
        ("claude", "1:zsh"),
        ("claude", "2:review"),
        ("claude", "2:zsh.1"),
        ("codex", "3:tests"),
        ("claude", "4:zsh"),
    ] {
        let line = &screen[row_of(&screen, &format!("{name} {BOX}"), place)];
        assert!(line.contains(&format!("{name} {BOX}")), "{line}");
    }
    // The two outside any container keep no marker: web's gemini and infra's working claude.
    let gemini = &screen[find(&screen, "gemini").0];
    assert!(!gemini.contains(BOX), "{gemini}");
    let infra = &screen[row_of(&screen, "claude", "1:zsh.1")];
    assert!(!infra.contains(BOX), "{infra}");
    let (_, boxed) = find(&screen, "2:review");
    let (_, plain) = find(&screen, "2:zsh.2");
    assert_eq!(
        boxed,
        plain,
        "the place column is where it is on every row:\n{}",
        screen.join("\n")
    );
}

#[test]
fn a_name_of_eight_letters_is_cut_to_six_and_an_ellipsis_before_the_box() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let mut snap = app.snap.clone();
    let codex = snap
        .agents
        .iter_mut()
        .find(|a| a.pane_id == "%231")
        .unwrap();
    codex.kind = Some("opencode".to_string());
    let review = snap
        .agents
        .iter_mut()
        .find(|a| a.pane_id == "%121")
        .unwrap();
    review.kind = Some("copilot".to_string());
    app.set_snapshot(snap);
    app.rebuild();

    let screen = drawn(&mut app, W, H);

    let (y, _) = find(&screen, &format!("openco\u{2026} {BOX}"));
    assert!(screen[y].contains("3:tests"), "{}", screen[y]);
    let (y, _) = find(&screen, &format!("copilot {BOX}"));
    assert!(screen[y].contains("2:review"), "{}", screen[y]);
    let (_, cut) = find(&screen, "3:tests");
    let (_, plain) = find(&screen, "2:zsh.2");
    assert_eq!(cut, plain);
}

/// The marker keeps its own colours on a quiet row, an open window's bold row, a blocked row
/// and an unseen done row; the name keeps the row's.
#[test]
fn the_box_keeps_its_own_colours_and_the_name_keeps_the_rows() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    for (name, place, name_colour, bold) in [
        ("claude", "2:review", theme.fg, false),
        ("claude", "2:zsh.1", theme.fg, true),
        (
            "claude",
            "4:zsh",
            theme.state_color(Some(AgentState::Blocked)),
            true,
        ),
        (
            "codex",
            "3:tests",
            theme.state_color(Some(AgentState::Done)),
            false,
        ),
    ] {
        let y = row_of(&screen, &format!("{name} {BOX}"), place);
        let x = column_of(&screen[y], BOX);
        let mark = &buffer[(x as u16, y as u16)];
        assert!(
            !mark.modifier.contains(Modifier::BOLD),
            "{place}: the box is never bold"
        );
        assert_ne!(
            mark.fg,
            theme.state_color(Some(AgentState::Blocked)),
            "{place}"
        );
        assert_ne!(
            mark.fg,
            theme.state_color(Some(AgentState::Done)),
            "{place}"
        );
        let first = &buffer[(column_of(&screen[y], name) as u16, y as u16)];
        assert_eq!(first.fg, name_colour, "{place}");
        assert_eq!(first.modifier.contains(Modifier::BOLD), bold, "{place}");
    }
}

#[test]
fn the_flat_list_draws_the_box_in_the_same_place_and_a_folded_header_never_does() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    app.layout = TreeLayout::Flat;
    app.rebuild();
    let screen = drawn(&mut app, W, H);
    assert_eq!(
        screen
            .iter()
            .filter(|line| line.contains(&format!("claude {BOX}"))
                || line.contains(&format!("codex {BOX}")))
            .count(),
        5,
        "{}",
        screen.join("\n")
    );

    app.layout = TreeLayout::Tree;
    app.depth = Depth::Sessions;
    app.rebuild();
    let screen = drawn(&mut app, W, H);
    assert!(
        !screen.iter().any(|line| line.contains(BOX)),
        "{}",
        screen.join("\n")
    );
}

/// On a boxed row, a filter match in the part of the name cut away lights the
/// ellipsis, and the box after it is never lit.
#[test]
fn a_match_in_the_cut_end_of_a_boxed_name_lights_the_ellipsis_and_never_the_box() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let mut snap = app.snap.clone();
    snap.agents
        .iter_mut()
        .find(|a| a.pane_id == "%231")
        .unwrap()
        .kind = Some("opencode-x".to_string());
    app.set_snapshot(snap);
    // An exact term found only past the seventh letter, so that the ellipsis is lit by the
    // cut and not by being the seventh letter of a match that begins there.
    app.filter = Some(Input::new("'-x"));
    app.rebuild();
    let theme = app.theme;
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    let y = row_of(&screen, BOX, "3:tests");
    assert!(
        screen[y].contains(&format!("openco\u{2026} {BOX}")),
        "{}",
        screen[y]
    );
    let cut = column_of(&screen[y], "\u{2026}");
    let mark = column_of(&screen[y], BOX);
    assert_eq!(
        buffer[(cut as u16, y as u16)].fg,
        theme.yellow,
        "the cut is lit: {}",
        screen[y]
    );
    assert_ne!(
        buffer[(mark as u16, y as u16)].fg,
        theme.yellow,
        "the box is never lit: {}",
        screen[y]
    );
}

/// A title is a title. A pane titled `sandbox` or `📦 docker` with no runtime seen has no
/// marker after its name.
#[test]
fn no_box_without_a_container_whatever_the_title_says() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    for (pane, title) in [("%311", "\u{1f4e6} docker"), ("%222", "sandbox")] {
        snap.agents
            .iter_mut()
            .find(|a| a.pane_id == pane)
            .unwrap()
            .pane_title = title.to_string();
    }
    app.set_snapshot(snap);
    app.rebuild();
    let screen = drawn(&mut app, W, H);

    let (y, _) = find(&screen, "claude     1:zsh.1");
    assert!(
        screen[y].contains("docker"),
        "the title is drawn as a title: {}",
        screen[y]
    );
    assert!(!screen[y].contains(BOX), "{}", screen[y]);

    // The pane titled `sandbox`: drawn as its title, with no marker after the name.
    let (y, _) = find(&screen, "2:zsh.2");
    assert!(
        screen[y].contains("sandbox"),
        "the title is drawn as a title: {}",
        screen[y]
    );
    assert!(!screen[y].contains(BOX), "{}", screen[y]);
}

#[test]
fn a_cut_inside_the_box_draws_a_space_and_nothing_from_after_it() {
    let line = Line::from(vec![
        Span::raw("ab"),
        Span::raw(" \u{1f4e6}"),
        Span::raw("xyz"),
    ]);
    let cut = truncate_line(line, 5, Color::Reset);
    let text: String = cut.spans.iter().map(|span| span.content.as_ref()).collect();
    assert_eq!(text, "ab  \u{2026}");
    assert_eq!(text.width(), 5);
}

#[test]
fn at_60_columns_a_boxed_row_keeps_its_name_box_place_and_state_and_cuts_its_title() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let screen = drawn(&mut app, 60, H);

    let y = row_of(&screen, BOX, "2:zsh.1");
    let line = &screen[y];
    assert!(
        line.contains(&format!("claude {BOX}")) && line.contains("working"),
        "{line}"
    );
    // The panel's right border ends the line; what is before it is the cut title's ellipsis.
    assert!(
        line.trim_end()
            .trim_end_matches('\u{2502}')
            .trim_end()
            .ends_with('\u{2026}'),
        "the title is what is cut: {line}"
    );
}

/// A row cut at every width up to a little past its box: what is drawn is always the start of the
/// row (the box whole, or two spaces where it would have been cut), never half a box and never
/// what follows the box standing in its place, and never wider than the room it is given.
#[test]
fn a_boxed_row_cut_at_any_width_is_only_ever_the_start_of_the_row() {
    let cfg = config("");
    let app = boxed_app(&cfg);
    let boxed = tree_lines(&app)
        .into_iter()
        .flatten()
        .find(|line| line.spans.iter().any(|span| span.content.contains(BOX)))
        .expect("a boxed row");
    let whole: String = boxed.spans.iter().map(|s| s.content.as_ref()).collect();
    let spaced = whole.replace(BOX, "  ");
    let before = whole.split(BOX).next().unwrap().width();
    for width in 2..=before as u16 + 6 {
        let cut = truncate_line(boxed.clone(), width, Color::Reset);
        let text: String = cut.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.width() <= width as usize, "{width}: {text:?}");
        let kept = text.trim_end_matches('\u{2026}');
        assert!(
            whole.starts_with(kept) || spaced.starts_with(kept),
            "{width}: {text:?} of {whole:?}"
        );
    }
}

#[test]
fn the_filter_sandbox_finds_no_row_since_a_boxed_agents_row_draws_a_box_and_no_word() {
    let cfg = config("");
    let listed = |query: &str| -> Vec<String> {
        let mut app = boxed_app(&cfg);
        app.filter = Some(Input::new(query));
        app.rebuild();
        app.rows
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pane(at) => Some(app.snap.panes[*at].pane_id.clone()),
                _ => None,
            })
            .collect()
    };
    assert!(listed("sandbox").is_empty(), "{:?}", listed("sandbox"));
    // The control: the session's name, drawn on the header, still finds a boxed agent's row.
    assert!(listed("mobile").contains(&"%111".to_string()));
}

/// The row index of `pane` in what the popup lists.
fn row_index(app: &App, pane: &str) -> usize {
    app.rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(at) if app.snap.panes[*at].pane_id == pane))
        .unwrap()
}

/// Drawn 180 columns wide, where the preview has room for the whole title. The box is two
/// columns, and the cell after it is drawn blank, so the screen string holds two spaces after it.
#[test]
fn the_preview_title_names_the_runtime_and_the_profile_of_a_boxed_agent() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let screen = drawn(&mut app, 180, H);
    assert!(
        screen[0].contains(&format!(
            "preview \u{b7} ~/code/mobile \u{b7} {BOX}  in a sandbox: docker, profile box"
        )),
        "{}",
        screen[0]
    );

    app.sel = row_index(&app, "%341");
    let screen = drawn(&mut app, 180, H);
    assert!(
        screen[0].contains(&format!("{BOX}  in a sandbox: docker, not started by taw")),
        "{}",
        screen[0]
    );

    app.sel = row_index(&app, "%222");
    let screen = drawn(&mut app, 180, H);
    assert!(!screen[0].contains("sandbox"), "{}", screen[0]);
    assert!(!screen[0].contains(BOX), "{}", screen[0]);
}

/// The sample world with `%222`'s agent started through the profile `box` and no runtime seen: a
/// profile that isolates with something other than a container.
fn profile_without_a_container_app(cfg: &Config) -> App<'_> {
    let mut app = boxed_app(cfg);
    let mut snap = app.snap.clone();
    let agent = snap
        .agents
        .iter_mut()
        .find(|agent| agent.pane_id == "%222")
        .unwrap();
    agent.container = None;
    agent.sandbox = Some("box".to_string());
    app.set_snapshot(snap);
    app.rebuild();
    app.sel = row_index(&app, "%222");
    app
}

/// A profile whose container taw cannot see has no box
/// on its row, and the preview's title still names the profile, with no claim of a sandbox.
#[test]
fn the_preview_title_names_a_profile_whose_container_taw_cannot_see() {
    let cfg = config("");
    let mut app = profile_without_a_container_app(&cfg);
    let screen = drawn(&mut app, 180, H);
    assert!(
        screen[0]
            .contains("preview \u{b7} ~/code/web \u{b7} sandbox profile box, no container seen "),
        "{}",
        screen[0]
    );
    assert!(!screen[0].contains(BOX), "{}", screen[0]);
    assert!(!screen[0].contains("in a sandbox"), "{}", screen[0]);
    let (y, _) = find(&screen, "2:zsh.2");
    assert!(!screen[y].contains(BOX), "no box on the row: {}", screen[y]);

    // Too long for the panel: cut with an ellipsis, as the container's title is.
    let screen = drawn(&mut app, 100, H);
    assert!(
        preview_title_end(&screen[0]).ends_with('\u{2026}'),
        "{}",
        screen[0]
    );
}

/// The title's room: the panel less four columns. One column too long is cut, one that fits
/// exactly is whole.
#[test]
fn a_preview_title_is_cut_when_one_column_too_long_for_the_panel_and_whole_when_it_fits() {
    let title = " preview \u{b7} ~/p \u{b7} sandbox profile box, no container seen ".to_string();
    let columns = title.width() as u16;
    assert_eq!(fit_title(title.clone(), columns + 4), title);
    let cut = fit_title(title.clone(), columns + 3);
    assert!(cut.ends_with('\u{2026}'), "{cut:?}");
    assert_eq!(cut.width(), usize::from(columns) - 1, "{cut:?}");
}

/// The call site's room, drawn: the panel's own width less four, whatever the popup's width.
/// At the width where the sandbox title fits exactly it is whole; one column narrower it is
/// cut to the room, ellipsis last, and the border runs on to the corner.
#[test]
fn a_drawn_preview_title_is_whole_where_it_fits_exactly_and_cut_one_column_narrower() {
    let cfg = config("");
    let mut app = profile_without_a_container_app(&cfg);
    let title = " preview \u{b7} ~/code/web \u{b7} sandbox profile box, no container seen ";
    let needed = title.width() as u16 + 4;
    let mut at: Vec<(u16, u16, Vec<String>)> = Vec::new();
    for popup in 100..=180 {
        let screen = drawn(&mut app, popup, H);
        at.push((popup, app.hits.preview.width, screen));
    }
    let exact = at
        .iter()
        .find(|(_, panel, _)| *panel == needed)
        .unwrap_or_else(|| panic!("no popup width gives a panel of {needed} columns"));
    assert!(exact.2[0].contains(title), "{}", exact.2[0]);
    assert!(!exact.2[0].contains('\u{2026}'), "{}", exact.2[0]);
    let narrow = at
        .iter()
        .find(|(_, panel, _)| *panel == needed - 1)
        .unwrap_or_else(|| panic!("no popup width gives a panel of {} columns", needed - 1));
    let end = preview_title_end(&narrow.2[0]);
    assert!(end.ends_with('\u{2026}'), "{}", narrow.2[0]);
    // The title runs from the column after the corner, the room is the panel less four, and
    // the border's two columns before the corner are what is left.
    let cut: String = end
        .chars()
        .rev()
        .take_while(|c| *c != '\u{256d}')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert_eq!(cut.width(), usize::from(needed - 1 - 4), "{cut:?}");
}

/// What comes before the preview's top-right corner on the first screen line: the end of its
/// title, once the border's corner and its run of lines are taken off.
fn preview_title_end(top: &str) -> &str {
    top.trim_end()
        .trim_end_matches('\u{256e}')
        .trim_end_matches('\u{2500}')
}

#[test]
fn a_boxed_agents_title_too_long_for_the_panel_ends_in_an_ellipsis() {
    let cfg = config("");
    let mut app = boxed_app(&cfg);
    let screen = drawn(&mut app, 100, H);
    // The title's last mark is the ellipsis, right before the border goes on to the corner: the
    // title was cut where the panel ends, not by the border in the middle of a word.
    assert!(
        preview_title_end(&screen[0]).ends_with('\u{2026}'),
        "{}",
        screen[0]
    );

    app.sel = row_index(&app, "%222");
    let screen = drawn(&mut app, 100, H);
    assert!(
        !screen[0].contains('\u{2026}'),
        "a title that fits is whole: {}",
        screen[0]
    );
    assert!(
        preview_title_end(&screen[0]).ends_with(' '),
        "{}",
        screen[0]
    );
}

/// With a `[sandbox]` table set, `n`, `w`, their hint lines, the help and `o` draw what they
/// draw without one, and ctrl-s in `n` and `w` does what it does without one.
#[test]
fn a_sandbox_table_changes_nothing_the_popup_draws_or_does() {
    let table = "[sandbox]\nprofile = \"max\"\n\n[sandbox.agents]\ncodex = \"default\"\n";
    let after = |toml: &str, keys: &[KeyEvent]| -> Vec<String> {
        let cfg = config(toml);
        let mut app = app(&cfg);
        for key in keys {
            app.on_key(*key);
        }
        drawn(&mut app, W, H)
    };
    let key = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    for c in ['?', 'o', 'n', 'w'] {
        assert_eq!(
            after(table, &[key(c)]),
            after("", &[key(c)]),
            "{c} with a [sandbox] table"
        );
    }
    for c in ['n', 'w'] {
        assert_eq!(
            after(table, &[key(c), ctrl_s]),
            after("", &[key(c), ctrl_s]),
            "ctrl-s in {c}"
        );
    }
}

/// The cut's `\u{2026}` is lit for a match in the cut part of a title and for no other, with the
/// same mapping from the matcher's count to characters the letters use: a title that starts with
/// letters carrying accents (each an `e` and a combining mark, one grapheme of two characters, and
/// counted by byte by the matcher) before a match drawn in full leaves the cut as it is.
#[test]
fn a_match_drawn_in_full_after_multi_character_graphemes_does_not_light_the_cut() {
    let cfg = config("");
    let mut app = app(&cfg);
    let mut snap = app.snap.clone();
    let accented = "e\u{301}".repeat(8);
    for agent in snap.agents.iter_mut().filter(|a| a.pane_id == "%111") {
        agent.pane_title = format!("{accented} one two three four five six seven eight nine ten");
    }
    app.set_snapshot(snap);
    let theme = app.theme;
    app.filter = Some(Input::new("two"));
    app.rebuild();
    let buffer = frame(&mut app, W, H);
    let screen = lines_of(&buffer);

    assert_eq!(lit_symbols(&buffer, &screen, "one two", &theme), "atwo");
    let (y, _) = find(&screen, "one two");
    let end = screen[y].find('\u{2026}').expect("the title is cut");
    let cut = &buffer[(screen[y][..end].chars().count() as u16, y as u16)];
    assert_ne!(cut.fg, theme.yellow, "the cut is not lit: {}", screen[y]);
}
