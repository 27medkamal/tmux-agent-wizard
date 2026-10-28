//! Turns agent state transitions into herdr-style notifications (toast + sound), and goes
//! to the next agent for `taw notification open-target` / `prefix+O`. Where `prefix+O` goes
//! is "next up", the popup's own walk.

pub mod sound;
pub mod system;
pub mod terminal;
pub mod tmux_message;

use crate::attention::{self, Group};
use crate::config::{Config, ToastDelivery};
use crate::paths::Paths;
use crate::tmux::Tmux;

/// Which edge of an agent's lifecycle a [`Notification`] reports: `Request` — the
/// agent just transitioned *into* `blocked` (it wants the user's attention); `Done` —
/// it completed, `idle`/`done` after `working` or `blocked`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Request,
    Done,
}

/// Which sound (if any) should accompany a delivered [`Notification`]; see
/// `sound::play`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundChoice {
    Request,
    Done,
    None,
}

/// One notification ready to deliver: the plain-text title/body that `terminal`
/// (OSC) and `system` (native) delivery show as-is, the pane it is about (the `herdr`
/// toast and the sound read the agent and its location from it), which sound to play,
/// and the agent it is about.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub class: Class,
    pub title: String,
    pub body: String,
    /// tmux pane id (`%N`), when this notification is about one specific pane.
    pub pane_id: Option<String>,
    pub sound: SoundChoice,
    /// The agent as it was when the notification was decided; empty for one about no
    /// agent (`taw notification show`).
    pub agent: NotifiedAgent,
}

/// Who a notification is about, as they were when it was decided: the name the user
/// knows the agent by (`@agent_display` -> `@agent_name` -> `@agent_kind`) and its kind.
/// Delivery reads both from the pane when it fires, so a rename in between is honoured,
/// and falls back to these once the pane no longer holds an agent: one that exited
/// mid-turn is toasted after its record is cleared, as herdr toasts it with the label
/// it had.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifiedAgent {
    pub label: String,
    pub kind: String,
}

/// tmux option holding the key bound to "next up" (see the plugin script), and its
/// documented default — read live by [`deliver`] so a rebind is always honoured, never
/// baked into a `Notification` at construction time.
const NOTIFICATION_KEY_OPTION: &str = "@agent-wizard-notification-key";
const DEFAULT_NOTIFICATION_KEY: &str = "O";

/// A pane's format for whether the terminals showing its session have focus, for
/// [`terminal_focus`] to read: `<tmux version>:<marks>`, one mark per client attached to
/// the pane's session, `1` while tmux believes its terminal has focus and `0` once the
/// terminal has reported losing it. Empty while `focus-events` is off — tmux then asks no
/// terminal to report its focus — and for a pane that does not exist.
///
/// Only the client loop (`#{L:…}`) reaches the flags of the clients on the pane's session:
/// a bare `#{client_flags}` describes whichever client tmux picks, attached there or not.
/// The loop also visits the client running the command, which has no session, so the
/// comparison with `#{session_name}` leaves it out. The session is tested by id, not by
/// name: `#{?…}` reads a session named `0` as false. Verified on tmux 3.6b.
macro_rules! terminal_focus_format {
    () => {
        "#{?#{&&:#{focus-events},#{session_id}},#{version}:#{L:#{?#{==:#{client_session},#{session_name}},#{?#{m:*focused*,#{client_flags}},1,0},}},}"
    };
}
pub(crate) use terminal_focus_format;

/// herdr's `outer_terminal_focus`, from a `terminal_focus_format!` expansion: `Some(true)`
/// when a terminal attached to the pane's session has focus, `Some(false)` when every one of
/// them has reported losing it, and `None` when tmux cannot say — `focus-events` off,
/// nobody attached, or a tmux before 3.6 (this was verified on 3.6b, and on a tmux whose
/// client flags lacked `focused` every terminal would read as unfocused). herdr treats
/// `None` as focused, and so does [`in_view`].
pub fn terminal_focus(expansion: &str) -> Option<bool> {
    let (version, marks) = expansion.split_once(':')?;
    let version = crate::tmux::version::parse(&format!("tmux {version}"))?;
    if !version.at_least(3, 6)
        || marks.is_empty()
        || !marks.bytes().all(|mark| mark == b'0' || mark == b'1')
    {
        return None;
    }
    Some(marks.contains('1'))
}

/// Whether a user could be looking at a pane: its window is the active window of a session
/// with at least one attached client, and the terminal showing it is not known to be in the
/// background (herdr's `active_tab_suppresses_notifications`: the active tab, with the outer
/// terminal not reported unfocused). The one predicate behind [`is_background`] and the
/// watcher's completion rule.
pub fn in_view(window_active: bool, session_attached: u32, terminal_focus: Option<bool>) -> bool {
    window_active && session_attached > 0 && terminal_focus != Some(false)
}

