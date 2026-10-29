//! Drawing one frame: the tree of sessions and panes, the preview, the hint line, and
//! whatever overlay is open on top.
//!
//! The view owns two things beyond pixels. It maintains the tree's scroll offset so the
//! cursor stays on screen, and it records the geometry the mouse needs
//! ([`Hits`](super::app::Hits)) — the app cannot know which screen row a row landed on,
//! and the view is the only place that does.
//!
//! The frame is laid out in columns: a row is a pane, written as a state
//! glyph, *what* is in it and *where* it is, with the columns aligned down the panel so
//! the eye can read one at a time. The one escape from the
//! fixed columns is a configured `[ui.sidebar.*] rows` template, which is drawn herdr's
//! way ([`rows::render_row`]) — a herdr config that customised its sidebar keeps working.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::attention;
use crate::config::{Depth, KeysPreset, RowToken, Sidebar};
use crate::machine::words;
use crate::session::naming;
use crate::store::AgentState;
use crate::tui::app::{App, PanelHit, TripWords, letter_among};
use crate::tui::data::{self, AgentRow, SpaceRow};
use crate::tui::keys::Action as KeyAction;
use crate::tui::machines;
use crate::tui::overlays;
use crate::tui::preview;
use crate::tui::rows;
use crate::tui::theme::Theme;
use crate::tui::tree::{self, Match, Row};

/// At this width and above the popup shows the preview beside the tree. Below it the two do
/// not both fit, and the tree wins (the list of agents comes first, so you can
/// navigate): it takes the whole width and the preview is not drawn.
pub const WIDE_COLUMNS: u16 = 100;

/// The width the tree panel may auto-widen to, borders included.
///
/// [`TREE_MIN`] is only the floor's own floor: `[ui] sidebar_width` raises it when it asks
/// for more, so a herdr config that set a wide sidebar still gets one, and a sidebar wider
/// than [`TREE_MAX`] raises the ceiling with it. Nothing lowers the panel below
/// [`TREE_MIN`] — the columns stop lining up — and nothing else raises it past
/// [`TREE_MAX`], which is wide enough for the fixed columns plus a couple of dozen
/// characters of an agent's title, and is what leaves room for the preview beside it.
pub const TREE_MIN: u16 = 36;
pub const TREE_MAX: u16 = 64;

/// The columns the preview keeps for itself whenever it is drawn beside the tree: the tree
/// is capped at `total - PREVIEW_MIN` as well as at [`TREE_MAX`], so the narrower of the
/// two limits wins.
///
/// [`TREE_MAX`] alone is a cap on the tree, not a promise to the preview. At
/// [`WIDE_COLUMNS`] — the narrowest popup that draws both panels — a tree grown to
/// [`TREE_MAX`] would leave the preview 36 columns, which is too narrow to read a pane in;
/// above roughly 104 columns the two limits never meet and this one does nothing.
pub const PREVIEW_MIN: u16 = 40;

/// The *what* column: every agent name and pane command is padded to this, so *where* it
/// lives starts at the same place on every row. A longer one is cut to
/// it with an `\u{2026}`, as a long target is to [`WHERE_COLUMNS`] ([`elide`]).
pub const WHAT_COLUMNS: usize = 10;

/// The *where* column: every `2:zsh.1` is padded to this, so the state that follows starts
/// at the same place on every row.
///
/// A pane's target is sized to it by its own row ([`pane_lines`], via [`elide`]): a window
/// renamed to something long is cut with an `\u{2026}` rather than pushing the state and
/// the title out of their columns on that row alone. The one target drawn whole is a
/// parked worktree's `worktree, closed` ([`worktree_line`]), which is wider than the
/// column by design and has no state or title after it to disturb.
const WHERE_COLUMNS: usize = 11;

/// The state column: `blocked 38m` and its neighbours, padded so an agent's title starts
/// at the same place on every row.
const STATE_COLUMNS: usize = 12;

/// Shown in the preview panel before the first capture arrives, or when nothing the cursor
/// is on has a pane to show.
pub const PREVIEW_PLACEHOLDER: &str = "no preview";

/// How old a capture has to be before the preview's title says so: a capture over a connection is
/// always a little behind, and a title that said so every time would say nothing.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// The line over the directories the `/` filter lists (`[ui] projects_in_filter`), one
/// column in from the border like the query line's own text.
const PROJECTS_HEADER: &str = " open a directory";

/// A directory's row starts where a session's name does, under the names above it.
const PROJECT_INDENT: &str = "    ";

/// The columns between a directory's path and the tag after it.
const PROJECT_GAP: usize = 2;

/// The fewest columns a directory's path is cut to, however narrow the panel: room for `~/`,
/// an ellipsis and the end of its name. Below that the row is cut at the panel's edge like any
/// other, and the tag is what goes.
const PROJECT_PATH_MIN: usize = 12;

/// The glyph of a pane with no agent in it, and of a worktree nothing is running.
const PLAIN_PANE: &str = "\u{25a2}";
/// The marker of an agent that runs in a container: a space and 📦, after its name. A
/// wide character, two columns in tmux and here.
const MARK: &str = " \u{1f4e6}";
/// The columns [`MARK`] takes of the name's [`WHAT_COLUMNS`].
const MARK_COLUMNS: usize = 3;
const CLOSED_WORKTREE: &str = "\u{25cc}";

/// What the filter says while nothing has been typed into it.
const PLACEHOLDER: &str = "search\u{2026}";

/// The separator between hints.
const SEPARATOR: &str = "  \u{b7}  ";

/// The states a folded header counts, in the order it counts them: what needs you first,
/// then what is busy, then what is quiet.
pub(crate) const COUNTS: [AgentState; 5] = [
    AgentState::Blocked,
    AgentState::Done,
    AgentState::Working,
    AgentState::Idle,
    AgentState::Unknown,
];

/// Draws the whole popup.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.theme;
    app.on_first_frame();
    f.render_widget(
        Block::default().style(Style::default().bg(theme.sidebar_bg).fg(theme.fg)),
        area,
    );

    // The hint line is measured before the body is laid out: it wraps to a second line
    // only when the width forces it, and the tree gets whatever is left.
    let hints = hint_lines(app, area.width.saturating_sub(1));
    let height = (hints.len() as u16).clamp(1, area.height.max(1));
    // The app has a line of its own above the body, which is also where its toast lands.
    let top = u16::from(app.is_app() && area.height > 2);
    let [top_area, body, hint_area] = Layout::vertical([
        Constraint::Length(top),
        Constraint::Min(0),
        Constraint::Length(height),
    ])
    .areas(area);
    if top > 0 {
        f.render_widget(
            Paragraph::new(app_line::app_top_line(app, area.width))
                .style(Style::default().bg(theme.sidebar_bg)),
            top_area,
        );
    }

    render_body(f, body, app);
    f.render_widget(
        Paragraph::new(hints).style(Style::default().bg(theme.sidebar_bg)),
        hint_area,
    );

    // `overlays::render` needs the overlay and the theme by reference while it fills the
    // hit map, so the map moves out and back rather than borrowing `app` twice.
    let mut overlay_hits = std::mem::take(&mut app.hits.overlay);
    let chrome = overlays::Chrome {
        theme: &theme,
        keymap: &app.keymap,
        preset: app.preset,
        indicators: app.cfg.ui.status_indicators,
        projects_in_filter: app.cfg.ui.projects_in_filter,
        numbers_work: app.numbers_work(),
        machines: app
            .snap
            .machines
            .iter()
            .any(|m| !m.this_server && !m.no_keeper),
        letters_work: app.cfg.ui.labels,
        app: app.is_app().then_some(overlays::AppLook {
            machines: app.snap.has_other_machines(),
            back_to_tmux: app.back_to_tmux,
        }),
    };
    overlays::render(f, area, &chrome, &app.overlay, &mut overlay_hits);
    app.hits.overlay = overlay_hits;
}

// ----------------------------------------------------------------- the hint line

/// One item of the hint line: a key and what it does, or the filter being typed.
enum Hint {
    Key(String, String),
    /// The query, written as `/<text>▏`, with the `▏` between what
    /// is before the input's cursor and what is after it.
    Query {
        before: String,
        after: String,
    },
}

impl Hint {
    /// One hint string split: everything up to the first space is the
    /// key, the rest is what it does.
    fn parse(text: &str) -> Hint {
        match text.split_once(' ') {
            Some((key, meaning)) => Hint::Key(key.to_string(), meaning.to_string()),
            None => Hint::Key(text.to_string(), String::new()),
        }
    }

    fn width(&self) -> usize {
        match self {
            Hint::Key(key, meaning) if meaning.is_empty() => key.width(),
            Hint::Key(key, meaning) => key.width() + 1 + meaning.width(),
            Hint::Query { before, after } if before.is_empty() && after.is_empty() => {
                2 + PLACEHOLDER.width()
            }
            Hint::Query { before, after } => 2 + before.width() + after.width(),
        }
    }

    fn spans(self, theme: &Theme) -> Vec<Span<'static>> {
        let key = Style::default()
            .fg(theme.yellow)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);
        match self {
            // A no-break space in a key holds its words together where the hint was split at its
            // first space; it is drawn as the space it stands for.
            Hint::Key(k, meaning) if meaning.is_empty() => {
                vec![Span::styled(k.replace('\u{a0}', " "), key)]
            }
            Hint::Key(k, meaning) => vec![
                Span::styled(k.replace('\u{a0}', " "), key),
                Span::raw(" "),
                Span::styled(meaning, dim),
            ],
            Hint::Query { before, after } => {
                let empty = before.is_empty() && after.is_empty();
                let mut spans = vec![Span::styled(
                    format!("/{before}\u{258f}{after}"),
                    Style::default().fg(theme.accent),
                )];
                if empty {
                    spans.push(Span::styled(PLACEHOLDER, dim));
                }
                spans
            }
        }
    }
}

