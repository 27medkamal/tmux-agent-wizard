//! The reference files under `third_party/herdr/` that are not a plain copy of herdr's
//! source, and the script that cuts one of them.
//!
//! Most of that directory is herdr's own files, copied byte for byte, and `cmp`
//! against herdr's source keeps those honest. Two files are a captured install and
//! generated output that herdr is never run to regenerate, so each release is followed
//! by a hand edit. A third, the default config, is a cut of one string in herdr's
//! `src/main.rs` and is made by `extract-default-config.sh`, never edited. These tests
//! pin the edits and the script, so a later refresh cannot undo one unnoticed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("third_party/herdr")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// The text of one `--- herdr <verb> ---` block of `cli-help.txt`, up to the next
/// `---` or `===` header.
fn help_section<'a>(help: &'a str, verb: &str) -> &'a str {
    let header = format!("--- herdr {verb} ---\n");
    let start = help
        .find(&header)
        .unwrap_or_else(|| panic!("cli-help.txt has no `{verb}` block"))
        + header.len();
    let rest = &help[start..];
    let end = ["\n--- ", "\n=== "]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// herdr 0.9.1 moved the `accent` key it documents for the whole UI out from under
/// `[ui.sidebar.spaces]`, where a TOML reader took it for a sidebar key (#2697). The file is
/// herdr's own text now, so this holds herdr to it, and holds the file to being herdr's.
#[test]
fn default_config_documents_ui_accent_in_the_ui_table() {
    let raw = read("reference/default-config.toml");
    // The file writes every table header and every default as a comment. A key belongs
    // to the last header above it.
    let mut table = String::new();
    let mut accent_tables = Vec::new();
    for line in raw.lines() {
        let body = line.strip_prefix("# ").unwrap_or(line).trim();
        if let Some(name) = body
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .filter(|name| !name.contains(char::is_whitespace))
        {
            table = name.to_string();
        } else if body.starts_with("accent = ") {
            accent_tables.push(table.clone());
        }
    }
    assert_eq!(accent_tables, ["theme.custom", "ui"]);
}

/// What herdr's default config gained from 0.9.0 to 0.9.3, as lines of the file: the
/// appearance-specific theme tables, Kitty graphics, the three-way `pane_borders`, the
/// `machine` row token, Letta in the sound list, `clear_pane` and the restore spacing. Each
/// is a line the file this crate kept by hand did not have, so a file older than 0.9.3 fails.
#[test]
fn default_config_carries_what_herdr_added_through_0_9_3() {
    let raw = read("reference/default-config.toml");
    let lines: Vec<&str> = raw.lines().collect();
    for wanted in [
        "# [theme.custom.light]",
        "# [theme.custom.dark]",
        "# kitty_graphics = true",
        "# pane_borders = \"auto\"",
        "# rows = [[\"state_icon\", \"machine\", \"workspace\", \"tab\"], [\"agent\"]]",
        "# qwen-code, letta, letta-code, maki.",
        "# clear_pane = \"\"                  # unbound; e.g. \"prefix+ctrl+k\"",
        "# startup_per_agent_delay_ms = 100",
    ] {
        assert!(
            lines.contains(&wanted),
            "default-config.toml has no line `{wanted}`"
        );
    }
    // The first and last lines of herdr's string: nothing was added around the cut.
    assert_eq!(lines.first(), Some(&"# herdr configuration"));
    assert_eq!(
        lines.last(),
        Some(&"# scrollback_limit_bytes = 10000000"),
        "the file ends where herdr's string does"
    );
    assert!(raw.ends_with('\n'));
}

/// herdr 0.9.1 scopes Claude's SessionStart hook to the sources that start a session,
/// so Grok, which imports Claude's hooks, no longer runs it (herdr `claude_settings.rs`,
/// `SESSION_START_MATCHER`).
#[test]
fn claude_settings_capture_scopes_session_start_to_its_sources() {
    let settings: serde_json::Value =
        serde_json::from_str(&read("integrations/claude/settings.json")).unwrap();
    let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]["matcher"],
        "^(startup|resume|clear|compact|fork)$"
    );
}

