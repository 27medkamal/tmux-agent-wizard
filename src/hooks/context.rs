//! What a hook is told about the event: the `TAW_*` environment, and the event's JSON.
//!
//! Both are built from the payload the event was emitted with, the one a plugin gets, so a
//! hook and a plugin hear the same thing. The JSON is that payload as a plugin's context
//! plus the event's name. The environment is a handful of the facts a script most often
//! wants without parsing anything: which session, directory, window, pane and agent, what
//! state it moved between, and where the agent runs.
//!
//! What is in the environment is data, and only data. A pane's title is whatever the
//! program in it wrote, and a directory is named by whoever made it, so neither is ever
//! part of a command line: the command is the person's own text, and this is what it is
//! handed.

use serde_json::{Map, Value};

use crate::plugin::{env as plugin_env, events as plugin_events};
use crate::tmux::{PaneInfo, ids};

/// Every variable a hook is given, in the order the README lists them. All of them are set
/// for every event, empty where the event has nothing to say: a script run under `set -u`
/// must not stop over `TAW_AGENT` on a `workspace.created`.
pub const VARIABLES: [&str; 17] = [
    "TAW_EVENT",
    "TAW_SESSION",
    "TAW_SESSION_ID",
    "TAW_DIR",
    "TAW_WINDOW",
    "TAW_PANE",
    "TAW_AGENT",
    "TAW_STATE",
    "TAW_PREVIOUS_STATE",
    "TAW_TITLE",
    "TAW_MESSAGE",
    "TAW_KIND",
    "TAW_TARGET",
    "TAW_SANDBOX",
    "TAW_RUNTIME",
    MACHINE,
    TEST,
];

/// The variable that is `1` when a hook is run by `taw hooks test` and empty for every real
/// event, so that a hook that acts on the pane can tell.
pub const TEST: &str = "TAW_TEST";

/// The variable that names the machine a notification is about, by its `[machines]` name: empty for this
/// machine's own agents and for every other event.
pub const MACHINE: &str = "TAW_MACHINE";

/// The longest an environment value is. A pane title can be as long as the program that set
/// it likes, and the environment has a size limit of its own; the JSON on standard input is
/// never cut.
pub const MAX_VALUE_BYTES: usize = 4096;

/// What a hook is given for one event.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// The variables taw adds to the environment the hook inherits, as `(name, value)`, in
    /// [`VARIABLES`] order.
    pub env: Vec<(String, String)>,
    /// The event as one line of JSON: the plugin context, with `"event"` first.
    pub json: String,
}

impl Built {
    /// This, for a hook `taw hooks test` runs: `TAW_TEST` is `1`, where a real event's is empty.
    pub fn marked_as_test(mut self) -> Built {
        if let Some((_, value)) = self.env.iter_mut().find(|(name, _)| name == TEST) {
            *value = "1".to_string();
        }
        self
    }

    /// This, for a notification about `machine` (its name in `[machines]`): `TAW_MACHINE` is that name
    /// and the JSON says so in `machine`. `taw hooks test notification --machine` makes its sample one.
    pub fn about_machine(mut self, machine: &str) -> Built {
        if let Some((_, value)) = self.env.iter_mut().find(|(name, _)| name == MACHINE) {
            *value = clean(machine);
        }
        if let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(&self.json) {
            fields.insert("machine".to_string(), Value::String(machine.to_string()));
            self.json = Value::Object(fields).to_string();
        }
        self
    }

    /// This, with the session named the way tmux takes an exact session: `=` in front and `:`
    /// after. tmux reads a session in a target as the start of a name when no session has exactly
    /// it, so `tmux kill-session -t taw-hooks-test` ends `taw-hooks-testing` if that is the one
    /// there is, and `=` is its mark for exactly this name. A command that takes a window reads a
    /// bare name as a window's first and then, when no window has it, as the start of a session's
    /// (`=` is only the exact-name mark of the window), so `tmux kill-window -t =taw-hooks-test`
    /// still reaches `taw-hooks-testing`; the `:` says the name is a session's, and tmux then
    /// looks for a session called exactly that. A test runs a person's hooks against a session
    /// that does not exist, and this is how a hook that acts on `$TAW_SESSION` finds none,
    /// whatever else is on the server, unless a session of theirs is called exactly that. A
    /// session that is not named stays empty, and only the environment has it: the JSON is the
    /// event's own.
    pub fn with_exact_session(mut self) -> Built {
        if let Some((_, value)) = self.env.iter_mut().find(|(name, _)| name == "TAW_SESSION")
            && !value.is_empty()
            && !value.starts_with('=')
        {
            value.insert(0, '=');
            value.push(':');
        }
        self
    }

