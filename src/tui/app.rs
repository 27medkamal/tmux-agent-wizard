//! The popup's state and every key and mouse binding, as a pure state machine.
//!
//! `App` reads a [`Snapshot`] and answers key and mouse events; it never runs a tmux
//! command. Anything that changes tmux leaves here as an [`Action`] for the run loop to
//! execute (`super::actions`). That is what makes the whole keymap unit-testable against
//! a hand-built snapshot, and it is the shape the "tmux is the database" rule
//! wants: the sidebar reads state, it does not compute it.
//!
//! There is one list, not two panels: [`tree::build`] turns the snapshot into rows and
//! [`App::sel`] is a position in them. You are the cursor: it opens on the pane the popup
//! was opened from ([`App::home_row`]) and moves only when you move it — the rows keep
//! tmux's order, and nothing re-sorts while the popup is open. Keys
//! arrive as [`KeyAction`]s from a [`Keymap`], so one dispatch serves the modal preset, the
//! search preset and any `[keys]` override; the per-row half — what `Enter`, `x` and `r`
//! mean on *this* row — lives in [`actions_for_row`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::text::Text;

use crate::attention::{self, Group};
use crate::config::{AgentPanelSort, Config, Depth, KeysPreset, Layout};
use crate::session::worktree::WorktreeRecord;
use crate::store::AgentState;
use crate::tui::actions::Action;
use crate::tui::agent_view::{self, AgentView};
use crate::tui::data::{AgentRow, PaneRow, Snapshot, SpaceRow};
use crate::tui::input::{self, Input, WordBoundary};
use crate::tui::keys::{self, Action as KeyAction, Keymap};
use crate::tui::overlays::{Answers, Confirm, Overlay, Response, RowAction, Scroll};
use crate::tui::picker::Project;
use crate::tui::prefix::{Chord, ChordOutcome};
use crate::tui::rows;
use crate::tui::theme::Theme;
use crate::tui::tree::{self, Query, Row};

mod actions_for_row;
pub use actions_for_row::SESSION_LAST_PANE;
mod projects;
pub mod walk;

/// Two clicks on the same row within this window are a double-click (go).
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How long a toast stays on the popup's hint line.
pub const TOAST_TTL: Duration = Duration::from_secs(4);

/// What the quit keys of an app the doorway started say they do: the terminal goes back to the tmux it came
/// from when the app ends.
pub const BACK_TO_TMUX: &str = "back to tmux";

/// How long Enter does nothing after a close (kill-pane, kill-session, git worktree remove)
/// has run. The close puts the cursor on whichever row slid into the closed one's place, and
/// the Enter a typist puts behind the confirmation's `y` — or a second Enter, pressed while a
/// slow remove kept the confirmation on screen — would otherwise go there and close the popup.
pub const SETTLE: Duration = Duration::from_millis(750);

/// The most directories the `/` filter lists under the sessions (`[ui] projects_in_filter`):
/// the first of what the project picker would list for the same text, which lists ten.
pub const FILTER_PROJECTS: usize = 8;

/// The depths `h` and `l` step through, shallowest first.
const DEPTHS: [Depth; 3] = [Depth::Sessions, Depth::Agents, Depth::All];

/// The letters that name rows: `a`–`e` are drawn beside the first five rows of every session
/// ([`App::row_letters`]) and jump to those of the session the cursor is in
/// ([`App::lettered_rows`]). `f` onwards are action keys and never name a row.
pub const JUMP_LETTERS: &str = "abcde";

/// The letter row `index` wears among `lettered`, rows with their letters as
/// [`App::row_letters`] or [`App::lettered_rows`] returned them, `None` for a row not among
/// them. Taking the rows rather than the app lets a caller that asks about every row — a frame
/// of the tree — work them out once instead of once a row.
pub fn letter_among(lettered: &[(usize, char)], index: usize) -> Option<char> {
    lettered
        .iter()
        .find_map(|&(row, letter)| (row == index).then_some(letter))
}

/// The search preset's query line: empty, and a pure fzf search, so its word keys (Alt-B, Alt-F, Alt-D) are fzf's.
/// Made here for the start and for every time the query is emptied, so that the boundary is chosen once.
fn search_query() -> Input {
    Input::new("").with_word_boundary(WordBoundary::Fzf)
}

/// Whether a filter takes `key` as text rather than as a command: a plain character, with
/// no ctrl and no alt. Everything else — a chord, `Tab`, an arrow, a function key — can
/// still mean what the keymap says while a query is being typed.
fn is_text(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(_))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// `key` as the query line of `preset` spells it. The search preset is a pure fzf search, so
/// where herdr's text fields and fzf mean different things by a key it keeps fzf's: fzf's
/// ctrl-backspace deletes one character (`backward-delete-char`), where herdr's cuts a word.
/// Every other key is one both mean the same by. (fzf's ctrl-d is also an end of input on an
/// empty query, which is no edit: [`App::is_end_of_input`] decides that before a key gets here.)
fn field_spelling(preset: KeysPreset, key: KeyEvent) -> KeyEvent {
    if preset == KeysPreset::Search
        && key.code == KeyCode::Backspace
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return KeyEvent::new(KeyCode::Backspace, key.modifiers - KeyModifiers::CONTROL);
    }
    key
}

/// Moves `sel` by `delta` within `len`, wrapping at both ends. `0` for an empty list.
pub fn step(sel: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    let next = (sel as isize + delta).rem_euclid(len);
    next as usize
}

/// The entry of an ascending `list` of row positions just past `from` in the direction
/// `dir`, wrapping to the far end when there is none, and `None` for an empty list.
/// `from` itself is the answer only when it is the list's only entry — the step
/// [`App::next_up_row`] takes.
fn step_wrapping(list: &[usize], from: usize, dir: isize) -> Option<usize> {
    let ahead = list
        .iter()
        .copied()
        .filter(|&at| if dir > 0 { at > from } else { at < from });
    if dir > 0 {
        ahead.min().or_else(|| list.first().copied())
    } else {
        ahead.max().or_else(|| list.last().copied())
    }
}

/// Where the mouse can land, recorded by the view every frame.
#[derive(Clone, Debug, Default)]
pub struct Hits {
    pub tree: PanelHit,
    /// The preview panel, border included, for the wheel.
    pub preview: Rect,
    /// The open overlay's box, and its clickable rows.
    pub overlay: OverlayHit,
}

/// Where an open overlay can be clicked: its box, and `(y, item index)` per clickable row.
#[derive(Clone, Debug, Default)]
pub struct OverlayHit {
    pub area: Rect,
    /// `(y, item index)` for the open overlay's clickable rows.
    pub rows: Vec<(u16, usize)>,
}

/// The tree panel's clickable geometry.
#[derive(Clone, Debug, Default)]
pub struct PanelHit {
    pub area: Rect,
    /// `(y, position in [`App::rows`])` for every visible row line.
    pub rows: Vec<(u16, usize)>,
    /// The line of the filter's directories' header, when one is drawn. It is a line of the
    /// panel and no row, so nothing selects it, and a right-click on it does nothing.
    pub header: Option<u16>,
}

impl PanelHit {
    fn at(&self, x: u16, y: u16) -> Option<usize> {
        self.area
            .contains((x, y).into())
            .then(|| self.rows.iter().find(|(row, _)| *row == y).map(|(_, i)| *i))
            .flatten()
    }

    /// Whether `(x, y)` is on the header of the filter's directories.
    fn on_header(&self, x: u16, y: u16) -> bool {
        self.header == Some(y) && self.area.contains((x, y).into())
    }
}

/// How a row is identified across a snapshot swap: a [`Row`] is indices, and indices move
/// as panes come and go, so the cursor is kept on a *thing* rather than on a position.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowId {
    Pane(String),
    Session(String),
    Worktree(String),
    /// A directory the filter lists, by its path in tilde form.
    Project(String),
    /// A machine's header, by the machine's name (`None` for this machine's own rows): the cursor is never on
    /// one, but the rows are all compared by it. Not by place, which moves when `[machines]` is edited.
    Machine(Option<String>),
    /// The row that starts a session on a machine that has none, by the machine's name: its place in the list
    /// moves when `[machines]` is edited while the app runs, and the cursor must stay on that machine's row.
    NoSession(String),
}

/// How the app child's screen ends, which is what the supervisor's terminal does next. Only the
/// app reads it ([`crate::tui::run_app`]); the popup closes the same way whichever it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEnd {
    /// Go back in: the keys that close the popup (`q`, `Esc`, ctrl-d). With no row you came out of
    /// there is nowhere to go back to, and the app ends.
    Back,
    /// Leave at once: Ctrl-C.
    Leave,
}

/// What this screen is: the popup over a tmux client, or the app that is the whole terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Popup,
    /// The app. `came_from` is the pane of the row you came out of, when you came out of one.
    App { came_from: Option<String> },
}

/// A pane of another machine under the cursor, as the preview asks for it.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct RemotePane {
    /// The list's id for it, `studio:%16`.
    pub pane: String,
    /// The machine's name.
    pub machine: String,
    /// Whether the machine is live: a machine that is away has no screen to read.
    pub live: bool,
}

/// How much of what Enter does on a connected machine's row the popup's hint line says: the machine and the key
/// that brings you back, the machine alone, or plain `go`. The line takes the longest that leaves all its hints on its
/// two lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TripWords {
    Full,
    Name,
    Plain,
}

