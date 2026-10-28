//! Row building against the default sample world.
//!
//! The snapshot comes from [`crate::tui::fixtures`], so an expected row list here can be read
//! straight off it.

use std::path::PathBuf;

use super::*;
use crate::store::AgentState;
use crate::tui::fixtures::{PARKED, snapshot, space, t, worktrees};

/// One readable line per row: `# <session>` for a header, [`row_text`] for a child.
fn labels(snap: &Snapshot, worktrees: &[WorktreeRecord], rows: &Rows) -> Vec<String> {
    rows.rows
        .iter()
        .map(|row| match row {
            Row::Session(i) => format!("# {}", snap.spaces[*i].session_name),
            other => row_text(snap, worktrees, other, false),
        })
        .collect()
}

fn rows_of(depth: Depth, layout: Layout, sort: AgentPanelSort, query: Option<&str>) -> Vec<String> {
    let snap = snapshot();
    let worktrees = worktrees();
    let query = query.map(|text| Query {
        text: text.to_string(),
    });
    let rows = build(
        &snap,
        &worktrees,
        depth,
        layout,
        sort,
        true,
        true,
        &Labels::default(),
        query.as_ref(),
    );
    assert_eq!(
        rows.rows.len(),
        rows.scores.len(),
        "every row needs a score slot"
    );
    labels(&snap, &worktrees, &rows)
}

#[test]
fn tree_lists_every_session_then_its_panes_in_tmux_order() {
    let got = rows_of(Depth::All, Layout::Tree, AgentPanelSort::Attention, None);

    assert_eq!(
        got,
        vec![
            "# mobile",
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle",
            "mobile nvim 3:nvim",
            "mobile zsh 4:zsh.1",
            "mobile htop 4:zsh.2",
            "# web",
            "web zsh 1:zsh",
            "web claude 2:zsh.1 working",
            "web gemini 2:zsh.2 idle",
            "web codex 3:tests done",
            "web node 4:server",
            "web feat/pdf worktree, closed",
            "# infra",
            "infra claude 1:zsh.1 working",
            "infra zsh 1:zsh.2",
            "infra nvim 2:docs",
            "infra tail 3:logs",
            "infra claude 4:zsh blocked",
            "# api",
            "api zsh 1:zsh",
            "# notes",
            "notes nvim 1:nvim",
            "notes zsh 2:zsh",
        ]
    );
}

#[test]
fn the_tree_is_never_sorted_by_attention() {
    let attention = rows_of(Depth::All, Layout::Tree, AgentPanelSort::Attention, None);
    let sessions = rows_of(Depth::All, Layout::Tree, AgentPanelSort::Sessions, None);

    assert_eq!(attention, sessions);
}

#[test]
fn without_a_query_no_row_carries_a_score() {
    let snap = snapshot();
    let worktrees = worktrees();

    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );

    assert!(rows.scores.iter().all(Option::is_none));
}

#[test]
fn depth_agents_keeps_agent_panes_and_worktrees() {
    let got = rows_of(Depth::Agents, Layout::Tree, AgentPanelSort::Attention, None);

    assert_eq!(
        got,
        vec![
            "# mobile",
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle",
            "# web",
            "web claude 2:zsh.1 working",
            "web gemini 2:zsh.2 idle",
            "web codex 3:tests done",
            "web feat/pdf worktree, closed",
            "# infra",
            "infra claude 1:zsh.1 working",
            "infra claude 4:zsh blocked",
            "# api",
            "# notes",
        ]
    );
}

#[test]
fn depth_sessions_shows_headers_only() {
    let got = rows_of(
        Depth::Sessions,
        Layout::Tree,
        AgentPanelSort::Attention,
        None,
    );

    assert_eq!(
        got,
        vec!["# mobile", "# web", "# infra", "# api", "# notes"]
    );
}

#[test]
fn a_flat_layout_at_the_sessions_depth_is_the_header_list() {
    assert_eq!(
        rows_of(
            Depth::Sessions,
            Layout::Flat,
            AgentPanelSort::Attention,
            None
        ),
        rows_of(
            Depth::Sessions,
            Layout::Tree,
            AgentPanelSort::Attention,
            None
        ),
    );
}

#[test]
fn where_label_names_the_pane_only_in_a_split_window() {
    let snap = snapshot();
    let single = &snap.panes[0]; // $1 window 1, one pane
    let split = &snap.panes[4]; // $1 window 4, two panes

    assert_eq!(where_label(single), "1:zsh");
    assert_eq!(where_label(split), "4:zsh.2");
}

#[test]
fn what_label_prefers_the_agents_display_name_then_its_name_then_its_kind() {
    let mut snap = snapshot();
    let pane = snap.panes[0].clone();
    let agent = pane.agent.expect("window 1 of $1 runs an agent");

    assert_eq!(what_label(&snap, &pane), "claude");
    snap.agents[agent].name = Some("reviewer".to_string());
    assert_eq!(what_label(&snap, &pane), "reviewer");
    snap.agents[agent].display = Some("Reviewer".to_string());
    assert_eq!(what_label(&snap, &pane), "Reviewer");
    assert_eq!(what_label(&snap, &snap.panes[2]), "nvim");
}

#[test]
fn a_closed_worktree_belongs_to_the_session_it_was_cut_from() {
    let snap = snapshot();
    let worktrees = worktrees();

    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(1, 0)]);
}

#[test]
fn a_worktree_with_a_session_running_at_its_path_is_not_closed() {
    let mut snap = snapshot();
    let worktrees = worktrees();
    snap.spaces.push(space(6, PARKED, "feat/pdf", (0, 0, 0, 0)));

    assert!(closed_worktrees(&snap, &worktrees).is_empty());
    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    let got = labels(&snap, &worktrees, &rows);
    assert!(
        !got.iter().any(|row| row.contains("worktree")),
        "no worktree row in {got:?}"
    );
}

#[test]
fn a_worktree_is_not_hung_under_a_session_that_reused_its_id() {
    let snap = snapshot();
    let mut worktrees = worktrees();
    // tmux restarted: `w2` now names web, but this record was cut from infra. The id alone
    // must not claim it — the repository it came out of decides.
    worktrees[0].repo_root = PathBuf::from("~/clients/infra");

    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(2, 0)]);
}

#[test]
fn a_session_below_the_repository_root_still_claims_its_worktree_by_id() {
    let mut snap = snapshot();
    let worktrees = worktrees();
    // web's session sits in a subdirectory of the repository the worktree was cut from,
    // which `git rev-parse --show-toplevel` reports as the root either way.
    snap.spaces[1].session_path = PathBuf::from("~/code/web/crates/core");

    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(1, 0)]);
}

#[test]
fn a_renumbered_session_still_claims_its_worktree_by_repository() {
    let snap = snapshot();
    let mut worktrees = worktrees();
    // tmux restarted and handed web a new session id, so the recorded `w2` resolves to
    // nothing; the repository the worktree came out of still names its session.
    worktrees[0].source_workspace_id = "w99".to_string();

    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(1, 0)]);
}

/// A machine's web session in a project behind a link there (`/var` is `/private/var`), and the record
/// of its worktree as that machine's git wrote it: the checkout and the root resolved.
fn linked_machine_world() -> (Snapshot, Vec<WorktreeRecord>) {
    let mut snap = snapshot();
    snap.spaces[1].machine = Some(0);
    snap.spaces[1].session_id = "studio:$2".to_string();
    snap.spaces[1].session_path = PathBuf::from("/var/folders/ab/proj");
    let mut worktrees = worktrees();
    worktrees[0].machine = Some(0);
    worktrees[0].path = PathBuf::from("/private/var/folders/ab/proj--feat-pdf");
    worktrees[0].repo_root = PathBuf::from("/private/var/folders/ab/proj");
    (snap, worktrees)
}

#[test]
fn a_machines_session_behind_a_link_gets_its_closed_row_by_the_folder_resolved_there() {
    let (mut snap, worktrees) = linked_machine_world();
    snap.spaces[1].real_path = Some(PathBuf::from("/private/var/folders/ab/proj"));

    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(1, 0)]);
}

#[test]
fn a_machines_session_behind_a_link_gets_no_closed_row_when_the_feed_sent_no_resolved_folder() {
    let (snap, worktrees) = linked_machine_world();

    // As before the feed sent them: the paths as written differ, so nothing pairs.
    assert!(closed_worktrees(&snap, &worktrees).is_empty());
}

#[test]
fn a_machines_record_behind_a_link_is_open_when_a_session_sits_in_its_resolved_checkout() {
    let (mut snap, worktrees) = linked_machine_world();
    snap.spaces[1].real_path = Some(PathBuf::from("/private/var/folders/ab/proj"));
    // Positive control: with no session in the checkout the row is closed.
    assert_eq!(closed_worktrees(&snap, &worktrees), vec![(1, 0)]);
    // A second session of the machine sits in the checkout, named as the shell gave it.
    let mut open = snap.spaces[1].clone();
    open.session_id = "studio:$9".to_string();
    open.session_path = PathBuf::from("/var/folders/ab/proj--feat-pdf");
    open.real_path = Some(PathBuf::from("/private/var/folders/ab/proj--feat-pdf"));
    snap.spaces.push(open);

    assert!(closed_worktrees(&snap, &worktrees).is_empty());
}