/// Whether `pane` is in the background: not [`in_view`]. This is herdr's "only notify for
/// background work" gate. One query: `#{window_active}`, `#{session_attached}` and the
/// focus of the terminals attached to its session (`terminal_focus_format!`).
///
/// A **detached** session (`session_attached == 0`) is therefore always background,
/// even for what would otherwise be its one active pane — nobody is watching a
/// detached session, so every pane in it counts as background for notification
/// purposes. So is the active window of an attached one while its client's terminal has
/// reported losing focus: the user is in another app.
pub fn is_background(t: &Tmux, pane: &str) -> anyhow::Result<bool> {
    let out = t.display(
        pane,
        concat!(
            "#{window_active}\t#{session_attached}\t",
            terminal_focus_format!()
        ),
    )?;
    let mut fields = out.split('\t');
    let window_active = fields.next() == Some("1");
    let session_attached: u32 = fields.next().unwrap_or("0").parse().unwrap_or(0);
    let focus = terminal_focus(fields.next().unwrap_or_default());
    Ok(!in_view(window_active, session_attached, focus))
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Builds the [`Notification`] for a state transition of `agent`. `title`/`body` are the
/// plain text `terminal`/`system` delivery show verbatim: title is `"<kind-or-name>
/// blocked · <session_name>:<window_index>"` for [`Class::Request`], `"<kind-or-name>
/// done · …"` for [`Class::Done`], with `agent.label` as the kind-or-name; body is
/// `detail` unchanged. `sound` is [`SoundChoice::Request`]/[`SoundChoice::Done`] to match
/// `class`. `pane_id` becomes `None` when empty (no agent pane behind this notification).
///
/// `notification_key` is accepted for symmetry with `tmux_message::format_message`'s
/// inputs, all of which this function otherwise has cleanly in hand — but the `herdr`
/// toast text itself is built later, by [`deliver`], from live tmux state at fire
/// time: the jump key from the `@agent-wizard-notification-key` tmux option (see
/// [`NOTIFICATION_KEY_OPTION`]) so a rebind between detection and delivery is always
/// honoured, and — when `pane_id` is non-empty — `kind_or_name`/`location` read
/// straight from the pane itself (see [`live_toast_context`]) rather than recovered by
/// parsing them back out of `title`. Both are plain, user-renameable text with no
/// charset restriction at this layer (a tmux session name via `rename-session`, or an
/// `@agent_display`/`@agent_name` override), so a value that happens to contain this
/// function's own `" blocked \u{b7} "`/`" done \u{b7} "` marker text must never be
/// able to corrupt a split. `notification_key` is therefore not threaded through the
/// returned value.
pub fn for_transition(
    agent: &NotifiedAgent,
    session_name: &str,
    window_index: u32,
    class: Class,
    detail: &str,
    pane_id: &str,
    notification_key: &str,
) -> Notification {
    let _ = notification_key;
    let word = match class {
        Class::Request => "blocked",
        Class::Done => "done",
    };
    let title = format!(
        "{} {word} \u{b7} {session_name}:{window_index}",
        agent.label
    );
    Notification {
        class,
        title,
        body: detail.to_string(),
        pane_id: non_empty(pane_id),
        sound: match class {
            Class::Request => SoundChoice::Request,
            Class::Done => SoundChoice::Done,
        },
        agent: agent.clone(),
    }
}

/// The live value of [`NOTIFICATION_KEY_OPTION`], or [`DEFAULT_NOTIFICATION_KEY`]
/// when unset, empty, or unreadable.
pub(crate) fn live_notification_key(t: &Tmux) -> String {
    t.get_global_option(NOTIFICATION_KEY_OPTION)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_NOTIFICATION_KEY.to_string())
}

/// Live `(kind_or_name, location)` for `pane`, read in one `display-message -p -t
/// <pane>` call (mirrors [`live_notification_key`]): `kind_or_name` follows the same
/// `@agent_display` -> `@agent_name` -> `@agent_kind` resolution order the TUI uses
/// for its "agent" token; `location` is `"<session_name>:<window_index>"`.
///
/// `show_toast`'s `herdr` branch uses this instead of recovering the same two pieces
/// by splitting `for_transition`'s `title` back apart (that approach is kept as
/// [`split_title`], which stays as the fallback for a pane-less toast): a tmux session name
/// (`rename-session`) and `@agent_display`/`@agent_name` are all just plain option
/// values, with nothing at this layer stopping them from containing the exact marker
/// text `for_transition` embeds in `title` (`" blocked \u{b7} "`/`" done \u{b7} "`),
/// which would silently corrupt a string-split — a title of `"sneaky blocked · thing
/// blocked \u{b7} main:2"` splits at the *first* marker, misreading `location` as
/// `"thing blocked \u{b7} main:2"`. Querying tmux directly has no such failure mode:
/// it reads whatever is actually there, marker text and all.
pub fn live_toast_context(t: &Tmux, pane: &str) -> anyhow::Result<(String, String)> {
    // Every column folded server-side, because four of these five are free text and the
    // reply is split on tabs: a tab in `@agent_display` would shift `@agent_name` and
    // `@agent_kind` along and put the wrong text in the toast's label. See
    // `tmux::fold_controls`.
    let out = t.display(pane, &TOAST_CONTEXT_FORMAT)?;
    let mut fields = out.split('\t');
    let session_name = fields.next().unwrap_or_default();
    let window_index = fields.next().unwrap_or_default();
    let display = fields.next().unwrap_or_default();
    let name = fields.next().unwrap_or_default();
    let kind = fields.next().unwrap_or_default();
    let kind_or_name = [display, name, kind]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string();
    Ok((kind_or_name, format!("{session_name}:{window_index}")))
}

/// [`live_toast_context`]'s five columns, tab-joined, each folded by
/// `tmux::fold_controls`. Built once: the fold makes the format string awkward to read
/// inline, and it is the same for every call.
static TOAST_CONTEXT_FORMAT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    [
        "#{session_name}",
        "#{window_index}",
        "#{@agent_display}",
        "#{@agent_name}",
        "#{@agent_kind}",
    ]
    .map(crate::tmux::fold_controls)
    .join("\t")
});

