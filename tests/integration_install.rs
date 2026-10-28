// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Tests for `taw::integration` — the ported herdr hook installers.
//!
//! Every test that touches `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `COPILOT_HOME` /
//! `XDG_CONFIG_HOME` / `PATH` holds `ENV_LOCK` for its duration: `cargo test` runs tests in
//! parallel threads by default, and `std::env` is shared, unsynchronized, process-global
//! state — without the lock, two tests setting different temp-dir values for the same
//! variable would race.

#[path = "common/exec.rs"]
mod exec;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use taw::integration::{AGENTS, install, status, status_all, uninstall};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets an env var for the guard's lifetime, restoring (or removing) the prior value on drop.
struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: caller holds `ENV_LOCK` for the guard's whole lifetime, so no other
        // thread in this test binary observes env state mid-mutation.
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

/// Prepends `dir` to `PATH` for the guard's lifetime. Not required by taw's own scripts
/// (they call an absolute `{{TAW_BIN}}` baked in at install time, never a `PATH` lookup),
/// but the fake `taw` fixture is also described as being "on PATH" — this guard makes
/// that literally true too, belt-and-suspenders.
struct PathPrependGuard {
    prev: Option<OsString>,
}

impl PathPrependGuard {
    fn new(dir: &Path) -> Self {
        let prev = std::env::var_os("PATH");
        let mut new_path = OsString::from(dir);
        if let Some(p) = &prev {
            new_path.push(":");
            new_path.push(p);
        }
        // SAFETY: see `EnvGuard::set`.
        unsafe { std::env::set_var("PATH", &new_path) };
        Self { prev }
    }
}

