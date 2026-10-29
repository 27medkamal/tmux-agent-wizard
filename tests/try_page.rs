//! `docs/try/index.html` is the public "try it in your browser" page: the popup over made-up
//! projects, with public copy. These checks hold what it must say and must not say. Its
//! wording (no em-dash, en-dash, arrow or curly quote, comments included) is scanned with every
//! other file a person reads, by `tests/user_text.rs`.

use std::path::{Path, PathBuf};

const PAGE: &str = "docs/try/index.html";

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The page shows remote machines as a switch under the window, labelled as the README names
/// them, with the sample machines' header drawn, and no choice of a question that was settled
/// (where a new terminal starts, what Esc does, the popup key inside, next up).
#[test]
fn the_published_page_shows_the_remote_machines_switch() {
    let page = read(PAGE);
    assert!(
        page.contains("<span>Remote machines</span>"),
        "{PAGE} has no switch labelled Remote machines"
    );
    assert!(page.contains("id=\"opt-remote\""), "the switch itself");
    for sample in ["this Mac", "studio", "build-box", "pi"] {
        assert!(page.contains(sample), "the sample machine {sample}");
    }
    for gone in ["rmstart", "rmesc", "rmkey", "rmnext"] {
        assert!(!page.contains(gone), "{PAGE} still holds the choice {gone}");
    }
}

/// The page opens on the richest view: the switch is ticked and the page's script turns the
/// machines on before its first drawing, so the top line counts across machines and Tab walks
/// across them. Two sample machines answer and one has dropped off, and between them the
/// connected machines have agents blocked, working, done and idle (the page's own machine has
/// the same), so the counts and Tab have something to cross.
#[test]
fn the_remote_machines_switch_is_on_when_the_page_opens() {
    let page = read(PAGE);
    let input = page.find("id=\"opt-remote\"").expect("the switch itself");
    let tag = &page[page[..input].rfind('<').unwrap()..input + page[input..].find('>').unwrap()];
    assert!(
        tag.contains("checked"),
        "{PAGE}: the switch is not drawn ticked: {tag}"
    );
    let start = page
        .find("  setRemote(true);\n  applyNaming();")
        .unwrap_or_else(|| {
            panic!("{PAGE}: the page does not turn the machines on before its first drawing")
        });
    assert!(
        page[start..].contains("syncControls(); draw();"),
        "{PAGE}: the first drawing is not after the machines are on"
    );
    assert!(
        page.contains("m === 'studio' || m === 'build-box' ? 'live' : S.rmPi"),
        "{PAGE}: studio and build-box no longer answer, or pi does"
    );
    assert!(
        page.contains("rmPi: 'dropped'"),
        "{PAGE}: pi starts off the network"
    );
    let from = page
        .find("const baseRemote = ")
        .expect("the sample machines");
    let remote = &page[from..from + page[from..].find("let RSTASH").unwrap()];
    for machine in ["studio", "build-box", "pi"] {
        assert!(
            remote.contains(&format!("machine: '{machine}'")),
            "{PAGE}: no sample agent on {machine}"
        );
    }
    let states_of = |machines: &[&str]| -> Vec<String> {
        remote
            .lines()
            .filter(|l| {
                machines
                    .iter()
                    .any(|m| l.contains(&format!("machine: '{m}', session")))
            })
            .filter_map(|l| l.split("state: '").nth(1))
            .map(|r| r.split('\'').next().unwrap().to_string())
            .collect()
    };
    let connected = states_of(&["studio", "build-box"]);
    for state in ["blocked", "working", "done", "idle"] {
        assert!(
            connected.iter().any(|s| s == state),
            "{PAGE}: the connected machines have no agent {state}: {connected:?}"
        );
    }
}

