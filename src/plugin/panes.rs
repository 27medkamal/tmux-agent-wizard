//! `[[panes]]`: open/focus/close plugin panes as tmux popups or split panes. See plugins.mdx "Panes".

use anyhow::Context as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{Context, PluginCtx, env, manifest, registry};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PaneError {
    #[error("not_found: plugin or pane '{0}' not found")]
    NotFound(String),
}

/// Opens `plugin.pane`'s entrypoint per its manifest `placement`:
/// - `"overlay"` -> `tmux display-popup -E -w 80% -h 80%`; returns the literal string
///   `"popup"` (a popup has no tmux pane id — plugins.mdx "A popup is a singleton session
///   resource rather than a Herdr pane ... it has no pane ID").
/// - `"side"`/`"bottom"` -> `tmux split-window -h`/`-v -l 40%`; returns the new tmux pane
///   id and records it (keyed by this plugin's pane entrypoint id) in
///   `<state>/plugins/state/<id>/panes.json` so [`focus`]/[`close`] can find it again.
///
/// The split/popup is opened relative to `context.pane`'s `terminal_id` when present, else
/// `context.caller_pane_id` (resolved as any pane id), else tmux's own default target.
pub fn open(ctx: &PluginCtx, qualified: &str, context: &Context) -> anyhow::Result<String> {
    open_with(ctx, qualified, context, &OpenOptions::default())
}

/// What herdr's `plugin pane open` flags ask beyond the entrypoint (`--placement`, `--direction`,
/// `--env`, `--focus`); the default is the positional form's, which the manifest decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenOptions {
    /// `overlay`, `popup`, `split`, `tab` or `zoomed`, over the manifest's `placement`.
    pub placement: Option<String>,
    /// `right` or `down`, for a split.
    pub direction: Option<String>,
    /// Variables the pane gets besides the plugin's own, a later one over an earlier.
    pub env: Vec<(String, String)>,
    /// Herdr's flag form leaves the new pane unfocused unless it is asked to focus it: `Some(focus)`
    /// is that form; `None` is the positional form, where tmux's own choice (the new pane is
    /// current) stands.
    pub focus: Option<bool>,
    /// The folder the pane starts in (herdr's `--cwd`, absolute), over the plugin's own folder.
    pub cwd: Option<String>,
    /// A popup's outer width and height, in cells (`80`) or as a percentage (`80%`); `80%` when unset.
    pub width: Option<String>,
    pub height: Option<String>,
    /// The tmux pane (a raw `%id`) the new pane is placed beside, over the context's (herdr's `--target-pane`).
    pub target_pane: Option<String>,
}

/// Where a plugin pane goes in tmux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// `display-popup`.
    Popup,
    /// `split-window`, beside (`horizontal`) or below.
    Split { horizontal: bool },
    /// `new-window`.
    Tab,
    /// `split-window`, then the new pane zoomed.
    Zoomed { horizontal: bool },
}

/// The tmux placement of `placement` (the manifest's, or the one a flag gave) and `direction`. The
/// manifest's `side` and `bottom` are taw's older words for a split to the right and one below.
fn plan(placement: &str, direction: Option<&str>) -> anyhow::Result<Plan> {
    let horizontal = match direction {
        None | Some("right") => true,
        Some("down") => false,
        Some(other) => anyhow::bail!("unknown direction '{other}' (expected right or down)"),
    };
    Ok(match placement {
        "overlay" | "popup" => Plan::Popup,
        "side" => Plan::Split { horizontal: true },
        "bottom" => Plan::Split { horizontal: false },
        "split" => Plan::Split { horizontal },
        "tab" => Plan::Tab,
        "zoomed" => Plan::Zoomed { horizontal },
        other => anyhow::bail!(
            "unsupported placement '{other}' (expected overlay, popup, split, tab, zoomed, side or bottom)"
        ),
    })
}

