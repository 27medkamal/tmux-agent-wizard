//! The single-verb commands: `taw status`, `taw config`, `taw skill`, `taw ui`.

use clap::Subcommand;
use serde_json::{Value, json};

use super::common;
use super::{Ctx, Sub};
use crate::config::{self, Level};
use crate::rail;

/// The agent skill, printed by `taw skill` for a coding agent to load into its context.
/// Compiled in so the binary is self-contained (herdr's `--skill` does the same).
pub const SKILL: &str = include_str!("../../SKILL.md");

#[derive(Subcommand, Debug)]
pub enum StatusCmd {
    /// Recompute the rail and write `@agent_wizard_status`
    Refresh,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Validate config.toml and print diagnostics
    Check,
    /// Re-read config.toml in every live watcher
    Reload,
    /// Remove the [keys] table from config.toml, leaving every other byte alone
    ResetKeys,
    /// Print the resolved config and state paths
    Path,
    /// Print the documented default config.toml (raw TOML, not a JSON envelope)
    Default,
}

/// `taw status [refresh]`: the status-rail text plus the counts behind it.
///
/// Plain `status` computes without writing, so a caller can read the rail without a side
/// effect; `refresh` is the variant that writes the global option a user's `status-left`
/// interpolates (`#{@agent_wizard_status}`).
pub(crate) fn status(ctx: &Ctx, cmd: Option<StatusCmd>) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Some(StatusCmd::Refresh) => ("refresh", refresh(ctx)),
        None => ("show", show(ctx)),
    }
}

fn counts_json(counts: &rail::Counts) -> Value {
    json!({
        "blocked": counts.blocked,
        "working": counts.working,
        "done": counts.done,
        "idle": counts.idle,
        "unknown": counts.unknown,
    })
}

fn show(ctx: &Ctx) -> anyhow::Result<Value> {
    let panes = common::list_panes(&ctx.tmux)?;
    let counts = rail::counts(&panes);
    Ok(json!({
        "text": rail::render(&counts, ctx.cfg.ui.status_indicators),
        "counts": counts_json(&counts),
    }))
}

fn refresh(ctx: &Ctx) -> anyhow::Result<Value> {
    let text = rail::refresh(&ctx.tmux, &ctx.cfg)?;
    let counts = rail::counts(&common::list_panes(&ctx.tmux)?);
    Ok(json!({
        "text": text,
        "counts": counts_json(&counts),
    }))
}

/// `config default` never reaches here: [`crate::cli::dispatch`] picks it off ahead of
/// this because it prints raw TOML instead of an envelope — see [`config_default`].
pub(crate) fn config(ctx: &Ctx, cmd: ConfigCmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        ConfigCmd::Check => ("check", check(ctx)),
        ConfigCmd::Reload => ("reload", reload(ctx)),
        ConfigCmd::ResetKeys => (
            super::terminal::RESET_KEYS,
            super::terminal::reset_keys(ctx),
        ),
        ConfigCmd::Path => ("path", path(ctx)),
        // Present for exhaustiveness only. An answer with a JSON-wrapped copy of the file
        // would be an envelope shape nothing emits, and would silently replace the raw
        // output the whole verb exists for if the dispatch above were ever removed. An
        // error says that out loud instead.
        ConfigCmd::Default => (
            "default",
            Err(anyhow::anyhow!(
                "`config default` prints raw TOML and must be dispatched before the \
                 envelope path; reaching cli::misc::config with it is a routing bug"
            )),
        ),
    }
}

/// The documented default `config.toml`, printed **raw** rather than wrapped in an
/// envelope: its whole purpose is to be redirected into a file
/// (`taw config default > ~/.config/tmux-agent-wizard/config.toml`), and a JSON wrapper
/// would make every user pipe it back out through `jq` first.
pub fn config_default() -> anyhow::Result<String> {
    Ok(config::DEFAULT_TOML.to_string())
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Info => "info",
    }
}

/// Diagnostics are ordered errors-first so a caller that shows only the first line shows
/// the one that actually stops the config loading.
fn check(ctx: &Ctx) -> anyhow::Result<Value> {
    check_with_status(ctx).map(|(fields, _)| fields)
}

/// The exit status of `taw config check`: 1 when it reports an error, 0 for a clean file and for
/// warnings alone, so a script, a pre-commit hook or a dotfiles CI can tell a broken file from a
/// good one. The one the other checks use (`taw machine check`): the answer is printed all the same.
const CHECK_FAILED: i32 = 1;

