//! The Agents view projection: herdr's `agent.view.set` query, kept in a tmux option.
//!
//! herdr's socket API installs one transient declarative query that the built-in Agents
//! view is projected through (`agent.view.set`, cleared by `agent.view.clear`). taw has
//! no socket, so the query lives where all of taw's state lives — in tmux. The global
//! option [`OPTION`] holds the exact `agent.view.set` params JSON; unset means no
//! projection, and the popup shows every agent.
//!
//! What a view projects is deliberately narrow, and it is herdr's own boundary: the
//! Agents panel's rows and the goto picker's agent rows. `taw agent list`, notifications
//! and detection all keep seeing every agent — a view is a way of looking at the panel,
//! not a filter on the world.
//!
//! The query and its evaluation order are herdr's
//! (`third_party/herdr/reference/socket-api-schema.json`, and the "Agent view queries"
//! section of `socket-api.mdx`), so a query written for herdr means the same thing here.
//! Its *limits* belong to the writer, `taw agent view set`, exactly as herdr puts them in
//! `agent.view.set` rather than in the renderer — see [`validate`].

use std::cmp::Ordering;

use serde::Deserialize;

use crate::store::AgentState;
use crate::tmux::ids;
use crate::tui::data::AgentRow;

/// The global tmux option holding the active `agent.view.set` params JSON.
pub const OPTION: &str = "@agent_wizard_view";

/// What the panel title says when a view carries no `label` of its own — herdr's word.
pub const DEFAULT_LABEL: &str = "filtered";

/// How deeply a filter may nest. Evaluation recurses, so this is a stack guard, and it
/// sits far above the 8 levels herdr allows and `taw agent view set` enforces — a reader
/// must never refuse a query the writer accepted. (`serde_json` already caps nesting at
/// 128 and errors rather than overflowing; this only makes the message say what broke.)
const MAX_FILTER_DEPTH: usize = 64;

/// How much of a `label` the panel's border chip will show.
const MAX_LABEL_CHARS: usize = 32;

/// One installed projection: herdr's `agent.view.set` params.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct AgentView {
    /// Who installed it. Only ever reported, never enforced here: ownership is what
    /// `agent view clear --source` checks, and this half of the feature only draws.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub filter: Option<Filter>,
    /// When non-empty this replaces `[ui] agent_panel_sort` for as long as the view
    /// lasts, without rewriting the config — herdr's rule.
    #[serde(default)]
    pub sort: Vec<Sort>,
}

/// One filter node. `all`/`any`/`not` compose; the rest test one field.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Filter {
    All { filters: Vec<Filter> },
    Any { filters: Vec<Filter> },
    Not { filter: Box<Filter> },
    Eq { field: Field, value: Value },
    In { field: Field, values: Vec<Value> },
    Exists { field: Field },
}

/// A built-in field, or one plugin-reported `@agent_token_<name>`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Field {
    Builtin(BuiltinField),
    Token { token: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinField {
    Status,
    WorkspaceId,
    TabId,
    PaneId,
    Agent,
    Seen,
    StateChangeSeq,
}

/// What a field is compared against.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    Bool(bool),
    Number(u64),
    Context { context: Context },
}

/// A value taken from what the popup is looking at, rather than written into the query.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    CurrentWorkspaceId,
    CurrentTabId,
}

/// One sort key. Sorts are stable and evaluated in order.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Sort {
    pub field: SortField,
    #[serde(default)]
    pub order: Order,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SortField {
    Builtin(BuiltinSortField),
    Token { token: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinSortField {
    WorkspaceOrder,
    TabOrder,
    PaneOrder,
    Attention,
    Status,
    Agent,
    Seen,
    StateChangeSeq,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

/// What a `{"context": …}` value resolves against: the workspace and tab the popup was
/// opened from, as public ids (`w3`, `w3:t7`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Current {
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
}

/// One field's value for one agent. The variant order is the cross-type sort order, so
/// a token that holds text sorts before one that holds a number — herdr's, so a mixed
/// sort lands the same way in both.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Eval {
    Text(String),
    Bool(bool),
    Number(u64),
}

/// Parses the value of [`OPTION`].
///
/// `Ok(None)` is "no projection": an unset or blank option. `Err` is a query that will
/// not be applied — JSON that does not parse, or one that breaks a herdr limit. The
/// caller shows every agent and says so once, rather than showing a filtered panel that
/// nobody asked for or an empty one nobody can explain.
pub fn parse(raw: Option<&str>) -> Result<Option<AgentView>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let mut view: AgentView =
        serde_json::from_str(raw).map_err(|e| first_line(&e.to_string()).to_string())?;
    validate(&mut view)?;
    Ok(Some(view))
}