#[test]
fn a_worktree_whose_parent_session_is_gone_is_not_shown() {
    let mut snap = snapshot();
    let worktrees = worktrees();
    snap.spaces.remove(1); // web, the session the worktree was cut from

    assert!(closed_worktrees(&snap, &worktrees).is_empty());
}

#[test]
fn flat_puts_agents_first_in_attention_order() {
    let got = rows_of(Depth::All, Layout::Flat, AgentPanelSort::Attention, None);

    assert_eq!(
        got,
        vec![
            // Agents: blocked, then done, then idle newest first, then working newest
            // first.
            "infra claude 4:zsh blocked",
            "web codex 3:tests done",
            // idle rows come before working ones
            "mobile claude 2:review idle",
            "web gemini 2:zsh.2 idle",
            "web claude 2:zsh.1 working",
            "mobile claude 1:zsh working",
            "infra claude 1:zsh.1 working",
            // Plain panes, tmux order.
            "mobile nvim 3:nvim",
            "mobile zsh 4:zsh.1",
            "mobile htop 4:zsh.2",
            "web zsh 1:zsh",
            "web node 4:server",
            "infra zsh 1:zsh.2",
            "infra nvim 2:docs",
            "infra tail 3:logs",
            "api zsh 1:zsh",
            "notes nvim 1:nvim",
            "notes zsh 2:zsh",
            // Parked worktrees last.
            "web feat/pdf worktree, closed",
        ]
    );
}

#[test]
fn flat_attention_puts_an_unseen_blocked_agent_before_a_seen_one() {
    let mut snap = snapshot();
    let worktrees = worktrees();
    // infra window 1 is working and seen; make it blocked, unseen and older than the
    // blocked-but-seen agent in window 4, so only the tier can put it first.
    snap.agents[5].state = Some(AgentState::Blocked);
    snap.agents[5].seen = false;
    snap.agents[5].state_since = t(16, 0);
    assert!(snap.agents[5].state_since < snap.agents[6].state_since);
    assert_eq!(snap.agents[6].state, Some(AgentState::Blocked));
    assert!(snap.agents[6].seen);

    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    let got = labels(&snap, &worktrees, &rows);

    assert_eq!(
        &got[..2],
        ["infra claude 1:zsh.1 blocked", "infra claude 4:zsh blocked"]
    );
}

#[test]
fn flat_sessions_sort_keeps_agents_in_tmux_order() {
    let got = rows_of(Depth::All, Layout::Flat, AgentPanelSort::Sessions, None);

    assert_eq!(
        &got[..7],
        [
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle",
            "web claude 2:zsh.1 working",
            "web gemini 2:zsh.2 idle",
            "web codex 3:tests done",
            "infra claude 1:zsh.1 working",
            "infra claude 4:zsh blocked",
        ]
    );
    assert_eq!(got.last().unwrap(), "web feat/pdf worktree, closed");
}

#[test]
fn a_query_keeps_a_session_whose_child_matches_and_drops_the_rest() {
    let got = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("gemini"),
    );

    assert_eq!(got, vec!["# web", "web gemini 2:zsh.2 idle"]);
}

/// A header that matches by its branch, which no child row carries, keeps every child: infra's
/// rows have its name but not its branch, so none of them matches `'main 'infra` itself.
#[test]
fn a_query_matching_a_header_keeps_all_of_its_children() {
    let got = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("'main 'infra"),
    );

    assert_eq!(
        got,
        vec![
            "# infra",
            "infra claude 1:zsh.1 working",
            "infra zsh 1:zsh.2",
            "infra nvim 2:docs",
            "infra tail 3:logs",
            "infra claude 4:zsh blocked",
        ]
    );
}

/// A session's folder is not drawn in the tree, so it is not searched (the filter searches
/// what you can see on your screen): a query only in the folder (`code`, `clients`) finds
/// no header and no row, and the
/// session's name still finds its header and its rows, which carry the name.
#[test]
fn a_query_only_in_a_sessions_folder_matches_nothing() {
    for folder in ["'code/", "'clients", "~/code"] {
        let got = rows_of(
            Depth::All,
            Layout::Tree,
            AgentPanelSort::Attention,
            Some(folder),
        );
        assert_eq!(got, Vec::<String>::new(), "{folder}");
    }
    let got = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("'infra"),
    );
    assert_eq!(got.first().map(String::as_str), Some("# infra"), "{got:?}");
    assert!(
        got[1..].iter().all(|row| row.starts_with("infra ")),
        "{got:?}"
    );
    assert!(got.len() > 1, "{got:?}");
}

#[test]
fn only_the_rows_that_matched_carry_a_score() {
    let snap = snapshot();
    let worktrees = worktrees();
    let query = Query {
        text: "gemini".to_string(),
    };

    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        Some(&query),
    );

    assert_eq!(rows.rows.len(), 2);
    assert!(rows.scores[0].is_none(), "the header is kept by its child");
    let (score, indices) = rows.scores[1].as_ref().expect("the pane matched");
    assert!(*score > 0, "a match scores above zero, got {score}");
    let text = row_text(&snap, &worktrees, &rows.rows[1], true);
    let matched: String = text
        .chars()
        .enumerate()
        .filter(|(i, _)| indices.contains(i))
        .map(|(_, c)| c)
        .collect();
    assert_eq!(matched, "gemini");
}

#[test]
fn a_flat_query_filters_every_row_on_its_own() {
    // "clients" is only in infra's session path, which no child row repeats.
    let got = rows_of(
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
        Some("clients"),
    );

    assert!(got.is_empty(), "expected no rows, got {got:?}");
}

#[test]
fn a_flat_query_keeps_the_rows_that_match() {
    let got = rows_of(
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
        Some("gemini"),
    );

    assert_eq!(got, vec!["web gemini 2:zsh.2 idle"]);
}

#[test]
fn an_empty_query_filters_nothing() {
    let all = rows_of(Depth::All, Layout::Tree, AgentPanelSort::Attention, None);
    let empty = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some(""),
    );

    assert_eq!(empty, all);
}

/// A boxed agent's row draws a box emoji and no word, so `sandbox` finds nothing: its row's text
/// has no such word, and the session's name, drawn on the header above, still finds it.
#[test]
fn a_sandboxed_agents_row_is_not_found_by_the_word_sandbox() {
    let mut snap = snapshot();
    crate::tui::fixtures::sandboxed(&mut snap);
    for boxed in ["%111", "%121", "%221", "%231", "%341"] {
        let index = snap.panes.iter().position(|p| p.pane_id == boxed).unwrap();
        let text = row_text(&snap, &[], &Row::Pane(index), true);
        assert!(!text.contains("sandbox"), "{boxed}: {text}");
    }
    assert!(
        shown(&snap, Depth::All, Layout::Tree, "sandbox")
            .rows
            .is_empty(),
        "no row has the word"
    );
    let by_session = shown(&snap, Depth::Agents, Layout::Flat, "mobile claude");
    assert!(
        by_session
            .rows
            .iter()
            .any(|row| matches!(row, Row::Pane(at) if snap.panes[*at].pane_id == "%111")),
        "the control: the session's name still finds the boxed agent"
    );
}

#[test]
fn row_text_is_what_the_query_matches() {
    let snap = snapshot();
    let worktrees = worktrees();

    assert_eq!(
        row_text(&snap, &worktrees, &Row::Session(0), true),
        "mobile master \u{b1}3 \u{2191}1"
    );
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Session(0), false),
        "mobile"
    );
    // A clean tree contributes a branch but no status flags.
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Session(2), true),
        "infra main"
    );
    // A child row ends with its header's branch and git flags, which the row does not draw.
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Pane(4), true),
        "mobile htop 4:zsh.2 master \u{b1}3 \u{2191}1"
    );
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Pane(4), false),
        "mobile htop 4:zsh.2"
    );
    let parked = Row::ClosedWorktree {
        space: 1,
        record: 0,
    };
    assert_eq!(
        row_text(&snap, &worktrees, &parked, true),
        "web feat/pdf worktree, closed feat/invoices \u{b1}1"
    );
    assert_eq!(
        row_text(&snap, &worktrees, &parked, false),
        "web feat/pdf worktree, closed"
    );
}

/// The filter also matches the state word an agent row draws: `/blocked` lists the blocked agents, each under its session's header,
/// as herdr's Go To lists them by state. A plain pane draws no state and has none to match.
#[test]
fn the_filter_matches_the_state_word_an_agent_row_draws() {
    let snap = snapshot();
    let worktrees = worktrees();
    // infra's claude in window 4 (`%341`) is the one blocked agent.
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Pane(14), false),
        "infra claude 4:zsh blocked"
    );
    assert_eq!(
        row_text(&snap, &worktrees, &Row::Pane(4), false),
        "mobile htop 4:zsh.2"
    );

    let tree = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("blocked"),
    );
    assert_eq!(tree, vec!["# infra", "infra claude 4:zsh blocked"]);
    let flat = rows_of(
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
        Some("blocked"),
    );
    assert_eq!(flat, vec!["infra claude 4:zsh blocked"]);
    // The start of a state word is enough, as it is for any other word of a row.
    assert_eq!(
        rows_of(
            Depth::All,
            Layout::Tree,
            AgentPanelSort::Attention,
            Some("bloc")
        ),
        tree
    );
}

