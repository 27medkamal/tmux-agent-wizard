//! The filter's directory section, against the sample world.
//!
//! With `[ui] projects_in_filter` on, the `/` filter also lists the directories the new
//! session picker would list for the same text, under every session and agent row; Enter on
//! one opens or creates its session. The world is [`crate::tui::fixtures`] under a scratch
//! `$HOME`, so every directory is a real one, and zoxide's history is handed over the way
//! the run loop hands it.

use std::fs;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton};
use ratatui::layout::Rect;

use super::super::tests::{app, ch, config, key, search_config};
use super::super::*;
use crate::tui::picker::{Project, ProjectPicker, Tag};

// ------------------------------------------------------------------ helpers

/// zoxide's history for most tests: two directories named like the fixture's `api` session
/// (one is where a test moves it to), and two names no fixture row contains.
const ZOXIDE: [&str; 4] = [
    "~/oss/api",
    "~/z/zebra-crossing",
    "~/z/shop-pad",
    "~/code/api",
];

/// A scratch `$HOME` holding the directories the tests list.
fn world() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for dir in [
        "code/api",
        "oss/api",
        "z/zebra-crossing",
        "z/shop-pad",
        "code/shop-notes",
        "code/quartz-lab",
        "clients/falcon",
    ] {
        fs::create_dir_all(home.path().join(dir)).unwrap();
    }
    home
}

/// The popup's config with the switch as asked, in either preset, at the `all` depth, with
/// two `project_dirs` under the scratch home.
fn configured(on: bool, home: &Path, search: bool) -> Config {
    let mut cfg = if search { search_config() } else { config() };
    cfg.ui.projects_in_filter = on;
    cfg.sessions.zoxide = true;
    cfg.sessions.project_dirs = vec![home.join("code"), home.join("clients")];
    cfg
}

/// The popup over the fixture's world with the scratch home as `$HOME` and `zoxide` handed
/// over, as `run` does once it has asked zoxide.
fn popup<'a>(cfg: &'a Config, home: &Path, zoxide: &[&str]) -> App<'a> {
    let mut popup = app(cfg);
    popup.home = home.to_path_buf();
    popup.set_zoxide(zoxide.iter().map(|d| (*d).to_string()).collect());
    popup
}

/// Opens the filter: `/` in the modal preset. The search preset's is open from the start,
/// and a `/` typed into it is text.
fn open_filter(popup: &mut App) {
    if popup.preset == KeysPreset::Modal {
        popup.on_key(ch('/'));
    }
}

fn type_str(popup: &mut App, text: &str) {
    for c in text.chars() {
        popup.on_key(ch(c));
    }
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The paths of the directory rows, in the order drawn.
fn listed<'a>(popup: &'a App<'_>) -> Vec<&'a str> {
    popup
        .rows
        .rows
        .iter()
        .filter_map(|row| match row {
            Row::Project(at) => Some(popup.projects[*at].path.as_str()),
            _ => None,
        })
        .collect()
}

/// What the new session picker lists for `typed` in this popup's world, first `limit`.
fn pickers(popup: &App, typed: &str, limit: usize) -> Vec<String> {
    let mut picker = ProjectPicker::new(popup.cfg, &popup.home);
    for c in typed.chars() {
        picker.on_key(ch(c));
    }
    picker.refresh(&popup.snap, popup.cfg, &popup.home, &popup.zoxide);
    picker
        .candidates
        .iter()
        .take(limit)
        .map(|c| c.path.clone())
        .collect()
}

/// The fixture's `api` session, moved to the scratch `~/code/api`, so it is running in a
/// directory that really exists there.
fn api_runs_in_the_scratch_home(popup: &mut App, home: &Path) {
    let mut snap = popup.snap.clone();
    let api = snap
        .spaces
        .iter_mut()
        .find(|s| s.session_name == "api")
        .expect("the fixture's api session");
    api.session_path = home.join("code/api");
    popup.set_snapshot(snap);
}

// ------------------------------------------------------------ the section

/// The switch is off unless asked for, and off is the popup as it was: no directory in any
/// state of the filter, in either preset, with or without a query.
#[test]
fn with_the_switch_off_the_filter_lists_no_directories() {
    let home = world();
    for search in [false, true] {
        let cfg = configured(false, home.path(), search);
        let mut popup = popup(&cfg, home.path(), &ZOXIDE);
        let untouched = popup.rows.rows.clone();
        open_filter(&mut popup);
        assert!(popup.projects.is_empty(), "search: {search}");
        assert!(listed(&popup).is_empty(), "search: {search}");
        assert_eq!(popup.rows.rows, untouched, "search: {search}");
        type_str(&mut popup, "shop");
        assert!(listed(&popup).is_empty(), "search: {search}, a query typed");
        assert!(popup.projects.is_empty(), "search: {search}, a query typed");
    }
}

/// The modal preset's section is the filter's: nothing before `/`, the list while it is
/// open, and nothing again once Esc has cleared it, the tree as it was.
#[test]
fn the_section_lives_as_long_as_the_filter_is_open() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    let before = popup.rows.rows.clone();
    assert!(listed(&popup).is_empty(), "no filter yet");

    popup.on_key(ch('/'));
    assert!(!listed(&popup).is_empty(), "the filter is open");

    type_str(&mut popup, "shop");
    popup.on_key(key(KeyCode::Esc));
    assert!(popup.filter.is_none());
    assert!(popup.projects.is_empty());
    assert!(listed(&popup).is_empty(), "Esc took the section away");
    assert_eq!(popup.rows.rows, before, "and the tree is as it was");
}

