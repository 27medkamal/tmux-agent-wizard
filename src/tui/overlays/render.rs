//! Drawing the open overlay.
//!
//! Every box is sized with [`box_size`] rather than `clamp`: a `display-popup` is sized
//! by the user and can be smaller than an overlay's minimum, and clamping would abort
//! the popup instead of drawing a squeezed box.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::{KeysPreset, StatusIndicators};
use crate::tui::app::OverlayHit;
use crate::tui::data::state_icon;
use crate::tui::keys::Keymap;
use crate::tui::overlays::state::{
    Confirm, Help, Menu, Overlay, Prompt, app_help, help_keys, help_text_for,
};
use crate::tui::picker::{ProjectPicker, Tag};
use crate::tui::theme::Theme;

/// The narrowest the help overlay's key column gets, in columns.
const HELP_KEY_COLUMNS: usize = 18;
/// The fewest spaces between a help line's keys and its meaning. The 18 is the
/// modal preset's longest key, `Tab / Shift-Tab`, plus these three.
const HELP_KEY_GAP: usize = 3;
/// The name column of the help's legend, and the width of its glyph column. The longest name
/// is seven characters; the nine columns leave two spaces after it, which is what lets the
/// modal legend's longest note (the session's, 86 characters) fit the 98 columns of a help
/// line.
const LEGEND_NAME_COLUMNS: usize = 9;
const LEGEND_GLYPH_COLUMNS: usize = 2;
/// A note's example (`3`, `├`, `2:zsh.1`) sits where a legend line's glyph and name do, so
/// its text starts in the same column as theirs.
const LEGEND_NOTE_COLUMNS: usize = LEGEND_GLYPH_COLUMNS + LEGEND_NAME_COLUMNS;

/// The project picker's input, dim, while nothing is typed, kept short to fit the
/// input line rather than the wider outcome line below it.
const PICKER_PLACEHOLDER: &str = "type a project, or a path";

/// What the overlays need from the app besides their own state.
///
/// The help names the keys that are *bound*, not the preset's, so it has to see the
/// keymap; the legend draws the same glyphs the tree does, so it has to see
/// `[ui] status_indicators`.
#[derive(Clone, Copy)]
pub struct Chrome<'a> {
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub preset: KeysPreset,
    pub indicators: StatusIndicators,
    /// `[ui] projects_in_filter`: the help says the filter lists directories too.
    pub projects_in_filter: bool,
    /// Whether rows of other machines are listed (in the popup: the app that keeps the connections runs): the help
    /// says what Enter does on one of them.
    pub machines: bool,
    /// Whether the letters work: `[ui] labels` on. The help names the letter keys only then.
    pub letters_work: bool,
    /// Whether the number keys work right now ([`App::numbers_work`](crate::tui::app::App)),
    /// `[ui] labels` on and a key bound: the help names the number keys, and its legend's number
    /// is yellow and says it moves the cursor, only while they do, as the tree looks once the
    /// help closes.
    pub numbers_work: bool,
    /// Set in `taw app`, which is the whole terminal and not a popup: the help says what its keys do
    /// there, and the picker for a new session says it is this Mac's.
    pub app: Option<AppLook>,
}

/// What `taw app` changes in the overlays.
#[derive(Clone, Copy, Debug)]
pub struct AppLook {
    /// Machines are listed: the new-session picker says it makes a session on this Mac.
    pub machines: bool,
    /// The app was started by the doorway and puts the terminal back in tmux when it ends: its quit keys say so.
    pub back_to_tmux: bool,
}

/// Draws the open overlay over `area`, recording its box and one `(y, item index)` pair
/// per clickable row into `hits`.
pub fn render(
    f: &mut Frame,
    area: Rect,
    chrome: &Chrome,
    overlay: &Overlay,
    hits: &mut OverlayHit,
) {
    let theme = chrome.theme;
    *hits = OverlayHit::default();
    match overlay {
        Overlay::None => {}
        Overlay::Help(scroll) => {
            let lines = help_lines(chrome);
            // The help is one screen: it may take every row but the hint line's, which stays
            // in sight under it and says how to leave. Nineteen-twentieths of the height left
            // a 40-row client one line short of the modal help once it named the letters.
            text_overlay(
                f,
                above_hint(area),
                theme,
                " help ",
                lines,
                scroll.0,
                [0.9, 1.0],
            );
        }
        Overlay::Settings(scroll, body) => {
            let lines: Vec<Line> = body
                .lines()
                .map(|line| {
                    let colour = if line.starts_with('#') {
                        theme.dim
                    } else if line.starts_with('[') {
                        theme.accent
                    } else {
                        theme.fg
                    };
                    Line::from(Span::styled(line.to_string(), Style::default().fg(colour)))
                })
                .collect();
            text_overlay(f, area, theme, " settings ", lines, scroll.0, [0.9, 0.9]);
        }
        Overlay::Project(picker) => {
            let far_title = picker
                .far
                .as_ref()
                .map(|far| crate::machine::words::far_picker_title(&far.machine));
            let title = match (&far_title, chrome.app) {
                (Some(title), _) => title.as_str(),
                (None, Some(AppLook { machines: true, .. })) => " new session on this Mac ",
                (None, _) => " new session ",
            };
            render_project(f, area, theme, picker, title, hits);
        }
        Overlay::Prompt(prompt) => render_prompt(f, area, theme, prompt),
        Overlay::Confirm(confirm) => render_confirm(f, area, theme, confirm),
        Overlay::Menu(menu) => render_menu(f, area, theme, menu, hits),
    }
}

/// The help overlay's lines: coloured groups — how to move, how to go, what the keys do to
/// the cursor row, the mouse, and what every glyph means.
///
/// Group titles are the accent, the key column is yellow, and each legend glyph wears its
/// own state's colour so the list reads as the same alphabet the tree is written in. A key
/// line nothing is bound to disappears rather than printing an empty column.
///
/// The key column is as wide as its longest key needs, never narrower than 18:
/// a key cut at the column's edge is a key the help stops naming — a `[keys]` override
/// such as `ctrl-alt-PageDown` would lose its end. One width for every line, so the
/// meanings still start in one column.
fn help_lines(chrome: &Chrome) -> Vec<Line<'static>> {
    let theme = chrome.theme;
    let title = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let key = Style::default()
        .fg(theme.yellow)
        .add_modifier(Modifier::BOLD);
    let fg = Style::default().fg(theme.fg);
    let dim = Style::default().fg(theme.dim);
    // The number keys are named only while the numbers work, `[ui] labels` on and a key bound,
    // as the hint line names them, and the letter keys only while the letters do: with labels off
    // neither is drawn and their keys do nothing.
    let entries = help_text_for(chrome.preset, chrome.projects_in_filter)
        .into_iter()
        .filter(|entry| match entry {
            Help::NumberKeys(_) => chrome.numbers_work,
            Help::LetterKeys(_) | Help::TypedRow(_) => chrome.letters_work,
            Help::Machines => chrome.machines && chrome.app.is_none(),
            _ => true,
        })
        .collect();
    let entries = if chrome.app.is_some() {
        app_help(
            entries,
            chrome.preset,
            chrome.keymap,
            chrome.app.is_some_and(|look| look.back_to_tmux),
        )
    } else {
        entries
    };
    let entries = drop_empty_groups(entries, chrome.keymap);
    let key_columns = entries
        .iter()
        .filter_map(|entry| entry.key_line(chrome.keymap))
        .map(|(keys, _)| keys.width() + HELP_KEY_GAP)
        .fold(HELP_KEY_COLUMNS, usize::max);
    let key_line = |keys: String, what: &str| {
        Line::from(vec![
            Span::styled(format!(" {}", pad(&keys, key_columns)), key),
            Span::styled(what.to_string(), fg),
        ])
    };
    let legend = |glyph: &str, colour, name: &str, what: &str| {
        Line::from(vec![
            Span::styled(
                format!(" {}", pad(glyph, LEGEND_GLYPH_COLUMNS)),
                Style::default().fg(colour),
            ),
            Span::styled(pad(name, LEGEND_NAME_COLUMNS), fg),
            Span::styled(what.to_string(), dim),
        ])
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            Help::Group(text) => Some(Line::from(Span::styled(format!(" {text}"), title))),
            Help::Keys(..)
            | Help::Pair { .. }
            | Help::Literal(..)
            | Help::Chord { .. }
            | Help::NumberKeys(_)
            | Help::LetterKeys(_)
            | Help::TypedRow(_) => entry
                .key_line(chrome.keymap)
                .map(|(keys, what)| key_line(keys, &what)),
            // No key of its own: the words start in the meanings' column, under Enter's.
            Help::Machines => Some(key_line(
                String::new(),
                &crate::machine::words::enter_on_machine_help(crate::tui::keys::leave_key(
                    chrome.preset,
                    chrome.keymap,
                )),
            )),
            Help::State(state, name, what) => Some(legend(
                state_icon(state, chrome.indicators),
                theme.state_color(state),
                name,
                what,
            )),
            Help::Glyph(glyph, name, what) => Some(legend(glyph, theme.dim, name, what)),
            Help::Note(subject, what) => Some(Line::from(vec![
                Span::styled(format!(" {}", pad(subject, LEGEND_NOTE_COLUMNS)), dim),
                Span::styled(what.to_string(), dim),
            ])),
            Help::Number(subject, ..) => Some(Line::from(vec![
                Span::styled(
                    format!(" {}", pad(subject, LEGEND_NOTE_COLUMNS)),
                    if chrome.numbers_work { key } else { dim },
                ),
                Span::styled(
                    entry
                        .number_words(chrome.numbers_work)
                        .unwrap_or_default()
                        .to_string(),
                    dim,
                ),
            ])),
            Help::Blank => Some(Line::default()),
        })
        .collect()
}

