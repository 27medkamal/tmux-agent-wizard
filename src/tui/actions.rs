// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! The popup's side effects: every tmux command the TUI is allowed to run.
//!
//! The sidebar itself is read-only: this is the short, explicit
//! list of things a key or a menu entry may change, and the only place in the TUI that
//! runs a mutating tmux command. `[App](super::app::App)` never executes anything: it
//! returns an [`Action`], and the run loop hands it here.

use std::path::Path;

use crate::config::Config;
use crate::hooks::{self, Closing};
use crate::notify;
use crate::paths::Paths;
use crate::plugin::PluginCtx;
use crate::session::{naming, worktree};
use crate::tmux::Tmux;
use crate::tui::overlays::Confirm;

/// What the popup, the app and the plugin's key say when no pane holds an agent, so there is nowhere
/// to go (`prefix+O`). The bats and acceptance tests read the same words off a real screen.
pub const NO_AGENTS: &str = "taw: no agents";

/// Something the user asked for that changes tmux (or leaves the popup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Focus a pane: switch the popup's client to `session`, then select the window and
    /// pane. Leaves the popup.
    Jump {
        session: String,
        window: String,
        pane: String,
    },
    /// Switch the popup's client to a workspace, leaving its own active window/pane
    /// alone. Leaves the popup.
    SwitchWorkspace {
        session: String,
    },
    /// `@agent_name` on a pane (herdr's agent rename).
    RenameAgent {
        pane: String,
        name: String,
    },
    RenameWorkspace {
        session: String,
        name: String,
    },
    /// `rename-window -t <window> <name>` on one window of a session.
    RenameWindow {
        window: String,
        name: String,
    },
    KillPane {
        pane: String,
    },
    KillWorkspace {
        session: String,
    },
    /// Open the workspace for a project directory, in tilde form: switch to the session
    /// already running there, or create one. Leaves the popup.
    OpenProject {
        path: String,
    },
    /// `prefix+O` from inside the popup: next up, the next agent that needs you, else the
    /// next agent of any kind, else a message.
    /// Leaves the popup.
    ///
    /// `own` is the pane the popup was opened from — the pane the walk starts *after*.
    /// `App` fills it in from the snapshot when the chord fires, because the popup itself
    /// cannot read it: `display-popup -E` leaves `$TMUX_PANE` unset, so `taw ui open`
    /// resolves the pane beforehand and passes it in (`taw ui --from`).
    NextUp {
        own: Option<String>,
    },
    /// Worktree verbs, delegated to `session::worktree`.
    NewWorktree {
        workspace: String,
        branch: String,
    },
    OpenWorktree {
        workspace: String,
    },
    /// Enter on a live machine's row in the popup: the popup ends and the client goes into the app, which goes
    /// into the machine (`taw app --go machine:id`). `id` is the machine's own (`%16`, `$3` or `@5`), not
    /// namespaced. Leaves the popup. Not a landing: nothing here goes anywhere itself.
    Doorway {
        machine: String,
        id: String,
    },
    RemoveWorktree {
        workspace: String,
        /// `git worktree remove --force`: what the popup runs once its user has said a
        /// removal git refused may go ahead.
        force: bool,
    },
    /// `kill-session` on a session and on the worktree sessions cut from it: the answer to
    /// "close them too?".
    KillGroup {
        session: String,
    },
    /// The clipboard's image into the agent of `pane`, the list's id (`%16`, or `studio:%16` on a machine).
    PasteImage {
        pane: String,
    },
}

/// What running an [`Action`] means for the popup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Close the popup: the user is now looking at the pane they jumped to.
    pub exit: bool,
    /// A line to show in the popup's own toast area (an error, or a confirmation of
    /// something that left no visible trace).
    pub toast: Option<String>,
    /// A question the action raised and the popup has to put to its user before anything
    /// more is done (a removal git refused, which a forced one would go through with).
    pub ask: Option<Confirm>,
}

impl Outcome {
    fn quiet() -> Outcome {
        Outcome::default()
    }

    fn exit() -> Outcome {
        Outcome {
            exit: true,
            ..Outcome::default()
        }
    }

    fn toast(message: impl Into<String>) -> Outcome {
        Outcome {
            toast: Some(message.into()),
            ..Outcome::default()
        }
    }
}

/// Runs one action.
///
/// Errors are turned into a toast rather than propagated: a failed `kill-pane` (the pane
/// died a moment ago, say) must not take the popup down with it. Only a failure that
/// makes the popup meaningless returns `Err`.
///
/// `client` is the client the popup was opened on, named by its launcher: every action that
/// leaves the popup by moving a client moves that one ([`switch_client`]).
pub fn exec(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    action: &Action,
    client: Option<&str>,
) -> anyhow::Result<Outcome> {
    Ok(match action {
        Action::Jump {
            session,
            window,
            pane,
        } => return jump(t, client, session, window, pane),
        Action::SwitchWorkspace { session } => {
            switch_outcome(switch_client(t, client, session), None)
        }
        Action::PasteImage { pane } => Outcome::toast(crate::image::paste_for_list(t, paths, pane)),
        Action::RenameAgent { pane, name } => {
            match crate::store::set_text_option(t, pane, "@agent_name", name) {
                Ok(()) => Outcome::toast(agent_renamed_toast(name)),
                Err(e) => Outcome::toast(short(&e)),
            }
        }
        // A `--` ends tmux's flags: a name that starts with a dash is a name, not an unknown flag.
        Action::RenameWorkspace { session, name } => run(
            t,
            &[
                "rename-session",
                "-t",
                session,
                "--",
                &crate::tmux::new_name(name),
            ],
        ),
        Action::RenameWindow { window, name } => run(
            t,
            &[
                "rename-window",
                "-t",
                window,
                "--",
                &crate::tmux::new_name(name),
            ],
        ),
        // The walk starts after the pane the popup was opened from, the row the cursor
        // opened on. The action carries that pane, so nothing here reads the environment —
        // see the variant's own doc comment.
        Action::NextUp { own } => return Ok(next_up(t, client, own.as_deref())),
        Action::KillPane { pane } => match may_close(t, cfg, paths, taw_bin, Closing::Pane(pane)) {
            Some(refused) => refused,
            None => {
                // The last pane of a session's last window takes the session with it.
                if let Some(session) = sole_pane_session(t, pane)
                    && let Err(e) = leave_before_closing(t, client, &[session.as_str()])
                {
                    return Ok(Outcome::toast(short(&e)));
                }
                run(t, &["kill-pane", "-t", pane])
            }
        },
        Action::KillWorkspace { session } => {
            match may_close(t, cfg, paths, taw_bin, Closing::Session(session)) {
                Some(refused) => refused,
                None => {
                    if let Err(e) = leave_before_closing(t, client, &[session.as_str()]) {
                        return Ok(Outcome::toast(short(&e)));
                    }
                    run(t, &["kill-session", "-t", session])
                }
            }
        }
        Action::KillGroup { session } => kill_group(t, cfg, paths, taw_bin, session, client),
        Action::Doorway { machine, id } => doorway(t, client, taw_bin, machine, id),
        Action::OpenProject { path } => return Ok(open_project(t, cfg, client, path)),
        Action::NewWorktree { workspace, branch } => {
            match worktree::create(
                t, cfg, paths, taw_bin, workspace, branch, None, false, false,
            ) {
                Ok(record) => {
                    return Ok(enter_worktree(
                        t,
                        client,
                        &record.session_name,
                        created_note(&record.branch),
                    ));
                }
                Err(e) => Outcome::toast(short(&e)),
            }
        }
        Action::OpenWorktree { workspace } => {
            match worktree::open(t, cfg, paths, taw_bin, workspace, false, false) {
                Ok((record, already_open)) => {
                    return Ok(enter_worktree(
                        t,
                        client,
                        &record.session_name,
                        opened_note(already_open, &record.branch),
                    ));
                }
                Err(e) => Outcome::toast(short(&e)),
            }
        }
        // A dirty worktree, or one with submodules, is git's call to refuse without `force`:
        // the popup then asks whether to remove it anyway, and `y` runs this again with
        // `force`. Any other failure is the toast's line. The branch is never deleted.
        // `[hooks] pre_worktree_remove` is asked inside `remove` every time, forced or not,
        // and its refusal is the toast's line, like a create hook's.
        Action::RemoveWorktree { workspace, force } => {
            return Ok(remove_worktree(t, cfg, paths, taw_bin, workspace, *force));
        }
    })
}

