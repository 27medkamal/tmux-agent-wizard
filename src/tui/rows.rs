// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Row templates: `[ui.sidebar.*] rows` tokens rendered to styled ratatui lines.
//!
//! herdr's sidebar rows are user-configured lists of tokens, optionally per agent kind
//! (`rows_by_agent`) and optionally styled inline (`{ token, fg, bold, dim }`). Rendering
//! is deliberately the only thing this module does: a token that a row cannot supply
//! disappears from the line rather than printing a placeholder, which is what keeps a
//! herdr config with tokens we have no source for (workspace metadata, say) readable.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::config::{RowToken, RowsConfig, Sidebar, StatusIndicators, TokenStyle};
use crate::tui::data::{self, AgentRow, Snapshot, SpaceRow};
use crate::tui::theme::{self, Theme};
use crate::tui::tree;

/// What a row needs to know about its surroundings to render.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub theme: &'a Theme,
    pub indicators: StatusIndicators,
    /// The columns every row of the list lines up in ([`Columns`]).
    pub columns: &'a Columns,
}

/// The widest a token's column grows: a longer value pushes the rest of its own row alone.
const COLUMN_CAP: usize = 24;
/// The same for the agent's name, which is a word and stays narrower.
const AGENT_CAP: usize = 16;

/// How wide each token position of a row template is across the rows drawn from it, so that the
/// rows of one list line up in columns: the widest value in the position, up to a cap. Measured
/// over every row the list holds (not the ones in view), so scrolling moves nothing.
///
/// A template line is told by its address: rows drawn from one line of one template share its
/// columns, whatever machine they came from.
#[derive(Default)]
pub struct Columns {
    widths: HashMap<usize, Vec<usize>>,
}

impl Columns {
    /// Takes one row of `tokens` into account.
    pub fn measure(
        &mut self,
        tokens: &[RowToken],
        src: &dyn TokenSource,
        indicators: StatusIndicators,
    ) {
        let widths = self
            .widths
            .entry(tokens.as_ptr() as usize)
            .or_insert_with(|| vec![0; tokens.len()]);
        for (at, cell) in cells(tokens, src, indicators).iter().enumerate() {
            if let Some(part) = cell {
                let cap = if part.token.token == "agent" {
                    AGENT_CAP
                } else {
                    COLUMN_CAP
                };
                widths[at] = widths[at].max(part.text.width().min(cap));
            }
        }
    }

    fn of(&self, tokens: &[RowToken]) -> Option<&[usize]> {
        self.widths
            .get(&(tokens.as_ptr() as usize))
            .map(Vec::as_slice)
    }
}

/// A row of the sidebar, seen as a bag of named tokens.
pub trait TokenSource {
    /// The text for `name`, or `None` when this row has no such token (unknown name, or
    /// a known one with nothing to show).
    fn token_text(&self, name: &str, indicators: StatusIndicators) -> Option<String>;
    /// The token's default colour, before any inline `fg` override.
    fn token_color(&self, name: &str, theme: &Theme) -> Color;
}

/// Renders one configured row to a styled line: token texts, each span coloured by its inline
/// `fg` when set and by the row's own default otherwise, with `bold`/`dim` applied on top. Every
/// token but the last one shown is padded to its column ([`Columns`]) and followed by a space; the
/// padding is plain, and a token a rule hid (or that has no text) in the middle leaves its column
/// blank, so the tokens after it stay in line. A row ends in no padding. A token's `rules` are read
/// first: the first one its text meets lays its style over the token's own, or hides the token
/// (a row with none left is an empty line, which callers drop).
pub fn render_row(tokens: &[RowToken], src: &dyn TokenSource, ctx: &Ctx) -> Line<'static> {
    render_row_with(tokens, src, ctx, Lit::Quiet)
}

/// How a row is lit: quiet, as it is by default; hot, when it needs you; or open, when its session
/// has its window open.
#[derive(Clone, Copy, Default)]
pub enum Lit {
    #[default]
    Quiet,
    /// The state's style (its colour, bold while blocked), which every token takes.
    Hot(Style),
    /// The popup's own rows of the open window: every token that is dim on another row is white,
    /// the agent and its place bold; the state glyph keeps its state's colour.
    Open,
}