/// The search preset's filter is open from the first frame, and so is its section.
#[test]
fn the_search_preset_lists_them_from_its_first_frame() {
    let home = world();
    let cfg = configured(true, home.path(), true);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);

    assert_eq!(listed(&popup), pickers(&popup, "", 8));
    assert!(!listed(&popup).is_empty());

    type_str(&mut popup, "shop");
    assert_eq!(listed(&popup), pickers(&popup, "shop", 8));
}

/// Right after `/` the section is the picker's list for nothing typed: zoxide's directories
/// in zoxide's own order, then the project directories', the first eight, every one of them
/// below every session and agent row, which are exactly what they were.
#[test]
fn right_after_slash_the_pickers_default_list_shows_below_the_tree_capped_at_eight() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let history: Vec<String> = (0..12).map(|n| format!("~/z/p{n:02}")).collect();
    let history: Vec<&str> = history.iter().map(String::as_str).collect();
    let mut popup = popup(&cfg, home.path(), &history);
    let tree = popup.rows.rows.clone();

    popup.on_key(ch('/'));

    assert_eq!(
        listed(&popup),
        (0..8).map(|n| format!("~/z/p{n:02}")).collect::<Vec<_>>()
    );
    assert_eq!(popup.rows.rows[..tree.len()], tree[..]);
    assert!(
        popup.rows.rows[tree.len()..]
            .iter()
            .all(|row| matches!(row, Row::Project(_))),
        "nothing but directories below the tree"
    );
    assert_eq!(popup.rows.first_project(), Some(tree.len()));
}

/// With a query the section is the picker's matches for that query, and only those: the
/// picker's own words, fzf's exact and negated terms included, however many it has.
#[test]
fn typing_narrows_the_section_to_what_the_picker_lists_for_the_query() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));

    for typed in ["s", "sh", "shop", "shop notes", "!api", "'kka"] {
        popup.on_key(ctrl('u'));
        type_str(&mut popup, typed);
        assert_eq!(listed(&popup), pickers(&popup, typed, 8), "{typed:?}");
    }
    // The cases above are not all empty: `shop` alone finds two directories, and only those.
    popup.on_key(ctrl('u'));
    type_str(&mut popup, "shop");
    assert_eq!(listed(&popup), ["~/z/shop-pad", "~/code/shop-notes"]);
}

/// More matches than the cap are cut to it, in the picker's order, and a path typed brings
/// the picker's filesystem completion in front, as it does there.
#[test]
fn a_query_matching_more_than_eight_shows_eight_and_a_path_completes() {
    let home = world();
    for n in 0..12 {
        fs::create_dir_all(home.path().join(format!("many/m{n:02}"))).unwrap();
    }
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &[]);
    popup.on_key(ch('/'));

    type_str(&mut popup, "~/many/");

    assert_eq!(listed(&popup).len(), 8);
    assert_eq!(listed(&popup), pickers(&popup, "~/many/", 8));
    assert_eq!(listed(&popup)[0], "~/many/m00");
}

/// A cursor key that leaves the text as it was does not read the filesystem again, and a key
/// that changes it lists again: the section follows the text, not the keys.
#[test]
fn the_section_is_listed_again_only_when_the_text_changes() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    type_str(&mut popup, "shop");
    let shop = popup.projects.clone();

    // A directory appears on the disk. A left arrow moves the input's cursor and nothing else.
    fs::create_dir_all(home.path().join("code/shop-new")).unwrap();
    popup.on_key(key(KeyCode::Left));
    assert_eq!(popup.projects, shop, "the text did not change");

    // An edit does list again, and finds it.
    popup.on_key(ch('x'));
    popup.on_key(key(KeyCode::Backspace));
    assert!(
        listed(&popup).contains(&"~/code/shop-new"),
        "{:?}",
        listed(&popup)
    );
}

