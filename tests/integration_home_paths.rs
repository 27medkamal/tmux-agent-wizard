//! What an install writes for the home folder, and what it makes of a config that spells it
//! another way.
//!
//! Installs keep the home folder implicit where the agent expands it (`"$HOME/..."` in a hook,
//! `${process.env.HOME}` in a plugin), and know their own entry when a config spells the script
//! as `~/...`, `$HOME/...` or `${HOME}/...`, as a dotfiles repository does. Every install here
//! runs under a scratch `$HOME`; nothing is read from or written to the real one.
//!
//! Every test holds `ENV_LOCK`: they set `$HOME` and the variables that move an agent's config
//! folder, which are process-wide.

#[path = "common/exec.rs"]
mod exec;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};
use taw::integration::{install, status, uninstall};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets (or removes) an environment variable for the guard's lifetime.
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

    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(key) };
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

/// A scratch home folder named `home_name` (which may be odd), with the environment that makes
/// `taw` find every agent's config under it. `outside` is a folder next to it that is not under
/// it.
struct Machine {
    _root: tempfile::TempDir,
    home: PathBuf,
    outside: PathBuf,
    _env: Vec<EnvGuard>,
}

fn machine_named(home_name: &str) -> Machine {
    let root = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let home = base.join(home_name);
    let outside = base.join("outside");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let env = vec![
        EnvGuard::set("HOME", &home),
        EnvGuard::unset("CLAUDE_CONFIG_DIR"),
        EnvGuard::unset("XDG_CONFIG_HOME"),
        EnvGuard::unset("XDG_STATE_HOME"),
        EnvGuard::unset("HERMES_HOME"),
        EnvGuard::unset("PI_CODING_AGENT_DIR"),
        EnvGuard::unset("PI_CONFIG_DIR"),
    ];
    Machine {
        _root: root,
        home,
        outside,
        _env: env,
    }
}

fn machine() -> Machine {
    machine_named("home")
}

fn make_executable(path: &Path) {
    crate::exec::make_executable(path);
}

/// A fake `taw` at `path` that writes its argv, one element per line, to `$FAKE_TAW_RECORD`.
fn fake_taw_at(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        "#!/bin/sh\nout=\"${FAKE_TAW_RECORD:-}\"\n[ -n \"$out\" ] || exit 0\n: > \"$out\"\n\
         for a in \"$@\"; do\n    printf '%s\\n' \"$a\" >> \"$out\"\ndone\n",
    )
    .unwrap();
    make_executable(path);
}

const SESSION_START: &str = r#"{"session_id":"abc","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","source":"startup"}"#;

