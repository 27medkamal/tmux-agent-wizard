// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! `config.toml` model, defaults, and `taw config check` diagnostics.
//!
//! Mirrors herdr's config surface where the semantics carry over to tmux.
//! Keys herdr has that tmux cannot honour (or that tmux itself owns) are still
//! accepted without warning — see the header of `config_default.toml` — but have
//! no effect. Keys neither honoured nor explicitly accepted produce a `Warning`
//! diagnostic from [`check`]; invalid values (bad enum strings, malformed TOML)
//! produce an `Error`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::paths::Paths;

mod rules;
pub use rules::{TokenRule, TokenStyle};

/// The commented default config file, printed by `taw config default`.
pub const DEFAULT_TOML: &str = include_str!("config_default.toml");

/// Severity of a single [`Diagnostic`] from [`check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// One finding from `taw config check`: a parse error, an unknown key, or an
/// invalid value, each with a human-readable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Error,
            message: message.into(),
        }
    }

    fn warning(message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Warning,
            message: message.into(),
        }
    }
}

/// Root of `config.toml`. Every field falls back to its documented default when
/// absent, at any level of nesting (missing tables and missing keys within a
/// present table both resolve the same way).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub update: Update,
    pub ui: Ui,
    pub session: Session,
    pub worktrees: Worktrees,
    pub plugins: Plugins,
    pub experimental: Experimental,
    /// The popup's own bindings: a preset plus per-action overrides. tmux-level
    /// bindings (opening the popup, jumping to a notification target) still come
    /// from `@agent-wizard*` tmux options, not this table.
    pub keys: Keys,
    pub sessions: Sessions,
    /// `[hooks]`: the user's own commands on taw's events, and the two that can refuse.
    pub hooks: Hooks,
    /// `[sandbox]`: the profile `taw sandbox` runs when you name none.
    pub sandbox: Sandbox,
    pub detection: Detection,
    /// Raw passthrough, accepted and ignored: tmux owns shells and pane creation.
    pub terminal: toml::Table,
    pub onboarding: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub name: String,
    pub auto_switch: bool,
    /// The theme drawn on a dark terminal with `auto_switch` on; unset takes `name`'s dark sibling
    /// ([`crate::tui::theme::name_for`]).
    pub dark_name: Option<String>,
    /// The theme drawn on a light terminal with `auto_switch` on; unset takes `name`'s light sibling.
    pub light_name: Option<String>,
    #[serde(deserialize_with = "deserialize_colour_tokens")]
    pub custom: BTreeMap<String, String>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "catppuccin".to_string(),
            auto_switch: false,
            dark_name: None,
            light_name: None,
            custom: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Update {
    pub manifest_check: bool,
    pub channel: String,
    pub version_check: bool,
}

impl Default for Update {
    fn default() -> Self {
        Update {
            manifest_check: true,
            channel: "stable".to_string(),
            version_check: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub sidebar_width: u16,
    pub agent_panel_sort: AgentPanelSort,
    pub status_indicators: StatusIndicators,
    pub confirm_close: bool,
    pub show_agent_labels_on_pane_borders: bool,
    /// How far the popup's tree expands when it opens.
    pub depth: Depth,
    /// Number the sessions 1–9 in the tree and bind `[keys] select_session` to them.
    pub labels: bool,
    /// Show each agent's pane title (Claude Code writes its task there) on its row.
    pub titles: bool,
    /// Tree (grouped, with headers) or flat (a single ordered list) at open.
    pub layout: Layout,
    /// Show the preview pane (auto-off under 100 columns regardless).
    pub preview: bool,
    /// Show branch and git-status flags on session headers.
    pub show_git: bool,
    /// Also list, under the sessions, the directories `n` would offer while the `/` filter
    /// is open; Enter on one opens or creates its session. Off unless asked for.
    pub projects_in_filter: bool,
    pub sidebar: Sidebar,
    pub toast: Toast,
    pub sound: Sound,
}

impl Default for Ui {
    fn default() -> Self {
        Ui {
            sidebar_width: 26,
            agent_panel_sort: AgentPanelSort::default(),
            status_indicators: StatusIndicators::default(),
            confirm_close: true,
            show_agent_labels_on_pane_borders: false,
            depth: Depth::default(),
            labels: true,
            titles: true,
            layout: Layout::default(),
            preview: true,
            show_git: true,
            projects_in_filter: false,
            sidebar: Sidebar::default(),
            toast: Toast::default(),
            sound: Sound::default(),
        }
    }
}

/// Agent panel ordering. `Attention` (default; alias `"priority"`) puts agents
/// that want attention first, in the attention tiers' order (blocked, then done and
/// not yet looked at, then idle or done and looked at, then working, then unknown),
/// most recent transition first within a tier. `Sessions` (aliases `"spaces"`,
/// `"workspaces"`) groups agents under their tmux session, in session order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPanelSort {
    #[default]
    #[serde(alias = "priority")]
    Attention,
    #[serde(alias = "spaces", alias = "workspaces")]
    Sessions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusIndicators {
    Dots,
    #[default]
    Symbols,
}

/// How deep the popup's tree expands when it opens: `agents` by default — shells are one
/// `l` away, and half the rows of a busy client were plain `zsh` panes nobody came to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    Sessions,
    #[default]
    Agents,
    All,
}

/// Tree (grouped, with headers) or flat (a single ordered list, agents first)
/// rendering of the popup's rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    Tree,
    Flat,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sidebar {
    pub agents: RowsConfig,
    pub spaces: RowsConfig,
}

impl Default for Sidebar {
    fn default() -> Self {
        Sidebar {
            agents: RowsConfig {
                row_gap: 0,
                rows: vec![
                    vec![
                        RowToken::bare("state_icon"),
                        RowToken::bare("workspace"),
                        RowToken::bare("tab"),
                    ],
                    vec![RowToken::bare("agent")],
                ],
                rows_by_agent: BTreeMap::new(),
            },
            spaces: RowsConfig {
                row_gap: 0,
                rows: vec![
                    vec![RowToken::bare("state_icon"), RowToken::bare("workspace")],
                    vec![RowToken::bare("branch"), RowToken::bare("git_status")],
                ],
                rows_by_agent: BTreeMap::new(),
            },
        }
    }
}

// `RowsConfig`'s own container-level `#[serde(default)]` (below) only ever
// fills a field missing *within* an already-present table from
// `RowsConfig::default()` — its type-neutral, empty-`rows` default. It has no
// way to know it is being deserialized as `Sidebar.agents` (default rows:
// state_icon/workspace/tab, agent) versus `Sidebar.spaces` (default rows:
// state_icon/workspace, branch/git_status). So once `[ui.sidebar.agents]` (or
// `.spaces`) is present at all — even with only `row_gap` or `rows_by_agent`
// set — a plain derived `Deserialize` for `Sidebar` would silently resolve the
// other, absent fields to the wrong (empty) default instead of that panel's
// real one. Deserialize each side as an all-optional shape instead, and merge
// explicitly against the correct default.
impl<'de> Deserialize<'de> for Sidebar {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct RawRows {
            row_gap: Option<u8>,
            rows: Option<Vec<Vec<RowToken>>>,
            rows_by_agent: Option<BTreeMap<String, Vec<Vec<RowToken>>>>,
        }

        impl RawRows {
            fn resolve(self, default: &RowsConfig) -> RowsConfig {
                RowsConfig {
                    row_gap: self.row_gap.unwrap_or(default.row_gap),
                    rows: self.rows.unwrap_or_else(|| default.rows.clone()),
                    rows_by_agent: self
                        .rows_by_agent
                        .unwrap_or_else(|| default.rows_by_agent.clone()),
                }
            }
        }

        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct RawSidebar {
            agents: Option<RawRows>,
            #[serde(alias = "sessions")]
            spaces: Option<RawRows>,
        }

