//! Action keymap: presets, `[keys]` overrides, and herdr's key grammar.
//!
//! An [`Action`] is what a key means, independent of which physical key produces it — the
//! run loop turns a [`crossterm::event::KeyEvent`] into an `Action` here, then decides what
//! that action does. A [`Keymap`] is a resolved set of bindings: [`preset`] builds the two
//! built-in ones, and [`from_config`]
//! layers a config's `[keys]` table of overrides on top of one. [`parse_key`]
//! is herdr's own key-spec grammar, used both for a `[keys]` entry's value and, by
//! [`super::prefix`], nowhere directly — that module has tmux's own key spellings instead.
//!
//! A terminal is not always consistent about whether an uppercase letter or `BackTab`
//! (Shift-Tab) also carries an explicit `KeyModifiers::SHIFT` alongside the key itself, since
//! both already fully encode their own shifted-ness. Every key this module stores or looks up
//! is normalized to strip that redundant bit first, so `Keymap::lookup` treats `Char('J')`
//! with or without `SHIFT` identically, and so does a `[keys]` binding's `R` versus `shift+r`.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Keys, KeysPreset};
use crate::tui::app::JUMP_LETTERS;

/// Something a key can ask the popup to do. Independent of any physical key: [`preset`] and
/// [`from_config`] are what decide which key produces which `Action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    NextAttention,
    PreviousAttention,
    Go,
    MoveDown,
    MoveUp,
    NextSession,
    PreviousSession,
    Filter,
    Shallower,
    Deeper,
    ToggleFlat,
    Rename,
    Close,
    NewSession,
    NewWorktree,
    Options,
    /// Paste the image on this machine's clipboard into the cursor row's agent, on whichever machine.
    PasteImage,
    Help,
    Quit,
    /// Move the cursor to the session labelled `n` (1–9). The one action with a payload:
    /// `[keys] select_session` is an ordered list, the nth key binding `SelectSession(n)`.
    SelectSession(u8),
}

/// Every action's canonical `[keys]` name, the single source of truth for
/// both [`action_by_name`] and [`action_name`].
const ACTIONS: &[(&str, Action)] = &[
    ("next_attention", Action::NextAttention),
    ("previous_attention", Action::PreviousAttention),
    ("go", Action::Go),
    ("move_down", Action::MoveDown),
    ("move_up", Action::MoveUp),
    ("next_session", Action::NextSession),
    ("previous_session", Action::PreviousSession),
    ("filter", Action::Filter),
    ("shallower", Action::Shallower),
    ("deeper", Action::Deeper),
    ("toggle_flat", Action::ToggleFlat),
    ("rename", Action::Rename),
    ("close", Action::Close),
    ("new_session", Action::NewSession),
    ("new_worktree", Action::NewWorktree),
    ("options", Action::Options),
    ("paste_image", Action::PasteImage),
    ("help", Action::Help),
    ("quit", Action::Quit),
];

/// herdr's alternate names for a taw action, e.g. `goto` for `filter`.
/// `open_notification_target` is deliberately absent: it names the prefix chord's key, read
/// from tmux (see [`super::prefix`]), never a `[keys]` entry.
pub const ALIASES: &[(&str, &str)] = &[
    ("goto", "filter"),
    ("workspace_picker", "filter"),
    ("new_workspace", "new_session"),
    ("rename_workspace", "rename"),
    ("close_workspace", "close"),
    ("next_workspace", "next_session"),
    ("previous_workspace", "previous_session"),
    ("settings", "options"),
    ("help", "help"),
    ("new_worktree", "new_worktree"),
];

/// Resolves a `[keys]` table key to the `Action` it overrides: a canonical name directly, or
/// a herdr alias one step removed.
fn action_by_name(name: &str) -> Option<Action> {
    let canonical = match ACTIONS.iter().find(|(n, _)| *n == name) {
        Some((_, action)) => return Some(*action),
        None => ALIASES.iter().find(|(alias, _)| *alias == name)?.1,
    };
    ACTIONS
        .iter()
        .find(|(n, _)| *n == canonical)
        .map(|(_, action)| *action)
}

