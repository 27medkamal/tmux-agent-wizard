//! The remaining herdr socket methods as `taw` verbs, end to end against a throwaway
//! tmux server: `session snapshot`, `terminal title set|clear`, `config reset-keys`,
//! `popup close`, and `agent view set|clear`.
//!
//! Each is checked against what it actually changed — the tmux options tmux is holding,
//! or the bytes on disk — rather than against its own reply.

mod common;

use common::Server;

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"
";

fn server() -> Server {
    let server = Server::start();
    server.write_config(CONFIG);
    server
}

#[test]
fn session_snapshot_reports_every_workspace_tab_pane_layout_and_agent_at_once() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    server.ok(
        &["pane", "split", &public, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );
    server.ok(
        &["workspace", "create", "--label", "second"],
        "cli:workspace:create",
        "workspace_created",
    );
    server.ok(
        &[
            "pane",
            "report-agent",
            &public,
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

    let result = server.ok(
        &["session", "snapshot"],
        "cli:session:snapshot",
        "session_snapshot",
    );
    let snap = &result["snapshot"];
    assert_eq!(snap["protocol"], 22, "herdr's socket protocol version");
    assert!(snap["version"].as_str().is_some(), "{snap}");

    // Two workspaces, and the panes of both.
    assert_eq!(snap["workspaces"].as_array().unwrap().len(), 2);
    assert_eq!(snap["panes"].as_array().unwrap().len(), 3);
    assert_eq!(snap["tabs"].as_array().unwrap().len(), 2);

    // One layout record per tab, in herdr's flat PaneLayoutSnapshot shape.
    let layouts = snap["layouts"].as_array().unwrap();
    assert_eq!(layouts.len(), 2);
    let split_tab = layouts
        .iter()
        .find(|l| l["panes"].as_array().unwrap().len() == 2)
        .unwrap_or_else(|| panic!("no two-pane layout in {layouts:?}"));
    assert_eq!(split_tab["splits"][0]["direction"], "down");
    assert!(split_tab["area"]["width"].as_u64().unwrap() > 0);

    // Exactly the panes with an agent in them.
    let agents = snap["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{agents:?}");
    assert_eq!(agents[0]["pane_id"], public.as_str());
    assert_eq!(agents[0]["agent_status"], "working");

    // No client is attached to a throwaway server, so nothing is focused — herdr's own
    // shape for that is null, not an error or an empty string.
    assert!(snap["focused_pane_id"].is_null(), "{snap}");
    assert!(snap["focused_workspace_id"].is_null(), "{snap}");
}

#[test]
fn terminal_title_set_writes_a_literal_title_and_clear_restores_what_was_there() {
    let server = server();
    // A titling scheme the user configured themselves, which `clear` must give back.
    server.tmux_ok(&["set-option", "-g", "set-titles", "off"]);
    server.tmux_ok(&["set-option", "-g", "set-titles-string", "mine: #S"]);

    let set = server.ok(
        &["terminal", "title", "set", "build:", "api"],
        "cli:terminal:title-set",
        "client_window_title",
    );
    assert_eq!(set["changed"], true);
    // No client is attached to this server, which is herdr's `no_foreground_client`.
    assert_eq!(set["reason"], "no_foreground_client");
    assert_eq!(set["title"], "build: api");
    assert_eq!(server.global_opt("set-titles"), "on");
    assert_eq!(server.global_opt("set-titles-string"), "build: api");

    let cleared = server.ok(
        &["terminal", "title", "clear"],
        "cli:terminal:title-clear",
        "client_window_title",
    );
    assert_eq!(cleared["changed"], true);
    assert_eq!(server.global_opt("set-titles"), "off");
    assert_eq!(server.global_opt("set-titles-string"), "mine: #S");

    // Clearing again has nothing saved to restore, and must not reset the user's own
    // titling to tmux's default.
    let again = server.ok(
        &["terminal", "title", "clear"],
        "cli:terminal:title-clear",
        "client_window_title",
    );
    assert_eq!(again["changed"], false);
    assert_eq!(server.global_opt("set-titles-string"), "mine: #S");
}

#[test]
fn a_title_holding_a_tmux_format_is_stored_escaped_so_it_displays_literally() {
    let server = server();
    server.ok(
        &["terminal", "title", "set", "#{pane_current_command} #S"],
        "cli:terminal:title-set",
        "client_window_title",
    );
    // Stored doubled…
    assert_eq!(
        server.global_opt("set-titles-string"),
        "##{pane_current_command} ##S"
    );
    // …so tmux's own expansion of the option yields the literal text back.
    assert_eq!(
        server.display(&server.pane(), "#{E:set-titles-string}"),
        "#{pane_current_command} #S"
    );
}

#[test]
fn a_second_title_set_keeps_the_users_original_as_the_thing_clear_restores() {
    let server = server();
    server.tmux_ok(&["set-option", "-g", "set-titles-string", "original"]);
    server.ok(
        &["terminal", "title", "set", "first"],
        "cli:terminal:title-set",
        "client_window_title",
    );
    server.ok(
        &["terminal", "title", "set", "second"],
        "cli:terminal:title-set",
        "client_window_title",
    );
    assert_eq!(server.global_opt("set-titles-string"), "second");
    server.ok(
        &["terminal", "title", "clear"],
        "cli:terminal:title-clear",
        "client_window_title",
    );
    assert_eq!(
        server.global_opt("set-titles-string"),
        "original",
        "clear must restore what the user had, not the previous taw title"
    );
}

#[test]
fn config_reset_keys_removes_the_keys_table_and_leaves_every_other_byte() {
    let server = server();
    let raw = "\
# my config
[theme]
name = \"nord\"

# my keybindings
[keys]
quit = \"q\"

[keys.agents]
open = \"o\"

[worktrees]
directory = \"~/wt\"
";
    server.write_config(raw);
    let result = server.ok(&["config", "reset-keys"], "cli:config:reset-keys", "ok");
    assert_eq!(result["removed"], true);

    let after = std::fs::read_to_string(server.config_dir().join("config.toml")).unwrap();
    assert_eq!(
        after,
        "\
# my config
[theme]
name = \"nord\"

[worktrees]
directory = \"~/wt\"
"
    );
    // The comment above the table went with it; every other comment stayed.
    assert!(after.contains("# my config"));
    assert!(!after.contains("keybindings"));
    // No temp file left behind.
    assert!(!server.config_dir().join("config.toml.tmp").exists());

    // Running it again is a no-op that says so, and does not rewrite the file.
    let again = server.ok(&["config", "reset-keys"], "cli:config:reset-keys", "ok");
    assert_eq!(again["removed"], false);
    assert_eq!(
        after,
        std::fs::read_to_string(server.config_dir().join("config.toml")).unwrap()
    );
    // And the result still loads.
    let checked = server.ok(&["config", "check"], "cli:config:check", "config_check");
    assert_eq!(checked["errors"], 0, "{checked}");
}

/// A config that starts with a BOM loads (the loader drops it), so `reset-keys` has to see
/// a `[keys]` table on that first line too, rather than reporting nothing to remove.
#[test]
fn config_reset_keys_finds_a_keys_table_in_a_config_that_starts_with_a_bom() {
    let server = server();
    server.write_config("\u{feff}[keys]\nquit = \"q\"\n\n[worktrees]\ndirectory = \"~/wt\"\n");

    let result = server.ok(&["config", "reset-keys"], "cli:config:reset-keys", "ok");
    assert_eq!(result["removed"], true, "{result}");

    let after = std::fs::read_to_string(server.config_dir().join("config.toml")).unwrap();
    assert_eq!(after, "[worktrees]\ndirectory = \"~/wt\"\n");
    let checked = server.ok(&["config", "check"], "cli:config:check", "config_check");
    assert_eq!(checked["errors"], 0, "{checked}");
}

#[test]
fn config_reset_keys_on_a_server_with_no_config_file_is_a_no_op() {
    let server = Server::start();
    let result = server.ok(&["config", "reset-keys"], "cli:config:reset-keys", "ok");
    assert_eq!(result["removed"], false);
    assert!(!server.config_dir().join("config.toml").exists());
}

#[test]
fn popup_close_succeeds_with_no_client_attached_rather_than_failing() {
    let server = server();
    let result = server.ok(&["popup", "close"], "cli:popup:close", "ok");
    // A throwaway server has no attached client, so there is no popup — which is a
    // success with nothing closed, not an error a tidying-up plugin has to handle.
    assert_eq!(result["clients"], 0);
    assert_eq!(result["closed"], 0);
}

#[test]
fn agent_view_set_stores_herdrs_own_params_json_in_the_global_option() {
    let server = server();
    assert_eq!(server.global_opt("@agent_wizard_view"), "");

    let filter = r#"{"op":"all","filters":[
        {"op":"eq","field":"status","value":"blocked"},
        {"op":"eq","field":"workspace_id","value":{"context":"current_workspace_id"}}
    ]}"#;
    let set = server.ok(
        &[
            "agent",
            "view",
            "set",
            "--source",
            "review",
            "--label",
            "Review queue",
            "--filter",
            filter,
        ],
        "cli:agent:view-set",
        "agent_view",
    );
    assert_eq!(set["active"], true);
    assert_eq!(set["source"], "review");
    assert_eq!(set["label"], "Review queue");

    // The option holds exactly herdr's `agent.view.set` params — the contract the popup
    // reads. Compared as parsed JSON, so key order is not part of it.
    let stored: serde_json::Value =
        serde_json::from_str(&server.global_opt("@agent_wizard_view")).unwrap();
    assert_eq!(stored["source"], "review");
    assert_eq!(stored["label"], "Review queue");
    assert_eq!(
        stored["filter"],
        serde_json::from_str::<serde_json::Value>(filter).unwrap()
    );
    // Nothing herdr would not have sent: no null label, no sort.
    let mut keys: Vec<&str> = stored
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["filter", "label", "source"]);

    // A view with no filter is legal and means "everything, under this label".
    server.ok(
        &["agent", "view", "set", "--source", "plain"],
        "cli:agent:view-set",
        "agent_view",
    );
    let stored: serde_json::Value =
        serde_json::from_str(&server.global_opt("@agent_wizard_view")).unwrap();
    assert_eq!(stored.as_object().unwrap().len(), 1, "{stored}");
    assert_eq!(stored["source"], "plain");
}

