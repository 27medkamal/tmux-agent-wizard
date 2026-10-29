//! The popup's wording rule: nothing it shows holds an em-dash, an
//! en-dash, an arrow or a curly quote. They read as machine-written, and a comma, a full
//! stop, a bracket or a new sentence says the same thing; a range is a hyphen (`1-9`).
//!
//! Two guards keep it true. The first collects what the popup can say from its data (the
//! help and legend of both presets, the hint line in each of its states, the menus, the
//! confirmations, the prompts, the options text, the toasts) and what a frame of each state
//! puts on screen. The second reads the source of the whole crate, the popup's and every
//! message, error, notification and help string besides, and refuses a string in it that
//! spells one of them out, so a string no test above reaches is caught too. Comments are
//! exempt: the rule is about what the user sees. The files a person reads that are not Rust
//! (the README, the documents, the scripts) and the rendered help of every command are
//! scanned by `tests/user_text.rs` and `src/cli/mod.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, MouseButton};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use super::tests::{
    api_has_one_window, api_is_in_a_group_of, app, ch, click, config, cursor_to_header,
    cursor_to_pane, cursor_to_worktree, key, search_config, with_api_worktrees,
};
use super::*;
use crate::config::{KeySpec, Keys, StatusIndicators};
use crate::paths::Paths;
use crate::session::worktree::RemoveRefusal;
use crate::tui::actions::{agent_renamed_toast, group_closed_toast, removal_question};
use crate::tui::data::Snapshot;
use crate::tui::fixtures::snapshot;
use crate::tui::overlays::{
    self, Chrome, Help, Menu, Prompt, PromptKind, also, help_keys, help_text, help_text_for,
    settings_text,
};
use crate::tui::picker::ProjectPicker;
use crate::tui::theme;
use crate::tui::view;

/// What the popup may not say. Spelled as escapes, so this file passes its own source scan.
const BANNED: [(char, &str); 11] = [
    ('\u{2014}', "an em-dash"),
    ('\u{2013}', "an en-dash"),
    ('\u{2192}', "an arrow"),
    ('\u{2018}', "a curly single quote"),
    ('\u{2019}', "a curly single quote"),
    ('\u{201c}', "a curly double quote"),
    ('\u{201d}', "a curly double quote"),
    // Look-alikes for the same things: a horizontal bar and a minus
    // sign where a dash would go, a double arrow and a heavy round-tipped one.
    ('\u{2015}', "a horizontal bar"),
    ('\u{2212}', "a minus sign"),
    ('\u{21d2}', "a double arrow"),
    ('\u{279c}', "an arrow"),
];

/// Everything a test collected, as `(where it was said, what was said)`.
#[derive(Default)]
struct Said(Vec<(String, String)>);

impl Said {
    fn add(&mut self, at: impl Into<String>, text: impl Into<String>) {
        self.0.push((at.into(), text.into()));
    }

    /// The toast the popup is showing, if it is showing one.
    fn add_toast(&mut self, at: &str, popup: &App) {
        if let Some(text) = popup.live_toast() {
            self.add(format!("the toast for {at}"), text);
        }
    }

    /// Whether anything said contains `needle`: a guard that saw nothing guards nothing, so
    /// each test names a string it must have reached.
    fn heard(&self, needle: &str) -> bool {
        self.0.iter().any(|(_, text)| text.contains(needle))
    }

    /// Whether what was said at `at` contains `needle`: for a sentence that two places say, so
    /// that [`Said::heard`] cannot tell which of them was reached.
    fn heard_at(&self, at: &str, needle: &str) -> bool {
        self.0
            .iter()
            .any(|(place, text)| place == at && text.contains(needle))
    }

    /// Every line that breaks the rule, and where it was said.
    fn broken(&self) -> Vec<String> {
        self.0
            .iter()
            .flat_map(|(at, text)| text.lines().map(move |line| (at, line)))
            .filter_map(|(at, line)| {
                let found: Vec<&str> = BANNED
                    .iter()
                    .filter(|(c, _)| line.contains(*c))
                    .map(|(_, name)| *name)
                    .collect();
                (!found.is_empty())
                    .then(|| format!("{at}: {} in {:?}", found.join(", "), line.trim()))
            })
            .collect()
    }

    /// Fails, listing every line that breaks the rule and where it was said.
    fn assert_clean(&self) {
        let broken = self.broken();
        assert!(
            broken.is_empty(),
            "the popup says what it may not:\n{}",
            broken.join("\n")
        );
    }
}

/// The text of a drawn buffer, one string per screen line.
fn lines_of(buffer: &Buffer) -> Vec<String> {
    let area = *buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect()
}

/// One frame of the popup, as text.
fn frame(popup: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
    terminal
        .draw(|f| view::draw(f, popup))
        .expect("drawing must not fail");
    lines_of(terminal.backend().buffer())
}

/// A config parsed from `toml`, at the `all` depth unless it says otherwise.
fn toml_config(toml: &str) -> Config {
    let mut cfg: Config = toml::from_str(toml).expect("the test config must parse");
    if !toml.contains("depth") {
        cfg.ui.depth = Depth::All;
    }
    cfg
}