/// Runs `argv` with `$HOME` set to `home`, `TMUX_PANE` `%1`, and `session` on its stdin, and says
/// what the fake taw it ran was called with (empty when it was not).
fn run_hook(argv: &[&str], home: &Path, record: &Path) -> Vec<String> {
    let _ = fs::remove_file(record);
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .env("HOME", home)
        .env("TMUX_PANE", "%1")
        .env("FAKE_TAW_RECORD", record)
        .env_remove("TAW_REPORT_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SESSION_START.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "hook failed: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(record)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn assert_reported_session(argv: &[String]) {
    assert!(
        argv.len() > 3 && argv[..3] == ["pane", "report-agent-session", "%1"],
        "taw was not run as the hook runs it: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--agent-session-id" && pair[1] == "abc"),
        "{argv:?}"
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn session_start_commands(settings: &Path) -> Vec<String> {
    let value = read_json(settings);
    value["hooks"]["SessionStart"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap().clone())
        .map(|hook| hook["command"].as_str().unwrap().to_string())
        .collect()
}

// =====================================================================================
// Claude: the entry an install writes
// =====================================================================================

#[test]
fn a_fresh_claude_install_writes_the_hook_from_the_home_folder_and_the_hook_runs() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);

    install("claude", &taw).unwrap();

    let settings = m.home.join(".claude/settings.json");
    let commands = session_start_commands(&settings);
    assert_eq!(
        commands,
        [r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#]
    );
    // Neither the settings nor the script carries the home folder.
    let home = m.home.to_string_lossy().to_string();
    assert!(!fs::read_to_string(&settings).unwrap().contains(&home));
    let script = fs::read_to_string(m.home.join(".claude/hooks/taw-agent-state.sh")).unwrap();
    assert!(!script.contains(&home), "{script}");
    assert!(
        script.contains("\nTAW_BIN=\"${HOME-}/bin/taw\"\n"),
        "{script}"
    );

    // The command is what Claude runs, through a shell.
    let record = m.outside.join("record.txt");
    let argv = run_hook(&["bash", "-c", &commands[0]], &m.home, &record);
    assert_reported_session(&argv);
}

#[test]
fn a_claude_install_in_a_config_folder_under_the_home_folder_names_it_from_the_home_folder() {
    let _g = lock();
    let m = machine();
    let work = m.home.join(".claude-work");
    fs::create_dir_all(&work).unwrap();
    let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &work);
    let taw = m.outside.join("taw");
    fake_taw_at(&taw);

    install("claude", &taw).unwrap();

    let commands = session_start_commands(&work.join("settings.json"));
    assert_eq!(
        commands,
        [r#"bash "$HOME/.claude-work/hooks/taw-agent-state.sh" session"#]
    );
    // Taw is not under the home folder, so the script names it in full.
    let script = fs::read_to_string(work.join("hooks/taw-agent-state.sh")).unwrap();
    assert!(
        script.contains(&format!("\nTAW_BIN='{}'\n", taw.display())),
        "{script}"
    );
    let record = m.outside.join("record.txt");
    assert_reported_session(&run_hook(&["bash", "-c", &commands[0]], &m.home, &record));
}

#[test]
fn a_claude_install_in_a_config_folder_outside_the_home_folder_names_it_in_full() {
    let _g = lock();
    let m = machine();
    let config = m.outside.join("claude");
    fs::create_dir_all(&config).unwrap();
    let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &config);
    let taw = m.outside.join("taw");
    fake_taw_at(&taw);

    install("claude", &taw).unwrap();

    let script = config.join("hooks/taw-agent-state.sh");
    assert_eq!(
        session_start_commands(&config.join("settings.json")),
        [format!("bash '{}' session", script.display())]
    );
}

// =====================================================================================
// Claude: an entry the config already has
// =====================================================================================

/// The matcher Claude's entry is written with.
const CURRENT_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";

fn claude_settings_with(entry: &Value, other: &Value) -> Value {
    json!({
        "theme": "dark",
        "hooks": {"SessionStart": [other, entry], "PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "echo pre"}
        ]}]}
    })
}

#[test]
fn an_install_over_an_entry_spelled_from_the_home_folder_adds_nothing_and_leaves_it_as_it_is() {
    for spelling in [
        "bash ~/.claude/hooks/taw-agent-state.sh session",
        "bash $HOME/.claude/hooks/taw-agent-state.sh session",
        "bash ${HOME}/.claude/hooks/taw-agent-state.sh session",
        r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#,
        // The variable quoted apart from the slash after it is the same path.
        r#"bash "$HOME"/.claude/hooks/taw-agent-state.sh session"#,
    ] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        // A user's entry as a dotfiles repository has it: the matcher and timeout an install
        // writes, and the path spelled from the home folder.
        let mine = json!({"matcher": CURRENT_MATCHER, "hooks": [
            {"type": "command", "command": spelling, "timeout": 10}
        ]});
        let other = json!({"matcher": "*", "hooks": [{"type": "command", "command": "echo hi"}]});
        let settings = m.home.join(".claude/settings.json");
        let seeded = claude_settings_with(&mine, &other);
        fs::write(&settings, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();

        install("claude", &taw).unwrap();

        let after = read_json(&settings);
        assert_eq!(
            after["hooks"]["SessionStart"],
            json!([other, mine]),
            "{spelling}"
        );
        assert_eq!(after, seeded, "{spelling}");
        // Byte for byte: the entry is written out as it was read.
        assert_eq!(
            serde_json::to_string(&after["hooks"]["SessionStart"][1]).unwrap(),
            serde_json::to_string(&mine).unwrap(),
            "{spelling}"
        );
        // The entry runs the script the install wrote.
        let record = m.outside.join("record.txt");
        let argv = run_hook(&["bash", "-c", spelling], &m.home, &record);
        assert_reported_session(&argv);

        uninstall("claude").unwrap();

        // Taw's entry is gone, with the script, and the rest of the file is as it was.
        let restored = read_json(&settings);
        assert_eq!(
            restored,
            json!({
                "theme": "dark",
                "hooks": {"SessionStart": [other], "PreToolUse": seeded["hooks"]["PreToolUse"]}
            }),
            "{spelling}"
        );
        assert!(!m.home.join(".claude/hooks/taw-agent-state.sh").exists());
    }
}

