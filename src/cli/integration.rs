//! `taw integration …` — installs the agent-side hook scripts that report state back to
//! `taw`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::integration::{self, Status};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Install an integration
    Install {
        #[arg(value_name = "TARGET")]
        target: String,
    },
    /// Uninstall an integration
    Uninstall {
        #[arg(value_name = "TARGET")]
        target: String,
    },
    /// Show integration status
    Status {
        /// One agent, or every supported agent when omitted.
        #[arg(value_name = "TARGET")]
        target: Option<String>,
        /// Report only integrations that installing again would change: an installed script
        /// older than this build's, or a registration the install has yet to add.
        #[arg(long = "outdated-only")]
        outdated_only: bool,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Install { target } => ("install", install(ctx, &target)),
        Cmd::Uninstall { target } => ("uninstall", uninstall(&target)),
        Cmd::Status {
            target,
            outdated_only,
        } => ("status", status(target.as_deref(), outdated_only)),
    }
}

/// herdr supports more agent kinds than we have ported hook scripts for. Rather than a
/// clap-level rejection (exit 2, no envelope), an unported-but-real herdr target reports
/// `unsupported` in the envelope, so a caller can tell "not supported here" from "you
/// typo'd".
fn check_agent(agent: &str) -> anyhow::Result<()> {
    if integration::AGENTS.contains(&agent) {
        return Ok(());
    }
    Err(CliError::unsupported(format!(
        "no integration for '{agent}'; supported: {}",
        integration::AGENTS.join(", ")
    )))
}

fn install(ctx: &Ctx, agent: &str) -> anyhow::Result<Value> {
    check_agent(agent)?;
    let report = integration::install(agent, &ctx.taw_bin)?;
    Ok(json!({
        "agent": report.agent,
        "files": report.files,
        "message": report.message,
    }))
}

fn uninstall(agent: &str) -> anyhow::Result<Value> {
    check_agent(agent)?;
    let report = integration::uninstall(agent)?;
    Ok(json!({
        "agent": report.agent,
        "files": report.files,
        "message": report.message,
    }))
}

fn status_json(status: &Status) -> Value {
    json!({
        "agent": status.agent,
        "installed": status.installed,
        "path": status.path,
        "version": status.version,
        "current_version": status.current,
        "outdated": status.outdated(),
        "note": status.note(),
        "experimental": integration::EXPERIMENTAL.contains(&status.agent.as_str()),
    })
}

