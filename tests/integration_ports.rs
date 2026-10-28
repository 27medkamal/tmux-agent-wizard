// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Tests for the 13 herdr integrations ported after the original four
//! (`tests/integration_install.rs` covers claude/codex/copilot/opencode).
//!
//! Every test that overrides an agent's config-directory variable — or `$HOME`, for the
//! three agents herdr gives no override — holds [`ENV_LOCK`] for its whole duration:
//! `cargo test` runs the tests in one binary as parallel threads, and `std::env` is
//! shared, unsynchronized, process-global state. (This is a *separate* lock from the one
//! in `integration_install.rs`, which is safe because cargo runs each test binary as its
//! own process, with its own environment.)
//!
//! The helpers here mirror that file's deliberately: a fake `taw` that records its argv,
//! a `run_script` that feeds a herdr-shaped event payload on stdin with `TMUX_PANE` set,
//! and `bash -n` over every rendered shell hook. [`AgentCase`] then drives the six checks
//! every agent has to pass (install / status / idempotent reinstall /
//! byte-identical uninstall / missing config dir / malformed config) from one table, so a
//! per-agent test only has to cover what is actually specific to that agent.

#[path = "common/exec.rs"]
mod exec;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use taw::integration::{install, status, uninstall};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets an env var for the guard's lifetime, restoring (or removing) the prior value on drop.
struct EnvGuard {
    key: String,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &str, value: &Path) -> Self {
        Self::set_raw(key, value.as_os_str())
    }

    fn set_raw(key: &str, value: &std::ffi::OsStr) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: caller holds `ENV_LOCK` for the guard's whole lifetime, so no other
        // thread in this test binary observes env state mid-mutation.
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            prev,
        }
    }

    /// Prepends `dir` to `PATH` for the guard's lifetime.
    fn prepend_path(dir: &Path) -> Self {
        let mut value = OsString::from(dir);
        if let Some(existing) = std::env::var_os("PATH") {
            value.push(":");
            value.push(existing);
        }
        Self::set_raw("PATH", &value)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

/// Writes an executable fake `taw` into `dir` that records its argv, one file per
/// invocation, inside the directory `$FAKE_TAW_RECORD` names.
///
/// A file per call, rather than appends to one file (as the fake in
/// `integration_install.rs` does): the lifecycle-authority integrations here report
/// several times per run and the plugin ones report *concurrently*, so appends from
/// separate processes interleave into an unreadable jumble. [`read_calls`] puts the calls
/// back in order by their `--seq`, which every integration sends and every one derives
/// from a monotonic clock or counter.
fn write_fake_taw(dir: &Path) -> PathBuf {
    let path = dir.join("taw");
    let script = "#!/bin/sh\n\
out=\"${FAKE_TAW_RECORD:-}\"\n\
[ -n \"$out\" ] || exit 0\n\
mkdir -p \"$out\" 2>/dev/null || exit 0\n\
file=\"$(mktemp \"$out/call.XXXXXX\")\" || exit 0\n\
for a in \"$@\"; do\n\
    printf '%s\\n' \"$a\" >> \"$file\"\n\
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
/// (removed entirely when `None`), `FAKE_TAW_RECORD` pointed at `record`, plus `extra_env`,
/// and `extra_args` appended positionally. Every taw hook must exit 0 by design, so this
/// asserts success.
fn run_script(
    script: &Path,
    tmux_pane: Option<&str>,
    stdin: &[u8],
    record: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) {
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
    cmd.env_remove("TAW_REPORT_FILE");
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
}

/// [`run_script`], additionally returning the hook's stdout — for the one hook that must
/// print a JSON object on every exit path.
fn run_script_stdout(
    script: &Path,
    tmux_pane: Option<&str>,
    stdin: &[u8],
    record: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> String {
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
    cmd.env_remove("TAW_REPORT_FILE");
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
        "script exited non-zero: status={:?}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Every `taw` invocation the fake recorded, one `Vec<String>` of argv per call, ordered
/// by the `--seq` each integration sends.
fn read_calls(dir: &Path) -> Vec<Vec<String>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut calls: Vec<Vec<String>> = entries
        .filter_map(|e| fs::read_to_string(e.ok()?.path()).ok())
        .map(|text| text.lines().map(str::to_string).collect())
        .filter(|argv: &Vec<String>| !argv.is_empty())
        .collect();
    calls.sort_by_key(|argv| {
        argv.iter()
            .position(|a| a == "--seq")
            .and_then(|i| argv.get(i + 1))
            .and_then(|v| v.parse::<u128>().ok())
            .unwrap_or(0)
    });
    calls
}

/// Forgets every call recorded so far, so one test can check several payloads in turn.
fn reset_record(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// [`read_calls`] once at least `at_least` of them have landed, or after ten seconds.
///
/// An extension invokes `taw` fire-and-forget, so a call can be written a moment after the
/// node driver itself has exited: reading the record once and indexing into it is a read
/// of whatever happened to be flushed, and panics outright on a loaded machine. The
/// caller still asserts on what came back — this only decides when to stop waiting.
fn wait_for_calls(record: &Path, at_least: usize) -> Vec<Vec<String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while read_calls(record).len() < at_least && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    read_calls(record)
}

/// The single `taw` invocation the fake recorded, panicking if there was not exactly one.
fn read_one_call(path: &Path) -> Vec<String> {
    let calls = read_calls(path);
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one taw call, got {calls:?}"
    );
    calls.into_iter().next().unwrap()
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

fn assert_no_flag(argv: &[String], name: &str) {
    assert!(
        !argv.iter().any(|a| a == name),
        "unexpected flag {name} in {argv:?}"
    );
}

/// Asserts `argv` carries a numeric `--seq` (its value is current-time nanoseconds, so
/// this checks shape, not the number).
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

/// True if `name` resolves on `PATH`. Node/python-backed integrations skip their
/// interpreter-dependent assertions rather than fail on a machine without one.
fn have(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// =====================================================================================
// The table-driven checks every ported agent has to pass
// =====================================================================================

/// One agent's file layout, as the shared checks need to see it.
struct AgentCase {
    /// The `taw integration install <agent>` target name.
    agent: &'static str,
    /// The variable that points the installer at a throwaway config directory, and what to
    /// set it to relative to the tempdir root. `HOME` for the agents herdr gives no
    /// override (`droid`, `kilo`, `mastracode`), so the fixture still never touches the
    /// developer's own dotfiles.
    env_var: &'static str,
    /// What `env_var` is set to, relative to the tempdir root (`""` = the root itself).
    env_value: &'static str,
    /// The config directory `install` requires, relative to the tempdir root.
    config_dir: &'static str,
    /// The installed script/plugin `status` reports, relative to the tempdir root.
    script: &'static str,
    /// Any config file install edits, relative to the tempdir root, together with a
    /// realistic pre-existing content fixture the revert must restore byte-for-byte.
    config: Option<(&'static str, &'static str)>,
    /// The comment prefix the version marker uses in the installed script.
    marker: &'static str,
    /// The `TAW_INTEGRATION_VERSION` the bundled template currently carries.
    version: u32,
    /// `(binary, --version output)` for an agent whose install probes its own version.
    /// The fixture puts a stub printing that output on `PATH`, so the checks below do not
    /// depend on whether the developer happens to have the real agent installed — or on
    /// which version of it.
    probe: Option<(&'static str, &'static str)>,
}

impl AgentCase {
    /// Creates the throwaway root, points `env_var` at it, creates the agent's config
    /// directory, and (for an agent that probes its own version) puts a stub of it on
    /// `PATH`. The returned guards must outlive every call.
    fn prepare(&self) -> (tempfile::TempDir, Vec<EnvGuard>) {
        let (root, guards) = self.prepare_without_config_dir();
        fs::create_dir_all(root.path().join(self.config_dir)).unwrap();
        (root, guards)
    }

    /// [`Self::prepare`] without the config directory — for the check that a missing one
    /// is refused.
    fn prepare_without_config_dir(&self) -> (tempfile::TempDir, Vec<EnvGuard>) {
        let root = tempfile::tempdir().unwrap();
        let mut guards = vec![EnvGuard::set(
            self.env_var,
            &root.path().join(self.env_value),
        )];
        if let Some((binary, version)) = self.probe {
            let bin = root.path().join("stub-bin");
            fs::create_dir_all(&bin).unwrap();
            let stub = bin.join(binary);
            fs::write(&stub, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
            make_executable(&stub);
            guards.push(EnvGuard::prepend_path(&bin));
        }
        (root, guards)
    }

    /// `env_var` pointed at `root`, spelled however the caller spelled it, and a stub of the
    /// agent's own binary on `PATH` for one that probes its version, kept in `stubs`. For the
    /// checks that reach one directory by two names, where [`Self::prepare`] makes a root of
    /// its own.
    fn point_at(&self, root: &Path, stubs: &Path) -> Vec<EnvGuard> {
        let mut guards = vec![EnvGuard::set(self.env_var, &root.join(self.env_value))];
        if let Some((binary, version)) = self.probe {
            let bin = stubs.join("stub-bin");
            fs::create_dir_all(&bin).unwrap();
            let stub = bin.join(binary);
            fs::write(&stub, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
            make_executable(&stub);
            guards.push(EnvGuard::prepend_path(&bin));
        }
        guards
    }

    fn script_path(&self, root: &Path) -> PathBuf {
        root.join(self.script)
    }

    fn config_path(&self, root: &Path) -> Option<PathBuf> {
        self.config.map(|(rel, _)| root.join(rel))
    }
}

/// The whole table. Kept in one place so a newly ported agent cannot quietly skip a check.
const CASES: &[AgentCase] = &[
    AgentCase {
        agent: "devin",
        env_var: "XDG_CONFIG_HOME",
        env_value: "",
        config_dir: "devin",
        script: "devin/taw-agent-state.sh",
        config: Some((
            "devin/config.json",
            "{\n  \"model\": \"devin-1\",\n  \"hooks\": {\n    \"Stop\": [\n      { \"hooks\": [{ \"type\": \"command\", \"command\": \"echo other\" }] }\n    ]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "droid",
        env_var: "HOME",
        env_value: "",
        config_dir: ".factory",
        script: ".factory/hooks/taw-agent-state.sh",
        config: Some((
            ".factory/settings.json",
            "{\n  \"hooks\": {\n    \"SessionStart\": [\n      { \"hooks\": [{ \"type\": \"command\", \"command\": \"echo other\" }] }\n    ]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "qodercli",
        env_var: "QODER_CONFIG_DIR",
        env_value: "qoder",
        config_dir: "qoder",
        script: "qoder/hooks/taw-agent-state.sh",
        config: Some((
            "qoder/settings.json",
            "{\n  \"hooks\": {\n    \"Stop\": [\n      { \"matcher\": \"*\", \"hooks\": [{ \"type\": \"command\", \"command\": \"echo bye\" }] }\n    ]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "pi",
        env_var: "PI_CODING_AGENT_DIR",
        env_value: "pi-agent",
        config_dir: "pi-agent",
        script: "pi-agent/extensions/taw-agent-state.ts",
        // Pi loads every extension in the directory; there is no config file.
        config: None,
        marker: "// TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "omp",
        // The agent directory OMP falls back to, `$HOME/${PI_CONFIG_DIR:-.omp}/agent`.
        env_var: "HOME",
        env_value: "",
        config_dir: ".omp/agent",
        script: ".omp/agent/extensions/taw-omp-agent-state.ts",
        config: None,
        marker: "// TAW_INTEGRATION_VERSION=",
        version: 4,
        probe: None,
    },
    AgentCase {
        agent: "kilo",
        env_var: "XDG_CONFIG_HOME",
        env_value: "",
        config_dir: "kilo",
        script: "kilo/plugin/taw-agent-state.js",
        // Kilo discovers plugins by directory convention; there is no config file.
        config: None,
        marker: "// TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "hermes",
        env_var: "HERMES_HOME",
        env_value: "hermes",
        config_dir: "hermes",
        script: "hermes/plugins/taw-agent-state/__init__.py",
        config: Some((
            "hermes/config.yaml",
            "# my hermes\nmodel: sonnet\nplugins:\n  enabled:\n    - other\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "kimi",
        env_var: "KIMI_CODE_HOME",
        env_value: "kimi-code",
        config_dir: "kimi-code",
        script: "kimi-code/hooks/taw-agent-state.sh",
        config: Some((
            "kimi-code/config.toml",
            "model = \"k2\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"echo theirs\"\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: Some(("kimi", "kimi 0.14.0")),
    },
    AgentCase {
        agent: "antigravity-cli",
        env_var: "ANTIGRAVITY_CLI_CONFIG_DIR",
        env_value: "gemini-config",
        config_dir: "gemini-config",
        script: "gemini-config/hooks/taw-agent-state.sh",
        config: Some((
            "gemini-config/hooks.json",
            "{\n  \"someOtherHook\": {\n    \"PreToolUse\": [{ \"type\": \"command\", \"command\": \"echo theirs\" }]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "mastracode",
        env_var: "HOME",
        env_value: "",
        config_dir: ".mastracode",
        script: ".mastracode/hooks/taw-agent-state.sh",
        config: Some((
            ".mastracode/hooks.json",
            "{\n  \"Stop\": [{ \"type\": \"command\", \"command\": \"echo theirs\", \"timeout\": 500 }]\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "grok",
        env_var: "GROK_HOME",
        env_value: "grok",
        config_dir: "grok",
        script: "grok/hooks/taw-agent-state.sh",
        // Both files grok gets are taw's own; there is no user config to revert.
        config: None,
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 3,
        probe: None,
    },
    AgentCase {
        agent: "cursor",
        env_var: "CURSOR_CONFIG_DIR",
        env_value: "cursor",
        config_dir: "cursor",
        script: "cursor/taw-agent-state.sh",
        config: Some((
            "cursor/hooks.json",
            "{\n  \"version\": 1,\n  \"hooks\": {\n    \"beforeShellExecution\": [{ \"command\": \"echo audit\" }]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "qwen",
        env_var: "QWEN_HOME",
        env_value: "qwen",
        config_dir: "qwen",
        script: "qwen/hooks/taw-agent-session.sh",
        config: Some(("qwen/settings.json", "{\n  \"theme\": \"dark\"\n}\n")),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
    AgentCase {
        agent: "letta",
        // herdr gives Letta no directory override; taw follows suit.
        env_var: "HOME",
        env_value: "",
        config_dir: ".letta",
        script: ".letta/hooks/taw-agent-session.sh",
        config: Some((
            ".letta/settings.json",
            "{\n  \"theme\": \"dark\",\n  \"hooks\": {\n    \"SessionStart\": [\n      { \"hooks\": [{ \"type\": \"command\", \"command\": \"echo user\" }] }\n    ]\n  }\n}\n",
        )),
        marker: "# TAW_INTEGRATION_VERSION=",
        version: 2,
        probe: None,
    },
];

fn case(agent: &str) -> &'static AgentCase {
    CASES
        .iter()
        .find(|c| c.agent == agent)
        .unwrap_or_else(|| panic!("no AgentCase for {agent}"))
}

/// install writes the script (with a version marker) and every file it reports.
fn check_install_writes_files(c: &AgentCase) {
    let (root, _env) = c.prepare();
    let taw_bin = root.path().join("taw-bin-placeholder");

    let report = install(c.agent, &taw_bin).expect("install");

    let script = c.script_path(root.path());
    assert!(script.is_file(), "{} was not written", script.display());
    let contents = fs::read_to_string(&script).unwrap();
    assert!(
        contents.contains(&format!("{}{}", c.marker, c.version)),
        "{} has no version marker",
        script.display()
    );
    assert!(
        !contents.contains("{{"),
        "{} still has an unsubstituted placeholder",
        script.display()
    );
    assert_eq!(report.agent, c.agent);
    for file in &report.files {
        assert!(
            file.exists(),
            "install reported {} but it does not exist",
            file.display()
        );
    }
    if let Some(config) = c.config_path(root.path()) {
        assert!(
            report.files.contains(&config),
            "install did not report {}",
            config.display()
        );
    }
}

/// status is false before install, true (at the case's current version, pointing at the
/// script) after, and false again once uninstalled.
fn check_status_round_trip(c: &AgentCase) {
    let (root, _env) = c.prepare();
    let taw_bin = root.path().join("taw-bin-placeholder");

    let before = status(c.agent).expect("status");
    assert!(!before.installed, "{} looked installed", c.agent);
    assert_eq!(before.version, None);

    install(c.agent, &taw_bin).expect("install");
    let during = status(c.agent).expect("status");
    assert!(during.installed, "{} did not look installed", c.agent);
    assert_eq!(during.version, Some(c.version));
    // What an install writes is what this build says it writes, so a fresh one is not outdated.
    assert_eq!(during.current, Some(c.version), "{}", c.agent);
    assert!(!during.outdated(), "{} is outdated as installed", c.agent);
    assert_eq!(during.path, c.script_path(root.path()));

    uninstall(c.agent).expect("uninstall");
    let after = status(c.agent).expect("status");
    assert!(!after.installed, "{} still looked installed", c.agent);
}

/// A second install over the first changes nothing: same bytes, no duplicated hook entry.
fn check_reinstall_is_idempotent(c: &AgentCase) {
    let (root, _env) = c.prepare();
    let taw_bin = root.path().join("taw-bin-placeholder");
    if let Some((rel, fixture)) = c.config {
        fs::write(root.path().join(rel), fixture).unwrap();
    }

    install(c.agent, &taw_bin).expect("first install");
    let after_first: Vec<Vec<u8>> = c
        .config_path(root.path())
        .into_iter()
        .chain([c.script_path(root.path())])
        .map(|p| fs::read(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display())))
        .collect();

    install(c.agent, &taw_bin).expect("second install");
    let after_second: Vec<Vec<u8>> = c
        .config_path(root.path())
        .into_iter()
        .chain([c.script_path(root.path())])
        .map(|p| fs::read(&p).unwrap())
        .collect();

    assert_eq!(
        after_first, after_second,
        "reinstalling {} changed its files",
        c.agent
    );
}

/// A realistic pre-existing config survives install + uninstall byte-for-byte, and the
/// script is gone afterwards.
fn check_uninstall_reverts_byte_identical(c: &AgentCase) {
    let (root, _env) = c.prepare();
    let taw_bin = root.path().join("taw-bin-placeholder");
    let Some((rel, fixture)) = c.config else {
        // No config edit at all: install/uninstall is just the script file.
        install(c.agent, &taw_bin).expect("install");
        uninstall(c.agent).expect("uninstall");
        assert!(!c.script_path(root.path()).exists());
        return;
    };
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    let original = fs::read(&config).unwrap();

    install(c.agent, &taw_bin).expect("install");
    let installed = fs::read(&config).unwrap();
    assert_ne!(installed, original, "install did not edit {rel}");

    uninstall(c.agent).expect("uninstall");
    assert_eq!(
        fs::read(&config).unwrap(),
        original,
        "{rel} did not revert byte-for-byte"
    );
    assert!(
        !c.script_path(root.path()).exists(),
        "the hook script survived uninstall"
    );
}

/// Every file under `dir`, at any depth.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// A config only its owner may read has a copy kept beside the hook that only its owner may
/// read: the copy holds the same bytes, and a config can hold tokens.
#[cfg(unix)]
fn check_the_copy_of_a_private_config_is_private(c: &AgentCase) {
    use std::os::unix::fs::PermissionsExt;
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

    install(c.agent, &root.path().join("taw-bin-placeholder")).expect("install");

    let copies: Vec<PathBuf> = files_under(root.path())
        .into_iter()
        .filter(|path| path.to_string_lossy().contains(".orig"))
        .collect();
    assert!(!copies.is_empty(), "{}: no copy of the config", c.agent);
    for copy in copies {
        let mode = fs::metadata(&copy).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}: {}", c.agent, copy.display());
    }
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}: the config itself", c.agent);
}

/// A missing config directory means the agent is not installed: install refuses with
/// `config_dir_missing` and writes nothing at all.
fn check_missing_config_dir_is_refused(c: &AgentCase) {
    // Deliberately *not* creating `c.config_dir`.
    let (root, _env) = c.prepare_without_config_dir();
    let taw_bin = root.path().join("taw-bin-placeholder");

    let err = install(c.agent, &taw_bin).expect_err("install must refuse");
    let err = err
        .downcast_ref::<taw::integration::IntegrationError>()
        .unwrap_or_else(|| panic!("{} raised {err:#} rather than a typed error", c.agent));
    assert!(
        matches!(
            err,
            taw::integration::IntegrationError::ConfigDirMissing { .. }
        ),
        "{} raised {err:?}",
        c.agent
    );
    assert!(
        !c.script_path(root.path()).exists(),
        "a refused install still wrote the hook script"
    );
    if let Some(config) = c.config_path(root.path()) {
        assert!(
            !config.exists(),
            "a refused install still wrote {}",
            config.display()
        );
    }
    assert!(!status(c.agent).expect("status").installed);
}

/// Every file and directory under `root` with its bytes, keyed by path relative to it: what
/// "an install that was refused wrote nothing" is compared against.
fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(
        root: &Path,
        dir: &Path,
        out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(relative, None);
                walk(root, &path, out);
            } else {
                out.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Installs `c.agent` over a config that cannot be edited, and checks the refusal is
/// complete: the error names the file's problem in its top-level message (the only part the
/// CLI prints), nothing at all is written or created under `root` (not the hook script, not
/// a backup of the config, not a directory), and `status` still says not installed.
fn assert_refused_without_writing_anything(c: &AgentCase, root: &Path, want_in_message: &str) {
    let before = tree(root);
    let err = install(c.agent, &root.join("taw-bin-placeholder"))
        .expect_err("install must refuse a config it cannot edit");
    let top = err.to_string();
    assert!(
        top.contains(want_in_message),
        "{}: the top-level message must say {want_in_message:?}: {top}",
        c.agent
    );
    assert_eq!(
        tree(root),
        before,
        "{}: a refused install must write nothing at all",
        c.agent
    );
    assert!(
        !status(c.agent).expect("status").installed,
        "{}: a refused install must not look installed",
        c.agent
    );
}

/// A config file that is not JSON (or not the shape a hook goes into) is reported, and
/// left exactly as it was found, and the install publishes nothing: the hook script comes
/// after the config has been read, not before (herdr's
/// `install_letta_does_not_publish_hook_when_settings_are_invalid`, for every agent).
fn check_malformed_config_is_reported(c: &AgentCase) {
    let Some((rel, _)) = c.config else { return };
    if !rel.ends_with(".json") {
        return; // TOML/YAML agents have their own malformed-input test
    }
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    let broken = "{ this is not json";
    fs::write(&config, broken).unwrap();

    assert_refused_without_writing_anything(c, root.path(), rel.rsplit('/').next().unwrap());
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        broken,
        "a refused install must leave the file alone"
    );
}

/// A config with a second hard link cannot be replaced without leaving the other name
/// holding stale content, so it is refused before anything is written (herdr's
/// `check_config_targets`, which runs before it writes its assets).
#[cfg(unix)]
fn check_hard_linked_config_is_refused_before_anything_is_written(c: &AgentCase) {
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    fs::hard_link(&config, root.path().join("alias-of-the-config")).unwrap();

    assert_refused_without_writing_anything(c, root.path(), "multiple hard links");
    assert_eq!(fs::read_to_string(&config).unwrap(), fixture);
}

/// A config somebody made read-only is refused before anything is written, even though its
/// directory is writable.
#[cfg(unix)]
fn check_read_only_config_is_refused_before_anything_is_written(c: &AgentCase) {
    use std::os::unix::fs::PermissionsExt;
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&config).is_ok() {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    assert_refused_without_writing_anything(c, root.path(), "read-only");
    assert_eq!(fs::read_to_string(&config).unwrap(), fixture);
}

/// A config somebody made read-only after taw installed into it: the uninstall is refused,
/// and what the person is told is the refusal itself (which file, that it was left as it
/// is, what to do), not "reverting <file>" wrapped around it, because the CLI prints only
/// an error's outermost message. The refusal changes nothing: the hook script and the
/// edited config stay as they were, so making the file writable and uninstalling again
/// finishes the job and gives the file back byte for byte.
#[cfg(unix)]
fn check_uninstall_of_a_read_only_config_says_why_and_changes_nothing(c: &AgentCase) {
    use std::os::unix::fs::PermissionsExt;
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    install(c.agent, &root.path().join("taw-bin-placeholder")).expect("install");
    let installed = fs::read(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&config).is_ok() {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    let err = uninstall(c.agent).expect_err("an uninstall must not rewrite a read-only config");

    let refusal = taw::integration::IntegrationError::ConfigReadOnly {
        path: config.display().to_string(),
    };
    assert_eq!(
        err.to_string(),
        refusal.to_string(),
        "{}: the message the CLI prints must be the refusal itself, with nothing wrapped around it",
        c.agent
    );
    assert_eq!(
        fs::read(&config).unwrap(),
        installed,
        "{}: a refused uninstall must leave the config as it found it",
        c.agent
    );
    assert!(
        c.script_path(root.path()).exists(),
        "{}: the hook script must stay until the config has been reverted",
        c.agent
    );
    assert!(status(c.agent).expect("status").installed);

    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    uninstall(c.agent).expect("uninstall once the config is writable again");
    assert_eq!(
        fs::read(&config).unwrap(),
        fixture.as_bytes(),
        "{}: the config did not revert byte-for-byte",
        c.agent
    );
    assert!(!c.script_path(root.path()).exists());
}

/// The same for a config that gained a second hard link after taw installed into it: a
/// rename would replace one name only, so the uninstall is refused, and the message the CLI
/// prints says which file and why instead of "reverting <file>". Removing the other name
/// lets the uninstall finish.
#[cfg(unix)]
fn check_uninstall_of_a_hard_linked_config_says_why_and_changes_nothing(c: &AgentCase) {
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    install(c.agent, &root.path().join("taw-bin-placeholder")).expect("install");
    let installed = fs::read(&config).unwrap();
    let alias = root.path().join("alias-of-the-config");
    fs::hard_link(&config, &alias).unwrap();

    let err = uninstall(c.agent).expect_err("an uninstall must not replace a hard-linked config");

    let top = err.to_string();
    assert!(
        top.contains(&config.display().to_string()) && top.contains("multiple hard links"),
        "{}: the message the CLI prints must name the file and say why: {top}",
        c.agent
    );
    assert_eq!(fs::read(&config).unwrap(), installed, "{}", c.agent);
    assert_eq!(fs::read(&alias).unwrap(), installed, "{}", c.agent);
    assert!(
        c.script_path(root.path()).exists(),
        "{}: the hook script must stay until the config has been reverted",
        c.agent
    );

    fs::remove_file(&alias).unwrap();
    uninstall(c.agent).expect("uninstall once the config has one name again");
    assert_eq!(
        fs::read(&config).unwrap(),
        fixture.as_bytes(),
        "{}: the config did not revert byte-for-byte",
        c.agent
    );
    assert!(!c.script_path(root.path()).exists());
}

/// A directory that is reachable by two names, as the user's Claude config is reachable as
/// `~/.claude-home` and `~/.claude-alt` (the same directory through links): `real`, which is
/// canonical, and `link`, a symlink to it.
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

/// An uninstall run with the agent's folder spelled another way than the install was finds its
/// own entry all the same: the entry is gone, the config is back byte for byte, and the hook
/// script and the copy kept of the config are gone with it. An install made through a link
/// wrote the entry with the link's path, and an uninstall through the real path did not know it
/// for its own, left it in the config, and still deleted the script and the copy, so that the
/// entry then pointed at nothing and nothing could restore the file.
#[cfg(unix)]
fn check_uninstall_finds_its_entry_through_another_spelling_of_the_folder(c: &AgentCase) {
    for installed_through_the_link in [true, false] {
        let dirs = one_directory_two_names();
        let stubs = tempfile::tempdir().unwrap();
        fs::create_dir_all(dirs.real.join(c.config_dir)).unwrap();
        if let Some((rel, fixture)) = c.config {
            fs::write(dirs.real.join(rel), fixture).unwrap();
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

        let env = c.point_at(first, stubs.path());
        install(c.agent, &dirs.real.join("taw-bin-placeholder"))
            .unwrap_or_else(|err| panic!("{}: install: {err:#}", c.agent));
        drop(env);

        let env = c.point_at(second, stubs.path());
        uninstall(c.agent).unwrap_or_else(|err| panic!("{} {how}: {err:#}", c.agent));
        assert!(
            !status(c.agent).expect("status").installed,
            "{} {how}: still looked installed",
            c.agent
        );
        drop(env);

        if let Some((rel, fixture)) = c.config {
            assert_eq!(
                fs::read_to_string(dirs.real.join(rel)).unwrap(),
                fixture,
                "{} {how}: {rel} did not come back byte for byte",
                c.agent
            );
        }
        assert!(
            !c.script_path(&dirs.real).exists(),
            "{} {how}: the hook script survived",
            c.agent
        );
        let copies: Vec<PathBuf> = files_under(&dirs.real)
            .into_iter()
            .filter(|path| path.to_string_lossy().contains(".orig"))
            .collect();
        assert!(
            copies.is_empty(),
            "{} {how}: the copy of the config was left behind: {copies:?}",
            c.agent
        );
    }
}

/// `entry` is an element of an array in `value` that mentions `script`: another like it, for a
/// script at `stranger`, is added to that array. `false` when nothing in `value` names the
/// script by its path (an agent whose entry is found by its key).
#[cfg(unix)]
fn add_a_hook_for_a_stranger(value: &mut serde_json::Value, script: &str, stranger: &str) -> bool {
    match value {
        serde_json::Value::Array(items) => {
            if let Some(entry) = items
                .iter()
                .find(|entry| entry.to_string().contains(script))
            {
                let copy = entry.to_string().replace(script, stranger);
                items.push(serde_json::from_str(&copy).unwrap());
                return true;
            }
            items
                .iter_mut()
                .any(|item| add_a_hook_for_a_stranger(item, script, stranger))
        }
        serde_json::Value::Object(map) => map
            .values_mut()
            .any(|item| add_a_hook_for_a_stranger(item, script, stranger)),
        _ => false,
    }
}

/// The agents whose config holds no hook that names the script by its path, so that there is no
/// stranger for an uninstall to refuse: the config of Antigravity finds its entry by its key
/// (`taw`) and removes it whole, whatever it says; Hermes keeps its hooks in YAML and Kimi in TOML,
/// which an uninstall edits as text; and the plugin or extension of Grok, Kilo, OMP and Pi is a file
/// of its own with no config to edit. Every other agent runs the refusal check, and one that cannot
/// is a failure, not a skip.
#[cfg(unix)]
const NO_HOOK_TO_REFUSE: &[&str] = &[
    "antigravity-cli",
    "grok",
    "hermes",
    "kilo",
    "kimi",
    "omp",
    "pi",
];

/// A hook that names taw's script by a path this machine cannot resolve might be taw's, from a
/// folder that has since gone, or somebody else's. An uninstall cannot tell, so it refuses,
/// saying which file and which path, and changes nothing: not the config, not the hook script
/// it would have deleted, and not the copy that is all that can restore the config.
#[cfg(unix)]
fn check_uninstall_refuses_a_taw_hook_it_cannot_match_and_changes_nothing(c: &AgentCase) {
    let json_config = c.config.filter(|(rel, _)| rel.ends_with(".json"));
    if NO_HOOK_TO_REFUSE.contains(&c.agent) {
        assert!(
            json_config.is_none() || c.agent == "antigravity-cli",
            "{}: it has a JSON config with hooks in it, so the refusal check applies to it: take \
             it off NO_HOOK_TO_REFUSE",
            c.agent
        );
        return;
    }
    let Some((rel, fixture)) = json_config else {
        panic!(
            "{}: the refusal check needs a JSON config, and this agent has none: if its entry \
             is found some other way, put it on NO_HOOK_TO_REFUSE",
            c.agent
        );
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    install(c.agent, &root.path().join("taw-bin-placeholder")).expect("install");
    let script = c.script_path(root.path());
    let stranger = format!(
        "/folder-that-is-not-there/{}",
        script.file_name().unwrap().to_string_lossy()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert!(
        add_a_hook_for_a_stranger(&mut value, &script.to_string_lossy(), &stranger),
        "{}: nothing in its config names the script by its path, so there is no hook to put a \
         stranger beside: if its entry is found some other way, put it on NO_HOOK_TO_REFUSE",
        c.agent
    );
    let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
    bytes.push(b'\n');
    fs::write(&config, &bytes).unwrap();
    let before = tree(root.path());

    let err = uninstall(c.agent).expect_err("a hook that cannot be matched must be refused");

    let message = err.to_string();
    assert!(
        message.contains(&config.display().to_string()) && message.contains(&stranger),
        "{}: the message must name the file and the path it could not match: {message}",
        c.agent
    );
    assert_eq!(
        tree(root.path()),
        before,
        "{}: a refused uninstall must change nothing",
        c.agent
    );
    assert!(status(c.agent).expect("status").installed);
}

/// The copy kept of a config goes only when the config was given back from it, or taw's entries
/// were taken out of the config. A config that holds nothing of taw's and is not what the copy
/// says it was (somebody took the entry out by hand and edited the file since) is left as it is,
/// and so is the copy.
#[cfg(unix)]
fn check_uninstall_keeps_the_copy_when_it_neither_restored_nor_removed_anything(c: &AgentCase) {
    let Some((rel, fixture)) = c.config else {
        return;
    };
    let edited = if rel.ends_with(".json") {
        let mut value: serde_json::Value = serde_json::from_str(fixture).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("edited_by_hand".to_string(), serde_json::json!(true));
        let mut text = serde_json::to_string_pretty(&value).unwrap();
        text.push('\n');
        text
    } else {
        format!("{fixture}\n# edited by hand\n")
    };
    let (root, _env) = c.prepare();
    let config = root.path().join(rel);
    fs::write(&config, fixture).unwrap();
    install(c.agent, &root.path().join("taw-bin-placeholder")).expect("install");
    fs::write(&config, &edited).unwrap();
    let copies = |dir: &Path| -> Vec<PathBuf> {
        files_under(dir)
            .into_iter()
            .filter(|path| path.to_string_lossy().contains(".orig"))
            .collect()
    };
    let kept = copies(root.path());
    assert!(!kept.is_empty(), "{}: no copy of the config", c.agent);

    uninstall(c.agent).expect("uninstall");

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        edited,
        "{}: a config with nothing of taw's in it was rewritten",
        c.agent
    );
    assert_eq!(
        copies(root.path()),
        kept,
        "{}: the copy of the config went though nothing was restored or removed",
        c.agent
    );
    assert!(!c.script_path(root.path()).exists());
}

/// Every check above, for one agent.
fn check_all(agent: &str) {
    let c = case(agent);
    check_install_writes_files(c);
    check_status_round_trip(c);
    check_reinstall_is_idempotent(c);
    check_uninstall_reverts_byte_identical(c);
    check_missing_config_dir_is_refused(c);
    check_malformed_config_is_reported(c);
    #[cfg(unix)]
    {
        check_the_copy_of_a_private_config_is_private(c);
        check_hard_linked_config_is_refused_before_anything_is_written(c);
        check_read_only_config_is_refused_before_anything_is_written(c);
        check_uninstall_of_a_read_only_config_says_why_and_changes_nothing(c);
        check_uninstall_of_a_hard_linked_config_says_why_and_changes_nothing(c);
        check_uninstall_finds_its_entry_through_another_spelling_of_the_folder(c);
        check_uninstall_refuses_a_taw_hook_it_cannot_match_and_changes_nothing(c);
        check_uninstall_keeps_the_copy_when_it_neither_restored_nor_removed_anything(c);
    }
}

// =====================================================================================
// devin
// =====================================================================================

const DEVIN_SESSION_JSON: &str =
    r#"{"session_id":"dv-1","hook_event_name":"SessionStart","source":"resume"}"#;

#[test]
fn devin_passes_every_shared_check() {
    let _g = lock();
    check_all("devin");
}

#[test]
fn devin_install_registers_all_six_events_once_each() {
    let _g = lock();
    let c = case("devin");
    let (root, _env) = c.prepare();
    install("devin", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(c.config_path(root.path()).unwrap()).unwrap())
            .unwrap();
    let script = c.script_path(root.path()).to_string_lossy().into_owned();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "Stop",
    ] {
        let entries = value["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("no hooks.{event} array"));
        let ours: Vec<_> = entries
            .iter()
            .filter(|e| e.to_string().contains(&script))
            .collect();
        assert_eq!(ours.len(), 1, "hooks.{event} has {} of ours", ours.len());
        assert_eq!(ours[0]["hooks"][0]["timeout"], 10);
        assert_eq!(ours[0]["hooks"][0]["type"], "command");
    }
}

#[test]
fn devin_rendered_script_passes_bash_n() {
    let _g = lock();
    let c = case("devin");
    let (root, _env) = c.prepare();
    install("devin", &root.path().join("taw-bin-placeholder")).unwrap();
    bash_n(&c.script_path(root.path()));
}

#[test]
fn devin_session_event_reports_the_payload_session_id() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the devin hook parses JSON with it");
        return;
    }
    let c = case("devin");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("devin", &fake).unwrap();
    let record = root.path().join("taw-calls");

    run_script(
        &c.script_path(root.path()),
        Some("%3"),
        DEVIN_SESSION_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );

    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%3"]);
    assert_flag(&argv, "--source", "integration:devin");
    assert_flag(&argv, "--agent", "devin");
    assert_flag(&argv, "--agent-session-id", "dv-1");
    assert_seq_present(&argv);
    // herdr's devin hook sends no transcript path and no session-start source, so
    // neither may appear here: an invented value would be reported as if devin had
    // supplied it.
    assert_no_flag(&argv, "--agent-session-path");
    assert_no_flag(&argv, "--session-start-source");
}

#[test]
fn devin_reports_nothing_without_tmux_pane_or_for_another_action() {
    let _g = lock();
    let c = case("devin");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("devin", &fake).unwrap();
    let script = c.script_path(root.path());
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        None,
        DEVIN_SESSION_JSON.as_bytes(),
        &record,
        &["session"],
        &[],
    );
    assert!(read_calls(&record).is_empty(), "reported without TMUX_PANE");

    run_script(
        &script,
        Some("%3"),
        DEVIN_SESSION_JSON.as_bytes(),
        &record,
        &["working"],
        &[],
    );
    assert!(
        read_calls(&record).is_empty(),
        "reported for an action the hook does not handle"
    );
}

/// herdr's guard: a payload with no `session_id` may fall back to `devin list`, except on
/// `UserPromptSubmit` and on a `startup` `SessionStart` — where devin has no session worth
/// looking up yet, so a list entry would attribute another pane's session to this one.
#[test]
fn devin_session_list_fallback_is_used_only_where_herdr_allows_it() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the devin hook parses JSON with it");
        return;
    }
    let c = case("devin");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("devin", &fake).unwrap();
    let script = c.script_path(root.path());
    let record = root.path().join("taw-calls");

    let work = root.path().join("proj");
    fs::create_dir_all(&work).unwrap();
    let listing = format!(
        r#"[{{"id":"from-list","working_directory":"{}"}}]"#,
        work.display()
    );
    let env: &[(&str, &str)] = &[
        ("TAW_DEVIN_LIST_JSON", listing.as_str()),
        ("DEVIN_PROJECT_DIR", work.to_str().unwrap()),
    ];

    // A tool event with no session id: the fallback applies.
    run_script(
        &script,
        Some("%4"),
        br#"{"hook_event_name":"PreToolUse"}"#,
        &record,
        &["session"],
        env,
    );
    assert_flag(&read_one_call(&record), "--agent-session-id", "from-list");

    // …but not for these two.
    for payload in [
        &br#"{"hook_event_name":"UserPromptSubmit"}"#[..],
        &br#"{"hook_event_name":"SessionStart","source":"startup"}"#[..],
    ] {
        reset_record(&record);
        run_script(&script, Some("%4"), payload, &record, &["session"], env);
        assert!(
            read_calls(&record).is_empty(),
            "the list fallback fired for {}",
            String::from_utf8_lossy(payload)
        );
    }

    // A working directory that is not this pane's is never adopted.
    reset_record(&record);
    run_script(
        &script,
        Some("%4"),
        br#"{"hook_event_name":"PreToolUse"}"#,
        &record,
        &["session"],
        &[
            (
                "TAW_DEVIN_LIST_JSON",
                r#"[{"id":"x","working_directory":"/somewhere/else"}]"#,
            ),
            ("DEVIN_PROJECT_DIR", work.to_str().unwrap()),
        ],
    );
    assert!(
        read_calls(&record).is_empty(),
        "adopted a session from another directory"
    );
}

// =====================================================================================
// droid
// =====================================================================================

#[test]
fn droid_passes_every_shared_check() {
    let _g = lock();
    check_all("droid");
}

#[test]
fn droid_session_start_reports_session_identity_only() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the droid hook parses JSON with it");
        return;
    }
    let c = case("droid");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("droid", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%7"),
        br#"{"session_id":"dr-9"}"#,
        &record,
        &["session"],
        &[],
    );

    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%7"]);
    assert_flag(&argv, "--source", "integration:droid");
    assert_flag(&argv, "--agent", "droid");
    assert_flag(&argv, "--agent-session-id", "dr-9");
    assert_seq_present(&argv);

    // A payload with no session id is not worth a report: herdr exits rather than
    // reporting the pane as an agent with no resumable session.
    reset_record(&record);
    run_script(&script, Some("%7"), br#"{}"#, &record, &["session"], &[]);
    assert!(read_calls(&record).is_empty());
}

/// herdr's install sweeps its own old entries out of `~/.factory/hooks.json`. taw never
/// wrote there, and every entry it removes is matched by the path of the script it
/// installed — so a file it did not write is left exactly alone.
#[test]
fn droid_never_touches_the_legacy_hooks_json() {
    let _g = lock();
    let c = case("droid");
    let (root, _env) = c.prepare();
    let legacy = root.path().join(".factory/hooks.json");
    let content = "{\n  \"hooks\": { \"SessionStart\": [{ \"hooks\": [] }] }\n}\n";
    fs::write(&legacy, content).unwrap();

    install("droid", &root.path().join("taw-bin-placeholder")).unwrap();
    assert_eq!(fs::read_to_string(&legacy).unwrap(), content);
    uninstall("droid").unwrap();
    assert_eq!(fs::read_to_string(&legacy).unwrap(), content);
}

// =====================================================================================
// qodercli
// =====================================================================================

#[test]
fn qodercli_passes_every_shared_check() {
    let _g = lock();
    check_all("qodercli");
}

#[test]
fn qodercli_install_uses_the_claude_shaped_matcher_entry() {
    let _g = lock();
    let c = case("qodercli");
    let (root, _env) = c.prepare();
    install("qodercli", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(c.config_path(root.path()).unwrap()).unwrap())
            .unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["matcher"], "*");
    assert_eq!(entries[0]["hooks"][0]["timeout"], 10);
}

#[test]
fn qodercli_session_start_reports_session_identity() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the qodercli hook parses JSON with it");
        return;
    }
    let c = case("qodercli");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("qodercli", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%8"),
        br#"{"session_id":"qo-1","hook_event_name":"SessionStart"}"#,
        &record,
        &["session"],
        &[],
    );

    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%8"]);
    assert_flag(&argv, "--source", "integration:qodercli");
    assert_flag(&argv, "--agent", "qodercli");
    assert_flag(&argv, "--agent-session-id", "qo-1");
    assert_seq_present(&argv);
}

// =====================================================================================
// qwen
// =====================================================================================

#[test]
fn qwen_passes_every_shared_check() {
    let _g = lock();
    check_all("qwen");
}

/// qwen's hook timeout is milliseconds, so the entry carries 10000 where the
/// seconds-schema agents carry 10. Getting this wrong gives the hook 10 ms to run.
#[test]
fn qwen_install_writes_a_millisecond_timeout() {
    let _g = lock();
    let c = case("qwen");
    let (root, _env) = c.prepare();
    install("qwen", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(c.config_path(root.path()).unwrap()).unwrap())
            .unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["matcher"], "*");
    assert_eq!(entries[0]["hooks"][0]["timeout"], 10_000);
}

#[test]
fn qwen_forwards_only_the_documented_session_start_sources() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the qwen hook parses JSON with it");
        return;
    }
    let c = case("qwen");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("qwen", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%9"),
        br#"{"session_id":"qw-1","source":"resume"}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%9"]);
    assert_flag(&argv, "--agent", "qwen");
    assert_flag(&argv, "--agent-session-id", "qw-1");
    assert_flag(&argv, "--session-start-source", "resume");

    // An undocumented reason is dropped rather than passed through unvalidated.
    reset_record(&record);
    run_script(
        &script,
        Some("%9"),
        br#"{"session_id":"qw-1","source":"something-new"}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_flag(&argv, "--agent-session-id", "qw-1");
    assert_no_flag(&argv, "--session-start-source");
}

// =====================================================================================
// letta (experimental)
// =====================================================================================

#[test]
fn letta_passes_every_shared_check() {
    let _g = lock();
    check_all("letta");
}

/// Letta's hook entry carries no `matcher` key at all — unlike every other agent's
/// `{"matcher": "*", ...}` shape — because Letta's own `SessionStart` schema has none
/// (herdr `ensure_letta_session_hook`).
#[test]
fn letta_install_writes_a_matcherless_millisecond_timeout_entry() {
    let _g = lock();
    let c = case("letta");
    let (root, _env) = c.prepare();
    install("letta", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(c.config_path(root.path()).unwrap()).unwrap())
            .unwrap();
    let entries = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].get("matcher").is_none());
    assert_eq!(entries[0]["hooks"][0]["timeout"], 10_000);
    assert_eq!(entries[0]["hooks"][0]["quiet"], true);
}

/// Letta shares the JSON-hook helpers Claude uses, and herdr's Letta install removes only
/// its own command from a group too: a hook somebody else put in the same `SessionStart`
/// group as taw's survives a reinstall and an uninstall.
#[test]
fn letta_reinstall_and_uninstall_keep_a_user_hook_in_the_same_group() {
    let _g = lock();
    let c = case("letta");
    let (root, _env) = c.prepare();
    let script = c.script_path(root.path());
    let taw_command = format!("bash '{}' session", script.display());
    let settings = c.config_path(root.path()).unwrap();
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "theme": "dark",
            "hooks": {"SessionStart": [{
                "hooks": [
                    {"type": "command", "command": taw_command, "timeout": 10_000, "quiet": true},
                    {"type": "command", "command": "echo user"},
                ]
            }]}
        }))
        .unwrap(),
    )
    .unwrap();
    let taw_bin = root.path().join("taw-bin-placeholder");

    install("letta", &taw_bin).unwrap();
    let installed = fs::read_to_string(&settings).unwrap();
    let value: serde_json::Value = serde_json::from_str(&installed).unwrap();
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo user");
    assert!(groups[1].get("matcher").is_none());
    assert_eq!(groups[1]["hooks"][0]["command"], taw_command.as_str());

    install("letta", &taw_bin).unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), installed);

    uninstall("letta").unwrap();
    let text = fs::read_to_string(&settings).unwrap();
    assert!(!text.contains(script.to_string_lossy().as_ref()), "{text}");
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["theme"], "dark");
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo user");
}

/// Follows herdr's `install_letta_does_not_publish_hook_when_settings_are_invalid`: a
/// settings.json that is not JSON is refused before the hook script is written, so nothing
/// is half installed and `status` says not installed (the shared checks make the same
/// claim, and the same about a hard-linked or read-only file, for every agent).
#[test]
fn letta_install_does_not_publish_the_hook_when_settings_are_invalid() {
    let _g = lock();
    let c = case("letta");
    let (root, _env) = c.prepare();
    let settings = c.config_path(root.path()).unwrap();
    fs::write(&settings, "not json").unwrap();

    assert!(install("letta", &root.path().join("taw-bin-placeholder")).is_err());

    assert!(!c.script_path(root.path()).exists());
    assert!(!status("letta").unwrap().installed);
    assert_eq!(fs::read_to_string(&settings).unwrap(), "not json");
}

/// A refused install must not save `not json` as the backup of the config: after settings.json
/// was fixed and the install repeated, the uninstall would fail on the backup and leave taw's
/// hook in settings.json. The refused
/// install leaves no backup, and the later pair install and uninstall behave as usual.
#[test]
fn letta_install_after_a_refused_one_uninstalls_cleanly() {
    let _g = lock();
    let c = case("letta");
    let (root, _env) = c.prepare();
    let settings = c.config_path(root.path()).unwrap();
    let taw_bin = root.path().join("taw-bin-placeholder");
    fs::write(&settings, "not json").unwrap();
    install("letta", &taw_bin).unwrap_err();

    let fixed = "{\n  \"theme\": \"dark\"\n}\n";
    fs::write(&settings, fixed).unwrap();
    install("letta", &taw_bin).unwrap();
    assert!(status("letta").unwrap().installed);

    uninstall("letta").unwrap();
    assert_eq!(fs::read_to_string(&settings).unwrap(), fixed);
    assert!(!c.script_path(root.path()).exists());
    assert!(!status("letta").unwrap().installed);
}

/// A backup an older build left behind after a refused install (it saved the config as it
/// found it, unparseable) cannot say what the config looked like, so uninstall does not
/// fail on it: it takes taw's entry out of the file as it is, and drops the backup.
#[test]
fn letta_uninstall_ignores_a_saved_copy_an_older_build_left_unparseable() {
    let _g = lock();
    let c = case("letta");
    let (root, _env) = c.prepare();
    let script = c.script_path(root.path());
    let settings = c.config_path(root.path()).unwrap();
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(&script, "#!/bin/sh\n# TAW_INTEGRATION_VERSION=1\n").unwrap();
    let mut backup = script.as_os_str().to_owned();
    backup.push(".orig.json");
    let backup = PathBuf::from(backup);
    fs::write(&backup, "not json").unwrap();
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "theme": "dark",
            "hooks": {"SessionStart": [
                {"hooks": [{"type": "command", "command": "echo user"}]},
                {"hooks": [{"type": "command",
                            "command": format!("bash '{}' session", script.display())}]},
            ]}
        }))
        .unwrap(),
    )
    .unwrap();

    uninstall("letta").unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(value["theme"], "dark");
    let groups = value["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["hooks"][0]["command"], "echo user");
    assert!(!script.exists());
    assert!(!backup.exists(), "the unusable backup must not linger");
}

