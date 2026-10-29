//! The verbs the app runs on a machine's rows (close a pane, rename a window, remove a worktree), each on a
//! thread of its own so that a key and a frame never wait for a machine.
//!
//! One verb is in flight per machine at most: a second one for the machine is refused by [`RemoteActions::start`]
//! and nothing is sent. The verb is the far `taw` over the same ssh as the machines' other short verbs
//! ([`ssh::command`] with `Mode::Command`), run by [`bounded::run`] for at most its bound ([`crate::machine::actions::bound`]:
//! twenty seconds, and twenty for each session of a group closed together). It is never tried a second time:
//! a verb that timed out may have run over there, and the person looks again before trying once more.
//!
//! The answer comes back as a [`Reply`] ([`classify`]) through [`RemoteActions::poll`], which the app calls
//! every tick, with the `token` the caller gave the verb so that it can tell which action answered (an
//! answer for a row that moved away is the caller's to drop). The hint that says `waiting for studio` after two
//! seconds is read off the clock the app gives ([`App::wall_ms`] has a fixed one under test); the end of a verb
//! whose thread has not answered a few seconds after its bound is read off the monotonic clock, from the start
//! of the verb, so that a wall clock that jumps (or a Mac that slept) gives no verb up early.
//!
//! Dropping [`RemoteActions`] kills and reaps every ssh in flight itself (the child is shared with the thread
//! that runs it, [`Ender`]) and then waits a bounded moment for the threads, as [`super::remote_preview`] does,
//! so no ssh outlives the app, whether or not its thread is scheduled in time.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use super::Feed;
use super::machine::Route;
use crate::machine::actions::{Reply, bound, classify, remote_argv};
use crate::machine::bounded::{self, Ender, Ran};
use crate::machine::ssh::{self, Mode, Ssh};
use crate::machine::{Machine, split_id, words};
use crate::paths::Paths;
use crate::tui::Lending;
use crate::tui::actions::Action;
use crate::tui::app::App;

/// How long a verb has been running when the hint line starts to say `waiting for <machine>`.
pub(super) const HINT_AFTER: Duration = Duration::from_secs(2);

/// How long after its bound a verb whose thread has still not answered is given up on (the thread has to
/// end its ssh and reap it, which takes a few looks): the reply is a timeout, the thread is told to end,
/// and the machine's slot stays taken until it has.
const GIVE_UP_AFTER: Duration = Duration::from_secs(5);

/// How long after a session was made on a machine the list may take to show its row, for the app to go in.
pub(super) const SESSION_WAIT: Duration = Duration::from_secs(5);

/// How often a verb in flight is looked at by the thread that runs it.
const LOOK: Duration = Duration::from_millis(30);

/// How long dropping waits for the verbs it ended to say they are over (their ssh killed and reaped).
const DROP_WAIT: Duration = Duration::from_millis(300);

/// The ssh command that runs `action` on `machine`: [`remote_argv`] over [`ssh::command`]. `None` when the
/// action has no verb over there, or its target is not an id the verb takes (the caller says so in a toast).
pub(super) fn command(
    ssh: &Ssh,
    paths: &Paths,
    machine: &Machine,
    action: &Action,
) -> Option<Command> {
    let argv = remote_argv(action);
    if argv.is_empty() {
        return None;
    }
    #[cfg(test)]
    if let Some(command) = seam::command(&argv) {
        return Some(command);
    }
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    Some(ssh::command(ssh, paths, machine, Mode::Command, &words))
}

/// What a verb that has ended came to, and which one it was.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Finished {
    pub machine: String,
    /// The `token` [`RemoteActions::start`] was given.
    pub token: u64,
    pub reply: Reply,
    /// What the verb printed when it ran to its end (a worktree's record, for a created one), else empty.
    pub stdout: Vec<u8>,
}

/// A verb that has not answered yet.
struct Flight {
    token: u64,
    /// What the thread says when the run is over.
    ran: Receiver<Ran>,
    /// Ends the run: its ssh is killed and reaped, by the thread at its next look or at once by [`Ender::end`].
    ender: Arc<Ender>,
    /// The app's clock when the verb was started, in milliseconds (the hint's).
    began_ms: u64,
    /// When the verb was started on the monotonic clock (the give-up's).
    began: Instant,
    bound: Duration,
    /// The reply was given already (a thread that did not answer in time): the slot is held until the
    /// thread has ended its ssh, and nothing is said twice.
    given_up: bool,
    /// The hint line said `waiting for <machine>` for this verb already: it says it once.
    hinted: bool,
}

/// Why a verb was not started.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// A verb is in flight for the machine already.
    Busy,
    /// The thread that runs the verb could not be made (the system's limit): nothing ran.
    NotRun(String),
}

/// The verbs in flight, one per machine, by the machine's name.
#[derive(Default)]
pub(super) struct RemoteActions {
    flights: HashMap<String, Flight>,
}

impl RemoteActions {
    /// Whether a verb is in flight for `machine`.
    pub(super) fn busy(&self, machine: &str) -> bool {
        self.flights.contains_key(machine)
    }

    /// Whether nothing is in flight.
    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.flights.is_empty()
    }

    /// Starts `command` (the ssh of a verb on `machine`: [`command`]) on a thread of its own, to be ended after
    /// `bound` ([`crate::machine::actions::bound`]), at the app's `now_ms`. Refused when a verb is in flight for the
    /// machine already, or the thread cannot be made: then `command` is not run and nothing is sent.
    pub(super) fn begin(
        &mut self,
        machine: &str,
        token: u64,
        command: Command,
        bound: Duration,
        now_ms: u64,
    ) -> Result<(), Refusal> {
        self.begin_with_look(machine, token, command, bound, now_ms, LOOK)
    }

    /// [`Self::begin`] with the thread's look set (a test gives it a long one, so that only what the app does itself
    /// can end the child in time).
    fn begin_with_look(
        &mut self,
        machine: &str,
        token: u64,
        command: Command,
        bound: Duration,
        now_ms: u64,
        look: Duration,
    ) -> Result<(), Refusal> {
        if self.busy(machine) {
            return Err(Refusal::Busy);
        }
        let ender = Ender::new();
        let ran = launch(command, bound, ender.clone(), look).map_err(Refusal::NotRun)?;
        self.flights.insert(
            machine.to_string(),
            Flight {
                token,
                ran,
                ender,
                began_ms: now_ms,
                began: Instant::now(),
                bound,
                given_up: false,
                hinted: false,
            },
        );
        Ok(())
    }

    /// [`Self::begin`] as a yes or no.
    #[cfg(test)]
    pub(super) fn start(
        &mut self,
        machine: &str,
        token: u64,
        command: Command,
        bound: Duration,
        now_ms: u64,
    ) -> bool {
        self.begin(machine, token, command, bound, now_ms).is_ok()
    }

    /// The verbs that have ended by `now` (the monotonic clock). A verb whose thread has not answered by its bound
    /// and [`GIVE_UP_AFTER`] from its start is told to end and answers `TimedOut`; it keeps its machine's slot
    /// until its thread is over. A thread that died without answering is `NoAnswer`. In the order of the
    /// machines' names.
    pub(super) fn poll(&mut self, now: Instant) -> Vec<Finished> {
        let mut finished = Vec::new();
        self.flights
            .retain(|machine, flight| match flight.ran.try_recv() {
                Ok(ran) => {
                    if !flight.given_up {
                        let stdout = match &ran {
                            Ran::Done { stdout, .. } => stdout.clone(),
                            _ => Vec::new(),
                        };
                        finished.push(Finished {
                            machine: machine.clone(),
                            token: flight.token,
                            reply: classify(machine, ran, flight.bound),
                            stdout,
                        });
                    }
                    false
                }
                Err(TryRecvError::Disconnected) => {
                    if !flight.given_up {
                        finished.push(Finished {
                            machine: machine.clone(),
                            token: flight.token,
                            reply: Reply::NoAnswer,
                            stdout: Vec::new(),
                        });
                    }
                    false
                }
                Err(TryRecvError::Empty) => {
                    if !flight.given_up
                        && now.saturating_duration_since(flight.began) >= give_up(flight)
                    {
                        flight.ender.set();
                        flight.given_up = true;
                        finished.push(Finished {
                            machine: machine.clone(),
                            token: flight.token,
                            reply: Reply::TimedOut {
                                secs: flight.bound.as_secs(),
                            },
                            stdout: Vec::new(),
                        });
                    }
                    true
                }
            });
        finished.sort_by(|a, b| a.machine.cmp(&b.machine));
        finished
    }

    /// The machine whose hint is due at `now_ms`: the verb that has waited longest of those that have waited
    /// [`HINT_AFTER`], have not been given up on and have not been hinted yet.
    fn due(&self, now_ms: u64) -> Option<&str> {
        self.flights
            .iter()
            .filter(|(_, flight)| {
                !flight.given_up && !flight.hinted && hint_due(flight.began_ms, now_ms)
            })
            .min_by(|(a, x), (b, y)| x.began_ms.cmp(&y.began_ms).then_with(|| a.cmp(b)))
            .map(|(machine, _)| machine.as_str())
    }

    /// What the hint line says at `now_ms`: `waiting for <machine>` for the verb that has waited longest
    /// when it has waited [`HINT_AFTER`], nothing before (and nothing once it was said, [`Self::hinted`]).
    #[cfg(test)]
    pub(super) fn hint(&self, now_ms: u64) -> Option<String> {
        self.due(now_ms).map(words::waiting)
    }

    /// Takes the hint that is due at `now_ms` and marks it said: a verb is hinted once.
    pub(super) fn take_hint(&mut self, now_ms: u64) -> Option<String> {
        let machine = self.due(now_ms)?.to_string();
        self.flights.get_mut(&machine)?.hinted = true;
        Some(words::waiting(&machine))
    }
}