/// The page names one popup key, the one the plugin binds (`default_keys` in the plugin
/// script): the key the reader is told and the key its script binds inside a machine agree,
/// and only taw's own words are drawn for the actions the machines can end in.
#[test]
fn the_page_names_the_default_popup_key_and_taws_own_words_for_remote_actions() {
    let page = read(PAGE);
    let plugin = read("tmux-agent-wizard.tmux");
    assert!(
        plugin.contains("default_keys=\"A\""),
        "the plugin's default popup key is no longer A: update the page and this test"
    );
    assert!(page.contains("prefix + A opens taw"), "the title bar's key");
    assert!(page.contains("ctrl-b then A"), "the key inside a machine");
    for stale in [
        "ctrl-b then C",
        "prefix+C",
        "prefix + C",
        "k === 'C'",
        "e.key === 'C'",
    ] {
        assert!(
            !page.contains(stale),
            "{PAGE} still names the old key: {stale}"
        );
    }
    for words in [
        "did not answer; look again before trying once more",
        ": that row is gone",
        "it may still have run",
        "`waiting for ${m}`",
        "is not connected",
    ] {
        assert!(page.contains(words), "{PAGE} lacks taw's words {words:?}");
    }
    for invented in [
        "read-only here",
        "nothing is waiting",
        "will wait: do it",
        "greyed when it has dropped off",
    ] {
        assert!(
            !page.contains(invented),
            "{PAGE} draws words taw never prints: {invented:?}"
        );
    }
}

/// What a browser cannot show (the terminal closing, a key the page does not take, a tmux the
/// page does not run) is the page's own narration, drawn in a note under the window headed
/// `page:`, never as a toast, which is only what taw prints; and the sample error that an
/// action ends in belongs to that action.
#[test]
fn the_page_narrates_what_it_cannot_show_in_a_note_and_not_as_a_toast() {
    let page = read(PAGE);
    assert!(
        page.contains("id=\"pagenote\""),
        "the note under the window"
    );
    assert!(page.contains("'page: ' + S.pageNote"), "the note's heading");
    for narration in [
        "here taw would end and the terminal would close",
        "the popup key does nothing on this page",
        "keys here would go to ${m}'s tmux; ctrl-b then A would take you back to the app",
        "here taw would switch the client on ${nameOf(m)} to ${placeLabel(id)}, next up",
    ] {
        assert!(
            page.contains(narration),
            "{PAGE} lacks the narration {narration:?}"
        );
    }
    for toast in [
        "taw app ends, and the terminal window closes",
        "you are in the app: the popup key does nothing here",
        "keys here go to ${m}'s tmux",
        ": switch-client on ${nameOf(m)} to ${placeLabel(id)}`)",
    ] {
        assert!(
            !page.contains(toast),
            "{PAGE} draws a toast taw never prints: {toast:?}"
        );
    }
    for error in ["closing pane '%9'", "closing workspace '$3'"] {
        assert!(
            page.contains(error),
            "{PAGE} lacks the error of a close: {error:?}"
        );
    }
}

/// What the popup does to tmux is not something taw prints: the page draws no toast that spells a
/// tmux or git command (`kill-pane -t "..."`, `switch-client -t "..."` and the rest). Where the
/// words teach, a page note says what taw runs; where the screen shows the effect, nothing is said.
/// A machine's group question is strict, as the app asks it (`Confirm::strict`): `y` closes the
/// group, and `n`, Enter and Esc close nothing. The toast after `y` is taw's own.
#[test]
fn the_page_draws_no_command_as_a_toast_and_a_machines_group_question_is_strict() {
    let page = read(PAGE);
    for command in [
        "switch-client -t \"",
        "kill-pane -t \"",
        "kill-session -t \"",
        "kill-window -t \"",
        "new-window -t \"",
        "split-window ",
        "rename-window -t \"",
        "rename-session -t ",
        "@agent_name = ",
        "git worktree remove ${force",
        "git worktree remove --force",
        "git worktree add ~",
        "new-session -d -s",
        "; popup closes",
        "pre_close asked about each first",
    ] {
        assert!(
            !page.contains(command),
            "{PAGE} draws a command as a toast: {command:?}"
        );
    }
    let start = page
        .find("function remoteGroup(")
        .unwrap_or_else(|| panic!("{PAGE} has no machine group question"));
    let group = &page[start..start + page[start..].find('\n').unwrap_or(page.len() - start)];
    assert!(
        group.contains("strict: true"),
        "a machine's group question is strict: {group}"
    );
    assert!(
        !group.contains("choice") && !group.contains("onNo"),
        "a machine's group question has no \"just this one\": {group}"
    );
    assert!(
        group.contains("and ${names.length - 3} more"),
        "its names are capped at three: {group}"
    );
    assert!(
        group.contains("closed ${m} \u{b7} ${s.name} and its ${wts.length} worktree session"),
        "its toast is the app's, with the machine named: {group}"
    );
    assert!(
        page.contains("toast(`closed ${s.name} and its ${wts.length} worktree session"),
        "the local group's toast is the app's"
    );
    assert!(
        page.contains("'y confirm  \u{b7}  n,\\u00a0Enter\\u00a0or\\u00a0Esc cancel'"),
        "the strict hint line"
    );
}

