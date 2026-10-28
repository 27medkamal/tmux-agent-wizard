//! `taw layout export | apply | set-split-ratio` against a throwaway tmux server.
//!
//! These are the three verbs herdr's `layout.*` socket methods map onto, and all three
//! are geometry: what they claim can only be checked against what tmux actually did, so
//! every assertion here reads the live window back rather than trusting the reply.
//!
//! The round trip is the load-bearing one: export a real window, apply the exported tree
//! onto a fresh tab, and the second tab must hold the same shape as the first.

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

/// A tab's geometry as a comparable string, walked from the `root` tree `layout export`
/// reports: every split's direction, and every pane's size and position.
///
/// Pane ids are deliberately left out. Two tabs with the same shape hold different panes,
/// so an id-carrying comparison could never be equal — what has to match is the cells.
fn structure(server: &Server, tab: &str) -> String {
    let exported = server.ok(
        &["layout", "export", tab],
        "cli:layout:export",
        "layout_export",
    );
    render(&exported["layout"]["root"])
}

fn render(node: &serde_json::Value) -> String {
    match node["type"].as_str() {
        Some("split") => format!(
            "split({},{})",
            node["direction"].as_str().unwrap_or("?"),
            [&node["first"], &node["second"]].map(render).join(",")
        ),
        _ => format!(
            "pane({}x{}@{},{})",
            node["width"], node["height"], node["x"], node["y"]
        ),
    }
}

