//! The popup's rows: one tree of sessions, their panes, and the worktrees they parked.
//!
//! herdr's two panels become a single tree: every tmux session is a
//! header, under it every pane in tmux's own order — agents and plain shells alike — and
//! then the git worktrees cut from that session that no session is currently running.
//! `depth` decides how far down that goes, `layout` whether the headers are drawn at all,
//! and a query filters it with fzf's syntax. With `[ui] projects_in_filter` on, the
//! directories the filter lists follow every one of those rows ([`append_projects`]).
//!
//! This is the pure layer: rows in, rows out, no selection, focus or drawing. A [`Row`] is
//! only ever indices into the [`Snapshot`] and the worktree slice it was built from, so a
//! row costs nothing to copy and cannot go stale in a way the renderer can't notice.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::attention;
use crate::config::{AgentPanelSort, Depth, Layout};
use crate::session::worktree::WorktreeRecord;
use crate::tmux::ids;
use crate::tui::agent_view::AgentView;
use crate::tui::app::JUMP_LETTERS;
use crate::tui::data::{PaneRow, Snapshot, git_status_text, strip_title_glyph};

/// One line of the popup's tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Row {
    /// A session header: an index into [`Snapshot::spaces`].
    Session(usize),
    /// One pane of that session, agent or plain shell: an index into [`Snapshot::panes`].
    Pane(usize),
    /// A git worktree with no session running at its path: `space` indexes
    /// [`Snapshot::spaces`] (the session it was cut from), `record` the worktree slice.
    ClosedWorktree { space: usize, record: usize },
    /// A directory the `/` filter offers to open (`[ui] projects_in_filter`): an index
    /// into the popup's own list of them, which [`build`] knows nothing about. They come
    /// after every other row ([`append_projects`]) and belong to no session.
    Project(usize),
    /// The line over one machine's sessions, drawn only while [`Snapshot::machines`] is not empty:
    /// `None` is this machine's own, `Some(i)` an index into [`Snapshot::machines`]. A header is a
    /// line and no place for the cursor: nothing selects it and no label names it.
    Machine(Option<usize>),
    /// The one row under the header of a connected machine that has no tmux session: an index into
    /// [`Snapshot::machines`]. The cursor rests on it and Enter starts a session there; it has no label, and
    /// neither `Tab` nor a number reaches it.
    NoSession(usize),
}

/// One row's match against the query: its score, and the character positions within
/// [`row_text`] that the query matched. A transparent alias, so
/// [`Rows::scores`] is still a plain `Vec<Option<(i64, Vec<usize>)>>`.
pub type Match = (i64, Vec<usize>);

/// One row paired with its match, while [`build`] is still deciding which rows to keep.
type Scored = (Row, Option<Match>);

/// The rows one frame draws, and how each one matched the query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    pub rows: Vec<Row>,
    /// Parallel to [`Rows::rows`]: the match score and the character positions of the
    /// match within that row's [`row_text`], or `None` when there is no query — or when
    /// the row itself did not match and is only here because its header or one of its
    /// children did. A header folded at the sessions depth scores the best of its own match
    /// and its agents', and lights only its own.
    pub scores: Vec<Option<Match>>,
    /// Under a query at the sessions depth, what each header on show stands for: the session's
    /// index into [`Snapshot::spaces`] and the indexes into [`Snapshot::agents`] of the agents
    /// the query would show under it at the agents depth (all of them when the header's own words
    /// match, else the ones that match), which `Tab` walks. Empty with no query and at every
    /// other depth.
    pub stands_for: HashMap<usize, Vec<usize>>,
}

impl Rows {
    /// The position of the section's first row, when there is a section: the directories
    /// the filter lists are always last, so this is where they start.
    pub fn first_project(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Project(_)))
    }
}

/// The popup's filter, in fzf's query syntax: space-separated terms, all of which must
/// match, with `'exact`, `^prefix`, `suffix$` and `!inverted` terms, smart case but for a
/// term that is an address (`4A`), which ignores case ([`parse`]). It matches
/// what a row draws, its labels included ([`match_text`]): `3` matches session 3's number and
/// the `3` in `3:tests` alike, and `4a` the address of session 4's row a. A query that is
/// exactly a drawn label with a number in it, in any case (`4`, `4a`, `4A`), names that row:
/// it stays in view and scores above every other: the search covers everything that is drawn
/// on the screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
}

/// The label one row wears: its number, its letter, and the address they spell, the text the
/// query matches in front of the row's own ([`match_text`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Label {
    /// The row's own number, 1 to 9: a header's in a tree, a row's in a flat list.
    pub number: Option<u8>,
    /// The row's own letter, `a` to `e`: a child's in a tree.
    pub letter: Option<char>,
    /// What is drawn for the row, as the reader would type it: a header's or a flat row's
    /// number as its key spells it (`4`), a child's session number followed by its letter
    /// (`4a`), the letter alone when its session has no number (a tenth session's `a`), and
    /// empty for a row with neither.
    pub address: String,
    /// Whether [`Label::address`] has a number in it: only such an address is a name a query
    /// can be exactly ([`Query`]); a letter alone is text like any other.
    pub numbered: bool,
}

/// Every row's [`Label`], worked out once from the rows as they are with nothing typed
/// ([`Labels::new`]), so a filter only hides rows: a hidden row takes its label with it and no
/// other row gets it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    of: HashMap<Row, Label>,
}

impl Labels {
    /// The labels of `rows`, the rows [`build`] made with no query. In a flat list the first
    /// nine rows that are not parked worktrees (nor directories of the filter's) are numbered, in
    /// the order drawn, and no row has a letter; in a tree a header has its session's number
    /// ([`session_label`]), one further on for each earlier listed machine with no session, which
    /// holds a place whether it is connected or not and draws its number on its row that starts a
    /// session ([`Row::NoSession`]) while that row is shown, and the first five children of each session the letters `a` to `e`
    /// in the order drawn, but for any not in `letters` (a `[keys]` entry gives it to an action,
    /// or the letters are not drawn at all): the row it would have named goes without, and the
    /// rows after it keep their places. `spell` is how a number is drawn (`Keymap::label`).
    pub fn new(
        snap: &Snapshot,
        rows: &Rows,
        flat: bool,
        letters: &[char],
        spell: impl Fn(u8) -> String,
    ) -> Labels {
        let mut of = HashMap::new();
        if flat {
            let mut count = 0usize;
            for row in &rows.rows {
                if matches!(row, Row::ClosedWorktree { .. } | Row::Project(_)) {
                    continue;
                }
                count += 1;
                if count <= 9 {
                    let n = count as u8;
                    of.insert(
                        row.clone(),
                        Label {
                            number: Some(n),
                            letter: None,
                            address: spell(n),
                            numbered: true,
                        },
                    );
                }
            }
            return Labels { of };
        }
        // A listed machine with no session (not this machine's own server, not one no app keeps)
        // holds a place in the numbers, as a session does, whether it is connected, still
        // connecting or away, so each session of a later machine numbers one on and a link that
        // comes and goes renumbers nothing after it. Its number is drawn on the row that starts a
        // session there, while that row is shown.
        let empties: Vec<usize> = snap
            .machines
            .iter()
            .enumerate()
            .filter(|&(machine, header)| {
                !header.this_server
                    && !header.no_keeper
                    && !snap.spaces.iter().any(|s| s.machine == Some(machine))
            })
            .map(|(machine, _)| machine)
            .collect();
        let number_of = |space: usize| {
            let shift = snap
                .spaces
                .get(space)
                .and_then(|s| s.machine)
                .map_or(0, |machine| {
                    empties.iter().filter(|&&m| m < machine).count()
                });
            session_label(space + shift)
        };
        let mut places: HashMap<usize, usize> = HashMap::new();
        for row in &rows.rows {
            match row {
                Row::NoSession(machine) => {
                    let before = snap
                        .spaces
                        .iter()
                        .filter(|s| s.machine.is_none_or(|m| m < *machine))
                        .count()
                        + empties.iter().filter(|&&m| m < *machine).count();
                    if let Some(n) = session_label(before) {
                        of.insert(
                            row.clone(),
                            Label {
                                number: Some(n),
                                letter: None,
                                address: spell(n),
                                numbered: true,
                            },
                        );
                    }
                }
                Row::Session(space) => {
                    if let Some(n) = number_of(*space) {
                        of.insert(
                            row.clone(),
                            Label {
                                number: Some(n),
                                letter: None,
                                address: spell(n),
                                numbered: true,
                            },
                        );
                    }
                }
                Row::Project(_) | Row::Machine(_) => {}
                child => {
                    let Some(space) = space_of(snap, child) else {
                        continue;
                    };
                    let place = places.entry(space).or_insert(0);
                    let letter = JUMP_LETTERS.chars().nth(*place);
                    *place += 1;
                    let Some(letter) = letter.filter(|l| letters.contains(l)) else {
                        continue;
                    };
                    let number = number_of(space);
                    let mut address = number.map(&spell).unwrap_or_default();
                    address.push(letter);
                    of.insert(
                        row.clone(),
                        Label {
                            number: None,
                            letter: Some(letter),
                            address,
                            numbered: number.is_some(),
                        },
                    );
                }
            }
        }
        Labels { of }
    }