/// The whole popup.
pub struct App<'a> {
    pub cfg: &'a Config,
    pub theme: Theme,
    pub snap: Snapshot,
    /// Tracked worktrees: the parked ones are rows of their own. Refreshed with the data
    /// tick by the run loop, which is what has `Paths`.
    pub worktrees: Vec<WorktreeRecord>,
    /// The rows one frame draws, and how each one matched the filter.
    pub rows: tree::Rows,
    /// The label every row wears, worked out from the rows as they are with nothing typed
    /// ([`App::rebuild`]), so that a filter only hides rows and never moves a label: a number or
    /// a letter is the same typed in the filter as pressed outside it. As if `[ui] labels` were
    /// on: [`App::label_index`] and [`App::row_letters`] read the setting when they are asked.
    labels: tree::Labels,
    /// The cursor: a position in [`App::rows`].
    pub sel: usize,
    /// The row the cursor was last moved to by anything but a depth or layout change, with
    /// its session: after `h`/`l`/`f` reshape the rows, [`App::keep_cursor`] prefers it
    /// when it is a leaf of the cursor's session that is drawn again.
    anchor: Option<(RowId, Option<String>)>,
    /// The row the cursor was on, and its session, when the filter was opened: where `Esc` puts the
    /// cursor back when the query had selected no row (nothing matched, or only a directory).
    before_filter: Option<(RowId, Option<String>)>,
    /// First visible row. The wheel owns this; the view only clamps it to a legal range,
    /// and pulls the cursor into view when [`App::follow_sel`] is set.
    pub scroll: usize,
    /// Whether the next frame should scroll the cursor into view.
    ///
    /// Set when the cursor moves (a key, a click, or a row that moved under it) and
    /// cleared by the wheel: without this, every frame would drag the viewport back to
    /// the cursor and the wheel would appear to do nothing.
    pub follow_sel: bool,
    pub depth: Depth,
    pub layout: Layout,
    pub sort: AgentPanelSort,
    /// The filter, `Some` while a query is being kept. In the search preset it is there
    /// from the first frame and never goes away.
    pub filter: Option<Input>,
    pub keymap: Keymap,
    pub preset: KeysPreset,
    /// What `[keys]` got wrong, for the options overlay.
    pub key_diagnostics: Vec<String>,
    /// The tmux prefix chord, when the run loop could read tmux's `prefix` or `prefix2`.
    pub chord: Option<Chord>,
    pub overlay: Overlay,
    /// Whether `[ui] preview` is on. It does not say the preview is drawn: that is decided
    /// by the popup's width every frame, beside the tree when both fit and not at all when
    /// they do not ([`crate::tui::view::WIDE_COLUMNS`]).
    pub show_preview: bool,
    /// The captured screen of the pane [`App::preview_shows`] names. It is captured again
    /// at each data tick, and at once when the cursor moves to a row with another pane
    /// ([`crate::tui::follow_preview`]), so it is always the screen of the row under the
    /// cursor.
    pub preview: Option<Text<'static>>,
    /// How old [`App::preview`] is when whoever captured it knows it is not fresh: a pane of
    /// another machine, captured over a connection, may be some seconds old. `None` is fresh,
    /// as every capture of this machine's own is. [`App::set_preview`] puts it back to `None`;
    /// the capturer that knows an age sets it after.
    pub preview_age: Option<Duration>,
    /// Rows from the bottom the wheel has scrolled the preview; `0` is the tail.
    pub preview_scroll: usize,
    /// How far the wheel can scroll the preview, set by the view each frame from the
    /// wrapped line count.
    pub preview_max_scroll: usize,
    /// The pane [`App::preview`] was captured from, so [`App::set_preview`] can tell a fresh
    /// capture of the same pane (keep the scroll) from a different one (reset it), and
    /// [`App::preview_wraps`] can follow the screen on show.
    preview_for: Option<String>,
    /// The row [`App::preview_scroll`] belongs to: the row under the cursor when
    /// [`App::follow_row`] last looked. Another row there resets the offset.
    preview_row: Option<RowId>,
    pub quit: bool,
    /// Popup or app ([`App::enter_app_mode`]).
    pub mode: Mode,
    /// Which way a quit ended, for the app: Ctrl-C leaves, the quit key goes back. Set with
    /// [`App::quit`] and read by the app's loop; the popup does not look.
    pub end: Option<AppEnd>,
    /// Until when [`App::go`] rests after a close ([`App::after_action`], [`SETTLE`]).
    settle_until: Option<Instant>,
    pub hits: Hits,
    /// Text on the hint line until it expires.
    pub toast: Option<(String, Instant)>,
    /// The wall clock in unix milliseconds, when it is fixed (a test's): `None` reads the system's.
    /// What the words of a refusal age against ([`App::wall_ms`]).
    pub(crate) clock_ms: Option<u64>,
    /// The options overlay's body, and the theme warnings it reports.
    pub settings: String,
    /// zoxide's directories, queried once by the run loop for the project picker and for the
    /// filter's directories ([`App::set_zoxide`]).
    pub zoxide: Vec<String>,
    /// The last folders each other machine gave its picker in this run of the app, by the machine's name: `n`
    /// there shows them at once while it asks again.
    pub far_folders: std::collections::HashMap<String, crate::tui::picker::FarList>,
    /// The machine whose folders `n` asked for, until the screen sends the ask ([`App::take_far_ask`]).
    pub far_ask: Option<String>,
    /// The machines whose taw answered that it is older than `session candidates`, for this run: `n` there opens
    /// step 1's prompt at once. A later list from the machine takes it off (a taw updated during the run).
    pub far_old: std::collections::HashSet<String>,
    /// The directories the `/` filter lists under the sessions while it is open and
    /// `[ui] projects_in_filter` is on: what the project picker would list for the query,
    /// at most [`FILTER_PROJECTS`], with its tags. The [`Row::Project`] rows index it.
    /// Listed again only when the query's text changes ([`App::refresh_projects`]); a tick
    /// only tells the tags again ([`App::retag_projects`]).
    pub projects: Vec<Project>,
    /// The first of [`App::projects`] the section shows, when the panel is too short
    /// for all of them. Kept by the view like [`App::scroll`], and back at the top whenever
    /// the list is made again.
    pub projects_scroll: usize,
    /// `$HOME`, for the project picker's tilde forms.
    pub home: PathBuf,
    /// Whether keystrokes edit the filter. Always true in the search preset; in the modal
    /// preset it is set by `/` and cleared by the Enter that hands the tree back.
    filter_focused: bool,
    last_click: Option<(Instant, usize)>,
    /// Whether [`App::on_first_frame`] has already run.
    seen_first_frame: bool,
    /// The app was started by the doorway: when it ends the terminal goes back to tmux, so its quit keys say that
    /// ([`BACK_TO_TMUX`]) and not that they quit.
    pub back_to_tmux: bool,
    /// The last ignored-projection message this popup has already complained about.
    reported_view_error: Option<String>,
    /// The popup was started on the new-session picker ([`App::start_on_new_session_picker`])
    /// and that picker is still open. There is no tree anyone asked to see behind it, so Esc
    /// closes the popup ([`App::apply`]). Off once that picker closes: the one `n` opens
    /// later goes back to the tree.
    started_on_picker: bool,
}