impl Drop for PathPrependGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::set`.
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// Writes an executable fake `taw` into `dir` that, on every invocation, truncates and
/// rewrites `$FAKE_TAW_RECORD` with one argv element per line (so a test can read back
/// exactly what it was called with).
fn write_fake_taw(dir: &Path) -> PathBuf {
    let path = dir.join("taw");
    let script = "#!/bin/sh\n\
out=\"${FAKE_TAW_RECORD:-}\"\n\
[ -n \"$out\" ] || exit 0\n\
: > \"$out\"\n\
for a in \"$@\"; do\n\
    printf '%s\\n' \"$a\" >> \"$out\"\n\
done\n";
    fs::write(&path, script).expect("write fake taw");
    make_executable(&path);
    path
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        crate::exec::make_executable(path);
    }
}

/// Runs a rendered hook script with `stdin` on its stdin, `TMUX_PANE` set to `tmux_pane`
/// (removed entirely when `None`), `FAKE_TAW_RECORD` pointed at `record`, plus any
/// `extra_env`, and `extra_args` appended positionally (e.g. `["session"]`). Every taw
/// script must exit 0 by design, so this asserts success and returns just for callers that
/// want to inspect it further.
#[allow(clippy::too_many_arguments)]
fn run_script(
    script: &Path,
    tmux_pane: Option<&str>,
    stdin: &[u8],
    record: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::ExitStatus {
    let mut cmd = Command::new("sh");
    cmd.arg(script);
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.env("FAKE_TAW_RECORD", record);
    match tmux_pane {
        Some(p) => {
            cmd.env("TMUX_PANE", p);
        }
        None => {
            cmd.env_remove("TMUX_PANE");
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn script");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for script");
    assert!(
        output.status.success(),
        "script exited non-zero: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.status
}

/// Parses a fake-taw record file (one argv element per line) into a `Vec<String>`. Returns
/// an empty `Vec` if the file doesn't exist (taw was never invoked).
fn read_record(path: &Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(s) => s.lines().map(str::to_string).collect(),
        Err(_) => Vec::new(),
    }
}

/// Asserts `argv` contains flag `name` immediately followed by `value`.
fn assert_flag(argv: &[String], name: &str, value: &str) {
    let pos = argv
        .iter()
        .position(|a| a == name)
        .unwrap_or_else(|| panic!("missing flag {name} in {argv:?}"));
    assert_eq!(
        argv.get(pos + 1).map(String::as_str),
        Some(value),
        "flag {name} in {argv:?}"
    );
}

/// Asserts `argv` contains a `--seq` flag with a numeric value (its exact value is
/// nondeterministic — current-time nanoseconds — so this checks shape, not the number).
fn assert_seq_present(argv: &[String]) {
    let pos = argv
        .iter()
        .position(|a| a == "--seq")
        .unwrap_or_else(|| panic!("missing --seq in {argv:?}"));
    let value = argv
        .get(pos + 1)
        .unwrap_or_else(|| panic!("--seq has no value in {argv:?}"));
    value
        .parse::<u128>()
        .unwrap_or_else(|_| panic!("--seq value {value:?} is not numeric"));
}

fn bash_n(script: &Path) {
    let output = Command::new("bash")
        .arg("-n")
        .arg(script)
        .output()
        .expect("run bash -n");
    assert!(
        output.status.success(),
        "bash -n {} failed: {}",
        script.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

// =====================================================================================
// claude
// =====================================================================================

const CLAUDE_SETTINGS_FIXTURE: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "bash '/opt/other-tool/hook.sh'",
            "timeout": 5
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "echo pre",
            "timeout": 3
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "echo post",
            "timeout": 3
          }
        ]
      }
    ]
  }
}
"#;

const CLAUDE_SESSION_START_JSON: &str = r#"{"session_id":"abc","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","source":"startup"}"#;

fn claude_script_path(home: &Path) -> PathBuf {
    home.join("hooks").join("taw-agent-state.sh")
}

#[test]
fn claude_install_writes_files_with_version_marker() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let report = install("claude", &taw_bin).expect("install");

    let script = claude_script_path(home.path());
    assert!(script.is_file());
    let contents = fs::read_to_string(&script).unwrap();
    assert!(contents.contains("# TAW_INTEGRATION_VERSION=5"));

    let settings = home.path().join("settings.json");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "settings.json must contain exactly one added entry"
    );
    assert!(
        entries[0]
            .to_string()
            .contains(&script.to_string_lossy().to_string())
    );

    assert_eq!(report.agent, "claude");
    assert_eq!(report.files, vec![script, settings]);
}

/// The copies an install kept beside its hook of the configs it edited: every file under `dir`
/// with `.orig` in its name.
#[cfg(unix)]
fn copies_of_configs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(copies_of_configs(&path));
        } else if path.to_string_lossy().contains(".orig") {
            found.push(path);
        }
    }
    found
}

/// A config only its owner may read can hold tokens, and so can the copy kept of it for the
/// uninstall: it is created with the config's own mode, for a JSON hook file (Claude's) and a
/// TOML one (Codex's) alike.
#[cfg(unix)]
#[test]
fn the_copy_kept_of_a_private_config_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let _g = lock();
    for (agent, var, files) in [
        ("claude", "CLAUDE_CONFIG_DIR", ["settings.json"].as_slice()),
        (
            "codex",
            "CODEX_HOME",
            ["hooks.json", "config.toml"].as_slice(),
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(var, home.path());
        for name in files {
            let config = home.path().join(name);
            fs::write(&config, if name.ends_with("json") { "{}\n" } else { "" }).unwrap();
            fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        }

        install(agent, &home.path().join("taw-bin-placeholder")).expect("install");

        let copies = copies_of_configs(home.path());
        assert_eq!(copies.len(), files.len(), "{agent}: {copies:?}");
        for copy in copies {
            let mode = fs::metadata(&copy).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{agent}: {}", copy.display());
        }
    }
}

#[test]
fn claude_status_before_install_during_and_after_uninstall() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let before = status("claude").expect("status");
    assert!(!before.installed);
    assert_eq!(before.version, None);

    install("claude", &taw_bin).expect("install");
    let during = status("claude").expect("status");
    assert!(during.installed);
    assert_eq!(during.version, Some(5));
    assert_eq!(
        during.current,
        Some(5),
        "what an install writes is what this build writes"
    );
    assert!(!during.outdated(), "a fresh install is not outdated");
    assert_eq!(during.path, claude_script_path(home.path()));

    uninstall("claude").expect("uninstall");
    let after = status("claude").expect("status");
    assert!(!after.installed);
}

#[test]
fn claude_uninstall_reverts_settings_json_byte_identical() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    let original_bytes = fs::read(&settings).unwrap();

    install("claude", &taw_bin).expect("install");

    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["SessionStart"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        installed["hooks"]["PreToolUse"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        installed["hooks"]["PostToolUse"].as_array().unwrap().len(),
        1
    );

    uninstall("claude").expect("uninstall");

    let reverted_bytes = fs::read(&settings).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "settings.json must revert byte-for-byte"
    );
    assert!(!claude_script_path(home.path()).exists());
}

/// A pre-install file whose `SessionStart` container is already present but *empty* must
/// survive an install/uninstall round-trip byte-for-byte. Stripping our own entry back out
/// empties the array, which the pruner removes; if the sidecar comparison did not prune the
/// same way, uninstall wrongly decided the file had changed and rewrote it canonically —
/// deleting the user's own (empty) `hooks` key and every other formatting choice.
#[test]
fn claude_uninstall_preserves_a_user_owned_empty_hooks_container() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let settings = home.path().join("settings.json");
    // The user's own empty `SessionStart` array, plus an unrelated key and deliberately
    // non-canonical spacing, so the check is genuinely byte-for-byte.
    let original = "{\n  \"model\": \"opus\",\n  \"hooks\": { \"SessionStart\": [] }\n}\n";
    fs::write(&settings, original).unwrap();
    let original_bytes = fs::read(&settings).unwrap();

    install("claude", &taw_bin).expect("install");
    // Our entry really did land in the user's container.
    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["SessionStart"].as_array().unwrap().len(),
        1
    );

    uninstall("claude").expect("uninstall");

    let reverted_bytes = fs::read(&settings).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "the user's empty hooks container (and formatting) must survive verbatim"
    );
}

#[test]
fn claude_rendered_script_passes_bash_n() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    install("claude", &home.path().join("taw-bin-placeholder")).unwrap();
    bash_n(&claude_script_path(home.path()));
}

#[test]
fn claude_script_exits_zero_immediately_without_tmux_pane() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    install("claude", &home.path().join("taw-bin-placeholder")).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        None,
        CLAUDE_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(
        !record.exists(),
        "taw must not be invoked when TMUX_PANE is unset"
    );
}

#[test]
fn claude_session_start_invokes_report_agent_session_with_expected_argv() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());
    let _env_path = PathPrependGuard::new(bin_dir.path());

    install("claude", &fake_taw).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        Some("%1"),
        CLAUDE_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    let argv = read_record(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%1"]);
    assert_flag(&argv, "--source", "integration:claude");
    assert_flag(&argv, "--agent", "claude");
    assert_flag(&argv, "--agent-session-id", "abc");
    assert_flag(&argv, "--agent-session-path", "/t.jsonl");
    assert_flag(&argv, "--session-start-source", "startup");
    assert_seq_present(&argv);
}

#[test]
fn claude_subagent_event_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("claude", &fake_taw).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    let payload = r#"{"session_id":"abc","hook_event_name":"SessionStart","agent_id":"sub"}"#;
    run_script(
        &script,
        Some("%1"),
        payload.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(!record.exists(), "a subagent event must never invoke taw");
}

/// herdr's `CURSOR_VERSION` guard (v0.9.0): Cursor embeds Claude Code, and a run under
/// Cursor must never get reported as a plain claude session.
#[test]
fn claude_cursor_embedded_env_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("claude", &fake_taw).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        Some("%1"),
        CLAUDE_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[("CURSOR_VERSION", "1")],
    );

    assert!(
        !record.exists(),
        "a CURSOR_VERSION-embedded session must never invoke taw"
    );
}

/// Follows herdr's `install_scopes_claude_session_start_sources`: Grok imports
/// Claude's hooks but reports `new`/`load` rather than Claude's own `SessionStart`
/// sources, so the installed matcher must accept exactly Claude's five and nothing else —
/// including a source that merely starts with one of them.
#[test]
fn claude_install_scopes_session_start_to_documented_sources() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());

    install("claude", &home.path().join("taw-bin-placeholder")).unwrap();

    let settings = home.path().join("settings.json");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let matcher = value["hooks"]["SessionStart"][0]["matcher"]
        .as_str()
        .unwrap();
    assert_eq!(matcher, "^(startup|resume|clear|compact|fork)$");

    let pattern = regex::Regex::new(matcher).unwrap();
    for source in ["startup", "resume", "clear", "compact", "fork"] {
        assert!(pattern.is_match(source), "Claude source: {source}");
    }
    for source in ["new", "load", "", "future-source", "startup-extra"] {
        assert!(!pattern.is_match(source), "non-Claude source: {source}");
    }
}

/// The separate-group half of herdr's `install_migrates_wildcard_session_start_and_
/// preserves_user_hook` (the shared-group half is the next two tests). Simulates upgrading
/// taw over an install made before this task: settings.json already carries taw's old
/// `"matcher": "*"` entry plus a genuinely separate hook another tool owns, and the
/// sidecar (written by that earlier install) holds the true pre-taw original, just the
/// other tool's hook. Migrating must end up with the other tool's entry untouched and
/// taw's own replaced by the narrowed-matcher entry, not duplicated, and uninstalling
/// afterwards must still get back to the true original, not to the intermediate
/// wildcard-matcher state.
#[test]
fn claude_install_migrates_a_wildcard_entry_while_preserving_a_sibling_hook() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");
    let script = claude_script_path(home.path());
    fs::create_dir_all(script.parent().unwrap()).unwrap();

    let sibling_only = r#"{"hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"echo keep","timeout":3}]}]}}"#;
    // The sidecar an earlier (pre-this-task) install would have written: the file as it
    // was *before taw ever touched it* — the sibling hook, nothing of taw's.
    let mut sidecar = script.as_os_str().to_owned();
    sidecar.push(".orig.json");
    fs::write(&sidecar, sibling_only).unwrap();
    // The file as that earlier install left it: taw's old wildcard entry alongside the
    // sibling hook.
    let settings = home.path().join("settings.json");
    fs::write(
        &settings,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"matcher":"*","hooks":[{{"type":"command","command":"bash '{}' session","timeout":10}}]}},{{"matcher":"*","hooks":[{{"type":"command","command":"echo keep","timeout":3}}]}}]}}}}"#,
            script.display()
        ),
    )
    .unwrap();

    install("claude", &taw_bin).unwrap();
    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let entries = installed["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        2,
        "the sibling hook must not be merged into taw's own entry"
    );
    let (ours, theirs) = if entries[0]["hooks"][0]["command"] == "echo keep" {
        (&entries[1], &entries[0])
    } else {
        (&entries[0], &entries[1])
    };
    assert_eq!(theirs["matcher"], "*");
    assert_eq!(theirs["hooks"][0]["command"], "echo keep");
    assert_eq!(ours["matcher"], "^(startup|resume|clear|compact|fork)$");

    // Idempotent: installing again over the already-migrated entry changes nothing.
    let after_first = fs::read_to_string(&settings).unwrap();
    install("claude", &taw_bin).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), after_first);

    uninstall("claude").unwrap();
    let remaining: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let remaining = remaining["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["hooks"][0]["command"], "echo keep");
}

/// The shared-group half of herdr's `install_migrates_wildcard_session_start_and_
/// preserves_user_hook`: taw's old `"matcher": "*"` command and a hook somebody else owns
/// sit in one group, which is where a settings.json ends up when a tool (or the user) adds
/// its hook to the group it finds. Reinstalling has to take out taw's command and nothing
/// else, give the fresh entry a group of its own and leave the other hook where it was;
/// uninstalling has to leave it there too.
#[test]
fn claude_reinstall_and_uninstall_keep_a_user_hook_in_taws_old_group() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");
    let script = claude_script_path(home.path());
    let taw_command = format!("bash '{}' session", script.display());
    let settings = home.path().join("settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"SessionStart": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": taw_command, "timeout": 10},
                    {"type": "command", "command": "echo keep", "timeout": 3},
                ]
            }]}
        }))
        .unwrap(),
    )
    .unwrap();

    install("claude", &taw_bin).unwrap();
    let installed = fs::read_to_string(&settings).unwrap();
    let value: serde_json::Value = serde_json::from_str(&installed).unwrap();
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "taw's command gets a group of its own");
    assert_eq!(groups[0]["matcher"], "*");
    assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
    assert_eq!(
        groups[1]["matcher"],
        "^(startup|resume|clear|compact|fork)$"
    );
    assert_eq!(groups[1]["hooks"][0]["command"], taw_command.as_str());

    install("claude", &taw_bin).unwrap();
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        installed,
        "reinstalling over the migrated file changes nothing"
    );

    uninstall("claude").unwrap();
    let text = fs::read_to_string(&settings).unwrap();
    assert!(
        !text.contains(script.to_string_lossy().as_ref()),
        "taw's command must be gone: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["matcher"], "*");
    assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo keep");
    assert!(!script.exists());
}

