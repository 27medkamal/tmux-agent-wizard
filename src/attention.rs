//! Attention ordering: which agent panes need a human first.
//!
//! Six tiers, worst first: an agent blocked and not yet looked at outranks one blocked
//! and already seen, which outranks a finished agent not yet looked at, which outranks
//! idle — where a finished agent joins once looked at, since a done agent you have
//! already seen is as quiet as an idle one — which outranks working, then unknown (also
//! anything with no reported state at all, since a pane can be an agent pane on `kind`
//! alone before a state is reported): an agent waiting at its prompt is something to come
//! back to, a running one is not. Within a tier the newest transition
//! sorts first, then ties break on tmux's own order (session position, window, pane) so
//! the list is stable rather than incidental to input order.
//!
//! `Tab` and `prefix+O` share one rule, "next up": the tiers fall into
//! three [`Group`]s (what needs you, idle, the rest). While an agent needs you only those are
//! walked, and with none every agent is, in tmux order, one step a press.
//! [`next_up`] is that walk over tmux's panes; the popup applies the same rule to the rows it
//! draws.

use std::collections::HashMap;

use crate::store::AgentState;
use crate::tmux::{PaneInfo, SessionInfo};

/// Human labels for tiers `0..=5`, in [`tier`]'s order.
pub const TIER_NAMES: [&str; 6] = [
    "blocked, not looked at",
    "blocked, looked at",
    "done, not looked at",
    "idle, or done and looked at",
    "working",
    "unknown",
];

/// The tier of idle agents, which a finished agent joins once looked at: the whole of
/// [`Group::Idle`]. [`tier`] returns it and [`Group::of`] reads the group off it, so the two
/// cannot drift apart.
const IDLE_TIER: u8 = 3;

/// A pane belongs on the Agents panel when tmux says an agent occupies it: a detected
/// kind, or a reported state (a hook can report a state before detection names a kind).
/// Same rule as `tui::data::is_agent`, copied rather than imported so this module does
/// not depend on the TUI.
pub fn is_agent(p: &PaneInfo) -> bool {
    p.agent.kind.is_some() || p.agent.state.is_some()
}

/// The attention tier for an agent in `state`, having been `seen` or not: 0 blocked and
/// unseen, 1 blocked and seen, 2 done and unseen, 3 idle — and done once looked at, since
/// a finished agent you have already seen is as quiet as an idle one — 4 working, 5
/// unknown (also a missing state, since a pane can be an agent pane on `kind` alone before
/// a state is reported). Blocked keeps its place looked at or not: the agent cannot
/// proceed until someone acts. An agent waiting at its prompt is something to come back
/// to, a running one is not.
pub fn tier(state: Option<AgentState>, seen: bool) -> u8 {
    match state {
        Some(AgentState::Blocked) => {
            if seen {
                1
            } else {
                0
            }
        }
        Some(AgentState::Done) => {
            if seen {
                IDLE_TIER
            } else {
                2
            }
        }
        Some(AgentState::Idle) => IDLE_TIER,
        Some(AgentState::Working) => 4,
        Some(AgentState::Unknown) | None => 5,
    }
}

/// Each session's position in `sessions`, keyed by `session_id` — the tiebreaker
/// [`position`] uses, and the source of truth for how a pane whose session is not in the
/// slice sorts last (via `usize::MAX` in [`position`]).
pub fn session_order(sessions: &[SessionInfo]) -> HashMap<String, usize> {
    sessions
        .iter()
        .enumerate()
        .map(|(index, s)| (s.session_id.clone(), index))
        .collect()
}

/// Whether an agent in `state`, `seen` or not, needs a human: tiers 0..=2 — blocked, or
/// finished and not yet looked at. The one predicate behind a row's colour, the border's
/// `N need you` and the first [`Group`] `Tab` and `prefix+O` walk.
pub fn hot(state: Option<AgentState>, seen: bool) -> bool {
    tier(state, seen) <= 2
}

/// The three groups of agents, most urgent first: the agents that need you, the idle ones, and
/// the rest. "Next up" walks what needs you while anything does, and every agent when nothing
/// does, so `Tab` and `prefix+O` never leave an agent that needs you for one that does not. `taw notification
/// open-target` reports the group of the pane it went to. The order the variants are declared
/// in is the order of urgency, which `min` and `<` follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    /// [`hot`]: blocked, looked at or not, and finished but not yet looked at.
    NeedsYou,
    /// Idle, and a finished agent you have looked at: the [`IDLE_TIER`] of [`tier`].
    Idle,
    /// Every other agent: working, unknown, and one that has reported no state yet.
    Rest,
}