/// The verbs and values herdr 0.9.1's CLI reference adds, without the SSH machine
/// verbs and `--machine`, which taw does not have.
#[test]
fn cli_help_lists_the_verbs_and_values_added_in_0_9_1() {
    let help = read("reference/cli-help.txt");

    assert!(
        help_section(&help, "workspace close")
            .contains("Usage: herdr workspace close <workspace_id> [--group]"),
    );
    for verb in [
        "worktree list",
        "worktree create",
        "worktree open",
        "worktree remove",
    ] {
        assert!(
            help_section(&help, verb).contains("\n      --trust-repository\n"),
            "{verb} takes --trust-repository"
        );
    }
    for verb in ["integration install", "integration uninstall"] {
        let section = help_section(&help, verb);
        assert!(
            section.contains(", grok, letta]"),
            "{verb} lists letta, the one experimental target, last"
        );
    }
    assert!(help_section(&help, "agent start").contains(", qwen, letta, maki, muse]"));

    assert!(!help.contains("--machine"));
    assert!(!help.contains("herdr machine"));
}

/// herdr 0.9.2 lets an agent report the command that resumes its session: both report
/// verbs take it after `--` (herdr `src/cli/spec.rs`, `resume_argv_arg`), a positional
/// that clap prints at the end of the usage line and under Arguments. The hand-kept help
/// carries exactly what clap prints for it.
#[test]
fn cli_help_report_verbs_take_the_resume_command_after_a_double_dash() {
    let help = read("reference/cli-help.txt");
    let resume = "  [RESUME_ARG]...\n          Command that resumes this session after a Herdr restart; starts with a plain command name\n";

    let report = help_section(&help, "pane report-agent");
    assert!(report.contains(
        "Usage: herdr pane report-agent [OPTIONS] --source <ID> --agent <LABEL> --state <STATUS> <PANE_ID> [-- [RESUME_ARG]...]\n"
    ));
    assert!(report.contains(&format!(
        "Arguments:\n  <PANE_ID>\n          \n\n{resume}\nOptions:\n"
    )));

    let session = help_section(&help, "pane report-agent-session");
    assert!(session.contains(
        "Usage: herdr pane report-agent-session [OPTIONS] --source <ID> --agent <LABEL> <PANE_ID> [-- [RESUME_ARG]...]\n"
    ));
    assert!(session.contains(&format!(
        "Arguments:\n  <PANE_ID>\n          \n\n{resume}\nOptions:\n"
    )));

    // Only those two: the other verbs of the group have no resume command to take.
    assert_eq!(
        help.matches("RESUME_ARG").count(),
        4,
        "two usage lines, two entries"
    );
    for verb in ["pane release-agent", "pane report-metadata", "agent start"] {
        assert!(!help_section(&help, verb).contains("RESUME_ARG"), "{verb}");
    }
}

// ---------------------------------------------------------------------------------------
// `extract-default-config.sh` cuts the default config out of herdr's `src/main.rs`.
// ---------------------------------------------------------------------------------------

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/herdr/extract-default-config.sh")
}

/// Runs the script on `args`, with the standard input shut so nothing waits on a terminal.
fn run_script(script: &Path, args: &[&Path]) -> Output {
    Command::new("sh")
        .arg(script)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("sh runs the script")
}

/// A herdr `src/main.rs` in miniature: other raw strings before and after, and
/// `DEFAULT_CONFIG` as `body`'s text laid where herdr lays it.
fn herdr_main(body: &str) -> String {
    format!(
        "const BEFORE: &str = r##\"not this\"##;\n\nconst DEFAULT_CONFIG: &str = r##\"{body}\"##;\n\n\
         const SKILL: &str = r##\"nor this\n\"##;\n"
    )
}

