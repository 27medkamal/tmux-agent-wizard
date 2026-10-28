// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Builds the herdr-shaped JSON records (`pane`, `workspace`, `tab`) that plugins receive
//! via `HERDR_PLUGIN_CONTEXT_JSON` and that a later task reuses verbatim for CLI JSON
//! output. Field names and shapes are herdr's, copied so plugins
//! written against herdr's real records keep working.

use serde_json::{Value, json};

use crate::tmux::{PaneInfo, SessionInfo, WindowInfo, ids};

use super::{Context, PluginCtx};

/// Builds a `Context` by resolving each given id (public or raw tmux, or a tmux target/
/// name for panes — anything `tmux::ids::resolve_*` accepts) through `ctx.tmux` and
/// serializing the matching record. Each parameter left `None` leaves the corresponding
/// field `None`. `caller_pane_id` and `extra` are always empty here — callers that need
/// them set fill them in on the returned `Context` (all fields are public).
pub fn from_ids(
    ctx: &PluginCtx,
    workspace: Option<&str>,
    tab: Option<&str>,
    pane: Option<&str>,
) -> anyhow::Result<Context> {
    if workspace.is_none() && tab.is_none() && pane.is_none() {
        // Nothing to resolve — do not touch tmux (there may not even be a server).
        return Ok(Context::default());
    }
    let panes = ctx.tmux.list_panes()?;

    // A pane names its window and session, and a tab names its session: an action invoked on a
    // pane has that pane's tab and workspace too, as in herdr, where a pane lives in one tab of
    // one workspace. What the caller named explicitly is kept as it is.
    let pane_info = match pane {
        Some(id) => {
            let pane_id = ids::resolve_pane(ctx.tmux, id)?;
            let found = panes
                .iter()
                .find(|p| p.pane_id == pane_id)
                .ok_or_else(|| anyhow::anyhow!("pane '{id}' not found"))?;
            Some(found)
        }
        None => None,
    };

    let window_info = match tab {
        Some(id) => {
            let window_id = ids::resolve_window(ctx.tmux, id)?;
            let found = ctx
                .tmux
                .list_windows()?
                .into_iter()
                .find(|w| w.window_id == window_id)
                .ok_or_else(|| anyhow::anyhow!("tab '{id}' not found"))?;
            Some(found)
        }
        None => match pane_info {
            Some(p) => ctx
                .tmux
                .list_windows()?
                .into_iter()
                .find(|w| w.window_id == p.window_id),
            None => None,
        },
    };

    let session_id = match workspace {
        Some(id) => Some(ids::resolve_session(ctx.tmux, id)?),
        None => window_info
            .as_ref()
            .map(|w| w.session_id.clone())
            .or_else(|| pane_info.map(|p| p.session_id.clone())),
    };
    let workspace = match session_id {
        Some(session_id) => {
            let session = ctx
                .tmux
                .list_sessions()?
                .into_iter()
                .find(|s| s.session_id == session_id);
            match (session, workspace) {
                (Some(session), _) => Some(workspace_json(&session, &panes)),
                (None, Some(id)) => anyhow::bail!("workspace '{id}' not found"),
                (None, None) => None,
            }
        }
        None => None,
    };
    let tab = window_info.map(|w| tab_json(&w, &panes));
    let pane = pane_info.map(pane_json);

    Ok(Context {
        workspace,
        tab,
        pane,
        caller_pane_id: None,
        extra: serde_json::Map::new(),
    })
}