/// The popup's bottom line(s): a live toast, else the open overlay's own keys, else the
/// query being typed followed by the tree's keys.
///
/// At most two lines, and a second one only when the width forces it; hints that do not fit in two lines are dropped rather than taking a third
/// row from the tree.
fn hint_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    // The app's toast is on its top line, not here.
    if let Some(toast) = app.live_toast()
        && !app.is_app()
    {
        return vec![Line::from(Span::styled(
            format!(" {}", truncate(toast, width)),
            Style::default().fg(theme.yellow),
        ))];
    }
    if app.overlay.is_open() {
        // An overlay says what *it* answers to; the tree's keys are not live under it.
        let items: Vec<Hint> = app
            .overlay
            .hints()
            .split(SEPARATOR)
            .map(Hint::parse)
            .collect();
        return wrap_hints(items, width, theme);
    }
    // Enter's words about a trip to another machine are the longest the line has: it says as much of them as
    // leaves every hint on its two lines, so a narrow popup keeps `? help` and says only `go`.
    let tree_items = |trip: TripWords| {
        let mut items: Vec<Hint> = Vec::new();
        // Only the modal preset's filter is announced here; the search preset's is a line
        // of the tree panel, where it is always visible ([`query_line`]).
        if app.typing() && app.preset != KeysPreset::Search {
            let (before, after) = cursor_split(app);
            items.push(Hint::Query { before, after });
        }
        items.extend(
            app.hint_line_with(trip)
                .into_iter()
                .map(|(key, meaning)| Hint::Key(key, meaning)),
        );
        items
    };
    for trip in [TripWords::Full, TripWords::Name] {
        let (lines, whole) = pack_hints(tree_items(trip), width, theme);
        if whole {
            return lines;
        }
    }
    wrap_hints(tree_items(TripWords::Plain), width, theme)
}

/// Packs `items` onto at most two lines of `width` columns, greedily.
fn wrap_hints(items: Vec<Hint>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    pack_hints(items, width, theme).0
}

/// [`wrap_hints`], and whether every item found a place: `false` when some were dropped for want of a third line.
fn pack_hints(items: Vec<Hint>, width: u16, theme: &Theme) -> (Vec<Line<'static>>, bool) {
    let width = width as usize;
    let separator = Style::default().fg(theme.dim);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut used = 1;
    let mut whole = true;
    for item in items {
        let wanted = item.width();
        // An item wider than the line is drawn cut: it did not find its place.
        whole &= wanted < width;
        let joined = if spans.len() > 1 {
            SEPARATOR.width()
        } else {
            0
        };
        if spans.len() > 1 && used + joined + wanted > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            if lines.len() == 2 {
                return (lines, false);
            }
            spans.push(Span::raw(" "));
            used = 1;
        } else if joined > 0 {
            spans.push(Span::styled(SEPARATOR, separator));
            used += joined;
        }
        used += wanted;
        spans.extend(item.spans(theme));
    }
    if spans.len() > 1 {
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    (lines, whole)
}

