//! `taw pane …` — herdr's pane verbs over tmux panes.
//!
//! This module holds the clap tree and the dispatch; the verbs themselves are grouped by
//! what they do: [`inspect`] for the read-only queries, [`io`] for text in and out,
//! [`control`] for the ones that change layout, [`clear`] for the one that clears a pane's
//! screen and history, [`layout`] for the `#{window_layout}` parser, and [`report`] for the
//! `@agent_*` writers.

pub mod clear;
pub mod control;
pub mod inspect;
pub mod io;
pub mod layout;
pub mod report;

use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::common::{self, Direction, Format, Source};
use super::{Ctx, Sub};

/// herdr's `pane split --direction` / `pane move --split`: the two axes tmux can split on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SplitDirection {
    Right,
    Down,
}

impl SplitDirection {
    /// `split-window`'s axis flag: `-h` puts the new pane to the right, `-v` below.
    fn flag(self) -> &'static str {
        match self {
            SplitDirection::Right => "-h",
            SplitDirection::Down => "-v",
        }
    }
}

/// herdr's `--right-click <TARGET>`: which surface a right-click in the pane goes to.
/// Accepted for parity; tmux's mouse bindings are global key tables, not per-pane
/// routing, so there is nothing to set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RightClick {
    Herdr,
    Pane,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List panes
    List {
        #[arg(long, value_name = "WORKSPACE_ID")]
        workspace: Option<String>,
    },
    /// Show the current pane
    Current {
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Show a pane
    Get { pane_id: String },
    /// Show pane layout information
    Layout {
        /// A pane or a tab; the layout of the tab holding it is reported.
        target: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Show pane process information
    ProcessInfo {
        target: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Find a pane neighbor
    Neighbor {
        target: Option<String>,
        #[arg(long, value_enum)]
        direction: Direction,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Show pane edge information
    Edges {
        target: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Focus a neighboring pane
    Focus {
        target: Option<String>,
        #[arg(long, value_enum)]
        direction: Direction,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Resize a pane split
    Resize {
        target: Option<String>,
        #[arg(long, value_enum)]
        direction: Direction,
        /// Cells to grow by (fractional values are rounded); default 5.
        #[arg(long, value_name = "FLOAT")]
        amount: Option<f64>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Toggle or set pane zoom
    Zoom {
        pane_id: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
        #[arg(long)]
        toggle: bool,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
    },
    /// Read pane terminal output
    Read {
        pane_id: String,
        #[arg(long, value_enum)]
        source: Option<Source>,
        #[arg(long, value_name = "N")]
        lines: Option<u32>,
        #[arg(long, value_enum)]
        format: Option<Format>,
        #[arg(long)]
        ansi: bool,
        /// Accepted for herdr parity; `--ansi` is the flag that keeps escapes.
        #[arg(long)]
        raw: bool,
    },
    /// Rename a pane
    Rename {
        pane_id: String,
        #[arg(value_name = "LABEL")]
        label: Vec<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Set pane input routing
    Input {
        pane_id: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
        #[arg(long = "right-click", value_enum, value_name = "TARGET")]
        right_click: Option<RightClick>,
    },
    /// Split a pane
    Split {
        pane_id: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
        #[arg(long, value_enum)]
        direction: Option<SplitDirection>,
        #[arg(long, value_name = "FLOAT")]
        ratio: Option<f64>,
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        /// Set an environment variable for the launched process
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long = "right-click", value_enum, value_name = "TARGET")]
        right_click: Option<RightClick>,
        #[arg(long)]
        focus: bool,
        #[arg(long = "no-focus")]
        no_focus: bool,
    },
    /// Swap panes
    Swap {
        #[arg(long, value_enum)]
        direction: Option<Direction>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
        #[arg(long = "source-pane", value_name = "ID")]
        source_pane: Option<String>,
        #[arg(long = "target-pane", value_name = "ID")]
        target_pane: Option<String>,
    },
    /// Move a pane
    Move {
        pane_id: String,
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        #[arg(long, value_enum, value_name = "DIRECTION")]
        split: Option<SplitDirection>,
        #[arg(long = "target-pane", value_name = "ID")]
        target_pane: Option<String>,
        #[arg(long, value_name = "FLOAT")]
        ratio: Option<f64>,
        #[arg(long = "new-tab")]
        new_tab: bool,
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
        #[arg(long = "new-workspace")]
        new_workspace: bool,
        /// Names what is created: the new tab with --new-tab, the new workspace with
        /// --new-workspace
        #[arg(long, value_name = "TEXT")]
        label: Option<String>,
        /// With --new-workspace: names that workspace's first tab
        #[arg(long = "tab-label", value_name = "TEXT")]
        tab_label: Option<String>,
        #[arg(long)]
        focus: bool,
        #[arg(long = "no-focus")]
        no_focus: bool,
    },
    /// Close a pane
    Close { pane_id: String },
    /// Clear a pane's history, and its screen when a shell is at its prompt
    Clear {
        pane_id: Option<String>,
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
        #[arg(long)]
        current: bool,
    },
    /// Send literal text to a pane
    SendText { pane_id: String, text: String },
    /// Send key presses to a pane
    SendKeys {
        pane_id: String,
        #[arg(required = true, value_name = "KEY")]
        keys: Vec<String>,
    },
    /// Wait for matching pane output
    WaitOutput {
        pane_id: String,
        /// Match a literal substring
        #[arg(long = "match", value_name = "TEXT")]
        match_text: Option<String>,
        /// Match a Rust regular expression. `--pattern` is accepted as an alias.
        #[arg(long = "regex", alias = "pattern", value_name = "PATTERN")]
        regex: Option<String>,
        #[arg(long, value_enum)]
        source: Option<Source>,
        /// Restrict the searched snapshot to N lines
        #[arg(long, value_name = "N")]
        lines: Option<u32>,
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
        /// Keep ANSI escape sequences while matching
        #[arg(long)]
        raw: bool,
    },
    /// Run a command in a pane
    Run {
        pane_id: String,
        #[arg(required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Report pane agent lifecycle state
    ReportAgent {
        pane_id: String,
        #[arg(long, value_name = "ID")]
        source: String,
        #[arg(long, value_name = "LABEL")]
        agent: String,
        #[arg(long, value_enum, value_name = "STATUS")]
        state: report::ReportState,
        /// Accepted for herdr parity; the message is not stored (tmux options hold state,
        /// not a log; use `--token` via `report-metadata` for durable text).
        #[arg(long, value_name = "TEXT")]
        message: Option<String>,
        /// The report's number in its source's order: a report whose number is not higher than
        /// the last one its source sent is ignored, and so is one with none from a source that
        /// has sent one. A timestamp works well.
        #[arg(long, value_name = "N")]
        seq: Option<u64>,
        #[arg(long = "agent-session-id", value_name = "ID")]
        agent_session_id: Option<String>,
        #[arg(long = "agent-session-path", value_name = "PATH")]
        agent_session_path: Option<String>,
        /// The command that resumes this session when `taw session restore` restores the pane;
        /// it starts with a plain command name
        #[arg(last = true, value_name = "RESUME_ARG")]
        resume: Option<Vec<String>>,
    },
    /// Report pane agent session identity
    ReportAgentSession {
        pane_id: String,
        #[arg(long, value_name = "ID")]
        source: String,
        #[arg(long, value_name = "LABEL")]
        agent: String,
        /// The report's number in its source's order, as for `report-agent`.
        #[arg(long, value_name = "N")]
        seq: Option<u64>,
        #[arg(long = "agent-session-id", value_name = "ID")]
        agent_session_id: Option<String>,
        #[arg(long = "agent-session-path", value_name = "PATH")]
        agent_session_path: Option<String>,
        #[arg(long = "session-start-source", value_name = "SOURCE")]
        session_start_source: Option<String>,
        /// The command that resumes this session when `taw session restore` restores the pane,
        /// as for `report-agent`
        #[arg(last = true, value_name = "RESUME_ARG")]
        resume: Option<Vec<String>>,
    },
    /// Release pane agent lifecycle authority
    ReleaseAgent {
        pane_id: String,
        #[arg(long, value_name = "ID")]
        source: String,
        #[arg(long, value_name = "LABEL")]
        agent: String,
        /// The release's number in its source's order, as for `report-agent`.
        #[arg(long, value_name = "N")]
        seq: Option<u64>,
    },
    /// Report display-only pane metadata
    ReportMetadata {
        pane_id: String,
        #[arg(long, value_name = "ID")]
        source: String,
        #[arg(long, value_name = "LABEL")]
        agent: Option<String>,
        /// Accepted for herdr parity; tmux options have no per-source layering.
        #[arg(long = "applies-to-source", value_name = "ID")]
        applies_to_source: Option<String>,
        #[arg(long, value_name = "TEXT")]
        title: Option<String>,
        #[arg(long = "clear-title")]
        clear_title: bool,
        #[arg(long = "display-agent", value_name = "TEXT")]
        display_agent: Option<String>,
        #[arg(long = "clear-display-agent")]
        clear_display_agent: bool,
        #[arg(long = "state-label", value_name = "STATUS=TEXT")]
        state_label: Vec<String>,
        #[arg(long = "clear-state-labels")]
        clear_state_labels: bool,
        #[arg(long, value_name = "NAME=VALUE")]
        token: Vec<String>,
        #[arg(long = "clear-token", value_name = "NAME")]
        clear_token: Vec<String>,
        #[arg(long, value_name = "N")]
        seq: Option<u64>,
        /// Accepted for herdr parity; tmux options do not expire.
        #[arg(long = "ttl-ms", value_name = "N")]
        ttl_ms: Option<u64>,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List { workspace } => ("list", inspect::list(ctx, workspace.as_deref())),
        Cmd::Current { pane, .. } => ("current", inspect::get_one(ctx, None, pane.as_deref())),
        Cmd::Get { pane_id } => ("get", inspect::get_one(ctx, Some(&pane_id), None)),
        Cmd::Layout { target, pane, .. } => (
            "layout",
            inspect::layout_of(ctx, target.as_deref(), pane.as_deref()),
        ),
        Cmd::ProcessInfo { target, pane, .. } => (
            "process-info",
            inspect::process_info(ctx, target.as_deref(), pane.as_deref()),
        ),
        Cmd::Neighbor {
            target,
            direction,
            pane,
            ..
        } => (
            "neighbor",
            inspect::neighbor(ctx, target.as_deref(), pane.as_deref(), direction),
        ),
        Cmd::Edges { target, pane, .. } => (
            "edges",
            inspect::edges(ctx, target.as_deref(), pane.as_deref()),
        ),
        Cmd::Focus {
            target,
            direction,
            pane,
            ..
        } => (
            "focus",
            control::focus_neighbor(ctx, target.as_deref(), pane.as_deref(), direction),
        ),
        Cmd::Resize {
            target,
            direction,
            amount,
            pane,
            ..
        } => (
            "resize",
            control::resize(ctx, target.as_deref(), pane.as_deref(), direction, amount),
        ),
        Cmd::Zoom {
            pane_id,
            pane,
            toggle,
            on,
            off,
            ..
        } => (
            "zoom",
            control::zoom(ctx, pane_id.as_deref(), pane.as_deref(), toggle, on, off),
        ),
        Cmd::Read {
            pane_id,
            source,
            lines,
            format,
            ansi,
            raw,
        } => (
            "read",
            io::read(
                ctx,
                &pane_id,
                source,
                lines,
                ansi || raw || format == Some(Format::Ansi),
            ),
        ),
        Cmd::Rename {
            pane_id,
            label,
            clear,
        } => ("rename", control::rename(ctx, &pane_id, &label, clear)),
        Cmd::Input { .. } => ("input", Ok(unsupported_noop())),
        Cmd::Split {
            pane_id,
            pane,
            direction,
            ratio,
            cwd,
            env,
            focus,
            no_focus,
            ..
        } => (
            "split",
            control::split(
                ctx,
                pane_id.as_deref(),
                pane.as_deref(),
                direction.unwrap_or(SplitDirection::Right),
                ratio,
                cwd.as_deref(),
                &env,
                common::wants_focus(focus, no_focus),
            ),
        ),
        Cmd::Swap {
            direction,
            pane,
            source_pane,
            target_pane,
            ..
        } => (
            "swap",
            control::swap(
                ctx,
                source_pane.as_deref().or(pane.as_deref()),
                target_pane.as_deref(),
                direction,
            ),
        ),
        Cmd::Move {
            pane_id,
            tab,
            split: split_dir,
            target_pane,
            ratio,
            new_tab,
            workspace,
            new_workspace,
            label,
            tab_label,
            focus,
            no_focus,
        } => (
            "move",
            control::move_pane(
                ctx,
                control::MoveArgs {
                    pane_id: &pane_id,
                    tab: tab.as_deref(),
                    split: split_dir,
                    target_pane: target_pane.as_deref(),
                    ratio,
                    new_tab,
                    workspace: workspace.as_deref(),
                    new_workspace,
                    label: label.as_deref(),
                    tab_label: tab_label.as_deref(),
                    focus: common::wants_focus(focus, no_focus),
                },
            ),
        ),
        Cmd::Close { pane_id } => ("close", control::close(ctx, &pane_id)),
        Cmd::Clear { pane_id, pane, .. } => (
            "clear",
            clear::clear(ctx, pane_id.as_deref(), pane.as_deref()),
        ),
        Cmd::SendText { pane_id, text } => ("send-text", io::send_text(ctx, &pane_id, &text)),
        Cmd::SendKeys { pane_id, keys } => ("send-keys", io::send_keys(ctx, &pane_id, &keys)),
        Cmd::WaitOutput {
            pane_id,
            match_text,
            regex,
            source,
            lines,
            timeout,
            raw,
        } => (
            "wait-output",
            io::wait_output(
                ctx,
                &pane_id,
                match_text.as_deref(),
                regex.as_deref(),
                source,
                lines,
                timeout,
                raw,
            ),
        ),
        Cmd::Run { pane_id, command } => ("run", io::run_command(ctx, &pane_id, &command)),
        Cmd::ReportAgent {
            pane_id,
            source,
            agent,
            state,
            seq,
            agent_session_id,
            agent_session_path,
            resume,
            ..
        } => (
            report::REPORT_AGENT,
            report::report_agent(
                ctx,
                &pane_id,
                &source,
                &agent,
                state,
                seq,
                agent_session_id.as_deref(),
                agent_session_path.as_deref(),
                resume.as_deref(),
            ),
        ),
        Cmd::ReportAgentSession {
            pane_id,
            source,
            agent,
            seq,
            agent_session_id,
            agent_session_path,
            session_start_source,
            resume,
        } => (
            report::REPORT_AGENT_SESSION,
            report::report_agent_session(
                ctx,
                &pane_id,
                &source,
                &agent,
                seq,
                agent_session_id.as_deref(),
                agent_session_path.as_deref(),
                session_start_source.as_deref(),
                resume.as_deref(),
            ),
        ),
        Cmd::ReleaseAgent {
            pane_id,
            source,
            agent,
            seq,
        } => (
            report::RELEASE_AGENT,
            report::release_agent(ctx, &pane_id, &source, &agent, seq),
        ),
        Cmd::ReportMetadata {
            pane_id,
            title,
            clear_title,
            display_agent,
            clear_display_agent,
            state_label,
            clear_state_labels,
            token,
            clear_token,
            ..
        } => (
            report::REPORT_METADATA,
            report::report_metadata(
                ctx,
                &pane_id,
                report::Metadata {
                    title: title.as_deref(),
                    clear_title,
                    display_agent: display_agent.as_deref(),
                    clear_display_agent,
                    state_labels: &state_label,
                    clear_state_labels,
                    tokens: &token,
                    clear_tokens: &clear_token,
                },
            ),
        ),
    }
}

/// `pane input` and any other verb accepted purely for parity: reports success with a
/// note, because refusing would break a herdr plugin that calls it unconditionally.
fn unsupported_noop() -> Value {
    json!({
        "note": "accepted for herdr parity and ignored: tmux has no per-pane input routing \
                 (mouse bindings are server-wide key tables)"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_direction_maps_to_the_tmux_axis_flag() {
        assert_eq!(SplitDirection::Right.flag(), "-h");
        assert_eq!(SplitDirection::Down.flag(), "-v");
    }

    #[test]
    fn direction_maps_to_the_tmux_resize_flag() {
        assert_eq!(Direction::Left.flag(), "-L");
        assert_eq!(Direction::Right.flag(), "-R");
        assert_eq!(Direction::Up.flag(), "-U");
        assert_eq!(Direction::Down.flag(), "-D");
    }
}
