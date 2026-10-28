//! Hot reload for a long-lived watcher: noticing that `config.toml`, the local agent
//! manifest overrides or the cached remote manifests changed, without polling anything
//! expensive.
//!
//! A watcher is started by `pipe-pane` and lives as long as its pane — days, across
//! however many times the user edits their config. Restarting every watcher to pick up a
//! new `[ui.toast] delay_seconds` would mean detaching and re-attaching every pipe, so it
//! re-reads instead.
//!
//! Two triggers, checked together once per read:
//!
//! * **mtime.** `config.toml`'s own modification time, and a fingerprint of the manifest
//!   override directory and of the remote manifest cache: the directory's own mtime and
//!   each entry's name, mtime and length, so an added, removed or edited file counts
//!   whatever its new mtime is (a `cp -p` or a restore from a backup puts back an older
//!   one, and on Linux a new file often has its directory's very timestamp). The cache is written by
//!   whichever watcher's update check or `taw manifest update` fetched a newer copy, and
//!   every other watcher reloads it from this, as herdr reloads a cached copy that
//!   changed on disk (`agents_needing_cache_reload`). A few `stat` calls, no tmux, no
//!   parsing — cheap enough for every read.
//! * **`@agent_wizard_reload`.** A global tmux option `taw config reload` and
//!   `taw manifest reload` bump. That covers the cases mtime cannot see: a config edited
//!   and saved back to the same timestamp, an override file whose content a tool rewrote
//!   in place, or simply wanting every live watcher to re-read *now*. It arrives with the
//!   data the evaluation already reads, so watching for it costs no extra tmux call.
//!
//! `$TAW_LOG` is deliberately **not** part of this: the logger is installed once at
//! process start (a file logger for `watch run`, see [`super::log`]), and `log` has no
//! way to swap a sink afterwards. Changing the level needs a restart, and saying so is
//! better than pretending otherwise.

use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

/// The global tmux option `taw config reload` / `taw manifest reload` bump, and every
/// live watcher watches. Its value is opaque — only *changing* means anything.
pub const RELOAD_OPTION: &str = "@agent_wizard_reload";

/// What changed since the watcher last looked.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changed {
    /// `config.toml` should be re-read.
    pub config: bool,
    /// The compiled manifest cache should be dropped so the overrides and the cached
    /// remote manifests are read again.
    pub manifests: bool,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.config || self.manifests
    }
}

/// What was on disk (and in `@agent_wizard_reload`) the last time the watcher looked.
#[derive(Debug)]
pub struct Tracker {
    config_mtime: Option<SystemTime>,
    overrides: Fingerprint,
    remote: Fingerprint,
    token: String,
}

impl Tracker {
    /// A tracker primed with the current state, so the first [`Tracker::poll`] reports
    /// nothing: the watcher has just loaded both, and re-reading them immediately would
    /// log a reload that changed nothing.
    pub fn new(config_file: &Path, override_dir: &Path, remote_dir: &Path, token: &str) -> Tracker {
        Tracker {
            config_mtime: mtime(config_file),
            overrides: fingerprint(override_dir),
            remote: fingerprint(remote_dir),
            token: token.to_string(),
        }
    }

    /// What changed, taking `token` as the current value of [`RELOAD_OPTION`].
    ///
    /// A changed token reloads **both**: one option covers both verbs, and re-reading a
    /// config that did not change is a parse of a small file — far cheaper than a second
    /// tmux format field to tell the two apart.
    pub fn poll(
        &mut self,
        config_file: &Path,
        override_dir: &Path,
        remote_dir: &Path,
        token: &str,
    ) -> Changed {
        let mut changed = Changed::default();

        let config_mtime = mtime(config_file);
        if config_mtime != self.config_mtime {
            self.config_mtime = config_mtime;
            changed.config = true;
        }
        let overrides = fingerprint(override_dir);
        if overrides != self.overrides {
            self.overrides = overrides;
            changed.manifests = true;
        }
        // herdr's `agents_needing_cache_reload`: a cached remote copy that changed on disk
        // is reloaded even when this watcher's own update check fetched nothing.
        let remote = fingerprint(remote_dir);
        if remote != self.remote {
            self.remote = remote;
            changed.manifests = true;
        }
        if token != self.token {
            self.token = token.to_string();
            changed.config = true;
            changed.manifests = true;
        }
        changed
    }
}