#[test]
fn a_hidden_branch_is_not_searchable() {
    let snap = snapshot();
    let worktrees = worktrees();
    let query = Query {
        text: "feat/invoices".to_string(),
    };
    let rows = |show_git| {
        build(
            &snap,
            &worktrees,
            Depth::All,
            Layout::Tree,
            AgentPanelSort::Attention,
            show_git,
            true,
            &Labels::default(),
            Some(&query),
        )
    };

    // Drawn: web's branch matches, so its header and every child stay.
    let shown = rows(true);
    assert_eq!(shown.rows[0], Row::Session(1));
    assert!(shown.scores[0].is_some(), "the header matched its branch");

    // Hidden: the branch is not on screen, so it is not searchable either.
    assert!(rows(false).rows.is_empty());
}

#[test]
fn score_finds_the_matched_characters_and_rejects_a_miss() {
    let query = Query {
        text: "mob".to_string(),
    };

    let (hit, indices) = score(&query, "mobile").expect("a prefix match");
    assert!(hit > 0);
    assert_eq!(indices, vec![0, 1, 2]);
    assert!(score(&query, "web").is_none());
}

/// The characters `query` matches in `text`, through the popup's own parse and matcher, or
/// `None` for a miss.
fn hits(query: &str, text: &str) -> Option<Vec<usize>> {
    let query = Query {
        text: query.to_string(),
    };
    score(&query, text).map(|(_, indices)| indices)
}

/// nucleo-matcher 0.3.1 compared a whole text with the term where a capital made the match
/// case-sensitive, so a prefix, a postfix or an exact term on ASCII text matched only a text
/// that was the term itself (taw's copy of the matcher compares the term's own text instead; see
/// `third_party/nucleo-matcher/TAW-CHANGES.md`).
#[test]
fn a_prefix_with_a_capital_anchors_at_the_start_of_the_text() {
    assert_eq!(hits("^Web", "Web api"), Some(vec![0, 1, 2]));
    assert_eq!(hits("^Web", "Webster"), Some(vec![0, 1, 2]));
    assert_eq!(
        hits("^Web", "web api"),
        None,
        "a capital in the term is case-sensitive"
    );
    assert_eq!(
        hits("^Web", "my Web api"),
        None,
        "a prefix is the start of the text"
    );
    // Without a capital the term ignores case, as it always did.
    assert_eq!(hits("^web", "Web api"), Some(vec![0, 1, 2]));
    assert_eq!(hits("^web", "web api"), Some(vec![0, 1, 2]));
}

#[test]
fn a_postfix_with_a_capital_anchors_at_the_end_of_the_text() {
    assert_eq!(hits("Api$", "my Api"), Some(vec![3, 4, 5]));
    assert_eq!(hits("Api$", "Api"), Some(vec![0, 1, 2]));
    assert_eq!(
        hits("Api$", "my api"),
        None,
        "a capital in the term is case-sensitive"
    );
    assert_eq!(
        hits("Api$", "Api x"),
        None,
        "a postfix is the end of the text"
    );
    assert_eq!(hits("api$", "my Api"), Some(vec![3, 4, 5]));
    assert_eq!(hits("api$", "my api"), Some(vec![3, 4, 5]));
}

#[test]
fn an_exact_term_and_a_substring_term_follow_smart_case() {
    assert_eq!(hits("^Web$", "Web"), Some(vec![0, 1, 2]));
    assert_eq!(hits("^Web$", "web"), None);
    assert_eq!(hits("^Web$", "Web api"), None);
    assert_eq!(hits("^web$", "Web"), Some(vec![0, 1, 2]));
    assert_eq!(hits("'Web", "my Web api"), Some(vec![3, 4, 5]));
    assert_eq!(hits("'Web", "my web api"), None);
    assert_eq!(hits("'web", "my Web api"), Some(vec![3, 4, 5]));
}

#[test]
fn an_inverse_prefix_or_postfix_with_a_capital_hides_what_it_names() {
    assert_eq!(hits("!^Web", "Web api"), None);
    assert!(hits("!^Web", "web api").is_some());
    assert!(hits("!^Web", "my Web api").is_some());
    assert_eq!(hits("!Api$", "my Api"), None);
    assert!(hits("!Api$", "my api").is_some());
    assert!(hits("!Api$", "Api x").is_some());
    // Beside a term that matches, the inverse still decides.
    assert_eq!(hits("x !^Web", "Web x"), None);
    assert_eq!(hits("x !^Web", "web x"), Some(vec![4]));
}

/// nucleo-matcher 0.3.1 never tried the last place a substring could start in a text with a
/// non-ASCII character in it, so a term at the very end of such a text did not match and an
/// inverse term there hid nothing.
#[test]
fn a_substring_at_the_end_of_a_text_with_a_non_ascii_character_matches() {
    assert_eq!(hits("'pi", "±1 api"), Some(vec![4, 5]));
    assert_eq!(hits("'fé", "café"), Some(vec![2, 3]));
    assert_eq!(hits("'api", "±1 api"), Some(vec![3, 4, 5]));
    // Not at the end, it always matched.
    assert_eq!(hits("'pi", "±1 api x"), Some(vec![4, 5]));
    assert_eq!(hits("'fé", "café au lait"), Some(vec![2, 3]));
    // A text the term is not in is still a miss.
    assert_eq!(hits("'pi", "±1 ap"), None);
    assert_eq!(hits("'fé", "cafe±"), None);
    // The whole text.
    assert_eq!(hits("'é±", "é±"), Some(vec![0, 1]));
}

#[test]
fn an_inverse_substring_at_the_end_of_a_text_with_a_non_ascii_character_hides_it() {
    assert_eq!(hits("!'pi", "±1 api"), None);
    assert!(hits("!'pi", "±1 ap").is_some());
    assert_eq!(hits("!'fé", "café"), None);
    assert_eq!(hits("x !'pi", "±1 api x"), None);
}

#[test]
fn an_address_term_ignores_case_and_a_lowercase_term_too() {
    // An address ignores case wherever it is.
    assert_eq!(hits("4A", "4a api"), Some(vec![0, 1]));
    assert_eq!(hits("^4A", "4a api"), Some(vec![0, 1]));
    assert_eq!(hits("!4A", "4a api"), None);
    assert!(hits("!4A", "5b api").is_some());
    // Another term with a capital still matches case-sensitively.
    assert_eq!(hits("Api", "my api"), None);
    assert_eq!(hits("Api", "my Api"), Some(vec![3, 4, 5]));
    // A lowercase term ignores case.
    assert_eq!(hits("api", "my Api"), Some(vec![3, 4, 5]));
    assert_eq!(hits("'api", "±1 API x"), Some(vec![3, 4, 5]));
}

/// A digit in the query is text, as fzf reads it: `3` is the `3` in
/// `3:tests`, or in mobile's `±3` (three modified files), never "session 3". A header
/// that matched on its own text brings its session along as it does for any query;
/// infra (session 3) keeps only the one row with a 3 in it.
#[test]
fn a_lone_digit_is_text_like_any_other_term() {
    let rows = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("3"),
    );
    assert_eq!(
        rows,
        vec![
            "# mobile",
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle",
            "mobile nvim 3:nvim",
            "mobile zsh 4:zsh.1",
            "mobile htop 4:zsh.2",
            "# web",
            "web codex 3:tests done",
            "# infra",
            "infra tail 3:logs",
        ]
    );
}

/// With a word beside it, the digit narrows as text too: `3 nvim` is the nvim in window
/// 3, not infra's nvim in window 2.
#[test]
fn a_digit_beside_a_word_narrows_as_text() {
    let rows = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("3 nvim"),
    );
    assert_eq!(
        rows,
        vec!["# mobile".to_string(), "mobile nvim 3:nvim".to_string()]
    );
}

#[test]
fn a_digit_no_row_contains_matches_nothing() {
    let rows = rows_of(
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        Some("9"),
    );
    assert!(rows.is_empty(), "{rows:?}");
}

#[test]
fn session_labels_are_the_first_nine_positions() {
    assert_eq!(session_label(0), Some(1));
    assert_eq!(session_label(8), Some(9));
    assert_eq!(session_label(9), None);
}

// ------------------------------------------------------------ agent views

/// The sample world with `json` installed in `@agent_wizard_view`.
fn snapshot_with_view(json: &str) -> Snapshot {
    let mut snap = snapshot();
    snap.view = crate::tui::agent_view::parse(Some(json))
        .unwrap_or_else(|e| panic!("expected {json} to parse: {e}"));
    assert!(snap.view.is_some(), "{json} should install a projection");
    snap
}

fn view_rows_of(json: &str, depth: Depth, layout: Layout, sort: AgentPanelSort) -> Vec<String> {
    let snap = snapshot_with_view(json);
    let worktrees = worktrees();
    let rows = build(
        &snap,
        &worktrees,
        depth,
        layout,
        sort,
        true,
        true,
        &Labels::default(),
        None,
    );
    labels(&snap, &worktrees, &rows)
}