#[test]
fn an_install_over_the_two_entries_the_old_install_left_keeps_one_spelled_from_the_home_folder() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    let script = m.home.join(".claude/hooks/taw-agent-state.sh");
    let mine = json!({"matcher": CURRENT_MATCHER, "hooks": [
        {"type": "command", "command": "bash ~/.claude/hooks/taw-agent-state.sh session", "timeout": 10}
    ]});
    let absolute = json!({"matcher": CURRENT_MATCHER, "hooks": [
        {"type": "command", "command": format!("bash '{}' session", script.display()), "timeout": 10}
    ]});
    let settings = m.home.join(".claude/settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [mine, absolute]}}))
            .unwrap(),
    )
    .unwrap();

    install("claude", &taw).unwrap();

    assert_eq!(read_json(&settings)["hooks"]["SessionStart"], json!([mine]));
}

#[test]
fn an_install_over_an_entry_with_the_absolute_path_replaces_it_with_the_home_relative_one() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    let script = m.home.join(".claude/hooks/taw-agent-state.sh");
    let old = json!({"matcher": "*", "hooks": [
        {"type": "command", "command": format!("bash '{}' session", script.display()), "timeout": 10}
    ]});
    let settings = m.home.join(".claude/settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [old]}})).unwrap(),
    )
    .unwrap();

    install("claude", &taw).unwrap();

    assert_eq!(
        session_start_commands(&settings),
        [r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#]
    );
}

#[test]
fn a_hook_of_the_same_name_in_another_folder_is_not_taws_and_stays() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude")).unwrap();
    fs::create_dir_all(m.home.join(".other/hooks")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    let theirs = json!({"hooks": [{"type": "command", "command": "bash ~/.other/hooks/taw-agent-state.sh session"}]});
    let settings = m.home.join(".claude/settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [theirs]}})).unwrap(),
    )
    .unwrap();

    install("claude", &taw).unwrap();
    assert_eq!(session_start_commands(&settings).len(), 2);

    uninstall("claude").unwrap();
    assert_eq!(
        read_json(&settings)["hooks"]["SessionStart"],
        json!([theirs])
    );
}

#[test]
fn an_uninstall_refuses_a_path_it_cannot_resolve_without_a_shell_as_before() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    install("claude", &taw).unwrap();
    let settings = m.home.join(".claude/settings.json");
    let mut value = read_json(&settings);
    value["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({"hooks": [{"type": "command", "command": "bash $XDG_CONFIG_HOME/taw-agent-state.sh session"}]}));
    fs::write(&settings, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    let before = fs::read(&settings).unwrap();

    let err = uninstall("claude").unwrap_err().to_string();

    assert!(err.contains("taw cannot tell"), "{err}");
    assert_eq!(fs::read(&settings).unwrap(), before);
}

// =====================================================================================
// Status judges by the version marker only
// =====================================================================================

#[test]
fn a_script_written_with_the_absolute_spelling_at_this_version_is_installed_and_not_outdated() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude")).unwrap();
    // Taw outside the home folder: the script names it in full, as every script did before.
    let taw = m.outside.join("taw");
    fake_taw_at(&taw);
    install("claude", &taw).unwrap();
    let script = fs::read_to_string(m.home.join(".claude/hooks/taw-agent-state.sh")).unwrap();
    assert!(
        script.contains(&format!("TAW_BIN='{}'", taw.display())),
        "{script}"
    );

    let st = status("claude").unwrap();

    assert!(st.installed);
    assert!(!st.outdated(), "{st:?}");
    assert_eq!(st.version, st.current);
}

