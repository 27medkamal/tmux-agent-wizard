// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The real TPM entry point, `tmux-agent-wizard.tmux`, against the real `taw` binary.
//!
//! `tests/bats/plugin.bats` covers the script's own decisions (option defaults, key
//! bindings, the version gate, hook idempotency) with a fake `taw` that only logs its
//! argv. What a fake cannot show is that the two halves fit together: that the hook
//! commands the script installs are ones the *real* binary accepts, that the watcher those
//! hooks start finds its config and state (it is a child of the tmux server, not of the
//! `taw` process that installed the hook), and that re-sourcing the plugin — which TPM
//! does on every `prefix + I` — neither stacks hooks nor toggles the watchers off.
//!
//! The last point is a regression guard: on tmux 3.6b `pipe-pane -o <cmd>` *toggles* the
//! pipe even when the command is unchanged, so a second `watch attach` on an
//! already-watched pane would turn watching silently off.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{Server, ServerOptions, taw_bin, wait_for};

/// Ceiling for a hook-driven side effect: `set-hook` runs the command with `run-shell -b`,
/// so it lands asynchronously. Generous enough for a loaded machine, well inside the
/// suite's 10 s cap.
const HOOK: Duration = Duration::from_secs(5);

/// Keeps the plugin's startup one-shots off the network and out of the host's notifier.
/// (`manifest update --if-stale` is one of them; the harness also pre-touches the staleness
/// marker, so it is a no-op either way.)
const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// Hooks the script installs, and the verb each entry must carry.
const WATCH_HOOKS: &[&str] = &[
    "after-new-window",
    "after-split-window",
    "after-new-session",
];
const SEEN_HOOKS: &[&str] = &[
    "after-select-pane",
    "after-select-window",
    "client-session-changed",
    "pane-focus-in",
];
const AUTOSAVE_HOOKS: &[&str] = &[
    "after-new-window",
    "after-split-window",
    "after-kill-pane",
    "session-closed",
];

