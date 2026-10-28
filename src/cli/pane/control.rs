//! `taw pane focus | resize | zoom | rename | split | swap | move | close` — the verbs
//! that change tmux's own layout.

use serde_json::{Value, json};

use super::SplitDirection;
use super::inspect::{geometry_of, own_geometry};
use crate::cli::Ctx;
use crate::cli::common::{self, Direction};
use crate::cli::envelope::CliError;
use crate::plugin::context;
use crate::store;
use crate::tmux::ids;

/// herdr's `pane focus --direction` focuses the *neighbour* in that direction (unlike
/// `agent focus`, which focuses a named agent's own pane).
pub(super) fn focus_neighbor(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
    direction: Direction,
) -> anyhow::Result<Value> {
    let (pane, geometry) = geometry_of(ctx, positional, pane_flag)?;
    let own = own_geometry(&geometry, &pane.pane_id)?;
    let found = common::neighbor(&geometry, own, direction).ok_or_else(|| {
        CliError::not_found(format!(
            "no pane {direction:?} of {}",
            ids::pane_id(&pane.session_id, &pane.pane_id)
        ))
    })?;
    ctx.tmux
        .run_ok(&["select-pane", "-t", &found.pane_id])
        .map_err(|e| CliError::tmux(format!("selecting pane: {e:#}")))?;
    let info = common::pane_info(&ctx.tmux, &found.pane_id)?;
    Ok(json!({ "pane": context::pane_json(&info) }))
}

pub(super) fn resize(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
    direction: Direction,
    amount: Option<f64>,
) -> anyhow::Result<Value> {
    /// tmux's own `resize-pane` default when no size is given.
    const DEFAULT_CELLS: f64 = 5.0;
    let cells = amount.unwrap_or(DEFAULT_CELLS);
    if !cells.is_finite() || cells <= 0.0 {
        return Err(CliError::invalid_argument(format!(
            "--amount must be a positive number of cells, got {cells}"
        )));
    }
    let cells = cells.round().max(1.0) as u32;
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    ctx.tmux
        .run_ok(&[
            "resize-pane",
            direction.flag(),
            "-t",
            &pane_id,
            &cells.to_string(),
        ])
        .map_err(|e| CliError::tmux(format!("resizing pane: {e:#}")))?;
    let pane = common::pane_info(&ctx.tmux, &pane_id)?;
    Ok(json!({ "pane": context::pane_json(&pane) }))
}

pub(super) fn zoom(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
    toggle: bool,
    on: bool,
    off: bool,
) -> anyhow::Result<Value> {
    if on && off {
        return Err(CliError::invalid_argument(
            "--on and --off are mutually exclusive",
        ));
    }
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    let zoomed_now = ctx
        .tmux
        .display(&pane_id, "#{window_zoomed_flag}")
        .map_err(|e| CliError::tmux(format!("reading zoom state: {e:#}")))?
        == "1";
    // tmux only has a *toggle* (`resize-pane -Z`), so `--on`/`--off` toggle only when the
    // pane is not already in the requested state. No flag at all means toggle, as herdr's
    // `--toggle` does.
    let want = if on {
        true
    } else if off {
        false
    } else {
        debug_assert!(toggle || !(on || off));
        !zoomed_now
    };
    if want != zoomed_now {
        ctx.tmux
            .run_ok(&["resize-pane", "-Z", "-t", &pane_id])
            .map_err(|e| CliError::tmux(format!("toggling zoom: {e:#}")))?;
    }
    let pane = common::pane_info(&ctx.tmux, &pane_id)?;
    Ok(json!({
        "pane": context::pane_json(&pane),
        "zoomed": want,
    }))
}