/// [`App::hint_line`] as one plain line: `Tab next up  ·  j/k move  ·  …`.
pub fn hint_text(app: &App) -> String {
    app.hint_line()
        .into_iter()
        .map(|(key, meaning)| format!("{key} {meaning}"))
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

// ---------------------------------------------------------------------- the body

/// Whether the preview is drawn beside the tree in a popup `total` columns wide: when
/// `[ui] preview` is on and the popup is [`WIDE_COLUMNS`] or more. Otherwise the tree has
/// the whole popup to itself, so there is always a list to navigate.
fn preview_beside(app: &App, total: u16) -> bool {
    app.show_preview && total >= WIDE_COLUMNS
}

fn render_body(f: &mut Frame, area: Rect, app: &mut App) {
    let lines = tree_lines(app);
    let (tree_area, preview_area) = if preview_beside(app, area.width) {
        let width = panel_width(app, &lines, &project_lines(app, None), area.width);
        let [tree_area, preview] =
            Layout::horizontal([Constraint::Length(width), Constraint::Min(0)]).areas(area);
        (tree_area, Some(preview))
    } else {
        (area, None)
    };
    render_tree(f, tree_area, app, lines);
    match preview_area {
        Some(preview_area) => render_preview(f, preview_area, app),
        // Nothing to hit where the preview was: a popup made narrow keeps no trace of it, or
        // the wheel over that spot would scroll a preview nobody can see.
        None => app.hits.preview = Rect::default(),
    }
}

/// The tree panel's width: the widest rendered row plus its two borders, clamped to
/// [`TREE_MIN`]..[`TREE_MAX`] and never past the popup itself.
pub fn tree_width(app: &App, total: u16) -> u16 {
    panel_width(app, &tree_lines(app), &project_lines(app, None), total)
}

/// `rows` are the tree's, and `projects` the filter's directories as wide as they would like
/// to be ([`project_lines`] with no panel to fit): a path too long for the panel widens it to
/// its ceiling and is cut to what is left, it does not widen it past that.
fn panel_width(
    app: &App,
    rows: &[Vec<Line<'static>>],
    projects: &[Line<'static>],
    total: u16,
) -> u16 {
    // With nothing to list, the panel is as wide as the sentence explaining that: the
    // half of it that says which key would fix it is the half that would be cut off.
    let longest = if rows.is_empty() && projects.is_empty() {
        empty_message(app).width().min(u16::MAX as usize) as u16
    } else {
        let header = (!projects.is_empty()).then(|| projects_header(app));
        rows.iter()
            .flatten()
            .chain(projects)
            .chain(&header)
            .map(line_width)
            .max()
            .unwrap_or(0)
    };
    // A `sidebar_width` past `TREE_MAX` raises the ceiling with the floor: the reader asked
    // for a wide sidebar and the panel is capped at `max(sidebar_width, TREE_MAX)`.
    // `.min(cap)` is not for the value's sake: `clamp` panics when it is handed a floor
    // above its ceiling.
    let mut cap = TREE_MAX.max(app.cfg.ui.sidebar_width);
    // With the preview beside it the tree gives way first, so the preview keeps
    // `PREVIEW_MIN`. The predicate is `render_body`'s own ([`preview_beside`]): below
    // `WIDE_COLUMNS`, or with the preview off, the tree has the popup to itself.
    if preview_beside(app, total) {
        cap = cap.min(total.saturating_sub(PREVIEW_MIN));
    }
    let floor = TREE_MIN.max(app.cfg.ui.sidebar_width).min(cap);
    longest.saturating_add(2).clamp(floor, cap).min(total)
}

fn line_width(line: &Line) -> u16 {
    let columns: usize = line.spans.iter().map(|s| s.content.width()).sum();
    columns.min(u16::MAX as usize) as u16
}

fn render_tree(f: &mut Frame, area: Rect, app: &mut App, rows: Vec<Vec<Line<'static>>>) {
    let theme = app.theme;
    let title = match app.view_label() {
        Some(label) => format!(" sessions \u{b7} {label} "),
        None => " sessions ".to_string(),
    };
    let mut block = panel_block(&theme, &title, true);
    if let Some(chip) = need_chip(app, area.width, &title) {
        block = block.title_top(chip.right_aligned());
    }
    if let Some(chip) = view_chip(app)
        && chip.width() + 2 <= area.width as usize
    {
        block = block.title_bottom(
            Line::from(Span::styled(
                chip,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        app.hits.tree = PanelHit::default();
        return;
    }
    app.hits.tree = PanelHit {
        area: inner,
        rows: Vec::new(),
        header: None,
    };
    let panel = Style::default().bg(theme.panel_bg);
    // The search preset's filter is the popup: it is always open, so it is drawn as the
    // panel's own first line rather than crowding the hint line.
    // It is not a row, so it takes a line off the list and stays out of the hit map — a
    // click on it must not select whatever row happens to sit at that y.
    let mut y = inner.y;
    if app.preset == KeysPreset::Search {
        f.render_widget(
            Paragraph::new(truncate_line(query_line(app), inner.width, theme.dim)).style(panel),
            Rect::new(inner.x, y, inner.width, 1),
        );
        y += 1;
    }
    let listed = inner.bottom().saturating_sub(y);
    // The filter's directories, drawn to the room this panel has.
    let projects = project_lines(app, Some(inner.width));
    if rows.is_empty() && projects.is_empty() {
        if listed > 0 {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    truncate(&empty_message(app), inner.width),
                    Style::default().fg(theme.dim),
                )))
                .style(panel),
                Rect::new(inner.x, y, inner.width, listed),
            );
        }
        return;
    }

    // The directories are pinned to the panel's foot when the tree is too tall to leave them
    // room under it, so they are on screen however many sessions there are; the tree scrolls
    // in the lines above them. Where the cursor is decides which of the two follows it, and
    // the other stays as it was. A tree that fits above the foot leaves them right under it.
    let size = section_size(listed, projects.len());
    let tree_bottom = inner.bottom() - size.map_or(0, SectionSize::lines);
    let above = rows.len();
    let heights: Vec<u16> = rows.iter().map(|lines| lines.len().max(1) as u16).collect();
    let tree_top = y;
    app.scroll = keep_visible(
        &heights,
        app.sel,
        app.scroll,
        tree_bottom.saturating_sub(y),
        app.follow_sel && app.sel < above,
    );
    'rows: for (index, lines) in rows.iter().enumerate().skip(app.scroll) {
        let style = if index == app.sel {
            Style::default().bg(theme.selection_bg).fg(theme.fg)
        } else {
            panel.fg(theme.fg)
        };
        // A machine's header is drawn to the panel's own width, its rule filling what the words
        // leave; the line the panel was measured by has the rule at its shortest.
        let header = match app.rows.rows.get(index) {
            Some(Row::Machine(machine)) => Some(machine_header::machine_line(
                app,
                *machine,
                Some(inner.width),
            )),
            _ => None,
        };
        for line in lines {
            if y >= tree_bottom {
                break 'rows;
            }
            let line = header.clone().unwrap_or_else(|| line.clone());
            f.render_widget(
                Paragraph::new(truncate_row(app, index, line, inner.width)).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            app.hits.tree.rows.push((y, index));
            y += 1;
        }
    }

    if let Some(size) = size {
        y = section_top(tree_top, span(&heights), tree_bottom);
        // The header is not a row: it is drawn in the panel's own colours, never lit, and
        // has no place in the hit map.
        if size.header {
            f.render_widget(
                Paragraph::new(truncate_line(projects_header(app), inner.width, theme.dim))
                    .style(panel.fg(theme.fg)),
                Rect::new(inner.x, y, inner.width, 1),
            );
            app.hits.tree.header = Some(y);
            y += 1;
        }
        let cursor = app.sel.checked_sub(above).filter(|k| *k < projects.len());
        app.projects_scroll = keep_visible(
            &vec![1; projects.len()],
            cursor.unwrap_or(0),
            app.projects_scroll,
            size.rows,
            app.follow_sel && cursor.is_some(),
        );
        for (k, line) in projects
            .into_iter()
            .enumerate()
            .skip(app.projects_scroll)
            .take(usize::from(size.rows))
        {
            let index = above + k;
            let style = if index == app.sel {
                Style::default().bg(theme.selection_bg).fg(theme.fg)
            } else {
                panel.fg(theme.fg)
            };
            f.render_widget(
                Paragraph::new(truncate_line(line, inner.width, theme.dim)).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            app.hits.tree.rows.push((y, index));
            y += 1;
        }
    }
    app.follow_sel = false;
}

/// How big the filter's directories' section is in a panel: whether the header is drawn, and
/// how many of the directories' rows. Where it starts is [`section_top`]'s to say.
#[derive(Clone, Copy)]
struct SectionSize {
    header: bool,
    rows: u16,
}

impl SectionSize {
    /// The panel's lines the section takes.
    fn lines(self) -> u16 {
        self.rows + u16::from(self.header)
    }
}

/// How big the section for `directories` is in a panel with `listed` lines: its header and a
/// line for each directory, and never more of the panel than half of it, so the tree keeps
/// the rest. A section with room for one line is that directory alone, because a header with
/// nothing under it says nothing; with room for none, there is none.
fn section_size(listed: u16, directories: usize) -> Option<SectionSize> {
    if directories == 0 {
        return None;
    }
    let lines = usize::from(listed / 2).min(directories + 1);
    match lines {
        0 => None,
        1 => Some(SectionSize {
            header: false,
            rows: 1,
        }),
        lines => Some(SectionSize {
            header: true,
            rows: (lines - 1) as u16,
        }),
    }
}

/// The line the filter's directories start on, just below the sessions. A tree of `tree_lines` lines that fits above `foot`, the line the section
/// starts on when it is against the panel's bottom, leaves it one blank line under the tree's
/// last line, with the panel's empty space below the section. A tree with no room for that
/// blank line drops it rather than let the section cover a line of the tree, and one too tall
/// for the room leaves the section at `foot` and scrolls, so it is never pushed out of the
/// panel. With no tree at all there is no line to leave a blank one under, and the section
/// starts at `top`.
fn section_top(top: u16, tree_lines: u32, foot: u16) -> u16 {
    if tree_lines == 0 {
        return top;
    }
    let under = u32::from(top) + tree_lines + 1;
    under.min(u32::from(foot)) as u16
}

/// The filter's text either side of its cursor, which is where the `▏` is drawn: the cursor
/// moves (Left, ctrl-b, ctrl-a, ...) and the line shows it, rather than keeping it at the end.
fn cursor_split(app: &App) -> (String, String) {
    app.filter
        .as_ref()
        .map(|input| {
            let (before, after) = input.around_cursor();
            (before.to_string(), after.to_string())
        })
        .unwrap_or_default()
}

/// The search preset's always-open filter, as the tree's first line: `> <text>▏`, the `▏` at
/// the cursor, with a dim `search…` while nothing has been typed into it.
fn query_line(app: &App) -> Line<'static> {
    let theme = &app.theme;
    let (before, after) = cursor_split(app);
    let empty = before.is_empty() && after.is_empty();
    let accent = Style::default().fg(theme.accent);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(">", accent),
        Span::raw(" "),
        Span::styled(before, accent),
        Span::styled("\u{258f}", accent),
        Span::styled(after, accent),
    ];
    if empty {
        spans.push(Span::styled(PLACEHOLDER, Style::default().fg(theme.dim)));
    }
    Line::from(spans)
}

/// What an empty tree says: why it is empty, and the key that makes a session when one is
/// bound. The search preset binds none, and its `n` is a letter of the query, so it names no
/// key at all.
fn empty_message(app: &App) -> String {
    match app.query() {
        Some(query) => {
            let nothing = format!(" nothing matches \"{}\"", query.text);
            match app.keymap.short_key(KeyAction::NewSession) {
                Some(key) => format!("{nothing}{SEPARATOR}{key} creates a session"),
                None => nothing,
            }
        }
        None => " no sessions".to_string(),
    }
}

/// What needs you, for the tree border: each state that needs you as its glyph and count in
/// its own colour, bold, most urgent first, then the words in the text colour, `⏸1 ✓2 need
/// you`, or `✓1 needs you` for one in all. `None` when nothing needs you, or when too little
/// is left beside the panel's own name to read any of it.
///
/// The glyph is the one the rows draw for the configured `[ui] status_indicators`, so a
/// count is never the colour of one state above the rows of another (a green
/// row under a blocked count would read as blocked). Cut to what fits, the words go first, then
/// groups from the least urgent end, whole `glyph+count` groups only.
fn need_chip(app: &App, width: u16, title: &str) -> Option<Line<'static>> {
    let split = app.hot_by_state();
    let total: usize = split.iter().map(|&(_, count)| count).sum();
    if total == 0 {
        return None;
    }
    let theme = &app.theme;
    let indicators = app.cfg.ui.status_indicators;
    let groups: Vec<(String, Style)> = split
        .iter()
        .map(|&(state, count)| {
            (
                format!("{}{count}", data::state_icon(Some(state), indicators)),
                Style::default()
                    .fg(theme.state_color(Some(state)))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    let words = format!("need{} you", if total == 1 { "s" } else { "" });
    let plain = Style::default().fg(theme.fg);
    let room = (width as usize)
        .saturating_sub(2)
        .saturating_sub(title.width())
        .saturating_sub(1);
    // Every group and the words, else the groups alone, else fewer of them.
    let readings = std::iter::once((groups.len(), true))
        .chain((1..=groups.len()).rev().map(|kept| (kept, false)));
    for (kept, with_words) in readings {
        let shown = &groups[..kept];
        let columns = 2
            + shown.iter().map(|(text, _)| text.width()).sum::<usize>()
            + (kept - 1)
            + if with_words { 1 + words.width() } else { 0 };
        if columns > room {
            continue;
        }
        let mut spans = vec![Span::styled(" ", plain)];
        for (at, (text, style)) in shown.iter().enumerate() {
            if at > 0 {
                spans.push(Span::styled(" ", plain));
            }
            spans.push(Span::styled(text.clone(), *style));
        }
        if with_words {
            spans.push(Span::styled(format!(" {words}"), plain));
        }
        spans.push(Span::styled(" ", plain));
        return Some(Line::from(spans));
    }
    None
}

/// The chip on the bottom border: what the tree is showing when it is not showing the
/// default — a flat list, or a depth short of everything.
fn view_chip(app: &App) -> Option<String> {
    if app.flat_now() {
        return Some(" flat ".to_string());
    }
    match app.depth {
        Depth::Sessions => Some(" sessions ".to_string()),
        Depth::Agents => Some(" agents ".to_string()),
        Depth::All => None,
    }
}

// ------------------------------------------------------------------- the rows

/// Every row of the tree, rendered, in [`App::rows`] order, up to the filter's directories,
/// which [`project_lines`] draws once the panel's width is known. One line each, except where
/// a configured `rows` template asks for more.
fn tree_lines(app: &App) -> Vec<Vec<Line<'static>>> {
    let defaults = Sidebar::default();
    let ctx = rows::Ctx {
        theme: &app.theme,
        indicators: app.cfg.ui.status_indicators,
    };
    // The rows that wear a letter, and those of them that are lit (the cursor's session's while
    // they are keys, or while the query is that session's number): worked out once for the
    // frame, as asking the app row by row would walk every row again for each one.
    let letters = app.row_letters();
    let lettered = app.lit_letters();
    // An open overlay takes every key itself: no number or letter is lit under it.
    let keys = app.tree_takes_keys();
    let above = app.rows.first_project().unwrap_or(app.rows.rows.len());
    app.rows
        .rows
        .iter()
        .enumerate()
        .take(above)
        .map(|(index, row)| {
            // The query matched the row's address and then its own text ([`tree::match_text`]):
            // the columns below read the positions in the row's own text, and the address's
            // light the label where it is drawn.
            let (matched, label_hits) = tree::split_address(
                app.rows.scores.get(index).and_then(|m| m.as_ref()),
                app.row_address(index).chars().count(),
            );
            let matched = drawn_match(app, row, matched);
            let matched = matched.as_ref();
            // The number key that reaches this row, drawn in its first column, with whether it
            // works right now (yellow: a key that works now); and, for a child, whether it is the
            // last of its session, which is the connector it hangs from. The next row starting a
            // session (or there being none) is what makes it the last, because a session's rows
            // are contiguous. A number the query matched needs no light of its own: while you
            // type every number works, so it is yellow already.
            let number = app.row_label(index).map(|text| Number {
                text,
                key: keys && app.label_works(index),
            });
            let last = app.rows.rows.get(index + 1).is_none_or(|next| {
                matches!(
                    next,
                    Row::Session(_) | Row::Project(_) | Row::Machine(_) | Row::NoSession(_)
                )
            });
            // Whether this child's session is the one the popup was opened from — the
            // tree line and the header's `*` both read off the same test, so a row never
            // disagrees with its own header about whose session it is.
            let mine = tree::space_of(&app.snap, row)
                .and_then(|space| app.snap.spaces.get(space))
                .is_some_and(|s| {
                    app.snap.current_session.as_deref() == Some(s.session_id.as_str())
                });
            // The letter this row wears, `a`–`e`, when it is one of the first five of its
            // session, lit when it is a key (the cursor is in that session), when the query is
            // that session's number, or when the query matched it: the letter is the last
            // character of a child's address, its session's number being drawn on its header.
            let address = app.row_address(index).chars().count();
            let letter = letter_among(&letters, index).map(|key| Letter {
                key,
                lit: keys
                    && (letter_among(&lettered, index).is_some()
                        || label_hits.contains(&address.saturating_sub(1))),
            });
            let greyed = tree::space_of(&app.snap, row)
                .and_then(|space| app.snap.spaces.get(space))
                .is_some_and(|space| app.snap.greyed(space.machine));
            let build = |matched: Option<&Match>| -> Vec<Line<'static>> {
                match row {
                    Row::Session(space) => {
                        session_lines(app, &defaults, &ctx, *space, matched, number.clone())
                    }
                    Row::Pane(pane) => pane_lines(
                        app,
                        &defaults,
                        &ctx,
                        *pane,
                        matched,
                        last,
                        number.clone(),
                        mine,
                        letter,
                    ),
                    Row::ClosedWorktree { space, record } => {
                        vec![worktree_line(
                            app,
                            *space,
                            *record,
                            matched,
                            last,
                            number.clone(),
                            mine,
                            letter,
                        )]
                    }
                    Row::Machine(machine) => {
                        vec![machine_header::machine_line(app, *machine, None)]
                    }
                    Row::NoSession(_) => vec![no_session_line(app, number.clone())],
                    // Never reached: the directories are after `above`.
                    Row::Project(_) => Vec::new(),
                }
            };
            let lines = build(matched);
            // A row of a machine that is away keeps its words and loses every colour: dim, the
            // state's glyph and the bold of a hot row included. The characters the filter matched
            // keep their light, bold and underlined in the dim colour: found by drawing the row
            // again with no match and taking what differs.
            let lines = if greyed {
                let plain = if matched.is_some() {
                    build(None)
                } else {
                    Vec::new()
                };
                let mut plain = plain.into_iter();
                lines
                    .into_iter()
                    .map(|l| greyed_line(l, plain.next(), app.theme.dim))
                    .collect()
            } else {
                lines
            };
            // A row that rendered to nothing still occupies one line, or the scroll
            // arithmetic and the hit map would disagree about where the next row landed.
            if lines.is_empty() {
                vec![Line::default()]
            } else {
                lines
            }
        })
        .collect()
}

/// `line` with every character in the dim colour and none bold, but for those `plain`, the same row
/// drawn with no match, draws differently: the characters the filter matched, which keep bold and
/// an underline, as a hit is lit on any row. With no `plain`, or one that does not read as the same
/// text, nothing is lit.
fn greyed_line(line: Line<'static>, plain: Option<Line<'static>>, dim: Color) -> Line<'static> {
    let quiet = Style::default().fg(dim).remove_modifier(Modifier::BOLD);
    let lit = Style::default()
        .fg(dim)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let cells = |line: &Line<'static>| -> Vec<(char, Style)> {
        line.spans
            .iter()
            .flat_map(|span| span.content.chars().map(|c| (c, span.style)))
            .collect()
    };
    let drawn = cells(&line);
    let others = plain.as_ref().map(cells).filter(|others| {
        others.len() == drawn.len() && others.iter().zip(&drawn).all(|(a, b)| a.0 == b.0)
    });
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = quiet;
    for (at, (c, style)) in drawn.iter().enumerate() {
        let hit = others.as_ref().is_some_and(|others| others[at].1 != *style);
        let wanted = if hit { lit } else { quiet };
        if wanted != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = wanted;
        run.push(*c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    Line::from(spans)
}

/// The row under a connected machine's header that has no tmux session: its number where a session
/// header's is, drawn as that one is, then its words, dim, under the header's name.
fn no_session_line(app: &App, number: Option<Number>) -> Line<'static> {
    Line::from(vec![
        number_span(number, &app.theme),
        Span::raw("  "),
        Span::styled(
            crate::machine::words::NO_SESSION_ROW,
            Style::default().fg(app.theme.dim),
        ),
    ])
}

