//! The app child's screen: [`run_app`], beside [`super::run`].
//!
//! The app is the whole terminal and has no tmux client of its own, so nothing here names a client:
//! the list is this Mac's sessions and the machines', merged. A landing (Enter on a row) leaves the
//! screen, runs `tmux attach` as a child with the terminal and waits for it ([`Feed::go_in`]): the
//! app is what the terminal comes back to. Everything the popup's loop does for a client (the bar,
//! the prefix chord, `--from`) is left out, and the top line ([`super::view`]) is the app's own.

mod machine;
mod remote_actions;
mod remote_preview;
mod toasts;

pub use machine::GoTarget;

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};

use super::app::{App, AppEnd, walk};
use super::{
    GIT_TTL, LENT, Lending, Screen, TICK, actions, capture_preview, data, follow_preview,
    lend_through, machines, overlays, theme, view, zoxide_dirs,
};
use crate::config::Config;
use crate::machine::config::load_machines;
use crate::machine::conn::{self, ConnCtx, Timing};
use crate::machine::files::{self, AppRecord};
use crate::machine::machines::Machines;
use crate::machine::ssh::Ssh;
use crate::machine::wire::{Down, Left};
use crate::machine::{Machine, MachineView, feed};
use crate::paths::Paths;
use crate::session::worktree::WorktreeRecord;
use crate::tmux::{SessionInfo, Tmux};
use data::Snapshot;

/// Set by the handler of `SIGTERM` and `SIGHUP` to the signal that came: the supervisor passed one on, or the
/// terminal went. 0 while none did.
static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_signal(signal: libc::c_int) {
    SIGNALLED.store(signal, Ordering::SeqCst);
}

/// Whether the app was asked to end by a signal (`SIGTERM` or `SIGHUP`). The screen's loop looks at it
/// after each wait and leaves, so the terminal is put away; whatever else waits on this process reads it
/// too.
pub(crate) fn signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst) != 0
}

/// The signal the app was asked to end by, or 0 while it was not.
fn the_signal() -> libc::c_int {
    SIGNALLED.load(Ordering::SeqCst)
}

/// From here a `SIGTERM` or `SIGHUP` only sets [`signalled`], so it never kills the app with the
/// terminal still raw, on the alternate screen and with the mouse captured.
fn catch_signals() {
    // Ignored by a parent, SIGCHLD is ignored here too, and the wait for an attach client would fail with ECHILD.
    crate::cli::app::reset_sigchld();
    // SAFETY: `signal` with a handler that only stores to an atomic.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGHUP] {
            libc::signal(
                signal,
                note_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

/// How often the watchdog looks, and how long it gives the screen's loop to end by itself.
const WATCH: Duration = Duration::from_millis(200);
const GRACE: Duration = Duration::from_secs(2);

/// How long the app waits for a client it has told to end.
const CLIENT_END_WAIT: Duration = Duration::from_millis(600);

/// How long the app waits for an ssh it has killed (`SIGKILL`): the kill cannot be refused, so this is for a
/// process that is slow to be reaped, and the wait must not hold the app up.
const KILLED_REAP_WAIT: Duration = Duration::from_millis(200);

/// The whole of the main thread's own ending of an ssh that ignores `SIGHUP`, from a signal to the restore bytes (a
/// look of [`WAIT_LOOK`], the wait, the `SIGKILL` and its reap), with a margin for the poll of the machines and for
/// sleeps that run long under load, fits in [`GRACE`]: the watchdog, which starts counting at the same signal, never
/// ends the process before the app has killed the ssh and written the restore bytes.
const _: () = assert!(
    WAIT_LOOK.as_millis() + CLIENT_END_WAIT.as_millis() + KILLED_REAP_WAIT.as_millis() + 500
        <= GRACE.as_millis()
);

/// What the watchdog makes of how things are.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Watch {
    Fine,
    /// The terminal is gone: nothing is left to put away.
    EndNow,
    /// The supervisor is gone or a signal was passed on: the loop is given [`GRACE`] to put a live terminal
    /// away, then the process ends.
    EndSoon,
}

/// Whether a process that started under `started_under` and is now under `now_under` has lost its parent: a
/// different one took it over, or it is `init`'s (1). The second is for a supervisor that died before this
/// process first looked, which leaves `started_under` at 1 too. A supervisor that is itself pid 1 (`taw app` as a
/// container's entrypoint) has every child judged orphaned at its first look, so the app ends at once: that is not
/// a supported way to run it, which needs a terminal and a tmux of its own.
fn gone_from(started_under: libc::pid_t, now_under: libc::pid_t) -> bool {
    now_under != started_under || now_under == 1
}

/// The watchdog's decision: `started_under` and `now_under` are the parent's pid at the start and now.
fn judge(
    started_under: libc::pid_t,
    now_under: libc::pid_t,
    terminal: bool,
    signalled: bool,
) -> Watch {
    if !terminal {
        Watch::EndNow
    } else if gone_from(started_under, now_under) || signalled {
        Watch::EndSoon
    } else {
        Watch::Fine
    }
}

/// Whether the watchdog writes the screen's restore bytes at its end: only when the terminal is still there and
/// the screen is not lent. While a program (a tmux client now) has the terminal the screen was put away already
/// and a second leave of the alternate screen would restore the cursor the first one saved, over what that
/// program wrote since; the main thread, which polls every [`WAIT_LOOK`] while it waits for the program, ends it.
fn puts_away(lent: bool, terminal: bool) -> bool {
    !lent && terminal
}

/// Whether the terminal on `fd` is still there. Two ways it is not, each by its own probe: a revoked one
/// (tmux hung the pane up) is no terminal to `isatty`, though reads of it come back at once with nothing;
/// and one whose master closed with no revoke (the emulator's end went) is still a terminal to `isatty` on
/// macOS (Linux hangs the slave up, so `isatty` says no there) but polls as hung up, which a live one never
/// does, input waiting or not (tried on macOS: a closed master gives `POLLHUP`, an open one never). What no
/// probe tells is a terminal that is there and is not read by anyone (a stopped emulator): that one is not
/// gone, and stays the signals' business.
fn terminal_is_there(fd: libc::c_int) -> bool {
    // SAFETY: `isatty` takes a descriptor number and touches no memory.
    if unsafe { libc::isatty(fd) } != 1 {
        return false;
    }
    let mut look = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `poll` of one `pollfd` that is alive, with no wait; an error leaves `revents` at 0.
    unsafe { libc::poll(&mut look, 1, 0) };
    look.revents & libc::POLLHUP == 0
}

/// Puts the terminal on standard output away without the loop's help: raw mode off and one `write(2)` of
/// `away` ([`super::restore_sequence`]). Nothing here waits for a lock of the screen's.
fn put_the_terminal_away(away: &[u8]) {
    // SAFETY: `write` of a buffer that is alive, to descriptor 1; a short or failed write is all there is.
    unsafe {
        libc::write(1, away.as_ptr().cast(), away.len());
    }
    let _ = crossterm::terminal::disable_raw_mode();
}

/// A thread that ends this process when the screen's loop cannot be relied on to: crossterm's wait for a
/// key does not come back to the loop on a revoked terminal (it reads nothing for ever), so a flag the loop
/// looks at is never looked at. Looks every [`WATCH`]; see [`judge`].
fn start_watchdog(paths: &Paths) {
    // SAFETY: `getppid` takes nothing and cannot fail.
    let started_under = unsafe { libc::getppid() };
    let paths = paths.clone();
    let pid = std::process::id();
    // Made here, so that the thread allocates nothing and takes no lock of the main loop's when it ends.
    let away = super::restore_sequence();
    let _ = std::thread::Builder::new()
        .name("app-watchdog".into())
        .spawn(move || {
            let mut asked = None;
            loop {
                std::thread::sleep(WATCH);
                // SAFETY: as above.
                let now_under = unsafe { libc::getppid() };
                let end = match judge(started_under, now_under, terminal_is_there(0), signalled()) {
                    Watch::Fine => false,
                    Watch::EndNow => true,
                    Watch::EndSoon => asked.get_or_insert_with(Instant::now).elapsed() >= GRACE,
                };
                if end {
                    // The record goes first, so that file IO does not run between the terminal being put away
                    // and the end: a main loop that came out of a slow tick then would draw onto the main screen.
                    if gone_from(started_under, now_under) {
                        // Nothing else will take the record of an app whose supervisor is gone.
                        files::remove_app_record(&paths, pid);
                    }
                    // The loop was too slow to look (a tick forks several tmux calls) or is stuck. A live
                    // terminal is put away here, as the loop would, so the shell is not left a raw, alternate
                    // screen with the mouse captured. While a client has the terminal the main thread is in
                    // `Feed::wait_for`, polls every 150 ms and ends the client itself, an ssh that ignores the
                    // hangup included (killed, then the restore bytes): that sequence fits in `GRACE` (see the
                    // assert at its constants), so a main thread that is merely in it is covered. A watchdog that
                    // fires meanwhile only ends the process: it never writes over a lent screen and never reaps.
                    // Only a main thread that is stuck outside `wait_for`'s look (in `hub.poll`, in the record's
                    // file write or in `spawn`) for the whole of `GRACE` is left: its client is left running, and
                    // ends when its terminal is hung up or it is detached.
                    if puts_away(LENT.load(Ordering::SeqCst), terminal_is_there(1)) {
                        put_the_terminal_away(&away);
                    }
                    // `_exit` skips `Machines`' drop: the connection children are not ended by their pids, they
                    // end on EOF of their piped stdin when this process is gone, and the keeper's lease is not
                    // given up, so the followers wait for it to run out (15 s) before one takes over.
                    // SAFETY: `_exit` ends the process at once without running anything of ours.
                    unsafe { libc::_exit(1) };
                }
            }
        });
}

/// Whether the app must end now: a signal was passed on, or the supervisor that started it is gone (killed by
/// its pid), so the terminal is back at the shell, and this app would draw on it and read its keys beside it.
/// The end puts the screen away (a lent screen is away already, and stays so) and takes the record; the
/// watchdog's look at the same thing is for a loop that is stuck.
fn must_end(feed: &Feed) -> bool {
    #[cfg(test)]
    if end_by_seam() {
        return true;
    }
    signalled() || feed.record.orphaned()
}

#[cfg(test)]
thread_local! {
    /// The checks of [`must_end`] this test's thread still lets pass before it answers true (and goes on
    /// answering true), set by [`end_after_checks`]: the signal flag and the supervisor are the whole process's,
    /// so a test cannot make an app end in the middle of a wait any other way.
    static END_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// From now on this test's thread answers [`must_end`] true at its `n`th check (1 is the next one) and after it.
/// `None` puts the product's own answer back. Not in the product build.
#[cfg(test)]
pub(crate) fn end_after_checks(n: Option<usize>) {
    END_AFTER.with(|left| left.set(n));
}

/// One check of the seam: counts down, and is true once the count is spent.
#[cfg(test)]
fn end_by_seam() -> bool {
    END_AFTER.with(|left| match left.get() {
        Some(n) => {
            let n = n.saturating_sub(1);
            left.set(Some(n));
            n == 0
        }
        None => false,
    })
}

/// Runs the app until the user leaves, and says how: [`AppEnd::Back`] or [`AppEnd::Leave`].
///
/// `came_from` is the pane of the row the screen opens on (the one you came out of), when there is
/// one; else the cursor opens on the active pane of the session attached most lately.
///
/// `go` is where `taw app --go` was asked to go: the machine is gone into before the first frame
/// ([`Feed::go_first`]), and the list is what comes back, or what stays with the words when it cannot be.
///
/// The machines come from the config the way `taw machine list` reads them. A `[machines]` table that
/// does not load starts the app with none, and its words are the first thing the screen says.
pub fn run_app(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    came_from: Option<String>,
    go: Option<GoTarget>,
    back_to_tmux: bool,
) -> anyhow::Result<AppEnd> {
    catch_signals();
    start_watchdog(paths);
    let resolved = theme::resolve(&cfg.theme.name, &cfg.theme.custom);
    for warning in &resolved.warnings {
        log::warn!("theme: {warning}");
    }
    let settings = overlays::settings_text(cfg, paths, &resolved, taw_bin);
    let (machines, doubt) = match load_machines(paths) {
        Ok(machines) => (machines, None),
        Err(doubt) => (Vec::new(), Some(doubt.to_string())),
    };
    // `last` is the session this Mac was in most lately, made for the current directory when there is none: before
    // the machines start, as no machine has to be waited for. A session that cannot be made is the first words.
    let (go, doubt) = match go {
        Some(GoTarget::Last) => match last_target(tmux, cfg, paths, taw_bin) {
            Ok(target) => (Some(target), doubt),
            Err(words) => (None, first_words(Some(words), doubt)),
        },
        go => (go, doubt),
    };
    let server = feed::server_word(tmux);
    let hub = Machines::start(conn_ctx(paths, &server), machines.clone());
    let git = data::GitWorker::new(GIT_TTL);
    let mut feed = Feed {
        tmux,
        cfg,
        paths,
        git,
        hub,
        machines,
        server,
        ssh: Ssh::from_env(),
        local: Local::default(),
        lefts: Vec::new(),
        walk_said: None,
        toasts: toasts::Toaster::new(paths, taw_bin),
        remote_preview: remote_preview::RemotePreview::default(),
        remote: remote_actions::Rows::default(),
        pasting: super::pasting::Pasting::default(),
        // The record names this process while it runs; an end that was asked for takes it away.
        record: Record::write(paths),
        own_pane: crate::cli::common::current_pane().ok(),
    };
    // Said once the first frame is drawn, so that its few seconds are not spent starting up.
    let mut doubt = doubt;
    let (mut app, mut screen) = open_going(
        &mut feed,
        cfg,
        resolved.theme,
        settings,
        came_from,
        go.as_ref().map(|target| (target, machine::GO_WAIT)),
        Screen::enter,
    )?;
    // Started by the doorway: the keys that end the app say they go back to tmux.
    app.back_to_tmux = back_to_tmux;
    // The theme is the one the client's light or dark mode asks for from the first frame (`auto_switch`); `shown` is
    // the name on screen, for the ticks to compare with.
    let mut shown = cfg.theme.name.clone();
    super::follow_theme(cfg, paths, taw_bin, &mut app, &mut shown);
    let mut last_tick = Instant::now();
    let end = loop {
        // Before the frame: an app that is to end has no business drawing, and after a lent end (see
        // [`Feed::go_in`]) the screen is the shell's.
        if must_end(&feed) {
            break AppEnd::Leave;
        }
        screen
            .terminal
            .draw(|frame| view::draw(frame, &mut app))
            .map_err(|e| anyhow::anyhow!("drawing the app: {e}"))?;
        if let Some(words) = doubt.take() {
            app.toast(words);
            continue;
        }

        if event::poll(TICK.saturating_sub(last_tick.elapsed()))? {
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                _ => None,
            };
            if let Some(action) = action {
                feed.perform(taw_bin, &mut app, &mut screen, &action)?;
            }
            feed.follow_preview(tmux, &mut app);
        }
        // The end of a paste that ran meanwhile: its toast comes when it ends.
        feed.say_pasted(&mut app);
        // A signal that came during the poll ends the loop at once.
        if must_end(&feed) {
            break AppEnd::Leave;
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            feed.tick(tmux, &mut app);
            super::follow_theme(cfg, paths, taw_bin, &mut app, &mut shown);
            feed.settle_remote(&mut app, &mut screen)?;
        }
        if app.quit {
            match app.end.unwrap_or(AppEnd::Back) {
                AppEnd::Leave => break AppEnd::Leave,
                AppEnd::Back => {
                    let Some(pane) = app.came_from().map(str::to_string) else {
                        break AppEnd::Back;
                    };
                    // Back is going in again, where you were.
                    app.quit = false;
                    app.end = None;
                    feed.go_back_in(&mut app, &mut screen, &pane)?;
                }
            }
        }
    };
    // Only an end that was asked for takes the record away. After a crash or an error it stays, for the
    // supervisor, which reads what the record says of the client and then removes it.
    feed.record.remove();
    // Leaving never waits for a paste under way: it runs as a process of its own and finishes on its own.
    Ok(end)
}

/// Reads this Mac once, builds the app on it and enters the screen with `enter`, then shows the first frame's
/// data from that read ([`Feed::refresh`]), not from a second one. The screen is entered before the data
/// is shown, as it always was; `enter` is a parameter so that a test needs no terminal.
fn open_app<'c, S>(
    feed: &mut Feed<'_>,
    cfg: &'c Config,
    theme: super::theme::Theme,
    settings: String,
    came_from: Option<String>,
    enter: impl FnOnce() -> anyhow::Result<S>,
) -> anyhow::Result<(App<'c>, S)> {
    feed.read_local();
    let last = feed
        .tmux
        .list_sessions()
        .ok()
        .and_then(|sessions| last_session_pane(&sessions, &feed.local.snap));
    let (snap, worktrees) = feed.combined();
    let mut app = App::new(cfg, theme, settings, snap);
    app.set_worktrees(worktrees);
    app.set_zoxide(zoxide_dirs(cfg));
    app.enter_app_mode(came_from, last.as_deref());
    let screen = enter()?;
    // The first frame is drawn from the read already made, not from a second one.
    feed.refresh(feed.tmux, &mut app);
    Ok((app, screen))
}

