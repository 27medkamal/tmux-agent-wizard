//! The lifecycle events `taw` fires at plugins, and the bridge the tmux hooks call.
//!
//! herdr owns its own UI, so it sees every workspace/tab/pane change from the inside and
//! can emit all of `third_party/herdr/reference/event-names.txt` itself. tmux owns the UI
//! here, and a user pressing `prefix c` or `prefix x` never goes through `taw` — so the
//! *tmux hook* is the only place those changes are observable. That splits the 19 events
//! into two sources, and **each event has exactly one** so nothing double-fires:
//!
//! | source | events |
//! |--------|--------|
//! | tmux hook (installed by `tmux-agent-wizard.tmux`, calling `taw plugin event`) | `workspace.created`, `tab.created`, `pane.created`, `tab.closed`, `tab.focused`, `tab.renamed`, `workspace.closed`, `workspace.focused`, `workspace.renamed`, `layout.updated`, `pane.exited`, `pane.focused`, `pane.closed` |
//! | the `taw` verb that causes it | `pane.updated`, `workspace.updated`, `worktree.created`, `worktree.opened`, `worktree.removed` |
//! | the pane watcher | `pane.agent_status_changed` |
//!
//! A hook-sourced event covers a `taw` verb too: `taw tab close` runs `kill-window`, which
//! fires `window-unlinked` in the same server, so the verb deliberately emits nothing.
//! The verb-sourced ones are the changes tmux fires no hook for at all: writing a pane or
//! session option (`pane.updated`, `workspace.updated`), or a git worktree, which is
//! taw's own concept and not a tmux event.
//!
//! The three "created" events are hook-sourced for the same reason as the rest: `prefix c`,
//! `prefix %` and a bare `tmux new-session` never pass through `taw`, so while the verbs
//! owned them a window, pane or session the user made with tmux itself was invisible to
//! every plugin. Their payloads are built here from exactly the records the verb's own
//! reply carries, so what a plugin receives is unchanged; and when one is built, it is left
//! on the object it is about for the verb to answer with ([`created_payload`]), so that the
//! two are one read of it.
//!
//! `pane.closed` is the awkward one, and it is hook-sourced by way of a *roster*: tmux
//! identifies no killed pane, so the id is recovered by diffing the live panes against the
//! previous set — see [`detect_closed`] for the evidence and the mechanism. That is what
//! makes `prefix x` emit it, which no verb ever could.
//!
//! Payload shapes are herdr's `EventData` variants
//! (`herdr-src/src/api/schema/events.rs`); [`crate::plugin::events`] turns a bare
//! `workspace_id`/`tab_id`/`pane_id` in one into the `HERDR_*_ID` a plugin hook expects.

use serde_json::{Value, json};

use super::common;
use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::plugin::context;
use crate::tmux::ids;

/// Every event name a plugin manifest's `[[events]] on` may reference — the whole of
/// `third_party/herdr/reference/event-names.txt`.
pub const NAMES: &[&str] = &[
    "layout.updated",
    "pane.agent_status_changed",
    "pane.closed",
    "pane.created",
    "pane.exited",
    "pane.focused",
    "pane.updated",
    "tab.closed",
    "tab.created",
    "tab.focused",
    "tab.renamed",
    "workspace.closed",
    "workspace.created",
    "workspace.focused",
    "workspace.renamed",
    "workspace.updated",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
];

/// The events `taw plugin event` builds a payload for: the ones whose whole content is
/// derivable from a tmux identifier, which is exactly the set the hooks fire. Every other
/// name in [`NAMES`] is emitted by the verb (or the watcher) that owns the change and has
/// the record to describe it — see the module docs.
const BRIDGED: &[(&str, &str)] = &[
    ("tab.closed", "the `window-unlinked` hook"),
    ("tab.focused", "the `session-window-changed` hook"),
    ("tab.renamed", "the `after-rename-window` hook"),
    ("workspace.closed", "the `session-closed` hook"),
    ("workspace.focused", "the `client-session-changed` hook"),
    ("workspace.renamed", "the `after-rename-session` hook"),
    ("layout.updated", "the `window-layout-changed` hook"),
    ("pane.exited", "the `pane-exited` hook"),
    ("pane.focused", "the `after-select-pane` hook"),
    (
        "pane.closed",
        "the `after-kill-pane` / `window-unlinked` roster diff",
    ),
    ("workspace.created", "the `session-created` hook"),
    ("tab.created", "the `window-linked` hook"),
    ("pane.created", "the `after-split-window` hook"),
];

/// Which `taw` verb (or component) owns each event `taw plugin event` refuses, so the
/// error can point at it instead of just saying no.
const OWNERS: &[(&str, &str)] = &[
    ("pane.updated", "taw pane rename / taw pane report-metadata"),
    ("workspace.updated", "taw workspace report-metadata"),
    ("worktree.created", "taw worktree create"),
    ("worktree.opened", "taw worktree open"),
    ("worktree.removed", "taw worktree remove"),
    ("pane.agent_status_changed", "the pane watcher"),
];

/// The identifiers a caller (in practice a tmux hook's `#{hook_*}`/`#{…_id}` formats) can
/// supply. Raw tmux ids (`$3`, `@7`, `%31`) and public ids (`w3`, `w3:t7`, `w3:p31`) are
/// both accepted; a hook always passes raw ones.
pub(crate) struct Ids<'a> {
    pub pane: Option<&'a str>,
    pub tab: Option<&'a str>,
    pub workspace: Option<&'a str>,
    pub label: Option<&'a str>,
}

/// The one event whose payload cannot be handed in, because tmux names no killed pane.
const PANE_CLOSED: &str = "pane.closed";