/// `pane rename` sets `@agent_title`, the pane record's `title` — the same field
/// `report-metadata --title` writes, since both mean "the display label for this pane".
///
/// tmux's own `#{pane_title}` is deliberately left alone: it is the OSC title the
/// detection engine reads, so overwriting it would corrupt detection input.
pub(super) fn rename(
    ctx: &Ctx,
    pane_id: &str,
    label: &[String],
    clear: bool,
) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    if clear {
        ctx.tmux
            .unset_pane_option(&resolved, "@agent_title")
            .map_err(|e| CliError::tmux(format!("clearing @agent_title: {e:#}")))?;
    } else {
        if label.is_empty() {
            return Err(CliError::invalid_argument(
                "pane rename needs a LABEL, or --clear to remove one",
            ));
        }
        store::set_text_option(&ctx.tmux, &resolved, "@agent_title", &label.join(" "))
            .map_err(|e| CliError::tmux(format!("setting @agent_title: {e:#}")))?;
    }
    let pane = common::pane_info(&ctx.tmux, &resolved)?;
    crate::cli::events::pane_updated(ctx, &pane);
    Ok(json!({ "pane": context::pane_json(&pane) }))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn split(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
    direction: SplitDirection,
    ratio: Option<f64>,
    cwd: Option<&str>,
    env: &[String],
    focus: bool,
) -> anyhow::Result<Value> {
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    // herdr defaults a split to the *source pane's* working directory (reference/SKILL.md:
    // "the current working directory"). With no explicit -c, tmux starts the new pane in
    // the cwd of the `split-window` client we spawn — taw's own process cwd — not the
    // pane's. So when --cwd is omitted, resolve the source pane's #{pane_current_path} and
    // pass it through ourselves.
    let cwd: Option<String> = match cwd {
        Some(cwd) => Some(cwd.to_string()),
        None => ctx
            .tmux
            .display(&pane_id, "#{pane_current_path}")
            .ok()
            .filter(|path| !path.is_empty()),
    };
    let mut args: Vec<String> = vec![
        "split-window".into(),
        direction.flag().into(),
        "-t".into(),
        pane_id.clone(),
    ];
    if let Some(ratio) = ratio {
        if !(0.0..=1.0).contains(&ratio) || !ratio.is_finite() {
            return Err(CliError::invalid_argument(format!(
                "--ratio must be between 0 and 1, got {ratio}"
            )));
        }
        // `-l N%` sizes the *new* pane, which is what `--ratio` means for a split.
        args.push("-l".into());
        args.push(format!("{}%", (ratio * 100.0).round() as u32));
    }
    if let Some(cwd) = &cwd {
        args.push("-c".into());
        args.push(crate::tmux::start_directory(cwd));
    }
    for pair in env {
        // Validated so a missing `=` is `invalid_argument` rather than a tmux error.
        common::parse_kv(pair)?;
        args.push("-e".into());
        args.push(pair.clone());
    }
    if !focus {
        args.push("-d".into());
    }
    // `-P -F` makes tmux print the new pane's id, so we never have to guess which it is.
    args.push("-P".into());
    args.push("-F".into());
    args.push("#{pane_id}".into());

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let new_pane = ctx
        .tmux
        .run(&borrowed)
        .map_err(|e| CliError::tmux(format!("splitting pane '{pane_id}': {e:#}")))?;
    let new_pane = new_pane.trim();
    // What the caller is told is what the `pane.created` event carries, when one is built, read once
    // for both.
    if let Some(payload) =
        crate::cli::events::created_payload(ctx, crate::cli::events::Created::Pane, new_pane)
    {
        return Ok(json!({ "pane": payload["pane"].clone() }));
    }
    let info = common::pane_info(&ctx.tmux, new_pane)?;
    let pane = context::pane_json(&info);

    // `pane.created` is deliberately **not** emitted here: `split-window` fires tmux's
    // `after-split-window`, whose hook builds the same payload from the new pane (the
    // command's target, which is what that hook's format context resolves to). One
    // source, and it covers `prefix %` too. See `crate::cli::events`.
    Ok(json!({ "pane": pane }))
}