    /// This, with the session, window and pane ids given put in wherever the event left one
    /// empty. Most events are about one thing and say nothing of the rest (a `workspace.renamed`
    /// has no pane), and an empty tmux target is the current pane, window or session, whichever
    /// one somebody is looking at: a hook that acts on `TAW_PANE` has to act on the one it was
    /// told of, or on none. The JSON is the event's own and is left as it is.
    pub fn with_ids(mut self, session_id: &str, window_id: &str, pane_id: &str) -> Built {
        for (name, id) in [
            ("TAW_SESSION_ID", session_id),
            ("TAW_WINDOW", window_id),
            ("TAW_PANE", pane_id),
        ] {
            if let Some((_, value)) = self.env.iter_mut().find(|(own, _)| own == name)
                && value.is_empty()
            {
                *value = clean(id);
            }
        }
        self
    }
}

/// The environment and the JSON for `event`, from the `payload` it was emitted with.
///
/// `panes` is what tmux lists, for what the payload leaves out: most payloads name a session,
/// a window or a pane by its id and carry no name or directory. It is only read, never
/// asked for here. A thing that is already gone (a closed session) is in no list, and its
/// variables stay empty.
pub fn build(event: &str, payload: &Value, panes: &[PaneInfo]) -> Built {
    let mut facts = Facts::from_payload(event, payload);
    // A notification about another machine names nothing of this one's: its pane id is that machine's, and
    // a pane of this Mac that happens to have the same number is not it.
    if facts.machine.is_empty() {
        facts.enrich(panes);
    }
    Built {
        env: facts.env(event),
        json: event_json(event, payload),
    }
}

/// The plugin context the payload makes, as one line of JSON, with the event's name put
/// first: a hook that pipes its standard input to a webhook sends the name along.
fn event_json(event: &str, payload: &Value) -> String {
    let context = plugin_env::context_json(&plugin_events::payload_to_context(payload.clone()));
    let mut map = Map::new();
    map.insert("event".to_string(), Value::String(event.to_string()));
    if let Value::Object(fields) = context {
        map.extend(fields);
    }
    Value::Object(map).to_string()
}

/// What an event says about itself, one string per variable, empty where it says nothing.
/// Ids are tmux's own (`$3`, `@7`, `%31`): the ones a `tmux` command takes, and the ones
/// `taw` accepts as well as its own `w3`, `w3:t7`, `w3:p31`, which the JSON carries.
#[derive(Debug, Default)]
struct Facts {
    session_id: String,
    session: String,
    dir: String,
    window: String,
    pane: String,
    agent: String,
    state: String,
    previous_state: String,
    title: String,
    message: String,
    kind: String,
    target: String,
    /// The profile the pane's agent was started through, and the container runtime it runs
    /// under: what the sandbox events say came or left, else what the pane's record says.
    sandbox: String,
    runtime: String,
    /// The machine a notification is about, by its `[machines]` name; empty for this machine's own.
    machine: String,
}

/// A non-empty string field of `value`, if it has one. A JSON `null`, a missing key and a
/// value that is not an object all read the same.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// The first of `candidates` that is there, or nothing.
fn first<'a>(candidates: &[Option<&'a str>]) -> &'a str {
    candidates.iter().flatten().next().copied().unwrap_or("")
}

