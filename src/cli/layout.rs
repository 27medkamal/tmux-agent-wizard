//! `taw layout …` — herdr's `layout.export` / `layout.apply` / `layout.set_split_ratio`
//! over tmux windows, plus the two layout record shapes the rest of the CLI shares.
//!
//! herdr owns its own BSP tree, so a layout is something it can hand back verbatim and
//! rebuild from. tmux owns the geometry instead: `#{window_layout}` *is* the tree (parsed
//! by [`super::pane::layout`]), `select-layout` is how it is re-applied, and
//! `resize-pane` is the only way to move a divider. So `export` reads the live window,
//! `apply` builds the panes a saved tree names and then re-imposes its ratios, and
//! `set-split-ratio` resizes one divider.
//!
//! Two record shapes come out of here, both herdr's:
//!
//! * [`description`] — `LayoutDescription` (`{workspace_id, tab_id, zoomed,
//!   focused_pane_id, root}`), what `layout.export`/`apply`/`set_split_ratio` answer with.
//! * [`snapshot`] — `PaneLayoutSnapshot` (`{workspace_id, tab_id, zoomed, area,
//!   focused_pane_id, panes, splits}`), the flat form herdr's `layout.updated` event
//!   carries.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::pane::layout as tree;
use super::{Ctx, Sub};
use crate::tmux::{PaneInfo, WindowInfo, ids};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Export a tab's layout as herdr's split tree
    Export {
        /// A tab or a pane; the layout of the tab holding it is exported. Defaults to the
        /// current pane's tab.
        target: Option<String>,
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        #[arg(long, value_name = "PANE_ID")]
        pane: Option<String>,
    },
    /// Apply a saved split tree to a tab
    Apply {
        /// The tab to apply it to (a tab or pane id, or `.` for the current pane's tab)
        #[arg(value_name = "TAB")]
        tab: String,
        /// The tree, as the JSON `root` node `layout export` prints (`-` reads stdin)
        #[arg(value_name = "LAYOUT")]
        layout: String,
        #[arg(long)]
        focus: bool,
    },
    /// Move one split's divider
    SetSplitRatio {
        /// A pane inside the tab whose split is being resized
        #[arg(value_name = "PANE")]
        pane: String,
        /// The first child's share of the split, 0..1
        #[arg(value_name = "RATIO")]
        ratio: f64,
        /// Which split, as the `first`/`second` path from the root that `layout export`
        /// reports as a split's `id` (`root`, the default, is the outermost split)
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::Export { target, tab, pane } => (
            "export",
            export(
                ctx,
                target.as_deref().or(tab.as_deref()).or(pane.as_deref()),
            ),
        ),
        // `.` means "the tab I am in", the only way a positional-tab verb can name the
        // current tab (an optional positional cannot precede a required one in clap).
        Cmd::Apply { tab, layout, focus } => (
            "apply",
            apply(
                ctx,
                Some(tab.as_str()).filter(|t| *t != "."),
                &layout,
                focus,
            ),
        ),
        Cmd::SetSplitRatio { pane, ratio, path } => (
            "set-split-ratio",
            set_split_ratio(ctx, &pane, ratio, path.as_deref()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Shared record shapes
// ---------------------------------------------------------------------------

/// The window a layout target names: a tab id, a pane id, or (with no target) the tab
/// holding `$TMUX_PANE`.
pub(crate) fn window_of(ctx: &Ctx, target: Option<&str>) -> anyhow::Result<WindowInfo> {
    let window_id = match target {
        Some(target) => match ids::parse(target) {
            Some(ids::Target::Tab(w)) => w,
            _ => common::pane_info(&ctx.tmux, target)?.window_id,
        },
        None => {
            let current = common::current_pane()?;
            common::pane_info(&ctx.tmux, &current)?.window_id
        }
    };
    common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == window_id)
        .ok_or_else(|| CliError::not_found(format!("tab '{window_id}' not found")))
}

/// Resolves a layout leaf's pane-id number against `panes`, restricted to `window_id`.
///
/// A leaf carries the `<n>` of `%<n>`, which is persistent and *not* the pane's
/// positional `#{pane_index}` — see [`super::pane::layout::Kind::Leaf`].
fn resolver<'a>(
    panes: &'a [PaneInfo],
    window_id: &'a str,
) -> impl Fn(u32) -> Option<(String, String, u32)> + 'a {
    move |number| {
        let want = format!("%{number}");
        panes
            .iter()
            .find(|p| p.window_id == window_id && p.pane_id == want)
            .map(|p| {
                (
                    ids::pane_id(&p.session_id, &p.pane_id),
                    p.pane_id.clone(),
                    p.pane_index,
                )
            })
    }
}

