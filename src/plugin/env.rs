//! The environment injected into every plugin process, and the two ways a plugin command
//! actually runs: synchronously with captured output (actions), or detached with output
//! appended to the plugin's log file (startup hooks, event hooks).

use anyhow::Context as _;
use std::path::{Path, PathBuf};

use super::{Context, PluginCtx, registry::PluginRecord};

/// Builds the full `HERDR_*` + `TAW_*` environment for a plugin process.
///
/// `event` is the literal value of `HERDR_PLUGIN_EVENT`: `"startup"` for startup hooks,
/// `"action"` for action invocations and plugin-pane opens, or the event name (e.g.
/// `"pane.agent_status_changed"`) for event hooks. `HERDR_PANE_ID`/`HERDR_TAB_ID`/
/// `HERDR_WORKSPACE_ID` are set only when `context.pane`/`.tab`/`.workspace` are present
/// and carry the matching `*_id` key (as produced by `context::pane_json` etc.).
/// `HERDR_SOCKET_PATH` is never set — plugins that need the raw socket transport are
/// unsupported (see spec).
///
/// Creates `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_PLUGIN_STATE_DIR` if they do not exist yet.
pub fn build(
    ctx: &PluginCtx,
    plugin: &PluginRecord,
    event: &str,
    context: &Context,
) -> anyhow::Result<Vec<(String, String)>> {
    let config_dir = super::config_dir(ctx, &plugin.id)?;
    let state_dir = plugin_state_dir(ctx, &plugin.id)?;

    let mut vars: Vec<(String, String)> = Vec::new();
    push(
        &mut vars,
        "BIN_PATH",
        absolute_path(ctx.taw_bin)?.to_string_lossy().into_owned(),
    );
    push(&mut vars, "ENV", "1".to_string());
    push(&mut vars, "PLUGIN_ID", plugin.id.clone());
    push(
        &mut vars,
        "PLUGIN_ROOT",
        plugin.path.to_string_lossy().into_owned(),
    );
    push(
        &mut vars,
        "PLUGIN_CONFIG_DIR",
        config_dir.to_string_lossy().into_owned(),
    );
    push(
        &mut vars,
        "PLUGIN_STATE_DIR",
        state_dir.to_string_lossy().into_owned(),
    );
    push(
        &mut vars,
        "PLUGIN_CONTEXT_JSON",
        serde_json::to_string(&context_json(context))?,
    );
    push(&mut vars, "PLUGIN_EVENT", event.to_string());
    if let Some(id) = extract_id(&context.pane, "pane_id") {
        push(&mut vars, "PANE_ID", id);
    }
    // The ids of the pane's tab and workspace are the pane's own (`context::from_ids` fills
    // both records for a pane, but a context built another way may carry the pane only).
    if let Some(id) =
        extract_id(&context.tab, "tab_id").or_else(|| extract_id(&context.pane, "tab_id"))
    {
        push(&mut vars, "TAB_ID", id);
    }
    if let Some(id) = extract_id(&context.workspace, "workspace_id")
        .or_else(|| extract_id(&context.tab, "workspace_id"))
        .or_else(|| extract_id(&context.pane, "workspace_id"))
    {
        push(&mut vars, "WORKSPACE_ID", id);
    }
    Ok(vars)
}

/// `HERDR_PLUGIN_EVENT_JSON` and its `TAW_` twin for an event hook: herdr's event envelope as one line of
/// JSON ([`super::events::herdr_event_json`]).
pub fn event_json_vars(event_json: &serde_json::Value) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    push(&mut vars, "PLUGIN_EVENT_JSON", event_json.to_string());
    vars
}

/// Pushes both `HERDR_<key>` and its `TAW_<key>` twin with the same value.
fn push(vars: &mut Vec<(String, String)>, key: &str, value: String) {
    vars.push((format!("HERDR_{key}"), value.clone()));
    vars.push((format!("TAW_{key}"), value));
}

fn extract_id(value: &Option<serde_json::Value>, key: &str) -> Option<String> {
    value.as_ref()?.get(key)?.as_str().map(str::to_string)
}

