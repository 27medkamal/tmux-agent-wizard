// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Hermes Agent integration.
//!
//! Writes `<HERMES_HOME|~/.hermes>/plugins/taw-agent-state/` — a `plugin.yaml` manifest and
//! an `__init__.py` — and adds `taw-agent-state` to `plugins.enabled` in
//! `<HERMES_HOME|~/.hermes>/config.yaml`. Hermes has to be restarted after installing
//! before it loads the plugin.
//!
//! The plugin reports the resumable session id; state stays with the watcher's manifest
//! detection, as in herdr.
//!
//! Follows herdr's `hermes` integration: the plugin from
//! `third_party/herdr/integrations/hermes/{__init__.py,plugin.yaml}`, the config edit from
//! herdr's `src/integration/config_edit.rs::update_hermes_enabled_plugin` (see
//! [`super::hermes_yaml`]).

use super::{
    Incomplete, IntegrationError, Refused, Report, Status, TemplateLang, env_dir, hermes_yaml,
    home_dir, parse_version_marker, plan_text_edit_repairing, render_template, require_config_dir,
    uninstall_text_edit,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const INIT_TEMPLATE: &str = include_str!("templates/taw-hermes-plugin.py");
const MANIFEST_TEMPLATE: &str = include_str!("templates/taw-hermes-plugin.yaml");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "hermes";
/// The plugin's directory name, which is also the name `plugins.enabled` refers to and the
/// `name:` in its manifest. All three have to agree or Hermes will not load it.
const PLUGIN_NAME: &str = "taw-agent-state";

fn config_dir() -> Result<PathBuf> {
    match env_dir("HERMES_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".hermes")),
    }
}

fn plugin_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("plugins").join(PLUGIN_NAME))
}

fn init_path() -> Result<PathBuf> {
    Ok(plugin_dir()?.join("__init__.py"))
}

fn manifest_path() -> Result<PathBuf> {
    Ok(plugin_dir()?.join("plugin.yaml"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.yaml"))
}

fn sidecar_path(init: &Path) -> PathBuf {
    let mut name = init.as_os_str().to_owned();
    name.push(".orig.yaml");
    PathBuf::from(name)
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;

    let init = init_path()?;
    let manifest = manifest_path()?;
    let config = config_path()?;
    // Everything that can refuse config.yaml is settled before a byte is written: no plugin
    // directory or file is published for a config this install then cannot edit.
    let config_edit = plan_text_edit_repairing(&config, &sidecar_path(&init), |original| {
        // Lines an older taw put where Hermes cannot read them come out first, whatever the
        // rest of the file then lets this install do.
        let repaired = hermes_yaml::repair(original, PLUGIN_NAME);
        let text = repaired.as_deref().unwrap_or(original);
        hermes_yaml::check(text).map_err(|why| {
            // What the lead says taw did, the tail does not say again.
            let (lead, left) = if repaired.is_some() {
                (
                    "taw took its older lines out of it, since they left Hermes unable to read \
                     the file, and changed nothing else. taw does not install yet: ",
                    " Add",
                )
            } else {
                ("", " It is left as it is: add")
            };
            Refused {
                error: anyhow::Error::from(IntegrationError::UnusableConfig {
                    path: config.display().to_string(),
                    detail: format!(
                        "{lead}{why}.{left} {PLUGIN_NAME} under \
                         plugins.enabled by hand, or write plugins the way taw's edit reads it, \
                         then install again"
                    ),
                }),
                repaired: repaired.clone(),
            }
        })?;
        Ok(hermes_yaml::enable(text, PLUGIN_NAME))
    })?;

    let dir = plugin_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Plain files, not executables: Hermes imports the package, it does not exec it.
    super::write_config_file(
        &init,
        render_template(INIT_TEMPLATE, TemplateLang::Py, taw_bin, AGENT).as_bytes(),
    )?;
    super::write_config_file(
        &manifest,
        render_template(MANIFEST_TEMPLATE, TemplateLang::Py, taw_bin, AGENT).as_bytes(),
    )?;

    config_edit
        .apply()
        .with_context(|| format!("updating {}", config.display()))?;

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![init, manifest, config],
        message: "installed hermes integration (restart hermes so the plugin loads)".to_string(),
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let init = init_path()?;
    let manifest = manifest_path()?;
    let config = config_path()?;

    uninstall_text_edit(
        &config,
        &sidecar_path(&init),
        |current| hermes_yaml::disable(current, PLUGIN_NAME),
        hermes_yaml::normalize_for_compare,
    )?;

    // The whole plugin directory is taw's, so it goes — but only the files taw wrote plus
    // the directory itself, and only if nothing else was put inside it.
    for path in [&init, &manifest] {
        if path.exists() {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    let dir = plugin_dir()?;
    if dir.is_dir() {
        // `remove_dir` (not `remove_dir_all`): a file somebody else put in there is
        // theirs, and taking it out with the directory would be destroying data taw
        // never created. It stays, and so does the directory holding it.
        let _ = std::fs::remove_dir(&dir);
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![init, manifest, config],
        message: "uninstalled hermes integration".to_string(),
    })
}

/// Lines of an older taw in `config.yaml` that leave Hermes unable to read it: an install takes
/// them out. Only the shapes taw can tell are its own by the lines it wrote are reported (see
/// [`hermes_yaml::repair`]); a file broken some other way says nothing.
fn config_incomplete(config: &Path) -> Option<Incomplete> {
    let text = std::fs::read_to_string(config).ok()?;
    hermes_yaml::repair(&text, PLUGIN_NAME).map(|_| {
        Incomplete::Reinstall(format!(
            "Hermes cannot read {} as an older taw left it: lines of its own sit where they \
             break the file; a reinstall takes them out and says what to change. Run `taw \
             integration install hermes` again.",
            config.display()
        ))
    })
}

pub(super) fn status() -> Result<Status> {
    let init = init_path()?;
    let (installed, version) = match std::fs::read_to_string(&init) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    let incomplete = if installed {
        config_incomplete(&config_path()?)
    } else {
        None
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: init,
        version,
        current: parse_version_marker(INIT_TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete,
    })
}
