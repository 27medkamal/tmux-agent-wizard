// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//
// These tests follow herdr's `stale_hook_report_sequence_is_ignored_for_same_source`,
// `stale_hook_report_cannot_overwrite_session_ref`,
// `unsequenced_hook_report_is_ignored_after_source_uses_sequence`,
// `stale_clear_all_sequence_is_checked_against_current_authority_source`,
// `same_sequence_from_different_sources_is_independent` and
// `custom_authority_reanchors_sequence_after_process_restart` (src/terminal/state.rs, v0.9.3).
// herdr's tests drive its terminal's state machine; these run the built binary's `pane
// report-agent`, `report-agent-session` and `release-agent` against a real tmux pane and read the
// pane back. The agent whose numbers start again is one the watcher saw leave its pane, where
// herdr's is a process it saw exit; and two reports race through a `tmux` that holds the first
// between its read and its write.

//! The order an agent's reports are taken in, end to end: the built binary against a throwaway tmux
//! server. A report whose `--seq` is not higher than the last one its source sent for the pane is
//! ignored, and so is one with no `--seq` from a source that has sent one: herdr's rule, which its guide
//! for agents makes the contract. Each source has its own numbers, a release is a report like the others,
//! and two reports that race are taken in their numbers' order, whichever of them lands first.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Server, ServerOptions, wait_for, write_script};
use serde_json::Value;

/// Ceiling for what a real tmux and the built binary take to do a thing, with `cargo test` running other
/// files beside this one.
const SETTLE: Duration = Duration::from_secs(20);

/// The words of a `taw pane report-agent` of `state` by `source` for `agent` on `pane`, numbered `seq`
/// when it is given, with `more` after them.
fn report_args<'a>(
    pane: &'a str,
    source: &'a str,
    agent: &'a str,
    state: &'a str,
    seq: Option<&'a str>,
    more: &[&'a str],
) -> Vec<&'a str> {
    let mut args = vec![
        "pane",
        "report-agent",
        pane,
        "--source",
        source,
        "--agent",
        agent,
        "--state",
        state,
    ];
    if let Some(seq) = seq {
        args.extend(["--seq", seq]);
    }
    args.extend(more);
    args
}

/// `taw pane report-agent`, and its result.
fn report(
    server: &Server,
    pane: &str,
    source: &str,
    agent: &str,
    state: &str,
    seq: Option<&str>,
    more: &[&str],
) -> Value {
    server.ok(
        &report_args(pane, source, agent, state, seq, more),
        "cli:pane:report-agent",
        "ok",
    )
}

/// `taw pane report-agent-session`, and its result.
fn report_session(
    server: &Server,
    pane: &str,
    source: &str,
    agent: &str,
    seq: Option<&str>,
    more: &[&str],
) -> Value {
    let mut args = vec![
        "pane",
        "report-agent-session",
        pane,
        "--source",
        source,
        "--agent",
        agent,
    ];
    if let Some(seq) = seq {
        args.extend(["--seq", seq]);
    }
    args.extend(more);
    server.ok(&args, "cli:pane:report-agent-session", "ok")
}

/// `taw pane release-agent`, and its result.
fn release(server: &Server, pane: &str, source: &str, agent: &str, seq: Option<&str>) -> Value {
    let mut args = vec![
        "pane",
        "release-agent",
        pane,
        "--source",
        source,
        "--agent",
        agent,
    ];
    if let Some(seq) = seq {
        args.extend(["--seq", seq]);
    }
    server.ok(&args, "cli:pane:release-agent", "ok")
}

#[test]
fn a_report_numbered_no_higher_than_its_sources_last_changes_nothing() {
    let server = Server::start();
    let pane = server.pane();

    let first = report(
        &server,
        &pane,
        "custom:pi",
        "pi",
        "working",
        Some("20"),
        &["--agent-session-path", "/sessions/root.jsonl"],
    );
    assert_eq!(first["in_order"], true, "{first}");

    // Lower, and the same number again: neither is newer, and neither the state nor the session
    // they carry is written.
    for seq in ["19", "20"] {
        let stale = report(
            &server,
            &pane,
            "custom:pi",
            "pi",
            "idle",
            Some(seq),
            &["--agent-session-path", "/sessions/new.jsonl"],
        );
        assert_eq!(stale["in_order"], false, "{seq}: {stale}");
        assert_eq!(server.pane_opt(&pane, "@agent_state"), "working", "{seq}");
        assert_eq!(
            server.pane_opt(&pane, "@agent_session_path"),
            "/sessions/root.jsonl",
            "{seq}"
        );
    }
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "custom:pi=20");

    // A higher one is taken.
    let newer = report(
        &server,
        &pane,
        "custom:pi",
        "pi",
        "idle",
        Some("21"),
        &["--agent-session-path", "/sessions/new.jsonl"],
    );
    assert_eq!(newer["in_order"], true, "{newer}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
    assert_eq!(
        server.pane_opt(&pane, "@agent_session_path"),
        "/sessions/new.jsonl"
    );
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "custom:pi=21");
}