/// The value of `HERDR_PLUGIN_CONTEXT_JSON`: herdr's flat `PluginInvocationContext` fields
/// (`workspace_id`, `tab_id`, `focused_pane_id`, `focused_pane_cwd`, `workspace_cwd`, ... each
/// `null` where taw has nothing), taw's nested `{"workspace", "tab", "pane", "caller_pane_id"}`
/// beside them, and `context.extra`'s keys merged at the top level (they win).
pub(crate) fn context_json(context: &Context) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let field = |value: &Option<serde_json::Value>, key: &str| {
        value
            .as_ref()
            .and_then(|v| v.get(key))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    let first = |candidates: [serde_json::Value; 3]| {
        candidates
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(serde_json::Value::Null)
    };
    let flat = [
        (
            "workspace_id",
            first([
                field(&context.workspace, "workspace_id"),
                field(&context.tab, "workspace_id"),
                field(&context.pane, "workspace_id"),
            ]),
        ),
        (
            "tab_id",
            first([
                field(&context.tab, "tab_id"),
                field(&context.pane, "tab_id"),
                serde_json::Value::Null,
            ]),
        ),
        ("focused_pane_id", field(&context.pane, "pane_id")),
        ("focused_pane_cwd", field(&context.pane, "cwd")),
        ("focused_pane_agent", field(&context.pane, "agent")),
        ("focused_pane_status", field(&context.pane, "agent_status")),
        ("workspace_cwd", field(&context.workspace, "cwd")),
        ("workspace_label", field(&context.workspace, "label")),
        ("tab_label", field(&context.tab, "label")),
        ("worktree", serde_json::Value::Null),
        ("selected_text", serde_json::Value::Null),
        ("clicked_url", serde_json::Value::Null),
        ("link_handler_id", serde_json::Value::Null),
        ("invocation_source", serde_json::Value::Null),
        ("correlation_id", serde_json::Value::Null),
    ];
    for (key, value) in flat {
        map.insert(key.to_string(), value);
    }
    map.insert(
        "workspace".to_string(),
        context.workspace.clone().unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "tab".to_string(),
        context.tab.clone().unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "pane".to_string(),
        context.pane.clone().unwrap_or(serde_json::Value::Null),
    );
    map.insert(
        "caller_pane_id".to_string(),
        context
            .caller_pane_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    );
    for (k, v) in &context.extra {
        map.insert(k.clone(), v.clone());
    }
    serde_json::Value::Object(map)
}