        let raw = RawSidebar::deserialize(deserializer)?;
        let default = Sidebar::default();
        Ok(Sidebar {
            agents: raw
                .agents
                .map(|a| a.resolve(&default.agents))
                .unwrap_or(default.agents),
            spaces: raw
                .spaces
                .map(|s| s.resolve(&default.spaces))
                .unwrap_or(default.spaces),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RowsConfig {
    pub row_gap: u8,
    pub rows: Vec<Vec<RowToken>>,
    pub rows_by_agent: BTreeMap<String, Vec<Vec<RowToken>>>,
}

/// One token in a sidebar row: either a bare name (`"workspace"`) or a table with inline
/// styling (`{ token = "workspace", fg = "#89b4fa", bold = true }`), which may also carry up
/// to sixteen `rules` (see [`TokenRule`]): conditions on the token's text that restyle it, or
/// hide it, for the rows it matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowToken {
    pub token: String,
    pub fg: Option<String>,
    pub bold: Option<bool>,
    pub dim: Option<bool>,
    pub rules: Vec<TokenRule>,
}

/// The most rules one token takes (herdr's limit).
const MAX_TOKEN_RULES: usize = 16;

impl RowToken {
    fn bare(token: &str) -> RowToken {
        RowToken {
            token: token.to_string(),
            fg: None,
            bold: None,
            dim: None,
            rules: Vec::new(),
        }
    }

    /// What this token is drawn with on a row whose text for it is `value`: its own `fg`,
    /// `bold` and `dim` with the first matching rule's laid over them, or `None` when that
    /// rule hides it. The value is the token's whole text, before it is cut to fit.
    pub fn style_for(&self, value: &str) -> Option<TokenStyle> {
        let own = TokenStyle {
            fg: self.fg.clone(),
            bold: self.bold,
            dim: self.dim,
        };
        rules::matching_style(&self.rules, own, value)
    }
}

impl<'de> Deserialize<'de> for RowToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TokenVisitor;

        // A visitor rather than an untagged enum: an untagged one answers every mistake with
        // "did not match any variant", and a rule that is wrong has to say how.
        impl<'de> Visitor<'de> for TokenVisitor {
            type Value = RowToken;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a token name, or a table with a `token` and its style")
            }

            fn visit_str<E: de::Error>(self, token: &str) -> Result<RowToken, E> {
                Ok(RowToken::bare(token))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RowToken, A::Error> {
                let mut styled = RowToken::bare("");
                let mut token = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "token" => token = Some(map.next_value::<String>()?),
                        "fg" => styled.fg = Some(map.next_value()?),
                        "bold" => styled.bold = Some(map.next_value()?),
                        "dim" => styled.dim = Some(map.next_value()?),
                        "rules" => styled.rules = map.next_value()?,
                        // herdr's own keys that mean nothing here stay accepted.
                        _ => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                styled.token = token.ok_or_else(|| de::Error::missing_field("token"))?;
                if styled.rules.len() > MAX_TOKEN_RULES {
                    return Err(de::Error::custom(format!(
                        "sidebar tokens may contain at most {MAX_TOKEN_RULES} rules"
                    )));
                }
                if !styled.rules.is_empty()
                    && matches!(styled.token.as_str(), "state_icon" | "git_status")
                {
                    return Err(de::Error::custom(
                        "sidebar rules require a text-valued token",
                    ));
                }
                Ok(styled)
            }
        }

        deserializer.deserialize_any(TokenVisitor)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Toast {
    pub delivery: ToastDelivery,
    pub delay_seconds: f64,
    /// Delay before a `Done`-class toast, separate from `delay_seconds` (which
    /// gates `Request`-class toasts). See [`check`] for the range this is
    /// validated against.
    pub done_delay_seconds: f64,
    /// Raw passthrough (e.g. `position`), accepted and ignored: `tmux
    /// display-message` has no configurable screen position.
    pub herdr: BTreeMap<String, toml::Value>,
}

impl Default for Toast {
    fn default() -> Self {
        Toast {
            delivery: ToastDelivery::default(),
            delay_seconds: 1.0,
            done_delay_seconds: 5.0,
            herdr: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastDelivery {
    Off,
    #[default]
    Herdr,
    Terminal,
    System,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sound {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    pub done_path: Option<PathBuf>,
    pub request_path: Option<PathBuf>,
    pub agents: BTreeMap<String, SoundMode>,
}

impl Default for Sound {
    fn default() -> Self {
        let mut agents = BTreeMap::new();
        agents.insert("droid".to_string(), SoundMode::Off);
        Sound {
            enabled: true,
            path: None,
            done_path: None,
            request_path: None,
            agents,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundMode {
    #[default]
    Default,
    On,
    Off,
}

// Follows herdr's src/config/model.rs, commit 46a5091838 (#4487, #4102
// in v0.9.2's changelog).
// The same key and default; taw applies it in session::restore.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Session {
    pub resume_agents_on_restore: bool,
    /// Milliseconds between successive automatic agent restores; zero disables the
    /// spacing. Follows herdr's `[session] startup_per_agent_delay_ms`:
    /// a session with several resumable agents would launch them all in the same instant,
    /// which could overload whatever they call out to.
    pub startup_per_agent_delay_ms: u32,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            resume_agents_on_restore: true,
            startup_per_agent_delay_ms: 100,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Worktrees {
    #[serde(deserialize_with = "deserialize_tilde_path")]
    pub directory: PathBuf,
}

impl Default for Worktrees {
    fn default() -> Self {
        Worktrees {
            directory: expand_tilde("~/.tmux-agent-wizard/worktrees"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Plugins {
    pub compat_herdr_version: String,
}

impl Default for Plugins {
    fn default() -> Self {
        Plugins {
            compat_herdr_version: "0.9.3".to_string(),
        }
    }
}

/// herdr's `[experimental]` table. Only `pane_history` has a tmux meaning; the rest of
/// herdr's keys there describe its own renderer and are accepted without effect (listed
/// in [`KNOWN_LEAVES`] so a herdr config produces no warnings, and a *typo* still does).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Experimental {
    /// Save each pane's screen alongside the layout, and paint it back on restore.
    ///
    /// Off by default, and worth leaving off unless the trade is wanted: the saved file
    /// holds whatever was on screen in plain text, and the bytes are replayed raw. See
    /// [`crate::session::history`] for both hazards in full.
    pub pane_history: bool,
}

/// Preset key bindings for the popup's own navigation. `Modal` (default) binds
/// one letter per action, no mode to enter; `Search` is a pure fzf
/// search — every letter is typing for the filter box, and its only keys are
/// fzf's own: Enter, Esc, and the arrows, ctrl-j/k and ctrl-n/p to move. Either
/// way, `[keys]` entries other than `preset` override individual actions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeysPreset {
    #[default]
    Modal,
    Search,
}

/// One `[keys]` binding's value: a single key spec, or several equivalent ones.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            KeySpec::One(key) => vec![key.clone()],
            KeySpec::Many(keys) => keys.clone(),
        }
    }
}

/// The popup's own bindings: `preset` picks the base scheme, every other key
/// under `[keys]` is an action name bound to one or more key specs, overriding
/// that action's preset binding.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keys {
    pub preset: KeysPreset,
    pub bindings: BTreeMap<String, KeySpec>,
}

/// Deserializes `preset` as a typed field and keeps every other key in the
/// table as a binding, tolerating values that are neither a string nor a list
/// of strings (e.g. herdr's legacy `[keys.indexed]` sub-table) by dropping
/// just that entry instead of failing the whole config — herdr keys taw does
/// not represent are accepted without effect throughout this file; this is
/// that same policy applied one level deeper, inside a table `taw` does use.
impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut table = toml::Table::deserialize(deserializer)?;
        let preset = match table.remove("preset") {
            Some(value) => value.try_into::<KeysPreset>().map_err(D::Error::custom)?,
            None => KeysPreset::default(),
        };
        let bindings = table
            .into_iter()
            .filter_map(|(key, value)| value.try_into::<KeySpec>().ok().map(|spec| (key, spec)))
            .collect();
        Ok(Keys { preset, bindings })
    }
}

/// How a project directory becomes a tmux session/workspace name.
/// `"full-path"` and `"short-path"` are hyphenated in config (not the
/// `rename_all = "snake_case"` default) to match herdr's own spelling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMode {
    #[default]
    Directory,
    #[serde(rename = "full-path")]
    FullPath,
    #[serde(rename = "short-path")]
    ShortPath,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sessions {
    pub name: NameMode,
    /// Prefer zoxide's ranked directories when browsing projects to open or
    /// create a workspace from, falling back to `project_dirs`, and tell zoxide
    /// about each directory opened (`zoxide add`). `taw open <term>` asks zoxide
    /// for its match whatever this is set to.
    pub zoxide: bool,
    #[serde(deserialize_with = "deserialize_tilde_paths")]
    pub project_dirs: Vec<PathBuf>,
    /// A command line, kept exactly as written: `session::create` runs it through `sh` with
    /// the session name and the directory added as two arguments, and expands a leading
    /// `~` itself against the home directory it is run with. A path is a command line too.
    pub pre_create_hook: Option<String>,
    /// As [`Sessions::pre_create_hook`], run once a session has been created.
    pub post_create_hook: Option<String>,
    /// How long a create hook may run, in seconds, before taw stops it: a pre hook that
    /// does refuses the open, and a post hook that does is logged. `0` means no limit. See
    /// [`check`] for the range this is validated against.
    pub hook_timeout_seconds: f64,
}

impl Default for Sessions {
    fn default() -> Self {
        Sessions {
            name: NameMode::default(),
            zoxide: true,
            project_dirs: Vec::new(),
            pre_create_hook: None,
            post_create_hook: None,
            hook_timeout_seconds: 10.0,
        }
    }
}

/// How long a `[hooks]` command may run before taw stops it, unless `timeout_seconds` says
/// otherwise.
pub const DEFAULT_HOOK_TIMEOUT_SECONDS: f64 = 30.0;

/// `[hooks]`: the user's own commands, run on taw's events (see [`crate::hooks`]).
///
/// `timeout_seconds` is the one typed key. Every other key is an event name, or one of the
/// two hooks that can refuse (`pre_close`, `pre_worktree_remove`), and its value is a command
/// line or a list of them. A command is kept exactly as written; a blank one is no hook, as
/// it is for the create hooks.
///
/// A name written without quotes (`worktree.created = "..."`) is a table inside a table to
/// TOML, and reads here as the name it spells. Every well-shaped key is kept, even one taw
/// never fires, so `taw hooks list` can say so; [`check`] is what warns about a name taw does
/// not fire and about a value that is neither a command nor a list of them, which this drops
/// as [`Keys`] drops a binding it cannot use.
#[derive(Clone, Debug, PartialEq)]
pub struct Hooks {
    /// Seconds an event hook may run before taw stops it, along with what it started; `0`
    /// means no limit. See [`check`] for the range this is validated against. The two hooks
    /// that can refuse are limited by `[sessions] hook_timeout_seconds` instead.
    pub timeout_seconds: f64,
    commands: BTreeMap<String, Vec<String>>,
}

impl Default for Hooks {
    fn default() -> Self {
        Hooks {
            timeout_seconds: DEFAULT_HOOK_TIMEOUT_SECONDS,
            commands: BTreeMap::new(),
        }
    }
}

impl Hooks {
    /// The commands configured for `event`, in the order written; none when there are none.
    pub fn commands(&self, event: &str) -> &[String] {
        self.commands.get(event).map_or(&[], Vec::as_slice)
    }

    /// Every name with at least one command, in name order.
    pub fn configured(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.commands
            .iter()
            .map(|(name, commands)| (name.as_str(), commands.as_slice()))
    }
}

impl<'de> Deserialize<'de> for Hooks {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut table = toml::Table::deserialize(deserializer)?;
        let timeout_seconds = match table.remove("timeout_seconds") {
            Some(value) => value.try_into::<f64>().map_err(D::Error::custom)?,
            None => DEFAULT_HOOK_TIMEOUT_SECONDS,
        };
        let mut commands: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, value) in hook_entries(&table) {
            let Some(list) = command_list(value) else {
                continue;
            };
            if !list.is_empty() {
                commands.entry(name).or_default().extend(list);
            }
        }
        Ok(Hooks {
            timeout_seconds,
            commands,
        })
    }
}

/// The entries of a `[hooks]` table as `(event name, value)`: a table is read as the dotted
/// name it spells, all the way down, so `[hooks.agent] done = "x"` and `"agent.done" = "x"`
/// are the same entry.
fn hook_entries(table: &toml::Table) -> Vec<(String, &toml::Value)> {
    fn walk<'a>(prefix: &str, table: &'a toml::Table, out: &mut Vec<(String, &'a toml::Value)>) {
        for (key, value) in table {
            let name = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(nested) => walk(&name, nested, out),
                other => out.push((name, other)),
            }
        }
    }
    let mut out = Vec::new();
    walk("", table, &mut out);
    out
}

/// A hook's value as its commands: a string, or a list of strings, with the blank ones
/// dropped. `None` for anything else.
fn command_list(value: &toml::Value) -> Option<Vec<String>> {
    let commands: Vec<&str> = match value {
        toml::Value::String(command) => vec![command.as_str()],
        toml::Value::Array(items) => items
            .iter()
            .map(toml::Value::as_str)
            .collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    Some(
        commands
            .into_iter()
            .filter(|command| !command.trim().is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `[sandbox]`: which profile `taw sandbox` runs when you name none. Nothing
/// here changes the popup, which shows 📦 on an agent in a container either way.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sandbox {
    /// `profile`: the profile for an agent `agents` names none for: one of yours, or `default`,
    /// taw's own, which is what runs while this is unset.
    pub profile: Option<String>,
    /// `[sandbox.agents]`: a profile for one agent, by the name taw gives it (`codex = "default"`).
    pub agents: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Detection {
    /// Layer taw's own state-detection heuristics on top of an installed
    /// agent integration's own reports, instead of relying on the
    /// integration alone.
    pub supplements: bool,
}

impl Default for Detection {
    fn default() -> Self {
        Detection { supplements: true }
    }
}

/// Expand a leading `~` (home directory) in a config-file path value. Applied
/// to `worktrees.directory` and `sessions.project_dirs`. Sound paths are kept
/// exactly as given; relative-to-config-dir resolution for those happens at use
/// time, elsewhere. The create hooks are command lines, not paths, and are kept
/// as written (see [`Sessions::pre_create_hook`]).
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if raw == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(raw)
}

fn deserialize_tilde_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(expand_tilde(&raw))
}

/// `[theme.custom]`: a colour token is a string. herdr also keeps two tables there, `light`
/// and `dark`, with the overrides for each appearance when `auto_switch` is on, and its
/// default config lists them. `auto_switch` switches the theme name here, not the colours, so
/// those two are accepted and left out, as the unknown-key check has always counted them as
/// known. What is inside them is not checked, not even that it is a colour: nothing here reads
/// it. A table under any other name is not a colour and
/// is refused with the path it sits at.
fn deserialize_colour_tokens<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Tokens;

    impl<'de> Visitor<'de> for Tokens {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a table of colour strings")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut tokens = BTreeMap::new();
            while let Some(name) = map.next_key::<String>()? {
                if matches!(name.as_str(), "light" | "dark") {
                    // A string under either name is a token like any other (and an unknown
                    // one, which `check` says); a table is herdr's overrides.
                    match map.next_value::<toml::Value>()? {
                        toml::Value::Table(_) => {}
                        toml::Value::String(colour) => {
                            tokens.insert(name, colour);
                        }
                        other => {
                            return Err(de::Error::invalid_type(
                                de::Unexpected::Other(other.type_str()),
                                &"a string",
                            ));
                        }
                    }
                } else {
                    tokens.insert(name, map.next_value::<String>()?);
                }
            }
            Ok(tokens)
        }
    }

    deserializer.deserialize_map(Tokens)
}

/// The `Vec` counterpart of [`deserialize_tilde_path`], for `sessions.project_dirs`.
fn deserialize_tilde_paths<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    Ok(raw.iter().map(|s| expand_tilde(s)).collect())
}

/// Exact dotted config paths we understand as single values. Anything not in
/// this list, and not under a [`WILDCARD_SUBTABLES`] prefix, is unknown.
const KNOWN_LEAVES: &[&str] = &[
    "onboarding",
    "theme.name",
    "theme.auto_switch",
    "theme.dark_name",
    "theme.light_name",
    "update.manifest_check",
    "update.channel",
    "update.version_check",
    "ui.sidebar_width",
    "ui.agent_panel_sort",
    "ui.status_indicators",
    "ui.confirm_close",
    "ui.show_agent_labels_on_pane_borders",
    "ui.depth",
    "ui.labels",
    "ui.titles",
    "ui.layout",
    "ui.preview",
    "ui.show_git",
    "ui.projects_in_filter",
    // herdr's own pane-border toggle: tmux draws split-pane borders itself, so this
    // has no tmux equivalent to drive (see also show_agent_labels_on_pane_borders).
    "ui.pane_borders",
    "ui.sidebar.agents.row_gap",
    "ui.sidebar.agents.rows",
    "ui.sidebar.spaces.row_gap",
    "ui.sidebar.spaces.rows",
    // [ui.sidebar.sessions] is accepted as an alias of [ui.sidebar.spaces] (see the
    // `spaces` field's `#[serde(alias = "sessions")]` on `Sidebar`'s deserializer).
    "ui.sidebar.sessions.row_gap",
    "ui.sidebar.sessions.rows",
    "ui.toast.delivery",
    "ui.toast.delay_seconds",
    "ui.toast.done_delay_seconds",
    "ui.sound.enabled",
    "ui.sound.path",
    "ui.sound.done_path",
    "ui.sound.request_path",
    "session.resume_agents_on_restore",
    "session.startup_per_agent_delay_ms",
    "worktrees.directory",
    "plugins.compat_herdr_version",
    "experimental.pane_history",
    // herdr's other `[experimental]` keys, accepted without effect: each describes its
    // own renderer or input handling, which tmux owns here. Enumerated rather than
    // wildcarded so a typo in `pane_history` is still a warning.
    "experimental.allow_nested",
    "experimental.kitty_graphics",
    "experimental.reveal_hidden_cursor_for_cjk_ime",
    "experimental.cjk_ime_agents",
    "experimental.cjk_ime_cursor_shape",
    "experimental.switch_ascii_input_source_in_prefix",
    "sessions.name",
    "sessions.zoxide",
    "sessions.project_dirs",
    "sessions.pre_create_hook",
    "sessions.post_create_hook",
    "sessions.hook_timeout_seconds",
    "detection.supplements",
    "keys.preset",
    "sandbox.profile",
];

/// Dotted prefixes where every key underneath is accepted without individual
/// enumeration: arbitrary token/agent-id names, or tables passed through raw.
const WILDCARD_SUBTABLES: &[&str] = &[
    "keys",
    "hooks",
    "terminal",
    "theme.custom",
    "theme.custom.light",
    "theme.custom.dark",
    "ui.toast.herdr",
    "ui.sound.agents",
    "ui.sidebar.agents.rows_by_agent",
    "ui.sidebar.sessions.rows_by_agent",
    "sandbox.agents",
    // The names of other machines, not keys: `crate::machine::config` reads the table and says what
    // it drops.
    "machines",
];

fn walk_unknown_keys(table: &toml::Table, prefix: &str, out: &mut Vec<Diagnostic>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if WILDCARD_SUBTABLES.contains(&path.as_str()) || KNOWN_LEAVES.contains(&path.as_str()) {
            continue;
        }
        match value {
            toml::Value::Table(nested) => walk_unknown_keys(nested, &path, out),
            _ => out.push(Diagnostic::warning(format!("unknown config key: {path}"))),
        }
    }
}

/// The keys a styled token in a sidebar row takes.
const TOKEN_KEYS: &[&str] = &["token", "fg", "bold", "dim", "rules"];

/// A key a styled token does not know, in `[ui.sidebar.agents]`, `[ui.sidebar.spaces]` and its
/// alias `[ui.sidebar.sessions]`: their `rows`, and each list under `rows_by_agent`. The token
/// reader leaves such a key alone, so the popup loads the config all the same; but a row that
/// ignored a `hide` written outside its rules, or a `rule` for `rules`, went on being drawn with
/// not a word as to why, which is what this says. The key is named by where it is.
fn check_token_keys(table: &toml::Table) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(toml::Value::Table(ui)) = table.get("ui") else {
        return out;
    };
    let Some(toml::Value::Table(sidebar)) = ui.get("sidebar") else {
        return out;
    };
    for panel in ["agents", "spaces", "sessions"] {
        let Some(toml::Value::Table(panel_table)) = sidebar.get(panel) else {
            continue;
        };
        let path = format!("ui.sidebar.{panel}");
        if let Some(rows) = panel_table.get("rows") {
            token_keys_in_rows(rows, &format!("{path}.rows"), &mut out);
        }
        if let Some(toml::Value::Table(by_agent)) = panel_table.get("rows_by_agent") {
            for (agent, rows) in by_agent {
                token_keys_in_rows(rows, &format!("{path}.rows_by_agent.{agent}"), &mut out);
            }
        }
    }
    out
}

/// [`check_token_keys`] over one list of rows, each a list of tokens, `path` being where it is.
fn token_keys_in_rows(rows: &toml::Value, path: &str, out: &mut Vec<Diagnostic>) {
    let toml::Value::Array(rows) = rows else {
        return;
    };
    for (row, tokens) in rows.iter().enumerate() {
        let toml::Value::Array(tokens) = tokens else {
            continue;
        };
        for (at, token) in tokens.iter().enumerate() {
            let toml::Value::Table(token) = token else {
                continue;
            };
            for key in token
                .keys()
                .filter(|key| !TOKEN_KEYS.contains(&key.as_str()))
            {
                let hint = match key.as_str() {
                    "hide" => " (a token's `hide` goes inside one of its `rules`)",
                    "rule" => " (did you mean `rules`?)",
                    _ => "",
                };
                out.push(Diagnostic::warning(format!(
                    "unknown config key: {path}[{row}][{at}].{key}{hint}"
                )));
            }
        }
    }
}

/// [`walk_unknown_keys`], collected from a table's root. The entry point the
/// config-key tests exercise directly, against a hand-built [`toml::Table`]
/// rather than a file on disk.
fn check_unknown_keys(table: &toml::Table) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    walk_unknown_keys(table, "", &mut out);
    out
}

