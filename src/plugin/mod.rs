//! herdr-compatible plugin system: manifests, install/link, actions, events, panes.
//!
//! A plugin is a directory with a `herdr-plugin.toml` manifest whose commands call back
//! into `taw` through `$HERDR_BIN_PATH` (or, for shell plugins, individual `HERDR_*` env
//! vars). This crate runs herdr plugins unmodified: same manifest format, same env vars,
//! same trust preview. See
//! `third_party/herdr/reference/plugins.mdx`.
//!
//! Every entry point here takes its dependencies explicitly via [`PluginCtx`], there is
//! no shared mutable app state. tmux remains the database; this module's only local state
//! is the plugin registry, plugin config/state directories, and (for split/side/bottom
//! panes) a small per-plugin `panes.json`, all under `Paths::plugins_dir`.

use anyhow::Context as _;

pub mod actions;
pub mod checkout;
pub mod context;
pub mod env;
pub mod events;
pub mod install;
pub mod manifest;
pub mod panes;
pub mod registry;
pub mod running;
pub mod update;

/// Everything a plugin operation needs: the tmux client, the loaded config, resolved
/// filesystem paths, and the absolute path to the running `taw` binary (for
/// `HERDR_BIN_PATH`). Passed explicitly to every function in this module, never stored.
///
/// `server_env` is tmux's global environment as a long-lived caller (the watcher) keeps it,
/// for the `[hooks]` commands it starts. A caller that has none reads it from `tmux` when a
/// hook is about to start, which is right for a process that ends a moment later.
pub struct PluginCtx<'a> {
    pub tmux: &'a crate::tmux::Tmux,
    pub cfg: &'a crate::config::Config,
    pub paths: &'a crate::paths::Paths,
    pub taw_bin: &'a std::path::Path,
    pub server_env: Option<&'a crate::hooks::server_env::ServerEnvCache>,
}

/// One plugin (or CLI) invocation's workspace/tab/pane focus, each already serialized to
/// its herdr-shaped JSON record (see [`context::pane_json`], [`context::workspace_json`],
/// [`context::tab_json`]), plus the raw/public id of the pane that issued the invocation
/// (independent of which pane, if any, the context itself focuses, e.g. `$TMUX_PANE` of
/// the CLI caller) and any extra fields a caller wants merged at the top level of
/// `HERDR_PLUGIN_CONTEXT_JSON` (used by `events::emit`, whose payload does not necessarily
/// fit the workspace/tab/pane shape).
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub workspace: Option<serde_json::Value>,
    pub tab: Option<serde_json::Value>,
    pub pane: Option<serde_json::Value>,
    pub caller_pane_id: Option<String>,
    pub extra: serde_json::Map<String, serde_json::Value>,
}

const MANIFEST_FILE: &str = "herdr-plugin.toml";

/// Installs a GitHub-hosted plugin: parses `spec` (`owner/repo[/subdir][@ref]`), fetches the
/// commit it names into a folder of its own, `<state>/plugins/github/<owner>/<repo>@<12-char
/// sha>` (see [`checkout`]), loads its manifest there, checks `min_herdr_version`, shows the
/// trust preview and requires confirmation (`yes`, or an interactive `y`, see
/// [`install::gate`]), runs `[[build]]` in that folder, registers the plugin, then runs
/// `[[startup]]` detached. Refuses to install over a plugin already registered by [`link`]
/// under the same id (unlink or uninstall it first, matches herdr).
///
/// A reinstall uses the same flow, so nothing live changes before consent and a good build: a
/// decline, a failed fetch or a failed build removes the new folder and leaves the
/// installed one and the records as they were. A folder left by Ctrl-C at the prompt is
/// removed at the next install or update of that repository (records and the live folder are
/// untouched). A build only runs in a folder this run made: at the commit already installed
/// the new folder is `<repo>@<sha12>-<n>`. Plugins that share the installed checkout (and the
/// ref, an install made before this layout) share the new folder: each one's gate and build
/// run there and every record switches in the same write. The old folder is removed after
/// that. The remote is asked for the ref's commit first, which names the folder; a ref that
/// looks like a short commit and that the remote does not name is refused before any fetch.
pub fn install(ctx: &PluginCtx, spec: &str, yes: bool) -> anyhow::Result<registry::PluginRecord> {
    let parsed = install::parse_spec(spec)?;
    let url = format!("https://github.com/{}/{}.git", parsed.owner, parsed.repo);
    install_from_url(
        ctx,
        parsed,
        &url,
        yes,
        &install::terminal_answer,
        &install::GitFetcher,
    )
}