/// Where a landing takes the app: the session to attach, and the pane the screen will open on when the
/// person comes back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    /// An `attach-session -t` target: a session id, or `=name`.
    pub target: String,
    /// The pane you went into, when tmux names one.
    pub pane: Option<String>,
    /// What the popup would say about the landing (a name that had to change, `created worktree X`), which
    /// the app says when it is back from the attach.
    pub note: Option<String>,
}

/// What a landing does in the app, which has no client to switch: it makes the target (opens the
/// session of a project or a worktree, detached, as the popup does before it switches) and says where to
/// attach. Whatever the popup would select on the way (the window and the pane of a [`Action::Jump`]) is
/// selected here, so the attach lands on it. `Err` is the toast's words, and nothing is attached.
/// An action that is no landing has nowhere to go.
pub fn land(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    action: &Action,
) -> Result<Landing, String> {
    match action {
        Action::Jump {
            session,
            window,
            pane,
        } => {
            // The window is named with its session: a bare window id with no client may be read in another
            // session of the group than the one that is attached next.
            let _ = t.run_ok(&["select-window", "-t", &format!("{session}:{window}")]);
            let _ = t.run_ok(&["select-pane", "-t", pane]);
            Ok(Landing {
                target: session.clone(),
                pane: Some(pane.clone()),
                note: None,
            })
        }
        Action::SwitchWorkspace { session } => Ok(Landing {
            target: session.clone(),
            pane: active_pane(t, session),
            note: None,
        }),
        Action::OpenProject { path } => {
            let home = dirs::home_dir().unwrap_or_default();
            let made = crate::session::create::open_or_create(t, cfg, path, &home)
                .map_err(|e| failure_toast(&e))?;
            Ok(named(t, &made.session_name, collision_note(&made)))
        }
        Action::NewWorktree { workspace, branch } => {
            let record = worktree::create(
                t, cfg, paths, taw_bin, workspace, branch, None, false, false,
            )
            .map_err(|e| short(&e))?;
            Ok(named(
                t,
                &record.session_name,
                Some(created_note(&record.branch)),
            ))
        }
        Action::OpenWorktree { workspace } => {
            let (record, already_open) =
                worktree::open(t, cfg, paths, taw_bin, workspace, false, false)
                    .map_err(|e| short(&e))?;
            Ok(named(
                t,
                &record.session_name,
                Some(opened_note(already_open, &record.branch)),
            ))
        }
        Action::NextUp { own } => match notify::open_target(t, false, own.as_deref()) {
            Ok(Some((pane, _group))) => {
                let session = t
                    .display(&pane, "#{session_id}")
                    .map(|id| id.trim().to_string())
                    .map_err(|e| short(&e))?;
                Ok(Landing {
                    target: session,
                    pane: Some(pane),
                    note: None,
                })
            }
            Ok(None) => Err(NO_AGENTS.to_string()),
            Err(e) => Err(short(&e)),
        },
        _ => Err(crate::machine::words::NOTHING_TO_GO_IN_TO.to_string()),
    }
}

/// A landing on the session called `name`, with the popup's `note` for it.
fn named(t: &Tmux, name: &str, note: Option<String>) -> Landing {
    let target = crate::tmux::session_target(name);
    let pane = active_pane(t, &target);
    Landing { target, pane, note }
}

/// The note for a worktree made: the popup's words, and the app's when it is back from the attach.
fn created_note(branch: &str) -> String {
    format!("created worktree {branch}")
}

/// The note for a worktree opened, or switched to when its session was already running.
pub(crate) fn opened_note(already_open: bool, branch: &str) -> String {
    let what = if already_open {
        "switched to"
    } else {
        "opened"
    };
    format!("{what} worktree {branch}")
}

/// The note for a project whose proposed session name had to change to dodge another session.
fn collision_note(made: &crate::session::create::Outcome) -> Option<String> {
    made.collision
        .as_ref()
        .map(|why| format!("session {} ({why})", made.session_name))
}

/// The active pane of `session` (any session target), when tmux says.
fn active_pane(t: &Tmux, session: &str) -> Option<String> {
    t.display(session, "#{pane_id}")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| id.starts_with('%'))
}

/// A landing on the session and the window of `pane`, with the pane selected: where the app goes back
/// in, to the pane a person came out of. `None` when tmux no longer has the pane.
pub fn land_on_pane(t: &Tmux, pane: &str) -> Option<Landing> {
    let said = t.display(pane, "#{session_id} #{window_id}").ok()?;
    let mut words = said.split_whitespace();
    let (session, window) = (words.next()?, words.next()?);
    // With the session, as in a jump (see [`land`]).
    let _ = t.run_ok(&["select-window", "-t", &format!("{session}:{window}")]);
    let _ = t.run_ok(&["select-pane", "-t", pane]);
    Some(Landing {
        target: session.to_string(),
        pane: Some(pane.to_string()),
        note: None,
    })
}

/// `git worktree remove` for the popup's `x`, `--force` when its user has said so. A removal
/// git refused, where a forced one would go through, comes back as the question to put to the
/// user (`y` runs it forced) and nothing else; every other failure is a toast.
fn remove_worktree(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    workspace: &str,
    force: bool,
) -> Outcome {
    match worktree::remove(t, cfg, paths, taw_bin, workspace, force, false) {
        Ok(record) => Outcome::toast(format!("removed worktree {}", record.branch)),
        Err(e) => {
            let refused =
                e.chain().find_map(
                    |cause| match cause.downcast_ref::<worktree::WorktreeError>() {
                        Some(worktree::WorktreeError::RemoveRefused {
                            why, branch, path, ..
                        }) => Some((*why, branch.clone(), path.clone())),
                        _ => None,
                    },
                );
            match refused {
                Some((why, branch, folder)) if !force => Outcome {
                    ask: Some(Confirm::strict(
                        removal_question(why, &branch, &folder),
                        Action::RemoveWorktree {
                            workspace: workspace.to_string(),
                            force: true,
                        },
                    )),
                    ..Outcome::default()
                },
                _ => Outcome::toast(failure_toast(&e)),
            }
        }
    }
}

/// What the popup asks once git has refused to remove `branch`'s worktree: why, and whether to
/// go on anyway. What goes is whatever is in the folder, so the answer it wants is a `y`. A folder
/// git has let go of is one this crate deletes itself, so that question names it.
pub(crate) fn removal_question(
    why: worktree::RemoveRefusal,
    branch: &str,
    folder: &Path,
) -> String {
    let reason = match why {
        worktree::RemoveRefusal::Dirty => {
            format!("git will not remove {branch}: it has modified or untracked files.")
        }
        worktree::RemoveRefusal::Submodules => {
            format!("git will not remove {branch}: it has submodules.")
        }
        worktree::RemoveRefusal::Leftover => {
            let folder = match dirs::home_dir() {
                Some(home) => naming::tilde(folder, &home),
                None => folder.display().to_string(),
            };
            format!(
                "git no longer lists {branch}, but part of its folder is still there: {folder}."
            )
        }
    };
    format!("{reason} remove anyway? uncommitted changes are lost")
}

/// `kill-session` on a session and on the worktree sessions cut from it, the answer `y` gives to
/// the popup's "close them too?". The group is worked out again here, from what is running now
/// (`session::worktree::linked_sessions`, the set `taw workspace close --group` closes), rather
/// than taken from what the question showed.
///
/// `[hooks] pre_close` is asked about every session that would close before any of them does,
/// and one refusal keeps them all open, so a hook is never left having agreed to half of it.
/// The worktree sessions close first and the session they were cut from last, except that the
/// session `client` (the popup's own) is attached to goes after all of them: with tmux's
/// default `detach-on-destroy` closing it takes the client with it, and the popup with the
/// client, so nothing the popup meant to close afterwards would be closed. The whole close is
/// one tmux command for the same reason: the server runs a command it has been given to the
/// end, whatever becomes of the client that sent it.
fn kill_group(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    session: &str,
    client: Option<&str>,
) -> Outcome {
    let source = match t.list_sessions() {
        Ok(sessions) => sessions.into_iter().find(|s| s.session_id == session),
        Err(e) => return Outcome::toast(short(&e)),
    };
    let Some(source) = source else {
        // Gone already, which is what closing it would have made it.
        return Outcome::toast("that session is already closed");
    };
    let linked = worktree::linked_sessions(t, paths, &source).unwrap_or_else(|err| {
        // The worktree records only say which sessions go with it: a file taw cannot read
        // must not stop a session closing, which needs nothing from it.
        log::warn!("closing a group: could not look up worktree sessions: {err:#}");
        Vec::new()
    });
    let mut order: Vec<&str> = linked.iter().map(|l| l.session_id.as_str()).collect();
    order.push(&source.session_id);
    for id in &order {
        if let Some(refused) = may_close(t, cfg, paths, taw_bin, Closing::Session(id)) {
            return refused;
        }
    }
    if let Err(e) = leave_before_closing(t, client, &order) {
        return Outcome::toast(short(&e));
    }
    let own = clients_session(t, client);
    let order = client_session_last(order, own.as_deref());
    if let Err(e) = t.run_ok(&kill_sessions_args(&order)) {
        return Outcome::toast(short(&e));
    }
    Outcome::toast(group_closed_toast(&source.session_name, linked.len()))
}

