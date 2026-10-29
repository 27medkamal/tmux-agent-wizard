//! End-to-end tests for the `taw` CLI: they run the *built binary* against a throwaway
//! `tmux -L <unique>` server and assert on the JSON envelope it prints. Never touches the
//! user's default tmux server (every invocation sets `TAW_TMUX_SOCKET_NAME`) and never
//! writes outside tempdirs (`TAW_CONFIG_DIR`/`TAW_STATE_DIR`).

mod common;

use common::shell_env::ShellHome;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A throwaway tmux server plus the tempdirs the CLI is pointed at, all torn down on
/// drop. See tests/tmux_client.rs for why the socket name carries both the pid and a
/// per-`Fixture` counter (`cargo test` runs these as threads of one process).
struct Fixture {
    socket: String,
    config: tempfile::TempDir,
    state: tempfile::TempDir,
    /// The cwd every pane on this server starts in — a tempdir, so no project config
    /// (`mise.toml`, `.envrc`, direnv, …) in the checkout can run inside a test pane.
    work: tempfile::TempDir,
    /// Where zoxide keeps its database (`_ZO_DATA_DIR`) for every `taw` this fixture runs:
    /// `session open` runs `zoxide add` on the directory it opens, and without a directory
    /// of its own that would write a temp path into the database of whoever runs the tests.
    zoxide: tempfile::TempDir,
    /// The `$HOME` the shells of the server have, and no history file: see `common::shell_env`.
    home: ShellHome,
}

impl Fixture {
    fn start() -> Fixture {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = format!("taw-test-cli-{}-{n}", std::process::id());
        let fixture = Fixture {
            socket,
            config: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
            zoxide: tempfile::tempdir().unwrap(),
            home: ShellHome::new(),
        };
        // No test here makes a request over the network: a watcher (or the plugin's startup one-shot)
        // refreshes the cached manifests from herdr.dev unless the state says that was done a moment
        // ago. (A test that is about the state saying otherwise takes the marker away itself.)
        common::shell_env::quiet_state(fixture.state.path());

        // `-f /dev/null`: a fresh `-L` socket still loads the host's tmux.conf, which
        // would break the fixed 80x24 geometry and index-0 assumptions below.
        //
        // `/bin/sh` as the pane command: `-f /dev/null` isolates tmux's config but not
        // the *shell's*. The host's login shell may print banners, run prompt tools, or
        // open an interactive prompt of its own (a `mise` trust question, say) that
        // swallows the keystrokes `pane run` sends — making every send/read test flaky for
        // reasons that have nothing to do with `taw`. `/bin/sh` reads no rc file and just
        // runs what it is given. It has to be named here *and* as `default-shell` below,
        // because the server does not outlive a `start-server` with no sessions, so there
        // is no earlier moment at which to set the option.
        fixture.home.start_server_with(
            &taw::tmux::Tmux::with_socket_name(&fixture.socket),
            &[
                ("TAW_TMUX_SOCKET_NAME", &fixture.socket),
                ("TAW_CONFIG_DIR", fixture.config.path().to_str().unwrap()),
                ("TAW_STATE_DIR", fixture.state.path().to_str().unwrap()),
            ],
            &[
                "-d",
                "-s",
                "one",
                "-x",
                "80",
                "-y",
                "24",
                "-c",
                fixture.work.path().to_str().unwrap(),
                "/bin/sh",
            ],
        );

        // Every pane created later (splits, new windows, new sessions) gets it too.
        fixture
            .tmux(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        // `automatic-rename` is on by default and renames a window whenever its
        // foreground command changes — so a window this fixture named `bgtab` becomes
        // `sh`, then `bash`, then `tmux` as a test attaches a client into it. Any
        // assertion about `#{window_name}` is then racing tmux's own renaming, which is
        // how `no_focus_leaves_the_attached_client_where_it_was_and_focus_moves_it`
        // intermittently read `bash` where it had just read `tmux`. A test that asks
        // "which window is selected" wants the name it gave, not the one tmux inferred.
        fixture
            .tmux(&["set-option", "-g", "automatic-rename", "off"])
            .unwrap();
        fixture
    }

    /// Runs raw `tmux -L <socket> <args>` — fixture setup only; the CLI itself is always
    /// exercised through [`Fixture::run`].
    ///
    /// The `TAW_*` variables are set here as well as on the CLI, because the *first* of
    /// these calls starts the tmux server and the server's environment is what every
    /// process tmux spawns inherits — including the `pipe-pane` watcher `watch attach`
    /// opens, which is not a child of any `taw` invocation. Without them that watcher
    /// would resolve `Paths::from_env()` to the developer's real `~/.local/state` and
    /// write its log there, which these tests must never do.
    ///
    /// A UTF-8 client (`-u`), as `taw`'s own reads are: in a locale that is not UTF-8
    /// (`LC_ALL=C`) tmux reads every non-ASCII value back as `_`, and a test comparing what
    /// it reads here with what `taw` wrote (the status line's glyphs) would fail for that.
    fn tmux(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("-u")
            .args(args)
            .env("TAW_TMUX_SOCKET_NAME", &self.socket)
            .env("TAW_CONFIG_DIR", self.config.path())
            .env("TAW_STATE_DIR", self.state.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Runs the built `taw` binary against this fixture. Returns `(exit code, stdout,
    /// stderr)`. `TMUX`/`TMUX_PANE` are cleared so a test that cares about "the current
    /// pane" sets them explicitly (via [`Fixture::run_in_pane`]) rather than inheriting
    /// whatever tmux this test suite happens to be running inside.
    fn run_raw(&self, args: &[&str]) -> (i32, String, String) {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_taw"));
        cmd.args(args)
            .env("TAW_TMUX_SOCKET_NAME", &self.socket)
            .env("TAW_CONFIG_DIR", self.config.path())
            .env("TAW_STATE_DIR", self.state.path())
            .env("_ZO_DATA_DIR", self.zoxide.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// Runs `taw` and parses its stdout as the JSON envelope.
    fn run(&self, args: &[&str]) -> (i32, Value) {
        let (code, stdout, stderr) = self.run_raw(args);
        (code, parse_envelope(args, &stdout, &stderr))
    }

    /// Like [`Fixture::run`], with `$TMUX_PANE` set — for the verbs that read "the
    /// current pane" from the environment (`pane current`, `workspace id`).
    fn run_in_pane(&self, pane: &str, args: &[&str]) -> (i32, Value) {
        let (code, stdout, stderr) = self.run_with_env(args, &[("TMUX_PANE", pane)]);
        (code, parse_envelope(args, &stdout, &stderr))
    }

    /// The envelope's `result`, asserting success and the expected `id` and `result.type`.
    fn ok(&self, args: &[&str], id: &str, result_type: &str) -> Value {
        let (code, env) = self.run(args);
        assert_eq!(code, 0, "`taw {}` failed: {env}", args.join(" "));
        assert_eq!(env["id"], id, "wrong envelope id: {env}");
        assert!(env.get("error").is_none(), "unexpected error: {env}");
        let result = env["result"].clone();
        assert_eq!(result["type"], result_type, "wrong result.type: {env}");
        result
    }

    /// `taw config check` over the file as it is now: its exit status must be `code` (1 when it
    /// reports an error, 0 otherwise), and its envelope is the answer either way. Returns the
    /// result, and what the command printed on stderr.
    fn config_check(&self, code: i32) -> (Value, String) {
        let args = ["config", "check"];
        let (got, stdout, stderr) = self.run_raw(&args);
        let env = parse_envelope(&args, &stdout, &stderr);
        assert_eq!(got, code, "`taw config check` exit status: {env}");
        assert_eq!(env["id"], "cli:config:check", "wrong envelope id: {env}");
        assert!(env.get("error").is_none(), "unexpected error: {env}");
        assert_eq!(env["result"]["type"], "config_check", "{env}");
        (env["result"].clone(), stderr)
    }

    /// The public pane id (`w<n>:p<n>`) of the fixture's single starting pane.
    fn first_pane(&self) -> String {
        let panes = self.ok(&["pane", "list"], "cli:pane:list", "pane_list");
        panes["panes"][0]["pane_id"].as_str().unwrap().to_string()
    }

    /// The raw tmux pane id (`%N`) behind a public pane id. Public ids are `taw`'s own
    /// invention, so anything handed to raw `tmux` has to be translated first.
    fn terminal_id(&self, pane: &str) -> String {
        let got = self.ok(&["pane", "get", pane], "cli:pane:get", "pane");
        got["pane"]["terminal_id"].as_str().unwrap().to_string()
    }

    /// One `@agent_*` pane option read straight from tmux — the check that `taw` really
    /// wrote what it claimed, independent of its own read path.
    fn pane_option(&self, pane: &str, name: &str) -> String {
        let terminal = self.terminal_id(pane);
        self.tmux(&["show-options", "-pqv", "-t", &terminal, name])
            .unwrap()
    }

    /// `set-option -p` on a pane, by public id — fixture setup that bypasses `taw`.
    fn set_pane_option(&self, pane: &str, name: &str, value: &str) {
        let terminal = self.terminal_id(pane);
        self.tmux(&["set-option", "-p", "-t", &terminal, name, value])
            .unwrap();
    }
}

/// Parses the CLI's stdout as the JSON envelope, panicking with both streams attached
/// when it is not valid JSON — that is always a CLI bug worth seeing in full.
fn parse_envelope(args: &[&str], stdout: &str, stderr: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|e| {
        let cmd = args.join(" ");
        panic!(
            "`taw {cmd}` printed non-JSON stdout ({e}):\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        )
    })
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

/// A pane no program has titled has no title in the JSON, whatever tmux calls it: tmux's own
/// title for it is the host name. A title a program sets is the `terminal_title` of the pane
/// and of the agent in it, with its glyph stripped in `terminal_title_stripped`.
#[test]
fn a_pane_nobody_titled_has_no_terminal_title_and_a_programs_title_is_kept() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let terminal = f.terminal_id(&pane);
    f.set_pane_option(&pane, "@agent_kind", "claude");

    let titles = |f: &Fixture| {
        let got = f.ok(&["pane", "get", &pane], "cli:pane:get", "pane");
        let listed = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
        let agent = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
        let agents = f.ok(&["agent", "list"], "cli:agent:list", "agent_list");
        [
            got["pane"].clone(),
            listed["panes"][0].clone(),
            agent["agent"].clone(),
            agents["agents"][0].clone(),
        ]
        .map(|record| {
            (
                record["terminal_title"].clone(),
                record["terminal_title_stripped"].clone(),
            )
        })
    };
    for (title, stripped) in titles(&f) {
        assert_eq!(title, "", "no program titled this pane");
        assert_eq!(stripped, "");
    }

    f.tmux(&["select-pane", "-t", &terminal, "-T", "\u{2733} Build"])
        .unwrap();
    for (title, stripped) in titles(&f) {
        assert_eq!(title, "\u{2733} Build");
        assert_eq!(stripped, "Build");
    }
}

#[test]
fn pane_list_prints_an_envelope_with_herdr_shaped_ids() {
    let f = Fixture::start();
    let result = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    let panes = result["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 1, "{result}");
    let p = &panes[0];

    let pane_id = p["pane_id"].as_str().unwrap();
    let re = regex::Regex::new(r"^w\d+:p\d+$").unwrap();
    assert!(re.is_match(pane_id), "pane_id {pane_id:?} is not w<n>:p<n>");
    assert!(
        p["terminal_id"].as_str().unwrap().starts_with('%'),
        "terminal_id should be the raw tmux pane id: {p}"
    );
    assert!(
        regex::Regex::new(r"^w\d+$")
            .unwrap()
            .is_match(p["workspace_id"].as_str().unwrap()),
        "{p}"
    );
    assert!(
        regex::Regex::new(r"^w\d+:t\d+$")
            .unwrap()
            .is_match(p["tab_id"].as_str().unwrap()),
        "{p}"
    );
    // Every documented pane field is present, even when empty.
    for field in [
        "cwd",
        "foreground_cwd",
        "restore_error",
        "focused",
        "agent_status",
        "agent",
        "name",
        "display_agent",
        "title",
        "terminal_title",
        "terminal_title_stripped",
        "revision",
        "scroll",
        "sandbox",
    ] {
        assert!(p.get(field).is_some(), "pane record missing {field}: {p}");
    }
    assert!(p["scroll"].get("viewport_rows").is_some(), "{p}");
    assert_eq!(p["agent_status"], Value::Null, "no agent in a fresh pane");
    assert_eq!(p["restore_error"], Value::Null, "no restore made the pane");

    // `pane current` reads $TMUX_PANE.
    let terminal_id = p["terminal_id"].as_str().unwrap().to_string();
    let current = f.run_in_pane(&terminal_id, &["pane", "current"]);
    assert_eq!(current.0, 0, "{:?}", current.1);
    assert_eq!(current.1["result"]["pane"]["pane_id"], p["pane_id"]);

    // `pane get` by public id, and `pane process-info`.
    let got = f.ok(&["pane", "get", pane_id], "cli:pane:get", "pane");
    assert_eq!(got["pane"]["pane_id"], p["pane_id"]);
    let info = f.ok(
        &["pane", "process-info", "--pane", pane_id],
        "cli:pane:process-info",
        "pane_process_info",
    );
    assert!(info["pid"].as_u64().unwrap() > 0, "{info}");
    assert!(!info["command"].as_str().unwrap().is_empty(), "{info}");

    // herdr's `PaneProcessInfo` under `process_info`: the shell, the terminal, the group in front
    // and its processes, each with the fields a plugin reads.
    let process_info = &info["process_info"];
    assert_eq!(process_info["pane_id"], p["pane_id"], "{info}");
    assert_eq!(process_info["shell_pid"], info["pid"], "{info}");
    assert!(
        process_info["tty"]
            .as_str()
            .is_some_and(|t| t.starts_with("/dev/")),
        "{info}"
    );
    let group = process_info["foreground_process_group_id"].as_u64();
    assert!(group.is_some_and(|g| g > 0), "{info}");
    let processes = process_info["foreground_processes"].as_array().unwrap();
    assert!(!processes.is_empty(), "no process in front: {info}");
    let leader = processes
        .iter()
        .find(|q| q["pid"].as_u64() == group)
        .unwrap_or_else(|| panic!("the group's leader is not listed: {info}"));
    for field in ["name", "argv0", "cmdline", "cwd"] {
        assert!(
            leader[field].as_str().is_some_and(|v| !v.is_empty()),
            "{field}: {info}"
        );
    }
    assert!(!leader["argv"].as_array().unwrap().is_empty(), "{info}");
}

/// A pane kept after its program ended (`remain-on-exit`) is still listed, and has no process: tmux 3.8
/// prints no `#{pane_pid}` for it (and the line used to be dropped from every listing), 3.6b prints the
/// pid of the program that ended. `pane process-info` says `pid: null` for it on both, as herdr's
/// `shell_pid` is null for a pane with no process.
#[test]
fn a_pane_kept_after_its_program_ended_is_listed_and_has_no_pid() {
    let f = Fixture::start();
    f.tmux(&["set-option", "-g", "remain-on-exit", "on"])
        .unwrap();
    let dead = f
        .tmux(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "one",
            "exit 0",
        ])
        .unwrap();
    assert!(
        common::wait_for(
            || f.tmux(&["display-message", "-p", "-t", &dead, "#{pane_dead}"])
                .is_ok_and(|dead| dead == "1"),
            std::time::Duration::from_secs(15)
        ),
        "the pane never ended"
    );
    let list = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    let listed = list["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["terminal_id"] == dead.as_str())
        .unwrap_or_else(|| panic!("the dead pane {dead} is not in the listing: {list}"));
    assert!(listed.get("pane_id").is_some(), "{listed}");
    let info = f.ok(
        &["pane", "process-info", "--pane", &dead],
        "cli:pane:process-info",
        "pane_process_info",
    );
    assert_eq!(info["dead"], true, "{info}");
    assert_eq!(info["pid"], Value::Null, "{info}");
    assert_eq!(info["process_info"]["shell_pid"], Value::Null, "{info}");
    assert_eq!(
        info["process_info"]["foreground_processes"],
        serde_json::json!([]),
        "{info}"
    );
}

/// Holds a real attached client on the fixture's server, so focus changes are observable:
/// `switch-client` is a no-op with nothing attached, which is exactly why the earlier
/// `--no-focus` assertions proved nothing.
struct AttachedClient(std::process::Child);

impl AttachedClient {
    /// Attaches in control mode (`-C`), which needs no tty: the client stays attached for
    /// as long as its stdin is open.
    fn attach(f: &Fixture) -> AttachedClient {
        let child = Command::new("tmux")
            .arg("-L")
            .arg(&f.socket)
            .args(["-C", "attach-session", "-t", "one"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn a control-mode tmux client");
        // Owned before the polling below, so `Drop` reaps the child even if that panics.
        let client = AttachedClient(child);

        // Attaching is asynchronous; wait for the server to actually report the client.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !f
                .tmux(&["list-clients", "-F", "#{client_name}"])
                .unwrap_or_default()
                .is_empty()
            {
                return client;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no tmux client attached within 5s");
    }

    /// The session name the attached client is currently looking at.
    fn session(&self, f: &Fixture) -> String {
        f.tmux(&["list-clients", "-F", "#{client_session}"])
            .unwrap()
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn no_focus_leaves_the_attached_client_where_it_was_and_focus_moves_it() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    assert_eq!(client.session(&f), "one", "the client starts on 'one'");

    // `--no-focus` must actually keep the client put. A weaker assertion would pass either
    // way: with `!no_focus && focus` written wrongly, `--no-focus` is a
    // no-op and creation never focuses, so the client stays on 'one' either way.
    f.ok(
        &["workspace", "create", "--label", "bg", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    assert_eq!(
        client.session(&f),
        "one",
        "--no-focus must not switch the attached client"
    );

    // The default is to focus, matching tmux's own `new-session`.
    f.ok(
        &["workspace", "create", "--label", "deflt"],
        "cli:workspace:create",
        "workspace_created",
    );
    assert_eq!(
        client.session(&f),
        "deflt",
        "creating a workspace focuses it by default"
    );

    // ...and `--focus` states that default explicitly.
    f.ok(
        &["workspace", "create", "--label", "fg", "--focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    assert_eq!(client.session(&f), "fg", "--focus must switch the client");

    // The same contract for tabs: `--no-focus` keeps the current window selected.
    let before = f
        .tmux(&["display-message", "-p", "#{window_name}"])
        .unwrap();
    f.ok(
        &["tab", "create", "--label", "bgtab", "--no-focus"],
        "cli:tab:create",
        "tab_created",
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "#{window_name}"])
            .unwrap(),
        before,
        "--no-focus must not select the new tab"
    );
    f.ok(
        &["tab", "create", "--label", "fgtab"],
        "cli:tab:create",
        "tab_created",
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "#{window_name}"])
            .unwrap(),
        "fgtab",
        "creating a tab selects it by default"
    );

    // And for pane splits, where focus means the active pane within the window. The
    // target is explicit: with `$TMUX_PANE` cleared there is no "current pane" to infer.
    let active_before = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    f.ok(
        &[
            "pane",
            "split",
            &active_before,
            "--direction",
            "down",
            "--no-focus",
        ],
        "cli:pane:split",
        "pane_split",
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap(),
        active_before,
        "--no-focus must leave the active pane alone"
    );
    let split = f.ok(
        &["pane", "split", &active_before, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap(),
        split["pane"]["terminal_id"].as_str().unwrap(),
        "a split focuses the new pane by default"
    );
}

#[test]
fn split_without_cwd_inherits_the_source_panes_directory() {
    // herdr's `pane split` defaults the new pane to "the current working directory" — the
    // *source pane's* cwd (reference/SKILL.md). tmux's own default for `split-window` with
    // no `-c` is the cwd of the client that ran it, i.e. taw's own process cwd, which is
    // not the pane's. So taw must resolve the source pane's `#{pane_current_path}` itself.
    let f = Fixture::start();
    let src = f.first_pane();
    let src_terminal = f.terminal_id(&src);
    let src_path = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &src_terminal,
            "#{pane_current_path}",
        ])
        .unwrap();
    assert!(!src_path.is_empty(), "source pane has no current path");

    let split = f.ok(
        &["pane", "split", &src, "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let new_terminal = split["pane"]["terminal_id"].as_str().unwrap();
    let new_path = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            new_terminal,
            "#{pane_current_path}",
        ])
        .unwrap();

    assert_eq!(
        new_path, src_path,
        "a split with no --cwd should start in the source pane's directory, not taw's"
    );
}

#[test]
fn move_to_a_new_tab_keeps_the_panes_directory() {
    // `pane move` relocates the *existing* pane (break-pane/join-pane), so it keeps its own
    // running process and cwd; there is no fresh pane to mis-default. This pins that parity
    // so a future change cannot silently reintroduce taw's cwd here.
    let f = Fixture::start();
    let src = f.first_pane();
    let src_terminal = f.terminal_id(&src);
    let src_path = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &src_terminal,
            "#{pane_current_path}",
        ])
        .unwrap();

    let moved = f.ok(
        &["pane", "move", &src, "--new-tab", "--no-focus"],
        "cli:pane:move",
        "pane_moved",
    );
    let new_terminal = moved["move_result"]["pane"]["terminal_id"]
        .as_str()
        .unwrap();
    let new_path = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            new_terminal,
            "#{pane_current_path}",
        ])
        .unwrap();

    assert_eq!(
        new_path, src_path,
        "moving a pane to a new tab must preserve its working directory"
    );
}

/// A destination workspace named `dest`, and a second pane in `one`'s only window so it
/// survives losing the pane about to be moved. Returns that pane's `(public id, raw id)`.
fn move_setup(f: &Fixture) -> (String, String) {
    f.ok(
        &["workspace", "create", "--label", "dest", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    let first = f.first_pane();
    let split = f.ok(
        &["pane", "split", &first, "--direction", "down", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    (
        split["pane"]["pane_id"].as_str().unwrap().to_string(),
        split["pane"]["terminal_id"].as_str().unwrap().to_string(),
    )
}

/// herdr v0.9.1 (#3760, #4153, #4171): `pane move --focus` takes an attached client to the
/// pane, as `agent focus` does. tmux's `join-pane` leaves a client in the session it was
/// on and a bare `select-pane` does not move it either, so a pane moved into another
/// workspace must not be selected where nobody is looking.
#[test]
fn pane_move_focus_takes_the_attached_client_to_the_pane() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    let (mover, mover_terminal) = move_setup(&f);
    assert_eq!(client.session(&f), "one", "the client starts on 'one'");

    let moved = f.ok(
        &["pane", "move", &mover, "--workspace", "dest", "--focus"],
        "cli:pane:move",
        "pane_moved",
    );

    assert_eq!(
        moved["move_result"]["pane"]["terminal_id"], mover_terminal,
        "{moved}"
    );
    assert_eq!(
        client.session(&f),
        "dest",
        "--focus must take the attached client to the pane it moved"
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "-t", "dest:", "#{pane_id}"])
            .unwrap(),
        mover_terminal,
        "the moved pane is the one selected in its new workspace"
    );
}

/// Moving into a new workspace is the same: the client goes with the pane.
#[test]
fn pane_move_focus_into_a_new_workspace_takes_the_attached_client_there() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    let (mover, _) = move_setup(&f);

    f.ok(
        &[
            "pane",
            "move",
            &mover,
            "--new-workspace",
            "--label",
            "fresh",
            "--focus",
        ],
        "cli:pane:move",
        "pane_moved",
    );

    assert_eq!(client.session(&f), "fresh");
}

/// Into a new workspace, `--no-focus` builds it and puts the pane there without taking the
/// client along.
#[test]
fn pane_move_no_focus_into_a_new_workspace_leaves_the_attached_client_where_it_was() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    let (mover, mover_terminal) = move_setup(&f);

    let moved = f.ok(
        &[
            "pane",
            "move",
            &mover,
            "--new-workspace",
            "--label",
            "fresh",
            "--no-focus",
        ],
        "cli:pane:move",
        "pane_moved",
    );

    assert_eq!(
        moved["move_result"]["pane"]["terminal_id"], mover_terminal,
        "{moved}"
    );
    assert_eq!(
        client.session(&f),
        "one",
        "--no-focus must not switch the client"
    );
    assert_eq!(
        f.tmux(&["display-message", "-p", "-t", "fresh:", "#{pane_id}"])
            .unwrap(),
        mover_terminal,
        "the pane is in the new workspace, and is all there is there"
    );
}

/// `--no-focus` leaves everything where it was: the client stays on its session, and the
/// destination keeps the pane it had selected — tmux's `join-pane` would otherwise make the
/// pane it just moved the active one.
#[test]
fn pane_move_no_focus_leaves_the_client_and_the_destinations_selection_alone() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    let (mover, mover_terminal) = move_setup(&f);
    let selected_before = f
        .tmux(&["display-message", "-p", "-t", "dest:", "#{pane_id}"])
        .unwrap();

    f.ok(
        &["pane", "move", &mover, "--workspace", "dest", "--no-focus"],
        "cli:pane:move",
        "pane_moved",
    );

    assert_eq!(
        client.session(&f),
        "one",
        "--no-focus must not switch the attached client"
    );
    let selected_after = f
        .tmux(&["display-message", "-p", "-t", "dest:", "#{pane_id}"])
        .unwrap();
    assert_eq!(
        selected_after, selected_before,
        "--no-focus must not make the moved pane {mover_terminal} the selected one"
    );
}

/// The same inside one session: moving a pane into another tab with `--no-focus` must not
/// flip the tab the user is looking at.
#[test]
fn pane_move_no_focus_keeps_the_tab_the_client_is_looking_at() {
    let f = Fixture::start();
    let _client = AttachedClient::attach(&f);
    let first = f.first_pane();
    let split = f.ok(
        &["pane", "split", &first, "--direction", "down", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let mover = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let other = f.ok(
        &["tab", "create", "--label", "other", "--no-focus"],
        "cli:tab:create",
        "tab_created",
    );
    let other_tab = other["tab"]["tab_id"].as_str().unwrap().to_string();
    let looking_at = f
        .tmux(&["display-message", "-p", "-t", "one", "#{window_id}"])
        .unwrap();

    f.ok(
        &["pane", "move", &mover, "--tab", &other_tab, "--no-focus"],
        "cli:pane:move",
        "pane_moved",
    );

    assert_eq!(
        f.tmux(&["display-message", "-p", "-t", "one", "#{window_id}"])
            .unwrap(),
        looking_at,
        "--no-focus must leave the session on the tab it was on"
    );
}

#[test]
fn workspace_create_then_list_shows_both_workspaces() {
    let f = Fixture::start();
    let dir = tempfile::tempdir().unwrap();

    let created = f.ok(
        &[
            "workspace",
            "create",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--label",
            "demo",
            "--no-focus",
        ],
        "cli:workspace:create",
        "workspace_created",
    );
    assert_eq!(created["workspace"]["label"], "demo", "{created}");
    assert!(created["tab"].is_object(), "{created}");
    assert!(created["root_pane"].is_object(), "{created}");
    let ws_id = created["workspace"]["workspace_id"].as_str().unwrap();

    let listed = f.ok(
        &["workspace", "list"],
        "cli:workspace:list",
        "workspace_list",
    );
    let workspaces = listed["workspaces"].as_array().unwrap();
    assert_eq!(workspaces.len(), 2, "{listed}");
    assert!(
        workspaces.iter().any(|w| w["label"] == "demo"),
        "created workspace missing from list: {listed}"
    );

    let got = f.ok(
        &["workspace", "get", ws_id],
        "cli:workspace:get",
        "workspace",
    );
    assert_eq!(got["workspace"]["label"], "demo", "{got}");
    assert_eq!(got["workspace"]["tab_count"], 1, "{got}");

    f.ok(
        &["workspace", "rename", ws_id, "renamed"],
        "cli:workspace:rename",
        "workspace",
    );
    f.ok(&["workspace", "close", ws_id], "cli:workspace:close", "ok");
    let after = f.ok(
        &["workspace", "list"],
        "cli:workspace:list",
        "workspace_list",
    );
    assert_eq!(after["workspaces"].as_array().unwrap().len(), 1, "{after}");

    // `workspace template` is accepted for herdr parity and reports `unsupported`.
    let (code, env) = f.run(&["workspace", "template"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "unsupported", "{env}");
}

#[test]
fn tab_create_rename_and_close() {
    let f = Fixture::start();
    let ws = f.ok(
        &["workspace", "list"],
        "cli:workspace:list",
        "workspace_list",
    )["workspaces"][0]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let created = f.ok(
        &["tab", "create", "--workspace", &ws, "--label", "logs"],
        "cli:tab:create",
        "tab_created",
    );
    assert_eq!(created["tab"]["label"], "logs", "{created}");
    assert!(created["root_pane"].is_object(), "{created}");
    let tab_id = created["tab"]["tab_id"].as_str().unwrap().to_string();

    let listed = f.ok(
        &["tab", "list", "--workspace", &ws],
        "cli:tab:list",
        "tab_list",
    );
    assert_eq!(listed["tabs"].as_array().unwrap().len(), 2, "{listed}");

    let renamed = f.ok(
        &["tab", "rename", &tab_id, "build"],
        "cli:tab:rename",
        "tab",
    );
    assert_eq!(renamed["tab"]["label"], "build", "{renamed}");
    assert_eq!(
        f.ok(&["tab", "get", &tab_id], "cli:tab:get", "tab")["tab"]["label"],
        "build"
    );

    f.ok(&["tab", "close", &tab_id], "cli:tab:close", "ok");
    let after = f.ok(
        &["tab", "list", "--workspace", &ws],
        "cli:tab:list",
        "tab_list",
    );
    assert_eq!(after["tabs"].as_array().unwrap().len(), 1, "{after}");
}

#[test]
fn pane_split_with_env_then_run_wait_output_and_read() {
    let f = Fixture::start();
    let pane = f.first_pane();

    let split = f.ok(
        &[
            "pane",
            "split",
            &pane,
            "--direction",
            "right",
            "--env",
            "TAW_TEST_VAR=taw_env_ok",
            "--no-focus",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let new_pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    assert_ne!(new_pane, pane, "split must return the *new* pane: {split}");
    let listed = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    assert_eq!(listed["panes"].as_array().unwrap().len(), 2, "{listed}");

    // The `-e` environment reached the new pane's shell.
    f.ok(
        &["pane", "run", &new_pane, "echo $TAW_TEST_VAR"],
        "cli:pane:run",
        "ok",
    );
    let waited = f.ok(
        &[
            "pane",
            "wait-output",
            &new_pane,
            "--pattern",
            "taw_env_ok",
            "--timeout",
            "10000",
        ],
        "cli:pane:wait-output",
        "pane_output_match",
    );
    assert!(waited["matched"].as_bool().unwrap(), "{waited}");

    let read = f.ok(&["pane", "read", &new_pane], "cli:pane:read", "pane_output");
    assert!(
        read["text"].as_str().unwrap().contains("taw_env_ok"),
        "pane read should show the echoed value: {read}"
    );

    // A pattern that never appears times out with the documented code.
    let (code, env) = f.run(&[
        "pane",
        "wait-output",
        &new_pane,
        "--pattern",
        "never_ever_taw",
        "--timeout",
        "600",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "timeout", "{env}");

    f.ok(&["pane", "close", &new_pane], "cli:pane:close", "ok");
    let after = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    assert_eq!(after["panes"].as_array().unwrap().len(), 1, "{after}");
}

/// `pane wait-output --match` must match real output, never a command typed at the prompt
/// but not yet run — that input carries the pattern too, and matching the whole screen
/// made the wait return the instant the caller had *typed* it.
#[test]
fn wait_output_ignores_a_typed_but_unrun_command_then_matches_real_output() {
    let f = Fixture::start();
    let pane = f.first_pane();

    // Type a command containing the needle, but do NOT run it: it sits on the prompt line.
    f.ok(
        &["pane", "send-text", &pane, "echo WAITNEEDLE"],
        "cli:pane:send-text",
        "ok",
    );
    // Wait until it has actually rendered, so the check below is genuinely "typed input is
    // present, and must still not match".
    let mut rendered = false;
    for _ in 0..200 {
        let read = f.ok(&["pane", "read", &pane], "cli:pane:read", "pane_output");
        if read["text"]
            .as_str()
            .unwrap_or_default()
            .contains("WAITNEEDLE")
        {
            rendered = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(rendered, "the typed command never rendered on the pane");

    // The needle is on the input line only — wait-output must not match it, so it times out.
    let (code, env) = f.run(&[
        "pane",
        "wait-output",
        &pane,
        "--match",
        "WAITNEEDLE",
        "--timeout",
        "800",
    ]);
    assert_eq!(code, 1, "typed-but-unrun input must not match: {env}");
    assert_eq!(env["error"]["code"], "timeout", "{env}");

    // Clear the typed line, then run the command for real: the needle now appears in output
    // above the fresh prompt, and the wait settles.
    f.ok(
        &["pane", "send-keys", &pane, "ctrl+c"],
        "cli:pane:send-keys",
        "ok",
    );
    f.ok(
        &["pane", "run", &pane, "echo WAITNEEDLE"],
        "cli:pane:run",
        "ok",
    );
    let waited = f.ok(
        &[
            "pane",
            "wait-output",
            &pane,
            "--match",
            "WAITNEEDLE",
            "--timeout",
            "10000",
        ],
        "cli:pane:wait-output",
        "pane_output_match",
    );
    assert!(
        waited["matched"].as_bool().unwrap(),
        "real output must match: {waited}"
    );
}

/// Text that ends in `;` reaches the shell whole. It is the last word of tmux's `send-keys`, and tmux takes
/// a `;` at the end of a word for the end of the command: `pane run` typed `find . -exec rm {} \;` as
/// `find . -exec rm {} ;`, and `pane send-text` typed `echo two;` as `echo two`.
#[test]
fn text_that_ends_in_a_semicolon_is_typed_whole() {
    let f = Fixture::start();
    let pane = f.first_pane();

    // What prints `x;|` is the command typed with its backslash; typed without it, it prints `x|`.
    f.ok(
        &["pane", "run", &pane, r"printf '%s|\n' x\;"],
        "cli:pane:run",
        "ok",
    );
    let (code, env) = f.run(&[
        "pane",
        "wait-output",
        &pane,
        "--match",
        "x;|",
        "--timeout",
        "10000",
    ]);
    assert_eq!(
        code,
        0,
        "the command did not reach the shell as it was given: {env}\n{}",
        f.ok(&["pane", "read", &pane], "cli:pane:read", "pane_output")["text"]
    );

    f.ok(
        &["pane", "send-text", &pane, "echo two;"],
        "cli:pane:send-text",
        "ok",
    );
    let mut last = String::new();
    let typed = (0..400).any(|_| {
        last = f.ok(&["pane", "read", &pane], "cli:pane:read", "pane_output")["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let whole = last.trim_end().ends_with("$ echo two;");
        if !whole {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        whole
    });
    assert!(typed, "the text was not typed whole:\n{last}");
}

/// The key `;` is sent. Each key is a word of tmux's `send-keys`, and tmux took the `;` for the end of
/// the command: `pane send-keys <pane> ';'` sent nothing and said ok, and a key after it was taken for a
/// command of its own ("unknown command").
#[test]
fn the_semicolon_key_is_sent() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.ok(
        &["pane", "send-text", &pane, "echo a"],
        "cli:pane:send-text",
        "ok",
    );

    let alone = f.ok(
        &["pane", "send-keys", &pane, ";"],
        "cli:pane:send-keys",
        "ok",
    );
    assert_eq!(alone["keys"], serde_json::json!([";"]), "{alone}");
    let then_more = f.ok(
        &["pane", "send-keys", &pane, ";", "b"],
        "cli:pane:send-keys",
        "ok",
    );
    assert_eq!(
        then_more["keys"],
        serde_json::json!([";", "b"]),
        "{then_more}"
    );

    let mut last = String::new();
    let typed = (0..400).any(|_| {
        last = f.ok(&["pane", "read", &pane], "cli:pane:read", "pane_output")["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let both = last.trim_end().ends_with("$ echo a;;b");
        if !both {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        both
    });
    assert!(typed, "the keys were not all sent:\n{last}");
}

/// A free-text option keeps a value that ends in `;`: the value is the last word of tmux's
/// `set-option`, and tmux took the `;` for the end of the command, so `pane rename <pane> 'deploy;'`
/// stored `deploy`, and a token reported as `step=one;` stored `one`.
#[test]
fn a_label_or_a_token_that_ends_in_a_semicolon_is_stored_whole() {
    let f = Fixture::start();
    let pane = f.first_pane();

    f.ok(
        &["pane", "rename", &pane, "deploy;"],
        "cli:pane:rename",
        "pane",
    );
    f.ok(
        &[
            "pane",
            "report-metadata",
            &pane,
            "--source",
            "test",
            "--token",
            "step=one;",
        ],
        "cli:pane:report-metadata",
        "ok",
    );

    assert_eq!(f.pane_option(&pane, "@agent_title"), "deploy;");
    assert_eq!(f.pane_option(&pane, "@agent_token_step"), "one;");
}

/// `agent explain` must not contradict the state the watcher writes. On a pane whose
/// identity is the kind hint alone — its foreground process names no agent and no manifest
/// rule matches the screen — the watcher records `unknown`; explain has to say the same,
/// not fall through to the engine's default `idle`.
#[test]
fn agent_explain_reports_unknown_for_a_hint_only_pane_with_no_matching_screen() {
    let f = Fixture::start();
    let pane = f.first_pane();
    // A hint-only identity, exactly what `agent start` (or a restore) leaves behind, on a
    // plain shell whose screen matches no claude rule.
    let raw = f
        .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    f.tmux(&["set-option", "-t", &raw, "@agent_kind_hint", "claude"])
        .unwrap();

    let (code, env) = f.run(&["agent", "explain", &pane]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["state"], "unknown",
        "a hint-only pane with no matching screen must explain as unknown: {env}"
    );
    assert_eq!(env["result"]["matched_rule"], Value::Null, "{env}");
}

/// A hint is the name `agent start --kind` was given, which can be another name of the agent's
/// kind (`kiro-cli` is Kiro): before the watcher records the kind, `agent explain` reads the pane
/// as the agent that name is, and not as a manifest called `kiro-cli`, which there is none of.
#[test]
fn agent_explain_names_the_kind_of_a_hint_that_is_another_name_of_it() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let raw = f
        .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    f.tmux(&[
        "set-option",
        "-p",
        "-t",
        &raw,
        "@agent_kind_hint",
        "kiro-cli",
    ])
    .unwrap();

    let (code, env) = f.run(&["agent", "explain", &pane]);
    assert_eq!(
        code, 0,
        "a pane hinted kiro-cli with no @agent_kind must explain as Kiro: {env}"
    );
    assert_eq!(env["result"]["agent"], "kiro", "{env}");
    assert_eq!(env["result"]["state"], "unknown", "{env}");
}

/// The same rule for an agent that runs in a container. The pane's foreground process is the
/// runtime, which names no agent, but the watcher found one in its arguments and marked the
/// pane (`@agent_container`), so the identity is not the hint's alone: a screen no rule
/// matches is the engine's default `idle`, as the watcher writes it, and not `unknown`.
#[test]
fn agent_explain_reads_a_container_pane_the_way_the_watcher_does() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let raw = f
        .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_kind_hint", "claude"),
        ("@agent_container", "docker"),
    ] {
        f.tmux(&["set-option", "-p", "-t", &raw, option, value])
            .unwrap();
    }

    let (code, env) = f.run(&["agent", "explain", &pane]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["state"], "idle",
        "an agent the runtime's arguments name is not a hint-only pane: {env}"
    );
    assert_eq!(env["result"]["matched_rule"], Value::Null, "{env}");

    // Take the marker away and the pane is the hint-only pane it was.
    f.tmux(&["set-option", "-u", "-p", "-t", &raw, "@agent_container"])
        .unwrap();
    let (code, env) = f.run(&["agent", "explain", &pane]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(env["result"]["state"], "unknown", "{env}");
}

/// `taw agent list` and `taw agent get` say where an agent runs: the runtime the watcher saw, and
/// the profile taw started it through, `null` outside a container.
#[test]
fn agent_list_and_get_say_where_the_agent_runs() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let raw = f
        .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    f.tmux(&["set-option", "-p", "-t", &raw, "@agent_kind", "claude"])
        .unwrap();

    // The field is there and `null`, not missing, which would read as `null` too.
    let got = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(got["agent"].get("sandbox"), Some(&Value::Null), "{got}");

    for (option, value) in [
        ("@agent_container", "docker"),
        ("@agent_sandbox", "default"),
    ] {
        f.tmux(&["set-option", "-p", "-t", &raw, option, value])
            .unwrap();
    }
    let listed = f.ok(&["agent", "list"], "cli:agent:list", "agent_list");
    assert_eq!(
        listed["agents"][0]["sandbox"],
        serde_json::json!({"profile": "default", "runtime": "docker"}),
        "{listed}"
    );
    let got = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(
        got["agent"]["sandbox"],
        serde_json::json!({"profile": "default", "runtime": "docker"}),
        "{got}"
    );
}

#[test]
fn report_agent_then_agent_list_rename_get_and_wait() {
    let f = Fixture::start();
    let pane = f.first_pane();

    let empty = f.ok(&["agent", "list"], "cli:agent:list", "agent_list");
    assert!(
        empty["agents"].as_array().unwrap().is_empty(),
        "no agents before report-agent: {empty}"
    );

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "blocked",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    let listed = f.ok(&["agent", "list"], "cli:agent:list", "agent_list");
    let agents = listed["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{listed}");
    assert_eq!(agents[0]["agent_status"], "blocked", "{listed}");
    assert_eq!(agents[0]["agent"], "claude", "{listed}");
    assert_eq!(agents[0]["pane_id"], pane.as_str(), "{listed}");

    // `hook:<source>` is what report-agent records as the authority.
    assert_eq!(f.pane_option(&pane, "@agent_source"), "hook:test");

    f.ok(
        &["agent", "rename", &pane, "reviewer"],
        "cli:agent:rename",
        "agent",
    );
    let got = f.ok(&["agent", "get", "reviewer"], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["name"], "reviewer", "{got}");
    assert_eq!(got["agent"]["agent_status"], "blocked", "{got}");
    assert!(
        got["agent"].get("state_change_seq").is_some(),
        "agent record needs state_change_seq: {got}"
    );

    // Already blocked, so the wait returns immediately.
    let waited = f.ok(
        &[
            "agent",
            "wait",
            "reviewer",
            "--state",
            "blocked",
            "--timeout",
            "1000",
        ],
        "cli:agent:wait",
        "agent",
    );
    assert_eq!(waited["agent"]["agent_status"], "blocked", "{waited}");

    // A state it will never reach times out.
    let (code, env) = f.run(&[
        "agent",
        "wait",
        "reviewer",
        "--state",
        "done",
        "--timeout",
        "600",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "timeout", "{env}");

    // An invalid agent name is rejected before anything is written.
    let (code, env) = f.run(&["agent", "rename", &pane, "Not Valid"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");

    f.ok(
        &["agent", "rename", &pane, "--clear"],
        "cli:agent:rename",
        "agent",
    );
    assert_eq!(f.pane_option(&pane, "@agent_name"), "");
}

/// A pane kept after its program ended (`remain-on-exit`) runs nothing, so its agent record, which
/// stays, holds no name: another pane takes the name, and the name then finds that pane. (Whether
/// its shell is idle cannot be told for such a pane, so without this the name stayed taken for as
/// long as the pane was kept.)
#[test]
fn a_pane_kept_after_its_program_ended_does_not_hold_its_agents_name() {
    let f = Fixture::start();
    let other = f.first_pane();
    f.tmux(&["set-option", "-g", "remain-on-exit", "on"])
        .unwrap();
    // `-b`: the dead pane comes before the other one in tmux's listing, so a name looked up in
    // listing order meets the dead pane first.
    let dead = f
        .tmux(&[
            "split-window",
            "-b",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "one",
            "exit 0",
        ])
        .unwrap();
    assert!(
        common::wait_for(
            || f.tmux(&["display-message", "-p", "-t", &dead, "#{pane_dead}"])
                .is_ok_and(|dead| dead == "1"),
            std::time::Duration::from_secs(15)
        ),
        "the pane never ended"
    );
    // The record of the agent that ran there, still standing, with its name.
    f.tmux(&["set-option", "-p", "-t", &dead, "@agent_kind", "claude"])
        .unwrap();
    f.tmux(&["set-option", "-p", "-t", &dead, "@agent_name", "rev"])
        .unwrap();
    f.ok(
        &[
            "pane",
            "report-agent",
            &other,
            "--source",
            "test",
            "--agent",
            "codex",
            "--state",
            "idle",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    let got = f.ok(
        &["agent", "rename", &other, "rev"],
        "cli:agent:rename",
        "agent",
    );
    assert_eq!(got["agent"]["name"], "rev", "{got}");
    assert_eq!(f.pane_option(&other, "@agent_name"), "rev");
    let listed = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    assert_eq!(
        listed["panes"][0]["terminal_id"], dead,
        "the dead pane is listed first: {listed}"
    );
    let got = f.ok(&["agent", "get", "rev"], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["pane_id"], other.as_str(), "{got}");
}

/// A `tmux` in front of the real one, for a test that wants to see what a verb asks it: it
/// appends each call's arguments to a log, one line a call (tabs and newlines folded to spaces),
/// runs the real tmux, and after a call that sets `@agent_kind` waits `pause` seconds, so that
/// what a reader sees in that time is what that call left behind. Returns the shim and the log.
fn tmux_shim(dir: &std::path::Path, pause: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join("tmux-calls.txt");
    let shim = dir.join("tmux-shim");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n{{ printf '%s' \"$*\" | tr '\\n\\r\\t' '   '; echo; }} >> {}\ntmux \"$@\"\nstatus=$?\ncase \"$*\" in *set-option*@agent_kind*) sleep {pause};; esac\nexit $status\n",
            log.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);
    (shim, log)
}

/// `taw pane report-agent` records the agent, its state and its owner (`@agent_source`) in one
/// tmux call. Each in a call of its own, a reader in between (the watcher, the popup) sees an
/// agent that nobody owns, and a watcher that sees one on a pane with a shell in front takes it
/// for an agent that has gone.
#[test]
fn report_agent_writes_the_agent_its_state_and_its_owner_in_one_tmux_call() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let (shim, log) = tmux_shim(f.work.path(), "0");

    let (code, stdout, stderr) = f.run_with_env(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ],
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{stdout}{stderr}");
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let writes: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("set-option"))
        .collect();
    assert_eq!(
        writes.len(),
        1,
        "the report was more than one tmux call: {writes:?}"
    );
    for option in [
        "@agent_kind opencode",
        "@agent_state working",
        "@agent_state_since",
        "@agent_rule",
        "@agent_source hook:integration:opencode",
    ] {
        assert!(
            writes[0].contains(option),
            "{option:?} is not in the one call: {}",
            writes[0]
        );
    }
}

/// The same, seen from the reader's side and without depending on luck: the write is held open
/// by a `tmux` that waits after a call that sets the agent, and a reader polling all the while
/// sees what each call left behind. Never the agent without its owner.
#[test]
fn a_reader_never_sees_the_agent_without_its_owner_while_report_agent_writes() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    let (shim, _log) = tmux_shim(f.work.path(), "0.6");
    let mut child = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args([
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ])
        .env("TAW_TMUX_SOCKET_NAME", &f.socket)
        .env("TAW_CONFIG_DIR", f.config.path())
        .env("TAW_STATE_DIR", f.state.path())
        .env("TAW_TMUX_BIN", &shim)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let mut seen = std::collections::BTreeSet::new();
    while child.try_wait().unwrap().is_none() {
        seen.insert(
            f.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{@agent_kind}|#{@agent_source}",
            ])
            .unwrap(),
        );
    }
    assert!(child.wait().unwrap().success());

    assert!(
        !seen.contains("opencode|"),
        "a reader saw the agent with no owner: {seen:?}"
    );
    assert!(
        seen.contains("opencode|hook:integration:opencode"),
        "the reader never saw the finished report: {seen:?}"
    );
}

/// A program that stands in for a watcher: a shell that counts each SIGURG it gets by appending a
/// line to a file. It is killed and reaped when this is dropped, which a test that fails drops as it
/// unwinds: a `Child` is not killed when it is dropped, and the loop of one a failed test left ran
/// for days, taken in by launchd. It also ends by itself once its directory is gone, or the test
/// process that started it is, should this never be dropped (a test process killed outright).
struct NudgeCounter {
    child: std::process::Child,
    /// The file it appends a line to for each SIGURG.
    log: PathBuf,
}

impl NudgeCounter {
    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for NudgeCounter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a [`NudgeCounter`] whose files are in `dir`, and waits for its trap to be set.
fn nudge_counter(dir: &std::path::Path) -> NudgeCounter {
    let log = dir.join("nudges.txt");
    let ready = dir.join("nudge-counter-ready");
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "trap 'echo nudged >> {}' URG; echo ready > {}; \
             while [ -d {} ] && kill -0 {} 2>/dev/null; do sleep 0.05; done",
            log.display(),
            ready.display(),
            dir.display(),
            std::process::id()
        ))
        .spawn()
        .unwrap();
    let counter = NudgeCounter { child, log };
    for _ in 0..200 {
        if ready.exists() {
            return counter;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the stand-in for a watcher never set its trap");
}

/// Whether the process `pid` has ended: it is not in the process table, or is only a zombie there.
fn has_ended(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.contains('Z')
}

/// Waits up to five seconds for `pid` to end.
fn ends_soon(pid: u32) -> bool {
    for _ in 0..200 {
        if has_ended(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    false
}

/// A test that fails after it started the stand-in for a watcher leaves nothing running: the stand-in
/// is killed and reaped as the failing test unwinds.
#[test]
fn a_stand_in_for_a_watcher_ends_with_the_test_that_fails_after_starting_it() {
    let work = tempfile::tempdir().unwrap();
    let pid = std::sync::Mutex::new(None);
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let counter = nudge_counter(work.path());
        *pid.lock().unwrap() = Some(counter.id());
        panic!("a test that fails after starting the stand-in");
    }));
    assert!(failed.is_err());
    let pid = pid
        .into_inner()
        .unwrap()
        .expect("the stand-in never started");
    let ended = has_ended(pid);
    if !ended {
        // Never left behind, whatever this test says.
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        ended,
        "the stand-in for a watcher outlived the test that failed"
    );
}

/// And should it never be dropped, it ends by itself once its directory is gone.
#[test]
fn a_stand_in_for_a_watcher_ends_by_itself_once_its_directory_is_gone() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("counter");
    std::fs::create_dir(&dir).unwrap();
    let counter = std::mem::ManuallyDrop::new(nudge_counter(&dir));
    let pid = counter.id();
    std::fs::remove_dir_all(&dir).unwrap();
    let ended = ends_soon(pid);
    if !ended {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        ended,
        "the stand-in for a watcher went on with its directory gone"
    );
}

fn nudges(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .count()
}

fn wait_for_nudges(log: &std::path::Path, want: usize) {
    for _ in 0..200 {
        if nudges(log) >= want {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{} nudges, not {want}", nudges(log));
}

/// A report tells the watcher of its pane that there is something to read, since the pane may print
/// nothing: one SIGURG to the pid in `@agent_watch_pid`, for each report of a state. SIGURG is
/// ignored by a process that has no handler for it, so a pid that has gone to another process is
/// not hurt by it.
#[test]
fn report_agent_nudges_the_watcher_the_pane_records_once_for_each_report() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let watcher = nudge_counter(f.work.path());
    let log = watcher.log.clone();
    f.set_pane_option(&pane, "@agent_watch_pid", &watcher.id().to_string());
    let report = |state: &str| {
        f.ok(
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "integration:opencode",
                "--agent",
                "opencode",
                "--state",
                state,
            ],
            "cli:pane:report-agent",
            "ok",
        );
    };

    report("working");
    wait_for_nudges(&log, 1);
    report("idle");
    wait_for_nudges(&log, 2);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(nudges(&log), 2, "one nudge for each report");

    // A report of a session reports no state, and leaves the watcher to its next read.
    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--agent-session-id",
            "ses_1",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(nudges(&log), 2, "a session report is no nudge");
}

/// What `@agent_watch_pid` holds is whatever was written to it: a report succeeds with no watcher
/// recorded and with a value that is no number, and nothing is nudged. The numbers that must never
/// be signalled (`0`, `1`, and what fits no pid) are pinned where the pid is chosen
/// (`watch::nudge::target`), without sending them: a test that sent them would signal a process
/// group or init if the check broke.
#[test]
fn a_report_nudges_nobody_when_the_pane_records_no_watcher_or_no_pid() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let bystander = nudge_counter(f.work.path());
    let log = bystander.log.clone();
    for value in ["", "abc", "not a pid", "99999999999999999999"] {
        f.set_pane_option(&pane, "@agent_watch_pid", value);
        f.ok(
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "test",
                "--agent",
                "claude",
                "--state",
                "working",
            ],
            "cli:pane:report-agent",
            "ok",
        );
        assert_eq!(f.pane_option(&pane, "@agent_state"), "working", "{value:?}");
    }
    // No option at all: the pane has no watcher.
    f.tmux(&[
        "set-option",
        "-p",
        "-u",
        "-t",
        &f.terminal_id(&pane),
        "@agent_watch_pid",
    ])
    .unwrap();
    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        nudges(&log),
        0,
        "a report nudged a process that is no watcher"
    );
    drop(bystander);
}

/// A report of `working` or `blocked` stamps the pane (`@agent_turn`) in the same tmux call as the
/// rest of the report, and no other report does. The watcher reads it to tell that a turn began and
/// ended between two of its reads: `working` and `idle` back to back leave the state it had taken in,
/// and only the stamp has moved.
#[test]
fn report_agent_stamps_the_turn_when_it_reports_working_or_blocked_and_only_then() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let stamp = || f.pane_option(&pane, "@agent_turn");
    let report = |state: &str| {
        f.ok(
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "integration:opencode",
                "--agent",
                "opencode",
                "--state",
                state,
            ],
            "cli:pane:report-agent",
            "ok",
        );
    };
    assert_eq!(stamp(), "");

    report("idle");
    assert_eq!(stamp(), "", "an idle starts no turn");

    report("working");
    let first = stamp();
    assert!(
        !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()),
        "{first:?}"
    );
    report("working");
    let second = stamp();
    assert_ne!(second, first, "each report of work moves it");
    report("blocked");
    let third = stamp();
    assert_ne!(third, second, "and each report of a wait");

    report("idle");
    report("unknown");
    assert_eq!(stamp(), third, "only work and waiting move it");

    // A report of a session reports no state.
    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--agent-session-id",
            "ses_1",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(stamp(), third);

    // The stamp is written with the rest of the report, in the one tmux call.
    let (shim, log) = tmux_shim(f.work.path(), "0");
    let (code, stdout, stderr) = f.run_with_env(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ],
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let calls = std::fs::read_to_string(&log).unwrap();
    let writes: Vec<&str> = calls
        .lines()
        .filter(|call| call.contains("set-option"))
        .collect();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].contains("@agent_turn"), "{}", writes[0]);
    assert_ne!(stamp(), third);
}

/// What a Letta-shaped integration reports about itself: the kind, the session and where it is
/// kept, and what started the session.
const SESSION_REPORT: [&str; 13] = [
    "pane",
    "report-agent-session",
    "%PANE",
    "--source",
    "integration:letta",
    "--agent",
    "letta",
    "--agent-session-id",
    "conv-1",
    "--agent-session-path",
    "/work/conv-1.json",
    "--session-start-source",
    "resume",
];

/// [`SESSION_REPORT`] for `pane`.
fn session_report(pane: &str) -> Vec<&str> {
    SESSION_REPORT
        .iter()
        .map(|word| if *word == "%PANE" { pane } else { *word })
        .collect()
}

/// A report of a session writes the agent's kind, its word that it said so, and the session it
/// reported in one tmux call. In calls of their own, a reader between them (the watcher, above
/// all) sees the agent with no word that it was the agent talking, and a watcher that finds a
/// shell in front of such an agent takes it for one that has gone.
#[test]
fn report_agent_session_writes_the_kind_and_the_session_in_one_tmux_call() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let (shim, log) = tmux_shim(f.work.path(), "0");

    let (code, stdout, stderr) = f.run_with_env(
        &session_report(&pane),
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{stdout}{stderr}");
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let writes: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("set-option"))
        .collect();
    assert_eq!(
        writes.len(),
        1,
        "the report was more than one tmux call: {writes:?}"
    );
    for option in [
        "@agent_kind letta",
        "@agent_reported_kind letta",
        "@agent_session_id conv-1",
        "@agent_session_path /work/conv-1.json",
        "@agent_token_session_start_source resume",
    ] {
        assert!(
            writes[0].contains(option),
            "{option:?} is not in the one call: {}",
            writes[0]
        );
    }
}

/// The same, from the reader's side and without depending on luck, as for `report-agent`: the
/// write is held open by a `tmux` that waits after a call that sets the agent, and a reader polling
/// all the while sees what each call left behind. Never the kind without what came with it.
#[test]
fn a_reader_never_sees_the_kind_of_a_session_report_without_its_session() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    let (shim, _log) = tmux_shim(f.work.path(), "0.6");
    let mut child = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args(session_report(&pane))
        .env("TAW_TMUX_SOCKET_NAME", &f.socket)
        .env("TAW_CONFIG_DIR", f.config.path())
        .env("TAW_STATE_DIR", f.state.path())
        .env("TAW_TMUX_BIN", &shim)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let mut seen = std::collections::BTreeSet::new();
    while child.try_wait().unwrap().is_none() {
        seen.insert(
            f.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{@agent_kind}|#{@agent_reported_kind}|#{@agent_session_id}|#{@agent_session_path}",
            ])
            .unwrap(),
        );
    }
    assert!(child.wait().unwrap().success());

    let whole = "letta|letta|conv-1|/work/conv-1.json";
    let halves: Vec<&String> = seen
        .iter()
        .filter(|row| row.starts_with("letta|") && row.as_str() != whole)
        .collect();
    assert!(
        halves.is_empty(),
        "a reader saw the kind without all of what came with it: {halves:?}"
    );
    assert!(
        seen.contains(whole),
        "the reader never saw the finished report: {seen:?}"
    );
}