impl<'a> App<'a> {
    pub fn new(cfg: &'a Config, theme: Theme, settings: String, snap: Snapshot) -> App<'a> {
        let (keymap, key_diagnostics) = keys::from_config(&cfg.keys);
        for diagnostic in &key_diagnostics {
            log::warn!("{diagnostic}");
        }
        let mut app = App {
            cfg,
            theme,
            snap,
            worktrees: Vec::new(),
            rows: tree::Rows::default(),
            labels: tree::Labels::default(),
            sel: 0,
            anchor: None,
            before_filter: None,
            scroll: 0,
            // The first frame has a cursor but no viewport yet, so it must follow.
            follow_sel: true,
            depth: cfg.ui.depth,
            layout: cfg.ui.layout,
            sort: cfg.ui.agent_panel_sort,
            // The search preset has no key that opens a filter, because it never closes
            // one: the input is the popup.
            // The search preset's query line is a pure fzf search, so its word keys are fzf's.
            filter: (cfg.keys.preset == KeysPreset::Search).then(search_query),
            keymap,
            preset: cfg.keys.preset,
            key_diagnostics,
            chord: None,
            overlay: Overlay::None,
            show_preview: cfg.ui.preview,
            preview: None,
            preview_age: None,
            preview_scroll: 0,
            preview_max_scroll: 0,
            preview_for: None,
            preview_row: None,
            quit: false,
            mode: Mode::Popup,
            end: None,
            settle_until: None,
            hits: Hits::default(),
            toast: None,
            clock_ms: None,
            settings,
            zoxide: Vec::new(),
            far_folders: std::collections::HashMap::new(),
            far_ask: None,
            far_old: std::collections::HashSet::new(),
            projects: Vec::new(),
            projects_scroll: 0,
            home: dirs::home_dir().unwrap_or_default(),
            filter_focused: false,
            last_click: None,
            seen_first_frame: false,
            back_to_tmux: false,
            reported_view_error: None,
            started_on_picker: false,
        };
        app.report_view_error();
        // The search preset's filter is open from the first frame, but its directories are
        // not listed here: they are listed once, by `set_zoxide`, when the run loop hands
        // zoxide's history over before the first draw, and a list made now would only be
        // made again then.
        app.rebuild();
        app.settle_cursor();
        app
    }

    /// Makes this screen the app's: the whole terminal, no client of its own. The cursor opens on
    /// the pane you came out of (`came_from`), else on `last` (the active pane of the session
    /// attached most lately), else it stays where it is. `came_from` is kept only when it is a pane the
    /// list draws (or its session): one that is gone is not somewhere Back could go in, and the cursor
    /// does not move for it.
    pub fn enter_app_mode(&mut self, came_from: Option<String>, last: Option<&str>) {
        // A filter typed before going in is still there on the way back and can hide the pane you were
        // in: it goes first, so the cursor is placed on the whole list.
        self.reset_filter();
        let came_from = came_from.filter(|pane| self.cursor_to_pane(pane));
        if came_from.is_none()
            && let Some(last) = last
        {
            self.cursor_to_pane(last);
        }
        self.mode = Mode::App { came_from };
    }

    /// Puts the cursor on the row of `pane`: its own row when it is drawn, else its session's header
    /// (a shell is no row at the default depth). True when the cursor moved; false when neither is
    /// drawn, or the snapshot does not hold the pane.
    pub fn cursor_to_pane(&mut self, pane: &str) -> bool {
        let own = self.rows.rows.iter().position(|row| {
            matches!(row, Row::Pane(p) if self.snap.panes.get(*p).is_some_and(|x| x.pane_id == pane))
        });
        let at = own.or_else(|| {
            let session = &self
                .snap
                .panes
                .iter()
                .find(|x| x.pane_id == pane)?
                .session_id;
            self.rows.rows.iter().position(|row| {
                matches!(row, Row::Session(_))
                    && self.row_session_id(row).as_deref() == Some(session.as_str())
            })
        });
        match at {
            Some(at) => {
                self.select(at);
                true
            }
            None => false,
        }
    }

    /// The pane the app went in from or last came back from, when there is one: what Back goes in at.
    pub fn came_from(&self) -> Option<&str> {
        match &self.mode {
            Mode::App { came_from } => came_from.as_deref(),
            Mode::Popup => None,
        }
    }

    /// Forgets the pane Back would go in at (it is gone): Back is then the end of the app.
    pub fn forget_came_from(&mut self) {
        if let Mode::App { came_from } = &mut self.mode {
            *came_from = None;
        }
    }

    /// Whether this is the app and not the popup.
    pub fn is_app(&self) -> bool {
        matches!(self.mode, Mode::App { .. })
    }

    /// Called at the top of every frame the popup draws. Only the first does anything: it
    /// says what `[keys]` got wrong.
    pub fn on_first_frame(&mut self) {
        if !self.seen_first_frame {
            self.seen_first_frame = true;
            // The first frame is the only chance to say a `[keys]` entry was dropped: the
            // log goes to a file nobody reading the popup is looking at, and the options
            // overlay is built before the keymap is. One toast, worded like any other.
            if let Some(first) = self.key_diagnostics.first().cloned() {
                let more = self.key_diagnostics.len() - 1;
                self.toast(match more {
                    0 => first,
                    n => format!("{first} (+{n} more)"),
                });
            }
        }
    }

    /// Replaces the snapshot, keeping the cursor on the same pane, session or worktree
    /// when it is still there (rows move as agents come and go, and the popup must not
    /// jump under the user's hands twice a second).
    pub fn set_snapshot(&mut self, snap: Snapshot) {
        self.set_data(Some(snap), None);
    }

    /// Replaces the tracked worktrees, whose parked entries are rows like any other.
    pub fn set_worktrees(&mut self, worktrees: Vec<WorktreeRecord>) {
        self.set_data(None, Some(worktrees));
    }

    /// Replaces whichever of the two the run loop managed to read this tick, rebuilding
    /// the rows once for both.
    ///
    /// `None` means "that read failed, keep what is already there": the popup goes on
    /// showing the last good answer, which is the right one when a pane died a moment ago
    /// or a `list-panes` raced a `kill-session`. Two reads but one rebuild — both sources
    /// feed every row, so a setter each would do the whole tree's work twice per tick.
    pub fn set_data(&mut self, snap: Option<Snapshot>, worktrees: Option<Vec<WorktreeRecord>>) {
        if snap.is_none() && worktrees.is_none() {
            return;
        }
        let keep = self.row_id();
        if let Some(snap) = snap {
            self.snap = snap;
            self.report_view_error();
            self.retag_projects();
        }
        if let Some(worktrees) = worktrees {
            self.worktrees = worktrees;
        }
        self.rebuild();
        // The letters need nothing here: they are worked out from the cursor each frame
        // ([`App::lettered_rows`]), so they follow it wherever this puts it.
        self.restore_cursor(keep);
    }

    /// Puts the cursor back on the row it was on, or as close as the new rows allow.
    ///
    /// Not [`App::select`]: a refresh is not a move the user made, so it must not touch
    /// the anchor. A session appearing above the cursor shifts its index without changing
    /// which row you are on, and the leaf you collapsed from is still the one `l` owes you.
    fn restore_cursor(&mut self, keep: Option<RowId>) {
        let at = match keep.and_then(|id| self.position_of(&id)) {
            Some(at) => self.off_header(at, 1),
            // The row is gone: the cursor stays where it was, as near as the list now reaches.
            None => self.nearest_row(self.sel.min(self.rows.rows.len().saturating_sub(1))),
        };
        // Only a real move follows the cursor: a plain 500 ms refresh must not drag a
        // wheel-scrolled viewport back to the highlight.
        if at != self.sel {
            self.sel = at;
            self.follow_sel = true;
        }
        self.follow_row();
    }

    /// Rebuilds the rows from the snapshot, the depth, the layout and the filter, with the
    /// filter's directories after them.
    ///
    /// The labels come from the rows with no query (after an Agents view's projection, at this
    /// depth and layout), and the query matches them as drawn ([`tree::match_text`]): `4a` finds
    /// session 4's row a, and a row the filter hides takes its label with it.
    pub fn rebuild(&mut self) {
        let query = self.query();
        let none = tree::Labels::default();
        // The words a configured `rows` template draws, which the filter finds a row by too.
        let defaults = crate::config::Sidebar::default();
        let drawn = |row: &Row| {
            rows::drawn_text(
                &self.cfg.ui.sidebar,
                &defaults,
                self.cfg.ui.status_indicators,
                &self.snap,
                row,
            )
        };
        let unfiltered = tree::build_drawn(
            &self.snap,
            &self.worktrees,
            self.depth,
            self.layout,
            self.sort,
            self.cfg.ui.show_git,
            self.cfg.ui.titles,
            &none,
            &drawn,
            None,
        );
        let flat = self.flat_now();
        let letters = if flat || self.depth == Depth::Sessions {
            Vec::new()
        } else {
            self.keymap.free_letters()
        };
        self.labels = tree::Labels::new(&self.snap, &unfiltered, flat, &letters, |n| {
            self.keymap.label(n)
        });
        self.rows = match query.as_ref() {
            Some(query) => tree::build_drawn(
                &self.snap,
                &self.worktrees,
                self.depth,
                self.layout,
                self.sort,
                self.cfg.ui.show_git,
                self.cfg.ui.titles,
                if self.cfg.ui.labels {
                    &self.labels
                } else {
                    &none
                },
                &drawn,
                Some(query),
            ),
            None => unfiltered,
        };
        tree::append_projects(
            &mut self.rows,
            self.projects.iter().map(|project| project.path.as_str()),
            query.as_ref(),
        );
    }

    /// The filter as a [`Query`], or `None` when there is nothing typed in it.
    pub fn query(&self) -> Option<Query> {
        self.filter
            .as_ref()
            .map(|input| Query {
                text: input.value().to_string(),
            })
            .filter(|q| !q.text.trim().is_empty())
    }

    /// Whether keystrokes edit the filter rather than running commands.
    pub fn typing(&self) -> bool {
        self.preset == KeysPreset::Search || self.filter_focused
    }

    // ---------------------------------------------------------------- cursor

    /// The row the cursor is on.
    pub fn current(&self) -> Option<&Row> {
        self.rows.rows.get(self.sel)
    }

    /// Moves the cursor, and asks the next frame to bring it into view.
    ///
    /// Every move the user makes goes through here, which is what drops the anchor: the
    /// row they chose, with the session it belongs to, for [`App::keep_cursor`] to find
    /// again after `h`, `l` or `f` reshapes the list. A depth or layout change sets
    /// [`App::sel`] itself instead, so it cannot move the anchor.
    fn select(&mut self, index: usize) {
        let index = self.off_header(index, 1);
        self.sel = index;
        self.follow_sel = true;
        self.anchor = self
            .rows
            .rows
            .get(index)
            .map(|row| (self.row_id_of(row), self.row_session_id(row)));
        self.follow_row();
    }

    /// Returns the preview to the tail when the row under the cursor is not the one the
    /// offset belongs to (it resets to the tail whenever the row under the cursor
    /// changes), even when the new row previews the same pane — a header and its active
    /// pane's row. Every move of the cursor ends here: the user's ([`App::select`]), and the
    /// ones a depth or layout change ([`App::keep_cursor`]) or a refresh
    /// ([`App::restore_cursor`]) make. It compares what the rows are, not where they are,
    /// so a row that only moved keeps its offset.
    fn follow_row(&mut self) {
        let now = self.row_id();
        if now != self.preview_row {
            self.preview_scroll = 0;
            self.preview_row = now;
        }
    }

    /// Moves the cursor by `delta` rows, wrapping.
    pub fn move_by(&mut self, delta: isize) {
        if self.rows.rows.is_empty() {
            return;
        }
        let at = step(self.sel, self.rows.rows.len(), delta);
        // A header is a line, not a stop: the move goes on past it in its own direction.
        self.select(self.off_header(at, delta.signum()));
    }

    /// Whether row `index` is a machine's header, which the cursor never rests on.
    fn is_header(&self, index: usize) -> bool {
        matches!(self.rows.rows.get(index), Some(Row::Machine(_)))
    }

    /// `index`, or the nearest row that is not a header: the first below it, and when the list ends on
    /// headers the last one above them. It does not wrap: the cursor was put here because the list got
    /// shorter, so the row above is the nearest, not the top of the list. Where every row is a header
    /// there is nowhere better: `index` itself.
    fn nearest_row(&self, index: usize) -> usize {
        let len = self.rows.rows.len();
        (index..len)
            .chain((0..index.min(len)).rev())
            .find(|&at| !self.is_header(at))
            .unwrap_or(index)
    }

    /// `index`, or the first row from it that is not a header, going `dir` (backwards when
    /// negative) and wrapping. Where every row is a header there is nowhere better: `index` itself.
    fn off_header(&self, index: usize, dir: isize) -> usize {
        let len = self.rows.rows.len();
        let mut at = index;
        for _ in 0..len {
            if !self.is_header(at) {
                return at;
            }
            at = step(at, len, if dir < 0 { -1 } else { 1 });
        }
        index
    }

    /// Where the cursor belongs now: the best match while filtering, your own row
    /// otherwise.
    pub fn settle_cursor(&mut self) {
        if self.query().is_some() {
            self.cursor_to_best_match();
        } else {
            self.cursor_to_own_row();
        }
    }

    /// Your own row — where you already are, so `Enter` there just closes the popup: the
    /// row of the pane the popup was opened from (`taw ui --from`, in
    /// [`crate::tui::data::Snapshot::own_pane`]) when it is drawn, else your session's
    /// header, else nothing. A flat list draws no headers, so with
    /// your pane not drawn it has no own row at all: every row in it is another pane to
    /// go to, the one the cursor opens on ([`App::home_row`]) included.
    pub fn own_row(&self) -> Option<usize> {
        if let Some(pane) = self.snap.own_pane.as_deref()
            && let Some(at) = self.rows.rows.iter().position(|row| {
                matches!(row, Row::Pane(p) if self.snap.panes.get(*p).is_some_and(|x| x.pane_id == pane))
            })
        {
            return Some(at);
        }
        let session = self.snap.current_session.as_deref()?;
        self.rows.rows.iter().position(|row| {
            matches!(row, Row::Session(_)) && self.row_session_id(row).as_deref() == Some(session)
        })
    }

    /// Where the cursor opens with nothing typed: your own row, else — in a flat list,
    /// which has no header to stand in for a pane it does not draw — your session's first
    /// row, wherever the flat sort put it; else nothing. After a
    /// jump and a reopen the cursor is therefore on the row you jumped to.
    pub fn home_row(&self) -> Option<usize> {
        self.own_row().or_else(|| {
            let session = self.snap.current_session.as_deref()?;
            if !self.flat_now() {
                return None;
            }
            self.rows
                .rows
                .iter()
                .position(|row| self.row_session_id(row).as_deref() == Some(session))
        })
    }

    /// Where the cursor belongs with nothing typed: [`App::home_row`], else the first row.
    fn cursor_to_own_row(&mut self) {
        let at = self.home_row().unwrap_or(0);
        self.select(at.min(self.rows.rows.len().saturating_sub(1)));
    }

    /// Puts the cursor on the best-scoring row; a child beats its header on a tie (a
    /// half point on the score, which only ever breaks ties).
    ///
    /// The filter's directories never compete: their scores only say what to highlight.
    /// The cursor goes where it always went when any session or agent
    /// row matched, and to the first directory when none did, which leaves the tree empty
    /// and nothing else to be on.
    pub fn cursor_to_best_match(&mut self) {
        let mut best: Option<(usize, (i64, u8))> = None;
        for (index, matched) in self.rows.scores.iter().enumerate() {
            let Some((score, _)) = matched else { continue };
            if matches!(self.rows.rows[index], Row::Project(_)) {
                continue;
            }
            let child = u8::from(!matches!(self.rows.rows[index], Row::Session(_)));
            let key = (*score, child);
            if best.is_none_or(|(_, best_key)| key > best_key) {
                best = Some((index, key));
            }
        }
        let at = best
            .map(|(index, _)| index)
            .or_else(|| self.rows.first_project())
            .unwrap_or(0);
        self.select(at);
    }

    /// `Tab`, "next up": the next (or previous) row of the walk, in the order the rows are
    /// drawn, wrapping: what needs you, else every agent. See [`App::next_up_rows`]. With no agent in view at all it walks the session headers
    /// instead.
    pub fn next_attention(&mut self, dir: isize) {
        match self.next_up_row(self.sel, dir) {
            Some(at) => self.select(at),
            None => self.next_session(dir),
        }
    }

    /// Moves the cursor to the next (or previous) session: the next header in a tree, the
    /// next row belonging to a different session in a flat list. A session of a machine that is
    /// away is not a stop.
    pub fn next_session(&mut self, dir: isize) {
        let len = self.rows.rows.len();
        if len == 0 {
            return;
        }
        let here = self
            .current()
            .and_then(|row| tree::space_of(&self.snap, row));
        let mut at = self.sel;
        for _ in 0..len {
            at = step(at, len, dir);
            let row = &self.rows.rows[at];
            let hit = if self.flat_now() {
                // A directory of the filter's belongs to no session, so it is never the
                // start of another one.
                !matches!(row, Row::Project(_)) && tree::space_of(&self.snap, row) != here
            } else {
                matches!(row, Row::Session(_))
            };
            // The walk is over the machines that answer: a session of one that is away is a row
            // that refuses everything, so Tab goes past it.
            let away = tree::space_of(&self.snap, row)
                .and_then(|space| self.snap.spaces.get(space))
                .is_some_and(|space| self.snap.greyed(space.machine));
            if hit && !away {
                self.select(at);
                return;
            }
        }
    }

    // ------------------------------------------------------------- attention

    /// The groups a row stands for in the walk: an agent row, the group its agent is in; a
    /// session header while the agents are folded away (the sessions-only depth, where the
    /// rows are headers whatever the layout says — see [`App::flat_now`]), every group one
    /// of its session's agents the view shows is in, and under a filter only the agents the find
    /// stands for ([`tree::Rows::stands_for`]): all of them when the words the header draws found
    /// it, the hot agent alone when only the title the header draws did; every other row, none. A
    /// row that stands for no group stands for no agent.
    fn row_groups(&self, row: &Row) -> Vec<Group> {
        match row {
            Row::Pane(_) => self
                .row_agent(row)
                .filter(|a| !self.snap.greyed(a.machine))
                .map(|a| vec![Group::of(a.state, a.seen)])
                .unwrap_or_default(),
            Row::Session(space) if self.depth == Depth::Sessions && self.query().is_some() => self
                .rows
                .stands_for
                .get(space)
                .map(|agents| self.groups_of(agents.iter().copied()))
                .unwrap_or_default(),
            Row::Session(space) if self.depth == Depth::Sessions => self.session_groups(*space),
            _ => Vec::new(),
        }
    }

    /// The group of each of a session's agents the view shows, one entry an agent.
    fn session_groups(&self, space: usize) -> Vec<Group> {
        self.groups_of(tree::shown_agents(&self.snap, space).into_iter())
    }

    /// The group of each of the agents at these indexes into [`Snapshot::agents`], but for one
    /// of a machine that is away.
    fn groups_of(&self, agents: impl Iterator<Item = usize>) -> Vec<Group> {
        agents
            .filter_map(|at| self.snap.agents.get(at))
            .filter(|a| !self.snap.greyed(a.machine))
            .map(|a| Group::of(a.state, a.seen))
            .collect()
    }

    /// The rows "next up" walks, in the order drawn: while an agent in view needs you, the
    /// rows that stand for one, and nothing idle or working is a stop; with none that does,
    /// every row that stands for an agent, idle and working alike. A folded header stands for its session's
    /// agents ([`App::row_groups`]). The order is the rows', which states changing under the
    /// sweep never reorder in a tree. Empty when no agent is in view: the rows are what a
    /// filter or a view left, so what they hide is not there to be walked to.
    pub fn next_up_rows(&self) -> Vec<usize> {
        let groups: Vec<Vec<Group>> = self
            .rows
            .rows
            .iter()
            .map(|row| self.row_groups(row))
            .collect();
        let needs_you = groups
            .iter()
            .flatten()
            .any(|&group| group == Group::NeedsYou);
        groups
            .iter()
            .enumerate()
            .filter(|(_, here)| {
                if needs_you {
                    here.contains(&Group::NeedsYou)
                } else {
                    !here.is_empty()
                }
            })
            .map(|(at, _)| at)
            .collect()
    }

    /// The stops of the walk across machines, in the app's names (this Mac's pane ids bare, the others
    /// `<machine>:<id>`), in walk order: what needs you while anything does, else every agent, the machine
    /// that answers only. It reads the snapshot, so it does not move with the filter, the folds or the depth
    /// ([`walk::stops`]).
    pub fn walk_stops(&self) -> Vec<String> {
        walk::stops(&self.snap)
    }

    /// A session's first agent that needs you, in tmux order (window, pane), as an index
    /// into [`Snapshot::agents`] — the title a folded header carries.
    pub fn session_hot_agent(&self, space: usize) -> Option<usize> {
        tree::hot_agent(&self.snap, space)
    }

    /// The next row of the walk after `from` in the direction `dir`, wrapping; `None` when
    /// no agent is in view. `from` itself is the answer only when it is the walk's only row.
    pub fn next_up_row(&self, from: usize, dir: isize) -> Option<usize> {
        step_wrapping(&self.next_up_rows(), from, dir)
    }

    /// The agents that need you ([`attention::hot`]), counted by the state that needs it,
    /// most urgent first: what the tree border counts, each group in its state's
    /// own glyph and colour ([`attention::hot_by_state`]).
    pub fn hot_by_state(&self) -> Vec<(AgentState, usize)> {
        attention::hot_by_state(
            self.snap
                .agents
                .iter()
                .filter(|a| !self.snap.greyed(a.machine))
                .map(|a| (a.state, a.seen)),
        )
    }

    /// How many agents need you, whatever the state that needs it.
    pub fn hot_count(&self) -> usize {
        self.hot_by_state().iter().map(|&(_, count)| count).sum()
    }

    /// The bar's numbers: the machines listed, sessions on the server and agents, and the away machines that no header of
    /// the drawn list names.
    pub fn bar_counts(&self) -> crate::cli::status_bar::BarCounts {
        crate::cli::status_bar::BarCounts {
            // This machine and the config's others, connected or not: a table of this
            // server alone is one machine.
            machines: 1 + self.snap.machines.iter().filter(|m| !m.this_server).count(),
            sessions: self
                .snap
                .spaces
                .iter()
                .filter(|s| !self.snap.greyed(s.machine))
                .count(),
            agents: self
                .snap
                .agents
                .iter()
                .filter(|a| !self.snap.greyed(a.machine))
                .count(),
            away: self.away_unheaded(),
        }
    }

    /// The names of the away machines that no header of the drawn list names
    /// ([`machines::away_unheaded`]).
    pub fn away_unheaded(&self) -> Vec<String> {
        crate::tui::machines::away_unheaded(&self.rows, &self.snap)
    }

    // ------------------------------------------------------------------ rows

    /// The tmux session id a row belongs to.
    fn row_session_id(&self, row: &Row) -> Option<String> {
        let space = tree::space_of(&self.snap, row)?;
        self.snap.spaces.get(space).map(|s| s.session_id.clone())
    }

    /// The pane a row is, when it is one.
    fn row_pane(&self, row: &Row) -> Option<&PaneRow> {
        match row {
            Row::Pane(pane) => self.snap.panes.get(*pane),
            _ => None,
        }
    }

    /// The agent occupying a row's pane, when one does.
    fn row_agent(&self, row: &Row) -> Option<&AgentRow> {
        let agent = self.row_pane(row)?.agent?;
        self.snap.agents.get(agent)
    }

    fn row_id(&self) -> Option<RowId> {
        self.current().map(|row| self.row_id_of(row))
    }

    /// Where a row is now, by identity rather than by position.
    fn position_of(&self, id: &RowId) -> Option<usize> {
        self.rows
            .rows
            .iter()
            .position(|row| self.row_id_of(row) == *id)
    }

    /// What a row is, independent of its position — see [`RowId`]. A row indexing
    /// something the snapshot no longer has gets an empty id, which matches nothing.
    fn row_id_of(&self, row: &Row) -> RowId {
        match row {
            Row::Pane(pane) => RowId::Pane(
                self.snap
                    .panes
                    .get(*pane)
                    .map(|p| p.pane_id.clone())
                    .unwrap_or_default(),
            ),
            Row::Session(space) => RowId::Session(
                self.snap
                    .spaces
                    .get(*space)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default(),
            ),
            Row::ClosedWorktree { record, .. } => RowId::Worktree(
                self.worktrees
                    .get(*record)
                    .map(|r| r.workspace_id.clone())
                    .unwrap_or_default(),
            ),
            Row::Project(at) => RowId::Project(
                self.projects
                    .get(*at)
                    .map(|project| project.path.clone())
                    .unwrap_or_default(),
            ),
            Row::Machine(machine) => RowId::Machine(machine.map(|at| self.machine_name(at))),
            Row::NoSession(machine) => RowId::NoSession(self.machine_name(*machine)),
        }
    }

    /// The name of the machine at place `at` in the snapshot's list; empty when it has none, which matches no
    /// machine.
    fn machine_name(&self, at: usize) -> String {
        self.snap
            .machines
            .get(at)
            .map(|machine| machine.name.clone())
            .unwrap_or_default()
    }

    /// The cursor row's session, when the snapshot still has it.
    pub fn selected_space(&self) -> Option<&SpaceRow> {
        let space = tree::space_of(&self.snap, self.current()?)?;
        self.snap.spaces.get(space)
    }

    /// The pane the preview shows: the cursor row's own pane, or the cursor session's
    /// active pane. A closed worktree has no pane to show, and nor has a row of another machine
    /// until its panes are captured over its connection: its ids are not this machine's tmux's.
    pub fn preview_pane(&self) -> Option<String> {
        match self.current()? {
            Row::Pane(pane) => self
                .snap
                .panes
                .get(*pane)
                .filter(|p| p.machine.is_none())
                .map(|p| p.pane_id.clone()),
            Row::Session(space) => {
                let space = self.snap.spaces.get(*space)?;
                space
                    .machine
                    .is_none()
                    .then(|| space.active_pane_id.clone())
                    .flatten()
            }
            Row::ClosedWorktree { .. } | Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => {
                None
            }
        }
    }

    /// The pane of another machine the preview is to show: the cursor row's own pane, or the cursor session's
    /// active pane, when that row is a machine's. Its id is the list's (`studio:%16`); `None` for every
    /// row [`App::preview_pane`] answers for, and for a row with no pane.
    pub fn preview_remote_pane(&self) -> Option<RemotePane> {
        let (pane, machine) = match self.current()? {
            Row::Pane(pane) => {
                let pane = self.snap.panes.get(*pane)?;
                (pane.pane_id.clone(), pane.machine?)
            }
            Row::Session(space) => {
                let space = self.snap.spaces.get(*space)?;
                (space.active_pane_id.clone()?, space.machine?)
            }
            Row::ClosedWorktree { .. } | Row::Project(_) | Row::Machine(_) | Row::NoSession(_) => {
                return None;
            }
        };
        let header = self.snap.machines.get(machine)?;
        Some(RemotePane {
            pane,
            machine: header.name.clone(),
            live: header.live,
        })
    }

    /// The pane the preview text on screen was captured from, which is the cursor row's
    /// ([`App::preview_pane`]) once the run loop has followed the cursor.
    pub fn preview_shows(&self) -> Option<&str> {
        self.preview_for.as_deref()
    }

    /// Whether the preview's lines may wrap, decided by the pane the text on screen was
    /// captured from ([`App::set_preview`]), which is the cursor row's pane. A pane in the alternate
    /// screen (`#{alternate_on}`: nvim, lazygit, less, htop) is drawn cut, as its layout is
    /// the screen itself; anything else — an agent's prose, a shell — wraps to the panel.
    pub fn preview_wraps(&self) -> bool {
        let Some(pane) = self.preview_for.as_deref() else {
            return false;
        };
        self.snap
            .panes
            .iter()
            .find(|p| p.pane_id == pane)
            .is_some_and(|p| !p.alternate_on)
    }

    /// Shows `text`, a fresh capture of `pane`. The offset survives a recapture of the same pane —
    /// you are reading history while the agent keeps printing — and resets to the tail
    /// when the pane changes, or there is none. A move of the cursor to another row has
    /// already reset it, at the move ([`App::follow_row`]).
    pub fn set_preview(&mut self, pane: Option<String>, text: Option<Text<'static>>) {
        if pane != self.preview_for {
            self.preview_scroll = 0;
        }
        self.preview_for = pane;
        self.preview = text;
        self.preview_age = None;
    }

    // ---------------------------------------------------------------- labels

    /// Which label (1–9) a row carries: in the tree, a session header's place in tmux
    /// order; in a flat list, the row's place among the first nine rows that are not
    /// parked worktrees, as the list is with nothing typed: a filter hides rows and never
    /// renumbers the rest ([`App::labels`]). `None` for every other row, and when `[ui] labels`
    /// is off.
    pub fn label_index(&self, index: usize) -> Option<u8> {
        if !self.cfg.ui.labels {
            return None;
        }
        self.labels.get(self.rows.rows.get(index)?)?.number
    }

    /// The address the query matched in front of row `index`'s own text ([`tree::match_text`]):
    /// its label as drawn, `4` or `4a`, and empty for a row with none or with `[ui] labels` off.
    pub fn row_address(&self, index: usize) -> &str {
        match self.rows.rows.get(index) {
            Some(row) if self.cfg.ui.labels => self.labels.address(row),
            _ => "",
        }
    }

    /// The label drawn on a row: the key that selects it ([`Keymap::label`]).
    pub fn row_label(&self, index: usize) -> Option<String> {
        self.label_index(index).map(|n| self.keymap.label(n))
    }

    /// Whether pressing number `n`'s key right now does what the number says, which is when
    /// the number is drawn in the yellow of a key (yellow: a key that works now):
    /// `[ui] labels` on, a key bound to it, and that key, the one its
    /// label names ([`Keymap::short_event`]), not one the modal preset's open filter takes as
    /// text. The modal preset binds the digits, so its numbers work until `/` is pressed and
    /// again after Esc; the search preset binds none, a digit being text for its query, but it
    /// asks its keymap before its query, so a `[keys] select_session` entry's key works there.
    ///
    /// While a filter is being typed into (the modal preset's open one, the search preset's), a
    /// number works whatever its key: typing it puts the cursor on its row, as a query that is
    /// exactly a drawn label does: what is drawn can be typed.
    pub fn number_works(&self, n: u8) -> bool {
        self.cfg.ui.labels && (self.typing() || self.number_key_works(n))
    }

    /// Whether number `n`'s key, the one its label names ([`Keymap::short_event`]), acts as a
    /// key right now: bound, and not one the modal preset's open filter takes as text.
    fn number_key_works(&self, n: u8) -> bool {
        self.keymap
            .short_event(KeyAction::SelectSession(n))
            .is_some_and(|key| {
                !(self.preset == KeysPreset::Modal && self.filter_focused && is_text(key))
            })
    }

    /// Whether any number works right now ([`App::number_works`]): what the hint line's
    /// number entry and the help's legend go by.
    pub fn numbers_work(&self) -> bool {
        (1..=9).any(|n| self.number_works(n))
    }

    /// Whether the tree's own keys act right now: no overlay is open. An open overlay (the help, a
    /// prompt, a question, the picker, a menu) takes every key itself, so the tree's numbers and
    /// its lit letters are dim under it; the help's legend still
    /// draws its number as the tree looks once the help closes ([`App::numbers_work`]).
    pub fn tree_takes_keys(&self) -> bool {
        !self.overlay.is_open()
    }

    /// Whether row `index`'s label works right now: its number does ([`App::number_works`]).
    pub fn label_works(&self, index: usize) -> bool {
        self.label_index(index)
            .is_some_and(|n| self.number_works(n))
    }

    /// A number key: the cursor moves to the row labelled `n` — in the tree, that session's
    /// header, whose letters then show as any session's do while the cursor is in it
    /// ([`App::lettered_rows`]). Nothing else happens; a label no drawn row carries (fewer
    /// sessions, or the session is filtered out) says so.
    ///
    /// With `[ui] labels` off there are no numbers on screen, so the keys do nothing at
    /// all — not even complain about a session the reader can see but cannot name.
    pub fn select_session(&mut self, n: u8) {
        if !self.cfg.ui.labels {
            return;
        }
        match (0..self.rows.rows.len()).find(|&i| self.label_index(i) == Some(n)) {
            Some(at) => {
                // A machine that dropped keeps its numbers, but there is nothing to go to.
                match self.machine_at(at).filter(|header| !header.live) {
                    Some(header) => {
                        let text = self.refusal(header);
                        self.toast(text);
                    }
                    None => self.select(at),
                }
            }
            None => self.toast(format!("no session numbered {n} is shown")),
        }
    }

    /// Whether the letters show at all: in a tree, below the sessions depth, with `[ui] labels`
    /// on, in either preset, typed into or not. A flat list's numbers already name its rows, and the
    /// sessions depth draws nothing under a header.
    fn letters_shown(&self) -> bool {
        self.cfg.ui.labels && !self.flat_now() && self.depth != Depth::Sessions
    }

    /// Whether a letter's key jumps to its row: while the letters show and nothing is being
    /// typed. While a filter is being typed into a letter is a letter of the query, and the
    /// address it completes (`4a`) is what puts the cursor on the row.
    fn letters_are_keys(&self) -> bool {
        self.letters_shown() && !self.typing()
    }

    /// Every row that wears a letter, with its letter: the first five child rows of each
    /// session (a pane or a parked worktree is its session's), in the order drawn with nothing
    /// typed and whatever kind they are, `a` to `e`; a row a filter hides takes its letter with
    /// it ([`App::labels`]). Empty when the letters do not show (`letters_shown`). A letter a
    /// `[keys]` entry gives to an action keeps that action, and the row it would have named goes
    /// without.
    ///
    /// Every session wears them, not only the cursor's, so the letter to press is known
    /// before pressing the number, without going to the session to find it out; only the
    /// cursor's session's are keys ([`App::lettered_rows`]), and the view lights those alone.
    pub fn row_letters(&self) -> Vec<(usize, char)> {
        if !self.letters_shown() {
            return Vec::new();
        }
        self.rows
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| Some((index, self.labels.get(row)?.letter?)))
            .collect()
    }

