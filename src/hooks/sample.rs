//! The sample events `taw hooks test` fires: one for each name a `[hooks]` key may take,
//! shaped like the real payload, so that a hook is tried against what it will be given.
//!
//! The story every sample tells is one session, `taw-hooks-test`, with a window `editor` and a
//! pane running Claude, whose title is `Fix the flaky auth test`. The records are built by the
//! same functions the real events use, from panes tmux might list, so a change to a record's
//! shape reaches the samples with it. The directories are ones taw makes for the purpose,
//! empty, so that a hook that changes into `$TAW_DIR` works and one that tidies up there breaks
//! nothing.
//!
//! **No id in a sample belongs to a real server.** `taw hooks test` runs a person's hooks for
//! real, and the README shows hooks that act on the ids they are given (`tmux send-keys -t
//! "$TAW_PANE"`). Ids a server has would make a test type into somebody's pane or rename their
//! session. tmux counts its ids up from 0 and no server has made a million of anything, so the
//! story's ids are the last ones below that, and a hook that acts on them finds nothing.
//!
//! The one way in is the person's own: `taw hooks test <event> --pane <pane>` tells the same
//! events about a pane they name, with its real ids, session, window and directory ([`Story::of`]).
//! What they are about is the pane's; what happened (an agent finishing, a toast's words, a
//! worktree's branch) is the sample's either way.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::context::{self, Built};
use crate::notify::{Class, Notification, NotifiedAgent, SoundChoice};
use crate::plugin::context::{pane_json, tab_json, workspace_json};
use crate::session::worktree::WorktreeRecord;
use crate::store::{AgentRecord, AgentState};
use crate::tmux::{PaneInfo, SessionInfo, WindowInfo, ids};

/// The story's session, window and pane, by tmux's own ids.
pub const SESSION_ID: &str = "$999999";
pub const WINDOW_ID: &str = "@999999";
pub const PANE_ID: &str = "%999999";
/// The session of the sample worktree, another one.
const WORKTREE_SESSION_ID: &str = "$999998";
/// The story's session, and the one of the sample worktree, which is named for it.
pub const SESSION_NAME: &str = "taw-hooks-test";
const WORKTREE_SESSION_NAME: &str = "taw-hooks-test--feat-x";

/// What `taw hooks list` and `taw hooks test` say about the sample, in the words the README
/// uses: what it names, and how a hook tells a test from a real event.
pub fn note() -> String {
    format!(
        "taw hooks test runs your hooks for real against a sample whose ids name nothing on any \
         tmux server: pane {PANE_ID}, window {WINDOW_ID}, session {SESSION_ID} called \
         {SESSION_NAME}. Every event has all three, even one a real event has none for. A hook \
         that acts on them finds nothing. TAW_SESSION is ={SESSION_NAME}: in a test, the = being \
         tmux's mark for exactly that name and the : saying it is a session's, so a hook that \
         acts on it cannot reach a session of yours whose name only begins the same way. A \
         session called exactly {SESSION_NAME} is still reached. TAW_TEST is 1 in a test and \
         empty otherwise, so a hook that acts on the pane or the session can start with: \
         [ -n \"$TAW_TEST\" ] && exit 0. taw hooks test <event> --pane <pane> uses a real \
         pane's own ids instead, and an event about something going away needs --force with it."
    )
}

/// What a `--pane` test says about itself, in the words the README uses: the ids are a real
/// pane's, so a hook that acts on them acts on it.
pub fn real_note(story: &Story) -> String {
    format!(
        "taw hooks test used the real pane {pane} (window {window}, session {session} called \
         {name}, in {dir}): its ids are your own, so a hook that acts on them acts on that pane. \
         What happened is still the sample's. TAW_TEST is 1 in a test and empty otherwise, so a \
         hook that acts on the pane can start with: [ -n \"$TAW_TEST\" ] && exit 0",
        pane = story.pane.pane_id,
        window = story.window.window_id,
        session = story.session.session_id,
        name = story.session.session_name,
        dir = story.pane.pane_current_path,
    )
}