/// `preset`'s bindings with every action given a key of its own, so the help shows every
/// line it can (the search preset's "act on the row" group only appears once bound).
fn every_action_bound(preset: KeysPreset) -> Keys {
    let table = [
        ("next_attention", "alt+a"),
        ("previous_attention", "alt+b"),
        ("go", "alt+g"),
        ("move_down", "alt+j"),
        ("move_up", "alt+k"),
        ("next_session", "alt+l"),
        ("previous_session", "alt+m"),
        ("filter", "alt+f"),
        ("shallower", "alt+h"),
        ("deeper", "alt+i"),
        ("toggle_flat", "alt+t"),
        ("rename", "alt+r"),
        ("close", "alt+x"),
        ("new_session", "alt+n"),
        ("new_worktree", "alt+w"),
        ("options", "alt+o"),
        ("help", "alt+q"),
        ("quit", "alt+e"),
    ];
    Keys {
        preset,
        bindings: table
            .into_iter()
            .map(|(name, spec)| (name.to_string(), KeySpec::One(spec.to_string())))
            .collect(),
    }
}

/// The help overlay of `preset` under `keys`, drawn on a screen big enough to hold it whole.
fn help_screen(preset: KeysPreset, keys: &Keys) -> Vec<String> {
    help_screen_with(preset, keys, false)
}

/// [`help_screen`] for a popup whose filter does or does not list directories.
fn help_screen_with(preset: KeysPreset, keys: &Keys, projects_in_filter: bool) -> Vec<String> {
    let (keymap, diagnostics) = crate::tui::keys::from_config(keys);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
    let chrome = Chrome {
        theme: &theme,
        keymap: &keymap,
        preset,
        indicators: StatusIndicators::Symbols,
        projects_in_filter,
        numbers_work: preset == KeysPreset::Modal,
        letters_work: true,
        machines: false,
        app: None,
    };
    let mut terminal = Terminal::new(TestBackend::new(220, 60)).expect("a test terminal");
    let mut hits = OverlayHit::default();
    terminal
        .draw(|f| {
            let area = f.area();
            overlays::render(
                f,
                area,
                &chrome,
                &Overlay::Help(Scroll::default()),
                &mut hits,
            );
        })
        .expect("drawing must not fail");
    lines_of(terminal.backend().buffer())
}

#[test]
fn the_help_and_legend_of_both_presets_use_none_of_them() {
    let mut said = Said::default();
    for preset in [KeysPreset::Modal, KeysPreset::Search] {
        let plain = Keys {
            preset,
            bindings: BTreeMap::new(),
        };
        for (label, keys) in [
            ("default keys", plain),
            ("every action bound", every_action_bound(preset)),
        ] {
            let at = format!("the {preset:?} help, {label}");
            // The data: every line's own words, and the key column the keymap gives it.
            let (keymap, _) = crate::tui::keys::from_config(&keys);
            for entry in help_text(preset) {
                match entry {
                    Help::Group(title) => said.add(&at, title),
                    Help::Keys(groups, what) => {
                        said.add(&at, what);
                        if let Some(keys) = help_keys(groups, &keymap) {
                            said.add(&at, keys);
                        }
                    }
                    // Its words, and what the keymap adds to them.
                    Help::Pair { of, what, alone } => {
                        said.add(&at, what);
                        for word in alone {
                            said.add(&at, word);
                        }
                        if let Some(keys) = help_keys(&[&of[..]], &keymap) {
                            said.add(&at, keys);
                        }
                        if let Some(also) = also(of, alone, &keymap) {
                            said.add(&at, also);
                        }
                    }
                    Help::Literal(keys, what) => {
                        said.add(&at, keys);
                        said.add(&at, what);
                    }
                    // Both keys it can name.
                    Help::Machines => {
                        for back in ["Q", "Ctrl-C"] {
                            said.add(&at, crate::machine::words::enter_on_machine_help(back));
                        }
                    }
                    // Both of the chord's wordings, whatever the keymap does with its line.
                    Help::Chord {
                        keys, same, alone, ..
                    } => {
                        said.add(&at, keys);
                        said.add(&at, same);
                        said.add(&at, alone);
                    }
                    Help::State(_, name, what) | Help::Glyph(_, name, what) => {
                        said.add(&at, name);
                        said.add(&at, what);
                    }
                    Help::Note(subject, what) => {
                        said.add(&at, subject);
                        said.add(&at, what);
                    }
                    // Both of the number line's wordings, whatever the keymap binds.
                    Help::Number(subject, keyed, label) => {
                        said.add(&at, subject);
                        said.add(&at, keyed);
                        said.add(&at, label);
                    }
                    Help::NumberKeys(what) | Help::LetterKeys(what) | Help::TypedRow(what) => {
                        said.add(&at, what)
                    }
                    Help::Blank => {}
                }
            }
            // And the box as drawn.
            for line in help_screen(preset, &keys) {
                said.add(format!("{at}, drawn"), line);
            }
        }
    }
    assert!(said.heard("show less / more"), "the help was collected");
    assert!(said.heard("waiting on you"), "and its legend");
    assert!(
        said.heard("straight there without the popup"),
        "and the chord's line, under next up"
    );
    assert!(
        said.heard("next up from anywhere in tmux: what needs you"),
        "and its wording with no next up line above it"
    );
    said.assert_clean();
}