/// Whether a verb started at `began_ms` has waited long enough, at `now_ms`, for the hint line to say so.
pub(super) fn hint_due(began_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(began_ms) >= HINT_AFTER.as_millis() as u64
}

/// How long after its start a verb's thread is given up on.
fn give_up(flight: &Flight) -> Duration {
    flight.bound + GIVE_UP_AFTER
}

impl Drop for RemoteActions {
    /// Kills and reaps every ssh in flight itself, then waits a bounded moment for the threads that ran them
    /// (they see their child gone and end), so no ssh outlives the app.
    fn drop(&mut self) {
        for flight in self.flights.values() {
            flight.ender.end();
        }
        let deadline = Instant::now() + DROP_WAIT;
        for flight in self.flights.values() {
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = flight.ran.recv_timeout(left);
        }
    }
}

/// Runs `command` by [`bounded::run_in`] on a thread of its own that sends how it ended, within `bound` or as
/// soon as `ender` ends it: then the process is killed and reaped. The error is why the thread could not be
/// made (nothing ran).
fn launch(
    command: Command,
    bound: Duration,
    ender: Arc<Ender>,
    look: Duration,
) -> Result<Receiver<Ran>, String> {
    let (sent, ran) = mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let ran = bounded::run_in(&ender, &mut (), command, bound, look, |_| false, |_| {});
            let _ = sent.send(ran);
        })
        .map_err(|err| err.to_string())?;
    Ok(ran)
}

/// `action` with the machine's name taken off its ids (`studio:%16` is `%16` over there), and the machine, when
/// the action has a verb and its target is a machine's. `None` for this Mac's own actions and the ones that go
/// somewhere rather than do something. An id of another machine than the first is left as it is, so that
/// [`remote_argv`] refuses it.
pub(super) fn unprefixed(action: &Action) -> Option<(String, Action)> {
    let first = match action {
        Action::KillPane { pane } | Action::RenameAgent { pane, .. } => pane,
        Action::KillWorkspace { session }
        | Action::KillGroup { session }
        | Action::RenameWorkspace { session, .. } => session,
        Action::RenameWindow { window, .. } => window,
        Action::NewWorktree { workspace, .. } | Action::RemoveWorktree { workspace, .. } => {
            workspace
        }
        _ => return None,
    };
    let machine = split_id(first).0?.to_string();
    let raw = |id: &str| match split_id(id) {
        (Some(name), raw) if name == machine => raw.to_string(),
        _ => id.to_string(),
    };
    let stripped = match action {
        Action::KillPane { pane } => Action::KillPane { pane: raw(pane) },
        Action::KillWorkspace { session } => Action::KillWorkspace {
            session: raw(session),
        },
        Action::KillGroup { session } => Action::KillGroup {
            session: raw(session),
        },
        Action::RenameAgent { pane, name } => Action::RenameAgent {
            pane: raw(pane),
            name: name.clone(),
        },
        Action::RenameWorkspace { session, name } => Action::RenameWorkspace {
            session: raw(session),
            name: name.clone(),
        },
        Action::RenameWindow { window, name } => Action::RenameWindow {
            window: raw(window),
            name: name.clone(),
        },
        Action::NewWorktree { workspace, branch } => Action::NewWorktree {
            workspace: raw(workspace),
            branch: branch.clone(),
        },
        Action::RemoveWorktree { workspace, force } => Action::RemoveWorktree {
            workspace: raw(workspace),
            force: *force,
        },
        _ => return None,
    };
    Some((machine, stripped))
}

/// What the app remembers of a verb it sent to a machine, until the machine answers.
struct Pending {
    token: u64,
    /// The action as the app made it, with the machine's name on its ids: what a question asked of the answer runs.
    action: Action,
    /// [`App::cursor_id`] when the key was pressed: the answer asks its question only of that row.
    row: Option<String>,
    /// For a group close: the session's name in the list, and how many sessions the group has (its own
    /// included), as they were when the key was pressed: the toast of the close says them.
    group: Option<(String, usize)>,
}

/// The verbs sent to machines and what each was for, over the worker that runs them.
#[derive(Default)]
pub(super) struct Rows {
    worker: RemoteActions,
    pending: HashMap<String, Pending>,
    next_token: u64,
    /// A session just made on a machine, whose row the list has not shown yet: the app goes in when it does.
    pub(super) awaiting: Option<Awaiting>,
}

/// The session a machine made for the app's `n`, waited for in the list ([`SESSION_WAIT`]).
pub(super) struct Awaiting {
    machine: String,
    name: String,
    pub(super) since: Instant,
}

impl Rows {
    /// Whether a verb is in flight for `machine`.
    pub(super) fn busy(&self, machine: &str) -> bool {
        self.worker.busy(machine)
    }

    /// Sends `action` (as the app made it) on `command` to `machine`, which is the one the row belongs to,
    /// named by the key's own ids. Says in a toast why nothing was sent: the action has no id to send, or the
    /// verb could not be started. A machine still answering the last action is [`Feed::perform_remote`]'s to
    /// say, before this: `send` refuses it silently, so a new caller must check [`Rows::busy`] and toast
    /// first, as `perform_remote` does. A new worktree says it is opening while it waits.
    pub(super) fn send(
        &mut self,
        app: &mut App,
        machine: &str,
        command: Option<Command>,
        action: &Action,
        bound: Duration,
    ) {
        let Some(command) = command else {
            app.toast(words::no_id_to_send(machine));
            return;
        };
        self.next_token += 1;
        let token = self.next_token;
        match self
            .worker
            .begin(machine, token, command, bound, app.wall_ms())
        {
            Ok(()) => {}
            // The caller said so already ([`Feed::perform_remote`] checks first); nothing is sent.
            Err(Refusal::Busy) => {
                debug_assert!(false, "send was called for a busy machine: toast first");
                return;
            }
            Err(Refusal::NotRun(why)) => {
                app.toast(words::ssh_not_run(machine, &why));
                return;
            }
        }
        if matches!(action, Action::NewWorktree { .. }) {
            app.toast(words::opening_worktree(machine));
        }
        let group = match action {
            Action::KillGroup { session } => {
                Some((app.session_name_of(session), app.group_size(session)))
            }
            _ => None,
        };
        self.pending.insert(
            machine.to_string(),
            Pending {
                token,
                action: action.clone(),
                row: app.cursor_id(),
                group,
            },
        );
    }

    /// What the machines answered by the app's clock: each answer is said, or asked about, or (a created
    /// worktree) a route to go in by, which the screen follows. An answer that comes when the token is not the
    /// one sent, or the cursor has left the row it was for, or something else is open, says only its toast.
    pub(super) fn settle(&mut self, app: &mut App) -> Vec<Route> {
        let now = app.wall_ms();
        let finished = self.worker.poll(Instant::now());
        let mut routes = Vec::new();
        let answered = !finished.is_empty();
        for done in finished {
            let machine = done.machine.as_str();
            let sent = self.pending.remove(machine);
            let current = sent.as_ref().filter(|sent| sent.token == done.token);
            let here =
                current.is_some_and(|sent| sent.row == app.cursor_id() && !app.overlay.is_open());
            match (done.reply, current) {
                // A session made over there: said, and when the cursor is still where `n` was pressed, gone
                // in to once the list shows its row.
                (Reply::Done, Some(sent))
                    if matches!(sent.action, Action::NewRemoteSession { .. }) =>
                {
                    match crate::machine::actions::session_opened(&done.stdout) {
                        Some((name, created, dir)) => {
                            app.toast(words::session_made(machine, &name, &dir, created));
                            if here {
                                self.awaiting = Some(Awaiting {
                                    machine: machine.to_string(),
                                    name,
                                    since: Instant::now(),
                                });
                            }
                        }
                        None => app.toast(words::session_made_unreadable(machine)),
                    }
                }
                (Reply::Usage, Some(sent))
                    if matches!(sent.action, Action::NewRemoteSession { .. }) =>
                {
                    app.toast(words::update_taw_to_make_sessions(machine));
                }
                // `open` says `not_found` for a folder or a term that is nowhere: the far words, not "row gone".
                (Reply::Gone, Some(sent))
                    if matches!(sent.action, Action::NewRemoteSession { .. }) =>
                {
                    let said =
                        crate::machine::actions::error_message(&done.stdout).unwrap_or_default();
                    app.toast(words::refused(machine, &said));
                }
                (Reply::Usage, _) => app.toast(crate::machine::actions::usage_words(machine)),
                (Reply::Done, Some(sent)) if here => {
                    if matches!(sent.action, Action::NewWorktree { .. }) {
                        match created(&done.stdout) {
                            Some(workspace) => routes.push(Route::OpenFirst {
                                machine: machine.to_string(),
                                workspace,
                            }),
                            None => app.toast(words::worktree_open_unreadable(machine)),
                        }
                    } else if let Some(said) = done_words(machine, sent, &done.stdout) {
                        app.toast(said);
                    }
                }
                // The verb ran, and the row, or the screen, is not where the key was pressed: the toast is
                // all that is left to say, and nothing is gone in to.
                (Reply::Done, Some(sent)) => {
                    if matches!(sent.action, Action::NewWorktree { .. }) {
                        app.toast(words::worktree_made_not_opened(machine));
                    } else if let Some(said) = done_words(machine, sent, &done.stdout) {
                        app.toast(said);
                    }
                }
                (Reply::Done, None) => {}
                (Reply::HookRefused { label, why }, _) => {
                    app.toast(words::hook_refused(machine, &label, &why));
                }
                (Reply::GroupCloseRequired, Some(sent)) if here => {
                    if !app.ask_group_close(machine, &sent.action) {
                        app.toast(words::answered_late(machine));
                    }
                }
                (Reply::Dirty, Some(sent)) if here => {
                    if !app.ask_dirty(&sent.action) {
                        app.toast(words::answered_late(machine));
                    }
                }
                (Reply::GroupCloseRequired | Reply::Dirty, _) => {
                    app.toast(words::answered_late(machine));
                }
                (Reply::Gone, _) => app.toast(words::row_gone(machine)),
                (Reply::Refused(said), _) => app.toast(said),
                (Reply::NoAnswer, _) => app.toast(words::no_answer(machine)),
                (Reply::TimedOut { secs }, _) => app.toast(words::timed_out(machine, secs)),
            }
        }
        routes.extend(self.go_in_to_awaited(app));
        // The hint is said once per verb, and never over another toast: it waits for an empty slot.
        if !answered
            && app.live_toast().is_none()
            && let Some(hint) = self.worker.take_hint(now)
        {
            app.toast(hint);
        }
        routes
    }
}

