// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The JSON envelope every `taw` command prints, and the error-code vocabulary it
//! reports failures with:
//!
//! ```json
//! {"id":"cli:pane:list","result":{"type":"pane_list","panes":[…]}}
//! {"id":"cli:pane:get","error":{"code":"not_found","message":"pane w9:p9 not found"}}
//! ```

use serde_json::{Value, json};

/// Every error code the CLI is allowed to report. Kept as an explicit list so a new
/// code cannot be introduced by accident — plugins and herdr's agent skill switch on
/// these strings. Order is the order of the documented list.
pub const CODES: &[&str] = &[
    "not_found",
    "invalid_argument",
    "unsupported",
    "tmux_error",
    "timeout",
    "config_dir_missing",
    "trust_required",
    "plugin_build_failed",
    "unsupported_plugin_version",
    "checkout_unreadable",
    "not_a_git_repo",
    "session_exists",
    "workspace_group_close_required",
    "worktree_dirty",
    "agent_explain_file_read_failed",
    "hook_refused",
    "shell_not_ready",
    "agent_not_started",
    "agent_prompt_stalled",
    "agent_not_running",
    "agent_not_found",
    "agent_not_ready",
    "invalid_resume_argv",
    "resume_not_accepted",
    "machine_unreachable",
    "internal",
];

/// A verb asked for something it will not do unless it is told again, so it refuses: on stderr,
/// as clap's own usage errors are, with status 2, and no envelope on stdout. It is about what
/// was asked and not about anything that failed, so it has no error code and is raised before
/// anything is looked up or run. The message is what is printed, whole.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct UsageRefusal(pub String);

/// An error carrying one of [`CODES`] explicitly, rather than leaving [`code_for`] to
/// infer one. Raised by CLI code itself (a bad flag combination, a target that does not
/// resolve, a poll that ran out of time); library errors are classified by [`code_for`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
}

impl CliError {
    /// Builds a `CliError` already boxed as an `anyhow::Error`, which is what every
    /// caller wants: these are constructed only to be returned from a verb.
    fn boxed(code: &'static str, message: impl Into<String>) -> anyhow::Error {
        debug_assert!(CODES.contains(&code), "undeclared error code {code:?}");
        anyhow::Error::new(CliError {
            code,
            message: message.into(),
        })
    }

    /// The target named does not exist (a pane/tab/workspace id, an agent name, a
    /// snapshot, a plugin, a manifest id).
    pub fn not_found(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("not_found", message)
    }

    /// ssh did not reach the machine, or the machine has no taw to answer.
    pub fn machine_unreachable(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("machine_unreachable", message)
    }

    /// Something taw itself could not do, with no better code.
    pub fn internal(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("internal", message)
    }

    /// An error with `code`, one of [`CODES`]: a code another machine's taw answered with.
    pub fn with_code(code: &'static str, message: impl Into<String>) -> anyhow::Error {
        Self::boxed(code, message)
    }

    /// The arguments parsed but do not make sense together, or a value is out of range.
    pub fn invalid_argument(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("invalid_argument", message)
    }

    /// A herdr verb or flag accepted for parity that tmux cannot express.
    pub fn unsupported(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("unsupported", message)
    }

    /// A poll (`agent wait`, `agent prompt --wait`, `agent start`, `pane wait-output`)
    /// ran out of time.
    pub fn timeout(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("timeout", message)
    }

    /// A `tmux` invocation this command issued directly failed.
    pub fn tmux(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("tmux_error", message)
    }

    /// `agent start` found the pane's shell not at its prompt, and still not at it when its wait
    /// ran out. What is typed at a shell that is still starting goes to whatever reads the
    /// terminal first (a question the shell's own startup asks, say), so nothing was typed.
    pub fn shell_not_ready(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("shell_not_ready", message)
    }

    /// `agent start` typed the agent's command and the agent did not start: the pane's shell is
    /// back at its prompt with nothing running in it (the command was not found, or the agent
    /// exited at once). Not `timeout`, which is the budget running out with the agent possibly
    /// still coming up.
    pub fn agent_not_started(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_not_started", message)
    }