/// [`open_app`], then the go `taw app --go` asked for ([`Feed::go_first`], waiting up to the given time for its
/// machine), all before the first frame of the loop is drawn: `run_app` draws its first frame only after this
/// returns, so the go comes first.
fn open_going<'c, S: Lending>(
    feed: &mut Feed<'_>,
    cfg: &'c Config,
    theme: super::theme::Theme,
    settings: String,
    came_from: Option<String>,
    go: Option<(&GoTarget, Duration)>,
    enter: impl FnOnce() -> anyhow::Result<S>,
) -> anyhow::Result<(App<'c>, S)> {
    let (mut app, mut screen) = open_app(feed, cfg, theme, settings, came_from, enter)?;
    if let Some((target, wait)) = go {
        feed.go_first(&mut app, &mut screen, target, wait)?;
    }
    Ok((app, screen))
}

/// What this Mac last said, kept for the tick that cannot read it: a failed read keeps the last good
/// one, as the popup does.
#[derive(Default)]
struct Local {
    snap: Snapshot,
    worktrees: Vec<WorktreeRecord>,
}

/// Everything a tick of the app reads, and the machines it holds.
struct Feed<'a> {
    tmux: &'a Tmux,
    cfg: &'a Config,
    paths: &'a Paths,
    git: data::GitWorker,
    hub: Machines,
    /// The machines the hub holds, kept to start it again.
    machines: Vec<Machine>,
    /// This Mac's tmux server as the hello says it, or empty while there is none.
    server: String,
    /// The ssh the attaches and the commands on a machine are run through (`TAW_SSH_BIN`), made once.
    ssh: Ssh,
    local: Local,
    /// The attaches that ended on a machine with this app's holder, kept (newest last, at most [`KEPT_LEFTS`])
    /// until an attach reads them: a line can come while the client still runs. The machine is its place in
    /// `[machines]`.
    lefts: Vec<(usize, Left)>,
    /// The walk this app last said, and when ([`Feed::say_walk`]).
    walk_said: Option<(Vec<String>, Instant)>,
    /// What the toasts need: this Mac's events tail, the top line's words, who was live.
    toasts: toasts::Toaster,
    /// The reads of the pane under the cursor when it is a machine's ([`remote_preview`]).
    remote_preview: remote_preview::RemotePreview,
    /// The verbs sent to a machine's rows that it has not answered ([`remote_actions`]).
    remote: remote_actions::Rows,
    /// The paste of an image under way, run off the screen's thread ([`super::pasting`]).
    pasting: super::pasting::Pasting,
    record: Record,
    /// The pane this app runs in (`$TMUX_PANE`), whose client's light or dark mode `[theme] auto_switch` follows; `None`
    /// outside tmux, where the app keeps `[theme] name`.
    own_pane: Option<String>,
}

/// How long this Mac's walk file is left as it is when the walk has not changed: the notification key takes
/// a walk younger than 15 s, so it is written again well before that.
const WALK_FILE_EVERY: Duration = Duration::from_secs(5);

/// What the app owes the machines and the walk file about the walk it has now.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum WalkDue {
    /// The walk is what was said lately.
    Nothing,
    /// The walk is what was said, but this Mac's file is due to be written again.
    File,
    /// The walk changed (or none was said): said to every machine, and this Mac's file is written.
    Send,
}

/// Whether `stops` is to be said, judged against `said` (the walk last said and when): on a change, not on
/// every tick. The machines hear it again with every lease line ([`Machines::send_walk`] keeps it), so only
/// this Mac's own file is written again while the walk is unchanged.
fn walk_due(said: Option<&(Vec<String>, Instant)>, stops: &[String], now: Instant) -> WalkDue {
    match said {
        Some((last, _)) if last != stops => WalkDue::Send,
        Some((_, at)) if now.duration_since(*at) >= WALK_FILE_EVERY => WalkDue::File,
        Some(_) => WalkDue::Nothing,
        None => WalkDue::Send,
    }
}

/// What an attach that ended leaves to do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Nothing: the list is back (or the app is to end).
    Done,
    /// Go to `go` at once: the `left` line of the client that ended carried it. `pane` is the pane that line
    /// names (in the app's names), where the cursor goes if `go` cannot be gone to.
    Go { go: String, pane: Option<String> },
}

/// How many `left` lines of this app the feed keeps: one is expected for each attach, and what is kept is
/// cleared before the next.
const KEPT_LEFTS: usize = 16;

/// What a connection needs, with `server` as this Mac's own tmux server (empty when none runs).
fn conn_ctx(paths: &Paths, server: &str) -> ConnCtx {
    ConnCtx {
        ssh: Ssh::from_env(),
        paths: paths.clone(),
        holder: conn::holder_word(),
        timing: Timing::default(),
        local_server: Some(server.to_string()).filter(|word| !word.is_empty()),
    }
}