/// SessionStart stdout is injected into Letta's next user message, so the hook must be
/// silent on every path — including a payload with no usable conversation id, which every
/// other agent's hook would otherwise be free to ignore noisily.
#[test]
fn letta_session_hook_never_prints_anything() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the letta hook parses JSON with it");
        return;
    }
    let c = case("letta");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("letta", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    for payload in [
        &br#"not json"#[..],
        br#"{"conversation_id":"default","is_new_session":false}"#, // no agent_id
        br#"{"agent_id":"agent-1","is_new_session":false}"#,        // no conversation_id
        br#"{}"#,
    ] {
        reset_record(&record);
        let stdout = run_script_stdout(&script, Some("%3"), payload, &record, &["session"], &[]);
        assert_eq!(stdout, "", "hook printed output for payload {payload:?}");
        assert!(
            read_calls(&record).is_empty(),
            "hook reported a session for unusable payload {payload:?}"
        );
    }
}

/// Letta's default conversation has no id of its own; the hook encodes it as
/// `default:<agent_id>` so `session restore` can tell it apart from a real conversation id
/// (`src/session/restore.rs`'s `letta` resume arm decodes the same encoding).
#[test]
fn letta_session_hook_encodes_the_default_conversation_and_forwards_is_new_session() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the letta hook parses JSON with it");
        return;
    }
    let c = case("letta");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("letta", &fake).unwrap();
    let script = c.script_path(root.path());
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%4"),
        br#"{"conversation_id":"default","agent_id":"agent-123","is_new_session":false}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%4"]);
    assert_flag(&argv, "--source", "integration:letta");
    assert_flag(&argv, "--agent", "letta");
    assert_flag(&argv, "--agent-session-id", "default:agent-123");
    assert_flag(&argv, "--session-start-source", "resume");
    assert_seq_present(&argv);

    reset_record(&record);
    run_script(
        &script,
        Some("%4"),
        br#"{"conversation_id":"conv-7","is_new_session":true}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_flag(&argv, "--agent-session-id", "conv-7");
    assert_flag(&argv, "--session-start-source", "new");
}