// Follows herdr's src/config/io.rs. Differences: the whole document is
// compared before and after dropping line-start BOMs, instead of asking the TOML error for
// the exact span of the character it stumbled on.
/// Normalizes UTF-8 byte-order marks in config text.
///
/// A line-oriented edit (`config reset-keys`, or an editor that does not know TOML) can
/// displace a leading BOM into the middle of the file, where — unlike at the very start —
/// this crate's TOML parser rejects it outright. So: a genuinely leading BOM is dropped
/// unconditionally (harmless either way, since this crate's parser already tolerates it
/// there). For anything left over, this must tell a displaced BOM from a U+FEFF that is
/// legitimate string data without being able to ask the parser *which character* it
/// stumbled on: if the file already parses, it is returned unchanged, BOM and all; if not,
/// every remaining line-start BOM is dropped in one pass and the result kept only if that
/// alone makes the document parse. A real syntax error elsewhere leaves it unable to
/// parse either way, so nothing is changed rather than guessing further and risking a
/// string's own content.
///
/// Follows herdr's `config/io.rs` `normalize_utf8_bom`, whose algorithm instead
/// repeatedly reparses and asks the TOML error for the exact byte span of the character
/// it stumbled on, removing only a span that is exactly one BOM. This crate's `toml`
/// dependency does not report a span that precise for this failure, and its `Value:
/// FromStr` (what herdr's own tests parse a whole document with) does not parse a
/// multi-key document at all — both confirmed against its actual behaviour, not assumed —
/// so this reaches the same documented behaviour by whole-document before/after
/// comparison (via `toml::from_str::<toml::Table>`, what this crate's own `load`/`check`
/// already parse a document with) instead of a per-character diagnostic.
pub(crate) fn normalize_utf8_bom(content: &str) -> String {
    // A leading BOM is dropped unconditionally: this crate's `toml` dependency tolerates
    // one there (unlike a displaced one), so there is nothing to check first — only
    // somewhere to stop looking for more.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let parses = |text: &str| toml::from_str::<toml::Table>(text).is_ok();
    if !content.contains('\u{feff}') || parses(content) {
        return content.to_owned();
    }
    let candidate: String = content
        .split_inclusive('\n')
        .map(|line| line.strip_prefix('\u{feff}').unwrap_or(line))
        .collect();
    if parses(&candidate) {
        candidate
    } else {
        content.to_owned()
    }
}

/// One key of the config whose value is one of a few words: where it lives (`*` stands for any key
/// of that table, the way `[ui.sound.agents]` names an agent), the words it shows as allowed, and
/// the other spellings it takes without showing them.
struct EnumKey {
    path: &'static [&'static str],
    allowed: &'static [&'static str],
    aliases: &'static [&'static str],
}

/// Every key of the config that takes one of a few words: the one place that says which they are and
/// what each allows, so a wrong value reads the same for all of them ([`wrong_words`]) rather
/// than in serde's words. A test pins each entry to what the config really takes.
const ENUM_KEYS: &[EnumKey] = &[
    EnumKey {
        path: &["ui", "agent_panel_sort"],
        allowed: &["attention", "sessions"],
        aliases: &["priority", "spaces", "workspaces"],
    },
    EnumKey {
        path: &["ui", "status_indicators"],
        allowed: &["dots", "symbols"],
        aliases: &[],
    },
    EnumKey {
        path: &["ui", "depth"],
        allowed: &["sessions", "agents", "all"],
        aliases: &[],
    },
    EnumKey {
        path: &["ui", "layout"],
        allowed: &["tree", "flat"],
        aliases: &[],
    },
    EnumKey {
        path: &["ui", "toast", "delivery"],
        allowed: &["off", "herdr", "terminal", "system"],
        aliases: &[],
    },
    EnumKey {
        path: &["ui", "sound", "agents", "*"],
        allowed: &["default", "on", "off"],
        aliases: &[],
    },
    EnumKey {
        path: &["keys", "preset"],
        allowed: &["modal", "search"],
        aliases: &[],
    },
    EnumKey {
        path: &["sessions", "name"],
        allowed: &["directory", "full-path", "short-path"],
        aliases: &[],
    },
];

/// What a value is, for a message that says what was given: a string in quotes, a number or a
/// boolean as written, a list or a table as the word for it.
fn given_words(value: &toml::Value) -> String {
    match value {
        toml::Value::String(word) => format!("\"{word}\""),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(n) => n.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(_) => "a list".to_string(),
        toml::Value::Table(_) => "a table".to_string(),
    }
}

/// A value in the config that is not one of the words its key allows: where it is (the tables and
/// the key), the first word the key allows, and what to say about it.
struct WrongWord {
    path: Vec<String>,
    allowed: &'static str,
    said: String,
}

/// The values in `table` that are not one of the words their key allows, each in plain words: the
/// key, what was given and the allowed words (`[ui] depth: 3 is not one of sessions, agents, all`).
/// Run when the config does not deserialize, to say why in these words instead of serde's.
fn wrong_words(table: &toml::Table) -> Vec<WrongWord> {
    let mut problems = Vec::new();
    for key in ENUM_KEYS {
        let (name, section) = key.path.split_last().expect("a key has a name");
        let mut tables: Vec<(Vec<String>, &toml::Table)> = vec![(Vec::new(), table)];
        for part in section {
            tables = tables
                .into_iter()
                .filter_map(|(at, t)| match t.get(*part)? {
                    toml::Value::Table(inner) => {
                        let mut at = at;
                        at.push((*part).to_string());
                        Some((at, inner))
                    }
                    _ => None,
                })
                .collect();
        }
        for (at, t) in tables {
            let entries: Vec<(&String, &toml::Value)> = if *name == "*" {
                t.iter().collect()
            } else {
                t.get_key_value(*name).into_iter().collect()
            };
            for (found, value) in entries {
                let takes = match value {
                    toml::Value::String(word) => {
                        key.allowed.contains(&word.as_str()) || key.aliases.contains(&word.as_str())
                    }
                    _ => false,
                };
                if !takes {
                    let said = format!(
                        "[{}] {found}: {} is not one of {}",
                        at.join("."),
                        given_words(value),
                        key.allowed.join(", ")
                    );
                    let mut path = at.clone();
                    path.push(found.clone());
                    problems.push(WrongWord {
                        path,
                        allowed: key.allowed[0],
                        said,
                    });
                }
            }
        }
    }
    problems
}

/// `table` as a [`Config`], and when it is not one, why: in [`wrong_words`]'s plain words when a
/// key that takes one of a few words was given something else, else serde's own. serde stops at the
/// first thing it cannot take, so with a wrong word it is asked again with an allowed word in each
/// wrong word's place, and what it then finds wrong elsewhere is said beside the plain words.
fn config_from(table: toml::Table) -> Result<Config, Vec<String>> {
    let wrong = wrong_words(&table);
    if wrong.is_empty() {
        return toml::Value::Table(table)
            .try_into::<Config>()
            .map_err(|err| vec![err.to_string()]);
    }
    let mut patched = table;
    for word in &wrong {
        let Some((key, tables)) = word.path.split_last() else {
            continue;
        };
        let mut at = Some(&mut patched);
        for part in tables {
            at = at.and_then(|t| match t.get_mut(part) {
                Some(toml::Value::Table(inner)) => Some(inner),
                _ => None,
            });
        }
        if let Some(t) = at {
            t.insert(key.clone(), toml::Value::String(word.allowed.to_string()));
        }
    }
    let mut problems: Vec<String> = wrong.into_iter().map(|word| word.said).collect();
    if let Err(err) = toml::Value::Table(patched).try_into::<Config>() {
        problems.push(err.to_string());
    }
    Err(problems)
}

/// Load `config.toml`, falling back to [`Config::default`] when the file does
/// not exist.
pub fn load(paths: &Paths) -> anyhow::Result<Config> {
    let path = &paths.config_file;
    if !path.exists() {
        return Ok(Config::default());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = normalize_utf8_bom(&raw);
    let table: toml::Table =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let config = config_from(table).map_err(|why| {
        anyhow::anyhow!("{}", why.join("; ")).context(format!("deserializing {}", path.display()))
    })?;
    Ok(config)
}

/// Why `taw sandbox` cannot tell from `config.toml` which profile you chose.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SandboxDoubt {
    /// The file does not load: why, on one line.
    #[error("config {file} did not load ({why})")]
    NotLoaded { file: String, why: String },
    /// Its `[sandbox]` table has a line taw drops, in config check's words.
    #[error("config {file} has a line in [sandbox] taw drops ({line})")]
    Dropped { file: String, line: String },
    /// It is a link to a file that is not there, as stow leaves one when the dotfiles move.
    #[error("config {file} is a link to a file that is not there")]
    Gone { file: String },
}

/// The config `taw sandbox` picks a profile by, read whole: `config.toml`, or the defaults when
/// there is none. Refused when taw cannot tell which profile you chose: the file does not load, or
/// its `[sandbox]` table has a line taw drops (a key it does not know, a line for an agent it does
/// not know or calls by another name). Run on the defaults then, a strict profile of yours could
/// quietly give way to taw's own.
pub fn load_for_sandbox(paths: &Paths) -> Result<Config, SandboxDoubt> {
    let path = &paths.config_file;
    let file = crate::machine::config::config_file_word(paths);
    let not_loaded = |why: String| SandboxDoubt::NotLoaded {
        file: file.clone(),
        why: one_line(&why),
    };
    // Only a file that is not there at all is no config; a link whose file is gone is one taw
    // cannot read.
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => return Err(not_loaded(err.to_string())),
        Ok(_) => {}
    }
    if std::fs::metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
        return Err(SandboxDoubt::Gone { file });
    }
    let raw = std::fs::read_to_string(path).map_err(|err| not_loaded(err.to_string()))?;
    let raw = normalize_utf8_bom(&raw);
    let table: toml::Table = toml::from_str(&raw).map_err(|err| not_loaded(err.to_string()))?;
    let mut dropped = Vec::new();
    if let Some(sandbox) = table.get("sandbox") {
        let only = toml::Table::from_iter([("sandbox".to_string(), sandbox.clone())]);
        walk_unknown_keys(&only, "", &mut dropped);
    }
    let config = config_from(table).map_err(|why| not_loaded(why.join("; ")))?;
    let mut lines: Vec<String> = dropped.into_iter().map(|found| found.message).collect();
    lines.extend(crate::sandbox::profile::dropped_lines(&config.sandbox));
    match lines.into_iter().next() {
        Some(line) => Err(SandboxDoubt::Dropped { file, line }),
        None => Ok(config),
    }
}

/// A parser's error on one line: its first line, then what it says after the lines of the file it
/// quotes (`TOML parse error at line 1, column 9: unclosed table, expected `]``).
pub(crate) fn one_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default().to_string();
    let said: Vec<&str> = lines
        .filter(|line| {
            !line
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start()
                .starts_with('|')
        })
        .collect();
    if said.is_empty() {
        first
    } else {
        format!("{first}: {}", said.join("; "))
    }
}

/// The warning `config check` gives when `[theme] auto_switch` is on and the tmux in use is older than
/// 3.6: that tmux has no `#{client_theme}`, so the popup and `taw app` cannot tell their terminal's light
/// or dark mode and keep `[theme] name`. `tmux` is what `tmux -V` said (`None` when it could not be run: nothing is
/// claimed then), and the warning names that version.
pub fn check_auto_switch(
    theme: &Theme,
    tmux: Option<crate::tmux::version::TmuxVersion>,
) -> Option<Diagnostic> {
    let version = tmux.filter(|v| theme.auto_switch && !v.at_least(3, 6))?;
    let patch = if version.patch == ' ' {
        String::new()
    } else {
        version.patch.to_string()
    };
    Some(Diagnostic::warning(format!(
        "[theme] auto_switch is on, but this tmux is {}.{}{patch} and cannot tell a popup whether its terminal is light or dark (tmux 3.6 and later can), so the popup and taw app keep [theme] name \"{}\"",
        version.major, version.minor, theme.name
    )))
}

/// Surfaces the theme resolver's own findings as warnings: an unknown `[theme]
/// name`, an unknown `[theme.custom]` token, or a custom value that is not a
/// colour. Without this, `config check` stays silent while the TUI quietly falls
/// back to the default palette. Resolves exactly what the TUI resolves at startup.
fn check_theme(theme: &Theme) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = crate::tui::theme::resolve(&theme.name, &theme.custom)
        .warnings
        .into_iter()
        .map(Diagnostic::warning)
        .collect();
    // With `auto_switch` on the terminal's mode picks `dark_name` or `light_name`, so a value in
    // either that no theme has would fall back to the default with nothing said.
    if theme.auto_switch {
        for (key, value) in [
            ("dark_name", theme.dark_name.as_deref()),
            ("light_name", theme.light_name.as_deref()),
        ] {
            let Some(value) = value else { continue };
            if !crate::tui::theme::resolve(value, &BTreeMap::new())
                .warnings
                .is_empty()
            {
                out.push(Diagnostic::warning(format!(
                    "unknown [theme] {key} '{value}', falling back to '{}'",
                    crate::tui::theme::DEFAULT_NAME
                )));
            }
        }
    }
    out
}

/// A syntactically valid but out-of-range `[ui.toast]` delay: neither
/// `delay_seconds` (`Request`-class toasts) nor `done_delay_seconds`
/// (`Done`-class toasts) fails to deserialize on a bad number the way a bad
/// enum string would, so each needs this explicit range check to surface as
/// an error instead of silently misbehaving at watch time.
fn check_toast(toast: &Toast) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (field, seconds) in [
        ("delay_seconds", toast.delay_seconds),
        ("done_delay_seconds", toast.done_delay_seconds),
    ] {
        if !(0.0..=3600.0).contains(&seconds) {
            out.push(Diagnostic::error(format!(
                "ui.toast.{field} must be between 0 and 3600"
            )));
        }
    }
    out
}

/// A `[sessions] hook_timeout_seconds` that is out of range. Like the toast delays it does not
/// fail to deserialize on a bad number, so it needs this explicit check to surface as an error
/// and not as a hook that is stopped at once or never.
fn check_sessions(sessions: &Sessions) -> Vec<Diagnostic> {
    if (0.0..=3600.0).contains(&sessions.hook_timeout_seconds) {
        return Vec::new();
    }
    vec![Diagnostic::error(
        "sessions.hook_timeout_seconds must be between 0 and 3600 (0 means no limit)",
    )]
}

/// A `[hooks] timeout_seconds` that is out of range. Like the toast delays and the create
/// hooks' limit it does not fail to deserialize on a bad number, so it needs this explicit
/// check to surface as an error and not as a hook that is stopped at once or never.
fn check_hooks(hooks: &Hooks) -> Vec<Diagnostic> {
    if (0.0..=3600.0).contains(&hooks.timeout_seconds) {
        return Vec::new();
    }
    vec![Diagnostic::error(
        "hooks.timeout_seconds must be between 0 and 3600 (0 means no limit)",
    )]
}

/// The `[hooks]` keys taw cannot use: a name it does not fire (a typo would otherwise be a
/// hook that never runs, with nothing to say so), and a value that is neither a command nor
/// a list of commands, which [`Hooks`]'s `Deserialize` impl drops.
fn check_hook_keys(table: &toml::Table) -> Vec<Diagnostic> {
    let Some(toml::Value::Table(hooks)) = table.get("hooks") else {
        return Vec::new();
    };
    let mut hooks = hooks.clone();
    hooks.remove("timeout_seconds");
    let mut out = Vec::new();
    for (name, value) in hook_entries(&hooks) {
        if !crate::hooks::events::is_known(&name) {
            out.push(Diagnostic::warning(format!(
                "unknown hook event: hooks.{name} (run `taw hooks list` for the names taw fires)"
            )));
        } else if command_list(value).is_none() {
            out.push(Diagnostic::warning(format!(
                "hooks.{name}: expected a command or a list of commands"
            )));
        }
    }
    out
}