    /// The rows the letters name as keys, each with its letter: [`App::row_letters`] for the
    /// session the cursor is in (a header is its own session's). Empty when the letters do not
    /// show, while a filter is being typed into (a letter is text there), or the session has no
    /// rows.
    ///
    /// Worked out from the cursor every time it is asked, so however the cursor moves — a
    /// key, a click, a refresh that keeps it or loses its row — the letters that jump are
    /// where it is: no number needed, nothing to put away.
    pub fn lettered_rows(&self) -> Vec<(usize, char)> {
        if !self.letters_are_keys() {
            return Vec::new();
        }
        self.cursor_session_letters()
    }

    /// The rows whose letters are drawn lit, yellow, with their letters: those a letter's key
    /// would jump to ([`App::lettered_rows`]), and while a filter is being typed into, the
    /// cursor's session's while the query is exactly that session's number, since the next
    /// letter typed completes the address (typing `4` lights session 4's letters, as pressing
    /// `4` does outside the filter). Every other letter is drawn unlit.
    pub fn lit_letters(&self) -> Vec<(usize, char)> {
        if !self.typing() {
            return self.lettered_rows();
        }
        let named = self
            .current()
            .and_then(|row| tree::space_of(&self.snap, row))
            .and_then(|space| self.labels.get(&Row::Session(space))?.number)
            .zip(self.query())
            .is_some_and(|(n, query)| {
                query.text.trim().to_lowercase() == self.keymap.label(n).to_lowercase()
            });
        if named {
            self.cursor_session_letters()
        } else {
            Vec::new()
        }
    }

