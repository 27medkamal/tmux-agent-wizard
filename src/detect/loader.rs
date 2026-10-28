// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Manifest precedence: local override → cached remote → bundled.
//!
//! A shadowed manifest is never silently dropped: whichever manifest ends up active
//! carries a `warning` explaining what was ignored and why.

use std::{fs, io::Write, path::PathBuf};

use anyhow::{Context, anyhow};

use crate::detect::{
    engine::{self, LoadedManifest, ManifestSource, parse_manifest},
    identity, supplements,
};

/// The directories a user can put manifests in.
#[derive(Debug, Clone)]
pub struct ManifestStore {
    /// `<config>/agent-detection` — hand-written overrides, highest precedence.
    pub override_dir: PathBuf,
    /// `<state>/agent-detection/remote` — the update cache.
    pub remote_dir: PathBuf,
    /// Whether `load` appends taw's own [`supplements`] rules after precedence.
    /// `cfg.detection.supplements` — on by default.
    supplements: bool,
}

/// What [`ManifestStore::list`] reports for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestSummary {
    pub id: String,
    pub active_version: Option<String>,
    pub active_source: ManifestSource,
    pub remote_version: Option<String>,
    pub bundled_version: Option<String>,
    /// True when an override file exists while a usable remote manifest is cached,
    /// i.e. the update cache is being ignored on purpose.
    pub shadowed_by_override: bool,
    /// How many taw supplement rules are appended to the active manifest — 0 when
    /// `[detection] supplements = false`, or when taw ships none for this agent.
    pub supplement_rules: usize,
}

impl ManifestStore {
    pub fn new(override_dir: PathBuf, remote_dir: PathBuf) -> Self {
        Self {
            override_dir,
            remote_dir,
            supplements: true,
        }
    }

    /// Turn taw's own [`supplements`] rules on or off. On by default —
    /// `cfg.detection.supplements` threads a config value in here.
    pub fn with_supplements(mut self, on: bool) -> Self {
        self.supplements = on;
        self
    }

    /// The manifest to evaluate for an agent, with precedence applied.
    ///
    /// Fails only when the id has no bundled manifest; a broken override or remote
    /// file degrades to the next source with a warning instead. A taw supplement, by
    /// contrast, ships with this binary rather than being installed by a user, so a
    /// broken one is treated like a broken bundled manifest — a build bug, not a
    /// degrade-and-warn case — and fails the load.
    pub fn load(&self, id: &str) -> anyhow::Result<LoadedManifest> {
        Ok(self.load_with_supplement_count(id)?.0)
    }

    /// One summary per bundled agent, in catalog order.
    pub fn list(&self) -> Vec<ManifestSummary> {
        identity::bundled_ids()
            .iter()
            .map(|id| self.summary(id))
            .collect()
    }

    pub fn override_path(&self, id: &str) -> PathBuf {
        self.override_dir.join(format!("{id}.toml"))
    }

    pub fn remote_path(&self, id: &str) -> PathBuf {
        self.remote_dir.join(format!("{id}.toml"))
    }

    /// The version of the cached remote manifest, when it is usable.
    pub(crate) fn cached_remote_version(&self, id: &str) -> Option<String> {
        let text = fs::read_to_string(self.remote_path(id)).ok()?;
        parse_remote(id, &text).ok()?.version
    }

