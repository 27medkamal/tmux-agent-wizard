// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Agent hook installers — ports of herdr's `herdr integration install <agent>` for every
//! agent herdr has an integration for, one module each ([`AGENTS`]).
//!
//! Each installer writes a small script into the agent's own config directory — POSIX `sh`
//! for a hook the agent execs, JavaScript/TypeScript for a plugin its node runtime loads,
//! Python for the one Hermes imports — and, where the agent needs it, registers that script
//! in the agent's own config file. The script reports the agent's native session id to
//! `taw` via `taw pane report-agent-session`, and for the agents whose events cover the
//! whole lifecycle (kimi, kilo, mastracode, pi, omp, opencode) its `idle`/`working`/
//! `blocked` state via `taw pane report-agent`, using `$TMUX_PANE` to identify the pane.
//! herdr's originals do the same over a unix socket; `third_party/herdr/integrations/` has
//! the vendored source each script is ported from, and every file names its own origin in
//! its header. `THIRD_PARTY_LICENSES.md` lists them all.
//!
//! Four things are true of every installer here, and are what the shared helpers below
//! exist for:
//!
//! * **A missing config directory is refused** ([`require_config_dir`]). It means the agent
//!   itself is not installed, and writing into it would fabricate config for software that
//!   may not be on this machine.
//! * **A config that cannot be edited is refused before anything is written.** Every
//!   config an install edits is checked, read and edited in memory first ([`ConfigEdit`]):
//!   one that is not JSON, has a shape no hook can be added to, has another hard link, or
//!   is read-only for this user stops the install while there is nothing yet to be half
//!   installed, with no hook script, no backup and no edit to a second file.
//! * **A refusal is reported as itself.** The CLI prints only the outermost message of an
//!   error, so a refusal that names the file it left alone and what to do about it (a
//!   config that is read-only or has another hard link, on an uninstall as on an install)
//!   is returned as it is, never wrapped in a context of an installer's own that would
//!   hide the reason.
//! * **Uninstall gives the config file back byte-for-byte** when nothing else touched it
//!   since install, by way of a sidecar copy of its pre-install bytes; when something else
//!   *did* touch it, only taw's own entries are removed and every other edit is kept.
//!
//! Windows is out of scope: herdr ships a PowerShell variant of each hook, and none of
//! them is ported here.

mod antigravity_cli;
mod claude;
mod codex;
pub use codex::HOOK_REVIEW as CODEX_HOOK_REVIEW;
mod copilot;
mod cursor;
mod devin;
mod droid;
mod grok;
mod hermes;
mod hermes_yaml;
mod kilo;
mod kimi;
mod letta;
mod mastracode;
mod omp;
mod opencode;
mod opencode_config;
mod pi;
mod qodercli;
mod qwen;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Every agent this crate can install a hook integration for — herdr's own seventeen, plus
/// `letta` (v0.9.1, experimental — see [`EXPERIMENTAL`]) — in the order they were ported.
/// `antigravity-cli` is the install target's name; the agent it reports is `agy` (see
/// `antigravity_cli.rs`). `letta` stays last so `status_all()` lists it last without any
/// extra ordering logic, matching herdr's own separate "(experimental)" listing.
pub const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "copilot",
    "opencode",
    "devin",
    "droid",
    "qodercli",
    "qwen",
    "grok",
    "cursor",
    "antigravity-cli",
    "mastracode",
    "kimi",
    "kilo",
    "hermes",
    "pi",
    "omp",
    "letta",
];

/// Agents whose integration `taw integration status` marks `"experimental": true`. herdr
/// lists Letta's integration separately, suffixed `(experimental)`
/// (`S/v0.9.1/src/cli/integration.rs:49-60`); taw's status is JSON, so this is a field on
/// the entry instead of a second listing call.
pub const EXPERIMENTAL: &[&str] = &["letta"];

/// Result of an [`install`] or [`uninstall`] call.
#[derive(Debug)]
pub struct Report {
    pub agent: String,
    pub files: Vec<PathBuf>,
    pub message: String,
}

/// Result of a [`status`] call.
#[derive(Debug)]
pub struct Status {
    pub agent: String,
    pub installed: bool,
    pub path: PathBuf,
    /// The version marker of the installed script, when it has one.
    pub version: Option<u32>,
    /// The version marker of the script this build installs: what `version` is measured
    /// against, integration by integration (they are bumped one at a time).
    pub current: Option<u32>,
    /// What the installed integration lacks that an install would add, or what keeps it from
    /// working that an install does not change, for the integrations whose install can be left
    /// incomplete or whose agent's config can stop it: OpenCode 2's registration, and the
    /// config files of Codex, Kimi and Hermes that an older taw or the user left in a shape the
    /// agent cannot use.
    pub incomplete: Option<Incomplete>,
}

/// What an installed integration lacks that an install would add, in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incomplete {
    /// Installing again adds it now, so the integration is outdated.
    Reinstall(String),
    /// Installing again changes nothing about it today, so it is said in the note and the
    /// integration is not outdated. Either it waits for the agent to have run once (OpenCode 2
    /// makes its own `cli.json` the first time it starts, and the install leaves its
    /// registration to that), or it is the user's own to fix and an install refuses to touch it
    /// (Codex's `config.toml` that sets `hooks = false`, writes `features` in a shape taw does
    /// not edit, or does not parse for a reason that is not taw's).
    Waiting(String),
}

impl Status {
    /// Whether installing again would change what is installed: the script is older than the
    /// one this build writes (or has no marker to say), or the install left out something a
    /// second one adds now. An integration that is not installed is not outdated.
    pub fn outdated(&self) -> bool {
        self.installed
            && (self.behind() || matches!(self.incomplete, Some(Incomplete::Reinstall(_))))
    }

    fn behind(&self) -> bool {
        match (self.version, self.current) {
            (None, _) => true,
            (Some(installed), Some(current)) => installed < current,
            (Some(_), None) => false,
        }
    }

    /// What is out of date or still to come, in plain words, and what to do about it; `None` when
    /// nothing is. It says more than [`Status::outdated`] does: an integration that waits for its
    /// agent is not outdated, and still has something to say.
    pub fn note(&self) -> Option<String> {
        if !self.installed {
            return None;
        }
        let reinstall = format!("taw integration install {}", self.agent);
        let mut note = String::new();
        if self.behind() {
            note.push_str(&match (self.version, self.current) {
                (Some(installed), Some(current)) => format!(
                    "The installed script is version {installed} and this build writes version \
                     {current}. "
                ),
                _ => "The installed script has no version marker. ".to_string(),
            });
            note.push_str(&format!("Run `{reinstall}` again to update it."));
        }
        if let Some(Incomplete::Reinstall(missing) | Incomplete::Waiting(missing)) =
            &self.incomplete
        {
            if !note.is_empty() {
                note.push(' ');
            }
            note.push_str(missing);
        }
        if self.agent == "codex" {
            if !note.is_empty() {
                note.push(' ');
            }
            note.push_str(codex::HOOK_REVIEW);
        }
        (!note.is_empty()).then_some(note)
    }
}

/// Install the hook integration for `agent`, baking `taw_bin` — an absolute path to the
/// `taw` binary, typically `std::env::current_exe()` — into the generated script(s).
///
/// Safe to call again over an existing install: any previously-installed hook entry is
/// replaced (not duplicated), and the config file's pre-install bytes are preserved for
/// [`uninstall`] to restore even across a reinstall.
pub fn install(agent: &str, taw_bin: &Path) -> Result<Report> {
    match agent {
        "claude" => claude::install(taw_bin),
        "codex" => codex::install(taw_bin),
        "copilot" => copilot::install(taw_bin),
        "opencode" => opencode::install(taw_bin),
        "devin" => devin::install(taw_bin),
        "droid" => droid::install(taw_bin),
        "qodercli" => qodercli::install(taw_bin),
        "qwen" => qwen::install(taw_bin),
        "grok" => grok::install(taw_bin),
        "cursor" => cursor::install(taw_bin),
        "antigravity-cli" => antigravity_cli::install(taw_bin),
        "mastracode" => mastracode::install(taw_bin),
        "kimi" => kimi::install(taw_bin),
        "kilo" => kilo::install(taw_bin),
        "hermes" => hermes::install(taw_bin),
        "pi" => pi::install(taw_bin),
        "omp" => omp::install(taw_bin),
        "letta" => letta::install(taw_bin),
        other => bail!("unknown integration agent {other:?} (expected one of {AGENTS:?})"),
    }
}

/// Remove the hook integration for `agent`. Reverts any config file edit — byte-for-byte,
/// if nothing else has touched the file since install — and deletes the installed
/// script/plugin. A no-op (not an error) if `agent` was never installed.
pub fn uninstall(agent: &str) -> Result<Report> {
    match agent {
        "claude" => claude::uninstall(),
        "codex" => codex::uninstall(),
        "copilot" => copilot::uninstall(),
        "opencode" => opencode::uninstall(),
        "devin" => devin::uninstall(),
        "droid" => droid::uninstall(),
        "qodercli" => qodercli::uninstall(),
        "qwen" => qwen::uninstall(),
        "grok" => grok::uninstall(),
        "cursor" => cursor::uninstall(),
        "antigravity-cli" => antigravity_cli::uninstall(),
        "mastracode" => mastracode::uninstall(),
        "kimi" => kimi::uninstall(),
        "kilo" => kilo::uninstall(),
        "hermes" => hermes::uninstall(),
        "pi" => pi::uninstall(),
        "omp" => omp::uninstall(),
        "letta" => letta::uninstall(),
        other => bail!("unknown integration agent {other:?} (expected one of {AGENTS:?})"),
    }
}

/// Report whether `agent`'s integration is installed.
pub fn status(agent: &str) -> Result<Status> {
    match agent {
        "claude" => claude::status(),
        "codex" => codex::status(),
        "copilot" => copilot::status(),
        "opencode" => opencode::status(),
        "devin" => devin::status(),
        "droid" => droid::status(),
        "qodercli" => qodercli::status(),
        "qwen" => qwen::status(),
        "grok" => grok::status(),
        "cursor" => cursor::status(),
        "antigravity-cli" => antigravity_cli::status(),
        "mastracode" => mastracode::status(),
        "kimi" => kimi::status(),
        "kilo" => kilo::status(),
        "hermes" => hermes::status(),
        "pi" => pi::status(),
        "omp" => omp::status(),
        "letta" => letta::status(),
        other => bail!("unknown integration agent {other:?} (expected one of {AGENTS:?})"),
    }
}

/// [`status`] for every agent in [`AGENTS`]. Infallible: an agent whose status can't be
/// determined is reported as not installed rather than propagating an error.
pub fn status_all() -> Vec<Status> {
    AGENTS
        .iter()
        .map(|agent| {
            status(agent).unwrap_or_else(|_| Status {
                agent: (*agent).to_string(),
                installed: false,
                path: PathBuf::new(),
                version: None,
                current: None,
                incomplete: None,
            })
        })
        .collect()
}

// -----------------------------------------------------------------------------------
// Shared helpers used by the per-agent modules above: one agent per file, and the JSON-edit
// helpers here. `hermes_yaml` is separate because it is a whole line-based YAML editor that
// only Hermes needs.
// -----------------------------------------------------------------------------------

/// `$var` if set and non-empty, else `None`. Read at call time (not cached) so tests can
/// override it per-call via `tempfile` dirs.
pub(crate) fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// [`env_dir`] for an XDG base-directory variable, which additionally must be absolute:
/// "If an implementation encounters a relative path in any of these variables it should
/// consider the path invalid and ignore it." Same rule as `paths::xdg_env_dir`.
pub(crate) fn xdg_env_dir(var: &str) -> Option<PathBuf> {
    env_dir(var).filter(|p| p.is_absolute())
}

/// The user's home directory, or an error.
///
/// Deliberately not falling back to `.`: every caller joins an agent's config directory
/// onto this (`~/.claude`, `~/.codex`, `~/.config/opencode`), so a fallback of `.` would
/// point the installer at `./.claude/hooks/` — writing hook scripts into whatever
/// directory `taw` happened to be run from, most likely the user's project, and reporting
/// success. Refusing is the only honest answer: an agent's configuration lives at a fixed
/// place under the user's home, and without a home there is no such place.
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| {
        anyhow!(
            "cannot determine the home directory (neither $HOME nor the platform lookup \
             answered), so there is no path to install an agent integration at"
        )
    })
}

/// Require that `dir` — an agent's own config directory, e.g. `~/.codex` — already exists.
///
/// herdr requires this before installing an integration: a missing config dir means the
/// agent itself isn't installed, so writing into it would fabricate config for software
/// that might not even be present on this machine. This is a hard gate on the top-level
/// config dir only — subdirectories *inside* an existing config dir (`hooks/`, `plugins/`)
/// are still created as needed by the normal write path.
pub(crate) fn require_config_dir(dir: &Path, agent: &str) -> Result<()> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(IntegrationError::ConfigDirMissing {
            path: dir.display().to_string(),
            agent: agent.to_string(),
        }
        .into())
    }
}

/// Errors this module reports as a specific, matchable code (via `Error::downcast`)
/// rather than only a formatted message — mirroring `plugin::install::InstallError` and
/// `session::worktree::WorktreeError`. `cli::envelope::code_for` maps these onto the
/// `{"error":{"code":…}}` vocabulary; a formatted string could not be matched on, so a
/// missing config dir would surface as the catch-all `internal`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    #[error("config_dir_missing: {path} does not exist. Is {agent} installed?")]
    ConfigDirMissing { path: String, agent: String },
    /// The agent's hooks file parses as JSON but is not the shape a hook can be added to
    /// — `{"hooks": null}`, `{"hooks": []}`, `{"hooks": {"SessionStart": {}}}`, a root
    /// that is not an object. The file belongs to the user, so this is reported and the
    /// file left alone; `detail` names the JSON path and what was found at it.
    #[error("{path} is not a hooks file this can edit: {detail}")]
    UnusableHooksFile { path: String, detail: String },
    /// The file to be edited exists and its owner has not left it writable to this user.
    /// A writable directory would still let a rename replace it, so the write is refused
    /// instead: someone made it read-only on purpose, or a tool that manages it did. The
    /// message is the one the CLI prints, on an uninstall (a config made read-only after
    /// install) as on an install.
    #[error("{path} is read-only, so it was left as it is; make it writable and try again")]
    ConfigReadOnly { path: String },
    /// A config the agent reads that an install would have to edit, and cannot: it is not
    /// JSON (or JSONC), or it holds a plugin list that is not a list. Like
    /// [`UnusableHooksFile`](Self::UnusableHooksFile) the file is the user's, so it is
    /// reported and left exactly as it was found; `detail` says what was wrong with it.
    #[error("{path} is not a config this can edit: {detail}")]
    UnusableConfig { path: String, detail: String },
    /// A hook in the config names a file called as taw's hook script is, and what it names cannot
    /// be told to be the script taw installed: the path does not resolve (a folder that has gone,
    /// or a link that is not there any more), or it is not a path that can be resolved without a
    /// shell. It may be taw's, written when the folder was spelled another way; it may be
    /// somebody else's. An uninstall that went on would delete the script and the saved copy of
    /// the config either way, so it refuses and changes nothing, and the person decides.
    #[error(
        "{path} has a hook that names {spelled}, and taw cannot tell whether that is the script it \
         installed at {script}. Nothing was changed. If the hook is taw's, remove it from the file \
         by hand, and uninstall again."
    )]
    UnmatchedHook {
        path: String,
        spelled: String,
        script: String,
    },
}

/// Which language `render_template` writes `{{TAW_BIN}}` in. The placeholder stands for the whole
/// expression (quotes included), so a template says `TAW_BIN={{TAW_BIN}}` and the render picks
/// the quoting: a path under the home folder is written relative to it, in the language's own
/// syntax, so a config or a plugin that is copied to another machine, or kept in a dotfiles
/// repository, does not carry the home folder in it.
pub(crate) enum TemplateLang {
    /// POSIX `sh`: `'/abs/path'`, or `"$HOME/rest"` under the home folder.
    Sh,
    /// The JavaScript and TypeScript plugins (OpenCode, Kilo, Pi, OMP): `"/abs/path"`, or
    /// `` `${process.env.HOME}/rest` `` under the home folder.
    Js,
    /// The Hermes plugin, in Python: `"/abs/path"`, or
    /// `os.path.join(os.path.expanduser("~"), "rest")` under the home folder.
    Py,
}

/// What follows the home folder in `path`, when `path` lies below it, and is a plain path that
/// every language writes as it is: none of `$`, a backtick, `\`, `"` and `'` (which a shell, a
/// template literal or a string reads specially) and no control character. A path with one of
/// them is written in full, the way it always was. A purely textual test: the path is spelled
/// as the caller spells it, and a home folder that is reached through a link is not recognised,
/// so the path stays absolute.
pub(crate) fn below_home(path: &Path, home: Option<&Path>) -> Option<String> {
    let home = home.filter(|home| home.is_absolute() && home.parent().is_some())?;
    let rest = path.strip_prefix(home).ok()?.to_str()?;
    let plain = !rest.is_empty()
        && !rest
            .chars()
            .any(|c| c.is_control() || matches!(c, '$' | '`' | '\\' | '"' | '\''));
    plain.then(|| rest.to_string())
}

