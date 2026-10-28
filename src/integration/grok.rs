// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Grok CLI integration.
//!
//! Writes two taw-owned files into `<GROK_HOME|~/.grok>/hooks/`: the hook script
//! `taw-agent-state.sh` and its own `taw.json` registering it on `SessionStart`.
//!
//! Grok merges every `hooks/*.json` in that directory, so — exactly as in herdr — there is
//! no shared config file to edit and no user hook file to rewrite. Uninstall is a straight
//! delete of both files, which is byte-for-byte safe by construction: taw owns them
//! outright and nothing else in the directory is read or written.
//!
//! Follows herdr's `grok` integration: the hook script from
//! `third_party/herdr/integrations/grok/herdr-agent-state.sh`, the hook config from herdr's
//! `src/integration/targets.rs::grok_hook_config`.

use super::{
    Report, Status, TemplateLang, check_config_target, env_dir, home_dir, parse_version_marker,
    render_template, require_config_dir, write_executable,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-grok.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "grok";

fn config_dir() -> Result<PathBuf> {
    match env_dir("GROK_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".grok")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw-agent-state.sh"))
}

fn hook_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw.json"))
}

/// The complete taw-owned hook config. herdr invokes the grok hook through `sh`, not
/// `bash` — kept, so the script's `#!/bin/sh` contract is what actually runs it.
fn hook_config(script: &Path) -> Value {
    json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": format!("sh '{}' session", script.display()),
                            "timeout": 10
                        }
                    ]
                }
            ]
        }
    })
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let config = hook_config_path()?;
    // taw owns this file outright, but a link to somewhere else, a hard link or a file made
    // read-only is still refused before the script is published.
    check_config_target(&config)?;

    write_executable(&script, &rendered)?;
    let mut bytes = serde_json::to_vec_pretty(&hook_config(&script))
        .expect("serde_json::Value always serializes");
    bytes.push(b'\n');
    super::write_config_file(&config, &bytes)?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, config],
        message: "installed grok integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let config = hook_config_path()?;
    for path in [&config, &script] {
        if path.exists() {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, config],
        message: "uninstalled grok integration".to_string(),
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
