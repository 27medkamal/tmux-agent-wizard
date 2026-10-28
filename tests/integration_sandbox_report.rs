//! The branch each integration takes inside a sandbox: with `TAW_REPORT_FILE`
//! set, the part that reports the session writes one report there, beside the file and renamed
//! into place, and nothing else. Each hook runs here with only `head`, `mv`, `rm` and `grep` on its
//! PATH, a taw that is not there and no `TMUX_PANE`: no taw, no python and no tmux are needed.

#[path = "common/exec.rs"]
mod exec;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CLAUDE: &str = r#"{"session_id":"4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d","transcript_path":"/home/you/.claude/projects/-work/4b1f2c3d-5e6f-4a7b-8c9d-0e1f2a3b4c5d.jsonl","cwd":"/work","hook_event_name":"SessionStart","source":"startup"}"#;
const CODEX: &str = r#"{"session_id":"019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b","transcript_path":"/home/you/.codex/sessions/2026/10/02/rollout-2026-10-02T09-12-01-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl","cwd":"/work","hook_event_name":"SessionStart","model":"gpt-5-codex","source":"startup"}"#;

/// `template` rendered as an install renders a shell hook, with a taw that is not there.
fn rendered(dir: &Path, template: &str, agent: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/integration/templates")
        .join(template);
    let text = std::fs::read_to_string(path).unwrap();
    let script = dir.join(template);
    std::fs::write(
        &script,
        text.replace("{{TAW_BIN}}", "'/nonexistent/taw'")
            .replace("{{AGENT}}", agent),
    )
    .unwrap();
    script
}

/// A folder of the only programs a hook's branch may use.
fn bare_bin(dir: &Path) -> PathBuf {
    let bin = dir.join("bare-bin");
    if bin.exists() {
        return bin;
    }
    std::fs::create_dir(&bin).unwrap();
    for tool in ["head", "mv", "rm", "grep"] {
        let found = ["/usr/bin", "/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|path| path.exists())
            .unwrap();
        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
    }
    bin
}

/// What a run left: its status, what it printed, the report, and every file in the report's folder.
struct Ran {
    status: i32,
    stdout: String,
    report: Option<Vec<u8>>,
    left: Vec<String>,
}

/// Runs `script action` with `input` on stdin and `env`, under `/bin/sh` with a bare PATH and, with
/// `inside`, `TAW_REPORT_FILE` in a folder of its own.
fn run(script: &Path, action: &str, input: &[u8], env: &[(&str, &str)], inside: bool) -> Ran {
    run_in("/bin/sh", script, action, input, env, inside)
}

/// `run` under the shell at `shell`.
fn run_in(
    shell: &str,
    script: &Path,
    action: &str,
    input: &[u8],
    env: &[(&str, &str)],
    inside: bool,
) -> Ran {
    let dir = script.parent().unwrap();
    let folder = tempfile::tempdir_in(dir).unwrap();
    let report = folder.path().join("session.json");
    let mut cmd = Command::new(shell);
    cmd.arg(script)
        .arg(action)
        .env_clear()
        .env("PATH", bare_bin(dir))
        .env("HOME", dir)
        .env("HISTFILE", "/dev/null")
        .env("CLAUDE_CONFIG_DIR", dir.join(".claude"))
        .env("CODEX_HOME", dir.join(".codex"))
        .env("XDG_CONFIG_HOME", dir.join(".config"))
        .env("XDG_STATE_HOME", dir.join(".local/state"))
        .env("XDG_CACHE_HOME", dir.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if inside {
        cmd.env("TAW_REPORT_FILE", &report);
    }
    for (name, value) in env {
        cmd.env(name, value);
    }
    let mut child = cmd.spawn().unwrap();
    // A hook reads at most 64 KiB and may close its input before all of it is written.
    let _ = child.stdin.take().unwrap().write_all(input);
    let out = child.wait_with_output().unwrap();
    let status = out.status.code().unwrap_or(-1);
    let mut left: Vec<String> = std::fs::read_dir(folder.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    Ran {
        status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        report: std::fs::read(&report).ok(),
        left,
    }
}

#[test]
fn claudes_hook_writes_its_session_input_as_it_came_and_nothing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-claude.sh", "claude");

    let ran = run(&script, "session", CLAUDE.as_bytes(), &[], true);

    assert_eq!(ran.status, 0);
    assert_eq!(ran.report.as_deref(), Some(CLAUDE.as_bytes()));
    assert_eq!(ran.left, ["session.json"]);
}

#[test]
fn claudes_hook_writes_nothing_for_another_action_or_for_cursors_claude() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-claude.sh", "claude");

    for (action, env) in [("state", vec![]), ("session", vec![("CURSOR_VERSION", "")])] {
        let ran = run(&script, action, CLAUDE.as_bytes(), &env, true);
        assert_eq!(ran.status, 0, "{action} {env:?}");
        assert!(ran.left.is_empty(), "{action} {env:?}: {:?}", ran.left);
    }
}

#[test]
fn a_report_is_at_most_64_kib() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-claude.sh", "claude");

    let ran = run(&script, "session", &vec![b' '; 70_000], &[], true);

    assert_eq!(ran.report.map(|report| report.len()), Some(65_536));
}

