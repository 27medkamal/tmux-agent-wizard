//! Integration tests for the herdr-compatible plugin system against a real (throwaway)
//! tmux server: links `tests/fixtures/plugins/echo-plugin`, invokes its action, emits a
//! matching event, and opens/closes its plugin pane. Never touches the default tmux
//! server, and never writes outside tempdirs and this throwaway server.

mod common;

use common::shell_env::ShellHome;
use taw::config::Config;
use taw::paths::Paths;
use taw::plugin::{self, PluginCtx, actions, context, events, panes};
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. See
/// tests/tmux_client.rs for why the socket name includes both the pid and a per-`Server`
/// counter.
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-plugin-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        // `-f /dev/null`: start clean, independent of the host's tmux.conf — see
        // tests/tmux_client.rs for the full rationale.
        home.start_server(&t, &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"]);
        // `/bin/sh` here *and* as `default-shell` below: `-f /dev/null` isolates tmux's
        // config but not the *shell's*. The host login shell may print banners, run prompt
        // tools, or open an interactive prompt of its own — in this repo, `mise`'s "trust
        // this config?" question in a fresh worktree — which makes any test that types into
        // a pane flaky for reasons that have nothing to do with `taw`. `/bin/sh` reads no rc
        // file. It has to be named twice because the server does not outlive a
        // `start-server` with no sessions, so there is no earlier moment at which to set the
        // option for this first pane. (Shared with `tests/common/mod.rs`, which does the
        // same for the `e2e_*` files.)
        t.run_ok(&["set-option", "-g", "default-shell", "/bin/sh"])
            .unwrap();
        Server(t, home)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.run_ok(&["kill-server"]);
    }
}

/// A `Paths` rooted entirely inside a tempdir — every field is independent of the real
/// `$TAW_CONFIG_DIR`/`$TAW_STATE_DIR`, so these tests never write outside it.
fn test_paths(dir: &std::path::Path) -> Paths {
    let plugins_dir = dir.join("plugins");
    Paths {
        config_dir: dir.join("config"),
        config_file: dir.join("config").join("config.toml"),
        state_dir: dir.join("state"),
        override_dir: dir.join("config").join("agent-detection"),
        remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
        sessions_dir: dir.join("state").join("sessions"),
        plugin_state_dir: plugins_dir.join("state"),
        plugins_dir,
        logs_dir: dir.join("state").join("logs"),
        manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
    }
}

fn fixture_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/plugins/echo-plugin"
    ))
}

/// Polls `f` every 20ms until it returns `true` or `timeout` elapses; returns whether it
/// ever returned `true`. Used for effects of detached/async processes (event hooks,
/// `kill-pane`) that do not complete synchronously with the call that triggers them.
fn poll_until(timeout: std::time::Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn links_fixture_plugin_and_invokes_its_action_with_the_documented_env() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };

    let record = plugin::link(&ctx, &fixture_dir(), true).unwrap();
    assert_eq!(record.id, "echo-plugin");
    assert_eq!(plugin::list(&ctx).unwrap().len(), 1);

    let pane_info = server.0.list_panes().unwrap().remove(0);
    let public_pane_id = taw::tmux::ids::pane_id(&pane_info.session_id, &pane_info.pane_id);
    let invoke_context = plugin::Context {
        pane: Some(context::pane_json(&pane_info)),
        ..Default::default()
    };

    let result = actions::invoke(&ctx, "echo-plugin.dump-env", &invoke_context).unwrap();
    assert_eq!(result.status, 0, "action failed, stderr: {}", result.stderr);

    let env_path = paths.plugin_state_dir.join("echo-plugin").join("env.txt");
    let contents = std::fs::read_to_string(&env_path).expect("env.txt should have been written");

    assert!(
        contents.contains(&format!("HERDR_BIN_PATH={}", taw_bin.display())),
        "expected HERDR_BIN_PATH={} in:\n{contents}",
        taw_bin.display()
    );
    assert!(
        std::path::Path::new(taw_bin.to_str().unwrap()).is_absolute(),
        "the test binary path itself must be absolute"
    );
    assert!(contents.contains("HERDR_ENV=1"), "{contents}");
    assert!(
        contents.contains("HERDR_PLUGIN_ID=echo-plugin"),
        "{contents}"
    );
    assert!(
        contents.contains(&format!("HERDR_PANE_ID={public_pane_id}")),
        "expected HERDR_PANE_ID={public_pane_id} in:\n{contents}"
    );
    assert!(
        !contents.contains("HERDR_SOCKET_PATH"),
        "HERDR_SOCKET_PATH must never be set: {contents}"
    );
    // Sanity: the public pane id really is w<N>:p<M> shaped.
    assert!(
        regex_like_pane_id(&public_pane_id),
        "public pane id '{public_pane_id}' is not w<N>:p<M> shaped"
    );
}