fn plugin_state_dir(ctx: &PluginCtx, id: &str) -> anyhow::Result<PathBuf> {
    let dir = ctx.paths.plugin_state_dir.join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn absolute_path(p: &Path) -> anyhow::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// Environment variable names that must never reach a plugin-provided subprocess, even
/// when inherited from `taw`'s own environment: herdr's live IPC socket path and its
/// `TAW_` twin. A `taw` launched inside a real herdr session must never leak this to any
/// plugin process — see [`strip_socket_env`].
const SOCKET_ENV_VARS: &[&str] = &["HERDR_SOCKET_PATH", "TAW_SOCKET_PATH"];

/// Removes [`SOCKET_ENV_VARS`] from `cmd`'s environment. Every subprocess this crate spawns
/// on a plugin's behalf calls this before adding its own env and running: [`run_sync`],
/// [`spawn_detached`], and `install::run_build_commands` (build commands otherwise inherit
/// the ambient process environment untouched, per plugins.mdx "Build commands ... do not
/// receive runtime plugin context or Herdr socket env" — that "or Herdr socket env" applies
/// here too, so build commands strip the same names).
pub fn strip_socket_env(cmd: &mut std::process::Command) {
    for var in SOCKET_ENV_VARS {
        cmd.env_remove(var);
    }
}

/// The variables that name the pane, tab and workspace a plugin was invoked for, and their `TAW_`
/// twins. A plugin process has them only from its invocation's context ([`build`]), never from
/// the process that starts it: with `@agent-wizard-herdr-env` on, tmux's global environment holds
/// `HERDR_PANE_ID=%calling` (and a run-shell job, which is what starts a plugin's startup hook,
/// inherits it), and a plugin that reads a pane id there is told of a pane it was not given.
pub const PANE_CONTEXT_VARS: [&str; 6] = [
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "TAW_PANE_ID",
    "TAW_TAB_ID",
    "TAW_WORKSPACE_ID",
];

/// Removes [`PANE_CONTEXT_VARS`] from `cmd`'s environment, so that what a caller then sets with
/// `envs` is all there is. Called wherever [`strip_socket_env`] is, and for `[hooks]` commands
/// (`HookEnv::apply`), which inherit tmux's global environment too.
pub fn strip_pane_context_env(cmd: &mut std::process::Command) {
    for var in PANE_CONTEXT_VARS {
        cmd.env_remove(var);
    }
}

/// Runs `command` (an argv array; `command[0]` is the program) synchronously in
/// `plugin_dir` with `vars` as its environment (merged over the inherited process
/// environment, matching herdr: "build and runtime commands ... inherit your
/// environment" — except [`SOCKET_ENV_VARS`], explicitly stripped even if inherited: a
/// `taw` launched inside a real herdr session must never leak herdr's live socket path to a
/// plugin process), capturing stdout/stderr. Returns `(exit status, stdout, stderr)`; the
/// exit status is `-1` when the process was terminated by a signal rather than exiting
/// normally. Used by `actions::invoke`.
pub fn run_sync(
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
) -> anyhow::Result<(i32, String, String)> {
    let (prog, args) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("plugin command is empty"))?;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args).current_dir(plugin_dir);
    strip_socket_env(&mut cmd);
    strip_pane_context_env(&mut cmd);
    let output = cmd
        .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .with_context(|| format!("running `{}`", shell_words::join(command)))?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Spawns `command` detached in `plugin_dir` with `vars` as its environment (see
/// [`run_sync`] on [`SOCKET_ENV_VARS`] stripping — the same applies here): stdin is
/// closed, stdout/stderr are appended to `log_path` (created if needed), and this function
/// returns as soon as the process has started — it never waits for it to exit. Used by
/// startup hooks, event hooks, and plugin-pane-adjacent bookkeeping never waits on plugin
/// processes either.
pub fn spawn_detached(
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    log_path: &Path,
) -> anyhow::Result<()> {
    spawn(plugin_dir, command, vars, log_path, false).map(|_| ())
}

/// [`spawn_detached`] for a command that is to be ended later: it leads a process group of its own,
/// and its pid (the group's id) is returned for [`super::running::record`].
pub fn spawn_detached_group(
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    log_path: &Path,
) -> anyhow::Result<u32> {
    spawn(plugin_dir, command, vars, log_path, true)
}

fn spawn(
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    log_path: &Path,
    own_group: bool,
) -> anyhow::Result<u32> {
    let (prog, args) = command
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("plugin command is empty"))?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let log_err = log_out
        .try_clone()
        .with_context(|| format!("duplicating log file handle for {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args).current_dir(plugin_dir);
    strip_socket_env(&mut cmd);
    strip_pane_context_env(&mut cmd);
    if own_group {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let child = cmd
        .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(log_out)
        .stderr(log_err)
        .spawn()
        .with_context(|| format!("spawning `{}`", shell_words::join(command)))?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::registry::PluginSource;
    use crate::plugin::tests_support::SOCKET_ENV_LOCK;
    use std::path::PathBuf;

    fn test_paths(dir: &Path) -> crate::paths::Paths {
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

    fn test_record(path: PathBuf) -> PluginRecord {
        PluginRecord {
            id: "echo-plugin".to_string(),
            source: PluginSource::Link,
            path,
            enabled: true,
            installed_unix_ms: 0,
        }
    }

    fn get<'a>(vars: &'a [(String, String)], key: &str) -> Option<&'a str> {
        vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn build_sets_every_documented_herdr_var_and_its_taw_twin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-env-test-unused");
        let taw_bin = PathBuf::from("/usr/local/bin/taw");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let record = test_record(dir.path().join("plugin-root"));

        let vars = build(&ctx, &record, "startup", &Context::default()).unwrap();

        assert_eq!(get(&vars, "HERDR_BIN_PATH"), Some("/usr/local/bin/taw"));
        assert_eq!(get(&vars, "TAW_BIN_PATH"), Some("/usr/local/bin/taw"));
        assert_eq!(get(&vars, "HERDR_ENV"), Some("1"));
        assert_eq!(get(&vars, "TAW_ENV"), Some("1"));
        assert_eq!(get(&vars, "HERDR_PLUGIN_ID"), Some("echo-plugin"));
        assert_eq!(get(&vars, "TAW_PLUGIN_ID"), Some("echo-plugin"));
        assert_eq!(
            get(&vars, "HERDR_PLUGIN_ROOT"),
            Some(dir.path().join("plugin-root").to_str().unwrap())
        );
        assert!(
            get(&vars, "HERDR_PLUGIN_CONFIG_DIR")
                .unwrap()
                .ends_with("plugins/echo-plugin")
        );
        assert!(
            get(&vars, "TAW_PLUGIN_CONFIG_DIR")
                .unwrap()
                .ends_with("plugins/echo-plugin")
        );
        assert!(
            get(&vars, "HERDR_PLUGIN_STATE_DIR")
                .unwrap()
                .contains("plugins/state/echo-plugin")
        );
        assert!(
            get(&vars, "TAW_PLUGIN_STATE_DIR")
                .unwrap()
                .contains("plugins/state/echo-plugin")
        );
        assert_eq!(get(&vars, "HERDR_PLUGIN_EVENT"), Some("startup"));
        assert_eq!(get(&vars, "TAW_PLUGIN_EVENT"), Some("startup"));
        assert!(get(&vars, "HERDR_PLUGIN_CONTEXT_JSON").is_some());
        assert!(get(&vars, "TAW_PLUGIN_CONTEXT_JSON").is_some());
    }

    #[test]
    fn build_never_sets_herdr_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-env-test-unused");
        let taw_bin = PathBuf::from("/usr/local/bin/taw");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let record = test_record(dir.path().join("plugin-root"));

        let vars = build(&ctx, &record, "action", &Context::default()).unwrap();

        assert!(
            vars.iter()
                .all(|(k, _)| k != "HERDR_SOCKET_PATH" && k != "TAW_SOCKET_PATH")
        );
    }

    #[test]
    fn build_creates_config_and_state_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-env-test-unused");
        let taw_bin = PathBuf::from("/usr/local/bin/taw");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let record = test_record(dir.path().join("plugin-root"));

        build(&ctx, &record, "action", &Context::default()).unwrap();

        assert!(
            paths
                .config_dir
                .join("plugins")
                .join("echo-plugin")
                .is_dir()
        );
        assert!(paths.plugin_state_dir.join("echo-plugin").is_dir());
    }

    #[test]
    fn build_resolves_a_relative_taw_bin_against_the_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-env-test-unused");
        let taw_bin = PathBuf::from("taw"); // relative
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let record = test_record(dir.path().join("plugin-root"));

        let vars = build(&ctx, &record, "action", &Context::default()).unwrap();

        let bin_path = get(&vars, "HERDR_BIN_PATH").unwrap();
        assert!(
            Path::new(bin_path).is_absolute(),
            "expected an absolute path, got {bin_path}"
        );
    }

    #[test]
    fn build_sets_pane_tab_workspace_ids_only_when_the_context_carries_them() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-env-test-unused");
        let taw_bin = PathBuf::from("/usr/local/bin/taw");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let record = test_record(dir.path().join("plugin-root"));

        let empty_vars = build(&ctx, &record, "action", &Context::default()).unwrap();
        assert!(get(&empty_vars, "HERDR_PANE_ID").is_none());
        assert!(get(&empty_vars, "HERDR_TAB_ID").is_none());
        assert!(get(&empty_vars, "HERDR_WORKSPACE_ID").is_none());

        let context = Context {
            pane: Some(serde_json::json!({"pane_id": "w3:p31"})),
            tab: Some(serde_json::json!({"tab_id": "w3:t7"})),
            workspace: Some(serde_json::json!({"workspace_id": "w3"})),
            caller_pane_id: Some("%31".to_string()),
            extra: serde_json::Map::new(),
        };
        let vars = build(&ctx, &record, "action", &context).unwrap();
        assert_eq!(get(&vars, "HERDR_PANE_ID"), Some("w3:p31"));
        assert_eq!(get(&vars, "TAW_PANE_ID"), Some("w3:p31"));
        assert_eq!(get(&vars, "HERDR_TAB_ID"), Some("w3:t7"));
        assert_eq!(get(&vars, "HERDR_WORKSPACE_ID"), Some("w3"));

        let context_json: serde_json::Value =
            serde_json::from_str(get(&vars, "HERDR_PLUGIN_CONTEXT_JSON").unwrap()).unwrap();
        assert_eq!(context_json["pane"]["pane_id"], "w3:p31");
        assert_eq!(context_json["caller_pane_id"], "%31");
    }

    #[test]
    fn context_json_merges_extra_keys_at_the_top_level() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "clicked_url".to_string(),
            serde_json::Value::String("https://example.com".to_string()),
        );
        let context = Context {
            extra,
            ..Context::default()
        };

        let value = context_json(&context);

        assert_eq!(value["clicked_url"], "https://example.com");
        assert!(value["workspace"].is_null());
        assert!(value["pane"].is_null());
    }

    #[test]
    fn run_sync_captures_status_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo out; echo err >&2; exit 7".to_string(),
        ];
        let (status, stdout, stderr) = run_sync(dir.path(), &command, &[]).unwrap();
        assert_eq!(status, 7);
        assert_eq!(stdout.trim(), "out");
        assert_eq!(stderr.trim(), "err");
    }

    #[test]
    fn run_sync_passes_the_given_env_and_cwd_to_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s' \"$FOO\" \"$PWD\"".to_string(),
        ];
        let vars = vec![("FOO".to_string(), "bar".to_string())];
        let (status, stdout, _) = run_sync(dir.path(), &command, &vars).unwrap();
        assert_eq!(status, 0);
        let canonical_dir = dir.path().canonicalize().unwrap();
        let (foo, pwd) = stdout.split_once('|').unwrap();
        assert_eq!(foo, "bar");
        assert_eq!(Path::new(pwd).canonicalize().unwrap(), canonical_dir);
    }

    #[test]
    fn spawn_detached_returns_immediately_and_appends_output_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("state").join("plugin.log");
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo from-detached".to_string(),
        ];

        spawn_detached(dir.path(), &command, &[], &log_path).unwrap();

        // Poll briefly: the process is detached, so completion isn't synchronous.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut contents = String::new();
        while std::time::Instant::now() < deadline {
            if let Ok(c) = std::fs::read_to_string(&log_path)
                && c.contains("from-detached")
            {
                contents = c;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(contents.contains("from-detached"), "log file: {contents:?}");
    }

    #[test]
    fn spawn_detached_appends_across_multiple_calls_rather_than_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("plugin.log");
        std::fs::write(&log_path, "existing\n").unwrap();

        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo appended".to_string(),
        ];
        spawn_detached(dir.path(), &command, &[], &log_path).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut contents = String::new();
        while std::time::Instant::now() < deadline {
            contents = std::fs::read_to_string(&log_path).unwrap_or_default();
            if contents.contains("appended") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(contents.contains("existing"), "log file: {contents:?}");
        assert!(contents.contains("appended"), "log file: {contents:?}");
    }

    #[test]
    fn run_sync_strips_herdr_socket_path_even_when_inherited_from_the_parent_process() {
        let _guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by SOCKET_ENV_LOCK (shared with install.rs's build-command
        // counterpart — same two var names, one lock); nothing else in this process
        // reads/writes these two specific var names while the guard is held.
        unsafe {
            std::env::set_var("HERDR_SOCKET_PATH", "/tmp/should-not-leak.sock");
            std::env::set_var("TAW_SOCKET_PATH", "/tmp/should-not-leak-taw.sock");
        }
        let dir = tempfile::tempdir().unwrap();
        let command = vec!["sh".to_string(), "-c".to_string(), "env".to_string()];
        let result = run_sync(dir.path(), &command, &[]);
        // SAFETY: still serialized by SOCKET_ENV_LOCK.
        unsafe {
            std::env::remove_var("HERDR_SOCKET_PATH");
            std::env::remove_var("TAW_SOCKET_PATH");
        }
        let (_, stdout, _) = result.unwrap();
        assert!(
            !stdout.contains("HERDR_SOCKET_PATH"),
            "HERDR_SOCKET_PATH leaked into the child's environment:\n{stdout}"
        );
        assert!(
            !stdout.contains("TAW_SOCKET_PATH"),
            "TAW_SOCKET_PATH leaked into the child's environment:\n{stdout}"
        );
    }

    /// The pane, tab and workspace ids of a plugin process come from its invocation's context and
    /// from nowhere else: what the starting process has (`HERDR_PANE_ID=%calling` from tmux's global
    /// environment, with `@agent-wizard-herdr-env` on) is taken out, and what the caller passes stays.
    #[test]
    fn run_sync_strips_inherited_pane_ids_and_keeps_the_ones_it_is_given() {
        let _guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by SOCKET_ENV_LOCK, which every test that sets one of these takes.
        unsafe {
            for name in PANE_CONTEXT_VARS {
                std::env::set_var(name, "%calling");
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s|%s|%s|%s' "${HERDR_PANE_ID-unset}" "${TAW_PANE_ID-unset}" "${HERDR_TAB_ID-unset}" "${TAW_WORKSPACE_ID-unset}""#.to_string(),
        ];
        let given = vec![
            ("HERDR_PANE_ID".to_string(), "w1:p1".to_string()),
            ("TAW_PANE_ID".to_string(), "w1:p1".to_string()),
        ];
        let ran = run_sync(dir.path(), &command, &given);
        // SAFETY: still serialized by SOCKET_ENV_LOCK.
        unsafe {
            for name in PANE_CONTEXT_VARS {
                std::env::remove_var(name);
            }
        }
        let (status, stdout, _) = ran.unwrap();
        assert_eq!(status, 0);
        assert_eq!(stdout, "w1:p1|w1:p1|unset|unset");
    }

    #[test]
    fn spawn_detached_strips_herdr_socket_path_even_when_inherited_from_the_parent_process() {
        let _guard = SOCKET_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by SOCKET_ENV_LOCK; see the run_sync counterpart above.
        unsafe {
            std::env::set_var("HERDR_SOCKET_PATH", "/tmp/should-not-leak-detached.sock");
            std::env::set_var("TAW_SOCKET_PATH", "/tmp/should-not-leak-detached-taw.sock");
        }
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("plugin.log");
        let command = vec!["sh".to_string(), "-c".to_string(), "env".to_string()];
        let spawn_result = spawn_detached(dir.path(), &command, &[], &log_path);
        // SAFETY: still serialized by SOCKET_ENV_LOCK.
        unsafe {
            std::env::remove_var("HERDR_SOCKET_PATH");
            std::env::remove_var("TAW_SOCKET_PATH");
        }
        spawn_result.unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut contents = String::new();
        while std::time::Instant::now() < deadline {
            contents = std::fs::read_to_string(&log_path).unwrap_or_default();
            if !contents.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !contents.is_empty(),
            "detached `env` never wrote its log output"
        );
        assert!(
            !contents.contains("HERDR_SOCKET_PATH"),
            "HERDR_SOCKET_PATH leaked into the detached child's environment:\n{contents}"
        );
        assert!(
            !contents.contains("TAW_SOCKET_PATH"),
            "TAW_SOCKET_PATH leaked into the detached child's environment:\n{contents}"
        );
    }
}
