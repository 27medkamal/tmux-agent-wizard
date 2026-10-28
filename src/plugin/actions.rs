//! `[[actions]]`: list every enabled plugin's actions, and invoke one synchronously.

use super::{Context, PluginCtx, env, manifest, registry};

/// One action, as listed across every enabled plugin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActionInfo {
    pub plugin_id: String,
    pub action_id: String,
    pub title: Option<String>,
    pub contexts: Vec<String>,
}

/// The result of running an action's command synchronously to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionError {
    #[error("not_found: plugin or action '{0}' not found")]
    NotFound(String),
}

/// Every action from every *enabled* plugin whose manifest still parses and whose
/// `platforms` (if any) allow this platform. A plugin whose manifest fails to load
/// contributes no actions (rather than failing the whole listing).
pub fn list(ctx: &PluginCtx) -> anyhow::Result<Vec<ActionInfo>> {
    let mut out = Vec::new();
    for record in registry::load(ctx.paths)?
        .plugins
        .into_iter()
        .filter(|p| p.enabled)
    {
        let Ok(manifest) = manifest::load(&record.path.join("herdr-plugin.toml")) else {
            continue;
        };
        for action in manifest.matching_actions() {
            out.push(ActionInfo {
                plugin_id: record.id.clone(),
                action_id: action.id.clone(),
                title: action.title.clone(),
                contexts: action.contexts.clone(),
            });
        }
    }
    Ok(out)
}

/// Splits `"plugin.action"` on the *last* `.` — plugin ids may themselves contain dots
/// (`example.layout`), action ids may not (plugins.mdx "Action ids ... may not [contain]
/// dots").
fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    qualified.rsplit_once('.')
}

/// Runs `plugin.action`'s command synchronously (captures stdout/stderr, returns its exit
/// status) with the standard plugin environment (`HERDR_PLUGIN_EVENT=action`) built from
/// `context`. Errors `not_found` (via [`ActionError`]) for a malformed qualified id, an
/// unregistered/disabled plugin, or an unknown/platform-excluded action id.
pub fn invoke(ctx: &PluginCtx, qualified: &str, context: &Context) -> anyhow::Result<InvokeResult> {
    let (plugin_id, action_id) =
        split_qualified(qualified).ok_or_else(|| ActionError::NotFound(qualified.to_string()))?;
    let record = registry::find(ctx.paths, plugin_id)?
        .filter(|r| r.enabled)
        .ok_or_else(|| ActionError::NotFound(qualified.to_string()))?;
    let manifest = manifest::load(&record.path.join("herdr-plugin.toml"))?;
    let action = manifest
        .find_action(action_id)
        .ok_or_else(|| ActionError::NotFound(qualified.to_string()))?;
    let vars = env::build(ctx, &record, "action", context)?;
    let (status, stdout, stderr) = env::run_sync(&record.path, &action.command, &vars)?;
    Ok(InvokeResult {
        status,
        stdout,
        stderr,
    })
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

    fn test_ctx<'a>(
        cfg: &'a crate::config::Config,
        paths: &'a crate::paths::Paths,
        tmux: &'a crate::tmux::Tmux,
        taw_bin: &'a std::path::Path,
    ) -> PluginCtx<'a> {
        PluginCtx {
            tmux,
            cfg,
            paths,
            taw_bin,
            server_env: None,
        }
    }

    #[test]
    fn split_qualified_splits_on_the_last_dot() {
        assert_eq!(
            split_qualified("example.layout.apply"),
            Some(("example.layout", "apply"))
        );
        assert_eq!(
            split_qualified("echo-plugin.dump-env"),
            Some(("echo-plugin", "dump-env"))
        );
        assert_eq!(split_qualified("no-dot"), None);
    }

    #[test]
    fn list_collects_actions_from_every_enabled_plugin_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-actions-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = test_ctx(&cfg, &paths, &tmux, &taw_bin);

        let a_dir = write_plugin(
            dir.path(),
            "a",
            r#"
id = "a"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "one"
title = "One"
contexts = ["pane"]
command = ["true"]
"#,
        );
        let b_dir = write_plugin(
            dir.path(),
            "b",
            r#"
id = "b"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "two"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "a".to_string(),
                source: PluginSource::Link,
                path: a_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        registry::add(
            &paths,
            PluginRecord {
                id: "b".to_string(),
                source: PluginSource::Link,
                path: b_dir,
                enabled: false,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        let actions = list(&ctx).unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].plugin_id, "a");
        assert_eq!(actions[0].action_id, "one");
        assert_eq!(actions[0].title.as_deref(), Some("One"));
        assert_eq!(actions[0].contexts, vec!["pane"]);
    }

    #[test]
    fn invoke_runs_the_action_command_with_env_and_returns_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-actions-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = test_ctx(&cfg, &paths, &tmux, &taw_bin);

        let plugin_dir = write_plugin(
            dir.path(),
            "echo-plugin",
            r#"
id = "echo-plugin"
version = "0.1.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "dump-env"
command = ["sh", "-c", "printf 'id=%s' \"$HERDR_PLUGIN_ID\"; exit 5"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "echo-plugin".to_string(),
                source: PluginSource::Link,
                path: plugin_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        let result = invoke(&ctx, "echo-plugin.dump-env", &Context::default()).unwrap();
        assert_eq!(result.status, 5);
        assert_eq!(result.stdout, "id=echo-plugin");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn invoke_errors_not_found_for_an_unknown_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-actions-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = test_ctx(&cfg, &paths, &tmux, &taw_bin);

        let err = invoke(&ctx, "nope.action", &Context::default()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ActionError>(),
            Some(&ActionError::NotFound("nope.action".to_string()))
        );
    }

    #[test]
    fn invoke_errors_not_found_for_a_disabled_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-actions-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = test_ctx(&cfg, &paths, &tmux, &taw_bin);

        let plugin_dir = write_plugin(
            dir.path(),
            "off",
            r#"
id = "off"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "a"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "off".to_string(),
                source: PluginSource::Link,
                path: plugin_dir,
                enabled: false,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        assert!(invoke(&ctx, "off.a", &Context::default()).is_err());
    }

    #[test]
    fn invoke_errors_not_found_for_an_unknown_action_on_a_known_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-actions-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = test_ctx(&cfg, &paths, &tmux, &taw_bin);

        let plugin_dir = write_plugin(
            dir.path(),
            "a",
            r#"
id = "a"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "real"
command = ["true"]
"#,
        );
        registry::add(
            &paths,
            PluginRecord {
                id: "a".to_string(),
                source: PluginSource::Link,
                path: plugin_dir,
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        let err = invoke(&ctx, "a.missing", &Context::default()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ActionError>(),
            Some(&ActionError::NotFound("a.missing".to_string()))
        );
    }
}