#[test]
fn an_action_invoked_on_a_pane_gets_its_workspace_and_tab_and_herdrs_flat_context() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();

    let pane_info = server.0.list_panes().unwrap().remove(0);
    let pane = taw::tmux::ids::pane_id(&pane_info.session_id, &pane_info.pane_id);
    let workspace = taw::tmux::ids::workspace_id(&pane_info.session_id);
    let tab = taw::tmux::ids::tab_id(&pane_info.session_id, &pane_info.window_id);
    // Only the pane is named, the way `taw plugin action invoke ID --pane P` does.
    let invoke_context = context::from_ids(&ctx, None, None, Some(&pane)).unwrap();
    let result = actions::invoke(&ctx, "echo-plugin.dump-env", &invoke_context).unwrap();
    assert_eq!(result.status, 0, "action failed, stderr: {}", result.stderr);

    let state = paths.plugin_state_dir.join("echo-plugin");
    let env = std::fs::read_to_string(state.join("env.txt")).unwrap();
    for line in [
        format!("HERDR_PANE_ID={pane}"),
        format!("HERDR_TAB_ID={tab}"),
        format!("HERDR_WORKSPACE_ID={workspace}"),
    ] {
        assert!(env.lines().any(|l| l == line), "no {line} in:\n{env}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state.join("context.json")).unwrap())
            .unwrap();
    assert_eq!(json["workspace_id"], workspace.as_str(), "{json}");
    assert_eq!(json["tab_id"], tab.as_str(), "{json}");
    assert_eq!(json["focused_pane_id"], pane.as_str(), "{json}");
    assert!(json["focused_pane_cwd"].is_string(), "{json}");
    assert!(json["workspace_cwd"].is_string(), "{json}");
    for key in [
        "selected_text",
        "clicked_url",
        "link_handler_id",
        "invocation_source",
        "correlation_id",
        "worktree",
        "focused_pane_agent",
    ] {
        assert!(json.get(key).is_some_and(|v| v.is_null()), "{key}: {json}");
    }
    // taw's nested records stay beside the flat fields.
    assert_eq!(json["pane"]["pane_id"], pane.as_str());
    assert_eq!(json["workspace"]["workspace_id"], workspace.as_str());
    assert_eq!(json["tab"]["tab_id"], tab.as_str());

    // A tab names its workspace.
    let tab_context = context::from_ids(&ctx, None, Some(&tab), None).unwrap();
    assert_eq!(
        tab_context.workspace.as_ref().unwrap()["workspace_id"],
        workspace.as_str()
    );
}

/// herdr's `plugin pane open` flags reach tmux: `--placement tab` makes a window of its own and, with no
/// `--focus`, leaves the current window current; `--placement split --direction down` splits the window
/// (the pane below), and `--focus` makes the new pane current.
#[test]
fn herdrs_pane_open_flags_choose_the_tmux_placement_and_focus() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();
    let first = server.0.list_panes().unwrap().remove(0);
    let context = context::from_ids(&ctx, None, None, Some(&first.pane_id)).unwrap();

    let tab = panes::open_with(
        &ctx,
        "echo-plugin.board",
        &context,
        &panes::OpenOptions {
            placement: Some("tab".to_string()),
            direction: None,
            env: vec![(
                "GITHUB_URL".to_string(),
                "https://example.com/1".to_string(),
            )],
            focus: Some(false),
            ..panes::OpenOptions::default()
        },
    )
    .unwrap();
    let windows = server.0.list_windows().unwrap();
    assert_eq!(windows.len(), 2, "no window of its own: {windows:?}");
    let active: Vec<_> = server
        .0
        .list_panes()
        .unwrap()
        .into_iter()
        .filter(|p| p.pane_active && p.window_active)
        .collect();
    assert_eq!(
        active[0].pane_id, first.pane_id,
        "the new tab took the focus"
    );
    assert!(
        server
            .0
            .list_panes()
            .unwrap()
            .iter()
            .any(|p| p.pane_id == tab),
        "{tab} is not a pane"
    );

    let split = panes::open_with(
        &ctx,
        "echo-plugin.board",
        &context,
        &panes::OpenOptions {
            placement: Some("split".to_string()),
            direction: Some("down".to_string()),
            env: Vec::new(),
            focus: Some(true),
            ..panes::OpenOptions::default()
        },
    )
    .unwrap();
    let panes_now = server.0.list_panes().unwrap();
    let new = panes_now
        .iter()
        .find(|p| p.pane_id == split)
        .expect("the split pane");
    let old = panes_now
        .iter()
        .find(|p| p.pane_id == first.pane_id)
        .unwrap();
    assert_eq!(new.window_id, old.window_id, "not in the same window");
    let top = |pane: &str| -> u32 {
        server
            .0
            .display(pane, "#{pane_top}")
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    assert!(top(&new.pane_id) > top(&old.pane_id), "not below it");
    assert!(new.pane_active, "--focus left the new pane unfocused");
}

fn regex_like_pane_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('w') else {
        return false;
    };
    let Some((session, pane)) = rest.split_once(":p") else {
        return false;
    };
    !session.is_empty()
        && session.chars().all(|c| c.is_ascii_digit())
        && !pane.is_empty()
        && pane.chars().all(|c| c.is_ascii_digit())
}