/// herdr's rule: a view projects the *agent* rows. Plain panes and parked worktrees are
/// not agents and stay; a session the view emptied goes with its agents.
#[test]
fn an_installed_view_drops_the_agent_rows_it_does_not_match() {
    let got = view_rows_of(
        r#"{"source":"cli","filter":{"op":"in","field":"status","values":["blocked","done"]}}"#,
        Depth::Agents,
        Layout::Tree,
        AgentPanelSort::Attention,
    );

    assert_eq!(
        got,
        vec![
            // mobile's two agents are working and idle: nothing of it is left.
            "# web",
            "web codex 3:tests done",
            "web feat/pdf worktree, closed",
            "# infra",
            "infra claude 4:zsh blocked",
            // api and notes run no agents at all, so the view took nothing from them and
            // they read exactly as they do with no view installed.
            "# api",
            "# notes",
        ]
    );
}

#[test]
fn a_view_leaves_plain_panes_alone() {
    let got = view_rows_of(
        r#"{"source":"cli","filter":{"op":"eq","field":"status","value":"blocked"}}"#,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
    );

    assert_eq!(
        got,
        vec![
            "# mobile",
            "mobile nvim 3:nvim",
            "mobile zsh 4:zsh.1",
            "mobile htop 4:zsh.2",
            "# web",
            "web zsh 1:zsh",
            "web node 4:server",
            "web feat/pdf worktree, closed",
            "# infra",
            "infra zsh 1:zsh.2",
            "infra nvim 2:docs",
            "infra tail 3:logs",
            "infra claude 4:zsh blocked",
            "# api",
            "api zsh 1:zsh",
            "# notes",
            "notes nvim 1:nvim",
            "notes zsh 2:zsh",
        ]
    );
}

#[test]
fn a_view_that_matches_nothing_leaves_an_empty_tree_not_an_unfiltered_one() {
    let got = view_rows_of(
        r#"{"source":"cli","filter":{"op":"eq","field":"agent","value":"aider"}}"#,
        Depth::Agents,
        Layout::Tree,
        AgentPanelSort::Attention,
    );

    // Only the sessions the view took nothing from, and web's parked worktree.
    assert_eq!(
        got,
        vec!["# web", "web feat/pdf worktree, closed", "# api", "# notes"]
    );
}

/// A view carrying `sort` stands in for `[ui] agent_panel_sort` — herdr's rule, and the
/// reason installing one does not rewrite the config.
#[test]
fn a_views_own_sort_replaces_the_flat_layouts_attention_order() {
    let got = view_rows_of(
        r#"{"source":"cli","sort":[{"field":"agent","order":"asc"}]}"#,
        Depth::Agents,
        Layout::Flat,
        AgentPanelSort::Attention,
    );

    assert_eq!(
        got,
        vec![
            // By agent kind, ties in tmux order — not blocked-first.
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle",
            "web claude 2:zsh.1 working",
            "infra claude 1:zsh.1 working",
            "infra claude 4:zsh blocked",
            "web codex 3:tests done",
            "web gemini 2:zsh.2 idle",
            "web feat/pdf worktree, closed",
        ]
    );
}

#[test]
fn a_view_with_no_sort_leaves_the_flat_order_to_the_config() {
    let filtered = view_rows_of(
        r#"{"source":"cli","label":"all","filter":{"op":"exists","field":"status"}}"#,
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
    );
    let plain = rows_of(Depth::All, Layout::Flat, AgentPanelSort::Attention, None);

    // Every agent has a status, so this view keeps all of them: same rows, same order.
    assert_eq!(filtered, plain);
}

/// A sort-only view reorders the flat list without removing anything, including from the
/// sessions whose agents it sorts.
#[test]
fn a_sort_only_view_removes_no_row_from_the_tree() {
    let sorted = view_rows_of(
        r#"{"source":"cli","sort":[{"field":"agent","order":"desc"}]}"#,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
    );
    let plain = rows_of(Depth::All, Layout::Tree, AgentPanelSort::Attention, None);

    assert_eq!(
        sorted, plain,
        "a tree keeps tmux order whatever the view sorts"
    );
}

/// A view projects agent rows, and a headers-only tree has none: dropping the sessions
/// whose agents it would have hidden would blank the popup at a depth that never showed
/// an agent in the first place.
#[test]
fn a_view_never_empties_a_headers_only_tree() {
    let got = view_rows_of(
        r#"{"source":"cli","filter":{"op":"eq","field":"agent","value":"aider"}}"#,
        Depth::Sessions,
        Layout::Tree,
        AgentPanelSort::Attention,
    );

    assert_eq!(
        got,
        vec!["# mobile", "# web", "# infra", "# api", "# notes"]
    );
}

#[test]
fn a_view_narrows_the_rows_a_query_then_searches() {
    let snap = snapshot_with_view(
        r#"{"source":"cli","filter":{"op":"eq","field":"agent","value":"codex"}}"#,
    );
    let worktrees = worktrees();
    let query = Query {
        text: "claude".to_string(),
    };
    let rows = build(
        &snap,
        &worktrees,
        Depth::Agents,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        Some(&query),
    );

    // The view hid every claude before the query ran, so the query finds nothing.
    assert!(
        labels(&snap, &worktrees, &rows).is_empty(),
        "{:?}",
        labels(&snap, &worktrees, &rows)
    );
}

// ------------------------------------------------- the directory section of the filter

fn tree_rows(query: Option<&Query>) -> Rows {
    build(
        &snapshot(),
        &worktrees(),
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        query,
    )
}

/// The directories the filter lists (`[ui] projects_in_filter`) are rows of their own kind,
/// after every session, pane and parked worktree, in the order the picker gave them: the
/// rows above are unchanged.
#[test]
fn directories_come_after_every_other_row_in_the_order_given() {
    let mut rows = tree_rows(None);
    let before = rows.clone();

    append_projects(&mut rows, ["~/z/alpha", "~/z/beta", "~/z/gamma"], None);

    assert_eq!(rows.rows[..before.rows.len()], before.rows[..]);
    assert_eq!(rows.scores[..before.scores.len()], before.scores[..]);
    assert_eq!(
        rows.rows[before.rows.len()..],
        [Row::Project(0), Row::Project(1), Row::Project(2)]
    );
    assert_eq!(rows.scores.len(), rows.rows.len(), "a score slot for each");
    assert_eq!(rows.first_project(), Some(before.rows.len()));
}

/// A tree with no directories added has no first directory, and nothing is appended for an
/// empty list, so an empty section leaves no header to draw either.
#[test]
fn no_directories_means_no_section() {
    let mut rows = tree_rows(None);
    let before = rows.clone();
    assert_eq!(rows.first_project(), None);

    append_projects(&mut rows, std::iter::empty(), None);

    assert_eq!(rows, before);
    assert_eq!(rows.first_project(), None);
}

/// The characters a query matched in a directory's path are what the row highlights, found
/// with the filter's own matcher; a directory the query does not match carries none. Only
/// the highlight: whether a directory is listed is the picker's call.
#[test]
fn a_directory_carries_the_characters_the_query_matched_in_its_path() {
    let mut rows = tree_rows(None);
    let query = Query {
        text: "beta".to_string(),
    };

    append_projects(&mut rows, ["~/z/alpha", "~/z/beta-tool"], Some(&query));

    let at = rows.first_project().unwrap();
    assert_eq!(rows.scores[at], None, "alpha does not contain beta");
    let (_, hit) = rows.scores[at + 1].clone().expect("beta-tool matches");
    assert_eq!(hit, [4, 5, 6, 7], "the four letters of beta, in the path");
}

/// A directory belongs to no session, so nothing that walks a session's rows finds it.
#[test]
fn a_directory_belongs_to_no_session() {
    assert_eq!(space_of(&snapshot(), &Row::Project(0)), None);
}

// ------------------------------------------- the labels are what the query matches