impl Feed<'_> {
    /// This Mac's sessions and closed worktrees. No tmux server is an empty list, not an error, and
    /// the snapshot has no current or previous session: the app has no client whose they would be.
    fn read_local(&mut self) {
        match data::build_without_client(self.tmux, self.cfg, &self.git) {
            Ok(snap) => {
                self.local.snap = for_app(snap);
                // The theme of the client that shows this pane, only when the theme follows the terminal.
                if self.cfg.theme.auto_switch {
                    self.local.snap.client_theme = self
                        .own_pane
                        .as_deref()
                        .and_then(|pane| data::read_pane_theme(self.tmux, pane));
                }
            }
            Err(err) => {
                if !crate::tmux::no_server(&err) {
                    log::debug!("the app's refresh failed, keeping the last snapshot: {err:#}");
                } else {
                    self.local.snap = Snapshot::default();
                    // The word it had is of a server that is gone: the next one is read when it is up, and
                    // the connections start again with it ([`Feed::follow_the_server`]).
                    self.server.clear();
                }
            }
        }
        match crate::session::worktree::list(self.tmux, self.paths) {
            Ok(worktrees) => self.local.worktrees = worktrees,
            Err(err) => log::debug!("listing worktrees for the app failed: {err:#}"),
        }
    }

    /// The rows and the closed worktrees the screen shows: this Mac's with the machines' added.
    /// Both merges, every time.
    fn combined(&self) -> (Snapshot, Vec<WorktreeRecord>) {
        combine(&self.local, &self.hub.views(), files::now_ms())
    }

    /// Reads again and shows it, with the preview of the row the cursor is on.
    fn show(&mut self, tmux: &Tmux, app: &mut App) {
        self.read_local();
        self.refresh(tmux, app);
    }

    /// Shows what was read last (this Mac's from [`Feed::read_local`], the machines' as they are now), with
    /// the preview of the row the cursor is on. Reads nothing of this Mac.
    fn refresh(&mut self, tmux: &Tmux, app: &mut App) {
        let (snap, worktrees) = self.combined();
        self.say_walk(&snap, Instant::now());
        for words in self.toasts.no_session_news(&snap) {
            app.toast(words);
        }
        app.set_data(Some(snap), Some(worktrees));
        if app.preview_remote_pane().is_some() {
            self.step_remote_preview(app);
            return;
        }
        // A read of a machine's pane in flight is ended: the cursor is not on a machine's pane.
        self.remote_preview.follow(app, Instant::now());
        let pane = app.preview_pane();
        let text = capture_preview(tmux, pane.as_deref());
        app.set_preview(pane, text);
    }

    /// The preview of the pane of a machine under the cursor: its read is started and its reply taken
    /// ([`remote_preview::RemotePreview::step`]), on a thread of its own. Called once a tick.
    fn step_remote_preview(&mut self, app: &mut App) {
        let (ssh, paths, machines) = (&self.ssh, self.paths, &self.machines);
        self.remote_preview
            .step(app, Instant::now(), &mut |name, raw, stop| {
                let machine = machines.iter().find(|machine| machine.name == name)?;
                remote_preview::launch(ssh, paths, machine, raw, stop)
            });
    }

    /// What follows a key: the preview of the cursor row's pane. A machine's pane is only switched to (its read
    /// comes with the tick, so a held key is not a read per key); this Mac's is captured as the popup does.
    fn follow_preview(&mut self, tmux: &Tmux, app: &mut App) {
        if app.preview_remote_pane().is_some() {
            if app.show_preview {
                self.remote_preview.follow(app, Instant::now());
            }
        } else {
            self.remote_preview.follow(app, Instant::now());
            follow_preview(tmux, app);
        }
    }

    /// Says the walk of `snap` to every machine when it changed, and writes this Mac's walk file (`you` empty:
    /// this Mac's ids are bare in it) when it changed or is due again. With no `[machines]` nothing is said and
    /// no file is written. A machine that is this Mac's own server is not told: its walk file is this one.
    ///
    /// `now` is the instant the walk is judged at (the clock is the caller's, so a test does not depend on how long
    /// the machine stalls).
    fn say_walk(&mut self, snap: &Snapshot, now: Instant) {
        if self.machines.is_empty() {
            return;
        }
        let stops = walk::stops(snap);
        let due = walk_due(self.walk_said.as_ref(), &stops, now);
        if due == WalkDue::Nothing {
            return;
        }
        if due == WalkDue::Send {
            let views = self.hub.views();
            for (at, machine) in self.machines.iter().enumerate() {
                if views.get(at).is_some_and(|view| view.this_server) {
                    continue;
                }
                self.hub.send_walk(at, &machine.name, &stops);
            }
        }
        files::write_walk(
            self.paths,
            self.record.holder(),
            "",
            &stops,
            files::now_ms(),
        );
        self.walk_said = Some((stops, now));
    }

    /// While a client has the terminal no tick runs, so the walk is looked at here, every
    /// [`WALK_FILE_EVERY`]: what needs you changes while you are away, and the key in the client reads it.
    fn keep_walk(&mut self) {
        if self.machines.is_empty()
            || self
                .walk_said
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < WALK_FILE_EVERY)
        {
            return;
        }
        self.read_local();
        let (snap, _) = self.combined();
        self.say_walk(&snap, Instant::now());
    }

    /// While this Mac has no tmux server its word is empty (a read that finds none empties it), and a machine
    /// of `[machines]` that is this Mac itself cannot be told from another. When a server comes up the word is
    /// read and the connections are started again with it (the machines show greyed for the second that takes), so that machine
    /// is not listed twice.
    fn follow_the_server(&mut self, tmux: &Tmux) {
        if !self.server.is_empty() {
            return;
        }
        let word = feed::server_word(tmux);
        if word.is_empty() {
            return;
        }
        if !self.machines.is_empty() {
            // The old connections end first: the lock that keeps them is theirs until then.
            drop(std::mem::replace(
                &mut self.hub,
                Machines::start(conn_ctx(self.paths, ""), Vec::new()),
            ));
            self.hub = Machines::start(conn_ctx(self.paths, &word), self.machines.clone());
            // The new connections have heard no walk: the next look says it again, unchanged or not.
            self.walk_said = None;
            // And none of them is connected yet: a machine that was live is not said to have dropped off.
            self.toasts.forget_live();
        }
        self.server = word;
    }

    /// One tick: the connections move on, then the screen is read again.
    fn tick(&mut self, tmux: &Tmux, app: &mut App) {
        self.follow_the_server(tmux);
        self.hub.poll(Instant::now());
        self.take_announced();
        self.say_toasts(app);
        self.show(tmux, app);
    }

    /// Takes what the machines announced since the last call, so that it does not pile up in an app that runs
    /// for days. The toasts are delivered where the person is ([`Feed::announce_toasts`]); a `left` line that is
    /// addressed to this app is kept ([`KEPT_LEFTS`] of them), the others are another app's and are dropped.
    fn take_announced(&mut self) {
        self.announce_toasts();
        for (machine, left) in self.hub.lefts() {
            if left.holder != self.record.holder() {
                continue;
            }
            if self.lefts.len() == KEPT_LEFTS {
                self.lefts.remove(0);
            }
            self.lefts.push((machine, left));
        }
    }

    /// The toast of a paste that has ended since the last look, if one has.
    fn say_pasted(&mut self, app: &mut App) {
        if let Some(words) = self.pasting.poll() {
            app.toast(words);
        }
    }

    /// Runs one action of the screen. A landing goes in on this Mac ([`Feed::go_in`]); the rest (a
    /// rename, a close, a prompt) act on this Mac's tmux as the popup's do.
    fn perform(
        &mut self,
        taw_bin: &Path,
        app: &mut App,
        screen: &mut impl Lending,
        action: &actions::Action,
    ) -> anyhow::Result<()> {
        // An action on a machine's row is that machine's to do: this Mac's tmux and hooks are not asked.
        if let Some((machine, raw)) = remote_actions::unprefixed(action) {
            self.perform_remote(app, &machine, &raw, action);
            return Ok(());
        }
        // The paste-image key runs in the background, here or on a machine (the image is sent over its ssh): the
        // screen keeps drawing and taking keys, and the toast comes when the paste ends ([`Feed::say_pasted`]).
        if let actions::Action::PasteImage { pane } = action {
            self.pasting
                .press(self.tmux, self.paths, pane, |words| app.toast(words));
            self.show(self.tmux, app);
            return Ok(());
        }
        // With machines the next stop is read off the list's own snapshot, across them all, this Mac's first,
        // and gone to the way Enter on its row goes. With none it is this Mac's tmux that is asked, as ever.
        if let actions::Action::NextUp { own } = action
            && !self.machines.is_empty()
        {
            match walk::next_after(&app.snap, own.as_deref()) {
                Some(go) => {
                    let step = Step::Go {
                        go,
                        pane: own.clone(),
                    };
                    self.follow_go(app, screen, step)?;
                }
                None => {
                    app.toast(actions::NO_AGENTS);
                    self.show(self.tmux, app);
                }
            }
            return Ok(());
        }
        if lands(action) {
            let active_pane_of = |session: &str| {
                app.snap
                    .spaces
                    .iter()
                    .find(|space| space.session_id == session)
                    .and_then(|space| space.active_pane_id.clone())
            };
            // A machine's row goes in on that machine, never through this Mac's landing.
            if let Some(route) = machine::route(action, active_pane_of) {
                self.go_by_route(app, screen, route)?;
                return Ok(());
            }
            match actions::land(self.tmux, self.cfg, self.paths, taw_bin, action) {
                // What the landing made (a session, a worktree) is in the list when it is back: going in
                // shows it, so it is not read a second time here.
                Ok(landing) => self.go_in(app, screen, landing)?,
                Err(words) => {
                    app.toast(words);
                    self.show(self.tmux, app);
                }
            }
            return Ok(());
        }
        let outcome = actions::exec(self.tmux, self.cfg, self.paths, taw_bin, action, None)?;
        if let Some(toast) = outcome.toast {
            app.toast(toast);
        }
        // The action changed tmux, so show the result now rather than a tick later.
        self.show(self.tmux, app);
        if let Some(question) = outcome.ask {
            app.ask(question);
        }
        if app.after_action(action, Instant::now()) {
            // Keys typed while a close ran were typed at its confirmation, not at the tree.
            while event::poll(Duration::ZERO)? {
                let _ = event::read()?;
            }
        }
        Ok(())
    }

    /// Goes in: the screen is given to a `tmux attach` of this Mac's server, run as a child with the
    /// terminal and waited for, and taken back when it ends. The record names the client while it
    /// lives. Coming back, the list is read again, every machine that waits tries now, and a filter that was
    /// typed is closed (the rows are built again). One pane is tried for the cursor: the one tmux named when
    /// the popup key sent the client back or, when it named none, the one you went into (`came_from`, which
    /// Back goes in at again). The cursor moves only when the list draws that pane (or its session); a pane
    /// tmux named that is not drawn is not given up for the other one, and the cursor stays where it was.
    fn go_in(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        landing: actions::Landing,
    ) -> anyhow::Result<()> {
        let step = self.go_in_once(app, screen, landing)?;
        self.follow_go(app, screen, step)
    }

    /// One attach of this Mac's tmux, as [`Feed::go_in`] describes it, without what its end sends on: a `left`
    /// line that carries `go` (the key in the client walked on to a stop of another machine) is not acted on
    /// here, and the list is not drawn for it: it comes back as [`Step::Go`].
    fn go_in_once(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        landing: actions::Landing,
    ) -> anyhow::Result<Step> {
        // A session that went while the list was on the screen is told here, in words, before the screen
        // is given away; `tmux attach` would say only that it exited 1 (its reason goes to the log).
        if self
            .tmux
            .run_ok(&["has-session", "-t", &landing.target])
            .is_err()
        {
            app.toast("that session is gone");
            // Back landed here at its own pane, and that pane went with its session: the next Back is
            // the end of the app, not a second "that pane is gone".
            if landing.pane.is_some() && app.came_from() == landing.pane.as_deref() {
                app.forget_came_from();
            }
            self.show(self.tmux, app);
            return Ok(Step::Done);
        }
        // What was written before this is not news: the key back writes its line after the attach starts.
        let mut tail = files::EventTail::at_end(self.paths);
        let command = crate::cli::open::attach_to(self.tmux, &landing.target);
        let holder = self.record.holder().to_string();
        // The terminal is the client's while it runs. The wait for the line comes after the screen is back
        // (raw, so no key is echoed and none ends or stops the app): the line comes before the client goes,
        // so it is there already, and a client that ended another way has none, and a short wait is how
        // that is told. An app that was asked to end does not wait.
        let supervisor = self.record.supervisor();
        let (ended, back_in) = lend_through(
            screen,
            || self.attach(command, ""),
            // The client was ended for a signal or a gone supervisor (`wait_for`): the screen is not taken back.
            ending(supervisor),
            || left_line(&mut tail, &holder, LEFT_WAIT),
        )?;
        let back_in = back_in.flatten();
        if signalled() || self.record.orphaned() {
            // When the screen was not taken back (`lend_through` skipped it) it is the shell's now; when the
            // client ended by itself and the screen was taken back before the signal came, it is still the app's.
            // Either way nothing more is drawn: the loop's first look, before it draws again, sees the flag or
            // the gone supervisor and ends the app, and the end puts away what is still the app's.
            return Ok(Step::Done);
        }
        let failure = match ended {
            Ok(status) if !status.success() => Some(format!("tmux ended the attach with {status}")),
            Ok(_) => None,
            Err(words) => Some(words),
        };
        // A line that says where to go next is acted on at once, with the list not drawn: the person pressed
        // the key to go on, and what they were in is not a place to come back to.
        if failure.is_none()
            && let Some(left) = back_in.as_ref()
            && let Some(go) = left.go.as_deref()
        {
            return Ok(Step::Go {
                go: go.to_string(),
                pane: Some(left.pane.clone()).filter(|pane| !pane.is_empty()),
            });
        }
        // With no pane to come back to (tmux named none) the filter is still closed and the pane of an
        // earlier visit is still forgotten.
        let pane = back_in
            .map(|left| left.pane)
            .filter(|pane| !pane.is_empty())
            .or(landing.pane);
        self.came_back(app, landing.note.as_deref(), failure.as_deref(), pane);
        Ok(Step::Done)
    }

    /// Acts on what the attaches that just ended left to do, one hop at a time, until one leaves nothing: each
    /// hop is a key the person pressed in a client, so the chain is as long as they press and no longer, and it
    /// is a loop so that a long walk takes no stack.
    pub(super) fn follow_go(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        mut step: Step,
    ) -> anyhow::Result<()> {
        while let Step::Go { go, pane } = step {
            // Every hop looks first, this Mac's as well as a machine's: a hop lends the terminal to a client, and
            // a signal or a gone supervisor means the terminal is the shell's now.
            if must_end(self) {
                return Ok(());
            }
            step = self.go_to(app, screen, &go, pane)?;
        }
        Ok(())
    }

    /// Goes to the stop `go` (the app's name for it: `%7` is this Mac's pane, `studio:%16` a machine's), the way
    /// Enter on its row goes. `pane` is the pane (in the app's names) the person was in, where the cursor goes
    /// when the stop cannot be gone to: the usual toast words say why, and the list is shown.
    fn go_to(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        go: &str,
        pane: Option<String>,
    ) -> anyhow::Result<Step> {
        match crate::machine::split_id(go) {
            (Some(machine), raw) => {
                let (went, step) =
                    self.go_into_machine_once(app, screen, machine, raw, Some(raw), None)?;
                if went == machine::Went::Refused {
                    app.enter_app_mode(pane, None);
                }
                Ok(step)
            }
            (None, raw) => {
                let landing = (crate::machine::valid_tmux_id(raw) && raw.starts_with('%'))
                    .then(|| actions::land_on_pane(self.tmux, raw))
                    .flatten();
                match landing {
                    Some(landing) => self.go_in_once(app, screen, landing),
                    None => {
                        app.toast(if crate::machine::valid_tmux_id(raw) {
                            "that pane is gone"
                        } else {
                            crate::machine::words::NOTHING_TO_GO_IN_TO
                        });
                        self.show(self.tmux, app);
                        app.enter_app_mode(pane, None);
                        Ok(Step::Done)
                    }
                }
            }
        }
    }

    /// Back is going in again, at `pane`, the one you came out of: a machine's pane (`studio:%16`) is gone into on
    /// that machine, this Mac's through its own tmux. A pane that cannot be gone into (a machine that is away or
    /// no longer in `[machines]`, a pane or session that is gone) is told in a toast and forgotten, so that the
    /// next Back ends the app and does not say it again.
    fn go_back_in(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        pane: &str,
    ) -> anyhow::Result<()> {
        if let (Some(name), raw) = crate::machine::split_id(pane) {
            let went = self.go_into_machine(app, screen, name, raw, Some(raw), None)?;
            if went != machine::Went::Attached {
                app.forget_came_from();
            }
            return Ok(());
        }
        match actions::land_on_pane(self.tmux, pane) {
            Some(landing) => self.go_in(app, screen, landing)?,
            None => {
                app.toast("that pane is gone");
                app.forget_came_from();
            }
        }
        Ok(())
    }

    /// What the app does when it is back from an attach, of this Mac's or a machine's: says the note and why
    /// the attach ended badly, shows the list read again, lets every machine that waits try now, and puts the
    /// cursor on `pane` (a filter that was typed is closed either way).
    fn came_back(
        &mut self,
        app: &mut App,
        note: Option<&str>,
        failure: Option<&str>,
        pane: Option<String>,
    ) {
        if let Some(words) = back_words(note, failure) {
            app.toast(words);
        }
        self.show(self.tmux, app);
        self.hub.wake();
        app.enter_app_mode(pane, None);
    }

    /// Runs `command` on the terminal and waits, with the record saying which client it is. The wait keeps
    /// the machines moving ([`Feed::wait_for`]), and ends early when a signal was passed on or the supervisor is gone.
    fn attach(
        &mut self,
        mut command: Command,
        inside: &str,
    ) -> Result<std::process::ExitStatus, String> {
        // An old line is never taken for this attach's: neither one kept before nor one still queued in the hub.
        // The hub is polled first, so that what a connection has already read is in the queue and goes with the
        // rest. A line still on the wire (not yet read by a connection thread) cannot be told apart this way.
        self.hub.poll(Instant::now());
        self.take_announced();
        self.lefts.clear();
        let child = command
            .spawn()
            .map_err(|err| format!("could not attach: {err}"))?;
        self.record.attached(child.id(), &own_tty(), inside);
        // Ends with this call however the app ends: dropped at the end of it, and a process of its own for the
        // end that runs no code of the app's (`SIGKILL`).
        let _guard = ClientGuard::start(
            child.id() as libc::pid_t,
            self.record.supervisor(),
            is_remote(inside),
        );
        // What was taken just now was for the list: the person is not there. A toast taken in the tick of
        // the person's Enter is therefore not shown anywhere (accepted: it is one tick).
        self.toasts.forget_top();
        let ended = self.wait_for(child.id(), is_remote(inside));
        // The machines are looked at once more while the person is still inside: a machine that went away
        // with the attach is no machine that dropped off after it.
        self.hub.poll(Instant::now());
        self.take_announced();
        self.record.detached();
        ended
    }

    /// Waits for the attach `pid`, the way the supervisor waits for its child (a stopped client is set going
    /// again), and every [`WAIT_LOOK`] lets the machines move on with no drawing: the keeper offers its
    /// lease and writes its files, so the followers do not take the machines for gone while a client is
    /// attached. The wait is the one owner of ending the client: a signal passed on ends it, and so does a
    /// supervisor that is gone, and the client is told to end as well ([`end_the_client`]). `remote` says the
    /// client is an ssh: see [`end_and_put_away`].
    fn wait_for(&mut self, pid: u32, remote: bool) -> Result<std::process::ExitStatus, String> {
        use std::os::unix::process::ExitStatusExt;
        let pid = pid as libc::pid_t;
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: `waitpid` of the client this process started and has not reaped, with a status of its
            // own to write to. `Child` is never asked to wait for it again.
            let waited =
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
            if waited == pid {
                if libc::WIFSTOPPED(status) {
                    // SAFETY: `kill` of the client that has just been seen stopped, so not yet reaped.
                    unsafe { libc::kill(pid, libc::SIGCONT) };
                    continue;
                }
                return Ok(std::process::ExitStatus::from_raw(status));
            }
            if waited < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err("could not wait for the attach".to_string());
            }
            if signalled() {
                // The client has the terminal, and a signal to this app hangs nothing up that it would notice:
                // it is told the same, so that it detaches, and is waited for (it is this app's child). The
                // screen is not taken back over a client that has just been told to end.
                end_and_put_away(pid, the_signal(), remote);
                return Err("the app was asked to end".to_string());
            }
            if self.record.orphaned() {
                // The supervisor is gone, so the terminal's program has gone: the client is hung up the way a
                // closed terminal would, and this is the one place that ends it (the watchdog never does).
                end_and_put_away(pid, libc::SIGHUP, remote);
                return Err("the app's supervisor is gone".to_string());
            }
            self.hub.poll(Instant::now());
            // What the machines announce is taken as the tick takes it: a line of the machine this client is
            // attached to arrives while the client still runs.
            self.take_announced();
            self.keep_walk();
            std::thread::sleep(WAIT_LOOK);
        }
    }
}