#[test]
fn codexs_hook_writes_its_input_and_leaves_out_a_thread_started_from_another() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-codex.sh", "codex");

    assert_eq!(
        run(&script, "session", CODEX.as_bytes(), &[], true)
            .report
            .as_deref(),
        Some(CODEX.as_bytes())
    );
    let own = [("CODEX_THREAD_ID", "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b")];
    assert_eq!(
        run(&script, "session", CODEX.as_bytes(), &own, true)
            .report
            .as_deref(),
        Some(CODEX.as_bytes())
    );
    let another = [("CODEX_THREAD_ID", "0199ffff-0000-7000-8000-000000000000")];
    assert!(
        run(&script, "session", CODEX.as_bytes(), &another, true)
            .left
            .is_empty()
    );
}

/// The shells a container may have, each by absolute path (a bash 5 first on PATH is not needed).
fn shells() -> Vec<&'static str> {
    [
        "/bin/sh",
        "/bin/dash",
        "/bin/bash",
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
    ]
    .into_iter()
    .filter(|shell| Path::new(shell).exists())
    .collect()
}

#[test]
fn codexs_thread_must_be_the_value_of_the_inputs_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-codex.sh", "codex");
    let id = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
    let parent = "0199ffff-0000-7000-8000-000000000000";
    // A fork: its own id is the session_id, its parent's id sits in another field.
    let forked_from = format!(
        r#"{{"session_id":"{id}","forked_from_id":"{parent}","hook_event_name":"SessionStart","source":"startup"}}"#
    );
    // A thread whose parent's id is in the input but is not its session_id.
    let parent_only = format!(
        r#"{{"session_id":"{id}","parent_thread_id":"{parent}","hook_event_name":"SessionStart"}}"#
    );
    let blanks = format!(r#"{{"session_id" : "{id}", "source": "resume"}}"#);
    let spaced_tab = format!("{{\"session_id\"\t:\t\"{id}\"}}");
    for shell in shells() {
        let go = |input: &str, thread: &str| {
            run_in(
                shell,
                &script,
                "session",
                input.as_bytes(),
                &[("CODEX_THREAD_ID", thread)],
                true,
            )
        };
        // The thread's id held only in another field is not reported.
        let ran = go(&parent_only, parent);
        assert!(ran.left.is_empty(), "{shell}: {:?}", ran.left);
        assert_eq!(ran.status, 0, "{shell}");
        let ran = go(&forked_from, parent);
        assert!(ran.left.is_empty(), "{shell}: {:?}", ran.left);
        // A thread id that is the session_id without its last character is not the session_id.
        let shorter = &id[..id.len() - 1];
        let ran = go(&forked_from, shorter);
        assert!(ran.left.is_empty(), "{shell}: {:?}", ran.left);
        assert_eq!(ran.status, 0, "{shell}");
        // Its own id as the session_id is, with or without blanks around the colon.
        assert_eq!(
            go(&forked_from, id).report.as_deref(),
            Some(forked_from.as_bytes()),
            "{shell}"
        );
        assert_eq!(
            go(&blanks, id).report.as_deref(),
            Some(blanks.as_bytes()),
            "{shell}"
        );
        assert_eq!(
            go(&spaced_tab, id).report.as_deref(),
            Some(spaced_tab.as_bytes()),
            "{shell}"
        );
        // An id with a character outside a token's is never reported, even when the input holds it.
        for odd in [
            format!("{id}\""),
            format!("{id} x"),
            format!("{id}$(x)"),
            ".*".to_string(),
            "a|b".to_string(),
        ] {
            let input = format!(r#"{{"session_id":"{odd}"}}"#);
            let ran = go(&input, &odd);
            assert!(ran.left.is_empty(), "{shell}: {odd}: {:?}", ran.left);
            assert_eq!(ran.status, 0, "{shell}: {odd}");
        }
    }
}

#[test]
fn inside_a_sandbox_the_branch_is_taken_first_even_with_tmux_pane_set() {
    for (template, agent, input, extra) in [
        ("taw-agent-state-claude.sh", "claude", CLAUDE, vec![]),
        (
            "taw-agent-state-codex.sh",
            "codex",
            CODEX,
            vec![("CODEX_THREAD_ID", "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b")],
        ),
    ] {
        taken_first(template, agent, "session", input, &extra);
    }
}

/// A container that sees TMUX_PANE still only reports to the file: the hook's own path (a python3
/// run, a taw) is never reached. Here python3 and mktemp are on the PATH, and taw is an executable,
/// so that a path taken by mistake leaves a mark.
fn taken_first(template: &str, agent: &str, action: &str, input: &str, extra: &[(&str, &str)]) {
    {
        let dir = tempfile::tempdir().unwrap();
        let script = rendered(dir.path(), template, agent);
        let marks = dir.path().join("marks");
        let stubs = dir.path().join("stubs");
        std::fs::create_dir(&stubs).unwrap();
        for stub in ["python3", "taw"] {
            let path = stubs.join(stub);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho {stub} >>'{}'\n", marks.display()),
            )
            .unwrap();
            crate::exec::make_executable(&path);
        }
        std::os::unix::fs::symlink("/usr/bin/mktemp", stubs.join("mktemp")).unwrap();
        let text = std::fs::read_to_string(&script).unwrap();
        std::fs::write(
            &script,
            text.replace("/nonexistent/taw", stubs.join("taw").to_str().unwrap()),
        )
        .unwrap();
        let path = format!("{}:{}", bare_bin(dir.path()).display(), stubs.display());
        let tmp = dir.path().to_str().unwrap().to_string();
        let mut env = vec![
            ("TMUX_PANE", "%1"),
            ("PATH", path.as_str()),
            ("TMPDIR", tmp.as_str()),
        ];
        env.extend_from_slice(extra);

        let ran = run(&script, action, input.as_bytes(), &env, true);

        assert_eq!(ran.status, 0, "{agent}");
        assert_eq!(ran.report.as_deref(), Some(input.as_bytes()), "{agent}");
        assert_eq!(ran.left, ["session.json"], "{agent}");
        assert!(!marks.exists(), "{agent}: the hook's own path ran");
    }
}

