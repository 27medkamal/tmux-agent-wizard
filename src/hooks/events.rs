//! The names a `[hooks]` key may take, and when each one fires.
//!
//! Four kinds share the table. The lifecycle events are the 19 taw already fires at
//! herdr-style plugins (`third_party/herdr/reference/event-names.txt`, emitted through
//! `plugin::events::emit`); the `agent.*` events are the same moments as
//! `pane.agent_status_changed` into one state; the `sandbox.*` events are the moments the
//! watcher sees a pane's agent come to run in a container and the container leave the pane;
//! `notification` is the moment taw shows a toast. Like `agent.*`, the `sandbox.*` events are
//! hooks and not plugin events. The two vetoes, `pre_close` and `pre_worktree_remove`, are not
//! events at all: taw waits for them and a non-zero exit refuses.

use serde_json::Value;

/// One name a `[hooks]` key may take, and when it fires in a person's words: what
/// `taw hooks list` prints and what the README table says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub name: &'static str,
    pub when: &'static str,
    /// Whether the event is about something going away: a close, a removal or a process ending.
    /// A hook hooked to it may close, remove or respawn what it is told of (the hook for
    /// `pane.exited` is the one that brings the pane back), so `taw hooks test <event> --pane`
    /// does not run it on a real pane unless it is told `--force`. Every entry says, so that an
    /// event added to the table cannot be left out of the question.
    pub going_away: bool,
}

/// Every event, in the order the README lists them: the lifecycle events by what they are
/// about, then the agent states, then a container coming and going, then the notification.
pub const EVENTS: &[Event] = &[
    Event {
        name: "workspace.created",
        when: "a tmux session is created, by taw or by hand",
        going_away: false,
    },
    Event {
        name: "workspace.closed",
        when: "a tmux session is closed",
        going_away: true,
    },
    Event {
        name: "workspace.focused",
        when: "a client switches to another session",
        going_away: false,
    },
    Event {
        name: "workspace.renamed",
        when: "a session is renamed",
        going_away: false,
    },
    Event {
        name: "workspace.updated",
        when: "a session gains worktree details, or has its metadata reported",
        going_away: false,
    },
    Event {
        name: "tab.created",
        when: "a window is created",
        going_away: false,
    },
    Event {
        name: "tab.closed",
        when: "a window is closed",
        going_away: true,
    },
    Event {
        name: "tab.focused",
        when: "a session shows another window",
        going_away: false,
    },
    Event {
        name: "tab.renamed",
        when: "a window is renamed",
        going_away: false,
    },
    Event {
        name: "pane.created",
        when: "a pane is split off",
        going_away: false,
    },
    Event {
        name: "pane.closed",
        when: "a pane is closed",
        going_away: true,
    },
    Event {
        name: "pane.exited",
        when: "the process in a pane ends and the pane stays",
        going_away: true,
    },
    Event {
        name: "pane.focused",
        when: "another pane is selected",
        going_away: false,
    },
    Event {
        name: "pane.updated",
        when: "a pane is renamed, or has its metadata reported",
        going_away: false,
    },
    Event {
        name: "layout.updated",
        when: "a window's layout changes, which includes every step of a resize",
        going_away: false,
    },
    Event {
        name: "worktree.created",
        when: "taw creates a worktree",
        going_away: false,
    },
    Event {
        name: "worktree.opened",
        when: "taw opens a worktree's session",
        going_away: false,
    },
    Event {
        name: "worktree.removed",
        when: "taw removes a worktree",
        going_away: true,
    },
    Event {
        name: "pane.agent_status_changed",
        when: "an agent changes state, or a pane gains or loses one",
        going_away: false,
    },
    Event {
        name: "agent.blocked",
        when: "an agent becomes blocked and is waiting for you",
        going_away: false,
    },
    Event {
        name: "agent.working",
        when: "an agent starts working",
        going_away: false,
    },
    Event {
        name: "agent.idle",
        when: "an agent goes idle, at its prompt",
        going_away: false,
    },
    Event {
        name: "agent.done",
        when: "an agent finishes while you are not looking",
        going_away: false,
    },
    Event {
        name: SANDBOX_STARTED,
        when: "a pane's agent is seen running in a container, the moment its marker appears",
        going_away: false,
    },
    Event {
        name: SANDBOX_STOPPED,
        when: "the container has left the pane, the moment its marker goes",
        going_away: false,
    },
    Event {
        name: NOTIFICATION,
        when: "taw shows a toast: a request or a done, after its delay, and only while you are not looking at that pane",
        going_away: false,
    },
];