// =====================================================================================
// grok
// =====================================================================================

#[test]
fn grok_passes_every_shared_check() {
    let _g = lock();
    check_all("grok");
}

/// grok merges every `hooks/*.json`, so taw ships its own file rather than editing the
/// user's. Both files are taw-owned, and uninstall removes exactly those two — an
/// unrelated hook file beside them is untouched.
#[test]
fn grok_owns_its_hook_config_and_leaves_other_hook_files_alone() {
    let _g = lock();
    let c = case("grok");
    let (root, _env) = c.prepare();
    let hooks_dir = root.path().join("grok/hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let theirs = hooks_dir.join("mine.json");
    fs::write(&theirs, "{\"hooks\":{}}\n").unwrap();

    let report = install("grok", &root.path().join("taw-bin-placeholder")).unwrap();
    let config = hooks_dir.join("taw.json");
    assert_eq!(
        report.files,
        vec![c.script_path(root.path()), config.clone()]
    );

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    assert_eq!(hook["timeout"], 10);
    // herdr runs the grok hook through `sh`, not `bash`.
    let command = hook["command"].as_str().unwrap();
    assert!(command.starts_with("sh '"), "{command}");
    assert!(command.ends_with("' session"), "{command}");

    uninstall("grok").unwrap();
    assert!(!config.exists(), "taw.json survived uninstall");
    assert!(!c.script_path(root.path()).exists());
    assert_eq!(
        fs::read_to_string(&theirs).unwrap(),
        "{\"hooks\":{}}\n",
        "another tool's hook file must be untouched"
    );
}

/// What `hooks/taw.json` holds when it is already there before a first install: another
/// machine's copy of taw's own hook file, from a dotfiles repository.
const GROK_TAW_JSON_FROM_ANOTHER_MACHINE: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sh '/Users/other/.grok/hooks/taw-agent-state.sh' session",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
"#;

/// Puts [`GROK_TAW_JSON_FROM_ANOTHER_MACHINE`] where grok's install will write its hook
/// file, in a directory that is otherwise empty, and returns its path.
fn grok_hook_config_already_there(root: &Path) -> PathBuf {
    let config = root.join("grok/hooks/taw.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, GROK_TAW_JSON_FROM_ANOTHER_MACHINE).unwrap();
    config
}

/// `hooks/taw.json` is the one file grok's install writes besides the hook script, and it
/// is the one the up-front config check covers: grok has no user config in the case table
/// (`config: None`), so the shared read-only and hard-link checks skip it. A hook file
/// somebody made read-only is refused before the script is published, whatever its
/// writable directory would let a rename do, and nothing else is written.
#[cfg(unix)]
#[test]
fn grok_install_refuses_a_read_only_hook_config_before_any_script_is_written() {
    use std::os::unix::fs::PermissionsExt;
    let _g = lock();
    let c = case("grok");
    let (root, _env) = c.prepare();
    let config = grok_hook_config_already_there(root.path());
    fs::set_permissions(&config, fs::Permissions::from_mode(0o444)).unwrap();
    if fs::OpenOptions::new().write(true).open(&config).is_ok() {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    assert_refused_without_writing_anything(c, root.path(), "read-only");

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        GROK_TAW_JSON_FROM_ANOTHER_MACHINE
    );
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert!(
        !c.script_path(root.path()).exists(),
        "the hook script was published for a hook file that could not be written"
    );
}

/// The same for a hook file with a second hard link, which a rename would replace in one
/// place only, leaving the other name holding the old content.
#[cfg(unix)]
#[test]
fn grok_install_refuses_a_hard_linked_hook_config_before_any_script_is_written() {
    let _g = lock();
    let c = case("grok");
    let (root, _env) = c.prepare();
    let config = grok_hook_config_already_there(root.path());
    fs::hard_link(&config, root.path().join("alias-of-the-hook-config")).unwrap();

    assert_refused_without_writing_anything(c, root.path(), "multiple hard links");

    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        GROK_TAW_JSON_FROM_ANOTHER_MACHINE
    );
    assert!(
        !c.script_path(root.path()).exists(),
        "the hook script was published for a hook file that could not be replaced"
    );
}

#[test]
fn grok_prefers_its_injected_session_id_over_the_payload() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the grok hook parses JSON with it");
        return;
    }
    let c = case("grok");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("grok", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    // GROK_SESSION_ID wins: grok injects it into every hook process.
    run_script(
        &script,
        Some("%11"),
        br#"{"hook_event_name":"session_start","session_id":"from-payload"}"#,
        &record,
        &["session"],
        &[("GROK_SESSION_ID", "from-env")],
    );
    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%11"]);
    assert_flag(&argv, "--agent", "grok");
    assert_flag(&argv, "--agent-session-id", "from-env");

    // …and the payload is the fallback when it is absent.
    reset_record(&record);
    run_script(
        &script,
        Some("%11"),
        br#"{"hook_event_name":"sessionStart","sessionId":"from-payload"}"#,
        &record,
        &["session"],
        &[],
    );
    assert_flag(
        &read_one_call(&record),
        "--agent-session-id",
        "from-payload",
    );

    // A non-session event is not reported, even with a session id to hand.
    reset_record(&record);
    run_script(
        &script,
        Some("%11"),
        br#"{"hook_event_name":"PreToolUse","session_id":"x"}"#,
        &record,
        &["session"],
        &[],
    );
    assert!(read_calls(&record).is_empty());
}

