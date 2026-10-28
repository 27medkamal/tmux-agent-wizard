//! `taw watch …` and `taw seen` — the pipe-pane watcher's control surface. Ours alone
//! (herdr's server watches terminals itself; tmux has no daemon, so a watcher process
//! per pane does the job).

use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::context;
use crate::tmux::ids;
use crate::watch::binary::{self, Binary};
use crate::watch::{self, Timing};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Attach a watcher to one pane
    Attach {
        #[arg(value_name = "PANE")]
        pane: String,
    },
    /// Attach a watcher to every pane that has none
    AttachAll {
        /// Also start again every watcher that runs an older taw than the one installed (the
        /// ones `taw watch status` marks old), each from the taw it started from
        #[arg(long)]
        restart: bool,
    },
    /// Detach the watcher from one pane
    Detach {
        #[arg(value_name = "PANE")]
        pane: String,
    },
    /// The watcher body: reads the pane's piped output on stdin (run by `pipe-pane`)
    Run {
        #[arg(value_name = "PANE")]
        pane: String,
    },
    /// Which panes currently have a live watcher, and which of those run an older taw than
    /// the one installed
    Status,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Attach { pane } => ("attach", attach(ctx, &pane)),
        Cmd::AttachAll { restart } => ("attach-all", attach_all(ctx, restart)),
        Cmd::Detach { pane } => ("detach", detach(ctx, &pane)),
        Cmd::Run { pane } => ("run", run_watcher(ctx, &pane)),
        Cmd::Status => ("status", status(ctx)),
    }
}

/// `attach` reports `attached: false` when `@agent-wizard-watch` is `off` — a user
/// opt-out, not a failure, so the tmux hooks that call this on every new pane stay quiet.
fn attach(ctx: &Ctx, pane: &str) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    let attached = watch::attach(&ctx.tmux, &pane_id, &ctx.taw_bin)?;
    Ok(json!({
        "terminal_id": pane_id,
        "attached": attached,
    }))
}

/// `attached` is how many panes got a watcher they had none of. With `--restart`, `restarted`
/// is how many watchers that ran an older taw were started again, which `attached` does not
/// count, and `failed` is how many were let go of and could not be started, which leaves their
/// panes with none, with `note` saying so in words (`null` when none failed). A plain
/// `attach-all`, the plugin's own call at startup, reports what it always has.
fn attach_all(ctx: &Ctx, restart: bool) -> anyhow::Result<Value> {
    let attached = watch::attach_all(&ctx.tmux, &ctx.taw_bin)?;
    if !restart {
        return Ok(json!({ "attached": attached }));
    }
    let restart = watch::restart_old(&ctx.tmux, &ctx.taw_bin)?;
    Ok(json!({
        "attached": attached,
        "restarted": restart.restarted,
        "failed": restart.failed,
        "note": binary::failed_sentence(restart.failed),
    }))
}

fn detach(ctx: &Ctx, pane: &str) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    watch::detach(&ctx.tmux, &pane_id)?;
    Ok(json!({ "terminal_id": pane_id }))
}

/// The watcher loop itself. Blocks until the pane closes or the pipe is removed, so its
/// envelope is printed only at the very end — tmux discards it either way.
///
/// Logging is set up by [`crate::cli::main`], which skips its own stderr logger for this
/// one verb and calls [`crate::watch::init_logging`] instead: `pipe-pane` inherits
/// whatever stderr tmux had, so a stderr log line here could land in a user's terminal.
fn run_watcher(ctx: &Ctx, pane: &str) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    watch::run(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        &ctx.taw_bin,
        &pane_id,
        Timing::default(),
    )?;
    Ok(json!({ "terminal_id": pane_id }))
}

/// One row per pane on the server, not only the watched ones: `piped` and
/// `watcher_alive` can disagree, and seeing *which* is what makes a stuck pane
/// diagnosable (piped with a dead pid means a watcher died; a live pid with no pipe means
/// one was detached).
///
/// A live watcher's row also says whether its taw is the one installed: `binary` is `current`
/// or `old` (`null` when no watcher is alive), and an old one says why in `binary_note`. `old`
/// counts the ones a restart would start (`WatchStatus::restartable`), and `restart` says what
/// to run when there are any, or that watching is off when it is (`null` when there are none).
fn status(ctx: &Ctx) -> anyhow::Result<Value> {
    let statuses = watch::status(&ctx.tmux)?;
    let old = statuses.iter().filter(|w| w.restartable()).count();
    let panes: Vec<Value> = statuses
        .iter()
        .map(|w| {
            json!({
                "pane_id": ids::pane_id(&w.pane.session_id, &w.pane.pane_id),
                "terminal_id": w.pane.pane_id,
                "piped": w.piped,
                "watcher_alive": w.watcher_alive,
                "watch_pid": w.pane.agent.watch_pid,
                "binary": w.binary.as_ref().map(Binary::word),
                "binary_note": w.binary.as_ref().and_then(Binary::note),
                "agent": context::agent_json(&w.pane),
            })
        })
        .collect();
    // Whether watching is on matters only to what is said of an old watcher, so it is read then.
    let restart = if old == 0 {
        None
    } else {
        binary::restart_sentence(old, watch::watching_enabled(&ctx.tmux))
    };
    Ok(json!({
        "watched": panes.iter().filter(|p| p["piped"] == true).count(),
        "old": old,
        "restart": restart,
        "panes": panes,
    }))
}

/// `taw seen <pane>` — records that a pane has been looked at, which is what
/// distinguishes `done` (finished but unseen) from `idle`. Bound to
/// `after-select-pane`/`after-select-window`/`pane-focus-in` by the plugin script.
pub(crate) fn seen(ctx: &Ctx, pane: &str) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)
        .map_err(|_| CliError::not_found(format!("pane '{pane}' not found")))?;
    watch::mark_seen(&ctx.tmux, &ctx.cfg, &ctx.paths, &pane_id)?;
    Ok(json!({ "terminal_id": pane_id }))
}
