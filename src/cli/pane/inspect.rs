//! `taw pane list | current | get | layout | process-info | neighbor | edges` — the
//! read-only pane queries. Nothing here changes tmux state.

use serde_json::{Value, json};

use super::layout;
use crate::cli::Ctx;
use crate::cli::common::{self, Direction};
use crate::cli::envelope::CliError;
use crate::plugin::context;
use crate::tmux::ids;

pub(super) fn list(ctx: &Ctx, workspace: Option<&str>) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let filter = match workspace {
        Some(w) => Some(common::resolve_session(&ctx.tmux, w)?),
        None => None,
    };
    let records: Vec<Value> = panes
        .iter()
        .filter(|p| filter.as_deref().is_none_or(|s| p.session_id == s))
        .map(context::pane_json)
        .collect();
    Ok(json!({ "panes": records }))
}

pub(super) fn get_one(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<Value> {
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    let pane = common::pane_info(&ctx.tmux, &pane_id)?;
    Ok(json!({ "pane": context::pane_json(&pane) }))
}

/// The layout of the tab holding the target. The target may name a pane *or* a tab —
/// herdr's `pane layout` takes a pane, its `layout <tab>` a tab.
pub(super) fn layout_of(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let window_id = match positional.or(pane_flag) {
        Some(target) => match ids::parse(target) {
            Some(ids::Target::Tab(w)) => w,
            _ => common::pane_info(&ctx.tmux, target)?.window_id,
        },
        None => {
            let current = common::current_pane()?;
            common::pane_info(&ctx.tmux, &current)?.window_id
        }
    };
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|w| w.window_id == window_id)
        .ok_or_else(|| CliError::not_found(format!("tab '{window_id}' not found")))?;

    let tree = layout::parse(&window.window_layout)?;
    // A layout leaf carries a pane-id NUMBER (the `<n>` of `%<n>`), not a `#{pane_index}`:
    // the two diverge the moment a pane is killed, so matching on the index resolved every
    // leaf but the first to null. Match on `#{pane_id}` and report the pane's own index.
    let by_number = |number: u32| {
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
    };
    Ok(json!({
        "tab_id": ids::tab_id(&window.session_id, &window.window_id),
        "layout": layout::to_json(&tree, &by_number),
        "window_layout": window.window_layout,
    }))
}

pub(super) fn process_info(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<Value> {
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    let pane = common::pane_info(&ctx.tmux, &pane_id)?;
    // herdr's `shell_pid` is null for a pane with no process, so a dead pane has none here on every
    // tmux version: 3.8 prints no pid for it, and 3.6b prints the one of the program that ended,
    // which the system may have given to another process since.
    let pid = (!pane.pane_dead && pane.pane_pid > 0).then_some(pane.pane_pid);
    // herdr's `PaneProcessInfo`: the terminal and the processes in front of it, read the way the
    // watcher reads them (`foreground_job`). A pane with no process has none of them.
    let tty = pid
        .and_then(|_| ctx.tmux.display(&pane.pane_id, "#{pane_tty}").ok())
        .map(|tty| tty.trim().to_string())
        .filter(|tty| !tty.is_empty());
    let job = pid.and_then(crate::watch::foreground_job);
    let foreground: Vec<Value> = job
        .as_ref()
        .map(|job| {
            job.processes
                .iter()
                .map(|process| foreground_process(process, &pane.pane_current_path, job.group))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "process_info": {
            "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
            "shell_pid": pid,
            "tty": tty,
            "foreground_process_group_id": job.as_ref().map(|job| job.group),
            "foreground_processes": foreground,
        },
        "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
        "terminal_id": pane.pane_id,
        "pid": pid,
        "command": pane.pane_current_command,
        "cwd": pane.pane_current_path,
        "dead": pane.pane_dead,
    }))
}