/// taw already records Claude's SessionStart `source`; grok's own hook does the same
/// (herdr `herdr-agent-state.sh:65,76-84`), and an absent source passes no flag at all
/// rather than an empty one.
#[test]
fn grok_reports_its_session_start_source_when_the_payload_has_one() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the grok hook parses JSON with it");
        return;
    }
    let c = case("grok");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("grok", &fake).unwrap();
    let script = c.script_path(root.path());
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%12"),
        br#"{"hook_event_name":"session_start","session_id":"s1","source":"new"}"#,
        &record,
        &["session"],
        &[],
    );
    assert_flag(&read_one_call(&record), "--session-start-source", "new");

    reset_record(&record);
    run_script(
        &script,
        Some("%12"),
        br#"{"hook_event_name":"session_start","session_id":"s1"}"#,
        &record,
        &["session"],
        &[],
    );
    assert_no_flag(&read_one_call(&record), "--session-start-source");
}

// =====================================================================================
// cursor
// =====================================================================================

#[test]
fn cursor_passes_every_shared_check() {
    let _g = lock();
    check_all("cursor");
}

/// Cursor's entry is a bare `{"command": …}` and the file needs a top-level `version`.
/// Install adds the version only when the file does not already declare one.
#[test]
fn cursor_install_writes_a_bare_command_entry_and_the_schema_version() {
    let _g = lock();
    let c = case("cursor");
    let (root, _env) = c.prepare();
    let hooks = c.config_path(root.path()).unwrap();

    // A hooks.json taw creates from nothing carries version 1.
    install("cursor", &root.path().join("taw-bin-placeholder")).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(value["version"], 1);
    let entries = value["hooks"]["sessionStart"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].as_object().unwrap().len(), 1, "{:?}", entries[0]);
    assert!(
        entries[0]["command"]
            .as_str()
            .unwrap()
            .ends_with("' session")
    );
    uninstall("cursor").unwrap();
    assert!(
        !hooks.exists(),
        "a file taw created must not be left behind"
    );

    // A file that already declares a version keeps its own.
    fs::write(&hooks, "{\n  \"version\": 2\n}\n").unwrap();
    install("cursor", &root.path().join("taw-bin-placeholder")).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(value["version"], 2);
}