/// The `@agent_kind` of `n`'s pane's occupant, or the kind `n` is about once the pane
/// holds no agent (see [`NotifiedAgent`]), or `""` when there is no pane (a pane-less
/// `Notification`) or the option is unreadable (e.g. the pane closed between detection
/// and delivery). `sound::play`'s per-agent mute lookup simply finds no override for
/// `""`, so the sound still plays unless globally disabled.
pub(crate) fn agent_kind_for_sound(t: &Tmux, n: &Notification) -> String {
    n.pane_id
        .as_ref()
        .and_then(|pane| t.get_pane_option(pane, "@agent_kind").ok().flatten())
        .filter(|kind| !kind.is_empty())
        .unwrap_or_else(|| n.agent.kind.clone())
}

/// Shows `n` per `delivery`; `Off` is a deliberate no-op (herdr's `off` disables only
/// the pop-up — see [`deliver`]).
pub(crate) fn show_toast(
    t: &Tmux,
    delivery: ToastDelivery,
    n: &Notification,
) -> anyhow::Result<()> {
    show_toast_from(t, delivery, n, None)
}

/// A toast another machine's watcher announced, as it is shown here: `machine` is the name this
/// machine gives it (or [`crate::machine::this_machine_word`]) and `location` is the
/// `<session>:<window>` it has there. See [`show_forwarded`].
#[derive(Clone, Debug, PartialEq)]
pub struct Forwarded {
    pub machine: String,
    /// The agent's name or kind; empty for a toast that is about no agent.
    pub agent: String,
    pub location: String,
    pub class: Class,
    pub title: String,
    pub body: String,
    /// Whether this is the toast of a machine dropping off, whose title already names the machine. Any
    /// other toast, an agent-less one included, gets the machine ahead of its title.
    pub dropped: bool,
}

/// Shows a toast another machine announced on this machine's clients, per `[ui.toast] delivery`
/// like every toast (`off` shows none) and by the same [`show_toast`] match: the tmux status line
/// gets `format_message(class, agent, "<machine> \u{b7} <location>", body, key_hint)` with this
/// machine's own `prefix+O` key, or, for a toast that is about no agent, `format_plain_message`;
/// the terminal and the desktop get its title, with the machine ahead of it, and its body. Nothing else happens: no sound, no hook
/// and no events line, because a toast that has been forwarded is never sent on.
pub fn show_forwarded(t: &Tmux, cfg: &Config, f: &Forwarded) -> anyhow::Result<()> {
    show_forwarded_as(t, cfg.ui.toast.delivery, f)
}

/// [`show_forwarded`] under `delivery`: a toast that a forward brought is drawn under the delivery of the
/// machine that sent it, not under this machine's own.
pub fn show_forwarded_as(t: &Tmux, delivery: ToastDelivery, f: &Forwarded) -> anyhow::Result<()> {
    let (n, place) = forwarded_parts(f);
    show_toast_from(t, delivery, &n, Some(&place))
}

/// The [`Notification`] a forwarded toast is shown as, and the place line (`<machine> \u{b7}
/// <location>`) the tmux status line gives it. A local toast's title carries its place
/// (`claude blocked \u{b7} garden:1`); the title of a toast from another machine says where it
/// came from by a `<machine>: ` ahead of it, which is what the terminal and the desktop draw, and
/// the plain status-line text too, for a toast that is about no agent. The toast of a machine
/// dropping off is the one exception: its own words already name the machine.
pub(crate) fn forwarded_parts(f: &Forwarded) -> (Notification, String) {
    // A toast that names no machine (this Mac's, with no other machine listed) says nothing of one.
    let title = if f.dropped || f.machine.is_empty() {
        f.title.clone()
    } else {
        format!("{}: {}", f.machine, f.title)
    };
    let n = Notification {
        class: f.class,
        title,
        body: f.body.clone(),
        pane_id: None,
        sound: SoundChoice::None,
        agent: NotifiedAgent {
            label: f.agent.clone(),
            kind: String::new(),
        },
    };
    let place = if f.location.is_empty() || f.machine.is_empty() {
        if f.machine.is_empty() {
            f.location.clone()
        } else {
            f.machine.clone()
        }
    } else {
        format!("{} \u{b7} {}", f.machine, f.location)
    };
    (n, place)
}

/// [`show_toast`], for a toast that came from another machine when `forwarded` holds the place
/// line (`<machine> \u{b7} <location>`) its words carry.
fn show_toast_from(
    t: &Tmux,
    delivery: ToastDelivery,
    n: &Notification,
    forwarded: Option<&str>,
) -> anyhow::Result<()> {
    match delivery {
        ToastDelivery::Off => Ok(()),
        ToastDelivery::Herdr => {
            // A pane-less notification (`taw notification show`) is a manual message about
            // nothing in particular: it has no agent, no location and nowhere to jump, so it
            // renders plainly rather than being forced into the agent-transition shape
            // (which would use the title as an agent name, show an empty location, and offer
            // a jump hint that goes nowhere).
            let text = match (forwarded, &n.pane_id) {
                // The agent, and where it is, are the other machine's: only the key is this one's.
                (Some(place), _) if !n.agent.label.is_empty() => tmux_message::format_message(
                    n.class,
                    &n.agent.label,
                    place,
                    &n.body,
                    &live_notification_key(t),
                ),
                (_, Some(pane)) => {
                    let (live, location) = live_toast_context(t, pane)?;
                    // A pane that no longer holds an agent names none: the toast names
                    // the one it is about (see `NotifiedAgent`).
                    let kind_or_name = if live.is_empty() {
                        n.agent.label.clone()
                    } else {
                        live
                    };
                    let key_hint = live_notification_key(t);
                    tmux_message::format_message(
                        n.class,
                        &kind_or_name,
                        &location,
                        &n.body,
                        &key_hint,
                    )
                }
                (_, None) => tmux_message::format_plain_message(&n.title, &n.body),
            };
            tmux_message::show(t, &text)
        }
        ToastDelivery::Terminal => {
            let payload = terminal::osc_payload(&n.title, &n.body);
            terminal::write_to_clients(t, &payload).map(|_| ())
        }
        ToastDelivery::System => system::send(&n.title, &n.body),
    }
}