/// A directory a session is running in is listed all the same, tagged `running`, and the
/// others say where they came from: the picker's three tags.
#[test]
fn a_directory_with_a_session_is_listed_and_tagged_running() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    api_runs_in_the_scratch_home(&mut popup, home.path());
    popup.on_key(ch('/'));

    let tag = |path: &str| {
        popup
            .projects
            .iter()
            .find(|p| p.path == path)
            .unwrap_or_else(|| panic!("{path} is not listed: {:?}", popup.projects))
            .tag
    };
    assert_eq!(tag("~/code/api"), Some(Tag::Running));
    assert_eq!(tag("~/oss/api"), Some(Tag::Zoxide));
    assert_eq!(tag("~/code/shop-notes"), Some(Tag::ProjectDir));
}

/// A directory's tag (`running`, `zoxide`, `project dir`) is a category, not words of the
/// directory: a query that is a tag word lists no directory (the picker lists by path), and one
/// that lists a directory by a path scattered through with the same letters lights the path's
/// letters only, none of the tag.
#[test]
fn a_directory_is_listed_and_lit_by_its_path_never_by_its_tag() {
    let home = world();
    fs::create_dir_all(home.path().join("z/zo-x-i-d-e")).unwrap();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &["~/z/zo-x-i-d-e", "~/z/shop-pad"]);
    api_runs_in_the_scratch_home(&mut popup, home.path());
    popup.on_key(ch('/'));
    assert!(
        listed(&popup).contains(&"~/z/zo-x-i-d-e"),
        "the unfiltered list is there: {:?}",
        listed(&popup)
    );

    type_str(&mut popup, "running");
    assert_eq!(listed(&popup), Vec::<&str>::new(), "no path says running");
    for _ in 0..7 {
        popup.on_key(key(KeyCode::Backspace));
    }
    type_str(&mut popup, "project dir");
    assert_eq!(
        listed(&popup),
        Vec::<&str>::new(),
        "no path says project dir"
    );
    for _ in 0..11 {
        popup.on_key(key(KeyCode::Backspace));
    }

    type_str(&mut popup, "zoxide");
    let at = popup
        .rows
        .first_project()
        .expect("the scattered path is listed");
    let Row::Project(shown) = popup.rows.rows[at] else {
        panic!("a directory");
    };
    let path = popup.projects[shown].path.as_str();
    assert_eq!(path, "~/z/zo-x-i-d-e");
    assert_eq!(popup.projects[shown].tag, Some(Tag::Zoxide));
    let (_, lit) = popup.rows.scores[at].clone().expect("its path matches");
    assert_eq!(lit.len(), 6, "the six letters of the path: {lit:?}");
    assert!(
        lit.iter().all(|&i| i < path.chars().count()),
        "none in the tag: {lit:?}"
    );
}

/// The section is the last thing in the list whatever the tree looks like: every layout at
/// every depth, the flat list included.
#[test]
fn the_section_is_last_in_every_layout_and_depth() {
    let home = world();
    for depth in [Depth::Sessions, Depth::Agents, Depth::All] {
        for layout in [Layout::Tree, Layout::Flat] {
            let mut cfg = configured(true, home.path(), false);
            cfg.ui.depth = depth;
            cfg.ui.layout = layout;
            let mut popup = popup(&cfg, home.path(), &ZOXIDE);
            popup.on_key(ch('/'));
            let first = popup
                .rows
                .first_project()
                .unwrap_or_else(|| panic!("{depth:?} {layout:?}: no section"));
            assert!(first > 0, "{depth:?} {layout:?}: sessions come first");
            assert!(
                popup.rows.rows[first..]
                    .iter()
                    .all(|row| matches!(row, Row::Project(_))),
                "{depth:?} {layout:?}"
            );
            assert!(
                popup.rows.rows[..first]
                    .iter()
                    .all(|row| !matches!(row, Row::Project(_))),
                "{depth:?} {layout:?}"
            );
        }
    }
}

/// Nothing to list is no section: the switch on, a query no directory matches, and the
/// rows are the tree's alone, so there is nothing to put a header over.
#[test]
fn a_query_no_directory_matches_leaves_no_section() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));

    type_str(&mut popup, "zzzzzz");

    assert!(popup.projects.is_empty());
    assert_eq!(popup.rows.first_project(), None);
    assert!(
        popup.rows.rows.is_empty(),
        "the tree matched nothing either"
    );
}

/// `[sessions] zoxide = false` keeps zoxide's history out of the section as it does the
/// picker's; the project directories still list.
#[test]
fn zoxide_switched_off_lists_only_the_project_directories() {
    let home = world();
    let mut cfg = configured(true, home.path(), false);
    cfg.sessions.zoxide = false;
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);

    popup.on_key(ch('/'));

    assert_eq!(
        listed(&popup),
        [
            "~/code/api",
            "~/code/quartz-lab",
            "~/code/shop-notes",
            "~/clients/falcon",
        ]
    );
}