/// What `taw hooks test <event> --pane <pane>` says, on stderr, when `event` is about something
/// going away ([`crate::hooks::events::goes_away`]) and `--force` was not given: the hook would
/// run on a live session and directory. `pane` is as the person named it.
pub fn forced_message(event: &str, pane: &str) -> String {
    format!(
        "taw hooks test: {event} with --pane runs your hook on the real session and directory \
         of {pane}; add --force if that is what you want"
    )
}

/// A sample: the payload the event is emitted with, and the panes tmux would list, which is
/// what fills in the names the payload leaves out.
pub struct Sample {
    pub payload: Value,
    pub panes: Vec<PaneInfo>,
}

/// Who and where a sample is about: a pane, its window and its session, and the checkout of the
/// worktree the worktree events are about. [`Story::invented`] has ids no server has;
/// [`Story::of`] is a real pane's.
#[derive(Clone, Debug)]
pub struct Story {
    pub session: SessionInfo,
    pub window: WindowInfo,
    pub pane: PaneInfo,
    /// The session the worktree events are about, and the directory of its checkout. The
    /// invented story has a session and a directory of their own for it; a real pane's own
    /// session and directory stand for it, which is what a hook that reads the checkout can
    /// try itself on.
    pub worktree_session: SessionInfo,
    pub checkout: PathBuf,
    /// Whether the pane is a real one.
    pub real: bool,
}

impl Story {
    /// The story with ids no server has, whose directories are under `root` ([`dirs`]).
    pub fn invented(root: &Path) -> Story {
        let (dir, checkout) = dirs(root);
        Story {
            session: session(SESSION_ID, SESSION_NAME, &dir),
            window: window(),
            pane: pane(&dir, AgentState::Working),
            worktree_session: session(WORKTREE_SESSION_ID, WORKTREE_SESSION_NAME, &checkout),
            checkout,
            real: false,
        }
    }

    /// The story of a real pane, with the session and the window tmux lists for it.
    pub fn of(pane: PaneInfo, session: SessionInfo, window: WindowInfo) -> Story {
        Story {
            worktree_session: session.clone(),
            checkout: PathBuf::from(&pane.pane_current_path),
            session,
            window,
            pane,
            real: true,
        }
    }

    /// The pane as an event about an agent going into `state` finds it: the agent is the story's
    /// (Claude, in `state`) whatever the pane holds, and everything else about it is the pane's.
    fn pane_in(&self, state: AgentState) -> PaneInfo {
        let mut pane = self.pane.clone();
        pane.agent = AgentRecord {
            kind: Some("claude".to_string()),
            state: Some(state),
            ..AgentRecord::default()
        };
        pane
    }
}

/// The sample session's directory and the sample worktree's, under `root`.
pub fn dirs(root: &Path) -> (PathBuf, PathBuf) {
    (root.join(SESSION_NAME), root.join(WORKTREE_SESSION_NAME))
}

/// The sample for `event`, or `None` for a name that is not one.
pub fn sample(event: &str, root: &Path) -> Option<Sample> {
    sample_of(event, &Story::invented(root))
}

