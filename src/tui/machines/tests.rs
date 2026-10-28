//! `merge` and what it makes of a machine's state.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use super::*;
use crate::config::{Config, Depth, Layout};
use crate::machine::MachineView;
use crate::machine::wire::{State, WireAgent, WirePane, WireSpace, WireWorktree};
use crate::session::worktree::WorktreeRecord;
use crate::tui::app::App;
use crate::tui::fixtures::{sandboxed, snapshot, worktrees};
use crate::tui::input::Input;
use crate::tui::{theme, view};

/// The time every merge of these tests is made at, in unix milliseconds: no test reads the clock.
pub(crate) const NOW: u64 = 1_000_000;

/// A live machine called `name` whose state is `snap`'s rows as the feed sends them.
fn machine_of(name: &str, snap: &Snapshot) -> MachineView {
    MachineView {
        name: name.to_string(),
        live: true,
        words: String::new(),
        state: Some(State {
            spaces: snap.spaces.iter().map(WireSpace::from).collect(),
            panes: snap.panes.iter().map(WirePane::from).collect(),
            agents: snap.agents.iter().map(WireAgent::from).collect(),
            worktrees: Vec::new(),
            open_worktrees: Vec::new(),
        }),
        last_seen: Some(1_000_000),
        this_server: false,
        no_keeper: false,
    }
}

/// One agent session, `$3` of machine `name`, with one agent pane `%7` (a bare state, for a test of
/// one thing).
fn small_state() -> State {
    State {
        spaces: vec![WireSpace {
            session_id: "$3".into(),
            session_name: "garden".into(),
            session_path: "/home/pi/garden".into(),
            windows: 1,
            active_window_id: "@5".into(),
            active_pane_id: Some("%7".into()),
            state: Some("working".into()),
            agent_count: 1,
            ..WireSpace::default()
        }],
        panes: vec![WirePane {
            pane_id: "%7".into(),
            session_id: "$3".into(),
            window_id: "@5".into(),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 1,
            pane_index: 1,
            pane_current_command: "claude".into(),
            pane_active: true,
            alternate_on: false,
        }],
        agents: vec![WireAgent {
            pane_id: "%7".into(),
            session_id: "$3".into(),
            session_name: "garden".into(),
            window_id: "@5".into(),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 1,
            pane_index: 1,
            pane_title: "\u{2733} Water schedule".into(),
            kind: Some("claude".into()),
            state: Some("working".into()),
            state_since: 100,
            ..WireAgent::default()
        }],
        worktrees: Vec::new(),
        open_worktrees: Vec::new(),
    }
}

fn view_with(name: &str, state: Option<State>) -> MachineView {
    MachineView {
        name: name.to_string(),
        live: true,
        state,
        last_seen: Some(5_000),
        ..MachineView::default()
    }
}

// ----------------------------------------------------------------- no machines: the frame

/// A frame of the popup over `snap`, at the given depth and layout, with `query` typed into the
/// filter when there is one.
fn frame(snap: Snapshot, depth: Depth, layout: Layout, query: Option<&str>) -> Buffer {
    frame_with(snap, worktrees(), depth, layout, query)
}

/// [`frame`] over `snap` and the closed worktrees `records`.
fn frame_with(
    snap: Snapshot,
    records: Vec<WorktreeRecord>,
    depth: Depth,
    layout: Layout,
    query: Option<&str>,
) -> Buffer {
    let mut cfg = Config::default();
    cfg.ui.depth = depth;
    cfg.ui.layout = layout;
    let mut app = App::new(
        &cfg,
        theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
        "settings".to_string(),
        snap,
    );
    app.set_worktrees(records);
    if let Some(query) = query {
        app.filter = Some(Input::new(query));
        app.rebuild();
        app.settle_cursor();
    }
    let mut terminal = Terminal::new(TestBackend::new(150, 39)).expect("a test terminal");
    terminal
        .draw(|f| view::draw(f, &mut app))
        .expect("drawing must not fail");
    terminal.backend().buffer().clone()
}

/// The first cell two frames of one size disagree on, symbol or style, for a failure to name.
fn first_difference(a: &Buffer, b: &Buffer) -> Option<String> {
    assert_eq!(a.area(), b.area());
    for y in 0..a.area().height {
        for x in 0..a.area().width {
            let (left, right) = (&a[(x, y)], &b[(x, y)]);
            if left != right {
                return Some(format!("({x},{y}): {left:?} against {right:?}"));
            }
        }
    }
    None
}

fn worlds() -> Vec<(&'static str, Snapshot)> {
    let mut boxed = snapshot();
    sandboxed(&mut boxed);
    vec![
        ("the sample world", snapshot()),
        ("the sample world in sandboxes", boxed),
        ("an empty world", Snapshot::default()),
    ]
}

#[test]
fn merging_no_machines_changes_no_cell_of_any_fixture_frame() {
    let mut compared = 0;
    for (what, world) in worlds() {
        for depth in [Depth::Sessions, Depth::Agents, Depth::All] {
            for layout in [Layout::Tree, Layout::Flat] {
                for query in [None, Some("claude"), Some("web")] {
                    let plain = frame(world.clone(), depth, layout, query);
                    let merged = frame_with(
                        merge(world.clone(), &[], NOW),
                        merge_worktrees(worktrees(), &[]),
                        depth,
                        layout,
                        query,
                    );
                    if let Some(at) = first_difference(&plain, &merged) {
                        panic!("{what} at {depth:?} {layout:?} {query:?} differs at {at}");
                    }
                    compared += 1;
                }
            }
        }
    }
    assert_eq!(compared, 54);
}

#[test]
fn the_frames_compared_hold_a_closed_worktree_of_this_machine() {
    // Without one in the sample, the proof above would say nothing of the rows that are new.
    let buf = frame(snapshot(), Depth::All, Layout::Tree, None);
    assert!(line(&buf, find(&buf, "feat/pdf")).contains("worktree, closed"));
}

#[test]
fn one_machine_does_change_the_frame() {
    // The positive half of the proof above: the comparison can tell frames apart.
    let world = snapshot();
    let views = [view_with("studio", Some(small_state()))];
    let plain = frame(world.clone(), Depth::All, Layout::Tree, None);
    let merged = frame(merge(world, &views, NOW), Depth::All, Layout::Tree, None);
    assert!(first_difference(&plain, &merged).is_some());
}

#[test]
fn merging_no_machines_returns_the_snapshot_field_by_field() {
    let world = snapshot();
    let merged = merge(world.clone(), &[], NOW);
    assert_eq!(merged.agents, world.agents);
    assert_eq!(merged.spaces, world.spaces);
    assert_eq!(merged.panes, world.panes);
    assert_eq!(merged.own_pane, world.own_pane);
    assert_eq!(merged.current_session, world.current_session);
    assert_eq!(merged.previous_session, world.previous_session);
    assert_eq!(merged.view_error, world.view_error);
    assert!(merged.machines.is_empty());
    assert!(world.agents.iter().all(|row| row.machine.is_none()));
    assert!(world.spaces.iter().all(|row| row.machine.is_none()));
    assert!(world.panes.iter().all(|row| row.machine.is_none()));
}

// ----------------------------------------------------------------------------- the rows

#[test]
fn a_remote_row_is_this_machines_row_with_a_namespaced_id_and_a_machine() {
    let world = snapshot();
    let merged = merge(Snapshot::default(), &[machine_of("studio", &world)], NOW);
    assert_eq!(merged.spaces.len(), world.spaces.len());
    assert_eq!(merged.panes.len(), world.panes.len());
    assert_eq!(merged.agents.len(), world.agents.len());
    let ns = |id: &str| format!("studio:{id}");
    for (back, was) in merged.spaces.iter().zip(&world.spaces) {
        let mut want = was.clone();
        want.session_id = ns(&was.session_id);
        want.active_window_id = ns(&was.active_window_id);
        want.active_pane_id = was.active_pane_id.as_deref().map(ns);
        want.machine = Some(0);
        assert_eq!(*back, want);
    }
    for (back, was) in merged.panes.iter().zip(&world.panes) {
        let mut want = was.clone();
        want.pane_id = ns(&was.pane_id);
        want.session_id = ns(&was.session_id);
        want.window_id = ns(&was.window_id);
        want.machine = Some(0);
        assert_eq!(*back, want);
    }
    for (back, was) in merged.agents.iter().zip(&world.agents) {
        let mut want = was.clone();
        want.pane_id = ns(&was.pane_id);
        want.session_id = ns(&was.session_id);
        want.window_id = ns(&was.window_id);
        want.machine = Some(0);
        assert_eq!(*back, want);
    }
}

#[test]
fn remote_rows_come_after_this_machines_and_each_machine_has_its_index() {
    let views = [
        view_with("studio", Some(small_state())),
        view_with("pi", Some(small_state())),
    ];
    let world = snapshot();
    let (locals, agents, panes) = (world.spaces.len(), world.agents.len(), world.panes.len());
    let merged = merge(world, &views, NOW);
    assert_eq!(merged.spaces.len(), locals + 2);
    assert!(merged.spaces[..locals].iter().all(|s| s.machine.is_none()));
    assert_eq!(merged.spaces[locals].machine, Some(0));
    assert_eq!(merged.spaces[locals].session_id, "studio:$3");
    assert_eq!(merged.spaces[locals + 1].machine, Some(1));
    assert_eq!(merged.spaces[locals + 1].session_id, "pi:$3");
    assert_eq!(merged.agents[agents].machine, Some(0));
    assert_eq!(merged.agents[agents + 1].machine, Some(1));
    assert_eq!(merged.panes[panes].machine, Some(0));
    assert_eq!(merged.panes[panes + 1].machine, Some(1));
    let names: Vec<&str> = merged.machines.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["studio", "pi"]);
}

#[test]
fn a_remote_pane_points_at_its_own_agent_among_this_machines_and_another_machines() {
    let world = snapshot();
    let agents = world.agents.len();
    let panes = world.panes.len();
    let merged = merge(
        world,
        &[
            view_with("studio", Some(small_state())),
            view_with("pi", Some(small_state())),
        ],
        NOW,
    );
    // The two remote panes are both `%7`: the namespace tells them apart.
    assert_eq!(merged.panes[panes].agent, Some(agents));
    assert_eq!(merged.panes[panes + 1].agent, Some(agents + 1));
    assert_eq!(merged.agents[agents].pane_id, "studio:%7");
    assert_eq!(merged.agents[agents + 1].pane_id, "pi:%7");
    // A local pane keeps the index it had.
    let local = merged.panes.iter().position(|p| p.agent.is_some()).unwrap();
    assert!(local < panes);
    assert_eq!(merged.panes[local].agent, snapshot().panes[local].agent);
}

#[test]
fn a_remote_agent_sorts_by_its_sessions_place_among_all_the_sessions() {
    let world = snapshot();
    let locals = world.spaces.len();
    let merged = merge(
        world,
        &[
            view_with("studio", Some(small_state())),
            view_with("pi", Some(small_state())),
        ],
        NOW,
    );
    let orders: Vec<usize> = merged
        .agents
        .iter()
        .rev()
        .take(2)
        .map(|a| a.space_order)
        .collect();
    assert_eq!(orders, [locals + 1, locals]);
}

#[test]
fn an_agent_whose_session_the_state_does_not_list_sorts_last() {
    let mut state = small_state();
    state.agents[0].session_id = "$99".into();
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert_eq!(merged.agents[0].space_order, usize::MAX);
}

#[test]
fn an_agent_suspended_on_another_machine_is_suspended_in_its_row() {
    let mut state = small_state();
    state.agents[0].state = Some("idle".into());
    state.agents[0].suspended = true;
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert!(merged.agents[0].suspended);
    assert_eq!(merged.agents[0].state, Some(AgentState::Idle));
    let mut state = small_state();
    state.agents[0].state = Some("idle".into());
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert!(!merged.agents[0].suspended);
}

#[test]
fn a_state_word_taw_does_not_know_is_no_state() {
    let mut state = small_state();
    state.agents[0].state = Some("snoozing".into());
    state.spaces[0].state = Some("snoozing".into());
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert_eq!(merged.agents[0].state, None);
    assert_eq!(merged.spaces[0].state, None);
    let mut state = small_state();
    state.agents[0].state = Some("blocked".into());
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert_eq!(merged.agents[0].state, Some(AgentState::Blocked));
}

#[test]
fn a_title_comes_over_raw_and_stays_raw() {
    let mut state = small_state();
    state.agents[0].pane_title = "\u{2733} \u{2733} two glyphs".into();
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert_eq!(merged.agents[0].pane_title, "\u{2733} \u{2733} two glyphs");
}

#[test]
fn a_view_with_no_state_adds_a_header_and_no_rows_live_or_not() {
    let world = snapshot();
    let before = (world.spaces.len(), world.panes.len(), world.agents.len());
    let mut away = view_with("pi", None);
    away.live = false;
    away.words = "connecting".into();
    let merged = merge(world, &[view_with("studio", None), away], NOW);
    assert_eq!(
        (merged.spaces.len(), merged.panes.len(), merged.agents.len()),
        before
    );
    assert_eq!(merged.machines.len(), 2);
    assert!(merged.machines[0].live, "live with no state yet is live");
    assert!(!merged.machines[1].live);
    assert_eq!(merged.machines[1].words, "connecting");
    assert_eq!(merged.machines[1].last_seen, Some(5_000));
}

#[test]
fn the_machine_that_is_this_tmux_server_adds_a_header_and_no_rows() {
    let mut itself = view_with("laptop", Some(small_state()));
    itself.this_server = true;
    let world = snapshot();
    let locals = world.spaces.len();
    let merged = merge(world, &[itself], NOW);
    assert_eq!(merged.spaces.len(), locals);
    assert_eq!(merged.machines.len(), 1);
}

#[test]
fn this_machines_current_and_previous_session_stay_this_machines() {
    let world = snapshot();
    let merged = merge(world.clone(), &[machine_of("studio", &world)], NOW);
    assert_eq!(merged.current_session, world.current_session);
    assert_eq!(merged.previous_session, world.previous_session);
    // The same `$1` on studio is another session: it has the machine in its id.
    assert!(
        merged
            .spaces
            .iter()
            .filter(|s| s.session_id == "$1")
            .count()
            == 1
    );
    assert!(merged.spaces.iter().any(|s| s.session_id == "studio:$1"));
}

