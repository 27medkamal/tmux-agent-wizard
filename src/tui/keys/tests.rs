//! Preset lookups, the herdr key grammar, and `[keys]` override semantics.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::config::{KeySpec, Keys, KeysPreset};

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn bare(code: KeyCode) -> KeyEvent {
    key(code, KeyModifiers::NONE)
}

fn keys_with(preset: KeysPreset, bindings: &[(&str, &str)]) -> Keys {
    let mut map = BTreeMap::new();
    for (name, spec) in bindings {
        map.insert((*name).to_string(), KeySpec::One((*spec).to_string()));
    }
    Keys {
        preset,
        bindings: map,
    }
}

#[test]
fn modal_preset_lookups() {
    let km = preset(KeysPreset::Modal);
    assert_eq!(km.lookup(bare(KeyCode::Char('j'))), Some(Action::MoveDown));
    assert_eq!(
        km.lookup(bare(KeyCode::Char('J'))),
        Some(Action::NextSession)
    );
    assert_eq!(km.lookup(bare(KeyCode::Char('?'))), Some(Action::Help));
    assert_eq!(
        km.lookup(bare(KeyCode::BackTab)),
        Some(Action::PreviousAttention)
    );
    // A terminal that also reports SHIFT alongside BackTab or an uppercase Char must still
    // match: the keymap treats `Char('J')` with or without SHIFT identically.
    assert_eq!(
        km.lookup(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
        Some(Action::PreviousAttention)
    );
    assert_eq!(
        km.lookup(key(KeyCode::Char('J'), KeyModifiers::SHIFT)),
        Some(Action::NextSession)
    );
    assert_eq!(km.lookup(bare(KeyCode::Tab)), Some(Action::NextAttention));
    assert_eq!(km.lookup(bare(KeyCode::Enter)), Some(Action::Go));
    assert_eq!(km.lookup(bare(KeyCode::Down)), Some(Action::MoveDown));
    assert_eq!(km.lookup(bare(KeyCode::Up)), Some(Action::MoveUp));
    assert_eq!(km.lookup(bare(KeyCode::Char('h'))), Some(Action::Shallower));
    assert_eq!(km.lookup(bare(KeyCode::Char('l'))), Some(Action::Deeper));
    assert_eq!(
        km.lookup(bare(KeyCode::Char('f'))),
        Some(Action::ToggleFlat)
    );
    assert_eq!(km.lookup(bare(KeyCode::Char('r'))), Some(Action::Rename));
    assert_eq!(km.lookup(bare(KeyCode::Char('x'))), Some(Action::Close));
    // Closing a window, splits and a new window are tmux's own keys.
    for gone in ['X', 's', 'v', 'c'] {
        assert_eq!(km.lookup(bare(KeyCode::Char(gone))), None, "{gone}");
    }
    assert_eq!(
        km.lookup(bare(KeyCode::Char('n'))),
        Some(Action::NewSession)
    );
    assert_eq!(
        km.lookup(bare(KeyCode::Char('w'))),
        Some(Action::NewWorktree)
    );
    assert_eq!(km.lookup(bare(KeyCode::Char('o'))), Some(Action::Options));
    assert_eq!(km.lookup(bare(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(km.lookup(bare(KeyCode::Esc)), Some(Action::Quit));
}

#[test]
fn search_preset_lookups() {
    let km = preset(KeysPreset::Search);
    let ctrl = KeyModifiers::CONTROL;
    assert_eq!(km.lookup(bare(KeyCode::Enter)), Some(Action::Go));
    assert_eq!(km.lookup(bare(KeyCode::Esc)), Some(Action::Quit));
    assert_eq!(km.lookup(bare(KeyCode::Down)), Some(Action::MoveDown));
    assert_eq!(km.lookup(bare(KeyCode::Up)), Some(Action::MoveUp));
    assert_eq!(
        km.lookup(key(KeyCode::Char('n'), ctrl)),
        Some(Action::MoveDown),
        "ctrl-n moves down, as in fzf; it does not open the project picker"
    );
    assert_eq!(
        km.lookup(key(KeyCode::Char('p'), ctrl)),
        Some(Action::MoveUp)
    );
    // No chord beyond fzf's own is bound.
    for (code, mods) in [
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::BackTab, KeyModifiers::NONE),
        (KeyCode::Up, ctrl),
        (KeyCode::Down, ctrl),
        (KeyCode::Left, ctrl),
        (KeyCode::Right, ctrl),
        (KeyCode::Char('t'), ctrl),
        (KeyCode::Char('x'), ctrl),
        (KeyCode::Char('r'), ctrl),
        (KeyCode::Char('o'), ctrl),
        (KeyCode::Char('/'), ctrl),
    ] {
        assert_eq!(km.lookup(key(code, mods)), None, "{code:?} {mods:?}");
    }
    // Typing keys are not bound to anything in the search preset.
    assert_eq!(km.lookup(bare(KeyCode::Char('j'))), None);
}

/// The search preset is a pure fzf search: Enter goes, Esc quits, and the arrows, ctrl-j /
/// ctrl-k and ctrl-n / ctrl-p move. Every key it binds is one of those, and each of those
/// is bound.
#[test]
fn the_search_preset_binds_fzfs_own_keys_and_nothing_else() {
    let km = preset(KeysPreset::Search);
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    let fzf: HashMap<(KeyCode, KeyModifiers), Action> = [
        ((KeyCode::Enter, none), Action::Go),
        ((KeyCode::Esc, none), Action::Quit),
        ((KeyCode::Up, none), Action::MoveUp),
        ((KeyCode::Down, none), Action::MoveDown),
        ((KeyCode::Char('k'), ctrl), Action::MoveUp),
        ((KeyCode::Char('j'), ctrl), Action::MoveDown),
        ((KeyCode::Char('p'), ctrl), Action::MoveUp),
        ((KeyCode::Char('n'), ctrl), Action::MoveDown),
    ]
    .into_iter()
    .collect();
    for (&(code, mods), &action) in &km.map {
        assert_eq!(
            fzf.get(&(code, mods)),
            Some(&action),
            "{} is bound to {action:?}",
            display_key(code, mods)
        );
    }
    assert_eq!(km.map.len(), fzf.len(), "{:?}", km.map);
}

/// A `[keys]` table written for the search preset before it lost its chords still binds
/// what it names: an entry is an override, whatever the preset has, and none of these is
/// anything to report.
#[test]
fn a_keys_entry_for_a_chord_the_search_preset_no_longer_has_is_still_just_an_override() {
    let cfg = keys_with(
        KeysPreset::Search,
        &[
            ("close", "ctrl+x"),
            ("help", "ctrl+/"),
            ("options", "ctrl+o"),
            ("toggle_flat", "ctrl+t"),
        ],
    );
    let (km, diagnostics) = from_config(&cfg);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let ctrl = KeyModifiers::CONTROL;
    assert_eq!(
        km.lookup(key(KeyCode::Char('x'), ctrl)),
        Some(Action::Close)
    );
    assert_eq!(km.lookup(key(KeyCode::Char('/'), ctrl)), Some(Action::Help));
    assert_eq!(
        km.lookup(key(KeyCode::Char('o'), ctrl)),
        Some(Action::Options)
    );
    assert_eq!(
        km.lookup(key(KeyCode::Char('t'), ctrl)),
        Some(Action::ToggleFlat)
    );
}

#[test]
fn keys_for_reports_sorted_display_names() {
    let km = preset(KeysPreset::Modal);
    assert_eq!(
        km.keys_for(Action::MoveDown),
        vec!["Down".to_string(), "j".to_string()]
    );
    assert_eq!(
        km.keys_for(Action::PreviousAttention),
        vec!["Shift-Tab".to_string()]
    );
    assert_eq!(km.keys_for(Action::Help), vec!["?".to_string()]);
}

/// `short_event` is the key `short_key` names, as the event pressing it sends: what a label that
/// names it asks the reader to press. `j` over `Down`, `Down` over ctrl-j and ctrl-n (the
/// shortest name), a digit for a number, nothing for an action nothing is bound to.
#[test]
fn short_event_is_the_key_short_key_names() {
    let km = preset(KeysPreset::Modal);
    assert_eq!(km.short_key(Action::MoveDown).as_deref(), Some("j"));
    assert_eq!(
        km.short_event(Action::MoveDown),
        Some(bare(KeyCode::Char('j')))
    );
    assert_eq!(
        km.short_event(Action::SelectSession(3)),
        Some(bare(KeyCode::Char('3')))
    );
    let search = preset(KeysPreset::Search);
    assert_eq!(search.short_key(Action::MoveDown).as_deref(), Some("Down"));
    assert_eq!(
        search.short_event(Action::MoveDown),
        Some(bare(KeyCode::Down))
    );
    assert_eq!(search.short_event(Action::SelectSession(3)), None);
    assert_eq!(search.short_event(Action::Help), None);
}

#[test]
fn keys_for_search_preset_uses_ctrl_display_names() {
    let km = preset(KeysPreset::Search);
    assert_eq!(
        km.keys_for(Action::MoveDown),
        vec!["Down", "ctrl-j", "ctrl-n"]
    );
    assert_eq!(km.keys_for(Action::MoveUp), vec!["Up", "ctrl-k", "ctrl-p"]);
    assert!(km.keys_for(Action::Help).is_empty());
    assert!(km.keys_for(Action::Close).is_empty());
}

#[test]
fn parse_key_grammar() {
    assert_eq!(
        parse_key("ctrl+x"),
        Ok(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
    );
    assert_eq!(parse_key("shift+tab"), Ok(bare(KeyCode::BackTab)));
    assert_eq!(
        parse_key("alt+enter"),
        Ok(key(KeyCode::Enter, KeyModifiers::ALT))
    );
    // A single uppercase letter already implies shift.
    assert_eq!(parse_key("R"), Ok(bare(KeyCode::Char('R'))));
    assert_eq!(parse_key("shift+r"), Ok(bare(KeyCode::Char('R'))));
    assert_eq!(parse_key("/"), Ok(bare(KeyCode::Char('/'))));
    assert_eq!(parse_key("slash"), Ok(bare(KeyCode::Char('/'))));
    assert_eq!(parse_key("question"), Ok(bare(KeyCode::Char('?'))));
    assert_eq!(parse_key("tab"), Ok(bare(KeyCode::Tab)));
    assert_eq!(parse_key("escape"), Ok(bare(KeyCode::Esc)));
    assert_eq!(parse_key("esc"), Ok(bare(KeyCode::Esc)));
    assert_eq!(
        parse_key("ctrl+alt+x"),
        Ok(key(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ))
    );
}

#[test]
fn parse_key_rejects_prefix_chords() {
    assert_eq!(
        parse_key("prefix+o"),
        Err("prefix chords are tmux's; set @agent-wizard-notification-key instead".to_string())
    );
}

#[test]
fn parse_key_rejects_unknown_specs() {
    assert!(parse_key("nonsense").is_err());
    assert!(parse_key("").is_err());
    assert!(parse_key("ctrl+nonsense").is_err());
}

#[test]
fn override_replaces_the_preset_key_rather_than_adding_to_it() {
    let cfg = keys_with(KeysPreset::Modal, &[("rename", "R")]);

    let (km, diagnostics) = from_config(&cfg);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(km.keys_for(Action::Rename), vec!["R".to_string()]);
    assert_eq!(km.lookup(bare(KeyCode::Char('r'))), None);
    assert_eq!(km.lookup(bare(KeyCode::Char('R'))), Some(Action::Rename));
}

/// A multi-key override (`KeySpec::Many`) also fully
/// replaces the preset's keys for that action, not just a single-key one.
#[test]
fn a_multi_key_override_also_replaces_rather_than_adds() {
    let mut map = BTreeMap::new();
    map.insert(
        "move_down".to_string(),
        KeySpec::Many(vec!["z".to_string(), "down".to_string()]),
    );
    let cfg = Keys {
        preset: KeysPreset::Modal,
        bindings: map,
    };

    let (km, diagnostics) = from_config(&cfg);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let mut down = km.keys_for(Action::MoveDown);
    down.sort();
    assert_eq!(down, vec!["Down".to_string(), "z".to_string()]);
    assert_eq!(km.lookup(bare(KeyCode::Char('j'))), None);
}

#[test]
fn a_key_claimed_by_two_actions_keeps_the_preset_binding() {
    // "r" is Rename's own preset key in the modal preset; binding Close to the same key
    // must lose to it, since the preset binding is always kept on a contest. Close's own
    // override is then completely empty (every key it asked for was someone else's), so it
    // must fall back to its own preset key ('x') rather than end up with nothing at all —
    // and the diagnostic must say so, not just report the collision.
    let cfg = keys_with(KeysPreset::Modal, &[("close", "r")]);

    let (km, diagnostics) = from_config(&cfg);

    assert_eq!(km.lookup(bare(KeyCode::Char('r'))), Some(Action::Rename));
    assert_eq!(km.lookup(bare(KeyCode::Char('x'))), Some(Action::Close));
    assert_eq!(km.keys_for(Action::Close), vec!["x".to_string()]);
    assert_eq!(
        diagnostics,
        vec![
            "keys: \"r\" is bound to both close and rename; keeping rename; close keeps its default keys"
                .to_string()
        ]
    );
}

#[test]
fn an_override_that_does_not_parse_leaves_the_preset_keys() {
    // The README's own example: a herdr line pasted as it is. `goto` is the filter action,
    // and `prefix+g` is refused, so `/` must still open the filter, and the diagnostic must
    // say the action kept its keys.
    let cfg = keys_with(KeysPreset::Modal, &[("goto", "prefix+g")]);

    let (km, diagnostics) = from_config(&cfg);

    assert_eq!(km.lookup(bare(KeyCode::Char('/'))), Some(Action::Filter));
    assert_eq!(km.keys_for(Action::Filter), vec!["/".to_string()]);
    assert_eq!(
        diagnostics,
        vec![
            "keys: \"prefix+g\": prefix chords are tmux's; set @agent-wizard-notification-key \
             instead; filter keeps its default keys"
                .to_string()
        ]
    );

    // A typo of any other kind is the same.
    let (km, diagnostics) = from_config(&keys_with(KeysPreset::Modal, &[("rename", "ctrl+zz")]));
    assert_eq!(km.keys_for(Action::Rename), vec!["r".to_string()]);
    assert!(
        diagnostics[0].ends_with("; rename keeps its default keys"),
        "{diagnostics:?}"
    );

    // An empty list is not a typo: it unbinds on purpose.
    let mut keys = Keys::default();
    keys.bindings
        .insert("rename".to_string(), KeySpec::Many(Vec::new()));
    let (km, diagnostics) = from_config(&keys);
    assert!(km.keys_for(Action::Rename).is_empty());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn a_select_session_override_that_does_not_parse_keeps_its_defaults() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec!["prefix+1".to_string()]),
    );
    let (map, diagnostics) = from_config(&keys);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].ends_with("; select_session keeps its default keys"));
    for n in 1..=9u8 {
        assert_eq!(
            map.lookup(bare(KeyCode::Char(char::from(b'0' + n)))),
            Some(Action::SelectSession(n)),
            "digit {n}"
        );
    }
}