/// The numbers the hooks taw installs send are nanoseconds since the epoch, 19 digits, and two reports a
/// nanosecond apart are two numbers: the order is decided on the whole number, never in tmux's format
/// arithmetic, which is floating point and takes the two for one.
#[test]
fn reports_a_nanosecond_apart_are_taken_in_their_order() {
    let server = Server::start();
    let pane = server.pane();
    let source = "integration:claude";

    report(
        &server,
        &pane,
        source,
        "claude",
        "working",
        Some("1790800000123456789"),
        &[],
    );
    let next = report(
        &server,
        &pane,
        source,
        "claude",
        "idle",
        Some("1790800000123456790"),
        &[],
    );
    assert_eq!(next["in_order"], true, "{next}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");

    let late = report(
        &server,
        &pane,
        source,
        "claude",
        "working",
        Some("1790800000123456789"),
        &[],
    );
    assert_eq!(late["in_order"], false, "{late}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
    assert_eq!(
        server.pane_opt(&pane, "@agent_report_seq"),
        "integration:claude=1790800000123456790"
    );
}

#[test]
fn a_report_with_no_number_is_taken_only_from_a_source_that_has_sent_none() {
    let server = Server::start();
    let pane = server.pane();

    // A source that never numbers its reports has each taken.
    for state in ["working", "idle", "working"] {
        let taken = report(&server, &pane, "mine", "my-agent", state, None, &[]);
        assert_eq!(taken["in_order"], true, "{state}: {taken}");
        assert_eq!(server.pane_opt(&pane, "@agent_state"), state);
    }
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "");

    // Once a source has numbered one, a report of its with no number is out of order.
    report(
        &server,
        &pane,
        "numbered",
        "my-agent",
        "blocked",
        Some("5"),
        &[],
    );
    let unnumbered = report(&server, &pane, "numbered", "my-agent", "idle", None, &[]);
    assert_eq!(unnumbered["in_order"], false, "{unnumbered}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "blocked");

    // The source that never numbered one still has its reports taken.
    let other = report(&server, &pane, "mine", "my-agent", "idle", None, &[]);
    assert_eq!(other["in_order"], true, "{other}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
}

#[test]
fn the_same_number_from_another_source_is_that_sources_own() {
    let server = Server::start();
    let pane = server.pane();

    report(
        &server,
        &pane,
        "integration:pi",
        "pi",
        "working",
        Some("20"),
        &[],
    );
    let other = report(&server, &pane, "custom:pi", "pi", "idle", Some("19"), &[]);

    assert_eq!(other["in_order"], true, "{other}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:custom:pi");
    assert_eq!(
        server.pane_opt(&pane, "@agent_report_seq"),
        "integration:pi=20 custom:pi=19"
    );
}

#[test]
fn a_release_numbered_no_higher_than_its_sources_last_is_not_made() {
    let server = Server::start();
    let pane = server.pane();
    report(
        &server,
        &pane,
        "custom:pi",
        "pi",
        "working",
        Some("20"),
        &[],
    );

    let stale = release(&server, &pane, "custom:pi", "pi", Some("19"));
    assert_eq!(stale["released"], false, "{stale}");
    assert_eq!(stale["in_order"], false, "{stale}");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:custom:pi");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "working");

    let unnumbered = release(&server, &pane, "custom:pi", "pi", None);
    assert_eq!(unnumbered["released"], false, "{unnumbered}");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:custom:pi");

    let newer = release(&server, &pane, "custom:pi", "pi", Some("21"));
    assert_eq!(newer["released"], true, "{newer}");
    assert_eq!(newer["in_order"], true, "{newer}");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "custom:pi=21");

    // A report its release outnumbers is older than the release, and is not taken back.
    let late = report(
        &server,
        &pane,
        "custom:pi",
        "pi",
        "blocked",
        Some("21"),
        &[],
    );
    assert_eq!(late["in_order"], false, "{late}");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "");
}