/// The filter's directories (`[ui] projects_in_filter`): what the section
/// says in every state of the filter a user can reach, in both presets on a wide client and
/// a narrow one, and what the help says about it.
#[test]
fn the_filters_directories_and_the_help_that_names_them_use_none_of_them() {
    let mut said = Said::default();
    let home = tempfile::tempdir().unwrap();
    for dir in ["oss/api", "z/quill-pad", "code/github/quill-notes"] {
        std::fs::create_dir_all(home.path().join(dir)).unwrap();
    }
    let projects = home.path().join("code/github");
    for search in [false, true] {
        let mut cfg = if search { search_config() } else { config() };
        cfg.ui.projects_in_filter = true;
        cfg.sessions.project_dirs = vec![projects.clone()];
        for (what, typed) in [
            ("nothing typed", ""),
            ("a query with matches", "quill"),
            ("a query nothing matches", "zzzzzz"),
        ] {
            for (w, h) in [(150, 39), (60, 24)] {
                let mut popup = app(&cfg);
                popup.home = home.path().to_path_buf();
                // The sample's `api` session runs in a directory the filter lists, so
                // `running` is drawn too.
                let mut snap = popup.snap.clone();
                snap.spaces[3].session_path = home.path().join("oss/api");
                popup.set_snapshot(snap);
                popup.set_zoxide(vec!["~/oss/api".to_string(), "~/z/quill-pad".to_string()]);
                if !search {
                    popup.on_key(ch('/'));
                }
                for c in typed.chars() {
                    popup.on_key(ch(c));
                }
                for line in frame(&mut popup, w, h) {
                    said.add(format!("{what}, search {search}, {w}x{h}"), line);
                }
            }
        }
    }
    // The help, in both presets, under the default keys and with every action bound.
    for preset in [KeysPreset::Modal, KeysPreset::Search] {
        let keys = Keys {
            preset,
            bindings: BTreeMap::new(),
        };
        let (keymap, _) = crate::tui::keys::from_config(&keys);
        for entry in help_text_for(preset, true) {
            match entry {
                Help::Keys(groups, what) => {
                    said.add("the help's data", what);
                    if let Some(keys) = help_keys(groups, &keymap) {
                        said.add("the help's data", keys);
                    }
                }
                Help::Pair { of, what, alone } => {
                    said.add("the help's data", what);
                    for word in alone {
                        said.add("the help's data", word);
                    }
                    if let Some(keys) = help_keys(&[&of[..]], &keymap) {
                        said.add("the help's data", keys);
                    }
                    if let Some(also) = also(of, alone, &keymap) {
                        said.add("the help's data", also);
                    }
                }
                Help::Literal(keys, what) => {
                    said.add("the help's data", keys);
                    said.add("the help's data", what);
                }
                Help::Chord {
                    keys, same, alone, ..
                } => {
                    said.add("the help's data", keys);
                    said.add("the help's data", same);
                    said.add("the help's data", alone);
                }
                _ => {}
            }
        }
        for line in help_screen_with(preset, &every_action_bound(preset), true) {
            said.add(format!("the {preset:?} help drawn, directories on"), line);
        }
    }
    assert!(said.heard("open a directory"), "the section's header");
    assert!(said.heard("zoxide"), "and its tags");
    assert!(said.heard("project dir"));
    assert!(said.heard("running"));
    assert!(
        said.heard("directories to open show below"),
        "and the help's line"
    );
    said.assert_clean();
}

