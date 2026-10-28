// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Cursor Agent CLI integration.
//!
//! Writes `<CURSOR_CONFIG_DIR|~/.cursor>/taw-agent-state.sh` and adds a
//! `hooks.sessionStart[*]` entry to `<same dir>/hooks.json`. Cursor's entry shape is the
//! flattest of the ported agents — a bare `{"command": …}`, with no `type`, `matcher` or
//! timeout (<https://cursor.com/docs/hooks>) — and the file carries a mandatory top-level
//! `"version": 1` which install adds when absent.
//!
//! Follows herdr's `cursor` integration: the hook script from
//! `third_party/herdr/integrations/cursor/herdr-agent-state.sh`, the config edit from
//! herdr's `src/integration/targets.rs::install_cursor`.

use super::{
    HOOKS_KEY, Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker,
    plan_json_edit, render_template, require_config_dir, strip_hook_entries, uninstall_json_edit,
    upsert_hook_entry, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-cursor.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const EVENT: &str = "sessionStart";
const AGENT: &str = "cursor";
/// The `hooks.json` schema version Cursor requires, and the value herdr writes.
const SCHEMA_VERSION: u64 = 1;

/// Events an older herdr Cursor integration registered. Install writes none of them;
/// uninstall still sweeps them so an upgraded install leaves nothing behind.
const LEGACY_EVENTS: [&str; 5] = [
    "beforeSubmitPrompt",
    "beforeShellExecution",
    "beforeMCPExecution",
    "stop",
    "sessionEnd",
];

fn config_dir() -> Result<PathBuf> {
    match env_dir("CURSOR_CONFIG_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".cursor")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("taw-agent-state.sh"))
}

fn hooks_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks.json"))
}

fn sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.json");
    PathBuf::from(name)
}

fn hook_entry(script: &Path) -> Value {
    json!({ "command": format!("bash '{}' session", script.display()) })
}

/// Remove our entry from `sessionStart` and from every [`LEGACY_EVENTS`] name.
fn strip(root: &mut Value, needle: &str) -> Result<bool> {
    let mut changed = false;
    for event in std::iter::once(EVENT).chain(LEGACY_EVENTS) {
        changed |= strip_hook_entries(root, HOOKS_KEY, event, needle)?;
    }
    Ok(changed)
}

/// Ignore a `"version": 1` install may have added when deciding whether the file is
/// otherwise untouched — without which a `hooks.json` that had no `version` before install
/// could never revert byte-for-byte. Applied to both sides of that comparison only, so a
/// file that *did* change since install keeps the key rather than losing it to the revert.
fn normalize_schema_version(root: &mut Value) {
    if let Some(map) = root.as_object_mut()
        && map.get("version") == Some(&json!(SCHEMA_VERSION))
    {
        map.remove("version");
    }
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let hooks = hooks_path()?;
    let needle = script.to_string_lossy().into_owned();
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let hooks_edit = plan_json_edit(&hooks, &sidecar_path(&script), |root| {
        // `version` first: a file we are creating gets it, one that already declares a
        // version keeps whatever it declares.
        if let Some(map) = root.as_object_mut()
            && !map.contains_key("version")
        {
            map.insert("version".to_string(), json!(SCHEMA_VERSION));
        }
        upsert_hook_entry(root, HOOKS_KEY, EVENT, &needle, hook_entry(&script))
    })?;

    write_executable(&script, &rendered)?;
    hooks_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "installed cursor integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let hooks = hooks_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_edit(
        &hooks,
        &sidecar_path(&script),
        |root| strip(root, &needle),
        normalize_schema_version,
    )?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "uninstalled cursor integration".to_string(),
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
