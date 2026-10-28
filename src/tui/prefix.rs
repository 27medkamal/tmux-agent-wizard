// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The tmux prefix chord, recognised from inside the popup.
//!
//! tmux has no way to leave a popup's own key table listening while still forwarding the
//! prefix through to it: a `display-popup`'d program just gets every keystroke raw,
//! prefix included. So the popup has to notice the prefix itself, `prefix` or `prefix2`
//! (tmux's second one; either starts the chord, as every prefix herdr is given does), and
//! treat the key that follows within [`Chord`]'s timeout the way tmux's own key table would
//! outside it: the notification key goes next up. Any other key after the prefix is
//! swallowed, the popup's own key included: the popup has no chord that closes it, since
//! its quit key already does.
//!
//! [`parse_tmux_key`] is tmux's own key-spelling grammar (`bind-key`'s), not herdr's:
//! [`super::keys::parse_key`] is that one, for `[keys]` values.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::keys::normalize;
use crate::tmux::Tmux;

/// tmux's own key-name grammar (as `bind-key`, `show-options` and friends spell a key):
/// `C-`/`M-`/`S-` modifier prefixes (combinable, e.g. `C-M-x`), a named key, or a single
/// character whose case implies shift (`"O"` is already shifted, same as herdr's own
/// grammar). `None` on anything unrecognised: tmux itself would refuse it as a key name.
pub fn parse_tmux_key(spec: &str) -> Option<KeyEvent> {
    let mut mods = KeyModifiers::NONE;
    let mut rest = spec;
    loop {
        let flag = match rest.as_bytes() {
            [b'C', b'-', ..] => KeyModifiers::CONTROL,
            [b'M', b'-', ..] => KeyModifiers::ALT,
            [b'S', b'-', ..] => KeyModifiers::SHIFT,
            _ => break,
        };
        mods |= flag;
        rest = &rest[2..];
    }
    if rest.is_empty() {
        return None;
    }
    let code = named_tmux_key(rest)?;
    // A Char or BackTab already fully encodes its own shifted-ness; an explicit S- beside
    // one (tmux does not itself produce this, but a hand-written option could) would
    // otherwise be double-counted: see keys.rs's identical rule.
    let mods = match code {
        KeyCode::Char(_) | KeyCode::BackTab => mods.difference(KeyModifiers::SHIFT),
        _ => mods,
    };
    Some(KeyEvent::new(code, mods))
}

/// `spec` with every `C-`/`M-`/`S-` prefix already stripped: one of tmux's named keys, an
/// `F<1-12>`, or a single character.
fn named_tmux_key(rest: &str) -> Option<KeyCode> {
    let code = match rest {
        "Space" => KeyCode::Char(' '),
        "Enter" => KeyCode::Enter,
        "Escape" => KeyCode::Esc,
        "Tab" => KeyCode::Tab,
        "BTab" => KeyCode::BackTab,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PPage" => KeyCode::PageUp,
        "NPage" => KeyCode::PageDown,
        _ => return function_key(rest).or_else(|| single_char_key(rest)),
    };
    Some(code)
}

/// `"F1"` through `"F12"`, tmux's own function-key spelling.
fn function_key(rest: &str) -> Option<KeyCode> {
    let digits = rest.strip_prefix('F')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u8 = digits.parse().ok()?;
    (1..=12).contains(&n).then_some(KeyCode::F(n))
}

/// `rest` as one literal character, case preserved (case is how tmux spells shift on a
/// plain key, same as herdr's own grammar).
fn single_char_key(rest: &str) -> Option<KeyCode> {
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(KeyCode::Char(c)),
        _ => None,
    }
}

/// What [`Chord::feed`] learned from one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChordOutcome {
    /// The prefix alone: a chord is now pending.
    Started,
    /// prefix + the notification key: go next up.
    Notification,
    /// prefix + anything else, within the timeout: consumed and forgotten.
    Ignored,
    /// No chord was pending, or the timeout already passed: `key` is not part of any
    /// chord and should be handled normally (e.g. looked up in [`super::keys::Keymap`]).
    NotAChord,
}

/// How long after a prefix its second key still counts.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The tmux prefix chord's state: which keys are the prefix, which second key means what,
/// and whether a prefix press is still waiting on its second key.
#[derive(Clone, Debug)]
pub struct Chord {
    /// tmux's `prefix` and `prefix2`, whichever of them is a key: each one starts the chord.
    prefixes: Vec<KeyEvent>,
    notification: Option<KeyEvent>,
    pending: Option<Instant>,
    timeout: Duration,
}

