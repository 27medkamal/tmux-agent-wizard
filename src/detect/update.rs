// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Refreshing the cached remote manifests from herdr.dev.
//!
//! A fetched manifest replaces the cache only when it parses, names the right agent,
//! declares an engine version this build implements, and is strictly newer than what
//! is cached. Anything else leaves the cache untouched.

use std::{cmp::Ordering, fmt, path::Path, time::Duration};

use serde::{Deserialize, Serialize};

use crate::detect::{
    identity,
    loader::{self, ManifestStore},
};

/// Where herdr publishes its manifests.
pub const DEFAULT_MANIFEST_BASE_URL: &str = "https://herdr.dev/agent-detection";

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// herdr's cap; a manifest that large is a mistake or an attack.
const MAX_FETCH_BYTES: u64 = 256 * 1024;

/// What happened to one agent's manifest during an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateStatus {
    /// The cache now holds a newer manifest.
    Updated,
    /// The published manifest is the one already cached.
    Current,
    /// The published manifest was fetched but refused.
    Rejected,
    /// The manifest could not be fetched at all.
    Failed,
}

/// One agent's update result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateOutcome {
    pub id: String,
    pub status: UpdateStatus,
    /// The new version for `Updated`, or the reason for `Rejected`/`Failed`.
    pub message: Option<String>,
}

/// Fetch every bundled agent's manifest and refresh the cache.
///
/// Never fails as a whole: one outcome per agent, in catalog order.
pub fn update_all(store: &ManifestStore, base_url: &str, user_agent: &str) -> Vec<UpdateOutcome> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .build(),
    );

    identity::bundled_ids()
        .iter()
        .map(|id| {
            let url = join_url(base_url, &identity::remote_path(id));
            let (status, message) = match fetch(&agent, &url, user_agent) {
                Err(err) => (UpdateStatus::Failed, Some(err)),
                Ok(body) => match commit(store, id, &body) {
                    Ok(Some(version)) => (UpdateStatus::Updated, Some(version)),
                    Ok(None) => (UpdateStatus::Current, None),
                    Err(err) => (UpdateStatus::Rejected, Some(err)),
                },
            };
            UpdateOutcome {
                id: (*id).to_string(),
                status,
                message,
            }
        })
        .collect()
}

/// Run [`update_all`] only if `marker` was last touched longer than `max_age` ago.
///
/// The marker is touched before the fetch, so a network outage costs one attempt per
/// `max_age` rather than one per watcher tick.
pub fn update_if_stale(
    store: &ManifestStore,
    marker: &Path,
    max_age: Duration,
    base_url: &str,
    user_agent: &str,
) -> Option<Vec<UpdateOutcome>> {
    if !is_stale(marker, max_age) {
        return None;
    }
    touch(marker);
    Some(update_all(store, base_url, user_agent))
}

/// True when the marker is missing, unreadable or older than `max_age`.
pub(crate) fn is_stale(marker: &Path, max_age: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(marker).and_then(|meta| meta.modified()) else {
        return true;
    };
    modified.elapsed().map(|age| age >= max_age).unwrap_or(true)
}

/// Record that an update was attempted now. Best effort: an unwritable state
/// directory must not stop detection.
pub(crate) fn touch(marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, b"");
}

/// Accept a fetched manifest into the cache.
///
/// `Ok(Some(version))` when the cache was replaced, `Ok(None)` when the cache was
/// already current, `Err(reason)` when the manifest was refused.
pub(crate) fn commit(
    store: &ManifestStore,
    id: &str,
    body: &str,
) -> Result<Option<String>, String> {
    // Same contract the loader applies to a cached file: never write something the
    // loader would refuse to read back.
    let manifest = loader::parse_remote(id, body)?;
    let Some(version) = manifest.ordered_version() else {
        return Err("remote manifest must include version".to_string());
    };

    if let Some(cached) = store
        .cached_remote_version(id)
        .and_then(|cached| ManifestVersion::parse(&cached).ok())
    {
        match version.cmp(&cached) {
            Ordering::Less => {
                return Err(format!(
                    "remote version {version} is older than cached {cached}"
                ));
            }
            Ordering::Equal => return Ok(None),
            Ordering::Greater => {}
        }
    }

    store
        .write_remote(id, body)
        .map_err(|err| format!("could not cache manifest: {err}"))?;
    Ok(Some(version.to_string()))
}