/// The public pane id of `window`'s active pane, `""` when it has none.
fn focused_pane_id(panes: &[PaneInfo], window_id: &str) -> String {
    panes
        .iter()
        .find(|p| p.window_id == window_id && p.pane_active)
        .map(|p| ids::pane_id(&p.session_id, &p.pane_id))
        .unwrap_or_default()
}

/// herdr's `LayoutDescription` for one window: the nested split tree, plus the ids and
/// zoom state that say which tab it describes.
pub(crate) fn description(ctx: &Ctx, window: &WindowInfo) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let cell = tree::parse(&window.window_layout)?;
    let by_number = resolver(&panes, &window.window_id);
    Ok(json!({
        "workspace_id": ids::workspace_id(&window.session_id),
        "tab_id": ids::tab_id(&window.session_id, &window.window_id),
        "zoomed": zoomed(ctx, &window.window_id),
        "focused_pane_id": focused_pane_id(&panes, &window.window_id),
        "root": tree::to_json(&cell, &by_number),
    }))
}

/// herdr's `PaneLayoutSnapshot` for one window: the same tree listed flat, which is what
/// the `layout.updated` event carries.
pub(crate) fn snapshot(ctx: &Ctx, window: &WindowInfo) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let cell = tree::parse(&window.window_layout)?;
    let by_number = resolver(&panes, &window.window_id);
    let focused = focused_pane_id(&panes, &window.window_id);
    let flat = tree::flatten(&cell, &by_number, &focused);
    Ok(json!({
        "workspace_id": ids::workspace_id(&window.session_id),
        "tab_id": ids::tab_id(&window.session_id, &window.window_id),
        "zoomed": zoomed(ctx, &window.window_id),
        "area": json!({ "x": 0, "y": 0, "width": cell.width, "height": cell.height }),
        "focused_pane_id": focused,
        "panes": flat.panes,
        "splits": flat.splits,
    }))
}

