//! taw on PATH in every pane with nothing to set up: when the plugin loads, tmux's global environment
//! PATH gets, in front, a folder of taw's own state folder (`bin`) that holds one entry, a `taw` link to
//! the binary the plugin resolved. `@agent-wizard-path off` turns it off, and a reload takes the folder
//! back out.
//!
//! Real tmux on private servers; the plugin is sourced only through that server's `run-shell`. The server's
//! state folder is the test's own (`TAW_STATE_DIR`), so the folder is made inside it.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, taw_bin, wait_for};

/// Ceiling for a pane to start and write, with other test files running beside this one.
const SETTLE: Duration = Duration::from_secs(20);

const OPTION: &str = "@agent-wizard-path";

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

/// The folder the plugin makes: `bin` in the server's state folder.
fn folder(server: &Server) -> PathBuf {
    server.state_dir().join("bin")
}

/// tmux's global PATH, or `None` when the global environment has none.
fn global_path(server: &Server) -> Option<String> {
    let line = server.tmux(&["show-environment", "-g", "PATH"]).ok()?;
    line.strip_prefix("PATH=").map(str::to_string)
}

fn entries_equal_to(path: &str, dir: &Path) -> usize {
    path.split(':')
        .filter(|entry| *entry == dir.to_str().unwrap())
        .count()
}

/// The names in the folder.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// What `command -v taw` prints in a pane created now, and what `taw --version` prints there.
///
/// The pane is made by a `tmux new-window` that a `run-shell` job runs, not by the test's own tmux
/// command: tmux gives a pane the PATH of the command's client when that client is not attached to a
/// session, so a command typed from the test process would give the pane the test's PATH. A job's client
/// has the server's global environment, which is what a key binding or a hook (an attached client's
/// command) gives a pane as well.
fn new_pane_taw(server: &Server, label: &str) -> (String, String) {
    let out = server.work_dir().join(format!("pane-{label}.txt"));
    // Written beside its name and renamed over it, so a file that is there is all there.
    let pane = format!(
        "{{ command -v taw; taw --version; }} > '{0}.part' 2>&1; mv '{0}.part' '{0}'",
        out.display()
    );
    // $TMUX is set in a job, so this `tmux` is this server's, and a job without it makes none.
    server.tmux_ok(&[
        "run-shell",
        &format!("[ -n \"$TMUX\" ] && tmux new-window -d -t one: \"{pane}\""),
    ]);
    assert!(
        wait_for(|| out.exists(), SETTLE),
        "the new pane never wrote what it found"
    );
    let text = std::fs::read_to_string(&out).unwrap();
    let (found, version) = text.split_once('\n').unwrap_or((&text, ""));
    (found.to_string(), version.trim().to_string())
}

fn version_of(bin: &Path) -> String {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_load_puts_a_folder_with_one_taw_link_in_front_of_path_and_a_new_pane_finds_taw() {
    let server = server();
    let before = global_path(&server).expect("the server has a PATH");
    let dir = folder(&server);
    server.source_plugin();

    let path = global_path(&server).expect("PATH after the load");
    assert_eq!(
        path,
        format!("{}:{before}", dir.display()),
        "the folder in front, every other entry as it was"
    );
    assert_eq!(names_in(&dir), vec!["taw".to_string()], "only the link");
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        PathBuf::from(taw_bin()),
        "the link leads to the binary the plugin resolved"
    );
    assert_eq!(
        version_of(&dir.join("taw")),
        version_of(Path::new(taw_bin()))
    );

    let (found, version) = new_pane_taw(&server, "on");
    assert_eq!(found, dir.join("taw").display().to_string());
    assert_eq!(version, version_of(Path::new(taw_bin())));
}

