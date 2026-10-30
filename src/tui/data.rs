//! One immutable read of the world, rebuilt on every 500 ms tick.
//!
//! tmux is the database: nothing here computes agent state, it
//! only reshapes what `list-panes`/`list-sessions`/`list-windows` already say into the
//! two lists the sidebar draws — plus git branch/status per workspace, which comes from
//! a background thread ([`GitWorker`]) so a slow repository can never stall a frame.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, RowsConfig, StatusIndicators};
use crate::git;
use crate::store::AgentState;
use crate::tmux::{Listing, PaneInfo, SessionInfo, Tmux, WindowInfo, ids};
use crate::tui::agent_view;

/// One agent-occupied pane, as the Agents panel needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentRow {
    pub pane_id: String,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    /// Panes in this row's window (`WindowInfo::window_panes`) — the same value
    /// [`PaneRow::window_panes`] reports for this pane.
    pub window_panes: u32,
    pub pane_index: u32,
    pub pane_current_command: String,
    pub pane_title: String,
    /// `PaneInfo::alternate_on`: the pane is showing a full-screen program; the preview
    /// never wraps such a pane.
    pub alternate_on: bool,
    /// `PaneInfo::pane_dead`: the pane was kept after its program ended (`remain-on-exit`). One half
    /// of what [`AgentRow::holds_its_agent_name`] reads.
    pub pane_dead: bool,
    /// `PaneInfo::pane_pid`, `0` for a pane with no process: what the idle look at the pane's own
    /// shell asks the process table about, and `0` is a shell that cannot be told.
    pub pane_pid: u32,
    pub kind: Option<String>,
    pub state: Option<AgentState>,
    /// The agent is suspended with Ctrl+Z (`@agent_rule` is [`crate::store::SUSPENDED_RULE`]): its
    /// row says [`crate::store::SUSPENDED_WORD`] where it says `idle` for one at its prompt.
    pub suspended: bool,
    /// `@agent_state_since` (unix seconds), `0` when unset — the `priority` sort's
    /// tie-breaker.
    pub state_since: u64,
    pub name: Option<String>,
    pub display: Option<String>,
    /// `@agent_seen`: whether this pane has been focused since its last state change.
    /// A filter field of the Agents view ([`crate::tui::agent_view`]).
    pub seen: bool,
    /// `@agent_container`: the runtime taw saw run this agent in its pane, from the process
    /// table alone, `None` outside any container. What draws 📦 after its name.
    pub container: Option<String>,
    /// `@agent_sandbox`: the profile taw started it through, `None` for one taw only
    /// recognised. The preview's title names it.
    pub sandbox: Option<String>,
    /// `@agent_token_*`, read only when the configured rows or the active Agents view
    /// actually use a `$token`.
    pub tokens: BTreeMap<String, String>,
    /// Index of this row's workspace in [`Snapshot::spaces`]: the `spaces` sort's
    /// primary key, so agents group under their workspace in workspace order.
    pub space_order: usize,
    /// The machine this row belongs to: an index into [`Snapshot::machines`], `None` for the machine
    /// taw runs on.
    pub machine: Option<usize>,
}

/// One tmux session (herdr "workspace"), as the Spaces panel needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpaceRow {
    pub session_id: String,
    pub session_name: String,
    pub session_path: PathBuf,
    /// `SessionInfo::session_windows`: how many windows (tabs) this session has.
    pub windows: u32,
    /// `SessionInfo::session_group_size`: how many sessions share this one's windows, itself
    /// included, `0` for a session in no group. More than one means closing its last pane
    /// closes the others with it.
    pub group_size: u32,
    pub attached: bool,
    pub active_window_id: String,
    /// The session's active pane, used as the jump and preview target for this row.
    pub active_pane_id: Option<String>,
    /// Worst state across the session's agents (blocked > working > done > idle >
    /// unknown); `None` when the session has no agents at all.
    pub state: Option<AgentState>,
    pub agent_count: usize,
    /// `None` when the session path is not a git work tree, or when git has not answered
    /// yet (the first tick after startup).
    pub git: Option<git::Info>,
    /// The machine this session is on ([`AgentRow::machine`]).
    pub machine: Option<usize>,
    /// A machine's session folder resolved there, when the feed sent one that differs from
    /// `session_path`: what that machine's rows are matched by ([`crate::tui::tree::closed_worktrees`]),
    /// never drawn. `None` on this machine's own rows.
    pub real_path: Option<PathBuf>,
}

/// One tmux pane, agent or plain shell alike, as the popup's per-pane tree needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneRow {
    pub pane_id: String,
    pub session_id: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    /// `WindowInfo::window_panes` for this pane's window (or the observed count, when
    /// `list-windows` did not report it) — how many panes to draw this window's split as.
    pub window_panes: u32,
    pub pane_index: u32,
    pub pane_current_command: String,
    pub pane_active: bool,
    /// `PaneInfo::alternate_on`: the pane is showing a full-screen program; the preview
    /// never wraps such a pane.
    pub alternate_on: bool,
    /// Index of this pane's own row in [`Snapshot::agents`], when an agent occupies it.
    pub agent: Option<usize>,
    /// The machine this pane is on ([`AgentRow::machine`]).
    pub machine: Option<usize>,
}

/// One machine besides this one, as the list's header line says it: what [`crate::machine::MachineView`]
/// holds of it that a frame draws. [`Snapshot::machines`] is in the order of the config.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MachineHeader {
    pub name: String,
    /// Connected, and heard from lately. A machine that is not live keeps its last rows, greyed.
    pub live: bool,
    /// What the header says of a machine that is not live (`connecting`, `not answering, ...`).
    pub words: String,
    /// The short form of `words` while they wait for the person to act (empty for words that change): the list is
    /// made wide enough for it, and a header with little room says it in place of `words`.
    pub short: String,
    /// Unix milliseconds of the last line heard from it.
    pub last_seen: Option<u64>,
    /// This machine's own tmux server, listed among the machines: no other machine, so the bar neither
    /// counts it nor names it offline.
    pub this_server: bool,
    /// No app keeps the connections, so nothing is known of the machine but its name: its header says how to
    /// connect it, and the bar neither counts it nor names it offline.
    pub no_keeper: bool,
    /// A state has come from it: with `live` and no session in it, the machine has no tmux session (or no
    /// tmux server), which its list says with a row ([`crate::tui::tree::Row::NoSession`]).
    pub stated: bool,
}

/// Everything one frame draws from.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub agents: Vec<AgentRow>,
    pub spaces: Vec<SpaceRow>,
    /// Every pane, tmux order: [`Snapshot::spaces`] order, then window index, then pane
    /// index — the popup's per-pane tree, agents and plain shells alike.
    pub panes: Vec<PaneRow>,
    pub taken_at: Instant,
    /// The workspace and tab the popup was opened from, which is what a `{"context": …}`
    /// value in an Agents view resolves against.
    pub current: agent_view::Current,
    /// The Agents view projection installed in `@agent_wizard_view`, when one is and it
    /// is a query herdr would accept.
    pub view: Option<agent_view::AgentView>,
    /// Why the installed projection was ignored, for the popup to say once.
    pub view_error: Option<String>,
    /// The pane the popup was opened from, when tmux still has it — the row the cursor
    /// opens on. It reaches [`build`] as an argument (`taw ui --from`, resolved by `taw ui
    /// open` before the popup exists), never from the environment: `display-popup -E` does
    /// not give its command the invoking pane's `$TMUX_PANE`.
    pub own_pane: Option<String>,
    /// The session the popup's client is showing (`own_pane`'s session, else the first
    /// attached one), as a session id. Drawn as `*` on its header.
    pub current_session: Option<String>,
    /// The session to go back to: the client's `#{client_last_session}` when `build` could
    /// read it, else the most recently attached session other than the current one
    /// ([`previous_session`]). Drawn as `-` on its header.
    pub previous_session: Option<String>,
    /// What the popup's client's `#{client_theme}` said (`dark`, `light` or `unknown`), read in the
    /// same query as its last session, and only when `[theme] auto_switch` is on. `None` when it was
    /// not asked, no client could be asked, or tmux (before 3.6) said nothing.
    pub client_theme: Option<String>,
    /// The machines besides this one, in the config's order. Empty when no `[machines]` table is
    /// set: every row is then this machine's and the list draws as it always has.
    pub machines: Vec<MachineHeader>,
    /// The worktrees another machine's sessions are running, as that machine's feed sent them
    /// ([`crate::machine::wire::State::open_worktrees`]), added by [`crate::tui::machines::merge`]. They are
    /// never rows: they only say which running sessions are a session's worktree sessions on that machine
    /// (`machine` is the index of its header in [`Snapshot::machines`]). Empty when no machine sent any.
    pub open_worktrees: Vec<crate::session::worktree::WorktreeRecord>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            agents: Vec::new(),
            spaces: Vec::new(),
            panes: Vec::new(),
            taken_at: Instant::now(),
            current: agent_view::Current::default(),
            view: None,
            view_error: None,
            own_pane: None,
            current_session: None,
            previous_session: None,
            client_theme: None,
            machines: Vec::new(),
            open_worktrees: Vec::new(),
        }
    }
}

