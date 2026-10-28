// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! `taw workspace …` — herdr's workspace verbs. A herdr workspace **is** a tmux session:
//! its label is the session name, its cwd `#{session_path}`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common::{self, parse_kv};
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::context;
use crate::session::worktree;
use crate::tmux::ids;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List workspaces
    List,
    /// Create a workspace
    Create {
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// Set an environment variable for the launched process
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long)]
        focus: bool,
        #[arg(long = "no-focus")]
        no_focus: bool,
    },
    /// Show a workspace
    Get { workspace_id: String },
    /// Focus a workspace
    Focus { workspace_id: String },
    /// Rename a workspace
    Rename {
        workspace_id: String,
        #[arg(required = true, value_name = "LABEL")]
        label: Vec<String>,
    },
    /// Report display-only workspace metadata
    ReportMetadata {
        workspace_id: String,
        #[arg(long, value_name = "ID")]
        source: String,
        #[arg(long, value_name = "NAME=VALUE")]
        token: Vec<String>,
        #[arg(long = "clear-token", value_name = "NAME")]
        clear_token: Vec<String>,
        /// Accepted for herdr parity; tmux applies writes in arrival order.
        #[arg(long, value_name = "N")]
        seq: Option<u64>,
        /// Accepted for herdr parity; tmux options do not expire.
        #[arg(long = "ttl-ms", value_name = "N")]
        ttl_ms: Option<u64>,
    },
    /// Close a workspace
    Close {
        workspace_id: String,
        /// Also close the worktree workspaces created from it. Required while any of them
        /// is open; the close is refused (`workspace_group_close_required`) without it.
        #[arg(long)]
        group: bool,
    },
    /// Print the current workspace id
    Id,
    /// Create a workspace from a template
    Template,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List => ("list", list(ctx)),
        Cmd::Create {
            cwd,
            label,
            env,
            focus,
            no_focus,
        } => (
            "create",
            create(
                ctx,
                cwd.as_deref(),
                label.as_deref(),
                &env,
                common::wants_focus(focus, no_focus),
            ),
        ),
        Cmd::Get { workspace_id } => ("get", get(ctx, &workspace_id)),
        Cmd::Focus { workspace_id } => ("focus", focus(ctx, &workspace_id)),
        Cmd::Rename {
            workspace_id,
            label,
        } => ("rename", rename(ctx, &workspace_id, &label.join(" "))),
        Cmd::ReportMetadata {
            workspace_id,
            token,
            clear_token,
            ..
        } => (
            "report-metadata",
            report_metadata(ctx, &workspace_id, &token, &clear_token),
        ),
        Cmd::Close {
            workspace_id,
            group,
        } => ("close", close(ctx, &workspace_id, group)),
        Cmd::Id => ("id", current_id(ctx)),
        // herdr's workspace templates describe a whole saved layout to instantiate; the
        // tmux equivalent is a session snapshot, which `taw session restore` already
        // covers, so there is nothing separate to build here.
        Cmd::Template => (
            "template",
            Err(CliError::unsupported(
                "workspace templates are not supported; use `taw session save`/`restore` \
                 for reusable layouts",
            )),
        ),
    }
}

fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let workspaces: Vec<Value> = common::list_sessions(&ctx.tmux)?
        .iter()
        .map(|s| context::workspace_json(s, &panes))
        .collect();
    Ok(json!({ "workspaces": workspaces }))
}

/// `new-session -d`, then report the new session with its first tab and root pane —
/// herdr's `workspace create` reply shape (`.result.workspace`, `.result.tab`,
/// `.result.root_pane`; see the vendored SKILL.md).
fn create(
    ctx: &Ctx,
    cwd: Option<&str>,
    label: Option<&str>,
    env: &[String],
    focus: bool,
) -> anyhow::Result<Value> {
    let mut args: Vec<String> = vec!["new-session".into(), "-d".into()];
    if let Some(label) = label {
        args.push("-s".into());
        args.push(crate::tmux::new_name(label));
    }
    if let Some(cwd) = cwd {
        args.push("-c".into());
        args.push(crate::tmux::start_directory(cwd));
    }
    for pair in env {
        parse_kv(pair)?;
        args.push("-e".into());
        args.push(pair.clone());
    }
    args.push("-P".into());
    args.push("-F".into());
    args.push("#{session_id}".into());

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let session_id = ctx
        .tmux
        .run(&borrowed)
        .map_err(|e| CliError::tmux(format!("creating workspace: {e:#}")))?;
    let session_id = session_id.trim().to_string();

    // No attached client means nothing to switch; that is not a failure.
    let switched = focus
        && ctx
            .tmux
            .run_ok(&["switch-client", "-t", &session_id])
            .is_ok();

    // What the caller is told is what the `workspace.created` event carries, when one is built: the
    // name of the session's first window, which tmux's automatic-rename changes as its shell starts,
    // read once for both. The hook can have read the session before the switch above, when no client
    // was on it yet, so whether one is on it is read again once one was moved there.
    if let Some(payload) = crate::cli::events::created_payload(
        ctx,
        crate::cli::events::Created::Workspace,
        &session_id,
    ) {
        let mut workspace = payload["workspace"].clone();
        if switched
            && let Ok(attached) = ctx.tmux.display(&session_id, "#{session_attached}")
            && let Some(focused) = workspace.get_mut("focused")
        {
            *focused = Value::Bool(attached.trim().parse::<u32>().is_ok_and(|n| n > 0));
        }
        return Ok(json!({
            "workspace": workspace,
            "tab": payload["tab"].clone(),
            "root_pane": payload["pane"].clone(),
        }));
    }

    let panes = common::list_panes(&ctx.tmux)?;
    let session = common::list_sessions(&ctx.tmux)?
        .into_iter()
        .find(|s| s.session_id == session_id)
        .ok_or_else(|| {
            CliError::not_found(format!("workspace '{session_id}' vanished after creation"))
        })?;
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.session_id == session_id);
    let root_pane = panes.iter().find(|p| p.session_id == session_id);

    let workspace = context::workspace_json(&session, &panes);
    let tab = window.as_ref().map(|w| context::tab_json(w, &panes));
    let root_pane = root_pane.map(context::pane_json);

    // `workspace.created` is deliberately **not** emitted here: `new-session` fires tmux's
    // `session-created`, whose hook builds the same payload. One source, and it covers a
    // bare `tmux new-session` too. See `crate::cli::events`.
    Ok(json!({
        "workspace": workspace,
        "tab": tab,
        "root_pane": root_pane,
    }))
}

