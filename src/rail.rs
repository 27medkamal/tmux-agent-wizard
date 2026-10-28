//! Status rail: `#{@agent_wizard_status}`, the opt-in status-line token
//! (it is a compact, user-placed summary of
//! `@agent_state` across every pane on the server — tmux remains the only database;
//! this module only reads `list_panes` and writes the one global option).

use crate::config::{Config, StatusIndicators};
use crate::store::AgentState;
use crate::tmux::{PaneInfo, Tmux};

/// Tally of panes per agent state, across the whole server (`list_panes` is
/// server-wide, not per-session — the rail is a global summary).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
    pub unknown: usize,
}

/// Tallies `panes` by `agent.state`; panes with no agent (`state: None`) are not
/// counted anywhere.
pub fn counts(panes: &[PaneInfo]) -> Counts {
    let mut c = Counts::default();
    for pane in panes {
        match pane.agent.state {
            Some(AgentState::Blocked) => c.blocked += 1,
            Some(AgentState::Working) => c.working += 1,
            Some(AgentState::Done) => c.done += 1,
            Some(AgentState::Idle) => c.idle += 1,
            Some(AgentState::Unknown) => c.unknown += 1,
            None => {}
        }
    }
    c
}

/// One rendered rail entry: the glyph and colour for a state, in the fixed
/// blocked -> working -> done -> idle -> unknown order.
struct Entry {
    count: usize,
    colour: &'static str,
    dot: &'static str,
    symbol: &'static str,
}

const ENTRIES: [(&str, &str, &str); 5] = [
    // (colour, dot glyph, symbol glyph), one row per state, in rail order.
    ("yellow", "\u{25cf}", "\u{23f8}"),      // blocked: ● / ⏸
    ("blue", "\u{25cf}", "\u{23f5}"),        // working: ● / ⏵
    ("green", "\u{25cf}", "\u{2713}"),       // done:    ● / ✓
    ("brightblack", "\u{25cf}", "\u{25cb}"), // idle:    ● / ○
    ("magenta", "\u{25cf}", "?"),            // unknown: ● / ?
];

/// Renders `c` as the rail text: one `#[fg=<colour>]<glyph><n>` per non-zero state,
/// space-separated, in blocked -> working -> done -> idle -> unknown order, ending in
/// `#[default]`. `"dots"` uses `●` for every state; `"symbols"` uses a distinct glyph
/// per state (`⏸ ⏵ ✓ ○ ?`). `""` when every count is zero.
pub fn render(c: &Counts, style: StatusIndicators) -> String {
    let counts = [c.blocked, c.working, c.done, c.idle, c.unknown];
    let entries: Vec<Entry> = counts
        .into_iter()
        .zip(ENTRIES)
        .map(|(count, (colour, dot, symbol))| Entry {
            count,
            colour,
            dot,
            symbol,
        })
        .collect();

    let parts: Vec<String> = entries
        .iter()
        .filter(|e| e.count > 0)
        .map(|e| {
            let glyph = match style {
                StatusIndicators::Dots => e.dot,
                StatusIndicators::Symbols => e.symbol,
            };
            format!("#[fg={}]{glyph}{}", e.colour, e.count)
        })
        .collect();

    if parts.is_empty() {
        return String::new();
    }
    format!("{}#[default]", parts.join(" "))
}

/// Recomputes the rail from a fresh `list_panes` and writes it to
/// `@agent_wizard_status` (the global option users add to their own status line).
/// Called by the watcher after every transition and by `taw status refresh`. Returns
/// the text that was written.
pub fn refresh(t: &Tmux, cfg: &Config) -> anyhow::Result<String> {
    let panes = t.list_panes()?;
    let text = render(&counts(&panes), cfg.ui.status_indicators);
    t.set_global_option("@agent_wizard_status", &text)?;
    Ok(text)
}

/// Test fixtures shared with other modules' tests (the status line's bar counts the same
/// panes the rail does), so both build one realistic [`PaneInfo`].
#[cfg(test)]
pub(crate) mod tests_support {
    use crate::store::{AgentRecord, AgentState};
    use crate::tmux::PaneInfo;