/// A `[keys]` entry other than `preset` whose value is neither a string nor a
/// list of strings: [`Keys`]'s `Deserialize` impl drops such an entry rather
/// than failing the whole config (serde has no way to also emit a warning
/// from inside a `Deserialize` impl), so this is the only place a malformed
/// binding is ever surfaced — without it, a typo like `rename = 5` silently
/// vanishes instead of producing a diagnostic. herdr's own legacy
/// `[keys.indexed]` sub-table is exempted: like every other compat-only herdr
/// key, it is accepted without effect, not something a user is likely to have
/// mistyped.
fn check_keys_bindings(table: &toml::Table) -> Vec<Diagnostic> {
    let Some(toml::Value::Table(keys)) = table.get("keys") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, value) in keys {
        if name == "preset" || name == "indexed" {
            continue;
        }
        let is_key_or_list = match value {
            toml::Value::String(_) => true,
            toml::Value::Array(items) => items
                .iter()
                .all(|item| matches!(item, toml::Value::String(_))),
            _ => false,
        };
        if !is_key_or_list {
            out.push(Diagnostic::warning(format!(
                "keys.{name}: expected a key or a list of keys"
            )));
        }
    }
    out
}

/// A `[keys]` spec that does not parse (a typo, or herdr's `prefix+g` pasted as it is) is
/// ignored by the popup, which keeps the action's preset keys and says so. `taw config check`
/// says the same, in the same words, as a warning. Unknown action names and two actions
/// contesting one key are the popup's to report: they leave nothing unbound.
fn check_keys_specs(keys: &Keys) -> Vec<Diagnostic> {
    use crate::tui::keys::DiagnosticKind;
    crate::tui::keys::resolve(keys)
        .1
        .into_iter()
        .filter(|d| d.kind == DiagnosticKind::Unparsed)
        .map(|d| Diagnostic::warning(d.message))
        .collect()
}

/// Validate `config.toml`: TOML syntax errors and invalid values (e.g. a bad
/// enum string) are [`Level::Error`]; unrecognized keys are [`Level::Warning`].
/// A missing file has nothing of its own to check. Your sandbox profiles folder is
/// read whatever the file says, and what is wrong there is a warning too
/// ([`check_apart`]).
pub fn check(paths: &Paths) -> Vec<Diagnostic> {
    let Checked { mut file, profiles } = check_apart(paths);
    file.extend(profiles);
    file
}

/// What [`check`] finds, by where it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// About `config.toml`: its syntax, its values and keys, and the profiles and agents its
    /// `[sandbox]` table names, which are checked only when the file loads.
    pub file: Vec<Diagnostic>,
    /// About your sandbox profiles folder alone: a file there that no line of the config names,
    /// and that is not executable or whose name cannot be a profile's. Read with no config file,
    /// and with one that does not load.
    pub profiles: Vec<Diagnostic>,
}

/// [`check`], with what is about the file kept apart from what is about your profiles folder
/// alone: `taw config reload` says whether the file took effect from the first.
pub fn check_apart(paths: &Paths) -> Checked {
    let (mut file, sandbox) = check_file(paths);
    let found = crate::sandbox::profile::check(paths, &sandbox.unwrap_or_default());
    file.extend(found.config.into_iter().map(Diagnostic::warning));
    Checked {
        file,
        profiles: found.folder.into_iter().map(Diagnostic::warning).collect(),
    }
}

/// The diagnostics of `config.toml` itself, and its `[sandbox]` table when the file loads.
fn check_file(paths: &Paths) -> (Vec<Diagnostic>, Option<Sandbox>) {
    let path = &paths.config_file;
    let mut diagnostics = Vec::new();
    if std::fs::symlink_metadata(path).is_err() {
        return (diagnostics, None);
    }
    // A link whose file is gone, as stow leaves one when the dotfiles move: no config is read.
    if std::fs::metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
        diagnostics.push(Diagnostic::warning(format!(
            "{0} is a link to a file that is not there, so taw reads no config (ls -l {0})",
            path.display()
        )));
        return (diagnostics, None);
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            diagnostics.push(Diagnostic::error(format!(
                "reading {}: {err}",
                path.display()
            )));
            return (diagnostics, None);
        }
    };
    let raw = normalize_utf8_bom(&raw);
    let table: toml::Table = match toml::from_str(&raw) {
        Ok(table) => table,
        Err(err) => {
            diagnostics.push(Diagnostic::error(format!("{}: {err}", path.display())));
            return (diagnostics, None);
        }
    };
    diagnostics.extend(check_unknown_keys(&table));
    // Lines of `[machines]` that taw drops: the file still loads, as with a bad `[sandbox]` line.
    diagnostics.extend(
        crate::machine::config::parse(&raw)
            .dropped
            .into_iter()
            .map(Diagnostic::warning),
    );
    diagnostics.extend(check_token_keys(&table));
    diagnostics.extend(check_keys_bindings(&table));
    diagnostics.extend(check_hook_keys(&table));
    match config_from(table) {
        Ok(config) => {
            diagnostics.extend(check_keys_specs(&config.keys));
            diagnostics.extend(check_theme(&config.theme));
            diagnostics.extend(check_toast(&config.ui.toast));
            diagnostics.extend(check_sessions(&config.sessions));
            diagnostics.extend(check_hooks(&config.hooks));
            (diagnostics, Some(config.sandbox))
        }
        Err(why) => {
            // One error for each key that was given the wrong thing, or serde's single one.
            for line in why {
                diagnostics.push(Diagnostic::error(format!("{}: {line}", path.display())));
            }
            (diagnostics, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn paths_for(dir: &Path) -> Paths {
        let agent_detection = dir.join("agent-detection");
        let plugins_dir = dir.join("plugins");
        Paths {
            config_dir: dir.to_path_buf(),
            config_file: dir.join("config.toml"),
            state_dir: dir.to_path_buf(),
            override_dir: agent_detection.clone(),
            remote_manifest_dir: agent_detection.join("remote"),
            manifest_marker: agent_detection.join("last-check"),
            sessions_dir: dir.join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.join("logs"),
        }
    }

    fn write_config(dir: &Path, contents: &str) -> Paths {
        let paths = paths_for(dir);
        std::fs::write(&paths.config_file, contents).unwrap();
        paths
    }

    fn error_messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .filter(|d| d.level == Level::Error)
            .map(|d| d.message.as_str())
            .collect()
    }

    /// A config file holding `value` (TOML text) at the dotted key `path`, as one nested table.
    fn doc_with(path: &[&str], value: &str) -> String {
        let (key, tables) = path.split_last().unwrap();
        let mut text = String::new();
        if !tables.is_empty() {
            text.push_str(&format!("[{}]\n", tables.join(".")));
        }
        text.push_str(&format!("{key} = {value}\n"));
        text
    }

    /// What `taw config check` reports as errors for a file with `text` in it.
    fn errors_of(text: &str) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_config(dir.path(), text);
        error_messages(&check(&paths))
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// A number, a wrong word and a wrong type where a word is wanted read as the key, what was
    /// given and the allowed words, not as serde's `invalid type: unit variant`.
    #[test]
    fn a_number_a_wrong_word_and_a_wrong_type_read_in_plain_words() {
        for (given, said) in [
            ("3", "[ui] depth: 3 is not one of sessions, agents, all"),
            (
                "\"deep\"",
                "[ui] depth: \"deep\" is not one of sessions, agents, all",
            ),
            (
                "true",
                "[ui] depth: true is not one of sessions, agents, all",
            ),
            (
                "[\"all\"]",
                "[ui] depth: a list is not one of sessions, agents, all",
            ),
            (
                "{ a = 1 }",
                "[ui] depth: a table is not one of sessions, agents, all",
            ),
        ] {
            let found = errors_of(&doc_with(&["ui", "depth"], given));
            assert_eq!(found.len(), 1, "{given}: {found:?}");
            assert!(found[0].ends_with(said), "{given}: {found:?}");
            assert!(!found[0].contains("unit variant"), "{given}: {found:?}");
            assert!(!found[0].contains("invalid type"), "{given}: {found:?}");
        }
    }

    /// A wrong word does not hide another error of the file: serde's error about the other key is
    /// said beside the word's plain line.
    #[test]
    fn a_wrong_word_does_not_hide_another_error_of_the_config() {
        let found = errors_of("[ui]\ndepth = 3\nsidebar_width = \"wide\"\n");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].ends_with("[ui] depth: 3 is not one of sessions, agents, all"),
            "{found:?}"
        );
        assert!(found[1].contains("sidebar_width"), "{found:?}");
        assert!(found[1].contains("\"wide\""), "{found:?}");
        // With nothing else wrong, the plain line is the only one.
        let alone = errors_of("[ui]\ndepth = 3\n");
        assert_eq!(alone.len(), 1, "{alone:?}");
    }

    /// Every enum key of the config goes through the one mapping, so each says its own key and its
    /// own allowed words the same way, including `[keys] preset`, which serde reports as `keys`.
    #[test]
    fn every_enum_key_reads_the_same_way_with_its_own_allowed_words() {
        for key in ENUM_KEYS {
            let path: Vec<&str> = key
                .path
                .iter()
                .map(|part| if *part == "*" { "claude" } else { part })
                .collect();
            let (name, section) = path.split_last().unwrap();
            let allowed = key.allowed.join(", ");
            for (given, shown) in [("3", "3"), ("\"nonsense\"", "\"nonsense\"")] {
                let found = errors_of(&doc_with(&path, given));
                let said = format!(
                    "[{}] {name}: {shown} is not one of {allowed}",
                    section.join(".")
                );
                assert_eq!(found.len(), 1, "{path:?} {given}: {found:?}");
                assert!(found[0].ends_with(&said), "{path:?} {given}: {found:?}");
            }
        }
        assert!(
            ENUM_KEYS.iter().any(|key| key.path == ["keys", "preset"]),
            "the mapping covers [keys] preset"
        );
    }

    /// The mapping says what serde takes: every allowed word and alias loads, and serde's own list of
    /// the words it knows is the mapping's allowed words and aliases, so the two cannot drift apart.
    #[test]
    fn the_allowed_words_are_the_ones_the_config_takes() {
        for key in ENUM_KEYS {
            let path: Vec<&str> = key
                .path
                .iter()
                .map(|part| if *part == "*" { "claude" } else { part })
                .collect();
            for word in key.allowed.iter().chain(key.aliases) {
                let dir = tempfile::tempdir().unwrap();
                let paths = write_config(dir.path(), &doc_with(&path, &format!("\"{word}\"")));
                assert!(load(&paths).is_ok(), "{path:?} takes {word}");
                assert_eq!(
                    errors_of(&doc_with(&path, &format!("\"{word}\""))),
                    Vec::<String>::new()
                );
            }
            let table: toml::Table = toml::from_str(&doc_with(&path, "\"nonsense\"")).unwrap();
            let serde_said = toml::Value::Table(table)
                .try_into::<Config>()
                .unwrap_err()
                .to_string();
            // "expected one of `a`, `b`, `c`", or "expected `a` or `b`" for two, on the first line.
            let first = serde_said
                .lines()
                .next()
                .and_then(|line| line.split("expected ").nth(1))
                .unwrap_or_else(|| panic!("{path:?}: {serde_said}"));
            let listed: Vec<&str> = first.split('`').skip(1).step_by(2).collect();
            let mut known: Vec<&str> = key.allowed.iter().chain(key.aliases).copied().collect();
            let mut listed = listed;
            known.sort_unstable();
            listed.sort_unstable();
            assert_eq!(listed, known, "{path:?}");
        }
    }

    /// `load` (what every verb reads the config with) says it in the same plain words.
    #[test]
    fn loading_a_config_with_a_wrong_enum_says_it_in_plain_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_config(dir.path(), "[keys]\npreset = \"vim\"\n");
        let said = format!("{:#}", load(&paths).unwrap_err());
        assert!(
            said.contains("[keys] preset: \"vim\" is not one of modal, search"),
            "{said}"
        );
    }

    #[test]
    fn defaults_match_spec() {
        let config = Config::default();

        assert_eq!(config.ui.toast.delivery, ToastDelivery::Herdr);
        assert_eq!(config.ui.toast.delay_seconds, 1.0);
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSort::Attention);
        assert_eq!(config.ui.status_indicators, StatusIndicators::Symbols);
        assert_eq!(config.ui.sidebar_width, 26);
        assert!(config.ui.confirm_close);
        assert_eq!(
            config.ui.sidebar.agents.rows,
            vec![
                vec![
                    RowToken::bare("state_icon"),
                    RowToken::bare("workspace"),
                    RowToken::bare("tab"),
                ],
                vec![RowToken::bare("agent")],
            ]
        );
        assert_eq!(
            config.ui.sidebar.spaces.rows,
            vec![
                vec![RowToken::bare("state_icon"), RowToken::bare("workspace")],
                vec![RowToken::bare("branch"), RowToken::bare("git_status")],
            ]
        );
        assert_eq!(config.ui.sound.agents.get("droid"), Some(&SoundMode::Off));
        assert!(config.session.resume_agents_on_restore);
        assert_eq!(config.plugins.compat_herdr_version, "0.9.3");
        assert_eq!(
            config.worktrees.directory,
            expand_tilde("~/.tmux-agent-wizard/worktrees")
        );
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_for(tmp.path());

        let config = load(&paths).unwrap();

        assert_eq!(config, Config::default());
        assert!(check(&paths).is_empty());
    }

    #[test]
    fn default_toml_parses_with_zero_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), DEFAULT_TOML);

        let diagnostics = check(&paths);

        assert!(
            diagnostics.is_empty(),
            "expected no diagnostics, got: {diagnostics:#?}"
        );
        load(&paths).expect("DEFAULT_TOML should also deserialize cleanly");
    }

    /// `taw config default` prints this file for users to copy, so its comments must
    /// describe the popup that ships. The filter has no "a number alone is that session" rule:
    /// a number key is what moves the cursor to a session.
    #[test]
    fn the_default_configs_labels_comment_describes_only_the_number_keys() {
        let labels = DEFAULT_TOML
            .split("\n\n")
            .find(|block| block.contains("# labels = true"))
            .expect("a comment block for [ui] labels");
        assert!(
            labels.contains("moves the cursor to that session"),
            "{labels}"
        );
        assert!(!DEFAULT_TOML.contains("filters to it"), "{labels}");
    }

    #[test]
    fn rows_accept_strings_and_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r##"
