//! `taw plugin pane open` and the other plugin verbs, called the way herdr's own plugins call them
//! (herdr-reviewr, github-link-preview): herdr's flags and herdr's answer, on a real tmux server and the
//! real `taw` binary.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, wait_for};

/// Ceiling for a pane to start and write, with other test files running beside this one.
const SETTLE: Duration = Duration::from_secs(20);

fn server() -> Server {
    let server = Server::start();
    server.write_config(
        "[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n\n[ui.sound]\nenabled = false\n",
    );
    server
}

/// Links a plugin `probe` whose pane `board` writes its folder and the value of `SECRET` to files in the
/// plugin's state directory, then sleeps. Returns that state directory.
fn link_probe(server: &Server) -> PathBuf {
    let dir = server.work_dir().join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = "id = \"probe\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.7.0\"\n\n\
        [[panes]]\nid = \"board\"\nplacement = \"split\"\n\
        command = ['sh', '-c', 'pwd -P > \"$HERDR_PLUGIN_STATE_DIR/cwd.txt\"; sleep 30']\n";
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("probe")
}

fn read_when_written(dir: &Path, file: &str) -> String {
    assert!(
        wait_for(|| dir.join(file).exists(), SETTLE),
        "{file} was never written"
    );
    std::fs::read_to_string(dir.join(file)).unwrap()
}

/// herdr-reviewr's call, flag for flag: `--target-pane` places the pane beside that pane, `--cwd` is the
/// folder it starts in, and the answer is herdr's `plugin_pane_opened` with `plugin_pane` holding
/// `plugin_id`, `entrypoint` and the pane's record, taw's older fields beside it.
#[test]
fn herdrs_target_pane_and_cwd_place_and_start_the_pane_and_the_answer_is_herdrs() {
    let server = server();
    let state = link_probe(&server);
    let first = server.pane();
    server.tmux_ok(&["new-window", "-d", "-t", "one:", "/bin/sh"]);
    let second = server.tmux_ok(&["display-message", "-p", "-t", "one:1", "#{pane_id}"]);
    let second_window = server.tmux_ok(&["display-message", "-p", "-t", &second, "#{window_id}"]);
    let first_window = server.tmux_ok(&["display-message", "-p", "-t", &first, "#{window_id}"]);
    assert_ne!(first_window, second_window);
    let repo = server.work_dir().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let target = server.public_pane_id(&second);

    let result = server.ok(
        &[
            "plugin",
            "pane",
            "open",
            "--plugin",
            "probe",
            "--entrypoint",
            "board",
            "--placement",
            "split",
            "--target-pane",
            &target,
            "--direction",
            "right",
            "--cwd",
            repo.to_str().unwrap(),
            "--focus",
        ],
        "cli:plugin:pane-open",
        "plugin_pane_opened",
    );

    let info = &result["plugin_pane"];
    assert_eq!(info["plugin_id"], "probe", "{result}");
    assert_eq!(info["entrypoint"], "board", "{result}");
    let pane = &info["pane"];
    let terminal = pane["terminal_id"].as_str().expect("a terminal_id");
    assert_ne!(terminal, second);
    assert_eq!(
        server.tmux_ok(&["display-message", "-p", "-t", terminal, "#{window_id}"]),
        second_window,
        "the pane is not beside --target-pane: {result}"
    );
    assert_eq!(server.public_pane_id(terminal), pane["pane_id"], "{result}");
    for key in [
        "workspace_id",
        "tab_id",
        "focused",
        "agent_status",
        "revision",
    ] {
        assert!(pane.get(key).is_some(), "no {key} in {result}");
    }
    // taw's own fields stay beside herdr's.
    assert_eq!(result["terminal_id"], terminal, "{result}");
    assert_eq!(result["pane"], "probe.board", "{result}");
    assert_eq!(
        read_when_written(&state, "cwd.txt").trim(),
        repo.canonicalize().unwrap().to_str().unwrap(),
        "--cwd was not the pane's folder"
    );
}

