// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Kilo Code CLI integration.
//!
//! Writes `<XDG_CONFIG_HOME|~/.config>/kilo/plugin/taw-agent-state.js`. There is no config
//! file to edit — Kilo discovers plugins by directory convention — so uninstall is just
//! deleting the file.
//!
//! Kilo's plugin events cover the whole lifecycle, so — as in herdr — this plugin is the
//! pane's state authority and reports `idle`/`working`/`blocked` as well as session
//! identity.
//!
//! herdr resolves this directory as `~/.config/kilo` unconditionally; taw honours
//! `$XDG_CONFIG_HOME` first, the same deviation `opencode.rs` already makes, so a user with
//! a relocated config directory is not silently written to the wrong place.
//!
//! Follows herdr's `kilo` integration — see
//! `third_party/herdr/integrations/kilo/herdr-agent-state.js`.

use super::{
    Report, Status, TemplateLang, home_dir, parse_version_marker, render_template,
    require_config_dir, xdg_env_dir,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-kilo.js");
const VERSION_MARKER_PREFIX: &str = "// TAW_INTEGRATION_VERSION=";
const AGENT: &str = "kilo";

fn config_dir() -> Result<PathBuf> {
    match xdg_env_dir("XDG_CONFIG_HOME") {
        Some(base) => Ok(base.join("kilo")),
        None => Ok(home_dir()?.join(".config").join("kilo")),
    }
}

/// `plugin/`, singular — Kilo's directory name, unlike OpenCode's `plugins/`.
fn plugin_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("plugin").join("taw-agent-state.js"))
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let plugin = plugin_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Js, taw_bin, AGENT);
    if let Some(parent) = plugin.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    super::write_config_file(&plugin, rendered.as_bytes())?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![plugin],
        message: "installed kilo integration".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let plugin = plugin_path()?;
    if plugin.exists() {
        std::fs::remove_file(&plugin).with_context(|| format!("removing {}", plugin.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![plugin],
        message: "uninstalled kilo integration".to_string(),
    })
}

pub(super) fn status() -> Result<Status> {
    let plugin = plugin_path()?;
    let (installed, version) = match std::fs::read_to_string(&plugin) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: plugin,
        version,
        current: parse_version_marker(TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete: None,
    })
}