/// The event that means "the pane's process ended", which is not a close.
const PANE_EXITED: &str = "pane.exited";

pub(crate) const SUB: Sub = "event";

/// `taw plugin event <NAME> …` — fires one lifecycle event at every plugin whose manifest
/// hooks it, and reports how many hook commands launched.
///
/// Failures here are the caller's: an unknown event name, an event this bridge does not
/// own, or a missing identifier. Everything *after* that is best-effort — a plugin whose
/// hook cannot spawn is logged by [`crate::plugin::events::emit`] and never fails the
/// hook that fired, because a tmux hook has nowhere to report an error to.
pub(crate) fn emit(ctx: &Ctx, name: &str, ids: Ids<'_>) -> anyhow::Result<Value> {
    if !NAMES.contains(&name) {
        return Err(CliError::invalid_argument(format!(
            "unknown event '{name}'; see third_party/herdr/reference/event-names.txt"
        )));
    }
    if !BRIDGED.iter().any(|(n, _)| *n == name) {
        let owner = OWNERS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, o)| *o)
            .unwrap_or("another component");
        return Err(CliError::invalid_argument(format!(
            "'{name}' is emitted by {owner}, not by `taw plugin event`"
        )));
    }
    let plugin_ctx = crate::plugin::PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    // `window-layout-changed` fires on every resize and `after-select-pane` on every
    // focus change, so the hooks call this often. Ask the cheap question first: with no
    // plugin subscribed there is nothing to build a payload *for*, and a payload means
    // reading tmux. This is what makes the hooks affordable to leave installed.
    // A pane whose process ended is gone from the roster, but that is not a close. Its exit
    // takes the roster the way any detection does, which is what drops the pane from it, and
    // what that finds gone besides (a pane killed in the same moment) is a close like any other:
    // so this runs whether or not anyone hooks `pane.exited`, gated only on someone caring about
    // `pane.closed` at all. See `detect_closed`.
    let mut closed_by_the_exit = Vec::new();
    if name == PANE_EXITED && crate::plugin::events::any_hook(&plugin_ctx, PANE_CLOSED)? {
        match report_closed(ctx, &plugin_ctx) {
            Ok((closed, _)) => closed_by_the_exit = closed,
            Err(err) => log::warn!("could not take the pane roster after an exit: {err:#}"),
        }
    }

    if !crate::plugin::events::any_hook(&plugin_ctx, name)? {
        if name == PANE_CLOSED {
            // Forget the roster rather than keep it: see `forget_roster`.
            forget_roster(ctx);
        }
        return Ok(json!({ "event": name, "launched": 0, "subscribed": false }));
    }

    // `pane.closed` fires once per pane that has gone, which may be none (a hook fired for
    // a pane that is still there) or several (a killed window took its panes with it).
    if name == PANE_CLOSED {
        let (closed, launched) = report_closed(ctx, &plugin_ctx)?;
        return Ok(json!({
            "event": name,
            "launched": launched,
            "subscribed": true,
            "closed": closed,
        }));
    }

    let payload = payload(ctx, name, &ids)?;
    keep_created_payload(ctx, name, &ids, &payload);
    let launched = crate::plugin::events::emit(&plugin_ctx, name, payload.clone())?;
    let mut result = json!({
        "event": name,
        "launched": launched,
        "subscribed": true,
        "payload": payload,
    });
    if !closed_by_the_exit.is_empty() {
        result["closed"] = json!(closed_by_the_exit);
    }
    Ok(result)
}

/// The option a creation event's payload is left in, on the session, window or pane it is about, for
/// the verb that made the object to answer with ([`created_payload`]).
pub(crate) const CREATED_OPTION: &str = "@agent_wizard_created";

/// How long a verb that made a workspace, a tab or a pane waits for the payload its creation event
/// carries ([`created_payload`]): the hook that builds it is a process tmux starts as the object is
/// made, which a loaded machine starts slowly.
const CREATED_PAYLOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// What a creation event is about, and so where its payload is kept: a session, a window or a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Created {
    Workspace,
    Tab,
    Pane,
}

impl Created {
    /// The event, and the tmux hook taw's plugin script fires it from.
    fn event_and_hook(self) -> (&'static str, &'static str) {
        match self {
            Created::Workspace => ("workspace.created", "session-created"),
            Created::Tab => ("tab.created", "window-linked"),
            Created::Pane => ("pane.created", "after-split-window"),
        }
    }

    /// The `set-option`/`show-options` flag of the object's own options.
    fn scope(self) -> &'static [&'static str] {
        match self {
            Created::Workspace => &[],
            Created::Tab => &["-w"],
            Created::Pane => &["-p"],
        }
    }
}

/// Leaves `payload`, the records a creation event carries, on the object it is about
/// ([`CREATED_OPTION`]), for the verb that made it to answer with ([`created_payload`]). Best-effort: a
/// payload that cannot be left there costs the verb a read of its own.
fn keep_created_payload(ctx: &Ctx, name: &str, given: &Ids<'_>, payload: &Value) {
    let target = match name {
        "workspace.created" => given
            .workspace
            .map(|workspace| raw_session(ctx, workspace).map(|id| (Created::Workspace, id))),
        "tab.created" => given
            .tab
            .map(|tab| raw_window(ctx, tab).map(|id| (Created::Tab, id))),
        "pane.created" => given
            .pane
            .map(|pane| raw_pane(ctx, pane).map(|id| (Created::Pane, id))),
        _ => None,
    };
    let Some(Ok((created, target))) = target else {
        return;
    };
    let value = payload.to_string();
    let mut args = vec!["set-option"];
    args.extend(created.scope());
    args.extend(["-t", target.as_str(), CREATED_OPTION, value.as_str()]);
    if let Err(err) = ctx.tmux.run_ok(&args) {
        log::debug!("leaving the {name} payload on {target}: {err:#}");
    }
}