/// A popup size as tmux's `-w` and `-h` take it: whole cells (`0` to `65535`) or `1%` to `100%`, the two
/// forms herdr's `PopupSize` allows.
fn popup_size(word: &str) -> anyhow::Result<String> {
    let ok = match word.strip_suffix('%') {
        Some(percent) => percent
            .parse::<u32>()
            .is_ok_and(|n| (1..=100).contains(&n) && !percent.starts_with('+')),
        None => word.parse::<u16>().is_ok() && !word.starts_with('+'),
    };
    anyhow::ensure!(
        ok,
        "unknown size '{word}' (expected whole cells such as 80, or a percentage such as 80%)"
    );
    Ok(word.to_string())
}

/// Whether `placement`, `direction`, `width` and `height`, as a person gave them, are words [`open_with`] knows.
pub fn check_options(options: &OpenOptions) -> anyhow::Result<()> {
    for size in [&options.width, &options.height].into_iter().flatten() {
        popup_size(size)?;
    }
    plan(
        options.placement.as_deref().unwrap_or("overlay"),
        options.direction.as_deref(),
    )
    .map(|_| ())
}

/// [`open`] with herdr's flags ([`OpenOptions`]).
pub fn open_with(
    ctx: &PluginCtx,
    qualified: &str,
    context: &Context,
    options: &OpenOptions,
) -> anyhow::Result<String> {
    let (record, pane_def) = resolve_pane_def(ctx, qualified)?;
    let mut vars = env::build(ctx, &record, "action", context)?;
    vars.extend(options.env.iter().cloned());
    let target = options
        .target_pane
        .clone()
        .or_else(|| split_target(ctx, context));
    // herdr's `--cwd` is where the pane starts; without it, the plugin's own folder.
    let dir: &Path = options.cwd.as_deref().map_or(&record.path, Path::new);
    // herdr finds a pane's program in the plugin's folder, wherever the pane starts.
    let command = in_plugin_folder(&record.path, &pane_def.command);
    let size = (
        popup_size(options.width.as_deref().unwrap_or("80%"))?,
        popup_size(options.height.as_deref().unwrap_or("80%"))?,
    );
    let placement = options.placement.as_deref().unwrap_or(&pane_def.placement);
    let plan = plan(placement, options.direction.as_deref())
        .map_err(|e| anyhow::anyhow!("plugin '{}': pane '{}': {e}", record.id, pane_def.id))?;
    // `-d` leaves the current pane current, which is herdr's default for a pane it opens.
    let keep_focus = options.focus == Some(false);

    let pane_id = match plan {
        Plan::Popup => {
            run_popup(
                ctx,
                dir,
                &command,
                &vars,
                target.as_deref(),
                (&size.0, &size.1),
            )?;
            return Ok("popup".to_string());
        }
        Plan::Split { horizontal } | Plan::Zoomed { horizontal } => run_split(
            ctx,
            dir,
            &command,
            &vars,
            target.as_deref(),
            horizontal,
            keep_focus,
        )?,
        Plan::Tab => run_tab(ctx, dir, &command, &vars, target.as_deref(), keep_focus)?,
    };
    if matches!(plan, Plan::Zoomed { .. }) {
        ctx.tmux.run_ok(&["resize-pane", "-Z", "-t", &pane_id])?;
    }
    record_pane(ctx, &record.id, &pane_def.id, &pane_id)?;
    Ok(pane_id)
}

/// A no-op for an `"overlay"` pane (popups have no independent focus state), and when the
/// entrypoint was never opened (nothing to focus). For `"side"`/`"bottom"` with a recorded
/// pane id: selects it if it still exists; if the user closed it by hand (so the recorded
/// id is now stale), forgets the stale entry and reports `not_found` rather than a raw
/// tmux "can't find pane" error or leaving the entrypoint permanently broken.
pub fn focus(ctx: &PluginCtx, qualified: &str) -> anyhow::Result<()> {
    let (record, pane_def) = resolve_pane_def(ctx, qualified)?;
    if pane_def.placement == "overlay" {
        return Ok(());
    }
    let Some(pane_id) = lookup_pane(ctx, &record.id, &pane_def.id)? else {
        return Ok(());
    };
    if !pane_exists(ctx, &pane_id) {
        forget_pane(ctx, &record.id, &pane_def.id)?;
        return Err(PaneError::NotFound(qualified.to_string()).into());
    }
    ctx.tmux.run_ok(&["select-pane", "-t", &pane_id])
}