    /// Replace the cached remote manifest, atomically.
    pub(crate) fn write_remote(&self, id: &str, text: &str) -> anyhow::Result<()> {
        let path = self.remote_path(id);
        fs::create_dir_all(&self.remote_dir)
            .with_context(|| format!("creating {}", self.remote_dir.display()))?;
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .inspect_err(|_| {
                    let _ = fs::remove_file(&tmp);
                })
                .with_context(|| format!("writing {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    fn summary(&self, id: &str) -> ManifestSummary {
        let bundled_version =
            identity::bundled_manifest_text(id).and_then(engine::manifest_version);
        let remote_version = self.cached_remote_version(id);
        let loaded = self.load_with_supplement_count(id).ok();
        let active = loaded.as_ref().map(|(manifest, _)| manifest);

        ManifestSummary {
            id: id.to_string(),
            active_version: active.and_then(|manifest| manifest.version.clone()),
            active_source: active
                .map(|manifest| manifest.source)
                .unwrap_or(ManifestSource::Bundled),
            shadowed_by_override: self.override_path(id).exists() && remote_version.is_some(),
            remote_version,
            bundled_version,
            supplement_rules: loaded.map(|(_, count)| count).unwrap_or(0),
        }
    }

    /// [`Self::load`], plus how many taw supplement rules ended up appended — the
    /// count [`ManifestSummary::supplement_rules`] reports. A private split rather
    /// than a public return-value change to `load`, since every existing caller wants
    /// just the manifest.
    fn load_with_supplement_count(&self, id: &str) -> anyhow::Result<(LoadedManifest, usize)> {
        let text = identity::bundled_manifest_text(id)
            .ok_or_else(|| anyhow!("no bundled manifest for agent {id}"))?;
        let bundled = parse_manifest(text, ManifestSource::Bundled)
            .with_context(|| format!("bundled {id} manifest is invalid"))?;

        let mut active = self.apply_override(id, self.remote_or(id, bundled));
        let supplement_rules = if self.supplements {
            match supplements::text(id) {
                Some(supplement_text) => active
                    .append_supplement(supplement_text)
                    .with_context(|| format!("taw supplement for {id} is invalid"))?,
                None => 0,
            }
        } else {
            0
        };
        Ok((active, supplement_rules))
    }

    fn remote_or(&self, id: &str, bundled: LoadedManifest) -> LoadedManifest {
        let path = self.remote_path(id);
        if !path.exists() {
            return bundled;
        }
        let remote = match fs::read_to_string(&path) {
            Ok(text) => parse_remote(id, &text),
            Err(err) => Err(err.to_string()),
        };
        let remote = match remote {
            Ok(remote) => remote,
            Err(err) => {
                return with_warning(
                    bundled,
                    format!(
                        "ignored remote manifest {} because it could not be loaded: {err}",
                        path.display()
                    ),
                );
            }
        };

        if let (Some(cached), Some(shipped)) = (remote.ordered_version(), bundled.ordered_version())
            && cached < shipped
        {
            return with_warning(
                bundled,
                format!(
                    "ignored remote manifest {} because cached version {cached} is older than bundled {shipped}",
                    path.display()
                ),
            );
        }
        remote
    }

    fn apply_override(&self, id: &str, active: LoadedManifest) -> LoadedManifest {
        let path = self.override_path(id);
        if !path.exists() {
            return active;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                return with_warning(
                    active,
                    format!(
                        "ignored override {} because it could not be loaded: {err}",
                        path.display()
                    ),
                );
            }
        };
        match parse_manifest(&text, ManifestSource::Override) {
            Ok(manifest) if manifest.matches_id(id) => manifest,
            Ok(manifest) => with_warning(
                active,
                format!(
                    "ignored override {} because manifest id {} does not match {id}",
                    path.display(),
                    manifest.id
                ),
            ),
            Err(err) => with_warning(
                active,
                format!(
                    "ignored override {} because it could not be compiled: {err}",
                    path.display()
                ),
            ),
        }
    }
}

/// Parse a manifest that claims to be the remote copy for `id`.
///
/// Remote manifests must name the agent and declare both `version` and
/// `min_engine_version` — herdr's `parse_remote_manifest_for_agent` contract.
/// `parse_manifest` has already refused anything that needs a newer engine.
pub(crate) fn parse_remote(id: &str, text: &str) -> Result<LoadedManifest, String> {
    let manifest = parse_manifest(text, ManifestSource::Remote).map_err(|err| err.to_string())?;
    if !manifest.matches_id(id) {
        return Err(format!("manifest id {} does not match {id}", manifest.id));
    }
    if manifest.version.is_none() {
        return Err("remote manifest must include version".to_string());
    }
    if manifest.min_engine_version().is_none() {
        return Err("remote manifest must include min_engine_version".to_string());
    }
    Ok(manifest)
}

fn with_warning(mut manifest: LoadedManifest, warning: String) -> LoadedManifest {
    manifest.warning = Some(warning);
    manifest
}

// Follows herdr's src/detect/manifest/tests.rs. Differences:
// the XDG env juggling and `reload_manifests()` calls become a `ManifestStore` over a
// `tempfile::TempDir`, so the tests share no process state and can run in
// parallel.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{AgentState, Snapshot, evaluate};
    use tempfile::TempDir;

    fn remote_manifest(version: &str, state: &str, contains: &str) -> String {
        format!(
            r#"
id = "codex"
version = "{version}"
min_engine_version = 1
updated_at = "2026-06-10T12:00:00Z"

[[rules]]
id = "test"
state = "{state}"
contains = ["{contains}"]
"#
        )
    }