/// The line over the directories: `open a directory`, dim. Not a row of the list, only the
/// first line of the first directory's row ([`render_tree`] keeps it out of the hit map and
/// out of the cursor's highlight).
fn projects_header(app: &App) -> Line<'static> {
    Line::from(Span::styled(
        PROJECTS_HEADER,
        Style::default().fg(app.theme.dim),
    ))
}

/// The filter's directories, one line each, in the order the picker lists them: what
/// [`App::projects`] holds, drawn in `inner` columns, or as wide as each would like to be when
/// `inner` is `None` (to measure the panel by).
fn project_lines(app: &App, inner: Option<u16>) -> Vec<Line<'static>> {
    let room = path_room(app, inner);
    let first = app.rows.first_project().unwrap_or(app.rows.rows.len());
    app.rows
        .rows
        .iter()
        .enumerate()
        .skip(first)
        .filter_map(|(index, row)| match row {
            Row::Project(at) => {
                let matched = app.rows.scores.get(index).cloned().flatten();
                let path = app.projects.get(*at).map_or("", |p| p.path.as_str());
                let matched = lit_chars(path, None, matched);
                Some(project_line(app, *at, matched.as_ref(), &room))
            }
            _ => None,
        })
        .collect()
}

/// How many columns a directory's path may take.
struct PathRoom {
    /// Where a tag follows: every tagged path is padded to this, so the tags start in one
    /// column. The longest path listed, or, when that is more than the panel can hold beside
    /// the widest tag, what the indent, the gap and that tag leave of it.
    tagged: usize,
    /// Where nothing follows, so nothing is kept back: the whole row, past the indent.
    bare: usize,
}

/// The room a directory's path has in a panel `inner` columns wide, or all it wants with none.
fn path_room(app: &App, inner: Option<u16>) -> PathRoom {
    let longest = app
        .projects
        .iter()
        .map(|project| project.path.width())
        .max()
        .unwrap_or(0);
    let Some(inner) = inner else {
        return PathRoom {
            tagged: longest,
            bare: usize::MAX,
        };
    };
    let after_indent = usize::from(inner).saturating_sub(PROJECT_INDENT.width());
    let widest_tag = app
        .projects
        .iter()
        .filter_map(|project| project.tag)
        .map(|tag| tag.label().width())
        .max();
    let beside = after_indent.saturating_sub(widest_tag.map_or(0, |tag| PROJECT_GAP + tag));
    PathRoom {
        tagged: longest.min(beside.max(PROJECT_PATH_MIN)),
        bare: after_indent.max(PROJECT_PATH_MIN),
    }
}

/// A directory the filter lists: its path in `fg`, the characters the query matched picked
/// out, and where it came from at the end of the row in dim (`running`, `zoxide`,
/// `project dir`; a plain completion has none). A path is cut from its middle when it is wider
/// than its room ([`shorten`]), so its last component stays; one with a tag after it is
/// padded to `room.tagged`, so the tags start in one column.
fn project_line(app: &App, at: usize, matched: Option<&Match>, room: &PathRoom) -> Line<'static> {
    let Some(project) = app.projects.get(at) else {
        return Line::default();
    };
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim);
    let style = Style::default().fg(theme.fg);
    let limit = if project.tag.is_some() {
        room.tagged
    } else {
        room.bare
    };
    let mut spans = vec![Span::raw(PROJECT_INDENT)];
    let drawn = match shorten(&project.path, limit) {
        None => {
            spans.extend(highlight(&project.path, matched, 0, style, theme));
            project.path.width()
        }
        Some(cut) => {
            if !cut.head.is_empty() {
                spans.extend(highlight(cut.head, matched, 0, style, theme));
            }
            spans.push(Span::styled("\u{2026}", dim));
            spans.extend(highlight(cut.tail, matched, cut.tail_from, style, theme));
            cut.head.width() + 1 + cut.tail.width()
        }
    };
    if let Some(tag) = project.tag {
        spans.push(Span::raw(" ".repeat(room.tagged.saturating_sub(drawn))));
        spans.push(Span::styled(
            format!("{}{}", " ".repeat(PROJECT_GAP), tag.label()),
            dim,
        ));
    }
    Line::from(spans)
}

/// A path drawn in fewer columns than it has: its head, an ellipsis, and its tail.
struct Shortened<'a> {
    head: &'a str,
    tail: &'a str,
    /// The character of the path that `tail` starts at, which is where the query's positions
    /// are counted from for it.
    tail_from: usize,
}

/// `path` in `columns` display columns: `None` when it fits, else its head, an ellipsis and
/// its tail, the middle gone. The tail is the last component with the slash before it, which
/// is what tells one directory from another, and the head is as many whole components from
/// the start as fit beside it. When the last component is itself too wide, or there is no
/// slash to cut at, the tail is the end of the path and there is no head.
fn shorten(path: &str, columns: usize) -> Option<Shortened<'_>> {
    if path.width() <= columns {
        return None;
    }
    // The ellipsis takes a column of its own.
    let budget = columns.saturating_sub(1);
    let leaf = path
        .rfind('/')
        .map(|at| &path[at..])
        .filter(|leaf| leaf.width() <= budget);
    let Some(tail) = leaf else {
        let tail = end_within(path, budget);
        return Some(Shortened {
            head: "",
            tail,
            tail_from: path.chars().count() - tail.chars().count(),
        });
    };
    let before = &path[..path.len() - tail.len()];
    let room = budget - tail.width();
    // Each prefix that ends in a slash is a run of whole components; the longest that fits.
    let head = before
        .match_indices('/')
        .map(|(at, _)| &before[..=at])
        .take_while(|head| head.width() <= room)
        .last()
        .unwrap_or("");
    Some(Shortened {
        head,
        tail,
        tail_from: before.chars().count(),
    })
}

/// The end of `text` that fits in `columns` display columns.
fn end_within(text: &str, columns: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (at, c) in text.char_indices().rev() {
        let width = c.width().unwrap_or(0);
        if used + width > columns {
            break;
        }
        used += width;
        start = at;
    }
    &text[start..]
}