#[test]
fn a_state_with_a_missing_field_still_makes_rows() {
    // Every wire field has a default, so a line from an older taw is a row with blanks.
    let state: State =
        serde_json::from_str(r#"{"spaces":[{"session_id":"$1"}],"panes":[{"pane_id":"%1"}]}"#)
            .unwrap();
    let merged = merge(
        Snapshot::default(),
        &[view_with("studio", Some(state))],
        NOW,
    );
    assert_eq!(merged.spaces[0].session_id, "studio:$1");
    assert_eq!(merged.spaces[0].session_name, "");
    assert_eq!(merged.panes[0].pane_id, "studio:%1");
    assert_eq!(merged.panes[0].agent, None);
}

// ----------------------------------------------------------------- headers in the tree

use crate::tui::fixtures::space;
use crate::tui::tree::Row;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

const STUDIO_WORDS: &str = "";
const PI_WORDS: &str = "not answering, last seen 2m ago, trying again in 30s";

/// A second session for studio, with two agents.
fn photos_state() -> State {
    let mut state = small_state();
    state.spaces.push(WireSpace {
        session_id: "$4".into(),
        session_name: "photos".into(),
        session_path: "/home/pi/photos".into(),
        windows: 1,
        active_window_id: "@6".into(),
        active_pane_id: Some("%8".into()),
        state: Some("done".into()),
        agent_count: 1,
        ..WireSpace::default()
    });
    state.panes.push(WirePane {
        pane_id: "%8".into(),
        session_id: "$4".into(),
        window_id: "@6".into(),
        window_index: 1,
        window_name: "zsh".into(),
        window_panes: 1,
        pane_index: 1,
        pane_current_command: "codex".into(),
        pane_active: true,
        alternate_on: false,
    });
    state.agents.push(WireAgent {
        pane_id: "%8".into(),
        session_id: "$4".into(),
        session_name: "photos".into(),
        window_id: "@6".into(),
        window_index: 1,
        window_name: "zsh".into(),
        window_panes: 1,
        pane_index: 1,
        pane_title: "resize the thumbnails".into(),
        kind: Some("codex".into()),
        state: Some("done".into()),
        state_since: 100,
        ..WireAgent::default()
    });
    state
}

/// pi's one session, `homelab`, with an idle agent.
fn homelab_state() -> State {
    let mut state = small_state();
    state.spaces[0].session_name = "homelab".into();
    state.spaces[0].session_id = "$9".into();
    state.spaces[0].state = Some("idle".into());
    state.panes[0].session_id = "$9".into();
    state.agents[0].session_id = "$9".into();
    state.agents[0].session_name = "homelab".into();
    state.agents[0].state = Some("idle".into());
    state.agents[0].pane_title = "Caddy config".into();
    state
}

/// This machine with eight sessions, studio live with `garden` and `photos`, and pi, which has
/// dropped off, with `homelab`.
fn spec_world() -> Snapshot {
    let mut local = snapshot();
    for n in 6..=8 {
        local
            .spaces
            .push(space(n, &format!("~/code/extra{n}"), "main", (0, 0, 0, 0)));
    }
    let mut pi = view_with("pi", Some(homelab_state()));
    pi.live = false;
    pi.words = PI_WORDS.to_string();
    let mut studio = view_with("studio", Some(photos_state()));
    studio.words = STUDIO_WORDS.to_string();
    merge(local, &[studio, pi], NOW)
}

pub(crate) fn depth_all() -> Config {
    let mut cfg = Config::default();
    cfg.ui.depth = Depth::All;
    cfg
}

pub(crate) fn app_over<'a>(cfg: &'a Config, snap: Snapshot) -> App<'a> {
    let mut app = App::new(
        cfg,
        theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme,
        "settings".to_string(),
        snap,
    );
    // The clock of the merges: a refusal's age is exact.
    app.clock_ms = Some(NOW);
    app
}

fn draw(app: &mut App) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(150, 39)).expect("a test terminal");
    terminal
        .draw(|f| view::draw(f, app))
        .expect("drawing must not fail");
    terminal.backend().buffer().clone()
}

/// The text of row `y` of `buf`.
fn line(buf: &Buffer, y: u16) -> String {
    (0..buf.area().width)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect()
}

/// The first row of `buf` whose text holds `needle`.
fn find(buf: &Buffer, needle: &str) -> u16 {
    (0..buf.area().height)
        .find(|&y| line(buf, y).contains(needle))
        .unwrap_or_else(|| panic!("no line holds {needle:?}"))
}

fn press(app: &mut App, c: char) {
    app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

/// Opens the filter as a person does (`/`) and types `text`, so that the next key is the filter's own.
fn open_the_filter_and_type(app: &mut App, text: &str) {
    press(app, '/');
    for c in text.chars() {
        press(app, c);
    }
}

fn on_session<'a>(app: &'a App<'a>) -> &'a str {
    match app.current().expect("a row under the cursor") {
        Row::Session(space) => &app.snap.spaces[*space].session_name,
        other => panic!("expected a session, got {other:?}"),
    }
}

fn headers(app: &App) -> Vec<Option<usize>> {
    app.rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Machine(machine) => Some(*machine),
            _ => None,
        })
        .collect()
}

/// The rows of an app over `snap`, one line each, as the tree builds them.
fn row_lines(cfg: &Config, snap: Snapshot) -> Vec<String> {
    app_over(cfg, snap)
        .rows
        .rows
        .iter()
        .map(|row| format!("{row:?}"))
        .collect()
}

#[test]
fn a_table_of_this_tmux_server_alone_draws_no_header_and_the_rows_of_no_machines() {
    let cfg = depth_all();
    let plain = row_lines(&cfg, snapshot());
    assert!(
        plain.iter().all(|row| !row.contains("Machine")),
        "the positive half: no machines, no header: {plain:?}"
    );
    for live in [true, false] {
        let merged = merge(snapshot(), &[itself(live)], NOW);
        assert_eq!(merged.machines.len(), 1, "the entry keeps its index");
        assert!(!merged.has_other_machines());
        assert_eq!(row_lines(&cfg, merged.clone()), plain, "live {live}");
        assert!(headers(&app_over(&cfg, merged)).is_empty(), "live {live}");
    }
}

#[test]
fn one_other_machine_beside_this_tmux_server_keeps_every_header() {
    let cfg = depth_all();
    let world = snapshot();
    let merged = merge(
        world.clone(),
        &[itself(true), machine_of("studio", &world)],
        NOW,
    );
    assert!(merged.has_other_machines());
    let app = app_over(&cfg, merged);
    // This machine's header, the own-server entry's (index 0, no rows under it) and studio's (index 1).
    assert_eq!(headers(&app), vec![None, Some(0), Some(1)]);
    assert!(
        app.snap.spaces.iter().all(|s| s.machine != Some(0)),
        "the own-server entry draws no rows"
    );
    // Studio listed first keeps both headers too, the indices as the config has them.
    let merged = merge(
        world.clone(),
        &[machine_of("studio", &world), itself(true)],
        NOW,
    );
    assert_eq!(
        headers(&app_over(&cfg, merged)),
        vec![None, Some(0), Some(1)]
    );
}

#[test]
fn a_machine_no_app_keeps_is_another_machine_for_the_headers() {
    let cfg = depth_all();
    let mut studio = view_with("studio", None);
    studio.no_keeper = true;
    let merged = merge(snapshot(), &[itself(true), studio], NOW);
    assert!(merged.has_other_machines());
    assert_eq!(
        headers(&app_over(&cfg, merged)),
        vec![None, Some(0), Some(1)]
    );
}

#[test]
fn the_spec_screen_has_a_header_over_each_machine_in_config_order() {
    let cfg = depth_all();
    let app = app_over(&cfg, spec_world());
    assert_eq!(headers(&app), vec![None, Some(0), Some(1)]);
    assert!(matches!(app.rows.rows[0], Row::Machine(None)));
    // Each header comes right before its machine's first session.
    for (at, row) in app.rows.rows.iter().enumerate() {
        if let Row::Machine(machine) = row {
            let Row::Session(space) = &app.rows.rows[at + 1] else {
                panic!("a header is followed by its first session");
            };
            assert_eq!(app.snap.spaces[*space].machine, *machine);
        }
    }
}

#[test]
fn a_header_is_a_rule_with_the_name_in_bold_accent_then_counts_in_the_rows_glyphs_and_colours() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    let theme = app.theme;
    let buf = draw(&mut app);
    let y = find(&buf, "studio");
    let text = line(&buf, y);
    let at = text.find("studio").unwrap();
    // The panel's border is one column, then the rule's two, a space, the dot and a space, then the name.
    let x = u16::try_from(text[..at].chars().count()).unwrap();
    assert_eq!(buf[(x, y)].fg, theme.accent);
    assert!(
        buf[(x, y)]
            .modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the name is bold"
    );
    assert!(
        text[..at].contains("\u{2501}\u{2501} \u{25cf} "),
        "{text:?}"
    );
    assert_eq!(buf[(x - 2, y)].fg, theme.green, "a live machine's dot");
    assert_eq!(buf[(x - 5, y)].fg, theme.dim, "the rule");
    // studio has one working and one done agent: done first, in COUNTS order.
    let counts = &text[at + "studio".len()..];
    let icons = |s| crate::tui::data::state_icon(Some(s), cfg.ui.status_indicators);
    let done = format!(" {}1", icons(AgentState::Done));
    let working = format!(" {}1", icons(AgentState::Working));
    let done_at = counts.find(&done).unwrap();
    let working_at = counts.find(&working).unwrap();
    assert!(done_at < working_at);
    let done_x = x + u16::try_from("studio".len() + counts[..done_at].chars().count()).unwrap() + 1;
    assert_eq!(
        buf[(done_x, y)].fg,
        theme.state_color(Some(AgentState::Done))
    );
}

#[test]
fn this_machines_header_counts_its_own_agents_and_no_other() {
    let world = spec_world();
    let local: usize = world.agents.iter().filter(|a| a.machine.is_none()).count();
    assert!(world.agents.len() > local, "the machines have agents too");
    let HeaderTail::Counts(counts) = header_tail(&world, None) else {
        panic!("this machine's header is counts");
    };
    assert_eq!(counts.iter().map(|(_, n)| n).sum::<usize>(), local);
    let cfg = depth_all();
    let mut app = app_over(&cfg, world);
    let buf = draw(&mut app);
    assert!(line(&buf, find(&buf, this_machine_word())).contains(this_machine_word()));
}

#[test]
fn a_machine_that_is_not_live_says_its_words_in_dim_and_no_counts() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    let theme = app.theme;
    let buf = draw(&mut app);
    let y = find(&buf, "pi  not");
    let text = line(&buf, y);
    assert!(text.contains(&format!("pi  {PI_WORDS}")), "{text:?}");
    let at = text.find(PI_WORDS).unwrap();
    let x = u16::try_from(text[..at].chars().count()).unwrap();
    assert_eq!(buf[(x, y)].fg, theme.dim);
    assert_eq!(
        header_tail(&app.snap, Some(1)),
        HeaderTail::Words(PI_WORDS.to_string())
    );
}

#[test]
fn a_header_longer_than_the_panel_is_cut_to_one_line() {
    let mut world = spec_world();
    world.machines[1].words = "w ".repeat(200);
    let cfg = depth_all();
    let mut app = app_over(&cfg, world);
    let buf = draw(&mut app);
    let y = find(&buf, "pi  w w");
    // The next line is another row, not the header's words wrapped.
    assert!(
        !line(&buf, y + 1).contains("w w w w"),
        "{:?}",
        line(&buf, y + 1)
    );
    assert!(line(&buf, y).contains('\u{2026}'));
}

#[test]
fn a_view_that_says_nothing_and_is_not_live_is_connecting() {
    let mut away = view_with("pi", None);
    away.live = false;
    away.last_seen = None;
    let merged = merge(Snapshot::default(), &[away.clone()], NOW);
    assert_eq!(merged.machines[0].words, "connecting");
    away.last_seen = Some(NOW - 125_000);
    let merged = merge(Snapshot::default(), &[away], NOW);
    assert_eq!(merged.machines[0].words, "connecting, last seen 2m ago");
}

#[test]
fn the_age_in_a_machines_words_is_read_against_the_time_the_merge_is_given() {
    let mut away = view_with("pi", None);
    away.live = false;
    away.last_seen = Some(NOW - 5_000);
    let words = |at: u64| {
        merge(Snapshot::default(), &[away.clone()], at).machines[0]
            .words
            .clone()
    };
    // Exact ages, since no clock is read: the same view, merged at three times.
    assert_eq!(words(NOW), "connecting, last seen 5s ago");
    assert_eq!(words(NOW + 55_000), "connecting, last seen 1m ago");
    assert_eq!(words(NOW + 3_595_000), "connecting, last seen 1h ago");
}

#[test]
fn the_cursor_goes_over_a_header_and_never_stops_on_one() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    assert!(
        !matches!(app.current(), Some(Row::Machine(_))),
        "the opening row is not a header"
    );
    // Every row is reachable by `j` from the top, and none of the stops is a header.
    let mut seen = Vec::new();
    for _ in 0..app.rows.rows.len() {
        seen.push(app.sel);
        assert!(!matches!(app.current(), Some(Row::Machine(_))));
        press(&mut app, 'j');
    }
    for (at, row) in app.rows.rows.iter().enumerate() {
        assert_eq!(
            seen.contains(&at),
            !matches!(row, Row::Machine(_)),
            "row {at}"
        );
    }
}

#[test]
fn j_from_the_last_row_of_this_machine_lands_on_garden_and_k_comes_back() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    let studio_header = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Machine(Some(0))))
        .unwrap();
    app.sel = studio_header - 1;
    let last_local = app.sel;
    press(&mut app, 'j');
    assert_eq!(on_session(&app), "garden");
    assert_eq!(app.sel, studio_header + 1);
    press(&mut app, 'k');
    assert_eq!(app.sel, last_local);
}

#[test]
fn a_click_on_a_header_does_nothing() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    let buf = draw(&mut app);
    let y = find(&buf, "studio");
    // A positive wait: a click on the row below is a click that moves the cursor.
    let before = app.sel;
    for button in [MouseButton::Left, MouseButton::Right] {
        let event = MouseEvent {
            kind: MouseEventKind::Down(button),
            column: 5,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(event).is_none());
        assert_eq!(app.sel, before);
        assert!(!app.overlay.is_open(), "no menu opens on a header");
    }
    let below = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 5,
        row: y + 1,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(below);
    assert_ne!(app.sel, before, "a click on a session row moves the cursor");
}

#[test]
fn a_right_click_on_a_row_of_another_machine_says_why_and_opens_no_menu() {
    let cfg = depth_all();
    // Studio live (read only) and pi away (not connected): each says its own words.
    for (name, said) in [
        ("garden", "studio's rows are acted on from the app"),
        ("homelab", "pi is not connected"),
    ] {
        let mut app = app_over(&cfg, five_and_two("idle", false));
        let buf = draw(&mut app);
        let y = find(&buf, name);
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(event).is_none());
        let toast = toast_of(&app).expect("a toast");
        assert!(toast.contains(said), "{name}: {toast:?}");
        assert!(!app.overlay.is_open(), "{name}: a menu opened");
    }
    // The positive half: a row of this machine's still opens its menu.
    let mut app = app_over(&cfg, five_and_two("idle", false));
    let buf = draw(&mut app);
    let event = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: 5,
        row: find(&buf, "web"),
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(event);
    assert!(app.overlay.is_open(), "this machine's row has a menu");
    assert_eq!(toast_of(&app), None);
}

#[test]
fn numbers_run_on_across_machines_so_studios_first_session_is_nine() {
    let cfg = depth_all();
    let app = app_over(&cfg, spec_world());
    let garden = app
        .rows
        .rows
        .iter()
        .position(
            |row| matches!(row, Row::Session(s) if app.snap.spaces[*s].session_name == "garden"),
        )
        .unwrap();
    assert_eq!(app.row_label(garden).as_deref(), Some("9"));
    // A header has no number.
    assert_eq!(app.row_label(0), None);
}