    /// [`App::row_letters`] for the session the cursor is in.
    fn cursor_session_letters(&self) -> Vec<(usize, char)> {
        let Some(space) = self
            .current()
            .and_then(|row| tree::space_of(&self.snap, row))
        else {
            return Vec::new();
        };
        self.row_letters()
            .into_iter()
            .filter(|&(index, _)| {
                self.rows
                    .rows
                    .get(index)
                    .and_then(|row| tree::space_of(&self.snap, row))
                    == Some(space)
            })
            .collect()
    }

    // ---------------------------------------------------------------- chrome

    /// The chip the tree border shows while a projection is installed: the query's own
    /// label, or herdr's word for an unlabelled one. `None` when nothing is installed.
    pub fn view_label(&self) -> Option<&str> {
        self.snap.view.as_ref().map(AgentView::label)
    }

    /// Says once, on the hint line, that an installed projection was ignored.
    ///
    /// Once, not per tick: the snapshot is rebuilt twice a second, and a popup that
    /// re-raised the same complaint 120 times a minute would never show anything else.
    fn report_view_error(&mut self) {
        if self.snap.view_error == self.reported_view_error {
            return;
        }
        self.reported_view_error = self.snap.view_error.clone();
        if let Some(error) = self.snap.view_error.clone() {
            self.toast(format!("{} ignored: {error}", agent_view::OPTION));
        }
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    /// Puts a question an action raised to the user, in place of whatever overlay was open
    /// (none: the answer that ran the action closed it).
    pub fn ask(&mut self, question: Confirm) {
        self.overlay = Overlay::Confirm(question);
    }

    /// The run loop's word that `action` has finished running, at `now`. After a close, Enter
    /// rests for [`SETTLE`]; the answer says whether it was one, so that the loop can also drop
    /// the keys typed while it ran.
    pub fn after_action(&mut self, action: &Action, now: Instant) -> bool {
        let close = matches!(
            action,
            Action::KillPane { .. }
                | Action::KillWorkspace { .. }
                | Action::KillGroup { .. }
                | Action::RemoveWorktree { .. }
        );
        if close {
            self.settle_until = Some(now + SETTLE);
        }
        close
    }

    /// Whether a close finished less than [`SETTLE`] ago.
    fn settling(&self) -> bool {
        self.settle_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// The toast still worth showing, if any.
    pub fn live_toast(&self) -> Option<&str> {
        self.toast
            .as_ref()
            .filter(|(_, at)| at.elapsed() < TOAST_TTL)
            .map(|(text, _)| text.as_str())
    }

    /// What `Enter` does, for the hint line: `go` in the popup (the row under the cursor, your own row
    /// included, where it just closes) and `go in` in the app (it attaches to the row). Kept as a method so
    /// the hint builder has one place to ask.
    ///
    /// In the popup, while the cursor is on a row of a connected machine, Enter hands the terminal to the app for
    /// the trip: the words say where it goes and which key there brings you back ([`words::enter_goes_to`], with
    /// [`crate::tui::keys::leave_key`]: `Q`, or `Ctrl-C` where Q does not end the app). A machine
    /// that is away answers Enter with a toast and goes nowhere, so its rows keep `go`.
    pub fn enter_meaning(&self) -> String {
        self.enter_words(TripWords::Full)
    }

    /// [`App::enter_meaning`] at `trip`'s length: the hint line says less of the trip when all of it does not fit.
    fn enter_words(&self, trip: TripWords) -> String {
        if self.is_app() {
            return "go in".to_string();
        }
        match self.machine_at(self.sel).filter(|header| header.live) {
            Some(header) => match trip {
                TripWords::Full => crate::machine::words::enter_goes_to(
                    &header.name,
                    crate::tui::keys::leave_key(self.preset, &self.keymap),
                ),
                TripWords::Name => crate::machine::words::enter_goes_to_short(&header.name),
                TripWords::Plain => "go".to_string(),
            },
            None => "go".to_string(),
        }
    }

    /// The hint line, as `(key, meaning)` pairs: `1-9 session · a-e rows · Tab next up · Enter
    /// go · j/k move · / filter · ? help`, naming whatever
    /// keys are actually bound — a `[keys]` override shows its own key, not the preset's.
    /// The numbers' entry is there exactly while they are keys ([`App::numbers_work`]). While the
    /// modal filter has focus the line is the filter's own: `4a
    /// session 4 row a · Esc clear · Tab next up · ↑/↓ move · Enter go`, naming no key the query
    /// takes as text, and saying first what typing a label does ([`App::typing_hints`]).
    /// The letters' entry is there exactly while they show (`letters_shown`), right after the
    /// numbers and never in their place, and it reads the same whatever session the cursor is
    /// in, however many rows that session has, so that it reads the same everywhere: `a-e rows`.
    /// A `[keys]` entry can
    /// take a letter ([`Keymap::free_letters`]), from the tail of the run (`a-d`) or from its
    /// *middle*: `a-e` would then claim the missing one jumps too, so a gap lists the letters
    /// instead (`a b d e rows`), as the help's line for them does ([`Keymap::letters_hint`]
    /// spells both). The meaning is `rows`, not `its rows` or `session's rows`, so the line
    /// is whole wherever it is (`session's rows` wrapped at 96 to 101 columns and cost the
    /// tree a row).
    /// `Enter`'s entry is `go` in the popup and `go in` in the app ([`App::enter_meaning`]). The popup's
    /// line has no `q quit`: it is in `?`, and the line is worth more to a reader who does not yet know how
    /// to leave. The app's line does say its ways out, right after `Enter` (`Q quit` unless `[keys]` gave
    /// Q away, and the quit key as `Esc back`, `Esc clear` while the modal filter holds text), so a narrow
    /// terminal keeps them ([`App::back_hint`]).
    ///
    /// The search preset's line is its whole keyboard: `type filter (fzf
    /// syntax) · 4a session 4 row a · ↑/↓ ctrl-n/p move · Enter go · right-click menu · Esc
    /// quit`. The move
    /// entry is written out for the preset's own keys, because three pairs of keys share
    /// those two actions and the shortest of each would name only the arrows; once a
    /// `[keys]` entry rebinds moving it names the keys bound ([`App::move_hint`]), since the
    /// written-out pairs may move nothing then. In the app the search line says `Enter go in`, then
    /// `Esc back` and `Ctrl-C quit`, then `right-click menu`.
    pub fn hint_line(&self) -> Vec<(String, String)> {
        self.hint_line_with(TripWords::Full)
    }

    /// [`App::hint_line`] with Enter's words about a trip to another machine at `trip`'s length.
    pub(crate) fn hint_line_with(&self, trip: TripWords) -> Vec<(String, String)> {
        let mut hints: Vec<(String, String)> = Vec::new();
        let mut push = |key: Option<String>, meaning: String| {
            if let Some(key) = key {
                hints.push((key, meaning));
            }
        };
        if self.preset == KeysPreset::Search {
            push(Some("type".to_string()), "filter (fzf syntax)".to_string());
            // Its digits are text for the query: it names number keys while a `[keys]
            // select_session` entry binds keys that act, its keymap being asked first, and then
            // what typing a label does.
            for (key, meaning) in self.typing_hints() {
                push(Some(key), meaning);
            }
            let preset = crate::tui::keys::preset(KeysPreset::Search);
            let own_keys = [KeyAction::MoveDown, KeyAction::MoveUp]
                .into_iter()
                .all(|action| self.keymap.keys_for(action) == preset.keys_for(action));
            push(
                if own_keys {
                    Some("\u{2191}/\u{2193} ctrl-n/p".to_string())
                } else {
                    self.move_hint()
                },
                "move".to_string(),
            );
            push(self.hint_key(KeyAction::Go), self.enter_words(trip));
            // In the app the ways out come before the lesser right-click hint, so a narrow terminal that
            // keeps two lines keeps them.
            if self.is_app() {
                push(self.hint_key(KeyAction::Quit), "back".to_string());
                push(Some("Ctrl-C".to_string()), self.quit_word().to_string());
                push(Some("right-click".to_string()), "menu".to_string());
            } else {
                push(Some("right-click".to_string()), "menu".to_string());
                push(self.hint_key(KeyAction::Quit), "quit".to_string());
            }
            return hints;
        }
        // The modal preset's open filter takes every plain character as text, so its line names
        // only the keys that act there:
        // Esc clears and Enter goes whatever `[keys]` says, next up by a key of its that is not
        // text, the filter's own keys to move, and the numbers while a key of theirs works.
        if self.filter_focused {
            for (key, meaning) in self.typing_hints() {
                push(Some(key), meaning);
            }
            push(Some("Esc".to_string()), "clear".to_string());
            push(
                self.keymap
                    .short_key_where(KeyAction::NextAttention, |key| !is_text(key)),
                "next up".to_string(),
            );
            push(Some(self.filter_move_hint()), "move".to_string());
            push(Some("Enter".to_string()), self.enter_words(trip));
            return hints;
        }
        // The numbers' entry is there exactly while they are keys, as the letters' is while
        // they show: not while `/` is open, where a digit is text.
        if self.numbers_work() {
            push(self.keymap.select_session_hint(), "session".to_string());
        }
        if self.letters_shown() {
            push(self.keymap.letters_hint(), "rows".to_string());
        }
        push(
            self.hint_key(KeyAction::NextAttention),
            "next up".to_string(),
        );
        push(self.hint_key(KeyAction::Go), self.enter_words(trip));
        // In the app the line also says the two ways out, by what is bound: Q ends it (unless `[keys]`
        // gave Q to something else), and the quit key goes back where you were. They come right after
        // the way in, before the lesser hints, so a narrow terminal that keeps two lines keeps them.
        if self.is_app() {
            if self.capital_q_quits() {
                push(Some("Q".to_string()), self.quit_word().to_string());
            }
            if let Some((key, meaning)) = self.back_hint() {
                push(Some(key), meaning.to_string());
            }
        }
        push(self.move_hint(), "move".to_string());
        push(self.hint_key(KeyAction::Filter), "filter".to_string());
        push(self.hint_key(KeyAction::Help), "help".to_string());
        hints
    }

    /// What the keys that end the app are said to do: quit, or go back to tmux ([`BACK_TO_TMUX`]).
    fn quit_word(&self) -> &'static str {
        if self.back_to_tmux {
            BACK_TO_TMUX
        } else {
            "quit"
        }
    }

    /// Whether a capital Q ends the app: it does where `[keys]` gave Q to no action ([`App::on_key`]).
    pub(crate) fn capital_q_quits(&self) -> bool {
        self.is_app()
            && self
                .keymap
                .lookup(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT))
                .is_none()
    }