/// A session header: `<number> <flag> <name>  <branch> <flags>`, plus one count per state
/// and the title of its agent that needs you when the depth is showing sessions only and
/// the panes below are hidden.
///
/// The number is the first column's alone, and drawn as the key it is ([`number_span`]): in
/// the theme's yellow, bold, on every header, the cursor's session or not, while pressing it
/// works, as the cursor's session's letters are; dim while it does not, with `/` open, under an overlay, which takes
/// every key itself, or in the search preset.
///
/// No state glyph: the colours and glyphs are the agents', and a session with
/// one blocked agent and one finished one is neither.
///
/// `index` is the session's place in [`Snapshot::spaces`](crate::tui::data::Snapshot),
/// which is also what [`App::session_hot_agent`] takes; `number` was read off the *row*
/// index by [`tree_lines`], which is the only place that knows both.
fn session_lines(
    app: &App,
    defaults: &Sidebar,
    ctx: &rows::Ctx,
    index: usize,
    matched: Option<&Match>,
    number: Option<Number>,
) -> Vec<Line<'static>> {
    let Some(space) = app.snap.spaces.get(index) else {
        return Vec::new();
    };
    let (flag, flag_style) = session_flag_style(app, space);
    if let Some(template) = rows::spaces_template(&app.cfg.ui.sidebar, defaults) {
        // With `[ui] labels` on, the number and the flag stay in front of the template's rows,
        // as on a built-in header: the key is still pressed, so it is still drawn.
        let prefix = if app.cfg.ui.labels {
            vec![
                number_span(number, &app.theme),
                Span::raw(" "),
                Span::styled(flag.to_string(), flag_style),
                Span::raw(" "),
            ]
        } else {
            Vec::new()
        };
        return prefixed(prefix, None, template_lines(template, space, ctx, None));
    }
    let theme = &app.theme;
    let indicators = app.cfg.ui.status_indicators;
    let dim = Style::default().fg(theme.dim);
    // Only the number is styled; the column after it is plain, as the one after a row's letter.
    let mut spans = vec![
        number_span(number, theme),
        Span::raw(" "),
        Span::styled(flag.to_string(), flag_style),
        Span::raw(" "),
    ];
    spans.extend(highlight(
        &space.session_name,
        matched,
        0,
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        theme,
    ));
    if app.cfg.ui.show_git
        && let Some(git) = &space.git
        && let Some(branch) = &git.branch
    {
        let status = data::git_status_text(git);
        let text = if status.is_empty() {
            format!("  {branch}")
        } else {
            format!("  {branch} {status}")
        };
        spans.push(Span::styled(text, dim));
    }
    if app.depth == Depth::Sessions {
        for (state, count) in state_counts(app, space) {
            spans.push(Span::styled(
                format!("  {}{count}", data::state_icon(Some(state), indicators)),
                Style::default().fg(theme.state_color(Some(state))),
            ));
        }
        // Folded, the header stands for the agent `Tab` and `Enter` would reach: say which
        // one, in its own words.
        if app.cfg.ui.titles
            && let Some(agent) = app
                .session_hot_agent(index)
                .and_then(|i| app.snap.agents.get(i))
        {
            let title = data::strip_title_glyph(&agent.pane_title);
            if !title.is_empty() {
                spans.push(Span::styled(format!("  {title}"), dim));
            }
        }
    }
    vec![Line::from(spans)]
}

/// The header's second column: `*` on the session the popup was opened from, `-` on the
/// one it would go back to, a space on every other — and its style: the own session's `*`
/// in accent, bold, so it reads at a glance; the rest dim, as ever.
fn session_flag_style(app: &App, space: &SpaceRow) -> (&'static str, Style) {
    let id = Some(space.session_id.as_str());
    let dim = Style::default().fg(app.theme.dim);
    if app.snap.current_session.as_deref() == id {
        (
            "*",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else if app.snap.previous_session.as_deref() == id {
        ("-", dim)
    } else {
        (" ", dim)
    }
}

/// A pane: the connector it hangs from, its state glyph, and the columns of [`row_line`].
///
/// An agent row says what state it is in and for how long, and carries the agent's own
/// pane title; when it needs you ([`attention::hot`]) every one of those spans is drawn in
/// the state's colour instead of the quiet ones, bold while it is blocked, and so is a row drawn
/// from a template, in each token with no `fg` of its own that no rule restyled. Otherwise, a
/// row of the window its session has open is lit whole: every column that is dim on other
/// rows is white on it — what is in it and its place bold, its state, elapsed time and
/// title, a flat row's session name, a plain pane's glyph;
/// and its tree line, `fg` and bold, hot or not ([`row_prefix`]). Every other row
/// keeps the quiet colours — the agent white, the rest dim; a plain pane dim throughout.
#[allow(clippy::too_many_arguments)] // one call site (`tree_lines`); a struct would only rename these nine facts
fn pane_lines(
    app: &App,
    defaults: &Sidebar,
    ctx: &rows::Ctx,
    index: usize,
    matched: Option<&Match>,
    last: bool,
    number: Option<Number>,
    mine: bool,
    letter: Option<Letter>,
) -> Vec<Line<'static>> {
    let Some(pane) = app.snap.panes.get(index) else {
        return Vec::new();
    };
    let agent: Option<&AgentRow> = pane.agent.and_then(|a| app.snap.agents.get(a));
    if let Some(agent) = agent
        && let Some(template) =
            rows::agents_template(&app.cfg.ui.sidebar, defaults, agent.kind.as_deref())
    {
        // With `[ui] labels` on, the letter and the connector stay in front of the template's
        // rows, as on a built-in row (lit as the open window's rows are).
        let prefix = if app.cfg.ui.labels {
            let open = tree::space_of(&app.snap, &Row::Pane(index))
                .and_then(|space| app.snap.spaces.get(space))
                .is_some_and(|s| s.active_window_id == pane.window_id);
            row_prefix(app, last, number, mine, open, letter)
        } else {
            Vec::new()
        };
        // A tree row that is not the last carries the tree line on down through its further
        // lines: the same six columns, with the connector's own `\u{2502}` under its `\u{251c}`.
        let rest = if app.flat_now() || last {
            None
        } else {
            prefix.get(prefix.len().wrapping_sub(2)).map(|connector| {
                vec![
                    Span::raw("    "),
                    Span::styled("\u{2502}", connector.style),
                    Span::raw(" "),
                ]
            })
        };
        // A row that needs you is drawn in the state's colour end to end, as a built-in row is.
        let hot = attention::hot(agent.state, agent.seen).then(|| {
            let mut style = Style::default().fg(app.theme.state_color(agent.state));
            if agent.state == Some(AgentState::Blocked) {
                style = style.add_modifier(Modifier::BOLD);
            }
            style
        });
        return prefixed(prefix, rest, template_lines(template, agent, ctx, hot));
    }
    let theme = &app.theme;
    let dim = Style::default().fg(theme.dim);
    // The window this session currently has open: Enter on its header lands there, and
    // in your own session it is where you are. Its rows are lit whole: window names
    // can be emoji, so a lit place column alone would be two characters wide.
    let open = tree::space_of(&app.snap, &Row::Pane(index))
        .and_then(|space| app.snap.spaces.get(space))
        .is_some_and(|s| s.active_window_id == pane.window_id);
    let open_style = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let lit = Style::default().fg(theme.fg);
    let what = tree::what_label(&app.snap, pane);
    let prefix = row_prefix(app, last, number, mine, open, letter);
    let parts = match agent {
        Some(agent) => {
            let hot = attention::hot(agent.state, agent.seen);
            let colour = theme.state_color(agent.state);
            let mut hot_style = Style::default().fg(colour);
            if agent.state == Some(AgentState::Blocked) {
                hot_style = hot_style.add_modifier(Modifier::BOLD);
            }
            let (what_style, where_style, tail_style) = if hot {
                (hot_style, hot_style, hot_style)
            } else if open {
                (open_style, open_style, lit)
            } else {
                (lit, dim, dim)
            };
            // A flat row's session name is dim on every row but the open window's: a hot row
            // keeps it dim, as it always has.
            let session_style = if open && !hot { lit } else { dim };
            // The state word, which the filter matches like the rest of the row, then how
            // long it has held.
            let state = tree::state_label(&app.snap, pane);
            let mut tail = state.clone();
            if agent.state_since > 0 && !tail.is_empty() {
                tail.push(' ');
                tail.push_str(&elapsed_label(agent.state_since, now_secs()));
            }
            // The agent's own title, minus the glyph Claude Code writes in front of it,
            // and never a second copy of the name already in the *what* column: the one rule
            // the filter finds a title by ([`tree::drawn_title`]). A match in it is counted
            // from where the row's own words end ([`tree::title_column`]).
            let title = tree::drawn_title(&app.snap, pane, app.cfg.ui.titles).map(|text| {
                let own = tree::own_words(
                    &app.snap,
                    &app.worktrees,
                    &Row::Pane(index),
                    app.cfg.ui.show_git,
                );
                TitleColumn {
                    text,
                    style: tail_style,
                    from: tree::title_column(&own),
                }
            });
            RowParts {
                prefix,
                // The glyph keeps its state's colour whether or not the row is hot; the
                // bold `hot_style` adds is only ever a blocked agent's, and a blocked
                // agent is hot by definition.
                glyph: data::state_icon(agent.state, app.cfg.ui.status_indicators).to_string(),
                glyph_style: hot_style,
                what_style,
                target_style: where_style,
                // Always `Some`, even for an agent whose state has not been read yet:
                // an empty state column is still a column, and the title after it lines
                // up with every other row's.
                tail: Some(Tail {
                    text: tail,
                    word: state.chars().count(),
                    style: tail_style,
                }),
                session_style,
                title,
                boxed: agent.container.is_some(),
            }
        }
        None => {
            let (glyph_style, style) = if open { (lit, open_style) } else { (dim, dim) };
            RowParts {
                prefix,
                glyph: PLAIN_PANE.to_string(),
                glyph_style,
                what_style: style,
                target_style: style,
                tail: None,
                session_style: if open { lit } else { dim },
                title: None,
                boxed: false,
            }
        }
    };
    vec![row_line(
        app,
        parts,
        &what,
        Target::Cut(&tree::where_label(pane)),
        &tree::session_name(&app.snap, &pane.session_id),
        matched,
    )]
}

/// A worktree with no session running it: `  └ ◌ <branch> worktree, closed`.
#[allow(clippy::too_many_arguments)] // one call site (`tree_lines`); a struct would only rename these eight facts
fn worktree_line(
    app: &App,
    space: usize,
    record: usize,
    matched: Option<&Match>,
    last: bool,
    number: Option<Number>,
    mine: bool,
    letter: Option<Letter>,
) -> Line<'static> {
    let dim = Style::default().fg(app.theme.dim);
    let session = tree::space_name(&app.snap, space);
    let branch = app
        .worktrees
        .get(record)
        .map(|r| r.branch.clone())
        .unwrap_or_default();
    row_line(
        app,
        RowParts {
            prefix: row_prefix(app, last, number, mine, false, letter),
            glyph: CLOSED_WORKTREE.to_string(),
            glyph_style: dim,
            what_style: dim,
            target_style: dim,
            tail: None,
            session_style: dim,
            title: None,
            boxed: false,
        },
        &branch,
        Target::Whole("worktree, closed"),
        &session,
        matched,
    )
}