/// A report names its agent (`TAW_REPORT_AGENT`, the profile's): another agent's hook inside the
/// sandbox, an imported copy such as Grok running Claude's hooks or `claude -p` run as a Codex
/// tool, writes nothing over the sandbox's own agent's report.
#[test]
fn a_hook_writes_nothing_when_the_report_names_another_agent() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, input, other) in [
        ("taw-agent-state-claude.sh", "claude", CLAUDE, "codex"),
        ("taw-agent-state-codex.sh", "codex", CODEX, "claude"),
    ] {
        let script = rendered(dir.path(), template, agent);
        for shell in shells() {
            let other_named = [("TAW_REPORT_AGENT", other)];
            let ran = run_in(
                shell,
                &script,
                "session",
                input.as_bytes(),
                &other_named,
                true,
            );
            assert_eq!(ran.status, 0, "{agent} {shell}");
            assert!(ran.left.is_empty(), "{agent} {shell}: {:?}", ran.left);
            assert_eq!(ran.report, None, "{agent} {shell}");

            // Its own name, and no name at all (a profile of your own that sets only the file).
            let own_named = [("TAW_REPORT_AGENT", agent)];
            for env in [&own_named[..], &[][..]] {
                let ran = run_in(shell, &script, "session", input.as_bytes(), env, true);
                assert_eq!(ran.status, 0, "{agent} {shell} {env:?}");
                assert_eq!(
                    ran.report.as_deref(),
                    Some(input.as_bytes()),
                    "{agent} {shell} {env:?}"
                );
                assert_eq!(ran.left, ["session.json"], "{agent} {shell} {env:?}");
            }
        }
    }
}

