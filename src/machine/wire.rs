//! The lines `taw machine feed` and the app say to each other: one JSON object a line.
//!
//! Down is the feed to the app (`hello`, `state`, `toast`, `ping`, `left`), up is the app to the feed
//! (`lease`, `walk`). Every struct has a default for every field, so a line from an older or a newer taw
//! still reads, and serde leaves out the fields it does not know. A line whose `type` is not one of
//! these is not a failure of the stream: [`parse_down`] and [`parse_up`] say so apart from a line that
//! is not JSON, and the reader skips it.
//!
//! The state mirrors the rows the popup draws ([`crate::tui::data`]) with tmux's own ids (`%7`, `$3`,
//! `@5`), and the agent states as their lowercase words.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::git;
use crate::session::worktree::WorktreeRecord;
use crate::tui::data::{AgentRow, PaneRow, SpaceRow};

/// The protocol this taw speaks.
pub const PROTOCOL: u32 = 1;

/// A line the feed sends to the app.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Down {
    Hello(Hello),
    State(State),
    Toast(Toast),
    Ping,
    Left(Left),
}

/// A line the app sends to the feed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Up {
    Lease(LeaseLine),
    Walk(WalkLine),
}

/// The first line of a feed: who is speaking.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Hello {
    /// The version of the taw that sends it.
    pub taw: String,
    pub protocol: u32,
    /// The host name, without a domain.
    pub host: String,
    /// `<tmux server pid> <server start unix seconds>`, or empty when no tmux server runs.
    pub server: String,
}

/// Everything the list draws of one machine, and the records of the worktrees its sessions run, which the
/// list never draws but the app reads to name them when it asks before closing a session.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct State {
    pub spaces: Vec<WireSpace>,
    pub panes: Vec<WirePane>,
    pub agents: Vec<WireAgent>,
    pub worktrees: Vec<WireWorktree>,
    /// The records of the worktrees one of this machine's sessions is running: the ones the list does not
    /// draw as rows ([`State::worktrees`] holds the closed ones). The app uses them to name a session's
    /// worktree sessions when it asks before closing it. Left out when there are none; a feed that sends
    /// none leaves the app asking the question that names no count.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub open_worktrees: Vec<WireWorktree>,
}

/// A notification the machine's watcher announced.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Toast {
    pub kind: String,
    /// The pane it is about, as that machine knows it (`%31`).
    pub pane: String,
    /// The line the watcher's own toast shows.
    pub text: String,
    pub title: String,
    pub body: String,
    pub agent: String,
    pub agent_kind: String,
    pub location: String,
    /// Whether the app that holds the lease is the one that speaks it: the machine saw a fresh lease.
    pub lease: bool,
}

/// An attach that ended on the machine: whoever was attached there has gone.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Left {
    pub holder: String,
    pub pane: String,
    pub go: Option<String>,
}

/// `{"type":"lease","holder":"<host>-<pid>"}`: the app that holds this machine's attention says so.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LeaseLine {
    pub holder: String,
}

/// `{"type":"walk","you":"studio","stops":["%7","studio:%16","pi:%3"]}`: the app's walk of every machine's
/// stops, in the app's own names. `you` is the name the app gives this machine, so that the machine can
/// tell its own ids from the others'.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WalkLine {
    pub you: String,
    pub stops: Vec<String>,
}

/// One tmux session ([`SpaceRow`]).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WireSpace {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub windows: u32,
    pub group_size: u32,
    pub attached: bool,
    pub active_window_id: String,
    pub active_pane_id: Option<String>,
    /// The worst state of its agents, as its lowercase word.
    pub state: Option<String>,
    pub agent_count: usize,
    pub git: Option<WireGit>,
    /// `session_path` resolved on the machine (symlinks followed), sent only when that differs from it.
    /// For matching a session with a worktree's folders there; nothing is drawn from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_path: Option<String>,
}

/// What git says of a session's folder ([`git::Info`]).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WireGit {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub modified: u32,
    pub untracked: u32,
}

/// One tmux pane ([`PaneRow`]), without the index of its agent: the agents come in their own list.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WirePane {
    pub pane_id: String,
    pub session_id: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_panes: u32,
    pub pane_index: u32,
    pub pane_current_command: String,
    pub pane_active: bool,
    pub alternate_on: bool,
}