fn get(ctx: &Ctx, workspace_id: &str) -> anyhow::Result<Value> {
    let session = common::session_info(&ctx.tmux, workspace_id)?;
    let panes = common::list_panes(&ctx.tmux)?;
    Ok(json!({ "workspace": context::workspace_json(&session, &panes) }))
}

fn focus(ctx: &Ctx, workspace_id: &str) -> anyhow::Result<Value> {
    let session_id = common::resolve_session(&ctx.tmux, workspace_id)?;
    ctx.tmux
        .run_ok(&["switch-client", "-t", &session_id])
        .map_err(|e| CliError::tmux(format!("focusing workspace '{workspace_id}': {e:#}")))?;
    Ok(json!({ "workspace_id": ids::workspace_id(&session_id) }))
}

fn rename(ctx: &Ctx, workspace_id: &str, label: &str) -> anyhow::Result<Value> {
    let session_id = common::resolve_session(&ctx.tmux, workspace_id)?;
    ctx.tmux
        .run_ok(&[
            "rename-session",
            "-t",
            &session_id,
            "--",
            &crate::tmux::new_name(label),
        ])
        .map_err(|e| CliError::tmux(format!("renaming workspace '{workspace_id}': {e:#}")))?;
    get(ctx, &session_id)
}

/// `workspace close`. A workspace with worktree workspaces still open is closed only with
/// `--group`, which closes them too — herdr v0.9.1 (#2874); without it the close is refused
/// with `workspace_group_close_required` and nothing is closed. Any other workspace closes
/// as it always did, with or without the flag.
///
/// The worktree workspaces are closed first and the workspace itself after them, and the
/// session holding the pane this `taw` is running in (from `$TMUX_PANE`, wherever in the
/// group that is) goes last of all: killing a session hangs up the processes in its panes,
/// `taw` included, so anything not yet closed by then would stay open. Each close is a
/// `kill-session`, so tmux's `session-closed` hook reports one `workspace.closed` per
/// session, exactly as for a hand `tmux kill-session`. `[hooks] pre_close` is asked about each of
/// them before any is closed, and a refusal is `hook_refused` with nothing closed.
fn close(ctx: &Ctx, workspace_id: &str, group: bool) -> anyhow::Result<Value> {
    let session = common::session_info(&ctx.tmux, workspace_id)?;
    // The worktree records only say whether a group close was asked for. A file taw cannot
    // read must not stop a workspace closing, which needs nothing from it: treat it as "no
    // worktree workspaces", as before this check existed, and say why in the log.
    let linked = worktree::linked_sessions(&ctx.tmux, &ctx.paths, &session).unwrap_or_else(|err| {
        log::warn!("workspace close: could not look up worktree workspaces: {err:#}");
        Vec::new()
    });
    if !linked.is_empty() && !group {
        let open: Vec<String> = linked
            .iter()
            .map(|l| format!("{} ({})", ids::workspace_id(&l.session_id), l.session_name))
            .collect();
        return Err(CliError::workspace_group_close_required(format!(
            "workspace '{workspace_id}' has worktree workspaces still open: {}; \
             pass --group to close them together with it",
            open.join(", ")
        )));
    }
    // (session id, how an error names it), in the order they are closed.
    let mut order: Vec<(String, String)> = linked
        .iter()
        .map(|l| {
            (
                l.session_id.clone(),
                format!("worktree workspace '{}'", l.session_name),
            )
        })
        .collect();
    order.push((
        session.session_id.clone(),
        format!("workspace '{workspace_id}'"),
    ));
    close_callers_session_last(&mut order, caller_session(ctx).as_deref());
    // Every session about to close is asked first, so one refusal keeps the whole group open:
    // a hook cannot be left having agreed to half of it.
    for (session_id, _) in &order {
        common::pre_close(ctx, crate::hooks::Closing::Session(session_id))?;
    }
    for (session_id, what) in &order {
        ctx.tmux
            .run_ok(&["kill-session", "-t", session_id])
            .map_err(|e| CliError::tmux(format!("closing {what}: {e:#}")))?;
    }
    let mut reply = json!({ "workspace_id": ids::workspace_id(&session.session_id) });
    if !linked.is_empty() {
        reply["worktree_workspace_ids"] = linked
            .iter()
            .map(|l| Value::String(ids::workspace_id(&l.session_id)))
            .collect();
    }
    Ok(reply)
}

