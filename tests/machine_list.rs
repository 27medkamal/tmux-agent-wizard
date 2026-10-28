//! `taw machine list`: the machines of `[machines]` in the order the file lists them, with what the
//! machine files say of each. Nothing here runs ssh or makes a folder: it reads the config, and the
//! file of each machine when one is there.

mod common;

use common::Server;
use serde_json::{Value, json};

fn server_with(config: Option<&str>) -> Server {
    let server = Server::start();
    if let Some(config) = config {
        server.write_config(config);
    }
    server
}

/// Writes `<state>/machines/<name>.json`, the way the app that keeps the connections will.
fn machine_file(server: &Server, name: &str, text: &str) {
    let dir = server.state_dir().join("machines");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), text).unwrap();
}

fn list(server: &Server) -> (i32, String, String) {
    server.taw_raw(&["machine", "list"])
}

fn machines_of(stdout: &str) -> Vec<Value> {
    let envelope: Value = serde_json::from_str(stdout).unwrap();
    assert_eq!(envelope["id"], "cli:machine:list", "{stdout}");
    assert_eq!(envelope["result"]["type"], "machine_list", "{stdout}");
    envelope["result"]["machines"].as_array().unwrap().clone()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn with_no_table_the_list_is_empty() {
    let server = server_with(Some("# nothing about machines\n"));
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "{\"id\":\"cli:machine:list\",\"result\":{\"machines\":[],\"type\":\"machine_list\"}}\n"
    );
}

#[test]
fn with_no_config_file_the_list_is_empty() {
    let server = server_with(None);
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (0, ""));
    assert!(machines_of(&stdout).is_empty(), "{stdout}");
}

#[test]
fn two_machines_come_in_the_order_of_the_file_with_their_destinations_and_the_long_forms_taw() {
    // `studio` is written before `pi`, which a sorted table would turn round.
    let server = server_with(Some(
        "[machines]\nstudio = \"studio.local\"\n[machines.pi]\nssh = \"pi\"\ntaw = \"~/.local/bin/taw\"\n",
    ));
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!(
        stdout,
        "{\"id\":\"cli:machine:list\",\"result\":{\"machines\":[\
         {\"name\":\"studio\",\"destination\":\"studio.local\",\"taw\":null,\"live\":false,\
         \"last_seen\":null,\"taw_version\":null,\"protocol\":null,\"words\":\"\"},\
         {\"name\":\"pi\",\"destination\":\"pi\",\"taw\":\"~/.local/bin/taw\",\"live\":false,\
         \"last_seen\":null,\"taw_version\":null,\"protocol\":null,\"words\":\"\"}],\
         \"type\":\"machine_list\"}}\n"
    );
}

#[test]
fn a_machine_file_is_read_and_its_fields_come_through() {
    let server = server_with(Some("[machines]\nstudio = \"studio.local\"\npi = \"pi\"\n"));
    machine_file(
        &server,
        "studio",
        r#"{"name":"studio","state":{"spaces":[]},"received_at":1790000000100,"last_seen":1790000000500,
            "live":false,"words":"connected: taw 0.1.0","taw_version":"0.1.0","protocol":1,"this_server":false}"#,
    );
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (0, ""));
    let machines = machines_of(&stdout);
    assert_eq!(
        machines[0],
        json!({
            "name": "studio",
            "destination": "studio.local",
            "taw": null,
            "live": false,
            "last_seen": 1790000000500u64,
            "taw_version": "0.1.0",
            "protocol": 1,
            // With no keeper the words are history and are not shown.
            "words": ""
        })
    );
    // The other machine has no file: nothing is known of it.
    assert_eq!(machines[1]["last_seen"], Value::Null);
    assert_eq!(machines[1]["words"], "");
}

#[test]
fn a_file_that_says_it_is_live_is_not_while_no_app_keeps_the_connections() {
    let server = server_with(Some("[machines]\nstudio = \"studio.local\"\n"));
    machine_file(
        &server,
        "studio",
        &format!(
            "{{\"name\":\"studio\",\"live\":true,\"last_seen\":{},\"taw_version\":\"0.1.0\",\"words\":\"connected: taw 0.1.0\"}}",
            now_ms()
        ),
    );
    let (code, stdout, _) = list(&server);
    assert_eq!(code, 0);
    let machine = &machines_of(&stdout)[0];
    assert_eq!(machine["live"], false, "{stdout}");
    // What the header said was true while a keeper kept the file; with none it is history, so the
    // words are empty while the times and the version still read.
    assert_eq!(machine["words"], "", "{stdout}");
    assert_eq!(machine["taw_version"], "0.1.0", "{stdout}");
}