    fn local_manifest(state: &str, contains: &str) -> String {
        format!(
            r#"
id = "codex"

[[rules]]
id = "test"
state = "{state}"
contains = ["{contains}"]
"#
        )
    }

    struct Fixture {
        _dir: TempDir,
        store: ManifestStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let store = ManifestStore::new(dir.path().join("config"), dir.path().join("remote"));
            Self { _dir: dir, store }
        }

        fn write_remote_codex(&self, content: &str) {
            fs::create_dir_all(&self.store.remote_dir).unwrap();
            fs::write(self.store.remote_path("codex"), content).unwrap();
        }

        fn write_local_codex(&self, content: &str) {
            fs::create_dir_all(&self.store.override_dir).unwrap();
            fs::write(self.store.override_path("codex"), content).unwrap();
        }

        fn explain_codex(&self, screen: &str) -> crate::detect::Evaluation {
            let manifest = self.store.load("codex").unwrap();
            evaluate(
                &manifest,
                &Snapshot {
                    content: screen,
                    osc_title: "",
                    osc_progress: "",
                },
            )
        }
    }

    #[test]
    fn bundled_is_used_when_nothing_else_is_installed() {
        let fixture = Fixture::new();
        let manifest = fixture.store.load("codex").unwrap();

        assert_eq!(manifest.source, ManifestSource::Bundled);
        assert!(manifest.warning.is_none());
    }

    #[test]
    fn unknown_agent_ids_have_no_manifest() {
        let fixture = Fixture::new();
        assert!(fixture.store.load("docker").is_err());
    }

    #[test]
    fn remote_manifest_loads_between_local_override_and_bundled() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));

        let explain = fixture.explain_codex("remote-ready");

        assert_eq!(explain.state, AgentState::Blocked);
        assert_eq!(explain.source, ManifestSource::Remote);
        assert_eq!(explain.manifest_version.as_deref(), Some("9999.01.01.1"));
    }

    #[test]
    fn fallback_preserves_active_manifest_version() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));

        let explain = fixture.explain_codex("ordinary prompt text");

        assert_eq!(explain.state, AgentState::Idle);
        assert!(explain.matched_rule.is_none());
        assert_eq!(explain.manifest_version.as_deref(), Some("9999.01.01.1"));
        assert_eq!(explain.source, ManifestSource::Remote);
    }

    #[test]
    fn older_cached_remote_manifest_does_not_shadow_newer_bundled_manifest() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("2026.06.10.0", "blocked", "remote-ready"));

        let explain = fixture.explain_codex("remote-ready");

        assert_eq!(explain.state, AgentState::Idle);
        assert_eq!(explain.source, ManifestSource::Bundled);
        assert!(
            explain
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("older than bundled"))
        );
        assert_eq!(
            fixture.store.cached_remote_version("codex").as_deref(),
            Some("2026.06.10.0")
        );
    }

    #[test]
    fn local_override_shadows_cached_remote_manifest() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));
        fixture.write_local_codex(&local_manifest("idle", "local-ready"));

        let explain = fixture.explain_codex("local-ready");

        assert_eq!(explain.state, AgentState::Idle);
        assert_eq!(explain.source, ManifestSource::Override);
        assert_eq!(
            explain.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("test")
        );

        let summary = fixture
            .store
            .list()
            .into_iter()
            .find(|summary| summary.id == "codex")
            .unwrap();
        assert!(summary.shadowed_by_override);
        assert_eq!(summary.remote_version.as_deref(), Some("9999.01.01.1"));
        assert_eq!(summary.active_source, ManifestSource::Override);
        assert!(summary.bundled_version.is_some());
    }

    #[test]
    fn invalid_local_override_falls_back_to_cached_remote_manifest() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));
        fixture.write_local_codex("id = ");

        let explain = fixture.explain_codex("remote-ready");

        assert_eq!(explain.state, AgentState::Blocked);
        assert_eq!(explain.source, ManifestSource::Remote);
        assert!(explain.warning.is_some());
    }

    #[test]
    fn override_for_another_agent_is_ignored_with_a_warning() {
        let fixture = Fixture::new();
        fixture.write_local_codex(
            r#"
id = "claude"

[[rules]]
id = "test"
state = "blocked"
contains = ["local-ready"]
"#,
        );

        let explain = fixture.explain_codex("local-ready");

        assert_eq!(explain.source, ManifestSource::Bundled);
        assert!(
            explain
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("does not match codex"))
        );
    }

    #[test]
    fn a_remote_manifest_without_a_version_is_ignored() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&local_manifest("blocked", "remote-ready"));

        let explain = fixture.explain_codex("remote-ready");

        assert_eq!(explain.source, ManifestSource::Bundled);
        assert!(
            explain
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("must include version"))
        );
    }

    // herdr's `detection_uses_cached_manifest_until_explicit_reload` pins its
    // process-wide manifest cache. taw has none, so the property to pin is the
    // opposite one: a store observes what is on disk right now.
    #[test]
    fn each_load_observes_the_current_files() {
        let fixture = Fixture::new();
        fixture.write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "cached-ready"));
        assert_eq!(
            fixture.explain_codex("cached-ready").state,
            AgentState::Blocked
        );

        fixture.write_remote_codex(&remote_manifest("9999.01.01.2", "working", "new-ready"));

        let reloaded = fixture.explain_codex("new-ready");
        assert_eq!(reloaded.state, AgentState::Working);
        assert_eq!(reloaded.manifest_version.as_deref(), Some("9999.01.01.2"));
    }

    #[test]
    fn write_remote_replaces_the_cache_atomically() {
        let fixture = Fixture::new();
        let content = remote_manifest("9999.01.01.1", "idle", "ready");
        fixture.store.write_remote("codex", &content).unwrap();

        assert_eq!(
            fs::read_to_string(fixture.store.remote_path("codex")).unwrap(),
            content
        );
        assert!(
            !fixture
                .store
                .remote_path("codex")
                .with_extension("toml.tmp")
                .exists()
        );
    }

    #[test]
    fn list_summarises_every_bundled_agent() {
        let fixture = Fixture::new();
        let summaries = fixture.store.list();

        assert_eq!(summaries.len(), identity::bundled_ids().len());
        for summary in &summaries {
            assert_eq!(summary.active_source, ManifestSource::Bundled);
            assert_eq!(summary.active_version, summary.bundled_version);
            assert!(summary.remote_version.is_none());
            assert!(!summary.shadowed_by_override);
        }
    }

    // -----------------------------------------------------------------------
    // Supplements.
    // -----------------------------------------------------------------------

    #[test]
    fn supplements_are_applied_by_default_and_counted_in_the_summary() {
        let fixture = Fixture::new();

        let manifest = fixture.store.load("claude").unwrap();
        let taw_rule_count = crate::detect::evaluate(
            &manifest,
            &Snapshot {
                content: "",
                osc_title: "",
                osc_progress: "",
            },
        )
        .evaluated_rules
        .iter()
        .filter(|rule| rule.source == crate::detect::RuleSource::Taw)
        .count();
        assert_eq!(taw_rule_count, 6);

        let summary = fixture
            .store
            .list()
            .into_iter()
            .find(|summary| summary.id == "claude")
            .unwrap();
        assert_eq!(summary.supplement_rules, 6);
    }

    #[test]
    fn with_supplements_false_appends_nothing() {
        let fixture = Fixture::new();
        let store = fixture.store.clone().with_supplements(false);

        let manifest = store.load("claude").unwrap();
        let taw_rule_count = crate::detect::evaluate(
            &manifest,
            &Snapshot {
                content: "",
                osc_title: "",
                osc_progress: "",
            },
        )
        .evaluated_rules
        .iter()
        .filter(|rule| rule.source == crate::detect::RuleSource::Taw)
        .count();
        assert_eq!(taw_rule_count, 0);

        let summary = store
            .list()
            .into_iter()
            .find(|summary| summary.id == "claude")
            .unwrap();
        assert_eq!(summary.supplement_rules, 0);
    }

    #[test]
    fn an_agent_with_no_supplement_reports_zero_regardless_of_the_switch() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture
                .store
                .list()
                .into_iter()
                .find(|summary| summary.id == "gemini")
                .unwrap()
                .supplement_rules,
            0
        );
    }

    #[test]
    fn codex_carries_one_supplement_rule_and_the_switch_takes_it_away() {
        let fixture = Fixture::new();
        let count = |store: &ManifestStore| {
            store
                .list()
                .into_iter()
                .find(|summary| summary.id == "codex")
                .unwrap()
                .supplement_rules
        };
        assert_eq!(count(&fixture.store), 1);
        assert_eq!(count(&fixture.store.clone().with_supplements(false)), 0);
    }
}