/// `path` as one POSIX shell word: `"$HOME/rest"` when it lies below `home`, else the absolute
/// path in single quotes. This is the word of a hook entry, which an agent's shell runs with the
/// agent's own environment, so `$HOME` is set.
pub(crate) fn sh_word_in(path: &Path, home: Option<&Path>) -> String {
    match below_home(path, home) {
        Some(rest) => format!("\"$HOME/{rest}\""),
        None => sh_absolute(path),
    }
}

/// `path` in single quotes, the one quoting that reads every character as itself.
fn sh_absolute(path: &Path) -> String {
    // Close the quote, emit an escaped quote, reopen: the standard POSIX-sh technique.
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// [`sh_word_in`] against the home folder installs use.
pub(crate) fn sh_word(path: &Path) -> String {
    sh_word_in(path, home_dir().ok().as_deref())
}

/// `path` as the value of a hook script's `TAW_BIN`: `"${HOME-}/rest"` when it lies below `home`,
/// else the absolute path in single quotes. A script runs under `set -u`, and with no `HOME`
/// that word is `/rest`, which is no file: its own `[ -x "$TAW_BIN" ]` ends it quietly, where
/// `"$HOME/rest"` would stop it with an error that an agent shows.
fn sh_script_word(path: &Path, home: Option<&Path>) -> String {
    match below_home(path, home) {
        Some(rest) => format!("\"${{HOME-}}/{rest}\""),
        None => sh_absolute(path),
    }
}

/// `path` as a JavaScript expression: `` `${process.env.HOME}/rest` `` when it lies below `home`,
/// else the absolute path as a double-quoted string.
fn js_expression(path: &Path, home: Option<&Path>) -> String {
    match below_home(path, home) {
        Some(rest) => format!("`${{process.env.HOME}}/{rest}`"),
        None => format!("\"{}\"", double_quoted_escape(&path.to_string_lossy())),
    }
}

/// `path` as a Python expression: `os.path.join(os.path.expanduser("~"), "rest")` when it lies
/// below `home`, else the absolute path as a double-quoted string.
fn py_expression(path: &Path, home: Option<&Path>) -> String {
    match below_home(path, home) {
        Some(rest) => format!("os.path.join(os.path.expanduser(\"~\"), \"{rest}\")"),
        None => format!("\"{}\"", double_quoted_escape(&path.to_string_lossy())),
    }
}

/// `text` escaped for a double-quoted string in JavaScript or Python, which escape a backslash and
/// a double quote alike.
fn double_quoted_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render a template's `{{TAW_BIN}}` / `{{AGENT}}` placeholders. `taw_bin` is quoted and escaped
/// for the destination language so a path containing a quote can't break the generated file, and
/// written relative to the home folder where it lies under it ([`TemplateLang`]).
pub(crate) fn render_template(
    template: &str,
    lang: TemplateLang,
    taw_bin: &Path,
    agent: &str,
) -> String {
    render_template_in(template, lang, taw_bin, agent, home_dir().ok().as_deref())
}

/// [`render_template`] for a home folder that is given, `None` when there is none.
pub(crate) fn render_template_in(
    template: &str,
    lang: TemplateLang,
    taw_bin: &Path,
    agent: &str,
    home: Option<&Path>,
) -> String {
    let bin = match lang {
        TemplateLang::Sh => sh_script_word(taw_bin, home),
        TemplateLang::Js => js_expression(taw_bin, home),
        TemplateLang::Py => py_expression(taw_bin, home),
    };
    template
        .replace("{{TAW_BIN}}", &bin)
        .replace("{{AGENT}}", agent)
}

/// Parse a `<prefix><digits>` version marker out of a rendered file's text (e.g.
/// `# TAW_INTEGRATION_VERSION=1` with `prefix = "# TAW_INTEGRATION_VERSION="`).
pub(crate) fn parse_version_marker(contents: &str, prefix: &str) -> Option<u32> {
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix)?.trim().parse().ok())
}

// write_config_file: a protected write for every config and script this crate manages.
// Follows herdr's src/integration/config_file.rs (`write_config`, `Replacement`), trimmed
// to what this crate needs: no xattr, ACL or ownership copying (taw runs as one user, not a
// multi-user desktop app), and `std::fs::rename` instead of herdr's `platform::replace_file`,
// since POSIX `rename(2)` is atomic for a same-directory replace, which staging beside the
// target guarantees. Errors are anyhow, a read-only config is a typed
// IntegrationError::ConfigReadOnly, and herdr's config target check is part of planning an
// edit (ConfigEdit), so it runs before an install writes its script.
// -----------------------------------------------------------------------------------

/// How many symlink hops [`resolve_config_target`] follows before giving up — generous
/// for any real chain, but never infinite for a symlink cycle.
const MAX_SYMLINK_HOPS: u32 = 40;

/// Resolves `path` past any chain of symlinks to the real file it (or nothing yet) names,
/// so a write can replace what a symlink points at rather than the symlink itself. Unlike
/// `Path::canonicalize`, this also resolves a *dangling* symlink — the usual case the
/// first time an integration is installed through a not-yet-existing dotfiles link.
///
/// Follows herdr's `config_file.rs` `resolve_target`.
fn resolve_config_target(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)
                    .with_context(|| format!("reading symlink {}", current.display()))?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().unwrap_or(Path::new(".")).join(link)
                };
            }
            Ok(metadata) if !metadata.is_file() => {
                bail!(
                    "cannot update {}: config is not a regular file",
                    path.display()
                );
            }
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", current.display()));
            }
        }
    }
    bail!("cannot update {}: too many symbolic links", path.display());
}

/// Refuses a config with more than one hard link: replacing one of its names (which is
/// all a rename-based atomic write can do) would silently leave the other holding the old
/// content — unlike a symlink, which [`resolve_config_target`] follows on purpose.
/// `fs::metadata` follows symlinks, so this checks the *resolved* target's link count even
/// when `path` itself is a symlink, not `path`'s own (always 1).
///
/// Follows herdr's `config_file.rs` `reject_hard_links`.
fn reject_hard_link(path: &Path) -> Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.is_file() && metadata.nlink() > 1 {
            bail!(
                "cannot update {}: config has multiple hard links; use a separate file or a \
                 symlink before retrying",
                path.display()
            );
        }
    }
    Ok(())
}

/// Refuses an existing config that this user has not been left able to write
/// ([`IntegrationError::ConfigReadOnly`]). `target` is `path` with its symlinks resolved.
///
/// A writable *directory* is enough for a rename to replace a file whatever the file's own
/// mode says, so without this a read-only config would be overwritten anyway. Opening it
/// for writing (no truncation, nothing written) asks the question a plain write would have
/// been asked: what the mode bits, an ACL, a read-only mount or a file flag answer.
///
/// Follows herdr's `Replacement::prepare`, which opens the existing target read-write
/// first ("A writable parent must not let rename bypass file write permissions").
fn check_writable(path: &Path, target: &Path) -> Result<()> {
    use std::io::ErrorKind;
    match fs::OpenOptions::new().read(true).write(true).open(target) {
        Ok(_) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            let shown = if path == target {
                path.display().to_string()
            } else {
                format!("{} (a link to {})", path.display(), target.display())
            };
            Err(IntegrationError::ConfigReadOnly { path: shown }.into())
        }
        Err(err) => Err(err).with_context(|| format!("opening {} for writing", target.display())),
    }
}

static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A process-unique suffix for [`write_config_file`]'s staging file name, so two writes
/// racing in the same directory (two `taw integration install` runs, or two files this
/// crate happens to stage at once) never collide on the same temporary path.
fn next_temp_id() -> u64 {
    NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Atomically replaces the bytes of a config or script file this crate manages, so a
/// write that fails partway — a full disk, a read-only directory, a process killed
/// mid-write — never leaves `path` truncated or holding only some of `contents`: the new
/// content is written and `fsync`ed to a staging file beside `path` first, and only *then*
/// renamed into place.
///
/// If `path` is a symlink (or a chain of them, even a dangling one), the write follows it
/// and replaces what it points at, leaving the symlink itself in place — a dotfiles-style
/// managed config keeps working exactly as it did before taw touched it. A config with
/// more than one hard link is refused outright ([`reject_hard_link`]), checked once before
/// staging and again immediately before the publishing rename. An existing target that
/// this user cannot write is refused too ([`check_writable`]): the staged rename would
/// otherwise replace it, whatever its own mode says. An existing target's Unix permission
/// bits are copied onto the replacement before it is published; a brand new file gets the
/// ordinary create/umask defaults, the same as a plain write would.
pub(crate) fn write_config_file(path: &Path, contents: &[u8]) -> Result<()> {
    write_config_file_as(path, contents, None)
}

/// [`write_config_file`], and a file that does not exist yet is given `permissions` in place of
/// the ordinary create/umask defaults. It is written the way an existing one is replaced: private
/// until its content is all there, and only then given them.
fn write_config_file_as(
    path: &Path,
    contents: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<()> {
    reject_hard_link(path)?;
    let target = resolve_config_target(path)?;
    let existing_permissions = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", target.display())),
    };
    if existing_permissions.is_some() {
        check_writable(path, &target)?;
    }
    // What the replacement ends up with: an existing file's own, else what a new one was given.
    let permissions = existing_permissions.or_else(|| permissions.cloned());

    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    let mut open_options = fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // An existing config can hold secrets: the staging file starts private, and only
        // widens to the original mode once its content is fully written — never a moment
        // where a partially-written secret sits world-readable. A brand new file has
        // nothing yet to protect, so it takes the ordinary create/umask default instead,
        // unless it was given the permissions of the file it is a copy of.
        open_options.mode(if permissions.is_some() { 0o600 } else { 0o666 });
    }

    let mut temp_path = parent.join(format!(".taw-config-{}.tmp", std::process::id()));
    let mut file = loop {
        temp_path.set_file_name(format!(
            ".taw-config-{}-{}.tmp",
            std::process::id(),
            next_temp_id()
        ));
        match open_options.open(&temp_path) {
            Ok(file) => break file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", temp_path.display()));
            }
        }
    };

    let write_result = (|| -> Result<()> {
        file.write_all(contents)
            .with_context(|| format!("writing {}", temp_path.display()))?;
        if let Some(permissions) = &permissions {
            file.set_permissions(permissions.clone())
                .with_context(|| format!("setting permissions on {}", temp_path.display()))?;
        }
        file.sync_all()
            .with_context(|| format!("syncing {}", temp_path.display()))?;
        Ok(())
    })();
    drop(file);
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    // A last check immediately before the rename that publishes the write: something
    // could have hard-linked `target` while the staging file above was being written.
    if let Err(err) = reject_hard_link(&target) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) =
        fs::rename(&temp_path, &target).with_context(|| format!("writing {}", target.display()))
    {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

/// Write `contents` to `path`, creating parent directories and (on unix) marking the file
/// executable.
pub(crate) fn write_executable(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    write_config_file(path, contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

fn pretty_json_bytes(value: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(value).expect("serde_json::Value always serializes");
    out.push(b'\n');
    out
}

/// How a hook's text relates to the script taw installed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// It names the script taw installed.
    Ours,
    /// It names the script taw installed, by a path spelled from the home folder (`~/...`,
    /// `$HOME/...` or `${HOME}/...`), which taw's own installs write too. It is taw's: an install
    /// writes its entry in the current form with the path word as it is spelled here
    /// ([`upsert_hook_entry_of`]), and an uninstall takes it out.
    OursFromHome,
    /// It names a file called as the script is, and what it names cannot be told to be the
    /// script: the path does not resolve (a folder that has gone, or a link that is not there
    /// any more), or it is not a path that can be resolved without a shell (`./x`, `~other/x`,
    /// another variable, a bare name), or it starts from the home folder and there is none to
    /// start from. It carries the path as it is spelled.
    Unsure(String),
    /// It does not name the script, or names a file of the same name in a folder that exists and
    /// is another one.
    Foreign,
}

/// The script taw installed, and what it takes for a hook to be taw's.
///
/// A hook is taw's when it names the script's file name by a path that is the script's own,
/// however the folder is spelled: the entry an install writes carries the path as `$CLAUDE_CONFIG_DIR`
/// was spelled then, and one Claude folder can be reachable as `~/.claude-work` and as
/// `~/.claude-alt`, the same directory through links. Comparing the text of the path found it
/// under one spelling only, so an uninstall under the other left the entry in the config and
/// deleted the script it ran. The file name is read off the script's path, and the folder of the
/// path an entry names is resolved ([`std::fs::canonicalize`]) and compared with the script's.
struct Needle {
    /// The script's path as this run spells it, which is what an install writes into an entry.
    script: String,
    /// The script's file name.
    name: String,
    /// Where the script is, with every link in its folder resolved: `None` when that folder is
    /// not there.
    resolved: Option<PathBuf>,
    /// The home folder a path that starts with `~/`, `$HOME/` or `${HOME}/` is expanded against.
    /// `None` for an agent whose hook runner is not known to run the command through a shell
    /// ([`Needle::new`]), whose entry may never have expanded such a path, and when there is no
    /// home folder: then such a path is unsure.
    home: Option<PathBuf>,
}

impl Needle {
    /// A needle for an agent whose hook runner is not known to run the command through a shell:
    /// `~/...` and `$HOME/...` are unsure, since its runner may never expand them.
    fn new(script: &str) -> Needle {
        Needle::in_home(script, None)
    }

    /// A needle for an agent whose hook runner is a shell (Claude's): `~/...`, `$HOME/...` and
    /// `${HOME}/...` are expanded against the home folder installs use.
    fn for_shell(script: &str) -> Needle {
        Needle::in_home(script, home_dir().ok())
    }

    /// [`Needle::new`] for a home folder that is given.
    fn in_home(script: &str, home: Option<PathBuf>) -> Needle {
        let path = Path::new(script);
        Needle {
            home: home.filter(|home| home.is_absolute()),
            script: script.to_string(),
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            resolved: resolved(path),
        }
    }

    /// What `value` is to taw: the strongest of what its strings are (taw's beats unsure, which
    /// beats not).
    #[cfg(test)]
    fn judge(&self, value: &Value) -> Owner {
        self.judge_spelled(value, &mut None)
    }

    /// [`Needle::judge`], and when the verdict rests on a path spelled from the home folder,
    /// `spelling` gets that path word as the text spells it (quotes included), when the word is
    /// the path and nothing else.
    fn judge_spelled(&self, value: &Value, spelling: &mut Option<String>) -> Owner {
        match value {
            Value::String(text) => self.judge_text_spelled(text, spelling),
            Value::Array(items) => {
                strongest(items.iter().map(|item| self.judge_spelled(item, spelling)))
            }
            Value::Object(map) => {
                strongest(map.values().map(|item| self.judge_spelled(item, spelling)))
            }
            _ => Owner::Foreign,
        }
    }

    /// What one string is to taw: it names the script by the path an install writes, or it names
    /// a file called as the script is, by a path whose folder is the script's folder.
    ///
    /// The path an install writes has to stand as a path of its own: it begins the string or
    /// follows a quote or a space, and it does not run on into a longer name. A path it is only
    /// the end of (a copy of the home folder under `/Volumes/Backup`) is another folder's, and is
    /// judged as any other path is, by where its folder is.
    ///
    /// Any other mention is read as a shell reads it ([`shell_scan`]): a comment (from a `#` that
    /// starts a word to the end of the line) is not judged, and the word that holds the
    /// name is the path when its value starts with `/`, and a path from the home folder when it
    /// starts with `~/` unquoted, or `$HOME/` or `${HOME}/` unquoted or in double quotes. Any
    /// other word that holds the name (a path in single quotes that starts `$HOME`, a word with
    /// `~/` in the middle of it, `~other/`, another variable, a command substitution) is not a
    /// path that can be told without a shell, and is unsure.
    #[cfg(test)]
    fn judge_text(&self, text: &str) -> Owner {
        self.judge_text_spelled(text, &mut None)
    }

    fn judge_text_spelled(&self, text: &str, spelling: &mut Option<String>) -> Owner {
        let scan = shell_scan(text);
        // What a shell would run: a comment is not judged.
        let live = scan.without_comments(text);
        let names_it_as_installed = live.match_indices(&self.script).any(|(start, found)| {
            let before = live[..start].chars().next_back();
            let after = live[start + found.len()..].chars().next();
            before.is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\''))
                && !after.is_some_and(in_a_name)
        });
        if names_it_as_installed {
            return Owner::Ours;
        }
        let mut unsure = None;
        let mut from_home = false;
        for word in scan.words {
            for (_, end) in mentions(&word.value, &self.name) {
                let path = &word.value[..end];
                let verdict = if path.starts_with('/') {
                    self.judge_path(path, Path::new(path), false)
                } else {
                    match (word.home_prefix(), &self.home) {
                        (Some(prefix), Some(home)) => {
                            let rest = path[prefix..].trim_start_matches('/');
                            self.judge_path(path, &home.join(rest), true)
                        }
                        _ => Owner::Unsure(self.name.clone()),
                    }
                };
                match verdict {
                    Owner::Ours => return Owner::Ours,
                    Owner::OursFromHome => {
                        from_home = true;
                        if end == word.value.len() && spelling.is_none() {
                            *spelling = Some(text[word.start..word.end].to_string());
                        }
                    }
                    Owner::Unsure(spelled) => {
                        unsure.get_or_insert(spelled);
                    }
                    Owner::Foreign => {}
                }
            }
        }
        if from_home {
            return Owner::OursFromHome;
        }
        unsure.map_or(Owner::Foreign, Owner::Unsure)
    }

    /// What the file at `path` is to taw, by where its folder is. `spelled` is the path as the
    /// text spells it, which an unsure verdict carries; `from_home` says it was spelled from the
    /// home folder and expanded to `path`.
    ///
    /// A path that is the script's own path as text is taw's whether or not its folder is there:
    /// a settings file from a dotfiles checkout can name the hook before the first install has
    /// made the folder.
    fn judge_path(&self, spelled: &str, path: &Path, from_home: bool) -> Owner {
        let ours = if from_home {
            Owner::OursFromHome
        } else {
            Owner::Ours
        };
        if path == Path::new(&self.script) {
            return ours;
        }
        match resolved(path) {
            None => Owner::Unsure(spelled.to_string()),
            Some(there) if self.resolved.as_deref() == Some(there.as_path()) => ours,
            Some(_) => Owner::Foreign,
        }
    }
}