/// The sample for `event` about `story`, or `None` for a name that is not an event.
pub fn sample_of(event: &str, story: &Story) -> Option<Sample> {
    let (from, to) = match event {
        "pane.agent_status_changed" | "agent.blocked" => ("working", "blocked"),
        "agent.working" => ("idle", "working"),
        "agent.idle" => ("working", "idle"),
        "agent.done" => ("working", "done"),
        _ => ("working", "working"),
    };
    let dir = PathBuf::from(&story.session.session_path);
    let pane = story.pane_in(to.parse::<AgentState>().expect("a sample state is a state"));
    let panes = vec![pane.clone()];
    let workspace = workspace_json(&story.session, &panes);
    let tab = tab_json(&story.window, &panes);
    let pane_record = pane_json(&pane);
    let (workspace_id, tab_id, pane_id) = (
        ids::workspace_id(&story.session.session_id),
        ids::tab_id(&story.session.session_id, &story.window.window_id),
        ids::pane_id(&story.session.session_id, &pane.pane_id),
    );
    let record = WorktreeRecord {
        workspace_id: ids::workspace_id(&story.worktree_session.session_id),
        session_name: story.worktree_session.session_name.clone(),
        path: story.checkout.clone(),
        branch: "feat/x".to_string(),
        base: Some("main".to_string()),
        repo_root: dir.clone(),
        source_workspace_id: workspace_id.clone(),
        created_unix_ms: 1_790_000_000_000,
        machine: None,
        real_path: None,
        real_repo_root: None,
    };
    let worktree_workspace = if story.real {
        workspace.clone()
    } else {
        workspace_json(&story.worktree_session, &[])
    };
    let payload = match event {
        "workspace.created" => json!({ "workspace": workspace, "tab": tab, "pane": pane_record }),
        "workspace.closed" | "workspace.focused" => json!({ "workspace_id": workspace_id }),
        "workspace.renamed" => {
            json!({ "workspace_id": workspace_id, "label": story.session.session_name })
        }
        "workspace.updated" => json!({ "workspace": workspace }),
        "tab.created" => json!({ "tab": tab, "pane": pane_record }),
        "tab.closed" | "tab.focused" => {
            json!({ "tab_id": tab_id, "workspace_id": workspace_id })
        }
        "tab.renamed" => {
            json!({ "tab_id": tab_id, "workspace_id": workspace_id, "label": story.window.window_name })
        }
        "pane.created" | "pane.updated" => json!({ "pane": pane_record }),
        "pane.closed" | "pane.exited" | "pane.focused" => {
            json!({ "pane_id": pane_id, "workspace_id": workspace_id })
        }
        "layout.updated" => json!({ "layout": {
            "workspace_id": workspace_id,
            "tab_id": tab_id,
            "zoomed": false,
            "area": { "x": 0, "y": 0, "width": pane.pane_width, "height": pane.pane_height },
            "focused_pane_id": pane_id,
            "panes": [{
                "pane_id": pane_id, "x": 0, "y": 0,
                "width": pane.pane_width, "height": pane.pane_height,
            }],
            "splits": [],
        }}),
        "worktree.created" | "worktree.opened" => {
            json!({ "workspace": worktree_workspace, "worktree": record })
        }
        "worktree.removed" => {
            json!({ "workspace_id": record.workspace_id, "worktree": record, "forced": false })
        }
        "pane.agent_status_changed"
        | "agent.blocked"
        | "agent.working"
        | "agent.idle"
        | "agent.done" => json!({
            "pane_id": pane_id,
            "workspace_id": workspace_id,
            "agent": "claude",
            "agent_status": to,
            "previous_status": from,
            "kind_changed": false,
            "pane": pane_record,
        }),
        // Where the agent runs: what came, or what left, and the pane as the event finds it, with
        // the container once it has come and with none once it has gone.
        "sandbox.started" | "sandbox.stopped" => {
            let mut record = pane.clone();
            if event == "sandbox.started" {
                record.agent.container = Some("docker".to_string());
                record.agent.sandbox = Some("default".to_string());
            }
            json!({
                "pane_id": pane_id,
                "workspace_id": workspace_id,
                "agent": "claude",
                "sandbox": { "profile": "default", "runtime": "docker" },
                "pane": pane_json(&record),
            })
        }
        "notification" => {
            let notification = Notification {
                class: Class::Request,
                title: format!(
                    "claude blocked \u{b7} {}:{}",
                    story.session.session_name, story.window.window_index
                ),
                body: "needs input".to_string(),
                pane_id: Some(pane.pane_id.clone()),
                sound: SoundChoice::None,
                agent: NotifiedAgent {
                    label: "claude".to_string(),
                    kind: "claude".to_string(),
                },
            };
            json!({
                "kind": crate::notify::kind_word(notification.class),
                "message": crate::notify::plain_text(&notification),
                "title": notification.title,
                "body": notification.body,
                "pane_id": pane_id,
                "workspace_id": workspace_id,
                "agent": "claude",
                "agent_status": "blocked",
                "pane": pane_record,
            })
        }
        "pre_close" => json!({ "target": "pane", "pane": pane_record }),
        "pre_worktree_remove" => {
            json!({ "workspace_id": record.workspace_id, "worktree": record })
        }
        _ => return None,
    };
    Some(Sample { payload, panes })
}