/// A listed directory is its path in tilde form and its tag; zoxide's first is listed first.
#[test]
fn a_project_is_its_path_and_its_tag() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    assert_eq!(
        popup.projects[0],
        Project {
            path: "~/oss/api".to_string(),
            tag: Some(Tag::Zoxide),
        }
    );
}

// -------------------------------------------------------------------- the cursor

/// The row the cursor is on, as an index into the rows.
fn on<'a>(popup: &'a App<'_>) -> &'a Row {
    popup.current().expect("a row under the cursor")
}

fn on_a_directory(popup: &App) -> bool {
    matches!(popup.current(), Some(Row::Project(_)))
}

/// The rule for the cursor: a query some session or agent row matches puts it where
/// it always went, on the best of them, whatever the directories score; the directories
/// never take it. Every query below matches a row of the tree, most of them only loosely (a
/// few letters of `claude`, `codex`, `gemini`, `nvim`), while a directory of exactly that
/// name is listed and outscores the row, so a cursor that went to the best match of
/// everything would be on the directory. With the switch off, the same queries put the
/// cursor on the same rows: the section changes nothing above it.
#[test]
fn a_query_a_session_or_agent_matches_puts_the_cursor_where_it_goes_without_directories() {
    let home = world();
    let names = ["clde", "gmn", "cdx", "nvm", "api", "zsh"];
    for name in names {
        fs::create_dir_all(home.path().join("z").join(name)).unwrap();
    }
    let zoxide: Vec<String> = names.iter().map(|name| format!("~/z/{name}")).collect();
    let zoxide: Vec<&str> = zoxide.iter().map(String::as_str).collect();

    for search in [false, true] {
        for query in names {
            let off = configured(false, home.path(), search);
            let mut without = popup(&off, home.path(), &zoxide);
            open_filter(&mut without);
            type_str(&mut without, query);

            let on = configured(true, home.path(), search);
            let mut with = popup(&on, home.path(), &zoxide);
            open_filter(&mut with);
            type_str(&mut with, query);

            assert!(
                !without.rows.rows.is_empty(),
                "{query:?}: the tree matches it, or this proves nothing"
            );
            assert!(
                listed(&with).contains(&format!("~/z/{query}").as_str()),
                "{query:?}: and the directory of that name is listed: {:?}",
                listed(&with)
            );
            assert_eq!(
                with.current(),
                without.current(),
                "{query:?}, search: {search}"
            );
            assert!(!on_a_directory(&with), "{query:?}, search: {search}");
        }
    }
}

/// A query no session or agent matches leaves the tree empty, and the cursor goes to the
/// first directory: type a name, Enter, the way tmux-session-wizard is used. In both presets.
#[test]
fn a_query_nothing_above_matches_puts_the_cursor_on_the_first_directory() {
    let home = world();
    for search in [false, true] {
        let cfg = configured(true, home.path(), search);
        let mut popup = popup(&cfg, home.path(), &ZOXIDE);
        open_filter(&mut popup);

        type_str(&mut popup, "shop");

        assert_eq!(listed(&popup), ["~/z/shop-pad", "~/code/shop-notes"]);
        assert_eq!(on(&popup), &Row::Project(0), "search: {search}");
        assert_eq!(popup.rows.first_project(), Some(popup.sel));
    }
}

/// A query walked from matching a pane to matching nothing above and back: the cursor is on
/// the pane, then on the first directory with the tree empty, then on the same pane again, and
/// the walk can be made twice. `cld` is a loose match for an agent's `claude` and an exact one
/// for a directory's name, which scores higher and still does not take the cursor from the
/// pane; `cld-` is in no row of the tree (which has no hyphen in it) and still in the
/// directory's. In both presets, and once more with the query cleared, which is the cursor's
/// own row.
#[test]
fn a_query_walked_from_a_pane_to_no_match_and_back_moves_the_cursor_there_and_back() {
    let home = world();
    fs::create_dir_all(home.path().join("z/cld-notes")).unwrap();
    let zoxide: Vec<&str> = ZOXIDE.iter().copied().chain(["~/z/cld-notes"]).collect();
    for search in [false, true] {
        let cfg = configured(true, home.path(), search);
        let mut popup = popup(&cfg, home.path(), &zoxide);
        let own = popup.sel;
        open_filter(&mut popup);
        type_str(&mut popup, "cld");
        let pane = popup.sel;
        assert!(
            matches!(on(&popup), Row::Pane(_)),
            "search: {search}: on an agent's pane: {:?}",
            on(&popup)
        );
        assert_eq!(listed(&popup), ["~/z/cld-notes"], "search: {search}");

        for round in 0..2 {
            popup.on_key(ch('-'));
            assert_eq!(
                on(&popup),
                &Row::Project(0),
                "search: {search}, round {round}: no row of the tree matches"
            );
            assert_eq!(
                popup.rows.rows,
                [Row::Project(0)],
                "search: {search}, round {round}: the tree is empty"
            );
            assert_eq!(popup.sel, 0);

            popup.on_key(key(KeyCode::Backspace));
            assert_eq!(
                popup.sel, pane,
                "search: {search}, round {round}: back on the pane it was on"
            );
            assert!(matches!(on(&popup), Row::Pane(_)));
            assert_eq!(listed(&popup), ["~/z/cld-notes"]);
        }

        // Clearing it all puts the cursor on your own row again.
        popup.on_key(ch('-'));
        assert!(on_a_directory(&popup));
        popup.on_key(ctrl('u'));
        assert_eq!(popup.sel, own, "search: {search}: your own row");
    }
}