#[test]
fn agent_view_clear_unsets_the_option_and_respects_the_owning_source() {
    let server = server();
    server.ok(
        &["agent", "view", "set", "--source", "review"],
        "cli:agent:view-set",
        "agent_view",
    );

    // Another plugin tidying up must not take down this one's projection.
    let refused = server.ok(
        &["agent", "view", "clear", "--source", "someone-else"],
        "cli:agent:view-clear",
        "agent_view",
    );
    assert_eq!(refused["active"], true);
    assert_eq!(refused["source"], "review");
    assert_ne!(server.global_opt("@agent_wizard_view"), "");

    let cleared = server.ok(
        &["agent", "view", "clear", "--source", "review"],
        "cli:agent:view-clear",
        "agent_view",
    );
    assert_eq!(cleared["active"], false);
    // Unset, not set to an empty document: unset is what "no projection" means.
    assert_eq!(server.global_opt("@agent_wizard_view"), "");

    // Clearing when nothing is set is a success.
    let again = server.ok(
        &["agent", "view", "clear"],
        "cli:agent:view-clear",
        "agent_view",
    );
    assert_eq!(again["active"], false);
}

#[test]
fn the_popups_own_reader_gets_back_exactly_what_view_set_stored() {
    // `taw::cli::view::read` is the function the popup calls; this drives it against a
    // real server so the contract is checked from the reading side too, not only by
    // inspecting the option's text.
    let server = server();
    let tmux = taw::tmux::Tmux::with_socket_name(server.socket());
    assert!(taw::cli::view::read(&tmux).is_none(), "unset means no view");

    server.ok(
        &[
            "agent",
            "view",
            "set",
            "--source",
            "review",
            "--label",
            "Queue",
            "--filter",
            r#"{"op":"exists","field":"agent"}"#,
        ],
        "cli:agent:view-set",
        "agent_view",
    );
    let view = taw::cli::view::read(&tmux).expect("the view should read back");
    assert_eq!(view["source"], "review");
    assert_eq!(view["label"], "Queue");
    assert_eq!(view["filter"]["op"], "exists");

    // Something set by hand that is not a view reads as no view, so a hand-edited option
    // cannot stop the popup drawing the agent list at all.
    server.tmux_ok(&["set-option", "-g", "@agent_wizard_view", "not json"]);
    assert!(taw::cli::view::read(&tmux).is_none());
    server.tmux_ok(&[
        "set-option",
        "-g",
        "@agent_wizard_view",
        r#"{"label":"no source"}"#,
    ]);
    assert!(taw::cli::view::read(&tmux).is_none());
    server.tmux_ok(&[
        "set-option",
        "-g",
        "@agent_wizard_view",
        r#"{"source":"x","filter":{"op":"nope"}}"#,
    ]);
    assert!(
        taw::cli::view::read(&tmux).is_none(),
        "a filter the popup could not evaluate must read as no view"
    );
}