/// What a session report leaves on a pane for `pane`: kind, reported kind, session id, session
/// path, in that order, read straight from tmux.
fn reported_identity(f: &Fixture, pane: &str) -> [String; 4] {
    [
        "@agent_kind",
        "@agent_reported_kind",
        "@agent_session_id",
        "@agent_session_path",
    ]
    .map(|name| f.pane_option(pane, name))
}

fn identity(kind: &str, reported: &str, id: &str, path: &str) -> [String; 4] {
    [kind, reported, id, path].map(str::to_string)
}

/// A pane a Claude took, with the session its own hook reported.
fn claude_with_its_session(f: &Fixture, pane: &str) {
    f.ok(
        &[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            "sess-old",
            "--agent-session-path",
            "/old/transcript.jsonl",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(
        reported_identity(f, pane),
        identity("claude", "claude", "sess-old", "/old/transcript.jsonl")
    );
}

/// Another agent reports its state on a pane that records a Claude and its session, and says
/// nothing of a session. What the Claude reported of itself was about the Claude: a session
/// restored from its id or path would resume the Claude's conversation under OpenCode.
#[test]
fn a_state_only_report_from_another_agent_drops_what_the_last_agent_reported_of_itself() {
    let f = Fixture::start();
    let pane = f.first_pane();
    claude_with_its_session(&f, &pane);

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    assert_eq!(f.pane_option(&pane, "@agent_state"), "working");
    assert_eq!(
        reported_identity(&f, &pane),
        identity("opencode", "", "", ""),
        "the last agent's session stayed on the pane"
    );
}

/// The same agent reporting again is the same agent: what it reported of itself stays.
#[test]
fn a_report_from_the_agent_the_pane_records_keeps_its_session() {
    let f = Fixture::start();
    let pane = f.first_pane();
    claude_with_its_session(&f, &pane);

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    assert_eq!(
        reported_identity(&f, &pane),
        identity("claude", "claude", "sess-old", "/old/transcript.jsonl")
    );
}

/// A pane that records no agent has nothing to drop, and a report that names its first agent and
/// its session writes both.
#[test]
fn the_first_report_on_a_pane_writes_the_session_it_carries() {
    let f = Fixture::start();
    let pane = f.first_pane();

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
            "--agent-session-id",
            "ses_1",
            "--agent-session-path",
            "/work/ses_1.json",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    assert_eq!(
        reported_identity(&f, &pane),
        identity("opencode", "", "ses_1", "/work/ses_1.json")
    );
}

/// A report that carries its own session writes it, and what the last agent left beside it, the
/// path a new id does not come with, goes all the same.
#[test]
fn a_report_from_another_agent_keeps_the_session_it_carries_and_drops_the_rest() {
    let f = Fixture::start();
    let pane = f.first_pane();
    claude_with_its_session(&f, &pane);

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
            "--agent-session-id",
            "ses_new",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    assert_eq!(
        reported_identity(&f, &pane),
        identity("opencode", "", "ses_new", "")
    );
}

/// A report of a session from another agent takes the pane the same way: what the last agent
/// reported goes, and what this one reports is written.
#[test]
fn a_session_report_from_another_agent_drops_what_the_last_agent_reported_of_itself() {
    let f = Fixture::start();
    let pane = f.first_pane();
    claude_with_its_session(&f, &pane);

    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:letta",
            "--agent",
            "letta",
            "--agent-session-id",
            "conv-1",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );

    assert_eq!(
        reported_identity(&f, &pane),
        identity("letta", "letta", "conv-1", ""),
        "the Claude's transcript path stayed on the pane"
    );
}

/// What started the last agent's session (`--session-start-source`, recorded as a token) is about
/// that session, and goes with it when another agent takes the pane; the same agent reporting again
/// keeps it.
#[test]
fn what_started_the_last_agents_session_goes_with_it_when_another_agent_takes_the_pane() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            "sess-old",
            "--session-start-source",
            "resume",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(
        f.pane_option(&pane, "@agent_token_session_start_source"),
        "resume"
    );
    let report = |agent: &str| {
        f.ok(
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "test",
                "--agent",
                agent,
                "--state",
                "working",
            ],
            "cli:pane:report-agent",
            "ok",
        );
    };

    report("claude");
    assert_eq!(
        f.pane_option(&pane, "@agent_token_session_start_source"),
        "resume",
        "the same agent keeps what started its session"
    );

    report("opencode");
    assert_eq!(f.pane_option(&pane, "@agent_kind"), "opencode");
    assert_eq!(
        f.pane_option(&pane, "@agent_token_session_start_source"),
        "",
        "the last agent's session-start token stayed on the pane"
    );
}