impl Facts {
    /// What the payload itself says. Most payloads carry a record of what they are about
    /// (`workspace`, `tab`, `pane`, and for a worktree `worktree`), and a bare public id
    /// (`w3`, `w3:t7`, `w3:p31`) for the rest, which is turned back into tmux's own.
    fn from_payload(event: &str, payload: &Value) -> Facts {
        let none = Value::Null;
        let record = |key: &str| payload.get(key).unwrap_or(&none);
        let (workspace, tab, pane, worktree, layout) = (
            record("workspace"),
            record("tab"),
            record("pane"),
            record("worktree"),
            record("layout"),
        );

        let session_id = match ids::parse(first(&[
            text(workspace, "workspace_id"),
            text(pane, "workspace_id"),
            text(tab, "workspace_id"),
            text(layout, "workspace_id"),
            text(payload, "workspace_id"),
        ])) {
            Some(ids::Target::Workspace(id)) => id,
            _ => String::new(),
        };
        let window = match ids::parse(first(&[
            text(tab, "tab_id"),
            text(pane, "tab_id"),
            text(layout, "tab_id"),
            text(payload, "tab_id"),
        ])) {
            Some(ids::Target::Tab(id)) => id,
            _ => String::new(),
        };
        let pane_id = match ids::parse(first(&[
            text(pane, "terminal_id"),
            text(pane, "pane_id"),
            text(payload, "pane_id"),
        ])) {
            Some(ids::Target::Pane(id)) => id,
            _ => String::new(),
        };
        // A renamed session's new name is the whole of what its event carries about it.
        let renamed = event
            .starts_with("workspace.")
            .then(|| text(payload, "label"))
            .flatten();
        // Where the agent runs: the sandbox events carry what came or left, which the pane's own
        // record may no longer hold; any other event about a pane has the record.
        let own = record("sandbox");
        let held = pane.get("sandbox").unwrap_or(&none);

        Facts {
            session_id,
            session: first(&[
                text(workspace, "label"),
                text(worktree, "session_name"),
                renamed,
            ])
            .to_string(),
            dir: first(&[
                text(worktree, "path"),
                text(pane, "cwd"),
                text(workspace, "cwd"),
            ])
            .to_string(),
            window,
            pane: pane_id,
            agent: first(&[text(payload, "agent"), text(pane, "agent")]).to_string(),
            state: first(&[text(payload, "agent_status"), text(pane, "agent_status")]).to_string(),
            previous_state: first(&[text(payload, "previous_status")]).to_string(),
            title: first(&[
                text(pane, "terminal_title"),
                text(tab, "label"),
                text(payload, "label"),
                text(workspace, "label"),
            ])
            .to_string(),
            message: first(&[text(payload, "message")]).to_string(),
            kind: first(&[text(payload, "kind")]).to_string(),
            target: first(&[text(payload, "target")]).to_string(),
            sandbox: first(&[text(own, "profile"), text(held, "profile")]).to_string(),
            runtime: first(&[text(own, "runtime"), text(held, "runtime")]).to_string(),
            machine: first(&[text(payload, "machine")]).to_string(),
        }
    }

    /// Fills in what the payload left out from `panes`, for the thing the event is about: its
    /// pane if it names one, else its window, else its session. A pane brings its agent, its
    /// state, where the agent runs and its title; a window or a session brings the session's name
    /// and directory.
    fn enrich(&mut self, panes: &[PaneInfo]) {
        let about_pane = (!self.pane.is_empty())
            .then(|| panes.iter().find(|p| p.pane_id == self.pane))
            .flatten();
        let found = about_pane
            .or_else(|| {
                (!self.window.is_empty())
                    .then(|| {
                        panes
                            .iter()
                            .filter(|p| p.window_id == self.window)
                            .max_by_key(|p| p.pane_active)
                    })
                    .flatten()
            })
            .or_else(|| {
                (!self.session_id.is_empty())
                    .then(|| {
                        panes
                            .iter()
                            .filter(|p| p.session_id == self.session_id)
                            .max_by_key(|p| (p.window_active, p.pane_active))
                    })
                    .flatten()
            });
        let Some(found) = found else {
            return;
        };
        fill(&mut self.session_id, &found.session_id);
        fill(&mut self.session, &found.session_name);
        match about_pane {
            Some(pane) => {
                fill(&mut self.window, &pane.window_id);
                fill(&mut self.dir, &pane.pane_current_path);
                fill(
                    &mut self.agent,
                    pane.agent.kind.as_deref().unwrap_or_default(),
                );
                fill(
                    &mut self.state,
                    pane.agent.state.map_or("", |state| state.as_str()),
                );
                fill(
                    &mut self.sandbox,
                    pane.agent.sandbox.as_deref().unwrap_or_default(),
                );
                fill(
                    &mut self.runtime,
                    pane.agent.container.as_deref().unwrap_or_default(),
                );
                fill(&mut self.title, &pane.pane_title);
            }
            None => {
                fill(&mut self.dir, &found.session_path);
                let name = if self.window.is_empty() {
                    &found.session_name
                } else {
                    &found.window_name
                };
                fill(&mut self.title, name);
            }
        }
    }