/// One pane an agent occupies ([`AgentRow`]).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WireAgent {
    pub pane_id: String,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_panes: u32,
    pub pane_index: u32,
    pub pane_current_command: String,
    pub pane_title: String,
    pub alternate_on: bool,
    pub pane_dead: bool,
    pub pane_pid: u32,
    pub kind: Option<String>,
    /// `idle`, `working`, `blocked`, `done` or `unknown`.
    pub state: Option<String>,
    pub state_since: u64,
    /// Suspended with Ctrl+Z (its state is `idle`): its row says `suspended`. A feed from a taw that
    /// sends no such field reads as not suspended.
    #[serde(default)]
    pub suspended: bool,
    pub name: Option<String>,
    pub display: Option<String>,
    pub seen: bool,
    pub container: Option<String>,
    pub sandbox: Option<String>,
    pub tokens: BTreeMap<String, String>,
    pub space_order: usize,
}

/// One worktree record ([`WorktreeRecord`]): a closed worktree, which the list draws as a row, or one a
/// session is running, which it never draws ([`State::open_worktrees`]).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WireWorktree {
    pub workspace_id: String,
    pub session_name: String,
    pub path: String,
    pub branch: String,
    pub base: Option<String>,
    pub repo_root: String,
    pub source_workspace_id: String,
    pub created_unix_ms: u64,
    /// `path` and `repo_root` resolved on the machine, each sent only when it differs from the one
    /// above: for matching with the sessions' folders there, never drawn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub real_repo_root: Option<String>,
}

/// `path` with its symlinks followed, as text, when that works and says something else than `path`
/// does: what the feed sends beside a folder so the app can match it with another folder of the
/// machine without a filesystem to resolve them on.
pub fn resolved_if_different(path: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    (real != path).then(|| real.display().to_string())
}

impl From<&git::Info> for WireGit {
    fn from(info: &git::Info) -> WireGit {
        WireGit {
            branch: info.branch.clone(),
            ahead: info.ahead,
            behind: info.behind,
            modified: info.modified,
            untracked: info.untracked,
        }
    }
}

impl From<&SpaceRow> for WireSpace {
    fn from(row: &SpaceRow) -> WireSpace {
        WireSpace {
            session_id: row.session_id.clone(),
            session_name: row.session_name.clone(),
            session_path: row.session_path.display().to_string(),
            windows: row.windows,
            group_size: row.group_size,
            attached: row.attached,
            active_window_id: row.active_window_id.clone(),
            active_pane_id: row.active_pane_id.clone(),
            state: row.state.map(|state| state.as_str().to_string()),
            agent_count: row.agent_count,
            git: row.git.as_ref().map(WireGit::from),
            real_path: resolved_if_different(&row.session_path),
        }
    }
}

impl From<&PaneRow> for WirePane {
    fn from(row: &PaneRow) -> WirePane {
        WirePane {
            pane_id: row.pane_id.clone(),
            session_id: row.session_id.clone(),
            window_id: row.window_id.clone(),
            window_index: row.window_index,
            window_name: row.window_name.clone(),
            window_panes: row.window_panes,
            pane_index: row.pane_index,
            pane_current_command: row.pane_current_command.clone(),
            pane_active: row.pane_active,
            alternate_on: row.alternate_on,
        }
    }
}

impl From<&AgentRow> for WireAgent {
    fn from(row: &AgentRow) -> WireAgent {
        WireAgent {
            pane_id: row.pane_id.clone(),
            session_id: row.session_id.clone(),
            session_name: row.session_name.clone(),
            window_id: row.window_id.clone(),
            window_index: row.window_index,
            window_name: row.window_name.clone(),
            window_panes: row.window_panes,
            pane_index: row.pane_index,
            pane_current_command: row.pane_current_command.clone(),
            pane_title: row.pane_title.clone(),
            alternate_on: row.alternate_on,
            pane_dead: row.pane_dead,
            pane_pid: row.pane_pid,
            kind: row.kind.clone(),
            state: row.state.map(|state| state.as_str().to_string()),
            state_since: row.state_since,
            suspended: row.suspended,
            name: row.name.clone(),
            display: row.display.clone(),
            seen: row.seen,
            container: row.container.clone(),
            sandbox: row.sandbox.clone(),
            tokens: row.tokens.clone(),
            space_order: row.space_order,
        }
    }
}