/// The page holds no comment in its style and none in its HTML (the ones in its script are
/// found with the build's own tokenizer, by a check kept with the build).
#[test]
fn the_published_page_holds_no_style_or_html_comment() {
    let page = read(PAGE);
    let script_start = page.find("<script>").expect("a script");
    let script_end = page.rfind("</script>").expect("the script's end");
    let style = &page
        [page.find("<style>").expect("a style")..page.find("</style>").expect("the style's end")];
    assert!(!style.contains("/*"), "the page's style holds a comment");
    let html = format!("{}{}", &page[..script_start], &page[script_end..]);
    assert!(!html.contains("<!--"), "the page holds an HTML comment");
}

/// The page says how many agents herdr's rules read, by hand: it is the number of manifests
/// taw bundles, so a manifest added or taken out makes this fail until the page says so.
#[test]
fn the_page_counts_the_agents_taw_bundles() {
    let bundled = std::fs::read_dir(repo("src/detect/manifests"))
        .expect("the bundled manifests")
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "toml"))
        })
        .count();
    assert!(bundled > 20, "only {bundled} manifests found");
    let said = format!("herdr's detection engine reads {bundled} agents");
    assert!(
        read(PAGE).contains(&said),
        "{PAGE} does not say {said:?}; change that sentence in the page"
    );
}

/// The page draws the agents in a container as the popup does, with a profile's name of its own
/// (a real name is not used).
#[test]
fn the_published_page_shows_the_box_on_the_agents_in_a_container() {
    let page = read(PAGE);
    assert!(page.contains("\u{1f4e6}"));
    assert!(page.contains("markLegend()"), "the legend's line");
    assert!(page.contains("profile: 'mine'"));
    assert!(
        page.contains("after an agent's name says it runs in a container"),
        "the page says what it means"
    );
}

/// What a machine's rows say, ask and draw is taw's. A machine's group question is
/// strict in its own function (a strict set, no choice, no way to close the session alone), a
/// machine's worktree removal and agent rename each draw taw's toast, an agent name another
/// agent holds is refused in taw's words on this machine's rows and a machine's, the picker's
/// own error is a line in its box and no toast, and the `w` note names the folder taw makes.
#[test]
fn the_page_draws_what_a_machines_rows_ask_and_say_in_taws_words() {
    let page = read(PAGE);
    let line_of = |start: &str| -> String {
        page.lines()
            .find(|l| l.trim_start().starts_with(start))
            .unwrap_or_else(|| panic!("{PAGE} has no line starting {start:?}"))
            .to_string()
    };
    assert!(
        page.contains("rtoast(`removed worktree ${r.wt.branch} on ${m}`)"),
        "{PAGE} draws no removed-worktree toast for a machine"
    );
    assert!(
        page.contains("rtoast(`renamed agent to ${v}`)"),
        "{PAGE} draws no renamed-agent toast for a machine"
    );
    assert!(
        page.contains("no session numbered ${n} is shown"),
        "{PAGE} lacks taw's words for a number no row has"
    );
    assert!(
        page.contains("o.error = `no such directory: ${target}`"),
        "the picker's error is not a line of its own"
    );
    assert!(
        !page.contains("toast(`no such directory"),
        "the picker's error is a toast"
    );
    assert!(
        page.contains("worktrees/${wtFolder(r.s, v)}"),
        "the w note names a folder taw does not make"
    );
    assert!(
        !page.contains("worktrees/${label}/${v}"),
        "the w note names the old folder"
    );
    assert!(
        page.contains("another agent is already named '${v}'"),
        "{PAGE} does not refuse a name another agent holds"
    );
    assert!(
        !page.contains("lowercase letter first, then a-z 0-9"),
        "a machine's agent name is checked in the old words"
    );
    let rename = line_of("const nameAgent = () => { S.overlay = { type: 'prompt', machine: true");
    assert!(
        rename.contains("agentNameProblem("),
        "a machine's rename does not use the shared name check: {rename}"
    );
}

