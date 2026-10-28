// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Qoder CLI integration.
//!
//! Writes `<QODER_CONFIG_DIR|~/.qoder>/hooks/taw-agent-state.sh` and adds a
//! `hooks.SessionStart[*]` entry (matcher `*`) to `<same dir>/settings.json`. Qoder's
//! settings schema mirrors Claude Code's, per <https://docs.qoder.com/zh/cli/hooks>.
//! Qoder's hooks do not cover every lifecycle transition, so — as in herdr — the hook
//! reports session identity only and manifest detection stays the state authority.
//!
//! Follows herdr's `qodercli` integration: the hook script from
//! `third_party/herdr/integrations/qodercli/herdr-agent-state.sh`, the config edit from
//! herdr's `src/integration/targets.rs::install_qodercli`.

use super::{
    HOOKS_KEY, Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker,
    plan_json_hook, render_template, require_config_dir, uninstall_json_hooks, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-qodercli.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const EVENT: &str = "SessionStart";
const AGENT: &str = "qodercli";

/// Events an older herdr Qoder integration registered for lifecycle state. Install writes
/// none of them; uninstall still sweeps them so an upgraded install leaves nothing behind.
const LEGACY_EVENTS: [&str; 11] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "Notification",
    "PermissionRequest",
    "Stop",
    "SessionEnd",
];

fn config_dir() -> Result<PathBuf> {
    match env_dir("QODER_CONFIG_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".qoder")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw-agent-state.sh"))
}

fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("settings.json"))
}

fn sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.json");
    PathBuf::from(name)
}

fn hook_entry(script: &Path) -> Value {
    json!({
        "matcher": "*",
        "hooks": [
            {
                "type": "command",
                "command": format!("bash '{}' session", script.display()),
                "timeout": 10
            }
        ]
    })
}

fn all_events() -> Vec<&'static str> {
    std::iter::once(EVENT)
        .chain(LEGACY_EVENTS.iter().copied())
        .collect()
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let settings = settings_path()?;
    let needle = script.to_string_lossy().into_owned();
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let settings_edit = plan_json_hook(
        &settings,
        &sidecar_path(&script),
        HOOKS_KEY,
        EVENT,
        &needle,
        hook_entry(&script),
    )?;

    write_executable(&script, &rendered)?;
    settings_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, settings],
        message: "installed qodercli integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let settings = settings_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_hooks(
        &settings,
        &sidecar_path(&script),
        HOOKS_KEY,
        &needle,
        &all_events(),
    )?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, settings],
        message: "uninstalled qodercli integration".to_string(),
    })
}

pub(super) fn status() -> Result<Status> {
    let script = script_path()?;
    let (installed, version) = match std::fs::read_to_string(&script) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: script,
        version,
        current: parse_version_marker(TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete: None,
    })
}
