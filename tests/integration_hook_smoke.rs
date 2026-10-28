//! End-to-end smoke test for every hook script and plugin `taw integration install`
//! writes: the *real* `taw` binary, the *real* rendered hook, a herdr-shaped event payload
//! on stdin, and a throwaway tmux server whose pane options are then read back.
//!
//! `tests/integration_ports.rs` checks what each hook *asks* taw to do, against a fake
//! `taw` that records its argv. This checks the other half — that the ask actually lands —
//! by running the assembled product: a hook fires, and `@agent_session_id`, `@agent_kind`
//! and (for a lifecycle authority) `@agent_state` / `@agent_source` come back set on the
//! pane. Between them, a hook cannot pass by inventing a flag `taw` does not accept.
//!
//! Nothing here touches the developer's tmux server, config, state or home: [`Server`] owns
//! a `tmux -L <unique> -f /dev/null` server it kills on drop, and every install is pointed
//! at a tempdir through the agent's own config-directory variable.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

#[path = "common/mod.rs"]
mod common;

use common::{Server, wait_for};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn have(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Installs `agent`'s integration into a tempdir, with `env_var` pointing at it and
/// `$HOME` moved there too, so nothing can reach the developer's own dotfiles even if an
/// installer resolved a path this test did not anticipate.
///
/// Runs `taw integration install` as a *subprocess*, so the `taw` path baked into the
/// rendered hook is the real binary under test rather than a placeholder — that is the
/// whole point of this file.
fn install_into(root: &Path, env_var: &str, env_value: &Path, agent: &str) -> serde_json::Value {
    let output = Command::new(common::taw_bin())
        .args(["integration", "install", agent])
        .env("HOME", root)
        .env(env_var, env_value)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TAW_REPORT_FILE")
        .output()
        .expect("run taw integration install");
    assert!(
        output.status.success(),
        "taw integration install {agent} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("install envelope is JSON");
    assert!(
        envelope.get("error").is_none(),
        "install reported an error: {envelope}"
    );
    envelope["result"].clone()
}

/// Waits for `option` on `pane` to become `expected`, then asserts it — so a failure says
/// what the option actually held rather than just timing out.
fn assert_pane_opt_eventually(server: &Server, pane: &str, option: &str, expected: &str) {
    let ok = wait_for(
        || server.pane_opt(pane, option) == expected,
        std::time::Duration::from_secs(10),
    );
    assert!(
        ok,
        "{option} never became {expected:?} (it is {:?})",
        server.pane_opt(pane, option)
    );
}

/// `(agent, config-dir variable, its value under the tempdir, the installed script,
/// the session-reporting action, a herdr-shaped payload, the expected @agent_kind and
/// @agent_session_id)`.
struct Case {
    agent: &'static str,
    env_var: &'static str,
    env_value: &'static str,
    /// Directory the installer requires, relative to the tempdir root.
    config_dir: &'static str,
    script: &'static str,
    action: &'static str,
    payload: &'static str,
    kind: &'static str,
    session_id: &'static str,
    /// Extra environment the hook itself reads.
    hook_env: &'static [(&'static str, &'static str)],
}

/// Every shell-hook integration, one row each. The plugin integrations (`pi`, `omp`,
/// `kilo`, `opencode`, `hermes`) are not here: they are loaded by their agent's own
/// runtime rather than executed as a script, and `tests/integration_ports.rs` drives them
/// under node and python instead.
const CASES: &[Case] = &[
    Case {
        agent: "claude",
        env_var: "CLAUDE_CONFIG_DIR",
        env_value: "claude",
        config_dir: "claude",
        script: "claude/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-claude","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","source":"startup"}"#,
        kind: "claude",
        session_id: "sm-claude",
        hook_env: &[],
    },
    Case {
        agent: "codex",
        env_var: "CODEX_HOME",
        env_value: "codex",
        config_dir: "codex",
        script: "codex/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-codex","transcript_path":"/t.jsonl","hook_event_name":"SessionStart","source":"startup"}"#,
        kind: "codex",
        session_id: "sm-codex",
        hook_env: &[],
    },
    Case {
        agent: "copilot",
        env_var: "COPILOT_HOME",
        env_value: "copilot",
        config_dir: "copilot",
        script: "copilot/hooks/taw-agent-state.sh",
        action: "",
        payload: r#"{"session_id":"sm-copilot","hook_event_name":"SessionStart"}"#,
        kind: "copilot",
        session_id: "sm-copilot",
        hook_env: &[],
    },
    Case {
        agent: "devin",
        env_var: "XDG_CONFIG_HOME",
        env_value: "",
        config_dir: "devin",
        script: "devin/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-devin","hook_event_name":"SessionStart","source":"resume"}"#,
        kind: "devin",
        session_id: "sm-devin",
        hook_env: &[],
    },
    Case {
        agent: "droid",
        env_var: "HOME",
        env_value: "",
        config_dir: ".factory",
        script: ".factory/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-droid"}"#,
        kind: "droid",
        session_id: "sm-droid",
        hook_env: &[],
    },
    Case {
        agent: "qodercli",
        env_var: "QODER_CONFIG_DIR",
        env_value: "qoder",
        config_dir: "qoder",
        script: "qoder/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-qoder","hook_event_name":"SessionStart"}"#,
        kind: "qodercli",
        session_id: "sm-qoder",
        hook_env: &[],
    },
    Case {
        agent: "qwen",
        env_var: "QWEN_HOME",
        env_value: "qwen",
        config_dir: "qwen",
        script: "qwen/hooks/taw-agent-session.sh",
        action: "session",
        payload: r#"{"session_id":"sm-qwen","source":"startup"}"#,
        kind: "qwen",
        session_id: "sm-qwen",
        hook_env: &[],
    },
    Case {
        agent: "cursor",
        env_var: "CURSOR_CONFIG_DIR",
        env_value: "cursor",
        config_dir: "cursor",
        script: "cursor/taw-agent-state.sh",
        action: "session",
        payload: r#"{"hook_event_name":"sessionStart","session_id":"sm-cursor"}"#,
        kind: "cursor",
        session_id: "sm-cursor",
        hook_env: &[],
    },
    Case {
        agent: "grok",
        env_var: "GROK_HOME",
        env_value: "grok",
        config_dir: "grok",
        script: "grok/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"hook_event_name":"session_start"}"#,
        kind: "grok",
        session_id: "sm-grok",
        hook_env: &[("GROK_SESSION_ID", "sm-grok")],
    },
    Case {
        agent: "antigravity-cli",
        env_var: "ANTIGRAVITY_CLI_CONFIG_DIR",
        env_value: "gemini-config",
        config_dir: "gemini-config",
        script: "gemini-config/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"conversationId":"sm-agy","transcriptPath":"/t/agy.json"}"#,
        // Antigravity CLI is `agy` everywhere taw names an agent.
        kind: "agy",
        session_id: "sm-agy",
        hook_env: &[],
    },
    Case {
        agent: "kimi",
        env_var: "KIMI_CODE_HOME",
        env_value: "kimi-code",
        config_dir: "kimi-code",
        script: "kimi-code/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-kimi"}"#,
        kind: "kimi",
        session_id: "sm-kimi",
        hook_env: &[],
    },
    Case {
        agent: "mastracode",
        env_var: "HOME",
        env_value: "",
        config_dir: ".mastracode",
        script: ".mastracode/hooks/taw-agent-state.sh",
        action: "session",
        payload: r#"{"session_id":"sm-mastra"}"#,
        kind: "mastracode",
        session_id: "sm-mastra",
        hook_env: &[],
    },
];