#[test]
fn unknown_action_name_is_a_diagnostic() {
    let cfg = keys_with(KeysPreset::Modal, &[("not_a_real_action", "z")]);

    let (_, diagnostics) = from_config(&cfg);

    assert_eq!(
        diagnostics,
        vec!["keys: unknown action \"not_a_real_action\"".to_string()]
    );
}

#[test]
fn herdr_aliases_map_to_their_taw_action() {
    let cfg = keys_with(KeysPreset::Modal, &[("goto", "g")]);

    let (km, diagnostics) = from_config(&cfg);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(km.lookup(bare(KeyCode::Char('g'))), Some(Action::Filter));
    // Filter's own preset key is gone: the override (via the alias) replaced it.
    assert_eq!(km.lookup(bare(KeyCode::Char('/'))), None);
}

#[test]
fn more_herdr_aliases() {
    // "0" rather than "9": the modal preset binds "1".."9" to select_session (see
    // `the_modal_preset_binds_the_digits_to_select_session`), so "9" would contest with
    // it here instead of exercising a clean, uncontested override.
    for (alias, canonical) in [
        ("workspace_picker", Action::Filter),
        ("new_workspace", Action::NewSession),
        ("rename_workspace", Action::Rename),
        ("close_workspace", Action::Close),
        ("next_workspace", Action::NextSession),
        ("previous_workspace", Action::PreviousSession),
        ("settings", Action::Options),
    ] {
        let cfg = keys_with(KeysPreset::Modal, &[(alias, "0")]);
        let (km, diagnostics) = from_config(&cfg);
        assert!(diagnostics.is_empty(), "{alias}: {diagnostics:?}");
        assert_eq!(
            km.lookup(bare(KeyCode::Char('0'))),
            Some(canonical),
            "{alias}"
        );
    }
}