impl From<&WorktreeRecord> for WireWorktree {
    fn from(record: &WorktreeRecord) -> WireWorktree {
        WireWorktree {
            workspace_id: record.workspace_id.clone(),
            session_name: record.session_name.clone(),
            path: record.path.display().to_string(),
            branch: record.branch.clone(),
            base: record.base.clone(),
            repo_root: record.repo_root.display().to_string(),
            source_workspace_id: record.source_workspace_id.clone(),
            created_unix_ms: record.created_unix_ms,
            real_path: resolved_if_different(&record.path),
            real_repo_root: resolved_if_different(&record.repo_root),
        }
    }
}

/// Why a line did not become a [`Down`] or an [`Up`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineError {
    /// Not JSON at all.
    NotJson,
    /// JSON, but not an object with a `type` taw knows: a line of a newer taw, to skip.
    UnknownType(String),
    /// A type taw knows whose fields do not fit.
    Malformed(String),
}

fn parse_typed<T: serde::de::DeserializeOwned>(line: &str, known: &[&str]) -> Result<T, LineError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| LineError::NotJson)?;
    let kind = value.get("type").and_then(|kind| kind.as_str());
    match kind {
        Some(kind) if known.contains(&kind) => {
            serde_json::from_value(value).map_err(|err| LineError::Malformed(err.to_string()))
        }
        other => Err(LineError::UnknownType(
            other.unwrap_or_default().to_string(),
        )),
    }
}

/// `text` as plain text for a terminal: without control characters (C0, DEL and C1), without a whole
/// escape sequence and cut at [`super::clean::MAX_TEXT`], by [`super::clean::plain`], which is also what
/// takes them out of what ssh says ([`super::ssh::plain`]).
pub fn clean_text(text: &str) -> String {
    super::clean::plain(text)
}

fn clean_option(text: Option<String>) -> Option<String> {
    text.map(|text| clean_text(&text))
}

impl Down {
    /// The line with every string in it through [`clean_text`]: what the far machine sent is text a
    /// terminal may be given only after this.
    pub fn cleaned(self) -> Down {
        match self {
            Down::Hello(hello) => Down::Hello(Hello {
                taw: clean_text(&hello.taw),
                protocol: hello.protocol,
                host: clean_text(&hello.host),
                server: clean_text(&hello.server),
            }),
            Down::State(state) => Down::State(State {
                spaces: state.spaces.into_iter().map(WireSpace::cleaned).collect(),
                panes: state.panes.into_iter().map(WirePane::cleaned).collect(),
                agents: state.agents.into_iter().map(WireAgent::cleaned).collect(),
                worktrees: state
                    .worktrees
                    .into_iter()
                    .map(WireWorktree::cleaned)
                    .collect(),
                open_worktrees: state
                    .open_worktrees
                    .into_iter()
                    .map(WireWorktree::cleaned)
                    .collect(),
            }),
            Down::Toast(toast) => Down::Toast(Toast {
                kind: clean_text(&toast.kind),
                pane: clean_text(&toast.pane),
                text: clean_text(&toast.text),
                title: clean_text(&toast.title),
                body: clean_text(&toast.body),
                agent: clean_text(&toast.agent),
                agent_kind: clean_text(&toast.agent_kind),
                location: clean_text(&toast.location),
                lease: toast.lease,
            }),
            Down::Ping => Down::Ping,
            Down::Left(left) => Down::Left(Left {
                holder: clean_text(&left.holder),
                pane: clean_text(&left.pane),
                go: clean_option(left.go),
            }),
        }
    }
}

// Each `cleaned` names every field, so that a field added to a row does not pass uncleaned.

impl WireSpace {
    fn cleaned(self) -> WireSpace {
        let WireSpace {
            session_id,
            session_name,
            session_path,
            windows,
            group_size,
            attached,
            active_window_id,
            active_pane_id,
            state,
            agent_count,
            git,
            real_path,
        } = self;
        WireSpace {
            session_id: clean_text(&session_id),
            session_name: clean_text(&session_name),
            session_path: clean_text(&session_path),
            windows,
            group_size,
            attached,
            active_window_id: clean_text(&active_window_id),
            active_pane_id: clean_option(active_pane_id),
            state: clean_option(state),
            agent_count,
            git: git.map(|git| WireGit {
                branch: clean_option(git.branch),
                ahead: git.ahead,
                behind: git.behind,
                modified: git.modified,
                untracked: git.untracked,
            }),
            real_path: clean_option(real_path),
        }
    }
}

