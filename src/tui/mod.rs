//! `taw ui`: herdr's sidebar and overlays, inside a `tmux display-popup`.
//!
//! tmux has no persistent sidebar, so herdr's always-on agent panel becomes a popup this
//! module draws: one tree of sessions and their panes, a screen preview of the highlighted
//! row, and the overlays (project picker, help, options, prompts, a confirmation, the
//! lettered menus).
//!
//! The layering is deliberate, and each part is testable on its own:
//!
//! - [`data`] turns one read of tmux into a [`Snapshot`](data::Snapshot);
//! - [`app`] is a pure state machine over that snapshot — keys and clicks in, an
//!   [`Action`](actions::Action) out, with [`tree`] deciding which rows appear, in what
//!   order, and how each one matched the filter, [`keys`] deciding what a key means and
//!   [`prefix`] catching tmux's own prefix chord;
//! - [`view`] draws a frame and records where the rows landed, for the mouse;
//! - [`actions`] is the only code here that changes tmux.
//!
//! This module is the loop that connects them, plus the terminal state a full-screen TUI
//! has to put back however it exits — including a panic.

pub mod actions;
pub mod agent_view;
pub mod app;
mod app_screen;
pub mod data;
#[cfg(test)]
pub mod fixtures;
pub mod input;
pub mod keys;
pub mod machines;
pub mod overlays;
pub mod pasting;
pub mod picker;
pub mod prefix;
pub mod preview;
pub mod rows;
pub mod theme;
pub mod tree;
pub mod view;

use std::io::{Stdout, Write, stdout};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::machine::config::load_machines;
use crate::machine::files;
use crate::machine::{Machine, MachineView, words};
use crate::paths::Paths;
use crate::session::worktree::WorktreeRecord;
use crate::tmux::Tmux;
use app::App;
pub use app_screen::{CONFIG_UNCHANGED_LOG, GoTarget, run_app};
use prefix::Chord;

/// How often the popup re-reads tmux and re-captures the preview.
pub const TICK: Duration = Duration::from_millis(500);

/// How long a workspace's git status is reused before the worker refreshes it.
pub const GIT_TTL: Duration = Duration::from_secs(2);

/// Runs the popup until the user quits or jumps somewhere.
///
/// `taw_bin` is this executable's path: the settings overlay reports it, and the worktree
/// verbs pass it to `session::worktree`, so a worktree's new session gets the same binary
/// this popup came from.
///
/// `origin` is the pane the popup was opened from — the row the cursor opens on, and the
/// pane `prefix+O` walks from. It is handed in rather than read here: `display-popup -E`
/// does not give its command the invoking pane's environment, so `$TMUX_PANE` is unset
/// inside the popup. `taw ui open` resolves the pane while tmux still knows it and passes
/// `--from` (`cli::misc::ui` also covers a `taw ui` typed in a pane).
///
/// `client` is the client the popup was opened on, which the launcher named (`--client`):
/// every switch the popup makes moves that client, however many share its session, and the
/// session it marks as the one to go back to is that client's own last one. `None` for a
/// `taw ui` typed in a pane, whose client is then picked from those attached.
///
/// `bar` is `--bar`: the launcher swapped the status line for taw's bar and this popup's
/// ticks should keep its counts fresh. `false` for every other way this popup runs — typed
/// in a pane, a launcher that opened without swapping — so [`refresh_bar`] never writes
/// `@agent_wizard_bar` on any session.
///
/// `new_session` is `--new-session`: the popup starts with the new-session picker open over
/// the tree, which is where `taw open` with nothing to open wants to be, and Esc on that
/// picker closes the popup ([`App::start_on_new_session_picker`]).
#[allow(clippy::too_many_arguments)]
pub fn run(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    origin: Option<&str>,
    client: Option<&str>,
    bar: bool,
    new_session: bool,
) -> anyhow::Result<()> {
    // `[machines]` is read once, with `cfg`: a table that does not load whole is no machines, and a config edited
    // while the popup is open does not make its rows come and go. The keeper and the files are looked at every tick.
    let machines = load_machines(paths).unwrap_or_default();
    let git = data::GitWorker::new(GIT_TTL);
    let snapshot = data::build(tmux, cfg, &git, origin, client)?;
    // The theme is drawn from the first frame as the client's own light or dark mode says (`auto_switch`), so it is
    // chosen from the snapshot just read; `shown` is the name on screen, for the ticks that follow to compare with.
    let mut shown = theme::chosen_name(
        &cfg.theme,
        theme::Appearance::parse(snapshot.client_theme.as_deref().unwrap_or_default()),
    )
    .to_string();
    let resolved = theme::resolve(&shown, &cfg.theme.custom);
    for warning in &resolved.warnings {
        log::warn!("theme: {warning}");
    }
    let settings = overlays::settings_text(cfg, paths, &resolved, taw_bin);
    let mut app = App::new(cfg, resolved.theme, settings, snapshot);

    // The three things `App` cannot reach on its own: tmux's prefix, `zoxide`'s history,
    // and the tracked worktrees (below, with the first refresh). `App::new` already took
    // `$HOME` for the picker's tilde forms.
    app.chord = Chord::from_tmux(tmux);
    if app.chord.is_none() {
        log::warn!(
            "neither tmux's `prefix` nor its `prefix2` option is a key the popup can read: its \
             prefix chord is off, so prefix+O cannot be pressed from inside the popup"
        );
    }
    // Through the setter: the search preset's filter, and so its directories, is open from
    // the first frame, and this is where they are listed, once, with zoxide's in them.
    app.set_zoxide(zoxide_dirs(cfg));
    if new_session {
        app.start_on_new_session_picker();
    }

    let mut screen = Screen::enter()?;
    refresh(tmux, cfg, paths, &machines, &git, &mut app, origin, client);
    follow_theme(cfg, paths, taw_bin, &mut app, &mut shown);
    // taw's bar on the status line of the session the popup was opened from: `taw ui open`
    // put it up and takes it down; the popup keeps its counts fresh by writing the bar's
    // text, never the line itself. Only while the origin pane is in the snapshot, whose
    // session is the one the launcher swapped — without it `current_session` falls back to
    // any attached session. Fixed for the popup's life, like the launcher's restore.
    let bar_session = app
        .snap
        .own_pane
        .as_ref()
        .and(app.snap.current_session.clone());
    let mut last_bar: Option<String> = None;
    refresh_bar(tmux, &app, bar, bar_session.as_deref(), &mut last_bar);
    let mut last_tick = Instant::now();
    let mut pasting = pasting::Pasting::default().on_client(client);
    loop {
        screen
            .terminal
            .draw(|frame| view::draw(frame, &mut app))
            .map_err(|e| anyhow::anyhow!("drawing the popup: {e}"))?;

        if event::poll(TICK.saturating_sub(last_tick.elapsed()))? {
            let action = match event::read()? {
                // Presses only: a terminal speaking the kitty keyboard protocol also
                // reports releases and repeats, which would double every keystroke.
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => None,
            };
            if let Some(action) = action
                && perform(
                    tmux,
                    cfg,
                    paths,
                    &machines,
                    taw_bin,
                    &git,
                    &mut app,
                    &mut pasting,
                    &action,
                    origin,
                    client,
                )?
            {
                break;
            }
            // Whatever the event did to the cursor, the body is the title's pane.
            follow_preview(tmux, &mut app);
        }
        // The end of a paste that ran meanwhile: its toast comes when it ends.
        if let Some(words) = pasting.poll() {
            app.toast(words);
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            refresh(tmux, cfg, paths, &machines, &git, &mut app, origin, client);
            follow_theme(cfg, paths, taw_bin, &mut app, &mut shown);
            refresh_bar(tmux, &app, bar, bar_session.as_deref(), &mut last_bar);
        }
        if app.quit {
            break;
        }
    }
    // Leaving never waits for a paste under way: it runs as a process of its own and finishes on its own.
    Ok(())
}

/// Draws the theme the popup's client's light or dark mode asks for: with `[theme] auto_switch` on, the snapshot
/// this tick read carries what `#{client_theme}` said, and a change of the theme name it chooses (`shown` is the
/// name on screen) redraws the palette and the options overlay's text from the next frame. Reads nothing itself,
/// so a popup that follows the terminal costs no process a tick; with `auto_switch` off the name never changes
/// and this does nothing.
fn follow_theme(cfg: &Config, paths: &Paths, taw_bin: &Path, app: &mut App, shown: &mut String) {
    if !cfg.theme.auto_switch {
        return;
    }
    let appearance = theme::Appearance::parse(app.snap.client_theme.as_deref().unwrap_or_default());
    let want = theme::chosen_name(&cfg.theme, appearance);
    if want == shown {
        return;
    }
    let resolved = theme::resolve(want, &cfg.theme.custom);
    app.settings = overlays::settings_text(cfg, paths, &resolved, taw_bin);
    app.theme = resolved.theme;
    *shown = want.to_string();
}