[ui.sidebar.agents]
rows = [["state_icon", { token = "workspace", fg = "#89b4fa", bold = true }], ["agent"]]
"##,
        );

        let config = load(&paths).unwrap();

        let rows = &config.ui.sidebar.agents.rows;
        assert_eq!(rows[0][0], RowToken::bare("state_icon"));
        assert_eq!(
            rows[0][1],
            RowToken {
                token: "workspace".to_string(),
                fg: Some("#89b4fa".to_string()),
                bold: Some(true),
                dim: None,
                rules: Vec::new(),
            }
        );
        assert_eq!(rows[1], vec![RowToken::bare("agent")]);
    }

    /// A styled token's `rules` are read, in order, with their conditions and what each
    /// one does (herdr v0.9.1, `configuration.mdx`, "Text-valued tokens also accept up to 16
    /// ordered `rules`").
    #[test]
    fn a_token_reads_its_rules_in_order() {
        let config: Config = toml::from_str(
            r##"
[ui.sidebar.agents]
rows = [[{ token = "$load", fg = "#fff", rules = [
  { gt = 80, fg = "#f55", bold = true },
  { gt = 50, fg = "#fc0" },
  { equals = "Local", ignore_case = true, hide = true },
  { contains = "x", dim = true },
  { starts_with = "y" },
] }]]
"##,
        )
        .unwrap();

        let token = &config.ui.sidebar.agents.rows[0][0];

        assert_eq!(token.token, "$load");
        assert_eq!(token.fg.as_deref(), Some("#fff"));
        assert_eq!(token.rules.len(), 5);
        let shown: Vec<String> = token.rules.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                r##"{ gt = 80.0, fg = "#f55", bold = true }"##,
                r##"{ gt = 50.0, fg = "#fc0" }"##,
                r#"{ equals = "Local", ignore_case = true, hide = true }"#,
                r#"{ contains = "x", dim = true }"#,
                r#"{ starts_with = "y" }"#,
            ]
        );
    }

    /// A bare token, and a styled one with no `rules`, have none.
    #[test]
    fn a_token_without_rules_has_none() {
        let config: Config = toml::from_str(
            "[ui.sidebar.agents]\nrows = [[\"agent\", { token = \"workspace\", bold = true }]]\n",
        )
        .unwrap();

        for token in &config.ui.sidebar.agents.rows[0] {
            assert!(token.rules.is_empty(), "{token:?}");
        }
    }

    /// Rules the popup cannot use are config errors, found by `taw config check`, each one
    /// saying what is wrong (herdr: "Unknown conditions and malformed rules are rejected when
    /// loading config").
    #[test]
    fn a_malformed_rule_is_a_config_check_error() {
        let many = format!("rules = [{}]", vec!["{ equals = \"a\" }"; 17].join(", "));
        let cases: [(&str, String, &str); 13] = [
            (
                "two conditions",
                r#"{ token = "$load", rules = [{ equals = "a", contains = "b" }] }"#.to_string(),
                "exactly one of equals, contains, starts_with, gt, lt",
            ),
            (
                "no condition",
                r##"{ token = "$load", rules = [{ fg = "#fff" }] }"##.to_string(),
                "exactly one of equals, contains, starts_with, gt, lt",
            ),
            (
                "an unknown condition",
                r#"{ token = "$load", rules = [{ regex = "a" }] }"#.to_string(),
                "unknown field `regex`",
            ),
            (
                "an infinite threshold",
                r#"{ token = "$load", rules = [{ gt = inf }] }"#.to_string(),
                "threshold must be finite",
            ),
            (
                "a threshold that is not a number",
                r#"{ token = "$load", rules = [{ lt = nan }] }"#.to_string(),
                "threshold must be finite",
            ),
            (
                "ignore_case on a number",
                r#"{ token = "$load", rules = [{ gt = 5, ignore_case = true }] }"#.to_string(),
                "ignore_case applies only to sidebar text conditions",
            ),
            (
                "a condition of the wrong type",
                r#"{ token = "$load", rules = [{ equals = 5 }] }"#.to_string(),
                "expected a string",
            ),
            (
                "a colour that is not one",
                r#"{ token = "$load", rules = [{ equals = "a", fg = "nope" }] }"#.to_string(),
                "sidebar rule fg",
            ),
            (
                "rules that are not a list",
                r#"{ token = "$load", rules = "x" }"#.to_string(),
                "expected a sequence",
            ),
            (
                "seventeen rules",
                format!("{{ token = \"$load\", {many} }}"),
                "at most 16 rules",
            ),
            (
                "rules on state_icon",
                r#"{ token = "state_icon", rules = [{ equals = "a" }] }"#.to_string(),
                "sidebar rules require a text-valued token",
            ),
            (
                "rules on git_status",
                r#"{ token = "git_status", rules = [{ equals = "a" }] }"#.to_string(),
                "sidebar rules require a text-valued token",
            ),
            (
                "a hide that is not a boolean",
                r#"{ token = "$load", rules = [{ equals = "a", hide = "yes" }] }"#.to_string(),
                "expected a boolean",
            ),
        ];
        for (what, token, message) in cases {
            for table in ["agents", "spaces"] {
                let tmp = tempfile::tempdir().unwrap();
                let paths = write_config(
                    tmp.path(),
                    &format!("[ui.sidebar.{table}]\nrows = [[{token}]]\n"),
                );

                let errors = error_messages(&check(&paths)).join("\n");

                assert!(
                    errors.contains(message),
                    "{what} in [ui.sidebar.{table}]: {message:?} not in {errors:?}"
                );
                assert!(load(&paths).is_err(), "{what}: load must refuse it too");
            }
        }
    }

    /// Sixteen rules are the most a token takes, and every kind of condition is accepted
    /// with the style fields it may carry.
    #[test]
    fn sixteen_well_formed_rules_are_a_clean_config() {
        let sixteen = vec![r#"{ contains = "a", bold = false, dim = true, hide = false }"#; 16];
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            &format!(
                "[ui.sidebar.agents]\nrows = [[{{ token = \"$load\", rules = [{}] }}, \"agent\"], [{{ token = \"branch\", rules = [{{ equals = \"main\", ignore_case = true, fg = \"red\" }}, {{ gt = 1e2 }}, {{ lt = -3 }}] }}]]\n",
                sixteen.join(", ")
            ),
        );

        assert!(check(&paths).is_empty(), "{:?}", check(&paths));
        assert_eq!(
            load(&paths).unwrap().ui.sidebar.agents.rows[0][0]
                .rules
                .len(),
            16
        );
    }

    #[test]
    fn bad_delivery_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui.toast]
delivery = "not-a-real-delivery"
"#,
        );

        let diagnostics = check(&paths);

        assert!(diagnostics.iter().any(|d| d.level == Level::Error));
        assert!(load(&paths).is_err());
    }

    #[test]
    fn tilde_is_expanded_in_worktrees_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[worktrees]
directory = "~/my-worktrees"
"#,
        );

        let config = load(&paths).unwrap();

        let expected = dirs::home_dir().unwrap().join("my-worktrees");
        assert_eq!(config.worktrees.directory, expected);
    }

    #[test]
    fn partial_table_falls_back_to_field_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui]
sidebar_width = 30
"#,
        );

        let config = load(&paths).unwrap();

        assert_eq!(config.ui.sidebar_width, 30);
        assert_eq!(config.ui.agent_panel_sort, AgentPanelSort::Attention);
        assert_eq!(config.ui.sound.agents.get("droid"), Some(&SoundMode::Off));
        assert!(check(&paths).is_empty());
    }

    #[test]
    fn partial_agents_row_table_keeps_agents_default_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui.sidebar.agents]
row_gap = 1
"#,
        );

        let config = load(&paths).unwrap();

        assert_eq!(config.ui.sidebar.agents.row_gap, 1);
        assert_eq!(
            config.ui.sidebar.agents.rows,
            Sidebar::default().agents.rows
        );
        assert!(config.ui.sidebar.agents.rows_by_agent.is_empty());
        assert!(check(&paths).is_empty());
    }

    #[test]
    fn partial_spaces_row_table_keeps_spaces_default_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui.sidebar.spaces]
row_gap = 1
"#,
        );

        let config = load(&paths).unwrap();

        assert_eq!(config.ui.sidebar.spaces.row_gap, 1);
        assert_eq!(
            config.ui.sidebar.spaces.rows,
            Sidebar::default().spaces.rows
        );
        assert!(config.ui.sidebar.spaces.rows_by_agent.is_empty());
        assert!(check(&paths).is_empty());
    }

    #[test]
    fn rows_by_agent_override_keeps_agents_default_rows_alongside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui.sidebar.agents.rows_by_agent]
claude = [["agent"]]
"#,
        );

        let config = load(&paths).unwrap();

        assert_eq!(
            config.ui.sidebar.agents.rows,
            Sidebar::default().agents.rows
        );
        assert_eq!(
            config.ui.sidebar.agents.rows_by_agent.get("claude"),
            Some(&vec![vec![RowToken::bare("agent")]])
        );
        assert!(check(&paths).is_empty());
    }

    #[test]
    fn unknown_key_is_a_warning_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[server]
headless_cols = 120
"#,
        );

        let diagnostics = check(&paths);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, Level::Warning);
        assert!(diagnostics[0].message.contains("server.headless_cols"));
        assert!(load(&paths).is_ok());
    }

    #[test]
    fn config_check_says_what_the_machines_table_drops_in_the_words_of_the_machine_module() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[machines]\n\"Studio\" = \"box.local\"\nattic = \"\"\npi = \"pi\"\nloft = \"pi\"\n\
             [machines.den]\nssh = \"den\"\nport = 22\n",
        );

        let found = check(&paths);

        assert_eq!(
            warnings(&found),
            [
                "machines.Studio: a machine's name is lowercase letters, digits, - and _, starting with a letter, at most 32 characters",
                "machines.attic: no ssh destination (what you type after ssh)",
                "machines.loft: the same ssh destination as machines.pi (pi)",
                "unknown config key: machines.den.port",
            ]
        );
        assert!(error_messages(&found).is_empty());
        // The file still loads, as one with a bad [sandbox] line does.
        assert!(load(&paths).is_ok());
    }

    #[test]
    fn config_check_has_nothing_to_say_about_a_good_machines_table() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[machines]\nstudio = \"studio.local\"\n[machines.pi]\nssh = \"pi\"\ntaw = \"~/.local/bin/taw\"\n",
        );

        assert!(check(&paths).is_empty());
    }

    #[test]
    fn config_check_still_warns_about_herdrs_remote_and_server_tables_and_they_still_load() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[machines]\nstudio = \"studio.local\"\n[remote]\nhost = \"x\"\n[server]\nheadless_cols = 120\n",
        );

        let found = check(&paths);

        assert_eq!(
            warnings(&found),
            [
                "unknown config key: remote.host",
                "unknown config key: server.headless_cols",
            ]
        );
        assert!(load(&paths).is_ok());
    }

    #[test]
    fn unknown_theme_name_is_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[theme]
name = "solarized-void"
"#,
        );

        let diagnostics = check(&paths);

        assert!(
            diagnostics.iter().any(|d| d.level == Level::Warning
                && d.message.contains("solarized-void")
                && d.message.contains("theme")),
            "expected a warning about the unknown theme name, got: {diagnostics:#?}"
        );
        // An unknown name falls back to the default; it is not a hard error.
        assert!(load(&paths).is_ok());
    }

    /// With `auto_switch` on, a `dark_name` or a `light_name` that no theme has is a warning that names the key
    /// and the value, as an unknown `name` is: the terminal's mode would otherwise draw the default theme and
    /// say nothing. Each key alone, and with `auto_switch` off the key is not read, so nothing is said.
    #[test]
    fn unknown_dark_or_light_name_is_a_warning_with_auto_switch_on() {
        let said = |text: &str| -> Vec<String> {
            let tmp = tempfile::tempdir().unwrap();
            let paths = write_config(tmp.path(), text);
            check(&paths).into_iter().map(|d| d.message).collect()
        };
        let dark = said("[theme]\nauto_switch = true\ndark_name = \"draculla\"\n");
        assert_eq!(dark.len(), 1, "{dark:?}");
        assert!(dark[0].contains("dark_name 'draculla'"), "{dark:?}");
        let light = said("[theme]\nauto_switch = true\nlight_name = \"solarised\"\n");
        assert_eq!(light.len(), 1, "{light:?}");
        assert!(light[0].contains("light_name 'solarised'"), "{light:?}");
        let both =
            said("[theme]\nauto_switch = true\ndark_name = \"dracula\"\nlight_name = \"nord\"\n");
        assert_eq!(both, Vec::<String>::new());
        let off = said("[theme]\ndark_name = \"draculla\"\nlight_name = \"solarised\"\n");
        assert_eq!(off, Vec::<String>::new());
    }

    #[test]
    fn unparseable_custom_theme_colour_is_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[theme.custom]
accent = "not-a-colour"
"#,
        );

        let diagnostics = check(&paths);

        assert!(
            diagnostics.iter().any(|d| d.level == Level::Warning
                && d.message.contains("accent")
                && d.message.contains("colour")),
            "expected a warning about the unparseable custom colour, got: {diagnostics:#?}"
        );
        assert!(load(&paths).is_ok());
    }

    #[test]
    fn parse_error_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), "this is not valid toml =====");

        let diagnostics = check(&paths);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].level, Level::Error);
        assert!(load(&paths).is_err());
    }

    #[test]
    fn agent_panel_sort_workspaces_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[ui]