/// `install`'s implementation, taking the clone URL and the fetch as explicit parameters so
/// tests can point it at a local hermetic repo instead of the real `https://github.com/...`
/// URL `install` always uses (and count its fetches), and the consent question as `answer` so
/// no test depends on whether its stdin is a terminal.
fn install_from_url(
    ctx: &PluginCtx,
    parsed: install::Spec,
    url: &str,
    yes: bool,
    answer: install::Answer,
    fetcher: &dyn install::Fetcher,
) -> anyhow::Result<registry::PluginRecord> {
    let paths = ctx.paths;
    let requested_ref = parsed.requested_ref.as_deref();
    // A folder left by a run that was killed, which no record points at.
    checkout::clear_leftovers(paths, &parsed.owner, &parsed.repo)?;

    // The plugin being installed again, and the plugins that share its folder and ref, which
    // move to the new folder with it.
    let installed = registry::load(paths)?.plugins;
    let replaced = installed.iter().find(|p| {
        matches!(
            &p.source,
            registry::PluginSource::Github { owner, repo, subdir, .. }
                if owner.eq_ignore_ascii_case(&parsed.owner)
                    && repo.eq_ignore_ascii_case(&parsed.repo)
                    && *subdir == parsed.subdir
        )
    });
    let old_root = replaced.and_then(checkout::root_of);
    let siblings: Vec<registry::PluginRecord> = match (&old_root, replaced) {
        (Some(root), Some(replaced)) => installed
            .iter()
            .filter(|p| {
                p.id != replaced.id
                    && checkout::root_of(p).is_some_and(|r| checkout::same_folder(&r, root))
                    && matches!(
                        &p.source,
                        registry::PluginSource::Github { requested_ref: r, .. }
                            if r.as_deref() == requested_ref
                    )
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };

    // An error asking the remote is not the end: the fetch says why, if it fails too.
    let named = fetcher.ls_remote(url, requested_ref);
    install::refuse_short_sha(&parsed.owner, &parsed.repo, requested_ref, &named)?;
    let hint = named.unwrap_or(None);
    let staged = checkout::stage(
        paths,
        &parsed.owner,
        &parsed.repo,
        url,
        requested_ref,
        hint.as_deref(),
        fetcher,
    )?;

    let result = (|| -> anyhow::Result<registry::PluginRecord> {
        let plugin_dir = install::plugin_dir(&staged.dir, parsed.subdir.as_deref());
        let manifest = checkout::load_member(ctx, &plugin_dir, None)?;
        if let Some(existing) = registry::find(paths, &manifest.id)?
            && matches!(existing.source, registry::PluginSource::Link)
        {
            anyhow::bail!(install::InstallError::LocalPluginConflict(
                manifest.id.clone()
            ));
        }
        let mut members = vec![(plugin_dir.clone(), manifest.clone())];
        for sibling in &siblings {
            let subdir = match &sibling.source {
                registry::PluginSource::Github { subdir, .. } => subdir.as_deref(),
                registry::PluginSource::Link => None,
            };
            let dir = install::plugin_dir(&staged.dir, subdir);
            let manifest = checkout::load_member(ctx, &dir, Some(&sibling.id))
                .with_context(|| format!("'{}' shares this plugin's checkout", sibling.id))?;
            members.push((dir, manifest));
        }
        checkout::gate_and_build(&members, &staged.commit, yes, answer).map_err(
            |(i, err)| match i {
                0 => err,
                i => err.context(format!(
                    "'{}' shares this plugin's checkout and failed",
                    siblings[i - 1].id
                )),
            },
        )?;

        let now = now_unix_ms();
        let record = registry::PluginRecord {
            id: manifest.id.clone(),
            source: registry::PluginSource::Github {
                owner: parsed.owner.clone(),
                repo: parsed.repo.clone(),
                subdir: parsed.subdir.clone(),
                requested_ref: parsed.requested_ref.clone(),
                resolved_commit: Some(staged.commit.clone()),
            },
            path: plugin_dir,
            enabled: true,
            installed_unix_ms: now,
        };
        let mut records = vec![record.clone()];
        records.extend(siblings.iter().map(|s| {
            let mut moved = s.clone();
            if let registry::PluginSource::Github {
                subdir,
                resolved_commit,
                ..
            } = &mut moved.source
            {
                moved.path = install::plugin_dir(&staged.dir, subdir.as_deref());
                *resolved_commit = Some(staged.commit.clone());
            }
            moved.installed_unix_ms = now;
            moved
        }));
        registry::add_all(paths, records)?;
        Ok(record)
    })();

    let record = match result {
        Ok(record) => record,
        Err(err) => {
            staged.discard();
            return Err(err);
        }
    };
    let mut old: Vec<std::path::PathBuf> = old_root.into_iter().collect();
    old.extend(siblings.iter().filter_map(checkout::root_of));
    // Not a failed install: the plugin works from its new folder.
    if let Some(note) = checkout::remove_unused(paths, &old, &staged.dir) {
        log::warn!("plugin install: {note}");
    }
    if let Ok(manifest) = manifest::load(&record.path.join(MANIFEST_FILE)) {
        run_startup_hooks(ctx, &record, &manifest);
    }
    Ok(record)
}

/// Links a local plugin directory: loads `<path>/herdr-plugin.toml`, checks
/// `min_herdr_version`, and registers it with `source: {kind: "link"}`. Runs no build and
/// no startup hooks (matches herdr: startup hooks run only when `taw plugin startup` is
/// invoked, not "when a plugin is linked or enabled", see plugins.mdx "Startup hooks").
pub fn link(
    ctx: &PluginCtx,
    path: &std::path::Path,
    enabled: bool,
) -> anyhow::Result<registry::PluginRecord> {
    let plugin_dir = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    let manifest = manifest::load(&manifest_path)
        .map_err(|e| anyhow::anyhow!("plugin manifest at {}: {e:#}", manifest_path.display()))?;
    install::check_min_version(ctx.cfg, &manifest)?;

    let record = registry::PluginRecord {
        id: manifest.id.clone(),
        source: registry::PluginSource::Link,
        path: plugin_dir,
        enabled,
        installed_unix_ms: now_unix_ms(),
    };
    registry::add(ctx.paths, record.clone())?;
    Ok(record)
}

/// Unregisters a plugin, leaving its files (checkout or linked directory) untouched. Also
/// used by [`uninstall`] as its first step.
pub fn unlink(ctx: &PluginCtx, id: &str) -> anyhow::Result<()> {
    registry::remove(ctx.paths, id)?;
    running::stop(ctx, id);
    Ok(())
}

/// Unregisters a plugin, and, only when it was installed from GitHub (never for a linked
/// plugin) *and* no other registered plugin still lives in the same folder, also deletes the
/// folder its record names. A single GitHub checkout can hold several separately-installed
/// plugins (one per subdir, e.g. this project's own echo-plugin-style monorepo layout);
/// uninstalling one must not delete the files backing its siblings.
pub fn uninstall(ctx: &PluginCtx, id: &str) -> anyhow::Result<()> {
    let loaded = registry::load(ctx.paths)?;
    let record = loaded
        .plugins
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("plugin '{id}' not found"))?;
    if let Some(checkout) = checkout::root_of(&record)
        && checkout.starts_with(ctx.paths.plugins_dir.join("github"))
    {
        refuse_a_path_outside_the_plugins_folder(ctx.paths, &record, &checkout)?;
        let shared_with_a_sibling = loaded.plugins.iter().any(|other| {
            other.id != id
                && checkout::root_of(other).is_some_and(|r| checkout::same_folder(&r, &checkout))
        });
        if !shared_with_a_sibling && checkout.exists() {
            std::fs::remove_dir_all(&checkout)
                .map_err(|e| anyhow::anyhow!("removing checkout {}: {e}", checkout.display()))?;
        }
    }
    unlink(ctx, id)
}

/// A record's path is read from `registry.json`, which a person can edit. Refuses, before
/// anything is removed, a path with a `..` component, and a folder that, made canonical, is
/// not strictly below an owner folder of `<state>/plugins/github`, that is not at
/// `plugins/github/<owner>/<folder>` or deeper (a symlink can lead out of it too).
fn refuse_a_path_outside_the_plugins_folder(
    paths: &crate::paths::Paths,
    record: &registry::PluginRecord,
    checkout: &std::path::Path,
) -> anyhow::Result<()> {
    let id = &record.id;
    if record
        .path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "plugin '{id}': its recorded path {} has a '..' in it, so uninstall removes \
             nothing; fix the path in the plugin registry or run `taw plugin unlink {id}`",
            record.path.display()
        );
    }
    if checkout.exists() {
        let managed = std::fs::canonicalize(paths.plugins_dir.join("github"));
        let inside = match (std::fs::canonicalize(checkout), managed) {
            // Strictly below an owner folder: `plugins/github` and `plugins/github/<owner>`
            // hold other plugins' folders, so a record naming either removes nothing.
            (Ok(real), Ok(managed)) => real
                .strip_prefix(managed)
                .is_ok_and(|below| below.components().count() >= 2),
            _ => false,
        };
        if !inside {
            anyhow::bail!(
                "plugin '{id}': its folder {} is not inside an owner folder of {}, so uninstall \
                 removes nothing; run `taw plugin unlink {id}` to forget it",
                checkout.display(),
                paths.plugins_dir.join("github").display()
            );
        }
    }
    Ok(())
}