#[test]
fn every_worktree_folder_the_page_shows_is_in_taws_shape() {
    // taw's folder is <worktrees directory>/<repository folder>--<branch with each / made ->: no
    // `/worktrees/<name>/<branch>` folder is left on the page (a path segment after the first one).
    let page = read("docs/try/index.html");
    let mut rest = page.as_str();
    let mut seen = 0;
    while let Some(at) = rest.find("/worktrees/") {
        rest = &rest[at + "/worktrees/".len()..];
        let folder: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && !"'\"`<${},)".contains(*c))
            .collect();
        assert!(
            !folder.contains('/'),
            "a worktree folder with a slash in it: {folder}"
        );
        seen += 1;
    }
    assert!(
        seen >= 3,
        "the page shows its sample worktree folders: {seen}"
    );
    assert!(
        page.contains("worktrees/mobile--fix-login"),
        "the open worktree session's folder"
    );
    // A folder built from a template stops the scan above at the `$`: the closed worktree's preview title and checkout line are built with wtFolder, and no
    // template of the old shape (`worktrees/${...}/${...}`) is left anywhere on the page.
    assert_eq!(
        page.matches("worktrees/${wtFolder(r.s, r.wt.branch)}")
            .count(),
        2,
        "the closed worktree's preview title and checkout line are built with wtFolder"
    );
    let mut templ = page.as_str();
    while let Some(at) = templ.find("worktrees/${") {
        templ = &templ[at + "worktrees/${".len()..];
        let expr = &templ[..templ.find('}').unwrap_or(templ.len())];
        assert!(
            expr.starts_with("wtFolder("),
            "a worktree folder template that is not built with wtFolder: {expr}"
        );
    }
    assert!(
        page.contains("wtFolder(r.s, r.wt.branch)"),
        "a closed worktree's folder is worked out in taw's shape"
    );
}

/// The page shows the image paste in the real screens' words: the help row and the help's mouse line, the right-click menu of an agent row, and the toast and the refusals
/// `ctrl-v` gives (taw's own constants, so a change of a word there fails here).
#[test]
fn the_page_shows_the_image_paste_in_the_real_screens_words() {
    use taw::tui::overlays::state::{MENU_HELP, PASTE_IMAGE_ENTRY, PASTE_IMAGE_ROW_HELP};
    let page = read(PAGE);
    let has =
        |what: &str, text: &str| assert!(page.contains(text), "{PAGE}: {what} lacks {text:?}");
    has(
        "the help row",
        &format!("K('r   x   ctrl-v', '{PASTE_IMAGE_ROW_HELP}')"),
    );
    has(
        "the help's mouse line",
        &format!("K('right-click', '{MENU_HELP}')"),
    );
    has(
        "the agent row's menu",
        &format!(
            "if (r.kind === 'agent') items.push({{ key: 'v', label: '{PASTE_IMAGE_ENTRY}', act: A.pasteImage }});"
        ),
    );
    has(
        "the refusal on another row",
        &format!("toast('{}')", taw::image::NOT_AN_AGENT),
    );
    has(
        "the refusal for an agent waiting on a question",
        &format!("('{}')", taw::image::WAITING_ON_A_QUESTION),
    );
    // The started words first, as the popup shows them, and the result after a moment.
    has(
        "the started words",
        &format!(
            "else toast(`{}`); setTimeout(() => {{ if (m) rtoast(`",
            taw::tui::pasting::started_words("${pane}")
        ),
    );
    has(
        "the started words of a machine's agent",
        &format!(
            "if (m) rtoast(`{}`)",
            taw::tui::pasting::started_words("${m}:${pane}")
        ),
    );
    has(
        "the toast",
        &format!(
            "else toast(`{}`); draw(); }}, 900)",
            taw::image::pasted_words("${pane}", None)
        ),
    );
    has(
        "the toast of a machine's agent",
        &taw::image::pasted_words("${pane}", Some("${m}")),
    );
    has(
        "ctrl-v reaches it",
        "k === 'v') { A.pasteImage(); return; }",
    );
}

