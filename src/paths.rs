//! XDG-ish filesystem locations for tmux-agent-wizard.
//!
//! Config lives under `$TAW_CONFIG_DIR` (default `$XDG_CONFIG_HOME/tmux-agent-wizard`,
//! falling back to `$HOME/.config/tmux-agent-wizard` when `XDG_CONFIG_HOME` is unset or
//! empty); state lives under `$TAW_STATE_DIR` (default
//! `$XDG_STATE_HOME/tmux-agent-wizard`, falling back to
//! `$HOME/.local/state/tmux-agent-wizard`). This is plain XDG Base Directory semantics
//! on *every* Unix, macOS included: unlike the `dirs` crate — which maps macOS onto
//! `~/Library/Application Support` and has no state-dir equivalent there at all — we
//! want the same `~/.config`/`~/.local/state` layout everywhere, matching herdr (which
//! uses `~/.config/herdr/config.toml` on macOS too).

use std::path::PathBuf;

/// Resolved filesystem locations tmux-agent-wizard reads and writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub override_dir: PathBuf,
    pub remote_manifest_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub plugin_state_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub manifest_marker: PathBuf,
}

impl Paths {
    /// Resolve paths from `$TAW_CONFIG_DIR` / `$TAW_STATE_DIR`, falling back to the
    /// platform's XDG-ish defaults (via the `dirs` crate) when unset.
    pub fn from_env() -> Paths {
        let config_dir = std::env::var_os("TAW_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_config_dir);
        let state_dir = std::env::var_os("TAW_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_state_dir);
        Paths::rooted_at(config_dir, state_dir)
    }

    /// The locations under the two folders given, as [`Paths::from_env`] derives them from its own: for a
    /// test that needs the paths of a `taw` it runs elsewhere.
    pub fn rooted_at(config_dir: PathBuf, state_dir: PathBuf) -> Paths {
        let agent_detection_state = state_dir.join("agent-detection");
        let plugins_dir = state_dir.join("plugins");
        Paths {
            config_file: config_dir.join("config.toml"),
            override_dir: config_dir.join("agent-detection"),
            remote_manifest_dir: agent_detection_state.join("remote"),
            manifest_marker: agent_detection_state.join("last-check"),
            sessions_dir: state_dir.join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: state_dir.join("logs"),
            config_dir,
            state_dir,
        }
    }

    /// Where ssh keeps its control sockets, one a machine (`<state>/m`). Made by the ssh runner
    /// (`machine::ssh`), never by [`Paths::ensure`]: nothing exists here until a machine is used.
    pub fn control_dir(&self) -> PathBuf {
        self.state_dir.join("m")
    }

    /// Where the files about machines live (`<state>/machines`). Nothing makes it until an app keeps the connections.
    pub fn machines_dir(&self) -> PathBuf {
        self.state_dir.join("machines")
    }

    /// The file the keeper app writes about one machine (`<state>/machines/<name>.json`), or `None`
    /// for a name that cannot be a machine's ([`crate::machine::config::valid_name`]): such a name
    /// could reach outside the folder.
    pub fn machine_file(&self, name: &str) -> Option<PathBuf> {
        crate::machine::config::valid_name(name)
            .then(|| self.machines_dir().join(format!("{name}.json")))
    }

    /// The lock the app that keeps the connections holds (`<state>/machines/keeper.lock`).
    pub fn keeper_lock_file(&self) -> PathBuf {
        self.machines_dir().join("keeper.lock")
    }

    /// Who holds this machine's attention, and since when (`<state>/machines/lease.json`).
    pub fn lease_file(&self) -> PathBuf {
        self.machines_dir().join("lease.json")
    }

    /// The folder of the walks the apps sent, one file for each lease holder (`<state>/machines/walks`).
    pub fn walks_dir(&self) -> PathBuf {
        self.machines_dir().join("walks")
    }

    /// What this machine's watcher and attaches tell the feed, one JSON object a line
    /// (`<state>/machines/events.jsonl`).
    pub fn events_file(&self) -> PathBuf {
        self.machines_dir().join("events.jsonl")
    }

    /// Where the clients that a `taw machine attach` made are recorded, one file for each terminal
    /// (`<state>/machines/attached`). Nothing makes it until one attaches.
    pub fn attached_dir(&self) -> PathBuf {
        self.machines_dir().join("attached")
    }

    /// The record of the client on the terminal `tty` (`/dev/ttys004` is
    /// `<state>/machines/attached/_dev_ttys004.json`), or `None` for no terminal: the slashes of the path
    /// become underscores, so no terminal name reaches outside the folder.
    pub fn attached_record(&self, tty: &str) -> Option<PathBuf> {
        (!tty.is_empty()).then(|| {
            self.attached_dir()
                .join(format!("{}.json", tty.replace('/', "_")))
        })
    }

    /// Where each running app keeps its record (`<state>/apps`). Nothing makes it until an app starts.
    pub fn apps_dir(&self) -> PathBuf {
        self.state_dir.join("apps")
    }

    /// The record of the app child whose process id is `pid` (`<state>/apps/<pid>.json`).
    pub fn app_record(&self, pid: u32) -> PathBuf {
        self.apps_dir().join(format!("{pid}.json"))
    }

    /// Create every directory this struct points at. `config_file` and
    /// `manifest_marker` are files, not directories, and are left for their
    /// respective writers to create.
    pub fn ensure(&self) -> std::io::Result<()> {
        for dir in [
            &self.config_dir,
            &self.state_dir,
            &self.override_dir,
            &self.remote_manifest_dir,
            &self.sessions_dir,
            &self.plugins_dir,
            &self.plugin_state_dir,
            &self.logs_dir,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

fn default_config_dir() -> PathBuf {
    match xdg_env_dir("XDG_CONFIG_HOME") {
        Some(dir) => dir.join("tmux-agent-wizard"),
        None => home_dir().join(".config").join("tmux-agent-wizard"),
    }
}

fn default_state_dir() -> PathBuf {
    match xdg_env_dir("XDG_STATE_HOME") {
        Some(dir) => dir.join("tmux-agent-wizard"),
        None => home_dir()
            .join(".local")
            .join("state")
            .join("tmux-agent-wizard"),
    }
}

/// Reads an XDG base-directory env var, treating an empty **or relative** value the same
/// as unset.
///
/// Both come straight from the XDG Base Directory Specification: "All paths set in these
/// environment variables must be absolute. If an implementation encounters a relative
/// path in any of these variables it should consider the path invalid and ignore it."
///
/// Ignoring rather than resolving matters here: a relative value would be joined against
/// whatever directory `taw` happened to be started in, so config and state would land in
/// a different place per invocation — and, for a `taw` run from a project, inside that
/// project.
fn xdg_env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Resolves `$HOME`, falling back to the platform home-directory lookup (`dirs::home_dir`,
/// which on Unix itself falls back to a `getpwuid` lookup) only when `$HOME` is unset.
/// `PathBuf::from(".")` is a last-resort so callers never have to handle a missing home
/// directory.
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_machine_file_is_only_for_a_name_a_machine_can_have() {
        let paths = Paths::rooted_at(PathBuf::from("/c"), PathBuf::from("/s"));
        for bad in [
            "../x", "a/b", "", ".", "..", ".hidden", "Studio", "a\\b", "1pi",
        ] {
            assert_eq!(paths.machine_file(bad), None, "{bad:?}");
        }
        assert_eq!(
            paths.machine_file("studio"),
            Some(PathBuf::from("/s/machines/studio.json"))
        );
    }

    #[test]
    fn an_attached_record_is_named_for_its_terminal_and_stays_in_its_folder() {
        let paths = Paths::rooted_at(PathBuf::from("/c"), PathBuf::from("/s"));
        assert_eq!(
            paths.attached_record("/dev/ttys004"),
            Some(PathBuf::from("/s/machines/attached/_dev_ttys004.json"))
        );
        // The slashes are what could reach outside the folder.
        let odd = paths.attached_record("/../../x").unwrap();
        assert_eq!(odd.parent(), Some(paths.attached_dir().as_path()));
        assert_eq!(paths.attached_record(""), None);
    }

    #[test]
    fn the_keeper_lock_is_in_the_machines_folder() {
        let paths = Paths::rooted_at(PathBuf::from("/c"), PathBuf::from("/s"));
        assert_eq!(
            paths.keeper_lock_file(),
            PathBuf::from("/s/machines/keeper.lock")
        );
    }

    // `from_env` (and its defaults) reads process-global env vars; serialize every
    // test that touches them so they cannot interleave under the default
    // multi-threaded test runner. Deliberately does NOT manage `HOME`: other test
    // modules in this same test binary (config.rs, tui/actions.rs) call
    // `dirs::home_dir()` unguarded, so mutating the real `HOME` here could race them.
    // Tests below read the ambient `HOME` instead, via the `home_dir()` helper.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Every env var `Paths::from_env` (transitively, via `default_config_dir` /
    // `default_state_dir`) consults, besides `HOME`.
    const MANAGED_VARS: [&str; 4] = [
        "TAW_CONFIG_DIR",
        "TAW_STATE_DIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
    ];

    struct EnvGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
    }

    impl EnvGuard<'static> {
        /// Locks `ENV_LOCK`, clears every `MANAGED_VARS` entry so each test starts
        /// from a known-unset baseline regardless of what the ambient shell
        /// exported, then applies `overrides`.
        fn new(overrides: &[(&str, &str)]) -> Self {
            let guard = EnvGuard {
                _lock: ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
            };
            // SAFETY: serialized by ENV_LOCK above; only this module's tests
            // touch MANAGED_VARS, and only under this lock.
            unsafe {
                for var in MANAGED_VARS {
                    std::env::remove_var(var);
                }
                for (var, value) in overrides {
                    std::env::set_var(var, value);
                }
            }
            guard
        }
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: single-threaded within the lock; no other code in this
            // process reads/writes MANAGED_VARS.
            unsafe {
                for var in MANAGED_VARS {
                    std::env::remove_var(var);
                }
            }
        }
    }

    #[test]
    fn from_env_overrides_take_precedence_and_derive_sub_paths() {
        let _guard = EnvGuard::new(&[
            ("TAW_CONFIG_DIR", "/tmp/taw-test-config"),
            ("TAW_STATE_DIR", "/tmp/taw-test-state"),
        ]);

        let paths = Paths::from_env();

        assert_eq!(paths.config_dir, PathBuf::from("/tmp/taw-test-config"));
        assert_eq!(
            paths.config_file,
            PathBuf::from("/tmp/taw-test-config/config.toml")
        );
        assert_eq!(
            paths.override_dir,
            PathBuf::from("/tmp/taw-test-config/agent-detection")
        );
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/taw-test-state"));
        assert_eq!(
            paths.remote_manifest_dir,
            PathBuf::from("/tmp/taw-test-state/agent-detection/remote")
        );
        assert_eq!(
            paths.manifest_marker,
            PathBuf::from("/tmp/taw-test-state/agent-detection/last-check")
        );
        assert_eq!(
            paths.sessions_dir,
            PathBuf::from("/tmp/taw-test-state/sessions")
        );
        assert_eq!(
            paths.plugins_dir,
            PathBuf::from("/tmp/taw-test-state/plugins")
        );
        assert_eq!(
            paths.plugin_state_dir,
            PathBuf::from("/tmp/taw-test-state/plugins/state")
        );
        assert_eq!(paths.logs_dir, PathBuf::from("/tmp/taw-test-state/logs"));
    }

    #[test]
    fn from_env_falls_back_to_home_when_taw_and_xdg_are_unset() {
        let _guard = EnvGuard::new(&[]);

        let paths = Paths::from_env();

        let expected_config = home_dir().join(".config").join("tmux-agent-wizard");
        let expected_state = home_dir()
            .join(".local")
            .join("state")
            .join("tmux-agent-wizard");
        assert_eq!(paths.config_dir, expected_config);
        assert_eq!(paths.config_file, expected_config.join("config.toml"));
        assert_eq!(paths.state_dir, expected_state);
        assert_eq!(paths.sessions_dir, expected_state.join("sessions"));
    }

    #[test]
    fn xdg_config_home_and_xdg_state_home_are_honoured_when_set() {
        let _guard = EnvGuard::new(&[
            ("XDG_CONFIG_HOME", "/tmp/taw-test-xdg-config"),
            ("XDG_STATE_HOME", "/tmp/taw-test-xdg-state"),
        ]);

        let paths = Paths::from_env();

        assert_eq!(
            paths.config_dir,
            PathBuf::from("/tmp/taw-test-xdg-config/tmux-agent-wizard")
        );
        assert_eq!(
            paths.config_file,
            PathBuf::from("/tmp/taw-test-xdg-config/tmux-agent-wizard/config.toml")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/tmp/taw-test-xdg-state/tmux-agent-wizard")
        );
        assert_eq!(
            paths.sessions_dir,
            PathBuf::from("/tmp/taw-test-xdg-state/tmux-agent-wizard/sessions")
        );
    }

    #[test]
    fn empty_xdg_config_home_and_xdg_state_home_are_treated_as_unset() {
        // The XDG Base Directory Specification treats an empty value the same as
        // unset; a real shell can plausibly export `XDG_CONFIG_HOME=""`.
        let _guard = EnvGuard::new(&[("XDG_CONFIG_HOME", ""), ("XDG_STATE_HOME", "")]);

        let paths = Paths::from_env();

        assert_eq!(
            paths.config_dir,
            home_dir().join(".config").join("tmux-agent-wizard")
        );
        assert_eq!(
            paths.state_dir,
            home_dir()
                .join(".local")
                .join("state")
                .join("tmux-agent-wizard")
        );
    }

    #[test]
    fn taw_config_dir_takes_precedence_over_xdg_config_home() {
        let _guard = EnvGuard::new(&[
            ("TAW_CONFIG_DIR", "/tmp/taw-test-config"),
            ("XDG_CONFIG_HOME", "/tmp/taw-test-xdg-config"),
        ]);

        let paths = Paths::from_env();

        assert_eq!(paths.config_dir, PathBuf::from("/tmp/taw-test-config"));
    }

    #[test]
    fn a_relative_xdg_value_is_ignored_the_way_the_xdg_spec_says_to() {
        // "If an implementation encounters a relative path in any of these variables it
        // should consider the path invalid and ignore it." Resolving one instead would
        // put a user's config and state under whatever directory `taw` happened to be
        // run from — a different place per invocation, and inside their project.
        let _guard = EnvGuard::new(&[
            ("XDG_CONFIG_HOME", "relative/config"),
            ("XDG_STATE_HOME", "../state"),
        ]);

        let paths = Paths::from_env();

        assert_eq!(
            paths.config_dir,
            home_dir().join(".config").join("tmux-agent-wizard"),
            "a relative XDG_CONFIG_HOME falls back to the documented default"
        );
        assert_eq!(
            paths.state_dir,
            home_dir()
                .join(".local")
                .join("state")
                .join("tmux-agent-wizard"),
            "a relative XDG_STATE_HOME falls back to the documented default"
        );
    }

    #[test]
    fn ensure_creates_every_directory_but_not_the_marker_files() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        let state_dir = tmp.path().join("state");
        let paths = Paths::rooted_at(config_dir, state_dir);

        paths.ensure().unwrap();

        assert!(paths.config_dir.is_dir());
        assert!(paths.state_dir.is_dir());
        assert!(paths.override_dir.is_dir());
        assert!(paths.remote_manifest_dir.is_dir());
        assert!(paths.sessions_dir.is_dir());
        assert!(paths.plugins_dir.is_dir());
        assert!(paths.plugin_state_dir.is_dir());
        assert!(paths.logs_dir.is_dir());
        // Files are never created by ensure(); only their parent directories are.
        assert!(!paths.config_file.exists());
        assert!(!paths.manifest_marker.exists());
        assert!(paths.manifest_marker.parent().unwrap().is_dir());
    }
}