    /// `row`'s label, `None` for a row that wears none.
    pub fn get(&self, row: &Row) -> Option<&Label> {
        self.of.get(row)
    }

    /// `row`'s address ([`Label::address`]), empty for a row that wears no label.
    pub fn address(&self, row: &Row) -> &str {
        self.get(row).map_or("", |label| label.address.as_str())
    }
}

/// The rows to draw, in order, with each row's match against `query`.
///
/// A tree keeps tmux's order at every depth — `sort` is a flat-layout setting, and the
/// cursor, not the list, is what moves to the agent that needs attention. A flat layout
/// drops the headers and puts agents first (in `sort`'s order), then plain panes in tmux
/// order, then the parked worktrees.
///
/// With a query, a header stays when it or any of its children matched, a child stays when
/// it or its header matched, and a session that matched nowhere disappears entirely. A
/// flat layout has no headers to inherit from, so each row stands on its own match, and
/// the best match sorts first.
///
/// `show_git` is the same flag the renderer draws with, so the query only ever matches text
/// the reader can see: with it off, a header's branch and status flags are neither drawn nor
/// searchable. `titles` (`[ui] titles`) is the same kind of flag: with it off an agent's title
/// is neither drawn nor found ([`drawn_title`]).
///
/// An Agents view installed in the snapshot ([`Snapshot::view`]) is applied before any of
/// that — see [`project`] and [`view_ranks`].
///
/// `labels` are the labels the rows wear ([`Labels::new`], made from these rows with no query),
/// which the query matches in front of each row's own text ([`match_text`]); a row whose
/// address the query is exactly ([`Query`]) stays, scored above every other. Empty labels (no
/// labels drawn) add nothing.
#[allow(clippy::too_many_arguments)] // the rows are these nine facts; a struct would only rename them
pub fn build(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    depth: Depth,
    layout: Layout,
    sort: AgentPanelSort,
    show_git: bool,
    titles: bool,
    labels: &Labels,
    query: Option<&Query>,
) -> Rows {
    build_drawn(
        snap,
        worktrees,
        depth,
        layout,
        sort,
        show_git,
        titles,
        labels,
        &|_| None,
        query,
    )
}

/// What a configured `rows` template draws on a row, `None` for a row drawn its default way
/// ([`crate::tui::rows::drawn_text`]).
pub type Drawn<'d> = dyn Fn(&Row) -> Option<String> + 'd;

/// [`build`] for rows some configured `rows` template draws: `drawn` is the plain text a row's
/// template draws ([`crate::tui::rows::drawn_text`], `None` for a row drawn its default way),
/// which the query matches after the rest of the row's text and lights nothing of. A row a
/// template applies to is found by what it draws and nothing else, even when that is nothing.
///
/// A child row (a pane or a closed worktree) carries its header's branch and git flags, so the
/// words of a query can be split between a session and its rows (`release claude`) in a tree,
/// where the header is drawn above them; a flat list has no header, so a row there is found only
/// by what it draws itself. The words come last in a child's text, where the row draws none of them. A
/// header that already matched carries all its children, so its children only keep a score of
/// their own while they match without those words (the cursor stays on the header). At the
/// sessions depth a header is found only by the words it draws: its own and the title of its hot
/// agent ([`push_session`]); [`Rows::stands_for`] names the agents it stands for.
#[allow(clippy::too_many_arguments)] // the rows are these ten facts; a struct would only rename them
pub fn build_drawn(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    depth: Depth,
    layout: Layout,
    sort: AgentPanelSort,
    show_git: bool,
    titles: bool,
    labels: &Labels,
    drawn: &Drawn<'_>,
    query: Option<&Query>,
) -> Rows {
    let closed = closed_worktrees(snap, worktrees);
    // A query of only whitespace parses to no terms, which would match everything with the
    // same score: treat it as no query at all rather than a filter that re-sorts nothing.
    let mut scorer = query.filter(|q| !q.text.trim().is_empty()).map(|q| {
        Scorer::new(q)
            .with_labels(labels)
            .with_titles(titles)
            .with_drawn(drawn)
    });
    match layout {
        Layout::Flat if depth != Depth::Sessions => {
            build_flat(snap, worktrees, &closed, depth, sort, show_git, &mut scorer)
        }
        _ => build_tree(snap, worktrees, &closed, depth, show_git, &mut scorer),
    }
}

/// Appends the directories the filter lists, one [`Row::Project`] each, in the order
/// given, after every row [`build`] made. An empty list appends nothing, so a section with
/// nothing in it has no first row to hang a header on.
///
/// A directory is listed by its path only: each row's score is how `query` matched it (positions
/// in the path, for the highlight). The tag its row draws beside it (`running`, `zoxide`,
/// `project dir`) is a category and is neither matched nor lit, or a short query would match
/// every directory of that kind. Whether a directory is listed at all is the project picker's
/// call, made before it gets here, and a directory never competes with a session or an agent
/// for the cursor (see `App::cursor_to_best_match`).
pub fn append_projects<'a>(
    rows: &mut Rows,
    paths: impl IntoIterator<Item = &'a str>,
    query: Option<&Query>,
) {
    let mut paths = paths.into_iter().peekable();
    // Nothing to append is nothing to score: no matcher is built for it.
    if paths.peek().is_none() {
        return;
    }
    let mut scorer = query.filter(|q| !q.text.trim().is_empty()).map(Scorer::new);
    for (index, path) in paths.enumerate() {
        rows.rows.push(Row::Project(index));
        rows.scores
            .push(scorer.as_mut().and_then(|scorer| scorer.score(path)));
    }
}

/// Sessions in [`Snapshot::spaces`] order, each followed by its children. With another machine listed
/// ([`Snapshot::has_other_machines`]) the sessions are grouped under a [`Row::Machine`] header each: this
/// machine's first, then every machine's in the config's order, which is the order their sessions
/// were appended in.
///
/// A header is there while its machine has a row drawn, or has no session at all and nothing is
/// being filtered (a machine just connected, or dropped before it said anything): a filter shows a
/// header only over machines that have a match (or whose row that starts a session the query names
/// exactly by its number), and a view that took every session of a machine leaves no header over
/// nothing.
fn build_tree(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    closed: &[(usize, usize)],
    depth: Depth,
    show_git: bool,
    scorer: &mut Option<Scorer<'_>>,
) -> Rows {
    let filtering = scorer.is_some();
    let mut out = Rows::default();
    // No machine but this one's own tmux server listed is one machine: no header over it, as with none listed.
    if !snap.has_other_machines() {
        for space in (0..snap.spaces.len()).filter(|&space| snap.spaces[space].machine.is_none()) {
            push_session(
                &mut out, snap, worktrees, closed, space, depth, show_git, scorer,
            );
        }
        return out;
    }
    let groups = std::iter::once(None).chain((0..snap.machines.len()).map(Some));
    for group in groups {
        let mine: Vec<usize> = (0..snap.spaces.len())
            .filter(|&space| snap.spaces[space].machine == group)
            .collect();
        let mut block = Rows::default();
        for &space in &mine {
            push_session(
                &mut block, snap, worktrees, closed, space, depth, show_git, scorer,
            );
        }
        // A machine that is connected, has said its state, and has no session: the row that starts
        // one. While filtering it stays only when the query is exactly its number, scored above
        // every other, as a session's number does: what is drawn can be typed.
        let no_session = group
            .filter(|&machine| {
                mine.is_empty()
                    && snap.machines.get(machine).is_some_and(|header| {
                        header.live && header.stated && !header.this_server && !header.no_keeper
                    })
            })
            .map(Row::NoSession)
            .and_then(|row| match scorer.as_ref() {
                None => Some((row, None)),
                Some(scorer) => scorer.named(&row).map(|named| (row, Some(named))),
            });
        if !block.rows.is_empty() || (mine.is_empty() && !filtering) || no_session.is_some() {
            out.rows.push(Row::Machine(group));
            out.scores.push(None);
            if let Some((row, score)) = no_session {
                out.rows.push(row);
                out.scores.push(score);
            }
        }
        out.rows.extend(block.rows);
        out.scores.extend(block.scores);
        out.stands_for.extend(block.stands_for);
    }
    out
}

