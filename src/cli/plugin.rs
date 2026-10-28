//! `taw plugin …` — the herdr-compatible plugin system, over `plugin::*`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::{self, PluginCtx, actions, context, panes, registry};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Install a plugin from GitHub
    Install {
        #[arg(value_name = "OWNER/REPO[/SUBDIR]")]
        spec: String,
        #[arg(long = "ref", value_name = "REF")]
        git_ref: Option<String>,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Update installed plugins from GitHub
    ///
    /// With no argument, every installed plugin; an argument is a plugin id or the source
    /// it was installed from (owner/repo[/subdir]). A plugin linked from a local folder is
    /// left alone. Each plugin's line says whether it was updated, was already current, is
    /// linked, or failed (and why); one failing does not stop the others, and the command
    /// then exits 1.
    Update {
        #[arg(value_name = "PLUGIN_OR_SOURCE")]
        plugins: Vec<String>,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Uninstall a plugin
    Uninstall {
        #[arg(value_name = "PLUGIN")]
        plugin: String,
    },
    /// Link a local plugin
    Link {
        #[arg(value_name = "PATH")]
        path: std::path::PathBuf,
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        enabled: bool,
    },
    /// Unlink a local plugin
    Unlink {
        #[arg(value_name = "PLUGIN_ID")]
        plugin: String,
    },
    /// Enable a plugin
    Enable {
        #[arg(value_name = "PLUGIN_ID")]
        plugin: String,
    },
    /// Disable a plugin
    Disable {
        #[arg(value_name = "PLUGIN_ID")]
        plugin: String,
    },
    /// List installed plugins
    List {
        #[arg(long, value_name = "ID")]
        plugin: Option<String>,
        /// Accepted for herdr parity; `taw` always prints JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a plugin config directory
    ConfigDir {
        #[arg(value_name = "PLUGIN_ID")]
        plugin: String,
    },
    /// Inspect plugin command logs
    #[command(alias = "logs")]
    Log {
        /// A plugin id, or the literal `list` for every plugin's log path.
        #[arg(value_name = "PLUGIN_ID")]
        plugin: String,
        /// With `list`: only this plugin's log, as herdr's `plugin log list --plugin ID`.
        #[arg(long = "plugin", value_name = "ID")]
        only: Option<String>,
    },
    /// List or invoke plugin actions
    Action {
        #[command(subcommand)]
        cmd: ActionCmd,
    },
    /// Manage plugin-owned panes
    Pane {
        #[command(subcommand)]
        cmd: PaneCmd,
    },
    /// Run every enabled plugin's startup commands
    Startup,
    /// Fire one lifecycle event at every plugin that hooks it
    ///
    /// Called by the tmux hooks `tmux-agent-wizard.tmux` installs, which are the only
    /// place a change the user made outside `taw` (`prefix c`, `prefix ,`, a manual
    /// `kill-window`) is observable. Every other event is emitted by the verb that causes
    /// it.
    Event {
        /// An event name, such as `tab.created`; `taw hooks list` shows the names
        #[arg(value_name = "EVENT")]
        name: String,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long, value_name = "ID")]
        tab: Option<String>,
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        /// The new name, for `tab.renamed` and `workspace.renamed`
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ActionCmd {
    /// List plugin actions
    List {
        /// Only this plugin's actions.
        #[arg(long, value_name = "ID")]
        plugin: Option<String>,
    },
    /// Invoke a plugin action
    Invoke {
        /// `PLUGIN.ACTION`, or just the action's own id with --plugin (herdr's form).
        #[arg(value_name = "PLUGIN.ACTION")]
        action: String,
        /// The plugin whose action it is (herdr's form: `invoke open --plugin persiyanov.reviewr`).
        #[arg(long, value_name = "ID")]
        plugin: Option<String>,
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "ID")]
        tab: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
    },
}