pub(super) fn swap(
    ctx: &Ctx,
    source: Option<&str>,
    target: Option<&str>,
    direction: Option<Direction>,
) -> anyhow::Result<Value> {
    let source_id = match source {
        Some(s) => common::resolve_pane(&ctx.tmux, s)?,
        None => {
            let current = common::current_pane()?;
            common::resolve_pane(&ctx.tmux, &current)?
        }
    };
    let target_id = match (target, direction) {
        (Some(t), _) => common::resolve_pane(&ctx.tmux, t)?,
        (None, Some(direction)) => {
            let pane = common::pane_info(&ctx.tmux, &source_id)?;
            let geometry = common::window_geometry(&ctx.tmux, &pane.window_id)?;
            let own = own_geometry(&geometry, &source_id)?;
            common::neighbor(&geometry, own, direction)
                .ok_or_else(|| {
                    CliError::not_found(format!(
                        "no pane {direction:?} of '{source_id}' to swap with"
                    ))
                })?
                .pane_id
                .clone()
        }
        (None, None) => {
            return Err(CliError::invalid_argument(
                "pane swap needs --target-pane or --direction",
            ));
        }
    };
    ctx.tmux
        .run_ok(&["swap-pane", "-s", &source_id, "-t", &target_id])
        .map_err(|e| CliError::tmux(format!("swapping panes: {e:#}")))?;
    // Public ids, like every other verb's reply. Read *after* the swap: swapping between
    // sessions changes which workspace qualifies each pane's public id.
    let source = common::pane_info(&ctx.tmux, &source_id)?;
    let target = common::pane_info(&ctx.tmux, &target_id)?;
    Ok(json!({
        "source_pane_id": ids::pane_id(&source.session_id, &source.pane_id),
        "target_pane_id": ids::pane_id(&target.session_id, &target.pane_id),
    }))
}

/// `pane move`'s many mutually-exclusive destinations, gathered so [`move_pane`] takes
/// one parameter instead of eleven.
pub(super) struct MoveArgs<'a> {
    pub pane_id: &'a str,
    pub tab: Option<&'a str>,
    pub split: Option<SplitDirection>,
    pub target_pane: Option<&'a str>,
    pub ratio: Option<f64>,
    pub new_tab: bool,
    pub workspace: Option<&'a str>,
    pub new_workspace: bool,
    /// Names whatever the move *creates*: the new tab under `--new-tab`, the new
    /// workspace under `--new-workspace` (socket-api.mdx, `pane.move` destinations).
    pub label: Option<&'a str>,
    /// With `--new-workspace` only: names that workspace's first tab. herdr keeps this
    /// distinct from `label`, which names the workspace itself.
    pub tab_label: Option<&'a str>,
    pub focus: bool,
}

/// `pane move`: `break-pane` for a new tab/workspace, `join-pane` for an existing one.
///
/// The pane keeps its tmux id (`%N`) but its *public* id is workspace-qualified, so it
/// changes whenever the pane lands in a different session — hence `previous_pane_id` in
/// the reply (herdr's own contract, see SKILL.md).
pub(super) fn move_pane(ctx: &Ctx, args: MoveArgs<'_>) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, args.pane_id)?;
    let before = common::pane_info(&ctx.tmux, &pane_id)?;
    let previous_pane_id = ids::pane_id(&before.session_id, &before.pane_id);
    let t = &ctx.tmux;

    if args.new_workspace {
        // tmux cannot break a pane straight into a new session: create the session (which
        // comes with a placeholder pane), join into its window, then drop the placeholder.
        let name = args
            .label
            .map(str::to_string)
            .unwrap_or_else(|| format!("taw-{}", common::now_unix()));
        let mut new_session: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            crate::tmux::new_name(&name),
            "-c".into(),
            crate::tmux::start_directory(&before.pane_current_path),
        ];
        // `--tab-label` names the *tab* inside the new workspace, which `--label` names.
        if let Some(tab_label) = args.tab_label {
            new_session.push("-n".into());
            new_session.push(crate::tmux::new_name(tab_label));
        }
        new_session.extend(["-P".into(), "-F".into(), "#{pane_id}".into()]);
        let borrowed: Vec<&str> = new_session.iter().map(String::as_str).collect();
        let placeholder = t
            .run(&borrowed)
            .map_err(|e| CliError::tmux(format!("creating workspace '{name}': {e:#}")))?;
        let placeholder = placeholder.trim().to_string();
        join(
            t,
            &pane_id,
            &placeholder,
            args.split,
            args.ratio,
            args.focus,
        )?;
        // Only now that the pane is safely in place is the placeholder redundant.
        t.run_ok(&["kill-pane", "-t", &placeholder])
            .map_err(|e| CliError::tmux(format!("removing placeholder pane: {e:#}")))?;
    } else if let Some(workspace) = args.workspace {
        let session = common::session_info(t, workspace)?;
        let window = if session.active_window_id.is_empty() {
            return Err(CliError::not_found(format!(
                "workspace '{workspace}' has no active tab to move into"
            )));
        } else {
            session.active_window_id.clone()
        };
        join(t, &pane_id, &window, args.split, args.ratio, args.focus)?;
    } else if let Some(tab) = args.tab {
        let window_id = common::resolve_window(t, tab)?;
        join(t, &pane_id, &window_id, args.split, args.ratio, args.focus)?;
    } else if let Some(target) = args.target_pane {
        let target_id = common::resolve_pane(t, target)?;
        join(t, &pane_id, &target_id, args.split, args.ratio, args.focus)?;
    } else if args.new_tab {
        // `-t <session id>:` is the pane's own session (its next free window index). With no
        // `-t`, and no client to go by, tmux uses the session it counts as current, the newest,
        // and the pane left its workspace. A target is not format-expanded, so the `$N` is as is.
        let mut break_args: Vec<String> = vec![
            "break-pane".into(),
            "-s".into(),
            pane_id.clone(),
            "-t".into(),
            format!("{}:", before.session_id),
        ];
        if !args.focus {
            break_args.push("-d".into());
        }
        if let Some(label) = args.label {
            // `break-pane -n` takes its name as it is, a `#` included (tmux 3.6b), and only a
            // trailing `;` has to be written so ([`crate::tmux::new_name`]).
            break_args.push("-n".into());
            break_args.push(crate::store::word(label));
        }
        let borrowed: Vec<&str> = break_args.iter().map(String::as_str).collect();
        t.run_ok(&borrowed)
            .map_err(|e| CliError::tmux(format!("breaking pane into a new tab: {e:#}")))?;
    } else {
        return Err(CliError::invalid_argument(
            "pane move needs one of --tab, --target-pane, --new-tab, --workspace or \
             --new-workspace",
        ));
    }

    let after = common::pane_info(t, &pane_id)?;
    if args.focus {
        // The move has happened; failing to follow the pane must not report it as failed.
        let _ = common::focus_pane(t, &after, args.pane_id);
    }
    Ok(json!({
        "move_result": {
            "pane": context::pane_json(&after),
            "previous_pane_id": previous_pane_id,
        }
    }))
}