/// A session restore writes the resumed id to a pane before the agent on it has reported anything,
/// so a pane can hold a session id and no agent. The first report that names an agent, a state and
/// nothing of a session, is not another agent taking the pane: there was none to take it from, and
/// the id stays for `session restore` to resume.
#[test]
fn a_session_on_a_pane_that_records_no_agent_stays_for_a_state_only_report() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.set_pane_option(&pane, "@agent_session_id", "resumed-1");
    f.set_pane_option(&pane, "@agent_session_path", "/resumed/transcript.jsonl");
    assert_eq!(f.pane_option(&pane, "@agent_kind"), "");

    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    assert_eq!(f.pane_option(&pane, "@agent_kind"), "opencode");
    assert_eq!(f.pane_option(&pane, "@agent_session_id"), "resumed-1");
    assert_eq!(
        f.pane_option(&pane, "@agent_session_path"),
        "/resumed/transcript.jsonl"
    );

    // And the same for a report of a session from the first agent that names itself.
    let other = f.ok(
        &["pane", "split", &pane, "--direction", "down", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let other = other["pane"]["pane_id"].as_str().unwrap().to_string();
    f.set_pane_option(&other, "@agent_session_path", "/resumed/other.jsonl");
    f.ok(
        &[
            "pane",
            "report-agent-session",
            &other,
            "--source",
            "test",
            "--agent",
            "letta",
            "--agent-session-id",
            "conv-2",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(f.pane_option(&other, "@agent_session_id"), "conv-2");
    assert_eq!(
        f.pane_option(&other, "@agent_session_path"),
        "/resumed/other.jsonl"
    );
}

/// What decides that the agent is another one is the label as it was typed: one with the
/// characters a tmux format gives a meaning to in it is the same agent when it is the same label,
/// and another when it is not.
#[test]
fn an_agent_label_with_format_characters_is_compared_as_typed() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let label = "a:b,c}d#{e}";
    let report = |agent: &str| {
        f.ok(
            &[
                "pane",
                "report-agent",
                &pane,
                "--source",
                "test",
                "--agent",
                agent,
                "--state",
                "idle",
            ],
            "cli:pane:report-agent",
            "ok",
        );
    };
    report(label);
    f.set_pane_option(&pane, "@agent_session_id", "kept");

    report(label);
    assert_eq!(f.pane_option(&pane, "@agent_session_id"), "kept");

    report("a:b,c}d#{f}");
    assert_eq!(f.pane_option(&pane, "@agent_kind"), "a:b,c}d#{f}");
    assert_eq!(f.pane_option(&pane, "@agent_session_id"), "");
}

/// The drop is one tmux call with the rest of the report, and it is made before the new kind
/// is written, so that it is the last agent it compares the label with.
#[test]
fn the_drop_is_in_the_one_tmux_call_that_writes_the_report() {
    let f = Fixture::start();
    let pane = f.first_pane();
    claude_with_its_session(&f, &pane);
    let (shim, log) = tmux_shim(f.work.path(), "0");

    let (code, stdout, stderr) = f.run_with_env(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
            "--agent-session-id",
            "ses_new",
        ],
        &[("TAW_TMUX_BIN", shim.to_str().unwrap())],
    );

    assert_eq!(code, 0, "{stdout}{stderr}");
    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let writes: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("set-option"))
        .collect();
    assert_eq!(
        writes.len(),
        1,
        "the report was more than one tmux call: {writes:?}"
    );
    let call = writes[0];
    for option in [
        "@agent_kind opencode",
        "@agent_state working",
        "@agent_source hook:integration:opencode",
        "@agent_session_id ses_new",
    ] {
        assert!(
            call.contains(option),
            "{option:?} is not in the call: {call}"
        );
    }
    assert!(
        call.find("@agent_reported_kind").unwrap() < call.find("@agent_kind opencode").unwrap(),
        "the drop has to come before the new kind is written: {call}"
    );
}

/// The same from the reader's side: between the report's calls, whatever they are, no reader
/// sees the new agent beside the session of the last one.
#[test]
fn a_reader_never_sees_another_agents_session_beside_the_new_kind() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    f.tmux(&["set-option", "-p", "-t", &pane, "@agent_kind", "claude"])
        .unwrap();
    f.tmux(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@agent_session_id",
        "sess-old",
    ])
    .unwrap();
    let (shim, _log) = tmux_shim(f.work.path(), "0.6");
    let mut child = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args([
            "pane",
            "report-agent",
            &pane,
            "--source",
            "integration:opencode",
            "--agent",
            "opencode",
            "--state",
            "working",
        ])
        .env("TAW_TMUX_SOCKET_NAME", &f.socket)
        .env("TAW_CONFIG_DIR", f.config.path())
        .env("TAW_STATE_DIR", f.state.path())
        .env("TAW_TMUX_BIN", &shim)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let mut seen = std::collections::BTreeSet::new();
    while child.try_wait().unwrap().is_none() {
        seen.insert(
            f.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{@agent_kind}|#{@agent_session_id}",
            ])
            .unwrap(),
        );
    }
    assert!(child.wait().unwrap().success());

    assert!(
        !seen.contains("opencode|sess-old"),
        "a reader saw the new agent with the last one's session: {seen:?}"
    );
    assert!(
        seen.contains("opencode|"),
        "the reader never saw the finished report: {seen:?}"
    );
}