// `Open` holds herdr's whole list of flags; the value is parsed once and moved, never copied about.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum PaneCmd {
    /// Open a plugin pane
    ///
    /// Name the pane as PLUGIN.PANE, or as herdr does with --plugin and --entrypoint.
    Open {
        #[arg(
            value_name = "PLUGIN.PANE",
            conflicts_with_all = ["plugin", "entrypoint"],
            required_unless_present_all = ["plugin", "entrypoint"]
        )]
        pane_ref: Option<String>,
        /// The plugin id (herdr's form, with --entrypoint).
        #[arg(long, value_name = "ID", requires = "entrypoint")]
        plugin: Option<String>,
        /// The pane entrypoint of that plugin.
        #[arg(long, value_name = "ID", requires = "plugin")]
        entrypoint: Option<String>,
        /// Where it opens: overlay, popup, split, tab or zoomed (the manifest's by default).
        #[arg(long, value_name = "PLACEMENT")]
        placement: Option<String>,
        /// For a split: right or down.
        #[arg(long, value_name = "DIRECTION")]
        direction: Option<String>,
        /// Place it beside this pane (herdr's --target-pane), over the pane the context names.
        #[arg(long, value_name = "ID")]
        target_pane: Option<String>,
        /// The folder the pane starts in, over the plugin's own.
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        /// A popup's width, in cells or as a percentage such as 80%.
        #[arg(long, value_name = "SIZE")]
        width: Option<String>,
        /// A popup's height, in cells or as a percentage such as 80%.
        #[arg(long, value_name = "SIZE")]
        height: Option<String>,
        /// A variable for the pane, KEY=VALUE; repeat it for more.
        #[arg(long = "env", value_name = "KEY=VALUE", allow_hyphen_values = true)]
        env: Vec<String>,
        /// Focus the new pane (herdr leaves it unfocused unless asked).
        #[arg(long)]
        focus: bool,
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long, value_name = "ID")]
        tab: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
    },
    /// Focus a plugin pane
    Focus {
        #[arg(value_name = "PLUGIN.PANE")]
        pane_ref: String,
    },
    /// Close a plugin pane
    Close {
        #[arg(value_name = "PLUGIN.PANE")]
        pane_ref: String,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    let pctx = PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    match cmd {
        Cmd::Install { spec, git_ref, yes } => {
            ("install", install(&pctx, &spec, git_ref.as_deref(), yes))
        }
        // `dispatch` routes `update` to [`update`] for its exit status; this arm is the same
        // call for any other caller.
        Cmd::Update { plugins, yes } => ("update", update(ctx, &plugins, yes).map(|(v, _)| v)),
        Cmd::Uninstall { plugin } => ("uninstall", uninstall(&pctx, &plugin)),
        Cmd::Link {
            path,
            disabled,
            enabled,
        } => ("link", link(&pctx, &path, !disabled || enabled)),
        Cmd::Unlink { plugin } => ("unlink", unlink(&pctx, &plugin)),
        Cmd::Enable { plugin } => ("enable", set_enabled(&pctx, &plugin, true)),
        Cmd::Disable { plugin } => ("disable", set_enabled(&pctx, &plugin, false)),
        Cmd::List { plugin, .. } => ("list", list(&pctx, plugin.as_deref())),
        Cmd::ConfigDir { plugin } => ("config-dir", config_dir(&pctx, &plugin)),
        Cmd::Log { plugin, only } => ("log", log(&pctx, &plugin, only.as_deref())),
        Cmd::Action { cmd } => match cmd {
            ActionCmd::List { plugin } => ("action-list", action_list(&pctx, plugin.as_deref())),
            ActionCmd::Invoke {
                action,
                plugin,
                workspace,
                tab,
                pane,
            } => (
                "action-invoke",
                action_invoke(
                    &pctx,
                    &qualified_action(&action, plugin.as_deref()),
                    workspace.as_deref(),
                    tab.as_deref(),
                    pane.as_deref(),
                ),
            ),
        },
        Cmd::Pane { cmd } => match cmd {
            PaneCmd::Open {
                pane_ref,
                plugin,
                entrypoint,
                placement,
                direction,
                target_pane,
                cwd,
                width,
                height,
                env,
                focus,
                workspace,
                tab,
                pane,
            } => (
                "pane-open",
                pane_open(
                    &pctx,
                    &pane_ref,
                    plugin.zip(entrypoint),
                    &panes::OpenOptions {
                        placement,
                        direction,
                        cwd,
                        width,
                        height,
                        target_pane,
                        ..panes::OpenOptions::default()
                    },
                    &env,
                    focus,
                    workspace.as_deref(),
                    tab.as_deref(),
                    pane.as_deref(),
                ),
            ),
            PaneCmd::Focus { pane_ref } => ("pane-focus", pane_focus(&pctx, &pane_ref)),
            PaneCmd::Close { pane_ref } => ("pane-close", pane_close(&pctx, &pane_ref)),
        },
        Cmd::Startup => ("startup", startup(&pctx)),
        Cmd::Event {
            name,
            pane,
            tab,
            workspace,
            label,
        } => (
            super::events::SUB,
            super::events::emit(
                ctx,
                &name,
                super::events::Ids {
                    pane: pane.as_deref(),
                    tab: tab.as_deref(),
                    workspace: workspace.as_deref(),
                    label: label.as_deref(),
                },
            ),
        ),
    }
}

