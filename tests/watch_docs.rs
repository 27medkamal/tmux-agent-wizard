//! What a person reads about watchers that run an old taw: the README (install, troubleshooting,
//! the CLI notes and "Beyond herdr"), SKILL.md, docs/ARCHITECTURE.md and docs/PARITY.md.
//!
//! The command the documents give is kept a real one, and the words they use for the fields of
//! `taw watch status` are the ones the verb prints, so a rename in either place fails here.
//! The sentences the hooks docs repeat about it are pinned by `tests/hooks_docs.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;

use taw::watch::binary::RESTART_COMMAND;

fn read(path: &str) -> String {
    let full: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// The text with every run of white space made one space, so a sentence wrapped across
/// lines is still one sentence.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The lines of the README's Troubleshooting entry about a rebuilt taw.
fn troubleshooting_entry() -> String {
    let readme = read("docs/troubleshooting.md");
    let start = readme
        .find("**\"I rebuilt `taw` and my open panes still behave as before.\"**")
        .expect("README's Troubleshooting has no entry for a rebuilt taw");
    let rest = &readme[start..];
    // It is the last entry of the page.
    rest.to_string()
}

#[test]
fn the_command_the_documents_give_is_a_flag_of_attach_all() {
    assert_eq!(RESTART_COMMAND, "taw watch attach-all --restart");
    let help = Command::new(env!("CARGO_BIN_EXE_taw"))
        .args(["watch", "attach-all", "--help"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--restart"), "{help}");
}

#[test]
fn the_readme_says_how_to_find_the_old_watchers_and_start_them_again() {
    let entry = flat(&troubleshooting_entry());
    for needle in [
        "`taw watch status` says which is which",
        "`binary`, `current` or `old` (`null` where no watcher is alive)",
        "`binary_note` says why, in words",
        "The result counts in `old` the watchers `--restart` would start (an old watcher whose pipe is already closed was let go of, and is not one), and `restart` names the command when there is any, or says that watching is off when it is",
        "taw watch attach-all --restart",
        "A watcher is old when the file it was started from is not the file at that path any more",
        "A watcher with no record of the file it was started from is old too",
        "opens the next from the file the old one started from, so the new watcher runs the `taw` that is there now whichever `taw` you typed the command with",
        "A watcher that wrote no record or whose file is gone has no such file, and starts again from the `taw` you ran, so run the installed one",
        "Watchers on the installed `taw` are left alone, nothing happens while `@agent-wizard-watch` is `off`",
        "So does a watcher whose recorded path is not absolute, or names a file that is not yours: it is not run",
        "`{\"attached\": 0, \"restarted\": 3, \"failed\": 0, \"note\": null}`",
        "A watcher that was let go of and whose new one could not be started is counted in `failed`, and `note` says so in words: its pane has no watcher now, and `taw watch attach-all` tries again",
    ] {
        assert!(
            entry.contains(needle),
            "README's Troubleshooting never says {needle:?}"
        );
    }

    let readme = flat(
        &(read("docs/install.md")
            + &read("docs/troubleshooting.md")
            + &read("docs/cli.md")
            + &read("docs/beyond-herdr.md")),
    );
    for needle in [
        // The install steps point at it, since that is when a rebuild happens.
        "After a rebuild or an update, the watchers that are already running keep the `taw` they started with. `taw watch status` marks them, and `taw watch attach-all --restart`, run with the new one, starts each of them again (see Troubleshooting).",
        // The CLI notes.
        "`taw watch status` lists every pane with whether it is piped and whether its watcher is alive, and marks each live watcher whose `taw` is not the one installed",
        "the count of those `--restart` would start in `old`",
        "`taw watch attach-all --restart` attaches a watcher to every pane that has none, as `attach-all` does, and starts each old watcher again as well, counting the ones it could not start in `failed`",
        // The ledger of what goes beyond herdr.
        "| **Old watchers are marked, and one command starts them again**",
    ] {
        assert!(readme.contains(needle), "README never says {needle:?}");
    }
}

#[test]
fn skill_tells_an_agent_what_the_verbs_do_and_when_to_run_them() {
    let skill = flat(&read("SKILL.md"));
    for needle in [
        "taw watch\n```",
        "`taw watch status` only reads",
        "`taw watch attach-all --restart` starts those again",
        "so run either only when the user asked",
        "`taw watch attach`, `detach` and `run` belong to the plugin's hooks: do not run them",
    ] {
        assert!(
            skill.contains(&flat(needle)),
            "SKILL.md never says {needle:?}"
        );
    }
}

#[test]
fn the_architecture_and_the_parity_ledger_name_the_record_and_the_row() {
    let architecture = flat(&read("docs/ARCHITECTURE.md"));
    for needle in [
        "| `@agent_watch_build` | which taw that watcher runs",
        "`<pid> <device>:<inode>:<size>:<seconds>:<nanoseconds> <path>`",
        "The watcher writes it before its pid.",
        "`taw watch attach-all --restart` (`watch::restart_old`) closes the pipe of each old watcher",
        "`watch/binary.rs`",
    ] {
        assert!(
            architecture.contains(needle),
            "ARCHITECTURE never says {needle:?}"
        );
    }

    let parity = flat(&read("docs/PARITY.md"));
    for needle in [
        "| After an update, the servers still on the old version are listed with the commands to restart each (v0.9.2, `herdr update`, 9dc3a1df2b) | ported, by another route |",
        "`taw watch attach-all --restart` starts each of them again, from the file it started from",
        "Nothing is ported from herdr's code, so no file carries herdr's header",
    ] {
        assert!(parity.contains(needle), "PARITY never says {needle:?}");
    }
}

/// The guide's lines on what goes online (the Install page) and on restore (the Save and restore
/// page), held to the code: the rules check runs at most once per `MANIFEST_MAX_AGE`, under the `User-Agent` the watcher sends, and the
/// switch named is the one the default config documents; a conversation comes back only once
/// the agent's hook is installed.
#[test]
fn the_landing_says_how_often_taw_goes_online_how_to_stop_it_and_when_a_conversation_comes_back() {
    let landing = flat(&format!(
        "{}\n{}",
        read("docs/install.md"),
        read("docs/restore.md")
    ));
    let max_age = taw::watch::MANIFEST_MAX_AGE.as_secs();
    assert_eq!(
        max_age % 3600,
        0,
        "the rules check's interval is whole hours"
    );
    let hours = max_age / 3600;
    for needle in [
        format!("At most once every {hours} hours, taw asks herdr.dev for newer detection rules"),
        "with `tmux-agent-wizard/<version>` as its `User-Agent`".to_string(),
        "`[update] manifest_check = false` turns that off".to_string(),
        "reopen in their own conversations once the agent's hook is installed (one command)"
            .to_string(),
        "until that version's release is out, each time tmux loads the plugin".to_string(),
        "`taw plugin install` and `taw plugin update` fetch the plugin you name (or each one you installed) from GitHub and run its build steps, which may fetch more"
            .to_string(),
        "again whenever an update changes its recipe".to_string(),
        "has mise install the agent (from npm for most of them) or runs its maker's installer"
            .to_string(),
        "installs node's latest long-term release when the agent needs it and the project names none"
            .to_string(),
        "and the tools in the project's `mise.toml`".to_string(),
        "`docker=read-only` also has your container runtime pull a small Docker proxy image from Docker Hub"
            .to_string(),
        "taw itself does nothing else online".to_string(),
    ] {
        assert!(
            landing.contains(&needle),
            "the Install and restore pages do not say: {needle}"
        );
    }
    let watcher = read("src/watch/mod.rs");
    assert!(
        watcher.contains("format!(\"tmux-agent-wizard/{}\", env!(\"CARGO_PKG_VERSION\"))"),
        "the watcher's User-Agent is not the one the Install page names"
    );
    let defaults = flat(&read("src/config_default.toml"));
    for needle in [
        "[update]".to_string(),
        format!("at most once every {hours} hours. # manifest_check = true"),
    ] {
        assert!(
            defaults.contains(&needle),
            "the default config does not say: {needle}"
        );
    }
}

/// What the Agents page and docs/PARITY.md say about when the job in front of a pane is read: the
/// watcher's own timings (`src/watch/job.rs`, `src/watch/state.rs`) are the numbers the words give.
#[test]
fn the_documents_say_when_the_job_in_front_of_a_pane_is_read() {
    let readme = flat(&read("docs/agents.md"));
    for needle in [
        "taw reads the arguments of the job in front of the pane when a program comes in front, and keeps only the agent they name",
        "While they name none it reads them again every five seconds during that program's first minute in front (a wrapper may start its agent a moment after it starts), and then no more for that program, unless they have named an agent: a program that has named one is read again every five seconds for as long as it is in front, so an agent that its wrapper restarts (`while :; do kimi; done`) is named again; while that agent runs they are not read again, and whether that program is still in front is asked every five seconds",
        "It never reads them while the pane's own shell is at its prompt, but it does when the pane's own process is a runtime (`exec codex` typed at the prompt, or a window started with `codex`)",
    ] {
        assert!(
            readme.contains(needle),
            "the Agents page does not say: {needle}"
        );
    }
    assert!(
        !readme.contains("once for each program that comes in front"),
        "the Agents page still says the job is read once for each program"
    );
    let job = read("src/watch/job.rs");
    assert!(job.contains("const NAMING_WINDOW: Duration = Duration::from_secs(60);"));
    let state = read("src/watch/state.rs");
    assert!(state.contains("IDENTITY_RECHECK: Duration = Duration::from_secs(5);"));

    let parity = flat(&read("docs/PARITY.md"));
    for needle in [
        "a Codex whose process names it reads `idle` through the known-agent fallback: the program tmux names, or, for npm's Codex, which runs as `node`, the job in front of the pane",
        "One that only its launch hint names, such as an npm Codex before the job in front has been read, reads `idle` at its empty composer",
    ] {
        assert!(
            parity.contains(needle),
            "docs/PARITY.md does not say: {needle}"
        );
    }
    assert!(
        !parity.contains("one that only its launch hint names (npm's runs as `node`)"),
        "docs/PARITY.md still says an npm Codex is named only by its launch hint"
    );
}