/// The first words say what to do and that nothing is installed, large, before the window.
#[test]
fn the_page_says_first_how_to_use_it_and_that_it_runs_in_the_browser() {
    let page = read(PAGE);
    let start = page
        .find("<h1 class=\"start\"><span class=\"cta\">Try it here: click inside the popup, then press Tab.</span>")
        .expect("the top words");
    let lead = page
        .find("<p class=\"lead\">It runs entirely in your browser. Nothing to install.</p>")
        .expect("the line under them");
    let window = page.find("id=\"termwrap\"").expect("the popup's box");
    assert!(
        start < lead && lead < window,
        "the words come before the popup"
    );
    assert!(
        page.contains("h1.start { font-size: 36px;"),
        "the words are drawn large"
    );
}

/// On a phone the top words say to tap a button below, with the computer's way under them, and
/// the popup opens at 60 columns, the narrowest width the switch offers; the embedded page keeps
/// its own narrow line.
#[test]
fn on_a_phone_the_page_says_to_tap_a_button_and_opens_at_60_columns() {
    let page = read(PAGE);
    let start = page
        .find("<h1 class=\"start\"><span class=\"cta\">Try it here: click inside the popup, then press Tab.</span>")
        .expect("the top words");
    let tap = page
        .find("<h1 class=\"start narrow\">Tap a button below to make something happen.</h1>")
        .expect("the phone's top words");
    let computer = page
        .find("<p class=\"lead narrow\">On a computer, click the popup and press Tab.</p>")
        .expect("the line under them");
    let window = page.find("id=\"termwrap\"").expect("the popup's box");
    assert!(start < tap && tap < computer && computer < window);
    for needle in [
        ".narrow { display: none; }",
        "@media (max-width: 599px) {\n    html:not(.embed) h1.start:not(.narrow) { display: none; }\n    html:not(.embed) .narrow { display: block; }",
        "if (!EMBED && window.matchMedia('(max-width: 599px)').matches) COLS = 60;",
        "<input type=\"radio\" name=\"width\" value=\"60\">",
    ] {
        assert!(page.contains(needle), "the page never has {needle:?}");
    }
}

/// The popup's own `?` help lists every key, so the page lists none: no key table, no key note,
/// no per-key prose. In their place one arrow, drawn as an SVG and not as a character, points at
/// the `?` in the popup's hint line, with the words `Press ? to see every key`, and it is found
/// again after every drawing (so it follows the hint when the width, the status line or the
/// layout moves it) and hidden while the hint is not the plain one.
#[test]
fn the_page_lists_no_keys_and_draws_one_arrow_at_the_help_hint() {
    let page = read(PAGE);
    for gone in [
        "class=\"keys\"",
        "What each key does",
        "<kbd>Tab</kbd>",
        "<kbd>Enter</kbd>",
        "prefix+O</kbd>",
        "Modal keys",
    ] {
        assert!(!page.contains(gone), "{PAGE} still explains keys: {gone}");
    }
    assert!(
        page.contains("<div class=\"qlabel\" id=\"qlabel\">Press ? to see every key</div>"),
        "the words of the arrow"
    );
    assert!(
        page.contains("<svg class=\"qarrow\" id=\"qarrow\"")
            && page.contains("marker-end=\"url(#qhead)\""),
        "the arrow is an SVG with a head"
    );
    assert_eq!(page.matches("class=\"qarrow\"").count(), 1, "one arrow");
    for how in [
        "el.textContent === '?'",
        "new MutationObserver(refit).observe(term, { childList: true })",
        "S.filter === null && S.model !== 'search' && !S.toast && !insideNow()",
    ] {
        assert!(
            page.contains(how),
            "{PAGE}: the arrow does not find the hint: {how}"
        );
    }
}