/// The same shared group, reached the way an upgrade reaches it: the earlier install left
/// its backup (the file as it was before taw first touched it, holding one hook), and
/// somebody added a second hook to taw's group afterwards. Uninstalling must keep both the
/// hook that was there before taw and the one added after it, and take only taw's out.
#[test]
fn claude_uninstall_keeps_hooks_added_to_taws_group_after_install() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");
    let script = claude_script_path(home.path());
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    let taw_command = format!("bash '{}' session", script.display());

    let mut sidecar = script.as_os_str().to_owned();
    sidecar.push(".orig.json");
    fs::write(
        &sidecar,
        r#"{"hooks":{"SessionStart":[{"matcher":"*","hooks":[{"type":"command","command":"echo before","timeout":3}]}]}}"#,
    )
    .unwrap();
    let settings = home.path().join("settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"SessionStart": [{
                "matcher": "*",
                "hooks": [
                    {"type": "command", "command": taw_command, "timeout": 10},
                    {"type": "command", "command": "echo before", "timeout": 3},
                    {"type": "command", "command": "echo after", "timeout": 3},
                ]
            }]}
        }))
        .unwrap(),
    )
    .unwrap();

    install("claude", &taw_bin).unwrap();
    uninstall("claude").unwrap();

    let text = fs::read_to_string(&settings).unwrap();
    assert!(
        !text.contains(script.to_string_lossy().as_ref()),
        "taw's command must be gone: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let commands: Vec<&str> = groups[0]["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands, ["echo before", "echo after"]);
}

/// The matcher narrows *which* Claude-documented sources invoke the hook at all, but Grok
/// (or anything else reading Claude's settings.json without honouring `matcher`) could
/// still run this script directly under an event that is not `SessionStart` at all — the
/// belt-and-suspenders the script's own guard exists for. Besides `agent_id` (a subagent),
/// every non-`SessionStart` event must be excluded.
#[test]
fn claude_non_session_start_event_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("claude", &fake_taw).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    let payload = r#"{"session_id":"abc","hook_event_name":"Stop"}"#;
    run_script(
        &script,
        Some("%1"),
        payload.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(
        !record.exists(),
        "a non-SessionStart event must never invoke taw, even without agent_id"
    );
}

/// The same guard, triggered through the hook payload's `cursor_version` field instead of
/// the environment — herdr checks both.
#[test]
fn claude_cursor_embedded_payload_field_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("claude", &fake_taw).unwrap();
    let script = claude_script_path(home.path());
    let record = home.path().join("record.txt");

    let payload =
        r#"{"session_id":"abc","hook_event_name":"SessionStart","cursor_version":"1.2.3"}"#;
    run_script(
        &script,
        Some("%1"),
        payload.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(
        !record.exists(),
        "a cursor_version hook payload must never invoke taw"
    );
}

// =====================================================================================
// codex
// =====================================================================================

const CODEX_HOOKS_JSON_FIXTURE: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "bash '/opt/other/hook.sh'",
            "timeout": 5,
            "type": "command"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "command": "echo end1",
            "timeout": 3,
            "type": "command"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "command": "echo prompt1",
            "timeout": 3,
            "type": "command"
          }
        ]
      }
    ]
  }
}
"#;

const CODEX_CONFIG_TOML_FIXTURE: &str =
    "[model]\nname = \"gpt-5-codex\"\n\n[other_tool]\nenabled = true\n";

const CODEX_SESSION_START_JSON: &str = r#"{"session_id":"c-abc","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","source":"startup"}"#;

fn codex_script_path(home: &Path) -> PathBuf {
    home.join("taw-agent-state.sh")
}

#[test]
fn codex_install_writes_files_with_version_marker() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let report = install("codex", &taw_bin).expect("install");

    let script = codex_script_path(home.path());
    assert!(script.is_file());
    let contents = fs::read_to_string(&script).unwrap();
    assert!(contents.contains("# TAW_INTEGRATION_VERSION=3"));

    let hooks_json = home.path().join("hooks.json");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks_json).unwrap()).unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "hooks.json must contain exactly one added entry"
    );

    let config_toml = home.path().join("config.toml");
    let toml_contents = fs::read_to_string(&config_toml).unwrap();
    assert!(toml_contents.contains("[features]"));
    assert!(toml_contents.contains("hooks = true"));

    assert_eq!(report.agent, "codex");
    assert_eq!(report.files, vec![script, hooks_json, config_toml]);
}

#[test]
fn codex_status_before_install_during_and_after_uninstall() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let before = status("codex").expect("status");
    assert!(!before.installed);

    install("codex", &taw_bin).expect("install");
    let during = status("codex").expect("status");
    assert!(during.installed);
    assert_eq!(during.version, Some(3));
    assert_eq!(
        during.current,
        Some(3),
        "what an install writes is what this build writes"
    );
    assert!(!during.outdated(), "a fresh install is not outdated");
    assert_eq!(during.path, codex_script_path(home.path()));

    uninstall("codex").expect("uninstall");
    let after = status("codex").expect("status");
    assert!(!after.installed);
}

#[test]
fn codex_uninstall_reverts_hooks_json_byte_identical() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let hooks_json = home.path().join("hooks.json");
    fs::write(&hooks_json, CODEX_HOOKS_JSON_FIXTURE).unwrap();
    let original_bytes = fs::read(&hooks_json).unwrap();

    install("codex", &taw_bin).expect("install");
    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks_json).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["SessionStart"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        installed["hooks"]["SessionEnd"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        installed["hooks"]["UserPromptSubmit"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    uninstall("codex").expect("uninstall");
    let reverted_bytes = fs::read(&hooks_json).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "hooks.json must revert byte-for-byte"
    );
}

#[test]
fn codex_uninstall_reverts_config_toml_byte_identical() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let config_toml = home.path().join("config.toml");
    fs::write(&config_toml, CODEX_CONFIG_TOML_FIXTURE).unwrap();
    let original_bytes = fs::read(&config_toml).unwrap();

    install("codex", &taw_bin).expect("install");
    let installed = fs::read_to_string(&config_toml).unwrap();
    assert!(installed.starts_with(CODEX_CONFIG_TOML_FIXTURE.trim_end_matches('\n')));
    // Marked lines, not a delimited block: Codex saves its own switches into the table it finds.
    assert!(
        !installed.contains("# >>> taw integration >>>"),
        "{installed}"
    );
    assert!(
        installed.contains("[features]  # taw integration"),
        "{installed}"
    );
    assert!(
        installed.contains("hooks = true  # taw integration"),
        "{installed}"
    );

    uninstall("codex").expect("uninstall");
    let reverted_bytes = fs::read(&config_toml).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "config.toml must revert byte-for-byte"
    );
}

#[test]
fn codex_rendered_script_passes_bash_n() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    install("codex", &home.path().join("taw-bin-placeholder")).unwrap();
    bash_n(&codex_script_path(home.path()));
}

#[test]
fn codex_script_exits_zero_immediately_without_tmux_pane() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    install("codex", &home.path().join("taw-bin-placeholder")).unwrap();
    let script = codex_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        None,
        CODEX_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(
        !record.exists(),
        "taw must not be invoked when TMUX_PANE is unset"
    );
}

#[test]
fn codex_session_start_invokes_report_agent_session_with_expected_argv() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CODEX_HOME", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());
    let _env_path = PathPrependGuard::new(bin_dir.path());

    install("codex", &fake_taw).unwrap();
    let script = codex_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        Some("%2"),
        CODEX_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    let argv = read_record(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%2"]);
    assert_flag(&argv, "--source", "integration:codex");
    assert_flag(&argv, "--agent", "codex");
    assert_flag(&argv, "--agent-session-id", "c-abc");
    assert_flag(&argv, "--session-start-source", "startup");
    assert_seq_present(&argv);
    assert!(
        !argv.contains(&"--agent-session-path".to_string()),
        "codex does not forward transcript_path (matches herdr)"
    );
}