#[test]
fn the_hint_line_in_each_of_its_states_uses_none_of_them() {
    let mut said = Said::default();
    let states: [(&str, Config, Option<&str>); 6] = [
        (
            "plain: the sessions depth, no letters",
            toml_config("[ui]\ndepth = \"sessions\"\n"),
            None,
        ),
        ("letters shown", config(), None),
        ("letters in a session of one row", config(), Some("api")),
        (
            "a letter a [keys] entry took",
            toml_config("[ui]\ndepth = \"all\"\n[keys]\nrename = \"b\"\n"),
            None,
        ),
        (
            "numbers and letters off",
            toml_config("[ui]\nlabels = false\ndepth = \"all\"\n"),
            None,
        ),
        ("search preset", search_config(), None),
    ];
    for (what, cfg, header) in &states {
        let mut popup = app(cfg);
        if let Some(session) = header {
            cursor_to_header(&mut popup, session);
        }
        for (key, meaning) in popup.hint_line() {
            said.add(format!("hint line, {what}"), format!("{key} {meaning}"));
        }
        said.add(format!("hint text, {what}"), view::hint_text(&popup));
        // The line as drawn, the last row of the frame.
        let screen = frame(&mut popup, 150, 39);
        said.add(
            format!("the drawn hint line, {what}"),
            screen.last().cloned().unwrap_or_default(),
        );
    }

    // The filter being typed, in the modal preset.
    let cfg = config();
    let mut popup = app(&cfg);
    popup.on_key(ch('/'));
    for c in "no such row".chars() {
        popup.on_key(ch(c));
    }
    let screen = frame(&mut popup, 150, 39);
    said.add(
        "the drawn hint line, a filter being typed",
        screen.last().cloned().unwrap_or_default(),
    );

    // What each overlay answers to, on the same line.
    for overlay in [
        Overlay::Help(Scroll::default()),
        Overlay::Settings(Scroll::default(), String::new()),
        Overlay::Project(Box::new(ProjectPicker::new(
            &cfg,
            Path::new("/home/nobody"),
        ))),
        Overlay::Prompt(Prompt::new(
            PromptKind::NewWorktree {
                workspace: "$1".to_string(),
            },
            "new worktree in web: branch",
            "",
        )),
        Overlay::Confirm(Confirm::new(
            "kill-session api?",
            Action::KillWorkspace {
                session: "$4".to_string(),
            },
        )),
        // The two kinds of question that have a default answer of their own.
        Overlay::Confirm(Confirm::strict(
            "remove anyway?",
            Action::RemoveWorktree {
                workspace: "w6".to_string(),
                force: true,
            },
        )),
        Overlay::Confirm(Confirm::choice(
            "close them too?",
            Action::KillGroup {
                session: "$4".to_string(),
            },
            Action::KillWorkspace {
                session: "$4".to_string(),
            },
        )),
        Overlay::Menu(Menu::new("api", Vec::new(), (0, 0))),
    ] {
        said.add("an overlay's hint line", overlay.hints());
    }
    assert!(said.heard("Tab next up"));
    assert!(said.heard("a-e rows"));
    assert!(said.heard("Enter go"));
    assert!(said.heard("n or Enter just this one"));
    assert!(said.heard("n,\u{a0}Enter\u{a0}or\u{a0}Esc cancel"));
    said.assert_clean();
}

/// The labels of the open menu, and its title.
fn menu_text(popup: &App) -> Vec<String> {
    match &popup.overlay {
        Overlay::Menu(menu) => std::iter::once(menu.title.clone())
            .chain(menu.items.iter().map(|item| item.label.clone()))
            .collect(),
        other => panic!("expected a menu, got {other:?}"),
    }
}

/// Puts the cursor on the row a test names: a pane id, a session's header (`header`), or
/// the parked worktree (`""`).
fn cursor_to(popup: &mut App, place: &str) {
    match place {
        "" => cursor_to_worktree(popup),
        header if !header.starts_with('%') => cursor_to_header(popup, header),
        pane => cursor_to_pane(popup, pane),
    }
}