/// Enables or disables a registered plugin (disabled plugins run no startup/action/event/
/// pane commands).
pub fn set_enabled(ctx: &PluginCtx, id: &str, enabled: bool) -> anyhow::Result<()> {
    registry::set_enabled(ctx.paths, id, enabled)?;
    if !enabled {
        // What its startup commands started (a loop that renames, a watcher) ends with it.
        running::stop(ctx, id);
    }
    Ok(())
}

/// Every registered plugin (installed or linked), enabled or not.
pub fn list(ctx: &PluginCtx) -> anyhow::Result<Vec<registry::PluginRecord>> {
    Ok(registry::load(ctx.paths)?.plugins)
}

/// `<config>/plugins/<id>`, creating it if needed, herdr's `HERDR_PLUGIN_CONFIG_DIR`.
pub fn config_dir(ctx: &PluginCtx, id: &str) -> anyhow::Result<std::path::PathBuf> {
    let dir = ctx.paths.config_dir.join("plugins").join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `<state>/plugins/state/<id>/plugin.log`, where detached plugin process output
/// (startup hooks, event hooks) is appended. Does not create the file or its directory;
/// [`env::spawn_detached`] does that when it actually writes to it.
pub fn log_path(ctx: &PluginCtx, id: &str) -> std::path::PathBuf {
    ctx.paths.plugin_state_dir.join(id).join("plugin.log")
}

/// Runs every *enabled* plugin's `[[startup]]` commands, detached. Returns the ids of the
/// plugins whose manifest loaded successfully (i.e. that startup was attempted for,
/// whether or not they declare any `[[startup]]` commands), a plugin whose manifest
/// fails to load is skipped (logged, not fatal: "a startup failure does not stop the
/// server", plugins.mdx "Startup hooks"). Called once by the `.tmux` plugin script on
/// tmux startup (there is no daemon to do this automatically).
pub fn startup(ctx: &PluginCtx) -> anyhow::Result<Vec<String>> {
    let mut started = Vec::new();
    for record in registry::load(ctx.paths)?
        .plugins
        .into_iter()
        .filter(|p| p.enabled)
    {
        match manifest::load(&record.path.join(MANIFEST_FILE)) {
            Ok(manifest) => {
                run_startup_hooks(ctx, &record, &manifest);
                started.push(record.id);
            }
            Err(err) => {
                log::warn!("plugin '{}': skipping startup, {err:#}", record.id);
            }
        }
    }
    Ok(started)
}

/// Spawns every `[[startup]]` step, detached, logging (not failing) individual command
/// failures, matches herdr: "A startup failure does not stop the server."
fn run_startup_hooks(
    ctx: &PluginCtx,
    record: &registry::PluginRecord,
    manifest: &manifest::PluginManifest,
) {
    let log = log_path(ctx, &record.id);
    running::forget_gone(ctx, &record.id);
    for step in manifest.startup_steps() {
        match env::build(ctx, record, "startup", &Context::default()) {
            Ok(vars) => match env::spawn_detached_group(&record.path, &step.command, &vars, &log) {
                Ok(pid) => running::record(ctx, &record.id, pid),
                Err(err) => log::warn!(
                    "plugin '{}': startup command failed to spawn, {err:#}",
                    record.id
                ),
            },
            Err(err) => {
                log::warn!(
                    "plugin '{}': building startup env failed, {err:#}",
                    record.id
                );
            }
        }
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Test-only helpers shared across this module's submodules' unit tests (a hand-built
/// [`crate::paths::Paths`] rooted at a tempdir, every field is independent, so tests never
/// need `Paths::from_env`/env vars).
#[cfg(test)]
pub(crate) mod tests_support {
    pub fn test_paths(dir: &std::path::Path) -> crate::paths::Paths {
        let plugins_dir = dir.join("plugins");
        crate::paths::Paths {
            config_dir: dir.join("config"),
            config_file: dir.join("config").join("config.toml"),
            state_dir: dir.join("state"),
            override_dir: dir.join("config").join("agent-detection"),
            remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
            sessions_dir: dir.join("state").join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.join("state").join("logs"),
            manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
        }
    }

    /// Serializes every test in this crate that mutates `HERDR_SOCKET_PATH`/
    /// `TAW_SOCKET_PATH` in the current process, real process-global state inherited by
    /// any child process any test spawns. Shared across `env.rs`'s (`run_sync`/
    /// `spawn_detached`) and `install.rs`'s (`run_build_commands`) socket-stripping tests
    /// so two of them running concurrently on different threads can never race each other
    /// by setting/unsetting the same var names at overlapping times (same reasoning as
    /// `src/paths.rs`'s `ENV_LOCK`, just shared across two files instead of private to one).
    pub static SOCKET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use tests_support::test_paths;

    fn write_plugin_dir(dir: &std::path::Path, name: &str, toml: &str) -> std::path::PathBuf {
        let plugin_dir = dir.join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join(MANIFEST_FILE), toml).unwrap();
        plugin_dir
    }

    fn ctx<'a>(
        tmux: &'a crate::tmux::Tmux,
        cfg: &'a crate::config::Config,
        paths: &'a crate::paths::Paths,
        taw_bin: &'a std::path::Path,
    ) -> PluginCtx<'a> {
        PluginCtx {
            tmux,
            cfg,
            paths,
            taw_bin,
            server_env: None,
        }
    }

    const SIMPLE_MANIFEST: &str = r#"
id = "simple"
version = "1.0.0"
min_herdr_version = "0.7.0"
"#;

    #[test]
    fn link_registers_and_config_dir_and_log_path_are_scoped_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(dir.path(), "simple", SIMPLE_MANIFEST);
        let record = link(&c, &plugin_dir, true).unwrap();

        assert_eq!(record.id, "simple");
        assert_eq!(record.source, registry::PluginSource::Link);
        assert!(record.enabled);
        assert_eq!(list(&c).unwrap(), vec![record]);

        let config_dir = config_dir(&c, "simple").unwrap();
        assert!(config_dir.ends_with("plugins/simple"));
        assert!(config_dir.is_dir());
        assert_eq!(
            log_path(&c, "simple"),
            paths.plugin_state_dir.join("simple").join("plugin.log")
        );
    }

    #[test]
    fn link_rejects_a_manifest_whose_min_herdr_version_is_too_new() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(
            dir.path(),
            "too-new",
            "id = \"too-new\"\nversion = \"1.0.0\"\nmin_herdr_version = \"99.0.0\"\n",
        );
        let err = link(&c, &plugin_dir, true).unwrap_err();
        assert_eq!(
            err.downcast_ref::<install::InstallError>(),
            Some(&install::InstallError::UnsupportedPluginVersion {
                required: "99.0.0".to_string(),
                compat: cfg.plugins.compat_herdr_version.clone(),
            })
        );
        assert!(list(&c).unwrap().is_empty());
    }

    #[test]
    fn unlink_unregisters_without_touching_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(dir.path(), "simple", SIMPLE_MANIFEST);
        link(&c, &plugin_dir, true).unwrap();

        unlink(&c, "simple").unwrap();

        assert!(list(&c).unwrap().is_empty());
        assert!(
            plugin_dir.join(MANIFEST_FILE).exists(),
            "unlink must not delete plugin files"
        );
    }

    #[test]
    fn uninstall_of_a_linked_plugin_does_not_delete_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(dir.path(), "simple", SIMPLE_MANIFEST);
        link(&c, &plugin_dir, true).unwrap();

        uninstall(&c, "simple").unwrap();

        assert!(list(&c).unwrap().is_empty());
        assert!(plugin_dir.join(MANIFEST_FILE).exists());
    }

    #[test]
    fn uninstall_of_a_github_plugin_deletes_its_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let checkout = install::checkout_dir(&paths, "owner", "repo");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(
            checkout.join(MANIFEST_FILE),
            "id = \"gh\"\nversion=\"1\"\nmin_herdr_version=\"0.7.0\"\n",
        )
        .unwrap();
        registry::add(
            &paths,
            registry::PluginRecord {
                id: "gh".to_string(),
                source: registry::PluginSource::Github {
                    owner: "owner".to_string(),
                    repo: "repo".to_string(),
                    subdir: None,
                    requested_ref: None,
                    resolved_commit: Some("abc123".to_string()),
                },
                path: checkout.clone(),
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        uninstall(&c, "gh").unwrap();

        assert!(list(&c).unwrap().is_empty());
        assert!(
            !checkout.exists(),
            "uninstall must delete the managed checkout"
        );
    }

    /// A GitHub record at `path`, as `registry.json` could be edited to say.
    fn edited_record(id: &str, path: std::path::PathBuf) -> registry::PluginRecord {
        registry::PluginRecord {
            id: id.to_string(),
            source: registry::PluginSource::Github {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                subdir: None,
                requested_ref: None,
                resolved_commit: Some("abc123".to_string()),
            },
            path,
            enabled: true,
            installed_unix_ms: 0,
        }
    }

    #[test]
    fn uninstall_refuses_a_recorded_path_with_a_dotdot_and_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);
        // Inside the plugins folder, but not under github: a path that starts with
        // `plugins/github` and walks back out of it.
        let victim = paths.plugins_dir.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep"), "mine").unwrap();
        let path = paths.plugins_dir.join("github").join("..").join("victim");
        registry::add(&paths, edited_record("gh", path)).unwrap();

        let err = uninstall(&c, "gh").unwrap_err();

        assert!(format!("{err:#}").contains("'..'"), "{err:#}");
        assert!(victim.join("keep").exists(), "the folder was removed");
        assert_eq!(list(&c).unwrap().len(), 1, "the record was removed");
    }

    #[test]
    fn uninstall_refuses_a_folder_that_leads_outside_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);
        let victim = dir.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep"), "mine").unwrap();
        let link = install::checkout_dir(&paths, "owner", "repo");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        registry::add(&paths, edited_record("gh", link)).unwrap();

        let err = uninstall(&c, "gh").unwrap_err();

        assert!(format!("{err:#}").contains("is not inside"), "{err:#}");
        assert!(victim.join("keep").exists(), "the folder was removed");
        assert_eq!(list(&c).unwrap().len(), 1, "the record was removed");
    }

    #[test]
    fn uninstall_refuses_the_github_folder_and_an_owner_folder_and_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);
        let github = paths.plugins_dir.join("github");
        let owner = github.join("owner");
        let other = owner.join("other@abc123");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("keep"), "another plugin").unwrap();
        for (i, path) in [github.clone(), owner.clone()].into_iter().enumerate() {
            let id = format!("gh{i}");
            registry::add(&paths, edited_record(&id, path.clone())).unwrap();

            let err = uninstall(&c, &id).unwrap_err();

            assert!(
                format!("{err:#}").contains("so uninstall removes nothing"),
                "{err:#}"
            );
            assert!(
                other.join("keep").exists(),
                "{} was removed",
                path.display()
            );
            assert_eq!(list(&c).unwrap().len(), i + 1, "a record was removed");
        }
        // A folder strictly below an owner folder is still removed.
        let own = owner.join("repo@abc123");
        std::fs::create_dir_all(&own).unwrap();
        registry::add(&paths, edited_record("fine", own.clone())).unwrap();
        uninstall(&c, "fine").unwrap();
        assert!(!own.exists());
        assert!(other.join("keep").exists());
    }

    #[test]
    fn uninstall_of_an_unregistered_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);
        assert!(uninstall(&c, "nope").is_err());
    }

    #[test]
    fn set_enabled_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(dir.path(), "simple", SIMPLE_MANIFEST);
        link(&c, &plugin_dir, true).unwrap();

        set_enabled(&c, "simple", false).unwrap();
        assert!(!list(&c).unwrap()[0].enabled);
    }

    #[test]
    fn startup_runs_only_enabled_plugins_and_returns_their_ids() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let marker = dir.path().join("started");
        let enabled_dir = write_plugin_dir(
            dir.path(),
            "enabled-plugin",
            &format!(
                "id = \"enabled-plugin\"\nversion=\"1\"\nmin_herdr_version=\"0.7.0\"\n\n[[startup]]\ncommand = [\"sh\", \"-c\", \"touch {}\"]\n",
                marker.display()
            ),
        );
        let disabled_dir = write_plugin_dir(dir.path(), "disabled-plugin", SIMPLE_MANIFEST);
        link(&c, &enabled_dir, true).unwrap();
        link(&c, &disabled_dir, false).unwrap();

        let started = startup(&c).unwrap();
        assert_eq!(started, vec!["enabled-plugin".to_string()]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "enabled plugin's startup hook did not run");
    }

    /// `disable` ends what `startup` started for that plugin (a loop it left running, here a `sleep`
    /// that writes its pid), by the pid and process group it recorded, and for no other plugin.
    #[test]
    fn disable_and_uninstall_end_what_startup_started_for_that_plugin_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let pids = dir.path().join("pids");
        std::fs::create_dir_all(&pids).unwrap();
        let manifest = |id: &str| {
            format!(
                "id = \"{id}\"\nversion=\"1\"\nmin_herdr_version=\"0.7.0\"\n\n[[startup]]\ncommand = [\"sh\", \"-c\", \"sleep 60 & echo $! > {}/{id}; wait\"]\n",
                pids.display()
            )
        };
        for id in ["loops", "other", "gone"] {
            let plugin_dir = write_plugin_dir(dir.path(), id, &manifest(id));
            link(&c, &plugin_dir, true).unwrap();
        }
        startup(&c).unwrap();
        let read_pid = |id: &str| -> i32 {
            let file = pids.join(id);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !file.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // The file may be seen before the line is complete.
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("no pid of {id}: {e}"))
                .trim()
                .parse()
                .unwrap()
        };
        let (loops, other, gone) = (read_pid("loops"), read_pid("other"), read_pid("gone"));
        // Ends every `sleep` this test made, whatever the assertions do.
        struct Guard(Vec<i32>);
        impl Drop for Guard {
            fn drop(&mut self) {
                for pid in &self.0 {
                    // SAFETY: two integers, no memory; the pids are children this test started.
                    unsafe { libc::kill(*pid, libc::SIGKILL) };
                }
            }
        }
        let _guard = Guard(vec![loops, other, gone]);
        let alive = |pid: i32| {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .map(|out| {
                    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    !stat.is_empty() && !stat.starts_with('Z')
                })
                .unwrap_or(false)
        };
        let ends = |pid: i32| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while alive(pid) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            !alive(pid)
        };
        assert!(alive(loops) && alive(other) && alive(gone));

        set_enabled(&c, "loops", false).unwrap();
        assert!(
            ends(loops),
            "disable left the plugin's startup process running"
        );
        assert!(alive(other), "disabling one plugin ended another's process");

        uninstall(&c, "gone").unwrap();
        assert!(
            ends(gone),
            "uninstall left the plugin's startup process running"
        );
        assert!(
            alive(other),
            "uninstalling one plugin ended another's process"
        );
    }

    #[test]
    fn startup_skips_a_plugin_whose_manifest_no_longer_loads_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let plugin_dir = write_plugin_dir(dir.path(), "broken", SIMPLE_MANIFEST);
        link(&c, &plugin_dir, true).unwrap();
        // Corrupt the manifest after linking, as if the plugin dir changed underneath us.
        std::fs::write(plugin_dir.join(MANIFEST_FILE), "not valid toml =====").unwrap();

        let started = startup(&c).unwrap();
        assert!(started.is_empty());
    }

    #[test]
    fn uninstall_keeps_a_checkout_shared_by_a_sibling_plugin_from_the_same_repo() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        // A monorepo-style checkout with two separately-registered plugins under different
        // subdirs, mirrors this project's own echo-plugin fixture layout, and real repos
        // like someone/plugin-examples.
        let checkout = install::checkout_dir(&paths, "owner", "monorepo");
        let plugin_a_dir = checkout.join("plugin-a");
        let plugin_b_dir = checkout.join("plugin-b");
        std::fs::create_dir_all(&plugin_a_dir).unwrap();
        std::fs::create_dir_all(&plugin_b_dir).unwrap();
        std::fs::write(
            plugin_a_dir.join(MANIFEST_FILE),
            "id = \"a\"\nversion = \"1\"\nmin_herdr_version = \"0.7.0\"\n",
        )
        .unwrap();
        std::fs::write(
            plugin_b_dir.join(MANIFEST_FILE),
            "id = \"b\"\nversion = \"1\"\nmin_herdr_version = \"0.7.0\"\n",
        )
        .unwrap();
        let source_for = |subdir: &str| registry::PluginSource::Github {
            owner: "owner".to_string(),
            repo: "monorepo".to_string(),
            subdir: Some(subdir.to_string()),
            requested_ref: None,
            resolved_commit: Some("abc123".to_string()),
        };
        registry::add(
            &paths,
            registry::PluginRecord {
                id: "a".to_string(),
                source: source_for("plugin-a"),
                path: plugin_a_dir.clone(),
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();
        registry::add(
            &paths,
            registry::PluginRecord {
                id: "b".to_string(),
                source: source_for("plugin-b"),
                path: plugin_b_dir.clone(),
                enabled: true,
                installed_unix_ms: 0,
            },
        )
        .unwrap();

        uninstall(&c, "a").unwrap();

        // "a" is unregistered...
        assert!(registry::find(&paths, "a").unwrap().is_none());
        // ...but "b" (a sibling plugin from the same repo checkout) is untouched, files and
        // all, the shared checkout must not have been deleted.
        assert!(registry::find(&paths, "b").unwrap().is_some());
        assert!(
            checkout.exists(),
            "shared checkout was deleted even though plugin 'b' still references it"
        );
        assert!(plugin_b_dir.join(MANIFEST_FILE).exists());

        // Once the last plugin referencing this repo is also uninstalled, the checkout
        // finally goes away.
        uninstall(&c, "b").unwrap();
        assert!(
            !checkout.exists(),
            "checkout should be removed once no plugin references it"
        );
    }

    /// Runs `git <args>`, for building a local hermetic "remote" repo in tests, never
    /// touches the network or the default tmux server.
    fn git(args: &[&str]) {
        let status = crate::git::command().args(args).status().unwrap();
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    fn git_capture(args: &[&str]) -> String {
        let output = crate::git::command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            output.status
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_manifest(remote: &std::path::Path, manifest_toml: &str, message: &str) {
        std::fs::write(remote.join(MANIFEST_FILE), manifest_toml).unwrap();
        let remote_str = remote.to_str().unwrap();
        git(&["-C", remote_str, "add", "."]);
        git(&[
            "-C",
            remote_str,
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ]);
    }

    /// Every file and folder under `dir` with its size, sorted, and the names of the folders
    /// beside `dir`: what a declined reinstall must leave as it was.
    fn folder_listing(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(folder) = stack.pop() {
            for entry in std::fs::read_dir(&folder).unwrap().flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(dir).unwrap().display().to_string();
                if path.is_dir() {
                    out.push(format!("{rel}/"));
                    stack.push(path);
                } else {
                    out.push(format!("{rel} {}", std::fs::metadata(&path).unwrap().len()));
                }
            }
        }
        for entry in std::fs::read_dir(dir.parent().unwrap()).unwrap().flatten() {
            out.push(format!("beside: {}", entry.file_name().to_string_lossy()));
        }
        out.sort();
        out
    }

    #[test]
    fn declining_a_reinstall_leaves_the_live_checkout_at_the_previously_confirmed_manifest() {
        // A local "remote" repo whose first commit has one startup command, and a second
        // commit that changes it, simulates a plugin publishing a new, unreviewed startup
        // hook after the user already installed (and trusted) an earlier version.
        let remote = tempfile::tempdir().unwrap();
        git(&["init", "-q", "-b", "main", remote.path().to_str().unwrap()]);
        commit_manifest(
            remote.path(),
            "id = \"refresh-test\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n\n[[startup]]\ncommand = [\"sh\", \"-c\", \"echo old\"]\n",
            "first",
        );
        let first_commit =
            git_capture(&["-C", remote.path().to_str().unwrap(), "rev-parse", "HEAD"]);

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let url = format!("file://{}", remote.path().display());
        let spec = || install::Spec {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            subdir: None,
            requested_ref: None,
        };

        // First install: yes=true, succeeds and registers the trusted (first) commit.
        let record =
            install_from_url(&c, spec(), &url, true, &|| Ok(None), &install::GitFetcher).unwrap();
        let checkout = record.path.clone();
        assert_eq!(record.id, "refresh-test");
        let registry::PluginSource::Github {
            resolved_commit, ..
        } = &record.source
        else {
            panic!("expected a Github source");
        };
        assert_eq!(resolved_commit.as_deref(), Some(first_commit.as_str()));
        let old_manifest = std::fs::read_to_string(checkout.join(MANIFEST_FILE)).unwrap();
        assert!(old_manifest.contains("echo old"));

        // The "remote" publishes a new, different (unreviewed) startup command.
        commit_manifest(
            remote.path(),
            "id = \"refresh-test\"\nversion = \"2.0.0\"\nmin_herdr_version = \"0.7.0\"\n\n[[startup]]\ncommand = [\"sh\", \"-c\", \"echo new-and-unreviewed\"]\n",
            "second",
        );

        // Re-install, declining (yes=false, non-tty): must fail trust_required...
        let before = folder_listing(&checkout);
        let err = install_from_url(&c, spec(), &url, false, &|| Ok(None), &install::GitFetcher)
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<install::InstallError>(),
            Some(&install::InstallError::TrustRequired)
        );
        assert_eq!(
            folder_listing(&checkout),
            before,
            "the live folder changed, or the new one was left beside it"
        );

        // ...and the *live* checkout on disk must still be the OLD, already-confirmed
        // manifest, not silently left at the refreshed-but-declined one, which would run
        // unconfirmed on the next `taw plugin startup`.
        let live_manifest = std::fs::read_to_string(checkout.join(MANIFEST_FILE)).unwrap();
        assert!(
            live_manifest.contains("echo old"),
            "checkout was left at the unreviewed version after a declined install: {live_manifest}"
        );
        assert!(!live_manifest.contains("echo new-and-unreviewed"));

        // The registry record is untouched too: still enabled, still the original commit.
        let still_registered = registry::find(&paths, "refresh-test").unwrap().unwrap();
        assert!(still_registered.enabled);
        let registry::PluginSource::Github {
            resolved_commit, ..
        } = &still_registered.source
        else {
            panic!("expected a Github source");
        };
        assert_eq!(resolved_commit.as_deref(), Some(first_commit.as_str()));
    }

    #[test]
    fn declining_a_first_time_install_removes_the_fresh_clone() {
        let remote = tempfile::tempdir().unwrap();
        git(&["init", "-q", "-b", "main", remote.path().to_str().unwrap()]);
        commit_manifest(
            remote.path(),
            "id = \"fresh-test\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n\n[[build]]\ncommand = [\"true\"]\n",
            "first",
        );

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = crate::config::Config::default();
        let tmux = crate::tmux::Tmux::with_socket_name("taw-plugin-mod-test-unused");
        let taw_bin = std::path::PathBuf::from("/bin/true");
        let c = ctx(&tmux, &cfg, &paths, &taw_bin);

        let url = format!("file://{}", remote.path().display());
        let spec = install::Spec {
            owner: "owner".to_string(),
            repo: "fresh".to_string(),
            subdir: None,
            requested_ref: None,
        };

        let err = install_from_url(&c, spec, &url, false, &|| Ok(None), &install::GitFetcher)
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<install::InstallError>(),
            Some(&install::InstallError::TrustRequired)
        );
        let github = paths.plugins_dir.join("github").join("owner");
        let left: Vec<_> = std::fs::read_dir(&github)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            left.is_empty(),
            "a declined first-time install must not leave a fresh clone behind: {left:?}"
        );
        assert!(registry::find(&paths, "fresh-test").unwrap().is_none());
    }
}