#[test]
fn a_remote_session_is_never_this_machines_current_or_previous_one() {
    // studio's `$3` and `$4` are other sessions than this machine's `$3` and `$4`.
    let mut local = snapshot();
    local.current_session = Some("$3".into());
    local.previous_session = Some("$4".into());
    let world = merge(local, &[view_with("studio", Some(photos_state()))], NOW);
    let cfg = depth_all();
    let mut app = app_over(&cfg, world);
    let buf = draw(&mut app);
    let marks = |name: &str| -> (bool, bool) {
        let text = line(&buf, find(&buf, name));
        let head = text.split(name).next().unwrap().to_string();
        (head.contains('*'), head.contains('-'))
    };
    assert_eq!(marks("garden"), (false, false));
    assert_eq!(marks("photos"), (false, false));
    // And this machine's own `$3` is still marked: the positive half.
    let (name, _) = (
        app.snap
            .spaces
            .iter()
            .find(|s| s.session_id == "$3")
            .unwrap()
            .session_name
            .clone(),
        (),
    );
    assert!(
        marks(&name).0,
        "this machine's own `$3` is the current session"
    );
}

// ------------------------------------------- the flat list is machine-major

/// A live machine called `name` whose one agent is blocked, and has been for longer than any of this
/// machine's: the row the Attention sort puts first of all.
fn blocked_machine(name: &str) -> MachineView {
    let mut state = small_state();
    state.agents[0].state = Some("blocked".into());
    state.agents[0].seen = false;
    state.agents[0].state_since = 1_000_000;
    view_with(name, Some(state))
}

/// The flat config with `sort`.
fn flat_sorted(sort: AgentPanelSort) -> Config {
    let mut cfg = flat_config();
    cfg.ui.agent_panel_sort = sort;
    cfg
}

/// This machine's agent rows with the label each wears, in the order drawn.
fn local_labels(app: &App) -> Vec<(String, Option<String>)> {
    app.rows
        .rows
        .iter()
        .enumerate()
        .filter_map(|(at, row)| match row {
            Row::Pane(pane) if app.snap.panes[*pane].machine.is_none() => {
                Some((app.snap.panes[*pane].pane_id.clone(), app.row_label(at)))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_machine_that_appears_moves_no_label_of_this_machines_in_the_flat_list() {
    for sort in [AgentPanelSort::Attention, AgentPanelSort::Sessions] {
        let cfg = flat_sorted(sort);
        let alone = app_over(&cfg, snapshot());
        let with = app_over(&cfg, merge(snapshot(), &[blocked_machine("studio")], NOW));
        assert_eq!(local_labels(&with), local_labels(&alone), "{sort:?}");
        // The positive half: the machine's own blocked row is there, and after this machine's.
        let order = machines_in_order(&with);
        assert_eq!(order.last(), Some(&Some(0)), "{sort:?}");
        assert!(order.contains(&None), "{sort:?}");
    }
}

#[test]
fn in_the_flat_list_this_machine_comes_first_then_each_live_machine_in_config_order_then_those_away()
 {
    for sort in [AgentPanelSort::Attention, AgentPanelSort::Sessions] {
        let cfg = flat_sorted(sort);
        // Config order: pi, nas (away), studio. Studio is the one with the blocked agent.
        let mut nas = view_with("nas", Some(homelab_state()));
        nas.live = false;
        let views = [
            view_with("pi", Some(homelab_state())),
            nas,
            blocked_machine("studio"),
        ];
        let app = app_over(&cfg, merge(snapshot(), &views, NOW));
        let order = machines_in_order(&app);
        let mut seen: Vec<Option<usize>> = Vec::new();
        for machine in order {
            if seen.last() != Some(&machine) {
                seen.push(machine);
            }
        }
        // This machine, pi (0), studio (2), then nas (1), the one away, last: each in one run.
        assert_eq!(seen, vec![None, Some(0), Some(2), Some(1)], "{sort:?}");
    }
}

#[test]
fn a_machine_that_drops_moves_after_the_others_and_nothing_else_in_the_flat_list() {
    let cfg = flat_sorted(AgentPanelSort::Attention);
    let panes = |app: &App| -> Vec<String> {
        app.rows
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Pane(pane) if app.snap.panes[*pane].machine != Some(0) => {
                    Some(app.snap.panes[*pane].pane_id.clone())
                }
                _ => None,
            })
            .collect()
    };
    let live = app_over(
        &cfg,
        merge(
            snapshot(),
            &[
                blocked_machine("studio"),
                view_with("pi", Some(homelab_state())),
            ],
            NOW,
        ),
    );
    let mut studio = blocked_machine("studio");
    studio.live = false;
    let dropped = app_over(
        &cfg,
        merge(
            snapshot(),
            &[studio, view_with("pi", Some(homelab_state()))],
            NOW,
        ),
    );
    // Every row that is not studio's keeps its place relative to the others, and studio's go last.
    assert_eq!(panes(&dropped), panes(&live));
    assert_eq!(
        machines_in_order(&dropped).last(),
        Some(&Some(0)),
        "studio's rows are the last"
    );
    // The positive half: while it was live its row was not last (pi's came after, in config order).
    assert_eq!(machines_in_order(&live).last(), Some(&Some(1)));
}

// ------------------------------------------- Enter with only a header listed

#[test]
fn enter_with_only_a_machines_header_listed_says_there_is_nothing_here() {
    let cfg = depth_all();
    // No session of this machine, and the one machine has sent none yet: the list is headers alone.
    let mut waiting = view_with("studio", None);
    waiting.live = false;
    let mut app = app_over(&cfg, merge(Snapshot::default(), &[waiting], NOW));
    assert!(
        app.rows
            .rows
            .iter()
            .all(|row| matches!(row, Row::Machine(_)))
    );
    assert!(matches!(app.current(), Some(Row::Machine(_))));
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(action.is_none());
    let toast = toast_of(&app).expect("a toast");
    assert!(toast.starts_with("nothing here"), "{toast:?}");
    // The positive half: with a session listed Enter says no such thing.
    let mut app = app_over(&cfg, snapshot());
    app.sel = row_of(&app, "web");
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(action.is_some());
    assert_eq!(toast_of(&app), None);
}

// ------------------------------------------- Tab walks the machines that answer

/// This machine, studio (live) and pi (away), with no agent anywhere: `next_attention` has no agent
/// to stop on, so it walks the sessions.
fn sessions_only_world() -> Snapshot {
    let mut local = snapshot();
    local.agents.clear();
    local.panes.iter_mut().for_each(|pane| pane.agent = None);
    let mut studio = photos_state();
    studio.agents.clear();
    let mut pi = homelab_state();
    pi.agents.clear();
    let mut pi = view_with("pi", Some(pi));
    pi.live = false;
    merge(local, &[view_with("studio", Some(studio)), pi], NOW)
}

#[test]
fn tab_and_next_session_go_past_the_sessions_of_a_machine_that_is_away() {
    for (layout, cfg) in [("tree", depth_all()), ("flat", flat_config())] {
        for attention in [false, true] {
            for dir in [1, -1] {
                let mut app = app_over(&cfg, sessions_only_world());
                let mut names = Vec::new();
                for _ in 0..(app.rows.rows.len() * 2) {
                    if attention {
                        app.next_attention(dir);
                    } else {
                        app.next_session(dir);
                    }
                    names.push(app.selected_space().unwrap().session_name.clone());
                }
                let at = format!("{layout} attention={attention} dir={dir}");
                assert!(
                    !names.iter().any(|name| name == "homelab"),
                    "{at}: {names:?}"
                );
                // The positive half: the walk does reach this machine's and a live machine's.
                assert!(names.iter().any(|name| name == "photos"), "{at}: {names:?}");
                assert!(names.iter().any(|name| name == "mobile"), "{at}: {names:?}");
            }
        }
    }
}

// ---------------------------------- the cursor never rests on a header, whichever way it is moved

fn on_a_header(app: &App) -> bool {
    matches!(app.current(), Some(Row::Machine(_)))
}

/// The index of the header of machine `machine`.
fn header_row(app: &App, machine: usize) -> usize {
    app.rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Machine(Some(m)) if *m == machine))
        .expect("a header")
}

#[test]
fn a_refresh_that_clamps_the_cursor_onto_a_header_moves_it_off() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    app.sel = app.rows.rows.len() - 1;
    // Studio and pi drop their rows: the new list ends on a header, where the clamped index lands.
    let mut gone = view_with("studio", None);
    gone.live = false;
    let mut also = view_with("pi", None);
    also.live = false;
    app.set_snapshot(merge(snapshot(), &[gone, also], NOW));
    assert!(matches!(app.rows.rows.last(), Some(Row::Machine(_))));
    assert!(!on_a_header(&app), "{:?}", app.current());
    // The list got shorter, so the nearest row is the last one above the headers, not the top of the
    // list (which is where a wrap past the end would put it).
    let last_row = app
        .rows
        .rows
        .iter()
        .rposition(|row| !matches!(row, Row::Machine(_)))
        .expect("this machine's rows");
    assert!(last_row > 1, "{last_row}");
    assert_eq!(app.sel, last_row);
    // The positive half: a refresh that keeps the row leaves the cursor on it.
    let mut app = app_over(&cfg, spec_world());
    app.sel = row_of(&app, "photos");
    app.set_snapshot(spec_world());
    assert_eq!(on_session(&app), "photos");
}

#[test]
fn h_l_and_f_never_leave_the_cursor_on_a_header() {
    let cfg = depth_all();
    for key in ['h', 'l', 'f', 'f', 'l', 'h'] {
        let mut app = app_over(&cfg, spec_world());
        // A cursor put on studio's header, as the row the depth change would keep.
        app.sel = header_row(&app, 0);
        press(&mut app, key);
        assert!(!on_a_header(&app), "{key}: {:?}", app.current());
    }
}

#[test]
fn tab_and_shift_tab_walk_the_sessions_and_the_agents_without_stopping_on_a_header() {
    // Both walks leave a header out of what they look for, and `select` goes past one again: this
    // pins what the walk does, not either guard alone (removing one leaves it green).
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    for _ in 0..(app.rows.rows.len() * 2) {
        app.next_session(1);
        assert!(!on_a_header(&app));
        app.next_attention(1);
        assert!(!on_a_header(&app));
        app.next_session(-1);
        assert!(!on_a_header(&app));
        app.next_attention(-1);
        assert!(!on_a_header(&app));
    }
    // The positive half: the walk does reach a session of each machine.
    let mut names = Vec::new();
    for _ in 0..app.rows.rows.len() {
        app.next_session(1);
        names.push(on_session(&app).to_string());
    }
    assert!(names.iter().any(|name| name == "photos"), "{names:?}");
    assert!(names.iter().any(|name| name == "mobile"), "{names:?}");
}

#[test]
fn the_filters_best_match_never_puts_the_cursor_on_a_header() {
    let cfg = depth_all();
    // No session of this machine: the first row is studio's header, and with nothing matched the
    // cursor goes to row 0.
    let mut app = app_over(
        &cfg,
        merge(
            Snapshot::default(),
            &[view_with("studio", Some(photos_state()))],
            NOW,
        ),
    );
    assert!(matches!(app.rows.rows.first(), Some(Row::Machine(_))));
    app.cursor_to_best_match();
    assert!(!on_a_header(&app), "{:?}", app.current());
    // The positive half: typing a name puts it on that row.
    app.filter = Some(Input::new("photos"));
    app.rebuild();
    app.settle_cursor();
    assert_eq!(app.selected_space().unwrap().session_name, "photos");
}

#[test]
fn this_machines_previous_session_carries_the_dash_and_a_remote_session_next_to_it_does_not() {
    // Studio's `photos` is `$4`, as this machine's `api` is: only `api` is the one to go back to.
    let mut local = snapshot();
    local.current_session = Some("$1".into());
    local.previous_session = Some("$4".into());
    let world = merge(local, &[view_with("studio", Some(photos_state()))], NOW);
    let cfg = depth_all();
    let mut app = app_over(&cfg, world);
    let buf = draw(&mut app);
    let head = |name: &str| -> String {
        let text = line(&buf, find(&buf, name));
        text.split(name).next().unwrap().to_string()
    };
    assert!(head("api").contains('-'), "{:?}", head("api"));
    assert!(!head("photos").contains('-'), "{:?}", head("photos"));
    assert!(!head("photos").contains('*'));
}

#[test]
fn with_a_filter_a_header_shows_only_over_a_machine_with_a_match() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, spec_world());
    app.filter = Some(Input::new("photos"));
    app.rebuild();
    assert_eq!(headers(&app), vec![Some(0)], "only studio has a match");
    assert!(
        app.rows
            .rows
            .iter()
            .any(|row| matches!(row, Row::Session(_)))
    );
    app.filter = Some(Input::new("zzzzqq"));
    app.rebuild();
    assert!(app.rows.rows.is_empty());
}

#[test]
fn a_remote_title_loses_one_leading_glyph_when_it_is_drawn_and_never_twice() {
    let mut once = small_state();
    once.agents[0].pane_title = "\u{2733} Water schedule".into();
    let mut twice = small_state();
    twice.agents[0].pane_title = "\u{2733} \u{2810} Tidy zsh".into();
    let world = merge(
        Snapshot::default(),
        &[
            view_with("studio", Some(once)),
            view_with("pi", Some(twice)),
        ],
        NOW,
    );
    // `merge` keeps both raw.
    assert_eq!(world.agents[0].pane_title, "\u{2733} Water schedule");
    assert_eq!(world.agents[1].pane_title, "\u{2733} \u{2810} Tidy zsh");
    let mut cfg = depth_all();
    cfg.ui.titles = true;
    let mut app = app_over(&cfg, world);
    let buf = draw(&mut app);
    let first = line(&buf, find(&buf, "Water schedule"));
    assert!(!first.contains('\u{2733}'), "{first:?}");
    // It is the last child of its session, drawn as one, though a header follows.
    assert!(first.contains('\u{2514}'), "{first:?}");
    let second = line(&buf, find(&buf, "Tidy zsh"));
    assert!(!second.contains('\u{2733}'), "{second:?}");
    assert!(
        second.contains('\u{2810}'),
        "the second glyph is kept: {second:?}"
    );
}

// ----------------------------------------------------------------- greyed rows

/// This machine's five sample sessions, then studio (live, `garden` and `photos`) and pi, whose
/// feed has dropped (`homelab`), and what pi's one agent is doing there.
pub(crate) fn five_and_two(pi_agent: &str, pi_live: bool) -> Snapshot {
    let mut pi_state = homelab_state();
    pi_state.agents[0].state = Some(pi_agent.into());
    pi_state.spaces[0].state = Some(pi_agent.into());
    let mut pi = view_with("pi", Some(pi_state));
    pi.live = pi_live;
    pi.words = PI_WORDS.to_string();
    pi.last_seen = Some(NOW - 125_000);
    merge(
        snapshot(),
        &[view_with("studio", Some(photos_state())), pi],
        NOW,
    )
}

pub(crate) fn row_of(app: &App, session: &str) -> usize {
    app.rows
        .rows
        .iter()
        .position(
            |row| matches!(row, Row::Session(s) if app.snap.spaces[*s].session_name == session),
        )
        .unwrap_or_else(|| panic!("no row for {session}"))
}

pub(crate) fn toast_of(app: &App) -> Option<String> {
    app.toast.as_ref().map(|(text, _)| text.clone())
}

const NOT_CONNECTED: &str = "pi is not connected (last seen 2 min ago)";

