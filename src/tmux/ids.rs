//! Public/tmux id mapping and target resolution.
//!
//! Public ids are herdr-shaped so plugins written for herdr keep working; tmux's own ids
//! (`$3`, `@7`, `%31`) are accepted everywhere too.
//!
//! | tmux              | public |
//! |--------------------|--------|
//! | session `$3`       | `w3`   |
//! | window `@7` (in `$3`) | `w3:t7` |
//! | pane `%31` (in `$3`)  | `w3:p31` |

use super::{PaneInfo, Tmux};
use anyhow::Context;

/// `"$3"` -> `"w3"`.
pub fn workspace_id(session_id: &str) -> String {
    format!("w{}", session_id.trim_start_matches('$'))
}

/// `("$3", "@7")` -> `"w3:t7"`.
pub fn tab_id(session_id: &str, window_id: &str) -> String {
    format!(
        "{}:t{}",
        workspace_id(session_id),
        window_id.trim_start_matches('@')
    )
}

/// `("$3", "%31")` -> `"w3:p31"`.
pub fn pane_id(session_id: &str, pane_id: &str) -> String {
    format!(
        "{}:p{}",
        workspace_id(session_id),
        pane_id.trim_start_matches('%')
    )
}

/// A parsed id, holding the tmux-native id inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Workspace(String),
    Tab(String),
    Pane(String),
}

/// Parses a public id (`w3`, `w3:t7`, `w3:p31`) or a raw tmux id (`$3`, `@7`, `%31`).
///
/// Returns `None` for anything else, including tmux *targets* (`session:window.pane`,
/// session/window names, `=exact` forms) — those are resolved by asking tmux, via
/// `resolve_pane`/`resolve_window`/`resolve_session`, never parsed here.
pub fn parse(id: &str) -> Option<Target> {
    if let Some(rest) = id.strip_prefix('$') {
        return is_all_digits(rest).then(|| Target::Workspace(id.to_string()));
    }
    if let Some(rest) = id.strip_prefix('@') {
        return is_all_digits(rest).then(|| Target::Tab(id.to_string()));
    }
    if let Some(rest) = id.strip_prefix('%') {
        return is_all_digits(rest).then(|| Target::Pane(id.to_string()));
    }
    let rest = id.strip_prefix('w')?;
    if let Some((session_part, tail)) = rest.split_once(':') {
        if !is_all_digits(session_part) {
            return None;
        }
        if let Some(win) = tail.strip_prefix('t') {
            return is_all_digits(win).then(|| Target::Tab(format!("@{win}")));
        }
        if let Some(pane) = tail.strip_prefix('p') {
            return is_all_digits(pane).then(|| Target::Pane(format!("%{pane}")));
        }
        None
    } else {
        is_all_digits(rest).then(|| Target::Workspace(format!("${rest}")))
    }
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Resolves `s` to a tmux pane id (`%N`). Accepts, in order: a public pane id
/// (`w<s>:p<k>`), a raw tmux pane id (`%k`), a tmux target containing `:` or `.`
/// (resolved by asking tmux), or an agent's `@agent_name` (the pane that holds the name
/// before one that only still records it, see [`pane_named`]).
pub fn resolve_pane(t: &Tmux, s: &str) -> anyhow::Result<String> {
    match parse(s) {
        Some(Target::Pane(id)) => return Ok(id),
        Some(_) => anyhow::bail!("'{s}' is a workspace/tab id, not a pane id"),
        None => {}
    }
    if s.contains(':') || s.contains('.') {
        let id = t
            .display(s, "#{pane_id}")
            .with_context(|| format!("resolving tmux target '{s}' to a pane"))?;
        return if id.is_empty() {
            anyhow::bail!("no pane matches tmux target '{s}'")
        } else {
            Ok(id)
        };
    }
    let panes = t
        .list_panes()
        .with_context(|| format!("looking up agent name '{s}'"))?;
    if let Some(p) = pane_named(&panes, s, crate::watch::pane_shell_known_idle) {
        return Ok(p.pane_id.clone());
    }
    anyhow::bail!("could not resolve pane '{s}' (tried pane id, tmux target, agent name)")
}

/// The pane an agent name finds. At most one pane holds a name
/// ([`PaneInfo::holds_its_agent_name`], the rule `agent rename` and `agent start` keep), but the
/// record of an agent that is gone stays on its pane (a dead one, kept after its program ended, or
/// one whose shell is back at its prompt), so a live agent may have taken the same name since. With
/// one pane carrying the name, that pane, whatever runs in it; with more, the one that holds it,
/// else the first that is not dead, else the first, wherever each is listed. `shell_idle` is asked
/// only when more than one pane carries the name.
fn pane_named<'a>(
    panes: &'a [PaneInfo],
    name: &str,
    shell_idle: impl Fn(&PaneInfo) -> bool,
) -> Option<&'a PaneInfo> {
    let named: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| p.agent.name.as_deref() == Some(name))
        .collect();
    if named.len() > 1
        && let Some(&holder) = named.iter().find(|p| p.holds_its_agent_name(&shell_idle))
    {
        return Some(holder);
    }
    named
        .iter()
        .find(|p| !p.pane_dead)
        .or(named.first())
        .copied()
}