/// The payload the creation event of the object a verb has just made carries (`target`, of `created`),
/// for the verb to answer with: what the caller is told of the object and what a plugin was told are
/// then one read. Read twice, they differed: tmux's automatic-rename renames a new window as its shell
/// starts, and on a loaded machine the hook's read and the verb's fell either side of it. Waited for, up
/// to [`CREATED_PAYLOAD_WAIT`], only when a plugin or a `[hooks]` command listens for the event and taw's
/// plugin script has put its hook on this server, which is when the event is built; `None` otherwise, or
/// when it does not come, and the verb reads the object itself. The payload is taken off the object once
/// it is read.
pub(crate) fn created_payload(ctx: &Ctx, created: Created, target: &str) -> Option<Value> {
    let (event, hook) = created.event_and_hook();
    let plugin_ctx = crate::plugin::PluginCtx {
        tmux: &ctx.tmux,
        cfg: &ctx.cfg,
        paths: &ctx.paths,
        taw_bin: &ctx.taw_bin,
        server_env: None,
    };
    if !crate::plugin::events::any_hook(&plugin_ctx, event).unwrap_or(false) {
        return None;
    }
    let hooks = ctx.tmux.run(&["show-hooks", "-g", hook]).ok()?;
    if !hooks.contains(&format!("plugin event {event}")) {
        return None;
    }
    let mut read = vec!["show-options", "-qv"];
    read.extend(created.scope());
    read.extend(["-t", target, CREATED_OPTION]);
    let deadline = std::time::Instant::now() + CREATED_PAYLOAD_WAIT;
    loop {
        if let Ok(kept) = ctx.tmux.run(&read)
            && let Ok(payload) = serde_json::from_str::<Value>(kept.trim())
        {
            let mut unset = vec!["set-option", "-u"];
            unset.extend(created.scope());
            unset.extend(["-t", target, CREATED_OPTION]);
            let _ = ctx.tmux.run_ok(&unset);
            return Some(payload);
        }
        if std::time::Instant::now() >= deadline {
            log::warn!(
                "the {event} payload for {target} did not come within {CREATED_PAYLOAD_WAIT:?}; \
                 reading it here"
            );
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Takes the roster ([`detect_closed`]) and fires `pane.closed` at the plugins for every pane it
/// reports: the ids reported, and how many hook commands launched.
fn report_closed(
    ctx: &Ctx,
    plugin_ctx: &crate::plugin::PluginCtx<'_>,
) -> anyhow::Result<(Vec<String>, usize)> {
    let closed = detect_closed(ctx)?;
    let mut launched = 0usize;
    for public in &closed {
        launched += crate::plugin::events::emit(plugin_ctx, PANE_CLOSED, closed_payload(public))?;
    }
    Ok((closed, launched))
}

/// A required identifier, or `invalid_argument` naming the flag that is missing.
fn need<'a>(value: Option<&'a str>, flag: &str, name: &str) -> anyhow::Result<&'a str> {
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CliError::invalid_argument(format!("'{name}' needs --{flag}")))
}

/// The herdr `EventData` payload for one bridged event.
///
/// The three "gone" events (`tab.closed`, `workspace.closed`, `pane.exited`) are built
/// from the raw ids alone, with **no tmux lookup**: by the time a hook runs, the window,
/// session or pane it names may already be destroyed, and a lookup would either fail or —
/// worse — resolve to whatever tmux reuses the target string for. Public ids are a pure
/// function of the raw ones ([`ids::tab_id`] and friends), so this needs nothing live.
fn payload(ctx: &Ctx, name: &str, given: &Ids<'_>) -> anyhow::Result<Value> {
    match name {
        "tab.closed" | "tab.focused" | "tab.renamed" => {
            let session = raw_session(ctx, need(given.workspace, "workspace", name)?)?;
            let window = raw_window(ctx, need(given.tab, "tab", name)?)?;
            let mut out = json!({
                "tab_id": ids::tab_id(&session, &window),
                "workspace_id": ids::workspace_id(&session),
            });
            if name == "tab.renamed" {
                let label = match given.label {
                    Some(label) => label.to_string(),
                    // Only reachable when a person runs the verb by hand: the hook always
                    // passes `#{window_name}`.
                    None => common::window_info(&ctx.tmux, &window)?.window_name,
                };
                out["label"] = Value::String(label);
            }
            Ok(out)
        }
        "workspace.closed" | "workspace.focused" | "workspace.renamed" => {
            let session = raw_session(ctx, need(given.workspace, "workspace", name)?)?;
            let mut out = json!({ "workspace_id": ids::workspace_id(&session) });
            if name == "workspace.renamed" {
                let label = match given.label {
                    Some(label) => label.to_string(),
                    None => common::session_info(&ctx.tmux, &session)?.session_name,
                };
                out["label"] = Value::String(label);
            }
            Ok(out)
        }
        "workspace.created" => {
            let session = raw_session(ctx, need(given.workspace, "workspace", name)?)?;
            let panes = common::list_panes(&ctx.tmux)?;
            let info = common::session_info(&ctx.tmux, &session)?;
            // The session's first tab and root pane ride along, so a plugin sees the whole
            // new workspace from one event — exactly what `taw workspace create` sent.
            let window = common::list_windows(&ctx.tmux)?
                .into_iter()
                .find(|w| w.session_id == session);
            let root = panes.iter().find(|p| p.session_id == session);
            Ok(json!({
                "workspace": context::workspace_json(&info, &panes),
                "tab": window.as_ref().map(|w| context::tab_json(w, &panes)),
                "pane": root.map(context::pane_json),
            }))
        }
        "tab.created" => {
            let session = raw_session(ctx, need(given.workspace, "workspace", name)?)?;
            let window = raw_window(ctx, need(given.tab, "tab", name)?)?;
            let panes = common::list_panes(&ctx.tmux)?;
            let info = common::list_windows(&ctx.tmux)?
                .into_iter()
                .find(|w| w.window_id == window && w.session_id == session)
                .ok_or_else(|| CliError::not_found(format!("tab '{window}' not found")))?;
            let root = panes.iter().find(|p| p.window_id == window);
            Ok(json!({
                "tab": context::tab_json(&info, &panes),
                "pane": root.map(context::pane_json),
            }))
        }
        "pane.created" => {
            let pane = raw_pane(ctx, need(given.pane, "pane", name)?)?;
            let info = common::pane_info(&ctx.tmux, &pane)?;
            Ok(json!({ "pane": context::pane_json(&info) }))
        }
        // `pane.closed` never reaches here: `emit` handles it through the roster, since a
        // killed pane has no identity to be handed in.
        "pane.exited" | "pane.focused" => {
            let pane = raw_pane(ctx, need(given.pane, "pane", name)?)?;
            // The pane's own session where it can still be read (a focus change, or a
            // pane kept by `remain-on-exit`), else the one the hook reported — the exited
            // pane's window, which is what tmux gives a `pane-exited` hook its context
            // from.
            let session = match common::pane_info(&ctx.tmux, &pane) {
                Ok(info) => info.session_id,
                Err(_) => raw_session(ctx, need(given.workspace, "workspace", name)?)?,
            };
            Ok(json!({
                "pane_id": ids::pane_id(&session, &pane),
                "workspace_id": ids::workspace_id(&session),
            }))
        }
        "layout.updated" => {
            let target = given
                .tab
                .filter(|t| !t.is_empty())
                .or(given.pane.filter(|p| !p.is_empty()));
            let window = super::layout::window_of(ctx, target)?;
            Ok(json!({ "layout": super::layout::snapshot(ctx, &window)? }))
        }
        other => Err(CliError::invalid_argument(format!(
            "'{other}' has no payload builder"
        ))),
    }
}