fn fetch(agent: &ureq::Agent, url: &str, user_agent: &str) -> Result<String, String> {
    let mut response = agent
        .get(url)
        .header("User-Agent", user_agent)
        .call()
        .map_err(|err| err.to_string())?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_FETCH_BYTES)
        .read_to_string()
        .map_err(|err| err.to_string())
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path)
}

/// A dotted-numeric manifest version such as `2026.08.31.1`.
///
/// Compared field by field as numbers, so `2026.6.10.1` beats `2026.6.9.9` and a
/// missing trailing field reads as zero.
#[derive(Debug, Clone)]
pub(crate) struct ManifestVersion(String);

impl ManifestVersion {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("version must not be empty".to_string());
        }
        for segment in trimmed.split('.') {
            if segment.is_empty() {
                return Err(format!("version {trimmed:?} contains an empty segment"));
            }
            if !segment.chars().all(|ch| ch.is_ascii_digit()) {
                return Err(format!("version {trimmed:?} must be dotted numeric"));
            }
            segment
                .parse::<u64>()
                .map_err(|_| format!("version {trimmed:?} contains an oversized segment"))?;
        }
        Ok(Self(trimmed.to_string()))
    }
}

impl fmt::Display for ManifestVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ManifestVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl Ord for ManifestVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut left = self.0.split('.');
        let mut right = other.0.split('.');

        loop {
            match (left.next(), right.next()) {
                (Some(left), Some(right)) => {
                    let left = left.parse::<u64>().unwrap_or(0);
                    let right = right.parse::<u64>().unwrap_or(0);
                    match left.cmp(&right) {
                        Ordering::Equal => {}
                        ordering => return ordering,
                    }
                }
                (Some(left), None) => {
                    if left.parse::<u64>().unwrap_or(0) == 0 {
                        continue;
                    }
                    return Ordering::Greater;
                }
                (None, Some(right)) => {
                    if right.parse::<u64>().unwrap_or(0) == 0 {
                        continue;
                    }
                    return Ordering::Less;
                }
                (None, None) => return Ordering::Equal,
            }
        }
    }
}

impl PartialOrd for ManifestVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ManifestVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ManifestVersion {}

