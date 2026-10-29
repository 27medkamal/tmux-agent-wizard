//! `taw ui open`: computes popup geometry that leaves the status line visible, puts taw's
//! bar on that line, runs `display-popup`, and puts the line back once the popup closes.
//! A full-size popup — one that covers everything but the status line's own rows — drops
//! its frame and title on tmux >= 3.3 ([`Geometry::is_full`], [`popup_args`]): the bar
//! already names it. A capped popup, or an older tmux, keeps the frame and title exactly
//! as today.
//!
//! Verified on tmux 3.6b: with `status-position top` and a one-row status line,
//! `display-popup -x 0 -y <client_height> -w <client_width> -h <client_height - 1>`
//! leaves row 0 — the status line — uncovered, and `display-message` still draws there
//! while the popup is up. `-y` is tmux's *bottom* edge for a popup, not its top: a popup
//! `client_height - 1` rows tall whose bottom edge sits at the last row starts one row
//! down, at row 1, so row 0 is never touched. A multi-row status line (`status`
//! `2`..`5`) reserves that many rows the same way — see [`geometry`].

use std::path::Path;

use anyhow::Context as _;
use serde_json::{Value, json};

use super::Ctx;
use super::envelope::CliError;
use super::status_bar;
use crate::machine::{files, wire};
use crate::tmux::CommandFailed;
use crate::tui::theme;

/// Default popup title (`@agent-wizard-popup-title`), matching the plugin script's
/// existing `display-popup -T` default.
const DEFAULT_TITLE: &str = " taw ";

/// The popup's own style on tmux >= 3.3 (`display-popup -s`): the terminal's default colours,
/// tmux's `terminal` colour (`default` would be `popup-style`'s own). What taw leaves at the
/// default colours (a pane's own text in the preview, and the whole popup in the `terminal` theme)
/// is then drawn at the terminal's, as in the pane itself, and not at `popup-style`'s, which tmux
/// 3.8 makes a dark grey. The frame keeps `popup-border-style`.
const POPUP_STYLE: &str = "fg=terminal,bg=terminal";

/// Default percentage cap on each axis (`@agent-wizard-width`/`-height`) when the option
/// is unset or does not parse as a plain number.
const DEFAULT_PERCENT: u16 = 100;

/// Whether, and where, the status line takes up a row — the only thing popup geometry
/// needs to know about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusPosition {
    Top,
    Bottom,
    Off,
}

/// A `display-popup -x -y -w -h` geometry. `y` is the popup's *bottom* edge, in tmux's
/// own `-y` sense — see [`geometry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Sizes and places the popup so the status line's rows, if the client has any, always
/// stay visible and drawable. `rows` is the status line's row count (`0` with it off, `1`
/// when on, `2`..`5` for a multi-row line — [`status_bar::status_rows`]).
///
/// * `Top`: the status line starts at row 0, so the popup must start no earlier than row
///   `rows` — `h = min(client_h - rows, cap_h_pct% of client_h)`, `y = client_h` (bottom
///   edge at the last row, however tall the popup ends up).
/// * `Bottom`: the status line is the last `rows` rows, so the popup's bottom edge has to
///   land `rows` rows short of it — same `h` as `Top`, `y = client_h - rows`.
/// * `Off`: no row to protect (`rows` is `0`) — `h = min(client_h, cap_h_pct% of
///   client_h)`, `y = client_h`.
///
/// Every case centers the popup horizontally: `w = min(client_w, cap_w_pct% of
/// client_w)`, `x = (client_w - w) / 2`.
pub fn geometry(
    client_w: u16,
    client_h: u16,
    status: StatusPosition,
    rows: usize,
    cap_w_pct: u16,
    cap_h_pct: u16,
) -> Geometry {
    let w = client_w.min(percent_of(client_w, cap_w_pct));
    let x = (client_w - w) / 2;
    let rows = rows as u16;
    let (h, y) = match status {
        StatusPosition::Top => (
            (client_h - rows).min(percent_of(client_h, cap_h_pct)),
            client_h,
        ),
        StatusPosition::Bottom => (
            (client_h - rows).min(percent_of(client_h, cap_h_pct)),
            client_h - rows,
        ),
        StatusPosition::Off => (client_h.min(percent_of(client_h, cap_h_pct)), client_h),
    };
    Geometry { x, y, w, h }
}

impl Geometry {
    /// Whether the popup covers the whole client: full width, and full height minus the
    /// status line's rows, if there is one. Such a popup needs no frame — the bar
    /// already names it.
    pub fn is_full(
        &self,
        client_w: u16,
        client_h: u16,
        status: StatusPosition,
        rows: usize,
    ) -> bool {
        let full_h = match status {
            StatusPosition::Off => client_h,
            _ => client_h.saturating_sub(rows as u16),
        };
        self.w == client_w && self.h == full_h
    }
}

/// `percent`% of `total`, floored, computed in `u32` so `total * percent` cannot
/// overflow `u16` before the division.
fn percent_of(total: u16, percent: u16) -> u16 {
    (u32::from(total) * u32::from(percent) / 100) as u16
}

/// Reads `status`/`status-position` the way [`geometry`] needs them: `status` anything
/// but `"off"` counts as on (tmux accepts `on`/`off`/`2`..`5` — a numbered multi-line
/// status is still "on" for this purpose), and a missing read defaults to tmux's own
/// defaults (`on`, `bottom`) rather than to `Off`/`Top` — an option this could not read
/// should leave the popup behaving as it does today, not silently start clipping the
/// status line.
fn status_position(status: Option<&str>, position: Option<&str>) -> StatusPosition {
    if status == Some("off") {
        return StatusPosition::Off;
    }
    match position {
        Some("top") => StatusPosition::Top,
        _ => StatusPosition::Bottom,
    }
}

/// Parses a `@agent-wizard-width`/`-height`-style percentage option, defaulting to
/// [`DEFAULT_PERCENT`] when unset or not a plain number.
fn percent_cap(raw: Option<&str>) -> u16 {
    raw.and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PERCENT)
}

/// `#{client_width} #{client_height}` split into its two numbers. With no client attached
/// tmux has no size to give and prints both empty, a lone space: there is nothing to open a
/// popup on.
fn parse_client_dims(raw: &str) -> anyhow::Result<(u16, u16)> {
    if raw.trim().is_empty() {
        return Err(CliError::tmux("no attached client to open the popup on"));
    }
    let mut parts = raw.split_whitespace();
    let w = parts.next().and_then(|s| s.parse().ok());
    let h = parts.next().and_then(|s| s.parse().ok());
    match (w, h) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err(CliError::tmux(format!(
            "could not parse client dimensions from {raw:?}"
        ))),
    }
}

/// `display-popup`'s flags before the command. `frameless_ok` and `border_ok` are the
/// tmux >= 3.3 checks (`-B`, `-s` and `-b` all arrived then); below that the popup keeps its
/// frame and title exactly as before. On tmux >= 3.3 the popup, framed or not, is drawn at the
/// terminal's default colours ([`POPUP_STYLE`]).
pub fn popup_args(
    geo: &Geometry,
    full: bool,
    frameless_ok: bool,
    title: &str,
    border: Option<&str>,
    border_ok: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "display-popup".into(),
        "-E".into(),
        "-x".into(),
        geo.x.to_string(),
        "-y".into(),
        geo.y.to_string(),
        "-w".into(),
        geo.w.to_string(),
        "-h".into(),
        geo.h.to_string(),
    ];
    if frameless_ok {
        args.push("-s".into());
        args.push(POPUP_STYLE.into());
    }
    if full && frameless_ok {
        args.push("-B".into());
        return args;
    }
    args.push("-T".into());
    args.push(title.to_string());
    if let (Some(border), true) = (border, border_ok) {
        args.push("-b".into());
        args.push(border.to_string());
    }
    args
}