/// The two hooks taw waits for, which can say no.
pub const VETOES: &[Event] = &[
    Event {
        name: PRE_CLOSE,
        when: "before taw closes a session, a window or a pane (the popup's x, workspace close, tab close, pane close); a non-zero exit refuses",
        going_away: true,
    },
    Event {
        name: PRE_WORKTREE_REMOVE,
        when: "before taw removes a worktree (the popup's x on a closed worktree, worktree remove); a non-zero exit refuses",
        going_away: true,
    },
];

pub const PRE_CLOSE: &str = "pre_close";
pub const PRE_WORKTREE_REMOVE: &str = "pre_worktree_remove";
pub const NOTIFICATION: &str = "notification";
pub const STATUS_CHANGED: &str = "pane.agent_status_changed";
pub const SANDBOX_STARTED: &str = "sandbox.started";
pub const SANDBOX_STOPPED: &str = "sandbox.stopped";

/// Whether `name` is a key `[hooks]` gives a meaning to.
pub fn is_known(name: &str) -> bool {
    EVENTS.iter().chain(VETOES).any(|event| event.name == name)
}

/// Whether `name` is an event about something going away ([`Event::going_away`]). A name that
/// is not an event is not.
pub fn goes_away(name: &str) -> bool {
    EVENTS
        .iter()
        .chain(VETOES)
        .any(|event| event.name == name && event.going_away)
}

/// The four states an `agent.*` event is named for.
pub const AGENT_STATES: [&str; 4] = ["blocked", "done", "idle", "working"];

/// The `agent.<state>` event a `pane.agent_status_changed` payload is also, if it is one:
/// the same moment, seen as "an agent went into this state". Only a change of state is one.
/// A payload whose occupant changed while its state did not (`kind_changed`), one where the
/// agent left (no state), one into `unknown`, and one whose state did not change are the
/// status event and nothing more.
pub fn agent_event(payload: &Value) -> Option<&'static str> {
    if payload.get("kind_changed").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let state = payload.get("agent_status")?.as_str()?;
    if payload.get("previous_status").and_then(Value::as_str) == Some(state) {
        return None;
    }
    AGENT_STATES
        .iter()
        .position(|candidate| *candidate == state)
        .map(|at| AGENT_EVENTS[at])
}

