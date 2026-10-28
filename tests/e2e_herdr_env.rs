//! herdr's environment for the agents that report to herdr themselves (Command Code, Crush, Kiro CLI,
//! Muse Code, Prime Agent): `@agent-wizard-herdr-env`, off unless `on`. With it on the plugin gives
//! tmux's global environment `HERDR_ENV=1`, `HERDR_BIN_PATH` (the taw binary the plugin runs) and
//! `HERDR_PANE_ID=%calling`, which taw's pane verbs read as the pane the process runs in (`$TMUX_PANE`).
//!
//! Real tmux on private servers; the plugin is sourced only through that server's `run-shell`. A pane's
//! environment is read as names and the values of the three herdr variables only, never printed whole.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, taw_bin, wait_for, write_script};

/// Ceiling for a pane to start and write, a report to land, with other test files running beside this one.
const SETTLE: Duration = Duration::from_secs(20);

const OPTION: &str = "@agent-wizard-herdr-env";
const NAMES: [&str; 3] = ["HERDR_ENV", "HERDR_BIN_PATH", "HERDR_PANE_ID"];

/// Keeps the plugin's startup one-shots off the network and out of the host's notifier.
const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

fn server() -> Server {
    let server = Server::start();
    server.write_config(CONFIG);
    server
}

/// What tmux's global environment holds for one of the three herdr names: `Some("1")`, or `None` when it
/// is not set (a name flagged for removal counts as not set).
fn global(server: &Server, name: &str) -> Option<String> {
    assert!(NAMES.contains(&name), "only the herdr names are read");
    let line = server.tmux(&["show-environment", "-g", name]).ok()?;
    line.strip_prefix(&format!("{name}=")).map(str::to_string)
}

/// What a pane created now (in the first session) starts with for the three herdr names, as
/// `(name, value)` for each one it has.
fn new_pane_herdr_env(server: &Server, label: &str) -> Vec<(String, String)> {
    let out = server.work_dir().join(format!("herdr-{label}.txt"));
    // Written beside its name and renamed over it, so a file that is there is all there.
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        &format!(
            "env | grep '^HERDR_' > '{0}.part'; mv '{0}.part' '{0}'",
            out.display()
        ),
    ]);
    assert!(
        wait_for(|| out.exists(), SETTLE),
        "the new pane never wrote its environment"
    );
    std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn names(env: &[(String, String)]) -> Vec<&str> {
    env.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn off_by_default_the_plugin_sets_none_of_the_three() {
    let server = server();
    server.source_plugin();
    for name in NAMES {
        assert_eq!(global(&server, name), None, "{name} must not be set");
    }
    assert!(
        new_pane_herdr_env(&server, "default").is_empty(),
        "a new pane must carry none of them"
    );

    // Any value but `on` is off.
    server.tmux_ok(&["set-option", "-g", OPTION, "yes"]);
    server.source_plugin();
    for name in NAMES {
        assert_eq!(global(&server, name), None, "{name} set by `yes`");
    }
}

#[test]
fn on_the_plugin_sets_three_values_and_a_new_pane_has_them() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();

    assert_eq!(global(&server, "HERDR_ENV").as_deref(), Some("1"));
    assert_eq!(
        global(&server, "HERDR_PANE_ID").as_deref(),
        Some("%calling")
    );
    let bin = global(&server, "HERDR_BIN_PATH").expect("HERDR_BIN_PATH is set");
    assert_eq!(bin, taw_bin(), "the taw binary the plugin runs");
    assert!(Path::new(&bin).is_absolute());

    let pane = new_pane_herdr_env(&server, "on");
    let value = |name: &str| {
        pane.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(value("HERDR_ENV"), Some("1"), "{:?}", names(&pane));
    assert_eq!(value("HERDR_BIN_PATH"), Some(bin.as_str()));
    assert_eq!(value("HERDR_PANE_ID"), Some("%calling"));
    assert_eq!(pane.len(), 3, "{:?}", names(&pane));

    // Loading again changes nothing.
    server.source_plugin();
    assert_eq!(
        global(&server, "HERDR_PANE_ID").as_deref(),
        Some("%calling")
    );
}

#[test]
fn a_relative_bin_option_still_gives_an_absolute_bin_path() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    // The plugin reads TAW_BIN first; a relative one must not reach the environment as it is.
    let dir = server.work_dir().join("rel");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(taw_bin(), dir.join("taw")).unwrap();
    common::make_executable(dir.join("taw"));
    server.tmux_ok(&[
        "run-shell",
        &format!(
            "cd '{}' && TAW_BIN=./taw bash '{}'",
            dir.display(),
            common::plugin_script().display()
        ),
    ]);
    let bin = global(&server, "HERDR_BIN_PATH").expect("set");
    assert!(Path::new(&bin).is_absolute(), "got a relative path");
    assert!(bin.ends_with("/rel/taw"), "{bin}");
}

