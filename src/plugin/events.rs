//! `[[events]]`: fires every enabled plugin's matching event hooks, detached, and the
//! `[hooks]` commands of the config, which hear the same events. See
//! `third_party/herdr/reference/event-names.txt` for the event names the watcher/CLI emit.

use super::{Context, PluginCtx, env, manifest, registry};

/// Runs every enabled plugin's `[[events]]` entries whose `on == name`, detached, each
/// with `HERDR_PLUGIN_EVENT=<name>` and `HERDR_PLUGIN_CONTEXT_JSON` built from `payload`:
/// a `workspace`/`tab`/`pane`/`caller_pane_id` key in `payload` becomes that field of the
/// context (and so can surface as `HERDR_WORKSPACE_ID`/`HERDR_TAB_ID`/`HERDR_PANE_ID`, see
/// `env::build`); every other top-level key of `payload` merges into the context's `extra`
/// (and so appears at the top level of the emitted JSON too). Returns the count of hook
/// commands that actually launched. A plugin whose manifest fails to load contributes
/// none; a hook whose env fails to build or whose command fails to spawn is logged (via
/// the `log` crate) and skipped — either way, every *other* plugin's matching hooks still
/// run. One plugin's problem must never silently suppress another's event hooks (matches
/// `startup`'s and `install`'s post-build startup run's per-plugin log-and-continue).
pub fn emit(ctx: &PluginCtx, name: &str, payload: serde_json::Value) -> anyhow::Result<usize> {
    let mut launched = crate::hooks::fire(ctx, name, &payload);
    let event_json = herdr_event_json(name, &payload);
    let context = payload_to_context(payload);
    for record in registry::load(ctx.paths)?
        .plugins
        .into_iter()
        .filter(|p| p.enabled)
    {
        let Ok(manifest) = manifest::load(&record.path.join("herdr-plugin.toml")) else {
            continue;
        };
        let log = super::log_path(ctx, &record.id);
        for hook in manifest.matching_events(name) {
            let vars = match env::build(ctx, &record, name, &context) {
                Ok(mut vars) => {
                    vars.extend(env::event_json_vars(&event_json));
                    vars
                }
                Err(err) => {
                    log::warn!("plugin '{}': building event env failed, {err:#}", record.id);
                    continue;
                }
            };
            match env::spawn_detached(&record.path, &hook.command, &vars, &log) {
                Ok(()) => launched += 1,
                Err(err) => {
                    log::warn!(
                        "plugin '{}': event hook failed to spawn, {err:#}",
                        record.id
                    );
                }
            }
        }
    }
    Ok(launched)
}

/// The fields herdr's `EventData` has for each event (`socket-api-schema.json`), besides `type`: what
/// `HERDR_PLUGIN_EVENT_JSON` holds under `data`.
const EVENT_FIELDS: &[(&str, &[&str])] = &[
    ("workspace.created", &["workspace"]),
    ("workspace.updated", &["workspace"]),
    ("workspace.closed", &["workspace", "workspace_id"]),
    ("workspace.renamed", &["workspace_id", "label"]),
    ("workspace.focused", &["workspace_id"]),
    ("worktree.created", &["workspace", "worktree"]),
    (
        "worktree.opened",
        &["workspace", "worktree", "already_open"],
    ),
    (
        "worktree.removed",
        &["workspace", "workspace_id", "worktree", "forced"],
    ),
    ("tab.created", &["tab"]),
    ("tab.closed", &["tab_id", "workspace_id"]),
    ("tab.renamed", &["tab_id", "workspace_id", "label"]),
    ("tab.focused", &["tab_id", "workspace_id"]),
    ("pane.created", &["pane"]),
    ("pane.updated", &["pane"]),
    ("pane.closed", &["pane_id", "workspace_id"]),
    ("pane.focused", &["pane_id", "workspace_id"]),
    ("pane.exited", &["pane_id", "workspace_id"]),
    (
        "pane.agent_status_changed",
        &[
            "pane_id",
            "workspace_id",
            "agent_status",
            "agent",
            "display_agent",
            "title",
            "state_labels",
        ],
    ),
];