impl Chord {
    /// Reads `prefix` and `prefix2` (tmux's own options) and `@agent-wizard-notification-key`
    /// (default `O`) from `t`. `None` when neither prefix is a key: the chord is then
    /// disabled entirely, since without one there is no way to tell tmux's own prefix from
    /// ordinary input (the caller logs this).
    ///
    /// tmux prints `None` for `prefix2` until it is set, and for a `prefix` set to `None`: no
    /// key. A tmux with no `prefix2` option at all fails that read, which is no key either.
    pub fn from_tmux(t: &Tmux) -> Option<Chord> {
        let read = |option: &str| t.run(&["show-options", "-gv", option]).unwrap_or_default();
        let notification_raw = t
            .get_global_option("@agent-wizard-notification-key")
            .ok()
            .flatten()
            .unwrap_or_else(|| "O".to_string());
        Chord::from_options(&read("prefix"), &read("prefix2"), &notification_raw)
    }

    /// The chord for what tmux printed for `prefix`, `prefix2` and the notification key.
    /// [`Chord::from_tmux`] is this plus the `show-options` reads. A value that is not a key
    /// (`None`, or anything tmux's own grammar would refuse) is dropped, and the others
    /// survive; with no prefix left there is no chord. A notification key that is not a key
    /// leaves a chord that goes nowhere.
    pub fn from_options(prefix: &str, prefix2: &str, notification: &str) -> Option<Chord> {
        let prefixes: Vec<KeyEvent> = [prefix, prefix2]
            .into_iter()
            .filter_map(|raw| parse_tmux_key(raw.trim()))
            .collect();
        if prefixes.is_empty() {
            return None;
        }
        Some(Chord {
            prefixes,
            notification: parse_tmux_key(notification.trim()),
            pending: None,
            timeout: TIMEOUT,
        })
    }

    /// The chord of a tmux with the one prefix `prefix`, from keys already parsed out of its
    /// options. Anything holding the keys already, the popup's own tests or a caller that read
    /// them another way, builds one directly.
    pub fn new(prefix: KeyEvent, notification: Option<KeyEvent>) -> Chord {
        Chord {
            prefixes: vec![prefix],
            notification,
            pending: None,
            timeout: TIMEOUT,
        }
    }

    /// Whether `key` is one of tmux's prefixes, `prefix` or `prefix2`, however the terminal
    /// spells its modifiers.
    pub fn is_prefix(&self, key: KeyEvent) -> bool {
        let key = normalize(key);
        self.prefixes.iter().any(|&prefix| key == normalize(prefix))
    }

    /// Feeds one key to the chord's state machine. `now` drives the timeout so tests can
    /// simulate one passing without sleeping.
    ///
    /// A prefix, either of them, always (re)starts the wait, whatever was pending before.
    /// Otherwise, a pending prefix is consumed by this very key, one-shot, whether it matches
    /// the notification key or not, so a stale pending state never lingers past the next
    /// key fed to it.
    pub fn feed(&mut self, key: KeyEvent, now: Instant) -> ChordOutcome {
        if self.is_prefix(key) {
            self.pending = Some(now);
            return ChordOutcome::Started;
        }
        let key = normalize(key);
        let Some(started) = self.pending.take() else {
            return ChordOutcome::NotAChord;
        };
        if now.duration_since(started) >= self.timeout {
            return ChordOutcome::NotAChord;
        }
        if Some(key) == self.notification.map(normalize) {
            return ChordOutcome::Notification;
        }
        ChordOutcome::Ignored
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn ctrl_b() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
    }