/// The strongest of `owners`: taw's, else taw's by a path spelled from the home folder, else the
/// first that is unsure, else foreign.
fn strongest(owners: impl Iterator<Item = Owner>) -> Owner {
    let mut unsure = None;
    let mut from_home = false;
    for owner in owners {
        match owner {
            Owner::Ours => return Owner::Ours,
            Owner::OursFromHome => from_home = true,
            Owner::Unsure(spelled) => {
                unsure.get_or_insert(spelled);
            }
            Owner::Foreign => {}
        }
    }
    if from_home {
        return Owner::OursFromHome;
    }
    unsure.map_or(Owner::Foreign, Owner::Unsure)
}

/// Where the file `path` is, with every link in its folder resolved: the folder's canonical path
/// joined with the file's name. The file itself need not be there, only its folder, and a script
/// that is itself a link is left as it is: it is the folder that is spelled more than one way.
fn resolved(path: &Path) -> Option<PathBuf> {
    let folder = fs::canonicalize(path.parent()?).ok()?;
    Some(folder.join(path.file_name()?))
}

/// Whether `c` can be part of a file's name, so that a name that runs on into one is a longer
/// name and not the one that was looked for (`my-taw-agent-state.sh`, or the `.orig.json` copy
/// of a config that is kept beside the script).
fn in_a_name(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// The byte ranges of `name` in `text` where it stands as a name of its own.
fn mentions<'a>(text: &'a str, name: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
    text.match_indices(name).filter_map(move |(start, found)| {
        let end = start + found.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        (!before.is_some_and(in_a_name) && !after.is_some_and(in_a_name)).then_some((start, end))
    })
}

/// How the shell read a character of a word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reading {
    /// Outside quotes.
    Plain,
    /// Inside double quotes.
    Double,
    /// Inside single quotes, or escaped by a backslash: it stands for itself.
    Literal,
}

/// A word of a command as a POSIX shell reads it: its value with the quotes taken out, and for
/// each byte of the value how it was read and in which run of the text (a run ends at a quote or
/// an escape).
struct Word {
    value: String,
    marks: Vec<(Reading, usize)>,
    /// Where the word is in the text it was read from.
    start: usize,
    end: usize,
}

/// The prefixes of a path that starts from the home folder, and nothing else does: `~other/x`,
/// `$HOME2/x`, `$XDG_CONFIG_HOME/x` and `./x` need a real shell to be told what they are.
const HOME_PREFIXES: [&str; 3] = ["~/", "$HOME/", "${HOME}/"];

impl Word {
    /// The length of the prefix that stands for the home folder when the shell expands it:
    /// `~/` outside quotes, `$HOME/` or `${HOME}/` outside quotes or inside double quotes. For
    /// `~/` all of it is one run; for the variables only the name is (`$HO"ME"/` and `$HO\ME/`
    /// are other variables, and `'$HOME/'` and `\~/` are text), and the `/` after it may be
    /// quoted any way (`"$HOME"/x`, `$HOME"/x"`, `$HOME'/'x`), since the shell expands the
    /// variable before it joins the pieces of the word.
    fn home_prefix(&self) -> Option<usize> {
        HOME_PREFIXES.iter().find_map(|prefix| {
            if !self.value.starts_with(prefix) {
                return None;
            }
            let tilde = *prefix == "~/";
            let marks = &self.marks[..prefix.len()];
            let (reading, run) = marks[0];
            let expands = match reading {
                Reading::Plain => true,
                Reading::Double => !tilde,
                Reading::Literal => false,
            };
            let one_run = if tilde {
                marks
            } else {
                &marks[..prefix.len() - 1]
            };
            (expands && one_run.iter().all(|mark| *mark == (reading, run))).then_some(prefix.len())
        })
    }
}

/// A command as a POSIX shell reads it: its words, and the comments between them.
struct Scan {
    words: Vec<Word>,
    /// The byte ranges of the comments, each from its `#` to the end of its line.
    comments: Vec<(usize, usize)>,
}

impl Scan {
    /// `text` with the comments cut out.
    fn without_comments(&self, text: &str) -> String {
        let mut live = String::new();
        let mut from = 0;
        for &(start, end) in &self.comments {
            live.push_str(&text[from..start]);
            from = end;
        }
        live.push_str(&text[from..]);
        live
    }
}

/// `text` split into words as a POSIX shell splits a command: at a space, a tab or a newline
/// outside quotes (not at any other blank, such as a non-breaking space), and at an operator
/// outside quotes (`;`, `&`, `|`, `(`, `)`, `<`, `>`), and a `#` that starts
/// a word starts a comment that runs to the end of the line. Inside single quotes every character is literal; inside double quotes a backslash escapes
/// only `$`, a backtick, `"`, `\` and a newline; outside quotes a backslash escapes the next
/// character. A quote that is not closed runs to the end.
fn shell_scan(text: &str) -> Scan {
    fn open(current: &mut Option<Word>, at: usize) -> &mut Word {
        current.get_or_insert_with(|| Word {
            value: String::new(),
            marks: Vec::new(),
            start: at,
            end: at,
        })
    }
    fn push(current: &mut Option<Word>, at: usize, c: char, mark: (Reading, usize)) {
        let word = open(current, at);
        word.value.push(c);
        word.marks.extend(std::iter::repeat_n(mark, c.len_utf8()));
    }

    let mut words = Vec::new();
    let mut comments = Vec::new();
    let mut current: Option<Word> = None;
    // The run a character belongs to: a quote or an escape starts another.
    let mut run = 0;
    let mut double = false;
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        if double {
            match c {
                '"' => {
                    double = false;
                    run += 1;
                }
                '\\' => {
                    let mut ahead = chars.clone();
                    match ahead.next() {
                        Some((_, '\n')) => chars = ahead,
                        Some((_, next @ ('$' | '`' | '"' | '\\'))) => {
                            chars = ahead;
                            run += 1;
                            push(&mut current, at, next, (Reading::Literal, run));
                            run += 1;
                        }
                        _ => push(&mut current, at, c, (Reading::Double, run)),
                    }
                }
                _ => push(&mut current, at, c, (Reading::Double, run)),
            }
            continue;
        }
        match c {
            // An unquoted operator ends the word before it, as a blank does, so a `#` right
            // after one starts a word and opens a comment.
            ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                if let Some(mut word) = current.take() {
                    word.end = at;
                    words.push(word);
                }
            }
            '#' if current.is_none() => {
                let mut end = text.len();
                for (next_at, next) in chars.by_ref() {
                    if next == '\n' {
                        end = next_at;
                        break;
                    }
                }
                comments.push((at, end));
            }
            '\\' => match chars.next() {
                Some((_, '\n')) => {}
                Some((_, next)) => {
                    run += 1;
                    push(&mut current, at, next, (Reading::Literal, run));
                    run += 1;
                }
                None => push(&mut current, at, c, (Reading::Literal, run)),
            },
            '\'' => {
                run += 1;
                open(&mut current, at);
                for (quoted_at, quoted) in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    push(&mut current, quoted_at, quoted, (Reading::Literal, run));
                }
                run += 1;
            }
            '"' => {
                run += 1;
                open(&mut current, at);
                double = true;
            }
            _ => push(&mut current, at, c, (Reading::Plain, run)),
        }
    }
    if let Some(mut word) = current.take() {
        word.end = text.len();
        words.push(word);
    }
    Scan { words, comments }
}

/// Where an agent keys its per-event hook arrays inside its config file: `Some(key)` for
/// the usual nesting under a `hooks` object (`{"hooks": {"SessionStart": [...]}}`), `None`
/// when the file root *is* the event map (`{"SessionStart": [...]}`). Both are an agent's
/// own documented format, so each is edited in place rather than normalised.
pub(crate) type HookContainer = Option<&'static str>;

/// The container the four original ports — and most of the 13 ported after them — use.
pub(crate) const HOOKS_KEY: HookContainer = Some("hooks");

/// MastraCode's container: the file root itself is the event map.
pub(crate) const ROOT_EVENTS: HookContainer = None;

/// The JSON path prefix used in error messages for `container`.
fn container_label(container: HookContainer) -> &'static str {
    container.unwrap_or("<root>")
}

/// The event map inside `root`, created when absent. `Err` if `root` — or the key it holds
/// — is not an object.
fn event_map(root: &mut Value, container: HookContainer) -> Result<&mut Map<String, Value>> {
    match container {
        None => object_mut(root, "<root>"),
        Some(key) => {
            let slot = object_mut(root, "<root>")?
                .entry(key)
                .or_insert_with(|| Value::Object(Map::new()));
            object_mut(slot, key)
        }
    }
}

/// The event map inside `root` if it is already there and is an object, else `None` — the
/// read-only counterpart of [`event_map`], which never creates anything.
fn existing_event_map(
    root: &mut Value,
    container: HookContainer,
) -> Option<&mut Map<String, Value>> {
    match container {
        None => root.as_object_mut(),
        Some(key) => root.get_mut(key).and_then(Value::as_object_mut),
    }
}

/// A hook of taw's that is spelled from the home folder, as [`remove_owned_hooks`] found it.
struct FromHome {
    /// The path word as the hook spells it (`~/.claude/hooks/taw-agent-state.sh`, with its quotes
    /// when it has them); `None` when the word is more than the path.
    word: Option<String>,
    /// Where the entry that held it stood among the entries that are left.
    at: usize,
}

/// What taking taw's hooks out of one event's entries came to.
struct Removal {
    /// Whether anything went.
    removed: bool,
    /// What the first hook that cannot be told from taw's names ([`Owner::Unsure`]): it stays.
    unsure: Option<String>,
    /// The first hook of taw's that was spelled from the home folder ([`Owner::OursFromHome`]).
    from_home: Option<FromHome>,
}

/// Take taw's hook out of one event's `entries`: whatever names the script `needle` stands for,
/// however the folder it is in is spelled ([`Needle`]).
///
/// An entry is either a *group*, `{"matcher": ..., "hooks": [<hook>, ...]}` (Claude, Codex,
/// Devin, Droid, Qoder, Qwen and Letta), or a bare hook that is the whole entry (Copilot,
/// Cursor and MastraCode). Nothing says a group holds only taw's hook: a tool, or the user,
/// may have added theirs next to it, so only the hook that is taw's leaves the group, and
/// the group goes only when that empties it (herdr's `remove_command_hook`). A group that
/// was already empty was never taw's to remove and stays. A bare hook goes whole. A hook that
/// cannot be told from taw's is left where it is, and reported.
///
/// What a hook spelled from the home folder looked like and where it stood is reported too, so
/// that an install can write its entry in the same place with the same path word.
fn remove_owned_hooks(entries: &mut Vec<Value>, needle: &Needle) -> Removal {
    let mut removal = Removal {
        removed: false,
        unsure: None,
        from_home: None,
    };
    let mut kept = 0;
    entries.retain_mut(|entry| {
        let mut spelling = None;
        let mut from_home = false;
        let keep = match entry.get_mut("hooks").and_then(Value::as_array_mut) {
            Some(hooks) => {
                let before = hooks.len();
                hooks.retain(|hook| match needle.judge_spelled(hook, &mut spelling) {
                    Owner::Ours => false,
                    Owner::OursFromHome => {
                        from_home = true;
                        false
                    }
                    Owner::Unsure(spelled) => {
                        removal.unsure.get_or_insert(spelled);
                        true
                    }
                    Owner::Foreign => true,
                });
                let removed = hooks.len() != before;
                removal.removed |= removed;
                !(removed && hooks.is_empty())
            }
            None => match needle.judge_spelled(entry, &mut spelling) {
                Owner::Ours => {
                    removal.removed = true;
                    false
                }
                Owner::OursFromHome => {
                    removal.removed = true;
                    from_home = true;
                    false
                }
                Owner::Unsure(spelled) => {
                    removal.unsure.get_or_insert(spelled);
                    true
                }
                Owner::Foreign => true,
            },
        };
        if from_home && removal.from_home.is_none() {
            // In the entry's place when it went whole, else right after the group it was taken from.
            let at = if keep { kept + 1 } else { kept };
            removal.from_home = Some(FromHome { word: spelling, at });
        }
        kept += usize::from(keep);
        keep
    });
    removal
}

/// Push `entry` onto `<container>[event]` (creating the array/object path as needed),
/// after taking any hook already in that array which names the script `needle` stands for out of
/// it ([`remove_owned_hooks`], under whichever spelling of its folder), so reinstalling replaces
/// rather than duplicates. A hook that cannot be told from taw's stays, and the install goes on.
///
/// Every level is checked rather than asserted. This file belongs to the user and to the
/// agent, not to `taw`: `settings.json` is hand-edited, written by another tool, and
/// carried between machines, so `{"hooks": null}`, `{"hooks": []}` and `{"hooks":
/// {"SessionStart": {}}}` are all things that turn up. None may end the process in a
/// Rust panic. An `Err` naming the JSON path is the honest answer — the caller reports it
/// as `invalid_argument` and the file is left exactly as it was found.
fn upsert_hook_entry(
    root: &mut Value,
    container: HookContainer,
    event: &str,
    needle: &str,
    entry: Value,
) -> Result<()> {
    upsert_hook_entry_of(root, container, event, &Needle::new(needle), entry, None)
}

/// [`upsert_hook_entry`] for a [`Needle`] that is already made. `word` is how `entry` names the
/// script (`"$HOME/.claude/hooks/taw-agent-state.sh"`), for an agent whose runner is a shell.
///
/// An entry of taw's is always written in the current form: the matcher, the timeout and the
/// words of its command are the install's own. When the one it replaces named the script from
/// the home folder, it keeps the path word as it was spelled (`~/...`, `$HOME/...`) and the
/// place it stood in; an entry that named the script in full is replaced by the new entry at the
/// end, which names it from the home folder where it can.
fn upsert_hook_entry_of(
    root: &mut Value,
    container: HookContainer,
    event: &str,
    needle: &Needle,
    mut entry: Value,
    word: Option<&str>,
) -> Result<()> {
    let label = container_label(container);
    let slot = event_map(root, container)?
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()));
    let kind = json_kind(slot);
    let arr = slot
        .as_array_mut()
        .ok_or_else(|| anyhow!("{label}.{event} is {kind}, not an array"))?;
    match remove_owned_hooks(arr, needle).from_home {
        Some(found) => {
            if let (Some(written), Some(spelled)) = (word, found.word.as_deref()) {
                respell(&mut entry, written, spelled);
            }
            arr.insert(found.at.min(arr.len()), entry);
        }
        None => arr.push(entry),
    }
    Ok(())
}

/// Replace `written` with `spelled` in every string of `value`.
fn respell(value: &mut Value, written: &str, spelled: &str) {
    match value {
        Value::String(text) => *text = text.replace(written, spelled),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| respell(item, written, spelled)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| respell(item, written, spelled)),
        _ => {}
    }
}

/// `value` as a JSON object, or an `Err` naming `path` and what was found there instead.
fn object_mut<'a>(value: &'a mut Value, path: &str) -> Result<&'a mut Map<String, Value>> {
    let kind = json_kind(value);
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{path} is {kind}, not an object"))
}

/// The JSON type name of `value`, for an error message a user can act on.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Remove taw's hook (whatever names the script `needle` stands for, under whichever spelling of
/// its folder; [`remove_owned_hooks`] leaves a hook somebody else put in taw's group) from
/// `<container>[event]`, dropping the `event` key (and then the container key itself, when it has
/// one) if it becomes empty. Returns `true` if a hook of taw's was removed.
///
/// A hook that cannot be told from taw's ([`Owner::Unsure`]) is an error
/// ([`IntegrationError::UnmatchedHook`]), and `root` is left to be thrown away: a caller that
/// deletes the script when this is done must not leave a hook that may run it behind.
fn strip_hook_entries(
    root: &mut Value,
    container: HookContainer,
    event: &str,
    needle: &str,
) -> Result<bool> {
    strip_hook_entries_of(root, container, event, &Needle::new(needle))
}