impl Snapshot {
    /// Whether any listed machine is not this machine's own tmux server ([`MachineHeader::this_server`]):
    /// a table of that server alone is one machine, and a list over one machine draws no machine header,
    /// in the popup and in the app, exactly as with no `[machines]` at all. The indices of
    /// [`Snapshot::machines`] stay as the config has them; the drawing asks this.
    pub fn has_other_machines(&self) -> bool {
        self.machines.iter().any(|m| !m.this_server)
    }

    /// Whether rows of machine `machine` are greyed: it is one of the config's machines and it is
    /// not live, so what the list shows of it is its last state, kept while it is away. This
    /// machine's own rows are never greyed.
    pub fn greyed(&self, machine: Option<usize>) -> bool {
        machine.is_some_and(|at| self.machines.get(at).is_some_and(|m| !m.live))
    }

    /// The pane the preview should show for the current highlight: the agent's own pane,
    /// or a workspace's active pane.
    pub fn preview_target(
        &self,
        agent: Option<&AgentRow>,
        space: Option<&SpaceRow>,
    ) -> Option<String> {
        agent
            .map(|a| a.pane_id.clone())
            .or_else(|| space.and_then(|s| s.active_pane_id.clone()))
    }
}

/// Strips one leading non-alphanumeric glyph and the space after it, herdr's
/// `terminal_title_stripped` (agents prefix their title with a status glyph).
pub fn strip_title_glyph(title: &str) -> String {
    let mut chars = title.chars();
    match (chars.next(), chars.next()) {
        (Some(first), Some(' ')) if !first.is_alphanumeric() => chars.as_str().to_string(),
        _ => title.to_string(),
    }
}

impl AgentRow {
    /// Whether the row's pane holds the name its agent record carries: the rule for agent names the
    /// command line keeps ([`crate::tmux::holds_agent_name`], also [`PaneInfo::holds_its_agent_name`]),
    /// over the parts the row carries. `shell_idle` is the look at the pane's own shell.
    pub fn holds_its_agent_name(&self, shell_idle: impl FnOnce(&AgentRow) -> bool) -> bool {
        crate::tmux::holds_agent_name(self.kind.is_some(), self.pane_dead, || shell_idle(self))
    }