/// The command `display-popup -E` runs: `"<taw>" ui`, with `--from <pane id>` when the
/// pane the popup is being opened from is known.
///
/// It is one shell *command line*, not an argv — tmux hands it to a shell — so both
/// halves go through `shell_words::quote`: a `taw` installed under a path with a space has
/// to survive that shell, and so, defensively, does whatever `#{pane_id}` reported.
///
/// `--from` exists because `display-popup -E` does **not** give its command the invoking
/// pane's environment: `$TMUX_PANE` is unset inside the popup, so it cannot read the pane
/// it was opened from for itself (`cli::common::switch_client_to` documents the same gap for
/// picking a client). The pane is resolved out here, where tmux still knows it, and
/// handed over explicitly. With no pane to name, the command is exactly what it was before
/// `--from` existed, and `taw ui` falls back to `$TMUX_PANE` on its own.
///
/// `--bar` is appended only when `bar` is true — [`open`] passes whether [`put_up_bar`]
/// actually swapped the status line, never merely whether it tried to: a popup this
/// launches without the bar up (`status off`, a swap that failed) must not claim it in
/// `--bar`, or the popup would write `@agent_wizard_bar` on a session no one is showing it.
///
/// `--new-session` is appended when the launcher was asked for it (`taw ui open
/// --new-session`, and `taw open` with nothing to open): the popup starts on the picker.
fn popup_command(
    bin: &Path,
    origin: Option<&str>,
    client: Option<&str>,
    bar: bool,
    new_session: bool,
) -> String {
    let bin = bin.to_string_lossy();
    let bin = shell_words::quote(&bin);
    let mut cmd = match origin {
        Some(pane) => format!("{bin} ui --from {}", shell_words::quote(pane)),
        None => format!("{bin} ui"),
    };
    if let Some(client) = client {
        cmd.push_str(&format!(" --client {}", shell_words::quote(client)));
    }
    if bar {
        cmd.push_str(" --bar");
    }
    if new_session {
        cmd.push_str(" --new-session");
    }
    cmd
}

/// The command the doorway has tmux run in a client's place:
/// `env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME 'TAW_TMUX_SOCKET_PATH=<socket>' <taw> app --go <machine>:<id>
/// [--back <session>]`, every word quoted for the client's shell (sh-like: tmux runs `-E` with the session's
/// default shell, which is not confirmed here, so a backslash in a path is a known
/// caveat). tmux runs it in the client process with the environment of the shell that ran `tmux attach`,
/// which names no server and may carry another one's `$TMUX`: the `env` prefix pins the app to `socket`, the
/// popup's own server, and clears what else would name one. `None` unless `id` is an id tmux makes (`%`, `$` or
/// `@` and digits) or `new`, which are the only things that may reach a machine. `id` is the machine's own, not
/// `studio:%16`.
///
/// `back` is the id of the session the popup was opened from (`$3`): the app puts the terminal back in tmux on it
/// when it ends ([`super::app`]), so that quitting it does not close the window whose program the tmux client was.
/// Left out unless it is a session id.
///
/// tmux ends the popup's own process before `detach-client` returns, so nothing in the popup runs after the
/// detach, and the popup's `--bar` restore stays with the launcher (`taw ui open`), which runs it when its
/// `display-popup` returns.
pub fn doorway_command(
    bin: &Path,
    socket: &Path,
    machine: &str,
    id: &str,
    back: Option<&str>,
) -> Option<String> {
    if !crate::machine::valid_go_id(id) {
        return None;
    }
    let bin = bin.to_string_lossy();
    let mut command = format!(
        "env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME {} {} app --go {}",
        shell_words::quote(&format!(
            "TAW_TMUX_SOCKET_PATH={}",
            socket.to_string_lossy()
        )),
        shell_words::quote(&bin),
        shell_words::quote(&format!("{machine}:{id}"))
    );
    if let Some(session) = back.filter(|session| crate::machine::valid_session_id(session)) {
        command.push_str(&format!(" --back {}", shell_words::quote(session)));
    }
    Some(command)
}

/// `taw ui open`: works out the client the popup is for ([`current_client`]), reads its size
/// and the live `status`/`status-position` and `@agent-wizard-*` options, computes
/// [`geometry`], and runs `display-popup` on that client for [`popup_command`] — the
/// ratatui popup, launched inside the popup this just opened, and told which client it is
/// on so that what it switches to moves that one.
///
/// While it is open, the status line of the session it was opened from shows taw's bar
/// ([`put_up_bar`]). `display-popup` blocks until the popup closes, so the line is put
/// back right after it returns — whether it opened the popup or failed to.
///
/// Run from a key binding (`run-shell`), so tmux resolves the current client (and the
/// `#{pane_id}` and `#{session_id}` read below) without needing one named explicitly: it is
/// the client whose key ran this. Run from a shell in a pane, as `taw open` does, tmux
/// resolves them the same way: the pane is `$TMUX_PANE`'s, and the client is the one most
/// recently used on its session.
///
/// `from` is `--from` (`taw ui --from <pane> open`): the pane the popup is opened from, which
/// its cursor opens on, in place of the one tmux resolves. Empty, it names none.
///
/// `new_session` starts the popup on the new-session picker ([`popup_command`]).
pub(crate) fn open(ctx: &Ctx, from: Option<&str>, new_session: bool) -> anyhow::Result<Value> {
    // The client the popup is for, worked out once and named to everything after: the size
    // is its own, the popup is shown on it, and the popup is told which it is.
    let client = current_client(ctx)?;
    // A client the app started is the app's: the key sends it back to the app's list and opens
    // no popup. Asked only while an app runs (nothing is asked of tmux with none), so with no
    // app this verb says to tmux what it always said. A request for a new session is not a way
    // back: it opens the popup on the picker, as it does anywhere.
    if !new_session && let Some(app) = files::app_client(&ctx.paths, &ctx.tmux, &client) {
        files::append_event(
            &ctx.paths,
            &wire::Down::Left(wire::Left {
                holder: app.holder,
                pane: app.pane.unwrap_or_default(),
                go: None,
            }),
        );
        ctx.tmux
            .run_ok(&["detach-client", "-t", &client])
            .context("sending the client back to the app")?;
        return Ok(json!({ "to_app": true }));
    }
    let dims = ctx
        .tmux
        .run(&[
            "display-message",
            "-c",
            &client,
            "-p",
            "#{client_width} #{client_height}",
        ])
        .context("reading the client size")?;
    let (client_w, client_h) = parse_client_dims(&dims)?;

    // Kept raw as well: the bar needs the number of rows, which `StatusPosition` drops.
    let status_raw = ctx
        .tmux
        .get_global_option("status")
        .context("reading the status option")?;
    let position = ctx
        .tmux
        .get_global_option("status-position")
        .context("reading the status-position option")?;
    let status = status_position(status_raw.as_deref(), position.as_deref());
    let rows = status_bar::status_rows(status_raw.as_deref());

    let cap_w = percent_cap(
        ctx.tmux
            .get_global_option("@agent-wizard-width")
            .context("reading @agent-wizard-width")?
            .as_deref(),
    );
    let cap_h = percent_cap(
        ctx.tmux
            .get_global_option("@agent-wizard-height")
            .context("reading @agent-wizard-height")?
            .as_deref(),
    );

    let geo = geometry(client_w, client_h, status, rows, cap_w, cap_h);

    let title = ctx
        .tmux
        .get_global_option("@agent-wizard-popup-title")
        .context("reading @agent-wizard-popup-title")?
        .unwrap_or_else(|| DEFAULT_TITLE.to_string());

    let border = ctx
        .tmux
        .get_global_option("@agent-wizard-popup-border")
        .context("reading @agent-wizard-popup-border")?
        .filter(|b| !b.is_empty());

    // `-B`/`-s`/`-b` (frameless popup, the popup's colours, border style) all need tmux >= 3.3;
    // older tmux recognises none of them, so a capped popup, or an older tmux, keeps the frame
    // and title exactly as before.
    let recent = ctx
        .tmux
        .version()
        .context("reading the tmux version")?
        .at_least(3, 3);
    let full = geo.is_full(client_w, client_h, status, rows);
    let mut args = popup_args(&geo, full, recent, &title, border.as_deref(), recent);
    // Shown on that client and no other, whatever tmux would take as the current one by
    // the time the popup opens.
    args.splice(1..1, ["-c".to_string(), client.clone()]);

    // The pane the popup is being opened *from*: the one `--from` names, when it names one,
    // else resolved here, and passed on because the popup itself cannot read it — see
    // [`popup_command`]. A read that fails, or prints nothing, simply leaves `--from` off.
    let origin = from
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ctx.tmux
                .run(&["display-message", "-p", "#{pane_id}"])
                .ok()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
        });

    // Last, after every read that can still fail `open`: nothing may return between the
    // swap and the restore below. `popup_command` needs its outcome — `--bar` names
    // whether the swap actually happened, not merely whether `open` tried it — so the
    // push of that command comes after it.
    let swapped = put_up_bar(ctx, rows);
    args.push(popup_command(
        &ctx.taw_bin,
        origin.as_deref(),
        Some(&client),
        swapped.is_some(),
        new_session,
    ));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = ctx.tmux.run_ok(&arg_refs);
    if let Some((session, saved)) = &swapped
        && let Err(err) = status_bar::restore(&ctx.tmux, session, saved)
    {
        log::warn!("could not put the status line back: {err:#}");
    }
    if let Err(err) = outcome {
        match popup_end(&err) {
            PopupEnd::ClosedFromOutside => {}
            PopupEnd::Status(status) => {
                return Err(CliError::tmux(format!(
                    "the popup ended with status {status}"
                )));
            }
            PopupEnd::Failed => return Err(err.context("opening the popup")),
        }
    }

    Ok(json!({ "x": geo.x, "y": geo.y, "w": geo.w, "h": geo.h }))
}