/// [`strip_hook_entries`] for a [`Needle`] that is already made, for a caller that sweeps several
/// events.
fn strip_hook_entries_of(
    root: &mut Value,
    container: HookContainer,
    event: &str,
    needle: &Needle,
) -> Result<bool> {
    let Some(events) = existing_event_map(root, container) else {
        // No container object at all — including a `hooks` that is null, an array or a
        // string. There is nothing of ours in a shape we never wrote, so there is nothing
        // to strip, and nothing here may touch a file it does not understand.
        return Ok(false);
    };
    let Some(arr) = events.get_mut(event).and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let removal = remove_owned_hooks(arr, needle);
    if let Some(spelled) = removal.unsure {
        return Err(IntegrationError::UnmatchedHook {
            path: String::new(),
            spelled,
            script: needle.script.clone(),
        }
        .into());
    }
    if arr.is_empty() {
        events.remove(event);
    }
    let events_now_empty = events.is_empty();
    // A keyed container that we emptied goes too, so a file we found without a `hooks`
    // key does not keep an empty one afterwards. The *root* is never removed: an empty
    // root is what `write_json_value_or_remove` treats as "delete the file", and that
    // decision belongs to it, not here.
    if let Some(key) = container
        && events_now_empty
        && let Some(map) = root.as_object_mut()
    {
        map.remove(key);
    }
    Ok(removal.removed)
}

/// `err`, with the file it was found in named when it is an [`IntegrationError::UnmatchedHook`]:
/// the strip that found it is not told which file it works on.
fn located_in(err: anyhow::Error, path: &Path) -> anyhow::Error {
    match err.downcast::<IntegrationError>() {
        Ok(IntegrationError::UnmatchedHook {
            spelled, script, ..
        }) => IntegrationError::UnmatchedHook {
            path: path.display().to_string(),
            spelled,
            script,
        }
        .into(),
        Ok(other) => other.into(),
        Err(err) => err,
    }
}

/// Removes the config file at `path`, if there is one. A symlinked config (a dotfiles link)
/// is followed, and what it points at is removed while the link stays where it is, the
/// mirror of how [`write_config_file`] writes through it: unlinking the link itself would
/// leave the file behind it in place, still holding whatever taw put there.
fn remove_config_file(path: &Path) -> Result<()> {
    let target = resolve_config_target(path)?;
    match fs::remove_file(&target) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", target.display())),
    }
}

/// Write `value` pretty-printed (2-space indent, trailing newline) to `path`, or delete
/// `path` if `value` is an empty object (nothing left worth keeping a file for).
fn write_json_value_or_remove(path: &Path, value: &Value) -> Result<()> {
    let is_empty = matches!(value, Value::Object(m) if m.is_empty());
    if is_empty {
        remove_config_file(path)?;
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        write_config_file(path, &pretty_json_bytes(value))?;
    }
    Ok(())
}

/// `config_path`'s permissions, `None` when it does not exist. Read through a symlink, as a write
/// goes through one.
fn read_permissions(config_path: &Path) -> Option<fs::Permissions> {
    fs::metadata(config_path)
        .ok()
        .map(|metadata| metadata.permissions())
}

/// `config_path`'s current bytes, empty when it does not exist.
fn read_original(config_path: &Path) -> Result<Vec<u8>> {
    if config_path.exists() {
        fs::read(config_path).with_context(|| format!("reading {}", config_path.display()))
    } else {
        Ok(Vec::new())
    }
}

/// Everything about one config file that can stop an install, settled before the install
/// writes a byte. herdr checks its config targets before it writes any asset for the same
/// reason (`check_config_targets`); here that check and the read, parse and edit all
/// happen in one step, so an install that has to refuse a config refuses it while nothing
/// yet exists to be half installed: no hook script, no pristine copy, no created
/// directory, no edit to a second config file.
///
/// Built by [`plan_json_edit`], [`plan_text_edit_checked`] and the wrappers over them; an install
/// builds all of its plans first, writes its script, and then [`ConfigEdit::apply`]s them.
#[derive(Debug)]
pub(crate) struct ConfigEdit {
    config_path: PathBuf,
    sidecar_path: PathBuf,
    /// The config's bytes as they were found (empty when it did not exist): what the
    /// pristine copy will hold.
    original: Vec<u8>,
    /// The config's permissions as they were found, which the pristine copy is created with: a
    /// config only its owner may read can hold tokens, and so can a copy of it.
    permissions: Option<fs::Permissions>,
    edited: Edited,
}

#[derive(Debug)]
enum Edited {
    Json(Value),
    Text(String),
}

impl ConfigEdit {
    /// Writes the pristine copy of the config, only if no copy is there yet (a reinstall
    /// must leave the *original* original alone), and then the edited config. Every write
    /// is [`write_config_file`]'s staged rename.
    pub(crate) fn apply(self) -> Result<()> {
        if !self.sidecar_path.exists() {
            if let Some(parent) = self.sidecar_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            write_config_file_as(
                &self.sidecar_path,
                &self.original,
                self.permissions.as_ref(),
            )?;
        }
        match self.edited {
            Edited::Json(value) => write_json_value_or_remove(&self.config_path, &value),
            Edited::Text(text) => {
                if let Some(parent) = self.config_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                write_config_file(&self.config_path, text.as_bytes())
            }
        }
    }
}

/// Checks that `path`, a config an install is about to edit, can be replaced at all:
/// not a second name for a file with other hard links ([`reject_hard_link`]), not a
/// directory or a symlink loop ([`resolve_config_target`]), and, when it exists, not one
/// this user cannot write ([`check_writable`]).
///
/// Follows herdr's `check_config_target`, which runs it for every config an install
/// touches before any asset is written. herdr's own write-permission check waits for the
/// write on Unix; this asks it up front as well, so the refusal comes before the hook
/// script is published, not after.
pub(crate) fn check_config_target(path: &Path) -> Result<()> {
    reject_hard_link(path)?;
    let target = resolve_config_target(path)?;
    if target.exists() {
        check_writable(path, &target)?;
    }
    Ok(())
}

