// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! OpenCode integration.
//!
//! OpenCode runs in two shapes, and the pane it runs in is known to different processes in
//! them. An OpenCode TUI is the only process that knows which pane it draws in; the server it
//! talks to may be one shared by many panes (that is all of OpenCode 2, and any TUI attached to
//! a remote server). So, as in herdr, the integration is a pair: a plugin that runs in the TUI
//! and reports the session that TUI shows, and a server plugin that is left with the local
//! `opencode run` and Mini clients, which have no TUI plugin.
//!
//! Install writes, under `<XDG_CONFIG_HOME|~/.config>/opencode`:
//!
//! * `plugins/taw-agent-state.js`, the server plugin, which OpenCode finds by directory
//!   convention;
//! * `taw-tui-session.js`, the TUI plugin, registered as `./taw-tui-session.js` in the `plugin`
//!   list of `tui.jsonc` (or of `tui.json`, when that is where the user already has it) for
//!   OpenCode 1;
//! * `taw-opencode/tui.js`, which re-exports the TUI plugin, registered as `./taw-opencode` in
//!   the `plugins` list of `cli.json` for OpenCode 2, which loads a plugin directory's `tui`
//!   entry.
//!
//! The registrations are edits to the user's own JSONC configs, made in place so their comments
//! and layout come through ([`opencode_config`]). Every one is planned before a file is written,
//! so a config that cannot be edited stops the install with nothing half installed, and each
//! keeps a copy of the file as it was found, from which an uninstall gives it back.
//!
//! OpenCode has to be restarted to load the updated plugins.
//!
//! Follows herdr's OpenCode integration; see `third_party/herdr/integrations/opencode/`.

use super::opencode_config::{
    self, CLI_CONFIG, CLI_KEY, TUI_CONFIGS, TUI_KEY, check_targets, plan_cli_registration,
    plan_tui_registration, remove_cli_registration, remove_tui_registration,
};
use super::{
    ConfigEdit, Incomplete, IntegrationError, Report, Status, TemplateLang, check_config_target,
    home_dir, parse_version_marker, render_template, require_config_dir, write_config_file,
    xdg_env_dir,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const SERVER_TEMPLATE: &str = include_str!("templates/taw-agent-state-opencode.js");
const TUI_TEMPLATE: &str = include_str!("templates/taw-tui-session.js");
const V2_TEMPLATE: &str = include_str!("templates/taw-opencode-tui.js");
const VERSION_MARKER_PREFIX: &str = "// TAW_INTEGRATION_VERSION=";
const AGENT: &str = "opencode";

const TUI_PLUGIN_NAME: &str = "taw-tui-session.js";
/// How OpenCode 1's `plugin` list names the TUI plugin file, relative to the config.
const TUI_SPEC: &str = "./taw-tui-session.js";
/// The directory OpenCode 2 loads the plugin from, and how its `plugins` list names it.
const V2_DIR: &str = "taw-opencode";
const V2_SPEC: &str = "./taw-opencode";

fn config_dir() -> Result<PathBuf> {
    match xdg_env_dir("XDG_CONFIG_HOME") {
        Some(base) => Ok(base.join("opencode")),
        None => Ok(home_dir()?.join(".config").join("opencode")),
    }
}

/// Where OpenCode keeps what it remembers between runs, `kv.json` among it.
fn state_dir() -> Result<PathBuf> {
    match xdg_env_dir("XDG_STATE_HOME") {
        Some(base) => Ok(base.join("opencode")),
        None => Ok(home_dir()?.join(".local").join("state").join("opencode")),
    }
}

fn server_plugin_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("plugins").join("taw-agent-state.js"))
}

fn tui_plugin_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(TUI_PLUGIN_NAME))
}

fn v2_entry_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(V2_DIR).join("tui.js"))
}

/// Where the copy of `config`, as it was found before taw's entry was added, is kept: beside
/// the TUI plugin in `dir`, named for both.
fn sidecar_path(dir: &Path, config: &Path) -> PathBuf {
    let mut name = dir.join(TUI_PLUGIN_NAME).into_os_string();
    name.push(".");
    name.push(config.file_name().unwrap_or_default());
    name.push(".orig");
    PathBuf::from(name)
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    let dir = config_dir()?;
    require_config_dir(&dir, AGENT)?;

    // Everything that can refuse a config is settled before a byte is written: none of the
    // three plugin files, no directory and no copy of a config exists yet for a refusal to
    // leave behind.
    check_targets(&dir)?;
    opencode_config::validate_configs(&dir)?;
    let sidecar = |config: &Path| sidecar_path(&dir, config);
    let edits: Vec<ConfigEdit> = [
        plan_tui_registration(&dir, TUI_SPEC, sidecar)?,
        plan_cli_registration(&dir, &state_dir()?, V2_SPEC, sidecar)?,
    ]
    .into_iter()
    .flatten()
    .collect();
    let configs: Vec<PathBuf> = edits.iter().map(|edit| edit.config_path.clone()).collect();

    let server = server_plugin_path()?;
    let tui = tui_plugin_path()?;
    let v2 = v2_entry_path()?;
    // The plugin files answer the same question, so that one that cannot be rewritten does not
    // leave the ones before it rewritten.
    for path in [&server, &tui, &v2] {
        check_config_target(path)?;
    }
    for (path, template) in [
        (&server, SERVER_TEMPLATE),
        (&tui, TUI_TEMPLATE),
        (&v2, V2_TEMPLATE),
    ] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let rendered = render_template(template, TemplateLang::Js, taw_bin, AGENT);
        write_config_file(path, rendered.as_bytes())?;
    }
    for edit in edits {
        edit.apply()?;
    }

    let mut message = "installed opencode integration; restart OpenCode to load it".to_string();
    if opencode_config::cli_registration_deferred(&dir, &state_dir()?) {
        message.push_str(
            ". OpenCode 2 still has settings to move into cli.json, so its plugin is not \
             registered yet: start OpenCode 2 once, then run this install again",
        );
    }

    let mut files = vec![server, tui, v2];
    files.extend(configs);
    Ok(Report {
        agent: AGENT.to_string(),
        files,
        message,
    })
}