agent_panel_sort = "workspaces"
"#,
        );

        let config = load(&paths).unwrap();

        assert_eq!(config.ui.agent_panel_sort, AgentPanelSort::Sessions);
    }

    #[test]
    fn new_defaults_match_the_documented_defaults() {
        let c = Config::default();
        assert_eq!(c.ui.agent_panel_sort, AgentPanelSort::Attention);
        assert_eq!(c.ui.status_indicators, StatusIndicators::Symbols);
        // The popup opens on the agents, not on every shell too.
        assert_eq!(c.ui.depth, Depth::Agents);
        assert_eq!(Depth::default(), Depth::Agents);
        assert_eq!(c.ui.layout, Layout::Tree);
        assert!(c.ui.preview && c.ui.show_git);
        assert_eq!(c.ui.toast.done_delay_seconds, 5.0);
        assert_eq!(c.keys.preset, KeysPreset::Modal);
        assert_eq!(c.sessions.name, NameMode::Directory);
        assert!(c.sessions.zoxide && c.sessions.project_dirs.is_empty());
        assert!(c.detection.supplements);
    }

    #[test]
    fn labels_and_titles_default_on_and_parse() {
        let cfg = Config::default();
        assert!(cfg.ui.labels);
        assert!(cfg.ui.titles);
        let cfg: Config = toml::from_str("[ui]\nlabels = false\ntitles = false\n").unwrap();
        assert!(!cfg.ui.labels);
        assert!(!cfg.ui.titles);
    }

    /// The toggle for the filter that also lists directories to open: it is
    /// off until asked for, so a config that never heard of it gets the popup it always had.
    #[test]
    fn projects_in_filter_is_off_unless_the_file_turns_it_on() {
        assert!(!Config::default().ui.projects_in_filter);
        let unset: Config = toml::from_str("[ui]\nlabels = false\n").unwrap();
        assert!(!unset.ui.projects_in_filter);
        let on: Config = toml::from_str("[ui]\nprojects_in_filter = true\n").unwrap();
        assert!(on.ui.projects_in_filter);
        let off: Config = toml::from_str("[ui]\nprojects_in_filter = false\n").unwrap();
        assert!(!off.ui.projects_in_filter);
    }

    #[test]
    fn projects_in_filter_is_a_known_key() {
        let diags = check_unknown_keys(
            &toml::from_str::<toml::Table>("[ui]\nprojects_in_filter = true\n").unwrap(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    /// `taw config default` prints the file people copy from, so the key is in it, commented
    /// out at its default, and says what turning it on does.
    #[test]
    fn the_default_config_describes_projects_in_filter_and_leaves_it_off() {
        let block = DEFAULT_TOML
            .split("\n\n")
            .find(|block| block.contains("# projects_in_filter = false"))
            .expect("a comment block for [ui] projects_in_filter");
        for said in ["`/` filter", "directories", "Enter"] {
            assert!(block.contains(said), "{said:?} is not in:\n{block}");
        }
        assert!(
            !DEFAULT_TOML.contains("\nprojects_in_filter"),
            "the shipped default leaves the key commented out"
        );
    }

    #[test]
    fn herdr_sort_values_are_accepted_as_aliases() {
        let c: Config = toml::from_str("[ui]\nagent_panel_sort = \"priority\"\n").unwrap();
        assert_eq!(c.ui.agent_panel_sort, AgentPanelSort::Attention);
        for v in ["spaces", "workspaces", "sessions"] {
            let c: Config = toml::from_str(&format!("[ui]\nagent_panel_sort = \"{v}\"\n")).unwrap();
            assert_eq!(c.ui.agent_panel_sort, AgentPanelSort::Sessions, "{v}");
        }
    }

    /// Follows herdr's `resume_agents_on_restore_defaults_on_and_parses`
    /// (`config/model.rs`, commit `46a5091838` (#4487, #4102 in v0.9.2's changelog)): restored agents start
    /// `startup_per_agent_delay_ms` apart (default 100), zero disables the spacing.
    #[test]
    fn session_table_parses_startup_per_agent_delay_ms() {
        let default_config = Config::default();
        assert!(default_config.session.resume_agents_on_restore);
        assert_eq!(default_config.session.startup_per_agent_delay_ms, 100);

        let c: Config = toml::from_str(
            "[session]\nresume_agents_on_restore = false\nstartup_per_agent_delay_ms = 0\n",
        )
        .unwrap();
        assert!(!c.session.resume_agents_on_restore);
        assert_eq!(c.session.startup_per_agent_delay_ms, 0);

        let diags = check_unknown_keys(
            &toml::from_str::<toml::Table>("[session]\nstartup_per_agent_delay_ms = 250\n")
                .unwrap(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn sessions_table_and_its_aliases_parse() {
        let c: Config = toml::from_str(
            "[sessions]\nname = \"full-path\"\nzoxide = false\nproject_dirs = [\"~/dev\"]\npost_create_hook = \"~/hook.sh\"\n[ui.sidebar.sessions]\nrow_gap = 1\n",
        )
        .unwrap();
        assert_eq!(c.sessions.name, NameMode::FullPath);
        assert!(!c.sessions.zoxide);
        assert_eq!(
            c.sessions.project_dirs[0],
            dirs::home_dir().unwrap().join("dev")
        );
        assert_eq!(c.ui.sidebar.spaces.row_gap, 1); // written as [ui.sidebar.sessions]
        // A hook is a command line: kept as written, its `~` expanded when it is run.
        assert_eq!(c.sessions.post_create_hook.as_deref(), Some("~/hook.sh"));
        assert_eq!(c.sessions.pre_create_hook, None);
        let diags = check_unknown_keys(
            &toml::from_str::<toml::Table>(
                "[sessions]\nname = \"directory\"\n[keys]\nrename = \"R\"\n[detection]\nsupplements = false\n",
            )
            .unwrap(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn the_hook_timeout_is_ten_seconds_unless_the_file_says_otherwise() {
        assert_eq!(Config::default().sessions.hook_timeout_seconds, 10.0);
        let unset: Config = toml::from_str("[sessions]\nzoxide = false\n").unwrap();
        assert_eq!(unset.sessions.hook_timeout_seconds, 10.0);

        for (written, seconds) in [("5", 5.0), ("2.5", 2.5), ("0", 0.0), ("3600", 3600.0)] {
            let c: Config =
                toml::from_str(&format!("[sessions]\nhook_timeout_seconds = {written}\n")).unwrap();
            assert_eq!(c.sessions.hook_timeout_seconds, seconds, "{written}");
        }
    }

    #[test]
    fn the_hook_timeout_is_a_known_key() {
        let diags = check_unknown_keys(
            &toml::from_str::<toml::Table>("[sessions]\nhook_timeout_seconds = 10\n").unwrap(),
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn a_hook_timeout_out_of_range_is_an_error_and_one_in_range_is_not() {
        for value in ["-1", "3601", "nan", "inf"] {
            let tmp = tempfile::tempdir().unwrap();
            let paths = write_config(
                tmp.path(),
                &format!("[sessions]\nhook_timeout_seconds = {value}\n"),
            );

            let diagnostics = check(&paths);

            assert_eq!(
                error_messages(&diagnostics),
                ["sessions.hook_timeout_seconds must be between 0 and 3600 (0 means no limit)"],
                "{value}: {diagnostics:#?}"
            );
        }
        for value in ["0", "0.5", "10", "3600"] {
            let tmp = tempfile::tempdir().unwrap();
            let paths = write_config(
                tmp.path(),
                &format!("[sessions]\nhook_timeout_seconds = {value}\n"),
            );

            let diagnostics = check(&paths);

            assert!(diagnostics.is_empty(), "{value}: {diagnostics:#?}");
        }
    }

    /// `taw config default` prints this file for users to copy: the hook timeout has to be
    /// in it, under `[sessions]`, with the default the code has and what `0` does.
    #[test]
    fn the_default_config_documents_the_hook_timeout_under_sessions() {
        let sessions = DEFAULT_TOML
            .split("\n[sessions]\n")
            .nth(1)
            .and_then(|rest| rest.split("\n[worktrees]").next())
            .expect("a [sessions] table in the default config");

        assert!(
            sessions.contains("\n# hook_timeout_seconds = 10\n"),
            "{sessions}"
        );
        assert!(sessions.contains("0 means no limit"), "{sessions}");
        assert_eq!(Sessions::default().hook_timeout_seconds, 10.0);
    }

    // --- [hooks] -----------------------------------------------------------------------

    fn hooks_of(toml: &str) -> Hooks {
        toml::from_str::<Config>(toml)
            .expect("the test config must parse")
            .hooks
    }

    /// Warnings and errors `taw config check` gives `toml`, as `(level, message)`.
    fn checked(toml: &str) -> Vec<(Level, String)> {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), toml);
        check(&paths)
            .into_iter()
            .map(|d| (d.level, d.message))
            .collect()
    }

    #[test]
    fn no_hooks_are_configured_until_the_file_says_so_and_a_hook_may_run_thirty_seconds() {
        let hooks = Config::default().hooks;
        assert_eq!(hooks.timeout_seconds, 30.0);
        assert!(hooks.configured().next().is_none());
        assert!(hooks.commands("agent.done").is_empty());
        // A `[hooks]` table with no limit in it keeps the default too.
        assert_eq!(
            hooks_of("[hooks]\n\"agent.done\" = \"true\"\n").timeout_seconds,
            30.0
        );
        assert_eq!(
            hooks_of("[sessions]\nzoxide = false\n").timeout_seconds,
            30.0
        );
    }

    #[test]
    fn a_hook_is_a_command_line_or_a_list_of_them_kept_as_written() {
        let hooks = hooks_of(
            r#"
[hooks]
"worktree.created" = "~/bin/setup-worktree"
"agent.done" = ["~/bin/log-finished", "say done"]
"notification" = "~/bin/push --topic 'my topic'"
"#,
        );
        assert_eq!(hooks.commands("worktree.created"), ["~/bin/setup-worktree"]);
        assert_eq!(
            hooks.commands("agent.done"),
            ["~/bin/log-finished", "say done"]
        );
        assert_eq!(
            hooks.commands("notification"),
            ["~/bin/push --topic 'my topic'"]
        );
        assert!(hooks.commands("agent.idle").is_empty());
        let names: Vec<&str> = hooks.configured().map(|(name, _)| name).collect();
        assert_eq!(names, ["agent.done", "notification", "worktree.created"]);
    }

    #[test]
    fn an_event_written_as_a_dotted_key_or_a_subtable_is_the_same_name() {
        // TOML reads `worktree.created = ...` under `[hooks]` as a table inside a table,
        // which is not what the person meant: it reads as the name they wrote.
        let hooks = hooks_of(
            r#"
[hooks]
worktree.created = "one"
"agent.blocked" = "two"

[hooks.agent]
done = ["three"]
"#,
        );
        assert_eq!(hooks.commands("worktree.created"), ["one"]);
        assert_eq!(hooks.commands("agent.blocked"), ["two"]);
        assert_eq!(hooks.commands("agent.done"), ["three"]);
    }

    #[test]
    fn the_two_hooks_that_can_refuse_are_read_like_any_other_key() {
        let hooks = hooks_of(
            r#"
[hooks]
pre_close = "~/bin/keep-agents-open"
pre_worktree_remove = ["~/bin/is-merged", "~/bin/is-pushed"]
"#,
        );
        assert_eq!(hooks.commands("pre_close"), ["~/bin/keep-agents-open"]);
        assert_eq!(
            hooks.commands("pre_worktree_remove"),
            ["~/bin/is-merged", "~/bin/is-pushed"]
        );
    }

    #[test]
    fn a_blank_command_is_no_hook_as_it_is_for_the_create_hooks() {
        let hooks = hooks_of(
            r#"
[hooks]
"agent.done" = ""
"agent.idle" = ["   ", "echo idle", ""]
"#,
        );
        assert!(hooks.commands("agent.done").is_empty());
        assert_eq!(hooks.commands("agent.idle"), ["echo idle"]);
    }

    #[test]
    fn the_time_limit_is_a_number_of_seconds_and_zero_is_none() {
        for (written, seconds) in [("5", 5.0), ("2.5", 2.5), ("0", 0.0), ("3600", 3600.0)] {
            let hooks = hooks_of(&format!("[hooks]\ntimeout_seconds = {written}\n"));
            assert_eq!(hooks.timeout_seconds, seconds, "{written}");
        }
        // The limit is not an event: it is not offered to a hook as a command.
        let hooks = hooks_of("[hooks]\ntimeout_seconds = 5\n\"agent.done\" = \"true\"\n");
        assert!(hooks.commands("timeout_seconds").is_empty());
        assert_eq!(hooks.configured().count(), 1);
    }

    #[test]
    fn a_time_limit_out_of_range_is_an_error_and_one_in_range_is_not() {
        for value in ["-1", "3601", "nan", "inf"] {
            let found = checked(&format!("[hooks]\ntimeout_seconds = {value}\n"));
            let errors: Vec<&str> = found
                .iter()
                .filter(|(level, _)| *level == Level::Error)
                .map(|(_, message)| message.as_str())
                .collect();
            assert_eq!(
                errors,
                ["hooks.timeout_seconds must be between 0 and 3600 (0 means no limit)"],
                "{value}: {found:#?}"
            );
        }
        for value in ["0", "0.5", "30", "3600"] {
            let found = checked(&format!("[hooks]\ntimeout_seconds = {value}\n"));
            assert!(found.is_empty(), "{value}: {found:#?}");
        }
    }

    #[test]
    fn every_event_and_both_vetoes_are_known_keys() {
        let mut text = String::from("[hooks]\ntimeout_seconds = 30\n");
        for event in crate::hooks::events::EVENTS
            .iter()
            .chain(crate::hooks::events::VETOES)
        {
            text.push_str(&format!("\"{}\" = \"true\"\n", event.name));
        }
        let found = checked(&text);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn an_event_taw_does_not_fire_is_a_warning_naming_it_and_the_file_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r#"
[hooks]
"agent.finished" = "true"
"worktree.create" = ["true"]
"agent.done" = "true"
"#,
        );

        let diagnostics = check(&paths);

        let warnings: Vec<&str> = diagnostics
            .iter()
            .filter(|d| d.level == Level::Warning)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            warnings,
            [
                "unknown hook event: hooks.agent.finished (run `taw hooks list` for the names taw fires)",
                "unknown hook event: hooks.worktree.create (run `taw hooks list` for the names taw fires)",
            ],
            "{diagnostics:#?}"
        );
        assert!(load(&paths).is_ok());
    }

    #[test]
    fn a_hook_that_is_neither_a_command_nor_a_list_of_them_is_a_warning() {
        let found = checked(
            r#"
[hooks]
"agent.done" = 5
"agent.idle" = ["true", 3]
"notification" = { command = "true" }
"agent.working" = ["true"]
"#,
        );
        let messages: Vec<&str> = found.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(
            messages,
            [
                "hooks.agent.done: expected a command or a list of commands",
                "hooks.agent.idle: expected a command or a list of commands",
                // A table is read as a dotted name, so this is `notification.command`.
                "unknown hook event: hooks.notification.command (run `taw hooks list` for the names taw fires)",
            ]
        );
        assert!(found.iter().all(|(level, _)| *level == Level::Warning));
    }

    #[test]
    fn the_default_config_documents_the_hooks_table() {
        let hooks = DEFAULT_TOML
            .split("\n[hooks]\n")
            .nth(1)
            .and_then(|rest| rest.split("\n[detection]").next())
            .expect("a [hooks] table in the default config");

        for needle in [
            "\n# timeout_seconds = 30\n",
            "0 means no limit",
            "# \"worktree.created\" = ",
            "# \"agent.done\" = [",
            "# \"notification\" = ",
            "# pre_close = ",
            "# pre_worktree_remove = ",
            "taw hooks test",
            "[sessions] hook_timeout_seconds",
        ] {
            assert!(hooks.contains(needle), "missing {needle:?} in:\n{hooks}");
        }
        assert_eq!(Hooks::default().timeout_seconds, 30.0);
    }

    #[test]
    fn the_sandbox_table_names_a_profile_and_one_for_an_agent() {
        let cfg: Config =
            toml::from_str("[sandbox]\nprofile = \"max\"\n[sandbox.agents]\ncodex = \"default\"\n")
                .unwrap();
        assert_eq!(cfg.sandbox.profile.as_deref(), Some("max"));
        assert_eq!(
            cfg.sandbox.agents.get("codex").map(String::as_str),
            Some("default")
        );
        assert_eq!(Config::default().sandbox, Sandbox::default());
    }

    #[test]
    fn a_sandbox_key_taw_does_not_know_is_a_warning_and_its_agents_table_takes_any_name() {
        let table: toml::Table =
            toml::from_str("[sandbox]\nprofil = \"x\"\n[sandbox.agents]\nanything = \"y\"\n")
                .unwrap();
        let messages: Vec<String> = check_unknown_keys(&table)
            .into_iter()
            .map(|d| d.message)
            .collect();
        assert_eq!(messages, ["unknown config key: sandbox.profil"]);
        assert!(
            check_unknown_keys(&toml::from_str("[sandbox]\nprofile = \"x\"\n").unwrap()).is_empty()
        );
    }

    /// What `taw sandbox` picks a profile by: the config read whole, or why taw cannot tell which
    /// profile you chose. No file is the defaults; a file that does not load says why on one line; a
    /// line of `[sandbox]` taw drops is said in config check's words; a file that loads with a
    /// `[sandbox]` taw reads whole is its config, a profile it names that is not there and an error
    /// elsewhere in the file included (taw sandbox refuses the first in words of its own, when it is
    /// the one picked).
    #[test]
    fn taw_sandbox_reads_the_config_whole_or_says_why_it_cannot_tell_your_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_for(tmp.path());
        assert_eq!(load_for_sandbox(&paths), Ok(Config::default()));

        let file = paths.config_file.display().to_string();
        for broken in [
            "[sandbox\nprofile = \"max\"\n",
            "[sandbox]\nprofile = 5\n",
            "[sandbox]\nprofile = \"max\"\n[sandbox]\nprofile = \"min\"\n",
            "[sandbox.agents]\ncodex = [\"max\"]\n",
        ] {
            std::fs::write(&paths.config_file, broken).unwrap();
            match load_for_sandbox(&paths) {
                Err(SandboxDoubt::NotLoaded { file: said, why }) => {
                    assert_eq!(said, file, "{broken:?}");
                    assert!(!why.is_empty(), "{broken:?}");
                    assert!(!why.contains('\n'), "{broken:?}: {why:?}");
                }
                other => panic!("{broken:?}: {other:?}"),
            }
        }

        for (dropped, line) in [
            (
                "[sandbox]\nprofil = \"max\"\n",
                "unknown config key: sandbox.profil",
            ),
            (
                "[sandbox]\nagent = \"codex\"\n",
                "unknown config key: sandbox.agent",
            ),
            (
                "[sandbox.agents]\nclaud = \"max\"\n",
                "sandbox.agents.claud: taw knows no agent called claud (taw manifest list names them)",
            ),
            (
                "[sandbox.agents]\ncursor-agent = \"max\"\n",
                "sandbox.agents.cursor-agent: taw calls this agent cursor, so its line is cursor = \"max\"",
            ),
        ] {
            std::fs::write(&paths.config_file, dropped).unwrap();
            assert_eq!(
                load_for_sandbox(&paths),
                Err(SandboxDoubt::Dropped {
                    file: file.clone(),
                    line: line.to_string()
                }),
                "{dropped:?}"
            );
        }

        sandbox_file(tmp.path(), "max", 0o755);
        std::fs::write(
            &paths.config_file,
            "[sandbox]\nprofile = \"team\"\n[sandbox.agents]\ncodex = \"max\"\n[ui.toast]\ndelay_seconds = 9000\n",
        )
        .unwrap();
        let cfg = load_for_sandbox(&paths).unwrap();
        assert_eq!(cfg.sandbox.profile.as_deref(), Some("team"));
        assert_eq!(
            cfg.sandbox.agents.get("codex").map(String::as_str),
            Some("max")
        );
    }

    #[test]
    fn the_default_config_has_the_specs_sandbox_block() {
        assert!(DEFAULT_TOML.contains(
            "[sandbox]\n\
             # The profile `taw sandbox` uses when you name none: one of yours in\n\
             # ~/.config/tmux-agent-wizard/sandbox/, or \"default\", taw's own, which is\n\
             # what it uses while this is unset. Nothing here changes the popup: it shows\n\
             # \u{1f4e6} on an agent in a container either way.\n\
             # profile = \"default\"\n\
             \n\
             # A profile for one agent, used when you name none for it.\n\
             [sandbox.agents]\n\
             # codex = \"default\"\n"
        ));
    }

    #[test]
    fn the_default_config_has_the_specs_machines_block() {
        assert!(DEFAULT_TOML.contains(
            "# Other machines the app lists with this one, and the popup shows read-only while the app runs; with no app\n\
             # running, the popup names each one and says to run taw app outside tmux.\n\
             # Each line is a name you choose and an ssh destination: what you type after `ssh`, a Host from\n\
             # your ~/.ssh/config or user@host. taw has to be installed on each. taw keeps no key, password\n\
             # or host key: ssh does all of that, and taw never answers a prompt, so `ssh <destination>` must\n\
             # work in a terminal without asking anything (the host key accepted, the key in ssh-agent).\n\
             # [machines]\n\
             # studio = \"studio.local\"\n\
             # pi = \"pi\"\n\
             #\n\
             # Only when taw is not on the PATH that ssh gives a command on that machine:\n\
             # [machines.pi]\n\
             # ssh = \"pi\"\n\
             # taw = \"~/.local/bin/taw\"\n"
        ));
        // It sits after the [sandbox] block, and the whole file still reads as no machines at all.
        let sandbox = DEFAULT_TOML.find("[sandbox.agents]").unwrap();
        let machines = DEFAULT_TOML.find("# [machines]").unwrap();
        assert!(sandbox < machines);
        assert!(
            crate::machine::config::parse(DEFAULT_TOML)
                .machines
                .is_empty()
        );
        assert!(
            crate::machine::config::parse(DEFAULT_TOML)
                .dropped
                .is_empty()
        );
        assert!(toml::from_str::<Config>(DEFAULT_TOML).is_ok());
    }

    fn sandbox_file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let folder = dir.join("sandbox");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join(name);
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable_with_mode(&file, mode);
        file
    }

    fn warnings(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .filter(|d| d.level == Level::Warning)
            .map(|d| d.message.as_str())
            .collect()
    }

    #[test]
    fn config_check_names_a_sandbox_profile_that_is_not_there_and_an_agent_taw_does_not_know() {
        let tmp = tempfile::tempdir().unwrap();
        sandbox_file(tmp.path(), "max", 0o755);
        let paths = write_config(
            tmp.path(),
            "[sandbox]\nprofile = \"team\"\n[sandbox.agents]\ncodex = \"max\"\nclaud = \"max\"\ncursor-agent = \"max\"\ngemini = \"gone\"\n",
        );

        let found = check(&paths);

        let dir = format!("{}/", tmp.path().join("sandbox").display());
        assert_eq!(
            warnings(&found),
            [
                format!("sandbox.profile: no profile called team (yours: default, max, in {dir})"),
                "sandbox.agents.claud: taw knows no agent called claud (taw manifest list names them)"
                    .to_string(),
                "sandbox.agents.cursor-agent: taw calls this agent cursor, so its line is cursor = \"max\""
                    .to_string(),
                format!("sandbox.agents.gemini: no profile called gone (yours: default, max, in {dir})"),
            ]
        );
    }

    #[test]
    fn config_check_names_a_profile_file_that_is_not_executable_once_and_a_file_that_cannot_be_one()
    {
        let tmp = tempfile::tempdir().unwrap();
        let draft = sandbox_file(tmp.path(), "draft", 0o644);
        let named = sandbox_file(tmp.path(), "named", 0o644);
        sandbox_file(tmp.path(), "mine.sh", 0o755);
        sandbox_file(tmp.path(), ".DS_Store", 0o644);
        std::fs::create_dir_all(tmp.path().join("sandbox").join("old.d")).unwrap();
        let paths = write_config(tmp.path(), "[sandbox]\nprofile = \"named\"\n");

        let found = check(&paths);

        assert_eq!(
            warnings(&found),
            [
                format!(
                    "sandbox.profile: the profile named is not executable (chmod +x {})",
                    named.display()
                ),
                format!(
                    "sandbox: the profile draft is not executable (chmod +x {})",
                    draft.display()
                ),
                format!(
                    "sandbox: mine.sh in {}/ is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit), so taw sandbox cannot run it",
                    tmp.path().join("sandbox").display()
                ),
            ]
        );
    }

    /// A config.toml that is a link to a file that is gone (stow, after the dotfiles moved) is no
    /// config taw can read: taw sandbox cannot tell which profile you chose, and config check says
    /// so. A link to a file that is there is read as the file.
    #[test]
    fn a_config_that_is_a_link_to_a_file_that_is_gone_is_not_read_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_for(tmp.path());
        let dotfiles = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(
            dotfiles.join("config.toml"),
            "[sandbox]\nprofile = \"strict\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(dotfiles.join("config.toml"), &paths.config_file).unwrap();
        assert_eq!(
            load_for_sandbox(&paths).unwrap().sandbox.profile.as_deref(),
            Some("strict")
        );

        std::fs::rename(&dotfiles, tmp.path().join("dotfiles-moved")).unwrap();

        let file = paths.config_file.display().to_string();
        assert_eq!(
            load_for_sandbox(&paths),
            Err(SandboxDoubt::Gone { file: file.clone() })
        );
        assert_eq!(
            SandboxDoubt::Gone { file: file.clone() }.to_string(),
            format!("config {file} is a link to a file that is not there")
        );
        assert_eq!(
            warnings(&check(&paths)),
            [format!(
                "{file} is a link to a file that is not there, so taw reads no config (ls -l {file})"
            )
            .as_str()]
        );
    }

    #[test]
    fn config_check_is_quiet_about_a_sandbox_table_that_is_fine() {
        let tmp = tempfile::tempdir().unwrap();
        sandbox_file(tmp.path(), "max", 0o755);
        let paths = write_config(
            tmp.path(),
            "[sandbox]\nprofile = \"default\"\n[sandbox.agents]\ncodex = \"max\"\n",
        );

        assert!(check(&paths).is_empty(), "{:?}", check(&paths));
    }

    /// Your profiles folder is read whatever `config.toml` says: with no file at all, with one
    /// that cannot be read or is not TOML, and with one that does not load. The names the file's
    /// `[sandbox]` table gives are checked only when it loads, so a file it names is then said
    /// as the folder says it.
    #[test]
    fn config_check_reads_the_profiles_folder_with_no_config_file_and_with_one_that_does_not_load()
    {
        let tmp = tempfile::tempdir().unwrap();
        let draft = sandbox_file(tmp.path(), "draft", 0o644);
        let paths = paths_for(tmp.path());
        let said = format!(
            "sandbox: the profile draft is not executable (chmod +x {})",
            draft.display()
        );

        let found = check(&paths);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(warnings(&found), [said.as_str()]);

        for (what, broken) in [
            ("not toml", "this is = not = toml"),
            (
                "a value that does not load",
                "[sandbox]\nprofile = \"draft\"\n[ui.toast]\ndelivery = \"not-a-real-delivery\"\n",
            ),
        ] {
            std::fs::write(&paths.config_file, broken).unwrap();
            let found = check(&paths);
            assert_eq!(error_messages(&found).len(), 1, "{what}: {found:?}");
            assert_eq!(warnings(&found), [said.as_str()], "{what}");
        }

        std::fs::remove_file(&paths.config_file).unwrap();
        std::fs::create_dir(&paths.config_file).unwrap();
        let found = check(&paths);
        assert_eq!(
            error_messages(&found).len(),
            1,
            "a file it cannot read: {found:?}"
        );
        assert_eq!(warnings(&found), [said.as_str()], "a file it cannot read");
    }

    /// Two profiles whose names give one image name (lowercase, one dash for each run of dashes and
    /// underscores, as the default profile names its image) rebuild each other's image when their
    /// recipes differ: config check says to rename one. One file under two names is one profile.
    #[test]
    fn config_check_says_two_profiles_share_one_image_name() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["A-b", "a__b", "c-d", "e"] {
            sandbox_file(tmp.path(), name, 0o755);
        }
        let folder = tmp.path().join("sandbox");
        std::os::unix::fs::symlink(folder.join("c-d"), folder.join("c_d")).unwrap();
        let paths = write_config(tmp.path(), "");

        assert_eq!(
            warnings(&check(&paths)),
            [
                "sandbox: the profiles A-b and a__b share one image name, taw-sandbox-a-b, so a start of one can rebuild the other's: rename one of them"
            ]
        );
    }

    /// With a line for an agent and one for another spelling of it, writing the second under the
    /// first's name would give the table the key twice, which stops the whole file loading: config
    /// check says to keep one of the two.
    #[test]
    fn config_check_says_to_keep_one_of_two_lines_for_one_agent() {
        let tmp = tempfile::tempdir().unwrap();
        sandbox_file(tmp.path(), "max", 0o755);
        sandbox_file(tmp.path(), "zed", 0o755);
        let paths = write_config(
            tmp.path(),
            "[sandbox.agents]\ncursor = \"max\"\ncursor-agent = \"zed\"\n",
        );

        assert_eq!(
            warnings(&check(&paths)),
            [
                "sandbox.agents.cursor-agent: taw calls this agent cursor, and the table has a cursor line too: keep one of the two"
            ]
        );
    }

    /// An empty name is said as one, never as a hole in the sentence, and a key of
    /// `[sandbox.agents]` that is not a bare word is written as TOML writes it.
    #[test]
    fn config_check_says_an_empty_name_as_one() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[sandbox]\nprofile = \"\"\n[sandbox.agents]\n\"\" = \"max\"\ncodex = \"\"\n\"cursor agent\" = \"max\"\n",
        );

        assert_eq!(
            warnings(&check(&paths)),
            [
                "sandbox.profile: an empty name is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)",
                "sandbox.agents.\"\": taw knows no agent with an empty name (taw manifest list names them)",
                "sandbox.agents.codex: an empty name is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)",
                "sandbox.agents.\"cursor agent\": taw knows no agent called cursor agent (taw manifest list names them)",
            ]
        );
    }

    /// A file the config names is said once, under the config's key, a name that cannot be a
    /// profile's too.
    #[test]
    fn config_check_says_a_file_whose_name_cannot_be_a_profiles_once_when_the_config_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        sandbox_file(tmp.path(), "mine.sh", 0o755);
        let paths = write_config(tmp.path(), "[sandbox]\nprofile = \"mine.sh\"\n");

        assert_eq!(
            warnings(&check(&paths)),
            [
                "sandbox.profile: mine.sh is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)"
            ]
        );
    }

    /// With a config folder that is not a full path taw reads no profiles of yours, and config check
    /// says why.
    #[test]
    fn config_check_says_why_a_config_folder_that_is_not_a_full_path_has_no_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut paths = write_config(tmp.path(), "[sandbox]\nprofile = \"default\"\n");
        paths.config_dir = PathBuf::new();

        assert_eq!(
            warnings(&check(&paths)),
            ["sandbox: TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours"]
        );
    }

    /// What `taw sandbox` would refuse for `default`, config check says, the config naming it or not,
    /// and once: a profiles folder or a config folder that is a link whose folder is gone, a folder
    /// called default, and a profiles folder that is a file.
    #[test]
    fn config_check_says_what_taw_sandbox_would_refuse_for_default() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("sandbox");
        std::os::unix::fs::symlink(tmp.path().join("dotfiles-moved").join("sandbox"), &folder)
            .unwrap();
        let mut paths = paths_for(tmp.path());
        // taw's state folder of its own: with one folder for both, the `default` there is taw's.
        paths.state_dir = tmp.path().join("state");
        let gone = format!(
            "your profiles folder {0} is a link to a folder that is not there (ls -ld {0})",
            folder.display()
        );
        assert_eq!(warnings(&check(&paths)), [format!("sandbox: {gone}")]);
        for named in ["default", "max"] {
            std::fs::write(
                &paths.config_file,
                format!("[sandbox]\nprofile = \"{named}\"\n"),
            )
            .unwrap();
            assert_eq!(
                warnings(&check(&paths)),
                [format!("sandbox.profile: {gone}")],
                "{named}"
            );
        }
        std::fs::remove_file(&paths.config_file).unwrap();

        std::fs::remove_file(&folder).unwrap();
        std::fs::create_dir_all(folder.join("default")).unwrap();
        assert_eq!(
            warnings(&check(&paths)),
            [format!(
                "sandbox: the profile default is not a file (ls -ld {})",
                folder.join("default").display()
            )]
        );

        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, "not a folder").unwrap();
        assert_eq!(
            warnings(&check(&paths)),
            [format!(
                "sandbox: taw cannot look at the profile default: not a directory (ls -ld {})",
                folder.join("default").display()
            )]
        );

        let config = tmp.path().join("cfg");
        std::os::unix::fs::symlink(tmp.path().join("moved"), &config).unwrap();
        paths.config_file = config.join("config.toml");
        paths.config_dir = config.clone();
        assert_eq!(
            warnings(&check(&paths)),
            [format!(
                "sandbox: your config folder {0} is a link to a folder that is not there (ls -ld {0})",
                config.display()
            )]
        );
    }

    /// A link of yours whose file is gone is said, the one the config names under its key and once,
    /// and the one nothing names by the folder: `default` among them, which would otherwise run taw's
    /// own in its place.
    #[test]
    fn config_check_names_a_link_of_yours_whose_file_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("sandbox");
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["default", "max"] {
            std::os::unix::fs::symlink(tmp.path().join("moved-away").join(name), folder.join(name))
                .unwrap();
        }
        let mut paths = write_config(tmp.path(), "[sandbox]\nprofile = \"max\"\n");
        // taw's state folder of its own: with one folder for both, the `default` there is taw's.
        paths.state_dir = tmp.path().join("state");

        assert_eq!(
            warnings(&check(&paths)),
            [
                format!(
                    "sandbox.profile: the profile max is a link to a file that is not there (ls -l {})",
                    folder.join("max").display()
                ),
                format!(
                    "sandbox: the profile default is a link to a file that is not there (ls -l {})",
                    folder.join("default").display()
                ),
            ]
        );
    }

    #[test]
    fn keys_accept_a_string_or_a_list_and_a_preset() {
        let c: Config = toml::from_str(
            "[keys]\npreset = \"search\"\nmove_down = [\"j\", \"down\"]\nrename = \"R\"\n",
        )
        .unwrap();
        assert_eq!(c.keys.preset, KeysPreset::Search);
        assert_eq!(
            c.keys.bindings["move_down"].to_vec(),
            vec!["j".to_string(), "down".to_string()]
        );
        assert_eq!(c.keys.bindings["rename"].to_vec(), vec!["R".to_string()]);
    }

    #[test]
    fn herdr_v090_compat_keys_are_known_and_ignored() {
        let t: toml::Table = toml::from_str(
            "[terminal]\nkitty_graphics = false\n[ui]\npane_borders = \"always\"\n[theme.custom.light]\naccent = \"#fff\"\n[theme.custom.dark]\naccent = \"#000\"\n",
        )
        .unwrap();
        assert!(check_unknown_keys(&t).is_empty());
    }

    /// `config check` warns when `auto_switch` is on and tmux is older than 3.6, naming the version, and is
    /// silent with `auto_switch` off, on 3.6 and later, and when tmux could not say its version.
    #[test]
    fn check_warns_of_auto_switch_only_on_a_tmux_before_3_6() {
        use crate::tmux::version::parse;
        let on = Theme {
            auto_switch: true,
            ..Theme::default()
        };
        let said = check_auto_switch(&on, parse("tmux 3.5a")).unwrap();
        assert_eq!(said.level, Level::Warning);
        assert!(said.message.contains("tmux is 3.5a"), "{}", said.message);
        assert!(said.message.contains("3.6"), "{}", said.message);
        let said = check_auto_switch(&on, parse("tmux 3.4")).unwrap();
        assert!(said.message.contains("tmux is 3.4 "), "{}", said.message);
        assert!(check_auto_switch(&on, parse("tmux 3.6b")).is_none());
        assert!(check_auto_switch(&on, parse("tmux 3.8")).is_none());
        assert!(check_auto_switch(&on, parse("tmux next-3.8")).is_none());
        assert!(check_auto_switch(&on, parse("tmux 3.10")).is_none());
        assert!(check_auto_switch(&on, None).is_none());
        assert!(check_auto_switch(&Theme::default(), parse("tmux 3.5a")).is_none());
    }

    /// herdr 0.9.0 layers appearance-specific overrides under `[theme.custom.light]` and
    /// `[theme.custom.dark]`, for when `auto_switch` is on, and its default config lists
    /// them. `auto_switch` switches the theme name only, so the tables are accepted and ignored.
    /// The unknown-key check has always counted them as known, but the typed model took every
    /// value under `[theme.custom]` for a colour and refused a table, so a config that had
    /// them did not load at all.
    #[test]
    fn herdr_appearance_theme_tables_load_and_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[theme.custom]\naccent = \"#89b4fa\"\n\
             [theme.custom.light]\npanel_bg = \"#eff1f5\"\ntext = \"#4c4f69\"\n\
             [theme.custom.dark]\npanel_bg = \"#1e1e2e\"\ntext = \"#cdd6f4\"\n",
        );

        assert_eq!(check(&paths), []);
        let config = load(&paths).unwrap();
        assert_eq!(
            config.theme.custom,
            BTreeMap::from([("accent".to_string(), "#89b4fa".to_string())]),
            "the colours of the two tables are not taw's to use, and the one above them is"
        );
    }

    /// Only the two tables herdr defines are let through: a table under any other name is not
    /// a colour, and is refused as it always was.
    #[test]
    fn another_table_under_theme_custom_is_still_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), "[theme.custom.accent]\nx = \"#fff\"\n");

        let diagnostics = check(&paths);

        assert!(
            error_messages(&diagnostics)
                .iter()
                .any(|message| message.contains("theme.custom.accent")),
            "{diagnostics:#?}"
        );
        assert!(load(&paths).is_err());
    }

    /// The `done_delay_seconds` range is validated: a value out of range is an error.
    #[test]
    fn done_delay_seconds_out_of_range_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), "[ui.toast]\ndone_delay_seconds = 4000\n");

        let diagnostics = check(&paths);

        assert!(
            diagnostics.iter().any(|d| d.level == Level::Error
                && d.message == "ui.toast.done_delay_seconds must be between 0 and 3600"),
            "{diagnostics:#?}"
        );
    }

    /// A malformed `[keys]` binding would vanish silently (`Keys::deserialize` drops what it
    /// can't parse as a `KeySpec`, with no way to also emit a diagnostic from inside a
    /// `Deserialize` impl), so `check_keys_bindings` surfaces it as a warning.
    #[test]
    fn a_malformed_keys_binding_is_a_warning_not_a_silent_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), "[keys]\nrename = 5\n");

        let diagnostics = check(&paths);

        assert_eq!(
            diagnostics,
            vec![Diagnostic {
                level: Level::Warning,
                message: "keys.rename: expected a key or a list of keys".to_string(),
            }]
        );
    }

    /// A `[keys]` spec that does not parse leaves the action on its preset keys in the popup;
    /// `taw config check` says so in the popup's own words, as a warning.
    #[test]
    fn a_keys_spec_that_does_not_parse_is_a_warning_that_the_action_keeps_its_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[keys]\ngoto = \"prefix+g\"\nnot_an_action = \"x\"\nnew_session = \"r\"\n",
        );

        let diagnostics = check(&paths);

        assert_eq!(
            diagnostics,
            vec![Diagnostic {
                level: Level::Warning,
                message: "keys: \"prefix+g\": prefix chords are tmux's; set \
                          @agent-wizard-notification-key instead; filter keeps its default keys"
                    .to_string(),
            }]
        );
    }

    /// A key a styled token does not know is not left to vanish without a word: `hide` written on the
    /// token and not inside one of its rules, or `rule` for `rules`, and the row went on being
    /// drawn as if the line had not been written. `taw config check` names it, as a warning, in
    /// every table a token can be in; the popup still loads the config, ignoring the key.
    #[test]
    fn an_unknown_key_in_a_styled_token_is_a_warning_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            r##"[ui.sidebar.agents]
