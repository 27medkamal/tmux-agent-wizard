//! `%calling`: the pane word taw's pane verbs read as "the pane this process runs in", from `$TMUX_PANE`.
//! It is the `HERDR_PANE_ID` the plugin hands out with `@agent-wizard-herdr-env` on (tmux gives every pane the
//! one global value), and it is accepted whether the setting is on or off.
//!
//! Real tmux on private servers.

mod common;

use common::{Server, ServerOptions};

fn server() -> Server {
    Server::start_with(ServerOptions::default())
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

fn error_message(env: &serde_json::Value) -> String {
    env["error"]["message"].as_str().unwrap_or("").to_string()
}

#[test]
fn calling_is_the_pane_the_process_runs_in() {
    let server = server();
    let first = server.pane();
    let second = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "one:",
        "sleep 60",
    ]);
    assert_ne!(first, second);
    for pane in [&first, &second] {
        let (code, env) = server.taw_in_pane(pane, &["pane", "get", "%calling"]);
        assert_eq!(code, 0, "{env}");
        assert_eq!(
            env["result"]["pane"]["pane_id"].as_str().unwrap(),
            server.public_pane_id(pane),
            "%calling with TMUX_PANE={pane}"
        );
    }

    // The other verbs that resolve a pane through the resolver take it too, as a flag as well.
    let (code, env) = server.taw_with_env(
        &["pane", "current", "--pane", "%calling"],
        &[("TMUX_PANE", &second)],
    );
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["pane"]["pane_id"].as_str().unwrap(),
        server.public_pane_id(&second)
    );
    let (code, env) = server.taw_in_pane(
        &second,
        &[
            "pane",
            "report-agent",
            "%calling",
            "--source",
            "custom:crush",
            "--agent",
            "crush",
            "--state",
            "working",
            "--seq",
            "1",
        ],
    );
    assert_eq!(code, 0, "{env}");
    assert!(
        listed_panes(&server)
            .iter()
            .any(|(p, _)| *p == server.public_pane_id(&second)),
        "{:?}",
        listed_panes(&server)
    );
}

#[test]
fn calling_without_tmux_pane_is_the_existing_no_current_pane_error() {
    let server = server();
    let (code, env) = server.taw(&["pane", "get", "%calling"]);
    assert_ne!(code, 0, "{env}");
    assert!(
        error_message(&env).contains("no current pane"),
        "got: {}",
        error_message(&env)
    );
    // An empty $TMUX_PANE is no pane either.
    let (code, env) = server.taw_with_env(&["pane", "get", "%calling"], &[("TMUX_PANE", "")]);
    assert_ne!(code, 0, "{env}");
    assert!(error_message(&env).contains("no current pane"), "{env}");
}

/// With another session in the way, whose own pane is not the caller's, `%calling` is the caller's pane
/// and with no `$TMUX_PANE` it is no pane (the other session answers for nothing).
#[test]
fn calling_is_the_callers_pane_with_another_session_beside_it() {
    let server = server();
    let named = server.tmux_ok(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        "calling",
        "sleep 60",
    ]);
    let mine = server.pane();
    assert_ne!(named, mine);
    let (code, env) = server.taw_in_pane(&mine, &["pane", "get", "%calling"]);
    assert_eq!(code, 0, "{env}");
    assert_eq!(
        env["result"]["pane"]["pane_id"].as_str().unwrap(),
        server.public_pane_id(&mine),
        "reached the session named calling"
    );
    // And with no $TMUX_PANE the session's name does not answer for it.
    let (code, env) = server.taw(&["pane", "get", "%calling"]);
    assert_ne!(code, 0, "{env}");
    assert!(error_message(&env).contains("no current pane"), "{env}");
}

/// A `$TMUX_PANE` that names a pane that is gone is a pane that is not there, as a `--pane` that names
/// it is: `%calling` is looked up and checked live like any other pane word, not taken on trust.
#[test]
fn calling_with_a_closed_pane_in_tmux_pane_is_not_found_as_that_pane_is() {
    let server = server();
    let gone = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "one:",
        "sleep 60",
    ]);
    server.tmux_ok(&["kill-pane", "-t", &gone]);

    let (code, by_flag) = server.taw(&["pane", "get", &gone]);
    assert_ne!(code, 0, "{by_flag}");
    assert_eq!(by_flag["error"]["code"], "not_found", "{by_flag}");

    let (code, calling) = server.taw_in_pane(&gone, &["pane", "get", "%calling"]);
    assert_ne!(code, 0, "{calling}");
    assert_eq!(calling["error"]["code"], "not_found", "{calling}");

    // A verb that hands the resolved id straight to tmux (`pane get` looks it up in a listing, which
    // would find it gone whatever the resolver did) answers the same.
    fn args(pane: &str) -> Vec<&str> {
        vec![
            "pane",
            "report-agent",
            pane,
            "--source",
            "custom:crush",
            "--agent",
            "crush",
            "--state",
            "working",
            "--seq",
            "1",
        ]
    }
    let (code, by_flag) = server.taw(&args(&gone));
    assert_ne!(code, 0, "{by_flag}");
    assert_eq!(by_flag["error"]["code"], "not_found", "{by_flag}");
    let (code, calling) = server.taw_with_env(&args("%calling"), &[("TMUX_PANE", &gone)]);
    assert_ne!(code, 0, "{calling}");
    assert_eq!(calling["error"]["code"], "not_found", "{calling}");
}