impl Rows {
    /// The route into the session a machine made for `n`, when the list shows its row: the row is selected and
    /// the route is the one Enter on it takes. Nothing after [`SESSION_WAIT`]: nothing is selected, the toast
    /// stays, and the app waits for nothing.
    fn go_in_to_awaited(&mut self, app: &mut App) -> Option<Route> {
        let waiting = self.awaiting.as_ref()?;
        let found = app.snap.spaces.iter().position(|space| {
            space.session_name == waiting.name
                && space
                    .machine
                    .and_then(|at| app.snap.machines.get(at))
                    .is_some_and(|header| header.name == waiting.machine)
        });
        let Some(space) = found else {
            if waiting.since.elapsed() >= SESSION_WAIT {
                self.awaiting = None;
            }
            return None;
        };
        self.awaiting = None;
        let session = app.snap.spaces[space].session_id.clone();
        if let Some(at) = app
            .rows
            .rows
            .iter()
            .position(|row| matches!(row, crate::tui::tree::Row::Session(s) if *s == space))
        {
            app.sel = at;
        }
        let active_pane_of = |id: &str| {
            app.snap
                .spaces
                .iter()
                .find(|space| space.session_id == id)
                .and_then(|space| space.active_pane_id.clone())
        };
        super::machine::route(&Action::SwitchWorkspace { session }, active_pane_of)
    }
}

/// What the popup says when its own action has run, said of `machine`'s: a group closed (the session and how
/// many worktree sessions went with it), an agent renamed, a worktree removed. The popup says nothing of a pane
/// or a session closed or a name changed, and neither does this.
fn done_words(machine: &str, sent: &Pending, stdout: &[u8]) -> Option<String> {
    match &sent.action {
        Action::KillGroup { .. } => {
            let (name, sessions) = sent.group.as_ref()?;
            // The count is the far side's own, as its reply names the worktree sessions it closed; the list's
            // count at the key is only for a reply that does not.
            let closed = crate::machine::actions::worktree_sessions_closed(stdout)
                .unwrap_or_else(|| sessions.saturating_sub(1));
            Some(crate::tui::actions::group_closed_toast(
                &words::on_machine(machine, name),
                closed,
            ))
        }
        Action::RenameAgent { name, .. } => Some(crate::tui::actions::agent_renamed_toast(name)),
        Action::RemoveWorktree { .. } => {
            let branch = crate::machine::actions::worktree_branch_removed(stdout);
            Some(words::worktree_removed(machine, branch.as_deref()))
        }
        _ => None,
    }
}

/// The `workspace_id` (`w5`) of the worktree `worktree create` made, read off its envelope (the last line).
fn created(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let reply: serde_json::Value = serde_json::from_str(line).ok()?;
    let id = reply["result"]["worktree"]["workspace_id"].as_str()?;
    let digits = id.strip_prefix('w')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

impl Feed<'_> {
    /// Sends the action of a row of a machine to that machine: the machine is the one the ids of the action
    /// name, resolved now, at the key. Its answer comes back through [`Feed::settle_remote`]. This Mac's hooks
    /// are never asked: nothing here reaches [`crate::tui::actions::exec`].
    pub(super) fn perform_remote(
        &mut self,
        app: &mut App,
        machine: &str,
        raw: &Action,
        action: &Action,
    ) {
        if self.remote.busy(machine) {
            app.toast(words::still_answering(machine));
            return;
        }
        if let Some(said) = app.away_words(machine) {
            app.toast(said);
            return;
        }
        let Some(found) = self.machines.iter().find(|m| m.name == machine).cloned() else {
            app.toast(words::no_such_machine(machine));
            return;
        };
        let command = command(&self.ssh, self.paths, &found, raw);
        let sessions = match action {
            Action::KillGroup { session } => app.group_size(session),
            _ => 1,
        };
        self.remote
            .send(app, machine, command, action, bound(raw, sessions));
    }

    /// Takes what the machines answered to the actions sent them, and goes in where an answer says to.
    pub(super) fn settle_remote(
        &mut self,
        app: &mut App,
        screen: &mut impl Lending,
    ) -> anyhow::Result<()> {
        for route in self.remote.settle(app) {
            self.go_by_route(app, screen, route)?;
        }
        Ok(())
    }
}

/// What a test puts in place of the ssh command: it is handed the verb's arguments and gives the command to
/// run (a `sh` script), so that no ssh is ever run. The one before is put back when its guard drops.
#[cfg(test)]
pub(super) mod seam {
    use std::cell::RefCell;
    use std::process::Command;

    type Make = Box<dyn Fn(&[String]) -> Command>;

    thread_local! {
        static MAKE: RefCell<Option<Make>> = const { RefCell::new(None) };
    }

    pub(super) fn command(argv: &[String]) -> Option<Command> {
        MAKE.with(|make| make.borrow().as_ref().map(|make| make(argv)))
    }

    /// Puts the seam that was in place back when it is dropped: bind it to a name, or the seam is gone at once.
    #[must_use = "the seam is put away when the guard is dropped: bind it to a name"]
    pub(super) struct Guard(Option<Make>);