    // Follows herdr's src/api/wait.rs (`agent_wait_timeout`). Differences:
    // none to the code string; the message is taw's own.
    /// `agent prompt --wait` sent the prompt and did not see the agent working or blocked within 5
    /// seconds. Not `timeout`, which is `--timeout` running out first. The prompt may well have been
    /// delivered, so a caller reads the agent before sending it again.
    pub fn agent_prompt_stalled(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_prompt_stalled", message)
    }

    // Follows herdr's src/api/wait.rs (`agent_wait_not_running`, which
    // `wait_for_resolved_agent` ends a wait with when the agent leaves its pane). Differences:
    // none to the code string; the message is taw's own.
    /// A wait (`agent wait`, `agent prompt --wait`) was for an agent that is no longer in its pane:
    /// it exited and its record was taken off, or the pane was closed. Not `timeout`, which is the
    /// budget running out with the agent still there, and not worth waiting out for a state that a
    /// pane with no agent never has.
    pub fn agent_not_running(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_not_running", message)
    }

    // Follows herdr's src/app/agents.rs (`agent_target_error_body`, and
    // `agent_not_found` in src/app/api/agents.rs): the same code string and message.
    /// `agent prompt` or `agent send-keys` was given a target that resolves to no pane: herdr's own
    /// code, which it answers before it sends anything. The other agent verbs say `not_found` for it.
    pub fn agent_not_found(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_not_found", message)
    }

    // Follows herdr's src/app/api/agents.rs (`agent_not_ready`, and `queue_agent_prompt` and
    // `handle_agent_send_keys`, which answer it before they send anything): the same code string
    // and messages.
    /// `agent prompt` or `agent send-keys` was given the pane of an agent that is not there to be
    /// typed into: the pane hosts no agent, or its agent has gone from it (herdr:
    /// `runtime_hosts_agent` is false, and the foreground process is no longer the agent). What is
    /// typed would land at the shell's prompt, and a shell may run it as a command, so nothing is.
    pub fn agent_not_ready(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_not_ready", message)
    }

    // Follows herdr's src/app/api/panes.rs (`invalid_resume_argv`, which the
    // report requests answer before they apply anything): the same code string, with taw's own
    // messages.
    /// `pane report-agent` or `report-agent-session` carried a resume command after `--` that cannot be
    /// kept: empty, not starting with a plain command name, over 64 words or 8 KiB, or holding a control
    /// character or an apostrophe. Nothing of the report is applied.
    pub fn invalid_resume_argv(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("invalid_resume_argv", message)
    }

    // Follows herdr's src/app/api/panes.rs (`resume_not_accepted`): the same code string, with
    // taw's own message.
    /// `pane report-agent-session` carried a resume command from a reporter that does not hold the
    /// pane: the rest of its report is applied, and the command is not kept. A reporter holds the pane
    /// once it has reported the agent's state with `pane report-agent`, or, while no hook holds it, when
    /// the agent in front of the pane is the one it reports for, by its process.
    pub fn resume_not_accepted(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("resume_not_accepted", message)
    }

    // Follows herdr's src/app/api/workspaces.rs: the same code string.
    /// `workspace close` on a workspace whose worktree workspaces are still open, without
    /// `--group`. Follows herdr's own code of the same name (#2874), which a
    /// plugin or herdr's agent skill may switch on: the workspace and its worktrees are
    /// only closed together on an explicit request.
    pub fn workspace_group_close_required(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("workspace_group_close_required", message)
    }

    /// `agent explain --file <path>` could not read `<path>`. Follows herdr's own
    /// code of the same name (`<herdr-src>/src/cli/agent.rs`) rather than the generic
    /// `not_found`, since a plugin or herdr's agent skill may switch on it specifically.
    pub fn agent_explain_file_read_failed(message: impl Into<String>) -> anyhow::Error {
        Self::boxed("agent_explain_file_read_failed", message)
    }
}

