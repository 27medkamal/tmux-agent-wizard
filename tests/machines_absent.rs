//! With no `[machines]` table taw behaves as it did before machines existed: the same answers, no ssh
//! run, and nothing made on disk for one.
//!
//! The answers are pinned here word for word, as they were when this was written (the output of the
//! build before machines, read off that build), not as "what the build gives": a change to them is a
//! change to what every user without machines sees.

mod common;

use common::Server;
use serde_json::{Value, json};

/// A server whose config file is `config` (none when `None`), and which has run `taw` on each of `verbs`.
fn run(config: Option<&str>, verbs: &[&[&str]]) -> (Server, Vec<(i32, String, String)>) {
    let server = Server::start();
    if let Some(config) = config {
        server.write_config(config);
    }
    let runs = verbs.iter().map(|verb| server.taw_raw(verb)).collect();
    (server, runs)
}

fn assert_nothing_for_machines(server: &Server) {
    assert_eq!(server.ssh_calls(), Vec::<String>::new(), "no ssh was run");
    // `m` is the control folder, which a `--machine` run makes (the last test shows it); `machines` is
    // not made by anything yet and becomes meaningful when a later batch writes that folder.
    for folder in ["m", "machines"] {
        let path = server.state_dir().join(folder);
        assert!(!path.exists(), "{} was made", path.display());
    }
}

/// What `workspace list` says on the fixture, word for word, and that no ssh ran and no folder was made
/// for machines.
fn assert_workspace_list_as_before(config: Option<&str>) {
    let (server, runs) = run(config, &[&["workspace", "list"]]);
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert!(
        stdout.ends_with('\n') && stdout.matches('\n').count() == 1,
        "{stdout:?}"
    );
    let mut envelope: Value = serde_json::from_str(stdout).unwrap();
    // The one thing that is the fixture's own: the folder its session started in.
    let cwd = envelope["result"]["workspaces"][0]["cwd"].take();
    assert!(
        cwd.as_str().is_some_and(|cwd| cwd.starts_with('/')),
        "{cwd}"
    );
    assert_eq!(
        envelope,
        json!({
            "id": "cli:workspace:list",
            "result": {
                "workspaces": [{
                    "workspace_id": "w0",
                    "label": "one",
                    "number": 0,
                    "focused": false,
                    "active_tab_id": "w0:t0",
                    "agent_status": null,
                    "pane_count": 1,
                    "tab_count": 1,
                    "cwd": null,
                    "branch": null
                }],
                "type": "workspace_list"
            }
        })
    );
    // The line is the same keys in the same order, as `jq` or a plugin reads it.
    assert!(
        stdout.starts_with(
            "{\"id\":\"cli:workspace:list\",\"result\":{\"workspaces\":[{\"workspace_id\":\"w0\",\
             \"label\":\"one\",\"number\":0,\"focused\":false,\"active_tab_id\":\"w0:t0\",\
             \"agent_status\":null,\"pane_count\":1,\"tab_count\":1,\"cwd\":\""
        ),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("\",\"branch\":null}],\"type\":\"workspace_list\"}}\n"),
        "{stdout}"
    );
    assert_nothing_for_machines(&server);
}

#[test]
fn workspace_list_answers_as_before_and_runs_no_ssh() {
    assert_workspace_list_as_before(Some("# nothing about machines\n"));
}

#[test]
fn workspace_list_with_no_config_file_at_all_answers_the_same() {
    assert_workspace_list_as_before(None);
}

#[test]
fn config_check_says_the_same_lines_for_a_file_with_no_machines() {
    let (server, runs) = run(
        Some("# nothing about machines\nmodel = \"x\"\n"),
        &[&["config", "check"]],
    );
    let file = server.config_dir().join("config.toml");
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        &format!(
            "{{\"id\":\"cli:config:check\",\"result\":{{\"config_file\":\"{}\",\"errors\":0,\
             \"diagnostics\":[{{\"level\":\"warning\",\"message\":\"unknown config key: model\"}}],\
             \"type\":\"config_check\"}}}}\n",
            file.display()
        )
    );
    assert_nothing_for_machines(&server);
}

#[test]
fn config_check_with_no_file_at_all_says_nothing_is_wrong() {
    let (server, runs) = run(None, &[&["config", "check"]]);
    let file = server.config_dir().join("config.toml");
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        &format!(
            "{{\"id\":\"cli:config:check\",\"result\":{{\"config_file\":\"{}\",\"errors\":0,\
             \"diagnostics\":[],\"type\":\"config_check\"}}}}\n",
            file.display()
        )
    );
    assert_nothing_for_machines(&server);
}