/// [`render_row`] for a row that is lit ([`Lit`]): the style applies to every token that has no
/// `fg` of its own and that none of its `rules` restyled. A token with its own `fg`, or one a rule
/// restyled, keeps what it was given.
pub fn render_row_with(
    tokens: &[RowToken],
    src: &dyn TokenSource,
    ctx: &Ctx,
    lit: Lit,
) -> Line<'static> {
    let cells = cells(tokens, src, ctx.indicators);
    let Some(last) = cells.iter().rposition(Option::is_some) else {
        return Line::default();
    };
    let columns = ctx.columns.of(tokens);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (at, cell) in cells.into_iter().enumerate().take(last + 1) {
        let wanted = columns.and_then(|c| c.get(at)).copied().unwrap_or(0);
        let Some(part) = cell else {
            // A column nothing on this row fills: blank, unless no row fills it either.
            if wanted > 0 {
                spans.push(Span::raw(" ".repeat(wanted + 1)));
            }
            continue;
        };
        let own = part.token.own_style();
        let unchanged = own.fg.is_none() && part.style == own;
        let lit_style = match lit {
            Lit::Hot(hot) if unchanged => Some(hot),
            Lit::Open if unchanged => open_style(&part.token.token, ctx.theme),
            _ => None,
        };
        let mut style = lit_style.unwrap_or_else(|| {
            Style::default().fg(part
                .style
                .fg
                .as_deref()
                .and_then(theme::parse_color)
                .unwrap_or_else(|| src.token_color(&part.token.token, ctx.theme)))
        });
        if part.style.bold == Some(true) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if part.style.dim == Some(true) {
            style = style.add_modifier(Modifier::DIM);
        }
        let pad = wanted.saturating_sub(part.text.width());
        spans.push(Span::styled(part.text, style));
        if at < last {
            spans.push(Span::raw(" ".repeat(pad + 1)));
        }
    }
    Line::from(spans)
}

/// What a token is drawn in on a row of the open window, as the popup's own columns are: the
/// agent and its place in `fg`, bold; the state word, the title and a session's name in `fg`.
/// `None` for a token the popup has no such column for (the glyph stays in its state's colour).
fn open_style(token: &str, theme: &Theme) -> Option<Style> {
    let fg = Style::default().fg(theme.fg);
    match token {
        "agent" | "tab" | "pane" => Some(fg.add_modifier(Modifier::BOLD)),
        "state_text" | "terminal_title" | "terminal_title_stripped" | "workspace" => Some(fg),
        _ => None,
    }
}

/// The plain text of one row — what the filter and the fuzzy pickers match against.
pub fn row_text(
    tokens: &[RowToken],
    src: &dyn TokenSource,
    indicators: StatusIndicators,
) -> String {
    cells(tokens, src, indicators)
        .into_iter()
        .flatten()
        .map(|part| part.text)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One token of a row that has something to show: its text, and what it is drawn with once its
/// rules have had their say.
struct Part<'t> {
    token: &'t RowToken,
    text: String,
    style: TokenStyle,
}

/// Each token of a row, in order: what it shows, or `None` when it has nothing to show. Rendering
/// and plain text must agree on which tokens are skipped, so both go through here: a token with no
/// text is skipped, and so is one a rule hides.
fn cells<'t>(
    tokens: &'t [RowToken],
    src: &dyn TokenSource,
    indicators: StatusIndicators,
) -> Vec<Option<Part<'t>>> {
    tokens
        .iter()
        .map(|token| {
            let text = src.token_text(&token.token, indicators)?;
            if text.is_empty() {
                return None;
            }
            let style = token.style_for(&text)?;
            Some(Part { token, text, style })
        })
        .collect()
}

/// The plain text of every row of one item, joined by spaces: the whole entry as the
/// filter sees it, so `/` matches text from any of an entry's lines.
pub fn item_text(
    rows: &[Vec<RowToken>],
    src: &dyn TokenSource,
    indicators: StatusIndicators,
) -> String {
    rows.iter()
        .map(|row| row_text(row, src, indicators))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The row template for an item: `rows_by_agent[<kind>]` when the agent's kind has an
/// override, else the panel's `rows`.
pub fn rows_for<'a>(cfg: &'a RowsConfig, kind: Option<&str>) -> &'a [Vec<RowToken>] {
    kind.and_then(|k| cfg.rows_by_agent.get(k))
        .unwrap_or(&cfg.rows)
}