/// Whether the program that has the terminal is an ssh: an attach that has a machine (`inside`) is, this Mac's
/// own tmux client is not. A tmux client puts the terminal back as it goes, because its server asks it to; an
/// ssh that is ended does not undo what the tmux on the far side set (the alternate screen, the mouse capture),
/// and what that tmux wrote to undo it is lost with the connection.
fn is_remote(inside: &str) -> bool {
    !inside.is_empty()
}

/// How the client the app told to end was left.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum End {
    /// It ended within the wait and is reaped.
    Gone,
    /// An ssh that did not end within the wait was sent `SIGKILL` (reaped within [`KILLED_REAP_WAIT`] or not: it
    /// runs no more code either way).
    Killed,
    /// A tmux client that did not end within the wait, left running.
    Running,
}

/// Whether the app writes the screen's restore bytes after ending the client: for an ssh that is gone or killed.
/// Never over a client that may still be running, which would draw them beside what it writes.
fn puts_away_after(remote: bool, end: End) -> bool {
    remote && end != End::Running
}

/// Whether the app puts the terminal's modes back after ending the client: only for an ssh it killed. One that ended
/// by itself undid the raw modes it had set as it went.
fn puts_modes_back_after(remote: bool, end: End) -> bool {
    remote && end == End::Killed
}

/// Sets the modes `saved` on the terminal `fd` at once. `fd` is standard input in the app; a test aims it at a pty.
fn put_back_modes(fd: libc::c_int, saved: &libc::termios) {
    // SAFETY: `tcsetattr` of a descriptor number with a `termios` that is alive; a failure is all there is.
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, saved);
    }
}

/// Ends the client `pid` that has the terminal ([`end_the_client`]) and, for an ssh that is gone or killed, puts
/// the terminal away ([`put_the_terminal_away`] with the screen's own restore bytes, one `write(2)`), and for one
/// it killed puts the cooked modes of [`super::cooked_modes`] back (the ssh set the terminal raw and died before it
/// could undo that; crossterm's own raw mode is off already, so [`put_the_terminal_away`]'s does nothing here).
/// The watchdog writes nothing while the screen is lent, and its grace covers this whole sequence ([`GRACE`]), so
/// only a main thread that is stuck while an ssh has the terminal is left unhandled: the ssh ends with its terminal,
/// and the far tmux's mode stays.
fn end_and_put_away(pid: libc::pid_t, signal: libc::c_int, remote: bool) {
    let end = end_the_client(pid, signal, remote);
    if puts_away_after(remote, end) {
        put_the_terminal_away(&super::restore_sequence());
    }
    if puts_modes_back_after(remote, end)
        && let Some(saved) = super::cooked_modes()
    {
        put_back_modes(0, &saved);
    }
}

/// Passes `signal` to the client `pid` (a tmux client detaches cleanly on `SIGTERM` and `SIGHUP`) and waits for it,
/// reaping it, for about [`CLIENT_END_WAIT`]. A client that is stopped is set going, since a signal waits for it.
/// Answers how it was left. An ssh (`remote`) that is still there after the wait gets `SIGKILL` and a reap
/// bounded by [`KILLED_REAP_WAIT`] (so a remote client is `Gone` or `Killed`, never `Running`); a tmux client that
/// is not gone is left as it is.
fn end_the_client(pid: libc::pid_t, signal: libc::c_int, remote: bool) -> End {
    // SAFETY: `kill` of the client this process started and has not reaped.
    unsafe {
        libc::kill(pid, signal);
        libc::kill(pid, libc::SIGCONT);
    }
    if reaped_within(pid, CLIENT_END_WAIT) {
        return End::Gone;
    }
    if !remote {
        return End::Running;
    }
    // SAFETY: as above: the client was not reaped, so its pid is still this process's child.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    reaped_within(pid, KILLED_REAP_WAIT);
    End::Killed
}

/// How often the guard of a client looks, and how long it gives a client it has hung up to end before `SIGKILL`.
const GUARD_LOOK_MS: libc::c_long = 100;
const GUARD_WAIT_LOOKS: u32 = 8;

/// A process the app forks while a client (a tmux client of this Mac, or the ssh of a machine's) has the terminal,
/// so that the client ends with the app however the app ends. The app's own wait ends the client when the app is
/// told to end or its supervisor goes ([`Feed::wait_for`]); an app that is killed (`SIGKILL`, the out-of-memory
/// killer) runs nothing, and the client, which is not in a group anything ends, would stay on the terminal beside
/// the shell that has it back. The guard looks every [`GUARD_LOOK_MS`] whether the app and its supervisor are both
/// gone: an app that is gone while its supervisor lives leaves the client where it is, on purpose (the supervisor
/// waits for it and starts the list again once it ends). Then it hangs the client up, kills it if it is still there
/// after [`GUARD_WAIT_LOOKS`] looks, and for an ssh puts the terminal away as the app's own end does. It ends by
/// itself when the client is gone, and the app ends it (and reaps it) when the attach is over. It touches the one
/// pid it was given, never a name.
struct ClientGuard {
    pid: libc::pid_t,
}

/// Sleeps `ms` milliseconds without allocating or taking a lock (the guard runs in a forked copy of a process with
/// threads, so it may call only what is async-signal-safe).
fn guard_nap(ms: libc::c_long) {
    let nap = libc::timespec {
        tv_sec: 0,
        tv_nsec: ms * 1_000_000,
    };
    // SAFETY: `nanosleep` of a timespec that is alive; an interrupted sleep only ends early.
    unsafe { libc::nanosleep(&nap, std::ptr::null_mut()) };
}

impl ClientGuard {
    /// Forks the guard of `client`, a child of this process. `None` when the fork fails: the attach then runs as
    /// it did, with only the app's own wait to end it.
    fn start(client: libc::pid_t, supervisor: libc::pid_t, remote: bool) -> Option<ClientGuard> {
        // Everything the guard needs is made here: the child of a fork may not allocate or lock.
        let away = if remote {
            super::restore_sequence()
        } else {
            Vec::new()
        };
        let cooked = if remote { super::cooked_modes() } else { None };
        // SAFETY: `getpid` takes nothing and cannot fail; `tcgetpgrp` of a descriptor number (-1 for no terminal).
        let (app, foreground) = unsafe { (libc::getpid(), libc::tcgetpgrp(0)) };
        // SAFETY: the child of this fork calls only async-signal-safe functions (`guard_run`) and ends with
        // `_exit`; it never returns into this process's code.
        match unsafe { libc::fork() } {
            -1 => None,
            0 => guard_run(app, supervisor, client, &away, cooked.as_ref(), foreground),
            pid => Some(ClientGuard { pid }),
        }
    }
}

