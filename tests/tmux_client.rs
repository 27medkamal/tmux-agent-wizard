mod common;

use common::shell_env::ShellHome;
use taw::store::{self, AgentState};
use taw::tmux::Tmux;

/// A throwaway tmux server on its own `-L` socket, torn down on drop. Never touches the
/// user's real tmux server (no `-L`/`-S` means the default socket; this always sets one).
/// The second field is the home its shells have, which goes with it: no history file and no
/// startup file of whoever runs the tests (see `common::shell_env`).
struct Server(Tmux, #[allow(dead_code)] ShellHome);

impl Server {
    fn start() -> Server {
        // `cargo test` runs the `#[test]` fns in this file concurrently, as separate
        // threads of *one* process — std::process::id() alone is the same for all of
        // them, so a name built only from the pid would make every test's Server race to
        // create session "one" on the very same socket. The atomic counter keeps the
        // documented `taw-test-<pid>` naming (see plan's "Global Constraints") while
        // giving every Server its own socket, so tests are actually isolated from
        // each other, not just from the user's real tmux server.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("taw-test-{}-{n}", std::process::id());
        let t = Tmux::with_socket_name(&name);
        let home = ShellHome::new();
        // `-f /dev/null`: a fresh `-L` socket still loads the *host's* `~/.config/tmux/tmux.conf`
        // (or `~/.tmux.conf`) — same as any other tmux invocation, `-L` only picks the socket.
        // On a machine with a personal config (base-index/pane-base-index != 0, TPM plugin
        // autoload, hooks, ...) that breaks both the "index 0" assumption below and the
        // "throwaway/isolated" premise of this whole test file. `-f /dev/null` makes the
        // server start clean, independent of whatever tmux.conf the host has.
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

#[test]
fn lists_panes_with_agent_options_in_one_call() {
    let s = Server::start();
    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    let p = &panes[0];
    assert!(p.pane_id.starts_with('%'));
    assert_eq!(p.session_name, "one");
    assert!(p.agent.kind.is_none());
    store::write_state(
        &s.0,
        &p.pane_id,
        AgentState::Working,
        Some("live_turn_working"),
        "manifest",
        1_700_000_000,
    )
    .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "claude")
        .unwrap();
    assert!(p.agent.reported_kind.is_none());
    s.0.set_pane_option(&p.pane_id, "@agent_reported_kind", "claude")
        .unwrap();
    let p2 = &s.0.list_panes().unwrap()[0];
    assert_eq!(p2.agent.kind.as_deref(), Some("claude"));
    assert_eq!(p2.agent.reported_kind.as_deref(), Some("claude"));
    assert_eq!(p2.agent.state, Some(AgentState::Working));
    assert_eq!(p2.agent.rule.as_deref(), Some("live_turn_working"));
    assert_eq!(p2.agent.state_since, Some(1_700_000_000));
    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());
    let cleared = &s.0.list_panes().unwrap()[0];
    assert!(cleared.agent.kind.is_none());
    // What an agent's hook said about the occupant goes with the occupant: the next one
    // must not be taken for something that named itself.
    assert!(cleared.agent.reported_kind.is_none());
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_reported_kind")
            .unwrap(),
        None
    );
}

/// What tmux titles a pane no program has titled is the host name, in full: `default_pane_title`
/// names it, and the listing, which the popup, the JSON verbs, the hooks and the plugins read,
/// has none for it. A title a program sets afterwards (OSC 2 and `select-pane -T` set the same
/// field) is the pane's title as it is.
#[test]
fn a_pane_no_program_titled_has_the_host_name_for_its_title_and_the_listing_has_none() {
    let s = Server::start();
    let default = taw::tmux::default_pane_title().expect("this machine has a host name");
    let raw = |pane: &str| {
        s.0.display(pane, "#{pane_title}")
            .unwrap()
            .trim()
            .to_string()
    };
    let first = s.0.list_panes().unwrap().remove(0);
    assert_eq!(
        raw(&first.pane_id),
        default,
        "tmux's default title is the host name, in full"
    );
    assert_eq!(first.pane_title, "", "and the listing has none");
    assert_eq!(
        taw::tmux::osc_title(&raw(&first.pane_id), Some(&default)),
        ""
    );

    // A split and a new window are panes nobody titled as well.
    s.0.run_ok(&["split-window", "-d", "-t", &first.pane_id, "/bin/sh"])
        .unwrap();
    s.0.run_ok(&["new-window", "-d", "-t", "one", "/bin/sh"])
        .unwrap();
    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 3);
    for other in &panes {
        assert_eq!(raw(&other.pane_id), default, "pane {}", other.pane_id);
        assert_eq!(other.pane_title, "", "pane {}", other.pane_id);
    }

    // A program that sets a title: it is the pane's title as it is.
    s.0.run_ok(&["select-pane", "-t", &first.pane_id, "-T", "project"])
        .unwrap();
    let titled =
        s.0.list_panes()
            .unwrap()
            .into_iter()
            .find(|other| other.pane_id == first.pane_id)
            .unwrap();
    assert_eq!(raw(&first.pane_id), "project");
    assert_eq!(titled.pane_title, "project");
}

#[test]
fn resolves_targets_and_names() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    let public = taw::tmux::ids::pane_id(&p.session_id, &p.pane_id);
    assert_eq!(
        taw::tmux::ids::resolve_pane(&s.0, &public).unwrap(),
        p.pane_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_pane(&s.0, &p.pane_id).unwrap(),
        p.pane_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_pane(&s.0, "one:0.0").unwrap(),
        p.pane_id
    ); // window/pane indexes default to 0 on a fresh server
    s.0.set_pane_option(&p.pane_id, "@agent_name", "reviewer")
        .unwrap();
    assert_eq!(
        taw::tmux::ids::resolve_pane(&s.0, "reviewer").unwrap(),
        p.pane_id
    );
    assert!(taw::tmux::ids::resolve_pane(&s.0, "nope").is_err());
}

#[test]
fn resolves_window_and_session_targets() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    let public_workspace = taw::tmux::ids::workspace_id(&p.session_id);
    assert_eq!(
        taw::tmux::ids::resolve_session(&s.0, &public_workspace).unwrap(),
        p.session_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_session(&s.0, &p.session_id).unwrap(),
        p.session_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_session(&s.0, "one").unwrap(),
        p.session_id
    );
    assert!(taw::tmux::ids::resolve_session(&s.0, "no-such-session").is_err());

    let public_tab = taw::tmux::ids::tab_id(&p.session_id, &p.window_id);
    assert_eq!(
        taw::tmux::ids::resolve_window(&s.0, &public_tab).unwrap(),
        p.window_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_window(&s.0, &p.window_id).unwrap(),
        p.window_id
    );
    assert_eq!(
        taw::tmux::ids::resolve_window(&s.0, "one:0").unwrap(),
        p.window_id
    );
}

