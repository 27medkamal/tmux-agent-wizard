//! What `taw integration install|uninstall|status opencode` does to an OpenCode config
//! directory: the three plugin files, the entries that register the TUI plugin for OpenCode
//! 1 (`tui.jsonc`, or `tui.json`) and OpenCode 2 (`cli.json`), and what an uninstall gives
//! back.
//!
//! The rules under test are the ones every integration keeps (see `src/integration/mod.rs`):
//! whatever cannot be edited is refused before anything is written, the file as it was found
//! is kept for a byte-for-byte revert, and an entry is never written back on the way out. What
//! is particular to OpenCode is that the configs are JSONC, so a user's comments and trailing
//! commas have to come through both ways.
//!
//! Every test holds [`ENV_LOCK`]: they set `$XDG_CONFIG_HOME` and `$XDG_STATE_HOME`, which are
//! process-global.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};
use taw::integration::{IntegrationError, install, status, uninstall};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: the caller holds `ENV_LOCK` for the guard's whole lifetime.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// An OpenCode config directory that exists, its state directory, and the environment that
/// points `taw` at them. Nothing here is under the real `$HOME`.
struct Machine {
    _root: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    taw: PathBuf,
    _env: [EnvGuard; 2],
}

fn machine() -> Machine {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config/opencode");
    let state = root.path().join("state/opencode");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&state).unwrap();
    let env = [
        EnvGuard::set("XDG_CONFIG_HOME", &root.path().join("config")),
        EnvGuard::set("XDG_STATE_HOME", &root.path().join("state")),
    ];
    Machine {
        taw: root.path().join("bin/taw"),
        config,
        state,
        _root: root,
        _env: env,
    }
}

impl Machine {
    fn path(&self, name: &str) -> PathBuf {
        self.config.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_else(|err| panic!("{name}: {err}"))
    }

    /// A file of the config directory as JSON; only for files taw made, which have no
    /// comments.
    fn json(&self, name: &str) -> Value {
        serde_json::from_str(&self.read(name)).unwrap_or_else(|err| panic!("{name}: {err}"))
    }

    fn install(&self) -> taw::integration::Report {
        install("opencode", &self.taw).expect("install")
    }

