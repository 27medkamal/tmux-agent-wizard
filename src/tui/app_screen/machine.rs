//! Going into a machine from the app: where a landing on a machine's row goes ([`route`], pure), and the
//! attach that puts that machine's tmux on the terminal ([`Feed::go_into_machine`]).

use std::time::{Duration, Instant};

use super::{Feed, LEFT_WAIT, Step, ending};
use crate::machine::bounded::{self, Ran};
use crate::machine::ssh::{self, Mode};
use crate::machine::wire::{Left, State};
use crate::machine::{Machine, split_id, valid_go_id, valid_tmux_id, words};
use crate::tmux::ids;
use crate::tui::Lending;
use crate::tui::actions::{self, Action};
use crate::tui::app::App;
use crate::tui::machines::namespaced;

/// How long the app waits for a machine to open a worktree before it ends the ssh.
const OPEN_BOUND: Duration = Duration::from_secs(20);

/// How often the wait for that reply looks, and lets the machines move on.
const OPEN_LOOK: Duration = Duration::from_millis(30);

/// How an attempt to go into a machine came out, for the caller that has something to do about a refusal.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Went {
    /// The machine's tmux had the terminal and the app is back.
    Attached,
    /// The attach ended badly: its words are said (ssh could not keep the machine, the far taw refused).
    Failed,
    /// Nothing was run: the machine is unknown or away, or the target is no id. Its words are said.
    Refused,
}

/// Where a landing on a machine's row goes. The ids are the machine's own (`%16`, not `studio:%16`).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Attach `target` (`%N`, `$N` or `@N`) on `machine`. `pane` is the raw pane to come back to when no
    /// line says another: the one you went into.
    Attach {
        machine: String,
        target: String,
        pane: Option<String>,
    },
    /// The closed worktree `workspace` (`w5`) is opened on `machine` first, and the session that made is
    /// attached.
    OpenFirst { machine: String, workspace: String },
    /// A landing whose ids are not one machine's row: nothing is run.
    Nothing,
}

/// The machine's name and the machine's own id of `id`, when `id` is a machine's (`studio:%16`).
fn machines_own(id: &str) -> Option<(&str, &str)> {
    match split_id(id) {
        (Some(machine), raw) => Some((machine, raw)),
        (None, _) => None,
    }
}

/// What a landing does when its ids are a machine's, or `None` when they are this Mac's (the landing is
/// [`crate::tui::actions::land`]'s). A jump attaches the pane (so tmux selects its window and its pane), a session
/// header the session, a closed worktree is opened first. `active_pane_of` says the active pane of a session of the
/// list (namespaced, as the rows carry it): the pane Back goes in at when a session was attached.
pub(super) fn route(
    action: &Action,
    active_pane_of: impl Fn(&str) -> Option<String>,
) -> Option<Route> {
    match action {
        Action::Jump {
            session,
            window,
            pane,
        } => {
            let (machine, raw) = machines_own(pane)?;
            // The three ids of a row are one machine's: a mix is no row.
            let same = |id: &str| machines_own(id).is_some_and(|(name, _)| name == machine);
            if !same(session) || !same(window) {
                return Some(Route::Nothing);
            }
            Some(Route::Attach {
                machine: machine.to_string(),
                target: raw.to_string(),
                pane: Some(raw.to_string()),
            })
        }
        Action::SwitchWorkspace { session } => {
            let (machine, raw) = machines_own(session)?;
            let pane = active_pane_of(session)
                .as_deref()
                .and_then(machines_own)
                .filter(|(name, raw)| *name == machine && raw.starts_with('%'))
                .map(|(_, raw)| raw.to_string());
            Some(Route::Attach {
                machine: machine.to_string(),
                target: raw.to_string(),
                pane,
            })
        }
        // The row of a machine with no session: a new one is started there, with no pane to come back to.
        Action::Doorway { machine, id } if id == words::NEW_ID => Some(Route::Attach {
            machine: machine.clone(),
            target: words::NEW_ID.to_string(),
            pane: None,
        }),
        Action::OpenWorktree { workspace } => {
            let (machine, raw) = machines_own(workspace)?;
            Some(Route::OpenFirst {
                machine: machine.to_string(),
                workspace: raw.to_string(),
            })
        }
        // A new worktree on a machine is not a landing: the screen sends it to the machine (`remote_actions`) and
        // goes in when it answers. The next agent is not routed here: with machines the screen walks the snapshot
        // itself (`Feed::perform`), and with none no id is a machine's.
        _ => None,
    }
}

impl Feed<'_> {
    /// Goes where `route` says: an attach, a closed worktree opened first and then attached, or nowhere.
    pub(super) fn go_by_route(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        route: Route,
    ) -> anyhow::Result<Went> {
        match route {
            Route::Attach {
                machine,
                target,
                pane,
            } => self.go_into_machine(app, screen, &machine, &target, pane.as_deref(), None),
            Route::OpenFirst { machine, workspace } => {
                if super::must_end(self) {
                    return Ok(Went::Attached);
                }
                let opened = self
                    .machine_to_enter(&machine)
                    .map_err(Stop::Said)
                    .and_then(|(_, found)| {
                        // The wait is a few seconds on a cold ssh: the screen says `opening worktree on <machine>`
                        // first (the words the `w` path toasts too), and nothing of the list stays on it.
                        screen.loading(&app.theme, &words::opening_worktree(&machine));
                        self.open_worktree_on(&found, &workspace, OPEN_BOUND)
                    });
                // The wait read nothing, so what was typed or scrolled meanwhile is still queued: dropped before
                // it can reach the far tmux through the ssh, or run the same open again after a failure.
                drain_after_open(screen, &opened);
                self.after_open(app, screen, &machine, opened)
            }
            Route::Nothing => Ok(self.refused(app, words::NOTHING_TO_GO_IN_TO.to_string())),
        }
    }

    /// What follows an open: the session it made is attached, a refusal is said as a toast, and an open that was
    /// stopped because the app must end says nothing and goes nowhere (the loop's first look ends the app).
    fn after_open(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        machine: &str,
        opened: Result<Opened, Stop>,
    ) -> anyhow::Result<Went> {
        match opened {
            Ok(opened) => self.go_into_machine(
                app,
                screen,
                machine,
                &opened.session,
                None,
                Some(&opened.note),
            ),
            Err(Stop::Said(words)) => Ok(self.refused(app, words)),
            Err(Stop::Ended) => Ok(Went::Attached),
        }
    }

    /// Says `words` as a toast, shows the list, and tells the caller nothing was run.
    fn refused(&mut self, app: &mut App, words: String) -> Went {
        app.toast(words);
        self.show(self.tmux, app);
        Went::Refused
    }

    /// [`Feed::refused`] for `taw app --go`, which has just read this Mac to open the app: the list is drawn from
    /// that read, not from a second one under the same loading frame.
    fn refused_from_read(&mut self, app: &mut App, words: String) -> Went {
        app.toast(words);
        self.refresh(self.tmux, app);
        Went::Refused
    }

    /// The machine called `name` and its place in `[machines]`, when it is one and is live now: judged by
    /// the connections as they are this moment (`self.hub.views()`), not by the list of the last tick. `Err` is
    /// the words that say why not.
    fn machine_to_enter(&self, name: &str) -> Result<(usize, Machine), String> {
        let Some((at, machine)) = self
            .machines
            .iter()
            .enumerate()
            .find(|(_, machine)| machine.name == name)
        else {
            return Err(words::no_such_machine(name));
        };
        let views = self.hub.views();
        let view = views.iter().find(|view| view.name == name);
        if view.is_some_and(|view| view.live) {
            return Ok((at, machine.clone()));
        }
        let seen = view.and_then(|view| view.last_seen).map_or(0, |at| {
            crate::machine::files::now_ms().saturating_sub(at) / 1000
        });
        Err(words::not_connected(name, seen))
    }

    /// Goes into the machine `name`: its tmux, attached to `target` (`%N`, `$N` or `@N`, the machine's own id),
    /// is put on the terminal through ssh, and the app is back on its list when it ends. `pane` is the pane (the
    /// machine's own `%N`) the cursor goes to when no `left` line names another, and `note` is what is said after
    /// coming back. Nothing here knows of the screen's rows: the callers are the landing, Back, and what is
    /// built on this later.
    ///
    /// In this order, each a toast with nothing run: the machine is one of `[machines]`, it is live now, and
    /// `target` is an id.
    pub(super) fn go_into_machine(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        name: &str,
        target: &str,
        pane: Option<&str>,
        note: Option<&str>,
    ) -> anyhow::Result<Went> {
        let (went, step) = self.go_into_machine_once(app, screen, name, target, pane, note)?;
        self.follow_go(app, screen, step)?;
        Ok(went)
    }

    /// One attach of a machine, as [`Feed::go_into_machine`] describes it, without what its end sends on: a
    /// `left` line that carries `go` is not acted on here, and the list is not drawn for it ([`Step::Go`]).
    pub(super) fn go_into_machine_once(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        name: &str,
        target: &str,
        pane: Option<&str>,
        note: Option<&str>,
    ) -> anyhow::Result<(Went, Step)> {
        // An app that must end starts nothing and lends nothing: the loop's first look ends it.
        if super::must_end(self) {
            return Ok((Went::Attached, Step::Done));
        }
        let (at, machine) = match self.machine_to_enter(name) {
            Ok(found) => found,
            Err(words) => return Ok((self.refused(app, words), Step::Done)),
        };
        if !valid_go_id(target) {
            return Ok((
                self.refused(app, words::NOTHING_TO_GO_IN_TO.to_string()),
                Step::Done,
            ));
        }
        // The screen says `loading <session>` now: nothing of the list stays on it while the attach is made.
        screen.loading(
            &app.theme,
            &super::going_words(&app.snap, &namespaced(name, target)),
        );
        let holder = self.record.holder().to_string();
        // `new` is no id: it asks the far taw to start a session, for a machine that has none.
        let far: Vec<&str> = if target == words::NEW_ID {
            vec!["machine", "attach", "--new", "--holder", &holder]
        } else {
            vec!["machine", "attach", target, "--holder", &holder]
        };
        let command = ssh::command(&self.ssh, self.paths, &machine, Mode::Attach, &far);
        let supervisor = self.record.supervisor();
        let theme = app.theme;
        // The terminal is the ssh's while it runs, and the far tmux draws on it. The wait for the machine's
        // line is below, after the screen is back.
        // A failed attach is not drawn over: its own words stay on the plain terminal, with one line under them,
        // until Enter (`words::attach_held`). The machines keep moving meanwhile, as they do during the attach.
        let (ended, back) = crate::tui::lend_through_holding(
            screen,
            self,
            |feed| feed.attach(command, name, None),
            |ended| failure_words(name, ended).map(|failure| words::attach_held(&failure)),
            |feed| {
                feed.hub.poll(Instant::now());
                feed.take_announced();
                super::must_end(feed)
            },
            || ending(supervisor)(),
            // The screen is back and says `loading` while the machine's line is waited for and the list is read.
            |screen| screen.loading(&theme, crate::tui::view::LOADING),
        )?;
        if back.is_none() || super::signalled() || self.record.orphaned() {
            // The app was asked to end: nothing more is drawn, and the loop's first look ends it.
            return Ok((Went::Attached, Step::Done));
        }
        // The line comes before the client goes when the key sent it back, so it is kept already; one that
        // is a moment late is waited for, and a client that ended another way has none.
        let line = self.left_of(at, LEFT_WAIT);
        screen.drain();
        let failure = failure_words(name, &ended);
        let went = if failure.is_some() {
            Went::Failed
        } else {
            Went::Attached
        };
        // A line that says where to go next is acted on at once, with the list not drawn.
        if failure.is_none()
            && let Some(left) = line.as_ref()
            && let Some(go) = left.go.as_deref()
        {
            return Ok((
                went,
                Step::Go {
                    go: go.to_string(),
                    pane: Some(namespaced(name, &left.pane)).filter(|_| !left.pane.is_empty()),
                },
            ));
        }
        self.came_back(
            app,
            screen,
            note,
            failure.as_deref(),
            come_back_to(name, line.map(|left| left.pane), pane),
        );
        Ok((went, Step::Done))
    }

    /// The line of `machine` (its place in `[machines]`) that is kept, or that comes within `wait`:
    /// the hub is polled while it waits, as the wait for an attach polls it. `None` when none came. A line of
    /// another machine is not this attach's, and one of another holder was never kept.
    fn left_of(&mut self, machine: usize, wait: Duration) -> Option<Left> {
        let until = Instant::now() + wait;
        loop {
            if let Some(at) = self.lefts.iter().rposition(|(from, _)| *from == machine) {
                return Some(self.lefts.remove(at).1);
            }
            if Instant::now() >= until {
                return None;
            }
            super::left_step();
            self.hub.poll(Instant::now());
            self.take_announced();
        }
    }

    /// Opens the closed worktree `workspace` (`w5`) on `machine` without switching anything there, and says what
    /// it made: the session to attach and the words to say after. The ssh is ended by its pid when the machine
    /// has not answered within `bound` (a hung machine must not hang the app) or the app was asked to end; the
    /// machines keep moving while it waits. `Err` is the words of a toast, or `Stop::Ended`.
    fn open_worktree_on(
        &mut self,
        machine: &Machine,
        workspace: &str,
        bound: Duration,
    ) -> Result<Opened, Stop> {
        let name = &machine.name;
        if !is_workspace_id(workspace) {
            return Err(Stop::Said(words::NOTHING_TO_GO_IN_TO.to_string()));
        }
        let command = ssh::command(
            &self.ssh,
            self.paths,
            machine,
            Mode::Command,
            &["worktree", "open", "--workspace", workspace, "--no-focus"],
        );
        let ran = bounded::run(
            self,
            command,
            bound,
            OPEN_LOOK,
            |feed| super::must_end(feed),
            |feed| {
                feed.hub.poll(Instant::now());
                feed.take_announced();
            },
        );
        match ran {
            Ran::Failed(err) => Err(Stop::Said(words::ssh_not_run(name, &err))),
            Ran::Ended => Err(Stop::Ended),
            Ran::TimedOut => Err(Stop::Said(words::worktree_open_timed_out(
                name,
                bound.as_secs(),
            ))),
            Ran::Done {
                code,
                signal,
                stdout,
                ..
            } => read_opened(name, code, signal, &stdout).map_err(Stop::Said),
        }
    }
}