/// A name that is another agent's, even an empty-looking or look-alike one, is not this agent's.
#[test]
fn a_look_alike_agent_name_is_another_agent() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-claude.sh", "claude");
    for name in ["Claude", "claude ", "claude-code", "cursor", "*", "cl*"] {
        let named = [("TAW_REPORT_AGENT", name)];
        let ran = run(&script, "session", CLAUDE.as_bytes(), &named, true);
        assert_eq!(ran.status, 0, "{name:?}");
        assert!(ran.left.is_empty(), "{name:?}: {:?}", ran.left);
    }
}

/// Grok runs Claude's hooks too and says so with `GROK_SESSION_ID` (its own hook reports its
/// session): Claude's branch writes nothing then, with no agent named at all.
#[test]
fn claudes_branch_writes_nothing_under_grok_even_with_no_agent_named() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-claude.sh", "claude");
    for shell in shells() {
        for value in ["", "0e1f2a3b"] {
            let grok = [("GROK_SESSION_ID", value)];
            let ran = run_in(shell, &script, "session", CLAUDE.as_bytes(), &grok, true);
            assert_eq!(ran.status, 0, "{shell} {value:?}");
            assert!(ran.left.is_empty(), "{shell} {value:?}: {:?}", ran.left);
        }
        let ran = run_in(shell, &script, "session", CLAUDE.as_bytes(), &[], true);
        assert_eq!(ran.report.as_deref(), Some(CLAUDE.as_bytes()), "{shell}");
    }
}

#[test]
fn without_the_variable_both_hooks_write_no_report() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, input) in [
        ("taw-agent-state-claude.sh", "claude", CLAUDE),
        ("taw-agent-state-codex.sh", "codex", CODEX),
    ] {
        let script = rendered(dir.path(), template, agent);
        let ran = run(&script, "session", input.as_bytes(), &[], false);
        assert_eq!(ran.status, 0, "{agent}");
        assert!(ran.left.is_empty(), "{agent}: {:?}", ran.left);
    }
}

