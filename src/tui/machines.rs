//! Machines in the list: each machine's state merged into the popup's [`Snapshot`], and the words its
//! header line says.
//!
//! This is the pure layer: a [`MachineView`] is what the app's connection to a machine (or the
//! machine's file) holds of it, and [`merge`] turns it back into the rows the popup draws, after this
//! machine's own. Nothing here connects anywhere.

use std::path::PathBuf;
use std::str::FromStr;

use crate::git;
use crate::machine::wire::{State, WireAgent, WireGit, WirePane, WireSpace, WireWorktree};
use crate::machine::{MachineView, this_machine_word, words};
use crate::session::worktree::WorktreeRecord;
use crate::store::AgentState;
use crate::tui::data::{AgentRow, MachineHeader, PaneRow, Snapshot, SpaceRow};
use crate::tui::view::COUNTS;

/// `local` with every machine of `views` added, in the order given (the config's).
///
/// Each view gives the snapshot a [`MachineHeader`], and its state's rows, turned back into the rows
/// the popup draws, are appended after this machine's own: a number ([`crate::tui::tree::session_label`])
/// counts across machines, and a machine that appears or drops never moves a number of this one's.
/// A remote row's ids are `<name>:<tmux id>` (`studio:%16`), so that no remote id is ever one of
/// this machine's, and it carries the index of its header in [`Snapshot::machines`].
///
/// A view with no state, or the one that is this machine's own tmux server, adds its header and no
/// rows, `live` or not: the first state of a feed has not come yet. With no views `local` comes back
/// as it is, and [`Snapshot::machines`] stays empty.
///
/// The rows are what the machine sent: raw. A title keeps its leading glyph, which the view takes
/// off when it draws, once, as it does for a row of this machine's.
///
/// `now_ms` (unix milliseconds) is the time of the merge, which the words of a machine that is away
/// age against ("last seen 2m ago"): the caller reads the clock, so the merge is a pure function.
pub fn merge(mut local: Snapshot, views: &[MachineView], now_ms: u64) -> Snapshot {
    for (machine, view) in views.iter().enumerate() {
        local.machines.push(MachineHeader {
            name: view.name.clone(),
            live: view.live,
            words: header_words(view, now_ms),
            last_seen: view.last_seen,
            this_server: view.this_server,
            no_keeper: view.no_keeper,
            stated: view.state.is_some(),
        });
        if view.this_server {
            continue;
        }
        if let Some(state) = &view.state {
            append(&mut local, machine, &view.name, state);
            local.open_worktrees.extend(
                state
                    .open_worktrees
                    .iter()
                    .map(|worktree| worktree_record(worktree, &view.name, machine)),
            );
        }
    }
    local
}

/// `local`, this machine's closed worktrees, with those of every machine of `views` added after
/// them, in the order given (the config's, as [`merge`]'s machines are, so `machine` is an index
/// into the same [`Snapshot::machines`]).
///
/// [`Snapshot`] does not hold the worktrees ([`crate::tui::app::App::worktrees`] does, and
/// [`crate::tui::tree::Row::ClosedWorktree`] indexes it), so they are merged here, and whoever
/// merges a snapshot hands the app these records beside it. A record's id is `<name>:<id>`
/// (`studio:w7`), as every id of a remote row is. A view with no state, or the one that is this
/// machine's own tmux server, adds none; with no views `local` comes back as it is.
pub fn merge_worktrees(
    mut local: Vec<WorktreeRecord>,
    views: &[MachineView],
) -> Vec<WorktreeRecord> {
    for (machine, view) in views.iter().enumerate() {
        if view.this_server {
            continue;
        }
        if let Some(state) = &view.state {
            local.extend(
                state
                    .worktrees
                    .iter()
                    .map(|worktree| worktree_record(worktree, &view.name, machine)),
            );
        }
    }
    local
}

/// What the header says of a machine that is not live when nobody has told it: `connecting`, with how
/// long ago it was last heard from, as of `now`, when it was. A view that carries words keeps them.
fn header_words(view: &MachineView, now: u64) -> String {
    if view.live || !view.words.is_empty() {
        return view.words.clone();
    }
    let seen = view.last_seen.map(|at| now.saturating_sub(at) / 1000);
    words::connecting(seen)
}

