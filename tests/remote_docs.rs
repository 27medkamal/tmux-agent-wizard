//! The remote machines' documents say what is built, in the words it may use: the "Remote machines" page
//! (its words table is the one `src/machine/words.rs` says, called here with the same samples), the
//! config table row, the CLI paragraph; docs/PARITY.md; docs/COMPAT.md; SKILL.md; the long help.

use std::path::Path;

use taw::machine::words;

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

/// `text` with every version number (digits, a dot, digits, a dot, digits) written `X.Y.Z`: the shape of a
/// version is checked, never its digits.
fn semver_shape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    while at < chars.len() {
        let digits = |from: usize| {
            chars[from..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count()
        };
        let mut end = at;
        let mut parts = 0;
        while parts < 3 {
            let n = digits(end);
            if n == 0 {
                break;
            }
            end += n;
            parts += 1;
            if parts < 3 {
                if chars.get(end) == Some(&'.') {
                    end += 1;
                } else {
                    break;
                }
            }
        }
        let starts_word = at == 0 || !chars[at - 1].is_alphanumeric();
        if parts == 3 && starts_word && !chars.get(end).is_some_and(|c| c.is_alphanumeric()) {
            out.push_str("X.Y.Z");
            at = end;
        } else {
            out.push(chars[at]);
            at += 1;
        }
    }
    out
}

/// The remote machines page, whole: its heading is the page title.
fn section_text() -> String {
    read("docs/machines.md")
}

/// Words the docs avoid, whole words or phrases. "online" and "offline" are a word for a row:
/// the header count string quotes "offline" once, in the words table, whose lines are left out of
/// that one check.
const NEVER_SAY: [&str; 7] = [
    "real-time",
    "instant",
    "seamless",
    "just works",
    "secure connection",
    "remote control",
    "sync",
];