/// The `version` install adds must not stop an otherwise-untouched file reverting
/// byte-for-byte, and must not be stripped from a file that *did* change since install.
#[test]
fn cursor_uninstall_handles_the_version_it_added_in_both_directions() {
    let _g = lock();
    let c = case("cursor");
    let (root, _env) = c.prepare();
    let hooks = c.config_path(root.path()).unwrap();
    let taw_bin = root.path().join("taw-bin-placeholder");

    // No version of their own: ours is added on install and gone after uninstall.
    let original = "{\n  \"hooks\": { \"stop\": [{ \"command\": \"echo bye\" }] }\n}\n";
    fs::write(&hooks, original).unwrap();
    install("cursor", &taw_bin).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&hooks).unwrap()).unwrap()["version"],
        1
    );
    uninstall("cursor").unwrap();
    assert_eq!(fs::read_to_string(&hooks).unwrap(), original);

    // An external edit after install: our entry goes, their new hook and the version stay.
    fs::write(&hooks, original).unwrap();
    install("cursor", &taw_bin).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    value["hooks"]["afterFileEdit"] = serde_json::json!([{ "command": "echo theirs" }]);
    fs::write(&hooks, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    uninstall("cursor").unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(value["version"], 1, "a changed file keeps its version key");
    assert!(value["hooks"]["afterFileEdit"].is_array());
    assert!(value["hooks"]["stop"].is_array());
    assert!(
        value["hooks"]["sessionStart"].is_null(),
        "our own entry must still be gone: {value}"
    );
}

#[test]
fn cursor_session_start_accepts_every_session_id_spelling() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the cursor hook parses JSON with it");
        return;
    }
    let c = case("cursor");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("cursor", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    for (payload, expected) in [
        (
            &br#"{"hook_event_name":"sessionStart","session_id":"a"}"#[..],
            "a",
        ),
        (
            &br#"{"hookEventName":"sessionStart","sessionId":"b"}"#[..],
            "b",
        ),
        (&br#"{"conversation_id":"c"}"#[..], "c"),
        (&br#"{"conversationId":"d"}"#[..], "d"),
    ] {
        reset_record(&record);
        run_script(&script, Some("%12"), payload, &record, &["session"], &[]);
        let argv = read_one_call(&record);
        assert_flag(&argv, "--agent", "cursor");
        assert_flag(&argv, "--agent-session-id", expected);
    }

    // Any other event is ignored, session id or not.
    reset_record(&record);
    run_script(
        &script,
        Some("%12"),
        br#"{"hook_event_name":"stop","session_id":"a"}"#,
        &record,
        &["session"],
        &[],
    );
    assert!(read_calls(&record).is_empty());
}

// =====================================================================================
// antigravity-cli
// =====================================================================================

#[test]
fn antigravity_cli_passes_every_shared_check() {
    let _g = lock();
    check_all("antigravity-cli");
}

/// Antigravity keys `hooks.json` by hook *name*, so everything taw writes lives under one
/// `taw` key it rewrites wholesale — and another tool's named hook beside it survives.
#[test]
fn antigravity_cli_owns_one_named_block_and_leaves_other_named_hooks_alone() {
    let _g = lock();
    let c = case("antigravity-cli");
    let (root, _env) = c.prepare();
    let hooks = c.config_path(root.path()).unwrap();
    fs::write(&hooks, c.config.unwrap().1).unwrap();

    install("antigravity-cli", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert!(
        value["someOtherHook"]["PreToolUse"].is_array(),
        "another tool's named hook was disturbed: {value}"
    );
    // `PreInvocation` takes a flat handler list; a matcher/hooks wrapper here would
    // invalidate the whole file.
    let handlers = value["taw"]["PreInvocation"].as_array().unwrap();
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0]["type"], "command");
    assert_eq!(handlers[0]["timeout"], 10);
    assert!(handlers[0].get("matcher").is_none(), "{:?}", handlers[0]);
    assert!(handlers[0].get("hooks").is_none(), "{:?}", handlers[0]);
    // PreInvocation is the only event registered: it is the one carrying conversationId.
    assert_eq!(value["taw"].as_object().unwrap().len(), 1);
}

#[test]
fn antigravity_cli_reports_the_conversation_as_agy_and_always_prints_an_object() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the antigravity hook parses JSON with it");
        return;
    }
    let c = case("antigravity-cli");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("antigravity-cli", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    let stdout = run_script_stdout(
        &script,
        Some("%13"),
        br#"{"conversationId":"conv-1","transcriptPath":"/t/conv-1.json"}"#,
        &record,
        &["session"],
        &[],
    );
    assert_eq!(
        stdout.trim(),
        "{}",
        "the hook must emit an empty JSON object"
    );

    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%13"]);
    assert_flag(&argv, "--source", "integration:antigravity-cli");
    // taw names this agent `agy` — its manifest, its @agent_kind and its resume command.
    assert_flag(&argv, "--agent", "agy");
    assert_flag(&argv, "--agent-session-id", "conv-1");
    assert_flag(&argv, "--agent-session-path", "/t/conv-1.json");

    // Every no-op path still emits an object: a bare exit would break the caller, which
    // expects JSON on stdout.
    for (pane, args, payload) in [
        (None, &["session"][..], &br#"{"conversationId":"c"}"#[..]),
        (
            Some("%13"),
            &["working"][..],
            &br#"{"conversationId":"c"}"#[..],
        ),
        (Some("%13"), &["session"][..], &br#"{}"#[..]),
    ] {
        reset_record(&record);
        let stdout = run_script_stdout(&script, pane, payload, &record, args, &[]);
        assert_eq!(stdout.trim(), "{}", "no-op path printed {stdout:?}");
    }
}

// =====================================================================================
// mastracode
// =====================================================================================

#[test]
fn mastracode_passes_every_shared_check() {
    let _g = lock();
    check_all("mastracode");
}

/// MastraCode keys `hooks.json` off the file root, not under a `hooks` object, and its
/// timeout is milliseconds.
#[test]
fn mastracode_install_registers_flat_root_keyed_entries() {
    let _g = lock();
    let c = case("mastracode");
    let (root, _env) = c.prepare();
    let hooks = c.config_path(root.path()).unwrap();
    fs::write(&hooks, c.config.unwrap().1).unwrap();

    install("mastracode", &root.path().join("taw-bin-placeholder")).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    assert!(
        value.get("hooks").is_none(),
        "entries must sit at the root, not under `hooks`: {value}"
    );
    let script = c.script_path(root.path()).to_string_lossy().into_owned();
    for (event, action) in [
        ("SessionStart", "session"),
        ("UserPromptSubmit", "working"),
        ("AgentStart", "working"),
        ("PreToolUse", "working"),
        ("PermissionRequest", "blocked"),
        ("PermissionResult", "working"),
        ("SubagentStart", "working"),
        ("SubagentEnd", "working"),
        ("Interrupt", "idle"),
        ("AgentEnd", "idle"),
        ("Stop", "idle"),
    ] {
        let entries = value[event]
            .as_array()
            .unwrap_or_else(|| panic!("no {event} array in {value}"));
        let ours: Vec<_> = entries
            .iter()
            .filter(|e| e["command"].as_str().is_some_and(|c| c.contains(&script)))
            .collect();
        assert_eq!(ours.len(), 1, "{event} has {} of ours", ours.len());
        assert_eq!(ours[0]["type"], "command");
        assert_eq!(ours[0]["timeout"], 10_000);
        assert!(
            ours[0]["command"]
                .as_str()
                .unwrap()
                .ends_with(&format!("' {action}")),
            "{event} runs the wrong action: {}",
            ours[0]["command"]
        );
    }
    // The user's own Stop entry is still there beside ours.
    assert_eq!(value["Stop"].as_array().unwrap().len(), 2);
}

#[test]
fn mastracode_reports_thread_identity_and_lifecycle_state() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the mastracode hook parses JSON with it");
        return;
    }
    let c = case("mastracode");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("mastracode", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%14"),
        br#"{"session_id":"thread-1"}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%14"]);
    assert_flag(&argv, "--agent", "mastracode");
    assert_flag(&argv, "--agent-session-id", "thread-1");
    assert_flag(&argv, "--session-start-source", "startup");

    // MastraCode is a lifecycle authority: every non-session action is a state report,
    // carrying the thread id when the payload has one.
    for state in ["working", "blocked", "idle"] {
        reset_record(&record);
        run_script(
            &script,
            Some("%14"),
            br#"{"session_id":"thread-1"}"#,
            &record,
            &[state],
            &[],
        );
        let argv = read_one_call(&record);
        assert_eq!(&argv[0..3], &["pane", "report-agent", "%14"]);
        assert_flag(&argv, "--state", state);
        assert_flag(&argv, "--agent-session-id", "thread-1");
    }

    // A state report with no thread id still reports the state.
    reset_record(&record);
    run_script(&script, Some("%14"), br#"{}"#, &record, &["idle"], &[]);
    let argv = read_one_call(&record);
    assert_flag(&argv, "--state", "idle");
    assert_no_flag(&argv, "--agent-session-id");

    // …but a session report with no thread id is not worth making.
    reset_record(&record);
    run_script(&script, Some("%14"), br#"{}"#, &record, &["session"], &[]);
    assert!(read_calls(&record).is_empty());
}

// =====================================================================================
// kimi
// =====================================================================================

#[test]
fn kimi_passes_every_shared_check() {
    let _g = lock();
    check_all("kimi");
}

/// The block is delimited and appended, so the user's own `[[hooks]]` table above it is
/// untouched and every taw table sits inside the markers.
#[test]
fn kimi_install_appends_a_delimited_block_of_hook_tables() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    fs::write(&config, c.config.unwrap().1).unwrap();

    install("kimi", &root.path().join("taw-bin-placeholder")).unwrap();
    let text = fs::read_to_string(&config).unwrap();

    let (before, block) = text.split_once("# >>> taw integration >>>").unwrap();
    let (block, after) = block.split_once("# <<< taw integration <<<").unwrap();
    assert!(before.contains("model = \"k2\""), "{before}");
    assert!(
        before.contains("echo theirs"),
        "the user's own hook was moved or lost"
    );
    assert_eq!(after.trim(), "");
    assert_eq!(block.matches("[[hooks]]").count(), 12);

    // Each action reaches the right event, and the AskUserQuestion tool is told apart
    // from every other tool by matcher.
    for (event, action) in [
        ("SessionStart", "session"),
        ("UserPromptSubmit", "working"),
        ("PermissionRequest", "blocked"),
        ("Stop", "idle"),
        ("Interrupt", "idle"),
    ] {
        let table = block
            .split("[[hooks]]")
            .find(|t| t.contains(&format!("event = \"{event}\"")))
            .unwrap_or_else(|| panic!("no table for {event} in {block}"));
        assert!(table.contains(&format!("' {action}\"")), "{event}: {table}");
        assert!(table.contains("timeout = 10"), "{event}: {table}");
    }
    assert!(block.contains(r#"matcher = "^AskUserQuestion$""#));
    assert!(block.contains(r#"matcher = "^(?!AskUserQuestion$).*$""#));
}

/// A config.toml that is not text at all cannot be edited: the install is refused with an
/// error naming the file, nothing is written (the hook script included), and the bytes are
/// left as they were.
#[test]
fn kimi_reports_a_config_toml_it_cannot_read_as_text() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    let invalid = [b'm', b'=', 0xff, 0xfe];
    fs::write(&config, invalid).unwrap();

    assert_refused_without_writing_anything(c, root.path(), "config.toml");
    assert_eq!(fs::read(&config).unwrap(), invalid);
}

/// Kimi's block is `[[hooks]]` tables at the end of `config.toml`. A file that already holds
/// `hooks` as anything but a list of tables would not parse with them added, and a file that
/// does not parse is one Kimi cannot load: the install writes nothing and says so.
#[test]
fn kimi_refuses_a_config_toml_its_hook_tables_would_break() {
    let _g = lock();
    let c = case("kimi");
    for theirs in [
        "model = \"k2\"\nhooks = []\n",
        "model = \"k2\"\n\n[hooks]\ntimeout = 5\n",
        "[hooks.extra]\ntimeout = 5\n",
    ] {
        let (root, _env) = c.prepare();
        let config = c.config_path(root.path()).unwrap();
        fs::write(&config, theirs).unwrap();

        assert_refused_without_writing_anything(c, root.path(), "config.toml");

        assert_eq!(fs::read_to_string(&config).unwrap(), theirs);
    }
}

/// An older taw appended its `[[hooks]]` block to a file that already held `hooks = []`: Kimi
/// cannot load the file, and status must not call the install current: it says so, and
/// an install takes taw's block out (it is what stops Kimi) before it refuses, keeping every
/// other byte.
#[test]
fn kimi_status_and_install_deal_with_a_config_an_older_taw_broke() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    let taw = root.path().join("taw-bin-placeholder");
    install("kimi", &taw).unwrap();
    let block = fs::read_to_string(&config).unwrap();
    assert!(block.starts_with("# >>> taw integration >>>"), "{block}");
    assert!(!status("kimi").unwrap().outdated());

    let theirs = "model = \"k2\"\nhooks = []\n";
    fs::write(&config, format!("{theirs}\n\n{block}")).unwrap();
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .parse::<toml::Table>()
            .is_err(),
        "the fixture must be a file Kimi cannot load"
    );

    let st = status("kimi").unwrap();
    let Some(taw::integration::Incomplete::Reinstall(note)) = &st.incomplete else {
        panic!("{:?}", st.incomplete);
    };
    assert!(st.outdated());
    assert!(note.contains("cannot load"), "{note}");
    assert!(note.contains("older taw"), "{note}");
    assert!(note.contains("taw integration install kimi"), "{note}");

    let err = install("kimi", &taw).expect_err("the install still refuses");
    let message = format!("{err:#}");
    assert!(message.contains("took its older block out"), "{message}");
    assert!(message.contains("hooks"), "{message}");
    // The lead says taw took its block out and changed nothing else: it does not also say the file
    // is left as it is.
    assert!(!message.contains("left as it is"), "{message}");
    assert_eq!(fs::read_to_string(&config).unwrap(), theirs);

    // Repaired, and nothing left to reinstall: Kimi loads the file, taw's hook is not in it.
    let st = status("kimi").unwrap();
    assert!(!st.outdated(), "{:?}", st.incomplete);
}

/// The repair keeps the file's final newline even when only one blank line stood before the block.
#[test]
fn kimi_repair_keeps_the_final_newline_of_the_file() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    let taw = root.path().join("taw-bin-placeholder");
    install("kimi", &taw).unwrap();
    let block = fs::read_to_string(&config).unwrap();
    let theirs = "model = \"k2\"\nhooks = []\n";
    fs::write(&config, format!("{theirs}\n{block}")).unwrap();

    install("kimi", &taw).expect_err("the install still refuses");

    assert_eq!(fs::read_to_string(&config).unwrap(), theirs);
}

/// A Kimi file that does not parse for a reason of the user's own is not called taw's doing.
#[test]
fn kimi_status_does_not_blame_taw_for_a_config_that_is_broken_by_itself() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    install("kimi", &root.path().join("taw-bin-placeholder")).unwrap();
    let block = fs::read_to_string(&config).unwrap();
    // Their own mistake, and taw's block as well: the file is broken without the block.
    fs::write(&config, format!("model = \n\n\n{block}")).unwrap();
    assert!(status("kimi").unwrap().incomplete.is_none());
    fs::write(&config, "model = \n").unwrap();

    let st = status("kimi").unwrap();

    assert!(st.incomplete.is_none(), "{:?}", st.incomplete);
}

/// Hermes: the shapes the older, unchecked line edit broke and taw can recognise by its own
/// lines are reported by status, repaired by an install (which then still refuses, because the
/// file's `plugins` is a shape the edit does not read), and only taw's lines go.
#[test]
fn hermes_status_and_install_deal_with_a_config_an_older_taw_broke() {
    let _g = lock();
    let c = case("hermes");
    for theirs in [
        "model: sonnet\nplugins: null\nmore: 1\n",
        "plugins:\n  enabled: null\n",
        "model: sonnet\n\"plugins\":\n  enabled:\n    - other\n",
    ] {
        let (root, _env) = c.prepare();
        let config = c.config_path(root.path()).unwrap();
        let taw = root.path().join("taw-bin-placeholder");
        // The older install's result: its lines in the user's file, and its plugin files.
        fs::write(&config, "model: sonnet\n").unwrap();
        install("hermes", &taw).unwrap();
        fs::write(&config, broken_by_older_edit(theirs)).unwrap();

        let st = status("hermes").unwrap();
        let Some(taw::integration::Incomplete::Reinstall(note)) = &st.incomplete else {
            panic!("{theirs:?}: {:?}", st.incomplete);
        };
        assert!(st.outdated(), "{theirs:?}");
        assert!(note.contains("config.yaml"), "{note}");
        assert!(note.contains("older taw"), "{note}");

        let err = install("hermes", &taw).expect_err("refused");
        let message = format!("{err:#}");
        assert!(message.contains("took its older"), "{theirs:?}: {message}");
        assert!(!message.contains("left as it is"), "{theirs:?}: {message}");
        assert_eq!(fs::read_to_string(&config).unwrap(), theirs);

        assert!(!status("hermes").unwrap().outdated(), "{theirs:?}");
    }
}

/// What the older line edit made of `theirs`: the lines it inserted, by the rules it had.
fn broken_by_older_edit(theirs: &str) -> String {
    match theirs {
        "model: sonnet\nplugins: null\nmore: 1\n" => {
            "model: sonnet\nplugins: null\n  enabled:\n    - taw-agent-state\nmore: 1\n".into()
        }
        "plugins:\n  enabled: null\n" => {
            "plugins:\n  enabled: null\n    - taw-agent-state\n".into()
        }
        _ => format!("{theirs}plugins:\n  enabled:\n    - taw-agent-state\n"),
    }
}

/// A `config.toml` that does not parse is left alone, with the line named and none of its text.
#[test]
fn kimi_leaves_a_config_toml_that_does_not_parse_alone() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    let broken = "token = \"sk-secret-value\"\nmodel = \n";
    fs::write(&config, broken).unwrap();

    let before = tree(root.path());
    let err = install("kimi", &root.path().join("taw-bin-placeholder")).expect_err("refused");
    let message = format!("{err:#}");

    assert!(message.contains("does not parse"), "{message}");
    assert!(message.contains("line 2"), "{message}");
    assert!(!message.contains("sk-secret-value"), "{message}");
    assert_eq!(fs::read_to_string(&config).unwrap(), broken);
    assert_eq!(tree(root.path()), before);
}

