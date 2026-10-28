//! The sample world, as a [`Snapshot`], for every test that needs one.
//!
//! Five sessions, split windows, two agents sharing one window, one parked worktree. [`tree`](super::tree) and
//! [`app`](super::app) both build their expectations off it, so a row list or a cursor
//! position asserted in either can be read straight off this one — and neither can drift
//! from the other by editing a private copy.

use std::path::PathBuf;

use crate::git;
use crate::session::worktree::WorktreeRecord;
use crate::store::AgentState;
use crate::tui::data::{AgentRow, PaneRow, Snapshot, SpaceRow};

/// Seconds since midnight.
pub fn t(h: u64, m: u64) -> u64 {
    h * 3600 + m * 60
}

/// `(modified, untracked, ahead, behind)`: a git status in numbers.
pub type Counts = (u32, u32, u32, u32);

/// One session at `path`, named after its last directory the way the popup
/// names it.
pub fn space(n: u32, path: &str, branch: &str, counts: Counts) -> SpaceRow {
    let (modified, untracked, ahead, behind) = counts;
    SpaceRow {
        session_id: format!("${n}"),
        session_name: path.rsplit('/').next().unwrap_or(path).to_string(),
        session_path: PathBuf::from(path),
        windows: 4,
        group_size: 0,
        attached: n == 1,
        active_window_id: format!("@{n}1"),
        active_pane_id: Some(format!("%{n}11")),
        state: None,
        agent_count: 0,
        git: Some(git::Info {
            branch: Some(branch.to_string()),
            ahead,
            behind,
            modified,
            untracked,
        }),
        machine: None,
        real_path: None,
    }
}

/// `(session, window index, window name, panes in that window, pane index, command)` —
/// every pane of the five sessions, in tmux order.
type PaneSpec = (u32, u32, &'static str, u32, u32, &'static str);

fn pane_specs() -> Vec<PaneSpec> {
    vec![
        // $1 mobile
        (1, 1, "zsh", 1, 1, "zsh"),
        (1, 2, "review", 1, 1, "zsh"),
        (1, 3, "nvim", 1, 1, "nvim"),
        (1, 4, "zsh", 2, 1, "zsh"),
        (1, 4, "zsh", 2, 2, "htop"),
        // $2 web
        (2, 1, "zsh", 1, 1, "zsh"),
        (2, 2, "zsh", 2, 1, "zsh"),
        (2, 2, "zsh", 2, 2, "zsh"),
        (2, 3, "tests", 1, 1, "zsh"),
        (2, 4, "server", 1, 1, "node"),
        // $3 infra
        (3, 1, "zsh", 2, 1, "zsh"),
        (3, 1, "zsh", 2, 2, "zsh"),
        (3, 2, "docs", 1, 1, "nvim"),
        (3, 3, "logs", 1, 1, "tail"),
        (3, 4, "zsh", 1, 1, "zsh"),
        // $4 api
        (4, 1, "zsh", 1, 1, "zsh"),
        // $5 notes
        (5, 1, "nvim", 1, 1, "nvim"),
        (5, 2, "zsh", 1, 1, "zsh"),
    ]
}

/// `(pane id, kind, state, seen, state_since, pane title)` — the agents occupying some of
/// those panes.
type AgentSpec = (
    &'static str,
    &'static str,
    AgentState,
    bool,
    u64,
    &'static str,
);

fn agent_specs() -> Vec<AgentSpec> {
    use AgentState::{Blocked, Done, Idle, Working};
    vec![
        (
            "%111",
            "claude",
            Working,
            true,
            t(17, 40),
            "✳ Tidy page styles and fonts",
        ),
        ("%121", "claude", Idle, true, t(16, 10), "✳ Review PR 42"),
        (
            "%221",
            "claude",
            Working,
            false,
            t(17, 52),
            "✳ Invoice parser: credit notes",
        ),
        ("%222", "gemini", Idle, true, t(15, 5), ""),
        ("%231", "codex", Done, false, t(17, 50), "fix flaky tests"),
        (
            "%311",
            "claude",
            Working,
            true,
            t(17, 31),
            "✳ Migrate auth to OIDC",
        ),
        (
            "%341",
            "claude",
            Blocked,
            true,
            t(17, 20),
            "✳ Error report review and fixes",
        ),
    ]
}

