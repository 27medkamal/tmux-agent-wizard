// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Antigravity CLI integration (install target `antigravity-cli`, reported agent `agy`).
//!
//! Writes `<ANTIGRAVITY_CLI_CONFIG_DIR|~/.gemini/config>/hooks/taw-agent-state.sh` and
//! inserts one taw-owned block into `<same dir>/hooks.json`.
//!
//! `~/.gemini/config` is where Antigravity CLI reads global customizations from;
//! `~/.gemini/antigravity-cli` holds runtime data and is never read for hooks.
//!
//! Antigravity keys `hooks.json` by *hook name*, not by event, so the whole taw entry lives
//! under one `taw` key that install rewrites wholesale and uninstall removes — every other
//! named hook in the file is untouched. Only `PreInvocation` is registered: it is the one
//! event carrying `conversationId`, and the others cannot express lifecycle safely
//! (Antigravity has no blocked event, `PostInvocation` is skipped on interruption, and
//! `Stop` is end-of-turn rather than process exit). `PreInvocation` also takes a flat
//! handler list — a `matcher`/`hooks` wrapper is valid only for the tool events, and
//! sending one here would invalidate the whole file.
//!
//! Follows herdr's `antigravity-cli` integration: the hook script from
//! `third_party/herdr/integrations/antigravity-cli/herdr-agent-state.sh`, the config edit
//! from herdr's `src/integration/targets.rs::install_antigravity_cli`.

use super::{
    Report, Status, TemplateLang, env_dir, home_dir, object_mut, parse_version_marker,
    plan_json_edit, render_template, require_config_dir, uninstall_json_edit, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-antigravity-cli.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "antigravity-cli";
/// The one `hooks.json` key taw owns. Everything it writes lives under this name, so
/// install can rewrite it wholesale without reading anything else in the file.
const BLOCK_NAME: &str = "taw";
const EVENT: &str = "PreInvocation";

fn config_dir() -> Result<PathBuf> {
    match env_dir("ANTIGRAVITY_CLI_CONFIG_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".gemini").join("config")),
    }
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

fn hook_block(script: &Path) -> Value {
    json!({
        EVENT: [
            {
                "type": "command",
                "command": format!("bash '{}' session", script.display()),
                "timeout": 10
            }
        ]
    })
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let hooks = hooks_path()?;
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let hooks_edit = plan_json_edit(&hooks, &sidecar_path(&script), |root| {
        object_mut(root, "<root>")?.insert(BLOCK_NAME.to_string(), hook_block(&script));
        Ok(())
    })?;

    write_executable(&script, &rendered)?;
    hooks_edit.apply()?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "installed antigravity-cli integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let hooks = hooks_path()?;

    uninstall_json_edit(
        &hooks,
        &sidecar_path(&script),
        |root| {
            Ok(root
                .as_object_mut()
                .is_some_and(|map| map.remove(BLOCK_NAME).is_some()))
        },
        |_| {},
    )?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, hooks],
        message: "uninstalled antigravity-cli integration".to_string(),
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
