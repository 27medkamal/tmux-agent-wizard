//! The native resume command for every integration taw can install.
//!
//! `tests/tmux_session.rs` covers the mechanism — that `session restore` types the command
//! into the pane it rebuilt, and that the restored pane already carries the id
//! it was resumed from. This file covers the table: one command per agent kind, matching
//! herdr's own "Native session restore" table (executable, flag, and `--flag value` versus
//! `--flag=value` spelling), because a wrong command is typed into the user's shell — plus,
//! at the bottom, one real-tmux test for Codex's *other* explicit-resume path: `agent
//! start --kind codex -- resume <id>`, which never goes through `session restore` at all.

use taw::integration::AGENTS;
use taw::session::restore::resume_command;

mod common;

use common::{Server, ServerOptions, named_agent};

/// The kind a pane carries is the `--agent` label its integration reported, which is the
/// install target's name for every agent but Antigravity CLI: its hook reports `agy`,
/// which is also its detection manifest, its executable and its `@agent_kind`.
fn kind_for(agent: &str) -> &str {
    match agent {
        "antigravity-cli" => "agy",
        other => other,
    }
}

#[test]
fn every_installable_integration_has_a_resume_command() {
    for agent in AGENTS {
        let kind = kind_for(agent);
        assert!(
            resume_command(kind, "abc").is_some(),
            "{agent} can be installed but a restored {kind} pane would never be resumed"
        );
    }
}

#[test]
fn each_command_matches_herdrs_native_session_restore_table() {
    for (kind, expected) in [
        ("claude", "claude --resume abc"),
        ("codex", "codex resume abc"),
        ("copilot", "copilot --resume=abc"),
        ("cursor", "cursor-agent --resume abc"),
        ("devin", "devin --resume abc"),
        ("droid", "droid --resume abc"),
        ("grok", "grok --resume abc"),
        ("hermes", "hermes --resume abc"),
        ("qodercli", "qodercli --resume abc"),
        ("qwen", "qwen --resume abc"),
        ("omp", "omp --resume=abc"),
        ("kilo", "kilo --session abc"),
        ("kimi", "kimi --session abc"),
        ("opencode", "opencode --session abc"),
        ("pi", "pi --session abc"),
        ("mastracode", "mastracode --thread abc"),
        ("agy", "agy --conversation abc"),
        // Letta's other form — its default conversation, encoded "default:<agent_id>" —
        // is covered by `session::restore::tests::resume_command_letta_encodes_…` instead,
        // since it needs a differently-shaped id than every other row here shares.
        ("letta", "letta --conversation abc"),
    ] {
        assert_eq!(
            resume_command(kind, "abc").as_deref(),
            Some(expected),
            "wrong resume command for {kind}"
        );
    }
}

/// Cursor's executable is `cursor-agent`, not `cursor`: herdr's docs are explicit that it
/// does not launch the generic `agent` command, and `cursor` is the editor.
#[test]
fn cursor_resumes_through_the_cursor_agent_executable() {
    let command = resume_command("cursor", "abc").unwrap();
    assert!(command.starts_with("cursor-agent "), "{command}");
}

/// A kind taw has no integration for restores as a plain shell rather than guessing.
#[test]
fn an_unknown_kind_has_no_resume_command() {
    for kind in [
        "gemini",
        "amp",
        "cline",
        "kiro",
        "maki",
        "antigravity-cli",
        "",
    ] {
        assert_eq!(
            resume_command(kind, "abc"),
            None,
            "{kind:?} must not be resumed"
        );
    }
}

/// `taw agent start --kind codex -- resume <id>` already carries that id, the same as a
/// session-restore resume does — ported from herdr's `agent_resume.rs`
/// `persisted_session_from_launch_args`, wired into `cli::agent::start`. Unlike the
/// generic `session restore` mechanism `tmux_session.rs` exercises, this never goes
/// through `session::restore` at all: it is `agent start`'s own launch args.
#[test]
fn agent_start_with_an_explicit_codex_resume_records_the_session_id_immediately() {
    // A Codex that reads to tmux as `codex` for as long as it runs, which is what `agent start`
    // takes for the agent being there.
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(named_agent(dir.path(), "codex", None)),
        ..ServerOptions::default()
    });
    let pane = server.pane();

    server.ok(
        &[
            "agent",
            "start",
            "rev",
            "--kind",
            "codex",
            "--pane",
            &pane,
            "--",
            "resume",
            "resume-id-1",
        ],
        "cli:agent:start",
        "agent",
    );

    let got = server.ok(&["agent", "get", "rev"], "cli:agent:get", "agent");
    assert_eq!(got["agent"]["agent_session_id"], "resume-id-1", "{got}");
}

/// The id is shell-quoted on the way out. This string is delivered with `send-keys … Enter`
/// and run by the pane's own shell, so an id carrying a metacharacter would otherwise
/// execute whatever it names. `valid_session_id` rejects those at every write; this is the
/// independent second guard, and the one that covers a snapshot written before that check.
#[test]
fn a_session_id_carrying_shell_metacharacters_is_quoted_for_every_kind() {
    let nasty = "a b;$(touch /tmp/taw-pwned)`id`";
    for agent in AGENTS {
        let command = resume_command(kind_for(agent), nasty)
            .unwrap_or_else(|| panic!("no resume command for {agent}"));
        let quoted = format!("'{nasty}'");
        assert!(
            command.ends_with(&quoted),
            "{agent}: the id was not single-quoted: {command}"
        );
        // Inside single quotes every one of those characters is inert; what matters is
        // that none of them reached the part of the line the shell does interpret.
        let outside = &command[..command.len() - quoted.len()];
        assert!(
            !outside.contains([';', '$', '`', '\'', '"', '\n']),
            "{agent}: a metacharacter reached the command line unquoted: {outside:?}"
        );
    }
}