#[test]
fn agent_view_set_refuses_a_filter_the_popup_could_not_evaluate() {
    let server = server();
    for bad in [
        "not json",
        r#"{"op":"between","field":"status"}"#,
        r#"{"op":"eq","field":"colour","value":"red"}"#,
        r#"{"op":"eq","field":"status","value":{"context":"whatever"}}"#,
        r#"{"op":"all","filters":[{"op":"eq","field":"status"}]}"#,
    ] {
        let (code, envelope) = server.taw(&[
            "agent", "view", "set", "--source", "review", "--filter", bad,
        ]);
        assert_eq!(code, 1, "{bad} was accepted: {envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_argument", "{bad}");
        // Nothing was written: the popup reads this option in another process and could
        // not report a parse error to anyone.
        assert_eq!(
            server.global_opt("@agent_wizard_view"),
            "",
            "{bad} left something in the option"
        );
    }

    let (code, envelope) = server.taw(&["agent", "view", "set", "--source", "  "]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
}

#[test]
fn agent_view_set_stores_a_sort_and_takes_the_whole_params_object() {
    let server = server();

    // --- the individual flags, including --sort ---
    server.ok(
        &[
            "agent",
            "view",
            "set",
            "--source",
            "review",
            "--sort",
            r#"[{"field":"attention","order":"desc"},{"field":"agent"}]"#,
        ],
        "cli:agent:view-set",
        "agent_view",
    );
    let stored: serde_json::Value =
        serde_json::from_str(&server.global_opt("@agent_wizard_view")).unwrap();
    assert_eq!(
        stored["sort"],
        serde_json::json!([
            {"field": "attention", "order": "desc"},
            {"field": "agent"}
        ])
    );

    // One sort object is accepted as a one-entry list, because `[{…}]` is easy to get
    // wrong for the common single-field case.
    server.ok(
        &[
            "agent",
            "view",
            "set",
            "--source",
            "review",
            "--sort",
            r#"{"field":"attention"}"#,
        ],
        "cli:agent:view-set",
        "agent_view",
    );
    let stored: serde_json::Value =
        serde_json::from_str(&server.global_opt("@agent_wizard_view")).unwrap();
    assert_eq!(stored["sort"], serde_json::json!([{"field": "attention"}]));

    // --- the whole params object, for a caller that already has one ---
    let params = r#"{"source":"queue","label":"Blocked","filter":{"op":"eq","field":"status","value":"blocked"},"sort":[{"field":"attention","order":"desc"}]}"#;
    let set = server.ok(
        &["agent", "view", "set", "--params", params],
        "cli:agent:view-set",
        "agent_view",
    );
    assert_eq!(set["source"], "queue");
    assert_eq!(set["label"], "Blocked");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&server.global_opt("@agent_wizard_view"))
            .unwrap(),
        serde_json::from_str::<serde_json::Value>(params).unwrap()
    );

    // `--params` and the individual flags are mutually exclusive, and one of the two
    // routes must supply a source.
    let (code, _, _) = server.taw_raw(&[
        "agent",
        "view",
        "set",
        "--params",
        r#"{"source":"x"}"#,
        "--source",
        "y",
    ]);
    assert_eq!(code, 2, "clap should reject both routes at once");
    let (code, _, _) = server.taw_raw(&["agent", "view", "set"]);
    assert_eq!(code, 2, "--source is required without --params");
    let (code, envelope) = server.taw(&["agent", "view", "set", "--params", r#"{"label":"x"}"#]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
}

#[test]
fn agent_view_set_enforces_herdrs_rules_so_the_popup_never_drops_a_stored_view() {
    // Each of these is a query herdr rejects. Accepting one would write it, report
    // `active`, and have the popup then ignore it — telling the user their own successful
    // command was wrong.
    let server = server();
    let cases: &[(&str, &str)] = &[
        // Shaped right, means nothing: an empty `any` matches no agent at all.
        ("--filter", r#"{"op":"any","filters":[]}"#),
        ("--filter", r#"{"op":"all","filters":[]}"#),
        ("--filter", r#"{"op":"in","field":"agent","values":[]}"#),
        // Not one of the five agent states.
        ("--filter", r#"{"op":"eq","field":"status","value":"busy"}"#),
        // A context against the wrong id field.
        (
            "--filter",
            r#"{"op":"eq","field":"tab_id","value":{"context":"current_workspace_id"}}"#,
        ),
        // A value whose type its field can never hold.
        ("--filter", r#"{"op":"eq","field":"seen","value":"true"}"#),
        ("--filter", r#"{"op":"eq","field":"agent","value":7}"#),
        // A token that could not be written as a pane token in the first place.
        (
            "--filter",
            r#"{"op":"exists","field":{"token":"has space"}}"#,
        ),
        // Sort rules.
        ("--sort", r#"[{"field":"whenever"}]"#),
        ("--sort", r#"[{"field":"attention","order":"sideways"}]"#),
        ("--sort", r#"[{"field":"attention","reverse":true}]"#),
    ];
    for (flag, value) in cases {
        let (code, envelope) =
            server.taw(&["agent", "view", "set", "--source", "review", flag, value]);
        assert_eq!(code, 1, "{value} was accepted: {envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_argument", "{value}");
        assert_eq!(
            server.global_opt("@agent_wizard_view"),
            "",
            "{value} left something in the option"
        );
    }

    // The source charset, and the label limit — both herdr's.
    for source in ["has space", "sl/ash", &"x".repeat(121)] {
        let (code, envelope) = server.taw(&["agent", "view", "set", "--source", source]);
        assert_eq!(code, 1, "source {source:?} was accepted: {envelope}");
    }
    let (code, envelope) = server.taw(&[
        "agent",
        "view",
        "set",
        "--source",
        "review",
        "--label",
        &"x".repeat(33),
    ]);
    assert_eq!(code, 1, "{envelope}");

    // A field herdr's params object does not have is a typo, not an extension.
    let (code, envelope) = server.taw(&[
        "agent",
        "view",
        "set",
        "--params",
        r#"{"source":"x","filters":{"op":"exists","field":"agent"}}"#,
    ]);
    assert_eq!(code, 1, "{envelope}");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("filters"),
        "{envelope}"
    );
}

#[test]
fn agent_view_set_normalizes_source_and_label_the_way_herdr_stores_them() {
    let server = server();
    server.ok(
        &[
            "agent",
            "view",
            "set",
            "--source",
            "  review  ",
            "--label",
            "  Review\tqueue  ",
        ],
        "cli:agent:view-set",
        "agent_view",
    );
    let stored: serde_json::Value =
        serde_json::from_str(&server.global_opt("@agent_wizard_view")).unwrap();
    assert_eq!(stored["source"], "review");
    // Trimmed, with control characters removed — herdr's normalize_label.
    assert_eq!(stored["label"], "Reviewqueue");

    // `clear --source` matches the normalized form, so the caller can pass what it
    // originally typed.
    let cleared = server.ok(
        &["agent", "view", "clear", "--source", "  review  "],
        "cli:agent:view-clear",
        "agent_view",
    );
    assert_eq!(cleared["active"], false);
    assert_eq!(server.global_opt("@agent_wizard_view"), "");
}

/// `list-windows` of one session as `index name id` lines, read straight from tmux.
fn windows_of(server: &Server, session_id: &str) -> Vec<String> {
    server
        .tmux_ok(&[
            "list-windows",
            "-t",
            session_id,
            "-F",
            "#{window_index} #{window_name} #{window_id}",
        ])
        .lines()
        .map(str::to_string)
        .collect()
}

/// [`windows_of`]'s lines without the names, as `index id`: tmux renames a window after the
/// command running in it (`automatic-rename`), so a name read while its shell still starts is
/// not the name read a moment later, and a window compared by its whole line looks new.
fn indexes_and_ids(windows: &[String]) -> Vec<String> {
    windows
        .iter()
        .map(|w| {
            let index = w.split(' ').next().unwrap_or_default();
            let id = w.rsplit(' ').next().unwrap_or_default();
            format!("{index} {id}")
        })
        .collect()
}

/// `pane move --new-tab` makes the tab in the pane's own workspace, not in the session tmux
/// counts as current: with no client to go by, `break-pane` with no `-t` picked the newest
/// session, so the pane left its workspace whenever another one was made after it.
#[test]
fn pane_move_new_tab_stays_in_the_panes_own_session() {
    for (flags, label) in [
        (vec!["--no-focus"], None),
        (vec![], None),
        (vec!["--no-focus"], Some("moved")),
    ] {
        let server = server();
        let pane = server.pane();
        let public = server.public_pane_id(&pane);
        let own = server.tmux_ok(&["display-message", "-p", "-t", &pane, "#{session_id}"]);
        // A second pane keeps the pane's window alive when the first is broken out.
        server.ok(
            &[
                "pane",
                "split",
                &public,
                "--direction",
                "down",
                "--no-focus",
            ],
            "cli:pane:split",
            "pane_split",
        );
        // The newer session, used last: tmux's "best" session for a command with no client.
        server.tmux_ok(&["new-session", "-d", "-s", "newer"]);
        let other = server.tmux_ok(&["display-message", "-p", "-t", "=newer:", "#{session_id}"]);
        assert_ne!(own, other);
        let own_before = windows_of(&server, &own);
        let other_before = windows_of(&server, &other);
        // A window is renamed after the command running in it while its shell starts: the names
        // read above need not be the names read after the move, and the checks must not care.
        for session in [&own, &other] {
            server.tmux_ok(&["rename-window", "-t", &format!("{session}:^"), "renamed"]);
        }
        let next_index = own_before
            .iter()
            .map(|w| w.split(' ').next().unwrap().parse::<u32>().unwrap())
            .max()
            .unwrap()
            + 1;

        let mut args = vec!["pane", "move", public.as_str(), "--new-tab"];
        args.extend(flags.iter().copied());
        if let Some(label) = label {
            args.extend(["--label", label]);
        }
        server.ok(&args, "cli:pane:move", "pane_moved");

        let landed = server.tmux_ok(&["display-message", "-p", "-t", &pane, "#{session_id}"]);
        assert_eq!(
            landed,
            own,
            "`taw {}` left the pane's session",
            args.join(" ")
        );
        let own_after = windows_of(&server, &own);
        assert_eq!(own_after.len(), own_before.len() + 1, "{own_after:?}");
        let before = indexes_and_ids(&own_before);
        let created = own_after
            .iter()
            .zip(indexes_and_ids(&own_after))
            .find(|(_, w)| !before.contains(w))
            .map(|(line, _)| line)
            .unwrap_or_else(|| panic!("no new window in {own_after:?}"));
        assert!(
            created.starts_with(&format!("{next_index} ")),
            "the new tab is not at the next index {next_index}: {created}"
        );
        if let Some(label) = label {
            assert!(created.contains(&format!(" {label} ")), "{created}");
        }
        assert_eq!(
            indexes_and_ids(&windows_of(&server, &other)),
            indexes_and_ids(&other_before),
            "the other session was touched"
        );
    }
}