/// One session's header and the children it keeps, onto `out`: nothing for a session a view
/// emptied or a filter found no match in.
///
/// At the sessions depth the header folds its agents, and under a query it is found by the words it
/// draws and no others: its name, branch and git flags, its machine's name and the title of the agent
/// that needs you, which it draws after them. What its folded agents are (their kind, state and place)
/// is not on it, so it finds nothing. [`Rows::stands_for`] says which agents a found header stands for:
/// all those a view shows when its own words found it, the hot agent when only its title did. A
/// match in the title lights nothing on the header.
#[allow(clippy::too_many_arguments)] // the facts `build_tree` has, passed down; a struct would only rename them
fn push_session(
    out: &mut Rows,
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    closed: &[(usize, usize)],
    space: usize,
    depth: Depth,
    show_git: bool,
    scorer: &mut Option<Scorer<'_>>,
) {
    let filtering = scorer.is_some();
    let children = child_rows(snap, closed, space, depth);
    let before = children.len();
    let children = project(snap, children);
    // A session the view emptied goes with its agents; one it took nothing from reads
    // exactly as it does with no view installed, bare header and all.
    if children.is_empty() && before > 0 {
        return;
    }
    let head = Row::Session(space);
    // The header's own words: its name, branch, git flags and machine.
    let own = score_row(scorer, snap, worktrees, &head, show_git, true);
    let head_matched = own.is_some();
    let kids = score_kids(scorer, snap, worktrees, children, show_git, head_matched);
    let mut head_score = own;
    let mut standing = Vec::new();
    if filtering && depth == Depth::Sessions {
        // A folded header is found by what it draws and nothing else: those words and the title of
        // its hot agent, which it draws after them ([`hot_agent`]). The agents' kinds, states and
        // places are not on it.
        let drawn = score_header_by_title(scorer, snap, worktrees, space, show_git);
        // A header its own words found stands for what it stands for with no filter, all the agents
        // a view shows; one only the drawn title found, for that agent.
        standing = match (&head_score, &drawn) {
            (Some(_), _) => shown_agents(snap, space),
            (None, Some(_)) => hot_agent(snap, space)
                .filter(|agent| shown_agents(snap, space).contains(agent))
                .into_iter()
                .collect(),
            (None, None) => Vec::new(),
        };
        // The title is on the header too, so a term it has (`!flaky`) drops a header the words
        // alone would keep.
        head_score = match (head_score, drawn) {
            (Some(_), None) => None,
            (Some((own, at)), Some((drawn, _))) => Some((own.max(drawn), at)),
            (None, Some((drawn, _))) => Some((drawn, Vec::new())),
            (None, None) => None,
        };
        if head_score.is_none() {
            standing.clear();
        }
    }
    if filtering && head_score.is_none() && !kids.iter().any(|(_, m)| m.is_some()) {
        return;
    }
    if !standing.is_empty() {
        out.stands_for.insert(space, standing);
    }
    out.rows.push(head);
    out.scores.push(head_score);
    for (row, score) in kids {
        // A matching header carries all its children: finding a session by its number,
        // name, branch or git flags shows its whole tree.
        if !filtering || head_matched || score.is_some() {
            out.rows.push(row);
            out.scores.push(score);
        }
    }
}

/// Every session's children, without the headers.
fn build_flat(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    closed: &[(usize, usize)],
    depth: Depth,
    sort: AgentPanelSort,
    show_git: bool,
    scorer: &mut Option<Scorer<'_>>,
) -> Rows {
    let filtering = scorer.is_some();
    let children: Vec<Row> = (0..snap.spaces.len())
        .flat_map(|space| child_rows(snap, closed, space, depth))
        .collect();
    let children = project(snap, children);
    // A flat list draws no header, so a row carries none of its words: what a row shows is its
    // own line.
    if let Some(scorer) = scorer.as_mut() {
        scorer.flat = true;
    }
    let mut scored = score_rows(scorer, snap, worktrees, children, show_git, false);
    if let Some(scorer) = scorer.as_mut() {
        scorer.flat = false;
    }
    if filtering {
        scored.retain(|(_, m)| m.is_some());
        // Best match first. The sort is stable, so equal scores keep tmux order.
        scored.sort_by_key(|(_, m)| Reverse(m.as_ref().map_or(0, |(score, _)| *score)));
    } else {
        let order = space_order(snap);
        let ranks = view_ranks(snap);
        scored.sort_by_key(|(row, _)| flat_key(snap, &order, row, sort, ranks.as_deref()));
    }
    // The rows of a machine that is away go after every live one, in the order they had.
    scored.sort_by_key(|(row, _)| {
        space_of(snap, row)
            .and_then(|space| snap.spaces.get(space))
            .is_some_and(|space| snap.greyed(space.machine))
    });
    let (rows, scores) = scored.into_iter().unzip();
    Rows {
        rows,
        scores,
        stands_for: HashMap::new(),
    }
}

/// The agents a session's rows show at the agents depth, as indexes into [`Snapshot::agents`]:
/// the ones an installed view selects. A session's header folds exactly these at the sessions
/// depth, and a view hides an agent from every depth's walk.
pub fn shown_agents(snap: &Snapshot, space: usize) -> Vec<usize> {
    project(snap, child_rows(snap, &[], space, Depth::Agents))
        .iter()
        .filter_map(|row| agent_of(snap, row).map(|(agent, _)| agent))
        .collect()
}

/// A session's first agent that needs you, in tmux order (window, pane), as an index into
/// [`Snapshot::agents`]: the agent whose title a folded header draws.
pub fn hot_agent(snap: &Snapshot, space: usize) -> Option<usize> {
    let session = &snap.spaces.get(space)?.session_id;
    snap.agents
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            a.session_id == *session && !snap.greyed(a.machine) && attention::hot(a.state, a.seen)
        })
        .min_by_key(|(_, a)| (a.window_index, a.pane_index))
        .map(|(i, _)| i)
}

/// The title a folded header draws ([`hot_agent`]'s, its glyph taken off), `None` when it draws
/// none: `[ui] titles` is off, no agent needs you, or the title is empty.
fn header_title(snap: &Snapshot, space: usize, titles: bool) -> Option<String> {
    if !titles {
        return None;
    }
    let title = strip_title_glyph(&snap.agents.get(hot_agent(snap, space)?)?.pane_title);
    (!title.is_empty()).then_some(title)
}

/// The children of one session, in tmux order: its panes — agents at any depth past
/// [`Depth::Sessions`], plain shells only at [`Depth::All`] — then its parked worktrees.
fn child_rows(snap: &Snapshot, closed: &[(usize, usize)], space: usize, depth: Depth) -> Vec<Row> {
    if depth == Depth::Sessions {
        return Vec::new();
    }
    let Some(session_id) = snap.spaces.get(space).map(|s| s.session_id.as_str()) else {
        return Vec::new();
    };
    // `Snapshot::panes` is already session, window, pane order, which is what tmux itself
    // would list.
    let mut out: Vec<Row> = snap
        .panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.session_id == session_id && (depth == Depth::All || p.agent.is_some()))
        .map(|(pane, _)| Row::Pane(pane))
        .collect();
    out.extend(
        closed
            .iter()
            .filter(|(parent, _)| *parent == space)
            .map(|&(space, record)| Row::ClosedWorktree { space, record }),
    );
    out
}

/// `rows` with the agent rows an installed Agents view does not select taken out.
///
/// What a view projects is herdr's own boundary — the agent rows, and nothing else — so a
/// plain pane, a parked worktree and a session header pass through untouched, and with no
/// view installed (or one carrying only a `sort`) every row stays. A pane whose agent the
/// snapshot no longer has is a plain pane here, exactly as it is to [`flat_key`].
fn project(snap: &Snapshot, rows: Vec<Row>) -> Vec<Row> {
    let Some(view) = snap.view.as_ref() else {
        return rows;
    };
    rows.into_iter()
        .filter(|row| match agent_of(snap, row) {
            Some((_, agent)) => view.matches(agent, &snap.current),
            None => true,
        })
        .collect()
}