// =====================================================================================
// The shell hook scripts
// =====================================================================================

#[test]
fn a_shell_hook_script_names_taw_from_the_home_folder_and_runs() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".codex")).unwrap();
    let _codex = EnvGuard::unset("CODEX_HOME");
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);

    install("codex", &taw).unwrap();

    let script = m.home.join(".codex/taw-agent-state.sh");
    let text = fs::read_to_string(&script).unwrap();
    assert!(text.contains("\nTAW_BIN=\"${HOME-}/bin/taw\"\n"), "{text}");
    let argv = run_hook(
        &["sh", script.to_str().unwrap(), "session"],
        &m.home,
        &m.outside.join("record.txt"),
    );
    assert_eq!(
        &argv[..3],
        ["pane", "report-agent-session", "%1"],
        "{argv:?}"
    );
}

#[test]
fn a_taw_path_with_a_space_under_a_home_folder_with_odd_characters_round_trips() {
    let _g = lock();
    // The home folder is never written, so whatever its name holds does not matter.
    let m = machine_named("my home $x \"q\" `b` \\d");
    fs::create_dir_all(m.home.join(".codex")).unwrap();
    let _codex = EnvGuard::unset("CODEX_HOME");
    let taw = m.home.join("bin dir/taw");
    fake_taw_at(&taw);

    install("codex", &taw).unwrap();

    let script = m.home.join(".codex/taw-agent-state.sh");
    let text = fs::read_to_string(&script).unwrap();
    assert!(
        text.contains("\nTAW_BIN=\"${HOME-}/bin dir/taw\"\n"),
        "{text}"
    );
    let argv = run_hook(
        &["sh", script.to_str().unwrap(), "session"],
        &m.home,
        &m.outside.join("record.txt"),
    );
    assert_eq!(
        &argv[..3],
        ["pane", "report-agent-session", "%1"],
        "{argv:?}"
    );
}

#[test]
fn a_taw_path_with_a_dollar_a_quote_a_backtick_or_a_backslash_is_written_in_full_and_runs() {
    // Only a plain path is written from the home folder: these characters are left to the
    // absolute spelling, whose single quotes read every one of them as itself.
    for odd in ["$y", "\"r\"", "`c`", "\\d", "it's", "$(id)"] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".codex")).unwrap();
        let _codex = EnvGuard::unset("CODEX_HOME");
        let taw = m.home.join(format!("bin {odd}/taw"));
        fake_taw_at(&taw);

        install("codex", &taw).unwrap();

        let script = m.home.join(".codex/taw-agent-state.sh");
        let text = fs::read_to_string(&script).unwrap();
        let line = text
            .lines()
            .find(|line| line.starts_with("TAW_BIN="))
            .unwrap();
        assert!(!line.contains("HOME"), "{odd}: {line}");
        let argv = run_hook(
            &["sh", script.to_str().unwrap(), "session"],
            &m.home,
            &m.outside.join("record.txt"),
        );
        assert_eq!(
            &argv[..3],
            ["pane", "report-agent-session", "%1"],
            "{odd}: {argv:?}"
        );
    }
}

// =====================================================================================
// The plugins
// =====================================================================================

fn opencode_plugin_files(m: &Machine) -> [PathBuf; 3] {
    let config = m.home.join(".config/opencode");
    [
        config.join("plugins/taw-agent-state.js"),
        config.join("taw-tui-session.js"),
        config.join("taw-opencode/tui.js"),
    ]
}

#[test]
fn an_opencode_install_with_taw_under_the_home_folder_writes_it_from_the_home_folder() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".config/opencode")).unwrap();
    let taw = m
        .home
        .join(".config/tmux/plugins/tmux-agent-wizard/bin/taw");
    fake_taw_at(&taw);

    install("opencode", &taw).unwrap();

    let home = m.home.to_string_lossy().to_string();
    let [server, tui, _entry] = opencode_plugin_files(&m);
    for plugin in [&server, &tui] {
        let text = fs::read_to_string(plugin).unwrap();
        assert!(
            text.contains(
                "\nconst TAW_BIN = `${process.env.HOME}/.config/tmux/plugins/tmux-agent-wizard/bin/taw`;\n"
            ),
            "{}",
            plugin.display()
        );
        assert!(!text.contains(&home), "{}", plugin.display());
    }
}

