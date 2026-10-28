//! The wording rule for the files a person reads outside the popup: no
//! em-dash, en-dash, arrow or curly quote in them. They read as machine-written, and a colon, a
//! comma, brackets or a new sentence says the same thing; a range is a hyphen (`1-9`). The
//! popup's `·` and `…` and the key glyphs a table draws (`↑`, `↓`) are not prose and stay.
//!
//! The rule is kept in several places, each where its text is made:
//!
//! * the popup's strings, and every string in taw's source (a message, an error, a toast),
//!   in `src/tui/app/wording.rs`;
//! * the help of every command, hidden ones included, in `src/cli/mod.rs`;
//! * the README's hooks, the documents' hooks and what the code prints about them, in
//!   `tests/hooks_docs.rs`;
//! * and here, every other file a person is sent to read: every markdown and TOML file at the
//!   repository root (the README, the skill `taw skill` prints, the licence notice, and the
//!   manifests of the crate and of mise), the documents (the try-it page `docs/try/index.html`
//!   included, its script too), the examples, the default config `taw config default` prints,
//!   the plugin script, the scripts, the end-to-end README, and
//!   the hook scripts and plugins taw installs into an agent's own config directory
//!   (`src/integration/templates`), and what is under `.github` (the issue templates a person
//!   fills in, and the release workflow). Each is scanned whole, comments included, because a
//!   person reads a script's comments and a config's too.
//!
//! Left out are the README's two pictures in `docs/assets`, which are images, not text. A path
//! named in `LEFT_OUT` below is left out, whether it is a folder, a file, or a developer's own
//! note at the root. The walk also skips what a Finder or an interpreter leaves beside real
//! files (a `.DS_Store`, a `__pycache__` folder, a dot-named file that is not UTF-8), and walks a
//! hidden folder of text like any other. The detection manifests are not scanned either: their
//! rules match what the agents draw. A template is a port of herdr's, and where
//! `tests/vendored_herdr.rs` holds a port to herdr's own bytes, a mark in herdr's text cannot be
//! reworded. None holds one now. If a refresh of the vendored files brings one, add the file and
//! the line, exactly as it is, to `HERDR_OWN` below, which the scan reads, instead of changing
//! herdr's words.

use std::path::{Path, PathBuf};

/// What may not appear. Spelled as escapes, so this file passes its own scan.
const BANNED: [(char, &str); 11] = [
    ('\u{2014}', "an em-dash"),
    ('\u{2013}', "an en-dash"),
    ('\u{2192}', "an arrow"),
    ('\u{2018}', "a curly single quote"),
    ('\u{2019}', "a curly single quote"),
    ('\u{201c}', "a curly double quote"),
    ('\u{201d}', "a curly double quote"),
    // Look-alikes for the same things: a horizontal bar and a minus sign where a dash would
    // go, a double arrow and a heavy round-tipped one.
    ('\u{2015}', "a horizontal bar"),
    ('\u{2212}', "a minus sign"),
    ('\u{21d2}', "a double arrow"),
    ('\u{279c}', "an arrow"),
];

/// The files scanned whole besides every markdown and TOML file at the repository root, as
/// paths from it: the default config, the plugin script and the end-to-end README.
const FILES: [&str; 3] = [
    "src/config_default.toml",
    "tmux-agent-wizard.tmux",
    "tests/e2e/README.md",
];

/// The directories scanned whole, every file in them and in the directories below them.
const DIRS: [&str; 5] = [
    ".github",
    "docs",
    "examples",
    "scripts",
    "src/integration/templates",
];

/// What is left out, as paths from the repository root: a folder or one file, in a scanned
/// directory or at the root.
const LEFT_OUT: [&str; 6] = [
    // The README's animation and stills and the card a link to the repository shows: pictures.
    // What they draw is the popup, whose words `src/tui/app/wording.rs` holds, and the demo's own screens
    // and card, which are under `scripts`.
    // The popup and status-line screenshots are also on the docs site's landing page.
    "docs/assets/demo.gif",
    "docs/assets/popup.png",
    "docs/assets/toast.png",
    "docs/assets/social-preview.png",
    // The landing page's two stills: the project picker, a worktree reopening into its session.
    // Pictures of the popup, made by scripts/demo/stills.tape.
    "docs/assets/picker.png",
    "docs/assets/worktree.png",
];