#[test]
fn the_modal_preset_binds_the_digits_to_select_session() {
    let map = preset(KeysPreset::Modal);
    for n in 1..=9u8 {
        let key = KeyEvent::new(KeyCode::Char(char::from(b'0' + n)), KeyModifiers::NONE);
        assert_eq!(map.lookup(key), Some(Action::SelectSession(n)), "digit {n}");
    }
    assert_eq!(
        map.lookup(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(map.label(3), "3");
    assert_eq!(map.select_session_hint().as_deref(), Some("1-9"));
}

#[test]
fn the_search_preset_leaves_the_digits_for_typing() {
    let map = preset(KeysPreset::Search);
    assert_eq!(
        map.lookup(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
        None
    );
    // Labels are still drawn as digits, but no key selects a session and no hint names
    // one: a digit typed here is text for the filter.
    assert_eq!(map.label(3), "3");
    assert_eq!(map.select_session_hint(), None);
}

#[test]
fn select_session_override_binds_in_order_and_relabels() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec!["a".to_string(), "x".to_string(), "d".to_string()]),
    );
    let (map, diagnostics) = from_config(&keys);
    // "x" is close in the modal preset: first claim keeps it, so "x" is reported and the
    // second session keeps no key of its own.
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("\"x\"") && d.contains("select_session")),
        "{diagnostics:?}"
    );
    assert_eq!(
        map.lookup(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        Some(Action::SelectSession(1))
    );
    assert_eq!(
        map.lookup(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        Some(Action::SelectSession(3))
    );
    assert_eq!(map.label(1), "a");
    assert_eq!(
        map.label(2),
        "2",
        "an unbound label falls back to its digit"
    );
    assert_eq!(map.label(3), "d");
    // The preset's digits are gone: an override replaces, as for every action.
    assert_eq!(
        map.lookup(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(map.select_session_hint().as_deref(), Some("a-d"));
}

/// A single key is a whole `select_session` override: one session gets a key, the other
/// eight fall back to their digit on the row and no key at all.
#[test]
fn select_session_with_one_key_binds_only_the_first_session() {
    let mut keys = Keys::default();
    keys.bindings
        .insert("select_session".to_string(), KeySpec::One("a".to_string()));
    let (map, diagnostics) = from_config(&keys);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(
        map.lookup(bare(KeyCode::Char('a'))),
        Some(Action::SelectSession(1))
    );
    assert_eq!(map.label(1), "a");
    assert_eq!(
        map.label(2),
        "2",
        "an unbound label falls back to its digit"
    );
    assert_eq!(
        map.select_session_hint().as_deref(),
        Some("a"),
        "one bound key names itself rather than a run"
    );
}

/// There are nine labels, so a tenth key has no session to select. It is dropped, not a
/// panic and not a tenth `SelectSession`.
#[test]
fn a_tenth_select_session_key_is_ignored() {
    let mut keys = Keys::default();
    let ten: Vec<String> = ["a", "b", "d", "e", "g", "i", "m", "p", "t", "u"]
        .iter()
        .map(|k| (*k).to_string())
        .collect();
    keys.bindings
        .insert("select_session".to_string(), KeySpec::Many(ten));
    let (map, diagnostics) = from_config(&keys);

    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(
        map.lookup(bare(KeyCode::Char('t'))),
        Some(Action::SelectSession(9)),
        "the ninth key is the last one that selects anything"
    );
    assert_eq!(
        map.lookup(bare(KeyCode::Char('u'))),
        None,
        "the tenth binds nothing at all"
    );
}

/// A key that fails the grammar is reported and costs only its own slot: the keys around
/// it still bind, and the session it would have named keeps its digit.
#[test]
fn an_unparsable_select_session_key_is_reported_and_the_others_still_bind() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec!["a".to_string(), "ctrl+".to_string(), "d".to_string()]),
    );
    let (map, diagnostics) = from_config(&keys);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("ctrl+"), "{diagnostics:?}");
    assert!(
        !diagnostics[0].contains("keeps its default keys"),
        "two keys did bind, so nothing fell back: {diagnostics:?}"
    );
    assert_eq!(
        map.lookup(bare(KeyCode::Char('a'))),
        Some(Action::SelectSession(1))
    );
    assert_eq!(
        map.lookup(bare(KeyCode::Char('d'))),
        Some(Action::SelectSession(3)),
        "the third key still names the third session"
    );
    assert_eq!(map.label(2), "2", "the slot it failed in keeps its digit");
}