/// Every entry of one global hook, one per line.
///
/// Queried by name rather than read out of a bare `show-hooks -g`: on tmux 3.6b the
/// unqualified listing does not enumerate `pane-focus-in` even when it is genuinely set
/// (`tests/bats/plugin.bats` documents the same quirk).
fn hook_entries(server: &Server, hook: &str) -> Vec<String> {
    server
        .tmux_ok(&["show-hooks", "-g", hook])
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn entries_matching(server: &Server, hook: &str, needle: &str) -> Vec<String> {
    hook_entries(server, hook)
        .into_iter()
        .filter(|line| line.contains(needle))
        .collect()
}

/// Every pane on the server, as raw tmux pane ids.
fn all_panes(server: &Server) -> Vec<String> {
    server
        .tmux_ok(&["list-panes", "-a", "-F", "#{pane_id}"])
        .lines()
        .map(str::to_string)
        .collect()
}

fn all_piped(server: &Server) -> bool {
    all_panes(server)
        .iter()
        .all(|pane| server.piped(pane) == "1")
}

#[test]
fn sourcing_the_plugin_installs_working_hooks_and_re_sourcing_is_idempotent() {
    let server = Server::start();
    server.write_config(CONFIG);

    // Nothing is watched before the plugin is loaded.
    assert_eq!(server.piped(&server.pane()), "0");

    server.source_plugin();

    // --- the hooks the plugin script installs ----------------------------
    for hook in WATCH_HOOKS {
        let entries = entries_matching(&server, hook, "watch attach");
        assert_eq!(
            entries.len(),
            1,
            "{hook} should carry exactly one `watch attach` entry, got {entries:?}"
        );
        assert!(
            entries[0].contains("run-shell -b"),
            "a hook must not block the server: {:?}",
            entries[0]
        );
        assert!(
            entries[0].contains(taw_bin()),
            "the hook must invoke the binary $TAW_BIN resolved to: {:?}",
            entries[0]
        );
        // The hook passes the pane the event fired for, falling back to the active pane.
        assert!(
            entries[0].contains("hook_pane"),
            "the hook must target the event's own pane: {:?}",
            entries[0]
        );
    }
    for hook in SEEN_HOOKS {
        let entries = entries_matching(&server, hook, " seen ");
        assert_eq!(
            entries.len(),
            1,
            "{hook} should carry exactly one `seen` entry, got {entries:?}"
        );
    }
    for hook in AUTOSAVE_HOOKS {
        let entries = entries_matching(&server, hook, "session autosave");
        assert_eq!(
            entries.len(),
            1,
            "{hook} should carry exactly one `session autosave` entry, got {entries:?}"
        );
    }

    // --- the startup one-shots ran, and recorded the binary's version -------------------
    // `watch attach-all` is fired with `run-shell -b`, so it lands asynchronously.
    let first_pane = server.pane();
    assert!(
        wait_for(|| server.piped(&first_pane) == "1", HOOK),
        "the plugin's `watch attach-all` should have attached the existing pane"
    );
    let version = server.global_opt("@agent_wizard_version");
    assert!(
        version.starts_with("taw "),
        "@agent_wizard_version should hold the binary's own --version, got {version:?}"
    );

    // --- a new window is watched by the hook, not by anything this test does ------------
    server.tmux_ok(&["new-window", "-t", "one"]);
    let panes = all_panes(&server);
    assert_eq!(panes.len(), 2, "new-window should have added a pane");
    let fresh = panes
        .iter()
        .find(|p| **p != first_pane)
        .expect("a pane other than the original");
    assert!(
        wait_for(|| server.piped(fresh) == "1", HOOK),
        "after-new-window should have attached a watcher to {fresh}"
    );
    assert!(
        wait_for(
            || server
                .pane_opt(fresh, "@agent_watch_pid")
                .parse::<u32>()
                .is_ok(),
            HOOK
        ),
        "the hook-started watcher should record its pid, got {:?}",
        server.pane_opt(fresh, "@agent_watch_pid")
    );

    // --- re-sourcing (TPM's `prefix + I`) ----------------------------------------------
    // Two things must survive it: the hook table must not grow, and the watchers already
    // running must neither be turned off (`pipe-pane -o` toggles on 3.6b) nor restarted
    // (any reissued `pipe-pane` replaces the pipe process, losing whatever evaluation it
    // was mid-way through).
    let pids_before: Vec<(String, String)> = all_panes(&server)
        .into_iter()
        .map(|pane| {
            let pid = server.pane_opt(&pane, "@agent_watch_pid");
            assert!(
                !pid.is_empty(),
                "pane {pane} has no watcher pid before the re-source"
            );
            (pane, pid)
        })
        .collect();

    server.source_plugin();
    // Give the second round of backgrounded startup one-shots time to have acted, so this
    // is not merely observing that they have not run yet.
    assert!(
        wait_for(
            || server
                .global_opt("@agent_wizard_version")
                .starts_with("taw "),
            HOOK
        ),
        "the second source should have re-recorded the version"
    );
    // Fixed sleep, not `wait_for`: the assertions below are that entry counts stay at 1
    // (no duplicate stacked) — a negative no poll can distinguish from "hasn't run yet".
    std::thread::sleep(Duration::from_millis(500));

    for hook in WATCH_HOOKS {
        assert_eq!(
            entries_matching(&server, hook, "watch attach").len(),
            1,
            "{hook} stacked a duplicate `watch attach` entry: {:?}",
            hook_entries(&server, hook)
        );
    }
    for hook in SEEN_HOOKS {
        assert_eq!(
            entries_matching(&server, hook, " seen ").len(),
            1,
            "{hook} stacked a duplicate `seen` entry: {:?}",
            hook_entries(&server, hook)
        );
    }
    for hook in AUTOSAVE_HOOKS {
        assert_eq!(
            entries_matching(&server, hook, "session autosave").len(),
            1,
            "{hook} stacked a duplicate `session autosave` entry: {:?}",
            hook_entries(&server, hook)
        );
    }

    assert!(
        all_piped(&server),
        "every pane must still be piped after re-sourcing: {:?}",
        all_panes(&server)
            .iter()
            .map(|p| (p.clone(), server.piped(p)))
            .collect::<Vec<_>>()
    );
    for (pane, before) in &pids_before {
        assert_eq!(
            &server.pane_opt(pane, "@agent_watch_pid"),
            before,
            "the watcher on {pane} was restarted by re-sourcing the plugin"
        );
    }

    // --- and the plugin's own hooks still work after the re-source ----------------------
    server.tmux_ok(&["split-window", "-t", "one"]);
    let latest = all_panes(&server);
    assert_eq!(latest.len(), 3);
    let split = latest
        .iter()
        .find(|p| !pids_before.iter().any(|(known, _)| known == *p))
        .expect("a pane created by the split");
    assert!(
        wait_for(|| server.piped(split) == "1", HOOK),
        "after-split-window should have attached a watcher to {split}"
    );
}

/// `@agent-wizard-watch off` is the documented opt-out: no watch hooks, no startup sweep,
/// and the seen/autosave hooks unaffected. bats asserts this against a fake binary; here
/// it is the real one, so "no watcher" is checked as "no pipe on any pane".
#[test]
fn the_watch_option_off_installs_no_watchers() {
    let server = Server::start();
    server.write_config(CONFIG);
    server.tmux_ok(&["set-option", "-g", "@agent-wizard-watch", "off"]);

    server.source_plugin();
    // The version marker is written by the last startup step, so once it is there the
    // one-shots have all been fired.
    assert!(
        wait_for(
            || server
                .global_opt("@agent_wizard_version")
                .starts_with("taw "),
            HOOK
        ),
        "the plugin's startup one-shots never completed"
    );

    for hook in WATCH_HOOKS {
        assert!(
            entries_matching(&server, hook, "watch attach").is_empty(),
            "{hook} should have no watch entry while @agent-wizard-watch is off: {:?}",
            hook_entries(&server, hook)
        );
    }
    for hook in SEEN_HOOKS {
        assert_eq!(
            entries_matching(&server, hook, " seen ").len(),
            1,
            "the seen hooks are independent of the watch setting: {:?}",
            hook_entries(&server, hook)
        );
    }

    server.tmux_ok(&["new-window", "-t", "one"]);
    // Fixed sleep, not `wait_for`: "never piped" is a negative no poll can confirm — a
    // short settle gives a wrongly-installed watcher a window to pipe the pane before
    // this checks for one, rather than merely checking too early to tell.
    std::thread::sleep(Duration::from_millis(500));
    for pane in all_panes(&server) {
        assert_eq!(
            server.piped(&pane),
            "0",
            "pane {pane} was watched despite @agent-wizard-watch off"
        );
    }
}

/// The session markers herdr strips from every new pane (`apply_pane_launch_env`,
/// `src/pane.rs` at v0.9.3; first in master's `8ac9542757` (#4461, #4104 in v0.9.2's changelog)).
const OUTER_AGENT_MARKERS: &[&str] = &[
    "CODEX_THREAD_ID",
    "OMPCODE",
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// The handles of the terminal that started the server, which herdr strips as well
/// (`apply_pane_terminal_env`, `src/pane.rs` at v0.9.3): iTerm2's, WezTerm's, Kitty's, Windows
/// Terminal's, screen's and Zellij's. herdr's list has `TMUX` and `TMUX_PANE` too. They are not
/// here, because tmux sets both for every pane itself.
const OUTER_TERMINAL_HANDLES: &[&str] = &[
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "WEZTERM_PANE",
    "KITTY_WINDOW_ID",
    "WT_SESSION",
    "STY",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
];

/// A pane created *now* in `session`: its raw id, and the environment it starts with, read
/// from a new window whose command is `env`, as `NAME=value` lines.
fn new_pane_in(server: &Server, session: &str, label: &str) -> (String, Vec<String>) {
    let out = server.work_dir().join(format!("env-{label}.txt"));
    // Written beside its name and renamed over it, so a file that is there is all there: a
    // long environment leaves `env` in more than one write.
    let id = server.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        &format!("{session}:"),
        &format!("env > '{0}.part' && mv '{0}.part' '{0}'", out.display()),
    ]);
    assert!(
        wait_for(|| out.exists(), HOOK),
        "the new pane never wrote its environment"
    );
    let environment = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    (id, environment)
}