/// One path's modification time, `None` when it does not exist or cannot be read.
///
/// `None` is a *value*, not a failure: a config file that has not been created yet, and
/// one that was just deleted, are different states, and moving between them is a change
/// the watcher should act on (it means "fall back to the defaults").
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// What a directory of manifest files looks like on disk: its own mtime and, sorted by
/// name, each immediate entry's name, mtime and length (`None` and 0 for one that cannot
/// be read).
#[derive(Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    own: Option<SystemTime>,
    entries: Vec<(OsString, Option<SystemTime>, u64)>,
}

/// The [`Fingerprint`] of `dir`, empty when it does not exist.
///
/// The directory's own mtime moves when a file is added or removed; an entry's mtime or
/// length moves when it is edited. Compared whole, these catch an edit that leaves the
/// newest mtime where it was: a file put back with an older mtime, or one written in the
/// same clock tick as its directory. An entry that is a link is read through it. Only one
/// level deep: the override directory and the remote cache hold `<agent>.toml` files, not
/// a tree.
fn fingerprint(dir: &Path) -> Fingerprint {
    let own = mtime(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Fingerprint {
            own,
            entries: Vec::new(),
        };
    };
    let mut entries: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| {
            let meta = std::fs::metadata(entry.path()).or_else(|_| entry.metadata());
            let (modified, len) = meta
                .map(|meta| (meta.modified().ok(), meta.len()))
                .unwrap_or((None, 0));
            (entry.file_name(), modified, len)
        })
        .collect();
    entries.sort();
    Fingerprint { own, entries }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Filesystem mtimes have coarse resolution (one second on some filesystems), so a
    /// test that writes twice in a row can see no change at all. Setting the timestamp
    /// explicitly makes "the file changed" unambiguous — and it moves *only* the file's
    /// mtime, never the directory's, which is what the "edited in place" case needs.
    fn set_mtime(path: &Path, secs_from_epoch: u64) {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs_from_epoch);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(time))
            .unwrap();
    }

    #[test]
    fn a_fresh_tracker_reports_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();

        let mut tracker = Tracker::new(&config, &overrides, &remote, "1");
        assert_eq!(
            tracker.poll(&config, &overrides, &remote, "1"),
            Changed::default()
        );
        assert!(!tracker.poll(&config, &overrides, &remote, "1").any());
    }

    #[test]
    fn a_changed_config_mtime_reloads_the_config_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();
        set_mtime(&config, 0);

        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        set_mtime(&config, 5);
        let changed = tracker.poll(&config, &overrides, &remote, "");
        assert!(changed.config);
        assert!(!changed.manifests);
        // Reported once: the tracker has taken the new mtime as its baseline.
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());
    }

    #[test]
    fn creating_or_deleting_the_config_is_itself_a_change() {
        // No config file means "use the defaults"; creating one, and later removing it,
        // both change which settings are in force.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();

        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());

        std::fs::write(&config, "[ui]\nconfirm_close = false\n").unwrap();
        assert!(tracker.poll(&config, &overrides, &remote, "").config);

        std::fs::remove_file(&config).unwrap();
        assert!(tracker.poll(&config, &overrides, &remote, "").config);
    }

    #[test]
    fn an_added_edited_or_removed_override_reloads_the_manifests_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();

        let mut tracker = Tracker::new(&config, &overrides, &remote, "");

        // Added: the directory's own mtime moves.
        let claude = overrides.join("claude.toml");
        std::fs::write(&claude, "id = \"claude\"\n").unwrap();
        let changed = tracker.poll(&config, &overrides, &remote, "");
        assert!(changed.manifests);
        assert!(!changed.config);

        // Edited in place: the directory's mtime does not move, the file's does.
        set_mtime(&claude, 30);
        assert!(tracker.poll(&config, &overrides, &remote, "").manifests);

        // Removed.
        std::fs::remove_file(&claude).unwrap();
        assert!(tracker.poll(&config, &overrides, &remote, "").manifests);
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());
    }

    /// A file put back with an older mtime (a `cp -p`, a restore from a backup), its length
    /// unchanged, while the directory's own mtime stays the newest, as it is for a file
    /// whose times were kept: the newest mtime of the directory does not move, the file's
    /// does. The same for the remote cache.
    #[test]
    fn an_override_put_back_with_an_older_mtime_reloads_the_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let claude = overrides.join("claude.toml");
        std::fs::write(&claude, "id = \"claude\"\n").unwrap();
        set_mtime(&claude, 1_000_000_000);
        let cached = remote.join("codex.toml");
        std::fs::write(&cached, "id = \"codex\"\n").unwrap();
        set_mtime(&cached, 1_000_000_000);

        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());

        std::fs::write(&claude, "id = \"CLAUDE\"\n").unwrap();
        set_mtime(&claude, 900_000_000);
        let changed = tracker.poll(&config, &overrides, &remote, "");
        assert!(
            changed.manifests,
            "an override with an older mtime drops the cache"
        );
        assert!(!changed.config);
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());

        std::fs::write(&cached, "id = \"CODEX\"\n").unwrap();
        set_mtime(&cached, 900_000_000);
        assert!(
            tracker.poll(&config, &overrides, &remote, "").manifests,
            "a remote copy with an older mtime drops the cache"
        );
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());
    }

    #[test]
    fn a_rewritten_remote_manifest_reloads_the_cache() {
        // herdr's `agents_needing_cache_reload`: a watcher reloads an agent whose cached
        // remote copy changed on disk even when its own update check fetched nothing —
        // another watcher, or `taw manifest update`, did.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        let claude = remote.join("claude.toml");
        std::fs::write(&claude, "id = \"claude\"\n").unwrap();
        set_mtime(&claude, 4_000_000_000);

        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        assert!(!tracker.poll(&config, &overrides, &remote, "").any());

        set_mtime(&claude, 4_000_000_060);
        let changed = tracker.poll(&config, &overrides, &remote, "");
        assert!(changed.manifests, "a rewritten remote copy drops the cache");
        assert!(!changed.config);
        assert!(
            !tracker.poll(&config, &overrides, &remote, "").any(),
            "reported once"
        );
    }

    #[test]
    fn a_bumped_reload_option_reloads_both_even_when_nothing_on_disk_moved() {
        // The point of the option: an editor that writes a file back with the same
        // timestamp, or a user who simply wants every live watcher to re-read now.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let overrides = dir.path().join("agent-detection");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&overrides).unwrap();

        let mut tracker = Tracker::new(&config, &overrides, &remote, "7");
        assert!(!tracker.poll(&config, &overrides, &remote, "7").any());

        let changed = tracker.poll(&config, &overrides, &remote, "8");
        assert!(changed.config);
        assert!(changed.manifests);
        assert!(!tracker.poll(&config, &overrides, &remote, "8").any());

        // An option that was unset and is now set counts, and so does the reverse: a
        // watcher started before the plugin script ran sees the option appear.
        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        assert!(tracker.poll(&config, &overrides, &remote, "1").any());
        assert!(tracker.poll(&config, &overrides, &remote, "").any());
    }

    #[test]
    fn a_missing_override_or_remote_directory_is_not_a_change_every_tick() {
        // The override directory is created on demand, so most servers never have one, and
        // the remote cache has none before the first fetch.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let overrides = dir.path().join("never-created");
        let remote = dir.path().join("remote");
        let mut tracker = Tracker::new(&config, &overrides, &remote, "");
        for _ in 0..3 {
            assert!(!tracker.poll(&config, &overrides, &remote, "").any());
        }
    }
}