#[test]
fn a_second_load_leaves_one_copy_and_a_stale_link_is_moved_to_the_new_binary() {
    let server = server();
    let dir = folder(&server);
    let other = server.work_dir().join("older");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(taw_bin(), other.join("taw")).unwrap();
    common::make_executable(other.join("taw"));
    server.tmux_ok(&[
        "run-shell",
        &format!(
            "TAW_BIN='{}' bash '{}'",
            other.join("taw").display(),
            common::plugin_script().display()
        ),
    ]);
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        other.join("taw"),
        "the first load links the binary it resolved"
    );
    let first = global_path(&server).unwrap();
    assert_eq!(entries_equal_to(&first, &dir), 1);

    server.source_plugin();
    let path = global_path(&server).unwrap();
    assert_eq!(entries_equal_to(&path, &dir), 1, "{path}");
    assert_eq!(path, first, "a reload changes nothing in PATH");
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        PathBuf::from(taw_bin()),
        "a link to an old binary is replaced by one to the new"
    );
    assert_eq!(names_in(&dir), vec!["taw".to_string()]);

    // A copy that is already in PATH, further back, is moved to the front, not added again.
    // (The rest of PATH is what the server had, so that the plugin script still finds tmux.)
    let rest = first.strip_prefix(&format!("{}:", dir.display())).unwrap();
    server.tmux_ok(&[
        "set-environment",
        "-g",
        "PATH",
        &format!("/usr/bin:{}:{rest}", dir.display()),
    ]);
    server.source_plugin();
    let path = global_path(&server).unwrap();
    assert_eq!(path, format!("{}:/usr/bin:{rest}", dir.display()));
}

/// A copy of the plugin (its script and `scripts`) in the server's work folder, with no `bin/taw` and no
/// `target/release/taw` beside it, so that the only way it can find taw is on PATH. Returns the script.
fn plugin_copy_without_a_binary(server: &Server) -> PathBuf {
    let root = server.work_dir().join("plugin-copy");
    std::fs::create_dir_all(&root).unwrap();
    let source = common::repo_root();
    std::fs::copy(
        source.join("tmux-agent-wizard.tmux"),
        root.join("tmux-agent-wizard.tmux"),
    )
    .unwrap();
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(source.join("scripts"))
        .arg(root.join("scripts"))
        .status()
        .unwrap();
    assert!(status.success());
    root.join("tmux-agent-wizard.tmux")
}

/// Every hook entry the server holds, as `show-hooks -g` prints them.
fn hook_lines(server: &Server) -> Vec<String> {
    let text = server.tmux_ok(&["show-hooks", "-g"]);
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| line.contains("taw") || line.contains("@agent_wizard"))
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

#[test]
fn a_taw_found_only_on_path_is_linked_and_a_reload_neither_links_the_link_nor_stacks_the_hooks() {
    let server = server();
    let dir = folder(&server);
    let script = plugin_copy_without_a_binary(&server);
    // The only taw there is: one in a folder of the user's, at the front of the server's PATH.
    let users = server.work_dir().join("users-bin");
    std::fs::create_dir_all(&users).unwrap();
    std::os::unix::fs::symlink(taw_bin(), users.join("taw")).unwrap();
    let original = global_path(&server).unwrap();
    server.tmux_ok(&[
        "set-environment",
        "-g",
        "PATH",
        &format!("{}:{original}", users.display()),
    ]);
    let load = || {
        server.tmux_ok(&[
            "run-shell",
            &format!("env -u TAW_BIN bash '{}'", script.display()),
        ])
    };

    load();
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        users.join("taw"),
        "the first load links the taw it found on PATH"
    );
    let hooks = hook_lines(&server);
    assert!(
        hooks.len() >= 3,
        "the load installs the plugin's hooks: {hooks:?}"
    );
    let path = global_path(&server).unwrap();
    assert_eq!(entries_equal_to(&path, &dir), 1, "{path}");

    load();
    load();
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        users.join("taw"),
        "a reload, which finds the folder in front of PATH, leaves the link on the binary"
    );
    assert_eq!(names_in(&dir), vec!["taw".to_string()]);
    assert_eq!(
        hook_lines(&server),
        hooks,
        "each hook is there as often as after the first load"
    );
    assert_eq!(global_path(&server).unwrap(), path);

    // A newer taw earlier on PATH than the older one is linked by a reload, which the link in front
    // of PATH would otherwise hide.
    let newer = server.work_dir().join("newer-bin");
    std::fs::create_dir_all(&newer).unwrap();
    std::os::unix::fs::symlink(taw_bin(), newer.join("taw")).unwrap();
    let rest = path.strip_prefix(&format!("{}:", dir.display())).unwrap();
    server.tmux_ok(&[
        "set-environment",
        "-g",
        "PATH",
        &format!("{}:{}:{rest}", dir.display(), newer.display()),
    ]);
    load();
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        newer.join("taw"),
        "the taw found beyond the plugin's own folder is the one linked"
    );
}