    /// Puts `make` in place for this thread until the guard is dropped.
    #[must_use = "the seam is put away when the guard is dropped: bind it to a name"]
    pub(super) fn set(make: impl Fn(&[String]) -> Command + 'static) -> Guard {
        Guard(MAKE.with(|slot| slot.borrow_mut().replace(Box::new(make))))
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let before = self.0.take();
            MAKE.with(|slot| *slot.borrow_mut() = before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE_GONE: &str =
        r#"{"id":"cli:pane:close","error":{"code":"not_found","message":"pane not found"}}"#;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// Waits (20 s) for `ready`, and panics with `what` when it never held.
    fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Polls until `machine` has answered, and gives the answer.
    fn answer(actions: &mut RemoteActions) -> Finished {
        let mut got = None;
        wait_until("the verb answering", || {
            got = actions.poll(Instant::now()).pop();
            got.is_some()
        });
        got.unwrap()
    }

    fn pid_in(pidfile: &std::path::Path) -> i32 {
        std::fs::read_to_string(pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the pid exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Kills the pid a test's process wrote to its file, if a failed assert left it running: only while armed,
    /// and only if the pid's command line still names the process the test started (a number another process
    /// reused is left alone).
    struct KillPidIn {
        pidfile: std::path::PathBuf,
        far: &'static str,
        armed: bool,
    }

    impl KillPidIn {
        fn new(pidfile: std::path::PathBuf, far: &'static str) -> KillPidIn {
            KillPidIn {
                pidfile,
                far,
                armed: true,
            }
        }
    }

    impl Drop for KillPidIn {
        fn drop(&mut self) {
            let pid = pid_in(&self.pidfile);
            if !self.armed || pid <= 1 {
                return;
            }
            let command = Command::new("ps")
                .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            if command.contains(self.far) {
                // SAFETY: kill(2) takes two integers; the pid is the test's process, checked just now by its command.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    #[test]
    fn a_verb_that_ended_answers_with_its_token_and_its_machine_named_in_the_reply() {
        let mut actions = RemoteActions::default();
        let script = format!("printf '%s\\n' '{ENVELOPE_GONE}'; exit 1");
        assert!(actions.start("studio", 41, sh(&script), Duration::from_secs(20), 1_000));
        assert!(actions.busy("studio"));
        let got = answer(&mut actions);
        assert_eq!(
            (got.machine.as_str(), got.token, &got.reply),
            ("studio", 41, &Reply::Gone)
        );
        assert!(
            !got.stdout.is_empty(),
            "what the verb printed comes with the answer"
        );
        assert!(!actions.busy("studio"), "the slot is free once it answered");
        assert!(actions.is_idle());
        // And the machine takes another verb now.
        assert!(actions.start("studio", 42, sh("exit 0"), Duration::from_secs(20), 2_000));
        assert_eq!(answer(&mut actions).reply, Reply::Done);
    }

    #[test]
    fn a_second_verb_for_a_machine_in_flight_sends_nothing_and_another_machine_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second, other) = (
            dir.path().join("first"),
            dir.path().join("second"),
            dir.path().join("other"),
        );
        let mut guard = KillPidIn::new(first.clone(), "sleep 301");
        let mut actions = RemoteActions::default();
        let long = format!("echo $$ > '{}'; exec sleep 301", first.display());
        assert!(actions.start("studio", 1, sh(&long), Duration::from_secs(20), 0));
        wait_until("the first verb running", || pid_in(&first) > 0);
        // The second one for studio: refused, and what it would have run does not run.
        let marker = format!("touch '{}'", second.display());
        assert!(!actions.start("studio", 2, sh(&marker), Duration::from_secs(20), 10));
        // A verb for another machine runs at once.
        let elsewhere = format!("touch '{}'", other.display());
        assert!(actions.start("pi", 3, sh(&elsewhere), Duration::from_secs(20), 10));
        wait_until("the other machine's verb running", || other.exists());
        let got = answer(&mut actions);
        assert_eq!((got.machine.as_str(), got.token), ("pi", 3));
        assert!(
            actions.busy("studio"),
            "the first verb still holds its slot"
        );
        assert!(
            !second.exists(),
            "the refused verb ran (checked after the other machine's ran)"
        );
        drop(actions);
        guard.armed = false;
    }

    #[test]
    fn dropping_ends_the_ssh_of_a_verb_in_flight_by_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let guard = KillPidIn::new(pidfile.clone(), "sleep 302");
        let mut actions = RemoteActions::default();
        let script = format!("echo $$ > '{}'; exec sleep 302", pidfile.display());
        // The thread looks every ten seconds: only Drop itself can end the child in time.
        let long = Duration::from_secs(10);
        assert!(
            actions
                .begin_with_look("studio", 1, sh(&script), Duration::from_secs(20), 0, long)
                .is_ok()
        );
        wait_until("the process writing its pid", || pid_in(&pidfile) > 0);
        let pid = pid_in(&pidfile);
        assert!(alive(pid), "positive half: it runs before the drop");
        drop(actions);
        // At once, with no wait: Drop kills and reaps the child itself, it does not leave it to its thread.
        // Mutants caught: Drop without `ender.end()`, and Drop that only sets the flag (`ender.set()`): the
        // thread would end the child at its next look, ten seconds away.
        assert!(!alive(pid), "the process {pid} outlived the app");
        let mut guard = guard;
        guard.armed = false;
    }

    #[test]
    fn a_verb_that_outlives_its_bound_is_ended_by_its_pid_and_answers_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut guard = KillPidIn::new(pidfile.clone(), "sleep 303");
        let mut actions = RemoteActions::default();
        let script = format!("echo $$ > '{}'; exec sleep 303", pidfile.display());
        assert!(actions.start("studio", 7, sh(&script), Duration::from_secs(1), 0));
        wait_until("the process writing its pid", || pid_in(&pidfile) > 0);
        let pid = pid_in(&pidfile);
        let got = answer(&mut actions);
        assert_eq!(got.reply, Reply::TimedOut { secs: 1 });
        assert!(!alive(pid), "the process {pid} outlived its bound");
        guard.armed = false;
    }

    #[test]
    fn the_hint_comes_after_two_seconds_on_the_apps_clock_for_the_verb_waiting_longest() {
        let mut actions = RemoteActions::default();
        assert_eq!(actions.hint(10_000), None, "nothing in flight");
        assert!(actions.start(
            "studio",
            1,
            sh("exec sleep 304"),
            Duration::from_secs(20),
            10_000
        ));
        assert!(actions.start(
            "pi",
            2,
            sh("exec sleep 305"),
            Duration::from_secs(20),
            10_500
        ));
        assert_eq!(actions.hint(10_000), None);
        assert_eq!(actions.hint(11_999), None, "one millisecond short");
        assert_eq!(actions.hint(12_000), Some("waiting for studio".into()));
        assert_eq!(
            actions.hint(12_600),
            Some("waiting for studio".into()),
            "the longest wait names the machine"
        );
        assert!(hint_due(5, 2_005) && !hint_due(5, 2_004));
        assert!(!hint_due(9_000, 1_000), "a clock that went back is no wait");
    }

    #[test]
    fn a_thread_that_has_not_answered_after_the_bound_is_given_up_once_and_holds_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut guard = KillPidIn::new(pidfile.clone(), "sleep 306");
        let mut actions = RemoteActions::default();
        let script = format!("echo $$ > '{}'; exec sleep 306", pidfile.display());
        // A bound of 100 s: the real time never reaches it, the clock the test hands to `poll` does. The
        // app's wall clock (the hint's, 0 here) plays no part.
        let before = Instant::now();
        assert!(actions.start("studio", 9, sh(&script), Duration::from_secs(100), 0));
        let after = Instant::now();
        wait_until("the process writing its pid", || pid_in(&pidfile) > 0);
        let pid = pid_in(&pidfile);
        assert!(
            actions
                .poll(before + Duration::from_millis(104_900))
                .is_empty(),
            "before bound plus slack"
        );
        let given_up = actions.poll(after + Duration::from_secs(105));
        assert_eq!(
            given_up,
            vec![Finished {
                machine: "studio".into(),
                token: 9,
                reply: Reply::TimedOut { secs: 100 },
                stdout: Vec::new()
            }]
        );
        assert_eq!(
            actions.hint(105_000),
            None,
            "a verb given up on is not waited for"
        );
        // The thread ends its ssh by the pid; the slot is free once it has said so, and nothing is said twice.
        wait_until("the slot freed", || {
            assert!(
                actions.poll(after + Duration::from_secs(106)).is_empty(),
                "said twice"
            );
            !actions.busy("studio")
        });
        assert!(!alive(pid), "the process {pid} outlived the give-up");
        guard.armed = false;
    }

    #[test]
    fn the_command_is_the_far_verb_over_ssh_and_none_for_an_action_without_one() {
        let ssh = Ssh {
            bin: "/nonexistent/ssh-for-the-test".into(),
        };
        let paths = Paths::rooted_at("/nonexistent/c".into(), "/nonexistent/s".into());
        let machine = Machine {
            name: "studio".into(),
            ssh: "studio-host".into(),
            taw: None,
        };
        let close = Action::KillPane { pane: "%16".into() };
        let made = command(&ssh, &paths, &machine, &close).expect("a verb");
        assert_eq!(made.get_program(), "/nonexistent/ssh-for-the-test");
        let line: Vec<String> = made
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(line.iter().any(|arg| arg == "studio-host"), "{line:?}");
        let said = line.join(" ");
        assert!(
            said.contains("pane") && said.contains("close") && said.contains("%16"),
            "{said}"
        );
        let bad = Action::KillPane {
            pane: "studio:%16".into(),
        };
        assert!(command(&ssh, &paths, &machine, &bad).is_none());
        let go = Action::Jump {
            session: "$1".into(),
            window: "@1".into(),
            pane: "%16".into(),
        };
        assert!(command(&ssh, &paths, &machine, &go).is_none());
    }

    #[test]
    fn the_test_seam_hands_the_verbs_arguments_to_a_command_of_the_test_and_is_put_away() {
        let ssh = Ssh {
            bin: "/nonexistent/ssh-for-the-test".into(),
        };
        let paths = Paths::rooted_at("/nonexistent/c".into(), "/nonexistent/s".into());
        let machine = Machine {
            name: "studio".into(),
            ssh: "studio-host".into(),
            taw: None,
        };
        let close = Action::KillPane { pane: "%16".into() };
        {
            let _guard =
                seam::set(|argv| sh(&format!("test '{}' = 'pane close %16'", argv.join(" "))));
            let made = command(&ssh, &paths, &machine, &close).unwrap();
            let mut actions = RemoteActions::default();
            assert!(actions.start("studio", 1, made, Duration::from_secs(20), 0));
            assert_eq!(answer(&mut actions).reply, Reply::Done);
        }
        let after = command(&ssh, &paths, &machine, &close).unwrap();
        assert_eq!(after.get_program(), "/nonexistent/ssh-for-the-test");
    }
}

#[cfg(test)]
mod rows_tests {
    //! The app acting on a machine's rows: a key on a live row to the verb that runs there and the answer back,
    //! through the real [`Feed`] over a stub tmux, with the seam handing the worker a `sh` that prints a far
    //! verb's envelope in place of ssh.

    use std::fs;
    use std::path::Path;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::cli::test_support::{Stub, stub_tmux};
    use crate::config::Config;
    use crate::tmux::Tmux;
    use crate::tui::app_screen::tests::{Unlent, feed_over, paths_in};
    use crate::tui::machines::tests::{NOW, app_over, depth_all, five_and_two, toast_of};
    use crate::tui::overlays::Overlay;
    use crate::tui::tree::Row;

    const CLOSED: &str = r#"{"id":"cli:pane:close","result":{"terminal_id":"%8","type":"ok"}}"#;
    const PRE_CLOSE: &str = r#"{"id":"cli:pane:close","error":{"code":"hook_refused","message":"pre-close hook echo uncommitted changes in ~/code/garden >&2; exit 1 failed (exit status: 1): uncommitted changes in ~/code/garden"}}"#;
    const GROUP: &str = r#"{"id":"cli:workspace:close","error":{"code":"workspace_group_close_required","message":"workspace '$4' has worktree workspaces still open: w2 (repo--feat-x); pass --group to close them together with it"}}"#;
    const DIRTY: &str = r#"{"id":"cli:worktree:remove","error":{"code":"worktree_dirty","message":"git failed (exit status: 128): contains modified or untracked files, use --force to delete it"}}"#;
    const NOT_A_REPO: &str = r#"{"id":"cli:worktree:create","error":{"code":"not_a_git_repo","message":"not_a_git_repo: '/home/studio/notes' is not inside a git repository"}}"#;
    const CREATED: &str = r#"{"id":"cli:worktree:create","result":{"worktree":{"workspace_id":"w7","session_name":"garden--feat-x","path":"/home/studio/co/garden--feat-x","branch":"feat-x"}}}"#;

    struct Rig {
        dir: tempfile::TempDir,
        tmux: Tmux,
        paths: Paths,
        cfg: Config,
    }

    impl Rig {
        fn new(hook: Option<&str>) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _) = stub_tmux(dir.path(), &Stub::default());
            let mut cfg = match hook {
                Some(command) => toml::from_str(&format!("[hooks]\npre_close = \"{command}\"\n"))
                    .expect("the test config parses"),
                None => Config::default(),
            };
            cfg.ui.depth = depth_all().ui.depth;
            let paths = paths_in(dir.path());
            Rig {
                dir,
                tmux,
                paths,
                cfg,
            }
        }

        fn feed(&self) -> Feed<'_> {
            let mut feed = feed_over(&self.tmux, &self.cfg, &self.paths);
            feed.machines = vec![Machine {
                name: "studio".into(),
                ssh: "studio".into(),
                taw: None,
            }];
            feed
        }

