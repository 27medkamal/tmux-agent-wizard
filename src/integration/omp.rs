// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! OMP integration.
//!
//! Writes `<agent dir>/extensions/taw-omp-agent-state.ts`, where the agent directory is
//! `$PI_CODING_AGENT_DIR` when set, else `$HOME/${PI_CONFIG_DIR:-.omp}/agent`. There is no
//! config file to edit, so uninstall is just deleting the file.
//!
//! OMP's extension events cover the whole lifecycle, so — as in herdr — this extension is
//! the pane's state authority and reports `idle`/`working`/`blocked` as well as session
//! identity. It additionally holds the pane in `working` through OMP's own auto-retry of a
//! provider error instead of flickering to `idle` and back.
//!
//! Two install guards, both herdr's:
//!
//! * If Pi and OMP resolve to the *same* extensions directory, the install is refused —
//!   `$PI_CODING_AGENT_DIR` is the complete agent directory for both, so installing OMP
//!   there would have Pi load the OMP extension (and report OMP's state as Pi's).
//! * A legacy Pi extension sitting in OMP's own directory is removed, but only when it
//!   carries taw's `pi` marker. A file taw did not write is left where it is.
//!
//! Follows herdr's `omp` integration — see
//! `third_party/herdr/integrations/omp/herdr-agent-state.ts` and herdr's
//! `src/integration/{env.rs::omp_extension_dir,targets.rs::install_omp}`.

use super::{
    Report, Status, TemplateLang, env_dir, home_dir, parse_version_marker, pi, render_template,
    require_config_dir,
};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-omp.ts");
const VERSION_MARKER_PREFIX: &str = "// TAW_INTEGRATION_VERSION=";
const AGENT: &str = "omp";
/// Not `taw-agent-state.ts`: Pi and OMP can share one extensions directory, and two
/// extensions with the same file name cannot.
const INSTALL_NAME: &str = "taw-omp-agent-state.ts";
/// The marker inside a taw-installed Pi extension, used to tell one apart from a file
/// somebody else put in OMP's extensions directory.
const PI_MARKER: &str = "TAW_INTEGRATION_AGENT=pi";

fn agent_dir() -> Result<PathBuf> {
    // `$PI_CODING_AGENT_DIR` is the complete agent directory for whichever of the two is
    // configured with it, which is exactly why the same-directory guard below exists.
    if let Some(dir) = env_dir("PI_CODING_AGENT_DIR") {
        return Ok(dir);
    }
    let config_dir = env_dir("PI_CONFIG_DIR").unwrap_or_else(|| PathBuf::from(".omp"));
    Ok(home_dir()?.join(config_dir).join("agent"))
}

fn extension_dir() -> Result<PathBuf> {
    Ok(agent_dir()?.join("extensions"))
}

fn extension_path() -> Result<PathBuf> {
    Ok(extension_dir()?.join(INSTALL_NAME))
}

/// Remove a taw-installed Pi extension from OMP's extensions directory. Returns whether
/// one was removed. A file of that name that is *not* ours is left alone.
fn remove_legacy_pi_extension(dir: &Path) -> Result<bool> {
    let legacy = dir.join("taw-agent-state.ts");
    if !legacy.is_file() {
        return Ok(false);
    }
    let contents = std::fs::read_to_string(&legacy)
        .with_context(|| format!("reading {}", legacy.display()))?;
    if !contents.contains(PI_MARKER) {
        return Ok(false);
    }
    std::fs::remove_file(&legacy).with_context(|| format!("removing {}", legacy.display()))?;
    Ok(true)
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    let dir = extension_dir()?;
    if dir == pi::extension_dir()? {
        bail!(
            "pi and omp both resolve to the extensions directory {}; the omp extension \
             installed there would be loaded by pi as well. configure separate agent \
             directories (PI_CODING_AGENT_DIR is the complete agent directory for whichever \
             agent it is set for) before installing the omp integration",
            dir.display()
        );
    }
    require_config_dir(&agent_dir()?, AGENT)?;

    let removed_legacy = remove_legacy_pi_extension(&dir).unwrap_or(false);

    let extension = extension_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Js, taw_bin, AGENT);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    super::write_config_file(&extension, rendered.as_bytes())?;

    let mut message = "installed omp integration".to_string();
    if removed_legacy {
        message.push_str("; removed a taw pi extension from the omp extensions directory");
    }
    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![extension],
        message,
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
        message: "uninstalled omp integration".to_string(),
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