/// `{"id":<id>,"result":{"type":<type>,…}}`. `result` is `type` merged into `fields`;
/// a non-object `fields` is wrapped as `{"type":…,"value":fields}` so the envelope shape
/// never varies.
pub fn success(id: &str, result_type: &str, fields: Value) -> Value {
    let mut result = match fields {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    result.insert("type".to_string(), Value::String(result_type.to_string()));
    json!({ "id": id, "result": Value::Object(result) })
}

/// `{"id":<id>,"error":{"code":…,"message":…}}`.
pub fn failure(id: &str, code: &str, message: &str) -> Value {
    json!({ "id": id, "error": { "code": code, "message": message } })
}

/// Classifies an [`anyhow::Error`] into one of [`CODES`].
///
/// The whole chain is searched, head first, so a code survives an `anyhow` `.context()`
/// wrapper: a [`CliError`] states its own code, and the library error enums that already
/// carry a code in their name (`plugin::install::InstallError`,
/// `plugin::actions::ActionError`, `plugin::panes::PaneError`,
/// `session::worktree::WorktreeError`, `integration::IntegrationError`) map to theirs.
/// Failing that, an error whose chain looks like a failed `tmux` invocation (the shape
/// `Tmux::run` fails with, [`crate::tmux::CommandFailed`]) is `tmux_error`, and anything
/// left is `internal`.
pub fn code_for(err: &anyhow::Error) -> &'static str {
    use crate::integration::IntegrationError;
    use crate::plugin::{actions::ActionError, install::InstallError, panes::PaneError};
    use crate::session::create::HookFailure;
    use crate::session::snapshot::SnapshotError;
    use crate::session::worktree::WorktreeError;

    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<CliError>() {
            return e.code;
        }
        // Only a pre-create hook's failure ever reaches a caller: a post-create hook's is
        // logged, and the session it ran after stays.
        if cause.downcast_ref::<HookFailure>().is_some() {
            return "hook_refused";
        }
        if let Some(e) = cause.downcast_ref::<InstallError>() {
            return match e {
                InstallError::TrustRequired => "trust_required",
                InstallError::BuildFailed { .. } => "plugin_build_failed",
                InstallError::UnsupportedPluginVersion { .. } => "unsupported_plugin_version",
                InstallError::CheckoutUnreadable(_) => "checkout_unreadable",
                InstallError::LocalPluginConflict(_) => "invalid_argument",
                InstallError::InvalidSpec { .. } => "invalid_argument",
            };
        }
        if cause.downcast_ref::<ActionError>().is_some()
            || cause.downcast_ref::<PaneError>().is_some()
        {
            return "not_found";
        }
        if let Some(e) = cause.downcast_ref::<WorktreeError>() {
            return match e {
                WorktreeError::NotAGitRepo(_) => "not_a_git_repo",
                WorktreeError::SessionExists(_) => "session_exists",
                WorktreeError::NotFound(_) | WorktreeError::CheckoutGone { .. } => "not_found",
                WorktreeError::InvalidBranch(_) => "invalid_argument",
                // git refused a removal that `--force` would have done (changes in the checkout,
                // submodules, or a leftover folder): the caller may ask and run it again forced.
                WorktreeError::RemoveRefused { .. } => "worktree_dirty",
            };
        }
        if let Some(SnapshotError::NewerFormat { .. }) = cause.downcast_ref::<SnapshotError>() {
            return "unsupported";
        }
        if let Some(e) = cause.downcast_ref::<crate::sandbox::start::StartError>() {
            return match e {
                crate::sandbox::start::StartError::NoSuchAgent(_)
                | crate::sandbox::start::StartError::NoSuchPane(_) => "not_found",
                crate::sandbox::start::StartError::Profile(missing) => {
                    missing_profile_code(missing)
                }
                crate::sandbox::start::StartError::Config(_)
                | crate::sandbox::start::StartError::NotAPane(_)
                | crate::sandbox::start::StartError::NotAtThePrompt { .. }
                | crate::sandbox::start::StartError::StateNotAFullPath
                | crate::sandbox::start::StartError::NoReportFolder { .. } => "invalid_argument",
            };
        }
        if let Some(missing) = cause.downcast_ref::<crate::sandbox::profile::Missing>() {
            return missing_profile_code(missing);
        }
        if let Some(e) = cause.downcast_ref::<IntegrationError>() {
            return match e {
                IntegrationError::ConfigDirMissing { .. } => "config_dir_missing",
                IntegrationError::UnusableHooksFile { .. } => "invalid_argument",
                IntegrationError::UnusableConfig { .. } => "invalid_argument",
                IntegrationError::UnmatchedHook { .. } => "invalid_argument",
                IntegrationError::ConfigReadOnly { .. } => "invalid_argument",
            };
        }
    }
    if looks_like_tmux_failure(err) {
        return "tmux_error";
    }
    "internal"
}

