// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Pi integration.
//!
//! Writes `<PI_CODING_AGENT_DIR|~/.pi/agent>/extensions/taw-agent-state.ts`. There is no
//! config file to edit — Pi loads every extension in that directory — so uninstall is just
//! deleting the file.
//!
//! Pi's extension events cover the whole lifecycle, so — as in herdr — this extension is
//! the pane's state authority and reports `idle`/`working`/`blocked` as well as session
//! identity.
//!
//! The gate is the *agent* directory, not `extensions/`: herdr creates `extensions/` when
//! the directory above it already exists, and refuses when it does not, which is the same
//! rule `require_config_dir` states — a missing agent directory means Pi is not installed.
//!
//! Follows herdr's `pi` integration — see
//! `third_party/herdr/integrations/pi/herdr-agent-state.ts`.

use super::{
    Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker, render_template,
    require_config_dir,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-pi.ts");
const VERSION_MARKER_PREFIX: &str = "// TAW_INTEGRATION_VERSION=";
const AGENT: &str = "pi";

/// Pi's agent directory. Shared with `omp.rs`, which resolves the same override first.
pub(super) fn agent_dir() -> Result<PathBuf> {
    match env_dir("PI_CODING_AGENT_DIR") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".pi").join("agent")),
    }
}

pub(super) fn extension_dir() -> Result<PathBuf> {
    Ok(agent_dir()?.join("extensions"))
}

fn extension_path() -> Result<PathBuf> {
    Ok(extension_dir()?.join("taw-agent-state.ts"))
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&agent_dir()?, AGENT)?;

    let extension = extension_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Js, taw_bin, AGENT);
    if let Some(parent) = extension.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    super::write_config_file(&extension, rendered.as_bytes())?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![extension],
        message: "installed pi integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let extension = extension_path()?;
    if extension.exists() {
        std::fs::remove_file(&extension)
            .with_context(|| format!("removing {}", extension.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![extension],
        message: "uninstalled pi integration".to_string(),
    })
}

pub(super) fn status() -> Result<Status> {
    let extension = extension_path()?;
    let (installed, version) = match std::fs::read_to_string(&extension) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: extension,
        version,
        current: parse_version_marker(TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete: None,
    })
}
