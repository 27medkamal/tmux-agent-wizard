//! The walk of "next up" across machines, read off a [`Snapshot`] alone: the stops, in the app's names,
//! and the stop after a pane. The stops are chosen as [`attention::next_up`] chooses them (what needs you
//! while any agent does, else every agent), independent of what the list shows: a filter, a fold or a
//! depth hides rows, not stops. A machine that does not answer has no stops.

use crate::attention::Group;
use crate::tui::data::Snapshot;

/// A pane's place in the walk: its session's place among the snapshot's sessions (this Mac's first, then
/// each machine's in the config's order), then its window index and pane index.
///
/// A local agent whose session `list-sessions` did not report has no place among the sessions: it sorts last (as
/// [`attention::position`] sorts it), and such agents of two machines can tie at the last place. That takes a race
/// inside one tmux read, and the next tick (500 ms) computes the walk again, so it is left as it is.
pub type Place = (usize, u32, u32);

/// Every stop with its place, in walk order.
fn placed_stops(snap: &Snapshot) -> Vec<(Place, &str)> {
    let mut agents: Vec<(Group, Place, &str)> = snap
        .agents
        .iter()
        .filter(|agent| !snap.greyed(agent.machine))
        .map(|agent| {
            (
                Group::of(agent.state, agent.seen),
                (agent.space_order, agent.window_index, agent.pane_index),
                agent.pane_id.as_str(),
            )
        })
        .collect();
    if agents.iter().any(|&(group, ..)| group == Group::NeedsYou) {
        agents.retain(|&(group, ..)| group == Group::NeedsYou);
    }
    agents.sort_by_key(|&(_, place, _)| place);
    agents
        .into_iter()
        .map(|(_, place, id)| (place, id))
        .collect()
}

/// The stops of the walk in the app's names, in walk order.
pub fn stops(snap: &Snapshot) -> Vec<String> {
    placed_stops(snap)
        .into_iter()
        .map(|(_, id)| id.to_string())
        .collect()
}

/// The place of `pane` (in the app's names) in the snapshot, whether or not it holds an agent.
fn place_of(snap: &Snapshot, pane: &str) -> Option<Place> {
    if let Some(agent) = snap.agents.iter().find(|agent| agent.pane_id == pane) {
        return Some((agent.space_order, agent.window_index, agent.pane_index));
    }
    let pane = snap.panes.iter().find(|row| row.pane_id == pane)?;
    let space = snap
        .spaces
        .iter()
        .position(|space| space.session_id == pane.session_id)?;
    Some((space, pane.window_index, pane.pane_index))
}

/// The stop after `own`, the pane you are in, wrapping: the first stop placed after it, which is the next
/// stop when `own` is one and the first after it when it is not. The first stop when `own` is none or is
/// not in the snapshot, and `own` itself when it is the walk's only stop. `None` when there is no stop.
pub fn next_after(snap: &Snapshot, own: Option<&str>) -> Option<String> {
    let stops = placed_stops(snap);
    let first = stops.first()?;
    let from = own.and_then(|id| place_of(snap, id));
    let next = match from {
        Some(from) => stops
            .iter()
            .find(|(place, _)| *place > from)
            .unwrap_or(first),
        None => first,
    };
    Some(next.1.to_string())
}