#[test]
fn lists_sessions_windows_and_clients() {
    let s = Server::start();
    let panes = s.0.list_panes().unwrap();
    let p = &panes[0];

    let sessions = s.0.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, p.session_id);
    assert_eq!(sessions[0].session_name, "one");
    assert_eq!(sessions[0].session_windows, 1);
    // tmux has no `#{active_window_id}` format variable; list_sessions derives it from
    // list_windows — assert that actually happened rather than silently being empty.
    assert_eq!(sessions[0].active_window_id, p.window_id);

    let windows = s.0.list_windows().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].window_id, p.window_id);
    assert_eq!(windows[0].session_id, p.session_id);
    assert!(windows[0].window_active);
    assert_eq!(windows[0].window_panes, 1);

    // No client is attached to this throwaway server.
    assert!(s.0.list_clients().unwrap().is_empty());
}

/// The sessions of a group (`new-session -t`) each report the group's size, and a session in no
/// group reports 0, which is tmux's empty `#{session_group_size}`. A session left alone in its
/// group still reports the group, of one, so a size over one is what says others share its windows.
#[test]
fn a_session_in_a_group_reports_the_groups_size_and_a_lone_one_reports_none() {
    let s = Server::start();
    assert_eq!(s.0.list_sessions().unwrap()[0].session_group_size, 0);

    s.0.run_ok(&["new-session", "-d", "-t", "one", "-s", "two"])
        .unwrap();
    s.0.run_ok(&["new-session", "-d", "-t", "one", "-s", "three"])
        .unwrap();
    let sessions = s.0.list_sessions().unwrap();
    assert_eq!(sessions.len(), 3);
    for session in &sessions {
        assert_eq!(session.session_group_size, 3, "{}", session.session_name);
    }

    s.0.run_ok(&["kill-session", "-t", "two"]).unwrap();
    s.0.run_ok(&["kill-session", "-t", "three"]).unwrap();
    let sessions = s.0.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_group_size, 1);
}

#[test]
fn pane_and_global_option_round_trip_through_get_set_unset() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_display").unwrap(),
        None
    );
    s.0.set_pane_option(&p.pane_id, "@agent_display", "Reviewer 1")
        .unwrap();
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_display").unwrap(),
        Some("Reviewer 1".to_string())
    );
    s.0.unset_pane_option(&p.pane_id, "@agent_display").unwrap();
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_display").unwrap(),
        None
    );

    assert_eq!(s.0.get_global_option("@agent_wizard_status").unwrap(), None);
    s.0.set_global_option("@agent_wizard_status", "RAIL-1")
        .unwrap();
    assert_eq!(
        s.0.get_global_option("@agent_wizard_status").unwrap(),
        Some("RAIL-1".to_string())
    );
}

#[test]
fn captures_pane_content() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    s.0.run_ok(&[
        "send-keys",
        "-t",
        &p.pane_id,
        "echo taw-capture-marker",
        "Enter",
    ])
    .unwrap();
    // Give the shell a moment to echo and redraw before capturing.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let content = s.0.capture_pane(&p.pane_id, false).unwrap();
    assert!(
        content.contains("taw-capture-marker"),
        "capture-pane output did not contain the marker: {content:?}"
    );
    // -e (with escapes) must not error; content is allowed to differ.
    s.0.capture_pane(&p.pane_id, true).unwrap();
}

#[test]
fn reports_a_tmux_version_at_least_the_project_floor() {
    let s = Server::start();
    let v = s.0.version().unwrap();
    assert!(
        v.at_least(3, 2),
        "tmux {v:?} is below the project's 3.2 floor"
    );
}

#[test]
fn set_seen_round_trips_through_read() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    assert!(!store::read(&s.0, &p.pane_id).unwrap().seen);
    store::set_seen(&s.0, &p.pane_id, true).unwrap();
    assert!(store::read(&s.0, &p.pane_id).unwrap().seen);
    store::set_seen(&s.0, &p.pane_id, false).unwrap();
    assert!(!store::read(&s.0, &p.pane_id).unwrap().seen);
}

#[test]
fn reads_custom_tokens_and_clear_agent_removes_them() {
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);

    s.0.set_pane_option(&p.pane_id, "@agent_token_pr", "482")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_token_branch", "task-1 branch")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_kind_hint", "claude")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_watch_pid", "4242")
        .unwrap();

    let record = store::read(&s.0, &p.pane_id).unwrap();
    assert_eq!(record.tokens.get("pr").map(String::as_str), Some("482"));
    assert_eq!(
        record.tokens.get("branch").map(String::as_str),
        Some("task-1 branch")
    );

    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());
    let cleared = store::read(&s.0, &p.pane_id).unwrap();
    assert!(
        cleared.tokens.is_empty(),
        "tokens survived clear_agent: {:?}",
        cleared.tokens
    );
    // watch_pid and kind_hint are deliberately kept across clear_agent.
    assert_eq!(cleared.kind_hint.as_deref(), Some("claude"));
    assert_eq!(cleared.watch_pid, Some(4242));
}

#[test]
fn clear_agent_removes_the_container_marker_with_the_agent() {
    // `@agent_container` says which runtime an agent runs under, so it belongs to the agent:
    // the pane's shell coming back must not leave a marker on a pane with nothing in it.
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "claude")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_container", "docker")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_kind_hint", "claude")
        .unwrap();
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_container").unwrap(),
        Some("docker".to_string())
    );

    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_container").unwrap(),
        None,
        "the marker survived clear_agent"
    );
    // The hint is the one thing that outlives the occupant, as before.
    assert_eq!(
        store::read(&s.0, &p.pane_id).unwrap().kind_hint.as_deref(),
        Some("claude")
    );
}