/// `bytes` as JSON, treating empty input as `{}`. `path` names the file in errors.
fn parse_json_or_empty(bytes: &[u8], path: &Path) -> Result<Value> {
    if bytes.is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_slice(bytes).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Plan applying `edit` to the JSON file at `config_path`, to be written, with the
/// pre-install bytes saved at `sidecar_path` for [`uninstall_json_edit`] to restore
/// byte-for-byte later, by [`ConfigEdit::apply`]. Nothing is written here.
///
/// A config that cannot be replaced ([`check_config_target`]), that is not JSON, or whose
/// shape `edit` cannot work with (reported as [`IntegrationError::UnusableHooksFile`]) is
/// an error, and the file is left exactly as it was found: it belongs to the user and to
/// the agent, not to `taw`.
pub(crate) fn plan_json_edit(
    config_path: &Path,
    sidecar_path: &Path,
    edit: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<ConfigEdit> {
    check_config_target(config_path)?;
    let original = read_original(config_path)?;
    let mut root = parse_json_or_empty(&original, config_path)?;
    edit(&mut root).map_err(|detail| IntegrationError::UnusableHooksFile {
        path: config_path.display().to_string(),
        detail: detail.to_string(),
    })?;
    Ok(ConfigEdit {
        config_path: config_path.to_path_buf(),
        sidecar_path: sidecar_path.to_path_buf(),
        permissions: read_permissions(config_path),
        original,
        edited: Edited::Json(root),
    })
}

/// Reverse of [`plan_json_edit`]. If `config_path` with `strip` applied still matches
/// the pristine copy at `sidecar_path` (also `strip`ped, then run through `normalize`
/// on both sides), restores those bytes verbatim — a byte-for-byte revert that keeps the
/// user's original formatting. Otherwise something else edited the file since install, so
/// this only applies `strip` and re-serializes, preserving the other edits. A no-op if
/// `config_path` doesn't exist and `strip` has nothing to remove.
///
/// The pristine copy goes only when the file was restored from it or when `strip` took taw's
/// entries out of the file. A file that holds nothing of taw's and is not what the copy says it
/// was (somebody took the entry out by hand, and edited the file since) is left as it is, and so
/// is the copy: the copy is all that can give the file back, and it is not this call's to give up.
/// A copy that is not JSON is no different: it cannot be restored from, and goes only with taw's
/// entries.
///
/// `strip` says whether it removed anything, or refuses a file it cannot be sure of
/// ([`IntegrationError::UnmatchedHook`]), in which case nothing is changed: not the file, and not
/// the copy. The caller deletes the hook script only after this returns.
///
/// `normalize` exists for a value install *adds* outside the hook entries themselves
/// (Cursor's mandatory `"version": 1`), which has to be ignored when deciding whether the file is
/// otherwise untouched, but must not be dropped from a file this is only stripping.
pub(crate) fn uninstall_json_edit(
    config_path: &Path,
    sidecar_path: &Path,
    strip: impl Fn(&mut Value) -> Result<bool>,
    normalize: impl Fn(&mut Value),
) -> Result<()> {
    let strip_here = |root: &mut Value| strip(root).map_err(|err| located_in(err, config_path));
    // Never installed by us (or already reverted): still a best-effort strip in case a
    // stray entry exists, but there's no pristine copy to restore verbatim. Says whether it took
    // taw's entries out of the file.
    let strip_only = || -> Result<bool> {
        if config_path.exists() {
            let bytes = fs::read(config_path)
                .with_context(|| format!("reading {}", config_path.display()))?;
            if let Ok(mut root) = serde_json::from_slice::<Value>(&bytes)
                && strip_here(&mut root)?
            {
                write_json_value_or_remove(config_path, &root)?;
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !sidecar_path.exists() {
        return strip_only().map(|_| ());
    }

    let sidecar =
        fs::read(sidecar_path).with_context(|| format!("reading {}", sidecar_path.display()))?;
    let Ok(sidecar_value) = parse_json_or_empty(&sidecar, sidecar_path) else {
        // A saved copy that is not JSON cannot say what the file looked like, so nothing
        // can be restored from it. An earlier install can have left one after stopping on a
        // config it could not parse, and an uninstall that failed on it would leave taw's
        // entry in the config with no way to remove it short of finding the hidden
        // file. Strip the config as it stands. The copy goes with taw's entries, so that it
        // cannot block again, and stays when there was nothing of taw's to take out: it is
        // the user's file as it was found, and not this call's to give up when it is unsure.
        log::warn!(
            "ignoring {}: the saved copy of the config is not valid JSON",
            sidecar_path.display()
        );
        if strip_only()? {
            let _ = fs::remove_file(sidecar_path);
        }
        return Ok(());
    };

    let current_bytes = if config_path.exists() {
        fs::read(config_path).with_context(|| format!("reading {}", config_path.display()))?
    } else {
        Vec::new()
    };
    let mut current_value = parse_json_or_empty(&current_bytes, config_path)?;
    let removed = strip_here(&mut current_value)?;

    // Normalise the sidecar the same way before comparing. Stripping our entry out of
    // `current` can leave — and then prune — an empty container
    // (`{"hooks":{"SessionStart":[]}}` becomes `{}`). If the user's *original* file already
    // had that empty container, `sidecar_value` still carries it, so a raw compare would
    // wrongly conclude "something else changed" and fall through to a canonical rewrite that
    // deletes the user's own `hooks` key. Pruning both sides identically keeps the
    // byte-for-byte restore whenever the only real difference is our own entry.
    //
    // A copy that itself held an entry of ours is not the original it stands for: taw's
    // entry was already in the file when install first looked at it (a settings file
    // carried over from another machine, or a hook added to taw's group by hand). Restoring
    // those bytes would put the entry back beside a script uninstall is about to delete, so
    // such a copy is never restored verbatim; the strip below keeps what else is there. The
    // same goes for a copy with a hook in it that cannot be told from ours.
    let mut sidecar_normalized = sidecar_value;
    let sidecar_held_ours = strip(&mut sidecar_normalized).unwrap_or(true);
    let mut current_normalized = current_value.clone();
    normalize(&mut current_normalized);
    normalize(&mut sidecar_normalized);

    if !sidecar_held_ours && current_normalized == sidecar_normalized {
        // Nothing besides our own entry changed since install: restore the exact original
        // bytes (preserves the user's original formatting byte-for-byte).
        if sidecar.is_empty() {
            remove_config_file(config_path)?;
        } else {
            write_config_file(config_path, &sidecar)?;
        }
        let _ = fs::remove_file(sidecar_path);
    } else if removed {
        // Something else changed the file since install: only remove our entries, keep
        // every other edit, and re-serialize with our canonical formatting.
        write_json_value_or_remove(config_path, &current_value)?;
        let _ = fs::remove_file(sidecar_path);
    }
    // Otherwise there was nothing of taw's in the file to remove, and it is not what the copy
    // says it was: the file and the copy stay as they are.
    Ok(())
}

/// [`plan_json_edit`] specialised to one hook entry at `<container>[event]`.
pub(crate) fn plan_json_hook(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    event: &str,
    needle: &str,
    entry: Value,
) -> Result<ConfigEdit> {
    plan_json_edit(config_path, sidecar_path, |root| {
        upsert_hook_entry(root, container, event, needle, entry)
    })
}

/// [`plan_json_hook`] for an agent whose hook runner is a shell (Claude's), whose `entry` names
/// the script as `word`. A hook of the config that names the script as `~/...`, `$HOME/...` or
/// `${HOME}/...` is taw's, and the install writes `entry` in its place with the path word as it
/// was spelled there.
pub(crate) fn plan_json_hook_in_shell(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    event: &str,
    needle: &str,
    entry: Value,
    word: &str,
) -> Result<ConfigEdit> {
    let needle = Needle::for_shell(needle);
    plan_json_edit(config_path, sidecar_path, |root| {
        upsert_hook_entry_of(root, container, event, &needle, entry, Some(word))
    })
}

/// [`plan_json_hook`] for several `(event, entry)` pairs at once, all sharing one
/// `needle`, for the agents whose install registers more than one event.
pub(crate) fn plan_json_hooks(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    needle: &str,
    entries: Vec<(&str, Value)>,
) -> Result<ConfigEdit> {
    plan_json_edit(config_path, sidecar_path, |root| {
        for (event, entry) in entries {
            upsert_hook_entry(root, container, event, needle, entry)?;
        }
        Ok(())
    })
}

/// Reverse of [`plan_json_hook`].
pub(crate) fn uninstall_json_hook(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    event: &str,
    needle: &str,
) -> Result<()> {
    uninstall_json_hooks(config_path, sidecar_path, container, needle, &[event])
}

/// Reverse of [`plan_json_hook_in_shell`].
pub(crate) fn uninstall_json_hook_in_shell(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    event: &str,
    needle: &str,
) -> Result<()> {
    uninstall_json_hooks_of(
        config_path,
        sidecar_path,
        container,
        Needle::for_shell(needle),
        &[event],
    )
}

/// Reverse of [`plan_json_hooks`]. `events` may name more events than install
/// registered — herdr's own uninstall lists the lifecycle events earlier versions used, so
/// upgrading and then uninstalling leaves nothing behind.
pub(crate) fn uninstall_json_hooks(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    needle: &str,
    events: &[&str],
) -> Result<()> {
    uninstall_json_hooks_of(
        config_path,
        sidecar_path,
        container,
        Needle::new(needle),
        events,
    )
}

/// [`uninstall_json_hooks`] for a [`Needle`] that is already made.
fn uninstall_json_hooks_of(
    config_path: &Path,
    sidecar_path: &Path,
    container: HookContainer,
    needle: Needle,
    events: &[&str],
) -> Result<()> {
    uninstall_json_edit(
        config_path,
        sidecar_path,
        |root| {
            let mut changed = false;
            for event in events {
                changed |= strip_hook_entries_of(root, container, event, &needle)?;
            }
            Ok(changed)
        },
        |_| {},
    )
}

/// Plan applying `edit` to the text file at `config_path`, to be written, with the
/// pre-install text saved at `sidecar_path` for [`uninstall_text_edit`] to restore
/// byte-for-byte later, by [`ConfigEdit::apply`]. Nothing is written here. A config that
/// cannot be replaced ([`check_config_target`]) or is not UTF-8 text is an error, and so is
/// what `edit` returns as an error: that is what the install stops with, before anything has
/// been written.
pub(crate) fn plan_text_edit_checked(
    config_path: &Path,
    sidecar_path: &Path,
    edit: impl FnOnce(&str) -> Result<String>,
) -> Result<ConfigEdit> {
    check_config_target(config_path)?;
    let original = read_original(config_path)?;
    let text = std::str::from_utf8(&original)
        .with_context(|| format!("{} is not UTF-8 text", config_path.display()))?;
    let edited = edit(text)?;
    Ok(ConfigEdit {
        config_path: config_path.to_path_buf(),
        sidecar_path: sidecar_path.to_path_buf(),
        original,
        permissions: read_permissions(config_path),
        edited: Edited::Text(edited),
    })
}

/// Reverse of [`plan_text_edit_checked`]. Byte-identical revert when `strip` brings the file
/// back to the pristine copy at `sidecar_path` — compared after running `normalize` over
/// both sides; otherwise writes `strip`'s result, preserving whatever else changed since
/// install. A pristine copy that itself holds what `strip` removes (taw's entry was in the
/// file before its first install) is not restored either: `strip`'s result is written.
/// A no-op if `config_path` doesn't exist and `strip` changes nothing.
///
/// The pristine copy goes only when the file was restored from it or when `strip` took something
/// out of the file. A file that holds nothing of taw's and is not what the copy says it was is
/// left as it is, and so is the copy (see [`uninstall_json_edit`]).
///
/// `normalize` is for a difference that is not a change the user made: an empty
/// `plugins:` scaffold Hermes' line-based edit cannot un-add. Applied to the comparison
/// only, never to what is written, so an untouched file reverts to its own exact bytes
/// and a changed one keeps its own.
pub(crate) fn uninstall_text_edit(
    config_path: &Path,
    sidecar_path: &Path,
    strip: impl Fn(&str) -> String,
    normalize: impl Fn(&str) -> String,
) -> Result<()> {
    let read = |path: &Path| -> Result<String> {
        if path.exists() {
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
        } else {
            Ok(String::new())
        }
    };

    if !sidecar_path.exists() {
        let current = read(config_path)?;
        let stripped = strip(&current);
        if stripped != current {
            if stripped.is_empty() {
                remove_config_file(config_path)?;
            } else {
                write_config_file(config_path, stripped.as_bytes())?;
            }
        }
        return Ok(());
    }

    let sidecar = read(sidecar_path)?;
    let current = read(config_path)?;
    let stripped_current = strip(&current);
    let removed = stripped_current != current;

    // A copy that itself held taw's entry is not the original it stands for: the entry was
    // already in the file when install first looked at it (a config carried over from
    // another machine, or left by an earlier taw). Restoring those bytes would put the
    // entry back beside a script uninstall is about to delete, so such a copy is never
    // restored verbatim; the write below keeps everything else. `strip` hands its input
    // back unchanged when there is nothing of taw's in it, which is what makes the
    // comparison a test for "held".
    let held = strip(&sidecar) != sidecar;

    if !held && normalize(&stripped_current) == normalize(&strip(&sidecar)) {
        if sidecar.is_empty() {
            remove_config_file(config_path)?;
        } else {
            write_config_file(config_path, sidecar.as_bytes())?;
        }
        let _ = fs::remove_file(sidecar_path);
    } else if removed {
        if stripped_current.is_empty() {
            remove_config_file(config_path)?;
        } else {
            write_config_file(config_path, stripped_current.as_bytes())?;
        }
        let _ = fs::remove_file(sidecar_path);
    }
    // Otherwise there was nothing of taw's in the file to remove, and it is not what the copy
    // says it was: the file and the copy stay as they are.
    Ok(())
}

/// `value` as a TOML basic string: double-quoted, with the escapes TOML defines. Follows
/// herdr's `config_edit::toml_basic_string`, because a hook path (or a matcher regex)
/// containing a quote or a backslash would otherwise produce a config file that does not
/// parse.
pub(crate) fn toml_basic_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{08}' => result.push_str("\\b"),
            '\t' => result.push_str("\\t"),
            '\n' => result.push_str("\\n"),
            '\u{0c}' => result.push_str("\\f"),
            '\r' => result.push_str("\\r"),
            ch if ch <= '\u{1f}' || ch == '\u{7f}' => {
                result.push_str(&format!("\\u{:04X}", ch as u32));
            }
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

const TOML_BLOCK_START: &str = "# >>> taw integration >>>";
const TOML_BLOCK_END: &str = "# <<< taw integration <<<";

fn render_toml_block(body: &str) -> String {
    format!("{TOML_BLOCK_START}\n{body}{TOML_BLOCK_END}\n")
}

/// Append [`render_toml_block`] to `original`, separated from any existing content by
/// exactly one blank line (and nothing at all if `original` is empty). Exact inverse of
/// [`strip_toml_block`], independent of whether `original` itself ends with a newline.
pub(crate) fn append_toml_block(original: &str, body: &str) -> String {
    let mut out = String::with_capacity(original.len() + body.len() + 96);
    out.push_str(original);
    if !original.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&render_toml_block(body));
    out
}

/// Remove the `TOML_BLOCK_START..TOML_BLOCK_END` region (and the blank-line separator
/// [`append_toml_block`] adds before it) from `content`, every such region there is: a file
/// two installs or two machines wrote to holds more than one. Returns `content` unchanged if no
/// block is present.
pub(crate) fn strip_toml_block(content: &str) -> String {
    let mut text = content.to_string();
    while let Some(next) = strip_first_toml_block(&text) {
        text = next;
    }
    text
}

/// [`strip_toml_block`] for the first block; `None` when there is none.
fn strip_first_toml_block(content: &str) -> Option<String> {
    Some(remove_toml_block_at(content, toml_block_range(content)?))
}

/// The byte range of the first delimited taw block in `text`, markers included.
fn toml_block_range(text: &str) -> Option<std::ops::Range<usize>> {
    let start = text.find(TOML_BLOCK_START)?;
    let end = start + text[start..].find(TOML_BLOCK_END)? + TOML_BLOCK_END.len();
    Some(start..end)
}

/// The byte ranges of every delimited taw block in `text`, markers included, in order. An
/// unpaired start marker ends the list.
pub(crate) fn toml_block_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(range) = toml_block_range(&text[from..]) {
        ranges.push(from + range.start..from + range.end);
        from += range.end;
    }
    ranges
}

/// Whether `text` holds a delimited taw block (an older taw's way of editing a TOML config).
pub(crate) fn has_toml_block(text: &str) -> bool {
    toml_block_range(text).is_some()
}

/// `text` without the block at `range`: [`strip_toml_block`] for one block, and the one rule both
/// follow. A block at the end of the file comes out with the blank-line separator
/// [`append_toml_block`] put before it, the exact inverse. When text follows the end marker the
/// separator would join the line before the block to the line after it (one blank line stood
/// there when the file had no final newline), so one line break is left between them.
pub(crate) fn remove_toml_block_at(text: &str, range: std::ops::Range<usize>) -> String {
    let after = &text[range.end..];
    let before = &text[..range.start];
    let separated = before.strip_suffix("\n\n").unwrap_or(before);
    let after = after.strip_prefix('\n').unwrap_or(after);
    let join = if after.trim().is_empty() || separated.is_empty() || separated.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{separated}{join}{after}")
}

/// An edit a plan refuses, and the file as it should be left instead when taw's own older text
/// has to come out of it either way: that text is what stops the agent from loading the file,
/// so it goes also when the install then cannot go on.
pub(crate) struct Refused {
    pub(crate) error: anyhow::Error,
    pub(crate) repaired: Option<String>,
}

impl From<anyhow::Error> for Refused {
    fn from(error: anyhow::Error) -> Self {
        Refused {
            error,
            repaired: None,
        }
    }
}

/// [`plan_text_edit_checked`] for an edit that can refuse with a repair: when `edit` returns a
/// [`Refused`] that holds a repaired text, that text is written (nothing else is: no pristine
/// copy, no hook script) and the install still fails with the error.
pub(crate) fn plan_text_edit_repairing(
    config_path: &Path,
    sidecar_path: &Path,
    edit: impl FnOnce(&str) -> std::result::Result<String, Refused>,
) -> Result<ConfigEdit> {
    let mut repaired = None;
    let planned = plan_text_edit_checked(config_path, sidecar_path, |text| {
        edit(text).map_err(|refused| {
            // A repair keeps the file's final newline: cutting a block off the end of a file
            // does not take it along.
            repaired = refused.repaired.map(|mut fixed| {
                if text.ends_with('\n') && !fixed.is_empty() && !fixed.ends_with('\n') {
                    fixed.push_str(if text.ends_with("\r\n") { "\r\n" } else { "\n" });
                }
                fixed
            });
            refused.error
        })
    });
    match (planned, repaired) {
        (Err(error), Some(text)) => {
            write_config_file(config_path, text.as_bytes())
                .with_context(|| format!("repairing {}", config_path.display()))?;
            Err(error)
        }
        (planned, _) => planned,
    }
}

/// `text` with taw's delimited blocks blanked out and every line kept, so a line number found in
/// the result is the line of the file itself.
pub(crate) fn blanked(text: &str) -> String {
    let mut out = text.to_string();
    while let Some(range) = toml_block_range(&out) {
        let newlines = "\n".repeat(out[range.clone()].matches('\n').count());
        out.replace_range(range, &newlines);
    }
    out
}

/// What is wrong with `text` as TOML, without quoting it (a config can hold a token): the
/// parser's message and the line it is on, counted in the file with taw's blocks left out of the
/// reading.
pub(crate) fn toml_problem(text: &str) -> (String, usize) {
    let blank = blanked(text);
    match blank.parse::<toml::Table>() {
        Err(err) => {
            let offset = err.span().map_or(0, |span| span.start).min(blank.len());
            let line = blank.as_bytes()[..offset]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1;
            (err.message().to_string(), line)
        }
        Ok(_) => ("invalid TOML".to_string(), 1),
    }
}

/// Plan installing (or reinstalling) the delimited taw block at the end of the TOML file
/// at `config_path`, keeping a pristine copy of the pre-install text at `sidecar_path`
/// (only on the *first* install) for [`uninstall_toml_block`] to revert byte-for-byte
/// later. See [`plan_text_edit_checked`].
///
/// `body` is the TOML the block wraps, newline-terminated: a run of `[[hooks]]` tables for Kimi
/// Code.
///
/// Neither the file as it is (with taw's older block taken out) nor the file with the block
/// added may fail to parse as TOML: the agent would refuse to start on it. The error says why.
/// The former leaves the file as it is. The latter happens when the file already holds the table
/// the block adds to in another shape (`hooks = []`, or a `[hooks]` table, beside `[[hooks]]`);
/// there, an older taw's block in the file is what stops the agent from loading it, so it is
/// taken out and the rest left as it was before the install stops (see [`Refused`]).
pub(crate) fn plan_toml_block(
    config_path: &Path,
    sidecar_path: &Path,
    body: &str,
) -> Result<ConfigEdit> {
    let unusable = |detail: String| -> anyhow::Error {
        IntegrationError::UnusableConfig {
            path: config_path.display().to_string(),
            detail,
        }
        .into()
    };
    plan_text_edit_repairing(config_path, sidecar_path, |original| {
        // dedupe: drop a pre-existing block before appending the current one
        let base = strip_toml_block(original);
        if base.parse::<toml::Table>().is_err() {
            let (message, line) = toml_problem(original);
            return Err(unusable(format!(
                "it does not parse as TOML ({message}, line {line}), so the agent cannot load it \
                 either; taw left it as it is. Fix the file, then install again"
            ))
            .into());
        }
        let edited = append_toml_block(&base, body);
        if let Err(err) = edited.parse::<toml::Table>() {
            // An older taw's block is what the agent cannot load the file with: it comes out
            // also now, and the file is otherwise as it was.
            let removed = base != original;
            let lead = if removed {
                "taw took its older block out of it, since that block stopped the agent from \
                 loading the file, and changed nothing else. taw does not install yet: "
            } else {
                ""
            };
            // What the lead says taw did, the tail does not say again.
            let left = if removed { "" } else { "; it is left as it is" };
            return Err(Refused {
                error: unusable(format!(
                    "{lead}taw's entries cannot be added to it without leaving a file the agent \
                     cannot load ({}){left}. Make what clashes with them (a hooks \
                     entry that is not a list of [[hooks]] tables) a list of tables, or add the \
                     entries by hand, then install again",
                    err.message()
                )),
                repaired: removed.then_some(base),
            });
        }
        Ok(edited)
    })
}

/// Reverse of [`plan_toml_block`]. Byte-identical revert when nothing besides our block
/// changed since install; otherwise strips just our block, preserving other edits. A no-op
/// if `config_path` doesn't exist and there is nothing to strip.
pub(crate) fn uninstall_toml_block(config_path: &Path, sidecar_path: &Path) -> Result<()> {
    uninstall_text_edit(config_path, sidecar_path, strip_toml_block, str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_never_answers_with_a_relative_path() {
        // A fallback of `PathBuf::from(".")` would be joined by every caller onto an agent's
        // config directory, so a missing home directory would mean installing hook
        // scripts into `./.claude/hooks/`: whatever directory `taw` happened to be run
        // from, most likely the user's own project, reported as a success. There is no
        // such answer, only an absolute path or an error saying why not.
        match home_dir() {
            Ok(home) => assert!(
                home.is_absolute(),
                "a home directory must be absolute, got {home:?}"
            ),
            Err(err) => {
                let text = format!("{err:#}");
                assert!(
                    text.contains("home directory"),
                    "the error must say what could not be determined: {text}"
                );
            }
        }
    }

    // -------------------------------------------------------------------------------
    // upsert_hook_entry / strip_hook_entries: a group can hold hooks that are not ours
    // -------------------------------------------------------------------------------

    const SCRIPT: &str = "/home/u/.claude/hooks/taw-agent-state.sh";

    fn our_hook() -> Value {
        serde_json::json!({
            "type": "command",
            "command": format!("bash '{SCRIPT}' session"),
            "timeout": 10
        })
    }

    fn their_hook(command: &str) -> Value {
        serde_json::json!({"type": "command", "command": command, "timeout": 3})
    }

    fn group(matcher: &str, hooks: Vec<Value>) -> Value {
        serde_json::json!({"matcher": matcher, "hooks": hooks})
    }

    fn settings_with(entries: Vec<Value>) -> Value {
        serde_json::json!({"hooks": {"SessionStart": entries}})
    }

    fn session_start(root: &Value) -> &Vec<Value> {
        root["hooks"]["SessionStart"].as_array().unwrap()
    }

    #[test]
    fn upsert_takes_only_taws_hook_out_of_a_shared_group() {
        let mut root = settings_with(vec![group("*", vec![our_hook(), their_hook("echo keep")])]);
        let fresh = group("^(startup)$", vec![our_hook()]);

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", SCRIPT, fresh.clone()).unwrap();

        assert_eq!(
            session_start(&root),
            &vec![group("*", vec![their_hook("echo keep")]), fresh]
        );
    }

    #[test]
    fn upsert_replaces_a_group_that_held_only_taws_hook() {
        let mut root = settings_with(vec![
            group("*", vec![our_hook()]),
            group("Bash", vec![their_hook("echo pre")]),
        ]);
        let fresh = group("^(startup)$", vec![our_hook()]);

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", SCRIPT, fresh.clone()).unwrap();

        assert_eq!(
            session_start(&root),
            &vec![group("Bash", vec![their_hook("echo pre")]), fresh]
        );
    }

    #[test]
    fn upsert_leaves_a_group_that_was_empty_before_taw_came() {
        let mut root = settings_with(vec![group("keep", vec![])]);
        let fresh = group("^(startup)$", vec![our_hook()]);

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", SCRIPT, fresh.clone()).unwrap();

        assert_eq!(session_start(&root), &vec![group("keep", vec![]), fresh]);
    }

    #[test]
    fn upsert_still_replaces_a_bare_hook_entry_whole() {
        // Copilot, Cursor and MastraCode put the hook itself in the event array, with no
        // group around it.
        let ours = serde_json::json!({"bash": format!("bash '{SCRIPT}'"), "type": "command"});
        let theirs = serde_json::json!({"bash": "echo keep", "type": "command"});
        let mut root = settings_with(vec![theirs.clone(), ours]);
        let fresh = serde_json::json!({"bash": format!("bash '{SCRIPT}'"), "timeoutSec": 10});

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", SCRIPT, fresh.clone()).unwrap();

        assert_eq!(session_start(&root), &vec![theirs, fresh]);
    }

    #[test]
    fn strip_keeps_a_users_hook_that_shares_taws_group() {
        let mut root = settings_with(vec![group("*", vec![our_hook(), their_hook("echo keep")])]);

        assert!(strip_hook_entries(&mut root, HOOKS_KEY, "SessionStart", SCRIPT).unwrap());

        assert_eq!(
            root,
            settings_with(vec![group("*", vec![their_hook("echo keep")])])
        );
    }

    #[test]
    fn strip_drops_a_group_it_empties_then_the_event_and_the_container() {
        let mut root = settings_with(vec![group("*", vec![our_hook()])]);

        assert!(strip_hook_entries(&mut root, HOOKS_KEY, "SessionStart", SCRIPT).unwrap());

        assert_eq!(root, serde_json::json!({}));
    }

    #[test]
    fn strip_leaves_a_group_that_was_empty_before_taw_came() {
        let mut root = settings_with(vec![group("keep", vec![]), group("*", vec![our_hook()])]);

        assert!(strip_hook_entries(&mut root, HOOKS_KEY, "SessionStart", SCRIPT).unwrap());

        assert_eq!(root, settings_with(vec![group("keep", vec![])]));
    }

    #[test]
    fn strip_reports_no_change_when_nothing_is_taws() {
        let mut root = settings_with(vec![group("*", vec![their_hook("echo keep")])]);
        let before = root.clone();

        assert!(!strip_hook_entries(&mut root, HOOKS_KEY, "SessionStart", SCRIPT).unwrap());

        assert_eq!(root, before);
    }

    // -------------------------------------------------------------------------------
    // Needle: which hooks are taw's, whichever way their folder is spelled
    // -------------------------------------------------------------------------------

    /// A folder with two names, as `~/.claude-work` and `~/.claude-alt` are the same directory
    /// through links: `(guard, real, link)`, with `real` canonical and `link` a symlink to it.
    #[cfg(unix)]
    fn one_folder_two_names() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let real = base.join("real");
        fs::create_dir_all(real.join("hooks")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (tmp, real, link)
    }

    fn script_in(folder: &Path) -> String {
        folder
            .join("hooks")
            .join("taw-agent-state.sh")
            .display()
            .to_string()
    }

    #[test]
    fn a_hook_that_names_the_script_by_the_path_it_was_installed_with_is_taws() {
        let needle = Needle::new(SCRIPT);
        assert_eq!(
            needle.judge_text(&format!("bash '{SCRIPT}' session")),
            Owner::Ours
        );
        assert_eq!(needle.judge(&our_hook()), Owner::Ours);
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_that_names_the_script_through_another_name_for_its_folder_is_taws() {
        let (_tmp, real, link) = one_folder_two_names();
        for (installed_as, named_as) in [(&real, &link), (&link, &real)] {
            let needle = Needle::new(&script_in(installed_as));
            let there = script_in(named_as);
            for text in [
                format!("bash '{there}' session"),
                format!("bash \"{there}\" session"),
                format!("bash {there} session"),
                format!("cd / && bash '{there}'"),
                format!("echo 'x' && bash {there}"),
            ] {
                assert_eq!(needle.judge_text(&text), Owner::Ours, "{text}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_with_a_space_in_its_name_is_resolved_too() {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let real = base.join("my config");
        fs::create_dir_all(real.join("hooks")).unwrap();
        let link = base.join("link to it");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let needle = Needle::new(&script_in(&real));

        let text = format!("bash '{}' session", script_in(&link));

        assert_eq!(needle.judge_text(&text), Owner::Ours, "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn the_script_need_not_be_there_for_its_folder_to_be_known() {
        // The hook script was deleted, and the entry that ran it is what is left to take out.
        let (_tmp, real, link) = one_folder_two_names();
        let needle = Needle::new(&script_in(&real));
        assert!(!Path::new(&script_in(&real)).exists());

        let text = format!("bash '{}' session", script_in(&link));

        assert_eq!(needle.judge_text(&text), Owner::Ours);
    }

    #[cfg(unix)]
    #[test]
    fn a_script_of_the_same_name_in_another_folder_that_exists_is_not_taws() {
        let (_tmp, real, _link) = one_folder_two_names();
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(other.path().join("hooks")).unwrap();
        let needle = Needle::new(&script_in(&real));

        let text = format!("bash '{}' session", script_in(other.path()));

        assert_eq!(needle.judge_text(&text), Owner::Foreign, "{text}");
    }

    #[test]
    fn a_path_that_does_not_resolve_cannot_be_told_from_taws() {
        // It may be taw's, written through a link that is not there any more, or somebody else's:
        // nothing here can say, and it says so.
        let needle = Needle::new(SCRIPT);
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        assert_eq!(
            needle.judge_text(&format!("bash '{stranger}' session")),
            Owner::Unsure(stranger.to_string())
        );
    }

    #[test]
    fn a_path_that_needs_a_shell_to_resolve_cannot_be_told_from_taws() {
        // Neither of these starts from the home folder, and the rest is not an absolute path.
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        for text in [
            "bash ./taw-agent-state.sh",
            "bash taw-agent-state.sh",
            "cd hooks && bash taw-agent-state.sh",
            "bash ~other/.claude/hooks/taw-agent-state.sh session",
            "bash $XDG_CONFIG_HOME/.claude/hooks/taw-agent-state.sh session",
            "bash ${CLAUDE_CONFIG_DIR}/hooks/taw-agent-state.sh session",
            "bash $HOME2/.claude/hooks/taw-agent-state.sh session",
        ] {
            let verdict = needle.judge_text(text);
            // The last one resolves, to somewhere that is not a folder.
            assert!(matches!(verdict, Owner::Unsure(_)), "{text}: {verdict:?}");
        }
    }

    /// A scratch home folder with taw's Claude hook folder in it and another folder of hooks
    /// beside it: `(guard, home)`, with `home` canonical.
    fn scratch_home() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("home");
        fs::create_dir_all(home.join(".claude").join("hooks")).unwrap();
        fs::create_dir_all(home.join(".other").join("hooks")).unwrap();
        (tmp, home)
    }

    fn home_script(home: &Path) -> String {
        home.join(".claude")
            .join("hooks")
            .join("taw-agent-state.sh")
            .display()
            .to_string()
    }

    #[test]
    fn a_path_from_the_home_folder_is_judged_as_the_absolute_path_it_stands_for() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let spellings = ["~/", "$HOME/", "${HOME}/"];
        for spelling in spellings {
            // The words a shell expands: the home folder starts the word, unquoted, or (for the
            // variable) inside double quotes.
            let mut texts = vec![
                format!("bash {spelling}.claude/hooks/taw-agent-state.sh session"),
                format!("{spelling}.claude/hooks/taw-agent-state.sh"),
                format!("FOO=1 bash {spelling}.claude/hooks/taw-agent-state.sh"),
            ];
            if spelling != "~/" {
                texts.push(format!(
                    "bash \"{spelling}.claude/hooks/taw-agent-state.sh\" session"
                ));
                texts.push(format!(
                    "bash \"{spelling}.claude/hooks/taw-agent-state.sh\""
                ));
            }
            for text in texts {
                assert_eq!(needle.judge_text(&text), Owner::OursFromHome, "{text}");
            }
            // The same name in another folder that is there is another file.
            let other = format!("bash {spelling}.other/hooks/taw-agent-state.sh session");
            assert_eq!(needle.judge_text(&other), Owner::Foreign, "{other}");
            // A folder that is not there cannot be told.
            let gone = format!("bash {spelling}.gone/hooks/taw-agent-state.sh session");
            assert_eq!(
                needle.judge_text(&gone),
                Owner::Unsure(format!("{spelling}.gone/hooks/taw-agent-state.sh")),
                "{gone}"
            );
        }
    }

    #[test]
    fn a_variable_quoted_apart_from_the_slash_after_it_is_still_the_home_folder() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let name = ".claude/hooks/taw-agent-state.sh";
        for text in [
            format!("bash \"$HOME\"/{name} session"),
            format!("bash \"${{HOME}}\"/{name} session"),
            format!("bash $HOME\"/{name}\" session"),
            format!("bash $HOME'/'{name} session"),
            format!("bash ${{HOME}}\"/{name}\" session"),
            format!("bash ${{HOME}}'/'{name} session"),
            format!("bash \"$HOME\"'/'{name} session"),
            format!("bash $HOME\\/{name} session"),
        ] {
            assert_eq!(needle.judge_text(&text), Owner::OursFromHome, "{text}");
        }
        // The tilde is one run: `~'/'` is not expanded by a shell.
        for text in [
            format!("bash ~'/'{name} session"),
            format!("bash ~\"/{name}\" session"),
        ] {
            let verdict = needle.judge_text(&text);
            assert!(matches!(verdict, Owner::Unsure(_)), "{text}: {verdict:?}");
        }
    }

    #[test]
    fn a_comment_is_not_judged() {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        let name = ".claude/hooks/taw-agent-state.sh";
        for text in [
            format!("echo hi # ~/{name}"),
            format!("echo hi # {script}"),
            format!("echo hi #{script}"),
            format!("echo hi\t# $HOME/{name} session"),
            format!("# {script}"),
        ] {
            assert_eq!(needle.judge_text(&text), Owner::Foreign, "{text}");
        }
        // A comment ends at the end of its line, and a `#` inside a word starts nothing.
        for text in [
            format!("echo hi # nothing\nbash ~/{name} session"),
            format!("echo hi # nothing\nbash {script} session"),
            format!("bash a#b ~/{name} session"),
            format!("bash \"#\" ~/{name} session"),
            format!("bash ''#~/{name} ~/{name}"),
        ] {
            let verdict = needle.judge_text(&text);
            assert!(
                matches!(verdict, Owner::Ours | Owner::OursFromHome),
                "{text}: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_hash_right_after_a_shell_operator_opens_a_comment() {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        let name = ".claude/hooks/taw-agent-state.sh";
        for operator in [";", "&&", "|", ")", "&", "(", "||", "<", ">", ">>"] {
            for text in [
                format!("echo hi{operator}# ~/{name} session"),
                format!("echo hi{operator}# {script} session"),
                format!("echo hi{operator}#~/{name}"),
                format!("echo hi {operator}# $HOME/{name}"),
            ] {
                assert_eq!(needle.judge_text(&text), Owner::Foreign, "{text}");
            }
            // The command after the operator is still judged, and a `#` inside a word of it
            // starts nothing.
            for text in [
                format!("echo hi{operator} bash ~/{name} session"),
                format!("echo hi{operator}bash a#b ~/{name} session"),
                format!("echo hi{operator}bash {script} session"),
            ] {
                let verdict = needle.judge_text(&text);
                assert!(
                    matches!(verdict, Owner::Ours | Owner::OursFromHome),
                    "{text}: {verdict:?}"
                );
            }
        }
        // Quoted or escaped, an operator is part of its word and a `#` after it starts nothing.
        for text in [
            format!("echo 'hi;'# ~/{name}"),
            format!("echo hi\\;# ~/{name}"),
            format!("echo \"hi|\"# ~/{name}"),
        ] {
            let verdict = needle.judge_text(&text);
            assert!(
                matches!(verdict, Owner::Ours | Owner::OursFromHome),
                "{text}: {verdict:?}"
            );
        }
    }

    #[test]
    fn only_a_space_a_tab_and_a_newline_split_a_command_into_words() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let name = ".claude/hooks/taw-agent-state.sh";
        // A non-breaking space is part of the word, so the word does not start with `~/`.
        let text = format!("bash\u{a0}~/{name} session");
        let verdict = needle.judge_text(&text);
        assert!(matches!(verdict, Owner::Unsure(_)), "{text:?}: {verdict:?}");
        for blank in [" ", "\t", "\n"] {
            let text = format!("bash{blank}~/{name}{blank}session");
            assert_eq!(needle.judge_text(&text), Owner::OursFromHome, "{text:?}");
        }
    }

    #[test]
    fn a_word_a_shell_would_not_expand_to_the_home_folder_is_unsure() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let name = ".claude/hooks/taw-agent-state.sh";
        for text in [
            // Single quotes keep every character as it is.
            format!("bash '$HOME/{name}' session"),
            format!("bash '${{HOME}}/{name}' session"),
            format!("bash '~/{name}' session"),
            // A tilde does not expand inside double quotes.
            format!("bash \"~/{name}\" session"),
            // A backslash outside quotes keeps the next character as it is.
            format!("bash \\~/{name} session"),
            format!("bash \\$HOME/{name} session"),
            // A backslash inside double quotes escapes `$` too.
            format!("bash \"\\$HOME/{name}\" session"),
            // The path is only in the middle of a longer word.
            format!("echo 'see ~/{name}'"),
            format!("echo \"see $HOME/{name}\""),
            format!("bash 'x ~/{name}'"),
            format!("echo see$HOME/{name}"),
            // Another user's home, another variable, a command substitution.
            format!("bash ~other/{name} session"),
            format!("bash $HOME2/{name} session"),
            format!("bash $(echo $HOME)/{name} session"),
            format!("bash \"$(echo /x)/{name}\" session"),
            format!("bash `echo ~`/{name} session"),
            // Quotes that break the name of the variable.
            format!("bash $HO\"ME\"/{name} session"),
            format!("bash $HO\\ME/{name} session"),
        ] {
            let verdict = needle.judge_text(&text);
            assert!(matches!(verdict, Owner::Unsure(_)), "{text}: {verdict:?}");
        }
    }

    #[test]
    fn an_escape_inside_double_quotes_is_read_as_the_character_it_escapes() {
        let (_tmp, home) = scratch_home();
        let folder = home.join("a$b `c` \"d\" \\e");
        fs::create_dir_all(folder.join("hooks")).unwrap();
        let script = folder.join("hooks").join("taw-agent-state.sh");
        let needle = Needle::in_home(&script.display().to_string(), Some(home.clone()));

        let text = r#"bash "$HOME/a\$b \`c\` \"d\" \\e/hooks/taw-agent-state.sh" session"#;

        assert_eq!(needle.judge_text(text), Owner::OursFromHome, "{text}");
        // Outside quotes the same escapes are read the same way.
        let text = r#"bash $HOME/a\$b\ \`c\`\ \"d\"\ \\e/hooks/taw-agent-state.sh session"#;
        assert_eq!(needle.judge_text(text), Owner::OursFromHome, "{text}");
    }

    #[test]
    fn an_absolute_path_is_read_as_a_shell_word_too() {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        for text in [
            format!("bash '{script}' session"),
            format!("bash \"{script}\" session"),
            format!("bash {script} session"),
        ] {
            assert_eq!(needle.judge_text(&text), Owner::Ours, "{text}");
        }
        // A word that does not start with the slash is not an absolute path.
        let other = home.join(".other/hooks/taw-agent-state.sh");
        let verdict = needle.judge_text(&format!("echo 'see {}'", other.display()));
        assert!(matches!(verdict, Owner::Unsure(_)), "{verdict:?}");
    }

    #[test]
    fn a_path_from_the_home_folder_matches_as_text_when_its_folder_is_not_there_yet() {
        // A dotfiles checkout on a new machine: no hooks folder, so nothing to resolve.
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        for spelling in ["~/", "$HOME/", "${HOME}/"] {
            let text = format!("bash {spelling}.claude/hooks/taw-agent-state.sh session");
            assert_eq!(needle.judge_text(&text), Owner::OursFromHome, "{text}");
            // Not the same folder, and not there to resolve: unsure, as an absolute path is.
            let other = format!("bash {spelling}.other/hooks/taw-agent-state.sh session");
            assert!(
                matches!(needle.judge_text(&other), Owner::Unsure(_)),
                "{other}"
            );
        }
    }

    #[test]
    fn a_path_from_the_home_folder_is_unsure_when_there_is_no_home_folder() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), None);
        for text in [
            "bash ~/.claude/hooks/taw-agent-state.sh session",
            "bash \"$HOME/.claude/hooks/taw-agent-state.sh\" session",
            "bash ${HOME}/.claude/hooks/taw-agent-state.sh",
        ] {
            assert!(
                matches!(needle.judge_text(text), Owner::Unsure(_)),
                "{text}: {:?}",
                needle.judge_text(text)
            );
        }
    }

    #[test]
    fn a_hook_with_a_path_from_the_home_folder_is_taws_through_any_of_its_strings() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let hook = serde_json::json!({
            "also": ["echo hi", {"command": "bash $HOME/.claude/hooks/taw-agent-state.sh session"}]
        });
        assert_eq!(needle.judge(&hook), Owner::OursFromHome);
    }

    fn from_home_hook() -> Value {
        serde_json::json!({
            "type": "command",
            "command": "bash ~/.claude/hooks/taw-agent-state.sh session"
        })
    }

    const HOME_WORD: &str = "\"$HOME/.claude/hooks/taw-agent-state.sh\"";

    /// What an install writes for Claude, in the shape of these tests.
    fn fresh_entry() -> Value {
        group(
            "^(startup)$",
            vec![their_hook(&format!("bash {HOME_WORD} session"))],
        )
    }

    #[test]
    fn a_reinstall_writes_an_entry_spelled_from_the_home_folder_in_the_current_form_where_it_stood()
    {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        let theirs = group("*", vec![their_hook("echo keep")]);
        let after = group("*", vec![their_hook("echo after")]);
        let mine = group("startup", vec![from_home_hook()]);
        let mut root = settings_with(vec![theirs.clone(), mine, after.clone()]);

        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh_entry(),
            Some(HOME_WORD),
        )
        .unwrap();

        // The matcher and the timeout are the install's own; the path word is the one found.
        let refreshed = group(
            "^(startup)$",
            vec![their_hook(
                "bash ~/.claude/hooks/taw-agent-state.sh session",
            )],
        );
        assert_eq!(session_start(&root), &vec![theirs, refreshed, after]);
    }

    #[test]
    fn a_reinstall_leaves_the_others_of_a_shared_group_and_puts_its_entry_after_it() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let mut root = settings_with(vec![
            group("*", vec![from_home_hook(), their_hook("echo also")]),
            group("*", vec![their_hook("echo last")]),
        ]);

        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh_entry(),
            Some(HOME_WORD),
        )
        .unwrap();

        assert_eq!(
            session_start(&root),
            &vec![
                group("*", vec![their_hook("echo also")]),
                group(
                    "^(startup)$",
                    vec![their_hook(
                        "bash ~/.claude/hooks/taw-agent-state.sh session"
                    )]
                ),
                group("*", vec![their_hook("echo last")]),
            ]
        );
    }

    #[test]
    fn a_path_word_that_is_more_than_the_path_is_not_kept_and_the_entry_is_the_installs_own() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let odd = group(
            "*",
            vec![their_hook(
                "bash ~/.claude/hooks/taw-agent-state.sh:echo session",
            )],
        );
        let mut root = settings_with(vec![odd]);

        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh_entry(),
            Some(HOME_WORD),
        )
        .unwrap();

        assert_eq!(session_start(&root), &vec![fresh_entry()]);
    }

    #[test]
    fn a_reinstall_drops_an_absolute_copy_beside_an_entry_spelled_from_the_home_folder() {
        // What an older install left: an entry and a second one beside it.
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        let absolute = group("*", vec![their_hook(&format!("bash '{script}' session"))]);
        let mine = group("startup", vec![from_home_hook()]);
        let mut root = settings_with(vec![mine, absolute]);

        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh_entry(),
            Some(HOME_WORD),
        )
        .unwrap();

        assert_eq!(
            session_start(&root),
            &vec![group(
                "^(startup)$",
                vec![their_hook(
                    "bash ~/.claude/hooks/taw-agent-state.sh session"
                )]
            )]
        );
    }

    #[test]
    fn a_reinstall_over_an_entry_that_names_the_script_in_full_writes_the_entry_as_it_is() {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let needle = Needle::in_home(&script, Some(home.clone()));
        let absolute = group("*", vec![their_hook(&format!("bash '{script}' session"))]);
        let mut root = settings_with(vec![absolute]);

        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh_entry(),
            Some(HOME_WORD),
        )
        .unwrap();

        assert_eq!(session_start(&root), &vec![fresh_entry()]);
    }

    #[test]
    fn an_uninstall_takes_out_an_entry_spelled_from_the_home_folder() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let theirs = group("*", vec![their_hook("echo keep")]);
        let mut root = settings_with(vec![
            theirs.clone(),
            group("startup", vec![from_home_hook(), their_hook("echo also")]),
        ]);

        let removed = strip_hook_entries_of(&mut root, HOOKS_KEY, "SessionStart", &needle).unwrap();

        assert!(removed);
        assert_eq!(
            session_start(&root),
            &vec![theirs, group("startup", vec![their_hook("echo also")])]
        );
    }

    #[test]
    fn a_bare_hook_entry_spelled_from_the_home_folder_is_refreshed_and_taken_out_whole() {
        let (_tmp, home) = scratch_home();
        let needle = Needle::in_home(&home_script(&home), Some(home.clone()));
        let bare = serde_json::json!({"bash": "bash $HOME/.claude/hooks/taw-agent-state.sh"});
        let fresh = serde_json::json!({
            "bash": format!("bash {HOME_WORD}"),
            "timeoutSec": 10
        });

        let mut root = settings_with(vec![bare]);
        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh,
            Some(HOME_WORD),
        )
        .unwrap();
        assert_eq!(
            session_start(&root),
            &vec![serde_json::json!({
                "bash": "bash $HOME/.claude/hooks/taw-agent-state.sh",
                "timeoutSec": 10
            })]
        );

        assert!(strip_hook_entries_of(&mut root, HOOKS_KEY, "SessionStart", &needle).unwrap());
        assert!(root.get("hooks").is_none(), "{root}");
    }

    #[test]
    fn an_agent_that_is_not_known_to_run_a_shell_does_not_know_a_path_from_the_home_folder() {
        let (_tmp, home) = scratch_home();
        let script = home_script(&home);
        let mut root = settings_with(vec![group("*", vec![from_home_hook()])]);
        let fresh = group(
            "^(startup)$",
            vec![their_hook(&format!("bash '{script}' session"))],
        );

        // The home folder is the scratch one, so the verdict is the needle's alone.
        let needle = Needle::in_home(&script, None);
        assert!(matches!(needle.judge(&from_home_hook()), Owner::Unsure(_)));
        upsert_hook_entry_of(
            &mut root,
            HOOKS_KEY,
            "SessionStart",
            &needle,
            fresh.clone(),
            None,
        )
        .unwrap();

        assert_eq!(
            session_start(&root),
            &vec![group("*", vec![from_home_hook()]), fresh]
        );
    }

    // -------------------------------------------------------------------------------
    // What a render writes for the taw path
    // -------------------------------------------------------------------------------

    fn rendered_bin(lang: TemplateLang, bin: &str, home: Option<&str>) -> String {
        render_template_in(
            "[{{TAW_BIN}}]",
            lang,
            Path::new(bin),
            "x",
            home.map(Path::new),
        )
    }

    #[test]
    fn a_shell_script_names_taw_from_the_home_folder_when_it_is_there() {
        let home = Some("/Users/sam");
        // With `$HOME` unset the word is `/.config/...`, which is not a file: the script's own
        // `[ -x "$TAW_BIN" ]` then ends it quietly, where `"$HOME/..."` would stop `set -u`.
        assert_eq!(
            rendered_bin(TemplateLang::Sh, "/Users/sam/.config/tmux/bin/taw", home),
            r#"["${HOME-}/.config/tmux/bin/taw"]"#
        );
        assert_eq!(
            rendered_bin(TemplateLang::Sh, "/Users/sam/my bin/taw", home),
            r#"["${HOME-}/my bin/taw"]"#
        );
    }

    #[test]
    fn a_path_with_a_character_a_shell_reads_specially_is_written_in_full() {
        // Home-relative only for a plain path: none of `$`, a backtick, `\`, `"`, `'` or a
        // control character after the home folder.
        let home = Some("/Users/sam");
        for odd in ["$c", "`d`", "\\e", "\"b\"", "it's", "a\nb", "a\u{7}b"] {
            let bin = format!("/Users/sam/a {odd}/taw");
            let expected = format!("['{}']", bin.replace('\'', r"'\''"));
            assert_eq!(
                rendered_bin(TemplateLang::Sh, &bin, home),
                expected,
                "{odd:?}"
            );
            assert_eq!(
                sh_word_in(Path::new(&bin), home.map(Path::new)),
                expected[1..expected.len() - 1],
                "{odd:?}"
            );
        }
        // The home folder's own name is never written, so what it holds does not matter.
        assert_eq!(
            rendered_bin(
                TemplateLang::Sh,
                "/Users/s $am/bin/taw",
                Some("/Users/s $am")
            ),
            r#"["${HOME-}/bin/taw"]"#
        );
    }

    #[test]
    fn a_hook_entry_names_a_script_from_the_home_folder_with_home_set() {
        // A hook runs only when Claude has a home folder, so the entry says `$HOME` plainly.
        let home = Some(Path::new("/Users/sam"));
        assert_eq!(
            sh_word_in(
                Path::new("/Users/sam/.claude/hooks/taw-agent-state.sh"),
                home
            ),
            r#""$HOME/.claude/hooks/taw-agent-state.sh""#
        );
        assert_eq!(
            sh_word_in(Path::new("/opt/it's/taw-agent-state.sh"), home),
            r"'/opt/it'\''s/taw-agent-state.sh'"
        );
    }

    #[test]
    fn a_shell_script_names_taw_in_single_quotes_when_it_is_not_under_the_home_folder() {
        assert_eq!(
            rendered_bin(TemplateLang::Sh, "/opt/taw/bin/taw", Some("/Users/sam")),
            "['/opt/taw/bin/taw']"
        );
        assert_eq!(
            rendered_bin(TemplateLang::Sh, "/opt/it's/taw", Some("/Users/sam")),
            r"['/opt/it'\''s/taw']"
        );
        // A folder that only starts like the home folder, no home folder, a home folder of "/".
        for (bin, home) in [
            ("/Users/samuel/bin/taw", Some("/Users/sam")),
            ("/Users/sam/bin/taw", None),
            ("/Users/sam/bin/taw", Some("/")),
            ("/Users/sam", Some("/Users/sam")),
        ] {
            assert_eq!(
                rendered_bin(TemplateLang::Sh, bin, home),
                format!("['{bin}']"),
                "{bin} {home:?}"
            );
        }
    }

    #[test]
    fn a_javascript_plugin_names_taw_from_the_home_folder_when_it_is_there() {
        let home = Some("/Users/sam");
        assert_eq!(
            rendered_bin(TemplateLang::Js, "/Users/sam/.config/tmux/bin/taw", home),
            "[`${process.env.HOME}/.config/tmux/bin/taw`]"
        );
        assert_eq!(
            rendered_bin(TemplateLang::Js, "/Users/sam/my bin/taw", home),
            "[`${process.env.HOME}/my bin/taw`]"
        );
        // What a template literal reads specially (a backtick, `${`, a backslash) and what a
        // shell does too (`$`) keep the path in full, as a double-quoted string.
        assert_eq!(
            rendered_bin(TemplateLang::Js, "/Users/sam/a`b${c}\\d \"e\"/taw", home),
            "[\"/Users/sam/a`b${c}\\\\d \\\"e\\\"/taw\"]"
        );
        assert_eq!(
            rendered_bin(TemplateLang::Js, "/Users/sam/a$b/taw", home),
            r#"["/Users/sam/a$b/taw"]"#
        );
        assert_eq!(
            rendered_bin(TemplateLang::Js, "/opt/a \"b\"\\c/taw", home),
            r#"["/opt/a \"b\"\\c/taw"]"#
        );
    }

    #[test]
    fn the_hermes_plugin_names_taw_from_the_home_folder_when_it_is_there() {
        let home = Some("/Users/sam");
        assert_eq!(
            rendered_bin(TemplateLang::Py, "/Users/sam/a b/taw", home),
            r#"[os.path.join(os.path.expanduser("~"), "a b/taw")]"#
        );
        assert_eq!(
            rendered_bin(TemplateLang::Py, "/Users/sam/a \"b\"\\c/taw", home),
            r#"["/Users/sam/a \"b\"\\c/taw"]"#
        );
        assert_eq!(
            rendered_bin(TemplateLang::Py, "/opt/taw", home),
            r#"["/opt/taw"]"#
        );
    }

    /// Runs the shell script at `script` as `session` with no `HOME` in its environment (and
    /// `extra` set), the input on stdin, and says `(exit code, stdout)`.
    fn run_without_home(script: &Path, extra: &[(&str, &Path)]) -> (Option<i32>, String) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut command = Command::new("sh");
        command
            .arg(script)
            .arg("session")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TMUX_PANE", "%1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        // A script that ends at once, as one with no home does, may have closed its stdin
        // before this is written: that broken pipe is expected, and what it did is checked
        // from its exit and its output.
        let written = child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"session_id":"abc","hook_event_name":"SessionStart"}"#);
        if let Err(error) = written {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
        }
        let output = child.wait_with_output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn every_shell_hook_script_under_the_home_folder_ends_quietly_when_there_is_no_home() {
        let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integration/templates");
        let tmp = tempfile::tempdir().unwrap();
        let mut scripts = 0;
        for entry in fs::read_dir(&templates).unwrap() {
            let template = entry.unwrap().path();
            if template.extension().is_none_or(|ext| ext != "sh") {
                continue;
            }
            scripts += 1;
            let name = template.file_name().unwrap().to_string_lossy().into_owned();
            let text = render_template_in(
                &fs::read_to_string(&template).unwrap(),
                TemplateLang::Sh,
                Path::new("/Users/sam/.config/tmux/plugins/tmux-agent-wizard/bin/taw"),
                "claude",
                Some(Path::new("/Users/sam")),
            );
            let script = tmp.path().join(&name);
            fs::write(&script, text).unwrap();

            let (code, stdout) = run_without_home(&script, &[]);
            assert_eq!(code, Some(0), "{name}");
            // Antigravity's CLI wants an empty object on stdout from every exit.
            let quiet = if name.contains("antigravity") {
                "{}\n"
            } else {
                ""
            };
            assert_eq!(stdout, quiet, "{name}");

            // The sandbox branch reads the report file and needs no taw at all.
            let report = tmp.path().join(format!("{name}.report"));
            let (code, stdout) = run_without_home(&script, &[("TAW_REPORT_FILE", &report)]);
            assert_eq!(code, Some(0), "{name} in a sandbox");
            assert_eq!(stdout, quiet, "{name} in a sandbox");
        }
        assert_eq!(scripts, 13);
    }

    #[test]
    fn the_same_taw_in_the_home_folder_is_written_from_it_in_every_language_and_the_agent_is_filled_in()
     {
        let out = render_template_in(
            "{{AGENT}} {{TAW_BIN}}",
            TemplateLang::Js,
            Path::new("/Users/sam/bin/taw"),
            "opencode",
            Some(Path::new("/Users/sam")),
        );
        assert_eq!(out, "opencode `${process.env.HOME}/bin/taw`");
    }

    #[test]
    fn a_longer_path_that_ends_in_the_scripts_path_is_not_the_scripts_path() {
        // A copy of the home folder on a backup volume: the script's path is the end of another
        // path, and a hook that runs the script there is not taw's. Its folder is not the one
        // that was installed into, so what is said of it depends on whether the folder is there.
        let needle = Needle::new(SCRIPT);
        let longer = format!("/Volumes/Backup{SCRIPT}");
        for text in [
            format!("bash '{longer}' session"),
            format!("{longer} session"),
            format!("bash \"{longer}\""),
        ] {
            assert_eq!(
                needle.judge_text(&text),
                Owner::Unsure(longer.clone()),
                "{text}"
            );
        }
        // A start, a quote or a space before the path is the path of its own.
        for text in [
            SCRIPT.to_string(),
            format!("bash '{SCRIPT}' session"),
            format!("bash \"{SCRIPT}\" session"),
            format!("FOO=1 {SCRIPT} session"),
        ] {
            assert_eq!(needle.judge_text(&text), Owner::Ours, "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_longer_path_that_ends_in_the_scripts_path_in_a_folder_that_is_there_is_foreign() {
        let (_tmp, real, _link) = one_folder_two_names();
        let backup = tempfile::tempdir().unwrap();
        let ours = script_in(&real);
        let longer = format!(
            "{}{ours}",
            fs::canonicalize(backup.path()).unwrap().display()
        );
        fs::create_dir_all(Path::new(&longer).parent().unwrap()).unwrap();

        let text = format!("bash '{longer}' session");

        assert_eq!(
            Needle::new(&ours).judge_text(&text),
            Owner::Foreign,
            "{text}"
        );
    }

    #[test]
    fn a_longer_name_and_the_saved_copy_of_a_config_do_not_name_the_script() {
        let needle = Needle::new(SCRIPT);
        for text in [
            "bash /elsewhere/my-taw-agent-state.sh",
            "bash /elsewhere/taw-agent-state.shx",
            "cat /home/u/.claude/hooks/taw-agent-state.sh.orig.json",
            "echo hello",
            "",
        ] {
            assert_eq!(needle.judge_text(text), Owner::Foreign, "{text:?}");
        }
    }

    #[test]
    fn a_hook_is_taws_if_any_of_its_strings_names_the_script() {
        let needle = Needle::new(SCRIPT);
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        let both = serde_json::json!({
            "type": "command",
            "command": format!("bash '{stranger}' session"),
            "also": [{"command": format!("bash '{SCRIPT}' session")}],
        });
        assert_eq!(needle.judge(&both), Owner::Ours);
        let neither = serde_json::json!({"type": "command", "command": "echo keep", "timeout": 3});
        assert_eq!(needle.judge(&neither), Owner::Foreign);
        let only_the_stranger = serde_json::json!({"command": format!("bash '{stranger}'")});
        assert_eq!(
            needle.judge(&only_the_stranger),
            Owner::Unsure(stranger.to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_reinstall_through_another_name_replaces_the_entry_and_does_not_add_a_second() {
        let (_tmp, real, link) = one_folder_two_names();
        let there = script_in(&link);
        let here = script_in(&real);
        let theirs = their_hook("echo keep");
        let old = serde_json::json!({
            "type": "command",
            "command": format!("bash '{there}' session"),
            "timeout": 10
        });
        let mut root = settings_with(vec![group("*", vec![old, theirs.clone()])]);
        let fresh = group(
            "^(startup)$",
            vec![serde_json::json!({
                "type": "command",
                "command": format!("bash '{here}' session"),
                "timeout": 10
            })],
        );

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", &here, fresh.clone()).unwrap();

        assert_eq!(session_start(&root), &vec![group("*", vec![theirs]), fresh]);
    }

    #[test]
    fn a_reinstall_leaves_a_hook_it_cannot_tell_from_its_own_where_it_is() {
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        let unsure = their_hook(&format!("bash '{stranger}' session"));
        let mut root = settings_with(vec![group("*", vec![unsure.clone(), our_hook()])]);
        let fresh = group("^(startup)$", vec![our_hook()]);

        upsert_hook_entry(&mut root, HOOKS_KEY, "SessionStart", SCRIPT, fresh.clone()).unwrap();

        assert_eq!(session_start(&root), &vec![group("*", vec![unsure]), fresh]);
    }

    #[test]
    fn strip_refuses_a_hook_it_cannot_tell_from_taws_and_says_which() {
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        let mut root = settings_with(vec![
            group("*", vec![our_hook()]),
            group("*", vec![their_hook(&format!("bash '{stranger}' session"))]),
        ]);

        let err = strip_hook_entries(&mut root, HOOKS_KEY, "SessionStart", SCRIPT).unwrap_err();

        match err.downcast_ref::<IntegrationError>() {
            Some(IntegrationError::UnmatchedHook {
                spelled, script, ..
            }) => {
                assert_eq!(spelled, stranger);
                assert_eq!(script, SCRIPT);
            }
            other => panic!("not an UnmatchedHook: {other:?}"),
        }
    }

    #[test]
    fn an_uninstall_that_refuses_a_hook_changes_nothing_not_even_the_saved_copy() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        fs::write(&sidecar, "{}").unwrap();
        let installed = serde_json::json!({"hooks": {"SessionStart": [
            group("*", vec![our_hook()]),
            group("*", vec![their_hook(&format!("bash '{stranger}' session"))]),
        ]}});
        let bytes = serde_json::to_vec_pretty(&installed).unwrap();
        fs::write(&config, &bytes).unwrap();

        let err =
            uninstall_json_hook(&config, &sidecar, HOOKS_KEY, "SessionStart", SCRIPT).unwrap_err();

        assert!(
            err.to_string().contains(&config.display().to_string()),
            "{err}"
        );
        assert_eq!(fs::read(&config).unwrap(), bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), b"{}");
    }

    #[test]
    fn a_saved_copy_with_a_hook_in_it_that_cannot_be_placed_is_not_restored_whole() {
        // The copy was taken when the file held a hook that may have been taw's (a folder that
        // has since gone): putting those bytes back would put that hook back beside a script the
        // uninstall deletes. The file as it stands is stripped instead, and keeps what else is in
        // it. The hook is not in the file any more, so nothing refuses.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
        let before = serde_json::json!({"theme": "dark", "hooks": {"SessionStart": [
            group("*", vec![their_hook(&format!("bash '{stranger}' session"))]),
        ]}});
        fs::write(&sidecar, serde_json::to_vec_pretty(&before).unwrap()).unwrap();
        let now = serde_json::json!({"theme": "dark", "hooks": {"SessionStart": [
            group("*", vec![our_hook()]),
        ]}});
        fs::write(&config, serde_json::to_vec_pretty(&now).unwrap()).unwrap();

        uninstall_json_hook(&config, &sidecar, HOOKS_KEY, "SessionStart", SCRIPT).unwrap();

        let after: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(after, serde_json::json!({"theme": "dark"}));
        assert!(!sidecar.exists());
    }

    // -------------------------------------------------------------------------------
    // ConfigEdit: an install settles its config edits before it writes anything
    // -------------------------------------------------------------------------------

    fn plan_session_start(config: &Path, sidecar: &Path) -> Result<ConfigEdit> {
        plan_json_hook(
            config,
            sidecar,
            HOOKS_KEY,
            "SessionStart",
            SCRIPT,
            group("*", vec![our_hook()]),
        )
    }

    #[test]
    fn a_planned_edit_writes_nothing_until_it_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&config, "{\"theme\":\"dark\"}\n").unwrap();

        let plan = plan_session_start(&config, &sidecar).unwrap();

        assert_eq!(fs::read(&config).unwrap(), b"{\"theme\":\"dark\"}\n");
        assert!(!sidecar.exists(), "planning saves no backup");

        plan.apply().unwrap();
        assert_eq!(
            fs::read(&sidecar).unwrap(),
            b"{\"theme\":\"dark\"}\n",
            "the backup is the file as it was found"
        );
        assert!(fs::read_to_string(&config).unwrap().contains(SCRIPT));
    }

    #[test]
    fn planning_over_json_it_cannot_parse_fails_and_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&config, "not json").unwrap();

        let err = plan_session_start(&config, &sidecar).unwrap_err();

        assert!(format!("{err:#}").contains("settings.json"), "{err:#}");
        assert!(!sidecar.exists());
        assert_eq!(fs::read(&config).unwrap(), b"not json");
    }

    #[test]
    fn planning_over_a_shape_no_hook_can_be_added_to_fails_and_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&config, r#"{"hooks": null}"#).unwrap();

        let err = plan_session_start(&config, &sidecar).unwrap_err();

        assert!(
            matches!(
                err.downcast_ref::<IntegrationError>(),
                Some(IntegrationError::UnusableHooksFile { .. })
            ),
            "{err:#}"
        );
        assert!(!sidecar.exists());
    }

    #[test]
    fn a_later_plan_leaves_the_first_backup_alone() {
        // A reinstall must not replace the file-as-it-was-before-taw with the file as the
        // first install left it.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&config, "{\"theme\":\"dark\"}\n").unwrap();
        plan_session_start(&config, &sidecar)
            .unwrap()
            .apply()
            .unwrap();

        plan_session_start(&config, &sidecar)
            .unwrap()
            .apply()
            .unwrap();

        assert_eq!(fs::read(&sidecar).unwrap(), b"{\"theme\":\"dark\"}\n");
    }

    #[test]
    fn planning_over_text_that_is_not_utf8_fails_and_leaves_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let sidecar = dir.path().join("hook.sh.orig.toml");
        fs::write(&config, [b'm', b'=', 0xff]).unwrap();

        let err = plan_toml_block(&config, &sidecar, "[features]\nhooks = true\n").unwrap_err();

        assert!(err.to_string().contains("config.toml"), "{err}");
        assert!(!sidecar.exists());
    }

    #[test]
    fn a_target_that_is_not_a_regular_file_is_refused_at_planning() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        fs::create_dir(&config).unwrap();

        let err = plan_session_start(&config, &dir.path().join("x.orig.json")).unwrap_err();

        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn uninstall_ignores_a_saved_copy_that_is_not_json() {
        // A copy an earlier install left after stopping on a config it could not parse. It
        // cannot say what the file looked like, so it cannot be restored from; the file is
        // stripped as it stands and the copy is dropped so it cannot block the next run.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&sidecar, "not json").unwrap();
        let installed = serde_json::json!({
            "theme": "dark",
            "hooks": {"SessionStart": [
                group("*", vec![their_hook("echo keep")]),
                group("*", vec![our_hook()]),
            ]}
        });
        fs::write(&config, serde_json::to_vec_pretty(&installed).unwrap()).unwrap();

        uninstall_json_hook(&config, &sidecar, HOOKS_KEY, "SessionStart", SCRIPT).unwrap();

        let now: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        assert_eq!(now["theme"], "dark");
        assert_eq!(
            session_start(&now),
            &vec![group("*", vec![their_hook("echo keep")])]
        );
        assert!(!sidecar.exists());
    }

    #[test]
    fn a_saved_copy_that_is_not_json_stays_when_the_uninstall_removes_nothing() {
        // The copy cannot say what the file looked like, but it is the user's file as it was
        // found, and the only one: it goes when taw's entry has been taken out of the config, and
        // not when there was nothing of taw's to take out (the entry was taken out by hand).
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&sidecar, "not json").unwrap();
        let theirs = serde_json::json!({
            "theme": "dark",
            "hooks": {"SessionStart": [group("*", vec![their_hook("echo keep")])]}
        });
        let bytes = serde_json::to_vec_pretty(&theirs).unwrap();
        fs::write(&config, &bytes).unwrap();

        uninstall_json_hook(&config, &sidecar, HOOKS_KEY, "SessionStart", SCRIPT).unwrap();

        assert_eq!(
            fs::read(&config).unwrap(),
            bytes,
            "the config was rewritten"
        );
        assert_eq!(
            fs::read(&sidecar).unwrap(),
            b"not json",
            "the saved copy went though nothing was removed"
        );
    }

    #[test]
    fn a_saved_copy_that_is_not_json_stays_when_the_config_is_not_json_either() {
        // Nothing can be said of the file, so nothing is taken out of it and nothing is given up.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("settings.json");
        let sidecar = dir.path().join("hook.sh.orig.json");
        fs::write(&sidecar, "not json").unwrap();
        fs::write(&config, "{ also not json").unwrap();

        uninstall_json_hook(&config, &sidecar, HOOKS_KEY, "SessionStart", SCRIPT).unwrap();

        assert_eq!(fs::read(&config).unwrap(), b"{ also not json");
        assert_eq!(fs::read(&sidecar).unwrap(), b"not json");
    }

    // -------------------------------------------------------------------------------
    // Removing a config: a dotfiles link stays, what it points at goes
    // -------------------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn removing_a_symlinked_config_removes_what_it_points_at_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-settings.json");
        let link = dir.path().join("settings.json");
        fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_config_file(&link).unwrap();

        assert!(!target.exists());
        assert_eq!(fs::read_link(&link).unwrap(), target, "the link stays");
        // Already gone is not an error: a second uninstall must be a no-op.
        remove_config_file(&link).unwrap();
        remove_config_file(&dir.path().join("never-existed.json")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn emptying_a_symlinked_json_config_removes_the_file_behind_the_link() {
        // Once nothing is left in a config, `write_json_value_or_remove` deletes it: for a
        // dotfiles link that has to be the file, not the link.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-settings.json");
        let link = dir.path().join("settings.json");
        fs::write(&target, r#"{"stale":true}"#).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_json_value_or_remove(&link, &serde_json::json!({})).unwrap();

        assert!(!target.exists());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    // -------------------------------------------------------------------------------
    // write_config_file — ported from herdr's `integration/config_file/tests.rs`
    // -------------------------------------------------------------------------------

    #[test]
    fn write_config_file_replaces_existing_content_and_creates_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing");
        fs::write(&existing, b"old preferences").unwrap();
        write_config_file(&existing, b"complete new preferences").unwrap();
        assert_eq!(fs::read(&existing).unwrap(), b"complete new preferences");

        let new = dir.path().join("new");
        write_config_file(&new, b"first install").unwrap();
        assert_eq!(fs::read(&new).unwrap(), b"first install");

        // No staging leftovers in either case.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    /// A write that cannot even begin (no permission to
    /// create the staging file next to the target) must leave the original bytes alone,
    /// not a truncated or empty file.
    #[cfg(unix)]
    #[test]
    fn write_config_file_leaves_the_original_untouched_when_the_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        if crate::test_root::skip_as_root("root writes into a folder of mode 0500") {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing");
        fs::write(&path, b"original").unwrap();
        let original_mode = fs::metadata(&path).unwrap().permissions().mode();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_config_file(&path, b"must not land");

        // Restore write access before any assertion can panic and leave a stuck tempdir.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "a read-only directory must fail the write");
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode(),
            original_mode
        );
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no leftover staging file"
        );
    }

    /// Whether this process is root, asked without libc: a file it creates belongs to its
    /// effective user.
    #[cfg(unix)]
    fn running_as_root(dir: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let probe = dir.join("owner-probe");
        fs::write(&probe, b"").unwrap();
        let root = fs::metadata(&probe).unwrap().uid() == 0;
        fs::remove_file(&probe).unwrap();
        root
    }

    /// Follows herdr's `writable_directory_does_not_bypass_read_only_config`. A rename
    /// replaces a file whatever the file's own mode says, provided its directory is
    /// writable, so a read-only config is overwritten unless the write is refused first.
    /// Runs in a child process, as herdr's does, because root ignores mode bits: as root
    /// the child drops to `nobody`, so the real user path is what gets tested.
    #[cfg(unix)]
    #[test]
    fn write_config_file_refuses_a_read_only_config_in_a_writable_directory() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        const CHILD: &str = "TAW_CONFIG_READ_ONLY_TEST";
        if let Some(path) = std::env::var_os(CHILD) {
            let err = write_config_file(Path::new(&path), b"must not replace").unwrap_err();
            assert!(
                matches!(
                    err.downcast_ref::<IntegrationError>(),
                    Some(IntegrationError::ConfigReadOnly { .. })
                ),
                "{err:#}"
            );
            println!("read-only rejection executed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read-only");
        fs::write(&path, b"original").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "integration::tests::write_config_file_refuses_a_read_only_config_in_a_writable_directory",
                "--nocapture",
            ])
            .env(CHILD, &path);
        if running_as_root(dir.path()) {
            child.gid(65534).uid(65534);
        }
        let output = child.output().unwrap();
        assert!(output.status.success(), "child failed: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("read-only rejection executed"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Follows herdr's `partial_write_errors_preserve_files_and_do_not_remove_
    /// collisions`. A write that dies partway (here: the file-size limit, `EFBIG`) removes
    /// its own staging file and leaves the original alone, and an unrelated file that
    /// already has the name the staging file would have taken is neither used nor removed.
    /// Runs as a child of a shell that sets the limit and ignores the signal it raises, and
    /// resets the counter so the first name it would try is the one that is taken.
    #[cfg(unix)]
    #[test]
    fn write_config_file_partial_write_errors_keep_files_and_never_remove_collisions() {
        use std::sync::atomic::Ordering;
        const CHILD: &str = "TAW_CONFIG_PARTIAL_WRITE_TEST";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            NEXT_TEMP.store(0, Ordering::Relaxed);
            let collision = dir.join(format!(".taw-config-{}-0.tmp", std::process::id()));
            fs::write(&collision, b"unrelated file").unwrap();
            for name in ["existing", "new"] {
                let err = write_config_file(&dir.join(name), &vec![b'x'; 8192]).unwrap_err();
                let io = err.downcast_ref::<std::io::Error>().unwrap();
                assert_eq!(io.kind(), std::io::ErrorKind::FileTooLarge, "{err:#}");
            }
            assert_eq!(fs::read(collision).unwrap(), b"unrelated file");
            println!("partial-write paths executed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("existing"), b"original").unwrap();
        let output = std::process::Command::new("bash")
            .args(["-c", "trap '' XFSZ; ulimit -f 1; exec \"$@\"", "taw-test"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "integration::tests::write_config_file_partial_write_errors_keep_files_and_never_remove_collisions",
                "--nocapture",
            ])
            .env(CHILD, dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "child failed: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("partial-write paths executed"));
        assert_eq!(fs::read(dir.path().join("existing")).unwrap(), b"original");
        assert!(!dir.path().join("new").exists());
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "only the original and the collision may remain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_config_file_rejects_a_hard_linked_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let alias = dir.path().join("alias");
        fs::write(&path, b"original").unwrap();
        fs::hard_link(&path, &alias).unwrap();

        for candidate in [&path, &alias] {
            let err = write_config_file(candidate, b"new").unwrap_err();
            assert!(
                format!("{err:#}").contains("multiple hard links"),
                "{err:#}"
            );
            assert_eq!(fs::read(candidate).unwrap(), b"original");
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn write_config_file_follows_a_symlink_chain_and_a_dangling_target_and_preserves_it() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        let target = other.join("preferences");
        let intermediate = other.join("link");
        let entry = dir.path().join("config");
        std::os::unix::fs::symlink(&target, &intermediate).unwrap();
        std::os::unix::fs::symlink(Path::new("other").join("link"), &entry).unwrap();

        assert!(!target.exists(), "the chain starts dangling");
        write_config_file(&entry, b"first install").unwrap();
        write_config_file(&entry, b"second install").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"second install");
        assert_eq!(
            fs::read_link(&entry).unwrap(),
            Path::new("other").join("link"),
            "the entry is still a symlink to the same place"
        );
        assert_eq!(
            fs::read_link(&intermediate).unwrap(),
            target,
            "the intermediate link is unchanged too"
        );
        // "link" (the intermediate symlink) and "preferences" (the resolved target) —
        // writing through the chain never materialised a third file in `other/`.
        assert_eq!(fs::read_dir(&other).unwrap().count(), 2);

        // The entry has one link of its own, but what it resolves to now has two: the
        // write is refused, and nothing it points at changes.
        let alias = other.join("hard-link");
        fs::hard_link(&target, &alias).unwrap();
        assert!(write_config_file(&entry, b"must not change").is_err());
        assert_eq!(fs::read(&alias).unwrap(), b"second install");
        assert_eq!(
            fs::read_link(&entry).unwrap(),
            Path::new("other").join("link")
        );

        // A link that points at itself stops at the hop limit instead of looping, and the
        // link is left as it was.
        let cycle = dir.path().join("cycle");
        std::os::unix::fs::symlink("cycle", &cycle).unwrap();
        assert!(write_config_file(&cycle, b"must not replace the link").is_err());
        assert_eq!(fs::read_link(&cycle).unwrap(), Path::new("cycle"));
    }

    #[cfg(unix)]
    #[test]
    fn write_config_file_preserves_existing_permissions_and_uses_ordinary_defaults_for_a_new_file()
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing");
        fs::write(&existing, b"original").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o640)).unwrap();

        write_config_file(&existing, b"new").unwrap();
        assert_eq!(
            fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o640
        );

        // A brand new file gets whatever an ordinary `fs::write` in the same directory
        // would (subject to umask), not a leftover private mode from staging.
        let ordinary = dir.path().join("ordinary");
        fs::write(&ordinary, b"ordinary creation").unwrap();
        let new = dir.path().join("new");
        write_config_file(&new, b"atomic creation").unwrap();
        assert_eq!(
            fs::metadata(&new).unwrap().permissions().mode(),
            fs::metadata(&ordinary).unwrap().permissions().mode()
        );
    }

    /// What an older taw left: its block appended with one blank line to a file with no final
    /// newline, and then text of the user's right after the end marker.
    const AFTER_BLOCK: [&str; 2] = [
        "[mcp_servers.x]\ncommand = \"y\"\n",
        "[projects.\"/home/you/code/api\"]\ntrust_level = \"trusted\"\n",
    ];

    #[test]
    fn taking_a_block_out_never_joins_the_line_before_it_to_the_line_after_it() {
        for after in AFTER_BLOCK {
            let block = render_toml_block("[features]\nhooks = true\n");
            let text = format!("model = \"gpt-5\"\n\n{block}{after}");
            let expected = format!("model = \"gpt-5\"\n{after}");
            assert_eq!(strip_toml_block(&text), expected);
            let range = toml_block_range(&text).unwrap();
            assert_eq!(remove_toml_block_at(&text, range), expected);
            // The same with a final newline before the block: two blank lines stand there.
            let text = format!("model = \"gpt-5\"\n\n\n{block}{after}");
            let expected = format!("model = \"gpt-5\"\n{after}");
            assert_eq!(strip_toml_block(&text), expected);
            let range = toml_block_range(&text).unwrap();
            assert_eq!(remove_toml_block_at(&text, range), expected);
        }
    }

    #[test]
    fn a_block_at_the_end_still_comes_out_as_the_inverse_of_appending_it() {
        for original in ["", "a = 1", "a = 1\n", "a = 1\n\n"] {
            let appended = append_toml_block(original, "[features]\nhooks = true\n");
            assert_eq!(strip_toml_block(&appended), original);
            let range = toml_block_range(&appended).unwrap();
            assert_eq!(remove_toml_block_at(&appended, range), original);
        }
    }
}