#[test]
fn emits_matching_event_and_the_fixture_writes_readable_context_json() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();

    let pane_info = server.0.list_panes().unwrap().remove(0);
    let payload = serde_json::json!({
        "pane": context::pane_json(&pane_info),
        "state": "blocked",
    });

    let launched = events::emit(&ctx, "pane.agent_status_changed", payload).unwrap();
    assert_eq!(launched, 1);

    let event_path = paths
        .plugin_state_dir
        .join("echo-plugin")
        .join("event.json");
    let appeared = poll_until(std::time::Duration::from_secs(3), || {
        std::fs::metadata(&event_path).is_ok_and(|m| m.len() > 0)
    });
    assert!(
        appeared,
        "event.json never appeared at {}",
        event_path.display()
    );

    let contents = std::fs::read_to_string(&event_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("event.json did not parse as JSON: {e}\ncontents: {contents}"));
    assert_eq!(parsed["state"], "blocked");
    assert_eq!(parsed["pane"]["terminal_id"], pane_info.pane_id);
}

#[test]
fn opens_a_plugin_pane_as_a_new_tmux_pane_and_close_removes_it() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();

    let before = server.0.list_windows().unwrap();
    assert_eq!(
        before[0].window_panes, 1,
        "fresh server should start with one pane"
    );

    let pane_id = panes::open(&ctx, "echo-plugin.board", &plugin::Context::default()).unwrap();
    assert!(
        pane_id.starts_with('%'),
        "expected a tmux pane id, got '{pane_id}'"
    );

    let after_open = server.0.list_windows().unwrap();
    assert_eq!(
        after_open[0].window_panes, 2,
        "open should have split a second pane into the window"
    );

    panes::close(&ctx, "echo-plugin.board").unwrap();

    let closed = poll_until(std::time::Duration::from_secs(3), || {
        server
            .0
            .list_windows()
            .map(|w| w[0].window_panes)
            .unwrap_or(2)
            == 1
    });
    assert!(closed, "close should have removed the plugin pane");
}

/// Closing a pane a plugin opened (`taw plugin pane close`) is a close taw makes and does not ask
/// `[hooks] pre_close` about, which the README says: a hook that refuses every close does not
/// keep the pane open.
#[test]
fn a_plugin_pane_is_closed_without_asking_pre_close() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg =
        toml::from_str::<Config>("[hooks]\npre_close = \"echo 'keep it' >&2; exit 1\"\n").unwrap();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();
    panes::open(&ctx, "echo-plugin.board", &plugin::Context::default()).unwrap();
    assert_eq!(server.0.list_windows().unwrap()[0].window_panes, 2);

    panes::close(&ctx, "echo-plugin.board").unwrap();

    let closed = poll_until(std::time::Duration::from_secs(3), || {
        server
            .0
            .list_windows()
            .map(|w| w[0].window_panes)
            .unwrap_or(2)
            == 1
    });
    assert!(closed, "pre_close kept the plugin's pane open");
}