/// `taw config check`: its answer, and the exit status that goes with it ([`CHECK_FAILED`]).
pub(crate) fn check_with_status(ctx: &Ctx) -> anyhow::Result<(Value, i32)> {
    let mut diagnostics = config::check(&ctx.paths);
    // `[theme] auto_switch` needs tmux 3.6: asked only when the file turns it on.
    if let Ok(loaded) = config::load(&ctx.paths)
        && loaded.theme.auto_switch
    {
        diagnostics.extend(config::check_auto_switch(
            &loaded.theme,
            ctx.tmux.version().ok(),
        ));
    }
    diagnostics.sort_by_key(|d| match d.level {
        Level::Error => 0,
        Level::Warning => 1,
        Level::Info => 2,
    });
    let errors = diagnostics
        .iter()
        .filter(|d| d.level == Level::Error)
        .count();
    let fields = json!({
        "config_file": ctx.paths.config_file,
        "errors": errors,
        "diagnostics": diagnostics
            .iter()
            .map(|d| json!({ "level": level_name(d.level), "message": d.message }))
            .collect::<Vec<_>>(),
    });
    Ok((fields, if errors > 0 { CHECK_FAILED } else { 0 }))
}

/// `config reload` — herdr's `server.reload_config`.
///
/// Validates first, and only bumps [`crate::watch::reload::RELOAD_OPTION`] when the file
/// actually loads: telling every live watcher to re-read a config that cannot be parsed
/// would be asking each of them to fail one by one. A watcher that reloads on its own
/// (by mtime) makes the same judgement — it keeps the settings already in force and warns
/// — so a broken file never changes behaviour by either route.
///
/// `status` is herdr's `ConfigReloadStatus`: `failed` when the file does not load,
/// `partial` when it loads but `config check` has something to say about it (an unknown
/// key is accepted and ignored, so *some* of the file did not take effect), `applied`
/// when it is clean. What `config check` says of your sandbox profiles folder alone is
/// listed with the rest, and leaves the status alone: no line of the file failed to take
/// effect for it.
fn reload(ctx: &Ctx) -> anyhow::Result<Value> {
    let config::Checked { file, profiles } = config::check_apart(&ctx.paths);
    let failed = file.iter().any(|d| d.level == Level::Error);
    let loadable = config::load(&ctx.paths).is_ok();
    let status = if failed || !loadable {
        "failed"
    } else if file.is_empty() {
        "applied"
    } else {
        "partial"
    };
    let diagnostics: Vec<_> = file.into_iter().chain(profiles).collect();
    let notified = if status == "failed" {
        0
    } else {
        super::manifest::bump_reload(ctx)?
    };
    Ok(json!({
        "status": status,
        "config_file": ctx.paths.config_file,
        "watchers_notified": notified,
        "diagnostics": diagnostics
            .iter()
            .map(|d| json!({ "level": level_name(d.level), "message": d.message }))
            .collect::<Vec<_>>(),
    }))
}

fn path(ctx: &Ctx) -> anyhow::Result<Value> {
    let p = &ctx.paths;
    Ok(json!({
        "config_dir": p.config_dir,
        "config_file": p.config_file,
        "state_dir": p.state_dir,
        "override_dir": p.override_dir,
        "remote_manifest_dir": p.remote_manifest_dir,
        "sessions_dir": p.sessions_dir,
        "plugins_dir": p.plugins_dir,
        "logs_dir": p.logs_dir,
        "exists": p.config_file.exists(),
    }))
}

/// `taw skill` — the agent skill, printed **raw** rather than wrapped in an envelope:
/// like `config default`, it exists to be redirected into a file or pasted straight into
/// an agent's context, and a JSON wrapper would make every consumer unwrap it first.
pub fn skill() -> anyhow::Result<String> {
    Ok(SKILL.to_string())
}

/// `taw ui` — the ratatui popup (`tui::run`). It draws to the terminal rather
/// than printing an envelope, so it reports only its exit.
///
/// `from` is the pane the popup was opened from: `taw ui open` resolves it *before*
/// `display-popup` runs and passes `--from`, because a `display-popup -E` command is not
/// given the invoking pane's environment and so has no `$TMUX_PANE` of its own. That
/// environment variable is the fallback for a `taw ui` typed in a pane by hand, and this
/// is the one place the popup path reads it: everything below — `tui::run`, `data::build`,
/// `Action::NextUp` — takes the pane as a plain parameter.
///
/// `client` is `taw ui open`'s `--client`, the client it opened this popup on: the popup's
/// switches all move that client, and its previous session is that client's. A `taw ui`
/// typed in a pane has none.
///
/// `bar` is `taw ui open`'s `--bar`: it swapped the status line for taw's bar and this
/// popup's ticks should keep its counts fresh. `false` for every other way `taw ui` runs —
/// typed in a pane, a launcher that opened without swapping — so none of those write
/// `@agent_wizard_bar` on any session.
///
/// `new_session` is `--new-session`, on `taw ui` and passed on by `taw ui open`: the popup
/// starts with the new-session picker open, and Esc closes it.
pub(crate) fn ui(
    ctx: &Ctx,
    from: Option<&str>,
    client: Option<&str>,
    bar: bool,
    new_session: bool,
) -> anyhow::Result<Value> {
    let origin = origin_pane(from, std::env::var("TMUX_PANE").ok());
    crate::tui::run(
        &ctx.tmux,
        &ctx.cfg,
        &ctx.paths,
        &ctx.taw_bin,
        origin.as_deref(),
        client.filter(|name| !name.is_empty()),
        bar,
        new_session,
    )?;
    Ok(common::ok())
}