/// The text-edit path of the same rule as Claude's: through a dotfiles link that pointed at
/// nothing when kimi was installed, uninstall removes the file the link points at, not
/// the link.
#[cfg(unix)]
#[test]
fn kimi_uninstall_through_a_dangling_dotfiles_link_removes_the_file_and_keeps_the_link() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let dotfiles = tempfile::tempdir().unwrap();
    let target = dotfiles.path().join("kimi-config.toml");
    let link = c.config_path(root.path()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    install("kimi", &root.path().join("taw-bin-placeholder")).unwrap();
    assert!(fs::read_to_string(&target).unwrap().contains("[[hooks]]"));

    uninstall("kimi").unwrap();

    assert!(!target.exists(), "the file behind the link must go");
    assert_eq!(fs::read_link(&link).unwrap(), target);
}

/// A config.toml that already carries a taw block when taw first sees it: a dotfiles copy
/// from another machine, or what an earlier taw left behind. The copy install saves of
/// that file holds the block too, so putting those bytes back on uninstall would restore a
/// block that points at a hook script the same uninstall deletes. The block is taken out
/// instead, and the user's own text around it is left exactly as it was.
#[test]
fn kimi_uninstall_removes_a_taw_block_that_was_in_the_config_before_the_first_install() {
    let _g = lock();
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    let theirs = c.config.unwrap().1;
    let from_another_machine = "# >>> taw integration >>>\n\
        [[hooks]]\n\
        event = \"Stop\"\n\
        command = \"bash '/Users/other/.kimi-code/hooks/taw-agent-state.sh' idle\"\n\
        timeout = 10\n\
        \n\
        # <<< taw integration <<<\n";
    fs::write(&config, format!("{theirs}\n\n{from_another_machine}")).unwrap();

    install("kimi", &root.path().join("taw-bin-placeholder")).unwrap();
    let installed = fs::read_to_string(&config).unwrap();
    assert_eq!(
        installed.matches("# >>> taw integration >>>").count(),
        1,
        "install replaces the block it found with its own: {installed}"
    );
    assert!(
        installed.contains(&c.script_path(root.path()).display().to_string()),
        "the block that is left is this install's: {installed}"
    );

    uninstall("kimi").unwrap();

    let reverted = fs::read_to_string(&config).unwrap();
    assert_eq!(
        reverted, theirs,
        "taw's block must be gone and the user's own text intact: {reverted}"
    );
    assert!(!c.script_path(root.path()).exists());
    let mut saved_copy = c.script_path(root.path()).into_os_string();
    saved_copy.push(".orig.toml");
    assert!(
        !Path::new(&saved_copy).exists(),
        "the saved copy is dropped with the install"
    );
}

/// herdr refuses to install into a Kimi Code older than 0.14.0, because these hook entries
/// would not work there and writing them would look like success. A version it cannot
/// determine is a warning on an install that still happens.
#[test]
fn kimi_install_is_gated_on_the_minimum_kimi_version() {
    let _g = lock();
    let c = case("kimi");

    // Too old: refused, and nothing is written.
    let root = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set(c.env_var, &root.path().join(c.env_value));
    fs::create_dir_all(root.path().join(c.config_dir)).unwrap();
    let bin = root.path().join("old-bin");
    fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("kimi");
    fs::write(&stub, "#!/bin/sh\nprintf 'kimi 0.13.9\\n'\n").unwrap();
    make_executable(&stub);
    let _path = EnvGuard::prepend_path(&bin);

    let err = install("kimi", &root.path().join("taw-bin-placeholder"))
        .expect_err("an old kimi must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("0.13.9") && text.contains("0.14.0"), "{text}");
    assert!(!c.script_path(root.path()).exists());
    assert!(!c.config_path(root.path()).unwrap().exists());

    // Unknown: installed, and the report says the version could not be verified. The
    // stub exits non-zero, standing in for a kimi that is absent or cannot be run.
    fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
    make_executable(&stub);
    let report = install("kimi", &root.path().join("taw-bin-placeholder")).expect("install");
    assert!(report.message.contains("warning:"), "{}", report.message);
    assert!(report.message.contains("0.14.0"), "{}", report.message);
    assert!(c.script_path(root.path()).exists());
}

#[test]
fn kimi_reports_session_identity_and_lifecycle_state() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the kimi hook parses JSON with it");
        return;
    }
    let c = case("kimi");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("kimi", &fake).unwrap();
    let script = c.script_path(root.path());
    bash_n(&script);
    let record = root.path().join("taw-calls");

    run_script(
        &script,
        Some("%15"),
        br#"{"session_id":"km-1"}"#,
        &record,
        &["session"],
        &[],
    );
    let argv = read_one_call(&record);
    assert_eq!(&argv[0..3], &["pane", "report-agent-session", "%15"]);
    assert_flag(&argv, "--source", "integration:kimi");
    assert_flag(&argv, "--agent", "kimi");
    assert_flag(&argv, "--agent-session-id", "km-1");
    assert_flag(&argv, "--session-start-source", "startup");

    for state in ["working", "blocked", "idle"] {
        reset_record(&record);
        run_script(
            &script,
            Some("%15"),
            br#"{"session_id":"km-1"}"#,
            &record,
            &[state],
            &[],
        );
        let argv = read_one_call(&record);
        assert_eq!(&argv[0..3], &["pane", "report-agent", "%15"]);
        assert_flag(&argv, "--state", state);
        assert_flag(&argv, "--agent-session-id", "km-1");
    }
}

// =====================================================================================
// kilo
// =====================================================================================

#[test]
fn kilo_passes_every_shared_check() {
    let _g = lock();
    check_all("kilo");
}

/// Kilo's plugin directory is `plugin/`, singular — unlike OpenCode's `plugins/`. Getting
/// it wrong installs a file Kilo never loads, and `status` would still call it installed.
#[test]
fn kilo_installs_into_the_singular_plugin_directory() {
    let _g = lock();
    let c = case("kilo");
    let (root, _env) = c.prepare();
    let report = install("kilo", &root.path().join("taw-bin-placeholder")).unwrap();

    assert_eq!(
        report.files,
        vec![root.path().join("kilo/plugin/taw-agent-state.js")]
    );
    assert!(!root.path().join("kilo/plugins").exists());
}

/// The plugin is the pane's state authority, so it must map Kilo's events onto all three
/// states and report session identity — checked by running it under node when node is
/// available.
#[test]
fn kilo_plugin_maps_events_onto_states_and_session_identity() {
    let _g = lock();
    let c = case("kilo");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("kilo", &fake).unwrap();
    let plugin = c.script_path(root.path());

    if !have("node") {
        eprintln!("skipping the behavioural half: node is not on PATH");
        // The static half still holds: every event Kilo emits has to appear in the file.
        let text = fs::read_to_string(&plugin).unwrap();
        for event in [
            "session.created",
            "session.updated",
            "session.status",
            "tool.execute.before",
            "permission.asked",
            "question.asked",
            "session.error",
            "session.idle",
            "session.compacted",
            "chat.message",
        ] {
            assert!(text.contains(event), "{event} is not handled");
        }
        return;
    }

    // Drive the plugin's own event handler with the payloads Kilo emits.
    let driver = root.path().join("drive.mjs");
    fs::write(
        &driver,
        format!(
            r#"import {{ TawAgentStatePlugin }} from "{plugin}";
const plugin = await TawAgentStatePlugin({{}});
const events = JSON.parse(process.argv[2]);
for (const event of events) {{
    if (event.chat) {{
        await plugin["chat.message"]({{ sessionID: event.chat }});
    }} else {{
        await plugin.event({{ event }});
    }}
}}
"#,
            plugin = plugin.display()
        ),
    )
    .unwrap();

    let events = serde_json::json!([
        { "type": "session.created", "properties": { "sessionID": "kl-1" } },
        { "type": "session.idle", "properties": { "sessionID": "kl-1" } },
        { "type": "permission.asked", "properties": { "sessionID": "kl-1" } },
        { "type": "tool.execute.before", "properties": { "sessionID": "kl-1" } },
        { "type": "session.status", "properties": { "sessionID": "kl-1", "status": "busy" } },
        { "type": "session.deleted", "properties": { "sessionID": "kl-1" } },
        { "chat": "kl-1" },
    ])
    .to_string();

    let record = root.path().join("taw-calls");
    let output = Command::new("node")
        .arg(&driver)
        .arg(&events)
        .env("TMUX_PANE", "%16")
        .env_remove("TAW_REPORT_FILE")
        .env("FAKE_TAW_RECORD", &record)
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "node driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // execFile is fire-and-forget, so wait for the calls to land rather than assume.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while read_calls(&record).len() < 6 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    let calls = read_calls(&record);
    let states: Vec<String> = calls
        .iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("report-agent"))
        .filter_map(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            c.get(i + 1).cloned()
        })
        .collect();
    let sessions: Vec<&Vec<String>> = calls
        .iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("report-agent-session"))
        .collect();

    assert_eq!(
        sessions.len(),
        1,
        "session.created must report identity once: {calls:?}"
    );
    assert_flag(sessions[0], "--agent", "kilo");
    assert_flag(sessions[0], "--agent-session-id", "kl-1");
    assert_flag(sessions[0], "--session-start-source", "startup");
    for state in ["idle", "blocked", "working"] {
        assert!(
            states.contains(&state.to_string()),
            "no {state} report among {states:?}"
        );
    }
    // session.deleted reports nothing, so exactly the six calls above landed.
    assert_eq!(calls.len(), 6, "{calls:?}");
    // Every state report carries the pane and the session id.
    for call in &calls {
        assert_eq!(call[2], "%16");
        assert_flag(call, "--agent-session-id", "kl-1");
        assert_seq_present(call);
    }
}

