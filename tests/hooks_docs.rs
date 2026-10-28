//! What a person reads about hooks: the README's "Hooks" section and its phone subsection,
//! the default config's `[hooks]` block, docs/COMPAT.md, and what SKILL.md tells an agent.
//!
//! Two things are kept true here. The documents say every event, every variable and every
//! verb the code has, so a new event cannot ship undocumented. And they follow the wording rule:
//! no em-dash, en-dash, arrow or curly quote in anything a user reads.
//! The whole of the README and of the other documents is scanned by `tests/user_text.rs`; the
//! parts written for hooks are scanned here as well, and the strings the code itself prints
//! are scanned where they are made (the help test in `src/cli/mod.rs`, the scan of the source
//! in `src/tui/app/wording.rs`, and the last test here).

#[path = "common/exec.rs"]
mod exec;

use std::path::{Path, PathBuf};

use taw::hooks::context::VARIABLES;
use taw::hooks::events::{EVENTS, VETOES};

/// What may not appear. Spelled as escapes, so this file passes its own scan.
const BANNED: [(char, &str); 11] = [
    ('\u{2014}', "an em-dash"),
    ('\u{2013}', "an en-dash"),
    ('\u{2192}', "an arrow"),
    ('\u{2018}', "a curly single quote"),
    ('\u{2019}', "a curly single quote"),
    ('\u{201c}', "a curly double quote"),
    ('\u{201d}', "a curly double quote"),
    ('\u{2015}', "a horizontal bar"),
    ('\u{2212}', "a minus sign"),
    ('\u{21d2}', "a double arrow"),
    ('\u{279c}', "an arrow"),
];

fn read(path: &str) -> String {
    let full: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// The lines from `heading` to the next heading of the same or a higher level. A `#` at the
/// start of a line inside a code fence is a shell comment, not a heading.
fn section(text: &str, heading: &str) -> String {
    let level = heading.chars().take_while(|c| *c == '#').count();
    let mut out = Vec::new();
    let mut started = false;
    let mut in_fence = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_fence = !in_fence;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        let is_heading = !in_fence && hashes > 0 && line[hashes..].starts_with(' ');
        if started && is_heading && hashes <= level {
            break;
        }
        if !started && line == heading {
            started = true;
        }
        if started {
            out.push(line);
        }
    }
    assert!(started, "no section {heading:?}");
    out.join("\n")
}