/// Rewrites the bar's text with the snapshot's counts, when the popup has one to keep
/// fresh — see [`crate::cli::status_bar::refresh_live`], which writes only that text, so a
/// line no launcher has swapped, or one a launcher has already put back, is never touched.
/// `bar` is `run`'s own flag, not derived from `session`: a popup the launcher did not
/// open with `--bar` — `taw ui` typed by hand, `status off`, a launcher whose swap failed
/// — can still resolve a `session` just fine (it is simply the client's own) and must
/// never write `@agent_wizard_bar` there regardless.
fn refresh_bar(
    tmux: &Tmux,
    app: &App,
    bar: bool,
    session: Option<&str>,
    last: &mut Option<String>,
) {
    if bar && let Some(session) = session {
        crate::cli::status_bar::refresh_live(tmux, session, &app.theme, &app.bar_counts(), last);
    }
}

/// `zoxide`'s directories for the project picker and the filter's directories, in tilde form.
///
/// Run once, when the popup opens, and never again: `zoxide query -l` is a subprocess,
/// the picker ranks whatever list it is handed, and a keystroke that shelled out would be
/// felt. An empty list is the honest answer when `[sessions] zoxide` is off or `zoxide` is
/// not installed — the source is then simply absent, not an error, and the picker still
/// has filesystem completion and `[sessions] project_dirs`.
pub(crate) fn zoxide_dirs(cfg: &Config) -> Vec<String> {
    if !cfg.sessions.zoxide {
        return Vec::new();
    }
    let Ok(bin) = which::which("zoxide") else {
        log::debug!("zoxide is not installed: the project picker lists no history");
        return Vec::new();
    };
    crate::session::projects::zoxide_candidates(&|args| zoxide_within(&bin, args, ZOXIDE_BOUND))
}

/// The longest `zoxide query -l` is waited for: a `zoxide` that hangs gives no history, as a
/// missing one does, and never holds the popup or a `taw session candidates` over ssh.
const ZOXIDE_BOUND: Duration = Duration::from_secs(5);

/// What `bin args` prints on stdout when it succeeds within `bound`; `None` when it fails, cannot
/// start, or is still running at the end of `bound` (then it is killed: it is this function's own child).
fn zoxide_within(bin: &Path, args: &[&str], bound: Duration) -> Option<String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a thread: a full pipe would otherwise stop the child before it could exit.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + bound;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // Not joined when it was killed: a grandchild may still hold the pipe, and the thread ends with it.
    status.filter(|status| status.success())?;
    let bytes = reader.join().ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Runs one action, reporting whether the popup should close.
// One call site, the run loop, which already holds every one of these; bundling them into
// a struct would only move the same list one line up.
#[allow(clippy::too_many_arguments)]
fn perform(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    machines: &[Machine],
    taw_bin: &Path,
    git: &data::GitWorker,
    app: &mut App,
    pasting: &mut pasting::Pasting,
    action: &actions::Action,
    origin: Option<&str>,
    client: Option<&str>,
) -> anyhow::Result<bool> {
    // The paste-image key runs in the background: the popup keeps drawing and taking keys, and its toast
    // comes when the paste ends ([`pasting::Pasting`]).
    if let actions::Action::PasteImage { pane } = action {
        pasting.press(tmux, paths, pane, |words| app.toast(words));
        return Ok(false);
    }
    let outcome = actions::exec(tmux, cfg, paths, taw_bin, action, client)?;
    let done = finish(
        tmux, cfg, paths, machines, git, app, action, outcome, origin, client,
    );
    if done.exit {
        return Ok(true);
    }
    if done.closed {
        // Keys typed while a close ran were typed at its confirmation, not at the tree.
        while event::poll(Duration::ZERO)? {
            let _ = event::read()?;
        }
    }
    Ok(false)
}

/// What the popup does next, once an action has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The popup is done: the user is looking at where they went.
    pub exit: bool,
    /// The action was a close, which Enter rests after ([`App::after_action`]).
    pub closed: bool,
}

/// What the run loop does with `outcome`, which `action` came back with: show its toast and put
/// its question, and unless the popup is leaving read tmux again, since the action changed it.
/// Apart from the terminal, which only the loop reads, this is everything it does after an
/// action, so a test drives a popup through real tmux by calling it.
// One call site in the loop and the tests, which already hold every one of these.
#[allow(clippy::too_many_arguments)]
pub fn finish(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    machines: &[Machine],
    git: &data::GitWorker,
    app: &mut App,
    action: &actions::Action,
    outcome: actions::Outcome,
    origin: Option<&str>,
    client: Option<&str>,
) -> Finished {
    if let Some(toast) = outcome.toast {
        app.toast(toast);
    }
    if outcome.exit {
        return Finished {
            exit: true,
            closed: false,
        };
    }
    // The action changed tmux, so show the result now rather than up to a tick later.
    refresh(tmux, cfg, paths, machines, git, app, origin, client);
    // After the refresh, so the question is the last thing the screen was told.
    if let Some(question) = outcome.ask {
        app.ask(question);
    }
    Finished {
        exit: false,
        closed: app.after_action(action, Instant::now()),
    }
}

/// Captures the preview again when the cursor row's pane is not the one the preview shows,
/// so the title and the body always name the same pane. The run loop calls it after each
/// event; a test drives a popup through real tmux by calling it.
///
/// Only the preview is read, not the snapshot: the tick still refreshes everything. A row
/// with no pane clears the body. With `[ui] preview` off, nothing runs. A failed capture is
/// logged and leaves no text, as in [`refresh`].
pub fn follow_preview(tmux: &Tmux, app: &mut App) {
    if !app.show_preview {
        return;
    }
    let pane = app.preview_pane();
    if pane.as_deref() == app.preview_shows() {
        return;
    }
    let text = capture_preview(tmux, pane.as_deref());
    app.set_preview(pane, text);
}

/// The styled screen of `pane`, or nothing when there is no pane or the capture failed.
///
/// An id with a `:` in it (`studio:%7`) is another machine's, never a target of this machine's tmux:
/// a session here could be called `studio`. It is not asked of tmux.
fn capture_preview(tmux: &Tmux, pane: Option<&str>) -> Option<ratatui::text::Text<'static>> {
    pane.filter(|pane| !pane.contains(':'))
        .and_then(|pane| match preview::capture(tmux, pane) {
            Ok(text) => Some(text),
            Err(e) => {
                log::debug!("capturing pane {pane} for the preview failed: {e:#}");
                None
            }
        })
}

/// Rebuilds the snapshot and the preview.
///
/// A failed read is logged and dropped: the popup keeps showing the last good snapshot,
/// which is the right answer when a pane died a moment ago or a `list-panes` raced a
/// `kill-session`. `machines` is the `[machines]` table the popup started with.
// Called by the loop, `finish` and the tests, each holding every one of these.
#[allow(clippy::too_many_arguments)]
fn refresh(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    machines: &[Machine],
    git: &data::GitWorker,
    app: &mut App,
    origin: Option<&str>,
    client: Option<&str>,
) {
    let snapshot = match data::build(tmux, cfg, git, origin, client) {
        Ok(snapshot) => Some(snapshot),
        Err(e) => {
            log::debug!("popup refresh failed, keeping the last snapshot: {e:#}");
            None
        }
    };
    // Cheap: `worktrees.json` plus one `exists()` per record, no git subprocess.
    let worktrees = match crate::session::worktree::list(tmux, paths) {
        Ok(worktrees) => Some(worktrees),
        Err(e) => {
            log::debug!("listing worktrees for the tree failed: {e:#}");
            None
        }
    };
    // The machines' rows from the files of the app that keeps them: each read is of what this tick read.
    let (snapshot, worktrees) = match (snapshot, worktrees) {
        (Some(snapshot), Some(worktrees)) => {
            let (snapshot, worktrees) = with_machines(paths, machines, snapshot, worktrees);
            (Some(snapshot), Some(worktrees))
        }
        (Some(snapshot), None) => (
            Some(with_machines(paths, machines, snapshot, Vec::new()).0),
            None,
        ),
        // The snapshot is the last good one, kept as it is (its machines' `live` flags too: the popup runs inside
        // this Mac's tmux server, so a read that keeps failing means the popup is closing). The worktree records
        // are this tick's, so the machines' closed worktrees are merged into them or their rows would vanish for
        // the tick.
        (None, Some(worktrees)) => (
            None,
            Some(machines::merge_worktrees(
                worktrees,
                &machine_views(paths, machines),
            )),
        ),
        (None, None) => (None, None),
    };
    // Both together: a setter each would rebuild the whole tree twice per tick, and the
    // frame between the two rebuilds would show worktrees that do not match the snapshot.
    app.set_data(snapshot, worktrees);
    let pane = app.preview_pane();
    let text = capture_preview(tmux, pane.as_deref());
    app.set_preview(pane, text);
}

