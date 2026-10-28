// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! `<state>/plugins/registry.json`: installed/linked plugin records.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::paths::Paths;

/// Where a plugin's files came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PluginSource {
    Github {
        owner: String,
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subdir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_commit: Option<String>,
    },
    Link,
}

/// One registered plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    pub id: String,
    pub source: PluginSource,
    /// The plugin's own directory (containing `herdr-plugin.toml`) — for a GitHub install
    /// with a `subdir`, this is the checkout dir joined with that subdir, not the checkout
    /// root itself.
    pub path: PathBuf,
    pub enabled: bool,
    pub installed_unix_ms: u64,
}

/// The full contents of `registry.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub plugins: Vec<PluginRecord>,
}

fn registry_path(paths: &Paths) -> PathBuf {
    paths.plugins_dir.join("registry.json")
}

/// Loads `registry.json`, or an empty [`Registry`] when the file does not exist yet.
pub fn load(paths: &Paths) -> anyhow::Result<Registry> {
    let path = registry_path(paths);
    if !path.exists() {
        return Ok(Registry::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

// Follows herdr's src/persist/plugin_registry.rs. Differences: the
// symlink walk is integration::write_config_file's, which also refuses a hard-linked registry.
/// Writes `registry.json` atomically. If it is a symlink — dotfiles-managed, pointing at
/// a checkout elsewhere — the write follows it and replaces what it points at, leaving
/// the symlink itself in place; a naive `fs::rename` onto the link path would instead
/// silently replace the symlink itself with a plain file. Follows herdr's
/// `persist/plugin_registry.rs` `resolve_write_target`, by way of
/// [`crate::integration::write_config_file`], the same protected write every agent config
/// edit already goes through, rather than duplicating its symlink walk here.
pub fn save(paths: &Paths, registry: &Registry) -> anyhow::Result<()> {
    let path = registry_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(registry).context("serializing registry.json")?;
    crate::integration::write_config_file(&path, json.as_bytes())
}

/// Registers `record`, replacing any existing record with the same `id` where it stands, so
/// a reinstall or an update keeps the plugin's place in `plugin list`.
pub fn add(paths: &Paths, record: PluginRecord) -> anyhow::Result<()> {
    add_all(paths, vec![record])
}

/// [`add`] for several records in one atomic write of `registry.json`: plugins that share a
/// checkout switch to its new folder together or not at all.
pub fn add_all(paths: &Paths, records: Vec<PluginRecord>) -> anyhow::Result<()> {
    let mut registry = load(paths)?;
    for record in records {
        match registry.plugins.iter_mut().find(|p| p.id == record.id) {
            Some(slot) => *slot = record,
            None => registry.plugins.push(record),
        }
    }
    save(paths, &registry)
}

/// Unregisters the plugin with id `id`. Not an error when no such plugin is registered —
/// callers that need to distinguish "was registered" should check [`find`] first.
pub fn remove(paths: &Paths, id: &str) -> anyhow::Result<()> {
    let mut registry = load(paths)?;
    registry.plugins.retain(|p| p.id != id);
    save(paths, &registry)
}

/// Sets the `enabled` flag on the plugin with id `id`.
pub fn set_enabled(paths: &Paths, id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut registry = load(paths)?;
    let record = registry
        .plugins
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow::anyhow!("plugin '{id}' not found"))?;
    record.enabled = enabled;
    save(paths, &registry)
}

/// Looks up the plugin with id `id`, if registered.
pub fn find(paths: &Paths, id: &str) -> anyhow::Result<Option<PluginRecord>> {
    Ok(load(paths)?.plugins.into_iter().find(|p| p.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &std::path::Path) -> Paths {
        let plugins_dir = dir.join("plugins");
        Paths {
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

    fn github_record(id: &str) -> PluginRecord {
        PluginRecord {
            id: id.to_string(),
            source: PluginSource::Github {
                owner: "someone".to_string(),
                repo: "plugin-examples".to_string(),
                subdir: Some("agent-telegram-notify".to_string()),
                requested_ref: None,
                resolved_commit: Some("deadbeef".to_string()),
            },
            path: PathBuf::from(
                "/state/plugins/github/someone/plugin-examples/agent-telegram-notify",
            ),
            enabled: true,
            installed_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn add_replaces_a_record_where_it_stands() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        for id in ["a", "b", "c"] {
            add(&paths, github_record(id)).unwrap();
        }
        let mut newer = github_record("a");
        newer.installed_unix_ms += 1;
        add(&paths, newer.clone()).unwrap();

        let registry = load(&paths).unwrap();
        let ids: Vec<&str> = registry.plugins.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(registry.plugins[0], newer);
    }

    #[test]
    fn load_with_no_file_returns_an_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(load(&paths).unwrap(), Registry::default());
    }

    #[test]
    fn add_then_load_round_trips_a_github_record() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());

        add(&paths, github_record("examples.agent-telegram-notify")).unwrap();

        let registry = load(&paths).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(
            registry.plugins[0],
            github_record("examples.agent-telegram-notify")
        );

        // The file itself is pretty-printed JSON with the documented shape.
        let raw = std::fs::read_to_string(paths.plugins_dir.join("registry.json")).unwrap();
        assert!(raw.contains("\"kind\": \"github\""));
        assert!(raw.contains("\n")); // pretty (multi-line), not a single compact line
        assert!(!paths.plugins_dir.join("registry.json.tmp").exists());
    }

    #[test]
    fn add_replaces_an_existing_record_with_the_same_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        add(&paths, github_record("dup")).unwrap();

        let mut updated = github_record("dup");
        updated.enabled = false;
        add(&paths, updated.clone()).unwrap();

        let registry = load(&paths).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0], updated);
    }

    #[test]
    fn link_source_round_trips_as_kind_link_with_no_extra_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let record = PluginRecord {
            id: "example.layout".to_string(),
            source: PluginSource::Link,
            path: PathBuf::from("/home/user/dev/example.layout"),
            enabled: true,
            installed_unix_ms: 42,
        };
        add(&paths, record.clone()).unwrap();

        let raw = std::fs::read_to_string(paths.plugins_dir.join("registry.json")).unwrap();
        assert!(raw.contains("\"kind\": \"link\""));
        assert!(!raw.contains("owner"));

        assert_eq!(find(&paths, "example.layout").unwrap(), Some(record));
    }

    #[test]
    fn remove_unregisters_a_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        add(&paths, github_record("a")).unwrap();
        add(&paths, github_record("b")).unwrap();

        remove(&paths, "a").unwrap();

        let registry = load(&paths).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].id, "b");
    }

    #[test]
    fn remove_of_an_unknown_id_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(remove(&paths, "nope").is_ok());
    }

    #[test]
    fn set_enabled_flips_the_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        add(&paths, github_record("a")).unwrap();

        set_enabled(&paths, "a", false).unwrap();
        assert!(!find(&paths, "a").unwrap().unwrap().enabled);

        set_enabled(&paths, "a", true).unwrap();
        assert!(find(&paths, "a").unwrap().unwrap().enabled);
    }

    #[test]
    fn set_enabled_on_an_unknown_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(set_enabled(&paths, "nope", true).is_err());
    }

    #[test]
    fn find_returns_none_for_an_unregistered_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(find(&paths, "nope").unwrap(), None);
    }

    /// Follows herdr's `plugin_registry.rs` `save_preserves_relative_registry_symlink`
    /// (v0.9.1, #3922): a registry managed by dotfiles (a relative symlink into a checkout
    /// elsewhere in `<state>/plugins/`) must still be a symlink afterward, not a plain file
    /// `fs::rename` silently swapped in over it. Calls `save` directly, the function this
    /// task changes — `add`'s own read-modify-write semantics (keep other ids, replace a
    /// matching one) are a different, already-tested concern.
    #[cfg(unix)]
    #[test]
    fn save_preserves_a_relative_registry_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        std::fs::create_dir_all(&paths.plugins_dir).unwrap();

        // The real file lives in a sibling directory dotfiles would manage.
        let real_dir = paths.plugins_dir.join("dotfiles-checkout");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_path = real_dir.join("registry.json");
        let mut real_paths = paths_in(tmp.path());
        real_paths.plugins_dir = real_dir.clone();
        save(
            &real_paths,
            &Registry {
                plugins: vec![github_record("example.first")],
            },
        )
        .unwrap();
        assert!(real_path.is_file());

        let link_path = registry_path(&paths);
        std::os::unix::fs::symlink("dotfiles-checkout/registry.json", &link_path).unwrap();

        save(
            &paths,
            &Registry {
                plugins: vec![github_record("example.second")],
            },
        )
        .unwrap();

        assert!(
            std::fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "saving the registry must preserve its symlink"
        );
        assert_eq!(
            std::fs::read_link(&link_path).unwrap(),
            std::path::Path::new("dotfiles-checkout/registry.json")
        );
        let registry = load(&paths).unwrap();
        assert_eq!(registry.plugins.len(), 1);
        assert_eq!(registry.plugins[0].id, "example.second");
        assert_eq!(
            std::fs::read(&link_path).unwrap(),
            std::fs::read(&real_path).unwrap()
        );
    }
}