/// [`AGENT_STATES`] as event names, in the same order.
pub const AGENT_EVENTS: [&str; 4] = ["agent.blocked", "agent.done", "agent.idle", "agent.working"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lifecycle_event_taw_fires_at_plugins_can_be_hooked() {
        for name in crate::cli::events::NAMES {
            assert!(
                EVENTS.iter().any(|event| event.name == *name),
                "{name} is fired at plugins but has no [hooks] entry"
            );
        }
    }

    #[test]
    fn the_four_agent_states_and_the_notification_are_events() {
        for name in [
            "agent.blocked",
            "agent.done",
            "agent.idle",
            "agent.working",
            NOTIFICATION,
        ] {
            assert!(is_known(name), "{name}");
        }
        // `unknown` is a state taw records but not one worth a hook, and an event name is
        // not a state name.
        assert!(!is_known("agent.unknown"));
        assert!(!is_known("done"));
    }

    /// A `pane.agent_status_changed` payload, as the watcher emits it.
    fn status(from: Option<&str>, to: Option<&str>, kind_changed: bool) -> Value {
        serde_json::json!({
            "pane_id": "w1:p2",
            "workspace_id": "w1",
            "agent": "claude",
            "agent_status": to,
            "previous_status": from,
            "kind_changed": kind_changed,
        })
    }

    #[test]
    fn a_change_into_each_of_the_four_states_is_that_agent_event() {
        for (state, event) in [
            ("blocked", "agent.blocked"),
            ("done", "agent.done"),
            ("idle", "agent.idle"),
            ("working", "agent.working"),
        ] {
            assert_eq!(
                agent_event(&status(Some("unknown"), Some(state), false)),
                Some(event),
                "{state}"
            );
            assert!(is_known(event), "{event}");
        }
        // The first state an agent is seen in is a change into it.
        assert_eq!(
            agent_event(&status(None, Some("working"), false)),
            Some("agent.working")
        );
    }

    #[test]
    fn the_events_that_are_states_are_the_ones_an_agent_can_be_in_and_a_hook_is_worth_having_for() {
        assert_eq!(AGENT_STATES.len(), AGENT_EVENTS.len());
        for (state, event) in AGENT_STATES.iter().zip(AGENT_EVENTS) {
            assert_eq!(event, format!("agent.{state}"));
        }
    }

    #[test]
    fn a_change_into_unknown_or_out_of_an_agent_altogether_is_no_agent_event() {
        assert_eq!(
            agent_event(&status(Some("working"), Some("unknown"), false)),
            None
        );
        // The agent left the pane: the status event says so, with no state.
        assert_eq!(agent_event(&status(Some("working"), None, false)), None);
    }

    /// An agent that only its launch hint names is read as `unknown` for a moment now and then.
    /// Going there fires the status event alone, and coming back is a change into a state like
    /// any other, so it fires that state's agent event again.
    #[test]
    fn an_agent_read_as_unknown_for_a_moment_fires_its_state_again_on_the_way_back() {
        assert_eq!(
            agent_event(&status(Some("working"), Some("unknown"), false)),
            None
        );
        assert_eq!(
            agent_event(&status(Some("unknown"), Some("working"), false)),
            Some("agent.working")
        );
        assert_eq!(
            agent_event(&status(Some("unknown"), Some("blocked"), false)),
            Some("agent.blocked")
        );
    }

    #[test]
    fn a_new_occupant_is_not_a_change_of_state() {
        // `kind_changed`: the state is what it was, and the pane has another agent in it.
        assert_eq!(agent_event(&status(Some("idle"), Some("idle"), true)), None);
        assert_eq!(
            agent_event(&status(Some("working"), Some("done"), true)),
            None
        );
    }

    #[test]
    fn a_state_that_did_not_change_is_no_agent_event() {
        assert_eq!(
            agent_event(&status(Some("done"), Some("done"), false)),
            None
        );
    }

    #[test]
    fn a_payload_with_no_status_at_all_is_no_agent_event() {
        assert_eq!(agent_event(&serde_json::json!({})), None);
        assert_eq!(agent_event(&serde_json::json!({"agent_status": 3})), None);
    }

    /// The events about something going away are marked in the table, and the mark is held to
    /// two other things the table says: what the names say (a `.closed`, a `.removed`, a
    /// `.exited`, a veto) and what a person reads of when each fires ("closed", "closes",
    /// "removes", "ends"). A close, a removal or an ending added to the table with the usual name
    /// or words, and not marked, fails this.
    #[test]
    fn what_is_going_away_is_marked_and_the_mark_agrees_with_the_names_and_the_words() {
        let mut marked = Vec::new();
        for event in EVENTS.iter().chain(VETOES) {
            let by_name = event.name.ends_with(".closed")
                || event.name.ends_with(".removed")
                || event.name.ends_with(".exited")
                || event.name.starts_with("pre_");
            let by_words = event.when.contains("clos")
                || event.when.contains("remov")
                || event.when.contains(" ends");
            assert_eq!(event.going_away, by_name, "{}: by its name", event.name);
            assert_eq!(event.going_away, by_words, "{}: by its words", event.name);
            assert_eq!(goes_away(event.name), event.going_away, "{}", event.name);
            if event.going_away {
                marked.push(event.name);
            }
        }
        // Not vacuous: the closes, the removals and the ending are there.
        for name in [
            "workspace.closed",
            "tab.closed",
            "pane.closed",
            "pane.exited",
            "worktree.removed",
            PRE_CLOSE,
            PRE_WORKTREE_REMOVE,
        ] {
            assert!(marked.contains(&name), "{name} is not marked: {marked:?}");
        }
        // A name that is not an event is not one going away.
        assert!(!goes_away("agent.finished"));
        assert!(!goes_away(""));
    }

    #[test]
    fn the_two_sandbox_events_are_hooks_and_say_when_they_fire_in_the_specs_words() {
        for (name, when) in [
            (
                "sandbox.started",
                "a pane's agent is seen running in a container, the moment its marker appears",
            ),
            (
                "sandbox.stopped",
                "the container has left the pane, the moment its marker goes",
            ),
        ] {
            let event = EVENTS
                .iter()
                .find(|event| event.name == name)
                .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(event.when, when);
            assert!(!event.going_away, "{name}");
            assert!(is_known(name), "{name}");
            assert!(
                !crate::cli::events::NAMES.contains(&name),
                "{name}: a hook, not a plugin event"
            );
        }
        assert_eq!(SANDBOX_STARTED, "sandbox.started");
        assert_eq!(SANDBOX_STOPPED, "sandbox.stopped");
    }

    #[test]
    fn the_two_vetoes_are_known_keys_and_not_events() {
        assert!(is_known(PRE_CLOSE));
        assert!(is_known(PRE_WORKTREE_REMOVE));
        assert!(!EVENTS.iter().any(|event| event.name == PRE_CLOSE));
        assert_eq!(VETOES.len(), 2);
    }

    #[test]
    fn names_are_unique_and_each_says_when_it_fires() {
        let mut seen = std::collections::HashSet::new();
        for event in EVENTS.iter().chain(VETOES) {
            assert!(seen.insert(event.name), "{} is listed twice", event.name);
            assert!(!event.when.trim().is_empty(), "{} has no text", event.name);
        }
    }
}
