use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{Config, NameMode};
use crate::session::projects::Source;
use crate::tui::actions::Action;
use crate::tui::data::{Snapshot, SpaceRow};
use crate::tui::overlays::state::Response;

use super::{Project, ProjectPicker, Tag, looks_like_path, projects, retag};

/// A temp `home` with `code/{mobile,web}`, `clients/infra`,
/// `oss/api` and `code/api`.
fn home_with_tree() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for dir in [
        "code/mobile",
        "code/web",
        "clients/infra",
        "oss/api",
        "code/api",
    ] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }
    tmp
}

fn config(zoxide: bool, project_dirs: &[PathBuf]) -> Config {
    let mut cfg = Config::default();
    cfg.sessions.name = NameMode::Directory;
    cfg.sessions.zoxide = zoxide;
    cfg.sessions.project_dirs = project_dirs.to_vec();
    cfg
}

/// One session named `api`, running in `~/code/api`.
fn snapshot(home: &Path) -> Snapshot {
    Snapshot {
        spaces: vec![SpaceRow {
            session_id: "$1".to_string(),
            session_name: "api".to_string(),
            session_path: home.join("code/api"),
            ..SpaceRow::default()
        }],
        ..Snapshot::default()
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Types `s` into the picker one key at a time, the way the run loop feeds it.
fn type_str(picker: &mut ProjectPicker, s: &str) {
    for c in s.chars() {
        picker.on_key(key(KeyCode::Char(c)));
    }
}

fn paths(picker: &ProjectPicker) -> Vec<&str> {
    picker.candidates.iter().map(|c| c.path.as_str()).collect()
}

fn tag_of(picker: &ProjectPicker, path: &str) -> Option<Tag> {
    let index = picker
        .candidates
        .iter()
        .position(|c| c.path == path)
        .unwrap_or_else(|| panic!("{path} is not a candidate: {:?}", paths(picker)));
    picker.tag(index)
}

/// The picker starts empty. Starting at `~/` would make `complete_path` own every
/// keystroke: typing a zoxide search term would be typing a path fragment under `~`, which
/// never finds anything.
#[test]
fn the_picker_opens_empty() {
    let picker = ProjectPicker::new(&Config::default(), Path::new("/home/nobody"));
    assert_eq!(picker.input.value(), "");
    assert_eq!(picker.input.cursor(), 0);
    assert!(picker.candidates.is_empty());
    assert_eq!(picker.outcome, None);
}

/// With nothing typed, `refresh` lists the sources in their own order rather than
/// fuzzy-ranking them: zoxide's directories first, in zoxide's own order, then the
/// configured project directories' children.
#[test]
fn an_empty_picker_lists_zoxide_then_project_dirs_in_order() {
    let home = home_with_tree();
    let cfg = config(true, &[home.path().join("oss")]);
    let snap = Snapshot::default();
    // Deliberately in neither path order nor name order: proves zoxide's order survives,
    // rather than getting re-sorted by path, by name or by an incidental score tie-break.
    let zoxide = vec![
        "~/code/web".to_string(),
        "~/clients/infra".to_string(),
        "~/code/mobile".to_string(),
    ];
    let mut picker = ProjectPicker::new(&cfg, home.path());

    picker.refresh(&snap, &cfg, home.path(), &zoxide);

    assert_eq!(
        paths(&picker),
        vec![
            "~/code/web",
            "~/clients/infra",
            "~/code/mobile",
            "~/oss/api"
        ]
    );
}

/// A term that only names a zoxide directory ranks that directory first: with the
/// picker starting empty, `fin` is a search term, not a path fragment under `~/`.
#[test]
fn typing_a_zoxide_fragment_ranks_its_directory_first() {
    let home = home_with_tree();
    let cfg = config(true, &[]);
    let snap = Snapshot::default();
    let zoxide = vec![
        // No `p` anywhere in this one, so `pay` cannot match it at all.
        "~/code/mobile".to_string(),
        "~/clients/infra/payments".to_string(),
    ];
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "pay");
    picker.refresh(&snap, &cfg, home.path(), &zoxide);

    assert_eq!(picker.candidates[0].path, "~/clients/infra/payments");
}

/// Typing something that looks like a path switches to filesystem completion, in front
/// of anything zoxide or a project directory would otherwise contribute.
#[test]
fn typing_a_tilde_path_switches_to_completion() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("alpha")).unwrap();
    fs::create_dir_all(tmp.path().join("beta")).unwrap();
    let cfg = config(false, &[]);
    let snap = Snapshot::default();
    let mut picker = ProjectPicker::new(&cfg, tmp.path());

    type_str(&mut picker, "~/");
    picker.refresh(&snap, &cfg, tmp.path(), &[]);

    assert_eq!(paths(&picker), vec!["~/alpha", "~/beta"]);
}