// Follows herdr's src/detect/manifest_update.rs tests.
// Differences: `process_agent_manifest` becomes `commit` over a `ManifestStore` in a
// `tempfile::TempDir`; herdr's `auto_update` catalog test needs a live HTTP server and
// is replaced by staleness-marker tests, and its three tests of the cache reload are not
// ported (see the header of this file).
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn remote_manifest(version: &str, contains: &str) -> String {
        format!(
            r#"
id = "codex"
version = "{version}"
min_engine_version = 1
updated_at = "2026-06-10T12:00:00Z"

[[rules]]
id = "idle"
state = "idle"
contains = ["{contains}"]
"#
        )
    }

    fn store() -> (TempDir, ManifestStore) {
        let dir = TempDir::new().unwrap();
        let store = ManifestStore::new(dir.path().join("config"), dir.path().join("remote"));
        (dir, store)
    }

    #[test]
    fn manifest_version_compares_dotted_numeric_segments() {
        assert!(
            ManifestVersion::parse("2026.6.10.1").unwrap()
                > ManifestVersion::parse("2026.6.9.9").unwrap()
        );
        assert!(ManifestVersion::parse("1.2.0").unwrap() == ManifestVersion::parse("1.2").unwrap());
        assert!(ManifestVersion::parse("1.2.1").unwrap() > ManifestVersion::parse("1.2").unwrap());
    }

    #[test]
    fn manifest_version_rejects_non_numeric_segments() {
        assert!(ManifestVersion::parse("").is_err());
        assert!(ManifestVersion::parse("2026.06.alpha").is_err());
        assert!(ManifestVersion::parse("2026..06").is_err());
        assert!(ManifestVersion::parse("2026.999999999999999999999999999999").is_err());
    }

    #[test]
    fn commit_writes_a_newer_manifest_to_the_cache() {
        let (_dir, store) = store();
        let content = remote_manifest("9999.01.01.1", "ready");

        assert_eq!(
            commit(&store, "codex", &content).unwrap().as_deref(),
            Some("9999.01.01.1")
        );
        assert_eq!(
            fs::read_to_string(store.remote_path("codex")).unwrap(),
            content
        );
    }

    #[test]
    fn commit_rejects_a_downgrade_and_keeps_the_cached_manifest() {
        let (_dir, store) = store();
        let current = remote_manifest("9999.01.01.1", "current");
        commit(&store, "codex", &current).unwrap();

        let older = remote_manifest("9999.01.01.0", "older");
        assert!(commit(&store, "codex", &older).is_err());
        assert_eq!(
            fs::read_to_string(store.remote_path("codex")).unwrap(),
            current
        );
    }

    #[test]
    fn commit_skips_the_same_version() {
        let (_dir, store) = store();
        let current = remote_manifest("9999.01.01.1", "current");
        commit(&store, "codex", &current).unwrap();

        assert!(commit(&store, "codex", &current).unwrap().is_none());
    }

    // herdr treats a same-version content change as an error. taw keeps the cache
    // untouched instead: the version is the only thing that promotes a manifest.
    #[test]
    fn commit_does_not_replace_the_cache_on_a_same_version_content_change() {
        let (_dir, store) = store();
        let current = remote_manifest("9999.01.01.1", "current");
        commit(&store, "codex", &current).unwrap();

        let changed = remote_manifest("9999.01.01.1", "changed");
        assert!(commit(&store, "codex", &changed).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(store.remote_path("codex")).unwrap(),
            current
        );
    }

    #[test]
    fn commit_refuses_manifests_it_cannot_use() {
        let (_dir, store) = store();

        assert!(commit(&store, "codex", "id = ").is_err(), "invalid TOML");
        assert!(
            commit(
                &store,
                "codex",
                &remote_manifest("9999.01.01.1", "ready")
                    .replace("min_engine_version = 1", "min_engine_version = 4")
            )
            .is_err(),
            "engine version above this build"
        );
        assert!(
            commit(&store, "claude", &remote_manifest("9999.01.01.1", "ready")).is_err(),
            "manifest for another agent"
        );
        assert!(
            commit(
                &store,
                "codex",
                r#"
id = "codex"
min_engine_version = 1

[[rules]]
id = "idle"
state = "idle"
contains = ["ready"]
"#
            )
            .is_err(),
            "no version"
        );
        assert!(!store.remote_path("codex").exists());
    }

    #[test]
    fn a_missing_or_old_marker_is_stale_and_a_fresh_one_is_not() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("nested").join("last-check");

        assert!(is_stale(&marker, Duration::from_secs(60)));
        touch(&marker);
        assert!(marker.exists());
        assert!(!is_stale(&marker, Duration::from_secs(60)));
        assert!(is_stale(&marker, Duration::ZERO));
    }

    #[test]
    fn update_if_stale_does_nothing_while_the_marker_is_fresh() {
        let (_dir, store) = store();
        let marker = store.remote_dir.join("last-check");
        touch(&marker);

        assert!(
            update_if_stale(
                &store,
                &marker,
                Duration::from_secs(6 * 60 * 60),
                "http://127.0.0.1:1/agent-detection",
                "taw/test",
            )
            .is_none()
        );
    }

    #[test]
    fn urls_join_without_doubling_the_separator() {
        assert_eq!(
            join_url("https://herdr.dev/agent-detection/", "claude.toml"),
            "https://herdr.dev/agent-detection/claude.toml"
        );
        assert_eq!(
            join_url("https://herdr.dev/agent-detection", "github-copilot.toml"),
            "https://herdr.dev/agent-detection/github-copilot.toml"
        );
    }
}