/// For `"overlay"`, runs `tmux display-popup -C` (closes whatever popup is open — tmux
/// supports at most one at a time). For `"side"`/`"bottom"` with a recorded pane id: kills
/// it and forgets the entry if it still exists; if the user already closed it by hand (or
/// the entrypoint was never opened), the desired end state — no such pane — already holds,
/// so this forgets any stale entry and returns `Ok(())` rather than erroring.
pub fn close(ctx: &PluginCtx, qualified: &str) -> anyhow::Result<()> {
    let (record, pane_def) = resolve_pane_def(ctx, qualified)?;
    if pane_def.placement == "overlay" {
        return ctx.tmux.run_ok(&["display-popup", "-C"]);
    }
    let Some(pane_id) = lookup_pane(ctx, &record.id, &pane_def.id)? else {
        return Ok(());
    };
    if !pane_exists(ctx, &pane_id) {
        return forget_pane(ctx, &record.id, &pane_def.id);
    }
    ctx.tmux.run_ok(&["kill-pane", "-t", &pane_id])?;
    forget_pane(ctx, &record.id, &pane_def.id)
}

/// Whether `pane_id` still resolves to a real tmux pane — the same existence check
/// `tmux::ids::resolve_*` relies on (`display-message -t <target>` errors when `<target>`
/// no longer exists).
fn pane_exists(ctx: &PluginCtx, pane_id: &str) -> bool {
    ctx.tmux
        .display(pane_id, "#{pane_id}")
        .is_ok_and(|s| !s.is_empty())
}

fn resolve_pane_def(
    ctx: &PluginCtx,
    qualified: &str,
) -> anyhow::Result<(registry::PluginRecord, manifest::PaneDef)> {
    let (plugin_id, pane_entry_id) = qualified
        .rsplit_once('.')
        .ok_or_else(|| PaneError::NotFound(qualified.to_string()))?;
    let record = registry::find(ctx.paths, plugin_id)?
        .filter(|r| r.enabled)
        .ok_or_else(|| PaneError::NotFound(qualified.to_string()))?;
    let manifest = manifest::load(&record.path.join("herdr-plugin.toml"))?;
    let pane_def = manifest
        .find_pane(pane_entry_id)
        .cloned()
        .ok_or_else(|| PaneError::NotFound(qualified.to_string()))?;
    Ok((record, pane_def))
}

/// The tmux pane id to open/split relative to: `context.pane`'s `terminal_id` (a raw tmux
/// pane id already, no resolution needed) if present, else `context.caller_pane_id`
/// resolved through `tmux::ids::resolve_pane` (accepts public or raw ids), else `None`
/// (tmux picks its own default target).
fn split_target(ctx: &PluginCtx, context: &Context) -> Option<String> {
    if let Some(terminal_id) = context
        .pane
        .as_ref()
        .and_then(|p| p.get("terminal_id"))
        .and_then(|v| v.as_str())
    {
        return Some(terminal_id.to_string());
    }
    context
        .caller_pane_id
        .as_deref()
        .and_then(|s| crate::tmux::ids::resolve_pane(ctx.tmux, s).ok())
}

/// The shell command tmux runs for a plugin pane or popup: `command` joined, behind an
/// `env -u NAME` for every name of [`env::PANE_CONTEXT_VARS`] that `vars` does not set.
///
/// tmux starts the pane or popup, so it inherits tmux's global environment, and with
/// `@agent-wizard-herdr-env` on that holds `HERDR_PANE_ID=%calling`; `-e` can set a variable but
/// never remove one. Without the prefix a plugin pane with no pane in its context would read the
/// id of the pane it runs in as the pane it acts on. The names `vars` sets still arrive through
/// `-e`. tmux does not expand formats in a shell command, and the command is quoted as before.
fn pane_command(command: &[String], vars: &[(String, String)]) -> String {
    let mut words: Vec<&str> = Vec::new();
    // An empty command stays empty: a bare `env` would print the environment.
    if !command.is_empty() {
        let unset: Vec<&str> = env::PANE_CONTEXT_VARS
            .into_iter()
            .filter(|name| !vars.iter().any(|(k, _)| k == name))
            .collect();
        if !unset.is_empty() {
            words.push("env");
            for name in unset {
                words.push("-u");
                words.push(name);
            }
        }
    }
    words.extend(command.iter().map(String::as_str));
    shell_words::join(words)
}