/// The herdr `pane` record for one tmux pane: `{pane_id, workspace_id, tab_id,
/// terminal_id, cwd, foreground_cwd, restore_error, focused, agent_status, agent, name,
/// display_agent, title, terminal_title, terminal_title_stripped, revision, scroll}`, and taw's
/// own `sandbox`, where the pane's agent runs ([`sandbox_json`]).
///
/// `restore_error`: why a session restore left the pane as tmux made it, in herdr's words
/// (`@agent_restore_error`), and `null` for every other pane, where herdr leaves the field out.
///
/// `cwd`/`foreground_cwd`: herdr distinguishes the pane's tracked cwd (labels,
/// follow-cwd, restored session state) from the live cwd of whatever process currently
/// controls the pane's PTY. tmux exposes only one live-computed per-pane path
/// (`#{pane_current_path}`, which *is* the foreground process's cwd), so both fields read
/// from it here.
pub fn pane_json(pane: &PaneInfo) -> Value {
    let terminal_title = pane.pane_title.clone();
    let terminal_title_stripped = strip_spinner(&terminal_title);
    json!({
        "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
        "workspace_id": ids::workspace_id(&pane.session_id),
        "tab_id": ids::tab_id(&pane.session_id, &pane.window_id),
        "terminal_id": pane.pane_id,
        "cwd": non_empty(&pane.pane_current_path),
        "foreground_cwd": non_empty(&pane.pane_current_path),
        "restore_error": pane.agent.restore_error,
        "focused": pane.pane_active,
        "agent_status": pane_agent_status(pane),
        "agent": pane.agent.kind.clone(),
        "name": pane.agent.name.clone(),
        "display_agent": pane.agent.display.clone(),
        "title": pane.agent.title.clone(),
        "sandbox": sandbox_json(pane),
        "terminal_title": terminal_title,
        "terminal_title_stripped": terminal_title_stripped,
        "revision": 0,
        "scroll": {
            "max_offset_from_bottom": 0,
            "offset_from_bottom": 0,
            "viewport_rows": pane.pane_height,
        },
    })
}

/// Where a pane's agent runs: `{"profile": ..., "runtime": ...}` once it runs in a container, or
/// taw started it through a profile, each `null` while it is not known; `null` outside both.
fn sandbox_json(pane: &PaneInfo) -> Value {
    if pane.agent.container.is_none() && pane.agent.sandbox.is_none() {
        return Value::Null;
    }
    json!({ "profile": pane.agent.sandbox, "runtime": pane.agent.container })
}

/// The herdr `agent` record for one tmux pane: every [`pane_json`] field plus
/// `{agent, agent_status, state_change_seq, agent_session_id, agent_session_path}`. `agent`/`agent_status` are already in the pane record and
/// keep the same values here; the other three are the agent-only additions.
///
/// `state_change_seq` is herdr's monotonic per-pane state counter. tmux has no such
/// counter, so this is `@agent_state_since` — unix seconds of the last state change,
/// which is monotonic across transitions and answers the same "has this changed since I
/// last looked?" question. `0` when the pane has recorded no transition yet.
pub fn agent_json(pane: &PaneInfo) -> Value {
    let mut value = pane_json(pane);
    let map = value
        .as_object_mut()
        .expect("pane_json always returns an object");
    map.insert(
        "state_change_seq".to_string(),
        Value::from(pane.agent.state_since.unwrap_or(0)),
    );
    map.insert(
        "agent_session_id".to_string(),
        opt_string(&pane.agent.session_id),
    );
    map.insert(
        "agent_session_path".to_string(),
        opt_string(&pane.agent.session_path),
    );
    value
}

fn opt_string(s: &Option<String>) -> Value {
    match s {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    }
}

/// The herdr `workspace` record for one tmux session: `{workspace_id, label, number,
/// focused, active_tab_id, agent_status, pane_count, tab_count, cwd, branch}`.
///
/// `panes` need not be pre-filtered to this session — every count and aggregate here
/// filters internally by `session.session_id`. `number` has no tmux equivalent (sessions
/// are not indexed the way windows/panes are), so it is the numeric part of the tmux
/// session id, e.g. `$3` -> `3`.
///
/// `branch` is the checked-out branch at `#{session_path}`, via [`crate::git::branch`] —
/// `null` for a workspace outside a repository, on a detached HEAD, or where `git` is not
/// callable, none of which is an error. It is one bounded `git rev-parse` per call, not a
/// `git status`, because `workspace list` asks for it once per session.
pub fn workspace_json(session: &SessionInfo, panes: &[PaneInfo]) -> Value {
    let own_panes: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| p.session_id == session.session_id)
        .collect();
    let active_tab_id = if session.active_window_id.is_empty() {
        Value::Null
    } else {
        Value::String(ids::tab_id(&session.session_id, &session.active_window_id))
    };
    json!({
        "workspace_id": ids::workspace_id(&session.session_id),
        "label": session.session_name,
        "number": session_number(&session.session_id),
        "focused": session.session_attached > 0,
        "active_tab_id": active_tab_id,
        "agent_status": worst_status(own_panes.iter().copied()),
        "pane_count": own_panes.len(),
        "tab_count": session.session_windows,
        "cwd": non_empty(&session.session_path),
        "branch": branch_of(&session.session_path),
    })
}