#[test]
fn focus_and_close_recover_cleanly_from_a_pane_closed_outside_taw() {
    let server = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let paths = test_paths(tmp.path());
    let cfg = Config::default();
    let taw_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_taw"));
    let ctx = PluginCtx {
        tmux: &server.0,
        cfg: &cfg,
        paths: &paths,
        taw_bin: &taw_bin,
        server_env: None,
    };
    plugin::link(&ctx, &fixture_dir(), true).unwrap();

    // --- focus() on a stale entry: must report not_found and forget the entry, not error
    // forever on every subsequent call. ---
    let pane_id = panes::open(&ctx, "echo-plugin.board", &plugin::Context::default()).unwrap();
    server.0.run_ok(&["kill-pane", "-t", &pane_id]).unwrap();
    let gone = poll_until(std::time::Duration::from_secs(3), || {
        server
            .0
            .list_panes()
            .map(|panes| !panes.iter().any(|p| p.pane_id == pane_id))
            .unwrap_or(false)
    });
    assert!(gone, "manually killed pane should actually be gone");

    let focus_err = panes::focus(&ctx, "echo-plugin.board").unwrap_err();
    assert!(
        focus_err.to_string().contains("not_found"),
        "expected a not_found-style error from focus() on a stale pane, got: {focus_err}"
    );
    // The stale entry must have been forgotten: a fresh open() works normally afterward,
    // proving the entrypoint isn't permanently broken.
    let reopened_pane_id =
        panes::open(&ctx, "echo-plugin.board", &plugin::Context::default()).unwrap();
    assert!(reopened_pane_id.starts_with('%'));
    assert_ne!(
        reopened_pane_id, pane_id,
        "should be a genuinely new pane, not the stale id"
    );

    // --- close() on a stale entry: must succeed (already-closed is the desired end state)
    // and forget the entry, not error forever. ---
    server
        .0
        .run_ok(&["kill-pane", "-t", &reopened_pane_id])
        .unwrap();
    let gone_again = poll_until(std::time::Duration::from_secs(3), || {
        server
            .0
            .list_panes()
            .map(|panes| !panes.iter().any(|p| p.pane_id == reopened_pane_id))
            .unwrap_or(false)
    });
    assert!(gone_again, "manually killed pane should actually be gone");

    panes::close(&ctx, "echo-plugin.board").unwrap();
    // Calling close() again on the now-forgotten (never-open) entry must still be a clean
    // no-op, not an error either.
    panes::close(&ctx, "echo-plugin.board").unwrap();
}

#[test]
fn workspace_records_carry_the_branch_of_the_session_path() {
    // herdr's `workspace` record has a `branch` field, and every `taw` workspace record
    // reported it as `null` — so a plugin or an agent reading `workspace list` could not
    // tell which branch a workspace was on, even though the TUI's Spaces panel shows
    // exactly that, from the same `git.rs`.
    let s = Server::start();
    let t = &s.0;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = taw::git::command()
            .arg("-C")
            .arg(&repo)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q", "-b", "trunk"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
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

    let plain = tmp.path().join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();

    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "in-repo",
        "-c",
        repo.to_str().unwrap(),
    ])
    .unwrap();
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "no-repo",
        "-c",
        plain.to_str().unwrap(),
    ])
    .unwrap();

    let panes = t.list_panes().unwrap();
    let sessions = t.list_sessions().unwrap();
    let workspace = |name: &str| {
        let session = sessions
            .iter()
            .find(|sess| sess.session_name == name)
            .unwrap_or_else(|| panic!("session '{name}' not found"));
        context::workspace_json(session, &panes)
    };

    assert_eq!(
        workspace("in-repo")["branch"],
        "trunk",
        "a workspace inside a repository must name its branch"
    );
    assert_eq!(
        workspace("no-repo")["branch"],
        serde_json::Value::Null,
        "a workspace outside a repository has no branch, and that is not an error"
    );

    // A detached HEAD is a repository with no branch name, which is `null` too — not the
    // literal "HEAD" that `git rev-parse --abbrev-ref` answers with.
    let head = taw::git::command()
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    git(&["checkout", "-q", "--detach", &head]);
    assert_eq!(
        workspace("in-repo")["branch"],
        serde_json::Value::Null,
        "a detached HEAD has no branch to report"
    );
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a repository, run again
/// with each of the variables pointing into a decoy, pass and leave it byte for byte as it was
/// (tests/common/git_decoy.rs).
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &["workspace_records_carry_the_branch_of_the_session_path"],
        "leave_the_one_gits_variables_point_at",
    );
}