#[test]
fn a_greyed_row_is_dim_whole_with_no_state_colour_and_keeps_its_state_words() {
    let cfg = depth_all();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    // The same working agent on a machine that is live and on one that is not.
    let mut live = app_over(&cfg, five_and_two("working", true));
    let mut gone = app_over(&cfg, five_and_two("working", false));
    let tree_cells = |buf: &Buffer, y: u16| -> Vec<(u16, ratatui::style::Color)> {
        (2..60)
            .filter(|&x| buf[(x, y)].symbol() != " ")
            .map(|x| (x, buf[(x, y)].fg))
            .collect()
    };
    // `homelab` is the last session: its agent is the line after its header.
    let live_buf = draw(&mut live);
    let y = find(&live_buf, "homelab") + 1;
    assert!(
        line(&live_buf, y).contains("working"),
        "{:?}",
        line(&live_buf, y)
    );
    assert!(
        tree_cells(&live_buf, y)
            .iter()
            .any(|(_, fg)| *fg == theme.blue),
        "a live working agent has the working colour"
    );
    let gone_buf = draw(&mut gone);
    let y = find(&gone_buf, "homelab") + 1;
    assert!(
        line(&gone_buf, y).contains("working"),
        "{:?}",
        line(&gone_buf, y)
    );
    for (x, fg) in tree_cells(&gone_buf, y) {
        assert_eq!(fg, theme.dim, "column {x} of {:?}", line(&gone_buf, y));
    }
    // The session's own header is dim too.
    let head = find(&gone_buf, "homelab");
    for (x, fg) in tree_cells(&gone_buf, head) {
        assert_eq!(fg, theme.dim, "column {x} of {:?}", line(&gone_buf, head));
    }
}

#[test]
fn a_greyed_agent_is_in_no_count_and_no_walk() {
    let cfg = depth_all();
    // pi's agent is blocked: it would need you, were pi live.
    let live = app_over(&cfg, five_and_two("blocked", true));
    let gone = app_over(&cfg, five_and_two("blocked", false));
    let local_only = app_over(&cfg, snapshot());
    assert!(
        live.hot_by_state()
            .iter()
            .any(|&(s, _)| s == AgentState::Blocked)
            && live.hot_count() > local_only.hot_count(),
        "a live blocked agent counts"
    );
    assert_eq!(gone.hot_by_state(), live_without_pi(&live));
    // The bar counts only what is live.
    let (live_bar, gone_bar) = (live.bar_counts(), gone.bar_counts());
    assert_eq!(live_bar.sessions, gone_bar.sessions + 1);
    assert_eq!(live_bar.agents, gone_bar.agents + 1);
    // The walk never stops on a row of pi.
    let pi_rows: Vec<usize> = gone
        .rows
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            crate::tui::tree::space_of(&gone.snap, row)
                .and_then(|s| gone.snap.spaces.get(s))
                .is_some_and(|s| s.machine == Some(1))
        })
        .map(|(at, _)| at)
        .collect();
    assert!(!pi_rows.is_empty());
    let walk = gone.next_up_rows();
    assert!(!walk.is_empty());
    assert!(walk.iter().all(|at| !pi_rows.contains(at)), "{walk:?}");
    assert!(live.next_up_rows().iter().any(|at| pi_rows.contains(at)));
}

/// What `hot_by_state` says of `app` when pi's rows are left out: the live app's, as the gone one's must be.
fn live_without_pi(app: &App) -> Vec<(AgentState, usize)> {
    crate::attention::hot_by_state(
        app.snap
            .agents
            .iter()
            .filter(|a| a.machine != Some(1))
            .map(|a| (a.state, a.seen)),
    )
}

#[test]
fn a_greyed_session_counts_nothing_on_a_folded_header() {
    let mut cfg = depth_all();
    cfg.ui.depth = Depth::Sessions;
    let mut app = app_over(&cfg, five_and_two("blocked", false));
    let buf = draw(&mut app);
    let text = line(&buf, find(&buf, "homelab"));
    let icon = crate::tui::data::state_icon(Some(AgentState::Blocked), cfg.ui.status_indicators);
    assert!(!text.contains(icon), "{text:?}");
    // And the same session of a live machine does show it.
    let mut live = app_over(&cfg, five_and_two("blocked", true));
    let buf = draw(&mut live);
    let text = line(&buf, find(&buf, "homelab"));
    assert!(text.contains(icon), "{text:?}");
}

#[test]
fn a_dropped_machine_moves_no_number() {
    let cfg = depth_all();
    let live = app_over(&cfg, five_and_two("idle", true));
    let gone = app_over(&cfg, five_and_two("idle", false));
    assert_eq!(live.rows.rows, gone.rows.rows);
    for name in ["garden", "photos", "homelab"] {
        let (a, b) = (row_of(&live, name), row_of(&gone, name));
        assert_eq!(live.row_label(a), gone.row_label(b), "{name}");
        assert!(gone.row_label(b).is_some(), "{name} keeps a number");
    }
}

#[test]
fn a_number_on_a_greyed_row_says_so_and_goes_nowhere() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", false));
    let before = app.sel;
    let number = app
        .row_label(row_of(&app, "homelab"))
        .expect("homelab has a number");
    press(&mut app, number.chars().next().unwrap());
    assert_eq!(toast_of(&app).as_deref(), Some(NOT_CONNECTED));
    assert_eq!(app.sel, before);
    // The positive half: a number of a live machine's session goes there.
    let number = app.row_label(row_of(&app, "garden")).unwrap();
    press(&mut app, number.chars().next().unwrap());
    assert_eq!(on_session(&app), "garden");
}

#[test]
fn enter_x_r_and_w_on_a_greyed_row_say_it_is_not_connected_and_do_nothing() {
    let cfg = depth_all();
    for key in [
        KeyCode::Enter,
        KeyCode::Char('x'),
        KeyCode::Char('r'),
        KeyCode::Char('w'),
    ] {
        let mut app = app_over(&cfg, five_and_two("idle", false));
        app.sel = row_of(&app, "homelab");
        let action = app.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(action.is_none(), "{key:?}");
        assert_eq!(toast_of(&app).as_deref(), Some(NOT_CONNECTED), "{key:?}");
        assert!(!app.overlay.is_open(), "{key:?} opened something");
    }
}

#[test]
fn x_r_and_w_on_a_live_remote_row_say_it_is_read_only_for_now() {
    let cfg = depth_all();
    for key in [KeyCode::Char('x'), KeyCode::Char('r'), KeyCode::Char('w')] {
        let mut app = app_over(&cfg, five_and_two("idle", false));
        app.sel = row_of(&app, "garden");
        let action = app.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(action.is_none(), "{key:?}");
        assert_eq!(toast_of(&app), Some(words::read_only("studio")), "{key:?}");
        assert!(!app.overlay.is_open(), "{key:?} opened something");
    }
}

/// In the app Enter is what goes into a machine: on a live machine's rows it produces the action this Mac's rows
/// produce, with the machine's ids (`studio:%7`), by every path that means Go (Enter, Enter inside a filter,
/// a double click). A machine that is away says as ever that it is not connected, whichever path.
#[test]
fn go_on_a_live_remote_row_in_the_app_goes_in_and_on_an_away_one_it_says_so() {
    use crate::tui::actions::Action;
    let cfg = depth_all();
    let enter = |app: &mut App| app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    // A session's header.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    app.sel = row_of(&app, "garden");
    assert_eq!(
        enter(&mut app),
        Some(Action::SwitchWorkspace {
            session: "studio:$3".into()
        })
    );
    assert_eq!(toast_of(&app), None);

    // A pane's row: the pane, its window and its session, all of studio's.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    app.sel = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(p) if app.snap.panes[*p].pane_id == "studio:%7"))
        .expect("studio's pane row");
    assert_eq!(
        enter(&mut app),
        Some(Action::Jump {
            session: "studio:$3".into(),
            window: "studio:@5".into(),
            pane: "studio:%7".into()
        })
    );

    // A closed worktree.
    let mut app = app_with(&cfg, rain_world(true));
    app.enter_app_mode(None, None);
    app.sel = worktree_row(&app, "feat/rain");
    assert!(matches!(
        enter(&mut app),
        Some(Action::OpenWorktree { ref workspace }) if workspace.starts_with("studio:")
    ));

    // Inside a typed filter Enter is the same Go, and so is a double click.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    open_the_filter_and_type(&mut app, "garden");
    assert_eq!(app.filter.as_ref().map(|f| f.value()), Some("garden"));
    let went = enter(&mut app);
    assert!(
        matches!(
            &went,
            Some(Action::SwitchWorkspace { session }) if session.starts_with("studio:")
        ) || matches!(&went, Some(Action::Jump { pane, .. }) if pane.starts_with("studio:")),
        "Enter inside the filter goes to a row of studio: {went:?}"
    );

    let double_click = |app: &mut App, name: &str| {
        let buf = draw(app);
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: find(&buf, name),
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.on_mouse(click).is_none(), "the first click only moves");
        app.on_mouse(click)
    };
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    assert_eq!(
        double_click(&mut app, "garden"),
        Some(Action::SwitchWorkspace {
            session: "studio:$3".into()
        }),
        "a double click"
    );

    // Away: the same words for every path, and nothing is produced.
    let mut away = app_over(&cfg, five_and_two("idle", false));
    away.enter_app_mode(None, None);
    away.sel = row_of(&away, "homelab");
    assert!(enter(&mut away).is_none());
    assert_eq!(toast_of(&away).as_deref(), Some(NOT_CONNECTED));
    let mut away = app_over(&cfg, five_and_two("idle", false));
    away.enter_app_mode(None, None);
    assert!(double_click(&mut away, "homelab").is_none());
    assert_eq!(toast_of(&away).as_deref(), Some(NOT_CONNECTED));
    let mut away = app_over(&cfg, five_and_two("idle", false));
    away.enter_app_mode(None, None);
    open_the_filter_and_type(&mut away, "homelab");
    assert_eq!(away.filter.as_ref().map(|f| f.value()), Some("homelab"));
    assert!(enter(&mut away).is_none());
    assert_eq!(toast_of(&away).as_deref(), Some(NOT_CONNECTED));
}

/// On a live machine's row in the app `x`, `r` and `w` work as on this Mac's row (a question, a prompt): nothing
/// says they are refused or not built. A right click opens the menu. The popup still says its own words.
#[test]
fn the_other_keys_on_a_live_remote_row_in_the_app_act_as_on_a_local_row() {
    let cfg = depth_all();
    for key in [KeyCode::Char('x'), KeyCode::Char('r'), KeyCode::Char('w')] {
        let mut app = app_over(&cfg, five_and_two("idle", true));
        app.enter_app_mode(None, None);
        app.sel = row_of(&app, "garden");
        let action = app.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(action.is_none(), "{key:?} asks first");
        assert_eq!(toast_of(&app), None, "{key:?}");
        assert!(app.overlay.is_open(), "{key:?} opened nothing");
    }
    // A right click opens the menu.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    let buf = draw(&mut app);
    let right = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: 5,
        row: find(&buf, "garden"),
        modifiers: KeyModifiers::NONE,
    };
    assert!(app.on_mouse(right).is_none());
    assert!(matches!(
        app.overlay,
        crate::tui::overlays::Overlay::Menu(_)
    ));
    assert_eq!(toast_of(&app), None);
    // Its close entry asks what the key asks.
    let asked = |app: &App| match &app.overlay {
        crate::tui::overlays::Overlay::Confirm(confirm) => confirm.message.clone(),
        _ => panic!("no question"),
    };
    assert!(
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
    );
    let mut keyed = app_over(&cfg, five_and_two("idle", true));
    keyed.enter_app_mode(None, None);
    keyed.sel = row_of(&keyed, "garden");
    assert!(
        keyed
            .on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(asked(&app), asked(&keyed));
    assert!(asked(&keyed).starts_with("kill-session studio \u{b7} garden"));
    // A machine that is away still refuses, and nothing is asked.
    let mut away = app_over(&cfg, five_and_two("idle", false));
    away.enter_app_mode(None, None);
    away.sel = row_of(&away, "homelab");
    assert!(
        away.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .is_none()
    );
    assert_eq!(toast_of(&away).as_deref(), Some(NOT_CONNECTED));
    assert!(!away.overlay.is_open());
    // The popup's right click says what it said.
    let mut popup = app_over(&cfg, five_and_two("idle", true));
    let buf = draw(&mut popup);
    let right = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column: 5,
        row: find(&buf, "garden"),
        modifiers: KeyModifiers::NONE,
    };
    assert!(popup.on_mouse(right).is_none());
    assert!(!popup.overlay.is_open());
    assert_eq!(toast_of(&popup), Some(words::read_only("studio")));
}

/// In the popup Enter on a live machine's session or pane is the doorway into the app, on the machine's own id
/// and its name; a closed worktree says it is acted on from the app, and an away machine that it is not connected.
/// Nothing opens, and x, r and w stay refused (the test above).
#[test]
fn enter_on_a_live_remote_row_in_the_popup_asks_for_the_doorway() {
    use crate::tui::actions::Action;
    let cfg = depth_all();
    let enter = |app: &mut App| app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.sel = row_of(&app, "garden");
    let session = app
        .snap
        .spaces
        .iter()
        .find(|s| s.machine == Some(0))
        .unwrap();
    let want_session = session.session_id.rsplit(':').next().unwrap().to_string();
    assert_eq!(
        enter(&mut app),
        Some(Action::Doorway {
            machine: "studio".into(),
            id: want_session
        })
    );
    assert_eq!(toast_of(&app), None);
    assert!(!app.overlay.is_open());

    // A pane of studio's: its own `%` id.
    let pane_row = app
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(p) if app.snap.panes[*p].machine == Some(0)))
        .expect("studio has a pane row at this depth");
    app.sel = pane_row;
    let Row::Pane(p) = app.rows.rows[pane_row] else {
        unreachable!()
    };
    let raw = app.snap.panes[p]
        .pane_id
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    assert!(raw.starts_with('%'), "{raw}");
    assert_eq!(
        enter(&mut app),
        Some(Action::Doorway {
            machine: "studio".into(),
            id: raw
        })
    );

    // This Mac's own rows go as before: a jump, never a doorway.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.sel = row_of(&app, "web");
    assert!(
        matches!(
            enter(&mut app),
            Some(Action::Jump { .. } | Action::SwitchWorkspace { .. })
        ),
        "this Mac's row goes in as before"
    );

    // Away: not connected, and nothing is asked for.
    let mut away = app_over(&cfg, five_and_two("idle", false));
    away.sel = row_of(&away, "homelab");
    assert!(enter(&mut away).is_none());
    assert_eq!(toast_of(&away).as_deref(), Some(NOT_CONNECTED));
}

#[test]
fn this_machines_own_rows_are_acted_on_as_before() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", false));
    app.sel = row_of(&app, "web");
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(action.is_some(), "Enter on a local session switches to it");
    assert_eq!(toast_of(&app), None);
}

#[test]
fn folded_headers_of_a_greyed_machine_are_not_in_the_walk_either() {
    let mut cfg = depth_all();
    cfg.ui.depth = Depth::Sessions;
    let gone = app_over(&cfg, five_and_two("blocked", false));
    let live = app_over(&cfg, five_and_two("blocked", true));
    let homelab = row_of(&gone, "homelab");
    assert!(
        live.next_up_rows().contains(&homelab),
        "live: the header stands for its agent"
    );
    assert!(!gone.next_up_rows().contains(&homelab));
    // Nor does a folded header of a machine that is away name its agent's title.
    assert_eq!(
        gone.session_hot_agent(
            gone.rows
                .rows
                .iter()
                .find_map(|r| match r {
                    Row::Session(s) if gone.snap.spaces[*s].session_name == "homelab" => Some(*s),
                    _ => None,
                })
                .unwrap()
        ),
        None
    );
}