/// The profile mark is taken off at its end only while the pane holds what the watcher's read found
/// there: the profile, its process or no process, and the report stamp. A `taw sandbox` typed since
/// marked the pane afresh with a process of its own, a restore's line that ran marked it with one,
/// and a report since the read is the next read's to decide on. Every option of the mark goes, and
/// the agent's kind stays.
#[test]
fn the_profile_mark_is_taken_off_only_while_the_pane_holds_what_the_read_found() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap().remove(0).pane_id;
    let mark = |pid: Option<&str>, stamp: Option<&str>| {
        s.0.set_pane_option(&pane, "@agent_sandbox", "box").unwrap();
        for (option, value) in [("@agent_sandbox_pid", pid), ("@agent_report_stamp", stamp)] {
            match value {
                Some(value) => s.0.set_pane_option(&pane, option, value).unwrap(),
                None => s.0.unset_pane_option(&pane, option).unwrap(),
            }
        }
    };
    let get = |option: &str| s.0.get_pane_option(&pane, option).unwrap();
    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();

    mark(Some("5151"), None);
    assert!(!store::end_sandbox(&s.0, &pane, "box", "4242", "").unwrap());
    assert!(!store::end_sandbox(&s.0, &pane, "box", "", "").unwrap());
    assert!(!store::end_sandbox(&s.0, &pane, "default", "5151", "").unwrap());
    mark(Some("5151"), Some("1790800000000000002"));
    assert!(!store::end_sandbox(&s.0, &pane, "box", "5151", "1790800000000000001").unwrap());
    assert_eq!(get("@agent_sandbox").as_deref(), Some("box"));
    assert_eq!(get("@agent_sandbox_pid").as_deref(), Some("5151"));

    assert!(store::end_sandbox(&s.0, &pane, "box", "5151", "1790800000000000002").unwrap());
    for option in store::SANDBOX_OPTIONS {
        assert_eq!(get(option), None, "{option}");
    }
    assert_eq!(get("@agent_kind").as_deref(), Some("claude"));

    // A restore's mark, with no process.
    mark(None, None);
    assert!(store::end_sandbox(&s.0, &pane, "box", "", "").unwrap());
    assert_eq!(get("@agent_sandbox"), None);
}

/// At the mark's end: what the pane's agent reported of itself (its session, what started it,
/// its word, the command that resumes it) goes with the profile mark, in the one call that takes the
/// mark off, so that no save can keep a session from inside the sandbox with no profile beside it.
/// A mark or a report stamp that changed since the read leaves all of it, the mark included. A start
/// that did not happen (`forget_sandbox`) takes the same off.
#[test]
fn the_profile_marks_end_takes_what_the_agent_reported_of_itself_with_it() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap().remove(0).pane_id;
    let reported = [
        ("@agent_session_id", "4b1f2c"),
        (
            "@agent_session_path",
            "/home/agent/.claude/projects/-work/4b1f2c.jsonl",
        ),
        ("@agent_token_session_start_source", "startup"),
        ("@agent_reported_kind", "claude"),
        ("@agent_resume_source", "integration:claude"),
        ("@agent_resume_agent", "claude"),
        ("@agent_resume_argv", "claude --resume 4b1f2c"),
    ];
    for (option, value) in reported {
        s.0.set_pane_option(&pane, option, value).unwrap();
    }
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_sandbox", "box"),
        ("@agent_sandbox_pid", "5151"),
        ("@agent_report_stamp", "1790800000000000002"),
    ] {
        s.0.set_pane_option(&pane, option, value).unwrap();
    }
    let get = |option: &str| s.0.get_pane_option(&pane, option).unwrap();
    let untouched = || {
        assert_eq!(get("@agent_sandbox").as_deref(), Some("box"));
        for (option, value) in reported {
            assert_eq!(get(option).as_deref(), Some(value), "{option}");
        }
    };

    // A report since the read, another profile, another process: nothing is taken off.
    assert!(!store::end_sandbox(&s.0, &pane, "box", "5151", "1790800000000000001").unwrap());
    untouched();
    assert!(!store::end_sandbox(&s.0, &pane, "max", "5151", "1790800000000000002").unwrap());
    untouched();
    assert!(!store::end_sandbox(&s.0, &pane, "box", "4242", "1790800000000000002").unwrap());
    untouched();

    assert!(store::end_sandbox(&s.0, &pane, "box", "5151", "1790800000000000002").unwrap());
    for option in store::SANDBOX_OPTIONS {
        assert_eq!(get(option), None, "{option}");
    }
    for (option, _) in reported {
        assert_eq!(get(option), None, "{option}");
    }
    assert_eq!(
        get("@agent_kind").as_deref(),
        Some("claude"),
        "the kind stays"
    );

    // A start that did not happen takes the same off, unguarded.
    for (option, value) in reported {
        s.0.set_pane_option(&pane, option, value).unwrap();
    }
    s.0.set_pane_option(&pane, "@agent_sandbox", "box").unwrap();
    store::forget_sandbox(&s.0, &pane).unwrap();
    assert_eq!(get("@agent_sandbox"), None);
    for (option, _) in reported {
        assert_eq!(get(option), None, "{option}");
    }
}

/// The sandbox report reader's write lands only while the pane holds the profile mark and the agent
/// the read found, and a path is kept exactly as it was given. It sets no report stamp: that guards
/// the watcher's own writes against a report from outside, which this is not.
#[test]
fn a_sandboxed_session_is_written_only_while_the_pane_holds_its_mark_and_its_agent() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    s.0.set_pane_option(&pane, "@agent_sandbox", "default")
        .unwrap();
    let session = store::Session {
        id: Some("4b1f2c"),
        path: Some("/home/you/My Projects/~work;/s.jsonl"),
    };

    assert!(
        !store::write_sandboxed_session(
            &s.0,
            &pane,
            "claude",
            "max",
            "",
            &session,
            Some("startup")
        )
        .unwrap()
    );
    assert!(
        store::read(&s.0, &pane).unwrap().session_id.is_none(),
        "another profile's mark"
    );
    assert!(
        !store::write_sandboxed_session(
            &s.0,
            &pane,
            "codex",
            "default",
            "",
            &session,
            Some("startup")
        )
        .unwrap()
    );
    assert!(
        store::read(&s.0, &pane).unwrap().session_id.is_none(),
        "another agent"
    );

    assert!(
        store::write_sandboxed_session(
            &s.0,
            &pane,
            "claude",
            "default",
            "",
            &session,
            Some("startup")
        )
        .unwrap()
    );

    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.session_id.as_deref(), Some("4b1f2c"));
    assert_eq!(
        record.session_path.as_deref(),
        Some("/home/you/My Projects/~work;/s.jsonl")
    );
    assert_eq!(record.reported_kind.as_deref(), Some("claude"));
    assert_eq!(
        record
            .tokens
            .get("session_start_source")
            .map(String::as_str),
        Some("startup")
    );
    assert_eq!(
        s.0.get_pane_option(&pane, "@agent_report_stamp").unwrap(),
        None
    );
}

