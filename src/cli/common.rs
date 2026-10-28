//! Helpers shared by the verb groups: target resolution that reports `not_found`, the
//! logical→tmux key-name translation herdr's `send-keys` vocabulary needs, terminal
//! snapshot reads (`--source`/`--lines`/`--ansi`), and `K=V` parsing.

use anyhow::Context as _;
use serde_json::Value;

use super::envelope::CliError;
use crate::tmux::{ClientInfo, PaneInfo, SessionInfo, Tmux, WindowInfo, ids};

/// herdr's `--source`: which terminal snapshot to read. tmux has one scrollback, so
/// these map onto `capture-pane` flags rather than distinct buffers:
///
/// | herdr | tmux |
/// |---|---|
/// | `visible` | `capture-pane -p` (the rendered viewport only) |
/// | `recent` | `capture-pane -p -S -<lines>` (viewport plus scrollback) |
/// | `recent-unwrapped` | the same, plus `-J` (soft-wrapped lines joined) |
/// | `detection` | `capture-pane -p` with escapes stripped — the exact text `detect` sees |
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    Visible,
    #[default]
    Recent,
    #[value(name = "recent-unwrapped")]
    RecentUnwrapped,
    Detection,
}

/// herdr's `--format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    #[default]
    Text,
    Ansi,
}

/// How many lines of scrollback `recent`/`recent-unwrapped` read when `--lines` is
/// absent. herdr does not document its own default; this is deep enough for a finished
/// agent turn without dragging in an entire session.
pub const DEFAULT_RECENT_LINES: u32 = 200;

/// Reads a pane's terminal text per herdr's `--source`/`--lines`/`--format` flags.
/// `Detection` always returns plain text (it is the detection engine's own input);
/// every other source honours `ansi`.
pub fn read_pane(
    t: &Tmux,
    pane: &str,
    source: Source,
    lines: Option<u32>,
    ansi: bool,
) -> anyhow::Result<String> {
    capture_pane(t, pane, source, lines, ansi, None)
}

/// `format` expanded for `pane` and its screen, in one tmux invocation (`display-message ... ;
/// capture-pane ...`), so that what the format says describes the screen: the expansion is one line,
/// and the screen is everything after it. The screen is what [`Source::Detection`] reads.
pub fn display_and_capture(t: &Tmux, pane: &str, format: &str) -> anyhow::Result<(String, String)> {
    let out = t.run(&[
        "display-message",
        "-p",
        "-t",
        pane,
        format,
        ";",
        "capture-pane",
        "-p",
        "-t",
        pane,
    ])?;
    let (head, screen) = out.split_once('\n').unwrap_or((out.as_str(), ""));
    Ok((head.to_string(), screen.to_string()))
}

/// Like [`read_pane`], but capturing only the pane rows strictly *above* the cursor — the
/// committed output, excluding the line the user may be typing an as-yet-unexecuted command
/// on. `pane wait-output` matches on this so a command *typed but not run* cannot
/// false-positive on the very pattern it contains: typed input sits on the cursor's line,
/// while executed output lands above it (the shell draws a fresh prompt below). Returns
/// `""` when the cursor is on the top visible row — nothing has been committed above it.
pub fn read_pane_above_cursor(
    t: &Tmux,
    pane: &str,
    source: Source,
    lines: Option<u32>,
    ansi: bool,
) -> anyhow::Result<String> {
    let cursor_y: i64 = t
        .display(pane, "#{cursor_y}")
        .map_err(|e| CliError::tmux(format!("reading cursor position of pane '{pane}': {e:#}")))?
        .trim()
        .parse()
        .unwrap_or(0);
    if cursor_y <= 0 {
        return Ok(String::new());
    }
    capture_pane(t, pane, source, lines, ansi, Some(cursor_y - 1))
}

/// Shared `capture-pane` body for [`read_pane`] and [`read_pane_above_cursor`]. `end`, when
/// set, bounds the capture with `-E <end>` (an inclusive last visible row, 0-based from the
/// top of the viewport).
fn capture_pane(
    t: &Tmux,
    pane: &str,
    source: Source,
    lines: Option<u32>,
    ansi: bool,
    end: Option<i64>,
) -> anyhow::Result<String> {
    let escapes = ansi && source != Source::Detection;
    let mut args: Vec<String> = vec!["capture-pane".into(), "-p".into()];
    if escapes {
        args.push("-e".into());
    }
    match source {
        Source::Visible | Source::Detection => {}
        Source::Recent | Source::RecentUnwrapped => {
            if source == Source::RecentUnwrapped {
                args.push("-J".into());
            }
            // `-S -N` starts N lines above the top of the viewport; tmux clamps it to
            // however much scrollback actually exists.
            args.push("-S".into());
            args.push(format!("-{}", lines.unwrap_or(DEFAULT_RECENT_LINES)));
        }
    }
    if let Some(end) = end {
        args.push("-E".into());
        args.push(end.to_string());
    }
    args.push("-t".into());
    args.push(pane.to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = t
        .run(&borrowed)
        .map_err(|e| CliError::tmux(format!("capturing pane '{pane}': {e:#}")))?;

    // For `visible`/`detection`, `--lines` trims the captured viewport to its last N rows
    // (there is no scrollback to widen into).
    match (source, lines) {
        (Source::Visible | Source::Detection, Some(n)) if n > 0 => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(n as usize);
            Ok(all[start..].join("\n"))
        }
        _ => Ok(text),
    }
}

/// The pane word for "the pane this process runs in", read from `$TMUX_PANE`. The plugin gives it
/// to every pane as `HERDR_PANE_ID` when `@agent-wizard-herdr-env` is on, because tmux hands every
/// pane the one global value: herdr's agents write `pane report-agent "$HERDR_PANE_ID"`. tmux reads
/// a leading `%` as a pane id, so no session or window name can have meant it.
pub const CALLING_PANE: &str = "%calling";