/// A session report is numbered from the same count as its source's state reports: one out of order
/// changes no session, and a state report its session report outnumbers is out of order too.
#[test]
fn a_session_report_numbered_no_higher_than_its_sources_last_changes_nothing() {
    let server = Server::start();
    let pane = server.pane();
    let source = "integration:claude";

    let first = report_session(
        &server,
        &pane,
        source,
        "claude",
        Some("20"),
        &["--agent-session-id", "sess-a"],
    );
    assert_eq!(first["in_order"], true, "{first}");
    let stale = report_session(
        &server,
        &pane,
        source,
        "claude",
        Some("19"),
        &["--agent-session-id", "sess-b"],
    );
    assert_eq!(stale["in_order"], false, "{stale}");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "sess-a");

    let state = report(
        &server,
        &pane,
        source,
        "claude",
        "working",
        Some("19"),
        &["--agent-session-id", "sess-b"],
    );
    assert_eq!(state["in_order"], false, "{state}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "sess-a");
}

/// OpenCode's TUI says which session the pane shows with no number, after its numbered reports of the
/// same source, and herdr takes it whatever was sent before (`is_unsequenced_opencode_selection`). Only
/// that choice: an unnumbered session report of the same source that is not one is out of order, and so
/// is one of another source.
#[test]
fn opencodes_tui_choosing_a_session_is_taken_with_no_number_after_numbered_reports() {
    let server = Server::start();
    let pane = server.pane();
    let source = "integration:opencode";
    report(
        &server,
        &pane,
        source,
        "opencode",
        "working",
        Some("100"),
        &["--agent-session-id", "ses_a"],
    );

    let chosen = report_session(
        &server,
        &pane,
        source,
        "opencode",
        None,
        &[
            "--agent-session-id",
            "ses_b",
            "--session-start-source",
            "select",
        ],
    );
    assert_eq!(chosen["in_order"], true, "{chosen}");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "ses_b");
    assert_eq!(
        server.pane_opt(&pane, "@agent_report_seq"),
        "integration:opencode=100",
        "the choice recorded a number"
    );

    let resumed = report_session(
        &server,
        &pane,
        source,
        "opencode",
        None,
        &[
            "--agent-session-id",
            "ses_c",
            "--session-start-source",
            "resume",
        ],
    );
    assert_eq!(resumed["in_order"], false, "{resumed}");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "ses_b");

    report(
        &server,
        &pane,
        "integration:mine",
        "opencode",
        "working",
        Some("7"),
        &[],
    );
    let other = report_session(
        &server,
        &pane,
        "integration:mine",
        "opencode",
        None,
        &[
            "--agent-session-id",
            "ses_d",
            "--session-start-source",
            "select",
        ],
    );
    assert_eq!(other["in_order"], false, "{other}");
    assert_eq!(server.pane_opt(&pane, "@agent_session_id"), "ses_b");
}

/// What a source sent goes with the agent: once the watcher has seen the agent leave its pane (its shell
/// is back) and taken its record off, the next agent's reporter starts its numbers again, as herdr starts
/// them again for an agent whose process exited and came back.
#[test]
fn a_sources_numbers_start_again_once_its_agent_has_left_the_pane() {
    let server = Server::start_with(ServerOptions {
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config("[update]\nmanifest_check = false\n\n[ui.toast]\ndelivery = \"off\"\n");
    let pane = server.pane();
    server.tmux_ok(&["send-keys", "-t", &pane, "sleep 300", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "sleep",
            SETTLE
        ),
        "`sleep` never took the pane"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");
    report(&server, &pane, "kimi", "kimi", "working", Some("100"), &[]);
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "kimi=100");

    // The agent quits, and the shell that comes back ends its record.
    server.tmux_ok(&["send-keys", "-t", &pane, "C-c"]);
    assert!(
        wait_for(|| server.pane_opt(&pane, "@agent_kind").is_empty(), SETTLE),
        "the record of the agent that left was never taken off\n{}",
        server.watcher_logs()
    );
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "");

    // Its reporter starts again, with a number lower than its last.
    server.tmux_ok(&["send-keys", "-t", &pane, "sleep 300", "Enter"]);
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") == "sleep",
            SETTLE
        ),
        "`sleep` never took the pane again"
    );
    let again = report(&server, &pane, "kimi", "kimi", "working", Some("1"), &[]);
    assert_eq!(again["in_order"], true, "{again}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "working");
}