rows = [["state_icon", { token = "$machine", hide = true }], [{ token = "$load", rule = [{ gt = 80, bold = true }] }]]
[ui.sidebar.agents.rows_by_agent]
claude = [[{ token = "workspace", colour = "red" }]]
[ui.sidebar.sessions]
rows = [[{ token = "branch", fg = "#fff", bold = true, dim = false, hidden = true, rules = [{ equals = "main", hide = true }] }]]
"##,
        );

        let diagnostics = check(&paths);

        assert_eq!(
            diagnostics,
            vec![
                Diagnostic {
                    level: Level::Warning,
                    message: "unknown config key: ui.sidebar.agents.rows[0][1].hide (a token's `hide` goes inside one of its `rules`)".to_string(),
                },
                Diagnostic {
                    level: Level::Warning,
                    message: "unknown config key: ui.sidebar.agents.rows[1][0].rule (did you mean `rules`?)".to_string(),
                },
                Diagnostic {
                    level: Level::Warning,
                    message: "unknown config key: ui.sidebar.agents.rows_by_agent.claude[0][0].colour".to_string(),
                },
                Diagnostic {
                    level: Level::Warning,
                    message: "unknown config key: ui.sidebar.sessions.rows[0][0].hidden".to_string(),
                },
            ]
        );
        let config = load(&paths).expect("the popup still loads it");
        assert_eq!(config.ui.sidebar.agents.rows[0][1].token, "$machine");
        assert_eq!(config.ui.sidebar.agents.rows[1][0].rules.len(), 0);
    }

    #[test]
    fn the_legacy_keys_indexed_subtable_is_not_warned_about() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "[keys.indexed]\ntabs = \"\"\nworkspaces = \"\"\nagents = \"\"\n",
        );

        let diagnostics = check(&paths);

        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    /// Byte index of the first '#' in `s` that starts a TOML comment, i.e. is
    /// not inside a quoted string (a hex color like `"#89b4fa"` must not be
    /// mistaken for a comment). Returns `s.len()` when there is none.
    fn comment_start(s: &str) -> usize {
        let mut in_string: Option<char> = None;
        let mut chars = s.char_indices();
        while let Some((idx, c)) = chars.next() {
            if let Some(q) = in_string {
                if c == '\\' && q == '"' {
                    chars.next();
                } else if c == q {
                    in_string = None;
                }
            } else if c == '"' || c == '\'' {
                in_string = Some(c);
            } else if c == '#' {
                return idx;
            }
        }
        s.len()
    }

    /// Split at the first '=' that is not inside a quoted string.
    fn split_first_unquoted_eq(s: &str) -> Option<(&str, &str)> {
        let mut in_string: Option<char> = None;
        let mut chars = s.char_indices();
        while let Some((idx, c)) = chars.next() {
            if let Some(q) = in_string {
                if c == '\\' && q == '"' {
                    chars.next();
                } else if c == q {
                    in_string = None;
                }
            } else if c == '"' || c == '\'' {
                in_string = Some(c);
            } else if c == '=' {
                return Some((&s[..idx], &s[idx + 1..]));
            }
        }
        None
    }

    fn looks_like_bare_key(key: &str) -> bool {
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    /// Does the matching close quote for the string starting at `value`'s first
    /// character land exactly at the end, i.e. nothing trails after the string?
    fn closes_at_end(after_open_quote: &str, quote: char) -> bool {
        let mut chars = after_open_quote.char_indices();
        while let Some((idx, c)) = chars.next() {
            if c == '\\' && quote == '"' {
                chars.next();
                continue;
            }
            if c == quote {
                return idx == after_open_quote.len() - quote.len_utf8();
            }
        }
        false
    }

    /// Is `value` (already comment-stripped and trimmed) a single complete
    /// TOML value, with nothing trailing? Rejects herdr's prose that merely
    /// contains a quoted example, e.g. `"shell" runs detached in the background.`
    fn looks_like_single_toml_value(value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        if value == "true" || value == "false" {
            return true;
        }
        if let Some(rest) = value.strip_prefix('"') {
            return closes_at_end(rest, '"');
        }
        if let Some(rest) = value.strip_prefix('\'') {
            return closes_at_end(rest, '\'');
        }
        if value.starts_with('[') {
            return value.ends_with(']');
        }
        if value.starts_with('{') {
            return value.ends_with('}');
        }
        // Numbers and dates/times: the only bare (unquoted) values herdr uses.
        value
            .chars()
            .all(|c| c.is_ascii_digit() || "+-.:TZ_".contains(c))
    }

    /// Does `after_hash_space` (a comment line with the leading `# ` already
    /// stripped) look like a real `key = value` default or a `[table]` /
    /// `[[table]]` header, as opposed to prose that happens to contain `=`,
    /// `[`, or a stray `#` inside a quoted hex color?
    fn looks_like_default_assignment(after_hash_space: &str) -> bool {
        let head = after_hash_space[..comment_start(after_hash_space)].trim();
        if head.is_empty() {
            return false;
        }
        if head.starts_with('[') && head.ends_with(']') {
            return true;
        }
        match split_first_unquoted_eq(head) {
            Some((key, value)) => {
                looks_like_bare_key(key.trim()) && looks_like_single_toml_value(value)
            }
            None => false,
        }
    }

    /// Uncomment every herdr reference-config line that is a bare `# key = value`
    /// or `# [table]` / `# [[table]]` header, leaving pure prose comments alone
    /// (including prose that merely mentions `key = value` syntax, e.g. the
    /// `[ui.toast]` legend or the `[[keys.command]]` field-by-field walkthrough),
    /// and skipping the `[[keys.command]]` example block entirely (it documents
    /// a user-defined custom command, not a default).
    fn uncomment_herdr_default(raw: &str) -> String {
        let mut out = String::new();
        let mut skipping_block = false;
        for line in raw.lines() {
            if skipping_block {
                if line.trim().is_empty() {
                    skipping_block = false;
                }
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if let Some(rest) = line.strip_prefix("# ") {
                if rest.trim_start().starts_with("[[keys.command]]") {
                    skipping_block = true;
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }
                if looks_like_default_assignment(rest) {
                    out.push_str(rest);
                    out.push('\n');
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    #[test]
    fn parses_herdr_default_config() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/third_party/herdr/reference/default-config.toml"
        ))
        .unwrap();
        let uncommented = uncomment_herdr_default(&raw);

        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(tmp.path(), &uncommented);

        let diagnostics = check(&paths);
        let errors = error_messages(&diagnostics);

        assert!(
            errors.is_empty(),
            "expected no errors, got: {errors:#?}\n--- uncommented config ---\n{uncommented}"
        );
    }

    // -------------------------------------------------------------------------------
    // normalize_utf8_bom — ported from herdr's `config/io.rs` tests (v0.9.1, #3678)
    // -------------------------------------------------------------------------------

    #[test]
    fn normalize_utf8_bom_removes_a_leading_bom() {
        let content = "\u{feff}onboarding = false\n[terminal]\n";
        assert_eq!(
            normalize_utf8_bom(content),
            "onboarding = false\n[terminal]\n"
        );
    }

    #[test]
    fn normalize_utf8_bom_recovers_from_a_displaced_mid_file_bom() {
        let content = "onboarding = false\n\u{feff}[terminal]\nname = \"x\"\n";
        let normalized = normalize_utf8_bom(content);
        assert_eq!(normalized, "onboarding = false\n[terminal]\nname = \"x\"\n");
        assert!(toml::from_str::<toml::Table>(&normalized).is_ok());
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_basic_strings() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\n";
        assert!(toml::from_str::<toml::Table>(content).is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_boms_in_multiline_literal_strings() {
        let content = "[theme]\nname = '''\nfirst\n\u{feff}second\n'''\n";
        assert!(toml::from_str::<toml::Table>(content).is_ok());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn normalize_utf8_bom_preserves_string_boms_despite_other_errors() {
        let content = "[theme]\nname = \"\"\"\nfirst\n\u{feff}second\n\"\"\"\nbroken = \n";
        assert!(toml::from_str::<toml::Table>(content).is_err());
        assert_eq!(normalize_utf8_bom(content), content);
    }

    #[test]
    fn config_load_recovers_from_a_mid_file_bom() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = write_config(
            tmp.path(),
            "onboarding = true\n\u{feff}[theme]\nname = \"catppuccin-latte\"\n",
        );

        let config = load(&paths).unwrap();

        assert_eq!(config.theme.name, "catppuccin-latte");
        assert!(
            check(&paths).is_empty(),
            "a displaced BOM must not be reported as an error"
        );
    }
}