/// The contract with the homepage (`docs/index.md` holds an iframe on `try/index.html?embed=1`,
/// a file name, so that a copy of the site opened from disk loads it too): in embed mode the
/// page draws no header, footer or link of its own but the top words, the popup, the
/// arrow and the options, in one column in the middle, with no scrolling of its own (it sets
/// the frame's height; the popup has fewer rows), on a transparent background in the docs
/// theme's light or dark, which it gives the frame too (else a browser paints a backdrop
/// behind the frame); under 700 px it says
/// the demo needs a keyboard and links to the full page, which has a short header with the
/// project's name, the repository and the docs.
#[test]
fn the_page_can_be_embedded_in_the_homepage() {
    let page = read(PAGE);
    for part in [
        "new URLSearchParams(window.location.search).get('embed') === '1'",
        "document.documentElement.classList.toggle('embed', EMBED)",
        "const ROWS = 34;",
        "html.embed { background: transparent; }",
        "html.embed .full { display: none; }",
        "frame.style.height = Math.ceil(main.getBoundingClientRect().bottom",
        "new ResizeObserver(fitFrame).observe(main)",
        "if (frame) frame.style.colorScheme = dark ? 'dark' : 'light';",
        "html.embed .term { width: max-content; max-width: 100%; margin: 0 auto; }",
        "data-md-color-scheme",
        "@media (max-width: 699px)",
        "html.embed .demo, html.embed h1.start, html.embed p.lead { display: none; }",
        "The demo needs a keyboard: open it on a computer.",
        "<a class=\"full-link\" href=\"./\" target=\"_top\">",
    ] {
        assert!(page.contains(part), "{PAGE} lacks {part:?}");
    }
    let header = page
        .find("<header class=\"site full\">")
        .expect("the full page's header");
    let header = &page[header..header + page[header..].find("</header>").unwrap()];
    for part in [
        "tmux-agent-wizard",
        "href=\"https://github.com/27medkamal/tmux-agent-wizard\"",
        "href=\"../\">Docs</a>",
    ] {
        assert!(header.contains(part), "the header lacks {part:?}");
    }
    // Everything of the full page's own sits in an element the embed hides: the header and the
    // sections after the options carry the class `full`.
    let sections = page
        .find("<div class=\"full\">")
        .expect("the full page's sections");
    assert!(page[sections..].contains("<h2>Get it</h2>"));
    assert!(
        !page[..sections].contains("<h2>"),
        "a heading outside what the embed hides"
    );
}

#[test]
fn a_machine_header_is_one_full_width_rule_with_a_dot_the_name_and_the_counts() {
    let page = read(PAGE);
    // One drawing function and one rule set: the heavy line, the dot by state, the bold accent
    // name, the machine's own words dim after the name, and no background behind any of it.
    for part in [
        ".mh-rule { color: var(--dim); }",
        ".mh-name { color: var(--accent); font-weight: 700; }",
        ".mh-words { color: var(--dim); }",
        ".mh-ok { color: var(--green); }",
        ".mh-wait { color: var(--yellow); }",
        ".mh-off { color: var(--red); }",
        ".mh-none { color: var(--dim); }",
        "live: ['\u{25cf}', 'mh-ok']",
        "connecting: ['\u{25cf}', 'mh-wait']",
        "dropped: ['\u{25cf}', 'mh-off']",
        "none: ['\u{25cb}', 'mh-none']",
        "span('\u{2501}\u{2501} ', 'mh-rule')",
        "'\u{2501}'.repeat(",
        "dropped: 'not answering, last seen 2m ago'",
        "const machineCount = () => (S.remote ? 1 + MACHINES.length : 1);",
        "span(' taw app', 'c-acc bold') + countParts().map(c => span('  \\u00b7  ', 'c-dim') + span(c, 'c-fg'))",
        "machineHeaderHtml(it.m, sideW - 2, it.counts)",
    ] {
        assert!(page.contains(part), "{PAGE} lacks {part:?}");
    }
    for rule in page.lines().filter(|l| l.contains(".mh-")) {
        assert!(
            !rule.contains("background"),
            "a header rule with a background: {rule}"
        );
    }
    assert!(
        !page.contains("offline, last seen"),
        "the old away words are still there"
    );
    assert!(
        !page.contains("trying again in 30s"),
        "the old header words are still there"
    );
}