#[test]
fn a_binary_that_is_the_folders_own_link_never_makes_the_link_point_at_itself() {
    let server = server();
    let dir = folder(&server);
    server.source_plugin();
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        PathBuf::from(taw_bin())
    );
    // A user whose `@agent-wizard-bin` names the link: the resolved binary is the link itself.
    server.tmux_ok(&[
        "set-option",
        "-g",
        "@agent-wizard-bin",
        &dir.join("taw").display().to_string(),
    ]);
    // Without TAW_BIN, which would come first.
    server.tmux_ok(&[
        "run-shell",
        &format!(
            "env -u TAW_BIN bash '{}'",
            common::plugin_script().display()
        ),
    ]);
    assert_eq!(
        std::fs::read_link(dir.join("taw")).unwrap(),
        PathBuf::from(taw_bin()),
        "the link still leads to the binary, and not to itself"
    );
    assert_eq!(
        version_of(&dir.join("taw")),
        version_of(Path::new(taw_bin()))
    );
}

#[test]
fn off_and_a_reload_takes_the_folder_out_and_keeps_the_rest_of_path_byte_for_byte() {
    let server = server();
    let dir = folder(&server);
    // Empty entries, a trailing one included, stay as they were.
    let original = format!("{}::/nonexistent/one:", global_path(&server).unwrap());
    server.tmux_ok(&["set-environment", "-g", "PATH", &original]);
    server.source_plugin();
    let on = global_path(&server).unwrap();
    assert_eq!(on, format!("{}:{original}", dir.display()));

    server.tmux_ok(&["set-option", "-g", OPTION, "off"]);
    server.source_plugin();
    assert_eq!(global_path(&server).as_deref(), Some(original.as_str()));

    // A new pane has no taw now (when the user has none of their own).
    let (found, _) = new_pane_taw(&server, "off");
    assert!(
        !found.starts_with(&format!("{}/", dir.display())),
        "a pane after the off reload found {found}"
    );

    // On again (the option removed brings the default back).
    server.tmux_ok(&["set-option", "-g", "-u", OPTION]);
    server.source_plugin();
    assert_eq!(global_path(&server).unwrap(), on);
}

#[test]
fn off_from_the_start_leaves_path_and_the_state_folder_alone() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", OPTION, "off"]);
    let before = global_path(&server).unwrap();
    server.source_plugin();
    assert_eq!(global_path(&server).as_deref(), Some(before.as_str()));
    assert!(
        !folder(&server).exists(),
        "no folder is made when it is off"
    );
}

#[test]
fn the_docs_say_taw_is_on_path_and_name_the_option_and_the_link_line() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let read = |name: &str| std::fs::read_to_string(root.join(name)).unwrap();
    let install = read("docs/install.md")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(install.contains(
        "`taw` is on `PATH` in every pane and window you open from inside tmux after the plugin loads"
    ));
    assert!(install.contains("the first pane of a session you start from a terminal"));
    assert!(install.contains("ln -s ~/.local/state/tmux-agent-wizard/bin/taw ~/.local/bin/taw"));
    assert!(read("docs/config.md").contains("| `@agent-wizard-path` | `on` |"));
    let skill = read("SKILL.md");
    assert!(skill.contains("call `taw` by name"));
    assert!(skill.contains("When `command -v taw` finds nothing, use the full path"));
    assert!(skill.contains("/tmux-agent-wizard}/bin/taw"));
    let compat = read("docs/COMPAT.md");
    assert!(compat.contains("`@agent-wizard-path off`"));
    assert!(compat.contains("`path_helper`"));
    assert!(compat.contains("`update-environment`"));
}