/// Delivers `n`: shows a toast per `cfg.ui.toast.delivery`, then plays a sound, each
/// step independent of the other's success. A failure is logged and goes no further, so
/// there is nothing for the caller to handle:
///
/// 1. **Toast**, via [`show_toast`]. A no-op for [`ToastDelivery::Off`] (herdr's
///    `off` disables only the pop-up). A delivery failure (e.g. `osascript`/
///    `notify-send` missing, no attached client to write an OSC sequence to, or the
///    `herdr` `display-message` call erroring) is logged and does not stop the rest
///    of this function.
/// 2. **Sound**, via `sound::play`, whenever `n.sound != SoundChoice::None` —
///    regardless of `delivery`, `Off` included (herdr mutes the pop-up, not the
///    sound). A spawn failure is logged.
///
/// Nothing is recorded in tmux: `prefix+O` walks next up from live state
/// ([`open_target`]), so there is no "last notification" to remember.
///
/// `deliver` does not wait or re-check anything: `[ui.toast] delay_seconds` is the
/// watcher's job (a later task) — it waits, re-reads `@agent_state`, and calls
/// `deliver` only when the state that triggered `n` is still current. By the time
/// `deliver` runs, that decision has already been made.
pub fn deliver(t: &Tmux, cfg: &Config, paths: &Paths, n: &Notification) {
    show_toast_logged(t, cfg, n);
    play_sound(t, cfg, paths, n);
}

/// [`deliver`]'s first half: [`show_toast`] per `cfg.ui.toast.delivery`, a failure logged.
fn show_toast_logged(t: &Tmux, cfg: &Config, n: &Notification) {
    if let Err(err) = show_toast(t, cfg.ui.toast.delivery, n) {
        log::warn!("notification toast delivery failed: {err:#}");
    }
}

/// [`deliver`]'s second half: the sound of `n`, whenever it has one, a failure logged.
pub(crate) fn play_sound(t: &Tmux, cfg: &Config, paths: &Paths, n: &Notification) {
    if n.sound != SoundChoice::None {
        let agent = agent_kind_for_sound(t, n);
        if let Err(err) = sound::play(&cfg.ui.sound, &paths.config_dir, &agent, n.sound) {
            log::warn!("notification sound failed: {err:#}");
        }
    }
}

/// The word a notification of `class` goes by in a hook's `TAW_KIND`: `request` for an agent
/// that wants you, `done` for one that finished.
pub fn kind_word(class: Class) -> &'static str {
    match class {
        Class::Request => "request",
        Class::Done => "done",
    }
}

/// A notification's text on one plain line, for a hook: the same words the toast has, without
/// its styling, its `prefix+O next up` hint (nowhere to press it on a phone) or the escaping
/// tmux needs. That is the title (`<agent> blocked \u{b7} <session>:<window>`), then the detail
/// after a middle dot. A pane's detail is cut to the length the toast cuts it to, so what a
/// phone is sent is the line the user would have seen; the body of a notification about no
/// pane, which the toast does not cut either, is kept whole. Control characters are dropped.
pub fn plain_text(n: &Notification) -> String {
    let title = strip_ascii_control(&n.title);
    let body = strip_ascii_control(&n.body);
    if body.trim().is_empty() {
        return title;
    }
    let body = if n.pane_id.is_some() {
        tmux_message::truncate_chars(&body, tmux_message::MAX_DETAIL_CHARS)
    } else {
        body
    };
    format!("{title} \u{b7} {body}")
}

/// [`deliver`], and then the `[hooks] notification` commands: everything taw does when it
/// decides a person should be told. The hook is a delivery of its own, so it runs whatever
/// `[ui.toast] delivery` is, as sounds do (`off` mutes the pop-up and nothing else). Each
/// step is independent of the other's success, and neither waits: the hooks are started in the
/// background.
///
/// Every caller that means "tell the person" calls this and not [`deliver`]: the watcher, when
/// a notification's delay is up and nobody is looking, and `taw notification show`. The
/// watcher's gate is what makes the hook fire only when a toast would.
pub fn announce(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &std::path::Path,
    n: &Notification,
) {
    announce_with(t, cfg, paths, taw_bin, None, n);
}