    /// herdr's agent display name: `@agent_display`, else `@agent_name`, else the
    /// detected kind, else `unknown`.
    pub fn agent_label(&self) -> String {
        self.display
            .clone()
            .or_else(|| self.name.clone())
            .or_else(|| self.kind.clone())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// The worst state among `states`, in rollup order
/// blocked > working > done > idle > unknown. `None` when `states` is empty.
pub fn rollup(states: impl IntoIterator<Item = Option<AgentState>>) -> Option<AgentState> {
    fn rank(state: Option<AgentState>) -> u8 {
        match state {
            Some(AgentState::Blocked) => 0,
            Some(AgentState::Working) => 1,
            Some(AgentState::Done) => 2,
            Some(AgentState::Idle) => 3,
            Some(AgentState::Unknown) => 4,
            None => 5,
        }
    }
    states
        .into_iter()
        .min_by_key(|&s| rank(s))
        .filter(|s| s.is_some())
        .flatten()
}

/// The `git_status` token: `±<modified> +<untracked> ↑<ahead> ↓<behind>`, each part
/// omitted when zero. Empty for a clean, up-to-date tree.
pub fn git_status_text(info: &git::Info) -> String {
    let parts = [
        ('\u{b1}', info.modified),
        ('+', info.untracked),
        ('\u{2191}', info.ahead),
        ('\u{2193}', info.behind),
    ];
    parts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(glyph, n)| format!("{glyph}{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds a snapshot from already-read tmux listings and a git lookup.
///
/// Split out from [`build`] so it is testable without a tmux server, and so the one
/// `list_panes` call per tick stays in one place. `git` is consulted per
/// distinct session path; it returns `None` for a non-repository or for a path the
/// background worker has not answered for yet.
pub fn build_from(
    panes: &[PaneInfo],
    sessions: &[SessionInfo],
    windows: &[WindowInfo],
    current_pane: Option<&str>,
    git: &dyn Fn(&Path) -> Option<git::Info>,
) -> Snapshot {
    // `list-sessions` order is the workspace order the sidebar shows, and the primary
    // sort key of the agents panel's `spaces` mode.
    let space_order: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| (s.session_id.as_str(), i))
        .collect();

    let mut agents: Vec<AgentRow> = panes
        .iter()
        .filter(|p| is_agent(p))
        .map(|p| AgentRow {
            pane_id: p.pane_id.clone(),
            session_id: p.session_id.clone(),
            session_name: p.session_name.clone(),
            window_id: p.window_id.clone(),
            window_index: p.window_index,
            window_name: p.window_name.clone(),
            window_panes: window_panes_for(p, windows, panes),
            pane_index: p.pane_index,
            pane_current_command: p.pane_current_command.clone(),
            pane_title: p.pane_title.clone(),
            alternate_on: p.alternate_on,
            pane_dead: p.pane_dead,
            pane_pid: p.pane_pid,
            kind: p.agent.kind.clone(),
            state: p.agent.state,
            state_since: p.agent.state_since.unwrap_or(0),
            suspended: p.agent.rule.as_deref() == Some(crate::store::SUSPENDED_RULE),
            name: p.agent.name.clone(),
            display: p.agent.display.clone(),
            seen: p.agent.seen,
            container: p.agent.container.clone(),
            sandbox: p.agent.sandbox.clone(),
            tokens: p.agent.tokens.clone(),
            // A pane in a session `list-sessions` did not report sorts last.
            space_order: space_order
                .get(p.session_id.as_str())
                .copied()
                .unwrap_or(usize::MAX),
            machine: None,
        })
        .collect();
    agents.sort_by(|a, b| {
        (a.space_order, a.window_index, a.pane_index).cmp(&(
            b.space_order,
            b.window_index,
            b.pane_index,
        ))
    });

    // Every pane, not just agent panes — the popup's per-pane tree draws plain shells
    // too. `agent` looks the pane up in the (already sorted) `agents` list above, so its
    // index is stable for the frame this snapshot serves.
    let mut pane_rows: Vec<PaneRow> = panes
        .iter()
        .map(|p| PaneRow {
            pane_id: p.pane_id.clone(),
            session_id: p.session_id.clone(),
            window_id: p.window_id.clone(),
            window_index: p.window_index,
            window_name: p.window_name.clone(),
            window_panes: window_panes_for(p, windows, panes),
            pane_index: p.pane_index,
            pane_current_command: p.pane_current_command.clone(),
            pane_active: p.pane_active,
            alternate_on: p.alternate_on,
            agent: agents.iter().position(|a| a.pane_id == p.pane_id),
            machine: None,
        })
        .collect();
    pane_rows.sort_by(|a, b| {
        let order = |row: &PaneRow| {
            space_order
                .get(row.session_id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        };
        (order(a), a.window_index, a.pane_index).cmp(&(order(b), b.window_index, b.pane_index))
    });

    let spaces = sessions
        .iter()
        .map(|s| {
            let own: Vec<&PaneInfo> = panes
                .iter()
                .filter(|p| p.session_id == s.session_id)
                .collect();
            let agent_panes = || own.iter().copied().filter(|p| is_agent(p));
            // `window_active` is authoritative from `list-windows`; `SessionInfo`'s own
            // field is derived the same way, and stands in when a session's windows are
            // not in the listing.
            let active_window_id = windows
                .iter()
                .find(|w| w.session_id == s.session_id && w.window_active)
                .map(|w| w.window_id.clone())
                .unwrap_or_else(|| s.active_window_id.clone());
            let active_pane_id = own
                .iter()
                .find(|p| p.pane_active && p.window_id == active_window_id)
                .or_else(|| own.iter().find(|p| p.pane_active))
                .map(|p| p.pane_id.clone());
            let path = PathBuf::from(&s.session_path);
            SpaceRow {
                session_id: s.session_id.clone(),
                session_name: s.session_name.clone(),
                windows: s.session_windows,
                group_size: s.session_group_size,
                attached: s.session_attached > 0,
                active_window_id,
                active_pane_id,
                state: rollup(agent_panes().map(|p| p.agent.state)),
                agent_count: agent_panes().count(),
                git: git(&path),
                session_path: path,
                machine: None,
                real_path: None,
            }
        })
        .collect();

    let own_pane = current_pane
        .filter(|id| panes.iter().any(|p| p.pane_id == *id))
        .map(str::to_string);
    let current_session = own_pane
        .as_deref()
        .and_then(|id| panes.iter().find(|p| p.pane_id == id))
        .map(|p| p.session_id.clone())
        .or_else(|| {
            sessions
                .iter()
                .find(|s| s.session_attached > 0)
                .map(|s| s.session_id.clone())
        });
    let previous = previous_session(sessions, current_session.as_deref());

    Snapshot {
        agents,
        spaces,
        panes: pane_rows,
        taken_at: Instant::now(),
        current: current_context(panes, sessions, current_pane),
        // The projection lives in a tmux option, which this function deliberately
        // cannot read; `build` fills it in.
        view: None,
        view_error: None,
        own_pane,
        current_session,
        previous_session: previous,
        client_theme: None,
        machines: Vec::new(),
        open_worktrees: Vec::new(),
    }
}

/// The session to go back to when no client can be asked: the most recently attached
/// session that is not `current`, by `session_last_attached`; `None` when no other session
/// was ever attached.
pub fn previous_session(sessions: &[SessionInfo], current: Option<&str>) -> Option<String> {
    sessions
        .iter()
        .filter(|s| Some(s.session_id.as_str()) != current && s.session_last_attached > 0)
        .max_by_key(|s| s.session_last_attached)
        .map(|s| s.session_id.clone())
}

/// `window_panes` for `pane`'s window: `list-windows`' own count for that (session,
/// window), falling back to how many of `panes` this listing actually holds for it — for
/// a caller that did not pass `list-windows`, or a window a split changed since it read.
fn window_panes_for(pane: &PaneInfo, windows: &[WindowInfo], panes: &[PaneInfo]) -> u32 {
    windows
        .iter()
        .find(|w| w.session_id == pane.session_id && w.window_id == pane.window_id)
        .map(|w| w.window_panes)
        .unwrap_or_else(|| {
            panes
                .iter()
                .filter(|p| p.session_id == pane.session_id && p.window_id == pane.window_id)
                .count() as u32
        })
}

/// herdr's "current UI context": the workspace and tab an Agents view's
/// `{"context": …}` value resolves against, as public ids.
///
/// `current_pane` is the pane the popup was opened from, which is exactly what herdr
/// means by the presented workspace. When there is none, or it names a pane that has
/// gone, the first attached session stands in; on a server with one client, which is the
/// normal case, that is the same answer.
fn current_context(
    panes: &[PaneInfo],
    sessions: &[SessionInfo],
    current_pane: Option<&str>,
) -> agent_view::Current {
    let from_pane = current_pane
        .and_then(|id| panes.iter().find(|p| p.pane_id == id))
        .map(|p| (p.session_id.clone(), p.window_id.clone()));
    let target = from_pane.or_else(|| {
        sessions
            .iter()
            .find(|s| s.session_attached > 0)
            .map(|s| (s.session_id.clone(), s.active_window_id.clone()))
    });
    match target {
        Some((session, window)) => agent_view::Current {
            workspace_id: Some(ids::workspace_id(&session)),
            tab_id: Some(ids::tab_id(&session, &window)),
        },
        None => agent_view::Current::default(),
    }
}

/// True when any configured row (either panel, including `rows_by_agent`) uses a
/// `$token`. Reading `@agent_token_*` costs one `show-options` per agent pane, so the
/// default configuration must not pay for it.
pub fn rows_use_custom_tokens(rows: &RowsConfig) -> bool {
    rows.rows
        .iter()
        .chain(rows.rows_by_agent.values().flatten())
        .flatten()
        .any(|t| t.token.starts_with('$'))
}

/// Reads tmux once and builds a snapshot: the panes, the windows, the sessions and the global options that hold
/// the Agents view, in one tmux call ([`Tmux::read_listing`]), with the popup's client's last session and theme
/// at the end of the same call.
/// `@agent_token_*` options are read per agent pane only when the config's rows or the
/// active view reference a `$token`.
///
/// `origin` is the pane the popup was opened from — [`Snapshot::own_pane`] and the
/// current context. It is a parameter rather than a `$TMUX_PANE` read because
/// `display-popup -E` does not give its command the invoking pane's environment: the
/// popup would never have one. `taw ui open` resolves the pane while tmux still knows it
/// and hands it down through `tui::run` (`cli::misc::ui` owns the `$TMUX_PANE` fallback
/// for a `taw ui` typed in a pane).
///
/// `client` is the client the popup was opened on, named by its launcher (`taw ui
/// --client`): the session to go back to ([`Snapshot::previous_session`]) is read for that
/// client. `None` for a `taw ui` typed in a pane, whose client is picked from those attached.
pub fn build(
    t: &Tmux,
    cfg: &Config,
    git: &GitWorker,
    origin: Option<&str>,
    client: Option<&str>,
) -> anyhow::Result<Snapshot> {
    build_with(t, cfg, git, origin, Some(client), None).map(|(snapshot, _)| snapshot)
}

/// [`build`] for a screen that has no client at all (`taw app`): no client is looked for, and none is
/// asked for its last session, which costs two tmux calls on every tick and which nobody would use. The
/// sessions of the same read come with it, for whoever needs them (the app's first cursor).
///
/// `own_pane` is the pane the app runs in: with `[theme] auto_switch` on and a pane, the theme of the client that
/// shows it is asked at the end of the same call ([`read_pane_theme`]'s question, in one process with the list) and
/// is [`Snapshot::client_theme`]. With auto_switch off, or no pane, no theme is asked.
pub fn build_without_client(
    t: &Tmux,
    cfg: &Config,
    git: &GitWorker,
    own_pane: Option<&str>,
) -> anyhow::Result<(Snapshot, Vec<SessionInfo>)> {
    build_with(t, cfg, git, None, None, own_pane)
}

/// [`build`], with `client` as `Some(<what build was given>)` when a client's last session is to be read,
/// and `None` when there is no client to ask.
fn build_with(
    t: &Tmux,
    cfg: &Config,
    git: &GitWorker,
    origin: Option<&str>,
    client: Option<Option<&str>>,
    own_pane: Option<&str>,
) -> anyhow::Result<(Snapshot, Vec<SessionInfo>)> {
    // The client to ask is found first (a `taw ui` typed in a pane picks one of those attached), and asked in
    // the call that reads the list.
    let asked = client.and_then(|named| ask_client(t, named, cfg.theme.auto_switch));
    // The app's own pane is asked for its theme instead of a client, and only when the theme follows the terminal.
    let pane_theme = own_pane.filter(|_| cfg.theme.auto_switch);
    let listing = match pane_theme {
        Some(pane) => t.read_listing_of_pane(pane, PANE_THEME_FORMAT)?,
        None => t.read_listing(
            asked
                .as_ref()
                .map(|(name, format)| (name.as_str(), *format)),
        )?,
    };
    let (view, view_error) = read_view(listing.option(agent_view::OPTION));
    let Listing {
        mut panes,
        windows,
        sessions,
        client: said,
        ..
    } = listing;

    if rows_use_custom_tokens(&cfg.ui.sidebar.agents)
        || view
            .as_ref()
            .is_some_and(agent_view::AgentView::uses_tokens)
    {
        for pane in panes.iter_mut().filter(|p| is_agent(p)) {
            if let Ok(record) = crate::store::read(t, &pane.pane_id) {
                pane.agent.tokens = record.tokens;
            }
        }
    }

    // Ask the worker for this tick's paths, then read whatever it has already answered.
    git.request(
        sessions
            .iter()
            .map(|s| PathBuf::from(&s.session_path))
            .collect(),
    );
    let mut snapshot = build_from(&panes, &sessions, &windows, origin, &|path| git.get(path));
    snapshot.view = view;
    snapshot.view_error = view_error;
    // The client's own idea of "last session" beats the timestamps when it can be read:
    // it is per client, so two screens each go back to their own place.
    if pane_theme.is_some() {
        snapshot.client_theme = said.as_deref().and_then(parse_pane_theme);
    } else if let Some(said) = said {
        let read = parse_client_read(
            &said,
            &sessions,
            snapshot.current_session.as_deref(),
            cfg.theme.auto_switch,
        );
        if let Some(previous) = read.previous_session {
            snapshot.previous_session = Some(previous);
        }
        snapshot.client_theme = read.theme;
    }
    Ok((snapshot, sessions))
}

/// What one question to the popup's client answers: its last session and, when asked, its theme.
#[derive(Debug, PartialEq, Eq)]
struct ClientRead {
    /// `#{client_last_session}` as a session id: `None` when there is no resolvable client, the
    /// option is empty, it names a session that is gone, or it names the current session (tmux
    /// reports the current one as "last" right after a fresh attach).
    previous_session: Option<String>,
    /// `#{client_theme}`: `dark`, `light` or `unknown`, `None` when it was not asked or came back
    /// empty (tmux before 3.6 has no such format and expands it to nothing).
    theme: Option<String>,
}

/// The question to the popup's client for `#{client_last_session}`, and `#{client_theme}` when `with_theme`: its
/// name and the format, asked in the call that reads the list, so a popup that follows the terminal's light or
/// dark mode makes no further process each tick.
///
/// The popup's client is `client`, the one its launcher named, and read by name; with none
/// (a `taw ui` typed in a pane) it is picked from those attached, by hand. Both are per client,
/// so with two screens on one session it is the popup's own screen that counts, and the first
/// client the server lists is the wrong one half the time. Without `with_theme` the question is
/// the one it always was, `#{client_last_session}` alone. `None` when no client can be found.
fn ask_client(t: &Tmux, client: Option<&str>, with_theme: bool) -> Option<(String, &'static str)> {
    let client = match client {
        Some(named) => named.to_string(),
        None => crate::cli::common::resolve_client(t).ok()?,
    };
    let format = if with_theme {
        "#{client_theme} #{client_last_session}"
    } else {
        "#{client_last_session}"
    };
    Some((client, format))
}

/// `#{client_theme}` of the client tmux finds for the caller, for `taw app`, which has no client of its own. `-t
/// <pane>` does not choose it: tmux takes the pane named by the caller's `TMUX_PANE`, which is the app's own pane,
/// and the most recently active client of that pane's session, so the answer is the same here. The answer is
/// `dark`, `light` or `unknown`; `None` when the question failed or tmux (before 3.6) said nothing. One
/// `display-message`; no query of the app's tick carries a client's fields (a `list-panes`, `list-windows` or
/// `list-sessions` format expands `#{client_theme}` to nothing), so the app asks it only with `[theme]
/// auto_switch` on.
pub fn read_pane_theme(t: &Tmux, pane: &str) -> Option<String> {
    let said = t
        .run(&["display-message", "-p", "-t", pane, PANE_THEME_FORMAT])
        .ok()?;
    parse_pane_theme(&said)
}

/// What [`read_pane_theme`] asks, in a call of its own or at the end of the app's one-call read.
const PANE_THEME_FORMAT: &str = "#{client_theme}";

/// The answer to [`PANE_THEME_FORMAT`]: the word, `None` when it came back empty (tmux before 3.6 has no such format).
fn parse_pane_theme(said: &str) -> Option<String> {
    Some(said.trim().to_string()).filter(|theme| !theme.is_empty())
}

/// [`ask_client`]'s answer, as tmux printed it: the theme first (one word, empty on a tmux that
/// has none) when it was asked for, then the last session's name, which may hold spaces.
fn parse_client_read(
    said: &str,
    sessions: &[SessionInfo],
    current: Option<&str>,
    with_theme: bool,
) -> ClientRead {
    let (theme, name) = if with_theme {
        let said = said.trim_end_matches(['\n', '\r']);
        let (theme, name) = said.split_once(' ').unwrap_or((said, ""));
        (
            Some(theme.trim().to_string()).filter(|t| !t.is_empty()),
            name,
        )
    } else {
        (None, said)
    };
    let name = name.trim();
    let previous_session = if name.is_empty() {
        None
    } else {
        sessions
            .iter()
            .find(|s| s.session_name == name)
            .map(|s| s.session_id.clone())
            .filter(|id| Some(id.as_str()) != current)
    };
    ClientRead {
        previous_session,
        theme,
    }
}

/// Reads and parses `@agent_wizard_view`.
///
/// A tmux server that cannot be asked is not an error here: the popup keeps drawing
/// without a projection, which is what an unset option means anyway. A query that *is*
/// installed but will not parse is returned as a message, because a user who asked for
/// a view and got everything deserves to be told why.
///
/// `debug!`, not `warn!`, for both: this runs on every tick of a full-screen TUI whose
/// stderr is the pane it is drawing in, and `warn` is on by default. The popup reports
/// the failure on its own bottom line instead — see `App::report_view_error`.
fn read_view(raw: Option<String>) -> (Option<agent_view::AgentView>, Option<String>) {
    match agent_view::parse(raw.as_deref()) {
        Ok(view) => (view, None),
        Err(e) => {
            log::debug!(
                "{} ignored: {e} (value: {})",
                agent_view::OPTION,
                raw.unwrap_or_default()
            );
            (None, Some(e))
        }
    }
}

/// A pane belongs in the Agents panel when tmux says an agent occupies it: a detected
/// kind, or a reported state (a hook can report a state before detection names a kind).
pub fn is_agent(pane: &PaneInfo) -> bool {
    pane.agent.kind.is_some() || pane.agent.state.is_some()
}

/// Runs [`git::Cache`] lookups on a background thread.
///
/// The render loop calls [`GitWorker::request`] with the paths this tick needs and
/// [`GitWorker::get`] for the answers it already has, so a repository that takes a
/// second to stat shows stale (or absent) git columns instead of freezing the popup.
/// Dropping the worker closes the request channel, which ends the thread.
pub struct GitWorker {
    requests: Sender<Vec<PathBuf>>,
    answers: Arc<Mutex<HashMap<PathBuf, Option<git::Info>>>>,
}

impl GitWorker {
    /// Spawns the worker thread with a [`git::Cache`] of `ttl`.
    pub fn new(ttl: Duration) -> GitWorker {
        let (tx, rx): (Sender<Vec<PathBuf>>, Receiver<Vec<PathBuf>>) = channel();
        let answers: Arc<Mutex<HashMap<PathBuf, Option<git::Info>>>> = Arc::default();
        let thread_answers = Arc::clone(&answers);
        std::thread::Builder::new()
            .name("taw-git".to_string())
            .spawn(move || {
                let mut cache = git::Cache::new(ttl);
                while let Ok(paths) = rx.recv() {
                    for path in paths {
                        let info = cache.get(&path);
                        if let Ok(mut map) = thread_answers.lock() {
                            map.insert(path, info);
                        }
                    }
                }
            })
            .expect("spawning the git worker thread");
        GitWorker {
            requests: tx,
            answers,
        }
    }

    /// Queues `paths` for refresh. Silently does nothing once the thread is gone — git
    /// columns are cosmetic, and the popup must not fail because of them.
    pub fn request(&self, paths: Vec<PathBuf>) {
        let _ = self.requests.send(paths);
    }

    /// The most recent answer for `path`, or `None` when the worker has not answered yet
    /// or the path is not a repository.
    pub fn get(&self, path: &Path) -> Option<git::Info> {
        self.answers.lock().ok()?.get(path).cloned().flatten()
    }
}

/// The glyph for one agent state, per `[ui] status_indicators` — the same glyphs the
/// status rail uses (`rail::ENTRIES`): dots colour a single `●`, symbols give each state
/// its own mark.
pub fn state_icon(state: Option<AgentState>, indicators: StatusIndicators) -> &'static str {
    match (indicators, state) {
        (StatusIndicators::Dots, Some(_)) => "\u{25cf}",
        (StatusIndicators::Symbols, Some(AgentState::Blocked)) => "\u{23f8}",
        (StatusIndicators::Symbols, Some(AgentState::Working)) => "\u{23f5}",
        (StatusIndicators::Symbols, Some(AgentState::Done)) => "\u{2713}",
        (StatusIndicators::Symbols, Some(AgentState::Idle)) => "\u{25cb}",
        (StatusIndicators::Symbols, Some(AgentState::Unknown)) => "?",
        // A workspace with no agents at all.
        (_, None) => "\u{b7}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_sessions() -> Vec<SessionInfo> {
        vec![session("main", 0), session("work space", 1)]
    }

    /// What the client's one answer holds: the theme first, then the last session's name (which may hold spaces).
    #[test]
    fn the_clients_answer_is_its_theme_then_its_last_session() {
        let sessions = two_sessions();
        let work = sessions[1].session_id.clone();
        for (said, theme) in [
            ("dark work space\n", "dark"),
            ("light work space", "light"),
            ("unknown work space", "unknown"),
        ] {
            let read = parse_client_read(said, &sessions, None, true);
            assert_eq!(read.theme.as_deref(), Some(theme), "{said:?}");
            assert_eq!(
                read.previous_session.as_deref(),
                Some(work.as_str()),
                "{said:?}"
            );
        }
        // tmux before 3.6 expands the format to nothing: no theme, and the session is still read.
        let old = parse_client_read(" work space\n", &sessions, None, true);
        assert_eq!(old.theme, None);
        assert_eq!(old.previous_session.as_deref(), Some(work.as_str()));
        // No last session: a theme and nothing else.
        let alone = parse_client_read("dark \n", &sessions, None, true);
        assert_eq!(
            (alone.theme.as_deref(), alone.previous_session),
            (Some("dark"), None)
        );
        // The current session is never the one to go back to.
        let current = parse_client_read("dark work space", &sessions, Some(work.as_str()), true);
        assert_eq!(current.previous_session, None);
        // Not asked for: the answer is the session's name alone, as it was.
        let plain = parse_client_read("work space\n", &sessions, None, false);
        assert_eq!((plain.theme, plain.previous_session), (None, Some(work)));
    }

    /// `taw app` asks the client that shows its pane, by the pane, once; an empty answer (an old tmux) is none.
    #[test]
    fn the_app_asks_for_the_theme_of_the_client_that_shows_its_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (t, log) = stand_in(dir.path(), "light");
        assert_eq!(read_pane_theme(&t, "%7").as_deref(), Some("light"));
        let asked = std::fs::read_to_string(&log).unwrap();
        assert_eq!(asked.lines().count(), 1, "{asked:?}");
        assert!(
            asked.contains("display-message\x1f-p\x1f-t\x1f%7\x1f#{client_theme}\x1f"),
            "{asked:?}"
        );
        let again = tempfile::tempdir().unwrap();
        let (t, _) = stand_in(again.path(), "");
        assert_eq!(read_pane_theme(&t, "%7"), None);
    }

    /// A stand-in `tmux` that logs its calls and answers `display-message` with `reply`.
    fn stand_in(dir: &Path, reply: &str) -> (Tmux, PathBuf) {
        let log = dir.join("argv.log");
        let script = dir.join("tmux");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\037' \"$@\" >> '{}'\nprintf '\\n' >> '{}'\nprintf '%s\\n' '{reply}'\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&script);
        (
            Tmux {
                bin: script,
                socket: None,
            },
            log,
        )
    }

    /// The popup's client is asked by name, for its theme and its last session together; the question is the
    /// old one, `#{client_last_session}` alone, when `auto_switch` is off. Asking starts no process when the
    /// client is named: the question joins the call that reads the list.
    #[test]
    fn the_client_is_asked_for_its_theme_and_its_last_session_in_one_question() {
        let dir = tempfile::tempdir().unwrap();
        let (t, log) = stand_in(dir.path(), "light work space");
        assert_eq!(
            ask_client(&t, Some("/dev/ttys009"), true),
            Some((
                "/dev/ttys009".to_string(),
                "#{client_theme} #{client_last_session}"
            ))
        );
        assert_eq!(
            ask_client(&t, Some("/dev/ttys009"), false),
            Some(("/dev/ttys009".to_string(), "#{client_last_session}"))
        );
        assert!(!log.exists(), "a process was started to ask");
    }

    use crate::store::AgentRecord;

    fn pane(session: &str, window: u32, pane_index: u32, state: Option<AgentState>) -> PaneInfo {
        PaneInfo {
            pane_id: format!("%{}{}{}", session.len(), window, pane_index),
            session_id: format!("${}", session.len()),
            session_name: session.to_string(),
            session_path: format!("/repos/{session}"),
            session_attached: 0,
            window_id: format!("@{window}"),
            window_index: window,
            window_name: "zsh".to_string(),
            window_active: window == 0,
            window_layout: String::new(),
            pane_index,
            pane_active: pane_index == 0,
            pane_pid: 0,
            pane_current_command: "zsh".to_string(),
            pane_current_path: format!("/repos/{session}"),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord {
                kind: state.map(|_| "claude".to_string()),
                state,
                ..Default::default()
            },
        }
    }

    fn session(name: &str, index: u32) -> SessionInfo {
        SessionInfo {
            session_id: format!("${}", name.len()),
            session_name: name.to_string(),
            session_path: format!("/repos/{name}"),
            session_attached: u32::from(index == 0),
            session_windows: 1,
            session_group_size: 0,
            active_window_id: "@0".to_string(),
            session_last_attached: 0,
        }
    }

    fn window(session: &str, index: u32) -> WindowInfo {
        WindowInfo {
            window_id: format!("@{index}"),
            session_id: format!("${}", session.len()),
            window_index: index,
            window_name: "zsh".to_string(),
            window_active: index == 0,
            window_panes: 1,
            window_layout: String::new(),
        }
    }

    fn no_git(_: &Path) -> Option<git::Info> {
        None
    }

    #[test]
    fn build_from_reads_where_each_agent_runs() {
        let mut boxed = pane("api", 0, 0, Some(AgentState::Working));
        boxed.agent.container = Some("docker".to_string());
        boxed.agent.sandbox = Some("default".to_string());
        let plain = pane("api", 1, 0, Some(AgentState::Idle));

        let snap = build_from(&[boxed, plain], &[session("api", 0)], &[], None, &no_git);

        let runs = |at: usize| {
            (
                snap.agents[at].container.as_deref(),
                snap.agents[at].sandbox.as_deref(),
            )
        };
        assert_eq!(runs(0), (Some("docker"), Some("default")));
        assert_eq!(runs(1), (None, None));
    }

    /// The rule for agent names is one definition: a pane, a popup row built from it and the rule over
    /// bare parts answer alike for every kind, dead flag and idle look, and the idle look is asked
    /// only when kind and dead flag leave the answer open.
    #[test]
    fn the_pane_and_the_popup_row_hold_a_name_by_one_rule() {
        for kind_known in [false, true] {
            for dead in [false, true] {
                for idle in [false, true] {
                    let mut p = pane("api", 0, 0, None);
                    p.agent.kind = kind_known.then(|| "claude".to_string());
                    p.agent.name = Some("rev".to_string());
                    p.pane_dead = dead;
                    p.pane_pid = 31;
                    let snap = build_from(
                        std::slice::from_ref(&p),
                        &[session("api", 0)],
                        &[],
                        None,
                        &no_git,
                    );
                    let row_asked = std::cell::Cell::new(false);
                    let pane_asked = std::cell::Cell::new(false);
                    let want = kind_known && !dead && !idle;
                    // A row of a pane with no kind and no state is no agent row: build it by hand.
                    let row = snap.agents.first().cloned().unwrap_or(AgentRow {
                        pane_dead: p.pane_dead,
                        pane_pid: p.pane_pid,
                        kind: p.agent.kind.clone(),
                        ..Default::default()
                    });

                    let by_row = row.holds_its_agent_name(|r| {
                        row_asked.set(true);
                        assert_eq!(r.pane_pid, 31);
                        idle
                    });
                    let by_pane = p.holds_its_agent_name(|q| {
                        pane_asked.set(true);
                        assert_eq!(q.pane_pid, 31);
                        idle
                    });
                    let by_parts = crate::tmux::holds_agent_name(kind_known, dead, || idle);

                    let at = format!("kind {kind_known}, dead {dead}, idle {idle}");
                    assert_eq!(by_row, want, "row, {at}");
                    assert_eq!(by_pane, want, "pane, {at}");
                    assert_eq!(by_parts, want, "parts, {at}");
                    assert_eq!(row_asked.get(), kind_known && !dead, "row asks, {at}");
                    assert_eq!(pane_asked.get(), kind_known && !dead, "pane asks, {at}");
                }
            }
        }
    }

    /// The two facts the rule for agent names reads off a pane, whether it was kept after its program
    /// ended and which process its shell is, reach the row, so the popup's rename check can follow
    /// the rule the command line follows.
    #[test]
    fn build_from_carries_the_pane_dead_flag_and_pid_into_the_row() {
        let mut kept = pane("api", 0, 0, Some(AgentState::Idle));
        kept.pane_dead = true;
        kept.pane_pid = 4242;
        let mut running = pane("api", 1, 0, Some(AgentState::Working));
        running.pane_pid = 777;

        let snap = build_from(&[kept, running], &[session("api", 0)], &[], None, &no_git);

        assert_eq!(
            (snap.agents[0].pane_dead, snap.agents[0].pane_pid),
            (true, 4242)
        );
        assert_eq!(
            (snap.agents[1].pane_dead, snap.agents[1].pane_pid),
            (false, 777)
        );
    }

    /// A profile whose sandbox taw cannot see: `@agent_sandbox` set and `@agent_container` not.
    /// The row still carries the profile, which the preview's title names.
    #[test]
    fn build_from_keeps_the_profile_of_an_agent_with_no_container() {
        let mut hidden = pane("api", 0, 0, Some(AgentState::Working));
        hidden.agent.sandbox = Some("box".to_string());

        let snap = build_from(&[hidden], &[session("api", 0)], &[], None, &no_git);

        assert_eq!(snap.agents[0].container, None);
        assert_eq!(snap.agents[0].sandbox.as_deref(), Some("box"));
    }

    /// A pane tmux titled itself, with the host name, is a row with no title and its kind for
    /// a name: the popup shows what a program set and nothing else, as herdr's rows do, where
    /// panes start with no title. Read from a listing, which is where the title is taken.
    #[test]
    fn an_agent_pane_tmux_titled_itself_is_a_row_with_no_title_and_its_kind() {
        const HOST: &str = "studio.local";
        let out = format!(
            "{}\n{}",
            crate::tmux::listing_line("%1", HOST, "claude"),
            crate::tmux::listing_line("%2", "\u{2733} Build", "codex")
        );
        let panes = crate::tmux::parse_pane_lines(&out, Some(HOST)).0;
        let snap = build_from(&panes, &[], &[], None, &no_git);

        let row = |kind: &str| {
            snap.agents
                .iter()
                .find(|agent| agent.kind.as_deref() == Some(kind))
                .unwrap_or_else(|| panic!("no {kind} row"))
        };
        assert_eq!(row("claude").pane_title, "");
        assert_eq!(row("claude").agent_label(), "claude");
        assert_eq!(row("codex").pane_title, "\u{2733} Build");
    }

    // ------------------------------------------ the last session of the popup's client

    use crate::cli::test_support::{Stub, calls, stub_tmux};

    /// The popup was told which client it is on, so its last session is asked of that client,
    /// by name, and no client is picked from a listing: with two on a session that would be
    /// the first the server lists, whichever the popup is on. A last session that is the
    /// current one is tmux reporting a fresh attach, and is not one to go back to.
    #[test]
    fn the_last_session_is_read_for_the_client_the_popup_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());
        let sessions = [session("main", 0), session("other-one", 1)];

        let asked = ask_client(&tmux, Some("/dev/ttys021"), false);

        assert_eq!(
            asked,
            Some(("/dev/ttys021".to_string(), "#{client_last_session}"))
        );
        assert!(
            calls(&log).is_empty(),
            "a client was picked from a listing: {:?}",
            calls(&log)
        );
        let got = parse_client_read("main", &sessions, Some("$9"), false).previous_session;
        assert_eq!(got.as_deref(), Some("$4"), "main, by its name");
        assert_eq!(
            parse_client_read("main", &sessions, Some("$4"), false).previous_session,
            None
        );
    }

