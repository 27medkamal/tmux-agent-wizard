//! Hot reload, end to end: a live `taw watch run` process picking up an edited
//! `config.toml` and edited manifest overrides without being restarted.
//!
//! A watcher is started by `pipe-pane` and lives as long as its pane, so "restart it" is
//! not an answer — the whole point is that a config edit reaches watchers that are
//! already running. Two triggers are covered here, the file's own mtime and the
//! `taw config reload` / `taw manifest reload` verbs, and the last test proves the
//! reloaded settings actually *change what the watcher does*, not merely that it re-read
//! the file.
//!
//! Every server here runs the fake agent in the watched pane. That is not incidental: a
//! pane with no agent in it is deliberately never read unless it prints (it is what makes
//! an idle pane free), so such a watcher only checks for a reload when the pane produces
//! output. With an agent present a quiet pane is still read every 5 s, by the identity
//! recheck, which is what makes these tests deterministic instead of waiting on the pane
//! to print something.

mod common;

use std::sync::Mutex;
use std::time::Duration;

use common::{FakeAgent, NotifyRecorder, Server, ServerOptions, wait_for};

/// Serialises these tests: each holds a tmux server, a real fake agent and a real watcher
/// process, and run concurrently they contend hard enough to reach the poll ceilings
/// below. (Same reason `tests/e2e_watch.rs` does it.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Ceiling for "the watcher's next read happened and it logged the reload".
///
/// The real latency is at most one identity recheck, 5 s after the last read. This is a
/// ceiling, not a sleep, and it is well above
/// that on purpose: `cargo test` runs every integration file in parallel, each holding
/// its own tmux server and real watcher processes, and a ceiling tuned to an idle machine
/// is how a suite becomes flaky.
const SETTLE: Duration = Duration::from_secs(20);

/// Ceiling for the fake agent starting, drawing, and being classified.
const AGENT: Duration = Duration::from_secs(20);

const CONFIG_OFF: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The watcher's own log file, `<state>/logs/watch-<pane>.log`.
fn watcher_log(server: &Server, pane: &str) -> String {
    let safe: String = pane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '%' | '@' | '$' | '_' | '-' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    std::fs::read_to_string(
        server
            .state_dir()
            .join("logs")
            .join(format!("watch-{safe}.log")),
    )
    .unwrap_or_default()
}

fn wait_for_log(server: &Server, pane: &str, needle: &str) -> bool {
    wait_for(|| watcher_log(server, pane).contains(needle), SETTLE)
}

/// A server with the fake agent running in its first pane and a watcher attached, so the
/// watcher ticks. The tempdir holding the fake's wrapper has to outlive the server, so it
/// is kept here.
struct Watched {
    server: Server,
    fake: FakeAgent,
    pane: String,
    _tmp: tempfile::TempDir,
}

fn watched_agent(config: &str, extra_env: Vec<(String, String)>) -> Watched {
    let tmp = tempfile::tempdir().unwrap();
    let fake = FakeAgent::new(tmp.path());
    // `TAW_LOG` goes on the *server*, so the `pipe-pane` child inherits it: the watcher is
    // a child of the tmux server, not of any `taw` invocation.
    let mut env = vec![("TAW_LOG".to_string(), "info".to_string())];
    env.extend(extra_env);
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(fake.bin_dir.clone()),
        env,
        ..ServerOptions::default()
    });
    server.write_config(config);
    let pane = server.pane();
    // The fake is a script, so tmux names its interpreter and not Claude: the watcher is what says
    // the agent is there, and `agent start` returns only once it has.
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    server.ok(
        &["agent", "start", "rev", "--kind", "claude", "--pane", &pane],
        "cli:agent:start",
        "agent",
    );
    fake.wait_until_drawn(&server, &pane, AGENT);
    assert!(
        wait_for(|| !watcher_log(&server, &pane).is_empty(), SETTLE),
        "the watcher never wrote a log line; is TAW_LOG reaching it?"
    );
    // Wait for the watcher's *first evaluation* to have finished, not merely for it to
    // have started logging.
    //
    // `watch attach` opens the `pipe-pane` and returns; the watcher is a child process
    // that gets to work in its own time, and it records the config's mtime as its
    // baseline a few steps *after* its first log line. A test that edited the config in
    // that window would have the edit baked into the baseline, with nothing left to
    // detect — which is exactly how this flaked before. `@agent_state` is only written by
    // an evaluation, so seeing it means the baseline is already taken.
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_state") == "idle", AGENT),
        "the watcher never classified the agent (state {:?}); log:\n{}",
        server.pane_opt(&pane, "@agent_state"),
        watcher_log(&server, &pane)
    );
    Watched {
        server,
        fake,
        pane,
        _tmp: tmp,
    }
}

#[test]
fn a_live_watcher_re_reads_its_config_its_manifest_overrides_and_the_remote_cache() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let w = watched_agent(CONFIG_OFF, Vec::new());
    let (server, pane) = (&w.server, w.pane.as_str());
    assert!(
        !watcher_log(server, pane).contains("config changed"),
        "the watcher reported a reload before anything changed:\n{}",
        watcher_log(server, pane)
    );

    // --- mtime: the file itself changed ---
    server.write_config(&format!("{CONFIG_OFF}\n[ui]\nconfirm_close = false\n"));
    assert!(
        wait_for_log(server, pane, "config changed"),
        "the watcher never re-read the edited config:\n{}",
        watcher_log(server, pane)
    );

    // --- mtime: a manifest override appeared ---
    let overrides = server.config_dir().join("agent-detection");
    std::fs::create_dir_all(&overrides).unwrap();
    std::fs::write(overrides.join("claude.toml"), "id = \"claude\"\n").unwrap();
    assert!(
        wait_for_log(server, pane, "agent manifest overrides changed"),
        "the watcher never noticed the new override:\n{}",
        watcher_log(server, pane)
    );

    // --- mtime: a newer copy landed in the remote cache ---
    // Another watcher's update check, or `taw manifest update`, fetched it. This watcher
    // fetched nothing itself and reloads it all the same, as herdr reloads a cached copy
    // that changed on disk (`agents_needing_cache_reload`).
    let reloads = || {
        watcher_log(server, pane)
            .matches("cached remote copies did")
            .count()
    };
    let before = reloads();
    let remote = server.state_dir().join("agent-detection").join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::write(
        remote.join("codex.toml"),
        include_str!("../src/detect/manifests/codex.toml"),
    )
    .unwrap();
    assert!(
        wait_for(|| reloads() > before, SETTLE),
        "the watcher never noticed the new remote copy:\n{}",
        watcher_log(server, pane)
    );
}

