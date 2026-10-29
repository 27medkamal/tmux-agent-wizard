//! What each command means on the row the cursor is on.
//!
//! Split out of `app.rs`, which stays the state machine (movement, attention, filter, key
//! and mouse dispatch): this file is the command surface — which [`Action`] a row produces,
//! what a confirmation says before a destructive one runs, which overlay a command opens,
//! and the validation a submitted prompt has to pass. One row kind is one `match` arm
//! everywhere, so a new kind of row cannot be half-supported.
//!
//! Closing a window, splitting and opening a new window are tmux's own keys, so no row offers
//! them.

use super::App;
use crate::machine::{split_id, words};
use crate::session::naming;
use crate::session::worktree::{LinkedSession, SessionRef, linked_among};
use crate::tui::actions::{self, Action, Leave, plan_leave};
use crate::tui::data::{AgentRow, MachineHeader, SpaceRow};
use crate::tui::keys::Action as KeyAction;
use crate::tui::overlays::state::PASTE_IMAGE_ENTRY;
use crate::tui::overlays::{Confirm, Item, Menu, Overlay, Prompt, PromptKind, RowAction, Scroll};
use crate::tui::picker::{FarAnswer, ProjectPicker};
use crate::tui::tree::{self, Row};

/// What one pane row needs to name a tmux target, lifted out of the snapshot so the
/// borrow is over before an overlay is opened or a confirmation asked for.
struct PaneTarget {
    pane: String,
    session: String,
    window: String,
    window_index: u32,
    window_name: String,
    /// How many panes that window has: one means closing this pane closes the window, and
    /// means tmux's target spelling leaves the pane index off.
    window_panes: u32,
    pane_index: u32,
    session_name: String,
    /// [`tree::where_label`] for this pane — the `index:name[.pane]` the row itself shows.
    where_label: String,
    what: String,
}

/// What a confirmation says of the last pane of a session's last window: tmux closes the
/// window with it, and the session with the window. The group question leads with it too, and the README's
/// remote words table quotes it in the question a machine's last pane asks (`tests/remote_docs.rs` pins that
/// row through this constant, so rewording it fails the pin until the README says the same).
pub const SESSION_LAST_PANE: &str = "It is the session's last pane, so the session closes with it.";

/// What it says when other sessions are grouped with that one (`new-session -t`): the sessions of
/// a group share their windows, so closing the window closes every one of them.
const GROUPED_SESSION_LAST_PANE: &str =
    "It is the session's last pane, so the session and the sessions grouped with it close.";

/// What a confirmation says of the last pane of a window that has a neighbour.
const WINDOW_LAST_PANE: &str = "It is the window's last pane, so the window closes too.";

/// What the confirmation says of the last pane of `space`'s last window.
fn session_last_pane(space: &SpaceRow) -> &'static str {
    if space.group_size > 1 {
        GROUPED_SESSION_LAST_PANE
    } else {
        SESSION_LAST_PANE
    }
}

/// The question `x` on a session with worktree sessions open asks: `lead`, which says what
/// would close, then which worktree sessions are open (three named at most), and whether they
/// go too.
fn group_question(lead: &str, linked: &[LinkedSession]) -> String {
    let names: Vec<&str> = linked.iter().map(|l| l.session_name.as_str()).collect();
    words::group_close_named(lead, &names)
}

/// What the confirmation adds of where a close leaves the client, for a close with one answer: nothing
/// when the close does not take the client's own session. `group` is a close of a session and the worktree
/// sessions cut from it, which says "no session is left" where a single close says "your only session".
fn landing_words(plan: &Leave, name: &dyn Fn(&str) -> String, group: bool) -> String {
    match plan {
        Leave::Stay => String::new(),
        Leave::GoTo(id) => format!(" You go to {}.", name(id)),
        Leave::NoneLeft if group => " No session is left: tmux ends.".to_string(),
        Leave::NoneLeft => " It is your only session: tmux ends.".to_string(),
    }
}

/// [`landing_words`] for the question with two answers: `yes` is where `y` (the group) leaves the client,
/// `no` where Enter (just the one session) does. One sentence when both go the same way, else both.
fn choice_landing_words(yes: &Leave, no: &Leave, name: &dyn Fn(&str) -> String) -> String {
    if yes == no {
        return landing_words(yes, name, true);
    }
    let clause = |plan: &Leave| match plan {
        Leave::Stay => "you stay".to_string(),
        Leave::GoTo(id) => format!("you go to {}", name(id)),
        Leave::NoneLeft => "tmux ends".to_string(),
    };
    format!(
        " If you close them too, {}; if not, {}.",
        clause(yes),
        clause(no)
    )
}