#[test]
fn a_bin_path_with_a_space_is_one_value_and_a_pane_can_run_it() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    // The binary the plugin finds through @agent-wizard-bin (no TAW_BIN), in a directory with a space in
    // its name: tmux's set-environment takes it as one argument only if the plugin quotes it.
    let dir = server.work_dir().join("a dir with spaces");
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("taw");
    std::fs::copy(taw_bin(), &bin).unwrap();
    common::make_executable(&bin);
    let bin = bin.display().to_string();
    server.tmux_ok(&["set-option", "-g", "@agent-wizard-bin", &bin]);
    server.tmux_ok(&[
        "run-shell",
        &format!("bash '{}'", common::plugin_script().display()),
    ]);

    assert_eq!(
        global(&server, "HERDR_BIN_PATH").as_deref(),
        Some(bin.as_str())
    );
    let out = server.work_dir().join("spaced-version.txt");
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        &format!(
            "\"$HERDR_BIN_PATH\" --version > '{0}.part' 2>&1; mv '{0}.part' '{0}'",
            out.display()
        ),
    ]);
    assert!(
        wait_for(|| out.exists(), SETTLE),
        "the pane never ran the binary"
    );
    let version = std::fs::read_to_string(&out).unwrap();
    assert!(
        version.contains("taw"),
        "the pane could not run it: {version}"
    );
}

#[test]
fn on_then_off_and_reloaded_removes_them_but_not_a_value_put_there_by_hand() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    assert!(global(&server, "HERDR_ENV").is_some());

    server.tmux_ok(&["set-option", "-g", OPTION, "off"]);
    server.source_plugin();
    for name in NAMES {
        assert_eq!(global(&server, name), None, "{name} after off");
    }
    assert!(
        new_pane_herdr_env(&server, "after-off").is_empty(),
        "a new pane after the off reload carries none"
    );

    // Unset option, too: on, then the option removed.
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    server.tmux_ok(&["set-option", "-gu", OPTION]);
    server.source_plugin();
    assert_eq!(
        global(&server, "HERDR_ENV"),
        None,
        "after the option is unset"
    );

    // A value an outer herdr (or the user) put there is not the plugin's: the off reload leaves all
    // three alone.
    server.tmux_ok(&["set-environment", "-g", "HERDR_ENV", "1"]);
    server.tmux_ok(&["set-environment", "-g", "HERDR_PANE_ID", "p_1"]);
    server.tmux_ok(&["set-environment", "-g", "HERDR_BIN_PATH", "/opt/herdr"]);
    server.source_plugin();
    assert_eq!(global(&server, "HERDR_PANE_ID").as_deref(), Some("p_1"));
    assert_eq!(global(&server, "HERDR_ENV").as_deref(), Some("1"));
    assert_eq!(
        global(&server, "HERDR_BIN_PATH").as_deref(),
        Some("/opt/herdr")
    );
}

#[test]
fn on_over_a_value_put_there_by_hand_takes_the_three() {
    let server = server();
    server.tmux_ok(&["set-environment", "-g", "HERDR_PANE_ID", "p_1"]);
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    assert_eq!(
        global(&server, "HERDR_PANE_ID").as_deref(),
        Some("%calling")
    );
}