/// The reader's write also lands only while the mark holds the process the read saw: a fresh `taw
/// sandbox` of the same profile marks the pane with another, and the old container's session is not
/// its to carry. A mark with no process is read as the empty text, and holds only an empty one.
#[test]
fn a_sandboxed_session_is_written_only_while_the_mark_holds_the_process_the_read_saw() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    s.0.set_pane_option(&pane, "@agent_sandbox", "default")
        .unwrap();
    s.0.set_pane_option(&pane, "@agent_sandbox_pid", "4242")
        .unwrap();
    let session = store::Session {
        id: Some("4b1f2c"),
        path: None,
    };
    for seen in ["4243", "", "424"] {
        assert!(
            !store::write_sandboxed_session(
                &s.0,
                &pane,
                "claude",
                "default",
                seen,
                &session,
                Some("startup")
            )
            .unwrap(),
            "{seen:?}"
        );
        assert!(
            store::read(&s.0, &pane).unwrap().session_id.is_none(),
            "{seen:?}: another process"
        );
    }
    assert!(
        store::write_sandboxed_session(
            &s.0,
            &pane,
            "claude",
            "default",
            "4242",
            &session,
            Some("startup")
        )
        .unwrap()
    );
    assert_eq!(
        store::read(&s.0, &pane).unwrap().session_id.as_deref(),
        Some("4b1f2c")
    );

    // A mark with no process holds only the empty text.
    s.0.unset_pane_option(&pane, "@agent_sandbox_pid").unwrap();
    s.0.unset_pane_option(&pane, "@agent_session_id").unwrap();
    assert!(
        !store::write_sandboxed_session(&s.0, &pane, "claude", "default", "4242", &session, None)
            .unwrap()
    );
    assert!(
        store::write_sandboxed_session(&s.0, &pane, "claude", "default", "", &session, None)
            .unwrap()
    );
}

/// A container that comes into a pane marks it, and takes off what the pane's last agent reported
/// of itself (its session, what started it, its word that it was there, and the command that
/// resumes it), in one call that lands only while the pane holds the report stamp the watcher's
/// read saw: a report since is the next read's to decide on, and nothing is written then. The rest
/// of the record stays.
#[test]
fn a_container_that_comes_takes_what_the_last_agent_reported_only_while_the_pane_holds_the_read_stamp()
 {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap().remove(0).pane_id;
    let reported = [
        ("@agent_session_id", "host-conv-1"),
        (
            "@agent_session_path",
            "/home/you/.claude/projects/x/host-conv-1.jsonl",
        ),
        ("@agent_token_session_start_source", "startup"),
        ("@agent_reported_kind", "claude"),
        ("@agent_resume_source", "integration:claude"),
        ("@agent_resume_agent", "claude"),
        ("@agent_resume_argv", "claude --resume host-conv-1"),
    ];
    for (option, value) in reported {
        s.0.set_pane_option(&pane, option, value).unwrap();
    }
    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    s.0.set_pane_option(&pane, "@agent_report_stamp", "1790800000000000002")
        .unwrap();
    let get = |option: &str| s.0.get_pane_option(&pane, option).unwrap();

    assert!(!store::mark_container(&s.0, &pane, "docker", "1790800000000000001").unwrap());
    assert_eq!(get("@agent_container"), None);
    for (option, value) in reported {
        assert_eq!(get(option).as_deref(), Some(value), "{option}");
    }

    assert!(store::mark_container(&s.0, &pane, "docker", "1790800000000000002").unwrap());
    assert_eq!(get("@agent_container").as_deref(), Some("docker"));
    for (option, _) in reported {
        assert_eq!(get(option), None, "{option}");
    }
    assert_eq!(get("@agent_kind").as_deref(), Some("claude"));
    assert_eq!(
        get("@agent_report_stamp").as_deref(),
        Some("1790800000000000002")
    );
}

#[test]
fn clear_agent_removes_the_turn_stamp_with_the_agent() {
    // `@agent_turn` is the stamp of the agent's last report of work: it belongs to the agent, and the
    // next one to take the pane starts from none.
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "opencode")
        .unwrap();
    s.0.set_pane_option(&p.pane_id, "@agent_turn", "1790800000123456789")
        .unwrap();

    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());
    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_turn").unwrap(),
        None,
        "the stamp survived clear_agent"
    );
}

#[test]
fn clear_agent_leaves_the_status_the_agent_had_where_a_wait_can_read_it() {
    // A wait that polls can miss the last state of an agent that is released a moment after it
    // reports it, so the watcher's clear records what the pane held, as tmux reads it when the
    // command runs, and leaves it when the record goes (herdr answers a wait with the final status
    // an agent is released with).
    let s = Server::start();
    let p = s.0.list_panes().unwrap().remove(0);
    store::write_state_with_kind(
        &s.0,
        &p.pane_id,
        Some("opencode"),
        AgentState::Done,
        None,
        "hook:test",
        1_700_000_000,
    )
    .unwrap();
    let before = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();

    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());

    assert_eq!(
        s.0.get_pane_option(&p.pane_id, "@agent_state").unwrap(),
        None,
        "the state went with the agent"
    );
    let left = store::Final::parse(
        &s.0.get_pane_option(&p.pane_id, store::FINAL_OPTION)
            .unwrap()
            .unwrap_or_default(),
    )
    .expect("the clear left no status behind");
    assert_eq!(left.state, AgentState::Done);
    assert!(
        left.at >= before,
        "{} is before the clear ({before})",
        left.at
    );

    // A record that holds no state leaves no status, and what the last agent left stands (a wait
    // reads it only when it is newer than the wait itself).
    s.0.set_pane_option(&p.pane_id, "@agent_kind", "claude")
        .unwrap();
    assert!(store::clear_agent(&s.0, &p.pane_id, "").unwrap());
    let again = store::Final::parse(
        &s.0.get_pane_option(&p.pane_id, store::FINAL_OPTION)
            .unwrap()
            .unwrap_or_default(),
    );
    assert_eq!(
        again,
        Some(left),
        "a record with no state replaced the status"
    );
}