/// One entry of `foreground_processes` (herdr's `PaneProcessInfoProcess`): `pid`, `name`, `argv0`
/// (the program as it was started, without its directories), `argv`, `cmdline` and `cwd`. The
/// arguments are the exact list when the system gave it, else the words of the line `ps` printed.
/// `cwd` is the process's own folder; tmux's folder for the pane is the leader's, so it stands in
/// for the leader (`pid == group`) when the system will not say.
fn foreground_process(process: &crate::detect::job::Process, pane_cwd: &str, group: u32) -> Value {
    let argv: Vec<String> = match (&process.argv, &process.line) {
        (Some(argv), _) => argv.clone(),
        (None, Some(line)) => line.split_whitespace().map(str::to_string).collect(),
        (None, None) => Vec::new(),
    };
    let cmdline = match (&process.argv, &process.line) {
        (Some(argv), _) => argv.join(" "),
        (None, Some(line)) => line.clone(),
        (None, None) => process.name.clone(),
    };
    let argv0 = argv
        .first()
        .and_then(|program| program.rsplit('/').next())
        .map_or_else(|| process.name.clone(), str::to_string);
    let cwd = process_cwd(process.pid)
        .or_else(|| (process.pid == group && !pane_cwd.is_empty()).then(|| pane_cwd.to_string()));
    json!({
        "pid": process.pid,
        "name": process.name,
        "argv0": argv0,
        "argv": argv,
        "cmdline": cmdline,
        "cwd": cwd,
    })
}

/// The working folder of the process `pid`, asked of the system and of no program: `/proc` on
/// Linux, `proc_pidinfo` on macOS. `None` when it cannot be read.
#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: an all-zero `proc_vnodepathinfo` is a valid value (integers and byte arrays).
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
    // SAFETY: `info` is `size` bytes of writable memory for the whole call.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            std::ptr::addr_of_mut!(info).cast::<libc::c_void>(),
            size,
        )
    };
    if got != size {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    // SAFETY: `vip_path` is `MAXPATHLEN` (1024) contiguous bytes, which the system fills in.
    let bytes = unsafe { std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), 1024) };
    let end = bytes.iter().position(|b| *b == 0)?;
    (end > 0).then(|| String::from_utf8_lossy(&bytes[..end]).into_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

/// Every pane of the target's window, with geometry, plus the target's own entry.
pub(super) fn geometry_of(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<(crate::tmux::PaneInfo, Vec<common::Geometry>)> {
    let pane_id = common::pane_target(&ctx.tmux, positional, pane_flag)?;
    let pane = common::pane_info(&ctx.tmux, &pane_id)?;
    let geometry = common::window_geometry(&ctx.tmux, &pane.window_id)?;
    Ok((pane, geometry))
}

pub(super) fn own_geometry<'a>(
    geometry: &'a [common::Geometry],
    pane_id: &str,
) -> anyhow::Result<&'a common::Geometry> {
    geometry
        .iter()
        .find(|g| g.pane_id == pane_id)
        .ok_or_else(|| CliError::not_found(format!("pane '{pane_id}' not found in its tab")))
}

pub(super) fn neighbor(
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
    let info = common::pane_info(&ctx.tmux, &found.pane_id)?;
    Ok(json!({ "pane": context::pane_json(&info) }))
}

pub(super) fn edges(
    ctx: &Ctx,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<Value> {
    let (pane, geometry) = geometry_of(ctx, positional, pane_flag)?;
    let own = own_geometry(&geometry, &pane.pane_id)?;
    Ok(json!({
        "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
        "left": own.left,
        "right": own.right,
        "top": own.top,
        "bottom": own.bottom,
        "width": pane.pane_width,
        "height": pane.pane_height,
        // Whether the pane touches each edge of its tab — i.e. has no neighbour there.
        "at_left": common::neighbor(&geometry, own, Direction::Left).is_none(),
        "at_right": common::neighbor(&geometry, own, Direction::Right).is_none(),
        "at_top": common::neighbor(&geometry, own, Direction::Up).is_none(),
        "at_bottom": common::neighbor(&geometry, own, Direction::Down).is_none(),
    }))
}