/// A raw tmux session id (`$N`) for a workspace identifier.
///
/// A raw or public id is turned into `$N` arithmetically — no tmux call, so a *closed*
/// session's id still yields the right `workspace_id`. Anything else (a session name) has
/// to be resolved by asking tmux.
fn raw_session(ctx: &Ctx, target: &str) -> anyhow::Result<String> {
    match ids::parse(target) {
        Some(ids::Target::Workspace(id)) => Ok(id),
        _ => common::resolve_session(&ctx.tmux, target),
    }
}

fn raw_window(ctx: &Ctx, target: &str) -> anyhow::Result<String> {
    match ids::parse(target) {
        Some(ids::Target::Tab(id)) => Ok(id),
        _ => common::resolve_window(&ctx.tmux, target),
    }
}

fn raw_pane(ctx: &Ctx, target: &str) -> anyhow::Result<String> {
    match ids::parse(target) {
        Some(ids::Target::Pane(id)) => Ok(id),
        _ => common::resolve_pane(&ctx.tmux, target),
    }
}

// ---------------------------------------------------------------------------
// pane.closed — reconstructed by diffing a roster
// ---------------------------------------------------------------------------

/// The global option holding the public pane ids that were live the last time the roster
/// was taken. Space-separated; a public pane id (`w3:p31`) never contains a space.
pub const PANE_ROSTER_OPTION: &str = "@agent_wizard_panes";

/// The global option holding the roster's generation: how many times the roster has been
/// written, as text, empty before the first. A write lands only while the generation is the one the
/// writer read before it read the live panes ([`replace_roster`]).
pub const PANE_ROSTER_GENERATION_OPTION: &str = "@agent_wizard_panes_generation";

/// The global option holding the raw ids (`%12`) of the panes whose process has ended, each
/// followed by a space. The `pane-exited` hook adds a pane to it with its own commands, ahead of the
/// job that tells plugins about the exit, so that every detection knows of the exit before it can
/// report anything ([`detect_closed`]). Whoever drops a pane from the roster takes its mark off.
pub const PANE_EXITED_OPTION: &str = "@agent_wizard_exited";

/// How many times a roster write that finds the roster changed under it reads the roster and the
/// live panes again and tries again. Every one of them is lost to another write that landed, so
/// this is as many writers as there are hooks for one change racing each other, which is a few.
const ROSTER_ATTEMPTS: usize = 20;

