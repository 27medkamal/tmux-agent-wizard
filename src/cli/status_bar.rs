//! taw's bar in the origin session's status line while the popup is open. The
//! line's *text* is swapped — `status-format[0]` on the session, the other rows blanked — so the bar sits wherever
//! the status line is and `display-message` keeps landing in it. The launcher (`taw ui
//! open`) saves, swaps and restores around `display-popup`; the popup keeps the counts
//! fresh with [`refresh_live`]; a launcher that died leaves [`SAVED_OPTION`] behind, and
//! the next launcher restores from it first ([`recover`]).
//!
//! Row 0 does not hold the bar's text itself: it holds [`BAR_ROW`], which draws the text
//! from the session option [`BAR_OPTION`]. The launcher writes both and takes both away;
//! the popup writes only the option. So a refresh the popup had in flight when it was
//! killed — landing after the launcher put the line back — reaches an option nothing
//! draws any more, instead of putting the bar back on a line whose record is gone.
//!
//! # How tmux scopes `status-format`
//!
//! Verified on tmux 3.6b. `status-format` is one array option: a session either has an
//! array of its own or inherits the global array whole. Setting one element on a session
//! that inherits gives it an array holding only that element — the rest of the global
//! array is not copied — and unsetting that element again leaves the session with an
//! *empty* array of its own, which tmux draws as a blank status line. So a session that
//! inherited its line gets it back by unsetting the whole option ([`restore`]), never
//! element by element, and [`swap`] records which of the two a session was.
use anyhow::Context;
use ratatui::style::Color;

use crate::attention;
use crate::machine::words;
use crate::tmux::{PaneInfo, Tmux};
use crate::tui::theme::Theme;

/// The session user option that records the rows [`swap`] replaced, from the moment the
/// bar goes up until [`restore`] puts them back. Still there on the next open means the
/// last launcher died with the popup open.
pub const SAVED_OPTION: &str = "@agent_wizard_status_saved";

/// The session user option holding the bar's text while the popup is open — the only
/// thing the popup ever writes ([`refresh_live`]). Changing it redraws the line at once.
pub const BAR_OPTION: &str = "@agent_wizard_bar";

/// What row 0 holds while the popup is open: [`BAR_OPTION`] expanded as a format, its
/// styles and the clock's `%H:%M` included. `T:` is the modifier that also expands strftime specifiers. In tmux since 3.1, below the
/// 3.2 floor; verified on 3.6b.
pub const BAR_ROW: &str = "#{T:@agent_wizard_bar}";

/// Separates the rows of a [`Saved`] record: a control character no status format has
/// any use for.
const RECORD_SEPARATOR: char = '\u{1f}';

/// The array option whose rows the bar replaces.
const STATUS_FORMAT: &str = "status-format";

/// How many rows tmux's `status` option gives the status line: `on` (or a value that
/// could not be read) is one, `off` none, `2`..`5` that many. Anything else counts as
/// on, as it does for the popup's geometry.
pub fn status_rows(status: Option<&str>) -> usize {
    match status.map(str::trim) {
        None | Some("on") => 1,
        Some("off") | Some("0") => 0,
        Some(n) => n.parse().unwrap_or(1),
    }
}

/// The bar's numbers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BarCounts {
    /// Every session on the server.
    pub sessions: usize,
    /// Every pane with an agent in it ([`attention::is_agent`]).
    pub agents: usize,
    /// Machines that are away and that no header of the list names, in the config's order
    /// ([`crate::tui::machines::away_unheaded`]); empty in the launcher's first bar and in the
    /// plain list, where every away machine has a header.
    pub away: Vec<String>,
}

/// [`BarCounts`] from one `list-panes` and the number of sessions — the launcher's
/// numbers, before the popup has a snapshot of its own (`App::bar_counts` is the popup's).
pub fn counts(panes: &[PaneInfo], sessions: usize) -> BarCounts {
    BarCounts {
        sessions,
        agents: panes.iter().filter(|p| attention::is_agent(p)).count(),
        away: Vec::new(),
    }
}