#[test]
fn list_panes_survives_a_tab_inside_a_free_text_agent_option() {
    // `@agent_display`/`@agent_name`/`@agent_title` and `pane_title` hold free text a
    // user or an agent can put a literal tab into, and tmux round-trips it verbatim.
    // Since `list_panes` asks for many columns joined by tabs and splits the reply on
    // tabs, one such tab would shift every later column and fail the *whole* listing —
    // taking out the rail, the popup, `watch status` and every CLI listing at once. tmux
    // de-tabs each column server-side, so the column count stays exact.
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();

    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    s.0.set_pane_option(&pane, "@agent_display", "a\tb")
        .unwrap();
    s.0.set_pane_option(&pane, "@agent_title", "left\tright")
        .unwrap();

    // The hazard is real, not hypothetical: expanding the bare format — the very thing
    // `list_panes` joins with tabs — yields the stored tab verbatim.
    assert_eq!(
        s.0.display(&pane, "#{@agent_display}").unwrap(),
        "a\tb",
        "tmux round-trips a literal tab through a user option"
    );

    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    let agent = &panes[0].agent;
    assert_eq!(
        agent.kind.as_deref(),
        Some("claude"),
        "columns must not shift"
    );
    assert_eq!(
        agent.display.as_deref(),
        Some("a b"),
        "the tab must arrive flattened to a space"
    );
    assert_eq!(agent.title.as_deref(), Some("left right"));

    // `store::read` splits on tabs the same way, so it is de-tabbed too.
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.kind.as_deref(), Some("claude"));
    assert_eq!(record.display.as_deref(), Some("a b"));
    assert_eq!(record.title.as_deref(), Some("left right"));

    // Every free-text column is wrapped the same way, so a tab anywhere is survivable —
    // including one in the *last* column, where a miscount would otherwise go unnoticed.
    s.0.set_pane_option(&pane, "@agent_name", "a\tname")
        .unwrap();
    s.0.set_pane_option(&pane, "@agent_kind_hint", "hint\there")
        .unwrap();
    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].agent.name.as_deref(), Some("a name"));
    assert_eq!(panes[0].agent.kind_hint.as_deref(), Some("hint here"));
    assert_eq!(panes[0].agent.kind.as_deref(), Some("claude"));
    assert_eq!(panes[0].agent.display.as_deref(), Some("a b"));
}

#[test]
fn write_state_sends_all_four_options_in_one_tmux_invocation() {
    // A reader must never observe a torn transition (a new `@agent_state` beside a stale
    // `@agent_rule`/`@agent_source`/`@agent_state_since`), so the four `set-option`s go
    // out as one `tmux` invocation. Proven with a shim standing in for the tmux binary
    // that records each invocation's argv — the only way to count invocations.
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let shim = dir.path().join("tmux-shim.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);

    let t = Tmux {
        bin: shim.clone(),
        socket: None,
    };
    store::write_state(
        &t,
        "%7",
        AgentState::Working,
        Some("live_turn_working"),
        "manifest",
        1_700_000_000,
    )
    .unwrap();

    let recorded = std::fs::read_to_string(&log).unwrap();
    let invocations: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        invocations.len(),
        1,
        "write_state must issue exactly one tmux invocation, got {invocations:#?}"
    );
    let argv = invocations[0];
    for option in [
        "@agent_state",
        "@agent_state_since",
        "@agent_rule",
        "@agent_source",
    ] {
        assert!(
            argv.contains(option),
            "{option} missing from the batch: {argv}"
        );
    }
    assert_eq!(
        argv.matches("set-option").count(),
        4,
        "one set-option per option: {argv}"
    );
    assert_eq!(
        argv.split_whitespace().filter(|a| *a == ";").count(),
        3,
        "four commands joined by three separators: {argv}"
    );

    // With no rule the batch still holds four commands, the third being the `-u` removal
    // form (tmux has no `unset-option`).
    std::fs::remove_file(&log).unwrap();
    store::write_state(&t, "%7", AgentState::Idle, None, "hook:test", 1_700_000_001).unwrap();
    let recorded = std::fs::read_to_string(&log).unwrap();
    assert_eq!(recorded.lines().count(), 1, "{recorded}");
    assert!(
        recorded.contains("set-option -u -p -t %7 @agent_rule"),
        "{recorded}"
    );
}

#[test]
fn write_state_lands_every_option_on_a_real_server() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    store::write_state(
        &s.0,
        &pane,
        AgentState::Blocked,
        Some("approval_prompt"),
        "hook:test",
        1_700_000_042,
    )
    .unwrap();

    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.state, Some(AgentState::Blocked));
    assert_eq!(record.state_since, Some(1_700_000_042));
    assert_eq!(record.rule.as_deref(), Some("approval_prompt"));
    assert_eq!(record.source.as_deref(), Some("hook:test"));

    // A rule-less transition removes `@agent_rule` rather than leaving the old one.
    store::write_state(
        &s.0,
        &pane,
        AgentState::Idle,
        None,
        "manifest",
        1_700_000_043,
    )
    .unwrap();
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.state, Some(AgentState::Idle));
    assert!(record.rule.is_none(), "stale rule survived: {record:?}");
    assert_eq!(record.source.as_deref(), Some("manifest"));
}

/// `taw pane report-agent` records the agent's kind with its state and its owner: one tmux
/// invocation of five commands, so that no reader sees an agent nobody owns.
#[test]
fn write_state_with_a_kind_sends_all_five_options_in_one_tmux_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let shim = dir.path().join("tmux-shim.sh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&shim);
    let t = Tmux {
        bin: shim,
        socket: None,
    };

    store::write_state_with_kind(
        &t,
        "%7",
        Some("opencode"),
        AgentState::Working,
        None,
        "hook:integration:opencode",
        1_700_000_000,
    )
    .unwrap();

    let recorded = std::fs::read_to_string(&log).unwrap();
    let invocations: Vec<&str> = recorded.lines().collect();
    assert_eq!(invocations.len(), 1, "{invocations:#?}");
    let argv = invocations[0];
    for option in [
        "@agent_kind opencode",
        "@agent_state working",
        "@agent_state_since",
        "-u -p -t %7 @agent_rule",
        "@agent_source hook:integration:opencode",
    ] {
        assert!(argv.contains(option), "{option} missing: {argv}");
    }
    assert_eq!(argv.matches("set-option").count(), 5, "{argv}");
    assert_eq!(
        argv.split_whitespace().filter(|a| *a == ";").count(),
        4,
        "five commands joined by four separators: {argv}"
    );

    // Without a kind it is the four options it always was.
    std::fs::remove_file(&log).unwrap();
    store::write_state_with_kind(&t, "%7", None, AgentState::Idle, None, "manifest", 1).unwrap();
    let recorded = std::fs::read_to_string(&log).unwrap();
    assert!(!recorded.contains("@agent_kind"), "{recorded}");
    assert_eq!(recorded.matches("set-option").count(), 4, "{recorded}");
}