/// Lines that are herdr's own words, in a port that `tests/vendored_herdr.rs` holds to herdr's
/// bytes, and so cannot be reworded: the file from the repository root, and the line exactly as
/// it is. None today. The scan reads this list, and fails when an entry names a file it does not
/// reach or a line the file no longer holds.
const HERDR_OWN: &[(&str, &str)] = &[];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Whether the file at `path` is UTF-8 text.
fn is_text(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| std::str::from_utf8(&bytes).is_ok())
}

/// The markdown and TOML files at the repository root, as paths from it, in order, but those
/// named in `left_out`: the README, the skill, the licence notice, the manifests of the crate
/// and of mise, and any that is added. A name that starts with a dot is a tool's own file on
/// someone's machine (a local `.mise.local.toml`, say) and is not taken.
fn root_files(root: &Path, left_out: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("the repository root: {e}"))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.is_file())
        .filter_map(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .filter(|name| !name.starts_with('.'))
        .filter(|name| name.ends_with(".md") || name.ends_with(".toml"))
        .filter(|name| !left_out.contains(&name.as_str()))
        .collect();
    found.sort();
    found
}

/// Every file under `dir` (a path from `root`), as paths from `root` in order, but those that
/// are left out, and what a Finder or an interpreter leaves beside real files: a `.DS_Store`, a
/// `__pycache__` folder, and a file with a dot in front of its name that is not UTF-8 (a swap
/// file, say). They are bytes, not words. A hidden folder of text is walked like any other, and
/// a file that is not UTF-8 and is none of those stops the scan.
fn files_under(root: &Path, dir: &str, left_out: &[&str], out: &mut Vec<String>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root.join(dir))
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let relative = path
            .strip_prefix(root)
            .expect("a path under the root")
            .to_string_lossy()
            .replace('\\', "/");
        if left_out.contains(&relative.as_str()) {
            continue;
        }
        if path.is_dir() {
            if name != "__pycache__" {
                files_under(root, &relative, left_out, out);
            }
        } else if name != ".DS_Store" && (!name.starts_with('.') || is_text(&path)) {
            out.push(relative);
        }
    }
}

/// Every file the rule holds for, as paths from `root`, in order and once each, but those
/// named in `left_out`.
fn scanned(root: &Path, left_out: &[&str]) -> Vec<String> {
    let mut files = root_files(root, left_out);
    files.extend(
        FILES
            .iter()
            .filter(|file| !left_out.contains(*file))
            .map(|file| file.to_string()),
    );
    for dir in DIRS {
        files_under(root, dir, left_out, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

/// The lines of `text`, the file `file`, that hold a banned character and are not one of `own`:
/// the line's number (from 1), what the character is, and the words around it.
fn banned_in(file: &str, text: &str, own: &[(&str, &str)]) -> Vec<(usize, &'static str, String)> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if own.iter().any(|(f, l)| *f == file && *l == line) {
            continue;
        }
        for (c, what) in BANNED {
            if let Some(at) = line.find(c) {
                let from = line[..at].chars().count().saturating_sub(40);
                let around: String = line.chars().skip(from).take(90).collect();
                found.push((number + 1, what, around));
            }
        }
    }
    found
}

/// What the rule forbids in `files` (paths from `root`): a line for each mark, `path:line:
/// what: ...the words around it...`. A file that is not UTF-8 text stops the scan.
fn findings(root: &Path, files: &[String], own: &[(&str, &str)]) -> Vec<String> {
    let mut broken = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| {
            panic!(
                "{file} is not UTF-8 text ({e}), and the wording rule is checked on text: a file \
                 that is no one's words belongs in LEFT_OUT in tests/user_text.rs"
            )
        });
        for (number, what, around) in banned_in(file, &text, own) {
            broken.push(format!("{file}:{number}: {what}: ...{around}..."));
        }
    }
    broken
}