/// Why an attach of `name` ended badly, in words: [`words::attach_ended`] for what the ssh ended with, and the
/// words of an attach that could not start or be waited for. `None` for a clean end.
fn failure_words(name: &str, ended: &Result<std::process::ExitStatus, String>) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    match ended {
        Ok(status) => words::attach_ended(name, status.code(), status.signal()),
        Err(words) => Some(words.clone()),
    }
}

/// What `taw app --go` opens on: this Mac's pane (`%7`), the session this Mac was in last (`last`), or a machine
/// of `[machines]` and one of that machine's own tmux ids, as `studio:%16` says them, or `new` (`studio:new`: a
/// new session there). A machine's id is not checked here: an id that is no id is the landing's to refuse.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum GoTarget {
    /// One of a machine's ids, or [`words::NEW_ID`].
    Machine { machine: String, id: String },
    /// A pane of this Mac, by its tmux id.
    Pane(String),
    /// The session of this Mac attached most lately; [`run_app`](super::run_app) turns it into
    /// [`GoTarget::Session`] before the first frame, and makes a session when there is none.
    Last,
    /// A session of this Mac, as an `attach-session -t` target: its id, or `=name`.
    Session(String),
}

impl GoTarget {
    /// `last`, a pane id of this Mac (`%7`), or `machine:id`; the usage words for anything else (a bare `$3` or
    /// `@5` names no pane to go into).
    pub fn parse(target: &str) -> Result<GoTarget, &'static str> {
        if target == "last" {
            return Ok(GoTarget::Last);
        }
        match split_id(target) {
            (Some(machine), id) if !machine.is_empty() => Ok(GoTarget::Machine {
                machine: machine.to_string(),
                id: id.to_string(),
            }),
            (None, id) if id.starts_with('%') && valid_tmux_id(id) => {
                Ok(GoTarget::Pane(id.to_string()))
            }
            _ => Err(words::GO_USAGE),
        }
    }
}

/// How long `--go` waits for its machine to be live before it gives up and shows the list.
pub(super) const GO_WAIT: Duration = Duration::from_secs(10);

impl Feed<'_> {
    /// `taw app --go`: goes into `target` as Enter on its row would (an attach of the id, with no pane to come
    /// back to), before the first frame. The machine may not be live yet (a follower reads files, a keeper has just
    /// started its connections), so a machine that is one of `[machines]` is waited for up to `wait`, live and with
    /// its first state, the hub polled and nothing drawn; then the landing says what it says of a machine that is away. An id that is no id
    /// and a machine that is not listed are refused at once: nothing is waited for that could not come. The
    /// list is shown with the words, and nothing is attached.
    ///
    /// The doorway app starts as a follower whenever another app holds the lease, so studio's popup key is not
    /// routed back to it (the key rule: a follower does not steal keys); this is by design, and detaching
    /// studio's client is the way back to the list.
    pub(super) fn go_first(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        target: &GoTarget,
        wait: Duration,
    ) -> anyhow::Result<Went> {
        let GoTarget::Machine {
            machine: name,
            id: raw,
        } = target
        else {
            return self.go_here(app, screen, target);
        };
        if !valid_go_id(raw) {
            return Ok(self.refused_from_read(app, words::NOTHING_TO_GO_IN_TO.to_string()));
        }
        let until = Instant::now() + wait;
        let listed = self.machines.iter().any(|m| &m.name == name);
        // Live is not enough: a machine is live when its feed is up, and its first state follows a moment later, so
        // the wait is for the state too (within the same bound), and the attach goes on without one at the end of it.
        while listed
            && (self.machine_to_enter(name).is_err() || !self.has_state(name))
            && Instant::now() < until
            && !super::must_end(self)
        {
            std::thread::sleep(OPEN_LOOK);
            self.hub.poll(Instant::now());
            self.take_announced();
        }
        // The wait read nothing, so what was typed meanwhile, with the screen saying `loading`, is still queued:
        // dropped before the terminal is lent, or it would reach the machine's pane through the ssh (a `y` or
        // Enter can answer an agent's prompt there). An app that must end drops nothing, as after an open.
        drain_unless_ended(screen, super::must_end(self));
        // A machine that is live and has delivered its state, and whose state holds no pane, session or window
        // of that id, has none: said as a gone target is, with nothing attached. With no state yet, or the id
        // in it, the attach goes on, and the far side's refusal (its own words) is for a state that lags.
        if self.state_lacks(name, raw) {
            return Ok(self.refused_from_read(app, words::gone(&format!("{name}:{raw}"))));
        }
        self.go_by_route(
            app,
            screen,
            Route::Attach {
                machine: name.clone(),
                target: raw.clone(),
                pane: None,
            },
        )
    }

    /// Whether the machine `name` is live and has delivered a state.
    fn has_state(&self, name: &str) -> bool {
        let views = self.hub.views();
        views
            .iter()
            .any(|view| view.name == name && view.live && view.state.is_some())
    }

    /// Whether the machine `name` is live, has delivered a state, and that state has no pane, session or window
    /// called `id` (its own `%N`, `$N` or `@N`).
    fn state_lacks(&self, name: &str, id: &str) -> bool {
        let views = self.hub.views();
        views
            .iter()
            .find(|view| view.name == name && view.live)
            .and_then(|view| view.state.as_ref())
            .is_some_and(|state| !state_holds(state, id))
    }

    /// `--go` on this Mac: a pane (`%7`) or a session ([`GoTarget::Session`], what `last` came to) is gone into as
    /// Enter on a local row goes ([`Feed::go_in`]: the terminal is lent the usual way, so the key back works and
    /// the list is what comes back), with no pane to come back to. A pane tmux no longer has is the toast
    /// [`words::gone`] and the list, and nothing is attached.
    fn go_here(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
        target: &GoTarget,
    ) -> anyhow::Result<Went> {
        let landing = match target {
            GoTarget::Pane(pane) => match actions::land_on_pane(self.tmux, pane) {
                Some(landing) => actions::Landing {
                    pane: None,
                    ..landing
                },
                None => return Ok(self.refused_from_read(app, words::gone(pane))),
            },
            GoTarget::Session(session) => actions::Landing {
                target: session.clone(),
                pane: None,
                note: None,
            },
            // A machine's is `go_first`'s, and `last` is turned into a session before the first frame.
            GoTarget::Machine { .. } | GoTarget::Last => {
                return Ok(self.refused_from_read(app, words::GO_USAGE.to_string()));
            }
        };
        self.go_in(app, screen, landing)?;
        Ok(Went::Attached)
    }
}

/// Whether a machine's `state` has a pane, a session or a window with the tmux id `id`, among its rows
/// and its agents' panes.
fn state_holds(state: &State, id: &str) -> bool {
    match id.chars().next() {
        Some('%') => {
            state.panes.iter().any(|p| p.pane_id == id)
                || state.agents.iter().any(|a| a.pane_id == id)
        }
        Some('$') => {
            state.spaces.iter().any(|s| s.session_id == id)
                || state.panes.iter().any(|p| p.session_id == id)
        }
        Some('@') => {
            state.spaces.iter().any(|s| s.active_window_id == id)
                || state.panes.iter().any(|p| p.window_id == id)
                || state.agents.iter().any(|a| a.window_id == id)
        }
        _ => true,
    }
}

/// The pane the cursor goes to when the app is back from `name`: the one the machine's `left` line names (a `%`
/// id, or empty when the key named none), else the one that was gone into, as the list names it (`studio:%16`).
fn come_back_to(name: &str, line: Option<String>, went_into: Option<&str>) -> Option<String> {
    line.filter(|pane| !pane.is_empty())
        .or_else(|| went_into.map(str::to_string))
        .map(|pane| namespaced(name, &pane))
}

/// Why an open gave no session: words to say as a toast, or the app must end and says nothing.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    Said(String),
    Ended,
}

/// What a worktree opened on a machine gives the app: the session to attach (`$N`) and the note to say.
#[derive(Debug, PartialEq, Eq)]
struct Opened {
    session: String,
    note: String,
}

/// Drops what was typed or scrolled during an open, unless it ended because the app must end: the terminal is the
/// shell's then, and `lend_through` drops nothing either.
fn drain_after_open(screen: &mut impl Lending, opened: &Result<Opened, Stop>) {
    drain_unless_ended(screen, matches!(opened, Err(Stop::Ended)));
}