/// `action`'s canonical `[keys]` name, for diagnostics.
fn action_name(action: Action) -> &'static str {
    match action {
        Action::SelectSession(_) => "select_session",
        _ => ACTIONS
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(name, _)| *name)
            .expect("every Action has a canonical name in ACTIONS"),
    }
}

/// Strips a `SHIFT` some terminals report alongside an uppercase `Char` or `BackTab`
/// (Shift-Tab), both of which already fully encode their own shifted-ness. Every other key
/// code's modifiers pass through untouched. `pub(crate)` because [`super::prefix::Chord`]
/// normalizes the tmux prefix chord's own keys the same way — one rule, shared, rather than
/// two independent copies that could drift.
pub(crate) fn normalize(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::Char(_) | KeyCode::BackTab => {
            KeyEvent::new(key.code, key.modifiers.difference(KeyModifiers::SHIFT))
        }
        _ => key,
    }
}

/// A key's display name, e.g. `"Tab"`, `"Shift-Tab"`, `"ctrl-x"`. `mods` is assumed already
/// [`normalize`]d.
fn display_key(code: KeyCode, mods: KeyModifiers) -> String {
    let mut out = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl-");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("alt-");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        out.push_str("shift-");
    }
    out.push_str(&match code {
        KeyCode::BackTab => "Shift-Tab".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    });
    out
}

/// A resolved set of key bindings: every physical key the popup recognises, mapped to the
/// [`Action`] it performs. Built by [`preset`] or [`from_config`], never constructed by hand.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
}

impl Keymap {
    /// The action bound to `key`, if any.
    pub fn lookup(&self, key: KeyEvent) -> Option<Action> {
        let normalized = normalize(key);
        self.map
            .get(&(normalized.code, normalized.modifiers))
            .copied()
    }

    /// Every key bound to `action`, as display names (e.g. `"Tab"`, `"Shift-Tab"`,
    /// `"ctrl-x"`), sorted for a deterministic order.
    pub fn keys_for(&self, action: Action) -> Vec<String> {
        self.bound(action)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The one key to name `action` by in a hint or the help: the shortest of its
    /// bindings, so `j` wins over `Down` and `q` over `Esc`. `None` when nothing is bound
    /// to it. Shared, so the hint line and the help overlay cannot name the same action
    /// two different ways.
    pub fn short_key(&self, action: Action) -> Option<String> {
        self.short(action).map(|(name, _)| name)
    }

    /// The key [`Keymap::short_key`] names, as the event pressing it sends: what a label that
    /// names it asks the reader to press, to tell whether that key works where the popup is.
    pub fn short_event(&self, action: Action) -> Option<KeyEvent> {
        self.short(action).map(|(_, event)| event)
    }

    /// [`Keymap::short_key`] among the keys `keep` admits: the key to name `action` by where only
    /// some keys act, as in the modal filter, which takes every plain character as text.
    pub fn short_key_where(
        &self,
        action: Action,
        keep: impl Fn(KeyEvent) -> bool,
    ) -> Option<String> {
        self.bound(action)
            .into_iter()
            .filter(|(_, event)| keep(*event))
            .min_by_key(|(name, _)| name.chars().count())
            .map(|(name, _)| name)
    }

    /// [`Keymap::short_key`] with its event: the first of the shortest names, in name order.
    fn short(&self, action: Action) -> Option<(String, KeyEvent)> {
        self.bound(action)
            .into_iter()
            .min_by_key(|(name, _)| name.chars().count())
    }

    /// Every key bound to `action`, by its display name and as its event, in name order.
    fn bound(&self, action: Action) -> Vec<(String, KeyEvent)> {
        let mut keys: Vec<(String, KeyEvent)> = self
            .map
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(&(code, mods), _)| (display_key(code, mods), KeyEvent::new(code, mods)))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        keys
    }

    /// The label drawn for session `n` (1–9): the spelling of the key that selects it, or
    /// the digit itself when nothing is bound — the search preset, whose digits are text
    /// for its filter.
    pub fn label(&self, n: u8) -> String {
        self.short_key(Action::SelectSession(n))
            .unwrap_or_else(|| n.to_string())
    }

