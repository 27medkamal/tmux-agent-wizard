//! `taw --machine <name> <verb>`: the verb runs on that machine, through ssh, and its answer comes back as
//! it was given. The machine here is the stand-in of `tests/common/machine.rs`: another throwaway server
//! with a `taw` of its own, reached by an ssh that is a script. No real ssh runs.

mod common;

use common::machine::{Failure, FarSide, StandIn};
use common::{Server, ServerOptions};
use serde_json::{Value, json};
use taw::machine::Machine;
use taw::machine::ssh::{Mode, Ssh, command};
use taw::paths::Paths;

/// This side (session `here`, with `studio` and `pi` in its `[machines]`), the far side `studio` (session
/// `garden`) and the stand-in that reaches it. `pi` has no far side: a call to it would not resolve.
struct Rig {
    here: Server,
    studio: FarSide,
    stand_in: StandIn,
}

impl Rig {
    fn new() -> Rig {
        let here = Server::start_with(ServerOptions {
            session: "here".to_string(),
            ..ServerOptions::default()
        });
        here.write_config("[machines]\nstudio = \"studio\"\npi = \"pi.local\"\n");
        let studio = FarSide::new("studio");
        let stand_in = StandIn::new(&[&studio]);
        Rig {
            here,
            studio,
            stand_in,
        }
    }

    /// Runs `taw <args>` on this side with the stand-in as its ssh: (status, stdout, stderr).
    fn taw(&self, args: &[&str]) -> (i32, String, String) {
        let bin = self.stand_in.bin.display().to_string();
        self.here.taw_raw_with_env(args, &[("TAW_SSH_BIN", &bin)])
    }

    fn envelope(&self, args: &[&str]) -> (i32, Value) {
        let (code, stdout, stderr) = self.taw(args);
        let value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("{args:?}: {e}: stdout {stdout:?}, stderr {stderr:?}"));
        (code, value)
    }
}

