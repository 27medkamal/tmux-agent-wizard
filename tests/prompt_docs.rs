//! What the docs say about the wait before taw types into a pane, held in place: what is never
//! typed into, what the check needs of `ps` and `pgrep`, and that `agent start`'s `--timeout` is
//! one budget. The passages are read from the README, docs/COMPAT.md and SKILL.md themselves.

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

/// The passages about the wait: the README's two, the two rows of COMPAT's envelope table, and
/// SKILL.md's.
fn passages() -> Vec<(&'static str, String)> {
    let restore = read("docs/restore.md");
    let cli = read("docs/cli.md");
    let compat = read("docs/COMPAT.md");
    let skill = read("SKILL.md");
    vec![
        (
            "docs/restore.md, save and restore",
            between(
                &restore,
                "A resume command is typed only once the pane's shell is at its prompt.",
                "Autosave runs on its own, debounced 5s",
            ),
        ),
        (
            "docs/cli.md, the CLI",
            between(
                &cli,
                "`taw agent start <name> --kind <kind> --pane <pane>",
                "Verbs added for the last herdr socket methods",
            ),
        ),
        (
            "docs/COMPAT.md, agent start",
            between(
                &compat,
                "| `taw agent start <name> --kind <kind> --pane <pane>`",
                "\n|",
            ),
        ),
        (
            "docs/COMPAT.md, session restore",
            between(&compat, "| `taw session restore` |", "\n|"),
        ),
        (
            "SKILL.md",
            between(
                &skill,
                "A successful `agent start` returns only after",
                "Submit work through the agent surface:",
            ),
        ),
    ]
}

#[test]
fn the_docs_name_what_is_never_typed_into_and_what_the_check_needs() {
    for (place, text) in passages() {
        // A restore's row and passage say what restore leaves alone; the others, `agent start`.
        let (never_typed, needs) = if place.contains("session restore") {
            (vec!["same never-typed-into cases as `agent start`"], vec![])
        } else {
            (
                vec!["ssh", "docker exec", "PS1", "`read` of its own", "compinit"],
                vec!["ps -o pgid=,tpgid=,comm=", "pgrep -g", "tpgid"],
            )
        };
        for needle in never_typed.into_iter().chain(needs) {
            assert!(
                text.contains(needle),
                "{place} never says {needle:?}:\n{text}"
            );
        }
    }
    // The cases that look like a prompt, named for what a person recognises them by.
    let readme = passages().remove(0).1;
    assert!(
        readme.contains("oh-my-zsh update prompt"),
        "the README never names the oh-my-zsh update prompt"
    );
}

#[test]
fn the_docs_say_the_timeout_of_agent_start_is_one_budget_the_shell_wait_counts_against() {
    for (place, text) in passages() {
        if place.contains("session restore") || place.contains("save and restore") {
            continue;
        }
        assert!(
            text.contains("one budget"),
            "{place} never says --timeout is one budget:\n{text}"
        );
        assert!(
            text.contains("counts against it"),
            "{place} never says the wait for the shell counts against it:\n{text}"
        );
    }
    let skill = read("SKILL.md");
    assert!(
        !skill.contains("Startup defaults to a 30-second timeout"),
        "SKILL.md still gives the agent a timeout of its own on top of the shell's"
    );
}

#[test]
fn the_docs_say_when_agent_start_is_done_and_what_it_says_when_the_agent_did_not_start() {
    for (place, text) in passages() {
        if place.contains("session restore") || place.contains("save and restore") {
            continue;
        }
        for needle in [
            // What the wait takes for the agent being there, one word for each of the three.
            "process",
            "reported",
            "watcher",
            "`unknown`",
            // The agent no process name gives away.
            "node",
            // A shell back at its prompt with the agent not there, and the budget running out.
            "agent_not_started",
            "`timeout`",
            "hint",
        ] {
            assert!(
                text.contains(needle),
                "{place} never says {needle:?}:\n{text}"
            );
        }
    }
}

/// What `agent prompt --wait` waits for, as SKILL.md and docs/COMPAT.md say it: herdr's rule, with
/// the gate, the two codes, the one budget and the turn that is already under way.
#[test]
fn the_docs_say_what_agent_prompt_wait_waits_for() {
    let skill = read("SKILL.md");
    let compat = read("docs/COMPAT.md");
    let passages = [
        (
            "SKILL.md",
            between(
                &skill,
                "With `--wait`, a prompt sent to an agent that is not working",
                "Use `--state`",
            ),
        ),
        (
            "docs/COMPAT.md",
            between(
                &compat,
                "| `taw agent prompt <target> <text> --wait`",
                "\n|",
            ),
        ),
    ];
    for (place, text) in passages {
        for needle in [
            "`working` or `blocked`",
            "5 seconds",
            "`agent_prompt_stalled`",
            "`timeout`",
            "may still have been delivered",
            "one budget",
            "already in",
        ] {
            assert!(
                text.contains(needle),
                "{place} never says {needle:?}:\n{text}"
            );
        }
        assert!(
            text.contains("already working") || text.contains("is working when the prompt"),
            "{place} never says what an agent that is working when the prompt is sent gets:\n{text}"
        );
    }
}

