//! What the docs say about `taw pane clear` and `@agent-wizard-clear-key`, held in place: that the
//! key is a tmux option that is empty (and binds nothing) by default, what the verb does in each
//! case, and the one shell that takes its key for text. The passages are read from the docs pages (cli, config, troubleshooting),
//! docs/COMPAT.md, docs/PARITY.md and SKILL.md themselves, and the option's name from the plugin
//! script.

use std::path::PathBuf;

fn read(path: &str) -> String {
    let full = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {}: {e}", full.display()))
}

/// The text from `start` up to (not including) `end`, with every run of whitespace a single
/// space, so a passage wrapped over lines reads as one.
fn between(text: &str, start: &str, end: &str) -> String {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("{start:?} is not in the text"));
    let rest = &text[from..];
    let to = rest.find(end).unwrap_or(rest.len());
    rest[..to].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The README's section on clearing a pane.
fn section() -> String {
    between(&read("docs/cli.md"), "## Clearing a pane", "\n## ")
}

#[test]
fn every_tmux_option_the_plugin_script_reads_is_in_the_readmes_table() {
    let script = read("tmux-agent-wizard.tmux");
    let readme = read("docs/config.md");
    let table = between(&readme, "## tmux options", "## `config.toml`");
    let mut names = 0;
    for line in script
        .lines()
        .filter(|line| line.starts_with("tmux_option_"))
    {
        let name = line
            .split('"')
            .nth(1)
            .unwrap_or_else(|| panic!("no option name in {line:?}"));
        names += 1;
        assert!(
            table.contains(&format!("| `{name}` |")),
            "the README's tmux options table has no row for {name}"
        );
    }
    assert!(names >= 9, "only {names} options found in the script");
}

#[test]
fn the_clear_key_is_empty_by_default_in_the_script_the_readme_and_the_example() {
    let script = read("tmux-agent-wizard.tmux");
    assert!(
        script.contains("default_clear_key=\"\""),
        "the script's default for the clear key is not empty"
    );
    let table = between(
        &read("docs/config.md"),
        "## tmux options",
        "## `config.toml`",
    );
    assert!(
        table.contains("| `@agent-wizard-clear-key` | *(empty)* |"),
        "the README does not say the clear key is empty by default"
    );
    assert!(table.contains("Empty, the default, binds nothing"));
    let example = read("examples/tmux.conf");
    assert!(
        example.contains("# set -g @agent-wizard-clear-key"),
        "the example config does not offer the clear key, commented out"
    );
}

#[test]
fn the_readme_says_what_each_case_does_and_what_it_cannot_tell() {
    let text = section();
    for needed in [
        "On the alternate screen",
        "it changes nothing",
        "it clears the pane's history",
        "bash, zsh or fish",
        "Ctrl-L",
        "nothing else in front of it",
        "a plain `sh`",
        "readline's vi mode",
        "literal `^L`",
        "one Backspace",
        "`screen_cleared` is `false`",
        "set keymap vi-insert",
        "\"\\C-l\": clear-screen",
        "`read`",
        "In a window with `synchronize-panes` on",
        "A screen that is clear already",
        "is not asked for anything",
        "did not answer",
        "wrapped past the right edge",
        "real fish's Ctrl-L has not been tried",
        "set -g @agent-wizard-clear-key C-k",
        "a space-separated list binds each",
        "nothing is bound",
        "`$TMUX_PANE`",
    ] {
        assert!(
            text.contains(needed),
            "README, \"Clearing a pane\", never says {needed:?}"
        );
    }
}

#[test]
fn the_troubleshooting_entry_says_why_the_screen_may_stay() {
    let readme = read("docs/troubleshooting.md");
    let entry = between(
        &readme,
        "**\"The clear key cleared the history and left the screen.\"**",
        "**\"The popup covers my status line,\"**",
    );
    for needed in [
        "bash, zsh or fish is at its prompt",
        "an agent, an editor, a build or a plain `sh`",
        "`synchronize-panes` on in the window",
        "on the alternate screen it does nothing at all",
        "`screen_cleared`",
        "readline's vi mode",
        "\"Clearing a pane\"",
    ] {
        assert!(
            entry.contains(needed),
            "the troubleshooting entry never says {needed:?}"
        );
    }
}

#[test]
fn the_readme_says_that_a_key_an_option_dropped_is_unbound_when_the_plugin_is_sourced_again() {
    let readme = read("docs/config.md");
    let text = between(
        &readme,
        "Change `@agent-wizard`",
        "Width and height are caps",
    );
    for needed in [
        "`@agent-wizard-notification-key`",
        "`@agent-wizard-clear-key`",
        // Only the clear key can be emptied: get_tmux_option gives an empty option its default.
        "or empty `@agent-wizard-clear-key`; the other two go back to `A` and `O` when empty",
        "source the plugin again",
        "a key the option no longer names is unbound",
        "`taw: could not open the popup`",
        "`taw: next up failed`",
        "`taw: could not clear the pane`",
        "a binding of your own on the same key is left alone",
    ] {
        assert!(
            text.contains(needed),
            "README, the tmux options, never says {needed:?}"
        );
    }
    // The three messages are the ones the plugin script shows when each binding fails: they are what
    // tells a binding of the plugin's from one of the user's.
    let script = read("tmux-agent-wizard.tmux");
    for message in [
        "taw: could not open the popup",
        "taw: next up failed",
        "taw: could not clear the pane",
    ] {
        assert!(
            script.contains(message),
            "the plugin script never shows {message:?}"
        );
        assert!(
            script.contains(&format!("unbind_stale_keys \"{message}\"")),
            "the plugin script does not unbind stale keys by {message:?}"
        );
    }
}

#[test]
fn the_other_docs_say_the_same_things_in_their_own_places() {
    let compat = read("docs/COMPAT.md");
    let row = between(&compat, "| `taw pane clear [<pane>]` |", "\n|");
    for needed in [
        "history_cleared",
        "screen_cleared",
        "alternate screen",
        "bash, zsh or fish",
        "Ctrl-L",
        "readline's vi mode",
        "`screen_cleared` is `false` with a note",
        "took Ctrl-L as text",
        "`synchronize-panes` on in the pane's window",
        "is not asked for anything",
        "wherever the `^L` landed",
        "`not_found`",
        "`@agent-wizard-clear-key`",
    ] {
        assert!(
            row.contains(needed),
            "COMPAT's pane clear row never says {needed:?}"
        );
    }

    let parity = read("docs/PARITY.md");
    let ported = between(
        &parity,
        "| A pane's screen and scrollback can be cleared (v0.9.2, #4383) |",
        "\n|",
    );
    assert!(ported.contains("ported, in part"));
    assert!(ported.contains("`synchronize-panes` on"));
    assert!(ported.contains("is not asked for anything"));
    assert!(ported.contains("fish is matched by name"));
    assert!(ported.contains("`keys.clear_pane`"));
    assert!(ported.contains("\"Structural gaps\""));
    let gap = between(
        &parity,
        "| Clearing a pane's screen (`pane.clear`) |",
        "\n|",
    );
    assert!(gap.contains("`clear-history` drops the scrollback only"));
    assert!(gap.contains("`send-keys -R`"));

    let skill = read("SKILL.md");
    let paragraph = between(&skill, "`pane clear <pane-id>` empties", "\n\n");
    assert!(paragraph.contains("clear only a pane you created or were asked to clear"));
    assert!(skill.contains("Do not close, rename, clear, or send input"));
}
