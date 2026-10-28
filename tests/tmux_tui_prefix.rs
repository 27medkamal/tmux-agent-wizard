//! The popup's prefix chord, read from a real tmux server's options.
//!
//! `tui::prefix` is unit-tested against keys already parsed; this checks the other half: that
//! what tmux prints for `prefix` and `prefix2` (`C-b`, `C-a`, and `None` for a second prefix
//! nobody set) is what the chord listens for, and that the popup goes next up on either one.
//! herdr v0.9.2 takes several prefix keys (#4653, `prefix = ["ctrl+space", "ctrl+s"]`); tmux's
//! own second prefix is `prefix2`.
//!
//! Every test runs on its own `-L` socket and tears the server down on drop; the user's real
//! tmux server is never touched.

mod common;

use common::shell_env::ShellHome;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taw::config::Config;
use taw::tmux::Tmux;
use taw::tui::actions::Action;
use taw::tui::app::App;
use taw::tui::data::{self, GitWorker};
use taw::tui::prefix::{Chord, ChordOutcome};
use taw::tui::theme;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. It has tmux's own
/// defaults for the two prefixes: `C-b` and `None`.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let t = Tmux::with_socket_name(&format!("taw-test-prefix-{}-{n}", std::process::id()));
        let home = ShellHome::new();
        // `-f /dev/null` so the host's own tmux.conf (which may well set a `prefix2`) cannot
        // change what this test observes, and `/bin/sh` so no login shell's rc file runs.
        home.start_server(&t, &["-d", "-s", "main", "-x", "80", "-y", "24", "/bin/sh"]);
        Server(t, home)
    }

    /// `set -g <option> <value>`, the way a tmux.conf sets a prefix.
    fn set(&self, option: &str, value: &str) {
        self.0.run_ok(&["set-option", "-g", option, value]).unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn bare(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// A tmux nobody has set a second prefix on prints `None` for it, and that is no key: the
/// chord has the one prefix, however a key is spelled.
#[test]
fn a_tmux_with_no_second_prefix_has_only_the_one() {
    let s = Server::start();
    assert_eq!(
        s.0.run(&["show-options", "-gv", "prefix2"]).unwrap(),
        "None",
        "the shape tmux gives an option that is no key"
    );

    let chord = Chord::from_tmux(&s.0).expect("tmux's default prefix reads");

    assert!(chord.is_prefix(ctrl('b')));
    assert!(!chord.is_prefix(ctrl('a')));
    assert!(!chord.is_prefix(bare('N')), "None is not the key N");
}

/// herdr's `multiple_prefix_keys_parse_reserve_and_share_prefix_bindings`, as tmux has it: with
/// `set -g prefix2 C-a` either key starts the chord, and the same key after it means the same
/// thing, whichever started it.
#[test]
fn prefix2_starts_the_chord_as_prefix_does() {
    let s = Server::start();
    s.set("prefix2", "C-a");

    for prefix in [ctrl('b'), ctrl('a')] {
        let mut chord = Chord::from_tmux(&s.0).expect("both prefixes read");
        let now = Instant::now();
        assert_eq!(chord.feed(prefix, now), ChordOutcome::Started, "{prefix:?}");
        assert_eq!(
            chord.feed(bare('O'), now),
            ChordOutcome::Notification,
            "{prefix:?} then O"
        );
    }
}

/// The popup on a tmux with `prefix2` goes next up on it: the chord's second key comes out as
/// the action the run loop performs.
#[test]
fn a_popup_on_a_tmux_with_prefix2_goes_next_up_on_either_prefix() {
    let s = Server::start();
    s.set("prefix2", "C-a");
    let cfg = Config::default();
    let git = GitWorker::new(Duration::from_secs(2));
    let theme = theme::resolve("catppuccin", &cfg.theme.custom).theme;

    for prefix in [ctrl('b'), ctrl('a')] {
        let snap = data::build(&s.0, &cfg, &git, None, None).unwrap();
        let mut app = App::new(&cfg, theme, String::new(), snap);
        app.chord = Chord::from_tmux(&s.0);

        assert_eq!(app.on_key(prefix), None, "{prefix:?} alone does nothing");
        assert_eq!(
            app.on_key(bare('O')),
            Some(Action::NextUp { own: None }),
            "{prefix:?} then O"
        );
    }
}

/// `set -g prefix None` is how a tmux has no first prefix; with a `prefix2` the second one is
/// the whole of the chord.
#[test]
fn with_no_prefix_a_prefix2_is_the_whole_chord() {
    let s = Server::start();
    s.set("prefix", "None");
    s.set("prefix2", "C-a");

    let chord = Chord::from_tmux(&s.0).expect("prefix2 alone is a chord");

    assert!(chord.is_prefix(ctrl('a')));
    assert!(!chord.is_prefix(ctrl('b')), "the default prefix is gone");
}

/// herdr's `empty_prefix_list_is_rejected`: with neither key there is no chord to read, and the
/// popup runs without one (and says so).
#[test]
fn with_neither_prefix_there_is_no_chord() {
    let s = Server::start();
    s.set("prefix", "None");

    assert!(Chord::from_tmux(&s.0).is_none());
}