/// herdr-reviewr's manifest names its pane's program by a path inside the plugin (`bin/herdr-reviewr`):
/// herdr finds that path in the plugin's folder, while the pane still starts in `--cwd`.
#[test]
fn a_pane_command_inside_the_plugin_is_found_there_while_the_pane_starts_in_cwd() {
    let server = server();
    let dir = server.work_dir().join("relplug");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let manifest = "id = \"relplug\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.7.0\"\n\n\
        [[panes]]\nid = \"board\"\nplacement = \"split\"\ncommand = ['bin/where', 'seen']\n";
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let script = dir.join("bin").join("where");
    std::fs::write(
        &script,
        "#!/bin/sh\npwd -P > \"$HERDR_PLUGIN_STATE_DIR/cwd.part\"\necho \"$1\" >> \"$HERDR_PLUGIN_STATE_DIR/cwd.part\"\n\
         mv \"$HERDR_PLUGIN_STATE_DIR/cwd.part\" \"$HERDR_PLUGIN_STATE_DIR/cwd.txt\"\nsleep 30\n",
    )
    .unwrap();
    crate::common::make_executable(&script);
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    let state = server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("relplug");
    std::fs::create_dir_all(&state).unwrap();
    let repo = server.work_dir().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let target = server.public_pane_id(&server.pane());

    let result = server.ok(
        &[
            "plugin",
            "pane",
            "open",
            "--plugin",
            "relplug",
            "--entrypoint",
            "board",
            "--placement",
            "split",
            "--target-pane",
            &target,
            "--cwd",
            repo.to_str().unwrap(),
        ],
        "cli:plugin:pane-open",
        "plugin_pane_opened",
    );

    assert_eq!(
        read_when_written(&state, "cwd.txt"),
        format!("{}\nseen\n", repo.canonicalize().unwrap().display()),
        "the pane's program did not run in --cwd: {result}"
    );
    assert!(
        result["plugin_pane"]["pane"]["pane_id"].is_string(),
        "the answer carries no pane: {result}"
    );
}

/// herdr fills what a caller leaves out of an action's context from the workspace, tab and pane in front
/// (socket-api.mdx: "Missing context fields are filled from the active workspace, tab, focused pane"):
/// herdr-reviewr's `invoke open --plugin persiyanov.reviewr`, given no flag, needs them. Run in a pane, it
/// is that pane; run outside tmux, the active pane of the session tmux takes as current; `--pane` wins.
#[test]
fn an_action_invoked_without_flags_gets_the_pane_it_runs_in() {
    let server = server();
    let dir = server.work_dir().join("ctxplug");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = r#"id = "d.ctx"
version = "0.1.0"
min_herdr_version = "0.7.0"

[[actions]]
id = "show"
contexts = ["pane"]
command = ['sh', '-c', "printf '%s|%s|%s' \"${HERDR_WORKSPACE_ID-unset}\" \"${HERDR_TAB_ID-unset}\" \"${HERDR_PANE_ID-unset}\""]
"#;
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    let first = server.pane();
    // A second window, left in the background, so the pane the action runs in is not the active one.
    server.tmux_ok(&["new-window", "-d", "-t", "one:", "/bin/sh"]);
    let second = server.tmux_ok(&["display-message", "-p", "-t", "one:1", "#{pane_id}"]);
    let ids = |pane: &str| {
        let got = server.ok(&["pane", "get", pane], "cli:pane:get", "pane");
        let p = &got["pane"];
        format!(
            "{}|{}|{}",
            p["workspace_id"].as_str().unwrap(),
            p["tab_id"].as_str().unwrap(),
            p["pane_id"].as_str().unwrap()
        )
    };
    let shown = |code: i32, answer: &serde_json::Value| {
        assert_eq!(code, 0, "{answer}");
        assert_eq!(answer["result"]["status"], 0, "{answer}");
        answer["result"]["stdout"].as_str().unwrap().to_string()
    };

    let (code, answer) = server.taw_in_pane(&second, &["plugin", "action", "invoke", "d.ctx.show"]);
    assert_eq!(
        shown(code, &answer),
        ids(&second),
        "run in a pane: {answer}"
    );

    let (code, answer) = server.taw_in_pane(
        &second,
        &[
            "plugin", "action", "invoke", "show", "--plugin", "d.ctx", "--pane", &first,
        ],
    );
    assert_eq!(shown(code, &answer), ids(&first), "--pane: {answer}");

    let (code, answer) = server.taw(&["plugin", "action", "invoke", "d.ctx.show"]);
    assert_eq!(shown(code, &answer), ids(&first), "outside tmux: {answer}");

    // `--tab` alone: that tab's active pane.
    let second_tab = ids(&second).split('|').nth(1).unwrap().to_string();
    let (code, answer) = server.taw(&[
        "plugin",
        "action",
        "invoke",
        "d.ctx.show",
        "--tab",
        &second_tab,
    ]);
    assert_eq!(shown(code, &answer), ids(&second), "--tab: {answer}");
}