impl WirePane {
    fn cleaned(self) -> WirePane {
        let WirePane {
            pane_id,
            session_id,
            window_id,
            window_index,
            window_name,
            window_panes,
            pane_index,
            pane_current_command,
            pane_active,
            alternate_on,
        } = self;
        WirePane {
            pane_id: clean_text(&pane_id),
            session_id: clean_text(&session_id),
            window_id: clean_text(&window_id),
            window_index,
            window_name: clean_text(&window_name),
            window_panes,
            pane_index,
            pane_current_command: clean_text(&pane_current_command),
            pane_active,
            alternate_on,
        }
    }
}

impl WireAgent {
    fn cleaned(self) -> WireAgent {
        let WireAgent {
            pane_id,
            session_id,
            session_name,
            window_id,
            window_index,
            window_name,
            window_panes,
            pane_index,
            pane_current_command,
            pane_title,
            alternate_on,
            pane_dead,
            pane_pid,
            kind,
            state,
            state_since,
            suspended,
            name,
            display,
            seen,
            container,
            sandbox,
            tokens,
            space_order,
        } = self;
        WireAgent {
            pane_id: clean_text(&pane_id),
            session_id: clean_text(&session_id),
            session_name: clean_text(&session_name),
            window_id: clean_text(&window_id),
            window_index,
            window_name: clean_text(&window_name),
            window_panes,
            pane_index,
            pane_current_command: clean_text(&pane_current_command),
            pane_title: clean_text(&pane_title),
            alternate_on,
            pane_dead,
            pane_pid,
            kind: clean_option(kind),
            state: clean_option(state),
            state_since,
            suspended,
            name: clean_option(name),
            display: clean_option(display),
            seen,
            container: clean_option(container),
            sandbox: clean_option(sandbox),
            tokens: tokens
                .into_iter()
                .map(|(key, value)| (clean_text(&key), clean_text(&value)))
                .collect(),
            space_order,
        }
    }
}

impl WireWorktree {
    fn cleaned(self) -> WireWorktree {
        let WireWorktree {
            workspace_id,
            session_name,
            path,
            branch,
            base,
            repo_root,
            source_workspace_id,
            created_unix_ms,
            real_path,
            real_repo_root,
        } = self;
        WireWorktree {
            workspace_id: clean_text(&workspace_id),
            session_name: clean_text(&session_name),
            path: clean_text(&path),
            branch: clean_text(&branch),
            base: clean_option(base),
            repo_root: clean_text(&repo_root),
            source_workspace_id: clean_text(&source_workspace_id),
            created_unix_ms,
            real_path: clean_option(real_path),
            real_repo_root: clean_option(real_repo_root),
        }
    }
}

/// A line from the feed.
pub fn parse_down(line: &str) -> Result<Down, LineError> {
    parse_typed(line, &["hello", "state", "toast", "ping", "left"])
}

/// A line from the app.
pub fn parse_up(line: &str) -> Result<Up, LineError> {
    parse_typed(line, &["lease", "walk"])
}