    /// Everything in the config directory, by name, for "nothing was written".
    fn listing(&self) -> Vec<String> {
        let mut names: Vec<String> = walk(&self.config)
            .into_iter()
            .map(|p| p.strip_prefix(&self.config).unwrap().display().to_string())
            .collect();
        names.sort();
        names
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

const TUI_SPEC: &str = "./taw-tui-session.js";
const V2_SPEC: &str = "./taw-opencode";

#[test]
fn a_fresh_install_writes_the_plugins_and_registers_the_tui_plugin_for_both_versions() {
    let _g = lock();
    let m = machine();

    let report = m.install();

    assert_eq!(report.agent, "opencode");
    // The server plugin is where OpenCode finds it by convention; the TUI plugin is beside
    // the config, where the registration names it; version 2 loads a directory.
    for name in [
        "plugins/taw-agent-state.js",
        "taw-tui-session.js",
        "taw-opencode/tui.js",
    ] {
        assert!(m.path(name).is_file(), "{name} was not written");
        assert!(
            report.files.contains(&m.path(name)),
            "{name} is not in {:?}",
            report.files
        );
    }
    let tui_plugin = m.read("taw-tui-session.js");
    assert!(tui_plugin.contains("// TAW_INTEGRATION_VERSION=5"));
    assert!(tui_plugin.contains(&m.taw.display().to_string()));
    assert!(
        !tui_plugin.contains("{{"),
        "a placeholder was left in the plugin"
    );
    let entry = m.read("taw-opencode/tui.js");
    assert!(entry.contains(r#"export { default } from "../taw-tui-session.js";"#));
    assert!(!entry.contains("{{"));

    // Version 1 reads `tui.jsonc`, and a fresh install leaves `tui.json` for OpenCode to
    // migrate its old settings into. Version 2 reads `cli.json`.
    assert_eq!(m.json("tui.jsonc"), json!({ "plugin": [TUI_SPEC] }));
    assert!(!m.path("tui.json").exists());
    assert_eq!(m.json("cli.json"), json!({ "plugins": [V2_SPEC] }));
    for config in [m.path("tui.jsonc"), m.path("cli.json")] {
        assert!(report.files.contains(&config), "{config:?} is not reported");
    }
}

#[test]
fn an_install_over_a_users_configs_keeps_their_comments_and_an_uninstall_gives_them_back() {
    let _g = lock();
    let m = machine();
    let tui = concat!(
        "{\n",
        "  // My theme.\n",
        "  \"theme\": \"system\",\n",
        "  \"plugin\": [\"example\", [\"configured\", {\"enabled\": true}]],\n",
        "}\n",
    );
    let cli =
        "{\n  /* mine */ \"theme\": { \"name\": \"catppuccin\" },\n  \"plugins\": [\"one\"]\n}\n";
    m.write("tui.jsonc", tui);
    m.write("cli.json", cli);

    m.install();

    let installed = m.read("tui.jsonc");
    assert!(installed.contains("// My theme."));
    assert!(installed.contains(TUI_SPEC));
    assert!(installed.contains(r#"["configured", {"enabled": true}]"#));
    let installed_cli = m.read("cli.json");
    assert!(installed_cli.contains("/* mine */") && installed_cli.contains(V2_SPEC));

    uninstall("opencode").expect("uninstall");

    assert_eq!(m.read("tui.jsonc"), tui);
    assert_eq!(m.read("cli.json"), cli);
    // What install made is gone, and so is what it kept to give the files back.
    assert_eq!(m.listing(), ["cli.json", "tui.jsonc"]);
}

#[test]
fn an_uninstall_of_a_fresh_install_leaves_nothing_behind() {
    let _g = lock();
    let m = machine();
    m.install();

    let report = uninstall("opencode").expect("uninstall");

    assert_eq!(m.listing(), Vec::<String>::new(), "{report:?}");
    assert!(!m.path("taw-opencode").exists());
    assert!(!status("opencode").unwrap().installed);
}

#[test]
fn a_reinstall_changes_nothing_and_keeps_the_copies_of_the_files_as_they_were_found() {
    let _g = lock();
    let m = machine();
    let tui = "{\n  \"theme\": \"system\"\n}\n";
    m.write("tui.jsonc", tui);
    m.install();
    let after_first = (m.read("tui.jsonc"), m.read("cli.json"), m.listing());

    m.install();

    assert_eq!(
        (m.read("tui.jsonc"), m.read("cli.json"), m.listing()),
        after_first
    );
    uninstall("opencode").expect("uninstall");
    assert_eq!(
        m.read("tui.jsonc"),
        tui,
        "the first copy is the one that counts"
    );
    assert!(!m.path("cli.json").exists());
}

#[test]
fn a_tui_json_that_already_holds_the_plugin_is_where_it_stays() {
    let _g = lock();
    let m = machine();
    let tui = format!("{{\"plugin\": [\"{TUI_SPEC}\"]}}");
    m.write("tui.json", &tui);

    let report = m.install();

    assert!(
        !m.path("tui.jsonc").exists(),
        "no second file for a plugin already registered"
    );
    assert_eq!(m.read("tui.json"), tui);
    assert!(
        !report.files.contains(&m.path("tui.json")),
        "nothing was written to it"
    );

    uninstall("opencode").expect("uninstall");

    assert_eq!(m.json("tui.json"), json!({}));
}

#[test]
fn cli_json_waits_while_opencode_has_settings_to_migrate_into_one() {
    let _g = lock();
    // A tui.json is one of the two things OpenCode 2 moves into a cli.json of its own, the
    // first time it starts, and only while there is none.
    let m = machine();
    m.write("tui.json", "{\"theme\": \"system\"}");
    m.install();
    assert!(!m.path("cli.json").exists());
    assert_eq!(m.json("tui.jsonc"), json!({ "plugin": [TUI_SPEC] }));
    uninstall("opencode").expect("uninstall");

    // And so is a kv.json in OpenCode's state directory.
    fs::remove_file(m.path("tui.json")).unwrap();
    fs::write(m.state.join("kv.json"), "{}").unwrap();
    m.install();
    assert!(!m.path("cli.json").exists());
}

#[test]
fn the_install_says_to_restart_opencode_and_when_version_2_has_to_wait() {
    let _g = lock();
    let m = machine();
    let report = m.install();
    assert!(
        report.message.contains("restart OpenCode"),
        "{}",
        report.message
    );
    assert!(!report.message.contains("cli.json"), "{}", report.message);
    uninstall("opencode").expect("uninstall");

    m.write("tui.json", "{}");
    let report = m.install();
    assert!(
        report.message.contains("restart OpenCode"),
        "{}",
        report.message
    );
    assert!(
        report.message.contains("cli.json") && report.message.contains("run this install again"),
        "{}",
        report.message
    );
}

#[test]
fn the_installed_files_are_the_templates_with_the_taw_path_and_the_agent_filled_in() {
    let _g = lock();
    let m = machine();
    let taw = "/opt/taw \"quoted\"/bin/taw";
    install("opencode", Path::new(taw)).expect("install");

    let render = |template: &str| {
        template
            .replace("{{TAW_BIN}}", &format!("\"{}\"", taw.replace('"', "\\\"")))
            .replace("{{AGENT}}", "opencode")
    };
    assert_eq!(
        m.read("taw-tui-session.js"),
        render(include_str!(
            "../src/integration/templates/taw-tui-session.js"
        ))
    );
    assert_eq!(
        m.read("taw-opencode/tui.js"),
        render(include_str!(
            "../src/integration/templates/taw-opencode-tui.js"
        ))
    );
    assert_eq!(
        m.read("plugins/taw-agent-state.js"),
        render(include_str!(
            "../src/integration/templates/taw-agent-state-opencode.js"
        ))
    );
}

#[test]
fn version_2_is_registered_by_the_next_install_once_opencode_has_made_its_cli_json() {
    let _g = lock();
    let m = machine();
    m.write("tui.json", "{\"theme\": \"system\"}");
    m.install();
    assert!(!m.path("cli.json").exists());

    // OpenCode 2 starts, moves the old settings into a cli.json of its own, and removes the
    // old file.
    fs::remove_file(m.path("tui.json")).unwrap();
    let migrated = "{\n  \"theme\": \"system\"\n}\n";
    m.write("cli.json", migrated);
    let report = m.install();

    assert_eq!(
        m.json("cli.json"),
        json!({"theme": "system", "plugins": [V2_SPEC]})
    );
    assert!(!report.message.contains("cli.json"), "{}", report.message);
    uninstall("opencode").expect("uninstall");
    assert_eq!(m.read("cli.json"), migrated);
}

#[test]
fn a_config_it_cannot_edit_is_refused_by_name_and_nothing_is_written() {
    let _g = lock();
    for (name, body) in [
        ("cli.json", r#"{"plugins": {}}"#),
        ("tui.jsonc", r#"{"plugin": "one"}"#),
        ("tui.json", "not json at all"),
        ("cli.json", "[]"),
    ] {
        let m = machine();
        m.write(name, body);
        let before = m.listing();

        let err = install("opencode", &m.taw).expect_err(name);

        assert!(
            matches!(
                err.downcast_ref::<IntegrationError>(),
                Some(IntegrationError::UnusableConfig { .. })
            ),
            "{name}: {err:#}"
        );
        assert!(err.to_string().contains(name), "{name}: {err}");
        assert_eq!(m.listing(), before, "{name}: something was written");
        assert_eq!(m.read(name), body);
    }
}

#[cfg(unix)]
#[test]
fn a_read_only_or_hard_linked_config_is_refused_before_a_script_is_written() {
    use std::os::unix::fs::PermissionsExt;

    let _g = lock();
    let m = machine();
    let cli = m.write("cli.json", "{}\n");
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o444)).unwrap();
    // Root can write anything, and then there is nothing to refuse.
    let refused = fs::OpenOptions::new().write(true).open(&cli).is_err();
    if refused {
        let before = m.listing();
        let err = install("opencode", &m.taw).unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<IntegrationError>(),
                Some(IntegrationError::ConfigReadOnly { .. })
            ),
            "{err:#}"
        );
        assert!(err.to_string().contains("cli.json is read-only"), "{err}");
        assert_eq!(m.listing(), before);
    }
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o644)).unwrap();

    // A second name for a file makes a rename-based write a lie about the other name.
    let elsewhere = m.config.join("..").join("cli-elsewhere.json");
    fs::hard_link(&cli, &elsewhere).unwrap();
    let before = m.listing();
    let err = install("opencode", &m.taw).unwrap_err();
    assert!(err.to_string().contains("multiple hard links"), "{err:#}");
    assert_eq!(m.listing(), before);
}

#[cfg(unix)]
#[test]
fn a_plugin_file_that_cannot_be_written_is_refused_before_any_other_is() {
    use std::os::unix::fs::PermissionsExt;

    let _g = lock();
    let m = machine();
    m.install();
    let tui_plugin = m.path("taw-tui-session.js");
    fs::set_permissions(&tui_plugin, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&tui_plugin).is_ok() {
        return; // root: nothing to refuse
    }
    let server_before = m.read("plugins/taw-agent-state.js");

    // A new taw path would change every plugin file: none of them may change.
    let err = install("opencode", &m.taw.with_file_name("moved-taw")).unwrap_err();

    assert!(
        matches!(
            err.downcast_ref::<IntegrationError>(),
            Some(IntegrationError::ConfigReadOnly { .. })
        ),
        "{err:#}"
    );
    assert!(
        err.to_string().contains("taw-tui-session.js is read-only"),
        "{err}"
    );
    assert!(
        m.read("plugins/taw-agent-state.js") == server_before,
        "the server plugin was rewritten before the refusal"
    );
    fs::set_permissions(&tui_plugin, fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_is_written_through_and_stays_a_link() {
    use std::os::unix::fs::symlink;

    let _g = lock();
    let m = machine();
    let dotfiles = m.config.join("../dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("tui.jsonc");
    fs::write(&real, "{\n  // mine\n  \"theme\": \"system\"\n}\n").unwrap();
    let link = m.path("tui.jsonc");
    symlink(&real, &link).unwrap();

    m.install();

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&real).unwrap().contains(TUI_SPEC));
    assert!(fs::read_to_string(&real).unwrap().contains("// mine"));

    uninstall("opencode").expect("uninstall");

    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        "{\n  // mine\n  \"theme\": \"system\"\n}\n"
    );
}

#[test]
fn an_uninstall_keeps_what_the_user_changed_since_and_takes_only_taws_entries() {
    let _g = lock();
    let m = machine();
    m.write("tui.jsonc", "{\n  \"theme\": \"system\"\n}\n");
    m.install();
    let edited = m
        .read("tui.jsonc")
        .replace("\"system\"", "\"tokyonight\"")
        .replace("{\n", "{\n  // added later\n");
    m.write("tui.jsonc", &edited);

    uninstall("opencode").expect("uninstall");

    let left = m.read("tui.jsonc");
    assert!(
        left.contains("// added later") && left.contains("tokyonight"),
        "{left}"
    );
    assert!(!left.contains("taw-tui-session"), "{left}");
    assert!(!m.path("taw-tui-session.js").exists());
}

#[test]
fn an_uninstall_that_cannot_edit_a_config_holding_taws_entry_refuses_and_removes_nothing() {
    let _g = lock();
    let m = machine();
    m.install();
    let installed = m.listing();

    // The user broke the file.
    let broken = "{\"plugin\": [\"./taw-tui-session.js\"";
    m.write("tui.jsonc", broken);
    let err = uninstall("opencode").unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<IntegrationError>(),
            Some(IntegrationError::UnusableConfig { .. })
        ),
        "{err:#}"
    );
    assert!(err.to_string().contains("tui.jsonc"), "{err}");
    assert_eq!(m.listing(), installed, "nothing was removed");
    assert_eq!(m.read("tui.jsonc"), broken);
}