#[test]
fn the_script_cuts_the_default_config_byte_for_byte() {
    // What herdr's string holds that a careless cut would change: a hash right after a quote,
    // escapes and a percent sign that printf and echo would act on, a tab, trailing
    // spaces, a blank line of spaces, and text that is not ASCII.
    let body = "# herdr configuration\n\
        # panel_bg = \"#eff1f5\"\n\
        # escapes = \"a\\nb\\t\" and 100% of %s and %d\n\
        \tkey = 1   \n\
        \x20\x20\n\
        # box = \"\u{2500}\u{2500} \u{203a}\"\n\
        \n\
        # last = 1\n";
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    let output = dir.path().join("default-config.toml");
    std::fs::write(&source, herdr_main(body)).unwrap();

    let ran = run_script(&script_path(), &[&source, &output]);

    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(std::fs::read(&output).unwrap(), body.as_bytes());
    // Nothing else was left beside it: the staged file is gone.
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["default-config.toml", "main.rs"]);
}

#[test]
fn the_script_takes_the_string_as_rust_does_wherever_it_starts_and_ends() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    let output = dir.path().join("out.toml");
    for (name, text, expected) in [
        // The closing quotes on the line of the last words: no newline is added.
        (
            "ends mid-line",
            "const DEFAULT_CONFIG: &str = r##\"abc\ndef\"##;\n",
            "abc\ndef",
        ),
        (
            "one line",
            "const DEFAULT_CONFIG: &str = r##\"one\"##;\n",
            "one",
        ),
        // The string starts with the newline that follows its opening.
        (
            "opens at the end of its line",
            "const DEFAULT_CONFIG: &str = r##\"\nfoo\n\"##;\n",
            "\nfoo\n",
        ),
        // The opening on the next line, as rustfmt would lay a long constant.
        (
            "opens on the next line",
            "const DEFAULT_CONFIG: &str =\n    r##\"x\ny\n\"##;\n",
            "x\ny\n",
        ),
        // A constant whose name only starts with DEFAULT_CONFIG is some other constant, wherever
        // it stands.
        (
            "a longer name first",
            "const DEFAULT_CONFIG_LEGACY: &str = r##\"not this\"##;\n\
             const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "a longer name first and public",
            "pub const DEFAULT_CONFIGURATION: &str = r##\"not this\"##;\n\
             pub const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "a longer name after",
            "const DEFAULT_CONFIG: &str = r##\"this\"##;\n\
             const DEFAULT_CONFIG_LEGACY: &str = r##\"not this\"##;\n",
            "this",
        ),
        // The type may be spaced as rustfmt would never write it.
        (
            "a space before the colon",
            "const DEFAULT_CONFIG : &str = r##\"this\"##;\n",
            "this",
        ),
        // Only a declaration is the constant: a comment or a string that says the same words is
        // not, whichever raw string comes next.
        (
            "a commented-out declaration first",
            "// const DEFAULT_CONFIG: &str = r##\"not this\"##;\n\
             const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "a doc comment that quotes it first",
            "/// Once `const DEFAULT_CONFIG: &str = r##\"not this\"##;`.\n\
             const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "a comment that names it above another raw string",
            "// const DEFAULT_CONFIG: moved below\n\
             const OTHER: &str = r##\"not this\"##;\n\
             const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "a string that names it first",
            "const HELP: &str = \"see const DEFAULT_CONFIG: below\";\n\
             const OTHER: &str = r##\"not this\"##;\n\
             const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        // A declaration may be indented, in a module or an impl, and public in any of Rust's
        // forms.
        (
            "indented with spaces",
            "mod m {\n    const DEFAULT_CONFIG: &str = r##\"this\"##;\n}\n",
            "this",
        ),
        (
            "indented with a tab",
            "mod m {\n\tconst DEFAULT_CONFIG: &str = r##\"this\"##;\n}\n",
            "this",
        ),
        (
            "public in the crate",
            "pub(crate) const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        (
            "public in a named module",
            "pub(in crate::cli) const DEFAULT_CONFIG: &str = r##\"this\"##;\n",
            "this",
        ),
        // A lone quote and hash inside are text: only `\"##` ends a raw string.
        (
            "hash inside",
            "const DEFAULT_CONFIG: &str = r##\"c = \\\"#fff\\\" \"# not the end\n\"##;\n",
            "c = \\\"#fff\\\" \"# not the end\n",
        ),
    ] {
        std::fs::write(&source, text).unwrap();
        let ran = run_script(&script_path(), &[&source, &output]);
        assert!(
            ran.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(
            std::fs::read(&output).unwrap(),
            expected.as_bytes(),
            "{name}"
        );
    }
}

#[test]
fn the_script_writes_nothing_when_the_string_is_missing_or_never_closed() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("main.rs");
    let output = dir.path().join("out.toml");
    for (name, text) in [
        ("no constant", "const OTHER: &str = r##\"x\"##;\n"),
        (
            "only a longer name",
            "const DEFAULT_CONFIG_LEGACY: &str = r##\"x\"##;\n",
        ),
        (
            "only a commented-out declaration",
            "// const DEFAULT_CONFIG: &str = r##\"x\"##;\n",
        ),
        (
            "only a string that names it",
            "const HELP: &str = \"see const DEFAULT_CONFIG: below\";\n\
             const OTHER: &str = r##\"x\"##;\n",
        ),
        ("no raw string", "const DEFAULT_CONFIG: &str = \"plain\";\n"),
        (
            "never closed",
            "const DEFAULT_CONFIG: &str = r##\"# herdr\n# more\n",
        ),
        ("empty file", ""),
    ] {
        std::fs::write(&source, text).unwrap();
        std::fs::write(&output, "keep me\n").unwrap();

        let ran = run_script(&script_path(), &[&source, &output]);

        assert_eq!(ran.status.code(), Some(1), "{name}");
        assert!(
            String::from_utf8_lossy(&ran.stderr).contains("no complete DEFAULT_CONFIG"),
            "{name}: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "keep me\n",
            "{name}"
        );
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left.len(), 2, "{name}: a staged file was left in {left:?}");
    }

    // A source that is not there, and no arguments at all.
    let missing = run_script(&script_path(), &[&dir.path().join("absent.rs"), &output]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(run_script(&script_path(), &[]).status.code(), Some(2));
}