    /// [`Keymap::label`] when a query can hold it as typed, a single character that is not a
    /// space or a control (`4`, or `r` with `[keys] select_session` on letters), so a row's
    /// address (`4a`) can be typed into the filter; `None` for a key's name (`ctrl-4`), which
    /// typing spells out as text and never as that key.
    pub fn typed_label(&self, n: u8) -> Option<String> {
        let label = self.label(n);
        let mut chars = label.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_whitespace() && !c.is_control() => Some(label),
            _ => None,
        }
    }

    /// The hint line's spelling of the select keys: `"1-9"` for a run from the first bound
    /// label to the last, `None` when no session key is bound at all.
    pub fn select_session_hint(&self) -> Option<String> {
        let bound: Vec<String> = (1..=9u8)
            .filter_map(|n| self.short_key(Action::SelectSession(n)))
            .collect();
        match bound.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            [first, .., last] => Some(format!("{first}-{last}")),
        }
    }

    /// The letters that can name a row, `a` to `e` ([`JUMP_LETTERS`]), but for any a `[keys]`
    /// entry gives to an action: that letter is the action's key.
    pub fn free_letters(&self) -> Vec<char> {
        JUMP_LETTERS
            .chars()
            .filter(|&letter| {
                self.lookup(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::NONE))
                    .is_none()
            })
            .collect()
    }

    /// The spelling of the letter keys, the hint line's and the help's alike: the letters that
    /// can name a row ([`Keymap::free_letters`]) as a run (`a-e`, `b-e`) while every letter
    /// between its first and its last is still there, and listed (`a b d e`) when a `[keys]` entry
    /// takes one from the middle, where `a-e` would claim the one it took still jumps. `None`
    /// when `[keys]` takes all five.
    pub fn letters_hint(&self) -> Option<String> {
        let letters = self.free_letters();
        let contiguous = letters
            .windows(2)
            .all(|pair| pair[1] as u32 == pair[0] as u32 + 1);
        match letters.as_slice() {
            [] => None,
            [only] => Some(only.to_string()),
            [first, .., last] if contiguous => Some(format!("{first}-{last}")),
            _ => Some(
                letters
                    .iter()
                    .map(char::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        }
    }

    fn bind(&mut self, code: KeyCode, mods: KeyModifiers, action: Action) {
        let normalized = normalize(KeyEvent::new(code, mods));
        self.map
            .insert((normalized.code, normalized.modifiers), action);
    }
}

/// The key that ends `taw app` at once, and so brings a person back to tmux from an app the popup's Enter started:
/// `Q` in the modal preset while `[keys]` gives Q to no action, else `Ctrl-C`, which always does (the search
/// preset's Q is text for its query). What the popup says of a trip to another machine names it.
pub fn leave_key(preset: KeysPreset, keymap: &Keymap) -> &'static str {
    let q_is_free = keymap
        .lookup(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT))
        .is_none();
    if preset == KeysPreset::Modal && q_is_free {
        "Q"
    } else {
        "Ctrl-C"
    }
}

/// The built-in bindings for `preset`: `Modal`'s short mnemonic letters, gated behind no
/// mode switch, or `Search`'s, which are fzf's own keys and
/// nothing else — every plain letter is free for typing into the always-on filter, and
/// everything past moving and going is the mouse's or a `[keys]` entry's.
pub fn preset(p: KeysPreset) -> Keymap {
    let mut map = Keymap::default();
    let none = KeyModifiers::NONE;
    let ctrl = KeyModifiers::CONTROL;
    use Action::*;
    use KeyCode::*;
    match p {
        KeysPreset::Modal => {
            map.bind(Tab, none, NextAttention);
            map.bind(BackTab, none, PreviousAttention);
            map.bind(Enter, none, Go);
            map.bind(Char('j'), none, MoveDown);
            map.bind(Down, none, MoveDown);
            map.bind(Char('k'), none, MoveUp);
            map.bind(Up, none, MoveUp);
            map.bind(Char('J'), none, NextSession);
            map.bind(Char('K'), none, PreviousSession);
            map.bind(Char('/'), none, Filter);
            map.bind(Char('h'), none, Shallower);
            map.bind(Char('l'), none, Deeper);
            map.bind(Char('f'), none, ToggleFlat);
            map.bind(Char('r'), none, Rename);
            map.bind(Char('x'), none, Close);
            map.bind(Char('n'), none, NewSession);
            map.bind(Char('w'), none, NewWorktree);
            map.bind(Char('o'), none, Options);
            map.bind(Char('v'), ctrl, PasteImage);
            map.bind(Char('?'), none, Help);
            map.bind(Char('q'), none, Quit);
            map.bind(Esc, none, Quit);
            for n in 1..=9u8 {
                map.bind(Char(char::from(b'0' + n)), none, SelectSession(n));
            }
        }
        KeysPreset::Search => {
            map.bind(Enter, none, Go);
            map.bind(Esc, none, Quit);
            map.bind(Up, none, MoveUp);
            map.bind(Down, none, MoveDown);
            map.bind(Char('k'), ctrl, MoveUp);
            map.bind(Char('j'), ctrl, MoveDown);
            map.bind(Char('p'), ctrl, MoveUp);
            map.bind(Char('n'), ctrl, MoveDown);
        }
    }
    map
}

