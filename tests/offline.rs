//! No test reaches herdr.dev.
//!
//! The product refreshes the cached agent manifests from herdr.dev in two places that a test can
//! start without meaning to: a watcher (any `watch attach`, or `agent start`), and the plugin script's
//! startup one-shot (`taw manifest update --if-stale`). Both look at the state directory first: a
//! marker of the last check that is less than six hours old, or `[update] manifest_check = false`,
//! and they ask for nothing. The helpers make the marker in the state directory of every server
//! they start (`common::Server`, `ShellHome`, the shell harness, the sweep), and put a proxy that
//! nothing listens on in the environment (`shell_env::OFFLINE`, which the HTTP client of the product
//! reads), so that what is attempted anyway fails at once, at home.
//!
//! These tests stand a listener where that proxy is and start what a test would: the one-shot, and a
//! watcher, with the helper's own state and nothing the test said. They assert that the product says
//! it did not check (`checked: false`, and the watcher's own log line that the cache is fresh), and
//! that the listener was not called. Then, as a control (a negative result that nothing could have
//! changed proves nothing), each takes the marker away, starts another watcher, and sees the listener
//! asked for `herdr.dev`.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::harness::harness_up;
use common::proxy::Proxy;
use common::shell_env::ShellHome;
use common::{Server, ServerOptions, taw_bin, wait_for};
use serde_json::Value;
use taw::tmux::{Socket, Tmux};

/// How long a watcher may take to start and say what it did about the manifests.
const CEILING: Duration = Duration::from_secs(30);

/// What a watcher logs when the marker says the cache was checked a moment ago.
const FRESH: &str = "manifest cache is fresh; no update attempted";

/// What the proxy is asked for when the product fetches a manifest.
const THE_MANIFESTS: &str = "CONNECT herdr.dev:443 HTTP/1.1";

/// Every log file under `dir`, whole.
fn logs_in(dir: &Path) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            out.push_str(&format!("== {}\n", entry.path().display()));
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    out
}

fn assert_no_request(proxy: &Proxy, what: &str) {
    assert_eq!(
        proxy.requests(),
        Vec::<String>::new(),
        "{what} asked for something over the network"
    );
}

/// The control: with the marker gone, a watcher on `pane` fetches, and the listener hears it.
fn assert_a_watcher_with_no_marker_asks(
    proxy: &Proxy,
    marker: &Path,
    attach: impl Fn(&str),
    new_pane: impl Fn() -> String,
    what: &str,
) {
    std::fs::remove_file(marker).unwrap();
    attach(&new_pane());
    assert!(
        wait_for(|| !proxy.requests().is_empty(), CEILING),
        "{what}: a watcher with no marker never tried to fetch, so this proves nothing about one with a marker"
    );
    assert_eq!(proxy.requests()[0], THE_MANIFESTS, "{what}");
}

#[test]
fn a_server_of_the_e2e_helper_asks_for_no_manifests_and_one_without_a_marker_does() {
    let proxy = Proxy::start();
    let server = Server::start_with(ServerOptions {
        env: vec![
            ("ALL_PROXY".to_string(), proxy.url()),
            ("TAW_LOG".to_string(), "debug".to_string()),
        ],
        ..ServerOptions::default()
    });

    let one_shot = server.ok(
        &["manifest", "update", "--if-stale"],
        "cli:manifest:update",
        "manifest_update",
    );
    assert_eq!(
        one_shot["checked"], false,
        "the plugin's one-shot would have fetched: {one_shot}"
    );
    let pane = server.pane();
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    assert!(
        wait_for(|| server.watcher_logs().contains(FRESH), CEILING),
        "the watcher never said what it did about the manifests:\n{}",
        server.watcher_logs()
    );
    assert_no_request(&proxy, "a server of common::Server");

    assert_a_watcher_with_no_marker_asks(
        &proxy,
        &server
            .state_dir()
            .join("agent-detection")
            .join("last-check"),
        |pane| {
            server.ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
        },
        || server.tmux_ok(&["new-window", "-d", "-P", "-F", "#{pane_id}"]),
        "common::Server",
    );
}