/// Drops a [`Help::Group`] whose whole section — everything up to the next `Group` — names
/// no key, along with the blank line that introduced it, so unbinding every action under a
/// section leaves no empty header behind. The search preset's "act on the row" is reachable
/// only through `[keys]`: with nothing bound under it, `help_keys`
/// returns `None` for each of its lines and this drops the group whole; a `[keys]` entry for
/// one of them brings the section — header included — back. A section with at least one
/// `Literal`, legend line or bound `Keys` line is never touched, which is every one of the
/// modal preset's and most of the search preset's.
fn drop_empty_groups(entries: Vec<Help>, keymap: &Keymap) -> Vec<Help> {
    let has_line = |entry: &Help| match entry {
        Help::Group(_) | Help::Blank => false,
        Help::Keys(groups, _) => help_keys(groups, keymap).is_some(),
        Help::Pair { .. } | Help::NumberKeys(_) => entry.key_line(keymap).is_some(),
        _ => true,
    };
    let mut sections: Vec<Vec<Help>> = Vec::new();
    for entry in entries {
        if sections.is_empty() || matches!(entry, Help::Group(_)) {
            sections.push(Vec::new());
        }
        sections.last_mut().expect("just pushed above").push(entry);
    }
    let mut kept: Vec<Vec<Help>> = Vec::new();
    for section in sections {
        let whole =
            !matches!(section.first(), Some(Help::Group(_))) || section.iter().any(has_line);
        if whole {
            kept.push(section);
        } else if !matches!(section.last(), Some(Help::Blank)) {
            // The emptied section had no blank line after it (it is the last before `mouse`,
            // which goes without one), so the blank line before it would now stand before the
            // next title. It goes too, as the modal preset has it. A dropped section with its
            // own blank line after it leaves the one before as the separator.
            if let Some(Help::Blank) = kept.last().and_then(|before| before.last()) {
                kept.last_mut().expect("just seen").pop();
            }
        }
    }
    kept.into_iter().flatten().collect()
}

/// `area` less its bottom row, where the popup draws its hint line.
fn above_hint(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// `text` in exactly `columns` columns: padded, or cut when it is already wider, and padded
/// where a wide character would have crossed the cut. Columns, not characters: the legend's box
/// is two.
fn pad(text: &str, columns: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + c.width().unwrap_or(0) > columns {
            break;
        }
        out.push(c);
    }
    // A wide character crossing the cut is left out, so the cut text can be a column short.
    let short = columns - out.width();
    out.push_str(&" ".repeat(short));
    out
}

/// `text` broken into lines of at most `columns` columns, at its spaces. A word wider than a
/// line is broken where the line ends, so that a long branch name or path is read whole and not
/// cut at the box's edge; what follows it goes on from its last piece.
fn wrap_words(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.width() + 1 + word.width() > columns {
            lines.push(std::mem::take(&mut line));
        }
        if word.width() > columns {
            // The line is empty here: nothing fits beside a word this wide.
            for c in word.chars() {
                if !line.is_empty() && line.width() + c.width().unwrap_or(0) > columns {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

/// One dimension of an overlay's box: what it wants, raised to `min` when there is room
/// for it, and never larger than what is `available`.
///
/// `Ord::clamp` cannot be used for this. It asserts `min <= max`, and a `display-popup`
/// is sized by the user — a popup narrower or shorter than an overlay's minimum is
/// perfectly legal, and clamping would abort the whole popup instead of drawing a
/// squeezed box.
fn box_size(wanted: u16, min: u16, available: u16) -> u16 {
    wanted.max(min).min(available)
}

fn block<'a>(theme: &Theme, title: &'a str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel_bg).fg(theme.fg))
}

fn text_overlay(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    lines: Vec<Line<'static>>,
    scroll: u16,
    // `size` is the *most* of `area` the overlay covers, as [width, height] fractions.
    // The box itself is only as big as its text, so a short help does not draw a mostly
    // empty frame over the tree.
    size: [f32; 2],
) {
    let widest = lines
        .iter()
        .map(|line| line.width().min(u16::MAX as usize) as u16)
        .max()
        .unwrap_or(0);
    let room = |fraction: f32, of: u16| ((of as f32 * fraction) as u16).min(of);
    // +3: two borders and a column of air at the right edge.
    let w = box_size(widest.saturating_add(3), 20, room(size[0], area.width));
    let h = box_size(
        (lines.len() as u16).saturating_add(2),
        3,
        room(size[1], area.height),
    );
    let rect = centered(area, w, h);
    f.render_widget(Clear, rect);
    let max_scroll = (lines.len() as u16).saturating_sub(h.saturating_sub(2));
    f.render_widget(
        Paragraph::new(lines)
            .block(block(theme, title))
            .scroll((scroll.min(max_scroll), 0)),
        rect,
    );
}

/// The "new session" picker: the path being typed, its candidates with the tag saying
/// where each came from, and the outcome line spelling out what Enter would do.
///
/// The outcome is what the overlay is *for*, so it is reserved space: a popup with room
/// for three lines shows the input, the outcome, and as many candidates as are left over
/// — never a full list with the answer cut off the bottom.
fn render_project(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    picker: &ProjectPicker,
    title: &str,
    hits: &mut OverlayHit,
) {
    let mut tail = vec![Line::from(""), outcome_line(picker, theme)];
    if let Some(collision) = picker.outcome.as_ref().and_then(|o| o.collision.as_ref()) {
        tail.push(Line::from(Span::styled(
            format!(" ({collision})"),
            Style::default().fg(theme.yellow),
        )));
    }
    if let Some(error) = &picker.error {
        tail.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::default().fg(theme.red),
        )));
    }
    // Another machine's picker says it is asking that machine until the answer comes, and shows the far
    // error's own words when the ask failed.
    if let Some(far) = &picker.far {
        if far.asking {
            tail.push(Line::from(Span::styled(
                format!(
                    " {}",
                    crate::machine::words::asking_for_folders(&far.machine)
                ),
                Style::default().fg(theme.dim),
            )));
        }
        if far.truncated {
            tail.push(Line::from(Span::styled(
                format!(" {}", crate::machine::words::folders_cut(&far.machine)),
                Style::default().fg(theme.dim),
            )));
        }
        if let Some(said) = &far.sessions_error {
            tail.push(Line::from(Span::styled(
                format!(
                    " {}",
                    crate::machine::words::far_sessions_error(&far.machine, said)
                ),
                Style::default().fg(theme.red),
            )));
        }
        if let Some(said) = &far.said {
            tail.push(Line::from(Span::styled(
                format!(" {said}"),
                Style::default().fg(theme.red),
            )));
        }
    }

    let wanted = 1 + picker.candidates.len() + tail.len() + 2;
    let w = box_size(area.width * 4 / 5, 30, area.width);
    let h = box_size(wanted as u16, 5, area.height);
    let rect = centered(area, w, h);
    hits.area = rect;
    f.render_widget(Clear, rect);
    let outer = block(theme, title);
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut y = inner.y;
    let line = |f: &mut Frame, y: u16, line: Line, style: Style| {
        f.render_widget(
            Paragraph::new(line).style(style),
            Rect::new(inner.x, y, inner.width, 1),
        );
    };

    let (before, after) = picker.input.around_cursor();
    let mut input_spans = vec![
        Span::styled("> ", Style::default().fg(theme.accent)),
        Span::styled(before.to_string(), Style::default().fg(theme.fg)),
        Span::styled("\u{2588}", Style::default().fg(theme.accent)),
        Span::styled(after.to_string(), Style::default().fg(theme.fg)),
    ];
    if picker.input.value().is_empty() {
        input_spans.push(Span::styled(
            PICKER_PLACEHOLDER,
            Style::default().fg(theme.dim),
        ));
    }
    line(f, y, Line::from(input_spans), Style::default());
    y += 1;

    let room = (inner.height as usize).saturating_sub(1 + tail.len());
    let shown = picker.candidates.len().min(room);
    let first = picker.sel.saturating_sub(shown.saturating_sub(1));
    for (index, candidate) in picker.candidates.iter().enumerate().skip(first).take(shown) {
        let selected = index == picker.sel;
        let style = if selected {
            Style::default().bg(theme.selection_bg).fg(theme.fg)
        } else {
            Style::default().fg(theme.fg)
        };
        let marker = if selected { " \u{bb} " } else { "   " };
        let tag = picker.tag(index).map(Tag::label).unwrap_or("");
        line(
            f,
            y,
            Line::from(vec![
                Span::styled(format!("{marker}{:<40}", candidate.path), style),
                Span::styled(format!("   {tag}"), Style::default().fg(theme.dim)),
            ]),
            style,
        );
        hits.rows.push((y, index));
        y += 1;
    }

    for text in tail {
        if y >= inner.bottom() {
            return;
        }
        line(f, y, text, Style::default());
        y += 1;
    }
}

/// What Enter would do, in one line: switch to the session already running there, the
/// `new-session` it would run instead, or why it would do neither.
fn outcome_line(picker: &ProjectPicker, theme: &Theme) -> Line<'static> {
    let dim = Style::default().fg(theme.dim);
    let Some(outcome) = &picker.outcome else {
        return Line::from(Span::styled(
            " type a path (~/\u{2026}) or a project name".to_string(),
            dim,
        ));
    };
    let prefix = Span::styled(" Enter: ", dim);
    if let Some(far) = &picker.far {
        return Line::from(vec![
            prefix,
            match &outcome.running {
                Some(name) => Span::styled(
                    crate::machine::words::far_running(&far.machine, name),
                    Style::default().fg(theme.green),
                ),
                None => Span::styled(
                    crate::machine::words::far_open(&far.machine, &outcome.target),
                    Style::default().fg(theme.fg),
                ),
            },
        ]);
    }
    match (&outcome.running, outcome.exists) {
        (Some(name), _) => Line::from(vec![
            prefix,
            Span::styled(
                format!("switch to running session \"{name}\""),
                Style::default().fg(theme.green),
            ),
        ]),
        (None, true) => Line::from(vec![
            prefix,
            Span::styled(
                format!(
                    "new-session -s \"{}\" -c {}",
                    outcome.name.clone().unwrap_or_default(),
                    outcome.target
                ),
                Style::default().fg(theme.fg),
            ),
        ]),
        (None, false) => Line::from(Span::styled(" no such directory yet".to_string(), dim)),
    }
}