    fn bare(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A chord as if `from_tmux` had read `prefix = C-b` and
    /// `@agent-wizard-notification-key = O` (the documented defaults). `@agent-wizard`,
    /// the key that opens the popup (`A` by default), is not read at all any more.
    fn chord() -> Chord {
        Chord::new(ctrl_b(), Some(bare(KeyCode::Char('O'))))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// The chord of a tmux that printed `prefix` and `prefix2` for those two options, with the
    /// default notification key.
    fn chord_of(prefix: &str, prefix2: &str) -> Option<Chord> {
        Chord::from_options(prefix, prefix2, "O")
    }

    #[test]
    fn parses_tmux_key_spellings() {
        assert_eq!(parse_tmux_key("C-b"), Some(ctrl_b()));
        assert_eq!(
            parse_tmux_key("M-a"),
            Some(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT))
        );
        assert_eq!(parse_tmux_key("O"), Some(bare(KeyCode::Char('O'))));
        assert_eq!(
            parse_tmux_key("C-M-x"),
            Some(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
        );
        assert_eq!(
            parse_tmux_key("S-F1"),
            Some(KeyEvent::new(KeyCode::F(1), KeyModifiers::SHIFT))
        );
        assert_eq!(parse_tmux_key("F12"), Some(bare(KeyCode::F(12))));
        assert_eq!(parse_tmux_key("Space"), Some(bare(KeyCode::Char(' '))));
        assert_eq!(parse_tmux_key("Enter"), Some(bare(KeyCode::Enter)));
        assert_eq!(parse_tmux_key("Escape"), Some(bare(KeyCode::Esc)));
        assert_eq!(parse_tmux_key("Tab"), Some(bare(KeyCode::Tab)));
        assert_eq!(parse_tmux_key("BTab"), Some(bare(KeyCode::BackTab)));
        assert_eq!(parse_tmux_key("Up"), Some(bare(KeyCode::Up)));
        assert_eq!(parse_tmux_key("PPage"), Some(bare(KeyCode::PageUp)));
        assert_eq!(parse_tmux_key("NPage"), Some(bare(KeyCode::PageDown)));
        assert_eq!(parse_tmux_key(""), None);
        assert_eq!(parse_tmux_key("F13"), None);
        assert_eq!(parse_tmux_key("X-b"), None);
        // What tmux prints for a prefix that is no key (`prefix2` until it is set) is no key
        // here either, and not the five letters it spells.
        assert_eq!(parse_tmux_key("None"), None);
    }

    #[test]
    fn is_prefix_knows_the_prefix_however_shift_is_reported() {
        let c = Chord::new(
            KeyEvent::new(KeyCode::Char('B'), KeyModifiers::CONTROL),
            None,
        );
        assert!(c.is_prefix(KeyEvent::new(
            KeyCode::Char('B'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(chord().is_prefix(ctrl_b()));
        assert!(!chord().is_prefix(bare(KeyCode::Char('b'))));
        assert!(!chord().is_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn lone_prefix_starts_the_chord() {
        let mut c = chord();
        assert_eq!(c.feed(ctrl_b(), Instant::now()), ChordOutcome::Started);
    }

    #[test]
    fn prefix_then_notification_key_goes_to_what_needs_you() {
        let mut c = chord();
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        assert_eq!(
            c.feed(bare(KeyCode::Char('O')), t0),
            ChordOutcome::Notification
        );
    }

    /// There is no `prefix+C` chord: `A`, tmux's default key for opening the popup, is just
    /// another second key: swallowed, not a close.
    #[test]
    fn prefix_then_the_popup_key_is_ignored_not_a_close() {
        let mut c = chord();
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        assert_eq!(c.feed(bare(KeyCode::Char('A')), t0), ChordOutcome::Ignored);
        assert_eq!(
            c.feed(bare(KeyCode::Char('A')), t0),
            ChordOutcome::NotAChord,
            "and the chord is spent"
        );
    }

    #[test]
    fn prefix_then_an_unrecognised_key_is_ignored_and_clears_the_pending_state() {
        let mut c = chord();
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        assert_eq!(c.feed(bare(KeyCode::Char('z')), t0), ChordOutcome::Ignored);
        // Pending is cleared: the very same key, still with no prefix before it, is now
        // not part of any chord.
        assert_eq!(
            c.feed(bare(KeyCode::Char('z')), t0),
            ChordOutcome::NotAChord
        );
    }

    #[test]
    fn prefix_then_a_key_after_the_timeout_is_not_a_chord() {
        let mut c = chord();
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        let later = t0 + Duration::from_millis(2100);
        assert_eq!(
            c.feed(bare(KeyCode::Char('O')), later),
            ChordOutcome::NotAChord
        );
    }

    #[test]
    fn no_pending_chord_is_not_a_chord() {
        let mut c = chord();
        assert_eq!(
            c.feed(bare(KeyCode::Char('z')), Instant::now()),
            ChordOutcome::NotAChord
        );
    }

    // Follows herdr's src/config/keybinds.rs tests, the first of the three
    // below, `multiple_prefix_keys_parse_reserve_and_share_prefix_bindings`. Differences: the two
    // prefixes are tmux's `prefix` and `prefix2` as `show-options` prints them, herdr's own pair
    // (`ctrl+space`, `ctrl+s`) spelled the way tmux spells it; the key after the prefix is the
    // notification key, not a prefix-mode action; herdr's check that every prefix is reserved
    // against a binding has nothing to match, since the chord's second key is not bound per prefix.
    #[test]
    fn every_prefix_tmux_has_starts_the_chord_and_means_the_same_after_it() {
        let mut c = chord_of("C-Space", "C-s").expect("two prefixes");
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        for prefix in [space, ctrl('s')] {
            let t0 = Instant::now();
            assert_eq!(c.feed(prefix, t0), ChordOutcome::Started, "{prefix:?}");
            assert_eq!(
                c.feed(bare(KeyCode::Char('O')), t0),
                ChordOutcome::Notification,
                "{prefix:?}, then the notification key"
            );
        }
        assert!(!c.is_prefix(ctrl_b()), "and no other key is a prefix");
        assert!(!c.is_prefix(bare(KeyCode::Char('s'))), "nor the bare key");
    }

    /// One rule for both keys: a prefix always (re)starts the wait, whichever of the two it is and
    /// whatever was pending, so the second prefix right after the first is a restart and never the
    /// chord's second key.
    #[test]
    fn either_prefix_restarts_the_wait_whatever_was_pending() {
        let mut c = chord_of("C-b", "C-a").expect("two prefixes");
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        assert_eq!(
            c.feed(ctrl('a'), t0),
            ChordOutcome::Started,
            "the second prefix after the first"
        );
        assert_eq!(
            c.feed(ctrl_b(), t0),
            ChordOutcome::Started,
            "and the first after the second"
        );
        assert_eq!(
            c.feed(bare(KeyCode::Char('O')), t0),
            ChordOutcome::Notification
        );
    }

    /// tmux prints `None` for a `prefix2` nobody set, and that is no key, so the chord is the
    /// one prefix it always was.
    #[test]
    fn a_prefix2_of_none_is_no_key() {
        let c = chord_of("C-b", "None").expect("the prefix alone");
        assert!(c.is_prefix(ctrl_b()));
        assert!(!c.is_prefix(ctrl('a')));
        assert!(
            !c.is_prefix(bare(KeyCode::Char('N'))),
            "None is not the key N"
        );
    }

    /// `set -g prefix None` with a `prefix2` leaves the second one the whole of the chord.
    #[test]
    fn a_prefix_of_none_leaves_prefix2_the_whole_chord() {
        let mut c = chord_of("None", "C-a").expect("prefix2 alone");
        let t0 = Instant::now();
        assert!(!c.is_prefix(ctrl_b()), "the default prefix is not kept");
        assert_eq!(c.feed(ctrl('a'), t0), ChordOutcome::Started);
        assert_eq!(
            c.feed(bare(KeyCode::Char('O')), t0),
            ChordOutcome::Notification
        );
    }

    // Follows herdr's src/config/keybinds.rs tests,
    // `invalid_prefix_entries_are_dropped_while_valid_ones_survive` (`["ctrl+a", "wat"]`).
    // Differences: the entries are tmux's two options, whichever one is the unreadable one;
    // herdr's diagnostic that names it is not ported (the popup has no place to say it).
    #[test]
    fn an_unreadable_prefix_is_dropped_while_the_readable_one_survives() {
        for (prefix, prefix2) in [("C-a", "wat"), ("wat", "C-a")] {
            let c = chord_of(prefix, prefix2)
                .unwrap_or_else(|| panic!("{prefix:?} and {prefix2:?} leave one prefix"));
            assert!(c.is_prefix(ctrl('a')), "{prefix:?} {prefix2:?}");
            assert!(!c.is_prefix(ctrl_b()), "{prefix:?} {prefix2:?}");
        }
    }

    // Follows herdr's src/config/keybinds.rs tests,
    // `empty_prefix_list_is_rejected_and_keeps_the_fallback`. Differences: a chord has no
    // fallback key to keep, so with no prefix to read there is no chord at all (the popup runs
    // without one, and logs it).
    #[test]
    fn with_no_readable_prefix_there_is_no_chord() {
        for (prefix, prefix2) in [("None", "None"), ("wat", "None"), ("None", "wat"), ("", "")] {
            assert!(
                chord_of(prefix, prefix2).is_none(),
                "{prefix:?} {prefix2:?}"
            );
        }
    }

    /// The notification key is read the same way whatever the prefixes are, and one that cannot
    /// be read leaves a chord that never goes anywhere: every second key is swallowed.
    #[test]
    fn the_notification_key_is_parsed_beside_the_prefixes() {
        let mut c = Chord::from_options("C-b", "C-a", "M-o").expect("two prefixes");
        let t0 = Instant::now();
        c.feed(ctrl('a'), t0);
        assert_eq!(
            c.feed(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT), t0),
            ChordOutcome::Notification
        );

        let mut c = Chord::from_options("C-b", "C-a", "wat").expect("two prefixes");
        c.feed(ctrl('a'), t0);
        assert_eq!(
            c.feed(bare(KeyCode::Char('O')), t0),
            ChordOutcome::Ignored,
            "no notification key is no way to go next up"
        );
    }

    #[test]
    fn a_stray_shift_on_an_uppercase_notification_key_still_matches() {
        // Some terminals report an uppercase Char with an explicit SHIFT modifier
        // alongside it; the chord must treat that the same as a bare uppercase Char.
        let mut c = chord();
        let t0 = Instant::now();
        assert_eq!(c.feed(ctrl_b(), t0), ChordOutcome::Started);
        assert_eq!(
            c.feed(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT), t0),
            ChordOutcome::Notification
        );
    }
}