pub fn spaces() -> Vec<SpaceRow> {
    // Web's open window is 2, where its two agents sit, not the
    // generic first window every other session in this fixture opens on.
    let mut web = space(2, "~/code/web", "feat/invoices", (1, 0, 0, 0));
    web.active_window_id = "@22".to_string();
    vec![
        space(1, "~/code/mobile", "master", (3, 0, 1, 0)),
        web,
        space(3, "~/clients/infra", "main", (0, 0, 0, 0)),
        space(4, "~/code/api", "main", (0, 0, 0, 0)),
        space(5, "~/notes", "main", (0, 2, 0, 0)),
    ]
}

/// The [`AgentRow`] `data::build_from` derives from an agent-occupied `pane`.
fn agent_row(pane: &PaneRow, session_name: &str, space_order: usize, spec: AgentSpec) -> AgentRow {
    let (_, kind, state, seen, since, title) = spec;
    AgentRow {
        pane_id: pane.pane_id.clone(),
        session_id: pane.session_id.clone(),
        session_name: session_name.to_string(),
        window_id: pane.window_id.clone(),
        window_index: pane.window_index,
        window_name: pane.window_name.clone(),
        window_panes: pane.window_panes,
        pane_index: pane.pane_index,
        pane_current_command: pane.pane_current_command.clone(),
        pane_title: title.to_string(),
        kind: Some(kind.to_string()),
        state: Some(state),
        state_since: since,
        seen,
        space_order,
        ..Default::default()
    }
}

/// The five sandboxed agents in the sample world: mobile's claude in
/// window 1, the row the popup opens on, and its review claude, in a profile of yours, `box`;
/// web's claude on its open window and its unseen codex, in taw's own, default; infra's blocked
/// claude, a `box` typed by hand, which taw only recognised.
pub fn sandboxed(snap: &mut Snapshot) {
    for (pane, profile) in [
        ("%111", Some("box")),
        ("%121", Some("box")),
        ("%221", Some("default")),
        ("%231", Some("default")),
        ("%341", None),
    ] {
        let agent = snap
            .agents
            .iter_mut()
            .find(|agent| agent.pane_id == pane)
            .expect("a sample agent");
        agent.container = Some("docker".to_string());
        agent.sandbox = profile.map(str::to_string);
    }
}

/// The sample world: five sessions, eighteen panes, seven agents.
pub fn snapshot() -> Snapshot {
    let spaces = spaces();
    let specs = agent_specs();
    let mut panes = Vec::new();
    let mut agents: Vec<AgentRow> = Vec::new();
    for (session, window_index, window_name, window_panes, pane_index, command) in pane_specs() {
        let space = session as usize - 1;
        let mut pane = PaneRow {
            pane_id: format!("%{session}{window_index}{pane_index}"),
            session_id: format!("${session}"),
            window_id: format!("@{session}{window_index}"),
            window_index,
            window_name: window_name.to_string(),
            window_panes,
            pane_index,
            pane_current_command: command.to_string(),
            pane_active: pane_index == 1,
            alternate_on: false,
            agent: None,
            machine: None,
        };
        // `data::build_from` fills `Snapshot::agents` by walking the panes in order, so
        // the agent rows come out in tmux order here too.
        if let Some(&spec) = specs.iter().find(|(id, ..)| *id == pane.pane_id) {
            agents.push(agent_row(&pane, &spaces[space].session_name, space, spec));
            pane.agent = Some(agents.len() - 1);
        }
        panes.push(pane);
    }
    Snapshot {
        agents,
        spaces,
        panes,
        // Mobile's claude (`%111`) is the pane the popup was opened from, in
        // mobile (`$1`); api (`$4`) is the session to go back to.
        own_pane: Some("%111".into()),
        current_session: Some("$1".into()),
        previous_session: Some("$4".into()),
        ..Snapshot::default()
    }
}

/// The path of the sample's one parked worktree.
pub const PARKED: &str = "~/code/web--feat-pdf";

/// That worktree's record: cut from web (`$2`, so workspace `w2`), no session at its path.
pub fn worktrees() -> Vec<WorktreeRecord> {
    vec![WorktreeRecord {
        workspace_id: "w6".to_string(),
        session_name: "web--feat-pdf".to_string(),
        path: PathBuf::from(PARKED),
        branch: "feat/pdf".to_string(),
        base: Some("feat/invoices".to_string()),
        repo_root: PathBuf::from("~/code/web"),
        source_workspace_id: "w2".to_string(),
        created_unix_ms: 0,
        machine: None,
        real_path: None,
        real_repo_root: None,
    }]
}