#[test]
fn codex_subagent_event_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CODEX_HOME", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("codex", &fake_taw).unwrap();
    let script = codex_script_path(home.path());
    let record = home.path().join("record.txt");

    let payload = r#"{"session_id":"c-abc","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","agent_id":"sub"}"#;
    run_script(
        &script,
        Some("%2"),
        payload.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    assert!(!record.exists(), "a subagent event must never invoke taw");
}

#[test]
fn codex_mismatched_thread_id_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("CODEX_HOME", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("codex", &fake_taw).unwrap();
    let script = codex_script_path(home.path());
    let record = home.path().join("record.txt");

    // CODEX_THREAD_ID names a different thread than the hook payload's session_id: herdr's
    // codex script treats this as a resumed/forked thread and does not report it.
    run_script(
        &script,
        Some("%2"),
        CODEX_SESSION_START_JSON.as_bytes(),
        &record,
        &["session"],
        &[("CODEX_THREAD_ID", "some-other-thread")],
    );

    assert!(
        !record.exists(),
        "a mismatched CODEX_THREAD_ID must suppress the report"
    );
}

// =====================================================================================
// copilot
// =====================================================================================

const COPILOT_SETTINGS_FIXTURE: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "bash": "bash '/opt/other/hook.sh'",
        "timeoutSec": 5,
        "type": "command"
      }
    ],
    "SessionEnd": [
      {
        "bash": "echo end1",
        "timeoutSec": 3,
        "type": "command"
      }
    ],
    "Notification": [
      {
        "bash": "echo notif1",
        "timeoutSec": 3,
        "type": "command"
      }
    ]
  }
}
"#;

const COPILOT_SESSION_START_JSON: &str =
    r#"{"session_id":"cp-abc","hook_event_name":"SessionStart"}"#;

fn copilot_script_path(home: &Path) -> PathBuf {
    home.join("hooks").join("taw-agent-state.sh")
}

#[test]
fn copilot_install_writes_files_with_version_marker() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let report = install("copilot", &taw_bin).expect("install");

    let script = copilot_script_path(home.path());
    assert!(script.is_file());
    let contents = fs::read_to_string(&script).unwrap();
    assert!(contents.contains("# TAW_INTEGRATION_VERSION=2"));

    let settings = home.path().join("settings.json");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "settings.json must contain exactly one added entry"
    );

    assert_eq!(report.agent, "copilot");
    assert_eq!(report.files, vec![script, settings]);
}

#[test]
fn copilot_status_before_install_during_and_after_uninstall() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let before = status("copilot").expect("status");
    assert!(!before.installed);

    install("copilot", &taw_bin).expect("install");
    let during = status("copilot").expect("status");
    assert!(during.installed);
    assert_eq!(during.version, Some(2));
    assert_eq!(
        during.current,
        Some(2),
        "what an install writes is what this build writes"
    );
    assert!(!during.outdated(), "a fresh install is not outdated");

    uninstall("copilot").expect("uninstall");
    let after = status("copilot").expect("status");
    assert!(!after.installed);
}

#[test]
fn copilot_uninstall_reverts_settings_json_byte_identical() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let settings = home.path().join("settings.json");
    fs::write(&settings, COPILOT_SETTINGS_FIXTURE).unwrap();
    let original_bytes = fs::read(&settings).unwrap();

    install("copilot", &taw_bin).expect("install");
    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["SessionStart"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        installed["hooks"]["SessionEnd"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        installed["hooks"]["Notification"].as_array().unwrap().len(),
        1
    );

    uninstall("copilot").expect("uninstall");
    let reverted_bytes = fs::read(&settings).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "settings.json must revert byte-for-byte"
    );
}

#[test]
fn copilot_rendered_script_passes_bash_n() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    install("copilot", &home.path().join("taw-bin-placeholder")).unwrap();
    bash_n(&copilot_script_path(home.path()));
}

#[test]
fn copilot_script_exits_zero_immediately_without_tmux_pane() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    install("copilot", &home.path().join("taw-bin-placeholder")).unwrap();
    let script = copilot_script_path(home.path());
    let record = home.path().join("record.txt");

    run_script(
        &script,
        None,
        COPILOT_SESSION_START_JSON.as_bytes(),
        &record,
        &[],
        &[],
    );

    assert!(
        !record.exists(),
        "taw must not be invoked when TMUX_PANE is unset"
    );
}

#[test]
fn copilot_session_start_invokes_report_agent_session_with_expected_argv() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("COPILOT_HOME", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());
    let _env_path = PathPrependGuard::new(bin_dir.path());

    install("copilot", &fake_taw).unwrap();
    let script = copilot_script_path(home.path());
    let record = home.path().join("record.txt");

    // copilot's script takes no positional action argument (unlike claude/codex).
    run_script(
        &script,
        Some("%3"),
        COPILOT_SESSION_START_JSON.as_bytes(),
        &record,
        &[],
        &[],
    );

    let argv = read_record(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%3"]);
    assert_flag(&argv, "--source", "integration:copilot");
    assert_flag(&argv, "--agent", "copilot");
    assert_flag(&argv, "--agent-session-id", "cp-abc");
    assert_seq_present(&argv);
}

#[test]
fn copilot_subagent_event_invokes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env_home = EnvGuard::set("COPILOT_HOME", home.path());
    let bin_dir = tempfile::tempdir().unwrap();
    let fake_taw = write_fake_taw(bin_dir.path());

    install("copilot", &fake_taw).unwrap();
    let script = copilot_script_path(home.path());
    let record = home.path().join("record.txt");

    let payload = r#"{"session_id":"cp-abc","hook_event_name":"SessionStart","agent_id":"sub"}"#;
    run_script(&script, Some("%3"), payload.as_bytes(), &record, &[], &[]);

    assert!(!record.exists(), "a subagent event must never invoke taw");
}

// =====================================================================================
// opencode
// =====================================================================================

fn opencode_plugin_path(xdg_config_home: &Path) -> PathBuf {
    xdg_config_home
        .join("opencode")
        .join("plugins")
        .join("taw-agent-state.js")
}

#[test]
fn opencode_install_writes_plugin_with_version_marker() {
    let _g = lock();
    let xdg = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
    // The install looks in OpenCode's state directory too, so it is pointed at the tempdir
    // and not at whatever the machine running this has.
    let _state = EnvGuard::set("XDG_STATE_HOME", xdg.path());
    // OpenCode's own config dir must already exist (it's created by OpenCode itself) —
    // this simulates OpenCode already being installed.
    fs::create_dir_all(xdg.path().join("opencode")).unwrap();
    let taw_bin = xdg.path().join("taw-bin-placeholder");

    let report = install("opencode", &taw_bin).expect("install");

    let plugin = opencode_plugin_path(xdg.path());
    assert!(plugin.is_file());
    let contents = fs::read_to_string(&plugin).unwrap();
    assert!(contents.contains("// TAW_INTEGRATION_VERSION=5"));
    assert!(
        !contents.contains("{{TAW_BIN}}"),
        "template placeholder must be substituted"
    );
    assert!(
        !contents.contains("{{AGENT}}"),
        "template placeholder must be substituted"
    );
    assert!(contents.contains(&taw_bin.display().to_string()));
    assert!(contents.contains("session.created"));
    assert!(contents.contains("session.idle"));
    assert!(contents.contains("permission.asked"));
    assert!(contents.contains("TMUX_PANE"));

    assert_eq!(report.agent, "opencode");
    // The server plugin is the first of the files, and the others (the TUI plugin, the
    // version 2 entry, and the configs that register them) are `integration_opencode_install.rs`'s.
    assert_eq!(report.files.first(), Some(&plugin));
}

#[test]
fn opencode_status_before_install_during_and_after_uninstall() {
    let _g = lock();
    let xdg = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
    // The install looks in OpenCode's state directory too, so it is pointed at the tempdir
    // and not at whatever the machine running this has.
    let _state = EnvGuard::set("XDG_STATE_HOME", xdg.path());
    // OpenCode's own config dir must already exist (it's created by OpenCode itself) —
    // this simulates OpenCode already being installed.
    fs::create_dir_all(xdg.path().join("opencode")).unwrap();
    let taw_bin = xdg.path().join("taw-bin-placeholder");

    let before = status("opencode").expect("status");
    assert!(!before.installed);

    install("opencode", &taw_bin).expect("install");
    let during = status("opencode").expect("status");
    assert!(during.installed);
    assert_eq!(during.version, Some(5));
    assert_eq!(
        during.current,
        Some(5),
        "what an install writes is what this build writes"
    );
    assert!(!during.outdated(), "a fresh install is not outdated");
    assert_eq!(during.path, opencode_plugin_path(xdg.path()));

    uninstall("opencode").expect("uninstall");
    let after = status("opencode").expect("status");
    assert!(!after.installed);
    assert!(!after.path.exists());
}