/// Resolves a pane target (public id, raw `%N`, tmux target, a live `@agent_name`, or
/// [`CALLING_PANE`]) to its tmux pane id, reporting `not_found` rather than the underlying resolution error.
///
/// A *well-formed* id is translated by `ids::parse` without asking tmux, so `w9:p99`
/// becomes `%99` whether or not that pane still exists. This confirms it does, so a stale
/// id — from a tmux hook firing as a pane closes, or a plugin holding an old handle —
/// reports `not_found` here rather than surfacing as a raw `tmux_error` from whatever
/// command runs next. The tmux-target and agent-name paths already went to the server in
/// order to resolve at all, so they need no second round trip.
pub fn resolve_pane(t: &Tmux, target: &str) -> anyhow::Result<String> {
    // The word `@agent-wizard-herdr-env` hands every pane as `HERDR_PANE_ID`: the pane this
    // process runs in, before any tmux lookup. Read once: a `$TMUX_PANE` that is itself the
    // word is looked up like any other target, and fails there.
    let calling;
    let target = if target == CALLING_PANE {
        calling = current_pane()?;
        calling.as_str()
    } else {
        target
    };
    let pane_id = ids::resolve_pane(t, target).map_err(|e| {
        log::debug!("resolving pane '{target}': {e:#}");
        CliError::not_found(format!("pane '{target}' not found"))
    })?;
    if ids::parse(target).is_some() {
        let live = t
            .display(&pane_id, "#{pane_id}")
            .map(|out| out.trim() == pane_id)
            .unwrap_or(false);
        if !live {
            return Err(CliError::not_found(format!("pane '{target}' not found")));
        }
    }
    Ok(pane_id)
}

/// [`resolve_pane`], then the full [`PaneInfo`] for it out of one server-wide listing.
pub fn pane_info(t: &Tmux, target: &str) -> anyhow::Result<PaneInfo> {
    let pane_id = resolve_pane(t, target)?;
    list_panes(t)?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .ok_or_else(|| CliError::not_found(format!("pane '{target}' not found")))
}

/// Resolves a window/tab target to its tmux window id, reporting `not_found`.
pub fn resolve_window(t: &Tmux, target: &str) -> anyhow::Result<String> {
    ids::resolve_window(t, target).map_err(|e| {
        log::debug!("resolving tab '{target}': {e:#}");
        CliError::not_found(format!("tab '{target}' not found"))
    })
}

/// Resolves a session/workspace target to its tmux session id, reporting `not_found`.
///
/// A *well-formed* id (`w<N>`, `$<N>`) is translated by `ids::parse` without asking tmux,
/// so — like [`resolve_pane`] — this then confirms the session actually exists. Otherwise
/// `workspace rename`/`close` and `tab create --workspace` would hand an id for a session
/// that is gone straight to `rename-session`/`kill-session`/`new-window -t`, which leaks a
/// raw tmux "can't find session" instead of a clean `not_found`.
pub fn resolve_session(t: &Tmux, target: &str) -> anyhow::Result<String> {
    let id = ids::resolve_session(t, target).map_err(|e| {
        log::debug!("resolving workspace '{target}': {e:#}");
        CliError::not_found(format!("workspace '{target}' not found"))
    })?;
    if ids::parse(target).is_some() {
        let live = t
            .display(&id, "#{session_id}")
            .map(|out| out.trim() == id)
            .unwrap_or(false);
        if !live {
            return Err(CliError::not_found(format!(
                "workspace '{target}' not found"
            )));
        }
    }
    Ok(id)
}

/// [`resolve_window`], then the [`WindowInfo`] for it.
pub fn window_info(t: &Tmux, target: &str) -> anyhow::Result<WindowInfo> {
    let window_id = resolve_window(t, target)?;
    list_windows(t)?
        .into_iter()
        .find(|w| w.window_id == window_id)
        .ok_or_else(|| CliError::not_found(format!("tab '{target}' not found")))
}

/// [`resolve_session`], then the [`SessionInfo`] for it.
pub fn session_info(t: &Tmux, target: &str) -> anyhow::Result<SessionInfo> {
    let session_id = resolve_session(t, target)?;
    list_sessions(t)?
        .into_iter()
        .find(|s| s.session_id == session_id)
        .ok_or_else(|| CliError::not_found(format!("workspace '{target}' not found")))
}

/// `list-panes -a` with a `tmux_error` code on failure. An empty server (no sessions at
/// all) lists nothing rather than failing.
pub fn list_panes(t: &Tmux) -> anyhow::Result<Vec<PaneInfo>> {
    t.list_panes().or_else(|e| empty_or_tmux_error(e, "panes"))
}

/// [`list_panes`], and the output of the tmux command `then` right after it in the same invocation
/// ([`Tmux::list_panes_then`]); no panes and no answer on a server that holds no sessions.
pub fn list_panes_then(t: &Tmux, then: &[&str]) -> anyhow::Result<(Vec<PaneInfo>, String)> {
    t.list_panes_then(then).or_else(|e| {
        empty_or_tmux_error::<PaneInfo>(e, "panes").map(|panes| (panes, String::new()))
    })
}

/// `list-windows` with a `tmux_error` code on failure.
pub fn list_windows(t: &Tmux) -> anyhow::Result<Vec<WindowInfo>> {
    t.list_windows().or_else(|e| empty_or_tmux_error(e, "tabs"))
}

/// `list-sessions` with a `tmux_error` code on failure.
pub fn list_sessions(t: &Tmux) -> anyhow::Result<Vec<SessionInfo>> {
    t.list_sessions()
        .or_else(|e| empty_or_tmux_error(e, "workspaces"))
}

/// tmux's list commands fail rather than print nothing when the server holds no sessions
/// ("no server running on ...", "no sessions"). For a *listing* verb that is an empty
/// result, not an error — anything else is a real `tmux_error`.
fn empty_or_tmux_error<T>(err: anyhow::Error, what: &str) -> anyhow::Result<Vec<T>> {
    let text = err.to_string();
    if text.contains("no server running") || text.contains("no sessions") {
        return Ok(Vec::new());
    }
    Err(CliError::tmux(format!("listing {what}: {err:#}")))
}

/// [`switch_client_to`] the session called `name`, or `false` when no client could be
/// moved: never a hard error, since whatever the caller already did (a session created, a
/// worktree entered) stands regardless of whether anyone was there to switch to it. `taw
/// session open` and `taw open --json` report it as `switched`.
///
/// `pane` is the pane this process runs in, if it runs in one (`$TMUX_PANE`); see
/// [`switch_client_to`] for what it decides.
///
/// The popup switches through [`switch_client_to`] itself (`tui::actions::switch_client`):
/// its toast has to say *why* a switch failed, which the `bool` here has already thrown
/// away.
pub(crate) fn switch_client(t: &Tmux, name: &str, pane: Option<&str>) -> anyhow::Result<bool> {
    Ok(switch_client_to(t, &crate::tmux::session_target(name), pane).is_ok())
}