/// `text` as lowercase words, so that a phrase is found whole: `real-time` and `real time` are one.
fn words_of(text: &str) -> String {
    let spaced: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    format!(
        " {} ",
        spaced.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

fn has_phrase(text: &str, phrase: &str) -> bool {
    words_of(text).contains(&words_of(phrase))
}

#[test]
fn the_section_gives_the_wezterm_line_and_the_two_ways_in() {
    let text = section_text();
    let line = text
        .lines()
        .find(|line| line.contains("default_prog"))
        .expect("no WezTerm line");
    assert_eq!(
        line.trim(),
        r#"config.default_prog = { "/bin/zsh", "-l", "-c", "source ~/.zshrc; exec taw app" }"#
    );
    assert!(!line.contains("--go"), "the line never carries --go last");
    let flat = flat(&text);
    for needle in [
        "taw app",
        "within a second",
        "over your ssh",
        "[machines]",
        "ssh-agent",
        "ssh <destination>",
        "`Esc`",
        "`Q`",
        "`Ctrl-C`",
        "--go last",
        "--go <machine>:<id>",
        "TAW_MACHINE",
        "taw --machine",
        "machine_unreachable",
        "not connected",
        "last seen",
        "no time limit",
        "255",
        "backslash",
        "the popup",
    ] {
        assert!(flat.contains(needle), "the section never says {needle:?}");
    }
}

#[test]
fn every_string_of_the_header_and_row_words_is_in_the_section_with_the_sample_values() {
    let text = semver_shape(&flat(&section_text()));
    let version = env!("CARGO_PKG_VERSION");
    let ago = 180;
    let retry = 30;
    let mut expected: Vec<String> = vec![
        words::connected(version),
        words::not_answering_once(),
        words::needs_you_hint(),
        words::no_taw(),
        words::no_machines(""),
        words::no_machines(version),
        words::newer("9.0.0", version),
        words::login(),
        words::host_key("studio"),
        words::not_answering(Some(ago), retry),
        words::not_answering(None, retry),
        words::garbage_header(),
        words::connecting(Some(ago)),
        words::connecting(None),
        words::this_server("studio"),
        words::NO_KEEPER.to_string(),
        words::enter_goes_to("studio", "Q"),
        words::enter_on_machine_help("Q"),
        words::not_connected("pi", ago),
        words::no_preview("pi"),
        words::capture_age(ago),
        words::away_names(&["studio".to_string(), "pi".to_string()]),
        words::dropped_toast("pi"),
        words::gone("studio:%16"),
        words::no_such_machine("pi"),
        words::read_only("studio"),
        words::acted_on_from_the_app("studio"),
        words::GO_USAGE.to_string(),
        words::NO_SESSION_ROW.to_string(),
        words::no_session_now("studio"),
        words::NOTHING_TO_GO_IN_TO.to_string(),
        words::NO_SERVER_PATH.to_string(),
        words::worktree_open_failed("studio", "no such worktree"),
        words::worktree_open_unreadable("studio"),
        words::opening_worktree("studio"),
        words::worktree_open_timed_out("studio", 20),
        words::ssh_not_run("studio", "no such file"),
        words::waiting("studio"),
        words::no_answer("studio"),
        words::hook_refused(
            "studio",
            "pre-close hook",
            "uncommitted changes in ~/code/garden",
        ),
        words::still_answering("studio"),
        words::timed_out("studio", 20),
        words::row_gone("studio"),
        words::refused("studio", "pane '%9' not found"),
        words::answered_late("studio"),
        words::no_id_to_send("studio"),
        words::on_machine("studio", "garden:1"),
        words::group_close_named(
            &format!("kill-session {}?", words::on_machine("studio", "garden")),
            &["garden--rain", "garden--pdf"],
        ),
        words::group_close_unseen(&format!(
            "kill-session {}?",
            words::on_machine("studio", "garden")
        )),
        words::group_close_named(
            &format!(
                "kill-pane {} (claude)? {}",
                words::on_machine("studio", "garden:1"),
                taw::tui::app::SESSION_LAST_PANE
            ),
            &["garden--rain", "garden--pdf"],
        ),
        words::worktree_removed("studio", Some("w4")),
        words::worktree_made_not_opened("studio"),
        // The popup's group close toast (src/tui/actions.rs group_closed_toast, crate-private) said of a machine's row.
        format!(
            "closed {} and its 2 worktree sessions",
            words::on_machine("studio", "garden")
        ),
    ];
    for (code, signal) in [
        (Some(255), None),
        (Some(2), None),
        (Some(7), None),
        (None, Some(9)),
        (None, None),
    ] {
        expected.push(words::attach_ended("studio", code, signal).unwrap());
    }
    expected.push(words::attach_held(
        &words::attach_ended("studio", Some(7), None).unwrap(),
    ));
    // A whole table cell, so a string that is part of a longer one cannot be found in it. A version
    // is matched by its shape, so a version bump edits no README line.
    for words in expected {
        // A cell may begin or end with a space of its own, as needs_you_hint's does.
        let lead = if words.starts_with(' ') { " " } else { "" };
        let trail = if words.ends_with(' ') { " " } else { "" };
        let cell = format!("| `{lead}{}{trail}` |", semver_shape(&flat(&words)));
        assert!(
            text.contains(&cell),
            "the section's words table never has {words:?} as a cell of its own (the sample values are studio, pi, 3 min or 3m for an age, 30s for a retry and a version such as {version})"
        );
    }
}

#[test]
fn the_section_says_none_of_the_words_to_avoid_and_offline_only_in_the_table() {
    let text = section_text();
    for words in NEVER_SAY {
        assert!(
            !has_phrase(&text, words),
            "README's remote section says {words:?}"
        );
    }
    // Every backticked run is what taw prints, so it is left out;
    // the rest, a table's Means column included, is the README's own prose.
    let prose: String = text
        .split('`')
        .enumerate()
        .filter(|(at, _)| at % 2 == 0)
        .map(|(_, part)| part)
        .collect::<Vec<_>>()
        .join("\n");
    for word in ["online", "offline"] {
        assert!(
            !has_phrase(&prose, word),
            "README's remote section says {word:?} outside what taw prints"
        );
    }
}

#[test]
fn the_readme_names_machines_in_its_config_table_and_no_longer_says_ssh_is_the_remote_story() {
    let readme = read("docs/config.md");
    let table = readme.clone();
    assert!(
        table.lines().any(
            |line| line.starts_with("| `[machines]` |") && line.contains("\"Remote machines\"")
        ),
        "no [machines] row in the main sections table"
    );
    let flat_readme = flat(&readme);
    assert!(
        !flat_readme.contains("`ssh` is the remote story"),
        "README still says ssh is the remote story"
    );
    assert!(
        flat_readme.contains("`[remote]`/`[server]`")
            && flat_readme.contains("`taw config check` names their keys as unknown"),
        "the [remote]/[server] line says what the code does"
    );
    let cli = flat(&read("docs/cli.md"));
    for needle in [
        "except eight",
        "`taw app`",
        "`taw machine feed`",
        "`taw machine attach`",
        "`taw machine check`",
        "taw --machine NAME",
        "`machine_unreachable`",
        "`app`, `machine`",
    ] {
        assert!(cli.contains(needle), "README's CLI never says {needle:?}");
    }
    let beyond = read("docs/beyond-herdr.md");
    assert!(
        beyond.contains("**Machines you do not sit at**"),
        "a row of its own in Beyond herdr"
    );
    let ssh_net = flat(&read("docs/development.md"));
    for name in ["ssh", "scp", "sftp", "ssh-add", "ssh-agent"] {
        assert!(
            ssh_net.contains(&format!("`{name}`")),
            "the test net text never names {name}"
        );
    }
}

#[test]
fn parity_has_the_saved_machines_row_the_machine_row_and_keeps_remote_and_server_ignored() {
    let parity = read("docs/PARITY.md");
    let row = |starts: &str| -> String {
        parity
            .lines()
            .find(|line| line.starts_with(starts))
            .unwrap_or_else(|| panic!("PARITY has no row starting {starts:?}"))
            .to_string()
    };
    let saved = row("| Saved machines");
    assert!(saved.contains("`[machines]`") && saved.contains("herdr machine add"));
    assert!(
        !saved.contains("Accepted and ignored"),
        "a parity row, not an ignored one"
    );
    let machine = row("| Running a command on a machine");
    assert!(machine.contains("--machine"));
    let attach = row("| Remote attach with local keybindings");
    assert!(attach.contains("its keys are its own"));
    assert!(attach.contains("image paste"));
    let ignored = row("| `[remote]` / `[server]`");
    assert!(ignored.contains("Accepted and ignored"));
    assert!(row("| named sessions, detach/attach, ssh attach").contains("taw app"));
}

#[test]
fn compat_names_the_app_the_verbs_the_ids_the_feed_and_the_codes() {
    let compat = flat(&section(&read("docs/COMPAT.md"), "## Machines"));
    for needle in [
        "`taw app",
        "`taw machine list`",
        "`taw machine check",
        "`taw machine feed",
        "`taw machine attach",
        "`taw --machine NAME",
        "cli:machine:list",
        "cli:machine:check",
        "`cli:machine`",
        "`machine_unreachable`",
        "protocol 1",
        "`hello`",
        "`state`",
        "`toast`",
        "`ping`",
        "`left`",
        "`lease`",
        "`walk`",
        "no time limit",
        "255",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
}

#[test]
fn skill_has_the_act_on_another_machine_section_right_after_use_ids() {
    let skill = read("SKILL.md");
    let use_ids = skill.find("\n## Use IDs").unwrap();
    let next = skill[use_ids + 1..]
        .find("\n## ")
        .map(|at| at + use_ids + 1)
        .unwrap();
    assert!(
        skill[next..].starts_with("\n## Act on another machine\n"),
        "the section is the one right after Use IDs"
    );
    let text = flat(&section(&skill, "## Act on another machine"));
    for needle in [
        "taw --machine NAME",
        "ids are that machine's",
        "`machine_unreachable`",
        "Nothing is retried",
        "no time limit",
        "255",
        "`taw app`",
    ] {
        assert!(
            text.contains(needle),
            "SKILL's machine section never says {needle:?}"
        );
    }
    for words in NEVER_SAY {
        assert!(
            !has_phrase(&text, words),
            "SKILL's machine section says {words:?}"
        );
    }
}

#[test]
fn the_long_help_names_the_four_commands_that_print_no_envelope_of_their_own() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_taw"))
        .arg("--help")
        .env_clear()
        .output()
        .expect("taw --help runs");
    let help = flat(&String::from_utf8_lossy(&out.stdout));
    for needle in [
        "`taw app`",
        "`taw machine feed`",
        "`taw machine attach`",
        "`taw machine check`",
    ] {
        assert!(help.contains(needle), "the long help never names {needle}");
    }
}

/// With no app running the popup still names each machine of the table and says how to connect it, and the
/// pages say where `taw` is on the PATH of a terminal that is not tmux's.
#[test]
fn the_docs_say_what_the_popup_shows_with_no_app_and_where_taw_is_on_the_path_outside_tmux() {
    let page = read("docs/machines.md");
    let ways = flat(&section(&page, "## Two ways in"));
    assert!(
        !ways.contains("shows this machine alone"),
        "the page still says the popup shows this machine alone"
    );
    let words = format!("`{}`", words::NO_KEEPER);
    for needle in [
        "With no app running, the popup lists this machine's sessions, and each machine's header says",
        words.as_str(),
        "Outside tmux `taw` is on your `PATH` only through the one link on the [install page](install.md)",
    ] {
        assert!(ways.contains(needle), "Two ways in never says {needle:?}");
    }
    let install = flat(&read("docs/install.md"));
    assert!(
        install.contains(
            "`~/.local/bin` has to be on that shell's `PATH`, or use another folder that is"
        ),
        "the install page never says the link's folder has to be on the PATH"
    );
}

/// What Enter does on another machine's row in the popup is said in the same words by the hint line, the help and the
/// page: `Two ways in` has them, in one paragraph.
#[test]
fn the_docs_say_what_enter_does_on_a_machines_row_in_the_popup_in_the_words_of_the_hint_and_the_help()
 {
    let page = read("docs/machines.md");
    let ways = flat(&section(&page, "## Two ways in"));
    for needle in [
        format!("`Enter {}`", words::enter_goes_to("studio", "Q")),
        format!("`{}`", words::enter_on_machine_help("Q")),
        "`Ctrl-C` in the search preset or when `[keys]` gives `Q` to another action".to_string(),
    ] {
        assert!(ways.contains(&needle), "Two ways in never says {needle:?}");
    }
}

/// A removal git refused for changes in the checkout is the code `worktree_dirty`: the README says so
/// where the app asks `remove anyway?` of a machine's row and where it describes `worktree remove`,
/// and COMPAT's row for the verb says so, so the code the app acts on is not left to SKILL alone.
#[test]
fn the_readme_and_compat_say_a_refused_removal_is_worktree_dirty() {
    assert!(taw::cli::envelope::CODES.contains(&"worktree_dirty"));
    let readme = read("docs/machines.md");
    let actions = flat(&section(&readme, "## Acting on a machine's rows"));
    assert!(
        actions.contains("(the machine answers `worktree_dirty`) asks `git will not remove"),
        "the actions paragraph never names worktree_dirty for the dirty worktree question"
    );
    let flat_readme = flat(&read("docs/sessions.md"));
    assert!(
        flat_readme
            .contains("`taw worktree remove` has the same `--force`, answers `worktree_dirty`"),
        "README's worktree remove paragraph never names worktree_dirty"
    );
    let compat = read("docs/COMPAT.md");
    let row = compat
        .lines()
        .find(|l| l.starts_with("| `taw worktree remove"))
        .expect("COMPAT has no row for taw worktree remove");
    assert!(row.contains("`worktree_dirty`"), "COMPAT's row: {row}");
    assert!(row.contains("remove anyway?"), "COMPAT's row: {row}");
}

/// The README and the other docs call the machine they run on `this machine`, as the words do.
#[test]
fn the_docs_say_this_machine_and_never_this_mac_or_a_dotfiles_sample_session() {
    for file in [
        "README.md",
        "docs/machines.md",
        "docs/cli.md",
        "docs/config.md",
        "docs/index.md",
        "docs/PARITY.md",
        "docs/COMPAT.md",
        "SKILL.md",
    ] {
        let text = read(file);
        assert!(!text.contains("this Mac"), "{file} says this Mac");
        assert!(!text.contains("This Mac"), "{file} says This Mac");
    }
    let compat = read("docs/COMPAT.md");
    assert!(
        !compat.contains("'dotfiles'"),
        "COMPAT names a session dotfiles"
    );
    assert!(
        !compat.contains("=dotfiles"),
        "COMPAT names a session dotfiles"
    );
}

/// A machine is added with a command, so the machines page shows it where the config is first shown and
/// COMPAT lists both verbs; every limit the page once had is said where a reader meets it (image paste
/// is carried now, so the page has no list of limits left).
#[test]
fn the_docs_add_machines_with_a_command_and_each_old_limit_sits_where_it_is_met() {
    let page = read("docs/machines.md");
    let flat_page = flat(&page);
    assert!(
        !flat_page.contains("not by a command"),
        "the machines page still says machines are not saved by a command"
    );
    let setting = flat(&section(&page, "## Setting a machine up"));
    for needle in [
        "taw machine add studio studio.local",
        "taw machine add pi pi --taw ~/.local/bin/taw",
        "`taw machine remove pi`",
        "keeps every comment and line of the file as it was",
        "It refuses a name that is already listed, and changes nothing when it refuses",
        "A running app picks a change up within a couple of seconds, with no restart",
        "Keep taw at the same version on every machine: a machine whose taw is older can drop a toast forwarded through it without a word",
        "Nothing is copied between machines: the app lists and shows, and taw on each machine does the work there",
    ] {
        assert!(setting.contains(needle), "setting up never says {needle:?}");
    }
    assert!(
        !page.contains("\n## Limits\n")
            && !flat_page.contains("Image paste over ssh is not carried"),
        "the machines page still lists a limit that image paste lifted"
    );
    let places = [
        (
            "docs/machines.md",
            "## The app's screen",
            "A machine that is away shows its last words and when it was last seen",
        ),
        (
            "docs/machines.md",
            "## Going in, and the key back",
            "No key of a machine's is forwarded into the app: the machine's own tmux has the terminal, and its keys are its own",
        ),
        (
            "docs/machines.md",
            "## Going in, and the key back",
            "the key goes back to whichever app attached the client, the app you started, a doorway app, an app in another terminal, or an app you reopened a moment after the last one quit",
        ),
        (
            "docs/machines.md",
            "## Going in, and the key back",
            "`taw app --go studio:%16` waits up to 10 s for studio's first list before it goes in, so a connected machine that sends none leaves the screen blank that long",
        ),
    ];
    for (file, heading, needle) in places {
        let body = flat(&section(&read(file), heading));
        assert!(body.contains(needle), "{heading} never says {needle:?}");
    }
    for (file, needle) in [
        (
            "docs/herdr.md",
            "herdr's `[remote]` and `[server]` tables stay accepted and ignored",
        ),
        (
            "docs/troubleshooting.md",
            "On a slow or loaded machine, `prefix + O` there toward an agent elsewhere can show the list instead: press it again on the list",
        ),
    ] {
        assert!(
            flat(&read(file)).contains(needle),
            "{file} never says {needle:?}"
        );
    }
    assert!(flat_page.contains("within a second of a change there, not at once"));
    let compat = flat(&read("docs/COMPAT.md"));
    for needle in [
        "`taw machine add <name> <ssh-host> [--taw <path>]` | `cli:machine:add`",
        "`taw machine remove <name>` | `cli:machine:remove`",
    ] {
        assert!(compat.contains(needle), "COMPAT never says {needle:?}");
    }
    let parity = read("docs/PARITY.md");
    let saved = parity
        .lines()
        .find(|line| line.starts_with("| Saved machines"))
        .expect("a Saved machines row");
    assert!(saved.contains("`taw machine add`") && saved.contains("herdr machine add"));
}

/// The guardrails of the attach (a failed attach keeps its words, the doorway's way back to tmux, no headers
/// over one machine) are said on the machines page, each in the words the page uses.
#[test]
fn the_attach_guardrails_are_documented() {
    let page = flat(&read("docs/machines.md"));
    for needle in [
        "runs the machine's tmux with `TERM=xterm-256color` when the entry is not there, or when `TERM` is unset or empty",
        "A terminal the machine knows keeps its own `TERM`",
        "it prints one line under them, `taw: <what went wrong>. Press Enter to go back to the list.`, and waits",
        "an attach that ended well holds nothing",
        "`taw app --back '$3'`",
        "the terminal becomes a tmux client again on that session, on the same tmux server",
        "it attaches to the session tmux picks, the most recent one",
        "If the tmux server is gone, the app ends as an app does",
        "`Q back to tmux`",
        "the list draws no machine header, as with no `[machines]` at all",
        "As soon as one other machine is listed, every header is drawn",
    ] {
        assert!(
            page.contains(needle),
            "the machines page never says {needle:?}"
        );
    }
}

/// A connected machine with no tmux session has its own row, Enter on it starts one, every attach says the
/// way back on the machine's own status line, and a doorway app says back to tmux where it said quit.
#[test]
fn the_docs_say_what_a_machine_with_no_tmux_session_shows_and_the_way_back_on_attach() {
    let machines = flat(&read("docs/machines.md"));
    for needle in [
        "no tmux session here: Enter starts one",
        "`taw machine attach --new`",
        "`taw app --go studio:new`",
        "It takes a number from the same 1 to 9 as the session rows, at its place in the list",
        "that number key puts the cursor on it, as a session's number does",
        "`Tab` passes it by",
        "studio has no tmux session now: Enter on its row starts one",
        "for 4 s on the status line of that machine's tmux",
        "C-b + A: back to the taw list",
        "In such an app every word that would say quit says `back to tmux`",
        "With no other machine listed, nothing names this machine",
        "or a machine and new as in studio:new",
    ] {
        assert!(
            machines.contains(needle),
            "docs/machines.md never says {needle:?}"
        );
    }
    let cli = flat(&read("docs/cli.md"));
    for needle in ["`taw machine attach --new`", "`taw app --go studio:new`"] {
        assert!(cli.contains(needle), "docs/cli.md never says {needle:?}");
    }
}