fn render_prompt(f: &mut Frame, area: Rect, theme: &Theme, prompt: &Prompt) {
    let w = box_size(area.width * 3 / 4, 20, area.width);
    let h = if prompt.error.is_some() { 4 } else { 3 };
    let rect = centered(area, w, h.min(area.height));
    f.render_widget(Clear, rect);
    let title = format!(" {} ", prompt.title);
    // The block sits *at* the cursor, not always at the end: the line is a full readline
    // input, so the two halves are what it is typed either side of.
    let (before, after) = prompt.input.around_cursor();
    let mut lines = vec![Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent)),
        Span::styled(before.to_string(), Style::default().fg(theme.fg)),
        Span::styled("\u{2588}", Style::default().fg(theme.accent)),
        Span::styled(after.to_string(), Style::default().fg(theme.fg)),
    ])];
    if let Some(error) = &prompt.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.red),
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block(theme, &title)), rect);
}

/// The question, on one line where the popup is wide enough for it and wrapped at its spaces
/// where it is not: the user answers it, and a sentence cut off at the popup's edge is one
/// they answer without having read. A popup too short for the wrapped sentence loses its
/// last lines, never the `y / n` under it, which is what the box is for.
fn render_confirm(f: &mut Frame, area: Rect, theme: &Theme, confirm: &Confirm) {
    let wanted = confirm.message.width().saturating_add(6);
    let w = box_size(wanted.min(u16::MAX as usize) as u16, 20, area.width);
    let mut lines = wrap_words(&confirm.message, w.saturating_sub(2) as usize);
    // The sentence, the `y / n` under it, and the two borders.
    let h = (lines.len() as u16).saturating_add(3);
    let rect = centered(area, w, h.min(area.height));
    lines.truncate(usize::from(rect.height.saturating_sub(3)));
    f.render_widget(Clear, rect);
    let mut text: Vec<Line> = lines
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.fg))))
        .collect();
    text.push(Line::from(Span::styled(
        confirm.answers.line(),
        Style::default().fg(theme.dim),
    )));
    f.render_widget(Paragraph::new(text).block(block(theme, " confirm ")), rect);
}

/// A lettered menu: its title on the border, then one line per entry with the key that
/// runs it in its own column, which the theme colours; the layout keeps the hit map and the
/// anchoring right either way.
fn render_menu(f: &mut Frame, area: Rect, theme: &Theme, menu: &Menu, hits: &mut OverlayHit) {
    let width = menu
        .items
        .iter()
        .map(|i| i.label.chars().count() as u16 + 4)
        .chain(std::iter::once(menu.title.chars().count() as u16))
        .max()
        .unwrap_or(10)
        + 4;
    let height = menu.items.len() as u16 + 2;
    // Anchor at the pointer, then pull back inside the popup.
    let x = menu.anchor.0.min(area.right().saturating_sub(width));
    let y = menu.anchor.1.min(area.bottom().saturating_sub(height));
    let rect = Rect::new(
        x.max(area.x),
        y.max(area.y),
        width.min(area.width),
        height.min(area.height),
    );
    hits.area = rect;
    f.render_widget(Clear, rect);
    let title = format!(" {} ", menu.title);
    let outer = block(theme, &title);
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    for (index, item) in menu.items.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + index as u16;
        let style = if index == menu.sel {
            Style::default().bg(theme.selection_bg).fg(theme.fg)
        } else {
            Style::default().fg(theme.fg)
        };
        let key = match item.key {
            Some(key) => format!(" {key} "),
            None => "   ".to_string(),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(key, style.fg(theme.yellow).add_modifier(Modifier::BOLD)),
                Span::styled(item.label.clone(), style),
            ]))
            .style(style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        hits.rows.push((y, index));
    }
}

