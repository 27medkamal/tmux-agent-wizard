// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! MastraCode integration.
//!
//! Writes `~/.mastracode/hooks/taw-agent-state.sh` and registers one entry per event in
//! [`EVENTS`] in `~/.mastracode/hooks.json`.
//!
//! Two things are unlike the other ports. MastraCode keys `hooks.json` straight off the
//! file root rather than under a `hooks` object, and its entries are flat
//! (`{"type", "command", "timeout", "description"}`) with a timeout in *milliseconds*.
//! And it has no screen-manifest fallback: the hook is the pane's state authority, so it
//! reports `idle`/`working`/`blocked` as well as thread identity.
//!
//! MastraCode has no config-directory environment override, in herdr or here. herdr also
//! creates `~/.mastracode/hooks/` without requiring the directory above it to exist; taw
//! keeps its own rule instead and requires `~/.mastracode` first, so a machine without
//! MastraCode installed does not get a directory tree invented for it.
//!
//! Follows herdr's `mastracode` integration: the hook script from
//! `third_party/herdr/integrations/mastracode/herdr-agent-state.sh`, the config edit from
//! herdr's `src/integration/targets.rs::install_mastracode`.

use super::{
    ROOT_EVENTS, Report, Status, TemplateLang, home_dir, parse_version_marker, plan_json_hooks,
    render_template, require_config_dir, uninstall_json_hooks, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-mastracode.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "mastracode";
/// Milliseconds — MastraCode's schema, and herdr's value for it.
const TIMEOUT_MS: u64 = 10_000;

/// `(event, reported action)`, in herdr's order. `session` reports thread identity; the
/// rest report the lifecycle state named.
const EVENTS: [(&str, &str); 11] = [
    ("SessionStart", "session"),
    ("UserPromptSubmit", "working"),
    ("AgentStart", "working"),
    ("PreToolUse", "working"),
    ("PermissionRequest", "blocked"),
    ("PermissionResult", "working"),
    ("SubagentStart", "working"),
    ("SubagentEnd", "working"),
    ("Interrupt", "idle"),
    ("AgentEnd", "idle"),
    ("Stop", "idle"),
];

/// Events an older herdr MastraCode integration registered. Install writes neither;
/// uninstall still sweeps them so an upgraded install leaves nothing behind.
const LEGACY_EVENTS: [&str; 1] = ["SessionEnd"];

fn config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".mastracode"))
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw-agent-state.sh"))
}

fn hooks_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks.json"))
}

fn sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.json");
    PathBuf::from(name)
}

fn hook_entry(script: &Path, action: &str) -> Value {
    json!({
        "type": "command",
        "command": format!("bash '{}' {action}", script.display()),
        "timeout": TIMEOUT_MS,
        "description": "Report MastraCode agent state to taw"
    })
}

fn all_events() -> Vec<&'static str> {
    EVENTS
        .iter()
        .map(|(event, _)| *event)
        .chain(LEGACY_EVENTS.iter().copied())
        .collect()
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let hooks = hooks_path()?;
    let needle = script.to_string_lossy().into_owned();
    let entries = EVENTS
        .iter()
        .map(|(event, action)| (*event, hook_entry(&script, action)))
        .collect();
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let hooks_edit = plan_json_hooks(
        &hooks,
        &sidecar_path(&script),
        ROOT_EVENTS,
        &needle,
        entries,
    )?;

    write_executable(&script, &rendered)?;
    hooks_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "installed mastracode integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let hooks = hooks_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_hooks(
        &hooks,
        &sidecar_path(&script),
        ROOT_EVENTS,
        &needle,
        &all_events(),
    )?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "uninstalled mastracode integration".to_string(),
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