/// Where a close that takes the client's own session leaves the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Leave {
    /// The close does not take the client's session, or there is no session to ask about.
    Stay,
    /// Another session survives: the client goes to it (a session id) before the close.
    GoTo(String),
    /// Nothing survives: tmux ends with the close.
    NoneLeft,
}

/// Where the client goes when the sessions `closing` close. Only a close that takes the client's own
/// session `own` moves it: to the client's last session `last` if that one survives, else to the first
/// survivor of `order` (every session, in tmux's order), and nowhere when none survives.
pub(crate) fn plan_leave(closing: &[&str], own: &str, last: Option<&str>, order: &[&str]) -> Leave {
    if !closing.contains(&own) {
        return Leave::Stay;
    }
    let mut survivors = order.iter().filter(|id| !closing.contains(id));
    match last.filter(|id| order.contains(id) && !closing.contains(id)) {
        Some(last) => Leave::GoTo(last.to_string()),
        None => survivors
            .next()
            .map_or(Leave::NoneLeft, |id| Leave::GoTo(id.to_string())),
    }
}

/// Before a close of the sessions `closing`: when it takes the session the popup's client is on and
/// another session survives, the client is moved to the one [`plan_leave`] picks, so tmux's
/// `detach-on-destroy` does not drop it out of tmux (and the popup with it). With no client to ask, or
/// none attached, nothing moves. The client is the popup's own, or the one tmux finds when the popup
/// was not given one (a `taw ui` typed in a pane).
fn leave_before_closing(t: &Tmux, client: Option<&str>, closing: &[&str]) -> anyhow::Result<()> {
    let client = match client {
        Some(client) => client.to_string(),
        None => match crate::cli::common::resolve_client(t) {
            Ok(client) => client,
            Err(_) => return Ok(()),
        },
    };
    let Some(own) = clients_session(t, Some(&client)) else {
        return Ok(());
    };
    if !closing.contains(&own.as_str()) {
        return Ok(());
    }
    let sessions = t.list_sessions()?;
    let order: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
    let said = t
        .run(&[
            "display-message",
            "-p",
            "-c",
            &client,
            "#{client_last_session}",
        ])
        .unwrap_or_default();
    let said = said.trim();
    let last = sessions
        .iter()
        .find(|s| !said.is_empty() && s.session_name == said)
        .map(|s| s.session_id.as_str());
    match plan_leave(closing, &own, last, &order) {
        Leave::GoTo(target) => switch_client(t, Some(&client), &target),
        Leave::Stay | Leave::NoneLeft => Ok(()),
    }
}

/// The session `pane` is the only pane of, when it is also that session's only window and the session is
/// in no group: closing the pane then closes the session.
fn sole_pane_session(t: &Tmux, pane: &str) -> Option<String> {
    let said = t
        .run(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{session_id} #{session_windows} #{window_panes} #{session_group_size}",
        ])
        .ok()?;
    sole_pane_of(&said)
}

/// The session in what [`sole_pane_session`] asked tmux (`#{session_id} #{session_windows}
/// #{window_panes} #{session_group_size}`), when it has one window of one pane and is in no group.
/// tmux 3.6b and 3.8 print nothing for the group size of a session in no group, so the line ends
/// in a space and an empty size counts as 0.
fn sole_pane_of(said: &str) -> Option<String> {
    let mut fields = said.trim_end_matches(['\r', '\n']).split(' ');
    let (session, windows, panes, group) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    let group = group
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .ok()?;
    (windows == "1" && panes == "1" && group <= 1).then(|| session.to_string())
}

/// The id of the session `client` is attached to, when the popup knows its client and tmux
/// still lists it.
fn clients_session(t: &Tmux, client: Option<&str>) -> Option<String> {
    let client = client?;
    t.list_clients()
        .ok()?
        .into_iter()
        .find(|c| c.client_name == client)
        .map(|c| c.session_id)
}

/// `order` with the session `own` last and every other entry where it was. An `own` that is not
/// in `order` (or none) leaves it as it is.
fn client_session_last<'a>(mut order: Vec<&'a str>, own: Option<&str>) -> Vec<&'a str> {
    if let Some(own) = own {
        // `sort_by_key` is stable, and `false` sorts before `true`.
        order.sort_by_key(|id| *id == own);
    }
    order
}

/// The arguments of one tmux command line that kills each of `sessions` in turn: a separate
/// `;` argument is how tmux is told where one command ends.
fn kill_sessions_args<'a>(sessions: &[&'a str]) -> Vec<&'a str> {
    let mut args = Vec::new();
    for (index, id) in sessions.iter().enumerate() {
        if index > 0 {
            args.push(";");
        }
        args.extend(["kill-session", "-t", id]);
    }
    args
}

/// What is said once an agent is renamed (the popup's rename, and the app's on a machine's agent).
pub(crate) fn agent_renamed_toast(name: &str) -> String {
    format!("renamed agent to {name}")
}

/// What the popup says once a group is closed: the session, and how many worktree sessions
/// went with it.
pub(crate) fn group_closed_toast(name: &str, worktree_sessions: usize) -> String {
    let plural = if worktree_sessions == 1 { "" } else { "s" };
    format!("closed {name} and its {worktree_sessions} worktree session{plural}")
}

/// Asks `[hooks] pre_close` whether `closing` may go ahead: `None` when it may, else the
/// popup's answer, which is the toast that says why not (the hook's own last line, behind its
/// name) and leaves the popup open. Nothing has been closed, and tmux has not been asked.
fn may_close(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    closing: Closing<'_>,
) -> Option<Outcome> {
    let ctx = PluginCtx {
        tmux: t,
        cfg,
        paths,
        taw_bin,
        server_env: None,
    };
    hooks::pre_close(&ctx, closing)
        .err()
        .map(|e| Outcome::toast(failure_toast(&e)))
}

/// Runs one tmux command for its side effect, reporting failure as a toast.
fn run(t: &Tmux, args: &[&str]) -> Outcome {
    match t.run_ok(args) {
        Ok(()) => Outcome::quiet(),
        Err(e) => Outcome::toast(short(&e)),
    }
}

/// tmux's error text is `\`tmux <args>\` failed (exit status: 1): <reason>`; the popup's
/// toast line only has room for the reason.
fn short(e: &anyhow::Error) -> String {
    let text = e.to_string();
    let reason = match text.rsplit_once("): ") {
        Some((_, reason)) if !reason.is_empty() => reason,
        _ => &text,
    };
    // git refuses a dirty worktree over several lines; a toast has room for the first.
    reason.lines().next().unwrap_or_default().trim().to_string()
}

/// Turns a `switch_client` result into the popup's outcome: exit on success, else a toast.
///
/// The toast carries only tmux's reason, via [`short`] — never the `tmux` binary path, the
/// client tty, or the `switch-client -c <tty> -t <session>` argv that its raw error spells
/// out (a failed jump to a dead session would read `` `…/tmux switch-client -c
/// /dev/ttys028 -t $7` failed …: can't find session: $7 ``). `prefix` is the
/// "created/opened worktree X" note the worktree paths keep in front of the reason.
fn switch_outcome(switched: anyhow::Result<()>, prefix: Option<&str>) -> Outcome {
    match switched {
        Ok(()) => Outcome::exit(),
        Err(e) => {
            let reason = short(&e);
            match prefix {
                Some(p) => Outcome::toast(format!("{p} ({reason})")),
                None => Outcome::toast(reason),
            }
        }
    }
}

/// Focuses a pane, then asks the run loop to close the popup.
fn jump(
    t: &Tmux,
    client: Option<&str>,
    session: &str,
    window: &str,
    pane: &str,
) -> anyhow::Result<Outcome> {
    // Selecting the window and pane is worth doing even with no client to switch: the
    // target is then already focused the next time someone attaches.
    let switched = switch_client(t, client, session);
    let _ = t.run_ok(&["select-window", "-t", window]);
    let _ = t.run_ok(&["select-pane", "-t", pane]);
    Ok(switch_outcome(switched, None))
}