/// `#{window_zoomed_flag}`, `false` when it cannot be read — a layout record is worth
/// reporting without the zoom flag, and this runs from an event hook where a window that
/// vanished mid-flight must not turn into an error.
fn zoomed(ctx: &Ctx, window_id: &str) -> bool {
    ctx.tmux
        .display(window_id, "#{window_zoomed_flag}")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

fn export(ctx: &Ctx, target: Option<&str>) -> anyhow::Result<Value> {
    let window = window_of(ctx, target)?;
    Ok(json!({ "layout": description(ctx, &window)? }))
}

/// Applies a saved tree: realizes it by splitting, in the tree's own directions, then
/// walks it once more imposing each split's exact `ratio`.
///
/// **The tab must hold exactly one pane.** A BSP tree is realized by splitting from a
/// single cell — that is the only way tmux can be made to reproduce a tree's *directions*,
/// not just its proportions. Rearranging a tab that already holds several panes would mean
/// killing them and starting over, and a pane here holds a live process, so this refuses
/// instead and says what to do. `taw tab create` gives a fresh single-pane tab to apply
/// onto; `taw layout set-split-ratio` adjusts a tab that already has the right shape.
///
/// What it does not re-create is a leaf's `command` or `label`: those name a process
/// herdr would have started, and starting one here would be running a saved command
/// string, which `taw` never does implicitly. A leaf's `cwd` *is* honoured, since the
/// panes are new.
fn apply(ctx: &Ctx, target: Option<&str>, layout: &str, focus: bool) -> anyhow::Result<Value> {
    let raw = if layout == "-" {
        read_stdin()?
    } else {
        layout.to_string()
    };
    let root: Value = serde_json::from_str(&raw)
        .map_err(|e| CliError::invalid_argument(format!("layout is not valid JSON: {e}")))?;
    // Rejects a bad `type` before anything is split, so a typo cannot leave the tab half
    // rebuilt.
    let wanted = count_panes(&root)?;
    let window = window_of(ctx, target)?;
    let panes = common::list_panes(&ctx.tmux)?;
    let live: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| p.window_id == window.window_id)
        .collect();
    if live.len() != 1 {
        return Err(CliError::invalid_argument(format!(
            "the tab holds {} panes; `layout apply` realizes a tree by splitting from a \
             single pane, so it never kills a running one. Close the extra panes, apply \
             onto a fresh `taw tab create`, or use `taw layout set-split-ratio` to adjust \
             the tab you have.",
            live.len()
        )));
    }
    let root_pane = live[0].pane_id.clone();

    let mut added = 0usize;
    realize(ctx, &root, &root_pane, &mut added)?;

    // A second pass for the ratios: `split-window -l N%` sizes the new pane against
    // whatever the pane being split measures *at that moment*, so a deep tree accumulates
    // rounding. This pass reads the realized layout and sets each divider in absolute
    // cells, which lands the proportions the tree actually asked for.
    let window = reread(ctx, &window.window_id)?;
    let applied = apply_ratios(ctx, &window, &root)?;

    if focus {
        let _ = ctx.tmux.run_ok(&["select-window", "-t", &window.window_id]);
    }
    let window = reread(ctx, &window.window_id)?;
    let layout = description(ctx, &window)?;
    // The layout changed, and `window-layout-changed` fired for each individual split on
    // the way — but a caller watching for "this apply is done" wants one event describing
    // the finished tab, which is what this is.
    super::common::emit_event(
        ctx,
        "layout.updated",
        json!({ "layout": snapshot(ctx, &window)? }),
    );
    Ok(json!({
        "layout": layout,
        "panes_created": added,
        "panes": wanted,
        "ratios_applied": applied,
    }))
}

fn reread(ctx: &Ctx, window_id: &str) -> anyhow::Result<WindowInfo> {
    common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == window_id)
        .ok_or_else(|| CliError::not_found("the tab vanished while applying the layout"))
}

/// Realizes one tree node into `target`, which already exists and holds the node's whole
/// area.
///
/// A `pane` leaf is already realized — `target` *is* it. A `split` node needs a second
/// cell, so `target` is split in the node's direction: tmux puts the new pane to the
/// right (`-h`) or below (`-v`), which is exactly the `second` branch, leaving `target`
/// holding `first`. Then each branch is realized into its own cell. Recursion depth is
/// the tree's depth, and the tree came from a `#{window_layout}` a real tmux window held.
fn realize(ctx: &Ctx, node: &Value, target: &str, added: &mut usize) -> anyhow::Result<()> {
    if node.get("type").and_then(Value::as_str) != Some("split") {
        return Ok(());
    }
    let direction = node
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("right");
    let flag = match direction {
        "right" => "-h",
        "down" => "-v",
        other => {
            return Err(CliError::invalid_argument(format!(
                "split direction must be 'right' or 'down', got {other:?}"
            )));
        }
    };
    let first = node
        .get("first")
        .ok_or_else(|| CliError::invalid_argument("split node has no 'first'"))?;
    let second = node
        .get("second")
        .ok_or_else(|| CliError::invalid_argument("split node has no 'second'"))?;

    // `-l` sizes the *new* pane, which holds `second` — so its share is `1 - ratio`.
    let ratio = node.get("ratio").and_then(Value::as_f64).unwrap_or(0.5);
    let second_percent = (((1.0 - ratio) * 100.0).round() as i64).clamp(1, 99);
    let mut args: Vec<String> = vec![
        "split-window".into(),
        "-d".into(),
        flag.into(),
        "-t".into(),
        target.into(),
        "-l".into(),
        format!("{second_percent}%"),
    ];
    // A leaf's `cwd` is honoured because the pane is new; its `command` is not run — see
    // this verb's doc comment.
    if let Some(cwd) = leaf_cwd(second) {
        args.push("-c".into());
        args.push(crate::tmux::start_directory(&cwd));
    }
    args.extend(["-P".into(), "-F".into(), "#{pane_id}".into()]);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let created = ctx
        .tmux
        .run(&borrowed)
        .map_err(|e| CliError::tmux(format!("splitting {target} {direction}: {e:#}")))?;
    let created = created.trim().to_string();
    if created.is_empty() {
        // tmux reads `-t ""` as the current pane, so an id that did not come back would
        // send the rest of this tree somewhere else entirely.
        return Err(CliError::tmux(
            "split-window -P answered an empty pane id".to_string(),
        ));
    }
    *added += 1;

    realize(ctx, first, target, added)?;
    realize(ctx, second, &created, added)
}