#[test]
fn config_reload_and_manifest_reload_force_a_re_read_and_count_live_watchers() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Before any watcher exists, both verbs still work and report nothing to notify.
    let bare = Server::start();
    bare.write_config(CONFIG_OFF);
    let reloaded = bare.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(reloaded["status"], "applied");
    assert_eq!(reloaded["watchers_notified"], 0);
    let manifests = bare.ok(
        &["manifest", "reload"],
        "cli:manifest:reload",
        "agent_manifest_reload",
    );
    assert_eq!(manifests["watchers_notified"], 0);
    assert!(
        !manifests["manifests"].as_array().unwrap().is_empty(),
        "manifest reload should report the manifests in force: {manifests}"
    );
    // The token is a counter, so two reloads in a row are two distinct values — a watcher
    // only compares it for inequality.
    let first = bare.global_opt("@agent_wizard_reload");
    bare.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_ne!(
        first,
        bare.global_opt("@agent_wizard_reload"),
        "the reload token did not change"
    );
    drop(bare);

    let w = watched_agent(CONFIG_OFF, Vec::new());
    let (server, pane) = (&w.server, w.pane.as_str());
    let counted = server.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(
        counted["watchers_notified"], 1,
        "the live watcher was not counted: {counted}"
    );
    assert!(
        wait_for_log(server, pane, "config changed"),
        "the forced reload never reached the watcher:\n{}",
        watcher_log(server, pane)
    );
}

#[test]
fn a_config_that_no_longer_parses_is_refused_and_never_reaches_a_watcher() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let w = watched_agent(CONFIG_OFF, Vec::new());
    let (server, pane) = (&w.server, w.pane.as_str());

    let before = server.global_opt("@agent_wizard_reload");
    server.write_config("[ui\nthis is not toml");
    let refused = server.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["watchers_notified"], 0);
    assert!(
        !refused["diagnostics"].as_array().unwrap().is_empty(),
        "a failed reload should say what is wrong: {refused}"
    );
    // Not bumped: telling every watcher to re-read a file that cannot be parsed is asking
    // each of them to fail in turn.
    assert_eq!(before, server.global_opt("@agent_wizard_reload"));

    // The watcher does notice the mtime on its own, and keeps the settings it has.
    assert!(
        wait_for_log(server, pane, "keeping the settings in force"),
        "the watcher should have warned and carried on:\n{}",
        watcher_log(server, pane)
    );

    // An unknown key is accepted and ignored, which is `partial`, not `failed`.
    server.write_config(&format!("{CONFIG_OFF}\n[nonsense]\nkey = 1\n"));
    let partial = server.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(partial["status"], "partial", "{partial}");
}

#[test]
fn a_reloaded_toast_setting_changes_what_a_live_watcher_actually_does() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The claim worth proving: not that the file was re-read, but that the reloaded
    // settings are the ones in force the next time the watcher decides something.
    let tmp = tempfile::tempdir().unwrap();
    let recorder = NotifyRecorder::new(tmp.path());
    let w = watched_agent(
        CONFIG_OFF,
        vec![(
            "TAW_NOTIFY_CMD".to_string(),
            recorder.script.display().to_string(),
        )],
    );
    let (server, pane) = (&w.server, w.pane.as_str());

    // Notifications are off, so reaching `blocked` delivers nothing.
    w.fake.signal("-USR2");
    assert!(
        wait_for(|| server.pane_opt(pane, "@agent_state") == "blocked", AGENT),
        "the agent never reached blocked (state {:?})",
        server.pane_opt(pane, "@agent_state")
    );
    assert!(
        recorder.matching("blocked").is_empty(),
        "a notification was delivered with delivery = off: {:?}",
        recorder.lines()
    );

    // Turn them on and force the reload. Nothing restarts: the same watcher process has
    // to be the one that honours this.
    server.write_config(
        "\
[update]
manifest_check = false

[ui.toast]
delivery = \"system\"
delay_seconds = 0

[ui.sound]
enabled = false
",
    );
    let reloaded = server.ok(&["config", "reload"], "cli:config:reload", "config_reload");
    assert_eq!(reloaded["watchers_notified"], 1, "{reloaded}");
    assert!(
        wait_for_log(server, pane, "config changed"),
        "the reload never reached the watcher:\n{}",
        watcher_log(server, pane)
    );

    // Back to idle and blocked again: a fresh transition, decided under the new config.
    w.fake.signal("-USR1");
    assert!(
        wait_for(|| server.pane_opt(pane, "@agent_state") != "blocked", AGENT),
        "the agent never left blocked"
    );
    w.fake.signal("-USR2");
    assert!(
        wait_for(|| !recorder.matching("blocked").is_empty(), AGENT),
        "the reloaded delivery = system never took effect; state {:?}, delivered {:?}",
        server.pane_opt(pane, "@agent_state"),
        recorder.lines()
    );
}
