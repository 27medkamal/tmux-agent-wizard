// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! `taw session …` — snapshot/restore of the tmux server's layout, with agent resume.
//!
//! herdr's `session` group manages its own persistent background servers; tmux *is* that
//! server, so ours manages **snapshots** of it instead: `save`,
//! `list`, `restore`, `delete`, plus `autosave` for the tmux hooks to call.

use std::path::Path;

use anyhow::Context as _;
use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::context;
use crate::session::create::Outcome;
use crate::session::{self, restore, snapshot};
use crate::tmux::ids;
use crate::tui::data::{Snapshot, SpaceRow};
use crate::tui::picker;

/// The snapshot name `save`/`restore` use when none is given — also the one `autosave`
/// writes.
const DEFAULT_NAME: &str = "default";

/// Autosave debounce: 5 s, via a marker file.
const AUTOSAVE_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How long `autosave --on-close` waits before checking whether tmux is still there to
/// save, as herdr's shutdown protection does.
const AUTOSAVE_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List saved snapshots
    List {
        /// Accepted for herdr parity; `taw` always prints JSON.
        #[arg(long)]
        json: bool,
    },
    /// Save a snapshot of the live server
    Save {
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Recreate whatever a snapshot holds that the live server is missing
    ///
    /// A session the server lacks is made whole. A session that is open gets back the saved
    /// windows with an agent whose conversation runs nowhere in the server, at its end; nothing
    /// open is changed. With no tmux server running, one is started with your own tmux config.
    Restore {
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Report the plan without executing it
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Delete a saved snapshot
    Delete {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Request a debounced autosave (called by the tmux hooks and the watcher)
    Autosave {
        /// The request comes from a pane or session closing, not opening (the
        /// after-kill-pane and session-closed hooks): wait five seconds, then skip the
        /// save if tmux itself has gone, so a shutdown keeps the layout it had.
        #[arg(long = "on-close")]
        on_close: bool,
    },
    /// Print the live session snapshot: every workspace, tab, pane, layout and agent
    Snapshot,
    /// Switch to, or create, the session for a directory (tilde or absolute)
    Open { path: String },
    /// List the folders the new-session picker offers on this machine
    ///
    /// What the picker lists before anything is typed, in its order: zoxide's folders, then the
    /// folders under `[sessions] project_dirs`, each told as `session` when a session is already
    /// running there, else `zoxide` or `project`. Nothing is asked and nothing is switched, and a
    /// machine with no zoxide, no project folders or no tmux server answers an empty list. Without
    /// `--json` it prints one line for each folder.
    Candidates {
        /// Print the envelope: `{"candidates":[{"kind","name","dir","session"}]}`
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List { .. } => ("list", list(ctx)),
        Cmd::Save { name } => ("save", save(ctx, name.as_deref())),
        Cmd::Restore { name, dry_run } => {
            ("restore", restore_snapshot(ctx, name.as_deref(), dry_run))
        }
        Cmd::Delete { name, .. } => ("delete", delete(ctx, &name)),
        Cmd::Autosave { on_close } => ("autosave", autosave(ctx, on_close)),
        Cmd::Snapshot => ("snapshot", live_snapshot(ctx)),
        Cmd::Open { path } => ("open", open(ctx, &path)),
        Cmd::Candidates { .. } => ("candidates", candidates(ctx)),
    }
}

/// `taw session candidates`: what the new-session picker lists on this machine before anything is
/// typed ([`picker::offered`]), as `{"candidates": [...]}`. Each has `kind`, `name` (the folder as the
/// picker shows it), `dir` (absolute) and, for a folder with a session running in it, `session`.
fn candidates(ctx: &Ctx) -> anyhow::Result<Value> {
    let home = dirs::home_dir().context("resolving the home directory")?;
    let zoxide = crate::tui::zoxide_dirs(&ctx.cfg);
    let offered = offered_here(ctx, &home, &zoxide)?;
    Ok(json!({ "candidates": offered.iter().map(candidate_json).collect::<Vec<_>>() }))
}

/// The picker's rows for this machine's own sessions.
pub(crate) fn offered_here(
    ctx: &Ctx,
    home: &Path,
    zoxide: &[String],
) -> anyhow::Result<Vec<picker::Offered>> {
    // A tmux that does not answer (no server, or a socket that is not there) has no sessions: the
    // folders are still this machine's, so this is not an error.
    let sessions = ctx.tmux.list_sessions().unwrap_or_else(|e| {
        log::debug!("session candidates: no sessions listed: {e:#}");
        Vec::new()
    });
    let spaces = sessions
        .into_iter()
        .map(|session| SpaceRow {
            session_id: session.session_id,
            session_name: session.session_name,
            session_path: session.session_path.into(),
            ..SpaceRow::default()
        })
        .collect();
    let snap = Snapshot {
        spaces,
        ..Snapshot::default()
    };
    Ok(picker::offered(
        &snap,
        &ctx.cfg,
        home,
        zoxide,
        picker::OFFERED_MAX,
    ))
}

fn candidate_json(row: &picker::Offered) -> Value {
    let mut fields = json!({
        "kind": row.kind,
        "name": row.name,
        "dir": row.dir.to_string_lossy(),
    });
    if let Some(session) = &row.session {
        fields["session"] = json!(session);
    }
    fields
}

/// The plain answer of `taw session candidates` without `--json`: a line for each folder, its kind
/// and name, and the session running there in brackets. Nothing for an empty list.
pub(crate) fn candidates_plain(ctx: &Ctx) -> anyhow::Result<String> {
    let home = dirs::home_dir().context("resolving the home directory")?;
    let zoxide = crate::tui::zoxide_dirs(&ctx.cfg);
    let offered = offered_here(ctx, &home, &zoxide)?;
    Ok(offered
        .iter()
        .map(|row| match &row.session {
            Some(session) => format!("{:<8} {} [{session}]\n", row.kind, row.name),
            None => format!("{:<8} {}\n", row.kind, row.name),
        })
        .collect())
}

/// herdr's socket protocol version, from the vendored schema, herdr v0.9.3's
/// (`third_party/herdr/reference/socket-api-schema.json`, `"protocol": 22`).
/// Reported so a consumer written against herdr's snapshot can tell which shape it is
/// reading: the schema's `SessionSnapshot` is the same at 20 and at 22.
const PROTOCOL: u32 = 22;

/// `session snapshot` — herdr's `session.snapshot`: the whole live server in one
/// envelope, as `{workspaces, tabs, panes, layouts, agents, focused_*}`.
///
/// This is not the same thing as `session save`, which writes a *file* for `restore` to
/// rebuild from. herdr's snapshot is a read: the current state of everything, in the
/// record shapes every other verb reports, for a client that wants one round trip instead
/// of five.
///
/// Built from a single `list_panes` plus one `list_windows`/`list_sessions` pair, because
/// a pane record already carries its session and window fields — so a server with fifty
/// panes still costs three tmux calls, not fifty.
fn live_snapshot(ctx: &Ctx) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let windows = common::list_windows(&ctx.tmux)?;
    let sessions = common::list_sessions(&ctx.tmux)?;

    let layouts: Vec<Value> = windows
        .iter()
        .filter_map(|window| match super::layout::snapshot(ctx, window) {
            Ok(layout) => Some(layout),
            // One window whose `#{window_layout}` will not parse must not take the whole
            // snapshot down: the other windows, and every pane record, are still true.
            Err(err) => {
                log::warn!(
                    "session snapshot: skipping the layout of tab {}: {err:#}",
                    window.window_id
                );
                None
            }
        })
        .collect();

    // The focused triple comes from the panes themselves: `#{session_attached}` says
    // which sessions a client is on, and `window_active`/`pane_active` narrow it to one
    // pane. No attached client means all three are null, which is herdr's own shape.
    let focused = panes
        .iter()
        .find(|p| p.session_attached > 0 && p.window_active && p.pane_active);

    Ok(json!({
        "snapshot": {
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": PROTOCOL,
            "workspaces": sessions
                .iter()
                .map(|s| context::workspace_json(s, &panes))
                .collect::<Vec<_>>(),
            "tabs": windows
                .iter()
                .map(|w| context::tab_json(w, &panes))
                .collect::<Vec<_>>(),
            "panes": panes.iter().map(context::pane_json).collect::<Vec<_>>(),
            "layouts": layouts,
            "agents": panes
                .iter()
                .filter(|p| p.agent.kind.is_some())
                .map(context::agent_json)
                .collect::<Vec<_>>(),
            "focused_workspace_id": focused.map(|p| ids::workspace_id(&p.session_id)),
            "focused_tab_id": focused.map(|p| ids::tab_id(&p.session_id, &p.window_id)),
            "focused_pane_id": focused.map(|p| ids::pane_id(&p.session_id, &p.pane_id)),
        }
    }))
}

fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    let sessions: Vec<Value> = snapshot::list(&ctx.paths)?
        .into_iter()
        .map(|info| {
            json!({
                "name": info.name,
                "path": info.path,
                "taken_unix": info.taken_unix,
                "sessions": info.sessions,
            })
        })
        .collect();
    Ok(json!({ "sessions": sessions }))
}