#[test]
fn the_menus_the_confirmations_and_the_prompts_use_none_of_them() {
    let mut said = Said::default();
    let cfg = config();
    // The sample world's second and third sessions, by the names the sample gives them.
    let sample = snapshot();
    let second = sample.spaces[1].session_name.clone();
    let third = sample.spaces[2].session_name.clone();

    // The right-click menu on every kind of row: an agent, a plain pane, a header and a
    // parked worktree.
    for (what, place) in [
        ("an agent row", "%221"),
        ("a plain pane row", "%211"),
        ("a header", second.as_str()),
        ("a parked worktree", ""),
    ] {
        let mut popup = app(&cfg);
        cursor_to(&mut popup, place);
        popup.on_mouse(click(5, 3, MouseButton::Right));
        for text in menu_text(&popup) {
            said.add(format!("the right-click menu on {what}"), text);
        }
    }

    // The rename chooser, which an agent row asks before it renames anything.
    let mut popup = app(&cfg);
    cursor_to(&mut popup, "%221");
    popup.on_key(ch('r'));
    for text in menu_text(&popup) {
        said.add("the rename chooser", text);
    }

    // The confirmation each kind of close asks: the last pane of a window (which takes the
    // window with it), a pane of a split window, a plain pane, a session, a parked worktree.
    for (what, place) in [
        ("the last pane of a window", "%341"),
        ("a pane of a split window", "%221"),
        ("a plain pane", "%211"),
        ("a session", "api"),
        ("a parked worktree", ""),
    ] {
        let mut popup = app(&cfg);
        cursor_to(&mut popup, place);
        popup.on_key(ch('x'));
        match &popup.overlay {
            Overlay::Confirm(confirm) => said.add(format!("closing {what}"), &confirm.message),
            other => panic!("expected a confirmation for {what}, got {other:?}"),
        }
    }

    // A session with worktree sessions open asks whether to close them too: the question, the
    // line under it, and the frame it is drawn in.
    for names in [
        &["api--fix-a"][..],
        &["api--fix-a", "api--fix-b", "api--fix-c", "api--fix-d"],
    ] {
        let mut popup = app(&cfg);
        with_api_worktrees(&mut popup, names);
        cursor_to_header(&mut popup, "api");
        popup.on_key(ch('x'));
        match &popup.overlay {
            Overlay::Confirm(confirm) => {
                said.add("closing a session with worktree sessions", &confirm.message);
                said.add("the line under that question", confirm.answers.line());
            }
            other => panic!("expected the group question, got {other:?}"),
        }
        for line in frame(&mut popup, 150, 39) {
            said.add("a frame with the group question", line);
        }
    }
    // The same question on the last pane of such a session's last window.
    let mut popup = app(&cfg);
    with_api_worktrees(&mut popup, &["api--fix-a", "api--fix-b"]);
    api_has_one_window(&mut popup);
    cursor_to_pane(&mut popup, "%411");
    popup.on_key(ch('x'));
    match &popup.overlay {
        Overlay::Confirm(confirm) => {
            said.add("closing the last pane of a session", &confirm.message);
            said.add("the line under that question", confirm.answers.line());
        }
        other => panic!("expected the last pane's question, got {other:?}"),
    }
    for line in frame(&mut popup, 150, 39) {
        said.add("a frame with the last pane's question", line);
    }
    // The last pane of a session with nothing cut from it: a plain question, the session's
    // own sentence, and the frame it is drawn in.
    let mut popup = app(&cfg);
    api_has_one_window(&mut popup);
    cursor_to_pane(&mut popup, "%411");
    popup.on_key(ch('x'));
    match &popup.overlay {
        Overlay::Confirm(confirm) => {
            said.add("closing the last pane of a session alone", &confirm.message);
            said.add("the line under that question", confirm.answers.line());
        }
        other => panic!("expected the plain last pane question, got {other:?}"),
    }
    for line in frame(&mut popup, 150, 39) {
        said.add("a frame with the plain last pane question", line);
    }
    // The same last pane in a tmux session group: the sessions grouped with it close as well.
    let mut popup = app(&cfg);
    api_has_one_window(&mut popup);
    api_is_in_a_group_of(&mut popup, 2);
    cursor_to_pane(&mut popup, "%411");
    popup.on_key(ch('x'));
    match &popup.overlay {
        Overlay::Confirm(confirm) => {
            said.add(
                "closing the last pane of a grouped session",
                &confirm.message,
            );
            said.add("the line under that question", confirm.answers.line());
        }
        other => panic!("expected the grouped last pane question, got {other:?}"),
    }
    for line in frame(&mut popup, 150, 39) {
        said.add("a frame with the grouped last pane question", line);
    }
    said.add("the toast for a closed group", group_closed_toast("api", 2));
    said.add(
        "the toast for a closed group of one",
        group_closed_toast("api", 1),
    );
    said.add(
        "the toast for a renamed agent",
        agent_renamed_toast("scribe"),
    );

    // The question a removal git refused raises, for each of the three refusals, and the line
    // under it.
    for why in [
        RemoveRefusal::Dirty,
        RemoveRefusal::Submodules,
        RemoveRefusal::Leftover,
    ] {
        let question = Confirm::strict(
            removal_question(why, "feat/pdf", Path::new("/work/api--feat-pdf")),
            Action::RemoveWorktree {
                workspace: "w6".to_string(),
                force: true,
            },
        );
        said.add(format!("the question for {why:?}"), &question.message);
        said.add("the line under it", question.answers.line());
        let mut popup = app(&cfg);
        popup.ask(question);
        for line in frame(&mut popup, 150, 39) {
            said.add(format!("a frame with the {why:?} question"), line);
        }
    }

    // The prompts' titles, and what each says when its answer is refused.
    let long = "x".repeat(40);
    let prompts: [(&str, &str, &[char]); 4] = [
        ("rename-window", "%211", &['r']),
        ("rename-agent", "%221", &['r', 'a']),
        ("rename-session", second.as_str(), &['r']),
        ("new-worktree", second.as_str(), &['w']),
    ];
    for (what, place, keys) in prompts {
        for typed in [
            "",
            "Upper",
            "a.b",
            long.as_str(),
            third.as_str(),
            second.as_str(),
        ] {
            let mut popup = app(&cfg);
            cursor_to(&mut popup, place);
            for &c in keys {
                popup.on_key(ch(c));
            }
            let Overlay::Prompt(prompt) = &mut popup.overlay else {
                panic!("expected the {what} prompt");
            };
            said.add(format!("the {what} prompt"), &prompt.title);
            prompt.input.set(typed);
            popup.on_key(key(KeyCode::Enter));
            if let Overlay::Prompt(prompt) = &popup.overlay
                && let Some(error) = &prompt.error
            {
                said.add(format!("the {what} prompt refusing {typed:?}"), error);
            }
        }
    }
    assert!(said.heard(&format!("kill-pane {third}:4 (claude)?")));
    assert!(said.heard("git worktree remove"));
    assert!(said.heard("Close them too?"));
    assert!(said.heard("It is the session's last pane, so the session closes with it."));
    assert!(
        said.heard_at(
            "closing the last pane of a session alone",
            "It is the session's last pane, so the session closes with it."
        ),
        "the plain question says it too, and not only the group's"
    );
    assert!(
        said.heard_at(
            "closing the last pane of a grouped session",
            "so the session and the sessions grouped with it close."
        ),
        "the grouped question says the group closes"
    );
    assert!(said.heard("and 1 more"));
    assert!(said.heard("remove anyway? uncommitted changes are lost"));
    assert!(said.heard("it has submodules"));
    assert!(said.heard("part of its folder is still there"));
    assert!(said.heard("closed api and its 2 worktree sessions"));
    assert!(said.heard("rename what?"));
    assert!(said.heard("tmux would rewrite"));
    said.assert_clean();
}