#[test]
fn a_report_folder_that_is_gone_ends_the_hook_quietly() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, input) in [
        ("taw-agent-state-claude.sh", "claude", CLAUDE),
        ("taw-agent-state-codex.sh", "codex", CODEX),
    ] {
        let script = rendered(dir.path(), template, agent);
        let gone = dir.path().join("no-such-folder/session.json");
        let mut child = Command::new("/bin/sh")
            .arg(&script)
            .arg("session")
            .env_clear()
            .env("PATH", bare_bin(dir.path()))
            .env("HOME", dir.path())
            .env("HISTFILE", "/dev/null")
            .env("TAW_REPORT_FILE", &gone)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{agent}");
        assert!(
            out.stdout.is_empty() && out.stderr.is_empty(),
            "{agent}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Every other shell hook: its template, the agent it is rendered for, the action its session
/// report is run with, and an input of the shape its agent gives.
const HOOKS: [(&str, &str, &str, &str); 11] = [
    (
        "taw-agent-state-copilot.sh",
        "copilot",
        "",
        r#"{"hook_event_name":"SessionStart","sessionId":"cp-1"}"#,
    ),
    (
        "taw-agent-state-cursor.sh",
        "cursor",
        "session",
        r#"{"hook_event_name":"sessionStart","conversation_id":"cu-1"}"#,
    ),
    (
        "taw-agent-state-devin.sh",
        "devin",
        "session",
        r#"{"hook_event_name":"SessionStart","session_id":"dv-1","source":"resume"}"#,
    ),
    (
        "taw-agent-state-droid.sh",
        "droid",
        "session",
        r#"{"session_id":"dr-1"}"#,
    ),
    (
        "taw-agent-state-grok.sh",
        "grok",
        "session",
        r#"{"hook_event_name":"session_start","session_id":"gk-1"}"#,
    ),
    (
        "taw-agent-state-kimi.sh",
        "kimi",
        "session",
        r#"{"session_id":"km-1"}"#,
    ),
    (
        "taw-agent-state-qodercli.sh",
        "qodercli",
        "session",
        r#"{"session_id":"qd-1"}"#,
    ),
    (
        "taw-agent-state-mastracode.sh",
        "mastracode",
        "session",
        r#"{"session_id":"mc-1"}"#,
    ),
    (
        "taw-agent-state-antigravity-cli.sh",
        "antigravity-cli",
        "session",
        r#"{"conversationId":"ag-1"}"#,
    ),
    (
        "taw-agent-session-letta.sh",
        "letta",
        "session",
        r#"{"conversation_id":"conv-9"}"#,
    ),
    (
        "taw-agent-session-qwen.sh",
        "qwen",
        "session",
        r#"{"session_id":"qw-1","source":"startup"}"#,
    ),
];

/// The agent's kind as taw passes it in `TAW_SANDBOX_AGENT` (its manifest id), which the profile
/// puts in `TAW_REPORT_AGENT`: Antigravity's install target is antigravity-cli, its kind `agy`.
fn kind_of(agent: &str) -> &str {
    match agent {
        "antigravity-cli" => "agy",
        other => other,
    }
}

/// What a hook prints on every path: nothing, but Antigravity's, which must print an object.
fn printed(agent: &str) -> &'static str {
    if agent == "antigravity-cli" {
        "{}\n"
    } else {
        ""
    }
}

/// The shells that run `agent`'s hook: Cursor, Devin and Antigravity run theirs with `bash` (3.2 or
/// 5); the others name `sh` in their first line, so a container may run them with any.
fn shells_for(agent: &str) -> Vec<&'static str> {
    shells()
        .into_iter()
        .filter(|shell| {
            !matches!(agent, "cursor" | "devin" | "antigravity-cli") || shell.ends_with("/bash")
        })
        .collect()
}

#[test]
fn every_other_shell_hook_writes_its_session_input_as_it_came() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, action, input) in HOOKS {
        let script = rendered(dir.path(), template, agent);
        for shell in shells_for(agent) {
            let ran = run_in(shell, &script, action, input.as_bytes(), &[], true);
            assert_eq!(ran.status, 0, "{agent} {shell}");
            assert_eq!(
                ran.report.as_deref(),
                Some(input.as_bytes()),
                "{agent} {shell}"
            );
            assert_eq!(ran.left, ["session.json"], "{agent} {shell}");
            assert_eq!(ran.stdout, printed(agent), "{agent} {shell}");
        }
    }
}