/// `looks_like_path` is what gates `complete_path`: only an explicit `~`, `/`, `./`,
/// `../`, `.` or `..` prefix means "this is a path", never a bare fragment that merely
/// contains a slash.
#[test]
fn looks_like_path_recognizes_explicit_path_prefixes_only() {
    for yes in ["~", "~/dev", "/etc", "./dev", "../dev", ".", ".."] {
        assert!(looks_like_path(yes), "{yes:?} should look like a path");
    }
    for no in ["code/api", "pay", "", "dev"] {
        assert!(!looks_like_path(no), "{no:?} should not look like a path");
    }
}

/// A plain search term that happens to contain a slash (typed with no path prefix at
/// all) must never be resolved by `complete_path` — that would read the popup process's
/// own working directory, and a coincidental match there would outrank the zoxide entry
/// the user actually typed for (`rank` always puts completion first). This test never
/// touches the process's cwd itself: the point is that `refresh` does not need to.
#[test]
fn a_search_term_with_a_slash_is_never_resolved_as_a_path() {
    let home = home_with_tree();
    let cfg = config(true, &[]);
    let snap = snapshot(home.path());
    let zoxide = vec!["~/code/api".to_string()];
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "code/api");
    picker.refresh(&snap, &cfg, home.path(), &zoxide);

    assert!(
        picker
            .candidates
            .iter()
            .all(|c| c.source != Source::Completion),
        "a plain search term must never resolve through complete_path: {:?}",
        picker.candidates
    );
    assert_eq!(picker.candidates[0].path, "~/code/api");
}

#[test]
fn tab_completes_the_highlighted_directory_and_keeps_completing() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/cod");
    picker.refresh(&snap, &cfg, home.path(), &[]);
    assert_eq!(paths(&picker), vec!["~/code"]);

    picker.on_key(key(KeyCode::Tab));
    assert_eq!(picker.input.value(), "~/code/");

    picker.refresh(&snap, &cfg, home.path(), &[]);
    assert_eq!(
        paths(&picker),
        vec!["~/code/api", "~/code/mobile", "~/code/web"]
    );
}

#[test]
fn candidates_are_tagged_running_zoxide_or_project_dir() {
    let home = home_with_tree();
    let cfg = config(true, &[home.path().join("code")]);
    let snap = snapshot(home.path());
    let zoxide = vec!["~/oss/api".to_string()];
    let mut picker = ProjectPicker::new(&cfg, home.path());

    // Nothing typed: the three sourced tags all come straight from the empty picker's
    // own listing (see `an_empty_picker_lists_zoxide_then_project_dirs_in_order`).
    picker.refresh(&snap, &cfg, home.path(), &zoxide);
    assert_eq!(tag_of(&picker, "~/code/api"), Some(Tag::Running));
    assert_eq!(tag_of(&picker, "~/oss/api"), Some(Tag::Zoxide));
    assert_eq!(tag_of(&picker, "~/code/mobile"), Some(Tag::ProjectDir));

    // A plain completion needs something typed — completion is empty on an empty
    // picker — so this is checked in a second refresh, once `~/code` is on the
    // candidate list on its own account.
    type_str(&mut picker, "~/cod");
    picker.refresh(&snap, &cfg, home.path(), &zoxide);
    assert_eq!(tag_of(&picker, "~/code"), None, "a plain completion");

    assert_eq!(
        [
            Tag::Running.label(),
            Tag::Zoxide.label(),
            Tag::ProjectDir.label()
        ],
        ["running", "zoxide", "project dir"]
    );
}

#[test]
fn the_outcome_proposes_a_name_and_reports_a_collision() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/oss/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome for ~/oss/api");
    assert_eq!(outcome.target, "~/oss/api");
    assert_eq!(outcome.running, None);
    assert_eq!(outcome.name.as_deref(), Some("oss/api"));
    assert!(
        outcome.collision.unwrap().contains("~/code/api"),
        "the collision names the session already holding \"api\""
    );
    assert!(outcome.exists);
}