impl Drop for ClientGuard {
    /// The attach is over: the guard is ended and reaped (it only sleeps and looks, so `SIGKILL` loses nothing).
    fn drop(&mut self) {
        // SAFETY: `kill` and `waitpid` of the child this process forked and has not reaped.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            let mut status: libc::c_int = 0;
            loop {
                let waited = libc::waitpid(self.pid, &mut status, 0);
                if waited >= 0
                    || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
    }
}

/// The body of the guard of `client` for the app `app` under `supervisor`: only async-signal-safe calls, no allocation. `away` is what
/// puts the screen away (empty for a client of this Mac's own tmux, which does it itself) and `cooked` the modes the
/// terminal had before the client took it, put back after a client that had to be killed, and only while the
/// terminal's foreground process group is still `foreground`, the one it had at the attach: a shell that has taken the
/// terminal back has set its own modes (a line editor's), which are not put into cooked mode under it.
fn guard_run(
    app: libc::pid_t,
    supervisor: libc::pid_t,
    client: libc::pid_t,
    away: &[u8],
    cooked: Option<&libc::termios>,
    foreground: libc::pid_t,
) -> ! {
    // SAFETY: every call below is async-signal-safe and takes only values or buffers that are alive.
    unsafe {
        // A guard that is stopped for writing to a terminal that is another group's now would never end.
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        // What the app holds (a lock, the pipes of the connections) is not the guard's to keep alive.
        for fd in 3..1024 {
            libc::close(fd);
        }
        loop {
            if libc::kill(client, 0) != 0 {
                libc::_exit(0);
            }
            // Both gone: the app (this guard's parent changed) and the supervisor (no process, or init's pid).
            if libc::getppid() != app && (supervisor <= 1 || libc::kill(supervisor, 0) != 0) {
                break;
            }
            guard_nap(GUARD_LOOK_MS);
        }
        libc::kill(client, libc::SIGHUP);
        libc::kill(client, libc::SIGCONT);
        let mut killed = false;
        for look in 0..=GUARD_WAIT_LOOKS {
            guard_nap(GUARD_LOOK_MS);
            if libc::kill(client, 0) != 0 {
                break;
            }
            if look == GUARD_WAIT_LOOKS {
                libc::kill(client, libc::SIGKILL);
                killed = true;
                guard_nap(GUARD_LOOK_MS);
            }
        }
        if !away.is_empty() && libc::isatty(1) == 1 {
            libc::write(1, away.as_ptr().cast(), away.len());
        }
        if killed
            && let Some(saved) = cooked
            && foreground > 0
            && libc::tcgetpgrp(0) == foreground
        {
            libc::tcsetattr(0, libc::TCSANOW, saved);
        }
        libc::_exit(0)
    }
}

/// Whether the child `pid` is gone, reaping it, within about `wait`. An interrupted `waitpid` is asked again (it
/// says nothing of the child); `ECHILD` means the child is not this process's to wait for, which counts as gone.
fn reaped_within(pid: libc::pid_t, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `waitpid` of that client, with a status of its own to write to.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if waited != 0 {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The check that the app was told to end while a client had the terminal: a signal came, or its supervisor is
/// gone. When it holds, the screen is not taken back (`lend_through`).
fn ending(supervisor: libc::pid_t) -> impl FnOnce() -> bool {
    // SAFETY (in the closure): `getppid` takes nothing and cannot fail.
    move || signalled() || gone_from(supervisor, unsafe { libc::getppid() })
}

/// What the app says when it is back from an attach: the popup's note for the landing (a name that had to
/// change, `created worktree X`), and why the attach ended badly, when it did. A note in front of a failure
/// reads as the popup's does: `note (reason)`.
fn back_words(note: Option<&str>, failure: Option<&str>) -> Option<String> {
    match (note, failure) {
        (Some(note), Some(why)) => Some(format!("{note} ({why})")),
        (Some(words), None) | (None, Some(words)) => Some(words.to_string()),
        (None, None) => None,
    }
}

/// How often the wait for an attach lets the machines move on.
const WAIT_LOOK: Duration = Duration::from_millis(150);

/// How long the app waits, after its client ended, for the line the key back writes.
const LEFT_WAIT: Duration = Duration::from_millis(500);

/// The first `left` line addressed to `holder` that is in the events file by `wait` from now, or `None` when
/// none came. Looks at once, so a line that is already there costs nothing.
fn left_line(tail: &mut files::EventTail, holder: &str, wait: Duration) -> Option<Left> {
    let until = Instant::now() + wait;
    loop {
        for line in tail.read_new() {
            if let Down::Left(left) = line
                && left.holder == holder
            {
                return Some(left);
            }
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The terminal this process is on, as `ttyname` gives it (`/dev/ttys004`), or empty.
fn own_tty() -> String {
    // SAFETY: `ttyname` takes a descriptor and returns a pointer into static storage or null; the
    // string is copied before anything else can call it.
    unsafe {
        let name = libc::ttyname(0);
        if name.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Whether an action is a landing: it goes somewhere a client is switched to, which the app has none
/// of. The doorway into a machine's new session (`new`, the row of a machine with no session) is one: the app
/// goes in at once.
fn lands(action: &actions::Action) -> bool {
    use actions::Action::{
        Doorway, Jump, NewWorktree, NextUp, OpenProject, OpenWorktree, SwitchWorkspace,
    };
    matches!(
        action,
        Jump { .. }
            | SwitchWorkspace { .. }
            | OpenProject { .. }
            | OpenWorktree { .. }
            | NewWorktree { .. }
            | NextUp { .. }
    ) || matches!(action, Doorway { id, .. } if id == crate::machine::words::NEW_ID)
}

/// `local` with every machine of `views` merged in: the rows and the closed worktrees, as the screen
/// is given them, both at `now_ms`.
fn combine(local: &Local, views: &[MachineView], now_ms: u64) -> (Snapshot, Vec<WorktreeRecord>) {
    (
        machines::merge(local.snap.clone(), views, now_ms),
        machines::merge_worktrees(local.worktrees.clone(), views),
    )
}

/// A snapshot as the app shows it: no row is marked as the session of a client or the one before it.
fn for_app(mut snap: Snapshot) -> Snapshot {
    snap.own_pane = None;
    snap.current_session = None;
    snap.previous_session = None;
    snap
}

/// The first words of the app when there are two doubts: the failure to go to `last`, then the `[machines]`
/// doubt, joined with `; `, so that neither takes the other's place; with one, that one; with none, none.
fn first_words(failure: Option<String>, doubt: Option<String>) -> Option<String> {
    match (failure, doubt) {
        (Some(failure), Some(doubt)) => Some(format!("{failure}; {doubt}")),
        (one, None) | (None, one) => one,
    }
}

/// The active pane of the session attached most lately (`session_last_attached`), among the sessions
/// of `snap` that have a pane to be on; `None` when none was ever attached.
fn last_session_pane(sessions: &[SessionInfo], snap: &Snapshot) -> Option<String> {
    most_lately_attached(sessions.iter().filter(|s| {
        s.session_last_attached > 0
            && snap
                .spaces
                .iter()
                .any(|space| space.machine.is_none() && space.session_id == s.session_id)
    }))
    .and_then(|s| {
        snap.spaces
            .iter()
            .find(|space| space.machine.is_none() && space.session_id == s.session_id)
            .and_then(|space| space.active_pane_id.clone())
    })
}

/// The session of `sessions` attached most lately: the greatest `session_last_attached`, and among equals the
/// FIRST one listed (a server whose sessions were never attached has all of them at 0). Each caller says
/// which sessions count before it asks, so that the cursor and `--go last` choose by the one rule.
fn most_lately_attached<'a>(
    sessions: impl Iterator<Item = &'a SessionInfo>,
) -> Option<&'a SessionInfo> {
    let mut best: Option<&SessionInfo> = None;
    for session in sessions {
        if best.is_none_or(|b| session.session_last_attached > b.session_last_attached) {
            best = Some(session);
        }
    }
    best
}

/// What `--go last` goes to: the session of this Mac attached most lately, and with no tmux server (or none
/// with a session) the session `taw open .` makes for the app's current directory, by [`crate::cli::open::open`]
/// itself (its hooks, its naming), asked for its envelope so that the session it made is named and not parsed
/// out of a command line. The words are those of the failure, for a toast.
fn last_target(
    tmux: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
) -> Result<GoTarget, String> {
    last_of(tmux.list_sessions(), || {
        let ctx = crate::cli::Ctx {
            tmux: tmux.clone(),
            cfg: cfg.clone(),
            paths: paths.clone(),
            taw_bin: taw_bin.to_path_buf(),
        };
        let here = crate::cli::open::Surroundings::from_env().map_err(|e| format!("{e:#}"))?;
        match crate::cli::open::open(&ctx, &[".".to_string()], true, &here) {
            Ok(crate::cli::open::Done::Envelope(envelope)) => envelope["session"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "taw open did not name the session it made".to_string()),
            Ok(_) => Err("taw open did not name the session it made".to_string()),
            Err(e) => Err(format!("{e:#}")),
        }
    })
}

/// [`pick_last`] over a listing of the sessions as tmux gave it: "no server running" is no session (the open
/// makes one), and any other failure of the listing is its words, with `make` never called, so that a listing
/// that failed for a passing reason cannot lead to an open while sessions exist.
fn last_of(
    listing: anyhow::Result<Vec<SessionInfo>>,
    make: impl FnOnce() -> Result<String, String>,
) -> Result<GoTarget, String> {
    let sessions = match listing {
        Ok(sessions) => sessions,
        Err(e) if crate::tmux::no_server(&e) => Vec::new(),
        Err(e) => return Err(format!("{e:#}")),
    };
    pick_last(&sessions, make)
}

/// The session of `sessions` with the greatest `session_last_attached`, the first one listed among equals (a
/// server whose sessions were never attached has all of them at 0), as [`GoTarget::Session`] by its id; with no
/// session, the name `make` gives to the one it makes, as `=name`.
fn pick_last(
    sessions: &[SessionInfo],
    make: impl FnOnce() -> Result<String, String>,
) -> Result<GoTarget, String> {
    match most_lately_attached(sessions.iter()) {
        Some(session) => Ok(GoTarget::Session(session.session_id.clone())),
        None => make().map(|name| GoTarget::Session(format!("={name}"))),
    }
}

/// The app's record file. It is removed by [`Record::remove`] and by nothing else: dropped by a panic or an
/// error it stays, and the supervisor, which reads it after a crash, removes it.
struct Record {
    paths: Paths,
    record: AppRecord,
}

impl Record {
    fn write(paths: &Paths) -> Record {
        let record = AppRecord {
            pid: std::process::id(),
            // SAFETY: `getppid` takes nothing and cannot fail.
            supervisor: unsafe { libc::getppid() }.max(0) as u32,
            holder: conn::holder_word(),
            started_at: files::now_ms(),
            ..AppRecord::default()
        };
        files::write_app_record(paths, &record);
        Record {
            paths: paths.clone(),
            record,
        }
    }

    /// The supervisor the record names.
    fn supervisor(&self) -> libc::pid_t {
        self.record.supervisor as libc::pid_t
    }

    /// Whether the supervisor named by the record is no longer this process's parent.
    fn orphaned(&self) -> bool {
        // SAFETY: `getppid` takes nothing and cannot fail.
        let now = unsafe { libc::getppid() };
        gone_from(self.record.supervisor as libc::pid_t, now)
    }

    /// The lease holder this app is.
    fn holder(&self) -> &str {
        &self.record.holder
    }

    /// The client the app waits for: its process, the terminal it runs on, and the machine it is inside
    /// (`""` for a client of this Mac).
    fn attached(&mut self, client_pid: u32, tty: &str, inside: &str) {
        self.record.client_pid = client_pid;
        self.record.client_tty = tty.to_string();
        self.record.inside = inside.to_string();
        files::write_app_record(&self.paths, &self.record);
    }

    /// The client is gone, and so is the machine it was inside.
    fn detached(&mut self) {
        self.attached(0, "", "");
    }

    /// The app has ended as it was asked to: its record goes.
    fn remove(&self) {
        files::remove_app_record(&self.paths, self.record.pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::wire::{State, WireSpace, WireWorktree};
    use crate::tui::fixtures::{snapshot, worktrees};

    fn session(id: &str, last_attached: u64) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            session_name: id.to_string(),
            session_path: "/".to_string(),
            session_attached: 0,
            session_windows: 1,
            session_group_size: 0,
            active_window_id: "@1".to_string(),
            session_last_attached: last_attached,
        }
    }

    #[test]
    fn a_tick_merges_the_rows_and_the_worktrees_of_every_machine() {
        let local = Local {
            snap: snapshot(),
            worktrees: worktrees(),
        };
        let mut moved = worktrees().remove(0);
        moved.workspace_id = "w9".to_string();
        let state = State {
            spaces: vec![WireSpace {
                session_id: "$3".into(),
                session_name: "garden".into(),
                ..WireSpace::default()
            }],
            worktrees: vec![WireWorktree::from(&moved)],
            ..State::default()
        };
        let views = [MachineView {
            name: "pi".to_string(),
            live: true,
            state: Some(state),
            ..MachineView::default()
        }];

        let (snap, records) = combine(&local, &views, 1_000);
        assert_eq!(snap.machines.len(), 1, "the header of pi");
        assert_eq!(
            snap.spaces.len(),
            local.snap.spaces.len() + 1,
            "its session"
        );
        assert_eq!(records.len(), 2, "this Mac's worktree and pi's");
        assert_eq!(records[1].workspace_id, "pi:w9");
        assert_eq!(records[1].machine, Some(0));

        // With no machines the merge changes nothing: no header, no row, no record.
        let (snap, records) = combine(&local, &[], 1_000);
        assert!(snap.machines.is_empty());
        assert_eq!(snap.spaces.len(), local.snap.spaces.len());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].workspace_id, "w6");
    }

    #[test]
    fn the_app_marks_no_row_as_a_client_session() {
        let mut snap = snapshot();
        snap.current_session = Some("$1".into());
        snap.previous_session = Some("$2".into());
        assert!(snap.own_pane.is_some());
        let snap = for_app(snap);
        assert_eq!(snap.own_pane, None);
        assert_eq!(snap.current_session, None);
        assert_eq!(snap.previous_session, None);
        assert!(!snap.spaces.is_empty(), "and only that is taken off");
    }

    #[test]
    fn the_cursor_opens_on_the_session_attached_most_lately() {
        let snap = snapshot();
        let sessions = [session("$2", 100), session("$4", 300), session("$1", 200)];
        assert_eq!(last_session_pane(&sessions, &snap).as_deref(), Some("%411"));
        // Never attached is no last session; a session the snapshot does not have is not one either.
        let sessions = [session("$2", 0), session("$99", 500)];
        assert_eq!(last_session_pane(&sessions, &snap), None);
        assert_eq!(last_session_pane(&[], &snap), None);
    }

    /// Two sessions attached in the same second: the cursor opens on the first listed and `--go last` goes
    /// into the first listed, by the one rule of [`most_lately_attached`].
    #[test]
    fn two_sessions_attached_in_the_same_second_are_chosen_by_the_first_listed_by_both_callers() {
        let snap = snapshot();
        let sessions = [session("$2", 300), session("$4", 300), session("$1", 100)];
        assert_eq!(last_session_pane(&sessions, &snap).as_deref(), Some("%211"));
        let sessions = [session("$4", 300), session("$2", 300)];
        assert_eq!(last_session_pane(&sessions, &snap).as_deref(), Some("%411"));
        let never = || -> Result<String, String> { panic!("a session exists") };
        assert_eq!(
            pick_last(&[session("$2", 300), session("$4", 300)], never),
            Ok(GoTarget::Session("$2".into()))
        );
        assert_eq!(
            pick_last(&[session("$4", 300), session("$2", 300)], never),
            Ok(GoTarget::Session("$4".into()))
        );
    }

    #[test]
    fn the_landings_are_the_actions_that_would_switch_a_client() {
        use actions::Action;
        let landing = [
            Action::Jump {
                session: "$1".into(),
                window: "@1".into(),
                pane: "%1".into(),
            },
            Action::SwitchWorkspace {
                session: "$1".into(),
            },
            Action::OpenProject { path: "~/a".into() },
            Action::OpenWorktree {
                workspace: "w1".into(),
            },
            Action::NewWorktree {
                workspace: "w1".into(),
                branch: "b".into(),
            },
            Action::NextUp { own: None },
        ];
        for action in &landing {
            assert!(lands(action), "{action:?}");
        }
        let stays = [
            Action::KillPane { pane: "%1".into() },
            Action::RenameWorkspace {
                session: "$1".into(),
                name: "x".into(),
            },
            Action::KillWorkspace {
                session: "$1".into(),
            },
        ];
        for action in &stays {
            assert!(!lands(action), "{action:?}");
        }
    }
    /// The watchdog writes the screen's restore bytes only when the terminal is there and the screen is not lent.
    #[test]
    fn the_watchdog_writes_only_to_a_terminal_that_is_there_and_not_lent() {
        assert!(puts_away(false, true), "free and there");
        assert!(!puts_away(true, true), "lent");
        assert!(!puts_away(false, false), "gone");
        assert!(!puts_away(true, false), "lent and gone");
    }

    /// The watchdog's decision: a gone terminal ends the process at once, a gone supervisor or a passed on
    /// signal give the loop its grace, and with all fine nothing is done.
    #[test]
    fn the_watchdog_ends_for_a_gone_terminal_a_gone_supervisor_and_a_passed_on_signal() {
        assert_eq!(judge(500, 500, true, false), Watch::Fine);
        assert_eq!(judge(500, 1, true, false), Watch::EndSoon, "orphaned");
        assert_eq!(judge(500, 500, true, true), Watch::EndSoon, "signalled");
        assert_eq!(judge(500, 500, false, false), Watch::EndNow, "terminal");
        assert_eq!(judge(500, 1, false, true), Watch::EndNow, "terminal first");
        // A supervisor that died before the child first looked leaves both pids at 1: still an orphan.
        assert_eq!(judge(1, 1, true, false), Watch::EndSoon, "started under 1");
        assert!(gone_from(1, 1));
        assert!(!gone_from(500, 500));
    }

    // Only the BSDs and macOS have `revoke(2)`; neither glibc nor musl does.
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    unsafe extern "C" {
        /// `revoke(2)`: every open descriptor of the device at `path` is hung up, as a closed window does.
        fn revoke(path: *const libc::c_char) -> libc::c_int;
    }

    /// Keeps `fds` from the processes other tests start meanwhile: `openpty` makes them inheritable, and a stand-in
    /// that a parallel test runs would hold the master open until it ended, so a master that was closed would
    /// not hang the slave up yet.
    fn close_on_exec(fds: &[libc::c_int]) {
        for fd in fds {
            // SAFETY: `fcntl` of descriptors this test made and still holds.
            unsafe { libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }

    /// A terminal is there until it is revoked, which is how a hung-up pane's tty is left to the process on it
    /// (`lsof` shows `(revoked)`); a descriptor that is no terminal is none either.
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    #[test]
    fn a_revoked_terminal_is_not_there_any_more() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes the two descriptors; the nulls are its defaults.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "no pseudo terminal");
        close_on_exec(&[master, slave]);
        assert!(terminal_is_there(slave), "a live terminal is there");
        // SAFETY: `ttyname` of the descriptor just made, copied at once; `revoke` of that device.
        unsafe {
            let device = std::ffi::CStr::from_ptr(libc::ttyname(slave)).to_owned();
            assert_eq!(revoke(device.as_ptr()), 0, "revoke failed");
        }
        assert!(!terminal_is_there(slave), "a revoked one is gone");
        // SAFETY: closing the two descriptors made here.
        unsafe {
            libc::close(slave);
            libc::close(master);
        }
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(!terminal_is_there(std::os::fd::AsRawFd::as_raw_fd(&null)));
    }

    /// A terminal whose master closed with no revoke is gone too: on macOS `isatty` still says yes of it, and
    /// its reads come back with nothing for ever, so only the hangup it polls with tells. One with input
    /// waiting is not gone.
    #[test]
    fn a_terminal_whose_master_closed_is_not_there_any_more() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes the two descriptors; the nulls are its defaults.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "no pseudo terminal");
        close_on_exec(&[master, slave]);
        assert!(terminal_is_there(slave), "a live terminal is there");
        // Input waiting is not a hangup.
        // SAFETY: `write` of a buffer that is alive, to the master just made.
        unsafe { libc::write(master, b"hi\n".as_ptr().cast(), 3) };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            terminal_is_there(slave),
            "input is waiting, the master is open"
        );
        // SAFETY: closing the master made here.
        unsafe { libc::close(master) };
        // On Linux the pty driver hangs the slave up as the master closes, so `isatty` is 0 there already.
        #[cfg(target_os = "macos")]
        assert!(
            // SAFETY: `isatty` of the slave, which is still open.
            unsafe { libc::isatty(slave) } == 1,
            "isatty alone still says yes"
        );
        // A process another test starts meanwhile holds a copy of the master for the moments between its fork and
        // its exec, so the hangup is waited for, and not asserted at one instant.
        let until = Instant::now() + Duration::from_secs(20);
        while terminal_is_there(slave) {
            assert!(
                Instant::now() < until,
                "a master that closed is not gone after 20 s; processes of the tests:\n{}",
                String::from_utf8_lossy(
                    &std::process::Command::new("/bin/ps")
                        .args(["-Ao", "pid=,ppid=,stat=,args="])
                        .output()
                        .map(|out| out.stdout)
                        .unwrap_or_default()
                )
                .lines()
                .filter(|line| line.contains("taw"))
                .collect::<Vec<_>>()
                .join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: closing the slave made here.
        unsafe { libc::close(slave) };
    }

    /// A feed over `tmux` with no machines (no connection is ever made) and a record of its own.
    pub(super) fn feed_over<'a>(tmux: &'a Tmux, cfg: &'a Config, paths: &'a Paths) -> Feed<'a> {
        Feed {
            tmux,
            cfg,
            paths,
            git: data::GitWorker::new(GIT_TTL),
            hub: Machines::start(conn_ctx(paths, ""), Vec::new()),
            machines: Vec::new(),
            server: String::new(),
            ssh: Ssh {
                bin: "/nonexistent/ssh".into(),
            },
            local: Local::default(),
            lefts: Vec::new(),
            walk_said: None,
            toasts: toasts::Toaster::new(paths, Path::new("/nonexistent/taw")),
            remote_preview: remote_preview::RemotePreview::default(),
            remote: remote_actions::Rows::default(),
            pasting: crate::tui::pasting::Pasting::default(),
            record: Record::write(paths),
            own_pane: None,
        }
    }

    /// With `[theme] auto_switch` on, a tick of the app inside tmux asks for the theme of the client that shows its
    /// own pane, by the pane (one `display-message`), and the screen it shows carries the answer, so that the app
    /// draws the theme of that mode; outside tmux (no pane) it asks nothing and keeps `name`.
    #[test]
    fn a_tick_of_the_app_follows_the_theme_of_the_client_showing_its_pane() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg: Config = toml::from_str(
            "[theme]\nname = \"nord\"\nauto_switch = true\ndark_name = \"dracula\"\nlight_name = \"solarized\"\n",
        )
        .unwrap();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.server = "4211 1790000000".to_string();
        let mut shown = "nord".to_string();

        // Outside tmux: nothing is asked and the name stays.
        feed.tick(&tmux, &mut app);
        super::super::follow_theme(&cfg, &paths, Path::new("taw"), &mut app, &mut shown);
        assert!(
            calls(&log)
                .iter()
                .all(|call| !call.iter().any(|word| word.contains("client_theme"))),
            "{:?}",
            calls(&log)
        );
        assert_eq!(shown, "nord");

        // In a pane: one question about it, and the dark name is drawn.
        feed.own_pane = Some("%5".to_string());
        feed.tick(&tmux, &mut app);
        super::super::follow_theme(&cfg, &paths, Path::new("taw"), &mut app, &mut shown);
        let asked: Vec<_> = calls(&log)
            .into_iter()
            .filter(|call| call.iter().any(|word| word == "#{client_theme}"))
            .collect();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(
            asked[0],
            ["display-message", "-p", "-t", "%5", "#{client_theme}"]
        );
        assert_eq!(app.snap.client_theme.as_deref(), Some("dark"));
        assert_eq!(shown, "dracula");
        assert_eq!(
            app.theme,
            theme::resolve("dracula", &cfg.theme.custom).theme
        );
    }