/// serde's messages carry an "at line 1 column 42" tail that means nothing for a value
/// read out of a tmux option, and the popup has one line to say this on.
fn first_line(message: &str) -> &str {
    message
        .split_once(" at line ")
        .map_or(message, |(head, _)| head)
}

impl AgentView {
    /// The chip the panel title shows.
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(DEFAULT_LABEL)
    }

    /// Whether one agent belongs in the projected view. A view with no `filter` keeps
    /// every agent — it may be installed only to reorder them.
    pub fn matches(&self, row: &AgentRow, current: &Current) -> bool {
        match &self.filter {
            Some(filter) => matches(filter, row, current),
            None => true,
        }
    }

    /// Reorders `indices` by this view's sort keys, stably — so agents the query does
    /// not distinguish stay in the order they came in. A no-op when it has no `sort`,
    /// which leaves `[ui] agent_panel_sort` in charge.
    pub fn sort(&self, rows: &[AgentRow], indices: &mut [usize]) {
        if self.sort.is_empty() {
            return;
        }
        indices.sort_by(|&left, &right| {
            for sort in &self.sort {
                let ordering = compare(
                    sort_value(&rows[left], &sort.field),
                    sort_value(&rows[right], &sort.field),
                    sort.order,
                );
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
    }

    /// Whether the query reads a `@agent_token_<name>`, which the snapshot only pays to
    /// read when something actually asks for it.
    pub fn uses_tokens(&self) -> bool {
        self.filter.as_ref().is_some_and(filter_uses_tokens)
            || self
                .sort
                .iter()
                .any(|sort| matches!(sort.field, SortField::Token { .. }))
    }
}

fn filter_uses_tokens(filter: &Filter) -> bool {
    match filter {
        Filter::All { filters } | Filter::Any { filters } => filters.iter().any(filter_uses_tokens),
        Filter::Not { filter } => filter_uses_tokens(filter),
        Filter::Eq { field, .. } | Filter::In { field, .. } | Filter::Exists { field } => {
            matches!(field, Field::Token { .. })
        }
    }
}

fn matches(filter: &Filter, row: &AgentRow, current: &Current) -> bool {
    match filter {
        Filter::All { filters } => filters.iter().all(|f| matches(f, row, current)),
        Filter::Any { filters } => filters.iter().any(|f| matches(f, row, current)),
        Filter::Not { filter } => !matches(filter, row, current),
        Filter::Eq { field, value } => field_value(row, field) == operand(value, current),
        Filter::In { field, values } => {
            let actual = field_value(row, field);
            values.iter().any(|value| actual == operand(value, current))
        }
        Filter::Exists { field } => field_value(row, field).is_some(),
    }
}

/// A missing value sorts after a present one whichever way the sort runs — herdr's
/// "missing values stay after present values", which is why the order is only applied
/// to the both-present case.
fn compare(left: Option<Eval>, right: Option<Eval>, order: Order) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match order {
            Order::Asc => left.cmp(&right),
            Order::Desc => left.cmp(&right).reverse(),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn field_value(row: &AgentRow, field: &Field) -> Option<Eval> {
    match field {
        Field::Token { token } => row.tokens.get(token).cloned().map(Eval::Text),
        Field::Builtin(BuiltinField::Status) => Some(Eval::Text(status(row))),
        Field::Builtin(BuiltinField::WorkspaceId) => {
            Some(Eval::Text(ids::workspace_id(&row.session_id)))
        }
        Field::Builtin(BuiltinField::TabId) => {
            Some(Eval::Text(ids::tab_id(&row.session_id, &row.window_id)))
        }
        Field::Builtin(BuiltinField::PaneId) => {
            Some(Eval::Text(ids::pane_id(&row.session_id, &row.pane_id)))
        }
        Field::Builtin(BuiltinField::Agent) => row.kind.clone().map(Eval::Text),
        Field::Builtin(BuiltinField::Seen) => Some(Eval::Bool(row.seen)),
        Field::Builtin(BuiltinField::StateChangeSeq) => state_change_seq(row).map(Eval::Number),
    }
}

fn sort_value(row: &AgentRow, field: &SortField) -> Option<Eval> {
    match field {
        SortField::Token { token } => row.tokens.get(token).cloned().map(Eval::Text),
        SortField::Builtin(field) => match field {
            BuiltinSortField::WorkspaceOrder => Some(Eval::Number(row.space_order as u64)),
            BuiltinSortField::TabOrder => Some(Eval::Number(u64::from(row.window_index))),
            BuiltinSortField::PaneOrder => Some(Eval::Number(u64::from(row.pane_index))),
            BuiltinSortField::Attention => Some(Eval::Number(u64::from(attention(row.state)))),
            BuiltinSortField::Status => Some(Eval::Text(status(row))),
            BuiltinSortField::Agent => row.kind.clone().map(Eval::Text),
            BuiltinSortField::Seen => Some(Eval::Bool(row.seen)),
            BuiltinSortField::StateChangeSeq => state_change_seq(row).map(Eval::Number),
        },
    }
}

fn operand(value: &Value, current: &Current) -> Option<Eval> {
    match value {
        Value::Text(text) => Some(Eval::Text(text.clone())),
        Value::Bool(flag) => Some(Eval::Bool(*flag)),
        Value::Number(number) => Some(Eval::Number(*number)),
        Value::Context {
            context: Context::CurrentWorkspaceId,
        } => current.workspace_id.clone().map(Eval::Text),
        Value::Context {
            context: Context::CurrentTabId,
        } => current.tab_id.clone().map(Eval::Text),
    }
}

/// The effective status: taw records herdr's `done` (finished unseen) as a state of its
/// own, so this is just the recorded state, with a pane that has none reading `unknown`.
fn status(row: &AgentRow) -> String {
    row.state.map_or("unknown", AgentState::as_str).to_string()
}

/// How much this agent wants the user, herdr's `tab_attention_priority`: blocked above
/// finished-unseen above working above idle above unknown.
fn attention(state: Option<AgentState>) -> u8 {
    match state {
        Some(AgentState::Blocked) => 4,
        Some(AgentState::Done) => 3,
        Some(AgentState::Working) => 2,
        Some(AgentState::Idle) => 1,
        Some(AgentState::Unknown) | None => 0,
    }
}

/// herdr's monotonic per-pane state counter, which taw keeps as `@agent_state_since`
/// (unix seconds). `0` is "no transition recorded", which is a missing value, not a
/// transition at the epoch.
fn state_change_seq(row: &AgentRow) -> Option<u64> {
    (row.state_since != 0).then_some(row.state_since)
}

/// Checks the one thing that would break the popup rather than merely look odd, and
/// tidies the label.
///
/// Everything else herdr refuses at `agent.view.set` time is *evaluated* here instead of
/// refused. That is deliberate, and it is herdr's own split: herdr validates in the API
/// (`validate_agent_view`) and its renderer (`apply_agent_view`) then evaluates whatever
/// survived, with no checks of its own. `taw agent view set` is this project's API
/// boundary, so it owns those rules; a reader that refused what the writer accepted would
/// blank the panel and blame the user for a query the CLI told them was fine.
///
/// So an empty `all` matches everything and an empty `any` or `in` matches nothing, an
/// unknown status simply never equals a real one, a context compared to the wrong id
/// field never matches, a mistyped value never matches, and any token name is just a
/// lookup that finds nothing. Each is a query that shows the wrong agents, which the
/// label chip makes visible — not one that hides the panel.
fn validate(view: &mut AgentView) -> Result<(), String> {
    view.label = view
        .label
        .take()
        .map(|label| clean_label(&label))
        .filter(|label| !label.is_empty());
    match &view.filter {
        Some(filter) => check_depth(filter, 1),
        None => Ok(()),
    }
}

/// A label goes on one border line, so control characters and unbounded length are the
/// two things worth taking out of it. An empty result is dropped by the caller, which
/// falls back to [`DEFAULT_LABEL`].
fn clean_label(label: &str) -> String {
    label
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect()
}

fn check_depth(filter: &Filter, depth: usize) -> Result<(), String> {
    if depth > MAX_FILTER_DEPTH {
        return Err(format!(
            "filter nests deeper than {MAX_FILTER_DEPTH} levels"
        ));
    }
    match filter {
        Filter::All { filters } | Filter::Any { filters } => filters
            .iter()
            .try_for_each(|filter| check_depth(filter, depth + 1)),
        Filter::Not { filter } => check_depth(filter, depth + 1),
        Filter::Eq { .. } | Filter::In { .. } | Filter::Exists { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests;
