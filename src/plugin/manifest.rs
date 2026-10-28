// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! `herdr-plugin.toml` manifest: parsing and platform filtering.
//!
//! Modified from herdr's plugin manifest format @v0.8.2 — see
//! `third_party/herdr/reference/plugins.mdx` "Manifest". `id`, `version`, and
//! `min_herdr_version` are required; every other section is optional and defaults to
//! absent/empty. Unknown keys are ignored (we do not use `deny_unknown_fields`), so a
//! manifest written for a newer herdr keeps parsing here.

use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

/// One of the platforms a manifest, or a single item within it, can restrict itself to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

/// The platform this `taw` binary is running on: `macos` on macOS, `linux` everywhere
/// else (the two platforms the herdr plugin ecosystem in practice ships for).
pub fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

/// Whether an item (build step, startup hook, action, event hook, pane, link handler)
/// with its own optional `platforms` may run here. Item-level `platforms` fully replaces
/// the manifest's top-level list when present (it does not merge with it) — see
/// plugins.mdx "Use `platforms = [...]` ... item-level platforms override the top-level
/// list." Absent everywhere means "runs on every platform".
fn item_allowed(
    manifest_platforms: &Option<Vec<Platform>>,
    item_platforms: &Option<Vec<Platform>>,
) -> bool {
    match item_platforms.as_ref().or(manifest_platforms.as_ref()) {
        None => true,
        Some(list) => list.contains(&current_platform()),
    }
}

/// One `[[build]]` or `[[startup]]` entry: an argv command, optionally platform-filtered.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Step {
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

/// One `[[actions]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ActionDef {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub contexts: Vec<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

/// One `[[events]]` entry: `on` is an event name from
/// `third_party/herdr/reference/event-names.txt`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EventDef {
    pub on: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

fn default_placement() -> String {
    "overlay".to_string()
}

/// One `[[panes]]` entry. `placement` defaults to `"overlay"` (see plugins.mdx "Manifest
/// pane `placement` defaults to `overlay`"); this crate only *acts* on `"overlay"`,
/// `"side"`, and `"bottom"` (see `panes::open`) but parses whatever string is present so a
/// manifest using another herdr placement still loads.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PaneDef {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_placement")]
    pub placement: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

/// One `[[link_handlers]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LinkHandlerDef {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub pattern: String,
    pub action: String,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
}

/// A parsed `herdr-plugin.toml`. `id`, `version`, and `min_herdr_version` are required by
/// TOML deserialization (missing any of them is a parse error); every other field is
/// optional and defaults to `None`/empty.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub version: String,
    pub min_herdr_version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub platforms: Option<Vec<Platform>>,
    #[serde(default)]
    pub build: Vec<Step>,
    #[serde(default)]
    pub startup: Vec<Step>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
    #[serde(default)]
    pub events: Vec<EventDef>,
    #[serde(default)]
    pub panes: Vec<PaneDef>,
    #[serde(default)]
    pub link_handlers: Vec<LinkHandlerDef>,
}

impl PluginManifest {
    /// `[[build]]` steps allowed to run on the current platform, in manifest order.
    pub fn build_steps(&self) -> impl Iterator<Item = &Step> + '_ {
        self.build
            .iter()
            .filter(|s| item_allowed(&self.platforms, &s.platforms))
    }

    /// `[[startup]]` steps allowed to run on the current platform, in manifest order.
    pub fn startup_steps(&self) -> impl Iterator<Item = &Step> + '_ {
        self.startup
            .iter()
            .filter(|s| item_allowed(&self.platforms, &s.platforms))
    }

    /// `[[actions]]` entries allowed to run on the current platform.
    pub fn matching_actions(&self) -> impl Iterator<Item = &ActionDef> + '_ {
        self.actions
            .iter()
            .filter(|a| item_allowed(&self.platforms, &a.platforms))
    }

    /// The action with local id `id`, if it exists and is allowed on this platform.
    pub fn find_action(&self, id: &str) -> Option<&ActionDef> {
        self.matching_actions().find(|a| a.id == id)
    }

    /// `[[events]]` entries whose `on == name`, allowed on this platform.
    pub fn matching_events<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a EventDef> + 'a {
        self.events
            .iter()
            .filter(move |e| e.on == name && item_allowed(&self.platforms, &e.platforms))
    }

    /// The pane entrypoint with local id `id`, if it exists and is allowed on this
    /// platform.
    pub fn find_pane(&self, id: &str) -> Option<&PaneDef> {
        self.panes
            .iter()
            .filter(|p| item_allowed(&self.platforms, &p.platforms))
            .find(|p| p.id == id)
    }
}