// =====================================================================================
// cross-cutting
// =====================================================================================

#[test]
fn status_all_covers_every_known_agent_when_none_are_installed() {
    let _g = lock();
    let claude_home = tempfile::tempdir().unwrap();
    let codex_home = tempfile::tempdir().unwrap();
    let copilot_home = tempfile::tempdir().unwrap();
    let xdg_home = tempfile::tempdir().unwrap();
    let _e1 = EnvGuard::set("CLAUDE_CONFIG_DIR", claude_home.path());
    let _e2 = EnvGuard::set("CODEX_HOME", codex_home.path());
    let _e3 = EnvGuard::set("COPILOT_HOME", copilot_home.path());
    let _e4 = EnvGuard::set("XDG_CONFIG_HOME", xdg_home.path());

    let statuses = status_all();
    assert_eq!(statuses.len(), AGENTS.len());
    for (st, agent) in statuses.iter().zip(AGENTS.iter()) {
        assert_eq!(&st.agent, agent);
        assert!(!st.installed);
    }
}

#[test]
fn unknown_agent_is_rejected_by_every_entry_point() {
    assert!(install("bogus", Path::new("/nonexistent")).is_err());
    assert!(uninstall("bogus").is_err());
    assert!(status("bogus").is_err());
}

#[test]
fn uninstall_without_install_is_a_graceful_no_op() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());

    // Nothing was ever installed into this temp home: uninstall must not error.
    let report = uninstall("claude").expect("uninstall of a never-installed agent must not fail");
    assert_eq!(report.agent, "claude");
    assert!(!claude_script_path(home.path()).exists());
    assert!(!home.path().join("settings.json").exists());
}

#[test]
fn uninstall_without_install_leaves_a_pre_existing_settings_json_untouched() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());

    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    let original_bytes = fs::read(&settings).unwrap();

    uninstall("claude").expect("uninstall of a never-installed agent must not fail");

    let bytes_after = fs::read(&settings).unwrap();
    assert_eq!(
        bytes_after, original_bytes,
        "a file we never touched must not change"
    );
}

#[test]
fn claude_reinstall_replaces_the_entry_and_still_reverts_to_the_pre_first_install_state() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());

    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    let original_bytes = fs::read(&settings).unwrap();

    install("claude", &home.path().join("taw-bin-v1")).expect("first install");
    // Reinstalling (e.g. an upgrade, or pointing at a different taw_bin) must replace our
    // entry rather than add a second one.
    install("claude", &home.path().join("taw-bin-v2")).expect("second install");

    let installed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let session_start = installed["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        session_start.len(),
        2,
        "reinstall must replace, not duplicate, our entry"
    );
    let ours: Vec<_> = session_start
        .iter()
        .filter(|e| e.to_string().contains("taw-agent-state.sh"))
        .collect();
    assert_eq!(ours.len(), 1);

    uninstall("claude").expect("uninstall");
    let reverted_bytes = fs::read(&settings).unwrap();
    assert_eq!(
        reverted_bytes, original_bytes,
        "revert must still restore the state from *before the first* install, even after a reinstall"
    );
}

// =====================================================================================
// install() requires the agent's own config dir to already exist (herdr parity: a missing
// config dir means the agent itself isn't installed, so we must not fabricate one).
// =====================================================================================

#[test]
fn claude_install_fails_when_config_dir_does_not_exist() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("does-not-exist");
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", &missing);
    let taw_bin = home.path().join("taw-bin-placeholder");

    let err =
        install("claude", &taw_bin).expect_err("install must fail when the config dir is missing");
    let message = err.to_string();
    assert!(
        message.contains("config_dir_missing"),
        "error message: {message}"
    );
    assert!(
        message.contains(&missing.display().to_string()),
        "error message: {message}"
    );

    assert!(
        !missing.exists(),
        "install must not create the agent's config dir"
    );
}

#[test]
fn codex_install_fails_when_config_dir_does_not_exist() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("does-not-exist");
    let _env = EnvGuard::set("CODEX_HOME", &missing);
    let taw_bin = home.path().join("taw-bin-placeholder");

    let err =
        install("codex", &taw_bin).expect_err("install must fail when the config dir is missing");
    let message = err.to_string();
    assert!(
        message.contains("config_dir_missing"),
        "error message: {message}"
    );
    assert!(
        message.contains(&missing.display().to_string()),
        "error message: {message}"
    );

    assert!(
        !missing.exists(),
        "install must not create the agent's config dir"
    );
}

#[test]
fn copilot_install_fails_when_config_dir_does_not_exist() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("does-not-exist");
    let _env = EnvGuard::set("COPILOT_HOME", &missing);
    let taw_bin = home.path().join("taw-bin-placeholder");

    let err =
        install("copilot", &taw_bin).expect_err("install must fail when the config dir is missing");
    let message = err.to_string();
    assert!(
        message.contains("config_dir_missing"),
        "error message: {message}"
    );
    assert!(
        message.contains(&missing.display().to_string()),
        "error message: {message}"
    );

    assert!(
        !missing.exists(),
        "install must not create the agent's config dir"
    );
}

#[test]
fn opencode_install_fails_when_config_dir_does_not_exist() {
    let _g = lock();
    let xdg = tempfile::tempdir().unwrap();
    // Note: XDG_CONFIG_HOME itself exists (it's a generic XDG root), but OpenCode's own
    // config dir ($XDG_CONFIG_HOME/opencode) does not — that's the directory that must
    // exist, since it's the one only OpenCode itself would have created.
    let _env = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
    let missing = xdg.path().join("opencode");
    let taw_bin = xdg.path().join("taw-bin-placeholder");

    let err = install("opencode", &taw_bin)
        .expect_err("install must fail when the config dir is missing");
    let message = err.to_string();
    assert!(
        message.contains("config_dir_missing"),
        "error message: {message}"
    );
    assert!(
        message.contains(&missing.display().to_string()),
        "error message: {message}"
    );

    assert!(
        !missing.exists(),
        "install must not create the agent's config dir"
    );
}

// =====================================================================================
// uninstall() preserves edits made to the config file *after* our install (the
// merge-preserving path in `uninstall_json_hook` / `uninstall_toml_block`, as opposed to
// the byte-identical-revert path exercised by the `*_uninstall_reverts_*_byte_identical`
// tests above, which only cover the no-external-edit case).
// =====================================================================================

#[test]
fn claude_uninstall_preserves_external_edits_made_after_install() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();

    install("claude", &taw_bin).expect("install");

    // Something else (a person, another tool) edits settings.json after our install: a new
    // hook on a event we don't touch, and a brand-new top-level key.
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    value
        .get_mut("hooks")
        .and_then(|h| h.as_object_mut())
        .expect("hooks object")
        .insert(
            "Notification".to_string(),
            serde_json::json!([
                {
                    "matcher": "*",
                    "hooks": [ { "type": "command", "command": "echo notify", "timeout": 3 } ]
                }
            ]),
        );
    value
        .as_object_mut()
        .expect("root object")
        .insert("env".to_string(), serde_json::json!({ "FOO": "bar" }));
    let mut edited_bytes = serde_json::to_vec_pretty(&value).unwrap();
    edited_bytes.push(b'\n');
    fs::write(&settings, &edited_bytes).unwrap();

    uninstall("claude").expect("uninstall");

    let bytes = fs::read(&settings).unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&bytes).expect("uninstall must leave valid JSON");

    // our entry + script are gone
    let session_start = after["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        session_start.len(),
        1,
        "only the pre-existing (non-ours) SessionStart entry should remain"
    );
    assert!(
        session_start[0]
            .to_string()
            .contains("/opt/other-tool/hook.sh")
    );
    assert!(!claude_script_path(home.path()).exists());

    // everything present before our install, and everything added *after* our install,
    // survives untouched
    assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(after["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["hooks"]["Notification"][0]["hooks"][0]["command"],
        "echo notify"
    );
    assert_eq!(after["env"]["FOO"], "bar");

    // still valid, canonically pretty-printed (2-space indent) JSON with a trailing newline
    let mut expected_bytes = serde_json::to_vec_pretty(&after).unwrap();
    expected_bytes.push(b'\n');
    assert_eq!(
        bytes, expected_bytes,
        "the merge-preserving path must re-serialize with our canonical pretty-printer"
    );
}

