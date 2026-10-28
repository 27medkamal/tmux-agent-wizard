//! `taw manifest …` — the detection manifests `detect` evaluates panes against. Ours
//! alone (herdr keeps these under `server agent-manifests`, which has no tmux analogue
//! since `taw` has no server).

use clap::Subcommand;
use serde_json::{Value, json};

use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::detect::{self, ManifestSource, ManifestStore, Snapshot, update};

/// How stale the update marker may get before `--if-stale` refetches: six hours.
///
/// Reused from the watcher rather than restated, because both touch the *same* marker
/// file (`<state>/agent-detection/last-check`). Under two different windows, whichever
/// ran first would keep resetting the clock the other measures against.
const STALE_AFTER: std::time::Duration = crate::watch::MANIFEST_MAX_AGE;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every bundled agent's manifest, with the version in force
    List,
    /// Fetch newer manifests into the cache
    Update {
        /// Only fetch when the last check is more than six hours old
        #[arg(long = "if-stale")]
        if_stale: bool,
    },
    /// Show one agent's manifest and its rules
    Show {
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Re-read the local manifest overrides in every live watcher
    Reload,
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List => ("list", list(ctx)),
        Cmd::Update { if_stale } => ("update", update_manifests(ctx, if_stale)),
        Cmd::Show { id } => ("show", show(ctx, &id)),
        Cmd::Reload => ("reload", reload(ctx)),
    }
}

/// `manifest reload` — herdr's `server.reload_agent_manifests`.
///
/// herdr has one process holding the manifests, so reloading is a function call. Here
/// every watcher holds its own compiled copy, and they are separate processes started by
/// `pipe-pane`: this bumps [`crate::watch::reload::RELOAD_OPTION`], which each watcher
/// reads with the display expansion it already makes once per evaluation, and drops its
/// cache when the value changed. Watchers pick a *file* edit up on their own by mtime;
/// this verb is for the cases mtime cannot see, and for "re-read now".
///
/// Reports the manifests in force after the reload, which is what herdr's
/// `agent_manifest_reload` result carries. The reload is asynchronous by nature — each
/// watcher acts on its next evaluation, within one tick — so `watchers_notified` counts
/// the live watchers the bump will reach, not ones that have already acted.
fn reload(ctx: &Ctx) -> anyhow::Result<Value> {
    let notified = bump_reload(ctx)?;
    let listed = list(ctx)?;
    Ok(json!({
        "manifests": listed["manifests"].clone(),
        "watchers_notified": notified,
    }))
}

/// Bumps the global reload token and returns how many live watchers will see it.
///
/// The token is a counter so that two reloads in a row are two distinct values; watchers
/// only compare it for inequality, so wrapping at `u64::MAX` would still register as a
/// change.
pub(crate) fn bump_reload(ctx: &Ctx) -> anyhow::Result<usize> {
    let current = ctx
        .tmux
        .get_global_option(crate::watch::reload::RELOAD_OPTION)
        .unwrap_or_default()
        .unwrap_or_default();
    let next = current.trim().parse::<u64>().unwrap_or(0).wrapping_add(1);
    ctx.tmux
        .set_global_option(crate::watch::reload::RELOAD_OPTION, &next.to_string())
        .map_err(|e| {
            CliError::tmux(format!(
                "setting {}: {e:#}",
                crate::watch::reload::RELOAD_OPTION
            ))
        })?;
    Ok(crate::watch::status(&ctx.tmux)
        .map(|all| all.iter().filter(|w| w.piped && w.watcher_alive).count())
        .unwrap_or(0))
}

fn store(ctx: &Ctx) -> ManifestStore {
    ManifestStore::new(
        ctx.paths.override_dir.clone(),
        ctx.paths.remote_manifest_dir.clone(),
    )
    .with_supplements(ctx.cfg.detection.supplements)
}