#[test]
fn the_script_writes_beside_itself_when_no_output_is_named() {
    // The refresh runs it with the source alone: the file lands in `reference/` next to the
    // script, which a copy of the script in a scratch tree shows without touching the real one.
    let dir = tempfile::tempdir().unwrap();
    let herdr = dir.path().join("third_party/herdr");
    std::fs::create_dir_all(herdr.join("reference")).unwrap();
    let script = herdr.join("extract-default-config.sh");
    std::fs::copy(script_path(), &script).unwrap();
    let source = dir.path().join("main.rs");
    std::fs::write(&source, herdr_main("# herdr configuration\n")).unwrap();

    let ran = run_script(&script, &[&source]);

    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(herdr.join("reference/default-config.toml")).unwrap(),
        "# herdr configuration\n"
    );
}

// ---------------------------------------------------------------------------------------
// The OpenCode plugins are ports, and a port is herdr's file with named changes.
//
// What the guard below compares is the plugin's logic: the constants, the queues, the
// selection and the lifecycle, and the plugin object with both entry points. It does not
// compare the transport, the code between those parts that starts at herdr's `requestOnce`,
// which taw replaces with its own (a `taw pane report-*` run in place of a socket request). A
// change of herdr's inside `requestOnce`, a new parameter or a new method say, passes every
// test here, so on each refresh of the vendored files `requestOnce` is compared with herdr's
// by hand and its change carried over to taw's transport.
// ---------------------------------------------------------------------------------------

fn read_taw_template(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/integration/templates")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// `text` from the first `marker` to the end.
fn from(text: &str, marker: &str) -> String {
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("no `{marker}` in the file"));
    text[start..].to_string()
}

/// `text` from the first `start` up to, not including, the first `end` after it.
fn between(text: &str, start: &str, end: &str) -> String {
    let from = from(text, start);
    let stop = from
        .find(end)
        .unwrap_or_else(|| panic!("no `{end}` after `{start}`"));
    from[..stop].to_string()
}

const HERDR_GATE: &str = "if (process.env.HERDR_ENV !== \"1\" || !process.env.HERDR_SOCKET_PATH || !process.env.HERDR_PANE_ID) return;";
const TAW_GATE: &str = "if (!process.env.TMUX_PANE && !process.env.TAW_REPORT_FILE) return;";