#[test]
fn write_state_with_a_kind_lands_the_kind_and_the_rest_on_a_real_server() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();

    store::write_state_with_kind(
        &s.0,
        &pane,
        Some("opencode"),
        AgentState::Blocked,
        None,
        "hook:integration:opencode",
        1_700_000_042,
    )
    .unwrap();

    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.kind.as_deref(), Some("opencode"));
    assert_eq!(record.state, Some(AgentState::Blocked));
    assert_eq!(record.state_since, Some(1_700_000_042));
    assert!(record.rule.is_none());
    assert_eq!(record.source.as_deref(), Some("hook:integration:opencode"));
}

/// The order of reports on a pane that holds no `--seq` of any source, for a report that carries none:
/// what a report written straight through the store, with no source's order to keep, is written over.
fn unordered() -> store::Order {
    store::Order {
        read: String::new(),
        next: String::new(),
    }
}

/// What `taw pane report-agent` writes, on a real server: the agent, its state and owner, and the
/// session that came with them, all there after one call; and what another agent's hook left on the
/// pane goes before the new kind is written.
#[test]
fn write_report_lands_the_agent_its_state_and_its_session_and_drops_the_last_agents() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    for (option, value) in [
        ("@agent_kind", "claude"),
        ("@agent_reported_kind", "claude"),
        ("@agent_session_id", "sess-old"),
        ("@agent_session_path", "/old/transcript.jsonl"),
    ] {
        s.0.set_pane_option(&pane, option, value).unwrap();
    }

    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind: "opencode",
            state: AgentState::Working,
            source: "hook:integration:opencode",
            session: store::Session {
                id: Some("ses_new"),
                path: None,
            },
            now: 1_700_000_042,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();

    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.kind.as_deref(), Some("opencode"));
    assert_eq!(record.state, Some(AgentState::Working));
    assert_eq!(record.state_since, Some(1_700_000_042));
    assert_eq!(record.source.as_deref(), Some("hook:integration:opencode"));
    assert_eq!(record.session_id.as_deref(), Some("ses_new"));
    assert_eq!(record.session_path, None, "the last agent's path stayed");
    assert_eq!(record.reported_kind, None);
}

/// A report of the state the pane already holds moves neither `@agent_state_since`, which says when
/// the state last moved, nor the turn stamp of a wait; a report of another state moves the first and
/// stamps a wait, and a report of work stamps whatever it was.
#[test]
fn a_report_of_the_state_the_pane_holds_moves_neither_the_time_nor_the_turn_stamp() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let report = |state: AgentState, now: u64| {
        store::write_report(
            &s.0,
            &pane,
            &store::Report {
                kind: "opencode",
                state,
                source: "hook:x",
                session: store::Session::default(),
                now,
                resume: None,
                reporter: "x",
            },
            &unordered(),
        )
        .unwrap();
        let record = store::read(&s.0, &pane).unwrap();
        (
            record.state_since,
            s.0.get_pane_option(&pane, store::TURN_OPTION).unwrap(),
        )
    };

    // The first report of a wait is a change from nothing: it is stamped, and the time is its own.
    let (since, stamp) = report(AgentState::Blocked, 100);
    assert_eq!(since, Some(100));
    let stamp = stamp.expect("a report of a wait on a pane with no state is stamped");

    // The same wait again: nothing moved.
    let (since, again) = report(AgentState::Blocked, 107);
    assert_eq!(
        since,
        Some(100),
        "the time moved for the state it was already in"
    );
    assert_eq!(again.as_deref(), Some(stamp.as_str()), "the stamp moved");

    // Another state, and the wait again after it, are changes.
    let (since, _) = report(AgentState::Idle, 110);
    assert_eq!(since, Some(110));
    let (since, anew) = report(AgentState::Blocked, 120);
    assert_eq!(since, Some(120));
    assert_ne!(
        anew.as_deref(),
        Some(stamp.as_str()),
        "a wait entered anew was not stamped"
    );

    // Work is stamped whenever it is reported, the same state again included.
    let (_, first) = report(AgentState::Working, 130);
    let (since, second) = report(AgentState::Working, 131);
    assert_eq!(
        since,
        Some(130),
        "the time moved for the state it was already in"
    );
    assert_ne!(first, second, "a second report of work was not stamped");
    assert!(first.is_some() && second.is_some());

    // A state that is not work or a wait leaves the stamp as it was.
    let (_, kept) = report(AgentState::Idle, 140);
    assert_eq!(kept, second);
}

/// Who holds a pane is asked of a real tmux: the reporter's `hook:<source>` for its agent, another hook,
/// or none, whatever a source or an agent is called.
#[test]
fn whether_a_reporter_holds_a_pane_is_asked_of_tmux() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let holding =
        |source: &str, agent: &str| store::read_holding(&s.0, &pane, source, agent).unwrap();

    assert_eq!(holding("s", "a"), (false, false), "an empty pane");
    s.0.set_pane_option(&pane, "@agent_source", "manifest")
        .unwrap();
    s.0.set_pane_option(&pane, "@agent_kind", "a").unwrap();
    assert_eq!(holding("s", "a"), (false, false), "the watcher's, no hook");
    s.0.set_pane_option(&pane, "@agent_source", "hook:s")
        .unwrap();
    assert_eq!(holding("s", "a"), (true, true));
    assert_eq!(holding("t", "a"), (false, true), "another source");
    assert_eq!(holding("s", "b"), (false, true), "another agent");
    for (source, agent) in [("a,b}c", "x#{y}"), ("my source", "agent:1"), ("*", "?")] {
        s.0.set_pane_option(&pane, "@agent_source", &format!("hook:{source}"))
            .unwrap();
        s.0.set_pane_option(&pane, "@agent_kind", agent).unwrap();
        assert_eq!(holding(source, agent), (true, true), "{source:?} {agent:?}");
        assert_eq!(holding("s", agent), (false, true), "{source:?} {agent:?}");
    }
}

