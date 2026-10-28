// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Qwen Code integration.
//!
//! Writes `<QWEN_HOME|~/.qwen>/hooks/taw-agent-session.sh` and adds a
//! `hooks.SessionStart[*]` entry (matcher `*`) to `<same dir>/settings.json`. Session
//! identity only, as in herdr — the hook's one job is to make the pane resumable, and
//! lifecycle state stays with the watcher's manifest detection.
//!
//! Qwen's hook timeout is milliseconds, not seconds: herdr passes `10_000` here where it
//! passes `10` to the agents whose schema is in seconds.
//!
//! Follows herdr's `qwen` integration: the hook script from
//! `third_party/herdr/integrations/qwen/herdr-agent-session.sh`, the config edit from
//! herdr's `src/integration/targets.rs::install_qwen`.

use super::{
    HOOKS_KEY, Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker,
    plan_json_hook, render_template, require_config_dir, uninstall_json_hook, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-session-qwen.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const EVENT: &str = "SessionStart";
const AGENT: &str = "qwen";
/// Milliseconds — qwen's schema, and herdr's value for it.
const TIMEOUT_MS: u64 = 10_000;

fn config_dir() -> Result<PathBuf> {
    match env_dir("QWEN_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".qwen")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw-agent-session.sh"))
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
                "timeout": TIMEOUT_MS
            }
        ]
    })
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
        message: "installed qwen integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let settings = settings_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_hook(&settings, &sidecar_path(&script), HOOKS_KEY, EVENT, &needle)?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, settings],
        message: "uninstalled qwen integration".to_string(),
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
