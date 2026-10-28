// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Kimi Code CLI integration.
//!
//! Writes `<KIMI_CODE_HOME|~/.kimi-code>/hooks/taw-agent-state.sh` and appends a delimited
//! taw block of `[[hooks]]` tables — one per entry in [`EVENTS`] — to
//! `<same dir>/config.toml`.
//!
//! Kimi's hooks cover the whole lifecycle, so — as in herdr — this integration is the
//! pane's state authority: it reports `idle`/`working`/`blocked` as well as session
//! identity. It needs Kimi Code [`MIN_VERSION`] or newer, which [`check_version`] enforces
//! before anything is written.
//!
//! Follows herdr's `kimi` integration: the hook script from
//! `third_party/herdr/integrations/kimi/herdr-agent-state.sh`, the config block from
//! herdr's `src/integration/config_edit.rs::build_kimi_config_with_hooks`, and the version
//! gate from its `src/integration/version.rs`.

use super::{
    Incomplete, Report, Status, TemplateLang, env_dir, has_toml_block, home_dir,
    parse_version_marker, plan_toml_block, render_template, require_config_dir, strip_toml_block,
    toml_basic_string, uninstall_toml_block, write_executable,
};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("templates/taw-agent-state-kimi.sh");
const VERSION_MARKER_PREFIX: &str = "# TAW_INTEGRATION_VERSION=";
const AGENT: &str = "kimi";
/// The oldest Kimi Code whose hook schema these entries fit, from herdr's
/// `KIMI_MIN_VERSION`.
const MIN_VERSION: &str = "0.14.0";

/// Kimi routes its "ask the user a question" tool through the same `PreToolUse`/`PostToolUse`
/// events as every other tool, so the two are told apart by matcher: that tool means
/// `blocked`, anything else means `working`.
const ASK_USER_QUESTION_MATCHER: &str = "^AskUserQuestion$";
const OTHER_TOOL_MATCHER: &str = "^(?!AskUserQuestion$).*$";

/// `(event, matcher, reported action)`, in herdr's order.
const EVENTS: [(&str, Option<&str>, &str); 12] = [
    ("SessionStart", None, "session"),
    ("UserPromptSubmit", None, "working"),
    ("PreToolUse", Some(OTHER_TOOL_MATCHER), "working"),
    ("PreToolUse", Some(ASK_USER_QUESTION_MATCHER), "blocked"),
    ("PostToolUse", Some(ASK_USER_QUESTION_MATCHER), "working"),
    (
        "PostToolUseFailure",
        Some(ASK_USER_QUESTION_MATCHER),
        "working",
    ),
    ("SubagentStart", None, "working"),
    ("PreCompact", None, "working"),
    ("PermissionRequest", None, "blocked"),
    ("PermissionResult", None, "working"),
    ("Stop", None, "idle"),
    ("Interrupt", None, "idle"),
];

fn config_dir() -> Result<PathBuf> {
    match env_dir("KIMI_CODE_HOME") {
        Some(dir) => Ok(dir),
        None => Ok(home_dir()?.join(".kimi-code")),
    }
}

fn script_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("hooks").join("taw-agent-state.sh"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn sidecar_path(script: &Path) -> PathBuf {
    let mut name = script.as_os_str().to_owned();
    name.push(".orig.toml");
    PathBuf::from(name)
}

/// The `[[hooks]]` tables for [`EVENTS`], in herdr's field order.
fn hook_tables(script: &Path) -> String {
    let path = script.display().to_string();
    let mut out = String::with_capacity(EVENTS.len() * 160);
    for (event, matcher, action) in EVENTS {
        out.push_str("[[hooks]]\n");
        out.push_str(&format!("event = {}\n", toml_basic_string(event)));
        if let Some(matcher) = matcher {
            out.push_str(&format!("matcher = {}\n", toml_basic_string(matcher)));
        }
        out.push_str(&format!(
            "command = {}\n",
            toml_basic_string(&format!("bash '{path}' {action}"))
        ));
        out.push_str("timeout = 10\n\n");
    }
    out
}

/// `text` as `(major, minor, patch)`, from the first whitespace-separated token that parses
/// as one. Follows herdr's `version::extract_version_triple`, so `kimi --version`
/// output like `kimi 0.14.2 (abc123)` or `v1.2` is read the same way.
fn version_triple(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches('v');
        let mut parts = token.splitn(3, '.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next()?.parse().ok()?;
        let patch: u64 = parts
            .next()
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0);
        Some((major, minor, patch))
    })
}