    /// A pane in session `$1`, with an agent in `state` — or no agent at all for `None`,
    /// a plain shell — and every other field a plausible value.
    pub(crate) fn pane_with_state(state: Option<AgentState>) -> PaneInfo {
        PaneInfo {
            pane_id: "%1".to_string(),
            session_id: "$1".to_string(),
            session_name: "s".to_string(),
            session_path: "/tmp".to_string(),
            session_attached: 0,
            window_id: "@1".to_string(),
            window_index: 0,
            window_name: "w".to_string(),
            window_active: false,
            window_layout: String::new(),
            pane_index: 0,
            pane_active: false,
            pane_pid: 0,
            pane_current_command: String::new(),
            pane_current_path: String::new(),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord {
                state,
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::pane_with_state;
    use super::*;

    #[test]
    fn counts_tallies_panes_by_state_and_ignores_agent_less_panes() {
        let panes = [
            pane_with_state(Some(AgentState::Blocked)),
            pane_with_state(Some(AgentState::Working)),
            pane_with_state(Some(AgentState::Working)),
            pane_with_state(Some(AgentState::Working)),
            pane_with_state(Some(AgentState::Done)),
            pane_with_state(Some(AgentState::Done)),
            pane_with_state(None),
        ];
        let c = counts(&panes);
        assert_eq!(
            c,
            Counts {
                blocked: 1,
                working: 3,
                done: 2,
                idle: 0,
                unknown: 0,
            }
        );
    }

    #[test]
    fn counts_of_no_panes_is_all_zero() {
        assert_eq!(counts(&[]), Counts::default());
    }

    #[test]
    fn render_is_empty_when_all_counts_are_zero() {
        assert_eq!(render(&Counts::default(), StatusIndicators::Dots), "");
        assert_eq!(render(&Counts::default(), StatusIndicators::Symbols), "");
    }

    #[test]
    fn render_dots_style_uses_a_coloured_dot_per_state() {
        let c = Counts {
            blocked: 1,
            working: 3,
            done: 0,
            idle: 0,
            unknown: 0,
        };
        assert_eq!(
            render(&c, StatusIndicators::Dots),
            "#[fg=yellow]\u{25cf}1 #[fg=blue]\u{25cf}3#[default]"
        );
    }

    #[test]
    fn render_symbols_style_uses_a_distinct_glyph_per_state() {
        let c = Counts {
            blocked: 1,
            working: 3,
            done: 2,
            idle: 0,
            unknown: 0,
        };
        assert_eq!(
            render(&c, StatusIndicators::Symbols),
            "#[fg=yellow]\u{23f8}1 #[fg=blue]\u{23f5}3 #[fg=green]\u{2713}2#[default]"
        );
    }

    #[test]
    fn render_orders_blocked_working_done_idle_unknown_and_omits_zero_counts() {
        // Only idle, unknown and blocked are non-zero; the field declaration order
        // (blocked, working, done, idle, unknown) is *not* blocked/idle/unknown, so
        // this proves render follows the documented rail order, not struct order.
        let c = Counts {
            blocked: 1,
            working: 0,
            done: 0,
            idle: 3,
            unknown: 2,
        };
        assert_eq!(
            render(&c, StatusIndicators::Symbols),
            "#[fg=yellow]\u{23f8}1 #[fg=brightblack]\u{25cb}3 #[fg=magenta]?2#[default]"
        );
    }

    #[test]
    fn render_symbols_glyphs_match_the_spec_table() {
        let one_of_each = Counts {
            blocked: 1,
            working: 1,
            done: 1,
            idle: 1,
            unknown: 1,
        };
        let text = render(&one_of_each, StatusIndicators::Symbols);
        assert!(text.contains("\u{23f8}1"), "blocked glyph ⏸: {text}");
        assert!(text.contains("\u{23f5}1"), "working glyph ⏵: {text}");
        assert!(text.contains('\u{2713}'), "done glyph ✓: {text}");
        assert!(text.contains('\u{25cb}'), "idle glyph ○: {text}");
        assert!(text.contains('?'), "unknown glyph ?: {text}");
    }
}