/// `HERDR_PLUGIN_EVENT_JSON`, as herdr builds it: its `EventEnvelope`, `{"event": <kind>, "data":
/// {"type": <kind>, ...}}`, with the kind as herdr writes it (`worktree_created` for `worktree.created`).
/// `data` holds what the payload holds (taw's own keys stay) and each field herdr's event has, taken from
/// the payload's workspace, tab and pane records where the payload does not name it, and `null` where
/// taw has nothing. A worktree event's `worktree` also has herdr's `WorktreeInfo` fields, and its
/// workspace's `worktree` herdr's `WorkspaceWorktreeInfo` (`checkout_path` and the rest), which is
/// where herdr-reviewr reads the new checkout.
pub(crate) fn herdr_event_json(name: &str, payload: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value, json};
    let kind = name.replace('.', "_");
    let mut data: Map<String, Value> = match payload {
        Value::Object(map) => map.clone(),
        Value::Null => Map::new(),
        other => Map::from_iter([("payload".to_string(), other.clone())]),
    };
    data.insert("type".to_string(), Value::String(kind.clone()));
    let records: Vec<Value> = {
        let get = |key: &str| {
            data.get(key)
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(Value::Null)
        };
        let (workspace, tab, pane) = (get("workspace"), get("tab"), get("pane"));
        match name.split('.').next() {
            Some("pane") => vec![pane, tab, workspace],
            Some("tab") => vec![tab, workspace],
            _ => vec![workspace],
        }
    };
    for key in EVENT_FIELDS
        .iter()
        .find(|(event, _)| *event == name)
        .map_or(&[][..], |(_, fields)| fields)
    {
        if data.get(*key).is_some_and(|v| !v.is_null()) {
            continue;
        }
        let found = records
            .iter()
            .find_map(|record| record.get(*key).filter(|v| !v.is_null()))
            .cloned()
            .unwrap_or(Value::Null);
        data.insert((*key).to_string(), found);
    }
    if name.starts_with("worktree.")
        && let Some(Value::Object(record)) = data.get("worktree").cloned()
    {
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
        let path = text("path");
        let repo_root = text("repo_root");
        let mut info = record.clone();
        for (key, value) in [
            ("label", json!(text("branch"))),
            ("is_bare", json!(false)),
            ("is_detached", json!(false)),
            ("is_prunable", json!(false)),
            ("is_linked_worktree", json!(true)),
            ("open_workspace_id", json!(text("workspace_id"))),
        ] {
            info.entry(key).or_insert(value);
        }
        data.insert("worktree".to_string(), Value::Object(info));
        if let Some(Value::Object(workspace)) = data.get_mut("workspace") {
            let repo_name = repo_root
                .as_deref()
                .and_then(|root| std::path::Path::new(root).file_name())
                .map(|name| name.to_string_lossy().into_owned());
            workspace.entry("worktree").or_insert_with(|| {
                json!({
                    "repo_key": repo_root,
                    "repo_name": repo_name,
                    "repo_root": repo_root,
                    "checkout_path": path,
                    "is_linked_worktree": true,
                })
            });
        }
    }
    json!({ "event": kind, "data": data })
}

