// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Claude Code integration.
//!
//! Writes `<CLAUDE_CONFIG_DIR|~/.claude>/hooks/taw-agent-state.sh` and adds a
//! `hooks.SessionStart[*]` entry to `<same dir>/settings.json`, matcher
//! [`SESSION_START_MATCHER`] rather than `*` (v0.9.1, herdr `claude_settings.rs:19`): Grok
//! imports Claude's hooks wholesale but reports its own `new`/`load` sources rather than
//! Claude's five documented ones, so a wildcard matcher would make Grok invoke this
//! script too, relabelling the pane `claude` with Grok's session id. `upsert_hook_entry`
//! finds the previously-installed entry by the script path it names rather than by
//! matcher, so reinstalling over an older wildcard-matcher install migrates it to the
//! narrower one rather than adding a second entry.
//!
//! Follows herdr's `claude` integration — see
//! `third_party/herdr/integrations/claude/herdr-agent-state.sh` and `settings.json`.

use super::{
    HOOKS_KEY, Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker,
    plan_json_hook_in_shell, render_template, require_config_dir, sh_word,
    uninstall_json_hook_in_shell, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-claude.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const EVENT: &str = "SessionStart";
const AGENT: &str = "claude";
// Follows herdr's src/integration/claude_settings.rs: the same matcher, and taw finds its
// own entry by the script path it names.
/// Claude's documented `SessionStart` sources. Follows herdr's `claude_settings.rs`
/// `SESSION_START_MATCHER`.
const SESSION_START_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";

fn config_dir() -> Result<PathBuf> {
    match env_dir("CLAUDE_CONFIG_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".claude")),
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

/// The entry an install writes. Claude runs a hook's command through a shell (its hook runner
/// starts `bash`, or PowerShell when the hook says so), so a script under the home folder is
/// named as `"$HOME/..."` and the settings carry no home folder; any other path is absolute.
fn hook_entry(script: &Path) -> Value {
    json!({
        "matcher": SESSION_START_MATCHER,
        "hooks": [
            {
                "type": "command",
                "command": format!("bash {} session", sh_word(script)),
                "timeout": 10
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
    let settings_edit = plan_json_hook_in_shell(
        &settings,
        &sidecar_path(&script),
        HOOKS_KEY,
        EVENT,
        &needle,
        hook_entry(&script),
        &sh_word(&script),
    )?;

    write_executable(&script, &rendered)?;
    settings_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, settings],
        message: "installed claude integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let settings = settings_path()?;
    let needle = script.to_string_lossy().into_owned();

    uninstall_json_hook_in_shell(&settings, &sidecar_path(&script), HOOKS_KEY, EVENT, &needle)?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, settings],
        message: "uninstalled claude integration".to_string(),
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