// =====================================================================================
// hermes
// =====================================================================================

#[test]
fn hermes_passes_every_shared_check() {
    let _g = lock();
    check_all("hermes");
}

/// Both plugin files land in a directory named for the plugin, and that name has to match
/// the `name:` in the manifest and the entry added to `plugins.enabled` — Hermes will not
/// load it otherwise.
#[test]
fn hermes_install_writes_the_plugin_and_enables_it_under_one_agreed_name() {
    let _g = lock();
    let c = case("hermes");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    fs::write(&config, c.config.unwrap().1).unwrap();

    let report = install("hermes", &root.path().join("taw-bin-placeholder")).unwrap();

    let dir = root.path().join("hermes/plugins/taw-agent-state");
    assert!(dir.join("__init__.py").is_file());
    assert!(dir.join("plugin.yaml").is_file());
    assert_eq!(
        report.files,
        vec![
            dir.join("__init__.py"),
            dir.join("plugin.yaml"),
            config.clone()
        ]
    );
    assert!(
        report.message.contains("restart hermes"),
        "the report must say the plugin needs a restart: {}",
        report.message
    );
    assert!(
        fs::read_to_string(dir.join("plugin.yaml"))
            .unwrap()
            .contains("name: taw-agent-state")
    );

    let yaml = fs::read_to_string(&config).unwrap();
    assert!(yaml.contains("- taw-agent-state"), "{yaml}");
    assert!(
        yaml.contains("- other"),
        "the user's own plugin was lost: {yaml}"
    );
    assert!(
        yaml.starts_with("# my hermes"),
        "a comment was lost: {yaml}"
    );
}

/// `plugins` written in a way taw's line edit does not know (a flow mapping, a null, a quoted key,
/// a different indent, a list at the key's own indent, an `enabled` that is not a list) is
/// refused: the edit would add a second `plugins` key or a line that breaks the file's
/// structure, and Hermes would lose the user's plugins or refuse the file.
#[test]
fn hermes_refuses_a_plugins_key_its_line_edit_would_break() {
    let _g = lock();
    let c = case("hermes");
    for theirs in [
        "plugins: {enabled: [other]}\n",
        "plugins: null\nmodel: sonnet\n",
        "plugins: ~\n",
        "\"plugins\":\n  enabled:\n    - other\n",
        "'plugins':\n  enabled:\n    - other\n",
        "plugins:\n    enabled:\n      - other\n",
        "plugins:\n- other\n",
        "plugins:\n  enabled: null\n",
        "plugins:\n  enabled: {a: 1}\n",
    ] {
        let (root, _env) = c.prepare();
        let config = c.config_path(root.path()).unwrap();
        fs::write(&config, theirs).unwrap();

        assert_refused_without_writing_anything(c, root.path(), "config.yaml");

        assert_eq!(fs::read_to_string(&config).unwrap(), theirs);
    }
}

/// The spellings the edit does know are still edited.
#[test]
fn hermes_still_edits_the_plugins_spellings_it_knows() {
    let _g = lock();
    let c = case("hermes");
    for theirs in [
        "model: sonnet\n",
        "plugins:\n",
        "plugins: # none yet\n",
        "plugins: []\n",
        "plugins: [other]\n",
        "plugins:\n  - other\n",
        "plugins:\n  enabled:\n    - other\n",
        "plugins:\n  enabled: [other]\n",
        "plugins:\n  enabled: []\n",
        "plugins:\n  disabled:\n    - x\n",
        "# only a comment\nplugins:\n  # a note\n  enabled:\n    - other\n",
    ] {
        let (root, _env) = c.prepare();
        let config = c.config_path(root.path()).unwrap();
        fs::write(&config, theirs).unwrap();

        install("hermes", &root.path().join("taw-bin-placeholder")).unwrap_or_else(|err| {
            panic!("{theirs:?}: {err:#}");
        });

        let edited = fs::read_to_string(&config).unwrap();
        assert!(edited.contains("taw-agent-state"), "{theirs:?}: {edited}");
    }
}

/// Uninstall takes back only what install put there: a file somebody else left in the
/// plugin directory keeps the directory alive rather than being deleted with it.
#[test]
fn hermes_uninstall_keeps_a_file_it_did_not_write() {
    let _g = lock();
    let c = case("hermes");
    let (root, _env) = c.prepare();
    install("hermes", &root.path().join("taw-bin-placeholder")).unwrap();

    let dir = root.path().join("hermes/plugins/taw-agent-state");
    let theirs = dir.join("notes.md");
    fs::write(&theirs, "mine\n").unwrap();

    uninstall("hermes").unwrap();
    assert!(!dir.join("__init__.py").exists());
    assert!(!dir.join("plugin.yaml").exists());
    assert!(theirs.is_file(), "a file taw never wrote was deleted");

    // With nothing of anyone else's in it, the directory goes too.
    fs::remove_file(&theirs).unwrap();
    install("hermes", &root.path().join("taw-bin-placeholder")).unwrap();
    uninstall("hermes").unwrap();
    assert!(
        !dir.exists(),
        "an empty plugin directory must not be left behind"
    );
}

/// A config.yaml that already lists the plugin when taw first sees it (a dotfiles copy from
/// another machine). The copy install saves of it lists the plugin too, so restoring those
/// bytes on uninstall would leave Hermes told to load a plugin whose directory the same
/// uninstall removes. The entry is taken out, and the user's other plugins, comments and
/// keys stay.
#[test]
fn hermes_uninstall_removes_the_plugin_entry_that_was_in_the_config_before_the_first_install() {
    let _g = lock();
    let c = case("hermes");
    let (root, _env) = c.prepare();
    let config = c.config_path(root.path()).unwrap();
    fs::write(
        &config,
        "# my hermes\nmodel: sonnet\nplugins:\n  enabled:\n    - taw-agent-state\n    - other\n",
    )
    .unwrap();

    install("hermes", &root.path().join("taw-bin-placeholder")).unwrap();
    assert_eq!(
        fs::read_to_string(&config)
            .unwrap()
            .matches("taw-agent-state")
            .count(),
        1,
        "the entry that was there is not added a second time"
    );

    uninstall("hermes").unwrap();

    let reverted = fs::read_to_string(&config).unwrap();
    assert_eq!(
        reverted, "# my hermes\nmodel: sonnet\nplugins:\n  enabled:\n    - other\n",
        "taw's entry must be gone and the rest intact: {reverted}"
    );
    assert!(!c.script_path(root.path()).exists());
}

/// The Python plugin reports through the taw binary baked in at install time.
#[test]
fn hermes_plugin_reports_the_session_for_an_interactive_platform_only() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and the hermes plugin is a python module");
        return;
    }
    let c = case("hermes");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("hermes", &fake).unwrap();

    let dir = root.path().join("hermes/plugins");
    let record = root.path().join("taw-calls");
    let driver = root.path().join("drive.py");
    fs::write(
        &driver,
        r#"import importlib
import sys

sys.path.insert(0, sys.argv[1])
module = importlib.import_module("taw_agent_state_pkg")


class Ctx:
    def __init__(self):
        self.hooks = {}

    def register_hook(self, name, fn):
        self.hooks[name] = fn


ctx = Ctx()
module.register(ctx)
assert sorted(ctx.hooks) == ["on_session_reset", "on_session_start", "pre_llm_call"], ctx.hooks
ctx.hooks["on_session_start"](platform="tui", session_id="hm-1")
# Not interactive: no report.
ctx.hooks["on_session_start"](platform="http", session_id="hm-2")
# No session id: no report.
ctx.hooks["on_session_start"](platform="tui")
# pre_llm_call reports a resume, but only on the cli platform.
ctx.hooks["pre_llm_call"](platform="cli", session_id="hm-3")
ctx.hooks["pre_llm_call"](platform="tui", session_id="hm-4")
ctx.hooks["on_session_reset"](platform="tui", session_id="hm-5")
"#,
    )
    .unwrap();

    // Import it under an identifier-safe package name (the installed directory has
    // hyphens, which python cannot import).
    let importable = root.path().join("py/taw_agent_state_pkg");
    fs::create_dir_all(&importable).unwrap();
    fs::copy(
        dir.join("taw-agent-state/__init__.py"),
        importable.join("__init__.py"),
    )
    .unwrap();

    let output = Command::new("python3")
        .arg(&driver)
        .arg(root.path().join("py"))
        .env("TMUX_PANE", "%17")
        .env_remove("TAW_REPORT_FILE")
        .env("FAKE_TAW_RECORD", &record)
        .output()
        .expect("run python3");
    assert!(
        output.status.success(),
        "python driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = read_calls(&record);
    assert_eq!(calls.len(), 3, "{calls:?}");
    for (call, (session, start)) in
        calls
            .iter()
            .zip([("hm-1", "startup"), ("hm-3", "resume"), ("hm-5", "new")])
    {
        assert_eq!(&call[0..3], &["pane", "report-agent-session", "%17"]);
        assert_flag(call, "--source", "integration:hermes");
        assert_flag(call, "--agent", "hermes");
        assert_flag(call, "--agent-session-id", session);
        assert_flag(call, "--session-start-source", start);
        assert_seq_present(call);
    }
}

/// The state of the most recent `report-agent` call in `calls`, if any.
fn last_state(calls: &[Vec<String>]) -> Option<String> {
    calls
        .iter()
        .rev()
        .find(|c| c[1] == "report-agent")
        .and_then(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            c.get(i + 1).cloned()
        })
}

/// Loads a pi/omp extension under node and runs `body` against it, returning the taw
/// calls it made in `--seq` order.
///
/// `body` runs with `pi` (an extension host with an event emitter and the handlers the
/// extension registered), `fire(event, payload, ctx)` to deliver one of those handlers,
/// `settle()` to let the extension's report queue drain, and `tui` — a context standing in
/// for an interactive pi session with a session file.
///
/// `settle()` matters: the report queue coalesces, so two transitions inside one tick are
/// sent as one report (the later). That is herdr's behaviour and
/// [`pi_coalesces_transitions_that_happen_while_a_report_is_in_flight`] pins it; a test
/// that wants to observe an intermediate state has to wait for it.
fn drive_pi_extension(root: &Path, extension: &Path, pane: &str, body: &str) -> Vec<Vec<String>> {
    drive_pi_extension_with_env(root, extension, pane, body, &[])
}