    /// The variables, in [`VARIABLES`] order, cleaned for the environment.
    fn env(&self, event: &str) -> Vec<(String, String)> {
        let values = [
            event,
            &self.session,
            &self.session_id,
            &self.dir,
            &self.window,
            &self.pane,
            &self.agent,
            &self.state,
            &self.previous_state,
            &self.title,
            &self.message,
            &self.kind,
            &self.target,
            &self.sandbox,
            &self.runtime,
            &self.machine,
            // Set by `Built::marked_as_test`, for a test.
            "",
        ];
        VARIABLES
            .iter()
            .zip(values)
            .map(|(name, value)| (name.to_string(), clean(value)))
            .collect()
    }
}

/// Sets `slot` to `value` when nothing has set it.
fn fill(slot: &mut String, value: &str) {
    if slot.is_empty() {
        *slot = value.to_string();
    }
}

/// `value` as an environment value: no NUL, which no environment can hold, and no more than
/// [`MAX_VALUE_BYTES`], cut on a character boundary.
fn clean(value: &str) -> String {
    let mut out: String = value.chars().filter(|c| *c != '\0').collect();
    if out.len() > MAX_VALUE_BYTES {
        let mut end = MAX_VALUE_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::context::pane_json;
    use crate::store::{AgentRecord, AgentState};
    use serde_json::json;

    /// A pane as tmux lists it: session `api` (`$3`), window `@7` named `editor`, pane `%31`.
    fn pane() -> PaneInfo {
        PaneInfo {
            pane_id: "%31".to_string(),
            session_id: "$3".to_string(),
            session_name: "api".to_string(),
            session_path: "/home/me/api".to_string(),
            session_attached: 1,
            window_id: "@7".to_string(),
            window_index: 2,
            window_name: "editor".to_string(),
            window_active: true,
            window_layout: "layout".to_string(),
            pane_index: 0,
            pane_active: true,
            pane_pid: 4242,
            pane_current_command: "claude".to_string(),
            pane_current_path: "/home/me/api/src".to_string(),
            pane_title: "Fix the flaky test".to_string(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord {
                kind: Some("claude".to_string()),
                state: Some(AgentState::Done),
                ..AgentRecord::default()
            },
        }
    }

    /// The value of `name` in a built environment: `None` when it is not set at all.
    fn var<'a>(built: &'a Built, name: &str) -> Option<&'a str> {
        built
            .env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The payload the watcher emits for an agent's state change.
    fn status_changed(pane: &PaneInfo, from: &str, to: &str) -> Value {
        json!({
            "pane_id": "w3:p31",
            "workspace_id": "w3",
            "agent": pane.agent.kind,
            "agent_status": to,
            "previous_status": from,
            "kind_changed": false,
            "pane": pane_json(pane),
        })
    }

    #[test]
    fn an_agent_state_change_says_which_agent_moved_between_which_states_and_where() {
        let pane = pane();

        let built = build(
            "agent.done",
            &status_changed(&pane, "working", "done"),
            std::slice::from_ref(&pane),
        );

        for (name, value) in [
            ("TAW_EVENT", "agent.done"),
            ("TAW_SESSION", "api"),
            ("TAW_SESSION_ID", "$3"),
            ("TAW_DIR", "/home/me/api/src"),
            ("TAW_WINDOW", "@7"),
            ("TAW_PANE", "%31"),
            ("TAW_AGENT", "claude"),
            ("TAW_STATE", "done"),
            ("TAW_PREVIOUS_STATE", "working"),
            ("TAW_TITLE", "Fix the flaky test"),
        ] {
            assert_eq!(var(&built, name), Some(value), "{name}");
        }
    }

    /// A pane tmux titled itself, with the host name, gives a hook no title: `TAW_TITLE` is
    /// the tab's label that stands in for a pane's title, and the JSON on its standard input
    /// has an empty `terminal_title`. A pane a program titled gives that title. Read from a
    /// listing, which is where the title is taken.
    #[test]
    fn a_pane_tmux_titled_itself_gives_a_hook_no_title() {
        const HOST: &str = "studio.local";
        let out = format!(
            "{}\n{}",
            crate::tmux::listing_line("%31", HOST, "claude"),
            crate::tmux::listing_line("%32", "Fix the flaky test", "claude")
        );
        let panes = crate::tmux::parse_pane_lines(&out, Some(HOST)).0;
        let built = |pane: &PaneInfo| {
            let mut payload = status_changed(pane, "working", "done");
            payload["tab"] = json!({ "label": "editor" });
            build("agent.done", &payload, std::slice::from_ref(pane))
        };

        let untitled = built(&panes[0]);
        assert_eq!(var(&untitled, "TAW_TITLE"), Some("editor"));
        let stdin: Value = serde_json::from_str(&untitled.json).unwrap();
        assert_eq!(stdin["pane"]["terminal_title"], "");

        let titled = built(&panes[1]);
        assert_eq!(var(&titled, "TAW_TITLE"), Some("Fix the flaky test"));
    }

    /// `TAW_TEST` is `1` under `taw hooks test` and empty for every real event, so a hook that
    /// acts on the pane can start with `[ -n "$TAW_TEST" ] && exit 0`, and a script run under
    /// `set -u` does not stop over it.
    #[test]
    fn a_real_event_is_no_test_and_a_test_says_so() {
        let real = build("agent.done", &json!({}), &[]);
        assert_eq!(var(&real, "TAW_TEST"), Some(""));

        let tested = build("agent.done", &json!({}), &[]).marked_as_test();

        assert_eq!(var(&tested, "TAW_TEST"), Some("1"));
        // Nothing else about it changes.
        let without = |built: &Built| -> Vec<(String, String)> {
            built
                .env
                .iter()
                .filter(|(name, _)| name != "TAW_TEST")
                .cloned()
                .collect()
        };
        assert_eq!(without(&real), without(&tested));
        assert_eq!(real.json, tested.json);
    }

    /// `=` in front of a name is tmux's mark for exactly that name, where a name alone is also the
    /// start of one, and `:` after it says the name is a session's, where a bare name is also a
    /// window's: it is put on a session that is named, and an event with none is left with none,
    /// since an empty target is the current session and `=:` is no session at all.
    #[test]
    fn a_session_can_be_named_as_exactly_that_name_and_one_that_is_not_named_stays_so() {
        let named = build(
            "workspace.renamed",
            &json!({"workspace_id": "w3", "label": "api"}),
            &[],
        );
        assert_eq!(var(&named, "TAW_SESSION"), Some("api"));

        let exact = named.clone().with_exact_session();

        assert_eq!(var(&exact, "TAW_SESSION"), Some("=api:"));
        assert_eq!(exact.json, named.json, "the JSON is the event's own");
        // Once is all it takes.
        assert_eq!(
            var(&exact.clone().with_exact_session(), "TAW_SESSION"),
            Some("=api:")
        );
        // Nothing else is touched.
        let others = |built: &Built| -> Vec<(String, String)> {
            built
                .env
                .iter()
                .filter(|(name, _)| name != "TAW_SESSION")
                .cloned()
                .collect()
        };
        assert_eq!(others(&exact), others(&named));

        let unnamed = build("workspace.closed", &json!({"workspace_id": "w3"}), &[]);
        assert_eq!(var(&unnamed.with_exact_session(), "TAW_SESSION"), Some(""));
    }

    #[test]
    fn every_variable_is_always_set_and_empty_where_the_event_has_nothing_to_say() {
        let built = build("workspace.created", &json!({}), &[]);

        let names: Vec<&str> = built.env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, VARIABLES);
        assert_eq!(var(&built, "TAW_EVENT"), Some("workspace.created"));
        for name in &VARIABLES[1..] {
            assert_eq!(var(&built, name), Some(""), "{name}");
        }
    }

    /// The reason the environment exists: a title, a directory or a message that a shell
    /// would act on arrives as the characters it is.
    #[test]
    fn a_title_that_looks_like_a_command_arrives_as_a_title() {
        let mut pane = pane();
        pane.pane_title = "$(rm -rf ~) `id` ; \"quoted\" 'x' \\n".to_string();
        pane.pane_current_path = "/tmp/$(touch pwned)".to_string();

        let built = build(
            "agent.done",
            &status_changed(&pane, "working", "done"),
            std::slice::from_ref(&pane),
        );

        assert_eq!(
            var(&built, "TAW_TITLE"),
            Some("$(rm -rf ~) `id` ; \"quoted\" 'x' \\n")
        );
        assert_eq!(var(&built, "TAW_DIR"), Some("/tmp/$(touch pwned)"));
    }

    #[test]
    fn a_notification_carries_its_text_and_whether_it_is_a_request_or_a_done() {
        let pane = pane();
        let payload = json!({
            "kind": "request",
            "message": "claude blocked \u{b7} api:2 \u{b7} needs input",
            "pane_id": "w3:p31",
            "workspace_id": "w3",
            "agent": "claude",
            "agent_status": "blocked",
            "pane": pane_json(&pane),
        });

        let built = build("notification", &payload, &[pane]);

        assert_eq!(var(&built, "TAW_KIND"), Some("request"));
        assert_eq!(
            var(&built, "TAW_MESSAGE"),
            Some("claude blocked \u{b7} api:2 \u{b7} needs input")
        );
        assert_eq!(var(&built, "TAW_SESSION"), Some("api"));
        assert_eq!(var(&built, "TAW_STATE"), Some("blocked"));
    }

    /// A notification that names a machine is about nothing this machine lists: a pane of this one with the
    /// same id does not fill its session, directory or title in, whatever the caller passed.
    #[test]
    fn a_notification_about_a_machine_names_it_and_is_filled_in_from_none_of_the_local_panes() {
        let pane = pane();
        let about = |machine: &str| {
            let payload = json!({
                "kind": "request",
                "message": "claude blocked",
                "pane_id": "%31",
                "agent": "claude",
                "agent_status": "blocked",
                "machine": machine,
            });
            build("notification", &payload, std::slice::from_ref(&pane))
        };

        // The positive half: with no machine the same payload is filled in from the pane it names.
        let own = about("");
        assert_eq!(var(&own, "TAW_MACHINE"), Some(""));
        assert_eq!(var(&own, "TAW_SESSION"), Some("api"));
        assert_eq!(var(&own, "TAW_DIR"), Some("/home/me/api/src"));

        let other = about("studio");
        assert_eq!(var(&other, "TAW_MACHINE"), Some("studio"));
        assert_eq!(var(&other, "TAW_KIND"), Some("request"));
        for name in ["TAW_SESSION", "TAW_SESSION_ID", "TAW_DIR", "TAW_TITLE"] {
            assert_eq!(var(&other, name), Some(""), "{name}");
        }
    }

    #[test]
    fn a_sample_made_about_a_machine_says_so_in_the_variable_and_in_the_json() {
        let built =
            build("notification", &json!({ "kind": "request" }), &[]).about_machine("studio");

        assert_eq!(var(&built, "TAW_MACHINE"), Some("studio"));
        let json: Value = serde_json::from_str(&built.json).unwrap();
        assert_eq!(json["machine"], "studio");
        assert_eq!(json["event"], "notification");
        assert_eq!(json["kind"], "request");
    }

    #[test]
    fn a_session_that_is_already_gone_has_only_the_id_its_event_carried() {
        // `workspace.closed` is built from the id alone: nothing is left to ask about.
        let built = build("workspace.closed", &json!({"workspace_id": "w3"}), &[]);

        assert_eq!(var(&built, "TAW_SESSION_ID"), Some("$3"));
        assert_eq!(var(&built, "TAW_SESSION"), Some(""));
        assert_eq!(var(&built, "TAW_DIR"), Some(""));
    }

    #[test]
    fn a_pane_closed_event_names_its_pane_and_session_by_id() {
        let built = build(
            "pane.closed",
            &json!({"pane_id": "w3:p31", "workspace_id": "w3"}),
            &[],
        );

        assert_eq!(var(&built, "TAW_PANE"), Some("%31"));
        assert_eq!(var(&built, "TAW_SESSION_ID"), Some("$3"));
    }

    #[test]
    fn a_window_event_is_filled_in_from_the_panes_tmux_lists() {
        let built = build(
            "tab.focused",
            &json!({"tab_id": "w3:t7", "workspace_id": "w3"}),
            &[pane()],
        );

        assert_eq!(var(&built, "TAW_SESSION"), Some("api"));
        assert_eq!(var(&built, "TAW_WINDOW"), Some("@7"));
        // The window is what the event is about: no pane, and the session's own directory.
        assert_eq!(var(&built, "TAW_PANE"), Some(""));
        assert_eq!(var(&built, "TAW_DIR"), Some("/home/me/api"));
        assert_eq!(var(&built, "TAW_TITLE"), Some("editor"));
    }

    #[test]
    fn a_session_event_is_about_the_session_and_not_one_of_its_panes() {
        let built = build(
            "workspace.focused",
            &json!({"workspace_id": "w3"}),
            &[pane()],
        );

        assert_eq!(var(&built, "TAW_SESSION"), Some("api"));
        assert_eq!(var(&built, "TAW_DIR"), Some("/home/me/api"));
        assert_eq!(var(&built, "TAW_PANE"), Some(""));
        assert_eq!(var(&built, "TAW_AGENT"), Some(""));
        assert_eq!(var(&built, "TAW_TITLE"), Some("api"));
    }

    #[test]
    fn a_layout_event_names_its_window_and_session_from_the_layout_it_carries() {
        // `layout.updated` is `{"layout": <snapshot>}`, and the snapshot holds the ids.
        let payload = json!({"layout": {
            "workspace_id": "w3",
            "tab_id": "w3:t7",
            "focused_pane_id": "w3:p31",
            "panes": [],
        }});

        let built = build("layout.updated", &payload, &[pane()]);

        assert_eq!(var(&built, "TAW_SESSION_ID"), Some("$3"));
        assert_eq!(var(&built, "TAW_SESSION"), Some("api"));
        assert_eq!(var(&built, "TAW_WINDOW"), Some("@7"));
        // The window changed, not one of its panes.
        assert_eq!(var(&built, "TAW_PANE"), Some(""));
    }

    #[test]
    fn a_rename_says_the_new_name() {
        let window = build(
            "tab.renamed",
            &json!({"tab_id": "w3:t7", "workspace_id": "w3", "label": "logs"}),
            &[],
        );
        assert_eq!(var(&window, "TAW_TITLE"), Some("logs"));
        assert_eq!(var(&window, "TAW_WINDOW"), Some("@7"));

        let session = build(
            "workspace.renamed",
            &json!({"workspace_id": "w3", "label": "api-v2"}),
            &[],
        );
        assert_eq!(var(&session, "TAW_SESSION"), Some("api-v2"));
    }

    #[test]
    fn a_worktree_event_says_where_the_checkout_is() {
        let payload = json!({
            "workspace": {
                "workspace_id": "w9",
                "label": "repo--feat-x",
                "cwd": "/home/me/.tmux-agent-wizard/worktrees/repo--feat-x",
            },
            "worktree": {
                "workspace_id": "w9",
                "session_name": "repo--feat-x",
                "path": "/home/me/.tmux-agent-wizard/worktrees/repo--feat-x",
                "branch": "feat/x",
                "repo_root": "/home/me/repo",
            },
        });

        let built = build("worktree.created", &payload, &[]);

        assert_eq!(var(&built, "TAW_SESSION"), Some("repo--feat-x"));
        assert_eq!(var(&built, "TAW_SESSION_ID"), Some("$9"));
        assert_eq!(
            var(&built, "TAW_DIR"),
            Some("/home/me/.tmux-agent-wizard/worktrees/repo--feat-x")
        );
    }

    #[test]
    fn a_removed_worktree_names_its_session_though_the_session_is_gone() {
        let payload = json!({
            "workspace_id": "w9",
            "worktree": {
                "workspace_id": "w9",
                "session_name": "repo--feat-x",
                "path": "/w/repo--feat-x",
                "branch": "feat/x",
            },
            "forced": false,
        });

        let built = build("worktree.removed", &payload, &[]);

        assert_eq!(var(&built, "TAW_SESSION"), Some("repo--feat-x"));
        assert_eq!(var(&built, "TAW_DIR"), Some("/w/repo--feat-x"));
    }

    #[test]
    fn a_close_that_is_asked_about_says_what_is_being_closed() {
        let pane = pane();
        let payload = json!({"target": "pane", "pane": pane_json(&pane)});

        let built = build("pre_close", &payload, &[pane]);

        assert_eq!(var(&built, "TAW_TARGET"), Some("pane"));
        assert_eq!(var(&built, "TAW_PANE"), Some("%31"));
        assert_eq!(var(&built, "TAW_SESSION"), Some("api"));
    }

    #[test]
    fn the_json_is_the_plugin_context_with_the_event_name_first() {
        let pane = pane();
        let payload = status_changed(&pane, "working", "done");

        let built = build("agent.done", &payload, std::slice::from_ref(&pane));

        let json: Value = serde_json::from_str(&built.json).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys[0], "event");
        assert_eq!(json["event"], "agent.done");
        // What a plugin's HERDR_PLUGIN_CONTEXT_JSON holds: the pane record, and every other
        // top-level key of the payload beside it.
        assert_eq!(json["pane"], pane_json(&pane));
        assert_eq!(json["agent_status"], "done");
        assert_eq!(json["previous_status"], "working");
        assert_eq!(json["kind_changed"], false);
        assert_eq!(json["workspace_id"], "w3");
        assert_eq!(built.json.lines().count(), 1, "one line");
    }

    #[test]
    fn an_agent_event_in_a_sandbox_says_the_profile_and_the_runtime_and_one_outside_says_nothing() {
        let mut pane = pane();
        pane.agent.container = Some("docker".to_string());
        pane.agent.sandbox = Some("default".to_string());

        let inside = build(
            "agent.done",
            &status_changed(&pane, "working", "done"),
            std::slice::from_ref(&pane),
        );

        assert_eq!(var(&inside, "TAW_SANDBOX"), Some("default"));
        assert_eq!(var(&inside, "TAW_RUNTIME"), Some("docker"));

        let outside = build(
            "agent.done",
            &status_changed(&self::pane(), "working", "done"),
            &[self::pane()],
        );
        assert_eq!(var(&outside, "TAW_SANDBOX"), Some(""));
        assert_eq!(var(&outside, "TAW_RUNTIME"), Some(""));
    }

    #[test]
    fn a_container_taw_only_recognised_has_a_runtime_and_no_profile() {
        let mut pane = pane();
        pane.agent.container = Some("podman".to_string());
        let built = build(
            "notification",
            &json!({"kind": "done", "message": "m", "pane": pane_json(&pane)}),
            std::slice::from_ref(&pane),
        );
        assert_eq!(var(&built, "TAW_SANDBOX"), Some(""));
        assert_eq!(var(&built, "TAW_RUNTIME"), Some("podman"));
    }

    #[test]
    fn sandbox_stopped_says_what_left_though_the_pane_holds_it_no_more() {
        let pane = pane();
        let payload = json!({
            "pane_id": "w3:p31",
            "workspace_id": "w3",
            "agent": "claude",
            "sandbox": {"profile": "box", "runtime": "docker"},
            "pane": pane_json(&pane),
        });
        let built = build("sandbox.stopped", &payload, std::slice::from_ref(&pane));
        assert_eq!(var(&built, "TAW_SANDBOX"), Some("box"));
        assert_eq!(var(&built, "TAW_RUNTIME"), Some("docker"));
    }

    /// The record an event carries of its pane says where the agent runs, though tmux lists the
    /// pane no more by the time the hook is told (a toast about a pane closed since).
    #[test]
    fn the_record_an_event_carries_says_where_its_agent_runs_with_no_listing() {
        let mut pane = pane();
        pane.agent.container = Some("docker".to_string());
        pane.agent.sandbox = Some("default".to_string());

        let built = build(
            "notification",
            &json!({"kind": "done", "message": "m", "pane": pane_json(&pane)}),
            &[],
        );

        assert_eq!(var(&built, "TAW_SANDBOX"), Some("default"));
        assert_eq!(var(&built, "TAW_RUNTIME"), Some("docker"));
    }

    /// An event about a pane that carries no record of it (`pane.focused` has its id alone) is told
    /// where the pane's agent runs from what tmux lists, as it is told the agent.
    #[test]
    fn an_event_that_names_a_sandboxed_pane_by_its_id_alone_is_told_where_its_agent_runs() {
        let mut pane = pane();
        pane.agent.container = Some("docker".to_string());
        pane.agent.sandbox = Some("default".to_string());

        let built = build(
            "pane.focused",
            &json!({"pane_id": "w3:p31", "workspace_id": "w3"}),
            std::slice::from_ref(&pane),
        );

        assert_eq!(var(&built, "TAW_AGENT"), Some("claude"));
        assert_eq!(var(&built, "TAW_SANDBOX"), Some("default"));
        assert_eq!(var(&built, "TAW_RUNTIME"), Some("docker"));
    }

    #[test]
    fn a_value_is_cut_on_a_character_boundary_and_holds_no_nul() {
        let mut pane = pane();
        pane.pane_title = format!("a\0b{}", "\u{e9}".repeat(MAX_VALUE_BYTES));

        let built = build(
            "agent.done",
            &status_changed(&pane, "working", "done"),
            std::slice::from_ref(&pane),
        );

        let title = var(&built, "TAW_TITLE").unwrap();
        assert!(title.starts_with("ab\u{e9}"), "{title:?}");
        assert!(!title.contains('\0'));
        assert!(title.len() <= MAX_VALUE_BYTES);
        // The JSON on standard input is whole.
        let json: Value = serde_json::from_str(&built.json).unwrap();
        assert!(json["pane"]["terminal_title"].as_str().unwrap().len() > MAX_VALUE_BYTES);
    }
}