/// Every key of the override was already someone else's, so `select_session` would have
/// been left answering to nothing at all. It keeps the keys it had — the preset's digits —
/// and the last diagnostic says so, exactly as the generic override path does.
#[test]
fn a_select_session_override_that_wins_no_key_keeps_its_defaults() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec!["r".to_string(), "x".to_string()]),
    );
    let (map, diagnostics) = from_config(&keys);

    assert_eq!(
        diagnostics,
        vec![
            "keys: \"r\" is bound to both rename and select_session; keeping rename".to_string(),
            "keys: \"x\" is bound to both close and select_session; keeping close\
             ; select_session keeps its default keys"
                .to_string(),
        ]
    );
    for n in 1..=9u8 {
        assert_eq!(
            map.lookup(bare(KeyCode::Char(char::from(b'0' + n)))),
            Some(Action::SelectSession(n)),
            "digit {n}"
        );
    }
    assert_eq!(map.label(1), "1");
    assert_eq!(map.select_session_hint().as_deref(), Some("1-9"));
    assert_eq!(
        map.lookup(bare(KeyCode::Char('r'))),
        Some(Action::Rename),
        "and the action that already owned the key still owns it"
    );
}

#[test]
fn action_names_round_trip_for_select_session() {
    assert_eq!(action_name(Action::SelectSession(4)), "select_session");
}