/// What `taw hooks test` gives a hook for `event` about `story`: the environment and the JSON of
/// the sample ([`sample_of`]), `TAW_TEST` set to `1`, and the story's own session, window and pane
/// ids in each of those three variables the real event leaves empty ([`Built::with_ids`]). The
/// invented story's session is `TAW_SESSION` as an exact session, `=taw-hooks-test:`
/// ([`Built::with_exact_session`]); a real pane's is its own. `None` for a name that is not an
/// event.
///
/// A real `workspace.renamed` has no pane, and a hook may leave it at that. A test has to name one
/// all the same: `tmux send-keys -t "$TAW_PANE"` with nothing in `TAW_PANE` is a command for the
/// current pane, which would be a live one.
pub fn built_of(event: &str, story: &Story) -> Option<Built> {
    let sample = sample_of(event, story)?;
    let built = context::build(event, &sample.payload, &sample.panes)
        .with_ids(
            &story.session.session_id,
            &story.window.window_id,
            &story.pane.pane_id,
        )
        .marked_as_test();
    // The invented session is named as exactly that session, so that a session of the person's
    // whose name begins the same way is out of reach. A real pane's own session is theirs to act
    // on.
    Some(if story.real {
        built
    } else {
        built.with_exact_session()
    })
}

fn session(id: &str, name: &str, dir: &Path) -> SessionInfo {
    SessionInfo {
        session_id: id.to_string(),
        session_name: name.to_string(),
        session_path: dir.display().to_string(),
        session_attached: 1,
        session_windows: 2,
        session_group_size: 0,
        active_window_id: WINDOW_ID.to_string(),
        session_last_attached: 0,
    }
}

fn window() -> WindowInfo {
    WindowInfo {
        window_id: WINDOW_ID.to_string(),
        session_id: SESSION_ID.to_string(),
        window_index: 2,
        window_name: "editor".to_string(),
        window_active: true,
        window_panes: 1,
        window_layout: String::new(),
    }
}