/// Which panes have gone since the roster was last taken, rewriting the roster.
///
/// **This exists because tmux identifies no killed pane.** Verified on tmux 3.6b: the only
/// hook that fires for `kill-pane` is `after-kill-pane`, and *every* `hook_*` variable is
/// empty there — `hook_pane` included — while the surrounding format context is whichever
/// pane happens to be current, which is sometimes the killed one and sometimes not. There
/// is no `pane-removed`/`pane-closed`/`pane-killed` hook at all (the full hook list was
/// enumerated). `pane-exited` *does* carry `#{hook_pane}`, but it fires only when a pane's
/// process ends, which is a different herdr event
/// (`herdr-src/src/app/api.rs` emits `PaneExited` from `AppEvent::PaneDied`, while
/// `PaneClosed` comes from the explicit close path in `api/panes.rs`).
///
/// So the pane's identity is recovered by comparing the live set against the previous one.
/// A public id carries its workspace, so a vanished pane still yields the whole payload
/// with no lookup — which matters, since by definition the pane is gone.
///
/// An **unset** roster seeds and reports nothing: the alternative is announcing every pane
/// that closed before anyone was listening.
///
/// The roster is replaced by [`replace_roster`], and the panes reported are the ones that write
/// took out of it: a pane is reported by the one detection whose write removed it, once, however
/// many hooks fire for one close and however slowly any of them runs.
///
/// **A pane whose process ended is gone from the live panes too, and is not a close.** The roster
/// itself cannot tell the two apart, so the exit is marked in [`PANE_EXITED_OPTION`] by the
/// `pane-exited` hook's own commands, and a detection leaves a marked pane out of what it reports.
/// The marks are read *after* the live panes, since that is the order in which they are
/// dependable: tmux runs the commands of a notify hook (`pane-exited`, `window-unlinked`) from the
/// server's own queue, in the loop pass that sees the event, before any client's queued command
/// (`server_loop` runs `cmdq_next(NULL)` and then each client's queue, in tmux 3.2 and 3.6b alike),
/// so a command that finds a pane gone finds its mark set. Marking in the job the hook starts (a
/// `run-shell -b` process, as the roster refresh was) left the exit open to a detection that lists
/// the panes and writes the roster before that job has begun: it reported the pane closed, for the
/// last pane of a window (`window-unlinked` fires beside `pane-exited`) three times in ten, and
/// whenever a kill landed with an exit. The exit's own take of the roster is a detection, and
/// reports what it finds gone besides the pane that exited (the kill that landed with it).
///
/// The one thing this cannot catch: a pane created and killed inside the hook latency. A
/// hook command runs detached (`run-shell -b`), so the roster is refreshed a few
/// milliseconds after the pane appears, and a pane that does not survive that window was
/// never in the set to be missed from. Passing the live set in from the hook instead was
/// tried and abandoned — the only tmux format that enumerates panes is
/// `#{S:#{W:#{P:…}}}`, whose loop modifiers need a current client, and on a detached
/// server tmux answers "no current client" and defers that error onto the next unrelated
/// command (it surfaced as a `send-keys` failure three commands later). A pane that lives
/// for milliseconds is a far smaller loss than an error on an unrelated command.
fn detect_closed(ctx: &Ctx) -> anyhow::Result<Vec<String>> {
    let replaced = replace_roster(ctx)?;
    let Some(previous) = replaced.previous.filter(|v| !v.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    Ok(without_exited(
        closed_between(&previous, &replaced.live),
        &replaced.exited,
    ))
}

/// What the roster option and its generation hold.
struct Stored {
    /// The pane ids, space-separated; `None` when the option is unset.
    roster: Option<String>,
    /// The generation as it is stored: a count, or empty when the roster has not been written.
    generation: String,
}

/// Reads the roster and its generation in one tmux call, so that the two belong together.
fn stored_roster(ctx: &Ctx) -> anyhow::Result<Stored> {
    let listing = ctx
        .tmux
        .run(&["show-options", "-g"])
        .map_err(|e| CliError::tmux(format!("reading {PANE_ROSTER_OPTION}: {e:#}")))?;
    let mut stored = Stored {
        roster: None,
        generation: String::new(),
    };
    for (name, value) in crate::tmux::parse_option_lines(&listing) {
        if name == PANE_ROSTER_OPTION {
            stored.roster = Some(value);
        } else if name == PANE_ROSTER_GENERATION_OPTION {
            stored.generation = value;
        }
    }
    Ok(stored)
}

/// The tmux command that writes `live` as the roster, and the generation after `generation`, only
/// while the generation is still `generation`, takes the marks of the panes in `gone` (raw ids of
/// panes that exited and are not live) off [`PANE_EXITED_OPTION`], and prints `won` or `lost` to say
/// which.
///
/// It is one invocation, and tmux runs the commands of one invocation without any other client's
/// commands between them (the condition is a format, expanded at once, and runs no process that the
/// queue would wait for), so the check and the write cannot be pulled apart. The same is done
/// for an agent's state (`store::write_state_over`). A mark is taken off by substituting it out of
/// the option as it is when the command runs, and not by writing the text that was read, so that a
/// mark added since (another pane's exit) is not lost.
fn roster_write_arguments(generation: &str, live: &[String], gone: &[String]) -> Vec<String> {
    let next = generation.parse::<u64>().map_or(1, |n| n.saturating_add(1));
    let condition = format!(
        "#{{==:#{{{PANE_ROSTER_GENERATION_OPTION}}},{}}}",
        crate::store::format_literal(generation)
    );
    // A public pane id is `w<n>:p<n>`, which a tmux command takes in single quotes as it is.
    let roster = live.join(" ").replace('\'', "");
    let mut write = format!(
        "set-option -g {PANE_ROSTER_OPTION} '{roster}' ; \
         set-option -g {PANE_ROSTER_GENERATION_OPTION} {next}"
    );
    // A raw pane id is `%` and digits, which a tmux command and a format take as they are, and a
    // regular expression matches as itself.
    for mark in gone.iter().filter(|mark| is_raw_pane_id(mark)) {
        write.push_str(&format!(
            " ; set-option -gF {PANE_EXITED_OPTION} '#{{s/{mark} //:{PANE_EXITED_OPTION}}}'"
        ));
    }
    write.push_str(" ; display-message -p won");
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        condition,
        write,
        "display-message -p lost".to_string(),
    ]
}

/// What a write of the roster found, and did.
struct Replaced {
    /// The roster as it was, `None` when the option was unset.
    previous: Option<String>,
    /// The panes that were live, as public ids.
    live: Vec<String>,
    /// The raw ids of the panes marked as having exited, read after the live panes.
    exited: Vec<String>,
}