/// The `cwd` of the first pane leaf reachable from `node`, in `first`-branch order — the
/// leaf that will land in the pane a split creates.
fn leaf_cwd(node: &Value) -> Option<String> {
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => node
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        Some("split") => node.get("first").and_then(leaf_cwd),
        _ => None,
    }
}

fn read_stdin() -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| CliError::invalid_argument(format!("reading the layout from stdin: {e}")))?;
    Ok(buf)
}

/// How many `pane` leaves a tree names. Rejects a node that is neither a pane nor a
/// split, so a typo in `type` is an `invalid_argument` rather than a silently empty tree.
fn count_panes(node: &Value) -> anyhow::Result<usize> {
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => Ok(1),
        Some("split") => {
            let first = node
                .get("first")
                .ok_or_else(|| CliError::invalid_argument("split node has no 'first'"))?;
            let second = node
                .get("second")
                .ok_or_else(|| CliError::invalid_argument("split node has no 'second'"))?;
            Ok(count_panes(first)? + count_panes(second)?)
        }
        other => Err(CliError::invalid_argument(format!(
            "layout node type must be 'pane' or 'split', got {other:?}"
        ))),
    }
}

/// Walks `root` in document order, resizing the divider of every `split` node to its
/// `ratio`. Returns how many ratios were applied.
fn apply_ratios(ctx: &Ctx, window: &WindowInfo, root: &Value) -> anyhow::Result<usize> {
    let mut applied = 0usize;
    let mut path: Vec<bool> = Vec::new();
    walk_ratios(ctx, window, root, &mut path, &mut applied)?;
    Ok(applied)
}

fn walk_ratios(
    ctx: &Ctx,
    window: &WindowInfo,
    node: &Value,
    path: &mut Vec<bool>,
    applied: &mut usize,
) -> anyhow::Result<()> {
    if node.get("type").and_then(Value::as_str) != Some("split") {
        return Ok(());
    }
    if let Some(ratio) = node.get("ratio").and_then(Value::as_f64)
        && resize_split(ctx, window, path, ratio).is_ok()
    {
        *applied += 1;
    }
    if let Some(first) = node.get("first") {
        path.push(false);
        walk_ratios(ctx, window, first, path, applied)?;
        path.pop();
    }
    if let Some(second) = node.get("second") {
        path.push(true);
        walk_ratios(ctx, window, second, path, applied)?;
        path.pop();
    }
    Ok(())
}

fn set_split_ratio(ctx: &Ctx, pane: &str, ratio: f64, path: Option<&str>) -> anyhow::Result<Value> {
    let path = parse_path(path.unwrap_or("root"))?;
    // Strictly between 0 and 1: either end means one side of the divider gets no cells,
    // which is not a layout tmux can hold — and `resize-pane` would silently clamp it to
    // one cell instead of reporting the impossible request.
    if !ratio.is_finite() || ratio <= 0.0 || ratio >= 1.0 {
        return Err(CliError::invalid_argument(format!(
            "ratio must be greater than 0 and less than 1, got {ratio}"
        )));
    }
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    let info = common::pane_info(&ctx.tmux, &pane_id)?;
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == info.window_id)
        .ok_or_else(|| CliError::not_found(format!("tab of pane '{pane}' not found")))?;
    resize_split(ctx, &window, &path, ratio)?;
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == info.window_id)
        .ok_or_else(|| CliError::not_found(format!("tab of pane '{pane}' not found")))?;
    Ok(json!({ "layout": description(ctx, &window)? }))
}