/// What the popup knows of each machine of `machines` (the `[machines]` table the popup started with), read from
/// the files the app that keeps the connections writes (never through a connection of its own): in the config's
/// order, live when a keeper is alive, the file says live and the last line from the machine is at most 15 s old
/// (`now - last_seen <= 15 s`, [`files::is_live`] through [`files::view_of`]). Empty, and nothing is read, when
/// there is no table (or it did not load whole): then the popup is as it is without machines. When no app keeps
/// the files, each machine of the table is a view with [`words::NO_KEEPER`] and `no_keeper` set, and no machine
/// file is read for its rows or its `live` flag (one left by an app that is gone is stale); only its
/// `this_server` is taken ([`left_as_this_server`]). The keeper is only asked for when there is a table;
/// nothing here writes or makes a folder.
fn machine_views(paths: &Paths, machines: &[Machine]) -> Vec<MachineView> {
    if machines.is_empty() {
        return Vec::new();
    }
    if !files::keeper_alive(paths) {
        return machines
            .iter()
            .map(|m| {
                // What the app said of it, as the keeper's own connection says it (`live` false, these words).
                let this_server = left_as_this_server(paths, &m.name);
                MachineView {
                    name: m.name.clone(),
                    live: false,
                    words: if this_server {
                        words::this_server(&m.name)
                    } else {
                        words::NO_KEEPER.to_string()
                    },
                    short: if this_server {
                        String::new()
                    } else {
                        words::NO_KEEPER.to_string()
                    },
                    state: None,
                    last_seen: None,
                    this_server,
                    no_keeper: !this_server,
                }
            })
            .collect();
    }
    let now_ms = files::now_ms();
    machines
        .iter()
        .map(|m| files::view_of(files::read(paths, &m.name).as_ref(), &m.name, true, now_ms))
        .collect()
}

/// How many machines the list holds, as `App::bar_counts` counts them: this one and each of the config's
/// `[machines]` that is not this machine's own tmux server ([`machine_views`]), connected or not. The launcher's
/// first bar says it before the popup has read its first snapshot. Reads only the config and the files an app
/// keeps (no connection, nothing slow), and writes nothing.
pub(crate) fn machine_count(paths: &Paths) -> usize {
    let machines = load_machines(paths).unwrap_or_default();
    1 + machine_views(paths, &machines)
        .iter()
        .filter(|view| !view.this_server)
        .count()
}

/// Whether the file an earlier app run left for `name` says that entry is this machine's own tmux server. That
/// is all the popup takes from a file nobody keeps: what an entry is does not age as its rows and its `live`
/// flag do, and with no app the popup has no other way to know ([`words::this_server`] is what the app says).
/// The popup then counts the entry as this machine: no header, no `not connected`.
fn left_as_this_server(paths: &Paths, name: &str) -> bool {
    files::read(paths, name).is_some_and(|file| file.this_server)
}

/// This machine's rows and closed worktrees with those of the machines of `machines` added after them
/// ([`machine_views`]), as the popup shows them: a machine's rows are read-only here, and Enter on one is the
/// doorway into the app. With no machines `snapshot` and `worktrees` come back as they are.
fn with_machines(
    paths: &Paths,
    machines: &[Machine],
    snapshot: data::Snapshot,
    worktrees: Vec<WorktreeRecord>,
) -> (data::Snapshot, Vec<WorktreeRecord>) {
    let views = machine_views(paths, machines);
    (
        machines::merge(snapshot, &views, files::now_ms()),
        machines::merge_worktrees(worktrees, &views),
    )
}

/// The terminal state the popup takes over, and puts back however it exits.
///
/// Raw mode, the alternate screen and mouse capture all have to be undone or the user is
/// left with an unusable terminal. `Drop` covers a normal return and an error return; the
/// panic hook covers a panic, which `Drop` alone would not — a panic message printed into
/// the alternate screen disappears with it.
struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// The terminal is lent to a program and the screen has not been taken back: it was put away already. True from
/// [`Screen::away`] (after its `restore`) until [`Screen::back`] has entered the alternate screen again. While it is
/// true neither the app's watchdog nor [`Screen`]'s `Drop` writes the screen's restore bytes (a panic's hook still
/// does). It does not claim that every program puts the terminal back: a tmux client's server does it as the client
/// goes, an ssh does not. It is a static because the watchdog thread reads it. There is one screen in a process, the
/// popup never lends the terminal, and [`Screen::enter`] clears it.
pub(crate) static LENT: AtomicBool = AtomicBool::new(false);

impl Screen {
    fn enter() -> anyhow::Result<Screen> {
        // A screen that is entered is not lent: a stale true from an earlier screen in this process would
        // silence the new one's `Drop` and the watchdog.
        LENT.store(false, Ordering::SeqCst);
        install_panic_hook();
        enable_raw_mode().map_err(|e| anyhow::anyhow!("entering raw mode: {e}"))?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture).map_err(|e| {
            let _ = disable_raw_mode();
            anyhow::anyhow!("entering the popup's screen: {e}")
        })?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout())).map_err(|e| {
            restore();
            anyhow::anyhow!("creating the terminal: {e}")
        })?;
        Ok(Screen { terminal })
    }
}

/// The three things lending the terminal does to it, so that their order can be tested.
trait Lending {
    /// The screen is left as it was found.
    fn away(&mut self);
    /// The screen is taken back: raw mode, the alternate screen, the mouse.
    fn back(&mut self) -> anyhow::Result<()>;
    /// Whatever was typed meanwhile and left unread is dropped.
    fn drain(&mut self);
    /// One frame of `app` is drawn, for a wait that holds the screen and says what it waits for. Only while the
    /// screen is the app's own; a screen that draws nowhere does nothing.
    fn draw(&mut self, _app: &mut app::App) {}
    /// `line` is printed on the plain terminal and the screen waits for Enter, until the terminal hangs up or
    /// `stop` says so (it is asked every [`HOLD_LOOK`] and does the waiting loop's own duties). A screen that
    /// draws nowhere holds nothing.
    fn hold(&mut self, _line: &str, _stop: &mut dyn FnMut() -> bool) {}
}

/// How often a wait for Enter looks at `stop`.
const HOLD_LOOK: Duration = Duration::from_millis(150);