/// The list of error codes in SKILL.md is what an agent switches on, so it has every code the
/// CLI can report: a code added to the CLI and left out of it is a code nobody was told of.
#[test]
fn skill_md_lists_every_error_code_the_cli_can_report() {
    let skill = read("SKILL.md");
    let list = skill
        .lines()
        .find(|line| line.starts_with("Errors carry a stable `error.code`:"))
        .expect("SKILL.md has no list of error codes");
    for code in taw::cli::envelope::CODES {
        assert!(
            list.contains(&format!("`{code}`")),
            "SKILL.md never lists the error code {code}: {list}"
        );
    }
}

/// The envelope sweep (`scripts/cli-sweep.sh`) flags an error whose code is not in the vocabulary it
/// carries, which is `CODES`: a code added to the CLI and left out of it makes the sweep report a
/// verb that does what it should as one that does not.
#[test]
fn the_sweep_knows_every_error_code_the_cli_can_report() {
    let sweep = read("scripts/cli-sweep.sh");
    let from = sweep
        .find("codes = {")
        .expect("the sweep has no vocabulary of error codes");
    let vocabulary = between(&sweep, &sweep[from..from + "codes = {".len()], "}");
    for code in taw::cli::envelope::CODES {
        assert!(
            vocabulary.contains(&format!("\"{code}\"")),
            "the sweep never lists the error code {code}: {vocabulary}"
        );
    }
}

/// `worktree_dirty` is the code the app asks "remove anyway?" on, so it is named in the code list,
/// in the sweep's vocabulary and in the rule about `--force`, not only through the loop over `CODES`.
#[test]
fn worktree_dirty_is_named_in_the_skill_and_the_sweep() {
    assert!(taw::cli::envelope::CODES.contains(&"worktree_dirty"));
    let skill = read("SKILL.md");
    assert!(
        skill.lines().any(|l| {
            l.starts_with("Errors carry a stable `error.code`:") && l.contains("`worktree_dirty`")
        }),
        "SKILL.md's list of codes lacks worktree_dirty"
    );
    assert!(
        skill
            .lines()
            .any(|l| l.contains("`worktree_dirty`") && l.contains("--force")),
        "SKILL.md never says what to do on worktree_dirty"
    );
    assert!(read("scripts/cli-sweep.sh").contains("\"worktree_dirty\""));
}

#[test]
fn the_docs_about_the_wait_use_no_dash_arrow_or_curly_quote() {
    const BANNED: [(char, &str); 7] = [
        ('\u{2014}', "an em-dash"),
        ('\u{2013}', "an en-dash"),
        ('\u{2192}', "an arrow"),
        ('\u{2018}', "a curly single quote"),
        ('\u{2019}', "a curly single quote"),
        ('\u{201c}', "a curly double quote"),
        ('\u{201d}', "a curly double quote"),
    ];
    for (place, text) in passages() {
        for (banned, name) in BANNED {
            assert!(!text.contains(banned), "{place} holds {name}:\n{text}");
        }
    }
}

/// herdr's agents page says two things about waiting on an agent, and the CLI page says both where it
/// documents `taw agent wait` and `agent prompt --wait`: an idle agent is not permission to approve
/// anything, and a wait observes the agent's state, not its background jobs.
#[test]
fn the_readme_says_idle_is_no_permission_and_a_wait_does_not_watch_background_jobs() {
    let readme = read("docs/cli.md");
    let text = between(
        &readme,
        "`taw agent wait` and `agent prompt --wait` end with `agent_not_running`",
        "`taw agent prompt` and `taw agent send-keys` refuse",
    );
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle = "An agent reading `idle` is not permission to approve anything, and a wait watches the \
        agent's state, not its background jobs: automation that trusts a mistaken `idle` can type into \
        an approval dialog, and an agent at its prompt reads `idle` while the commands, monitors and \
        subagents it started keep running.";
    assert!(
        text.contains(needle),
        "the CLI page lacks {needle:?}: {text}"
    );
}