/// Each agent's place in the order an installed view's own `sort` puts it in, indexed by
/// [`Snapshot::agents`] — `None` when no view is installed or it carries no `sort`, which
/// leaves `[ui] agent_panel_sort` in charge.
///
/// herdr's rule: for as long as a view lasts, its `sort` stands in for the configured one,
/// without rewriting the config. It orders agents only, so [`flat_key`] still puts every
/// agent ahead of the plain panes and the parked worktrees.
fn view_ranks(snap: &Snapshot) -> Option<Vec<usize>> {
    let view = snap.view.as_ref().filter(|v| !v.sort.is_empty())?;
    let mut order: Vec<usize> = (0..snap.agents.len()).collect();
    AgentView::sort(view, &snap.agents, &mut order);
    let mut ranks = vec![usize::MAX; snap.agents.len()];
    for (rank, &agent) in order.iter().enumerate() {
        ranks[agent] = rank;
    }
    Some(ranks)
}

/// The agent occupying a row's pane — its index into [`Snapshot::agents`] and its row —
/// when the row is a pane and an agent the snapshot still has is in it.
fn agent_of<'a>(snap: &'a Snapshot, row: &Row) -> Option<(usize, &'a crate::tui::data::AgentRow)> {
    let Row::Pane(pane) = row else { return None };
    let agent = snap.panes.get(*pane)?.agent?;
    Some((agent, snap.agents.get(agent)?))
}

/// The worktrees no session is running, each paired with the session it was cut from:
/// `(index into `[`Snapshot::spaces`]`, index into `worktrees`)`, in `worktrees` order.
///
/// A record whose path a session is sitting in is open, not parked — that session's own
/// header already represents it. A record whose parent session is gone is dropped: there is
/// no header to hang it under.
///
/// A record of another machine ([`WorktreeRecord::machine`]) is judged among that machine's
/// sessions alone: its paths are paths there, and a session of this machine, or of another, at the
/// same path says nothing of it.
pub fn closed_worktrees(snap: &Snapshot, worktrees: &[WorktreeRecord]) -> Vec<(usize, usize)> {
    let running = running_paths(snap);
    worktrees
        .iter()
        .enumerate()
        .filter(|(_, record)| !is_running(snap, &running, record))
        .filter_map(|(record, r)| parent_space(snap, &running, r).map(|space| (space, record)))
        .collect()
}

/// The indexes into `worktrees` of the records a session is running: the ones [`closed_worktrees`] leaves
/// out because that session's own header represents them, in `worktrees` order. A record whose parent
/// session is gone is still open when a session sits in it.
pub fn open_worktrees(snap: &Snapshot, worktrees: &[WorktreeRecord]) -> Vec<usize> {
    let running = running_paths(snap);
    worktrees
        .iter()
        .enumerate()
        .filter(|(_, record)| is_running(snap, &running, record))
        .map(|(at, _)| at)
        .collect()
}

/// Whether a session of the record's own machine is sitting in the record's folder. `running` is
/// [`running_paths`].
fn is_running(snap: &Snapshot, running: &[String], record: &WorktreeRecord) -> bool {
    let path = path_on(record.machine, record.path_to_match());
    snap.spaces
        .iter()
        .zip(running)
        .any(|(space, running)| space.machine == record.machine && *running == path)
}

/// Every session's path in [`path_on`] form, in [`Snapshot::spaces`] order.
fn running_paths(snap: &Snapshot) -> Vec<String> {
    snap.spaces
        .iter()
        .map(|s| path_on(s.machine, s.real_path.as_deref().unwrap_or(&s.session_path)))
        .collect()
}

/// `path` as one comparable string on machine `machine`: [`comparable`] on this machine, whose
/// files are here to resolve, and as given on any other, whose files are not.
///
/// On another machine the caller gives the path resolved there when the feed sent one
/// ([`WorktreeRecord::path_to_match`], [`SpaceRow::real_path`]): the feed resolves the session's folder
/// and the record's folders on the machine, where the files are, so a link there (a remote Mac's `/var`
/// and `/private/var`, a project folder linked to another disk) cannot make one directory two paths.
/// A feed that sent none leaves the path as the machine wrote it.
fn path_on(machine: Option<usize>, path: &Path) -> String {
    if machine.is_none() {
        comparable(path)
    } else {
        path.display().to_string()
    }
}

/// The session a worktree hangs under: the one whose public workspace id the record was
/// created from, or — failing that — whichever session is sitting in the repository the
/// worktree came out of.
///
/// The recorded id is only `$2` -> `w2`, and tmux hands those ids out afresh every server
/// restart while `worktrees.json` persists, so `w2` alone is not evidence: after a restart
/// it usually names a different repository altogether. An id match therefore only counts
/// when that session is actually sitting in the recorded repository — `repo_root` comes from
/// `git rev-parse --show-toplevel` in the source session, so the source session's path is
/// `repo_root` or below it by construction. Anything else falls through to the path lookup.
///
/// Only the sessions of the record's own machine are looked at ([`closed_worktrees`]), and a
/// remote session's id is read as its machine knows it (`studio:$3` is `$3`, workspace `w3`).
///
/// `running` is [`running_paths`].
fn parent_space(snap: &Snapshot, running: &[String], record: &WorktreeRecord) -> Option<usize> {
    let root = path_on(record.machine, record.repo_root_to_match());
    let here = |space: usize| snap.spaces.get(space).map(|s| s.machine) == Some(record.machine);
    snap.spaces
        .iter()
        .position(|s| {
            s.machine == record.machine
                && ids::workspace_id(crate::machine::split_id(&s.session_id).1)
                    == record.source_workspace_id
        })
        .filter(|&space| running.get(space).is_some_and(|path| inside(path, &root)))
        .or_else(|| (0..running.len()).find(|&space| here(space) && running[space] == root))
}