/// The template a session header is drawn by, `None` while `[ui.sidebar.spaces] rows` is the
/// default (`defaults`, a [`Sidebar`] nothing was configured on) and the header draws its own way.
pub fn spaces_template<'a>(
    sidebar: &'a Sidebar,
    defaults: &Sidebar,
) -> Option<&'a [Vec<RowToken>]> {
    (sidebar.spaces.rows != defaults.spaces.rows).then_some(sidebar.spaces.rows.as_slice())
}

/// The template an agent's row is drawn by, `None` while the panel has no `rows` of its own and
/// no `rows_by_agent` (an agent of kind `kind` may have one).
pub fn agents_template<'a>(
    sidebar: &'a Sidebar,
    defaults: &Sidebar,
    kind: Option<&str>,
) -> Option<&'a [Vec<RowToken>]> {
    (sidebar.agents.rows != defaults.agents.rows || !sidebar.agents.rows_by_agent.is_empty())
        .then(|| rows_for(&sidebar.agents, kind))
}

/// The token that draws a state's glyph: a glyph says no word the filter could match (the state's
/// word is `state_text`'s).
const GLYPH_TOKEN: &str = "state_icon";

/// `template` without its glyph tokens ([`GLYPH_TOKEN`]).
fn without_glyphs(template: &[Vec<RowToken>]) -> Vec<Vec<RowToken>> {
    template
        .iter()
        .map(|row| {
            row.iter()
                .filter(|token| token.token != GLYPH_TOKEN)
                .cloned()
                .collect()
        })
        .collect()
}

/// The plain text of a row a configured template draws ([`item_text`], glyphs left out), `None` for a
/// row that is drawn its default way: the words only the template draws, which the filter finds the
/// row by ([`tree::build_drawn`]). A template that applies and draws nothing for a row (a token the
/// row has no value for) is `Some` of an empty text: the row is still the template's, and it draws no
/// word.
pub fn drawn_text(
    sidebar: &Sidebar,
    defaults: &Sidebar,
    indicators: StatusIndicators,
    snap: &Snapshot,
    row: &tree::Row,
) -> Option<String> {
    match row {
        tree::Row::Session(space) => snap
            .spaces
            .get(*space)
            .zip(spaces_template(sidebar, defaults))
            .map(|(space, template)| item_text(&without_glyphs(template), space, indicators)),
        tree::Row::Pane(pane) => snap
            .panes
            .get(*pane)
            .and_then(|pane| snap.agents.get(pane.agent?))
            .and_then(|agent| {
                let template = agents_template(sidebar, defaults, agent.kind.as_deref())?;
                Some(item_text(&without_glyphs(template), agent, indicators))
            }),
        _ => None,
    }
}

impl TokenSource for AgentRow {
    fn token_text(&self, name: &str, indicators: StatusIndicators) -> Option<String> {
        let text = match name {
            "state_icon" => data::state_icon(self.state, indicators).to_string(),
            "state_text" => self.state?.as_str().to_string(),
            "workspace" => self.session_name.clone(),
            // herdr's `tab` is "which window"; taw answers it the way tmux spells a
            // target, and the way every fixed-column row in the tree already does.
            "tab" => tree::target(
                self.window_index,
                &self.window_name,
                self.window_panes,
                self.pane_index,
            ),
            "pane" => self.pane_index.to_string(),
            "agent" => self.agent_label(),
            "terminal_title" => self.pane_title.clone(),
            "terminal_title_stripped" => data::strip_title_glyph(&self.pane_title),
            "branch" | "git_status" => return None, // workspace-only tokens
            custom => self.tokens.get(custom.strip_prefix('$')?)?.clone(),
        };
        (!text.is_empty()).then_some(text)
    }

    fn token_color(&self, name: &str, theme: &Theme) -> Color {
        match name {
            "state_icon" | "state_text" => theme.state_color(self.state),
            "agent" => theme.accent,
            "tab" | "pane" | "terminal_title" | "terminal_title_stripped" => theme.dim,
            _ => theme.fg,
        }
    }
}