/// The configs an uninstall has to write: the ones that hold taw's entry, or that taw kept a
/// copy of. A config that is neither is not taw's to check, or to refuse.
fn configs_to_revert(dir: &Path) -> Vec<(PathBuf, &'static str, &'static str)> {
    let mut candidates: Vec<(PathBuf, &'static str, &'static str)> = TUI_CONFIGS
        .iter()
        .map(|name| (dir.join(name), TUI_KEY, TUI_SPEC))
        .collect();
    candidates.push((dir.join(CLI_CONFIG), CLI_KEY, V2_SPEC));
    candidates
        .into_iter()
        .filter(|(path, _, spec)| {
            sidecar_path(dir, path).exists()
                || std::fs::read_to_string(path).is_ok_and(|text| text.contains(spec))
        })
        .collect()
}

pub(super) fn uninstall() -> Result<Report> {
    let dir = config_dir()?;
    let server = server_plugin_path()?;
    let tui = tui_plugin_path()?;
    let v2 = v2_entry_path()?;

    // Refused before anything is removed: a config taw has to edit and cannot leaves its entry
    // naming a plugin file that is about to go.
    for (path, key, spec) in configs_to_revert(&dir) {
        check_config_target(&path)?;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.contains(spec)
            && let Err(detail) = opencode_config::validate_list(&text, key)
        {
            return Err(IntegrationError::UnusableConfig {
                path: path.display().to_string(),
                detail,
            }
            .into());
        }
    }

    let sidecar = |config: &Path| sidecar_path(&dir, config);
    let mut files = Vec::new();
    if remove_cli_registration(&dir, V2_SPEC, sidecar)? {
        files.push(dir.join(CLI_CONFIG));
    }
    files.extend(remove_tui_registration(&dir, TUI_SPEC, sidecar)?);

    for path in [&v2, &tui, &server] {
        if path.exists() {
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    // The entry's directory goes with it, unless something of the user's is in it.
    if let Some(v2_dir) = v2.parent()
        && v2_dir.is_dir()
    {
        let _ = std::fs::remove_dir(v2_dir);
    }

    let mut reported = vec![server, tui, v2];
    reported.extend(files);
    Ok(Report {
        agent: AGENT.to_string(),
        files: reported,
        message: "uninstalled opencode integration".to_string(),
    })
}

pub(super) fn status() -> Result<Status> {
    let plugin = server_plugin_path()?;
    let (installed, version) = match std::fs::read_to_string(&plugin) {
        Ok(contents) => (true, parse_version_marker(&contents, VERSION_MARKER_PREFIX)),
        Err(_) => (false, None),
    };
    // OpenCode 2 loads the TUI plugin from `cli.json`. An install puts it there, unless
    // OpenCode 2 has settings of version 1 to move into a `cli.json` of its own first and there
    // is none yet: then it waits. A `cli.json` that exists without taw's entry (OpenCode 2 has
    // run) is one a second install adds; while there is none, a second install would only
    // wait again, so that is said and is not outdated.
    let incomplete = if installed {
        match std::fs::read_to_string(config_dir()?.join(CLI_CONFIG)) {
            Ok(text) if opencode_config::is_registered(&text, CLI_KEY, V2_SPEC) => None,
            Ok(_) => Some(Incomplete::Reinstall(
                "OpenCode 2 reads the plugin that tells taw which pane it is in from cli.json, \
                 and that holds no entry for taw's, so its panes can be mixed up. Run `taw \
                 integration install opencode` again to add it. OpenCode 1 is not affected."
                    .to_string(),
            )),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(Incomplete::Waiting(
                "OpenCode 2 reads the plugin that tells taw which pane it is in from cli.json, \
                 which does not exist yet, so its panes can be mixed up if you use it. Start \
                 OpenCode 2 once, so that it makes cli.json, then run `taw integration install \
                 opencode` again. OpenCode 1 is not affected."
                    .to_string(),
            )),
            // A cli.json that cannot be read says nothing either way.
            Err(_) => None,
        }
    } else {
        None
    };
    Ok(Status {
        agent: AGENT.to_string(),
        installed,
        path: plugin,
        version,
        current: parse_version_marker(SERVER_TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete,
    })
}