    /// How the app's way back is named, with what it does now: the quit key, `Esc` when Esc is among the
    /// quit keys, and `clear` instead of `back` while Esc would clear the modal
    /// filter's text before it goes back ([`App::run_key`]). `None` when the quit action has no key.
    pub(crate) fn back_hint(&self) -> Option<(String, &'static str)> {
        let esc = self
            .keymap
            .keys_for(KeyAction::Quit)
            .iter()
            .any(|key| key == "Esc");
        if esc {
            let clears = self.preset == KeysPreset::Modal && self.filter.is_some();
            return Some(("Esc".to_string(), if clears { "clear" } else { "back" }));
        }
        self.hint_key(KeyAction::Quit).map(|key| (key, "back"))
    }

    /// What the hint line says about the labels while a filter is being typed into: the number
    /// keys while a key of theirs acts there (a `[keys] select_session` entry the filter cannot
    /// type), then what typing a label does: where the letters are drawn,
    /// `4a session 4 row a`, a session's number followed by a row's letter, as typed in the
    /// filter or pressed outside it, left out while a session's label is no character a query
    /// can hold (`[keys] select_session` on ctrl+1 to ctrl+9: `ctrl-4a` cannot be typed,
    /// [`Keymap::typed_label`]); where only numbers are (a flat list, the sessions depth),
    /// `1-9 session`. Nothing with `[ui] labels` off.
    fn typing_hints(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if !self.cfg.ui.labels {
            return out;
        }
        let numbers = |key: String| (key, "session".to_string());
        if (1..=9).any(|n| self.number_key_works(n))
            && let Some(key) = self.keymap.select_session_hint()
        {
            out.push(numbers(key));
        }
        let typed = match self.keymap.free_letters().first() {
            Some(&letter) if self.letters_shown() => self.keymap.typed_label(4).map(|label| {
                (
                    format!("{label}{letter}"),
                    format!("session 4 row {letter}"),
                )
            }),
            _ => Some(numbers(
                self.keymap
                    .select_session_hint()
                    .unwrap_or_else(|| "1-9".to_string()),
            )),
        };
        if let Some(typed) = typed
            && !out.contains(&typed)
        {
            out.push(typed);
        }
        out
    }

