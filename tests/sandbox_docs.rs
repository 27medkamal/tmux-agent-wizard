//! The sandbox's documents say what it does in the words it may use (the words to avoid are listed
//! below): README's "Agents in a sandbox", its legend line and the limits it names; SKILL.md;
//! docs/COMPAT.md; docs/PARITY.md.

use std::path::Path;

fn read(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

/// The text under `heading` up to the next heading of the same depth or a shallower one.
fn section(text: &str, heading: &str) -> String {
    let depth = heading.chars().take_while(|c| *c == '#').count();
    let start = text
        .find(&format!("{heading}\n"))
        .unwrap_or_else(|| panic!("no {heading:?}"));
    let body = &text[start + heading.len() + 1..];
    let end = body
        .lines()
        .scan(0usize, |at, line| {
            let here = *at;
            *at += line.len() + 1;
            Some((here, line))
        })
        .find(|(_, line)| {
            let marks = line.chars().take_while(|c| *c == '#').count();
            marks > 0 && marks <= depth && line.chars().nth(marks) == Some(' ')
        })
        .map_or(body.len(), |(at, _)| at);
    body[..end].to_string()
}

/// `text` with every run of whitespace, a line's end and the next line's indent included, as one
/// space: a phrase wrapped over two lines of the markdown is found as the reader reads it.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The words to avoid, as the default profile's test has them.
const NEVER_SAY: [&str; 13] = [
    "secure",
    "safe",
    "can't escape",
    "isolated",
    "zero risk",
    "untouched",
    "stops prompt injection",
    "stops data theft",
    "never enter the container",
    "vm-level",
    "verified sandboxed",
    "works with every agent",
    "no performance cost",
];

#[test]
fn the_readme_says_what_a_sandbox_is_and_none_of_the_words_to_avoid() {
    let text = flat(&read("docs/sandbox.md"));
    let lower = text.to_lowercase();
    for words in NEVER_SAY {
        assert!(
            !lower.contains(words),
            "README's sandbox section says {words:?}"
        );
    }
    assert!(
        !lower.contains("alias"),
        "the sandbox section has no alias line"
    );
    for needle in [
        "taw sandbox [<agent>] [--profile <name>] [-- <the agent's own arguments>]",
        "It is not a wall against an agent that is trying to get out",
        "in a container that taw started, or saw start",
        "TAW_SANDBOX_DIR",
        "TAW_SANDBOX_AGENT",
        "TAW_SANDBOX_REPORT",
        "TAW_SANDBOX_PROFILE",
        "TAW_SANDBOX_PANE",
        "-v \"$TAW_SANDBOX_REPORT:/run/taw\" -e TAW_REPORT_FILE=/run/taw/session.json",
        // Item 3 of what a profile does names the agent inside too, as the default profile does.
        "TAW_REPORT_AGENT",
        // The preview's third title: a profile runs and no container is seen.
        "sandbox profile <name>, no container seen",
        // The line restore types, in its common case, and how it names taw otherwise.
        "taw sandbox claude -- --resume <id>",
        "when it is under the pane's home",
        "skip resume in pane %31: its sandbox profile team is gone",
        "its sandbox profile team is not executable (chmod +x",
        "container taw did not start, so it is not started again outside it",
        "a sandboxed agent starts only through its profile",
        "did not start in its sandbox in pane %31",
        "taw sandbox: not in tmux, so this agent gets no popup row and no restore",
        "taw sandbox: not in a tmux pane, so this agent gets no popup row and no restore",
        // Every refusal of the start, in the code's words.
        "taw sandbox: not typed at the prompt of pane",
        "agents and scripts start one with taw agent start --sandbox",
        "so taw cannot tell which profile you chose: fix it (taw config check says what is wrong), or name one with --profile",
        "taw sandbox runs the agent in the pane it is typed in: make the place first",
        "~ is your home folder, not a project",
        "in a git checkout of your whole home folder",
        "tmux's socket folder",
        "taw's state folder",
        "taw's config folder",
        "openco\u{2026} \u{1f4e6}",
        "sandbox.started",
        "sandbox.stopped",
        // The JSON field's two nulls.
        "`profile` is `null` for a container taw did not start",
        "`runtime` is `null` while no container is seen",
        "taw agent start <name> --kind <agent> --pane <id> --sandbox",
        "a save keeps the conversation it is resuming",
        "Docker makes the two folders at the top level, empty",
        // One sentence for every agent's restore, none named as a case of its own.
        "only when its own integration is installed (`taw integration install <agent>`)",
        "sandbox.agents.claud: taw knows no agent called claud",
    ] {
        assert!(
            text.contains(needle),
            "README's sandbox section never says {needle:?}"
        );
    }
    for line in [
        "ask_first=yes",
        "settings=own",
        "block_local_network=yes",
        "open_ports=\"8317\"",
        "docker=read-only",
        "docker=full",
        "runtime=podman",
        "tools=\"node@22 uv\"",
        "pass=\"OPENAI_API_KEY GH_TOKEN\"",
    ] {
        assert!(
            text.contains(&format!("| `{line}` |")),
            "the recipe line {line} is not a row of README's table"
        );
    }
}

#[test]
fn the_recipes_table_marks_every_agent_tested_or_untested() {
    let text = section(&read("docs/sandbox.md"), "## Logging in, agent by agent");
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.starts_with("| ") && !line.starts_with("| Agent") && !line.starts_with("|---")
        })
        .collect();
    assert_eq!(rows.len(), 12, "twelve agents have a recipe: {rows:?}");
    for row in rows {
        assert!(
            row.trim_end().ends_with("| tested |") || row.trim_end().ends_with("| untested |"),
            "{row}"
        );
    }
}