/// Whether any enabled plugin, or the config's `[hooks]`, hooks `name`.
///
/// The `window-layout-changed` / `after-select-pane` hooks fire this module's events very
/// often — every resize, every focus change — and building a payload means reading tmux.
/// This is the cheap question to ask first: one registry read plus each plugin's manifest,
/// and no tmux calls at all, so a server with no plugin subscribed to an event pays
/// almost nothing for having the hooks installed.
pub fn any_hook(ctx: &PluginCtx, name: &str) -> anyhow::Result<bool> {
    if crate::hooks::any_hook(ctx.cfg, name) {
        return Ok(true);
    }
    for record in registry::load(ctx.paths)?
        .plugins
        .into_iter()
        .filter(|p| p.enabled)
    {
        let Ok(manifest) = manifest::load(&record.path.join("herdr-plugin.toml")) else {
            continue;
        };
        if manifest.matching_events(name).next().is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The plugin [`Context`] an event payload makes: `workspace`/`tab`/`pane`/`caller_pane_id`
/// keys become its fields, a bare id becomes a stub record, and every other key is `extra`.
pub(crate) fn payload_to_context(payload: serde_json::Value) -> Context {
    let mut context = Context::default();
    if let serde_json::Value::Object(mut map) = payload {
        context.workspace = map.remove("workspace");
        context.tab = map.remove("tab");
        context.pane = map.remove("pane");
        context.caller_pane_id = map
            .remove("caller_pane_id")
            .and_then(|v| v.as_str().map(str::to_string));
        // A "gone" event (`tab.closed`, `workspace.closed`, `pane.exited`, …) carries only
        // the id: the thing it names no longer exists, so there is no record to attach.
        // herdr still fills its plugin invocation context's ids straight from that event
        // data, so a stub in the matching slot is what keeps `HERDR_TAB_ID`/
        // `HERDR_WORKSPACE_ID`/`HERDR_PANE_ID` set for those hooks — `env::build` reads
        // them out of the record with `extract_id`. A real record already in the slot always
        // wins: `pane.created` carries the whole pane, and a stub must never replace it.
        for (slot, key) in [
            (&mut context.workspace, "workspace_id"),
            (&mut context.tab, "tab_id"),
            (&mut context.pane, "pane_id"),
        ] {
            if slot.is_none()
                && let Some(id) = map.get(key).and_then(serde_json::Value::as_str)
            {
                *slot = Some(serde_json::json!({ key: id }));
            }
        }
        context.extra = map;
    } else if !payload.is_null() {
        context.extra.insert("payload".to_string(), payload);
    }
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::registry::{PluginRecord, PluginSource};

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

    fn write_plugin(dir: &std::path::Path, id: &str, toml: &str) -> std::path::PathBuf {
        let plugin_dir = dir.join(id);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("herdr-plugin.toml"), toml).unwrap();
        plugin_dir
    }

    #[test]
    fn payload_to_context_pulls_known_keys_and_merges_the_rest_as_extra() {
        let payload = serde_json::json!({
            "pane": {"pane_id": "w1:p2"},
            "caller_pane_id": "%2",
            "state": "blocked",
        });
        let context = payload_to_context(payload);
        assert_eq!(context.pane, Some(serde_json::json!({"pane_id": "w1:p2"})));
        assert_eq!(context.caller_pane_id.as_deref(), Some("%2"));
        assert!(context.workspace.is_none());
        assert_eq!(
            context.extra.get("state"),
            Some(&serde_json::Value::String("blocked".to_string()))
        );
    }

    #[test]
    fn payload_to_context_synthesizes_the_record_slots_from_bare_ids() {
        // `tab.closed`/`workspace.closed`/`pane.exited` carry only ids — the object they
        // name is already gone, so there is no record to attach. herdr still fills the
        // invocation context's ids from that event data, so `HERDR_TAB_ID` and friends
        // must be set here too: a stub `{"<x>_id": …}` in the matching slot is what
        // `env::build`'s `extract_id` reads.
        let payload = serde_json::json!({
            "tab_id": "w1:t2",
            "workspace_id": "w1",
            "pane_id": "w1:p3",
        });
        let context = payload_to_context(payload);
        assert_eq!(context.tab, Some(serde_json::json!({"tab_id": "w1:t2"})));
        assert_eq!(
            context.workspace,
            Some(serde_json::json!({"workspace_id": "w1"}))
        );
        assert_eq!(context.pane, Some(serde_json::json!({"pane_id": "w1:p3"})));
        // The ids stay at the top level of the emitted JSON too: that is herdr's own
        // event-data shape, which a plugin reading HERDR_PLUGIN_CONTEXT_JSON switches on.
        assert_eq!(
            context.extra.get("tab_id").and_then(|v| v.as_str()),
            Some("w1:t2")
        );
        assert_eq!(
            context.extra.get("workspace_id").and_then(|v| v.as_str()),
            Some("w1")
        );
    }

    #[test]
    fn payload_to_context_prefers_a_real_record_over_the_id_stub() {
        // `pane.created` carries the whole record; a bare `pane_id` alongside it must not
        // replace it with a stub that has none of the pane's fields.
        let payload = serde_json::json!({
            "pane": {"pane_id": "w1:p1", "agent_status": "idle"},
            "pane_id": "w1:p1",
        });
        let context = payload_to_context(payload);
        assert_eq!(
            context.pane,
            Some(serde_json::json!({"pane_id": "w1:p1", "agent_status": "idle"}))
        );
    }

    /// herdr's envelope: the kind in herdr's spelling, `data.type` the same, and the fields herdr's event has,
    /// from the payload's records where the payload does not name them.
    #[test]
    fn the_event_json_is_herdrs_envelope_with_its_fields_filled_from_the_records() {
        let tab_closed = herdr_event_json(
            "tab.closed",
            &serde_json::json!({"tab": {"tab_id": "w1:t2", "workspace_id": "w1"}}),
        );
        assert_eq!(tab_closed["event"], "tab_closed");
        assert_eq!(tab_closed["data"]["type"], "tab_closed");
        assert_eq!(tab_closed["data"]["tab_id"], "w1:t2");
        assert_eq!(tab_closed["data"]["workspace_id"], "w1");

        // A field the payload has no word for is null, and a payload's own key stays.
        let renamed = herdr_event_json(
            "workspace.renamed",
            &serde_json::json!({"workspace": {"workspace_id": "w3"}, "extra": 1}),
        );
        assert_eq!(renamed["data"]["workspace_id"], "w3");
        assert!(renamed["data"]["label"].is_null(), "{renamed}");
        assert_eq!(renamed["data"]["extra"], 1);

        let status = herdr_event_json(
            "pane.agent_status_changed",
            &serde_json::json!({
                "pane": {"pane_id": "w1:p4", "workspace_id": "w1", "agent": "claude", "title": null},
                "agent_status": "blocked",
            }),
        );
        for (key, want) in [
            ("pane_id", serde_json::json!("w1:p4")),
            ("workspace_id", serde_json::json!("w1")),
            ("agent_status", serde_json::json!("blocked")),
            ("agent", serde_json::json!("claude")),
            ("title", serde_json::Value::Null),
            ("display_agent", serde_json::Value::Null),
            ("state_labels", serde_json::Value::Null),
        ] {
            assert_eq!(status["data"][key], want, "{key}: {status}");
        }

        // Not an event herdr has: kept as it is, with its type.
        let own = herdr_event_json("agent.done", &serde_json::json!({"state": "done"}));
        assert_eq!(own["event"], "agent_done");
        assert_eq!(own["data"]["state"], "done");
    }

    /// herdr-reviewr reads the new checkout from `/data/worktree/path` and, failing that,
    /// `/data/workspace/worktree/checkout_path`.
    #[test]
    fn a_worktree_events_json_names_the_checkout_where_herdr_reviewr_reads_it() {
        let json = herdr_event_json(
            "worktree.created",
            &serde_json::json!({
                "workspace": {"workspace_id": "w2", "label": "demo--wt"},
                "worktree": {
                    "workspace_id": "w2", "path": "/src/demo--wt", "branch": "wt",
                    "repo_root": "/src/demo", "session_name": "demo--wt",
                },
            }),
        );
        assert_eq!(json["data"]["worktree"]["path"], "/src/demo--wt");
        assert_eq!(json["data"]["worktree"]["label"], "wt");
        assert_eq!(json["data"]["worktree"]["is_linked_worktree"], true);
        assert_eq!(json["data"]["worktree"]["is_bare"], false);
        let on_workspace = &json["data"]["workspace"]["worktree"];
        assert_eq!(on_workspace["checkout_path"], "/src/demo--wt");
        assert_eq!(on_workspace["repo_root"], "/src/demo");
        assert_eq!(on_workspace["repo_name"], "demo");
        assert_eq!(on_workspace["is_linked_worktree"], true);
    }

    /// An event hook's process gets the envelope in `HERDR_PLUGIN_EVENT_JSON`; a startup hook does not.
    #[test]
    fn an_event_hook_gets_the_event_json_in_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-events-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let out = dir.path().join("event-json");
        let plugin_dir = write_plugin(
            dir.path(),
            "dump",
            &format!(
                r#"
id = "dump"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "worktree.created"
command = ["sh", "-c", 'printf "%s" "$HERDR_PLUGIN_EVENT_JSON" > "{0}.part" && mv "{0}.part" "{0}"']
"#,
                out.display()
            ),
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "dump".to_string(),
                source: PluginSource::Link,
                path: plugin_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        let payload = serde_json::json!({
            "workspace": {"workspace_id": "w2"},
            "worktree": {"path": "/src/demo--wt", "repo_root": "/src/demo", "branch": "wt"},
        });
        assert_eq!(emit(&ctx, "worktree.created", payload).unwrap(), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&out).expect("the hook never wrote the variable");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["event"], "worktree_created", "{text}");
        assert_eq!(json["data"]["worktree"]["path"], "/src/demo--wt", "{text}");
        assert_eq!(
            json["data"]["workspace"]["worktree"]["checkout_path"], "/src/demo--wt",
            "{text}"
        );
    }

    #[test]
    fn emit_runs_only_matching_event_hooks_on_enabled_plugins_and_counts_them() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-events-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let marker = dir.path().join("fired");
        let matching_dir = write_plugin(
            dir.path(),
            "matching",
            &format!(
                r#"
id = "matching"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.agent_status_changed"
command = ["sh", "-c", "touch {marker}"]
"#,
                marker = marker.display(),
            ),
        );
        let other_event_dir = write_plugin(
            dir.path(),
            "other-event",
            r#"
id = "other-event"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.created"
command = ["true"]
"#,
        );
        let disabled_dir = write_plugin(
            dir.path(),
            "disabled",
            r#"
id = "disabled"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.agent_status_changed"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "matching".to_string(),
                source: PluginSource::Link,
                path: matching_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        registry::add(
            &paths,
            PluginRecord {
                id: "other-event".to_string(),
                source: PluginSource::Link,
                path: other_event_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        registry::add(
            &paths,
            PluginRecord {
                id: "disabled".to_string(),
                source: PluginSource::Link,
                path: disabled_dir,
                enabled: false,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        let launched = emit(&ctx, "pane.agent_status_changed", serde_json::json!({})).unwrap();
        assert_eq!(launched, 1);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "matching plugin's event hook did not run");
    }

    #[test]
    fn emit_continues_past_one_plugins_spawn_failure_and_still_fires_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-events-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let marker = dir.path().join("healthy-fired");
        // A command whose *program* does not exist, so `spawn_detached`'s `.spawn()` call
        // itself fails (ENOENT) rather than merely exiting non-zero.
        let failing_dir = write_plugin(
            dir.path(),
            "failing",
            r#"
id = "failing"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.agent_status_changed"
command = ["/definitely/does/not/exist/taw-test-binary-xyz"]
"#,
        );
        let healthy_dir = write_plugin(
            dir.path(),
            "healthy",
            &format!(
                r#"
id = "healthy"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.agent_status_changed"
command = ["sh", "-c", "touch {marker}"]
"#,
                marker = marker.display(),
            ),
        );
        // Register the failing plugin *first*: before this fix, the early `?` on
        // `spawn_detached` would have aborted `emit` before ever reaching "healthy".
        registry::add(
            &paths,
            PluginRecord {
                id: "failing".to_string(),
                source: PluginSource::Link,
                path: failing_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        registry::add(
            &paths,
            PluginRecord {
                id: "healthy".to_string(),
                source: PluginSource::Link,
                path: healthy_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        let launched = emit(&ctx, "pane.agent_status_changed", serde_json::json!({})).unwrap();
        assert_eq!(
            launched, 1,
            "only the healthy plugin's hook should count as launched"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "healthy plugin's event hook did not run after the failing plugin's hook"
        );
    }

    #[test]
    fn any_hook_answers_without_touching_tmux_and_only_for_enabled_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        // A socket name no server listens on: `any_hook` must not make a tmux call, so
        // this staying unused is itself part of what the test proves.
        let tmux = crate::tmux::Tmux::with_socket_name("taw-any-hook-test-no-server");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        assert!(!any_hook(&ctx, "layout.updated").unwrap());

        let disabled_dir = write_plugin(
            dir.path(),
            "disabled",
            r#"
id = "disabled"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "layout.updated"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "disabled".to_string(),
                source: PluginSource::Link,
                path: disabled_dir,
                enabled: false,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        assert!(
            !any_hook(&ctx, "layout.updated").unwrap(),
            "a disabled plugin must not make an event look subscribed"
        );

        let enabled_dir = write_plugin(
            dir.path(),
            "enabled",
            r#"
id = "enabled"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "layout.updated"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "enabled".to_string(),
                source: PluginSource::Link,
                path: enabled_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        assert!(any_hook(&ctx, "layout.updated").unwrap());
        assert!(!any_hook(&ctx, "pane.focused").unwrap());
    }

    fn config_with_hooks(toml: &str) -> crate::config::Config {
        toml::from_str(toml).expect("the test config parses")
    }

    #[test]
    fn emit_counts_the_config_hooks_it_started_beside_the_plugin_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = config_with_hooks("[hooks]\n\"tab.focused\" = [\"true\", \"true\"]\n");
        let tmux = crate::tmux::Tmux::with_socket_name("taw-events-test-unused");
        // The supervisor is a script that runs nothing, which is all a count needs. It has to
        // be a real executable: a hook whose `taw` is not one is not started, and is not
        // counted (`/bin/true` is not on every system).
        let taw_bin = dir.path().join("taw");
        std::fs::write(&taw_bin, "#!/bin/sh\nexit 0\n").unwrap();
        crate::test_exec::make_executable(&taw_bin);
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let launched = emit(&ctx, "tab.focused", serde_json::json!({})).unwrap();
        assert_eq!(launched, 2);
        assert_eq!(emit(&ctx, "tab.closed", serde_json::json!({})).unwrap(), 0);
    }

    #[test]
    fn an_event_only_the_config_hooks_is_hooked_and_so_is_a_status_change_while_an_agent_event_is()
    {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = config_with_hooks(
            "[hooks]\n\"layout.updated\" = \"true\"\n\"agent.blocked\" = \"true\"\n",
        );
        // A socket no server listens on: the question must not reach tmux.
        let tmux = crate::tmux::Tmux::with_socket_name("taw-any-hook-test-no-server");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        assert!(any_hook(&ctx, "layout.updated").unwrap());
        // The watcher emits one event for every state change, and `agent.blocked` is one.
        assert!(any_hook(&ctx, "pane.agent_status_changed").unwrap());
        assert!(!any_hook(&ctx, "pane.focused").unwrap());
        assert!(!any_hook(&ctx, "agent.done").unwrap());
    }

    #[test]
    fn emit_returns_zero_when_no_plugin_has_a_matching_hook() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-events-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let launched = emit(&ctx, "workspace.created", serde_json::json!({})).unwrap();
        assert_eq!(launched, 0);
    }
}