    /// The filter's keys to move, for its hint line: the arrows, as the filter's hint writes them, while
    /// they are bound to moving (the presets' own); fzf's ctrl-p and ctrl-n otherwise, which move
    /// in the filter whatever `[keys]` says.
    fn filter_move_hint(&self) -> String {
        let arrow = |code, action| {
            self.keymap.lookup(KeyEvent::new(code, KeyModifiers::NONE)) == Some(action)
        };
        if arrow(KeyCode::Up, KeyAction::MoveUp) && arrow(KeyCode::Down, KeyAction::MoveDown) {
            "\u{2191}/\u{2193}".to_string()
        } else {
            "ctrl-p/n".to_string()
        }
    }

    /// The key to name `action` by on the hint line — [`Keymap::short_key`], which the
    /// help overlay names it by too.
    fn hint_key(&self, action: KeyAction) -> Option<String> {
        self.keymap.short_key(action)
    }

    /// The hint line's keys for moving: down, then up, each by the key the help's key column
    /// names it by (`j/k`), or the one of the two that has a key.
    fn move_hint(&self) -> Option<String> {
        match (
            self.hint_key(KeyAction::MoveDown),
            self.hint_key(KeyAction::MoveUp),
        ) {
            (Some(down), Some(up)) => Some(format!("{down}/{up}")),
            (one, None) | (None, one) => one,
        }
    }

    // ------------------------------------------------------------------ keys