#[test]
fn an_uninstall_does_not_mind_a_broken_config_that_never_held_taws_entry() {
    let _g = lock();
    let m = machine();
    m.install();
    m.write("tui.json", "not json at all");

    uninstall("opencode").expect("a file taw never wrote to is not its business");

    assert_eq!(m.read("tui.json"), "not json at all");
    assert!(!m.path("taw-tui-session.js").exists());
}

#[cfg(unix)]
#[test]
fn an_uninstall_refuses_a_config_that_was_made_read_only_since() {
    use std::os::unix::fs::PermissionsExt;

    let _g = lock();
    let m = machine();
    m.install();
    let installed = m.listing();
    let cli = m.path("cli.json");
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&cli).is_ok() {
        return; // root: nothing to refuse
    }

    let err = uninstall("opencode").unwrap_err();

    assert!(
        matches!(
            err.downcast_ref::<IntegrationError>(),
            Some(IntegrationError::ConfigReadOnly { .. })
        ),
        "{err:#}"
    );
    assert_eq!(m.listing(), installed, "nothing was removed");
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn an_uninstall_that_was_never_preceded_by_an_install_is_not_an_error() {
    let _g = lock();
    let m = machine();
    let tui = "{\n  \"theme\": \"system\"\n}\n";
    m.write("tui.jsonc", tui);

    uninstall("opencode").expect("uninstall");

    assert_eq!(m.read("tui.jsonc"), tui);
    assert_eq!(m.listing(), ["tui.jsonc"]);
}