/// The columns of a child row before its glyph: the connector it hangs from (`└` when it is
/// the last row of its session, `├` otherwise), or, in a flat list, the number key that
/// reaches it, drawn as a header's number is ([`number_span`]).
///
/// `open` is whether this row is in the window its session has open: its connector is `fg`,
/// bold, in every session, the one you came from included, because the tree line is where that
/// mark is looked for. Whether the row needs you changes nothing here, and
/// a parked worktree, which has no window, is never open.
///
/// `mine` is whether this row's session is the one the popup was opened from
/// ([`Snapshot::current_session`](data::Snapshot::current_session)): its other rows'
/// connectors are drawn in the theme's accent, bold, so the eye finds that session's rows
/// without reading every header; every other row's connector stays dim, as before. A flat
/// list has no connectors — its number key is never coloured by whose session it is.
///
/// `letter` is the letter this row wears when it is one of the first five of its session
/// ([`App::row_letters`]): drawn in the two columns before the connector (`  a ├ ⏵ claude …`),
/// in the theme's yellow, bold, when it is a key, in the session the cursor is in
/// ([`App::lettered_rows`]) while no overlay takes the keys, and dim in every other session.
/// Those columns are blank otherwise, so a row is the same width lettered or not. A flat list has no letters.
///
/// The first column is the session numbers' alone (letters beside every
/// session's rows would clutter the numbers), so a tree's child row starts two columns in, its
/// letter under its header's `*` and its connector under the session's name.
fn row_prefix(
    app: &App,
    last: bool,
    number: Option<Number>,
    mine: bool,
    open: bool,
    letter: Option<Letter>,
) -> Vec<Span<'static>> {
    let dim = Style::default().fg(app.theme.dim);
    if app.flat_now() {
        // A flat row's number is a key as a header's is (it puts the cursor on its row), and is
        // drawn the same way, so `f` never turns a key that works dim.
        vec![number_span(number, &app.theme), Span::raw(" ")]
    } else {
        let style = if open {
            Style::default()
                .fg(app.theme.fg)
                .add_modifier(Modifier::BOLD)
        } else if mine {
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        // Only the letter is styled; the column after it is plain, like the one after the
        // connector. A terminal writes a style only where it changes, so after a plain
        // column the connector's own weight is written out again rather than carried over
        // from the letter.
        let mut spans = vec![Span::raw("  ")];
        match letter {
            Some(Letter { key, lit }) => spans.extend([
                Span::styled(
                    key.to_string(),
                    if lit {
                        Style::default()
                            .fg(app.theme.yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        dim
                    },
                ),
                Span::raw(" "),
            ]),
            None => spans.push(Span::raw("  ")),
        }
        spans.push(Span::styled(
            if last { "\u{2514}" } else { "\u{251c}" },
            style,
        ));
        spans.push(Span::raw(" "));
        spans
    }
}

/// A row's number, a header's or a flat row's, and whether it is a key that works right now
/// ([`App::label_works`]).
#[derive(Clone)]
struct Number {
    text: String,
    key: bool,
}

/// A number as the tree draws it: in the theme's yellow, bold, while pressing it works (a yellow
/// number or letter is a key that works now), dim when it does not (an overlay takes every key
/// itself), and a blank where a row has none. While you type, in either preset, typing a number
/// works as pressing it does, so the numbers stay yellow.
fn number_span(number: Option<Number>, theme: &Theme) -> Span<'static> {
    match number {
        Some(Number { text, key: true }) => Span::styled(
            text,
            Style::default()
                .fg(theme.yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Some(Number { text, key: false }) => Span::styled(text, Style::default().fg(theme.dim)),
        None => Span::styled(" ", Style::default().fg(theme.dim)),
    }
}

/// The letter a child row wears in its gutter, and whether it is lit: a key, in the session
/// the cursor is in ([`row_prefix`]).
#[derive(Clone, Copy)]
struct Letter {
    key: char,
    lit: bool,
}

/// What one child row draws, apart from the three strings [`row_line`] highlights against
/// the query. Each column carries its own style because a row that needs you is drawn in
/// the attention colour end to end, and a quiet one in three different quiet colours.
struct RowParts {
    prefix: Vec<Span<'static>>,
    glyph: String,
    glyph_style: Style,
    what_style: Style,
    target_style: Style,
    /// The state and how long it has held, `blocked 38m`, on an agent row only.
    tail: Option<Tail>,
    /// The session name a flat list writes after the row's own columns.
    session_style: Style,
    /// The agent's own pane title, when `[ui] titles` is on and it has one.
    title: Option<TitleColumn>,
    /// Whether the row's agent runs in a container (`@agent_container`): [`MARK`] after its name.
    boxed: bool,
}

/// An agent row's title: its text, its colour, and where a match in it starts among the positions
/// of the row's match, once they are in characters ([`tree::title_column`]).
struct TitleColumn {
    text: String,
    style: Style,
    from: usize,
}

/// An agent row's state column, `blocked 38m`: its first `word` characters are the state word,
/// which the filter matches ([`tree::state_label`]); the time after it is not matched.
struct Tail {
    text: String,
    word: usize,
    style: Style,
}

/// What a child row's *where* column holds, and how it fits the column.
enum Target<'a> {
    /// A pane's `2:zsh.1`, cut to the column with an `\u{2026}` when it is wider ([`elide`]).
    Cut(&'a str),
    /// A parked worktree's `worktree, closed`, drawn whole though it is wider ([`fill`]).
    Whole(&'a str),
}

/// The columns every child row shares, and the session name a flat list appends because it
/// has no header to sit under.
///
/// `matched`'s positions are into [`tree::row_text`], which puts the session name first on
/// every child row, so each text column takes the positions of its own characters there
/// ([`column_hits`]), a flat row's session name at its end included, and the title the
/// positions after the row's words ([`tree::title_column`]). A match in the part of a title that
/// the panel cuts off lights the `\u{2026}` there ([`truncate_row`]). A long *what* or *where*
/// is cut with an `\u{2026}`, and a match the cut hides lights it ([`cut_hits`]). The state
/// word comes after the target in that text, as it does on the row.
fn row_line(
    app: &App,
    parts: RowParts,
    what: &str,
    target: Target,
    session: &str,
    matched: Option<&Match>,
) -> Line<'static> {
    let flat = app.flat_now();
    let offset = if session.is_empty() {
        0
    } else {
        session.chars().count() + 1
    };
    let what_len = what.chars().count();
    let mut spans = parts.prefix;
    spans.push(Span::styled(parts.glyph, parts.glyph_style));
    spans.push(Span::raw(" "));
    if parts.boxed {
        spans.extend(boxed_name(
            what,
            matched,
            offset,
            parts.what_style,
            &app.theme,
        ));
    } else {
        spans.extend(highlight_at(
            &elide(what, WHAT_COLUMNS),
            &cut_hits(what, WHAT_COLUMNS, column_hits(matched, offset, what_len)),
            parts.what_style,
            &app.theme,
        ));
    }
    spans.push(Span::raw(" "));
    let target_from = offset + what_len + 1;
    let target_len = match target {
        Target::Cut(text) | Target::Whole(text) => text.chars().count(),
    };
    let (drawn, hits) = match target {
        Target::Cut(text) => (
            elide(text, WHERE_COLUMNS),
            cut_hits(
                text,
                WHERE_COLUMNS,
                column_hits(matched, target_from, text.chars().count()),
            ),
        ),
        Target::Whole(text) => (
            fill(text, WHERE_COLUMNS),
            column_hits(matched, target_from, text.chars().count()),
        ),
    };
    spans.extend(highlight_at(&drawn, &hits, parts.target_style, &app.theme));
    if let Some(tail) = parts.tail {
        spans.push(Span::raw(" "));
        spans.extend(highlight_at(
            &pad(&tail.text, STATE_COLUMNS),
            &column_hits(matched, target_from + target_len + 1, tail.word),
            tail.style,
            &app.theme,
        ));
    }
    // The session first and the title last, so that when the panel is too narrow for both
    // it is the title — the one column with nothing else to say — that takes the cut. The
    // session's name is the start of the row's text, so a match in it is lit here.
    if flat && !session.is_empty() {
        spans.push(Span::styled("  ", parts.session_style));
        spans.extend(highlight_at(
            session,
            &column_hits(matched, 0, session.chars().count()),
            parts.session_style,
            &app.theme,
        ));
    }
    if let Some(title) = parts.title {
        let hits = column_hits(matched, title.from, title.text.chars().count());
        if hits.is_empty() {
            spans.push(Span::styled(format!("  {}", title.text), title.style));
        } else {
            spans.push(Span::styled("  ", title.style));
            spans.extend(highlight_at(&title.text, &hits, title.style, &app.theme));
        }
    }
    Line::from(spans)
}

/// The name column of an agent in a container: its name, cut as [`elide`] cuts a long one but to
/// leave [`MARK`] its columns (`openco… 📦`), a match lighting the letters drawn and, for one in
/// the part cut away, the ellipsis ([`cut_hits`]); then the marker, with no style of the row's
/// and never lit, so the terminal draws the emoji in its own colours; then the spaces that keep
/// the place column where it is on every row.
fn boxed_name(
    what: &str,
    matched: Option<&Match>,
    offset: usize,
    style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let room = WHAT_COLUMNS - MARK_COLUMNS;
    // `truncate`, not `elide`: `elide` pads the cut name to its columns, and the marker
    // follows the name itself.
    let name = truncate(what, room as u16);
    let hits = cut_hits(
        what,
        room,
        column_hits(matched, offset, what.chars().count()),
    );
    let mut spans = highlight_at(&name, &hits, style, theme);
    spans.push(Span::raw(MARK));
    spans.push(Span::raw(
        " ".repeat(WHAT_COLUMNS.saturating_sub(name.width() + MARK_COLUMNS)),
    ));
    spans
}

/// How long a state has held: `now` under a minute, then whole
/// minutes, hours and days. A `since` in the future — a clock that stepped back, or a tmux
/// option written by a machine an hour ahead — reads `now` rather than an error.
pub fn elapsed_label(since: u64, now: u64) -> String {
    let seconds = now.saturating_sub(since);
    if seconds < 60 {
        "now".to_string()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// The wall clock in unix seconds, which is what `@agent_state_since` is written in. `0`
/// before the epoch, where the arithmetic above reads `now` anyway.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `lines` with `prefix` in front of the first, and `rest` (or as many blank columns when it is
/// `None`) in front of each of the others, so a template that draws more than one line for a row
/// keeps them under its first. No prefix leaves the lines as they are.
fn prefixed(
    prefix: Vec<Span<'static>>,
    rest: Option<Vec<Span<'static>>>,
    lines: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    if prefix.is_empty() {
        return lines;
    }
    if lines.is_empty() {
        // Every token of the row was empty: the key it carries is still drawn.
        return vec![Line::from(prefix)];
    }
    let width: usize = prefix.iter().map(|s| s.content.width()).sum();
    lines
        .into_iter()
        .enumerate()
        .map(|(at, line)| {
            let mut spans = if at == 0 {
                prefix.clone()
            } else {
                rest.clone()
                    .unwrap_or_else(|| vec![Span::raw(" ".repeat(width))])
            };
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// One entry's configured rows, herdr's way; `hot` is the style of an agent that needs you
/// ([`rows::render_row_with`]). A row whose every token is empty (a session
/// with no repository, say) is dropped rather than drawn as a blank line.
fn template_lines(
    template: &[Vec<RowToken>],
    src: &dyn rows::TokenSource,
    ctx: &rows::Ctx,
    hot: Option<Style>,
) -> Vec<Line<'static>> {
    template
        .iter()
        .map(|row| rows::render_row_with(row, src, ctx, hot))
        .filter(|line| !line.spans.is_empty())
        .collect()
}

/// How many of a session's agents are in each state, in [`COUNTS`] order, skipping the
/// states nothing is in.
fn state_counts(app: &App, space: &SpaceRow) -> Vec<(AgentState, usize)> {
    // A machine that is away counts nothing: what it last said is not what is happening.
    if app.snap.greyed(space.machine) {
        return Vec::new();
    }
    COUNTS
        .into_iter()
        .filter_map(|state| {
            let count = app
                .snap
                .agents
                .iter()
                .filter(|a| a.session_id == space.session_id && a.state == Some(state))
                .count();
            (count > 0).then_some((state, count))
        })
        .collect()
}

/// `text`, with the characters the query matched in yellow bold and the rest in `style`.
///
/// `matched`'s positions are character indices into the row's [`tree::row_text`]; `from` is
/// where this column starts within it. A position outside what is drawn (the session's name in
/// front of a child row's text, say, which the tree draws on the header alone) is simply not
/// highlighted.
fn highlight(
    text: &str,
    matched: Option<&Match>,
    from: usize,
    style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let positions: Vec<usize> = match matched {
        Some((_, indices)) => indices.iter().filter_map(|i| i.checked_sub(from)).collect(),
        None => Vec::new(),
    };
    highlight_at(text, &positions, style, theme)
}

/// `row`'s match, as split off its address ([`tree::split_address`]), with its positions moved
/// from the matcher's count to the characters the row draws ([`lit_chars`]): the row's own words
/// and, for an agent's row, the title it draws after them.
fn drawn_match(app: &App, row: &Row, matched: Option<Match>) -> Option<Match> {
    let own = tree::own_words(&app.snap, &app.worktrees, row, app.cfg.ui.show_git);
    let title = match row {
        Row::Pane(pane) => app
            .snap
            .panes
            .get(*pane)
            .and_then(|pane| tree::drawn_title(&app.snap, pane, app.cfg.ui.titles)),
        _ => None,
    };
    lit_chars(&own, title.as_deref(), matched)
}

/// `matched` with its positions moved from the matcher's count to characters, the one place the
/// two differ. The matcher indexes a text that is not ASCII by grapheme, naming its first
/// character (a letter and its accent, `\u{26a0}\u{fe0f}`, a ZWJ emoji are one position), and a
/// text whose graphemes all start with an ASCII character by byte; the renderer counts
/// characters. A position in `own` (the row's own words, counted from their start) lights every
/// character of its grapheme; one at or after the title's start is counted in the title's
/// graphemes the same way. The title's positions begin where the matcher counts the end of `own`
/// ([`tree::title_start`], bytes for some texts), and they are moved to where the row draws the
/// title, after `own`'s characters ([`tree::title_column`]). Any other position (the words a row
/// has of its header's, which light nothing) is kept.
fn lit_chars(own_words: &str, title: Option<&str>, matched: Option<Match>) -> Option<Match> {
    let (score, positions) = matched?;
    let own = units(own_words);
    let from = tree::title_start(own_words);
    let column = tree::title_column(own_words);
    let title = title.map(units);
    let mut out = Vec::new();
    for at in positions {
        if let Some(chars) = own.get(at) {
            out.extend(chars.clone());
        } else if let Some(units) = &title
            && let Some(chars) = at.checked_sub(from).and_then(|at| units.get(at))
        {
            out.extend(chars.start + column..chars.end + column);
        } else {
            out.push(at);
        }
    }
    out.sort_unstable();
    out.dedup();
    Some((score, out))
}

/// The characters of `text` each position the matcher counts in it stands for ([`lit_chars`]).
fn units(text: &str) -> Vec<std::ops::Range<usize>> {
    let chars = text.chars().count();
    if text.is_ascii() {
        return (0..chars).map(|at| at..at + 1).collect();
    }
    if text
        .graphemes(true)
        .all(|grapheme| grapheme.starts_with(|c: char| c.is_ascii()))
    {
        return text
            .chars()
            .enumerate()
            .flat_map(|(at, c)| std::iter::repeat_n(at..at + 1, c.len_utf8()))
            .collect();
    }
    let mut at = 0;
    text.graphemes(true)
        .map(|grapheme| {
            let end = at + grapheme.chars().count();
            let range = at..end;
            at = end;
            range
        })
        .collect()
}

/// The positions of `matched` that fall in one column of a row: those of its own `len`
/// characters, which start at `from` in the row's [`tree::row_text`], counted from the
/// column's start. A position past the column is the next column's and is not lit here.
fn column_hits(matched: Option<&Match>, from: usize, len: usize) -> Vec<usize> {
    match matched {
        Some((_, indices)) => indices
            .iter()
            .filter_map(|i| i.checked_sub(from))
            .filter(|&i| i < len)
            .collect(),
        None => Vec::new(),
    }
}

/// `hits` in `text` as [`elide`] draws it in `columns`: those in the part it keeps, and its
/// `\u{2026}` in place of any in the part it cuts away, so a row that a match in the hidden
/// end of a long name keeps on screen shows why it is there.
fn cut_hits(text: &str, columns: usize, hits: Vec<usize>) -> Vec<usize> {
    if columns == 0 || text.width() <= columns {
        return hits;
    }
    let kept = cut(text, columns - 1).chars().count();
    let hidden = hits.iter().any(|&i| i >= kept);
    let mut out: Vec<usize> = hits.into_iter().filter(|&i| i < kept).collect();
    if hidden {
        out.push(kept);
    }
    out
}

/// `text`, with the characters at `positions` in yellow bold and the rest in `style`.
fn highlight_at(
    text: &str,
    positions: &[usize],
    style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if positions.is_empty() {
        return vec![Span::styled(text.to_string(), style)];
    }
    let hit = Style::default()
        .fg(theme.yellow)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (index, c) in text.chars().enumerate() {
        let is_hit = positions.contains(&index);
        if is_hit != run_hit && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if run_hit { hit } else { style },
            ));
        }
        run_hit = is_hit;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hit { hit } else { style }));
    }
    spans
}

// -------------------------------------------------------------------- the preview

fn render_preview(f: &mut Frame, area: Rect, app: &mut App) {
    app.hits.preview = area;
    let theme = &app.theme;
    // A row of a machine that is away has no screen to show: its words say so, and no capture age
    // is told of a capture nobody draws.
    let away = preview_machine(app)
        .filter(|machine| !machine.live)
        .map(|machine| machine.name.clone());
    let age = app
        .preview_age
        .filter(|age| away.is_none() && *age > STALE_AFTER)
        .map(|age| format!(" {}", words::capture_age(age.as_secs())))
        .unwrap_or_default();
    let title = match (preview_path(app), preview_sandbox(app)) {
        (Some(path), Some(sandbox)) => fit_title(
            format!(" preview \u{b7} {path} \u{b7} {sandbox}{age} "),
            area.width,
        ),
        (Some(path), None) => format!(" preview \u{b7} {path}{age} "),
        (None, _) => format!(" preview{age} "),
    };
    let block = panel_block(theme, &title, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    match app.preview.as_ref().filter(|_| away.is_none()) {
        Some(text) => {
            let lines = if app.preview_wraps() {
                preview::wrap(text, inner.width)
            } else {
                text.lines.clone()
            };
            // Once, before the count: the blank rows `capture-pane` pads a screen with are
            // never drawn, so a limit that counted them would give the wheel dead notches.
            let lines = preview::trim_trailing_blank(lines);
            app.preview_max_scroll = preview::max_offset(lines.len(), inner.height);
            app.preview_scroll = app.preview_scroll.min(app.preview_max_scroll);
            // The pane in its own colours: what it leaves at the terminal's default colours is
            // drawn at the terminal's default colours, as in the pane itself, so a translucent
            // terminal shows through the preview as it does through the pane. On the panel's
            // colour instead, text after a colour reset would sit in a box of another colour.
            f.render_widget(
                Paragraph::new(preview::window(lines, inner.height, app.preview_scroll))
                    .style(Style::default().fg(Color::Reset).bg(Color::Reset)),
                inner,
            );
        }
        None => {
            app.preview_max_scroll = 0;
            let words = match &away {
                Some(machine) => words::no_preview(machine),
                None => PREVIEW_PLACEHOLDER.to_string(),
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" {words}"),
                    Style::default().fg(theme.dim),
                ))),
                inner,
            )
        }
    }
}