/// Resolves `[keys]`'s preset plus its overrides into a [`Keymap`], and every diagnostic
/// found along the way: an unknown action name, a key that fails [`parse_key`]'s grammar, or
/// a key contested by two actions. An override *replaces* the preset's keys for that action
/// rather than adding to them, unless no key it lists can be used, each one either failing
/// [`parse_key`] or already belonging to a different action, in which case the action falls
/// back to whatever keys it had before this override (ordinarily its preset keys) rather than
/// ending up completely unbound, and the
/// diagnostic says so (`"; <action> keeps its default keys"`) rather than leaving
/// the action's total silence unexplained. Overrides are applied in the config's own
/// order (`[keys]`'s `BTreeMap`, i.e. alphabetical by table key); the first action to claim a
/// physical key keeps it, so a contest with an action that was never itself overridden always
/// resolves in the preset's favour. `select_session` is one override covering all nine
/// `SelectSession(n)` at once and follows the same rules, fallback included; besides, an
/// entry of its list that does not parse keeps that session's preset key, unless another
/// entry of the list has taken it (the diagnostic then says the session is left unbound).
pub fn from_config(keys: &Keys) -> (Keymap, Vec<String>) {
    let (keymap, diagnostics) = resolve(keys);
    (keymap, diagnostics.into_iter().map(|d| d.message).collect())
}

/// What a [`from_config`] diagnostic is about, so a reader that wants only some of them (the
/// config check) filters on this rather than on the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A `[keys]` name that is no action.
    UnknownAction,
    /// A key that fails [`parse_key`]'s grammar.
    Unparsed,
    /// A key two actions both claim.
    Contested,
}

/// One diagnostic of [`resolve`]: its kind and the whole message, a fallback note included.
#[derive(Clone, Debug)]
pub struct KeyDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

impl KeyDiagnostic {
    fn unparsed(key_str: &str, error: &str) -> Self {
        Self {
            kind: DiagnosticKind::Unparsed,
            message: format!("keys: \"{key_str}\": {error}"),
        }
    }
}