#[test]
fn the_readme_has_the_legends_line_the_pointer_the_addition_and_the_commands() {
    let readme = read("docs/keys.md");
    let glyphs = flat(&section(&readme, "## Dots and glyphs"));
    assert!(
        glyphs.contains("in a container that taw started, or saw start"),
        "the legend's line"
    );
    // The containers section keeps its restore paragraph and its limits, and points at the sandbox section.
    let containers = flat(&read("docs/agents/containers.md"));
    assert!(
        containers.contains("(see \"Agents in a sandbox\")"),
        "it points at the sandbox section"
    );
    let beyond = read("docs/beyond-herdr.md");
    assert!(
        beyond.contains("**Agents in a sandbox**"),
        "a row of its own"
    );
    let cli = flat(&read("docs/cli.md"));
    for command in [
        "except eight",
        "taw sandbox [<agent>] [--profile <name>] [-- <agent args>]",
        "taw sandbox new <name> [--from <profile>]",
        "taw sandbox list",
        "--sandbox [--profile <name>]",
        "with `--sandbox`, 300000",
    ] {
        assert!(cli.contains(command), "README's CLI never names {command}");
    }
    let trouble = flat(&read("docs/troubleshooting.md"));
    for words in [
        "waiting for docker to answer",
        "no recipe for",
        "is not executable (chmod +x",
    ] {
        assert!(
            trouble.contains(words),
            "Troubleshooting never says {words:?}"
        );
    }
}

/// The units of a document that the words-to-avoid check reads: a paragraph is one unit read as one
/// line of text (so a phrase wrapped over two lines is found), and every row of a table is a unit
/// of its own (a table is one paragraph to the blank-line split, and its rows have nothing to do
/// with each other).
fn units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut paragraph = String::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('|') {
            if !paragraph.is_empty() {
                units.push(flat(&paragraph));
                paragraph.clear();
            }
            if !line.trim().is_empty() {
                units.push(flat(line));
            }
        } else {
            paragraph.push_str(line);
            paragraph.push('\n');
        }
    }
    if !paragraph.is_empty() {
        units.push(flat(&paragraph));
    }
    units
}

/// Every paragraph and table row of the four documents that speaks of a sandbox or 📦, outside
/// README's own section (which has its test above), keeps to the words allowed. Two
/// literal phrases that are not claims about the sandbox are taken out first: git's
/// `safe.directory` and zsh's "insecure directories". A guard: it passes while the text is clean.
#[test]
fn no_paragraph_about_a_sandbox_uses_a_word_to_avoid() {
    for file in [
        "README.md",
        "docs/sandbox.md",
        "docs/index.md",
        "SKILL.md",
        "docs/COMPAT.md",
        "docs/PARITY.md",
    ] {
        for unit in units(&read(file)) {
            let lower = unit
                .to_lowercase()
                .replace("safe.directory", "")
                .replace("insecure directories", "");
            if !(lower.contains("sandbox") || unit.contains('\u{1f4e6}')) {
                continue;
            }
            for words in NEVER_SAY {
                assert!(
                    !lower.contains(words),
                    "{file} says {words:?} in a paragraph about the sandbox: {unit}"
                );
            }
        }
    }
}

#[test]
fn skill_compat_and_parity_say_what_the_sandbox_adds() {
    let skill = read("SKILL.md");
    let section = section(&skill, "## Make a sandbox profile with the user");
    for needle in [
        "Do this only when the user asks for a sandbox profile.",
        "Run `taw sandbox new <name>` and read the file's path from `.result.path`.",
        "`TAW_SANDBOX_PRINT=1 <path> <agent>`",
        "Never write a secret's value",
        "`taw agent start <agent-name> --kind <agent> --pane <id> --sandbox --profile <name>`",
    ] {
        assert!(
            flat(&section).contains(needle),
            "SKILL.md's section never says {needle:?}"
        );
    }
    let skill = flat(&skill);
    assert!(
        skill.contains("add `--sandbox`"),
        "Start and coordinate an agent gains its line"
    );
    assert!(
        skill.contains("A fourth, `taw sandbox` typed with an agent or nothing"),
        "the raw-output paragraph names taw sandbox's start as the fourth command without an envelope"
    );

    let compat = flat(&read("docs/COMPAT.md"));
    // COMPAT keeps the two sandbox events, and that they are hooks only.
    for guard in [
        "`sandbox.started`",
        "`sandbox.stopped`",
        "Hooks only: no plugin event fires",
    ] {
        assert!(compat.contains(guard), "COMPAT lost {guard:?}");
    }
    for needle in [
        "`cli:sandbox:new`",
        "`cli:sandbox:list`",
        "`--sandbox`",
        "\"sandbox\": {\"profile\"",
        "\"refused\": [{\"name\", \"why\"}]",
        "\"from\"",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }

    let parity = flat(&read("docs/PARITY.md"));
    assert!(
        !parity.contains("restore cannot yet resume such an agent"),
        "the detection row's old limit is gone"
    );
    assert!(
        !parity.contains("cannot resume such an agent"),
        "the container row's old limit is gone"
    );
    assert!(
        parity.contains("restore brings it back through that profile"),
        "the detection row notes the profile"
    );
    assert!(
        parity.contains("| an agent in a sandbox"),
        "Beyond herdr's row is in the ledger too"
    );
}