/// Parses a `herdr-plugin.toml` document already read into memory.
pub fn parse(toml_str: &str) -> anyhow::Result<PluginManifest> {
    toml::from_str(toml_str).context("parsing herdr-plugin.toml")
}

/// Reads and parses the manifest at `path` (the full path to `herdr-plugin.toml`, not its
/// containing directory).
pub fn load(path: &Path) -> anyhow::Result<PluginManifest> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&raw).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From plugins.mdx "Manifest" — the full-featured example with every section.
    const LAYOUT_EXAMPLE: &str = r#"
id = "example.layout"
name = "Layout"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Apply project layouts"
platforms = ["linux", "macos", "windows"]

[[build]]
command = ["npm", "ci"]

[[build]]
command = ["npm", "run", "build"]
platforms = ["linux", "macos"]

[[startup]]
command = ["node", "dist/restore.js"]

[[actions]]
id = "apply"
title = "Apply layout"
contexts = ["workspace"]
command = ["node", "dist/apply.js"]

[[events]]
on = "worktree.created"
command = ["herdr", "workspace", "list"]

[[panes]]
id = "board"
title = "Project board"
placement = "overlay"
command = ["herdr-board"]

[[link_handlers]]
id = "github-issue"
title = "Open GitHub issue"
pattern = "^https://github\\.com/[^/]+/[^/]+/(issues|pull)/[0-9]+$"
action = "apply"
"#;

    /// From plugins.mdx "First plugin" — only `id`/`name`/`version`/`min_herdr_version`/
    /// `description`/`platforms` plus one action; every other section absent.
    const WORKSPACE_TOOLS_EXAMPLE: &str = r#"
id = "example.workspace-tools"
name = "Workspace Tools"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Small workspace helpers"
platforms = ["linux", "macos", "windows"]