/// `switch-client -t <target>` for the client a person is at: a session by exact name
/// ([`crate::tmux::session_target`]), or an id tmux resolves itself.
///
/// From a `pane` (the one this process was started in, `$TMUX_PANE`) whose session has a
/// client attached, which client is the person's is tmux's to say: with no `-c`, it moves
/// the most recently active client of the pane's session, the one whose keys ran the
/// command. Taking the first client the server lists instead moves the wrong screen when
/// a session is on two of them.
///
/// With no pane there is nothing for tmux to go by: a key binding's `run-shell`, a hook and
/// a popup's own `display-popup -E` command have no `$TMUX_PANE`, and plain `switch-client`
/// there either moves nothing or, on a multi-client server, moves a client the user is not
/// looking at. [`resolve_client`] picks one by hand and names it with `-c`. A pane whose
/// session nobody is attached to is treated as no pane: a shell in a background session
/// does not say which client is the person's, and tmux would move whichever client was used
/// last on the whole server.
///
/// A popup that `taw ui open` opened does not come through here: its launcher named the
/// client it opened it on, and every switch the popup makes moves that one
/// (`tui::actions::switch_client`).
pub(crate) fn switch_client_to(t: &Tmux, target: &str, pane: Option<&str>) -> anyhow::Result<()> {
    if let Some(pane) = pane
        && has_a_client(t, pane)
    {
        return t.run_ok(&["switch-client", "-t", target]);
    }
    let client = resolve_client(t)?;
    t.run_ok(&["switch-client", "-c", &client, "-t", target])
}

/// Whether any client is attached to the session `pane` is in.
fn has_a_client(t: &Tmux, pane: &str) -> bool {
    t.display(pane, "#{session_attached}")
        .ok()
        .and_then(|attached| attached.trim().parse::<u32>().ok())
        .is_some_and(|attached| attached > 0)
}

/// Takes an attached client to `pane`: switches it to the pane's session, selects the pane's
/// window, then selects the pane — what `agent focus` does, and what `pane move --focus` does
/// once its pane has landed in another session. `join-pane` and a bare `select-pane` change
/// what a session has selected but never move a client onto that session, so without the
/// `switch-client` a pane moved into another workspace was selected where nobody was looking
/// (herdr v0.9.1, #3760, #4153, #4171).
///
/// `label` is how the caller named the pane, for the error messages.
///
/// `@agent_seen` is deliberately *not* written here: tmux fires `after-select-pane` and
/// `after-select-window` for these very commands, and the plugin script binds those to `taw
/// seen <pane>`, so seen-ness is recorded by the same path a human's keypress takes.
pub(crate) fn focus_pane(t: &Tmux, pane: &PaneInfo, label: &str) -> anyhow::Result<()> {
    // A detached session has no client to switch; that is not a failure.
    let _ = t.run_ok(&["switch-client", "-t", &pane.session_id]);
    t.run_ok(&["select-window", "-t", &pane.window_id])
        .map_err(|e| CliError::tmux(format!("selecting tab of pane '{label}': {e:#}")))?;
    t.run_ok(&["select-pane", "-t", &pane.pane_id])
        .map_err(|e| CliError::tmux(format!("selecting pane '{label}': {e:#}")))?;
    Ok(())
}

/// Which tmux client a headless `taw` invocation — or a popup that was not told its client
/// (a `taw ui` typed in a pane) — should switch.
///
/// 1. the attached client whose session is *this* invocation's own session (from
///    `$TMUX`'s third field) — the client the user pressed the key, or ran the hook, in;
/// 2. failing that, when exactly one client is attached to the server, take it (the
///    common single-terminal case, and the only unambiguous answer available);
/// 3. otherwise refuse, rather than switch a client the user is not looking at.
pub(crate) fn resolve_client(t: &Tmux) -> anyhow::Result<String> {
    pick_client(&t.list_clients()?, caller_session_id().as_deref())
}

/// [`resolve_client`]'s rule, over values rather than a live server, so every branch
/// (including the refusal) is testable.
fn pick_client(clients: &[ClientInfo], caller_session: Option<&str>) -> anyhow::Result<String> {
    anyhow::ensure!(!clients.is_empty(), "no attached client to switch");
    if let Some(session) = caller_session
        && let Some(client) = clients.iter().find(|c| c.session_id == session)
    {
        return Ok(client.client_name.clone());
    }
    if let [only] = clients {
        return Ok(only.client_name.clone());
    }
    anyhow::bail!(
        "cannot tell which of {} attached clients to switch",
        clients.len()
    )
}

/// The session id (third field) of the session `$TMUX` names, or `None` outside tmux —
/// which a `display-popup -E` command never is, but a plain `run-shell`/hook invocation
/// still carries `$TMUX` from the server that spawned it, naming its own session rather
/// than any particular client's.
fn caller_session_id() -> Option<String> {
    parse_session_id(&std::env::var("TMUX").ok()?)
}

/// The session id in a `$TMUX` value (`<socket path>,<server pid>,<session id>`), as the
/// `$N` form tmux uses everywhere else. `None` when the field is missing or empty.
fn parse_session_id(tmux: &str) -> Option<String> {
    let id = tmux.split(',').nth(2)?.trim();
    (!id.is_empty()).then(|| format!("${id}"))
}

/// The tmux pane id of the pane this process is running in, from `$TMUX_PANE`.
pub fn current_pane() -> anyhow::Result<String> {
    std::env::var("TMUX_PANE")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CliError::not_found(
                "no current pane: $TMUX_PANE is not set (run this from inside a tmux pane, \
                 or pass a pane id)",
            )
        })
}

/// Picks the pane a `[TARGET] [--pane <id>] [--current]` verb should act on: the explicit
/// positional target, else `--pane`, else `$TMUX_PANE` (whether or not `--current` was
/// passed — herdr's `--current` is an explicit spelling of the same default).
pub fn pane_target(
    t: &Tmux,
    positional: Option<&str>,
    pane_flag: Option<&str>,
) -> anyhow::Result<String> {
    match positional.or(pane_flag) {
        Some(target) => resolve_pane(t, target),
        None => {
            let pane = current_pane()?;
            resolve_pane(t, &pane)
        }
    }
}