#[test]
fn status_reports_the_server_plugin_and_its_version_marker() {
    let _g = lock();
    let m = machine();
    assert!(!status("opencode").unwrap().installed);

    m.install();
    let during = status("opencode").unwrap();

    assert!(during.installed);
    assert_eq!(during.path, m.path("plugins/taw-agent-state.js"));
    assert_eq!(during.version, Some(5));
}

#[cfg(unix)]
#[test]
fn the_copy_of_a_private_config_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let _g = lock();
    let m = machine();
    let private = fs::Permissions::from_mode(0o600);
    for name in ["tui.jsonc", "cli.json"] {
        m.write(name, "{\"theme\": \"system\"}\n");
        fs::set_permissions(m.path(name), private.clone()).unwrap();
    }

    m.install();

    let copies: Vec<String> = m
        .listing()
        .into_iter()
        .filter(|name| name.ends_with(".orig"))
        .collect();
    assert_eq!(copies.len(), 2, "{copies:?}");
    for name in copies
        .iter()
        .map(String::as_str)
        .chain(["tui.jsonc", "cli.json"])
    {
        let mode = fs::metadata(m.path(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
}

#[test]
fn a_fresh_install_is_not_outdated() {
    let _g = lock();
    let m = machine();

    m.install();
    let fresh = status("opencode").unwrap();

    assert_eq!(fresh.current, Some(5));
    assert!(!fresh.outdated(), "{:?}", fresh.note());
    assert_eq!(fresh.note(), None);
}

#[test]
fn a_registration_put_off_is_noted_until_a_second_install_adds_it() {
    let _g = lock();
    let m = machine();
    // Settings of version 1 that OpenCode 2 has yet to move into a `cli.json` of its own.
    fs::write(m.state.join("kv.json"), "{}\n").unwrap();

    m.install();
    assert!(
        !m.config.join("cli.json").exists(),
        "the install put it off"
    );
    // Nothing a second install does would change that yet, so the integration is not outdated,
    // and status says what it waits for.
    let waiting = status("opencode").unwrap();
    assert!(waiting.installed);
    assert!(!waiting.outdated());
    let note = waiting.note().unwrap();
    assert!(
        note.contains("cli.json") && note.contains("Start OpenCode 2 once"),
        "{note}"
    );

    // OpenCode 2 starts, and makes a `cli.json` with none of taw's in it: now a second install is
    // what adds the entry, so it is outdated, and the note no longer says to start OpenCode 2.
    m.write("cli.json", "{}\n");
    let started = status("opencode").unwrap();
    assert!(started.outdated());
    assert!(!started.note().unwrap().contains("Start OpenCode 2 once"));

    m.install();
    let done = status("opencode").unwrap();
    assert!(!done.outdated(), "{:?}", done.note());
    assert_eq!(done.note(), None);
}

#[test]
fn a_home_of_opencode_1_only_is_never_outdated_for_want_of_a_cli_json() {
    let _g = lock();
    // What waits for OpenCode 2 to make a `cli.json`: its `kv.json`, or a `tui.json`.
    for waiting_on in ["kv.json", "tui.json"] {
        let m = machine();
        let settings = if waiting_on == "kv.json" {
            m.state.join(waiting_on)
        } else {
            m.path(waiting_on)
        };
        fs::write(&settings, "{}\n").unwrap();

        m.install();

        let waiting = status("opencode").unwrap();
        assert!(!waiting.outdated(), "{waiting_on}: {:?}", waiting.note());
        assert!(waiting.note().is_some(), "{waiting_on}");
    }
}

#[test]
fn a_config_directory_that_does_not_exist_is_refused_as_before() {
    let _g = lock();
    let root = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("XDG_CONFIG_HOME", root.path());
    let _state = EnvGuard::set("XDG_STATE_HOME", root.path());

    let err = install("opencode", &root.path().join("taw")).unwrap_err();

    assert!(err.to_string().contains("config_dir_missing"), "{err}");
    assert!(!root.path().join("opencode").exists());
}
