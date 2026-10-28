//! docs/cli.md names every group of `taw --help`, and only those, so the list cannot drift.

mod common;

use std::collections::BTreeSet;

fn groups_of_help() -> BTreeSet<String> {
    let out = std::process::Command::new(common::taw_bin())
        .arg("--help")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("taw --help runs");
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    help.lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_string)
        .collect()
}

fn groups_of_doc() -> BTreeSet<String> {
    let doc = std::fs::read_to_string(common::repo_root().join("docs/cli.md")).unwrap();
    let from = doc
        .find("Groups: ")
        .expect("docs/cli.md has no Groups line");
    let list = &doc[from..];
    let list = &list[..list.find("\n\n").unwrap_or(list.len())];
    list.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

#[test]
fn cli_md_lists_exactly_the_groups_of_taw_help() {
    let help = groups_of_help();
    assert!(
        help.len() > 20,
        "the help parsed to too few groups: {help:?}"
    );
    assert!(help.contains("image"), "the help lost `image`: {help:?}");
    let doc = groups_of_doc();
    let missing: Vec<_> = help.difference(&doc).collect();
    let extra: Vec<_> = doc.difference(&help).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "docs/cli.md misses {missing:?} and names {extra:?} that `taw --help` does not list"
    );
}