#[test]
fn machine_list_with_no_table_is_empty_runs_no_ssh_and_makes_no_folder() {
    let (server, runs) = run(
        Some("# nothing about machines\n"),
        &[&["machine", "list"], &["machine", "list"]],
    );
    for (code, stdout, stderr) in &runs {
        assert_eq!((*code, stderr.as_str()), (0, ""));
        assert_eq!(
            stdout,
            "{\"id\":\"cli:machine:list\",\"result\":{\"machines\":[],\"type\":\"machine_list\"}}\n"
        );
    }
    assert_nothing_for_machines(&server);
}

#[test]
fn config_check_on_a_file_that_still_has_herdrs_tables_says_what_it_said_before() {
    let (server, runs) = run(
        Some("[server]\nheadless_cols = 120\n[remote]\nhost = \"x\"\n"),
        &[&["config", "check"]],
    );
    let file = server.config_dir().join("config.toml");
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        &format!(
            "{{\"id\":\"cli:config:check\",\"result\":{{\"config_file\":\"{}\",\"errors\":0,\
             \"diagnostics\":[{{\"level\":\"warning\",\"message\":\"unknown config key: remote.host\"}},\
             {{\"level\":\"warning\",\"message\":\"unknown config key: server.headless_cols\"}}],\
             \"type\":\"config_check\"}}}}\n",
            file.display()
        )
    );
    assert_nothing_for_machines(&server);
}

#[test]
fn config_check_on_a_good_machines_table_says_nothing_is_wrong_and_runs_no_ssh() {
    let (server, runs) = run(
        Some(
            "[machines]\nstudio = \"studio.local\"\n[machines.pi]\nssh = \"pi\"\ntaw = \"~/.local/bin/taw\"\n",
        ),
        &[&["config", "check"]],
    );
    let file = server.config_dir().join("config.toml");
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        &format!(
            "{{\"id\":\"cli:config:check\",\"result\":{{\"config_file\":\"{}\",\"errors\":0,\
             \"diagnostics\":[],\"type\":\"config_check\"}}}}\n",
            file.display()
        )
    );
    assert_nothing_for_machines(&server);
}

#[test]
fn config_check_on_a_machines_table_with_drops_says_each_line_as_a_warning() {
    let (server, runs) = run(
        Some(
            "[machines]\n\"Studio\" = \"box.local\"\nattic = \"\"\npi = \"pi\"\nloft = \"pi\"\n\
             [machines.den]\nssh = \"den\"\nport = 22\n",
        ),
        &[&["config", "check"]],
    );
    let file = server.config_dir().join("config.toml");
    let (code, stdout, stderr) = &runs[0];
    assert_eq!((*code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        &format!(
            "{{\"id\":\"cli:config:check\",\"result\":{{\"config_file\":\"{}\",\"errors\":0,\
             \"diagnostics\":[\
             {{\"level\":\"warning\",\"message\":\"machines.Studio: a machine's name is lowercase letters, digits, - and _, starting with a letter, at most 32 characters\"}},\
             {{\"level\":\"warning\",\"message\":\"machines.attic: no ssh destination (what you type after ssh)\"}},\
             {{\"level\":\"warning\",\"message\":\"machines.loft: the same ssh destination as machines.pi (pi)\"}},\
             {{\"level\":\"warning\",\"message\":\"unknown config key: machines.den.port\"}}],\
             \"type\":\"config_check\"}}}}\n",
            file.display()
        )
    );
    // Checking runs no ssh and writes nothing: only a machine that is used makes either.
    assert_nothing_for_machines(&server);
}

#[test]
fn the_checks_above_could_see_what_they_say_did_not_happen() {
    // The positive half: with a machine listed, a `--machine` run does reach ssh (the recorder logs it)
    // and makes the control folder, so an empty log and no folder above mean something.
    let (server, runs) = run(
        Some("[machines]\nstudio = \"studio.local\"\n"),
        &[&["--machine", "studio", "workspace", "list"]],
    );
    assert_eq!(runs[0].0, 1, "{}", runs[0].1);
    assert_eq!(server.ssh_calls().len(), 1);
    assert!(server.state_dir().join("m").is_dir());
}