/// The pane the popup was opened from: `from` (`--from`) when it names one, else `env`
/// (`$TMUX_PANE`), and nothing when neither does. An empty or blank value names no pane, so
/// an empty `--from` falls back as a missing one does.
fn origin_pane(from: Option<&str>, env: Option<String>) -> Option<String> {
    let named = |id: &str| (!id.trim().is_empty()).then(|| id.trim().to_string());
    from.and_then(named)
        .or_else(|| env.as_deref().and_then(named))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--from` when it names a pane, else `$TMUX_PANE`, else none. An empty `--from` names
    /// none, so it falls back as a missing one does; it does not win and open the popup from
    /// no pane at all when `$TMUX_PANE` names one. A blank value is empty too.
    #[test]
    fn the_origin_is_from_else_tmux_pane() {
        let env = || Some("%7".to_string());
        assert_eq!(origin_pane(Some("%3"), env()).as_deref(), Some("%3"));
        assert_eq!(origin_pane(None, env()).as_deref(), Some("%7"));
        assert_eq!(origin_pane(Some(""), env()).as_deref(), Some("%7"));
        assert_eq!(origin_pane(Some(" "), env()).as_deref(), Some("%7"));
        assert_eq!(origin_pane(Some(""), Some(String::new())), None);
        assert_eq!(origin_pane(None, Some(" ".to_string())), None);
        assert_eq!(origin_pane(None, None), None);
    }

    /// `taw config check` itself, over a tmux that says `tmux 3.5a`: with `[theme] auto_switch` on its
    /// diagnostics carry the warning that names the version, and with it off they carry none.
    #[test]
    fn config_check_warns_of_auto_switch_on_a_tmux_before_3_6_only_when_it_is_on() {
        use crate::cli::test_support::{Stub, stub_ctx, stub_tmux};
        let warnings = |config: &str| -> Vec<String> {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _log) = stub_tmux(
                dir.path(),
                &Stub {
                    version: "tmux 3.5a",
                    ..Stub::default()
                },
            );
            let ctx = stub_ctx(tmux);
            std::fs::create_dir_all(ctx.paths.config_file.parent().unwrap()).unwrap();
            std::fs::write(&ctx.paths.config_file, config).unwrap();
            let checked = check(&ctx).unwrap();
            checked["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["message"].as_str().unwrap().to_string())
                .collect()
        };
        let on = warnings("[theme]\nauto_switch = true\n");
        assert_eq!(on.len(), 1, "{on:?}");
        assert!(on[0].contains("this tmux is 3.5a"), "{on:?}");
        assert_eq!(
            warnings("[theme]\nauto_switch = false\n"),
            Vec::<String>::new()
        );
    }

    /// The status `taw config check` exits with: 1 for an error, 0 for a warning alone and for nothing.
    #[test]
    fn config_check_status_is_1_for_an_error_and_0_for_a_warning_or_nothing() {
        use crate::cli::test_support::{Stub, stub_ctx, stub_tmux};
        let status = |config: &str| -> i32 {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, _log) = stub_tmux(dir.path(), &Stub::default());
            let ctx = stub_ctx(tmux);
            std::fs::create_dir_all(ctx.paths.config_file.parent().unwrap()).unwrap();
            std::fs::write(&ctx.paths.config_file, config).unwrap();
            check_with_status(&ctx).unwrap().1
        };
        assert_eq!(status("[ui]\ndepth = \"all\"\n"), 0);
        assert_eq!(status("[ui]\nfrobnicate = 1\n"), 0, "a warning alone");
        assert_eq!(status("[ui]\ndepth = \"sideways\"\n"), 1);
        assert_eq!(status("this is = not = toml"), 1);
    }

    #[test]
    fn diagnostic_levels_are_the_lowercase_labels() {
        assert_eq!(level_name(Level::Error), "error");
        assert_eq!(level_name(Level::Warning), "warning");
        assert_eq!(level_name(Level::Info), "info");
    }

    #[test]
    fn the_compiled_in_skill_is_the_repo_skill_and_guards_on_tmux() {
        assert!(SKILL.contains("taw"), "SKILL.md must document `taw`");
        assert!(
            SKILL.contains("$TMUX"),
            "SKILL.md must guard on $TMUX, not herdr's HERDR_ENV"
        );
        assert!(
            !SKILL.contains("HERDR_ENV"),
            "SKILL.md must not tell agents to check HERDR_ENV"
        );
    }
}