/// A fake agent written to herdr's guide: it does nothing unless `HERDR_ENV=1` and `HERDR_PANE_ID` and
/// `HERDR_BIN_PATH` are set, then reports working as a Crush, waits for `release`, and releases.
fn fake_crush(dir: &Path) -> PathBuf {
    let script = dir.join("guide-agent");
    write_script(
        &script,
        &format!(
            r#"dir='{dir}'
if [ "$HERDR_ENV" != "1" ] || [ -z "$HERDR_PANE_ID" ] || [ -z "$HERDR_BIN_PATH" ]; then
  echo skipped > "$dir/outcome.part"; mv "$dir/outcome.part" "$dir/outcome"
  exit 0
fi
"$HERDR_BIN_PATH" pane report-agent "$HERDR_PANE_ID" --source custom:crush --agent crush --state working --seq 1 > "$dir/report.out" 2>&1
echo "reported $?" > "$dir/outcome.part"; mv "$dir/outcome.part" "$dir/outcome"
while [ ! -e "$dir/release" ]; do sleep 0.1; done
"$HERDR_BIN_PATH" pane release-agent "$HERDR_PANE_ID" --source custom:crush --agent crush --seq 2 > "$dir/release.out" 2>&1
echo "released $?" > "$dir/released.part"; mv "$dir/released.part" "$dir/released"
echo done
sleep 30
"#,
            dir = dir.display()
        ),
    );
    script
}

fn listed_panes(server: &Server) -> Vec<(String, String)> {
    let (code, env) = server.taw(&["agent", "list"]);
    assert_eq!(code, 0, "{env}");
    env["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["pane_id"].as_str().unwrap_or("").to_string(),
                a["agent"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn outcome(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

#[test]
fn a_fake_agent_written_to_herdrs_guide_reports_to_taw_with_the_setting_on() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    let dir = server.work_dir().join("guide-on");
    std::fs::create_dir_all(&dir).unwrap();
    let script = fake_crush(&dir);

    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "one:",
        &format!("sh '{}'", script.display()),
    ]);
    assert!(
        wait_for(|| outcome(&dir, "outcome").is_some(), SETTLE),
        "the fake agent never ran"
    );
    let public = server.public_pane_id(&pane);
    assert_eq!(
        outcome(&dir, "outcome").as_deref(),
        Some("reported 0"),
        "report-agent said {:?}",
        outcome(&dir, "report.out")
    );
    assert!(
        listed_panes(&server)
            .iter()
            .any(|(p, a)| *p == public && a == "crush"),
        "agent list: {:?}",
        listed_panes(&server)
    );

    std::fs::write(dir.join("release"), "").unwrap();
    assert!(
        wait_for(|| outcome(&dir, "released").is_some(), SETTLE),
        "release never ran"
    );
    assert_eq!(
        outcome(&dir, "released").as_deref(),
        Some("released 0"),
        "release-agent said {:?}",
        outcome(&dir, "release.out")
    );
    // The release gives the pane back to the watcher, which reads it on its next output and finds a
    // plain script.
    assert!(
        wait_for(
            || listed_panes(&server).iter().all(|(p, _)| *p != public),
            SETTLE
        ),
        "still listed after the release: {:?}",
        listed_panes(&server)
    );
}

#[test]
fn the_same_fake_agent_reports_nothing_with_the_setting_off() {
    let server = server();
    server.source_plugin();
    let dir = server.work_dir().join("guide-off");
    std::fs::create_dir_all(&dir).unwrap();
    let script = fake_crush(&dir);

    let pane = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "one:",
        &format!("sh '{}'", script.display()),
    ]);
    assert!(
        wait_for(|| outcome(&dir, "outcome").is_some(), SETTLE),
        "the fake agent never ran"
    );
    assert_eq!(outcome(&dir, "outcome").as_deref(), Some("skipped"));
    let _ = pane;
    assert!(
        listed_panes(&server).is_empty(),
        "agent list: {:?}",
        listed_panes(&server)
    );
}