/// Whether `path` is `root` itself or a directory inside it. Both are [`comparable`] forms,
/// so this is a plain prefix test on a `/`-separated boundary.
fn inside(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `path` as one comparable string: resolved when it exists (so a symlinked `/tmp` and
/// `/private/tmp` are one directory), and as written when it does not.
fn comparable(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Each session's position in [`Snapshot::spaces`], by `session_id`.
fn space_order(snap: &Snapshot) -> HashMap<&str, usize> {
    snap.spaces
        .iter()
        .enumerate()
        .map(|(i, s)| (s.session_id.as_str(), i))
        .collect()
}

/// A flat row's sort key: its machine first (this machine's rows, then each machine's in the
/// config's order), then the kind (agents, plain panes, parked worktrees), then attention (only under
/// [`AgentPanelSort::Attention`], and only for agents), then tmux's own order.
///
/// The machine leads so that a machine that connects, drops or comes back moves no number of this
/// machine's and no row of another: the rows of a machine that is away are put last afterwards, by a
/// stable sort, so they keep this order among themselves. With no machine listed every row has the
/// same first component and the order is what it was.
type FlatKey = (usize, u8, u8, Reverse<u64>, usize, u32, u32);

/// A row's machine as the first component of its [`FlatKey`]: `0` for this machine's, a machine's
/// index plus one for any other's.
fn flat_machine(snap: &Snapshot, row: &Row) -> usize {
    let machine = match row {
        Row::Machine(machine) => *machine,
        Row::NoSession(machine) => Some(*machine),
        Row::Pane(pane) => snap.panes.get(*pane).and_then(|p| p.machine),
        _ => space_of(snap, row)
            .and_then(|space| snap.spaces.get(space))
            .and_then(|space| space.machine),
    };
    machine.map_or(0, |machine| machine + 1)
}

/// `ranks` is [`view_ranks`]: when a view installed a `sort`, an agent is ordered by its
/// place in that order alone, within its machine (the machine comes first in the key) — the
/// whole point of the override — and every other row is keyed exactly as it is without one.
fn flat_key(
    snap: &Snapshot,
    order: &HashMap<&str, usize>,
    row: &Row,
    sort: AgentPanelSort,
    ranks: Option<&[usize]>,
) -> FlatKey {
    let (kind, attention, since, space, window, pane) = kind_key(snap, order, row, sort, ranks);
    (
        flat_machine(snap, row),
        kind,
        attention,
        since,
        space,
        window,
        pane,
    )
}

/// [`flat_key`] without its machine.
fn kind_key(
    snap: &Snapshot,
    order: &HashMap<&str, usize>,
    row: &Row,
    sort: AgentPanelSort,
    ranks: Option<&[usize]>,
) -> (u8, u8, Reverse<u64>, usize, u32, u32) {
    match row {
        // Headers never appear in a flat layout; sorting one by its own position is the
        // answer that cannot reorder anything else.
        Row::Session(space) => (0, 0, Reverse(0), *space, 0, 0),
        Row::Pane(pane) => {
            let Some(p) = snap.panes.get(*pane) else {
                return (1, 0, Reverse(0), usize::MAX, 0, 0);
            };
            // A pane whose session `list-sessions` did not report sorts after every known
            // session rather than first.
            let space = order
                .get(p.session_id.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            let position = (space, p.window_index, p.pane_index);
            match agent_of(snap, row) {
                Some((agent, _)) if let Some(ranks) = ranks => {
                    let rank = ranks.get(agent).copied().unwrap_or(usize::MAX);
                    (0, 0, Reverse(0), rank, 0, 0)
                }
                Some((_, a)) if sort == AgentPanelSort::Attention => (
                    0,
                    attention::tier(a.state, a.seen),
                    Reverse(a.state_since),
                    position.0,
                    position.1,
                    position.2,
                ),
                Some(_) => (0, 0, Reverse(0), position.0, position.1, position.2),
                None => (1, 0, Reverse(0), position.0, position.1, position.2),
            }
        }
        Row::ClosedWorktree { space, .. } => (2, 0, Reverse(0), *space, 0, 0),
        // The directories the filter lists are appended after the sort, never sorted in.
        Row::Project(index) => (3, 0, Reverse(0), *index, 0, 0),
        // Headers never appear in a flat layout either.
        Row::Machine(machine) => (0, 0, Reverse(0), machine.unwrap_or(0), 0, 0),
        Row::NoSession(machine) => (0, 0, Reverse(0), *machine, 0, 0),
    }
}

/// A row's own text, which the query matches after the row's address ([`match_text`]).
///
/// What the row draws, so the search covers everything that is drawn on the screen: a header is `"<name> <branch> <git flags>"`, a pane `"<session> <what>
/// <where> <state>"` (an agent in a container is drawn with a box and found by no word of it),
/// a parked worktree `"<session> <branch> worktree, closed"`. Empty parts are left
/// out, and `show_git` false drops the branch and the flags. A header's folder is not in it:
/// the tree never draws it (it titles the preview), and no `[ui.sidebar.spaces] rows` token
/// draws it either. A child row starts with its session's name, drawn on the header above
/// it, so `web claude` finds web's claude row, and ends with the header's branch and git
/// flags, drawn on the header too, so `release claude` finds it when the session is on
/// branch `release` ([`row_words`]). A row indexing something the snapshot
/// no longer has is empty rather than a panic: rows outlive a snapshot by up to one tick.
pub fn row_text(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    row: &Row,
    show_git: bool,
) -> String {
    let (own, inherited) = row_words(snap, worktrees, row, show_git);
    join([own, inherited])
}

/// The words a row has of its own, as [`row_text`] starts: what a match's first positions count
/// into, for a caller that maps them onto what is drawn.
pub fn own_words(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    row: &Row,
    show_git: bool,
) -> String {
    row_words(snap, worktrees, row, show_git).0
}

/// A row's [`row_text`] in two parts: the words the row has of its own, and, for a child row, the
/// words it has of its header's (the header's branch and git flags while `show_git` draws them),
/// which come last. A term anchors (`^`, `$`) in the first alone, and the second lights nothing:
/// the row draws none of those words ([`Scorer::by_terms`]).
fn row_words(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    row: &Row,
    show_git: bool,
) -> (String, String) {
    match row {
        Row::Session(space) => {
            let Some(s) = snap.spaces.get(*space) else {
                return Default::default();
            };
            let mut parts = vec![s.session_name.clone(), header_git(snap, *space, show_git)];
            // The machine's name is found by the filter, and drawn on the machine's own line above
            // the header, not on it: last, so a match on it lights nothing.
            if let Some(machine) = machine_name(snap, s.machine) {
                parts.push(machine.to_string());
            }
            (join(parts), String::new())
        }
        Row::Pane(pane) => {
            let Some(p) = snap.panes.get(*pane) else {
                return Default::default();
            };
            let parts = vec![
                session_name(snap, &p.session_id),
                what_label(snap, p),
                where_label(p),
                state_label(snap, p),
            ];
            let inherited = space_of(snap, row)
                .map(|space| header_git(snap, space, show_git))
                .unwrap_or_default();
            (join(parts), inherited)
        }
        // A directory's text is its path, which only the popup's list of them knows.
        Row::Project(_) => Default::default(),
        // A header is no row to find: the sessions under it carry its machine's name.
        Row::Machine(_) | Row::NoSession(_) => Default::default(),
        Row::ClosedWorktree { space, record } => (
            join([
                space_name(snap, *space),
                worktrees
                    .get(*record)
                    .map(|r| r.branch.clone())
                    .unwrap_or_default(),
                "worktree, closed".to_string(),
            ]),
            header_git(snap, *space, show_git),
        ),
    }
}

/// The words a row has when its configured template draws it ([`Scorer::row`]), as [`row_words`]
/// gives them: the template's text is the row's, so a default word the template leaves out is
/// not here. A header keeps the machine's name, drawn on the machine's own line above it; a
/// pane row has none of its own, and the words of its header it carries start with its
/// session's name (with the machine's) and its branch and git flags, for a default header that
/// draws them.
fn templated_words(snap: &Snapshot, row: &Row, show_git: bool) -> (String, String) {
    match row {
        Row::Session(space) => {
            let machine = snap
                .spaces
                .get(*space)
                .and_then(|s| machine_name(snap, s.machine))
                .unwrap_or_default();
            (machine.to_string(), String::new())
        }
        Row::Pane(pane) => {
            let Some(p) = snap.panes.get(*pane) else {
                return Default::default();
            };
            let git = space_of(snap, row)
                .map(|space| header_git(snap, space, show_git))
                .unwrap_or_default();
            (
                String::new(),
                join([session_name(snap, &p.session_id), git]),
            )
        }
        _ => Default::default(),
    }
}

/// The branch and git flags a session's default header draws, `"<branch> <flags>"`: empty when
/// `show_git` is off, the session has no branch (the header draws the flags beside the branch
/// only) or the space is gone. The one place that says it, for the header's own words and every
/// row that carries them ([`Scorer::header_words`] when a template draws the header instead).
fn header_git(snap: &Snapshot, space: usize, show_git: bool) -> String {
    let Some(git) = snap.spaces.get(space).and_then(|s| s.git.as_ref()) else {
        return String::new();
    };
    let Some(branch) = git.branch.clone().filter(|_| show_git) else {
        return String::new();
    };
    join([branch, git_status_text(git)])
}

/// The title an agent's row draws after its state word, `None` when it draws none: `[ui] titles` is
/// off, the row is no agent's, the title (Claude Code's own glyph taken off, [`strip_title_glyph`])
/// is empty, or it only repeats the *what* column ([`what_label`]). The renderer draws this and the
/// query matches it, so the two cannot disagree about what is on the row.
pub fn drawn_title(snap: &Snapshot, p: &PaneRow, titles: bool) -> Option<String> {
    if !titles {
        return None;
    }
    let agent = snap.agents.get(p.agent?)?;
    let title = strip_title_glyph(&agent.pane_title);
    (!title.is_empty() && title != what_label(snap, p)).then_some(title)
}

/// The title `row` draws ([`drawn_title`]), for a pane row only.
fn title_of(snap: &Snapshot, row: &Row, titles: bool) -> Option<String> {
    match row {
        Row::Pane(pane) => drawn_title(snap, snap.panes.get(*pane)?, titles),
        _ => None,
    }
}

/// Where a title starts among the positions of a match: `text` is what the positions before it
/// count into (a row's own words, or its address and words), and a space comes between. A
/// match in the title lights positions from here on, in the title's own positions. The count is
/// the matcher's ([`Utf32Str`]: a grapheme for some non-ASCII texts, a byte for others), not
/// the characters: [`title_column`] is those.
pub fn title_start(text: &str) -> usize {
    Utf32Str::new(text, &mut Vec::new()).len() + 1
}

/// Where a title starts among the characters a row draws: `text`'s characters and a space. The
/// renderer counts from here once a match is moved from the matcher's count to characters.
pub fn title_column(text: &str) -> usize {
    text.chars().count() + 1
}

/// What the query matches a row against: what it draws, its label first. Its address
/// ([`Label::address`]: a header's number, `"4 "`, a child's session number and letter,
/// `"4a "`) and a space, then its [`row_text`]; a row with no label is its text alone, and with
/// `[ui] labels` off (empty `labels`) nothing is added. The renderer subtracts the address to map
/// a match's positions onto the columns it draws, and lights the label's own characters.
pub fn match_text(
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    row: &Row,
    show_git: bool,
    labels: &Labels,
) -> String {
    labelled(
        labels.address(row),
        row_text(snap, worktrees, row, show_git),
    )
}

/// `text` after `address` and a space, or `text` alone when there is no address.
fn labelled(address: &str, text: String) -> String {
    if address.is_empty() {
        text
    } else {
        format!("{address} {text}")
    }
}

/// Where a pane is: `"<window index>:<window name>"`, plus `".<pane index>"` when the
/// window is split and the index is the only way to tell its panes apart.
pub fn where_label(p: &PaneRow) -> String {
    target(p.window_index, &p.window_name, p.window_panes, p.pane_index)
}

/// [`where_label`] from the four fields it reads, for a row that is not a [`PaneRow`]:
/// an [`AgentRow`](crate::tui::data::AgentRow) carries the same four, and the `tab` token
/// of a configured `rows` template has to spell a pane the way every other row does.
pub fn target(window_index: u32, window_name: &str, window_panes: u32, pane_index: u32) -> String {
    let window = format!("{window_index}:{window_name}");
    if window_panes > 1 {
        format!("{window}.{pane_index}")
    } else {
        window
    }
}

/// What is running in a pane: the agent's own name when one occupies it (`@agent_display`,
/// else `@agent_name`, else its detected kind), otherwise the pane's command.
pub fn what_label(snap: &Snapshot, p: &PaneRow) -> String {
    match p.agent.and_then(|a| snap.agents.get(a)) {
        Some(agent) => agent.agent_label(),
        None => p.pane_current_command.clone(),
    }
}

/// The state word an agent's row draws in its state column (`blocked`), empty for a plain
/// pane and for an agent whose state has not been read yet. The filter matches it like the
/// rest of the row, so `/blocked` lists the blocked agents (as herdr's Go To filters
/// by state). A suspended agent's is [`crate::store::SUSPENDED_WORD`].
pub fn state_label(snap: &Snapshot, p: &PaneRow) -> String {
    let Some(agent) = p.agent.and_then(|a| snap.agents.get(a)) else {
        return String::new();
    };
    if agent.suspended && agent.state.is_some() {
        return crate::store::SUSPENDED_WORD.to_string();
    }
    agent
        .state
        .map(|state| state.to_string())
        .unwrap_or_default()
}

/// The name of the session a pane belongs to, empty when the snapshot has no such session.
///
/// `pub` because [`row_text`] puts it in front of every pane row, and the renderer has to
/// subtract exactly the same prefix to map a match's character positions onto the columns
/// it actually draws.
///
/// A session of another machine is `studio \u{b7} garden`: the machine, then the session, so a flat
/// list says where each row is.
pub fn session_name(snap: &Snapshot, session_id: &str) -> String {
    snap.spaces
        .iter()
        .find(|s| s.session_id == session_id)
        .map(|s| match machine_name(snap, s.machine) {
            Some(machine) => format!("{machine} \u{b7} {}", s.session_name),
            None => s.session_name.clone(),
        })
        .unwrap_or_default()
}

/// The name the session at `space` is called in a row: [`session_name`] of it.
pub fn space_name(snap: &Snapshot, space: usize) -> String {
    snap.spaces
        .get(space)
        .map(|s| session_name(snap, &s.session_id))
        .unwrap_or_default()
}

/// The name of the machine a row of machine `machine` is on, `None` for this machine's own.
fn machine_name(snap: &Snapshot, machine: Option<usize>) -> Option<&str> {
    snap.machines.get(machine?).map(|m| m.name.as_str())
}

/// The label a session header carries: its place in tmux order, for the first nine. In a tree a
/// listed machine with no session holds a place too ([`Labels::new`]).
pub fn session_label(space: usize) -> Option<u8> {
    (space < 9).then_some(space as u8 + 1)
}

/// The session a row belongs to, as an index into [`Snapshot::spaces`].
pub fn space_of(snap: &Snapshot, row: &Row) -> Option<usize> {
    match row {
        Row::Session(space) | Row::ClosedWorktree { space, .. } => Some(*space),
        Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => None,
        Row::Pane(pane) => {
            let session = &snap.panes.get(*pane)?.session_id;
            snap.spaces.iter().position(|s| s.session_id == *session)
        }
    }
}

/// The non-empty `parts`, single-spaced.
fn join(parts: impl IntoIterator<Item = String>) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text`'s match against `query`: its score and the character positions that matched, or
/// `None` when it does not match. [`build`] shares one matcher across a whole frame; this
/// is the one-off version, for a caller with a single string to test.
pub fn score(query: &Query, text: &str) -> Option<Match> {
    Scorer::new(query).score(text)
}

/// One row's match as [`build`] scores it, from what the row wears and says rather than from
/// the row: its label's `address` (empty for a row with no label), whether that address has a
/// number in it ([`Label::numbered`]) and its own words ([`row_text`]). [`NAMED`] when the
/// query is exactly a numbered address, else the query's terms against the labelled text and
/// the own words. The positions are in the labelled text ([`match_text`]). For a caller that
/// has a row's words but no snapshot, such as a check that compares another implementation of the matcher with the popup's.
pub fn score_labelled(query: &Query, address: &str, numbered: bool, own: &str) -> Option<Match> {
    Scorer::new(query).labelled_row(address, numbered, own, "", "", "")
}

/// A row's match with its title, as the popup scores it ([`score_labelled`] for a row that draws
/// no title, with the same score and positions): its score, the positions lit in the labelled
/// text (the address, a space, then the row's own words) and the positions lit in the title,
/// counted from the title's first character. A term also matches the title, as one run of its
/// characters ([`Scorer::by_terms`]).
pub fn score_titled(
    query: &Query,
    address: &str,
    numbered: bool,
    own: &str,
    title: &str,
) -> Option<Titled> {
    score_carrying(query, address, numbered, own, title, "")
}

/// [`score_titled`] for a row under a header, which carries the header's words (`inherited`:
/// the branch and git flags it draws, or what its template draws) as [`build`] does under a
/// header the query did not match, and in the flat list: a last resort of a term, found there as
/// one run, never anchored, lighting nothing ([`Scorer::by_terms`]). With no `inherited` words
/// the result is [`score_titled`]'s. A child under a header the query matched is not this
/// function's business: [`build`] scores such a child by its own words and title alone.
pub fn score_carrying(
    query: &Query,
    address: &str,
    numbered: bool,
    own: &str,
    title: &str,
    inherited: &str,
) -> Option<Titled> {
    let (score, positions) =
        Scorer::new(query).labelled_row(address, numbered, own, "", inherited, title)?;
    let from = title_start(&labelled(address, own.to_string()));
    let (labelled, title): (Vec<usize>, Vec<usize>) =
        positions.iter().copied().partition(|&at| at < from);
    Some(Titled {
        score,
        labelled,
        title: title.into_iter().map(|at| at - from).collect(),
    })
}

/// A row's match with its title: [`score_titled`], [`score_carrying`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Titled {
    pub score: i64,
    /// Positions in the labelled text.
    pub labelled: Vec<usize>,
    /// Positions in the title.
    pub title: Vec<usize>,
}

/// A row's match split at its address ([`match_text`]): the match within the row's own
/// text, its positions counted from that text's start (the address and the space after it
/// taken off), and the positions within the address, which light the label. A row with no
/// address keeps its match whole.
pub fn split_address(matched: Option<&Match>, address: usize) -> (Option<Match>, Vec<usize>) {
    let Some((score, positions)) = matched else {
        return (None, Vec::new());
    };
    if address == 0 {
        return (Some((*score, positions.clone())), Vec::new());
    }
    let label = positions.iter().copied().filter(|&i| i < address).collect();
    let own = positions
        .iter()
        .filter_map(|&i| i.checked_sub(address + 1))
        .collect();
    (Some((*score, own)), label)
}

/// One parsed query plus the matcher's scratch space, reused for every row of a frame, and
/// the labels the rows wear, which it matches in front of each row's text.
struct Scorer<'a> {
    pattern: Pattern,
    matcher: Matcher,
    haystack: Vec<char>,
    /// A second haystack, for a row's own words while [`Scorer::haystack`] holds them with the
    /// label in front ([`Scorer::by_terms`]).
    plain: Vec<char>,
    /// A third, for a row's `after` words, a fourth for the words it inherited from its header
    /// and a fifth for its title ([`Scorer::by_terms`]).
    tail: Vec<char>,
    carried: Vec<char>,
    heading: Vec<char>,
    /// The pattern's atoms as they match a title, each only as one run of its characters:
    /// a fuzzy atom becomes a substring one, the rest keep their kind. Parallel to the
    /// pattern's atoms.
    contiguous: Vec<Atom>,
    /// Whether a row's title is drawn and so found (`[ui] titles`).
    titles: bool,
    /// Whether a session header's words include the title of its hot agent, which a folded header
    /// draws ([`score_header_by_title`] turns it on for one scoring).
    header_title: bool,
    /// Whether the rows being scored are the flat list's: no header above them, so a row draws its
    /// session's name itself ([`build_flat`] turns it on while it scores).
    flat: bool,
    /// The plain text a row's configured template draws ([`build_drawn`]), `None` for no templates.
    drawn: Option<&'a Drawn<'a>>,
    indices: Vec<u32>,
    /// The query, trimmed: what a row's address is compared with ([`Scorer::named`]).
    text: String,
    labels: Labels,
}

/// The query parsed as fzf does, in smart case, but for a term that is an address, one or more
/// digits and then one letter (`4a`, `!4A`, `^12b$`): that term ignores case,
/// as the shift key says nothing about which row is meant. Any
/// other term with a capital in it (`Api`) still matches case-sensitively.
fn parse(text: &str) -> Pattern {
    let mut pattern = Pattern::parse(text, CaseMatching::Smart, Normalization::Smart);
    for atom in &mut pattern.atoms {
        let needle: String = atom.needle_text().chars().collect();
        if is_address(&needle) {
            let negative = atom.negative;
            *atom = Atom::new(
                &needle,
                CaseMatching::Ignore,
                Normalization::Smart,
                atom.kind,
                false,
            );
            atom.negative = negative;
        }
    }
    pattern
}

/// Whether a term's text, its operators taken off, spells an address: digits, then one letter.
fn is_address(term: &str) -> bool {
    let mut chars = term.chars();
    let Some(letter) = chars.next_back() else {
        return false;
    };
    let digits = chars.as_str();
    letter.is_ascii_alphabetic() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// The score a row whose address the query is exactly gets: above any match's.
pub const NAMED: i64 = i64::MAX;

impl<'a> Scorer<'a> {
    fn new(query: &Query) -> Scorer<'a> {
        let pattern = parse(&query.text);
        let contiguous = pattern
            .atoms
            .iter()
            .map(|atom| {
                let mut atom = atom.clone();
                if atom.kind == AtomKind::Fuzzy {
                    atom.kind = AtomKind::Substring;
                }
                atom
            })
            .collect();
        Scorer {
            pattern,
            matcher: Matcher::new(Config::DEFAULT),
            haystack: Vec::new(),
            plain: Vec::new(),
            tail: Vec::new(),
            carried: Vec::new(),
            heading: Vec::new(),
            contiguous,
            titles: false,
            header_title: false,
            flat: false,
            drawn: None,
            indices: Vec::new(),
            text: query.text.trim().to_string(),
            labels: Labels::default(),
        }
    }

    fn with_labels(mut self, labels: &Labels) -> Scorer<'a> {
        self.labels = labels.clone();
        self
    }

    fn with_titles(mut self, titles: bool) -> Scorer<'a> {
        self.titles = titles;
        self
    }

    fn with_drawn(mut self, drawn: &'a Drawn<'a>) -> Scorer<'a> {
        self.drawn = Some(drawn);
        self
    }

    /// `row`'s match: [`NAMED`] with its address's characters when the query is exactly its
    /// address and that has a number in it, in any case (`4a`, `4A`), else how the query's terms
    /// match its [`match_text`] (label first) and its [`row_text`] alone, term by term
    /// ([`Scorer::by_terms`]), so `^name` still anchors a row's own words and `!4` drops the
    /// rows labelled 4; the positions are always those of the [`match_text`].
    ///
    /// A row also has the words its configured template draws, and (unless `inherit` is false) the
    /// words of its header's it carries ([`Scorer::header_words`]); neither lights anything.
    fn row(
        &mut self,
        snap: &Snapshot,
        worktrees: &[WorktreeRecord],
        row: &Row,
        show_git: bool,
        inherit: bool,
    ) -> Option<Match> {
        let (address, numbered) = self
            .labels
            .get(row)
            .map_or((String::new(), false), |label| {
                (label.address.clone(), label.numbered)
            });
        let drawn = self.drawn.and_then(|drawn| drawn(row));
        // A row a template applies to shows what the template shows (which may be nothing) and
        // none of its default words: the words left are the ones drawn on lines above it
        // ([`templated_words`]).
        let templated = drawn.is_some() && matches!(row, Row::Session(_) | Row::Pane(_));
        let drawn = drawn.unwrap_or_default();
        let (own, inherited) = if templated {
            templated_words(snap, row, show_git)
        } else {
            let (own, inherited) = row_words(snap, worktrees, row, show_git);
            (self.without_drawn_session(snap, row, own), inherited)
        };
        let inherited = if inherit {
            self.header_words(snap, row, inherited)
        } else {
            String::new()
        };
        let title = match row {
            _ if templated => String::new(),
            Row::Session(space) if self.header_title => {
                header_title(snap, *space, self.titles).unwrap_or_default()
            }
            _ => title_of(snap, row, self.titles).unwrap_or_default(),
        };
        self.labelled_row(&address, numbered, &own, &drawn, &inherited, &title)
    }

    /// The words a child row carries of its header's: exactly what the header draws of them. A
    /// configured `[ui.sidebar.spaces] rows` template draws the header: its text, else the default
    /// header's (`default`, [`header_git`]: the branch and git flags where it draws them). A row
    /// that is no child's carries none.
    /// A templated header also carries the machine's name, which the line above it draws.
    fn header_words(&self, snap: &Snapshot, row: &Row, default: String) -> String {
        let (Row::Pane(_) | Row::ClosedWorktree { .. }) = row else {
            return String::new();
        };
        let Some(templated) = self.header_drawn(snap, row) else {
            return default;
        };
        let machine = space_of(snap, row)
            .and_then(|space| snap.spaces.get(space))
            .and_then(|s| machine_name(snap, s.machine))
            .unwrap_or_default();
        join([machine.to_string(), templated])
    }

    /// What a configured template draws on the header of `row`'s session, `None` while the header
    /// is drawn its default way (or `row` has no header).
    fn header_drawn(&self, snap: &Snapshot, row: &Row) -> Option<String> {
        let space = space_of(snap, row)?;
        self.drawn?(&Row::Session(space))
    }

    /// `own`, a child row's words, with its session's name blanked out (spaces of its width, so
    /// the positions of the words after it stay where the renderer counts them) when a template
    /// draws the header above it, whether or not that header matched: the header draws that name
    /// only if its template does, and then `header_words` has it. The flat list has no header, and
    /// draws the name on the row. A row that is no child's, or whose header is drawn its default
    /// way (which draws the name), keeps `own`.
    fn without_drawn_session(&self, snap: &Snapshot, row: &Row, own: String) -> String {
        let (Row::Pane(_) | Row::ClosedWorktree { .. }) = row else {
            return own;
        };
        if self.flat || self.header_drawn(snap, row).is_none() {
            return own;
        }
        let name = space_name(snap, space_of(snap, row).unwrap_or_default());
        match own.strip_prefix(name.as_str()) {
            Some(rest) if !name.is_empty() => {
                format!("{}{rest}", " ".repeat(name.chars().count()))
            }
            _ => own,
        }
    }

    /// [`NAMED`] when the query is exactly `row`'s address and that has a number in it, in any
    /// case, else `None`: how a row with no words of its own is found ([`Row::NoSession`]).
    fn named(&self, row: &Row) -> Option<Match> {
        let label = self.labels.get(row)?;
        (label.numbered && label.address.to_lowercase() == self.text.to_lowercase())
            .then(|| (NAMED, (0..label.address.chars().count()).collect()))
    }

    /// A row's match from its label's `address` (empty for a row that wears none), whether
    /// that address has a number in it, the row's own words, the `after` words its template draws
    /// (empty for a row with none), the words it `inherited` from its header (empty for a row
    /// with none) and its `title` (empty for a row that draws none): what [`Scorer::row`] does
    /// once it has read them off the row. The title's positions come after the label's, the
    /// words' and the `after` words' ([`title_start`]).
    fn labelled_row(
        &mut self,
        address: &str,
        numbered: bool,
        own: &str,
        after: &str,
        inherited: &str,
        title: &str,
    ) -> Option<Match> {
        if numbered && address.to_lowercase() == self.text.to_lowercase() {
            return Some((NAMED, (0..address.chars().count()).collect()));
        }
        if address.is_empty() && after.is_empty() && inherited.is_empty() {
            if title.is_empty() {
                return self.score(own);
            }
            return self.by_terms(own, own, 0, "", "", title);
        }
        if address.is_empty() {
            return self.by_terms(own, own, 0, after, inherited, title);
        }
        let shift = if address.is_empty() {
            0
        } else {
            address.chars().count() + 1
        };
        self.by_terms(
            &labelled(address, own.to_string()),
            own,
            shift,
            after,
            inherited,
            title,
        )
    }

    /// The query matched term by term against a row's text with its
    /// label in front (`with`) and its own words alone (`own`, which starts `shift` characters
    /// into `with`). A term matches when either text does, scoring the better, a tie going to
    /// the labelled one, so `^api` anchors the row's own words and `^4` its label; an inverse
    /// term (`!4`) drops the row when its text is in either, the label being drawn. The row
    /// matches when every term does; each term's positions come from the text it matched,
    /// moved to where `with` has them, which is what the renderer takes the address off again.
    ///
    /// A row's `after` words (empty when it has none: the words its template draws) and its
    /// `title` (empty when it draws none) are two more texts a term can match, each only as one run
    /// of the term's characters ([`Scorer::contiguous`]; `^` at the text's start, `$` at its end):
    /// they are long and a row's words are short, so letters scattered through them would let a
    /// short query light every row. A term keeps its best match, a tie going to the earlier of
    /// `with` and `own`, `after` and `title`. The positions come after those of `with` in that
    /// order, `after` first ([`title_start`]), so the title's start is where the row's text with
    /// its `after` words ends. An inverse term drops the row when `after` or the title has it too.
    ///
    /// The words a row `inherited` from its header are the last resort of a term: found there as
    /// one run too, but never anchored (they are not the start or the end of anything the row
    /// draws), and only when the row's own words, its `after` words and its title lack the term,
    /// whatever the scores, so a match the row draws is the one lit. They light nothing. An
    /// inverse term drops the row when they have it. With none of these, or none a term matches,
    /// the result is what `with` and `own` alone give.
    fn by_terms(
        &mut self,
        with: &str,
        own: &str,
        shift: usize,
        after: &str,
        inherited: &str,
        title: &str,
    ) -> Option<Match> {
        let Scorer {
            pattern,
            matcher,
            haystack,
            plain,
            heading,
            tail,
            carried,
            contiguous,
            indices,
            ..
        } = self;
        let with = Utf32Str::new(with, haystack);
        let own = Utf32Str::new(own, plain);
        let after = (!after.is_empty()).then(|| Utf32Str::new(after, tail));
        // The title's start, in the matcher's positions ([`title_start`]).
        let after_from = with.len() + 1;
        let from = match after {
            Some(after) => after_from + after.len() + 1,
            None => after_from,
        };
        let inherited = (!inherited.is_empty()).then(|| Utf32Str::new(inherited, carried));
        let title = (!title.is_empty()).then(|| Utf32Str::new(title, heading));
        let mut total = 0i64;
        let mut at = Vec::new();
        for (atom, run) in pattern.atoms.iter().zip(contiguous.iter()) {
            let anchored = matches!(
                run.kind,
                AtomKind::Prefix | AtomKind::Postfix | AtomKind::Exact
            );
            if atom.negative {
                // A negative atom scores `None` where its text is found.
                atom.score(with, matcher)?;
                atom.score(own, matcher)?;
                if let Some(after) = after {
                    run.score(after, matcher)?;
                }
                if let Some(inherited) = inherited.filter(|_| !anchored) {
                    run.score(inherited, matcher)?;
                }
                if let Some(title) = title {
                    run.score(title, matcher)?;
                }
                continue;
            }
            indices.clear();
            let labelled = atom
                .indices(with, matcher, indices)
                .map(|score| (score, indices.clone()));
            indices.clear();
            let alone = atom.indices(own, matcher, indices).map(|score| {
                let moved = indices
                    .iter()
                    .map(|&i| i + shift as u32)
                    .collect::<Vec<_>>();
                (score, moved)
            });
            let words = match (labelled, alone) {
                (Some(labelled), Some(alone)) if alone.0 > labelled.0 => Some(alone),
                (Some(labelled), _) => Some(labelled),
                (None, alone) => alone,
            };
            let mut found = |text: Option<Utf32Str<'_>>, start: usize| {
                indices.clear();
                text.and_then(|text| {
                    run.indices(text, matcher, indices).map(|score| {
                        let moved = indices
                            .iter()
                            .map(|&i| i + start as u32)
                            .collect::<Vec<_>>();
                        (score, moved)
                    })
                })
            };
            let later = found(after, after_from);
            let heading = found(title, from);
            let better =
                |best: Option<(u16, Vec<u32>)>, next: Option<(u16, Vec<u32>)>| match (best, next) {
                    (Some(best), Some(next)) if next.0 > best.0 => Some(next),
                    (Some(best), _) => Some(best),
                    (None, next) => next,
                };
            let drawn = better(better(words, later), heading);
            let (score, hits) = match drawn {
                Some(drawn) => drawn,
                None => {
                    let carried = inherited
                        .filter(|_| !anchored)
                        .and_then(|text| run.score(text, matcher))?;
                    (carried, Vec::new())
                }
            };
            total += i64::from(score);
            at.extend(hits.into_iter().map(|i| i as usize));
        }
        at.sort_unstable();
        at.dedup();
        Some((total, at))
    }

    fn score(&mut self, text: &str) -> Option<Match> {
        self.indices.clear();
        let haystack = Utf32Str::new(text, &mut self.haystack);
        let score = self
            .pattern
            .indices(haystack, &mut self.matcher, &mut self.indices)?;
        // nucleo appends each term's positions in the order the terms were written, and
        // leaves overlaps in; highlighting wants them sorted and unique.
        self.indices.sort_unstable();
        self.indices.dedup();
        Some((
            i64::from(score),
            self.indices.iter().map(|&i| i as usize).collect(),
        ))
    }
}

/// One row's match, or `None` when there is no query or the row does not match. `inherit` says
/// whether a child row carries its header's branch and git flags ([`row_words`]).
fn score_row(
    scorer: &mut Option<Scorer<'_>>,
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    row: &Row,
    show_git: bool,
    inherit: bool,
) -> Option<Match> {
    scorer
        .as_mut()?
        .row(snap, worktrees, row, show_git, inherit)
}

/// A folded header's match with the title of its hot agent among its words: what the header draws
/// ([`Row::Session`]'s own words and that title).
fn score_header_by_title(
    scorer: &mut Option<Scorer<'_>>,
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    space: usize,
    show_git: bool,
) -> Option<Match> {
    let scorer = scorer.as_mut()?;
    scorer.header_title = true;
    let found = scorer.row(snap, worktrees, &Row::Session(space), show_git, true);
    scorer.header_title = false;
    found
}

/// `rows`, each paired with its match, every child carrying its header's words.
fn score_rows(
    scorer: &mut Option<Scorer<'_>>,
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    rows: Vec<Row>,
    show_git: bool,
    inherit: bool,
) -> Vec<Scored> {
    rows.into_iter()
        .map(|row| {
            let score = score_row(scorer, snap, worktrees, &row, show_git, inherit);
            (row, score)
        })
        .collect()
}

/// The children of one header, each paired with its match, scored once. A child carries its
/// header's words only while the header did not match: a header that matched carries all its
/// children already, so each of them stands on its own words and title (the cursor stays on the
/// header), as it did before a child carried them.
fn score_kids(
    scorer: &mut Option<Scorer<'_>>,
    snap: &Snapshot,
    worktrees: &[WorktreeRecord],
    rows: Vec<Row>,
    show_git: bool,
    head_matched: bool,
) -> Vec<Scored> {
    score_rows(scorer, snap, worktrees, rows, show_git, !head_matched)
}

#[cfg(test)]
mod tests;