#[test]
fn an_opencode_install_with_taw_elsewhere_writes_the_absolute_path() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".config/opencode")).unwrap();
    let taw = m.outside.join("taw");
    fake_taw_at(&taw);

    install("opencode", &taw).unwrap();

    let [server, tui, _entry] = opencode_plugin_files(&m);
    for plugin in [&server, &tui] {
        let text = fs::read_to_string(plugin).unwrap();
        assert!(
            text.contains(&format!("\nconst TAW_BIN = \"{}\";\n", taw.display())),
            "{}",
            plugin.display()
        );
    }
}

/// Whether `node` is here and new enough to register module hooks (20.6), as the OpenCode
/// plugin tests ask.
fn node_with_module_hooks() -> bool {
    let Ok(output) = Command::new("node").arg("--version").output() else {
        eprintln!("skipping: node is not on PATH");
        return false;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    let found = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if found < (20, 6) {
        eprintln!(
            "skipping: node {} cannot register module hooks",
            version.trim()
        );
        return false;
    }
    true
}

#[test]
fn the_opencode_plugin_an_install_wrote_runs_the_taw_it_names() {
    if !node_with_module_hooks() {
        return;
    }
    // A plain path is written from the home folder; one with the characters a template literal
    // reads specially is written in full, as a double-quoted string.
    for (relative, from_home) in [("my bin/taw", true), ("t`a${w}\\x \"q\"/taw", false)] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".config/opencode")).unwrap();
        let taw = m.home.join(relative);
        fake_taw_at(&taw);

        install("opencode", &taw).unwrap();

        let [_server, tui, _entry] = opencode_plugin_files(&m);
        let text = fs::read_to_string(&tui).unwrap();
        assert_eq!(
            text.contains("`${process.env.HOME}/my bin/taw`"),
            from_home,
            "{text}"
        );
        if !from_home {
            assert!(!text.contains("process.env.HOME}/t"), "{text}");
        }
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
        let output = Command::new("node")
            .arg("--import")
            .arg(fixtures.join("register-fake-child-process.mjs"))
            .arg(fixtures.join("tui-session-path.mjs"))
            .env("HOME", &m.home)
            .env("TAW_PLUGIN", &tui)
            .env("TAW_BIN_EXPECTED", &taw)
            .env_remove("TAW_REPORT_FILE")
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "{relative}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn the_other_javascript_plugins_and_the_hermes_plugin_name_taw_from_the_home_folder_too() {
    let _g = lock();
    let m = machine();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    for (agent, dir, file) in [
        (
            "kilo",
            ".config/kilo",
            ".config/kilo/plugin/taw-agent-state.js",
        ),
        ("pi", ".pi/agent", ".pi/agent/extensions/taw-agent-state.ts"),
        (
            "omp",
            ".omp/agent",
            ".omp/agent/extensions/taw-omp-agent-state.ts",
        ),
    ] {
        fs::create_dir_all(m.home.join(dir)).unwrap();
        install(agent, &taw).unwrap_or_else(|err| panic!("{agent}: {err:#}"));
        let text = fs::read_to_string(m.home.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(
            text.contains("\nconst TAW_BIN = `${process.env.HOME}/bin/taw`;\n"),
            "{agent}: {text}"
        );
    }

    fs::create_dir_all(m.home.join(".hermes")).unwrap();
    install("hermes", &taw).unwrap();
    let plugin = m.home.join(".hermes/plugins/taw-agent-state/__init__.py");
    let text = fs::read_to_string(&plugin).unwrap();
    assert!(
        text.contains("\n_TAW_BIN = os.path.join(os.path.expanduser(\"~\"), \"bin/taw\")\n"),
        "{text}"
    );
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipping the run: python3 is not on PATH");
        return;
    }
    // The plugin resolves the path it names.
    let output = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(
            "import importlib.util, os, sys\n\
             spec = importlib.util.spec_from_file_location('p', sys.argv[1])\n\
             mod = importlib.util.module_from_spec(spec)\n\
             spec.loader.exec_module(mod)\n\
             print(mod._TAW_BIN)\n",
        )
        .arg(&plugin)
        .env_clear()
        .env("HOME", &m.home)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        taw.to_string_lossy()
    );
}