/// [`from_config`] with each diagnostic tagged by its kind.
pub fn resolve(keys: &Keys) -> (Keymap, Vec<KeyDiagnostic>) {
    let mut keymap = preset(keys.preset);
    let mut diagnostics = Vec::new();

    for (name, spec) in &keys.bindings {
        if name == "select_session" {
            // One override for all nine `SelectSession(n)` at once, but the same rules as
            // every other action: replace rather than add to, and keep what was replaced in
            // case every key listed turns out to be someone else's.
            let previous: Vec<((KeyCode, KeyModifiers), Action)> = keymap
                .map
                .iter()
                .filter(|(_, a)| matches!(a, Action::SelectSession(_)))
                .map(|(&slot, &action)| (slot, action))
                .collect();
            keymap
                .map
                .retain(|_, a| !matches!(a, Action::SelectSession(_)));

            let mut claimed = 0usize;
            let mut conflicts = 0usize;
            let mut refused = 0usize;
            let mut entry_diagnostics = Vec::new();
            let mut slot_notes: Vec<(usize, usize)> = Vec::new();
            // There are nine labels, so a tenth key has no session to name and is dropped.
            for (index, key_str) in spec.to_vec().iter().enumerate().take(9) {
                let action = Action::SelectSession(index as u8 + 1);
                match parse_key(key_str) {
                    Ok(event) => {
                        let normalized = normalize(event);
                        let slot = (normalized.code, normalized.modifiers);
                        match keymap.map.get(&slot) {
                            Some(&existing) if existing != action => {
                                conflicts += 1;
                                entry_diagnostics.push(contested(key_str, action, existing));
                            }
                            _ => {
                                keymap.map.insert(slot, action);
                                claimed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        refused += 1;
                        // Only recorded here: the slot is put back after the loop, so the
                        // result does not depend on the order of the entries.
                        slot_notes.push((entry_diagnostics.len(), index + 1));
                        entry_diagnostics.push(KeyDiagnostic::unparsed(key_str, &e));
                    }
                }
            }
            if claimed == 0 && conflicts + refused > 0 && !previous.is_empty() {
                for (slot, action) in previous {
                    keymap.map.insert(slot, action);
                }
                note_fallback(&mut entry_diagnostics, Action::SelectSession(1));
            } else {
                // A refused entry's session gets its preset key back if no entry took it.
                for (at, n) in slot_notes {
                    let action = Action::SelectSession(n as u8);
                    let mut had = false;
                    let mut kept = false;
                    for &(slot, was) in &previous {
                        if was == action {
                            had = true;
                            if let std::collections::hash_map::Entry::Vacant(free) =
                                keymap.map.entry(slot)
                            {
                                free.insert(action);
                                kept = true;
                            }
                        }
                    }
                    if kept {
                        entry_diagnostics[at]
                            .message
                            .push_str(&format!("; session {n} keeps its default key"));
                    } else if had {
                        entry_diagnostics[at]
                            .message
                            .push_str(&format!("; session {n} is left unbound"));
                    }
                }
            }
            diagnostics.extend(entry_diagnostics);
            continue;
        }
        let Some(action) = action_by_name(name) else {
            diagnostics.push(KeyDiagnostic {
                kind: DiagnosticKind::UnknownAction,
                message: format!("keys: unknown action \"{name}\""),
            });
            continue;
        };
        // Replace, not add to: drop every key this action held from the preset (or an
        // earlier override for the same action name — impossible in practice, since
        // `keys.bindings`'s keys are themselves unique) before claiming the new ones. Kept
        // around in case every new key turns out to be someone else's, below.
        let previous: Vec<(KeyCode, KeyModifiers)> = keymap
            .map
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(&slot, _)| slot)
            .collect();
        keymap.map.retain(|_, a| *a != action);

        let mut claimed = 0usize;
        let mut conflicts = 0usize;
        let mut refused = 0usize;
        let mut entry_diagnostics = Vec::new();
        for key_str in spec.to_vec() {
            match parse_key(&key_str) {
                Ok(event) => {
                    let normalized = normalize(event);
                    let slot = (normalized.code, normalized.modifiers);
                    match keymap.map.get(&slot) {
                        Some(&existing) if existing != action => {
                            conflicts += 1;
                            entry_diagnostics.push(contested(&key_str, action, existing));
                        }
                        _ => {
                            keymap.map.insert(slot, action);
                            claimed += 1;
                        }
                    }
                }
                Err(e) => {
                    refused += 1;
                    entry_diagnostics.push(KeyDiagnostic::unparsed(&key_str, &e));
                }
            }
        }

        // Every key this override tried was already someone else's, or did not parse: fall
        // back to this action's previous keys rather than leaving it completely unbound, and
        // say so: otherwise a user rebinding one action could silently lose a different one,
        // and a typo would leave an action with no key. An empty list is not a typo: it
        // unbinds on purpose.
        if claimed == 0 && conflicts + refused > 0 && !previous.is_empty() {
            for slot in previous {
                keymap.map.insert(slot, action);
            }
            note_fallback(&mut entry_diagnostics, action);
        }
        diagnostics.extend(entry_diagnostics);
    }
    (keymap, diagnostics)
}

/// The diagnostic for a key two actions both claim: both names, sorted so the wording does
/// not depend on which one was overridden, then whose the key stays. Shared, so
/// `select_session`'s own branch cannot word it differently from every other action's.
fn contested(key_str: &str, wanted: Action, existing: Action) -> KeyDiagnostic {
    let mut names = [action_name(wanted), action_name(existing)];
    names.sort_unstable();
    KeyDiagnostic {
        kind: DiagnosticKind::Contested,
        message: format!(
            "keys: \"{key_str}\" is bound to both {} and {}; keeping {}",
            names[0],
            names[1],
            action_name(existing)
        ),
    }
}

/// Notes on an override's diagnostics that the action kept the keys it already had, every key
/// it asked for having been someone else's or not a key at all. It goes on the last key that
/// did not parse, else on the last diagnostic, so a reader that leaves the contested keys out
/// (the config check) still has it. Without it the reader is told which keys were refused
/// but not that the action is still reachable.
fn note_fallback(entry_diagnostics: &mut [KeyDiagnostic], action: Action) {
    let at = entry_diagnostics
        .iter()
        .rposition(|d| d.kind == DiagnosticKind::Unparsed)
        .or(entry_diagnostics.len().checked_sub(1));
    if let Some(at) = at {
        entry_diagnostics[at]
            .message
            .push_str(&format!("; {} keeps its default keys", action_name(action)));
    }
}

/// One `[keys]` value, herdr's grammar: `+`-joined modifiers (`ctrl`/`control`,
/// `alt`/`meta`/`option`, `shift`) then a named key or a single character, whose case implies
/// shift — `"R"` and `"shift+r"` parse identically. `prefix+…` is refused: the prefix chord
/// is tmux's own (see [`super::prefix`]), not a `[keys]` binding.
pub fn parse_key(spec: &str) -> Result<KeyEvent, String> {
    let tokens: Vec<&str> = spec.split('+').collect();
    let key_token = tokens[tokens.len() - 1];
    let modifier_tokens = &tokens[..tokens.len() - 1];

    let mut mods = KeyModifiers::NONE;
    for raw in modifier_tokens {
        match raw.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "prefix" => {
                return Err(
                    "prefix chords are tmux's; set @agent-wizard-notification-key instead"
                        .to_string(),
                );
            }
            other => return Err(format!("unknown modifier \"{other}\" in key \"{spec}\"")),
        }
    }

    let named = named_key(key_token);
    let (code, mods) = if let Some(code) = named {
        // `shift+tab` is really `BackTab` in crossterm; every other named key's SHIFT
        // (rare, and not used by either preset) just passes through as a modifier.
        if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
            (KeyCode::BackTab, mods.difference(KeyModifiers::SHIFT))
        } else {
            (code, mods)
        }
    } else {
        let mut chars = key_token.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(format!("unknown key \"{spec}\""));
        };
        // An explicit `shift+` on a lowercase letter uppercases it — "shift+r" and "R" are
        // the same key. Either way SHIFT is never also set on the resulting Char: a single
        // uppercase letter already implies shift — see this module's normalization rule.
        let c = if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
            c.to_ascii_uppercase()
        } else {
            c
        };
        (KeyCode::Char(c), mods.difference(KeyModifiers::SHIFT))
    };
    Ok(KeyEvent::new(code, mods))
}

/// The named keys in herdr's grammar, matched case-insensitively. Words stand in for
/// characters that would otherwise be awkward or ambiguous to type directly in a `[keys]`
/// value (`+` is the modifier separator, so a literal `+` key is `"plus"`, never `"+"`).
fn named_key(token: &str) -> Option<KeyCode> {
    Some(match token.to_ascii_lowercase().as_str() {
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "slash" => KeyCode::Char('/'),
        "backslash" => KeyCode::Char('\\'),
        "minus" => KeyCode::Char('-'),
        "comma" => KeyCode::Char(','),
        "period" => KeyCode::Char('.'),
        "quote" => KeyCode::Char('\''),
        "semicolon" => KeyCode::Char(';'),
        "colon" => KeyCode::Char(':'),
        "percent" => KeyCode::Char('%'),
        "ampersand" => KeyCode::Char('&'),
        "backtick" => KeyCode::Char('`'),
        "plus" => KeyCode::Char('+'),
        "question" => KeyCode::Char('?'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