/// With nothing typed the cursor stays on your own row, as it does with the switch off:
/// the directories are there to scroll to, not somewhere the cursor is sent.
#[test]
fn right_after_slash_the_cursor_stays_where_it_was() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    let before = popup.sel;
    assert_eq!(
        popup.current(),
        Some(&Row::Pane(0)),
        "the pane the popup was opened from"
    );

    popup.on_key(ch('/'));

    assert_eq!(popup.sel, before);
    assert!(!listed(&popup).is_empty());
    // Backing out of a query returns it there, as always.
    type_str(&mut popup, "shop");
    assert!(on_a_directory(&popup));
    popup.on_key(ctrl('u'));
    assert_eq!(popup.sel, before, "an empty query is your own row again");
    popup.on_key(key(KeyCode::Esc));
    assert_eq!(popup.sel, before);
}

/// The keys that move the cursor inside the filter (the arrows, fzf's ctrl-n and ctrl-p, and
/// ctrl-j and ctrl-k) take it into the section and out of it like any row, wrapping at both
/// ends of the whole list, the directories included.
#[test]
fn every_key_that_moves_goes_into_and_out_of_the_section() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().expect("a section");
    let last = popup.rows.rows.len() - 1;
    assert!(first > 0 && last > first);

    for (down, up) in [
        (key(KeyCode::Down), key(KeyCode::Up)),
        (ctrl('n'), ctrl('p')),
        (ctrl('j'), ctrl('k')),
    ] {
        popup.sel = first - 1;
        popup.on_key(down);
        assert_eq!(on(&popup), &Row::Project(0), "down into the section");
        popup.on_key(up);
        assert_eq!(popup.sel, first - 1, "and up out of it");

        popup.sel = last;
        popup.on_key(down);
        assert_eq!(popup.sel, 0, "past the last directory wraps to the top");
        popup.on_key(up);
        assert_eq!(popup.sel, last, "and back to the last directory");
    }
}

// --------------------------------------------------------- keys that skip directories

/// The rows Tab stops on, in order, going round once from wherever the cursor is.
fn tab_stops(popup: &mut App, code: KeyCode, presses: usize) -> Vec<Row> {
    (0..presses)
        .map(|_| {
            popup.on_key(key(code));
            on(popup).clone()
        })
        .collect()
}

/// Next up walks the agents that need you and nothing else: no directory is a stop, going
/// either way, and from a directory the walk goes back to its ends.
#[test]
fn tab_and_shift_tab_never_stop_on_a_directory() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    let hot: Vec<usize> = popup.next_up_rows();
    assert_eq!(hot.len(), 2, "the fixture has two agents that need you");

    let forward = tab_stops(&mut popup, KeyCode::Tab, 5);
    let backward = tab_stops(&mut popup, KeyCode::BackTab, 5);
    for stop in forward.iter().chain(&backward) {
        assert!(matches!(stop, Row::Pane(_)), "stopped on {stop:?}");
    }

    let first = popup.rows.first_project().unwrap();
    popup.sel = first;
    popup.on_key(key(KeyCode::Tab));
    assert_eq!(
        popup.sel, hot[0],
        "from a directory Tab goes to the first agent"
    );
    popup.sel = first + 1;
    popup.on_key(key(KeyCode::BackTab));
    assert_eq!(popup.sel, hot[1], "and Shift-Tab to the last");
}