/// A plugin whose startup hook, two actions and one pane each write the four pane-naming variables
/// (`HERDR_PANE_ID`, `TAW_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `unset` for one the process
/// does not have) to a file in the plugin's state directory, and nothing else of its environment.
fn link_probe_plugin(server: &Server) -> PathBuf {
    let dir = server.work_dir().join("envprobe");
    std::fs::create_dir_all(&dir).unwrap();
    let writer = |file: &str| {
        format!(
            "printf \"%s|%s|%s|%s\" \"${{HERDR_PANE_ID-unset}}\" \"${{TAW_PANE_ID-unset}}\" \
             \"${{HERDR_TAB_ID-unset}}\" \"${{HERDR_WORKSPACE_ID-unset}}\" > \
             \"$HERDR_PLUGIN_STATE_DIR/{file}.part\"; mv \"$HERDR_PLUGIN_STATE_DIR/{file}.part\" \
             \"$HERDR_PLUGIN_STATE_DIR/{file}\""
        )
    };
    let manifest = format!(
        "id = \"envprobe\"\nversion = \"0.1.0\"\nmin_herdr_version = \"0.7.0\"\n\n\
         [[startup]]\ncommand = ['sh', '-c', '{startup}']\n\n\
         [[actions]]\nid = \"with-pane\"\ncontexts = [\"pane\"]\ncommand = ['sh', '-c', '{with}']\n\n\
         [[actions]]\nid = \"without\"\ncontexts = [\"pane\"]\ncommand = ['sh', '-c', '{without}']\n\n\
         [[panes]]\nid = \"board\"\nplacement = \"bottom\"\ncommand = ['sh', '-c', '{board}; sleep 30']\n",
        startup = writer("startup.txt"),
        with = writer("with-pane.txt"),
        without = writer("without.txt"),
        board = writer("board.txt"),
    );
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    let linked = server.taw(&["plugin", "link", dir.to_str().unwrap()]);
    assert_eq!(linked.0, 0, "{}", linked.1);
    server
        .state_dir()
        .join("plugins")
        .join("state")
        .join("envprobe")
}

fn probe_result(dir: &Path, file: &str) -> String {
    assert!(
        wait_for(|| dir.join(file).exists(), SETTLE),
        "{file} was never written"
    );
    std::fs::read_to_string(dir.join(file)).unwrap()
}

#[test]
fn a_plugin_startup_hook_with_no_pane_does_not_inherit_the_pane_word() {
    let server = server();
    let state = link_probe_plugin(&server);
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    // The plugin's startup is a run-shell job, which has tmux's global environment: HERDR_PANE_ID is
    // `%calling` there, and herdr gives a startup hook no pane at all.
    assert_eq!(
        global(&server, "HERDR_PANE_ID").as_deref(),
        Some("%calling")
    );
    assert_eq!(
        probe_result(&state, "startup.txt"),
        "unset|unset|unset|unset"
    );
}

/// The public ids a pane lives under: its own, its tab's and its workspace's, as the probe plugin
/// writes them (`HERDR_PANE_ID`, `TAW_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`). In herdr a pane
/// lives in one tab of one workspace, so an invocation given only a pane has all three.
fn pane_ids(server: &Server, pane: &str) -> String {
    let got = server.ok(&["pane", "get", pane], "cli:pane:get", "pane");
    let text = |key: &str| got["pane"][key].as_str().unwrap().to_string();
    let public = text("pane_id");
    format!(
        "{public}|{public}|{}|{}",
        text("tab_id"),
        text("workspace_id")
    )
}