/// The session holding `$TMUX_PANE`, if this `taw` was run from a pane (and one that still
/// exists): the session whose kill would hang it up.
fn caller_session(ctx: &Ctx) -> Option<String> {
    let pane = common::current_pane().ok()?;
    common::pane_info(&ctx.tmux, &pane)
        .ok()
        .map(|info| info.session_id)
}

/// Moves the entry for `caller`'s session to the end of `order`, keeping every other entry
/// where it was. A `caller` that is not in `order` (or none) leaves it as it is.
fn close_callers_session_last(order: &mut [(String, String)], caller: Option<&str>) {
    if let Some(caller) = caller {
        // `sort_by_key` is stable, and `false` sorts before `true`.
        order.sort_by_key(|(session_id, _)| session_id == caller);
    }
}

/// Workspace tokens live on the tmux *session* (`set-option -t <session> @agent_token_*`),
/// the same namespace pane tokens use one level down.
fn report_metadata(
    ctx: &Ctx,
    workspace_id: &str,
    tokens: &[String],
    clear_tokens: &[String],
) -> anyhow::Result<Value> {
    let session_id = common::resolve_session(&ctx.tmux, workspace_id)?;
    let mut changed = Vec::new();
    for pair in tokens {
        let (key, value) = parse_kv(pair)?;
        ctx.tmux
            .run_ok(&[
                "set-option",
                "-t",
                &session_id,
                &format!("@agent_token_{key}"),
                &value,
            ])
            .map_err(|e| CliError::tmux(format!("setting workspace token '{key}': {e:#}")))?;
        changed.push(format!("token:{key}"));
    }
    for key in clear_tokens {
        ctx.tmux
            .run_ok(&[
                "set-option",
                "-u",
                "-t",
                &session_id,
                &format!("@agent_token_{key}"),
            ])
            .map_err(|e| CliError::tmux(format!("clearing workspace token '{key}': {e:#}")))?;
        changed.push(format!("token:{key}"));
    }
    // The workspace record changed, which is what herdr's `workspace.updated` means. No
    // tmux hook covers this: tmux fires nothing for a session option write.
    if !changed.is_empty() {
        crate::cli::events::workspace_updated(ctx, &session_id);
    }
    Ok(json!({
        "workspace_id": ids::workspace_id(&session_id),
        "changed": changed,
    }))
}

/// The workspace holding `$TMUX_PANE` — herdr injects `HERDR_WORKSPACE_ID` into every
/// managed pane; in tmux the pane's own id is the only thing in the environment, so the
/// session is looked up from it.
fn current_id(ctx: &Ctx) -> anyhow::Result<Value> {
    let pane = common::current_pane()?;
    let info = common::pane_info(&ctx.tmux, &pane)?;
    Ok(json!({ "workspace_id": ids::workspace_id(&info.session_id) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter()
            .map(|id| (id.to_string(), format!("workspace {id}")))
            .collect()
    }

    fn ids(order: &[(String, String)]) -> Vec<&str> {
        order.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn the_callers_own_session_is_closed_last_and_the_rest_keep_their_order() {
        // worktree a, worktree b, then the workspace itself: run from inside worktree a,
        // a's kill must come after every other one.
        let mut o = order(&["$1", "$2", "$0"]);
        close_callers_session_last(&mut o, Some("$1"));
        assert_eq!(ids(&o), ["$2", "$0", "$1"]);
    }

    #[test]
    fn a_caller_in_the_workspace_itself_or_outside_the_group_changes_nothing() {
        let mut in_workspace = order(&["$1", "$2", "$0"]);
        close_callers_session_last(&mut in_workspace, Some("$0"));
        assert_eq!(ids(&in_workspace), ["$1", "$2", "$0"]);

        let mut elsewhere = order(&["$1", "$2", "$0"]);
        close_callers_session_last(&mut elsewhere, Some("$9"));
        assert_eq!(ids(&elsewhere), ["$1", "$2", "$0"]);

        let mut headless = order(&["$1", "$2", "$0"]);
        close_callers_session_last(&mut headless, None);
        assert_eq!(ids(&headless), ["$1", "$2", "$0"]);
    }
}