#[test]
fn no_file_a_person_reads_holds_a_dash_an_arrow_or_a_curly_quote() {
    let root = repo();
    let files = scanned(&root, &LEFT_OUT);

    // A scan that saw little guards little: these are the kinds of place the rule is for, so
    // each must have been reached and have something in it, and nothing left out may have been
    // reached.
    assert!(files.len() > 12, "the scan reached {files:?}");
    for kind in [
        "README.md",
        "SKILL.md",
        "THIRD_PARTY_LICENSES.md",
        "Cargo.toml",
        "mise.toml",
        "docs/PARITY.md",
        "docs/COMPAT.md",
        "docs/ARCHITECTURE.md",
        "docs/try/index.html",
        "examples/tmux.conf",
        "examples/status-line.md",
        "src/config_default.toml",
        "tmux-agent-wizard.tmux",
        "tests/e2e/README.md",
        "scripts/check.sh",
        "src/integration/templates/taw-agent-state-claude.sh",
        "src/integration/templates/taw-agent-state-omp.ts",
        ".github/ISSUE_TEMPLATE/bug.md",
        ".github/ISSUE_TEMPLATE/detection.md",
        ".github/workflows/release.yml",
    ] {
        assert!(files.iter().any(|f| f == kind), "{kind} was not scanned");
        let text =
            std::fs::read_to_string(root.join(kind)).unwrap_or_else(|e| panic!("{kind}: {e}"));
        assert!(!text.is_empty(), "{kind} was read whole");
    }
    assert!(
        !files.iter().any(|f| LEFT_OUT
            .iter()
            .any(|out| f == out || f.starts_with(&format!("{out}/")))),
        "what is left out was scanned: {files:?}"
    );

    let broken = findings(&root, &files, HERDR_OWN);
    assert!(
        broken.is_empty(),
        "what a person reads says what it may not:\n{}",
        broken.join("\n")
    );

    // An exception names a file the scan reaches and a line that file still holds, so that one
    // cannot outlive the words it was made for.
    for &(file, line) in HERDR_OWN {
        assert!(
            files.iter().any(|f| f == file),
            "HERDR_OWN names {file}, which the scan does not reach"
        );
        let text =
            std::fs::read_to_string(root.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(
            text.lines().any(|l| l == line),
            "{file} no longer holds the line HERDR_OWN excuses: {line}"
        );
    }
}

/// The scan reads everything under `.github`, `docs`, `examples`, `scripts` and the integration
/// templates but what is left out, at any depth, so a file added there later is held to the
/// rule without anyone listing it, a hidden file or folder of text included. A Finder's
/// `.DS_Store`, a `__pycache__` and a dot-named file that is not UTF-8 are not read, and are no
/// part of what is left out: they are bytes, not words.
#[test]
fn a_file_added_under_a_scanned_directory_is_scanned_and_only_the_left_out_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for path in [
        "docs/a.md",
        "docs/deeper/b.md",
        "docs/assets/demo.gif",
        "docs/try/index.html",
        "docs/try/other.html",
        // Words with a dot in front of their name are words all the same.
        "docs/.note",
        "docs/.hidden/z.md",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "text\n").unwrap();
    }
    // What a Finder or an interpreter leaves beside real files is no one's words: a `.DS_Store`,
    // a swap file and a compiled file in a `__pycache__` are bytes that are not UTF-8.
    for path in [
        "docs/.DS_Store",
        "docs/deeper/.DS_Store",
        "docs/.swapfile",
        "docs/__pycache__/x.pyc",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, [0xff, 0xfe, 0x00, 0x42]).unwrap();
    }
    let mut found = Vec::new();
    files_under(root, "docs", &LEFT_OUT, &mut found);
    assert_eq!(
        found,
        [
            "docs/.hidden/z.md",
            "docs/.note",
            "docs/a.md",
            "docs/deeper/b.md",
            "docs/try/index.html",
            "docs/try/other.html",
        ]
    );
}

/// A dash in a file in a hidden folder is reported: the walk skips a Finder's `.DS_Store` and
/// what is not text, and nothing else, so a folder like `scripts/.hooks` is not a way round the
/// rule.
#[test]
fn a_dash_in_a_hidden_folder_of_text_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let hooks = root.join("scripts/.hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("x.sh"),
        "#!/bin/sh\n# a note \u{2014} here\necho ok\n",
    )
    .unwrap();
    std::fs::write(root.join("scripts/.DS_Store"), [0xff, 0xfe, 0x00, 0x42]).unwrap();
    let mut files = Vec::new();
    files_under(root, "scripts", &LEFT_OUT, &mut files);
    assert_eq!(files, ["scripts/.hooks/x.sh"]);
    let found = findings(root, &files, &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with("scripts/.hooks/x.sh:2: an em-dash"),
        "{found:?}"
    );
}