    /// What the machines announce does not pile up: a tick takes the toasts and the attach ends and drops them.
    #[test]
    fn a_tick_takes_what_the_machines_announced() {
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.hub.announce(
            0,
            crate::machine::wire::Toast::default(),
            crate::machine::wire::Left::default(),
        );
        assert_eq!(feed.hub.waiting(), (1, 1), "the announcements are there");

        feed.tick(&tmux, &mut app);

        assert_eq!(feed.hub.waiting(), (0, 0));
    }

    /// The paste-image key does not run on the app's thread, on this Mac's row or a machine's: `perform` returns at
    /// once with the paste under way, a second key says so, and the toast of its end comes when it ends.
    #[test]
    fn the_apps_paste_image_key_returns_at_once_and_its_toast_comes_when_the_paste_ends() {
        use crate::cli::test_support::{Stub, stub_tmux};
        use crate::tui::pasting;
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let (open, gate) = std::sync::mpsc::channel();
        feed.pasting =
            pasting::Pasting::with(pasting::gated(gate, "image pasted into %16 on studio"));
        let action = actions::Action::PasteImage {
            pane: "studio:%16".to_string(),
        };

        let began = Instant::now();
        feed.perform(
            Path::new("/nonexistent/taw"),
            &mut app,
            &mut Unlent,
            &action,
        )
        .unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "perform waited for the paste: {:?}",
            began.elapsed()
        );
        assert_eq!(
            app.live_toast(),
            Some(pasting::started_words("studio:%16").as_str())
        );
        feed.say_pasted(&mut app);
        assert_eq!(
            app.live_toast(),
            Some(pasting::started_words("studio:%16").as_str()),
            "nothing is said of an end that has not come"
        );

        feed.perform(
            Path::new("/nonexistent/taw"),
            &mut app,
            &mut Unlent,
            &action,
        )
        .unwrap();
        assert_eq!(app.live_toast(), Some(pasting::UNDER_WAY));

        open.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.live_toast() != Some("image pasted into %16 on studio") {
            feed.say_pasted(&mut app);
            assert!(
                Instant::now() < deadline,
                "the toast of the end never came: {:?}",
                app.live_toast()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The app has no client, so it asks tmux for none: a tick reads the sessions and nothing of a client,
    /// and showing what was read reads nothing at all.
    #[test]
    fn a_tick_of_the_app_asks_tmux_for_no_client() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        // A server that is up: a tick reads its word only while there is none, and that read is a
        // display-message too (of the server, not of a client).
        feed.server = "4211 1790000000".to_string();

        feed.tick(&tmux, &mut app);

        let asked = calls(&log);
        assert!(
            asked.iter().any(|call| call[0] == "list-sessions"),
            "the tick read the sessions: {asked:?}"
        );
        let verbs: Vec<&str> = asked.iter().map(|call| call[0].as_str()).collect();
        assert!(
            !verbs.contains(&"display-message") && !verbs.contains(&"list-clients"),
            "a client was looked for or asked: {asked:?}"
        );

        let before = asked.len();
        feed.refresh(&tmux, &mut app);
        assert_eq!(
            calls(&log).len(),
            before,
            "showing what was read reads nothing"
        );
    }

    /// A screen that is lent to nobody: the order of its steps is `lend_through`'s own test.
    pub(super) struct Unlent;
    impl Lending for Unlent {
        fn away(&mut self) {}
        fn back(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn drain(&mut self) {}
    }

    /// A landing that went in reads the sessions once when it is back, not twice (going in shows, and
    /// the landing that called it does not show again); one that did not land shows once too.
    #[test]
    fn a_landing_reads_the_list_once() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let reads = || {
            calls(&log)
                .iter()
                .filter(|call| call[0] == "list-sessions")
                .count()
        };

        let jump = actions::Action::SwitchWorkspace {
            session: "$3".into(),
        };
        feed.perform(Path::new("/nonexistent/taw"), &mut app, &mut Unlent, &jump)
            .unwrap();
        // The stand-in's log splits a format that holds a newline into more lines: keep the verbs.
        let verbs: Vec<String> = calls(&log)
            .into_iter()
            .map(|call| call[0].clone())
            .filter(|verb| verb.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
            .collect();
        assert!(
            verbs.iter().any(|verb| verb == "attach-session"),
            "the landing went in: {verbs:?}"
        );
        assert_eq!(reads(), 1, "{verbs:?}");

        // A landing that cannot be made says why, and shows once.
        let before = reads();
        let nowhere = actions::Action::OpenProject {
            path: "/nonexistent-dir-for-taw".into(),
        };
        feed.perform(
            Path::new("/nonexistent/taw"),
            &mut app,
            &mut Unlent,
            &nowhere,
        )
        .unwrap();
        assert_eq!(reads(), before + 1);
    }

    /// The first frame is drawn from the read the opening already made: the app reads this Mac's sessions
    /// once for its rows and once for the session attached most lately, and showing the first frame reads
    /// nothing more.
    #[test]
    fn the_first_frame_is_drawn_from_the_read_already_made() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = || theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let reads = || {
            calls(&log)
                .iter()
                .filter(|call| call[0] == "list-sessions")
                .count()
        };

        // What one read of this Mac costs.
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.read_local();
        let one_read = reads();
        assert!(one_read >= 1, "a read lists the sessions");

        let before = reads();
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let entered = std::cell::Cell::new(false);
        open_app(&mut feed, &cfg, theme(), String::new(), None, || {
            entered.set(true);
            Ok(())
        })
        .unwrap();
        assert!(entered.get(), "the screen was entered");

        // One read for the rows, plus the one list of sessions that names the last attached; none for the
        // first frame.
        let verbs: Vec<String> = calls(&log).iter().map(|call| call[0].clone()).collect();
        assert_eq!(reads() - before, one_read + 1, "the calls were {verbs:?}");
    }

    /// A screen that, once it is taken back, has the key back's line written `LATE` later, as `taw ui` writes
    /// it when the client is sent back, and notes when it was taken back and when whatever follows ended.
    struct LineComesAfterBack {
        paths: Paths,
        holder: String,
        writer: Option<std::thread::JoinHandle<()>>,
        back_at: Option<Instant>,
        after_at: Option<Instant>,
    }
    impl LineComesAfterBack {
        const LATE: Duration = Duration::from_millis(150);
    }
    impl Lending for LineComesAfterBack {
        fn away(&mut self) {}
        fn back(&mut self) -> anyhow::Result<()> {
            self.back_at = Some(Instant::now());
            let (paths, holder) = (self.paths.clone(), self.holder.clone());
            self.writer = Some(std::thread::spawn(move || {
                std::thread::sleep(Self::LATE);
                files::append_event(&paths, &left(&holder, "%77"));
            }));
            Ok(())
        }
        fn drain(&mut self) {
            self.after_at = Some(Instant::now());
            if let Some(writer) = self.writer.take() {
                writer.join().unwrap();
            }
        }
    }

    /// The wait for the key back's line comes after the screen is back: what follows the lent program
    /// (the left line's wait) ends no sooner than the line, which is written only once the screen is taken
    /// back. A wait that ran before the screen came back would be over by then and the screen would be
    /// back at once (cooked terminal: keys echoed, Ctrl-C passed on).
    #[test]
    fn going_in_waits_for_the_left_line_after_the_screen_is_back() {
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let mut screen = LineComesAfterBack {
            paths: paths.clone(),
            holder: feed.record.holder().to_string(),
            writer: None,
            back_at: None,
            after_at: None,
        };

        feed.go_in(
            &mut app,
            &mut screen,
            actions::Landing {
                target: "$3".into(),
                pane: Some("%1".into()),
                note: None,
            },
        )
        .unwrap();

        let back = screen.back_at.expect("the screen was taken back");
        let after = screen.after_at.expect("what follows ran");
        assert!(
            after.duration_since(back) >= LineComesAfterBack::LATE,
            "the wait ended {:?} after the screen came back, before the line was written",
            after.duration_since(back)
        );
    }

    /// A screen that, on its first return, has the key back's line written with a `go` on to another stop, as the
    /// key in the client does when it walks on.
    struct WalksOn {
        paths: Paths,
        holder: String,
        go: String,
        sent: bool,
    }
    impl Lending for WalksOn {
        fn away(&mut self) {}
        fn back(&mut self) -> anyhow::Result<()> {
            if !std::mem::replace(&mut self.sent, true) {
                files::append_event(
                    &self.paths,
                    &Down::Left(crate::machine::wire::Left {
                        holder: self.holder.clone(),
                        pane: "%1".into(),
                        go: Some(self.go.clone()),
                    }),
                );
            }
            Ok(())
        }
        fn drain(&mut self) {}
    }

    /// `--go %7` (a go on this Mac) follows the walk the key in its client asks for, as every other way into a
    /// client does: the first attach ends with a line that names `%9`, and `%9` is gone into once, after it.
    #[test]
    fn a_go_on_this_mac_follows_the_walk_the_client_asks_for() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let mut screen = WalksOn {
            paths: paths.clone(),
            holder: feed.record.holder().to_string(),
            go: "%9".into(),
            sent: false,
        };
        feed.go_first(
            &mut app,
            &mut screen,
            &GoTarget::Pane("%7".into()),
            Duration::ZERO,
        )
        .unwrap();
        let all = calls(&log);
        let count = |verb: &str| all.iter().filter(|c| c[0] == verb).count();
        assert_eq!(count("attach-session"), 2, "{all:?}");
        let selected = |pane: &str| {
            all.iter()
                .filter(|c| c[0] == "select-pane" && c.contains(&pane.to_string()))
                .count()
        };
        assert_eq!((selected("%7"), selected("%9")), (1, 1), "{all:?}");
    }

    fn sessions_at(attached: &[(&str, u64)]) -> Vec<SessionInfo> {
        attached
            .iter()
            .map(|(id, at)| SessionInfo {
                session_id: (*id).into(),
                session_name: String::new(),
                session_path: String::new(),
                session_attached: 0,
                session_windows: 1,
                session_group_size: 0,
                active_window_id: String::new(),
                session_last_attached: *at,
            })
            .collect()
    }