#[test]
fn the_options_overlay_uses_none_of_them() {
    let mut said = Said::default();
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Paths::from_env();
    paths.config_file = dir.path().join("config.toml");
    let bin = Path::new("/usr/local/bin/taw");

    // No config file yet, with a theme that has problems of every kind (which the overlay
    // reports), then a config file, then a config that rebinds keys.
    let dots = toml_config("[ui]\nstatus_indicators = \"dots\"\n");
    let rebound = toml_config(
        "[keys]\npreset = \"search\"\nrename = [\"ctrl-r\", \"alt-r\"]\n[ui.sidebar.agents]\nrows = [[{ token = \"agent\", bold = true }]]\n",
    );
    let bad_theme = BTreeMap::from([
        ("nonsense".to_string(), "#ffffff".to_string()),
        ("accent".to_string(), "not-a-colour".to_string()),
    ]);
    for (what, cfg, name, custom) in [
        ("no config file", &dots, "catppuccin", BTreeMap::new()),
        ("a bad theme", &dots, "nonesuch", bad_theme),
    ] {
        let resolved = theme::resolve(name, &custom);
        said.add(what, settings_text(cfg, &paths, &resolved, bin));
    }
    std::fs::write(&paths.config_file, "[ui]\n").unwrap();
    for (what, cfg) in [("a config file", &dots), ("rebound keys", &rebound)] {
        let resolved = theme::resolve("catppuccin", &BTreeMap::new());
        said.add(what, settings_text(cfg, &paths, &resolved, bin));
    }
    assert!(said.heard("no config file yet"));
    assert!(said.heard("accepted, no effect inside tmux"));
    assert!(said.heard("[onboarding]"));
    said.assert_clean();
}

#[test]
fn the_toasts_use_none_of_them() {
    let mut said = Said::default();
    let cfg = config();
    // The second session of the sample world, by the name the sample gives it.
    let second = snapshot().spaces[1].session_name.clone();

    // A number no session has.
    let mut popup = app(&cfg);
    popup.select_session(9);
    said.add_toast("a number no session has", &popup);

    // A letter the session the cursor is in has no row for: api has one.
    let mut popup = app(&cfg);
    cursor_to_header(&mut popup, "api");
    popup.on_key(ch('b'));
    said.add_toast("a letter with no row", &popup);
    assert!(said.heard("no row b in this session"));

    // Enter and w with nothing in the tree.
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut empty = App::new(&cfg, theme, String::new(), Snapshot::default());
    empty.on_key(key(KeyCode::Enter));
    said.add_toast("Enter with nothing to go to", &empty);
    empty.on_key(ch('w'));
    said.add_toast("w with no session", &empty);

    // Renaming what has no name of its own, and naming what is no agent.
    let mut popup = app(&cfg);
    cursor_to(&mut popup, "");
    popup.on_key(ch('r'));
    said.add_toast("r on a parked worktree", &popup);
    cursor_to(&mut popup, "%211");
    popup.row_action(RowAction::NameAgent);
    said.add_toast("naming a plain pane's agent", &popup);
    cursor_to(&mut popup, &second);
    popup.row_action(RowAction::RenameWindow);
    said.add_toast("renaming a header's window", &popup);

    // A projection the popup ignored.
    let mut snap = snapshot();
    snap.view_error = Some("filter nests deeper than 8 levels".to_string());
    let mut popup = app(&cfg);
    popup.set_snapshot(snap);
    said.add_toast("an ignored projection", &popup);

    // A `[keys]` table that gets everything wrong: each diagnostic, and the toast that
    // names the first.
    let broken = toml_config(
        "[keys]\nnot_an_action = \"x\"\nrename = \"ctrl+wat+r\"\nclose = \"nokey\"\ngo = \"prefix+o\"\nnew_session = \"n\"\nnew_worktree = \"n\"\n",
    );
    let mut popup = app(&broken);
    assert!(
        popup.key_diagnostics.len() >= 4,
        "{:?}",
        popup.key_diagnostics
    );
    for diagnostic in &popup.key_diagnostics {
        said.add("a [keys] diagnostic", diagnostic);
    }
    popup.on_first_frame();
    said.add_toast("a broken [keys] table", &popup);

    // How an agent's name is refused.
    for name in ["", "Upper", "1st", "a.b", "x".repeat(40).as_str()] {
        if let Err(reason) = crate::tui::actions::validate_agent_name(name) {
            said.add("an agent name refused", reason);
        }
    }
    assert!(said.heard("no session numbered 9 is shown"));
    assert!(said.heard("nothing here; n creates a session"));
    assert!(said.heard("a closed worktree keeps its branch name"));
    said.assert_clean();
}