/// Focuses a worktree's session and closes the popup.
///
/// `worktree::create`/`open` take a `focus` flag of their own, but it runs
/// `switch-client` with no `-c`. Inside a popup there is no current client for tmux to
/// resolve, so that flag cannot move the terminal the user is looking at (and on a
/// multi-client server it could move one they are not). Both are therefore called with
/// `focus: false`, and the switch happens here, through the same client resolution
/// [`Action::Jump`] uses. When there is no client to switch, the popup stays open and
/// says what it did — the worktree and its session exist either way.
fn enter_worktree(t: &Tmux, client: Option<&str>, session: &str, done: String) -> Outcome {
    switch_outcome(
        switch_client(t, client, &crate::tmux::session_target(session)),
        Some(&done),
    )
}

/// The project picker's Enter: switch to the session already running in that directory,
/// or create one there, then focus it and close the popup.
///
/// Every part of that — canonicalising the path, refusing one that is not a directory,
/// proposing a name, the create hooks, `zoxide add` — belongs to
/// [`crate::session::create::open_or_create`], which `taw session open` runs too; this
/// only adds the popup's own client switch, since `open_or_create` deliberately never
/// switches anything.
///
/// The collision note rides in front of a failed switch the way the worktree verbs' note
/// does: when the proposed name had to grow to dodge one, a popup left open by a failed
/// switch says which session it made, not just why it could not go there.
fn open_project(t: &Tmux, cfg: &Config, client: Option<&str>, path: &str) -> Outcome {
    let home = dirs::home_dir().unwrap_or_default();
    let outcome = match crate::session::create::open_or_create(t, cfg, path, &home) {
        Ok(outcome) => outcome,
        Err(e) => return Outcome::toast(failure_toast(&e)),
    };
    let note = collision_note(&outcome);
    switch_outcome(
        switch_client(
            t,
            client,
            &crate::tmux::session_target(&outcome.session_name),
        ),
        note.as_deref(),
    )
}

/// The toast for something a hook may have stopped: an open, a close, a worktree removal. When
/// a hook is what stopped it, the hook says so in its own one-line summary, which names it;
/// anything else has only [`short`]'s reason.
fn failure_toast(e: &anyhow::Error) -> String {
    match e.downcast_ref::<crate::session::create::HookFailure>() {
        Some(failure) => failure.summary(),
        None => short(e),
    }
}

/// `prefix+O` from inside the popup: go to the next agent, next up.
///
/// [`notify::open_target`] picks the pane (the next agent after this one, in tmux order, that
/// needs you, else of any kind) and selects its window and pane; `focus: false`
/// stops it there, because the `switch-client` it would otherwise run has
/// no `-c` and tmux cannot resolve a current client from inside a `display-popup` — on a
/// multi-client server that could move a client the user is not looking at. The switch
/// happens here instead, through the same resolution every other exiting verb uses. The
/// pane id is a legal `-t` for `switch-client` — tmux resolves it to the session holding
/// that pane — so no second lookup is needed to name the session.
///
/// `own` is the pane to walk from — the pane the popup was opened from, carried by
/// [`Action::NextUp`] from the snapshot rather than read out of the environment
/// (there is no `$TMUX_PANE` inside a `display-popup`), so a test can supply any pane it
/// likes. With `None`, `notify::open_target` asks tmux for its own current pane.
fn next_up(t: &Tmux, client: Option<&str>, own: Option<&str>) -> Outcome {
    match notify::open_target(t, false, own) {
        Ok(Some((pane, _group))) => switch_outcome(switch_client(t, client, &pane), None),
        Ok(None) => Outcome::toast(NO_AGENTS),
        Err(e) => Outcome::toast(short(&e)),
    }
}

/// `switch-client -c <the client the popup is on> -t <target>`.
///
/// `client` is the client the launcher opened the popup on and named (`taw ui --client`),
/// so the popup moves the client it is shown on, however many clients share the session.
/// Without one (a `taw ui` typed in a pane has no launcher to say) the client is picked by
/// hand: a `display-popup -E` command runs inside its own popup, not inside a pane, so
/// `$TMUX_PANE` is unset and tmux has no current client to resolve `-t` against. A headless
/// `taw` invocation, a key binding's `run-shell` or a hook, needs the same rule, so
/// [`crate::cli::common::switch_client_to`] owns the one copy of it and this calls that,
/// with no pane. What stays here is the error: the CLI reports "could not switch" as a
/// `bool`, while the popup's toast has to say *why*.
///
/// A session by name is passed as [`crate::tmux::session_target`]'s `=name`, so the
/// home directory's session, called `~`, is not the marked pane.
fn switch_client(t: &Tmux, client: Option<&str>, target: &str) -> anyhow::Result<()> {
    match client {
        Some(client) => t.run_ok(&["switch-client", "-c", client, "-t", target]),
        None => crate::cli::common::switch_client_to(t, target, None),
    }
}

/// The doorway: `tmux detach-client -t <client> -E "<command>"`, the command being
/// [`crate::cli::ui_open::doorway_command`] (`<taw> app --go <machine>:<id>` under an `env` that names the popup's
/// own server). tmux detaches the client and runs the app in its terminal in the client's place (the same process,
/// the same tty), and the popup, which is the client's, ends with it; `taw app` is then the terminal's program
/// until it ends, and the terminal goes back to the shell that ran `tmux attach`. The command is quoted for the
/// client's shell (sh-like; a backslash in the path is a known caveat): tmux runs `-E` with the
/// session's default shell (not confirmed here). With no client named (`taw ui` typed in a
/// pane) tmux's current client is the one. An id that is no id runs nothing.
///
/// The socket path is the popup's own server's: known when the popup talks to one by path, else asked of tmux
/// with `#{socket_path}` (a named socket or the default one).
fn doorway(t: &Tmux, client: Option<&str>, taw_bin: &Path, machine: &str, id: &str) -> Outcome {
    if !crate::machine::valid_go_id(id) {
        return Outcome::toast(crate::machine::words::NOTHING_TO_GO_IN_TO);
    }
    let socket = match &t.socket {
        Some(crate::tmux::Socket::Path(path)) => path.clone(),
        _ => match t.run(&["display-message", "-p", "#{socket_path}"]) {
            Ok(path) if !path.trim().is_empty() => std::path::PathBuf::from(path.trim()),
            Ok(_) => return Outcome::toast(crate::machine::words::NO_SERVER_PATH),
            Err(e) => return Outcome::toast(short(&e)),
        },
    };
    // The session the popup was opened from, which the app puts the terminal back in when it ends. A lookup that
    // fails leaves the flag out: the app then ends as it did, and the doorway still opens.
    let back = session_of(t, client);
    let Some(command) =
        crate::cli::ui_open::doorway_command(taw_bin, &socket, machine, id, back.as_deref())
    else {
        return Outcome::toast(crate::machine::words::NOTHING_TO_GO_IN_TO);
    };
    let mut args = vec!["detach-client"];
    if let Some(client) = client {
        args.extend(["-t", client]);
    }
    args.extend(["-E", &command]);
    match t.run_ok(&args) {
        Ok(()) => Outcome::exit(),
        Err(e) => Outcome::toast(short(&e)),
    }
}

/// The id (`$3`) of the session `client` is on, or of tmux's current client's when none is named; `None` when tmux
/// does not say one.
fn session_of(t: &Tmux, client: Option<&str>) -> Option<String> {
    let mut args = vec!["display-message", "-p"];
    if let Some(client) = client {
        args.extend(["-c", client]);
    }
    args.push("#{session_id}");
    let id = t.run(&args).ok()?;
    let id = id.trim();
    crate::machine::valid_session_id(id).then(|| id.to_string())
}