/// A session of studio, a machine of the list, at `path`.
fn studio_session(name: &str, path: PathBuf) -> SpaceRow {
    SpaceRow {
        session_id: "studio:$1".to_string(),
        session_name: name.to_string(),
        session_path: path,
        machine: Some(0),
        ..SpaceRow::default()
    }
}

/// Only studio runs `api`, at the path of this Mac's folder `~/code/api`: the folder is not running
/// here, and Enter on it opens a session, it does not switch to one of studio's.
#[test]
fn a_remote_session_at_a_local_folders_path_does_not_mark_it_running() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = Snapshot {
        spaces: vec![studio_session("api", home.path().join("code/api"))],
        ..Snapshot::default()
    };
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome for ~/code/api");
    assert_eq!(
        outcome.running, None,
        "studio's session is not running here"
    );
    assert_eq!(outcome.name.as_deref(), Some("api"), "a session is created");
    assert_eq!(outcome.collision, None);
    // The positive half: the same folder with a session of this Mac's is running.
    let local = Snapshot {
        spaces: vec![SpaceRow {
            machine: None,
            ..studio_session("api", home.path().join("code/api"))
        }],
        ..Snapshot::default()
    };
    picker.refresh(&local, &cfg, home.path(), &[]);
    let outcome = picker.outcome.clone().expect("an outcome for ~/code/api");
    assert_eq!(outcome.running.as_deref(), Some("api"));
}

/// Studio has a session `api` elsewhere; this Mac has none: a new session at `~/oss/api` may be
/// called `oss/api`'s own name, `api`, with no collision noted. A session of this Mac's `api` still
/// makes it dodge (the positive half, `the_outcome_proposes_a_name_and_reports_a_collision`).
#[test]
fn a_new_session_may_take_a_name_only_another_machines_session_has() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = Snapshot {
        spaces: vec![studio_session("api", PathBuf::from("/srv/api"))],
        ..Snapshot::default()
    };
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/oss/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome for ~/oss/api");
    assert_eq!(outcome.name.as_deref(), Some("api"));
    assert_eq!(outcome.collision, None, "studio's api is not in the way");

    // A local duplicate is still refused: this Mac's own `api` makes the name dodge.
    picker.refresh(&snapshot(home.path()), &cfg, home.path(), &[]);
    let outcome = picker.outcome.clone().expect("an outcome for ~/oss/api");
    assert_eq!(outcome.name.as_deref(), Some("oss/api"));
    assert!(outcome.collision.is_some());
}

#[test]
fn the_outcome_says_when_a_session_is_already_running_there() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome for ~/code/api");
    assert_eq!(outcome.running.as_deref(), Some("api"));
    assert_eq!(
        outcome.name, None,
        "nothing is created, so nothing is named"
    );
    assert_eq!(outcome.collision, None);
    assert!(outcome.exists);
}

#[test]
fn the_outcome_of_a_path_that_is_not_there_yet_reports_it() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/nope");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome for ~/nope");
    assert_eq!(outcome.target, "~/nope");
    assert!(!outcome.exists);
    assert_eq!(outcome.running, None);
}

#[test]
fn enter_on_a_missing_directory_stays_with_an_error() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/nope");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(picker.on_key(key(KeyCode::Enter)), Response::Stay);
    assert_eq!(picker.error.as_deref(), Some("no such directory: ~/nope"));
}

#[test]
fn enter_on_a_running_project_opens_it() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(
        picker.on_key(key(KeyCode::Enter)),
        Response::Run(Action::OpenProject {
            path: "~/code/api".to_string()
        })
    );
    assert_eq!(picker.error, None);
}

#[test]
fn enter_on_an_existing_directory_without_a_session_opens_it_too() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/oss/api");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(
        picker.on_key(key(KeyCode::Enter)),
        Response::Run(Action::OpenProject {
            path: "~/oss/api".to_string()
        })
    );
}

#[test]
fn enter_acts_on_the_highlighted_candidate_not_the_typed_text() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/cod");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(
        picker.on_key(key(KeyCode::Enter)),
        Response::Run(Action::OpenProject {
            path: "~/code".to_string()
        })
    );
}