/// The registry record as herdr's `{id, source:{kind,…}, path, enabled,
/// installed_unix_ms}`. `source` is serialized whole, so a GitHub
/// install keeps its owner/repo/subdir/ref/commit rather than collapsing to a label.
fn record_json(record: &registry::PluginRecord) -> Value {
    json!({
        "id": record.id,
        "source": record.source,
        "path": record.path,
        "enabled": record.enabled,
        "installed_unix_ms": record.installed_unix_ms,
    })
}

/// herdr accepts the ref either inline (`owner/repo@ref`) or as `--ref`; `parse_spec`
/// only understands the inline form, so a `--ref` is appended — unless the spec already
/// pins one, in which case the explicit flag would be ambiguous.
fn install(
    pctx: &PluginCtx,
    spec: &str,
    git_ref: Option<&str>,
    yes: bool,
) -> anyhow::Result<Value> {
    let spec = match git_ref {
        Some(r) if spec.contains('@') => {
            return Err(CliError::invalid_argument(format!(
                "'{spec}' already pins a ref; drop --ref {r}"
            )));
        }
        Some(r) => format!("{spec}@{r}"),
        None => spec.to_string(),
    };
    let record = plugin::install(pctx, &spec, yes)?;
    Ok(json!({ "plugin": record_json(&record) }))
}

/// `taw plugin update`: the envelope of [`update_envelope`] and its exit status. An
/// argument that names no installed plugin is `not_found`, found before anything is touched.
pub(crate) fn update(ctx: &Ctx, args: &[String], yes: bool) -> anyhow::Result<(Value, i32)> {
    let pctx = PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    let not_found = |err: anyhow::Error| match err.downcast_ref::<plugin::update::NotFound>() {
        Some(missing) => CliError::not_found(missing.to_string()),
        None => err,
    };
    plugin::update::select(pctx.paths, args).map_err(not_found)?;
    let lines = plugin::update::update(&pctx, args, yes).map_err(not_found)?;
    Ok(update_envelope(lines))
}