/// `select_session`'s list is positional, so a typo in one entry used to leave that session
/// with no number key at all. The entry that does not parse keeps its own session's preset
/// key; the others bind as given, and the diagnostic names the session.
#[test]
fn a_select_session_entry_that_does_not_parse_keeps_that_sessions_preset_key() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec![
            "1".to_string(),
            "prefix+2".to_string(),
            "3".to_string(),
        ]),
    );
    let (map, diagnostics) = from_config(&keys);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("\"prefix+2\""), "{diagnostics:?}");
    assert!(
        diagnostics[0].ends_with("; session 2 keeps its default key"),
        "{diagnostics:?}"
    );
    for n in [1u8, 2, 3] {
        assert_eq!(
            map.lookup(bare(KeyCode::Char(char::from(b'0' + n)))),
            Some(Action::SelectSession(n)),
            "digit {n}"
        );
    }
    assert_eq!(
        map.lookup(bare(KeyCode::Char('4'))),
        None,
        "sessions past the list stay unbound, as a shorter list says"
    );
}

/// A spec with a key that does not parse and a key another action holds: the check still
/// tells that the action kept its preset keys, on the line it does report.
#[test]
fn the_fallback_note_goes_on_the_key_that_did_not_parse_when_a_contested_key_follows() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "rename".to_string(),
        KeySpec::Many(vec!["prefix+r".to_string(), "j".to_string()]),
    );
    let (_, diagnostics) = resolve(&keys);

    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert_eq!(diagnostics[0].kind, DiagnosticKind::Unparsed);
    assert!(
        diagnostics[0]
            .message
            .ends_with("; rename keeps its default keys"),
        "{diagnostics:?}"
    );
    assert_eq!(diagnostics[1].kind, DiagnosticKind::Contested);
}