#[test]
fn a_file_with_unknown_and_missing_fields_still_reads() {
    let server = server_with(Some("[machines]\nstudio = \"studio.local\"\npi = \"pi\"\n"));
    machine_file(
        &server,
        "studio",
        r#"{"words":"connecting","taw_version":"0.3.0","from_a_newer_taw":{"x":[1,2]}}"#,
    );
    machine_file(&server, "pi", "{}");
    let (code, stdout, _) = list(&server);
    assert_eq!(code, 0, "{stdout}");
    let machines = machines_of(&stdout);
    assert_eq!(machines[0]["words"], "");
    assert_eq!(machines[0]["taw_version"], "0.3.0");
    assert_eq!(machines[0]["last_seen"], Value::Null);
    assert_eq!(machines[0]["protocol"], Value::Null);
    assert_eq!(machines[1]["words"], "");
}

#[test]
fn a_broken_machine_file_counts_as_none() {
    let server = server_with(Some(
        "[machines]\nstudio = \"studio.local\"\npi = \"pi\"\nattic = \"attic\"\n",
    ));
    machine_file(&server, "studio", "this is not json");
    machine_file(
        &server,
        "pi",
        "{\"last_seen\":\"yesterday\",\"words\":\"x\"}",
    );
    machine_file(&server, "attic", "");
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (0, ""));
    for machine in machines_of(&stdout) {
        assert_eq!(machine["words"], "", "{machine}");
        assert_eq!(machine["last_seen"], Value::Null, "{machine}");
        assert_eq!(machine["live"], false, "{machine}");
    }
}

#[test]
fn a_line_taw_drops_refuses_the_table_in_the_words_a_machine_run_uses() {
    let server = server_with(Some(
        "[machines]\nstudio = \"studio.local\"\nattic = \"\"\n",
    ));
    let (code, stdout, stderr) = list(&server);
    assert_eq!((code, stderr.as_str()), (1, ""));
    let envelope: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["id"], "cli:machine:list");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    let message = envelope["error"]["message"].as_str().unwrap().to_string();
    assert!(
        message.contains("has a line in [machines] taw drops")
            && message.contains("machines.attic: no ssh destination (what you type after ssh)"),
        "{message}"
    );

    // `taw --machine` says it in the same words.
    let (code, stdout, _) = server.taw_raw(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 1);
    let forwarded: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(forwarded["error"]["message"], message.as_str());
    assert_eq!(server.ssh_calls().len(), 0, "a refused table runs no ssh");
}

#[test]
fn a_config_that_is_not_toml_gives_the_envelope_and_the_generic_warning() {
    let server = server_with(Some("[machines\nstudio = \n"));
    let (code, stdout, stderr) = list(&server);
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stderr.contains("taw: warning: using default settings, config "),
        "{stderr}"
    );
    let envelope: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("could not be read"),
        "{stdout}"
    );
}

#[test]
fn a_config_with_a_wrong_type_elsewhere_still_lists_and_says_the_generic_warning() {
    let server = server_with(Some(
        "[sandbox]\nprofile = 5\n[machines]\nstudio = \"studio.local\"\n",
    ));
    let (code, stdout, stderr) = list(&server);
    assert_eq!(code, 0, "{stdout}");
    let machines = machines_of(&stdout);
    assert_eq!(machines.len(), 1, "{stdout}");
    assert_eq!(machines[0]["name"], "studio");
    // The same words `taw workspace list` writes for the same file.
    let (_, _, other) = server.taw_raw(&["workspace", "list"]);
    let warning = other
        .lines()
        .find(|line| line.contains("using default settings"))
        .unwrap_or_else(|| panic!("workspace list did not warn: {other}"));
    assert!(stderr.contains(warning), "{stderr}");
}

#[test]
fn listing_runs_no_ssh_and_makes_no_folder() {
    let server = server_with(Some("[machines]\nstudio = \"studio.local\"\n"));
    let (code, stdout, _) = list(&server);
    assert_eq!(code, 0, "{stdout}");
    // The positive half: the machine is listed, so the checks below are about a list that was made.
    assert_eq!(machines_of(&stdout).len(), 1);
    assert_eq!(server.ssh_calls().len(), 0);
    for folder in ["m", "machines"] {
        assert!(!server.state_dir().join(folder).exists(), "{folder}");
    }
}