/// One line per plugin: `{id, status}` with `status` one of `updated` (`from`, `to`, and a `note` when the old copy would not go),
/// `unchanged` (`commit`), `linked`, or `failed` (`code`, `reason`). Every line is listed
/// either way; the status is 1 when any plugin failed, as `taw machine check --json` does
/// when a machine did not connect, and 0 otherwise (linked and unchanged are not failures).
fn update_envelope(lines: Vec<plugin::update::PluginUpdate>) -> (Value, i32) {
    use plugin::update::Outcome;
    let mut code = 0;
    let plugins: Vec<Value> = lines
        .into_iter()
        .map(|line| match line.outcome {
            Outcome::Updated { from, to, note } => {
                let mut value =
                    json!({ "id": line.id, "status": "updated", "from": from, "to": to });
                if let Some(note) = note {
                    value["note"] = json!(note);
                }
                value
            }
            Outcome::Unchanged { commit } => {
                json!({ "id": line.id, "status": "unchanged", "commit": commit })
            }
            Outcome::Linked => json!({ "id": line.id, "status": "linked" }),
            Outcome::Failed { code: why, reason } => {
                code = 1;
                json!({ "id": line.id, "status": "failed", "code": why, "reason": reason })
            }
        })
        .collect();
    (json!({ "plugins": plugins }), code)
}

/// Registry operations name a plugin that may not exist; `not_found` is the right code
/// for that, and the underlying error text is preserved in the message.
fn registry_op(id: &str, result: anyhow::Result<()>) -> anyhow::Result<()> {
    result.map_err(|e| {
        let text = e.to_string();
        if text.contains("not found") || text.contains("no plugin") {
            CliError::not_found(format!("plugin '{id}' not found"))
        } else {
            e
        }
    })
}

fn uninstall(pctx: &PluginCtx, id: &str) -> anyhow::Result<Value> {
    registry_op(id, plugin::uninstall(pctx, id))?;
    Ok(json!({ "id": id }))
}

fn link(pctx: &PluginCtx, path: &std::path::Path, enabled: bool) -> anyhow::Result<Value> {
    let record = plugin::link(pctx, path, enabled)?;
    Ok(json!({ "plugin": record_json(&record) }))
}

fn unlink(pctx: &PluginCtx, id: &str) -> anyhow::Result<Value> {
    // `registry::remove` (behind `plugin::unlink`) is idempotent — a no-op for an unknown
    // id — so without an explicit existence check `plugin unlink <unknown>` reported success
    // for a plugin that was never linked. Confirm it exists first, matching the `not_found`
    // that `plugin get`/`enable`/`disable`/`uninstall` already report.
    if registry::find(pctx.paths, id)?.is_none() {
        return Err(CliError::not_found(format!("plugin '{id}' not found")));
    }
    registry_op(id, plugin::unlink(pctx, id))?;
    Ok(json!({ "id": id }))
}

fn set_enabled(pctx: &PluginCtx, id: &str, enabled: bool) -> anyhow::Result<Value> {
    registry_op(id, plugin::set_enabled(pctx, id, enabled))?;
    Ok(json!({ "id": id, "enabled": enabled }))
}

fn list(pctx: &PluginCtx, filter: Option<&str>) -> anyhow::Result<Value> {
    let plugins: Vec<Value> = plugin::list(pctx)?
        .iter()
        .filter(|p| filter.is_none_or(|id| p.id == id))
        .map(record_json)
        .collect();
    Ok(json!({ "plugins": plugins }))
}

fn config_dir(pctx: &PluginCtx, id: &str) -> anyhow::Result<Value> {
    let dir = plugin::config_dir(pctx, id)?;
    Ok(json!({ "id": id, "path": dir }))
}

/// `plugin log <id>` reports one plugin's log path. herdr spells the listing form
/// `plugin log list`, so the literal id `list` is treated as that listing — a plugin
/// genuinely called `list` would collide, which is why the shape is documented here.
fn log(pctx: &PluginCtx, id: &str, only: Option<&str>) -> anyhow::Result<Value> {
    if id == "list" {
        let logs: Vec<Value> = plugin::list(pctx)?
            .iter()
            .filter(|p| only.is_none_or(|only| p.id == only))
            .map(|p| {
                json!({
                    "id": p.id,
                    "path": plugin::log_path(pctx, &p.id),
                })
            })
            .collect();
        return Ok(json!({ "logs": logs }));
    }
    let path = plugin::log_path(pctx, id);
    Ok(json!({
        "id": id,
        "path": path,
        "exists": path.exists(),
    }))
}