/// A profile that is not there is `not_found`; a name that cannot be one, or a file that cannot
/// run, is `invalid_argument`.
fn missing_profile_code(missing: &crate::sandbox::profile::Missing) -> &'static str {
    match missing {
        crate::sandbox::profile::Missing::NoSuch { .. } => "not_found",
        _ => "invalid_argument",
    }
}

/// Whether any error in the chain is the message `Tmux::run` fails with for a non-zero
/// `tmux` exit: `` `<bin> <args>` failed (<status>): <stderr> ``. This is a heuristic on
/// text, and not a check for the [`crate::tmux::CommandFailed`] itself, because callers
/// also spell that failure into messages of their own (`CliError::tmux(format!(..))`),
/// which have to count too; it only ever downgrades the code from `internal` to the more
/// specific `tmux_error`, so a miss is harmless.
fn looks_like_tmux_failure(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains("` failed ("))
}

/// Renders `value` for stdout: one line, or indented when `pretty`.
pub fn render(value: &Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_merges_the_type_into_the_result_object() {
        let env = success("cli:pane:list", "pane_list", json!({"panes": []}));
        assert_eq!(env["id"], "cli:pane:list");
        assert_eq!(env["result"]["type"], "pane_list");
        assert!(env["result"]["panes"].as_array().unwrap().is_empty());
        assert!(env.get("error").is_none());
    }

    #[test]
    fn success_wraps_a_non_object_payload_rather_than_changing_shape() {
        let env = success("cli:status", "status", json!("text"));
        assert_eq!(env["result"]["type"], "status");
        assert_eq!(env["result"]["value"], "text");
    }

    #[test]
    fn failure_carries_the_id_code_and_message() {
        let env = failure("cli:pane:get", "not_found", "pane w9:p9 not found");
        assert_eq!(env["id"], "cli:pane:get");
        assert_eq!(env["error"]["code"], "not_found");
        assert_eq!(env["error"]["message"], "pane w9:p9 not found");
        assert!(env.get("result").is_none());
    }

    #[test]
    fn cli_errors_report_their_own_code() {
        for (err, code) in [
            (CliError::not_found("x"), "not_found"),
            (CliError::invalid_argument("x"), "invalid_argument"),
            (CliError::unsupported("x"), "unsupported"),
            (CliError::timeout("x"), "timeout"),
            (CliError::tmux("x"), "tmux_error"),
            (CliError::shell_not_ready("x"), "shell_not_ready"),
            (CliError::agent_not_started("x"), "agent_not_started"),
            (CliError::agent_prompt_stalled("x"), "agent_prompt_stalled"),
            (CliError::invalid_resume_argv("x"), "invalid_resume_argv"),
            (CliError::resume_not_accepted("x"), "resume_not_accepted"),
        ] {
            assert_eq!(code_for(&err), code);
        }
    }

    #[test]
    fn an_uninstall_that_cannot_match_a_hook_reports_the_code_a_bad_file_has() {
        use crate::integration::IntegrationError;
        let err = anyhow::Error::new(IntegrationError::UnmatchedHook {
            path: "/home/u/.claude/settings.json".into(),
            spelled: "/gone/hooks/taw-agent-state.sh".into(),
            script: "/home/u/.claude/hooks/taw-agent-state.sh".into(),
        });
        assert_eq!(code_for(&err), "invalid_argument");
        // The CLI prints this message alone: it names the file and the path.
        let message = err.to_string();
        assert!(
            message.contains("/home/u/.claude/settings.json"),
            "{message}"
        );
        assert!(
            message.contains("/gone/hooks/taw-agent-state.sh"),
            "{message}"
        );
        assert!(message.contains("Nothing was changed."), "{message}");
    }

    #[test]
    fn cli_error_code_survives_added_context() {
        let err = CliError::timeout("waiting for blocked").context("agent wait 'reviewer'");
        assert_eq!(code_for(&err), "timeout");
    }

    #[test]
    fn a_group_close_refusal_carries_herdrs_code() {
        let err = CliError::workspace_group_close_required("workspace w1 has worktrees open")
            .context("closing w1");
        assert_eq!(code_for(&err), "workspace_group_close_required");
        assert!(CODES.contains(&"workspace_group_close_required"));
    }

    #[test]
    fn a_snapshot_from_a_newer_taw_is_unsupported_and_says_so() {
        use crate::session::snapshot::SnapshotError;
        let err = anyhow::Error::new(SnapshotError::NewerFormat {
            name: "default".to_string(),
            found: 2,
            supported: 1,
        });
        assert_eq!(code_for(&err), "unsupported");
        assert!(err.to_string().contains("newer taw"), "{err}");
        // It survives whatever context a caller wraps around it.
        assert_eq!(code_for(&err.context("restoring")), "unsupported");
    }

    #[test]
    fn library_error_enums_map_to_their_documented_codes() {
        use crate::plugin::install::InstallError;
        use crate::session::worktree::WorktreeError;

        assert_eq!(
            code_for(&anyhow::Error::new(InstallError::TrustRequired)),
            "trust_required"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(
                InstallError::UnsupportedPluginVersion {
                    required: "1.0.0".into(),
                    compat: "0.8.2".into(),
                }
            )),
            "unsupported_plugin_version"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::NotAGitRepo(
                "/tmp".into()
            ))),
            "not_a_git_repo"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::SessionExists(
                "x".into()
            ))),
            "session_exists"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::NotFound("x".into()))),
            "not_found"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::CheckoutGone {
                branch: "feat/x".into(),
                path: "/gone/repo--feat-x".into(),
            })),
            "not_found"
        );
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::InvalidBranch(
                "must not start with '-'".into()
            ))),
            "invalid_argument"
        );
        // Wrapped in context, the chain still classifies.
        assert_eq!(
            code_for(&anyhow::Error::new(WorktreeError::NotFound("x".into())).context("removing")),
            "not_found"
        );
    }

    #[test]
    fn a_removal_git_refused_is_worktree_dirty() {
        use crate::session::worktree::{RemoveRefusal, WorktreeError};

        let err = anyhow::Error::new(WorktreeError::RemoveRefused {
            why: RemoveRefusal::Dirty,
            branch: "feat/x".to_string(),
            path: std::path::PathBuf::from("/tmp/garden--feat-x"),
            message: "contains modified or untracked files".to_string(),
        });
        assert_eq!(code_for(&err), "worktree_dirty");
        assert!(CODES.contains(&"worktree_dirty"));
        assert_eq!(code_for(&err.context("removing")), "worktree_dirty");
    }

    #[test]
    fn a_pre_create_hook_that_refused_is_hook_refused() {
        use crate::session::create::{Hook, HookFailure};
        use std::os::unix::process::ExitStatusExt;

        let err = anyhow::Error::new(HookFailure::Exited {
            hook: Hook::Pre,
            command: "/opt/pre.sh".to_string(),
            status: std::process::ExitStatus::from_raw(1 << 8),
            stderr: "not today".to_string(),
        });
        assert_eq!(code_for(&err), "hook_refused");
        assert!(CODES.contains(&"hook_refused"));
        // It survives whatever context a caller wraps around it.
        assert_eq!(code_for(&err.context("opening")), "hook_refused");
    }

    #[test]
    fn a_failed_tmux_invocation_is_a_tmux_error_and_anything_else_is_internal() {
        let tmux = anyhow::anyhow!("`tmux list-panes` failed (exit status: 1): no server running");
        assert_eq!(code_for(&tmux), "tmux_error");
        assert_eq!(code_for(&anyhow::anyhow!("something else")), "internal");
    }

    #[test]
    fn every_declared_code_is_unique() {
        let mut sorted = CODES.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "duplicate error code in CODES");
    }

    /// A start `taw sandbox` refused has the code of what was wrong: an agent or a profile that is
    /// not there is `not_found`, and anything about a name or a file that cannot be one to run is
    /// `invalid_argument`, every way a profile can be missing among them.
    #[test]
    fn a_refused_sandbox_start_has_the_code_of_what_was_wrong() {
        use crate::sandbox::profile::Missing;
        use crate::sandbox::start::StartError;
        let code = |err: StartError| code_for(&anyhow::Error::new(err));
        let name = || "m".to_string();
        let path = || "~/x/m".to_string();
        assert_eq!(code(StartError::NoSuchAgent("claud".into())), "not_found");
        assert_eq!(code(StartError::NoSuchPane("%9".into())), "not_found");
        for refused in [
            StartError::NotAPane("../../x".into()),
            StartError::StateNotAFullPath,
            StartError::NotAtThePrompt {
                pane: "%3".into(),
                why: "its input is no terminal",
            },
            StartError::NoReportFolder {
                server: "..".into(),
                pane: "%3".into(),
            },
        ] {
            assert_eq!(code(refused.clone()), "invalid_argument", "{refused:?}");
        }
        assert_eq!(
            code(StartError::Config(crate::config::SandboxDoubt::Dropped {
                file: "~/.config/tmux-agent-wizard/config.toml".into(),
                line: "unknown config key: sandbox.profil".into()
            })),
            "invalid_argument"
        );
        assert_eq!(
            code(StartError::Profile(Missing::NoSuch {
                name: "t".into(),
                names: "default".into(),
                dir: "~/x/".into()
            })),
            "not_found"
        );
        for missing in [
            Missing::BadName("../x".into()),
            Missing::NotExecutable {
                name: name(),
                path: path(),
            },
            Missing::NoInterpreter {
                name: name(),
                path: path(),
                interpreter: "/no/bash".into(),
            },
            Missing::InterpreterNotAFullPath {
                name: name(),
                path: path(),
                interpreter: "bash".into(),
            },
            Missing::FolderGone {
                which: "profiles folder",
                path: path(),
            },
            Missing::NoFolder { name: name() },
            Missing::BrokenLink {
                name: name(),
                path: path(),
            },
            Missing::NotAFile {
                name: name(),
                path: path(),
            },
            Missing::CannotLook {
                name: name(),
                path: path(),
                why: "too many levels of symbolic links".into(),
            },
        ] {
            assert_eq!(
                code(StartError::Profile(missing.clone())),
                "invalid_argument",
                "{missing:?}"
            );
            // A profile's refusal found on its own, as a context of a failure, says the same.
            let wrapped = anyhow::Error::new(missing.clone()).context("starting the sandbox");
            assert_eq!(code_for(&wrapped), "invalid_argument", "{missing:?}");
        }
    }

    #[test]
    fn render_pretty_and_compact_carry_the_same_value() {
        let env = success("cli:pane:list", "pane_list", json!({"panes": [1, 2]}));
        let compact = render(&env, false);
        let pretty = render(&env, true);
        assert_eq!(compact.lines().count(), 1);
        assert!(pretty.lines().count() > 1);
        assert_eq!(
            serde_json::from_str::<Value>(&compact).unwrap(),
            serde_json::from_str::<Value>(&pretty).unwrap()
        );
    }
}