/// Every shell hook, fired for real against a throwaway server: the session identity it
/// reports has to reach the pane.
#[test]
fn every_shell_hook_reports_session_identity_onto_the_pane() {
    let _g = lock();
    if !have("python3") {
        eprintln!(
            "skipping: python3 is not on PATH, and every ported shell hook parses JSON with it"
        );
        return;
    }
    let server = Server::start();
    let pane = server.pane();

    for case in CASES {
        let root = tempfile::tempdir().unwrap();
        let env_value: PathBuf = root.path().join(case.env_value);
        std::fs::create_dir_all(root.path().join(case.config_dir)).unwrap();
        // kimi refuses to install into a too-old Kimi Code, so stand one in.
        let stub_bin = root.path().join("stub-bin");
        std::fs::create_dir_all(&stub_bin).unwrap();
        let kimi = stub_bin.join("kimi");
        std::fs::write(&kimi, "#!/bin/sh\nprintf 'kimi 0.14.0\\n'\n").unwrap();
        common::make_executable(&kimi);
        let path = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let output = Command::new(common::taw_bin())
            .args(["integration", "install", case.agent])
            .env("HOME", root.path())
            .env(case.env_var, &env_value)
            .env("PATH", &path)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE")
            .output()
            .expect("run taw integration install");
        assert!(
            output.status.success(),
            "taw integration install {} failed: {}{}",
            case.agent,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // A fresh pane per case, so one agent's report cannot be mistaken for another's.
        let case_pane = server.tmux_ok(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &pane,
            "/bin/sh",
        ]);
        let script = root.path().join(case.script);
        assert!(
            script.is_file(),
            "{} was not installed at {}",
            case.agent,
            script.display()
        );

        let action: &[&str] = if case.action.is_empty() {
            &[]
        } else {
            &[case.action]
        };
        let mut cmd = Command::new("sh");
        cmd.arg(&script).args(action);
        fire_hook_with(&server, cmd, &case_pane, case.payload, case.hook_env);

        assert_pane_opt_eventually(&server, &case_pane, "@agent_session_id", case.session_id);
        assert_eq!(
            server.pane_opt(&case_pane, "@agent_kind"),
            case.kind,
            "{} reported the wrong @agent_kind",
            case.agent
        );
        // `report-agent-session` must not invent a state: herdr's SessionStart fires
        // before the agent has done anything, and a guess would fight the watcher.
        assert_eq!(
            server.pane_opt(&case_pane, "@agent_state"),
            "",
            "{} overwrote @agent_state from a session report",
            case.agent
        );
        server.tmux_ok(&["kill-pane", "-t", &case_pane]);
    }
}