/// The session walk never rests on a directory: with no agent in view Tab walks the session
/// headers, and in a flat list, which tells one session's rows from the next by the session
/// each row belongs to, a directory is not the start of another session.
#[test]
fn with_no_agent_in_view_the_session_walk_skips_the_directories_too() {
    let home = world();
    for layout in [Layout::Tree, Layout::Flat] {
        let mut cfg = configured(true, home.path(), false);
        cfg.ui.layout = layout;
        cfg.ui.depth = Depth::Sessions;
        let mut popup = popup(&cfg, home.path(), &ZOXIDE);
        popup.on_key(ch('/'));
        // Depth `sessions` shows headers only and the tree is a list of them either way.
        let last = popup.rows.first_project().unwrap() - 1;
        popup.sel = last;
        for dir in [1, -1, 1, 1, -1] {
            popup.next_session(dir);
            assert!(!on_a_directory(&popup), "{layout:?}: next_session({dir})");
        }
        // Down at the sessions depth, next up has no agent to stop on and walks the headers.
        popup.sel = last;
        popup.next_attention(1);
        assert!(!on_a_directory(&popup), "{layout:?}: Tab");
    }

    // A flat list of agents: the session walk (`J`, when it is a key of its own) leaves the
    // last row's session for another and never rests on a directory in between.
    let mut cfg = configured(true, home.path(), false);
    cfg.ui.layout = Layout::Flat;
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    popup.sel = popup.rows.first_project().unwrap() - 1;
    popup.next_session(1);
    assert!(!on_a_directory(&popup), "{:?}", popup.current());
    popup.sel = popup.rows.first_project().unwrap();
    popup.next_session(-1);
    assert!(!on_a_directory(&popup), "{:?}", popup.current());
}

/// A directory is never numbered or lettered: no label in a flat list (which numbers its
/// first nine rows and counts none of the directories), and no letter for it, which the
/// filter being open takes away from every row anyway.
#[test]
fn a_directory_wears_no_number_and_no_letter() {
    let home = world();
    let mut cfg = configured(true, home.path(), false);
    cfg.ui.layout = Layout::Flat;
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().unwrap();
    let numbered = |popup: &App| -> Vec<Option<u8>> {
        (0..popup.rows.rows.len())
            .map(|at| popup.label_index(at))
            .collect()
    };

    let labels = numbered(&popup);

    assert!(labels[first..].iter().all(Option::is_none), "{labels:?}");
    assert!(
        labels[..first].iter().any(Option::is_some),
        "the rows above are numbered"
    );
    assert!(
        popup.lettered_rows().is_empty(),
        "no letters while the filter is open"
    );
    popup.select_session(9);
    assert!(
        !on_a_directory(&popup),
        "a number with no row moves nowhere"
    );
}

/// `x`, `r` and `w` have nothing to act on in a directory: no confirmation, no prompt, no
/// menu, no message. The modal filter takes a plain letter as text, so the keys reach it
/// through `[keys]` entries in the search preset and through the row actions the menus run.
#[test]
fn close_rename_and_new_worktree_ignore_a_directory() {
    let home = world();
    let mut cfg = configured(true, home.path(), true);
    for (action, spec) in [
        ("close", "ctrl+x"),
        ("rename", "ctrl+r"),
        ("new_worktree", "ctrl+w"),
    ] {
        cfg.keys.bindings.insert(
            action.to_string(),
            crate::config::KeySpec::One(spec.to_string()),
        );
    }
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    type_str(&mut popup, "shop");
    assert!(on_a_directory(&popup));

    for chord in ['x', 'r'] {
        assert_eq!(popup.on_key(ctrl(chord)), None, "ctrl-{chord}");
        assert!(
            !popup.overlay.is_open(),
            "ctrl-{chord}: {:?}",
            popup.overlay
        );
        assert_eq!(popup.live_toast(), None, "ctrl-{chord}");
    }
    assert_eq!(popup.row_action(RowAction::Close), None);
    assert_eq!(popup.row_action(RowAction::Rename), None);
    assert!(!popup.overlay.is_open());
    assert_eq!(popup.live_toast(), None);

    // A new worktree is cut from a session, and a directory is not one.
    assert_eq!(popup.row_action(RowAction::NewWorktree), None);
    assert!(!popup.overlay.is_open());
    assert!(on_a_directory(&popup), "and the cursor is where it was");
}

// ------------------------------------------------------------------------ the mouse

/// Where a frame would have put the rows: one per line down the panel, so a click can name
/// one without drawing.
fn lay_out_the_rows(popup: &mut App) {
    popup.hits.tree = PanelHit {
        area: Rect::new(0, 0, 60, 60),
        rows: (0..popup.rows.rows.len())
            .map(|at| (at as u16, at))
            .collect(),
        ..PanelHit::default()
    };
}

/// A right-click on a directory does nothing: no menu, and the cursor stays. On a session
/// row it still opens that row's menu.
#[test]
fn a_right_click_on_a_directory_does_nothing() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    lay_out_the_rows(&mut popup);
    let first = popup.rows.first_project().unwrap();
    let before = popup.sel;

    assert_eq!(
        popup.on_mouse(super::super::tests::click(
            3,
            first as u16,
            MouseButton::Right
        )),
        None
    );

    assert!(!popup.overlay.is_open(), "no menu for a directory");
    assert_eq!(popup.sel, before, "and the cursor stayed");

    // With the cursor on a directory, a right-click on nothing opens no menu either.
    popup.sel = first;
    popup.on_mouse(super::super::tests::click(3, 59, MouseButton::Right));
    assert!(!popup.overlay.is_open());

    // A session row keeps its menu.
    popup.on_mouse(super::super::tests::click(3, 0, MouseButton::Right));
    assert!(
        matches!(popup.overlay, Overlay::Menu(_)),
        "{:?}",
        popup.overlay
    );
    assert_eq!(popup.sel, 0);
}