/// [`announce`] for a caller that keeps tmux's global environment for its hooks (the watcher).
pub fn announce_with(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &std::path::Path,
    server_env: Option<&crate::hooks::server_env::ServerEnvCache>,
    n: &Notification,
) {
    if crate::machine::files::lease_fresh(paths, crate::machine::files::now_ms()) {
        // An app on a Mac holds this machine's attention: the toast is shown here as it always was
        // (a person at this machine's own terminal sees it), and the app is told, so it speaks on the
        // Mac: it plays the sound and runs the hook there, once.
        show_toast_logged(t, cfg, n);
        hand_to_the_app(t, paths, n);
        return;
    }
    deliver(t, cfg, paths, n);
    let ctx = crate::plugin::PluginCtx {
        tmux: t,
        cfg,
        paths,
        taw_bin,
        server_env,
    };
    crate::hooks::notification(&ctx, n);
}

/// Appends the `toast` line of `n` to this machine's events file, for the app that holds the lease
/// ([`crate::machine::files`]). The agent, the location and the kind are the ones the toast here shows
/// (live, like [`show_toast`] reads them); the lease flag is left false, the feed that sends it sets it.
fn hand_to_the_app(t: &Tmux, paths: &Paths, n: &Notification) {
    let (agent, location) = match &n.pane_id {
        Some(pane) => match live_toast_context(t, pane) {
            Ok((live, location)) if !live.is_empty() => (live, location),
            Ok((_, location)) => (n.agent.label.clone(), location),
            Err(_) => (n.agent.label.clone(), String::new()),
        },
        None => (n.agent.label.clone(), String::new()),
    };
    let toast = crate::machine::wire::Toast {
        kind: kind_word(n.class).to_string(),
        pane: n.pane_id.clone().unwrap_or_default(),
        text: plain_text(n),
        title: n.title.clone(),
        body: n.body.clone(),
        agent,
        agent_kind: agent_kind_for_sound(t, n),
        location,
        lease: false,
    };
    crate::machine::files::append_event(paths, &crate::machine::wire::Down::Toast(toast));
}

