//! `[theme] auto_switch` end to end: `taw app` in a pane of a private tmux server draws `dark_name` while the
//! terminal of the client that shows its pane reports dark, and `light_name` after the terminal reports light.
//!
//! The terminal is a stand-in on a pseudo terminal ([`common::terminal`]): it runs the server's tmux client,
//! answers the client's question of its light or dark mode (`ESC [ ? 996 n`) with `ESC [ ? 997 ; 1 n` (dark) or
//! `ESC [ ? 997 ; 2 n` (light), and pushes the other mode unasked as a terminal does when its mode changes. A client
//! attached inside another tmux never gets such an answer, which is why no nested tmux stands in here.
//!
//! The popup's own path (it asks its client in the query it already makes) is covered by the unit tests of
//! `src/tui/data.rs` and `src/tui/mod.rs`: no test of this suite reads the cells of a popup on a client.
//!
//! Every wait is positive, with a 20 s deadline, and panics with what was seen. On a tmux before 3.6, which has no
//! `#{client_theme}`, the test says why it does not run and passes.

mod common;

use std::time::Duration;

use common::owned::{Owned, own, record_names};
use common::terminal::{Mode, Terminal};
use common::{Server, ServerOptions, taw_bin, wait_for};

/// Why the test does not run on the tmux that said `said` to `-V`, or `None` when it does: only 3.6 and later have
/// `#{client_theme}`, and a version that cannot be read is not run either.
fn skip_reason(said: &str) -> Option<String> {
    match taw::tmux::version::parse(said) {
        Some(version) if version.at_least(3, 6) => None,
        _ => Some(format!(
            "{said} has no #{{client_theme}} (3.6 and later have)"
        )),
    }
}

/// The skip decision follows the `tmux -V` text: 3.5a skips and says why, 3.6, 3.6b and 3.8 run.
#[test]
fn the_test_skips_on_a_tmux_before_3_6_and_runs_on_3_6_and_later() {
    let why = skip_reason("tmux 3.5a").expect("3.5a has no client_theme");
    assert!(
        why.contains("tmux 3.5a") && why.contains("3.6 and later have"),
        "{why}"
    );
    assert_eq!(skip_reason("tmux 3.6"), None);
    assert_eq!(skip_reason("tmux 3.6b"), None);
    assert_eq!(skip_reason("tmux 3.8"), None);
    assert!(skip_reason("tmux nonsense").is_some());
}

/// The pane's screen with its escapes: the colours as drawn.
fn styled(server: &Server) -> String {
    server
        .tmux(&["capture-pane", "-p", "-e", "-t", &server.pane()])
        .unwrap_or_default()
}

/// Whether `screen` draws `rgb` (`r;g;b`) as a foreground or a background colour.
fn draws(screen: &str, rgb: &str) -> bool {
    screen.contains(&format!("38;2;{rgb}m")) || screen.contains(&format!("48;2;{rgb}m"))
}

/// Dracula's accent `#bd93f9` and solarized's `#268bd2`, which the app draws in its header: the dark side and the
/// light side of the config below.
const DRACULA_ACCENT: &str = "189;147;249";
const SOLARIZED_ACCENT: &str = "38;139;210";

fn client_theme(server: &Server, term: &Terminal) -> String {
    server
        .tmux(&["list-clients", "-F", "#{client_name} #{client_theme}"])
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.strip_prefix(&format!("{} ", term.tty))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[test]
fn the_app_follows_the_light_or_dark_mode_its_clients_terminal_reports() {
    let _owned = Owned::new();
    let server = Server::start_with(ServerOptions {
        width: 100,
        height: 30,
        ..ServerOptions::default()
    });
    if let Some(why) = skip_reason(&server.tmux_ok(&["-V"])) {
        eprintln!("skipped: {why}");
        return;
    }
    server.write_config(
        "[theme]\nname = \"nord\"\nauto_switch = true\ndark_name = \"dracula\"\nlight_name = \"solarized\"\n",
    );
    let seen = |what: &str, term: &Terminal| {
        format!(
            "{what} never happened.\nclient theme: {:?}\nthe client wrote (first 2 KB, hex): {}\nscreen:\n{}",
            client_theme(&server, term),
            term.head_hex(2048),
            styled(&server).replace('\u{1b}', "ESC")
        )
    };

    // The terminal answers dark when asked.
    let term = Terminal::attach(&server, "one", Mode::Dark);
    assert!(
        wait_for(
            || client_theme(&server, &term) == "dark",
            Duration::from_secs(20)
        ),
        "{}",
        seen("the client reading dark", &term)
    );
    assert!(
        term.has_written(b"\x1b[?996n"),
        "the client never asked its terminal for its mode: {}",
        term.head_hex(2048)
    );

    // The app in the pane, started by the shell the way a person types it.
    let err = server.work_dir().join("err");
    let line = format!("'{}' app --child 2>'{}'", taw_bin(), err.display());
    server.tmux_ok(&["send-keys", "-t", &server.pane(), "-l", &line]);
    server.tmux_ok(&["send-keys", "-t", &server.pane(), "Enter"]);
    let records = || {
        record_names(
            std::fs::read_dir(server.state_dir().join("apps"))
                .map(|dir| {
                    dir.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default(),
        )
    };
    assert!(
        wait_for(|| records().len() == 1, Duration::from_secs(20)),
        "{}",
        seen("one record of an app", &term)
    );
    let pid: u32 = records()[0]
        .trim_end_matches(".json")
        .parse()
        .expect("a pid");
    own(pid);

    assert!(
        wait_for(
            || draws(&styled(&server), DRACULA_ACCENT),
            Duration::from_secs(20)
        ),
        "{}",
        seen("the app drawing dracula for a dark terminal", &term)
    );
    assert!(
        !draws(&styled(&server), SOLARIZED_ACCENT),
        "the dark app also draws the light theme's accent"
    );

    // The terminal's mode changes: it reports light unasked.
    term.push(Mode::Light);
    assert!(
        wait_for(
            || client_theme(&server, &term) == "light",
            Duration::from_secs(20)
        ),
        "{}",
        seen("the client reading light", &term)
    );
    assert!(
        wait_for(
            || draws(&styled(&server), SOLARIZED_ACCENT),
            Duration::from_secs(20)
        ),
        "{}",
        seen("the app drawing solarized for a light terminal", &term)
    );
    // The dark phase's cells are gone, so the wait below for dracula again cannot pass on what was left.
    assert!(
        wait_for(
            || !draws(&styled(&server), DRACULA_ACCENT),
            Duration::from_secs(20)
        ),
        "{}",
        seen("the dark theme's accent going from the screen", &term)
    );

    // And back.
    term.push(Mode::Dark);
    assert!(
        wait_for(
            || draws(&styled(&server), DRACULA_ACCENT),
            Duration::from_secs(20)
        ),
        "{}",
        seen("the app drawing dracula again", &term)
    );
}