/// The environment a pane created *now* in the first session starts with.
fn new_pane_environment(server: &Server, label: &str) -> Vec<String> {
    new_pane_in(server, "one", label).1
}

/// Which of `names` a pane environment carries. Names only: the rest of the environment is
/// the developer's own, and an assertion must never print it.
fn names_in(environment: &[String], names: &[&'static str]) -> Vec<&'static str> {
    names
        .iter()
        .copied()
        .filter(|name| value_in(environment, name).is_some())
        .collect()
}

/// What a pane environment holds for `name`. Only for a name a test gave an invented value, or
/// one tmux sets itself.
fn value_in<'a>(environment: &'a [String], name: &str) -> Option<&'a str> {
    environment
        .iter()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
}

/// What tmux gives its panes as `TERM_PROGRAM_VERSION`: the part of `tmux -V` after the name.
fn tmux_version(server: &Server) -> String {
    server
        .tmux_ok(&["-V"])
        .strip_prefix("tmux ")
        .expect("`tmux -V` names tmux")
        .to_string()
}

/// A tmux server started from inside Claude Code, Codex or OMP inherits their session
/// markers into its global environment and hands them to every new pane, so Claude Code
/// refuses to start in one (it believes it is a child session) and a Codex or OMP started
/// there reads as a nested session. With the plugin loaded, a new pane carries none of the
/// six; anything else, an API key for one, is left exactly as it was.
#[test]
fn a_new_pane_does_not_inherit_an_outer_agent_sessions_markers() {
    let mut env: Vec<(String, String)> = OUTER_AGENT_MARKERS
        .iter()
        .map(|name| (name.to_string(), "outer-session".to_string()))
        .collect();
    env.push(("ANTHROPIC_API_KEY".into(), "fake-api-key".into()));
    let server = Server::start_with(ServerOptions {
        env,
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);

    // The premise: before the plugin, a new pane does inherit all six.
    let before = new_pane_environment(&server, "before");
    assert_eq!(
        names_in(&before, OUTER_AGENT_MARKERS),
        OUTER_AGENT_MARKERS,
        "the server's environment should reach a new pane before the plugin runs"
    );

    server.source_plugin();

    let after = new_pane_environment(&server, "after");
    assert_eq!(
        names_in(&after, OUTER_AGENT_MARKERS),
        Vec::<&str>::new(),
        "these must not reach a new pane"
    );
    assert!(
        after.contains(&"ANTHROPIC_API_KEY=fake-api-key".to_string()),
        "an unrelated variable must survive"
    );

    // Sourcing again (TPM does, on every `prefix + I`) changes nothing.
    server.source_plugin();
    let again = new_pane_environment(&server, "again");
    assert_eq!(
        names_in(&again, OUTER_AGENT_MARKERS),
        Vec::<&str>::new(),
        "these came back after a second source"
    );
}

// Follows herdr's `pane_terminal_identity_removes_outer_terminal_identity` (src/pane.rs, v0.9.3):
// herdr's test hands the command it spawns a cleaned environment and reads that back. Here a
// tmux server is started with the same invented values (each handle `outer-session`, and the
// starting terminal's `TERM_PROGRAM`), the plugin is loaded, and a new pane's environment is read.
// tmux sets `TERM_PROGRAM`, `TMUX` and `TMUX_PANE` for every pane itself, so they are asserted
// as tmux's own where herdr's test asserts herdr's.
//
/// A tmux server started from a terminal keeps that terminal's handles in its global
/// environment and hands them to every new pane, even one you look at from another terminal
/// later: `update-environment` governs only the names it lists, and these are not among them.
/// With the plugin loaded a new pane carries none of the ten. A value in the session's own
/// environment, which is where tmux puts what a terminal brings, still reaches it.
#[test]
fn a_new_pane_does_not_inherit_the_terminal_that_started_tmux() {
    let mut env: Vec<(String, String)> = OUTER_TERMINAL_HANDLES
        .iter()
        .map(|name| (name.to_string(), "outer-session".to_string()))
        .collect();
    env.push(("TERM_PROGRAM".into(), "iTerm.app".into()));
    env.push(("TERM_PROGRAM_VERSION".into(), "outer-version".into()));
    env.push(("ANTHROPIC_API_KEY".into(), "fake-api-key".into()));
    env.push(("DISPLAY".into(), ":42".into()));
    let server = Server::start_with(ServerOptions {
        env,
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    // A server started inside another tmux holds its `TMUX` and `TMUX_PANE`. The harness keeps
    // both out of its own processes, so they go into the server's environment directly.
    for name in ["TMUX", "TMUX_PANE"] {
        server.tmux_ok(&["set-environment", "-g", name, "outer-session"]);
    }

    // The premise: before the plugin, a new pane does inherit all ten.
    let (before_id, before) = new_pane_in(&server, "one", "before");
    assert_eq!(
        names_in(&before, OUTER_TERMINAL_HANDLES),
        OUTER_TERMINAL_HANDLES,
        "the server's environment should reach a new pane before the plugin runs"
    );
    // And tmux's own answer for its own variables does not depend on the plugin at all.
    assert_eq!(value_in(&before, "TMUX_PANE"), Some(before_id.as_str()));

    server.source_plugin();

    let (id, after) = new_pane_in(&server, "one", "after");
    assert_eq!(
        names_in(&after, OUTER_TERMINAL_HANDLES),
        Vec::<&str>::new(),
        "these must not reach a new pane"
    );
    assert_eq!(value_in(&after, "TERM_PROGRAM"), Some("tmux"));
    assert_eq!(
        value_in(&after, "TERM_PROGRAM_VERSION"),
        Some(tmux_version(&server).as_str())
    );
    assert_eq!(
        value_in(&after, "TMUX_PANE"),
        Some(id.as_str()),
        "TMUX_PANE is the pane's own, not the outer tmux's"
    );
    assert!(
        value_in(&after, "TMUX").is_some_and(|tmux| tmux.contains(server.socket())),
        "TMUX names this server, not the outer tmux"
    );
    assert_eq!(
        value_in(&after, "ANTHROPIC_API_KEY"),
        Some("fake-api-key"),
        "an unrelated variable must survive"
    );
    assert_eq!(
        value_in(&after, "DISPLAY"),
        Some(":42"),
        "a display setting must survive"
    );

    // Sourcing again (TPM does, on every `prefix + I`) changes nothing.
    server.source_plugin();
    let (_, again) = new_pane_in(&server, "one", "again");
    assert_eq!(
        names_in(&again, OUTER_TERMINAL_HANDLES),
        Vec::<&str>::new(),
        "these came back after a second source"
    );

    // A value in the session's own environment is laid over the global removal, for that one
    // name and no other.
    server.tmux_ok(&[
        "set-environment",
        "-t",
        "one",
        "ITERM_SESSION_ID",
        "w1t1p1:session-value",
    ]);
    let (_, with_value) = new_pane_in(&server, "one", "session-value");
    assert_eq!(
        value_in(&with_value, "ITERM_SESSION_ID"),
        Some("w1t1p1:session-value"),
        "a session's own value must reach its new panes"
    );
    assert_eq!(
        names_in(&with_value, OUTER_TERMINAL_HANDLES),
        vec!["ITERM_SESSION_ID"],
        "only the session's own value comes back"
    );
}

/// `tmux <args>` run by a client started in a terminal that has `terminal` in its own
/// environment. tmux copies the names `update-environment` lists from the environment of the
/// client that creates a session, or attaches to one, and not from the server's.
fn tmux_from_terminal(server: &Server, terminal: &[(&str, &str)], args: &[&str]) -> String {
    let mut cmd = Command::new("tmux");
    cmd.arg("-L").arg(server.socket()).args(args);
    for (name, value) in terminal {
        cmd.env(name, value);
    }
    cmd.env_remove("TMUX").env_remove("TMUX_PANE");
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "tmux {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// A name `update-environment` lists still reaches a session from the terminal that started it,
/// one value for each terminal, because a session's own environment is laid over the global
/// one the plugin flags. A name it does not list stays out, whichever terminal brought it.
/// (tmux does the same copy when a terminal attaches to a session. Attaching needs a terminal,
/// so this starts sessions instead.)
#[test]
fn a_handle_named_in_update_environment_reaches_the_session_a_terminal_started() {
    let env: Vec<(String, String)> = OUTER_TERMINAL_HANDLES
        .iter()
        .map(|name| (name.to_string(), "outer-session".to_string()))
        .collect();
    let server = Server::start_with(ServerOptions {
        env,
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server.source_plugin();
    server.tmux_ok(&["set-option", "-ga", "update-environment", "LC_TERMINAL"]);

    // Two terminals each start a session. Each brings `LC_TERMINAL`, which is listed, and
    // `ITERM_SESSION_ID`, which is not.
    let work = server.work_dir().to_str().unwrap().to_string();
    for (session, lc_terminal, iterm) in [
        ("two", "terminal-a", "w0t0p0:terminal-a"),
        ("three", "terminal-b", "w0t0p0:terminal-b"),
    ] {
        tmux_from_terminal(
            &server,
            &[("LC_TERMINAL", lc_terminal), ("ITERM_SESSION_ID", iterm)],
            &["new-session", "-d", "-s", session, "-c", &work, "/bin/sh"],
        );
    }

    let (_, in_two) = new_pane_in(&server, "two", "in-two");
    assert_eq!(value_in(&in_two, "LC_TERMINAL"), Some("terminal-a"));
    assert_eq!(
        names_in(&in_two, OUTER_TERMINAL_HANDLES),
        vec!["LC_TERMINAL"],
        "only the listed name comes with the terminal"
    );
    let (_, in_three) = new_pane_in(&server, "three", "in-three");
    assert_eq!(value_in(&in_three, "LC_TERMINAL"), Some("terminal-b"));
    assert_eq!(
        names_in(&in_three, OUTER_TERMINAL_HANDLES),
        vec!["LC_TERMINAL"]
    );

    // The first session was started before the name was listed, and no terminal has brought it
    // since, so it gets nothing.
    let (_, in_one) = new_pane_in(&server, "one", "in-one");
    assert_eq!(
        names_in(&in_one, OUTER_TERMINAL_HANDLES),
        Vec::<&str>::new(),
        "a session no terminal started since the name was listed must not hold any of them"
    );
}
