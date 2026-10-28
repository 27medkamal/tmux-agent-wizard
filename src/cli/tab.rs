//! `taw tab …` — herdr's tab verbs. A herdr tab **is** a tmux window:
//! its label is the window name, its number `#{window_index}`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common::{self, parse_kv};
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::context;
use crate::tmux::ids;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List tabs
    List {
        #[arg(long, value_name = "WORKSPACE_ID")]
        workspace: Option<String>,
    },
    /// Create a tab
    Create {
        #[arg(long, value_name = "WORKSPACE_ID")]
        workspace: Option<String>,
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
    /// Show a tab
    Get { tab_id: String },
    /// Focus a tab
    Focus { tab_id: String },
    /// Rename a tab
    Rename {
        tab_id: String,
        #[arg(required = true, value_name = "LABEL")]
        label: Vec<String>,
    },
    /// Close a tab
    Close { tab_id: String },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List { workspace } => ("list", list(ctx, workspace.as_deref())),
        Cmd::Create {
            workspace,
            cwd,
            label,
            env,
            focus,
            no_focus,
        } => (
            "create",
            create(
                ctx,
                workspace.as_deref(),
                cwd.as_deref(),
                label.as_deref(),
                &env,
                common::wants_focus(focus, no_focus),
            ),
        ),
        Cmd::Get { tab_id } => ("get", get(ctx, &tab_id)),
        Cmd::Focus { tab_id } => ("focus", focus(ctx, &tab_id)),
        Cmd::Rename { tab_id, label } => ("rename", rename(ctx, &tab_id, &label.join(" "))),
        Cmd::Close { tab_id } => ("close", close(ctx, &tab_id)),
    }
}

fn list(ctx: &Ctx, workspace: Option<&str>) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let filter = match workspace {
        Some(w) => Some(common::resolve_session(&ctx.tmux, w)?),
        None => None,
    };
    let tabs: Vec<Value> = common::list_windows(&ctx.tmux)?
        .iter()
        .filter(|w| filter.as_deref().is_none_or(|s| w.session_id == s))
        .map(|w| context::tab_json(w, &panes))
        .collect();
    Ok(json!({ "tabs": tabs }))
}

/// `new-window`, reporting the new tab and its root pane — herdr's `tab create` reply
/// shape (`.result.tab`, `.result.root_pane`).
fn create(
    ctx: &Ctx,
    workspace: Option<&str>,
    cwd: Option<&str>,
    label: Option<&str>,
    env: &[String],
    focus: bool,
) -> anyhow::Result<Value> {
    let mut args: Vec<String> = vec!["new-window".into()];
    if let Some(workspace) = workspace {
        let session_id = common::resolve_session(&ctx.tmux, workspace)?;
        args.push("-t".into());
        args.push(session_id);
    }
    if let Some(label) = label {
        args.push("-n".into());
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
    if !focus {
        args.push("-d".into());
    }
    args.push("-P".into());
    args.push("-F".into());
    args.push("#{window_id}".into());

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let window_id = ctx
        .tmux
        .run(&borrowed)
        .map_err(|e| CliError::tmux(format!("creating tab: {e:#}")))?;
    let window_id = window_id.trim().to_string();

    // What the caller is told is what the `tab.created` event carries, when one is built: the new
    // window's name, which tmux's automatic-rename changes as its shell starts, read once for both.
    if let Some(payload) =
        crate::cli::events::created_payload(ctx, crate::cli::events::Created::Tab, &window_id)
    {
        return Ok(json!({
            "tab": payload["tab"].clone(),
            "root_pane": payload["pane"].clone(),
        }));
    }

    let panes = common::list_panes(&ctx.tmux)?;
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == window_id)
        .ok_or_else(|| CliError::not_found(format!("tab '{window_id}' vanished after creation")))?;
    let root_pane = panes.iter().find(|p| p.window_id == window_id);

    let tab = context::tab_json(&window, &panes);
    let root_pane = root_pane.map(context::pane_json);

    // `tab.created` is deliberately **not** emitted here. `new-window` fires tmux's
    // `window-linked` in this same server, and the hook installed on it builds the same
    // payload from the same records — so the event has one source and covers `prefix c`
    // too. Emitting here as well would fire it twice for one new tab. See
    // `crate::cli::events`.
    Ok(json!({
        "tab": tab,
        "root_pane": root_pane,
    }))
}

fn get(ctx: &Ctx, tab_id: &str) -> anyhow::Result<Value> {
    let window = common::window_info(&ctx.tmux, tab_id)?;
    let panes = common::list_panes(&ctx.tmux)?;
    Ok(json!({ "tab": context::tab_json(&window, &panes) }))
}

fn focus(ctx: &Ctx, tab_id: &str) -> anyhow::Result<Value> {
    let window = common::window_info(&ctx.tmux, tab_id)?;
    // Switch the client to the owning session first, or selecting a window in another
    // session would have no visible effect.
    let _ = ctx
        .tmux
        .run_ok(&["switch-client", "-t", &window.session_id]);
    ctx.tmux
        .run_ok(&["select-window", "-t", &window.window_id])
        .map_err(|e| CliError::tmux(format!("focusing tab '{tab_id}': {e:#}")))?;
    Ok(json!({ "tab_id": ids::tab_id(&window.session_id, &window.window_id) }))
}

fn rename(ctx: &Ctx, tab_id: &str, label: &str) -> anyhow::Result<Value> {
    let window_id = common::resolve_window(&ctx.tmux, tab_id)?;
    ctx.tmux
        .run_ok(&[
            "rename-window",
            "-t",
            &window_id,
            "--",
            &crate::tmux::new_name(label),
        ])
        .map_err(|e| CliError::tmux(format!("renaming tab '{tab_id}': {e:#}")))?;
    get(ctx, &window_id)
}

fn close(ctx: &Ctx, tab_id: &str) -> anyhow::Result<Value> {
    let window = common::window_info(&ctx.tmux, tab_id)?;
    let public = ids::tab_id(&window.session_id, &window.window_id);
    common::pre_close(ctx, crate::hooks::Closing::Window(&window.window_id))?;
    ctx.tmux
        .run_ok(&["kill-window", "-t", &window.window_id])
        .map_err(|e| CliError::tmux(format!("closing tab '{tab_id}': {e:#}")))?;
    Ok(json!({ "tab_id": public }))
}
