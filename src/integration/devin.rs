// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Devin CLI integration.
//!
//! Writes `<XDG_CONFIG_HOME/devin|~/.config/devin>/taw-agent-state.sh` and adds one
//! `hooks.<event>[*]` entry per event in [`EVENTS`] to `<same dir>/config.json`. herdr
//! registers the same six events, all reporting the `session` action: Devin's hooks do not
//! emit a reliable transition after every permission cancellation or interrupt, so the hook
//! only refreshes session identity and the watcher's manifest detection stays the state
//! authority (herdr `docs/.../integrations.mdx`, "Devin CLI").
//!
//! Follows herdr's `devin` integration: the hook script from
//! `third_party/herdr/integrations/devin/herdr-agent-state.sh`, the config edit from
//! herdr's `src/integration/targets.rs::install_devin`.

use super::{
    HOOKS_KEY, Report, Status, TemplateLang, home_dir, parse_version_marker, plan_json_hooks,
    render_template, require_config_dir, uninstall_json_hooks, write_executable, xdg_env_dir,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-devin.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "devin";

/// The events herdr's `install_devin` registers, in its order. Every one reports session
/// identity only.
const EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
];

/// Events an older herdr Devin integration registered for lifecycle state. Nothing here
/// installs them, but uninstall still sweeps them so a config written by herdr — or by an
/// earlier taw — is left clean.
const LEGACY_EVENTS: [&str; 1] = ["SessionEnd"];

fn config_dir() -> Result<PathBuf> {
    match xdg_env_dir("XDG_CONFIG_HOME") {
        Some(base) => Ok(base.join("devin")),
        None => Ok(home_dir()?.join(".config").join("devin")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("taw-agent-state.sh"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

fn sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.json");
    PathBuf::from(name)
}

fn hook_entry(script: &Path) -> Value {
    json!({
        "hooks": [
            {
                "type": "command",
                "command": format!("bash '{}' session", script.display()),
                "timeout": 10
            }
        ]
    })
}

/// Every event that uninstall must sweep: what install writes, plus [`LEGACY_EVENTS`].
fn all_events() -> Vec<&'static str> {
    EVENTS.iter().chain(LEGACY_EVENTS.iter()).copied().collect()
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let config = config_path()?;
    let needle = script.to_string_lossy().into_owned();
    let entries = EVENTS
        .iter()
        .map(|event| (*event, hook_entry(&script)))
        .collect();
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let config_edit =
        plan_json_hooks(&config, &sidecar_path(&script), HOOKS_KEY, &needle, entries)?;

    write_executable(&script, &rendered)?;
    config_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, config],
        message: "installed devin integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let config = config_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_hooks(
        &config,
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
        files: vec![script, config],
        message: "uninstalled devin integration".to_string(),
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