// =====================================================================================
// Agents whose runner is not known to use a shell keep the absolute path
// =====================================================================================

#[test]
fn another_agents_hook_entry_keeps_the_absolute_path() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".codex")).unwrap();
    let _codex = EnvGuard::unset("CODEX_HOME");
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);

    install("codex", &taw).unwrap();

    let hooks = read_json(&m.home.join(".codex/hooks.json"));
    let command = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let script = m.home.join(".codex/taw-agent-state.sh");
    assert_eq!(command, format!("bash '{}' session", script.display()));
}

// =====================================================================================
// A hook is read as shell words
// =====================================================================================

#[test]
fn a_path_a_shell_would_not_expand_is_not_taws_and_install_adds_its_own_beside_it() {
    for spelling in [
        "bash '$HOME/.claude/hooks/taw-agent-state.sh' session",
        "bash '${HOME}/.claude/hooks/taw-agent-state.sh' session",
        "bash '~/.claude/hooks/taw-agent-state.sh' session",
        r#"bash "~/.claude/hooks/taw-agent-state.sh" session"#,
        r"bash \~/.claude/hooks/taw-agent-state.sh session",
        "echo 'see ~/.claude/hooks/taw-agent-state.sh'",
    ] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        let theirs = json!({"hooks": [{"type": "command", "command": spelling}]});
        let settings = m.home.join(".claude/settings.json");
        fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [theirs]}})).unwrap(),
        )
        .unwrap();

        install("claude", &taw).unwrap();

        assert_eq!(
            session_start_commands(&settings),
            [
                spelling.to_string(),
                r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#.to_string()
            ],
            "{spelling}"
        );
    }
}

// =====================================================================================
// A folder that is not there yet
// =====================================================================================

#[test]
fn an_install_with_no_hooks_folder_yet_over_a_dotfiles_entry_leaves_one_entry() {
    for spelling in [
        "bash ~/.claude/hooks/taw-agent-state.sh session",
        "bash $HOME/.claude/hooks/taw-agent-state.sh session",
        "bash ${HOME}/.claude/hooks/taw-agent-state.sh session",
        r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#,
    ] {
        let _g = lock();
        let m = machine();
        // A dotfiles checkout on a new machine: the settings are there, the hooks folder is not.
        fs::create_dir_all(m.home.join(".claude")).unwrap();
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        let mine = json!({"matcher": CURRENT_MATCHER, "hooks": [
            {"type": "command", "command": spelling, "timeout": 10}
        ]});
        let settings = m.home.join(".claude/settings.json");
        let seeded = json!({"hooks": {"SessionStart": [mine]}});
        fs::write(&settings, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();
        assert!(!m.home.join(".claude/hooks").exists());

        install("claude", &taw).unwrap();

        assert_eq!(read_json(&settings), seeded, "{spelling}");
        assert!(m.home.join(".claude/hooks/taw-agent-state.sh").exists());

        uninstall("claude").unwrap();

        assert!(
            !settings.exists() || read_json(&settings)["hooks"].is_null(),
            "{spelling}"
        );
    }
}

// =====================================================================================
// A reinstall still refreshes taw's own entry
// =====================================================================================

#[test]
fn a_reinstall_gives_a_home_spelled_entry_the_current_matcher_and_timeout_and_keeps_its_path() {
    for spelling in [
        "bash ~/.claude/hooks/taw-agent-state.sh session",
        r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#,
        "bash ${HOME}/.claude/hooks/taw-agent-state.sh session",
    ] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        let old = json!({"matcher": "*", "hooks": [
            {"type": "command", "command": spelling, "timeout": 5}
        ]});
        let other = json!({"matcher": "*", "hooks": [{"type": "command", "command": "echo hi"}]});
        let settings = m.home.join(".claude/settings.json");
        fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [old, other]}}))
                .unwrap(),
        )
        .unwrap();

        install("claude", &taw).unwrap();

        // Refreshed where it stood, with its path spelled as it was found.
        let fresh = json!({"matcher": CURRENT_MATCHER, "hooks": [
            {"type": "command", "command": spelling, "timeout": 10}
        ]});
        assert_eq!(
            read_json(&settings)["hooks"]["SessionStart"],
            json!([fresh, other]),
            "{spelling}"
        );
        assert_eq!(
            serde_json::to_string(&read_json(&settings)["hooks"]["SessionStart"][0]).unwrap(),
            serde_json::to_string(&fresh).unwrap(),
            "{spelling}"
        );
    }
}