/// A `tmux` that holds the first report to read the order of reports of a pane, right after it answers
/// that read, until `go` is in `dir`, and says it is holding by making `held` in `dir`; any other call, and
/// any later read, goes straight through. The report it holds writes over what it read, after the report
/// the test makes meanwhile has landed.
fn tmux_that_holds_a_report_after_its_read(dir: &Path) -> PathBuf {
    let real = taw::tmux::Tmux::with_socket_name("unused").bin;
    let wrapper = dir.join("tmux-holding-a-report");
    write_script(
        &wrapper,
        &format!(
            "out=$('{real}' \"$@\"); status=$?\n\
             printf '%s\\n' \"$out\"\n\
             case \"$*\" in\n  *' display-message -p -t '*'#{{@agent_report_seq}}')\n    \
             if mkdir '{dir}/held' 2>/dev/null; then\n      i=0\n      \
             while [ ! -e '{dir}/go' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n    \
             fi ;;\nesac\nexit $status\n",
            dir = dir.display(),
            real = real.display(),
        ),
    );
    wrapper
}

/// Races a report of `working` numbered `held` by `source` on `pane` against one of `idle` numbered 20:
/// the first reads the order of reports, is held, the second lands whole, and the first then writes.
/// Returns the first's result.
fn race(server: &Server, pane: &str, dir: &Path, held: &str) -> Value {
    let wrapper = tmux_that_holds_a_report_after_its_read(dir);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            server.taw_with_env(
                &report_args(pane, "custom:pi", "pi", "working", Some(held), &[]),
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        assert!(
            wait_for(|| dir.join("held").exists(), SETTLE),
            "the first report never read the order of reports"
        );
        let second = report(server, pane, "custom:pi", "pi", "idle", Some("20"), &[]);
        assert_eq!(second["in_order"], true, "{second}");
        std::fs::write(dir.join("go"), "").unwrap();
        let (code, envelope) = first.join().unwrap();
        assert_eq!(code, 0, "{envelope}");
        envelope["result"].clone()
    })
}

/// Two reports of one source race: the lower one reads the order before the higher one lands and writes
/// after it. The check and the write are one tmux call that writes only while the pane holds what the
/// report read, so the lower one finds that another report has landed, decides again, and is out of
/// order: the higher one's state stands.
#[test]
fn a_lower_report_that_races_a_higher_one_is_out_of_order_whichever_lands_first() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let pane = server.pane();

    let first = race(&server, &pane, tmp.path(), "10");

    assert_eq!(first["in_order"], false, "{first}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "idle");
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "custom:pi=20");
}

/// The same race, with the held report the higher one: it finds that another has landed, decides again
/// over what that one left, and is still the newer: it is taken.
#[test]
fn a_higher_report_that_a_lower_one_lands_before_is_still_taken() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let pane = server.pane();

    let first = race(&server, &pane, tmp.path(), "30");

    assert_eq!(first["in_order"], true, "{first}");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "working");
    assert_eq!(server.pane_opt(&pane, "@agent_report_seq"), "custom:pi=30");
}

/// A release reads whether its source holds the pane and the order of reports, and then clears the
/// source: another source that takes the pane in between, with a report that carries no number and so
/// leaves the order as it was, is not released by it. Whether the source still holds the pane is asked
/// in the call that clears it, with the order.
#[test]
fn a_release_is_not_made_for_a_pane_another_source_took_meanwhile() {
    let tmp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let pane = server.pane();
    report(
        &server,
        &pane,
        "custom:pi",
        "pi",
        "working",
        Some("20"),
        &[],
    );
    let wrapper = tmux_that_holds_a_report_after_its_read(tmp.path());

    let released = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            server.taw_with_env(
                &[
                    "pane",
                    "release-agent",
                    &pane,
                    "--source",
                    "custom:pi",
                    "--agent",
                    "pi",
                    "--seq",
                    "21",
                ],
                &[("TAW_TMUX_BIN", wrapper.to_str().unwrap())],
            )
        });
        assert!(
            wait_for(|| tmp.path().join("held").exists(), SETTLE),
            "the release never read the order of reports"
        );
        report(&server, &pane, "mine", "pi", "blocked", None, &[]);
        std::fs::write(tmp.path().join("go"), "").unwrap();
        let (code, envelope) = first.join().unwrap();
        assert_eq!(code, 0, "{envelope}");
        envelope["result"].clone()
    });

    assert_eq!(released["released"], false, "{released}");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "hook:mine");
    assert_eq!(server.pane_opt(&pane, "@agent_state"), "blocked");
}