#[test]
fn codex_uninstall_preserves_external_edits_to_config_toml_byte_for_byte() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");

    let config_toml = home.path().join("config.toml");
    fs::write(&config_toml, CODEX_CONFIG_TOML_FIXTURE).unwrap();

    install("codex", &taw_bin).expect("install");

    // Something else appends a brand-new table *after* our marked table.
    let installed = fs::read_to_string(&config_toml).unwrap();
    let external_addition = "\n[tui]\nfoo = \"bar\"\n";
    let edited = format!("{installed}{external_addition}");
    fs::write(&config_toml, &edited).unwrap();

    uninstall("codex").expect("uninstall");

    let reverted = fs::read_to_string(&config_toml).unwrap();
    let expected = format!("{CODEX_CONFIG_TOML_FIXTURE}{external_addition}");
    assert_eq!(
        reverted, expected,
        "uninstall must remove exactly our marked lines and leave the external table byte-for-byte"
    );
    assert!(
        !reverted.contains("taw integration"),
        "our marked lines must be gone"
    );
    assert!(
        reverted.contains("[tui]"),
        "the externally-added table must survive"
    );
}

/// A config.toml that already carries taw's block when taw first sees it: a dotfiles copy
/// from another machine, or what an earlier taw left behind. The copy install saves of that
/// file holds the block too, so putting those bytes back on uninstall would restore a block
/// that turns hooks on for a script the same uninstall deletes. The block is taken out
/// instead, and the user's own tables are left exactly as they were.
#[test]
fn codex_uninstall_removes_a_taw_block_that_was_in_config_toml_before_the_first_install() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    let taw_bin = home.path().join("taw-bin-placeholder");
    let config_toml = home.path().join("config.toml");
    let from_another_machine =
        "# >>> taw integration >>>\n[features]\nhooks = true\n# <<< taw integration <<<\n";
    fs::write(
        &config_toml,
        format!("{CODEX_CONFIG_TOML_FIXTURE}\n\n{from_another_machine}"),
    )
    .unwrap();

    install("codex", &taw_bin).expect("install");
    let installed = fs::read_to_string(&config_toml).unwrap();
    assert_eq!(
        installed.matches("# >>> taw integration >>>").count(),
        0,
        "install turns the block it found into marked lines: {installed}"
    );
    assert_eq!(installed.matches("hooks = true").count(), 1, "{installed}");

    uninstall("codex").expect("uninstall");

    let reverted = fs::read_to_string(&config_toml).unwrap();
    assert_eq!(
        reverted, CODEX_CONFIG_TOML_FIXTURE,
        "taw's block must be gone and the user's own tables intact: {reverted}"
    );
    assert!(!codex_script_path(home.path()).exists());
}

/// Shapes a real `settings.json` can plausibly hold that are not the one shape the hook
/// installer assumed. Each must not abort the process with a Rust panic (`install` calls
/// `.expect("root is always a JSON object")` and friends on whatever `serde_json` parsed)
/// instead of reporting an error and leaving the file alone.
const IMPLAUSIBLE_BUT_LEGAL_SETTINGS: &[(&str, &str)] = &[
    ("root is an array", "[]"),
    ("root is a string", "\"nope\""),
    ("root is a number", "42"),
    ("hooks is null", r#"{"hooks": null}"#),
    ("hooks is an array", r#"{"hooks": []}"#),
    ("hooks is a string", r#"{"hooks": "none"}"#),
    (
        "the event is an object, not an array",
        r#"{"hooks": {"SessionStart": {}}}"#,
    ),
    (
        "the event is a string",
        r#"{"hooks": {"SessionStart": "run-me"}}"#,
    ),
];

#[test]
fn claude_install_reports_an_unusable_settings_json_instead_of_panicking() {
    for (what, json) in IMPLAUSIBLE_BUT_LEGAL_SETTINGS {
        let _g = lock();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
        let settings = home.path().join("settings.json");
        fs::write(&settings, json).unwrap();

        let err = install("claude", &home.path().join("taw-bin-placeholder"))
            .expect_err(&format!("{what} must be an error, not a panic"));
        let text = format!("{err:#}");
        assert!(
            text.contains("hooks") || text.contains("settings.json"),
            "{what}: the message must name the JSON path it choked on, got: {text}"
        );

        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            *json,
            "{what}: a file we could not understand must be left untouched"
        );
        assert_eq!(
            fs::read_dir(home.path()).unwrap().count(),
            1,
            "{what}: a refused install writes nothing else (no hook script, no backup)"
        );
        assert!(!status("claude").unwrap().installed, "{what}");
    }
}

/// The usual first install through a dotfiles link: `settings.json` is a symlink to a file
/// that does not exist yet. Install creates that file through the link. Uninstall has to
/// take the file away again and leave the link where it is, so the dotfiles are as they
/// were; unlinking the link instead would strand the file with taw's hook still in it,
/// pointing at a script that was just deleted.
#[cfg(unix)]
#[test]
fn claude_uninstall_through_a_dangling_dotfiles_link_removes_the_file_and_keeps_the_link() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let dotfiles = tempfile::tempdir().unwrap();
    let target = dotfiles.path().join("claude-settings.json");
    let link = home.path().join("settings.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    install("claude", &home.path().join("taw-bin-placeholder")).unwrap();
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("SessionStart"),
        "install wrote through the link"
    );

    uninstall("claude").unwrap();

    assert!(
        !target.exists(),
        "the file the link points at must go, not be left holding taw's hook"
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        target,
        "the link itself stays, as it was before install"
    );
}

/// Installs Codex over `files` (name, bytes) in a fresh `CODEX_HOME`, and checks the install
/// is refused with nothing written: no hook script, no backup, and every file that was
/// already there exactly as it was.
fn assert_codex_install_is_refused_untouched(what: &str, files: &[(&str, &[u8])]) {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CODEX_HOME", home.path());
    for (name, bytes) in files {
        fs::write(home.path().join(name), bytes).unwrap();
    }

    install("codex", &home.path().join("taw-bin-placeholder"))
        .expect_err(&format!("{what}: install must refuse"));

    assert_eq!(
        fs::read_dir(home.path()).unwrap().count(),
        files.len(),
        "{what}: nothing may be written but the files that were already there"
    );
    for (name, bytes) in files {
        assert_eq!(
            fs::read(home.path().join(name)).unwrap(),
            *bytes,
            "{what}: {name} must be exactly as it was"
        );
    }
    assert!(!status("codex").unwrap().installed, "{what}");
}

/// Codex edits two files, and either one being unusable refuses the whole install before
/// a byte is written: not the hook script, not a backup, and not the edit to the other
/// file that would otherwise already have landed.
#[test]
fn codex_install_writes_nothing_when_either_config_file_cannot_be_edited() {
    let not_text: &[u8] = &[b'm', b'=', 0xff, 0xfe];
    assert_codex_install_is_refused_untouched(
        "hooks.json is not JSON",
        &[("hooks.json", &b"{ not json"[..])],
    );
    assert_codex_install_is_refused_untouched(
        "config.toml is not text",
        &[("config.toml", not_text)],
    );
    // The one that could go half wrong: hooks.json is edited before the TOML was read.
    assert_codex_install_is_refused_untouched(
        "hooks.json is fine but config.toml is not text",
        &[("hooks.json", &b"{}"[..]), ("config.toml", not_text)],
    );
}

#[test]
fn copilot_install_writes_nothing_when_settings_json_is_not_json() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, "{ not json").unwrap();

    install("copilot", &home.path().join("taw-bin-placeholder")).unwrap_err();

    assert_eq!(fs::read_to_string(&settings).unwrap(), "{ not json");
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    assert!(!status("copilot").unwrap().installed);
}