/// The pane the stories are about: Claude in `dir`, in `state`.
fn pane(dir: &Path, state: AgentState) -> PaneInfo {
    PaneInfo {
        pane_id: PANE_ID.to_string(),
        session_id: SESSION_ID.to_string(),
        session_name: SESSION_NAME.to_string(),
        session_path: dir.display().to_string(),
        session_attached: 1,
        window_id: WINDOW_ID.to_string(),
        window_index: 2,
        window_name: "editor".to_string(),
        window_active: true,
        window_layout: String::new(),
        pane_index: 0,
        pane_active: true,
        pane_pid: 4242,
        pane_current_command: "claude".to_string(),
        pane_current_path: dir.display().to_string(),
        pane_title: "Fix the flaky auth test".to_string(),
        pane_width: 180,
        pane_height: 45,
        pane_dead: false,
        alternate_on: false,
        agent: AgentRecord {
            kind: Some("claude".to_string()),
            state: Some(state),
            ..AgentRecord::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::events::{EVENTS, VETOES};

    fn root() -> PathBuf {
        PathBuf::from("/samples")
    }

    fn built(event: &str) -> context::Built {
        built_of(event, &Story::invented(&root()))
            .unwrap_or_else(|| panic!("no sample for {event}"))
    }

    fn var<'a>(built: &'a context::Built, name: &str) -> &'a str {
        built
            .env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("{name} is not set"))
    }

    #[test]
    fn every_event_and_both_vetoes_have_a_sample_and_a_name_that_is_not_one_has_none() {
        for event in EVENTS.iter().chain(VETOES) {
            assert!(sample(event.name, &root()).is_some(), "{}", event.name);
        }
        assert!(sample("agent.finished", &root()).is_none());
        assert!(sample("", &root()).is_none());
    }

    #[test]
    fn each_sample_is_told_its_own_event_by_the_environment_and_the_json() {
        for event in EVENTS.iter().chain(VETOES) {
            let built = built(event.name);

            assert_eq!(var(&built, "TAW_EVENT"), event.name);
            let json: Value = serde_json::from_str(&built.json).unwrap();
            assert_eq!(json["event"], event.name);
        }
    }

    #[test]
    fn every_sample_names_the_session_the_story_is_about() {
        // The events about a session, a window, a pane, an agent or a worktree all say which
        // session, from the panes tmux would list, whatever their payload carries.
        for event in EVENTS.iter().chain(VETOES) {
            let built = built(event.name);
            let session = var(&built, "TAW_SESSION");
            assert!(
                session == "=taw-hooks-test:" || session == "=taw-hooks-test--feat-x:",
                "{}: TAW_SESSION={session:?}",
                event.name
            );
        }
    }

    /// `taw hooks test` runs a person's hooks for real, and the README recommends acting on
    /// the ids a hook is given (`tmux send-keys -t "$TAW_PANE"`). A sample with ids that a
    /// server has would make a test type into somebody's pane, so no id of a sample is one a
    /// server has: tmux counts up from 0, and no server has made a million of anything.
    #[test]
    fn the_ids_of_a_sample_are_ones_no_tmux_server_has() {
        fn collect(value: &Value, ids: &mut Vec<String>) {
            match value {
                Value::String(text) => {
                    let id =
                        regex::Regex::new(r"^(?:[$@%][0-9]+|w[0-9]+(?::[tp][0-9]+)?)$").unwrap();
                    if id.is_match(text) {
                        ids.push(text.clone());
                    }
                }
                Value::Array(items) => items.iter().for_each(|item| collect(item, ids)),
                Value::Object(fields) => fields.values().for_each(|item| collect(item, ids)),
                _ => {}
            }
        }
        let digits = regex::Regex::new(r"[0-9]+").unwrap();
        for event in EVENTS.iter().chain(VETOES) {
            let built = built(event.name);
            let json: Value = serde_json::from_str(&built.json).unwrap();
            let mut ids: Vec<String> = ["TAW_PANE", "TAW_WINDOW", "TAW_SESSION_ID"]
                .iter()
                .map(|name| var(&built, name).to_string())
                .filter(|id| !id.is_empty())
                .collect();
            collect(&json, &mut ids);
            assert!(
                !ids.is_empty(),
                "{}: a sample with no id at all",
                event.name
            );
            for id in &ids {
                for number in digits.find_iter(id) {
                    let number: u64 = number.as_str().parse().unwrap();
                    assert!(
                        number >= 999_990,
                        "{}: {id} is an id a server may have",
                        event.name
                    );
                }
            }
        }
    }

    /// An empty tmux target is the current pane, window or session, so a hook that acts on a
    /// variable an event leaves empty acts on whichever one somebody is looking at: the sample
    /// says nothing of a pane in a `workspace.renamed`, and `tmux send-keys -t "$TAW_PANE"` typed
    /// into a live pane. Every event of a test is told all three ids, the story's own, and the
    /// real events that have none are the ones this catches.
    #[test]
    fn every_event_of_a_test_names_a_pane_a_window_and_a_session_none_of_them_empty() {
        for event in EVENTS.iter().chain(VETOES) {
            let built = built(event.name);

            assert_eq!(var(&built, "TAW_PANE"), "%999999", "{}", event.name);
            assert_eq!(var(&built, "TAW_WINDOW"), "@999999", "{}", event.name);
            // The worktree events are about the worktree's own session, another invented one.
            let session = var(&built, "TAW_SESSION_ID");
            assert!(
                session == "$999999" || session == "$999998",
                "{}: TAW_SESSION_ID={session:?}",
                event.name
            );
        }
    }

    /// tmux reads a session name in a target as the start of a name when no session has exactly
    /// it: `tmux kill-session -t taw-hooks-test` kills `taw-hooks-testing` if that is the session
    /// there is. `=` in front is tmux's mark for exactly this name, and `:` after it says the name
    /// is a session's: a command that takes a window reads a bare `=taw-hooks-test` as a window's
    /// first and then as the start of a session's, so `tmux kill-window -t =taw-hooks-test` still
    /// reached `taw-hooks-testing`. A test names its session `=taw-hooks-test:`, so that a hook
    /// that acts on `$TAW_SESSION` finds no session called that instead of one of yours whose name
    /// begins the same way. Only the environment has the mark: the JSON is the event's own, and it
    /// says the name a real event would.
    #[test]
    fn a_test_names_its_session_by_its_exact_name_and_leaves_the_json_alone() {
        for event in EVENTS.iter().chain(VETOES) {
            let built = built(event.name);

            let session = var(&built, "TAW_SESSION");
            assert!(
                session.starts_with("=taw-hooks-test")
                    && session.ends_with(':')
                    && !session.starts_with("==")
                    && !session.ends_with("::"),
                "{}: TAW_SESSION={session:?}",
                event.name
            );
            assert!(
                !built.json.contains("=taw-hooks-test"),
                "{}: the mark is in the JSON {}",
                event.name,
                built.json
            );
        }
        let json: Value = serde_json::from_str(&built("workspace.updated").json).unwrap();
        assert_eq!(json["workspace"]["label"], "taw-hooks-test");
    }

    /// The same holds for a real pane: the events that have no pane of their own are told the
    /// one the person named, so a hook that acts on it acts on that pane and not on whichever
    /// one their client is looking at.
    #[test]
    fn every_event_about_a_real_pane_names_that_pane_its_window_and_its_session() {
        let story = real_story();
        for event in EVENTS.iter().chain(VETOES) {
            let built = built_about(event.name, &story);

            assert_eq!(var(&built, "TAW_PANE"), "%31", "{}", event.name);
            assert_eq!(var(&built, "TAW_WINDOW"), "@7", "{}", event.name);
            assert_eq!(var(&built, "TAW_SESSION_ID"), "$3", "{}", event.name);
        }
    }

    /// Only what the event left empty is filled: an event that names its own thing keeps it, and
    /// the JSON a hook reads is the event's own, with no id that the real event does not carry
    /// (herdr's context has its flat `tab_id` and the rest as `null` where there is nothing, so a
    /// null is how "no id" reads).
    #[test]
    fn what_an_event_names_is_kept_and_its_json_is_not_touched() {
        let renamed = built("workspace.renamed");
        let json: Value = serde_json::from_str(&renamed.json).unwrap();
        assert!(json.get("pane").is_none_or(Value::is_null), "{json}");
        assert!(json.get("pane_id").is_none_or(Value::is_null), "{json}");
        assert!(json.get("tab_id").is_none_or(Value::is_null), "{json}");
        assert!(
            json.get("focused_pane_id").is_none_or(Value::is_null),
            "{json}"
        );

        let worktree = built("worktree.created");
        assert_eq!(var(&worktree, "TAW_SESSION_ID"), "$999998");
        assert_eq!(var(&built("tab.focused"), "TAW_WINDOW"), "@999999");
    }

    #[test]
    fn the_story_names_its_pane_window_and_session_with_those_ids() {
        let built = built("agent.done");

        assert_eq!(var(&built, "TAW_PANE"), "%999999");
        assert_eq!(var(&built, "TAW_WINDOW"), "@999999");
        assert_eq!(var(&built, "TAW_SESSION_ID"), "$999999");
        assert_eq!(var(&built, "TAW_SESSION"), "=taw-hooks-test:");
    }

    #[test]
    fn an_agent_event_is_a_change_into_its_own_state() {
        for (event, state, previous) in [
            ("agent.blocked", "blocked", "working"),
            ("agent.working", "working", "idle"),
            ("agent.idle", "idle", "working"),
            ("agent.done", "done", "working"),
            ("pane.agent_status_changed", "blocked", "working"),
        ] {
            let built = built(event);

            assert_eq!(var(&built, "TAW_STATE"), state, "{event}");
            assert_eq!(var(&built, "TAW_PREVIOUS_STATE"), previous, "{event}");
            assert_eq!(var(&built, "TAW_AGENT"), "claude", "{event}");
            assert_eq!(var(&built, "TAW_PANE"), "%999999", "{event}");
            assert_eq!(
                var(&built, "TAW_TITLE"),
                "Fix the flaky auth test",
                "{event}"
            );
            assert_eq!(var(&built, "TAW_DIR"), "/samples/taw-hooks-test", "{event}");
        }
    }

    #[test]
    fn the_sample_notification_is_a_request_with_the_text_a_toast_would_have() {
        let built = built("notification");

        assert_eq!(var(&built, "TAW_KIND"), "request");
        assert_eq!(
            var(&built, "TAW_MESSAGE"),
            "claude blocked \u{b7} taw-hooks-test:2 \u{b7} needs input"
        );
        assert_eq!(var(&built, "TAW_STATE"), "blocked");
    }

    #[test]
    fn the_sample_worktree_events_are_about_the_sample_checkout() {
        for event in [
            "worktree.created",
            "worktree.opened",
            "worktree.removed",
            "pre_worktree_remove",
        ] {
            let built = built(event);

            assert_eq!(
                var(&built, "TAW_DIR"),
                "/samples/taw-hooks-test--feat-x",
                "{event}"
            );
            assert_eq!(
                var(&built, "TAW_SESSION"),
                "=taw-hooks-test--feat-x:",
                "{event}"
            );
            let json: Value = serde_json::from_str(&built.json).unwrap();
            assert_eq!(json["worktree"]["branch"], "feat/x", "{event}");
        }
    }

    /// The two sandbox samples say where the agent runs, in the event's own `sandbox` (what came, or
    /// what left), and in the pane's record: the container is there once it has come, and gone from
    /// the pane once it has left.
    #[test]
    fn the_sandbox_samples_say_where_the_agent_runs() {
        let started = built("sandbox.started");
        let json: Value = serde_json::from_str(&started.json).unwrap();
        assert_eq!(
            json["sandbox"],
            json!({"profile": "default", "runtime": "docker"})
        );
        assert_eq!(
            json["pane"]["sandbox"],
            json!({"profile": "default", "runtime": "docker"})
        );
        assert_eq!(json["agent"], "claude");
        let stopped = built("sandbox.stopped");
        let json: Value = serde_json::from_str(&stopped.json).unwrap();
        assert_eq!(
            json["sandbox"],
            json!({"profile": "default", "runtime": "docker"})
        );
        assert_eq!(
            json["pane"]["sandbox"],
            Value::Null,
            "the container has gone from the pane"
        );
        for built in [&started, &stopped] {
            assert_eq!(var(built, "TAW_PANE"), "%999999");
            assert_eq!(var(built, "TAW_AGENT"), "claude");
        }
    }

    #[test]
    fn the_sample_close_says_what_is_being_closed() {
        let built = built("pre_close");

        assert_eq!(var(&built, "TAW_TARGET"), "pane");
        assert_eq!(var(&built, "TAW_PANE"), "%999999");
    }

    #[test]
    fn the_sample_window_and_workspace_events_carry_their_names() {
        assert_eq!(var(&built("tab.renamed"), "TAW_TITLE"), "editor");
        assert_eq!(var(&built("tab.focused"), "TAW_WINDOW"), "@999999");
        assert_eq!(
            var(&built("workspace.renamed"), "TAW_SESSION"),
            "=taw-hooks-test:"
        );
        assert_eq!(var(&built("workspace.closed"), "TAW_SESSION_ID"), "$999999");
    }

    // ---- a real pane's story ----

    /// A pane as tmux lists it on a real server: not the story's, and none of its ids.
    fn real_story() -> Story {
        let dir = "/home/you/api";
        let pane = PaneInfo {
            pane_id: "%31".to_string(),
            session_id: "$3".to_string(),
            session_name: "api".to_string(),
            session_path: dir.to_string(),
            session_attached: 1,
            window_id: "@7".to_string(),
            window_index: 4,
            window_name: "server".to_string(),
            window_active: true,
            window_layout: String::new(),
            pane_index: 1,
            pane_active: true,
            pane_pid: 777,
            pane_current_command: "zsh".to_string(),
            pane_current_path: "/home/you/api/src".to_string(),
            pane_title: "server logs".to_string(),
            pane_width: 120,
            pane_height: 30,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord::default(),
        };
        Story::of(
            pane,
            SessionInfo {
                session_id: "$3".to_string(),
                session_name: "api".to_string(),
                session_path: dir.to_string(),
                session_attached: 1,
                session_windows: 3,
                session_group_size: 0,
                active_window_id: "@7".to_string(),
                session_last_attached: 0,
            },
            WindowInfo {
                window_id: "@7".to_string(),
                session_id: "$3".to_string(),
                window_index: 4,
                window_name: "server".to_string(),
                window_active: true,
                window_panes: 2,
                window_layout: String::new(),
            },
        )
    }

    fn built_about(event: &str, story: &Story) -> context::Built {
        built_of(event, story).unwrap_or_else(|| panic!("no sample for {event}"))
    }

    /// A test told about a real pane is about that pane and nothing invented: every event and
    /// both vetoes say its session, and no id of the story with ids no server has comes near it.
    #[test]
    fn every_event_about_a_real_pane_is_about_that_pane_and_nothing_invented() {
        let story = real_story();
        for event in EVENTS.iter().chain(VETOES) {
            let built = built_about(event.name, &story);

            assert_eq!(var(&built, "TAW_SESSION"), "api", "{}", event.name);
            assert_eq!(var(&built, "TAW_SESSION_ID"), "$3", "{}", event.name);
            assert_eq!(var(&built, "TAW_TEST"), "1", "{}", event.name);
            assert!(
                !built.json.contains("999999"),
                "{}: an invented id in {}",
                event.name,
                built.json
            );
            for (name, value) in &built.env {
                assert!(!value.contains("999999"), "{}: {name}={value}", event.name);
                assert!(!value.contains("taw-hooks-test"), "{}: {name}", event.name);
            }
        }
    }

    #[test]
    fn a_real_panes_ids_directory_and_title_reach_the_events_about_it() {
        let story = real_story();

        for event in ["pane.updated", "pane.focused", "pre_close", "agent.done"] {
            let built = built_about(event, &story);
            assert_eq!(var(&built, "TAW_PANE"), "%31", "{event}");
            assert_eq!(var(&built, "TAW_WINDOW"), "@7", "{event}");
        }
        let built = built_about("pane.updated", &story);
        assert_eq!(var(&built, "TAW_DIR"), "/home/you/api/src");
        assert_eq!(var(&built, "TAW_TITLE"), "server logs");
        // The window and the session are as they are.
        assert_eq!(var(&built_about("tab.focused", &story), "TAW_WINDOW"), "@7");
        assert_eq!(
            var(&built_about("tab.renamed", &story), "TAW_TITLE"),
            "server"
        );
        assert_eq!(
            var(&built_about("workspace.renamed", &story), "TAW_SESSION"),
            "api"
        );
    }

    /// What happened is the sample's however real the pane: an agent event is Claude going into
    /// its state, and a toast says what it would say for that pane's session and window.
    #[test]
    fn what_happened_is_still_the_samples_for_a_real_pane() {
        let story = real_story();

        let done = built_about("agent.done", &story);
        assert_eq!(var(&done, "TAW_AGENT"), "claude");
        assert_eq!(var(&done, "TAW_STATE"), "done");
        assert_eq!(var(&done, "TAW_PREVIOUS_STATE"), "working");
        let toast = built_about("notification", &story);
        assert_eq!(var(&toast, "TAW_KIND"), "request");
        assert_eq!(
            var(&toast, "TAW_MESSAGE"),
            "claude blocked \u{b7} api:4 \u{b7} needs input"
        );
        assert_eq!(var(&toast, "TAW_PANE"), "%31");
    }

    /// The worktree events are about the real pane's own session and directory: a hook that
    /// reads the checkout has a real one to try itself on, and the branch is the sample's.
    #[test]
    fn the_worktree_events_about_a_real_pane_use_its_session_and_directory() {
        let story = real_story();
        for event in [
            "worktree.created",
            "worktree.opened",
            "worktree.removed",
            "pre_worktree_remove",
        ] {
            let built = built_about(event, &story);

            assert_eq!(var(&built, "TAW_DIR"), "/home/you/api/src", "{event}");
            assert_eq!(var(&built, "TAW_SESSION"), "api", "{event}");
            let json: Value = serde_json::from_str(&built.json).unwrap();
            assert_eq!(json["worktree"]["branch"], "feat/x", "{event}");
            assert_eq!(json["worktree"]["path"], "/home/you/api/src", "{event}");
            assert_eq!(json["worktree"]["workspace_id"], "w3", "{event}");
        }
    }

    #[test]
    fn the_sample_says_when_it_is_a_real_pane_and_the_invented_one_says_how_to_use_one() {
        let story = real_story();

        let said = real_note(&story);

        assert!(said.contains("the real pane %31"), "{said}");
        assert!(said.contains("window @7"), "{said}");
        assert!(said.contains("session $3 called api"), "{said}");
        assert!(said.contains("acts on that pane"), "{said}");
        assert!(said.contains("[ -n \"$TAW_TEST\" ] && exit 0"), "{said}");
        assert!(note().contains("--pane <pane>"), "{}", note());
    }
}