// ----------------------------------------------------------------- the flat list and the filter

fn flat_config() -> Config {
    let mut cfg = depth_all();
    cfg.ui.layout = Layout::Flat;
    cfg
}

/// The machine of each pane row, in the order the rows are drawn.
fn machines_in_order(app: &App) -> Vec<Option<usize>> {
    app.rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Pane(pane) => Some(app.snap.panes[*pane].machine),
            _ => None,
        })
        .collect()
}

/// This machine's five sessions, then pi (dropped, first in the config) and studio (live).
fn dropped_machine_first() -> Snapshot {
    let mut pi = view_with("pi", Some(homelab_state()));
    pi.live = false;
    pi.words = PI_WORDS.to_string();
    merge(
        snapshot(),
        &[pi, view_with("studio", Some(photos_state()))],
        NOW,
    )
}

#[test]
fn a_flat_row_of_another_machine_names_its_session_with_the_machine() {
    let cfg = flat_config();
    let mut app = app_over(&cfg, five_and_two("idle", true));
    let buf = draw(&mut app);
    let text = line(&buf, find(&buf, "studio \u{b7} garden"));
    assert!(text.contains("claude"), "{text:?}");
    assert!(
        (0..buf.area().height).any(|y| line(&buf, y).contains("pi \u{b7} homelab")),
        "pi's row says its machine too"
    );
    // This machine's own rows say the session alone.
    let tree_part = |y: u16| -> String { line(&buf, y).chars().take(62).collect() };
    let local: Vec<u16> = (0..buf.area().height)
        .filter(|&y| tree_part(y).contains("mobile"))
        .collect();
    assert!(!local.is_empty(), "this machine's rows are drawn");
    assert!(local.iter().all(|&y| !tree_part(y).contains('\u{b7}')));
}

#[test]
fn the_flat_list_puts_the_rows_of_a_machine_that_is_away_after_every_live_row() {
    let cfg = flat_config();
    let app = app_over(&cfg, dropped_machine_first());
    let order = machines_in_order(&app);
    assert!(
        order.contains(&Some(0)),
        "the dropped machine's row is listed"
    );
    let first_greyed = order.iter().position(|m| *m == Some(0)).unwrap();
    assert!(
        order[first_greyed..].iter().all(|m| *m == Some(0)),
        "{order:?}"
    );
    // In the config's order pi comes before studio: the tree keeps it so.
    let tree_cfg = depth_all();
    let tree = app_over(&tree_cfg, dropped_machine_first());
    let in_tree = machines_in_order(&tree);
    assert!(
        in_tree.iter().position(|m| *m == Some(0)) < in_tree.iter().position(|m| *m == Some(1))
    );
}

#[test]
fn the_machines_name_is_in_the_text_a_filter_matches() {
    let world = five_and_two("idle", true);
    for (layout, cfg) in [("tree", depth_all()), ("flat", flat_config())] {
        let mut app = app_over(&cfg, world.clone());
        app.filter = Some(Input::new("studio"));
        app.rebuild();
        let studio: Vec<bool> = app
            .rows
            .rows
            .iter()
            .filter_map(|row| crate::tui::tree::space_of(&app.snap, row))
            .map(|space| app.snap.spaces[space].machine == Some(0))
            .collect();
        assert!(studio.contains(&true), "{layout}: studio's rows match");
        assert!(
            !app.rows.rows.is_empty() && studio.iter().filter(|s| **s).count() >= 2,
            "{layout}"
        );
        // Every session of studio is listed, and a session of this machine that does not hold the
        // word is not.
        let names: Vec<&str> = app
            .rows
            .rows
            .iter()
            .filter_map(|row| crate::tui::tree::space_of(&app.snap, row))
            .map(|space| app.snap.spaces[space].session_name.as_str())
            .collect();
        assert!(
            names.contains(&"garden") && names.contains(&"photos"),
            "{layout}: {names:?}"
        );
        assert!(
            !names.contains(&"homelab"),
            "{layout}: pi is not studio: {names:?}"
        );
    }
}

#[test]
fn a_machine_that_is_away_is_found_by_its_name_or_its_session() {
    for query in ["homelab", "pi"] {
        let cfg = flat_config();
        let mut app = app_over(&cfg, five_and_two("idle", false));
        app.filter = Some(Input::new(query));
        app.rebuild();
        assert!(
            machines_in_order(&app).contains(&Some(1)),
            "/{query} lists pi's row"
        );
    }
}

#[test]
fn a_match_in_the_session_column_lights_the_session_and_not_the_machine() {
    let cfg = flat_config();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.filter = Some(Input::new("garden"));
    app.rebuild();
    let buf = draw(&mut app);
    let y = find(&buf, "studio \u{b7} garden");
    let text = line(&buf, y);
    let lit: String = (0..buf.area().width)
        .filter(|&x| buf[(x, y)].fg == theme.yellow && x < 62)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect();
    assert_eq!(lit, "garden", "{text:?}");
}

#[test]
fn a_header_row_holds_the_machines_name_after_what_it_draws() {
    let world = five_and_two("idle", true);
    let session = world
        .spaces
        .iter()
        .position(|s| s.session_name == "garden")
        .unwrap();
    let text = crate::tui::tree::row_text(&world, &[], &Row::Session(session), true);
    assert!(text.starts_with("garden"), "{text:?}");
    assert!(text.ends_with("studio"), "{text:?}");
    // This machine's own header is as it was.
    let local = crate::tui::tree::row_text(&world, &[], &Row::Session(0), true);
    assert!(!local.contains("studio"));
}

// ----------------------------------------------------------------- the preview's words

/// The top line of the preview panel: its title.
fn preview_title(buf: &Buffer) -> String {
    let y = (0..buf.area().height)
        .find(|&y| line(buf, y).contains("\u{256d}") && line(buf, y).contains(" preview"))
        .expect("a preview panel is drawn");
    line(buf, y)
}

#[test]
fn the_previews_title_says_how_old_a_capture_is_once_it_is_over_two_seconds() {
    use std::time::Duration;
    let cfg = depth_all();
    let titled = |age: Option<Duration>| {
        let mut app = app_over(&cfg, five_and_two("idle", true));
        app.sel = row_of(&app, "garden");
        app.preview_age = age;
        preview_title(&draw(&mut app))
    };
    let five = titled(Some(Duration::from_secs(5)));
    assert!(
        five.contains(" preview \u{b7} /home/pi/garden (5s ago) "),
        "{five:?}"
    );
    assert!(titled(Some(Duration::from_secs(125))).contains("(2m ago)"));
    // A fresh capture, one of this machine's, and one two seconds old say nothing.
    assert!(!titled(None).contains("ago"));
    assert!(!titled(Some(Duration::from_secs(2))).contains("ago"));
    assert!(titled(Some(Duration::from_millis(2001))).contains("(2s ago)"));
}

#[test]
fn a_capture_set_by_set_preview_is_fresh_until_its_capturer_says_otherwise() {
    use std::time::Duration;
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.preview_age = Some(Duration::from_secs(30));
    app.set_preview(Some("%1".into()), None);
    assert_eq!(app.preview_age, None);
}

#[test]
fn the_preview_of_a_row_of_a_machine_that_is_away_says_it_is_not_connected() {
    use ratatui::text::Text;
    let cfg = depth_all();
    let shown = |live: bool, session: &str| {
        let mut app = app_over(&cfg, five_and_two("idle", live));
        app.sel = row_of(&app, session);
        // A capture taken before the machine dropped is not shown over its words.
        app.preview = Some(Text::from("stale capture"));
        let buf = draw(&mut app);
        (0..buf.area().height)
            .map(|y| line(&buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let away = shown(false, "homelab");
    assert!(away.contains("no preview: pi is not connected"), "{away}");
    assert!(!away.contains("stale capture"));
    // The positive half: pi's row while pi is live shows the capture, and studio's row the capture
    // while pi is away.
    let live = shown(true, "homelab");
    assert!(live.contains("stale capture") && !live.contains("is not connected"));
    let other = shown(false, "garden");
    assert!(other.contains("stale capture") && !other.contains("is not connected"));
}

#[test]
fn a_row_of_a_machine_that_is_away_has_no_capture_age_in_its_title() {
    use std::time::Duration;
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", false));
    app.sel = row_of(&app, "homelab");
    app.preview_age = Some(Duration::from_secs(40));
    let buf = draw(&mut app);
    assert!(
        preview_title(&buf).contains("/home/pi/garden"),
        "{:?}",
        preview_title(&buf)
    );
    assert!(!preview_title(&buf).contains("ago"));
}

// ----------------------------------------------------------------- the bar's machines part

/// A world of this machine and one machine called `name` per entry, live or not.
fn world_of(machines: &[(&str, bool)]) -> Snapshot {
    let views: Vec<MachineView> = machines
        .iter()
        .map(|(name, live)| {
            let mut view = view_with(name, Some(small_state()));
            view.live = *live;
            view
        })
        .collect();
    merge(snapshot(), &views, NOW)
}

#[test]
fn the_bar_counts_this_machine_among_the_machines_and_names_those_that_are_offline() {
    // The bar: this Mac, studio and pi, which dropped.
    assert_eq!(
        machines_part(&world_of(&[("studio", true), ("pi", false)])),
        "3 machines, pi offline"
    );
    assert_eq!(machines_part(&spec_world()), "3 machines, pi offline");
}

#[test]
fn the_bar_says_only_how_many_machines_when_none_is_offline() {
    assert_eq!(
        machines_part(&world_of(&[("studio", true), ("pi", true)])),
        "3 machines"
    );
    assert_eq!(machines_part(&world_of(&[("studio", true)])), "2 machines");
}

#[test]
fn the_bar_joins_the_names_of_machines_that_are_offline_in_the_configs_order() {
    assert_eq!(
        machines_part(&world_of(&[
            ("studio", false),
            ("nas", true),
            ("pi", false)
        ])),
        "4 machines, studio, pi offline"
    );
}

#[test]
fn a_machine_that_has_not_answered_yet_is_offline_too() {
    let mut view = view_with("studio", None);
    view.live = false;
    assert_eq!(
        machines_part(&merge(snapshot(), &[view], NOW)),
        "2 machines, studio offline"
    );
}

/// This Mac's own tmux server, listed among the machines as a view that is `this_server`, `live` or not.
fn itself(live: bool) -> MachineView {
    let mut view = view_with("this-mac", None);
    view.this_server = true;
    view.live = live;
    view
}

#[test]
fn the_bar_does_not_count_the_view_that_is_this_machines_own_server_nor_name_it_offline() {
    let studio = view_with("studio", Some(small_state()));
    let mut pi = view_with("pi", Some(small_state()));
    pi.live = false;
    // This Mac, studio and the view of this Mac's own server: two machines.
    assert_eq!(
        machines_part(&merge(snapshot(), &[studio.clone(), itself(true)], NOW)),
        "2 machines"
    );
    // Not live, it is still no machine and is never named: pi is the one offline.
    assert_eq!(
        machines_part(&merge(snapshot(), &[studio, itself(false), pi], NOW)),
        "3 machines, pi offline"
    );
}

#[test]
fn a_bar_over_this_machine_and_the_view_of_its_own_server_says_nothing_of_machines() {
    assert_eq!(machines_part(&merge(snapshot(), &[itself(false)], NOW)), "");
}

#[test]
fn the_bar_says_nothing_of_machines_when_no_machine_is_listed() {
    assert_eq!(machines_part(&snapshot()), "");
    assert_eq!(machines_part(&merge(snapshot(), &[], NOW)), "");
}

// ----------------------------------------------------------------- a machine's closed worktrees

/// A closed worktree of the machine whose session `garden` (`$3`, workspace `w3`) is at
/// `/home/pi/garden`: `feat/rain`, workspace `w7`.
fn rain() -> WireWorktree {
    WireWorktree {
        workspace_id: "w7".into(),
        session_name: "garden--feat-rain".into(),
        path: "/home/pi/garden--feat-rain".into(),
        branch: "feat/rain".into(),
        base: Some("main".into()),
        repo_root: "/home/pi/garden".into(),
        source_workspace_id: "w3".into(),
        created_unix_ms: 1234,
        real_path: None,
        real_repo_root: None,
    }
}

fn with_rain(mut state: State) -> State {
    state.worktrees.push(rain());
    state
}

type World = (Snapshot, Vec<WorktreeRecord>);

/// This machine, with its own closed worktree `feat/pdf`, and studio (`garden` with its closed
/// worktree `feat/rain`, and `photos`), live or away since two minutes.
fn rain_world(studio_live: bool) -> World {
    let mut studio = view_with("studio", Some(with_rain(photos_state())));
    studio.live = studio_live;
    if !studio_live {
        studio.words = "not answering".to_string();
        studio.last_seen = Some(NOW - 125_000);
    }
    let views = [studio];
    (
        merge(snapshot(), &views, NOW),
        merge_worktrees(worktrees(), &views),
    )
}

fn app_with<'a>(cfg: &'a Config, (snap, records): World) -> App<'a> {
    let mut app = app_over(cfg, snap);
    app.set_worktrees(records);
    app
}

/// The row of the closed worktree on `branch`.
fn worktree_row(app: &App, branch: &str) -> usize {
    app.rows
        .rows
        .iter()
        .position(|row| {
            matches!(row, Row::ClosedWorktree { record, .. } if app.worktrees[*record].branch == branch)
        })
        .unwrap_or_else(|| panic!("no row for the worktree {branch}"))
}

fn has_worktree_row(app: &App, branch: &str) -> bool {
    app.rows.rows.iter().any(|row| {
        matches!(row, Row::ClosedWorktree { record, .. } if app.worktrees[*record].branch == branch)
    })
}

#[test]
fn a_machines_closed_worktree_is_a_record_with_its_machine_and_a_namespaced_id() {
    let views = [
        view_with("studio", Some(with_rain(small_state()))),
        view_with("pi", Some(with_rain(small_state()))),
    ];
    let records = merge_worktrees(worktrees(), &views);
    assert_eq!(records.len(), 3);
    // This machine's own come first, as they were.
    assert_eq!(records[0].workspace_id, "w6");
    assert_eq!(records[0].machine, None);
    for (record, name, machine) in [(&records[1], "studio", 0), (&records[2], "pi", 1)] {
        assert_eq!(record.machine, Some(machine));
        assert_eq!(record.workspace_id, format!("{name}:w7"));
        assert_eq!(
            crate::machine::split_id(&record.workspace_id),
            (Some(name), "w7")
        );
        assert_eq!(record.session_name, "garden--feat-rain");
        assert_eq!(record.branch, "feat/rain");
        assert_eq!(record.base.as_deref(), Some("main"));
        assert_eq!(record.path, PathBuf::from("/home/pi/garden--feat-rain"));
        assert_eq!(record.repo_root, PathBuf::from("/home/pi/garden"));
        assert_eq!(record.source_workspace_id, "w3");
        assert_eq!(record.created_unix_ms, 1234);
    }
}

#[test]
fn a_record_turned_into_wire_and_back_is_the_same_but_for_its_id_and_machine() {
    let mut local = worktrees();
    local[0].base = None;
    local.push(worktrees().remove(0));
    for was in &local {
        let mut state = State::default();
        state.worktrees.push(WireWorktree::from(was));
        let back = merge_worktrees(Vec::new(), &[view_with("studio", Some(state))]).remove(0);
        assert_eq!(back.workspace_id, format!("studio:{}", was.workspace_id));
        assert_eq!(back.machine, Some(0));
        assert_eq!(back.session_name, was.session_name);
        assert_eq!(back.path, was.path);
        assert_eq!(back.branch, was.branch);
        assert_eq!(back.base, was.base);
        assert_eq!(back.repo_root, was.repo_root);
        assert_eq!(back.source_workspace_id, was.source_workspace_id);
        assert_eq!(back.created_unix_ms, was.created_unix_ms);
    }
}

#[test]
fn a_view_with_no_state_or_that_is_this_tmux_server_adds_no_worktree() {
    let mut here = view_with("here", Some(with_rain(small_state())));
    here.this_server = true;
    let views = [
        view_with("silent", None),
        here,
        view_with("studio", Some(with_rain(small_state()))),
    ];
    let records = merge_worktrees(Vec::new(), &views);
    // The positive half: the one machine that can has its worktree, under its own index.
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].machine, Some(2));
    assert_eq!(records[0].workspace_id, "studio:w7");
}