/// A settings.json somebody made read-only stays as it is even though its directory is
/// writable, which is all a rename needs to replace it. Before atomic writes, `fs::write`
/// failed on such a file; herdr refuses it too. The message is the one the CLI prints, so
/// it has to say which file and why.
#[cfg(unix)]
#[test]
fn claude_install_refuses_a_read_only_settings_json_and_leaves_it_unchanged() {
    use std::os::unix::fs::PermissionsExt;

    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&settings).is_ok() {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    let err = install("claude", &home.path().join("taw-bin-placeholder"))
        .expect_err("a read-only settings.json must be refused");

    let text = err.to_string();
    assert!(
        text.contains(&settings.display().to_string()) && text.contains("read-only"),
        "the message must name the file and say it is read-only: {text}"
    );
    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        CLAUDE_SETTINGS_FIXTURE
    );
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o444
    );
    // Refused up front: no hook script and no backup were written either.
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    assert!(!status("claude").unwrap().installed);
}

/// `agent` is installed, `config` is the file its uninstall has to rewrite and `script` its
/// hook script. Makes `config` read-only and checks the uninstall is refused with the
/// refusal itself as the message the CLI prints (nothing wrapped around it, because the
/// envelope carries only the outermost message), leaves `config` and the script as they
/// were, and finishes once the file is writable again, giving `original` back byte for
/// byte. The agents in `integration_ports.rs` get this from its shared checks.
#[cfg(unix)]
fn assert_uninstall_of_a_read_only_config_says_why(
    agent: &str,
    config: &Path,
    script: &Path,
    original: &str,
) {
    use std::os::unix::fs::PermissionsExt;
    let installed = fs::read(config).unwrap();
    fs::set_permissions(config, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(config).is_ok() {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    let err = uninstall(agent).expect_err("an uninstall must not rewrite a read-only config");

    let refusal = taw::integration::IntegrationError::ConfigReadOnly {
        path: config.display().to_string(),
    };
    assert_eq!(
        err.to_string(),
        refusal.to_string(),
        "{agent}: the message the CLI prints must be the refusal itself, with nothing wrapped around it"
    );
    assert_eq!(
        fs::read(config).unwrap(),
        installed,
        "{agent}: a refused uninstall must leave the config as it found it"
    );
    assert!(
        script.exists(),
        "{agent}: the hook script must stay until the config has been reverted"
    );
    assert!(status(agent).unwrap().installed);

    fs::set_permissions(config, fs::Permissions::from_mode(0o644)).unwrap();
    uninstall(agent).expect("uninstall once the config is writable again");
    assert_eq!(
        fs::read_to_string(config).unwrap(),
        original,
        "{agent}: the config did not revert byte-for-byte"
    );
    assert!(!script.exists());
    assert!(!status(agent).unwrap().installed);
}

#[cfg(unix)]
#[test]
fn claude_uninstall_of_a_read_only_settings_json_says_why_and_changes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    install("claude", &home.path().join("taw-bin-placeholder")).unwrap();

    assert_uninstall_of_a_read_only_config_says_why(
        "claude",
        &settings,
        &claude_script_path(home.path()),
        CLAUDE_SETTINGS_FIXTURE,
    );
}

#[cfg(unix)]
#[test]
fn copilot_uninstall_of_a_read_only_settings_json_says_why_and_changes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("COPILOT_HOME", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, COPILOT_SETTINGS_FIXTURE).unwrap();
    install("copilot", &home.path().join("taw-bin-placeholder")).unwrap();

    assert_uninstall_of_a_read_only_config_says_why(
        "copilot",
        &settings,
        &copilot_script_path(home.path()),
        COPILOT_SETTINGS_FIXTURE,
    );
}

/// Codex edits two files, and either one being read-only stops the uninstall with that
/// file named: hooks.json is reverted first, config.toml second.
#[cfg(unix)]
#[test]
fn codex_uninstall_of_a_read_only_hooks_json_or_config_toml_says_why_and_changes_nothing() {
    for read_only in ["hooks.json", "config.toml"] {
        let _g = lock();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("CODEX_HOME", home.path());
        fs::write(home.path().join("hooks.json"), CODEX_HOOKS_JSON_FIXTURE).unwrap();
        fs::write(home.path().join("config.toml"), CODEX_CONFIG_TOML_FIXTURE).unwrap();
        install("codex", &home.path().join("taw-bin-placeholder")).unwrap();

        assert_uninstall_of_a_read_only_config_says_why(
            "codex",
            &home.path().join(read_only),
            &codex_script_path(home.path()),
            match read_only {
                "hooks.json" => CODEX_HOOKS_JSON_FIXTURE,
                _ => CODEX_CONFIG_TOML_FIXTURE,
            },
        );
    }
}

#[test]
fn claude_uninstall_reports_an_unusable_settings_json_instead_of_panicking() {
    // The strip path has the same assumption, and it runs on a file the user has had
    // every chance to edit since install.
    for (what, json) in IMPLAUSIBLE_BUT_LEGAL_SETTINGS {
        let _g = lock();
        let home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
        let settings = home.path().join("settings.json");

        // A sidecar makes uninstall take its "revert against the pristine copy" path.
        install("claude", &home.path().join("taw-bin-placeholder")).expect("install");
        fs::write(&settings, json).unwrap();

        let result = uninstall("claude");
        assert!(
            result.is_ok() || format!("{:#}", result.unwrap_err()).contains("hooks"),
            "{what}: uninstall must not panic"
        );
        // Whatever it decided, it did not lose the user's file.
        assert!(settings.exists() || fs::read_dir(home.path()).unwrap().count() > 0);
    }
}

#[test]
fn opencode_ignores_a_relative_xdg_config_home_instead_of_resolving_it() {
    // The XDG spec says a relative value in `$XDG_CONFIG_HOME` is invalid and must be
    // ignored. Resolving it would put the opencode plugin under whatever directory `taw`
    // was run from — a different place per invocation, and inside the user's project.
    let _g = lock();
    let _env = EnvGuard::set("XDG_CONFIG_HOME", Path::new("relative/config"));

    let status = status("opencode").expect("status must not fail on a relative XDG value");

    assert!(
        status.path.is_absolute(),
        "the plugin path must be absolute, got {:?}",
        status.path
    );
    assert!(
        !status.path.starts_with("relative"),
        "the relative value must have been ignored, got {:?}",
        status.path
    );
    assert!(
        status.path.ends_with("opencode/plugins/taw-agent-state.js"),
        "and the documented default used instead, got {:?}",
        status.path
    );
}

// -----------------------------------------------------------------------------------
// One config folder under two names
//
// The user's Claude config is reachable as `~/.claude-home` and as `~/.claude-alt`: the same
// directory through links. An install made with `CLAUDE_CONFIG_DIR` spelled the one way wrote its
// hook entry with that spelling, and an uninstall run with the other did not know the entry for
// its own, left it in settings.json, and still deleted the hook script and the copy of the
// config, so the entry then pointed at nothing. (The ported agents have the same checks in
// `tests/integration_ports.rs`, from one table.)
// -----------------------------------------------------------------------------------

/// A directory that is reachable by two names: `real`, which is canonical, and `link`, a
/// symlink to it.
#[cfg(unix)]
struct TwoNames {
    _tmp: tempfile::TempDir,
    real: PathBuf,
    link: PathBuf,
}

#[cfg(unix)]
fn one_directory_two_names() -> TwoNames {
    let tmp = tempfile::tempdir().unwrap();
    // The temporary directory can itself be reached through a link (`/var` on macOS), which is one
    // more name for it: `real` is made canonical so that it is the one name that is not a link.
    let base = fs::canonicalize(tmp.path()).unwrap();
    let real = base.join("real");
    fs::create_dir(&real).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    TwoNames {
        _tmp: tmp,
        real,
        link,
    }
}