/// Resolves `s` to a tmux window id (`@N`). Accepts a public tab id (`w<s>:t<k>`), a raw
/// tmux window id (`@k`), or a tmux target (resolved by asking tmux).
pub fn resolve_window(t: &Tmux, s: &str) -> anyhow::Result<String> {
    match parse(s) {
        Some(Target::Tab(id)) => return Ok(id),
        Some(_) => anyhow::bail!("'{s}' is a workspace/pane id, not a window id"),
        None => {}
    }
    let id = t
        .display(s, "#{window_id}")
        .with_context(|| format!("resolving '{s}' to a window"))?;
    if id.is_empty() {
        anyhow::bail!("no window matches '{s}'");
    }
    Ok(id)
}

/// Resolves `s` to a tmux session id (`$N`). Accepts a public workspace id (`w<s>`), a raw
/// tmux session id (`$k`), or a session name / tmux target (resolved by asking tmux).
pub fn resolve_session(t: &Tmux, s: &str) -> anyhow::Result<String> {
    match parse(s) {
        Some(Target::Workspace(id)) => return Ok(id),
        Some(_) => anyhow::bail!("'{s}' is a tab/pane id, not a workspace id"),
        None => {}
    }
    let id = t
        .display(s, "#{session_id}")
        .with_context(|| format!("resolving '{s}' to a session"))?;
    if id.is_empty() {
        anyhow::bail!("no session matches '{s}'");
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_tmux_ids_to_public() {
        assert_eq!(workspace_id("$3"), "w3");
        assert_eq!(tab_id("$3", "@7"), "w3:t7");
        assert_eq!(pane_id("$3", "%31"), "w3:p31");
    }
    #[test]
    // Compares with `assert_eq!(_, true)`, which clippy otherwise flags as
    // `bool_assert_comparison`.
    #[allow(clippy::bool_assert_comparison)]
    fn parses_public_and_tmux_ids() {
        assert!(matches!(parse("w3"), Some(Target::Workspace(s)) if s == "$3"));
        assert!(matches!(parse("w3:t7"), Some(Target::Tab(s)) if s == "@7"));
        assert!(matches!(parse("w3:p31"), Some(Target::Pane(s)) if s == "%31"));
        assert!(matches!(parse("%31"), Some(Target::Pane(s)) if s == "%31"));
        assert!(matches!(parse("@7"), Some(Target::Tab(s)) if s == "@7"));
        assert!(matches!(parse("$3"), Some(Target::Workspace(s)) if s == "$3"));
        assert_eq!(parse("main:1.2").is_none(), true); // targets are resolved via tmux, not parsed
    }

    /// A pane whose agent record, of a `claude`, carries `name`.
    fn pane_named_as(id: &str, name: &str, dead: bool) -> PaneInfo {
        let mut p = crate::rail::tests_support::pane_with_state(None);
        p.pane_id = id.to_string();
        p.agent.kind = Some("claude".to_string());
        p.agent.name = Some(name.to_string());
        p.pane_dead = dead;
        p
    }

    fn found<'a>(
        panes: &'a [PaneInfo],
        name: &str,
        shell_idle: impl Fn(&PaneInfo) -> bool,
    ) -> Option<&'a str> {
        pane_named(panes, name, shell_idle).map(|p| p.pane_id.as_str())
    }

    /// A dead pane listed first still carries `rev`, and the live agent that took the name
    /// since is listed after it: the name finds the live one.
    #[test]
    fn an_agent_name_finds_a_live_pane_before_a_dead_one() {
        let panes = [
            pane_named_as("%0", "rev", true),
            pane_named_as("%1", "fix", false),
            pane_named_as("%2", "rev", false),
        ];
        let nothing_idle = |_: &PaneInfo| false;
        assert_eq!(found(&panes, "rev", nothing_idle), Some("%2"));
        assert_eq!(found(&panes, "fix", nothing_idle), Some("%1"));
        assert_eq!(found(&panes, "gone", nothing_idle), None);
    }

    /// An agent that exited with no watcher left `rev` on the pane listed first, whose shell is
    /// back at its prompt, and the agent that took the name since is listed after it: the name
    /// finds the one that holds it.
    #[test]
    fn an_agent_name_finds_the_pane_that_holds_it_before_one_whose_shell_is_back() {
        let panes = [
            pane_named_as("%0", "rev", false),
            pane_named_as("%1", "rev", false),
        ];
        assert_eq!(
            found(&panes, "rev", |p: &PaneInfo| p.pane_id == "%0"),
            Some("%1")
        );
        // When no pane holds it, the first that is not dead.
        let panes = [
            pane_named_as("%0", "rev", true),
            pane_named_as("%1", "rev", false),
        ];
        assert_eq!(found(&panes, "rev", |_: &PaneInfo| true), Some("%1"));
    }

    /// With one pane carrying the name, that pane, dead or not, as before, and the process table
    /// is not asked about it.
    #[test]
    fn an_agent_name_one_pane_carries_finds_it_without_asking_the_process_table() {
        let asked = |_: &PaneInfo| -> bool { panic!("asked whether a shell is idle") };
        let panes = [
            pane_named_as("%0", "fix", false),
            pane_named_as("%1", "rev", true),
        ];
        assert_eq!(found(&panes, "rev", asked), Some("%1"));
        assert_eq!(found(&panes, "fix", asked), Some("%0"));
    }
}