/// taw's TUI plugin is herdr's `herdr-tui-session.js` with a different header and transport,
/// the plugin object named for taw, and its environment gate a `$TMUX_PANE` check, or a report file
/// inside a sandbox: everything
/// else, which is the whole of its logic, is the same bytes. So when herdr's file is refreshed,
/// this test says what the port has to catch up with, except for the transport: `requestOnce`
/// lies outside what it compares, and is compared by hand on every refresh.
#[test]
fn the_tui_plugin_is_herdrs_but_for_its_transport_its_gate_and_its_name() {
    let herdr = read("integrations/opencode/herdr-tui-session.js");
    let taw = read_taw_template("taw-tui-session.js");

    // What sits between the constants and the transport.
    assert_eq!(
        between(
            &herdr,
            "const ROUTE_POLL_INTERVAL_MS",
            "function requestOnce"
        )
        .trim_end(),
        between(&taw, "const ROUTE_POLL_INTERVAL_MS", "// How long one run").trim_end(),
    );
    // Everything after the transport: the plugin object and both entry points.
    let expected = from(&herdr, "export default {")
        .replace(HERDR_GATE, TAW_GATE)
        .replace(
            "herdr.opencode.session-selection",
            "taw.opencode.session-selection",
        );
    assert_eq!(expected, from(&taw, "export default {"));
    assert_eq!(expected.matches(TAW_GATE).count(), 2);
}

/// The version 2 entry is herdr's `tui.js` naming taw's plugin file.
#[test]
fn the_version_2_entry_is_herdrs_naming_taws_plugin_file() {
    let herdr = read("integrations/opencode/tui.js");
    let taw = read_taw_template("taw-opencode-tui.js");
    let export = |text: &str| from(text, "export {");
    assert_eq!(
        export(&herdr).replace("herdr-tui-session.js", "taw-tui-session.js"),
        export(&taw)
    );
}

/// taw's server plugin is herdr's `herdr-agent-state.js` with a different header and transport,
/// the factory and plugin named for taw, a `$TMUX_PANE` check for the environment gate, and
/// `permission.updated` handled wherever `permission.asked` is. The transport, `requestOnce`,
/// lies outside what it compares, and is compared by hand on every refresh.
#[test]
fn the_server_plugin_is_herdrs_but_for_its_transport_its_gate_its_names_and_one_event() {
    let herdr = read("integrations/opencode/herdr-agent-state.js");
    let taw = read_taw_template("taw-agent-state-opencode.js");

    // The report state, the child-session table, and the queue in front of the transport.
    let expected = between(
        &herdr,
        "let reportSeq",
        "function requestOnce(method, params)",
    )
    .replace(
        "  [\"permission.asked\", \"blocked\"],\n",
        "  [\"permission.asked\", \"blocked\"],\n  [\"permission.updated\", \"blocked\"],\n",
    );
    assert_eq!(
        expected,
        between(
            &taw,
            "let reportSeq",
            "function requestOnce(method, params)"
        )
    );

    // Everything after the transport: the session and state reports, which sessions own the
    // lifecycle, the hooks and both entry points.
    let expected = from(&herdr, "function reportSession(sessionID)")
        .replace(
            "export const HerdrAgentStatePlugin = async () => {\n  if (\n    !ownsLocalLifecycle() ||\n    process.env.HERDR_ENV !== \"1\" ||\n    !process.env.HERDR_SOCKET_PATH ||\n    !process.env.HERDR_PANE_ID\n  ) {",
            "export const TawAgentStatePlugin = async () => {\n  if (!ownsLocalLifecycle() || !process.env.TMUX_PANE) {",
        )
        .replace(
            "        case \"permission.asked\":\n        case \"question.asked\":",
            "        case \"permission.asked\":\n        case \"permission.updated\":\n        case \"question.asked\":",
        )
        .replace("id: \"herdr.opencode\",\n  server: HerdrAgentStatePlugin,", "id: \"taw.opencode\",\n  server: TawAgentStatePlugin,");
    assert_eq!(expected, from(&taw, "function reportSession(sessionID)"));
}