/// `command` with its program found as herdr finds it: a relative path with a slash in it
/// (`bin/herdr-reviewr`) names a file inside the plugin's folder `root`, so it becomes that file's
/// absolute path, and the pane may start anywhere (`--cwd`). A bare name is left for PATH, an
/// absolute path is kept, and the arguments are never touched.
fn in_plugin_folder(root: &Path, command: &[String]) -> Vec<String> {
    let mut words = command.to_vec();
    if let Some(program) = words.first_mut()
        && program.contains('/')
        && Path::new(program.as_str()).is_relative()
    {
        *program = root.join(program.as_str()).to_string_lossy().into_owned();
    }
    words
}

fn tmux_env_args(vars: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::with_capacity(vars.len() * 2);
    for (k, v) in vars {
        out.push("-e".to_string());
        out.push(format!("{k}={v}"));
    }
    out
}

/// The tmux client the overlay popup should render on: the one attached to the caller
/// pane's own session (`target`), else any attached client, else `None`.
///
/// `taw` invokes `display-popup` as a one-off `tmux` process — a key binding's `run-shell`,
/// or the CLI — with no `$TMUX`, so tmux has no "current client" to fall back on. Without an
/// explicit `-c`, tmux can resolve the popup to no drawable client and still return success,
/// so the overlay silently never appears (side/bottom `split-window` panes are unaffected:
/// they act on the pane's session and need no client). Naming the client makes the overlay
/// land on the caller's screen. `None` only when the server has no attached client at all —
/// then there is genuinely nowhere to draw and tmux's own error stands.
fn resolve_popup_client(ctx: &PluginCtx, target: Option<&str>) -> Option<String> {
    let clients = ctx.tmux.list_clients().ok()?;
    // Prefer the client viewing the caller pane's session — the screen the user is on.
    if let Some(t) = target
        && let Ok(session) = ctx.tmux.display(t, "#{session_id}")
        && let Some(c) = clients.iter().find(|c| c.session_id == session)
    {
        return Some(c.client_name.clone());
    }
    // Otherwise any attached client: a screen to draw on, just not provably the caller's.
    clients.first().map(|c| c.client_name.clone())
}

/// The `display-popup` argv for an overlay pane, pulled out of [`run_popup`] so its shape —
/// crucially the `-c <client>` that makes the overlay actually render on the caller's
/// terminal — is unit-testable without a live server. `client`/`target` are omitted when
/// `None`.
fn popup_args<'a>(
    dir: &'a str,
    joined: &'a str,
    env_args: &'a [String],
    target: Option<&'a str>,
    client: Option<&'a str>,
    (width, height): (&'a str, &'a str),
) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec!["display-popup", "-E", "-w", width, "-h", height, "-d", dir];
    if let Some(c) = client {
        args.push("-c");
        args.push(c);
    }
    if let Some(t) = target {
        args.push("-t");
        args.push(t);
    }
    for a in env_args {
        args.push(a);
    }
    args.push(joined);
    args
}

/// A failed tmux call of a plugin pane, as one plain line: tmux's own words, never the command line,
/// which holds every `--env` value the pane was given (a token handed to a pane must not end up in an
/// error, the plugin's log or a toast).
fn without_command_line(err: anyhow::Error) -> anyhow::Error {
    let reason = match err.downcast_ref::<crate::tmux::CommandFailed>() {
        Some(failed) => failed.reason(),
        // A tmux that could not be started: the system's words, which hold no argument.
        None => err.root_cause().to_string(),
    };
    crate::cli::envelope::CliError::tmux(format!("tmux could not open the plugin pane: {reason}"))
}