/// Drops what was typed or scrolled during a wait that read nothing, unless the app must end (`ended`): the
/// terminal is the shell's then. The open's drain and the `--go` wait's drain are this one.
fn drain_unless_ended(screen: &mut impl Lending, ended: bool) {
    if !ended {
        screen.drain();
    }
}

/// Whether `id` is the id a worktree record of a machine carries: `w` and digits.
fn is_workspace_id(id: &str) -> bool {
    matches!(ids::parse(id), Some(ids::Target::Workspace(_))) && id.starts_with('w')
}

/// Reads the reply of `taw worktree open ...` run on `name`, which ended with `code` (or `signal`):
/// `{"id":..,"result":{"worktree":{"workspace_id":"w5","branch":..},"already_open":bool,"session_id":"$9"}}`, or an error
/// envelope `{"id":..,"error":{"code":..,"message":..}}`. The words of a toast for anything else.
fn read_opened(
    name: &str,
    code: Option<i32>,
    signal: Option<i32>,
    stdout: &[u8],
) -> Result<Opened, String> {
    // ssh's own failure (or an ssh that was ended) has no reply to read.
    if (code.is_none() || code == Some(255))
        && let Some(said) = words::attach_ended(name, code, signal)
    {
        return Err(said);
    }
    // The envelope is the last line: a login shell may have said something before it.
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty());
    let Some(reply) = line.and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    else {
        return Err(words::worktree_open_unreadable(name));
    };
    if let Some(message) = reply["error"]["message"].as_str() {
        return Err(words::worktree_open_failed(name, message));
    }
    let worktree = &reply["result"]["worktree"];
    // The session is the one `session_id` names, the running session's own id. The record's `workspace_id` is the
    // id the session had when the worktree was created, and a session the open made has another: it is never
    // taken for the session, and a taw that says no `session_id` (an older one) attaches nothing.
    let session = reply["result"]["session_id"]
        .as_str()
        .filter(|id| id.starts_with('$') && crate::machine::valid_tmux_id(id))
        .map(str::to_string);
    let (Some(session), Some(branch)) = (session, worktree["branch"].as_str()) else {
        return Err(words::worktree_open_unreadable(name));
    };
    let already_open = reply["result"]["already_open"].as_bool().unwrap_or(false);
    Ok(Opened {
        session,
        note: actions::opened_note(already_open, branch),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn jump(session: &str, window: &str, pane: &str) -> Action {
        Action::Jump {
            session: session.into(),
            window: window.into(),
            pane: pane.into(),
        }
    }

    /// A jump on a machine's pane attaches the PANE, by the machine's own id: tmux then selects its window
    /// and pane as `attach -t %N` does.
    #[test]
    fn a_jump_on_a_machines_pane_attaches_the_pane_by_its_own_id() {
        assert_eq!(
            route(&jump("studio:$3", "studio:@5", "studio:%16"), none),
            Some(Route::Attach {
                machine: "studio".into(),
                target: "%16".into(),
                pane: Some("%16".into()),
            })
        );
    }

    /// The target is split from the namespaced id, not built from it: the machine's name never reaches tmux.
    #[test]
    fn the_target_is_the_machines_own_id_without_the_name() {
        for action in [
            jump("pi:$3", "pi:@5", "pi:%16"),
            Action::SwitchWorkspace {
                session: "pi:$3".into(),
            },
        ] {
            match route(&action, none) {
                Some(Route::Attach { target, .. }) => {
                    assert!(!target.contains(':'), "{target}");
                    assert!(target.starts_with(['%', '$']), "{target}");
                }
                other => panic!("{action:?}: {other:?}"),
            }
        }
    }

    /// A session header attaches the session; the pane to come back to is the session's active pane, when the
    /// list knows it and it is that machine's, and none otherwise.
    #[test]
    fn a_session_header_attaches_the_session_and_comes_back_to_its_active_pane() {
        let header = Action::SwitchWorkspace {
            session: "studio:$3".into(),
        };
        let active = |session: &str| {
            assert_eq!(session, "studio:$3");
            Some("studio:%7".to_string())
        };
        assert_eq!(
            route(&header, active),
            Some(Route::Attach {
                machine: "studio".into(),
                target: "$3".into(),
                pane: Some("%7".into()),
            })
        );
        // None known, or one that is another machine's, or no pane: none to come back to.
        for said in [None, Some("pi:%7"), Some("studio:@5"), Some("%7")] {
            let said = said.map(str::to_string);
            match route(&header, |_| said.clone()) {
                Some(Route::Attach { pane, .. }) => assert_eq!(pane, None, "{said:?}"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// A closed worktree is opened on its machine first, by the machine's own workspace id.
    #[test]
    fn a_closed_worktree_is_opened_first() {
        assert_eq!(
            route(
                &Action::OpenWorktree {
                    workspace: "studio:w5".into()
                },
                none
            ),
            Some(Route::OpenFirst {
                machine: "studio".into(),
                workspace: "w5".into(),
            })
        );
    }

    /// This Mac's ids are not routed: the landing is the local one's.
    #[test]
    fn this_macs_ids_are_left_to_the_local_landing() {
        assert_eq!(route(&jump("$3", "@5", "%16"), none), None);
        assert_eq!(
            route(
                &Action::SwitchWorkspace {
                    session: "$3".into()
                },
                none
            ),
            None
        );
        assert_eq!(
            route(
                &Action::OpenWorktree {
                    workspace: "w5".into()
                },
                none
            ),
            None
        );
        assert_eq!(
            route(
                &Action::OpenProject {
                    path: "~/a:b".into()
                },
                none
            ),
            None
        );
        assert_eq!(route(&Action::NextUp { own: None }, none), None);
        assert_eq!(
            route(
                &Action::NextUp {
                    own: Some("%4".into())
                },
                none
            ),
            None
        );
        // Not even with a machine's id: the next agent is walked by the screen, never routed.
        assert_eq!(
            route(
                &Action::NextUp {
                    own: Some("studio:%4".into())
                },
                none
            ),
            None
        );
    }

    /// A landing whose ids are not one machine's row runs nothing.
    #[test]
    fn a_landing_no_machine_row_makes_goes_nowhere() {
        // A jump whose three ids are not one machine's is no row.
        assert_eq!(
            route(&jump("pi:$3", "studio:@5", "studio:%16"), none),
            Some(Route::Nothing)
        );
        assert_eq!(
            route(&jump("$3", "studio:@5", "studio:%16"), none),
            Some(Route::Nothing)
        );
    }

    // ---- Going into a machine: the attach, its refusals, and how it ends ----

    use std::path::{Path, PathBuf};

    use super::super::tests::{KillOnDrop, Unlent, feed_over, paths_in, wait_until_not_running};
    use crate::cli::test_support::{Stub, stub_tmux};
    use crate::config::Config;
    use crate::machine::conn::{self, ConnCtx, Timing};
    use crate::machine::files::{self, MachineFile};
    use crate::machine::machines::Machines;
    use crate::machine::ssh::Ssh;
    use crate::machine::wire::{Left, State, WirePane, WireSpace};
    use crate::paths::Paths;
    use crate::tui::data::Snapshot;
    use crate::tui::theme;

    /// One session `$3` with two panes, `%7` and `%9`, as studio's feed sends it.
    fn studio_state() -> State {
        let pane = |id: &str, index: u32, active: bool| WirePane {
            pane_id: id.into(),
            session_id: "$3".into(),
            window_id: "@5".into(),
            window_index: 1,
            window_name: "zsh".into(),
            window_panes: 2,
            pane_index: index,
            pane_current_command: "zsh".into(),
            pane_active: active,
            alternate_on: false,
        };
        State {
            spaces: vec![WireSpace {
                session_id: "$3".into(),
                session_name: "garden".into(),
                session_path: "/home/pi/garden".into(),
                windows: 1,
                active_window_id: "@5".into(),
                active_pane_id: Some("%7".into()),
                ..WireSpace::default()
            }],
            panes: vec![pane("%7", 1, true), pane("%9", 2, false)],
            agents: Vec::new(),
            worktrees: Vec::new(),
            open_worktrees: Vec::new(),
        }
    }

    fn studio() -> Machine {
        Machine {
            name: "studio".into(),
            ssh: "studio".into(),
            taw: None,
        }
    }

    /// Writes an ssh stand-in: it records each call's arguments (one per line, `---` after each call), copies the
    /// app's record of this process to `record-during` while it runs (once it names this stand-in as the client:
    /// the attach writes it just after the spawn, so the stand-in waits up to 20 s for it, and says so in the copy
    /// when it never did), prints `reply` when asked for `worktree`,
    /// and ends with `exit_code` otherwise. Returns the program and the log.
    fn stand_in_ssh(dir: &Path, paths: &Paths, reply: &str, exit_code: i32) -> (PathBuf, PathBuf) {
        let log = dir.join("ssh-calls");
        let program = dir.join("ssh-stand-in");
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done >> '{log}'\necho '---' >> '{log}'\n\
             case \"$*\" in *worktree*) printf '%s\\n' '{reply}'; exit 0;; esac\n\
             n=0\nwhile [ $n -lt 1000 ]; do grep -q \"\\\"client_pid\\\":$$\" '{record}' 2>/dev/null && break; n=$((n+1)); sleep 0.02; done\n\
             cp '{record}' '{during}' 2>/dev/null\n\
             [ $n -ge 1000 ] && echo \"the record never named pid $$ within 20 s\" >> '{during}'\n\
             exit {exit_code}\n",
            log = log.display(),
            reply = reply,
            record = paths.app_record(std::process::id()).display(),
            during = dir.join("record-during").display(),
        );
        std::fs::write(&program, script).unwrap();
        crate::test_exec::make_executable(&program);
        (program, log)
    }

    /// The calls the stand-in saw: each call's arguments, but not the reads of the pane under the cursor
    /// (`pane read`, [`super::super::remote_preview`]): they are made on a thread of their own, as the tick
    /// comes, so a test that counts what its action ran counts without them.
    fn ssh_calls(log: &Path) -> Vec<Vec<String>> {
        ssh_calls_with_reads(log)
            .into_iter()
            .filter(|call| !remote_of(call).contains("'pane' 'read'"))
            .collect()
    }

    /// Every call the stand-in saw, the reads of the preview included.
    fn ssh_calls_with_reads(log: &Path) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let mut calls = Vec::new();
        let mut call = Vec::new();
        for line in text.lines() {
            if line == "---" {
                calls.push(std::mem::take(&mut call));
            } else {
                call.push(line.to_string());
            }
        }
        calls
    }

    /// A feed whose machines are `studio`, read from its file by a follower: live when `live`, and when a keeper
    /// (the returned lock, held by the test) keeps the file. Its ssh is `ssh`.
    fn studio_feed<'a>(
        tmux: &'a crate::tmux::Tmux,
        cfg: &'a Config,
        paths: &'a Paths,
        ssh: &Path,
        live: bool,
        keeper: bool,
    ) -> (Feed<'a>, Option<files::Keeper>) {
        let now = files::now_ms();
        // The lock of an earlier feed of the test may still be held for a moment by a process another test is
        // starting (it inherits the descriptor until its exec), so the lock is waited for.
        let lock = keeper.then(|| {
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(lock) = files::take_keeper(paths) {
                    break lock;
                }
                assert!(Instant::now() < until, "the keeper lock was never free");
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        files::write_machine_file(
            paths,
            &MachineFile {
                name: "studio".into(),
                state: Some(studio_state()),
                received_at: Some(now - 125_000),
                last_seen: Some(if live { now } else { now - 125_000 }),
                live,
                words: "connected: taw 0.1.0".into(),
                short: String::new(),
                taw_version: None,
                protocol: None,
                this_server: false,
            },
        );
        let mut feed = feed_over(tmux, cfg, paths);
        feed.ssh = Ssh { bin: ssh.into() };
        feed.machines = vec![studio()];
        feed.hub = Machines::follow(
            ConnCtx {
                ssh: Ssh {
                    bin: "/nonexistent/ssh".into(),
                },
                paths: paths.clone(),
                holder: conn::holder_word(),
                timing: Timing::default(),
                local_server: None,
            },
            vec![studio()],
        );
        feed.hub.poll(Instant::now());
        (feed, lock)
    }

    fn app_over<'a>(cfg: &'a Config) -> App<'a> {
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        App::new(cfg, theme, String::new(), Snapshot::default())
    }

    fn toast_of(app: &App) -> Option<String> {
        app.toast.as_ref().map(|(text, _)| text.clone())
    }

    /// The last argument of an ssh call: the line the machine runs, its words single-quoted.
    fn remote_of(call: &[String]) -> &str {
        call.last().map(String::as_str).unwrap_or_default()
    }

    /// A machine that is not in `[machines]`, one that is away (its file says so, or nobody keeps its file), and a
    /// target that is no id all say so and run nothing, in that order: known, live now, a valid id.
    #[test]
    fn going_into_a_machine_that_cannot_be_gone_into_runs_nothing_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);

        // Positive half: the live machine is gone into with a good target, so that the refusals below are the
        // refusals' own and not an ssh that never runs.
        let went = feed
            .go_into_machine(&mut app, &mut Unlent, "studio", "%7", None, None)
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert_eq!(ssh_calls(&log).len(), 1, "the live machine was gone into");

        let mut says = |feed: &mut Feed, name: &str, target: &str| {
            app.toast = None;
            let went = feed
                .go_into_machine(&mut app, &mut Unlent, name, target, None, None)
                .unwrap();
            assert_eq!(went, Went::Refused, "{name} {target}");
            toast_of(&app).unwrap_or_default()
        };
        assert_eq!(
            says(&mut feed, "ghost", "%7"),
            words::no_such_machine("ghost")
        );
        // The machine is judged before the target: a machine that is not there says so for a bad target too.
        assert_eq!(
            says(&mut feed, "ghost", "nonsense"),
            words::no_such_machine("ghost")
        );
        for target in ["", "studio:%7", "%7 x", "%", "$", "w5", "%7;ls", "-t", "%x"] {
            assert_eq!(
                says(&mut feed, "studio", target),
                words::NOTHING_TO_GO_IN_TO,
                "{target:?}"
            );
        }
        assert_eq!(ssh_calls(&log).len(), 1, "nothing refused was run");

        // Away: the file says it is not live. Said before the target is looked at.
        let (mut away, _keeper_of_away) = {
            drop(feed);
            drop(_keeper);
            studio_feed(&tmux, &cfg, &paths, &ssh, false, true)
        };
        assert_eq!(
            says(&mut away, "studio", "%7"),
            words::not_connected("studio", 125)
        );
        assert_eq!(
            says(&mut away, "studio", "nonsense"),
            words::not_connected("studio", 125)
        );
        // Nobody keeps the file: whatever it says, the machine is not live.
        // Its own folder: a lock file nobody ever took, so that no process another test starts holds it.
        let fresh = tempfile::tempdir().unwrap();
        let fresh_paths = paths_in(fresh.path());
        let (mut kept_by_nobody, _) = {
            drop(away);
            drop(_keeper_of_away);
            studio_feed(&tmux, &cfg, &fresh_paths, &ssh, true, false)
        };
        assert!(says(&mut kept_by_nobody, "studio", "%7").contains("is not connected"));
        assert_eq!(ssh_calls(&log).len(), 1, "nothing refused was run");
    }

    /// A machine is attached by its own id with this app's holder, on a terminal that is the ssh's (`-t`), and the
    /// record names the machine while it runs. Back, the cursor is on the namespaced pane that was gone into.
    #[test]
    fn a_live_machine_is_attached_by_its_own_id_with_this_apps_holder() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let holder = feed.record.holder().to_string();

        let went = feed
            .go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
            .unwrap();

        assert_eq!(went, Went::Attached);
        assert_eq!(toast_of(&app), None, "a clean end says nothing");
        let calls = ssh_calls(&log);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains(&"-t".to_string()), "{:?}", calls[0]);
        let remote = remote_of(&calls[0]);
        assert!(
            remote.contains(&format!("'machine' 'attach' '%7' '--holder' '{holder}'")),
            "{remote}"
        );
        assert!(
            !remote.contains("studio:"),
            "the name never reaches the machine: {remote}"
        );
        let during = std::fs::read_to_string(dir.path().join("record-during")).unwrap();
        assert!(
            during.contains("\"inside\":\"studio\""),
            "the record named the machine while ssh ran: {during}"
        );
        let after = files::read_app_record(&paths, std::process::id()).unwrap();
        assert_eq!(after.inside, "", "and does not after");
        assert_eq!(app.came_from(), Some("studio:%7"));
    }

    /// An attach that ends badly says why in one place's words, and the app is back all the same.
    #[test]
    fn an_attach_that_ends_badly_says_why_and_the_app_is_back() {
        for (code, expected) in [
            (255, words::attach_ended("studio", Some(255), None)),
            (2, words::attach_ended("studio", Some(2), None)),
            (7, words::attach_ended("studio", Some(7), None)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", code);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);

            let went = feed
                .go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
                .unwrap();

            assert_eq!(ssh_calls(&log).len(), 1, "the attach ran ({code})");
            assert_eq!(went, Went::Failed, "{code}");
            assert_eq!(toast_of(&app), expected, "{code}");
            assert_eq!(app.came_from(), Some("studio:%7"), "{code}");
        }
    }

    /// The pane the cursor goes to when the app is back: the machine's line wins when it names a pane, else the
    /// pane that was gone into; both as the list names them (`studio:%16`).
    #[test]
    fn back_from_a_machine_the_cursor_goes_to_the_pane_its_line_names_else_the_one_gone_into() {
        let said = |line: Option<&str>, into: Option<&str>| {
            come_back_to("studio", line.map(str::to_string), into)
        };
        assert_eq!(said(Some("%4"), Some("%16")).as_deref(), Some("studio:%4"));
        assert_eq!(said(None, Some("%16")).as_deref(), Some("studio:%16"));
        // A line that names no pane (the key named none) is no line.
        assert_eq!(said(Some(""), Some("%16")).as_deref(), Some("studio:%16"));
        assert_eq!(said(None, None), None);
        assert_eq!(said(Some(""), None), None);
    }

    /// The line a machine sent is taken for the attach of that machine only, newest first, and a wait that has
    /// nothing to wait for does not wait.
    #[test]
    fn a_kept_line_is_taken_for_its_machine_and_none_makes_the_wait_end() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, _) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let line = |pane: &str| Left {
            pane: pane.into(),
            ..Left::default()
        };
        feed.lefts = vec![(1, line("%1")), (0, line("%2")), (0, line("%3"))];

        let pane =
            |feed: &mut Feed, machine, wait| feed.left_of(machine, wait).map(|left| left.pane);
        assert_eq!(pane(&mut feed, 0, Duration::ZERO).as_deref(), Some("%3"));
        assert_eq!(pane(&mut feed, 0, Duration::ZERO).as_deref(), Some("%2"));
        // Positive half done: both of machine 0's were there. What is left is another machine's.
        let began = Instant::now();
        assert_eq!(pane(&mut feed, 0, Duration::from_millis(100)), None);
        assert!(began.elapsed() >= Duration::from_millis(100), "it waited");
        assert_eq!(feed.lefts.len(), 1, "another machine's line is not taken");
        assert_eq!(pane(&mut feed, 1, Duration::ZERO).as_deref(), Some("%1"));
    }

    /// A line that came before the attach (still queued in the hub, not yet taken) is an old line: the attach
    /// does not take it for its own, and the cursor goes to the pane gone into (a line that comes during the attach
    /// is taken: the tests of the line the wait is given)
    #[test]
    fn a_line_that_was_queued_before_the_attach_is_not_taken_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, _) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mine = Left {
            holder: feed.record.holder().to_string(),
            pane: "%9".into(),
            go: None,
        };
        // Delivered by the attach's own first poll, which is before the attach starts: so it is an old line.
        Machines::announce_on_poll(1, 0, mine);
        let mut app = app_over(&cfg);

        let went = feed
            .go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
            .unwrap();

        assert_eq!(went, Went::Attached);
        assert_eq!(
            app.came_from(),
            Some("studio:%7"),
            "the pane gone into, not the pane of an old line"
        );
        assert_eq!(
            feed.hub.waiting(),
            (0, 0),
            "the old line was taken and dropped, and no other came"
        );
    }

    /// A line that comes while the attach is waited for decides where the cursor goes when the app is back: the
    /// pane it names, when it is this app's holder's and this machine's; a line of another holder or of another
    /// machine is not this attach's. Each case is delivered on the second poll after the attach is begun (the first is the attach's own, which drops what came before it).
    #[test]
    fn a_line_that_comes_during_the_attach_is_taken_when_it_is_this_apps_and_this_machines() {
        for (own_holder, machine, expected) in [
            (true, 0, "studio:%9"),
            (false, 0, "studio:%7"),
            (true, 1, "studio:%7"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, _) = stand_in_ssh(dir.path(), &paths, "", 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let holder = if own_holder {
                feed.record.holder().to_string()
            } else {
                "another-4242".to_string()
            };
            Machines::announce_on_poll(
                2,
                machine,
                Left {
                    holder,
                    pane: "%9".into(),
                    go: None,
                },
            );
            let mut app = app_over(&cfg);

            let went = feed
                .go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
                .unwrap();

            assert_eq!(went, Went::Attached);
            assert_eq!(
                app.came_from(),
                Some(expected),
                "own holder {own_holder}, machine {machine}"
            );
        }
    }

    /// The reply of a machine that made a session (`$9`) for the worktree whose record still holds the id the
    /// session had when the worktree was created (`w5`).
    const OPENED: &str = r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":"$9"}}"#;

    /// A closed worktree is opened first, by its own id and without taking the machine's client anywhere, and
    /// the session that made is attached, with what happened said when the app is back.
    #[test]
    fn a_closed_worktree_is_opened_on_its_machine_and_the_session_it_made_is_attached() {
        for (reply, note) in [
            (OPENED.to_string(), actions::opened_note(false, "feat/rain")),
            (
                OPENED.replace("false", "true"),
                actions::opened_note(true, "feat/rain"),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, log) = stand_in_ssh(dir.path(), &paths, &reply, 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);

            let went = feed
                .go_by_route(
                    &mut app,
                    &mut Unlent,
                    Route::OpenFirst {
                        machine: "studio".into(),
                        workspace: "w5".into(),
                    },
                )
                .unwrap();

            assert_eq!(went, Went::Attached);
            let calls = ssh_calls(&log);
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(
                remote_of(&calls[0]).ends_with("'worktree' 'open' '--workspace' 'w5' '--no-focus'"),
                "{:?}",
                calls[0]
            );
            assert!(
                remote_of(&calls[1]).contains("'attach' '$9' '--holder'"),
                "{:?}",
                calls[1]
            );
            assert_eq!(toast_of(&app).as_deref(), Some(note.as_str()));
        }
    }

    /// A screen that logs what is done to it, in order, with the number of ssh calls the stand-in had logged
    /// when it was done.
    struct Recording {
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        ssh_log: PathBuf,
    }

    impl Recording {
        fn note(&self, what: &str) {
            let calls = ssh_calls(&self.ssh_log).len();
            self.log.borrow_mut().push(format!("{what} {calls}"));
        }
    }

    impl Lending for Recording {
        fn away(&mut self) {
            self.note("away");
        }
        fn back(&mut self) -> anyhow::Result<()> {
            self.note("back");
            Ok(())
        }
        fn drain(&mut self) {
            self.note("drain");
        }
        fn loading(&mut self, _theme: &crate::tui::theme::Theme, words: &str) {
            self.note(&format!("loading ({words})"));
        }
        fn list(&mut self, _app: &mut App) {
            self.note("list");
        }
    }

    /// Whatever was typed while the open waited is dropped before anything else is done with the screen, on the
    /// way to the attach and on the way to a refusal alike: a second Enter must reach neither the far tmux nor
    /// the same open again.
    #[test]
    fn what_was_typed_while_the_open_waited_is_dropped_before_the_screen_is_lent() {
        for (reply, attached) in [
            (OPENED, true),
            (
                r#"{"id":1,"error":{"code":"not_found","message":"gone"}}"#,
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, log) = stand_in_ssh(dir.path(), &paths, reply, 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut screen = Recording {
                log: seen.clone(),
                ssh_log: log,
            };

            feed.go_by_route(
                &mut app,
                &mut screen,
                Route::OpenFirst {
                    machine: "studio".into(),
                    workspace: "w5".into(),
                },
            )
            .unwrap();

            let seen = seen.borrow().clone();
            // The frame that says what is waited for comes before the ssh (no call yet), the open was one call,
            // and the drain came right after it, before the attach started.
            assert_eq!(
                seen[0],
                format!("loading ({}) 0", words::opening_worktree("studio"))
            );
            assert_eq!(seen[1], "drain 1", "{seen:?}");
            if attached {
                // The attach says `loading` again before it lends the screen.
                assert_eq!(seen[2], "loading (loading) 1", "{seen:?}");
                assert_eq!(seen[3], "away 1", "{seen:?}");
                assert!(seen.iter().any(|what| what.starts_with("back")), "{seen:?}");
            } else {
                assert_eq!(seen.len(), 2, "a refusal lends nothing: {seen:?}");
            }
        }
    }

    /// Going into a machine says `loading` before the first ssh call, and coming back says it as soon as the screen
    /// is taken back, before the wait for the machine's line, which a machine's client still makes (counted).
    #[test]
    fn a_machine_is_gone_into_and_come_back_from_under_the_loading_frame() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, OPENED, 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut screen = Recording {
            log: seen.clone(),
            ssh_log: log,
        };
        crate::tui::app_screen::LEFT_SLEEPS.with(|n| n.set(0));

        feed.go_by_route(
            &mut app,
            &mut screen,
            Route::Attach {
                machine: "studio".into(),
                target: "$9".into(),
                pane: None,
            },
        )
        .unwrap();

        let seen = seen.borrow().clone();
        assert_eq!(seen[0], "loading (loading) 0", "{seen:?}");
        assert_eq!(seen[1], "away 0", "{seen:?}");
        let back = seen.iter().position(|w| w.starts_with("back")).unwrap();
        assert_eq!(seen[back + 1], "loading (loading) 1", "{seen:?}");
        assert!(
            crate::tui::app_screen::LEFT_SLEEPS.with(|n| n.get()) > 0,
            "a machine's client keeps its wait for the line"
        );
        // The keys typed meanwhile are dropped right before the list's first draw.
        let tail: Vec<&str> = seen[seen.len() - 2..]
            .iter()
            .map(|w| w.split(' ').next().unwrap())
            .collect();
        assert_eq!(tail, ["drain", "list"], "{seen:?}");
    }

    /// An app that must end (its supervisor is gone) starts no ssh, draws nothing and lends nothing, on the way
    /// into a machine and on the way through an open: the positive half is the same call with a living
    /// supervisor, which runs the stand-in.
    #[test]
    fn an_app_that_must_end_goes_into_no_machine_and_lends_nothing() {
        let route_of = |open: bool| {
            if open {
                Route::OpenFirst {
                    machine: "studio".into(),
                    workspace: "w5".into(),
                }
            } else {
                Route::Attach {
                    machine: "studio".into(),
                    target: "%7".into(),
                    pane: Some("%7".into()),
                }
            }
        };
        for open in [false, true] {
            for living in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
                let cfg = Config::default();
                let paths = paths_in(dir.path());
                let (ssh, log) = stand_in_ssh(dir.path(), &paths, OPENED, 0);
                let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
                if !living {
                    // This process is never its own parent.
                    feed.record.record.supervisor = std::process::id();
                    assert!(
                        feed.record.orphaned(),
                        "the premise: the supervisor is gone"
                    );
                }
                let mut app = app_over(&cfg);
                let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                let mut screen = Recording {
                    log: seen.clone(),
                    ssh_log: log.clone(),
                };

                let went = feed
                    .go_by_route(&mut app, &mut screen, route_of(open))
                    .unwrap();

                assert_eq!(went, Went::Attached, "open {open} living {living}");
                let (calls, seen) = (ssh_calls(&log), seen.borrow().clone());
                if living {
                    assert!(!calls.is_empty(), "open {open}: the stand-in ran");
                    assert!(!seen.is_empty(), "open {open}: the screen was lent");
                } else {
                    assert!(calls.is_empty(), "open {open}: no ssh: {calls:?}");
                    assert!(
                        seen.is_empty(),
                        "open {open}: nothing lent or drawn: {seen:?}"
                    );
                    assert_eq!(toast_of(&app), None, "open {open}");
                }
            }
        }
    }

    /// The wait for an open ends when the supervisor is gone, as it does for a signal: the ssh is ended by its pid
    /// long before the bound.
    #[test]
    fn the_wait_for_an_open_ends_when_the_supervisor_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let hung = dir.path().join("ssh-hung");
        std::fs::write(&hung, "#!/bin/sh\nexec sleep 60\n").unwrap();
        crate::test_exec::make_executable(&hung);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &hung, true, true);
        feed.record.record.supervisor = std::process::id();
        assert!(
            feed.record.orphaned(),
            "the premise: the supervisor is gone"
        );

        let began = Instant::now();
        let ended = feed.open_worktree_on(&studio(), "w5", Duration::from_secs(20));

        assert_eq!(ended, Err(Stop::Ended));
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "ended by the gone supervisor, not by the bound: {:?}",
            began.elapsed()
        );
    }

    /// What was typed during an open is dropped, unless the open ended because the app must end (the terminal is the
    /// shell's then, as in `lend_through`). The decision is tested as it is made; the positive halves are the
    /// refusal and the made session, which drain (the test above also pins that the route calls it, and the test
    /// below pins that it does not when the open ends mid-wait).
    #[test]
    fn an_open_that_ends_because_the_app_must_end_drains_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ssh, log) = stand_in_ssh(dir.path(), &paths_in(dir.path()), "", 0);
        let _ = ssh;
        for (opened, drains) in [
            (Err(Stop::Said("no".to_string())), true),
            (
                Ok(Opened {
                    session: "$9".into(),
                    note: String::new(),
                }),
                true,
            ),
            (Err(Stop::Ended), false),
        ] {
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut screen = Recording {
                log: seen.clone(),
                ssh_log: log.clone(),
            };
            drain_after_open(&mut screen, &opened);
            let seen = seen.borrow().clone();
            assert_eq!(
                seen.iter().any(|what| what.starts_with("drain")),
                drains,
                "{opened:?}: {seen:?}"
            );
        }
    }

    /// Sets the seam of `must_end` for a stretch of a test and puts the product's own answer back when it goes out of
    /// scope, a panicking assert included.
    struct EndSeam;

    impl EndSeam {
        fn after(n: Option<usize>) -> EndSeam {
            super::super::end_after_checks(n);
            EndSeam
        }
    }

    impl Drop for EndSeam {
        fn drop(&mut self) {
            super::super::end_after_checks(None);
        }
    }

    /// The route's own call: an open that ends mid-wait because the app must end (the seam of `must_end`, whose
    /// second check is the wait's first look) drains nothing, says nothing and lends nothing, and goes nowhere; the
    /// positive half is the same route with an open that is refused, which does drain (the ssh that hangs is ended
    /// by its pid, as the app ends it).
    #[test]
    fn an_open_that_ends_mid_wait_through_the_route_drains_nothing() {
        for ends in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (refusing, log) = stand_in_ssh(
                dir.path(),
                &paths,
                r#"{"id":1,"error":{"code":"not_found","message":"gone"}}"#,
                0,
            );
            let hung = dir.path().join("ssh-hung");
            std::fs::write(&hung, "#!/bin/sh\nexec sleep 60\n").unwrap();
            crate::test_exec::make_executable(&hung);
            let ssh = if ends { &hung } else { &refusing };
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, ssh, true, true);
            let mut app = app_over(&cfg);
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut screen = Recording {
                log: seen.clone(),
                ssh_log: log.clone(),
            };
            // The first check is the route's own, before the open; the second is the first look of the wait.
            let went = {
                let _seam = EndSeam::after(ends.then_some(2));
                feed.go_by_route(
                    &mut app,
                    &mut screen,
                    Route::OpenFirst {
                        machine: "studio".into(),
                        workspace: "w5".into(),
                    },
                )
                .unwrap()
            };

            assert_eq!(
                went,
                if ends { Went::Attached } else { Went::Refused },
                "ends {ends}"
            );
            let seen = seen.borrow().clone();
            let drained = seen.iter().any(|what| what.starts_with("drain"));
            assert_eq!(drained, !ends, "ends {ends}: {seen:?}");
            if ends {
                assert!(
                    seen.iter().all(|what| what.starts_with("loading")),
                    "ends: nothing lent, only the frame that said it waits: {seen:?}"
                );
            } else {
                assert!(
                    toast_of(&app).is_some_and(|said| said.contains("gone")),
                    "the refusal is said: {:?}",
                    toast_of(&app)
                );
            }
        }
    }

    /// An open that was stopped because the app must end says nothing and goes nowhere (no toast, nothing lent);
    /// a refusal says its words, and a session that was made is attached.
    #[test]
    fn an_open_that_was_stopped_by_the_end_of_the_app_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut screen = Recording {
            log: seen.clone(),
            ssh_log: log.clone(),
        };

        // Positive halves: a refusal says its words, and an open that made a session attaches it.
        let mut app = app_over(&cfg);
        let went = feed
            .after_open(
                &mut app,
                &mut screen,
                "studio",
                Err(Stop::Said("no".into())),
            )
            .unwrap();
        assert_eq!(
            (went, toast_of(&app).as_deref()),
            (Went::Refused, Some("no"))
        );
        let opened = Opened {
            session: "$9".into(),
            note: "opened".into(),
        };
        let went = feed
            .after_open(&mut app, &mut screen, "studio", Ok(opened))
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert_eq!(ssh_calls(&log).len(), 1, "the session was attached");

        let (calls, lent) = (ssh_calls(&log).len(), seen.borrow().len());
        let mut app = app_over(&cfg);
        let went = feed
            .after_open(&mut app, &mut screen, "studio", Err(Stop::Ended))
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert_eq!(toast_of(&app), None, "nothing is said");
        assert_eq!(ssh_calls(&log).len(), calls, "nothing run");
        assert_eq!(seen.borrow().len(), lent, "nothing lent");
    }

    /// A reply cut in the middle of its line when the ssh ended, whose rest comes 0.8 s later (past the old 500 ms
    /// bound, with 1.2 s to spare under the 2 s one) through a process that holds the pipe, is read whole: the wait goes on
    /// while what came does not end in a newline.
    #[test]
    fn a_reply_cut_before_its_newline_is_waited_for_a_moment() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, _log) = stand_in_ssh(dir.path(), &paths, OPENED, 0);
        let (head, rest) = OPENED.split_at(OPENED.len() / 2);
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nprintf '%s' '{head}'\n( sleep 0.8; printf '%s\\n' '{rest}' ) &\nexit 0\n"
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&ssh);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);

        let opened = feed.open_worktree_on(&studio(), "w5", Duration::from_secs(20));

        assert_eq!(opened.map(|opened| opened.session), Ok("$9".to_string()));
    }

    /// A process the ssh left behind that keeps its standard output open (`sleep 5 &` without a redirect) does not
    /// take the reply: it is read as it comes, the attach follows, and the call does not wait for the process.
    #[test]
    fn a_reply_is_read_although_something_the_ssh_started_holds_its_output_open() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, OPENED, 0);
        // The reply line is printed, a background process inherits the pipe, and the ssh ends 0. The sleep ends by
        // itself.
        let script = std::fs::read_to_string(&ssh).unwrap();
        assert!(script.contains("exit 0;; esac"), "the stand-in's shape");
        std::fs::write(
            &ssh,
            script.replace("exit 0;; esac", "sleep 5 &\nexit 0;; esac"),
        )
        .unwrap();
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);

        let began = Instant::now();
        let opened = feed.open_worktree_on(&studio(), "w5", Duration::from_secs(20));
        let took = began.elapsed();

        assert_eq!(opened.map(|opened| opened.session), Ok("$9".to_string()));
        assert!(
            took < Duration::from_secs(3),
            "the call did not wait for the process the ssh left: {took:?}"
        );
        assert_eq!(ssh_calls(&log).len(), 1, "one call, the open");
    }

    /// A worktree the machine cannot open (its error, a reply that is no envelope, an ssh that did not answer,
    /// an id that is no workspace's) says so and attaches nothing.
    #[test]
    fn a_worktree_that_does_not_open_says_why_and_attaches_nothing() {
        let error = r#"{"id":1,"error":{"code":"not_found","message":"no such worktree"}}"#;
        for (reply, workspace, expected) in [
            (
                error,
                "w5",
                words::worktree_open_failed("studio", "no such worktree"),
            ),
            (
                "not json at all",
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            // An older taw there says no `session_id`, and one that says an id that is no session's is not
            // believed: nothing is attached, and the record's own id is never taken for the session.
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":null}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":"%3"}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":"$x"}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":"$"}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (
                r#"{"id":1,"result":{"worktree":{"workspace_id":"w5","branch":"feat/rain"},"already_open":false,"session_id":9}}"#,
                "w5",
                words::worktree_open_unreadable("studio"),
            ),
            (OPENED, "w5; ls", words::NOTHING_TO_GO_IN_TO.to_string()),
            (OPENED, "$5", words::NOTHING_TO_GO_IN_TO.to_string()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, log) = stand_in_ssh(dir.path(), &paths, reply, 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);

            let went = feed
                .go_by_route(
                    &mut app,
                    &mut Unlent,
                    Route::OpenFirst {
                        machine: "studio".into(),
                        workspace: workspace.into(),
                    },
                )
                .unwrap();

            assert_eq!(went, Went::Refused, "{reply} {workspace}");
            assert_eq!(toast_of(&app), Some(expected), "{reply} {workspace}");
            let calls = ssh_calls(&log);
            assert!(
                calls
                    .iter()
                    .all(|call| !remote_of(call).contains("'attach'")),
                "no attach: {calls:?}"
            );
        }
    }

    /// A machine that does not answer within the bound is ended by its pid and its words say so: a hung machine
    /// does not hang the app.
    #[test]
    fn a_machine_that_does_not_open_the_worktree_in_time_has_its_ssh_ended() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let pid_file = dir.path().join("hung.pid");
        let hung = dir.path().join("ssh-hung");
        std::fs::write(
            &hung,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
                pid_file.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&hung);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &hung, true, true);

        let began = Instant::now();
        let err = feed
            .open_worktree_on(&studio(), "w5", Duration::from_secs(3))
            .unwrap_err();

        assert_eq!(err, Stop::Said(words::worktree_open_timed_out("studio", 3)));
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .expect("the stand-in ran")
            .trim()
            .parse()
            .unwrap();
        // The pid was a child of this test, reaped by the code under test: its guard leaves a reaped pid alone.
        let _guard = KillOnDrop(pid);
        wait_until_not_running(pid, "the hung ssh");
    }

    /// Back goes into the machine's pane when the pane is a machine's, and a machine that cannot be gone into is
    /// told and forgotten, so the next Back ends the app.
    #[test]
    fn back_into_a_machines_pane_goes_through_the_machine_and_forgets_one_that_is_away() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        feed.show(&tmux, &mut app);
        app.enter_app_mode(Some("studio:%7".into()), None);
        assert_eq!(
            app.came_from(),
            Some("studio:%7"),
            "the pane is on the list"
        );

        feed.go_back_in(&mut app, &mut Unlent, "studio:%7").unwrap();

        let calls = ssh_calls(&log);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(remote_of(&calls[0]).contains("'attach' '%7' '--holder'"));
        assert_eq!(
            app.came_from(),
            Some("studio:%7"),
            "kept after a good attach"
        );

        // A machine that is not one of `[machines]` any more.
        app.toast = None;
        feed.go_back_in(&mut app, &mut Unlent, "ghost:%7").unwrap();
        assert_eq!(toast_of(&app), Some(words::no_such_machine("ghost")));
        assert_eq!(app.came_from(), None);

        // A machine that is away.
        drop(feed);
        drop(keeper);
        let (mut away, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, false, true);
        let mut app = app_over(&cfg);
        away.show(&tmux, &mut app);
        app.enter_app_mode(Some("studio:%7".into()), None);
        assert_eq!(app.came_from(), Some("studio:%7"));
        away.go_back_in(&mut app, &mut Unlent, "studio:%7").unwrap();
        assert_eq!(toast_of(&app), Some(words::not_connected("studio", 125)));
        assert_eq!(app.came_from(), None);
        assert_eq!(ssh_calls(&log).len(), 1, "the away machine was not run");
    }

    // ------------------------------------------------------------------ the next stop and the line that says go

    /// How many times the list was read from this Mac's tmux: what shows it again after an attach.
    fn list_reads(log: &Path) -> usize {
        crate::cli::test_support::calls(log)
            .iter()
            .filter(|call| call.iter().any(|word| word == "list-panes"))
            .count()
    }

    /// The line a client of studio's leaves when its key walked on: addressed to this app, naming the pane it was
    /// in (`%7`) and the stop to go to.
    fn go_line(feed: &Feed, go: &str) -> Left {
        Left {
            holder: feed.record.holder().to_string(),
            pane: "%7".into(),
            go: Some(go.to_string()),
        }
    }

    /// The attach calls of `log`, as the pane each attached.
    fn attached_panes(log: &Path) -> Vec<String> {
        ssh_calls(log)
            .iter()
            .filter(|call| remote_of(call).contains("'attach'"))
            .map(|call| {
                let remote = remote_of(call);
                let after = remote.split("'attach' '").nth(1).unwrap_or_default();
                after.split('\'').next().unwrap_or_default().to_string()
            })
            .collect()
    }

    /// A line that says go is acted on at once: the machine's second attach follows the first, and the list is
    /// read from this Mac's tmux once, after the last, as it is after a single attach. The line is acted on
    /// once: the chain ends where no line says go, with the cursor on the pane the last attach named.
    #[test]
    fn a_left_line_with_go_is_acted_on_once_and_at_once_with_no_list_between() {
        // The baseline: one attach with no line reads the list once.
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, _) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        feed.go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
            .unwrap();
        let one_read = list_reads(&log);
        assert!(one_read > 0, "a list read is what the count looks for");

        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let paths = paths_in(dir.path());
        let (ssh, ssh_log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let line = go_line(&feed, "studio:%9");
        Machines::announce_on_poll(2, 0, line);
        let went = feed
            .go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert_eq!(attached_panes(&ssh_log), ["%7", "%9"], "gone on at once");
        assert_eq!(
            list_reads(&log),
            one_read,
            "the list was read once, after the last attach"
        );
        assert!(feed.lefts.is_empty(), "the line was taken");
        assert_eq!(app.came_from(), Some("studio:%9"));
    }

    /// A `go` the app cannot act on falls back to the list with the usual words and the cursor on the line's pane:
    /// a machine that is not in `[machines]`, an id that is no id, and a bare id that is no pane.
    #[test]
    fn a_go_that_cannot_be_acted_on_shows_the_list_with_the_usual_words_and_the_cursor_on_the_lines_pane()
     {
        for (go, words) in [
            ("pi:%3", words::no_such_machine("pi")),
            ("studio:garbage", words::NOTHING_TO_GO_IN_TO.to_string()),
            ("garbage", words::NOTHING_TO_GO_IN_TO.to_string()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, ssh_log) = stand_in_ssh(dir.path(), &paths, "", 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);
            let line = go_line(&feed, go);
            Machines::announce_on_poll(2, 0, line);
            feed.go_into_machine(&mut app, &mut Unlent, "studio", "%7", Some("%7"), None)
                .unwrap();
            assert_eq!(
                attached_panes(&ssh_log),
                ["%7"],
                "the first attach ran ({go})"
            );
            assert_eq!(toast_of(&app).as_deref(), Some(words.as_str()), "{go}");
            assert_eq!(app.came_from(), Some("studio:%7"), "the line's pane ({go})");
        }
    }

    /// A hop to a machine that is not listed, after an attach, is refused with the list drawn at once: the drain and
    /// the list are the screen's only events, and the list was read once, by the refusal.
    #[test]
    fn a_hop_refused_after_an_attach_draws_the_list_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, ssh_log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut screen = Recording {
            log: seen.clone(),
            ssh_log,
        };
        let before = list_reads(&log);
        feed.follow_go(
            &mut app,
            &mut screen,
            Step::Go {
                go: "pi:%3".into(),
                pane: Some("studio:%7".into()),
            },
        )
        .unwrap();
        let events: Vec<String> = seen
            .borrow()
            .iter()
            .map(|e| e.split_whitespace().next().unwrap_or("").to_string())
            .collect();
        assert_eq!(events, ["drain", "list"], "{:?}", seen.borrow());
        assert_eq!(list_reads(&log) - before, 1, "one read, the refusal's");
        assert_eq!(
            toast_of(&app).as_deref(),
            Some(words::no_such_machine("pi").as_str())
        );
    }

    /// A hop of this Mac's looks at whether the app must end before it does anything: with the app made to end at
    /// that look no tmux call is made (no read of the session's id, no `select-pane`) and the screen is not lent; the positive
    /// half is the same hop with the app not ending, which does both.
    #[test]
    fn a_hop_to_a_pane_of_this_mac_looks_at_the_end_before_it_lends_the_terminal() {
        for ends in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
            let cfg = Config::default();
            let paths = paths_in(dir.path());
            let (ssh, ssh_log) = stand_in_ssh(dir.path(), &paths, "", 0);
            let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
            let mut app = app_over(&cfg);
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut screen = Recording {
                log: seen.clone(),
                ssh_log,
            };
            {
                let _seam = EndSeam::after(ends.then_some(1));
                feed.follow_go(
                    &mut app,
                    &mut screen,
                    Step::Go {
                        go: "%341".into(),
                        pane: None,
                    },
                )
                .unwrap();
            }
            let tmux_calls = crate::cli::test_support::calls(&log);
            let names: Vec<&str> = tmux_calls.iter().map(|call| call[0].as_str()).collect();
            let touched = |word: &str| names.contains(&word);
            let lent = seen.borrow().iter().any(|what| what.starts_with("away"));
            assert_eq!(touched("select-pane"), !ends, "ends {ends}: {names:?}");
            // Going in asks whether the session is there (and its id) in one `display-message`.
            let asked = tmux_calls
                .iter()
                .any(|call| call.last().is_some_and(|word| word == "#{session_id}"));
            assert_eq!(asked, !ends, "ends {ends}: {names:?}");
            assert_eq!(lent, !ends, "ends {ends}: {:?}", seen.borrow());
        }
    }

    /// An app over this Mac's sample world and one blocked agent of studio's, `%8`.
    fn app_over_studio_agent<'a>(cfg: &'a Config) -> App<'a> {
        use crate::machine::MachineView;
        use crate::machine::wire::WireAgent;
        let view = MachineView {
            name: "studio".into(),
            live: true,
            last_seen: Some(5_000),
            state: Some(State {
                spaces: vec![WireSpace {
                    session_id: "$8".into(),
                    ..WireSpace::default()
                }],
                panes: vec![WirePane {
                    pane_id: "%8".into(),
                    session_id: "$8".into(),
                    window_id: "@8".into(),
                    window_index: 1,
                    pane_index: 1,
                    ..WirePane::default()
                }],
                agents: vec![WireAgent {
                    pane_id: "%8".into(),
                    session_id: "$8".into(),
                    window_id: "@8".into(),
                    window_index: 1,
                    pane_index: 1,
                    state: Some("blocked".into()),
                    kind: Some("claude".into()),
                    ..WireAgent::default()
                }],
                worktrees: Vec::new(),
                open_worktrees: Vec::new(),
            }),
            ..MachineView::default()
        };
        let theme = theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;
        App::new(
            cfg,
            theme,
            String::new(),
            crate::tui::machines::merge(crate::tui::fixtures::snapshot(), &[view], 5_000),
        )
    }

    /// The chord in the app: from this Mac's last stop the next is studio's, which goes through the machine's
    /// attach by the pane's own id, and from a pane that is not a stop it is the first stop after it.
    #[test]
    fn the_chord_goes_to_the_next_stop_on_a_machine_through_its_attach() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, ssh_log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over_studio_agent(&cfg);
        let chord = |own: &str| Action::NextUp {
            own: Some(own.to_string()),
        };

        feed.perform(Path::new("taw"), &mut app, &mut Unlent, &chord("%341"))
            .unwrap();
        assert_eq!(
            attached_panes(&ssh_log),
            ["%8"],
            "studio's agent is next after this Mac's last"
        );

        // From a shell drawn between this Mac's two stops, the next is %341: this Mac's, so no ssh runs. The list
        // after the first attach was read from the stand-in, which lists nothing: a world of its own again.
        let mut app = app_over_studio_agent(&cfg);
        feed.perform(Path::new("taw"), &mut app, &mut Unlent, &chord("%311"))
            .unwrap();
        assert_eq!(
            attached_panes(&ssh_log),
            ["%8"],
            "a stop of this Mac is not gone to over ssh"
        );
        // It was gone to through this Mac's tmux, at the stop after %311 (%341), not at the walk's first (%231).
        let selected: Vec<String> = crate::cli::test_support::calls(&log)
            .into_iter()
            .filter(|call| call[0] == "select-pane")
            .map(|call| call[2].clone())
            .collect();
        assert!(selected.contains(&"%341".to_string()), "{selected:?}");
        assert!(!selected.contains(&"%231".to_string()), "{selected:?}");
    }

    // ---- `taw app --go machine:id`: the landing before the first frame ----

    /// `--go` takes three forms and nothing else: `last`, a pane of this Mac (`%7`) and a machine with one of its
    /// ids; a bare session or window id names no pane and is refused with the usage words, as is everything else.
    /// A machine's id that is no id is left to the landing, which says so in the list.
    #[test]
    fn go_takes_last_a_pane_of_this_mac_and_a_machine_with_an_id_and_nothing_else() {
        for id in ["%16", "$3", "@5", "7", "w5", "%x"] {
            let target = GoTarget::parse(&format!("studio:{id}")).unwrap();
            assert_eq!(target, self::target("studio", id));
        }
        assert_eq!(GoTarget::parse("last"), Ok(GoTarget::Last));
        assert_eq!(GoTarget::parse("%7"), Ok(GoTarget::Pane("%7".into())));
        assert_eq!(GoTarget::parse("%0"), Ok(GoTarget::Pane("%0".into())));
        for bad in [
            "$3", "@5", "", ":%7", "studio", "7", "%", "%x", "%7 ", "Last", "last ", "-1",
        ] {
            assert_eq!(GoTarget::parse(bad), Err(words::GO_USAGE), "{bad:?}");
        }
    }

    fn target(machine: &str, id: &str) -> GoTarget {
        GoTarget::Machine {
            machine: machine.into(),
            id: id.into(),
        }
    }

    /// A pane, a session and a window of a live machine are each attached by the machine's own id, with this
    /// app's holder, and the list is what comes back.
    #[test]
    fn going_first_attaches_a_pane_a_session_and_a_window_by_their_own_ids() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let holder = feed.record.holder().to_string();
        for id in ["%7", "$3", "@5"] {
            let went = feed
                .go_first(&mut app, &mut Unlent, &target("studio", id), GO_WAIT)
                .unwrap();
            assert_eq!(went, Went::Attached, "{id}");
        }
        let calls = ssh_calls(&log);
        assert_eq!(calls.len(), 3, "{calls:?}");
        for (call, id) in calls.iter().zip(["%7", "$3", "@5"]) {
            assert!(call.contains(&"-t".to_string()), "{call:?}");
            let remote = remote_of(call);
            assert!(
                remote.contains(&format!("'machine' 'attach' '{id}' '--holder' '{holder}'")),
                "{remote}"
            );
        }
        assert_eq!(toast_of(&app), None);
    }

    /// A live machine whose state has no pane, session or window of the id is `<machine>:<id> is gone`, with the
    /// list and nothing run (as a gone pane of this Mac is); an id its state holds is attached, and a machine that
    /// is not live says why it is not, not that the id is gone.
    #[test]
    fn going_first_to_an_id_the_live_state_lacks_says_it_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        for id in ["%99", "$9", "@9"] {
            app.toast = None;
            let went = feed
                .go_first(&mut app, &mut Unlent, &target("studio", id), GO_WAIT)
                .unwrap();
            assert_eq!(went, Went::Refused, "{id}");
            assert_eq!(
                toast_of(&app).as_deref(),
                Some(format!("studio:{id} is gone").as_str())
            );
        }
        assert_eq!(ssh_calls(&log).len(), 0, "nothing gone was attached");
        let went = feed
            .go_first(&mut app, &mut Unlent, &target("studio", "%9"), GO_WAIT)
            .unwrap();
        assert_eq!(went, Went::Attached, "an id the state holds goes on");
        assert_eq!(ssh_calls(&log).len(), 1);
        drop(feed);
        drop(keeper);

        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, false, true);
        app.toast = None;
        let went = feed
            .go_first(
                &mut app,
                &mut Unlent,
                &target("studio", "%99"),
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(went, Went::Refused);
        let said = toast_of(&app).unwrap();
        assert!(
            !said.contains("is gone") && said.contains("studio"),
            "a machine that is not live says it is not: {said}"
        );
    }

    /// A machine that is live but has sent no state within the wait is attached all the same, after the whole
    /// wait (the state was waited for), and not said gone: with no state there is nothing to say an id lacks.
    #[test]
    fn going_first_to_a_live_machine_with_no_state_attaches_once_the_wait_is_over() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let now = files::now_ms();
        files::write_machine_file(
            &paths,
            &MachineFile {
                name: "studio".into(),
                state: None,
                received_at: None,
                last_seen: Some(now),
                live: true,
                words: "connected: taw 0.1.0".into(),
                ..MachineFile::default()
            },
        );
        // The follower reads the file again within a moment: waited for, to see the machine without its state.
        let until = Instant::now() + Duration::from_secs(20);
        while feed.has_state("studio") {
            assert!(
                Instant::now() < until,
                "the feed never took the machine's state away"
            );
            std::thread::sleep(Duration::from_millis(20));
            feed.hub.poll(Instant::now());
        }
        assert!(
            feed.machine_to_enter("studio").is_ok(),
            "positive half: the machine is live and has no state"
        );
        /// Notes when the wait was over: the drain is the first thing after it.
        struct DrainedAt(Option<Instant>);
        impl crate::tui::Lending for DrainedAt {
            fn away(&mut self) {}
            fn back(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
            fn drain(&mut self) {
                self.0.get_or_insert_with(Instant::now);
            }
        }
        let mut app = app_over(&cfg);
        let mut screen = DrainedAt(None);
        let started = Instant::now();
        let went = feed
            .go_first(
                &mut app,
                &mut screen,
                &target("studio", "%99"),
                Duration::from_millis(600),
            )
            .unwrap();
        assert_eq!(went, Went::Attached, "{:?}", toast_of(&app));
        let waited = screen
            .0
            .expect("the screen was drained")
            .duration_since(started);
        assert!(
            waited >= Duration::from_millis(600),
            "the state was waited for the whole wait: {waited:?}"
        );
        assert_eq!(toast_of(&app), None);
        let calls = ssh_calls(&log);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            remote_of(&calls[0]).contains("'machine' 'attach' '%99'"),
            "{calls:?}"
        );
    }

    /// A machine that is not live yet (a keeper has just started its connections) is waited for, with nothing
    /// drawn, and attached the moment it is; one that is still not live when the wait is over is not run and
    /// says why.
    #[test]
    fn going_first_waits_for_a_machine_that_is_not_live_yet() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, false, true);
        let mut app = app_over(&cfg);

        // Not live for the whole short wait: said, and not run. The wait itself is the positive half.
        let started = Instant::now();
        let went = feed
            .go_first(
                &mut app,
                &mut Unlent,
                &target("studio", "%7"),
                Duration::from_millis(600),
            )
            .unwrap();
        assert_eq!(went, Went::Refused);
        assert!(
            started.elapsed() >= Duration::from_millis(600),
            "the wait was cut short: {:?}",
            started.elapsed()
        );
        let said = toast_of(&app).unwrap_or_default();
        assert!(said.contains("studio is not connected"), "{said}");
        assert_eq!(ssh_calls(&log).len(), 0, "an away machine was not run");

        // Live a moment into the wait: attached, without waiting out the rest.
        let writer = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                let now = files::now_ms();
                files::write_machine_file(
                    &paths,
                    &MachineFile {
                        name: "studio".into(),
                        state: Some(studio_state()),
                        received_at: Some(now),
                        last_seen: Some(now),
                        live: true,
                        words: "connected: taw 0.1.0".into(),
                        ..MachineFile::default()
                    },
                );
            })
        };
        let started = Instant::now();
        let went = feed
            .go_first(
                &mut app,
                &mut Unlent,
                &target("studio", "%7"),
                Duration::from_secs(15),
            )
            .unwrap();
        writer.join().unwrap();
        assert_eq!(went, Went::Attached, "{:?}", toast_of(&app));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            ssh_calls(&log).len(),
            1,
            "the machine was gone into once it was live"
        );
    }

    /// A machine that is not one of `[machines]` and an id that is no id show the list with their words at once,
    /// whatever the wait: nothing is waited for that could not come.
    #[test]
    fn going_first_to_no_machine_or_no_id_shows_the_list_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, false, true);
        let mut app = app_over(&cfg);
        let wait = Duration::from_secs(30);
        for (machine, id, words) in [
            ("ghost", "%7", words::no_such_machine("ghost")),
            ("studio", "7", words::NOTHING_TO_GO_IN_TO.to_string()),
            ("studio", "%7;ls", words::NOTHING_TO_GO_IN_TO.to_string()),
        ] {
            app.toast = None;
            let started = Instant::now();
            let went = feed
                .go_first(&mut app, &mut Unlent, &target(machine, id), wait)
                .unwrap();
            assert_eq!(went, Went::Refused, "{machine} {id}");
            assert_eq!(toast_of(&app), Some(words), "{machine} {id}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{machine} {id}: waited {:?}",
                started.elapsed()
            );
        }
        assert_eq!(ssh_calls(&log).len(), 0, "nothing refused was run");
    }

    fn recording(log: &Path) -> (Recording, std::rc::Rc<std::cell::RefCell<Vec<String>>>) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        (
            Recording {
                log: seen.clone(),
                ssh_log: log.to_path_buf(),
            },
            seen,
        )
    }

    /// `--go %7` on this Mac attaches the pane's own session after selecting the pane, as a jump does, and the
    /// list is what comes back; a pane tmux no longer has is the toast `%7 is gone`, with nothing attached and
    /// the terminal never lent. A target that is not a pane or a session of this Mac is the usage words.
    #[test]
    fn going_first_on_this_mac_attaches_a_pane_and_says_a_gone_one_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let mut feed = feed_over(&tmux, &cfg, &paths);
        let mut app = app_over(&cfg);
        let went = feed
            .go_first(&mut app, &mut Unlent, &GoTarget::Pane("%7".into()), GO_WAIT)
            .unwrap();
        assert_eq!(went, Went::Attached);
        let calls = crate::cli::test_support::calls(&log);
        let verb = |name: &str| calls.iter().filter(|c| c[0] == name).collect::<Vec<_>>();
        assert_eq!(verb("attach-session").len(), 1, "{calls:?}");
        assert_eq!(verb("attach-session")[0][1..3], ["-t", "$3"], "{calls:?}");
        assert!(
            verb("select-pane")
                .iter()
                .any(|c| c.contains(&"%7".to_string())),
            "the pane was selected: {calls:?}"
        );

        // A pane that is not there: the toast, and nothing attached. A stub that fails with words, and one that
        // does as real tmux does for `display-message -p -t %7` of a pane that is not there: an empty line, exit 0.
        for (n, answer) in [
            ("fails", "echo \"can't find pane\" >&2; exit 1"),
            ("empty", "echo; exit 0"),
        ] {
            let gone_log = dir.path().join(format!("gone-calls-{n}"));
            let script = dir.path().join(format!("tmux-without-the-pane-{n}"));
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\n[ \"$1\" = -u ] && shift\necho \"$1\" >> '{}'\ncase \"$1\" in display-message) {answer};; esac\nexit 0\n",
                    gone_log.display()
                ),
            )
            .unwrap();
            crate::test_exec::make_executable(&script);
            let tmux = crate::tmux::Tmux {
                bin: script,
                socket: None,
            };
            let mut feed = feed_over(&tmux, &cfg, &paths);
            let mut app = app_over(&cfg);
            let went = feed
                .go_first(&mut app, &mut Unlent, &GoTarget::Pane("%7".into()), GO_WAIT)
                .unwrap();
            assert_eq!(went, Went::Refused, "{n}");
            assert_eq!(toast_of(&app).as_deref(), Some("%7 is gone"), "{n}");
            let asked = std::fs::read_to_string(&gone_log).unwrap();
            assert!(
                asked.contains("display-message"),
                "{n}: the pane was asked for: {asked}"
            );
            assert!(
                !asked.contains("attach-session"),
                "{n}: nothing was attached: {asked}"
            );
        }
    }

    /// What was typed while the screen said `loading` during the wait for the machine is dropped before the terminal
    /// is lent (the drain is logged before the away), as the open path does; an app that must end drops nothing and
    /// lends nothing (the seam of `must_end`: the drain's look is its first check, since a live machine is not waited
    /// for).
    #[test]
    fn going_first_drops_what_was_typed_during_the_wait_before_the_terminal_is_lent() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let mut app = app_over(&cfg);
        let (mut screen, seen) = recording(&log);

        let went = feed
            .go_first(&mut app, &mut screen, &target("studio", "%7"), GO_WAIT)
            .unwrap();
        assert_eq!(went, Went::Attached);
        let seen_now = seen.borrow().clone();
        let at = |what: &str| seen_now.iter().position(|line| line.starts_with(what));
        assert_eq!(ssh_calls(&log).len(), 1, "the machine was gone into");
        assert!(
            at("drain").is_some() && at("drain") < at("away"),
            "the drain comes before the terminal is lent: {seen_now:?}"
        );
        assert_eq!(seen_now[0], "drain 0", "{seen_now:?}");

        // An app that must end: nothing dropped, nothing lent, nothing run.
        let before = ssh_calls(&log).len();
        let (mut screen, seen) = recording(&log);
        let _end = EndSeam::after(Some(1));
        let went = feed
            .go_first(&mut app, &mut screen, &target("studio", "%7"), GO_WAIT)
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert_eq!(seen.borrow().clone(), Vec::<String>::new());
        assert_eq!(
            ssh_calls(&log).len(),
            before,
            "an app that must end runs nothing"
        );
    }

    /// An app that must end while it waits for a machine that is not live stops waiting at once: nothing is run,
    /// said or lent. The positive half is the wait of `going_first_waits_for_a_machine_that_is_not_live_yet`,
    /// which without the seam is as long as it is given (here 30 s, the bound the assert below is far inside).
    #[test]
    fn going_first_stops_waiting_when_the_app_must_end() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, false, true);
        let mut app = app_over(&cfg);
        let (mut screen, seen) = recording(&log);
        let _end = EndSeam::after(Some(1));

        let started = Instant::now();
        let went = feed
            .go_first(
                &mut app,
                &mut screen,
                &target("studio", "%7"),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(went, Went::Attached);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait went on after the app had to end: {:?}",
            started.elapsed()
        );
        assert_eq!(toast_of(&app), None, "an app that ends says nothing");
        assert_eq!(ssh_calls(&log).len(), 0, "nothing was run");
        assert_eq!(seen.borrow().clone(), Vec::<String>::new());
    }

    /// The go is part of opening the app, not of its loop: the screen is entered, then lent (`away`) and given back
    /// (`back`), all before `open_going` returns the app, so `run_app`'s loop draws its first frame only after the
    /// go. (`run_app` draws through its own terminal handle, which this recording never sees: what is pinned is the
    /// order of the open itself, and that the machine was gone into once.)
    #[test]
    fn the_go_is_done_while_the_app_is_opened_after_its_screen_is_entered() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
        let cfg = Config::default();
        let paths = paths_in(dir.path());
        let (ssh, log) = stand_in_ssh(dir.path(), &paths, "", 0);
        let (mut feed, _keeper) = studio_feed(&tmux, &cfg, &paths, &ssh, true, true);
        let (screen, seen) = recording(&log);
        let theme = crate::tui::theme::resolve(&cfg.theme.name, &cfg.theme.custom).theme;

        let go = target("studio", "%7");
        let entered = seen.clone();
        let _ = super::super::open_going(
            &mut feed,
            &cfg,
            theme,
            String::new(),
            None,
            false,
            Some((&go, GO_WAIT)),
            || {
                entered.borrow_mut().push("enter".to_string());
                Ok(screen)
            },
        )
        .unwrap();

        let seen = seen.borrow().clone();
        let at = |what: &str| {
            seen.iter()
                .position(|line| line == what || line.starts_with(&format!("{what} ")))
        };
        let enter = at("enter").unwrap_or_else(|| panic!("the screen was entered: {seen:?}"));
        let away = at("away").unwrap_or_else(|| panic!("the terminal was lent: {seen:?}"));
        let back = at("back").unwrap_or_else(|| panic!("the terminal was given back: {seen:?}"));
        assert!(
            enter < away && away < back,
            "the screen is entered, lent, then given back: {seen:?}"
        );
        assert_eq!(ssh_calls(&log).len(), 1, "the machine was gone into");
    }
}

#[cfg(test)]
mod no_session_tests {
    use super::*;

    /// `studio:new` is the one word form of a machine's id: `--go` takes it, as the doorway and the row do.
    #[test]
    fn go_takes_a_machine_and_new() {
        assert_eq!(
            GoTarget::parse("studio:new"),
            Ok(GoTarget::Machine {
                machine: "studio".into(),
                id: "new".into()
            })
        );
        // `new` alone names no machine.
        assert_eq!(GoTarget::parse("new"), Err(words::GO_USAGE));
        assert!(valid_go_id("new") && !valid_go_id("old") && !valid_go_id("newer"));
    }

    /// The doorway on `new` (what Enter on the row of a machine with no session is) attaches a new session
    /// there, with no pane to come back to.
    #[test]
    fn the_row_of_a_machine_with_no_session_routes_to_a_new_attach() {
        let action = Action::Doorway {
            machine: "studio".into(),
            id: "new".into(),
        };
        assert_eq!(
            route(&action, |_| None),
            Some(Route::Attach {
                machine: "studio".into(),
                target: "new".into(),
                pane: None
            })
        );
        let other = Action::Doorway {
            machine: "studio".into(),
            id: "%3".into(),
        };
        assert_eq!(route(&other, |_| None), None);
    }
}