#[test]
fn a_reinstall_of_taws_own_entry_under_the_old_matcher_refreshes_it() {
    let _g = lock();
    let m = machine();
    fs::create_dir_all(m.home.join(".claude/hooks")).unwrap();
    let taw = m.home.join("bin/taw");
    fake_taw_at(&taw);
    let word = r#"bash "$HOME/.claude/hooks/taw-agent-state.sh" session"#;
    let old =
        json!({"matcher": "*", "hooks": [{"type": "command", "command": word, "timeout": 5}]});
    let settings = m.home.join(".claude/settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [old]}})).unwrap(),
    )
    .unwrap();

    install("claude", &taw).unwrap();

    assert_eq!(
        read_json(&settings)["hooks"]["SessionStart"],
        json!([{"matcher": CURRENT_MATCHER, "hooks": [
            {"type": "command", "command": word, "timeout": 10}
        ]}])
    );
}

// =====================================================================================
// A config folder or a taw path with characters a shell reads specially
// =====================================================================================

#[test]
fn a_config_folder_under_home_with_shell_characters_installs_once_in_full_and_uninstalls() {
    for name in [
        "cfg $x",
        "cfg `x`",
        "cfg \"q\"",
        "cfg \\d",
        "cfg it's",
        "cfg ${HOME}",
    ] {
        let _g = lock();
        let m = machine();
        let config = m.home.join(name);
        fs::create_dir_all(&config).unwrap();
        let _config = EnvGuard::set("CLAUDE_CONFIG_DIR", &config);
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        let settings = config.join("settings.json");
        let script = config.join("hooks/taw-agent-state.sh");

        install("claude", &taw).unwrap();
        install("claude", &taw).unwrap();

        let commands = session_start_commands(&settings);
        assert_eq!(commands.len(), 1, "{name}: {commands:?}");
        assert!(
            commands[0].starts_with("bash '") && commands[0].ends_with("' session"),
            "{name}: {commands:?}"
        );
        assert!(commands[0].contains(&*m.home.to_string_lossy()), "{name}");
        // The command runs the script it names.
        let record = m.outside.join("record.txt");
        assert_reported_session(&run_hook(&["bash", "-c", &commands[0]], &m.home, &record));

        uninstall("claude").unwrap();

        assert!(!script.exists(), "{name}");
        assert!(
            !settings.exists() || read_json(&settings)["hooks"].is_null(),
            "{name}"
        );
    }
}

// =====================================================================================
// Only an agent whose hook runner is a shell reads ~ and $HOME
// =====================================================================================

#[test]
fn another_agents_entry_spelled_from_the_home_folder_is_not_known_and_install_adds_its_own() {
    for spelling in [
        "bash ~/.codex/taw-agent-state.sh session",
        "bash $HOME/.codex/taw-agent-state.sh session",
    ] {
        let _g = lock();
        let m = machine();
        fs::create_dir_all(m.home.join(".codex")).unwrap();
        let _codex = EnvGuard::unset("CODEX_HOME");
        let taw = m.home.join("bin/taw");
        fake_taw_at(&taw);
        let theirs = json!({"hooks": [{"type": "command", "command": spelling}]});
        let hooks = m.home.join(".codex/hooks.json");
        fs::write(
            &hooks,
            serde_json::to_string_pretty(&json!({"hooks": {"SessionStart": [theirs]}})).unwrap(),
        )
        .unwrap();

        install("codex", &taw).unwrap();

        let commands = session_start_commands(&hooks);
        let script = m.home.join(".codex/taw-agent-state.sh");
        assert_eq!(
            commands,
            [
                spelling.to_string(),
                format!("bash '{}' session", script.display())
            ],
            "{spelling}"
        );
    }
}