/// `Ok(None)` when the installed Kimi Code is new enough, `Ok(Some(warning))` when its
/// version cannot be determined — install proceeds, saying so — and `Err` when it is too
/// old, because these hook entries would not work and writing them would look like success.
fn check_version() -> Result<Option<String>> {
    let required = version_triple(MIN_VERSION).expect("MIN_VERSION is a valid version triple");
    let unknown = || {
        Some(format!(
            "warning: could not run `kimi --version` to verify the installed version; \
             these hooks require kimi code {MIN_VERSION} or newer"
        ))
    };

    let output = match std::process::Command::new("kimi").arg("--version").output() {
        Ok(output) if output.status.success() => output,
        _ => return Ok(unknown()),
    };
    let Some(found) = version_triple(&String::from_utf8_lossy(&output.stdout)) else {
        return Ok(Some(format!(
            "warning: could not parse the kimi code version out of `kimi --version`; \
             these hooks require kimi code {MIN_VERSION} or newer"
        )));
    };
    if found < required {
        bail!(
            "kimi code {}.{}.{} is too old: these hooks require kimi code {MIN_VERSION} or \
             newer. upgrade kimi code, then re-run install",
            found.0,
            found.1,
            found.2
        );
    }
    Ok(None)
}

pub(super) fn install(taw_bin: &Path) -> Result<Report> {
    require_config_dir(&config_dir()?, AGENT)?;
    let warning = check_version()?;

    let script = script_path()?;
    let rendered = render_template(TEMPLATE, TemplateLang::Sh, taw_bin, AGENT);
    let config = config_path()?;
    // Everything that can refuse the config is settled before a byte is written: no hook
    // script is published for a config this install then cannot edit.
    let config_edit = plan_toml_block(&config, &sidecar_path(&script), &hook_tables(&script))?;

    write_executable(&script, &rendered)?;
    config_edit
        .apply()
        .with_context(|| format!("updating {}", config.display()))?;

    let mut message =
        format!("installed kimi integration (requires kimi code {MIN_VERSION} or newer)");
    if let Some(warning) = warning {
        message.push_str("; ");
        message.push_str(&warning);
    }
    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, config],
        message,
    })
}

pub(super) fn uninstall() -> Result<Report> {
    let script = script_path()?;
    let config = config_path()?;

    uninstall_toml_block(&config, &sidecar_path(&script))?;
    if script.exists() {
        std::fs::remove_file(&script).with_context(|| format!("removing {}", script.display()))?;
    }

    Ok(Report {
        agent: AGENT.to_string(),
        files: vec![script, config],
        message: "uninstalled kimi integration".to_string(),
    })
}

/// A `config.toml` Kimi cannot load because of an older taw's block in it (the file parses once
/// the block is out): an install takes the block out, so a reinstall repairs it. A file that does
/// not parse for another reason is not taw's doing and is not reported.
fn config_incomplete(config: &Path) -> Option<Incomplete> {
    let text = std::fs::read_to_string(config).ok()?;
    let broken_by_taw = has_toml_block(&text)
        && text.parse::<toml::Table>().is_err()
        && strip_toml_block(&text).parse::<toml::Table>().is_ok();
    broken_by_taw.then(|| {
        Incomplete::Reinstall(format!(
            "Kimi Code cannot load {}: an older taw's block is in it, beside a hooks entry of \
             yours that [[hooks]] tables clash with; a reinstall takes the block out and says \
             what to change. Run `taw integration install kimi` again.",
            config.display()
        ))
    })
}

pub(super) fn status() -> Result<Status> {
    let script = script_path()?;
    let (installed, version) = match std::fs::read_to_string(&script) {
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
        path: script,
        version,
        current: parse_version_marker(TEMPLATE, VERSION_MARKER_PREFIX),
        incomplete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_triple_reads_the_shapes_kimi_prints() {
        assert_eq!(version_triple("kimi 0.14.2"), Some((0, 14, 2)));
        assert_eq!(version_triple("v1.2"), Some((1, 2, 0)));
        assert_eq!(version_triple("0.15.0-rc1 (abc)"), Some((0, 15, 0)));
        assert_eq!(version_triple("no version here"), None);
        // The comparison the gate makes, spelled out: 0.13.x is refused, 0.14.0 is not.
        let required = version_triple(MIN_VERSION).unwrap();
        assert!(version_triple("kimi 0.13.9").unwrap() < required);
        assert!(version_triple("kimi 0.14.0").unwrap() >= required);
        assert!(version_triple("kimi 1.0.0").unwrap() >= required);
    }

    #[test]
    fn hook_tables_quote_a_path_that_would_otherwise_break_the_toml() {
        let tables = hook_tables(Path::new(r#"/tmp/we"ird\path/taw-agent-state.sh"#));
        assert!(
            tables.contains(r#"command = "bash '/tmp/we\"ird\\path/taw-agent-state.sh' session""#),
            "{tables}"
        );
        // Every event gets a table, and the two tool matchers are both present.
        assert_eq!(tables.matches("[[hooks]]").count(), EVENTS.len());
        assert!(tables.contains(r#"matcher = "^AskUserQuestion$""#));
        assert!(tables.contains(r#"matcher = "^(?!AskUserQuestion$).*$""#));
    }
}