impl TokenSource for SpaceRow {
    fn token_text(&self, name: &str, indicators: StatusIndicators) -> Option<String> {
        let text = match name {
            "state_icon" => data::state_icon(self.state, indicators).to_string(),
            "state_text" => self.state?.as_str().to_string(),
            "workspace" => self.session_name.clone(),
            "branch" => self.git.as_ref()?.branch.clone()?,
            "git_status" => data::git_status_text(self.git.as_ref()?),
            // A workspace has no `@agent_token_*` options and taw keeps no workspace
            // metadata store, so herdr's `$name` workspace tokens have no source here.
            _ => return None,
        };
        (!text.is_empty()).then_some(text)
    }

    fn token_color(&self, name: &str, theme: &Theme) -> Color {
        match name {
            "state_icon" | "state_text" => theme.state_color(self.state),
            "workspace" => theme.fg,
            _ => theme.dim,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::store::AgentState;

    use crate::config::Config;
    use crate::git;

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn tokens(names: &[&str]) -> Vec<RowToken> {
        let toml = format!(
            "[ui.sidebar.agents]\nrows = [[{}]]\n",
            names
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        toml::from_str::<Config>(&toml)
            .unwrap()
            .ui
            .sidebar
            .agents
            .rows[0]
            .clone()
    }

    fn agent() -> AgentRow {
        AgentRow {
            pane_id: "%7".to_string(),
            session_name: "web".to_string(),
            window_index: 3,
            window_name: "zsh".to_string(),
            pane_current_command: "zsh".to_string(),
            pane_index: 1,
            pane_title: "\u{25cf} claude review".to_string(),
            kind: Some("claude".to_string()),
            state: Some(AgentState::Blocked),
            ..Default::default()
        }
    }

    fn ctx<'a>(theme: &'a Theme, indicators: StatusIndicators) -> Ctx<'a> {
        // No columns measured: every row is as wide as its own tokens.
        Ctx {
            theme,
            indicators,
            columns: Box::leak(Box::default()),
        }
    }

    fn theme() -> Theme {
        theme::resolve("catppuccin", &BTreeMap::new()).theme
    }

    #[test]
    fn render_row_joins_state_icon_workspace_and_tab() {
        let t = theme();
        let line = render_row(
            &tokens(&["state_icon", "workspace", "tab"]),
            &agent(),
            &ctx(&t, StatusIndicators::Symbols),
        );
        assert_eq!(plain(&line), "\u{23f8} web 3:zsh");
    }

    /// The `tab` token spells a pane the way tmux does, and the way every fixed-column
    /// row of the tree does: the pane index only once the window is split.
    #[test]
    fn the_tab_token_is_tmuxs_own_target_spelling() {
        let mut row = agent();
        row.window_panes = 1;
        assert_eq!(
            row_text(&tokens(&["tab"]), &row, StatusIndicators::Dots),
            "3:zsh"
        );
        row.window_panes = 2;
        row.pane_index = 2;
        assert_eq!(
            row_text(&tokens(&["tab"]), &row, StatusIndicators::Dots),
            "3:zsh.2"
        );
    }

    #[test]
    fn render_row_colours_the_state_icon_by_state_and_the_rest_by_its_default() {
        let t = theme();
        let line = render_row(
            &tokens(&["state_icon", "workspace"]),
            &agent(),
            &ctx(&t, StatusIndicators::Dots),
        );
        let styled: Vec<(String, Option<Color>)> = line
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert_eq!(styled[0], ("\u{25cf}".to_string(), Some(t.attention)));
        assert_eq!(styled[2], ("web".to_string(), Some(t.fg)));
    }

    #[test]
    fn render_row_applies_inline_fg_bold_and_dim() {
        let t = theme();
        let cfg: Config = toml::from_str(
            "[ui.sidebar.agents]\nrows = [[{ token = \"workspace\", fg = \"#ff0000\", bold = true }, { token = \"agent\", dim = true }]]\n",
        )
        .unwrap();
        let line = render_row(
            &cfg.ui.sidebar.agents.rows[0],
            &agent(),
            &ctx(&t, StatusIndicators::Dots),
        );
        assert_eq!(plain(&line), "web claude");
        assert_eq!(line.spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(line.spans[2].style.add_modifier.contains(Modifier::DIM));
        assert!(!line.spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_custom_dollar_token_comes_from_the_agent_record_tokens() {
        let mut row = agent();
        row.tokens
            .insert("jj_status".to_string(), "3 files".to_string());
        assert_eq!(
            row_text(&tokens(&["$jj_status"]), &row, StatusIndicators::Dots),
            "3 files"
        );
        // An unset custom token disappears rather than printing an empty gap.
        assert_eq!(
            row_text(
                &tokens(&["$nope", "workspace"]),
                &row,
                StatusIndicators::Dots
            ),
            "web"
        );
    }

    #[test]
    fn unknown_and_empty_tokens_are_skipped_entirely() {
        let mut row = agent();
        row.pane_title = String::new();
        let text = row_text(
            &tokens(&["terminal_title", "not_a_token", "workspace"]),
            &row,
            StatusIndicators::Dots,
        );
        assert_eq!(text, "web");
    }

    #[test]
    fn agent_rows_render_titles_raw_and_stripped() {
        let row = agent();
        assert_eq!(
            row_text(&tokens(&["terminal_title"]), &row, StatusIndicators::Dots),
            "\u{25cf} claude review"
        );
        assert_eq!(
            row_text(
                &tokens(&["terminal_title_stripped"]),
                &row,
                StatusIndicators::Dots
            ),
            "claude review"
        );
    }

    #[test]
    fn agent_rows_have_no_branch_or_git_status_tokens() {
        let row = agent();
        assert_eq!(
            row_text(
                &tokens(&["branch", "git_status", "pane"]),
                &row,
                StatusIndicators::Dots
            ),
            "1"
        );
    }

    #[test]
    fn space_rows_render_the_default_template() {
        let t = theme();
        let cfg = Config::default();
        let space = SpaceRow {
            session_name: "web".to_string(),
            state: Some(AgentState::Working),
            git: Some(git::Info {
                branch: Some("master".to_string()),
                ahead: 1,
                behind: 0,
                modified: 2,
                untracked: 0,
            }),
            ..Default::default()
        };
        let rows = rows_for(&cfg.ui.sidebar.spaces, None);
        assert_eq!(
            plain(&render_row(
                &rows[0],
                &space,
                &ctx(&t, StatusIndicators::Symbols)
            )),
            "\u{23f5} web"
        );
        assert_eq!(
            plain(&render_row(
                &rows[1],
                &space,
                &ctx(&t, StatusIndicators::Symbols)
            )),
            "master \u{b1}2 \u{2191}1"
        );
    }

    #[test]
    fn a_space_with_no_agents_and_no_repository_shows_a_dim_dot_only() {
        let t = theme();
        let cfg = Config::default();
        let space = SpaceRow {
            session_name: "scratch".to_string(),
            ..Default::default()
        };
        let rows = rows_for(&cfg.ui.sidebar.spaces, None);
        assert_eq!(
            plain(&render_row(
                &rows[0],
                &space,
                &ctx(&t, StatusIndicators::Dots)
            )),
            "\u{b7} scratch"
        );
        assert_eq!(
            plain(&render_row(
                &rows[1],
                &space,
                &ctx(&t, StatusIndicators::Dots)
            )),
            ""
        );
        assert_eq!(
            render_row(&rows[0], &space, &ctx(&t, StatusIndicators::Dots)).spans[0]
                .style
                .fg,
            Some(t.dim)
        );
    }

    #[test]
    fn rows_by_agent_overrides_the_template_for_that_kind_only() {
        let cfg: Config =
            toml::from_str("[ui.sidebar.agents.rows_by_agent]\nclaude = [[\"agent\", \"pane\"]]\n")
                .unwrap();
        let agents = &cfg.ui.sidebar.agents;
        assert_eq!(rows_for(agents, Some("claude")).len(), 1);
        assert_eq!(
            row_text(
                &rows_for(agents, Some("claude"))[0],
                &agent(),
                StatusIndicators::Dots
            ),
            "claude 1"
        );
        // A different kind keeps the default two-row template.
        assert_eq!(rows_for(agents, Some("codex")), agents.rows.as_slice());
        assert_eq!(rows_for(agents, None), agents.rows.as_slice());
    }

    #[test]
    fn item_text_joins_every_row_of_an_entry_for_the_filter() {
        let cfg = Config::default();
        let text = item_text(
            &cfg.ui.sidebar.agents.rows,
            &agent(),
            StatusIndicators::Dots,
        );
        assert_eq!(text, "\u{25cf} web 3:zsh claude");
    }

    // ------------------------------------------------------------------ token rules

    /// Rows from a config's `[ui.sidebar.agents] rows`, the way a user writes them.
    fn rows_of(toml_rows: &str) -> Vec<Vec<RowToken>> {
        toml::from_str::<Config>(&format!("[ui.sidebar.agents]\nrows = {toml_rows}\n"))
            .unwrap()
            .ui
            .sidebar
            .agents
            .rows
    }

    /// An agent row reporting `$name = value` for each pair.
    fn agent_with(tokens: &[(&str, &str)]) -> AgentRow {
        let mut row = agent();
        for (name, value) in tokens {
            row.tokens.insert(name.to_string(), value.to_string());
        }
        row
    }

    /// herdr's own example (`configuration.mdx`): a custom `$load` token reporting `90`
    /// selects red and bold, `60` yellow, and `90%` keeps white.
    #[test]
    fn a_custom_token_reporting_90_renders_red_bold_and_60_yellow_and_90_percent_plain() {
        let t = theme();
        let rows = rows_of(
            r##"[[{ token = "$load", fg = "#fff", rules = [{ gt = 80, fg = "#f55", bold = true }, { gt = 50, fg = "#fc0" }] }]]"##,
        );
        let style_of = |value: &str| {
            let line = render_row(
                &rows[0],
                &agent_with(&[("load", value)]),
                &ctx(&t, StatusIndicators::Dots),
            );
            assert_eq!(plain(&line), value);
            let span = &line.spans[0];
            (
                span.style.fg,
                span.style.add_modifier.contains(Modifier::BOLD),
            )
        };

        assert_eq!(style_of("90"), (Some(Color::Rgb(255, 85, 85)), true));
        assert_eq!(style_of("60"), (Some(Color::Rgb(255, 204, 0)), false));
        assert_eq!(style_of("90%"), (Some(Color::Rgb(255, 255, 255)), false));
        assert_eq!(style_of("10"), (Some(Color::Rgb(255, 255, 255)), false));
    }

    /// The first matching rule wins, and a rule says only what it means to change: what it
    /// leaves out is the token's own.
    #[test]
    fn the_first_matching_rule_wins_and_what_it_leaves_out_is_the_tokens_own() {
        let t = theme();
        let rows = rows_of(
            r##"[[{ token = "$load", fg = "#00f", bold = true, dim = true, rules = [{ gt = 80, fg = "#f00" }, { gt = 50, fg = "#0f0", bold = false }] }]]"##,
        );
        let span = |value: &str| {
            render_row(
                &rows[0],
                &agent_with(&[("load", value)]),
                &ctx(&t, StatusIndicators::Dots),
            )
            .spans[0]
                .style
        };

        let high = span("90");
        assert_eq!(high.fg, Some(Color::Rgb(255, 0, 0)));
        assert!(
            high.add_modifier.contains(Modifier::BOLD),
            "bold is the token's"
        );
        assert!(
            high.add_modifier.contains(Modifier::DIM),
            "dim is the token's"
        );
        let mid = span("60");
        assert_eq!(mid.fg, Some(Color::Rgb(0, 255, 0)));
        assert!(
            !mid.add_modifier.contains(Modifier::BOLD),
            "the rule turned bold off"
        );
        assert!(mid.add_modifier.contains(Modifier::DIM));
        let low = span("10");
        assert_eq!(
            low.fg,
            Some(Color::Rgb(0, 0, 255)),
            "no rule: the token's own"
        );
        assert!(low.add_modifier.contains(Modifier::BOLD));
    }

    /// `hide = true` removes the token and the separator that went with it.
    #[test]
    fn a_token_hidden_by_a_rule_goes_with_its_separator() {
        let t = theme();
        let rows = rows_of(
            r##"[["workspace", { token = "$machine", rules = [{ equals = "Local", hide = true }] }, "agent"]]"##,
        );
        let line = |value: &str| {
            render_row(
                &rows[0],
                &agent_with(&[("machine", value)]),
                &ctx(&t, StatusIndicators::Dots),
            )
        };

        assert_eq!(plain(&line("Local")), "web claude");
        assert_eq!(line("Local").spans.len(), 3, "two tokens and one separator");
        assert_eq!(plain(&line("devbox")), "web devbox claude");
        assert_eq!(plain(&line("localhost")), "web localhost claude");
        // With no value at all the token is gone anyway, rules or not.
        assert_eq!(
            plain(&render_row(
                &rows[0],
                &agent(),
                &ctx(&t, StatusIndicators::Dots)
            )),
            "web claude"
        );
    }

    /// A row left with no tokens draws nothing, and the filter's text for the entry agrees
    /// with what is drawn.
    #[test]
    fn a_row_with_every_token_hidden_draws_nothing_and_matches_nothing() {
        let t = theme();
        let rows = rows_of(
            r##"[["workspace"], [{ token = "$machine", rules = [{ equals = "Local", hide = true }] }]]"##,
        );
        let local = agent_with(&[("machine", "Local")]);

        let line = render_row(&rows[1], &local, &ctx(&t, StatusIndicators::Dots));

        assert!(line.spans.is_empty(), "{line:?}");
        assert_eq!(row_text(&rows[1], &local, StatusIndicators::Dots), "");
        assert_eq!(
            item_text(&rows, &local, StatusIndicators::Dots),
            "web",
            "the filter does not match text that is not shown"
        );
        assert_eq!(
            item_text(
                &rows,
                &agent_with(&[("machine", "devbox")]),
                StatusIndicators::Dots
            ),
            "web devbox"
        );
    }

    /// A template's matched text leaves out its glyph tokens: the state icon is drawn but is no
    /// word, so the glyph finds nothing (its word, `state_text`, does).
    #[test]
    fn a_templates_matched_text_leaves_out_the_state_icon() {
        let cfg: Config = toml::from_str(
            "[ui.sidebar.agents]\nrows = [[\"state_icon\", \"agent\", \"state_text\"]]\n",
        )
        .unwrap();
        let snap = crate::tui::fixtures::snapshot();
        let at = snap
            .panes
            .iter()
            .position(|p| p.agent.is_some())
            .expect("an agent pane");
        let agent = &snap.agents[snap.panes[at].agent.unwrap()];
        let glyph = data::state_icon(agent.state, StatusIndicators::Dots).to_string();
        let template = &cfg.ui.sidebar.agents.rows;
        assert!(
            item_text(template, agent, StatusIndicators::Dots).contains(&glyph),
            "the drawn row has the glyph"
        );

        let text = drawn_text(
            &cfg.ui.sidebar,
            &Sidebar::default(),
            StatusIndicators::Dots,
            &snap,
            &tree::Row::Pane(at),
        )
        .expect("the template draws the row");

        assert!(
            text.contains(&agent.agent_label()),
            "its words stay: {text:?}"
        );
        assert!(!text.contains(&glyph), "the glyph is not matched: {text:?}");
    }

    /// A rule matches the token's whole text, before anything is cut to fit, and a token a row
    /// cannot supply is not matched at all.
    #[test]
    fn a_rule_reads_the_built_in_tokens_text_too() {
        let t = theme();
        let rows = rows_of(
            r##"[[{ token = "agent", rules = [{ equals = "claude", fg = "#0f0" }] }, { token = "state_text", rules = [{ equals = "blocked", bold = true }] }, { token = "tab", rules = [{ starts_with = "3:", dim = true }] }]]"##,
        );

        let line = render_row(&rows[0], &agent(), &ctx(&t, StatusIndicators::Dots));

        assert_eq!(plain(&line), "claude blocked 3:zsh");
        assert_eq!(line.spans[0].style.fg, Some(Color::Rgb(0, 255, 0)));
        assert!(line.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert!(line.spans[4].style.add_modifier.contains(Modifier::DIM));
    }

    /// Rules work on a session header's rows too (`[ui.sidebar.spaces]`).
    #[test]
    fn a_space_row_takes_rules_as_well() {
        let t = theme();
        let cfg: Config = toml::from_str(
            r#"[ui.sidebar.spaces]
rows = [["workspace", { token = "branch", rules = [{ equals = "master", hide = true }] }]]
"#,
        )
        .unwrap();
        let rows = rows_for(&cfg.ui.sidebar.spaces, None);
        let on = |branch: &str| SpaceRow {
            session_name: "web".to_string(),
            git: Some(git::Info {
                branch: Some(branch.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let line =
            |branch: &str| render_row(&rows[0], &on(branch), &ctx(&t, StatusIndicators::Dots));

        assert_eq!(plain(&line("master")), "web");
        assert_eq!(plain(&line("feat/x")), "web feat/x");
    }
}