/// The preset key of a refused entry goes back after every entry has been read, so an
/// explicit entry wins in either order and never meets its own action as a contest.
#[test]
fn an_explicit_select_session_entry_wins_the_key_in_either_order() {
    let mut keys = Keys::default();
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec![
            "prefix+1".to_string(),
            "1".to_string(),
            "3".to_string(),
        ]),
    );
    let (map, diagnostics) = from_config(&keys);

    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("\"prefix+1\""), "{diagnostics:?}");
    assert!(
        !diagnostics[0].contains("keeps its default key")
            && !diagnostics[0].contains("bound to both"),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics[0].ends_with("; session 1 is left unbound"),
        "{diagnostics:?}"
    );
    assert_eq!(
        map.lookup(bare(KeyCode::Char('1'))),
        Some(Action::SelectSession(2))
    );
    assert_eq!(
        map.lookup(bare(KeyCode::Char('3'))),
        Some(Action::SelectSession(3))
    );
    assert_eq!(map.lookup(bare(KeyCode::Char('2'))), None);

    // The mirror order gives the same kind of result: the user's `2` is session 1's key.
    keys.bindings.insert(
        "select_session".to_string(),
        KeySpec::Many(vec!["2".to_string(), "prefix+2".to_string()]),
    );
    let (map, diagnostics) = from_config(&keys);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        map.lookup(bare(KeyCode::Char('2'))),
        Some(Action::SelectSession(1))
    );
}
