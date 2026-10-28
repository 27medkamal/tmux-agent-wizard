//! `taw worktree …` — herdr's git-worktree workspaces, over `session::worktree`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::session::worktree::{self, WorktreeRecord};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List worktree workspaces
    List {
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
    },
    /// Create and open a Git worktree
    Create {
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        /// A folder inside the repository: its session is used, or made at the repository's root.
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        #[arg(long, value_name = "NAME")]
        branch: String,
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Accepted for herdr parity; the checkout directory comes from
        /// `[worktrees] directory` in `config.toml`.
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Accepted for herdr parity; the tmux session is named `<repo>--<branch>`.
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        #[arg(long)]
        focus: bool,
        #[arg(long = "no-focus")]
        no_focus: bool,
        /// Trust the repository's ownership for this git invocation (`-c
        /// safe.directory=<repo root>`), matching herdr's `--trust-repository`.
        #[arg(long = "trust-repository")]
        trust_repository: bool,
    },
    /// Open an existing Git worktree
    Open {
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        #[arg(long, value_name = "NAME")]
        branch: Option<String>,
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        #[arg(long)]
        focus: bool,
        #[arg(long = "no-focus")]
        no_focus: bool,
        /// Accepted for herdr parity; taw's `open` makes no git calls to apply it to.
        #[arg(long = "trust-repository")]
        trust_repository: bool,
    },
    /// Remove a worktree checkout
    Remove {
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long)]
        force: bool,
        /// Trust the repository's ownership for this git invocation (`-c
        /// safe.directory=<repo root>`), matching herdr's `--trust-repository`.
        #[arg(long = "trust-repository")]
        trust_repository: bool,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List { workspace, .. } => ("list", list(ctx, workspace.as_deref())),
        Cmd::Create {
            workspace,
            cwd,
            branch,
            base,
            focus,
            no_focus,
            trust_repository,
            ..
        } => (
            "create",
            create(
                ctx,
                workspace.as_deref(),
                cwd.as_deref(),
                &branch,
                base.as_deref(),
                common::wants_focus(focus, no_focus),
                trust_repository,
            ),
        ),
        Cmd::Open {
            workspace,
            branch,
            path,
            focus,
            no_focus,
            trust_repository,
            ..
        } => (
            "open",
            open(
                ctx,
                workspace
                    .as_deref()
                    .or(branch.as_deref())
                    .or(path.as_deref()),
                common::wants_focus(focus, no_focus),
                trust_repository,
            ),
        ),
        Cmd::Remove {
            workspace,
            force,
            trust_repository,
        } => (
            "remove",
            remove(ctx, workspace.as_deref(), force, trust_repository),
        ),
    }
}

/// The worktree record: `{workspace_id, path, branch, base, created_unix_ms}`,
/// plus the tmux session name — the handle `open`/`remove` also accept, and the one a
/// user sees in their session list.
fn record_json(record: &WorktreeRecord) -> Value {
    json!({
        "workspace_id": record.workspace_id,
        "session_name": record.session_name,
        "path": record.path,
        "branch": record.branch,
        "base": record.base,
        "created_unix_ms": record.created_unix_ms,
    })
}

fn list(ctx: &Ctx, workspace: Option<&str>) -> anyhow::Result<Value> {
    let records = worktree::list(&ctx.tmux, &ctx.paths)?;
    let worktrees: Vec<Value> = records
        .iter()
        .filter(|r| {
            workspace.is_none_or(|w| r.workspace_id == w || r.session_name == w || r.branch == w)
        })
        .map(record_json)
        .collect();
    Ok(json!({ "worktrees": worktrees }))
}

/// The workspace a worktree is cut from: the one named, else the caller's own.
fn source_workspace(ctx: &Ctx, workspace: Option<&str>) -> anyhow::Result<String> {
    match workspace {
        Some(w) => Ok(w.to_string()),
        None => {
            let pane = common::current_pane()?;
            let info = common::pane_info(&ctx.tmux, &pane)?;
            Ok(info.session_id)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    ctx: &Ctx,
    workspace: Option<&str>,
    cwd: Option<&str>,
    branch: &str,
    base: Option<&str>,
    focus: bool,
    trust_repository: bool,
) -> anyhow::Result<Value> {
    if branch.is_empty() {
        return Err(CliError::invalid_argument("--branch must not be empty"));
    }
    // `--cwd` is a folder inside the repository (herdr's), whose session is found or made;
    // `--workspace` names a workspace and wins.
    let workspace = match (workspace, cwd) {
        (None, Some(cwd)) => worktree::session_of_repository(&ctx.tmux, cwd, trust_repository)?,
        _ => source_workspace(ctx, workspace)?,
    };
    let record = worktree::create(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        &ctx.taw_bin,
        &workspace,
        branch,
        base,
        focus,
        trust_repository,
    )?;
    Ok(json!({ "worktree": record_json(&record) }))
}

fn open(
    ctx: &Ctx,
    target: Option<&str>,
    focus: bool,
    trust_repository: bool,
) -> anyhow::Result<Value> {
    let target = target.ok_or_else(|| {
        CliError::invalid_argument("worktree open needs --workspace, --branch or --path")
    })?;
    let (record, already_open) = worktree::open(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        &ctx.taw_bin,
        target,
        focus,
        trust_repository,
    )?;
    // The record keeps the id the session had when the worktree was created; a session made by this open has
    // another. The running session is found by its name, and its own id is said beside the record's.
    // A tmux error is left as no id (the near side then attaches nothing, which fails closed), and is written at
    // debug level so that it leaves a trace.
    let session_id = ctx
        .tmux
        .list_sessions()
        .inspect_err(|err| {
            log::debug!(
                "could not look up the session of {}: {err:#}",
                record.session_name
            )
        })
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|s| s.session_name == record.session_name)
                .map(|s| s.session_id)
        });
    Ok(json!({
        "worktree": record_json(&record),
        "already_open": already_open,
        "session_id": session_id,
    }))
}

fn remove(
    ctx: &Ctx,
    workspace: Option<&str>,
    force: bool,
    trust_repository: bool,
) -> anyhow::Result<Value> {
    let target =
        workspace.ok_or_else(|| CliError::invalid_argument("worktree remove needs --workspace"))?;
    let record = worktree::remove(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        &ctx.taw_bin,
        target,
        force,
        trust_repository,
    )?;
    Ok(json!({ "worktree": record_json(&record) }))
}