fn action_list(pctx: &PluginCtx, only: Option<&str>) -> anyhow::Result<Value> {
    let mut list = actions::list(pctx)?;
    if let Some(only) = only {
        list.retain(|action| action.plugin_id == only);
    }
    Ok(json!({ "actions": list }))
}

/// Builds the context an action or plugin pane receives, with `caller_pane_id` set from
/// `$TMUX_PANE` so a plugin can tell *who* invoked it apart from what it was aimed at.
/// The value is validated against this server first (see [`validated_caller_pane`]): a
/// `$TMUX_PANE` inherited from a different tmux server is dropped rather than exposed.
fn plugin_context(
    pctx: &PluginCtx,
    workspace: Option<&str>,
    tab: Option<&str>,
    pane: Option<&str>,
) -> anyhow::Result<plugin::Context> {
    // `--pane` takes every word a pane verb does, `%calling` among them: one rule, the resolver's.
    let pane = pane
        .map(|word| common::resolve_pane(pctx.tmux, word))
        .transpose()?;
    let mut context = context::from_ids(pctx, workspace, tab, pane.as_deref())?;
    context.caller_pane_id = validated_caller_pane(pctx.tmux, common::current_pane().ok());
    Ok(context)
}

/// The caller pane id to expose in a plugin context: `$TMUX_PANE` verbatim, but only when
/// it names a pane that exists on the tmux server taw is actually addressing.
///
/// `$TMUX_PANE` is tmux's own per-pane variable, set by whichever server owns the pane the
/// invoking process sits in. That is almost always the server taw then talks to — but not
/// guaranteed: under a `TAW_TMUX_SOCKET_*` override, or with a second running server, the
/// inherited `$TMUX_PANE` can name a pane on a *different* server. herdr sidesteps this by
/// injecting its own `$HERDR_PANE_ID` into the panes it manages and trusting that; taw
/// reuses tmux's variable, so it confirms the pane is live on this server (a cheap
/// `resolve_pane`) and drops a foreign or stale id rather than handing it to a plugin or
/// into `split_target`. The happy path — a real pane on this server — is unchanged.
fn validated_caller_pane(t: &crate::tmux::Tmux, pane: Option<String>) -> Option<String> {
    pane.filter(|p| common::resolve_pane(t, p).is_ok())
}

/// The pane an action's context takes when its caller names no pane, as herdr fills a missing context
/// from the active workspace, tab and focused pane: the active pane of `--tab`, else of `--workspace`,
/// else the pane this command runs in, else the active pane of the session tmux takes as current. The
/// tab and workspace then follow from that pane, and what the caller named is kept. `None` when the
/// caller named a pane, or tmux has no such pane (a wrong `--tab` is then reported as before).
fn pane_in_front(
    pctx: &PluginCtx,
    workspace: Option<&str>,
    tab: Option<&str>,
    pane: Option<&str>,
) -> Option<String> {
    if pane.is_some() {
        return None;
    }
    let active_pane_of = |target: &str| {
        pctx.tmux
            .display(target, "#{pane_id}")
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    };
    if let Some(tab) = tab {
        return active_pane_of(&common::resolve_window(pctx.tmux, tab).ok()?);
    }
    if let Some(workspace) = workspace {
        return active_pane_of(&crate::tmux::ids::resolve_session(pctx.tmux, workspace).ok()?);
    }
    validated_caller_pane(pctx.tmux, common::current_pane().ok()).or_else(|| {
        pctx.tmux
            .run(&["display-message", "-p", "#{pane_id}"])
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    })
}

/// The action as `PLUGIN.ACTION`: herdr's `invoke ACTION --plugin PLUGIN` joins the two, and a dotted name
/// that already starts with the plugin (`invoke PLUGIN.ACTION --plugin PLUGIN`) is left as it is.
fn qualified_action(action: &str, plugin: Option<&str>) -> String {
    match plugin {
        Some(plugin) if !action.starts_with(&format!("{plugin}.")) => format!("{plugin}.{action}"),
        _ => action.to_string(),
    }
}