/// A left click selects a directory like any row.
#[test]
fn a_click_selects_a_directory() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    lay_out_the_rows(&mut popup);
    let first = popup.rows.first_project().unwrap();

    popup.on_mouse(super::super::tests::click(
        3,
        first as u16 + 1,
        MouseButton::Left,
    ));

    assert_eq!(on(&popup), &Row::Project(1));
}

// -------------------------------------------------------------------------- Enter

/// Enter on a directory asks for what the picker's Enter asks for: the session for that
/// directory, switched to when one is running there and created first when not (the run loop
/// hands the path to `session::create::open_or_create`, which runs the create hooks). The
/// popup closes when that has worked, as after any go, and shows a toast when it has not, so
/// nothing here quits or leaves the filter.
#[test]
fn enter_on_a_directory_asks_to_open_or_create_its_session() {
    let home = world();
    for search in [false, true] {
        let cfg = configured(true, home.path(), search);
        let mut popup = popup(&cfg, home.path(), &ZOXIDE);
        open_filter(&mut popup);
        type_str(&mut popup, "shop");
        assert!(on_a_directory(&popup));

        assert_eq!(
            popup.on_key(key(KeyCode::Enter)),
            Some(Action::OpenProject {
                path: "~/z/shop-pad".to_string()
            }),
            "search: {search}"
        );
        assert!(!popup.quit, "search: {search}");
        assert!(popup.filter.is_some(), "search: {search}");
    }
}

/// A directory somebody already has a session in is opened the same way: it is the run
/// loop's `open_or_create` that finds the session by path and switches to it.
#[test]
fn enter_on_a_directory_with_a_session_asks_for_the_same_open() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    api_runs_in_the_scratch_home(&mut popup, home.path());
    popup.on_key(ch('/'));
    let at = popup
        .projects
        .iter()
        .position(|p| p.path == "~/code/api")
        .expect("the directory a session runs in is listed");
    assert_eq!(popup.projects[at].tag, Some(Tag::Running));
    popup.sel = popup
        .rows
        .rows
        .iter()
        .position(|row| row == &Row::Project(at))
        .unwrap();

    assert_eq!(
        popup.on_key(key(KeyCode::Enter)),
        Some(Action::OpenProject {
            path: "~/code/api".to_string()
        })
    );
}

/// Enter goes to a directory the cursor walked to, as it does to any row, and a double-click
/// on one opens it like it goes to any other row.
#[test]
fn a_directory_reached_by_walking_or_double_clicked_opens() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    popup.sel = popup.rows.first_project().unwrap() - 1;
    popup.on_key(key(KeyCode::Down));
    popup.on_key(key(KeyCode::Down));
    assert_eq!(on(&popup), &Row::Project(1));
    assert_eq!(
        popup.on_key(key(KeyCode::Enter)),
        Some(Action::OpenProject {
            path: popup.projects[1].path.clone()
        })
    );

    lay_out_the_rows(&mut popup);
    let y = popup.rows.first_project().unwrap() as u16 + 2;
    let click = super::super::tests::click;
    assert_eq!(popup.on_mouse(click(3, y, MouseButton::Left)), None);
    assert_eq!(
        popup.on_mouse(click(3, y, MouseButton::Left)),
        Some(Action::OpenProject {
            path: popup.projects[2].path.clone()
        }),
        "the second click of a double-click"
    );
}

/// After an Enter on a directory that could not be opened the filter is as it was, the
/// cursor on the same directory: what the run loop does with a failed open is a toast.
#[test]
fn the_filter_and_the_cursor_survive_an_enter_the_run_loop_could_not_honour() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    type_str(&mut popup, "shop");
    let at = popup.sel;

    let action = popup.on_key(key(KeyCode::Enter)).expect("an open");
    // The run loop reports a failure on the hint line and refreshes the world.
    popup.toast("no such directory: ~/z/shop-pad");
    popup.set_data(Some(popup.snap.clone()), None);

    assert!(matches!(action, Action::OpenProject { .. }));
    assert_eq!(popup.sel, at);
    assert!(on_a_directory(&popup));
    assert_eq!(
        popup.filter.as_ref().map(|input| input.value()),
        Some("shop")
    );
}

// -------------------------------------------------------------------------- the tick

/// The popup's snapshot with one more session, `name`, running in `at`.
fn with_a_session(popup: &App, name: &str, at: &Path) -> crate::tui::data::Snapshot {
    let mut snap = popup.snap.clone();
    snap.spaces.push(crate::tui::data::SpaceRow {
        session_id: "$9".to_string(),
        session_name: name.to_string(),
        session_path: at.to_path_buf(),
        ..crate::tui::data::SpaceRow::default()
    });
    snap
}

