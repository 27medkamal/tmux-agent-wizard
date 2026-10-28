//! The stateful CLI journeys of the assembled system, through the built binary: session
//! snapshot/restore (including agent resume), the plugin registry and one plugin action,
//! and installing/uninstalling an agent integration.
//!
//! The unit and library-level integration tests cover each of these against `taw`'s own
//! API. What is proved here is the end-to-end shape: that the CLI's `--dry-run` really
//! creates nothing, that a restore recreates the tmux layout a `save` captured, that the
//! resume command is *executed* in the restored pane rather than merely typed at it, that
//! a plugin action's process is handed the environment herdr documents, and that
//! uninstalling an integration leaves the agent's own `settings.json` byte-identical.

mod common;

use std::time::Duration;

use common::{Server, ServerOptions, taw_bin, wait_for, write_script};

const SETTLE: Duration = Duration::from_secs(5);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// `restore::apply` recreates a killed session with an explicit `200x50`, because a
/// detached session needs a size for `select-layout` to apply. Snapshotting at the same
/// size is what lets a captured `#{window_layout}` round-trip geometrically.
const RESTORE_WIDTH: u32 = 200;
const RESTORE_HEIGHT: u32 = 50;

/// Every live session name.
fn sessions(server: &Server) -> Vec<String> {
    server
        .tmux_ok(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// `session:window_name` for every window, sorted — the shape a restore has to reproduce.
fn windows(server: &Server, session: &str) -> Vec<String> {
    let mut names: Vec<String> = server
        .tmux_ok(&[
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_index}:#{window_name}",
        ])
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

fn pane_count(server: &Server, target: &str) -> usize {
    server
        .tmux_ok(&["list-panes", "-t", target, "-F", "#{pane_id}"])
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// Save, kill, dry-run, restore — and then a restore that resumes an agent.
#[test]
fn session_save_and_restore_rebuild_a_killed_workspace_and_resume_its_agent() {
    // A fake `claude` on every pane shell's PATH that records its argv and then drops to a
    // shell. Recording the argv is what makes the resume assertion about *execution*:
    // dropping the `Enter` that `restore` sends would still leave the right text sitting
    // unexecuted on the prompt line.
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let argv_log = tmp.path().join("claude-argv.txt");
    write_script(
        &bin_dir.join("claude"),
        &format!(
            "printf '%s\\n' \"$@\" >> '{}'\nexec /bin/sh\n",
            argv_log.display()
        ),
    );

    let server = Server::start_with(ServerOptions {
        session: "alpha".to_string(),
        width: RESTORE_WIDTH,
        height: RESTORE_HEIGHT,
        path_prefix: Some(bin_dir),
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);

    // Two sessions (so the server never runs out of them while `alpha` is killed — a few
    // tmux reads need *some* session to resolve against), one of them with a split and a
    // second, named window.
    server.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-x",
        &RESTORE_WIDTH.to_string(),
        "-y",
        &RESTORE_HEIGHT.to_string(),
        "-c",
        server.work_dir().to_str().unwrap(),
    ]);
    server.tmux_ok(&["rename-window", "-t", "alpha:0", "editor"]);
    server.tmux_ok(&["split-window", "-t", "alpha:0"]);
    server.tmux_ok(&["new-window", "-t", "alpha", "-n", "logs"]);

    let saved_windows = windows(&server, "alpha");
    assert_eq!(saved_windows.len(), 2, "{saved_windows:?}");
    assert_eq!(pane_count(&server, "alpha:0"), 2);

    // --- save ---------------------------------------------------------------------------
    let save = server.ok(&["session", "save"], "cli:session:save", "session_saved");
    assert_eq!(save["name"], "default", "{save}");
    assert_eq!(save["sessions"], 2, "{save}");
    let snapshot_path = save["path"].as_str().unwrap();
    assert!(
        std::path::Path::new(snapshot_path).is_file(),
        "the snapshot file should exist at {snapshot_path}"
    );
    assert!(
        snapshot_path.starts_with(server.state_dir().to_str().unwrap()),
        "the snapshot must live under $TAW_STATE_DIR, not {snapshot_path}"
    );

    let listed = server.ok(&["session", "list"], "cli:session:list", "session_list");
    let names: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"default"), "{listed}");

    // --- kill one, then a dry run that must change nothing -------------------------------
    server.tmux_ok(&["kill-session", "-t", "alpha"]);
    assert_eq!(sessions(&server), vec!["beta".to_string()]);

    let dry = server.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(dry["dry_run"], true, "{dry}");
    assert_eq!(
        dry["created"].as_array().unwrap().len(),
        0,
        "a dry run must create nothing: {dry}"
    );
    assert_eq!(
        dry["skipped"].as_array().unwrap(),
        &vec![serde_json::json!("beta")],
        "the live session should be reported as skipped: {dry}"
    );
    let plan = dry["plan"].as_array().unwrap();
    assert!(
        plan.iter()
            .any(|line| line.as_str().unwrap().contains("create session 'alpha'")),
        "the plan should name the missing session: {dry}"
    );
    assert!(
        plan.iter()
            .any(|line| line.as_str().unwrap().contains("editor")),
        "the plan should name the windows it would recreate: {dry}"
    );
    assert_eq!(
        sessions(&server),
        vec!["beta".to_string()],
        "a dry run must not touch the server"
    );

    // --- the real restore ----------------------------------------------------------------
    let restored = server.ok(
        &["session", "restore"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        restored["created"].as_array().unwrap(),
        &vec![serde_json::json!("alpha")],
        "{restored}"
    );
    assert_eq!(
        restored["skipped"].as_array().unwrap(),
        &vec![serde_json::json!("beta")],
        "{restored}"
    );
    let mut live = sessions(&server);
    live.sort();
    assert_eq!(live, vec!["alpha".to_string(), "beta".to_string()]);
    assert_eq!(
        windows(&server, "alpha"),
        saved_windows,
        "restore should reproduce the window indexes and names"
    );
    assert_eq!(
        pane_count(&server, "alpha:0"),
        2,
        "the split window should come back split"
    );
    assert_eq!(pane_count(&server, "alpha:1"), 1);

    // --- and a restore that resumes an agent ---------------------------------------------
    let pane = server.pane_of("alpha");
    server.set_pane_opt(&pane, "@agent_kind", "claude");
    server.set_pane_opt(&pane, "@agent_session_id", "abc");
    server.ok(
        &["session", "save", "--name", "resume"],
        "cli:session:save",
        "session_saved",
    );
    server.tmux_ok(&["kill-session", "-t", "alpha"]);

    let resumed = server.ok(
        &["session", "restore", "--name", "resume"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(
        resumed["resumed"].as_array().unwrap().len(),
        1,
        "the saved agent pane should be resumed: {resumed}"
    );
    assert!(
        wait_for(
            || std::fs::read_to_string(&argv_log)
                .is_ok_and(|text| text.contains("--resume") && text.contains("abc")),
            SETTLE
        ),
        "`claude --resume abc` was never executed in the restored pane (argv log: {:?})",
        std::fs::read_to_string(&argv_log)
    );
    // The restored pane carries the kind hint, so re-detection lands on the right identity
    // before the resumed process has reported anything itself.
    let hinted: Vec<String> = server
        .tmux_ok(&["list-panes", "-s", "-t", "alpha", "-F", "#{pane_id}"])
        .lines()
        .filter(|pane| server.pane_opt(pane, "@agent_kind_hint") == "claude")
        .map(str::to_string)
        .collect();
    assert_eq!(
        hinted.len(),
        1,
        "exactly one restored pane should carry @agent_kind_hint=claude"
    );
    // And it is marked as being taken, as `agent start` marks the agent it launches: the resumed
    // agent's first idle is no finished turn.
    assert_eq!(server.pane_opt(&hinted[0], "@agent_acquiring"), "taken");

    // A second restore with everything already live is a no-op that reports it.
    let again = server.ok(
        &["session", "restore", "--name", "resume"],
        "cli:session:restore",
        "session_restored",
    );
    assert_eq!(again["created"].as_array().unwrap().len(), 0, "{again}");
    assert_eq!(again["skipped"].as_array().unwrap().len(), 2, "{again}");
}

/// The plugin registry and one action's process environment.
#[test]
fn a_linked_plugin_is_listed_and_its_action_gets_the_documented_environment() {
    let server = Server::start();
    server.write_config(CONFIG);
    let fixture = common::repo_root()
        .join("tests")
        .join("fixtures")
        .join("plugins")
        .join("echo-plugin");

    let linked = server.ok(
        &["plugin", "link", fixture.to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );
    assert_eq!(linked["plugin"]["id"], "echo-plugin", "{linked}");
    assert_eq!(linked["plugin"]["enabled"], true, "{linked}");
    assert_eq!(linked["plugin"]["source"]["kind"], "link", "{linked}");

    let list = server.ok(&["plugin", "list"], "cli:plugin:list", "plugin_list");
    let ids: Vec<&str> = list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["echo-plugin"], "{list}");

    let actions = server.ok(
        &["plugin", "action", "list"],
        "cli:plugin:action-list",
        "plugin_action_list",
    );
    assert!(
        serde_json::to_string(&actions["actions"])
            .unwrap()
            .contains("dump-env"),
        "{actions}"
    );

    // The fixture's `dump-env` action writes every HERDR_*/TAW_* variable it was given to
    // `env.txt` in its own state dir.
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    let invoked = server.ok(
        &[
            "plugin",
            "action",
            "invoke",
            "echo-plugin.dump-env",
            "--pane",
            &public,
        ],
        "cli:plugin:action-invoke",
        "plugin_action_result",
    );
    assert_eq!(invoked["status"], 0, "the action failed: {invoked}");

    let env_path = server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("echo-plugin")
        .join("env.txt");
    let env_text = std::fs::read_to_string(&env_path)
        .unwrap_or_else(|e| panic!("{} was not written: {e}", env_path.display()));

    // `HERDR_BIN_PATH` is how a herdr plugin calls back into the tool: it must be *this*
    // binary, absolutely pathed, not whatever `taw` happens to be on `$PATH`.
    assert!(
        env_text.contains(&format!("HERDR_BIN_PATH={}", taw_bin())),
        "expected HERDR_BIN_PATH={}, got:\n{env_text}",
        taw_bin()
    );
    assert!(env_text.contains("HERDR_ENV=1"), "{env_text}");
    assert!(
        env_text.contains("HERDR_PLUGIN_ID=echo-plugin"),
        "{env_text}"
    );
    assert!(
        env_text.contains(&format!("HERDR_PANE_ID={public}")),
        "expected HERDR_PANE_ID={public}, got:\n{env_text}"
    );
    // herdr's plugins talk to a socket; `taw` has no server, so a plugin that probes for
    // one must find nothing rather than a stale or inherited path.
    assert!(
        !env_text.contains("HERDR_SOCKET_PATH"),
        "HERDR_SOCKET_PATH must never be set:\n{env_text}"
    );

    // Disabling removes it from the action surface without unlinking it.
    server.ok(
        &["plugin", "disable", "echo-plugin"],
        "cli:plugin:disable",
        "ok",
    );
    let list = server.ok(&["plugin", "list"], "cli:plugin:list", "plugin_list");
    assert_eq!(list["plugins"][0]["enabled"], false, "{list}");
    let (code, env) = server.taw(&[
        "plugin",
        "action",
        "invoke",
        "echo-plugin.dump-env",
        "--pane",
        &public,
    ]);
    assert_ne!(code, 0, "a disabled plugin's action must not run: {env}");
}

/// Codex holds a new hook until the user reviews it, so installing its integration, and its status
/// after, say that Codex asks once and that taw's hook must be trusted there.
#[test]
fn the_codex_integration_says_codex_asks_to_review_taws_hook() {
    let server = Server::start();
    server.write_config(CONFIG);
    let codex_home = tempfile::tempdir().unwrap();
    let env = [("CODEX_HOME", codex_home.path().to_str().unwrap())];
    let review = "Codex asks once to review the new hook (Hooks need review): trust taw's hook \
                  there, or it never runs and codex sessions are not kept for a restore.";

    let installed = server.taw_with_env(&["integration", "install", "codex"], &env);
    assert_eq!(installed.0, 0, "install failed: {}", installed.1);
    let result = &installed.1["result"];
    assert_eq!(result["type"], "integration_installed", "{result}");
    assert_eq!(
        result["message"],
        format!("installed codex integration. {review}"),
        "{result}"
    );

    let status = server.taw_with_env(&["integration", "status", "codex"], &env);
    assert_eq!(status.0, 0, "status failed: {}", status.1);
    let codex = &status.1["result"]["integrations"][0];
    assert_eq!(codex["installed"], true, "{codex}");
    assert_eq!(codex["outdated"], false, "{codex}");
    assert_eq!(codex["note"], review, "{codex}");
}

/// Installing an integration writes the agent's hook and leaves its
/// `settings.json` exactly as it was once uninstalled.
#[test]
fn installing_and_uninstalling_the_claude_integration_is_byte_for_byte_reversible() {
    let server = Server::start();
    server.write_config(CONFIG);

    // `integration install` refuses a config dir that does not exist (the agent is not
    // installed), so the tempdir stands in for `~/.claude` and holds a settings file with
    // a value of the user's own that must survive the round trip.
    let claude_home = tempfile::tempdir().unwrap();
    let settings = claude_home.path().join("settings.json");
    let original = "{\n  \"theme\": \"dark\",\n  \"hooks\": {\n    \"Notification\": [\n      {\n        \"matcher\": \"*\",\n        \"hooks\": [ { \"type\": \"command\", \"command\": \"echo mine\" } ]\n      }\n    ]\n  }\n}\n";
    std::fs::write(&settings, original).unwrap();
    let claude_env = [("CLAUDE_CONFIG_DIR", claude_home.path().to_str().unwrap())];

    let installed = server.taw_with_env(&["integration", "install", "claude"], &claude_env);
    assert_eq!(installed.0, 0, "install failed: {}", installed.1);
    let result = &installed.1["result"];
    assert_eq!(result["type"], "integration_installed", "{result}");
    assert_eq!(result["agent"], "claude", "{result}");

    let hook_script = claude_home.path().join("hooks").join("taw-agent-state.sh");
    assert!(
        hook_script.is_file(),
        "the hook script should exist at {}",
        hook_script.display()
    );
    let script_text = std::fs::read_to_string(&hook_script).unwrap();
    assert!(
        script_text.contains(taw_bin()),
        "the hook script should call this binary:\n{script_text}"
    );

    // The hook entry was added to settings.json, and the user's own entry left alone.
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["theme"], "dark", "{after}");
    assert_eq!(
        after["hooks"]["Notification"][0]["hooks"][0]["command"], "echo mine",
        "an unrelated hook must survive: {after}"
    );
    let session_start = serde_json::to_string(&after["hooks"]["SessionStart"]).unwrap();
    assert!(
        session_start.contains("taw-agent-state.sh"),
        "the SessionStart hook should invoke our script: {session_start}"
    );

    // `integration status` reports it as installed.
    let status = server.taw_with_env(&["integration", "status", "claude"], &claude_env);
    assert_eq!(status.0, 0, "{}", status.1);
    assert_eq!(
        status.1["result"]["type"], "integration_status",
        "{}",
        status.1
    );

    // --- uninstall reverts the file byte for byte ----------------------------------------
    let uninstalled = server.taw_with_env(&["integration", "uninstall", "claude"], &claude_env);
    assert_eq!(uninstalled.0, 0, "uninstall failed: {}", uninstalled.1);
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "uninstall must restore settings.json byte for byte"
    );
    assert!(
        !hook_script.exists(),
        "the hook script should be removed by uninstall"
    );

    // An agent herdr supports but this port has no hook script for reports `unsupported`
    // in the envelope rather than failing at the clap level.
    let (code, env) = server.taw_with_env(&["integration", "install", "gemini"], &claude_env);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "unsupported", "{env}");
}