/// herdr's `--focus` / `--no-focus` pair, resolved the same way for every verb that
/// creates something (`pane split`, `workspace create`, `tab create`, `worktree
/// create`/`open`).
///
/// Focusing is the **default**: tmux's own `split-window`/`new-window`/`new-session`
/// focus what they create, and herdr's SKILL tells agents to pass `--no-focus` for
/// background work — advice that only makes sense if the default is to focus. So
/// `--focus` merely states that default explicitly, `--no-focus` is the flag that
/// changes anything, and `--no-focus` wins if both are somehow given (the quieter
/// reading of a contradiction).
///
/// This lives here rather than per-group because the two spellings had diverged:
/// `pane split` defaulted to focusing while `workspace`/`tab`/`worktree` used
/// `!no_focus && focus`, which never focused without `--focus` *and* made `--no-focus` a
/// no-op — so `workspace create --no-focus` was silently doing nothing at all.
pub fn wants_focus(_focus: bool, no_focus: bool) -> bool {
    !no_focus
}

/// Parses `KEY=VALUE` (as `--env`/`--token`/`--state-label` take it). The value may
/// contain `=`; the key may not be empty.
pub fn parse_kv(s: &str) -> anyhow::Result<(String, String)> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(CliError::invalid_argument(format!(
            "expected KEY=VALUE, got '{s}'"
        ))),
    }
}

/// Sends literal text to a pane (`send-keys -l -- <text>`); `--` keeps text that starts
/// with `-` from being read as a flag. The text is the last word of the command, so one that
/// ends in `;` is written as [`crate::store::word`] writes it: tmux takes a `;` at the end of a
/// word for the end of the command, and typed `find . -exec rm {} \;` as `find . -exec rm {} ;`
/// and `claude --x;` as `claude --x`.
pub fn send_text(t: &Tmux, pane: &str, text: &str) -> anyhow::Result<()> {
    t.run_ok(&[
        "send-keys",
        "-l",
        "-t",
        pane,
        "--",
        &crate::store::word(text),
    ])
    .map_err(|e| CliError::tmux(format!("sending text to pane '{pane}': {e:#}")))
}

/// Sends Enter to a pane.
pub fn send_enter(t: &Tmux, pane: &str) -> anyhow::Result<()> {
    t.run_ok(&["send-keys", "-t", pane, "Enter"])
        .map_err(|e| CliError::tmux(format!("sending Enter to pane '{pane}': {e:#}")))
}

/// What a send that tmux makes only while the pane runs what the caller saw, and takes input, did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sent {
    /// tmux sent it.
    Typed,
    /// The command in front of the pane was not the one the caller had seen when tmux came to send
    /// it, and tmux sent nothing. `typed` says whether some of what the caller sends had gone before
    /// (a piece of a long text), which is then left on the line.
    HeldBack { typed: bool },
    /// The pane took no input when tmux came to send it, for the reason `why` gives: a person had put
    /// it in copy mode, or turned its input off, since the caller looked. tmux sent nothing, which the
    /// mode would have had, or nobody. `typed` says whether some of what the caller sends had gone
    /// before (a piece of a long text).
    NoInput {
        why: crate::session::prompt::NoInput,
        typed: bool,
    },
}

/// What tmux prints first when [`guarded`] held a send back, and then why, a field each after a tab
/// ([`held_back_by`]). A send prints nothing.
const HELD_BACK: &str = "taw-held-back";

/// The most bytes of text one guarded send carries. The text is written as a word of a tmux command
/// line ([`crate::tmux::command_word`]), which at most doubles it, and a tmux command line cannot be
/// more than about 16 KiB, so a longer text is sent in pieces.
const PIECE: usize = 4096;

/// Runs the tmux command `then` for `pane` only if, when tmux comes to run it, the command in front of
/// the pane is still `seen` (when the caller saw one) and the pane takes input: one `if-shell -F` in the
/// one tmux invocation, so that nothing happens between the question and the send. A caller that has
/// checked that the agent is in front, and then types, would otherwise type into the shell the agent
/// leaves behind when it exits in between (it would be the shell's prompt, and a shell may run what it
/// is typed), or into a pane a person has put in copy mode in between, or turned the input of off: tmux
/// gives what is sent to the mode, which drops it with nobody attached, or to nobody, and the verb said
/// it had sent it. It cannot close the gap fully: an agent that exits and leaves a command of the same
/// name in front (the agent is a script, and its shell has the same name as what ran it) is not told
/// apart. A send held back has tmux print why, in the same invocation: whether the command in front was
/// still `seen`, and the pane's mode and input.
fn guarded(t: &Tmux, pane: &str, seen: Option<&str>, then: &str) -> anyhow::Result<Sent> {
    let still_in_front = match seen {
        Some(seen) => format!(
            "#{{==:#{{pane_current_command}},{}}}",
            crate::store::format_literal(seen)
        ),
        None => "1".to_string(),
    };
    let takes_input = "#{?pane_in_mode,0,#{?pane_input_off,0,1}}";
    let held_back = format!(
        "display-message -p -t {} {}",
        crate::tmux::command_word(pane),
        crate::tmux::command_word(&format!(
            "{HELD_BACK}\t{still_in_front}\t#{{pane_in_mode}}\t#{{pane_mode}}\t#{{pane_input_off}}"
        ))
    );
    let out = t.run(&[
        "if-shell",
        "-F",
        "-t",
        pane,
        &format!("#{{&&:{still_in_front},{takes_input}}}"),
        then,
        &held_back,
    ])?;
    Ok(held_back_by(&out))
}

/// What the tmux of a [`guarded`] send printed says it did: nothing for a send it made, and for one it
/// held back, [`HELD_BACK`] and why, a field each: whether the command in front was still the one the
/// caller had seen (`1`), and the pane's `#{pane_in_mode}`, `#{pane_mode}` and `#{pane_input_off}`. An
/// agent that has gone is what is said of a pane that also takes no input: the input is no matter then.
fn held_back_by(out: &str) -> Sent {
    let mut fields = out.trim_end_matches(['\n', '\r']).split('\t');
    if fields.next() != Some(HELD_BACK) {
        return Sent::Typed;
    }
    let mut next = || fields.next().unwrap_or_default();
    let (in_front, in_mode, mode, input_off) = (next(), next(), next(), next());
    match crate::session::prompt::NoInput::of(in_mode, mode, input_off) {
        Some(why) if in_front == "1" => Sent::NoInput { why, typed: false },
        _ => Sent::HeldBack { typed: false },
    }
}

