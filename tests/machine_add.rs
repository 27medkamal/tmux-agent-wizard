//! `taw machine add` and `taw machine remove`: the config file is edited in place, with every comment and
//! line of it kept, and `taw machine list` shows the change. Nothing here runs ssh.

mod common;

use common::Server;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;

fn config_path(server: &Server) -> std::path::PathBuf {
    server.config_dir().join("config.toml")
}

fn config(server: &Server) -> String {
    std::fs::read_to_string(config_path(server)).unwrap()
}

fn listed(server: &Server) -> Vec<(String, String, Option<String>)> {
    let (code, stdout, stderr) = server.taw_raw(&["machine", "list"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    let envelope: Value = serde_json::from_str(&stdout).unwrap();
    envelope["result"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap().to_string(),
                m["destination"].as_str().unwrap().to_string(),
                m["taw"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn envelope(stdout: &str) -> Value {
    serde_json::from_str(stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"))
}

#[test]
fn a_machine_is_added_listed_and_removed_with_the_file_kept_as_it_was() {
    let server = Server::start();
    let original = "# my config\n[theme]\n# a comment\nname = \"x\"\n\n# machines:\n[machines]\n# studio = \"studio.local\"\npi = \"pi\"\n\n[plugins]\nx = 1\n";
    server.write_config(original);

    let (code, stdout, stderr) = server.taw_raw(&["machine", "add", "studio", "studio.local"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    let got = envelope(&stdout);
    assert_eq!(got["id"], "cli:machine:add", "{stdout}");
    assert_eq!(got["result"]["type"], "machine_added", "{stdout}");
    assert_eq!(got["result"]["machine"]["name"], "studio", "{stdout}");
    assert_eq!(
        got["result"]["note"], "The app shows this the next time it opens.",
        "{stdout}"
    );
    assert_eq!(
        config(&server),
        original.replace("pi = \"pi\"\n", "pi = \"pi\"\nstudio = \"studio.local\"\n")
    );

    let (code, stdout, stderr) = server.taw_raw(&[
        "machine",
        "add",
        "den",
        "me@den",
        "--taw",
        "~/.local/bin/taw",
    ]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert_eq!(
        listed(&server),
        [
            ("pi".to_string(), "pi".to_string(), None),
            ("studio".to_string(), "studio.local".to_string(), None),
            (
                "den".to_string(),
                "me@den".to_string(),
                Some("~/.local/bin/taw".to_string())
            ),
        ]
    );
    assert!(
        config(&server)
            .contains("\n[machines.den]\nssh = \"me@den\"\ntaw = \"~/.local/bin/taw\"\n")
    );

    for name in ["den", "studio"] {
        let (code, stdout, stderr) = server.taw_raw(&["machine", "remove", name]);
        assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
        let got = envelope(&stdout);
        assert_eq!(got["id"], "cli:machine:remove", "{stdout}");
        assert_eq!(got["result"]["type"], "machine_removed", "{stdout}");
        assert_eq!(got["result"]["removed"], name, "{stdout}");
    }
    assert_eq!(config(&server), original);
    assert_eq!(
        listed(&server),
        [("pi".to_string(), "pi".to_string(), None)]
    );
}

#[test]
fn a_missing_config_is_made_with_just_the_table() {
    let server = Server::start();
    assert!(!config_path(&server).exists());
    let (code, stdout, stderr) = server.taw_raw(&["machine", "add", "pi", "pi"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert_eq!(config(&server), "[machines]\npi = \"pi\"\n");
    assert_eq!(listed(&server).len(), 1);
}

#[test]
fn a_name_already_listed_and_a_name_not_listed_are_errors_that_leave_the_file_alone() {
    let server = Server::start();
    let original = "[machines]\npi = \"pi\"\n";
    server.write_config(original);

    let (code, stdout, _) = server.taw_raw(&["machine", "add", "pi", "elsewhere"]);
    assert_ne!(code, 0, "{stdout}");
    let got = envelope(&stdout);
    assert_eq!(
        got["error"]["message"], "a machine named pi is already listed",
        "{stdout}"
    );

    let (code, stdout, _) = server.taw_raw(&["machine", "add", "Bad", "h"]);
    assert_ne!(code, 0, "{stdout}");
    assert!(
        envelope(&stdout)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lowercase letters"),
        "{stdout}"
    );

    let (code, stdout, _) = server.taw_raw(&["machine", "remove", "den"]);
    assert_ne!(code, 0, "{stdout}");
    assert_eq!(
        envelope(&stdout)["error"]["message"],
        "no machine named den is listed",
        "{stdout}"
    );
    assert_eq!(config(&server), original);
}

#[test]
fn a_config_that_does_not_parse_is_not_touched() {
    let server = Server::start();
    let original = "[machines\npi = \n";
    server.write_config(original);
    let (code, stdout, _) = server.taw_raw(&["machine", "add", "den", "den"]);
    assert_ne!(code, 0, "{stdout}");
    assert_eq!(config(&server), original);
}

#[test]
fn a_config_that_is_a_link_stays_a_link_and_keeps_its_permissions() {
    let server = Server::start();
    let real = server.work_dir().join("dotfiles-config.toml");
    std::fs::write(&real, "# kept\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&real, config_path(&server)).unwrap();

    let (code, stdout, stderr) = server.taw_raw(&["machine", "add", "pi", "pi"]);
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert!(
        std::fs::symlink_metadata(config_path(&server))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "# kept\n\n[machines]\npi = \"pi\"\n"
    );
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