/// Unix milliseconds now.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What the bar says of the machines: `3 machines, pi offline`, this machine counted, the names of
/// those that are not live after it ([`words::machines`]). The view of this machine's own tmux server
/// is not another machine: it is not counted and never named, and neither is a machine of the table that no
/// app keeps ([`MachineHeader::no_keeper`]): nobody is looking at it, so it is not offline. Empty when no other
/// machine is listed, so a bar over one machine says nothing of machines.
pub fn machines_part(snap: &Snapshot) -> String {
    let others: Vec<&MachineHeader> = snap
        .machines
        .iter()
        .filter(|m| !m.this_server && !m.no_keeper)
        .collect();
    if others.is_empty() {
        return String::new();
    }
    let offline: Vec<&str> = others
        .iter()
        .filter(|machine| !machine.live)
        .map(|machine| machine.name.as_str())
        .collect();
    words::machines(others.len() + 1, &offline)
}

/// What a machine's header line calls it: this machine's own is [`this_machine_word`].
pub fn header_name(snap: &Snapshot, machine: Option<usize>) -> String {
    match machine {
        None => this_machine_word().to_string(),
        Some(at) => snap
            .machines
            .get(at)
            .map(|m| m.name.clone())
            .unwrap_or_default(),
    }
}

/// What follows a machine's name on its header line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderTail {
    /// A live machine with sessions: its agents by state, in the rows' own order.
    Counts(Vec<(AgentState, usize)>),
    /// Any other: the words it says of itself, dim.
    Words(String),
}

/// The tail of `machine`'s header line: this machine's and a live machine's that lists sessions are
/// counts of its own agents ([`COUNTS`] order, the states nothing is in left out), any other's its words.
pub fn header_tail(snap: &Snapshot, machine: Option<usize>) -> HeaderTail {
    if let Some(at) = machine {
        let header = snap.machines.get(at);
        let listed = snap.spaces.iter().any(|space| space.machine == machine);
        if !header.is_some_and(|h| h.live) || !listed {
            return HeaderTail::Words(header.map(|h| h.words.clone()).unwrap_or_default());
        }
    }
    HeaderTail::Counts(
        COUNTS
            .into_iter()
            .filter_map(|state| {
                let count = snap
                    .agents
                    .iter()
                    .filter(|a| a.machine == machine && a.state == Some(state))
                    .count();
                (count > 0).then_some((state, count))
            })
            .collect(),
    )
}

/// `state`'s rows after `snap`'s, as machine `machine`'s, called `name`.
fn append(snap: &mut Snapshot, machine: usize, name: &str, state: &State) {
    let first_space = snap.spaces.len();
    snap.spaces.extend(
        state
            .spaces
            .iter()
            .map(|space| space_row(space, name, machine)),
    );
    let first_agent = snap.agents.len();
    for agent in &state.agents {
        let mut row = agent_row(agent, name, machine);
        // The place of its session among the rows just added, as `data::build` counts it; a
        // session the state does not list sorts last.
        row.space_order = snap.spaces[first_space..]
            .iter()
            .position(|space| space.session_id == row.session_id)
            .map_or(usize::MAX, |at| first_space + at);
        snap.agents.push(row);
    }
    for pane in &state.panes {
        let mut row = pane_row(pane, name, machine);
        row.agent = snap.agents[first_agent..]
            .iter()
            .position(|agent| agent.pane_id == row.pane_id)
            .map(|at| first_agent + at);
        snap.panes.push(row);
    }
}

/// The id a machine's `id` has in the list: `studio:%16`.
pub(crate) fn namespaced(name: &str, id: &str) -> String {
    format!("{name}:{id}")
}

fn state_of(word: &Option<String>) -> Option<AgentState> {
    word.as_deref()
        .and_then(|word| AgentState::from_str(word).ok())
}

fn git_info(git: &WireGit) -> git::Info {
    git::Info {
        branch: git.branch.clone(),
        ahead: git.ahead,
        behind: git.behind,
        modified: git.modified,
        untracked: git.untracked,
    }
}