/// A report is written by the command line of an `if-shell`, which tmux parses from a string, so every
/// value it carries goes through tmux's parser: a quote, a `$`, a `~`, a `;`, a format, a line break,
/// a `#` after spaces, characters that are not ASCII, each comes out as it went in, in the kind, the
/// owner, the session path and the token of what started the session, and the report is one report.
#[test]
fn a_report_of_values_full_of_what_a_command_line_reads_for_a_meaning_stores_each_as_it_is() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let kind = "my \"agent\" $HOME ~ a;b #{pane_id} \u{1f600}";
    let source = "hook:~/src;$x \"q\" #{s}";
    let path = "~/work/a b/\"x\"/$y/;\n  # not a comment\t\u{e9};";
    let start = "re$sume ~ \"#{x}\" ;";

    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind,
            state: AgentState::Blocked,
            source,
            session: store::Session {
                id: Some("ses_1"),
                path: Some(path),
            },
            now: 9,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.kind.as_deref(), Some(kind));
    assert_eq!(record.state, Some(AgentState::Blocked));
    assert_eq!(record.source.as_deref(), Some(source));
    assert_eq!(s.0.display(&pane, "#{@agent_session_path}").unwrap(), path);

    // The same agent's session report: the drop of another agent's identity compares the kind as
    // written, so nothing it reported goes.
    store::write_session_report(
        &s.0,
        &pane,
        kind,
        &store::Session {
            id: None,
            path: None,
        },
        Some(start),
        None,
        &unordered(),
        None,
    )
    .unwrap();
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.session_id.as_deref(), Some("ses_1"));
    assert_eq!(record.reported_kind.as_deref(), Some(kind));
    assert_eq!(
        s.0.display(&pane, "#{@agent_token_session_start_source}")
            .unwrap(),
        start
    );
}

/// A value that ends in `;` is one word of a command to tmux, not the end of the command: a
/// session path is stored whole, and the report that carries it is not cut in two.
#[test]
fn a_session_path_that_ends_in_a_semicolon_is_stored_whole_by_a_report() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();

    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind: "opencode",
            state: AgentState::Idle,
            source: "hook:x",
            session: store::Session {
                id: Some("ses_1"),
                path: Some("/tmp/a;b;"),
            },
            now: 1,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();
    store::write_session_report(
        &s.0,
        &pane,
        "opencode",
        &store::Session {
            id: None,
            path: Some("/tmp/c;"),
        },
        Some("resume;"),
        None,
        &unordered(),
        None,
    )
    .unwrap();

    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.state, Some(AgentState::Idle));
    assert_eq!(record.session_id.as_deref(), Some("ses_1"));
    assert_eq!(record.session_path.as_deref(), Some("/tmp/c;"));
    assert_eq!(
        record
            .tokens
            .get("session_start_source")
            .map(String::as_str),
        Some("resume;")
    );
    // The same pane holds the first report's own path when nothing came after it.
    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind: "opencode",
            state: AgentState::Idle,
            source: "hook:x",
            session: store::Session {
                id: None,
                path: Some("/tmp/a;b;"),
            },
            now: 2,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();
    assert_eq!(
        store::read(&s.0, &pane).unwrap().session_path.as_deref(),
        Some("/tmp/a;b;")
    );
}

/// What the watcher writes over an agent's report, the completion rule's `done` over its `idle`, is
/// written only while the pane still holds the state the watcher read: a report that came since is
/// the newer word, and an agent has no screen to put it back from. The check is tmux's own, in the
/// invocation that writes.
#[test]
fn write_state_over_writes_only_while_the_pane_still_holds_the_state_it_was_read_in() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind: "opencode",
            state: AgentState::Idle,
            source: "hook:integration:opencode",
            session: store::Session::default(),
            now: 100,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();

    // The pane holds `idle`: the `done` goes over it, with the hook the source and the kind kept.
    store::write_state_over(&s.0, &pane, AgentState::Idle, AgentState::Done, 200).unwrap();
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.state, Some(AgentState::Done));
    assert_eq!(record.state_since, Some(200));
    assert_eq!(record.source.as_deref(), Some("hook:integration:opencode"));
    assert_eq!(record.kind.as_deref(), Some("opencode"));

    // A newer report has made it `working` since the read: the watcher's write does not undo it,
    // and its since is the report's own.
    store::write_report(
        &s.0,
        &pane,
        &store::Report {
            kind: "opencode",
            state: AgentState::Working,
            source: "hook:integration:opencode",
            session: store::Session::default(),
            now: 300,
            resume: None,
            reporter: "x",
        },
        &unordered(),
    )
    .unwrap();
    store::write_state_over(&s.0, &pane, AgentState::Idle, AgentState::Done, 400).unwrap();
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.state, Some(AgentState::Working));
    assert_eq!(record.state_since, Some(300));
}

/// The pid of a pane's watcher comes back from the same invocation that writes the report, and only
/// when the pane records one that is a number.
#[test]
fn write_report_returns_the_pid_the_pane_records_for_its_watcher() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let report = store::Report {
        kind: "opencode",
        state: AgentState::Working,
        source: "hook:x",
        session: store::Session::default(),
        now: 1,
        resume: None,
        reporter: "x",
    };
    assert_eq!(
        store::write_report(&s.0, &pane, &report, &unordered()).unwrap(),
        store::Written::Taken(None)
    );
    s.0.set_pane_option(&pane, "@agent_watch_pid", "4242")
        .unwrap();
    assert_eq!(
        store::write_report(&s.0, &pane, &report, &unordered()).unwrap(),
        store::Written::Taken(Some(4242))
    );
    s.0.set_pane_option(&pane, "@agent_watch_pid", "not a pid")
        .unwrap();
    assert_eq!(
        store::write_report(&s.0, &pane, &report, &unordered()).unwrap(),
        store::Written::Taken(None)
    );
}

#[test]
fn list_panes_survives_a_newline_inside_a_free_text_agent_option() {
    // A newline is worse than a tab. `list_panes` splits the reply into *lines* first,
    // so one `@agent_display` holding `a\nb` would turn a single pane's row into two
    // half-rows, both of which fail the field count — and `list_panes` returned `Err`,
    // taking out the rail, `taw ui`, `agent list`, `pane list`, `watch status`, `session
    // save` and every plugin payload at once, for as long as the option stayed set.
    // A user can set that value through the product: `taw pane report-metadata
    // --display-agent`.
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();

    s.0.set_pane_option(&pane, "@agent_kind", "claude").unwrap();
    s.0.set_pane_option(&pane, "@agent_display", "a\nb")
        .unwrap();

    // The hazard is real, not hypothetical: tmux stores and replays the newline verbatim.
    assert_eq!(
        s.0.display(&pane, "#{@agent_display}").unwrap(),
        "a\nb",
        "tmux round-trips a literal newline through a user option"
    );

    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1, "one pane must still list as one row");
    assert_eq!(
        panes[0].agent.kind.as_deref(),
        Some("claude"),
        "columns must not shift"
    );
    assert_eq!(
        panes[0].agent.display.as_deref(),
        Some("a b"),
        "the newline must arrive flattened to a space"
    );

    // `store::read` splits the same reply the same way.
    let record = store::read(&s.0, &pane).unwrap();
    assert_eq!(record.display.as_deref(), Some("a b"));

    // A carriage return would split nothing here but still corrupts a rendered row, and
    // the same fold covers it. Checked in the last column too, where a miscount is
    // easiest to miss.
    s.0.set_pane_option(&pane, "@agent_title", "left\r\nright")
        .unwrap();
    s.0.set_pane_option(&pane, "@agent_kind_hint", "hint\nhere")
        .unwrap();
    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].agent.title.as_deref(), Some("left  right"));
    assert_eq!(panes[0].agent.kind_hint.as_deref(), Some("hint here"));
    assert_eq!(panes[0].agent.kind.as_deref(), Some("claude"));
}

