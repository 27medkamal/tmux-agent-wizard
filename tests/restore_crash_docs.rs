//! What the docs say about the copy a new tmux server's first save keeps and the look-back of a
//! plain `taw session restore`, held in place against the text itself: the README's restore section
//! and "Beyond herdr", docs/COMPAT.md's `session_restored` row and docs/PARITY.md. The words are the
//! ones the verb prints (`src/session/restore.rs`), which `tests/e2e_restore_after_crash.rs` runs.

use std::path::PathBuf;

fn read(path: &str) -> String {
    let full = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("reading {}: {e}", full.display()))
}

/// `text` with every run of whitespace a single space, so a passage wrapped over lines reads as one.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn readme() -> String {
    flat(
        &(read("docs/restore.md")
            + &read("docs/sessions.md")
            + &read("docs/cli.md")
            + &read("docs/beyond-herdr.md")),
    )
}

#[test]
fn the_readme_says_how_a_plain_restore_finds_the_old_layout_and_how_far_back_it_goes() {
    let readme = readme();
    for needle in [
        "**After a tmux server dies.**",
        "strictly before tmux's `#{start_time}`",
        "one layout and no more",
        "never goes further back",
        "Nothing is restored when a server starts; it is a command you run.",
        "restored the layout saved at 16:23, before this tmux server started at 16:41",
        "`--dry-run` says `would restore ...` and makes nothing",
        "`--name <name>` is exactly that file, and `--name default` exactly `default.json`",
    ] {
        assert!(readme.contains(needle), "the README lacks {needle:?}");
    }
}

#[test]
fn the_readme_says_when_a_copy_is_made_without_the_wait_and_how_to_restore_an_older_one() {
    let readme = readme();
    for needle in [
        "copies it first whatever the age of the newest copy",
        "a layout the newest copy already has is still not copied twice",
        "**Older copies.**",
        "copy a file from `session-snapshots/` to `sessions/<a name>.json` and run \
         `taw session restore --name <a name>`",
    ] {
        let needle = flat(needle);
        assert!(readme.contains(&needle), "the README lacks {needle:?}");
    }
}

#[test]
fn beyond_herdr_has_the_copy_and_the_look_back_and_says_herdr_never_restores_an_older_snapshot() {
    let section = flat(&read("docs/beyond-herdr.md"));
    let section = &section;
    assert!(section.contains(
        "**A new tmux server keeps the old layout, and a plain `taw session restore` finds it**"
    ));
    assert!(section.contains("herdr never restores an older snapshot by itself"));
    assert!(section.contains("one layout, never further"));
}

#[test]
fn compat_names_the_three_new_fields_of_a_restore_and_the_first_plan_line() {
    let compat = read("docs/COMPAT.md");
    let row = compat
        .lines()
        .find(|line| line.starts_with("| `taw session restore` | `session_restored` |"))
        .expect("the session restore row");
    let row = flat(row);
    for needle in [
        "`snapshot_file`",
        "`snapshot_taken_unix`",
        "`server_start_unix`",
        "`null` when it could not be read",
        "never by the copy's file name or time",
        "`nothing to restore: the layout saved at 16:23, before this tmux server started at 16:41, \
         has every session open`",
        "`restored the layout saved at 16:23, before this tmux server started at 16:41`",
        "`--name` is exactly that file",
    ] {
        let needle = flat(needle);
        assert!(
            row.contains(&needle),
            "COMPAT's restore row lacks {needle:?}"
        );
    }
}

#[test]
fn parity_says_taw_adds_the_new_server_case_and_recovers_older_copies_by_hand() {
    let parity = flat(&read("docs/PARITY.md"));
    assert!(parity.contains("Recovery of an older copy is by hand"));
    assert!(parity.contains("a new tmux server"));
    assert!(parity.contains("one layout and never further"));
}

#[test]
fn the_readme_says_a_claude_pane_comes_back_under_its_profile_and_the_dry_run_lists_the_lines() {
    let readme = readme();
    for needle in [
        "**A Claude pane comes back under the profile it ran under.**",
        "`<profile>/projects/<project>/<id>.jsonl`",
        "`CLAUDE_CONFIG_DIR=/home/you/.claude-alt claude --resume <id>`",
        "A pane that ran on the default profile gets the bare `claude --resume <id>`",
        "The project folder plays no part",
        "Codex's hook reports no home",
        "`taw session restore --dry-run` lists the exact line it would type in each pane",
        "the result's `would_type`",
    ] {
        assert!(readme.contains(needle), "the README lacks {needle:?}");
    }
}