/// The sample world fits the keys that reach it: the whole list, across the machines, holds
/// nine sessions at most (1 to 9 reach each), and no session holds more than five agents (a to
/// e). The preview cuts a long line at its right edge and never wraps it, at any width.
#[test]
fn the_sample_fits_the_number_and_letter_keys_and_the_preview_never_wraps() {
    let page = read(PAGE);
    let block = |from: &str, to: &str| -> String {
        let start = page
            .find(from)
            .unwrap_or_else(|| panic!("{PAGE} lacks {from:?}"));
        page[start..start + page[start..].find(to).unwrap()].to_string()
    };
    let local = block("const baseSessions = ", "];");
    let remote = block("const baseRemote = ", "let RSTASH");
    let remote_sessions = &remote[..remote.find("windows: [").unwrap()];
    let sessions: Vec<String> = local
        .lines()
        .chain(remote_sessions.lines())
        .filter_map(|l| l.trim().strip_prefix("{ id: '"))
        .map(|r| r.split('\'').next().unwrap().to_string())
        .collect();
    assert!(
        sessions.len() >= 6,
        "{PAGE}: the sample sessions were not found: {sessions:?}"
    );
    assert!(
        sessions.len() <= 9,
        "{PAGE}: {} sample sessions, more than 1 to 9 reach: {sessions:?}",
        sessions.len()
    );
    let agents = block("const baseAgents = ", "];") + &remote;
    for session in &sessions {
        let n = agents.matches(&format!("session: '{session}'")).count();
        assert!(
            n <= 5,
            "{PAGE}: {n} agents in the session {session}, more than a to e reach"
        );
    }
    assert!(
        page.contains("  S.wrap = false;\n  setRemote(true);"),
        "{PAGE}: the preview still wraps long lines"
    );
    assert!(
        page.contains("<input type=\"checkbox\" id=\"opt-wrap\">"),
        "{PAGE}: the wrap switch is drawn ticked"
    );
}

/// The rows are the ones taw draws by default (a fresh install's look), in columns: what, where, the state, then the
/// title, and a session header's branch and git; the list is as wide as taw's rule says.
#[test]
fn the_rows_are_taws_default_rows_in_columns_with_taws_list_width() {
    let page = read(PAGE);
    for part in [
        // an agent row: the what, the where and the state in columns, then the title
        "kindCell(r.a, sub(r.m, off, r.a.kind.length), nameC) + ' ' + whereCell(wh,",
        "pad(`${r.a.state} ${elapsed(r.a.since)}`, 12)",
        "const WHATW = 10, WHEREW = 11;",
        // a session header: the name, then the branch and the git flags
        "'  ' + r.s.branch + (r.s.git ? ' ' + r.s.git : '')",
        // taw's width rule: the widest row plus 2, 36 to 64
        "const auto = Math.min(64, Math.max(36, ",
    ] {
        assert!(page.contains(part), "{PAGE} lacks {part:?}");
    }
    for calm in ["CALM", "r.s.branch !== 'main'", "S.titles ? 68 : 52"] {
        assert!(!page.contains(calm), "{PAGE} still has {calm:?}");
    }
}

/// The window is wide and short, like a screen: 160 columns by 34 rows. Its font is the largest at which the
/// columns fit the width and the rows fit the viewport's height under the site header (no 12.5 px cap), and
/// the homepage gives it the viewport's width less a margin, at most 1600 px, and scrolls to it under the header.
#[test]
fn the_window_is_160_columns_by_34_rows_sized_to_the_viewport() {
    let page = read(PAGE);
    for part in [
        "let COLS = 160;",
        "const ROWS = 34;",
        "<span id=\"clientw\">160</span>",
        "<input type=\"radio\" name=\"width\" value=\"160\" checked>160 columns</label>",
        "function roomBelow()",
        "view = window.parent.innerHeight;",
        "scrollMarginTop",
        "window.parent.addEventListener('resize', refit)",
        "rowHeight(size) * ROWS > room",
    ] {
        assert!(page.contains(part), "{PAGE} lacks {part:?}");
    }
    for old in [
        "Math.min(12.5",
        "advance() * 12.5",
        "value=\"150\"",
        "COLS = 150",
        "ROWS = EMBED",
    ] {
        assert!(!page.contains(old), "{PAGE} still has {old:?}");
    }
    let css = read("docs/stylesheets/extra.css");
    let rule = css
        .find(".taw-hero-try {")
        .map(|at| &css[at..at + css[at..].find('}').unwrap()])
        .expect("the homepage's try rule");
    for part in [
        "width: min(calc(100vw - 3rem), 1600px)",
        "left: 50%",
        "translateX(-50%)",
        "scroll-margin-top: 3rem",
    ] {
        assert!(rule.contains(part), "the try rule lacks {part:?}: {rule}");
    }
}