    /// Handles one key press, returning an action for the run loop to execute.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            self.end = Some(AppEnd::Leave);
            return None;
        }
        // An overlay owns the keyboard while it is open, prefix chord included: its keys
        // are looked up before the modal or search preset sees any. Its text fields
        // still leave a tmux prefix its key: the filter's chord is checked before the filter
        // sees anything, and a prompt does the same for the keys herdr's fields added.
        if self.overlay.is_open() {
            // The `y` behind the one that confirmed the removal this question came of is a
            // double tap, not an answer: nothing that loses work is said yes to in the moment
            // after the action that asked. Every other key answers at once.
            if self.settling()
                && matches!(key.code, KeyCode::Char('y' | 'Y'))
                && matches!(&self.overlay, Overlay::Confirm(c) if c.answers == Answers::Strict)
            {
                return None;
            }
            if self.overlay.takes_text()
                && input::is_extra(key)
                && self
                    .chord
                    .as_ref()
                    .is_some_and(|chord| chord.is_prefix(key))
            {
                return None;
            }
            let response = self.overlay.on_key(key);
            let action = self.apply(response);
            // The project picker cannot reach the filesystem itself, so whatever it just
            // consumed leaves its candidates stale until this runs.
            self.refresh_project_picker();
            return action;
        }
        if let Some(chord) = self.chord.as_mut() {
            match chord.feed(key, Instant::now()) {
                ChordOutcome::Started | ChordOutcome::Ignored => return None,
                // The walk starts from the pane the popup was opened from — the same
                // pane the cursor opened on. `exec` cannot read it for itself: a
                // `display-popup -E` command has no `$TMUX_PANE`. In the app, which has no client
                // to ask, it starts from the pane you came out of when there is one.
                ChordOutcome::Notification => {
                    return Some(Action::NextUp {
                        own: self
                            .came_from()
                            .map(str::to_string)
                            .or_else(|| self.snap.own_pane.clone()),
                    });
                }
                ChordOutcome::NotAChord => {}
            }
        }
        // The letters: while they show, `a`–`e` names a row of the session the cursor is in,
        // and the key moves the cursor there and does nothing else. The hint names all five in
        // every session, so a letter that session has no row for says so
        // rather than going on to the keymap. One a `[keys]` entry gave to an action is that
        // action's key. After the tmux prefix the chord above has already taken the key.
        if key.modifiers.is_empty()
            && let KeyCode::Char(c) = key.code
            && self.letters_are_keys()
            && self.keymap.free_letters().contains(&c)
        {
            match self
                .lettered_rows()
                .iter()
                .find(|&&(_, letter)| letter == c)
            {
                Some(&(row, _)) => self.select(row),
                None => self.toast(format!("no row {c} in this session")),
            }
            return None;
        }
        // In the modal preset a focused filter takes the keyboard whole: a plain letter is
        // text, never a command. The search preset's bindings are all ctrl-chords and
        // named keys, so there the keymap can go first and the rest is text.
        if self.preset == KeysPreset::Modal && self.filter_focused {
            return self.filter_key(key);
        }
        match self.keymap.lookup(key) {
            Some(action) => self.run_key(action, key),
            None if self.is_end_of_input(key) => self.run_key(KeyAction::Quit, key),
            // `taw app` ends at once on a capital Q (the popup has no such key). Only when
            // nothing is being typed (a focused filter took the key above, and the search
            // preset's query takes every letter), and only when `[keys]` gave Q to no action.
            None if self.is_app() && !self.typing() && is_capital_q(key) => {
                self.quit = true;
                self.end = Some(AppEnd::Leave);
                None
            }
            None => {
                self.edit_filter(key);
                None
            }
        }
    }

    /// Whether `key` is fzf's end of input: ctrl-d on the search preset's empty query. The search
    /// preset is a pure fzf search, and fzf's ctrl-d is `delete-char/eof`: on a query with text
    /// it deletes the character under the cursor, which the input does (and at the end of the
    /// text it finds none and leaves the popup open), and on an empty one it quits, as Esc does.
    /// It is the query line's alone: the modal preset's filter, a prompt and the picker never
    /// close on it, and a `[keys]` entry bound to ctrl-d has answered before this is asked.
    fn is_end_of_input(&self, key: KeyEvent) -> bool {
        self.preset == KeysPreset::Search
            && matches!(key.code, KeyCode::Char('d' | 'D'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && self.filter.as_ref().is_some_and(|f| f.value().is_empty())
    }

    /// The keys a focused filter claims in the modal preset: the commands first, then
    /// whatever is left is text for the query.
    ///
    /// Three layers, in order.
    ///
    /// `Esc` and `Enter` are fixed whatever `[keys]` says — they are how a filter is left,
    /// and a rebinding that turned either into text would trap the user inside it.
    /// `ctrl-j` / `ctrl-k` and `ctrl-n` / `ctrl-p` come next — down and up, fzf's own keys —
    /// intrinsic to the query line rather than bound (arrows and ctrl-j/k
    /// move, ctrl-n/p beside them); the modal preset spends no binding
    /// on them, and `ctrl-k` therefore moves the cursor here rather than deleting to the
    /// end of the line as it does in a prompt.
    ///
    /// Everything else a text input would never claim ([`is_text`]) means exactly what the
    /// keymap says it means, and only the movement half of that survives: the filter runs
    /// no command that would take the keyboard away from it. Nothing is hardcoded past
    /// this point — `Tab` goes next up because `next_attention` is bound to it, so
    /// rebinding that action away from `Tab` really does stop `Tab` going there, and a
    /// `Tab` the keymap has given to some other action does nothing here rather than
    /// falling back to the action `Tab` has by default. A key the keymap does not know at all is left
    /// to the input, which is how Backspace, the horizontal arrows and the readline chords
    /// still edit.
    ///
    /// A plain letter is never a command: `j` is `move_down` on the tree and the first
    /// letter of "json" in a query.
    fn filter_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Esc drops the query; the tree comes back as it was.
            KeyCode::Esc => {
                self.clear_filter();
                return None;
            }
            // Enter acts on the row the query found, from inside the filter.
            KeyCode::Enter => return self.row_action(RowAction::Go),
            KeyCode::Char('j' | 'n') if ctrl => {
                self.move_by(1);
                return None;
            }
            KeyCode::Char('k' | 'p') if ctrl => {
                self.move_by(-1);
                return None;
            }
            _ => {}
        }
        if !is_text(key)
            && let Some(action) = self.keymap.lookup(key)
        {
            self.move_cursor(action);
            return None;
        }
        self.edit_filter(key);
        None
    }

    /// Moves the cursor for a movement action. Anything else is a command the filter does
    /// not run: movement is all it shares with the tree.
    fn move_cursor(&mut self, action: KeyAction) {
        match action {
            KeyAction::NextAttention => self.next_attention(1),
            KeyAction::PreviousAttention => self.next_attention(-1),
            KeyAction::MoveDown => self.move_by(1),
            KeyAction::MoveUp => self.move_by(-1),
            KeyAction::NextSession => self.next_session(1),
            KeyAction::PreviousSession => self.next_session(-1),
            // A number key is movement too. The preset's own digits are text here and
            // never reach this, but a `[keys] select_session` rebound to something the
            // filter cannot type does, and it must move the cursor the way it does on the
            // tree rather than being swallowed.
            KeyAction::SelectSession(n) => self.select_session(n),
            _ => {}
        }
    }

    /// Feeds `key` to the filter input when one is focused, rebuilding the rows and
    /// settling the cursor when it changed the query. Whether the key was consumed.
    fn edit_filter(&mut self, key: KeyEvent) -> bool {
        if !self.typing() {
            return false;
        }
        let Some(input) = self.filter.as_mut() else {
            return false;
        };
        let before = input.value().to_string();
        if !input.handle(field_spelling(self.preset, key)) {
            return false;
        }
        // Only a change of text lists the directories again: a cursor key would read the
        // filesystem for the answer it already has.
        if input.value() != before {
            self.refresh_projects();
        }
        self.rebuild();
        self.settle_cursor();
        true
    }

    /// `/`: open the filter, or put the keyboard back into one already open.
    fn open_filter(&mut self) {
        if self.filter.is_none() {
            self.filter = Some(Input::new(""));
            self.before_filter = self
                .row_id()
                .map(|id| (id, self.current().and_then(|row| self.row_session_id(row))));
        }
        self.filter_focused = true;
        // The query shares the hint line with toasts, so a leftover toast would hide what
        // is being typed.
        self.toast = None;
        // The directories are the picker's default list until something is typed. The rows
        // gain them at the end, so the cursor stays where it was.
        if self.projects_shown() {
            self.refresh_projects();
            self.rebuild();
        }
    }

    /// The filter as it is at the start: closed, or in the search preset open and empty. Nothing is
    /// rebuilt when it already is.
    fn reset_filter(&mut self) {
        let search = self.preset == KeysPreset::Search;
        let typed = self.filter.as_ref().is_some_and(|f| !f.value().is_empty());
        if !typed && !self.filter_focused && self.filter.is_some() == search {
            return;
        }
        self.filter = search.then(search_query);
        self.filter_focused = false;
        self.refresh_projects();
        self.rebuild();
        self.settle_cursor();
    }

    /// `Esc`: the query goes and the whole tree is back, with the cursor on the row it was on in
    /// the filtered list (docs/keys.md: it stays where you put it). A query that selected no row,
    /// or only a directory the tree does not list, leaves it where it was before the filter opened.
    fn clear_filter(&mut self) {
        let selected = self
            .row_id()
            .filter(|id| !matches!(id, RowId::Project(_)))
            .map(|id| (id, self.current().and_then(|row| self.row_session_id(row))));
        let keep = selected.or_else(|| self.before_filter.take());
        self.before_filter = None;
        self.filter = None;
        self.filter_focused = false;
        self.refresh_projects();
        self.rebuild();
        match keep {
            // `select`, not `keep_cursor`: the choice is the user's, so it is the new anchor.
            Some((id, session)) => match self.position_of(&id) {
                Some(at) => self.select(at),
                None => self.keep_cursor(None, session),
            },
            None => self.settle_cursor(),
        }
    }

    /// Runs one resolved [`KeyAction`]. `key` is the physical key behind it, which only
    /// `Quit` looks at — Esc leaves a filter before it leaves the popup, `q` does not.
    fn run_key(&mut self, action: KeyAction, key: KeyEvent) -> Option<Action> {
        match action {
            KeyAction::NextAttention => self.next_attention(1),
            KeyAction::PreviousAttention => self.next_attention(-1),
            KeyAction::MoveDown => self.move_by(1),
            KeyAction::MoveUp => self.move_by(-1),
            KeyAction::NextSession => self.next_session(1),
            KeyAction::PreviousSession => self.next_session(-1),
            KeyAction::Filter => self.open_filter(),
            KeyAction::Shallower => self.step_depth(-1),
            KeyAction::Deeper => self.step_depth(1),
            KeyAction::ToggleFlat => self.toggle_flat(),
            KeyAction::NewSession => self.new_session(),
            KeyAction::Help => self.overlay = Overlay::Help(Scroll::default()),
            KeyAction::Quit => {
                if key.code == KeyCode::Esc
                    && self.preset == KeysPreset::Modal
                    && self.filter.is_some()
                {
                    self.clear_filter();
                } else {
                    self.quit = true;
                    self.end = Some(AppEnd::Back);
                }
            }
            // Everything else acts on the row the cursor is on.
            KeyAction::Go => return self.row_action(RowAction::Go),
            KeyAction::Rename => return self.row_action(RowAction::Rename),
            KeyAction::Close => return self.row_action(RowAction::Close),
            KeyAction::NewWorktree => return self.row_action(RowAction::NewWorktree),
            KeyAction::Options => return self.row_action(RowAction::Options),
            KeyAction::PasteImage => return self.row_action(RowAction::PasteImage),
            // A number moves the cursor to that session and does nothing else.
            KeyAction::SelectSession(n) => self.select_session(n),
        }
        None
    }

    /// Re-finds the cursor after the rows changed shape. The anchor — the leaf you last
    /// moved to yourself — wins when it belongs to the cursor's session and is drawn again
    /// (you collapsed from it and are expanding back); else the same row if still drawn;
    /// else that session's header (its first row when flat); else the index, clamped.
    fn keep_cursor(&mut self, before: Option<RowId>, before_session: Option<String>) {
        let mut at = None;
        if let Some((anchor, anchor_session)) = &self.anchor
            && !matches!(anchor, RowId::Session(_))
            && anchor_session.is_some()
            && *anchor_session == before_session
        {
            at = self.position_of(anchor);
        }
        if at.is_none()
            && let Some(id) = &before
        {
            at = self.position_of(id);
        }
        if at.is_none()
            && let Some(session) = &before_session
        {
            at = self.rows.rows.iter().position(|row| {
                self.row_session_id(row).as_deref() == Some(session.as_str())
                    && (self.flat_now() || matches!(row, Row::Session(_)))
            });
        }
        // Not `select`: a depth change must not move the anchor.
        let clamped = self.sel.min(self.rows.rows.len().saturating_sub(1));
        self.sel = self.off_header(at.unwrap_or(clamped), 1);
        self.follow_sel = true;
        self.follow_row();
    }

    /// `h`/`l`: one step shallower or deeper, clamped at both ends, with the cursor kept
    /// on its row.
    fn step_depth(&mut self, delta: isize) {
        let before = self.row_id();
        let before_session = self.current().and_then(|row| self.row_session_id(row));
        let at = DEPTHS.iter().position(|d| *d == self.depth).unwrap_or(2) as isize;
        let next = (at + delta).clamp(0, DEPTHS.len() as isize - 1) as usize;
        self.depth = DEPTHS[next];
        self.rebuild();
        self.keep_cursor(before, before_session);
    }

    /// Whether the rows are a flat list right now: the flat layout, at a depth that has
    /// something to flatten. At the sessions depth there are only headers, so the layout
    /// is invisible there and `f` leaves it alone.
    pub fn flat_now(&self) -> bool {
        self.layout == Layout::Flat && self.depth != Depth::Sessions
    }

    /// `f`: the tree becomes one ordered list, and back, with the cursor kept on its row.
    /// At the sessions depth nothing changes on screen — the headers are the list either
    /// way — but the layout flips, so `l` shows the flat agents.
    fn toggle_flat(&mut self) {
        let before = self.row_id();
        let before_session = self.current().and_then(|row| self.row_session_id(row));
        self.layout = match self.layout {
            Layout::Tree => Layout::Flat,
            Layout::Flat => Layout::Tree,
        };
        self.rebuild();
        self.keep_cursor(before, before_session);
    }

    // ----------------------------------------------------------------- mouse

    /// Handles one mouse event.
    pub fn on_mouse(&mut self, ev: MouseEvent) -> Option<Action> {
        let (x, y) = (ev.column, ev.row);
        // A right-click outside an open menu dismisses it and is then a right-click like
        // any other, on whatever it landed on: the row under the pointer opens its own menu,
        // as a context menu does. Inside the box it is still nothing.
        if matches!(self.overlay, Overlay::Menu(_))
            && ev.kind == MouseEventKind::Down(MouseButton::Right)
            && !self.hits.overlay.area.contains((x, y).into())
        {
            self.apply(Response::Close);
        }
        if self.overlay.is_open() {
            return match ev.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let inside = self.hits.overlay.area.contains((x, y).into());
                    let hit = inside
                        .then(|| {
                            self.hits
                                .overlay
                                .rows
                                .iter()
                                .find(|(row, _)| *row == y)
                                .map(|(_, i)| *i)
                        })
                        .flatten();
                    match hit {
                        Some(index) => {
                            let response = self.overlay.click_item(index);
                            let action = self.apply(response);
                            self.refresh_project_picker();
                            action
                        }
                        // A click outside a menu dismisses it, as a context menu should.
                        // Prompts, confirmations, help and the picker wait for their own
                        // keys.
                        None if !inside && matches!(self.overlay, Overlay::Menu(_)) => {
                            self.apply(Response::Close)
                        }
                        None => None,
                    }
                }
                MouseEventKind::ScrollDown => {
                    self.overlay.scroll(true);
                    None
                }
                MouseEventKind::ScrollUp => {
                    self.overlay.scroll(false);
                    None
                }
                _ => None,
            };
        }
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let index = self.hits.tree.at(x, y)?;
                // A machine's header is a line: a click on it does nothing.
                if self.is_header(index) {
                    return None;
                }
                self.select(index);
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(at, row)| {
                    row == index && now.duration_since(at) <= DOUBLE_CLICK
                });
                self.last_click = Some((now, index));
                if double {
                    self.last_click = None;
                    return self.row_action(RowAction::Go);
                }
                None
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // The header of the filter's directories is a line, not a row, and has no
                // menu; without this it would open the menu of the row the cursor is on.
                if self.hits.tree.on_header(x, y) {
                    return None;
                }
                if let Some(index) = self.hits.tree.at(x, y) {
                    // A directory of the filter's has no menu, and the click does nothing
                    // at all: it does not even move the cursor there.
                    if matches!(self.rows.rows.get(index), Some(Row::Project(_))) {
                        return None;
                    }
                    // Nor has a machine's header.
                    if self.is_header(index) {
                        return None;
                    }
                    self.select(index);
                }
                self.open_context_menu(x, y);
                None
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let up = ev.kind == MouseEventKind::ScrollUp;
                if self.hits.preview.contains((x, y).into()) {
                    self.preview_scroll = if up {
                        (self.preview_scroll + 1).min(self.preview_max_scroll)
                    } else {
                        self.preview_scroll.saturating_sub(1)
                    };
                    return None;
                }
                self.scroll = if up {
                    self.scroll.saturating_sub(1)
                } else {
                    self.scroll.saturating_add(1)
                };
                self.follow_sel = false;
                None
            }
            _ => None,
        }
    }

    // -------------------------------------------------------------- overlays

    /// Turns an overlay's answer into an action, closing the overlay unless it asked to
    /// stay. A confirmation's action is already confirmed, and a menu entry's work is done
    /// by [`App::row_action`], which confirms for itself.
    fn apply(&mut self, response: Response) -> Option<Action> {
        match response {
            Response::Stay => None,
            Response::Close => {
                // Esc on the picker the popup started on: nothing behind it to go back to.
                if self.started_on_picker && matches!(self.overlay, Overlay::Project(_)) {
                    self.quit = true;
                }
                self.started_on_picker = false;
                self.overlay = Overlay::None;
                None
            }
            Response::Run(action) => {
                // The picker's Enter, whatever comes of it: a picker opened later is `n`'s.
                self.started_on_picker = false;
                self.overlay = Overlay::None;
                Some(action)
            }
            Response::Submit(text) => self.submit(text),
            Response::Row(what) => {
                self.overlay = Overlay::None;
                self.row_action(what)
            }
        }
    }

    /// Wraps a destructive action in a confirmation naming the tmux command it will run,
    /// unless `[ui] confirm_close` is off.
    pub(super) fn guard(&mut self, message: String, action: Action) -> Option<Action> {
        if !self.cfg.ui.confirm_close {
            return Some(action);
        }
        self.overlay = Overlay::Confirm(Confirm::new(message, action));
        None
    }
}

/// A capital Q, with the shift flag or without it (terminals differ), and no ctrl or alt.
fn is_capital_q(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('Q')
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wording;

#[cfg(test)]
mod app_mode;