fn source_name(source: ManifestSource) -> &'static str {
    match source {
        ManifestSource::Bundled => "bundled",
        ManifestSource::Remote => "remote",
        ManifestSource::Override => "override",
    }
}

fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    let manifests: Vec<Value> = store(ctx)
        .list()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "active_version": m.active_version,
                "active_source": source_name(m.active_source),
                "remote_version": m.remote_version,
                "bundled_version": m.bundled_version,
                "shadowed_by_override": m.shadowed_by_override,
                "supplement_rules": m.supplement_rules,
            })
        })
        .collect();
    Ok(json!({ "manifests": manifests }))
}

/// The User-Agent the manifest channel sees — the crate name and version, so a server
/// operator can tell which build is asking.
fn user_agent() -> String {
    format!("tmux-agent-wizard/{}", env!("CARGO_PKG_VERSION"))
}

fn outcome_json(outcome: &update::UpdateOutcome) -> Value {
    json!({
        "id": outcome.id,
        "status": outcome.status,
        "message": outcome.message,
    })
}

/// `manifest update [--if-stale]`.
///
/// `--if-stale` is the unattended form: the plugin script fires it on every tmux server
/// start, and it is the only manifest fetch nobody asked for by hand. So it answers to
/// `[update] manifest_check` as well as to the marker — the same switch the watcher gates
/// its background refresh on (`watch::run`). Without that, turning the switch off silenced
/// one caller and left the other reaching out on every server start.
///
/// A plain `manifest update` is the user asking, and is never gated: the switch is about
/// what `taw` does on its own.
fn update_manifests(ctx: &Ctx, if_stale: bool) -> anyhow::Result<Value> {
    if if_stale && !ctx.cfg.update.manifest_check {
        return Ok(json!({ "checked": false, "outcomes": [] }));
    }
    let store = store(ctx);
    let base_url = update::DEFAULT_MANIFEST_BASE_URL;
    let outcomes = if if_stale {
        update::update_if_stale(
            &store,
            &ctx.paths.manifest_marker,
            STALE_AFTER,
            base_url,
            &user_agent(),
        )
    } else {
        Some(update::update_all(&store, base_url, &user_agent()))
    };
    match outcomes {
        Some(outcomes) => Ok(json!({
            "checked": true,
            "outcomes": outcomes.iter().map(outcome_json).collect::<Vec<_>>(),
        })),
        // `--if-stale` and the marker is fresh: nothing was fetched, which is a success.
        None => Ok(json!({ "checked": false, "outcomes": [] })),
    }
}