/// [`send_text`] while the command in front of `pane` is still `seen` and the pane takes input
/// ([`guarded`]): a piece at a time, and none after one that is held back.
pub fn send_text_if_in_front(t: &Tmux, pane: &str, text: &str, seen: &str) -> anyhow::Result<Sent> {
    for (index, piece) in pieces_of(text).enumerate() {
        let then = format!(
            "send-keys -l -t {} -- {}",
            crate::tmux::command_word(pane),
            crate::tmux::command_word(piece)
        );
        match guarded(t, pane, Some(seen), &then)
            .map_err(|e| CliError::tmux(format!("sending text to pane '{pane}': {e:#}")))?
        {
            Sent::Typed => {}
            Sent::NoInput { why, .. } => {
                return Ok(Sent::NoInput {
                    why,
                    typed: index > 0,
                });
            }
            Sent::HeldBack { .. } => return Ok(Sent::HeldBack { typed: index > 0 }),
        }
    }
    Ok(Sent::Typed)
}

/// [`send_enter`] while the command in front of `pane` is still `seen` and the pane takes input
/// ([`guarded`]).
pub fn send_enter_if_in_front(t: &Tmux, pane: &str, seen: &str) -> anyhow::Result<Sent> {
    let then = format!("send-keys -t {} Enter", crate::tmux::command_word(pane));
    guarded(t, pane, Some(seen), &then)
        .map_err(|e| CliError::tmux(format!("sending Enter to pane '{pane}': {e:#}")))
}

/// `text` in pieces of at most [`PIECE`] bytes, each ending where a character does.
fn pieces_of(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(PIECE);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (piece, tail) = rest.split_at(end);
        rest = tail;
        Some(piece)
    })
}

/// Translates herdr's logical key names into tmux key names, so `esc`, `ctrl+c` and
/// `Escape`/`C-c` all work. Every key is translated (and rejected) *before* any byte is
/// written, matching herdr: "validates all keys before writing any bytes".
///
/// Accepts `ctrl+`/`c-`, `alt+`/`m-`/`meta+` and `shift+` modifiers in any order, a
/// named key (`esc`, `enter`, `tab`, `space`, `bspace`, arrows, `home`/`end`, `pgup`/
/// `pgdn`, `f1`-`f12`, `insert`, `delete`), or a single literal character.
pub fn translate_key(key: &str) -> anyhow::Result<String> {
    let mut rest = key;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(r) = strip_modifier(&lower, &["ctrl+", "control+", "c-"]) {
            ctrl = true;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = strip_modifier(&lower, &["alt+", "meta+", "m-"]) {
            alt = true;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = strip_modifier(&lower, &["shift+", "s-"]) {
            shift = true;
            rest = &rest[rest.len() - r.len()..];
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return Err(CliError::invalid_argument(format!(
            "'{key}' names modifiers but no key"
        )));
    }

    let base = named_key(&rest.to_ascii_lowercase())
        .map(str::to_string)
        .or_else(|| {
            // A single character is itself; anything longer that is not a known name is a
            // tmux key name we pass through only when tmux could plausibly know it (all
            // ASCII alphanumeric), so typos like `not+a+key` are still rejected.
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c.to_string()),
                _ => None,
            }
        });
    let base = base.ok_or_else(|| {
        CliError::invalid_argument(format!(
            "unknown key '{key}' (use a single character, a named key such as esc/enter/tab/up, \
             or a modifier form such as ctrl+c)"
        ))
    })?;

    let mut out = String::new();
    if ctrl {
        out.push_str("C-");
    }
    if alt {
        out.push_str("M-");
    }
    if shift {
        out.push_str("S-");
    }
    out.push_str(&base);
    Ok(out)
}

fn strip_modifier<'a>(lower: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        // `c-`/`m-`/`s-` must not swallow a lone `-` key or a name like `c-` alone.
        .filter(|r| !r.is_empty())
}

/// herdr's logical key names → tmux's. `esc` is herdr's canonical Escape spelling.
fn named_key(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "esc" | "escape" => "Escape",
        "enter" | "return" | "cr" => "Enter",
        "tab" => "Tab",
        "btab" | "backtab" => "BTab",
        "space" => "Space",
        "bspace" | "backspace" | "bs" => "BSpace",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "home" => "Home",
        "end" => "End",
        "pgup" | "pageup" | "ppage" => "PageUp",
        "pgdn" | "pagedown" | "npage" => "PageDown",
        "insert" | "ic" => "IC",
        "delete" | "del" | "dc" => "DC",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        _ => return None,
    })
}

/// [`send_keys`] while the command in front of `pane` is still `seen` and the pane takes input
/// ([`guarded`]): the keys that were sent, and whether they were.
pub fn send_keys_if_in_front(
    t: &Tmux,
    pane: &str,
    keys: &[String],
    seen: &str,
) -> anyhow::Result<(Vec<String>, Sent)> {
    let translated: Vec<String> = keys
        .iter()
        .map(|k| translate_key(k))
        .collect::<anyhow::Result<_>>()?;
    let words: Vec<String> = translated
        .iter()
        .map(|key| crate::tmux::command_word(key))
        .collect();
    let then = format!(
        "send-keys -t {} {}",
        crate::tmux::command_word(pane),
        words.join(" ")
    );
    let sent = guarded(t, pane, Some(seen), &then)
        .map_err(|e| CliError::tmux(format!("sending keys to pane '{pane}': {e:#}")))?;
    Ok((translated, sent))
}

/// Sends a whole key sequence, translating (and so validating) every key first. Each key is a word of
/// tmux's command, so one that ends in `;` (the key `;` itself, or `M-;`) is written as
/// [`crate::store::word`] writes it: tmux takes a `;` at the end of a word for the end of the command,
/// and sent nothing for the key `;`, and failed on the key after it with "unknown command".
pub fn send_keys(t: &Tmux, pane: &str, keys: &[String]) -> anyhow::Result<Vec<String>> {
    let translated: Vec<String> = keys
        .iter()
        .map(|k| translate_key(k))
        .collect::<anyhow::Result<_>>()?;
    let words: Vec<String> = translated
        .iter()
        .map(|key| crate::store::word(key))
        .collect();
    let mut args: Vec<&str> = vec!["send-keys", "-t", pane];
    args.extend(words.iter().map(String::as_str));
    t.run_ok(&args)
        .map_err(|e| CliError::tmux(format!("sending keys to pane '{pane}': {e:#}")))?;
    Ok(translated)
}