[[actions]]
id = "list-workspaces"
title = "List workspaces"
contexts = ["workspace"]
command = ["node", "index.js"]
"#;

    #[test]
    fn parses_the_full_layout_example_from_plugins_mdx() {
        let manifest = parse(LAYOUT_EXAMPLE).unwrap();

        assert_eq!(manifest.id, "example.layout");
        assert_eq!(manifest.name.as_deref(), Some("Layout"));
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.min_herdr_version, "0.7.0");
        assert_eq!(
            manifest.description.as_deref(),
            Some("Apply project layouts")
        );
        assert_eq!(
            manifest.platforms,
            Some(vec![Platform::Linux, Platform::Macos, Platform::Windows])
        );

        assert_eq!(manifest.build.len(), 2);
        assert_eq!(manifest.build[0].command, vec!["npm", "ci"]);
        assert_eq!(manifest.build[1].command, vec!["npm", "run", "build"]);
        assert_eq!(
            manifest.build[1].platforms,
            Some(vec![Platform::Linux, Platform::Macos])
        );

        assert_eq!(manifest.startup.len(), 1);
        assert_eq!(manifest.startup[0].command, vec!["node", "dist/restore.js"]);

        assert_eq!(manifest.actions.len(), 1);
        let action = &manifest.actions[0];
        assert_eq!(action.id, "apply");
        assert_eq!(action.title.as_deref(), Some("Apply layout"));
        assert_eq!(action.contexts, vec!["workspace"]);
        assert_eq!(action.command, vec!["node", "dist/apply.js"]);

        assert_eq!(manifest.events.len(), 1);
        assert_eq!(manifest.events[0].on, "worktree.created");
        assert_eq!(
            manifest.events[0].command,
            vec!["herdr", "workspace", "list"]
        );

        assert_eq!(manifest.panes.len(), 1);
        assert_eq!(manifest.panes[0].id, "board");
        assert_eq!(manifest.panes[0].title.as_deref(), Some("Project board"));
        assert_eq!(manifest.panes[0].placement, "overlay");
        assert_eq!(manifest.panes[0].command, vec!["herdr-board"]);

        assert_eq!(manifest.link_handlers.len(), 1);
        let handler = &manifest.link_handlers[0];
        assert_eq!(handler.id, "github-issue");
        assert_eq!(handler.action, "apply");
        assert!(handler.pattern.contains("github"));
    }

    #[test]
    fn parses_the_minimal_workspace_tools_example_with_absent_sections_empty() {
        let manifest = parse(WORKSPACE_TOOLS_EXAMPLE).unwrap();

        assert_eq!(manifest.id, "example.workspace-tools");
        assert_eq!(manifest.name.as_deref(), Some("Workspace Tools"));
        assert!(manifest.build.is_empty());
        assert!(manifest.startup.is_empty());
        assert!(manifest.events.is_empty());
        assert!(manifest.panes.is_empty());
        assert!(manifest.link_handlers.is_empty());
        assert_eq!(manifest.actions.len(), 1);
        assert_eq!(manifest.actions[0].id, "list-workspaces");
    }

    #[test]
    fn only_id_version_and_min_herdr_version_are_required() {
        let manifest = parse(
            r#"
id = "bare.plugin"
version = "1.0.0"
min_herdr_version = "0.7.0"
"#,
        )
        .unwrap();

        assert_eq!(manifest.id, "bare.plugin");
        assert_eq!(manifest.name, None);
        assert_eq!(manifest.description, None);
        assert_eq!(manifest.platforms, None);
        assert!(manifest.build.is_empty());
    }

    #[test]
    fn missing_id_is_a_parse_error() {
        let err = parse(
            r#"
version = "1.0.0"
min_herdr_version = "0.7.0"
"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("id"));
    }

    #[test]
    fn missing_version_is_a_parse_error() {
        assert!(
            parse(
                r#"id = "x"
min_herdr_version = "0.7.0""#
            )
            .is_err()
        );
    }

    #[test]
    fn missing_min_herdr_version_is_a_parse_error() {
        assert!(
            parse(
                r#"id = "x"
version = "1.0.0""#
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let manifest = parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"
some_future_key = "whatever"

[[actions]]
id = "a"
command = ["true"]
some_future_action_key = 42
"#,
        )
        .unwrap();
        assert_eq!(manifest.id, "x");
        assert_eq!(manifest.actions.len(), 1);
    }

    #[test]
    fn item_level_platforms_override_top_level_not_merge() {
        // Top-level restricts to windows only; the build step restricts itself to linux —
        // item-level should win outright, not intersect or union.
        let manifest = parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"
platforms = ["windows"]

[[build]]
command = ["true"]
platforms = ["linux"]
"#,
        )
        .unwrap();
        assert!(
            item_allowed(&manifest.platforms, &manifest.build[0].platforms)
                == (current_platform() == Platform::Linux)
        );
    }

    #[test]
    fn find_action_respects_platform_filtering() {
        let other = if current_platform() == Platform::Linux {
            "macos"
        } else {
            "linux"
        };
        let manifest = parse(&format!(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "only-here"
command = ["true"]

[[actions]]
id = "not-here"
command = ["true"]
platforms = ["{other}"]
"#
        ))
        .unwrap();

        assert!(manifest.find_action("only-here").is_some());
        assert!(manifest.find_action("not-here").is_none());
        assert!(manifest.find_action("missing").is_none());
    }

    #[test]
    fn matching_events_filters_by_name() {
        let manifest = parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[events]]
on = "pane.created"
command = ["true"]

[[events]]
on = "pane.closed"
command = ["true"]
"#,
        )
        .unwrap();

        let matches: Vec<_> = manifest.matching_events("pane.created").collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].on, "pane.created");
        assert_eq!(manifest.matching_events("pane.focused").count(), 0);
    }

    #[test]
    fn pane_placement_defaults_to_overlay() {
        let manifest = parse(
            r#"
id = "x"
version = "1.0.0"
min_herdr_version = "0.7.0"

[[panes]]
id = "p"
command = ["true"]
"#,
        )
        .unwrap();
        assert_eq!(manifest.panes[0].placement, "overlay");
    }

    #[test]
    fn load_reads_the_fixture_plugin_from_disk() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/plugins/echo-plugin/herdr-plugin.toml"
        ));
        let manifest = load(path).unwrap();
        assert_eq!(manifest.id, "echo-plugin");
        assert_eq!(manifest.actions.len(), 1);
        assert_eq!(manifest.actions[0].id, "dump-env");
        // The fixture hooks every lifecycle event taw emits (one recorder per event, for
        // tests/e2e_events.rs), so this asserts the *first* entry and that they all
        // parsed — not a count that every new event would have to bump.
        assert_eq!(manifest.events[0].on, "pane.agent_status_changed");
        assert!(
            manifest.events.len() > 1,
            "the fixture should hook more than one event"
        );
        assert_eq!(
            manifest.matching_events("layout.updated").count(),
            1,
            "the fixture should carry exactly one layout.updated recorder"
        );
        assert_eq!(manifest.panes.len(), 1);
        assert_eq!(manifest.panes[0].id, "board");
        assert_eq!(manifest.panes[0].placement, "bottom");
    }
}