fn save(ctx: &Ctx, name: Option<&str>) -> anyhow::Result<Value> {
    let name = name.unwrap_or(DEFAULT_NAME);
    let snap = snapshot::take(&ctx.tmux)?;
    let path = snapshot::save_for_server(&ctx.paths, name, &snap, ctx.tmux.server_start_unix())?;
    // Opt-in (`[experimental] pane_history`), and a no-op that also *removes* any history
    // already saved under this name when it is off — see `session::write_history`.
    let history_panes = session::write_history(&ctx.tmux, &ctx.cfg, &ctx.paths, name, &snap);
    Ok(json!({
        "name": name,
        "path": path,
        "pane_history": ctx.cfg.experimental.pane_history,
        "panes_captured": history_panes,
        "sessions": snap.sessions.len(),
        "taken_unix": snap.taken_unix,
        // Normally 0. Non-zero means tmux reported panes this snapshot could not read, so
        // what was written is missing them — and a later `session restore` would rebuild
        // a layout that never existed. Reported rather than refused: the same code path
        // runs unattended from autosave.
        "skipped_panes": snap.skipped_panes,
    }))
}

fn restore_snapshot(ctx: &Ctx, name: Option<&str>, dry_run: bool) -> anyhow::Result<Value> {
    let shown = name.unwrap_or(DEFAULT_NAME);
    // A missing snapshot file is the common failure and deserves the precise code.
    let not_found = |e: anyhow::Error| {
        if snapshot::list(&ctx.paths)
            .map(|all| all.iter().all(|s| s.name != shown))
            .unwrap_or(false)
        {
            CliError::not_found(format!("no saved session named '{shown}'"))
        } else {
            e
        }
    };
    // `--name` is exactly that file. With none, the layout `default` holds, and the one before this
    // tmux server started when `default` has nothing to restore (`restore::apply_default`).
    let report = match name {
        Some(name) => restore::apply(&ctx.tmux, &ctx.cfg, &ctx.paths, name, dry_run),
        None => restore::apply_default(&ctx.tmux, &ctx.cfg, &ctx.paths, dry_run),
    }
    .map_err(not_found)?;
    let used = report.snapshot.as_ref();
    Ok(json!({
        "name": shown,
        "dry_run": dry_run,
        "created": report.created,
        "added": report.added,
        "skipped": report.skipped,
        "started_server": report.started_server,
        "attach": report.attach,
        "resumed": report.resumed,
        "not_resumed": report.not_resumed,
        "plan": report.plan,
        "would_type": report.would_type.iter().map(|w| json!({
            "session": w.session,
            "window": w.window,
            "pane": w.pane,
            "line": w.line,
        })).collect::<Vec<_>>(),
        "snapshot_file": used.map(|u| u.file.display().to_string()),
        "snapshot_taken_unix": used.map(|u| u.taken_unix),
        "server_start_unix": used.and_then(|u| u.server_start),
    }))
}