/// One manifest, with each of its rules.
///
/// `LoadedManifest` keeps its rules private, so they are enumerated by evaluating the
/// manifest against an *empty* snapshot: no rule can match empty text, and the
/// `evaluated_rules` of that run is exactly the manifest's rule list with each rule's id,
/// region, state and priority.
fn show(ctx: &Ctx, id: &str) -> anyhow::Result<Value> {
    let manifest = store(ctx)
        .load(id)
        .map_err(|e| CliError::not_found(format!("no manifest for agent '{id}': {e}")))?;
    let probe = detect::evaluate(
        &manifest,
        &Snapshot {
            content: "",
            osc_title: "",
            osc_progress: "",
        },
    );
    let rules: Vec<Value> = probe
        .evaluated_rules
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "region": r.region,
                "state": r.state,
                "priority": r.priority,
                "source": r.source,
            })
        })
        .collect();
    let supplement_rules = probe
        .evaluated_rules
        .iter()
        .filter(|r| r.source == detect::RuleSource::Taw)
        .count();
    Ok(json!({
        "id": manifest.id,
        "version": manifest.version,
        "source": source_name(manifest.source),
        "warning": manifest.warning,
        "engine_version": detect::MANIFEST_ENGINE_VERSION,
        "supplement_rules": supplement_rules,
        "rules": rules,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, paths::Paths, tmux::Tmux};
    use tempfile::TempDir;

    #[test]
    fn source_names_are_the_lowercase_labels_used_everywhere_else() {
        assert_eq!(source_name(ManifestSource::Bundled), "bundled");
        assert_eq!(source_name(ManifestSource::Remote), "remote");
        assert_eq!(source_name(ManifestSource::Override), "override");
    }

    #[test]
    fn the_user_agent_names_the_crate_and_version() {
        let ua = user_agent();
        assert!(ua.starts_with("tmux-agent-wizard/"), "{ua}");
        assert!(ua.contains(env!("CARGO_PKG_VERSION")), "{ua}");
    }

    /// A `Ctx` over throwaway directories (so `store(ctx)` falls through to the
    /// bundled manifest, plus supplements) and a socket name no real server binds —
    /// `show`/`list` never send it a command, they only read `ctx.paths`/`ctx.cfg`.
    fn test_ctx(dir: &TempDir, supplements: bool) -> Ctx {
        let mut cfg = Config::default();
        cfg.detection.supplements = supplements;
        Ctx {
            tmux: Tmux::with_socket_name("taw-test-manifest-show-nonexistent"),
            cfg,
            paths: Paths {
                config_dir: dir.path().join("config"),
                config_file: dir.path().join("config/config.toml"),
                state_dir: dir.path().join("state"),
                override_dir: dir.path().join("agent-detection-override"),
                remote_manifest_dir: dir.path().join("agent-detection-remote"),
                sessions_dir: dir.path().join("sessions"),
                plugins_dir: dir.path().join("plugins"),
                plugin_state_dir: dir.path().join("plugin-state"),
                logs_dir: dir.path().join("logs"),
                manifest_marker: dir.path().join("state/agent-detection/last-check"),
            },
            taw_bin: std::path::PathBuf::from("taw"),
        }
    }

    #[test]
    fn show_lists_each_rule_source_and_the_supplement_rule_count() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir, true);

        let result = show(&ctx, "claude").unwrap();
        let rules = result["rules"].as_array().unwrap();
        let taw_rule_ids: Vec<&str> = rules
            .iter()
            .filter(|rule| rule["source"] == "taw")
            .map(|rule| rule["id"].as_str().unwrap())
            .collect();

        assert_eq!(result["supplement_rules"], 6);
        assert_eq!(taw_rule_ids.len(), 6);
        for id in [
            "taw_live_turn_working_dot",
            "taw_live_turn_working_vim_footer",
            "taw_background_shell_working",
            "taw_background_agents_working_asterisk",
            "taw_stop_hooks_working",
            "taw_subagent_running",
        ] {
            assert!(
                taw_rule_ids.contains(&id),
                "missing {id} in {taw_rule_ids:?}"
            );
        }
        // Every rule from the bundled manifest itself must still say "manifest".
        assert!(
            rules
                .iter()
                .any(|rule| rule["id"] == "live_prompt_box" && rule["source"] == "manifest")
        );
    }

    #[test]
    fn show_reports_no_supplement_rules_when_the_switch_is_off() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir, false);

        let result = show(&ctx, "claude").unwrap();
        assert_eq!(result["supplement_rules"], 0);
        assert!(
            result["rules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|rule| rule["source"] == "manifest")
        );
    }

    #[test]
    fn list_reports_supplement_rules_per_agent() {
        let dir = TempDir::new().unwrap();
        let ctx = test_ctx(&dir, true);

        let result = list(&ctx).unwrap();
        let manifests = result["manifests"].as_array().unwrap();
        let claude = manifests
            .iter()
            .find(|m| m["id"] == "claude")
            .expect("claude in the list");
        assert_eq!(claude["supplement_rules"], 6);

        let codex = manifests
            .iter()
            .find(|m| m["id"] == "codex")
            .expect("codex in the list");
        assert_eq!(codex["supplement_rules"], 1);

        let gemini = manifests
            .iter()
            .find(|m| m["id"] == "gemini")
            .expect("gemini in the list");
        assert_eq!(gemini["supplement_rules"], 0);
    }
}