fn alive(pid: &str) -> bool {
    // A zombie is an ended process whose parent has not reaped it yet.
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .map(|out| {
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}

/// A startup command that puts its work in the background and exits (`cmd &`, a daemon's usual shape)
/// leaves a group with no leader: `disable` ends it as it ends one whose command stays.
#[test]
fn disable_ends_a_startup_command_that_put_itself_in_the_background() {
    let server = server();
    let dir = server.work_dir().join("bgplug");
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = server.work_dir().join("sleeper.pid");
    let manifest = format!(
        "id = \"bgplug\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.7.0\"\n\n\
         [[startup]]\ncommand = ['sh', '-c', 'nohup sleep 300 >/dev/null 2>&1 & echo $! > {}']\n",
        pid_file.display()
    );
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    let started = server.taw(&["plugin", "startup"]);
    assert_eq!(started.0, 0, "{}", started.1);
    assert!(wait_for(|| pid_file.exists(), SETTLE), "no sleeper started");
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    struct Reap(String);
    impl Drop for Reap {
        fn drop(&mut self) {
            // The sleeper this test started, by its pid.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &self.0])
                .status();
        }
    }
    let _reap = Reap(pid.clone());
    assert!(alive(&pid), "the sleeper is not running");

    let disabled = server.taw(&["plugin", "disable", "bgplug"]);
    assert_eq!(disabled.0, 0, "{}", disabled.1);
    assert!(
        wait_for(|| !alive(&pid), SETTLE),
        "disable left the backgrounded startup process {pid} running"
    );
}

/// A popup with no client attached to show it on says so in one short line, and nothing the pane was
/// given (`--env` values, its whole command line) is in the error.
#[test]
fn a_popup_with_no_client_says_so_in_a_short_line_and_holds_no_env_value() {
    let server = server();
    link_probe(&server);
    for placement in ["popup", "overlay"] {
        let (code, answer) = server.taw(&[
            "plugin",
            "pane",
            "open",
            "--plugin",
            "probe",
            "--entrypoint",
            "board",
            "--placement",
            placement,
            "--env",
            "SECRET_TOKEN=abc123",
        ]);
        assert_eq!(code, 1, "{placement}: {answer}");
        assert_eq!(answer["error"]["code"], "tmux_error", "{answer}");
        let message = answer["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("no attached client to open the popup on"),
            "{message}"
        );
        assert!(message.len() < 160, "not short: {message}");
        assert!(!answer.to_string().contains("abc123"), "{answer}");
    }
}

/// herdr's form `invoke ACTION --plugin ID`, the one herdr-reviewr's README gives, runs the action beside
/// the dotted `ID.ACTION`.
#[test]
fn action_invoke_takes_herdrs_plugin_flag_beside_the_dotted_form() {
    let server = server();
    let dir = server.work_dir().join("actplug");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = "id = \"d.act\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.7.0\"\n\n\
        [[actions]]\nid = \"hello\"\ncontexts = [\"pane\"]\ncommand = ['sh', '-c', 'echo hi']\n";
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    let pane = server.public_pane_id(&server.pane());

    for args in [
        vec!["plugin", "action", "invoke", "d.act.hello", "--pane", &pane],
        vec![
            "plugin", "action", "invoke", "hello", "--plugin", "d.act", "--pane", &pane,
        ],
    ] {
        let result = server.ok(&args, "cli:plugin:action-invoke", "plugin_action_result");
        assert_eq!(result["action"], "d.act.hello", "{args:?}: {result}");
        assert_eq!(result["status"], 0, "{args:?}: {result}");
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "hi", "{result}");
    }
}