/// A lifecycle-authority hook (kimi and mastracode are the two shell ones) writes the
/// state *and* claims authority for its own source, so the watcher stops overruling it.
#[test]
fn a_lifecycle_hook_writes_the_state_and_claims_authority() {
    let _g = lock();
    if !have("python3") {
        eprintln!("skipping: python3 is not on PATH, and these hooks parse JSON with it");
        return;
    }
    let server = Server::start();
    let pane = server.pane();

    for (agent, env_var, env_value, config_dir, script, payload) in [
        (
            "mastracode",
            "HOME",
            "",
            ".mastracode",
            ".mastracode/hooks/taw-agent-state.sh",
            r#"{"session_id":"sm-mastra"}"#,
        ),
        (
            "kimi",
            "KIMI_CODE_HOME",
            "kimi-code",
            "kimi-code",
            "kimi-code/hooks/taw-agent-state.sh",
            r#"{"session_id":"sm-kimi"}"#,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(config_dir)).unwrap();
        let stub_bin = root.path().join("stub-bin");
        std::fs::create_dir_all(&stub_bin).unwrap();
        let kimi = stub_bin.join("kimi");
        std::fs::write(&kimi, "#!/bin/sh\nprintf 'kimi 0.14.0\\n'\n").unwrap();
        common::make_executable(&kimi);
        let path = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new(common::taw_bin())
            .args(["integration", "install", agent])
            .env("HOME", root.path())
            .env(env_var, root.path().join(env_value))
            .env("PATH", &path)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TAW_REPORT_FILE")
            .output()
            .expect("run taw integration install");
        assert!(output.status.success(), "{agent} install failed");

        let case_pane = server.tmux_ok(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &pane,
            "/bin/sh",
        ]);
        let script = root.path().join(script);

        for state in ["working", "blocked", "idle"] {
            let mut cmd = Command::new("sh");
            cmd.arg(&script).arg(state);
            fire_hook_with(&server, cmd, &case_pane, payload, &[]);
            assert_pane_opt_eventually(&server, &case_pane, "@agent_state", state);
        }
        assert_eq!(
            server.pane_opt(&case_pane, "@agent_source"),
            format!("hook:integration:{agent}"),
            "{agent} did not claim hook authority for its own source"
        );
        server.tmux_ok(&["kill-pane", "-t", &case_pane]);
    }
}

/// Outside tmux there is no `$TMUX_PANE`, so a hook that fires anyway must do nothing at
/// all — never report against another pane, never fail the agent's own turn.
#[test]
fn a_hook_without_tmux_pane_reports_nothing_and_still_exits_zero() {
    let _g = lock();
    let server = Server::start();
    let pane = server.pane();

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("claude")).unwrap();
    install_into(
        root.path(),
        "CLAUDE_CONFIG_DIR",
        &root.path().join("claude"),
        "claude",
    );
    let script = root.path().join("claude/hooks/taw-agent-state.sh");

    let output = Command::new("sh")
        .arg(&script)
        .arg("session")
        .env_remove("TMUX_PANE")
        .env_remove("TAW_REPORT_FILE")
        .env_remove("TMUX")
        .env("TAW_TMUX_SOCKET_NAME", server.socket())
        .env("TAW_CONFIG_DIR", server.config_dir())
        .env("TAW_STATE_DIR", server.state_dir())
        .stdin(Stdio::null())
        .output()
        .expect("run hook");
    assert!(output.status.success(), "the hook must exit 0");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");
}

/// Runs an already-built hook command with `payload` on its stdin, `$TMUX_PANE` set to
/// `pane`, and the throwaway server's socket and paths in the environment — so the `taw`
/// the hook invokes talks to *that* server and writes nowhere near the developer's own
/// state. Asserts the hook exits 0, as every taw hook must, whatever it decides to do.
fn fire_hook_with(
    server: &Server,
    mut cmd: Command,
    pane: &str,
    payload: &str,
    extra_env: &[(&str, &str)],
) {
    use std::io::Write;

    cmd.env("TMUX_PANE", pane)
        .env("TAW_TMUX_SOCKET_NAME", server.socket())
        .env("TAW_CONFIG_DIR", server.config_dir())
        .env("TAW_STATE_DIR", server.state_dir())
        .env_remove("TMUX")
        .env_remove("TAW_REPORT_FILE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait for hook");
    assert!(
        output.status.success(),
        "hook exited non-zero: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