/// The fixture's labels at the `all` depth in a tree, the letters `a` to `e` all free, each
/// number drawn as its digit (the modal preset's keys).
fn tree_labels(snap: &Snapshot, worktrees: &[WorktreeRecord]) -> Labels {
    let rows = build(
        snap,
        worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    Labels::new(snap, &rows, false, &['a', 'b', 'c', 'd', 'e'], |n| {
        n.to_string()
    })
}

/// A header's text starts with its number, a child's with its address (its session's number
/// and its letter), a parked worktree's too; a sixth row has no letter and no address.
#[test]
fn match_text_carries_a_headers_number_and_a_childs_address() {
    let snap = snapshot();
    let worktrees = worktrees();
    let labels = tree_labels(&snap, &worktrees);
    let text = |row: Row| match_text(&snap, &worktrees, &row, false, &labels);

    assert_eq!(text(Row::Session(1)), "2 web");
    assert_eq!(text(Row::Pane(5)), "2a web zsh 1:zsh");
    assert_eq!(text(Row::Pane(7)), "2c web gemini 2:zsh.2 idle");
    assert_eq!(text(Row::Pane(9)), "2e web node 4:server");
    let parked = Row::ClosedWorktree {
        space: 1,
        record: 0,
    };
    assert_eq!(
        text(parked),
        "web feat/pdf worktree, closed",
        "web's sixth row"
    );
    assert_eq!(text(Row::Pane(15)), "4a api zsh 1:zsh");
    assert_eq!(
        match_text(&snap, &worktrees, &Row::Pane(15), false, &Labels::default()),
        "api zsh 1:zsh",
        "no labels, no address"
    );
}

/// A letter a `[keys]` entry gives to an action is not drawn, and the row it would have named
/// goes without while the rows after it keep theirs.
#[test]
fn a_letter_taken_by_a_key_leaves_its_row_without_and_the_rest_in_place() {
    let snap = snapshot();
    let worktrees = worktrees();
    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    let labels = Labels::new(&snap, &rows, false, &['a', 'c', 'd', 'e'], |n| {
        n.to_string()
    });
    assert_eq!(labels.address(&Row::Pane(6)), "", "web's b is a key's");
    assert_eq!(labels.address(&Row::Pane(7)), "2c");
}

/// A flat list numbers its first nine rows that are not parked worktrees, in the order drawn,
/// and no row has a letter.
#[test]
fn a_flat_list_numbers_its_first_nine_rows() {
    let snap = snapshot();
    let worktrees = worktrees();
    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Flat,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    let labels = Labels::new(&snap, &rows, true, &['a', 'b', 'c', 'd', 'e'], |n| {
        n.to_string()
    });
    let numbers: Vec<Option<u8>> = rows
        .rows
        .iter()
        .map(|row| labels.get(row).and_then(|l| l.number))
        .collect();
    let expected: Vec<Option<u8>> = (1..=9)
        .map(Some)
        .chain(std::iter::repeat_n(None, rows.rows.len() - 9))
        .collect();
    assert_eq!(numbers, expected);
    assert!(
        rows.rows
            .iter()
            .all(|row| labels.get(row).is_none_or(|l| l.letter.is_none()))
    );
}

/// A flat list of fewer than nine rows numbers every row but a parked worktree, which takes
/// no number and moves none: the rows after it keep their places.
#[test]
fn a_flat_list_leaves_a_parked_worktree_unnumbered() {
    let snap = snapshot();
    let parked = Row::ClosedWorktree {
        space: 1,
        record: 0,
    };
    let rows = Rows {
        rows: vec![parked.clone(), Row::Pane(5), Row::Pane(6)],
        scores: vec![None; 3],
        ..Rows::default()
    };
    let labels = Labels::new(&snap, &rows, true, &['a', 'b', 'c', 'd', 'e'], |n| {
        n.to_string()
    });
    assert_eq!(labels.get(&parked), None);
    assert_eq!(labels.address(&Row::Pane(5)), "1");
    assert_eq!(labels.address(&Row::Pane(6)), "2");
}

/// A row's address is its session's number as `spell` draws it (the app passes
/// `Keymap::label`), then its letter.
#[test]
fn an_address_is_spelled_by_the_caller() {
    let snap = snapshot();
    let worktrees = worktrees();
    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    let labels = Labels::new(&snap, &rows, false, &['a', 'b', 'c', 'd', 'e'], |n| {
        format!("<{n}>")
    });
    assert_eq!(labels.address(&Row::Session(3)), "<4>");
    assert_eq!(labels.address(&Row::Pane(15)), "<4>a");
}

/// A query that is exactly a numbered address, in any case, keeps that row and scores it above
/// every other; a letter alone is not a name, nor is an address the query only contains.
#[test]
fn a_query_that_is_exactly_an_address_names_its_row() {
    let snap = snapshot();
    let worktrees = worktrees();
    let labels = tree_labels(&snap, &worktrees);
    let built = |text: &str| {
        build(
            &snap,
            &worktrees,
            Depth::All,
            Layout::Tree,
            AgentPanelSort::Attention,
            true,
            true,
            &labels,
            Some(&Query {
                text: text.to_string(),
            }),
        )
    };
    let score_of = |rows: &Rows, row: Row| {
        let at = rows.rows.iter().position(|r| *r == row).expect("in view");
        rows.scores[at].clone().expect("matched")
    };
    // `2C` has a capital, so smart case makes it match nothing as text: the name keeps it.
    for text in ["2c", "2C", " 2c "] {
        let rows = built(text);
        assert_eq!(
            score_of(&rows, Row::Pane(7)),
            (NAMED, vec![0, 1]),
            "{text:?}"
        );
    }
    assert_eq!(score_of(&built("2"), Row::Session(1)), (NAMED, vec![0]));
    assert_ne!(score_of(&built("2c web"), Row::Pane(7)).0, NAMED);
}

// ------------------------------- `^` anchors a row's own words

/// The query's match for `row` in the fixture's tree at the `all` depth, labels drawn.
fn match_of(text: &str, row: Row) -> Option<Match> {
    let snap = snapshot();
    let worktrees = worktrees();
    let labels = tree_labels(&snap, &worktrees);
    let rows = build(
        &snap,
        &worktrees,
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &labels,
        Some(&Query {
            text: text.to_string(),
        }),
    );
    let at = rows.rows.iter().position(|r| *r == row)?;
    rows.scores[at].clone()
}

/// `^name` anchors the session's name: its header and its child (whose text starts with the
/// session's name too) are found by it although a label is in front, and a session whose name
/// only contains it is not.
#[test]
fn a_prefix_anchor_finds_a_header_and_a_child_by_the_sessions_name() {
    assert!(match_of("^api", Row::Session(3)).is_some(), "header");
    assert!(match_of("^api", Row::Pane(15)).is_some(), "child");
    assert!(match_of("^pi", Row::Session(3)).is_none(), "not a prefix");
    assert!(
        match_of("^api", Row::Session(1)).is_none(),
        "another session"
    );
}

/// The label stays what a prefix anchor can match: `^4` is session 4's header (and its rows,
/// whose address starts with 4), `^api` is the name and `4a` still reaches row a.
#[test]
fn a_prefix_anchor_still_finds_a_row_by_its_label() {
    assert!(match_of("^4", Row::Session(3)).is_some());
    assert!(
        match_of("^4", Row::Pane(15)).is_some(),
        "its address starts with 4"
    );
    assert!(match_of("^4", Row::Session(1)).is_none(), "2 web");
    assert_eq!(match_of("4a", Row::Pane(15)), Some((NAMED, vec![0, 1])));
    assert!(
        match_of("4a", Row::Session(3)).is_none()
            || match_of("4a", Row::Session(3)).unwrap().0 != NAMED
    );
}

/// A match through the row's own words lights the same characters of the text the renderer
/// shows, with or without a label: positions in the match text, past the address and the
/// space after it (`4 api ...`, so `api` is 2, 3, 4).
#[test]
fn a_match_through_the_plain_text_keeps_the_columns_it_had() {
    assert_eq!(match_of("^api", Row::Session(3)).unwrap().1, vec![2, 3, 4]);
    // Row a of session 4 reads `4a api zsh 1:zsh`: the name starts at 3.
    assert_eq!(match_of("^api", Row::Pane(15)).unwrap().1, vec![3, 4, 5]);
    // A match the label does not take part in is the same with or without the anchor.
    assert_eq!(match_of("zsh", Row::Pane(15)).unwrap().1, vec![7, 8, 9]);
}

/// A suffix anchor works as it did: it is the end of the row's text either way.
#[test]
fn a_suffix_anchor_matches_the_end_of_a_rows_text() {
    assert!(match_of("idle$", Row::Pane(7)).is_some());
    assert!(match_of("web$", Row::Pane(7)).is_none());
}

// ------------------------------------------------- an agent row is found by its title

/// The fixture with `title` as the title of the agent in `pane_id` (the glyph Claude Code writes in
/// front of it comes off when the row draws it).
fn titled(pane_id: &str, title: &str) -> Snapshot {
    let mut snap = snapshot();
    let agent = snap
        .agents
        .iter_mut()
        .find(|a| a.pane_id == pane_id)
        .expect("an agent in that pane");
    agent.pane_title = title.to_string();
    snap
}

fn pane_row(snap: &Snapshot, pane_id: &str) -> Row {
    Row::Pane(
        snap.panes
            .iter()
            .position(|p| p.pane_id == pane_id)
            .expect("that pane"),
    )
}

/// The tree at the `all` depth for `query`, `titles` being `[ui] titles`.
fn found(snap: &Snapshot, query: &str, titles: bool) -> Rows {
    let query = Query {
        text: query.to_string(),
    };
    build(
        snap,
        &worktrees(),
        Depth::All,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        titles,
        &Labels::default(),
        Some(&query),
    )
}

/// The pane rows `found` keeps.
fn found_panes(snap: &Snapshot, query: &str, titles: bool) -> Vec<String> {
    let rows = found(snap, query, titles);
    rows.rows
        .iter()
        .filter_map(|row| match row {
            Row::Pane(pane) => Some(snap.panes[*pane].pane_id.clone()),
            _ => None,
        })
        .collect()
}

/// A word of the title the row draws finds the row, which its own words alone would not.
#[test]
fn a_title_word_finds_the_agent_row() {
    let snap = titled("%111", "\u{2733} Fix flaky auth tests");
    assert_eq!(found_panes(&snap, "flaky", true), ["%111", "%231"]);
    // The header over a found row stays, as it does for any child.
    let rows = found(&snap, "auth", true);
    assert_eq!(
        rows.rows
            .iter()
            .filter(|row| matches!(row, Row::Session(_)))
            .count(),
        2,
        "mobile for the title, infra for `Migrate auth to OIDC`"
    );
    let at = rows
        .rows
        .iter()
        .position(|row| *row == pane_row(&snap, "%111"))
        .unwrap();
    let (_, positions) = rows.scores[at].clone().expect("the row matched itself");
    let own = own_words(&snap, &worktrees(), &pane_row(&snap, "%111"), true);
    let from = title_start(&own);
    assert_eq!(
        positions,
        [from + 10, from + 11, from + 12, from + 13],
        "`auth` is at 10 in the title, after the row's own words"
    );
}

/// The title is found whole, though the row draws it cut: the end of a long one finds the row.
#[test]
fn a_word_only_in_the_cut_end_of_a_title_finds_the_row() {
    let snap = titled(
        "%111",
        "Reconcile the ledger against the bank statement for october",
    );
    assert_eq!(found_panes(&snap, "october", true), ["%111"]);
    assert_eq!(found_panes(&snap, "ledger", true), ["%111"]);
}

/// Letters scattered through a title never find a row: long titles would otherwise light every
/// agent for a short query. The same letters as one run do.
#[test]
fn letters_scattered_through_a_title_do_not_find_the_row() {
    let snap = titled("%111", "Fix flaky auth tests");
    assert_eq!(found_panes(&snap, "flath", true), Vec::<String>::new());
    assert_eq!(found_panes(&snap, "flaky", true), ["%111", "%231"]);
    assert_eq!(found_panes(&snap, "'y au", true), ["%111"]);
    // An exact term is one run as well.
    assert_eq!(found_panes(&snap, "'flath", true), Vec::<String>::new());
}

/// An inverse term drops a row whose title has the word.
#[test]
fn an_inverse_term_drops_a_row_by_its_title() {
    let snap = snapshot();
    let all = found_panes(&snap, "claude", true);
    assert!(all.contains(&"%111".to_string()), "{all:?}");
    let without = found_panes(&snap, "claude !tidy", true);
    assert!(!without.contains(&"%111".to_string()), "{without:?}");
    assert_eq!(
        without.len() + 1,
        all.len(),
        "only the row whose title has `Tidy` goes"
    );
    // With titles off the word is not on any row, so nothing is dropped.
    assert_eq!(found_panes(&snap, "claude !tidy", false), all);
}

/// `^` is the title's start and `$` its end, not the row's.
#[test]
fn a_title_is_anchored_at_its_own_ends() {
    let snap = titled("%111", "Fix flaky auth tests");
    assert_eq!(found_panes(&snap, "^Fix", true), ["%111"]);
    assert_eq!(found_panes(&snap, "^flaky", true), Vec::<String>::new());
    assert_eq!(found_panes(&snap, "tests$", true), ["%111", "%231"]);
    assert_eq!(found_panes(&snap, "auth$", true), Vec::<String>::new());
    assert_eq!(found_panes(&snap, "^Fix flaky auth tests$", true), ["%111"]);
}

/// With `[ui] titles` off the title is not drawn, so it is not found either.
#[test]
fn with_titles_off_a_title_is_neither_drawn_nor_found() {
    let snap = titled("%111", "Fix flaky auth tests");
    let Row::Pane(pane) = pane_row(&snap, "%111") else {
        unreachable!()
    };
    let p = &snap.panes[pane];
    assert_eq!(
        drawn_title(&snap, p, true).as_deref(),
        Some("Fix flaky auth tests")
    );
    assert_eq!(drawn_title(&snap, p, false), None);
    assert_eq!(found_panes(&snap, "flaky", false), Vec::<String>::new());
}

/// A title that only repeats the *what* column is not drawn, so it adds nothing to the row's match.
#[test]
fn a_title_equal_to_the_what_column_is_not_added_twice() {
    let snap = titled("%111", "claude");
    let Row::Pane(pane) = pane_row(&snap, "%111") else {
        unreachable!()
    };
    assert_eq!(drawn_title(&snap, &snap.panes[pane], true), None);
    let plain = snapshot();
    let with = found(&snap, "claude", true);
    let without = found(&plain, "claude", true);
    assert_eq!(with, without, "the same rows with the same positions");
    // `^claude` anchors a row's own words, which start with the session's name, and a title: were
    // the repeated one added, the row would be found by the title alone.
    assert_eq!(
        found(&snap, "^claude", true),
        found(&plain, "^claude", true)
    );
    assert!(!found_panes(&snap, "^claude", true).contains(&"%111".to_string()));
}

/// A row with no title, and a query that matches no title, score and light exactly as they did.
#[test]
fn a_row_with_no_title_scores_and_lights_as_it_did() {
    let snap = snapshot();
    let worktrees = worktrees();
    for text in [
        "claude",
        "working",
        "zsh",
        "^web",
        "!idle",
        "infra claude",
        "4a",
    ] {
        assert_eq!(
            found(&snap, text, true),
            found(&snap, text, false),
            "{text}: no title matches, so titles change nothing"
        );
    }
    // The shared scorer for a row's words and a title: no title is `score_labelled`.
    let query = Query {
        text: "zsh".to_string(),
    };
    let row = pane_row(&snap, "%222");
    let own = row_text(&snap, &worktrees, &row, true);
    let plain = score_labelled(&query, "2d", true, &own).expect("zsh is in its words");
    let titled = score_titled(&query, "2d", true, &own, "").expect("and still is");
    assert_eq!((titled.score, titled.labelled), plain);
    assert!(titled.title.is_empty());
    // A title that matches nothing, on a row with a numbered address, leaves the match alone too.
    let other = score_titled(&query, "2d", true, &own, "tidy page").expect("still matches");
    assert_eq!((other.score, other.labelled), plain);
    assert!(other.title.is_empty());
}

/// The match the try page checks: a title found as one run lights the title's own characters.
#[test]
fn a_titled_match_lights_the_labelled_text_and_the_title_apart() {
    let query = Query {
        text: "web flaky".to_string(),
    };
    let found = score_titled(
        &query,
        "2a",
        true,
        "web claude 2:zsh working",
        "Fix flaky auth",
    )
    .expect("both terms");
    assert_eq!(found.labelled, [3, 4, 5], "`web` in the words, after `2a `");
    assert_eq!(found.title, [4, 5, 6, 7, 8], "`flaky` in the title");
    let scattered = Query {
        text: "fxa".to_string(),
    };
    assert_eq!(
        score_titled(&scattered, "", false, "web claude", "Fix flaky auth"),
        None
    );
}

/// The seam that carries a header's words: with none it is `score_titled`, and with some they are a
/// last resort of a term (one run, never anchored, lighting nothing), the row's own letters
/// lighting where a term is in both.
#[test]
fn a_row_carrying_its_headers_words_is_found_by_them_last() {
    let own = "claude 1:zsh working";
    let title = "Fix flaky auth";
    let header = "main \u{b1}1";
    for text in [
        "claude", "flaky", "^claude", "!flaky", "4a", "fxat", "main", "",
    ] {
        let query = Query {
            text: text.to_string(),
        };
        assert_eq!(
            score_carrying(&query, "4a", true, own, title, ""),
            score_titled(&query, "4a", true, own, title),
            "{text:?}: no header words is score_titled"
        );
    }
    let find = |text: &str| {
        let query = Query {
            text: text.to_string(),
        };
        score_carrying(&query, "4a", true, own, title, header)
    };
    let found = find("main").expect("a header's word finds the row");
    assert!(
        found.labelled.is_empty() && found.title.is_empty(),
        "and lights nothing"
    );
    assert!(find("ma").is_some(), "a run of the header's word is found");
    assert!(
        find("mn").is_none(),
        "`m` and `n` are scattered in `main`, which a fuzzy match would find and one run does not"
    );
    assert!(find("^main").is_none() && find("main$").is_none() && find("^main$").is_none());
    assert!(
        find("!main").is_none(),
        "an inverse term drops the row by them"
    );
    assert!(
        find("!^main").is_some(),
        "an anchored one does not look in them"
    );
    let shared = "claude main \u{b1}1";
    let query = Query {
        text: "claude".to_string(),
    };
    let both = score_carrying(&query, "4a", true, own, title, shared)
        .expect("a word in the row's own words and its header's");
    assert!(
        !both.labelled.is_empty(),
        "the row's own letters light, not nothing as the header's would"
    );
    assert_eq!(
        Some((both.score, both.labelled)),
        score_titled(&query, "4a", true, own, title).map(|t| (t.score, t.labelled)),
        "a word in the row's own words and its header's: the row's own letters light"
    );
}

/// The title's positions start where the matcher counts the end of the row's own words, which is
/// in bytes when every grapheme of them starts with an ASCII character: two NFD accents or a
/// letter and U+FE0F before a title leave the title's letters its own positions, and the
/// words' last letters none of the title's.
#[test]
fn the_title_starts_where_the_matcher_counts_the_end_of_the_own_words() {
    let query = Query {
        text: "fix".to_string(),
    };
    for own in [
        "Re\u{301}sume\u{301} claude working",
        "zs\u{fe0f}h claude working",
    ] {
        let found = score_titled(&query, "", false, own, "fix flaky").expect("the title has fix");
        assert_eq!(found.title, [0, 1, 2], "{own:?}");
        assert!(found.labelled.is_empty(), "{own:?}: {:?}", found.labelled);
    }
}

/// A parked worktree draws `worktree, closed`, so `closed` finds it and lights what is drawn.
#[test]
fn a_closed_worktrees_row_is_found_by_closed() {
    let snap = snapshot();
    let rows = found(&snap, "closed", true);
    let parked = rows
        .rows
        .iter()
        .position(|row| matches!(row, Row::ClosedWorktree { .. }))
        .expect("the parked row is kept");
    let (_, positions) = rows.scores[parked].clone().expect("it matched");
    let text = row_text(&snap, &worktrees(), &rows.rows[parked], true);
    assert_eq!(text, "web feat/pdf worktree, closed feat/invoices \u{b1}1");
    assert_eq!(positions, (23..29).collect::<Vec<_>>());
    let drawn: String = positions
        .iter()
        .map(|&at| text.chars().nth(at).unwrap())
        .collect();
    assert_eq!(drawn, "closed");
}

// ------------------------------------- a row carries its header's words; a folded header its agents'

/// The fixture with mobile's session on branch `release`.
fn released() -> Snapshot {
    let mut snap = snapshot();
    snap.spaces[0].git.as_mut().expect("git").branch = Some("release".to_string());
    snap
}

/// The tree at `depth` for `query`, with the git words, titles and no labels.
fn shown(snap: &Snapshot, depth: Depth, layout: Layout, query: &str) -> Rows {
    let query = Query {
        text: query.to_string(),
    };
    build(
        snap,
        &worktrees(),
        depth,
        layout,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        Some(&query),
    )
}

/// [`labels`] of the rows `shown` keeps: a header as `# name`, a child as its drawn words.
fn lines(snap: &Snapshot, rows: &Rows) -> Vec<String> {
    labels(snap, &worktrees(), rows)
}

/// Words split between a header and a row find the row: `release` is mobile's branch, drawn on its
/// header, `claude` is what the row draws. Both rows of mobile's claude agents are found, under the
/// header, and no other session's.
#[test]
fn words_split_between_a_header_and_its_row_find_the_row() {
    let snap = released();
    let rows = shown(&snap, Depth::Agents, Layout::Tree, "release claude");
    assert_eq!(
        lines(&snap, &rows),
        vec![
            "# mobile",
            "mobile claude 1:zsh working",
            "mobile claude 2:review idle"
        ]
    );
    assert!(rows.scores[0].is_none(), "the header matched nothing alone");
    assert!(rows.scores[1].is_some() && rows.scores[2].is_some());
    // The words only the header draws light nothing on the row: what is lit within the drawn words
    // is `claude`, and the rest is past them.
    let own = row_text(&snap, &worktrees(), &rows.rows[1], false);
    let (_, positions) = rows.scores[1].clone().unwrap();
    let lit: Vec<usize> = positions
        .iter()
        .copied()
        .filter(|&at| at < own.chars().count())
        .collect();
    assert_eq!(lit, (7..13).collect::<Vec<_>>(), "{positions:?} in {own:?}");

    let none = shown(&snap, Depth::Agents, Layout::Tree, "release qqq");
    assert!(none.rows.is_empty(), "{:?}", lines(&snap, &none));
}

/// A query of the header's words alone keeps the cursor on the header: the header scores, the rows
/// it carries do not (the cursor goes to the best score, a tie to a child).
#[test]
fn a_query_of_header_words_scores_the_header_and_none_of_its_rows() {
    let snap = released();
    let rows = shown(&snap, Depth::All, Layout::Tree, "release");
    let session = lines(&snap, &rows);
    assert_eq!(session.len(), 6, "{session:?}");
    assert_eq!(
        session[0], "# mobile",
        "the session's whole tree is carried"
    );
    assert!(rows.scores[0].is_some());
    assert!(
        rows.scores[1..].iter().all(Option::is_none),
        "{:?}",
        rows.scores
    );
}

/// A flat list draws no header, so the header's branch is on no screen of it: `release claude`
/// finds mobile's two agents in the tree (the header above them draws `release`) and none in a flat
/// list, which still finds them by what its rows draw, the session's name included.
#[test]
fn the_flat_list_does_not_find_a_row_by_the_branch_of_a_header_it_does_not_draw() {
    let snap = released();
    let tree = shown(&snap, Depth::Agents, Layout::Tree, "release claude");
    let flat = shown(&snap, Depth::Agents, Layout::Flat, "release claude");
    assert_eq!(tree.rows.len(), 3, "{:?}", lines(&snap, &tree));
    assert!(flat.rows.is_empty(), "{:?}", lines(&snap, &flat));
    let by_name = shown(&snap, Depth::Agents, Layout::Flat, "mobile claude");
    assert_eq!(by_name.rows.len(), 2, "{:?}", lines(&snap, &by_name));
}

/// An inverse term takes a row's header words into account as well.
#[test]
fn an_inverse_term_drops_the_rows_of_a_header_with_that_word() {
    let snap = released();
    let rows = shown(&snap, Depth::Agents, Layout::Tree, "claude !release");
    let found = lines(&snap, &rows);
    assert!(
        !found.iter().any(|line| line.contains("mobile")),
        "{found:?}"
    );
    assert!(found.contains(&"infra claude 4:zsh blocked".to_string()));
}

/// The header words are found as they are drawn, in one run: web's branch is `bug/locale`, so
/// `loc zsh` finds its zsh rows (`loc` from the header, `zsh` from the row), while `bloc zsh` finds
/// none of them: `bloc` is scattered over `bug/locale`, which a fuzzy match would accept, and
/// the rows' own words have no such letters.
#[test]
fn the_header_words_are_matched_in_one_run_not_scattered() {
    let mut snap = snapshot();
    snap.spaces[1].git.as_mut().expect("git").branch = Some("bug/locale".to_string());
    let web = |query: &str| -> Vec<String> {
        let rows = shown(&snap, Depth::All, Layout::Tree, query);
        let all = lines(&snap, &rows);
        all.into_iter()
            .filter(|line| line.starts_with("web "))
            .collect()
    };
    assert_eq!(
        web("loc zsh"),
        vec![
            "web zsh 1:zsh",
            "web claude 2:zsh.1 working",
            "web gemini 2:zsh.2 idle"
        ]
    );
    assert_eq!(web("bloc zsh"), Vec::<String>::new());
}

/// The sessions depth finds a header only by the words it draws: its session's name, the branch
/// and git flags, and the title of the agent that needs you. What its folded agents are (their
/// kind, their state, where they sit, a title the header does not draw) is on no screen, so it
/// finds nothing.
#[test]
fn at_the_sessions_depth_a_header_is_found_only_by_the_words_it_draws() {
    let snap = snapshot();
    let headers = |query: &str| lines(&snap, &shown(&snap, Depth::Sessions, Layout::Tree, query));
    // The agents' kinds and states: the header draws counts, not these words.
    assert_eq!(headers("codex"), Vec::<String>::new());
    assert_eq!(headers("gemini"), Vec::<String>::new());
    assert_eq!(headers("claude"), Vec::<String>::new());
    assert_eq!(headers("blocked"), Vec::<String>::new());
    assert_eq!(headers("done"), Vec::<String>::new());
    // A title the header does not draw: mobile's claude is working, so no title is on its header.
    assert_eq!(headers("tidy"), Vec::<String>::new());
    // The positive controls: the session's name, its branch, and the hot agent's drawn title.
    assert_eq!(headers("web"), vec!["# web"]);
    assert_eq!(headers("infra"), vec!["# infra"]);
    assert_eq!(headers("invoices"), vec!["# web"]);
    assert_eq!(headers("flaky"), vec!["# web"]);
    assert_eq!(headers("oidc"), Vec::<String>::new());
    assert_eq!(headers("fixes"), vec!["# infra"]);
    // A word of the name and a word of the drawn title together.
    assert_eq!(headers("web flaky"), vec!["# web"]);
    assert_eq!(headers("infra flaky"), Vec::<String>::new());
    // An inverse term drops the header whose drawn title has it.
    assert_eq!(headers("web !flaky"), Vec::<String>::new());
    assert_eq!(headers("qqq"), Vec::<String>::new());
    // Without a query, nothing changes.
    let all = build(
        &snap,
        &worktrees(),
        Depth::Sessions,
        Layout::Tree,
        AgentPanelSort::Attention,
        true,
        true,
        &Labels::default(),
        None,
    );
    assert_eq!(lines(&snap, &all).len(), 5);
    assert!(all.stands_for.is_empty());
}

/// With `[ui] titles` off the header draws no title, so the hot agent's title finds nothing.
#[test]
fn a_header_that_draws_no_title_is_not_found_by_one() {
    let snap = snapshot();
    let query = Query {
        text: "flaky".to_string(),
    };
    let rows = |titles: bool| {
        build(
            &snap,
            &worktrees(),
            Depth::Sessions,
            Layout::Tree,
            AgentPanelSort::Attention,
            true,
            titles,
            &Labels::default(),
            Some(&query),
        )
    };
    assert_eq!(lines(&snap, &rows(true)), vec!["# web"]);
    assert!(lines(&snap, &rows(false)).is_empty());
}

/// A header found by the title it draws lights nothing on the name, and one found by its name
/// lights the name.
#[test]
fn a_header_found_by_a_drawn_title_lights_nothing_of_its_name() {
    let snap = snapshot();
    let by_title = shown(&snap, Depth::Sessions, Layout::Tree, "flaky");
    let (score, positions) = by_title.scores[0].clone().expect("found by the title");
    assert!(score > 0);
    assert!(
        positions.iter().all(|&at| at >= "web feat/invoices".len()),
        "{positions:?}"
    );
    let by_name = shown(&snap, Depth::Sessions, Layout::Tree, "web");
    let (_, positions) = by_name.scores[0].clone().expect("found by the name");
    assert_eq!(positions, vec![0, 1, 2]);
}

/// A header found by its own words stands for what it stands for with no filter: every agent the
/// view shows in its session. A header found only by the title it draws stands for that agent.
#[test]
fn a_folded_header_stands_for_what_its_words_found_it_by() {
    let snap = snapshot();
    let agent_ids = |rows: &Rows, space: usize| -> Vec<String> {
        rows.stands_for[&space]
            .iter()
            .map(|&agent| snap.agents[agent].pane_id.clone())
            .collect()
    };
    // Found by the name: all the agents `Tab` walks with no filter.
    let own = shown(&snap, Depth::Sessions, Layout::Tree, "infra");
    assert_eq!(agent_ids(&own, 2), vec!["%311", "%341"]);
    assert_eq!(
        agent_ids(&own, 2),
        shown_agents(&snap, 2)
            .iter()
            .map(|&agent| snap.agents[agent].pane_id.clone())
            .collect::<Vec<_>>()
    );
    // Found by the name, though a word of its hot agent's title would too: still all of them.
    let web = shown(&snap, Depth::Sessions, Layout::Tree, "web");
    assert_eq!(agent_ids(&web, 1), vec!["%221", "%222", "%231"]);
    // Found by the hot agent's title alone: that agent.
    let title = shown(&snap, Depth::Sessions, Layout::Tree, "flaky");
    assert_eq!(agent_ids(&title, 1), vec!["%231"]);
    // A query no header's words match stands for nothing.
    let none = shown(&snap, Depth::Sessions, Layout::Tree, "idle");
    assert!(none.stands_for.is_empty());
}

/// The session whose branch is `mclaude`, so that `mcl` is a run in its header's words and
/// letters scattered through its claude rows' own.
fn mclaude() -> Snapshot {
    let mut snap = snapshot();
    snap.spaces[0].git.as_mut().expect("git").branch = Some("mclaude".to_string());
    snap
}

/// A row that matches by its own words (scattered letters, a low score) and by its header's words
/// (one run, as high as the header's own score) is scored by its own, so the header keeps the
/// cursor, which goes to the best score and, at a tie, to a child.
#[test]
fn a_header_word_query_keeps_the_cursor_on_the_header_though_a_childs_run_scores_as_high() {
    let snap = mclaude();
    let rows = shown(&snap, Depth::Agents, Layout::Tree, "mcl");
    let head = rows.scores[0].clone().expect("the header matched").0;
    assert!(
        lines(&snap, &rows)[1].starts_with("mobile claude"),
        "{:?}",
        lines(&snap, &rows)
    );
    for score in &rows.scores[1..3] {
        let (score, _) = score
            .clone()
            .expect("a child of a matched header may score");
        assert!(score < head, "{score} against the header's {head}");
    }
}

/// A term the row's own words have is lit there, though the header's words have it as one run that
/// scores higher: `mcl` is scattered over the row's words (`m`, `cl`) and whole in the branch.
#[test]
fn a_term_in_a_childs_own_words_and_its_headers_lights_the_childs_own_letters() {
    let snap = mclaude();
    // `zsh` keeps the header from matching alone, so the row stands on its words and its header's.
    let rows = shown(&snap, Depth::Agents, Layout::Tree, "mcl zsh");
    let own = row_text(&snap, &worktrees(), &rows.rows[1], false);
    assert_eq!(own, "mobile claude 1:zsh working");
    assert!(rows.scores[0].is_none(), "the header matched nothing alone");
    let (_, positions) = rows.scores[1].clone().expect("the row matched");
    let lit: String = positions
        .iter()
        .filter(|&&at| at < own.chars().count())
        .map(|&at| own.chars().nth(at).unwrap())
        .collect();
    assert_eq!(lit, "mclzsh", "{positions:?}");
}

/// `^` and `$` anchor a row's own words, its label or its title, never the words it has of its
/// header's: `^rel` and `release$` find no row by the header's branch, in a tree or a flat list,
/// where `rel` finds them.
#[test]
fn an_anchored_term_never_finds_a_row_by_its_headers_words() {
    let snap = released();
    for layout in [Layout::Tree, Layout::Flat] {
        let by = |query: &str| -> Vec<String> {
            let rows = shown(&snap, Depth::Agents, layout, query);
            lines(&snap, &rows)
                .into_iter()
                .filter(|line| line.starts_with("mobile"))
                .collect()
        };
        if layout == Layout::Tree {
            assert_eq!(by("rel claude").len(), 2, "{layout:?}");
        } else {
            // A flat list draws no header, so it has no branch to find a row by: `rel` only
            // matches the other row's scattered letters (`review`, `claude`).
            assert!(
                by("rel claude").iter().all(|line| !line.contains("1:zsh")),
                "{layout:?}"
            );
        }
        assert!(by("^release claude").is_empty(), "{layout:?}");
        assert!(by("^rel claude").is_empty(), "{layout:?}");
        assert!(by("release$ claude").is_empty(), "{layout:?}");
        assert!(by("^release$ claude").is_empty(), "{layout:?}");
        // An inverse anchored term does not drop a row by them either.
        assert_eq!(by("claude !^release").len(), 2, "{layout:?}");
    }
}

/// A session with git state and no branch (a detached HEAD) draws no flags on its header, so its
/// rows carry none: `\u{b1}1` finds web's rows while web is on a branch and not once it is not.
#[test]
fn a_session_with_no_branch_passes_no_git_flags_to_its_rows() {
    let rows_of_web = |snap: &Snapshot| -> usize {
        let rows = shown(snap, Depth::Agents, Layout::Tree, "claude \u{b1}1");
        lines(snap, &rows)
            .iter()
            .filter(|line| line.starts_with("web claude"))
            .count()
    };
    let snap = snapshot();
    assert_eq!(
        rows_of_web(&snap),
        1,
        "a branch: the header draws the flags"
    );
    let mut detached = snapshot();
    detached.spaces[1].git.as_mut().expect("git").branch = None;
    assert_eq!(rows_of_web(&detached), 0);
}

/// At the sessions depth a header found by its name stands for the agents an installed view shows:
/// the agent it hides is none of the agents `Tab` walks, and a hidden agent's drawn title still finds
/// the header, which then stands for no agent.
#[test]
fn an_agent_a_view_hides_is_none_a_found_header_stands_for() {
    let view = r#"{"source":"cli","filter":{"op":"eq","field":"agent","value":"codex"}}"#;
    let hidden = snapshot_with_view(view);
    let headers = |snap: &Snapshot, query: &str| {
        let rows = shown(snap, Depth::Sessions, Layout::Tree, query);
        (lines(snap, &rows), rows)
    };
    // Infra's claude agents are hidden by the view: its name still finds the header, which stands
    // for none of them.
    let (found, rows) = headers(&hidden, "infra");
    assert_eq!(found, vec!["# infra"]);
    assert!(!rows.stands_for.contains_key(&2), "{:?}", rows.stands_for);
    let (_, rows) = headers(&snapshot(), "infra");
    assert_eq!(rows.stands_for[&2].len(), 2);
    // Web's codex, the agent the view shows, stands alone for the header found by its name.
    let (selected, rows) = headers(&hidden, "web");
    assert_eq!(selected, vec!["# web"]);
    assert_eq!(rows.stands_for[&1].len(), 1);
    // A hidden hot agent's title is still drawn on the header, so it still finds it, standing for none.
    let (selected, rows) = headers(&hidden, "fixes");
    assert_eq!(selected, vec!["# infra"]);
    assert!(rows.stands_for.is_empty());
}

#[test]
fn a_suspended_agents_row_says_suspended_where_it_says_its_state() {
    let mut snap = crate::tui::fixtures::snapshot();
    let (index, agent) = snap
        .agents
        .iter()
        .enumerate()
        .find(|(_, agent)| agent.state.is_some())
        .map(|(index, agent)| (index, agent.clone()))
        .expect("an agent with a state");
    let pane = snap
        .panes
        .iter()
        .find(|pane| pane.agent == Some(index))
        .cloned()
        .expect("its pane");
    assert_eq!(
        crate::tui::tree::state_label(&snap, &pane),
        agent.state.unwrap().to_string()
    );
    snap.agents[index].suspended = true;
    assert_eq!(
        crate::tui::tree::state_label(&snap, &pane),
        crate::store::SUSPENDED_WORD
    );
}