#[test]
fn with_no_machines_the_records_come_back_as_they_were() {
    let records = merge_worktrees(worktrees(), &[]);
    let was = worktrees();
    assert_eq!(records.len(), was.len());
    assert_eq!(records[0].workspace_id, was[0].workspace_id);
    assert_eq!(records[0].branch, was[0].branch);
    assert_eq!(records[0].machine, None);
}

#[test]
fn a_remote_closed_worktree_is_drawn_as_this_machines_under_its_own_session() {
    let cfg = depth_all();
    let mut app = app_with(&cfg, rain_world(true));
    let at = worktree_row(&app, "feat/rain");
    // Under studio's garden: the rows between that session's header and it hold no other header.
    let Row::ClosedWorktree { space, .. } = app.rows.rows[at] else {
        unreachable!()
    };
    assert_eq!(app.snap.spaces[space].session_name, "garden");
    assert_eq!(app.snap.spaces[space].machine, Some(0));
    let header = app
        .rows
        .rows
        .iter()
        .position(|row| *row == Row::Session(space))
        .unwrap();
    assert!(header < at);
    assert!(
        app.rows.rows[header + 1..at]
            .iter()
            .all(|row| matches!(row, Row::Pane(_)))
    );
    // The same line as this machine's own parked worktree: glyph, colour and words.
    let buf = draw(&mut app);
    let (remote, local) = (find(&buf, "feat/rain"), find(&buf, "feat/pdf"));
    assert!(line(&buf, remote).contains("worktree, closed"));
    let glyph = |y: u16| -> (u16, ratatui::style::Color) {
        let x = (0..60)
            .find(|&x| buf[(x, y)].symbol() == "\u{25cc}")
            .unwrap();
        (x, buf[(x, y)].fg)
    };
    assert_eq!(glyph(remote), glyph(local));
}

#[test]
fn a_remote_closed_worktree_hangs_under_its_own_machines_session_and_never_another() {
    // studio and pi each have a `garden` at one path with one workspace id, and this machine has a
    // session at that path with that id too: the worktree is pi's, so it hangs under pi's.
    for source in ["w3", "w99"] {
        // This machine's own `$3` is sitting at that path too, ahead of both machines' rows.
        let mut here = snapshot();
        let ours = here
            .spaces
            .iter()
            .position(|s| s.session_id == "$3")
            .unwrap();
        here.spaces[ours].session_path = PathBuf::from("/home/pi/garden");
        let mut state = with_rain(small_state());
        state.worktrees[0].source_workspace_id = source.into();
        let views = [
            view_with("studio", Some(small_state())),
            view_with("pi", Some(state)),
        ];
        let world = (
            merge(here, &views, NOW),
            merge_worktrees(worktrees(), &views),
        );
        let cfg = depth_all();
        let app = app_with(&cfg, world);
        let at = worktree_row(&app, "feat/rain");
        let Row::ClosedWorktree { space, .. } = app.rows.rows[at] else {
            unreachable!()
        };
        assert_eq!(app.snap.spaces[space].machine, Some(1), "source {source}");
        assert_eq!(
            app.snap.spaces[space].session_id, "pi:$3",
            "source {source}"
        );
    }
}

#[test]
fn a_remote_worktree_whose_checkout_has_a_session_on_its_machine_is_open_not_closed() {
    let mut state = with_rain(photos_state());
    state.spaces[1].session_path = "/home/pi/garden--feat-rain".into();
    let cfg = depth_all();
    let app = app_with(&cfg, {
        let views = [view_with("studio", Some(state))];
        (
            merge(snapshot(), &views, NOW),
            merge_worktrees(worktrees(), &views),
        )
    });
    assert!(!has_worktree_row(&app, "feat/rain"));
    // The positive half: this machine's own, and the same state without that session.
    assert!(has_worktree_row(&app, "feat/pdf"));
    let app = app_with(&cfg, rain_world(true));
    assert!(has_worktree_row(&app, "feat/rain"));
}

#[test]
fn a_session_of_this_machine_at_a_remote_checkouts_path_does_not_open_it() {
    let mut here = snapshot();
    here.spaces.push(crate::tui::fixtures::space(
        8,
        "/home/pi/garden--feat-rain",
        "main",
        (0, 0, 0, 0),
    ));
    let views = [view_with("studio", Some(with_rain(small_state())))];
    let cfg = depth_all();
    let app = app_with(
        &cfg,
        (
            merge(here, &views, NOW),
            merge_worktrees(worktrees(), &views),
        ),
    );
    assert!(has_worktree_row(&app, "feat/rain"));
}

#[test]
fn a_remote_worktree_with_no_session_on_its_machine_is_drawn_nowhere() {
    // This machine has a session at the repository, and the machine does not.
    let mut here = snapshot();
    here.spaces.push(crate::tui::fixtures::space(
        8,
        "/nowhere",
        "main",
        (0, 0, 0, 0),
    ));
    let mut state = with_rain(small_state());
    state.worktrees[0].repo_root = "/nowhere".into();
    state.worktrees[0].source_workspace_id = "w99".into();
    let views = [view_with("studio", Some(state))];
    let cfg = depth_all();
    let app = app_with(
        &cfg,
        (
            merge(here, &views, NOW),
            merge_worktrees(worktrees(), &views),
        ),
    );
    assert!(!has_worktree_row(&app, "feat/rain"));
    assert!(
        has_worktree_row(&app, "feat/pdf"),
        "this machine's still is"
    );
}

#[test]
fn a_closed_worktree_of_a_machine_that_is_away_is_dim_and_counted_nowhere() {
    let cfg = depth_all();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    // The cursor is in garden, so the letters of its rows are lit keys, and the worktree's colour
    // shows where the row is not dim by itself: its letter and its tree line.
    let lit = |live: bool| -> Vec<ratatui::style::Color> {
        let mut app = app_with(&cfg, rain_world(live));
        app.sel = row_of(&app, "garden");
        let buf = draw(&mut app);
        let y = find(&buf, "feat/rain");
        (1..60)
            .filter(|&x| buf[(x, y)].symbol() != " ")
            .map(|x| buf[(x, y)].fg)
            .filter(|fg| *fg != theme.dim)
            .collect()
    };
    assert!(
        !lit(true).is_empty(),
        "a live machine's worktree row has a lit key"
    );
    assert!(lit(false).is_empty(), "an away machine's is dim whole");
    let mut app = app_with(&cfg, rain_world(false));
    let at = worktree_row(&app, "feat/rain");
    let buf = draw(&mut app);
    let y = find(&buf, "feat/rain");
    let cells: Vec<_> = (2..60)
        .filter(|&x| buf[(x, y)].symbol() != " ")
        .map(|x| buf[(x, y)].fg)
        .collect();
    assert!(!cells.is_empty() && cells.iter().all(|fg| *fg == theme.dim));
    // The bar and the walk are what they are without it.
    let without = {
        let mut studio = view_with("studio", Some(photos_state()));
        studio.live = false;
        let mut bare = app_over(&cfg, merge(snapshot(), &[studio], NOW));
        bare.set_worktrees(worktrees());
        bare
    };
    let (counts, bare) = (app.bar_counts(), without.bar_counts());
    assert_eq!(counts.sessions, bare.sessions);
    assert_eq!(counts.agents, bare.agents);
    assert_eq!(counts.needs, bare.needs);
    assert!(!app.next_up_rows().contains(&at));
    // Positive half: the walk does reach this machine's agents.
    assert!(!app.next_up_rows().is_empty());
}

#[test]
fn a_closed_worktree_of_a_live_machine_is_no_stop_of_the_walk_either() {
    let cfg = depth_all();
    let app = app_with(&cfg, rain_world(true));
    let at = worktree_row(&app, "feat/rain");
    assert!(!app.next_up_rows().contains(&at));
    assert!(
        app.rows
            .rows
            .iter()
            .any(|row| matches!(row, Row::Pane(pane) if app.snap.panes[*pane].machine == Some(0))),
        "studio's own panes are rows"
    );
    // No number names it: numbers are sessions', and its letter is a child's.
    assert_eq!(app.label_index(at), None);
}

#[test]
fn enter_x_r_and_w_on_a_remote_closed_worktree_answer_as_on_any_row_of_its_machine() {
    let cfg = depth_all();
    for key in [
        KeyCode::Enter,
        KeyCode::Char('x'),
        KeyCode::Char('r'),
        KeyCode::Char('w'),
    ] {
        let mut live = app_with(&cfg, rain_world(true));
        live.sel = worktree_row(&live, "feat/rain");
        let action = live.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(action.is_none(), "{key:?}");
        // Enter is answered without "Enter goes there"; the other keys keep today's words.
        let said = if key == KeyCode::Enter {
            words::acted_on_from_the_app("studio")
        } else {
            words::read_only("studio")
        };
        assert_eq!(toast_of(&live), Some(said), "{key:?}");
        assert!(!live.overlay.is_open(), "{key:?}");
        let mut away = app_with(&cfg, rain_world(false));
        away.sel = worktree_row(&away, "feat/rain");
        let action = away.on_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(action.is_none(), "{key:?}");
        assert_eq!(
            toast_of(&away).as_deref(),
            Some("studio is not connected (last seen 2 min ago)"),
            "{key:?}"
        );
        assert!(!away.overlay.is_open(), "{key:?}");
    }
}

#[test]
fn this_machines_closed_worktree_still_opens_with_enter() {
    let cfg = depth_all();
    let mut app = app_with(&cfg, rain_world(true));
    app.sel = worktree_row(&app, "feat/pdf");
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        action,
        Some(crate::tui::actions::Action::OpenWorktree { ref workspace }) if workspace == "w6"
    ));
    assert_eq!(toast_of(&app), None);
}

#[test]
fn the_filter_finds_a_remote_closed_worktree_by_its_name_and_its_machines() {
    for (layout, cfg) in [("tree", depth_all()), ("flat", flat_config())] {
        for (query, listed) in [
            ("rain", true),
            ("studio rain", true),
            ("garden rain", true),
            ("pi rain", false),
            ("pdf", false),
        ] {
            let mut app = app_with(&cfg, rain_world(true));
            app.filter = Some(Input::new(query));
            app.rebuild();
            assert_eq!(
                has_worktree_row(&app, "feat/rain"),
                listed,
                "{layout}: /{query}"
            );
        }
        // The positive half for the rows that are left out: this machine's is found by its name.
        let mut app = app_with(&cfg, rain_world(true));
        app.filter = Some(Input::new("pdf"));
        app.rebuild();
        assert!(has_worktree_row(&app, "feat/pdf"), "{layout}");
    }
}

#[test]
fn the_flat_list_puts_a_machines_closed_worktree_after_this_machines_and_a_machine_that_is_away_last()
 {
    let cfg = flat_config();
    let live = app_with(&cfg, rain_world(true));
    let (pdf, rain) = (
        worktree_row(&live, "feat/pdf"),
        worktree_row(&live, "feat/rain"),
    );
    // Machine-major: this machine's panes, then its worktree, then studio's panes, then its worktree.
    let last_pane = live
        .rows
        .rows
        .iter()
        .rposition(|row| matches!(row, Row::Pane(pane) if live.snap.panes[*pane].machine.is_none()))
        .unwrap();
    let first_remote_pane = live
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(pane) if live.snap.panes[*pane].machine.is_some()))
        .unwrap();
    assert!(
        last_pane < pdf && pdf < first_remote_pane && first_remote_pane < rain,
        "{last_pane} {pdf} {first_remote_pane} {rain}"
    );
    // Away: studio's panes and its worktree go after this machine's, its worktree last.
    let away = app_with(&cfg, rain_world(false));
    let (pdf, rain) = (
        worktree_row(&away, "feat/pdf"),
        worktree_row(&away, "feat/rain"),
    );
    assert_eq!(rain, away.rows.rows.len() - 1);
    let first_studio = away
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Pane(pane) if away.snap.panes[*pane].machine == Some(0)))
        .unwrap();
    assert!(
        pdf < first_studio && first_studio < rain,
        "{pdf} {first_studio} {rain}"
    );
}

#[test]
fn a_flat_line_of_a_remote_closed_worktree_names_its_session_with_the_machine() {
    let cfg = flat_config();
    let mut app = app_with(&cfg, rain_world(true));
    let buf = draw(&mut app);
    let text = line(&buf, find(&buf, "feat/rain"));
    assert!(text.contains("studio \u{b7} garden"), "{text:?}");
    // This machine's own says its session alone.
    let local = line(&buf, find(&buf, "feat/pdf"));
    assert!(!local.chars().take(62).any(|c| c == '\u{b7}'), "{local:?}");
}

#[test]
fn one_machine_with_a_closed_worktree_does_change_the_frame_and_draws_it() {
    // The positive half of the frame proof: the worktree is a row that was not there.
    let (snap, records) = rain_world(true);
    let without = {
        let views = [view_with("studio", Some(photos_state()))];
        frame_with(
            merge(snapshot(), &views, NOW),
            merge_worktrees(worktrees(), &views),
            Depth::All,
            Layout::Tree,
            None,
        )
    };
    let with = frame_with(snap, records, Depth::All, Layout::Tree, None);
    assert!(first_difference(&without, &with).is_some());
    assert!(line(&with, find(&with, "feat/rain")).contains("worktree, closed"));
}

#[test]
fn a_remote_path_is_compared_as_its_machine_wrote_it() {
    // A limitation, accepted: a remote path is compared as written, never resolved, because
    // its files are not here. On a Mac `/tmp` and `/private/tmp` are one directory, and here they
    // count as two: a session in one does not open a worktree in the other, so that worktree is
    // drawn as closed. The feed is not changed to make them one.
    let mut state = with_rain(photos_state());
    state.spaces[1].session_path = "/tmp".into();
    state.worktrees[0].path = "/private/tmp".into();
    let views = [view_with("studio", Some(state))];
    let cfg = depth_all();
    let app = app_with(
        &cfg,
        (
            merge(snapshot(), &views, NOW),
            merge_worktrees(worktrees(), &views),
        ),
    );
    assert!(has_worktree_row(&app, "feat/rain"));
}

// -------------------------------------- no machines: nothing is drawn differently
//
// The frame test near the top compares `merge(world, &[])` with `world`, and both sides run the same
// drawing code. These checks can fail on a change to that code: with `machines` empty no header is
// ever built or drawn, no text holds the middle dot, and the numbers are what a hand count of the
// world says. The popup's own tests, which this batch changed only in literals
// (`git diff ec5d8ae -- src/tui/app/tests.rs src/tui/fixtures.rs`), are the base proof.