/// Installs `agent` with `var` naming its folder one way and uninstalls it with `var` naming it
/// the other, in both orders: every file in `files` (relative to the folder, with the bytes it
/// has before the install) comes back byte for byte, the hook script and every copy of a config
/// are gone, and `status` says not installed.
#[cfg(unix)]
fn assert_uninstall_through_the_other_name(
    agent: &str,
    var: &'static str,
    files: &[(&str, &str)],
    script: &str,
) {
    for installed_through_the_link in [true, false] {
        let _g = lock();
        let dirs = one_directory_two_names();
        for (name, bytes) in files {
            fs::write(dirs.real.join(name), bytes).unwrap();
        }
        let (first, second) = if installed_through_the_link {
            (&dirs.link, &dirs.real)
        } else {
            (&dirs.real, &dirs.link)
        };
        let how = if installed_through_the_link {
            "installed through the link, uninstalled through the real path"
        } else {
            "installed through the real path, uninstalled through the link"
        };

        {
            let _env = EnvGuard::set(var, first);
            install(agent, &dirs.real.join("taw-bin-placeholder")).expect("install");
        }
        {
            let _env = EnvGuard::set(var, second);
            uninstall(agent).unwrap_or_else(|err| panic!("{agent} {how}: {err:#}"));
            assert!(
                !status(agent).expect("status").installed,
                "{agent} {how}: still looked installed"
            );
        }

        for (name, bytes) in files {
            assert_eq!(
                fs::read_to_string(dirs.real.join(name)).unwrap(),
                *bytes,
                "{agent} {how}: {name} did not come back byte for byte"
            );
        }
        assert!(
            !dirs.real.join(script).exists(),
            "{agent} {how}: the hook script survived"
        );
        let copies = copies_of_configs(&dirs.real);
        assert!(
            copies.is_empty(),
            "{agent} {how}: the copy of a config was left behind: {copies:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_uninstall_finds_its_entry_when_the_config_folder_is_spelled_another_way() {
    assert_uninstall_through_the_other_name(
        "claude",
        "CLAUDE_CONFIG_DIR",
        &[("settings.json", CLAUDE_SETTINGS_FIXTURE)],
        "hooks/taw-agent-state.sh",
    );
}

/// The user's own setup: the config folder is reachable under two names, and `settings.json` in
/// it is itself a link to a file in another directory, a dotfiles repository. An install through
/// one name writes through the link; an uninstall through the other has to take taw's entry out
/// of the file the link points at and give it back as it was, leave the link where it is, and
/// take the hook script away, so that the dotfiles are as they were before the install.
#[cfg(unix)]
#[test]
fn claude_uninstall_through_the_other_name_keeps_a_dotfiles_link_and_gives_back_what_it_points_at()
{
    for installed_through_the_link in [true, false] {
        let _g = lock();
        let dirs = one_directory_two_names();
        let dotfiles = tempfile::tempdir().unwrap();
        let target = fs::canonicalize(dotfiles.path())
            .unwrap()
            .join("claude-settings.json");
        fs::write(&target, CLAUDE_SETTINGS_FIXTURE).unwrap();
        std::os::unix::fs::symlink(&target, dirs.real.join("settings.json")).unwrap();
        let (first, second) = if installed_through_the_link {
            (&dirs.link, &dirs.real)
        } else {
            (&dirs.real, &dirs.link)
        };
        let how = if installed_through_the_link {
            "installed through the link, uninstalled through the real path"
        } else {
            "installed through the real path, uninstalled through the link"
        };

        {
            let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", first);
            install("claude", &dirs.real.join("taw-bin-placeholder")).expect("install");
        }
        let installed = fs::read_to_string(&target).unwrap();
        assert!(
            installed.contains("taw-agent-state.sh"),
            "{how}: the install did not write through the link: {installed}"
        );
        {
            let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", second);
            uninstall("claude").unwrap_or_else(|err| panic!("{how}: {err:#}"));
        }

        assert_eq!(
            fs::read_link(dirs.real.join("settings.json")).unwrap(),
            target,
            "{how}: the link must stay where it was"
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            CLAUDE_SETTINGS_FIXTURE,
            "{how}: the file the link points at did not come back byte for byte"
        );
        assert!(
            !dirs.real.join("hooks/taw-agent-state.sh").exists(),
            "{how}: the hook script survived"
        );
        for folder in [&dirs.real, &fs::canonicalize(dotfiles.path()).unwrap()] {
            let copies = copies_of_configs(folder);
            assert!(
                copies.is_empty(),
                "{how}: the copy of the config was left behind: {copies:?}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn codex_uninstall_finds_its_entry_when_the_config_folder_is_spelled_another_way() {
    assert_uninstall_through_the_other_name(
        "codex",
        "CODEX_HOME",
        &[
            ("hooks.json", CODEX_HOOKS_JSON_FIXTURE),
            ("config.toml", CODEX_CONFIG_TOML_FIXTURE),
        ],
        "taw-agent-state.sh",
    );
}

#[cfg(unix)]
#[test]
fn copilot_uninstall_finds_its_entry_when_the_config_folder_is_spelled_another_way() {
    assert_uninstall_through_the_other_name(
        "copilot",
        "COPILOT_HOME",
        &[("settings.json", COPILOT_SETTINGS_FIXTURE)],
        "hooks/taw-agent-state.sh",
    );
}

/// What the user's two spellings do to a config that someone has edited since the install: taw's
/// entry goes through either name, and every other edit stays.
#[cfg(unix)]
#[test]
fn claude_uninstall_through_the_other_name_keeps_the_edits_made_after_install() {
    let _g = lock();
    let dirs = one_directory_two_names();
    let settings = dirs.real.join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    {
        let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", &dirs.link);
        install("claude", &dirs.real.join("taw-bin-placeholder")).expect("install");
    }
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("env".to_string(), serde_json::json!({ "FOO": "bar" }));
    fs::write(&settings, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    {
        let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", &dirs.real);
        uninstall("claude").expect("uninstall");
    }

    let after: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(after["env"]["FOO"], "bar");
    assert!(
        !after.to_string().contains("taw-agent-state.sh"),
        "taw's entry is still in settings.json: {after}"
    );
    assert!(!dirs.real.join("hooks/taw-agent-state.sh").exists());
}

/// A hook that names `taw-agent-state.sh` by a path this machine cannot resolve might be taw's,
/// from a folder that has since gone, or somebody else's. An uninstall cannot tell, and says so:
/// it names the file and the path, and changes nothing. The hook script and the copy of the
/// config stay, because the copy is all that can give the config back.
#[cfg(unix)]
#[test]
fn claude_uninstall_refuses_a_hook_it_cannot_tell_from_its_own_and_changes_nothing() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    install("claude", &home.path().join("taw-bin-placeholder")).expect("install");
    let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    value["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("bash '{stranger}' session"),
                "timeout": 10
            }]
        }));
    let edited = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(&settings, &edited).unwrap();
    let script = claude_script_path(home.path());
    let copies = copies_of_configs(home.path());
    assert_eq!(copies.len(), 1, "{copies:?}");
    let copy_bytes = fs::read(&copies[0]).unwrap();
    let script_bytes = fs::read(&script).unwrap();

    let err = uninstall("claude").expect_err("an entry it cannot match must be refused");

    let message = err.to_string();
    assert!(
        message.contains(&settings.display().to_string()) && message.contains(stranger),
        "the message must name the file and the path it could not match: {message}"
    );
    assert_eq!(
        fs::read(&settings).unwrap(),
        edited,
        "settings.json changed"
    );
    assert_eq!(
        fs::read(&script).unwrap(),
        script_bytes,
        "the script changed"
    );
    assert_eq!(
        copies_of_configs(home.path()),
        copies,
        "the copy of the config went"
    );
    assert_eq!(fs::read(&copies[0]).unwrap(), copy_bytes);
    assert!(status("claude").expect("status").installed);
}

/// The copy of the config goes only when the config was given back from it, or taw's entries were
/// taken out of the config. Taw's entry taken out by hand, and the file edited since, leaves
/// nothing to restore and nothing to remove: the file stays as it is, and so does the copy.
#[cfg(unix)]
#[test]
fn claude_uninstall_keeps_the_copy_when_the_entry_was_taken_out_by_hand() {
    let _g = lock();
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("CLAUDE_CONFIG_DIR", home.path());
    let settings = home.path().join("settings.json");
    fs::write(&settings, CLAUDE_SETTINGS_FIXTURE).unwrap();
    install("claude", &home.path().join("taw-bin-placeholder")).expect("install");
    let by_hand = "{\"theme\":\"light\"}";
    fs::write(&settings, by_hand).unwrap();
    let copies = copies_of_configs(home.path());
    assert_eq!(copies.len(), 1, "{copies:?}");

    uninstall("claude").expect("uninstall");

    assert_eq!(
        fs::read_to_string(&settings).unwrap(),
        by_hand,
        "a config with nothing of taw's in it was rewritten"
    );
    assert_eq!(copies_of_configs(home.path()), copies, "the copy went");
    assert!(!claude_script_path(home.path()).exists());
}