/// A plugin action invoked from a pane: with `--pane` it has that pane, with its tab and workspace; without
/// one, its context is filled from the pane it runs in, as herdr fills a missing context from the focused pane,
/// and never with the `%calling` the pane inherited. The pane it runs in is a background window's, so the fill
/// is that pane and not the session's active one. (No case of an action with no pane is left while tmux runs:
/// outside any pane the fill is the current session's active pane.)
#[test]
fn a_plugin_action_has_the_pane_it_was_given_or_else_the_pane_it_runs_in_with_its_tab_and_workspace()
 {
    let server = server();
    let state = link_probe_plugin(&server);
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    let pane = server.pane();

    // From a pane created after the load, which has HERDR_PANE_ID=%calling and HERDR_BIN_PATH, as an
    // agent's own shell command would run it, in a window left in the background.
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        &format!(
            "\"$HERDR_BIN_PATH\" plugin action invoke envprobe.with-pane --pane '{pane}' >/dev/null; \
             \"$HERDR_BIN_PATH\" plugin action invoke envprobe.without >/dev/null; sleep 30"
        ),
    ]);
    assert_eq!(
        probe_result(&state, "with-pane.txt"),
        pane_ids(&server, &pane),
        "the pane the context has"
    );
    let runs_in = server.tmux_ok(&["display-message", "-p", "-t", "one:1", "#{pane_id}"]);
    assert_ne!(runs_in.trim(), pane, "the action runs in a pane of its own");
    assert_eq!(
        probe_result(&state, "without.txt"),
        pane_ids(&server, runs_in.trim()),
        "a context with no pane is filled from the pane the action runs in"
    );
}

/// A plugin pane is started by tmux, so it has tmux's global environment, `HERDR_PANE_ID=%calling` with the
/// setting on: with no pane in the invocation's context it must have none of the three ids, and with a
/// `--pane` the real ones.
#[test]
fn a_plugin_pane_has_the_pane_it_was_given_with_its_tab_and_workspace_and_none_where_the_context_has_none()
 {
    let server = server();
    let state = link_probe_plugin(&server);
    server.tmux_ok(&["set-option", "-g", OPTION, "on"]);
    server.source_plugin();
    assert_eq!(
        global(&server, "HERDR_PANE_ID").as_deref(),
        Some("%calling")
    );

    let (code, out) = server.taw(&["plugin", "pane", "open", "envprobe.board"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        probe_result(&state, "board.txt"),
        "unset|unset|unset|unset",
        "pane open with no --pane"
    );

    let pane = server.pane();
    std::fs::remove_file(state.join("board.txt")).unwrap();
    let (code, out) = server.taw(&["plugin", "pane", "open", "envprobe.board", "--pane", &pane]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        probe_result(&state, "board.txt"),
        pane_ids(&server, &pane),
        "pane open --pane"
    );
}

/// `%calling` is every pane verb's word for the pane the process runs in, and the plugin verbs that take
/// a `--pane` are two of them: an agent that hands `"$HERDR_PANE_ID"` on gets the pane it runs in.
#[test]
fn plugin_action_invoke_and_pane_open_take_calling_as_their_pane() {
    let server = server();
    let state = link_probe_plugin(&server);
    let pane = server.pane();

    let (code, env) = server.taw_in_pane(
        &pane,
        &[
            "plugin",
            "action",
            "invoke",
            "envprobe.with-pane",
            "--pane",
            "%calling",
        ],
    );
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        probe_result(&state, "with-pane.txt"),
        pane_ids(&server, &pane),
        "action invoke --pane %calling"
    );

    let (code, env) = server.taw_in_pane(
        &pane,
        &[
            "plugin",
            "pane",
            "open",
            "envprobe.board",
            "--pane",
            "%calling",
        ],
    );
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        probe_result(&state, "board.txt"),
        pane_ids(&server, &pane),
        "pane open --pane %calling"
    );

    // Without a $TMUX_PANE it is the resolver's own error, for both.
    let (code, env) = server.taw(&[
        "plugin",
        "action",
        "invoke",
        "envprobe.with-pane",
        "--pane",
        "%calling",
    ]);
    assert_ne!(code, 0, "{env}");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("no current pane"),
        "{env}"
    );
}
