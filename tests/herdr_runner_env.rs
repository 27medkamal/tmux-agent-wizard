//! A test server starts without what herdr gives the panes it runs, whatever the process that runs the
//! tests has: `HERDR_ENV`, `HERDR_BIN_PATH`, `HERDR_PANE_ID`, `HERDR_SOCKET_PATH`, `HERDR_TAB_ID` and
//! `HERDR_WORKSPACE_ID`. A suite run from a pane of herdr has them, and the tests of
//! `@agent-wizard-herdr-env` (what a pane has with it on, and none of it with it off) would see the
//! runner's instead of the plugin's.

mod common;

use common::shell_env::HERDR_RUNNER_VARIABLES;

/// Runs the test below in a process of its own with each variable set (the environment is the process's),
/// which that test checks it has.
#[test]
fn a_test_server_starts_without_the_variables_herdr_gives_its_panes() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "a_test_server_started_from_a_process_with_herdr_variables_has_none",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .envs(HERDR_RUNNER_VARIABLES.map(|name| (name, "from-the-runner")))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("1 passed"),
        "the test that looks at the server did not run:\n{said}"
    );
}

/// Run by the test above, with the variables set; ignored so that nothing else runs it without them.
#[test]
#[ignore = "run by a_test_server_starts_without_the_variables_herdr_gives_its_panes"]
fn a_test_server_started_from_a_process_with_herdr_variables_has_none() {
    for name in HERDR_RUNNER_VARIABLES {
        assert!(
            std::env::var_os(name).is_some(),
            "this process was to have {name}, to show the server does not"
        );
    }
    let server = common::Server::start();
    for name in HERDR_RUNNER_VARIABLES {
        let said = server.tmux(&["show-environment", "-g", name]);
        assert!(
            said.is_err(),
            "the test server started with {name}: {said:?}"
        );
    }
    // Nor does a pane of it, which only has the names counted here and never printed.
    let counted = server.work_dir().join("herdr-count.txt");
    server.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "one:",
        &format!(
            "env | grep -c '^HERDR_' > '{0}.part'; mv '{0}.part' '{0}'",
            counted.display()
        ),
    ]);
    assert!(
        common::wait_for(|| counted.exists(), std::time::Duration::from_secs(20)),
        "the pane never wrote its count"
    );
    assert_eq!(std::fs::read_to_string(&counted).unwrap().trim(), "0");

    // A fixture of its own that starts its server through the home keeps them out as well.
    let home = common::shell_env::ShellHome::new();
    let mut command = std::process::Command::new("true");
    home.apply(&mut command);
    for name in HERDR_RUNNER_VARIABLES {
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()),
            "the home's start environment does not take {name} out"
        );
    }

    // And a test that wants one gives it (ServerOptions::env wins).
    let wanted = common::Server::start_with(common::ServerOptions {
        env: vec![("HERDR_PANE_ID".to_string(), "p_a-test-wants-it".to_string())],
        ..common::ServerOptions::default()
    });
    assert_eq!(
        wanted.tmux_ok(&["show-environment", "-g", "HERDR_PANE_ID"]),
        "HERDR_PANE_ID=p_a-test-wants-it"
    );
}