/// The machine the cursor's row is on, `None` for a row of this machine's and for a row that is
/// no session's.
fn preview_machine<'a>(app: &'a App) -> Option<&'a data::MachineHeader> {
    let space = tree::space_of(&app.snap, app.current()?)?;
    let machine = app.snap.spaces.get(space)?.machine?;
    app.snap.machines.get(machine)
}

/// The directory the preview's title names: the cursor session's path, or, on a parked
/// worktree, which has no session yet, the checkout Enter would open. A path under your
/// home folder reads with `~`, as the project picker writes it.
fn preview_path(app: &App) -> Option<String> {
    match app.current()? {
        Row::ClosedWorktree { record, .. } => app
            .worktrees
            .get(*record)
            .map(|r| naming::tilde(&r.path, &app.home)),
        Row::Project(at) => app.projects.get(*at).map(|project| project.path.clone()),
        _ => app
            .selected_space()
            .map(|space| naming::tilde(&space.session_path, &app.home)),
    }
}

/// `whole`, a title for a panel `panel` columns wide: cut with an ellipsis when it does not fit
/// the room the border leaves, as cut words would read as whole ones.
fn fit_title(whole: String, panel: u16) -> String {
    let room = panel.saturating_sub(4);
    if whole.width() > usize::from(room) {
        truncate(&whole, room)
    } else {
        whole
    }
}