/// `join-pane -s <pane> -t <target>`, with the split axis and size herdr's flags name.
///
/// Without `focus` it is `join-pane -d`: tmux's default makes the joined pane the
/// destination window's active one *and* selects that window in the destination session, so
/// a `--no-focus` move flipped the tab the user was looking at.
pub(super) fn join(
    t: &crate::tmux::Tmux,
    pane_id: &str,
    target: &str,
    split: Option<SplitDirection>,
    ratio: Option<f64>,
    focus: bool,
) -> anyhow::Result<()> {
    let mut args: Vec<String> = vec!["join-pane".into(), "-s".into(), pane_id.into()];
    if !focus {
        args.push("-d".into());
    }
    if let Some(direction) = split {
        args.push(direction.flag().into());
    }
    if let Some(ratio) = ratio {
        if !(0.0..=1.0).contains(&ratio) || !ratio.is_finite() {
            return Err(CliError::invalid_argument(format!(
                "--ratio must be between 0 and 1, got {ratio}"
            )));
        }
        args.push("-l".into());
        args.push(format!("{}%", (ratio * 100.0).round() as u32));
    }
    args.push("-t".into());
    args.push(target.into());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    t.run_ok(&borrowed)
        .map_err(|e| CliError::tmux(format!("joining pane '{pane_id}' to '{target}': {e:#}")))
}

pub(super) fn close(ctx: &Ctx, pane_id: &str) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    common::pre_close(ctx, crate::hooks::Closing::Pane(&resolved))?;
    ctx.tmux
        .run_ok(&["kill-pane", "-t", &resolved])
        .map_err(|e| CliError::tmux(format!("closing pane '{pane_id}': {e:#}")))?;
    // `pane.closed` is deliberately **not** emitted here. `kill-pane` fires tmux's
    // `after-kill-pane` in this same server, and the hook installed on it recovers the
    // pane's id by diffing the live panes against the previous set — so it covers this
    // verb and `prefix x` alike, from one source. Emitting here as well would fire the
    // event twice for one close. See `crate::cli::events::detect_closed`.
    Ok(json!({ "terminal_id": resolved }))
}