/// `session delete <name>` — the name has to match a saved snapshot exactly (as in herdr v0.9.1);
/// a wrong-cased one is refused with the spelling that is saved, since a case-insensitive
/// disk would delete it without a word.
fn delete(ctx: &Ctx, name: &str) -> anyhow::Result<Value> {
    snapshot::delete(&ctx.paths, name).map_err(|e| {
        if e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        }) {
            let hint = snapshot::case_variant(&ctx.paths, name)
                .map(|saved| {
                    format!(" (a saved session is named '{saved}'; names must match exactly)")
                })
                .unwrap_or_default();
            CliError::not_found(format!("no saved session named '{name}'{hint}"))
        } else {
            e
        }
    })?;
    Ok(json!({ "name": name }))
}

/// Marks an autosave as wanted and performs it if the debounce window has passed —
/// cheap enough for every `after-new-window`/`after-split-window`/`after-kill-pane`/
/// `session-closed` hook to call. `on_close` (the latter two) waits
/// [`AUTOSAVE_CLOSE_WAIT`] and skips if tmux itself is no longer there by then, rather
/// than racing the rest of a shutdown to save whatever partial layout this one closed
/// pane or session leaves behind.
fn autosave(ctx: &Ctx, on_close: bool) -> anyhow::Result<Value> {
    session::autosave_touch(&ctx.paths)?;
    let saved = if on_close {
        session::autosave_after_close(
            &ctx.tmux,
            &ctx.cfg,
            &ctx.paths,
            AUTOSAVE_MIN_INTERVAL,
            AUTOSAVE_CLOSE_WAIT,
        )?
    } else {
        session::autosave_if_due(&ctx.tmux, &ctx.cfg, &ctx.paths, AUTOSAVE_MIN_INTERVAL)?
    };
    Ok(json!({
        "saved": saved,
        "name": DEFAULT_NAME,
    }))
}