        fn app(&self) -> App<'_> {
            let mut app = app_over(&self.cfg, five_and_two("idle", true));
            app.enter_app_mode(None, None);
            app
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.dir.path().join(name)
        }

        /// The ssh of a verb on studio is a `sh` that records its argv (one word a line, appended to `argv.txt`)
        /// and then prints `line` and exits with `code`, or (`hang`) sleeps until it is ended.
        fn fake(&self, line: &str, code: i32, hang: bool) -> seam::Guard {
            fs::write(self.path("envelope.txt"), format!("{line}\n")).unwrap();
            let tail = if hang {
                "exec sleep 31".to_string()
            } else {
                format!("cat '{}'; exit {code}", self.path("envelope.txt").display())
            };
            let script = format!(
                r#"printf '%s\n' "$@" >> '{}'; {tail}"#,
                self.path("argv.txt").display()
            );
            seam::set(move |argv| {
                let mut command = Command::new("sh");
                command.arg("-c").arg(&script).arg("sh").args(argv);
                command
            })
        }

        fn sent(&self) -> Vec<String> {
            fs::read_to_string(self.path("argv.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    fn press(app: &mut App, key: KeyCode) -> Option<Action> {
        app.on_key(KeyEvent::new(key, KeyModifiers::NONE))
    }

    fn type_in(app: &mut App, text: &str) {
        for c in text.chars() {
            let _ = press(app, KeyCode::Char(c));
        }
    }

    fn select(app: &mut App, id: &str) {
        app.sel = app
            .rows
            .rows
            .iter()
            .position(|row| match row {
                Row::Pane(p) => app.snap.panes[*p].pane_id == id,
                Row::Session(s) => app.snap.spaces[*s].session_id == id,
                _ => false,
            })
            .unwrap_or_else(|| panic!("no row {id}"));
    }

    fn send(feed: &mut Feed, app: &mut App, action: &Action) {
        feed.perform(Path::new("/nonexistent/taw"), app, &mut Unlent, action)
            .unwrap();
    }

    /// Settles until studio's verb has answered; the routes the answers made.
    fn answered(feed: &mut Feed, app: &mut App) -> Vec<Route> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut routes = Vec::new();
        while feed.remote.busy("studio") {
            assert!(
                Instant::now() < deadline,
                "no answer in 20 s; toast {:?}",
                toast_of(app)
            );
            routes.extend(feed.remote.settle(app));
            std::thread::sleep(Duration::from_millis(10));
        }
        routes.extend(feed.remote.settle(app));
        routes
    }

    fn question(app: &App) -> String {
        match &app.overlay {
            Overlay::Confirm(confirm) => confirm.message.clone(),
            _ => panic!("no question; toast {:?}", toast_of(app)),
        }
    }

    /// `x` on studio's pane asks with the machine named, `y` sends `pane close` with the row's own id, and the
    /// app says nothing of a close that went through.
    /// Mutation: the id sent keeps the machine's name (`unprefixed` returns the action as it is).
    #[test]
    fn x_then_y_on_a_live_row_sends_pane_close_with_the_rows_id() {
        let rig = Rig::new(None);
        let _fake = rig.fake(CLOSED, 0, false);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:%8");
        assert!(press(&mut app, KeyCode::Char('x')).is_none());
        assert_eq!(
            question(&app),
            "kill-pane studio \u{b7} photos:1 (codex)? It is the session's last pane, so the session closes with it."
        );
        let action = press(&mut app, KeyCode::Char('y')).expect("y runs the close");
        send(&mut feed, &mut app, &action);
        assert!(answered(&mut feed, &mut app).is_empty());
        assert_eq!(rig.sent(), ["pane", "close", "%8"]);
        assert_eq!(toast_of(&app), None);
    }

    /// A far hook that refuses is a toast in the hook's words, with the machine, and the row is still there.
    /// Mutation: `HookRefused` answers with `words::refused` instead of `words::hook_refused`.
    #[test]
    fn a_refusing_hook_is_a_toast_and_the_row_stays() {
        let rig = Rig::new(None);
        let _fake = rig.fake(PRE_CLOSE, 1, false);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:%8");
        let rows = app.rows.rows.len();
        send(
            &mut feed,
            &mut app,
            &Action::KillPane {
                pane: "studio:%8".into(),
            },
        );
        answered(&mut feed, &mut app);
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("pre-close hook on studio: uncommitted changes in ~/code/garden")
        );
        assert_eq!(app.rows.rows.len(), rows);
        assert!(!app.overlay.is_open());
    }

    /// The group question names the machine and only `y` runs `--group`; a dirty worktree asks this Mac's own
    /// question, and only `y` sends `--force`.
    /// Mutation: the answer to a dirty worktree sends `force: false` again.
    #[test]
    fn the_group_and_the_dirty_questions_are_asked_and_y_sends_the_stronger_verb() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:$4");
        let _guard = rig.fake(GROUP, 1, false);
        send(
            &mut feed,
            &mut app,
            &Action::KillWorkspace {
                session: "studio:$4".into(),
            },
        );
        answered(&mut feed, &mut app);
        assert_eq!(
            question(&app),
            "kill-session studio \u{b7} photos? It has worktree sessions open. Close them too?"
        );
        assert!(press(&mut app, KeyCode::Enter).is_none(), "Enter is no");
        assert!(!app.overlay.is_open());
        let _ = press(&mut app, KeyCode::Char('x'));
        assert_eq!(question(&app), "kill-session studio \u{b7} photos?");
        let _ = press(&mut app, KeyCode::Char('y'));
        drop(_guard);
        let _fake = rig.fake(DIRTY, 1, false);
        let mut record = crate::tui::fixtures::worktrees().remove(0);
        record.workspace_id = "studio:w5".into();
        record.branch = "feat-x".into();
        record.machine = Some(1);
        app.worktrees = vec![record];
        let remove = Action::RemoveWorktree {
            workspace: "studio:w5".into(),
            force: false,
        };
        send(&mut feed, &mut app, &remove);
        answered(&mut feed, &mut app);
        assert_eq!(
            question(&app),
            "git will not remove feat-x: it has modified or untracked files. remove anyway? uncommitted changes are lost"
        );
        let n = press(&mut app, KeyCode::Char('n'));
        assert!(n.is_none() && !app.overlay.is_open(), "n is no");
        send(&mut feed, &mut app, &remove);
        answered(&mut feed, &mut app);
        let forced = press(&mut app, KeyCode::Char('y')).expect("y answers");
        send(&mut feed, &mut app, &forced);
        answered(&mut feed, &mut app);
        let sent = rig.sent();
        assert_eq!(
            sent[sent.len() - 5..],
            ["worktree", "remove", "--workspace", "w5", "--force"],
            "{sent:?}"
        );
    }

    /// Renaming a machine's session is checked against that machine's sessions, not this Mac's: a name this Mac's
    /// sessions hold is free on studio, and a name another studio session holds is refused.
    /// Mutation: `RenameSession` filters on `machine.is_none()` again.
    #[test]
    fn a_machines_session_is_renamed_against_that_machines_names() {
        let rig = Rig::new(None);
        let mut app = rig.app();
        let local = app
            .snap
            .spaces
            .iter()
            .find(|s| s.machine.is_none())
            .expect("a local session")
            .session_name
            .clone();
        select(&mut app, "studio:$4");
        assert!(press(&mut app, KeyCode::Char('r')).is_none());
        for _ in 0.."photos".len() {
            let _ = press(&mut app, KeyCode::Backspace);
        }
        type_in(&mut app, &local);
        let action = press(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            Some(Action::RenameWorkspace {
                session: "studio:$4".into(),
                name: local
            }),
            "toast {:?}",
            toast_of(&app)
        );
    }

    /// A running worktree session on studio, cut from `photos` (`$4`): the list then knows the session the
    /// group close takes with it. Built here, from the fixtures the rows tests already use.
    fn with_a_worktree_session(app: &mut App) {
        let photos = app
            .snap
            .spaces
            .iter()
            .find(|s| s.session_id == "studio:$4")
            .expect("studio's photos")
            .clone();
        let mut linked = photos.clone();
        linked.session_id = "studio:$11".into();
        linked.session_name = "photos--feat-x".into();
        linked.session_path = "/home/pi/photos--feat-x".into();
        let record = crate::session::worktree::WorktreeRecord {
            workspace_id: "w11".into(),
            session_name: "photos--feat-x".into(),
            path: "/home/pi/photos--feat-x".into(),
            branch: "feat-x".into(),
            base: None,
            repo_root: photos.session_path.clone(),
            source_workspace_id: "w4".into(),
            created_unix_ms: 0,
            machine: photos.machine,
            real_path: None,
            real_repo_root: None,
        };
        app.snap.spaces.push(linked);
        // The machine sends a worktree one of its sessions runs beside its closed ones, never among them.
        app.snap.open_worktrees = vec![record];
    }

    /// `x` on a machine's session with worktree sessions open asks only the strict group question, naming the
    /// machine: no "close only this session" (the far verb would refuse it), `n` and Enter are no, and `y` sends
    /// `--group`.
    /// Mutation: the question is `Confirm::choice` again (Enter then closes just the session), or `y` sends
    /// `KillWorkspace` (no `--group` on the wire).
    #[test]
    fn x_on_a_machines_session_with_worktree_sessions_asks_only_the_group_question() {
        let rig = Rig::new(None);
        let _fake = rig.fake(CLOSED, 0, false);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        with_a_worktree_session(&mut app);
        select(&mut app, "studio:$4");
        for answer in [KeyCode::Enter, KeyCode::Char('n')] {
            assert!(press(&mut app, KeyCode::Char('x')).is_none());
            assert_eq!(
                question(&app),
                "kill-session studio \u{b7} photos? It has 1 worktree session open (photos--feat-x). Close them too?"
            );
            assert!(
                press(&mut app, answer).is_none(),
                "{answer:?} is no, never a local close"
            );
            assert!(!app.overlay.is_open());
        }
        assert!(rig.sent().is_empty(), "{:?}", rig.sent());
        let _ = press(&mut app, KeyCode::Char('x'));
        let action = press(&mut app, KeyCode::Char('y')).expect("y runs the group close");
        send(&mut feed, &mut app, &action);
        answered(&mut feed, &mut app);
        assert_eq!(rig.sent(), ["workspace", "close", "$4", "--group"]);
    }

    /// `x` on the last pane of a machine's session with worktree sessions open asks the strict group question
    /// too, in the pane's words: `n` and Enter send nothing, `y` sends `--group` once.
    /// Mutation: the pane branch asks no group question on a machine (the plain pane question follows).
    #[test]
    fn x_on_a_machines_last_pane_with_worktree_sessions_asks_only_the_group_question() {
        let rig = Rig::new(None);
        let _fake = rig.fake(CLOSED, 0, false);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        with_a_worktree_session(&mut app);
        select(&mut app, "studio:%8");
        let asked = words::group_close_named(
            "kill-pane studio \u{b7} photos:1 (codex)? It is the session's last pane, so the session closes with it.",
            &["photos--feat-x"],
        );
        for answer in [KeyCode::Enter, KeyCode::Char('n')] {
            assert!(press(&mut app, KeyCode::Char('x')).is_none());
            assert_eq!(question(&app), asked);
            assert!(
                press(&mut app, answer).is_none(),
                "{answer:?} is no, never a pane close"
            );
            assert!(!app.overlay.is_open());
        }
        assert!(rig.sent().is_empty(), "{:?}", rig.sent());
        let _ = press(&mut app, KeyCode::Char('x'));
        let action = press(&mut app, KeyCode::Char('y')).expect("y runs the group close");
        send(&mut feed, &mut app, &action);
        answered(&mut feed, &mut app);
        assert_eq!(rig.sent(), ["workspace", "close", "$4", "--group"]);
    }

    /// A worktree session cut from a closed worktree of the machine's session is in the group: the closed record
    /// is namespaced for the list (`studio:w9`) and the record cut from its session names it as the machine does
    /// (`w9`), so the chain is followed in the machine's ids. `x` on photos names the open `photos--a--b`.
    /// Mutation: `linked_sessions_of` keeps the closed record's namespaced id (the chain is cut, no group question).
    #[test]
    fn x_on_a_machines_session_follows_the_chain_through_a_closed_worktree() {
        let rig = Rig::new(None);
        let mut app = rig.app();
        let photos = app
            .snap
            .spaces
            .iter()
            .find(|s| s.session_id == "studio:$4")
            .expect("studio's photos")
            .clone();
        let record = |name: &str, id: &str, from: &str, root: &str, path: &str| {
            crate::session::worktree::WorktreeRecord {
                workspace_id: id.into(),
                session_name: name.into(),
                path: path.into(),
                branch: name.into(),
                base: None,
                repo_root: root.into(),
                source_workspace_id: from.into(),
                created_unix_ms: 0,
                machine: photos.machine,
                real_path: None,
                real_repo_root: None,
            }
        };
        let root = photos.session_path.to_string_lossy().to_string();
        // Closed, cut from photos (`w4`); as the list holds it, its id is namespaced.
        app.worktrees = vec![record(
            "photos--a",
            "studio:w9",
            "w4",
            &root,
            "/home/pi/photos--a",
        )];
        // Open, cut from the session `photos--a` had (`w9`), as the machine sends it.
        app.snap.open_worktrees = vec![record(
            "photos--a--b",
            "w11",
            "w9",
            "/home/pi/photos--a",
            "/home/pi/photos--a--b",
        )];
        let mut open = photos.clone();
        open.session_id = "studio:$11".into();
        open.session_name = "photos--a--b".into();
        open.session_path = "/home/pi/photos--a--b".into();
        app.snap.spaces.push(open);
        select(&mut app, "studio:$4");
        assert!(press(&mut app, KeyCode::Char('x')).is_none());
        assert_eq!(
            question(&app),
            words::group_close_named("kill-session studio \u{b7} photos?", &["photos--a--b"])
        );
    }

    /// The group question is answered with `y` whether the list knows the worktree sessions (the question names
    /// them, and `y` sends `--group`) or not and only the far side's `workspace_group_close_required` told of
    /// them (the question says they are unseen, and `y` sends `--group` all the same).
    /// Mutation: `ask_group_close` builds its answer as `KillWorkspace` (the second case sends no `--group`).
    #[test]
    fn y_to_the_group_question_sends_group_whether_or_not_the_list_knows_the_sessions() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        with_a_worktree_session(&mut app);
        select(&mut app, "studio:$4");
        let _fake = rig.fake(CLOSED, 0, false);
        let _ = press(&mut app, KeyCode::Char('x'));
        let known = press(&mut app, KeyCode::Char('y')).expect("y answers");
        send(&mut feed, &mut app, &known);
        answered(&mut feed, &mut app);
        assert_eq!(rig.sent(), ["workspace", "close", "$4", "--group"]);
        drop(_fake);
        // The list does not know the sessions: a plain close is sent, and the far side asks for the group.
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:$4");
        let _fake = rig.fake(GROUP, 1, false);
        let plain = press(&mut app, KeyCode::Char('x'));
        assert!(plain.is_none() && question(&app) == "kill-session studio \u{b7} photos?");
        let close = press(&mut app, KeyCode::Char('y')).expect("y closes the one session");
        assert!(matches!(close, Action::KillWorkspace { .. }), "{close:?}");
        send(&mut feed, &mut app, &close);
        answered(&mut feed, &mut app);
        assert!(question(&app).starts_with("kill-session studio \u{b7} photos? "));
        let before = rig.sent().len();
        let _fake2 = rig.fake(CLOSED, 0, false);
        let group = press(&mut app, KeyCode::Char('y')).expect("y answers the far side's question");
        send(&mut feed, &mut app, &group);
        answered(&mut feed, &mut app);
        let sent = rig.sent();
        assert_eq!(
            sent[before..],
            ["workspace", "close", "$4", "--group"],
            "{sent:?}"
        );
    }

    /// The count in the toast of a group close is the far reply's (`worktree_workspace_ids`), not the list's: a
    /// list that knows nothing (the far side asked for the group) and a reply naming one id says one, a reply
    /// naming two says two even where the list knows one.
    /// Mutation: `done_words` takes the count from the list again (the first two cases say 0 and the last 1).
    #[test]
    fn the_group_close_toast_counts_the_sessions_the_far_reply_names() {
        let rig = Rig::new(None);
        let group = Action::KillGroup {
            session: "studio:$4".into(),
        };
        let reply = |ids: &str| {
            format!(
                r#"{{"id":"cli:workspace:close","result":{{"workspace_id":"w4","worktree_workspace_ids":{ids}}}}}"#
            )
        };
        let cases: [(bool, &str, &str); 3] = [
            (false, r#"["w11"]"#, "1 worktree session"),
            (false, r#"["w11","w12"]"#, "2 worktree sessions"),
            (true, r#"["w11","w12"]"#, "2 worktree sessions"),
        ];
        for (list_knows, ids, count) in cases {
            let (mut feed, mut app) = (rig.feed(), rig.app());
            if list_knows {
                with_a_worktree_session(&mut app);
            }
            select(&mut app, "studio:$4");
            let _fake = rig.fake(&reply(ids), 0, false);
            send(&mut feed, &mut app, &group);
            answered(&mut feed, &mut app);
            assert_eq!(
                toast_of(&app),
                Some(format!("closed studio \u{b7} photos and its {count}")),
                "list knows {list_knows}, reply {ids}"
            );
        }
    }

    /// Renaming a machine's agent is checked against that machine's agent names, not this Mac's: a name only a
    /// local agent holds is free on studio, and a name another studio agent holds is refused in the usual words.
    /// Mutation: `check_unique_by` filters on `machine.is_none()` again (the first name is then refused).
    #[test]
    fn a_machines_agent_is_renamed_against_that_machines_names() {
        let rig = Rig::new(None);
        let mut app = rig.app();
        let on_studio = |a: &crate::tui::data::AgentRow| a.machine == Some(0);
        let other = app
            .snap
            .agents
            .iter()
            .position(|a| on_studio(a) && a.pane_id != "studio:%8")
            .expect("another agent on studio");
        let local = app
            .snap
            .agents
            .iter()
            .position(|a| a.machine.is_none())
            .expect("a local agent");
        app.snap.agents[other].name = Some("scout".into());
        app.snap.agents[local].name = Some("mine".into());
        let ask = |app: &mut App, name: &str| {
            select(app, "studio:%8");
            let _ = press(app, KeyCode::Char('r'));
            let _ = press(app, KeyCode::Char('a'));
            assert!(
                matches!(app.overlay, Overlay::Prompt(_)),
                "toast {:?}",
                toast_of(app)
            );
            type_in(app, name);
            press(app, KeyCode::Enter)
        };
        assert_eq!(
            ask(&mut app, "mine"),
            Some(Action::RenameAgent {
                pane: "studio:%8".into(),
                name: "mine".into()
            }),
            "toast {:?}",
            toast_of(&app)
        );
        app.overlay = Overlay::None;
        assert!(ask(&mut app, "scout").is_none());
        assert!(
            matches!(&app.overlay, Overlay::Prompt(p) if p.error.as_deref() == Some("another agent is already named 'scout'")),
            "{:?}",
            toast_of(&app)
        );
    }

    /// A second action on a machine that is still answering sends nothing and says so.
    /// The check is [`Feed::perform_remote`]'s alone (it comes first; the worker's own refusal is silent).
    /// Mutation: `perform_remote` stops checking `busy` (the toast is then not said and this fails).
    #[test]
    fn a_second_action_on_the_same_machine_says_it_is_still_answering() {
        let rig = Rig::new(None);
        let _fake = rig.fake("{}", 0, true);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:%8");
        let close = Action::KillPane {
            pane: "studio:%8".into(),
        };
        send(&mut feed, &mut app, &close);
        assert!(feed.remote.busy("studio"));
        let first = rig.sent();
        send(&mut feed, &mut app, &close);
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("studio is still answering the last action")
        );
        assert_eq!(rig.sent(), first, "nothing more was sent");
    }

    /// An answer whose token is not the one sent, or that comes after the cursor moved, changes only the toast.
    /// Mutation: `settle` stops comparing the token (`sent.token == done.token`).
    #[test]
    fn a_stale_answer_changes_only_the_toast() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        let remove = Action::RemoveWorktree {
            workspace: "studio:w5".into(),
            force: false,
        };
        // The cursor moved before the machine answered.
        let _fake = rig.fake(DIRTY, 1, false);
        select(&mut app, "studio:%8");
        send(&mut feed, &mut app, &remove);
        select(&mut app, "studio:$4");
        answered(&mut feed, &mut app);
        assert!(!app.overlay.is_open());
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("studio answered after the list changed; look again before trying once more")
        );
        // The token is not the one sent.
        app.toast("");
        send(&mut feed, &mut app, &remove);
        feed.remote.pending.get_mut("studio").unwrap().token += 1;
        answered(&mut feed, &mut app);
        assert!(!app.overlay.is_open());
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("studio answered after the list changed; look again before trying once more")
        );
    }

    /// Two seconds after a verb began, on the app's own clock, the hint line says it is waiting; a second
    /// before, it says nothing.
    /// Mutation: `settle` stops asking the worker for its hint.
    #[test]
    fn the_hint_comes_after_two_seconds_on_the_fixed_clock() {
        let rig = Rig::new(None);
        let _fake = rig.fake("{}", 0, true);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:%8");
        send(
            &mut feed,
            &mut app,
            &Action::KillPane {
                pane: "studio:%8".into(),
            },
        );
        app.clock_ms = Some(NOW + 1_999);
        assert!(feed.remote.settle(&mut app).is_empty());
        assert_eq!(toast_of(&app), None);
        app.clock_ms = Some(NOW + 2_000);
        assert!(feed.remote.settle(&mut app).is_empty());
        assert_eq!(toast_of(&app).as_deref(), Some("waiting for studio"));
    }

    /// The hint is said once per verb, and never over another toast: a busy toast survives the two-second mark,
    /// the hint waits for an empty slot, and once said it is not said again.
    /// Mutation: `take_hint` does not mark the verb as hinted (it comes back), or `settle` drops the
    /// `live_toast().is_none()` check (the busy toast is overwritten).
    #[test]
    fn the_hint_is_said_once_and_never_over_another_toast() {
        let rig = Rig::new(None);
        let _fake = rig.fake("{}", 0, true);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:%8");
        let close = Action::KillPane {
            pane: "studio:%8".into(),
        };
        send(&mut feed, &mut app, &close);
        send(&mut feed, &mut app, &close);
        let busy = "studio is still answering the last action";
        assert_eq!(toast_of(&app).as_deref(), Some(busy));
        app.clock_ms = Some(NOW + 2_500);
        for _ in 0..3 {
            assert!(feed.remote.settle(&mut app).is_empty());
            assert_eq!(toast_of(&app).as_deref(), Some(busy), "the hint waits");
        }
        app.toast = None;
        assert!(feed.remote.settle(&mut app).is_empty());
        assert_eq!(toast_of(&app).as_deref(), Some("waiting for studio"));
        app.toast = None;
        app.clock_ms = Some(NOW + 3_000);
        assert!(feed.remote.settle(&mut app).is_empty());
        assert_eq!(toast_of(&app), None, "said once, not again");
    }

    /// A verb that ran says what the popup says of its own: a group close, an agent rename and a worktree removal
    /// have a toast (the machine named where the popup names the session); a pane close says nothing.
    /// Mutation: `done_words` returns `None` for `KillGroup`.
    #[test]
    fn a_verb_that_ran_says_what_the_popup_says_of_its_own() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        with_a_worktree_session(&mut app);
        select(&mut app, "studio:$4");
        assert_eq!(app.group_size("studio:$4"), 2, "the list knows one");
        let _fake = rig.fake(CLOSED, 0, false);
        send(
            &mut feed,
            &mut app,
            &Action::KillGroup {
                session: "studio:$4".into(),
            },
        );
        answered(&mut feed, &mut app);
        // A reply that names no ids: the list's count (one) is the fallback.
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("closed studio \u{b7} photos and its 1 worktree session")
        );
        select(&mut app, "studio:%8");
        send(
            &mut feed,
            &mut app,
            &Action::RenameAgent {
                pane: "studio:%8".into(),
                name: "scribe".into(),
            },
        );
        answered(&mut feed, &mut app);
        assert_eq!(
            toast_of(&app),
            Some(crate::tui::actions::agent_renamed_toast("scribe"))
        );
        drop(_fake);
        let _fake = rig.fake(
            r#"{"id":"cli:worktree:remove","result":{"worktree":{"workspace_id":"w5","branch":"feat-x"}}}"#,
            0,
            false,
        );
        send(
            &mut feed,
            &mut app,
            &Action::RemoveWorktree {
                workspace: "studio:w5".into(),
                force: false,
            },
        );
        answered(&mut feed, &mut app);
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("removed worktree feat-x on studio")
        );
    }

    /// A worktree made after the cursor moved is not silent: it says the worktree is there and where Enter goes.
    /// Mutation: the stale `Done` arm says nothing for `NewWorktree`.
    #[test]
    fn a_worktree_made_after_the_cursor_moved_says_so_and_goes_nowhere() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:$4");
        let _fake = rig.fake(CREATED, 0, false);
        send(
            &mut feed,
            &mut app,
            &Action::NewWorktree {
                workspace: "studio:$4".into(),
                branch: "feat-x".into(),
            },
        );
        select(&mut app, "studio:%8");
        assert!(answered(&mut feed, &mut app).is_empty(), "no go-in");
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("worktree made on studio: Enter on its row goes in")
        );
    }

    /// `w` asks for a branch, sends `worktree create ... --no-focus`, says it is opening, and on the answer goes
    /// in by the worktree's `workspace_id` (the route Enter on a closed worktree takes). A refusal opens nothing.
    /// Mutation: `created` reads another field (`session_name`).
    #[test]
    fn a_new_worktree_is_created_on_the_machine_and_gone_in_by_its_workspace_id() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:$4");
        assert!(press(&mut app, KeyCode::Char('w')).is_none());
        type_in(&mut app, "feat-x");
        let action = press(&mut app, KeyCode::Enter).expect("the branch is the answer");
        let guard = rig.fake(CREATED, 0, false);
        send(&mut feed, &mut app, &action);
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("opening worktree on studio")
        );
        let routes = answered(&mut feed, &mut app);
        assert_eq!(
            routes,
            [Route::OpenFirst {
                machine: "studio".into(),
                workspace: "w7".into()
            }]
        );
        assert_eq!(
            rig.sent(),
            [
                "worktree",
                "create",
                "--workspace",
                "$4",
                "--branch",
                "feat-x",
                "--no-focus"
            ]
        );
        drop(guard);
        let _fake = rig.fake(NOT_A_REPO, 1, false);
        send(&mut feed, &mut app, &action);
        assert!(answered(&mut feed, &mut app).is_empty(), "nothing opens");
        assert!(
            toast_of(&app)
                .unwrap()
                .starts_with("studio: not_a_git_repo")
        );
    }

    const OPENED: &str = r#"{"id":"cli:open","result":{"type":"session_open","session":"garden","created":true,"collision":null,"switched":false,"dir":"/home/studio/code/garden"}}"#;
    const NO_DIR: &str = r#"{"id":"cli:open","error":{"code":"not_found","message":"no directory matches nothing-like-it"}}"#;

    /// `n` on a session row of studio: a prompt titled for the machine; Esc closes it, Enter on an empty line does
    /// nothing, and Enter on a term gives the action for studio.
    /// Mutation: `n` opens this Mac's picker for a machine's row (`new_session` goes straight to the picker).
    #[test]
    fn n_on_a_machines_row_asks_for_a_folder_on_that_machine() {
        let rig = Rig::new(None);
        let mut app = rig.app();
        select(&mut app, "studio:$4");
        assert!(press(&mut app, KeyCode::Char('n')).is_none());
        match &app.overlay {
            Overlay::Prompt(p) => {
                assert_eq!(p.title, "new session on studio: a folder or a zoxide term")
            }
            other => panic!("not the prompt: {other:?}"),
        }
        assert!(press(&mut app, KeyCode::Enter).is_none());
        assert!(
            matches!(app.overlay, Overlay::Prompt(_)),
            "an empty line does nothing"
        );
        let _ = press(&mut app, KeyCode::Esc);
        assert!(matches!(app.overlay, Overlay::None), "Esc closes it");
        // The header line and the empty-machine row of the machine aim at it too.
        for row in [Row::Machine(Some(0)), Row::NoSession(0)] {
            app.rows.rows.push(row);
            app.sel = app.rows.rows.len() - 1;
            let _ = press(&mut app, KeyCode::Char('n'));
            assert!(
                matches!(&app.overlay, Overlay::Prompt(p) if p.title.contains("on studio")),
                "{:?}",
                toast_of(&app)
            );
            let _ = press(&mut app, KeyCode::Esc);
        }
        select(&mut app, "studio:$4");
        let _ = press(&mut app, KeyCode::Char('n'));
        type_in(&mut app, "-odd");
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Some(Action::NewRemoteSession {
                machine: "studio".into(),
                term: "-odd".into()
            })
        );
    }

    /// A local row still opens this Mac's picker, a machine that is away says why, and the popup says the row is
    /// read only.
    /// Mutation: the away and popup checks go (`!header.live || !self.is_app()` becomes false).
    #[test]
    fn n_on_a_local_row_opens_the_picker_and_elsewhere_it_refuses() {
        let rig = Rig::new(None);
        let mut app = rig.app();
        let local = app
            .rows
            .rows
            .iter()
            .position(|row| matches!(row, Row::Pane(p) if app.snap.panes[*p].machine.is_none()))
            .expect("a local pane");
        app.sel = local;
        let _ = press(&mut app, KeyCode::Char('n'));
        assert!(
            matches!(app.overlay, Overlay::Project(_)),
            "{:?}",
            toast_of(&app)
        );

        let mut away = app_over(&rig.cfg, five_and_two("idle", false));
        away.enter_app_mode(None, None);
        let pi = away
            .rows
            .rows
            .iter()
            .position(|row| matches!(row, Row::Session(s) if away.snap.spaces[*s].session_id.starts_with("pi:")))
            .expect("a session of pi, which is away");
        away.sel = pi;
        let _ = press(&mut away, KeyCode::Char('n'));
        assert!(matches!(away.overlay, Overlay::None));
        assert!(
            toast_of(&away).unwrap().starts_with("pi is not connected"),
            "{:?}",
            toast_of(&away)
        );
    }

    /// The far argv is exactly `open --json --no-switch -- <term>` (a dash term stays a term), a second press while
    /// it runs sends nothing, a success says made or already there with the folder, and when the list shows the
    /// session the row is selected and the route is Enter's.
    /// Mutation: `remote_argv` drops `--no-switch` or the `--`.
    #[test]
    fn a_session_made_on_the_machine_is_said_and_gone_in_to() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        let name = app
            .snap
            .spaces
            .iter()
            .find(|s| s.session_id == "studio:$4")
            .unwrap()
            .session_name
            .clone();
        let opened = OPENED.replace(r#""session":"garden""#, &format!(r#""session":"{name}""#));
        let guard = rig.fake(&opened, 0, false);
        select(&mut app, "studio:$4");
        let action = Action::NewRemoteSession {
            machine: "studio".into(),
            term: "-odd".into(),
        };
        send(&mut feed, &mut app, &action);
        send(&mut feed, &mut app, &action);
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("studio is still answering the last action")
        );
        let routes = answered(&mut feed, &mut app);
        assert_eq!(
            rig.sent(),
            ["open", "--json", "--no-switch", "--", "-odd"],
            "sent once"
        );
        assert_eq!(
            toast_of(&app),
            Some(words::session_made(
                "studio",
                &name,
                "/home/studio/code/garden",
                true
            ))
        );
        assert_eq!(routes.len(), 1, "{routes:?}");
        assert!(
            matches!(&routes[0], Route::Attach { machine, target, .. } if machine == "studio" && target == "$4"),
            "{routes:?}"
        );
        assert!(
            matches!(app.current(), Some(Row::Session(s)) if app.snap.spaces[*s].session_id == "studio:$4")
        );
        drop(guard);
        // Already there says so.
        let _again = rig.fake(&opened.replace("true", "false"), 0, false);
        send(&mut feed, &mut app, &action);
        let _ = answered(&mut feed, &mut app);
        assert_eq!(
            toast_of(&app),
            Some(words::session_made(
                "studio",
                &name,
                "/home/studio/code/garden",
                false
            ))
        );
    }

    /// A row that never shows leaves nothing selected after the wait and nothing hangs.
    /// Mutation: the wait never ends (`SESSION_WAIT` is not looked at).
    #[test]
    fn a_session_row_that_never_shows_selects_nothing_and_waits_for_nothing() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        let _fake = rig.fake(
            &OPENED.replace("garden\",\"created", "nowhere-listed\",\"created"),
            0,
            false,
        );
        select(&mut app, "studio:$4");
        let sel = app.sel;
        send(
            &mut feed,
            &mut app,
            &Action::NewRemoteSession {
                machine: "studio".into(),
                term: "garden".into(),
            },
        );
        assert!(answered(&mut feed, &mut app).is_empty());
        assert!(
            feed.remote.awaiting.is_some(),
            "still waiting inside the wait"
        );
        feed.remote.awaiting.as_mut().unwrap().since =
            Instant::now() - SESSION_WAIT - Duration::from_secs(1);
        assert!(feed.remote.settle(&mut app).is_empty());
        assert!(feed.remote.awaiting.is_none(), "given up");
        assert_eq!(app.sel, sel, "nothing selected");
        assert!(
            toast_of(&app)
                .unwrap()
                .starts_with("made session nowhere-listed on studio")
        );
    }

    /// A far error is toasted in the far words, an old taw's usage error says to update it.
    /// Mutation: `Reply::Usage` reads as the generic refusal.
    #[test]
    fn a_far_error_is_toasted_and_an_old_taw_is_told_to_update() {
        let rig = Rig::new(None);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        select(&mut app, "studio:$4");
        let action = Action::NewRemoteSession {
            machine: "studio".into(),
            term: "nothing-like-it".into(),
        };
        let guard = rig.fake(NO_DIR, 1, false);
        send(&mut feed, &mut app, &action);
        assert!(answered(&mut feed, &mut app).is_empty());
        assert_eq!(
            toast_of(&app).as_deref(),
            Some("studio: no directory matches nothing-like-it")
        );
        drop(guard);
        let _old = rig.fake("", 2, false);
        send(&mut feed, &mut app, &action);
        assert!(answered(&mut feed, &mut app).is_empty());
        assert_eq!(
            toast_of(&app),
            Some("update taw on studio to make sessions there from here".to_string())
        );
    }

    /// This Mac's `pre_close` is asked for this Mac's row and never for a machine's.
    /// Mutation: `Feed::perform` stops sending a machine's action to `perform_remote`.
    #[test]
    fn this_macs_hooks_are_never_asked_about_a_remote_row() {
        let dir = std::env::temp_dir().join(format!("taw-hook-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let hook = dir.join("hook.txt");
        let _ = fs::remove_file(&hook);
        let rig = Rig::new(Some(&format!("echo asked >> '{}'", hook.display())));
        let _fake = rig.fake(CLOSED, 0, false);
        let (mut feed, mut app) = (rig.feed(), rig.app());
        let taw = Path::new("/nonexistent/taw");
        let own = Action::KillPane { pane: "%3".into() };
        crate::tui::actions::exec(&rig.tmux, &rig.cfg, &rig.paths, taw, &own, None).unwrap();
        let asked = fs::read_to_string(&hook)
            .unwrap_or_default()
            .lines()
            .count();
        assert!(asked > 0, "the recorder works for this Mac's own row");
        send(
            &mut feed,
            &mut app,
            &Action::KillPane {
                pane: "studio:%8".into(),
            },
        );
        answered(&mut feed, &mut app);
        assert_eq!(rig.sent(), ["pane", "close", "%8"]);
        let after = fs::read_to_string(&hook)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(after, asked, "a remote row asked this Mac's hook");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `unprefixed` takes the machine's name off every id of an action and leaves this Mac's actions alone.
    #[test]
    fn unprefixed_names_the_machine_and_strips_its_ids() {
        let stripped = unprefixed(&Action::RenameWindow {
            window: "studio:@5".into(),
            name: "x".into(),
        });
        assert_eq!(
            stripped,
            Some((
                "studio".to_string(),
                Action::RenameWindow {
                    window: "@5".into(),
                    name: "x".into()
                }
            ))
        );
        assert_eq!(unprefixed(&Action::KillPane { pane: "%8".into() }), None);
        assert_eq!(
            unprefixed(&Action::SwitchWorkspace {
                session: "studio:$4".into()
            }),
            None
        );
    }
}