impl App<'_> {
    /// Where the client goes when the sessions `closing` close, on this machine: `Stay` when the popup
    /// does not know its client's session. The last session is the client's own `#{client_last_session}`
    /// when the snapshot could read it, tmux's order is the list's.
    fn landing_after(&self, closing: &[&str]) -> Leave {
        let Some(own) = self.snap.current_session.as_deref() else {
            return Leave::Stay;
        };
        let order: Vec<&str> = self
            .snap
            .spaces
            .iter()
            .filter(|s| s.machine.is_none())
            .map(|s| s.session_id.as_str())
            .collect();
        plan_leave(closing, own, self.snap.previous_session.as_deref(), &order)
    }

    /// [`landing_words`] for closing `session` alone on this machine.
    fn landing_words_for(&self, session: &str) -> String {
        let name = |id: &str| self.session_name_of(id);
        landing_words(&self.landing_after(&[session]), &name, false)
    }

    /// [`choice_landing_words`] for `session`, with `linked` the sessions `y` closes with it.
    fn choice_landing_words_for(&self, session: &str, linked: &[LinkedSession]) -> String {
        let name = |id: &str| self.session_name_of(id);
        let mut group: Vec<&str> = linked.iter().map(|l| l.session_id.as_str()).collect();
        group.push(session);
        choice_landing_words(
            &self.landing_after(&group),
            &self.landing_after(&[session]),
            &name,
        )
    }

    /// The running worktree sessions cut from session `space` (`session::worktree::linked_among`,
    /// the group `taw workspace close --group` closes), out of what the popup already holds:
    /// the tracked worktrees and the sessions in its snapshot.
    ///
    /// Only the machine `space` is on is looked at: another machine's record keeps the ids and the
    /// names that machine gave (`w3`, `garden`), which say nothing of a session here.
    fn linked_sessions_of(&self, space: usize) -> Vec<LinkedSession> {
        let Some(source) = self.snap.spaces.get(space) else {
            return Vec::new();
        };
        let machine = source.machine;
        // A machine's records hold the ids that machine gave (`w4`), so its sessions are compared by those:
        // the list's `studio:$4` loses the machine's name.
        let prefix = machine
            .and_then(|at| self.snap.machines.get(at))
            .map(|header| format!("{}:", header.name));
        let prefix = prefix.as_deref();
        fn as_ref<'a>(prefix: Option<&str>, s: &'a SpaceRow) -> SessionRef<'a> {
            SessionRef {
                id: prefix
                    .and_then(|prefix| s.session_id.strip_prefix(prefix))
                    .unwrap_or(&s.session_id),
                name: &s.session_name,
                // A machine's session is compared by its folder resolved there when the feed sent one.
                path: s.real_path.as_deref().unwrap_or(&s.session_path),
            }
        }
        let running: Vec<SessionRef<'_>> = self
            .snap
            .spaces
            .iter()
            .filter(|s| s.machine == machine)
            .map(|s| as_ref(prefix, s))
            .collect();
        // A machine's closed records, and the ones its sessions run ([`Snapshot::open_worktrees`]): the
        // machine sends the first as rows and the second beside them, so that its running worktree
        // sessions can be told from its other sessions.
        let records: Vec<_> = self
            .worktrees
            .iter()
            .chain(self.snap.open_worktrees.iter())
            .filter(|r| r.machine == machine)
            .cloned()
            .map(|mut record| {
                // A closed record's id is namespaced for the list (`studio:w9`), while a record cut from its
                // session names it by the machine's own (`w9`): the whole chain is compared in the machine's.
                if record.machine.is_some() {
                    record.workspace_id = split_id(&record.workspace_id).1.to_string();
                }
                record
            })
            .collect();
        linked_among(&records, &running, as_ref(prefix, source))
    }

    /// Does `what` to the cursor row. The keys and the lettered menus share this, so a
    /// menu entry can never drift from the key with the same letter.
    pub(super) fn row_action(&mut self, what: RowAction) -> Option<Action> {
        // Another machine's rows are not acted on from here: a machine that is away says so, and
        // one that is live says its rows are acted on from the app. The one thing the app does on a
        // live machine's row is go in (every path that means Go comes through here).
        // Options is read-only, and paste image says for itself (it is the one verb a live machine's row takes in
        // the popup too, which sends the image over ssh itself).
        if !matches!(what, RowAction::Options | RowAction::PasteImage)
            && let Some(text) = self.refusal_for(self.sel, what)
        {
            self.toast(text);
            return None;
        }
        match what {
            RowAction::Go => self.go(),
            RowAction::Rename => {
                self.rename();
                None
            }
            RowAction::RenameWindow => {
                self.rename_window();
                None
            }
            RowAction::NameAgent => {
                self.name_agent();
                None
            }
            RowAction::Close => self.close_row(),
            RowAction::NewWorktree => {
                self.open_new_worktree();
                None
            }
            RowAction::Options => {
                self.overlay = Overlay::Settings(Scroll::default(), self.settings.clone());
                None
            }
            RowAction::PasteImage => self.paste_image(),
        }
    }

    /// What a row of another machine says when it is acted on, or `None` for a row of this machine's
    /// (or no row), and for a live machine's row in the app, which is acted on there: [`words::not_connected`]
    /// for a machine that is away, [`words::read_only`] for one that is live in the popup.
    pub(super) fn refusal_at(&self, index: usize) -> Option<String> {
        self.machine_at(index)
            .filter(|header| !(header.live && self.is_app()))
            .map(|header| self.refusal(header))
    }

    /// [`App::refusal_at`] for `what`: in the popup going into a live machine's row is the one action that is not
    /// refused (it is the doorway, [`App::doorway_for_row`]); in the app every action on a live machine's row
    /// produces the action this Mac's row produces, with the machine's ids, which the screen sends to the machine.
    /// A machine that is away refuses every action.
    pub(super) fn refusal_for(&self, index: usize, what: RowAction) -> Option<String> {
        self.machine_at(index)
            .filter(|header| !(header.live && (what == RowAction::Go || self.is_app())))
            .map(|header| self.refusal(header))
    }

    /// The header of the machine row `index` belongs to, `None` for a row of this machine's.
    pub(super) fn machine_at(&self, index: usize) -> Option<&MachineHeader> {
        let row = self.rows.rows.get(index)?;
        let machine = tree::space_of(&self.snap, row)
            .and_then(|space| self.snap.spaces.get(space))?
            .machine?;
        self.snap.machines.get(machine)
    }

    /// Unix milliseconds now: the fixed clock when one is set, the system's otherwise. The only
    /// place the popup reads the time that a machine's age is told against.
    pub(crate) fn wall_ms(&self) -> u64 {
        self.clock_ms.unwrap_or_else(crate::tui::machines::now_ms)
    }

    /// What acting on a row of `header`'s machine says.
    pub(super) fn refusal(&self, header: &MachineHeader) -> String {
        if header.live {
            words::read_only(&header.name)
        } else {
            let now = self.wall_ms();
            let seen = header
                .last_seen
                .map_or(0, |at| now.saturating_sub(at) / 1000);
            words::not_connected(&header.name, seen)
        }
    }

    /// `Enter`: go to the row under the cursor, whoever put it there. A session header is
    /// the session as you left it (`switch-client` only). A directory the filter lists is
    /// opened: its session switched to, or created first. Your own row ([`App::own_row`]:
    /// the pane you came from, or your session's header when that pane is not drawn) is
    /// where you already are, so the popup closes and no tmux command runs. Any other row
    /// — the first row a flat list opens on included — goes: its pane, or its parked
    /// worktree reopened. For a moment after a close ([`super::SETTLE`]) it does nothing at
    /// all: that Enter was meant for the close's confirmation, and the row under the cursor
    /// is the one that slid into the closed one's place. With no row it says so, and names
    /// the key that makes a session when one is bound: the search preset binds none, and its
    /// `n` is a letter of the query.
    fn go(&mut self) -> Option<Action> {
        if self.settling() {
            return None;
        }
        // A header is a line, not a row: the cursor is on one only when nothing else is listed (a
        // machine that has sent no sessions yet), and that is "nothing here" as an empty list is.
        if self
            .current()
            .is_none_or(|row| matches!(row, Row::Machine(_)))
        {
            self.toast(match self.keymap.short_key(KeyAction::NewSession) {
                Some(key) => format!("nothing here; {key} creates a session"),
                None => "nothing here".to_string(),
            });
            return None;
        }
        if Some(self.sel) == self.own_row() {
            self.quit = true;
            return None;
        }
        self.go_to_row(self.sel)
    }

    /// The plain Go for the row at `at`.
    fn go_to_row(&mut self, at: usize) -> Option<Action> {
        // The popup does not go into a machine itself: its Enter on a machine's row ends the popup and hands the
        // client to the app.
        if !self.is_app() && self.machine_at(at).is_some() {
            return self.doorway_for_row(at);
        }
        match self.rows.rows.get(at).cloned()? {
            Row::Pane(pane) => {
                let p = self.snap.panes.get(pane)?;
                Some(Action::Jump {
                    session: p.session_id.clone(),
                    window: p.window_id.clone(),
                    pane: p.pane_id.clone(),
                })
            }
            Row::Session(space) => Some(Action::SwitchWorkspace {
                session: self.snap.spaces.get(space)?.session_id.clone(),
            }),
            // A parked worktree has no session yet; `open` creates one at its checkout and
            // switches to it, after which it is a session row like any other.
            Row::ClosedWorktree { record, .. } => Some(Action::OpenWorktree {
                workspace: self.worktrees.get(record)?.workspace_id.clone(),
            }),
            // A directory the filter lists: the open the picker's Enter runs, which switches
            // to the session already running there or creates one first.
            Row::Project(project) => Some(Action::OpenProject {
                path: self.projects.get(project)?.path.clone(),
            }),
            // A machine's header is a line, not a stop: the cursor is never on one.
            Row::Machine(_) => None,
            // A connected machine with no session: Enter starts one there. The popup's way is the doorway into
            // the app on `studio:new`; the app's own is the same action, which its screen routes to the attach.
            Row::NoSession(machine) => Some(Action::Doorway {
                machine: self.snap.machines.get(machine)?.name.clone(),
                id: words::NEW_ID.to_string(),
            }),
        }
    }

    /// The popup's Enter on a live machine's row: the doorway into the app, on that machine's own id (`%16` for
    /// a pane, `$3` for a session). A closed worktree is not one: it is acted on from the app, and says so.
    fn doorway_for_row(&mut self, at: usize) -> Option<Action> {
        let id = match self.rows.rows.get(at)? {
            Row::Pane(pane) => self.snap.panes.get(*pane)?.pane_id.clone(),
            Row::Session(space) => self.snap.spaces.get(*space)?.session_id.clone(),
            // Answers the Enter just pressed, so it does not say "Enter goes there"; x, r and w keep `read_only`.
            Row::ClosedWorktree { .. } => {
                let header = self.machine_at(at)?;
                let said = if header.live {
                    words::acted_on_from_the_app(&header.name)
                } else {
                    self.refusal(header)
                };
                self.toast(said);
                return None;
            }
            _ => return None,
        };
        match crate::machine::split_id(&id) {
            (Some(machine), raw) => Some(Action::Doorway {
                machine: machine.to_string(),
                id: raw.to_string(),
            }),
            (None, _) => None,
        }
    }

    /// `x`: close the row, naming the tmux command in the confirmation.
    fn close_row(&mut self) -> Option<Action> {
        let row = self.current().cloned()?;
        // On a machine's row every sentence names the machine, as `kill-pane studio \u{b7} garden:1 (claude)?`
        // does: the same sentences as for this Mac's row, built by the same code.
        let machine = self.machine_at(self.sel).map(|header| header.name.clone());
        let named = |text: String| match &machine {
            Some(machine) => words::on_machine(machine, &text),
            None => text,
        };
        match row {
            Row::Pane(_) => {
                let t = self.pane_target(&row)?;
                // The last pane of a window takes the window with it, so tmux's own target
                // spelling drops the pane index, and the prompt says what will happen.
                let last = t.window_panes <= 1;
                let target = named(self.target_text(&t));
                // And the last pane of a session's last window takes the session with it: a
                // session close, which says so and asks what closing the session asks when
                // worktree sessions cut from it are still running. With confirmations off
                // there is nobody to ask, and the answer the question defaults to (just this
                // pane) is what `x` has always done.
                let space = self
                    .snap
                    .spaces
                    .iter()
                    .position(|s| s.session_id == t.session)
                    .filter(|&space| last && self.snap.spaces[space].windows <= 1);
                if let Some(space) = space
                    && self.cfg.ui.confirm_close
                {
                    let linked = self.linked_sessions_of(space);
                    if !linked.is_empty() {
                        let said = session_last_pane(&self.snap.spaces[space]);
                        let lead = format!("kill-pane {target} ({})? {said}", t.what);
                        let landing =
                            match machine.is_some() || self.snap.spaces[space].group_size > 1 {
                                true => String::new(),
                                false => self.choice_landing_words_for(&t.session, &linked),
                            };
                        // A machine's far verb always refuses a plain close of a session with worktree
                        // sessions open, so there is no "just this one": only the group question, only `y`.
                        self.overlay = Overlay::Confirm(if machine.is_some() {
                            Confirm::strict(
                                group_question(&lead, &linked),
                                Action::KillGroup { session: t.session },
                            )
                        } else {
                            Confirm::choice(
                                format!("{}{landing}", group_question(&lead, &linked)),
                                Action::KillGroup { session: t.session },
                                Action::KillPane { pane: t.pane },
                            )
                        });
                        return None;
                    }
                }
                let what = &t.what;
                let message = match (space, last) {
                    (Some(space), _) => {
                        let said = session_last_pane(&self.snap.spaces[space]);
                        let landing = if machine.is_some() || self.snap.spaces[space].group_size > 1
                        {
                            String::new()
                        } else {
                            self.landing_words_for(&t.session)
                        };
                        format!("kill-pane {target} ({what})? {said}{landing}")
                    }
                    (None, true) => format!("kill-pane {target} ({what})? {WINDOW_LAST_PANE}"),
                    (None, false) => format!("kill-pane {target} ({what})?"),
                };
                self.guard(message, Action::KillPane { pane: t.pane })
            }
            Row::Session(space) => {
                let s = self.snap.spaces.get(space)?;
                let (name, session) = (named(s.session_name.clone()), s.session_id.clone());
                let linked = self.linked_sessions_of(space);
                // Worktree sessions cut from this one are still running: closing it leaves
                // them open unless the user says otherwise. With confirmations off there is
                // nobody to ask, and the answer a question like this defaults to (just this
                // session) is what `x` has always done.
                // What a machine's rows say stays as it was: where the client goes is this machine's.
                let (landing, group_landing) = if machine.is_some() {
                    (String::new(), String::new())
                } else {
                    (
                        self.landing_words_for(&session),
                        self.choice_landing_words_for(&session, &linked),
                    )
                };
                if linked.is_empty() || !self.cfg.ui.confirm_close {
                    return self.guard(
                        format!("kill-session {name}?{landing}"),
                        Action::KillWorkspace { session },
                    );
                }
                let question = format!(
                    "{}{group_landing}",
                    group_question(&format!("kill-session {name}?"), &linked)
                );
                let group = Action::KillGroup {
                    session: session.clone(),
                };
                // A machine's far verb always refuses a plain close of a session with worktree sessions
                // open: only the group question is asked, and only `y` runs it.
                self.overlay = Overlay::Confirm(if machine.is_some() {
                    Confirm::strict(question, group)
                } else {
                    Confirm::choice(question, group, Action::KillWorkspace { session })
                });
                None
            }
            Row::ClosedWorktree { record, .. } => {
                let r = self.worktrees.get(record)?;
                let message = format!(
                    "git worktree remove {}? (the branch is kept)",
                    named(r.branch.clone())
                );
                let workspace = r.workspace_id.clone();
                self.guard(
                    message,
                    Action::RemoveWorktree {
                        workspace,
                        force: false,
                    },
                )
            }
            // A directory the filter lists is not open: there is nothing to close.
            Row::Project(_) => None,
            Row::Machine(_) | Row::NoSession(_) => None,
        }
    }

    /// What identifies the row under the cursor across a redraw: its pane, its session, or its worktree's
    /// workspace id, as the rows carry them (a machine's own are namespaced). A machine's answer to an action
    /// is acted on only while the cursor is still on the row the action was for.
    pub(crate) fn cursor_id(&self) -> Option<String> {
        match self.current()? {
            Row::Pane(pane) => self.snap.panes.get(*pane).map(|p| p.pane_id.clone()),
            Row::Session(space) => self.snap.spaces.get(*space).map(|s| s.session_id.clone()),
            Row::ClosedWorktree { record, .. } => {
                self.worktrees.get(*record).map(|r| r.workspace_id.clone())
            }
            Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => None,
        }
    }

    /// What the cursor's row is, for the one question "is it still the same row": [`App::cursor_id`] for a row
    /// that has an id, and for a row that has none (a machine's header or empty-machine row, a directory the
    /// filter offers) a word that says which one, so two rows with no id never match each other.
    pub(crate) fn row_identity(&self) -> Option<String> {
        let machine = |at: &usize| {
            self.snap
                .machines
                .get(*at)
                .map_or("", |header| header.name.as_str())
        };
        self.cursor_id().or_else(|| match self.current()? {
            Row::Machine(Some(at)) => Some(format!("header of {}", machine(at))),
            Row::Machine(None) => Some("header of this machine".to_string()),
            Row::NoSession(at) => Some(format!("empty row of {}", machine(at))),
            Row::Project(at) => Some(format!("directory {at}")),
            Row::Pane(_) | Row::Session(_) | Row::ClosedWorktree { .. } => None,
        })
    }

    /// The name the list gives the session `session` (the id the actions use), or the id when it is not listed.
    pub(crate) fn session_name_of(&self, session: &str) -> String {
        self.snap
            .spaces
            .iter()
            .find(|s| s.session_id == session)
            .map_or_else(|| session.to_string(), |s| s.session_name.clone())
    }

    /// How many sessions `workspace close --group` closes for session `session`: it and the worktree sessions
    /// cut from it that the list shows.
    pub(crate) fn group_size(&self, session: &str) -> usize {
        self.snap
            .spaces
            .iter()
            .position(|s| s.session_id == session)
            .map_or(1, |space| self.linked_sessions_of(space).len() + 1)
    }

    /// What is said of acting on `machine`'s rows when it is away or not listed, `None` when it is live.
    pub(crate) fn away_words(&self, machine: &str) -> Option<String> {
        match self
            .snap
            .machines
            .iter()
            .find(|header| header.name == machine)
        {
            Some(header) if header.live => None,
            Some(header) => Some(self.refusal(header)),
            None => Some(words::no_such_machine(machine)),
        }
    }

    /// The question a machine's `workspace_group_close_required` raises for `action` (a session close it was
    /// sent): the group question of `x` on a session, naming the machine; only `y` runs the `--group` verb.
    /// `false` when `action` is not a session close.
    pub(crate) fn ask_group_close(&mut self, machine: &str, action: &Action) -> bool {
        let Action::KillWorkspace { session } = action else {
            return false;
        };
        let Some(space) = self
            .snap
            .spaces
            .iter()
            .position(|s| &s.session_id == session)
        else {
            return false;
        };
        let name = self.snap.spaces[space].session_name.clone();
        let lead = format!("kill-session {}?", words::on_machine(machine, &name));
        let linked = self.linked_sessions_of(space);
        let question = if linked.is_empty() {
            words::group_close_unseen(&lead)
        } else {
            group_question(&lead, &linked)
        };
        self.overlay = Overlay::Confirm(Confirm::strict(
            question,
            Action::KillGroup {
                session: session.clone(),
            },
        ));
        true
    }

    /// The question a machine's `worktree_dirty` raises for the removal `action`: this Mac's own words for a
    /// worktree git will not remove, and only `y` removes it, with `--force`. `false` for any other action.
    pub(crate) fn ask_dirty(&mut self, action: &Action) -> bool {
        let Action::RemoveWorktree { workspace, .. } = action else {
            return false;
        };
        let branch = self
            .worktrees
            .iter()
            .find(|r| &r.workspace_id == workspace)
            .map_or_else(|| workspace.clone(), |r| r.branch.clone());
        self.overlay = Overlay::Confirm(Confirm::strict(
            actions::removal_question(
                crate::session::worktree::RemoveRefusal::Dirty,
                &branch,
                std::path::Path::new(""),
            ),
            Action::RemoveWorktree {
                workspace: workspace.clone(),
                force: true,
            },
        ));
        true
    }

    /// `r`: an agent row has two things to rename and asks which; everything else has one.
    fn rename(&mut self) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        match row {
            Row::Pane(_) => match self.rename_chooser(&row) {
                Some(menu) => self.overlay = Overlay::Menu(menu),
                None => self.rename_window(),
            },
            Row::Session(_) => self.rename_session(),
            Row::ClosedWorktree { .. } => self.toast("a closed worktree keeps its branch name"),
            // A directory the filter lists has no name of its own to change.
            Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => {}
        }
    }

    /// The `w` window / `a` agent chooser, for a pane an agent occupies. `None` for a
    /// plain pane, which has only its window to rename.
    fn rename_chooser(&self, row: &Row) -> Option<Menu> {
        let pane = self.row_pane(row)?;
        let agent = self.row_agent(row)?;
        let title = format!(
            "rename what? {}:{}",
            self.session_name(&pane.session_id),
            tree::where_label(pane)
        );
        Some(Menu::new(
            title,
            vec![
                Item::new(
                    'w',
                    format!("the window  (now \"{}\")", pane.window_name),
                    RowAction::RenameWindow,
                ),
                Item::new(
                    'a',
                    format!(
                        "the agent  (taw label, now \"{}\"; also the CLI handle)",
                        agent.agent_label()
                    ),
                    RowAction::NameAgent,
                ),
            ],
            (0, 0),
        ))
    }

    /// `rename-window` on the cursor row's window.
    fn rename_window(&mut self) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        let Some(t) = self.pane_target(&row) else {
            self.toast("r on a header renames the session");
            return;
        };
        self.overlay = Overlay::Prompt(Prompt::new(
            PromptKind::RenameWindow { window: t.window },
            format!("rename-window {}:{}", t.session_name, t.window_index),
            t.window_name,
        ));
    }

    /// The paste-image key: the clipboard's image into the cursor row's agent, on this machine or on the
    /// machine the row is on (the popup sends it there itself, as the app does). A row that is not an agent's
    /// says so, a machine that is away says it is away, and an agent that is waiting on a question (blocked, on
    /// this machine or another) says to answer it first: the paste would go to the question's dialog, which drops
    /// it. A working or an idle agent takes it.
    pub(super) fn paste_image(&mut self) -> Option<Action> {
        let agent = self.current().cloned().and_then(|row| {
            self.row_agent(&row)
                .map(|agent| (agent.pane_id.clone(), agent.state))
        });
        let Some((pane, state)) = agent else {
            self.toast(crate::image::NOT_AN_AGENT);
            return None;
        };
        if let Some(header) = self.machine_at(self.sel)
            && !header.live
        {
            let text = self.refusal(header);
            self.toast(text);
            return None;
        }
        if state == Some(crate::store::AgentState::Blocked) {
            self.toast(crate::image::WAITING_ON_A_QUESTION);
            return None;
        }
        Some(Action::PasteImage { pane })
    }

    /// herdr's agent label (`@agent_name`) on the cursor row's agent.
    fn name_agent(&mut self) {
        let row = self.current().cloned();
        let named = row
            .as_ref()
            .and_then(|row| self.row_agent(row))
            .map(|agent| (agent.pane_id.clone(), agent.agent_label()));
        let Some((pane, kind)) = named else {
            self.toast("only an agent row has an agent to name");
            return;
        };
        self.overlay = Overlay::Prompt(Prompt::new(
            PromptKind::RenameAgent { pane },
            format!("name agent (taw label for {kind}, not a tmux rename)"),
            String::new(),
        ));
    }

    /// `rename-session` on the cursor row's session.
    fn rename_session(&mut self) {
        let Some(space) = self
            .current()
            .cloned()
            .and_then(|row| tree::space_of(&self.snap, &row))
        else {
            return;
        };
        let Some(s) = self.snap.spaces.get(space) else {
            return;
        };
        let (session, name) = (s.session_id.clone(), s.session_name.clone());
        self.overlay = Overlay::Prompt(Prompt::new(
            PromptKind::RenameSession { session },
            format!("rename-session {name}"),
            name,
        ));
    }

    /// `w`: a branch prompt for a worktree cut from the cursor row's session.
    fn open_new_worktree(&mut self) {
        let named = self
            .current()
            .cloned()
            .and_then(|row| tree::space_of(&self.snap, &row))
            .and_then(|space| self.snap.spaces.get(space))
            .map(|s| (s.session_id.clone(), s.session_name.clone()));
        let Some((workspace, label)) = named else {
            self.toast("no session selected");
            return;
        };
        self.overlay = Overlay::Prompt(Prompt::new(
            PromptKind::NewWorktree { workspace },
            format!("new worktree in {label}: branch"),
            String::new(),
        ));
    }

    /// The header of the machine the row `index` belongs to, for any row of a machine: its sessions, panes
    /// and closed worktrees ([`Self::machine_at`]), its own header line and its empty-machine row. `None` for
    /// a row of this machine's.
    fn machine_of_row(&self, index: usize) -> Option<&MachineHeader> {
        match self.rows.rows.get(index)? {
            Row::Machine(Some(at)) | Row::NoSession(at) => self.snap.machines.get(*at),
            _ => self.machine_at(index),
        }
    }

    /// `n`: on a row of another machine, a session is made on that machine: the app opens that machine's picker,
    /// filled from its taw ([`Self::open_far_picker`]; an older taw there gets the prompt for a folder or a zoxide
    /// term instead, [`Self::far_answered`]), and the popup, which has no connection
    /// to a machine, says the row is read only, as it does for `r`, `x` and `w`. A machine that is not
    /// connected says why, in the app too. Everywhere else it is this machine's picker ([`Self::open_project_picker`]).
    pub(super) fn new_session(&mut self) {
        let Some(header) = self.machine_of_row(self.sel) else {
            self.open_project_picker();
            return;
        };
        if !header.live || !self.is_app() {
            let said = self.refusal(header);
            self.toast(said);
            return;
        }
        let machine = header.name.clone();
        self.open_far_picker(&machine);
    }

    /// `n` on a row of `machine`, another machine, in the app: that machine's picker, at once, with the folders
    /// it gave last in this run (none the first time) while the screen asks it again ([`Self::take_far_ask`]).
    fn open_far_picker(&mut self, machine: &str) {
        // A taw that answered "older" this run goes straight to step 1's prompt; the ask still goes, so a taw
        // updated meanwhile gets the picker at the next `n`.
        if self.far_old.contains(machine) {
            self.overlay = Overlay::Prompt(Prompt::new(
                PromptKind::NewSessionOn {
                    machine: machine.to_string(),
                },
                words::new_session_prompt(machine),
                String::new(),
            ));
            self.far_ask = Some(machine.to_string());
            return;
        }
        let kept = self.far_folders.get(machine).cloned().unwrap_or_default();
        self.overlay = Overlay::Project(Box::new(ProjectPicker::far(
            self.cfg, &self.home, machine, kept,
        )));
        self.far_ask = Some(machine.to_string());
    }

    /// The machine whose folders `n` asked for, taken: the screen sends the ask over that machine's worker.
    pub(crate) fn take_far_ask(&mut self) -> Option<String> {
        self.far_ask.take()
    }

    /// What `machine` answered when its picker asked for its folders. A list is kept for the next `n` there and
    /// fills its picker when that is open; an older taw (no `session candidates`) turns the open picker into
    /// the prompt for a folder or a zoxide term, with what was typed; any other failure is shown in the
    /// picker in the far error's own words. A picker that is closed, or another machine's, is left alone.
    pub(crate) fn far_answered(&mut self, machine: &str, answer: FarAnswer) {
        match &answer {
            FarAnswer::Folders(folders) => {
                self.far_old.remove(machine);
                self.far_folders
                    .insert(machine.to_string(), folders.clone());
            }
            FarAnswer::OldTaw => {
                self.far_old.insert(machine.to_string());
            }
            FarAnswer::Said(_) => {}
        }
        let Overlay::Project(picker) = &mut self.overlay else {
            return;
        };
        if !matches!(&picker.far, Some(far) if far.machine == machine) {
            return;
        }
        match answer {
            FarAnswer::Folders(folders) => picker.set_far_answer(Ok(folders)),
            FarAnswer::Said(said) => picker.set_far_answer(Err(said)),
            FarAnswer::OldTaw => {
                let typed = picker.input.value().to_string();
                self.overlay = Overlay::Prompt(Prompt::new(
                    PromptKind::NewSessionOn {
                        machine: machine.to_string(),
                    },
                    words::new_session_prompt(machine),
                    typed,
                ));
            }
        }
    }

    /// `n`: the project picker, already filled in for what is typed (`~/`).
    pub(super) fn open_project_picker(&mut self) {
        let mut picker = ProjectPicker::new(self.cfg, &self.home);
        picker.refresh(&self.snap, self.cfg, &self.home, &self.zoxide);
        self.overlay = Overlay::Project(Box::new(picker));
    }

    /// Recomputes the project picker's candidates after a key or a click it consumed. The
    /// refresh needs the snapshot, the config, `$HOME` and zoxide's history, which only the
    /// app holds, and it reads the filesystem: path completion, the project directories'
    /// children, and the canonical path of each session's directory and of each candidate.
    /// The filter's directories read it the same ways ([`App::refresh_projects`] and
    /// [`App::retag_projects`]).
    pub(crate) fn refresh_project_picker(&mut self) {
        if !matches!(self.overlay, Overlay::Project(_)) {
            return;
        }
        // Lifted out and put back: `refresh` needs the snapshot and the config while the
        // picker itself lives in `self.overlay`.
        let mut overlay = std::mem::take(&mut self.overlay);
        if let Overlay::Project(picker) = &mut overlay {
            picker.refresh(&self.snap, self.cfg, &self.home, &self.zoxide);
        }
        self.overlay = overlay;
    }

    /// Right-click: a menu of the row's own actions, labelled with their keys — go,
    /// rename, close, new worktree, and on an agent row paste image (the search preset's only way to it, since
    /// it binds fzf's own keys and nothing else) — and nothing tmux already has a key for (splits,
    /// windows). Not options, which is `o`. The same entries in both presets: the search
    /// preset has no `ctrl-o` menu. A parked worktree
    /// has no name of its own to change, so its menu has no rename.
    pub(super) fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        // A directory the filter lists has no menu: nothing to rename, close or cut a
        // worktree from, and a right-click on it does nothing.
        if matches!(row, Row::Project(_) | Row::Machine(_) | Row::NoSession(_)) {
            return;
        }
        // A row of another machine has no menu while every item of it is refused: the click says
        // why, as Enter on it does. The menu comes back with the actions a machine can take.
        if let Some(text) = self.refusal_at(self.sel) {
            self.toast(text);
            return;
        }
        let pane = self.pane_target(&row);
        let session = tree::space_of(&self.snap, &row)
            .and_then(|space| self.snap.spaces.get(space))
            .map(|s| s.session_name.clone())
            .unwrap_or_default();
        let worktree = matches!(row, Row::ClosedWorktree { .. });

        let mut items = vec![Item::plain("go", RowAction::Go)];
        if !worktree {
            items.push(Item::new(
                'r',
                match &row {
                    Row::Pane(_) if self.row_agent(&row).is_some() => "rename (window or agent)",
                    Row::Pane(_) => "rename the window",
                    _ => "rename the session",
                },
                RowAction::Rename,
            ));
        }
        items.push(Item::new(
            'x',
            match &row {
                Row::Session(_) => "close the session (kill-session)".to_string(),
                Row::ClosedWorktree { .. } => "remove the worktree".to_string(),
                Row::Pane(_) => "close this pane (kill-pane)".to_string(),
                Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => String::new(),
            },
            RowAction::Close,
        ));
        items.push(Item::new('w', "new worktree…", RowAction::NewWorktree));
        if self.row_agent(&row).is_some() {
            items.push(Item::new('v', PASTE_IMAGE_ENTRY, RowAction::PasteImage));
        }

        let title = match (&row, &pane) {
            (Row::Pane(_), Some(t)) if !t.what.is_empty() => {
                format!("{}:{} {}", t.session_name, t.where_label, t.what)
            }
            (Row::Pane(_), Some(t)) => format!("{}:{}", t.session_name, t.where_label),
            (Row::ClosedWorktree { record, .. }, _) => self
                .worktrees
                .get(*record)
                .map(|r| r.branch.clone())
                .unwrap_or_default(),
            _ => session,
        };
        self.overlay = Overlay::Menu(Menu::new(title, items, (x, y)));
    }

    // ----------------------------------------------------------------- prompts

    /// Validates a submitted prompt: either an action (and the prompt closes), or an error
    /// written back into the prompt (which stays open).
    pub(super) fn submit(&mut self, text: String) -> Option<Action> {
        let Overlay::Prompt(prompt) = &self.overlay else {
            return None;
        };
        let result = match prompt.kind.clone() {
            // An empty line does nothing: the prompt stays as it is.
            PromptKind::NewSessionOn { .. } if text.trim().is_empty() => return None,
            PromptKind::NewSessionOn { machine } => Ok(Action::NewRemoteSession {
                machine,
                term: text.trim().to_string(),
                exact: false,
            }),
            PromptKind::NewWorktree { workspace } => {
                if text.is_empty() {
                    Err("enter a branch".to_string())
                } else {
                    Ok(Action::NewWorktree {
                        workspace,
                        branch: text,
                    })
                }
            }
            PromptKind::RenameAgent { pane } => actions::validate_agent_name(&text)
                .and_then(|()| self.check_unique(&pane, &text))
                .map(|()| Action::RenameAgent {
                    pane,
                    name: text.clone(),
                }),
            // A window name is tmux's, so it is the session rule minus the uniqueness:
            // two windows of one session may share a name.
            PromptKind::RenameWindow { window } => {
                naming::validate_typed(&text, &[]).map(|()| Action::RenameWindow {
                    window,
                    name: text.clone(),
                })
            }
            PromptKind::RenameSession { session } => {
                // The sessions of the machine the renamed one is on (this Mac when it is a local row): a name
                // only another machine has is free here.
                let on = self
                    .snap
                    .spaces
                    .iter()
                    .find(|s| s.session_id == session)
                    .and_then(|s| s.machine);
                let live: Vec<String> = self
                    .snap
                    .spaces
                    .iter()
                    .filter(|s| s.machine == on && s.session_id != session)
                    .map(|s| s.session_name.clone())
                    .collect();
                naming::validate_typed(&text, &live).map(|()| Action::RenameWorkspace {
                    session,
                    name: text.clone(),
                })
            }
        };
        match result {
            Ok(action) => {
                self.overlay = Overlay::None;
                Some(action)
            }
            Err(message) => {
                if let Overlay::Prompt(prompt) = &mut self.overlay {
                    prompt.error = Some(message);
                }
                None
            }
        }
    }

    /// herdr's agent names are unique among live agents, by the rule `agent rename` and `agent start`
    /// keep ([`crate::tmux::holds_agent_name`]): a name that only an ended agent still records is free.
    pub(super) fn check_unique(&self, pane: &str, name: &str) -> Result<(), String> {
        self.check_unique_by(pane, name, |a| crate::watch::shell_known_idle(a.pane_pid))
    }

    /// [`Self::check_unique`] with the idle look at a row's own shell given. It is asked only of the
    /// rows that carry `name` (other than `pane`), and no more once one holds it, so a free name
    /// never reads the process table.
    pub(super) fn check_unique_by(
        &self,
        pane: &str,
        name: &str,
        shell_idle: impl Fn(&AgentRow) -> bool,
    ) -> Result<(), String> {
        // The agents of the machine the renamed one is on (this Mac for a local row): another machine's name is
        // its own. A machine's agent has no pid here for the idle look, so its name is held while it is listed.
        let on = self
            .snap
            .agents
            .iter()
            .find(|a| a.pane_id == pane)
            .and_then(|a| a.machine);
        let taken = self
            .snap
            .agents
            .iter()
            .filter(|a| a.machine == on && a.pane_id != pane && a.name.as_deref() == Some(name))
            .any(|a| on.is_some() || a.holds_its_agent_name(&shell_idle));
        if taken {
            Err(format!("another agent is already named '{name}'"))
        } else {
            Ok(())
        }
    }

    // ----------------------------------------------------------------- naming

    /// Everything a pane row's commands need, with no borrow of the snapshot left over.
    /// `None` for a header or a parked worktree, which have no pane.
    fn pane_target(&self, row: &Row) -> Option<PaneTarget> {
        let pane = self.row_pane(row)?;
        Some(PaneTarget {
            pane: pane.pane_id.clone(),
            session: pane.session_id.clone(),
            window: pane.window_id.clone(),
            window_index: pane.window_index,
            window_name: pane.window_name.clone(),
            window_panes: pane.window_panes,
            pane_index: pane.pane_index,
            session_name: self.session_name(&pane.session_id),
            where_label: tree::where_label(pane),
            what: tree::what_label(&self.snap, pane),
        })
    }

    /// tmux's own target spelling for a pane: `<session>:<window>`, plus `.<pane>` when
    /// the window has more than one and the index is the only way to tell them apart.
    fn target_text(&self, t: &PaneTarget) -> String {
        if t.window_panes > 1 {
            format!("{}:{}.{}", t.session_name, t.window_index, t.pane_index)
        } else {
            format!("{}:{}", t.session_name, t.window_index)
        }
    }

    /// How a session is named in a message: its session name, falling back to the raw
    /// session id when no row matches (the snapshot moved under us).
    fn session_name(&self, session_id: &str) -> String {
        self.snap
            .spaces
            .iter()
            .find(|s| s.session_id == session_id)
            .map(|s| s.session_name.clone())
            .unwrap_or_else(|| session_id.to_string())
    }
}