#[test]
fn the_highlight_moves_with_the_arrows_and_ctrl_j_k_and_wraps() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    // No zoxide and no project dirs configured, so nothing typed would list nothing to
    // wrap through — `~/` is typed to get three completion candidates instead.
    type_str(&mut picker, "~/");
    picker.refresh(&snap, &cfg, home.path(), &[]);
    assert_eq!(paths(&picker), vec!["~/clients", "~/code", "~/oss"]);

    picker.on_key(key(KeyCode::Down));
    assert_eq!(picker.sel, 1);
    picker.on_key(ctrl('j'));
    assert_eq!(picker.sel, 2);
    picker.on_key(ctrl('j'));
    assert_eq!(picker.sel, 0, "moving past the last wraps to the first");
    picker.on_key(key(KeyCode::Up));
    assert_eq!(picker.sel, 2, "moving before the first wraps to the last");
    picker.on_key(ctrl('k'));
    assert_eq!(picker.sel, 1);
}

#[test]
fn typing_resets_the_highlight_and_clears_the_error() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    // As above: `~/` first, purely to have more than one candidate to move off of.
    type_str(&mut picker, "~/");
    picker.refresh(&snap, &cfg, home.path(), &[]);
    picker.on_key(key(KeyCode::Down));
    assert_eq!(picker.sel, 1);

    picker.error = Some("no such directory: ~/nope".to_string());
    type_str(&mut picker, "d");
    assert_eq!(picker.input.value(), "~/d");
    assert_eq!(picker.sel, 0);
    assert_eq!(picker.error, None);
}

#[test]
fn the_list_never_grows_past_ten_candidates() {
    let home = home_with_tree();
    for n in 0..14 {
        fs::create_dir_all(home.path().join(format!("many/p{n:02}"))).unwrap();
    }
    let cfg = config(false, &[]);
    let snap = snapshot(home.path());
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/many/");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(picker.candidates.len(), 10);
    assert_eq!(picker.candidates[0].path, "~/many/p00");
}

#[test]
fn a_running_session_is_matched_by_path_not_by_name() {
    let home = home_with_tree();
    let cfg = config(false, &[]);
    // A session called `api` whose directory is `~/oss/api`: the one running in
    // `~/code/api` must not be mistaken for it.
    let mut snap = snapshot(home.path());
    snap.spaces.push(SpaceRow {
        session_id: "$2".to_string(),
        session_name: "other".to_string(),
        session_path: home.path().join("code/mobile"),
        ..SpaceRow::default()
    });
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/mobile");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    let outcome = picker.outcome.clone().expect("an outcome");
    assert_eq!(outcome.running.as_deref(), Some("other"));
}

// ------------------------------------------------- the list the / filter shows too

/// A home with the picker's usual tree, three project directories whose children are
/// projects, and a zoxide history of twelve directories on top: seventeen candidates for an
/// empty picker, more than either cap (ten in the picker, eight in the filter).
fn crowded() -> (tempfile::TempDir, Config, Vec<String>) {
    let home = home_with_tree();
    let cfg = config(
        true,
        &[
            home.path().join("code"),
            home.path().join("clients"),
            home.path().join("oss"),
        ],
    );
    let zoxide: Vec<String> = (0..12).map(|n| format!("~/z/p{n:02}")).collect();
    (home, cfg, zoxide)
}

/// What the picker lists for `typed`, each candidate with its tag.
fn pickers_list(
    home: &Path,
    cfg: &Config,
    snap: &Snapshot,
    zoxide: &[String],
    typed: &str,
) -> Vec<(String, Option<Tag>)> {
    let mut picker = ProjectPicker::new(cfg, home);
    type_str(&mut picker, typed);
    picker.refresh(snap, cfg, home, zoxide);
    picker
        .candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.path.clone(), picker.tag(index)))
        .collect()
}

fn listed(list: &[Project]) -> Vec<(String, Option<Tag>)> {
    list.iter().map(|p| (p.path.clone(), p.tag)).collect()
}

/// The filter's directories are exactly what the picker
/// would list for the same query, tags included, only fewer of them. Every kind of query the
/// picker knows: nothing, a fragment, a path, fzf's exact and negated terms, a fragment with
/// a slash in it, and one nothing matches.
#[test]
fn the_filters_directories_are_the_pickers_list_for_the_same_query_cut_to_the_cap() {
    let (home, cfg, zoxide) = crowded();
    let snap = snapshot(home.path());
    for typed in [
        "", "p0", "api", "we", "!p0", "'p1", "~/cod", "code/api", "zzz",
    ] {
        let pickers = pickers_list(home.path(), &cfg, &snap, &zoxide, typed);
        // A list nothing was put in would agree with anything.
        assert_eq!(pickers.is_empty(), typed == "zzz", "{typed:?}: {pickers:?}");
        for limit in [8, 3, 10] {
            let filters = projects(typed, &snap, &cfg, home.path(), &zoxide, limit);
            assert_eq!(
                listed(&filters),
                pickers.iter().take(limit).cloned().collect::<Vec<_>>(),
                "{typed:?} capped at {limit}"
            );
        }
    }
}