#[test]
fn the_hooks_that_report_a_state_write_nothing_for_one() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent) in [
        ("taw-agent-state-kimi.sh", "kimi"),
        ("taw-agent-state-mastracode.sh", "mastracode"),
    ] {
        let script = rendered(dir.path(), template, agent);
        let ran = run(&script, "working", br#"{"session_id":"x-1"}"#, &[], true);
        assert!(ran.left.is_empty(), "{agent}: {:?}", ran.left);
    }
}

/// Every hook whose branch checks the action reports a session and nothing else: another action
/// (a state) writes nothing, under every shell the hook runs in. (Kimi's and Mastra Code's are
/// above; Copilot's and Antigravity's take no action.)
#[test]
fn the_hooks_that_report_a_session_write_nothing_for_another_action() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = [
        "cursor", "devin", "droid", "grok", "qodercli", "letta", "qwen",
    ];
    let mut seen = 0;
    for (template, agent, action, input) in HOOKS {
        if !hooks.contains(&agent) {
            continue;
        }
        seen += 1;
        let script = rendered(dir.path(), template, agent);
        for shell in shells_for(agent) {
            let sent = run_in(shell, &script, action, input.as_bytes(), &[], true);
            assert!(
                sent.report.is_some(),
                "{agent} {shell}: its own action reports"
            );
            let ran = run_in(shell, &script, "working", input.as_bytes(), &[], true);
            assert_eq!(ran.status, 0, "{agent} {shell}");
            assert!(ran.report.is_none(), "{agent} {shell}");
            assert!(ran.left.is_empty(), "{agent} {shell}: {:?}", ran.left);
        }
    }
    assert_eq!(seen, hooks.len());
}

#[test]
fn antigravitys_hook_still_answers_with_an_empty_object() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(
        dir.path(),
        "taw-agent-state-antigravity-cli.sh",
        "antigravity-cli",
    );
    let ran = run(
        &script,
        "session",
        br#"{"conversationId":"ag-1"}"#,
        &[],
        true,
    );
    assert_eq!(ran.stdout, "{}\n");
}

#[test]
fn grok_reports_the_session_id_its_environment_gives() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-grok.sh", "grok");

    let ran = run(
        &script,
        "session",
        br#"{"session_id":"from-input"}"#,
        &[("GROK_SESSION_ID", "gk-env-7")],
        true,
    );
    assert_eq!(
        ran.report.as_deref(),
        Some(
            &br#"{"hook_event_name":"SessionStart","session_id":"gk-env-7","taw_agent":"grok"}"#[..]
        )
    );
    let odd = run(
        &script,
        "session",
        br#"{"session_id":"from-input"}"#,
        &[("GROK_SESSION_ID", "a b\"c")],
        true,
    );
    assert_eq!(
        odd.report.as_deref(),
        Some(&br#"{"session_id":"from-input"}"#[..]),
        "a value that is no plain token is not written"
    );
}

/// Grok's report of its own shape names the agent the hook was installed for, not a word of its
/// own, under every shell, and the hook reads its whole input first, so that Grok never writes
/// into a pipe already closed.
#[test]
fn groks_own_report_names_its_agent_and_its_input_is_read_to_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let script = rendered(dir.path(), "taw-agent-state-grok.sh", "grok-under-test");
    let input = format!(
        r#"{{"session_id":"from-input","pad":"{}"}}"#,
        "x".repeat(4000)
    );
    let fed = dir.path().join("input.json");
    std::fs::write(&fed, &input).unwrap();
    for shell in shells_for("grok") {
        let folder = tempfile::tempdir_in(dir.path()).unwrap();
        let report = folder.path().join("session.json");
        let mut stdin = std::fs::File::open(&fed).unwrap();
        let status = Command::new(shell)
            .arg(&script)
            .arg("session")
            .env_clear()
            .env("PATH", bare_bin(dir.path()))
            .env("HOME", dir.path())
            .env("HISTFILE", "/dev/null")
            .env("TAW_REPORT_FILE", &report)
            .env("GROK_SESSION_ID", "gk-env-7")
            .stdin(stdin.try_clone().unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(0), "{shell}");
        assert_eq!(
            std::fs::read(&report).ok().as_deref(),
            Some(&br#"{"hook_event_name":"SessionStart","session_id":"gk-env-7","taw_agent":"grok-under-test"}"#[..]),
            "{shell}"
        );
        // The input shares its offset with the hook's: read to the end, it is at the end.
        use std::io::Seek;
        assert_eq!(
            stdin.stream_position().unwrap(),
            input.len() as u64,
            "{shell}"
        );
    }
}