/// What the preview's title says of the sandbox the cursor's agent runs in. A container taw saw
/// gives the box, the runtime and the profile when taw started it. A profile whose container taw
/// cannot see (the agent was started through a profile and no runtime taw knows is in front)
/// gets no box and no claim of one: the title names the profile and says why the box is
/// missing. `None` for any other row.
fn preview_sandbox(app: &App) -> Option<String> {
    let Some(Row::Pane(at)) = app.current() else {
        return None;
    };
    let agent = app
        .snap
        .panes
        .get(*at)?
        .agent
        .and_then(|a| app.snap.agents.get(a))?;
    Some(
        match (agent.container.as_deref(), agent.sandbox.as_deref()) {
            (Some(runtime), Some(profile)) => {
                format!("\u{1f4e6} in a sandbox: {runtime}, profile {profile}")
            }
            (Some(runtime), None) => {
                format!("\u{1f4e6} in a sandbox: {runtime}, not started by taw")
            }
            (None, Some(profile)) => format!("sandbox profile {profile}, no container seen"),
            (None, None) => return None,
        },
    )
}

fn panel_block(theme: &Theme, title: &str, focused: bool) -> Block<'static> {
    let border = if focused { theme.accent } else { theme.dim };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(if focused { theme.accent } else { theme.dim })
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel_bg))
}

// ------------------------------------------------------------------- scrolling

/// The first row to draw.
///
/// `scroll` is the viewport the wheel owns, so this only ever narrows it to a legal
/// offset — never past [`max_scroll`], where the last row sits at the bottom of the panel.
/// When `follow_sel` is set (the cursor just moved), it additionally moves the viewport
/// the smallest distance that brings row `sel` into view.
pub fn keep_visible(
    heights: &[u16],
    sel: usize,
    scroll: usize,
    height: u16,
    follow_sel: bool,
) -> usize {
    if heights.is_empty() {
        return 0;
    }
    let mut scroll = scroll.min(max_scroll(heights, height));
    if !follow_sel {
        return scroll;
    }
    let sel = sel.min(heights.len() - 1);
    scroll = scroll.min(sel);
    while scroll < sel && span(&heights[scroll..=sel]) > u32::from(height) {
        scroll += 1;
    }
    scroll
}

/// The furthest a panel may scroll: the first offset from which everything left fits, so
/// the wheel cannot push the list off the bottom and leave an empty panel. `0` when the
/// whole list already fits.
pub fn max_scroll(heights: &[u16], height: u16) -> usize {
    (0..heights.len())
        .find(|&offset| span(&heights[offset..]) <= u32::from(height))
        .unwrap_or_else(|| heights.len().saturating_sub(1))
}

/// The total height of a run of rows, in `u32` so a long list cannot overflow.
fn span(heights: &[u16]) -> u32 {
    heights.iter().copied().map(u32::from).sum()
}

// ----------------------------------------------------------------------- text

/// `text` in exactly `columns` display columns: padded with spaces, or cut when it is
/// already wider. This is what aligns the *where* column.
fn pad(text: &str, columns: usize) -> String {
    let width = text.width();
    if width >= columns {
        return cut(text, columns);
    }
    format!("{text}{}", " ".repeat(columns - width))
}

/// `text` in at least `columns` display columns, padded with spaces. Unlike [`pad`] it
/// never cuts: a worktree's `worktree, closed` is wider than the *where* column and is
/// drawn whole, the way it always has been, rather than mangled to fit.
fn fill(text: &str, columns: usize) -> String {
    let width = text.width();
    if width >= columns {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(columns - width))
}

/// `text` in exactly `columns` display columns, cut with a trailing `\u{2026}` when it was
/// too wide. [`pad`] cuts too, but silently: a window renamed `documentation-pad` would
/// read as the shorter `documentatio`, a name it does not have.
fn elide(text: &str, columns: usize) -> String {
    let columns_u16 = u16::try_from(columns).unwrap_or(u16::MAX);
    // `truncate` can come back a column short when a double-width character straddles the
    // cut, so `fill` squares it up; when nothing was cut, `fill` is the whole job.
    fill(&truncate(text, columns_u16), columns)
}

/// `text` cut to at most `columns` display columns, with nothing added.
fn cut(text: &str, columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    // By grapheme, the width the screen gives it: a warning sign and its emoji selector are two
    // columns, though their characters' widths add up to one.
    for grapheme in text.graphemes(true) {
        let width = grapheme.width();
        if used + width > columns {
            break;
        }
        used += width;
        out.push_str(grapheme);
    }
    out
}

/// Cuts `text` to `width` display columns, ending in `…` when something was cut.
fn truncate(text: &str, width: u16) -> String {
    let width = width as usize;
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}\u{2026}", cut(text, width - 1))
}

/// [`truncate_line`] for the tree's row `index`, whose `\u{2026}` is lit when the cut hides
/// characters of its title that the query matched, as the one a long name's or window name's
/// cut is ([`cut_hits`]): the row is there because of them and nothing else lit says so. A greyed
/// row (a machine that is away) lights it as it lights its other matches ([`greyed_line`]).
fn truncate_row(app: &App, index: usize, line: Line<'static>, width: u16) -> Line<'static> {
    let dim = Style::default().fg(app.theme.dim);
    let hidden = title_hidden(app, index, &line, width);
    let style = match hidden {
        None => dim,
        Some(true) => dim.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        Some(false) => Style::default()
            .fg(app.theme.yellow)
            .add_modifier(Modifier::BOLD),
    };
    truncate_line_with(line, width, style)
}

/// Whether cutting `line`, row `index`'s, to `width` columns hides a character of its title that
/// the query matched: `None` when it does not, else `Some(greyed)`. The title is the end of the
/// line: it is only looked for there, and never on a row a configured template draws.
fn title_hidden(app: &App, index: usize, line: &Line<'static>, width: u16) -> Option<bool> {
    if line_width(line) <= width || width == 0 {
        return None;
    }
    let row @ Row::Pane(pane) = app.rows.rows.get(index)? else {
        return None;
    };
    let pane = app.snap.panes.get(*pane)?;
    // A row a template draws lights nothing: the positions of its text are not the title's.
    let kind = pane
        .agent
        .and_then(|a| app.snap.agents.get(a))?
        .kind
        .as_deref();
    if rows::agents_template(&app.cfg.ui.sidebar, &Sidebar::default(), kind).is_some() {
        return None;
    }
    let title = tree::drawn_title(&app.snap, pane, app.cfg.ui.titles)?;
    let drawn: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    if !drawn.ends_with(&title) {
        return None;
    }
    let (matched, _) = tree::split_address(
        app.rows.scores.get(index).and_then(|m| m.as_ref()),
        app.row_address(index).chars().count(),
    );
    let matched = drawn_match(app, row, matched);
    let own = tree::own_words(&app.snap, &app.worktrees, row, app.cfg.ui.show_git);
    let hits = column_hits(
        matched.as_ref(),
        tree::title_column(&own),
        title.chars().count(),
    );
    // The columns the cut keeps, the `\u{2026}` taking the last of them.
    let room = usize::from(width) - 1;
    let mut column = usize::from(line_width(line)) - title.width();
    // By grapheme, as the screen draws them: a warning sign and its emoji selector are two
    // columns, though their characters' widths add up to one. The hits are in characters.
    let mut hidden = false;
    let mut at = 0;
    for grapheme in title.graphemes(true) {
        let chars = grapheme.chars().count();
        let wide = grapheme.width();
        hidden |= (at..at + chars).any(|char| hits.contains(&char)) && column + wide > room;
        column += wide;
        at += chars;
    }
    let greyed = tree::space_of(&app.snap, row)
        .and_then(|space| app.snap.spaces.get(space))
        .is_some_and(|space| app.snap.greyed(space.machine));
    hidden.then_some(greyed)
}

/// [`truncate`], for a line whose spans each carry their own colour.
fn truncate_line(line: Line<'static>, width: u16, dim: Color) -> Line<'static> {
    truncate_line_with(line, width, Style::default().fg(dim))
}

/// [`truncate_line`] with the `\u{2026}` that marks the cut in `ellipsis`.
fn truncate_line_with(line: Line<'static>, width: u16, ellipsis: Style) -> Line<'static> {
    if line_width(&line) <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let room = width as usize - 1;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let kept = cut(&span.content, room - used);
        // Whether the cut fell inside this span: nothing after it is drawn.
        let short = kept.width() < span.content.width();
        used += kept.width();
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if short {
            // A wide character that would cross the cut is not drawn at all: its first column
            // is a space, and the ellipsis follows.
            out.push(Span::styled(" ".repeat(room - used), span.style));
            break;
        }
        if used >= room {
            break;
        }
    }
    out.push(Span::styled("\u{2026}", ellipsis));
    Line::from(out)
}

mod app_line;
mod machine_header;

#[cfg(test)]
mod tests;