/// With nothing typed, the picker lists its sources in their own order (zoxide's, then the
/// project directories'); the filter's list right after `/` is the first eight of that.
#[test]
fn nothing_typed_lists_zoxide_first_and_stops_at_the_limit() {
    let (home, cfg, zoxide) = crowded();
    let snap = Snapshot::default();

    let list = projects("", &snap, &cfg, home.path(), &zoxide, 8);

    assert_eq!(
        list.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        (0..8).map(|n| format!("~/z/p{n:02}")).collect::<Vec<_>>()
    );
}

/// The tags are the picker's: a directory a session is running in says so, whichever
/// source listed it, and the others say where they came from.
#[test]
fn a_listed_directory_is_tagged_running_zoxide_or_project_dir() {
    let home = home_with_tree();
    let cfg = config(true, &[home.path().join("code")]);
    let snap = snapshot(home.path());
    let zoxide = vec!["~/oss/api".to_string()];

    let list = projects("", &snap, &cfg, home.path(), &zoxide, 8);

    let tag = |path: &str| {
        list.iter()
            .find(|p| p.path == path)
            .unwrap_or_else(|| panic!("{path} is not listed: {list:?}"))
            .tag
    };
    assert_eq!(tag("~/code/api"), Some(Tag::Running));
    assert_eq!(tag("~/oss/api"), Some(Tag::Zoxide));
    assert_eq!(tag("~/code/mobile"), Some(Tag::ProjectDir));
}

/// A session appearing where a directory is listed changes its tag without listing anything
/// again: the popup's tick hands the new snapshot over twice a second, and the filesystem
/// is read for a keystroke, never for a tick. The directories are gone from the disk by the
/// time the tag is told, and every one is still listed, in the same order.
#[test]
fn retagging_follows_the_sessions_and_never_lists_again() {
    let home = home_with_tree();
    let cfg = config(false, &[home.path().join("code")]);
    let mut list = projects("", &Snapshot::default(), &cfg, home.path(), &[], 8);
    let paths: Vec<String> = list.iter().map(|p| p.path.clone()).collect();
    assert_eq!(paths, ["~/code/api", "~/code/mobile", "~/code/web"]);
    assert!(list.iter().all(|p| p.tag == Some(Tag::ProjectDir)));

    fs::remove_dir_all(home.path().join("code")).unwrap();
    let mut snap = Snapshot::default();
    snap.spaces.push(SpaceRow {
        session_id: "$9".to_string(),
        session_name: "web".to_string(),
        session_path: home.path().join("code/web"),
        ..SpaceRow::default()
    });
    retag(&mut list, &snap, &cfg, home.path(), &[]);

    assert_eq!(
        list.iter().map(|p| p.path.clone()).collect::<Vec<_>>(),
        paths,
        "the same directories, though none is on the disk any more"
    );
    assert_eq!(list[1].tag, Some(Tag::ProjectDir));
    assert_eq!(list[2].tag, Some(Tag::Running));

    // And back: the session closes.
    retag(&mut list, &Snapshot::default(), &cfg, home.path(), &[]);
    assert_eq!(list[2].tag, Some(Tag::ProjectDir));
}

/// `[sessions] zoxide = false` keeps zoxide's directories out of the filter's list exactly
/// as it does the picker's, whatever the popup was handed.
#[test]
fn zoxide_switched_off_lists_none_of_its_directories() {
    let home = home_with_tree();
    let cfg = config(false, &[home.path().join("oss")]);
    let zoxide = vec!["~/code/api".to_string()];

    let list = projects("", &Snapshot::default(), &cfg, home.path(), &zoxide, 8);

    assert_eq!(
        list.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        ["~/oss/api"]
    );
}