/// clap accepts `pane_id` (positional) either before or after `report-agent`'s options —
/// there is nothing herdr-specific pinned here, but a caller building a command line one
/// argument at a time (or copying an example the other way round) should not have to care
/// which order this is in. An invalid pane makes the assertion about parsing, not a tmux
/// effect: reaching `not_found` proves every flag and the positional were read correctly,
/// since a parse failure would exit 2 with clap's own usage error instead.
#[test]
fn report_agent_parses_with_the_positional_pane_before_or_after_its_options() {
    let f = Fixture::start();

    let (code, env) = f.run(&[
        "pane",
        "report-agent",
        "--state=idle",
        "--source=x",
        "--agent=claude",
        "nope",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "not_found",
        "options-before-positional: {env}"
    );

    let (code, env) = f.run(&[
        "pane",
        "report-agent",
        "nope",
        "--state=idle",
        "--source=x",
        "--agent=claude",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "not_found",
        "positional-before-options: {env}"
    );
}

#[test]
fn release_agent_clears_the_hook_source() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    assert_eq!(f.pane_option(&pane, "@agent_source"), "hook:test");

    let released = f.ok(
        &[
            "pane",
            "release-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
        ],
        "cli:pane:release-agent",
        "ok",
    );
    assert_eq!(released["released"], true, "{released}");
    assert_eq!(
        f.pane_option(&pane, "@agent_source"),
        "",
        "@agent_source must be cleared"
    );
}

#[test]
fn report_agent_session_records_the_kind_the_agent_reported_and_claims_no_state() {
    // The watcher keeps a pane whose agent named itself through its own hook, whatever
    // process is in front of it, until the shell is back, so a session report has to say
    // that it was the agent talking: `@agent_reported_kind`. It says nothing about the
    // state, which stays the watcher's to read from the screen, so `@agent_source` (the
    // state's authority) is not written.
    let f = Fixture::start();
    let pane = f.first_pane();
    assert_eq!(f.pane_option(&pane, "@agent_reported_kind"), "");

    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "integration:letta",
            "--agent",
            "letta",
            "--agent-session-id",
            "conv-1",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(f.pane_option(&pane, "@agent_reported_kind"), "letta");
    assert_eq!(f.pane_option(&pane, "@agent_kind"), "letta");
    assert_eq!(f.pane_option(&pane, "@agent_session_id"), "conv-1");
    assert_eq!(f.pane_option(&pane, "@agent_source"), "");
    assert_eq!(f.pane_option(&pane, "@agent_state"), "");

    // A state report is an authority of its own (`hook:<source>`), and is not this.
    let split = f.ok(
        &["pane", "split", &pane, "--direction", "down", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let other = split["pane"]["pane_id"].as_str().unwrap().to_string();
    f.ok(
        &[
            "pane",
            "report-agent",
            &other,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "working",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    assert_eq!(f.pane_option(&other, "@agent_reported_kind"), "");
}

#[test]
fn report_agent_session_and_metadata_surface_on_the_agent_record() {
    let f = Fixture::start();
    let pane = f.first_pane();

    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            "sess-abc",
            "--agent-session-path",
            "/tmp/transcript.jsonl",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    let got = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["agent_session_id"], "sess-abc", "{got}");
    assert_eq!(
        got["agent"]["agent_session_path"], "/tmp/transcript.jsonl",
        "{got}"
    );

    f.ok(
        &[
            "pane",
            "report-metadata",
            &pane,
            "--source",
            "test",
            "--display-agent",
            "Reviewer Bot",
            "--title",
            "reviewing #482",
            "--token",
            "cost=1.25",
        ],
        "cli:pane:report-metadata",
        "ok",
    );
    let got = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["display_agent"], "Reviewer Bot", "{got}");
    assert_eq!(got["agent"]["title"], "reviewing #482", "{got}");
    assert_eq!(f.pane_option(&pane, "@agent_token_cost"), "1.25");

    f.ok(
        &[
            "pane",
            "report-metadata",
            &pane,
            "--source",
            "test",
            "--clear-display-agent",
            "--clear-token",
            "cost",
        ],
        "cli:pane:report-metadata",
        "ok",
    );
    let got = f.ok(&["agent", "get", &pane], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["display_agent"], Value::Null, "{got}");
}

/// herdr's `agent explain --file` reports a specific `agent_explain_file_read_failed`
/// code for an unreadable file, not the generic `not_found` — see
/// `<herdr-src>/src/cli/agent.rs`.
#[test]
fn agent_explain_with_an_unreadable_file_reports_its_own_error_code() {
    let f = Fixture::start();
    let (code, env) = f.run(&[
        "agent",
        "explain",
        "--file",
        "/nonexistent/definitely-not-here.txt",
        "--agent",
        "claude",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "agent_explain_file_read_failed",
        "{env}"
    );
}

#[test]
fn agent_explain_reports_every_evaluated_rule() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.set_pane_option(&pane, "@agent_kind", "claude");

    let result = f.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(result["agent"], "claude", "{result}");
    let rules = result["evaluated_rules"].as_array().unwrap();
    assert!(!rules.is_empty(), "evaluated_rules must not be empty");
    let r = &rules[0];
    for field in ["id", "region", "state", "matched", "priority", "evidence"] {
        assert!(
            r.get(field).is_some(),
            "evaluated rule missing {field}: {r}"
        );
    }
    assert!(result.get("matched_rule").is_some(), "{result}");
    assert!(result.get("state").is_some(), "{result}");
}

#[test]
fn unknown_pane_is_not_found_and_exits_one() {
    let f = Fixture::start();
    let (code, env) = f.run(&["pane", "get", "nope"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["id"], "cli:pane:get", "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(
        env["error"]["message"].as_str().unwrap().contains("nope"),
        "the message should name what was not found: {env}"
    );
    assert!(env.get("result").is_none(), "{env}");

    // A well-formed but absent public id too.
    let (code, env) = f.run(&["pane", "get", "w9:p99"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");

    // Unknown agent name.
    let (code, env) = f.run(&["agent", "get", "ghost"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn notification_show_then_open_target() {
    let f = Fixture::start();
    let shown = f.ok(
        &["notification", "show", "hello", "--body", "world"],
        "cli:notification:show",
        "ok",
    );
    assert!(shown.is_object(), "{shown}");

    // No pane holds an agent, so open-target has nowhere to go, and says so.
    let (code, env) = f.run(&["notification", "open-target"]);
    if code == 0 {
        assert_eq!(env["result"]["type"], "ok", "{env}");
    } else {
        assert_eq!(code, 1, "{env}");
        assert_eq!(env["error"]["code"], "not_found", "{env}");
        let message = env["error"]["message"].as_str().unwrap_or_default();
        assert!(message.starts_with("no agents"), "{env}");
    }

    // With an agent pane, open-target goes to it. This one has reported no state yet, so it
    // is in the last group, the rest; the notification above named no pane and moved
    // nothing.
    let pane = f.first_pane();
    f.set_pane_option(&pane, "@agent_kind", "claude");
    let terminal_id = f.ok(&["pane", "get", &pane], "cli:pane:get", "pane")["pane"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_string();
    let opened = f.ok(
        &["notification", "open-target"],
        "cli:notification:open-target",
        "ok",
    );
    assert_eq!(opened["pane_id"], terminal_id.as_str(), "{opened}");
    assert_eq!(opened["target"], "rest", "{opened}");
}

/// `open-target` reports the group of the pane it went to, `target`: `needs_you`, `idle` or
/// `rest` (next up: what needs you, else every agent). The one agent pane here changes group
/// as its state does.
#[test]
fn open_target_reports_the_group_of_the_pane_it_went_to() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.set_pane_option(&pane, "@agent_kind", "claude");
    let terminal_id = f.terminal_id(&pane);

    for (state, group) in [
        ("blocked", "needs_you"),
        ("idle", "idle"),
        ("working", "rest"),
    ] {
        f.set_pane_option(&pane, "@agent_state", state);
        let opened = f.ok(
            &["notification", "open-target"],
            "cli:notification:open-target",
            "ok",
        );
        assert_eq!(opened["pane_id"], terminal_id.as_str(), "{state}: {opened}");
        assert_eq!(opened["target"], group, "{state}: {opened}");
    }
}

#[test]
fn manifest_list_covers_every_bundled_agent() {
    let f = Fixture::start();
    let result = f.ok(&["manifest", "list"], "cli:manifest:list", "manifest_list");
    let manifests = result["manifests"].as_array().unwrap();
    assert_eq!(
        manifests.len(),
        22,
        "one entry per bundled manifest (src/detect/manifests/*.toml): {result}"
    );
    let claude = manifests
        .iter()
        .find(|m| m["id"] == "claude")
        .unwrap_or_else(|| panic!("no claude manifest: {result}"));
    assert_eq!(claude["active_source"], "bundled", "{claude}");
    assert!(claude["bundled_version"].is_string(), "{claude}");

    let shown = f.ok(
        &["manifest", "show", "claude"],
        "cli:manifest:show",
        "manifest",
    );
    assert_eq!(shown["id"], "claude", "{shown}");
    assert!(
        !shown["rules"].as_array().unwrap().is_empty(),
        "a manifest should list its rules: {shown}"
    );

    let (code, env) = f.run(&["manifest", "show", "not-an-agent"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn config_check_path_and_default() {
    let f = Fixture::start();
    let checked = f.ok(&["config", "check"], "cli:config:check", "config_check");
    assert!(
        checked["diagnostics"].is_array(),
        "diagnostics must be an array even when clean: {checked}"
    );
    assert!(
        checked["diagnostics"].as_array().unwrap().is_empty(),
        "a missing config.toml has nothing to report: {checked}"
    );

    let paths = f.ok(&["config", "path"], "cli:config:path", "config_path");
    assert!(
        paths["config_file"]
            .as_str()
            .unwrap()
            .starts_with(f.config.path().to_str().unwrap()),
        "config path must honour $TAW_CONFIG_DIR: {paths}"
    );

    // `config default` prints raw TOML, not an envelope: it exists to be redirected
    // (`taw config default > config.toml`), and a JSON wrapper would make every user
    // pipe it back out through `jq`.
    let (code, stdout, stderr) = f.run_raw(&["config", "default"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(stdout.contains("[ui.toast]"), "{stdout}");
    assert!(
        !stdout.trim_start().starts_with('{'),
        "config default must not be wrapped in an envelope: {stdout}"
    );
    assert!(stdout.ends_with('\n'), "should end with a newline");
    // It really is valid TOML, i.e. redirecting it produces a usable config file.
    let parsed: toml::Table = toml::from_str(&stdout).expect("config default must emit valid TOML");
    assert!(parsed.contains_key("ui"), "{parsed:?}");

    // A broken config.toml is reported as an error diagnostic, not a crash.
    std::fs::write(f.config.path().join("config.toml"), "this is = not = toml").unwrap();
    let (checked, _) = f.config_check(1);
    let diagnostics = checked["diagnostics"].as_array().unwrap();
    assert!(!diagnostics.is_empty(), "{checked}");
    assert_eq!(diagnostics[0]["level"], "error", "{checked}");

    // ...and other commands still work, on defaults, with a warning on stderr.
    let (code, stdout, stderr) = f.run_raw(&["pane", "list"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert!(
        stderr.to_lowercase().contains("config"),
        "a config parse failure should warn on stderr: {stderr}"
    );
}

/// A file in your sandbox profiles folder that is not executable is a warning of `taw config
/// check` with no config.toml at all, and `taw config reload` lists it too. Reload's status is the
/// file's alone: `applied` beside that warning, since no line of the file failed to take effect,
/// and `partial` for a line that did, a profile the file names that is not there among them.
#[test]
fn config_check_reads_the_profiles_folder_and_reload_judges_the_file_alone() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::start();
    let folder = f.config.path().join("sandbox");
    std::fs::create_dir_all(&folder).unwrap();
    let draft = folder.join("draft");
    std::fs::write(&draft, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = format!(
        "sandbox: the profile draft is not executable (chmod +x {})",
        draft.display()
    );
    let listed = |result: &Value| -> Vec<(String, String)> {
        result["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["level"].as_str().unwrap().to_string(),
                    d["message"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let the_warning = [("warning".to_string(), said.clone())];

    let checked = f.ok(&["config", "check"], "cli:config:check", "config_check");
    assert_eq!(listed(&checked), the_warning, "no config.toml: {checked}");
    let reloaded = f.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(reloaded["status"], "applied", "no config.toml: {reloaded}");
    assert_eq!(listed(&reloaded), the_warning, "no config.toml: {reloaded}");

    let file = f.config.path().join("config.toml");
    std::fs::write(&file, "[ui.toast]\ndelivery = \"off\"\n").unwrap();
    let reloaded = f.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(
        reloaded["status"], "applied",
        "a file with nothing wrong: {reloaded}"
    );
    assert_eq!(
        listed(&reloaded),
        the_warning,
        "a file with nothing wrong: {reloaded}"
    );

    for line in ["[sandbox]\nprofile = \"team\"\n", "[nonsense]\nkey = 1\n"] {
        std::fs::write(&file, line).unwrap();
        let reloaded = f.ok(&["config", "reload"], "cli:config:reload", "config_reload");
        assert_eq!(reloaded["status"], "partial", "{line:?}: {reloaded}");
        assert!(
            listed(&reloaded).contains(&the_warning[0]),
            "{line:?}: {reloaded}"
        );
    }

    std::fs::write(&file, "this is = not = toml").unwrap();
    let (checked, _) = f.config_check(1);
    let found = listed(&checked);
    assert_eq!(found.len(), 2, "a broken config.toml: {checked}");
    assert_eq!(found[0].0, "error", "a broken config.toml: {checked}");
    assert_eq!(found[1], the_warning[0], "a broken config.toml: {checked}");
    let reloaded = f.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(
        reloaded["status"], "failed",
        "a broken config.toml: {reloaded}"
    );
}

#[test]
fn status_renders_the_rail_and_refresh_writes_it() {
    let f = Fixture::start();
    let status = f.ok(&["status"], "cli:status:show", "status");
    assert!(status["text"].is_string(), "{status}");
    assert_eq!(status["counts"]["blocked"], 0, "{status}");

    let pane = f.first_pane();
    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "blocked",
        ],
        "cli:pane:report-agent",
        "ok",
    );

    let refreshed = f.ok(&["status", "refresh"], "cli:status:refresh", "status");
    assert_eq!(refreshed["counts"]["blocked"], 1, "{refreshed}");
    assert!(
        !refreshed["text"].as_str().unwrap().is_empty(),
        "one blocked agent must render something: {refreshed}"
    );
    // `refresh` is the variant that writes the global option the status line reads.
    let written = f
        .tmux(&["show-options", "-gqv", "@agent_wizard_status"])
        .unwrap();
    assert_eq!(written, refreshed["text"].as_str().unwrap());
}

#[test]
fn pane_layout_of_a_split_window_follows_herdrs_bsp_tree() {
    let f = Fixture::start();
    let pane = f.first_pane();
    f.ok(
        &["pane", "split", &pane, "--direction", "right", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );

    let result = f.ok(
        &["pane", "layout", "--pane", &pane],
        "cli:pane:layout",
        "pane_layout",
    );
    // herdr's documented BSP tree: `split` nodes with `direction` (`right`/`down`),
    // `ratio`, `first` and `second`; leaves are `pane` nodes
    // (third_party/herdr/reference/socket-api.mdx, `layout.export`).
    let root = &result["layout"];
    assert_eq!(root["type"], "split", "{result}");
    assert_eq!(
        root["direction"], "right",
        "a `--direction right` split reuses that same vocabulary: {result}"
    );
    assert!(
        root.get("children").is_none(),
        "herdr's tree is binary — first/second, never a children array: {result}"
    );
    for side in ["first", "second"] {
        let child = &root[side];
        assert_eq!(child["type"], "pane", "{side}: {child}");
        assert!(child["pane_id"].is_string(), "{side}: {child}");
        assert!(child["width"].as_u64().unwrap() > 0, "{side}: {child}");
    }
    assert_ne!(
        root["first"]["pane_id"], root["second"]["pane_id"],
        "the two sides must be different panes: {result}"
    );
    let ratio = root["ratio"].as_f64().unwrap();
    assert!(
        (0.3..=0.7).contains(&ratio),
        "an even split should be near 0.5, got {ratio}"
    );

    // A single-pane window is a bare `pane` node, and a tab id works as the target too.
    let f2 = Fixture::start();
    let solo_pane = f2.first_pane();
    let solo = f2.ok(
        &["pane", "layout", &solo_pane],
        "cli:pane:layout",
        "pane_layout",
    );
    assert_eq!(solo["layout"]["type"], "pane", "{solo}");
    assert_eq!(solo["layout"]["pane_id"], solo_pane.as_str(), "{solo}");
    let tab_id = solo["tab_id"].as_str().unwrap().to_string();
    let by_tab = f2.ok(
        &["pane", "layout", &tab_id],
        "cli:pane:layout",
        "pane_layout",
    );
    assert_eq!(by_tab["layout"], solo["layout"], "{by_tab}");

    // With no target and no $TMUX_PANE there is nothing to report.
    let (code, env) = f2.run(&["pane", "layout"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

/// Every leaf of a `pane layout` must resolve to a live pane even after a pane has been
/// killed. tmux writes the persistent pane-id NUMBER into each layout leaf, not the
/// positional `#{pane_index}`; matching leaves against the index resolved every leaf but
/// the first to null the moment the two diverged (which a kill guarantees).
#[test]
fn pane_layout_resolves_every_leaf_after_a_pane_is_killed() {
    let f = Fixture::start();
    let pane = f.first_pane();

    // Four panes, then kill the first so pane_index (renumbered 0,1,2) no longer tracks the
    // pane-id numbers (1,2,3) that the layout leaves carry.
    for dir in ["right", "down", "right"] {
        f.ok(
            &["pane", "split", &pane, "--direction", dir, "--no-focus"],
            "cli:pane:split",
            "pane_split",
        );
    }
    // Raw tmux ids (`%N`) — `first_pane()` is taw's public id, which raw tmux cannot target.
    let raw_first = f
        .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    let window = f
        .tmux(&["display-message", "-p", "-t", &raw_first, "#{window_id}"])
        .unwrap();
    f.tmux(&["kill-pane", "-t", &raw_first]).unwrap();

    let mut live: Vec<String> = f
        .tmux(&["list-panes", "-t", &window, "-F", "#{pane_id}"])
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    live.sort();
    assert_eq!(live.len(), 3, "expected three panes left after the kill");

    let result = f.ok(
        &["pane", "layout", "--pane", &live[0]],
        "cli:pane:layout",
        "pane_layout",
    );

    fn collect_leaves(node: &Value, out: &mut Vec<Value>) {
        if node["type"] == "pane" {
            out.push(node["terminal_id"].clone());
        } else if node["type"] == "split" {
            collect_leaves(&node["first"], out);
            collect_leaves(&node["second"], out);
        }
    }
    let mut leaves = Vec::new();
    collect_leaves(&result["layout"], &mut leaves);

    assert!(
        leaves.iter().all(|t| t.is_string()),
        "a layout leaf resolved to null after the kill: {result}"
    );
    let mut got: Vec<String> = leaves
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect();
    got.sort();
    assert_eq!(
        got, live,
        "every layout leaf must map to exactly the live panes: {result}"
    );
}

#[test]
fn pane_neighbor_and_edges_report_geometry() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let split = f.ok(
        &["pane", "split", &pane, "--direction", "right", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let right = split["pane"]["pane_id"].as_str().unwrap().to_string();

    let neighbor = f.ok(
        &["pane", "neighbor", "--pane", &pane, "--direction", "right"],
        "cli:pane:neighbor",
        "pane",
    );
    assert_eq!(neighbor["pane"]["pane_id"], right.as_str(), "{neighbor}");

    // Nothing to the left of the leftmost pane.
    let (code, env) = f.run(&["pane", "neighbor", "--pane", &pane, "--direction", "left"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");

    let edges = f.ok(
        &["pane", "edges", "--pane", &pane],
        "cli:pane:edges",
        "pane_edges",
    );
    assert_eq!(edges["at_left"], true, "{edges}");
    assert_eq!(edges["at_right"], false, "{edges}");
    assert_eq!(edges["at_top"], true, "{edges}");
}

#[test]
fn session_save_then_list_and_delete() {
    let f = Fixture::start();
    let saved = f.ok(
        &["session", "save", "--name", "snap1"],
        "cli:session:save",
        "session_saved",
    );
    assert_eq!(saved["name"], "snap1", "{saved}");
    assert_eq!(saved["sessions"], 1, "{saved}");

    let listed = f.ok(&["session", "list"], "cli:session:list", "session_list");
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["name"], "snap1", "{listed}");
    assert!(sessions[0]["taken_unix"].as_u64().unwrap() > 0, "{listed}");

    // The saved session already exists on this server, so a restore skips it.
    let restored = f.ok(
        &["session", "restore", "--name", "snap1", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
    assert!(
        !restored["plan"].as_array().unwrap().is_empty(),
        "a dry run must explain its plan: {restored}"
    );
    assert!(
        restored["created"].as_array().unwrap().is_empty(),
        "--dry-run must not create anything: {restored}"
    );

    f.ok(&["session", "delete", "snap1"], "cli:session:delete", "ok");
    let after = f.ok(&["session", "list"], "cli:session:list", "session_list");
    assert!(after["sessions"].as_array().unwrap().is_empty(), "{after}");

    let (code, env) = f.run(&["session", "delete", "snap1"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

/// herdr v0.9.1 (#3819, #3220): a saved session is deleted by its exact name. On a
/// case-insensitive disk (macOS's default) `Default` must not reach `default.json`: it
/// is `not_found`, the refusal names the spelling that does exist, and the snapshot stays.
#[test]
fn session_delete_needs_the_saved_names_exact_spelling() {
    let f = Fixture::start();
    f.ok(&["session", "save"], "cli:session:save", "session_saved");

    let (code, env) = f.run(&["session", "delete", "Default"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("'default'"),
        "the refusal should name the spelling that is saved: {message}"
    );

    let listed = f.ok(&["session", "list"], "cli:session:list", "session_list");
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["name"], "default", "{listed}");

    f.ok(
        &["session", "delete", "default"],
        "cli:session:delete",
        "ok",
    );
}

/// herdr v0.9.1 (#4233): a saved name that starts with `-` is reachable after `--`, so it
/// gets to the delete rather than being read as an option.
#[test]
fn session_delete_reaches_a_name_that_starts_with_a_hyphen_after_a_double_dash() {
    let f = Fixture::start();
    f.ok(
        &["session", "save", "--name=-x"],
        "cli:session:save",
        "session_saved",
    );

    f.ok(
        &["session", "delete", "--", "-x"],
        "cli:session:delete",
        "ok",
    );

    let listed = f.ok(&["session", "list"], "cli:session:list", "session_list");
    assert!(
        listed["sessions"].as_array().unwrap().is_empty(),
        "{listed}"
    );
}

/// herdr treats a saved session from a newer version as unloadable. A snapshot written by a
/// newer taw is therefore not restored by this one (`unsupported`, the message says why and
/// what to do), still shows in `session list`, and is copied to `session-backups/` before a
/// save replaces it, even though it still parses.
#[test]
fn session_restore_refuses_a_snapshot_a_newer_taw_wrote_and_save_keeps_a_copy() {
    let f = Fixture::start();
    f.ok(&["session", "save"], "cli:session:save", "session_saved");
    let file = f.state.path().join("sessions").join("default.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["version"] = Value::from(99);
    let newer = serde_json::to_vec_pretty(&value).unwrap();
    std::fs::write(&file, &newer).unwrap();

    let (code, env) = f.run(&["session", "restore", "--dry-run"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "unsupported", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("newer taw") && message.contains("update taw"),
        "{message}"
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        newer,
        "restore leaves it alone"
    );

    let listed = f.ok(&["session", "list"], "cli:session:list", "session_list");
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1, "{listed}");

    f.ok(&["session", "save"], "cli:session:save", "session_saved");
    let backups: Vec<_> =
        std::fs::read_dir(f.state.path().join("sessions").join("session-backups"))
            .unwrap()
            .map(|e| std::fs::read(e.unwrap().path()).unwrap())
            .collect();
    assert_eq!(
        backups,
        vec![newer],
        "the newer file was kept before it was replaced"
    );
    f.ok(
        &["session", "restore", "--dry-run"],
        "cli:session:restore",
        "session_restored",
    );
}

/// A directory that does not exist is `not_found`, not the generic `internal` a plain
/// `anyhow::bail!` would otherwise report (see `session::create::open_or_create`, which
/// has no typed error for `cli::session::open` to downcast).
#[test]
fn session_open_on_a_missing_directory_is_not_found() {
    let f = Fixture::start();
    let missing = f.work.path().join("no-such-project");

    let (code, env) = f.run(&["session", "open", missing.to_str().unwrap()]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no such directory"),
        "{env}"
    );
}

/// Every folder taw gives tmux opens as itself, one with a `#` in its name and one whose name ends in
/// `;` included: tmux expands formats in a start directory (`C#Sharp` became `C<session>harp`, and a
/// folder that is not there is the home folder), and ends a command at a word that ends in `;` (the rest
/// of the verb's words then failed as `unknown command`). Through `pane split --cwd`, `workspace create
/// --cwd`, `tab create --cwd`, `pane move --new-workspace`, and `session open` of a folder inside one.
#[test]
fn every_folder_taw_gives_tmux_opens_as_itself_whatever_its_name_holds() {
    let f = Fixture::start();
    let tmp = tempfile::tempdir().unwrap();
    let cwd_of = |terminal: &str| {
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            terminal,
            "#{pane_current_path}",
        ])
        .unwrap()
    };
    let path_of = |target: &str| {
        f.tmux(&["display-message", "-p", "-t", target, "#{session_path}"])
            .unwrap()
    };
    for (n, name) in ["C#Sharp", "semi;"].into_iter().enumerate() {
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap().display().to_string();

        let split = f.ok(
            &[
                "pane",
                "split",
                &f.first_pane(),
                "--cwd",
                &dir,
                "--no-focus",
            ],
            "cli:pane:split",
            "pane_split",
        );
        let split = split["pane"]["terminal_id"].as_str().unwrap().to_string();
        assert!(
            common::wait_for(|| cwd_of(&split) == dir, std::time::Duration::from_secs(10)),
            "pane split --cwd {dir}: {}",
            cwd_of(&split)
        );

        let label = format!("folder-{n}");
        let created = f.ok(
            &[
                "workspace",
                "create",
                "--cwd",
                &dir,
                "--label",
                &label,
                "--no-focus",
            ],
            "cli:workspace:create",
            "workspace_created",
        );
        let root = created["root_pane"]["terminal_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            common::wait_for(|| cwd_of(&root) == dir, std::time::Duration::from_secs(10)),
            "workspace create --cwd {dir}: {}",
            cwd_of(&root)
        );
        assert_eq!(path_of(&format!("={label}:")), dir);

        let tab = f.ok(
            &[
                "tab",
                "create",
                "--workspace",
                &label,
                "--cwd",
                &dir,
                "--no-focus",
            ],
            "cli:tab:create",
            "tab_created",
        );
        let tab = tab["root_pane"]["terminal_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            common::wait_for(|| cwd_of(&tab) == dir, std::time::Duration::from_secs(10)),
            "tab create --cwd {dir}: {}",
            cwd_of(&tab)
        );

        let moved = format!("moved-{n}");
        f.ok(
            &[
                "pane",
                "move",
                &split,
                "--new-workspace",
                "--label",
                &moved,
                "--no-focus",
            ],
            "cli:pane:move",
            "pane_moved",
        );
        assert_eq!(
            path_of(&format!("={moved}:")),
            dir,
            "pane move --new-workspace"
        );

        // `session open` names the session after the folder, and tmux reads a name as a format too
        // (a name of its own, which this test leaves alone): a folder inside this one has a plain
        // name, and its path the `#` or `;` of this one.
        let project = std::path::Path::new(&dir).join("project");
        std::fs::create_dir(&project).unwrap();
        let dir = project.display().to_string();
        let opened = f.ok(
            &["session", "open", &dir],
            "cli:session:open",
            "session_open",
        );
        assert_eq!(opened["created"], true, "{opened}");
        let session = opened["session"].as_str().unwrap();
        let first = f
            .tmux(&[
                "list-panes",
                "-t",
                &format!("={session}:"),
                "-F",
                "#{pane_id}",
            ])
            .unwrap();
        assert!(
            common::wait_for(|| cwd_of(&first) == dir, std::time::Duration::from_secs(10)),
            "session open {dir}: {}",
            cwd_of(&first)
        );
        assert_eq!(path_of(&format!("={session}:")), dir, "session open");
    }
}

/// A name that starts with a dash is the name `workspace rename` and `tab rename` give tmux. tmux reads a
/// word after `rename-session -t <id>` or `rename-window -t <id>` that starts with a dash as a flag
/// (`unknown flag -dash`) unless a `--` ends the flags, so a rename to such a name was refused. Both
/// verbs put a `--` ahead of the name they give tmux.
#[test]
fn a_name_that_starts_with_a_dash_renames_a_workspace_and_a_tab() {
    let f = Fixture::start();
    let created = f.ok(
        &["workspace", "create", "--label", "plain", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let terminal = created["root_pane"]["terminal_id"].as_str().unwrap();
    let tab = f.ok(
        &[
            "tab",
            "create",
            "--workspace",
            &workspace,
            "--label",
            "plain tab",
            "--no-focus",
        ],
        "cli:tab:create",
        "tab_created",
    );
    let tab_id = tab["tab"]["tab_id"].as_str().unwrap().to_string();

    let got = f.ok(
        &["workspace", "rename", &workspace, "--", "-dash"],
        "cli:workspace:rename",
        "workspace",
    );
    assert_eq!(got["workspace"]["label"], "-dash", "{got}");
    let got = f.ok(
        &["tab", "rename", &tab_id, "--", "-dash tab"],
        "cli:tab:rename",
        "tab",
    );
    assert_eq!(got["tab"]["label"], "-dash tab", "{got}");
    // What tmux holds, read from tmux itself and not from the verbs' own reply.
    let held = f
        .tmux(&["display-message", "-p", "-t", terminal, "#{session_name}"])
        .unwrap();
    assert_eq!(held, "-dash", "the session tmux holds");
    let windows = f
        .tmux(&[
            "list-windows",
            "-t",
            &format!("={held}:"),
            "-F",
            "#{window_name}",
        ])
        .unwrap();
    assert!(
        windows.lines().any(|name| name == "-dash tab"),
        "the windows tmux holds: {windows}"
    );
}

/// Every name taw gives tmux for a session or a window it makes or renames is the name it gets, a `#` in
/// it or a `;` at its end included: tmux expands formats in such a name (`C#Sharp` became `Charp`, a
/// window `W#Sx` `W<the session's name>x`), and ends a command at a word that ends in `;`. Through
/// `workspace create --label` and `workspace rename`, `tab create --label` and `tab rename`, `pane move
/// --new-workspace` with its two labels, `pane move --new-tab --label`, and `session open` of a folder of
/// that name.
#[test]
fn every_name_taw_gives_tmux_for_a_session_or_a_window_is_the_name_it_gets() {
    let f = Fixture::start();
    let tmp = tempfile::tempdir().unwrap();
    let names_of = |terminal: &str| {
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            terminal,
            "#{session_name}\t#{window_name}",
        ])
        .unwrap()
    };
    let split = |f: &Fixture| {
        let split = f.ok(
            &["pane", "split", &f.first_pane(), "--no-focus"],
            "cli:pane:split",
            "pane_split",
        );
        (
            split["pane"]["pane_id"].as_str().unwrap().to_string(),
            split["pane"]["terminal_id"].as_str().unwrap().to_string(),
        )
    };
    for name in ["C#Sharp", "semi;"] {
        let created = f.ok(
            &["workspace", "create", "--label", name, "--no-focus"],
            "cli:workspace:create",
            "workspace_created",
        );
        assert_eq!(created["workspace"]["label"], name, "{created}");
        let workspace = created["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_string();
        let root = created["root_pane"]["terminal_id"].as_str().unwrap();
        assert!(
            names_of(root).starts_with(&format!("{name}\t")),
            "workspace create --label {name}: {}",
            names_of(root)
        );
        let renamed = format!("re {name}");
        let got = f.ok(
            &["workspace", "rename", &workspace, &renamed],
            "cli:workspace:rename",
            "workspace",
        );
        assert_eq!(got["workspace"]["label"], renamed.as_str(), "{got}");

        let label = format!("tab {name}");
        let tab = f.ok(
            &[
                "tab",
                "create",
                "--workspace",
                &workspace,
                "--label",
                &label,
                "--no-focus",
            ],
            "cli:tab:create",
            "tab_created",
        );
        assert_eq!(tab["tab"]["label"], label.as_str(), "{tab}");
        let tab_id = tab["tab"]["tab_id"].as_str().unwrap().to_string();
        let relabelled = format!("re tab {name}");
        let got = f.ok(
            &["tab", "rename", &tab_id, &relabelled],
            "cli:tab:rename",
            "tab",
        );
        assert_eq!(got["tab"]["label"], relabelled.as_str(), "{got}");

        let (pane, terminal) = split(&f);
        let (moved, moved_tab) = (format!("moved {name}"), format!("moved tab {name}"));
        f.ok(
            &[
                "pane",
                "move",
                &pane,
                "--new-workspace",
                "--label",
                &moved,
                "--tab-label",
                &moved_tab,
                "--no-focus",
            ],
            "cli:pane:move",
            "pane_moved",
        );
        assert_eq!(names_of(&terminal), format!("{moved}\t{moved_tab}"));

        let (pane, terminal) = split(&f);
        let broken = format!("broken {name}");
        f.ok(
            &[
                "pane",
                "move",
                &pane,
                "--new-tab",
                "--label",
                &broken,
                "--no-focus",
            ],
            "cli:pane:move",
            "pane_moved",
        );
        // The window's name: the session tmux breaks the pane out into is not this test's.
        assert!(
            names_of(&terminal).ends_with(&format!("\t{broken}")),
            "pane move --new-tab --label {broken}: {}",
            names_of(&terminal)
        );

        // `session open` names the session after the folder.
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap().display().to_string();
        let opened = f.ok(
            &["session", "open", &dir],
            "cli:session:open",
            "session_open",
        );
        assert_eq!(opened["created"], true, "{opened}");
        assert_eq!(opened["session"], name, "{opened}");
        assert_eq!(
            f.tmux(&[
                "display-message",
                "-p",
                "-t",
                &format!("={name}:"),
                "#{session_name}",
            ])
            .unwrap(),
            name,
            "session open {dir}"
        );
    }
}

/// The happy path: a real directory with no session on it yet gets a new one, and the
/// attached client is switched to it.
#[test]
fn session_open_creates_a_session_and_switches_the_attached_client() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    assert_eq!(client.session(&f), "one", "the client starts on 'one'");

    let project = f.work.path().join("my-project");
    std::fs::create_dir(&project).unwrap();

    let opened = f.ok(
        &["session", "open", project.to_str().unwrap()],
        "cli:session:open",
        "session_open",
    );
    assert_eq!(opened["created"], true, "{opened}");
    assert_eq!(opened["collision"], Value::Null, "{opened}");
    assert_eq!(opened["switched"], true, "{opened}");
    let name = opened["session"].as_str().unwrap().to_string();

    assert_eq!(
        client.session(&f),
        name,
        "the attached client should have switched to the new session"
    );
    let session_path = f
        .tmux(&["display-message", "-p", "-t", &name, "#{session_path}"])
        .unwrap();
    assert_eq!(
        session_path,
        project.canonicalize().unwrap().to_str().unwrap()
    );
}

/// Opening the same directory twice must not create a second session: the running
/// session's `session_path` already names it, so the second call just reports it back.
#[test]
fn session_open_on_an_already_open_directory_finds_it_rather_than_duplicating_it() {
    let f = Fixture::start();
    let project = f.work.path().join("reopened-project");
    std::fs::create_dir(&project).unwrap();

    let first = f.ok(
        &["session", "open", project.to_str().unwrap()],
        "cli:session:open",
        "session_open",
    );
    assert_eq!(first["created"], true, "{first}");
    let name = first["session"].as_str().unwrap().to_string();

    let second = f.ok(
        &["session", "open", project.to_str().unwrap()],
        "cli:session:open",
        "session_open",
    );
    assert_eq!(second["created"], false, "{second}");
    assert_eq!(second["session"], name, "{second}");

    let sessions = f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap();
    assert_eq!(
        sessions.lines().filter(|s| *s == name).count(),
        1,
        "must not create a duplicate session for the same directory: {sessions:?}"
    );
}

/// An executable `/bin/sh` script at `path`, for the create-hook tests below.
fn write_hook(path: &std::path::Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    crate::common::make_executable(path);
}

/// A config file with the create hooks set as written (`zoxide` off: it defaults to on,
/// and `session open` would then run the real `zoxide add` on a temp path).
fn write_hook_config(f: &Fixture, pre: Option<&str>, post: Option<&str>) {
    let mut text = String::from("[sessions]\nzoxide = false\n");
    for (key, value) in [("pre_create_hook", pre), ("post_create_hook", post)] {
        if let Some(value) = value {
            text.push_str(&format!(
                "{key} = {}\n",
                toml::Value::String(value.to_string())
            ));
        }
    }
    std::fs::write(f.config.path().join("config.toml"), text).unwrap();
}

/// A hook's own `tmux` reaches this test's server and not the developer's: `$TMUX` names
/// the fixture's socket, as it does in a real popup or key binding.
fn hook_env(f: &Fixture) -> String {
    let socket = f
        .tmux(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    format!("{socket},0,0")
}

/// The post-create hook runs right after the session is made and before the attached client
/// is moved to it: from inside the hook the client is still where it was, and by the time
/// `session open` returns it has moved.
#[test]
fn session_open_runs_the_post_hook_before_it_switches_the_client() {
    let f = Fixture::start();
    let client = AttachedClient::attach(&f);
    let project = f.work.path().join("hooked-project");
    std::fs::create_dir(&project).unwrap();
    let log = f.work.path().join("post.log");
    let hook = f.work.path().join("post.sh");
    write_hook(
        &hook,
        &format!(
            "tmux list-clients -F '#{{client_session}}' >> '{}'\n",
            log.display()
        ),
    );
    write_hook_config(&f, None, Some(hook.to_str().unwrap()));

    let (code, stdout, stderr) = f.run_with_env(
        &["session", "open", project.to_str().unwrap()],
        &[("TMUX", &hook_env(&f))],
    );

    assert_eq!(code, 0, "{stdout}{stderr}");
    let opened = parse_envelope(&["session", "open"], &stdout, &stderr);
    assert_eq!(opened["result"]["created"], true, "{opened}");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "one\n",
        "the client had not moved yet when the hook ran"
    );
    assert_eq!(
        client.session(&f),
        opened["result"]["session"].as_str().unwrap(),
        "and it has by the time the command returns"
    );
}

/// A create hook with flags of its own that answers with a name and a directory, run by the
/// built binary from a config file: the session takes the name, in that directory.
#[test]
fn session_open_follows_a_pre_hook_with_flags_that_prints_a_name_and_a_directory() {
    let f = Fixture::start();
    let project = f.work.path().join("ideas");
    let elsewhere = f.work.path().join("elsewhere");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let hook = f.work.path().join("pre.sh");
    write_hook(
        &hook,
        &format!(
            "[ \"$1\" = --prefix ] || exit 9\necho \"$2$3\"\necho '{}'\n",
            elsewhere.display()
        ),
    );
    write_hook_config(
        &f,
        Some(&format!("{} --prefix work-", hook.display())),
        None,
    );

    let opened = f.ok(
        &["session", "open", project.to_str().unwrap()],
        "cli:session:open",
        "session_open",
    );

    assert_eq!(opened["session"], "work-ideas", "{opened}");
    assert_eq!(opened["created"], true, "{opened}");
    let session_path = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            "work-ideas",
            "#{session_path}",
        ])
        .unwrap();
    assert_eq!(
        session_path,
        elsewhere.canonicalize().unwrap().to_str().unwrap(),
        "the second line is where the session runs"
    );
}

/// A pre-create hook that exits non-zero stops `session open`: the error carries the code
/// `hook_refused`, says which hook and why, and nothing is created.
#[test]
fn session_open_stops_when_the_pre_create_hook_fails() {
    let f = Fixture::start();
    let project = f.work.path().join("refused");
    std::fs::create_dir(&project).unwrap();
    let hook = f.work.path().join("pre.sh");
    write_hook(&hook, "echo 'not today' >&2\nexit 4\n");
    write_hook_config(&f, Some(hook.to_str().unwrap()), None);

    let (code, env) = f.run(&["session", "open", project.to_str().unwrap()]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "hook_refused",
        "a refusal is not an internal error: {env}"
    );
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("pre-create hook") && message.contains("not today"),
        "{env}"
    );
    assert_eq!(
        f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap(),
        "one",
        "no session was created"
    );
}

/// A pre-create hook that runs past `[sessions] hook_timeout_seconds` is stopped and refuses
/// the open, with the same code as any other refusal and a message that says it took too long.
#[test]
fn session_open_refuses_when_the_pre_create_hook_runs_past_the_limit() {
    let f = Fixture::start();
    let project = f.work.path().join("slow");
    std::fs::create_dir(&project).unwrap();
    let hook = f.work.path().join("pre.sh");
    write_hook(&hook, "sleep 30\n");
    write_hook_config(&f, Some(hook.to_str().unwrap()), None);
    let config = f.config.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("hook_timeout_seconds = 1\n");
    std::fs::write(&config, text).unwrap();

    let started = std::time::Instant::now();
    let (code, env) = f.run(&["session", "open", project.to_str().unwrap()]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    assert_eq!(
        env["error"]["message"], "pre-create hook took longer than 1 s",
        "{env}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(9),
        "session open waited {:?} with a limit of 1 s",
        started.elapsed()
    );
    assert_eq!(
        f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap(),
        "one",
        "no session was created"
    );
}

/// Of `pids`, those still running (stopped ones included) whose command line has one of `marks` in
/// it, by one `ps`: a zombie has ended, and a pid the system has given to another process since is
/// none of this test's.
fn still_running(pids: &[u32], marks: &[&str]) -> Vec<u32> {
    if pids.is_empty() {
        return Vec::new();
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let out = Command::new("ps")
        .args(["-o", "pid=,stat=,command=", "-p", &list])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let stat = fields.next()?;
            let ours = marks.iter().any(|mark| line.contains(mark));
            (!stat.contains('Z') && ours).then_some(pid)
        })
        .collect()
}

/// A `kill` that passes a stop on to the real one and cannot send a kill: what a machine short of
/// processes does to a `kill` that taw has to start for the second of the two.
fn kill_that_cannot_kill(dir: &std::path::Path) -> std::path::PathBuf {
    let bin = dir.join("kill-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_hook(
        &bin.join("kill"),
        "case \"$1\" in -KILL|-9) exit 1 ;; esac\nexec /bin/kill \"$@\"\n",
    );
    bin
}

/// A pre-create hook that runs past its limit is stopped with what it started, and nothing of it is
/// left: neither a process that runs on nor one frozen in place. taw sends the signals itself, so a
/// `kill` it could not start would not leave the hook's tree stopped for good, taken in by launchd,
/// as one did on a loaded machine.
#[test]
fn a_pre_create_hook_stopped_at_its_limit_leaves_nothing_behind_when_kill_cannot_run() {
    let f = Fixture::start();
    let project = f.work.path().join("slow");
    std::fs::create_dir(&project).unwrap();
    let pids = f.work.path().join("pids");
    let started = f.work.path().join("started");
    let hook = f.work.path().join("pre.sh");
    // The hook, a shell under it that starts a sleep every few milliseconds, and each sleep, every one
    // noting its pid.
    write_hook(
        &hook,
        &format!(
            "echo $$ >> '{pids}'\n\
             ( exec sh -c 'echo $$ >> \"$0\"; while :; do sleep 31.9 & echo $! >> \"$0\"; : > \"$1\"; sleep 0.02; done' '{pids}' '{started}' ) &\n\
             wait\n",
            pids = pids.display(),
            started = started.display()
        ),
    );
    write_hook_config(&f, Some(hook.to_str().unwrap()), None);
    let config = f.config.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("hook_timeout_seconds = 2\n");
    std::fs::write(&config, text).unwrap();
    let path = format!(
        "{}:{}",
        kill_that_cannot_kill(f.work.path()).display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let (code, stdout, _) = f.run_with_env(
        &["session", "open", project.to_str().unwrap()],
        &[("PATH", &path)],
    );

    let recorded: Vec<u32> = std::fs::read_to_string(&pids)
        .unwrap_or_default()
        .lines()
        .filter_map(|pid| pid.trim().parse().ok())
        .collect();
    let work = f.work.path().display().to_string();
    let marks = ["sleep 31.9", work.as_str()];
    let mut left = still_running(&recorded, &marks);
    for _ in 0..200 {
        if left.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        left = still_running(&recorded, &marks);
    }
    for pid in &left {
        // Never left behind, whatever this test says: by the pids the hook noted.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("hook_refused"), "{stdout}");
    assert!(started.exists(), "the hook never started a sleep");
    assert!(
        left.is_empty(),
        "{} of the {} processes of the hook were left behind: {left:?}",
        left.len(),
        recorded.len()
    );
}

/// A directory the pre-create hook prints that is not there is `not_found`, like the one
/// `session open` was given, and its message says the hook chose it.
#[test]
fn session_open_reports_a_missing_directory_the_pre_create_hook_chose_as_not_found() {
    let f = Fixture::start();
    let project = f.work.path().join("chosen");
    std::fs::create_dir(&project).unwrap();
    let missing = f.work.path().join("no-such-dir");
    let hook = f.work.path().join("pre.sh");
    write_hook(
        &hook,
        &format!("echo custom\necho '{}'\n", missing.display()),
    );
    write_hook_config(&f, Some(hook.to_str().unwrap()), None);

    let (code, env) = f.run(&["session", "open", project.to_str().unwrap()]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!("no such directory: {}", missing.display()))
            && message.contains("pre-create hook"),
        "{env}"
    );
    assert_eq!(
        f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap(),
        "one",
        "no session was created"
    );
}

// ---------------------------------------------------------------------------------------
// `[hooks]`: the commands a person hooks to taw's events.
// ---------------------------------------------------------------------------------------

/// Polls `ready` for up to 20 s: a hook is a supervisor process of its own, started under
/// whatever load the rest of the suite puts on the machine.
fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    ready()
}

/// A config file holding `[hooks]` with each `(event, command)` given.
fn write_hooks_config(f: &Fixture, hooks: &[(&str, &str)]) {
    let mut text = String::from("[hooks]\n");
    for (event, command) in hooks {
        text.push_str(&format!(
            "\"{event}\" = {}\n",
            toml::Value::String(command.to_string())
        ));
    }
    std::fs::write(f.config.path().join("config.toml"), text).unwrap();
}

/// The path a hook is told to write to, in a directory of its own so that a test can tell
/// which hook wrote what.
fn hook_out(f: &Fixture, name: &str) -> std::path::PathBuf {
    let dir = f.work.path().join("hook-out");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// The tmux hooks the plugin script installs call `taw plugin event`, so this is the path a
/// hand-made `rename-window` takes: the `[hooks]` command runs, and what tmux knows (the
/// session's name, the window's id) is in its environment though the event carries ids only.
#[test]
fn plugin_event_runs_the_config_hooks_with_the_names_tmux_knows() {
    let f = Fixture::start();
    let out = hook_out(&f, "env.txt");
    write_hooks_config(
        &f,
        &[(
            "tab.renamed",
            &format!(
                "env | grep '^TAW_' | sort > '{0}.tmp' && mv '{0}.tmp' '{0}'",
                out.display()
            ),
        )],
    );
    let window = f.tmux(&["display-message", "-p", "#{window_id}"]).unwrap();
    let session = f.tmux(&["display-message", "-p", "#{session_id}"]).unwrap();

    let result = f.ok(
        &[
            "plugin",
            "event",
            "tab.renamed",
            "--tab",
            &window,
            "--workspace",
            &session,
            "--label",
            "logs",
        ],
        "cli:plugin:event",
        "plugin_event",
    );

    assert_eq!(result["launched"], 1, "{result}");
    assert!(eventually(|| out.exists()), "the hook never ran");
    let env = std::fs::read_to_string(&out).unwrap();
    for line in [
        "TAW_EVENT=tab.renamed".to_string(),
        "TAW_SESSION=one".to_string(),
        format!("TAW_SESSION_ID={session}"),
        format!("TAW_WINDOW={window}"),
        "TAW_TITLE=logs".to_string(),
        // A real event is no test.
        "TAW_TEST=".to_string(),
        // The session's directory as tmux reports it, which is how it was created.
        format!("TAW_DIR={}", f.work.path().display()),
    ] {
        assert!(
            env.lines().any(|l| l == line),
            "{line} is missing from\n{env}"
        );
    }
}

/// A hook that a `taw` command starts (`taw plugin event` here, which is what tmux's own hooks
/// call) gets tmux's global environment as well: which process fires the event does not decide
/// whether a credential is found.
#[test]
fn plugin_event_hooks_get_tmuxs_global_environment() {
    let f = Fixture::start();
    f.tmux(&[
        "set-environment",
        "-g",
        "NTFY_TOPIC",
        "taw-hooks-test-topic",
    ])
    .unwrap();
    let out = hook_out(&f, "topic.txt");
    write_hooks_config(
        &f,
        &[(
            "tab.focused",
            &format!(
                "echo \"$NTFY_TOPIC\" > '{0}.tmp' && mv '{0}.tmp' '{0}'",
                out.display()
            ),
        )],
    );
    let window = f.tmux(&["display-message", "-p", "#{window_id}"]).unwrap();
    let session = f.tmux(&["display-message", "-p", "#{session_id}"]).unwrap();

    f.ok(
        &[
            "plugin",
            "event",
            "tab.focused",
            "--tab",
            &window,
            "--workspace",
            &session,
        ],
        "cli:plugin:event",
        "plugin_event",
    );

    assert!(eventually(|| out.exists()), "the hook never ran");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "taw-hooks-test-topic"
    );
}

/// A hook a `taw` command starts from a pane gets what tmux would give a new pane of the session
/// the event is about: tmux's global environment with the session's own over it, and what the
/// session removes taken out. The pane's own process has what its session had when the pane was
/// made, and a client that attached to the session since has changed that (tmux copies
/// `SSH_AUTH_SOCK` in from the terminal it came from, and marks what that terminal did not have
/// as removed), so a hook that kept the pane's copy would reach an ssh agent that is gone. What
/// the session says nothing of stays as the pane's process has it, and an event about a session
/// that is gone has the global environment alone.
#[test]
fn a_hook_started_from_a_pane_gets_the_environment_of_the_session_the_event_is_about() {
    let f = Fixture::start();
    f.tmux(&["new-session", "-d", "-s", "two"]).unwrap();
    f.tmux(&["set-environment", "-g", "SSH_AUTH_SOCK", "global-sock"])
        .unwrap();
    f.tmux(&[
        "set-environment",
        "-t",
        "one",
        "SSH_AUTH_SOCK",
        "sock-of-one",
    ])
    .unwrap();
    f.tmux(&["set-environment", "-t", "one", "-r", "STALE_TOKEN"])
        .unwrap();
    f.tmux(&[
        "set-environment",
        "-t",
        "two",
        "SSH_AUTH_SOCK",
        "sock-of-two",
    ])
    .unwrap();
    let out = hook_out(&f, "session-env.txt");
    let record = format!(
        "printf '%s|%s|%s|%s\\n' \"$TAW_EVENT\" \"$SSH_AUTH_SOCK\" \"${{STALE_TOKEN-unset}}\" \
         \"$TAW_SESSION\" >> '{}'",
        out.display()
    );
    write_hooks_config(
        &f,
        &[("tab.focused", &record), ("workspace.closed", &record)],
    );
    let pane = f
        .tmux(&["display-message", "-p", "-t", "=one:", "#{pane_id}"])
        .unwrap();
    let of = |session: &str, what: &str| {
        f.tmux(&["display-message", "-p", "-t", &format!("={session}:"), what])
            .unwrap()
    };
    // The process of the pane `taw` is run from, which has what the session had when the pane was
    // made: an ssh agent that a later client replaced, and a variable the session has since
    // removed.
    let fire = |event: &str, window: Option<&str>, session: &str| {
        let mut args = vec!["plugin", "event", event];
        if let Some(window) = window {
            args.extend(["--tab", window]);
        }
        args.extend(["--workspace", session]);
        let (code, stdout, stderr) = f.run_with_env(
            &args,
            &[
                ("TMUX_PANE", &pane),
                ("SSH_AUTH_SOCK", "sock-of-the-pane"),
                ("STALE_TOKEN", "token-of-the-pane"),
            ],
        );
        assert_eq!(code, 0, "{stdout}{stderr}");
    };
    let lines = |count: usize| {
        assert!(
            eventually(
                || std::fs::read_to_string(&out).is_ok_and(|text| text.lines().count() == count)
            ),
            "{count} hooks were expected to run: {:?}",
            std::fs::read_to_string(&out)
        );
        let mut lines: Vec<String> = std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines.sort();
        lines
    };

    fire(
        "tab.focused",
        Some(&of("one", "#{window_id}")),
        &of("one", "#{session_id}"),
    );
    fire(
        "tab.focused",
        Some(&of("two", "#{window_id}")),
        &of("two", "#{session_id}"),
    );
    assert_eq!(
        lines(2),
        [
            "tab.focused|sock-of-one|unset|one",
            "tab.focused|sock-of-two|token-of-the-pane|two",
        ]
    );

    // The session is gone by the time `workspace.closed` is told: nothing of its own is left to
    // read, and the pane's copy of what only the session removed is all there is.
    let gone = of("two", "#{session_id}");
    f.tmux(&["kill-session", "-t", "=two"]).unwrap();
    fire("workspace.closed", None, &gone);
    assert_eq!(
        lines(3)[2],
        "workspace.closed|global-sock|token-of-the-pane|",
        "the global environment alone, over what the pane's process has"
    );
}

/// `taw notification show` shows a toast, so it runs the notification hook too: a script that
/// reports its own result can reach a phone the way an agent does. The text is all of what it
/// was given, it is about no pane, and `--sound request` is what makes it a request.
#[test]
fn notification_show_runs_the_notification_hook() {
    let f = Fixture::start();
    let out = hook_out(&f, "shown.txt");
    write_hooks_config(
        &f,
        &[(
            "notification",
            &format!(
                "printf '%s|%s|%s|%s\\n' \"$TAW_EVENT\" \"$TAW_KIND\" \"$TAW_MESSAGE\" \"$TAW_PANE\" >> '{}'",
                out.display()
            ),
        )],
    );
    let config = f.config.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    // No pop-up on the test server: the hook is the delivery under test.
    text.push_str("\n[ui.toast]\ndelivery = \"off\"\n");
    std::fs::write(&config, text).unwrap();

    f.ok(
        &[
            "notification",
            "show",
            "Build finished",
            "--body",
            "3 warnings",
            "--sound",
            "done",
        ],
        "cli:notification:show",
        "ok",
    );
    f.ok(
        &["notification", "show", "Deploy stuck", "--sound", "request"],
        "cli:notification:show",
        "ok",
    );

    assert!(
        eventually(|| std::fs::read_to_string(&out).is_ok_and(|text| text.lines().count() == 2)),
        "the hook did not run twice: {:?}",
        std::fs::read_to_string(&out)
    );
    let mut lines: Vec<String> = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "notification|done|Build finished \u{b7} 3 warnings|",
            "notification|request|Deploy stuck|",
        ]
    );
}

/// `taw hooks list` is the one place a person can see every name a hook may take, when it
/// fires, and what is hooked to it: the events, then the two hooks that can refuse, then any
/// name in the config that taw never fires.
#[test]
fn hooks_list_prints_every_event_and_both_vetoes_with_when_they_fire_and_what_is_hooked() {
    let f = Fixture::start();
    std::fs::write(
        f.config.path().join("config.toml"),
        r#"
[hooks]
timeout_seconds = 12
"agent.done" = ["~/bin/log-finished", "say done"]
"notification" = "~/bin/push"
pre_close = "~/bin/keep-agents-open"
"agent.finished" = "true"
"#,
    )
    .unwrap();

    let result = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");

    assert_eq!(result["timeout_seconds"], 12.0, "{result}");
    assert_eq!(result["veto_timeout_seconds"], 10.0, "{result}");
    assert!(
        result["log"].as_str().unwrap().ends_with("logs/hooks.log"),
        "{result}"
    );
    let hooks = result["hooks"].as_array().unwrap();
    let named: Vec<&str> = hooks.iter().map(|h| h["name"].as_str().unwrap()).collect();
    let expected: Vec<&str> = taw::hooks::events::EVENTS
        .iter()
        .chain(taw::hooks::events::VETOES)
        .map(|event| event.name)
        .collect();
    assert_eq!(
        named, expected,
        "every name, in the order the README lists them"
    );
    assert_eq!(
        hooks.len(),
        28,
        "19 lifecycle events, 4 agent states, a container coming and going, the notification and 2 vetoes"
    );
    for hook in hooks {
        assert!(!hook["when"].as_str().unwrap().is_empty(), "{hook}");
        let kind = if hook["name"].as_str().unwrap().starts_with("pre_") {
            "veto"
        } else {
            "event"
        };
        assert_eq!(hook["kind"], kind, "{hook}");
    }
    let by_name = |name: &str| hooks.iter().find(|h| h["name"] == name).unwrap();
    assert_eq!(
        by_name("agent.done")["commands"],
        serde_json::json!(["~/bin/log-finished", "say done"])
    );
    assert_eq!(
        by_name("notification")["commands"],
        serde_json::json!(["~/bin/push"])
    );
    assert_eq!(
        by_name("pre_close")["commands"],
        serde_json::json!(["~/bin/keep-agents-open"])
    );
    assert_eq!(by_name("agent.idle")["commands"], serde_json::json!([]));
    // What taw never fires is said, so a typo is not a hook that quietly never runs.
    assert_eq!(
        result["unknown"],
        serde_json::json!([{"name": "agent.finished", "commands": ["true"]}]),
        "{result}"
    );
    assert!(
        result["variables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "TAW_MESSAGE"),
        "{result}"
    );
}

#[test]
fn hooks_list_with_no_config_lists_every_name_and_nothing_hooked() {
    let f = Fixture::start();

    let result = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");

    assert_eq!(result["timeout_seconds"], 30.0, "{result}");
    assert!(
        result["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["commands"] == serde_json::json!([])),
        "{result}"
    );
    assert_eq!(result["unknown"], serde_json::json!([]), "{result}");
}

/// `taw hooks test <event>` runs the hooks of one event with a realistic sample, waits for
/// them, and says for each what it ran, how it ended and what it wrote to stderr, and the
/// environment it was given. It runs them for real.
#[test]
fn hooks_test_runs_the_hooks_of_one_event_with_sample_values_and_reports_each() {
    let f = Fixture::start();
    let wrote = hook_out(&f, "wrote.txt");
    std::fs::write(
        f.config.path().join("config.toml"),
        format!(
            "[hooks]\n\"agent.done\" = [{}, {}]\n",
            toml::Value::String(format!(
                "printf '%s|%s|%s|%s' \"$TAW_EVENT\" \"$TAW_STATE\" \"$TAW_SESSION\" \"$(cat | tr -d '\\n' | head -c 60)\" > '{0}'; pwd -P > '{0}.pwd'; echo 'the server said ok'; echo 'a warning' >&2",
                wrote.display()
            )),
            toml::Value::String("echo 'it went wrong' >&2; exit 3".to_string()),
        ),
    )
    .unwrap();

    let result = f.ok(
        &["hooks", "test", "agent.done"],
        "cli:hooks:test",
        "hooks_test",
    );

    assert_eq!(result["event"], "agent.done", "{result}");
    assert_eq!(result["kind"], "event", "{result}");
    assert_eq!(result["count"], 2, "{result}");
    let hooks = result["hooks"].as_array().unwrap();
    assert_eq!(hooks[0]["ok"], true, "{result}");
    assert_eq!(hooks[0]["status"], 0, "{result}");
    assert_eq!(hooks[0]["stderr"], "a warning", "{result}");
    // What it printed is shown too: a `curl` that reports an error in its reply, and still
    // exits zero, says so there.
    assert_eq!(hooks[0]["stdout"], "the server said ok", "{result}");
    assert!(
        hooks[0]["command"].as_str().unwrap().starts_with("printf "),
        "{result}"
    );
    assert_eq!(hooks[1]["ok"], false, "{result}");
    assert_eq!(hooks[1]["status"], 3, "{result}");
    assert_eq!(hooks[1]["failure"], "failed (exit status 3)", "{result}");
    assert_eq!(hooks[1]["stderr"], "it went wrong", "{result}");
    // The environment it was given, and the JSON, are the sample's.
    let env = &result["env"];
    assert_eq!(env["TAW_EVENT"], "agent.done", "{result}");
    assert_eq!(env["TAW_STATE"], "done", "{result}");
    assert_eq!(env["TAW_PREVIOUS_STATE"], "working", "{result}");
    assert_eq!(env["TAW_SESSION"], "=taw-hooks-test:", "{result}");
    assert_eq!(env["TAW_AGENT"], "claude", "{result}");
    assert_eq!(env["TAW_TITLE"], "Fix the flaky auth test", "{result}");
    assert_eq!(result["input"]["event"], "agent.done", "{result}");
    // It ran for real, with those values, and the JSON on its standard input.
    let written = std::fs::read_to_string(&wrote).unwrap();
    assert!(
        written.starts_with("agent.done|done|=taw-hooks-test:|{\"event\":\"agent.done\""),
        "{written}"
    );
    // The directory it started in is one taw made for the purpose under the state directory,
    // and took away again: nothing of a test is left behind.
    let dir = hooks[0]["dir"].as_str().unwrap();
    assert!(
        dir.starts_with(f.state.path().to_str().unwrap()),
        "{result}"
    );
    let started_in = std::fs::read_to_string(format!("{}.pwd", wrote.display())).unwrap();
    assert!(
        started_in.trim().ends_with("/taw-hooks-test"),
        "the hook did not start in the sample directory: {started_in}"
    );
    assert!(
        !f.state.path().join("hooks-test").exists(),
        "taw hooks test left {} behind",
        f.state.path().join("hooks-test").display()
    );
}

/// A hook can print a great deal (`curl -v`), and `taw hooks test` shows what a hook printed:
/// the end of each stream, where the verdict is, and not all of it.
#[test]
fn hooks_test_shows_the_end_of_what_a_chatty_hook_printed_and_not_all_of_it() {
    let f = Fixture::start();
    write_hooks_config(
        &f,
        &[(
            "agent.done",
            "yes out | head -c 200000; echo 'all done'; yes err | head -c 200000 >&2; echo 'the verdict' >&2",
        )],
    );

    let result = f.ok(
        &["hooks", "test", "agent.done"],
        "cli:hooks:test",
        "hooks_test",
    );

    let hook = &result["hooks"][0];
    let stdout = hook["stdout"].as_str().unwrap();
    let stderr = hook["stderr"].as_str().unwrap();
    // What is shown says first that it is only the end, so that nobody reads it as the whole.
    for shown in [stdout, stderr] {
        assert!(shown.starts_with(CUT_NOTE_LINE), "{:?}", &shown[..60]);
        assert!(
            shown.len() <= CUT_NOTE_LINE.len() + 16 * 1024,
            "{} bytes",
            shown.len()
        );
    }
    assert!(stdout.ends_with("out\nall done"), "{stdout:?}");
    assert!(stderr.ends_with("err\nthe verdict"), "{stderr:?}");
    assert_eq!(hook["ok"], true, "{result}");
}

/// The line `taw hooks test` puts first when it shows only the end of what a hook printed.
const CUT_NOTE_LINE: &str = "(earlier output cut; the last 16 KiB follow)\n";

/// The note is for a cut and only for one: output that fills the 16 KiB exactly is all there is,
/// and one byte more is cut.
#[test]
fn hooks_test_says_it_cut_a_hooks_output_only_when_it_did() {
    let f = Fixture::start();
    write_hooks_config(
        &f,
        &[(
            "agent.done",
            "head -c 16384 /dev/zero | tr '\\0' o; head -c 16385 /dev/zero | tr '\\0' e >&2",
        )],
    );

    let result = f.ok(
        &["hooks", "test", "agent.done"],
        "cli:hooks:test",
        "hooks_test",
    );

    let hook = &result["hooks"][0];
    let stdout = hook["stdout"].as_str().unwrap();
    let stderr = hook["stderr"].as_str().unwrap();
    assert_eq!(stdout.len(), 16384, "all of it, and no note");
    assert!(!stdout.contains("cut"), "{:?}", &stdout[..60]);
    assert!(
        stderr.starts_with(CUT_NOTE_LINE),
        "one byte too many is cut"
    );
    assert_eq!(stderr.len(), CUT_NOTE_LINE.len() + 16384);
}

#[test]
fn hooks_test_of_the_notification_says_it_is_a_request_with_a_toasts_text() {
    let f = Fixture::start();
    write_hooks_config(&f, &[("notification", "true")]);

    let result = f.ok(
        &["hooks", "test", "notification"],
        "cli:hooks:test",
        "hooks_test",
    );

    assert_eq!(result["env"]["TAW_KIND"], "request", "{result}");
    assert_eq!(
        result["env"]["TAW_MESSAGE"], "claude blocked \u{b7} taw-hooks-test:2 \u{b7} needs input",
        "{result}"
    );
}

/// `--machine studio` sends the notification sample as a toast about a machine of `[machines]`: the hook is told
/// `TAW_MACHINE`, and the JSON says `machine`. Without it the variable is empty and the JSON has none.
#[test]
fn hooks_test_of_the_notification_can_be_about_a_machine_of_machines() {
    let f = Fixture::start();
    std::fs::write(
        f.config.path().join("config.toml"),
        "[machines]\nstudio = \"studio\"\n\n[hooks]\nnotification = \"true\"\n",
    )
    .unwrap();

    let own = f.ok(
        &["hooks", "test", "notification"],
        "cli:hooks:test",
        "hooks_test",
    );
    assert_eq!(own["env"]["TAW_MACHINE"], "", "{own}");
    assert_eq!(own["env"]["TAW_KIND"], "request", "{own}");
    assert!(own["input"].get("machine").is_none(), "{own}");

    let about = f.ok(
        &["hooks", "test", "notification", "--machine", "studio"],
        "cli:hooks:test",
        "hooks_test",
    );
    assert_eq!(about["env"]["TAW_MACHINE"], "studio", "{about}");
    assert_eq!(about["env"]["TAW_KIND"], "request", "{about}");
    assert_eq!(about["input"]["machine"], "studio", "{about}");
}

/// A name that is not under `[machines]` is refused in the words `taw --machine` uses, before any hook runs; so
/// is any event but the notification, and `--pane` (a real pane is this machine's).
#[test]
fn hooks_test_machine_refuses_a_name_taw_does_not_know_and_runs_no_hook() {
    let f = Fixture::start();
    let ran = hook_out(&f, "ran.txt");
    std::fs::write(
        f.config.path().join("config.toml"),
        format!(
            "[machines]\nstudio = \"studio\"\n\n[hooks]\nnotification = {}\n",
            toml::Value::String(format!("touch '{}'", ran.display()))
        ),
    )
    .unwrap();
    // The positive half: a name it knows runs the hook.
    f.ok(
        &["hooks", "test", "notification", "--machine", "studio"],
        "cli:hooks:test",
        "hooks_test",
    );
    assert!(ran.exists(), "the hook did not run for a known machine");
    std::fs::remove_file(&ran).unwrap();

    let (code, stdout, stderr) = f.run_raw(&["hooks", "test", "notification", "--machine", "pi"]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "taw: no machine called pi in [machines] (yours: studio)\n"
    );
    assert!(!ran.exists(), "the hook ran though the machine is unknown");

    let (code, _, stderr) = f.run_raw(&["hooks", "test", "agent.done", "--machine", "studio"]);
    assert_ne!(code, 0, "{stderr}");
    let (code, _, stderr) = f.run_raw(&[
        "hooks",
        "test",
        "notification",
        "--machine",
        "studio",
        "--pane",
        "%0",
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(!ran.exists());
}

/// A hook that can refuse is answered when its shell ends, and `taw hooks test` reads it the
/// same way: one that leaves a process holding its output is shown as agreeing at once, as a
/// real close would find it, and not as timed out after the whole limit.
#[test]
fn hooks_test_of_a_veto_that_leaves_a_process_behind_answers_as_a_real_close_does() {
    let f = Fixture::start();
    let pid_file = hook_out(&f, "holder.pid");
    // The limit a veto has is three seconds here, so a test that waited for the process
    // would be told it timed out.
    std::fs::write(
        f.config.path().join("config.toml"),
        format!(
            "[sessions]\nhook_timeout_seconds = 3\n\n[hooks]\npre_close = {}\n",
            toml::Value::String(format!("sleep 30 & echo $! > '{}'", pid_file.display()))
        ),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let result = f.ok(
        &["hooks", "test", "pre_close"],
        "cli:hooks:test",
        "hooks_test",
    );
    let took = started.elapsed();

    let pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
    let _ = Command::new("kill")
        .args(["-KILL", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status();
    assert_eq!(result["hooks"][0]["ok"], true, "{result}");
    assert_eq!(result["hooks"][0]["timed_out"], false, "{result}");
    assert!(took < std::time::Duration::from_secs(2), "waited {took:?}");
}

/// `taw hooks test` runs hooks for real, and a hook may act on the ids it is given
/// (`tmux send-keys -t "$TAW_PANE"`, which the README shows). The ids of the sample are ones no
/// server has, so such a hook fails in a test and never types into somebody's pane; `TAW_TEST`
/// says it is a test, and the result says both.
#[test]
fn hooks_test_uses_ids_no_server_has_and_says_so() {
    let f = Fixture::start();
    let socket = f.socket.clone();
    write_hooks_config(
        &f,
        &[(
            "agent.done",
            // `-L` names the throwaway server, so nothing here can reach another one.
            &format!("tmux -L {socket} send-keys -t \"$TAW_PANE\" 'echo pwned' Enter"),
        )],
    );

    let result = f.ok(
        &["hooks", "test", "agent.done"],
        "cli:hooks:test",
        "hooks_test",
    );

    let env = &result["env"];
    // (`display-message -t` succeeds for a target that is not there, so these ask a command
    // that fails for one.)
    for (name, id, command) in [
        ("TAW_PANE", "%999999", "capture-pane"),
        ("TAW_WINDOW", "@999999", "list-panes"),
        ("TAW_SESSION_ID", "$999999", "has-session"),
    ] {
        assert_eq!(env[name], id, "{result}");
        assert!(
            f.tmux(&[command, "-t", id]).is_err(),
            "the server has {id}, which the sample names"
        );
    }
    assert_eq!(env["TAW_TEST"], "1", "{result}");
    assert!(
        result.get("real_pane").is_some_and(Value::is_null),
        "no real pane, said so: {result}"
    );
    // The hook that acts on the pane found none, and nothing was typed anywhere.
    let hook = &result["hooks"][0];
    assert_eq!(hook["ok"], false, "{result}");
    assert!(
        hook["stderr"].as_str().unwrap().contains("can't find pane"),
        "{result}"
    );
    assert!(
        !f.tmux(&["capture-pane", "-p", "-t", "one"])
            .unwrap()
            .contains("pwned")
    );
    let note = result["sample"].as_str().unwrap();
    assert!(
        note.contains("name nothing on any tmux server")
            && note.contains("[ -n \"$TAW_TEST\" ] && exit 0"),
        "{result}"
    );
}

/// A hook that acts on the ids it is given reaches nothing in a test, whichever event is tested
/// and not only the ones that name a pane. A real `workspace.renamed` has no pane and its
/// `TAW_PANE` is empty, and an empty tmux target is the current pane, window or session: in a test
/// that was a live one, typed into and renamed. Every event of the table is tried, with a hook that
/// types into `$TAW_PANE`, renames `$TAW_WINDOW` and renames `$TAW_SESSION_ID`, on a server that has
/// a live pane, window and session, and none of them changes.
#[test]
fn hooks_test_reaches_no_live_pane_window_or_session_whichever_event_is_tested() {
    let f = Fixture::start();
    let socket = f.socket.clone();
    f.tmux(&["rename-window", "-t", "one:0", "live-window"])
        .unwrap();
    let pane = f
        .tmux(&["list-panes", "-t", "one:live-window", "-F", "#{pane_id}"])
        .unwrap();
    let listed = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");
    let names: Vec<&str> = listed["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["name"].as_str().unwrap())
        .collect();
    assert!(names.len() >= 22, "{names:?}");
    // `-L` names the throwaway server, so nothing here can reach another one. Each command is
    // one that acts on what it is given, and each is run whatever the one before it did.
    let command = format!(
        "tmux -L {socket} send-keys -t \"$TAW_PANE\" 'typed-by-a-hook' Enter; \
         tmux -L {socket} rename-window -t \"$TAW_WINDOW\" renamed-by-a-hook; \
         tmux -L {socket} rename-session -t \"$TAW_SESSION_ID\" renamed-by-a-hook"
    );
    let hooks: Vec<(&str, &str)> = names.iter().map(|name| (*name, command.as_str())).collect();
    write_hooks_config(&f, &hooks);

    for name in &names {
        let result = f.ok(&["hooks", "test", name], "cli:hooks:test", "hooks_test");

        let now = f
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{window_name}|#{session_name}",
            ])
            .unwrap();
        assert_eq!(
            now, "live-window|one",
            "{name}: a hook renamed a live window or session"
        );
        // The hook ran, and each of its three commands found nothing where it was sent: the
        // proof that it was aimed at something, and that what it was aimed at is not there.
        let stderr = result["hooks"][0]["stderr"].as_str().unwrap();
        for missing in ["can't find pane", "can't find window", "can't find session"] {
            assert!(
                stderr.contains(missing),
                "{name}: {missing:?} not in {stderr:?}"
            );
        }
    }

    // Whatever was typed has been read by the shell once it has read what is typed after it.
    f.tmux(&["send-keys", "-t", &pane, "echo end-of-the-test", "Enter"])
        .unwrap();
    let done = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(25));
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .unwrap()
            .lines()
            .any(|line| line.trim() == "end-of-the-test")
    });
    assert!(done, "the shell never answered");
    assert!(
        !f.tmux(&["capture-pane", "-p", "-t", &pane])
            .unwrap()
            .contains("typed-by-a-hook"),
        "a hook typed into a live pane"
    );
}

/// tmux reads a session name in a target as the start of a name when no session has exactly it:
/// `tmux kill-session -t taw-hooks-test` ends `taw-hooks-testing` if that is the session there is.
/// A test names its session `=taw-hooks-test:` (and `=taw-hooks-test--feat-x:` for the worktree
/// events) in `TAW_SESSION`. The `=` is tmux's mark for exactly that name, and the `:` after it says
/// the name is a session's: a command that takes a window reads a bare name as a window's first and
/// then as the start of a session's, so with the `=` alone `new-window`, `kill-window`,
/// `rename-window`, `respawn-window -k` and `send-keys -t "$TAW_SESSION.0"` still reached a live
/// `taw-hooks-testing`. A hook that does any of those, or renames, kills or types into the session,
/// finds no session, on a server that has a live one whose name begins the same way. Every event of
/// the table is tried against each of two live sessions: one that begins like the story's session
/// and one that begins like its worktree's.
#[test]
fn hooks_test_reaches_no_session_whose_name_only_begins_like_the_samples() {
    // What a hook may aim at a session by its name, as a session, as a window and as a pane. Each
    // is run whatever the one before it did.
    const AIMED_AT_THE_SESSION: [&str; 8] = [
        "send-keys -t \"$TAW_SESSION\" 'typed-by-a-hook' Enter",
        "send-keys -t \"$TAW_SESSION.0\" 'typed-by-a-hook' Enter",
        "rename-session -t \"$TAW_SESSION\" renamed-by-a-hook",
        "kill-session -t \"$TAW_SESSION\"",
        "new-window -d -t \"$TAW_SESSION\"",
        "rename-window -t \"$TAW_SESSION\" renamed-by-a-hook",
        "respawn-window -k -t \"$TAW_SESSION\"",
        "kill-window -t \"$TAW_SESSION\"",
    ];
    let f = Fixture::start();
    let socket = f.socket.clone();
    let listed = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");
    let names: Vec<&str> = listed["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["name"].as_str().unwrap())
        .collect();
    assert!(names.len() >= 22, "{names:?}");
    // `-L` names the throwaway server, so nothing here can reach another one.
    let command = AIMED_AT_THE_SESSION
        .iter()
        .map(|aimed| format!("tmux -L {socket} {aimed}"))
        .collect::<Vec<_>>()
        .join("; ");
    let hooks: Vec<(&str, &str)> = names.iter().map(|name| (*name, command.as_str())).collect();
    write_hooks_config(&f, &hooks);

    // Two at a time would each be the other's prefix and match neither, so one live session
    // at a time: the story's first, and then the worktree's.
    for live in ["taw-hooks-testing", "taw-hooks-test--feat-xylophone"] {
        f.tmux(&["new-session", "-d", "-s", live, "-n", "live-window"])
            .unwrap();
        let pane = f
            .tmux(&["list-panes", "-t", &format!("={live}:"), "-F", "#{pane_id}"])
            .unwrap();
        for name in &names {
            let result = f.ok(&["hooks", "test", name], "cli:hooks:test", "hooks_test");

            // The live session is as it was: still there under its own name, with the one
            // window it had under the name it had.
            let sessions = f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap();
            let windows = f
                .tmux(&[
                    "list-windows",
                    "-t",
                    &format!("={live}:"),
                    "-F",
                    "#{window_name}",
                ])
                .unwrap_or_else(|gone| format!("(no such session: {gone})"));
            assert!(
                sessions.lines().any(|session| session == live) && windows == "live-window",
                "{name}: a hook ended, renamed or opened a window in {live}: sessions {sessions:?}, \
                 windows {windows:?}"
            );
            // The hook ran, and each of its commands found no session of that exact name: the
            // proof that it was aimed at one, and that the one it was aimed at is not there.
            let stderr = result["hooks"][0]["stderr"].as_str().unwrap();
            assert_eq!(
                stderr.matches("can't find session").count(),
                AIMED_AT_THE_SESSION.len(),
                "{name} with {live}: {stderr:?}"
            );
        }
        // Whatever was typed has been read by the shell once it has read what is typed after it.
        f.tmux(&["send-keys", "-t", &pane, "echo end-of-the-test", "Enter"])
            .unwrap();
        let done = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(25));
            f.tmux(&["capture-pane", "-p", "-t", &pane])
                .unwrap()
                .lines()
                .any(|line| line.trim() == "end-of-the-test")
        });
        assert!(done, "the shell of {live} never answered");
        assert!(
            !f.tmux(&["capture-pane", "-p", "-t", &pane])
                .unwrap()
                .contains("typed-by-a-hook"),
            "a hook typed into {live}"
        );
        f.tmux(&["kill-session", "-t", &format!("={live}")])
            .unwrap();
    }
}

/// `taw hooks test <event> --pane <pane>` runs the hook against a real pane: its ids, its
/// session, its window and its directory, with `TAW_TEST` still `1`, and the output says a real
/// pane was used. The hook here acts on `$TAW_PANE` and it really does: what it types shows up
/// in the pane. (A private server, so the pane is one of this test's own.)
#[test]
fn hooks_test_with_a_pane_runs_the_hook_on_that_real_pane_and_says_so() {
    let f = Fixture::start();
    let socket = f.socket.clone();
    // A window of its own in a directory of its own, so that what the hook is told is this
    // pane's and not the first one's.
    let dir = std::fs::canonicalize(tempfile::tempdir().unwrap().keep()).unwrap();
    f.tmux(&[
        "new-window",
        "-d",
        "-t",
        "one",
        "-n",
        "editor",
        "-c",
        dir.to_str().unwrap(),
    ])
    .unwrap();
    let pane = f
        .tmux(&["list-panes", "-t", "one:editor", "-F", "#{pane_id}"])
        .unwrap();
    let window = f
        .tmux(&["display-message", "-p", "-t", &pane, "#{window_id}"])
        .unwrap();
    let session = f
        .tmux(&["display-message", "-p", "-t", &pane, "#{session_id}"])
        .unwrap();
    let told = hook_out(&f, "told.txt");
    write_hooks_config(
        &f,
        &[(
            "agent.done",
            &format!(
                "tmux -L {socket} send-keys -t \"$TAW_PANE\" 'echo typed-by-the-hook' Enter; \
                 echo \"$TAW_PANE|$TAW_WINDOW|$TAW_SESSION_ID|$TAW_SESSION|$TAW_DIR|$TAW_TEST\" > '{}'",
                told.display()
            ),
        )],
    );

    let result = f.ok(
        &["hooks", "test", "agent.done", "--pane", &pane],
        "cli:hooks:test",
        "hooks_test",
    );

    let env = &result["env"];
    assert_eq!(env["TAW_PANE"], pane.as_str(), "{result}");
    assert_eq!(env["TAW_WINDOW"], window.as_str(), "{result}");
    assert_eq!(env["TAW_SESSION_ID"], session.as_str(), "{result}");
    assert_eq!(env["TAW_SESSION"], "one", "{result}");
    assert_eq!(env["TAW_DIR"], dir.to_str().unwrap(), "{result}");
    assert_eq!(env["TAW_TEST"], "1", "{result}");
    // Says so, in the field and in the words.
    assert_eq!(result["real_pane"], pane.as_str(), "{result}");
    let note = result["sample"].as_str().unwrap();
    assert!(
        note.contains(&format!("the real pane {pane}")) && note.contains("acts on that pane"),
        "{result}"
    );
    // The hook ran on that pane: what it typed is there.
    assert_eq!(result["hooks"][0]["ok"], true, "{result}");
    let typed = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .unwrap()
            .contains("typed-by-the-hook")
    });
    assert!(typed, "the hook never typed into {pane}");
    let told = std::fs::read_to_string(&told).unwrap();
    assert_eq!(
        told.trim(),
        format!("{pane}|{window}|{session}|one|{}|1", dir.display())
    );
    // The event is still the sample's: nothing else of the story is the pane's.
    assert_eq!(env["TAW_STATE"], "done", "{result}");
    assert_eq!(env["TAW_AGENT"], "claude", "{result}");
}

/// Every name a hook can take, the two that can refuse included, is told about the pane it is
/// given: none is left with the sample's ids, and none has no sample at all.
#[test]
fn hooks_test_with_a_pane_tells_every_event_about_that_pane() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    let session = f.tmux(&["display-message", "-p", "#{session_id}"]).unwrap();
    let listed = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");
    let names: Vec<&str> = listed["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["name"].as_str().unwrap())
        .collect();
    assert!(names.len() >= 22, "{names:?}");

    let going = going_away();
    for name in names {
        // An event about something going away is refused with a pane unless it is forced.
        let mut args = vec!["hooks", "test", name, "--pane", &pane];
        if going.contains(&name) {
            args.push("--force");
        }
        let result = f.ok(&args, "cli:hooks:test", "hooks_test");

        assert_eq!(result["real_pane"], pane.as_str(), "{name}");
        assert_eq!(result["env"]["TAW_SESSION"], "one", "{name}");
        assert_eq!(result["env"]["TAW_SESSION_ID"], session.as_str(), "{name}");
        assert_eq!(result["env"]["TAW_TEST"], "1", "{name}");
        assert!(
            !result["input"].to_string().contains("999999"),
            "{name}: an invented id in {}",
            result["input"]
        );
    }
}

/// The events about something going away, a close, a removal or a process ending, as the table
/// says of them (the words `taw hooks list` prints): not a list kept here, so that an event added
/// to the table with such words is one this finds.
fn going_away() -> Vec<&'static str> {
    use taw::hooks::events::{EVENTS, VETOES};
    EVENTS
        .iter()
        .chain(VETOES)
        .filter(|event| {
            event.when.contains("clos")
                || event.when.contains("remov")
                || event.when.contains(" ends")
        })
        .map(|event| event.name)
        .collect()
}

/// `taw hooks test <event> --pane <pane>` runs the person's real hook on a real pane, so for an
/// event about something going away (a hook that closes or removes what it is told of) it refuses
/// unless it is given `--force`: on stderr, as a usage error is, with status 2, and nothing runs.
/// With `--force` it runs.
#[test]
fn hooks_test_with_a_pane_refuses_an_event_about_something_going_away_unless_forced() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    let going = going_away();
    assert!(
        going.len() >= 7
            && going.contains(&"worktree.removed")
            && going.contains(&"pre_close")
            && going.contains(&"pane.exited"),
        "{going:?}"
    );
    let ran = hook_out(&f, "ran.txt");

    for name in going {
        write_hooks_config(&f, &[(name, &format!("touch '{}'", ran.display()))]);

        let (code, stdout, stderr) = f.run_raw(&["hooks", "test", name, "--pane", &pane]);

        assert_eq!(code, 2, "{name}: {stdout}{stderr}");
        assert_eq!(stdout, "", "{name}: nothing on stdout");
        assert_eq!(
            stderr,
            format!(
                "taw hooks test: {name} with --pane runs your hook on the real session and directory of {pane}; add --force if that is what you want\n"
            ),
            "{name}"
        );
        assert!(!ran.exists(), "{name}: the hook ran though it was refused");

        let result = f.ok(
            &["hooks", "test", name, "--pane", &pane, "--force"],
            "cli:hooks:test",
            "hooks_test",
        );
        assert_eq!(result["real_pane"], pane.as_str(), "{name}");
        assert!(ran.exists(), "{name}: forced, the hook did not run");
        std::fs::remove_file(&ran).unwrap();
    }
}

/// The reason `pane.exited` is one of them: the hook a person hooks to it is the one that brings
/// the pane back (`respawn-pane -k`), and tried on a pane that is alive it would kill what is
/// running there. Refused, the process is the same one; forced, the pane is respawned.
#[test]
fn hooks_test_of_pane_exited_on_a_live_pane_kills_its_process_only_when_forced() {
    let f = Fixture::start();
    let socket = f.socket.clone();
    f.tmux(&["new-window", "-d", "-t", "one", "-n", "victim", "sleep 300"])
        .unwrap();
    let pane = f
        .tmux(&["list-panes", "-t", "one:victim", "-F", "#{pane_id}"])
        .unwrap();
    let pid_of = || {
        f.tmux(&["display-message", "-p", "-t", &pane, "#{pane_pid}"])
            .unwrap()
    };
    let alive = |pid: &str| {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    let before = pid_of();
    assert!(alive(&before));
    write_hooks_config(
        &f,
        &[(
            "pane.exited",
            &format!("tmux -L {socket} respawn-pane -k -t \"$TAW_PANE\""),
        )],
    );

    let (code, _stdout, stderr) = f.run_raw(&["hooks", "test", "pane.exited", "--pane", &pane]);

    assert_eq!(code, 2, "{stderr}");
    assert_eq!(pid_of(), before, "the pane's process was replaced");
    assert!(alive(&before), "the pane's process was killed");

    f.ok(
        &["hooks", "test", "pane.exited", "--pane", &pane, "--force"],
        "cli:hooks:test",
        "hooks_test",
    );
    let respawned = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        pid_of() != before
    });
    assert!(respawned, "forced, the hook never respawned the pane");
}

/// Everything else is as it was: an event that is not about something going away takes `--pane`
/// with no `--force`, and every event takes the sample, the ones going away included, with none.
#[test]
fn hooks_test_needs_no_force_for_the_sample_or_for_an_event_that_is_not_going_away() {
    let f = Fixture::start();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();
    let listed = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");
    let names: Vec<&str> = listed["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["name"].as_str().unwrap())
        .collect();
    let going = going_away();

    for name in names {
        let sample = f.ok(&["hooks", "test", name], "cli:hooks:test", "hooks_test");
        assert!(sample["real_pane"].is_null(), "{name}");
        if !going.contains(&name) {
            let told = f.ok(
                &["hooks", "test", name, "--pane", &pane],
                "cli:hooks:test",
                "hooks_test",
            );
            assert_eq!(told["real_pane"], pane.as_str(), "{name}");
        }
    }
}

/// The refusal is about what was asked, so it comes before anything is looked up: a pane that is
/// not there is refused the same, and only a forced test finds it is not there. And `--force` is
/// for `--pane` alone.
#[test]
fn the_refusal_comes_first_and_force_belongs_to_a_pane() {
    let f = Fixture::start();
    let ran = hook_out(&f, "ran.txt");
    write_hooks_config(&f, &[("pre_close", &format!("touch '{}'", ran.display()))]);

    let (code, _stdout, stderr) = f.run_raw(&["hooks", "test", "pre_close", "--pane", "%99999"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("add --force if that is what you want"),
        "{stderr}"
    );

    let (code, env) = f.run(&["hooks", "test", "pre_close", "--pane", "%99999", "--force"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");

    let (code, _stdout, stderr) = f.run_raw(&["hooks", "test", "pre_close", "--force"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--pane"), "{stderr}");
    assert!(!ran.exists(), "a hook ran");
}

/// A pane is named the way every other verb takes one, and a pane that is not there is a clean
/// `not_found` before any hook runs.
#[test]
fn hooks_test_with_a_pane_takes_a_public_id_and_refuses_one_that_is_not_there() {
    let f = Fixture::start();
    let public = f.first_pane();
    let terminal = f.terminal_id(&public);
    let ran = hook_out(&f, "ran.txt");
    write_hooks_config(&f, &[("agent.done", &format!("touch '{}'", ran.display()))]);

    let result = f.ok(
        &["hooks", "test", "agent.done", "--pane", &public],
        "cli:hooks:test",
        "hooks_test",
    );
    assert_eq!(result["real_pane"], terminal.as_str(), "{result}");
    assert!(ran.exists());
    std::fs::remove_file(&ran).unwrap();

    let (code, env) = f.run(&["hooks", "test", "agent.done", "--pane", "%99999"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(!ran.exists(), "a hook ran for a pane that is not there");
}

/// `taw hooks list` says the same about the sample, since that is where a person looks first,
/// and lists `TAW_TEST` with the other variables.
#[test]
fn hooks_list_says_what_a_test_is_run_against_and_lists_taw_test() {
    let f = Fixture::start();

    let result = f.ok(&["hooks", "list"], "cli:hooks:list", "hooks_list");

    assert!(
        result["variables"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("TAW_TEST")),
        "{result}"
    );
    let note = result["sample"].as_str().unwrap();
    assert!(
        note.contains("taw hooks test") && note.contains("[ -n \"$TAW_TEST\" ] && exit 0"),
        "{result}"
    );
}

#[test]
fn hooks_test_of_an_event_with_no_hooks_says_so_and_runs_nothing() {
    let f = Fixture::start();

    let result = f.ok(
        &["hooks", "test", "worktree.created"],
        "cli:hooks:test",
        "hooks_test",
    );

    assert_eq!(result["count"], 0, "{result}");
    assert_eq!(result["hooks"], serde_json::json!([]), "{result}");
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("no hooks are configured for worktree.created"),
        "{result}"
    );
    // It still shows what a hook would be given.
    assert_eq!(result["env"]["TAW_EVENT"], "worktree.created", "{result}");
}

#[test]
fn hooks_test_of_a_name_that_is_not_an_event_is_an_invalid_argument() {
    let f = Fixture::start();

    let (code, env) = f.run(&["hooks", "test", "agent.finished"]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("taw hooks list"),
        "{env}"
    );
}

#[test]
fn hooks_test_stops_a_hook_at_its_time_limit_and_says_it_did() {
    let f = Fixture::start();
    std::fs::write(
        f.config.path().join("config.toml"),
        "[hooks]\ntimeout_seconds = 0.5\n\"agent.done\" = \"sleep 30\"\n",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let result = f.ok(
        &["hooks", "test", "agent.done"],
        "cli:hooks:test",
        "hooks_test",
    );

    let hook = &result["hooks"][0];
    assert_eq!(hook["ok"], false, "{result}");
    assert_eq!(hook["timed_out"], true, "{result}");
    assert_eq!(hook["failure"], "timed out after 0.5 s", "{result}");
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
}

/// A hook that can refuse is tried with the veto's own limit, and a refusal is reported as
/// one, with what the hook said.
#[test]
fn hooks_test_of_a_veto_reports_whether_it_would_refuse() {
    let f = Fixture::start();
    write_hooks_config(&f, &[("pre_close", REFUSE_CLOSE)]);

    let result = f.ok(
        &["hooks", "test", "pre_close"],
        "cli:hooks:test",
        "hooks_test",
    );

    assert_eq!(result["kind"], "veto", "{result}");
    assert_eq!(result["hooks"][0]["ok"], false, "{result}");
    assert_eq!(
        result["hooks"][0]["stderr"], "agent is still working",
        "{result}"
    );
    assert_eq!(result["env"]["TAW_TARGET"], "pane", "{result}");
}

/// What `taw hooks test` runs a hook with is what an event runs it with: tmux's global
/// environment, where the credentials of a push are kept, and `TMUX` for the server, so that a
/// test that passes is a hook that will send. What it prints of the environment is the `TAW_*`
/// variables and nothing else, since the rest may hold secrets.
#[test]
fn hooks_test_gives_the_hook_tmuxs_global_environment_and_the_server_it_is_run_for() {
    let f = Fixture::start();
    f.tmux(&[
        "set-environment",
        "-g",
        "NTFY_TOPIC",
        "taw-hooks-test-topic",
    ])
    .unwrap();
    write_hooks_config(
        &f,
        &[("notification", "echo \"$NTFY_TOPIC\"; echo \"$TMUX\" >&2")],
    );
    let socket_path = f
        .tmux(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    let pane = f.tmux(&["display-message", "-p", "#{pane_id}"]).unwrap();

    let (code, env) = f.run_in_pane(&pane, &["hooks", "test", "notification"]);

    assert_eq!(code, 0, "{env}");
    let result = &env["result"];
    let hook = &result["hooks"][0];
    assert_eq!(hook["stdout"], "taw-hooks-test-topic", "{result}");
    assert!(
        hook["stderr"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{socket_path},")),
        "TMUX is not this server's: {result}"
    );
    for name in result["env"].as_object().unwrap().keys() {
        assert!(
            name.starts_with("TAW_"),
            "{name} is printed, and tmux's environment may hold secrets"
        );
    }
}

/// `taw hooks log` prints the end of the hooks log, where a hook that failed in the background
/// left its line.
#[test]
fn hooks_log_prints_the_tail_of_the_log_and_is_empty_before_anything_failed() {
    let f = Fixture::start();
    let before = f.ok(&["hooks", "log"], "cli:hooks:log", "hooks_log");
    assert_eq!(before["lines"], serde_json::json!([]), "{before}");
    assert!(
        before["path"].as_str().unwrap().ends_with("logs/hooks.log"),
        "{before}"
    );

    write_hooks_config(
        &f,
        &[("tab.focused", "echo 'no route to host' >&2; exit 7")],
    );
    let window = f.tmux(&["display-message", "-p", "#{window_id}"]).unwrap();
    let session = f.tmux(&["display-message", "-p", "#{session_id}"]).unwrap();
    for _ in 0..3 {
        f.ok(
            &[
                "plugin",
                "event",
                "tab.focused",
                "--tab",
                &window,
                "--workspace",
                &session,
            ],
            "cli:plugin:event",
            "plugin_event",
        );
    }

    assert!(
        eventually(|| {
            let log = f.ok(&["hooks", "log"], "cli:hooks:log", "hooks_log");
            log["lines"].as_array().unwrap().len() == 3
        }),
        "the failures were never logged"
    );
    let log = f.ok(&["hooks", "log"], "cli:hooks:log", "hooks_log");
    let last = log["lines"][2].as_str().unwrap();
    assert!(
        last.contains(" tab.focused failed (exit status 7): echo 'no route to host'"),
        "{last}"
    );
    assert!(last.ends_with(" | no route to host"), "{last}");
    let two = f.ok(
        &["hooks", "log", "--lines", "2"],
        "cli:hooks:log",
        "hooks_log",
    );
    assert_eq!(two["lines"].as_array().unwrap().len(), 2, "{two}");
}

/// `taw config check` warns about an event name taw does not fire.
#[test]
fn config_check_warns_about_a_hook_event_taw_does_not_fire() {
    let f = Fixture::start();
    write_hooks_config(&f, &[("agent.done", "true"), ("agent.finished", "true")]);

    let checked = f.ok(&["config", "check"], "cli:config:check", "config_check");

    let diagnostics = checked["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{checked}");
    assert_eq!(diagnostics[0]["level"], "warning", "{checked}");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("unknown hook event: hooks.agent.finished"),
        "{checked}"
    );
}

/// A row token's `rules` (herdr v0.9.1): `taw config check` calls a malformed one an error and
/// says what is wrong with it, and finds nothing to say about a well-formed one, `hide` and
/// all, in both `[ui.sidebar]` tables.
#[test]
fn config_check_reports_a_malformed_row_token_rule_and_passes_a_good_one() {
    let f = Fixture::start();
    let config = f.config.path().join("config.toml");

    std::fs::write(
        &config,
        r##"[ui.sidebar.agents]
rows = [["state_icon", { token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true }, { gt = 50, fg = "#fc0" }] }], [{ token = "$machine", rules = [{ equals = "Local", hide = true }] }]]
[ui.sidebar.spaces]
rows = [["workspace", { token = "branch", rules = [{ starts_with = "main", ignore_case = true, dim = true }] }]]
"##,
    )
    .unwrap();
    let checked = f.ok(&["config", "check"], "cli:config:check", "config_check");
    assert!(
        checked["diagnostics"].as_array().unwrap().is_empty(),
        "well-formed rules are clean: {checked}"
    );

    for (rule, said) in [
        (
            r#"{ equals = "a", contains = "b" }"#,
            "exactly one of equals, contains, starts_with, gt, lt",
        ),
        (r#"{ regex = "a" }"#, "unknown field `regex`"),
        (
            r#"{ gt = 5, ignore_case = true }"#,
            "ignore_case applies only to sidebar text conditions",
        ),
    ] {
        std::fs::write(
            &config,
            format!("[ui.sidebar.agents]\nrows = [[{{ token = \"$load\", rules = [{rule}] }}]]\n"),
        )
        .unwrap();
        let (checked, _) = f.config_check(1);
        let diagnostics = checked["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{rule}: {checked}");
        assert_eq!(diagnostics[0]["level"], "error", "{rule}: {checked}");
        assert!(
            diagnostics[0]["message"].as_str().unwrap().contains(said),
            "{rule}: {checked}"
        );
    }
}

/// A key a row token does not know, `hide` outside a rule or `rule` for `rules`, is a warning
/// naming it from `taw config check`, and is no error: the config still loads.
#[test]
fn config_check_warns_about_a_key_a_row_token_does_not_know() {
    let f = Fixture::start();
    let config = f.config.path().join("config.toml");
    std::fs::write(
        &config,
        "[ui.sidebar.agents]\nrows = [[{ token = \"$machine\", hide = true }, { token = \"$load\", rule = [{ gt = 80 }] }]]\n",
    )
    .unwrap();

    let checked = f.ok(&["config", "check"], "cli:config:check", "config_check");

    let diagnostics = checked["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2, "{checked}");
    for (diagnostic, key) in diagnostics.iter().zip(["hide", "rule"]) {
        assert_eq!(diagnostic["level"], "warning", "{checked}");
        assert!(
            diagnostic["message"].as_str().unwrap().contains(&format!(
                "unknown config key: ui.sidebar.agents.rows[0][{}].{key}",
                if key == "hide" { 0 } else { 1 }
            )),
            "{checked}"
        );
    }
}

/// A refusing `pre_close` hook: it says why on stderr, so the refusal can quote it.
const REFUSE_CLOSE: &str = "echo 'agent is still working' >&2; exit 1";

/// `taw workspace close` asks `[hooks] pre_close` first. A refusal is the error code
/// `hook_refused`, its message names the hook and quotes what it said, and nothing is closed.
#[test]
fn workspace_close_is_refused_by_a_pre_close_hook_and_nothing_is_closed() {
    let f = Fixture::start();
    f.tmux(&["new-session", "-d", "-s", "two"]).unwrap();
    write_hooks_config(&f, &[("pre_close", REFUSE_CLOSE)]);

    let (code, env) = f.run(&["workspace", "close", "two"]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("pre-close hook") && message.contains("agent is still working"),
        "{env}"
    );
    assert_eq!(session_names(&f), ["one", "two"], "nothing was closed");

    // The same close, once the hook agrees.
    write_hooks_config(&f, &[("pre_close", "exit 0")]);
    f.ok(&["workspace", "close", "two"], "cli:workspace:close", "ok");
    assert_eq!(session_names(&f), ["one"]);
}

#[test]
fn tab_close_is_refused_by_a_pre_close_hook_and_nothing_is_closed() {
    let f = Fixture::start();
    let created = f.ok(
        &["tab", "create", "--workspace", "one", "--no-focus"],
        "cli:tab:create",
        "tab_created",
    );
    let tab = created["tab"]["tab_id"].as_str().unwrap().to_string();
    let windows = |f: &Fixture| {
        f.tmux(&["list-windows", "-t", "one"])
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(windows(&f), 2);
    write_hooks_config(&f, &[("pre_close", REFUSE_CLOSE)]);

    let (code, env) = f.run(&["tab", "close", &tab]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    assert_eq!(windows(&f), 2, "the window is still there");

    write_hooks_config(&f, &[("pre_close", "exit 0")]);
    f.ok(&["tab", "close", &tab], "cli:tab:close", "ok");
    assert_eq!(windows(&f), 1);
}

#[test]
fn pane_close_is_refused_by_a_pre_close_hook_and_nothing_is_closed() {
    let f = Fixture::start();
    let first = f.first_pane();
    let split = f.ok(
        &[
            "pane",
            "split",
            &first,
            "--direction",
            "right",
            "--no-focus",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let new_pane = split["pane"]["pane_id"].as_str().unwrap().to_string();
    let panes = |f: &Fixture| {
        f.tmux(&["list-panes", "-t", "one"])
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(panes(&f), 2);
    write_hooks_config(&f, &[("pre_close", REFUSE_CLOSE)]);

    let (code, env) = f.run(&["pane", "close", &new_pane]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    assert_eq!(panes(&f), 2, "the pane is still there");

    write_hooks_config(&f, &[("pre_close", "exit 0")]);
    f.ok(&["pane", "close", &new_pane], "cli:pane:close", "ok");
    assert_eq!(panes(&f), 1);
}

/// The hook is told what it is asked about: each close names its target, and the session, the
/// window and the pane that it is.
#[test]
fn a_pre_close_hook_is_told_what_is_being_closed() {
    let f = Fixture::start();
    f.tmux(&["new-session", "-d", "-s", "two"]).unwrap();
    let asked = hook_out(&f, "asked.txt");
    write_hooks_config(
        &f,
        &[(
            "pre_close",
            &format!(
                "echo \"$TAW_TARGET $TAW_SESSION $TAW_SESSION_ID\" >> '{}'",
                asked.display()
            ),
        )],
    );
    let two_id = f
        .tmux(&["display-message", "-p", "-t", "two", "#{session_id}"])
        .unwrap();

    f.ok(&["workspace", "close", "two"], "cli:workspace:close", "ok");

    assert_eq!(
        std::fs::read_to_string(&asked).unwrap().trim(),
        format!("session two {two_id}")
    );
}

/// A hook taw waits for gets tmux's global environment and `TMUX` too, whichever process asked:
/// here it is a `taw` command, which has neither.
#[test]
fn a_pre_close_hook_gets_tmuxs_global_environment_and_the_server_it_is_asked_from() {
    let f = Fixture::start();
    f.tmux(&["new-session", "-d", "-s", "two"]).unwrap();
    f.tmux(&[
        "set-environment",
        "-g",
        "NTFY_TOPIC",
        "taw-hooks-test-topic",
    ])
    .unwrap();
    let socket_path = f
        .tmux(&["display-message", "-p", "#{socket_path}"])
        .unwrap();
    write_hooks_config(
        &f,
        &[(
            "pre_close",
            &format!(
                "[ \"$NTFY_TOPIC\" = taw-hooks-test-topic ] || {{ echo 'no topic' >&2; exit 1; }}; \
                 case \"$TMUX\" in '{socket_path},'*) exit 0;; esac; echo 'wrong server' >&2; exit 1"
            ),
        )],
    );

    let (code, env) = f.run(&["workspace", "close", "two"]);

    assert_eq!(code, 0, "{env}");
    assert_eq!(session_names(&f), ["one"]);
}

/// A close of a group (a workspace and its worktree workspaces) asks about every session in
/// it before it closes any: one refusal keeps the whole group open. The session refused is
/// the last one in the order they close in (worktree workspaces first, the workspace itself
/// after them), so a close that asked and closed one session at a time would already have
/// closed the worktree's by then, and this would see it gone.
#[test]
fn a_group_close_asks_about_every_session_and_one_refusal_keeps_them_all() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let linked = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let mut everything = vec!["keeper".to_string(), linked.clone(), "one".to_string()];
    everything.sort();
    let mut config = std::fs::read_to_string(f.config.path().join("config.toml")).unwrap();
    config.push_str(
        "\n[hooks]\npre_close = \"[ \\\"$TAW_SESSION\\\" = one ] && { echo 'keep the workspace' >&2; exit 1; }; exit 0\"\n",
    );
    std::fs::write(f.config.path().join("config.toml"), config).unwrap();

    let (code, env) = f.run(&["workspace", "close", "one", "--group"]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(message.contains("keep the workspace"), "{env}");
    assert!(
        session_names(&f).contains(&linked),
        "the worktree's session was closed before the hook was asked about the workspace"
    );
    assert_eq!(
        session_names(&f),
        everything,
        "one refusal did not keep the whole group open"
    );
}

/// `taw worktree open` on a worktree whose session is closed and whose checkout is gone is refused
/// with `not_found`, naming the folder, and makes no session, where tmux would make one in the home
/// folder and say nothing. `worktree list` drops such a worktree, as it did.
#[test]
fn worktree_open_refuses_a_worktree_whose_checkout_is_gone_and_names_the_folder() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let path = created["worktree"]["path"].as_str().unwrap().to_string();
    let workspace = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    f.tmux(&["kill-session", "-t", &format!("={linked}")])
        .unwrap();
    std::fs::remove_dir_all(&path).unwrap();

    let (code, env) = f.run(&["worktree", "open", "--workspace", &workspace]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert_eq!(
        env["error"]["message"],
        format!("worktree_not_found: the checkout of worktree feat/x is gone: {path}"),
        "{env}"
    );
    assert!(!session_names(&f).contains(&linked), "a session was made");
    let listed = f.ok(&["worktree", "list"], "cli:worktree:list", "worktree_list");
    assert_eq!(listed["worktrees"], serde_json::json!([]), "{listed}");
}

/// `taw worktree open` on a worktree whose session was closed makes a new session, with a new tmux id, and says
/// it in `session_id`: the running session's own `$N`, which `taw app` attaches to on a machine. The record's
/// `workspace_id` is the id the session had when the worktree was created, and stays as it was.
#[test]
fn worktree_open_says_the_id_of_the_session_it_made() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let workspace = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let first_id = f
        .tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={linked}:"),
            "#{session_id}",
        ])
        .unwrap();
    f.tmux(&["kill-session", "-t", &format!("={linked}")])
        .unwrap();

    let opened = f.ok(
        &["worktree", "open", "--workspace", &workspace, "--no-focus"],
        "cli:worktree:open",
        "worktree_opened",
    );

    let running = f
        .tmux(&["list-sessions", "-F", "#{session_name} #{session_id}"])
        .unwrap();
    let running_id = running
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{linked} ")))
        .unwrap_or_else(|| panic!("the session {linked} is not running:\n{running}"))
        .to_string();
    assert_eq!(opened["already_open"], false, "{opened}");
    assert_eq!(
        opened["session_id"],
        serde_json::json!(running_id),
        "the reply names the running session: {opened}"
    );
    assert_ne!(
        running_id, first_id,
        "the premise: a new session has a new id"
    );
    assert_eq!(
        opened["worktree"]["workspace_id"],
        serde_json::json!(workspace),
        "the record's own id stays as it was: {opened}"
    );
}

/// `taw worktree remove` asks `[hooks] pre_worktree_remove` before git is: a refusal leaves
/// the checkout, the branch and the record where they were.
#[test]
fn worktree_remove_is_refused_by_a_pre_worktree_remove_hook_and_the_checkout_stays() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let path = std::path::PathBuf::from(created["worktree"]["path"].as_str().unwrap());
    let workspace = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut config = std::fs::read_to_string(f.config.path().join("config.toml")).unwrap();
    config
        .push_str("\n[hooks]\npre_worktree_remove = \"echo 'feat/x is not merged' >&2; exit 1\"\n");
    std::fs::write(f.config.path().join("config.toml"), &config).unwrap();

    let (code, env) = f.run(&["worktree", "remove", "--workspace", &workspace]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "hook_refused", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("pre-worktree-remove hook") && message.contains("feat/x is not merged"),
        "{env}"
    );
    assert!(path.join("README.md").exists(), "the checkout was removed");
    let listed = f.ok(&["worktree", "list"], "cli:worktree:list", "worktree_list");
    assert_eq!(listed["worktrees"].as_array().unwrap().len(), 1, "{listed}");

    // Once the hook agrees, the removal goes through.
    let agreeing = config.replace("exit 1", "exit 0");
    std::fs::write(f.config.path().join("config.toml"), agreeing).unwrap();
    f.ok(
        &["worktree", "remove", "--workspace", &workspace],
        "cli:worktree:remove",
        "worktree_removed",
    );
    assert!(!path.exists());
}

/// `taw worktree remove` refused because the checkout holds changes answers `worktree_dirty`,
/// the code a caller (the app, on a machine's row) switches on to ask whether to remove anyway;
/// the checkout stays, and `--force` removes it.
#[test]
fn worktree_remove_refused_for_changes_is_worktree_dirty_and_force_removes_it() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let path = std::path::PathBuf::from(created["worktree"]["path"].as_str().unwrap());
    let workspace = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(path.join("scratch.txt"), "not committed\n").unwrap();

    let (code, env) = f.run(&["worktree", "remove", "--workspace", &workspace]);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "worktree_dirty", "{env}");
    assert!(
        path.join("scratch.txt").exists(),
        "the refused removal deleted a file"
    );

    f.ok(
        &["worktree", "remove", "--workspace", &workspace, "--force"],
        "cli:worktree:remove",
        "worktree_removed",
    );
    assert!(!path.exists(), "--force left the checkout");
}

/// `taw worktree remove` closes the worktree's session when it is open, and asks
/// `pre_worktree_remove` about that and not `pre_close`: a hook that refuses every close does
/// not keep the session open. The README says so, so that a person who guards sessions in
/// `pre_close` guards removal too.
#[test]
fn worktree_remove_closes_an_open_session_without_asking_pre_close() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let linked = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let workspace = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        session_names(&f).contains(&linked),
        "the worktree session is open"
    );
    let mut config = std::fs::read_to_string(f.config.path().join("config.toml")).unwrap();
    config.push_str(&format!(
        "\n[hooks]\npre_close = {}\n",
        toml::Value::String(REFUSE_CLOSE.to_string())
    ));
    std::fs::write(f.config.path().join("config.toml"), config).unwrap();

    f.ok(
        &["worktree", "remove", "--workspace", &workspace],
        "cli:worktree:remove",
        "worktree_removed",
    );

    assert!(
        !session_names(&f).contains(&linked),
        "pre_close kept the worktree's session open"
    );
}

/// With no plugin and no `[hooks]` entry for the event, `taw plugin event` reports that
/// nobody is subscribed and starts nothing, as it did before `[hooks]` existed.
#[test]
fn plugin_event_with_nothing_hooked_to_it_starts_nothing() {
    let f = Fixture::start();
    write_hooks_config(&f, &[("tab.closed", "touch never")]);
    let window = f.tmux(&["display-message", "-p", "#{window_id}"]).unwrap();
    let session = f.tmux(&["display-message", "-p", "#{session_id}"]).unwrap();

    let result = f.ok(
        &[
            "plugin",
            "event",
            "tab.focused",
            "--tab",
            &window,
            "--workspace",
            &session,
        ],
        "cli:plugin:event",
        "plugin_event",
    );

    assert_eq!(result["subscribed"], false, "{result}");
    assert_eq!(result["launched"], 0, "{result}");
}

/// `taw integration status`, against a home this test builds: a Claude config directory with a
/// settings file of the user's own, and the environment every test server has (`ShellHome`), so
/// that every agent's config directory is found in that home and none of the runner's is read.
#[test]
fn integration_status_reports_each_agent() {
    let f = Fixture::start();
    let home = ShellHome::new();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("settings.json"), "{\"theme\": \"dark\"}\n").unwrap();
    let vars = home.server_vars();
    let env: Vec<(&str, &str)> = vars
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let status = |args: &[&str]| -> Value {
        let (code, stdout, stderr) = f.run_with_env(args, &env);
        let envelope = parse_envelope(args, &stdout, &stderr);
        assert_eq!(code, 0, "{envelope}");
        assert_eq!(envelope["id"], "cli:integration:status", "{envelope}");
        assert_eq!(
            envelope["result"]["type"], "integration_status",
            "{envelope}"
        );
        envelope["result"].clone()
    };
    // Which agents' paths are not in the home: names only, as the path of one that is not would be
    // the runner's.
    let outside = |entries: &[Value]| -> Vec<String> {
        entries
            .iter()
            .filter(|entry| {
                !entry["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with(home.path()))
            })
            .map(|entry| entry["agent"].as_str().unwrap_or("?").to_string())
            .collect()
    };

    let result = status(&["integration", "status", "claude"]);
    let entries = result["integrations"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{result}");
    let e = &entries[0];
    assert_eq!(e["agent"], "claude", "{result}");
    assert_eq!(
        e["installed"], false,
        "nothing was installed in the home: {e}"
    );
    assert!(e.get("version").is_some(), "{e}");
    assert_eq!(
        outside(entries),
        Vec::<String>::new(),
        "claude's config was looked for outside the home this test built"
    );

    let all = status(&["integration", "status"]);
    let entries = all["integrations"].as_array().unwrap();
    assert_eq!(entries.len(), taw::integration::AGENTS.len(), "{all}");
    assert_eq!(
        outside(entries),
        Vec::<String>::new(),
        "these agents' config was looked for outside the home this test built"
    );

    // An agent herdr detects but has no integration for is reported, not a clap failure.
    let (code, stdout, stderr) = f.run_with_env(&["integration", "status", "gemini"], &env);
    let envelope = parse_envelope(&["integration", "status", "gemini"], &stdout, &stderr);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "unsupported", "{envelope}");
}

/// An OpenCode config directory and state directory under `root`, and the environment that
/// points `taw` at them, so that `integration status` reads nothing of the developer's own.
fn opencode_home(root: &std::path::Path) -> [(String, String); 3] {
    let dir = |relative: &str| {
        let path = root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path.display().to_string()
    };
    [
        ("HOME".to_string(), dir("home")),
        ("XDG_CONFIG_HOME".to_string(), dir("config")),
        ("XDG_STATE_HOME".to_string(), dir("state")),
    ]
}

/// `taw integration status opencode --outdated-only`, and its one entry when it lists one.
fn outdated_opencode(f: &Fixture, env: &[(String, String)]) -> Vec<Value> {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let args = ["integration", "status", "opencode", "--outdated-only"];
    let (code, stdout, stderr) = f.run_with_env(&args, &env);
    let envelope = parse_envelope(&args, &stdout, &stderr);
    assert_eq!(code, 0, "{envelope}");
    envelope["result"]["integrations"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn outdated_only_measures_each_integration_against_its_own_current_version() {
    let f = Fixture::start();
    let root = tempfile::tempdir().unwrap();
    let env = opencode_home(root.path());
    let plugins = root.path().join("config/opencode/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    // The single server plugin an earlier taw wrote, version 2. This build writes version 5, and
    // the check was against one version for every integration, which was 1.
    std::fs::write(
        plugins.join("taw-agent-state.js"),
        "// TAW_INTEGRATION_VERSION=2\n",
    )
    .unwrap();
    std::fs::write(root.path().join("config/opencode/cli.json"), "{}\n").unwrap();

    let listed = outdated_opencode(&f, &env);

    assert_eq!(listed.len(), 1, "{listed:?}");
    let entry = &listed[0];
    assert_eq!(entry["agent"], "opencode");
    assert_eq!(entry["outdated"], true, "{entry}");
    assert_eq!(entry["version"], 2, "{entry}");
    assert_eq!(entry["current_version"], 5, "{entry}");
    let note = entry["note"].as_str().unwrap();
    assert!(
        note.contains("version 2") && note.contains("version 5"),
        "{note}"
    );
    assert!(
        note.contains("`taw integration install opencode`"),
        "{note}"
    );

    // Once it is as current as this build's, it is not listed.
    std::fs::write(
        plugins.join("taw-agent-state.js"),
        "// TAW_INTEGRATION_VERSION=5\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("config/opencode/cli.json"),
        "{\"plugins\": [\"./taw-opencode\"]}\n",
    )
    .unwrap();
    assert!(outdated_opencode(&f, &env).is_empty());
}

/// The one entry `taw integration status opencode` reports.
fn opencode_status(f: &Fixture, env: &[(String, String)]) -> Value {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let args = ["integration", "status", "opencode"];
    let (code, stdout, stderr) = f.run_with_env(&args, &env);
    let envelope = parse_envelope(&args, &stdout, &stderr);
    assert_eq!(code, 0, "{envelope}");
    envelope["result"]["integrations"][0].clone()
}

/// OpenCode's config directory with the server plugin as this build writes it, which is what an
/// install that has been run leaves.
fn opencode_installed(root: &std::path::Path) -> PathBuf {
    let config = root.join("config/opencode");
    std::fs::create_dir_all(config.join("plugins")).unwrap();
    std::fs::write(
        config.join("plugins/taw-agent-state.js"),
        "// TAW_INTEGRATION_VERSION=5\n",
    )
    .unwrap();
    config
}

#[test]
fn an_opencode_2_that_has_never_run_is_noted_but_is_not_outdated() {
    let f = Fixture::start();
    // A home of OpenCode 1 only, with the settings OpenCode 2 would move into a `cli.json` of its
    // own the first time it starts (its `kv.json`, or a `tui.json`), and no `cli.json`: the
    // install put OpenCode 2's registration off, and nothing a second install does would change
    // that until OpenCode 2 has run.
    for waiting_on in ["state/opencode/kv.json", "config/opencode/tui.json"] {
        let root = tempfile::tempdir().unwrap();
        let env = opencode_home(root.path());
        opencode_installed(root.path());
        let settings = root.path().join(waiting_on);
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}\n").unwrap();

        // Not listed by `--outdated-only`, and not outdated in the entry, which still says so.
        assert!(outdated_opencode(&f, &env).is_empty(), "{waiting_on}");
        let entry = opencode_status(&f, &env);
        assert_eq!(entry["outdated"], false, "{waiting_on}: {entry}");
        let note = entry["note"].as_str().unwrap();
        assert!(
            note.contains("cli.json") && note.contains("Start OpenCode 2 once"),
            "{waiting_on}: {note}"
        );
        assert!(
            note.contains("`taw integration install opencode`")
                && note.contains("OpenCode 1 is not affected"),
            "{waiting_on}: {note}"
        );
    }
}

#[test]
fn a_cli_json_without_taws_entry_is_outdated_and_a_reinstall_adds_it() {
    let f = Fixture::start();
    let root = tempfile::tempdir().unwrap();
    let env = opencode_home(root.path());
    let config = opencode_installed(root.path());
    // OpenCode 2 has run and made its `cli.json`, with none of taw's in it.
    std::fs::write(config.join("cli.json"), "{}\n").unwrap();

    let listed = outdated_opencode(&f, &env);

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["outdated"], true, "{}", listed[0]);
    let note = listed[0]["note"].as_str().unwrap();
    assert!(
        note.contains("cli.json") && !note.contains("Start OpenCode 2 once"),
        "{note}"
    );
    assert!(
        note.contains("`taw integration install opencode`"),
        "{note}"
    );

    // With the entry in it, nothing is left to do, and the entry says so in the same fields.
    std::fs::write(
        config.join("cli.json"),
        "{\"plugins\": [\"./taw-opencode\"]}\n",
    )
    .unwrap();
    assert!(outdated_opencode(&f, &env).is_empty());
    let entry = opencode_status(&f, &env);
    assert_eq!(entry["outdated"], false, "{entry}");
    assert_eq!(entry["note"], Value::Null, "{entry}");
    assert_eq!(entry["current_version"], 5, "{entry}");
}

#[test]
fn installing_an_integration_without_its_config_dir_reports_config_dir_missing() {
    let f = Fixture::start();
    // A missing config dir means the agent itself is not installed, so `install` must
    // refuse rather than fabricate config for absent software. Pointing
    // `CLAUDE_CONFIG_DIR` at a path inside a tempdir that does not exist exercises the
    // gate and writes nothing — in particular it never touches the real `~/.claude`.
    let absent = f.work.path().join("no-such-claude-dir");
    assert!(!absent.exists());

    let (code, stdout, stderr) = f.run_with_env(
        &["integration", "install", "claude"],
        &[("CLAUDE_CONFIG_DIR", absent.to_str().unwrap())],
    );
    let env = parse_envelope(&["integration", "install", "claude"], &stdout, &stderr);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "config_dir_missing",
        "a declared code must not fall through to `internal`: {env}"
    );
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no-such-claude-dir"),
        "the message should name the directory it wanted: {env}"
    );
    assert!(!absent.exists(), "install must not create the config dir");
}

#[test]
fn plugin_list_is_an_empty_envelope_before_anything_is_installed() {
    let f = Fixture::start();
    let result = f.ok(&["plugin", "list"], "cli:plugin:list", "plugin_list");
    assert!(
        result["plugins"].as_array().unwrap().is_empty(),
        "a fresh state dir has no plugins: {result}"
    );

    let actions = f.ok(
        &["plugin", "action", "list"],
        "cli:plugin:action-list",
        "plugin_action_list",
    );
    assert!(
        actions["actions"].as_array().unwrap().is_empty(),
        "{actions}"
    );

    let started = f.ok(
        &["plugin", "startup"],
        "cli:plugin:startup",
        "plugin_startup",
    );
    assert!(
        started["started"].as_array().unwrap().is_empty(),
        "{started}"
    );

    let dir = f.ok(
        &["plugin", "config-dir", "demo"],
        "cli:plugin:config-dir",
        "plugin_config_dir",
    );
    assert!(
        dir["path"]
            .as_str()
            .unwrap()
            .starts_with(f.config.path().to_str().unwrap()),
        "{dir}"
    );

    let (code, env) = f.run(&["plugin", "enable", "ghost"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn plugin_update_lists_a_linked_plugin_as_linked_and_names_an_unknown_id() {
    let f = Fixture::start();
    let folder = f.work.path().join("mine");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("herdr-plugin.toml"),
        "id = \"mine\"\nversion = \"1.0.0\"\nmin_herdr_version = \"0.7.0\"\n",
    )
    .unwrap();
    f.ok(
        &["plugin", "link", folder.to_str().unwrap()],
        "cli:plugin:link",
        "plugin_linked",
    );

    // With no id: every installed plugin, and a linked one is left alone and says so.
    let all = f.ok(
        &["plugin", "update", "--yes"],
        "cli:plugin:update",
        "plugin_update",
    );
    assert_eq!(
        all["plugins"],
        serde_json::json!([{ "id": "mine", "status": "linked" }]),
        "{all}"
    );
    let named = f.ok(
        &["plugin", "update", "mine"],
        "cli:plugin:update",
        "plugin_update",
    );
    assert_eq!(named["plugins"][0]["status"], "linked", "{named}");

    // An unknown id is an error naming it.
    let (code, env) = f.run(&["plugin", "update", "mine", "ghost"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'ghost'"),
        "{env}"
    );

    // A source is taken as well as an id; one nothing was installed from is not found,
    // and a linked plugin is never named by a source.
    let (code, env) = f.run(&["plugin", "update", "owner/ghost"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    assert!(
        env["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'owner/ghost'"),
        "{env}"
    );
}
// ---------------------------------------------------------------------------------------
// `workspace close --group` — herdr v0.9.1 (#2874): a workspace with worktree workspaces
// still open is closed together with them only on an explicit `--group`.
// ---------------------------------------------------------------------------------------

/// Makes the fixture's first workspace (`one`, whose directory is `f.work`) a git
/// repository and points `[worktrees] directory` at a tempdir, so `worktree create` has
/// somewhere to put a checkout. A third workspace, `keeper`, keeps the server alive when
/// the others are closed. The returned tempdir holds the checkouts.
fn worktree_setup(f: &Fixture) -> tempfile::TempDir {
    let git = |args: &[&str]| {
        let status = taw::git::command()
            .arg("-C")
            .arg(f.work.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(f.work.path().join("README.md"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=Test",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "-m",
        "first",
    ]);
    let checkouts = tempfile::tempdir().unwrap();
    std::fs::write(
        f.config.path().join("config.toml"),
        format!(
            "[worktrees]\ndirectory = \"{}\"\n",
            checkouts.path().display()
        ),
    )
    .unwrap();
    f.tmux(&["new-session", "-d", "-s", "keeper"]).unwrap();
    checkouts
}

fn session_names(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = f
        .tmux(&["list-sessions", "-F", "#{session_name}"])
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

#[test]
fn workspace_close_needs_group_while_worktree_workspaces_are_open() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/x",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let linked_name = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let linked_id = created["worktree"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Without --group: refused, and nothing is closed.
    let (code, env) = f.run(&["workspace", "close", "one"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "workspace_group_close_required",
        "{env}"
    );
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("--group"),
        "the refusal names the flag: {message}"
    );
    assert!(
        message.contains(&linked_name),
        "the refusal names what --group would also close: {message}"
    );
    let mut everything = vec!["keeper".to_string(), linked_name.clone(), "one".to_string()];
    everything.sort();
    assert_eq!(
        session_names(&f),
        everything,
        "a refused close must leave every session open"
    );

    // With --group: the workspace and its worktree workspace close together, and the reply
    // says which worktree workspaces went with it.
    let closed = f.ok(
        &["workspace", "close", "one", "--group"],
        "cli:workspace:close",
        "ok",
    );
    assert_eq!(
        closed["worktree_workspace_ids"],
        serde_json::json!([linked_id]),
        "{closed}"
    );
    assert_eq!(session_names(&f), vec!["keeper".to_string()]);
}

/// The refusal is only for a workspace with worktree workspaces still open: a plain one,
/// a worktree workspace on its own, and a workspace whose worktrees are already closed all
/// close without `--group`, as before.
#[test]
fn workspace_close_without_open_worktree_workspaces_needs_no_group() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);

    // A workspace nothing was cut from, and `--group` on it, are just a close.
    f.ok(
        &["workspace", "create", "--label", "plain", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    let closed = f.ok(
        &["workspace", "close", "plain"],
        "cli:workspace:close",
        "ok",
    );
    assert!(
        closed.get("worktree_workspace_ids").is_none(),
        "nothing else was closed: {closed}"
    );
    f.ok(
        &["workspace", "create", "--label", "plain2", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );
    f.ok(
        &["workspace", "close", "plain2", "--group"],
        "cli:workspace:close",
        "ok",
    );

    // A worktree workspace closes alone; afterwards its source has nothing open left, so it
    // closes without --group too.
    let created = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/y",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let linked_name = created["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    f.ok(
        &["workspace", "close", &linked_name],
        "cli:workspace:close",
        "ok",
    );
    assert_eq!(
        session_names(&f),
        vec!["keeper".to_string(), "one".to_string()]
    );
    f.ok(&["workspace", "close", "one"], "cli:workspace:close", "ok");
    assert_eq!(session_names(&f), vec!["keeper".to_string()]);
}

/// herdr expands a group only from a workspace that is not a linked worktree itself: a
/// worktree workspace closes on its own even when another worktree was cut from it. The
/// nested one stays open, and still closes with the primary.
#[test]
fn workspace_close_of_a_worktree_that_has_a_worktree_of_its_own_needs_no_group() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    let first = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            "one",
            "--branch",
            "feat/a",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let first_name = first["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let nested = f.ok(
        &[
            "worktree",
            "create",
            "--workspace",
            &first_name,
            "--branch",
            "feat/nested",
            "--no-focus",
        ],
        "cli:worktree:create",
        "worktree_created",
    );
    let nested_name = nested["worktree"]["session_name"]
        .as_str()
        .unwrap()
        .to_string();

    let closed = f.ok(
        &["workspace", "close", &first_name],
        "cli:workspace:close",
        "ok",
    );
    assert!(
        closed.get("worktree_workspace_ids").is_none(),
        "only the worktree workspace itself closed: {closed}"
    );
    let mut left = vec!["keeper".to_string(), nested_name.clone(), "one".to_string()];
    left.sort();
    assert_eq!(session_names(&f), left);

    // The primary still has the nested worktree in its group.
    let (code, env) = f.run(&["workspace", "close", "one"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(
        env["error"]["code"], "workspace_group_close_required",
        "{env}"
    );
    f.ok(
        &["workspace", "close", "one", "--group"],
        "cli:workspace:close",
        "ok",
    );
    assert_eq!(session_names(&f), vec!["keeper".to_string()]);
}

/// `taw` run from a pane of one of the sessions it is asked to close is hung up on when
/// that session is killed, and whatever it had not closed yet stays open. So the session
/// holding the caller's own pane is closed last of all, and a group close started from
/// inside a worktree workspace still closes the whole group.
#[test]
fn workspace_close_group_run_from_inside_a_worktree_workspace_closes_the_whole_group() {
    let f = Fixture::start();
    let _checkouts = worktree_setup(&f);
    for branch in ["feat/a", "feat/b"] {
        f.ok(
            &[
                "worktree",
                "create",
                "--workspace",
                "one",
                "--branch",
                branch,
                "--no-focus",
            ],
            "cli:worktree:create",
            "worktree_created",
        );
    }
    // The first worktree workspace the records name is the first one `--group` would close,
    // so it is where the command has to run to catch a wrong order.
    let records = std::fs::read_to_string(f.state.path().join("worktrees.json")).unwrap();
    let records: Value = serde_json::from_str(&records).unwrap();
    let first_name = records["worktrees"][0]["session_name"]
        .as_str()
        .unwrap()
        .to_string();
    let pane = f
        .tmux(&["list-panes", "-t", &first_name, "-F", "#{pane_id}"])
        .unwrap();
    let pane = pane.lines().next().unwrap().to_string();

    let out = f.work.path().join("group-close.out");
    let command = format!(
        "{} workspace close one --group > {} 2>&1",
        env!("CARGO_BIN_EXE_taw"),
        out.display()
    );
    f.tmux(&["send-keys", "-t", &pane, &command, "Enter"])
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while session_names(&f) != vec!["keeper".to_string()] {
        assert!(
            std::time::Instant::now() < deadline,
            "the group was not closed from inside it; left: {:?}; output: {:?}",
            session_names(&f),
            std::fs::read_to_string(&out).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The worktree records only tell `workspace close` whether a group close was asked for; a
/// record file taw cannot read must not stop an ordinary workspace closing.
#[test]
fn workspace_close_still_works_when_the_worktree_records_are_unreadable() {
    let f = Fixture::start();
    std::fs::write(f.state.path().join("worktrees.json"), "not json at all").unwrap();
    f.ok(
        &["workspace", "create", "--label", "plain", "--no-focus"],
        "cli:workspace:create",
        "workspace_created",
    );

    let closed = f.ok(
        &["workspace", "close", "plain"],
        "cli:workspace:close",
        "ok",
    );

    assert!(closed.get("worktree_workspace_ids").is_none(), "{closed}");
    let names = f.tmux(&["list-sessions", "-F", "#{session_name}"]).unwrap();
    assert_eq!(names, "one", "only the fixture's own workspace is left");
}

#[test]
fn worktree_list_is_empty_and_create_outside_a_repo_is_not_a_git_repo() {
    let f = Fixture::start();
    let result = f.ok(&["worktree", "list"], "cli:worktree:list", "worktree_list");
    assert!(
        result["worktrees"].as_array().unwrap().is_empty(),
        "{result}"
    );

    // The fixture's session cwd is this test process's cwd (the repo), so point a fresh
    // workspace at a non-repo tempdir to exercise the error path deterministically.
    let dir = tempfile::tempdir().unwrap();
    let created = f.ok(
        &[
            "workspace",
            "create",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--label",
            "norepo",
            "--no-focus",
        ],
        "cli:workspace:create",
        "workspace_created",
    );
    let ws = created["workspace"]["workspace_id"].as_str().unwrap();
    let (code, env) = f.run(&[
        "worktree",
        "create",
        "--workspace",
        ws,
        "--branch",
        "feature-x",
    ]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_a_git_repo", "{env}");

    let (code, env) = f.run(&["worktree", "remove", "--workspace", "nope"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn version_and_skill_and_pretty() {
    let f = Fixture::start();

    let (code, stdout, _) = f.run_raw(&["--version"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "--version should print the crate version: {stdout}"
    );

    // `skill` prints raw markdown, not an envelope: it exists to be redirected into a
    // file or pasted straight into an agent's context.
    let (code, text, stderr) = f.run_raw(&["skill"]);
    assert_eq!(code, 0, "stdout={text} stderr={stderr}");
    assert!(
        !text.trim_start().starts_with('{'),
        "skill must not be wrapped in an envelope: {text}"
    );
    assert!(
        text.starts_with("---"),
        "should begin with SKILL.md's frontmatter: {}",
        text.lines().next().unwrap_or_default()
    );
    assert!(text.contains("taw"), "SKILL.md should document `taw`");
    assert!(
        text.contains("$TMUX"),
        "SKILL.md must guard on $TMUX, not HERDR_ENV"
    );
    assert!(
        !text.contains("HERDR_ENV"),
        "SKILL.md must not tell agents to check HERDR_ENV"
    );
    // `--pretty` has nothing to format for a raw payload, and must not corrupt it.
    let (_, pretty_skill, _) = f.run_raw(&["--pretty", "skill"]);
    assert_eq!(pretty_skill, text, "--pretty must not alter a raw payload");

    // `--pretty` indents the same envelope; the default is one line.
    let (_, one_line, _) = f.run_raw(&["pane", "list"]);
    assert_eq!(one_line.trim_end().lines().count(), 1, "{one_line}");
    let (_, pretty, _) = f.run_raw(&["--pretty", "pane", "list"]);
    assert!(pretty.lines().count() > 1, "{pretty}");
    assert_eq!(
        serde_json::from_str::<Value>(&pretty).unwrap(),
        serde_json::from_str::<Value>(&one_line).unwrap(),
        "--pretty must only change formatting"
    );

    // A clap-level syntax error exits 2, like herdr.
    let (code, _, stderr) = f.run_raw(&["pane", "definitely-not-a-verb"]);
    assert_eq!(code, 2, "{stderr}");
}

/// A new window whose pane runs a program that stays in front of the terminal and ignores an
/// interrupt (`sleep` standing for an agent's process, run as `claude`, since what the verbs that type
/// look for in front of a pane is a process that names its agent), with a claude reported for it:
/// what the verbs that type need to find. A pane whose own shell is idle at its prompt hosts no agent,
/// whatever it records (herdr's `runtime_hosts_agent`), and they refuse it. Nothing is typed into a
/// shell here, so no history is written either.
fn pane_with_an_agent_in_front(f: &Fixture) -> String {
    // A path of the fixture's own: a window made by a client that is not attached starts in the
    // client's directory, which is the checkout's.
    let claude = f.work.path().join("claude");
    let pane = f
        .tmux(&[
            "new-window",
            "-d",
            "-t",
            "one:",
            "-P",
            "-F",
            "#{pane_id}",
            &format!(
                "trap '' INT; ln -s \"$(command -v sleep)\" '{0}'; exec '{0}' 600",
                claude.display()
            ),
        ])
        .unwrap();
    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    pane
}

#[test]
fn agent_send_keys_prompt_and_focus_reach_the_pane() {
    let f = Fixture::start();
    let pane = pane_with_an_agent_in_front(&f);

    // `agent prompt` without --wait is send-text + Enter: the terminal echoes it.
    f.ok(
        &["agent", "prompt", &pane, "echo taw_prompt_ok"],
        "cli:agent:prompt",
        "ok",
    );
    let waited = f.ok(
        &[
            "pane",
            "wait-output",
            &pane,
            "--match",
            "taw_prompt_ok",
            "--timeout",
            "10000",
        ],
        "cli:pane:wait-output",
        "pane_output_match",
    );
    assert!(waited["matched"].as_bool().unwrap(), "{waited}");

    // Logical key names are translated to tmux key names.
    f.ok(
        &["agent", "send-keys", &pane, "esc", "ctrl+c"],
        "cli:agent:send-keys",
        "ok",
    );
    let read = f.ok(
        &["agent", "read", &pane, "--lines", "50"],
        "cli:agent:read",
        "pane_output",
    );
    assert!(read["text"].is_string(), "{read}");

    f.ok(&["agent", "focus", &pane], "cli:agent:focus", "ok");
    f.ok(&["agent", "attach", &pane], "cli:agent:attach", "ok");

    // An unknown key name is rejected before any bytes are written.
    let (code, env) = f.run(&["agent", "send-keys", &pane, "not+a+key"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");
}

/// herdr's CLI parser (`<herdr-src>/src/cli/agent.rs`, `agent_prompt`) rejects `--until`
/// or `--timeout` given without `--wait` (a usage error, exit 2) — they only mean
/// anything paired with it. taw's `prompt` (`src/cli/agent.rs`) checks `wait` before ever
/// looking at `until`/`timeout` and returns early, so the same flags are accepted and
/// silently ignored instead of rejected. This pins taw's current behaviour rather than
/// herdr's, which differs (the behaviour is left as it is).
#[test]
fn agent_prompt_silently_ignores_until_and_timeout_without_wait_unlike_herdr() {
    let f = Fixture::start();
    let pane = pane_with_an_agent_in_front(&f);

    f.ok(
        &[
            "agent",
            "prompt",
            &pane,
            "echo taw_prompt_ignored_flags",
            "--until",
            "idle",
            "--timeout",
            "10000",
        ],
        "cli:agent:prompt",
        "ok",
    );
}

#[test]
fn pane_zoom_rename_swap_move_and_input() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let split = f.ok(
        &["pane", "split", &pane, "--direction", "down", "--no-focus"],
        "cli:pane:split",
        "pane_split",
    );
    let other = split["pane"]["pane_id"].as_str().unwrap().to_string();

    let zoomed = f.ok(
        &["pane", "zoom", &pane, "--on"],
        "cli:pane:zoom",
        "pane_zoom",
    );
    assert_eq!(zoomed["zoomed"], true, "{zoomed}");
    let unzoomed = f.ok(
        &["pane", "zoom", &pane, "--off"],
        "cli:pane:zoom",
        "pane_zoom",
    );
    assert_eq!(unzoomed["zoomed"], false, "{unzoomed}");

    let renamed = f.ok(
        &["pane", "rename", &pane, "build", "logs"],
        "cli:pane:rename",
        "pane",
    );
    assert_eq!(renamed["pane"]["title"], "build logs", "{renamed}");
    let cleared = f.ok(
        &["pane", "rename", &pane, "--clear"],
        "cli:pane:rename",
        "pane",
    );
    assert_eq!(cleared["pane"]["title"], Value::Null, "{cleared}");

    f.ok(
        &[
            "pane",
            "swap",
            "--source-pane",
            &pane,
            "--target-pane",
            &other,
        ],
        "cli:pane:swap",
        "ok",
    );

    // `pane input` is accepted for herdr parity and does nothing.
    f.ok(
        &["pane", "input", &pane, "--right-click", "pane"],
        "cli:pane:input",
        "ok",
    );

    // Move the pane to its own new tab.
    let moved = f.ok(
        &["pane", "move", &other, "--new-tab", "--no-focus"],
        "cli:pane:move",
        "pane_moved",
    );
    assert!(
        moved["move_result"]["pane"]["pane_id"].is_string(),
        "{moved}"
    );
    assert_eq!(
        moved["move_result"]["previous_pane_id"],
        other.as_str(),
        "{moved}"
    );
    let tabs = f.ok(&["tab", "list"], "cli:tab:list", "tab_list");
    assert_eq!(tabs["tabs"].as_array().unwrap().len(), 2, "{tabs}");
}

#[test]
fn workspace_id_and_pane_current_come_from_the_environment() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let terminal_id = f.ok(&["pane", "get", &pane], "cli:pane:get", "pane")["pane"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, env) = f.run_in_pane(&terminal_id, &["workspace", "id"]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(env["id"], "cli:workspace:id", "{env}");
    assert!(
        regex::Regex::new(r"^w\d+$")
            .unwrap()
            .is_match(env["result"]["workspace_id"].as_str().unwrap()),
        "{env}"
    );

    // Outside a pane there is nothing to report.
    let (code, env) = f.run(&["workspace", "id"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn watch_attach_status_detach_and_seen() {
    let f = Fixture::start();
    let pane = f.first_pane();
    let terminal = f.terminal_id(&pane);

    // `watch status` reports every pane with its watch state, so the pane is listed
    // before it is watched — with `piped: false`.
    let before = f.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    let panes = before["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 1, "{before}");
    assert_eq!(panes[0]["terminal_id"], terminal.as_str(), "{before}");
    assert_eq!(panes[0]["piped"], false, "{before}");
    assert_eq!(before["watched"], 0, "{before}");

    let attached = f.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert_eq!(attached["attached"], true, "{attached}");
    assert_eq!(attached["terminal_id"], terminal.as_str(), "{attached}");

    let during = f.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(during["watched"], 1, "{during}");
    assert_eq!(during["panes"][0]["piped"], true, "{during}");

    // NOTE: re-attaching an already-watched pane is deliberately *not* asserted here.
    // `watch::attach` uses `pipe-pane -o`, which tmux treats as a toggle: on a pane that
    // already has a pipe it *closes* it. So a second `watch attach`, or an `attach-all`
    // after one, currently turns watching off while still reporting success. That is a
    // bug in `watch::attach`, so asserting today's behaviour
    // here would only lock it in.

    f.ok(&["watch", "detach", &pane], "cli:watch:detach", "ok");
    let after = f.ok(&["watch", "status"], "cli:watch:status", "watch_status");
    assert_eq!(after["watched"], 0, "{after}");
    assert_eq!(after["panes"][0]["piped"], false, "{after}");

    // `seen` clears the `done`-vs-`idle` distinction for a pane that has an agent.
    f.ok(
        &[
            "pane",
            "report-agent",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--state",
            "idle",
        ],
        "cli:pane:report-agent",
        "ok",
    );
    let seen = f.ok(&["seen", &pane], "cli:seen", "ok");
    assert_eq!(seen["terminal_id"], terminal.as_str(), "{seen}");
    assert_eq!(f.pane_option(&pane, "@agent_seen"), "1");

    // An unknown pane is `not_found`, not a panic — these run from tmux hooks, often
    // just as a pane is going away.
    let (code, env) = f.run(&["seen", "w9:p99"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
    let (code, env) = f.run(&["watch", "attach", "w9:p99"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found", "{env}");
}

#[test]
fn ui_is_registered_and_reports_an_envelope_without_a_terminal() {
    let f = Fixture::start();
    // `taw ui` draws a ratatui popup, so it cannot be exercised headlessly. What matters
    // here is that it is wired (not a clap error, not `unsupported`) and that failing
    // without a terminal still produces an envelope rather than a panic.
    let (code, env) = f.run(&["ui"]);
    assert!(code == 0 || code == 1, "`taw ui` exited {code}: {env}");
    assert_eq!(env["id"], "cli:ui", "{env}");
    if code == 1 {
        let reported = env["error"]["code"].as_str().unwrap();
        assert_ne!(
            reported, "unsupported",
            "the TUI is built into this binary, so `ui` must not report unsupported: {env}"
        );
    }

    // `--from` is how `ui open` hands the popup the pane it was opened from. It must not
    // disturb the envelope: same id, same absence of a clap error.
    let pane = f.terminal_id(&f.first_pane());
    let (code, env) = f.run(&["ui", "--from", &pane]);
    assert!(
        code == 0 || code == 1,
        "`taw ui --from` exited {code}: {env}"
    );
    assert_eq!(env["id"], "cli:ui", "{env}");
}

/// The bug this flag exists for: `display-popup -E` does not give its command the
/// invoking pane's environment, so `$TMUX_PANE` is unset inside the popup and it could
/// never tell which pane it was opened from — the cursor never opened on your own row and
/// `prefix+O` walked from nowhere. `ui open` runs from the key binding's `run-shell`,
/// where tmux still resolves the current client's active pane, so it asks there and passes
/// the answer down as `--from`.
///
/// `display-popup` itself is intercepted through `$TAW_TMUX_BIN` — a shim that records the
/// command line and opens nothing — because a real popup needs an attached client and
/// would then run a whole TUI. Everything else, `#{pane_id}` included, is answered by the
/// fixture's real tmux.
#[test]
fn ui_open_tells_the_popup_which_pane_it_was_opened_from() {
    let f = Fixture::start();
    let record = f.work.path().join("display-popup-command");
    let shim = f.work.path().join("tmux-recording-the-popup");
    std::fs::write(
        &shim,
        // Answers the two things a detached fixture server cannot (no client, so no client
        // name and no client size), records the popup command line, and opens nothing.
        // Everything else is the real tmux — `#{pane_id}`, the answer under test, included.
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *client_name*) echo \'/dev/ttys999\' ; exit 0 ;;\n  *client_width*) echo \'80 24\' ; exit 0 ;;\n  *\" display-popup \"*) printf \'%s\' \"$*\" > {} ; exit 0 ;;\nesac\nexec tmux \"$@\"\n",
            record.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);

    // What tmux itself answers for "the current pane" on this server, asked exactly as
    // `ui open` asks it.
    let pane = f
        .tmux(&["display-message", "-p", "#{pane_id}"])
        .expect("tmux reports a current pane");

    let args = ["ui", "open"];
    let (code, stdout, stderr) = f.run_with_env(&args, &[("TAW_TMUX_BIN", shim.to_str().unwrap())]);
    let env = parse_envelope(&args, &stdout, &stderr);
    assert_eq!(code, 0, "{env}");

    let command = std::fs::read_to_string(&record).expect("the shim recorded a display-popup");
    // The popup command is `display-popup`'s last argument, so it ends the recorded line.
    // This fixture's session has its status line on, so the swap succeeds and the popup is
    // told the pane it was opened from, then the client it is on, then that the bar is up.
    assert!(
        command.ends_with(&format!("ui --from '{pane}' --client /dev/ttys999 --bar")),
        "the popup must be told the pane it was opened from, the client it is on, then that the bar is up: {command}"
    );
    // ... and it is shown on that client, whatever tmux takes as the current one by then.
    assert!(
        command.contains(" display-popup -c /dev/ttys999 -E "),
        "the popup must be opened on the client the launcher named: {command}"
    );
}

#[test]
fn free_text_metadata_is_stored_with_its_control_characters_folded_to_spaces() {
    // The write side of the same hazard `tests/tmux_client.rs` covers on the read side.
    // `--display-agent`/`--title` take arbitrary text from a hook script, and a newline in
    // one of them could break every listing on the server. Nothing that
    // reaches a tmux option through this verb may carry a line break at all.
    let f = Fixture::start();
    let pane = f.first_pane();

    f.ok(
        &[
            "pane",
            "report-metadata",
            &pane,
            "--source",
            "test",
            "--display-agent",
            "Reviewer\nBot",
            "--title",
            "reviewing\r\n#482",
        ],
        "cli:pane:report-metadata",
        "ok",
    );

    assert_eq!(
        f.pane_option(&pane, "@agent_display"),
        "Reviewer Bot",
        "a newline in --display-agent must be folded before it reaches tmux"
    );
    assert_eq!(f.pane_option(&pane, "@agent_title"), "reviewing  #482");

    // `pane rename` writes the same option from a different verb, so it folds too.
    f.ok(
        &["pane", "rename", &pane, "one\ttwo\nthree"],
        "cli:pane:rename",
        "pane",
    );
    assert_eq!(f.pane_option(&pane, "@agent_title"), "one two three");

    // And the listing the fold protects still reads back exactly one pane.
    let listed = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    assert_eq!(listed["panes"].as_array().unwrap().len(), 1, "{listed}");
}

#[test]
fn an_agent_session_id_that_could_reach_a_shell_is_rejected_at_the_write() {
    // `@agent_session_id` is interpolated into the command `session restore` types into a
    // pane with `send-keys … Enter`. A hook that reports `abc; rm -rf ~` would have that
    // run at the next restore, so the id is checked where it is written, not only where it
    // is used, and the pane keeps whatever valid id it already had.
    let f = Fixture::start();
    let pane = f.first_pane();

    f.ok(
        &[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            "good-id.1_2",
        ],
        "cli:pane:report-agent-session",
        "ok",
    );
    assert_eq!(f.pane_option(&pane, "@agent_session_id"), "good-id.1_2");

    for bad in [
        "abc; rm -rf /",
        "abc\nrm -rf /",
        "a b",
        "",
        &"x".repeat(129),
    ] {
        let (code, env) = f.run(&[
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            "test",
            "--agent",
            "claude",
            "--agent-session-id",
            bad,
        ]);
        assert_ne!(code, 0, "{bad:?} must be rejected: {env}");
        assert_eq!(env["error"]["code"], "invalid_argument", "{bad:?}: {env}");
    }
    assert_eq!(
        f.pane_option(&pane, "@agent_session_id"),
        "good-id.1_2",
        "a rejected id must not overwrite the one already stored"
    );

    // `report-agent` carries the same optional identity, through the same check.
    let (code, env) = f.run(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "test",
        "--agent",
        "claude",
        "--state",
        "working",
        "--agent-session-id",
        "abc;id",
    ]);
    assert_ne!(code, 0, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");
}

#[test]
fn manifest_update_if_stale_obeys_the_configured_manifest_check_switch() {
    // `--if-stale` is what the plugin script fires on every tmux server start, and it is
    // the only manifest fetch a user never asks for by hand. `[update] manifest_check =
    // false` is the documented way to say "never reach out on your own" — the watcher
    // gates its background refresh on it, and this verb must too, or turning it off
    // silences one caller and leaves the other fetching on every server start.
    let f = Fixture::start();
    std::fs::write(
        f.config.path().join("config.toml"),
        "[update]\nmanifest_check = false\n",
    )
    .unwrap();

    // The marker is deliberately absent, which is exactly what "stale" means here — so a
    // `checked: false` below can only have come from the config switch. (It is also why
    // this test asserts the switch's *off* position only: proving the on position would
    // mean letting a test in this suite reach herdr.dev.)
    let marker = f.state.path().join("agent-detection").join("last-check");
    std::fs::remove_file(&marker).unwrap();
    assert!(!marker.exists(), "precondition: no marker, so stale");

    let result = f.ok(
        &["manifest", "update", "--if-stale"],
        "cli:manifest:update",
        "manifest_update",
    );
    assert_eq!(result["checked"], false, "{result}");
    assert_eq!(result["outcomes"].as_array().unwrap().len(), 0, "{result}");

    // `update_if_stale` touches the marker before it fetches anything, so an untouched
    // marker is the evidence that nothing was even attempted.
    assert!(
        !marker.exists(),
        "no fetch may have been attempted, but the marker was touched"
    );
    assert!(
        !f.state
            .path()
            .join("agent-detection")
            .join("remote")
            .exists(),
        "nothing may have been written to the remote manifest cache"
    );
}

#[test]
fn installing_over_a_settings_json_of_the_wrong_shape_is_an_error_not_a_panic() {
    // `settings.json` belongs to the user and to Claude Code, not to `taw`: it is
    // hand-edited, written by other tools and carried between machines, so `{"hooks":
    // null}` and `{"hooks": {"SessionStart": {}}}` are shapes that turn up. Each must not
    // end `taw` in a Rust panic — no envelope, no exit code a caller could read, and a
    // stack trace on stderr.
    let f = Fixture::start();
    let claude_dir = f.work.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    let original = r#"{"hooks": {"SessionStart": {}}}"#;
    std::fs::write(&settings, original).unwrap();

    let args = ["integration", "install", "claude"];
    let (code, stdout, stderr) = f.run_with_env(
        &args,
        &[("CLAUDE_CONFIG_DIR", claude_dir.to_str().unwrap())],
    );
    let env = parse_envelope(&args, &stdout, &stderr);

    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");
    let message = env["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("hooks.SessionStart"),
        "the message must name the JSON path it choked on: {env}"
    );
    assert!(
        message.contains("settings.json"),
        "and the file it was reading: {env}"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "a file we could not understand must be left exactly as it was"
    );
}

/// A settings.json that was made read-only after taw installed its hook. The envelope
/// carries only an error's outermost message, and an uninstall must not wrap the refusal in
/// "reverting <file>", which would tell the person which file and nothing about why. The message
/// says what the install's says: which file, that it was left as it is, what to do.
/// The refusal changes nothing, and once the file is writable again the uninstall
/// finishes and gives the file back as it was.
#[cfg(unix)]
#[test]
fn uninstalling_over_a_read_only_settings_json_says_why_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::start();
    let claude_dir = f.work.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    let original = "{\n  \"theme\": \"dark\"\n}\n";
    std::fs::write(&settings, original).unwrap();
    // A scratch HOME as well as CLAUDE_CONFIG_DIR: nothing here may reach the real one.
    let scratch_env = [
        ("CLAUDE_CONFIG_DIR", claude_dir.to_str().unwrap()),
        ("HOME", f.work.path().to_str().unwrap()),
    ];
    let install = ["integration", "install", "claude"];
    let (code, stdout, stderr) = f.run_with_env(&install, &scratch_env);
    let installed_reply = parse_envelope(&install, &stdout, &stderr);
    assert_eq!(code, 0, "{installed_reply}");
    let installed = std::fs::read_to_string(&settings).unwrap();
    assert!(installed.contains("taw-agent-state.sh"), "{installed}");
    let script = claude_dir.join("hooks").join("taw-agent-state.sh");
    assert!(script.exists());

    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .write(true)
        .open(&settings)
        .is_ok()
    {
        eprintln!("skipping: this process can write a 0444 file (root), so the mode means nothing");
        return;
    }

    let uninstall = ["integration", "uninstall", "claude"];
    let (code, stdout, stderr) = f.run_with_env(&uninstall, &scratch_env);
    let refused = parse_envelope(&uninstall, &stdout, &stderr);

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["error"]["code"], "invalid_argument", "{refused}");
    assert_eq!(
        refused["error"]["message"],
        format!(
            "{} is read-only, so it was left as it is; make it writable and try again",
            settings.display()
        ),
        "the message must say why, not only which file: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        installed,
        "a refused uninstall must leave the file as it found it"
    );
    assert!(script.exists(), "and the hook script with it");

    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (code, stdout, stderr) = f.run_with_env(&uninstall, &scratch_env);
    let done = parse_envelope(&uninstall, &stdout, &stderr);
    assert_eq!(code, 0, "{done}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    assert!(!script.exists());
}

/// The user's Claude folder is reachable as `~/.claude-home` and as `~/.claude-alt`: the same
/// directory through links. An install made with `CLAUDE_CONFIG_DIR` spelled the one way wrote
/// its hook entry with that spelling, and an uninstall run with the other did not know the
/// entry for its own, left it in settings.json, and still deleted the hook script and the saved
/// copy of the file, so the entry then pointed at nothing and nothing could restore the file.
#[cfg(unix)]
#[test]
fn uninstalling_through_another_name_for_the_claude_folder_finds_taws_entry() {
    let f = Fixture::start();
    // A scratch HOME as well as CLAUDE_CONFIG_DIR: nothing here may reach the real one.
    let base = std::fs::canonicalize(f.work.path()).unwrap();
    let real = base.join("claude-alt");
    std::fs::create_dir(&real).unwrap();
    let link = base.join("claude-home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let settings = real.join("settings.json");
    let original = "{\"theme\":\"dark\"}";
    std::fs::write(&settings, original).unwrap();
    let env_for = |dir: &std::path::Path| {
        [
            ("CLAUDE_CONFIG_DIR", dir.to_str().unwrap().to_string()),
            ("HOME", base.to_str().unwrap().to_string()),
        ]
    };
    let run = |args: &[&str], dir: &std::path::Path| {
        let env = env_for(dir);
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (code, stdout, stderr) = f.run_with_env(args, &env);
        (code, parse_envelope(args, &stdout, &stderr))
    };

    let (code, installed) = run(&["integration", "install", "claude"], &link);
    assert_eq!(code, 0, "{installed}");
    let text = std::fs::read_to_string(&settings).unwrap();
    assert!(
        text.contains("claude-home/hooks/taw-agent-state.sh"),
        "the entry should carry the spelling it was installed through: {text}"
    );

    let (code, uninstalled) = run(&["integration", "uninstall", "claude"], &real);

    assert_eq!(code, 0, "{uninstalled}");
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "settings.json must come back byte for byte"
    );
    assert!(!real.join("hooks").join("taw-agent-state.sh").exists());
    let kept: Vec<_> = std::fs::read_dir(real.join("hooks"))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(kept.is_empty(), "left behind: {kept:?}");
}

/// A hook that names `taw-agent-state.sh` by a path this machine cannot resolve might be taw's,
/// from a folder that has since gone, or somebody else's. The uninstall cannot tell, so it says
/// which file and which path, with the error code a bad file has, and changes nothing: the hook
/// script and the saved copy stay, so that removing the hook by hand lets the uninstall finish.
#[cfg(unix)]
#[test]
fn uninstalling_over_a_hook_it_cannot_tell_from_its_own_names_the_file_and_changes_nothing() {
    let f = Fixture::start();
    let claude_dir = f.work.path().join("claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    let original = "{\"theme\":\"dark\"}";
    std::fs::write(&settings, original).unwrap();
    let scratch_env = [
        ("CLAUDE_CONFIG_DIR", claude_dir.to_str().unwrap()),
        ("HOME", f.work.path().to_str().unwrap()),
    ];
    let install = ["integration", "install", "claude"];
    let (code, stdout, stderr) = f.run_with_env(&install, &scratch_env);
    assert_eq!(code, 0, "{}", parse_envelope(&install, &stdout, &stderr));
    let stranger = "/folder-that-is-not-there/hooks/taw-agent-state.sh";
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
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
    std::fs::write(&settings, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let edited = std::fs::read(&settings).unwrap();
    let script = claude_dir.join("hooks").join("taw-agent-state.sh");

    let uninstall = ["integration", "uninstall", "claude"];
    let (code, stdout, stderr) = f.run_with_env(&uninstall, &scratch_env);
    let refused = parse_envelope(&uninstall, &stdout, &stderr);

    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["error"]["code"], "invalid_argument", "{refused}");
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&settings.display().to_string()) && message.contains(stranger),
        "the message must name the file and the path it could not match: {refused}"
    );
    assert_eq!(std::fs::read(&settings).unwrap(), edited);
    assert!(script.exists(), "the hook script must stay");

    // Taking the hook out by hand is what the message asks for, and then the uninstall finishes.
    value["hooks"]["SessionStart"].as_array_mut().unwrap().pop();
    std::fs::write(&settings, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let (code, stdout, stderr) = f.run_with_env(&uninstall, &scratch_env);
    let done = parse_envelope(&uninstall, &stdout, &stderr);
    assert_eq!(code, 0, "{done}");
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
    assert!(!script.exists());
}

/// An unknown but well-formed target must report a clean `not_found`, not leak a raw tmux
/// error (`workspace rename`/`close`, `tab create --workspace`) or silently succeed
/// (`plugin unlink`).
#[test]
fn unknown_targets_report_not_found_rather_than_leaking_or_succeeding() {
    let f = Fixture::start();

    // `w99` is a well-formed workspace id for a session that does not exist.
    for args in [
        vec!["workspace", "rename", "w99", "renamed"],
        vec!["workspace", "close", "w99"],
        vec!["tab", "create", "--workspace", "w99"],
        vec!["plugin", "unlink", "no-such-plugin"],
    ] {
        let (code, env) = f.run(&args);
        assert_ne!(code, 0, "`{}` should fail: {env}", args.join(" "));
        assert_eq!(
            env["error"]["code"],
            "not_found",
            "`{}` should report not_found: {env}",
            args.join(" ")
        );
    }
}

// The three "created" lifecycle events are not asserted here. They are hook-sourced
// (`session-created`, `window-linked`, `after-split-window`), and this file's
// `Fixture` deliberately never sources `tmux-agent-wizard.tmux` — it exercises the CLI
// alone — so no hook could fire for it. The coverage is in
// `tests/e2e_events.rs::a_creation_made_with_plain_tmux_fires_exactly_one_created_event`
// and `::the_creation_verbs_still_deliver_one_created_event_each_and_the_same_payload`,
// which source the real plugin script and additionally pin that each event is delivered
// exactly once.

#[test]
fn plugin_install_refuses_a_spec_that_would_escape_the_plugins_directory() {
    // Every segment of the spec is joined onto `<state>/plugins/github/`, so `..` walks
    // back out of it — and `plugin install` does not merely clone, it runs the plugin's
    // build steps in whatever directory it landed in.
    let f = Fixture::start();

    for escaping in ["../../x/y", "owner/repo/../../..", "owner/../repo"] {
        let args = ["plugin", "install", escaping];
        let (code, env) = f.run(&args);
        assert_ne!(code, 0, "{escaping:?} must be refused: {env}");
        assert_eq!(
            env["error"]["code"], "invalid_argument",
            "{escaping:?}: {env}"
        );
        assert!(
            env["error"]["message"]
                .as_str()
                .unwrap()
                .contains("plugins directory"),
            "the message should say why: {env}"
        );
    }

    // A bare `..` is refused one check earlier, for not being `owner/repo` at all — a
    // different message, the same code, and the same outcome.
    let (code, env) = f.run(&["plugin", "install", ".."]);
    assert_ne!(code, 0, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");

    // Nothing outside the state dir was created, and no checkout was made inside it.
    let github = f.state.path().join("plugins").join("github");
    assert!(
        !github.exists() || std::fs::read_dir(&github).unwrap().count() == 0,
        "a refused spec must not leave a checkout behind"
    );
}

#[test]
fn session_save_reports_how_many_panes_it_could_not_read() {
    // `list_panes` skips a row it cannot parse rather than failing the whole listing,
    // which is right for the rail — but `session save` *persists* that listing, so a
    // skipped pane is a pane missing from the snapshot and a `session restore` that
    // rebuilds a layout which never existed. The count is reported so the loss is
    // visible; it is not an error, because the same path runs unattended from autosave.
    let f = Fixture::start();

    let saved = f.ok(
        &["session", "save", "--name", "snap"],
        "cli:session:save",
        "session_saved",
    );
    assert_eq!(
        saved["skipped_panes"], 0,
        "a healthy server loses nothing: {saved}"
    );
    assert_eq!(saved["sessions"], 1, "{saved}");

    // The field survives the round trip through the file, so a restore can be told too.
    let listed = f.ok(&["session", "list"], "cli:session:list", "session_list");
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "snap"),
        "{listed}"
    );
}

#[test]
fn a_rejected_session_id_leaves_no_partial_write_behind() {
    // The validation has to come before the write it guards: otherwise `report-agent` has
    // already set `@agent_kind` and the reported state by the time it refuses the id, and the
    // caller is told its report failed while part of it was applied.
    let f = Fixture::start();
    let pane = f.first_pane();

    let (code, env) = f.run(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "test",
        "--agent",
        "claude",
        "--state",
        "working",
        "--agent-session-id",
        "abc; rm -rf /",
    ]);
    assert_ne!(code, 0, "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument", "{env}");

    for option in [
        "@agent_kind",
        "@agent_state",
        "@agent_state_since",
        "@agent_source",
        "@agent_session_id",
    ] {
        assert_eq!(
            f.pane_option(&pane, option),
            "",
            "{option} must be untouched by a refused report"
        );
    }

    // Same for `report-agent-session`, which writes `@agent_kind` first too.
    let (code, env) = f.run(&[
        "pane",
        "report-agent-session",
        &pane,
        "--source",
        "test",
        "--agent",
        "claude",
        "--agent-session-id",
        "bad id",
    ]);
    assert_ne!(code, 0, "{env}");
    assert_eq!(f.pane_option(&pane, "@agent_kind"), "");
    assert_eq!(f.pane_option(&pane, "@agent_reported_kind"), "");
}

#[test]
fn a_token_value_is_stored_with_its_control_characters_folded() {
    // `@agent_token_*` values come from a hook's `--token k=v` and are read back out of
    // `show-options -p`, whose output is one option per line — a newline in a value would
    // split one option into two lines that parse as neither.
    let f = Fixture::start();
    let pane = f.first_pane();
    f.ok(
        &[
            "pane",
            "report-metadata",
            &pane,
            "--source",
            "test",
            "--token",
            "note=first\nsecond",
            "--token",
            "cost=1\t25",
        ],
        "cli:pane:report-metadata",
        "ok",
    );

    // Read straight from tmux, so this is what was actually stored, not what a `taw`
    // read path chose to show.
    assert_eq!(f.pane_option(&pane, "@agent_token_note"), "first second");
    assert_eq!(f.pane_option(&pane, "@agent_token_cost"), "1 25");

    // `show-options -p` is line-oriented, and `store::read` parses tokens out of it — an
    // unfolded newline would have made one of these two options unreadable.
    let listed = f.ok(&["pane", "list"], "cli:pane:list", "pane_list");
    assert_eq!(listed["panes"].as_array().unwrap().len(), 1, "{listed}");
}

#[test]
fn session_save_counts_a_pane_it_could_not_read() {
    // The other half of `skipped_panes`: a healthy server proves the field exists, not
    // that it is ever anything but 0. This drives a real `session save` against a tmux
    // that appends one malformed row to every `list-panes` reply, which is exactly the
    // shape `parse_pane_lines` skips — so the count has to come out non-zero.
    //
    // The binary is swapped through `$TAW_TMUX_BIN` (which `Tmux::from_env` reads) rather
    // than a library seam, so this exercises the whole path the product uses.
    let f = Fixture::start();
    let shim = f.work.path().join("tmux-adding-a-bad-row");
    std::fs::write(
        &shim,
        // `list-panes` gets the real reply plus one row with the wrong field count;
        // everything else passes straight through.
        "#!/bin/sh\ncase \" $* \" in\n  *\" list-panes \"*)\n    tmux \"$@\" || exit $?\n    printf 'not\\ta\\tpane\\n'\n    exit 0\n    ;;\nesac\nexec tmux \"$@\"\n",
    )
    .unwrap();
    crate::common::make_executable(&shim);

    let args = ["session", "save", "--name", "lossy"];
    let (code, stdout, stderr) = f.run_with_env(&args, &[("TAW_TMUX_BIN", shim.to_str().unwrap())]);
    let env = parse_envelope(&args, &stdout, &stderr);

    assert_eq!(code, 0, "a bad row must not fail the save: {env}");
    assert_eq!(
        env["result"]["skipped_panes"], 1,
        "the pane that could not be read has to be counted: {env}"
    );
    // The save still happened, with the panes that *could* be read.
    assert_eq!(env["result"]["sessions"], 1, "{env}");
    assert!(
        f.state.path().join("sessions").join("lossy.json").is_file(),
        "the snapshot is still written — refusing would block autosave for ever"
    );
}

/// `taw sandbox new` and `taw sandbox list` print the usual envelope: what new made, and every
/// profile taw can run. A name that is taken is `invalid_argument`, and a `--from` that is no
/// profile `not_found`. Run with a scratch home and agent folders: nothing of the runner's is read.
#[test]
fn sandbox_new_and_list_print_their_envelopes() {
    let f = Fixture::start();
    let home = tempfile::tempdir().unwrap();
    let scratch: Vec<(String, String)> = [
        ("HOME", ""),
        ("CLAUDE_CONFIG_DIR", ".claude"),
        ("CODEX_HOME", ".codex"),
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_STATE_HOME", ".local/state"),
        ("XDG_CACHE_HOME", ".cache"),
        ("HISTFILE", "/dev/null"),
    ]
    .into_iter()
    .map(|(name, under)| {
        let value = if under.starts_with('/') {
            under.to_string()
        } else {
            home.path().join(under).display().to_string()
        };
        (name.to_string(), value)
    })
    .collect();
    let run = |args: &[&str]| {
        let env: Vec<(&str, &str)> = scratch
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let (code, stdout, stderr) = f.run_with_env(args, &env);
        (code, parse_envelope(args, &stdout, &stderr))
    };
    let ok = |args: &[&str], id: &str, result_type: &str| {
        let (code, env) = run(args);
        assert_eq!(code, 0, "`taw {}` failed: {env}", args.join(" "));
        assert_eq!(env["id"], id, "{env}");
        assert_eq!(env["result"]["type"], result_type, "{env}");
        env["result"].clone()
    };

    let made = ok(
        &["sandbox", "new", "mine"],
        "cli:sandbox:new",
        "sandbox_profile",
    );
    assert_eq!(made["name"], "mine");
    assert_eq!(made["from"], "default");
    let path = made["path"].as_str().unwrap().to_string();
    assert!(path.ends_with("/sandbox/mine"), "{made}");
    assert!(Path::new(&path).starts_with(f.config.path()), "{made}");

    let listed = ok(&["sandbox", "list"], "cli:sandbox:list", "sandbox_list");
    let names: Vec<&str> = listed["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "mine"], "{listed}");
    assert_eq!(listed["profiles"][1]["path"], path.as_str());
    assert_eq!(listed["profiles"][1]["yours"], true);
    assert_eq!(listed["profiles"][0]["yours"], false);
    assert_eq!(listed["profiles"][0]["default"], true);
    assert_eq!(listed["profiles"][0]["agents"], serde_json::json!([]));
    assert_eq!(listed["refused"], serde_json::json!([]), "{listed}");

    let (code, env) = run(&["sandbox", "new", "mine"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["id"], "cli:sandbox:new", "{env}");
    assert_eq!(env["error"]["code"], "invalid_argument");
    let (code, env) = run(&["sandbox", "new", "other", "--from", "team"]);
    assert_eq!(code, 1, "{env}");
    assert_eq!(env["error"]["code"], "not_found");
    assert!(!f.config.path().join("sandbox/other").exists());

    // With a profiles folder whose link is gone, nothing can run, and the list says why.
    let folder = f.config.path().join("sandbox");
    std::fs::remove_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(f.config.path().join("moved"), &folder).unwrap();
    let listed = ok(&["sandbox", "list"], "cli:sandbox:list", "sandbox_list");
    assert_eq!(listed["profiles"], serde_json::json!([]), "{listed}");
    assert_eq!(
        listed["refused"],
        serde_json::json!([{
            "name": "default",
            "why": format!(
                "your profiles folder {0} is a link to a folder that is not there (ls -ld {0})",
                folder.display()
            ),
        }]),
        "{listed}"
    );
    // Nothing was made in the scratch home either.
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["worktree", "a_group_close_asks_about_every_session"],
        "leave_the_one_gits_variables_point_at",
    );
}

/// `taw config check` is what a script, a pre-commit hook or a dotfiles CI runs, so its exit
/// status tells a broken file from a good one: 1 when it reports an error (a TOML syntax error,
/// a word that is not allowed, a number out of range), 0 for a clean file and for warnings alone.
/// It never tells you to run `taw config check`, which is what was just run.
#[test]
fn config_check_exits_1_on_an_error_and_0_on_a_warning_or_nothing() {
    let f = Fixture::start();
    let file = f.config.path().join("config.toml");

    std::fs::write(&file, "[ui]\ndepth = \"all\"\n").unwrap();
    let (checked, stderr) = f.config_check(0);
    assert_eq!(checked["errors"], 0, "{checked}");
    assert_eq!(stderr, "", "a clean file says nothing on stderr");

    std::fs::write(&file, "[ui]\nfrobnicate = 1\n").unwrap();
    let (checked, _) = f.config_check(0);
    assert_eq!(
        checked["errors"], 0,
        "an unknown key is a warning: {checked}"
    );
    assert_eq!(
        checked["diagnostics"].as_array().unwrap().len(),
        1,
        "{checked}"
    );

    for broken in [
        "[ui.toast]\ndelivery = \"loud\"\n",
        "this is = not = toml",
        "[keys]\npreset = \"vim\"\n",
        "[ui.toast]\ndelay_seconds = 99999\n",
    ] {
        std::fs::write(&file, broken).unwrap();
        let (checked, stderr) = f.config_check(1);
        assert!(
            checked["errors"].as_u64().unwrap() >= 1,
            "{broken:?}: {checked}"
        );
        assert!(
            !stderr.contains("taw config check"),
            "{broken:?}: the check tells you to run itself: {stderr}"
        );
    }
}

/// What `taw session candidates` is run against: a scratch `$HOME` with some folders, a fake
/// `zoxide` that lists three of them, and the fixture's config with `project_dirs` set.
struct Candidates {
    home: tempfile::TempDir,
    bin: tempfile::TempDir,
}

impl Candidates {
    /// `~/web`, `~/api`, `~/mobile` and `~/proj/{app,lib}`; the fake zoxide lists `~/web`,
    /// `~/api` and `~/mobile`, and answers `add` with nothing, as the real one does.
    fn new(f: &Fixture, with_zoxide: bool) -> Candidates {
        let home = tempfile::tempdir().unwrap();
        for dir in ["web", "api", "mobile", "proj/app", "proj/lib"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        let bin = tempfile::tempdir().unwrap();
        if with_zoxide {
            let list: String = ["web", "api", "mobile"]
                .iter()
                .map(|d| format!("echo '{}/{d}'\n", home.path().display()))
                .collect();
            write_hook(
                &bin.path().join("zoxide"),
                &format!("[ \"$1\" = add ] && exit 0\n{list}"),
            );
        }
        let text = format!(
            "[sessions]\nzoxide = {with_zoxide}\nproject_dirs = [\"{}\"]\n",
            home.path().join("proj").display()
        );
        std::fs::write(f.config.path().join("config.toml"), text).unwrap();
        Candidates { home, bin }
    }

    /// `taw <args>` with the scratch `$HOME`, the fake zoxide first on `PATH` (the real tmux after
    /// it), and `socket` as the tmux server to ask.
    fn run(&self, f: &Fixture, socket: &str, args: &[&str]) -> (i32, String, String) {
        self.run_env(f, socket, &[], args)
    }

    /// [`Candidates::run`] with more environment, such as `TAW_TMUX_BIN` for a stand-in tmux.
    fn run_env(
        &self,
        f: &Fixture,
        socket: &str,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> (i32, String, String) {
        let path = format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_taw"));
        cmd.args(args)
            .env("HOME", self.home.path())
            .env("PATH", path)
            .env("TAW_TMUX_SOCKET_NAME", socket)
            .env("TAW_CONFIG_DIR", f.config.path())
            .env("TAW_STATE_DIR", f.state.path())
            .env("_ZO_DATA_DIR", f.zoxide.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        cmd.envs(env.iter().copied());
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn dir(&self, name: &str) -> String {
        self.home.path().join(name).display().to_string()
    }
}

/// The candidates are the picker's list: zoxide's folders in zoxide's order, then the folders under
/// `project_dirs`, one row each. A folder with a session running in it (`~/api`) is listed where the picker lists
/// it, as `session`, with the session's name.
#[test]
fn session_candidates_lists_what_the_picker_lists() {
    let f = Fixture::start();
    let c = Candidates::new(&f, true);
    let args = ["session", "open", &c.dir("api")];
    let (code, stdout, stderr) = c.run(&f, &f.socket, &args);
    assert_eq!(code, 0, "{stdout} {stderr}");
    let session = parse_envelope(&args, &stdout, &stderr)["result"]["session"]
        .as_str()
        .unwrap()
        .to_string();

    let args = ["session", "candidates", "--json"];
    let (code, stdout, stderr) = c.run(&f, &f.socket, &args);
    assert_eq!(code, 0, "{stderr}");
    let env = parse_envelope(&args, &stdout, &stderr);
    assert_eq!(env["id"], "cli:session:candidates", "{env}");
    assert_eq!(env["result"]["type"], "session_candidates", "{env}");
    let row = |kind: &str, name: &str, dir: &str, session: Option<&str>| {
        let mut row = serde_json::json!({ "kind": kind, "name": name, "dir": c.dir(dir) });
        if let Some(session) = session {
            row["session"] = serde_json::json!(session);
        }
        row
    };
    assert_eq!(
        env["result"]["candidates"],
        serde_json::json!([
            row("zoxide", "~/web", "web", None),
            row("session", "~/api", "api", Some(&session)),
            row("zoxide", "~/mobile", "mobile", None),
            row("project", "~/proj/app", "proj/app", None),
            row("project", "~/proj/lib", "proj/lib", None),
        ]),
        "{env}"
    );
}

/// A zoxide history longer than its cap does not push the project folders out: the two sources are
/// capped apart, so the far picker can still find a project folder by typing, and `truncated` says
/// that a row was cut. A short list has no `truncated` key.
#[test]
fn session_candidates_with_a_long_zoxide_history_still_lists_the_project_folders() {
    let f = Fixture::start();
    let c = Candidates::new(&f, false);
    std::fs::write(
        f.config.path().join("config.toml"),
        format!(
            "[sessions]\nzoxide = true\nproject_dirs = [\"{}\"]\n",
            c.home.path().join("proj").display()
        ),
    )
    .unwrap();
    let list: String = (0..1100)
        .map(|n| format!("echo '{}/old/z{n}'\n", c.home.path().display()))
        .collect();
    write_hook(
        &c.bin.path().join("zoxide"),
        &format!("[ \"$1\" = add ] && exit 0\n{list}"),
    );

    let args = ["session", "candidates", "--json"];
    let (code, stdout, stderr) = c.run(&f, &f.socket, &args);
    assert_eq!(code, 0, "{stderr}");
    let env = parse_envelope(&args, &stdout, &stderr);
    let rows = env["result"]["candidates"].as_array().unwrap();
    let kind = |kind: &str| rows.iter().filter(|row| row["kind"] == kind).count();
    assert_eq!(kind("zoxide"), 1000, "zoxide rows");
    assert_eq!(kind("project"), 2, "project rows: {rows:?}");
    assert_eq!(
        env["result"]["truncated"], true,
        "{}",
        env["result"]["truncated"]
    );

    // Short lists carry no `truncated` key.
    let short = Candidates::new(&f, true);
    let (code, stdout, stderr) = short.run(&f, &f.socket, &args);
    assert_eq!(code, 0, "{stderr}");
    let env = parse_envelope(&args, &stdout, &stderr);
    assert!(env["result"].get("truncated").is_none(), "{env}");
}

/// No zoxide, no project folders and no server are an empty answer, not an error.
#[test]
fn session_candidates_with_nothing_to_offer_is_an_empty_list() {
    let f = Fixture::start();
    let c = Candidates::new(&f, false);
    std::fs::write(
        f.config.path().join("config.toml"),
        "[sessions]\nzoxide = true\n",
    )
    .unwrap();
    // `PATH` holds tmux alone: there is no `zoxide` to find.
    let tmux = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v tmux"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(tmux.trim(), c.bin.path().join("tmux")).unwrap();
    let only = |socket: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_taw"));
        cmd.args(["session", "candidates", "--json"])
            .env("HOME", c.home.path())
            .env("PATH", c.bin.path())
            .env("TAW_TMUX_SOCKET_NAME", socket)
            .env("TAW_CONFIG_DIR", f.config.path())
            .env("TAW_STATE_DIR", f.state.path())
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };

    // A server that answers, then one that is not running.
    for socket in [f.socket.clone(), format!("{}-none", f.socket)] {
        let (code, stdout) = only(&socket);
        assert_eq!(code, 0, "{socket}: {stdout}");
        let env: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            env["result"]["candidates"],
            serde_json::json!([]),
            "{socket}: {env}"
        );
    }
}

/// A tmux server that is not running still lists the folders: they are not its.
#[test]
fn session_candidates_without_a_server_lists_the_folders() {
    let f = Fixture::start();
    let c = Candidates::new(&f, true);
    let socket = format!("{}-none", f.socket);
    let args = ["session", "candidates", "--json"];
    let (code, stdout, stderr) = c.run(&f, &socket, &args);
    assert_eq!(code, 0, "{stderr}");
    let env = parse_envelope(&args, &stdout, &stderr);
    let kinds: Vec<&str> = env["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["zoxide", "zoxide", "zoxide", "project", "project"],
        "{env}"
    );
}

/// A tmux that fails for any reason but "no server" still lists the folders, none as `session`, and
/// says why in `sessions_error`; "no server running" and a missing socket say nothing.
#[test]
fn session_candidates_says_when_tmux_fails() {
    let f = Fixture::start();
    let c = Candidates::new(&f, true);
    let args = ["session", "candidates", "--json"];
    let answers = |stderr: &str| {
        let tmux = c.bin.path().join("tmux-stand-in");
        write_hook(&tmux, &format!("echo '{stderr}' >&2\nexit 1"));
        let tmux = tmux.display().to_string();
        let (code, stdout, err) = c.run_env(&f, &f.socket, &[("TAW_TMUX_BIN", &tmux)], &args);
        assert_eq!(code, 0, "{err}");
        parse_envelope(&args, &stdout, &err)
    };

    let env = answers("protocol version mismatch (client 8, server 7)");
    assert_eq!(
        env["result"]["sessions_error"], "protocol version mismatch (client 8, server 7)",
        "{env}"
    );
    let kinds: Vec<&str> = env["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["zoxide", "zoxide", "zoxide", "project", "project"],
        "{env}"
    );

    for said in [
        "no server running on /tmp/tmux-501/x",
        "error connecting to /tmp/tmux-501/x (No such file or directory)",
    ] {
        let env = answers(said);
        assert!(
            env["result"].get("sessions_error").is_none(),
            "{said}: {env}"
        );
    }
}

/// A tmux that does not answer within the bound is cut off and reaped: the verb answers with the
/// folders and a `sessions_error`, and no tmux is left running.
#[test]
fn session_candidates_does_not_wait_for_a_tmux_that_hangs() {
    let f = Fixture::start();
    let c = Candidates::new(&f, true);
    let tmux = c.bin.path().join("tmux-hangs");
    let pid_file = c.bin.path().join("tmux.pid");
    write_hook(
        &tmux,
        &format!("echo $$ > '{}'\nexec sleep 60", pid_file.display()),
    );
    let tmux = tmux.display().to_string();
    let args = ["session", "candidates", "--json"];
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = c.run_env(&f, &f.socket, &[("TAW_TMUX_BIN", &tmux)], &args);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "waited for the hang"
    );
    let env = parse_envelope(&args, &stdout, &stderr);
    let error = env["result"]["sessions_error"].as_str().unwrap_or_default();
    assert!(error.contains("did not answer"), "{env}");
    assert_eq!(
        env["result"]["candidates"].as_array().unwrap().len(),
        5,
        "{env}"
    );

    // The stand-in is gone: it was killed and reaped, not left behind.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap();
    assert!(!alive.success(), "tmux {pid} is still running");
}

/// Without `--json` it is a line for each folder, its kind and name, and no envelope.
#[test]
fn session_candidates_without_json_is_a_line_for_each_folder() {
    let f = Fixture::start();
    let c = Candidates::new(&f, true);
    let args = ["session", "open", &c.dir("api")];
    let (code, stdout, stderr) = c.run(&f, &f.socket, &args);
    assert_eq!(code, 0, "{stdout} {stderr}");
    let session = parse_envelope(&args, &stdout, &stderr)["result"]["session"]
        .as_str()
        .unwrap()
        .to_string();

    let (code, stdout, stderr) = c.run(&f, &f.socket, &["session", "candidates"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "zoxide   ~/web\nsession  ~/api [{session}]\nzoxide   ~/mobile\n\
             project  ~/proj/app\nproject  ~/proj/lib\n"
        )
    );
}

#[test]
fn session_candidates_help_says_what_it_lists() {
    let f = Fixture::start();
    let (code, stdout, _) = f.run_raw(&["session", "candidates", "--help"]);
    assert_eq!(code, 0);
    for word in ["picker", "zoxide", "project_dirs", "--json"] {
        assert!(stdout.contains(word), "no `{word}` in:\n{stdout}");
    }
}