fn labels(envelope: &Value) -> Vec<String> {
    envelope["result"]["workspaces"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter_map(|w| w["label"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_verb_runs_on_the_far_side_and_its_envelope_comes_back_as_it_was_given() {
    let rig = Rig::new();
    let (code, envelope) = rig.envelope(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 0);
    assert_eq!(envelope["id"], "cli:workspace:list");
    assert_eq!(
        labels(&envelope),
        ["garden"],
        "the far side, never this one"
    );

    // Each side shows its own.
    let (_, ours) = rig.here.taw(&["workspace", "list"]);
    assert_eq!(labels(&ours), ["here"]);
    assert_eq!(rig.stand_in.calls().len(), 1);
}

#[test]
fn the_option_may_come_after_pretty_or_as_one_word_and_pretty_arrives() {
    let rig = Rig::new();
    let (_, plain, _) = rig.taw(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(
        plain.trim_end().lines().count(),
        1,
        "one line without --pretty: {plain}"
    );
    for args in [
        vec!["--pretty", "--machine", "studio", "workspace", "list"],
        vec!["--machine", "studio", "--pretty", "workspace", "list"],
        vec!["--pretty", "--machine=studio", "workspace", "list"],
    ] {
        let (code, stdout, stderr) = rig.taw(&args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(
            stdout.trim_end().lines().count() > 3,
            "{args:?} indented: {stdout}"
        );
        assert!(
            stdout.contains("\n  \"id\": \"cli:workspace:list\""),
            "{args:?}: {stdout}"
        );
    }
}

#[test]
fn text_arguments_reach_the_far_side_as_typed_and_nothing_runs() {
    let rig = Rig::new();
    let scratch = tempfile::tempdir().unwrap();
    let pwned = scratch.path().join("pwned");
    let typed = format!(
        "it's  a $(touch {}) `touch {}` ; b",
        pwned.display(),
        pwned.display()
    );
    let (code, envelope) =
        rig.envelope(&["--machine", "studio", "workspace", "rename", "w0", &typed]);
    assert_eq!(code, 0, "{envelope}");
    let (_, listed) = rig.envelope(&["--machine", "studio", "workspace", "list"]);
    let label = listed["result"]["workspaces"][0]["label"]
        .as_str()
        .unwrap()
        .to_string();
    // The far side stores what it was given, up to what tmux itself does to a name.
    let squeezed = |s: &str| s.replace(['.', ':'], "_");
    assert_eq!(squeezed(&label), squeezed(&typed), "stored as typed");
    // Words the far shell would expand or read as something else if they went over bare.
    for word in ["{a,b}", "x{1..3}", "{}", "=x", "a b{c,d}", "!", "^", ","] {
        let (code, envelope) =
            rig.envelope(&["--machine", "studio", "workspace", "rename", "w0", word]);
        assert_eq!(code, 0, "{word}: {envelope}");
        let (_, listed) = rig.envelope(&["--machine", "studio", "workspace", "list"]);
        assert_eq!(
            labels(&listed)
                .iter()
                .map(|l| squeezed(l))
                .collect::<Vec<_>>(),
            [squeezed(word)],
            "{word} stored as typed"
        );
    }
    // A positive wait first: the far side did take the rename, so a file that was to be made by now
    // would be there.
    assert!(!pwned.exists(), "a shell on the way ran the text");
}

#[test]
fn a_far_error_passes_through_with_status_one() {
    let rig = Rig::new();
    let (code, envelope) = rig.envelope(&["--machine", "studio", "workspace", "get", "w99"]);
    assert_eq!(code, 1);
    assert_eq!(envelope["id"], "cli:workspace:get");
    assert_eq!(envelope["error"]["code"], "not_found");
}

#[test]
fn a_refused_connection_is_a_machine_unreachable_envelope_and_one_call() {
    let rig = Rig::new();
    rig.stand_in.fail_as("studio", Failure::Refused);
    let (code, stdout, stderr) = rig.taw(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 1);
    assert_eq!(stderr, "");
    let expected = json!({"id": "cli:machine", "error": {
        "code": "machine_unreachable",
        "message": "studio: ssh: connect to host studio port 22: Connection refused"}});
    assert_eq!(stdout, format!("{expected}\n"));
    // Never retried: the one call is the only one there will be.
    assert_eq!(rig.stand_in.calls().len(), 1);

    let (_, pretty, _) = rig.taw(&["--pretty", "--machine", "studio", "workspace", "list"]);
    assert_eq!(
        pretty,
        format!("{}\n", serde_json::to_string_pretty(&expected).unwrap())
    );
    assert_eq!(rig.stand_in.calls().len(), 2);
}

#[test]
fn each_other_way_a_connection_ends_is_told_in_its_own_words() {
    let rig = Rig::new();
    let table = [
        (
            Failure::TimedOut,
            "studio: ssh: connect to host studio port 22: Operation timed out",
        ),
        (
            Failure::PermissionDenied,
            "studio: studio: Permission denied (publickey).",
        ),
        (Failure::HostKey, "studio: Host key verification failed."),
        (
            Failure::NoTaw,
            "studio: no taw there: install it as you did here",
        ),
    ];
    for (how, message) in table {
        rig.stand_in.fail_as("studio", how.clone());
        let (code, envelope) = rig.envelope(&["--machine", "studio", "agent", "list"]);
        assert_eq!(code, 1, "{how:?}");
        assert_eq!(envelope["id"], "cli:machine", "{how:?}");
        assert_eq!(envelope["error"]["code"], "machine_unreachable", "{how:?}");
        assert_eq!(envelope["error"]["message"], message, "{how:?}");
    }
}

#[test]
fn what_the_far_taw_says_on_stderr_and_its_status_are_ours() {
    let rig = Rig::new();
    rig.stand_in
        .fail_as("studio", Failure::OldTaw("0.1.0".to_string()));
    let (code, stdout, stderr) = rig.taw(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("unrecognized subcommand 'machine'"),
        "{stderr}"
    );
}

#[test]
fn a_verb_that_belongs_to_this_side_is_refused_before_any_ssh() {
    let rig = Rig::new();
    let table = [
        (
            vec!["ui"],
            "taw --machine studio ui: the popup is that machine's own; use the app: taw app",
        ),
        (
            vec!["app"],
            "taw --machine studio app: run taw app here; it lists studio",
        ),
        (
            vec!["open", "w0"],
            "taw --machine studio open: needs --json; go in from the app: taw app",
        ),
        (
            vec!["agent", "attach", "%3"],
            "taw --machine studio agent attach: use the app: taw app --go studio:<target>",
        ),
        (
            vec!["machine", "list"],
            "taw --machine studio machine: taw machine runs here: taw machine check studio",
        ),
    ];
    for (verb, words) in table {
        let mut args = vec!["--machine", "studio"];
        args.extend(verb.iter().copied());
        let (code, stdout, stderr) = rig.taw(&args);
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(stdout, "", "{args:?}");
        assert_eq!(stderr, format!("taw: {words}\n"), "{args:?}");
    }
    assert!(
        rig.stand_in.calls().is_empty(),
        "{:?}",
        rig.stand_in.calls()
    );

    // The positive half: a verb that is not refused is recorded, so the empty log above could have
    // had something in it.
    let (code, _) = rig.envelope(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 0);
    assert_eq!(rig.stand_in.calls().len(), 1);
}

#[test]
fn open_with_json_and_the_other_agent_verbs_are_forwarded() {
    let rig = Rig::new();
    rig.stand_in.fail_as("studio", Failure::Refused);
    for args in [
        vec!["--machine", "studio", "open", "--json", "w0"],
        vec!["--machine", "studio", "agent", "list"],
        vec!["--machine", "studio", "agent", "get", "%3"],
    ] {
        let (code, _, _) = rig.taw(&args);
        assert_eq!(code, 1, "{args:?}");
    }
    assert_eq!(rig.stand_in.calls().len(), 3);
}

#[test]
fn an_unknown_name_and_a_raw_destination_are_refused_with_the_names_there_are() {
    let rig = Rig::new();
    for name in ["nope", "me@somewhere", "studio.local", "Studio"] {
        let (code, stdout, stderr) = rig.taw(&["--machine", name, "workspace", "list"]);
        assert_eq!(code, 2, "{name}");
        assert_eq!(stdout, "", "{name}");
        assert_eq!(
            stderr,
            format!("taw: no machine called {name} in [machines] (yours: studio, pi)\n"),
            "{name}"
        );
    }
    assert!(rig.stand_in.calls().is_empty());
}

#[test]
fn with_no_machines_listed_the_refusal_says_where_it_looked() {
    let rig = Rig::new();
    rig.here.write_config("model = \"x\"\n");
    let (code, _, stderr) = rig.taw(&["--machine", "studio", "workspace", "list"]);
    let file = rig.here.config_dir().join("config.toml");
    assert_eq!(code, 2);
    assert_eq!(
        stderr,
        format!(
            "taw: no machine called studio: [machines] in {} lists none\n",
            file.display()
        )
    );
    std::fs::remove_file(&file).unwrap();
    let (code, _, again) = rig.taw(&["--machine", "studio", "workspace", "list"]);
    assert_eq!((code, again), (2, stderr));
    assert!(rig.stand_in.calls().is_empty());
}

#[test]
fn a_machines_table_taw_cannot_read_whole_fails_as_an_invalid_argument() {
    let rig = Rig::new();
    rig.here
        .write_config("[machines]\nStudio = \"studio\"\nstudio = \"studio\"\n");
    let (code, envelope) = rig.envelope(&["--machine", "studio", "workspace", "list"]);
    assert_eq!(code, 1);
    assert_eq!(envelope["id"], "cli:machine");
    assert_eq!(envelope["error"]["code"], "invalid_argument");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("has a line in [machines] taw drops"),
        "{message}"
    );
    assert!(
        message.contains("machines.Studio: a machine's name is"),
        "{message}"
    );
    assert!(rig.stand_in.calls().is_empty());
}

#[test]
fn a_command_line_clap_should_answer_is_not_sent_anywhere() {
    let rig = Rig::new();
    for args in [
        vec!["--machine"],
        vec!["--machine", "studio"],
        vec!["--machine", "studio", "--help"],
        vec![
            "--machine",
            "studio",
            "--machine",
            "pi",
            "workspace",
            "list",
        ],
        vec!["workspace", "list", "--machine", "studio"],
    ] {
        let (code, stdout, stderr) = rig.taw(&args);
        // Help is the one that succeeds, and says where --machine goes.
        if args.contains(&"--help") {
            assert_eq!(code, 0, "{args:?}");
            assert!(stdout.contains("--machine <NAME>"), "{stdout}");
        } else {
            assert_eq!(code, 2, "{args:?}: {stderr}");
        }
    }
    let (code, _, stderr) = rig.taw(&["--machine=", "workspace", "list"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("--machine needs the name of a machine"),
        "{stderr}"
    );
    assert!(rig.stand_in.calls().is_empty());
}

#[test]
fn the_recorded_argv_is_what_the_runner_builds_option_by_option() {
    let rig = Rig::new();
    let (code, _) = rig.envelope(&[
        "--pretty",
        "--machine",
        "studio",
        "workspace",
        "rename",
        "w0",
        "a b",
    ]);
    assert_eq!(code, 0);
    let calls = rig.stand_in.calls();
    assert_eq!(calls.len(), 1);

    let paths = Paths::rooted_at(
        rig.here.config_dir().to_path_buf(),
        rig.here.state_dir().to_path_buf(),
    );
    let machine = Machine {
        name: "studio".to_string(),
        ssh: "studio".to_string(),
        taw: None,
    };
    let ssh = Ssh {
        bin: rig.stand_in.bin.clone(),
    };
    let built = command(
        &ssh,
        &paths,
        &machine,
        Mode::Command,
        &["--pretty", "workspace", "rename", "w0", "a b"],
    );
    let wanted: Vec<String> = built
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        calls[0].len(),
        wanted.len(),
        "{:?} against {wanted:?}",
        calls[0]
    );
    for (i, (got, want)) in calls[0].iter().zip(&wanted).enumerate() {
        assert_eq!(got, want, "argument {i}");
    }
    // Its options are the ones a single quiet command needs.
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ConnectionAttempts=1",
    ] {
        assert!(
            calls[0].iter().any(|a| a == option),
            "{option} in {:?}",
            calls[0]
        );
    }
    let _ = &rig.studio;
}

/// Runs the real binary on `args` with nothing around it: (status, stdout, stderr). It runs no ssh:
/// the lines here end before one is built, and its ssh would be a path that is not there.
fn bare_taw(args: &[std::ffi::OsString]) -> (i32, String, String) {
    let scratch = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_taw"))
        .args(args)
        .env_clear()
        .env("HOME", scratch.path())
        .env("TAW_SSH_BIN", scratch.path().join("no-ssh"))
        .env("HISTFILE", "/dev/null")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_line_for_a_machine_with_a_word_that_is_not_utf8_is_refused_in_its_own_words() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(vec![0xff]);
    let os = |s: &str| OsString::from(s);
    for args in [
        vec![
            os("--machine"),
            os("studio"),
            os("worktree"),
            os("create"),
            bad.clone(),
        ],
        vec![os("--machine=studio"), os("workspace"), bad.clone()],
        vec![os("--pretty"), os("--machine"), os(""), bad.clone()],
        vec![os("--machine"), bad.clone()],
    ] {
        let (code, stdout, stderr) = bare_taw(&args);
        assert_eq!(code, 2, "{args:?}: {stderr}");
        assert_eq!(stdout, "", "{args:?}");
        assert_eq!(
            stderr, "taw: --machine runs words of UTF-8 only\n",
            "{args:?}"
        );
    }
    // The positive half: with only UTF-8 words the empty name keeps its own answer, and a word that is
    // not UTF-8 on a line with no machine in it is clap's to answer, as before.
    let (code, _, stderr) = bare_taw(&[os("--machine"), os(""), os("workspace"), os("list")]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("taw: --machine needs the name of a machine"),
        "{stderr}"
    );
    let (code, _, stderr) = bare_taw(&[os("workspace"), bad]);
    assert_eq!(code, 2);
    assert!(!stderr.contains("--machine"), "{stderr}");
}

#[test]
fn a_control_folder_that_cannot_be_made_is_said_on_stderr_and_the_command_still_runs() {
    let rig = Rig::new();
    // A short root of its own, so that the state folder's own socket path stays within the 86 bytes
    // that choose it, whatever the runner's TMPDIR is.
    let scratch = tempfile::Builder::new()
        .prefix("t")
        .tempdir_in("/tmp")
        .unwrap();
    // A state folder under a regular file: the folder for the control sockets cannot be made there.
    let file = scratch.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let state = file.join("state");
    let bin = rig.stand_in.bin.display().to_string();
    let (code, stdout, stderr) = rig.here.taw_raw_with_env(
        &["--machine", "studio", "workspace", "list"],
        &[
            ("TAW_SSH_BIN", &bin),
            ("TAW_STATE_DIR", state.to_str().unwrap()),
        ],
    );
    // The positive half: the command ran on the far side.
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"garden\""), "{stdout}");
    assert_eq!(rig.stand_in.calls().len(), 1);
    assert!(
        stderr.contains("could not make") && stderr.contains("private"),
        "no warning on stderr: {stderr:?}"
    );
    assert!(
        stderr.contains(state.join("m").to_str().unwrap()),
        "{stderr}"
    );
}