/// One shape for both forms: a single `--target` still reports a one-element list, so a
/// caller parses `integration status` and `integration status claude` identically.
fn status(agent: Option<&str>, outdated_only: bool) -> anyhow::Result<Value> {
    let statuses = match agent {
        Some(agent) => {
            check_agent(agent)?;
            vec![integration::status(agent)?]
        }
        None => integration::status_all(),
    };
    let integrations: Vec<Value> = statuses
        .iter()
        .filter(|s| !outdated_only || s.outdated())
        .map(status_json)
        .collect();
    Ok(json!({ "integrations": integrations }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::envelope::code_for;
    use crate::integration::Incomplete;

    #[test]
    fn accepts_every_ported_agent() {
        for agent in integration::AGENTS {
            assert!(check_agent(agent).is_ok(), "rejected {agent}");
        }
    }

    #[test]
    fn reports_unsupported_for_a_herdr_agent_we_have_not_ported() {
        // Real herdr targets from its `integration install` help that we do not support.
        for agent in ["amp", "cline", "gemini", "kiro", "maki"] {
            let err = check_agent(agent).unwrap_err();
            assert_eq!(code_for(&err), "unsupported", "accepted {agent}");
            assert!(err.to_string().contains(agent));
        }
    }

    #[test]
    fn a_missing_agent_config_dir_reports_config_dir_missing() {
        // A missing config dir means the agent itself is not installed. The code has to
        // come from a *typed* error: as a formatted string it could not be matched on and
        // fell through to the catch-all `internal`, even though `config_dir_missing` is
        // declared and SKILL.md promises it.
        let err = anyhow::Error::new(crate::integration::IntegrationError::ConfigDirMissing {
            path: "/nonexistent/.claude".to_string(),
            agent: "claude".to_string(),
        });
        assert_eq!(code_for(&err), "config_dir_missing");
        assert!(err.to_string().contains("/nonexistent/.claude"), "{err}");
        assert!(err.to_string().contains("claude"), "{err}");

        // And it survives whatever context an installer wraps around it.
        let wrapped = anyhow::Error::new(crate::integration::IntegrationError::ConfigDirMissing {
            path: "/nonexistent/.codex".to_string(),
            agent: "codex".to_string(),
        })
        .context("installing the codex integration");
        assert_eq!(code_for(&wrapped), "config_dir_missing");
    }

    #[test]
    fn a_read_only_config_reports_invalid_argument_and_says_which_file() {
        // The envelope prints only the outermost message, so it has to carry the file and
        // the reason itself; and, like an unusable hooks file, it is something the person
        // running the install has to change, not a fault in taw.
        let err = anyhow::Error::new(crate::integration::IntegrationError::ConfigReadOnly {
            path: "/home/u/.claude/settings.json".to_string(),
        });
        assert_eq!(code_for(&err), "invalid_argument");
        assert!(
            err.to_string()
                .contains("/home/u/.claude/settings.json is read-only"),
            "{err}"
        );
    }

    /// An installed integration at `version`, whose build writes `current`.
    fn installed(agent: &str, version: Option<u32>, current: Option<u32>) -> Status {
        Status {
            agent: agent.into(),
            installed: true,
            path: "/x".into(),
            version,
            current,
            incomplete: None,
        }
    }

    #[test]
    fn outdated_only_measures_each_integration_against_its_own_current_version() {
        // The versions are bumped one integration at a time, so one number for all of them
        // called a version 2 OpenCode plugin (this build writes 3) up to date.
        assert!(!installed("claude", Some(3), Some(3)).outdated());
        assert!(installed("opencode", Some(2), Some(3)).outdated());
        assert!(!installed("codex", Some(1), Some(1)).outdated());
        assert!(!installed("grok", Some(2), Some(2)).outdated());
        // Ahead of this build (installed by a newer one) is not behind it.
        assert!(!installed("claude", Some(4), Some(3)).outdated());
        // A script with no marker at all is older than any that has one.
        assert!(installed("codex", None, Some(1)).outdated());
        let not_installed = Status {
            installed: false,
            ..installed("copilot", None, Some(1))
        };
        assert!(
            !not_installed.outdated(),
            "a missing integration is not 'outdated'"
        );
    }

    #[test]
    fn an_install_that_left_out_what_a_second_one_adds_is_outdated_however_current_its_script_is() {
        let missing = Status {
            incomplete: Some(Incomplete::Reinstall("cli.json has no entry.".to_string())),
            ..installed("opencode", Some(3), Some(3))
        };
        assert!(missing.outdated());
        assert_eq!(missing.note().as_deref(), Some("cli.json has no entry."));
        let neither = installed("opencode", Some(3), Some(3));
        assert!(!neither.outdated());
        assert_eq!(neither.note(), None);
    }

    #[test]
    fn what_waits_for_the_agent_is_noted_and_is_not_outdated() {
        // Nothing a second install does would change it until OpenCode 2 has made its `cli.json`,
        // so `--outdated-only` has no use for it, and the note still says what it waits for.
        let waiting = Status {
            incomplete: Some(Incomplete::Waiting("Start OpenCode 2 once.".to_string())),
            ..installed("opencode", Some(3), Some(3))
        };
        assert!(!waiting.outdated());
        assert_eq!(waiting.note().as_deref(), Some("Start OpenCode 2 once."));
        // Behind as well, it is outdated for that, and the note says both.
        let behind_and_waiting = Status {
            incomplete: Some(Incomplete::Waiting("Start OpenCode 2 once.".to_string())),
            ..installed("opencode", Some(2), Some(3))
        };
        assert!(behind_and_waiting.outdated());
        assert!(
            behind_and_waiting
                .note()
                .unwrap()
                .ends_with("to update it. Start OpenCode 2 once.")
        );
        // Not installed, there is nothing to wait for.
        let absent = Status {
            installed: false,
            incomplete: Some(Incomplete::Waiting("Start OpenCode 2 once.".to_string())),
            ..installed("opencode", None, Some(3))
        };
        assert!(!absent.outdated());
        assert_eq!(absent.note(), None);
    }

    #[test]
    fn an_installed_codex_says_that_codex_asks_to_review_taws_hook() {
        let codex = installed("codex", Some(1), Some(1));
        assert_eq!(
            codex.note().as_deref(),
            Some(integration::CODEX_HOOK_REVIEW)
        );
        assert!(!codex.outdated());
        let behind = installed("codex", Some(1), Some(2)).note().unwrap();
        assert!(
            behind.ends_with(&format!("to update it. {}", integration::CODEX_HOOK_REVIEW)),
            "{behind}"
        );
        assert_eq!(installed("claude", Some(1), Some(1)).note(), None);
        let not_installed = Status {
            installed: false,
            ..installed("codex", None, Some(1))
        };
        assert_eq!(not_installed.note(), None);
    }

    #[test]
    fn the_note_says_what_is_behind_and_what_to_run() {
        let behind = installed("opencode", Some(2), Some(3));
        assert_eq!(
            behind.note().as_deref(),
            Some(
                "The installed script is version 2 and this build writes version 3. Run `taw \
                 integration install opencode` again to update it."
            )
        );
        assert_eq!(
            installed("kimi", None, Some(1)).note().as_deref(),
            Some(
                "The installed script has no version marker. Run `taw integration install \
                 kimi` again to update it."
            )
        );
        let both = Status {
            incomplete: Some(Incomplete::Reinstall("cli.json has no entry.".to_string())),
            ..behind
        };
        assert!(
            both.note()
                .unwrap()
                .ends_with("to update it. cli.json has no entry."),
            "{:?}",
            both.note()
        );
    }

    #[test]
    fn every_integration_knows_the_version_this_build_writes() {
        // Read from the script's own template, so it cannot drift from what an install writes.
        for status in integration::status_all() {
            assert!(
                status.current.is_some(),
                "{} has no version marker in its template",
                status.agent
            );
        }
    }
}