use crate::config::AgentPanelSort;
use crate::tui::tree;

/// A session with no panes, the way a tmux session in a fresh directory is.
fn bare_sessions(snap: &mut Snapshot, from: u32, to: u32) {
    for n in from..=to {
        snap.spaces
            .push(space(n, &format!("~/code/extra{n}"), "main", (0, 0, 0, 0)));
    }
}

/// The sample world with `json` installed as its Agents view.
fn with_view(json: &str) -> Snapshot {
    let mut snap = snapshot();
    snap.view = crate::tui::agent_view::parse(Some(json)).expect("a query taw would install");
    assert!(snap.view.is_some(), "{json} should install a view");
    snap
}

/// A world the popup knew before machines, with the numbers a hand count of it gives: sessions,
/// agents, and the agents that need you by state.
struct Known {
    what: &'static str,
    snap: Snapshot,
    sessions: usize,
    agents: usize,
    needs: Vec<(AgentState, usize)>,
}

fn known_worlds() -> Vec<Known> {
    // The sample: five sessions, seven agents; only infra's blocked claude and notes' unseen codex
    // (web's `%231`) need you, the three working and two idle ones do not.
    let needs = vec![(AgentState::Blocked, 1), (AgentState::Done, 1)];
    let mut boxed = snapshot();
    sandboxed(&mut boxed);
    let mut many = snapshot();
    bare_sessions(&mut many, 6, 11);
    let view =
        with_view(r#"{"source":"cli","filter":{"op":"eq","field":"status","value":"blocked"}}"#);
    let sorted = with_view(r#"{"source":"cli","sort":[{"field":"agent","order":"asc"}]}"#);
    let known = |what, snap: Snapshot, sessions, agents, needs: &[(AgentState, usize)]| Known {
        what,
        snap,
        sessions,
        agents,
        needs: needs.to_vec(),
    };
    vec![
        known("the sample world", snapshot(), 5, 7, &needs),
        known("the sample in sandboxes", boxed, 5, 7, &needs),
        known("eleven sessions", many, 11, 7, &needs),
        known("an Agents view that filters", view, 5, 7, &needs),
        known("an Agents view that sorts", sorted, 5, 7, &needs),
        known("an empty world", Snapshot::default(), 0, 0, &[]),
    ]
}

/// Every cell of `buf`, as text.
fn all_text(buf: &Buffer) -> String {
    (0..buf.area().height)
        .map(|y| line(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rows a tree of `snap` at `depth` lists with nothing typed and no machine, counted by hand
/// from the world: each session, its panes in order (the agents only, below the deepest depth),
/// then web's parked worktree, the one record the sample has.
fn hand_counted_tree(snap: &Snapshot, depth: Depth) -> Vec<Row> {
    let mut rows = Vec::new();
    for (at, space) in snap.spaces.iter().enumerate() {
        rows.push(Row::Session(at));
        if depth == Depth::Sessions {
            continue;
        }
        for (pane, p) in snap.panes.iter().enumerate() {
            if p.session_id == space.session_id && (depth == Depth::All || p.agent.is_some()) {
                rows.push(Row::Pane(pane));
            }
        }
        if space.session_name == "web" {
            rows.push(Row::ClosedWorktree {
                space: at,
                record: 0,
            });
        }
    }
    rows
}

#[test]
fn with_no_machines_no_header_no_dot_and_the_numbers_a_hand_count_gives() {
    let mut checked = 0;
    for world in known_worlds() {
        for depth in [Depth::Sessions, Depth::Agents, Depth::All] {
            for layout in [Layout::Tree, Layout::Flat] {
                for sort in [AgentPanelSort::Attention, AgentPanelSort::Sessions] {
                    for query in [None, Some("claude"), Some("web"), Some("a")] {
                        let at =
                            format!("{} at {depth:?} {layout:?} {sort:?} {query:?}", world.what);
                        let mut cfg = Config::default();
                        cfg.ui.depth = depth;
                        cfg.ui.layout = layout;
                        cfg.ui.agent_panel_sort = sort;
                        let mut app = app_over(&cfg, world.snap.clone());
                        app.set_worktrees(worktrees());
                        if let Some(query) = query {
                            app.filter = Some(Input::new(query));
                            app.rebuild();
                            app.settle_cursor();
                        }
                        // No header row, no machine in any text a row or a frame holds.
                        assert!(
                            app.rows
                                .rows
                                .iter()
                                .all(|row| !matches!(row, Row::Machine(_))),
                            "{at}: a header row"
                        );
                        for row in &app.rows.rows {
                            let text = tree::row_text(&app.snap, &app.worktrees, row, true);
                            assert!(!text.contains('\u{b7}'), "{at}: {text:?} has the dot");
                        }
                        for space in 0..app.snap.spaces.len() {
                            let name = tree::space_name(&app.snap, space);
                            assert_eq!(name, app.snap.spaces[space].session_name, "{at}");
                        }
                        let text = all_text(&draw(&mut app));
                        // The base draws the dot as a separator and a glyph: what it never draws is
                        // one in front of a session's name.
                        for space in &app.snap.spaces {
                            let named = format!("\u{b7} {}", space.session_name);
                            assert!(!text.contains(&named), "{at}: the frame has {named:?}");
                        }
                        assert!(
                            !text.contains(this_machine_word()),
                            "{at}: a machine header"
                        );
                        // The numbers: what a hand count of the world says.
                        let bar = app.bar_counts();
                        assert_eq!(bar.sessions, world.sessions, "{at}");
                        assert_eq!(bar.agents, world.agents, "{at}");
                        assert_eq!(bar.needs, world.needs, "{at}");
                        if query.is_none() {
                            let flat = layout == Layout::Flat && depth != Depth::Sessions;
                            let mut numbered = 0;
                            for (index, row) in app.rows.rows.iter().enumerate() {
                                let expected = if flat {
                                    // The first nine rows that are not parked worktrees.
                                    if matches!(row, Row::ClosedWorktree { .. }) {
                                        None
                                    } else {
                                        numbered += 1;
                                        (numbered <= 9).then_some(numbered as u8)
                                    }
                                } else {
                                    match row {
                                        Row::Session(space) => {
                                            (*space < 9).then_some(*space as u8 + 1)
                                        }
                                        _ => None,
                                    }
                                };
                                assert_eq!(app.label_index(index), expected, "{at}: row {index}");
                            }
                            if layout == Layout::Tree && world.snap.view.is_none() {
                                assert_eq!(
                                    app.rows.rows,
                                    hand_counted_tree(&world.snap, depth),
                                    "{at}"
                                );
                            }
                        }
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 6 * 3 * 2 * 2 * 4);
}

#[test]
fn the_scan_for_a_header_and_a_dot_can_fail_with_a_machine() {
    // The positive half: the same scan finds both in a world that has a machine.
    let cfg = depth_all();
    let views = [view_with("studio", Some(small_state()))];
    let mut app = app_over(&cfg, merge(snapshot(), &views, NOW));
    assert!(
        app.rows
            .rows
            .iter()
            .any(|row| matches!(row, Row::Machine(_)))
    );
    let text = all_text(&draw(&mut app));
    assert!(text.contains(this_machine_word()));
    let mut flat = Config::default();
    flat.ui.depth = Depth::All;
    flat.ui.layout = Layout::Flat;
    let mut app = app_over(&flat, merge(snapshot(), &views, NOW));
    assert!(all_text(&draw(&mut app)).contains("studio \u{b7} garden"));
}

#[test]
fn a_machine_appearing_gives_this_machines_rows_the_labels_they_had_and_its_header_none() {
    let cfg = depth_all();
    let before = app_over(&cfg, snapshot());
    let views = [view_with("studio", Some(small_state()))];
    let after = app_over(&cfg, merge(snapshot(), &views, NOW));
    for (at, row) in before.rows.rows.iter().enumerate() {
        let here = after
            .rows
            .rows
            .iter()
            .position(|other| other == row)
            .expect("this machine's row is still there");
        assert_eq!(after.label_index(here), before.label_index(at));
    }
    for (at, row) in after.rows.rows.iter().enumerate() {
        if matches!(row, Row::Machine(_)) {
            assert_eq!(after.label_index(at), None, "a header wears no number");
        }
    }
    // The numbers run on: studio's session is the sixth.
    let studio = after
        .rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::Session(5)))
        .expect("studio's session");
    assert_eq!(after.label_index(studio), Some(6));
}

// ------------------------------------- the filter's light on the rows of a machine that is away

/// This machine's five sessions, studio (garden and photos) away since two minutes, and pi live.
fn studio_away() -> Snapshot {
    let mut studio = view_with("studio", Some(photos_state()));
    studio.live = false;
    studio.words = "not answering".to_string();
    studio.last_seen = Some(NOW - 125_000);
    merge(
        snapshot(),
        &[studio, view_with("pi", Some(homelab_state()))],
        NOW,
    )
}

#[test]
fn a_greyed_row_keeps_the_filters_light_on_the_matched_characters_in_the_dim_colour() {
    use ratatui::style::Modifier;
    for (layout, cfg, needle) in [
        ("flat", flat_config(), "studio \u{b7} garden"),
        ("tree", depth_all(), "garden"),
    ] {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = app_over(&cfg, studio_away());
        app.filter = Some(Input::new("garden"));
        app.rebuild();
        app.settle_cursor();
        // A positive half: the filter does list the row.
        let buf = draw(&mut app);
        // The first line of the tree (the preview, beside it, names garden's folder too).
        let y = (0..buf.area().height)
            .find(|&y| {
                line(&buf, y)
                    .chars()
                    .take(62)
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("{layout}: no line of the tree holds {needle:?}"));
        let text = line(&buf, y);
        let tree_cells = || (2..60u16).filter(|&x| buf[(x, y)].symbol() != " ");
        let lit: String = tree_cells()
            .filter(|&x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect();
        assert_eq!(lit, "garden", "{layout}: {text:?}");
        for x in tree_cells() {
            assert_eq!(buf[(x, y)].fg, theme.dim, "{layout} column {x} of {text:?}");
            let emphasised = buf[(x, y)].modifier.contains(Modifier::BOLD);
            let matched = buf[(x, y)].modifier.contains(Modifier::UNDERLINED);
            assert_eq!(emphasised, matched, "{layout} column {x} of {text:?}");
        }
    }
}

/// The worktrees a machine's sessions run reach the snapshot with the machine's index and its own ids, and
/// never the list of closed worktrees (the rows).
#[test]
fn a_machines_open_worktrees_are_kept_on_the_snapshot_and_are_no_closed_worktrees() {
    let mut state = photos_state();
    let mut open = rain();
    open.workspace_id = "w9".into();
    state.open_worktrees.push(open);
    let views = [view_with("studio", Some(state))];
    let snap = merge(snapshot(), &views, NOW);
    assert_eq!(snap.open_worktrees.len(), 1);
    assert_eq!(snap.open_worktrees[0].workspace_id, "studio:w9");
    assert_eq!(snap.open_worktrees[0].machine, Some(0));
    assert!(
        merge_worktrees(worktrees(), &views)
            .iter()
            .all(|r| r.workspace_id != "studio:w9")
    );
    assert!(merge(snapshot(), &[], NOW).open_worktrees.is_empty());
}

// ------------------------------------ Enter on another machine's row, in words

/// What the popup's hint line says, as one line, with the cursor on `session`'s row.
fn hint_on(app: &mut App, session: &str) -> String {
    app.sel = row_of(app, session);
    view::hint_text(app)
}

#[test]
fn in_the_popup_the_hint_says_enter_goes_to_the_machine_under_the_cursor_and_how_q_brings_you_back()
{
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", true));
    let studio = hint_on(&mut app, "garden");
    assert!(
        studio.contains("Enter go to studio (Q there brings you back)"),
        "{studio}"
    );
    let pi = hint_on(&mut app, "homelab");
    assert!(
        pi.contains("Enter go to pi (Q there brings you back)"),
        "{pi}"
    );
    // This machine's own rows keep the plain words.
    let own = app
        .snap
        .spaces
        .iter()
        .find(|space| space.machine.is_none())
        .map(|space| space.session_name.clone())
        .expect("a row of this machine");
    let own = hint_on(&mut app, &own);
    assert!(own.contains("Enter go  \u{b7}"), "{own}");
    assert!(!own.contains("go to"), "{own}");
}

#[test]
fn the_hint_names_no_trip_for_a_row_of_a_machine_that_is_away_or_in_the_app() {
    let cfg = depth_all();
    // Enter on an away machine's row answers that it is not connected: it goes nowhere.
    let mut away = app_over(&cfg, five_and_two("idle", false));
    let said = hint_on(&mut away, "homelab");
    // A positive half first: the line is the popup's own hint line.
    assert!(said.contains("Enter go"), "{said}");
    assert!(!said.contains("go to pi"), "{said}");
    // In the app Enter is the same on every row: it goes in.
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.enter_app_mode(None, None);
    let said = hint_on(&mut app, "garden");
    assert!(said.contains("Enter go in"), "{said}");
    assert!(!said.contains("Q there"), "{said}");
}

/// The popup's frame at `width` columns, as lines.
fn frame_at(app: &mut App, width: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, 39)).expect("a test terminal");
    terminal
        .draw(|f| view::draw(f, app))
        .expect("drawing must not fail");
    let buf = terminal.backend().buffer().clone();
    (0..buf.area().height).map(|y| line(&buf, y)).collect()
}

#[test]
fn the_enter_words_are_cut_by_width_before_any_hint_of_the_line_is_dropped() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", true));
    app.sel = row_of(&app, "garden");
    let bottom = |app: &mut App, width: u16| {
        let frame = frame_at(app, width);
        frame[frame.len() - 2..].join("\n")
    };
    // Room for all of it: the machine and how Q brings you back, and the line's last hint.
    let wide = bottom(&mut app, 120);
    for needle in ["Enter go to studio (Q there brings you back)", "? help"] {
        assert!(wide.contains(needle), "{needle:?} in\n{wide}");
    }
    // Two lines of 80 hold every hint with the machine's name, not with the rest.
    let narrow = bottom(&mut app, 80);
    for needle in ["Enter go to studio", "j/k move", "/ filter", "? help"] {
        assert!(narrow.contains(needle), "{needle:?} in\n{narrow}");
    }
    assert!(!narrow.contains("Q there"), "{narrow}");
    // Narrower still, it is what it is on this machine's rows, as the line was before.
    let narrowest = bottom(&mut app, 44);
    assert!(narrowest.contains("Enter go"), "{narrowest}");
    assert!(!narrowest.contains("go to studio"), "{narrowest}");
}

#[test]
fn the_help_says_it_in_a_few_more_words_under_enter_in_the_popup_with_machines_only() {
    let help = crate::machine::words::enter_on_machine_help("Q");
    let help = help.as_str();
    let cfg = depth_all();
    let mut app = app_over(&cfg, five_and_two("idle", true));
    press(&mut app, '?');
    let frame = frame_at(&mut app, 150);
    let at = frame
        .iter()
        .position(|l| l.contains("go to the row under the cursor"))
        .expect("the Enter line of the help");
    assert!(
        frame[at + 1].contains(help),
        "right under Enter:\n{}",
        frame[at..at + 3].join("\n")
    );
    // With no other machine listed the help says nothing of machines.
    let mut alone = app_over(&cfg, snapshot());
    press(&mut alone, '?');
    let frame = frame_at(&mut alone, 150);
    assert!(
        frame.iter().any(|l| l.contains("go to the row under")),
        "a positive half: the help is drawn"
    );
    assert!(
        !frame.iter().any(|l| l.contains(help)),
        "the line is for a list with machines"
    );
    // In the app Enter is the same on every row, and the help says so in its own words.
    let mut inside = app_over(&cfg, five_and_two("idle", true));
    inside.enter_app_mode(None, None);
    press(&mut inside, '?');
    let frame = frame_at(&mut inside, 150);
    assert!(
        frame.iter().any(|l| l.contains("go in to the row")),
        "a positive half: the app's help is drawn"
    );
    assert!(!frame.iter().any(|l| l.contains(help)));
}

/// The trip's words name the key that ends the app there, as `[keys]` leaves it: Q gone to another action (here
/// the options) leaves Ctrl-C, in the hint line and in the help alike, and so does the search preset, whose Q is
/// text for its query.
#[test]
fn the_trip_words_name_ctrl_c_where_q_does_not_end_the_app() {
    let mut rebound = depth_all();
    rebound.keys.bindings.insert(
        "options".to_string(),
        crate::config::KeySpec::One("shift+q".to_string()),
    );
    let mut search = depth_all();
    search.keys.preset = crate::config::KeysPreset::Search;
    // The search preset's `?` is text: the help is opened by a key of its own in both.
    for cfg in [&mut rebound, &mut search] {
        cfg.keys.bindings.insert(
            "help".to_string(),
            crate::config::KeySpec::One("ctrl+g".to_string()),
        );
    }
    for (what, cfg) in [("Q rebound", &rebound), ("search preset", &search)] {
        let mut app = app_over(cfg, five_and_two("idle", true));
        let said = hint_on(&mut app, "garden");
        assert!(
            said.contains("go to studio (Ctrl-C there brings you back)"),
            "{what}: {said}"
        );
        assert!(!said.contains("Q there"), "{what}: {said}");
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('g'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        let frame = frame_at(&mut app, 150);
        assert!(
            frame
                .iter()
                .any(|l| l.contains("hands this terminal to the app; Ctrl-C there brings you back")),
            "{what}:\n{}",
            frame.join("\n")
        );
    }
}

// ------------------------------------------------ a connected machine with no tmux session: the row that starts one

/// The rows of the list over `views` merged into the plain snapshot, as the popup builds them.
fn rows_over(views: &[MachineView]) -> Vec<Row> {
    let cfg = depth_all();
    let app = app_over(&cfg, merge(snapshot(), views, NOW));
    app.rows.rows.clone()
}

#[test]
fn a_connected_machine_with_no_session_has_the_row_that_starts_one_under_its_header() {
    let rows = rows_over(&[view_with("studio", Some(State::default()))]);
    let at = rows
        .iter()
        .position(|row| *row == Row::Machine(Some(0)))
        .expect("studio's header");
    assert_eq!(rows.get(at + 1), Some(&Row::NoSession(0)), "{rows:?}");
    assert_eq!(rows.len(), at + 2, "nothing else under it: {rows:?}");
}

#[test]
fn the_row_is_there_only_for_a_machine_that_is_connected_and_has_said_its_state() {
    let has = |views: &[MachineView]| rows_over(views).contains(&Row::NoSession(0));
    // Positive half first: the same machine, live and stated, has it.
    assert!(has(&[view_with("studio", Some(State::default()))]));
    let mut away = view_with("studio", Some(State::default()));
    away.live = false;
    assert!(!has(&[away]), "a machine that is away has no row");
    assert!(!has(&[view_with("studio", None)]), "no state yet, no row");
    let mut unkept = view_with("studio", Some(State::default()));
    unkept.no_keeper = true;
    assert!(!has(&[unkept]), "nobody keeps it: it is not connected");
    let world = snapshot();
    assert!(
        !has(&[machine_of("studio", &world)]),
        "a machine with sessions has none"
    );
}

#[test]
fn the_cursor_rests_on_the_row_and_enter_in_the_popup_is_the_doorway_on_new() {
    use crate::tui::actions::Action;
    let cfg = depth_all();
    let mut world = snapshot();
    world.spaces.clear();
    world.panes.clear();
    world.agents.clear();
    let mut app = app_over(
        &cfg,
        merge(world, &[view_with("studio", Some(State::default()))], NOW),
    );
    assert_eq!(app.current(), Some(&Row::NoSession(0)));
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        action,
        Some(Action::Doorway {
            machine: "studio".into(),
            id: "new".into()
        })
    );
}

#[test]
fn j_walks_down_to_the_row_past_this_machines_sessions() {
    let cfg = depth_all();
    let mut app = app_over(
        &cfg,
        merge(
            snapshot(),
            &[view_with("studio", Some(State::default()))],
            NOW,
        ),
    );
    let mut seen = Vec::new();
    for _ in 0..40 {
        seen.push(format!("{:?}", app.current()));
        if app.current() == Some(&Row::NoSession(0)) {
            return;
        }
        press(&mut app, 'j');
    }
    panic!("j never reached the row: {seen:?}");
}

// ------------------------------------------------ the row that starts a session takes a number

/// This machine's five sessions, studio's two, `spare` connected with no session, then pi's one: the
/// row under `spare` sits between machines that have sessions.
fn with_a_spare_between() -> Snapshot {
    merge(
        snapshot(),
        &[
            view_with("studio", Some(photos_state())),
            view_with("spare", Some(State::default())),
            view_with("pi", Some(homelab_state())),
        ],
        NOW,
    )
}

/// Where the row under machine `machine` is.
fn no_session_row(app: &App, machine: usize) -> usize {
    app.rows
        .rows
        .iter()
        .position(|row| *row == Row::NoSession(machine))
        .unwrap_or_else(|| panic!("no row under machine {machine}: {:?}", app.rows.rows))
}

#[test]
fn the_row_takes_the_next_number_and_the_later_sessions_move_up_by_one() {
    let cfg = depth_all();
    let app = app_over(&cfg, with_a_spare_between());
    let label = |at: usize| app.row_label(at);
    assert_eq!(label(row_of(&app, "garden")).as_deref(), Some("6"));
    assert_eq!(label(row_of(&app, "photos")).as_deref(), Some("7"));
    assert_eq!(label(no_session_row(&app, 1)).as_deref(), Some("8"));
    assert_eq!(label(row_of(&app, "homelab")).as_deref(), Some("9"));
    // Without the spare machine homelab is 8: the row is what moved it.
    let without = app_over(&cfg, five_and_two("idle", true));
    assert_eq!(
        without.row_label(row_of(&without, "homelab")).as_deref(),
        Some("8")
    );
}

#[test]
fn the_rows_number_is_drawn_where_a_sessions_is_and_in_its_style() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, with_a_spare_between());
    let buf = draw(&mut app);
    let column = |y: u16, digit: &str| -> u16 {
        (0..buf.area().width)
            .find(|&x| buf[(x, y)].symbol() == digit)
            .unwrap_or_else(|| panic!("no {digit} on {:?}", line(&buf, y)))
    };
    let (session, row) = (find(&buf, "garden"), find(&buf, words::NO_SESSION_ROW));
    let (at, here) = (column(session, "6"), column(row, "8"));
    assert_eq!(at, here, "{:?}\n{:?}", line(&buf, session), line(&buf, row));
    assert_eq!(buf[(at, session)].style(), buf[(here, row)].style());
    // Its words keep their column.
    assert!(
        line(&buf, row)
            .chars()
            .skip(usize::from(here))
            .collect::<String>()
            .starts_with(&format!("8  {}", words::NO_SESSION_ROW)),
        "{:?}",
        line(&buf, row)
    );
}

#[test]
fn a_row_past_the_last_number_gets_none() {
    let cfg = depth_all();
    let mut local = snapshot();
    for n in 6..=8 {
        local
            .spaces
            .push(space(n, &format!("~/code/extra{n}"), "main", (0, 0, 0, 0)));
    }
    // Eight sessions here: `spare` is ninth, and garden, photos and `empty` are past the last number.
    let app = app_over(
        &cfg,
        merge(
            local,
            &[
                view_with("spare", Some(State::default())),
                view_with("studio", Some(photos_state())),
                view_with("empty", Some(State::default())),
            ],
            NOW,
        ),
    );
    assert_eq!(app.row_label(no_session_row(&app, 0)).as_deref(), Some("9"));
    assert_eq!(app.row_label(row_of(&app, "garden")), None);
    assert_eq!(app.row_label(row_of(&app, "photos")), None);
    assert_eq!(app.row_label(no_session_row(&app, 2)), None);
}

#[test]
fn the_rows_number_puts_the_cursor_on_it_and_enter_there_still_starts_a_session() {
    use crate::tui::actions::Action;
    let cfg = depth_all();
    let mut app = app_over(&cfg, with_a_spare_between());
    // A session's number on another machine puts the cursor on that session: the positive half.
    press(&mut app, '6');
    assert_eq!(on_session(&app), "garden");
    press(&mut app, '8');
    assert_eq!(app.current(), Some(&Row::NoSession(1)));
    assert_eq!(toast_of(&app), None);
    let action = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        action,
        Some(Action::Doorway {
            machine: "spare".into(),
            id: "new".into()
        })
    );
}

#[test]
fn tab_passes_the_numbered_row_by_and_j_and_k_stop_on_it() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, with_a_spare_between());
    let at = no_session_row(&app, 1);
    assert!(app.row_label(at).is_some(), "the row has a number");
    // Tab walks what needs you and never rests on the row.
    let mut seen = Vec::new();
    for _ in 0..12 {
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        seen.push(app.current().cloned());
    }
    // The positive half: Tab rests on agents.
    assert!(
        seen.iter().all(|row| matches!(row, Some(Row::Pane(_)))),
        "Tab rested on an agent each time: {seen:?}"
    );
    assert!(
        !seen.contains(&Some(Row::NoSession(1))),
        "Tab rested on the row: {seen:?}"
    );
    // j and k stop on it from either side.
    app.sel = at;
    press(&mut app, 'j');
    assert_ne!(app.sel, at, "j left the row");
    press(&mut app, 'k');
    assert_eq!(app.sel, at, "k came back to the row");
    press(&mut app, 'k');
    assert_ne!(app.sel, at, "k left the row");
    press(&mut app, 'j');
    assert_eq!(app.sel, at, "j came back to the row");
}

#[test]
fn typing_the_rows_number_in_the_search_preset_puts_the_cursor_on_it() {
    let mut cfg = depth_all();
    cfg.keys.preset = crate::config::KeysPreset::Search;
    // A session with an 8 in its name also matches the 8 typed: the numbered row must win over it.
    let mut snap = with_a_spare_between();
    snap.spaces[0].session_name = "node18".into();
    let mut app = app_over(&cfg, snap);
    let at = no_session_row(&app, 1);
    assert!(
        app.label_works(at),
        "the row's 8 is drawn as a key that works"
    );
    press(&mut app, '8');
    assert!(
        app.rows.rows.iter().any(|row| matches!(
            row,
            Row::Session(space) if app.snap.spaces[*space].session_name == "node18"
        )),
        "the other match is listed beside the row: {:?}",
        app.rows.rows
    );
    assert_eq!(
        app.current(),
        Some(&Row::NoSession(1)),
        "typing the row's number puts the cursor on it, not on the other match: {:?}",
        app.rows.rows
    );
}

#[test]
fn a_machine_that_is_this_servers_own_holds_no_place_in_the_numbers() {
    let cfg = depth_all();
    let mut own = view_with("laptop", Some(State::default()));
    own.this_server = true;
    let snap = merge(
        snapshot(),
        &[own, view_with("studio", Some(photos_state()))],
        NOW,
    );
    let app = app_over(&cfg, snap);
    // This machine's own server is listed first and has no row, so studio's sessions are the sixth
    // and seventh: no number is skipped and none is drawn for the own server.
    assert_eq!(app.row_label(row_of(&app, "garden")).as_deref(), Some("6"));
    assert_eq!(app.row_label(row_of(&app, "photos")).as_deref(), Some("7"));
    assert!(
        !app.rows
            .rows
            .iter()
            .any(|row| matches!(row, Row::NoSession(_))),
        "the own server has no row that starts a session: {:?}",
        app.rows.rows
    );
}

#[test]
fn typing_the_rows_number_after_slash_puts_the_cursor_on_it() {
    let cfg = depth_all();
    let mut app = app_over(&cfg, with_a_spare_between());
    press(&mut app, '/');
    assert!(
        app.label_works(no_session_row(&app, 1)),
        "the row's 8 is drawn as a key that works"
    );
    press(&mut app, '8');
    assert_eq!(
        app.current(),
        Some(&Row::NoSession(1)),
        "typing the row's number puts the cursor on it: {:?}",
        app.rows.rows
    );
}

#[test]
fn typing_the_number_of_a_session_after_the_row_lights_its_letters() {
    let mut cfg = depth_all();
    cfg.keys.preset = crate::config::KeysPreset::Search;
    let mut app = app_over(&cfg, with_a_spare_between());
    press(&mut app, '9');
    assert_eq!(on_session(&app), "homelab");
    let lit = app.lit_letters();
    assert!(!lit.is_empty(), "homelab's letters are lit after typing 9");
    let homelab = app
        .snap
        .spaces
        .iter()
        .position(|s| s.session_name == "homelab")
        .expect("homelab");
    for (index, _) in &lit {
        assert_eq!(
            crate::tui::tree::space_of(&app.snap, &app.rows.rows[*index]),
            Some(homelab),
            "only homelab's letters are lit: {lit:?}"
        );
    }
}

#[test]
fn a_sessionless_machine_that_drops_connects_or_comes_back_moves_no_number() {
    let cfg = depth_all();
    let with_spare = |spare: MachineView| {
        merge(
            snapshot(),
            &[
                view_with("studio", Some(photos_state())),
                spare,
                view_with("pi", Some(homelab_state())),
            ],
            NOW,
        )
    };
    let mut still_connecting = view_with("spare", None);
    still_connecting.live = false;
    still_connecting.words = "connecting".to_string();
    let connected_unstated = view_with("spare", None);
    let mut dropped = view_with("spare", Some(State::default()));
    dropped.live = false;
    dropped.words = "not answering".to_string();
    let worlds = [
        (
            "connected",
            with_spare(view_with("spare", Some(State::default()))),
        ),
        ("still connecting", with_spare(still_connecting)),
        (
            "connected, nothing said yet",
            with_spare(connected_unstated),
        ),
        ("dropped", with_spare(dropped)),
        ("back again", with_a_spare_between()),
    ];
    for (what, snap) in worlds {
        let app = app_over(&cfg, snap);
        let label = |session: &str| app.row_label(row_of(&app, session));
        assert_eq!(label("garden").as_deref(), Some("6"), "{what}");
        assert_eq!(label("photos").as_deref(), Some("7"), "{what}");
        assert_eq!(label("homelab").as_deref(), Some("9"), "{what}");
        let row = app.rows.rows.iter().position(|r| *r == Row::NoSession(1));
        match row {
            Some(at) => assert_eq!(app.row_label(at).as_deref(), Some("8"), "{what}"),
            None => assert!(
                what != "connected" && what != "back again",
                "{what}: no row under spare: {:?}",
                app.rows.rows
            ),
        }
    }
}