/// The client the popup opens on: the one tmux takes as the current client, by name. When a
/// key binding ran the launcher that is the client whose key it was, and when it was typed
/// in a pane it is the client most recently used at that pane's session. Empty with no
/// client attached, and then there is nothing to open a popup on.
fn current_client(ctx: &Ctx) -> anyhow::Result<String> {
    let name = ctx
        .tmux
        .run(&["display-message", "-p", "#{client_name}"])
        .context("finding the client the popup opens on")?;
    let name = name.trim();
    if name.is_empty() {
        return Err(CliError::tmux("no attached client to open the popup on"));
    }
    Ok(name.to_string())
}

/// How a `display-popup -E` that did not come back with 0 ended.
enum PopupEnd {
    /// The popup was closed from outside (`taw popup close`, `display-popup -C`): it was shown
    /// and it is gone, which is a way a popup ends and not a failure.
    ClosedFromOutside,
    /// The command in the popup ended with this status, and tmux said nothing about it.
    Status(i32),
    /// tmux refused, said something, or was stopped: the failure as it is.
    Failed,
}

/// The status `display-popup` ends with when its popup is closed from outside.
const CLOSED_FROM_OUTSIDE: i32 = 129;

/// `display-popup -E` ends with the status of the command it ran, and says nothing of its
/// own. So a failure with nothing on stderr is the popup's end, and one with words is tmux
/// refusing.
fn popup_end(err: &anyhow::Error) -> PopupEnd {
    let Some(failed) = err.downcast_ref::<CommandFailed>() else {
        return PopupEnd::Failed;
    };
    match failed.status.code() {
        Some(CLOSED_FROM_OUTSIDE) if failed.stderr.is_empty() => PopupEnd::ClosedFromOutside,
        Some(status) if failed.stderr.is_empty() => PopupEnd::Status(status),
        _ => PopupEnd::Failed,
    }
}