/// The reverse of `From<&SpaceRow> for WireSpace`, with its ids namespaced.
fn space_row(space: &WireSpace, name: &str, machine: usize) -> SpaceRow {
    SpaceRow {
        session_id: namespaced(name, &space.session_id),
        session_name: space.session_name.clone(),
        session_path: PathBuf::from(&space.session_path),
        windows: space.windows,
        group_size: space.group_size,
        attached: space.attached,
        active_window_id: namespaced(name, &space.active_window_id),
        active_pane_id: space.active_pane_id.as_ref().map(|id| namespaced(name, id)),
        state: state_of(&space.state),
        agent_count: space.agent_count,
        git: space.git.as_ref().map(git_info),
        machine: Some(machine),
        real_path: space.real_path.as_ref().map(PathBuf::from),
    }
}

/// The reverse of `From<&PaneRow> for WirePane`, with its ids namespaced and no agent yet: the
/// agents come in their own list.
fn pane_row(pane: &WirePane, name: &str, machine: usize) -> PaneRow {
    PaneRow {
        pane_id: namespaced(name, &pane.pane_id),
        session_id: namespaced(name, &pane.session_id),
        window_id: namespaced(name, &pane.window_id),
        window_index: pane.window_index,
        window_name: pane.window_name.clone(),
        window_panes: pane.window_panes,
        pane_index: pane.pane_index,
        pane_current_command: pane.pane_current_command.clone(),
        pane_active: pane.pane_active,
        alternate_on: pane.alternate_on,
        agent: None,
        machine: Some(machine),
    }
}

/// The reverse of `From<&AgentRow> for WireAgent`, with its ids namespaced. `space_order` is the
/// machine's own; [`append`] sets the one the list sorts by. The title is raw.
fn agent_row(agent: &WireAgent, name: &str, machine: usize) -> AgentRow {
    AgentRow {
        pane_id: namespaced(name, &agent.pane_id),
        session_id: namespaced(name, &agent.session_id),
        session_name: agent.session_name.clone(),
        window_id: namespaced(name, &agent.window_id),
        window_index: agent.window_index,
        window_name: agent.window_name.clone(),
        window_panes: agent.window_panes,
        pane_index: agent.pane_index,
        pane_current_command: agent.pane_current_command.clone(),
        pane_title: agent.pane_title.clone(),
        alternate_on: agent.alternate_on,
        pane_dead: agent.pane_dead,
        pane_pid: agent.pane_pid,
        kind: agent.kind.clone(),
        state: state_of(&agent.state),
        state_since: agent.state_since,
        suspended: agent.suspended,
        name: agent.name.clone(),
        display: agent.display.clone(),
        seen: agent.seen,
        container: agent.container.clone(),
        sandbox: agent.sandbox.clone(),
        tokens: agent.tokens.clone(),
        space_order: agent.space_order,
        machine: Some(machine),
    }
}

/// The reverse of `From<&WorktreeRecord> for WireWorktree`, with its id namespaced. The id of the
/// session it was cut from stays the machine's own (`w3`): it is only ever read against that
/// machine's sessions, which [`crate::tui::tree::closed_worktrees`] looks at for a record that has a
/// machine.
fn worktree_record(worktree: &WireWorktree, name: &str, machine: usize) -> WorktreeRecord {
    WorktreeRecord {
        workspace_id: namespaced(name, &worktree.workspace_id),
        session_name: worktree.session_name.clone(),
        path: PathBuf::from(&worktree.path),
        branch: worktree.branch.clone(),
        base: worktree.base.clone(),
        repo_root: PathBuf::from(&worktree.repo_root),
        source_workspace_id: worktree.source_workspace_id.clone(),
        created_unix_ms: worktree.created_unix_ms,
        machine: Some(machine),
        real_path: worktree.real_path.as_ref().map(PathBuf::from),
        real_repo_root: worktree.real_repo_root.as_ref().map(PathBuf::from),
    }
}

#[cfg(test)]
pub(crate) mod tests;