/// Goes to the next agent, "next up", from live tmux state: the next
/// agent pane after the pane you are in, in tmux order, that needs you, else, with none that
/// does, the next agent pane of any kind (see [`attention::next_up`]), and
/// focuses it: `select-window` then `select-pane`, which make it the active pane of its own
/// session without moving any client. Returns the pane id and the [`Group`] of the pane it
/// went to, or `None` when no pane holds an agent. One press is one step, so pressing the key
/// again goes on to the next, wrapping.
///
/// The latest notification is not a step of its own, so a fresh request does not
/// outrank the walk.
///
/// `focus` decides whether a *client* moves too, and only one kind of caller may ask for
/// it:
///
/// - `true` — a `taw` invocation tmux can resolve a current client for: the plugin's key
///   binding (`run-shell "<taw> notification open-target"`), or a hook. `switch-client -t
///   <session>` runs first, its failure ignored, since there may be no client attached to
///   switch at all.
/// - `false` — the popup. That bare `switch-client` has no `-c`, and a `display-popup -E`
///   command runs inside its own popup rather than a pane, so `$TMUX_PANE` is unset and
///   tmux has to guess which client is "current" — on a multi-client server it can move
///   one the user is not looking at. The popup takes the pane id back and switches the
///   client it is shown on instead (`tui::actions::switch_client`: the client its launcher
///   named, else one picked by hand through `cli::common::resolve_client`), for the same
///   reason the popup calls the worktree verbs with `focus: false`.
///
/// `after` is the pane to walk from: the popup passes its own pane; a `run-shell` caller
/// passes `None` and the current client's active pane is read here.
pub fn open_target(
    t: &Tmux,
    focus: bool,
    after: Option<&str>,
) -> anyhow::Result<Option<(String, Group)>> {
    let panes = t.list_panes()?;
    let sessions = t.list_sessions()?;
    let current = match after {
        Some(id) => Some(id.to_string()),
        None => t
            .run(&["display-message", "-p", "#{pane_id}"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let Some((pane, group)) = attention::next_up(&panes, &sessions, current.as_deref()) else {
        return Ok(None);
    };
    let pane_id = pane.pane_id.clone();
    focus_on(t, pane, focus)?;
    Ok(Some((pane_id, group)))
}

/// The three commands that put `pane` in front: `switch-client` first when `focus` (a failure of it is not one: no
/// client may be current), then `select-window` and `select-pane`.
fn focus_on(t: &Tmux, pane: &crate::tmux::PaneInfo, focus: bool) -> anyhow::Result<()> {
    if focus {
        let _ = t.run_ok(&["switch-client", "-t", &pane.session_id]);
    }
    t.run_ok(&["select-window", "-t", &pane.window_id])?;
    t.run_ok(&["select-pane", "-t", &pane.pane_id])?;
    Ok(())
}

/// How old the walk file may be and still be the app's word: the app writes it again at least every 5 s.
pub const WALK_FRESH_MS: u64 = 15_000;

/// Where the key in a client the app started goes next ([`next_stop`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// A stop of this machine: its own pane id, focused as `open_target` focuses.
    Here(String),
    /// A stop of another machine, in the app's names (`studio:%16`, or this Mac's bare `%7`): the app goes there.
    Elsewhere(String),
}

/// The stop of `walk` that follows `current` (this machine's own pane id): the stop after it in the walk's order,
/// wrapping, when it is one, and the first stop after it when it is not (the walk lists this machine's stops in
/// tmux order, so a pane that is none is placed among them by `place`, tmux's own order). `None` for a walk that
/// is older than [`WALK_FRESH_MS`] or empty, or a pane that is no stop and that `place` cannot place: the caller
/// walks this machine as it always did.
///
/// A pane that is no stop, on a machine that has no stop in the walk, goes to stop 0 (the walk's first): the walk
/// carries no order of the machines, so "the first stop after this pane" cannot be told, and this machine's place
/// in that order is not known here.
///
/// The walk is in the app's names: `walk.you` is the name the app gives this machine, the prefix of its own ids
/// (empty on this Mac, whose ids are bare).
pub fn next_stop(
    walk: &crate::machine::files::Walk,
    current: &str,
    now_ms: u64,
    place: impl Fn(&str) -> Option<attention::Position>,
) -> Option<Next> {
    if walk.stops.is_empty() || now_ms.saturating_sub(walk.at) >= WALK_FRESH_MS {
        return None;
    }
    // This machine's raw id of a stop, when the stop is this machine's.
    let mine = |stop: &str| -> Option<String> {
        match crate::machine::split_id(stop) {
            (None, raw) if walk.you.is_empty() => Some(raw.to_string()),
            (Some(name), raw) if name == walk.you => Some(raw.to_string()),
            _ => None,
        }
    };
    let len = walk.stops.len();
    let at = match walk
        .stops
        .iter()
        .position(|stop| mine(stop).as_deref() == Some(current))
    {
        Some(at) => (at + 1) % len,
        None => {
            let here = place(current)?;
            let own: Vec<(usize, attention::Position)> = walk
                .stops
                .iter()
                .enumerate()
                .filter_map(|(at, stop)| Some((at, place(&mine(stop)?)?)))
                .collect();
            match own.iter().find(|(_, place)| *place > here) {
                Some(&(at, _)) => at,
                None => own.last().map_or(0, |&(at, _)| (at + 1) % len),
            }
        }
    };
    let stop = &walk.stops[at];
    Some(match mine(stop) {
        Some(raw) => Next::Here(raw),
        None => Next::Elsewhere(stop.clone()),
    })
}

/// Focuses `pane_id` of `panes` as [`open_target`] does (`switch-client` first when `focus`), and returns its
/// [`Group`]. `None` when tmux has no such pane.
pub fn focus_pane(
    t: &Tmux,
    panes: &[crate::tmux::PaneInfo],
    pane_id: &str,
    focus: bool,
) -> Option<Group> {
    let pane = panes.iter().find(|pane| pane.pane_id == pane_id)?;
    focus_on(t, pane, focus).ok()?;
    Some(Group::of(pane.agent.state, pane.agent.seen))
}

/// Strips ASCII control characters (`0x00`-`0x1F`, `0x7F`) — shared by
/// `tmux_message::format_message` and `terminal::osc_payload`, which both embed
/// caller-supplied text into a control-character-sensitive protocol (tmux's own
/// `#[...]` styling / `display-message`, and OSC terminal escape sequences).
pub(crate) fn strip_ascii_control(s: &str) -> String {
    s.chars().filter(|&c| !is_ascii_control(c)).collect()
}

fn is_ascii_control(c: char) -> bool {
    (c as u32) <= 0x1f || c as u32 == 0x7f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: &str) -> NotifiedAgent {
        NotifiedAgent {
            label: kind.to_string(),
            kind: kind.to_string(),
        }
    }

    /// The title the terminal and the desktop draw for a forwarded toast names the machine; the one
    /// of a machine dropping off already does and stays as it is.
    #[test]
    fn a_forwarded_toast_title_names_the_machine_it_is_about() {
        let mut f = Forwarded {
            machine: "studio".into(),
            agent: "claude".into(),
            location: "garden:1".into(),
            class: Class::Request,
            title: "claude blocked \u{b7} garden:1".into(),
            body: "Do you want to proceed?".into(),
            dropped: false,
        };
        let (n, place) = forwarded_parts(&f);
        assert_eq!(n.title, "studio: claude blocked \u{b7} garden:1");
        assert_eq!(n.body, "Do you want to proceed?");
        assert_eq!(place, "studio \u{b7} garden:1");

        f.agent = String::new();
        f.location = String::new();
        f.title = "pi dropped off (last seen just now)".into();
        f.dropped = true;
        let (n, place) = forwarded_parts(&f);
        assert_eq!(n.title, "pi dropped off (last seen just now)");
        assert_eq!(place, "studio");

        // Any other toast without an agent names the machine too.
        f.title = "Build finished".into();
        f.dropped = false;
        let (n, _) = forwarded_parts(&f);
        assert_eq!(n.title, "studio: Build finished");
    }

    #[test]
    fn for_transition_builds_request_title_body_pane_and_sound() {
        let n = for_transition(
            &agent("claude"),
            "main",
            2,
            Class::Request,
            "needs input",
            "%7",
            "O",
        );
        assert_eq!(n.class, Class::Request);
        assert_eq!(n.title, "claude blocked \u{b7} main:2");
        assert_eq!(n.body, "needs input");
        assert_eq!(n.pane_id.as_deref(), Some("%7"));
        assert_eq!(n.sound, SoundChoice::Request);
    }

    #[test]
    fn for_transition_builds_done_title_and_sound() {
        let n = for_transition(
            &agent("codex"),
            "work",
            0,
            Class::Done,
            "finished",
            "%3",
            "O",
        );
        assert_eq!(n.title, "codex done \u{b7} work:0");
        assert_eq!(n.sound, SoundChoice::Done);
    }

    #[test]
    fn for_transition_treats_an_empty_pane_id_as_none() {
        let n = for_transition(&agent("claude"), "main", 0, Class::Done, "d", "", "O");
        assert_eq!(n.pane_id, None);
    }

    #[test]
    fn terminal_focus_reads_the_marks_of_a_verified_tmux_and_nothing_else() {
        // One mark per client attached to the pane's session.
        assert_eq!(terminal_focus("3.6b:1"), Some(true));
        assert_eq!(terminal_focus("3.6b:0"), Some(false));
        assert_eq!(
            terminal_focus("3.6b:010"),
            Some(true),
            "one terminal in front of the user is enough"
        );
        assert_eq!(terminal_focus("3.6b:00"), Some(false));
        assert_eq!(
            terminal_focus("next-3.7:0"),
            Some(false),
            "a build past 3.6"
        );
        // tmux cannot say: `focus-events` off (nothing at all), or nobody attached (no
        // marks)…
        assert_eq!(terminal_focus(""), None);
        assert_eq!(terminal_focus("3.6b:"), None);
        // …or a tmux this was not verified on, where a `focused` flag it lacked would read
        // as every terminal in the background.
        assert_eq!(terminal_focus("3.5a:0"), None);
        assert_eq!(terminal_focus("3.2:1"), None);
        // Nor is anything else this format's answer: a version tmux does not number, or what
        // an older tmux leaves of a loop it does not know.
        assert_eq!(terminal_focus("openbsd-7.6:0"), None);
        assert_eq!(terminal_focus("3.6b:L:"), None);
        assert_eq!(terminal_focus("3.6b:1 0"), None);
    }

    #[test]
    fn in_view_is_an_attached_sessions_active_window_with_its_terminal_not_away() {
        assert!(
            in_view(true, 1, None),
            "focus nobody can tell counts as looking"
        );
        assert!(in_view(true, 2, Some(true)));
        assert!(!in_view(true, 1, Some(false)), "the user is in another app");
        assert!(!in_view(false, 1, Some(true)), "another window");
        assert!(!in_view(true, 0, None), "a detached session");
    }

    #[test]
    fn a_notification_is_a_request_or_a_done_to_a_hook() {
        assert_eq!(kind_word(Class::Request), "request");
        assert_eq!(kind_word(Class::Done), "done");
    }

    fn about(pane: Option<&str>, title: &str, body: &str) -> Notification {
        Notification {
            class: Class::Request,
            title: title.to_string(),
            body: body.to_string(),
            pane_id: pane.map(str::to_string),
            sound: SoundChoice::None,
            agent: NotifiedAgent::default(),
        }
    }

    #[test]
    fn the_plain_text_of_a_notification_about_a_pane_is_its_title_and_its_detail() {
        let n = about(Some("%7"), "claude blocked \u{b7} main:2", "needs input");
        assert_eq!(
            plain_text(&n),
            "claude blocked \u{b7} main:2 \u{b7} needs input"
        );
        // No detail leaves no separator behind, as in the toast.
        let quiet = about(Some("%7"), "claude done \u{b7} main:2", "  \x07 ");
        assert_eq!(plain_text(&quiet), "claude done \u{b7} main:2");
    }

    /// The toast cuts a pane's detail to 60 characters, so the text a phone is sent is the
    /// same line the user would have seen.
    #[test]
    fn a_detail_is_cut_to_the_length_the_toast_cuts_it_to() {
        let n = about(Some("%7"), "claude done \u{b7} main:2", &"x".repeat(100));
        let text = plain_text(&n);
        assert!(
            text.ends_with(&format!("{}\u{2026}", "x".repeat(60))),
            "{text}"
        );
    }

    #[test]
    fn a_notification_about_no_pane_keeps_all_of_what_it_was_given() {
        let n = about(None, "Build finished", &"y".repeat(100));
        assert_eq!(
            plain_text(&n),
            format!("Build finished \u{b7} {}", "y".repeat(100))
        );
        assert_eq!(plain_text(&about(None, "Heads up", "")), "Heads up");
    }

    /// The text is for a script and a phone, not for tmux: nothing in it is a format to
    /// escape, so a `#` and a `%` are what they were, and no control character is left.
    #[test]
    fn the_plain_text_is_not_escaped_for_tmux_and_holds_no_control_character() {
        let n = about(
            Some("%7"),
            "claude done \u{b7} main:2",
            "PR #482 is 100% done\x1b[31m\n",
        );
        assert_eq!(
            plain_text(&n),
            "claude done \u{b7} main:2 \u{b7} PR #482 is 100% done[31m"
        );
    }

    /// The net the gate puts first on PATH for the desktop (scripts/desktop-net.sh) has a refusing stub
    /// for every program taw shows a notification or plays a sound through, on every platform, so a test
    /// that has no recorder of its own reaches a stub and not the screen or the speakers of whoever runs
    /// it. The agents and runtimes have a net of their own (scripts/agent-net-names.txt), which shares no
    /// name with this one.
    #[test]
    fn the_gates_desktop_net_has_a_stub_for_every_program_taw_notifies_or_plays_through() {
        let names = |list: &'static str| -> std::collections::HashSet<&'static str> {
            list.lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect()
        };
        let net = names(include_str!("../../scripts/desktop-net-names.txt"));
        let programs = [
            system::OSASCRIPT,
            system::NOTIFY_SEND,
            sound::AFPLAY,
            sound::PAPLAY,
            sound::APLAY,
        ];
        let missing: Vec<&str> = programs
            .into_iter()
            .filter(|program| !net.contains(program))
            .collect();
        assert!(missing.is_empty(), "the net has no stub for {missing:?}");
        let agents = names(include_str!("../../scripts/agent-net-names.txt"));
        let mut shared: Vec<&&str> = net.intersection(&agents).collect();
        shared.sort();
        assert!(shared.is_empty(), "both nets have a stub for {shared:?}");
    }

    #[test]
    fn strip_ascii_control_removes_c0_and_del_only() {
        assert_eq!(strip_ascii_control("a\x1b\x07\x7fb"), "ab");
        assert_eq!(strip_ascii_control("caf\u{e9}"), "caf\u{e9}"); // non-ASCII untouched
    }

    // ------------------------------------------------------------------ the walk the app sent

    use crate::machine::files::Walk;

    fn walk(you: &str, stops: &[&str], at: u64) -> Walk {
        Walk {
            you: you.into(),
            stops: stops.iter().map(|stop| stop.to_string()).collect(),
            at,
            holder: String::new(),
        }
    }

    /// tmux's order of this machine's panes, as `place` answers it: (session, window, pane).
    fn place(id: &str) -> Option<attention::Position> {
        match id {
            "%7" => Some((0, 1, 1)),
            "%16" => Some((1, 1, 1)),
            "%17" => Some((1, 1, 2)),
            "%20" => Some((1, 2, 1)),
            "%30" => Some((2, 1, 1)),
            _ => None,
        }
    }

    const NOW: u64 = 100_000;

    fn next(walk: &Walk, current: &str) -> Option<Next> {
        next_stop(walk, current, NOW, place)
    }

    fn here(id: &str) -> Option<Next> {
        Some(Next::Here(id.into()))
    }

    fn elsewhere(id: &str) -> Option<Next> {
        Some(Next::Elsewhere(id.into()))
    }

    /// Studio's copy of the walk: this Mac's %7 first, studio's own two stops, then pi's.
    fn studio_walk() -> Walk {
        walk(
            "studio",
            &["%7", "studio:%16", "studio:%20", "pi:%3"],
            NOW - 1_000,
        )
    }

    #[test]
    fn the_next_stop_is_this_machines_own_foreign_wrapping_or_first_after_a_pane_that_is_none() {
        let w = studio_walk();
        // Own: the next stop is on this machine.
        assert_eq!(next(&w, "%16"), here("%20"));
        // Foreign: the stop after studio's last is pi's.
        assert_eq!(next(&w, "%20"), elsewhere("pi:%3"));
        // Not a stop, in the middle of this machine's: the first stop after it.
        assert_eq!(next(&w, "%17"), here("%20"));
        // Not a stop, after all of this machine's: the stop that follows its block.
        assert_eq!(next(&w, "%30"), elsewhere("pi:%3"));
        // Wrap: the walk of pi's machine ends on its own stop and goes round to the first, this Mac's.
        let pi = walk("pi", &["%7", "studio:%16", "pi:%3"], NOW - 1_000);
        assert_eq!(
            next_stop(&pi, "%3", NOW, |id| (id == "%3").then_some((0, 1, 1))),
            elsewhere("%7")
        );
    }

    /// `you` is what tells this machine's `%7` from this Mac's: studio's own `%7` is `studio:%7` in the walk.
    #[test]
    fn this_machines_own_ids_are_told_from_another_machines_by_you() {
        let w = walk("studio", &["%7", "studio:%7", "pi:%3"], NOW - 1_000);
        assert_eq!(
            next(&w, "%7"),
            elsewhere("pi:%3"),
            "studio's %7 is the second stop"
        );
        // On this Mac `you` is empty and the bare ids are its own.
        let mac = walk("", &["%7", "studio:%16"], NOW - 1_000);
        assert_eq!(next(&mac, "%7"), elsewhere("studio:%16"));
    }

    #[test]
    fn a_stale_or_empty_walk_or_an_unplaceable_pane_gives_none() {
        let w = studio_walk();
        assert_eq!(
            next_stop(&w, "%16", NOW + WALK_FRESH_MS, place),
            None,
            "15 s old"
        );
        assert_eq!(
            next_stop(&w, "%16", NOW + WALK_FRESH_MS - 2_000, place),
            here("%20"),
            "just young enough"
        );
        assert_eq!(next(&walk("studio", &[], NOW), "%16"), None);
        assert_eq!(
            next(&w, "%99"),
            None,
            "a pane that is no stop and that tmux cannot place"
        );
    }
}

#[cfg(test)]
mod no_machine_word_tests {
    use super::*;

    /// A forwarded toast that names no machine (this Mac's, with no other machine listed) has no `: ` ahead of its
    /// title and no machine in its place line; one that names a machine keeps both.
    #[test]
    fn a_forwarded_toast_with_no_machine_word_names_none() {
        let toast = |machine: &str| Forwarded {
            machine: machine.into(),
            agent: "claude".into(),
            location: "garden:1".into(),
            class: Class::Request,
            title: "claude blocked".into(),
            body: String::new(),
            dropped: false,
        };
        let (named, place) = forwarded_parts(&toast("studio"));
        assert_eq!(
            (named.title.as_str(), place.as_str()),
            ("studio: claude blocked", "studio \u{b7} garden:1")
        );
        let (none, place) = forwarded_parts(&toast(""));
        assert_eq!(
            (none.title.as_str(), place.as_str()),
            ("claude blocked", "garden:1")
        );
    }
}