/// Every markdown and TOML file at the root is scanned, a new one included, and a file of
/// another kind, a hidden one, and what is in a folder are not taken for one of them.
#[test]
fn every_markdown_and_toml_file_at_the_root_is_scanned_and_nothing_else_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in [
        "README.md",
        "Cargo.toml",
        "NEW.md",
        "notes.txt",
        "LICENSE",
        "Cargo.lock",
        ".hidden.md",
    ] {
        std::fs::write(root.join(name), "text\n").unwrap();
    }
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/inner.md"), "text\n").unwrap();
    assert_eq!(root_files(root, &[]), ["Cargo.toml", "NEW.md", "README.md"]);
}

/// A markdown or TOML file at the root that is no one's to read, a developer's own note, is left
/// out by naming it in `LEFT_OUT`, as a folder or a file under `docs` is.
#[test]
fn a_root_file_named_in_left_out_is_not_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for path in [
        "README.md",
        "NOTES.md",
        ".github/ISSUE_TEMPLATE/i.md",
        "docs/a.md",
        "examples/e.md",
        "scripts/s.sh",
        "src/integration/templates/t.sh",
    ] {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "text\n").unwrap();
    }
    let all = scanned(root, &[]);
    assert!(all.iter().any(|f| f == "NOTES.md"), "{all:?}");
    let some = scanned(root, &["NOTES.md"]);
    assert!(!some.iter().any(|f| f == "NOTES.md"), "{some:?}");
    assert!(some.iter().any(|f| f == "README.md"), "{some:?}");
    assert!(some.iter().any(|f| f == "docs/a.md"), "{some:?}");
    assert!(
        some.iter().any(|f| f == ".github/ISSUE_TEMPLATE/i.md"),
        "{some:?}"
    );
}

/// A line of herdr's own words that is named is excused in its own file and only as it is:
/// another file, or the line changed, is found as any other line is.
#[test]
fn a_line_that_is_herdrs_own_is_excused_in_its_own_file_and_only_as_it_is() {
    let file = "src/integration/templates/x.sh";
    let line = "herdr says \u{2014} this";
    let own = [(file, line)];
    let text = format!("fine\n{line}\nmore\n");
    assert!(banned_in(file, &text, &own).is_empty());
    assert_eq!(
        banned_in("src/integration/templates/y.sh", &text, &own).len(),
        1
    );
    let changed = format!("fine\n{line} and more\n");
    assert_eq!(banned_in(file, &changed, &own).len(), 1);
    assert_eq!(banned_in(file, &text, &[]).len(), 1);
}

/// Each mark is found on its own line, once, and what is not prose is not found: the popup's
/// separator and ellipsis, the key glyphs a table draws, and a plain hyphen in a range.
#[test]
fn each_mark_is_found_and_the_glyphs_that_are_not_prose_are_not() {
    let text = format!(
        "fine\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n1-9 and a-e, `{}`, {} {} {} {}\n",
        "an \u{2014} here",
        "an \u{2013} here",
        "an \u{2192} here",
        "a \u{2018} here",
        "a \u{2019} here",
        "a \u{201c} here",
        "a \u{201d} here",
        "a \u{2015} here",
        "a \u{2212} here",
        "a \u{21d2} here",
        "a \u{279c} here",
        "\u{2191}\u{2193}",
        "\u{b7}",
        "\u{2026}",
        "\u{25ba}",
        "\u{2265}",
    );
    let found: Vec<(usize, &str)> = banned_in("x.md", &text, &[])
        .into_iter()
        .map(|(number, what, _)| (number, what))
        .collect();
    assert_eq!(
        found,
        [
            (2, "an em-dash"),
            (3, "an en-dash"),
            (4, "an arrow"),
            (5, "a curly single quote"),
            (6, "a curly single quote"),
            (7, "a curly double quote"),
            (8, "a curly double quote"),
            (9, "a horizontal bar"),
            (10, "a minus sign"),
            (11, "a double arrow"),
            (12, "an arrow"),
        ]
    );
}