fn tag_of(popup: &App, path: &str) -> Option<Tag> {
    popup
        .projects
        .iter()
        .find(|p| p.path == path)
        .unwrap_or_else(|| panic!("{path} is not listed: {:?}", popup.projects))
        .tag
}

/// The two-a-second tick hands the popup a new snapshot, and the section follows it without
/// reading a directory: a session that opens where one is listed makes it `running` on the
/// next tick, one that closes makes it what it was, and every directory is still listed in
/// its place though none is on the disk any more.
#[test]
fn a_tick_tells_the_tags_again_and_reads_no_directory() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    let paths: Vec<String> = popup.projects.iter().map(|p| p.path.clone()).collect();
    let shop = "~/code/shop-notes";
    assert_eq!(tag_of(&popup, shop), Some(Tag::ProjectDir));
    let before = popup.snap.clone();

    fs::remove_dir_all(home.path().join("code")).unwrap();
    let opened = with_a_session(&popup, "shop-notes", &home.path().join("code/shop-notes"));
    popup.set_snapshot(opened);

    assert_eq!(
        popup
            .projects
            .iter()
            .map(|p| p.path.clone())
            .collect::<Vec<_>>(),
        paths,
        "the same directories, in the same order"
    );
    assert_eq!(
        tag_of(&popup, shop),
        Some(Tag::Running),
        "a session opened there"
    );

    popup.set_snapshot(before);
    assert_eq!(tag_of(&popup, shop), Some(Tag::ProjectDir), "and closed");
}

/// The cursor stays on the directory it is on when a tick changes the rows above it: a
/// session appearing shifts every directory down a row, and the cursor goes with its
/// directory, not with the position.
#[test]
fn a_tick_that_adds_a_session_keeps_the_cursor_on_its_directory() {
    let home = world();
    let cfg = configured(true, home.path(), false);
    let mut popup = popup(&cfg, home.path(), &ZOXIDE);
    popup.on_key(ch('/'));
    let first = popup.rows.first_project().unwrap();
    popup.sel = first + 1;
    let path = popup.projects[1].path.clone();
    let rows = popup.rows.rows.len();

    let opened = with_a_session(&popup, "elsewhere", &home.path().join("code/api"));
    popup.set_snapshot(opened);

    assert_eq!(popup.rows.rows.len(), rows + 1, "a header joined the tree");
    assert!(on_a_directory(&popup));
    let Some(Row::Project(at)) = popup.current().cloned() else {
        panic!("on a directory");
    };
    assert_eq!(popup.projects[at].path, path);
    assert_eq!(popup.sel, first + 2, "one row further down");
}

/// The search preset's filter is open from the first frame, and the run loop hands zoxide's
/// history over before drawing it: that is when its directories are listed, once, with zoxide's
/// in them. Listing them when the popup is built, before zoxide is asked, only means listing
/// them again a moment later, so a popup that has not been handed zoxide's answer lists none.
#[test]
fn the_search_presets_directories_are_listed_once_when_zoxide_answers() {
    let home = world();
    let cfg = configured(true, home.path(), true);
    let mut popup = app(&cfg);
    popup.home = home.path().to_path_buf();
    assert!(
        popup.projects.is_empty(),
        "nothing is listed before zoxide has answered: {:?}",
        popup.projects
    );

    popup.set_zoxide(ZOXIDE.iter().map(|d| (*d).to_string()).collect());

    assert_eq!(listed(&popup), pickers(&popup, "", 8));
    assert_eq!(
        listed(&popup)[0],
        "~/oss/api",
        "zoxide's first, then the rest"
    );
    assert_eq!(
        popup.rows.first_project(),
        Some(popup.rows.rows.len() - listed(&popup).len())
    );
}

/// zoxide answers after the popup is up: the search preset's section, listed from the first
/// frame without it, gains zoxide's directories in front once it has, and the cursor stays.
#[test]
fn zoxide_arriving_after_the_first_frame_lists_its_directories_without_moving_the_cursor() {
    let home = world();
    let cfg = configured(true, home.path(), true);
    let mut popup = app(&cfg);
    popup.home = home.path().to_path_buf();
    popup.refresh_projects();
    popup.rebuild();
    let before = popup.sel;
    assert!(
        !listed(&popup).contains(&"~/z/shop-pad"),
        "zoxide has not answered: {:?}",
        listed(&popup)
    );

    popup.set_zoxide(vec!["~/z/shop-pad".to_string()]);

    assert_eq!(
        listed(&popup)[0],
        "~/z/shop-pad",
        "zoxide's directories come first"
    );
    assert_eq!(popup.sel, before);
}