/// Every state of the popup a user can reach, drawn: nothing on any screen holds one.
#[test]
fn no_frame_of_the_popup_shows_one_of_them() {
    let mut said = Said::default();
    let dir = tempfile::tempdir().unwrap();
    let states: Vec<(&str, Config)> = vec![
        ("the tree", config()),
        (
            "the agents depth",
            toml_config("[ui]\ndepth = \"agents\"\n"),
        ),
        (
            "the sessions depth",
            toml_config("[ui]\ndepth = \"sessions\"\n"),
        ),
        ("the flat list", toml_config("[ui]\nlayout = \"flat\"\n")),
        ("dots", toml_config("[ui]\nstatus_indicators = \"dots\"\n")),
        ("the search preset", search_config()),
    ];
    let options = settings_text(
        &Config::default(),
        &Paths::from_env(),
        &theme::resolve("catppuccin", &BTreeMap::new()),
        Path::new("/usr/local/bin/taw"),
    );
    for (what, cfg) in &states {
        // The plain frame, on a wide client and a narrow one.
        for (w, h) in [(150, 39), (60, 24)] {
            let mut popup = app(cfg);
            for line in frame(&mut popup, w, h) {
                said.add(format!("{what}, {w}x{h}"), line);
            }
        }
        // Every overlay the keys open, over the same frame.
        let steps: [(&str, &[char]); 7] = [
            ("help", &['?']),
            ("options", &['o']),
            ("the new-session picker", &['n']),
            ("the new-worktree prompt", &['J', 'w']),
            ("the close confirmation", &['j', 'x']),
            ("the rename chooser", &['r']),
            ("a filter with no match", &['/', 'z', 'q', 'z']),
        ];
        for (overlay, keys) in steps {
            let mut popup = app(cfg);
            popup.home = dir.path().to_path_buf();
            popup.settings = options.clone();
            for &c in keys {
                popup.on_key(ch(c));
            }
            for line in frame(&mut popup, 150, 39) {
                said.add(format!("{what}, {overlay}"), line);
            }
            // The picker with something typed: a path that is not there.
            if matches!(popup.overlay, Overlay::Project(_)) {
                for c in "~/nope".chars() {
                    popup.on_key(ch(c));
                }
                for line in frame(&mut popup, 150, 39) {
                    said.add(format!("{what}, the picker after ~/nope"), line);
                }
            }
        }
    }
    // A popup with nothing in it.
    let cfg = config();
    let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
    let mut popup = App::new(&cfg, theme, String::new(), Snapshot::default());
    for line in frame(&mut popup, 150, 39) {
        said.add("an empty popup", line);
    }
    assert!(said.heard("sessions"));
    assert!(said.heard("need you"));
    said.assert_clean();
}

/// taw's bar shows in the status line while the popup is open, so it is the popup's text too.
#[test]
fn taws_bar_uses_none_of_them() {
    use crate::cli::status_bar::{BarCounts, bar_format};
    let mut said = Said::default();
    let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
    for (sessions, agents) in [(0, 0), (1, 1), (3, 5)] {
        let counts = BarCounts {
            machines: 0,
            sessions,
            agents,
            away: Vec::new(),
        };
        said.add(
            format!("the bar, {sessions} and {agents}"),
            bar_format(&theme, &counts),
        );
    }
    assert!(said.heard("agent"));
    said.assert_clean();
}

// ------------------------------------------------------------------ the source

/// The part of `line` that is code: everything before a `//` comment that is not inside a
/// string literal.
fn code_of(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    let mut previous = None;
    for (at, c) in line.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '/' && previous == Some('/') {
            return &line[..at - 1];
        }
        previous = Some(c);
    }
    line
}

/// Whether `line` opens an inline module: `mod tests {`, or the same behind a visibility.
fn opens_a_module(line: &str) -> bool {
    let mut words = line
        .split_whitespace()
        .skip_while(|word| word.starts_with("pub"));
    words.next() == Some("mod") && line.trim_end().ends_with('{')
}

/// The production code of `source`: every line's code, with its number, up to the inline
/// test module (`#[cfg(test)]` over a `mod tests {`), which ends a file in this crate. Any
/// other `#[cfg(test)]` item is not the end: what follows a test-only helper is production.
fn production_code(source: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = source.lines().collect();
    let end = lines
        .windows(2)
        .position(|pair| pair[0].trim() == "#[cfg(test)]" && opens_a_module(pair[1]))
        .unwrap_or(lines.len());
    lines[..end]
        .iter()
        .enumerate()
        .map(|(at, line)| (at + 1, code_of(line)))
        .collect()
}