/// Prints `line` on `out`, then waits until something can be read from `fd` (a person pressed Enter, which a
/// terminal in its cooked modes delivers as a line), the terminal hangs up (a read of 0 bytes, or any error that
/// is not an interrupted call), or `stop` says so. The wait is a `poll` with a timeout, never a spin: a hung-up
/// terminal makes `poll` answer at once, and the read after it ends the wait.
fn hold_for_enter(
    fd: libc::c_int,
    out: &mut impl Write,
    line: &str,
    stop: &mut dyn FnMut() -> bool,
) {
    // What was typed before the words came (a second Enter on the row) is not the answer to them.
    // SAFETY: `tcflush` of a descriptor number; a descriptor that is no terminal is refused, which is all.
    unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
    let _ = write!(out, "\r{line}\r\n");
    let _ = out.flush();
    loop {
        if stop() {
            return;
        }
        let mut waiting = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` of one descriptor structure of ours, with a timeout in milliseconds.
        let polled = unsafe { libc::poll(&mut waiting, 1, HOLD_LOOK.as_millis() as libc::c_int) };
        if polled < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if polled == 0 {
            continue;
        }
        let mut byte = [0u8; 256];
        // SAFETY: `read` into a buffer of ours, no more than its length.
        let got = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), byte.len()) };
        if got < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        // A line (Enter), an end of input or an error all end the wait.
        return;
    }
}

/// [`lend_through`] for a program whose failure is told on its own screen: when `held` finds words for what `run`
/// gave, they are printed on the plain terminal (with the program's own words still on it) and the screen waits
/// for Enter ([`Lending::hold`]) before it is taken back. `with` is what `run`, `stop` and the wait share; `stop`
/// is the wait's duties and its answer to whether to end it. Nothing is held when `ended` says so (the app was
/// told to end), and the wait is not begun again after it ends.
fn lend_through_holding<L: Lending, C, T, U>(
    on: &mut L,
    with: &mut C,
    run: impl FnOnce(&mut C) -> T,
    held: impl FnOnce(&T) -> Option<String>,
    mut stop: impl FnMut(&mut C) -> bool,
    ended: impl Fn() -> bool,
    after: impl FnOnce() -> U,
) -> anyhow::Result<(T, Option<U>)> {
    on.away();
    let out = run(with);
    if ended() {
        return Ok((out, None));
    }
    if let Some(line) = held(&out) {
        on.hold(&line, &mut || stop(with) || ended());
        if ended() {
            return Ok((out, None));
        }
    }
    on.back()?;
    let then = after();
    on.drain();
    Ok((out, Some(then)))
}

/// Lends the terminal to `run`, takes the screen back, runs `after` (which may wait, so it does so with the
/// screen back and not with a cooked terminal that echoes keys and passes Ctrl-C and Ctrl-Z on), then drops
/// what was typed meanwhile. When `ended` says so once `run` is over (the app was told to end or its
/// supervisor is gone, and the program it lent the terminal to was told with it) the screen is not taken
/// back, nothing waits and nothing is dropped: the terminal stays as the shell will find it, and `after`
/// gives `None`.
fn lend_through<L: Lending, T, U>(
    on: &mut L,
    run: impl FnOnce() -> T,
    ended: impl FnOnce() -> bool,
    after: impl FnOnce() -> U,
) -> anyhow::Result<(T, Option<U>)> {
    on.away();
    let out = run();
    if ended() {
        return Ok((out, None));
    }
    on.back()?;
    let then = after();
    on.drain();
    Ok((out, Some(then)))
}

/// The modes of the terminal on standard input as [`Screen::away`] left them, the cooked ones the program that is
/// lent the terminal finds. An ssh that is killed leaves the terminal in the raw modes it set (it undoes them only
/// as it exits), and crossterm forgets the modes it saved once raw mode is off, so the app puts these back.
static COOKED: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);

/// The cooked modes saved by the last [`Screen::away`], if the terminal could be read.
pub(crate) fn cooked_modes() -> Option<libc::termios> {
    *COOKED.lock().unwrap_or_else(|e| e.into_inner())
}

impl Lending for Screen {
    fn away(&mut self) {
        restore();
        // SAFETY: `tcgetattr` fills a `termios` of ours; a failure (no terminal) leaves it unused.
        let saved = unsafe {
            let mut modes: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(0, &mut modes) == 0).then_some(modes)
        };
        *COOKED.lock().unwrap_or_else(|e| e.into_inner()) = saved;
        LENT.store(true, Ordering::SeqCst);
    }

    fn back(&mut self) -> anyhow::Result<()> {
        enable_raw_mode().map_err(|e| anyhow::anyhow!("entering raw mode: {e}"))?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| anyhow::anyhow!("entering the screen again: {e}"))?;
        // The screen is entered again: from here it is this one's to put away.
        LENT.store(false, Ordering::SeqCst);
        self.terminal
            .clear()
            .map_err(|e| anyhow::anyhow!("clearing the screen: {e}"))
    }

    fn drain(&mut self) {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if event::read().is_err() {
                break;
            }
        }
    }

    fn draw(&mut self, app: &mut app::App) {
        let _ = self.terminal.draw(|frame| view::draw(frame, app));
    }

    fn hold(&mut self, line: &str, stop: &mut dyn FnMut() -> bool) {
        // An ssh that was ended from outside leaves the terminal raw: the cooked modes make Enter a line and
        // show what is typed.
        if let Some(saved) = cooked_modes() {
            // SAFETY: `tcsetattr` of standard input with modes that were read from it.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        }
        hold_for_enter(0, &mut stdout(), line, stop);
        // What was typed on the way to Enter is read and gone; the screen is taken back right after.
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        match written_on_drop(LENT.load(Ordering::SeqCst)) {
            Some(bytes) => {
                let mut out = stdout();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
                let _ = disable_raw_mode();
                let _ = self.terminal.show_cursor();
            }
            // A second leave of the alternate screen would restore the cursor the first one saved, over
            // whatever the program that had the terminal wrote since.
            None => {
                let _ = disable_raw_mode();
            }
        }
    }
}

/// What dropping a [`Screen`] writes: the bytes that put the screen away, or nothing when the terminal is
/// lent and the screen was put away already. ratatui's own `Terminal` drop still shows the cursor, which is
/// harmless.
fn written_on_drop(lent: bool) -> Option<Vec<u8>> {
    (!lent).then(restore_sequence)
}

/// Undoes everything [`Screen::enter`] did. Safe to call twice, and safe to call when
/// nothing was entered: every step is independent, and its failure is ignored because
/// there is nothing useful left to do about it.
fn restore() {
    let mut out = stdout();
    let _ = out.write_all(&restore_sequence());
    let _ = out.flush();
    let _ = disable_raw_mode();
}

/// The bytes that put the screen away: mouse capture off, the alternate screen left, the cursor shown. One
/// place for them, so that [`restore`] and the app's watchdog (which writes them with a bare `write(2)`,
/// since it must not wait for anything the main loop holds) cannot differ.
pub(crate) fn restore_sequence() -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = queue!(bytes, DisableMouseCapture, LeaveAlternateScreen, Show);
    bytes
}

/// Restores the terminal before a panic prints, then lets the previous hook run.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    fn script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("zoxide");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_zoxide_that_answers_is_read_and_one_that_hangs_gives_nothing_at_the_bound() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        // Written whole before it runs, so Linux has no busy file to refuse (ETXTBSY).
        let answers = script(dir.path(), "echo /a; echo /b");
        assert_eq!(
            super::zoxide_within(&answers, &["query", "-l"], Duration::from_secs(20)),
            Some("/a\n/b\n".to_string())
        );
        let fails = script(dir.path(), "echo /a; exit 1");
        assert_eq!(
            super::zoxide_within(&fails, &[], Duration::from_secs(20)),
            None
        );
        let hangs = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        assert_eq!(
            super::zoxide_within(&hangs, &[], Duration::from_millis(300)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A log of what lending the terminal did, in order.
    struct Logged(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>);

    impl Lending for Logged {
        fn away(&mut self) {
            self.0.borrow_mut().push("screen_away");
        }
        fn back(&mut self) -> anyhow::Result<()> {
            self.0.borrow_mut().push("screen_back");
            Ok(())
        }
        fn drain(&mut self) {
            self.0.borrow_mut().push("drain");
        }
        fn hold(&mut self, _line: &str, _stop: &mut dyn FnMut() -> bool) {
            self.0.borrow_mut().push("hold");
        }
    }

    /// A pipe: its read end, and its write end (closed by [`close`]).
    fn pipe() -> (libc::c_int, libc::c_int) {
        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: `pipe` fills two descriptor numbers of ours.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        (ends[0], ends[1])
    }

    fn close(fd: libc::c_int) {
        // SAFETY: a descriptor this test opened and closes once.
        unsafe { libc::close(fd) };
    }

    /// How many times `hold_for_enter` asked `stop` before it returned for what happens on `fd`, and what it
    /// printed. `stop_at` is the call at which `stop` says yes (0: never).
    fn held(fd: libc::c_int, stop_at: usize) -> (usize, String, std::time::Duration) {
        let mut out = Vec::new();
        let mut asked = 0;
        let began = Instant::now();
        hold_for_enter(fd, &mut out, "the line", &mut || {
            asked += 1;
            asked == stop_at
        });
        (asked, String::from_utf8(out).unwrap(), began.elapsed())
    }

    /// A line on the terminal ends the wait, and the words are printed first, on a line of their own.
    #[test]
    fn a_line_typed_ends_the_hold_after_the_words_are_printed() {
        let (read, write) = pipe();
        // SAFETY: `write` of two bytes to the pipe's write end.
        unsafe { libc::write(write, b"\n".as_ptr().cast(), 1) };
        let (asked, printed, _) = held(read, 0);
        assert_eq!(printed, "\rthe line\r\n");
        assert_eq!(asked, 1, "asked once, before the line was read");
        close(read);
        close(write);
    }

    /// A terminal that hangs up reads 0 bytes for ever: the wait ends at once and does not spin.
    #[test]
    fn a_hung_up_input_ends_the_hold_without_spinning() {
        let (read, write) = pipe();
        close(write);
        let (asked, _, took) = held(read, 0);
        assert_eq!(asked, 1);
        assert!(took < Duration::from_secs(5), "{took:?}");
        close(read);
    }

    /// The same on a real terminal device whose other side is gone (a window that closed).
    #[test]
    fn a_terminal_whose_other_side_is_gone_ends_the_hold_without_spinning() {
        let (mut master, mut slave) = (0 as libc::c_int, 0 as libc::c_int);
        // SAFETY: `openpty` fills two descriptor numbers of ours; no name, modes or size are asked for.
        let made = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(made, 0, "no pty");
        close(master);
        let (asked, _, took) = held(slave, 0);
        assert!(asked <= 2, "the wait spun: asked {asked} times");
        assert!(took < Duration::from_secs(5), "{took:?}");
        close(slave);
    }

    /// While nothing is typed the wait goes on, looking at `stop` every beat and not oftener, and ends when it
    /// says so.
    #[test]
    fn the_hold_looks_at_stop_each_beat_and_ends_when_it_says_so() {
        let (read, write) = pipe();
        let (asked, _, took) = held(read, 3);
        assert_eq!(asked, 3);
        assert!(took >= HOLD_LOOK, "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        close(read);
        close(write);
    }

    /// A failure's words are held with the screen still lent, before it is taken back and what was typed is
    /// dropped.
    #[test]
    fn a_held_failure_is_waited_on_before_the_screen_comes_back() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut on = Logged(log.clone());
        let mut with = 0;
        let (a, b) = lend_through_holding(
            &mut on,
            &mut with,
            |_| {
                log.borrow_mut().push("attach");
                1
            },
            |_| Some("words".to_string()),
            |_| false,
            || false,
            || {
                log.borrow_mut().push("left_line");
                2
            },
        )
        .unwrap();
        assert_eq!((a, b), (1, Some(2)));
        assert_eq!(
            *log.borrow(),
            [
                "screen_away",
                "attach",
                "hold",
                "screen_back",
                "left_line",
                "drain"
            ]
        );
    }

    /// An attach that ended well holds nothing.
    #[test]
    fn a_clean_end_holds_nothing() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut on = Logged(log.clone());
        let mut with = 0;
        lend_through_holding(
            &mut on,
            &mut with,
            |_| log.borrow_mut().push("attach"),
            |_| None,
            |_| false,
            || false,
            || (),
        )
        .unwrap();
        assert_eq!(
            *log.borrow(),
            ["screen_away", "attach", "screen_back", "drain"]
        );
    }

    /// An app that was told to end holds nothing and takes nothing back, whatever the attach said.
    #[test]
    fn an_app_told_to_end_holds_nothing_for_a_failure() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut on = Logged(log.clone());
        let mut with = 0;
        let (_, back) = lend_through_holding(
            &mut on,
            &mut with,
            |_| log.borrow_mut().push("attach"),
            |_| Some("words".to_string()),
            |_| false,
            || true,
            || (),
        )
        .unwrap();
        assert_eq!(back, None);
        assert_eq!(*log.borrow(), ["screen_away", "attach"]);
    }

    /// The screen comes back before the wait that follows the lent program (the left line), and what was typed is
    /// dropped last: a wait on a terminal that is still cooked would echo keys and pass Ctrl-C on.
    #[test]
    fn the_screen_is_back_before_the_wait_that_follows_the_lent_program() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut on = Logged(log.clone());
        let (a, b) = lend_through(
            &mut on,
            || {
                log.borrow_mut().push("attach");
                1
            },
            || false,
            || {
                log.borrow_mut().push("left_line");
                2
            },
        )
        .unwrap();
        assert_eq!((a, b), (1, Some(2)));
        assert_eq!(
            *log.borrow(),
            ["screen_away", "attach", "screen_back", "left_line", "drain"]
        );
    }

    /// An app that was told to end while the program it lent the terminal to ran does not take the screen
    /// back over it: nothing but the lending away and the program happens.
    #[test]
    fn an_app_that_was_told_to_end_does_not_take_the_screen_back() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut on = Logged(log.clone());
        let (a, b) = lend_through(
            &mut on,
            || {
                log.borrow_mut().push("attach");
                1
            },
            || true,
            || {
                log.borrow_mut().push("left_line");
                2
            },
        )
        .unwrap();
        assert_eq!((a, b), (1, None));
        assert_eq!(*log.borrow(), ["screen_away", "attach"]);
    }

    /// A screen that is lent when it is dropped (the app was told to end while a client had the terminal) writes
    /// nothing: the leave of the alternate screen was written when it was lent, and a second one restores the
    /// cursor saved by the first, over what the client wrote since. (ratatui's own `Terminal` drop still shows
    /// the cursor, which is harmless and not under this test.)
    #[test]
    fn a_screen_that_is_lent_when_it_is_dropped_writes_nothing() {
        assert_eq!(written_on_drop(true), None, "lent");
        assert_eq!(written_on_drop(false), Some(restore_sequence()), "not lent");
    }

    /// What puts the screen away is the mouse off, the alternate screen left and the cursor shown, in that
    /// order, and nothing else: the watchdog writes exactly these bytes.
    #[test]
    fn the_sequence_that_puts_the_screen_away_is_mouse_off_leave_and_cursor() {
        let bytes = String::from_utf8(restore_sequence()).unwrap();
        let leave = bytes
            .find("\u{1b}[?1049l")
            .expect("leaves the alternate screen");
        let mouse = bytes.find("\u{1b}[?1000l").expect("mouse capture off");
        let cursor = bytes.find("\u{1b}[?25h").expect("cursor shown");
        assert!(mouse < leave && leave < cursor, "{bytes:?}");
        assert!(bytes.ends_with("\u{1b}[?25h"), "{bytes:?}");
        assert!(!bytes.contains("\u{1b}[?1049h"), "{bytes:?}");
    }

    use std::path::PathBuf;

    use super::*;
    use crate::tui::data::Snapshot;

    /// A stand-in `tmux` for [`refresh_bar`]: logs every argv it is run with to
    /// `argv.log`, one line per call, each argument `\x1f`-separated — `cli::ui_open`'s
    /// `stub_tmux` minus the emulated reads, since `set_session_option` is the only call
    /// the gate can make.
    fn stub_tmux(dir: &Path) -> (Tmux, PathBuf) {
        let log = dir.join("argv.log");
        let script = dir.join("tmux");
        let body = format!(
            "#!/bin/sh\n[ \"$1\" = -u ] && shift\n{{ for a in \"$@\"; do printf '%s\\037' \"$a\"; done; printf '\\n'; }} >> '{}'\nexit 0\n",
            log.display()
        );
        std::fs::write(&script, body).unwrap();
        crate::test_exec::make_executable(&script);
        (
            Tmux {
                bin: script,
                socket: None,
            },
            log,
        )
    }

    /// Whether the log holds a `set-option` call — [`stub_tmux`]'s script never fails one,
    /// so this is the only thing worth telling apart.
    fn wrote_the_bar(log: &Path) -> bool {
        log.exists()
            && std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .any(|line| line.split('\u{1f}').next() == Some("set-option"))
    }

    /// A minimal `App` for [`refresh_bar`]'s gate: an empty [`Snapshot`] is enough, since
    /// the gate takes `session` as a plain argument (`run` is what folds `own_pane` and
    /// `current_session` into it) and `bar_counts()` tolerates an empty world.
    fn test_app(cfg: &Config) -> App<'_> {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        App::new(cfg, theme, String::new(), Snapshot::default())
    }

    /// `cfg` with `[theme] auto_switch` as `on` says, dracula for the dark side and solarized for the light.
    fn switching_config(on: bool) -> Config {
        toml::from_str(&format!(
            "[theme]\nname = \"nord\"\nauto_switch = {on}\ndark_name = \"dracula\"\nlight_name = \"solarized\"\n"
        ))
        .unwrap()
    }

    fn told(app: &mut App, theme: Option<&str>) {
        let snap = Snapshot {
            client_theme: theme.map(String::from),
            ..Snapshot::default()
        };
        app.set_data(Some(snap), None);
    }

    /// An open popup follows its client's light or dark mode with the snapshot each tick reads: the palette and the
    /// options overlay's `name` change with what `#{client_theme}` says, and `unknown` goes back to `[theme] name`.
    #[test]
    fn an_open_popup_follows_the_clients_theme_with_each_snapshot() {
        let cfg = switching_config(true);
        let paths = Paths::from_env();
        let bin = Path::new("taw");
        let of = |name: &str| theme::resolve(name, &cfg.theme.custom).theme;
        let mut shown = "nord".to_string();
        let mut app = test_app(&cfg);
        assert_eq!(app.theme, of("nord"));

        told(&mut app, Some("light"));
        follow_theme(&cfg, &paths, bin, &mut app, &mut shown);
        assert_eq!((app.theme, shown.as_str()), (of("solarized"), "solarized"));
        assert!(
            app.settings.contains("name = \"solarized\""),
            "{}",
            app.settings
        );

        told(&mut app, Some("dark"));
        follow_theme(&cfg, &paths, bin, &mut app, &mut shown);
        assert_eq!((app.theme, shown.as_str()), (of("dracula"), "dracula"));
        assert!(
            app.settings.contains("name = \"dracula\""),
            "{}",
            app.settings
        );

        // A tmux with no `#{client_theme}` (nothing read), and a client that cannot say, are `name` again.
        told(&mut app, None);
        follow_theme(&cfg, &paths, bin, &mut app, &mut shown);
        assert_eq!(app.theme, of("nord"));
        told(&mut app, Some("light"));
        follow_theme(&cfg, &paths, bin, &mut app, &mut shown);
        told(&mut app, Some("unknown"));
        follow_theme(&cfg, &paths, bin, &mut app, &mut shown);
        assert_eq!((app.theme, shown.as_str()), (of("nord"), "nord"));
    }

    /// Without `auto_switch` the popup keeps `[theme] name` whatever its client says.
    #[test]
    fn without_auto_switch_the_popup_keeps_its_theme() {
        let cfg = switching_config(false);
        let paths = Paths::from_env();
        let mut shown = "nord".to_string();
        let mut app = test_app(&cfg);
        let before = app.theme;
        for said in ["light", "dark"] {
            told(&mut app, Some(said));
            follow_theme(&cfg, &paths, Path::new("taw"), &mut app, &mut shown);
            assert_eq!((app.theme, shown.as_str()), (before, "nord"), "{said}");
        }
    }

    /// Whether the log holds a `capture-pane` call.
    fn captured(log: &Path) -> bool {
        log.exists()
            && std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .any(|line| line.split('\u{1f}').next() == Some("capture-pane"))
    }

    /// This machine's sample world with studio's one session, `garden`, after it.
    fn two_machines() -> Snapshot {
        use crate::machine::MachineView;
        use crate::machine::wire::{State, WirePane, WireSpace};
        let state = State {
            spaces: vec![WireSpace {
                session_id: "$3".into(),
                session_name: "garden".into(),
                session_path: "/home/pi/garden".into(),
                windows: 1,
                active_window_id: "@5".into(),
                active_pane_id: Some("%7".into()),
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
                pane_current_command: "zsh".into(),
                pane_active: true,
                alternate_on: false,
            }],
            ..State::default()
        };
        let view = MachineView {
            name: "studio".into(),
            live: true,
            state: Some(state),
            last_seen: Some(5_000),
            ..MachineView::default()
        };
        crate::tui::machines::merge(crate::tui::fixtures::snapshot(), &[view], 5_000)
    }

    fn row_where(app: &App, wanted: impl Fn(&crate::tui::tree::Row) -> bool) -> usize {
        app.rows
            .rows
            .iter()
            .position(wanted)
            .expect("the row is in the list")
    }

    #[test]
    fn a_live_remote_row_under_the_cursor_asks_local_tmux_for_no_capture_and_a_local_row_does() {
        use crate::config::Depth;
        use crate::tui::tree::Row;
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path());
        let mut cfg = Config::default();
        cfg.ui.depth = Depth::All;
        cfg.ui.preview = true;
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), two_machines());
        // The positive half: a row of this machine is captured.
        app.sel = row_where(&app, |row| matches!(row, Row::Session(0)));
        follow_preview(&tmux, &mut app);
        assert!(captured(&log), "a local row is captured");
        std::fs::remove_file(&log).unwrap();
        // A remote session and a remote pane: nothing is asked of local tmux.
        for remote in [
            row_where(&app, |row| matches!(row, Row::Session(5))),
            row_where(&app, |row| matches!(row, Row::Pane(p) if *p >= 18)),
        ] {
            app.sel = remote;
            follow_preview(&tmux, &mut app);
            assert_eq!(app.preview_pane(), None);
            assert!(!captured(&log), "a remote row reached local tmux");
        }
    }

    #[test]
    fn an_id_with_a_colon_is_never_captured_by_local_tmux() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path());
        // A local session could be called `studio`: `studio:%7` is not a target of this machine.
        assert!(capture_preview(&tmux, Some("studio:%7")).is_none());
        assert!(!captured(&log));
        assert!(capture_preview(&tmux, Some("%7")).is_some());
        assert!(captured(&log), "a local id is");
    }

    #[test]
    fn refresh_bar_with_bar_false_never_writes_the_session_option() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path());
        let cfg = Config::default();
        let app = test_app(&cfg);
        let mut last = None;
        refresh_bar(&tmux, &app, false, Some("$3"), &mut last);
        assert!(
            !wrote_the_bar(&log),
            "bar=false must never call set-option, even with a session"
        );
    }

    #[test]
    fn refresh_bar_with_bar_true_and_a_session_writes_the_session_option() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path());
        let cfg = Config::default();
        let app = test_app(&cfg);
        let mut last = None;
        refresh_bar(&tmux, &app, true, Some("$3"), &mut last);
        assert!(
            wrote_the_bar(&log),
            "bar=true with a session must write the bar"
        );
    }

    #[test]
    fn refresh_bar_with_bar_true_and_no_session_never_writes_the_session_option() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path());
        let cfg = Config::default();
        let app = test_app(&cfg);
        let mut last = None;
        refresh_bar(&tmux, &app, true, None, &mut last);
        assert!(
            !wrote_the_bar(&log),
            "no session to write to, regardless of bar"
        );
    }

    // ---- The machines' rows in the popup: read-only, from the keeper's files ----

    use crate::machine::files::{self, MachineFile};
    use crate::machine::wire::State;

    fn studio_file(live: bool, last_seen: u64) -> MachineFile {
        use crate::machine::wire::{WirePane, WireSpace};
        MachineFile {
            name: "studio".into(),
            state: Some(State {
                spaces: vec![WireSpace {
                    session_id: "$3".into(),
                    session_name: "garden".into(),
                    session_path: "/home/pi/garden".into(),
                    windows: 1,
                    active_window_id: "@5".into(),
                    active_pane_id: Some("%7".into()),
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
                    pane_current_command: "zsh".into(),
                    pane_active: true,
                    alternate_on: false,
                }],
                ..State::default()
            }),
            received_at: Some(last_seen),
            last_seen: Some(last_seen),
            live,
            words: "connected: taw 0.1.0".into(),
            ..MachineFile::default()
        }
    }

    /// The `[machines]` table as `run` holds it for the popup's life: read once.
    fn table(paths: &Paths) -> Vec<crate::machine::Machine> {
        load_machines(paths).unwrap_or_default()
    }

    fn scratch_paths(dir: &Path) -> Paths {
        Paths::rooted_at(dir.join("config"), dir.join("state"))
    }

    fn config_with(paths: &Paths, text: &str) {
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, text).unwrap();
    }

    /// The popup's frame over `snapshot` and `records`, 150 by 39.
    fn popup_frame(
        snapshot: Snapshot,
        records: Vec<crate::session::worktree::WorktreeRecord>,
    ) -> ratatui::buffer::Buffer {
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), snapshot);
        app.set_worktrees(records);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(150, 39)).unwrap();
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text_of(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The new-session picker's title in the app follows the machines of the snapshot: `on this Mac` only with
    /// another machine listed, and plain with none or with this machine's own tmux server alone (the wiring of
    /// `AppLook.machines` in `view::draw`, which the title's own test gives by hand).
    #[test]
    fn the_app_titles_the_new_session_picker_by_whether_another_machine_is_listed() {
        let header = |name: &str, this_server: bool| data::MachineHeader {
            name: name.into(),
            live: true,
            this_server,
            ..data::MachineHeader::default()
        };
        let picker = |machines: Vec<data::MachineHeader>| {
            let mut snap = crate::tui::fixtures::snapshot();
            snap.machines = machines;
            let cfg = Config::default();
            let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
            let mut app = App::new(&cfg, theme, String::new(), snap);
            app.enter_app_mode(None, None);
            app.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('n'),
                crossterm::event::KeyModifiers::NONE,
            ));
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(150, 39)).unwrap();
            terminal.draw(|f| view::draw(f, &mut app)).unwrap();
            text_of(terminal.backend().buffer())
        };
        let with_other = picker(vec![header("laptop", true), header("studio", false)]);
        assert!(
            with_other.contains("new session on this Mac"),
            "positive half: {with_other}"
        );
        for alone in [Vec::new(), vec![header("laptop", true)]] {
            let shown = picker(alone);
            assert!(shown.contains("new session"), "the picker is open: {shown}");
            assert!(!shown.contains("this Mac"), "{shown}");
        }
    }

    /// A table whose only entry is this machine's own tmux server is one machine: the popup draws exactly what it
    /// draws with no `[machines]`, with no header and nothing about machines. Another machine beside it brings
    /// every header back (this machine's, the own server's and the other's).
    #[test]
    fn the_popup_draws_no_header_over_one_machine_and_all_of_them_over_two() {
        let header = |name: &str, this_server: bool| data::MachineHeader {
            name: name.into(),
            live: true,
            words: "connected: taw 0.1.0".into(),
            this_server,
            ..data::MachineHeader::default()
        };
        let plain = text_of(&popup_frame(crate::tui::fixtures::snapshot(), Vec::new()));
        assert!(plain.contains("mobile"), "the positive half: {plain}");
        assert!(
            !plain.contains(crate::machine::this_machine_word()),
            "{plain}"
        );

        let mut own = crate::tui::fixtures::snapshot();
        own.machines = vec![header("laptop", true)];
        let shown = text_of(&popup_frame(own, Vec::new()));
        assert_eq!(shown, plain, "this server alone draws as no machines");

        let mut two = crate::tui::fixtures::snapshot();
        two.machines = vec![header("laptop", true), header("studio", false)];
        let shown = text_of(&popup_frame(two, Vec::new()));
        assert!(
            shown.contains(crate::machine::this_machine_word()),
            "{shown}"
        );
        assert!(
            shown.contains("laptop") && shown.contains("studio"),
            "{shown}"
        );
    }

    /// With a keeper holding the lock, a whole `[machines]` table and a live file, the popup shows studio's
    /// rows under its header, after this machine's.
    #[test]
    fn a_kept_machines_file_adds_its_header_and_rows_to_the_popup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let _keeper = files::take_keeper(&paths).expect("the test takes the lock");
        files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
        let local = crate::tui::fixtures::snapshot();
        let rows_before = local.spaces.len();
        let (snap, _) = with_machines(&paths, &table(&paths), local, Vec::new());
        assert_eq!(snap.machines.len(), 1);
        assert_eq!(snap.machines[0].name, "studio");
        assert!(snap.machines[0].live);
        assert_eq!(snap.spaces.len(), rows_before + 1);
        let last = snap.spaces.last().unwrap();
        assert_eq!(last.session_id, "studio:$3");
        assert_eq!(last.machine, Some(0));
        let shown = text_of(&popup_frame(snap, Vec::new()));
        assert!(shown.contains("studio"), "{shown}");
        assert!(shown.contains("garden"), "{shown}");
    }

    /// A file whose last line from the machine is older than 15 s is not live, though it says so and a keeper keeps it.
    #[test]
    fn a_file_not_heard_from_for_15_seconds_is_greyed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let _keeper = files::take_keeper(&paths).unwrap();
        files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
        let (fresh, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        assert!(
            fresh.machines[0].live,
            "the positive half: a fresh file is live"
        );
        files::write_machine_file(&paths, &studio_file(true, files::now_ms() - 16_000));
        let (old, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        assert_eq!(old.machines.len(), 1);
        assert!(!old.machines[0].live, "16 s without a line is not live");
    }

    /// Without a table, or with a table that does not read whole, the popup is today's: no header, no row, and
    /// the frame is the one it drew before machines existed.
    #[test]
    fn without_a_table_or_a_whole_table_the_popup_is_todays() {
        let cases: [(&str, Option<&str>, bool); 3] = [
            ("keeper, no table", None, true),
            ("keeper, empty table", Some("[machines]\n"), true),
            ("keeper, bad table", Some("[machines]\nstudio = 3\n"), true),
        ];
        for (what, config, keeper) in cases {
            let dir = tempfile::tempdir().unwrap();
            let paths = scratch_paths(dir.path());
            if let Some(text) = config {
                config_with(&paths, text);
            }
            let held = keeper.then(|| files::take_keeper(&paths).unwrap());
            files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
            let (snap, records) = with_machines(
                &paths,
                &table(&paths),
                crate::tui::fixtures::snapshot(),
                crate::tui::fixtures::worktrees(),
            );
            assert!(snap.machines.is_empty(), "{what}");
            let drawn = text_of(&popup_frame(snap, records));
            let want = text_of(&popup_frame(
                crate::tui::fixtures::snapshot(),
                crate::tui::fixtures::worktrees(),
            ));
            assert_eq!(drawn, want, "{what}");
            drop(held);
        }
    }

    /// With a table and no app keeping the connections, each machine of the table has its header after this
    /// machine's sessions, in the table's order, with `NO_KEEPER` and no rows; a live file left by an app that is
    /// gone is not read, and the bar says nothing of machines.
    #[test]
    fn with_no_keeper_each_machine_of_the_table_shows_its_header_saying_how_to_connect() {
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        config_with(&paths, "[machines]\nstudio = \"studio\"\npi = \"pi\"\n");
        files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
        assert!(!files::keeper_alive(&paths), "no app keeps the files");
        let local = crate::tui::fixtures::snapshot();
        let (spaces, panes, agents) = (local.spaces.len(), local.panes.len(), local.agents.len());
        let (snap, _) = with_machines(&paths, &table(&paths), local, Vec::new());
        let names: Vec<&str> = snap.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["studio", "pi"]);
        for header in &snap.machines {
            assert!(!header.live && header.no_keeper && !header.this_server);
            assert_eq!(header.words, crate::machine::words::NO_KEEPER);
            assert_eq!(header.short, crate::machine::words::NO_KEEPER);
            assert_eq!(header.last_seen, None);
        }
        assert_eq!(
            (snap.spaces.len(), snap.panes.len(), snap.agents.len()),
            (spaces, panes, agents),
            "no remote row: the stale file of studio was not read"
        );
        assert!(snap.spaces.iter().all(|s| s.machine.is_none()));
        let shown = text_of(&popup_frame(snap, Vec::new()));
        for name in ["studio", "pi"] {
            let line = shown
                .lines()
                .find(|l| l.contains(name) && l.contains(crate::machine::words::NO_KEEPER))
                .unwrap_or_else(|| panic!("no header of {name} with the words:\n{shown}"));
            // Whatever the fixture's rows: the sentence whole, with its rule after it (the list is made wide
            // enough for these words, which never change).
            assert!(
                line.contains(&format!("{} \u{2501}", crate::machine::words::NO_KEEPER)),
                "{line:?}"
            );
        }
    }

    /// With no app keeping the files, an entry whose file (left by an earlier app run, which found it was this
    /// machine's own tmux server) says `this_server` is this machine: no header and no `not connected`, the frame
    /// is the one without machines; another entry of the table still says how to connect.
    #[test]
    fn with_no_keeper_an_entry_an_earlier_run_found_to_be_this_server_draws_no_header() {
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let mut file = studio_file(false, files::now_ms());
        file.this_server = true;
        files::write_machine_file(&paths, &file);
        assert!(!files::keeper_alive(&paths), "no app keeps the files");
        let (snap, records) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            crate::tui::fixtures::worktrees(),
        );
        assert_eq!(snap.machines.len(), 1);
        assert!(snap.machines[0].this_server, "the file says so");
        assert!(!snap.has_other_machines());
        assert!(snap.spaces.iter().all(|s| s.machine.is_none()));
        let drawn = text_of(&popup_frame(snap, records));
        let want = text_of(&popup_frame(
            crate::tui::fixtures::snapshot(),
            crate::tui::fixtures::worktrees(),
        ));
        assert_eq!(drawn, want);
        assert!(!drawn.contains(crate::machine::words::NO_KEEPER), "{drawn}");

        // With a second entry that is no file's this server: its header, and only its header.
        config_with(&paths, "[machines]\nstudio = \"studio\"\npi = \"pi\"\n");
        let (snap, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        assert!(snap.machines[0].this_server && !snap.machines[1].this_server);
        let shown = text_of(&popup_frame(snap, Vec::new()));
        assert!(
            shown
                .lines()
                .any(|l| l.contains("pi") && l.contains(crate::machine::words::NO_KEEPER)),
            "{shown}"
        );
        assert!(
            shown
                .lines()
                .any(|l| l.contains("studio is this tmux server")),
            "{shown}"
        );
        assert!(
            !shown
                .lines()
                .any(|l| l.contains("studio") && l.contains(crate::machine::words::NO_KEEPER)),
            "{shown}"
        );
    }

    /// The cursor never rests on such a header: the first row it rests on is this machine's, and moving down
    /// past the last of this machine's rows goes on to the first again, not to a header.
    #[test]
    fn the_cursor_never_rests_on_a_header_that_says_not_connected() {
        use crate::tui::tree::Row;
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        config_with(&paths, "[machines]\nstudio = \"studio\"\npi = \"pi\"\n");
        let (snap, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), snap);
        let headers = app
            .rows
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Machine(Some(_))))
            .count();
        assert_eq!(headers, 2, "the positive half: both headers are listed");
        assert!(
            !matches!(app.current(), Some(Row::Machine(_))),
            "the first row the cursor rests on"
        );
        for _ in 0..app.rows.rows.len() * 2 {
            app.move_by(1);
            assert!(!matches!(app.current(), Some(Row::Machine(_))));
        }
        for _ in 0..app.rows.rows.len() * 2 {
            app.move_by(-1);
            assert!(!matches!(app.current(), Some(Row::Machine(_))));
        }
    }

    /// A popup opened with no `[machines]` reads no machine file and makes no `<state>/machines` folder.
    #[test]
    fn a_popup_with_no_machines_table_leaves_no_machines_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = scratch_paths(dir.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let (snap, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        assert!(snap.machines.is_empty());
        assert!(
            !paths.state_dir.join("machines").exists(),
            "the popup made a machines folder"
        );
        // And with a table whose machines nobody keeps: the hint header, and still no folder.
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let (snap, _) = with_machines(
            &paths,
            &table(&paths),
            crate::tui::fixtures::snapshot(),
            Vec::new(),
        );
        assert_eq!(snap.machines.len(), 1);
        assert!(snap.machines[0].no_keeper);
        assert!(!paths.state_dir.join("machines").exists());
    }

    /// The popup's own refresh puts the machines' rows in the app it fills, tick after tick, and takes them out
    /// when the keeper goes (the lock is let go): the machine is then a header saying `NO_KEEPER`, and its rows
    /// are gone though its file is still there.
    #[test]
    fn the_popups_refresh_shows_a_kept_machine_and_drops_it_when_nobody_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path());
        let paths = scratch_paths(&dir.path().join("p"));
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let keeper = files::take_keeper(&paths).unwrap();
        files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
        let cfg = Config::default();
        let mut app = test_app(&cfg);
        let git = data::GitWorker::new(GIT_TTL);
        let machines = table(&paths);
        refresh(&tmux, &cfg, &paths, &machines, &git, &mut app, None, None);
        assert_eq!(app.snap.machines.len(), 1, "a kept machine is in the popup");
        let kept = text_of(&popup_frame(app.snap.clone(), app.worktrees.clone()));
        assert!(kept.contains("garden"), "the positive half: {kept}");
        drop(keeper);
        refresh(&tmux, &cfg, &paths, &machines, &git, &mut app, None, None);
        assert_eq!(app.snap.machines.len(), 1, "the header stays");
        assert!(app.snap.machines[0].no_keeper && !app.snap.machines[0].live);
        assert!(app.snap.spaces.iter().all(|s| s.machine.is_none()));
        let gone = text_of(&popup_frame(app.snap.clone(), app.worktrees.clone()));
        assert!(!gone.contains("garden"), "nobody keeps it: {gone}");
        // The sentence waits for the person, so the list is made wide enough for it: whole, with its rule.
        let said = crate::machine::words::NO_KEEPER;
        assert!(gone.contains(&format!("{said} \u{2501}")), "{gone}");
    }

    /// The table the popup started with is the one it keeps: the rows of its machines stay when the config file
    /// is edited afterwards (the positive half is the first refresh, which shows them).
    #[test]
    fn the_popup_keeps_the_machines_table_it_started_with() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path());
        let paths = scratch_paths(&dir.path().join("p"));
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let _keeper = files::take_keeper(&paths).unwrap();
        files::write_machine_file(&paths, &studio_file(true, files::now_ms()));
        let cfg = Config::default();
        let mut app = test_app(&cfg);
        let git = data::GitWorker::new(GIT_TTL);
        let machines = table(&paths);
        refresh(&tmux, &cfg, &paths, &machines, &git, &mut app, None, None);
        assert_eq!(app.snap.machines.len(), 1, "studio is in the popup");

        config_with(&paths, "[machines]\n");
        refresh(&tmux, &cfg, &paths, &machines, &git, &mut app, None, None);
        assert_eq!(
            app.snap.machines.len(),
            1,
            "an edit of the config does not take studio away mid-popup"
        );
    }

    /// A tick whose tmux read fails keeps the machines' closed worktree rows: the worktree records of the tick are
    /// merged with them though the snapshot is the last good one.
    #[test]
    fn a_tick_whose_tmux_read_fails_keeps_a_machines_closed_worktree_row() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path());
        let missing = Tmux {
            bin: dir.path().join("no-such-tmux"),
            socket: None,
        };
        let paths = scratch_paths(&dir.path().join("p"));
        config_with(&paths, "[machines]\nstudio = \"studio\"\n");
        let _keeper = files::take_keeper(&paths).unwrap();
        let mut file = studio_file(true, files::now_ms());
        file.state
            .as_mut()
            .unwrap()
            .worktrees
            .push(crate::machine::wire::WireWorktree {
                workspace_id: "w7".into(),
                session_name: "garden-fix".into(),
                path: "/home/pi/garden-fix".into(),
                branch: "fix".into(),
                ..Default::default()
            });
        files::write_machine_file(&paths, &file);
        let cfg = Config::default();
        let mut app = test_app(&cfg);
        let git = data::GitWorker::new(GIT_TTL);
        let machines = table(&paths);
        let has_row = |app: &App| app.worktrees.iter().any(|w| w.workspace_id == "studio:w7");
        refresh(&tmux, &cfg, &paths, &machines, &git, &mut app, None, None);
        assert!(has_row(&app), "the positive half: a good tick has the row");

        refresh(
            &missing, &cfg, &paths, &machines, &git, &mut app, None, None,
        );
        assert!(has_row(&app), "a failed read took the closed worktree away");
    }
    /// The paste-image key does not run on the popup's thread: `perform` returns at once with the paste still under
    /// way, a second key says one is under way, and the toast of its end comes when it ends.
    #[test]
    fn the_popups_paste_image_key_returns_at_once_and_its_toast_comes_when_the_paste_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _log) = stub_tmux(dir.path());
        let paths = scratch_paths(dir.path());
        let cfg = Config::default();
        let mut app = test_app(&cfg);
        let git = data::GitWorker::new(GIT_TTL);
        let (open, gate) = std::sync::mpsc::channel();
        let mut pasting = pasting::Pasting::with(pasting::gated(gate, "image pasted into %16"));
        let action = actions::Action::PasteImage {
            pane: "%16".to_string(),
        };

        let began = Instant::now();
        let done = perform(
            &tmux,
            &cfg,
            &paths,
            &[],
            Path::new("/nonexistent/taw"),
            &git,
            &mut app,
            &mut pasting,
            &action,
            None,
            None,
        )
        .unwrap();
        assert!(!done, "the popup stays open");
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "perform waited for the paste: {:?}",
            began.elapsed()
        );
        assert_eq!(
            app.live_toast(),
            Some(pasting::started_words("%16").as_str())
        );
        assert_eq!(pasting.poll(), None, "the paste has not ended");

        perform(
            &tmux,
            &cfg,
            &paths,
            &[],
            Path::new("/nonexistent/taw"),
            &git,
            &mut app,
            &mut pasting,
            &action,
            None,
            None,
        )
        .unwrap();
        assert_eq!(app.live_toast(), Some(pasting::UNDER_WAY));

        open.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let words = loop {
            if let Some(words) = pasting.poll() {
                break words;
            }
            assert!(Instant::now() < deadline, "the paste never ended");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(words, "image pasted into %16");
    }
}