#[test]
fn compat_and_parity_describe_the_profile_and_the_dry_runs_lines() {
    let compat = read("docs/COMPAT.md");
    let row = compat
        .lines()
        .find(|line| line.starts_with("| `taw session restore` | `session_restored` |"))
        .expect("the session restore row");
    let row = flat(row);
    for needle in [
        "`CLAUDE_CONFIG_DIR=<profile> claude --resume <id>`",
        "the default `$HOME/.claude`, a missing or relative path and another shape get the bare",
        "Codex's hook reports no home",
        "`would_type`",
        "`would not resume: <reason>`",
    ] {
        assert!(
            row.contains(needle),
            "COMPAT's restore row lacks {needle:?}"
        );
    }
    let parity = flat(&read("docs/PARITY.md"));
    assert!(parity.contains("is typed `CLAUDE_CONFIG_DIR=<profile> claude --resume <id>`"));
    assert!(parity.contains("the dry run lists each line it would type"));
}

#[test]
fn the_docs_say_a_claude_pane_comes_back_with_the_flags_it_was_started_with_and_keep_nothing_else()
{
    let readme = readme();
    for needle in [
        "**A Claude pane comes back with the flags it was started with.**",
        "`--effort`, `--model`, `--permission-mode` and `--agent`, each with its value, and \
         `--dangerously-skip-permissions`",
        "written back as `--flag value`, in the order you gave them",
        "`CLAUDE_CONFIG_DIR=/home/you/.claude-alt claude --effort max --model opus --resume <id>`",
        "never a prompt, never print mode, never a flag that chooses a session",
        "arguments can carry credentials, so everything outside the list is dropped unread and \
         nothing is logged",
        "nothing is kept, so a line is never guessed at",
        "`--resume <id>` in its own arguments, which is never kept",
        "`agent_claude_flags` (older snapshots have none) and on the pane as `@agent_claude_flags`",
        "A Claude pane outside a sandbox comes back at the effort the session's last turn ran at, \
         including one set inside the session with `/effort`",
        "the last 2 MiB of the transcript its Claude reported, the two effort fields of the turn \
         records, nothing else kept, nothing logged",
        "With no file or no field, the line is as before.",
    ] {
        let needle = flat(needle);
        assert!(readme.contains(&needle), "the README lacks {needle:?}");
    }

    let compat = read("docs/COMPAT.md");
    let row = compat
        .lines()
        .find(|line| line.starts_with("| `taw session restore` | `session_restored` |"))
        .expect("the session restore row");
    let row = flat(row);
    for needle in [
        "`CLAUDE_CONFIG_DIR=<profile> claude --effort max --model x --resume <id>`",
        "`@agent_claude_flags`",
        "an older snapshot has none",
        "A prompt, print mode, the flags that choose a session and every other flag are never kept",
        "the effort of the last turn record that gives one in the last 2 MiB of the file at the saved transcript path, \
         its `perTurnEffort` or its `effort` when that is null",
        "and the saved one when it does not",
    ] {
        assert!(
            row.contains(needle),
            "COMPAT's restore row lacks {needle:?}"
        );
    }
    let parity = flat(&read("docs/PARITY.md"));
    assert!(parity.contains("with the flags it was started with that set how the session runs"));
    assert!(
        parity
            .contains("with `--effort` at the level its last turn ran at when its transcript says")
    );

    let job = flat(&read("src/watch/job.rs").replace("//!", ""));
    assert!(job.contains("nothing here logs or keeps them"));
    assert!(job.contains("The one exception is a Claude's flags that set how its session runs"));

    let architecture = flat(&read("docs/ARCHITECTURE.md"));
    assert!(architecture.contains("| `@agent_claude_flags` |"));
    assert!(architecture.contains(
        "with `--effort` overridden by the effort of the last turn record that gives one in the last 2 MiB"
    ));
}

#[test]
fn the_readme_says_the_look_back_lasts_while_the_server_runs_and_an_unreadable_default_is_an_error()
{
    let readme = readme();
    for needle in [
        "a plain restore can also bring back a session from the previous server's last layout \
         that you closed since",
        "`--name default` restores this server's own saved layout only, with no look-back",
        "A `default.json` that taw cannot read is an error and not a reason to look back",
        "the restore says what it could not read and uses no copy, and a copy can be restored by \
         hand",
    ] {
        let needle = flat(needle);
        assert!(readme.contains(&needle), "the README lacks {needle:?}");
    }
}

#[test]
fn compats_restore_row_ends_each_sentence_with_a_full_stop() {
    let compat = read("docs/COMPAT.md");
    let row = compat
        .lines()
        .find(|line| line.starts_with("| `taw session restore` | `session_restored` |"))
        .expect("the session restore row");
    let row = flat(row);
    for needle in [
        "and made again). With no `--name`",
        "the start's against today). A Claude pane is typed the profile",
        "in a restore that makes anything. A Claude pane is typed the flags",
    ] {
        assert!(
            row.contains(needle),
            "COMPAT's restore row lacks {needle:?}"
        );
    }
}