#[test]
fn every_other_shell_hooks_branch_is_taken_first_even_with_tmux_pane_set() {
    for (template, agent, action, input) in HOOKS {
        taken_first(template, agent, action, input, &[]);
    }
}

#[test]
fn every_other_shell_hook_writes_nothing_when_the_report_names_another_agent() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, action, input) in HOOKS {
        let script = rendered(dir.path(), template, agent);
        // Claude's name, and the install target's name where it is not the kind (antigravity-cli).
        let mut others = vec!["claude"];
        if kind_of(agent) != agent {
            others.push(agent);
        }
        for shell in shells_for(agent) {
            for other in &others {
                let named = [("TAW_REPORT_AGENT", *other)];
                let ran = run_in(shell, &script, action, input.as_bytes(), &named, true);
                assert_eq!(ran.status, 0, "{agent} {shell} {other}");
                assert!(
                    ran.left.is_empty(),
                    "{agent} {shell} {other}: {:?}",
                    ran.left
                );
                assert_eq!(ran.stdout, printed(agent), "{agent} {shell} {other}");
            }
            // Its own kind, and no name at all (a profile of your own that sets only the file).
            let own = [("TAW_REPORT_AGENT", kind_of(agent))];
            for env in [&own[..], &[][..]] {
                let ran = run_in(shell, &script, action, input.as_bytes(), env, true);
                assert_eq!(
                    ran.report.as_deref(),
                    Some(input.as_bytes()),
                    "{agent} {shell} {env:?}"
                );
                assert_eq!(ran.left, ["session.json"], "{agent} {shell} {env:?}");
            }
        }
    }
}

#[test]
fn without_the_variable_no_other_shell_hook_writes_a_report() {
    let dir = tempfile::tempdir().unwrap();
    for (template, agent, action, input) in HOOKS {
        let script = rendered(dir.path(), template, agent);
        let ran = run(&script, action, input.as_bytes(), &[], false);
        assert_eq!(ran.status, 0, "{agent}");
        assert!(ran.left.is_empty(), "{agent}: {:?}", ran.left);
        assert_eq!(ran.stdout, printed(agent), "{agent}");
    }
}

#[test]
fn a_gone_report_folder_ends_every_other_shell_hook_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let grok = ("taw-agent-state-grok.sh", "grok", "session", HOOKS[4].3);
    let cases = HOOKS
        .iter()
        .map(|hook| (*hook, None))
        .chain([(grok, Some("gk-env-7"))]);
    for ((template, agent, action, input), grok_id) in cases {
        let script = rendered(dir.path(), template, agent);
        let gone = dir.path().join("no-such-folder/session.json");
        for shell in shells_for(agent) {
            let mut cmd = Command::new(shell);
            cmd.arg(&script)
                .arg(action)
                .env_clear()
                .env("PATH", bare_bin(dir.path()))
                .env("HOME", dir.path())
                .env("HISTFILE", "/dev/null")
                .env("TAW_REPORT_FILE", &gone)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(id) = grok_id {
                cmd.env("GROK_SESSION_ID", id);
            }
            let mut child = cmd.spawn().unwrap();
            let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
            let out = child.wait_with_output().unwrap();
            assert_eq!(out.status.code(), Some(0), "{agent} {shell} {grok_id:?}");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                printed(agent),
                "{agent} {shell}"
            );
            assert!(
                out.stderr.is_empty(),
                "{agent} {shell} {grok_id:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