fn action_invoke(
    pctx: &PluginCtx,
    action: &str,
    workspace: Option<&str>,
    tab: Option<&str>,
    pane: Option<&str>,
) -> anyhow::Result<Value> {
    let in_front = pane_in_front(pctx, workspace, tab, pane);
    let context = plugin_context(pctx, workspace, tab, pane.or(in_front.as_deref()))?;
    let result = actions::invoke(pctx, action, &context)?;
    Ok(json!({
        "action": action,
        "status": result.status,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }))
}

/// Herdr's `--env KEY=VALUE`: the first `=` splits, so a value may hold more. A wrong word is named
/// by its place among the `--env` flags, never shown: it can be a secret.
fn parse_env(words: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    words
        .iter()
        .enumerate()
        .map(|(at, word)| {
            let wrong = |what: &str| {
                CliError::invalid_argument(format!(
                    "--env takes KEY=VALUE, and --env number {} {what}",
                    at + 1
                ))
            };
            match word.split_once('=') {
                None => Err(wrong("has no '='")),
                Some(("", _)) => Err(wrong("has no name before its '='")),
                Some((key, value)) => Ok((key.to_string(), value.to_string())),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn pane_open(
    pctx: &PluginCtx,
    pane_ref: &Option<String>,
    named: Option<(String, String)>,
    options: &panes::OpenOptions,
    env: &[String],
    focus: bool,
    workspace: Option<&str>,
    tab: Option<&str>,
    pane: Option<&str>,
) -> anyhow::Result<Value> {
    // The positional `PLUGIN.PANE`, or herdr's `--plugin ID --entrypoint ENTRY`.
    let flag_form = named.is_some();
    let pane_ref = match (pane_ref, named) {
        (Some(pane_ref), _) => pane_ref.clone(),
        (None, Some((plugin, entrypoint))) => format!("{plugin}.{entrypoint}"),
        (None, None) => {
            return Err(CliError::invalid_argument(
                "name the pane as PLUGIN.PANE, or with --plugin and --entrypoint",
            ));
        }
    };
    // herdr's CLI expands a relative `--cwd`; tmux would read it against the server's own folder.
    let cwd = options
        .cwd
        .as_deref()
        .map(|word| {
            std::path::absolute(word)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(|e| CliError::invalid_argument(format!("--cwd '{word}': {e}")))
        })
        .transpose()?;
    let target_pane = options
        .target_pane
        .as_deref()
        .map(|word| common::resolve_pane(pctx.tmux, word))
        .transpose()?;
    let options = panes::OpenOptions {
        cwd,
        target_pane,
        env: parse_env(env)?,
        // herdr's flag form leaves the pane unfocused unless `--focus`; the positional form is as it was.
        focus: flag_form.then_some(focus),
        ..options.clone()
    };
    panes::check_options(&options).map_err(|e| CliError::invalid_argument(e.to_string()))?;
    let context = plugin_context(pctx, workspace, tab, pane)?;
    let opened = panes::open_with(pctx, &pane_ref, &context, &options)?;
    // herdr's `PluginPaneInfo`: `{plugin_id, entrypoint, pane}`. A popup is no pane (herdr: "it has no
    // pane ID"), so its `pane` is null.
    let (plugin_id, entrypoint) = pane_ref.rsplit_once('.').unwrap_or((&pane_ref, ""));
    let pane_record = pctx
        .tmux
        .list_panes()?
        .iter()
        .find(|p| p.pane_id == opened)
        .map(context::pane_json)
        .unwrap_or(Value::Null);
    Ok(json!({
        "plugin_pane": {
            "plugin_id": plugin_id,
            "entrypoint": entrypoint,
            "pane": pane_record,
        },
        // taw's own fields, from before the herdr shape: the name asked for and the tmux pane id
        // (the literal "popup" for a popup, which has none).
        "pane": pane_ref,
        "terminal_id": opened,
    }))
}

fn pane_focus(pctx: &PluginCtx, pane_ref: &str) -> anyhow::Result<Value> {
    panes::focus(pctx, pane_ref)?;
    Ok(json!({ "pane": pane_ref }))
}

fn pane_close(pctx: &PluginCtx, pane_ref: &str) -> anyhow::Result<Value> {
    panes::close(pctx, pane_ref)?;
    Ok(json!({ "pane": pane_ref }))
}

fn startup(pctx: &PluginCtx) -> anyhow::Result<Value> {
    let started = plugin::startup(pctx)?;
    Ok(json!({ "started": started }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::Tmux;

    /// A wrong `--env` word is named by its place among the `--env` flags and what is wrong with
    /// it, never by the word: it can be a secret (`--env "$TOKEN"`, or `--env "$NAME=$TOKEN"` with
    /// `NAME` unset). A word that starts with `-` is taken as the flag's value, not refused by
    /// the parser with the word in its message.
    #[test]
    fn a_wrong_env_word_is_named_by_its_place_and_never_shown() {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let message = |list: &[&str]| parse_env(&words(list)).unwrap_err().to_string();
        let no_equals = message(&["A=1", "s3cr3t-token"]);
        assert_eq!(
            no_equals,
            "--env takes KEY=VALUE, and --env number 2 has no '='"
        );
        let no_name = message(&["=s3cr3t-token"]);
        assert_eq!(
            no_name,
            "--env takes KEY=VALUE, and --env number 1 has no name before its '='"
        );
        for text in [&no_equals, &no_name] {
            assert!(!text.contains("s3cr3t"), "{text}");
        }
        assert_eq!(
            parse_env(&words(&["A=1", "B==x=y"])).unwrap(),
            [
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "=x=y".to_string())
            ]
        );
        let parsed = crate::cli::parse_from([
            "taw", "plugin", "pane", "open", "p.x", "--env", "-s3cr3t", "--env", "-K=v",
        ]);
        assert!(parsed.is_ok(), "{:?}", parsed.err().map(|e| e.to_string()));
    }

    #[test]
    fn an_action_named_with_plugin_is_joined_to_it_and_a_dotted_name_is_kept() {
        assert_eq!(
            qualified_action("open", Some("persiyanov.reviewr")),
            "persiyanov.reviewr.open"
        );
        assert_eq!(
            qualified_action("persiyanov.reviewr.open", Some("persiyanov.reviewr")),
            "persiyanov.reviewr.open"
        );
        assert_eq!(
            qualified_action("persiyanov.reviewr.open", None),
            "persiyanov.reviewr.open"
        );
    }

    #[test]
    fn update_envelope_names_each_outcome_and_exits_1_only_for_a_failure() {
        use plugin::update::{Outcome, PluginUpdate};
        let line = |id: &str, outcome| PluginUpdate {
            id: id.to_string(),
            outcome,
        };
        let quiet = vec![
            line(
                "a",
                Outcome::Updated {
                    from: Some("c1".to_string()),
                    to: "c2".to_string(),
                    note: None,
                },
            ),
            line(
                "b",
                Outcome::Updated {
                    from: None,
                    to: "c3".to_string(),
                    note: None,
                },
            ),
            line(
                "f",
                Outcome::Updated {
                    from: Some("c5".to_string()),
                    to: "c6".to_string(),
                    note: Some("could not remove the old copy".to_string()),
                },
            ),
            line(
                "c",
                Outcome::Unchanged {
                    commit: "c4".to_string(),
                },
            ),
            line("d", Outcome::Linked),
        ];
        let (fields, code) = update_envelope(quiet.clone());
        assert_eq!(code, 0, "updated, unchanged and linked are not failures");
        assert_eq!(
            fields,
            json!({ "plugins": [
                { "id": "a", "status": "updated", "from": "c1", "to": "c2" },
                { "id": "b", "status": "updated", "from": null, "to": "c3" },
                { "id": "f", "status": "updated", "from": "c5", "to": "c6", "note": "could not remove the old copy" },
                { "id": "c", "status": "unchanged", "commit": "c4" },
                { "id": "d", "status": "linked" },
            ]})
        );

        let mut with_failure = quiet;
        with_failure.insert(
            1,
            line(
                "e",
                Outcome::Failed {
                    code: "trust_required",
                    reason: "no consent".to_string(),
                },
            ),
        );
        let (fields, code) = update_envelope(with_failure);
        assert_eq!(code, 1, "a failed plugin makes the command exit 1");
        assert_eq!(fields["plugins"].as_array().unwrap().len(), 6);
        assert_eq!(
            fields["plugins"][1],
            json!({ "id": "e", "status": "failed", "code": "trust_required", "reason": "no consent" })
        );
        assert_eq!(fields["plugins"][5]["status"], "linked");
    }

    /// A throwaway `tmux -L <unique> -f /dev/null` server, killed on drop — never the
    /// user's own. See tests/tmux_client.rs for why the socket name carries the pid and a
    /// per-server counter (cargo runs these as threads of one process). The second field is
    /// the home its shells have, which goes with it: no history file and no startup file of
    /// whoever runs the tests (`tests/common/shell_env.rs` says why).
    struct Server(
        Tmux,
        #[allow(dead_code)] tempfile::TempDir,
        std::path::PathBuf,
    );

    impl Server {
        fn start() -> Server {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = format!("taw-plugin-caller-{}-{n}", std::process::id());
            let t = Tmux::with_socket_name(&name);
            let home = tempfile::tempdir().unwrap();
            // The shell's variables are in the environment of the command that starts the server,
            // so that its first pane has them too.
            let started = t
                .command()
                .args(["-u", "-f", "/dev/null", "new-session", "-d", "-s", "one"])
                .args(["-x", "80", "-y", "24", "/bin/sh"])
                .env("HISTFILE", "/dev/null")
                .env("HOME", home.path())
                .env("ZDOTDIR", home.path())
                .status()
                .unwrap();
            assert!(started.success(), "the test server did not start");
            // Its socket file goes with it (tmux leaves it behind): the exact path it says.
            let socket_file = t
                .run(&["display-message", "-p", "#{socket_path}"])
                .unwrap()
                .into();
            Server(t, home, socket_file)
        }

        fn pane(&self) -> String {
            self.0
                .display("one", "#{pane_id}")
                .unwrap()
                .trim()
                .to_string()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.run_ok(&["kill-server"]);
            crate::cli::test_support::take_the_socket_away(&self.0, &self.2);
        }
    }

    /// tmux leaves the socket file of a server that has ended where it was: a dropped server takes
    /// its own away (`test_support::take_the_socket_away`), once the server is gone.
    #[test]
    fn a_dropped_server_takes_its_socket_file_with_it() {
        use std::os::unix::fs::FileTypeExt;

        let s = Server::start();
        let file = s.2.clone();
        assert!(
            std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_socket()),
            "a server that is up has a socket file, and there is none at {file:?}"
        );

        drop(s);

        assert!(
            !file.exists(),
            "a dropped server left its socket file behind: {file:?}"
        );
    }

    #[test]
    fn a_caller_pane_that_lives_on_this_server_is_kept_verbatim() {
        let s = Server::start();
        let pane = s.pane();
        assert_eq!(
            validated_caller_pane(&s.0, Some(pane.clone())),
            Some(pane),
            "a real pane on this server is exposed unchanged"
        );
    }

    #[test]
    fn a_caller_pane_that_is_not_on_this_server_is_dropped() {
        let s = Server::start();
        // A pane id no server handed out for this session: not on this socket.
        assert_eq!(
            validated_caller_pane(&s.0, Some("%999".to_string())),
            None,
            "$TMUX_PANE from a different server must not be exposed as this server's caller"
        );
        assert_eq!(validated_caller_pane(&s.0, None), None);
    }
}