impl Group {
    /// The group an agent in `state`, `seen` or not, is in. The first is exactly what
    /// [`hot`] says needs you and the second exactly the [`IDLE_TIER`], so a change to the
    /// tiers moves the groups with them.
    pub fn of(state: Option<AgentState>, seen: bool) -> Group {
        if hot(state, seen) {
            Group::NeedsYou
        } else if tier(state, seen) == IDLE_TIER {
            Group::Idle
        } else {
            Group::Rest
        }
    }
}

/// The pane "next up" goes to from `after`, the pane you are in: the next agent pane in tmux
/// order that needs you, wrapping to the first, while any does; with none, the next agent
/// pane of any kind. The first when `after` is `None` or names a pane tmux no longer has, and
/// `after` itself when it is the walk's only member. The pane's own [`Group`] comes back with
/// it, for `taw notification open-target` to report. `None` when no pane holds an agent.
pub fn next_up<'a>(
    panes: &'a [PaneInfo],
    sessions: &[SessionInfo],
    after: Option<&str>,
) -> Option<(&'a PaneInfo, Group)> {
    let order = session_order(sessions);
    let mut agents: Vec<(Group, &PaneInfo)> = panes
        .iter()
        .filter(|p| is_agent(p))
        .map(|p| (Group::of(p.agent.state, p.agent.seen), p))
        .collect();
    if agents.iter().any(|&(group, _)| group == Group::NeedsYou) {
        agents.retain(|&(group, _)| group == Group::NeedsYou);
    }
    agents.sort_by_key(|&(_, p)| position(p, &order));
    let from = after
        .and_then(|id| panes.iter().find(|p| p.pane_id == id))
        .map(|p| position(p, &order));
    let next = match from {
        Some(from) => agents
            .iter()
            .find(|&&(_, p)| position(p, &order) > from)
            .or_else(|| agents.first()),
        None => agents.first(),
    };
    next.map(|&(group, p)| (p, group))
}

/// A pane's place in tmux's own order: session position (`list-sessions` order), window
/// index, pane index. The order the popup draws and the order [`next_up`] walks.
pub type Position = (usize, u32, u32);

/// [`Position`] of one pane against a `session_order` built by [`session_order`]. A pane
/// whose session is missing from it sorts after every known session.
pub fn position(p: &PaneInfo, session_order: &HashMap<String, usize>) -> Position {
    let order = session_order
        .get(&p.session_id)
        .copied()
        .unwrap_or(usize::MAX);
    (order, p.window_index, p.pane_index)
}