/// Every pane's `width x height` in a tab, in tmux's own order.
fn sizes(server: &Server, target: &str) -> Vec<String> {
    server
        .tmux_ok(&[
            "list-panes",
            "-t",
            target,
            "-F",
            "#{pane_width}x#{pane_height}",
        ])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn export_reports_herdrs_layout_description_for_the_live_window() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);

    let single = server.ok(
        &["layout", "export", &public],
        "cli:layout:export",
        "layout_export",
    );
    let layout = &single["layout"];
    assert_eq!(layout["root"]["type"], "pane");
    assert_eq!(layout["root"]["pane_id"], public.as_str());
    assert_eq!(layout["focused_pane_id"], public.as_str());
    assert_eq!(layout["zoomed"], false);
    assert!(
        layout["tab_id"].as_str().unwrap().contains(":t"),
        "{layout}"
    );

    // Split twice in different directions: the tree must report both.
    let right = server.ok(
        &[
            "pane",
            "split",
            &public,
            "--direction",
            "right",
            "--ratio",
            "0.5",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let right_pane = right["pane"]["pane_id"].as_str().unwrap().to_string();
    server.ok(
        &["pane", "split", &right_pane, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );

    let exported = server.ok(
        &["layout", "export", &public],
        "cli:layout:export",
        "layout_export",
    );
    let root = &exported["layout"]["root"];
    assert_eq!(root["type"], "split");
    assert_eq!(root["direction"], "right");
    assert_eq!(root["first"]["type"], "pane");
    assert_eq!(root["second"]["type"], "split");
    assert_eq!(root["second"]["direction"], "down");

    // A zoomed window says so.
    server.ok(
        &["pane", "zoom", &public, "--on"],
        "cli:pane:zoom",
        "pane_zoom",
    );
    let zoomed = server.ok(
        &["layout", "export", &public],
        "cli:layout:export",
        "layout_export",
    );
    assert_eq!(zoomed["layout"]["zoomed"], true);
}

#[test]
fn a_layout_exported_from_one_tab_applies_onto_a_fresh_tab_with_the_same_shape() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);

    // Build a three-pane shape: one vertical split on the right half.
    let right = server.ok(
        &[
            "pane",
            "split",
            &public,
            "--direction",
            "right",
            "--ratio",
            "0.4",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let right_pane = right["pane"]["pane_id"].as_str().unwrap().to_string();
    server.ok(
        &[
            "pane",
            "split",
            &right_pane,
            "--direction",
            "down",
            "--ratio",
            "0.25",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let source_tab = server.ok(&["pane", "get", &public], "cli:pane:get", "pane")["pane"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    let exported = server.ok(
        &["layout", "export", &source_tab],
        "cli:layout:export",
        "layout_export",
    );
    let root = exported["layout"]["root"].to_string();

    let created = server.ok(
        &["tab", "create", "--label", "target"],
        "cli:tab:create",
        "tab_created",
    );
    let target_tab = created["tab"]["tab_id"].as_str().unwrap().to_string();

    let applied = server.ok(
        &["layout", "apply", &target_tab, &root],
        "cli:layout:apply",
        "layout_apply",
    );
    assert_eq!(applied["panes"], 3);
    assert_eq!(applied["panes_created"], 2);
    assert_eq!(applied["ratios_applied"], 2);

    // The reply's own tree must match the tree that went in, structurally.
    let out = &applied["layout"]["root"];
    assert_eq!(out["type"], "split");
    assert_eq!(out["direction"], "right");
    assert_eq!(out["second"]["type"], "split");
    assert_eq!(out["second"]["direction"], "down");

    // …and so must the geometry tmux actually produced, which is the claim that matters:
    // every split direction, and every pane's cell size and position.
    assert_eq!(
        structure(&server, &source_tab),
        structure(&server, &target_tab),
        "applied tab does not have the exported tab's geometry"
    );
    let source_window = server.display(&pane, "#{window_id}");
    let target_window = format!("@{}", target_tab.rsplit_once(":t").unwrap().1);
    assert_eq!(
        sizes(&server, &source_window),
        sizes(&server, &target_window)
    );
}

#[test]
fn apply_refuses_a_tab_that_already_holds_several_panes_rather_than_killing_one() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    server.ok(&["pane", "split", &public], "cli:pane:split", "pane_split");
    let before = sizes(&server, &server.display(&pane, "#{window_id}"));

    let tab = server.ok(&["pane", "get", &public], "cli:pane:get", "pane")["pane"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, envelope) = server.taw(&["layout", "apply", &tab, r#"{"type":"pane"}"#]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(message.contains("set-split-ratio"), "{message}");
    // Nothing was touched: the refusal is the whole point.
    assert_eq!(
        before,
        sizes(&server, &server.display(&pane, "#{window_id}"))
    );
}

#[test]
fn apply_rejects_a_malformed_tree_before_it_splits_anything() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    let tab = server.ok(&["pane", "get", &public], "cli:pane:get", "pane")["pane"]["tab_id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = sizes(&server, &server.display(&pane, "#{window_id}"));

    for bad in [
        "not json at all",
        r#"{"type":"leaf"}"#,
        r#"{"type":"split","direction":"right","ratio":0.5,"first":{"type":"pane"}}"#,
        r#"{"type":"split","direction":"sideways","ratio":0.5,"first":{"type":"pane"},"second":{"type":"pane"}}"#,
    ] {
        let (code, envelope) = server.taw(&["layout", "apply", &tab, bad]);
        assert_eq!(code, 1, "{bad} was accepted: {envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_argument", "{bad}");
        assert_eq!(
            before,
            sizes(&server, &server.display(&pane, "#{window_id}")),
            "{bad} changed the tab before failing"
        );
    }
}

#[test]
fn set_split_ratio_moves_the_named_divider_and_leaves_the_others_alone() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);
    let window = server.display(&pane, "#{window_id}");
    let width: u32 = server.display(&pane, "#{window_width}").parse().unwrap();

    let right = server.ok(
        &[
            "pane",
            "split",
            &public,
            "--direction",
            "right",
            "--ratio",
            "0.5",
        ],
        "cli:pane:split",
        "pane_split",
    );
    let right_pane = right["pane"]["pane_id"].as_str().unwrap().to_string();

    let applied = server.ok(
        &["layout", "set-split-ratio", &public, "0.25"],
        "cli:layout:set-split-ratio",
        "layout_split_ratio_set",
    );
    assert_eq!(applied["layout"]["root"]["type"], "split");
    let left_width: u32 = server.display(&pane, "#{pane_width}").parse().unwrap();
    let expected = (0.25 * f64::from(width)).round() as u32;
    assert!(
        left_width.abs_diff(expected) <= 1,
        "left pane is {left_width} cells, expected about {expected} of {width}"
    );

    // A nested split is addressed by the path `layout export` reports for it.
    server.ok(
        &["pane", "split", &right_pane, "--direction", "down"],
        "cli:pane:split",
        "pane_split",
    );
    let exported = server.ok(
        &["layout", "export", &public],
        "cli:layout:export",
        "layout_export",
    );
    // The path to address comes from the tree's own shape, which is what a caller reads.
    assert_eq!(exported["layout"]["root"]["second"]["type"], "split");
    // `display` speaks raw tmux ids, so the public id has to be turned back into `%N`.
    let right_raw =
        server.ok(&["pane", "get", &right_pane], "cli:pane:get", "pane")["pane"]["terminal_id"]
            .as_str()
            .unwrap()
            .to_string();
    let height: u32 = server
        .display(&right_raw, "#{window_height}")
        .parse()
        .unwrap();
    server.ok(
        &[
            "layout",
            "set-split-ratio",
            &public,
            "0.7",
            "--path",
            "root.second",
        ],
        "cli:layout:set-split-ratio",
        "layout_split_ratio_set",
    );
    let upper: u32 = server
        .display(&right_raw, "#{pane_height}")
        .parse()
        .unwrap();
    let want = (0.7 * f64::from(height)).round() as u32;
    assert!(
        upper.abs_diff(want) <= 1,
        "upper right pane is {upper} rows, expected about {want} of {height}"
    );
    // The outer divider did not move.
    let left_width_after: u32 = server.display(&pane, "#{pane_width}").parse().unwrap();
    assert_eq!(left_width, left_width_after);
    let _ = window;
}

#[test]
fn set_split_ratio_rejects_a_ratio_out_of_range_and_a_path_with_no_split() {
    let server = server();
    let pane = server.pane();
    let public = server.public_pane_id(&pane);

    // 0 and 1 are degenerate, not merely out of range: one side of the divider would get
    // no cells at all, and `resize-pane` would quietly clamp instead of refusing.
    for bad in ["1.5", "0", "1"] {
        let (code, envelope) = server.taw(&["layout", "set-split-ratio", &public, bad]);
        assert_eq!(code, 1, "{bad} was accepted: {envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_argument", "{bad}");
    }
    // A leading `-` is a flag to clap, so a negative ratio arrives after `--`.
    let (code, envelope) = server.taw(&["layout", "set-split-ratio", &public, "--", "-0.2"]);
    assert_eq!(code, 1, "{envelope}");
    // A single-pane tab has no split to move.
    let (code, envelope) = server.taw(&["layout", "set-split-ratio", &public, "0.5"]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "not_found");

    server.ok(&["pane", "split", &public], "cli:pane:split", "pane_split");
    let (code, envelope) = server.taw(&[
        "layout",
        "set-split-ratio",
        &public,
        "0.5",
        "--path",
        "root.second.second",
    ]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "not_found");
    let (code, envelope) = server.taw(&[
        "layout",
        "set-split-ratio",
        &public,
        "0.5",
        "--path",
        "root.middle",
    ]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
}

/// The pane `layout apply` makes for a leaf that names a folder opens in that folder as it is, a `#`
/// in its name and a trailing `;` included: tmux expands formats in a start directory (`C#Sharp`
/// became `C<session>harp`, the home folder when that is not there) and ends a command at a word that
/// ends in `;`.
#[test]
fn apply_opens_a_leafs_pane_in_its_folder_whatever_the_folders_name_holds() {
    let server = server();
    let tmp = tempfile::tempdir().unwrap();
    for (n, name) in ["C#Sharp", "semi;"].into_iter().enumerate() {
        let dir = tmp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap().display().to_string();
        let created = server.ok(
            &["tab", "create", "--label", &format!("leaf-{n}")],
            "cli:tab:create",
            "tab_created",
        );
        let tab = created["tab"]["tab_id"].as_str().unwrap().to_string();
        let tree = serde_json::json!({
            "type": "split",
            "direction": "right",
            "ratio": 0.5,
            "first": {"type": "pane"},
            "second": {"type": "pane", "cwd": dir},
        })
        .to_string();

        let applied = server.ok(
            &["layout", "apply", &tab, &tree],
            "cli:layout:apply",
            "layout_apply",
        );

        assert_eq!(applied["panes_created"], 1, "{applied}");
        let window = format!("@{}", tab.rsplit_once(":t").unwrap().1);
        let made = server.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            &format!("{window}.1"),
            "#{pane_id}",
        ]);
        assert!(
            common::wait_for(
                || server.display(&made, "#{pane_current_path}") == dir,
                std::time::Duration::from_secs(10)
            ),
            "the leaf's pane opened in {}, not {dir}",
            server.display(&made, "#{pane_current_path}")
        );
    }
}
