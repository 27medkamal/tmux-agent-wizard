// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Agent detection: a port of herdr's TOML manifest engine.
//!
//! The engine is pure — it never talks to tmux. Callers hand it a [`Snapshot`] of
//! the pane (screen text plus the OSC-derived title and progress strings) and a
//! [`LoadedManifest`], and get an [`Evaluation`] back.
//!
//! * [`engine`] — manifest parsing, rule compilation and evaluation.
//! * [`regions`] — the region helpers rules select their text with.
//! * [`identity`] — which agent (if any) a pane's command names.
//! * [`job`]: which agent (if any) the processes in front of a pane are, by their names and arguments.
//! * [`container`]: which agent (if any) a container runtime in front of a pane runs.
//! * [`loader`] — override → remote → bundled manifest precedence.
//! * [`supplements`] — taw's own additive rules, appended after that precedence.
//! * [`update`] — fetching newer manifests from herdr.dev.

pub mod container;
pub mod engine;
pub mod identity;
pub mod job;
pub mod loader;
pub mod regions;
pub mod supplements;
pub mod update;

pub use container::{
    ContainerAgent, identify_argv as identify_container_argv, identify_ps as identify_container_ps,
    runtime as container_runtime,
};
pub use engine::{
    EvaluatedRule, Evaluation, LoadedManifest, ManifestError, ManifestSource, MatchedRule,
    RuleEvidence, RuleSource, evaluate, parse_manifest,
};
pub use identity::{bundled_ids, executable, identify};
pub use loader::{ManifestStore, ManifestSummary};
pub use regions::region_text;
pub use update::{UpdateOutcome, UpdateStatus, update_all, update_if_stale};

/// Manifest engine version this build implements. Manifests declaring a
/// `min_engine_version` above this are rejected. Matches herdr's constant of the
/// same name so vendored rule files behave identically.
pub const MANIFEST_ENGINE_VERSION: u32 = 3;

/// Input to the detection engine.
///
/// `osc_title` is the raw terminal title (tmux `#{pane_title}`); `osc_progress` is
/// the raw OSC `9;4` payload formatted `4;<state>[;<pct>]`. Both are empty strings
/// when unknown, which makes the OSC regions match nothing — exactly the pre-OSC
/// behaviour.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub content: &'a str,
    pub osc_title: &'a str,
    pub osc_progress: &'a str,
}

/// The state an agent is in — shared with the tmux store (`@agent_state`).
///
/// `Done` has no manifest rule behind it: the watcher promotes a background
/// `working → idle` transition to `done` until the user looks at the pane.
pub use crate::store::AgentState;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_round_trips_through_its_lowercase_label() {
        for state in [
            AgentState::Idle,
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Unknown,
        ] {
            assert_eq!(state.to_string().parse::<AgentState>().unwrap(), state);
        }
        assert!("busy".parse::<AgentState>().is_err());
        assert!("Idle".parse::<AgentState>().is_err());
    }
}
