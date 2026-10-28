//! `taw pane read | send-text | send-keys | run | wait-output` — moving text into a pane
//! and getting it back out again.

use serde_json::{Value, json};

use crate::cli::Ctx;
use crate::cli::common::{self, Source};
use crate::cli::envelope::CliError;
use crate::tmux::ids;

pub(super) fn read(
    ctx: &Ctx,
    pane_id: &str,
    source: Option<Source>,
    lines: Option<u32>,
    ansi: bool,
) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    let text = common::read_pane(
        &ctx.tmux,
        &resolved,
        source.unwrap_or_default(),
        lines,
        ansi,
    )?;
    let pane = common::pane_info(&ctx.tmux, &resolved)?;
    Ok(json!({
        "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
        "text": text,
    }))
}

pub(super) fn send_text(ctx: &Ctx, pane_id: &str, text: &str) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    common::send_text(&ctx.tmux, &resolved, text)?;
    Ok(common::ok())
}

pub(super) fn send_keys(ctx: &Ctx, pane_id: &str, keys: &[String]) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    let sent = common::send_keys(&ctx.tmux, &resolved, keys)?;
    Ok(json!({ "keys": sent }))
}

/// `pane run`: send the command text and Enter in one call, herdr's atomic "run this".
pub(super) fn run_command(ctx: &Ctx, pane_id: &str, command: &[String]) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    let text = command.join(" ");
    common::send_text(&ctx.tmux, &resolved, &text)?;
    common::send_enter(&ctx.tmux, &resolved)?;
    Ok(common::ok())
}

/// Polls a pane's snapshot until it matches. The snapshot is searched *before* any
/// sleeping, so output that already exists matches (herdr: "searches the selected
/// snapshot immediately, including existing output, then polls").
#[allow(clippy::too_many_arguments)]
pub(super) fn wait_output(
    ctx: &Ctx,
    pane_id: &str,
    match_text: Option<&str>,
    regex: Option<&str>,
    source: Option<Source>,
    lines: Option<u32>,
    timeout: Option<u64>,
    raw: bool,
) -> anyhow::Result<Value> {
    let resolved = common::resolve_pane(&ctx.tmux, pane_id)?;
    let source = source.unwrap_or_default();
    let matcher: Box<dyn Fn(&str) -> bool> = match (match_text, regex) {
        (Some(_), Some(_)) => {
            return Err(CliError::invalid_argument(
                "--match and --regex are mutually exclusive",
            ));
        }
        (Some(text), None) => {
            let needle = text.to_string();
            Box::new(move |haystack: &str| haystack.contains(&needle))
        }
        (None, Some(pattern)) => {
            let re = regex::Regex::new(pattern).map_err(|e| {
                CliError::invalid_argument(format!("invalid --regex '{pattern}': {e}"))
            })?;
            Box::new(move |haystack: &str| re.is_match(haystack))
        }
        (None, None) => {
            return Err(CliError::invalid_argument(
                "pane wait-output needs --match <TEXT> or --regex <PATTERN>",
            ));
        }
    };

    let described = match_text
        .map(|t| format!("literal '{t}'"))
        .unwrap_or_else(|| format!("pattern '{}'", regex.unwrap_or_default()));
    let what = format!("output matching {described} in pane '{pane_id}'");
    let text = common::poll_until(timeout, &what, || {
        // Match on committed output only — the rows above the cursor. A command typed at
        // the prompt but not yet run sits on the cursor's own line, so matching the whole
        // screen made `wait-output --match` fire on the pattern in the input the caller had
        // only just typed. Real output lands above the cursor, so a genuine match still
        // settles here.
        let text = common::read_pane_above_cursor(&ctx.tmux, &resolved, source, lines, raw)?;
        Ok(matcher(&text).then_some(text))
    })?;

    // The canonical `wN:pN`, not whatever spelling the caller passed (a raw `%N`, a tmux
    // target, an agent name) — every other verb replies with the public id.
    let pane = common::pane_info(&ctx.tmux, &resolved)?;
    Ok(json!({
        "pane_id": ids::pane_id(&pane.session_id, &pane.pane_id),
        "matched": true,
        // The snapshot that matched, so a caller need not read again and risk a redraw
        // having scrolled the match away.
        "text": text,
    }))
}