/// A home with two repositories under `~/code`, each with the `.git` folder git makes, and
/// `docs` with a `src` folder as well. A session named `docs` runs in `~/code/docs`.
fn home_with_repositories() -> (tempfile::TempDir, Config, Snapshot) {
    let home = tempfile::tempdir().unwrap();
    for dir in ["code/docs/.git/objects", "code/docs/src", "code/web/.git"] {
        fs::create_dir_all(home.path().join(dir)).unwrap();
    }
    let cfg = config(false, &[home.path().join("code")]);
    let snap = Snapshot {
        spaces: vec![SpaceRow {
            session_id: "$1".to_string(),
            session_name: "docs".to_string(),
            session_path: home.path().join("code/docs"),
            ..SpaceRow::default()
        }],
        ..Snapshot::default()
    };
    (home, cfg, snap)
}

/// The steps of the fresh-machine test: type `~/code/do`, press Tab. The folder offered is
/// the repository, never its `.git`, and the last line says what Enter does for the running
/// session there.
#[test]
fn tab_after_a_prefix_of_a_running_repository_offers_it_not_its_git_folder() {
    let (home, cfg, snap) = home_with_repositories();
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/do");
    picker.refresh(&snap, &cfg, home.path(), &[]);
    assert_eq!(paths(&picker), vec!["~/code/docs"]);

    picker.on_key(key(KeyCode::Tab));
    assert_eq!(picker.input.value(), "~/code/docs/");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(paths(&picker), vec!["~/code/docs/src"], "no .git folder");
    let at_the_folder = picker.outcome_for("~/code/docs");
    assert_eq!(at_the_folder.running.as_deref(), Some("docs"));
}

/// The exact folder of a running session, typed with no slash after it, comes first, ahead
/// of the folders inside it, and Enter switches to that session.
#[test]
fn a_typed_folder_of_a_running_session_is_offered_first_and_enter_switches() {
    let (home, cfg, snap) = home_with_repositories();
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/docs");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(paths(&picker), vec!["~/code/docs", "~/code/docs/src"]);
    assert_eq!(picker.tag(0), Some(Tag::Running));
    let outcome = picker.outcome.clone().expect("an outcome");
    assert_eq!(outcome.target, "~/code/docs");
    assert_eq!(outcome.running.as_deref(), Some("docs"));
    assert_eq!(
        picker.on_key(key(KeyCode::Enter)),
        Response::Run(Action::OpenProject {
            path: "~/code/docs".to_string()
        })
    );
}

/// A session's folder that no source lists (not in zoxide, not under a project directory)
/// is first all the same once it is typed in full.
#[test]
fn a_typed_folder_of_a_running_session_no_source_lists_is_first_too() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join("elsewhere/site/assets")).unwrap();
    let cfg = config(false, &[]);
    let snap = Snapshot {
        spaces: vec![SpaceRow {
            session_id: "$1".to_string(),
            session_name: "site".to_string(),
            session_path: home.path().join("elsewhere/site"),
            ..SpaceRow::default()
        }],
        ..Snapshot::default()
    };
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/elsewhere/site");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(
        paths(&picker),
        vec!["~/elsewhere/site", "~/elsewhere/site/assets"]
    );
    assert_eq!(picker.tag(0), Some(Tag::Running));
}

/// With a dot typed the `.git` folder is listed, as a shell lists it, but it is no project:
/// nothing inside a `.git` folder is tagged `project dir`.
#[test]
fn nothing_inside_a_git_folder_is_a_project_dir() {
    let (home, cfg, snap) = home_with_repositories();
    let mut picker = ProjectPicker::new(&cfg, home.path());

    type_str(&mut picker, "~/code/web/.");
    picker.refresh(&snap, &cfg, home.path(), &[]);

    assert_eq!(paths(&picker), vec!["~/code/web/.git"]);
    assert_eq!(picker.tag(0), None);
    assert_eq!(
        super::tag_for(
            "~/code/web/.git/hooks",
            &[],
            &[],
            &cfg.sessions.project_dirs,
            home.path()
        ),
        None
    );
    assert_eq!(
        super::tag_for(
            "~/code/web",
            &[],
            &[],
            &cfg.sessions.project_dirs,
            home.path()
        ),
        Some(Tag::ProjectDir)
    );
}

/// The `/` filter lists the picker's list for the same text, so it too offers no `.git`.
#[test]
fn the_filter_lists_no_git_folder_either() {
    let (home, cfg, snap) = home_with_repositories();
    let list = projects("~/code/docs", &snap, &cfg, home.path(), &[], 8);
    let listed: Vec<&str> = list.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(listed, vec!["~/code/docs", "~/code/docs/src"]);
}