/// Puts taw's bar on the status line of the session the popup is being opened from, with
/// counts from one `list-sessions` and one `list-panes` (the popup keeps them fresh from
/// there). `Some((session, saved))` when the bar went up, for [`open`] to take down.
///
/// Whatever a launcher that died left behind is undone first ([`status_bar::recover`]),
/// even with the status line off (`rows == 0`), so a line turned back on later is the
/// user's own. Nothing here stops the popup opening: a session that cannot be resolved,
/// or a swap tmux refused, is logged and leaves the usual status line up — and a swap that
/// failed partway is undone at once from the record it had already written.
fn put_up_bar(ctx: &Ctx, rows: usize) -> Option<(String, status_bar::Saved)> {
    let session = ctx
        .tmux
        .run(&["display-message", "-p", "#{session_id}"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if rows == 0 {
        if let Err(err) = status_bar::recover(&ctx.tmux, &session) {
            log::warn!("could not undo a stale status line record: {err:#}");
        }
        return None;
    }
    let theme = theme::resolve(&ctx.cfg.theme.name, &ctx.cfg.theme.custom).theme;
    let sessions = ctx.tmux.list_sessions().map(|s| s.len()).unwrap_or(0);
    let panes = ctx.tmux.list_panes().unwrap_or_default();
    let bar = status_bar::bar_format(&theme, &status_bar::counts(&panes, sessions));
    match status_bar::swap(&ctx.tmux, &session, rows, &bar) {
        Ok(saved) => Some((session, saved)),
        Err(err) => {
            log::warn!("could not put taw's bar on the status line: {err:#}");
            if let Err(err) = status_bar::recover(&ctx.tmux, &session) {
                log::warn!("could not put the status line back: {err:#}");
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doorway's command names the session the popup was opened from as `--back`, quoted for the shell, and
    /// leaves the flag out for anything that is not a session id (the app then ends as it did).
    #[test]
    fn the_doorway_command_carries_the_session_to_go_back_to() {
        let go = |back: Option<&str>| {
            doorway_command(
                Path::new("/opt/taw/taw"),
                Path::new("/run/work.sock"),
                "studio",
                "%16",
                back,
            )
            .unwrap()
        };
        let plain = "env -u TMUX -u TMUX_PANE -u TAW_TMUX_SOCKET_NAME \
             'TAW_TMUX_SOCKET_PATH=/run/work.sock' /opt/taw/taw app --go 'studio:%16'";
        assert_eq!(go(None), plain);
        assert_eq!(go(Some("$3")), format!("{plain} --back '$3'"));
        for not_a_session in ["", "$", "3", "%3", "@3", "main", "$3;ls", "$3 x"] {
            assert_eq!(go(Some(not_a_session)), plain, "{not_a_session:?}");
        }
    }

    #[test]
    fn top_status_leaves_row_zero_for_the_status_line() {
        assert_eq!(
            geometry(200, 50, StatusPosition::Top, 1, 100, 100),
            Geometry {
                x: 0,
                y: 50,
                w: 200,
                h: 49
            }
        );
        assert_eq!(
            geometry(200, 50, StatusPosition::Top, 1, 80, 80),
            Geometry {
                x: 20,
                y: 50,
                w: 160,
                h: 40
            }
        );
    }

    #[test]
    fn bottom_status_stops_the_popup_one_row_short_of_the_last_row() {
        assert_eq!(
            geometry(200, 50, StatusPosition::Bottom, 1, 100, 100),
            Geometry {
                x: 0,
                y: 49,
                w: 200,
                h: 49
            }
        );
        assert_eq!(
            geometry(200, 50, StatusPosition::Bottom, 1, 80, 80),
            Geometry {
                x: 20,
                y: 49,
                w: 160,
                h: 40
            }
        );
    }

    #[test]
    fn no_status_line_lets_the_popup_use_the_full_height() {
        assert_eq!(
            geometry(200, 50, StatusPosition::Off, 0, 100, 100),
            Geometry {
                x: 0,
                y: 50,
                w: 200,
                h: 50
            }
        );
        assert_eq!(
            geometry(200, 50, StatusPosition::Off, 0, 80, 80),
            Geometry {
                x: 20,
                y: 50,
                w: 160,
                h: 40
            }
        );
    }

    /// A multi-row status line (`status 2`..`5`) reserves every one of its rows, not
    /// just one — otherwise the popup would cover the bar's row when the line sits at
    /// the bottom, or the last of several rows at the top.
    #[test]
    fn top_status_with_a_multirow_bar_reserves_every_row() {
        assert_eq!(
            geometry(200, 50, StatusPosition::Top, 2, 100, 100),
            Geometry {
                x: 0,
                y: 50,
                w: 200,
                h: 48
            }
        );
    }

    #[test]
    fn bottom_status_with_a_multirow_bar_reserves_every_row() {
        assert_eq!(
            geometry(200, 50, StatusPosition::Bottom, 2, 100, 100),
            Geometry {
                x: 0,
                y: 48,
                w: 200,
                h: 48
            }
        );
    }

    #[test]
    fn a_full_size_popup_is_frameless_on_a_recent_tmux_and_framed_otherwise() {
        let geo = geometry(200, 50, StatusPosition::Top, 1, 100, 100);
        assert!(geo.is_full(200, 50, StatusPosition::Top, 1));
        let args = popup_args(&geo, true, true, " taw ", None, true);
        assert!(args.contains(&"-B".to_string()), "{args:?}");
        assert!(
            !args.contains(&"-T".to_string()),
            "no title without a frame: {args:?}"
        );
        let args = popup_args(&geo, true, false, " taw ", None, true);
        assert!(
            !args.contains(&"-B".to_string()),
            "tmux < 3.3 keeps the frame: {args:?}"
        );
        assert!(args.contains(&"-T".to_string()));
    }

    #[test]
    fn a_capped_popup_keeps_its_frame_and_title() {
        let geo = geometry(200, 50, StatusPosition::Top, 1, 80, 80);
        assert!(!geo.is_full(200, 50, StatusPosition::Top, 1));
        let args = popup_args(&geo, false, true, " taw ", Some("double"), true);
        assert!(!args.contains(&"-B".to_string()));
        let t = args.iter().position(|a| a == "-T").expect("-T");
        assert_eq!(args[t + 1], " taw ");
        let b = args.iter().position(|a| a == "-b").expect("-b");
        assert_eq!(args[b + 1], "double");
    }

    #[test]
    fn a_recent_tmux_draws_the_popup_at_the_terminals_own_colours_framed_or_not() {
        // tmux 3.8's popup-style paints what taw leaves at the default colours a dark grey; the
        // popup's own style keeps them at the terminal's, so a translucent terminal shows through
        // the preview as it does through the pane.
        let style = |args: Vec<String>| {
            let s = args.iter().position(|a| a == "-s")?;
            args.get(s + 1).cloned()
        };
        let full = geometry(200, 50, StatusPosition::Top, 1, 100, 100);
        let capped = geometry(200, 50, StatusPosition::Top, 1, 80, 80);
        let recent_full = popup_args(&full, true, true, " taw ", None, true);
        let recent_capped = popup_args(&capped, false, true, " taw ", Some("double"), true);
        let old = popup_args(&full, true, false, " taw ", None, false);
        assert_eq!(
            style(recent_full).as_deref(),
            Some("fg=terminal,bg=terminal")
        );
        assert_eq!(
            style(recent_capped).as_deref(),
            Some("fg=terminal,bg=terminal")
        );
        assert_eq!(style(old), None, "tmux < 3.3 has no -s");
    }

    #[test]
    fn status_off_full_size_means_the_whole_client() {
        let geo = geometry(200, 50, StatusPosition::Off, 0, 100, 100);
        assert!(geo.is_full(200, 50, StatusPosition::Off, 0));
    }

    #[test]
    fn status_off_wins_over_a_top_position() {
        assert_eq!(
            status_position(Some("off"), Some("top")),
            StatusPosition::Off
        );
    }

    #[test]
    fn any_status_value_other_than_off_counts_as_on() {
        assert_eq!(
            status_position(Some("on"), Some("top")),
            StatusPosition::Top
        );
        assert_eq!(
            status_position(Some("2"), Some("bottom")),
            StatusPosition::Bottom
        );
    }

    #[test]
    fn a_missing_status_read_defaults_to_tmuxs_own_on_and_bottom() {
        assert_eq!(status_position(None, None), StatusPosition::Bottom);
    }

    #[test]
    fn percent_cap_defaults_to_100_when_unset_or_unparsable() {
        assert_eq!(percent_cap(None), 100);
        assert_eq!(percent_cap(Some("not a number")), 100);
        assert_eq!(percent_cap(Some("80")), 80);
    }

    #[test]
    fn parse_client_dims_reads_the_displaymessage_pair() {
        assert_eq!(parse_client_dims("212 58").unwrap(), (212, 58));
        assert!(parse_client_dims("212").is_err());
        assert!(
            parse_client_dims("wide tall")
                .unwrap_err()
                .to_string()
                .starts_with("could not parse client dimensions"),
        );
    }

    /// With no client attached tmux has no size to give: `display-message` prints the two
    /// formats empty, a lone space. That is a popup with nowhere to open, and it is said
    /// so, and not as a size that does not parse.
    #[test]
    fn no_client_to_size_the_popup_for_is_said_so() {
        for raw in ["", " ", "\n"] {
            assert_eq!(
                parse_client_dims(raw).unwrap_err().to_string(),
                "no attached client to open the popup on",
                "{raw:?}"
            );
        }
    }

    /// `shell_words::quote` wraps a pane id because `%` is not one of the characters it
    /// considers safe bare — harmless, and the same shell sees `%5` either way.
    #[test]
    fn the_popup_command_names_the_pane_it_was_opened_from() {
        assert_eq!(
            popup_command(
                Path::new("/usr/local/bin/taw"),
                Some("%5"),
                None,
                false,
                false
            ),
            "/usr/local/bin/taw ui --from '%5'"
        );
    }

    /// No pane to name — `display-message` failed, or printed nothing — leaves the
    /// command exactly as it was before `--from` existed, so `taw ui` falls back to
    /// `$TMUX_PANE` on its own.
    #[test]
    fn the_popup_command_omits_from_when_there_is_no_pane() {
        assert_eq!(
            popup_command(Path::new("/usr/local/bin/taw"), None, None, false, false),
            "/usr/local/bin/taw ui"
        );
    }

    /// The whole command is one `display-popup` argument that tmux hands to a shell, so
    /// both halves stay quoted: a binary under a path with a space, and (defensively — a
    /// tmux pane id is always `%N`) whatever `#{pane_id}` reported.
    #[test]
    fn the_popup_command_quotes_the_binary_and_the_pane() {
        assert_eq!(
            popup_command(Path::new("/opt/my taw/taw"), Some("%5"), None, false, false),
            "'/opt/my taw/taw' ui --from '%5'"
        );
        assert_eq!(
            popup_command(
                Path::new("/usr/local/bin/taw"),
                Some("a b"),
                None,
                false,
                false
            ),
            "/usr/local/bin/taw ui --from 'a b'"
        );
    }

    /// `--bar` only when the launcher actually swapped the status line — never merely
    /// because it tried to. Pane ids still go through `shell_words::quote` like everywhere
    /// else in this command, so `%3` — not "safe bare" to that crate — comes back quoted.
    #[test]
    fn the_popup_command_carries_bar_only_when_the_launcher_swapped_the_status_line() {
        let bin = Path::new("/opt/taw");
        assert_eq!(
            popup_command(bin, Some("%3"), None, true, false),
            "/opt/taw ui --from '%3' --bar"
        );
        assert_eq!(
            popup_command(bin, Some("%3"), None, false, false),
            "/opt/taw ui --from '%3'"
        );
        assert_eq!(
            popup_command(bin, None, None, true, false),
            "/opt/taw ui --bar"
        );
    }

    /// `--new-session` goes last, and only when asked for: the popup then starts on the
    /// new-session picker, which is what `taw open` with nothing to open wants.
    #[test]
    fn the_popup_command_carries_new_session_only_when_asked_for() {
        let bin = Path::new("/opt/taw");
        assert_eq!(
            popup_command(bin, Some("%3"), None, true, true),
            "/opt/taw ui --from '%3' --bar --new-session"
        );
        assert_eq!(
            popup_command(bin, Some("%3"), None, false, true),
            "/opt/taw ui --from '%3' --new-session"
        );
        assert_eq!(
            popup_command(bin, None, None, false, true),
            "/opt/taw ui --new-session"
        );
        assert_eq!(popup_command(bin, None, None, false, false), "/opt/taw ui");
    }

    /// The popup is told which client it is on, after the pane, and the client's name is
    /// quoted for the shell it goes through like the rest.
    #[test]
    fn the_popup_command_names_the_client_it_was_opened_on() {
        let bin = Path::new("/opt/taw");
        assert_eq!(
            popup_command(bin, Some("%3"), Some("/dev/ttys021"), true, true),
            "/opt/taw ui --from '%3' --client /dev/ttys021 --bar --new-session"
        );
        assert_eq!(
            popup_command(bin, None, Some("/dev/ttys021"), false, false),
            "/opt/taw ui --client /dev/ttys021"
        );
        assert_eq!(
            popup_command(bin, None, Some("a b"), false, false),
            "/opt/taw ui --client 'a b'"
        );
    }

    // ------------------------------------------- the bar around `display-popup`

    use std::path::PathBuf;

    use crate::tmux::Tmux;

    use crate::cli::test_support::{Stub, calls, stub_ctx, stub_tmux};

    /// The stand-in for [`open`]: a client of 200x50 in session `$3` (one window) at pane
    /// `%5`, `status` set to `status`, and `display-popup` exiting `popup_exit`.
    fn stub(dir: &Path, status: &'static str, popup_exit: u8) -> (Tmux, PathBuf) {
        stub_tmux(
            dir,
            &Stub {
                status,
                popup_exit,
                ..Stub::default()
            },
        )
    }

    /// What `open` changed, in order: every `set-option`, and `display-popup` with the
    /// command it runs, its last argument — the geometry and frame flags before it are the
    /// popup's own tests' concern, but that command is where `--bar` tells the popup
    /// whether the bar went up.
    fn changes(log: &Path) -> Vec<Vec<String>> {
        calls(log)
            .into_iter()
            .filter(|argv| argv[0] == "set-option" || argv[0] == "display-popup")
            .map(|argv| {
                if argv[0] == "display-popup" {
                    vec![argv[0].clone(), argv[argv.len() - 1].clone()]
                } else {
                    argv
                }
            })
            .collect()
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// The bar `open` puts up for the stub's server: one session, no agents.
    fn stub_bar() -> String {
        let theme = theme::resolve("catppuccin", &Default::default()).theme;
        status_bar::bar_format(
            &theme,
            &status_bar::BarCounts {
                machines: 0,
                sessions: 1,
                agents: 0,
                away: Vec::new(),
            },
        )
    }

    /// `display-popup` running the popup for the stub's pane `%5`: with `--bar` last
    /// exactly when `open` swapped the status line for taw's bar.
    fn popup_run(bar: bool) -> Vec<String> {
        let command = "/usr/local/bin/taw ui --from '%5' --client /dev/ttys001";
        let command = if bar {
            format!("{command} --bar")
        } else {
            command.to_string()
        };
        vec!["display-popup".to_string(), command]
    }

    /// The record, then the bar's text, then row 0 drawing it, then the popup, told the
    /// bar is up; then the line back, and the bar's text and the record gone.
    fn swap_popup_restore() -> Vec<Vec<String>> {
        vec![
            argv(&["set-option", "-t", "$3", status_bar::SAVED_OPTION, "unset"]),
            argv(&[
                "set-option",
                "-t",
                "$3",
                status_bar::BAR_OPTION,
                &stub_bar(),
            ]),
            argv(&[
                "set-option",
                "-t",
                "$3",
                "status-format[0]",
                status_bar::BAR_ROW,
            ]),
            popup_run(true),
            argv(&["set-option", "-u", "-t", "$3", "status-format"]),
            argv(&["set-option", "-u", "-t", "$3", status_bar::BAR_OPTION]),
            argv(&["set-option", "-u", "-t", "$3", status_bar::SAVED_OPTION]),
        ]
    }

    #[test]
    fn the_bar_is_up_exactly_while_the_popup_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 0);
        open(&stub_ctx(tmux), None, false).unwrap();
        assert_eq!(changes(&log), swap_popup_restore());
    }

    /// `open --new-session` is the same launch, the same bar and the same restore, with the
    /// popup it runs told to start on the picker.
    #[test]
    fn opening_with_new_session_starts_the_popup_on_the_picker() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 0);
        open(&stub_ctx(tmux), None, true).unwrap();
        let mut expected = swap_popup_restore();
        expected[3] = vec![
            "display-popup".to_string(),
            "/usr/local/bin/taw ui --from '%5' --client /dev/ttys001 --bar --new-session"
                .to_string(),
        ];
        assert_eq!(changes(&log), expected);
    }

    /// `display-popup -E` comes back with the status of the command the popup ran, and says
    /// nothing of its own. Closing the popup from outside (`taw popup close`, `display-popup
    /// -C`) ends it with 129: the popup was shown and is gone, which is how a popup ends, not
    /// a failure. The line is put back as ever.
    #[test]
    fn a_popup_closed_from_outside_is_a_normal_end() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                popup_exit: 129,
                popup_says: "",
                ..Stub::default()
            },
        );

        open(&stub_ctx(tmux), None, false).expect("closed from outside is not an error");

        assert_eq!(changes(&log), swap_popup_restore());
    }

    /// Any other status with nothing said is the popup's own command ending badly, and the
    /// status is the one thing there is to tell: one plain line, never the `display-popup`
    /// command line with a colon after it and nothing behind that.
    #[test]
    fn a_popup_whose_command_ended_badly_says_its_status() {
        for status in [1, 2, 130] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = stub_tmux(
                dir.path(),
                &Stub {
                    popup_exit: status,
                    popup_says: "",
                    ..Stub::default()
                },
            );

            let err = open(&stub_ctx(tmux), None, false).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                format!("the popup ended with status {status}")
            );
            assert_eq!(changes(&log), swap_popup_restore(), "status {status}");
        }
    }

    /// 129 with words from tmux is not a close from outside: tmux refused, and says why.
    #[test]
    fn a_popup_tmux_refused_says_why_whatever_the_status() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = stub_tmux(
            dir.path(),
            &Stub {
                popup_exit: 129,
                popup_says: "no current client",
                ..Stub::default()
            },
        );

        let err = open(&stub_ctx(tmux), None, false).unwrap_err();

        assert!(
            format!("{err:#}").starts_with("opening the popup: ")
                && format!("{err:#}").ends_with("no current client"),
            "{err:#}"
        );
    }

    /// `taw ui --from <pane> open` opens the popup from that pane: the command it runs names
    /// it, and the flag is not dropped for the pane tmux takes as current.
    /// An empty `--from` names none, and the pane is read from tmux as it is without one.
    #[test]
    fn a_pane_given_to_the_launcher_is_the_one_the_popup_opens_from() {
        for (from, named) in [(Some("%9"), "%9"), (Some(""), "%5"), (None, "%5")] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = stub(dir.path(), "on", 0);

            open(&stub_ctx(tmux), from, false).unwrap();

            let popup = calls(&log)
                .into_iter()
                .find(|call| call[0] == "display-popup")
                .expect("the popup opened");
            assert_eq!(
                popup.last().unwrap(),
                &format!("/usr/local/bin/taw ui --from '{named}' --client /dev/ttys001 --bar"),
                "{from:?}"
            );
        }
    }

    /// The client the popup is for is worked out once, by tmux's own rule, and named to
    /// everything after: the size is read for it, the popup is shown on it (`-c`), and the
    /// popup is told which client it is on, so that every switch it makes moves that client,
    /// however many share the session.
    #[test]
    fn the_client_is_asked_for_once_and_named_to_everything_after() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 0);

        open(&stub_ctx(tmux), None, false).unwrap();

        let calls = calls(&log);
        let asked_for_the_name = calls
            .iter()
            .filter(|call| {
                call[0] == "display-message"
                    && call.last().map(String::as_str) == Some("#{client_name}")
            })
            .count();
        assert_eq!(asked_for_the_name, 1, "{calls:?}");
        assert!(
            calls.contains(&argv(&[
                "display-message",
                "-c",
                "/dev/ttys001",
                "-p",
                "#{client_width} #{client_height}"
            ])),
            "the size was not read for that client: {calls:?}"
        );
        let popup = calls
            .iter()
            .find(|call| call[0] == "display-popup")
            .unwrap();
        assert_eq!(
            popup[..3],
            ["display-popup", "-c", "/dev/ttys001"],
            "not shown on that client: {popup:?}"
        );
        assert!(
            popup.last().unwrap().contains(" --client /dev/ttys001"),
            "the popup was not told: {popup:?}"
        );
    }

    /// Nothing is read, put up or opened once it is known there is no client to open the
    /// popup on: the status line is not touched either.
    #[test]
    fn with_no_client_attached_no_popup_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                clients: false,
                ..Stub::default()
            },
        );

        let err = open(&stub_ctx(tmux), None, false).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "no attached client to open the popup on"
        );
        assert!(changes(&log).is_empty(), "{:?}", calls(&log));
    }

    #[test]
    fn a_popup_that_fails_to_open_still_gets_the_status_line_back() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 1);
        let err = open(&stub_ctx(tmux), None, false).unwrap_err();
        assert!(format!("{err:#}").contains("opening the popup"), "{err:#}");
        assert_eq!(changes(&log), swap_popup_restore());
    }

    /// A swap tmux refuses partway: the record and the bar's text are written, then row 0's
    /// `set-option` fails. The launcher undoes it at once from the record it wrote, so the
    /// popup opens over the usual status line, its command has no `--bar` (it must never write
    /// `@agent_wizard_bar`), and nothing the launcher set is left on the session, while the
    /// popup is open or after.
    #[test]
    fn a_swap_that_fails_opens_the_popup_without_bar_and_leaves_no_option_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_tmux(
            dir.path(),
            &Stub {
                keeps_options: true,
                refuses: Some("status-format[0]"),
                ..Stub::default()
            },
        );

        open(&stub_ctx(tmux), None, false).expect("a failed swap does not stop the popup");

        assert_eq!(
            changes(&log),
            vec![
                argv(&["set-option", "-t", "$3", status_bar::SAVED_OPTION, "unset"]),
                argv(&[
                    "set-option",
                    "-t",
                    "$3",
                    status_bar::BAR_OPTION,
                    &stub_bar()
                ]),
                // Refused: the stand-in set nothing.
                argv(&[
                    "set-option",
                    "-t",
                    "$3",
                    "status-format[0]",
                    status_bar::BAR_ROW
                ]),
                argv(&["set-option", "-u", "-t", "$3", "status-format"]),
                argv(&["set-option", "-u", "-t", "$3", status_bar::BAR_OPTION]),
                argv(&["set-option", "-u", "-t", "$3", status_bar::SAVED_OPTION]),
                popup_run(false),
            ]
        );
        let held = |file: &str| -> Vec<String> {
            let path = dir.path().join(file);
            if path.is_dir() {
                std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            } else {
                std::fs::read_to_string(path)
                    .expect("the popup was opened")
                    .lines()
                    .map(str::to_string)
                    .collect()
            }
        };
        assert_eq!(
            held("options-at-popup"),
            Vec::<String>::new(),
            "the session held options of the launcher's while the popup was open"
        );
        assert_eq!(
            held("options"),
            Vec::<String>::new(),
            "the launcher left options on the session"
        );
    }

    /// No line, no bar, and a popup not told of one, so it writes `@agent_wizard_bar` on
    /// no session — but a record a dead launcher left is still looked for, so a line
    /// turned back on later is the user's own.
    #[test]
    fn with_the_status_line_off_there_is_no_bar() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "off", 0);
        open(&stub_ctx(tmux), None, false).unwrap();
        assert_eq!(changes(&log), vec![popup_run(false)]);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(&format!(
                "show-options\u{1f}-v\u{1f}-t\u{1f}$3\u{1f}{}\u{1f}",
                status_bar::SAVED_OPTION
            )),
            "the stale record was not looked for: {calls:?}"
        );
    }

    // ------------------------------------------- the key back to the app

    /// A process of the test's own that lives until it is ended: the stand-in for a client, whose
    /// parent is this test.
    struct Child(std::process::Child);

    impl Child {
        fn start() -> Child {
            Child(
                std::process::Command::new("/bin/sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn app_record(pid: u32, holder: &str) -> files::AppRecord {
        files::AppRecord {
            pid,
            holder: holder.to_string(),
            ..files::AppRecord::default()
        }
    }

    /// The stand-in with a client whose own process is `client_pid`.
    fn stub_with_client(dir: &Path, client_pid: u32) -> (Tmux, PathBuf) {
        stub_tmux(
            dir,
            &Stub {
                client_pid,
                ..Stub::default()
            },
        )
    }

    /// A client whose parent is a live app goes back to the app: a `left` line for that app's
    /// holder with the pane the client was in, the client detached, and no popup.
    #[test]
    fn a_client_the_app_started_is_sent_back_to_it_and_no_popup_opens() {
        let dir = tempfile::tempdir().unwrap();
        let client = Child::start();
        let (tmux, log) = stub_with_client(dir.path(), client.pid());
        let ctx = stub_ctx(tmux);
        // This test is the client's parent, so it stands in for the app child, which names the client.
        let mut record = app_record(std::process::id(), "mac-77");
        record.client_pid = client.pid();
        files::write_app_record(&ctx.paths, &record);

        let answer = open(&ctx, None, false).unwrap();

        assert_eq!(answer, json!({ "to_app": true }));
        let calls = calls(&log);
        assert!(
            calls.contains(&argv(&["detach-client", "-t", "/dev/ttys001"])),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call[0] == "display-popup"),
            "a popup was opened: {calls:?}"
        );
        let said = std::fs::read_to_string(ctx.paths.events_file()).unwrap();
        let line = wire::parse_down(said.lines().next().unwrap()).unwrap();
        assert_eq!(
            line,
            wire::Down::Left(wire::Left {
                holder: "mac-77".into(),
                pane: "%5".into(),
                go: None
            })
        );
        assert_eq!(said.lines().count(), 1, "{said}");
    }

    /// A new-session request typed in a client the app started opens the popup on the picker: it is not a way
    /// back to the list, and it must not lose its intent. Without it the key goes back (the positive half).
    #[test]
    fn a_new_session_request_in_the_apps_client_opens_the_popup_and_does_not_go_back() {
        let dir = tempfile::tempdir().unwrap();
        let client = Child::start();
        let (tmux, log) = stub_with_client(dir.path(), client.pid());
        let ctx = stub_ctx(tmux);
        let mut record = app_record(std::process::id(), "mac-77");
        record.client_pid = client.pid();
        files::write_app_record(&ctx.paths, &record);

        let answer = open(&ctx, None, false).unwrap();
        assert_eq!(answer, json!({ "to_app": true }), "positive half");
        assert!(
            calls(&log).iter().any(|call| call[0] == "detach-client"),
            "the key back detaches"
        );
        let before = calls(&log).len();

        let answer = open(&ctx, None, true).unwrap();

        assert_ne!(answer, json!({ "to_app": true }));
        let after = calls(&log).split_off(before);
        let popups = popup_calls(&after);
        assert_eq!(popups.len(), 1, "{after:?}");
        assert!(
            popups[0].iter().any(|word| word.contains("--new-session")),
            "the popup is on the picker: {popups:?}"
        );
        assert!(
            !after.iter().any(|call| call[0] == "detach-client"),
            "{after:?}"
        );
    }

    /// A client that is not an app's child (someone attached it by hand) gets the popup with the
    /// argv it always had, and says nothing to any app.
    #[test]
    fn a_client_somebody_attached_by_hand_gets_the_popup_as_before() {
        // The popup's argv with no app at all: what "as before" is.
        let plain = tempfile::tempdir().unwrap();
        let (tmux, plain_log) = stub_with_client(plain.path(), 4242);
        open(&stub_ctx(tmux), None, false).unwrap();
        let before = popup_calls(&calls(&plain_log));
        assert_eq!(before.len(), 1, "the popup opened once: {before:?}");

        let dir = tempfile::tempdir().unwrap();
        let client = Child::start();
        let (tmux, log) = stub_with_client(dir.path(), client.pid());
        let ctx = stub_ctx(tmux);
        // An app runs (a live process with a record), but it is not this client's parent.
        let app = Child::start();
        let mut record = app_record(app.pid(), "mac-77");
        record.client_pid = client.pid();
        files::write_app_record(&ctx.paths, &record);
        assert_eq!(files::read_app_records(&ctx.paths).len(), 1, "the app runs");

        open(&ctx, None, false).unwrap();

        let calls = calls(&log);
        assert_eq!(popup_calls(&calls), before, "the popup's argv changed");
        assert!(
            !calls.iter().any(|call| call[0] == "detach-client"),
            "{calls:?}"
        );
        assert!(!ctx.paths.events_file().exists(), "nothing was said");
    }

    /// The `display-popup` calls among `calls`.
    fn popup_calls(calls: &[Vec<String>]) -> Vec<Vec<String>> {
        calls
            .iter()
            .filter(|call| call[0] == "display-popup")
            .cloned()
            .collect()
    }

    /// With no app record the argv is today's, and a record of an app that is gone changes none of
    /// it: the calls are the same ones, and the record is taken away.
    #[test]
    fn a_record_of_a_dead_app_changes_no_argv_and_is_removed() {
        let plain = tempfile::tempdir().unwrap();
        let (tmux, plain_log) = stub(plain.path(), "on", 0);
        open(&stub_ctx(tmux), None, false).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 0);
        let ctx = stub_ctx(tmux);
        let mut gone = std::process::Command::new("/bin/sleep")
            .arg("0")
            .spawn()
            .unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        files::write_app_record(&ctx.paths, &app_record(dead, "mac-77"));
        assert!(ctx.paths.app_record(dead).exists());

        open(&ctx, None, false).unwrap();

        assert_eq!(calls(&log), calls(&plain_log));
        assert!(!ctx.paths.app_record(dead).exists(), "the record stayed");
        // The two runs would share an extra ask, so what is pinned is that neither made one: the popup
        // opened (positive half) and no call asked for a client's pid.
        for (run, said) in [("plain", calls(&plain_log)), ("dead record", calls(&log))] {
            assert!(
                said.iter().any(|call| call[0] == "display-popup"),
                "{run}: no popup: {said:?}"
            );
            assert!(
                !said
                    .iter()
                    .any(|call| call.iter().any(|word| word.contains("#{client_pid}"))),
                "{run} asked for a client's pid: {said:?}"
            );
        }
    }

    // ----------------------------------------- the key back, on a machine

    /// The stand-in with a client whose process is `client_pid`, on the terminal `tty`, among the
    /// terminals `clients` that have one (the stand-in alone says `/dev/ttys001` and never a terminal in
    /// the question for the key rule): the question `#{client_pid} #{pane_id} #{client_tty}` is
    /// answered with the terminal added, the list of clients with one line a terminal, and everything else
    /// is the stand-in's own, which logs every call.
    fn stub_on_terminal(
        dir: &Path,
        client_pid: u32,
        tty: &str,
        clients: &[&str],
    ) -> (Tmux, PathBuf) {
        let (tmux, log) = stub_with_client(dir, client_pid);
        let inner = dir.join("tmux-inner");
        std::fs::rename(&tmux.bin, &inner).unwrap();
        let listed: String = clients
            .iter()
            .map(|tty| format!("{tty}\t{tty}\t$0\tmain\n"))
            .collect();
        std::fs::write(
            &tmux.bin,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *'#{{client_tty}}'*) printf '%s %s\\n' \"$('{inner}' \"$@\")\" '{tty}'; exit 0 ;;\n  *list-clients*) '{inner}' \"$@\" >/dev/null; printf '{listed}'; exit 0 ;;\nesac\nexec '{inner}' \"$@\"\n",
                inner = inner.display(),
                listed = listed.replace('\n', "\\n").replace('$', "\\$"),
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&tmux.bin);
        (tmux, log)
    }

    /// This machine's lease, held by `holder` and last refreshed `age_ms` ago.
    fn lease_held(ctx: &Ctx, holder: &str, age_ms: u64) {
        assert!(files::offer_lease(
            &ctx.paths,
            holder,
            files::now_ms() - age_ms
        ));
    }

    /// What a `taw machine attach` left for the client on `tty`.
    fn attached(ctx: &Ctx, tty: &str, holder: &str, pid: u32) {
        attached_at(ctx, tty, holder, pid, files::now_ms());
    }

    /// The same, written at `at` (Unix milliseconds).
    fn attached_at(ctx: &Ctx, tty: &str, holder: &str, pid: u32, at: u64) {
        files::write_attached(
            &ctx.paths,
            &files::AttachedRecord {
                tty: tty.to_string(),
                holder: holder.to_string(),
                at,
                pid,
            },
        )
        .unwrap();
    }

    /// The pid of a process that has ended.
    fn a_dead_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        dead
    }

    fn left_lines(ctx: &Ctx) -> Vec<wire::Down> {
        std::fs::read_to_string(ctx.paths.events_file())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| wire::parse_down(line).ok())
            .collect()
    }

    /// A client an app on another machine attached here, while that app looks at this machine, goes back
    /// to the app: a `left` line for the app's holder with the pane the client was in, the client
    /// detached, and no popup.
    #[test]
    fn a_client_an_app_elsewhere_attached_is_sent_back_to_it_and_no_popup_opens() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_on_terminal(dir.path(), 4242, "/dev/ttys001", &["/dev/ttys001"]);
        let ctx = stub_ctx(tmux);
        attached(&ctx, "/dev/ttys001", "mac-77", 4242);
        lease_held(&ctx, "mac-77", 0);

        let answer = open(&ctx, None, false).unwrap();

        assert_eq!(answer, json!({ "to_app": true }));
        let said = calls(&log);
        assert!(
            said.contains(&argv(&["detach-client", "-t", "/dev/ttys001"])),
            "{said:?}"
        );
        assert!(
            !said.iter().any(|call| call[0] == "display-popup"),
            "a popup was opened: {said:?}"
        );
        assert_eq!(
            left_lines(&ctx),
            vec![wire::Down::Left(wire::Left {
                holder: "mac-77".into(),
                pane: "%5".into(),
                go: None
            })]
        );
    }

    /// The lease has no say in the way back: a client an app attached here goes back to that app with
    /// no lease on file, under a lease 16 s old, and under another app's fresh lease (a doorway app or a second app
    /// never holds it, and an app reopened a moment after the last one quit does not hold it yet).
    #[test]
    fn a_client_an_app_attached_goes_back_to_it_whoever_holds_the_lease() {
        for lease in [None, Some(("mac-77", 16_000)), Some(("mac-88", 0))] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = stub_on_terminal(dir.path(), 4242, "/dev/ttys001", &["/dev/ttys001"]);
            let ctx = stub_ctx(tmux);
            attached(&ctx, "/dev/ttys001", "mac-77", 4242);
            if let Some((holder, age)) = lease {
                lease_held(&ctx, holder, age);
            }

            let answer = open(&ctx, None, false).unwrap();

            assert_eq!(answer, json!({ "to_app": true }), "{lease:?}");
            let said = calls(&log);
            assert!(
                said.contains(&argv(&["detach-client", "-t", "/dev/ttys001"])),
                "{lease:?}: {said:?}"
            );
            assert!(popup_calls(&said).is_empty(), "{lease:?}: {said:?}");
            assert_eq!(
                left_lines(&ctx),
                vec![wire::Down::Left(wire::Left {
                    holder: "mac-77".into(),
                    pane: "%5".into(),
                    go: None
                })],
                "{lease:?}"
            );
        }
    }

    /// Without a record for the client's terminal the popup opens, whatever the lease says; a record for
    /// another terminal is not this client's.
    #[test]
    fn a_client_with_no_record_gets_the_popup_even_under_a_fresh_lease() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_on_terminal(
            dir.path(),
            4242,
            "/dev/ttys001",
            &["/dev/ttys001", "/dev/ttys002"],
        );
        let ctx = stub_ctx(tmux);
        // The record of the other terminal names the client the stub lists (4242), so the test is about the
        // terminal alone: the client of this terminal has no record, and the other one's is not its.
        attached(&ctx, "/dev/ttys002", "mac-77", 4242);
        lease_held(&ctx, "mac-77", 0);
        // The question was asked: the record of the other terminal is there and its client is.
        let answer = open(&ctx, None, false).unwrap();

        assert_ne!(answer, json!({ "to_app": true }));
        let said = calls(&log);
        assert!(
            said.iter()
                .any(|call| call.last().is_some_and(|w| w.contains("#{client_tty}"))),
            "positive half, the client was asked about: {said:?}"
        );
        assert_eq!(popup_calls(&said).len(), 1, "{said:?}");
        assert!(!said.iter().any(|call| call[0] == "detach-client"));
        assert!(left_lines(&ctx).is_empty());
    }

    /// Two apps each attached a client here (the app a person started and a doorway app, say), and one of them
    /// holds the lease: each client goes back to its own app, with a `left` line for that app's holder.
    #[test]
    fn of_two_holders_with_a_record_each_each_client_goes_back_to_its_own_app() {
        let both = ["/dev/ttys001", "/dev/ttys002"];
        let answer_for = |tty: &str, holder: &str| {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = stub_on_terminal(dir.path(), 4242, tty, &both);
            let ctx = stub_ctx(tmux);
            attached(&ctx, "/dev/ttys001", "mac-77", 4242);
            attached(&ctx, "/dev/ttys002", "mac-88", 4242);
            lease_held(&ctx, holder, 0);
            let answer = open(&ctx, None, false).unwrap();
            let detached = calls(&log).iter().any(|call| call[0] == "detach-client");
            (
                answer == json!({ "to_app": true }),
                detached,
                left_lines(&ctx),
            )
        };
        let left = |holder: &str| {
            vec![wire::Down::Left(wire::Left {
                holder: holder.into(),
                pane: "%5".into(),
                go: None,
            })]
        };
        for lease in ["mac-77", "mac-88"] {
            assert_eq!(
                answer_for("/dev/ttys001", lease),
                (true, true, left("mac-77"))
            );
            assert_eq!(
                answer_for("/dev/ttys002", lease),
                (true, true, left("mac-88"))
            );
        }
    }

    /// A terminal that a later client took over under the same name is not the client the record is for:
    /// the record names the process that attached.
    #[test]
    fn a_client_that_took_over_the_terminal_later_is_not_the_one_the_record_is_for() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_on_terminal(dir.path(), 4242, "/dev/ttys001", &["/dev/ttys001"]);
        let ctx = stub_ctx(tmux);
        attached(&ctx, "/dev/ttys001", "mac-77", 9999);
        lease_held(&ctx, "mac-77", 0);

        let answer = open(&ctx, None, false).unwrap();

        assert_ne!(answer, json!({ "to_app": true }));
        assert_eq!(popup_calls(&calls(&log)).len(), 1);
        assert!(left_lines(&ctx).is_empty());
    }

    /// A client that is both an app's own (its parent is a live app that names it) and has a record here
    /// belongs to the app on this Mac: its `left` line is for that app's holder.
    #[test]
    fn a_client_that_is_the_apps_own_and_has_a_record_here_goes_to_the_app_on_this_mac() {
        let dir = tempfile::tempdir().unwrap();
        let client = Child::start();
        let (tmux, log) =
            stub_on_terminal(dir.path(), client.pid(), "/dev/ttys001", &["/dev/ttys001"]);
        let ctx = stub_ctx(tmux);
        let mut record = app_record(std::process::id(), "mac-77");
        record.client_pid = client.pid();
        files::write_app_record(&ctx.paths, &record);
        attached(&ctx, "/dev/ttys001", "studio-5", client.pid());
        lease_held(&ctx, "studio-5", 0);

        let answer = open(&ctx, None, false).unwrap();

        assert_eq!(answer, json!({ "to_app": true }));
        assert!(calls(&log).iter().any(|call| call[0] == "detach-client"));
        assert_eq!(
            left_lines(&ctx),
            vec![wire::Down::Left(wire::Left {
                holder: "mac-77".into(),
                pane: "%5".into(),
                go: None
            })]
        );
    }

    /// A record whose terminal has no client, whose process is gone and that is more than 10 s old is
    /// taken away as it is read, and with nothing else to ask about the key asks tmux nothing more than
    /// which clients there are.
    #[test]
    fn a_record_whose_terminal_has_no_client_is_removed_as_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub_on_terminal(dir.path(), 4242, "/dev/ttys001", &["/dev/ttys001"]);
        let ctx = stub_ctx(tmux);
        attached_at(
            &ctx,
            "/dev/ttys009",
            "mac-77",
            a_dead_pid(),
            files::now_ms() - 11_000,
        );
        lease_held(&ctx, "mac-77", 0);
        assert!(files::read_attached(&ctx.paths, "/dev/ttys009").is_some());

        let answer = open(&ctx, None, false).unwrap();

        assert_ne!(answer, json!({ "to_app": true }));
        assert!(files::read_attached(&ctx.paths, "/dev/ttys009").is_none());
        let said = calls(&log);
        assert!(
            said.iter().any(|call| call[0] == "list-clients"),
            "positive half: {said:?}"
        );
        assert!(
            !said
                .iter()
                .any(|call| call.iter().any(|w| w.contains("#{client_pid}"))),
            "{said:?}"
        );
    }

    /// A client that `taw machine attach` is still starting (its record is written, tmux does not list it
    /// yet) is not forgotten by a key pressed elsewhere in that gap: the record stays, that key opens the
    /// popup, and once the client is on the list its own key goes back to the app.
    #[test]
    fn a_client_that_is_not_listed_yet_keeps_its_record_and_its_key_goes_back_once_it_is() {
        // The client's process has ended (a pid that is not alive here), and the stub and the record name it: the
        // grace of a record written just now is what keeps it.
        let gone = a_dead_pid();
        let before = tempfile::tempdir().unwrap();
        let (not_yet, not_yet_log) = stub_on_terminal(before.path(), gone, "/dev/ttys001", &[]);
        let after = tempfile::tempdir().unwrap();
        let (listed, listed_log) =
            stub_on_terminal(after.path(), gone, "/dev/ttys001", &["/dev/ttys001"]);
        let mut ctx = stub_ctx(listed.clone());
        // The record is the attach's own: written just now, naming the client's process.
        attached(&ctx, "/dev/ttys001", "mac-77", gone);
        lease_held(&ctx, "mac-77", 0);

        ctx.tmux = not_yet;
        let answer = open(&ctx, None, false).unwrap();
        assert_ne!(answer, json!({ "to_app": true }));
        let said = calls(&not_yet_log);
        assert!(
            said.iter().any(|call| call[0] == "list-clients"),
            "positive half, the clients were asked: {said:?}"
        );
        assert_eq!(popup_calls(&said).len(), 1, "{said:?}");
        assert!(files::read_attached(&ctx.paths, "/dev/ttys001").is_some());

        // The client is on the list now: the same record, still there, sends its key back.
        ctx.tmux = listed;
        let answer = open(&ctx, None, false).unwrap();
        assert_eq!(answer, json!({ "to_app": true }));
        assert!(
            calls(&listed_log)
                .iter()
                .any(|call| call[0] == "detach-client")
        );
    }

    /// An empty `attached/` folder (every record gone) changes nothing about what the key says to tmux:
    /// the calls are those of a run with no folder, and neither asks tmux about a client.
    #[test]
    fn an_empty_attached_folder_changes_no_argv() {
        let plain = tempfile::tempdir().unwrap();
        let (tmux, plain_log) = stub(plain.path(), "on", 0);
        open(&stub_ctx(tmux), None, false).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = stub(dir.path(), "on", 0);
        let ctx = stub_ctx(tmux);
        std::fs::create_dir_all(ctx.paths.attached_dir()).unwrap();

        open(&ctx, None, false).unwrap();

        assert_eq!(calls(&log), calls(&plain_log));
        for (run, said) in [("plain", calls(&plain_log)), ("empty folder", calls(&log))] {
            assert!(
                said.iter().any(|call| call[0] == "display-popup"),
                "{run}: no popup: {said:?}"
            );
            assert!(
                !said.iter().any(|call| call[0] == "list-clients"
                    || call.iter().any(|w| w.contains("#{client_pid}"))),
                "{run} asked about a client: {said:?}"
            );
        }
    }
}

#[cfg(test)]
mod doorway_new_tests {
    use super::*;

    /// The doorway takes `new` as an id of a machine, for the row of a machine with no session.
    #[test]
    fn the_doorway_command_takes_new_and_no_other_word() {
        let line = |id: &str| {
            doorway_command(
                Path::new("/opt/taw/taw"),
                Path::new("/run/work.sock"),
                "studio",
                id,
                None,
            )
        };
        assert!(
            line("new").is_some_and(|line| line.ends_with("app --go studio:new")),
            "{:?}",
            line("new")
        );
        assert_eq!(line("old"), None);
        assert_eq!(line(""), None);
    }
}