    /// A `taw ui` typed in a pane has no launcher to name its client: it is picked from those
    /// attached, by hand, as it always was.
    #[test]
    fn with_no_client_named_the_last_session_is_read_for_the_one_picked_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(dir.path(), &Stub::default());

        let asked = ask_client(&tmux, None, false);

        assert_eq!(
            asked,
            Some(("/dev/ttys001".to_string(), "#{client_last_session}"))
        );
        let calls = calls(&log);
        assert!(
            calls.iter().any(|call| call[0] == "list-clients"),
            "{calls:?}"
        );
    }

    /// `session("api", 0)` is the attached one, holding pane `%300` in window `@0`;
    /// `session("docs", 1)` is detached, holding `%400` in `@0`.
    #[test]
    fn the_current_context_follows_the_pane_the_popup_was_opened_from() {
        let panes = [
            pane("api", 0, 0, Some(AgentState::Working)),
            pane("docs", 0, 0, Some(AgentState::Working)),
        ];
        let sessions = [session("api", 0), session("docs", 1)];
        let opened_from = panes[1].pane_id.clone();
        let snap = build_from(&panes, &sessions, &[], Some(&opened_from), &no_git);
        assert_eq!(snap.current.workspace_id.as_deref(), Some("w4"));
        assert_eq!(snap.current.tab_id.as_deref(), Some("w4:t0"));
    }

    #[test]
    fn the_current_context_falls_back_to_the_attached_workspace() {
        let panes = [pane("api", 0, 0, Some(AgentState::Working))];
        let sessions = [session("api", 0), session("docs", 1)];
        // No origin pane at all, and one that is no longer on the server.
        for current in [None, Some("%999")] {
            let snap = build_from(&panes, &sessions, &[], current, &no_git);
            assert_eq!(snap.current.workspace_id.as_deref(), Some("w3"));
            assert_eq!(snap.current.tab_id.as_deref(), Some("w3:t0"));
        }
    }

    #[test]
    fn the_current_context_is_empty_when_nothing_is_attached() {
        let detached = [session("docs", 1)];
        let snap = build_from(&[], &detached, &[], None, &no_git);
        assert_eq!(snap.current, agent_view::Current::default());
    }

    #[test]
    fn build_from_records_the_own_pane_and_its_session() {
        // `docs` (sessions[0]) is attached and listed first; `api` (sessions[1]) is
        // detached and listed second. The own pane sits in `api`, so `current_session`
        // only matches here by following the pane's own session — the first-attached
        // fallback (tested next) would answer `docs` instead.
        let sessions = [session("docs", 0), session("api", 1)];
        let panes = [pane("docs", 0, 0, None), pane("api", 0, 0, None)];
        let own = panes[1].pane_id.clone();
        let snap = build_from(&panes, &sessions, &[], Some(&own), &no_git);
        assert_eq!(snap.own_pane.as_deref(), Some(own.as_str()));
        assert_eq!(
            snap.current_session.as_deref(),
            Some(sessions[1].session_id.as_str())
        );
    }

    #[test]
    fn build_from_drops_an_own_pane_tmux_no_longer_has() {
        let sessions = [session("docs", 0)];
        let panes = [pane("docs", 0, 0, None)];
        let snap = build_from(&panes, &sessions, &[], Some("%99"), &no_git);
        assert_eq!(snap.own_pane, None);
        // The first attached session stands in, as for `current`.
        assert_eq!(
            snap.current_session.as_deref(),
            Some(sessions[0].session_id.as_str())
        );
    }

    #[test]
    fn previous_session_is_the_most_recently_attached_other_session() {
        let mut a = session("docs", 0);
        let mut b = session("api", 1);
        let mut c = session("notes", 2);
        a.session_last_attached = 1_700_000_300;
        b.session_last_attached = 1_700_000_200;
        c.session_last_attached = 0; // never attached
        assert_eq!(
            previous_session(
                &[a.clone(), b.clone(), c.clone()],
                Some(a.session_id.as_str())
            )
            .as_deref(),
            Some(b.session_id.as_str())
        );
        // Never the one you are in, even when it is the most recent.
        assert_eq!(
            previous_session(&[a.clone(), b.clone()], Some(a.session_id.as_str())).as_deref(),
            Some(b.session_id.as_str())
        );
        // Nothing else ever attached: none.
        assert_eq!(
            previous_session(&[a.clone(), c.clone()], Some(a.session_id.as_str())),
            None
        );
        // No current session known: the most recent of all.
        assert_eq!(
            previous_session(&[a.clone(), b.clone(), c.clone()], None).as_deref(),
            Some(a.session_id.as_str())
        );
    }

    #[test]
    fn build_from_reads_the_seen_flag_of_each_agent() {
        let mut seen = pane("api", 0, 0, Some(AgentState::Done));
        seen.agent.seen = true;
        let snap = build_from(&[seen], &[session("api", 0)], &[], None, &no_git);
        assert!(snap.agents[0].seen);
    }

    #[test]
    fn build_from_lists_only_agent_panes_and_every_session() {
        let panes = [
            pane("docs", 0, 0, Some(AgentState::Working)),
            pane("docs", 0, 1, None), // a plain shell: not an agent
            pane("api", 0, 0, Some(AgentState::Blocked)),
        ];
        let sessions = [session("api", 0), session("docs", 1)];
        let windows = [window("api", 0), window("docs", 0)];
        let snap = build_from(&panes, &sessions, &windows, None, &no_git);
        assert_eq!(snap.agents.len(), 2);
        assert_eq!(snap.spaces.len(), 2);
        assert!(snap.agents.iter().all(|a| a.state.is_some()));
    }

    /// `api`'s window is a split (two panes, one an agent); `docs` has one plain
    /// shell in its own window.
    #[test]
    fn build_from_lists_every_pane_in_tmux_order() {
        let agent_pane = pane("api", 0, 0, Some(AgentState::Working));
        let shell_pane = pane("api", 0, 1, None); // splits the agent's window
        let other_session_pane = pane("docs", 0, 0, None);
        let panes = [
            agent_pane.clone(),
            shell_pane.clone(),
            other_session_pane.clone(),
        ];
        let sessions = [session("api", 0), session("docs", 1)];
        let mut api_window = window("api", 0);
        api_window.window_panes = 2; // the split
        let windows = [api_window, window("docs", 0)];
        let snap = build_from(&panes, &sessions, &windows, None, &no_git);

        assert_eq!(snap.agents.len(), 1, "only the first pane is an agent");
        assert_eq!(
            snap.panes.len(),
            3,
            "every pane gets a row, not just agents"
        );
        let ids: Vec<&str> = snap.panes.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                agent_pane.pane_id.as_str(),
                shell_pane.pane_id.as_str(),
                other_session_pane.pane_id.as_str(),
            ],
            "session order, then window index, then pane index"
        );
        assert_eq!(
            snap.panes[0].window_panes, 2,
            "the split window has two panes"
        );
        assert_eq!(snap.panes[1].window_panes, 2);
        assert_eq!(snap.panes[2].window_panes, 1);
        assert_eq!(
            snap.panes[0].agent,
            Some(0),
            "the agent's pane points at its own AgentRow"
        );
        assert_eq!(snap.panes[1].agent, None, "a plain shell has no agent");
        assert_eq!(snap.panes[2].agent, None);
        assert_eq!(
            snap.agents[0].window_panes, 2,
            "AgentRow reports its window's pane count too"
        );
    }

    /// `window()`/`pane()` derive `window_id` from the window index alone, not the
    /// session, so api's window and docs's window are both `"@0"` here — same as a
    /// real tmux server never produces, but exactly the string collision the fallback's
    /// `(session_id, window_id)` join has to survive. No `WindowInfo` at all, so every
    /// `window_panes` below comes from the fallback's own count, not `list-windows`.
    #[test]
    fn build_from_falls_back_to_the_observed_pane_count_per_session_and_window() {
        let api_agent = pane("api", 0, 0, Some(AgentState::Working));
        let api_shell = pane("api", 0, 1, None); // splits api's window
        let docs_pane = pane("docs", 0, 0, None); // window_id "@0" too, other session
        let panes = [api_agent.clone(), api_shell.clone(), docs_pane.clone()];
        let sessions = [session("api", 0), session("docs", 1)];
        let snap = build_from(&panes, &sessions, &[], None, &no_git);

        let row = |id: &str| snap.panes.iter().find(|p| p.pane_id == id).unwrap();
        assert_eq!(
            row(api_agent.pane_id.as_str()).window_panes,
            2,
            "api's window has two panes"
        );
        assert_eq!(row(api_shell.pane_id.as_str()).window_panes, 2);
        assert_eq!(
            row(docs_pane.pane_id.as_str()).window_panes,
            1,
            "docs's one pane must not inherit api's count just because both windows are named \"@0\""
        );
        assert_eq!(
            snap.agents[0].window_panes, 2,
            "AgentRow's fallback is scoped by session the same way"
        );
    }

    #[test]
    fn build_from_keeps_session_path_on_spaces() {
        let mut api = session("api", 0);
        api.session_windows = 3;
        let snap = build_from(&[], &[api], &[], None, &no_git);
        assert_eq!(snap.spaces[0].session_path, PathBuf::from("/repos/api"));
        assert_eq!(
            snap.spaces[0].windows, 3,
            "SessionInfo::session_windows carries through"
        );
    }

    #[test]
    fn build_from_orders_agents_by_workspace_then_window_then_pane() {
        let mut second_pane = pane("api", 1, 3, Some(AgentState::Idle));
        second_pane.window_id = "@1".to_string();
        let panes = [
            pane("docs", 0, 0, Some(AgentState::Working)),
            second_pane,
            pane("api", 0, 0, Some(AgentState::Blocked)),
        ];
        // `list-sessions` order is the workspace order: api first, docs second.
        let sessions = [session("api", 0), session("docs", 1)];
        let windows = [window("api", 0), window("api", 1), window("docs", 0)];
        let snap = build_from(&panes, &sessions, &windows, None, &no_git);
        let order: Vec<(String, u32, u32)> = snap
            .agents
            .iter()
            .map(|a| (a.session_name.clone(), a.window_index, a.pane_index))
            .collect();
        assert_eq!(
            order,
            vec![
                ("api".to_string(), 0, 0),
                ("api".to_string(), 1, 3),
                ("docs".to_string(), 0, 0),
            ]
        );
        assert_eq!(snap.agents[0].space_order, 0);
        assert_eq!(snap.agents[2].space_order, 1);
    }

    #[test]
    fn build_from_rolls_the_worst_agent_state_up_to_the_workspace() {
        let panes = [
            pane("api", 0, 0, Some(AgentState::Idle)),
            pane("api", 0, 1, Some(AgentState::Blocked)),
            pane("api", 0, 2, Some(AgentState::Working)),
        ];
        let sessions = [session("api", 0)];
        let windows = [window("api", 0)];
        let snap = build_from(&panes, &sessions, &windows, None, &no_git);
        assert_eq!(snap.spaces[0].state, Some(AgentState::Blocked));
        assert_eq!(snap.spaces[0].agent_count, 3);
    }

    #[test]
    fn build_from_leaves_an_agent_less_workspace_with_no_state() {
        let panes = [pane("api", 0, 0, None)];
        let snap = build_from(
            &panes,
            &[session("api", 0)],
            &[window("api", 0)],
            None,
            &no_git,
        );
        assert_eq!(snap.spaces[0].state, None);
        assert_eq!(snap.spaces[0].agent_count, 0);
    }

    #[test]
    fn build_from_records_the_active_pane_of_each_workspace_for_jump_and_preview() {
        let mut active = pane("api", 0, 2, None);
        active.pane_active = true;
        active.window_active = true;
        let mut inactive = pane("api", 0, 0, None);
        inactive.pane_active = false;
        let snap = build_from(
            &[inactive, active.clone()],
            &[session("api", 0)],
            &[window("api", 0)],
            None,
            &no_git,
        );
        assert_eq!(
            snap.spaces[0].active_pane_id.as_deref(),
            Some(active.pane_id.as_str())
        );
    }

    #[test]
    fn build_from_attaches_git_info_per_workspace_path() {
        let info = git::Info {
            branch: Some("main".to_string()),
            ahead: 1,
            behind: 0,
            modified: 2,
            untracked: 0,
        };
        let lookup = |path: &Path| {
            (path == Path::new("/repos/api")).then(|| git::Info {
                branch: Some("main".to_string()),
                ahead: 1,
                behind: 0,
                modified: 2,
                untracked: 0,
            })
        };
        let snap = build_from(
            &[pane("api", 0, 0, None)],
            &[session("api", 0), session("docs", 1)],
            &[window("api", 0), window("docs", 0)],
            None,
            &lookup,
        );
        let api = snap
            .spaces
            .iter()
            .find(|s| s.session_name == "api")
            .unwrap();
        let docs = snap
            .spaces
            .iter()
            .find(|s| s.session_name == "docs")
            .unwrap();
        assert_eq!(api.git, Some(info));
        assert_eq!(docs.git, None);
    }

    #[test]
    fn rollup_prefers_blocked_then_working_then_done_then_idle_then_unknown() {
        use AgentState::*;
        assert_eq!(rollup([Some(Idle), Some(Blocked)]), Some(Blocked));
        assert_eq!(rollup([Some(Done), Some(Working)]), Some(Working));
        assert_eq!(rollup([Some(Idle), Some(Done)]), Some(Done));
        assert_eq!(rollup([Some(Unknown), Some(Idle)]), Some(Idle));
        assert_eq!(rollup([Some(Unknown)]), Some(Unknown));
        assert_eq!(rollup([None, Some(Idle)]), Some(Idle));
        assert_eq!(rollup([None, None]), None);
        assert_eq!(rollup([]), None);
    }

    #[test]
    fn git_status_text_is_compact_and_omits_zeroes() {
        assert_eq!(git_status_text(&git::Info::default()), "");
        assert_eq!(
            git_status_text(&git::Info {
                branch: None,
                ahead: 2,
                behind: 1,
                modified: 3,
                untracked: 4,
            }),
            "\u{b1}3 +4 \u{2191}2 \u{2193}1"
        );
        assert_eq!(
            git_status_text(&git::Info {
                modified: 1,
                ..Default::default()
            }),
            "\u{b1}1"
        );
    }

    #[test]
    fn strip_title_glyph_removes_one_leading_glyph_and_its_space() {
        assert_eq!(
            strip_title_glyph("\u{25cf} claude: review"),
            "claude: review"
        );
        assert_eq!(strip_title_glyph("claude: review"), "claude: review");
        assert_eq!(strip_title_glyph("\u{25cf}claude"), "\u{25cf}claude");
        assert_eq!(strip_title_glyph(""), "");
        assert_eq!(strip_title_glyph("* a"), "a");
    }

    #[test]
    fn agent_label_prefers_display_then_name_then_kind() {
        let mut row = AgentRow {
            kind: Some("claude".to_string()),
            ..Default::default()
        };
        assert_eq!(row.agent_label(), "claude");
        row.name = Some("reviewer".to_string());
        assert_eq!(row.agent_label(), "reviewer");
        row.display = Some("Reviewer".to_string());
        assert_eq!(row.agent_label(), "Reviewer");
        assert_eq!(AgentRow::default().agent_label(), "unknown");
    }

    #[test]
    fn state_icons_match_the_status_rail_glyphs() {
        use AgentState::*;
        assert_eq!(
            state_icon(Some(Working), StatusIndicators::Dots),
            "\u{25cf}"
        );
        assert_eq!(
            state_icon(Some(Blocked), StatusIndicators::Dots),
            "\u{25cf}"
        );
        assert_eq!(
            state_icon(Some(Blocked), StatusIndicators::Symbols),
            "\u{23f8}"
        );
        assert_eq!(
            state_icon(Some(Working), StatusIndicators::Symbols),
            "\u{23f5}"
        );
        assert_eq!(
            state_icon(Some(Done), StatusIndicators::Symbols),
            "\u{2713}"
        );
        assert_eq!(
            state_icon(Some(Idle), StatusIndicators::Symbols),
            "\u{25cb}"
        );
        assert_eq!(state_icon(Some(Unknown), StatusIndicators::Symbols), "?");
        assert_eq!(state_icon(None, StatusIndicators::Dots), "\u{b7}");
    }

    #[test]
    fn custom_tokens_are_only_read_when_a_row_asks_for_one() {
        let default = Config::default();
        assert!(!rows_use_custom_tokens(&default.ui.sidebar.agents));
        let cfg: Config =
            toml::from_str("[ui.sidebar.agents]\nrows = [[\"state_icon\", \"$jj_status\"]]\n")
                .unwrap();
        assert!(rows_use_custom_tokens(&cfg.ui.sidebar.agents));
        let by_agent: Config =
            toml::from_str("[ui.sidebar.agents.rows_by_agent]\nclaude = [[\"$branch_hint\"]]\n")
                .unwrap();
        assert!(rows_use_custom_tokens(&by_agent.ui.sidebar.agents));
    }

    #[test]
    fn the_git_worker_answers_off_thread_and_stops_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let worker = GitWorker::new(Duration::from_secs(60));
        // Nothing requested yet: no answer, and no blocking.
        assert_eq!(worker.get(dir.path()), None);
        worker.request(vec![dir.path().to_path_buf()]);
        // The answer for a non-repository is `None`, so wait for the *entry* to appear.
        let mut answered = false;
        for _ in 0..200 {
            if worker.answers.lock().unwrap().contains_key(dir.path()) {
                answered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(answered, "the git worker never answered");
    }
}