/// The lines of `source`'s production code that spell a banned character out, as itself or
/// as the escape a Rust string would use (`\u{2014}`): each one's number, what it spells,
/// and the code.
fn spelled_out(source: &str) -> Vec<(usize, &'static str, &str)> {
    let mut found = Vec::new();
    for (number, code) in production_code(source) {
        for (c, what) in BANNED {
            let escape = format!("\\u{{{:x}}}", c as u32);
            if code.contains(c) || code.to_lowercase().contains(&escape) {
                found.push((number, what, code.trim()));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn a_comment_is_not_code_but_a_slash_pair_in_a_string_is() {
    assert_eq!(code_of("let x = 1; // a note"), "let x = 1; ");
    assert_eq!(code_of("/// a doc line"), "");
    assert_eq!(
        code_of("open(\"https://example.org\"); // and a note"),
        "open(\"https://example.org\"); "
    );
    assert_eq!(code_of("say(\"a \\\" // b\");"), "say(\"a \\\" // b\");");
    let source = "fn a() {}\n// note\n#[cfg(test)]\nmod tests {\nfn b() {}\n}\n";
    let numbers: Vec<usize> = production_code(source).iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, [1, 2], "the inline test module is not production");
}

/// The scan ends where the inline test module begins, `#[cfg(test)]` over a `mod ... {`, and
/// nowhere else. Another `#[cfg(test)]` item, a helper only the tests call, is not the end of
/// the production code, so a string written below it is still read.
#[test]
fn a_test_only_helper_does_not_end_the_scan_but_the_test_module_does() {
    let source = "\
fn a() {}
#[cfg(test)]
fn helper() {
}
fn b() { say(\"\\u{2014}\"); }
#[cfg(test)]
mod tests {
    fn c() { say(\"\\u{2014}\"); }
}
";
    let numbers: Vec<usize> = production_code(source).iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5], "everything above the test module");
    assert_eq!(
        spelled_out(source),
        [(5, "an em-dash", "fn b() { say(\"\\u{2014}\"); }")],
        "the string below the helper is read, the one inside the test module is not"
    );
    // What counts as opening a module, and what does not.
    assert!(opens_a_module("mod tests {"));
    assert!(opens_a_module("    pub(crate) mod tests {"));
    assert!(!opens_a_module("mod tests;"));
    assert!(!opens_a_module("fn helper() {"));
    assert!(!opens_a_module("impl Helper {"));
}

/// The look-alikes are refused too: a horizontal bar and a minus sign
/// where a dash would go, a double arrow and a heavy one. Said in the popup, written in a
/// string in its source, or spelled as the escape a Rust string would use, in either case
/// of its hex digits.
#[test]
fn a_horizontal_bar_a_minus_sign_and_two_more_arrows_are_refused_as_well() {
    for (c, what) in [
        ('\u{2015}', "a horizontal bar"),
        ('\u{2212}', "a minus sign"),
        ('\u{21d2}', "a double arrow"),
        ('\u{279c}', "an arrow"),
    ] {
        let mut said = Said::default();
        said.add("a frame", format!("go {c} there"));
        assert_eq!(
            said.broken(),
            [format!("a frame: {what} in \"go {c} there\"")]
        );

        let source = format!("fn a() {{ say(\"x {c} y\"); }}\n");
        assert_eq!(
            spelled_out(&source),
            [(1, what, source.trim())],
            "{c:?} as itself"
        );
        for escape in [
            format!("\\u{{{:x}}}", c as u32),
            format!("\\u{{{:X}}}", c as u32),
        ] {
            let source = format!("fn a() {{ say(\"{escape}\"); }}\n");
            assert_eq!(spelled_out(&source), [(1, what, source.trim())], "{escape}");
        }
    }
}

/// A source string that spells one of the characters out, as itself or as the escape a
/// Rust string would use (`\u{2014}`), is refused, wherever it sits in taw's code: the
/// popup's, and every message, error, notification and help string besides.
#[test]
fn no_string_in_taws_source_spells_one_of_them_out() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    files.sort();
    assert!(
        files.len() > 100,
        "taw's source was found: {} files",
        files.len()
    );
    let mut broken = Vec::new();
    for file in files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // Tests, their fixtures and this guard may name them: they are how the rule is
        // checked, and a fake agent's screen draws what the real one does.
        if [
            "tests.rs",
            "wording.rs",
            "fixtures.rs",
            "test_support.rs",
            "sim.rs",
        ]
        .contains(&name)
        {
            continue;
        }
        let source = std::fs::read_to_string(&file).expect("a source file");
        for (number, what, code) in spelled_out(&source) {
            broken.push(format!(
                "{}:{number}: {what}: {code}",
                file.strip_prefix(&root).unwrap_or(&file).display(),
            ));
        }
    }
    assert!(
        broken.is_empty(),
        "taw's source says what it may not:\n{}",
        broken.join("\n")
    );
}