/// The line as it is written: the object and a newline.
pub fn render(line: &Down) -> String {
    let mut text = serde_json::to_string(line).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AgentState;

    fn every_down() -> Vec<Down> {
        vec![
            Down::Hello(Hello {
                taw: "0.1.0".into(),
                protocol: 1,
                host: "studio".into(),
                server: "4211 1790000000".into(),
            }),
            Down::State(State {
                spaces: vec![WireSpace {
                    session_id: "$3".into(),
                    session_name: "garden".into(),
                    session_path: "/home/a/garden".into(),
                    windows: 2,
                    attached: true,
                    active_pane_id: Some("%7".into()),
                    state: Some("working".into()),
                    agent_count: 1,
                    git: Some(WireGit {
                        branch: Some("main".into()),
                        ahead: 1,
                        ..WireGit::default()
                    }),
                    ..WireSpace::default()
                }],
                panes: vec![WirePane {
                    pane_id: "%7".into(),
                    pane_active: true,
                    ..WirePane::default()
                }],
                agents: vec![WireAgent {
                    pane_id: "%7".into(),
                    kind: Some("claude".into()),
                    state: Some("blocked".into()),
                    tokens: BTreeMap::from([("branch".to_string(), "main".to_string())]),
                    ..WireAgent::default()
                }],
                worktrees: vec![WireWorktree {
                    branch: "fix".into(),
                    base: Some("main".into()),
                    ..WireWorktree::default()
                }],
                open_worktrees: vec![WireWorktree {
                    branch: "open".into(),
                    ..WireWorktree::default()
                }],
            }),
            Down::Toast(Toast {
                kind: "blocked".into(),
                pane: "%7".into(),
                text: "garden needs you".into(),
                title: "t".into(),
                body: "b".into(),
                agent: "claude".into(),
                agent_kind: "claude".into(),
                location: "garden".into(),
                lease: true,
            }),
            Down::Ping,
            Down::Left(Left {
                holder: "laptop-123".into(),
                pane: "%7".into(),
                go: Some("pi:%3".into()),
            }),
            Down::Left(Left::default()),
        ]
    }

    #[test]
    fn every_down_line_round_trips() {
        for line in every_down() {
            let text = render(&line);
            assert!(text.ends_with('\n') && !text[..text.len() - 1].contains('\n'));
            assert_eq!(parse_down(text.trim_end()), Ok(line.clone()), "{text}");
        }
    }

    #[test]
    fn every_up_line_round_trips() {
        for line in [
            Up::Lease(LeaseLine {
                holder: "laptop-9".into(),
            }),
            Up::Walk(WalkLine {
                you: "studio".into(),
                stops: vec!["%7".into(), "studio:%16".into(), "pi:%3".into()],
            }),
        ] {
            let text = serde_json::to_string(&line).unwrap();
            assert_eq!(parse_up(&text), Ok(line));
        }
    }

    #[test]
    fn the_designs_hello_line_parses() {
        let line = r#"{"type":"hello","taw":"0.4.0","protocol":1,"host":"studio","server":"4211 1790000000"}"#;
        assert_eq!(
            parse_down(line),
            Ok(Down::Hello(Hello {
                taw: "0.4.0".into(),
                protocol: 1,
                host: "studio".into(),
                server: "4211 1790000000".into()
            }))
        );
    }

    #[test]
    fn the_walk_line_is_written_as_the_far_side_reads_it() {
        let line = Up::Walk(WalkLine {
            you: "studio".into(),
            stops: vec!["%7".into(), "studio:%16".into(), "pi:%3".into()],
        });
        assert_eq!(
            serde_json::to_string(&line).unwrap(),
            r#"{"type":"walk","you":"studio","stops":["%7","studio:%16","pi:%3"]}"#
        );
    }

    #[test]
    fn an_agent_from_a_taw_that_sends_no_suspended_field_is_not_suspended() {
        let agent: WireAgent =
            serde_json::from_str(r#"{"pane_id":"%3","kind":"claude","state":"idle"}"#).unwrap();
        assert!(!agent.suspended);
        assert_eq!(agent.state.as_deref(), Some("idle"));
        let suspended = WireAgent {
            suspended: true,
            ..agent
        };
        let line = serde_json::to_string(&suspended).unwrap();
        assert!(line.contains(r#""suspended":true"#), "{line}");
        assert_eq!(serde_json::from_str::<WireAgent>(&line).unwrap(), suspended);
        // The feed is cleaned on its way in, and keeps the suspend.
        let down = Down::State(State {
            agents: vec![suspended],
            ..State::default()
        })
        .cleaned();
        let Down::State(state) = down else {
            panic!("not a state line: {down:?}");
        };
        assert!(state.agents[0].suspended);
    }

    #[test]
    fn a_lease_line_with_no_holder_still_parses() {
        assert_eq!(
            parse_up(r#"{"type":"lease"}"#),
            Ok(Up::Lease(LeaseLine::default()))
        );
        assert_eq!(
            parse_up(r#"{"type":"walk"}"#),
            Ok(Up::Walk(WalkLine::default()))
        );
    }

    #[test]
    fn a_line_with_an_unknown_field_parses_and_the_field_is_dropped() {
        let line = r#"{"type":"hello","taw":"9","later":{"a":[1]},"protocol":1}"#;
        match parse_down(line) {
            Ok(Down::Hello(hello)) => assert_eq!((hello.taw.as_str(), hello.protocol), ("9", 1)),
            other => panic!("{other:?}"),
        }
        let state = r#"{"type":"state","spaces":[{"session_id":"$1","new":true}],"extra":0}"#;
        match parse_down(state) {
            Ok(Down::State(state)) => assert_eq!(state.spaces[0].session_id, "$1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_type_is_told_apart_from_bad_json() {
        assert_eq!(
            parse_down(r#"{"type":"weather","x":1}"#),
            Err(LineError::UnknownType("weather".into()))
        );
        assert_eq!(
            parse_down(r#"{"no":"type"}"#),
            Err(LineError::UnknownType(String::new()))
        );
        assert_eq!(
            parse_down("[1]"),
            Err(LineError::UnknownType(String::new()))
        );
        assert_eq!(parse_down("this is not json"), Err(LineError::NotJson));
        assert_eq!(parse_down(""), Err(LineError::NotJson));
        assert_eq!(
            parse_up(r#"{"type":"ping"}"#),
            Err(LineError::UnknownType("ping".into()))
        );
    }

    #[test]
    fn a_toast_or_a_left_line_with_no_fields_reads_as_the_default() {
        assert_eq!(
            parse_down(r#"{"type":"toast"}"#),
            Ok(Down::Toast(Toast::default()))
        );
        assert_eq!(
            parse_down(r#"{"type":"left"}"#),
            Ok(Down::Left(Left::default()))
        );
    }

    #[test]
    fn a_state_whose_rows_have_no_fields_reads_as_default_rows() {
        let line =
            r#"{"type":"state","spaces":[{"git":{}}],"panes":[{}],"agents":[{}],"worktrees":[{}]}"#;
        assert_eq!(
            parse_down(line),
            Ok(Down::State(State {
                spaces: vec![WireSpace {
                    git: Some(WireGit::default()),
                    ..WireSpace::default()
                }],
                panes: vec![WirePane::default()],
                agents: vec![WireAgent::default()],
                worktrees: vec![WireWorktree::default()],
                open_worktrees: Vec::new(),
            }))
        );
    }

    /// Text with an escape sequence, a bell and a C1 control in it.
    const DIRTY: &str = "a\u{1b}[2Jb\u{7}c\u{1b}]0;title\u{7}d\u{9b}e\u{7f}f";

    #[test]
    fn clean_text_takes_out_escape_sequences_and_every_control_character() {
        assert_eq!(clean_text(DIRTY), "abcdef");
        assert_eq!(
            clean_text("plain text, with  spaces and \u{e9}"),
            "plain text, with  spaces and \u{e9}"
        );
        assert_eq!(clean_text("a\nb\tc\r"), "abc");
    }

    fn text_leaves(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => found.push(text.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|v| text_leaves(v, found)),
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    found.push(key.clone());
                    text_leaves(value, found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn a_toast_comes_out_of_cleaned_without_control_characters() {
        let toast = Toast {
            kind: DIRTY.into(),
            pane: DIRTY.into(),
            text: DIRTY.into(),
            title: format!("{DIRTY} title"),
            body: format!("body {DIRTY}"),
            agent: DIRTY.into(),
            agent_kind: DIRTY.into(),
            location: DIRTY.into(),
            lease: true,
        };
        match Down::Toast(toast).cleaned() {
            Down::Toast(toast) => {
                assert_eq!(toast.title, "abcdef title");
                assert_eq!(toast.body, "body abcdef");
                assert_eq!(toast.text, "abcdef");
                assert!(toast.lease);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_states_rows_and_a_hello_and_a_left_line_come_out_of_cleaned_clean() {
        let state = Down::State(State {
            spaces: vec![WireSpace {
                session_id: DIRTY.into(),
                session_name: DIRTY.into(),
                session_path: DIRTY.into(),
                active_window_id: DIRTY.into(),
                active_pane_id: Some(DIRTY.into()),
                state: Some(DIRTY.into()),
                git: Some(WireGit {
                    branch: Some(DIRTY.into()),
                    ..WireGit::default()
                }),
                ..WireSpace::default()
            }],
            panes: vec![WirePane {
                pane_id: DIRTY.into(),
                session_id: DIRTY.into(),
                window_id: DIRTY.into(),
                window_name: DIRTY.into(),
                pane_current_command: DIRTY.into(),
                ..WirePane::default()
            }],
            agents: vec![WireAgent {
                pane_id: DIRTY.into(),
                session_id: DIRTY.into(),
                session_name: DIRTY.into(),
                window_id: DIRTY.into(),
                window_name: DIRTY.into(),
                pane_current_command: DIRTY.into(),
                pane_title: format!("\u{1b}]52;c;QUJD\u{7}{DIRTY} writing"),
                kind: Some(DIRTY.into()),
                state: Some(DIRTY.into()),
                name: Some(DIRTY.into()),
                display: Some(DIRTY.into()),
                container: Some(DIRTY.into()),
                sandbox: Some(DIRTY.into()),
                tokens: BTreeMap::from([(DIRTY.to_string(), DIRTY.to_string())]),
                ..WireAgent::default()
            }],
            worktrees: vec![WireWorktree {
                workspace_id: DIRTY.into(),
                session_name: DIRTY.into(),
                path: DIRTY.into(),
                branch: DIRTY.into(),
                base: Some(DIRTY.into()),
                repo_root: DIRTY.into(),
                source_workspace_id: DIRTY.into(),
                ..WireWorktree::default()
            }],
            open_worktrees: vec![WireWorktree {
                session_name: DIRTY.into(),
                path: DIRTY.into(),
                branch: DIRTY.into(),
                ..WireWorktree::default()
            }],
        });
        let hello = Down::Hello(Hello {
            taw: DIRTY.into(),
            protocol: 1,
            host: DIRTY.into(),
            server: DIRTY.into(),
        });
        let left = Down::Left(Left {
            holder: DIRTY.into(),
            pane: DIRTY.into(),
            go: Some(DIRTY.into()),
        });
        for line in [state, hello, left] {
            let mut dirty = Vec::new();
            text_leaves(&serde_json::to_value(&line).unwrap(), &mut dirty);
            assert!(dirty.iter().any(|t| t.contains('\u{1b}')), "{line:?}");
            let cleaned = line.clone().cleaned();
            let mut found = Vec::new();
            text_leaves(&serde_json::to_value(&cleaned).unwrap(), &mut found);
            assert!(!found.is_empty());
            for text in found {
                assert!(
                    !text.chars().any(char::is_control),
                    "{text:?} in {cleaned:?}"
                );
            }
        }
        match (Down::State(State {
            agents: vec![WireAgent {
                pane_title: "\u{1b}[31mwriting\u{7}".into(),
                ..WireAgent::default()
            }],
            ..State::default()
        }))
        .cleaned()
        {
            Down::State(state) => assert_eq!(state.agents[0].pane_title, "writing"),
            other => panic!("{other:?}"),
        }
        assert_eq!(Down::Ping.cleaned(), Down::Ping);
    }

    #[test]
    fn a_known_type_with_a_field_of_the_wrong_kind_is_malformed() {
        assert!(matches!(
            parse_down(r#"{"type":"hello","protocol":"one"}"#),
            Err(LineError::Malformed(_))
        ));
    }

    #[test]
    fn the_wire_rows_carry_the_popups_rows_with_states_as_words() {
        let space = SpaceRow {
            session_id: "$3".into(),
            session_name: "garden".into(),
            session_path: "/g".into(),
            state: Some(AgentState::Blocked),
            git: Some(git::Info {
                branch: Some("main".into()),
                ahead: 2,
                behind: 1,
                modified: 3,
                untracked: 4,
            }),
            ..SpaceRow::default()
        };
        let wire = WireSpace::from(&space);
        assert_eq!(wire.state.as_deref(), Some("blocked"));
        assert_eq!(wire.session_path, "/g");
        assert_eq!(wire.git.unwrap().untracked, 4);
        let agent = AgentRow {
            pane_id: "%7".into(),
            state: Some(AgentState::Done),
            pane_pid: 99,
            ..AgentRow::default()
        };
        let wire = WireAgent::from(&agent);
        assert_eq!((wire.state.as_deref(), wire.pane_pid), (Some("done"), 99));
        let pane = PaneRow {
            pane_id: "%7".into(),
            agent: Some(0),
            pane_active: true,
            ..PaneRow::default()
        };
        assert!(WirePane::from(&pane).pane_active);
    }

    #[test]
    fn resolved_folders_round_trip_and_are_left_out_when_there_are_none() {
        let state = State {
            spaces: vec![WireSpace {
                session_path: "/var/x".into(),
                real_path: Some("/private/var/x".into()),
                ..WireSpace::default()
            }],
            worktrees: vec![WireWorktree {
                real_path: Some("/private/var/x--w".into()),
                real_repo_root: Some("/private/var/x".into()),
                ..WireWorktree::default()
            }],
            ..State::default()
        };
        let line = render(&Down::State(state.clone()));
        assert!(line.contains(r#""real_path":"/private/var/x""#), "{line}");
        assert_eq!(parse_down(line.trim()), Ok(Down::State(state)));

        let plain = render(&Down::State(State {
            spaces: vec![WireSpace::default()],
            worktrees: vec![WireWorktree::default()],
            ..State::default()
        }));
        assert!(!plain.contains("real_"), "{plain}");
    }

    #[test]
    fn a_state_without_resolved_folders_parses_to_none_and_an_unknown_field_is_ignored() {
        let old =
            r#"{"type":"state","spaces":[{"session_path":"/a"}],"worktrees":[{"path":"/b"}]}"#;
        match parse_down(old) {
            Ok(Down::State(state)) => {
                assert_eq!(state.spaces[0].real_path, None);
                assert_eq!(state.worktrees[0].real_path, None);
                assert_eq!(state.worktrees[0].real_repo_root, None);
            }
            other => panic!("{other:?}"),
        }
        let newer = r#"{"type":"state","spaces":[{"session_path":"/a","real_path":"/r","later":1}],"worktrees":[{"later":{"x":2}}]}"#;
        match parse_down(newer) {
            Ok(Down::State(state)) => assert_eq!(state.spaces[0].real_path.as_deref(), Some("/r")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_feed_sends_a_folder_resolved_only_when_it_differs() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(real.join("proj")).unwrap();
        // A project folder behind a link, as `/var` is behind `/private/var` on macOS.
        std::os::unix::fs::symlink(real.join("proj"), real.join("link")).unwrap();

        let behind = SpaceRow {
            session_path: real.join("link"),
            ..SpaceRow::default()
        };
        let direct = SpaceRow {
            session_path: real.join("proj"),
            ..SpaceRow::default()
        };
        let gone = SpaceRow {
            session_path: real.join("missing"),
            ..SpaceRow::default()
        };
        assert_eq!(
            WireSpace::from(&behind).real_path,
            Some(real.join("proj").display().to_string())
        );
        assert_eq!(WireSpace::from(&direct).real_path, None);
        assert_eq!(WireSpace::from(&gone).real_path, None);

        let mut record = crate::tui::fixtures::worktrees().remove(0);
        record.path = real.join("link");
        record.repo_root = real.join("proj");
        let wire = WireWorktree::from(&record);
        assert_eq!(
            wire.real_path,
            Some(real.join("proj").display().to_string())
        );
        assert_eq!(wire.real_repo_root, None);
    }

    #[test]
    fn open_worktrees_round_trip_and_a_state_without_them_parses_to_an_empty_list() {
        let state = State {
            worktrees: vec![WireWorktree {
                branch: "closed".into(),
                ..WireWorktree::default()
            }],
            open_worktrees: vec![WireWorktree {
                branch: "open".into(),
                path: "/w/open".into(),
                ..WireWorktree::default()
            }],
            ..State::default()
        };
        let line = render(&Down::State(state.clone()));
        assert!(line.contains(r#""open_worktrees":[{"#), "{line}");
        assert_eq!(parse_down(line.trim()), Ok(Down::State(state)));

        let none = render(&Down::State(State::default()));
        assert!(!none.contains("open_worktrees"), "{none}");
        match parse_down(r#"{"type":"state","spaces":[],"worktrees":[{"path":"/b"}]}"#) {
            Ok(Down::State(state)) => assert!(state.open_worktrees.is_empty()),
            other => panic!("{other:?}"),
        }
    }
}