    /// `last` is the session attached most lately; among equals (never attached: all 0) the first listed; with
    /// no session, the one `make` made, by name; and a failure to make one is its words.
    #[test]
    fn last_is_the_session_attached_most_lately_or_the_one_made() {
        let never = || -> Result<String, String> { panic!("a session exists: none is made") };
        assert_eq!(
            pick_last(&sessions_at(&[("$1", 5), ("$2", 9), ("$3", 7)]), never),
            Ok(GoTarget::Session("$2".into()))
        );
        assert_eq!(
            pick_last(&sessions_at(&[("$4", 0), ("$2", 0)]), never),
            Ok(GoTarget::Session("$4".into()))
        );
        assert_eq!(
            pick_last(&[], || Ok("proj".into())),
            Ok(GoTarget::Session("=proj".into()))
        );
        assert_eq!(
            pick_last(&[], || Err("no directory".into())),
            Err("no directory".to_string())
        );
    }

    /// A listing of the sessions that fails for any reason but "no server running" is a refusal with its words
    /// and makes no session; "no server running" makes one, as an empty list does.
    #[test]
    fn a_failed_listing_of_the_sessions_makes_no_session_and_no_server_makes_one() {
        let never = || -> Result<String, String> { panic!("the listing failed: none is made") };
        let failed = anyhow::anyhow!("tmux list-windows timed out after 5s");
        assert_eq!(
            last_of(Err(failed), never),
            Err("tmux list-windows timed out after 5s".to_string())
        );
        let no_server = anyhow::anyhow!("no server running on /tmp/tmux-501/default");
        assert_eq!(
            last_of(Err(no_server), || Ok("proj".into())),
            Ok(GoTarget::Session("=proj".into()))
        );
        assert_eq!(
            last_of(Ok(sessions_at(&[("$1", 5)])), never),
            Ok(GoTarget::Session("$1".into()))
        );
    }

    /// A failure to go to `last` and a `[machines]` doubt are both said, the failure first; with one only, that
    /// one; with none, nothing.
    #[test]
    fn the_first_words_keep_the_failure_and_the_machines_doubt() {
        let words = |a: Option<&str>, b: Option<&str>| {
            first_words(a.map(str::to_string), b.map(str::to_string))
        };
        assert_eq!(
            words(Some("no directory"), Some("[machines] is not a table")).as_deref(),
            Some("no directory; [machines] is not a table")
        );
        assert_eq!(
            words(Some("no directory"), None).as_deref(),
            Some("no directory")
        );
        assert_eq!(words(None, Some("bad table")).as_deref(), Some("bad table"));
        assert_eq!(words(None, None), None);
    }

    /// Back from an attach the app says the popup's note for the landing, and with a failed attach the note
    /// goes in front of the reason as the popup's does.
    #[test]
    fn coming_back_the_app_says_the_note_of_the_landing() {
        assert_eq!(back_words(None, None), None);
        assert_eq!(
            back_words(Some("created worktree x"), None).as_deref(),
            Some("created worktree x")
        );
        assert_eq!(
            back_words(None, Some("tmux ended the attach with exit status: 1")).as_deref(),
            Some("tmux ended the attach with exit status: 1")
        );
        assert_eq!(
            back_words(Some("created worktree x"), Some("that session is gone")).as_deref(),
            Some("created worktree x (that session is gone)")
        );

        // Through go_in: the toast is the landing's note.
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.go_in(
            &mut app,
            &mut Unlent,
            actions::Landing {
                target: "$3".into(),
                pane: None,
                note: Some("created worktree x".into()),
            },
        )
        .unwrap();
        assert_eq!(
            app.toast.as_ref().map(|(words, _)| words.as_str()),
            Some("created worktree x")
        );
    }

    /// A session that went between the landing and the attach: Back, which came for its pane, forgets the
    /// pane (the next Back ends the app); a landing that is not Back's leaves it.
    #[test]
    fn a_session_that_went_before_the_attach_is_forgotten_by_back() {
        use crate::cli::test_support::{Stub, stub_tmux};
        use crate::tui::app::Mode;
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(
            dir.path(),
            &Stub {
                session_gone: true,
                ..Stub::default()
            },
        );
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let landing = |pane: &str| actions::Landing {
            target: "$3".into(),
            pane: Some(pane.into()),
            note: None,
        };

        // Another pane's landing (a menu jump) does not touch what Back would go in at.
        app.mode = Mode::App {
            came_from: Some("%121".into()),
        };
        feed.go_in(&mut app, &mut Unlent, landing("%999")).unwrap();
        assert_eq!(app.came_from(), Some("%121"));

        // Back's own landing: the pane went with its session.
        feed.go_in(&mut app, &mut Unlent, landing("%121")).unwrap();
        assert_eq!(
            app.toast.as_ref().map(|(words, _)| words.as_str()),
            Some("that session is gone")
        );
        assert_eq!(app.came_from(), None);
    }

    /// An attach that names no pane to come back to still closes a filter typed before going in.
    #[test]
    fn a_landing_with_no_pane_still_closes_the_filter() {
        use crate::cli::test_support::{Stub, stub_tmux};
        use crate::tui::input::Input;
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        app.filter = Some(Input::new("one"));
        assert!(app.filter.is_some());

        feed.go_in(
            &mut app,
            &mut Unlent,
            actions::Landing {
                target: "$3".into(),
                pane: None,
                note: None,
            },
        )
        .unwrap();

        assert!(app.filter.is_none(), "the filter was closed");
        assert!(app.is_app());
    }

    fn left(holder: &str, pane: &str) -> Down {
        Down::Left(crate::machine::wire::Left {
            holder: holder.into(),
            pane: pane.into(),
            go: None,
        })
    }

    pub(super) fn paths_in(dir: &std::path::Path) -> Paths {
        Paths::rooted_at(dir.join("config"), dir.join("state"))
    }