/// `taw session open <path>` — herdr's `session.open`: switches to the session for
/// `path` if one already has it as its `session_path`, else creates one
/// ([`session::create::open_or_create`]), then switches the current client to it.
fn open(ctx: &Ctx, path: &str) -> anyhow::Result<Value> {
    let home = dirs::home_dir().context("resolving the home directory")?;
    open_at(
        ctx,
        path,
        &home,
        common::current_pane().ok().as_deref(),
        true,
    )
}

/// [`open`] with the home directory and the pane this runs in chosen by the caller, and
/// with `switch` false for a run that moves no client (`taw open --no-switch`: `switched` is
/// then false). `taw open --json` prints this envelope for the directory it found, so a
/// script gets the same answer from either.
pub(crate) fn open_at(
    ctx: &Ctx,
    path: &str,
    home: &Path,
    pane: Option<&str>,
    switch: bool,
) -> anyhow::Result<Value> {
    let outcome = open_session(ctx, path, home)?;

    // `common::switch_client` picks which client to move (from `pane` when there is one,
    // else by hand: a key binding, a hook or a popup's own command has no `$TMUX_PANE` to
    // ask tmux for "the current one") and reports `false` rather than erroring when it
    // can't: a plugin or headless caller may still want the session created/found even
    // with nothing to switch.
    let switched = switch && common::switch_client(&ctx.tmux, &outcome.session_name, pane)?;

    Ok(json!({
        "session": outcome.session_name,
        "created": outcome.created,
        "collision": outcome.collision,
        "switched": switched,
    }))
}

/// The part of `session open` that finds or makes the session, and switches nothing: what
/// `taw open` runs before it moves a client or attaches a terminal itself.
///
/// `open_or_create` has no typed error to downcast for `code_for` (unlike
/// `restore_snapshot`'s missing-snapshot check above), so a missing directory is
/// recognised by its exact message text and remapped to `not_found` here instead of
/// falling through to `code_for`'s generic default.
pub(crate) fn open_session(ctx: &Ctx, path: &str, home: &Path) -> anyhow::Result<Outcome> {
    session::create::open_or_create(&ctx.tmux, &ctx.cfg, path, home).map_err(|e| {
        let message = e.to_string();
        if message.starts_with("no such directory: ") {
            CliError::not_found(message)
        } else {
            e
        }
    })
}