/// A `--source` is a name, kept in the pane's options and among the last sources' numbers, which a
/// report writes in one tmux command: one of more than 128 bytes is refused with `invalid_argument` by
/// each verb that takes one, and nothing of its report is applied; one of 128 bytes is taken.
#[test]
fn a_source_longer_than_128_bytes_is_refused_and_nothing_of_its_report_is_applied() {
    let server = Server::start();
    let pane = server.pane();
    let long = "s".repeat(129);
    for args in [
        report_args(&pane, &long, "my-agent", "working", Some("1"), &[]),
        vec![
            "pane",
            "report-agent-session",
            &pane,
            "--source",
            &long,
            "--agent",
            "my-agent",
            "--agent-session-id",
            "s1",
        ],
        vec![
            "pane",
            "release-agent",
            &pane,
            "--source",
            &long,
            "--agent",
            "my-agent",
        ],
    ] {
        let (code, envelope) = server.taw(&args);
        assert_ne!(code, 0, "{envelope}");
        assert_eq!(envelope["error"]["code"], "invalid_argument", "{envelope}");
    }
    for option in [
        "@agent_kind",
        "@agent_state",
        "@agent_session_id",
        "@agent_report_seq",
    ] {
        assert_eq!(server.pane_opt(&pane, option), "", "{option}");
    }

    let fits = "s".repeat(128);
    let taken = report(&server, &pane, &fits, "my-agent", "working", Some("1"), &[]);
    assert_eq!(taken["in_order"], true, "{taken}");
}

/// The last sources' numbers are kept in at most 2 KiB, the oldest going first, so a report's tmux
/// command, which holds them twice, stays far below the 16 KiB tmux takes in one: reports from more
/// sources with long names than fit leave the newest kept and the oldest gone, and a report from
/// another source still lands.
#[test]
fn the_numbers_of_sources_with_long_names_are_kept_in_2_kib_the_oldest_going_first() {
    let server = Server::start();
    let pane = server.pane();
    // 128 bytes each, every one of them written as three (`%C3%A9`).
    let sources: Vec<String> = (0..12)
        .map(|n| format!("{n:02}{}", "\u{e9}".repeat(63)))
        .collect();
    for (n, source) in sources.iter().enumerate() {
        let seq = (n + 1).to_string();
        let taken = report(
            &server,
            &pane,
            source,
            "my-agent",
            "working",
            Some(&seq),
            &[],
        );
        assert_eq!(taken["in_order"], true, "{n}: {taken}");
    }

    let kept = server.pane_opt(&pane, "@agent_report_seq");
    assert!(kept.len() <= 2048, "{} bytes kept", kept.len());
    // The newest source's number is kept: one no higher than it is out of order.
    let again = report(
        &server,
        &pane,
        &sources[11],
        "my-agent",
        "idle",
        Some("12"),
        &[],
    );
    assert_eq!(again["in_order"], false, "{again}");
    // The oldest's went: its own first number is taken again.
    let oldest = report(
        &server,
        &pane,
        &sources[0],
        "my-agent",
        "idle",
        Some("1"),
        &[],
    );
    assert_eq!(oldest["in_order"], true, "{oldest}");
    let other = report(&server, &pane, "other", "my-agent", "idle", Some("1"), &[]);
    assert_eq!(other["in_order"], true, "{other}");
}

/// The numbers a pane keeps go twice into the one tmux call that writes a report, beside the command
/// that report carries, and tmux takes about 16 KiB in one call: kept in at most 2 KiB, they leave room
/// for the longest command the checks let through, 8 KiB of plain words, however many sources with long
/// names have reported before. Such a command is kept, and not refused as too long for tmux.
#[test]
fn the_longest_command_fits_beside_the_most_numbers_a_pane_keeps() {
    let server = Server::start();
    let pane = server.pane();
    // 128 bytes each, every one but two written as three (`%25`): more than are kept.
    for n in 0..14 {
        let source = format!("{n:02}{}", "%".repeat(126));
        let taken = report(
            &server,
            &pane,
            &source,
            "my-agent",
            "working",
            Some("1"),
            &[],
        );
        assert_eq!(taken["in_order"], true, "{n}: {taken}");
    }
    let kept = server.pane_opt(&pane, "@agent_report_seq").len();
    assert!(kept <= 2048, "{kept} bytes kept");

    let words = "x".repeat(8 * 1024 - "prime-agent".len());
    let (code, envelope) = server.taw(&report_args(
        &pane,
        "prime-agent",
        "prime-agent",
        "working",
        Some("1"),
        &["--", "prime-agent", &words],
    ));

    assert_eq!(code, 0, "{kept} bytes of numbers kept: {envelope}");
    assert_eq!(
        server.pane_opt(&pane, "@agent_resume_argv"),
        format!("prime-agent {words}")
    );
}