fn drive_pi_extension_with_env(
    root: &Path,
    extension: &Path,
    pane: &str,
    body: &str,
    extra_env: &[(&str, &str)],
) -> Vec<Vec<String>> {
    let record = root.join("taw-calls");
    reset_record(&record);
    let driver = root.join("drive-pi.mjs");
    fs::write(
        &driver,
        format!(
            r#"import {{ EventEmitter }} from "node:events";
import register from "{extension}";

const handlers = new Map();
const pi = {{
    events: new EventEmitter(),
    on: (event, fn) => handlers.set(event, fn),
}};
const tui = {{
    mode: "tui",
    hasUI: true,
    isIdle: () => false,
    sessionManager: {{
        getSessionId: () => "pi-session-id",
        getSessionFile: () => "/tmp/pi-session.json",
    }},
}};
register(pi);
const fire = async (event, payload, ctx) => {{
    const fn = handlers.get(event);
    if (fn) await fn(payload, ctx);
}};
// Wait for the extension's report queue to drain. Its queue coalesces — only the most
// recent state is sent — so two transitions in one tick land as one report.
const settle = () => new Promise((r) => setTimeout(r, 120));
{body}
// Let the extension's serialised report queue drain before exiting.
await new Promise((r) => setTimeout(r, 400));
"#,
            extension = extension.display(),
            body = body
        ),
    )
    .unwrap();

    let mut cmd = Command::new("node");
    cmd.arg(&driver)
        .env("TMUX_PANE", pane)
        .env_remove("TAW_REPORT_FILE")
        .env("FAKE_TAW_RECORD", &record)
        // herdr #4351's lesson: `Command` inherits the whole environment, so a `cargo
        // test` process launched from inside an OMP shell would otherwise carry
        // `OMPCODE=1` into every one of these drives (pi's included) and, once OMP's own
        // extension honours it, make its every test look like a nested session.
        .env_remove("OMPCODE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("run node");
    assert!(
        output.status.success(),
        "node driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_calls(&record)
}

// =====================================================================================
// pi
// =====================================================================================

#[test]
fn pi_passes_every_shared_check() {
    let _g = lock();
    // `$PI_CODING_AGENT_DIR` is the *complete* agent directory, so `extensions/` under it
    // is created by the install rather than required — but the agent directory itself is
    // required, which is what the shared missing-config-dir check exercises.
    check_all("pi");
}

/// The states pi's own events map onto, and the session reference that goes with them.
///
/// Each transition is observed in its own run, ending on the state under test: the report
/// queue coalesces, so a state that is superseded before its report goes out is never sent
/// — see [`pi_coalesces_transitions_that_happen_while_a_report_is_in_flight`], which pins
/// that on purpose. Asserting a long sequence in one run would be asserting how fast the
/// machine happened to be.
#[test]
fn pi_extension_drives_pi_events_onto_states_and_a_session_reference() {
    let _g = lock();
    let c = case("pi");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("pi", &fake).unwrap();
    let extension = c.script_path(root.path());

    if !have("node") {
        eprintln!("skipping the behavioural half: node is not on PATH");
        // The static half still holds: every event pi emits has to be handled, and the
        // headless-mode gate has to be there.
        let text = fs::read_to_string(&extension).unwrap();
        for needle in [
            "session_start",
            "agent_start",
            "agent_settled",
            "taw:blocked",
            "ctx?.mode !== \"tui\"",
        ] {
            assert!(text.contains(needle), "{needle} is missing");
        }
        return;
    }

    // A headless pi (RPC/JSON/print) has no pane to show, so nothing is reported at all.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%20",
        r#"await fire("session_start", { reason: "startup" }, { mode: "print" });"#,
    );
    assert!(calls.is_empty(), "a headless session reported: {calls:?}");

    // session_start reports identity, then the state pi is already in.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%20",
        r#"await fire("session_start", { reason: "startup" }, tui);"#,
    );
    assert_eq!(&calls[0][0..3], &["pane", "report-agent-session", "%20"]);
    assert_flag(&calls[0], "--source", "integration:pi");
    assert_flag(&calls[0], "--agent", "pi");
    // The session *path* is preferred over the id: pi resumes from either, and a path
    // survives a session whose id no longer resolves.
    assert_flag(&calls[0], "--agent-session-path", "/tmp/pi-session.json");
    assert_no_flag(&calls[0], "--agent-session-id");
    assert_flag(&calls[0], "--session-start-source", "startup");
    assert_eq!(last_state(&calls), Some("working".to_string()), "{calls:?}");

    // agent_start refreshes the session reference as well as the state.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%20",
        r#"
        await fire("session_start", { reason: "startup" }, { ...tui, isIdle: () => true });
        await settle();
        await fire("agent_start", {}, tui);
        "#,
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| c[1] == "report-agent-session")
            .count(),
        2,
        "agent_start must re-report the session reference: {calls:?}"
    );
    assert_eq!(last_state(&calls), Some("working".to_string()), "{calls:?}");

    // A block carries its label as the report message.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%20",
        r#"
        await fire("session_start", { reason: "startup" }, { ...tui, isIdle: () => true });
        await settle();
        pi.events.emit("taw:blocked", { active: true, label: "approve?" });
        "#,
    );
    let blocked = calls
        .iter()
        .rev()
        .find(|c| c[1] == "report-agent")
        .expect("a state report");
    assert_flag(blocked, "--state", "blocked");
    assert_flag(blocked, "--message", "approve?");

    // agent_settled goes idle, but only once pi itself says it is idle.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%20",
        r#"
        await fire("session_start", { reason: "startup" }, tui);
        await settle();
        await fire("agent_settled", {}, tui);
        await settle();
        await fire("agent_settled", {}, { ...tui, isIdle: () => true });
        "#,
    );
    assert_eq!(last_state(&calls), Some("idle".to_string()), "{calls:?}");
}

/// With no session file to hand, the id is reported instead.
#[test]
fn pi_falls_back_to_the_session_id_when_there_is_no_session_file() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("pi");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("pi", &fake).unwrap();

    drive_pi_extension(
        root.path(),
        &c.script_path(root.path()),
        "%21",
        r#"
        await fire("session_start", { reason: "resume" }, {
            mode: "tui",
            isIdle: () => true,
            sessionManager: { getSessionId: () => "pi-1", getSessionFile: () => undefined },
        });
        "#,
    );
    let calls = wait_for_calls(&root.path().join("taw-calls"), 1);
    assert!(
        !calls.is_empty(),
        "pi's extension recorded no taw call at all"
    );
    assert_flag(&calls[0], "--agent-session-id", "pi-1");
    assert_no_flag(&calls[0], "--agent-session-path");
    assert_flag(&calls[0], "--session-start-source", "resume");
}

// =====================================================================================
// omp
// =====================================================================================

#[test]
fn omp_passes_every_shared_check() {
    let _g = lock();
    check_all("omp");
}

/// `$PI_CODING_AGENT_DIR` is the complete agent directory for whichever agent it is set
/// for, so with it set both resolve to the same `extensions/` — and an OMP extension
/// installed there would be loaded by Pi too. herdr refuses; so does this.
#[test]
fn omp_refuses_to_install_into_pis_extension_directory() {
    let _g = lock();
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared-agent");
    fs::create_dir_all(&shared).unwrap();
    let _env = EnvGuard::set("PI_CODING_AGENT_DIR", &shared);

    let err = install("omp", &root.path().join("taw-bin-placeholder"))
        .expect_err("a shared extensions directory must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("pi and omp"), "{text}");
    assert!(text.contains("PI_CODING_AGENT_DIR"), "{text}");
    assert!(
        !shared.join("extensions").exists(),
        "a refused install still wrote a file"
    );

    // Pi on its own is fine in that same directory.
    install("pi", &root.path().join("taw-bin-placeholder")).expect("pi install");
    assert!(shared.join("extensions/taw-agent-state.ts").is_file());
}

/// The OMP install clears a taw `pi` extension out of OMP's own directory — but only one
/// taw wrote. Anything else with that name is somebody else's file and stays.
#[test]
fn omp_removes_only_a_taw_pi_extension_from_its_own_directory() {
    let _g = lock();
    let c = case("omp");
    let (root, _env) = c.prepare();
    let extensions = root.path().join(".omp/agent/extensions");
    fs::create_dir_all(&extensions).unwrap();
    let legacy = extensions.join("taw-agent-state.ts");

    // Ours: removed, and the report says so.
    fs::write(&legacy, "// TAW_INTEGRATION_AGENT=pi\n").unwrap();
    let report = install("omp", &root.path().join("taw-bin-placeholder")).unwrap();
    assert!(
        !legacy.exists(),
        "a taw pi extension should have been removed"
    );
    assert!(
        report.message.contains("removed a taw pi extension"),
        "{}",
        report.message
    );
    uninstall("omp").unwrap();

    // Not ours: kept, and the report says nothing about it.
    let theirs = "// someone else's extension\n";
    fs::write(&legacy, theirs).unwrap();
    let report = install("omp", &root.path().join("taw-bin-placeholder")).unwrap();
    assert_eq!(fs::read_to_string(&legacy).unwrap(), theirs);
    assert!(!report.message.contains("removed"), "{}", report.message);
}

/// OMP holds the pane in Working through its own auto-retry of a provider error, rather
/// than flickering to Idle and back. The grace window is configurable so this can check
/// both sides of it without waiting seconds.
#[test]
fn omp_holds_working_through_a_retryable_provider_error_then_blocks() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("omp");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("omp", &fake).unwrap();

    let calls = drive_pi_extension_with_env(
        root.path(),
        &c.script_path(root.path()),
        "%22",
        r#"
        await fire("session_start", {}, tui);
        await fire("agent_start", {}, tui);
        await fire("agent_end", {
            messages: [{ role: "assistant", stopReason: "error", errorMessage: "overloaded" }],
        }, tui);
        await new Promise((r) => setTimeout(r, 200));
        "#,
        // A 25 ms grace window, so the hold expires inside the test.
        &[
            ("TAW_OMP_RETRY_GRACE_MS", "25"),
            ("TAW_OMP_IDLE_DEBOUNCE_MS", "5"),
        ],
    );

    let states: Vec<&str> = calls
        .iter()
        .filter(|c| c[1] == "report-agent")
        .filter_map(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            Some(c[i + 1].as_str())
        })
        .collect();
    assert_eq!(
        states,
        vec!["working", "blocked"],
        "a retryable error must hold Working and only then go Blocked: {calls:?}"
    );
    // An unretryable end goes Idle instead, after the debounce.
    let calls = drive_pi_extension_with_env(
        root.path(),
        &c.script_path(root.path()),
        "%22",
        r#"
        await fire("session_start", {}, tui);
        await fire("agent_start", {}, tui);
        await fire("agent_end", {
            messages: [{ role: "assistant", stopReason: "end_turn" }],
        }, tui);
        await new Promise((r) => setTimeout(r, 200));
        "#,
        &[
            ("TAW_OMP_RETRY_GRACE_MS", "25"),
            ("TAW_OMP_IDLE_DEBOUNCE_MS", "5"),
        ],
    );
    let states: Vec<&str> = calls
        .iter()
        .filter(|c| c[1] == "report-agent")
        .filter_map(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            Some(c[i + 1].as_str())
        })
        .collect();
    assert_eq!(states, vec!["working", "idle"], "{calls:?}");
}

/// herdr's v0.9.0 `agent_end` guard: an end event carrying `willContinue: true` is not a
/// real settle — a continuation is already scheduled — so it must not flip the pane to
/// Idle. The unretryable-end case just above (`willContinue` absent) already pins the
/// "older builds omit the field and fall through as before" half of that guard.
#[test]
fn omp_agent_end_with_will_continue_stays_working() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("omp");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("omp", &fake).unwrap();

    let calls = drive_pi_extension_with_env(
        root.path(),
        &c.script_path(root.path()),
        "%23",
        r#"
        await fire("session_start", {}, tui);
        await fire("agent_start", {}, tui);
        await fire("agent_end", {
            messages: [{ role: "assistant", stopReason: "end_turn" }],
            willContinue: true,
        }, tui);
        await new Promise((r) => setTimeout(r, 200));
        "#,
        &[
            ("TAW_OMP_RETRY_GRACE_MS", "25"),
            ("TAW_OMP_IDLE_DEBOUNCE_MS", "5"),
        ],
    );

    let states: Vec<&str> = calls
        .iter()
        .filter(|c| c[1] == "report-agent")
        .filter_map(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            Some(c[i + 1].as_str())
        })
        .collect();
    assert_eq!(
        states,
        vec!["working"],
        "an agent_end with willContinue must not settle to idle: {calls:?}"
    );
}

/// OMP marks every shell it spawns with `OMPCODE=1`. An `omp` launched from a parent
/// session's shell inherits it, so that nested process is not the pane's root agent and
/// must not claim the pane's session for its own short-lived conversation — ported from
/// herdr's v0.9.1 `herdr-agent-state.test.ts` ("OMP ignores nested sessions launched
/// inside another OMP shell"), which asserts zero events are even registered.
#[test]
fn omp_ignores_a_nested_session_launched_inside_another_omp_shell() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("omp");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("omp", &fake).unwrap();

    let calls = drive_pi_extension_with_env(
        root.path(),
        &c.script_path(root.path()),
        "%24",
        r#"
        await fire("session_start", {}, tui);
        await new Promise((r) => setTimeout(r, 200));
        "#,
        &[("OMPCODE", "1")],
    );

    assert!(
        calls.is_empty(),
        "a nested OMP session must never report over the parent pane's: {calls:?}"
    );
}

/// OMP's `ask` tool and its approval prompts are both Blocked, and OMP's own event names
/// for them are not Pi's.
/// OMP's `ask` tool and its approval prompts are both Blocked, and each carries its own
/// label. One run per case, ending on the block, so nothing can coalesce it away.
#[test]
fn omp_reports_blocked_for_an_approval_and_for_the_ask_tool() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("omp");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("omp", &fake).unwrap();
    let extension = c.script_path(root.path());

    for (body, message) in [
        (
            r#"
            await fire("session_start", {}, tui);
            await settle();
            await fire("tool_approval_requested", { toolName: "bash" }, tui);
            "#,
            "bash approval",
        ),
        (
            r#"
            await fire("session_start", {}, tui);
            await settle();
            await fire("tool_execution_start", {
                toolName: "ask",
                args: { questions: [{ question: "which branch?" }] },
            }, tui);
            "#,
            "which branch?",
        ),
        (
            // No question to quote: herdr's fallback label.
            r#"
            await fire("session_start", {}, tui);
            await settle();
            await fire("tool_execution_start", { toolName: "ask", args: {} }, tui);
            "#,
            "waiting for user input",
        ),
    ] {
        let calls = drive_pi_extension(root.path(), &extension, "%23", body);
        let blocked = calls
            .iter()
            .rev()
            .find(|c| c[1] == "report-agent")
            .unwrap_or_else(|| panic!("no state report for {message}: {calls:?}"));
        assert_flag(blocked, "--state", "blocked");
        assert_flag(blocked, "--message", message);
    }

    // Resolving the block goes back to working; a tool that is not `ask` is not a block.
    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%23",
        r#"
        await fire("session_start", {}, tui);
        await settle();
        await fire("tool_approval_requested", { toolName: "bash" }, tui);
        await settle();
        await fire("tool_approval_resolved", {}, tui);
        "#,
    );
    assert_eq!(last_state(&calls), Some("working".to_string()), "{calls:?}");

    let calls = drive_pi_extension(
        root.path(),
        &extension,
        "%23",
        r#"
        await fire("session_start", {}, tui);
        await settle();
        await fire("tool_execution_start", { toolName: "bash", args: {} }, tui);
        "#,
    );
    assert_eq!(
        last_state(&calls),
        Some("working".to_string()),
        "a non-ask tool must not read as blocked: {calls:?}"
    );
}

/// The report queue coalesces: a state that is superseded before its report goes out is
/// never sent, so a burst of transitions cannot leave the pane showing a stale one. This
/// is herdr's `queueState` behaviour, kept because taw applies writes in arrival order.
#[test]
fn pi_coalesces_transitions_that_happen_while_a_report_is_in_flight() {
    let _g = lock();
    if !have("node") {
        eprintln!("skipping: node is not on PATH");
        return;
    }
    let c = case("pi");
    let (root, _env) = c.prepare();
    let bin = tempfile::tempdir().unwrap();
    let fake = write_fake_taw(bin.path());
    install("pi", &fake).unwrap();

    let calls = drive_pi_extension(
        root.path(),
        &c.script_path(root.path()),
        "%24",
        r#"
        await fire("session_start", { reason: "startup" }, tui);
        // Blocked and unblocked inside one tick: only the resulting state is reported.
        pi.events.emit("taw:blocked", { active: true, label: "approve?" });
        pi.events.emit("taw:blocked", { active: false });
        "#,
    );
    let states: Vec<&str> = calls
        .iter()
        .filter(|c| c[1] == "report-agent")
        .filter_map(|c| {
            let i = c.iter().position(|a| a == "--state")?;
            Some(c[i + 1].as_str())
        })
        .collect();
    assert!(
        !states.contains(&"blocked"),
        "a superseded blocked state was still reported: {calls:?}"
    );
    assert_eq!(states.last(), Some(&"working"), "{calls:?}");
}