#[test]
fn a_server_started_through_shell_home_asks_for_no_manifests_and_one_without_a_marker_does() {
    let proxy = Proxy::start();
    let name = format!("taw-test-offline-{}", std::process::id());
    let tmux = Tmux::with_socket_name(&name);
    // After the proxy, so that the server is gone (with the home) before the listener is.
    let home = ShellHome::new();
    let url = proxy.url();
    home.start_server_with(
        &tmux,
        &[("ALL_PROXY", &url), ("TAW_LOG", "debug")],
        &["-d", "-s", "one", "-x", "80", "-y", "24", "/bin/sh"],
    );
    // Nothing of the test's own in the environment, as for the tmux_* fixtures: the product finds
    // its state where the home says, as the processes in the server do.
    let taw = |args: &[&str]| -> Value {
        let mut command = Command::new(taw_bin());
        command.args(args);
        home.apply(&mut command);
        command
            .env("TAW_TMUX_SOCKET_NAME", &name)
            .env("ALL_PROXY", &url)
            .env("TAW_LOG", "debug")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        let out = command.output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
            panic!(
                "taw {} printed no envelope ({err}): {}{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    };
    let state = home
        .path()
        .join(".local")
        .join("state")
        .join("tmux-agent-wizard");

    let one_shot = taw(&["manifest", "update", "--if-stale"]);
    assert_eq!(
        one_shot["result"]["checked"], false,
        "the plugin's one-shot would have fetched: {one_shot}"
    );
    let pane = tmux.display("one", "#{pane_id}").unwrap();
    taw(&["watch", "attach", &pane]);
    assert!(
        wait_for(|| logs_in(&state.join("logs")).contains(FRESH), CEILING),
        "the watcher never said what it did about the manifests:\n{}",
        logs_in(&state.join("logs"))
    );
    assert_no_request(&proxy, "a server started through ShellHome");

    assert_a_watcher_with_no_marker_asks(
        &proxy,
        &state.join("agent-detection").join("last-check"),
        |pane| {
            taw(&["watch", "attach", pane]);
        },
        || {
            tmux.run(&["new-window", "-d", "-P", "-F", "#{pane_id}"])
                .unwrap()
        },
        "ShellHome",
    );
}

#[test]
fn the_e2e_shell_harness_asks_for_no_manifests() {
    let proxy = Proxy::start();
    let tmp = tempfile::tempdir().unwrap();
    let harness = harness_up(
        tmp.path(),
        &[("E2E_PROXY", &proxy.url()), ("TAW_LOG", "debug")],
    );
    let (config, state) = (harness.dir.join("config"), harness.dir.join("state"));
    let Some(Socket::Path(socket)) = &harness.inner.socket else {
        panic!("the harness's servers are reached by the paths of their sockets");
    };
    let taw = |args: &[&str]| -> Value {
        let out = Command::new(taw_bin())
            .args(args)
            .env("TAW_CONFIG_DIR", &config)
            .env("TAW_STATE_DIR", &state)
            .env("TAW_TMUX_SOCKET_PATH", socket)
            .env("ALL_PROXY", proxy.url())
            .env("TAW_LOG", "debug")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
            panic!(
                "taw {} printed no envelope ({err}): {}{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    };

    let one_shot = taw(&["manifest", "update", "--if-stale"]);
    assert_eq!(
        one_shot["result"]["checked"], false,
        "the plugin's one-shot would have fetched: {one_shot}"
    );
    let pane = harness.inner.display("main:0.0", "#{pane_id}").unwrap();
    taw(&["watch", "attach", &pane]);
    assert!(
        wait_for(|| logs_in(&state.join("logs")).contains(FRESH), CEILING),
        "the watcher never said what it did about the manifests:\n{}",
        logs_in(&state.join("logs"))
    );
    // What the plugin fired when the harness loaded it counts too: it has had all this time.
    assert_no_request(&proxy, "the e2e harness");
}