/// The agents that need you ([`hot`]), counted by the state that needs it, most urgent
/// first: each group is placed by the [`tier`] of its most urgent member, so blocked comes
/// before done, and a state nothing needing you is in is not listed. `agents` is each
/// agent's state and whether it has been looked at.
///
/// The tree border and taw's bar count what needs you this way, each group in its state's
/// own glyph and colour, because a bare number cannot say which colour to be: a green row
/// under a count drawn in the blocked colour would read as blocked.
pub fn hot_by_state(
    agents: impl IntoIterator<Item = (Option<AgentState>, bool)>,
) -> Vec<(AgentState, usize)> {
    // (the most urgent tier in the group, its state, how many)
    let mut groups: Vec<(u8, AgentState, usize)> = Vec::new();
    for (state, seen) in agents {
        let Some(state) = state.filter(|_| hot(state, seen)) else {
            continue;
        };
        let tier = tier(Some(state), seen);
        match groups.iter_mut().find(|(_, s, _)| *s == state) {
            Some(group) => {
                group.0 = group.0.min(tier);
                group.2 += 1;
            }
            None => groups.push((tier, state, 1)),
        }
    }
    groups.sort_by_key(|&(tier, ..)| tier);
    groups
        .into_iter()
        .map(|(_, state, count)| (state, count))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AgentRecord, AgentState};
    use crate::tmux::{PaneInfo, SessionInfo};

    fn pane(
        session: &str,
        window: u32,
        pane_index: u32,
        state: Option<AgentState>,
        seen: bool,
        since: u64,
    ) -> PaneInfo {
        PaneInfo {
            pane_id: format!("%{}{}{}", session.len(), window, pane_index),
            session_id: format!("${}", session.len()),
            session_name: session.to_string(),
            session_path: format!("/repos/{session}"),
            session_attached: 0,
            window_id: format!("@{window}"),
            window_index: window,
            window_name: "zsh".to_string(),
            window_active: window == 0,
            window_layout: String::new(),
            pane_index,
            pane_active: pane_index == 0,
            pane_pid: 0,
            pane_current_command: "zsh".to_string(),
            pane_current_path: format!("/repos/{session}"),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord {
                kind: state.map(|_| "claude".to_string()),
                state,
                seen,
                state_since: Some(since),
                ..Default::default()
            },
        }
    }

    fn session(name: &str, index: u32) -> SessionInfo {
        SessionInfo {
            session_id: format!("${}", name.len()),
            session_name: name.to_string(),
            session_path: format!("/repos/{name}"),
            session_attached: u32::from(index == 0),
            session_windows: 1,
            session_group_size: 0,
            active_window_id: "@0".to_string(),
            session_last_attached: 0,
        }
    }

    #[test]
    fn tiers_follow_the_spec() {
        use AgentState::*;
        assert_eq!(tier(Some(Blocked), false), 0);
        assert_eq!(tier(Some(Blocked), true), 1);
        assert_eq!(tier(Some(Done), false), 2);
        assert_eq!(tier(Some(Done), true), 3, "looked at, a done agent is idle");
        assert_eq!(tier(Some(Idle), true), 3);
        assert_eq!(tier(Some(Idle), false), 3);
        assert_eq!(
            tier(Some(Working), true),
            4,
            "running outranks nothing that waits for you"
        );
        assert_eq!(tier(Some(Unknown), true), 5);
        assert_eq!(tier(None, false), 5);
        assert_eq!(TIER_NAMES.len(), 6);
        assert_eq!(TIER_NAMES[3], "idle, or done and looked at");
        assert_eq!(TIER_NAMES[4], "working");
    }

    #[test]
    fn a_finished_agent_stops_needing_you_once_looked_at() {
        use AgentState::*;
        assert_eq!(tier(Some(Done), false), 2);
        assert_eq!(tier(Some(Done), true), 3);
        assert!(hot(Some(Blocked), true));
        assert!(hot(Some(Blocked), false));
        assert!(hot(Some(Done), false));
        assert!(!hot(Some(Done), true));
        assert!(
            !hot(Some(Idle), false),
            "idle never needs you, whatever its tier"
        );
        assert!(!hot(Some(Working), false));
        assert!(!hot(None, false));
    }

    /// What needs you is counted by state, most urgent first: blocked (looked at or not)
    /// before done, only the states with something in them, and nothing that does not need
    /// you (a done agent you have looked at, idle, working, unknown, no state at all).
    #[test]
    fn what_needs_you_is_counted_by_state_most_urgent_first() {
        use AgentState::*;
        assert!(hot_by_state([]).is_empty());
        assert_eq!(hot_by_state([(Some(Done), false)]), [(Done, 1)]);
        assert_eq!(
            hot_by_state([
                (Some(Done), false),
                (Some(Blocked), true),
                (Some(Done), false),
                (Some(Blocked), false),
                (Some(Working), false),
                (Some(Idle), false),
                (Some(Done), true),
                (Some(Unknown), false),
                (None, false),
            ]),
            [(Blocked, 2), (Done, 2)],
            "blocked, seen or not, comes first, and the rest is not counted"
        );
        assert_eq!(
            hot_by_state([(Some(Done), false), (Some(Blocked), true)]),
            [(Blocked, 1), (Done, 1)],
            "blocked and looked at is still ahead of done"
        );
        assert!(
            hot_by_state([(Some(Working), true), (Some(Done), true)]).is_empty(),
            "nothing waits for you"
        );
    }

    /// The membership and the order are read off [`hot`] and [`tier`], never a list of their
    /// own: every state that can need you is in some tier 0 to 2, and a state the tiers put
    /// ahead of another comes first.
    #[test]
    fn the_split_follows_the_tiers_for_every_state() {
        use AgentState::*;
        let every = [Blocked, Done, Working, Idle, Unknown];
        for seen in [false, true] {
            let split = hot_by_state(every.map(|state| (Some(state), seen)));
            let needing: Vec<AgentState> = every
                .into_iter()
                .filter(|&state| hot(Some(state), seen))
                .collect();
            let mut listed: Vec<AgentState> = split.iter().map(|&(state, _)| state).collect();
            assert!(
                listed.iter().all(|state| needing.contains(state))
                    && needing.iter().all(|state| listed.contains(state)),
                "seen={seen}: {listed:?} against {needing:?}"
            );
            let tiers: Vec<u8> = listed
                .iter()
                .map(|&state| tier(Some(state), seen))
                .collect();
            assert!(tiers.is_sorted(), "seen={seen}: {tiers:?}");
            listed.dedup();
            assert_eq!(listed.len(), split.len(), "each state once");
        }
    }

    // ------------------------------------------------------------------ next up

    /// A pane holding an agent that has reported no state yet: an agent pane on its kind
    /// alone.
    fn stateless_agent(session: &str, window: u32, pane_index: u32) -> PaneInfo {
        let mut p = pane(session, window, pane_index, None, false, 0);
        p.agent.kind = Some("claude".to_string());
        p
    }

    /// Two sessions with every kind of pane in them, in tmux order:
    ///
    /// ```text
    /// ab   1 working   2 idle   3 blocked (looked at)   4 shell
    /// cde  1 done      2 idle   3 working   4 done (looked at)   5 agent, no state yet
    /// ```
    ///
    /// Ids are `%<session name length><window><pane>`: `%231` is ab's third window. Needing
    /// you: `%231` and `%311`. Idle: `%221`, `%321` and `%341`. The rest: `%211`, `%331`
    /// and `%351`.
    fn world() -> (Vec<PaneInfo>, [SessionInfo; 2]) {
        use AgentState::*;
        let panes = vec![
            pane("ab", 1, 1, Some(Working), true, 10),
            pane("ab", 2, 1, Some(Idle), true, 20),
            pane("ab", 3, 1, Some(Blocked), true, 30),
            pane("ab", 4, 1, None, false, 0),
            pane("cde", 1, 1, Some(Done), false, 40),
            pane("cde", 2, 1, Some(Idle), true, 50),
            pane("cde", 3, 1, Some(Working), false, 60),
            pane("cde", 4, 1, Some(Done), true, 70),
            stateless_agent("cde", 5, 1),
        ];
        (panes, [session("ab", 0), session("cde", 1)])
    }

    /// Gives the pane `id` a state, as a watcher would.
    fn set(panes: &mut [PaneInfo], id: &str, state: Option<AgentState>, seen: bool) {
        let p = panes
            .iter_mut()
            .find(|p| p.pane_id == id)
            .unwrap_or_else(|| panic!("no pane {id}"));
        p.agent.state = state;
        p.agent.seen = seen;
    }

    /// The panes `n` presses of `prefix+O` visit, each press starting after the pane the one
    /// before landed on, the first after `start`; and the group each walked.
    fn presses<'a>(
        panes: &'a [PaneInfo],
        sessions: &[SessionInfo],
        start: Option<&str>,
        n: usize,
    ) -> Vec<(&'a str, Group)> {
        let mut at = start.map(str::to_string);
        let mut out = Vec::new();
        for _ in 0..n {
            let Some((pane, group)) = next_up(panes, sessions, at.as_deref()) else {
                break;
            };
            out.push((pane.pane_id.as_str(), group));
            at = Some(pane.pane_id.clone());
        }
        out
    }

    #[test]
    fn every_agent_is_in_one_group_and_the_tiers_say_which() {
        use AgentState::*;
        assert_eq!(Group::of(Some(Blocked), false), Group::NeedsYou);
        assert_eq!(
            Group::of(Some(Blocked), true),
            Group::NeedsYou,
            "blocked keeps needing you, looked at or not"
        );
        assert_eq!(Group::of(Some(Done), false), Group::NeedsYou);
        assert_eq!(
            Group::of(Some(Done), true),
            Group::Idle,
            "a finished agent you have looked at is idle"
        );
        assert_eq!(Group::of(Some(Idle), false), Group::Idle);
        assert_eq!(Group::of(Some(Idle), true), Group::Idle);
        assert_eq!(Group::of(Some(Working), false), Group::Rest);
        assert_eq!(Group::of(Some(Working), true), Group::Rest);
        assert_eq!(Group::of(Some(Unknown), true), Group::Rest);
        assert_eq!(Group::of(None, false), Group::Rest, "no state reported yet");
        // Read off the tiers for every state, never a list of their own: the first group
        // is exactly what `hot` says needs you, the second exactly the idle tier.
        for seen in [false, true] {
            for state in [
                Some(Blocked),
                Some(Done),
                Some(Idle),
                Some(Working),
                Some(Unknown),
                None,
            ] {
                let group = Group::of(state, seen);
                assert_eq!(
                    group == Group::NeedsYou,
                    hot(state, seen),
                    "{state:?} {seen}"
                );
                assert_eq!(
                    group == Group::Idle,
                    tier(state, seen) == IDLE_TIER,
                    "{state:?} {seen}"
                );
            }
        }
    }

    #[test]
    fn the_groups_are_most_urgent_first_and_named_for_the_json() {
        assert!(Group::NeedsYou < Group::Idle && Group::Idle < Group::Rest);
        for (group, name) in [
            (Group::NeedsYou, "needs_you"),
            (Group::Idle, "idle"),
            (Group::Rest, "rest"),
        ] {
            assert_eq!(serde_json::to_value(group).unwrap(), name);
        }
    }

    #[test]
    fn next_up_walks_only_what_needs_you_while_anything_does() {
        use Group::NeedsYou;
        let (panes, sessions) = world();
        // Idle and working agents sit between and after the two that need you, and are
        // never a stop.
        assert_eq!(
            presses(&panes, &sessions, None, 5),
            [
                ("%231", NeedsYou),
                ("%311", NeedsYou),
                ("%231", NeedsYou),
                ("%311", NeedsYou),
                ("%231", NeedsYou),
            ],
            "one step a press, wrapping"
        );
        // Starting from a pane that is not in the walk: the next member after it, in tmux
        // order, whatever kind of pane it is.
        for (start, first) in [
            ("%211", "%231"),
            ("%221", "%231"),
            ("%241", "%311"),
            ("%321", "%231"),
            ("%351", "%231"),
        ] {
            assert_eq!(
                presses(&panes, &sessions, Some(start), 1),
                [(first, NeedsYou)],
                "after {start}"
            );
        }
    }

    /// With nothing that needs you, next up walks every agent, not the idle ones alone: idle
    /// and working alike, and one that has reported no state yet, in tmux order, wrapping; the shell is none of them. Each press reports
    /// the group of the pane it went to.
    #[test]
    fn with_nothing_that_needs_you_next_up_walks_every_agent() {
        use AgentState::*;
        use Group::{Idle, Rest};
        let (mut panes, sessions) = world();
        set(&mut panes, "%231", Some(Working), true);
        // A finished agent you have looked at is idle.
        set(&mut panes, "%311", Some(Done), true);
        assert_eq!(
            presses(&panes, &sessions, None, 9),
            [
                ("%211", Rest),
                ("%221", Idle),
                ("%231", Rest),
                ("%311", Idle),
                ("%321", Idle),
                ("%331", Rest),
                ("%341", Idle),
                ("%351", Rest),
                ("%211", Rest),
            ],
            "every agent, in tmux order, and round"
        );
        assert_eq!(
            presses(&panes, &sessions, Some("%241"), 1),
            [("%311", Idle)],
            "from the shell, the next agent after it"
        );
    }

    /// The sweep holds as states change under it: an idle agent that starts working is still
    /// a stop, one that blocks takes the walk to itself, and once it is answered the sweep
    /// goes on after it.
    #[test]
    fn the_sweep_of_every_agent_holds_as_states_change_under_it() {
        use AgentState::*;
        use Group::{Idle, NeedsYou, Rest};
        let (mut panes, sessions) = world();
        set(&mut panes, "%231", Some(Working), true);
        set(&mut panes, "%311", Some(Working), true);
        assert_eq!(
            presses(&panes, &sessions, Some("%311"), 1),
            [("%321", Idle)]
        );
        set(&mut panes, "%321", Some(Working), true);
        assert_eq!(
            presses(&panes, &sessions, Some("%311"), 1),
            [("%321", Rest)],
            "working now, and still a stop"
        );
        set(&mut panes, "%331", Some(Blocked), false);
        assert_eq!(
            presses(&panes, &sessions, Some("%321"), 2),
            [("%331", NeedsYou), ("%331", NeedsYou)],
            "a blocked agent takes the walk"
        );
        set(&mut panes, "%331", Some(Working), true);
        assert_eq!(
            presses(&panes, &sessions, Some("%331"), 1),
            [("%341", Idle)],
            "answered, the sweep goes on after it"
        );
    }

    #[test]
    fn with_no_idle_agent_either_next_up_walks_the_rest() {
        use AgentState::*;
        use Group::Rest;
        let (mut panes, sessions) = world();
        for id in ["%231", "%311", "%221", "%321", "%341"] {
            set(&mut panes, id, Some(Working), true);
        }
        set(&mut panes, "%331", Some(Unknown), false);
        let ids: Vec<&str> = presses(&panes, &sessions, None, 9)
            .into_iter()
            .map(|(id, group)| {
                assert_eq!(group, Rest);
                id
            })
            .collect();
        assert_eq!(
            ids,
            [
                "%211", "%221", "%231", "%311", "%321", "%331", "%341", "%351", "%211"
            ],
            "every agent, working, unknown and no state alike, and the shell is none of them"
        );
    }

    #[test]
    fn one_member_next_up_stays_on_it() {
        use Group::NeedsYou;
        let (mut panes, sessions) = world();
        set(&mut panes, "%311", Some(AgentState::Working), true);
        assert_eq!(
            presses(&panes, &sessions, Some("%231"), 3),
            [("%231", NeedsYou), ("%231", NeedsYou), ("%231", NeedsYou)],
            "the only agent that needs you, pressed again"
        );
        assert_eq!(
            presses(&panes, &sessions, Some("%321"), 1),
            [("%231", NeedsYou)]
        );
    }

    #[test]
    fn from_a_pane_tmux_no_longer_has_next_up_starts_at_the_first() {
        let (panes, sessions) = world();
        assert_eq!(
            next_up(&panes, &sessions, Some("%999")).map(|(p, _)| p.pane_id.as_str()),
            Some("%231")
        );
        assert_eq!(
            next_up(&panes, &sessions, None).map(|(p, _)| p.pane_id.as_str()),
            Some("%231")
        );
    }

    #[test]
    fn next_up_has_nowhere_to_go_without_an_agent() {
        let (_, sessions) = world();
        assert!(next_up(&[], &sessions, None).is_none());
        let shells = [
            pane("ab", 1, 1, None, false, 0),
            pane("cde", 1, 1, None, false, 0),
        ];
        assert!(next_up(&shells, &sessions, Some("%211")).is_none());
    }

    /// What needs you is the walk while anything does, so an agent that is looked at, or
    /// answered, is out of it; once nothing needs you every agent is in it: the same panes,
    /// one change at a time.
    #[test]
    fn next_up_moves_down_a_group_as_agents_are_looked_at_and_answered() {
        use AgentState::*;
        let (mut panes, sessions) = world();
        let group = |panes: &[PaneInfo]| {
            next_up(panes, &sessions, None).map(|(p, group)| (p.pane_id.clone(), group))
        };
        assert_eq!(group(&panes), Some(("%231".to_string(), Group::NeedsYou)));

        // Blocked stays blocked when looked at; only a finished agent is settled by it.
        set(&mut panes, "%231", Some(Blocked), false);
        set(&mut panes, "%231", Some(Blocked), true);
        assert_eq!(group(&panes), Some(("%231".to_string(), Group::NeedsYou)));
        set(&mut panes, "%231", Some(Working), true); // answered
        assert_eq!(group(&panes), Some(("%311".to_string(), Group::NeedsYou)));
        set(&mut panes, "%311", Some(Done), true); // looked at
        assert_eq!(
            group(&panes),
            Some(("%211".to_string(), Group::Rest)),
            "nothing needs you: every agent, the first a working one"
        );
        for id in ["%221", "%311", "%321", "%341"] {
            set(&mut panes, id, Some(Working), true);
        }
        assert_eq!(group(&panes), Some(("%211".to_string(), Group::Rest)));
        for p in &mut panes {
            p.agent = Default::default();
        }
        assert_eq!(group(&panes), None, "no agent is left to go to");
    }

    /// Within the group the order is tmux's (session, window, pane), never recency or tier:
    /// a blocked agent nobody has looked at, newer and in a later session, is a later stop.
    #[test]
    fn within_the_group_next_up_is_in_tmux_order_not_attention_order() {
        let sessions = [session("ab", 0), session("cde", 1)];
        let seen_blocked = pane("ab", 3, 1, Some(AgentState::Blocked), true, 100);
        let unseen_blocked = pane("cde", 1, 1, Some(AgentState::Blocked), false, 900);
        let done = pane("ab", 4, 1, Some(AgentState::Done), false, 500);
        let panes = [unseen_blocked.clone(), done.clone(), seen_blocked.clone()];
        let ids: Vec<&str> = presses(&panes, &sessions, None, 3)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            ids,
            [
                seen_blocked.pane_id.as_str(),
                done.pane_id.as_str(),
                unseen_blocked.pane_id.as_str()
            ]
        );
    }
}