/// Replaces the roster with the live panes, and says what it held, what was live, and which panes
/// had exited.
///
/// A roster write must never put back a pane that an exit already took out, and never be older than
/// the roster already written. A detection reads the live panes some time before it writes, and
/// another hook's write can land in that time (the exit of a pane refreshes the roster without it,
/// and a write that read the panes before the exit then lists it once more, so that a later
/// detection reports a pane that exited as closed). So a write is a compare-and-set on a generation,
/// which is a count of the writes, kept in its own option and written with the roster in one tmux
/// invocation: the writer reads the roster and the generation, **then** the live panes, and its write
/// lands only if the generation is still what it read. If another write landed in between, the
/// panes it read may be older than that write, and it reads again. The marks of exits are read last,
/// after the live panes ([`detect_closed`] says why), and the write takes off the ones whose pane
/// is no longer live.
///
/// The generation, and not the roster's own text, is what is compared, since a roster can come back
/// to a text it had (a pane made and ended between two writes) while a count never does. It is a
/// count, not a lock, because a hook that is killed holding a lock would hold it for ever
/// (`wait-for -L` is never released by a client that goes), and not a time, because a time says when
/// the panes were read and not whether anything was written since, and tmux compares text, not
/// numbers, across the versions this runs on.
fn replace_roster(ctx: &Ctx) -> anyhow::Result<Replaced> {
    for _ in 0..ROSTER_ATTEMPTS {
        let stored = stored_roster(ctx)?;
        let (live, exited) = live_panes_and_exits(ctx)?;
        let gone = gone_marks(&exited, &live);
        let args = roster_write_arguments(&stored.generation, &live, &gone);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let answer = ctx
            .tmux
            .run(&args)
            .map_err(|e| CliError::tmux(format!("recording {PANE_ROSTER_OPTION}: {e:#}")))?;
        if answer.trim() == "won" {
            return Ok(Replaced {
                previous: stored.roster,
                live,
                exited,
            });
        }
    }
    Err(CliError::tmux(format!(
        "{PANE_ROSTER_OPTION} changed under {ROSTER_ATTEMPTS} attempts to write it"
    )))
}

/// Every pane alive on the server, as public ids, and the raw ids of the panes marked as having
/// exited, as the `pane-exited` hook left them: read in one tmux invocation, the marks right after
/// the panes. A word of the marks that is not a raw pane id is not a mark, since the option is one
/// anybody can set and what is in it ends up in a tmux command.
///
/// The panes are read with `list-panes -a` rather than expanded from a tmux format in the hook. The
/// format that would enumerate panes is `#{S:#{W:#{P:…}}}`, and those loop modifiers
/// need a **current client**: on a server with none attached — every one of these tests,
/// and any detached session — expanding one in a hook produces "no current client", and
/// tmux defers that error onto the *next* unrelated command. It surfaced as a
/// `send-keys` failure three commands later, which is a far worse bug than the staleness
/// it was meant to avoid. `list-panes -a` needs no client.
fn live_panes_and_exits(ctx: &Ctx) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let (panes, marks) =
        common::list_panes_then(&ctx.tmux, &["show-options", "-gqv", PANE_EXITED_OPTION]).map_err(
            |e| {
                CliError::tmux(format!(
                    "listing the panes and reading {PANE_EXITED_OPTION}: {e:#}"
                ))
            },
        )?;
    let live = panes
        .iter()
        .map(|p| ids::pane_id(&p.session_id, &p.pane_id))
        .collect();
    let mut exited: Vec<String> = marks
        .split_whitespace()
        .filter(|mark| is_raw_pane_id(mark))
        .map(str::to_string)
        .collect();
    exited.dedup();
    Ok((live, exited))
}