/// Unix seconds now — the clock `@agent_state_since` is written in.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The interval `agent wait`, `agent prompt --wait`, `agent start` and
/// `pane wait-output` poll tmux at: 250 ms.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Polls `probe` every [`POLL_INTERVAL`] until it yields `Some`, or `timeout_ms` elapses
/// (`None` waits indefinitely, as herdr's flagless waits do). `probe` is always called
/// once before any sleeping, so an already-satisfied condition returns immediately.
pub fn poll_until<T>(
    timeout_ms: Option<u64>,
    what: &str,
    mut probe: impl FnMut() -> anyhow::Result<Option<T>>,
) -> anyhow::Result<T> {
    let deadline =
        timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    loop {
        if let Some(found) = probe()? {
            return Ok(found);
        }
        if let Some(deadline) = deadline
            && std::time::Instant::now() >= deadline
        {
            return Err(CliError::timeout(format!(
                "timed out after {}ms waiting for {what}",
                timeout_ms.unwrap_or(0)
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Reads a pane's geometry: `(left, right, top, bottom)` in cells, for every pane in the
/// window holding `pane`. Used by `pane neighbor`/`edges`/`focus`/`swap --direction`,
/// which need adjacency tmux itself only exposes relative to the *active* pane.
pub struct Geometry {
    pub pane_id: String,
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// Every pane of the window holding `pane_id`, with its cell geometry.
pub fn window_geometry(t: &Tmux, window_id: &str) -> anyhow::Result<Vec<Geometry>> {
    let out = t
        .run(&[
            "list-panes",
            "-t",
            window_id,
            "-F",
            "#{pane_id}\t#{pane_left}\t#{pane_right}\t#{pane_top}\t#{pane_bottom}",
        ])
        .map_err(|e| CliError::tmux(format!("reading geometry of tab '{window_id}': {e:#}")))?;
    out.lines()
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            anyhow::ensure!(f.len() == 5, "unexpected list-panes geometry line '{line}'");
            let num = |i: usize| -> anyhow::Result<u32> {
                f[i].parse()
                    .with_context(|| format!("parsing '{}' in '{line}'", f[i]))
            };
            Ok(Geometry {
                pane_id: f[0].to_string(),
                left: num(1)?,
                right: num(2)?,
                top: num(3)?,
                bottom: num(4)?,
            })
        })
        .collect()
}

/// herdr's `--direction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// The `resize-pane`/`select-pane` flag for this direction.
    pub fn flag(self) -> &'static str {
        match self {
            Direction::Left => "-L",
            Direction::Right => "-R",
            Direction::Up => "-U",
            Direction::Down => "-D",
        }
    }
}

/// The pane immediately `direction` of `from` within its window: the closest pane whose
/// facing edge touches `from`'s, and whose span on the other axis overlaps `from`'s.
/// `None` when `from` is already at that edge of the window.
pub fn neighbor<'a>(
    panes: &'a [Geometry],
    from: &Geometry,
    direction: Direction,
) -> Option<&'a Geometry> {
    let overlaps_vertically = |g: &Geometry| g.top <= from.bottom && g.bottom >= from.top;
    let overlaps_horizontally = |g: &Geometry| g.left <= from.right && g.right >= from.left;

    let candidates = panes.iter().filter(|g| g.pane_id != from.pane_id);
    match direction {
        // `right + 1 == left` is the divider column between two adjacent panes; tmux
        // also allows a pane to abut directly, so both are accepted.
        Direction::Left => candidates
            .filter(|g| overlaps_vertically(g) && g.right < from.left)
            .max_by_key(|g| g.right),
        Direction::Right => candidates
            .filter(|g| overlaps_vertically(g) && g.left > from.right)
            .min_by_key(|g| g.left),
        Direction::Up => candidates
            .filter(|g| overlaps_horizontally(g) && g.bottom < from.top)
            .max_by_key(|g| g.bottom),
        Direction::Down => candidates
            .filter(|g| overlaps_horizontally(g) && g.top > from.bottom)
            .min_by_key(|g| g.top),
    }
}

/// `{"type":"ok"}` with no extra fields — for verbs whose whole result is "it worked".
pub fn ok() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Fires a plugin lifecycle `event` with `payload`, building the plugin context from the
/// CLI's own. A missing manifest or a failed emit is logged and never fails the verb that
/// emitted it — the workspace/tab/pane already exists by the time this runs, so an event
/// problem must not unwind a mutation that has already succeeded (matches how
/// `session::worktree`'s own event emits handle failure).
pub(crate) fn emit_event(ctx: &super::Ctx, event: &str, payload: Value) {
    let plugin_ctx = crate::plugin::PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    if let Err(err) = crate::plugin::events::emit(&plugin_ctx, event, payload) {
        log::warn!("{event} event: {err:#}");
    }
}

/// Asks `[hooks] pre_close` whether `closing` may go ahead, before a verb closes it. A refusal
/// is an error carrying the hook's own failure, which the envelope reports as `hook_refused`
/// with the hook's name and what it wrote to stderr; nothing has been closed.
pub(crate) fn pre_close(
    ctx: &super::Ctx,
    closing: crate::hooks::Closing<'_>,
) -> anyhow::Result<()> {
    let plugin_ctx = crate::plugin::PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    crate::hooks::pre_close(&plugin_ctx, closing)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A text longer than a piece is cut at the end of a character, in order, and loses nothing.
    #[test]
    fn a_text_is_sent_in_pieces_that_end_where_a_character_does_and_lose_nothing() {
        assert_eq!(pieces_of("").count(), 0);
        assert_eq!(pieces_of("short").collect::<Vec<_>>(), ["short"]);
        // Three-byte characters never fit a piece evenly: the cut moves back to the end of one.
        let text = "\u{65e5}".repeat(3 * PIECE / 2);
        let pieces: Vec<&str> = pieces_of(&text).collect();
        assert!(pieces.len() >= 3, "{}", pieces.len());
        assert!(pieces.iter().all(|piece| piece.len() <= PIECE));
        assert!(
            pieces[..pieces.len() - 1]
                .iter()
                .all(|piece| piece.len() >= PIECE - 3)
        );
        assert_eq!(pieces.concat(), text);
        let plain = "x".repeat(PIECE * 2 + 1);
        let lengths: Vec<usize> = pieces_of(&plain).map(str::len).collect();
        assert_eq!(lengths, [PIECE, PIECE, 1]);
    }

    /// What a guarded send's tmux printed: nothing for a send it made, and for one it held back, whether
    /// the command in front was still the one seen, and the pane's mode and input.
    #[test]
    fn a_send_held_back_says_whether_the_agent_had_gone_or_the_pane_took_no_input() {
        use crate::session::prompt::NoInput;
        assert_eq!(held_back_by(""), Sent::Typed);
        assert_eq!(held_back_by("\n"), Sent::Typed);
        // The agent has gone, from a pane that takes input.
        assert_eq!(
            held_back_by("taw-held-back\t0\t0\t\t0\n"),
            Sent::HeldBack { typed: false }
        );
        // The agent is still in front of a pane in copy mode, or with its input off.
        assert_eq!(
            held_back_by("taw-held-back\t1\t1\tcopy-mode\t0\n"),
            Sent::NoInput {
                why: NoInput::Mode("copy mode".to_string()),
                typed: false
            }
        );
        assert_eq!(
            held_back_by("taw-held-back\t1\t0\t\t1"),
            Sent::NoInput {
                why: NoInput::Off,
                typed: false
            }
        );
        // Gone from a pane that takes no input either: the agent has gone, whatever the input.
        assert_eq!(
            held_back_by("taw-held-back\t0\t1\tcopy-mode\t1\n"),
            Sent::HeldBack { typed: false }
        );
        // A tmux that said nothing more than that it held the send back.
        assert_eq!(
            held_back_by("taw-held-back"),
            Sent::HeldBack { typed: false }
        );
    }

    #[test]
    fn translates_herdr_key_names_to_tmux_key_names() {
        for (input, expected) in [
            ("esc", "Escape"),
            ("escape", "Escape"),
            ("Escape", "Escape"),
            ("enter", "Enter"),
            ("tab", "Tab"),
            ("space", "Space"),
            ("up", "Up"),
            ("pgdn", "PageDown"),
            ("f5", "F5"),
            ("ctrl+c", "C-c"),
            ("C-c", "C-c"),
            ("alt+x", "M-x"),
            ("ctrl+alt+delete", "C-M-DC"),
            ("a", "a"),
            ("/", "/"),
        ] {
            assert_eq!(translate_key(input).unwrap(), expected, "for {input:?}");
        }
    }

    #[test]
    fn rejects_unknown_key_names_before_any_bytes_are_written() {
        for bad in ["not+a+key", "definitely-not-a-key", "ctrl+", ""] {
            let err = translate_key(bad).unwrap_err();
            assert_eq!(
                super::super::envelope::code_for(&err),
                "invalid_argument",
                "for {bad:?}"
            );
        }
    }

    #[test]
    fn focus_is_the_default_and_no_focus_turns_it_off() {
        assert!(
            wants_focus(false, false),
            "tmux's own create commands focus, so must ours"
        );
        assert!(wants_focus(true, false), "--focus restates the default");
        assert!(
            !wants_focus(false, true),
            "--no-focus must actually turn focusing off"
        );
        assert!(
            !wants_focus(true, true),
            "--no-focus wins a contradiction (the quieter reading)"
        );
    }

    #[test]
    fn parses_key_value_pairs_keeping_equals_in_the_value() {
        assert_eq!(
            parse_kv("FOO=bar").unwrap(),
            ("FOO".to_string(), "bar".to_string())
        );
        assert_eq!(
            parse_kv("URL=https://x/?a=b").unwrap(),
            ("URL".to_string(), "https://x/?a=b".to_string())
        );
        assert_eq!(
            parse_kv("EMPTY=").unwrap(),
            ("EMPTY".to_string(), String::new())
        );
        assert!(parse_kv("noequals").is_err());
        assert!(parse_kv("=novalue").is_err());
    }

    fn geom(id: &str, left: u32, right: u32, top: u32, bottom: u32) -> Geometry {
        Geometry {
            pane_id: id.to_string(),
            left,
            right,
            top,
            bottom,
        }
    }

    #[test]
    fn finds_the_neighbour_in_each_direction_of_a_left_right_split() {
        // 80x24 split down the middle: %1 is columns 0-39, %2 columns 41-79.
        let panes = vec![geom("%1", 0, 39, 0, 23), geom("%2", 41, 79, 0, 23)];
        assert_eq!(
            neighbor(&panes, &panes[0], Direction::Right)
                .unwrap()
                .pane_id,
            "%2"
        );
        assert_eq!(
            neighbor(&panes, &panes[1], Direction::Left)
                .unwrap()
                .pane_id,
            "%1"
        );
        assert!(neighbor(&panes, &panes[0], Direction::Left).is_none());
        assert!(neighbor(&panes, &panes[0], Direction::Up).is_none());
        assert!(neighbor(&panes, &panes[0], Direction::Down).is_none());
    }

    #[test]
    fn finds_the_neighbour_in_a_top_bottom_split_and_ignores_non_overlapping_panes() {
        // Top row spans the width; the bottom row is split in two.
        let panes = vec![
            geom("%1", 0, 79, 0, 11),
            geom("%2", 0, 39, 13, 23),
            geom("%3", 41, 79, 13, 23),
        ];
        assert_eq!(
            neighbor(&panes, &panes[0], Direction::Down)
                .unwrap()
                .pane_id,
            "%2",
            "the leftmost overlapping pane below wins the tie by position"
        );
        assert_eq!(
            neighbor(&panes, &panes[2], Direction::Up).unwrap().pane_id,
            "%1"
        );
        assert_eq!(
            neighbor(&panes, &panes[1], Direction::Right)
                .unwrap()
                .pane_id,
            "%3"
        );
        assert!(neighbor(&panes, &panes[2], Direction::Right).is_none());
    }

    #[test]
    fn a_lone_pane_has_no_neighbours() {
        let panes = vec![geom("%1", 0, 79, 0, 23)];
        for d in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert!(neighbor(&panes, &panes[0], d).is_none());
        }
    }

    #[test]
    fn poll_until_returns_immediately_when_already_satisfied() {
        let mut calls = 0;
        let got = poll_until(Some(0), "test", || {
            calls += 1;
            Ok(Some(7))
        })
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(
            calls, 1,
            "must probe before sleeping or checking the deadline"
        );
    }

    #[test]
    fn poll_until_reports_timeout_with_the_documented_code() {
        let err = poll_until(Some(1), "a state that never arrives", || Ok(None::<()>)).unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "timeout");
        assert!(err.to_string().contains("a state that never arrives"));
    }

    #[test]
    fn poll_until_propagates_a_probe_error_rather_than_retrying_forever() {
        let err = poll_until(None, "x", || -> anyhow::Result<Option<()>> {
            Err(CliError::tmux("boom"))
        })
        .unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "tmux_error");
    }

    fn client(name: &str, session_id: &str) -> ClientInfo {
        ClientInfo {
            client_name: name.to_string(),
            client_tty: name.to_string(),
            session_id: session_id.to_string(),
            session_name: format!("s{session_id}"),
        }
    }

    #[test]
    fn one_attached_client_is_the_one_that_invoked_taw() {
        let clients = [client("/dev/ttys001", "$1")];
        // Whatever the caller's own session is, there is only one answer available.
        assert_eq!(pick_client(&clients, Some("$9")).unwrap(), "/dev/ttys001");
        assert_eq!(pick_client(&clients, None).unwrap(), "/dev/ttys001");
    }

    #[test]
    fn among_several_clients_the_one_on_the_callers_own_session_wins() {
        let clients = [
            client("/dev/ttys001", "$1"),
            client("/dev/ttys002", "$2"),
            client("/dev/ttys003", "$3"),
        ];
        assert_eq!(pick_client(&clients, Some("$2")).unwrap(), "/dev/ttys002");
    }

    #[test]
    fn several_clients_and_no_match_is_a_refusal_not_a_guess() {
        let clients = [client("/dev/ttys001", "$1"), client("/dev/ttys002", "$2")];
        for caller_session in [Some("$9"), None] {
            let err = pick_client(&clients, caller_session)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("cannot tell which of 2 attached clients"),
                "{err}"
            );
        }
    }

    #[test]
    fn no_attached_client_is_reported_as_such() {
        let err = pick_client(&[], Some("$1")).unwrap_err().to_string();
        assert_eq!(err, "no attached client to switch");
    }

    // ------------------------------------------------------------- switching

    use crate::cli::test_support::{Stub, calls, changes, stub_tmux};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    /// The stand-in tmux for a switch: a client attached (`/dev/ttys001`), and pane `%5`'s
    /// session watched by it or not.
    fn switching(
        dir: &std::path::Path,
        clients: bool,
        watched: bool,
    ) -> (Tmux, std::path::PathBuf) {
        stub_tmux(
            dir,
            &Stub {
                clients,
                watched,
                ..Stub::default()
            },
        )
    }

    /// From a pane, which client is the one that is there is tmux's to say: the most
    /// recently active client of the pane's session, which is the one whose keys ran this
    /// command. So no `-c`, and no listing of clients to pick from by hand.
    #[test]
    fn from_a_pane_tmux_is_left_to_pick_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = switching(dir.path(), true, true);

        switch_client_to(&tmux, &crate::tmux::session_target("api"), Some("%5")).unwrap();

        assert_eq!(
            changes(&log, &["switch-client"]),
            vec![argv(&["switch-client", "-t", "=api"])]
        );
        assert!(
            calls(&log).iter().all(|call| call[0] != "list-clients"),
            "a client was picked by hand: {:?}",
            calls(&log)
        );
    }

    /// With no pane (a popup's own command, a hook, a key binding's `run-shell`) tmux has
    /// nothing to tell the client from, so it is picked here, and named.
    #[test]
    fn with_no_pane_the_client_is_picked_by_hand_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = switching(dir.path(), true, true);

        switch_client_to(&tmux, &crate::tmux::session_target("api"), None).unwrap();

        assert_eq!(
            changes(&log, &["switch-client"]),
            vec![argv(&["switch-client", "-c", "/dev/ttys001", "-t", "=api"])]
        );
    }

    /// A pane whose session nobody is looking at does not say which client is the person's:
    /// left to itself tmux would move whichever client was used last on the whole server,
    /// which is a client that is not there. The client is picked by hand, and refused when
    /// it cannot be told, as it is with no pane.
    #[test]
    fn a_pane_that_nobody_is_looking_at_is_no_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, log) = switching(dir.path(), true, false);

        switch_client_to(&tmux, &crate::tmux::session_target("api"), Some("%5")).unwrap();

        assert_eq!(
            changes(&log, &["switch-client"]),
            vec![argv(&["switch-client", "-c", "/dev/ttys001", "-t", "=api"])]
        );
        assert!(
            calls(&log).contains(&argv(&[
                "display-message",
                "-p",
                "-t",
                "%5",
                "#{session_attached}"
            ])),
            "the pane's session was not asked about: {:?}",
            calls(&log)
        );
    }

    #[test]
    fn a_switch_with_no_client_to_move_says_so_and_moves_nothing() {
        for pane in [None, Some("%5")] {
            let dir = tempfile::tempdir().unwrap();
            let (tmux, log) = switching(dir.path(), false, false);

            let err =
                switch_client_to(&tmux, &crate::tmux::session_target("api"), pane).unwrap_err();

            assert_eq!(err.to_string(), "no attached client to switch", "{pane:?}");
            assert!(changes(&log, &["switch-client"]).is_empty(), "{pane:?}");
        }
    }

    /// `session open` reports `switched`, and never fails for want of a client.
    #[test]
    fn switch_client_says_whether_a_client_moved() {
        let dir = tempfile::tempdir().unwrap();
        let (tmux, _) = switching(dir.path(), true, true);
        assert!(switch_client(&tmux, "api", None).unwrap());
        assert!(switch_client(&tmux, "api", Some("%5")).unwrap());

        let empty = tempfile::tempdir().unwrap();
        let (tmux, _) = switching(empty.path(), false, false);
        assert!(!switch_client(&tmux, "api", None).unwrap());
    }

    #[test]
    fn the_callers_session_comes_from_the_third_field_of_tmux() {
        assert_eq!(
            parse_session_id("/tmp/tmux-501/default,1234,3"),
            Some("$3".to_string())
        );
        assert_eq!(parse_session_id("/tmp/s,1234, 7 "), Some("$7".to_string()));
        assert_eq!(parse_session_id("/tmp/s,1234,"), None);
        assert_eq!(parse_session_id("/tmp/s,1234"), None);
        assert_eq!(parse_session_id(""), None);
    }
}