fn run_popup(
    ctx: &PluginCtx,
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    target: Option<&str>,
    size: (&str, &str),
) -> anyhow::Result<()> {
    // The popup's start directory, as tmux reads it back (`-d` expands formats, as `-c` does).
    let dir_str = crate::tmux::start_directory(&plugin_dir.to_string_lossy());
    let joined = pane_command(command, vars);
    let env_args = tmux_env_args(vars);
    // Without a client there is no screen to draw on, and tmux says so only after it has been given
    // the whole command line.
    let Some(client) = resolve_popup_client(ctx, target) else {
        return Err(crate::cli::envelope::CliError::tmux(
            "no attached client to open the popup on: attach one, or open the pane as a split with --placement split",
        ));
    };
    let args = popup_args(&dir_str, &joined, &env_args, target, Some(&client), size);
    ctx.tmux.run_ok(&args).map_err(without_command_line)
}

fn run_split(
    ctx: &PluginCtx,
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    target: Option<&str>,
    horizontal: bool,
    keep_focus: bool,
) -> anyhow::Result<String> {
    let dir_str = crate::tmux::start_directory(&plugin_dir.to_string_lossy());
    let joined = pane_command(command, vars);
    let env_args = tmux_env_args(vars);
    let mut args: Vec<&str> = vec![
        "split-window",
        if horizontal { "-h" } else { "-v" },
        "-l",
        "40%",
        "-c",
        &dir_str,
        "-P",
        "-F",
        "#{pane_id}",
    ];
    if keep_focus {
        args.push("-d");
    }
    if let Some(t) = target {
        args.push("-t");
        args.push(t);
    }
    for a in &env_args {
        args.push(a);
    }
    args.push(&joined);
    ctx.tmux.run(&args).map_err(without_command_line)
}

/// A plugin pane as a window of its own: after the target's window, so that no index is taken.
fn run_tab(
    ctx: &PluginCtx,
    plugin_dir: &Path,
    command: &[String],
    vars: &[(String, String)],
    target: Option<&str>,
    keep_focus: bool,
) -> anyhow::Result<String> {
    let dir_str = crate::tmux::start_directory(&plugin_dir.to_string_lossy());
    let joined = pane_command(command, vars);
    let env_args = tmux_env_args(vars);
    // `new-window -t` takes a window, so a pane target becomes its window.
    let window = match target {
        Some(pane) => Some(ctx.tmux.display(pane, "#{window_id}")?.trim().to_string()),
        None => None,
    };
    let mut args: Vec<&str> = vec!["new-window", "-a", "-c", &dir_str, "-P", "-F", "#{pane_id}"];
    if keep_focus {
        args.push("-d");
    }
    if let Some(w) = window.as_deref() {
        args.push("-t");
        args.push(w);
    }
    for a in &env_args {
        args.push(a);
    }
    args.push(&joined);
    ctx.tmux.run(&args).map_err(without_command_line)
}

fn panes_state_path(ctx: &PluginCtx, plugin_id: &str) -> PathBuf {
    ctx.paths
        .plugin_state_dir
        .join(plugin_id)
        .join("panes.json")
}