/// Whether `word` is a raw tmux pane id: `%` and digits.
fn is_raw_pane_id(word: &str) -> bool {
    word.strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// The raw id (`%31`) of the pane a public id (`w3:p31`) names.
fn raw_pane_of(public: &str) -> Option<String> {
    let (_, number) = public.rsplit_once(":p")?;
    let raw = format!("%{number}");
    is_raw_pane_id(&raw).then_some(raw)
}

/// The marks in `exited` whose pane is not among the `live` public ids: the ones a write of the
/// roster has no more use for.
fn gone_marks(exited: &[String], live: &[String]) -> Vec<String> {
    let alive: Vec<String> = live.iter().filter_map(|id| raw_pane_of(id)).collect();
    exited
        .iter()
        .filter(|mark| !alive.contains(mark))
        .cloned()
        .collect()
}

/// The ids in `closed` whose pane did not exit: a pane whose process ended is a `pane.exited`, and
/// never a `pane.closed`.
fn without_exited(closed: Vec<String>, exited: &[String]) -> Vec<String> {
    closed
        .into_iter()
        .filter(|public| !raw_pane_of(public).is_some_and(|raw| exited.contains(&raw)))
        .collect()
}

/// The ids in `previous` (a roster string) that are no longer in `live`, in the roster's
/// own order. Pure, so the diff is testable without a server.
fn closed_between(previous: &str, live: &[String]) -> Vec<String> {
    previous
        .split_whitespace()
        .filter(|id| !live.iter().any(|current| current == id))
        .map(str::to_string)
        .collect()
}

/// Forgets the roster, so the next detection seeds instead of reporting a backlog.
///
/// Called when no plugin hooks `pane.closed`: without it, a plugin installed after a
/// day's work would be handed every pane that had closed in the meantime, all at once.
fn forget_roster(ctx: &Ctx) {
    // All in one call: a roster without its generation, or the other way round, is no state, and
    // the marks of exits are for the roster there is.
    if let Err(err) = ctx.tmux.run_ok(&[
        "set-option",
        "-gu",
        PANE_ROSTER_OPTION,
        ";",
        "set-option",
        "-gu",
        PANE_ROSTER_GENERATION_OPTION,
        ";",
        "set-option",
        "-gu",
        PANE_EXITED_OPTION,
    ]) {
        log::debug!("could not clear {PANE_ROSTER_OPTION}: {err:#}");
    }
}

/// `pane.closed`'s payload for one vanished public pane id. The workspace is the part of
/// the id before the `:`, so a gone pane needs no lookup.
fn closed_payload(public: &str) -> Value {
    let workspace = public.split_once(':').map_or(public, |(ws, _)| ws);
    json!({ "pane_id": public, "workspace_id": workspace })
}

// ---------------------------------------------------------------------------
// The verb-sourced payloads, built where the verb already holds the record
// ---------------------------------------------------------------------------

/// `pane.updated` — `{pane}`, the whole record after the change.
pub(crate) fn pane_updated(ctx: &Ctx, pane: &crate::tmux::PaneInfo) {
    common::emit_event(
        ctx,
        "pane.updated",
        json!({ "pane": context::pane_json(pane) }),
    );
}

/// `workspace.updated` — `{workspace}`, the whole record after the change.
pub(crate) fn workspace_updated(ctx: &Ctx, session_id: &str) {
    let record = common::session_info(&ctx.tmux, session_id)
        .and_then(|session| {
            let panes = common::list_panes(&ctx.tmux)?;
            Ok(context::workspace_json(&session, &panes))
        })
        .ok();
    // No record means the session went away between the write and this read; the event
    // still carries the id, which is all a plugin needs to re-read whatever it tracks.
    let payload = match record {
        Some(workspace) => json!({ "workspace": workspace }),
        None => json!({ "workspace_id": ids::workspace_id(session_id) }),
    };
    common::emit_event(ctx, "workspace.updated", payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vendored_event_name_is_listed() {
        let vendored = include_str!("../../third_party/herdr/reference/event-names.txt");
        let mut expected: Vec<&str> = vendored
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        expected.sort_unstable();
        let mut have = NAMES.to_vec();
        have.sort_unstable();
        assert_eq!(
            have, expected,
            "NAMES is out of sync with third_party/herdr/reference/event-names.txt"
        );
    }

    #[test]
    fn every_event_has_exactly_one_source() {
        // The whole point of splitting hook-sourced from verb-sourced: an event in both
        // lists would fire twice for one change, and an event in neither would never fire.
        for name in NAMES {
            let bridged = BRIDGED.iter().any(|(n, _)| n == name);
            let owned = OWNERS.iter().any(|(n, _)| n == name);
            assert!(
                bridged ^ owned,
                "'{name}' must be either hook-sourced or verb-sourced, exactly one \
                 (bridged: {bridged}, owned: {owned})"
            );
        }
    }

    #[test]
    fn the_roster_diff_reports_only_the_ids_that_have_gone() {
        let live = ["w0:p0".to_string(), "w0:p2".to_string()];
        assert_eq!(
            closed_between("w0:p0 w0:p1 w0:p2", &live),
            vec!["w0:p1".to_string()]
        );
        // Nothing gone.
        assert!(closed_between("w0:p0 w0:p2", &live).is_empty());
        // A whole window's worth at once, in the roster's own order.
        assert_eq!(
            closed_between("w0:p0 w1:p5 w1:p6 w0:p2", &live),
            vec!["w1:p5".to_string(), "w1:p6".to_string()]
        );
        // A pane created since the last diff is simply not in the roster, and its arrival
        // is not a close.
        assert!(closed_between("w0:p0", &live).is_empty());
        // An empty roster reports nothing, whatever is live.
        assert!(closed_between("", &live).is_empty());
        assert!(closed_between("   ", &live).is_empty());
        // Everything gone, including when nothing is live at all.
        assert_eq!(
            closed_between("w0:p0 w0:p2", &[]),
            vec!["w0:p0".to_string(), "w0:p2".to_string()]
        );
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_roster_write_is_one_compare_and_set_on_the_generation_it_read() {
        let args = roster_write_arguments("7", &ids(&["w0:p0", "w0:p2"]), &[]);
        assert_eq!(
            args,
            [
                "if-shell",
                "-F",
                "#{==:#{@agent_wizard_panes_generation},7}",
                "set-option -g @agent_wizard_panes 'w0:p0 w0:p2' ; set-option -g @agent_wizard_panes_generation 8 ; display-message -p won",
                "display-message -p lost",
            ]
        );
    }

    #[test]
    fn the_first_write_compares_with_no_generation_and_makes_the_first() {
        let args = roster_write_arguments("", &ids(&["w0:p0"]), &[]);
        assert_eq!(args[2], "#{==:#{@agent_wizard_panes_generation},}");
        assert!(
            args[3].contains("@agent_wizard_panes_generation 1 ;"),
            "{}",
            args[3]
        );
    }

    #[test]
    fn a_generation_that_is_not_a_count_is_compared_as_it_is_and_followed_by_the_first() {
        // The option is one a person could set: it is compared as typed, with what a format gives
        // a meaning to escaped, and the next one is the first.
        let args = roster_write_arguments("a,b}#", &ids(&["w0:p0"]), &[]);
        assert_eq!(args[2], "#{==:#{@agent_wizard_panes_generation},a#,b#}##}");
        assert!(args[3].contains("_generation 1 ;"), "{}", args[3]);
        // A count at the top of its range does not wrap round to one a writer could have read.
        let args = roster_write_arguments(&u64::MAX.to_string(), &ids(&["w0:p0"]), &[]);
        assert!(
            args[3].contains(&format!("_generation {} ;", u64::MAX)),
            "{}",
            args[3]
        );
    }

    #[test]
    fn a_roster_of_no_panes_is_written_as_an_empty_string() {
        let args = roster_write_arguments("3", &[], &[]);
        assert!(
            args[3].starts_with("set-option -g @agent_wizard_panes '' ;"),
            "{}",
            args[3]
        );
    }

    #[test]
    fn only_a_percent_sign_and_digits_is_a_raw_pane_id() {
        for id in ["%0", "%31", "%1234"] {
            assert!(is_raw_pane_id(id), "{id}");
        }
        // The option the marks are kept in is one anybody can set, and a word from it ends up in
        // a tmux command: nothing but a raw pane id may.
        for word in [
            "",
            "%",
            "31",
            "%3x",
            "%-1",
            "w0:p1",
            "%1;kill-server",
            "%1'",
            "%1}",
            "%1 ",
            "%1\n",
        ] {
            assert!(!is_raw_pane_id(word), "{word:?}");
        }
    }

    #[test]
    fn a_public_pane_id_names_the_raw_pane_it_is_made_from() {
        assert_eq!(raw_pane_of("w3:p31").as_deref(), Some("%31"));
        assert_eq!(raw_pane_of("w0:p0").as_deref(), Some("%0"));
        for id in ["w3", "w3:p", "w3:pxx", "w3:t7", "p31", ""] {
            assert_eq!(raw_pane_of(id), None, "{id:?}");
        }
    }

    #[test]
    fn a_pane_that_exited_is_left_out_of_what_is_reported_closed() {
        let closed = ids(&["w0:p1", "w0:p2", "w1:p5"]);
        let exited = ids(&["%2"]);
        assert_eq!(
            without_exited(closed.clone(), &exited),
            ids(&["w0:p1", "w1:p5"])
        );
        // No marks, nothing left out; and a mark for a pane that is not among them is nothing.
        assert_eq!(without_exited(closed.clone(), &[]), closed);
        assert_eq!(without_exited(closed.clone(), &ids(&["%9"])), closed);
        // Every pane of a window that exited.
        assert!(without_exited(closed, &ids(&["%1", "%2", "%5"])).is_empty());
    }

    #[test]
    fn the_marks_a_write_has_no_more_use_for_are_those_of_panes_that_are_not_live() {
        let exited = ids(&["%1", "%2", "%3"]);
        assert_eq!(
            gone_marks(&exited, &ids(&["w0:p0", "w0:p2"])),
            ids(&["%1", "%3"])
        );
        assert!(gone_marks(&exited, &ids(&["w0:p1", "w0:p2", "w4:p3"])).is_empty());
        assert!(gone_marks(&[], &ids(&["w0:p0"])).is_empty());
    }

    #[test]
    fn a_write_takes_off_the_marks_of_the_panes_it_drops_by_substituting_them_out() {
        let args = roster_write_arguments("7", &ids(&["w0:p0"]), &ids(&["%1", "%12"]));
        assert_eq!(
            args[3],
            "set-option -g @agent_wizard_panes 'w0:p0' ; \
             set-option -g @agent_wizard_panes_generation 8 ; \
             set-option -gF @agent_wizard_exited '#{s/%1 //:@agent_wizard_exited}' ; \
             set-option -gF @agent_wizard_exited '#{s/%12 //:@agent_wizard_exited}' ; \
             display-message -p won"
        );
        // Still one compare-and-set on the generation, the marks only in the branch that wins.
        assert_eq!(args[2], "#{==:#{@agent_wizard_panes_generation},7}");
        assert_eq!(args[4], "display-message -p lost");
    }

    #[test]
    fn a_word_that_is_not_a_raw_pane_id_is_never_put_in_the_command() {
        let args = roster_write_arguments(
            "1",
            &ids(&["w0:p0"]),
            &ids(&["%1;kill-server", "'; kill-server; '", "%2"]),
        );
        assert!(!args[3].contains("kill-server"), "{}", args[3]);
        assert!(
            args[3].contains("#{s/%2 //:@agent_wizard_exited}"),
            "{}",
            args[3]
        );
    }

    #[test]
    fn a_closed_panes_workspace_comes_from_its_own_public_id() {
        // The pane is gone, so nothing can be looked up — but a public id carries its
        // workspace, which is the whole reason the roster stores public ids.
        assert_eq!(
            closed_payload("w3:p31"),
            serde_json::json!({"pane_id": "w3:p31", "workspace_id": "w3"})
        );
        assert_eq!(
            closed_payload("w0:p0"),
            serde_json::json!({"pane_id": "w0:p0", "workspace_id": "w0"})
        );
        // Defensive: something with no `:` at all still yields a whole payload rather
        // than a panic, since the roster is a tmux option a person could edit.
        assert_eq!(
            closed_payload("nonsense"),
            serde_json::json!({"pane_id": "nonsense", "workspace_id": "nonsense"})
        );
    }

    #[test]
    fn need_names_the_missing_flag_as_an_invalid_argument() {
        let err = need(None, "workspace", "tab.closed").unwrap_err();
        assert_eq!(super::super::envelope::code_for(&err), "invalid_argument");
        assert!(err.to_string().contains("--workspace"), "{err}");
        // An empty string is what a tmux format expands to when the variable is unset, so
        // it has to be treated as absent rather than passed through as an id.
        assert!(need(Some(""), "tab", "tab.closed").is_err());
        assert_eq!(need(Some("$1"), "workspace", "x").unwrap(), "$1");
    }
}