/// The lines of `text` that hold a banned character, with where they are.
fn banned_in(what: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        for (c, name) in BANNED {
            if line.contains(c) {
                found.push(format!(
                    "{what}, line {}: {name}: {}",
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    found
}

/// The words of the block of the default config that starts after `after` and ends at `before`:
/// they are comments, so what is read is what follows the `#` on each line, on one line.
fn default_config_words(after: &str, before: &str) -> String {
    let config = read("src/config_default.toml");
    let block = config
        .split(after)
        .nth(1)
        .and_then(|rest| rest.split(before).next())
        .unwrap_or_else(|| panic!("no block after {after:?} in the default config"));
    flat(
        &block
            .lines()
            .map(|line| line.trim_start().trim_start_matches('#'))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Codex holds a new hook until the user reviews it, and taw leaves that review to them: the Agents
/// page and COMPAT say so, in the words the install and the status note use.
#[test]
fn the_readme_and_compat_say_codex_asks_to_review_taws_hook() {
    let readme = flat(&read("docs/agents.md"));
    assert!(
        readme.contains(
            "Codex asks once to review a new or changed hook (Hooks need review): trust taw's hook \
             there, or it never runs and codex sessions are not kept for a restore; taw never writes \
             Codex's trust for you."
        ),
        "the Agents page does not say Codex asks to review taw's hook"
    );
    let compat = flat(&read("docs/COMPAT.md"));
    assert!(
        compat.contains(
            "An installed Codex always has a `note`, which says that Codex asks once to review the \
             new hook (Hooks need review) and that taw's hook must be trusted there, as the \
             `message` of `taw integration install codex` does"
        ),
        "COMPAT does not say Codex's note asks for the hook review"
    );
}

fn readme_hooks() -> String {
    read("docs/hooks.md")
}

fn readme_phone() -> String {
    section(
        &read("docs/notifications.md"),
        "## Notifications on your phone",
    )
}

/// The body of the first fenced code block in `text` that holds `marker`.
fn fenced_block(text: &str, marker: &str) -> String {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.starts_with("```") {
            match current.take() {
                Some(lines) => blocks.push(lines.join("\n")),
                None => current = Some(Vec::new()),
            }
        } else if let Some(lines) = &mut current {
            lines.push(line);
        }
    }
    blocks
        .into_iter()
        .find(|block| block.contains(marker))
        .unwrap_or_else(|| panic!("no code block holds {marker:?}"))
}

/// A script from the README, run under `sh` with a `curl` in front of the real one that writes
/// what it is given to files: its arguments one to a line, and its standard input. The
/// environment is `env` and nothing else, so nothing of the machine's own reaches the script.
struct Ran {
    status: std::process::ExitStatus,
    args: Option<String>,
    stdin: Option<String>,
    stderr: String,
}

fn run_with_a_recording_curl(script: &str, env: &[(&str, &str)]) -> Ran {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}/args'\ncat > '{0}/stdin'\n",
            tmp.path().display()
        ),
    )
    .unwrap();
    {
        crate::exec::make_executable(&curl);
    }
    let path = tmp.path().join("script.sh");
    std::fs::write(&path, script).unwrap();
    let out = std::process::Command::new("sh")
        .arg(&path)
        .current_dir(tmp.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .envs(env.iter().copied())
        .output()
        .unwrap();
    Ran {
        status: out.status,
        args: std::fs::read_to_string(tmp.path().join("args")).ok(),
        stdin: std::fs::read_to_string(tmp.path().join("stdin")).ok(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// `text` with every run of whitespace, line breaks included, as one space: a sentence that the
/// README wraps across lines is still one sentence to look for.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_readme_lists_every_event_and_both_vetoes_with_the_words_taw_hooks_list_prints() {
    let hooks = readme_hooks();
    for event in EVENTS.iter().chain(VETOES) {
        let row = format!("| `{}` | {} |", event.name, event.when);
        assert!(
            hooks.lines().any(|line| line == row),
            "README is missing the row:\n{row}"
        );
    }
}

#[test]
fn the_readme_names_every_variable_a_hook_is_given_and_every_verb() {
    let hooks = readme_hooks();
    for variable in VARIABLES {
        assert!(
            hooks.contains(&format!("| `{variable}` |")),
            "README does not describe {variable}"
        );
    }
    for verb in [
        "taw hooks list",
        "taw hooks test",
        "taw hooks log",
        "hook_refused",
        "timeout_seconds",
    ] {
        assert!(
            hooks.contains(verb),
            "README's Hooks section never says {verb}"
        );
    }
}

#[test]
fn the_readme_has_the_three_phone_examples_and_says_when_it_fires() {
    let phone = readme_phone();
    for needle in [
        "ntfy",
        "Pushover",
        "Any command",
        "TAW_MESSAGE",
        "TAW_KIND",
        "delivery",
        "not for a pane you are looking at",
        "--data-raw",
        "--form-string",
    ] {
        assert!(
            phone.contains(needle),
            "the phone subsection never says {needle:?}"
        );
    }
}

/// A hook has tmux's global environment whichever process fired it, and the README says where
/// to put a credential in a way that is true: `set-environment -g` reaches every hook, the
/// watcher's included, which is a `pipe-pane` child that has only the environment the server
/// was launched with.
#[test]
fn the_readme_says_that_tmuxs_global_environment_reaches_every_hook() {
    let phone = readme_phone();
    for needle in [
        "tmux's global environment plus the `TAW_*` variables",
        "`set-environment -g NTFY_TOPIC yourtopic`",
        "`tmux.conf`",
        "`taw config reload`",
        "`taw hooks test notification` runs your command with the same",
    ] {
        assert!(
            phone.contains(needle),
            "the phone subsection never says {needle:?}"
        );
    }
    let hooks = flat(&readme_hooks());
    for needle in [
        "**With what environment.**",
        "`TMUX` for the server that fired the event",
        "the `TAW_*` variables, which win over a variable of the same name",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
}

/// A variable set with `set-environment -gh` is hidden: tmux leaves it out of `show-environment`
/// and passes it to no pane, and taw gives it to a hook all the same. That is what makes it the
/// place for a token that a hook needs and a pane's shell should not have, so the README says all
/// three.
#[test]
fn the_readme_says_that_a_hidden_variable_reaches_hooks_and_no_pane() {
    assert!(
        flat(&readme_phone()).contains(
            "`set-environment -gh` works too: the value stays out of `tmux show-environment` \
             and out of every pane's shell, and it still reaches taw's hooks"
        ),
        "the phone subsection never says what a hidden variable is for"
    );
}

/// A hook is given what tmux would give a new pane of the session the event is about: the global
/// environment with that session's own over it, which is where tmux keeps the ssh agent of the
/// terminal that attached last, and what the session removes taken out. The documents say so, and
/// what an event about no session, or about one that is gone, has instead.
#[test]
fn the_documents_say_that_a_hook_has_the_environment_of_the_session_the_event_is_about() {
    let hooks = flat(&readme_hooks());
    for needle in [
        "tmux would give a new pane in the session the event is about",
        "with that session's own environment over it",
        "`SSH_AUTH_SOCK`",
        "`update-environment`",
        "removes (`set-environment -r`) is taken out of what the process that fired the event has",
        "read for each event",
        "has the global environment alone",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    assert!(
        flat(&readme_phone()).contains("goes over the global one"),
        "the phone subsection never says what a session's own environment does"
    );
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in [
        "the environment tmux would give a new pane of the session the event is about",
        "the session's own over tmux's global environment",
        "`TMUX`",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    assert!(
        flat(&read("docs/ARCHITECTURE.md"))
            .contains("the environment of the session the event is about"),
        "ARCHITECTURE never says what a hook reads of its session"
    );
}

/// `[sessions] hook_timeout_seconds = 0` is no limit, and it is the limit of the two hooks that
/// can refuse as much as of the create hooks: one that never ends holds the close, and the
/// popup or the command that asked, for as long as it runs. The documents say so where they give
/// the limit of those two, and where they list the key.
#[test]
fn the_documents_say_that_a_zero_hook_timeout_is_no_limit_for_the_hooks_that_can_refuse_too() {
    let hooks = flat(&readme_hooks());
    for needle in [
        "`0` means no limit here as it does for the create hooks",
        "a hook that never ends holds the close",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    let readme = flat(&read("docs/config.md"));
    assert!(
        readme.contains("how long `pre_close` and `pre_worktree_remove` may take to answer; `0` means no limit for all of them"),
        "the README's key table never says what the veto limit is, or that 0 is none"
    );
    let compat = flat(&read("docs/COMPAT.md"));
    for needle in [
        "and `0` there means no limit here as it does for the create hooks",
        "and how long `[hooks] pre_close` and `pre_worktree_remove` may take to answer; `0` means no limit for all of them",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    assert!(
        default_config_words("\n[hooks]\n", "\n[detection]")
            .contains("0 there means no limit for these as it does for the create hooks"),
        "the default config's [hooks] block never says what 0 is for the two that can refuse"
    );
    assert!(
        default_config_words("\n[sessions]\n", "\n[worktrees]")
            .contains("how long [hooks] pre_close and pre_worktree_remove may take to answer"),
        "the default config's [sessions] block never says the key limits the two that can refuse"
    );
}

/// `taw hooks test` runs hooks for real against ids that no server has, and the README says so
/// where it describes the verb, and tells a person how to keep a hook that acts on the pane
/// from acting in a test.
#[test]
fn the_readme_says_what_a_hooks_test_is_run_against_and_how_to_tell_it_from_a_real_event() {
    let hooks = readme_hooks();
    for needle in [
        "`%999999`",
        "`@999999`",
        "`$999999`",
        "`taw-hooks-test`",
        "name nothing on any tmux server",
        "`TAW_TEST`",
        "[ -n \"$TAW_TEST\" ] && exit 0",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
}

/// Every event of a test is told a pane, a window and a session, and the documents say so, with
/// the reason: a real event may have none, and a hook that acts on an empty `tmux` target acts on
/// the current pane. What the verb prints about its sample says so too.
#[test]
fn the_documents_say_every_event_of_a_test_is_told_all_three_ids_and_why() {
    let readme = flat(&readme_hooks());
    for needle in [
        "Every event is told all three, the ones a real event has no pane or window for included",
        "a real `workspace.renamed` has no `TAW_PANE`",
        "an empty `tmux` target is the current pane",
    ] {
        assert!(
            readme.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    assert!(
        compat.contains("name nothing on any tmux server and are given to every event"),
        "COMPAT never says the ids go to every event"
    );
    let skill = flat(&read("SKILL.md"));
    assert!(
        skill.contains("Every event of the sample has a pane, a window and a session id"),
        "SKILL.md never says every event has all three ids"
    );
    let note = taw::hooks::sample::note();
    assert!(
        note.contains("Every event has all three"),
        "the sample's own note never says it: {note}"
    );
}

/// A test names its session `=taw-hooks-test:`, and the documents say why a test run shows the `=`
/// and the `:`. tmux reads a name in a target as the start of a name when no session is called
/// exactly that, so `taw-hooks-test` alone would reach a session of the person's called
/// `taw-hooks-testing`; `=` is its mark for exactly this name, and `:` says the name is a
/// session's, since a command that takes a window reads a bare name as a window's first and then
/// as the start of a session's. A session called exactly `taw-hooks-test` is still reached, which
/// is what the `TAW_TEST` guard is for. A real event and a test with `--pane` have the plain name.
/// What the verb prints about its sample says so too.
#[test]
fn the_documents_say_why_a_test_names_its_session_with_an_equals_sign_and_a_colon() {
    let readme = flat(&readme_hooks());
    for needle in [
        "`TAW_SESSION` is `=taw-hooks-test:` in a test, and `=taw-hooks-test--feat-x:` for the worktree events",
        "tmux reads a name in a target as the start of a name when no session is called exactly that",
        "`tmux kill-session -t taw-hooks-test` would end a session of yours called `taw-hooks-testing`",
        "The `=` is tmux's mark for exactly this name, and the `:` after it says the name is a session's",
        "a command that takes a window (`new-window`, `kill-window`, `rename-window`) reads a bare name as a window's first and then as the start of a session's",
        "One session is still reached: one of yours called exactly `taw-hooks-test`, which is what the `[ -n \"$TAW_TEST\" ] && exit 0` line above is for",
        "A real event, and a test with `--pane`, has the plain name",
    ] {
        assert!(
            readme.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in [
        "`TAW_SESSION` is `=taw-hooks-test:` there, the `=` being tmux's mark for exactly that name and the `:` saying the name is a session's",
        "a hook that acts on it, a window command included, cannot reach a session of yours whose name only begins the same way",
        "a session of yours called exactly `taw-hooks-test` is still reached, which the `[ -n \"$TAW_TEST\" ] && exit 0` guard is for",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    assert!(
        flat(&read("SKILL.md")).contains("`TAW_SESSION` is `=taw-hooks-test:`, an exact session name, so a hook that acts on it finds no session either, unless the user has a session called exactly `taw-hooks-test`, which the `[ -n \"$TAW_TEST\" ] && exit 0` guard is for"),
        "SKILL.md never says the sample's session is an exact session name, and what is still reached"
    );
    let config = default_config_words("\n[hooks]\n", "\n[detection]");
    for needle in [
        "its TAW_SESSION is =taw-hooks-test:, which tmux reads as exactly that session",
        "one of yours called exactly that is still reached, which the line above is for",
    ] {
        assert!(
            config.contains(needle),
            "the default config's [hooks] block never says {needle:?}"
        );
    }
    let note = taw::hooks::sample::note();
    for needle in [
        "TAW_SESSION is =taw-hooks-test: in a test",
        "the = being tmux's mark for exactly that name and the : saying it is a session's",
        "A session called exactly taw-hooks-test is still reached",
    ] {
        assert!(
            note.contains(needle),
            "the sample's own note never says {needle:?}: {note}"
        );
    }
}

/// How a hook is started, said as the code does it: a directory that cannot be entered is the
/// home directory's, and starting a hook holds the caller up only until the hook has started, and
/// only when the JSON is larger than a pipe holds.
#[test]
fn the_readme_says_where_a_hook_starts_and_what_starting_it_waits_for() {
    let hooks = flat(&readme_hooks());
    for needle in [
        "when it exists and can be entered, else in your home directory",
        "does not wait for it to end",
        "JSON larger than a pipe holds",
        "holds the caller up until the hook has started",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    assert!(
        !hooks.contains("It never waits"),
        "the Hooks section still says a hook never waits"
    );
}

/// What the agent events do at the edges, said as the watcher does it: an agent whose process
/// ends mid-turn has the turn ended first, and one that reads `unknown` for a moment fires its
/// state again when it comes back. The README's Events section and COMPAT say so.
#[test]
fn the_documents_say_what_the_agent_events_do_when_an_agent_exits_or_reads_unknown() {
    let events = flat(&section(&read("docs/hooks.md"), "## Events"));
    for needle in [
        "When an agent's process ends while it is working or blocked, its turn is ended first",
        "`agent.done` (`agent.idle` if you are looking at the pane) and a done notification fire",
        "its leaving the pane after that is the status event alone",
        "can be read as `unknown` for a moment and then as its state again",
        "fires the agent event of the state it comes back to",
        "A new agent is read as `unknown` for its first few seconds",
    ] {
        assert!(
            events.contains(needle),
            "README's Events never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in [
        "has its turn ended first, so `agent.done` (`agent.idle` for a pane being looked at) fires before it leaves",
        "fires the agent event of the state it comes back to",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
}

/// The agents whose integration script reports a lifecycle state (`--state`) fire the same events for
/// what they report as any agent does, and the README names exactly those. The scripts are the
/// source: a new one that reports a state is not one the README can leave out. The watcher takes each
/// report in and announces the change once (the status event, the agent event, the notification), a
/// repeat is nothing, and the pane's own shell coming back makes it write the turn's end as it does
/// for any agent that exits, so the status event, the agent event and the done notification fire,
/// and the pane is let go a read later. The documents say all of it.
#[test]
fn the_readme_names_exactly_the_integrations_whose_scripts_report_a_state() {
    let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/integration/templates");
    let mut reporting: Vec<String> = std::fs::read_dir(&templates)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name()?.to_str()?.to_string();
            let agent = name.strip_prefix("taw-agent-state-")?.split('.').next()?;
            std::fs::read_to_string(&path)
                .ok()?
                .contains("--state")
                .then(|| agent.to_string())
        })
        .collect();
    reporting.sort();
    assert!(
        !reporting.is_empty(),
        "no integration script reports a state?"
    );

    let events = flat(&section(&read("docs/hooks.md"), "## Events"));
    let after = events
        .split("(`taw integration install` for ")
        .nth(1)
        .expect("the Events section never lists the agents that report their own state");
    let listed = after.split(')').next().unwrap();
    let mut named: Vec<String> = listed
        .split([',', ' '])
        .map(|word| word.trim_matches('`'))
        .filter(|word| !word.is_empty() && *word != "or")
        .map(str::to_string)
        .collect();
    named.sort();
    assert_eq!(named, reporting, "the README's list and the scripts differ");

    for needle in [
        "fires the same events for what it reports",
        "`taw pane report-agent` writes the state and nudges the pane's watcher, which takes the report in within a moment, though the pane prints nothing, and announces the change as it does one it reads off the screen: the status event, the `agent.*` event of the state and the `notification` hook where a toast is shown, once for each change",
        "The same report again fires nothing, and OpenCode makes it for every tool it runs",
        "A turn that ends while you are not looking at the pane is `done`, and `idle` if you are looking at it, as for any agent, and a second `idle` report leaves an unseen `done` as it is",
        "A turn shorter than one tick of the watcher, `working` and then `idle` reported back to back, is announced as well, its start and its end, from a stamp that `taw pane report-agent` leaves on the pane for each report of work",
        "Its end is announced too: when the pane's own shell is back at its prompt and the last state taw had heard of the agent was `working` or `blocked`, taw writes the turn's end (`idle`, or `done` while you are not looking at the pane) as it does for any agent that exits",
        "which covers an agent whose last report is its end, as `opencode run` ends",
        "so `agent.done` (`agent.idle` if you are looking at the pane), the status event and a done notification fire once",
        "the pane is let go a read later, which is the status event alone",
        "One that had been announced as `idle` or `done` has no turn to end, and only the status event fires",
        "report who the agent is and leave its state to taw, which reads it off the screen",
        "Watchers started by an older taw announce no reports until they are started again: `taw watch status` marks them, and `taw watch attach-all --restart`, run with the taw that is installed, starts each of them again",
    ] {
        assert!(
            events.contains(needle),
            "README's Events never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in [
        "fires these for what it reports, as any agent does for a change of state",
        "`taw pane report-agent` writes the state and nudges the pane's watcher, which takes the report in within a moment, though the pane prints nothing, and announces the change, once, with the status event, the `agent.*` event and the `notification` for the toast it earns",
        "The same report again fires nothing",
        "An `idle` that ends a turn for a pane that is not being looked at is `done`, and stays `done` when `idle` is reported again",
        "A turn shorter than one tick of the watcher, `working` and then `idle` reported back to back, is announced as its start and its end, from a stamp `taw pane report-agent` leaves on the pane",
        "Its end is announced too: when the pane's own shell is back at its prompt and the last state taw had heard was `working` or `blocked`",
        "so the status event, `agent.done` (`agent.idle` for a pane being looked at) and the done `notification` fire once",
        "letting the pane go a read later is the status event alone",
        "Watchers started by an older taw announce no reports until they are started again: `taw watch status` marks them, and `taw watch attach-all --restart`, run with the taw that is installed, starts each of them again",
    ] {
        assert!(
            compat.contains(needle),
            "COMPAT never says what an agent that reports its own state fires: {needle:?}"
        );
    }
}

/// A hook that cannot be started is usually one a watcher of a taw that has since been replaced
/// fired, and the README says how to start that watcher again: with the taw that is installed.
#[test]
fn the_readme_says_how_to_start_the_watcher_of_a_hook_that_cannot_be_started_again() {
    let readme = flat(&section(&read("docs/hooks.md"), "## How a hook runs"));
    for needle in [
        "One that cannot be started is usually fired by a watcher whose `taw` has been moved or replaced since it began, an upgrade doing that: the line names that `taw`.",
        "`taw watch status` marks the watchers that run an older one, and `taw watch attach-all --restart`, run with the current one, starts them again.",
    ] {
        assert!(
            readme.contains(needle),
            "README's \"How a hook runs\" never says {needle:?}"
        );
    }
}

/// The closes `pre_close` does not see, all of them: two are taw's own, and a person who guards
/// sessions in `pre_close` has to know both to guard removal as well.
#[test]
fn the_documents_name_every_close_that_pre_close_is_not_asked_about() {
    let readme = flat(&readme_hooks());
    for needle in [
        "tmux's own `prefix + x` and `prefix + &`, a process that ends by itself, and `taw plugin pane close`, which closes a pane a plugin opened",
        "`taw worktree remove` also closes the worktree's session when it is open, and asks `pre_worktree_remove` about that and not `pre_close`",
        "a hook that keeps a session open belongs under both keys",
    ] {
        assert!(
            readme.contains(needle),
            "README's Hooks section never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in [
        "`taw worktree remove` also closes the worktree's session when it is open, and asks only `pre_worktree_remove` about that",
        "`taw plugin pane close` closes a plugin's pane without asking",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
}

/// Two lines that describe hooks outside the README (the module map's row for the CLI, and the
/// acceptance script's row in the e2e README, which names `[hooks]`) follow the wording rule too.
#[test]
fn the_two_lines_about_hooks_outside_the_readme_hold_no_dash_arrow_or_curly_quote() {
    for (path, start) in [
        ("docs/ARCHITECTURE.md", "| `main.rs`, `cli/mod.rs`"),
        ("tests/e2e/README.md", "| `acceptance.sh` |"),
    ] {
        let text = read(path);
        let line = text
            .lines()
            .find(|line| line.starts_with(start))
            .unwrap_or_else(|| panic!("{path} has no line starting {start:?}"));
        let found = banned_in(path, line);
        assert!(found.is_empty(), "{}", found.join("\n"));
    }
}

/// Every account on a shared machine can read a command line with `ps`. The Pushover script and
/// the ntfy script with the priority are the README's own text, run here against a `curl` that
/// records what it is given: what is secret in them (the token and the user, the topic) is on its
/// standard input and nowhere on its command line.
#[test]
fn the_phone_scripts_keep_their_secrets_off_curls_command_line() {
    let phone = readme_phone();

    let pushover = fenced_block(&phone, "# ~/bin/push-pushover");
    let ran = run_with_a_recording_curl(
        &pushover,
        &[
            ("PUSHOVER_TOKEN", "SECRET-TOKEN"),
            ("PUSHOVER_USER", "SECRET-USER"),
            ("TAW_MESSAGE", "@claude blocked"),
        ],
    );
    assert!(ran.status.success(), "{}", ran.stderr);
    let args = ran.args.expect("curl never ran");
    assert!(
        !args.contains("SECRET"),
        "a secret is on the command line:\n{args}"
    );
    assert!(
        args.lines().any(|arg| arg == "message=@claude blocked"),
        "{args}"
    );
    assert!(args.lines().any(|arg| arg == "-K"), "{args}");
    let stdin = ran.stdin.unwrap();
    assert!(
        stdin.contains("form-string = \"token=SECRET-TOKEN\""),
        "{stdin}"
    );
    assert!(
        stdin.contains("form-string = \"user=SECRET-USER\""),
        "{stdin}"
    );

    let ntfy = fenced_block(&phone, "# ~/bin/push-ntfy");
    for (kind, priority) in [("request", "high"), ("done", "default")] {
        let ran = run_with_a_recording_curl(
            &ntfy,
            &[
                ("NTFY_TOPIC", "secret-topic"),
                ("TAW_KIND", kind),
                ("TAW_MESSAGE", "hello"),
            ],
        );
        assert!(ran.status.success(), "{}", ran.stderr);
        let args = ran.args.expect("curl never ran");
        assert!(
            !args.contains("secret-topic"),
            "the topic is on the command line:\n{args}"
        );
        assert!(
            args.lines()
                .any(|arg| arg == format!("Priority: {priority}")),
            "{kind}: {args}"
        );
        assert_eq!(
            ran.stdin.as_deref(),
            Some("url = \"https://ntfy.sh/secret-topic\"\n"),
            "{kind}"
        );
    }
    // No topic is said aloud and nothing is sent.
    let ran = run_with_a_recording_curl(&ntfy, &[("TAW_KIND", "done"), ("TAW_MESSAGE", "hello")]);
    assert!(!ran.status.success());
    assert!(ran.stderr.contains("set NTFY_TOPIC"), "{}", ran.stderr);
    assert!(ran.args.is_none(), "curl ran with no topic");
}

/// The README's `is-merged` example is tried by `taw hooks test`, whose sample checkout is an
/// empty directory and not a git worktree. Run there, without the line for a test it refuses
/// with git's error, which is why the line is in the example; with it, it passes.
#[test]
fn the_is_merged_example_passes_in_the_empty_checkout_of_a_hooks_test() {
    let script = fenced_block(&readme_hooks(), "# ~/bin/is-merged");
    let empty = tempfile::tempdir().unwrap();
    let run = |test: Option<&str>| {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(&script).current_dir(empty.path());
        match test {
            Some(value) => command.env("TAW_TEST", value),
            None => command.env_remove("TAW_TEST"),
        };
        command.output().unwrap()
    };

    let in_a_test = run(Some("1"));
    assert!(
        in_a_test.status.success(),
        "{}",
        String::from_utf8_lossy(&in_a_test.stderr)
    );
    let for_real = run(None);
    assert!(
        !for_real.status.success(),
        "an empty directory is not a merged branch, so a real removal is refused there"
    );
}

/// What a test leaves and what its sample has, said plainly: the directory is taken away, and the
/// worktree's checkout is empty.
#[test]
fn the_readme_says_a_hooks_test_takes_its_directory_away_and_has_no_git_checkout() {
    let hooks = flat(&readme_hooks());
    for needle in [
        "in a directory taw makes under the state directory for the purpose and takes away afterwards",
        "The `pre_worktree_remove` sample's checkout is an empty directory and not a git worktree",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
}

/// `taw hooks test` says first when it shows only the end of what a hook printed, and the
/// documents quote that line as the code says it.
#[test]
fn the_documents_quote_the_line_hooks_test_puts_first_when_it_cuts_a_hooks_output() {
    let note = taw::hooks::run::cut_note();
    assert_eq!(note, "(earlier output cut; the last 16 KiB follow)");
    let readme = flat(&readme_hooks());
    assert!(
        readme.contains(&note),
        "the Hooks section never says {note:?}"
    );
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    assert!(compat.contains(&note), "COMPAT never says {note:?}");
}

/// `taw hooks test <event> --pane <pane>` tells the event about a real pane, and the documents
/// say what that means: the ids are the pane's own, so a hook that acts on them acts on it, and the
/// output says it used a real pane. SKILL.md tells an agent not to pass a pane the user did not
/// name.
#[test]
fn the_documents_say_what_hooks_test_with_a_pane_does() {
    let hooks = flat(&readme_hooks());
    for needle in [
        "`taw hooks test <event> --pane <pane>` tells the event about a real pane instead",
        "that pane's own ids, its session, window and directory",
        "still with `TAW_TEST` set to `1`",
        "the output says it used a real pane",
        "a hook that acts on `$TAW_PANE` acts on it",
        "the pane's own session and directory stand for the worktree",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in ["`real_pane`", "--pane <pane>"] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    let skill = flat(&read("SKILL.md"));
    assert!(
        skill.contains("`--pane <id>`") && skill.contains("never pass one the user did not name"),
        "SKILL.md never says what --pane does or when not to pass it"
    );
}

/// `taw hooks test <event> --pane <pane>` refuses an event about something going away unless
/// `--force` is given, and the documents say which they are (from the table, so that one added
/// is one they must name), say what the refusal is, quoting it as the code says it, and tell an
/// agent that `--force` is not its to add.
#[test]
fn the_documents_say_which_events_hooks_test_refuses_with_a_pane_and_what_force_is_for() {
    let going: Vec<&str> = EVENTS
        .iter()
        .chain(VETOES)
        .filter(|event| event.going_away)
        .map(|event| event.name)
        .collect();
    assert!(
        going.len() >= 7 && going.contains(&"pane.exited"),
        "{going:?}"
    );

    let hooks = flat(&readme_hooks());
    // The paragraph itself lists them, and not only the table of events above it.
    let paragraph = hooks
        .split("An event about something going away")
        .nth(1)
        .and_then(|rest| rest.split("is refused with `--pane`").next())
        .expect("the Hooks section has no paragraph about the events --pane refuses");
    for name in &going {
        assert!(
            paragraph.contains(&format!("`{name}`")),
            "the paragraph about --pane never names {name}, which it refuses"
        );
    }
    let refusal = taw::hooks::sample::forced_message("worktree.removed", "%31");
    for needle in [
        "is refused with `--pane` unless you add `--force`",
        refusal.as_str(),
        "the exit status is 2 and nothing runs",
    ] {
        assert!(
            hooks.contains(needle),
            "the Hooks section never says {needle:?}"
        );
    }
    let compat = flat(&section(
        &read("docs/COMPAT.md"),
        "## Hooks from the config",
    ));
    for needle in ["`--force`", "exit status 2", "no envelope"] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    let sentence = compat
        .split("An event about something going away")
        .nth(1)
        .and_then(|rest| rest.split("is refused with `--pane`").next())
        .expect("COMPAT has no sentence about the events --pane refuses");
    for name in &going {
        assert!(
            sentence.contains(&format!("`{name}`")),
            "COMPAT never names {name}, which --pane refuses"
        );
    }
    let skill = flat(&read("SKILL.md"));
    assert!(
        skill.contains(
            "refuses an event about something closing, being removed or ending unless `--force` is added"
        ) && skill.contains("`--force` is the user's to add"),
        "SKILL.md never says what --force is for or whose it is"
    );
}

#[test]
fn the_readme_has_the_hooks_rows_in_its_tables() {
    let readme = read("docs/config.md");
    assert!(readme.contains("| `[hooks]` |"), "the sections table");
    assert!(
        readme.contains("| `[hooks] timeout_seconds` | `30` |"),
        "the keys table"
    );
    assert!(
        read("docs/beyond-herdr.md").contains("| **Hooks** (`[hooks]`)"),
        "Beyond herdr"
    );
    assert!(
        read("docs/cli.md").contains("`layout`, `notification`,\n`hooks`,"),
        "the list of CLI groups"
    );
}

#[test]
fn the_compat_document_says_what_the_config_hooks_hear_and_what_a_refusal_is() {
    let compat = section(&read("docs/COMPAT.md"), "## Hooks from the config");
    for needle in [
        "agent.done",
        "notification",
        "pre_close",
        "pre_worktree_remove",
        "hook_refused",
        "hooks_list",
        "hooks_test",
        "hooks_log",
        "[hooks] timeout_seconds",
    ] {
        assert!(
            compat.contains(needle),
            "docs/COMPAT.md never says {needle:?}"
        );
    }
}

#[test]
fn the_skill_tells_an_agent_about_hooks_test_and_about_a_refusal_it_must_not_work_around() {
    let skill = read("SKILL.md");
    assert!(
        skill.contains("`taw hooks test <event>`"),
        "SKILL.md never mentions taw hooks test"
    );
    assert!(
        skill.contains("`hook_refused`"),
        "SKILL.md never mentions hook_refused"
    );
    assert!(
        skill
            .lines()
            .any(|line| line.contains("hook_refused") && line.contains("work around"))
    );
}

#[test]
fn the_words_about_hooks_use_no_dash_arrow_or_curly_quote() {
    let config = read("src/config_default.toml");
    let default_hooks = config
        .split("\n[hooks]\n")
        .nth(1)
        .and_then(|rest| rest.split("\n[detection]").next())
        .expect("a [hooks] block in the default config");
    let skill = read("SKILL.md");
    let skill_lines: Vec<&str> = skill
        .lines()
        .filter(|line| line.contains("taw hooks") || line.contains("`hook_refused`"))
        .collect();
    assert!(!skill_lines.is_empty());

    let mut broken = Vec::new();
    broken.extend(banned_in("docs/hooks.md", &readme_hooks()));
    broken.extend(banned_in(
        "docs/notifications.md, Notifications on your phone",
        &readme_phone(),
    ));
    broken.extend(banned_in("src/config_default.toml, [hooks]", default_hooks));
    broken.extend(banned_in(
        "docs/COMPAT.md, Hooks from the config",
        &section(&read("docs/COMPAT.md"), "## Hooks from the config"),
    ));
    broken.extend(banned_in("SKILL.md, hooks lines", &skill_lines.join("\n")));

    assert!(
        broken.is_empty(),
        "what a person reads about hooks says what it may not:\n{}",
        broken.join("\n")
    );
}

/// What the code itself prints about hooks: when each event fires, a refusal's one line, and
/// the warnings `taw config check` gives.
#[test]
fn what_the_code_says_about_hooks_uses_no_dash_arrow_or_curly_quote() {
    use std::os::unix::process::ExitStatusExt;
    use taw::session::create::{Hook, HookFailure};

    let mut said: Vec<String> = EVENTS
        .iter()
        .chain(VETOES)
        .map(|event| event.when.to_string())
        .collect();
    said.push(taw::hooks::sample::note());
    said.push(taw::hooks::sample::real_note(
        &taw::hooks::sample::Story::invented(Path::new("/samples")),
    ));
    said.push(taw::hooks::sample::forced_message(
        "worktree.removed",
        "%31",
    ));
    for hook in [Hook::PreClose, Hook::PreWorktreeRemove] {
        for stderr in ["keep it open", ""] {
            let refusal = HookFailure::Exited {
                hook,
                command: "~/bin/veto".to_string(),
                status: std::process::ExitStatus::from_raw(1 << 8),
                stderr: stderr.to_string(),
            };
            said.push(refusal.summary());
            said.push(refusal.to_string());
        }
        said.push(
            HookFailure::TimedOut {
                hook,
                limit: std::time::Duration::from_secs(10),
            }
            .to_string(),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "[hooks]\ntimeout_seconds = 4000\n\"agent.finished\" = \"x\"\n\"agent.done\" = 5\n",
    )
    .unwrap();
    let paths = taw::paths::Paths {
        config_dir: dir.path().to_path_buf(),
        config_file: file,
        state_dir: dir.path().to_path_buf(),
        override_dir: dir.path().join("o"),
        remote_manifest_dir: dir.path().join("r"),
        manifest_marker: dir.path().join("m"),
        sessions_dir: dir.path().join("s"),
        plugin_state_dir: dir.path().join("ps"),
        plugins_dir: dir.path().join("p"),
        logs_dir: dir.path().join("l"),
    };
    let diagnostics = taw::config::check(&paths);
    assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
    said.extend(diagnostics.into_iter().map(|d| d.message));

    let broken = banned_in("what the code says", &said.join("\n"));
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}