/// A theme colour as a tmux style colour: `#rrggbb`, `colourN`, or the ANSI name (the
/// `terminal` theme). ratatui's `Gray` is ANSI white and its `White` bright white, so
/// they map to tmux's `white` and `brightwhite`; `Reset` is tmux's `default`.
pub fn tmux_colour(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => format!("colour{n}"),
        Color::Reset => "default".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "white".into(),
        Color::DarkGray => "brightblack".into(),
        Color::LightRed => "brightred".into(),
        Color::LightGreen => "brightgreen".into(),
        Color::LightYellow => "brightyellow".into(),
        Color::LightBlue => "brightblue".into(),
        Color::LightMagenta => "brightmagenta".into(),
        Color::LightCyan => "brightcyan".into(),
        Color::White => "brightwhite".into(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The bar as a `status-format` value: ` taw ` in the accent colour, bold; the session
/// and agent counts; then, right-aligned, the clock. What needs you is not here: the list's
/// border says it, and the rows and machine headers show the states. Only the machines that are
/// away and that no header names ([`BarCounts::away`]) are named, before the clock.
pub fn bar_format(theme: &Theme, counts: &BarCounts) -> String {
    // Names come from the config; `#` in one is tmux's format escape, so it is doubled.
    let away = if counts.away.is_empty() {
        String::new()
    } else {
        format!(
            "#[fg={}]{}#[default]  \u{2502}  ",
            tmux_colour(theme.dim),
            words::away_names(&counts.away).replace('#', "##"),
        )
    };
    format!(
        "#[fg={},bold] taw #[default]#[fg={}]  {} session{}  \u{b7}  {} agent{}#[default]#[align=right]{away}%H:%M ",
        tmux_colour(theme.accent),
        tmux_colour(theme.fg),
        counts.sessions,
        plural(counts.sessions),
        counts.agents,
        plural(counts.agents),
    )
}

/// One entry per status row: the session's own `status-format[i]`, or `None` when the
/// row was inheriting the global value.
///
/// Stored in [`SAVED_OPTION`] as one record per row — `set<TAB><value>` or `unset` —
/// joined by `\x1f`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved(pub Vec<Option<String>>);

impl Saved {
    /// The text [`Saved::decode`] reads back.
    pub fn encode(&self) -> String {
        self.0
            .iter()
            .map(|row| match row {
                Some(value) => format!("set\t{value}"),
                None => "unset".to_string(),
            })
            .collect::<Vec<_>>()
            .join(&RECORD_SEPARATOR.to_string())
    }

    /// Reads [`Saved::encode`]'s text. A row that is not `set<TAB>…` reads as unset.
    pub fn decode(text: &str) -> Saved {
        if text.is_empty() {
            return Saved(vec![]);
        }
        Saved(
            text.split(RECORD_SEPARATOR)
                .map(|row| row.strip_prefix("set\t").map(str::to_string))
                .collect(),
        )
    }
}

fn row_name(i: usize) -> String {
    format!("{STATUS_FORMAT}[{i}]")
}

/// Whether `session` has a `status-format` array of its own — even an empty one — rather
/// than inheriting the global array. `show-options -t` without `-v` lists the session's
/// elements, or the bare option name for an array with none, and prints nothing at all
/// for an option the session inherits.
fn owns_status_format(t: &Tmux, session: &str) -> anyhow::Result<bool> {
    let own = t
        .run(&["show-options", "-t", session, STATUS_FORMAT])
        .with_context(|| format!("reading the status line of session {session}"))?;
    Ok(!own.trim().is_empty())
}

/// Puts the bar up on `session`'s first `rows` status rows: a stale record from a
/// launcher that died is undone first, then the session's own rows are read and
/// recorded in [`SAVED_OPTION`], then `bar` goes into [`BAR_OPTION`], row 0 becomes
/// [`BAR_ROW`] and every other row blank. The text before the row, so the line never
/// draws an empty bar.
///
/// A session with an array of its own records every row as set — a row its array has no
/// element for, or an empty one, as set to the empty string, which tmux draws as the same
/// blank row. A session that inherits records every row as unset. [`restore`] tells the
/// two apart by exactly that. With no rows there is nothing to swap and nothing recorded.
pub fn swap(t: &Tmux, session: &str, rows: usize, bar: &str) -> anyhow::Result<Saved> {
    recover(t, session)?;
    if rows == 0 {
        return Ok(Saved::default());
    }
    let saved = if owns_status_format(t, session)? {
        let mut own = Vec::with_capacity(rows);
        for i in 0..rows {
            own.push(Some(
                t.session_option(session, &row_name(i))?.unwrap_or_default(),
            ));
        }
        Saved(own)
    } else {
        Saved(vec![None; rows])
    };
    t.set_session_option(session, SAVED_OPTION, &saved.encode())
        .context("recording the status line before swapping it")?;
    t.set_session_option(session, BAR_OPTION, bar)?;
    for i in 0..rows {
        let value = if i == 0 { BAR_ROW } else { "" };
        t.set_session_option(session, &row_name(i), value)?;
    }
    Ok(saved)
}

/// Puts the session's rows back exactly as [`swap`] found them, then drops the bar's text
/// and, last, the record — so a restore cut short is finished by the next [`recover`].
///
/// A record whose rows are all unset is a session that inherited its line, and the only
/// way back to inheriting is to unset the whole option (see the module's notes on
/// scoping). Otherwise each row is set back to its value, or unset where the record says
/// so — [`swap`] never mixes the two, but a record written by hand may. A record with no
/// rows touches no row.
pub fn restore(t: &Tmux, session: &str, saved: &Saved) -> anyhow::Result<()> {
    if !saved.0.is_empty() && saved.0.iter().all(Option::is_none) {
        t.unset_session_option(session, STATUS_FORMAT)?;
    } else {
        for (i, row) in saved.0.iter().enumerate() {
            match row {
                Some(value) => t.set_session_option(session, &row_name(i), value)?,
                None => t.unset_session_option(session, &row_name(i))?,
            }
        }
    }
    t.unset_session_option(session, BAR_OPTION)?;
    t.unset_session_option(session, SAVED_OPTION)
}

/// A record left behind by a launcher that never got to restore: undo it now. With no
/// record, a bar's text still on the session is a popup's refresh that landed after the
/// last restore: nothing draws it, and it is dropped too.
pub fn recover(t: &Tmux, session: &str) -> anyhow::Result<()> {
    match t.session_option(session, SAVED_OPTION)? {
        Some(record) => restore(t, session, &Saved::decode(&record)),
        None if t.session_option(session, BAR_OPTION)?.is_some() => {
            t.unset_session_option(session, BAR_OPTION)
        }
        None => Ok(()),
    }
}

/// The popup's side: rewrites the bar's text ([`BAR_OPTION`]) with fresh counts, only
/// when it changed. Never a row: while a launcher has the bar up, row 0 draws the option
/// ([`BAR_ROW`]); otherwise — a `taw ui` typed in a pane, a status line that is off, a
/// swap that failed, a launcher that has already put the line back — nothing draws it,
/// and the line is untouched. `last` keeps the text, so an unchanged count costs no tmux
/// call at all; a failed write is tried again on the next call.
pub fn refresh_live(
    t: &Tmux,
    session: &str,
    theme: &Theme,
    counts: &BarCounts,
    last: &mut Option<String>,
) {
    let text = bar_format(theme, counts);
    if last.as_deref() == Some(text.as_str()) {
        return;
    }
    match t.set_session_option(session, BAR_OPTION, &text) {
        Ok(()) => *last = Some(text),
        Err(err) => log::debug!("refreshing the status bar failed: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme;
    use ratatui::style::Color;

    #[test]
    fn status_rows_follow_tmuxs_status_option() {
        assert_eq!(status_rows(None), 1);
        assert_eq!(status_rows(Some("on")), 1);
        assert_eq!(status_rows(Some("off")), 0);
        assert_eq!(status_rows(Some("0")), 0);
        assert_eq!(status_rows(Some("2")), 2);
        assert_eq!(status_rows(Some("3")), 3);
        assert_eq!(status_rows(Some("weird")), 1);
    }

    #[test]
    fn theme_colours_become_tmux_styles() {
        assert_eq!(tmux_colour(Color::Rgb(0x89, 0xb4, 0xfa)), "#89b4fa");
        assert_eq!(tmux_colour(Color::Reset), "default");
        assert_eq!(tmux_colour(Color::Blue), "blue");
        assert_eq!(tmux_colour(Color::DarkGray), "brightblack");
        assert_eq!(tmux_colour(Color::Indexed(208)), "colour208");
    }

    /// The bar names taw, the session and agent counts and, at the right, the clock.
    #[test]
    fn the_bar_names_taw_the_counts_and_the_clock() {
        let t = theme::resolve("catppuccin", &Default::default()).theme;
        let bar = bar_format(
            &t,
            &BarCounts {
                sessions: 7,
                agents: 9,
                away: Vec::new(),
            },
        );
        assert!(
            bar.starts_with("#[fg=#89b4fa,bold] taw #[default]"),
            "{bar}"
        );
        assert!(bar.contains("7 sessions  \u{b7}  9 agents"), "{bar}");
        assert!(bar.ends_with("#[align=right]%H:%M "), "{bar}");
        let one = bar_format(
            &t,
            &BarCounts {
                sessions: 1,
                agents: 1,
                away: Vec::new(),
            },
        );
        assert!(one.contains("1 session  \u{b7}  1 agent#"), "{one}");
    }

    /// What needs you is on the list's border, so the bar says none of it: no need words, no group
    /// glyphs and no rail, whatever the counts are (rule 1 of the counts' round).
    #[test]
    fn the_bar_holds_no_need_words_and_no_rail() {
        let t = theme::resolve("catppuccin", &Default::default()).theme;
        for (sessions, agents) in [(0, 0), (1, 1), (7, 9)] {
            let bar = bar_format(
                &t,
                &BarCounts {
                    sessions,
                    agents,
                    away: Vec::new(),
                },
            );
            for word in ["need", "nothing", "you", "agent_wizard_status", "\u{2502}"] {
                assert!(!bar.contains(word), "{word:?} in {bar}");
            }
            assert!(bar.contains(&format!("{agents} agent")), "{bar}");
        }
    }

    /// An away machine that no header names is named before the clock; with none away the bar has
    /// no such words, and a `#` in a name is doubled for tmux's format.
    #[test]
    fn the_bar_names_the_away_machines_before_the_clock() {
        let t = theme::resolve("catppuccin", &Default::default()).theme;
        let plain = bar_format(&t, &BarCounts::default());
        assert!(!plain.contains("not connected"), "{plain}");
        let counts = BarCounts {
            sessions: 2,
            agents: 3,
            away: vec!["studio".to_string(), "pi".to_string()],
        };
        let bar = bar_format(&t, &counts);
        assert!(bar.contains("studio, pi not connected"), "{bar}");
        assert!(bar.ends_with("%H:%M "), "{bar}");
        assert!(
            bar.find("agents").unwrap() < bar.find("studio, pi").unwrap()
                && bar.find("studio, pi").unwrap() < bar.find("%H:%M").unwrap(),
            "{bar}"
        );
        let hash = BarCounts {
            away: vec!["a#b".to_string()],
            ..BarCounts::default()
        };
        assert!(bar_format(&t, &hash).contains("a##b not connected"));
    }

    #[test]
    fn a_saved_record_round_trips_set_and_unset_rows() {
        let saved = Saved(vec![
            Some("#[align=left] #S \"quoted\"".to_string()),
            None,
            Some(String::new()),
        ]);
        let text = saved.encode();
        assert_eq!(Saved::decode(&text), saved);
        assert_eq!(Saved::decode(""), Saved(vec![]));
        assert_eq!(Saved::decode("unset"), Saved(vec![None]));
    }

    #[test]
    fn counts_come_from_the_panes() {
        let agent =
            crate::rail::tests_support::pane_with_state(Some(crate::store::AgentState::Blocked));
        let idle =
            crate::rail::tests_support::pane_with_state(Some(crate::store::AgentState::Idle));
        let shell = crate::rail::tests_support::pane_with_state(None);
        let c = counts(&[agent, idle, shell], 4);
        assert_eq!(
            c,
            BarCounts {
                sessions: 4,
                agents: 2,
                away: Vec::new(),
            }
        );
    }

    // ------------------------------------------------ against a real tmux server

    use std::time::{Duration, Instant};

    use crate::tmux::Socket;

    /// Writes into `dir` a `tmux` that is `real` run with `-u`, with no history file and `dir` for
    /// the home of the shells of a server it starts: the two-line wrapper every test server's
    /// calls go through. (`tests/common/shell_env.rs` says why a throwaway server needs both.)
    ///
    /// A tmux client that does not think its terminal speaks UTF-8 — no `$TMUX`, and a
    /// locale such as `LC_ALL=C` — prints each non-ASCII or control character of a value it
    /// reads back as `_`: `·`, `│` and the record's `\x1f` would come back mangled, and
    /// these tests would fail outside tmux for tmux's reasons, not taw's. [`Tmux::run`] is a
    /// UTF-8 client of its own (`-u`), so this wrapper is for the client a test attaches.
    fn utf8_tmux(dir: &std::path::Path, real: &std::path::Path) -> std::path::PathBuf {
        let wrapper = dir.join("tmux");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nHISTFILE=/dev/null HOME={dir} ZDOTDIR={dir} exec {} -u \"$@\"\n",
                shell_words::quote(&real.to_string_lossy()),
                dir = shell_words::quote(&dir.to_string_lossy())
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&wrapper);
        wrapper
    }

    /// A throwaway tmux server on its own `-L` socket, killed on drop — never the user's.
    /// `-f /dev/null`, so the host's tmux.conf cannot decide what a session inherits; and
    /// `/bin/sh`, which reads no rc file. Every call goes through [`utf8_tmux`].
    struct Server {
        tmux: Tmux,
        /// The one session's id (`$0`), the form `taw ui open` targets.
        session: String,
        /// Holds the `tmux -u` wrapper for as long as the server lives.
        _bin: tempfile::TempDir,
        /// The file the server listens on, which goes with it (tmux leaves it behind).
        socket_file: std::path::PathBuf,
    }

    impl Server {
        fn start() -> Server {
            // Tests run as threads of one process: the counter gives each its own socket.
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut tmux =
                Tmux::with_socket_name(&format!("taw-test-status-bar-{}-{n}", std::process::id()));
            let bin = tempfile::tempdir().unwrap();
            tmux.bin = utf8_tmux(bin.path(), &tmux.bin);
            // The guard first, so a failure below still kills the server.
            let mut server = Server {
                tmux,
                session: String::new(),
                _bin: bin,
                socket_file: std::path::PathBuf::new(),
            };
            server
                .tmux
                .run_ok(&[
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "s",
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "/bin/sh",
                ])
                .unwrap();
            server.session = server.tmux.display("s", "#{session_id}").unwrap();
            server.socket_file = server
                .tmux
                .run(&["display-message", "-p", "#{socket_path}"])
                .unwrap()
                .into();
            server
        }

        /// The bar's text, [`BAR_OPTION`], as the session holds it.
        fn bar(&self) -> Option<String> {
            self.tmux.session_option(&self.session, BAR_OPTION).unwrap()
        }

        /// A client attached to the session, running in the only pane of a second
        /// throwaway server — the acceptance suite's arrangement — so a test can read the
        /// status line as tmux draws it.
        fn attach(&self) -> Client {
            let Some(Socket::Name(socket)) = &self.tmux.socket else {
                unreachable!("Server::start names its socket");
            };
            let mut client = Client {
                outer: Tmux {
                    bin: self.tmux.bin.clone(),
                    socket: Some(Socket::Name(format!("{socket}-outer"))),
                },
                socket_file: std::path::PathBuf::new(),
            };
            let attach = format!(
                "{} -L {} attach-session -t {}",
                shell_words::quote(&self.tmux.bin.to_string_lossy()),
                shell_words::quote(socket),
                shell_words::quote(&self.session),
            );
            client
                .outer
                .run_ok(&[
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "host",
                    "-x",
                    "80",
                    "-y",
                    "6",
                    &attach,
                ])
                .unwrap();
            client.socket_file = client
                .outer
                .run(&["display-message", "-p", "#{socket_path}"])
                .unwrap()
                .into();
            client
        }

        /// `status-format` as the session itself holds it — `show-options -t` without `-v`
        /// lists each element the session owns, the bare name for an array with no
        /// elements, and nothing at all when the session inherits the global array.
        fn own_status_format(&self) -> String {
            self.tmux
                .run(&["show-options", "-t", &self.session, "status-format"])
                .unwrap()
        }

        fn row(&self, i: usize) -> Option<String> {
            self.tmux
                .session_option(&self.session, &format!("status-format[{i}]"))
                .unwrap()
        }

        fn record(&self) -> Option<String> {
            self.tmux
                .session_option(&self.session, SAVED_OPTION)
                .unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.tmux.run_ok(&["kill-server"]);
            crate::cli::test_support::take_the_socket_away(&self.tmux, &self.socket_file);
        }
    }

    /// `t` as a popup sees it when its read of [`SAVED_OPTION`] ran before the launcher's
    /// restore and its write lands after — the popup killed from outside with a refresh in
    /// flight: every read of the record still finds one, and everything else reaches the
    /// server as it is now. The wrapper lives in `dir`.
    fn before_the_restore(t: &Tmux, dir: &std::path::Path) -> Tmux {
        let script = dir.join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in *'show-options -v -t '*' {SAVED_OPTION}') echo unset; exit 0 ;; esac\nexec {} \"$@\"\n",
                shell_words::quote(&t.bin.to_string_lossy())
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);
        Tmux {
            bin: script,
            socket: t.socket.clone(),
        }
    }

    /// See [`Server::attach`]. Declared after its server, so it is killed first.
    struct Client {
        outer: Tmux,
        /// The file the second server listens on, which goes with it.
        socket_file: std::path::PathBuf,
    }

    impl Client {
        /// The status line as the client draws it: its screen's last row, tmux's default
        /// position.
        fn status_line(&self) -> String {
            self.outer
                .run(&["capture-pane", "-p", "-t", "host"])
                .map(|screen| screen.lines().last().unwrap_or_default().to_string())
                .unwrap_or_default()
        }

        /// The status line once `until` holds for it, or as it is after five seconds.
        fn status_line_once(&self, until: impl Fn(&str) -> bool) -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let line = self.status_line();
                if until(&line) || Instant::now() > deadline {
                    return line;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// The first status line drawn over the next `window` for which `bad` holds.
        fn status_line_during(
            &self,
            window: Duration,
            bad: impl Fn(&str) -> bool,
        ) -> Option<String> {
            let deadline = Instant::now() + window;
            while Instant::now() < deadline {
                let line = self.status_line();
                if bad(&line) {
                    return Some(line);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.outer.run_ok(&["kill-server"]);
            crate::cli::test_support::take_the_socket_away(&self.outer, &self.socket_file);
        }
    }

    /// The common case — a session showing the global status line — and a two-row line:
    /// while the popup is open row 0 is the bar and row 1 is blank; afterwards the session
    /// inherits the global array again. Unsetting the swapped *elements* would not do
    /// that: tmux keeps the session's own, now empty, array and draws a blank line.
    #[test]
    fn a_session_that_showed_the_global_line_shows_it_again_after_the_popup() {
        let s = Server::start();
        s.tmux.set_global_option("status", "2").unwrap();
        assert_eq!(
            s.own_status_format(),
            "",
            "precondition: the session inherits"
        );

        let saved = swap(&s.tmux, &s.session, 2, "BAR").unwrap();
        assert_eq!(saved, Saved(vec![None, None]));
        assert_eq!(
            s.row(0).as_deref(),
            Some(BAR_ROW),
            "row 0 draws the bar's option"
        );
        assert_eq!(s.bar().as_deref(), Some("BAR"));
        assert_eq!(s.row(1), None, "row 1 is blank while the popup is open");
        assert_eq!(s.record().as_deref(), Some("unset\u{1f}unset"));

        restore(&s.tmux, &s.session, &saved).unwrap();
        assert_eq!(
            s.own_status_format(),
            "",
            "the session inherits the global line again"
        );
        assert_eq!(s.bar(), None, "the bar's text goes with the restore");
        assert_eq!(s.record(), None, "the record goes with the restore");
    }

    /// What the user sees, drawn by a real client: the bar with its counts and a live clock, and
    /// neither the status rail nor any need words, while the popup is open; the popup's refresh redraws it at
    /// once although only the option changed (`status-interval 0`: no periodic redraw);
    /// and once the launcher has put the line back, a refresh that raced the restore
    /// ([`before_the_restore`]) changes nothing on screen.
    #[test]
    fn the_drawn_line_is_the_bar_with_its_clock_and_a_late_refresh_leaves_it_alone() {
        let s = Server::start();
        s.tmux.set_global_option("status-interval", "0").unwrap();
        s.tmux
            .set_global_option("@agent_wizard_status", "RAIL-7")
            .unwrap();
        let theme = crate::tui::theme::resolve("catppuccin", &Default::default()).theme;
        let counts = BarCounts {
            sessions: 1,
            agents: 2,
            away: Vec::new(),
        };
        let client = s.attach();
        let own = client.status_line_once(|l| l.starts_with("[s]"));
        assert!(
            own.starts_with("[s]"),
            "the session's own line first: {own:?}"
        );

        let saved = swap(&s.tmux, &s.session, 1, &bar_format(&theme, &counts)).unwrap();
        let drawn = client.status_line_once(|l| l.starts_with(" taw "));
        assert!(
            drawn.starts_with(" taw   1 session  \u{b7}  2 agents"),
            "{drawn:?}"
        );
        assert!(
            !drawn.contains("RAIL-7") && !drawn.contains("need") && !drawn.contains('\u{2502}'),
            "the positive half is above: no rail, no need words: {drawn:?}"
        );
        let clock = regex::Regex::new("2 agents +[0-9]{2}:[0-9]{2}$").unwrap();
        assert!(
            clock.is_match(&drawn),
            "the clock closes the line: {drawn:?}"
        );

        let mut last = None;
        let busier = BarCounts {
            agents: 3,
            ..counts.clone()
        };
        refresh_live(&s.tmux, &s.session, &theme, &busier, &mut last);
        let drawn = client.status_line_once(|l| l.contains("3 agents"));
        assert!(drawn.contains("3 agents"), "{drawn:?}");

        let dir = tempfile::tempdir().unwrap();
        let popup = before_the_restore(&s.tmux, dir.path());
        restore(&s.tmux, &s.session, &saved).unwrap();
        let back = client.status_line_once(|l| l.starts_with("[s]"));
        assert!(back.starts_with("[s]"), "the user's line is back: {back:?}");
        let later = BarCounts {
            agents: 4,
            ..counts
        };
        refresh_live(&popup, &s.session, &theme, &later, &mut last);
        assert_eq!(
            client.status_line_during(Duration::from_millis(500), |l| !l.starts_with("[s]")),
            None,
            "a refresh racing the restore changed the user's line"
        );
    }

    /// A session with a line of its own gets that exact text back — quotes, `#{}` and
    /// `%` untouched — and an element beyond the swapped rows is never touched at all.
    #[test]
    fn a_session_with_its_own_line_gets_that_exact_text_back() {
        let s = Server::start();
        let own = "#[align=left] #S \"quoted\" #{pane_title} 100%";
        s.tmux
            .set_session_option(&s.session, "status-format[0]", own)
            .unwrap();
        s.tmux
            .set_session_option(&s.session, "status-format[2]", "KEEP")
            .unwrap();
        let before = s.own_status_format();

        let saved = swap(&s.tmux, &s.session, 1, "BAR").unwrap();
        assert_eq!(saved, Saved(vec![Some(own.to_string())]));
        assert_eq!(s.row(0).as_deref(), Some(BAR_ROW));
        assert_eq!(s.bar().as_deref(), Some("BAR"));

        restore(&s.tmux, &s.session, &saved).unwrap();
        assert_eq!(s.own_status_format(), before);
        assert_eq!(s.row(0).as_deref(), Some(own));
        assert_eq!(s.bar(), None);
        assert_eq!(s.record(), None);
    }

    /// The only path that reads a `set` record back from tmux: a launcher dies with the bar
    /// up on a session whose line is its own, and the next open finds the record. A record
    /// mangled on the way back would read as "inherited", and the restore would hand the
    /// session the global line and discard the user's own for good; instead that exact line
    /// — quotes, `#{}`, `%`, `·` and `│` — is back, is what the next swap records, and is
    /// what its restore puts back.
    #[test]
    fn a_dead_launcher_over_a_sessions_own_line_gives_that_exact_line_back_on_the_next_open() {
        let s = Server::start();
        let own = "#[align=left] #S \"quoted\" \u{b7} #{pane_title} \u{2502} 100%";
        s.tmux
            .set_session_option(&s.session, "status-format[0]", own)
            .unwrap();
        let before = s.own_status_format();

        swap(&s.tmux, &s.session, 1, "OLD-BAR").unwrap();
        assert_eq!(s.record(), Some(format!("set\t{own}")));
        // ... and the launcher dies here, without restoring.

        let next = swap(&s.tmux, &s.session, 1, "NEW-BAR").unwrap();
        assert_eq!(
            next,
            Saved(vec![Some(own.to_string())]),
            "the next open recorded the user's line, not the dead launcher's bar"
        );
        assert_eq!(s.bar().as_deref(), Some("NEW-BAR"));

        restore(&s.tmux, &s.session, &next).unwrap();
        assert_eq!(s.row(0).as_deref(), Some(own));
        assert_eq!(s.own_status_format(), before);
        assert_eq!(s.bar(), None);
        assert_eq!(s.record(), None);
    }

    /// A blank row of a session's own array is the session's, not the global one: it is
    /// recorded as set-to-empty, so the restore keeps the session's array (and its line)
    /// rather than handing the session the global line it never showed.
    #[test]
    fn a_blank_row_of_a_sessions_own_line_stays_blank() {
        let s = Server::start();
        s.tmux.set_global_option("status", "2").unwrap();
        s.tmux
            .set_session_option(&s.session, "status-format[1]", "ONLY-ROW-1")
            .unwrap();

        let saved = swap(&s.tmux, &s.session, 2, "BAR").unwrap();
        assert_eq!(
            saved,
            Saved(vec![Some(String::new()), Some("ONLY-ROW-1".to_string())])
        );

        restore(&s.tmux, &s.session, &saved).unwrap();
        assert_eq!(s.row(0), None, "row 0 is blank, as before");
        assert_eq!(s.row(1).as_deref(), Some("ONLY-ROW-1"));
        assert_ne!(
            s.own_status_format(),
            "",
            "the session still owns its array"
        );
    }

    /// A launcher that died with the popup open left the bar up and its record behind.
    /// The next swap undoes that first, so what it records is the user's line — not the
    /// dead launcher's bar, which a restore would otherwise put back for good.
    #[test]
    fn a_launcher_that_died_is_undone_before_the_next_swap() {
        let s = Server::start();
        let first = swap(&s.tmux, &s.session, 1, "OLD-BAR").unwrap();
        // ... and the launcher dies here, without restoring.

        let second = swap(&s.tmux, &s.session, 1, "NEW-BAR").unwrap();
        assert_eq!(second, first, "the second swap recorded the user's line");
        assert_eq!(s.row(0).as_deref(), Some(BAR_ROW));
        assert_eq!(s.bar().as_deref(), Some("NEW-BAR"));

        restore(&s.tmux, &s.session, &second).unwrap();
        assert_eq!(s.own_status_format(), "");
        assert_eq!(s.bar(), None);
        assert_eq!(s.record(), None);
    }

    /// What a dead launcher leaves on a session that inherits its line: row 0 drawing the
    /// bar's option, the option, and the record. `recover` puts the inherited line back and
    /// drops both options. (An older launcher's stale bar written straight into row 0 is
    /// undone the same way: the rows come back from the record.)
    #[test]
    fn recover_undoes_a_stale_record() {
        let s = Server::start();
        s.tmux
            .set_session_option(&s.session, "status-format[0]", BAR_ROW)
            .unwrap();
        s.tmux
            .set_session_option(&s.session, BAR_OPTION, "STALE-BAR")
            .unwrap();
        s.tmux
            .set_session_option(&s.session, SAVED_OPTION, "unset")
            .unwrap();

        recover(&s.tmux, &s.session).unwrap();
        assert_eq!(s.own_status_format(), "");
        assert_eq!(s.bar(), None);
        assert_eq!(s.record(), None);
        // Nothing left behind: a second recover has nothing to do.
        recover(&s.tmux, &s.session).unwrap();
        assert_eq!(s.own_status_format(), "");
    }

    /// The popup writes only the bar's text. With a launcher's bar up, fresh counts reach
    /// the line through row 0's `#{T:…}`, which the popup never writes; with none — a
    /// `taw ui` typed in a pane, a status line that is off, a swap that failed — nothing
    /// draws the option, so the line is untouched. An unchanged text costs no call.
    #[test]
    fn the_popups_refresh_writes_only_the_bars_text() {
        let s = Server::start();
        let theme = crate::tui::theme::resolve("catppuccin", &Default::default()).theme;
        let counts = BarCounts {
            sessions: 1,
            agents: 2,
            away: Vec::new(),
        };
        let mut last = None;

        refresh_live(&s.tmux, &s.session, &theme, &counts, &mut last);
        assert_eq!(
            s.own_status_format(),
            "",
            "no launcher: the line is untouched"
        );

        swap(&s.tmux, &s.session, 1, "LAUNCHER-BAR").unwrap();
        let mut last = None;
        refresh_live(&s.tmux, &s.session, &theme, &counts, &mut last);
        assert_eq!(
            s.bar(),
            Some(bar_format(&theme, &counts)),
            "fresh counts replace the launcher's"
        );
        assert_eq!(
            s.row(0).as_deref(),
            Some(BAR_ROW),
            "row 0 is the launcher's"
        );

        // The same text again is not written again.
        s.tmux
            .set_session_option(&s.session, BAR_OPTION, "SENTINEL")
            .unwrap();
        refresh_live(&s.tmux, &s.session, &theme, &counts, &mut last);
        assert_eq!(s.bar().as_deref(), Some("SENTINEL"));

        // New counts are.
        let busier = BarCounts {
            agents: 5,
            ..counts
        };
        refresh_live(&s.tmux, &s.session, &theme, &busier, &mut last);
        assert_eq!(s.bar(), Some(bar_format(&theme, &busier)));
        assert_eq!(s.row(0).as_deref(), Some(BAR_ROW));
    }

    /// A race: the popup's refresh read the record, the launcher
    /// restored the line, and then the refresh's write landed, putting taw's bar back on a
    /// line with no record left to ever take it down. Played by a popup whose view of tmux
    /// is from before the restore ([`before_the_restore`]). Its write can reach only the
    /// bar's option now, which nothing draws once the line is back; the next open drops it.
    #[test]
    fn a_refresh_racing_the_restore_cannot_put_the_bar_back_and_the_next_open_drops_it() {
        let s = Server::start();
        let theme = crate::tui::theme::resolve("catppuccin", &Default::default()).theme;
        let counts = BarCounts {
            sessions: 1,
            agents: 2,
            away: Vec::new(),
        };
        let saved = swap(&s.tmux, &s.session, 1, &bar_format(&theme, &counts)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let popup = before_the_restore(&s.tmux, dir.path());
        restore(&s.tmux, &s.session, &saved).unwrap();

        let mut last = None;
        let busier = BarCounts {
            agents: 5,
            ..counts
        };
        refresh_live(&popup, &s.session, &theme, &busier, &mut last);
        assert_eq!(
            s.own_status_format(),
            "",
            "the session still inherits its line"
        );
        assert_eq!(s.record(), None);

        recover(&s.tmux, &s.session).unwrap(); // the next open
        assert_eq!(s.bar(), None, "the next open dropped the late write");
        assert_eq!(s.own_status_format(), "");
    }

    // --------------------------------------------------- the restore's arguments

    /// A stand-in `tmux` that appends each argv it is run with to `argv.log` — one line
    /// per call, every argument followed by `\x1f` — and succeeds, printing nothing. The
    /// `-u` that [`Tmux::run`] puts ahead of its arguments is dropped before the line is
    /// written, so a call reads as its verb and what follows.
    fn recording_tmux(dir: &std::path::Path) -> (Tmux, std::path::PathBuf) {
        let log = dir.join("argv.log");
        let script = dir.join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = -u ] && shift\n{{ for a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\n'; }} >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);
        (
            Tmux {
                bin: script,
                socket: None,
            },
            log,
        )
    }

    fn calls(log: &std::path::Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| {
                let line = line.strip_suffix('\u{1f}').unwrap_or(line);
                line.split('\u{1f}').map(str::to_string).collect()
            })
            .collect()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// Set rows go back to their value — the empty string included — and unset rows are
    /// unset, one element each; then the record goes.
    #[test]
    fn restore_sets_set_rows_and_unsets_unset_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (t, log) = recording_tmux(dir.path());
        let saved = Saved(vec![
            Some("#[align=left] #S".to_string()),
            None,
            Some(String::new()),
        ]);
        restore(&t, "$3", &saved).unwrap();
        assert_eq!(
            calls(&log),
            vec![
                argv(&[
                    "set-option",
                    "-t",
                    "$3",
                    "status-format[0]",
                    "#[align=left] #S"
                ]),
                argv(&["set-option", "-u", "-t", "$3", "status-format[1]"]),
                argv(&["set-option", "-t", "$3", "status-format[2]", ""]),
                argv(&["set-option", "-u", "-t", "$3", BAR_OPTION]),
                argv(&["set-option", "-u", "-t", "$3", SAVED_OPTION]),
            ]
        );
    }

    /// Every row unset means the session had no `status-format` of its own: the whole
    /// option is unset, so the session inherits the global array again.
    #[test]
    fn restore_of_a_session_that_inherited_unsets_the_whole_option() {
        let dir = tempfile::tempdir().unwrap();
        let (t, log) = recording_tmux(dir.path());
        restore(&t, "$3", &Saved(vec![None, None])).unwrap();
        assert_eq!(
            calls(&log),
            vec![
                argv(&["set-option", "-u", "-t", "$3", "status-format"]),
                argv(&["set-option", "-u", "-t", "$3", BAR_OPTION]),
                argv(&["set-option", "-u", "-t", "$3", SAVED_OPTION]),
            ]
        );
    }

    /// A record with no rows restores no row: in particular it never unsets a
    /// `status-format` the swap did not touch. The bar's text and the record still go.
    #[test]
    fn restore_of_no_rows_touches_no_row_and_drops_the_bar_and_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let (t, log) = recording_tmux(dir.path());
        restore(&t, "$3", &Saved(vec![])).unwrap();
        assert_eq!(
            calls(&log),
            vec![
                argv(&["set-option", "-u", "-t", "$3", BAR_OPTION]),
                argv(&["set-option", "-u", "-t", "$3", SAVED_OPTION]),
            ]
        );
    }
}