/// herdr's agent-name rule: lowercase, starts with a letter, up to 32 characters of
/// `[a-z0-9_-]`.
pub fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name.len() > 32 {
        return Err("name must be at most 32 characters".to_string());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap_or_default();
    if !first.is_ascii_lowercase() {
        return Err("name must start with a lowercase letter".to_string());
    }
    if let Some(bad) = chars.find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '_' | '-')) {
        return Err(format!("'{bad}' is not allowed in a name (a-z, 0-9, _, -)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// tmux 3.6b and 3.8 print nothing for `#{session_group_size}` when the session is in no group:
    /// such a session with one window of one pane is the one closing the pane closes.
    #[test]
    fn a_session_in_no_group_with_one_pane_is_found_with_tmux_s_empty_group_size() {
        assert_eq!(sole_pane_of("$3 1 1 ").as_deref(), Some("$3"));
        assert_eq!(sole_pane_of("$3 1 1 0").as_deref(), Some("$3"));
        assert_eq!(sole_pane_of("$3 1 1 1").as_deref(), Some("$3"));
        assert_eq!(sole_pane_of("$3 1 1 2"), None);
        assert_eq!(sole_pane_of("$3 2 1 "), None);
        assert_eq!(sole_pane_of("$3 1 2 "), None);
        assert_eq!(sole_pane_of("$3 1"), None);
    }

    /// The plugin's key turns the CLI's not-found into these words itself (a shell line that cannot name a Rust
    /// constant), so the one constant is tied to that copy here: changing either alone fails this.
    #[test]
    fn the_plugins_key_says_the_same_words_as_no_agents() {
        let plugin = include_str!("../../tmux-agent-wizard.tmux");
        assert!(
            plugin.contains(&format!("display-message '{NO_AGENTS}'")),
            "tmux-agent-wizard.tmux does not say {NO_AGENTS:?} on a not-found"
        );
    }

    /// The `tmux` binary a test points [`exec`] at: a path that is not there, so every
    /// command fails before it runs and [`crate::tmux::Tmux::run`]'s context reports the
    /// whole argv it tried. That is how a test with no tmux server reads back the command
    /// an arm built — the argv *is* the behaviour, and a swapped target or a reordered
    /// flag is exactly what these pin.
    const MISSING_TMUX: &str = "/nonexistent/taw-test-tmux";

    fn broken_tmux() -> Tmux {
        Tmux {
            bin: PathBuf::from(MISSING_TMUX),
            socket: None,
        }
    }

    /// Runs `action` against [`broken_tmux`] and returns the toast it produced, asserting
    /// on the way that a failed command neither takes the popup down (`exec` returns `Ok`)
    /// nor closes it.
    fn failed_toast(action: &Action) -> String {
        failed_toast_on(action, None)
    }

    /// [`failed_toast`] for a popup that was told which client it is on.
    fn failed_toast_on(action: &Action, client: Option<&str>) -> String {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let outcome = exec(
            &broken_tmux(),
            &cfg,
            &paths,
            Path::new("/nonexistent/taw"),
            action,
            client,
        )
        .expect("a failing tmux command is a toast, never an error out of the popup");
        assert!(!outcome.exit, "a failed action keeps the popup open");
        outcome.toast.expect("a failed action toasts")
    }

    /// The toast [`failed_toast`] gets when the command `argv` never ran at all.
    fn never_ran(argv: &str) -> String {
        format!("failed to run `{MISSING_TMUX} {argv}`")
    }

    /// [`never_ran`] without its closing backtick, for a command whose arguments are a
    /// long `-F` format an assertion has no reason to spell out in full.
    fn tried(argv: &str) -> String {
        format!("failed to run `{MISSING_TMUX} {argv}")
    }

    /// A config whose `[hooks]` is `toml`.
    fn hooked(toml: &str) -> Config {
        toml::from_str(toml).expect("the test config parses")
    }

    /// The `env` prefix of the doorway command for the socket `/run/work.sock`.
    const PINNED: &str =
        "env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME 'TAW_TMUX_SOCKET_PATH=/run/work.sock'";

    /// A tmux that talks to the socket `/run/work.sock` by path and is not there to run anything.
    fn pinned_tmux() -> Tmux {
        Tmux {
            bin: PathBuf::from(MISSING_TMUX),
            socket: Some(crate::tmux::Socket::Path(PathBuf::from("/run/work.sock"))),
        }
    }

    /// The doorway runs `detach-client -t <client> -E "<command>"`: the binary and the target quoted for `sh`,
    /// under an `env` that clears what names a server and pins the popup's own, and an id that is no id runs
    /// nothing. A popup with no client named leaves out `-t`.
    #[test]
    fn the_doorway_detaches_the_client_into_the_app_going_to_the_machines_own_id() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let go_on = |t: &Tmux, bin: &str, client: Option<&str>, machine: &str, id: &str| {
            let action = Action::Doorway {
                machine: machine.into(),
                id: id.into(),
            };
            exec(t, &cfg, &paths, Path::new(bin), &action, client).unwrap()
        };
        let go = |bin: &str, client: Option<&str>, machine: &str, id: &str| {
            go_on(&pinned_tmux(), bin, client, machine, id)
        };
        let ran = |outcome: Outcome| outcome.toast.expect("the broken tmux names what it tried");
        assert_eq!(
            ran(go("/opt/taw/taw", Some("/dev/ttys003"), "studio", "%16")),
            never_ran(&format!(
                "detach-client -t /dev/ttys003 -E {PINNED} /opt/taw/taw app --go 'studio:%16'"
            ))
        );
        assert_eq!(
            ran(go("/opt/taw/taw", None, "studio", "$3")),
            never_ran(&format!(
                "detach-client -E {PINNED} /opt/taw/taw app --go 'studio:$3'"
            ))
        );
        // A binary with a space and a quote in its path: one word for sh, as the popup's own line has it.
        let odd = ran(go(
            "/tmp/my bin/it's/taw",
            Some("/dev/ttys003"),
            "studio",
            "@5",
        ));
        assert_eq!(
            odd,
            never_ran(&format!(
                "detach-client -t /dev/ttys003 -E {PINNED} '/tmp/my bin/it'\\''s/taw' app --go studio:@5"
            ))
        );
        // A socket path with a space and a quote: the whole `VAR=value` word is one word for sh.
        let mut spaced = pinned_tmux();
        spaced.socket = Some(crate::tmux::Socket::Path(PathBuf::from(
            "/tmp/a b/it's.sock",
        )));
        assert_eq!(
            ran(go_on(&spaced, "/opt/taw/taw", None, "studio", "%16")),
            never_ran(
                "detach-client -E env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME \
                 'TAW_TMUX_SOCKET_PATH=/tmp/a b/it'\\''s.sock' /opt/taw/taw app --go 'studio:%16'"
            )
        );
        // An id that is no id, or one with more in it, is never sent anywhere.
        for id in ["", "7", "%", "%7;ls", "w5", "%7 x"] {
            let outcome = go("/opt/taw/taw", Some("/dev/ttys003"), "studio", id);
            assert_eq!(
                outcome.toast.as_deref(),
                Some(crate::machine::words::NOTHING_TO_GO_IN_TO),
                "{id:?}"
            );
            assert!(!outcome.exit, "{id:?}");
        }
    }

    /// A popup on a named socket or the default one asks tmux for `#{socket_path}` and pins the app to the path it
    /// is told; a tmux that cannot be asked toasts and runs no detach.
    #[test]
    fn the_doorway_asks_tmux_for_the_socket_path_when_the_popup_only_knows_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let stub = dir.path().join("tmux");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = '#{{socket_path}}' ]; then\n    echo /run/named.sock\n    exit 0\n  fi\n  if [ \"$a\" = '#{{session_id}}' ]; then\n    echo '$3'\n    exit 0\n  fi\ndone\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        {
            crate::test_exec::make_executable(&stub);
        }
        let cfg = Config::default();
        let paths = Paths::from_env();
        let action = Action::Doorway {
            machine: "studio".into(),
            id: "%16".into(),
        };
        for socket in [None, Some(crate::tmux::Socket::Name("work".into()))] {
            let t = Tmux {
                bin: stub.clone(),
                socket,
            };
            let outcome = exec(&t, &cfg, &paths, Path::new("/opt/taw/taw"), &action, None).unwrap();
            assert!(outcome.exit, "the stub tmux took the detach");
        }
        let seen = std::fs::read_to_string(&log).unwrap();
        let tail = "detach-client -E env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME \
             'TAW_TMUX_SOCKET_PATH=/run/named.sock' /opt/taw/taw app --go 'studio:%16' --back '$3'\n";
        assert_eq!(seen, format!("-u {tail}-L work -u {tail}"));
        // A tmux that cannot be asked: the reason is the toast and nothing is detached.
        let outcome = exec(
            &broken_tmux(),
            &cfg,
            &paths,
            Path::new("/opt/taw/taw"),
            &action,
            None,
        )
        .unwrap();
        assert_eq!(
            outcome.toast.as_deref(),
            Some(never_ran("display-message -p #{socket_path}").as_str())
        );
        assert!(!outcome.exit);
    }

    /// The session the popup was opened from is asked of tmux for the client the popup is on (`-c`), and carried
    /// as `--back`; an answer that is no session id leaves the flag out, and the doorway still opens.
    #[test]
    fn the_doorway_carries_the_session_the_popup_was_opened_from() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let stub = dir.path().join("tmux");
        // `$ANSWER` is what `#{session_id}` is told as.
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = '#{{session_id}}' ]; then\n    printf '%s\\n' \"$*\" >> {0}\n    echo \"$ANSWER\"\n    exit 0\n  fi\ndone\nprintf '%s\\n' \"$*\" >> {0}\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        {
            crate::test_exec::make_executable(&stub);
        }
        let cfg = Config::default();
        let paths = Paths::from_env();
        let action = Action::Doorway {
            machine: "studio".into(),
            id: "%16".into(),
        };
        let t = Tmux {
            bin: stub.clone(),
            socket: Some(crate::tmux::Socket::Path(PathBuf::from("/run/work.sock"))),
        };
        for (answer, client, tail) in [
            ("$7", Some("/dev/ttys003"), " --back '$7'"),
            ("$7", None, " --back '$7'"),
            ("", Some("/dev/ttys003"), ""),
            ("main", Some("/dev/ttys003"), ""),
            ("$7;ls", Some("/dev/ttys003"), ""),
        ] {
            std::fs::write(&log, "").unwrap();
            // SAFETY: a test of this module only; the variable is read by the stub it starts, nothing else.
            unsafe { std::env::set_var("ANSWER", answer) };
            let outcome =
                exec(&t, &cfg, &paths, Path::new("/opt/taw/taw"), &action, client).unwrap();
            assert!(outcome.exit, "{answer:?}");
            let seen = std::fs::read_to_string(&log).unwrap();
            let asked = match client {
                Some(client) => {
                    format!("-S /run/work.sock -u display-message -p -c {client} #{{session_id}}\n")
                }
                None => "-S /run/work.sock -u display-message -p #{session_id}\n".to_string(),
            };
            let detach = match client {
                Some(client) => format!("-S /run/work.sock -u detach-client -t {client} -E "),
                None => "-S /run/work.sock -u detach-client -E ".to_string(),
            };
            assert!(seen.starts_with(&asked), "{answer:?}: {seen}");
            let rest = &seen[asked.len()..];
            assert!(
                rest.starts_with(&detach)
                    && rest
                        .trim_end()
                        .ends_with(&format!("app --go 'studio:%16'{tail}")),
                "{answer:?}: {rest}"
            );
        }
    }

    /// A tmux that answers the question of its own socket with nothing says so as a toast, and nothing is
    /// detached: the app could not be pinned to a server.
    #[test]
    fn the_doorway_with_an_empty_socket_answer_says_so_and_detaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let stub = dir.path().join("tmux");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = '#{{socket_path}}' ]; then\n    echo\n    exit 0\n  fi\ndone\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        {
            crate::test_exec::make_executable(&stub);
        }
        let cfg = Config::default();
        let paths = Paths::from_env();
        let action = Action::Doorway {
            machine: "studio".into(),
            id: "%16".into(),
        };
        let t = Tmux {
            bin: stub,
            socket: None,
        };
        let outcome = exec(&t, &cfg, &paths, Path::new("/opt/taw/taw"), &action, None).unwrap();
        assert_eq!(
            outcome.toast.as_deref(),
            Some(crate::machine::words::NO_SERVER_PATH),
            "the popup says why it did not go"
        );
        assert!(!outcome.exit, "the popup stays");
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !seen.contains("detach-client"),
            "nothing was detached: {seen:?}"
        );
    }

    /// Runs `action` against [`broken_tmux`] with `cfg`, and returns what the popup does.
    fn outcome_with(cfg: &Config, action: &Action) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        exec(
            &broken_tmux(),
            cfg,
            &crate::plugin::tests_support::test_paths(dir.path()),
            Path::new("/nonexistent/taw"),
            action,
            None,
        )
        .expect("a refusal is a toast, never an error out of the popup")
    }

    /// `[hooks] pre_close` is asked before tmux is: a refusal is the toast, the popup stays
    /// open, and no `kill-*` was even attempted. With `broken_tmux` that shows as the toast
    /// being the hook's, not tmux's "failed to run".
    #[test]
    fn a_pre_close_hook_that_refuses_stops_a_close_before_tmux_is_asked() {
        let cfg = hooked("[hooks]\npre_close = \"echo 'not while it works' >&2; exit 1\"\n");
        for action in [
            Action::KillPane {
                pane: "%3".to_string(),
            },
            Action::KillWorkspace {
                session: "$3".to_string(),
            },
        ] {
            let outcome = outcome_with(&cfg, &action);

            assert_eq!(
                outcome.toast.as_deref(),
                Some("pre-close hook: not while it works"),
                "{action:?}"
            );
            assert!(!outcome.exit, "a refused close keeps the popup open");
        }
    }

    #[test]
    fn a_pre_close_hook_that_says_yes_leaves_the_close_to_tmux() {
        let cfg = hooked("[hooks]\npre_close = \"exit 0\"\n");

        let outcome = outcome_with(
            &cfg,
            &Action::KillWorkspace {
                session: "$3".to_string(),
            },
        );

        assert_eq!(outcome.toast, Some(never_ran("kill-session -t $3")));
    }

    #[test]
    fn a_refusal_with_nothing_said_shows_the_exit_status() {
        let cfg = hooked("[hooks]\npre_close = \"exit 7\"\n");

        let outcome = outcome_with(
            &cfg,
            &Action::KillPane {
                pane: "%3".to_string(),
            },
        );

        assert_eq!(
            outcome.toast.as_deref(),
            Some("pre-close hook refused (exit status 7)")
        );
    }

    /// `pre_worktree_remove` is asked inside `worktree::remove`, so the popup's `x` on a closed
    /// worktree and `taw worktree remove` are both refused by it, and the toast is the hook's
    /// own line and not git's.
    #[test]
    fn a_pre_worktree_remove_hook_that_refuses_is_the_removals_toast() {
        let cfg =
            hooked("[hooks]\npre_worktree_remove = \"echo 'feat/x is not merged' >&2; exit 1\"\n");
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(
            paths.state_dir.join("worktrees.json"),
            r#"{"worktrees":[{"workspace_id":"w9","session_name":"repo--feat-x","path":"/nonexistent/repo--feat-x","branch":"feat/x","base":null,"repo_root":"/nonexistent/repo","source_workspace_id":"w1","created_unix_ms":0}]}"#,
        )
        .unwrap();

        let outcome = exec(
            &broken_tmux(),
            &cfg,
            &paths,
            Path::new("/nonexistent/taw"),
            &Action::RemoveWorktree {
                workspace: "w9".to_string(),
                force: false,
            },
            None,
        )
        .unwrap();

        assert_eq!(
            outcome.toast.as_deref(),
            Some("pre-worktree-remove hook: feat/x is not merged")
        );
        assert!(!outcome.exit);
    }

    #[test]
    fn renaming_a_window_runs_rename_window_with_the_new_name_last() {
        assert_eq!(
            failed_toast(&Action::RenameWindow {
                window: "@9".to_string(),
                name: "review notes".to_string(),
            }),
            never_ran("rename-window -t @9 -- review notes")
        );
    }

    /// A name the popup renames a workspace or a window to is what tmux calls it, a `#` or a `;` at its
    /// end included: tmux expands formats in the name, so each `#` goes as `##`, and ends a command at a
    /// word that ends in `;`, so that one goes with a `\` before it ([`crate::tmux::new_name`]). The
    /// target goes as it is, and a `--` ahead of the name keeps tmux from reading a name that starts
    /// with a dash as a flag.
    #[test]
    fn a_name_the_popup_renames_to_is_given_so_that_tmux_takes_it_as_typed() {
        assert_eq!(
            failed_toast(&Action::RenameWorkspace {
                session: "$3".to_string(),
                name: "C#Sharp;".to_string(),
            }),
            never_ran("rename-session -t $3 -- C##Sharp\\;")
        );
        assert_eq!(
            failed_toast(&Action::RenameWindow {
                window: "@9".to_string(),
                name: "W#Sx".to_string(),
            }),
            never_ran("rename-window -t @9 -- W##Sx")
        );
    }

    /// The rest of the row verbs, so a reordered arm or a swapped target cannot pass.
    #[test]
    fn the_other_row_verbs_keep_their_argv() {
        let cases = [
            (
                Action::KillPane {
                    pane: "%3".to_string(),
                },
                "kill-pane -t %3",
            ),
            (
                Action::KillWorkspace {
                    session: "api".to_string(),
                },
                "kill-session -t api",
            ),
            (
                Action::RenameWorkspace {
                    session: "api".to_string(),
                    name: "oss/api".to_string(),
                },
                "rename-session -t api -- oss/api",
            ),
        ];
        for (action, argv) in cases {
            assert_eq!(failed_toast(&action), never_ran(argv), "{action:?}");
        }
    }

    /// `OpenProject` hands the path to [`crate::session::create::open_or_create`], and its
    /// refusal of a directory that is not there is what the toast says — no tmux command
    /// is reached at all, so the message is the refusal rather than an argv.
    #[test]
    fn opening_a_project_that_is_not_a_directory_says_so() {
        assert_eq!(
            failed_toast(&Action::OpenProject {
                path: "/nonexistent/taw-test-project".to_string(),
            }),
            "no such directory: /nonexistent/taw-test-project"
        );
    }

    /// Every verb that leaves the popup resolves the client to switch *first*, so with no
    /// server to list clients from it fails there — naming `list-clients`, never
    /// `switch-client`.
    #[test]
    fn a_jump_resolves_the_popups_own_client_before_switching() {
        let toast = failed_toast(&Action::SwitchWorkspace {
            session: "api".to_string(),
        });
        assert!(toast.starts_with(&tried("list-clients")), "{toast}");
    }

    /// The popup was told which client it is on, so every switch moves that one, by name: it
    /// does not list the clients to pick one, which with two on the session would move the
    /// first the server lists, whichever the popup is shown on. A session by id, as the tree
    /// hands them, goes the same way, and so does the jump to a pane.
    #[test]
    fn a_switch_moves_the_client_the_popup_is_on() {
        let on = Some("/dev/ttys021");
        assert_eq!(
            failed_toast_on(
                &Action::SwitchWorkspace {
                    session: "$7".to_string()
                },
                on
            ),
            never_ran("switch-client -c /dev/ttys021 -t $7")
        );
        assert_eq!(
            failed_toast_on(
                &Action::Jump {
                    session: "$7".to_string(),
                    window: "@3".to_string(),
                    pane: "%9".to_string(),
                },
                on
            ),
            never_ran("switch-client -c /dev/ttys021 -t $7")
        );
    }

    /// `prefix+O` delegates to [`notify::open_target`] rather than deciding for itself
    /// which pane is next up, so its first step is that function's own first step: listing
    /// the panes. With no server to list them from, the toast is that command's own failure.
    #[test]
    fn next_up_asks_notify_for_the_target() {
        let toast = failed_toast(&Action::NextUp { own: None });
        assert!(toast.starts_with(&tried("list-panes -a -F")), "{toast}");
    }

    #[test]
    fn agent_names_follow_herdrs_rule() {
        assert_eq!(validate_agent_name("reviewer"), Ok(()));
        assert_eq!(validate_agent_name("a"), Ok(()));
        assert_eq!(validate_agent_name("plan-2_b"), Ok(()));
        assert!(validate_agent_name("").is_err());
        assert!(validate_agent_name("Reviewer").is_err());
        assert!(validate_agent_name("2fast").is_err());
        assert!(validate_agent_name("has space").is_err());
        assert!(validate_agent_name(&"a".repeat(33)).is_err());
        assert_eq!(validate_agent_name(&"a".repeat(32)), Ok(()));
    }

    #[test]
    fn a_multi_line_git_refusal_is_cut_to_its_first_line() {
        let e = anyhow::anyhow!(
            "`git worktree remove /w` failed (exit status: 1): fatal: '/w' contains modified or untracked files, use --force to delete it\nhint: something else"
        );
        assert_eq!(
            short(&e),
            "fatal: '/w' contains modified or untracked files, use --force to delete it"
        );
    }

    #[test]
    fn tmux_error_text_is_trimmed_to_its_reason_for_the_toast_line() {
        let e =
            anyhow::anyhow!("`tmux kill-pane -t %9` failed (exit status: 1): can't find pane %9");
        assert_eq!(short(&e), "can't find pane %9");
        let plain = anyhow::anyhow!("no attached client to switch");
        assert_eq!(short(&plain), "no attached client to switch");
    }

    #[test]
    fn a_failed_jump_toast_carries_only_tmuxs_reason_never_its_path_argv_or_tty() {
        // The raw error `switch-client` produces names the tmux binary path, the client
        // tty and the whole argv; a popup toast must show only tmux's reason.
        let raw = || {
            anyhow::anyhow!(
                "`/opt/homebrew/bin/tmux switch-client -c /dev/ttys028 -t $7` failed (exit status: 1): can't find session: $7"
            )
        };

        let jumped = switch_outcome(Err(raw()), None);
        assert_eq!(jumped.toast.as_deref(), Some("can't find session: $7"));
        assert!(!jumped.exit, "a failed switch keeps the popup open");

        // The worktree paths keep their "created/opened worktree X" note in front.
        let entered = switch_outcome(Err(raw()), Some("created worktree feature"))
            .toast
            .expect("a failed switch toasts");
        assert_eq!(entered, "created worktree feature (can't find session: $7)");

        for leaked in [
            "/opt/homebrew/bin/tmux",
            "/dev/ttys028",
            "switch-client",
            "failed",
            "exit status",
            "`",
        ] {
            assert!(
                !jumped.toast.as_deref().unwrap().contains(leaked) && !entered.contains(leaked),
                "toast leaked {leaked:?}",
            );
        }

        // A successful switch exits the popup with nothing to say.
        let ok = switch_outcome(Ok(()), Some("created worktree feature"));
        assert!(ok.exit && ok.toast.is_none());
    }

    /// The question for a folder git has let go of names the folder, as a path under home is
    /// written (`~/...`), because that is what `y` deletes; the other two do not.
    #[test]
    fn the_question_for_a_leftover_folder_names_it_and_the_others_do_not() {
        use worktree::RemoveRefusal::{Dirty, Leftover, Submodules};
        let folder = Path::new("/work/api--feat-pdf");
        assert_eq!(
            removal_question(Leftover, "feat/pdf", folder),
            "git no longer lists feat/pdf, but part of its folder is still there: /work/api--feat-pdf. remove anyway? uncommitted changes are lost"
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                removal_question(Leftover, "feat/pdf", &home.join("wt/api--feat-pdf")),
                "git no longer lists feat/pdf, but part of its folder is still there: ~/wt/api--feat-pdf. remove anyway? uncommitted changes are lost"
            );
        }
        for why in [Dirty, Submodules] {
            assert!(
                !removal_question(why, "feat/pdf", folder).contains("api--feat-pdf"),
                "{why:?}"
            );
        }
    }

    /// The session a popup's client is attached to closes after every other one of its group,
    /// whatever place it had, and everything else keeps its order.
    #[test]
    fn a_group_closes_the_popups_own_session_last_and_keeps_the_rest_in_order() {
        let group = vec!["$3", "$4", "$1"];
        assert_eq!(
            client_session_last(group.clone(), Some("$3")),
            ["$4", "$1", "$3"]
        );
        assert_eq!(
            client_session_last(group.clone(), Some("$4")),
            ["$3", "$1", "$4"]
        );
        assert_eq!(
            client_session_last(group.clone(), Some("$1")),
            ["$3", "$4", "$1"]
        );
        assert_eq!(client_session_last(group.clone(), Some("$9")), group);
        assert_eq!(client_session_last(group.clone(), None), group);
    }

    /// One tmux command line for the whole close, each kill told apart by a `;` argument.
    #[test]
    fn a_group_close_is_one_command_line() {
        assert_eq!(
            kill_sessions_args(&["$3", "$1"]),
            ["kill-session", "-t", "$3", ";", "kill-session", "-t", "$1"]
        );
        assert_eq!(kill_sessions_args(&["$1"]), ["kill-session", "-t", "$1"]);
    }
    // ------------------------------------------------ a landing in the app

    use crate::cli::test_support::{Stub, calls, stub_tmux};

    fn land_on(tmux: &Tmux, action: &Action) -> Result<Landing, String> {
        let dir = tempfile::tempdir().unwrap();
        land(
            tmux,
            &Config::default(),
            &crate::plugin::tests_support::test_paths(dir.path()),
            Path::new("/nonexistent/taw"),
            action,
        )
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    /// A jump in the app selects the window and then the pane, and attaches the pane's session: it
    /// switches no client, which the app has none of.
    #[test]
    fn a_jump_in_the_app_selects_the_window_and_the_pane_and_names_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let landing = land_on(
            &tmux,
            &Action::Jump {
                session: "$7".into(),
                window: "@4".into(),
                pane: "%9".into(),
            },
        )
        .unwrap();

        assert_eq!(
            landing,
            Landing {
                target: "$7".into(),
                pane: Some("%9".into()),
                note: None,
            }
        );
        assert_eq!(
            calls(&log),
            vec![
                argv(&["select-window", "-t", "$7:@4"]),
                argv(&["select-pane", "-t", "%9"])
            ]
        );
    }

    /// A session header is attached as it was left: nothing is selected, and the pane to come back to
    /// is the session's active one.
    #[test]
    fn a_workspace_in_the_app_is_attached_as_it_was_left() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let landing = land_on(
            &tmux,
            &Action::SwitchWorkspace {
                session: "$7".into(),
            },
        )
        .unwrap();

        assert_eq!(landing.target, "$7");
        assert_eq!(landing.pane.as_deref(), Some("%5"), "the active pane");
        let said = calls(&log);
        assert!(
            said.iter().all(|call| call[0] == "display-message"),
            "nothing was selected or switched: {said:?}"
        );
    }

    /// A project opened in the app is a session made detached, and the landing names it as `=name` (a bare
    /// name would match a prefix) with the pane tmux says is its active one.
    #[test]
    fn a_project_opened_in_the_app_lands_on_the_new_session_by_its_exact_name() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir(&project).unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let mut cfg = Config::default();
        cfg.sessions.zoxide = false;
        let landing = land(
            &tmux,
            &cfg,
            &crate::plugin::tests_support::test_paths(dir.path()),
            Path::new("/nonexistent/taw"),
            &Action::OpenProject {
                path: project.display().to_string(),
            },
        )
        .unwrap();

        let said = calls(&log);
        let made = said
            .iter()
            .find(|call| call[0] == "new-session")
            .expect("the session was made");
        let name = made[made.iter().position(|a| a == "-s").unwrap() + 1].clone();
        assert_eq!(landing.target, format!("={name}"));
        assert_eq!(landing.pane.as_deref(), Some("%5"), "tmux's active pane");
        assert!(
            said.iter()
                .any(|call| call[0] == "display-message" && call.contains(&landing.target)),
            "the pane was asked of the exact name: {said:?}"
        );
    }

    /// The popup's words for a landing travel with it, so the app can say them when it is back from the
    /// attach: the name a project's session had to take to dodge another, and what a worktree did.
    #[test]
    fn a_landing_carries_the_words_the_popup_would_have_said() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _log) = stub_tmux(dir.path(), &Stub::default());
        let mut cfg = Config::default();
        cfg.sessions.zoxide = false;
        let paths = crate::plugin::tests_support::test_paths(dir.path());

        // The stand-in runs one session, `main`, in /tmp: a project of that name elsewhere gets another.
        let project = dir.path().join("main");
        std::fs::create_dir(&project).unwrap();
        let landing = land(
            &tmux,
            &cfg,
            &paths,
            Path::new("/nonexistent/taw"),
            &Action::OpenProject {
                path: project.display().to_string(),
            },
        )
        .unwrap();
        let note = landing.note.expect("a note for the name that changed");
        assert!(
            note.starts_with("session ") && !note.starts_with("session main "),
            "{note}"
        );

        // A worktree whose session is running is switched to, one whose session is not is opened.
        let checkout = dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let record = |name: &str, id: &str| {
            serde_json::json!({
                "workspace_id": id,
                "session_name": name,
                "path": checkout,
                "branch": format!("branch-{name}"),
                "base": null,
                "repo_root": checkout,
                "source_workspace_id": "w1",
                "created_unix_ms": 1,
            })
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(
            paths.state_dir.join("worktrees.json"),
            serde_json::json!({ "worktrees": [record("main", "w7"), record("rain", "w8")] })
                .to_string(),
        )
        .unwrap();
        let said = |workspace: &str| {
            land(
                &tmux,
                &cfg,
                &paths,
                Path::new("/nonexistent/taw"),
                &Action::OpenWorktree {
                    workspace: workspace.into(),
                },
            )
            .unwrap()
            .note
        };
        assert_eq!(
            said("w7").as_deref(),
            Some("switched to worktree branch-main")
        );
        assert_eq!(said("w8").as_deref(), Some("opened worktree branch-rain"));
        assert_eq!(created_note("feature"), "created worktree feature");
    }

    /// A worktree made from the app is a worktree really made (in a temp git repository), and its landing
    /// carries the note the popup says for it, through the same `created_note`.
    #[test]
    fn a_worktree_made_by_a_landing_carries_the_created_note() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = crate::git::command()
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                ])
                .args(args)
                .status()
                .expect("run git")
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "first"]);

        // A tmux that runs one session, `main`, in the repository, and a second one once `new-session` ran.
        let made = dir.path().join("made");
        let script = dir.path().join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = -u ] && shift\ncase \"$1\" in\n  list-sessions)\n    printf '$3\\tmain\\t{repo}\\t1\\t1\\t1700000000\\t\\n'\n    if [ -f '{made}' ]; then printf '$9\\t%s\\t/tmp\\t0\\t1\\t1700000001\\t\\n' \"$(cat '{made}')\"; fi ;;\n  new-session)\n    while [ $# -gt 0 ]; do [ \"$1\" = -s ] && {{ printf '%s' \"$2\" > '{made}'; cat '{made}'; echo; }}; shift; done ;;\nesac\nexit 0\n",
                repo = repo.display(),
                made = made.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);
        let tmux = Tmux {
            bin: script,
            socket: None,
        };
        let mut cfg = Config::default();
        cfg.sessions.zoxide = false;
        cfg.worktrees.directory = dir.path().join("worktrees");
        let paths = crate::plugin::tests_support::test_paths(dir.path());

        let landing = land(
            &tmux,
            &cfg,
            &paths,
            Path::new("/nonexistent/taw"),
            &Action::NewWorktree {
                workspace: "w3".into(),
                branch: "feature".into(),
            },
        )
        .unwrap();

        assert_eq!(landing.note.as_deref(), Some("created worktree feature"));
        assert!(
            dir.path().join("worktrees/repo--feature").is_dir(),
            "the worktree was made on disk"
        );
    }

    /// A directory that is not there is the toast's words and no session: nothing of tmux is changed.
    #[test]
    fn a_project_that_is_not_there_says_so_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let said = land_on(
            &tmux,
            &Action::OpenProject {
                path: "/nonexistent-dir-for-taw".into(),
            },
        )
        .unwrap_err();

        assert!(said.contains("no such directory"), "{said}");
        assert!(
            calls(&log).iter().all(|call| call[0] != "new-session"),
            "{:?}",
            calls(&log)
        );
    }

    /// An action that goes nowhere has no landing.
    #[test]
    fn an_action_that_is_not_a_landing_has_nowhere_to_go() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let said = land_on(&tmux, &Action::KillPane { pane: "%3".into() }).unwrap_err();
        assert_eq!(said, "nothing to go in to");
        assert!(calls(&log).is_empty());
    }

    /// Going back in at a pane finds its session and window, selects them, and names the session.
    #[test]
    fn going_back_in_at_a_pane_selects_its_window_and_its_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let landing = land_on_pane(&tmux, "%5").unwrap();

        assert_eq!(landing.target, "$3");
        assert_eq!(landing.pane.as_deref(), Some("%5"));
        let said = calls(&log);
        assert_eq!(
            said[1..],
            [
                argv(&["select-window", "-t", "$3:@1"]),
                argv(&["select-pane", "-t", "%5"])
            ]
        );
        // A pane tmux no longer has is no landing.
        assert_eq!(land_on_pane(&broken_tmux(), "%5"), None);
    }

    /// A close that takes the client's own session moves it to tmux's last session when that survives,
    /// else to the first survivor in tmux's order, and to nowhere when none survives; a close that does
    /// not take it moves nothing.
    #[test]
    fn a_close_of_your_own_session_leaves_you_in_the_last_session_or_the_first_survivor() {
        let all = ["$1", "$2", "$3", "$4"];
        let go = |id: &str| Leave::GoTo(id.to_string());
        // The last session survives.
        assert_eq!(plan_leave(&["$2"], "$2", Some("$4"), &all), go("$4"));
        // The last session is among those closing: the first survivor.
        assert_eq!(plan_leave(&["$2", "$4"], "$2", Some("$4"), &all), go("$1"));
        // No last session, or one that is gone.
        assert_eq!(plan_leave(&["$1"], "$1", None, &all), go("$2"));
        assert_eq!(plan_leave(&["$1"], "$1", Some("$9"), &all), go("$2"));
        // A group close that takes the client's own session.
        assert_eq!(
            plan_leave(&["$1", "$2", "$3"], "$3", Some("$2"), &all),
            go("$4")
        );
        // Nothing survives.
        assert_eq!(plan_leave(&["$1"], "$1", None, &["$1"]), Leave::NoneLeft);
        assert_eq!(
            plan_leave(&["$1", "$2"], "$2", Some("$1"), &["$1", "$2"]),
            Leave::NoneLeft
        );
        // The client's session stays open: it stays.
        assert_eq!(plan_leave(&["$2"], "$1", Some("$4"), &all), Leave::Stay);
    }
}