fn load_panes(ctx: &PluginCtx, plugin_id: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let path = panes_state_path(ctx, plugin_id);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_panes(
    ctx: &PluginCtx,
    plugin_id: &str,
    map: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let path = panes_state_path(ctx, plugin_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(map).context("serializing panes.json")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn lookup_pane(ctx: &PluginCtx, plugin_id: &str, entry_id: &str) -> anyhow::Result<Option<String>> {
    Ok(load_panes(ctx, plugin_id)?.get(entry_id).cloned())
}

fn record_pane(
    ctx: &PluginCtx,
    plugin_id: &str,
    entry_id: &str,
    pane_id: &str,
) -> anyhow::Result<()> {
    let mut map = load_panes(ctx, plugin_id)?;
    map.insert(entry_id.to_string(), pane_id.to_string());
    save_panes(ctx, plugin_id, &map)
}

fn forget_pane(ctx: &PluginCtx, plugin_id: &str, entry_id: &str) -> anyhow::Result<()> {
    let mut map = load_panes(ctx, plugin_id)?;
    map.remove(entry_id);
    save_panes(ctx, plugin_id, &map)
}

#[cfg(test)]
mod tests {
    #[test]
    fn plan_maps_herdrs_placements_onto_tmux_and_keeps_taws_older_words() {
        use super::{Plan, plan};
        assert_eq!(plan("overlay", None).unwrap(), Plan::Popup);
        assert_eq!(plan("popup", None).unwrap(), Plan::Popup);
        assert_eq!(
            plan("side", None).unwrap(),
            Plan::Split { horizontal: true }
        );
        assert_eq!(
            plan("bottom", None).unwrap(),
            Plan::Split { horizontal: false }
        );
        assert_eq!(
            plan("split", None).unwrap(),
            Plan::Split { horizontal: true }
        );
        assert_eq!(
            plan("split", Some("down")).unwrap(),
            Plan::Split { horizontal: false }
        );
        assert_eq!(plan("tab", None).unwrap(), Plan::Tab);
        assert_eq!(
            plan("zoomed", Some("right")).unwrap(),
            Plan::Zoomed { horizontal: true }
        );
        assert!(plan("split", Some("left")).is_err());
        assert!(plan("floating", None).is_err());
    }

    use super::*;

    #[test]
    fn split_target_prefers_context_pane_terminal_id_over_caller_pane_id() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-panes-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let context = Context {
            pane: Some(serde_json::json!({"terminal_id": "%9", "pane_id": "w1:p9"})),
            caller_pane_id: Some("%5".to_string()),
            ..Context::default()
        };
        assert_eq!(split_target(&ctx, &context).as_deref(), Some("%9"));
    }

    #[test]
    fn split_target_is_none_without_pane_or_caller_pane_id() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-panes-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        assert_eq!(split_target(&ctx, &Context::default()), None);
    }

    #[test]
    fn tmux_env_args_interleaves_dash_e_flags() {
        let vars = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        assert_eq!(tmux_env_args(&vars), vec!["-e", "A=1", "-e", "B=2"]);
    }

    #[test]
    fn popup_args_names_the_client_so_the_overlay_renders() {
        let env_args = tmux_env_args(&[("HERDR_X".to_string(), "1".to_string())]);
        let args = popup_args(
            "/plug",
            "sh -c board",
            &env_args,
            Some("%3"),
            Some("/dev/ttys7"),
            ("80%", "80%"),
        );

        // The overlay only renders when a real client is named: `-c <client>` must be
        // present, carry the resolved client, and precede `-t` (the order tmux accepts).
        let ci = args
            .iter()
            .position(|a| *a == "-c")
            .expect("popup must target a client with -c");
        assert_eq!(args[ci + 1], "/dev/ttys7");
        let ti = args
            .iter()
            .position(|a| *a == "-t")
            .expect("popup keeps its -t target pane");
        assert_eq!(args[ti + 1], "%3");
        assert!(ci < ti, "-c comes before -t: {args:?}");

        // Overall shape is unchanged: display-popup -E …, env interleaved, command last.
        assert_eq!(&args[..2], &["display-popup", "-E"]);
        assert!(
            args.contains(&"-e") && args.contains(&"HERDR_X=1"),
            "{args:?}"
        );
        assert_eq!(*args.last().unwrap(), "sh -c board");
    }

    /// A plugin's folder is the start directory of its popup (`-d`) and of its split (`-c`), where tmux
    /// expands formats and ends a command at a word that ends in `;`: both are given as tmux reads them
    /// back (`tmux::start_directory`).
    #[test]
    fn a_plugins_folder_is_given_to_tmux_as_the_folder_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$*\" in *list-clients*) printf '/dev/ttys9\\t/dev/ttys9\\t$1\\ts\\n';; *) echo '%9';; esac\n",
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let tmux = crate::tmux::Tmux {
            bin,
            socket: Some(crate::tmux::Socket::Name("s".to_string())),
        };
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let plugin_dir = std::path::Path::new("/plugins/C#Sharp;");
        let command = ["board".to_string()];
        let after = |flag: &str| -> Vec<String> {
            let calls: Vec<String> = std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            std::fs::remove_file(&log).unwrap();
            let at = calls
                .iter()
                .position(|word| word == flag)
                .unwrap_or_else(|| panic!("no {flag} in {calls:?}"));
            calls[at + 1..at + 2].to_vec()
        };

        run_split(&ctx, plugin_dir, &command, &[], Some("%3"), true, false).unwrap();
        assert_eq!(after("-c"), ["/plugins/C##Sharp\\;"]);
        run_popup(&ctx, plugin_dir, &command, &[], Some("%3"), ("80%", "80%")).unwrap();
        assert_eq!(after("-d"), ["/plugins/C##Sharp\\;"]);
    }

    /// Runs `pane_command(command, vars)` as tmux does (`sh -c <string>`), in an environment that holds
    /// what tmux's global environment holds with the setting on, plus what `-e` adds for `vars`.
    fn run_pane_command(command: &[&str], vars: &[(&str, &str)]) -> String {
        let command: Vec<String> = command.iter().map(|w| w.to_string()).collect();
        let owned: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(pane_command(&command, &owned))
            .env("HERDR_PANE_ID", "%calling")
            .env("HERDR_TAB_ID", "inherited")
            .env("TAW_WORKSPACE_ID", "inherited")
            .envs(vars.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    const SHOW: &str = "printf '%s|%s|%s|%s' \"${HERDR_PANE_ID-unset}\" \"${HERDR_TAB_ID-unset}\" \
         \"${TAW_WORKSPACE_ID-unset}\" \"${TAW_PANE_ID-unset}\"";

    #[test]
    fn a_pane_command_unsets_each_context_name_the_invocation_does_not_set() {
        assert_eq!(
            run_pane_command(&["sh", "-c", SHOW], &[]),
            "unset|unset|unset|unset"
        );
        // The names it does set keep their values, and only the others go; a value with a space, `#`,
        // `$` and a quote arrives whole.
        let hard = "a b #S $HOME 'q' \"d\" ;";
        assert_eq!(
            run_pane_command(
                &["sh", "-c", SHOW],
                &[("HERDR_PANE_ID", hard), ("TAW_PANE_ID", "p_1")]
            ),
            format!("{hard}|unset|unset|p_1")
        );
        // The command's own words are not touched: a space, a `#` and a `$` in an argument.
        assert_eq!(
            run_pane_command(&["printf", "%s", "x y #S $HOME"], &[]),
            "x y #S $HOME"
        );
    }

    #[test]
    fn a_pane_command_is_the_joined_command_for_an_empty_one_and_never_a_bare_env() {
        assert_eq!(pane_command(&[], &[]), "");
        let all: Vec<(String, String)> = env::PANE_CONTEXT_VARS
            .iter()
            .map(|n| (n.to_string(), "v".to_string()))
            .collect();
        assert_eq!(
            pane_command(&["board".to_string()], &all),
            "board",
            "nothing to unset, no prefix"
        );
    }

    /// A program named by a path inside the plugin is found in the plugin's folder wherever the pane
    /// starts; a bare name is left to PATH, and an absolute path and the arguments stay as they are.
    #[test]
    fn a_relative_program_path_is_found_in_the_plugins_folder() {
        let root = Path::new("/plugins/reviewr");
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(
            in_plugin_folder(root, &words(&["bin/herdr-reviewr", "bin/arg"])),
            words(&["/plugins/reviewr/bin/herdr-reviewr", "bin/arg"])
        );
        assert_eq!(
            in_plugin_folder(root, &words(&["./run.sh"])),
            words(&["/plugins/reviewr/./run.sh"])
        );
        assert_eq!(
            in_plugin_folder(root, &words(&["sh", "-c", "bin/x"])),
            words(&["sh", "-c", "bin/x"])
        );
        assert_eq!(
            in_plugin_folder(root, &words(&["/usr/bin/env", "x"])),
            words(&["/usr/bin/env", "x"])
        );
        assert!(in_plugin_folder(root, &[]).is_empty());
    }

    /// Both placements hand tmux the prefixed command as the last word, with the names the context sets
    /// left to `-e`.
    #[test]
    fn a_split_and_a_popup_run_the_command_behind_the_unset() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$*\" in *list-clients*) printf '/dev/ttys9\\t/dev/ttys9\\t$1\\ts\\n';; *) echo '%9';; esac\n",
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let tmux = crate::tmux::Tmux {
            bin,
            socket: Some(crate::tmux::Socket::Name("s".to_string())),
        };
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };
        let command = ["board".to_string(), "a b".to_string()];
        let vars = vec![("HERDR_PANE_ID".to_string(), "p_1".to_string())];
        let last = || -> String {
            let calls = std::fs::read_to_string(&log).unwrap();
            std::fs::remove_file(&log).unwrap();
            // The recorder's last line is the command (the "-e" words come before it).
            calls.lines().last().unwrap().to_string()
        };
        let want = "env -u HERDR_TAB_ID -u HERDR_WORKSPACE_ID -u TAW_PANE_ID -u TAW_TAB_ID \
                    -u TAW_WORKSPACE_ID board 'a b'";
        run_split(
            &ctx,
            Path::new("/p"),
            &command,
            &vars,
            Some("%3"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(last(), want);
        run_popup(
            &ctx,
            Path::new("/p"),
            &command,
            &vars,
            Some("%3"),
            ("80%", "80%"),
        )
        .unwrap();
        assert_eq!(last(), want);
    }

    /// herdr's `PopupSize`: whole cells or `1%` to `100%`; the size reaches tmux's `-w` and `-h`.
    #[test]
    fn a_popup_size_is_cells_or_a_percentage_and_reaches_tmux() {
        for good in ["0", "20", "65535", "1%", "80%", "100%"] {
            assert_eq!(popup_size(good).unwrap(), good);
        }
        for bad in [
            "", "%", "0%", "101%", "65536", "-1", "+5", "8 0", "wide", "1.5", "80%%",
        ] {
            assert!(popup_size(bad).is_err(), "{bad:?} was accepted");
        }
        let args = popup_args("/plug", "board", &[], None, None, ("60", "40%"));
        let after = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
        assert_eq!((after("-w"), after("-h")), ("60", "40%"));
    }

    /// A failed tmux call reads as one line of tmux's own words, with none of the command line (which
    /// holds each `--env` value).
    #[test]
    fn a_failed_tmux_call_is_one_line_without_the_command_line() {
        let status = std::process::Command::new("false").status().unwrap();
        let failed = crate::tmux::CommandFailed {
            command: "tmux split-window -e SECRET_TOKEN=abc123 board".to_string(),
            status,
            stderr: "no space for new pane\nmore".to_string(),
        };
        let text = format!("{:#}", without_command_line(failed.into()));
        assert_eq!(
            text,
            "tmux could not open the plugin pane: no space for new pane"
        );
        let spawn: anyhow::Error = anyhow::Error::new(std::io::Error::other("no such file"))
            .context("failed to run `tmux -e SECRET_TOKEN=abc123`");
        let text = format!("{:#}", without_command_line(spawn));
        assert!(!text.contains("abc123"), "{text}");
    }

    #[test]
    fn popup_args_omits_client_and_target_when_none_resolved() {
        let args = popup_args("/plug", "board", &[], None, None, ("80%", "80%"));
        assert!(
            !args.contains(&"-c"),
            "no client target when none is resolved: {args:?}"
        );
        assert!(!args.contains(&"-t"), "{args:?}");
        assert_eq!(*args.last().unwrap(), "board");
    }

    #[test]
    fn open_errors_not_found_for_an_unknown_plugin_pane() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-panes-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        let err = open(&ctx, "nope.board", &Context::default()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<PaneError>(),
            Some(&PaneError::NotFound("nope.board".to_string()))
        );
    }

    #[test]
    fn pane_state_round_trips_through_record_lookup_and_forget() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-panes-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let ctx = PluginCtx {
            tmux: &tmux,
            cfg: &cfg,
            paths: &paths,
            taw_bin: &taw_bin,
            server_env: None,
        };

        assert_eq!(lookup_pane(&ctx, "echo-plugin", "board").unwrap(), None);
        record_pane(&ctx, "echo-plugin", "board", "%42").unwrap();
        assert_eq!(
            lookup_pane(&ctx, "echo-plugin", "board").unwrap(),
            Some("%42".to_string())
        );
        forget_pane(&ctx, "echo-plugin", "board").unwrap();
        assert_eq!(lookup_pane(&ctx, "echo-plugin", "board").unwrap(), None);
    }
}