/// `root`, `root.first`, `root.second.first`, … — or the raw `[bool]` form herdr's
/// `layout.set_split_ratio` takes, written as `0`/`1`. `false`/`0` is `first`.
fn parse_path(raw: &str) -> anyhow::Result<Vec<bool>> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("root")
        .map(|rest| rest.trim_start_matches('.'))
        .unwrap_or(trimmed);
    if body.is_empty() {
        return Ok(Vec::new());
    }
    body.split([',', '.'])
        .map(|step| match step.trim().to_ascii_lowercase().as_str() {
            "first" | "0" | "false" => Ok(false),
            "second" | "1" | "true" => Ok(true),
            other => Err(CliError::invalid_argument(format!(
                "layout path step must be 'first' or 'second' (or 0/1), got {other:?}"
            ))),
        })
        .collect()
}

/// Moves the divider of the split `path` names to `ratio`, by resizing the pane at the
/// end of the split's `first` branch.
///
/// tmux has no "set this split to N%" command: `resize-pane -x`/`-y` sizes one *pane*,
/// and a pane's size along the split axis is exactly the first branch's share of it. So
/// the target is the first pane reachable through `first`, and the size is `ratio` of the
/// span the split covers — read from the live layout, so a nested split is measured
/// against its own span rather than the whole window.
fn resize_split(ctx: &Ctx, window: &WindowInfo, path: &[bool], ratio: f64) -> anyhow::Result<()> {
    let cell = tree::parse(&window.window_layout)?;
    let found = tree::at_path(&cell, path).ok_or_else(|| {
        CliError::not_found(format!(
            "no split at path {} in tab '{}'",
            tree::render_path(path),
            ids::tab_id(&window.session_id, &window.window_id),
        ))
    })?;
    let cells = (ratio * f64::from(found.span)).round().max(1.0) as u32;
    let target = format!("%{}", found.first_pane_number);
    let flag = if found.horizontal { "-x" } else { "-y" };
    ctx.tmux
        .run_ok(&["resize-pane", flag, &cells.to_string(), "-t", &target])
        .map_err(|e| CliError::tmux(format!("resizing the split: {e:#}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_path_accepts_the_reported_id_form_and_the_raw_bool_form() {
        assert_eq!(parse_path("root").unwrap(), Vec::<bool>::new());
        assert_eq!(parse_path("").unwrap(), Vec::<bool>::new());
        assert_eq!(parse_path("root.first").unwrap(), vec![false]);
        assert_eq!(parse_path("root.second.first").unwrap(), vec![true, false]);
        // The socket API's own `[bool]`, written either way round.
        assert_eq!(parse_path("0,1").unwrap(), vec![false, true]);
        assert_eq!(parse_path("second,second").unwrap(), vec![true, true]);
        assert_eq!(parse_path("FIRST").unwrap(), vec![false]);
    }

    #[test]
    fn parse_path_rejects_a_step_that_names_neither_branch() {
        let err = parse_path("root.middle").unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "invalid_argument");
    }

    #[test]
    fn count_panes_counts_the_leaves_and_rejects_an_unknown_node_type() {
        assert_eq!(count_panes(&json!({"type": "pane"})).unwrap(), 1);
        let tree = json!({
            "type": "split",
            "first": {"type": "pane"},
            "second": {"type": "split", "first": {"type": "pane"}, "second": {"type": "pane"}},
        });
        assert_eq!(count_panes(&tree).unwrap(), 3);
        let err = count_panes(&json!({"type": "leaf"})).unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "invalid_argument");
        let err = count_panes(&json!({"type": "split", "first": {"type": "pane"}})).unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "invalid_argument");
    }
}