#[test]
fn every_tab_joined_listing_folds_control_characters_not_just_list_panes() {
    // `list_panes` was fixed; `list_sessions`, `list_windows` and `list_clients` split the
    // same way and were not folded at all.
    //
    // The column that actually carries the risk is the *path*, not the name: tmux escapes
    // a tab in a session or window name to the two characters `\t` when it stores it
    // (verified on 3.6b), but `#{session_path}` and `#{pane_current_path}` are filesystem
    // paths and a directory name may hold a tab or a newline verbatim. One such directory
    // shifted every later column of the sessions listing, and `list_sessions` returns
    // `Err` for a wrong field count — which is the rail, `taw ui`, `workspace list` and
    // `session save` all failing at once for as long as a session sat in that directory.
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let odd = tmp.path().join("we\tird\nname");
    std::fs::create_dir_all(&odd).unwrap();

    s.0.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "odd",
        "-c",
        odd.to_str().unwrap(),
        "/bin/sh",
    ])
    .unwrap();

    // The hazard is real, not hypothetical: tmux replays the path's own bytes.
    let raw = s.0.display("odd", "#{session_path}").unwrap();
    assert!(
        raw.contains('\t') && raw.contains('\n'),
        "precondition: tmux round-trips a path's control characters, got {raw:?}"
    );

    let sessions = s.0.list_sessions().unwrap();
    assert_eq!(sessions.len(), 2, "two sessions must list as two rows");
    let odd_session = sessions
        .iter()
        .find(|sess| sess.session_name == "odd")
        .expect("the odd session must survive the listing");
    assert_eq!(
        odd_session.session_path,
        odd.to_string_lossy().replace(['\t', '\n'], " "),
        "the path's control characters must arrive folded to spaces"
    );

    // Same reply shape, same fold: the windows and clients listings must not break either.
    assert_eq!(s.0.list_windows().unwrap().len(), 2);
    assert!(s.0.list_clients().is_ok(), "no clients, but no parse error");

    // And the panes listing agrees about the same path, through `#{pane_current_path}`.
    let panes = s.0.list_panes().unwrap();
    assert_eq!(panes.len(), 2);
    assert!(
        panes
            .iter()
            .any(|p| p.session_name == "odd" && !p.pane_current_path.contains('\t')),
        "pane_current_path must be folded too: {:?}",
        panes
            .iter()
            .map(|p| &p.pane_current_path)
            .collect::<Vec<_>>()
    );
}

/// The flags a Claude was started with are written in the tmux command that writes its session report:
/// set to the words as they are (a space, a quote and a `;` in a value come out whole), taken off by an
/// empty list, left alone by none, gone with the agent's other records when another agent takes the
/// pane, and not written at all by a report that lost the race (`Written::Changed`).
#[test]
fn a_session_report_writes_the_claude_flags_with_the_session_or_not_at_all() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let session = store::Session {
        id: Some("sess-1"),
        path: None,
    };
    let report = |kind: &str, flags: Option<&[String]>, order: &store::Order| {
        store::write_session_report(&s.0, &pane, kind, &session, None, flags, order, None).unwrap()
    };
    let words: Vec<String> = ["--effort", "max", "--model", "a b'c;"]
        .map(String::from)
        .to_vec();

    assert_eq!(
        report("claude", Some(&words), &unordered()),
        store::Written::Taken(false)
    );
    assert_eq!(store::read(&s.0, &pane).unwrap().claude_flags, words);
    assert_eq!(
        s.0.display(&pane, "#{@agent_claude_flags}").unwrap(),
        r#"["--effort","max","--model","a b'c;"]"#
    );

    // A report that did not find the process leaves what the pane holds.
    report("claude", None, &unordered());
    assert_eq!(store::read(&s.0, &pane).unwrap().claude_flags, words);

    // One that found it, with no flags, clears them.
    report("claude", Some(&[]), &unordered());
    assert!(store::read(&s.0, &pane).unwrap().claude_flags.is_empty());
    assert_eq!(s.0.display(&pane, "#{@agent_claude_flags}").unwrap(), "");

    // Another agent that takes the pane drops what the last one's flags were.
    report("claude", Some(&words), &unordered());
    report("codex", None, &unordered());
    assert!(store::read(&s.0, &pane).unwrap().claude_flags.is_empty());

    // A report that lost the race (the pane's order is not what it read) writes neither the session
    // nor the flags.
    report("claude", None, &unordered());
    let stale = store::Order {
        read: "other=1".to_string(),
        next: "other=2".to_string(),
    };
    assert_eq!(
        report("claude", Some(&words), &stale),
        store::Written::Changed
    );
    assert!(store::read(&s.0, &pane).unwrap().claude_flags.is_empty());
}

/// The watcher's clear of an agent that has left takes the flags its Claude was started with off the
/// pane with the rest of the agent's record, so the next agent in the pane is saved with none of them.
#[test]
fn the_watchers_clear_of_a_departed_agent_takes_the_claude_flags_off() {
    let s = Server::start();
    let pane = s.0.list_panes().unwrap()[0].pane_id.clone();
    let session = store::Session {
        id: Some("sess-1"),
        path: None,
    };
    let words: Vec<String> = ["--effort", "max"].map(String::from).to_vec();
    store::write_session_report(
        &s.0,
        &pane,
        "claude",
        &session,
        None,
        Some(&words),
        &unordered(),
        None,
    )
    .unwrap();
    assert_eq!(
        store::read(&s.0, &pane).unwrap().claude_flags,
        words,
        "precondition"
    );

    let stamp = s.0.display(&pane, "#{@agent_report_stamp}").unwrap();
    assert!(store::clear_agent(&s.0, &pane, &stamp).unwrap());

    assert!(store::read(&s.0, &pane).unwrap().claude_flags.is_empty());
    assert_eq!(s.0.display(&pane, "#{@agent_claude_flags}").unwrap(), "");
}
