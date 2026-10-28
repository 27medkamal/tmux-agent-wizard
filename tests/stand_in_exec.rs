//! A stand-in a test runs is made executable through `make_executable`, never by a plain chmod.
//!
//! Linux refuses to run a file that any process holds open for writing ("Text file busy"), and a
//! child forked by another test thread while a test still had its stand-in open for writing holds
//! such a writer until it execs. `make_executable` (`src/test_exec.rs` for the library's tests,
//! `tests/common/exec.rs` for these) gives the path a new inode that no thread of the test binary
//! ever held open, so a stand-in runs at once. This test reads every source file under `src/` and
//! `tests/` and counts the places that set a mode with an execute bit (or a mode held in a
//! variable) by any other means; each file's count must be the one [`ALLOWED`] gives it, with its
//! reason: folders, modes a test checks or restores, and product code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The files that may set an execute mode other than through `make_executable`, how many times,
/// and why. A new stand-in goes through `make_executable`; a new folder mode raises its count.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "src/crash.rs",
        3,
        "its private folder (product), a report's own mode and a wider folder a test sets",
    ),
    ("src/hooks/mod.rs", 1, "a locked folder put back"),
    (
        "src/image.rs",
        3,
        "its private folder (product, twice), a folder a test links to",
    ),
    (
        "src/integration/mod.rs",
        5,
        "a config file's mode and write_executable (product, a hook script an agent runs \
         later), a test's folder modes",
    ),
    (
        "src/machine/ssh.rs",
        9,
        "the private control folder (product, four), the folder modes the tests check",
    ),
    (
        "src/plugin/update.rs",
        2,
        "a folder a test makes read-only and puts back",
    ),
    (
        "src/sandbox/profile.rs",
        2,
        "taw's own profile and a new one of yours (product, run later from a pane)",
    ),
    (
        "src/sandbox/report.rs",
        2,
        "a private folder (product), a folder a test links to",
    ),
    (
        "src/session/restore.rs",
        2,
        "a test's folder mode helper and its guard",
    ),
    (
        "tests/e2e_sandbox.rs",
        2,
        "a profile its owner may not run, which the test expects refused, and a folder",
    ),
    ("tests/hooks_run.rs", 2, "a locked folder put back"),
    (
        "tests/sandbox_profile.rs",
        4,
        "read-only folders and their guards",
    ),
    (
        "tests/tmux_session.rs",
        2,
        "a test's folder mode helper and its guard",
    ),
    (
        "tests/tmux_tui_worktrees.rs",
        2,
        "a read-only folder and its guard",
    ),
    (
        "tests/tmux_worktree.rs",
        2,
        "a read-only folder and its guard",
    ),
];

/// The helpers themselves, and this file, whose scan and its test name the calls.
const SKIPPED: &[&str] = &[
    "src/test_exec.rs",
    "tests/common/exec.rs",
    "tests/stand_in_exec.rs",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Whether the mode argument `arg` (the last argument of a mode call) has an execute bit, or is
/// not a literal at all and so may have one.
fn may_be_executable(arg: &str) -> bool {
    match arg.strip_prefix("0o") {
        Some(digits) => !matches!(u32::from_str_radix(digits, 8), Ok(mode) if mode & 0o111 == 0),
        None => !arg.is_empty(),
    }
}

/// The places in `text` that set an execute mode, as `line: text`.
fn exec_modes(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for call in ["from_mode(", "set_mode(", ".mode("] {
            for (at, _) in line.match_indices(call) {
                if line[..at].trim_end().ends_with("fn") {
                    continue;
                }
                let rest = &line[at + call.len()..];
                let Some(end) = rest.find(')') else {
                    continue;
                };
                let arg = rest[..end].rsplit(',').next().unwrap_or("").trim();
                if may_be_executable(arg) {
                    found.push(format!("{}: {}", number + 1, line.trim()));
                }
            }
        }
    }
    found
}

#[test]
fn a_stand_in_is_made_executable_only_through_make_executable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("tests"), &mut files);
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let rel = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if SKIPPED.contains(&rel.as_str()) {
            continue;
        }
        let places = exec_modes(&std::fs::read_to_string(&file).unwrap());
        if !places.is_empty() {
            seen.insert(rel, places);
        }
    }
    let allowed: BTreeMap<&str, usize> = ALLOWED.iter().map(|(f, n, _)| (*f, *n)).collect();
    let mut wrong = Vec::new();
    for (file, places) in &seen {
        let want = allowed.get(file.as_str()).copied().unwrap_or(0);
        if places.len() != want {
            wrong.push(format!(
                "{file}: {} places, {want} allowed (a stand-in a test runs goes through \
                 make_executable):\n  {}",
                places.len(),
                places.join("\n  ")
            ));
        }
    }
    for (file, want) in &allowed {
        if !seen.contains_key(*file) {
            wrong.push(format!(
                "{file}: 0 places, {want} allowed: lower its count in ALLOWED"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The scan itself: what it counts and what it lets pass.
#[test]
fn the_scan_counts_execute_modes_and_passes_the_rest() {
    let text = "\
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();\n\
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();\n\
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o700);\n\
        set_mode(dir, 0o000);\n\
        fn set_mode(path: &Path, mode: u32) {\n\
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();\n\
        let mode = meta.permissions().mode() & 0o777;\n\
        // std::fs::Permissions::from_mode(0o755)\n\
        std::fs::OpenOptions::new().mode(0o600);\n\
        std::fs::DirBuilder::new().mode(0o711);\n";
    let lines: Vec<String> = exec_modes(text)
        .iter()
        .map(|place| place.split(':').next().unwrap().to_string())
        .collect();
    assert_eq!(lines, ["1", "3", "6", "10"]);
}