/// The herdr `tab` record for one tmux window: `{tab_id, workspace_id, label, number,
/// focused, agent_status, pane_count}`. `panes` need not be pre-filtered to this window —
/// see [`workspace_json`].
pub fn tab_json(window: &WindowInfo, panes: &[PaneInfo]) -> Value {
    let own_panes: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| p.window_id == window.window_id)
        .collect();
    json!({
        "tab_id": ids::tab_id(&window.session_id, &window.window_id),
        "workspace_id": ids::workspace_id(&window.session_id),
        "label": window.window_name,
        "number": window.window_index,
        "focused": window.window_active,
        "agent_status": worst_status(own_panes.iter().copied()),
        "pane_count": own_panes.len(),
    })
}

/// The branch at a session's path, as JSON. An empty path (a session tmux reports no
/// `#{session_path}` for) is not looked up at all — `git -C ""` would resolve against the
/// process's own cwd and report a branch belonging to no workspace.
fn branch_of(session_path: &str) -> Value {
    if session_path.is_empty() {
        return Value::Null;
    }
    match crate::git::branch(std::path::Path::new(session_path)) {
        Some(branch) => Value::String(branch),
        None => Value::Null,
    }
}

fn session_number(session_id: &str) -> u64 {
    session_id.trim_start_matches('$').parse().unwrap_or(0)
}

fn non_empty(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}

/// This pane's own `agent_status`: `null` when no agent occupies it (no `@agent_kind`),
/// else its `@agent_state` or `"unknown"` when that option is unset.
fn pane_agent_status(pane: &PaneInfo) -> Value {
    if pane.agent.kind.is_none() {
        return Value::Null;
    }
    match pane.agent.state {
        Some(state) => Value::String(state.to_string()),
        None => Value::String("unknown".to_string()),
    }
}

/// Severity used to pick the worst `agent_status` among a set of panes for a
/// workspace/tab: `blocked > working > done > idle > unknown > null` — lower sorts first
/// (worse).
fn severity(status: &Value) -> u8 {
    match status.as_str() {
        Some("blocked") => 0,
        Some("working") => 1,
        Some("done") => 2,
        Some("idle") => 3,
        Some("unknown") => 4,
        _ => 5, // null (no agent in that pane), or defensively any unrecognized string
    }
}

fn worst_status<'a>(panes: impl Iterator<Item = &'a PaneInfo>) -> Value {
    panes
        .map(pane_agent_status)
        .min_by_key(severity)
        .unwrap_or(Value::Null)
}