    /// The line the key back writes is the one addressed to this app's holder, and only a line written
    /// after the tail started is news.
    #[test]
    fn the_left_line_is_the_one_for_this_app_and_written_after_the_tail_began() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        files::append_event(&paths, &left("mac-1", "%old"));
        let mut tail = files::EventTail::at_end(&paths);
        files::append_event(&paths, &left("mac-2", "%theirs"));
        files::append_event(&paths, &left("mac-1", "%mine"));
        assert_eq!(
            left_line(&mut tail, "mac-1", Duration::ZERO)
                .map(|left| left.pane)
                .as_deref(),
            Some("%mine")
        );
        // Read: the next look has nothing, and with no wait it does not wait.
        assert_eq!(left_line(&mut tail, "mac-1", Duration::ZERO), None);
    }

    /// A line that comes a little after the client ended is waited for and found.
    #[test]
    fn a_left_line_that_comes_late_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let mut tail = files::EventTail::at_end(&paths);
        let later = paths.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            files::append_event(&later, &left("mac-1", "%late"));
        });
        let found = left_line(&mut tail, "mac-1", Duration::from_secs(20));
        writer.join().unwrap();
        assert_eq!(found.map(|left| left.pane).as_deref(), Some("%late"));
    }

    /// The record says which client the app waits for while it attaches, and says none after.
    #[test]
    fn the_record_names_the_client_while_it_lives_and_no_one_after() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let mut record = Record::write(&paths);
        let pid = std::process::id();
        record.attached(4242, "/dev/ttys009", "");
        let read = files::read_app_record(&paths, pid).unwrap();
        assert_eq!(
            (read.client_pid, read.client_tty.as_str()),
            (4242, "/dev/ttys009")
        );
        assert_eq!(read.inside, "", "a client of this Mac is inside no machine");
        assert_eq!(read.holder, record.holder());
        record.detached();
        let read = files::read_app_record(&paths, pid).unwrap();
        assert_eq!((read.client_pid, read.client_tty.as_str()), (0, ""));
        record.remove();
        assert!(files::read_app_record(&paths, pid).is_none());
    }

    /// The record names the machine the client is inside for the time of the attach, and clears it after.
    #[test]
    fn the_record_names_the_machine_the_client_is_inside_and_clears_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let mut record = Record::write(&paths);
        let pid = std::process::id();
        assert_eq!(files::read_app_record(&paths, pid).unwrap().inside, "");
        record.attached(4242, "/dev/ttys009", "studio");
        let read = files::read_app_record(&paths, pid).unwrap();
        assert_eq!(read.inside, "studio");
        assert_eq!(read.client_pid, 4242, "the client is named with it");
        record.detached();
        let read = files::read_app_record(&paths, pid).unwrap();
        assert_eq!((read.inside.as_str(), read.client_pid), ("", 0));
    }

    /// A line that arrives while the client still runs is kept, and only the ones addressed to this app: the
    /// tick and the wait both take what is announced, and the toasts are dropped as before.
    #[test]
    fn a_left_line_for_this_app_is_kept_and_another_apps_is_dropped() {
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let mine = feed.record.holder().to_string();
        let line = |holder: &str, pane: &str| crate::machine::wire::Left {
            holder: holder.into(),
            pane: pane.into(),
            go: None,
        };
        feed.hub
            .announce(0, Default::default(), line("mac-9999", "%1"));
        feed.hub.announce(1, Default::default(), line(&mine, "%2"));
        feed.take_announced();
        assert_eq!(feed.hub.waiting(), (0, 0), "everything was taken");
        assert_eq!(
            feed.lefts,
            vec![(1, line(&mine, "%2"))],
            "only mine is kept"
        );

        // The cap: the newest KEPT_LEFTS stay.
        for n in 0..KEPT_LEFTS + 4 {
            feed.hub
                .announce(0, Default::default(), line(&mine, &format!("%{}", 10 + n)));
        }
        feed.take_announced();
        assert_eq!(feed.lefts.len(), KEPT_LEFTS);
        assert_eq!(
            feed.lefts.last().map(|(_, left)| left.pane.clone()),
            Some(format!("%{}", 10 + KEPT_LEFTS + 3))
        );

        // The wait takes them too, while the client runs.
        feed.lefts.clear();
        feed.hub.announce(0, Default::default(), line(&mine, "%3"));
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .unwrap();
        let _guard = KillOnDrop(child.id() as libc::pid_t);
        let pid = child.id();
        // The wait looks every `WAIT_LOOK`, and the client runs much longer than that (long enough for a test
        // thread that a loaded machine did not run for a second): the line is taken, and kept, while it runs.
        let _ = feed.wait_for(pid, false);
        let _ = child.try_wait();
        assert_eq!(feed.lefts, vec![(0, line(&mine, "%3"))]);
    }

    /// A record dropped without the call (a panic, an error return) leaves its file, which the supervisor
    /// reads after a crash for the client the app had; only the call takes it away.
    #[test]
    fn a_record_is_removed_by_the_call_and_not_by_being_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let pid = std::process::id();
        let mut record = Record::write(&paths);
        record.attached(4242, "/dev/ttys009", "");
        drop(record);
        let left = files::read_app_record(&paths, pid).expect("the file is still there");
        assert_eq!(left.client_pid, 4242, "with what the supervisor reads");

        let record = Record::write(&paths);
        record.remove();
        assert!(files::read_app_record(&paths, pid).is_none());
    }

    /// Ends the pid it holds, if it is still a child of this process that has not been reaped, when dropped: a
    /// test that starts a child owns it. A pid that the code under test reaped is left alone, since it may have
    /// been given to another process since.
    pub(super) struct KillOnDrop(pub(super) libc::pid_t);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if self.0 <= 0 {
                return;
            }
            let mut status: libc::c_int = 0;
            // SAFETY: `waitpid` with WNOHANG of a pid this test started: 0 says it is still our child and not
            // yet reaped, so the kill goes to that child and not to a pid that was given away; the blocking
            // wait after the kill is for that same child.
            unsafe {
                if libc::waitpid(self.0, &mut status, libc::WNOHANG) == 0 {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, &mut status, 0);
                }
            }
        }
    }

    /// Waits (20 s) until the client `pid`, a child of this process, no longer runs: `waitpid` answers other than
    /// 0 (it was reaped by the wait under test, or is reaped by this poll: either is "ended"). Panics with what
    /// `ps` says of it.
    pub(super) fn wait_until_not_running(pid: libc::pid_t, what: &str) {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: `waitpid` with WNOHANG, with a status of its own to write to.
            if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != 0 {
                return;
            }
            assert!(
                Instant::now() < until,
                "{what}: the client {pid} still runs: {}",
                String::from_utf8_lossy(
                    &std::process::Command::new("/bin/ps")
                        .args(["-o", "pid=,ppid=,stat=,args=", "-p", &pid.to_string()])
                        .output()
                        .map(|out| out.stdout)
                        .unwrap_or_default()
                )
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A wait for a client ends it when the app's supervisor is gone: the client is told to end and is gone (reaped
    /// by the wait itself: `waitpid` answers -1, no child of ours, right after it returns), and the wait gives an
    /// error, well before the client would have ended by itself.
    #[test]
    fn waiting_for_a_client_ends_it_when_the_supervisor_is_gone() {
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let mut feed = feed_over(&tmux, &cfg, &paths);
        // This process is never its own parent.
        feed.record.record.supervisor = std::process::id();
        assert!(
            feed.record.orphaned(),
            "the premise: the supervisor is gone"
        );
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .expect("sleep starts");
        let guard = KillOnDrop(child.id() as libc::pid_t);
        let started = Instant::now();
        let ended = feed.wait_for(child.id(), false);
        let took = started.elapsed();
        assert!(ended.is_err(), "the wait ended with {ended:?}");
        assert!(took < Duration::from_secs(3), "the wait took {took:?}");
        assert_eq!(
            reap_answer(guard.0),
            -1,
            "the wait reaped the client (a poll that reaps it is not the wait's)"
        );
        let _ = child.try_wait();
    }

    /// What `waitpid(pid, WNOHANG)` answers now: the pid when it reaps it, 0 for a child that still runs, and -1
    /// for a pid that is not a child (already reaped).
    fn reap_answer(pid: libc::pid_t) -> libc::pid_t {
        let mut status: libc::c_int = 0;
        // SAFETY: `waitpid` with WNOHANG, with a status of its own to write to.
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) }
    }

    /// A shell that ignores `SIGHUP` and waits for a line that never comes, owned by the test. It says `ready`
    /// once the trap is set, so the test does not signal it before.
    fn stubborn_client(dir: &std::path::Path) -> (std::process::Child, KillOnDrop) {
        use std::process::Stdio;
        let ready = dir.join("ready");
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "trap '' HUP; echo ready > '{}'; read line",
                ready.display()
            ))
            .stdin(Stdio::piped())
            .spawn()
            .expect("sh starts");
        let guard = KillOnDrop(child.id() as libc::pid_t);
        let until = Instant::now() + Duration::from_secs(20);
        while std::fs::read_to_string(&ready).map_or(true, |text| !text.ends_with('\n')) {
            assert!(Instant::now() < until, "the shell never said it was ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        (child, guard)
    }

    /// A client that ends on `SIGHUP` is reaped by `end_the_client`, which says so, local or remote; and the wait
    /// loop is what reaps it (`waitpid` then answers -1).
    #[test]
    fn ending_a_client_that_ends_on_hangup_says_it_was_reaped() {
        for remote in [false, true] {
            let mut child = std::process::Command::new("/bin/sleep")
                .arg("5")
                .spawn()
                .expect("sleep starts");
            let guard = KillOnDrop(child.id() as libc::pid_t);
            assert_eq!(
                end_the_client(guard.0, libc::SIGHUP, remote),
                End::Gone,
                "remote={remote}: a client that ends on SIGHUP is reaped"
            );
            assert_eq!(
                reap_answer(guard.0),
                -1,
                "remote={remote}: reaped by the call"
            );
            let _ = child.try_wait();
        }
    }

    /// An ssh that ignores `SIGHUP` is killed after the wait and reaped; a tmux client that does is left
    /// running (and the answer says it was not reaped).
    #[test]
    fn an_ssh_that_ignores_hangup_is_killed_and_a_tmux_client_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let (_remote_child, remote_guard) = stubborn_client(dir.path());
        let started = Instant::now();
        assert_eq!(
            end_the_client(remote_guard.0, libc::SIGHUP, true),
            End::Killed,
            "an ssh that ignores SIGHUP is killed"
        );
        assert!(
            started.elapsed() >= CLIENT_END_WAIT,
            "the kill comes after the polite wait, not instead of it"
        );
        assert_eq!(reap_answer(remote_guard.0), -1, "it is gone");

        let other = tempfile::tempdir().unwrap();
        let (_local_child, local_guard) = stubborn_client(other.path());
        assert_eq!(
            end_the_client(local_guard.0, libc::SIGHUP, false),
            End::Running,
            "a tmux client that ignores SIGHUP is not reaped"
        );
        assert_eq!(
            reap_answer(local_guard.0),
            0,
            "and it is left running (the positive half: it is our child, still there)"
        );
    }

    /// An attach with a machine runs an ssh; one with none, this Mac's tmux client. The restore bytes are only
    /// for an ssh that is gone.
    #[test]
    fn only_an_attach_with_a_machine_is_remote_and_puts_the_terminal_away_once_gone() {
        assert!(is_remote("studio"));
        assert!(!is_remote(""));
        assert!(puts_away_after(true, End::Gone));
        assert!(
            puts_away_after(true, End::Killed),
            "a killed ssh runs no more"
        );
        assert!(
            !puts_away_after(true, End::Running),
            "never over a live ssh"
        );
        for end in [End::Gone, End::Killed, End::Running] {
            assert!(
                !puts_away_after(false, end),
                "{end:?}: a tmux client's server does it"
            );
            assert_eq!(
                puts_modes_back_after(true, end),
                end == End::Killed,
                "{end:?}: only a killed ssh left the modes it set"
            );
            assert!(!puts_modes_back_after(false, end), "{end:?}: local");
        }
    }

    /// The cooked modes are set again on a terminal an ssh left raw: ICANON, ECHO, OPOST and ICRNL, which a shell
    /// needs, come back. The terminal is a pty of the test's own; the app's call passes standard input.
    #[test]
    fn the_modes_an_ssh_left_raw_are_put_back_cooked() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes the two descriptors; the nulls are its defaults.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "no pseudo terminal");
        close_on_exec(&[master, slave]);
        /// Closes the two descriptors when the test ends, by a failed assert as well.
        struct Closing([libc::c_int; 2]);
        impl Drop for Closing {
            fn drop(&mut self) {
                for fd in self.0 {
                    // SAFETY: a descriptor `openpty` made for this test, closed once.
                    unsafe { libc::close(fd) };
                }
            }
        }
        let _closing = Closing([master, slave]);
        let cooked_flags = |modes: &libc::termios| {
            (
                modes.c_lflag & (libc::ICANON | libc::ECHO),
                modes.c_oflag & libc::OPOST,
                modes.c_iflag & libc::ICRNL,
            )
        };
        // SAFETY: `tcgetattr`, `cfmakeraw` and `tcsetattr` on the slave just made and a `termios` of ours.
        let (saved, raw) = unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave, &mut saved), 0, "no modes to read");
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &raw), 0);
            (saved, raw)
        };
        let read_back = || {
            // SAFETY: as above.
            unsafe {
                let mut modes: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(slave, &mut modes), 0);
                modes
            }
        };
        // The positive half: the terminal reads raw before the call, and the saved modes were cooked.
        assert_eq!(
            cooked_flags(&read_back()),
            (0, 0, 0),
            "the terminal is not raw before the call"
        );
        assert_eq!(cooked_flags(&raw), (0, 0, 0));
        assert_eq!(
            cooked_flags(&saved),
            (libc::ICANON | libc::ECHO, libc::OPOST, libc::ICRNL),
            "the saved modes are not cooked"
        );
        put_back_modes(slave, &saved);
        assert_eq!(
            cooked_flags(&read_back()),
            (libc::ICANON | libc::ECHO, libc::OPOST, libc::ICRNL),
            "the cooked modes are not back"
        );
    }

    /// A feed whose supervisor is gone lends the screen and runs the attach, and never takes the screen back
    /// (nor waits for the left line, nor drops what was typed): the client was told to end and is gone.
    #[test]
    fn going_in_with_the_supervisor_gone_never_takes_the_screen_back() {
        struct Logging(std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>);
        impl Lending for Logging {
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
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("attach.pid");
        let script = dir.path().join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = -u ] && shift\ncase \"$1\" in\n  has-session) exit 0 ;;\n  attach-session) echo $$ > '{}'; exec /bin/sleep 30 ;;\nesac\nexit 0\n",
                pid_file.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);
        let tmux = Tmux {
            bin: script,
            socket: None,
        };
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        let mut app = App::new(&cfg, theme, String::new(), Snapshot::default());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        feed.record.record.supervisor = std::process::id();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let started = Instant::now();
        let went = feed.go_in(
            &mut app,
            &mut Logging(log.clone()),
            actions::Landing {
                target: "$3".into(),
                pane: Some("%1".into()),
                note: None,
            },
        );
        let took = started.elapsed();
        // The client is told to end at the wait's first look, which can come before its shell has written its
        // pid (it is then gone before it ran a line): when it did write it, it is ended and reaped by now, and
        // is killed here if the wait did not end it. The lending is the positive half: `screen_away` is in the
        // log, so the attach was run through `lend_through` (its script is the one that just answered
        // `has-session`), and `go_in` came back well before the client's 30 s.
        let attached: Option<libc::pid_t> = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        let _guard = KillOnDrop(attached.unwrap_or(0));
        assert!(went.is_ok(), "go_in gave {went:?}");
        assert_eq!(*log.borrow(), ["screen_away"], "what lending did");
        assert!(took < Duration::from_secs(3), "going in took {took:?}");
        if let Some(attached) = attached {
            wait_until_not_running(attached, "the client was not ended");
        }
    }

    // ------------------------------------------------------------------ the walk said to the machines

    fn stops(of: &[&str]) -> Vec<String> {
        of.iter().map(|id| id.to_string()).collect()
    }

    /// The walk is said when it changed, written again for this Mac's file when it is old, and otherwise left.
    #[test]
    fn the_walk_is_due_on_a_change_and_not_on_every_tick() {
        let now = Instant::now();
        let said = (stops(&["%7", "studio:%16"]), now);
        let same = stops(&["%7", "studio:%16"]);
        assert_eq!(
            walk_due(None, &same, now),
            WalkDue::Send,
            "nothing said yet"
        );
        assert_eq!(
            walk_due(Some(&said), &same, now + Duration::from_millis(500)),
            WalkDue::Nothing,
            "a tick later and unchanged"
        );
        assert_eq!(
            walk_due(
                Some(&said),
                &stops(&["%7"]),
                now + Duration::from_millis(500)
            ),
            WalkDue::Send,
            "changed"
        );
        assert_eq!(
            walk_due(Some(&said), &same, now + WALK_FILE_EVERY),
            WalkDue::File,
            "unchanged but the file is old: written again, not said again"
        );
    }

    fn two_machines() -> Vec<Machine> {
        ["studio", "pi"]
            .iter()
            .map(|name| Machine {
                name: name.to_string(),
                ssh: name.to_string(),
                taw: None,
            })
            .collect()
    }

    /// A feed that holds `machines` and starts no connection (a follower never connects before its first poll).
    fn feed_with<'a>(
        tmux: &'a Tmux,
        cfg: &'a Config,
        paths: &'a Paths,
        machines: Vec<Machine>,
    ) -> Feed<'a> {
        let mut feed = feed_over(tmux, cfg, paths);
        feed.hub = Machines::follow(conn_ctx(paths, ""), machines.clone());
        feed.machines = machines;
        feed
    }

    #[test]
    fn the_feed_says_the_walk_to_each_machine_and_writes_this_macs_file_on_a_change_only() {
        use crate::cli::test_support::{Stub, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let mut feed = feed_with(&tmux, &cfg, &paths, two_machines());
        let holder = feed.record.holder().to_string();
        let walk_file = files::walk_path(&paths, &holder).unwrap();
        let mut snap = snapshot();
        let first = walk::stops(&snap);
        assert_eq!(
            first,
            ["%231", "%341"],
            "the sample world's agents that need you"
        );

        let t0 = Instant::now();
        feed.say_walk(&snap, t0);
        for (at, name) in ["studio", "pi"].iter().enumerate() {
            let said = feed.hub.walk_said(at).expect("the walk was said");
            assert_eq!((said.you.as_str(), &said.stops), (*name, &first));
        }
        let file = files::read_walk(&paths, &holder).expect("this Mac's walk file");
        assert_eq!((file.you.as_str(), &file.stops), ("", &first));
        assert_eq!(
            file.holder,
            feed.record.holder(),
            "the app's own holder is in its file"
        );

        // The same walk a tick later: nothing is written. The file is taken away first, so that a write shows.
        std::fs::remove_file(&walk_file).unwrap();
        feed.say_walk(&snap, t0 + Duration::from_millis(500));
        assert!(
            files::read_walk(&paths, &holder).is_none(),
            "an unchanged walk is not written again"
        );

        // The positive halves: an unchanged walk whose file is old is written again, and a change is said and written.
        feed.say_walk(&snap, t0 + WALK_FILE_EVERY);
        assert_eq!(files::read_walk(&paths, &holder).unwrap().stops, first);
        snap.agents.retain(|agent| agent.pane_id != "%231");
        feed.say_walk(&snap, t0 + WALK_FILE_EVERY + Duration::from_millis(500));
        assert_eq!(files::read_walk(&paths, &holder).unwrap().stops, ["%341"]);
        assert_eq!(feed.hub.walk_said(1).unwrap().stops, ["%341"]);
    }

    /// When this Mac's server comes up the connections are made again, and they have heard no walk: the same walk is
    /// said again (to the new hub, and this Mac's file is written again with it). Without a replacement the same
    /// walk is not said again (the positive half is the replacement).
    #[test]
    fn a_walk_is_said_again_to_connections_made_after_the_server_came_up() {
        use crate::cli::test_support::{Stub, stub_tmux};
        for replaced in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            // The stand-in knows its server's word (`#{pid} #{start_time}`), as a running server does.
            let script = dir.path().join("tmux");
            let text = std::fs::read_to_string(&script).unwrap();
            let text = text.replace(
                "      '#{pane_id}') echo '%5' ;;",
                "      '#{pane_id}') echo '%5' ;;\n      '#{pid} #{start_time}') echo '4211 1790000000' ;;",
            );
            std::fs::write(&script, text).unwrap();
            let paths = paths_in(dir.path());
            let cfg = Config::default();
            // Another keeper holds the lock, so the hub made later follows and starts no ssh.
            let _keeper = files::take_keeper(&paths).expect("the keeper lock");
            let mut feed = feed_with(&tmux, &cfg, &paths, two_machines());
            let holder = feed.record.holder().to_string();
            let walk_file = files::walk_path(&paths, &holder).unwrap();
            if !replaced {
                feed.server = "4211 1790000000".into();
            }
            let snap = snapshot();
            let t0 = Instant::now();
            feed.say_walk(&snap, t0);
            assert!(feed.hub.walk_said(0).is_some(), "the walk was said");
            std::fs::remove_file(&walk_file).unwrap();

            feed.follow_the_server(&tmux);
            assert_eq!(
                feed.hub.walk_said(0).is_none(),
                replaced,
                "the new hub has said nothing (replaced {replaced})"
            );
            feed.say_walk(&snap, t0 + Duration::from_millis(500));

            assert_eq!(walk_file.exists(), replaced, "replaced {replaced}");
            assert!(feed.hub.walk_said(0).is_some());
        }
    }

    /// With no `[machines]` the walk costs nothing: no file, and `keep_walk` (which a client's long stay runs every
    /// 150 ms) asks this Mac's tmux nothing. The positive half is the same feed with machines, where it reads.
    #[test]
    fn with_no_machines_the_feed_says_nothing_and_writes_no_walk_file() {
        use crate::cli::test_support::{Stub, calls, stub_tmux};
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let cfg = Config::default();
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let holder = feed.record.holder().to_string();
        let walk_file = files::walk_path(&paths, &holder).unwrap();
        let before = calls(&log).len();
        feed.say_walk(&snapshot(), Instant::now());
        feed.keep_walk();
        assert_eq!(calls(&log).len(), before, "keep_walk asked tmux something");
        assert!(files::read_walk(&paths, &holder).is_none());
        assert!(!walk_file.exists());

        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let mut feed = feed_with(&tmux, &cfg, &paths, two_machines());
        let before = calls(&log).len();
        feed.keep_walk();
        assert!(calls(&log).len() > before, "with machines it reads");
    }
}