/// A `w`x`h` rectangle in the middle of `area`.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(row);
    rect
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use std::collections::BTreeMap;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::config::Config;
    use crate::tui::actions::Action;
    use crate::tui::data::{Snapshot, SpaceRow};
    use crate::tui::overlays::state::{Item, PromptKind, RowAction, Scroll, help_text};
    use crate::tui::overlays::test_support::keymap_with;
    use crate::tui::theme;

    /// A temp `home` holding `oss/api` and `work/api`, for the project picker: it reads
    /// the filesystem, so its candidates have to come from a real one.
    fn home_with_tree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["oss/api", "work/api"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        tmp
    }

    /// A project picker over `home`, with `api` already running in `~/work/api` and
    /// `zoxide` knowing that directory too.
    fn project_picker(home: &std::path::Path, typed: &str) -> ProjectPicker {
        let mut cfg = Config::default();
        cfg.sessions.zoxide = true;
        let snap = Snapshot {
            spaces: vec![SpaceRow {
                session_id: "$1".to_string(),
                session_name: "api".to_string(),
                session_path: home.join("work/api"),
                ..SpaceRow::default()
            }],
            ..Snapshot::default()
        };
        let mut picker = ProjectPicker::new(&cfg, home);
        for c in typed.chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        picker.refresh(&snap, &cfg, home, &["~/work/api".to_string()]);
        picker
    }

    /// The default modal chrome: what every overlay is drawn against unless a test
    /// wants a different preset or keymap.
    fn chrome<'a>(theme: &'a Theme, keymap: &'a Keymap) -> Chrome<'a> {
        Chrome {
            theme,
            keymap,
            preset: KeysPreset::Modal,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: true,
            app: None,
            letters_work: true,
            machines: false,
        }
    }

    /// Another machine's picker is titled for it and says it is asking it until the answer comes; the answer's
    /// folders are listed with their tags, Enter says it opens there, and a far error shows its own words.
    /// Mutation: the far title is left out (the picker keeps `new session`).
    #[test]
    fn another_machines_picker_says_whose_it_is_and_that_it_is_asking() {
        use crate::tui::picker::FarFolder;
        let home = tempfile::tempdir().unwrap();
        let mut picker = ProjectPicker::far(
            &Config::default(),
            home.path(),
            "studio",
            Default::default(),
        );
        let text = drawn(&Overlay::Project(Box::new(picker.clone())), 100, 14);
        assert!(text.contains(" new session on studio "), "{text}");
        assert!(!text.contains("this Mac"), "{text}");
        assert!(text.contains("asking studio for its folders"), "{text}");
        picker.set_far_answer(Ok(crate::tui::picker::FarList {
            folders: vec![
                FarFolder {
                    kind: "session".into(),
                    name: "~/code/garden".into(),
                    dir: "/home/studio/code/garden".into(),
                    session: Some("garden".into()),
                },
                FarFolder {
                    kind: "zoxide".into(),
                    name: "~/web".into(),
                    dir: "/home/studio/web".into(),
                    session: None,
                },
            ],
            truncated: true,
            sessions_error: Some("server exited unexpectedly".into()),
        }));
        let text = drawn(&Overlay::Project(Box::new(picker.clone())), 100, 14);
        assert!(!text.contains("asking studio"), "{text}");
        assert!(
            text.contains("studio has more folders than it listed"),
            "{text}"
        );
        assert!(
            text.contains("studio: tmux could not list its sessions: server exited unexpectedly"),
            "{text}"
        );
        assert!(text.contains("~/code/garden"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("~/web"), "{text}");
        assert!(
            text.contains("go to running session \"garden\" on studio"),
            "{text}"
        );
        picker.move_by(1);
        picker.refresh(&Snapshot::default(), &Config::default(), home.path(), &[]);
        let text = drawn(&Overlay::Project(Box::new(picker.clone())), 100, 14);
        assert!(text.contains("open ~/web on studio"), "{text}");
        picker.set_far_answer(Err("studio: permission denied".into()));
        let text = drawn(&Overlay::Project(Box::new(picker)), 100, 14);
        assert!(text.contains("studio: permission denied"), "{text}");
        assert!(text.contains("~/web"), "the list it had stays: {text}");
    }

    /// Everything `render` draws, as text, for a `w`x`h` popup.
    fn drawn(overlay: &Overlay, w: u16, h: u16) -> String {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let keymap = crate::tui::keys::preset(KeysPreset::Modal);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = OverlayHit::default();
        terminal
            .draw(|f| {
                let area = f.area();
                render(f, area, &chrome(&theme, &keymap), overlay, &mut hits);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn every_overlay(home: &std::path::Path) -> Vec<Overlay> {
        vec![
            Overlay::Help(Scroll::default()),
            Overlay::Settings(Scroll::default(), "[ui]\nsidebar_width = 26\n".to_string()),
            Overlay::Project(Box::new(project_picker(home, ""))),
            Overlay::Project(Box::new(project_picker(home, "~/nope"))),
            Overlay::Prompt(Prompt::new(
                PromptKind::NewWorktree {
                    workspace: "$1".to_string(),
                },
                "new worktree in web: branch",
                "feat/pdf",
            )),
            Overlay::Confirm(Confirm::new(
                "kill-session web?",
                Action::KillWorkspace {
                    session: "$1".to_string(),
                },
            )),
            Overlay::Menu(Menu::new(
                "rename what? api:2:zsh.1",
                vec![
                    Item::new('w', "the window  (now \"zsh\")", RowAction::RenameWindow),
                    Item::new('a', "the agent  (now \"claude\")", RowAction::NameAgent),
                ],
                (4, 5),
            )),
        ]
    }

    /// A `display-popup` is sized by the user (`-w`/`-h` percentages of a terminal that
    /// can be tiny), and `render` is handed the whole frame — so every overlay has to
    /// survive an area smaller than the box it wants to draw.
    #[test]
    fn every_overlay_renders_in_a_popup_far_too_small_for_it() {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let home = home_with_tree();
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let chrome = Chrome {
                theme: &theme,
                keymap: &keymap,
                preset,
                indicators: StatusIndicators::Symbols,
                projects_in_filter: false,
                numbers_work: preset == KeysPreset::Modal,
                app: None,
                letters_work: true,
                machines: false,
            };
            for (w, h) in [(16, 10), (30, 8), (30, 2), (8, 3), (2, 2), (1, 1)] {
                for overlay in &every_overlay(home.path()) {
                    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                    let mut hits = OverlayHit::default();
                    terminal
                        .draw(|f| {
                            let area = f.area();
                            render(f, area, &chrome, overlay, &mut hits);
                        })
                        .unwrap_or_else(|e| panic!("{w}x{h} {overlay:?}: {e}"));
                }
            }
        }
    }

    #[test]
    fn the_project_picker_draws_its_candidates_with_tags_and_what_enter_would_do() {
        let home = home_with_tree();
        let mut picker = project_picker(home.path(), "~/work/");
        // `~/work/api` is both the completion of what was typed and a directory zoxide
        // knows — and a session is running there, which is what the row says.
        assert_eq!(picker.candidates[0].path, "~/work/api");
        let text = drawn(&Overlay::Project(Box::new(picker.clone())), 70, 14);

        assert!(text.contains("new session"), "{text}");
        assert!(text.contains("> ~/work/"), "{text}");
        assert!(text.contains("~/work/api"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(
            text.contains("Enter: switch to running session \"api\""),
            "{text}"
        );

        // Nothing is created, so no `new-session` is previewed.
        assert!(!text.contains("new-session"), "{text}");
        picker.error = Some("no such directory: ~/nope".to_string());
        assert!(
            drawn(&Overlay::Project(Box::new(picker)), 70, 14)
                .contains("no such directory: ~/nope")
        );
    }

    /// The fresh-machine steps: a repository under a project directory with a session running in
    /// it, `~/code/do` typed, Tab pressed. The popup shows the repository and says Enter switches
    /// to its session; it never offers the repository's `.git` or names a session `_git`.
    #[test]
    fn tab_on_a_running_repository_says_switch_and_shows_no_git_folder() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("code/docs/.git")).unwrap();
        let mut cfg = Config::default();
        cfg.sessions.zoxide = false;
        cfg.sessions.project_dirs = vec![home.path().join("code")];
        let snap = Snapshot {
            spaces: vec![SpaceRow {
                session_id: "$1".to_string(),
                session_name: "docs".to_string(),
                session_path: home.path().join("code/docs"),
                ..SpaceRow::default()
            }],
            ..Snapshot::default()
        };
        let mut picker = ProjectPicker::new(&cfg, home.path());
        for c in "~/code/do".chars() {
            picker.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        picker.refresh(&snap, &cfg, home.path(), &[]);
        let before = drawn(&Overlay::Project(Box::new(picker.clone())), 80, 14);
        assert!(before.contains("~/code/docs"), "{before}");
        picker.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        picker.refresh(&snap, &cfg, home.path(), &[]);

        let text = drawn(&Overlay::Project(Box::new(picker)), 80, 14);

        assert!(text.contains("> ~/code/docs/"), "{text}");
        assert!(
            text.contains("Enter: switch to running session \"docs\""),
            "{text}"
        );
        assert!(!text.contains(".git"), "{text}");
        assert!(!text.contains("_git"), "{text}");
    }

    /// The picker opens empty, so the input line carries a hint of its
    /// own rather than looking blank (the outcome line already says `type a path (~/…)
    /// or a project name` once there truly are no candidates, but that only appears
    /// below the candidate list, and an empty picker with zoxide history is not empty
    /// there).
    #[test]
    fn the_picker_shows_a_placeholder_until_something_is_typed() {
        let home = home_with_tree();
        let empty = drawn(
            &Overlay::Project(Box::new(project_picker(home.path(), ""))),
            70,
            14,
        );
        assert!(empty.contains("type a project, or a path"), "{empty}");

        let typed = drawn(
            &Overlay::Project(Box::new(project_picker(home.path(), "~/work/"))),
            70,
            14,
        );
        assert!(
            !typed.contains("type a project, or a path"),
            "the placeholder should not linger once something is typed: {typed}"
        );
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// The prompt's block is drawn where its cursor is, not always at the end: ctrl-b,
    /// ctrl-a, Left and ctrl-e move it, in text that is more than one byte to a character.
    #[test]
    fn a_prompt_draws_its_cursor_where_it_is() {
        let mut prompt = Prompt::new(
            PromptKind::NewWorktree {
                workspace: "$1".to_string(),
            },
            "new worktree in web: branch",
            "h\u{e9}llo",
        );
        let drawn = |prompt: &Prompt| drawn(&Overlay::Prompt(prompt.clone()), 60, 6);

        assert!(
            drawn(&prompt).contains("> h\u{e9}llo\u{2588}"),
            "{}",
            drawn(&prompt)
        );
        prompt
            .input
            .handle(press(KeyCode::Left, KeyModifiers::NONE));
        prompt
            .input
            .handle(press(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(
            drawn(&prompt).contains("> h\u{e9}l\u{2588}lo"),
            "{}",
            drawn(&prompt)
        );
        prompt
            .input
            .handle(press(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(
            drawn(&prompt).contains("> \u{2588}h\u{e9}llo"),
            "{}",
            drawn(&prompt)
        );
        prompt
            .input
            .handle(press(KeyCode::Char('f'), KeyModifiers::CONTROL));
        prompt
            .input
            .handle(press(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(
            drawn(&prompt).contains("> h\u{e9}\u{2588}llo"),
            "{}",
            drawn(&prompt)
        );
        prompt
            .input
            .handle(press(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(
            drawn(&prompt).contains("> h\u{e9}llo\u{2588}"),
            "{}",
            drawn(&prompt)
        );
    }

    /// The same for the project picker's line.
    #[test]
    fn the_picker_draws_its_cursor_where_it_is() {
        let home = home_with_tree();
        let mut picker = project_picker(home.path(), "~/work/");
        let drawn =
            |picker: &ProjectPicker| drawn(&Overlay::Project(Box::new(picker.clone())), 70, 14);

        assert!(
            drawn(&picker).contains("> ~/work/\u{2588}"),
            "{}",
            drawn(&picker)
        );
        picker.on_key(press(KeyCode::Left, KeyModifiers::NONE));
        picker.on_key(press(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            drawn(&picker).contains("> ~/wor\u{2588}k/"),
            "{}",
            drawn(&picker)
        );
        picker.on_key(press(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(
            drawn(&picker).contains("> \u{2588}~/work/"),
            "{}",
            drawn(&picker)
        );
    }

    #[test]
    fn the_project_picker_previews_the_new_session_and_its_collision() {
        let home = home_with_tree();
        let text = drawn(
            &Overlay::Project(Box::new(project_picker(home.path(), "~/oss/api"))),
            70,
            14,
        );

        assert!(
            text.contains("Enter: new-session -s \"oss/api\" -c ~/oss/api"),
            "{text}"
        );
        assert!(text.contains("is taken by ~/work/api"), "{text}");
    }

    #[test]
    fn the_project_picker_says_when_the_directory_is_not_there_yet() {
        let home = home_with_tree();
        let text = drawn(
            &Overlay::Project(Box::new(project_picker(home.path(), "~/nope"))),
            70,
            14,
        );

        assert!(text.contains("no such directory yet"), "{text}");
        assert!(!text.contains("new-session"), "{text}");
    }

    /// The outcome line is the point of the overlay, so it survives a popup with room
    /// for almost nothing — the candidate list is what gives way.
    #[test]
    fn a_short_popup_keeps_the_outcome_line_and_drops_candidates() {
        let home = home_with_tree();
        let text = drawn(
            &Overlay::Project(Box::new(project_picker(home.path(), "~/work/"))),
            70,
            6,
        );

        assert!(text.contains("> ~/work/"), "{text}");
        assert!(
            text.contains("Enter: switch to running session \"api\""),
            "{text}"
        );
    }

    #[test]
    fn an_overlay_that_fits_is_drawn_inside_the_area_it_was_given() {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let keymap = crate::tui::keys::preset(KeysPreset::Modal);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut hits = OverlayHit::default();
        let overlay = Overlay::Confirm(Confirm::new(
            "close this pane?",
            Action::KillPane {
                pane: "%1".to_string(),
            },
        ));
        terminal
            .draw(|f| {
                let area = f.area();
                render(f, area, &chrome(&theme, &keymap), &overlay, &mut hits);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..buf.area().height)
            .map(|y| {
                (0..buf.area().width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("close this pane?"), "{text}");
        assert!(text.contains("confirm"), "{text}");
    }

    /// A confirmation is a sentence the user answers, so no popup cuts it off: one line where
    /// it fits, and wrapped at a space where the popup is narrower. The last-pane sentence
    /// made the longest one 83 columns, wider than an 80-column popup.
    #[test]
    fn a_confirmation_wider_than_the_popup_wraps_instead_of_being_cut() {
        let message =
            "kill-pane docs:4 (claude)? It is the window's last pane, so the window closes too.";
        let overlay = Overlay::Confirm(Confirm::new(
            message,
            Action::KillPane {
                pane: "%1".to_string(),
            },
        ));
        // The sentence as the box holds it: every line between the borders but the last,
        // which is the `y / n` under it.
        let sentence = |width: u16| -> String {
            let text = drawn(&overlay, width, 12);
            let boxed: Vec<String> = text
                .lines()
                .filter(|line| line.contains('\u{2502}'))
                .map(|line| {
                    line.trim_matches(|c| c == '\u{2502}' || c == ' ')
                        .to_string()
                })
                .collect();
            assert_eq!(boxed.last().map(String::as_str), Some("y / n"), "{text}");
            boxed[..boxed.len() - 1].join(" ")
        };
        for width in [120, 90, 80, 60, 40, 30] {
            assert_eq!(sentence(width), message, "{width} columns wide");
        }
        let text = drawn(&overlay, 120, 12);
        assert!(text.contains(message), "one line where it fits:\n{text}");
        let text = drawn(&overlay, 60, 12);
        assert!(!text.contains(message), "two where it does not:\n{text}");
    }

    /// The line under the question says how it is answered: `y / n` for an ordinary
    /// confirmation, and `y / N` for the two that have a default answer of their own (a
    /// removal git refused, which a stray Enter must keep, and the group close).
    #[test]
    fn the_line_under_a_question_is_the_answers_of_its_kind() {
        let action = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        for (overlay, under) in [
            (Confirm::new("kill-session api?", action.clone()), "y / n"),
            (
                Confirm::strict(
                    "remove anyway? uncommitted changes are lost",
                    action.clone(),
                ),
                "y / N",
            ),
            (
                Confirm::choice("close them too?", action.clone(), action.clone()),
                "y / N",
            ),
        ] {
            let message = overlay.message.clone();
            let text = drawn(&Overlay::Confirm(overlay), 80, 12);
            let boxed: Vec<String> = text
                .lines()
                .filter(|line| line.contains('\u{2502}'))
                .map(|line| {
                    line.trim_matches(|c| c == '\u{2502}' || c == ' ')
                        .to_string()
                })
                .collect();
            assert_eq!(boxed, [message.as_str(), under], "{text}");
        }
    }

    /// The questions the popup raises itself say what they say whatever the popup's width:
    /// never cut at its edge, wrapped at a space, and the `y / N` under them always there. (The
    /// sentences are the longest the popup says: 100, 180 and 183 columns, past an 80-column
    /// popup's.)
    #[test]
    fn the_questions_a_removal_and_a_group_raise_are_never_cut_off() {
        let action = Action::KillWorkspace {
            session: "$2".to_string(),
        };
        let questions = [
            "kill-session repo? It has 3 worktree sessions open (repo--feat-a, repo--feat-b, repo--feat-c). Close them too?",
            "git no longer lists feature/a-long-branch-name, but part of its folder is still there: ~/worktrees/a-repository/feature-a-long-branch-name. remove anyway? uncommitted changes are lost",
            "kill-pane repo:1 (claude)? It is the session's last pane, so the session closes with it. It has 3 worktree sessions open (repo--feat-a, repo--feat-b, repo--feat-c). Close them too?",
        ];
        for message in questions {
            for confirm in [
                Confirm::strict(message, action.clone()),
                Confirm::choice(message, action.clone(), action.clone()),
            ] {
                let overlay = Overlay::Confirm(confirm);
                for width in [150, 120, 100, 80, 60, 40] {
                    let text = drawn(&overlay, width, 14);
                    let boxed: Vec<String> = text
                        .lines()
                        .filter(|line| line.contains('\u{2502}'))
                        .map(|line| {
                            line.trim_matches(|c| c == '\u{2502}' || c == ' ')
                                .to_string()
                        })
                        .collect();
                    assert_eq!(
                        boxed.last().map(String::as_str),
                        Some("y / N"),
                        "{width}: {text}"
                    );
                    // Nothing is cut: every word is there, in order. Where each word fits a line
                    // the lines are the sentence's own words, and where one does not (the folder
                    // in a 40-column popup) it is broken, so only its spaces differ.
                    let sentence = &boxed[..boxed.len() - 1];
                    assert_eq!(
                        sentence.concat().replace(' ', ""),
                        message.replace(' ', ""),
                        "{width} columns wide:\n{text}"
                    );
                    if width >= 60 {
                        assert_eq!(sentence.join(" "), message, "{width} columns wide:\n{text}");
                    }
                    if width <= 80 {
                        assert!(boxed.len() > 2, "{width} columns wraps it:\n{text}");
                    }
                }
            }
        }
    }

    /// A popup too short for the whole sentence loses the sentence's last lines and never
    /// the `y / n` under it, which is what the box is for. At 30 columns the last-pane
    /// sentence is four lines, so a popup 7 rows high holds all of it, 6 rows three of its
    /// lines and 5 rows two.
    #[test]
    fn a_confirmation_in_a_popup_too_short_for_it_drops_its_last_lines_and_keeps_the_y_n() {
        let overlay = Overlay::Confirm(Confirm::new(
            "kill-pane docs:4 (claude)? It is the window's last pane, so the window closes too.",
            Action::KillPane {
                pane: "%1".to_string(),
            },
        ));
        // What the box holds between its borders, one string per line.
        let boxed = |height: u16| -> Vec<String> {
            drawn(&overlay, 30, height)
                .lines()
                .filter(|line| line.contains('\u{2502}'))
                .map(|line| {
                    line.trim_matches(|c| c == '\u{2502}' || c == ' ')
                        .to_string()
                })
                .collect()
        };
        assert_eq!(
            boxed(7),
            [
                "kill-pane docs:4 (claude)?",
                "It is the window's last",
                "pane, so the window closes",
                "too.",
                "y / n",
            ],
            "all of it where it fits"
        );
        assert_eq!(
            boxed(6),
            [
                "kill-pane docs:4 (claude)?",
                "It is the window's last",
                "pane, so the window closes",
                "y / n",
            ],
            "30x6 drops the last line"
        );
        assert_eq!(
            boxed(5),
            [
                "kill-pane docs:4 (claude)?",
                "It is the window's last",
                "y / n"
            ],
            "30x5 drops the last two"
        );
    }

    /// A word wider than the line is broken where the line ends, so that a long branch name or
    /// path is read whole and not cut at the box's edge, and what follows it goes on from its
    /// last piece. A wide character is never split.
    #[test]
    fn words_wrap_at_spaces_and_a_word_wider_than_the_line_is_broken_where_it_ends() {
        assert_eq!(wrap_words("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap_words("one two three", 13), ["one two three"]);
        assert_eq!(wrap_words("abcdefghij kl", 4), ["abcd", "efgh", "ij", "kl"]);
        assert_eq!(wrap_words("ab cdefgh k", 4), ["ab", "cdef", "gh k"]);
        assert_eq!(
            wrap_words("\u{65e5}\u{672c}\u{8a9e}\u{65e5}\u{672c}\u{8a9e}", 5),
            ["\u{65e5}\u{672c}", "\u{8a9e}\u{65e5}", "\u{672c}\u{8a9e}"]
        );
        assert_eq!(wrap_words("", 4), [""]);
        assert_eq!(wrap_words("a b", 0), ["a", "b"]);
    }

    /// A long folder name in a question is drawn whole across the lines of its box.
    #[test]
    fn a_confirmation_with_a_word_wider_than_its_box_shows_all_of_it() {
        let folder = "~/worktrees/a-repository/feature-with-a-very-long-branch-name-indeed";
        let message = format!("git no longer lists it, but part of {folder} is still there.");
        let overlay = Overlay::Confirm(Confirm::new(
            message,
            Action::KillWorkspace {
                session: "$1".to_string(),
            },
        ));
        let text = drawn(&overlay, 40, 14);
        let shown: String = text
            .lines()
            .filter(|line| line.contains('\u{2502}'))
            .map(|line| line.trim_matches(|c| c == '\u{2502}' || c == ' '))
            .collect::<Vec<_>>()
            .concat();
        assert!(shown.contains(folder), "{text}");
    }

    /// The help as [`help_lines`] draws it, one string per line, with `preset`'s own keys:
    /// the widths below are the renderer's, not a copy of its column rules.
    fn help_lines_for(preset: KeysPreset) -> Vec<String> {
        help_lines_under(preset, &crate::tui::keys::preset(preset))
    }

    /// [`help_lines_for`] under `keymap`, for a `[keys]` table that binds more than the
    /// preset does.
    fn help_lines_under(preset: KeysPreset, keymap: &Keymap) -> Vec<String> {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let chrome = Chrome {
            theme: &theme,
            keymap,
            preset,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: preset == KeysPreset::Modal,
            app: None,
            letters_work: true,
            machines: false,
        };
        help_lines(&chrome)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    /// [`help_lines_for`] with the filter's directories switch as given: the modal chrome of
    /// `preset`'s own keys, drawn the way the popup draws it.
    fn help_lines_with(preset: KeysPreset, projects_in_filter: bool) -> Vec<String> {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let keymap = crate::tui::keys::preset(preset);
        let chrome = Chrome {
            theme: &theme,
            keymap: &keymap,
            preset,
            indicators: StatusIndicators::Symbols,
            projects_in_filter,
            numbers_work: preset == KeysPreset::Modal,
            app: None,
            letters_work: true,
            machines: false,
        };
        help_lines(&chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The help as drawn with the numbers and letters working or not (`[ui] labels` on or off),
    /// in `preset`'s own keys.
    fn help_lines_labels(preset: KeysPreset, labels: bool) -> Vec<String> {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let keymap = crate::tui::keys::preset(preset);
        let chrome = Chrome {
            theme: &theme,
            keymap: &keymap,
            preset,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: labels,
            app: None,
            letters_work: labels,
            machines: false,
        };
        help_lines(&chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The drawn help says, in both presets, that typing a row's number and letter reaches it,
    /// and the legend's number says what typing or pressing it does; with `[ui] labels` off
    /// there is no address to type, and neither says it.
    #[test]
    fn the_drawn_help_says_how_to_type_a_rows_address_only_while_labels_are_on() {
        for (preset, typed, number) in [
            (
                KeysPreset::Modal,
                "jump to a row of your session  \u{b7}  row a of session 4, in the search too",
                "session: bold, numbered; its number puts the cursor on it",
            ),
            (
                KeysPreset::Search,
                "row a of session 4: type its number, then its letter",
                "session: bold, numbered; typing its number puts the cursor on it",
            ),
        ] {
            let on = help_lines_labels(preset, true);
            let line = on
                .iter()
                .find(|line| line.contains("row a of session 4"))
                .unwrap_or_else(|| panic!("{preset:?}: the 4a line: {on:#?}"));
            assert!(line.contains("4a "), "{preset:?}: {line:?}");
            assert!(line.ends_with(typed), "{preset:?}: {line:?}");
            assert!(
                on.iter().any(|line| line.contains(number)),
                "{preset:?}: {on:#?}"
            );
            let off = help_lines_labels(preset, false);
            assert!(
                !off.iter()
                    .any(|line| line.trim_start().starts_with("4a ")
                        || line.contains("of session 4")),
                "{preset:?}: labels off: {off:#?}"
            );
            assert!(
                off.iter()
                    .any(|line| line.contains("session: bold, numbered in tmux order")),
                "{preset:?}: {off:#?}"
            );
        }
    }

    /// With `[keys] select_session` on ctrl+1 to ctrl+9 a session's label is a key's name, and
    /// `ctrl-4a` cannot be typed into a query: the help names no typed address, in either preset,
    /// while the modal preset's letters line still names the letters. On the digits backwards
    /// (session 4 on `6`) the address `6a` can be typed and is named.
    #[test]
    fn the_help_names_a_typed_address_only_while_a_query_can_hold_it() {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let ctrl: Vec<String> = (1..=9).map(|n| format!("ctrl+{n}")).collect();
        let ctrl: Vec<&str> = ctrl.iter().map(String::as_str).collect();
        let plain = ["9", "8", "7", "6", "5", "4", "3", "2", "1"];
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            for (keys, address) in [(&ctrl[..], None), (&plain[..], Some("6a "))] {
                let keymap = keymap_with(preset, &[("select_session", keys)]);
                let chrome = Chrome {
                    theme: &theme,
                    keymap: &keymap,
                    preset,
                    indicators: StatusIndicators::Symbols,
                    projects_in_filter: false,
                    numbers_work: true,
                    app: None,
                    letters_work: true,
                    machines: false,
                };
                let lines: Vec<String> = help_lines(&chrome)
                    .iter()
                    .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                    .collect();
                let named = lines.iter().find(|line| line.contains("of session 4"));
                match address {
                    None => assert!(named.is_none(), "{preset:?}: {lines:#?}"),
                    Some(address) => assert!(
                        named.is_some_and(|line| line.contains(address)),
                        "{preset:?}: {lines:#?}"
                    ),
                }
                if preset == KeysPreset::Modal {
                    assert!(
                        lines
                            .iter()
                            .any(|line| line.contains("jump to a row of your session")),
                        "{preset:?}: the letters line stays: {lines:#?}"
                    );
                }
            }
        }
    }

    /// The drawn help names the directories the filter lists only when it lists them, on
    /// the line of the filter in either preset, and it still fits the 98 columns of a help
    /// line with the longer wording.
    #[test]
    fn the_drawn_help_names_the_directories_only_when_the_filter_lists_them() {
        let said = "filter (fzf syntax), by state too; directories to open show below";
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let off = help_lines_with(preset, false);
            let on = help_lines_with(preset, true);

            assert!(
                on.iter().any(|line| line.contains(said)),
                "{preset:?}: {on:?}"
            );
            assert!(
                !off.iter().any(|line| line.contains("directories")),
                "{preset:?}: {off:?}"
            );
            assert_eq!(on.len(), off.len(), "{preset:?}: no line is added");
            for line in &on {
                let columns = line.width();
                assert!(columns <= 98, "{preset:?}: {columns} columns: {line}");
            }
        }
    }

    /// Drawn: no blank line before the `mouse` title in either preset. The search preset's
    /// emptied "act on the row" group leaves no blank line behind, so the two presets look alike.
    #[test]
    fn no_blank_line_before_the_mouse_title_in_either_preset() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let lines = help_lines_for(preset);
            let at = lines
                .iter()
                .position(|line| line.trim() == "mouse")
                .unwrap_or_else(|| panic!("{preset:?}: the mouse title: {lines:?}"));
            assert!(at > 0, "{preset:?}: {lines:?}");
            assert!(
                !lines[at - 1].trim().is_empty(),
                "{preset:?}: a blank line before mouse: {lines:?}"
            );
        }
    }

    /// `pad` gives exactly `columns` columns, counted as the terminal does: a wide character that
    /// would cross the cut is left out and a space takes its place.
    #[test]
    fn pad_is_exactly_the_columns_asked_for_cut_or_not() {
        for (text, columns, want) in [
            ("ab", 5, "ab   "),
            ("abc", 3, "abc"),
            ("abcd", 3, "abc"),
            ("ab\u{1f4e6}", 3, "ab "),
            ("\u{1f4e6}\u{1f4e6}", 3, "\u{1f4e6} "),
            ("\u{1f4e6}\u{1f4e6}", 4, "\u{1f4e6}\u{1f4e6}"),
            ("\u{1f4e6}", 1, " "),
        ] {
            let padded = pad(text, columns);
            assert_eq!(padded, want, "{text:?} in {columns}");
            assert_eq!(padded.width(), columns, "{text:?} in {columns}");
        }
    }

    /// No help line is cut by its box. The box is at most nine tenths of
    /// the popup and three columns wider than its widest line, so a line of 98 columns is
    /// whole on a popup 113 columns wide.
    #[test]
    fn every_help_line_fits_ninety_eight_columns_in_both_presets() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let lines = help_lines_for(preset);
            assert!(
                lines.iter().any(|line| line.contains("legend")),
                "{preset:?}: the whole help, legend included: {lines:?}"
            );
            for line in lines {
                let columns = line.width();
                assert!(columns <= 98, "{preset:?}: {columns} columns: {line}");
            }
        }
    }

    /// The box's legend line starts its words where the other notes do: the 2-column mark is
    /// padded as two columns, not one character.
    #[test]
    fn the_boxs_legend_line_starts_its_words_in_the_notes_column() {
        let lines = help_lines_for(KeysPreset::Modal);
        let start = |words: &str| {
            let line = lines.iter().find(|line| line.contains(words)).unwrap();
            line[..line.find(words).unwrap()].width()
        };
        assert_eq!(
            start("in a container that taw started, or saw start"),
            start("window 2, named zsh, pane 1")
        );
    }

    /// The walk's line is the longest of the key lines, and the search preset has it only
    /// once `[keys]` binds the walk: it fits the same 98 columns there, whole.
    #[test]
    fn the_walks_line_fits_ninety_eight_columns_in_the_search_preset_once_it_is_bound() {
        use crate::config::{KeySpec, Keys};
        let keys = Keys {
            preset: KeysPreset::Search,
            bindings: [
                (
                    "next_attention".to_string(),
                    KeySpec::One("tab".to_string()),
                ),
                (
                    "previous_attention".to_string(),
                    KeySpec::One("shift+tab".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let (keymap, diagnostics) = crate::tui::keys::from_config(&keys);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let lines = help_lines_under(KeysPreset::Search, &keymap);
        let walk = lines
            .iter()
            .find(|line| line.contains("who to deal with next"))
            .unwrap_or_else(|| panic!("the walk's line is in the help: {lines:?}"));
        assert!(walk.contains("Tab / Shift-Tab"), "{walk}");
        for line in &lines {
            let columns = line.width();
            assert!(columns <= 98, "{columns} columns: {line}");
        }
    }

    /// The line of `lines` that starts with `keys` once its leading space is off.
    fn line_of<'a>(lines: &'a [String], keys: &str) -> (usize, &'a str) {
        lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.trim_start().starts_with(keys))
            .map(|(at, line)| (at, line.as_str()))
            .unwrap_or_else(|| panic!("no line starts with {keys:?}: {lines:#?}"))
    }

    /// The `prefix+O` line is the next up line's neighbour, right under it and saying it is
    /// the same next up; it is not under `go`, which holds Enter alone. The modal preset, and the search preset once `[keys]` binds the walk.
    #[test]
    fn the_drawn_chord_line_is_right_under_the_next_up_line() {
        let same = "the same next up from anywhere in tmux, straight there without the popup";
        let next_up = [
            ("next_attention", &["tab"][..]),
            ("previous_attention", &["shift+tab"][..]),
        ];
        for (preset, lines) in [
            (KeysPreset::Modal, help_lines_for(KeysPreset::Modal)),
            (
                KeysPreset::Search,
                help_lines_under(
                    KeysPreset::Search,
                    &keymap_with(KeysPreset::Search, &next_up),
                ),
            ),
        ] {
            let (walk, walk_line) = line_of(&lines, "Tab / Shift-Tab");
            assert!(
                walk_line.ends_with("(what needs you, else every agent)"),
                "{preset:?}: {walk_line}"
            );
            let chord = &lines[walk + 1];
            assert!(
                chord.trim_start().starts_with("prefix+O") && chord.ends_with(same),
                "{preset:?}: the line under the next up line: {chord:?}"
            );
            let (go, _) = line_of(&lines, "go");
            assert!(
                lines[go + 1].trim_start().starts_with("Enter"),
                "{preset:?}: {lines:#?}"
            );
            assert!(
                lines[go + 1..]
                    .iter()
                    .take_while(|line| !line.trim().is_empty())
                    .all(|line| !line.contains("prefix+O")),
                "{preset:?}: the chord is not under go: {lines:#?}"
            );
        }
    }

    /// With `[keys]` leaving next up no key its line is not drawn, and the chord's line stands
    /// where it would have been and says what next up is: "the same" would be the same as
    /// nothing. The search preset is that case as it ships, and one key left is not.
    #[test]
    fn the_drawn_chord_line_says_what_next_up_is_when_no_line_above_it_does() {
        let alone = "next up from anywhere in tmux: what needs you, else every agent";
        let same = "the same next up from anywhere in tmux, straight there without the popup";
        let none = [("next_attention", &[][..]), ("previous_attention", &[][..])];
        let modal = help_lines_under(KeysPreset::Modal, &keymap_with(KeysPreset::Modal, &none));
        for (preset, lines, above, below) in [
            (KeysPreset::Modal, modal, "a-e   4a", "h / l"),
            (
                KeysPreset::Search,
                help_lines_for(KeysPreset::Search),
                "Down / Up",
                "go",
            ),
        ] {
            assert!(
                !lines
                    .iter()
                    .any(|line| line.contains("who to deal with next")),
                "{preset:?}: no next up line: {lines:#?}"
            );
            let (chord, chord_line) = line_of(&lines, "prefix+O");
            assert!(chord_line.ends_with(alone), "{preset:?}: {chord_line:?}");
            assert!(
                !lines.iter().any(|line| line.contains("the same")),
                "{preset:?}: the same as nothing: {lines:#?}"
            );
            assert!(
                lines[chord - 1].trim_start().starts_with(above),
                "{preset:?}: where the next up line would be: {lines:#?}"
            );
            let after = lines[chord + 1..]
                .iter()
                .find(|line| !line.trim().is_empty());
            assert!(
                after.is_some_and(|line| line.trim_start().starts_with(below)),
                "{preset:?}: {lines:#?}"
            );
        }
        // One of the two keys left, whichever it is: the line is drawn, with that key alone,
        // and the chord is the same as it.
        for (unbound, left) in [
            ("previous_attention", "Tab "),
            ("next_attention", "Shift-Tab"),
        ] {
            let one = [(unbound, &[][..])];
            let keymap = keymap_with(KeysPreset::Modal, &one);
            let lines = help_lines_under(KeysPreset::Modal, &keymap);
            let (walk, walk_line) = line_of(&lines, left);
            assert!(
                !walk_line.contains("Tab / Shift-Tab"),
                "{unbound} unbound: {walk_line}"
            );
            assert!(
                lines[walk + 1].ends_with(same),
                "{unbound} unbound: {lines:#?}"
            );
        }
    }

    /// The modal help is one screen for someone new: every group, the mouse
    /// and the legend included, in at most 40 lines.
    #[test]
    fn the_modal_help_legend_included_is_at_most_forty_lines() {
        let lines = help_lines_for(KeysPreset::Modal);
        assert!(
            lines
                .last()
                .is_some_and(|line| line.contains("the way tmux writes it")),
            "the legend's last line is the help's last: {lines:?}"
        );
        assert!(lines.len() <= 40, "{} lines: {lines:#?}", lines.len());
    }

    /// A key the help names is drawn whole: a `[keys]` override longer than the
    /// 18-column key column — `go = "ctrl+alt+pagedown"` in the search preset — cut there
    /// would be a key the help stops naming. The column widens to the longest key instead,
    /// and every meaning still starts in one column.
    #[test]
    fn no_key_the_help_names_is_cut_from_its_column() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let keys: Vec<(String, String)> = help_text(preset)
                .iter()
                .filter_map(|entry| entry.key_line(&keymap))
                .collect();
            let lines = help_lines_for(preset);
            let mut starts = Vec::new();
            for (keys, what) in keys {
                let line = lines
                    .iter()
                    .find(|line| line.ends_with(&what) && line.trim_start().starts_with(&keys))
                    .unwrap_or_else(|| panic!("{preset:?}: no whole line for {keys} {what}"));
                starts.push(line.chars().count() - what.chars().count());
            }
            starts.dedup();
            assert_eq!(
                starts.len(),
                1,
                "{preset:?}: one meaning column: {starts:?}"
            );
        }
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let (keymap, diagnostics) = crate::tui::keys::from_config(&crate::config::Keys {
            preset: KeysPreset::Search,
            bindings: [(
                "go".to_string(),
                crate::config::KeySpec::One("ctrl+alt+pagedown".to_string()),
            )]
            .into_iter()
            .collect(),
        });
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let chrome = Chrome {
            theme: &theme,
            keymap: &keymap,
            preset: KeysPreset::Search,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: false,
            app: None,
            letters_work: true,
            machines: false,
        };
        let lines: Vec<String> = help_lines(&chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let go = lines
            .iter()
            .find(|line| line.ends_with("go to the row under the cursor"))
            .unwrap_or_else(|| panic!("a line for go: {lines:?}"));
        assert!(go.starts_with(" ctrl-alt-PageDown   "), "{go:?}");
        let quit = lines
            .iter()
            .find(|line| line.ends_with("quit"))
            .unwrap_or_else(|| panic!("a line for quit: {lines:?}"));
        assert_eq!(
            go.chars().count() - "go to the row under the cursor".chars().count(),
            quit.chars().count() - "quit".chars().count(),
            "the meanings still start in one column: {go:?} {quit:?}"
        );
    }

    /// `drop_empty_groups` on lines no shipped preset reaches: an emptied section with no
    /// blank line of its own takes the blank line before it along (it is the last section,
    /// which goes without one); an emptied section that has its own blank line leaves the
    /// one before it as the separator. A section that names a key is never touched.
    #[test]
    fn an_emptied_group_takes_the_blank_before_it_only_when_it_has_none_after_it() {
        use crate::tui::keys::Action as K;
        const GONE: &[&[K]] = &[&[K::NextAttention]];
        let keymap = keymap_with(KeysPreset::Modal, &[("next_attention", &[][..])]);
        let shape = |entries: Vec<Help>| -> Vec<String> {
            drop_empty_groups(entries, &keymap)
                .iter()
                .map(|entry| match entry {
                    Help::Group(name) => format!("group {name}"),
                    Help::Literal(key, _) => format!("literal {key}"),
                    Help::Keys(..) => "keys".to_string(),
                    Help::Blank => "blank".to_string(),
                    _ => "other".to_string(),
                })
                .collect()
        };
        // The emptied group is last, with no blank line after it: the blank before it goes.
        assert_eq!(
            shape(vec![
                Help::Group("one"),
                Help::Literal("x", "kept"),
                Help::Blank,
                Help::Group("two"),
                Help::Keys(GONE, "gone"),
            ]),
            ["group one", "literal x"]
        );
        // The emptied group has its own blank line: the blank before it stays, one separator.
        assert_eq!(
            shape(vec![
                Help::Group("one"),
                Help::Literal("x", "kept"),
                Help::Blank,
                Help::Group("two"),
                Help::Keys(GONE, "gone"),
                Help::Blank,
                Help::Group("three"),
                Help::Literal("y", "kept"),
            ]),
            [
                "group one",
                "literal x",
                "blank",
                "group three",
                "literal y"
            ]
        );
    }

    /// The search preset's "act on the row" group — reachable, per the README, only
    /// through `[keys]`, names none of its actions and does not show its
    /// own header while every one of them is unbound; binding one, in the modal help's own
    /// wording, brings both the line and the group back.
    #[test]
    fn the_search_help_names_a_keys_bound_action_once_it_has_one() {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let default_keymap = crate::tui::keys::preset(KeysPreset::Search);
        let default_chrome = Chrome {
            theme: &theme,
            keymap: &default_keymap,
            preset: KeysPreset::Search,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: false,
            app: None,
            letters_work: true,
            machines: false,
        };
        let default_lines: Vec<String> = help_lines(&default_chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            !default_lines
                .iter()
                .any(|line| line.contains("act on the row")),
            "no line names the group while nothing under it is bound: {default_lines:?}"
        );

        let (keymap, diagnostics) = crate::tui::keys::from_config(&crate::config::Keys {
            preset: KeysPreset::Search,
            bindings: [(
                "rename".to_string(),
                crate::config::KeySpec::One("ctrl+g".to_string()),
            )]
            .into_iter()
            .collect(),
        });
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let chrome = Chrome {
            theme: &theme,
            keymap: &keymap,
            preset: KeysPreset::Search,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: false,
            app: None,
            letters_work: true,
            machines: false,
        };
        let lines: Vec<String> = help_lines(&chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("act on the row")),
            "the group appears now something under it is bound: {lines:?}"
        );
        let rename = lines
            .iter()
            .find(|line| line.ends_with("rename  \u{b7}  close"))
            .unwrap_or_else(|| panic!("a rename line: {lines:?}"));
        assert!(rename.trim_start().starts_with("ctrl-g"), "{rename:?}");
    }
    /// The default preset's help lists `ctrl-v` (on the row of rename and close, which a row of its own would
    /// push off the sample client); the search preset's lists it only once `[keys]` binds `paste_image`.
    #[test]
    fn the_help_lists_the_paste_image_key_in_the_default_preset_and_in_the_search_one_once_bound() {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let lines_of = |preset: KeysPreset, keymap: &Keymap| -> Vec<String> {
            let chrome = Chrome {
                theme: &theme,
                keymap,
                preset,
                indicators: StatusIndicators::Symbols,
                projects_in_filter: false,
                numbers_work: false,
                app: None,
                letters_work: true,
                machines: false,
            };
            help_lines(&chrome)
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let modal = lines_of(
            KeysPreset::Modal,
            &crate::tui::keys::preset(KeysPreset::Modal),
        );
        assert!(
            modal.iter().any(|line| line.contains("ctrl-v")
                && line.ends_with("rename  \u{b7}  close  \u{b7}  paste image")),
            "{modal:?}"
        );
        let search = lines_of(
            KeysPreset::Search,
            &crate::tui::keys::preset(KeysPreset::Search),
        );
        // The mouse line names the menu's paste image entry; no key line does.
        assert!(
            search.iter().all(|line| !line.contains("ctrl-v")
                && (line.contains("right-click") || !line.contains("paste"))),
            "{search:?}"
        );
        assert!(
            search.iter().any(|line| line.contains("right-click")
                && line.ends_with("menu: go, rename, close, new worktree, paste image")),
            "{search:?}"
        );
        let (bound, diagnostics) = crate::tui::keys::from_config(&crate::config::Keys {
            preset: KeysPreset::Search,
            bindings: [(
                "paste_image".to_string(),
                crate::config::KeySpec::One("ctrl+v".to_string()),
            )]
            .into_iter()
            .collect(),
        });
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let search = lines_of(KeysPreset::Search, &bound);
        assert!(
            search.iter().any(|line| line.contains("ctrl-v")
                && line.ends_with("paste the clipboard's image into the agent")),
            "{search:?}"
        );
    }

    /// The help of `taw app` as drawn, one string per line, in `preset`'s own keys.
    fn app_help_lines(preset: KeysPreset) -> Vec<String> {
        app_help_lines_with(preset, &crate::tui::keys::preset(preset))
    }

    /// [`app_help_lines`] with `keymap` as the keys.
    fn app_help_lines_with(preset: KeysPreset, keymap: &Keymap) -> Vec<String> {
        app_help_lines_going_back(preset, keymap, false)
    }

    /// [`app_help_lines_with`] for an app the doorway started when `back_to_tmux`.
    fn app_help_lines_going_back(
        preset: KeysPreset,
        keymap: &Keymap,
        back_to_tmux: bool,
    ) -> Vec<String> {
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let chrome = Chrome {
            theme: &theme,
            keymap,
            preset,
            indicators: StatusIndicators::Symbols,
            projects_in_filter: false,
            numbers_work: preset == KeysPreset::Modal,
            app: Some(AppLook {
                machines: true,
                back_to_tmux,
            }),
            letters_work: true,
            machines: false,
        };
        help_lines(&chrome)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// `taw app`'s help says what the three ways out do, and still fits: every line at most 98
    /// columns and the modal help at most 40 lines.
    #[test]
    fn the_app_help_says_go_in_quit_and_back_and_still_fits() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let lines = app_help_lines(preset);
            let text = lines.join("\n");
            assert!(text.contains("go in"), "{preset:?}: {text}");
            assert!(
                text.contains("back in where you came from"),
                "{preset:?}: {text}"
            );
            assert!(text.contains("quit at once"), "{preset:?}: {text}");
            for line in &lines {
                assert!(line.width() <= 98, "{preset:?} line too wide: {line}");
            }
        }
        let modal = app_help_lines(KeysPreset::Modal);
        assert!(modal.len() <= 40, "the help is {} lines", modal.len());
        let quit = modal.iter().find(|l| l.contains("quit at once")).unwrap();
        assert!(quit.contains("Q / Ctrl-C"), "{quit}");
        // Search: Q is text there, so only Ctrl-C is named for it.
        let search = app_help_lines(KeysPreset::Search);
        let quit = search.iter().find(|l| l.contains("quit at once")).unwrap();
        assert!(quit.contains("Ctrl-C") && !quit.contains("Q /"), "{quit}");
        // The popup's help is not touched.
        let popup = help_lines_for(KeysPreset::Modal).join("\n");
        assert!(!popup.contains("quit at once"), "{popup}");
    }

    /// The help names the keys that are bound: the quit keys for going back (the modal `q` too, and a `[keys]`
    /// entry's own), and a `Q` only while no `[keys]` entry took it.
    #[test]
    fn the_app_help_names_the_keys_that_go_back_and_quit() {
        let with = |name: &str, key: &str| {
            let mut cfg = crate::config::Config::default();
            cfg.keys.bindings.insert(
                name.to_string(),
                crate::config::KeySpec::One(key.to_string()),
            );
            crate::tui::keys::from_config(&cfg.keys).0
        };
        let back = |lines: &[String]| {
            lines
                .iter()
                .find(|l| l.contains("back in where you came from"))
                .cloned()
                .expect("a back line")
        };
        let lines = app_help_lines(KeysPreset::Modal);
        let line = back(&lines);
        assert!(
            line.contains("Esc / q"),
            "the modal keys are both named: {line}"
        );
        let search = app_help_lines(KeysPreset::Search);
        assert!(back(&search).trim_start().starts_with("Esc"), "{search:?}");

        // `quit` on ctrl-x: it is named, and Esc is nobody's key.
        let lines = app_help_lines_with(KeysPreset::Modal, &with("quit", "ctrl+x"));
        let line = back(&lines);
        assert!(line.contains("ctrl-x") && !line.contains("Esc"), "{line}");
        let quit = lines.iter().find(|l| l.contains("quit at once")).unwrap();
        assert!(quit.contains("Q / Ctrl-C"), "{quit}");

        // `help` on Q: Q opens the help, so it is not said to quit.
        let lines = app_help_lines_with(KeysPreset::Modal, &with("help", "shift+q"));
        let quit = lines.iter().find(|l| l.contains("quit at once")).unwrap();
        assert!(quit.contains("Ctrl-C") && !quit.contains("Q /"), "{quit}");
    }

    /// The help of an app the doorway started says that its quit keys go back to tmux, and an app of its own
    /// keeps saying it quits at once.
    #[test]
    fn the_help_of_an_app_the_doorway_started_says_q_goes_back_to_tmux() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let own = app_help_lines_going_back(preset, &keymap, false);
            assert!(own.iter().any(|l| l.contains("quit at once")), "{own:?}");
            assert!(!own.iter().any(|l| l.contains("back to tmux")), "{own:?}");
            let doorway = app_help_lines_going_back(preset, &keymap, true);
            let line = doorway
                .iter()
                .find(|l| l.contains("Ctrl-C") && l.contains(crate::tui::app::BACK_TO_TMUX))
                .unwrap_or_else(|| panic!("{preset:?}: {doorway:?}"));
            assert!(!line.contains("quit at once"), "{line}");
            assert!(
                !doorway.iter().any(|l| l.contains("quit at once")),
                "{doorway:?}"
            );
        }
    }

    /// Every word of the help that ends the app says `back to tmux` in an app the doorway started: the `Esc` line
    /// ("with none, quit"), `Q` and Ctrl-C, and the `q` of the options line; a plain `taw app` says quit in each.
    #[test]
    fn the_help_of_a_doorway_app_says_back_to_tmux_wherever_it_says_quit() {
        for preset in [KeysPreset::Modal, KeysPreset::Search] {
            let keymap = crate::tui::keys::preset(preset);
            let own = app_help_lines_going_back(preset, &keymap, false);
            let esc = own
                .iter()
                .find(|l| l.contains("with none, quit"))
                .unwrap_or_else(|| panic!("{preset:?}: the Esc line in {own:?}"));
            assert!(!esc.contains("back to tmux"), "{esc}");
            assert!(!own.iter().any(|l| l.contains("back to tmux")), "{own:?}");

            let doorway = app_help_lines_going_back(preset, &keymap, true);
            assert!(
                doorway
                    .iter()
                    .any(|l| l.contains("with none, back to tmux")),
                "{preset:?}: {doorway:?}"
            );
            let said_quit: Vec<&String> = doorway
                .iter()
                .filter(|l| l.contains("quit") && !l.contains("quit key"))
                .collect();
            assert!(said_quit.is_empty(), "{preset:?}: {said_quit:?}");
        }
    }

    /// The picker `n` opens is titled for this Mac in `taw app` when other machines exist, and
    /// keeps its own title in the popup.
    #[test]
    fn the_new_session_picker_says_this_mac_in_the_app() {
        let home = home_with_tree();
        let theme = theme::resolve("catppuccin", &BTreeMap::new()).theme;
        let keymap = crate::tui::keys::preset(KeysPreset::Modal);
        let picker = Overlay::Project(Box::new(project_picker(home.path(), "")));
        let draw = |app: Option<AppLook>| {
            let mut chrome = chrome(&theme, &keymap);
            chrome.app = app;
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut hits = OverlayHit::default();
            terminal
                .draw(|f| {
                    let area = f.area();
                    render(f, area, &chrome, &picker, &mut hits);
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            (0..buf.area().height)
                .map(|y| {
                    (0..buf.area().width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            draw(Some(AppLook {
                machines: true,
                back_to_tmux: false,
            }))
            .contains("new session on this Mac")
        );
        let popup = draw(None);
        assert!(
            popup.contains("new session") && !popup.contains("this Mac"),
            "{popup}"
        );
        // The app with no machine has only this Mac: today's title, with nothing to tell apart.
        let alone = draw(Some(AppLook {
            machines: false,
            back_to_tmux: false,
        }));
        assert!(
            alone.contains("new session") && !alone.contains("this Mac"),
            "{alone}"
        );
    }
}

#[cfg(test)]
mod size_tests {
    use super::box_size;

    #[test]
    fn box_size_raises_to_the_minimum_but_never_past_what_is_available() {
        assert_eq!(box_size(12, 20, 60), 20, "raised to the minimum");
        assert_eq!(box_size(40, 20, 60), 40, "what it asked for");
        assert_eq!(box_size(80, 20, 60), 60, "capped by the area");
        // The case `Ord::clamp` would abort on: less room than the minimum.
        assert_eq!(box_size(12, 20, 16), 16);
        assert_eq!(box_size(12, 20, 0), 0);
        assert_eq!(box_size(3, 3, 1), 1);
    }
}