/// Removes a leading spinner/status glyph and its following space, when `title` starts
/// with a non-alphanumeric character — e.g. `"✳ Building"` -> `"Building"`. Titles that
/// start with a letter or digit are returned unchanged.
fn strip_spinner(title: &str) -> String {
    let mut chars = title.chars();
    match chars.next() {
        Some(c) if !c.is_alphanumeric() => {
            let rest = chars.as_str();
            rest.strip_prefix(' ').unwrap_or(rest).to_string()
        }
        _ => title.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AgentRecord, AgentState};
    use std::collections::BTreeMap;

    fn pane(session_id: &str, window_id: &str, pane_id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_string(),
            session_id: session_id.to_string(),
            session_name: "one".to_string(),
            session_path: "/repo".to_string(),
            session_attached: 1,
            window_id: window_id.to_string(),
            window_index: 0,
            window_name: "editor".to_string(),
            window_active: true,
            window_layout: "layout".to_string(),
            pane_index: 0,
            pane_active: true,
            pane_pid: 4242,
            pane_current_command: "zsh".to_string(),
            pane_current_path: "/repo/src".to_string(),
            pane_title: "zsh".to_string(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord::default(),
        }
    }

    fn with_agent(mut p: PaneInfo, kind: &str, state: Option<AgentState>) -> PaneInfo {
        p.agent = AgentRecord {
            kind: Some(kind.to_string()),
            state,
            state_since: None,
            rule: None,
            source: None,
            session_id: None,
            session_path: None,
            name: None,
            display: None,
            title: None,
            seen: false,
            kind_hint: None,
            watch_pid: None,
            reported_kind: None,
            container: None,
            sandbox: None,
            tokens: BTreeMap::new(),
            resume: None,
            restore_error: None,
            restore_saved: None,
            claude_flags: Vec::new(),
        };
        p
    }

    #[test]
    fn pane_json_has_the_documented_field_names() {
        let value = pane_json(&pane("$3", "@7", "%31"));
        let obj = value.as_object().unwrap();
        let expected_keys = [
            "pane_id",
            "workspace_id",
            "tab_id",
            "terminal_id",
            "cwd",
            "foreground_cwd",
            "restore_error",
            "focused",
            "agent_status",
            "agent",
            "name",
            "display_agent",
            "title",
            "terminal_title",
            "terminal_title_stripped",
            "revision",
            "scroll",
            "sandbox",
        ];
        for key in expected_keys {
            assert!(obj.contains_key(key), "missing key '{key}' in {value:#}");
        }
        assert_eq!(obj.len(), expected_keys.len());
    }

    #[test]
    fn pane_json_says_where_the_agent_runs_and_null_outside_a_container() {
        let plain = with_agent(pane("$1", "@1", "%1"), "claude", Some(AgentState::Idle));
        assert_eq!(pane_json(&plain)["sandbox"], Value::Null);

        // A profile taw started whose container it has not seen yet.
        let mut starting = plain.clone();
        starting.agent.sandbox = Some("default".to_string());
        assert_eq!(
            pane_json(&starting)["sandbox"],
            json!({"profile": "default", "runtime": null})
        );

        let mut seen = plain.clone();
        seen.agent.container = Some("docker".to_string());
        assert_eq!(
            pane_json(&seen)["sandbox"],
            json!({"profile": null, "runtime": "docker"})
        );

        let mut started = seen.clone();
        started.agent.sandbox = Some("default".to_string());
        assert_eq!(
            pane_json(&started)["sandbox"],
            json!({"profile": "default", "runtime": "docker"})
        );
        assert_eq!(
            agent_json(&started)["sandbox"],
            json!({"profile": "default", "runtime": "docker"})
        );
    }

    /// herdr's `restore_error`: what a restore says of a pane it left as tmux made it, and `null` for
    /// every other pane, the pane of an agent included.
    #[test]
    fn pane_json_says_why_a_restore_left_the_pane_as_tmux_made_it() {
        let plain = with_agent(pane("$1", "@1", "%1"), "claude", Some(AgentState::Idle));
        assert_eq!(pane_json(&plain)["restore_error"], Value::Null);

        let mut left = pane("$1", "@1", "%2");
        left.agent.restore_error = Some(
            "Saved directory is unavailable. Restore the directory and restart this session."
                .to_string(),
        );
        assert_eq!(
            pane_json(&left)["restore_error"],
            "Saved directory is unavailable. Restore the directory and restart this session."
        );
        assert_eq!(
            agent_json(&left)["restore_error"],
            pane_json(&left)["restore_error"]
        );
    }

    #[test]
    fn pane_json_maps_ids_and_terminal_id() {
        let value = pane_json(&pane("$3", "@7", "%31"));
        assert_eq!(value["pane_id"], "w3:p31");
        assert_eq!(value["workspace_id"], "w3");
        assert_eq!(value["tab_id"], "w3:t7");
        assert_eq!(value["terminal_id"], "%31");
    }

    #[test]
    fn pane_json_cwd_and_foreground_cwd_both_come_from_pane_current_path() {
        let value = pane_json(&pane("$3", "@7", "%31"));
        assert_eq!(value["cwd"], "/repo/src");
        assert_eq!(value["foreground_cwd"], "/repo/src");
    }

    #[test]
    fn pane_json_empty_cwd_is_null() {
        let mut p = pane("$3", "@7", "%31");
        p.pane_current_path = String::new();
        let value = pane_json(&p);
        assert!(value["cwd"].is_null());
        assert!(value["foreground_cwd"].is_null());
    }

    #[test]
    fn pane_json_scroll_reports_pane_height_as_viewport_rows_and_zeros_otherwise() {
        let mut p = pane("$3", "@7", "%31");
        p.pane_height = 42;
        let value = pane_json(&p);
        assert_eq!(value["scroll"]["viewport_rows"], 42);
        assert_eq!(value["scroll"]["max_offset_from_bottom"], 0);
        assert_eq!(value["scroll"]["offset_from_bottom"], 0);
    }

    #[test]
    fn pane_json_revision_is_always_zero() {
        assert_eq!(pane_json(&pane("$3", "@7", "%31"))["revision"], 0);
    }

    #[test]
    fn pane_json_agent_status_is_null_without_an_agent_kind() {
        let value = pane_json(&pane("$3", "@7", "%31"));
        assert!(value["agent_status"].is_null());
        assert!(value["agent"].is_null());
    }

    #[test]
    fn pane_json_agent_status_is_unknown_when_kind_present_but_state_absent() {
        let p = with_agent(pane("$3", "@7", "%31"), "claude", None);
        let value = pane_json(&p);
        assert_eq!(value["agent_status"], "unknown");
        assert_eq!(value["agent"], "claude");
    }

    #[test]
    fn pane_json_agent_status_reflects_agent_state() {
        let p = with_agent(pane("$3", "@7", "%31"), "claude", Some(AgentState::Blocked));
        let value = pane_json(&p);
        assert_eq!(value["agent_status"], "blocked");
    }

    #[test]
    fn terminal_title_stripped_removes_leading_glyph_and_space() {
        assert_eq!(strip_spinner("✳ Building"), "Building");
        assert_eq!(strip_spinner("● working"), "working");
    }

    #[test]
    fn terminal_title_stripped_leaves_alphanumeric_titles_untouched() {
        assert_eq!(strip_spinner("claude: reviewer"), "claude: reviewer");
        assert_eq!(strip_spinner("3 files changed"), "3 files changed");
    }

    #[test]
    fn terminal_title_stripped_handles_empty_and_glyph_only_titles() {
        assert_eq!(strip_spinner(""), "");
        assert_eq!(strip_spinner("✳"), "");
        // Non-alphanumeric glyph with no following space: only the glyph is removed.
        assert_eq!(strip_spinner("✳Building"), "Building");
    }

    #[test]
    fn pane_json_terminal_title_is_stripped_in_terminal_title_stripped() {
        let mut p = pane("$3", "@7", "%31");
        p.pane_title = "✳ Building".to_string();
        let value = pane_json(&p);
        assert_eq!(value["terminal_title"], "✳ Building");
        assert_eq!(value["terminal_title_stripped"], "Building");
    }

    /// A pane tmux titled itself, with the host name, has no title in its record: a program
    /// that set one has, and its glyph is stripped as before. Read from a listing, which is
    /// where the title is taken.
    #[test]
    fn a_pane_tmux_titled_itself_has_no_terminal_title_in_its_record() {
        const HOST: &str = "studio.local";
        let out = format!(
            "{}\n{}",
            crate::tmux::listing_line("%1", HOST, "claude"),
            crate::tmux::listing_line("%2", "\u{2733} Building", "claude")
        );
        let panes = crate::tmux::parse_pane_lines(&out, Some(HOST)).0;

        let untitled = pane_json(&panes[0]);
        assert_eq!(untitled["terminal_title"], "");
        assert_eq!(untitled["terminal_title_stripped"], "");
        let titled = pane_json(&panes[1]);
        assert_eq!(titled["terminal_title"], "\u{2733} Building");
        assert_eq!(titled["terminal_title_stripped"], "Building");
    }

    fn session(id: &str, active_window: &str) -> SessionInfo {
        SessionInfo {
            session_id: id.to_string(),
            session_name: "one".to_string(),
            session_path: "/repo".to_string(),
            session_attached: 1,
            session_windows: 2,
            session_group_size: 0,
            active_window_id: active_window.to_string(),
            session_last_attached: 0,
        }
    }

    #[test]
    fn workspace_json_has_the_documented_field_names() {
        let value = workspace_json(&session("$3", "@7"), &[]);
        let obj = value.as_object().unwrap();
        let expected_keys = [
            "workspace_id",
            "label",
            "number",
            "focused",
            "active_tab_id",
            "agent_status",
            "pane_count",
            "tab_count",
            "cwd",
            "branch",
        ];
        for key in expected_keys {
            assert!(obj.contains_key(key), "missing key '{key}' in {value:#}");
        }
        assert_eq!(obj.len(), expected_keys.len());
    }

    #[test]
    fn workspace_json_maps_fields() {
        let value = workspace_json(&session("$3", "@7"), &[]);
        assert_eq!(value["workspace_id"], "w3");
        assert_eq!(value["label"], "one");
        assert_eq!(value["number"], 3);
        assert!(value["focused"].as_bool().unwrap());
        assert_eq!(value["active_tab_id"], "w3:t7");
        assert_eq!(value["tab_count"], 2);
        assert_eq!(value["cwd"], "/repo");
        assert!(value["branch"].is_null());
    }

    #[test]
    fn workspace_json_filters_panes_by_session_and_counts_them() {
        let panes = vec![
            pane("$3", "@7", "%31"),
            pane("$3", "@7", "%32"),
            pane("$9", "@1", "%1"), // a different session — must not be counted
        ];
        let value = workspace_json(&session("$3", "@7"), &panes);
        assert_eq!(value["pane_count"], 2);
    }

    #[test]
    fn workspace_json_agent_status_is_null_when_every_pane_has_no_agent() {
        let panes = vec![pane("$3", "@7", "%31"), pane("$3", "@7", "%32")];
        let value = workspace_json(&session("$3", "@7"), &panes);
        assert!(value["agent_status"].is_null());
    }

    #[test]
    fn workspace_json_agent_status_is_the_worst_among_its_panes() {
        let panes = vec![
            with_agent(pane("$3", "@7", "%31"), "claude", Some(AgentState::Idle)),
            with_agent(pane("$3", "@7", "%32"), "claude", Some(AgentState::Blocked)),
            with_agent(pane("$3", "@7", "%33"), "claude", Some(AgentState::Working)),
        ];
        let value = workspace_json(&session("$3", "@7"), &panes);
        assert_eq!(value["agent_status"], "blocked");
    }

    #[test]
    fn workspace_json_active_tab_id_is_null_when_session_has_no_active_window() {
        let value = workspace_json(&session("$3", ""), &[]);
        assert!(value["active_tab_id"].is_null());
    }

    fn window(session_id: &str, window_id: &str, index: u32, active: bool) -> WindowInfo {
        WindowInfo {
            window_id: window_id.to_string(),
            session_id: session_id.to_string(),
            window_index: index,
            window_name: "editor".to_string(),
            window_active: active,
            window_panes: 2,
            window_layout: "layout".to_string(),
        }
    }

    #[test]
    fn tab_json_has_the_documented_field_names() {
        let value = tab_json(&window("$3", "@7", 0, true), &[]);
        let obj = value.as_object().unwrap();
        let expected_keys = [
            "tab_id",
            "workspace_id",
            "label",
            "number",
            "focused",
            "agent_status",
            "pane_count",
        ];
        for key in expected_keys {
            assert!(obj.contains_key(key), "missing key '{key}' in {value:#}");
        }
        assert_eq!(obj.len(), expected_keys.len());
    }

    #[test]
    fn tab_json_maps_fields_and_filters_panes_by_window() {
        let panes = vec![
            with_agent(pane("$3", "@7", "%31"), "claude", Some(AgentState::Done)),
            pane("$3", "@8", "%40"), // different window — excluded
        ];
        let value = tab_json(&window("$3", "@7", 3, true), &panes);
        assert_eq!(value["tab_id"], "w3:t7");
        assert_eq!(value["workspace_id"], "w3");
        assert_eq!(value["label"], "editor");
        assert_eq!(value["number"], 3);
        assert!(value["focused"].as_bool().unwrap());
        assert_eq!(value["pane_count"], 1);
        assert_eq!(value["agent_status"], "done");
    }

    fn test_paths(dir: &std::path::Path) -> crate::paths::Paths {
        let plugins_dir = dir.join("plugins");
        crate::paths::Paths {
            config_dir: dir.join("config"),
            config_file: dir.join("config").join("config.toml"),
            state_dir: dir.join("state"),
            override_dir: dir.join("config").join("agent-detection"),
            remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
            sessions_dir: dir.join("state").join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.join("state").join("logs"),
            manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
        }
    }

    #[test]
    fn from_ids_with_no_ids_requested_returns_an_empty_context_without_touching_tmux() {
        // Real id resolution needs a live tmux server — exercised in tests/tmux_plugin.rs,
        // which has one. This only checks the all-`None` short circuit: it must succeed
        // even though `tmux` here points at a socket with no server running at all.
        let cfg = crate::config::Config::default();
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let tmux = crate::tmux::Tmux::with_socket_name("taw-context-test-no-such-server");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let context = from_ids(&ctx, None, None, None).unwrap();
        assert!(context.workspace.is_none());
        assert!(context.tab.is_none());
        assert!(context.pane.is_none());
        assert!(context.caller_pane_id.is_none());
        assert!(context.extra.is_empty());
    }
}
