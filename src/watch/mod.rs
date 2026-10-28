// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The pipe-pane watcher: one `taw watch run <pane>` process per pane, started by tmux.
//!
//! ```text
//! tmux pipe-pane -t <pane> 'taw watch run <pane>'
//!        │ stdin = the pane's output bytes
//!        ▼
//!   300 ms tick, read after output  →  display-message (+ capture-pane with an agent)
//!        ▼
//!   detect::evaluate  →  watch::state::decide  →  @agent_* pane options
//!                                                 rail · plugin event · notification
//! ```
//!
//! Three things make this cheap enough to run per pane:
//!
//! * **The bytes are only a signal.** They are never parsed as a transcript — output only
//!   makes the pane eligible for a read at its next tick, plus a scan for OSC `9;4`
//!   progress (see [`stream`]). The screen is read back from tmux when a read is due. A
//!   report an agent makes of its state does the same through [`nudge`], for the pane
//!   that prints nothing.
//! * **Nothing polls for nothing.** The loop blocks on the byte channel with a timeout
//!   that is exactly the time until the next read is owed (`next_read`). It wakes on the
//!   tick grid only while an agent is present or output is pending: a quiet agent pane
//!   costs one `display-message` every few seconds, and a quiet pane with no agent in it
//!   costs nothing at all.
//! * **tmux is the database.** The watcher owns no state that outlives it: everything it
//!   learns goes into `@agent_*` pane options, which is where the popup, the status rail,
//!   the CLI and plugins read it from. Nothing here is a daemon — tmux starts and stops
//!   this process with the pipe.
//!

pub mod binary;
mod container;
mod job;
mod log;
pub mod nudge;
pub mod reload;
#[cfg(test)]
mod sim;
pub mod state;
pub mod stream;
mod suspend;
pub mod typed_line;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// `watch::log` shadows the `log` crate inside this module, so the macros are imported
// by name (see log.rs).
use ::log::{debug, info, warn};
use anyhow::Context;

use crate::config::Config;
use crate::detect::{self, LoadedManifest, ManifestStore};
use crate::hooks::server_env::ServerEnvCache;
use crate::notify;
use crate::paths::Paths;
use crate::plugin::{self, PluginCtx};
use crate::rail;
use crate::store::{self, AgentRecord, AgentState};
use crate::tmux::{PaneInfo, Socket, Tmux, ids};

use self::binary::Binary;
pub(crate) use self::job::foreground_job;
pub use self::log::init_logging;
use self::state::Forced;
pub use self::state::{Decision, Observed, decide};
pub use self::stream::{StreamState, scan_osc_progress};

/// User option that turns automatic watching off (`set -g @agent-wizard-watch off`).
const WATCH_OPTION: &str = "@agent-wizard-watch";
/// Pane option holding the pid of the watcher attached to a pane.
const WATCH_PID_OPTION: &str = "@agent_watch_pid";
/// `@agent_container`: the container runtime (`docker`, `podman`, `nerdctl`, `container` or
/// `sbx`) in front of the pane while the agent in it was named by that runtime's arguments,
/// so a row can say the agent runs in a container. It is taken away at the first read that
/// finds the runtime no longer in front, and `store::clear_agent` takes it with the rest of
/// the agent's record.
const CONTAINER_OPTION: &str = "@agent_container";
/// `@agent_pid`: the pid of the process `taw agent start` launched, recorded so its exit is
/// detectable even when its last screen still matches a rule. Its `@agent_kind_hint` (the
/// identity a hint-started agent keeps) deliberately outlives the occupant — neither
/// `store::clear_agent` nor herdr clears it — so an exited hint-only agent is reported as
/// `unknown` (hint kept), never wiped: see [`Watcher::hint_only_agent_exited`] and
/// [`state::decide`]'s `agent_exited`.
const AGENT_PID_OPTION: &str = "@agent_pid";
/// The manifest rule that explains the current state — the pane's evidence.
const RULE_OPTION: &str = "@agent_rule";
/// The name `taw agent start` or `agent rename` gave the pane's agent: its handle for the
/// `agent` verbs, and it belongs to that agent (see `state::name_leaves_with`).
const NAME_OPTION: &str = "@agent_name";
/// Written to `@agent_source` for every state this module decides.
const MANIFEST_SOURCE: &str = "manifest";
const EVENT_AGENT_STATUS_CHANGED: &str = "pane.agent_status_changed";
/// Cached manifests are refreshed at most this often (marker file mtime). Shared with
/// `taw manifest update --if-stale`, which touches the same marker — the two must agree
/// about staleness or one would keep invalidating the other's work.
pub const MANIFEST_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const MANIFEST_BASE_URL: &str = "https://herdr.dev/agent-detection";
/// Upper bound on one read from the pipe. The bytes are a signal, not a payload, so a
/// bigger buffer buys nothing: it only delays the quiet-gap clock.
const READ_CHUNK: usize = 64 * 1024;

/// One column of [`DISPLAY_FORMAT`], with every newline, carriage return and tab in the
/// *value* replaced by a space by tmux itself — three global substitutions chained with
/// `;` (`#{s/<lf>/ /;s/<cr>/ /;s/<tab>/ /:var}`, verified on tmux 3.6b, including on
/// `@`-prefixed user options).
///
/// This is what keeps the parse positional. Half of those columns are free text a user
/// or a hook can set to anything — a renamed session, `@agent_display`, `@agent_title`,
/// a pane title straight off an OSC 2 sequence — and `set-option -p @agent_display
/// "a<tab>b"` round-trips the tab, as it does a newline. Splitting on tab without this
/// turns one such value into a misread record, or (with a strict field count) into a
/// watcher that logs and gives up on the pane for as long as the option is set; a newline
/// is worse still, since it makes the one-line answer stop looking like one row at all.
/// The cost is that a tab inside one of those values reads as a space, which is what a
/// terminal renders it as anyway.
///
/// Note a pattern must not contain `:` — tmux ends the last modifier at the first one, so
/// `s/[[:blank:]]/ /` silently expands to nothing.
macro_rules! folded {
    ($variable:literal) => {
        concat!("#{s/\n/ /;s/\r/ /;s/\t/ /:", $variable, "}")
    };
}

/// Everything the evaluation loop needs from one `display-message` call, in one round
/// trip. Column order is what [`Observation::parse`] expects.
const DISPLAY_FORMAT: &str = concat!(
    folded!("pane_current_command"),
    "\t",
    folded!("pane_title"),
    "\t",
    folded!("window_active"),
    "\t",
    folded!("session_attached"),
    "\t",
    folded!("@agent_kind_hint"),
    "\t",
    folded!("@agent_source"),
    "\t",
    folded!("@agent_state"),
    "\t",
    folded!("@agent_seen"),
    "\t",
    folded!("@agent_kind"),
    "\t",
    folded!("session_name"),
    "\t",
    folded!("window_index"),
    "\t",
    folded!("@agent_display"),
    "\t",
    folded!("@agent_name"),
    "\t",
    // Read because `decide` compares it with the rule that matches *now*: without it
    // `prev.rule` is always `None` and every evaluation of any matching pane answers
    // `Decision::RuleChanged`, writing an unchanged value once per pane per tick.
    folded!("@agent_rule"),
    "\t",
    // The hot-reload token (`taw config reload` / `taw manifest reload` bump it). Read
    // here rather than with its own `show-options` call: this format is expanded once per
    // evaluation anyway, so watching for a forced reload costs nothing extra. It is a
    // *global* option, but `display-message -p -t <pane>` expands globals too.
    folded!("@agent_wizard_reload"),
    "\t",
    // The kind the agent's own hook reported, which `decide` compares with `@agent_kind`: an
    // agent that named itself is not judged by the process in front of it.
    folded!("@agent_reported_kind"),
    "\t",
    // The session the agent's hook reported, which a replacement takes away (`Update::
    // release_identity`) only while it is still the one this read saw, and which `decide`
    // compares with the one the last read saw: a new session ends the completion the pane had.
    folded!("@agent_session_id"),
    "\t",
    // The session's path, which some agents report in place of an id (Pi, OMP): a new one is a
    // new session as a new id is.
    folded!("@agent_session_path"),
    "\t",
    // The stamp of the agent's last report of work or a wait (`store::TURN_OPTION`), which `decide`
    // compares with the one the last read took in: a turn that began and ended between two reads
    // leaves the state where it was, and moves this.
    folded!("@agent_turn"),
    "\t",
    // Whether the pane's agent is being taken (`store::ACQUIRING_OPTION`), which `taw agent start`
    // marks before the agent it launches is read, and which outlives this watcher.
    folded!("@agent_acquiring"),
    "\t",
    // Where `taw agent start` typed the agent's command (`store::TYPED_LINE_OPTION`), and what says
    // where that line is now: the size of the pane's history, which grows by a row for each row that
    // leaves the top of the screen, and whether the alternate screen is in use.
    folded!("@agent_typed_line"),
    "\t",
    folded!("history_size"),
    "\t",
    folded!("alternate_on"),
    "\t",
    // The stamp of the last report taken for the pane's agent (`store::REPORT_STAMP_OPTION`): a clear of
    // the record of an agent that has left lands only while the pane still holds it, so an agent that
    // claimed the pane after this read is not cleared with the one that left.
    folded!("@agent_report_stamp"),
    "\t",
    // The runtime an earlier read of this pane found its agent running under, by this watcher or
    // one before it (`@agent_container`): a watcher started again takes up from it, and announces
    // no container that was already there.
    folded!("@agent_container"),
    "\t",
    // The profile `taw sandbox` started the pane's agent through (`@agent_sandbox`): the hooks'
    // `TAW_SANDBOX`, and the mark's two ends (`sandbox::mark`).
    folded!("@agent_sandbox"),
    "\t",
    // The process that set it (`@agent_sandbox_pid`), whose end, with no container in front, is
    // the mark's (`sandbox::mark`).
    folded!("@agent_sandbox_pid"),
    "\t",
    // Whether the user's terminal has focus, for "is the user looking" (see
    // `notify::terminal_focus`). Not folded, and needing no fold: it expands to a tmux
    // version, a `:` and `0`/`1` marks, or to nothing. It stays the last column.
    crate::notify::terminal_focus_format!()
);
const DISPLAY_FIELDS: usize = 28;

// Follows herdr's src/app/actions.rs, the completion reset of #4457 (lines 1686
// to 1712), which takes a pane's pending notification away when its session or its agent changes.
// Differences: a notification here waits out its delay on a thread of its own, which reads the
// pane's agent and session when it comes due and drops the notification if either changed.
/// What a notification's thread reads of its pane when its delay is up, in one `display-message`:
/// the state, and the agent and the session it is the state of (see [`state::Pending::due`]).
const DUE_FORMAT: &str = concat!(
    folded!("@agent_state"),
    "\t",
    folded!("@agent_kind"),
    "\t",
    folded!("@agent_session_id"),
    "\t",
    folded!("@agent_session_path"),
);

/// A [`DUE_FORMAT`] answer: the state (`None` when unset), and who holds it.
fn due_read(line: &str) -> (Option<String>, state::Earned) {
    let mut fields = line
        .split('\t')
        .map(|field| Some(field.to_string()).filter(|field| !field.is_empty()));
    let mut next = || fields.next().flatten();
    let current = next();
    let earned = state::Earned {
        kind: next(),
        session_id: next(),
        session_path: next(),
    };
    (current, earned)
}

/// How long to wait after `n` consecutive failed evaluations: 1, 2, 4, 8, 16, then 30
/// seconds. A watcher lives for days, so a pane that is briefly unreadable (a busy tmux
/// server, a transient `display-message` failure) must neither retire the watcher nor
/// let it spin retrying.
fn backoff_delay(consecutive_failures: u32) -> Duration {
    const MAX_SECONDS: u64 = 30;
    let seconds = 1u64
        .checked_shl(consecutive_failures.saturating_sub(1).min(31))
        .unwrap_or(MAX_SECONDS);
    Duration::from_secs(seconds.min(MAX_SECONDS))
}

/// tmux messages that mean this watcher has nothing left to watch: its pane is gone, or
/// the server that owned it is. Everything else a tmux call can fail with — a busy
/// server, a hit file-descriptor limit, a signal — is transient, and retrying is right.
///
/// Getting this wrong in either direction is bad: too broad retires a watcher over a
/// hiccup and the pane silently stops being tracked for the rest of its life; too narrow
/// spins forever on a pane that will never come back.
const GONE_MESSAGES: &[&str] = &[
    "can't find pane",
    "no server running",
    "error connecting to",
];

/// Whether a tmux error means there is nothing left to watch. Matched on the stderr text
/// `Tmux::run` puts in its error chain.
fn is_gone(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    GONE_MESSAGES.iter().any(|message| text.contains(message))
}

/// Whether a [`DISPLAY_FORMAT`] expansion describes no pane at all.
///
/// `display-message -p -t <missing pane>` **succeeds** on tmux 3.6b, and the format's
/// *literal* tabs survive the empty expansions: the answer is exactly
/// `DISPLAY_FIELDS - 1` tab bytes (verified on 3.6b with `focus-events` on and off), so
/// it is emphatically not an empty string and testing for one is testing for nothing.
///
/// What a missing target does give is every column blank, and only a missing target can:
/// a live pane always fills `#{window_active}`, `#{session_attached}`, `#{session_name}`
/// and `#{window_index}` — even one whose process has already exited. So "all columns
/// blank" is the signal, and it is checked here rather than left to
/// `capture-pane`'s `can't find pane` a call later: relying on that would make this
/// module's pane-gone handling depend on a second tmux call it does not need.
fn describes_no_pane(line: &str) -> bool {
    line.split('\t').all(|field| field.trim().is_empty())
}

/// When the watcher reads the pane: herdr v0.9.1's pane detection cadence (see
/// `next_read`).
///
/// Reads fall on a tick grid anchored at the previous read. Output since that read makes
/// the pane *eligible* for the next tick and does nothing else — it never sets or holds a
/// state, and no amount of it delays a read. A tick with nothing owed reads nothing.
///
/// * `identified` — the grid while an agent is in the pane (herdr's `TICK_IDENTIFIED`).
/// * `unidentified` — the grid for a pane with no agent, read only after output, so an
///   agentless pane that prints nothing costs nothing (`TICK_UNIDENTIFIED`).
/// * `hold_recheck` — the grid while a `working -> idle` verdict is on hold, read whether
///   or not anything printed (`AGENT_PENDING_IDLE_RECHECK`).
/// * `identity_recheck` — how long a quiet agent pane goes unread: the re-check that
///   notices a foreground process change no byte announced
///   (`PROCESS_RECHECK_IDENTIFIED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub identified: Duration,
    pub unidentified: Duration,
    pub hold_recheck: Duration,
    pub identity_recheck: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            identified: Duration::from_millis(300),
            unidentified: Duration::from_millis(500),
            hold_recheck: Duration::from_millis(100),
            identity_recheck: state::IDENTITY_RECHECK,
        }
    }
}

/// The read the watcher owes whether or not the pane prints anything: the arbiter's
/// ([`state::Arbiter::forced_read`]), else the next tick after a read that did not find the line an
/// agent's command was typed on ([`typed_line::Misses`]), which only a second read in a row takes for
/// gone: a pane whose agent has drawn and gone quiet may print nothing more for a while.
fn forced_read(arbiter: Option<Forced>, misses: &typed_line::Misses) -> Option<Forced> {
    arbiter.or_else(|| misses.owe_a_look().then_some(Forced::NextTick))
}

/// Whether a look that found no agent in the runtime `command` in front of the pane keeps `last`, the
/// agent the look before found: only when this look could not see into the runtime at all
/// (`unread`), has not been given up on, and `command` is still the runtime `last` ran in.
fn keeps_the_last_container(
    last: Option<&detect::ContainerAgent>,
    command: &str,
    unread: bool,
    given_up: bool,
) -> bool {
    unread
        && !given_up
        && last.is_some_and(|last| Some(last.runtime) == detect::container_runtime(command))
}

/// The earlier of two reads owed whether or not the pane prints: the next tick comes before any
/// instant after the last read, and of two instants, the first.
fn earliest(a: Option<Forced>, b: Option<Forced>) -> Option<Forced> {
    match (a, b) {
        (Some(Forced::NextTick), _) | (_, Some(Forced::NextTick)) => Some(Forced::NextTick),
        (Some(Forced::From(x)), Some(Forced::From(y))) => Some(Forced::From(x.min(y))),
        (one, other) => one.or(other),
    }
}

// Rules ported from herdr `src/pane.rs` and `src/pane/agent_detection.rs` @v0.9.1, rules
// unchanged at master `0d5d6f1`. Differences: tmux reads instead of an
// in-process terminal; a pane that printed nothing is re-read only when a rule owes it.
/// When the watcher should next read the pane, or `None` when nothing is owed until the
/// pane prints something. A due instant at or before `now` means "read now".
///
/// * The first read (`last_read` is `None`) is due at once.
/// * While a hold is active, the next read is `hold_recheck` after the last, output or
///   not.
/// * Otherwise reads fall on a grid of `identified` (an agent is present) or
///   `unidentified` (none is) anchored at `last_read`: output since the last read
///   (`eligible_since`, its first byte) is owed the first tick at or after it, and a
///   `forced` read the next tick or the first tick at or after its instant.
/// * An agent pane with nothing owed is still read `identity_recheck` after the last read;
///   an agentless one is not read at all.
pub(crate) fn next_read(
    now: Instant,
    last_read: Option<Instant>,
    timing: &Timing,
    eligible_since: Option<Instant>,
    hold: bool,
    forced: Option<Forced>,
    agent_present: bool,
) -> Option<Instant> {
    let Some(last) = last_read else {
        return Some(now);
    };
    if hold {
        // herdr re-reads every tick while a verdict is on hold, whether or not anything
        // printed: the confirmations are the point.
        return Some(last + timing.hold_recheck);
    }
    let period = if agent_present {
        timing.identified
    } else {
        timing.unidentified
    };
    // The first tick of the grid `last + k * period`, k >= 1, at or after `from`.
    let tick_from = |from: Instant| {
        let since = from.saturating_duration_since(last);
        let ticks = since.as_nanos().div_ceil(period.as_nanos().max(1)).max(1);
        last + period * u32::try_from(ticks).unwrap_or(u32::MAX)
    };
    let owed = [
        eligible_since.map(tick_from),
        forced.map(|forced| match forced {
            Forced::NextTick => tick_from(last),
            Forced::From(at) => tick_from(at),
        }),
        agent_present.then(|| last + timing.identity_recheck),
    ];
    owed.into_iter().flatten().min()
}

/// One pane's watcher state, for `taw watch status`.
#[derive(Clone, Debug)]
pub struct WatchStatus {
    pub pane: PaneInfo,
    /// tmux's own `#{pane_pipe}`: whether *any* `pipe-pane` is open on the pane.
    pub piped: bool,
    /// Whether the pid in `@agent_watch_pid` is a live process.
    pub watcher_alive: bool,
    /// How the taw that live watcher runs stands against the file where it started from (see
    /// [`binary`]). `None` when no watcher is alive, which has no taw to judge.
    pub binary: Option<Binary>,
}

impl WatchStatus {
    /// Whether `taw watch attach-all --restart` starts this watcher again: it runs an older taw
    /// than the one installed, and its pipe is open. A watcher whose pipe is closed was let go of
    /// (by `taw watch detach`, say) and is on its way out, and nobody asked for another on its
    /// pane. The one place that says so, for what `status` counts and for what a restart does.
    pub fn restartable(&self) -> bool {
        self.piped && self.binary.as_ref().is_some_and(Binary::is_old)
    }
}

/// What one call to [`attach`]/[`attach_all`] did to a pane. Both need to tell "already
/// watched" apart from "newly attached" — [`attach_all`] to report how many watchers it
/// *actually* opened, [`attach`] only to fold the first two into `false`/`true`.
enum AttachOutcome {
    /// `@agent-wizard-watch` is `off`; nothing was done.
    Disabled,
    /// The pane already had a pipe open; nothing was done.
    AlreadyWatched,
    /// No pipe was open; one was opened now.
    Attached,
}

/// The shared logic behind [`attach`] and [`attach_all`].
///
/// tmux 3.6b's `pipe-pane -o <cmd>` is **not** idempotent: called again with the exact
/// same command it *toggles* the pipe off instead of leaving it alone (verified on a
/// throwaway server — first call opens it, second closes it, third reopens it). That
/// makes `-o` unusable here, since both the per-pane hooks and `attach_all` at plugin
/// startup can legitimately call this on a pane that is already watched, and a silent
/// "off" would be indistinguishable from success.
///
/// So idempotency is done by hand instead: `#{pane_pipe}` is checked first, and
/// `pipe-pane` (with no `-o`) is only run when the pane has no pipe open yet. Restarting
/// an already-running watcher would replace its process — resetting whatever state it was
/// mid-evaluation with — so an already-piped pane is left alone rather than re-piped.
fn attach_inner(t: &Tmux, pane: &str, taw_bin: &Path) -> anyhow::Result<AttachOutcome> {
    if !watching_enabled(t) {
        debug!("{WATCH_OPTION} is off; not attaching a watcher to pane '{pane}'");
        return Ok(AttachOutcome::Disabled);
    }
    let piped = t
        .display(pane, "#{pane_pipe}")
        .with_context(|| format!("checking whether pane '{pane}' already has a watcher"))?;
    if piped == "1" {
        debug!("pane '{pane}' already has a pipe open; leaving its watcher alone");
        return Ok(AttachOutcome::AlreadyWatched);
    }
    let command = watch_run_command(t, taw_bin, pane)
        .with_context(|| format!("building the watcher command for pane '{pane}'"))?;
    t.run_ok(&["pipe-pane", "-t", pane, &command])
        .with_context(|| format!("opening a watcher pipe on pane '{pane}'"))?;
    Ok(AttachOutcome::Attached)
}

/// The `pipe-pane` shell command that starts the watcher on `pane`, pinned to the **same**
/// tmux server this [`Tmux`] talks to.
///
/// A `pipe-pane` child does not inherit a usable `$TMUX` — it is empty on Linux, and on
/// macOS it is the server's *launch* environment rather than the pane's — so without a pin
/// the spawned `taw watch run` would resolve [`Tmux::from_env`] to the default socket and
/// either track the wrong server or die with "pane not found", silently stopping all agent
/// tracking on any `-L`/`-S` server. The socket is baked in as a `TAW_TMUX_SOCKET_*` env
/// assignment, which `from_env` honours ahead of `$TMUX`, so the watcher reaches exactly
/// the server that owns the pipe. See [`socket_env_prefix`].
fn watch_run_command(t: &Tmux, taw_bin: &Path, pane: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{}{} watch run {}",
        socket_env_prefix(t)?,
        pipe_command_word(&taw_bin.to_string_lossy()),
        pipe_command_word(pane)
    ))
}

/// The `VAR=value ` shell prefix (with a trailing space) that pins a spawned `taw` to the
/// server `t` talks to, or the empty string when `t` uses the default socket and its path
/// cannot be resolved (in which case the child's own default resolution is the only
/// fallback left). A `Name`/`Path` socket is baked in directly; a `None` (default) socket
/// is resolved to its concrete path once via `#{socket_path}` so that a child which
/// inherited a stale or foreign `$TMUX` still lands on this exact server. Both the variable
/// name (a fixed ASCII literal) and the value pass through [`pipe_command_word`], so tmux's
/// `#`/`%` expansion and the shell both see the intended bytes.
fn socket_env_prefix(t: &Tmux) -> anyhow::Result<String> {
    let (var, value) = match &t.socket {
        Some(Socket::Name(name)) => ("TAW_TMUX_SOCKET_NAME", name.clone()),
        Some(Socket::Path(path)) => ("TAW_TMUX_SOCKET_PATH", path.to_string_lossy().into_owned()),
        None => {
            let path = t
                .run(&["display-message", "-p", "#{socket_path}"])
                .context("resolving the default tmux socket path to pin the watcher")?;
            let path = path.trim().to_string();
            if path.is_empty() {
                return Ok(String::new());
            }
            ("TAW_TMUX_SOCKET_PATH", path)
        }
    };
    Ok(format!("{var}={} ", pipe_command_word(&value)))
}

/// Opens a watcher pipe on `pane`, unless `@agent-wizard-watch` is `off` or the pane is
/// already watched (in which case this leaves it alone — see `attach_inner` for why).
/// Returns `Ok(false)` only when watching is disabled; `Ok(true)` both when a pipe was
/// newly opened and when the pane already had one.
pub fn attach(t: &Tmux, pane: &str, taw_bin: &Path) -> anyhow::Result<bool> {
    Ok(!matches!(
        attach_inner(t, pane, taw_bin)?,
        AttachOutcome::Disabled
    ))
}

/// [`attach`] for every pane of the server; returns how many watchers were **newly**
/// opened. A pane that was already watched is left alone by `attach_inner` and does not
/// count — which is what makes a second `attach_all` (the plugin's startup path, run
/// after the per-pane hooks have already attached each pane individually) report `0`
/// rather than re-counting panes it never touched.
///
/// A pane that cannot be piped (it closed mid-sweep, say) is logged and skipped: one bad
/// pane must not cost the rest of the server its watchers.
pub fn attach_all(t: &Tmux, taw_bin: &Path) -> anyhow::Result<usize> {
    if !watching_enabled(t) {
        debug!("{WATCH_OPTION} is off; not attaching any watchers");
        return Ok(0);
    }
    let mut attached = 0;
    for pane in t
        .list_panes()
        .context("listing panes to attach watchers to")?
    {
        match attach_inner(t, &pane.pane_id, taw_bin) {
            Ok(AttachOutcome::Attached) => attached += 1,
            Ok(AttachOutcome::AlreadyWatched | AttachOutcome::Disabled) => {}
            Err(err) => warn!(
                "could not attach a watcher to pane '{}': {err:#}",
                pane.pane_id
            ),
        }
    }
    Ok(attached)
}

/// Closes the pipe on `pane` (`pipe-pane` with no command), which sends the watcher EOF.
pub fn detach(t: &Tmux, pane: &str) -> anyhow::Result<()> {
    t.run_ok(&["pipe-pane", "-t", pane])
        .with_context(|| format!("closing the watcher pipe on pane '{pane}'"))
}

/// How long [`restart_old`] waits for the watchers it let go of to leave before it starts their
/// successors. They leave within a read. The wait keeps two watchers from reading one pane at
/// once, and it keeps a watcher of a taw from before [`binary`] from erasing its successor: it
/// clears the pid in `@agent_watch_pid` without asking whose it is ([`release_record`] asks), as
/// the first thing it does on the way out. One that is later than this takes its successor's pid
/// with it, and the successor goes on reading without one, which a report's nudge then cannot find.
const RESTART_WAIT: Duration = Duration::from_secs(3);

/// A watcher [`restart_old`] has let go of, and what it needs to start the next one.
struct Leaving {
    pane: String,
    pid: u32,
    /// The taw to start its successor from.
    from: PathBuf,
}

/// What [`restart_old`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restart {
    /// The old watchers started again.
    pub restarted: usize,
    /// The old watchers let go of whose successors could not be started, because tmux refused
    /// or because watching was turned off meanwhile: their panes have no watcher now.
    pub failed: usize,
}

/// Starts again every watcher that runs an older taw than the one installed ([`binary`]), and
/// returns what it did ([`Restart`]): how many it started, and how many it let go of and could not
/// start, which leaves their panes with none. Each is let go of by closing its pipe, which gives
/// it EOF, and its successor is opened from the file the old one started from, so that it runs
/// the taw there now whichever `taw` ran this; `taw_bin`, this one, is used only for a watcher
/// that has no such file. A pane that has a pipe of someone else's, or no live watcher, is left
/// as it is, and so is every watcher on the installed taw.
///
/// Nothing is done while `@agent-wizard-watch` is `off`: that is no watchers at all, and
/// closing the old ones' pipes would leave their panes with none to open in their place.
pub fn restart_old(t: &Tmux, taw_bin: &Path) -> anyhow::Result<Restart> {
    restart_old_waiting(t, taw_bin, RESTART_WAIT)
}

/// [`restart_old`], waiting at most `wait` for the old watchers to leave, so that a test can
/// give it all the time there is, or none.
pub fn restart_old_waiting(t: &Tmux, taw_bin: &Path, wait: Duration) -> anyhow::Result<Restart> {
    if !watching_enabled(t) {
        debug!("{WATCH_OPTION} is off; not restarting any watchers");
        return Ok(Restart::default());
    }
    let mut leaving = Vec::new();
    for watcher in status(t)? {
        let (Some(binary), Some(pid)) = (&watcher.binary, watcher.pane.agent.watch_pid) else {
            continue;
        };
        if !watcher.restartable() {
            continue;
        }
        let pane = watcher.pane.pane_id.clone();
        match detach(t, &pane) {
            Ok(()) => leaving.push(Leaving {
                pane,
                pid,
                from: binary.restart_from(taw_bin),
            }),
            Err(err) => warn!("could not let go of the watcher of pane '{pane}': {err:#}"),
        }
    }
    await_departure(t, &leaving, wait);

    let mut restart = Restart::default();
    for old in &leaving {
        match attach_inner(t, &old.pane, &old.from) {
            Ok(AttachOutcome::Attached) => restart.restarted += 1,
            // Somebody else opened a watcher on the pane meanwhile: it has one.
            Ok(AttachOutcome::AlreadyWatched) => {}
            // Watching was turned off after the check at the start. The old watcher is gone, so
            // the pane has none, as when tmux refuses.
            Ok(AttachOutcome::Disabled) => {
                restart.failed += 1;
                warn!(
                    "watching was turned off while the watcher of pane '{}' was started again; \
                     the pane has none now",
                    old.pane
                );
            }
            Err(err) => {
                restart.failed += 1;
                warn!(
                    "could not start the watcher of pane '{}' again: {err:#}",
                    old.pane
                );
            }
        }
    }
    Ok(restart)
}

/// Waits, for at most `wait`, until each of the `leaving` watchers has gone from its pane: its
/// pid is no longer in `@agent_watch_pid` (a watcher clears it as it leaves) or the process has
/// ended.
fn await_departure(t: &Tmux, leaving: &[Leaving], wait: Duration) {
    if leaving.is_empty() {
        return;
    }
    let deadline = Instant::now() + wait;
    loop {
        // A listing that fails says nothing of who has left: they are taken to be there until
        // one that works says otherwise, or the time is up.
        let left = match t.run(&["list-panes", "-a", "-F", "#{pane_id}\t#{@agent_watch_pid}"]) {
            Ok(pids) => {
                let held: HashMap<&str, &str> = pids
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .collect();
                !leaving.iter().any(|old| {
                    held.get(old.pane.as_str())
                        .is_some_and(|held| *held == old.pid.to_string())
                        && process_alive(old.pid)
                })
            }
            Err(err) => {
                debug!("could not list who is on the panes: {err:#}");
                false
            }
        };
        if left {
            return;
        }
        if Instant::now() >= deadline {
            debug!("a watcher was still leaving after {wait:?}; starting its successor");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What [`status`] reads of every pane besides the agent record: whether a pipe is open, and
/// what the watcher on it wrote down about the taw it runs ([`binary::OPTION`], folded as the
/// listing's other free text is, so that a path with a tab in it cannot break a row).
const STATUS_FORMAT: &str = concat!("#{pane_id}\t#{pane_pipe}\t", folded!("@agent_watch_build"));

/// Every pane on the server, with whether it is piped, whether its recorded watcher is still
/// alive, and for a live one whether its taw is the one installed. The first two can
/// disagree, which is the point: a `piped` pane with a dead pid means a watcher died, and a
/// live pid with no pipe means one was detached.
pub fn status(t: &Tmux) -> anyhow::Result<Vec<WatchStatus>> {
    let panes = t.list_panes().context("listing panes for watch status")?;
    let out = t
        .run(&["list-panes", "-a", "-F", STATUS_FORMAT])
        .context("listing pane pipe state")?;
    let mut piped: HashMap<&str, bool> = HashMap::new();
    let mut records: HashMap<&str, binary::Record> = HashMap::new();
    for line in out.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(pane_id), Some(pipe)) = (fields.next(), fields.next()) else {
            continue;
        };
        piped.insert(pane_id, pipe == "1");
        if let Some(record) = fields.next().and_then(binary::Record::parse) {
            records.insert(pane_id, record);
        }
    }

    Ok(panes
        .into_iter()
        .map(|pane| {
            let pid = pane.agent.watch_pid.filter(|pid| process_alive(*pid));
            WatchStatus {
                piped: piped.get(pane.pane_id.as_str()).copied().unwrap_or(false),
                watcher_alive: pid.is_some(),
                binary: pid.map(|pid| {
                    binary::judge(
                        records.get(pane.pane_id.as_str()),
                        pid,
                        binary::Identity::of,
                    )
                }),
                pane,
            }
        })
        .collect())
}

/// The watcher loop, reading the pane's output from this process's stdin until EOF.
///
/// This is what `taw watch run <pane>` runs, and what tmux keeps alive for as long as the
/// pipe is open. It returns `Ok(())` on EOF (the pane closed, or the pipe was removed)
/// and when the pane stops existing.
pub fn run(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    pane: &str,
    timing: Timing,
) -> anyhow::Result<()> {
    run_with_reader(
        t,
        cfg,
        paths,
        taw_bin,
        pane,
        timing,
        Box::new(std::io::stdin()),
    )
}

/// [`run`], reading pane output from `reader` instead of stdin — the seam the integration
/// tests drive the loop through (a `std::io::pipe()` they hold the writing end of).
pub fn run_with_reader(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    taw_bin: &Path,
    pane: &str,
    timing: Timing,
    reader: Box<dyn Read + Send>,
) -> anyhow::Result<()> {
    // Prefer the pane's own `%N` id over whatever target string we were started with: it
    // is stable, unambiguous, and what the plugin payloads are specified to carry.
    let pane = resolve_pane_id(t, pane);
    let pid = std::process::id();
    info!(
        "watching pane {pane} (pid {pid}, tick {:?} with an agent, {:?} without, {:?} on \
         hold, identity recheck {:?})",
        timing.identified, timing.unidentified, timing.hold_recheck, timing.identity_recheck
    );
    // Listening for the nudge of a report before the pid that it is sent to is published, so that
    // no report that finds the pid finds a watcher that cannot hear it.
    let (tx, rx) = mpsc::channel::<Wake>();
    nudge::subscribe(tx.clone());
    // Which taw this is, written before the pid, so that no reader that finds the pid finds a
    // watcher that has said nothing of itself (see [`binary`]). Read now, while the file at the
    // path is the one that was just started.
    match binary::Record::of(pid, taw_bin) {
        Some(record) => write_record(t, &pane, &record),
        None => warn!(
            "could not read {} to record which taw this watcher runs",
            taw_bin.display()
        ),
    }
    if let Err(err) = t.set_pane_option(&pane, WATCH_PID_OPTION, &pid.to_string()) {
        warn!("could not record {WATCH_PID_OPTION} on pane {pane}: {err:#}");
    }

    let store = ManifestStore::new(
        paths.override_dir.clone(),
        paths.remote_manifest_dir.clone(),
    )
    .with_supplements(cfg.detection.supplements);
    let manifests_stale = Arc::new(AtomicBool::new(false));
    if cfg.update.manifest_check {
        spawn_manifest_refresh(
            store.clone(),
            paths.manifest_marker.clone(),
            Arc::clone(&manifests_stale),
        );
    }

    // The reader thread exists so a blocking read can never delay an evaluation: it does
    // nothing but hand chunks over and let the loop decide when to look at the pane.
    std::thread::spawn(move || pipe_reader(reader, tx));

    let mut watcher = Watcher {
        t,
        cfg: cfg.clone(),
        paths,
        taw_bin,
        pane: &pane,
        timing,
        store,
        manifests: HashMap::new(),
        manifests_stale,
        notifications: Vec::new(),
        agent_present: false,
        arbiter: state::Arbiter::new(timing.identity_recheck),
        last_read: None,
        last_capture: None,
        exited_under: None,
        misses: typed_line::Misses::default(),
        container: container::Probe::new(timing.identity_recheck),
        job: job::Probe::new(timing.identity_recheck),
        foreground: container::Foreground::default(),
        note: suspend::Note::default(),
        note_started: false,
        container_last: None,
        container_given_up: false,
        container_written: None,
        container_announced: None,
        announced_profile: None,
        announced_kind: None,
        listing: None,
        seeded: false,
        mark: crate::sandbox::mark::Mark::default(),
        server: None,
        report_seen: None,
        report_on_the_pane: false,
        // Primed from what is on disk *now*, so the first evaluation does not report a
        // reload of the config this call was just handed.
        reload: reload::Tracker::new(
            &paths.config_file,
            &paths.override_dir,
            &paths.remote_manifest_dir,
            &t.get_global_option(reload::RELOAD_OPTION)
                .unwrap_or_default()
                .unwrap_or_default(),
        ),
        server_env: Arc::new(ServerEnvCache::default()),
    };
    watcher.pump(&rx);

    // Shutdown, in this order: drop the pid first so `watch status` stops advertising a
    // watcher that is on its way out, then let any pending notification threads finish
    // (they are short: a `delay_seconds` sleep and a re-check).
    if let Err(err) = release_record(t, &pane, pid) {
        debug!("could not clear {WATCH_PID_OPTION} on pane {pane}: {err:#}");
    }
    for handle in watcher.notifications.drain(..) {
        let _ = handle.join();
    }
    info!("stopped watching pane {pane}");
    Ok(())
}

/// Writes `record` to `pane`, and once more if tmux refuses it: a watcher that has said nothing
/// of itself is taken for one of an older taw for as long as it runs.
fn write_record(t: &Tmux, pane: &str, record: &binary::Record) {
    for attempt in 1..=2 {
        match t.set_pane_option(pane, binary::OPTION, &record.argument()) {
            Ok(()) => return,
            Err(err) => warn!(
                "could not record {} on pane {pane} (try {attempt} of 2): {err:#}",
                binary::OPTION
            ),
        }
    }
}

/// Takes the pid and the record of the watcher `pid` off `pane`, unless the pane has another
/// watcher's by now. A restart closes the old watcher's pipe and opens the new one's straight
/// away, and the old watcher is still on its way out when the new one has written its pid: what
/// it clears is only what is still its own, in the one tmux command that checks.
fn release_record(t: &Tmux, pane: &str, pid: u32) -> anyhow::Result<()> {
    let ours = format!("#{{==:#{{{WATCH_PID_OPTION}}},{pid}}}");
    let drops = format!(
        "set-option -p -u -t {pane} {WATCH_PID_OPTION} ; set-option -p -u -t {pane} {}",
        binary::OPTION
    );
    t.run_ok(&["if-shell", "-F", "-t", pane, &ours, &drops])
        .with_context(|| format!("clearing the pid of the watcher of pane '{pane}'"))
}

/// Drops what the last agent's own hook reported for `pane`: the session id and path, its word
/// that it was there (`@agent_reported_kind`), and the command it reported that resumes its
/// session. They were about that agent, and a session restored from them would resume its
/// conversation under whoever took the pane. `saw` is the session id the read that found the
/// replacement saw on the pane, and `stamp` the report stamp it saw (`@agent_report_stamp`,
/// empty for none), and the drops are made only while the pane still holds both, in the one tmux
/// command that checks: the agent that took the pane may have reported since that read, with its
/// own session or its own command, and that is not the last one's to lose. A pane that held no
/// session is checked for holding none. Says whether they were dropped.
pub fn release_identity(
    t: &Tmux,
    pane: &str,
    saw: Option<&str>,
    stamp: &str,
) -> anyhow::Result<bool> {
    let condition = match saw {
        Some(id) if crate::session::restore::valid_session_id(id) => {
            format!("#{{==:#{{@agent_session_id}},{id}}}")
        }
        // Not an id a report can have written: nothing to compare it with.
        Some(_) => "1".to_string(),
        None => "#{==:#{@agent_session_id},}".to_string(),
    };
    store::drop_identity(t, pane, &condition, stamp)
}

/// `taw seen <pane>`: records that the user has looked at the pane.
///
/// tmux calls this from its focus hooks (`after-select-pane`, `pane-focus-in`, …). Its
/// one job beyond setting `@agent_seen` is releasing `done` back to `idle`: `done` exists
/// only to say "finished while you were not looking", so looking is what ends it. The
/// rule and source are kept — the state did not change because the *screen* changed, so
/// the evidence behind it still stands.
///
/// `cfg`/`paths` are passed in rather than read from the environment: this runs from a
/// tmux hook on every focus change, and a library function that quietly resolves its own
/// config cannot be told to use a different one (nor tested against one).
pub fn mark_seen(t: &Tmux, cfg: &Config, paths: &Paths, pane: &str) -> anyhow::Result<()> {
    let record = store::read(t, pane)
        .with_context(|| format!("reading the agent record of pane '{pane}'"))?;
    let Some(seen) = state::marked_seen(&record) else {
        debug!("`taw seen` on pane '{pane}': no agent there, nothing to mark");
        return Ok(());
    };
    store::set_seen(t, pane, true)?;
    let Some(to) = seen.state.filter(|to| record.state != Some(*to)) else {
        return Ok(());
    };
    store::write_state(
        t,
        pane,
        to,
        record.rule.as_deref(),
        record.source.as_deref().unwrap_or(MANIFEST_SOURCE),
        unix_seconds(),
    )?;
    rail::refresh(t, cfg)?;
    // Same as any other state change: the saved snapshot is now behind.
    if let Err(err) = crate::session::autosave_touch(paths) {
        warn!("requesting a session autosave failed: {err:#}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Borrowed context plus the little mutable state one watcher accumulates.
struct Watcher<'a> {
    t: &'a Tmux,
    /// Owned, not borrowed: a watcher lives for days and re-reads `config.toml` when it
    /// changes (see [`reload`]), so the settings in force are its own state.
    cfg: Config,
    paths: &'a Paths,
    taw_bin: &'a Path,
    pane: &'a str,
    timing: Timing,
    store: ManifestStore,
    /// Compiled manifests, per agent kind: a pane's occupant rarely changes, and
    /// compiling a manifest on every evaluation would dwarf the rest of the work.
    manifests: HashMap<String, LoadedManifest>,
    /// Set by the manifest refresh thread when it replaced a cached manifest, so
    /// `manifests` can be dropped and recompiled from the new rules.
    manifests_stale: Arc<AtomicBool>,
    /// Threads waiting out `[ui.toast] delay_seconds` before delivering.
    notifications: Vec<JoinHandle<()>>,
    /// Whether the last read found an agent: what picks the tick grid, and whether a quiet
    /// pane is re-checked at all.
    agent_present: bool,
    /// herdr's per-pane arbitration state, carried between reads (see [`state::Arbiter`]).
    arbiter: state::Arbiter,
    /// When the last read finished, `None` before the first: the anchor of the tick grid.
    last_read: Option<Instant>,
    /// The last `capture-pane`, reused by a quiet read that finds the same foreground
    /// command and hint (see [`Watcher::screen`]).
    last_capture: Option<Capture>,
    /// The foreground command under which the last read found a hinted agent's recorded
    /// process gone (see [`Watcher::hint_only_agent_exited`]).
    exited_under: Option<String>,
    /// The line `taw agent start` typed the agent's command on, when the last read looked for it and did
    /// not find it (see [`Watcher::read_below_the_typed_line`]).
    misses: typed_line::Misses,
    /// The agent in the container runtime in front of the pane, read off its arguments once
    /// per process (see [`container::Probe`]), and which process that is (see
    /// [`container::Foreground`]).
    container: container::Probe,
    /// The job in front of the pane, read when tmux names a shell or a runtime (`node`, `sh`).
    job: job::Probe,
    foreground: container::Foreground,
    /// The process of the pane's agent, as a read found it in front of the pane: what a suspend is
    /// told by ([`Watcher::suspended_agent`]).
    note: suspend::Note,
    /// Whether the note the pane kept ([`store::PROCESS_OPTION`]) has been read back, which the
    /// watcher's first read does.
    note_started: bool,
    /// The agent the last look found in the runtime in front of the pane, which a look that could
    /// not see into that runtime keeps ([`Watcher::kept_container`]), and whether the look at it has
    /// been given up on, which ends that.
    container_last: Option<detect::ContainerAgent>,
    container_given_up: bool,
    /// The runtime `@agent_container` was last set to by this watcher, `None` when it is
    /// unset: the option is written when this changes, and never once per read.
    container_written: Option<String>,
    /// The runtime the last read that was written whole left the pane marked with: what a read's
    /// container is compared with to say it came or left (`sandbox::mark`). A read whose writes failed
    /// part of the way is decided again, and its change with it.
    container_announced: Option<String>,
    /// Whether this watcher has read the pane yet: its first read takes the runtime the pane is
    /// marked with as the one it found last, so that a watcher started again takes no container
    /// already there for one that came.
    seeded: bool,
    /// What the watcher keeps of the pane's profile mark between reads (`sandbox::mark`).
    mark: crate::sandbox::mark::Mark,
    /// The tmux server's name, which the pane's report folder is kept by (`sandbox::report`), once
    /// tmux has said it.
    server: Option<String>,
    /// What the last look at the pane's sandbox report found (`sandbox::report`), `None` before
    /// the first and once the pane has lost the session it gave: a report is taken once each time it
    /// changes, and again after any drop of what the agent reported of itself.
    report_seen: Option<crate::sandbox::report::Look>,
    /// Whether the session of the report last looked at is on the pane: written, or there already.
    /// A read that then finds no session on the pane has seen it dropped: by a container that came
    /// (again, after a look that gave up), another agent, the agent gone, the mark's
    /// end, or anyone else.
    report_on_the_pane: bool,
    /// The profile and the agent the read that announced [`Watcher::container_announced`] found, and
    /// the pane as it was last listed while that container was in it: what `sandbox.stopped` is told
    /// when the container ends with its pane, which leaves no pane to read or list.
    announced_profile: Option<String>,
    announced_kind: Option<String>,
    listing: Option<PaneInfo>,
    /// `config.toml` / manifest-override staleness, checked once per read.
    reload: reload::Tracker,
    /// tmux's global environment, which every `[hooks]` command this watcher starts is
    /// given. A watcher is a `pipe-pane` child, so it has only the environment the server
    /// was launched with, and a variable set with `set-environment -g` would never reach a
    /// hook. Read the first time a hook needs it, and again after a reload of the config
    /// (`taw config reload`), not for every event. Shared with the notification threads.
    server_env: Arc<ServerEnvCache>,
}

/// A screen as `capture-pane` returned it, with the identity inputs it was taken under.
struct Capture {
    command: String,
    kind_hint: Option<String>,
    screen: Screen,
}

/// A pane's screen, with what was read of the pane in the same tmux invocation to say where the line its
/// agent's command was typed on is on it: the size of the history and whether the alternate screen is
/// on. Read in another invocation, output in between could scroll the screen, and the line would be
/// looked for a row off.
#[derive(Clone, Debug)]
struct Screen {
    text: String,
    history: u64,
    alternate: bool,
}

/// What wakes the loop from outside it.
pub(crate) enum Wake {
    /// A chunk of the pane's output, from the pipe tmux keeps open.
    Output(Vec<u8>),
    /// A report was made for the pane (see [`nudge`]): it is owed a read though it printed nothing.
    Nudge,
    /// The pipe is closed: the pane is gone, or something removed the pipe, and there is nothing
    /// left to watch. Said by the reader, since the channel is not closed with it: the nudge keeps
    /// a sender of its own for as long as the process lives.
    Closed,
}

/// Whether a pane is read on the agent's timing though it prints nothing: an agent is there, or a
/// container runtime whose arguments, or a job in front, could not be read yet may be running one,
/// and is looked at again as an agent's pane is.
fn read_while_quiet(agent_present: bool, container: &container::Probe, job: &job::Probe) -> bool {
    agent_present || container.wants_recheck() || job.wants_recheck()
}

/// Whether the loop should keep going after an evaluation.
enum Step {
    Continue,
    PaneGone,
}

impl Watcher<'_> {
    /// Blocks on the byte channel, reading the pane whenever [`next_read`] says a read is
    /// due, until the pipe closes or the pane stops existing.
    fn pump(&mut self, rx: &Receiver<Wake>) {
        let mut stream = StreamState::default();
        let mut failures: u32 = 0;
        // Set after a failed read: a due read waits for this before retrying.
        let mut retry_after: Option<Instant> = None;

        loop {
            let now = Instant::now();
            // `self.last_read` is `None` until the first read succeeds, which makes that
            // read due at once: a watcher attached to a pane that is *already* running an
            // agent must classify it without waiting for it to say something, and a
            // failed first look must be retried or a watcher that started while tmux was
            // busy would never classify it. A failed read leaves `last_read` and the
            // stream alone, so it stays owed and is retried once the backoff is over.
            let due = next_read(
                now,
                self.last_read,
                &self.timing,
                stream.eligible_since,
                self.arbiter.hold_active(),
                // A marked pane whose profile still runs with no container in front is read again a
                // second after the last read, though it prints nothing (`sandbox::mark`).
                earliest(
                    forced_read(self.arbiter.forced_read(), &self.misses),
                    self.mark.recheck(self.last_read).map(Forced::From),
                ),
                read_while_quiet(self.agent_present, &self.container, &self.job),
            )
            .map(|due| retry_after.map_or(due, |retry| due.max(retry)));

            if due.is_some_and(|due| due <= now) {
                retry_after = None;
                debug!(
                    "reading pane {} ({} bytes since the last read, the last at {:?})",
                    self.pane, stream.bytes_since_eval, stream.last_output_at
                );
                match self.evaluate(&stream) {
                    Ok(Step::Continue) => {
                        stream.mark_read();
                        if self.arbiter.take_progress_reset() {
                            // The agent that printed it has left the pane: its successor
                            // starts with no progress of its own.
                            stream.forget_progress();
                        }
                        // Measured from after the work, so a slow read cannot eat into
                        // the interval to the next one.
                        self.last_read = Some(Instant::now());
                        failures = 0;
                    }
                    Ok(Step::PaneGone) => {
                        info!("pane {} no longer exists; stopping", self.pane);
                        self.end_with_the_pane(true);
                        return;
                    }
                    Err(err) => {
                        failures += 1;
                        let delay = backoff_delay(failures);
                        warn!(
                            "reading pane {} failed ({failures} in a row, retrying in {delay:?}): {err:#}",
                            self.pane
                        );
                        retry_after = Some(Instant::now() + delay);
                    }
                }
                continue;
            }

            // `None` means nothing is owed: no output since the last read and no agent to
            // re-check, so block until the pane says something. This is why an agentless
            // pane costs nothing.
            let received = match due {
                Some(due) => rx.recv_timeout(due.saturating_duration_since(now)),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(Wake::Output(chunk)) => {
                    if stream.feed(&chunk, Instant::now()) {
                        debug!("pane {} osc progress: {}", self.pane, stream.osc_progress);
                    }
                }
                Ok(Wake::Nudge) => {
                    debug!("pane {} nudged by a report", self.pane);
                    stream.nudge(Instant::now());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Ok(Wake::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    info!("pipe closed for pane {}", self.pane);
                    self.end_with_the_pane(false);
                    return;
                }
            }
        }
    }

    /// One read of the pane: a `display-message`, a `capture-pane` when an agent's screen
    /// can decide anything — in the same tmux invocation when the read expects one — one
    /// decision, and whatever writing that decision implies.
    fn evaluate(&mut self, stream: &StreamState) -> anyhow::Result<Step> {
        // A background refresh may have replaced the rules under us. A watcher lives for
        // days; a compiled manifest from Tuesday must not outlive Tuesday's update.
        if self.manifests_stale.swap(false, Ordering::Relaxed) {
            info!(
                "manifests were updated; dropping {} cached",
                self.manifests.len()
            );
            self.manifests.clear();
        }

        let (line, prefetched) = match self.display(self.expects_capture(stream)) {
            // tmux 3.6b answers `display-message -p -t <missing pane>` with *success*,
            // not an error, so this — not an `Err` — is what a gone pane actually looks
            // like from here. See [`describes_no_pane`] for what the answer contains.
            Ok((line, _)) if describes_no_pane(&line) => {
                debug!("pane {} resolved to nothing", self.pane);
                return Ok(Step::PaneGone);
            }
            Ok(read) => read,
            Err(err) if is_gone(&err) => {
                debug!("pane {} is gone: {err:#}", self.pane);
                return Ok(Step::PaneGone);
            }
            // Anything else is a tmux call that failed, not a pane that vanished: a busy
            // server, a hit resource limit, a signal. Retried with backoff by `pump`.
            Err(err) => return Err(err.context(format!("reading pane {}", self.pane))),
        };
        let mut observation = Observation::parse(
            &line,
            String::new(),
            &stream.osc_progress,
            crate::tmux::default_pane_title().as_deref(),
        )?;
        // The first read takes up from the runtime the pane is marked with, by this watcher's
        // predecessor if it had one: a container already there is no container that came. The agent
        // the pane records is the one that predecessor found in it, which a first look that cannot
        // see into the runtime keeps, as a look that had seen it before would.
        if !self.seeded {
            self.container_written = observation.container_mark.clone();
            self.container_announced = observation.container_mark.clone();
            self.container_last = observation
                .container_mark
                .as_deref()
                .and_then(detect::container_runtime)
                .zip(observation.observed.prev.kind.clone())
                .map(|(runtime, kind)| detect::ContainerAgent { runtime, kind });
            self.announced_profile = observation
                .container_mark
                .as_ref()
                .and(observation.sandbox.clone());
            self.announced_kind = observation
                .container_mark
                .as_ref()
                .and(observation.observed.prev.kind.clone());
            self.seeded = true;
        }
        // A container runtime in front of the pane names no agent itself: its arguments do,
        // and they are read once per process, not once per read.
        observation.observed.container = {
            let command = observation.observed.command.as_str();
            let (t, pane) = (self.t, self.pane);
            let look = self.container.agent(
                command,
                Instant::now(),
                || self.foreground.pid(t, pane),
                |pid| container::read_args(pid, command),
            );
            if look.gave_up {
                // Never the arguments: they can hold credentials.
                warn!(
                    "pane {}: could not read what {command} was started with, {} tries in a row; \
                     taking it as running no agent, and trying again in {:?}",
                    self.pane,
                    container::ATTEMPTS,
                    self.timing.identity_recheck
                );
            }
            self.kept_container(command, look)
        };
        // A shell or a runtime in front of the pane (`node /path/to/bin/codex`, `sh /path/to/codex`)
        // names no agent itself: the job it leads does, read once for each leader. A shell at its
        // prompt, the pane's own process, has no job read. The log tells the job's answer when it
        // differs from the read before it, not at every read. Never the arguments in the log: they
        // can hold credentials.
        observation.observed.job_agent = {
            let command = observation.observed.command.as_str();
            // A hook-owned record ends when the shell is back, which this answer decides: a prompt
            // seen before is asked about again, since a script under the shell's own name may lead now.
            state::forget_prompt_of_hook_owned(&mut self.job, &observation.observed);
            let (t, pane) = (self.t, self.pane);
            let foreground = &mut self.foreground;
            let agent = self.job.agent(
                command,
                Instant::now(),
                || {
                    let leader = foreground.pid(t, pane)?;
                    Some((leader, foreground.shell()?))
                },
                job::job_of_group,
                job::still_in_group,
            );
            if self.job.answer_changed() {
                match agent.as_deref() {
                    Some(agent) => info!(
                        "pane {}: the job in front of it ({command}) runs {agent}",
                        self.pane
                    ),
                    None => info!("pane {}: the job in front of it names no agent", self.pane),
                }
            }
            agent
        };
        // A hint-started agent that has exited leaves its last screen behind, which may
        // still match a rule — so `decide`'s own `unknown` fallback never fires and the pane
        // reports a live idle agent for ever. The recorded start pid settles the one thing a
        // single screen cannot (exited vs mid-redraw); passing that to `decide` lets it drop
        // the stale evidence and report `unknown` — keeping the hint rather than
        // wiping an identity that deliberately outlives its occupant.
        let agent_exited = self.hint_only_agent_exited(&observation)?;
        observation.agent_exited = agent_exited;
        observation.observed.suspended = self.suspended_agent(&observation.observed);
        // The shell of a hinted agent the last read already found gone, under the same
        // command: its screen can say nothing about an agent the dead pid has settled, so
        // the user working in that shell costs no capture per read.
        let settled_exit = agent_exited
            && self.exited_under.as_deref() == Some(observation.observed.command.as_str());
        self.exited_under = agent_exited.then(|| observation.observed.command.clone());
        // The command the agent reported that resumes its session goes with it (herdr drops it when the
        // agent's process exits): the pane keeps its hint, and its record, for the next agent, and a
        // restore must not bring back the session of one that has left.
        if agent_exited
            && !settled_exit
            && !store::drop_resume(self.t, self.pane, &observation.report_stamp)?
        {
            info!(
                "pane {}: an agent reported in it since it was read; the command it reported stays",
                self.pane
            );
        }
        // Only a pane an agent may occupy has a screen worth capturing: without one,
        // `decide` answers from the display alone.
        if state::wants_screen(&observation.observed, &self.arbiter) && !settled_exit {
            let displayed = (observation.history, observation.alternate);
            match self.screen(stream, &observation.observed, prefetched, displayed) {
                Ok(screen) => {
                    observation.observed.screen = screen.text;
                    observation.history = screen.history;
                    observation.alternate = screen.alternate;
                }
                Err(err) if is_gone(&err) => {
                    debug!("pane {} is gone: {err:#}", self.pane);
                    return Ok(Step::PaneGone);
                }
                Err(err) => return Err(err.context(format!("capturing pane {}", self.pane))),
            }
            self.read_below_the_typed_line(&mut observation);
        } else {
            // A capture is only reusable by the read straight after it: whatever the pane
            // printed while it went uncaptured is not in it.
            self.last_capture = None;
            self.misses.looked_away();
        }
        self.apply_reload(&observation.reload_token);
        // A pane whose agent reports for itself and that no process names is kept until the
        // pane's own shell is idle, which only the process table says.
        if self
            .arbiter
            .wants_shell_probe(&observation.observed, Instant::now())
        {
            observation.observed.pane_shell_idle = self.pane_shell_idle();
        }

        let checkpoint = self.arbiter.checkpoint();
        let decision = decide(
            &self.store,
            &mut self.manifests,
            &observation.observed,
            agent_exited,
            Instant::now(),
            &mut self.arbiter,
        );
        // The agent whose command `agent start` typed has gone: the line it was typed on is nobody's. Not
        // while a new agent's grace runs, in which nothing it does is believed: the pid the verb leaves when
        // its budget runs out with the agent still coming up is what was in front of the pane then, which
        // can be a helper the shell ran before the command (a slow preexec hook's), dead a moment later and
        // before the agent has a process of its own, and with the line gone the frame above it would be
        // read once the grace is over.
        if agent_exited
            && !self.arbiter.in_startup_grace()
            && let Some(line) = &observation.typed_line
        {
            self.forget_typed_line(line);
        }
        debug!("pane {} decision: {decision:?}", self.pane);
        if let Err(err) = self.apply(decision, &observation) {
            // What the read found was not written, or not announced: the read that retries it has
            // to find it again, as a change, and not as the state the pane already holds; and the
            // agent it took, or the session it saw, as news.
            self.arbiter.restore(checkpoint);
            return Err(err);
        }
        Ok(Step::Continue)
    }

    /// The agent in the container runtime in front of the pane, as this read takes it from `look`:
    /// what the look found; or, while the runtime `command` is still in front and could not be looked
    /// into this time (a `ps` that failed under load), the agent last found in that runtime, until the
    /// look has been given up on ([`container::ATTEMPTS`] in a row). A moment taw cannot see into the
    /// runtime is no container leaving the pane: its marker would go and come back with it, and with
    /// the marker the profile's mark, the `sandbox.*` hooks and what the agent reported.
    fn kept_container(
        &mut self,
        command: &str,
        look: container::Look,
    ) -> Option<detect::ContainerAgent> {
        if look.gave_up {
            self.container_given_up = true;
        }
        let unread = look.agent.is_none() && self.container.wants_recheck();
        if !unread {
            self.container_given_up = false;
        }
        match look.agent {
            Some(agent) => {
                self.container_last = Some(agent.clone());
                Some(agent)
            }
            None if keeps_the_last_container(
                self.container_last.as_ref(),
                command,
                unread,
                self.container_given_up,
            ) =>
            {
                debug!(
                    "pane {}: could not look into {command} at this read; taking it as still running \
                     the agent it ran",
                    self.pane
                );
                self.container_last.clone()
            }
            None => {
                self.container_last = None;
                None
            }
        }
    }

    /// The display line, and with `with_screen` the pane's screen too, taken in the same
    /// tmux invocation (`display-message … ; capture-pane …`): one process for the read
    /// instead of two. The display line is always one line — [`DISPLAY_FORMAT`] folds
    /// every newline in the values — so the screen is everything after the first.
    fn display(&self, with_screen: bool) -> anyhow::Result<(String, Option<String>)> {
        if !with_screen {
            return Ok((self.t.display(self.pane, DISPLAY_FORMAT)?, None));
        }
        let out = self.t.run(&[
            "display-message",
            "-p",
            "-t",
            self.pane,
            DISPLAY_FORMAT,
            ";",
            "capture-pane",
            "-p",
            "-t",
            self.pane,
        ])?;
        let (line, screen) = out.split_once('\n').unwrap_or((out.as_str(), ""));
        Ok((line.to_string(), Some(screen.to_string())))
    }

    /// Whether this read will want the screen, so it can ride with the display: the last
    /// read used one, and this one is owed to output, a hold or a forced read rather than
    /// the quiet recheck that reuses it, with no startup grace running and no exited
    /// agent's shell in front. A wrong guess costs a capture the read throws away, or a
    /// second invocation for the one it lacked — never a different answer.
    fn expects_capture(&self, stream: &StreamState) -> bool {
        self.last_capture.is_some()
            && !self.quiet(stream)
            && !self.arbiter.in_startup_grace()
            && self.exited_under.is_none()
    }

    /// Whether this read is owed to nothing but the identity recheck: no output since the
    /// last read, no hold, no forced read.
    fn quiet(&self, stream: &StreamState) -> bool {
        self.last_read.is_some()
            && stream.eligible_since.is_none()
            && !self.arbiter.hold_active()
            && self.arbiter.forced_read().is_none()
    }

    /// The pane's screen for this read: `prefetched` when the display brought it along, with the
    /// size of the history and the alternate screen that display read (`displayed`), in the same
    /// invocation.
    ///
    /// A read owed to nothing but the identity recheck — no output since the last read —
    /// whose command and kind hint are still the ones the last capture was taken under
    /// reuses that capture: tmux changes a pane's screen only when something in it
    /// prints, so a fresh `capture-pane` would return the same text, and a quiet agent
    /// pane then costs one `display-message` per recheck. Any other read captures, and reads
    /// the size of the history and the alternate screen again with it ([`Watcher::capture`]).
    fn screen(
        &mut self,
        stream: &StreamState,
        o: &Observed,
        prefetched: Option<String>,
        displayed: (u64, bool),
    ) -> anyhow::Result<Screen> {
        if prefetched.is_none()
            && self.quiet(stream)
            && let Some(last) = &self.last_capture
            && last.command == o.command
            && last.kind_hint == o.kind_hint
        {
            return Ok(last.screen.clone());
        }
        let screen = match prefetched {
            Some(text) => Screen {
                text,
                history: displayed.0,
                alternate: displayed.1,
            },
            None => self.capture()?,
        };
        self.last_capture = Some(Capture {
            command: o.command.clone(),
            kind_hint: o.kind_hint.clone(),
            screen: screen.clone(),
        });
        Ok(screen)
    }

    /// The pane's screen, with the size of its history and whether the alternate screen is on, read
    /// in one tmux invocation (`display-message ... ; capture-pane ...`).
    fn capture(&self) -> anyhow::Result<Screen> {
        let out = self.t.run(&[
            "display-message",
            "-p",
            "-t",
            self.pane,
            "#{history_size}\t#{alternate_on}",
            ";",
            "capture-pane",
            "-p",
            "-t",
            self.pane,
        ])?;
        let (head, text) = out.split_once('\n').unwrap_or((out.as_str(), ""));
        let mut fields = head.split('\t');
        Ok(Screen {
            history: fields
                .next()
                .and_then(|history| history.trim().parse().ok())
                .unwrap_or(0),
            alternate: fields
                .next()
                .is_some_and(|alternate| alternate.trim() == "1"),
            text: text.to_string(),
        })
    }

    /// Re-reads `config.toml`, and drops the compiled manifest cache when the overrides or
    /// the cached remote manifests changed.
    ///
    /// Called once per read, straight after the display read that carried `token`, so it
    /// costs a few `stat` calls and nothing more when nothing changed. A
    /// config that now fails to *parse* keeps the settings already in force and warns:
    /// a watcher is running against a live pane, and swapping its notification and rail
    /// behaviour for the defaults because someone saved a half-typed line would be worse
    /// than carrying on with what it had.
    fn apply_reload(&mut self, token: &str) {
        let changed = self.reload.poll(
            &self.paths.config_file,
            &self.paths.override_dir,
            &self.paths.remote_manifest_dir,
            token,
        );
        if changed.config {
            match crate::config::load(self.paths) {
                Ok(cfg) => {
                    info!(
                        "config changed; re-read {}",
                        self.paths.config_file.display()
                    );
                    self.cfg = cfg;
                }
                Err(err) => warn!(
                    "config changed but {} could not be loaded, keeping the settings in \
                     force: {err:#}",
                    self.paths.config_file.display()
                ),
            }
            // A reload is also how a change to tmux's own environment reaches the hooks:
            // `taw config reload` says "read everything again", and this is one thing read.
            self.server_env.forget();
        }
        if changed.manifests && !self.manifests.is_empty() {
            info!(
                "agent manifest overrides changed, or the cached remote copies did; dropping {} \
                 cached",
                self.manifests.len()
            );
            self.manifests.clear();
        } else if changed.manifests {
            info!(
                "agent manifest overrides changed, or the cached remote copies did; nothing \
                 cached to drop"
            );
        }
    }

    /// Whether this pane's identity comes only from its kind hint AND the process
    /// `agent start` recorded for it has exited. `taw agent start --kind claude` sets
    /// `@agent_kind_hint`, which outlives any one occupant by design; when the started agent
    /// then exits, its foreground command reverts to a plain shell but its final prompt box
    /// / OSC title can still match a manifest rule, so a single screen cannot tell it is
    /// gone. Guarded three ways so a *live* hint-only agent (a wrapper script whose process
    /// name we do not match) is never mistaken for a dead one: the identity must be hint-only
    /// (no process names an agent), a start pid must have been recorded, and that pid must be
    /// dead. `decide` turns a `true` here into `unknown` (hint kept), never a clear.
    fn hint_only_agent_exited(&self, observation: &Observation) -> anyhow::Result<bool> {
        let o = &observation.observed;
        // A process-identified pane reaches `NoAgent` through the ordinary path when its
        // command stops naming an agent; only a hint-only identity needs this.
        if o.kind_hint.is_none() || o.process_identity().is_some() {
            return Ok(false);
        }
        recorded_process_exited(self.t, self.pane)
    }

    /// Whether the agent the pane records is suspended (herdr v0.9.4): the process a read found it
    /// running as, in front of the pane, is alive behind something else on the pane's terminal, stopped
    /// with Ctrl+Z or run on in the background, and no other agent has reported a session in the pane
    /// since ([`suspend::Note`]). The note is kept on the pane as well, and the first read of a watcher
    /// takes it back.
    fn suspended_agent(&mut self, o: &Observed) -> bool {
        let (t, pane) = (self.t, self.pane);
        let seen = suspend::Seen {
            kind: o.prev.kind.as_deref(),
            in_front: o.prev.kind.is_some() && o.process_identity() == o.prev.kind,
            command: &o.command,
            session: o.prev.session_id.as_deref(),
        };
        let now = Instant::now();
        let mut look = PaneLook {
            t,
            pane,
            foreground: &mut self.foreground,
        };
        let mut changed = false;
        if !self.note_started {
            self.note_started = true;
            let saved = t
                .get_pane_option(pane, store::PROCESS_OPTION)
                .ok()
                .flatten();
            changed = self.note.start(saved.as_deref(), &seen, now, &mut look);
        }
        let read = self.note.read(&seen, now, &mut look);
        if changed || read.changed {
            let written = match self.note.process() {
                Some(process) => {
                    t.set_pane_option(pane, store::PROCESS_OPTION, &process.option_value())
                }
                None => t.unset_pane_option(pane, store::PROCESS_OPTION),
            };
            if let Err(err) = written {
                debug!("pane {pane}: the note of its agent's process was not kept: {err:#}");
            }
        }
        if read.suspended {
            debug!("pane {pane}: its agent's process is alive behind its shell");
        }
        read.suspended
    }

    /// While the line `taw agent start` typed the agent's command on is on the screen, the screen this
    /// read decides on is the rows below it: the pane still shows above it what its last occupant left,
    /// a permission prompt or a turn the new agent's rules read as well as one it draws, and what the
    /// command started can have drawn only below it (see [`typed_line`]). Nothing drawn there is a screen
    /// with nothing on it, which is `unknown` for the new agent, whether its hint alone names it or a
    /// process does too (the job in front of the pane, a script under `sh`). Once the line has gone
    /// off the screen, the agent's drawing has taken all of it: the screen is read whole, and the pane
    /// option is taken off. A line one read does not find is not gone yet ([`typed_line::Misses`]): that
    /// read decides on no screen at all, and the next looks again.
    fn read_below_the_typed_line(&mut self, observation: &mut Observation) {
        let Some(line) = &observation.typed_line else {
            self.misses.looked_away();
            return;
        };
        match self.misses.read(
            line,
            &observation.observed.screen,
            observation.history,
            observation.alternate,
        ) {
            typed_line::Read::Rows(rows) => {
                observation.observed.screen = rows.to_string();
                observation.observed.below_typed_line = true;
            }
            typed_line::Read::Nothing => {
                debug!(
                    "pane {}: the line its agent's command was typed on is not on the screen; \
                     reading nothing of it until the next read looks again",
                    self.pane
                );
                observation.observed.screen.clear();
                observation.observed.screen_unread = true;
            }
            typed_line::Read::Gone => self.forget_typed_line(line),
        }
    }

    /// Takes `@agent_typed_line` off the pane while it still holds `line`: `taw agent start` may have
    /// written the next agent's since this read. Asked of tmux in the same invocation as the unset.
    fn forget_typed_line(&self, line: &typed_line::TypedLine) {
        let still = format!(
            "#{{==:#{{{}}},{}}}",
            store::TYPED_LINE_OPTION,
            store::format_literal(&line.value())
        );
        let unset = format!(
            "set-option -p -u -t {} {}",
            crate::tmux::command_word(self.pane),
            store::TYPED_LINE_OPTION
        );
        match self
            .t
            .run_ok(&["if-shell", "-F", "-t", self.pane, &still, &unset])
        {
            Ok(()) => debug!(
                "pane {}: the line its agent's command was typed on is not read from any more",
                self.pane
            ),
            Err(err) => debug!(
                "pane {}: taking {} off: {err:#}",
                self.pane,
                store::TYPED_LINE_OPTION
            ),
        }
    }

    /// Whether the pane's own shell, the process tmux started in it, is idle
    /// ([`Observed::pane_shell_idle`]); `None` when tmux, `ps` or `pgrep` cannot say.
    fn pane_shell_idle(&self) -> Option<bool> {
        let pid = self
            .t
            .display(self.pane, "#{pane_pid}")
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        pane_shell_is_idle(pid)
    }

    /// Writes a [`Decision`] into tmux, and fans out everything that hangs off it.
    ///
    /// What is written is [`state::update`]'s, the mapping the fake-clock harness applies
    /// to its own record. What hangs off it — the rail, the plugin event, the
    /// notification and the session snapshot — only a status change or a new occupant
    /// earns. A hook-owned pane has its kind recorded, when the watcher knows it, and
    /// nothing else; a rule refresh is evidence only, so `@agent_state_since` is left
    /// saying when the *state* last moved.
    fn apply(&mut self, decision: Decision, observation: &Observation) -> anyhow::Result<()> {
        let o = &observation.observed;
        // With what the read leaves to be written besides: the pane marked seen when the agent's
        // session changed or another agent took it, and whether the agent is being taken.
        let update = state::update(&o.prev, &decision).finish(&mut self.arbiter, &o.prev);
        self.write(&update, observation)?;
        // The notification the change earns waits for the agent and the session it was earned in.
        let pending = update.notice.clone().map(|notice| state::Pending {
            notice,
            earned: state::Earned::after(&o.prev, &update),
        });
        match &decision {
            Decision::NoAgent { clear: true } => {
                self.refresh_rail();
                self.emit_status_changed(None, o.prev.state, None);
                self.autosave();
            }
            Decision::KindChanged { kind } => {
                // Not a status change: `@agent_state` and friends still describe the pane
                // correctly, so no notification and no `agent_status_changed` claiming a
                // transition that did not happen.
                info!(
                    "pane {}: occupant is now {kind} (state unchanged at {})",
                    self.pane,
                    o.prev
                        .state
                        .map(|state| state.to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                self.refresh_rail();
                self.emit_kind_changed(kind, o.prev.state);
                self.autosave();
            }
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                detail,
                ..
            } => {
                info!(
                    "pane {}: {} -> {to} ({}, rule {})",
                    self.pane,
                    from.map(|state| state.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    kind,
                    rule.as_deref().unwrap_or("none")
                );
                self.announce(kind, *from, *to, detail, pending, observation);
            }
            // An agent's own report of a change, taken through the same steps as one read off the
            // screen. What the watcher wrote back over a report that repeated itself is no change.
            Decision::Reported {
                kind,
                from,
                via,
                to,
                detail,
                replaced,
                ..
            } => {
                let before = from
                    .map(|state| state.to_string())
                    .unwrap_or_else(|| "none".to_string());
                if let Some(via) = via {
                    // A turn that began and ended between two reads: its start, then its end, each
                    // announced as a change of its own, the toast with the end.
                    info!(
                        "pane {}: {before} -> {via} -> {to} ({kind}, a turn between two reads, reported by its hook)",
                        self.pane
                    );
                    self.announce(kind, *from, *via, detail, None, observation);
                    self.announce(kind, Some(*via), *to, detail, pending, observation);
                } else if *from != Some(*to) {
                    info!(
                        "pane {}: {before} -> {to} ({kind}, reported by its hook)",
                        self.pane
                    );
                    self.announce(kind, *from, *to, detail, pending, observation);
                } else if let Some(last) = replaced {
                    // The same state from another agent: not a status change, and the occupant
                    // is what the event says, as for one a screen read finds in the pane's place.
                    info!(
                        "pane {}: occupant is now {kind}, from {last} (state unchanged at {to}, reported by its hook)",
                        self.pane
                    );
                    self.refresh_rail();
                    self.emit_kind_changed(kind, Some(*to));
                    self.autosave();
                }
            }
            // Kept at `idle`: a turn it was in ends with no notification, nothing having finished.
            Decision::Suspended {
                kind, from, detail, ..
            } => {
                info!("pane {}: {kind} is suspended, and kept", self.pane);
                if *from == Some(AgentState::Idle) {
                    self.refresh_rail();
                    self.autosave();
                } else {
                    self.announce(kind, *from, AgentState::Idle, detail, None, observation);
                }
            }
            Decision::HookAuthoritative { kind: Some(kind) } if update.kind.is_some() => {
                debug!("pane {}: kind {kind} (state left to its hook)", self.pane);
            }
            Decision::NoAgent { clear: false }
            | Decision::Keep
            | Decision::HookAuthoritative { .. }
            | Decision::RuleChanged { .. } => {}
        }
        self.agent_present = !matches!(decision, Decision::NoAgent { .. });
        Ok(())
    }

    /// What a change of state fans out to, whoever decided it: the rail, the status event (which
    /// the `agent.*` event of the state it went into is also), the notification its class earns,
    /// and the session snapshot's autosave. The one place a change is announced, so that a change
    /// an agent reported and one read off its screen are announced the same.
    fn announce(
        &mut self,
        kind: &str,
        from: Option<AgentState>,
        to: AgentState,
        detail: &str,
        pending: Option<state::Pending>,
        observation: &Observation,
    ) {
        self.refresh_rail();
        self.emit_status_changed(Some(kind), from, Some(to));
        if let Some(pending) = pending {
            self.notify(kind, pending, detail, observation);
        }
        self.autosave();
    }

    /// Makes the writes `update` lists: the clear, then where the agent runs, then the state
    /// (with since, rule and source) or the rule alone, then who is in the pane, then whether
    /// it was seen; and then the profile mark's end, when the read found one.
    fn write(&mut self, update: &state::Update, observation: &Observation) -> anyhow::Result<()> {
        let o = &observation.observed;
        if update.clear {
            info!(
                "pane {}: no agent any more, clearing @agent_* options",
                self.pane
            );
            if store::clear_agent(self.t, self.pane, &observation.report_stamp)? {
                // `@agent_container` went with the rest.
                self.container_written = None;
            } else {
                info!(
                    "pane {}: an agent reported in it since it was read; its record stays for the \
                     next read",
                    self.pane
                );
            }
        }
        // Before anything that makes the pane an agent's (`@agent_kind` is what the popup
        // and the CLI take for one), so a reader that sees the agent sees where it runs.
        self.sync_container(update, observation)?;
        if let Some((to, rule)) = &update.state {
            // What the agent leaves with, before the state that says it has left: a wait that sees
            // the new state sees the status it left with too.
            if state::leaves(rule.as_deref(), observation.agent_exited) {
                // The state the agent leaves with is written as `manifest`'s, which would hand a
                // pane an agent claimed since this read back from its hook: only while no report
                // has landed since.
                let left =
                    state::left_as(o.prev.state, *to, rule.as_deref(), observation.agent_exited);
                if !store::write_exit(
                    self.t,
                    self.pane,
                    left.map(|left| (left, unix_millis())),
                    *to,
                    rule.as_deref(),
                    MANIFEST_SOURCE,
                    unix_seconds(),
                    &observation.report_stamp,
                )? {
                    info!(
                        "pane {}: an agent reported in it since it was read; what the last one left \
                         with is not written over the report",
                        self.pane
                    );
                }
            } else {
                store::write_state(
                    self.t,
                    self.pane,
                    *to,
                    rule.as_deref(),
                    MANIFEST_SOURCE,
                    unix_seconds(),
                )?;
            }
        }
        if let Some((over, to)) = update.complete {
            store::write_state_over(self.t, self.pane, over, to, unix_seconds())?;
            debug!(
                "pane {}: {to} written over the reported {over}, its hook still the source",
                self.pane
            );
        }
        if let Some(rule) = &update.rule {
            match rule {
                // Folded, exactly as `store::write_state` folds the same option on a
                // transition: a rule id is whatever a manifest author typed, and
                // manifests are user-editable TOML.
                Some(rule) => store::set_text_option(self.t, self.pane, RULE_OPTION, rule)?,
                // tmux has no `unset-option`; `set-option -u` is the removal form.
                None => self.t.unset_pane_option(self.pane, RULE_OPTION)?,
            }
            debug!(
                "pane {}: rule is now {} (state unchanged)",
                self.pane,
                rule.as_deref().unwrap_or("none")
            );
        }
        if update.release_identity {
            // Before the kind is recorded: a drop that fails leaves the last kind on the pane, so
            // the next read finds the replacement again and makes it, where a kind already
            // written would have nothing left to tell the next read what was not dropped.
            if release_identity(
                self.t,
                self.pane,
                o.prev.session_id.as_deref(),
                &observation.report_stamp,
            )? {
                info!(
                    "pane {}: {} took the pane from {}; the session {:?} it had reported went with it",
                    self.pane,
                    update.kind.as_deref().unwrap_or("?"),
                    o.prev.kind.as_deref().unwrap_or("?"),
                    o.prev.session_id.as_deref().unwrap_or_default()
                );
            } else {
                info!(
                    "pane {}: {} took the pane from {}; what that one reported stays, since the \
                     pane's session, or the report of an agent in it, changed after it was read",
                    self.pane,
                    update.kind.as_deref().unwrap_or("?"),
                    o.prev.kind.as_deref().unwrap_or("?"),
                );
            }
        }
        if let Some(kind) = &update.kind {
            // With the end of what a restore kept of the pane, if it did: it has an agent of its own.
            store::record_kind(self.t, self.pane, kind)?;
        }
        if update.release_name {
            // The name was the last agent's: a stale one would have `taw agent prompt
            // <name>` type into whoever took the pane.
            self.t.unset_pane_option(self.pane, NAME_OPTION)?;
            info!(
                "pane {}: {} took the pane from {}; the name {:?} went with it",
                self.pane,
                update.kind.as_deref().unwrap_or("?"),
                o.prev.kind.as_deref().unwrap_or("?"),
                o.prev.name.as_deref().unwrap_or_default()
            );
        }
        if let Some(seen) = update.seen {
            store::set_seen(self.t, self.pane, seen)?;
        }
        match &update.acquiring {
            Some(state::MarkWrite::Set(at)) => {
                self.t
                    .set_pane_option(self.pane, store::ACQUIRING_OPTION, &at.value())?;
            }
            Some(state::MarkWrite::Unset) => self
                .t
                .unset_pane_option(self.pane, store::ACQUIRING_OPTION)?,
            None => {}
        }
        // The runtime the last read left the pane marked with, and the one this read leaves.
        let before = self.container_announced.clone();
        let after = self.container_written.clone();
        self.sync_mark(before.as_deref(), after.as_deref(), observation)?;
        self.sandbox_events(before.as_deref(), after.as_deref(), observation);
        self.announce_container(after, observation);
        self.take_report(update, observation)?;
        Ok(())
    }

    /// Takes the session a sandboxed agent's integration reported from inside its sandbox (`sandbox::report`): the pane is marked with a profile, the container in front of
    /// it runs the agent the pane records, and the report has changed since the last look, or the
    /// pane has lost what it took. Nothing the file holds reaches a log, and nothing is written back.
    fn take_report(
        &mut self,
        update: &state::Update,
        observation: &Observation,
    ) -> anyhow::Result<()> {
        use crate::sandbox::report::{self, Look};
        let o = &observation.observed;
        let (Some(mark), Some(found)) = (observation.sandbox.as_deref(), o.container.as_ref())
        else {
            return Ok(());
        };
        let recorded = if update.clear {
            None
        } else {
            update.kind.as_deref().or(o.prev.kind.as_deref())
        };
        if recorded != Some(found.kind.as_str()) {
            return Ok(());
        }
        let held = o.prev.session_id.is_some() || o.prev.session_path.is_some();
        if self.report_on_the_pane && !held {
            debug!(
                "pane {}: the session its sandbox reported is no longer on it; the report is taken \
                 again",
                self.pane
            );
            self.report_seen = None;
            self.report_on_the_pane = false;
        }
        let Some(folder) = self
            .server_key()
            .and_then(|server| report::folder(self.paths, &server, self.pane))
        else {
            return Ok(());
        };
        let read = report::read(&folder);
        let look = match &read {
            Ok(None) => Look::Nothing,
            Ok(Some(file)) => Look::File(file.id),
            Err(why) => Look::Refused(*why),
        };
        if self.report_seen == Some(look) {
            return Ok(());
        }
        self.report_seen = Some(look);
        self.report_on_the_pane = false;
        let file = match read {
            Ok(Some(file)) => file,
            Ok(None) => return Ok(()),
            Err(why) => {
                debug!(
                    "pane {}: its sandbox's report was not read: {why}",
                    self.pane
                );
                return Ok(());
            }
        };
        let reported = match report::session_of(&found.kind, &file.bytes) {
            Ok(reported) => reported,
            Err(why) => {
                debug!(
                    "pane {}: its sandbox's report was not taken: {why}",
                    self.pane
                );
                return Ok(());
            }
        };
        if reported.id == o.prev.session_id && reported.path == o.prev.session_path {
            self.report_on_the_pane = true;
            return Ok(());
        }
        let session = store::Session {
            id: reported.id.as_deref(),
            path: reported.path.as_deref(),
        };
        let written = refused_if_too_long(store::write_sandboxed_session(
            self.t,
            self.pane,
            &found.kind,
            mark,
            &observation.sandbox_pid_text,
            &session,
            reported.source.as_deref(),
        ))?;
        let Some(written) = written else {
            // This report is looked at once, as any other refused one: the same file is not tried
            // again, and what this read still has to do goes on.
            debug!(
                "pane {}: its sandbox's report was not taken: it is too long for one tmux command",
                self.pane
            );
            return Ok(());
        };
        if !written {
            // The next read finds the pane as it is now, and looks again.
            self.report_seen = None;
            debug!(
                "pane {}: its sandbox's report came as the pane changed; the next read looks again",
                self.pane
            );
            return Ok(());
        }
        self.report_on_the_pane = true;
        info!(
            "pane {}: took the session its {} reported from inside its sandbox",
            self.pane, found.kind
        );
        // A save made before it would resume nothing.
        self.autosave();
        Ok(())
    }

    /// Takes `runtime` as the one the pane is marked with now, with the profile and the agent this
    /// read found in it: what a container that ends with its pane is said to have been
    /// ([`Watcher::end_with_the_pane`]). No runtime is no container, and nothing of one is kept.
    fn announce_container(&mut self, runtime: Option<String>, observation: &Observation) {
        let o = &observation.observed;
        self.announced_profile = runtime.as_ref().and(observation.sandbox.clone());
        self.announced_kind = runtime.as_ref().and(
            o.container
                .as_ref()
                .map(|found| found.kind.clone())
                .or_else(|| o.prev.kind.clone()),
        );
        if runtime.is_none() {
            self.listing = None;
        }
        self.container_announced = runtime;
    }

    /// Fires `sandbox.started` when a container comes to run the pane's agent and `sandbox.stopped`
    /// when it leaves the pane, with the runtime and the profile; a runtime that changes under one
    /// read is the one leaving and the other coming. Hooks only, and nothing at all when no hook is
    /// set for the event. Best effort: a hook never holds the watcher up.
    fn sandbox_events(
        &mut self,
        before: Option<&str>,
        after: Option<&str>,
        observation: &Observation,
    ) {
        use crate::hooks::events::{SANDBOX_STARTED, SANDBOX_STOPPED};
        let o = &observation.observed;
        let profile = observation.sandbox.as_deref();
        if let Some(gone) = before.filter(|gone| Some(*gone) != after) {
            self.sandbox_event(SANDBOX_STOPPED, gone, profile, o.prev.kind.as_deref());
        }
        if let Some(came) = after.filter(|came| Some(*came) != before) {
            let kind = o.container.as_ref().map(|found| found.kind.as_str());
            self.sandbox_event(SANDBOX_STARTED, came, profile, kind);
        }
        // A container that ends with its pane leaves no pane to list: it is listed while it is
        // there, for the stop a hook would be told of then ([`Watcher::end_with_the_pane`]).
        if after.is_some()
            && self.listing.is_none()
            && !self.cfg.hooks.commands(SANDBOX_STOPPED).is_empty()
        {
            self.listing = self.listed(SANDBOX_STOPPED);
        }
    }

    /// The pane as tmux lists it now, kept as the last listing of it, `None` when tmux would not
    /// list it.
    fn listed(&mut self, event: &str) -> Option<PaneInfo> {
        let info = match self.t.list_panes() {
            Ok(panes) => panes.into_iter().find(|pane| pane.pane_id == self.pane),
            Err(err) => {
                warn!("could not read pane records for {event}: {err:#}");
                return None;
            }
        };
        if info.is_none() {
            debug!("pane {} vanished before its {event}", self.pane);
        }
        info
    }

    fn sandbox_event(
        &mut self,
        event: &str,
        runtime: &str,
        profile: Option<&str>,
        kind: Option<&str>,
    ) {
        if self.cfg.hooks.commands(event).is_empty() {
            return;
        }
        let Some(info) = self.listed(event) else {
            return;
        };
        self.listing = Some(info.clone());
        let started = self.fire_sandbox_event(event, runtime, profile, kind, Some(&info));
        info!(
            "pane {}: {event} ({runtime}, profile {}), {started} hook(s)",
            self.pane,
            profile.unwrap_or("none")
        );
    }

    /// Starts the hooks of a sandbox event about this pane, as `info` lists it, or by its id alone
    /// when there is no listing of it: the pane's ids, the agent, what came or left (`sandbox`),
    /// and the pane's record. Returns how many started.
    fn fire_sandbox_event(
        &self,
        event: &str,
        runtime: &str,
        profile: Option<&str>,
        kind: Option<&str>,
        info: Option<&PaneInfo>,
    ) -> usize {
        let ctx = PluginCtx {
            tmux: self.t,
            cfg: &self.cfg,
            paths: self.paths,
            taw_bin: self.taw_bin,
            server_env: Some(&self.server_env),
        };
        let payload = match info {
            Some(info) => serde_json::json!({
                "pane_id": ids::pane_id(&info.session_id, &info.pane_id),
                "workspace_id": ids::workspace_id(&info.session_id),
                "agent": kind,
                "sandbox": { "profile": profile, "runtime": runtime },
                "pane": plugin::context::pane_json(info),
            }),
            None => serde_json::json!({
                "pane_id": self.pane,
                "agent": kind,
                "sandbox": { "profile": profile, "runtime": runtime },
            }),
        };
        crate::hooks::fire(&ctx, event, &payload)
    }

    /// The watcher's pipe has closed, or a read found no pane (`known_gone`). When the pane itself has
    /// gone with the container this watcher last announced in it (x on its row, a closed window, a
    /// server that ended), that container has left the pane too: `sandbox.stopped` fires, told what
    /// the last read found (the runtime, the profile, the agent) and the pane as it was last listed,
    /// with no container in it. A pipe that was only taken away, as `taw watch detach` and a watcher
    /// started again take it, ends nothing: tmux still has the pane, and the container in it.
    fn end_with_the_pane(&mut self, known_gone: bool) {
        use crate::hooks::events::SANDBOX_STOPPED;
        let Some(runtime) = self.container_announced.clone() else {
            return;
        };
        if self.cfg.hooks.commands(SANDBOX_STOPPED).is_empty() {
            return;
        }
        if !known_gone && !self.pane_is_gone() {
            debug!(
                "pane {}: its watcher's pipe was taken away; the pane, and the {runtime} in it, are \
                 still there",
                self.pane
            );
            return;
        }
        let gone = self.listing.clone().map(|mut info| {
            info.agent.container = None;
            info.agent.sandbox = None;
            info
        });
        let (profile, kind) = (self.announced_profile.clone(), self.announced_kind.clone());
        let started = self.fire_sandbox_event(
            SANDBOX_STOPPED,
            &runtime,
            profile.as_deref(),
            kind.as_deref(),
            gone.as_ref(),
        );
        info!(
            "pane {}: {SANDBOX_STOPPED} ({runtime}, profile {}), {started} hook(s), as the pane went",
            self.pane,
            profile.as_deref().unwrap_or("none")
        );
        self.container_announced = None;
    }

    /// Whether the pane has gone: tmux knows no such pane any more, or no server answers.
    fn pane_is_gone(&self) -> bool {
        match self.t.display(self.pane, "#{pane_id}") {
            Ok(id) => id.trim() != self.pane,
            Err(err) => is_gone(&err),
        }
    }

    /// Takes the pane's profile mark off at one of its ends (`sandbox::mark`), every option of it and
    /// what the agent reported of itself with it (`store::end_sandbox`), in
    /// the one tmux command that checks the pane still holds the mark this read found, and its process
    /// or none, and the report stamp this read saw: a `taw sandbox` typed since has marked the pane
    /// afresh, and what a read after a report finds is decided on by that read. The pane's report
    /// folder is emptied with it: what the agent's sandbox left there is no report of the next one's.
    fn sync_mark(
        &mut self,
        before: Option<&str>,
        after: Option<&str>,
        observation: &Observation,
    ) -> anyhow::Result<()> {
        use crate::sandbox::mark::{Read, Step, asks};
        let mark = observation.sandbox.as_deref();
        let pid = observation.sandbox_pid;
        let started_alive = match pid {
            Some(pid) if asks(mark, Some(pid), after) => Some(process_alive(pid)),
            _ => None,
        };
        let read = Read {
            mark,
            pid,
            container_before: before,
            container_after: after,
            started_alive,
        };
        if self.mark.step(read) != Step::Clear {
            return Ok(());
        }
        let Some(mark) = mark else {
            return Ok(());
        };
        if !store::end_sandbox(
            self.t,
            self.pane,
            mark,
            &observation.sandbox_pid_text,
            &observation.report_stamp,
        )? {
            self.mark.held_back();
            info!(
                "pane {}: the sandbox profile {mark} has ended there, and the pane changed since it \
                 was read; the next read, a second from now, decides again",
                self.pane
            );
            return Ok(());
        }
        info!(
            "pane {}: the sandbox profile {mark} has ended there; its mark is taken off",
            self.pane
        );
        self.empty_report_folder();
        Ok(())
    }

    /// Empties the pane's report folder (`sandbox::report`), best effort: one that cannot be emptied
    /// is said in the log, and emptied again when taw next starts an agent in the pane.
    fn empty_report_folder(&mut self) {
        let Some(server) = self.server_key() else {
            return;
        };
        let Some(folder) = crate::sandbox::report::folder(self.paths, &server, self.pane) else {
            debug!("pane {}: no sandbox report folder to empty", self.pane);
            return;
        };
        match crate::sandbox::report::empty(&folder) {
            Ok(()) => debug!("pane {}: its sandbox report folder is empty", self.pane),
            Err(err) => warn!(
                "pane {}: could not empty its sandbox report folder {}: {err:#}",
                self.pane,
                folder.display()
            ),
        }
    }

    /// The tmux server's name, which the pane's report folder is kept by, asked of tmux once; `None`,
    /// said in the log, when tmux does not say.
    fn server_key(&mut self) -> Option<String> {
        if self.server.is_none() {
            match crate::sandbox::report::server_key(self.t) {
                Ok(key) => self.server = Some(key),
                Err(err) => {
                    warn!(
                        "pane {}: tmux did not say which server this is, so its sandbox report \
                         folder is neither read nor emptied: {err:#}",
                        self.pane
                    );
                    return None;
                }
            }
        }
        self.server.clone()
    }

    /// Marks the pane with the runtime its agent runs under (`@agent_container`), and takes
    /// the mark away when there is none. Only a change is written: a read that finds the
    /// same runtime as the last one makes no tmux call.
    ///
    /// A container that comes takes off what the pane's last agent reported of itself, its session
    /// and the command that resumes it, in the call that marks the pane, while the pane holds the
    /// report stamp this read saw (`store::mark_container`): it was reported from outside, by the
    /// plain agent the watcher takes the container's for when it saw no shell between the two, and a
    /// restore would resume it outside any container. That is where `sandbox.started` fires, and
    /// before anything a sandboxed agent reports through its pane's report folder is taken. Another
    /// agent than the pane records takes it off through the record's own release
    /// (`release_identity`), guarded on the session the read saw.
    fn sync_container(
        &mut self,
        update: &state::Update,
        observation: &Observation,
    ) -> anyhow::Result<()> {
        let o = &observation.observed;
        let runtime = o.container.as_ref().map(|found| found.runtime);
        if runtime == self.container_written.as_deref() {
            return Ok(());
        }
        match &o.container {
            Some(found) if update.release_identity => {
                self.t
                    .set_pane_option(self.pane, CONTAINER_OPTION, found.runtime)?;
                info!(
                    "pane {}: {} runs in {} (identity source {})",
                    self.pane,
                    found.kind,
                    found.runtime,
                    found.source()
                );
            }
            Some(found) => {
                if !store::mark_container(
                    self.t,
                    self.pane,
                    found.runtime,
                    &observation.report_stamp,
                )? {
                    info!(
                        "pane {}: an agent reported in it since it was read; the container {} is \
                         marked at the next read",
                        self.pane, found.runtime
                    );
                    return Ok(());
                }
                info!(
                    "pane {}: {} runs in {} (identity source {}); what the last agent reported of \
                     itself went as it came",
                    self.pane,
                    found.kind,
                    found.runtime,
                    found.source()
                );
                // A save made before it would hold the session that went.
                self.autosave();
            }
            None => self.t.unset_pane_option(self.pane, CONTAINER_OPTION)?,
        }
        self.container_written = runtime.map(str::to_string);
        Ok(())
    }

    /// Recomputes `@agent_wizard_status`. A failure here loses a status-line update, not
    /// the transition that caused it, so it is logged and swallowed.
    fn refresh_rail(&self) {
        match rail::refresh(self.t, &self.cfg) {
            Ok(text) => debug!("rail: {text:?}"),
            Err(err) => warn!("refreshing the status rail failed: {err:#}"),
        }
    }

    /// Emits `pane.agent_status_changed` to every enabled plugin. Best effort by design:
    /// a plugin must never be able to break the watcher.
    fn emit_status_changed(
        &self,
        kind: Option<&str>,
        previous: Option<AgentState>,
        current: Option<AgentState>,
    ) {
        self.emit_event(kind, previous, current, false);
    }

    fn emit_event(
        &self,
        kind: Option<&str>,
        previous: Option<AgentState>,
        current: Option<AgentState>,
        kind_changed: bool,
    ) {
        let info = match self.t.list_panes() {
            Ok(panes) => panes.into_iter().find(|pane| pane.pane_id == self.pane),
            Err(err) => {
                warn!("could not read pane records for the plugin event: {err:#}");
                return;
            }
        };
        let Some(info) = info else {
            debug!("pane {} vanished before its plugin event", self.pane);
            return;
        };
        let ctx = PluginCtx {
            tmux: self.t,
            cfg: &self.cfg,
            paths: self.paths,
            taw_bin: self.taw_bin,
            server_env: Some(&self.server_env),
        };
        let payload = serde_json::json!({
            "pane_id": ids::pane_id(&info.session_id, &info.pane_id),
            "workspace_id": ids::workspace_id(&info.session_id),
            "agent": kind,
            "agent_status": current.map(AgentState::as_str),
            "previous_status": previous.map(AgentState::as_str),
            "kind_changed": kind_changed,
            "pane": plugin::context::pane_json(&info),
        });
        match plugin::events::emit(&ctx, EVENT_AGENT_STATUS_CHANGED, payload) {
            Ok(0) => {}
            Ok(launched) => debug!("{EVENT_AGENT_STATUS_CHANGED}: {launched} plugin hook(s)"),
            Err(err) => warn!("emitting {EVENT_AGENT_STATUS_CHANGED} failed: {err:#}"),
        }
    }

    /// The same event for a pane whose *occupant* changed while its state did not, with
    /// `previous_status == agent_status` and `kind_changed: true` — so a plugin tracking
    /// who is in a pane still hears about it, and one reacting to status changes can tell
    /// this is not one.
    fn emit_kind_changed(&self, kind: &str, state: Option<AgentState>) {
        self.emit_event(Some(kind), state, state, true);
    }

    /// Starts the notification a transition earned (`pending`, see [`state::notice`]).
    ///
    /// The wait happens on its own thread so the watcher keeps evaluating through it —
    /// which is the whole point of the delay: by the time it elapses, the state may have
    /// moved on (a spinner blinking through `idle`, the user opening the pane, a new
    /// session or another agent in the pane), and the thread re-reads `@agent_state` with
    /// the agent and its session, and re-checks backgroundness, before delivering.
    fn notify(
        &mut self,
        kind: &str,
        pending: state::Pending,
        detail: &str,
        observation: &Observation,
    ) {
        let class = pending.notice.class;
        let delay = delay_for(class, &self.cfg.ui.toast);
        let t = self.t.clone();
        let cfg = self.cfg.clone();
        let paths = self.paths.clone();
        let taw_bin = self.taw_bin.to_path_buf();
        let server_env = Arc::clone(&self.server_env);
        let pane = self.pane.to_string();
        let agent = notify::NotifiedAgent {
            label: if observation.label.is_empty() {
                kind.to_string()
            } else {
                observation.label.clone()
            },
            kind: kind.to_string(),
        };
        let session_name = observation.session_name.clone();
        let window_index = observation.window_index;
        let detail = detail.to_string();

        let handle = std::thread::spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            match t.display(&pane, DUE_FORMAT).map(|line| due_read(&line)) {
                Ok((current, now))
                    if pending.due(current.as_deref(), &now, || {
                        recorded_process_exited(&t, &pane).unwrap_or(false)
                    }) => {}
                Ok((current, now)) => {
                    debug!(
                        "pane {pane}: no notification, state is now {current:?} for {now:?}, \
                         earned for {:?}",
                        pending.earned
                    );
                    return;
                }
                Err(err) => {
                    debug!("pane {pane}: no notification, re-reading its state failed: {err:#}");
                    return;
                }
            }
            match notify::is_background(&t, &pane) {
                Ok(true) => {}
                Ok(false) => {
                    debug!("pane {pane}: no notification, the user is looking at it");
                    return;
                }
                Err(err) => {
                    debug!("pane {pane}: no notification, backgroundness unknown: {err:#}");
                    return;
                }
            }
            // The jump key is deliberately not looked up here: `notify::for_transition`
            // documents that it ignores the argument, because `deliver` reads
            // `@agent-wizard-notification-key` itself at fire time (so a rebind between
            // detection and delivery is honoured). Reading it here would be a tmux call
            // whose result is thrown away.
            let notification = notify::for_transition(
                &agent,
                &session_name,
                window_index,
                class,
                &detail,
                &pane,
                "",
            );
            notify::announce_with(&t, &cfg, &paths, &taw_bin, Some(&server_env), &notification);
        });

        self.notifications.retain(|handle| !handle.is_finished());
        self.notifications.push(handle);
    }

    /// Nudges the session-snapshot autosave debounce: the pane records just changed, so
    /// the saved snapshot is behind. Only touches a marker file — the snapshot itself is
    /// debounced by `session::autosave_if_due` — and an unwritable state directory must
    /// never cost the transition that triggered it.
    fn autosave(&self) {
        if let Err(err) = crate::session::autosave_touch(self.paths) {
            warn!("requesting a session autosave failed: {err:#}");
        }
    }
}

/// One parsed [`DISPLAY_FORMAT`] line: the observation `decide` needs, plus the pieces
/// only the notification text uses.
#[derive(Debug)]
struct Observation {
    observed: Observed,
    session_name: String,
    window_index: u32,
    /// `@agent_wizard_reload`, for [`reload::Tracker`] to compare against.
    reload_token: String,
    /// `@agent_display` -> `@agent_name` -> `@agent_kind`, the same "which name does the
    /// user know this agent by" order the popup uses.
    label: String,
    /// The pid recorded for the pane's agent is dead (see [`Watcher::hint_only_agent_exited`]), as
    /// [`Watcher::step`] found when it read the pane.
    agent_exited: bool,
    /// Where `taw agent start` typed the command of the pane's agent, while the pane records it.
    typed_line: Option<typed_line::TypedLine>,
    /// `#{history_size}` and `#{alternate_on}`: where the typed line is now.
    history: u64,
    alternate: bool,
    /// `@agent_report_stamp` as this read saw it (empty for none), which a clear of the record is made
    /// only while the pane still holds (`store::clear_agent`).
    report_stamp: String,
    /// `@agent_container`, as this read found it.
    container_mark: Option<String>,
    /// `@agent_sandbox`, as this read found it.
    sandbox: Option<String>,
    /// `@agent_sandbox_pid`, as this read found it, when it is a pid.
    sandbox_pid: Option<u32>,
    /// `@agent_sandbox_pid`'s text as this read found it (empty for none), which the mark is taken off
    /// only while the pane still holds.
    sandbox_pid_text: String,
}

/// What the reader's write came to, with a write too long for one tmux command (`store::TooLong`)
/// taken as a refused report (`None`) and not as a failed read: any other error stays one. A report
/// of the sizes `sandbox::report` keeps never comes to it, so this is the last guard, and the read's
/// other work does not stop for it.
fn refused_if_too_long(written: anyhow::Result<bool>) -> anyhow::Result<Option<bool>> {
    match written {
        Err(err) if err.chain().any(|cause| cause.is::<store::TooLong>()) => Ok(None),
        written => written.map(Some),
    }
}

impl Observation {
    /// `default_title` is what tmux titles a pane no program has titled
    /// ([`crate::tmux::default_pane_title`]): the observation carries no title for it.
    fn parse(
        line: &str,
        screen: String,
        osc_progress: &str,
        default_title: Option<&str>,
    ) -> anyhow::Result<Observation> {
        let fields: Vec<&str> = line.split('\t').collect();
        anyhow::ensure!(
            fields.len() == DISPLAY_FIELDS,
            "display: expected {DISPLAY_FIELDS} tab-separated fields, got {}: {line:?}",
            fields.len()
        );
        let set = |index: usize| -> Option<String> {
            Some(fields[index])
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        let prev = AgentRecord {
            kind: set(8),
            state: fields[6].parse().ok(),
            rule: set(13),
            source: set(5),
            name: set(12),
            seen: fields[7] == "1",
            kind_hint: set(4),
            reported_kind: set(15),
            session_id: set(16),
            session_path: set(17),
            ..Default::default()
        };
        let label = [fields[11], fields[12], fields[8]]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or_default()
            .to_string();

        Ok(Observation {
            observed: Observed {
                command: fields[0].to_string(),
                title: crate::tmux::osc_title(fields[1], default_title).to_string(),
                window_active: fields[2] == "1",
                session_attached: fields[3].parse().unwrap_or(0),
                terminal_focus: notify::terminal_focus(fields[DISPLAY_FIELDS - 1]),
                kind_hint: set(4),
                container: None,
                job_agent: None,
                prev,
                screen,
                osc_progress: osc_progress.to_string(),
                pane_shell_idle: None,
                turn: set(18),
                acquiring: store::Acquired::parse(fields[19]),
                screen_unread: false,
                below_typed_line: false,
                suspended: false,
            },
            session_name: fields[9].to_string(),
            window_index: fields[10].parse().unwrap_or(0),
            reload_token: fields[14].to_string(),
            label,
            agent_exited: false,
            typed_line: typed_line::TypedLine::parse(fields[20]),
            history: fields[21].trim().parse().unwrap_or(0),
            alternate: fields[22].trim() == "1",
            report_stamp: fields[23].trim().to_string(),
            container_mark: set(DISPLAY_FIELDS - 4),
            sandbox: set(DISPLAY_FIELDS - 3),
            sandbox_pid: set(DISPLAY_FIELDS - 2).and_then(|pid| pid.trim().parse().ok()),
            sandbox_pid_text: fields[DISPLAY_FIELDS - 2].to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// How long to wait before delivering a notification of `class`, from the live
/// `[ui.toast]` config: `Done` gets its own, longer `done_delay_seconds` so a Claude
/// turn that only pauses briefly on its way to actually finishing does not read as
/// "done" and toast early; `Request` keeps using `delay_seconds`. Falls back to one
/// second on an out-of-range value that fails to convert (config validation is
/// expected to have already rejected those; see `check_toast`).
fn delay_for(class: notify::Class, toast: &crate::config::Toast) -> Duration {
    let seconds = match class {
        notify::Class::Request => toast.delay_seconds,
        notify::Class::Done => toast.done_delay_seconds,
    };
    Duration::try_from_secs_f64(seconds).unwrap_or(Duration::from_secs(1))
}

/// Reads `reader` until EOF, handing every chunk to the loop, and telling it when the pipe is
/// closed ([`Wake::Closed`]). A closed channel means the loop has already stopped, and a read
/// error on a pipe tmux owns is the same event as EOF.
fn pipe_reader(mut reader: Box<dyn Read + Send>, tx: Sender<Wake>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => {
                if tx.send(Wake::Output(buf[..read].to_vec())).is_err() {
                    return;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = tx.send(Wake::Closed);
}

/// Refreshes the cached manifests in the background, at most once every
/// [`MANIFEST_MAX_AGE`] across all watchers (the marker file's mtime is the lock).
/// Detached on purpose: a slow network must not hold up a single evaluation, and an
/// aborted fetch loses nothing (the cache is written by rename).
fn spawn_manifest_refresh(
    store: ManifestStore,
    marker: std::path::PathBuf,
    stale: Arc<AtomicBool>,
) {
    let user_agent = format!("tmux-agent-wizard/{}", env!("CARGO_PKG_VERSION"));
    std::thread::spawn(move || {
        match detect::update_if_stale(
            &store,
            &marker,
            MANIFEST_MAX_AGE,
            MANIFEST_BASE_URL,
            &user_agent,
        ) {
            None => debug!("manifest cache is fresh; no update attempted"),
            Some(outcomes) => {
                let updated: Vec<&str> = outcomes
                    .iter()
                    .filter(|outcome| outcome.status == detect::UpdateStatus::Updated)
                    .map(|outcome| outcome.id.as_str())
                    .collect();
                info!(
                    "manifest update: {} checked, updated {:?}",
                    outcomes.len(),
                    updated
                );
                if !updated.is_empty() {
                    // Tells the loop to recompile: the rules on disk are no longer the
                    // rules it has in hand.
                    stale.store(true, Ordering::Relaxed);
                }
            }
        }
    });
}

/// The pane's `%N` id, falling back to the target string we were given when tmux cannot
/// resolve it (it will fail again in the loop, which is where "pane is gone" is handled).
fn resolve_pane_id(t: &Tmux, pane: &str) -> String {
    match t.display(pane, "#{pane_id}") {
        Ok(id) if !id.is_empty() => id,
        Ok(_) => pane.to_string(),
        Err(err) => {
            debug!("could not resolve pane '{pane}' to a pane id: {err:#}");
            pane.to_string()
        }
    }
}

/// Whether `@agent-wizard-watch` allows attaching. Only the literal `off` disables it;
/// unset means the documented default, `on`. An unreadable option is treated as `on` —
/// failing to watch silently is worse than watching when told not to.
pub fn watching_enabled(t: &Tmux) -> bool {
    match t.run(&["show-option", "-gqv", WATCH_OPTION]) {
        Ok(value) => value.trim() != "off",
        Err(err) => {
            warn!("could not read {WATCH_OPTION}: {err:#}");
            true
        }
    }
}

/// Quotes one word of a `pipe-pane` shell command.
///
/// tmux expands the command before `/bin/sh` ever sees it, in two ways that both bite
/// here (verified against tmux 3.6b):
///
/// * `#`-sequences are format-substituted — `#{pane_id}` in the command is replaced.
/// * The whole string goes through `strftime`, so **`%` is eaten**: a bare `%12` pane id
///   comes out as the empty string, and `A%B` comes out as `ASeptember`.
///
/// Both escapes are doubling (`##`, `%%`), and `shell_words::quote` never introduces
/// either character, so quoting first and escaping after is order-safe.
fn pipe_command_word(word: &str) -> String {
    shell_words::quote(word)
        .replace('#', "##")
        .replace('%', "%%")
}

/// Whether the process `taw agent start` recorded for the pane's agent (`@agent_pid`) is gone. Without
/// a recorded pid, an exited agent is indistinguishable from a live one whose process name we simply
/// do not match, so the answer is `false`: only a pid that is recorded and dead settles it.
fn recorded_process_exited(t: &Tmux, pane: &str) -> anyhow::Result<bool> {
    let Some(pid) = t.get_pane_option(pane, AGENT_PID_OPTION)? else {
        return Ok(false);
    };
    let Ok(pid) = pid.trim().parse::<u32>() else {
        return Ok(false);
    };
    Ok(!process_alive(pid))
}

/// Whether a pid names a live process: signal 0 is sent to it, which delivers nothing and only says
/// whether it could be. Only the system's word that there is no such process (`ESRCH`) is dead. A
/// process this user may not signal (one that became `sudo ...`, `EPERM`) is there all the same, as
/// `session`'s own check takes it, and a number no process can have is taken as alive too: every
/// caller is on the safe side when it keeps what a live process would (a profile's mark, an agent's
/// record, a wait for an agent), and a pid that is gone is what ends each of them.
pub(crate) fn process_alive(pid: u32) -> bool {
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0) else {
        return true;
    };
    // SAFETY: `kill` takes two integers and touches no memory of this process, and signal 0 sends
    // nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Whether the pane's own shell, the process `pid` tmux started in it, is idle: at its prompt
/// with nothing running in it. herdr's `pane_shell_is_idle` (`available_pane_shell_from_job`,
/// `src/platform/mod.rs`, master `0d5d6f1`), asked as it asks, by the pane's pid: the terminal's
/// foreground process group is led by that process, no other process is in it, and its name is
/// a shell ([`state::SHELLS`]). A shell that runs a command has given the terminal to the
/// command's group, a script that runs an agent shares its group with it, and a shell started
/// inside the pane is in a group of its own, so none of them is the pane's shell back.
///
/// `ps` and `pgrep` are spawned, one each, rather than reading the process table through `libc`: the question
/// is asked at most once a second of the rare pane whose agent taw cannot name, and tmux does not
/// expose process groups. The group's members come from `pgrep -g`
/// ([`job::members_of_group`]), with `-a` on macOS only: there `pgrep` leaves out all of its
/// ancestors unless given `-a`, so asked from a process inside the group (a hook of the agent that
/// runs `taw`), or below it, it would see the group smaller than it is. Elsewhere (procps) plain
/// `pgrep -g` lists the whole group. `None` when either look cannot say, which keeps the record.
///
/// `pid` 0 is a pane with no process (a pane kept after its program ended, which tmux 3.8 gives no
/// pid): nothing is asked of the process table for it, since there process 0 is the kernel's own, and
/// the answer is `None`.
pub(crate) fn pane_shell_is_idle(pid: u32) -> Option<bool> {
    shell_idle_by(pid, |pid| {
        printed_by("ps", &["-o", "pgid=,tpgid=,comm=", "-p", &pid.to_string()])
    })
}

/// Whether [`pane_shell_is_idle`] says the pane's own shell is idle, as the rule for agent names
/// reads it ([`PaneInfo::holds_its_agent_name`], for `agent rename`, `agent start`, the lookup by name
/// and the popup's rename check): a shell that cannot be told (`None`) is not idle, so its pane is
/// taken to hold its agent.
pub(crate) fn pane_shell_known_idle(pane: &PaneInfo) -> bool {
    shell_known_idle(pane.pane_pid)
}

/// [`pane_shell_known_idle`] by the pane's own process `pid`, for a caller that holds the pid and not
/// the whole [`PaneInfo`] (the popup's agent rows): the one look every reader of the rule shares.
pub(crate) fn shell_known_idle(pid: u32) -> bool {
    pane_shell_is_idle(pid) == Some(true)
}

/// [`pane_shell_is_idle`] with the look at `pid` given: `ps_of` is not called for a pane with no process.
fn shell_idle_by(pid: u32, ps_of: impl FnOnce(u32) -> Option<String>) -> Option<bool> {
    if pid == 0 {
        return None;
    }
    idle_shell(pid, &ps_of(pid)?, job::members_of_group)
}

/// The agent the container runtime `command` in front of the pane whose own process is `shell` runs,
/// read off its arguments now: the watcher's own reading ([`container`]), for a verb that waits for an
/// agent to come up in a container. The process is the one tmux names, the leader of the terminal's
/// foreground group, asked of the pane's own process. `None` when `command` is no runtime, its process
/// cannot be read or is another by now, or it runs no agent.
pub(crate) fn container_agent(shell: u32, command: &str) -> Option<String> {
    detect::container_runtime(command)?;
    let pid = container::foreground_of(shell)??;
    container::read_args(pid, command)?
        .agent(command)
        .map(|found| found.kind)
}

/// Whether the process `pid` is in the foreground of the pane's terminal: its process group is the
/// group that terminal gives its input to, which is asked of the pane's own process `pane_pid` (the one
/// tmux started in the pty, which has it for its terminal): `ps -o pid=,pgid=,tpgid=` for both, in one
/// call. A process that has gone is not, nor is one that is stopped (a shell takes the terminal back
/// from a job suspended with C-z), a job in the background, or a process with no terminal. Nor is a
/// process in front of another terminal: its own terminal's foreground group is no evidence of this
/// one's, and a pid that the system gave to a program of another pane is not the pane's agent. The pid
/// `taw agent start` or a restore recorded for an agent stands for it, in the verbs that type, only
/// while this is so: that it is alive says nothing of what is in front of the pane.
///
/// Neither pid is ever 0: a pane with no process (`pane_pid` 0, see [`pane_shell_is_idle`]) has nothing in
/// front of it, and 0 is no agent's pid.
pub(crate) fn process_in_front(pid: u32, pane_pid: u32) -> bool {
    in_front_by(pid, pane_pid, |wanted| {
        printed_whatever_the_status("ps", &["-o", "pid=,pgid=,tpgid=", "-p", wanted])
    })
}

/// [`process_in_front`] with the look given: `ps_of` is asked for the list `pid,pane_pid`, and not at all
/// when either is 0.
fn in_front_by(pid: u32, pane_pid: u32, ps_of: impl FnOnce(&str) -> Option<String>) -> bool {
    if pid == 0 || pane_pid == 0 {
        return false;
    }
    ps_of(&format!("{pid},{pane_pid}"))
        .is_some_and(|ps| in_front_of_the_panes_terminal(&ps, pid, pane_pid))
}

/// [`process_in_front`] from what `ps -o pid=,pgid=,tpgid= -p <pid>,<pane_pid>` printed: a row for each
/// process that is there, in no order.
fn in_front_of_the_panes_terminal(ps_output: &str, pid: u32, pane_pid: u32) -> bool {
    let row = |wanted: u32| -> Option<(i64, i64)> {
        ps_output.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            if fields.next()?.parse::<u32>().ok()? != wanted {
                return None;
            }
            let group = fields.next()?.parse().ok()?;
            let foreground = fields.next()?.parse().ok()?;
            Some((group, foreground))
        })
    };
    match (row(pid), row(pane_pid)) {
        (Some((group, _)), Some((_, foreground))) => group > 0 && group == foreground,
        _ => false,
    }
}

/// What the suspend note ([`suspend::Note`]) asks of a pane: tmux for its shell, `ps` for the rest.
struct PaneLook<'a> {
    t: &'a Tmux,
    pane: &'a str,
    foreground: &'a mut container::Foreground,
}

impl suspend::Look for PaneLook<'_> {
    fn foreground(&mut self) -> Option<u32> {
        self.foreground.pid(self.t, self.pane)
    }

    fn shell(&mut self) -> Option<u32> {
        if self.foreground.shell().is_none() {
            self.foreground.pid(self.t, self.pane);
        }
        self.foreground.shell()
    }

    fn ps(&mut self, pids: &str) -> Option<String> {
        printed_whatever_the_status(
            "ps",
            &["-o", "pid=,pgid=,tpgid=,tty=,stat=,ucomm=", "-p", pids],
        )
    }
}

/// How many looks ([`newest_children_in_front`]) a wait for the process a command started makes before its
/// bound can end it, at a restore and at `agent start` alike: a floor of looks, not of time. One look
/// cannot agree with another, and on a busy machine a look can take longer than the whole bound (each is a
/// `pgrep` and a `ps`, a second or two apiece when the machine runs out of room), so that the two it made
/// had caught the helper a shell runs before the command and then the command, which do not agree, and
/// the agent's pid was not recorded. A third finds the command again.
pub(crate) const LOOKS_AT_THE_LEAST: usize = 3;

/// The look at which such a wait ends once its bound has run out, while its last two looks still
/// disagree: past the bound, the looks go on until the sixth. Three slow ones can still catch the helper,
/// then nothing while the command comes up, then the command, and a fourth finds the command again; the
/// sixth keeps a wait whose looks never agree (a shell that keeps starting and ending children) bounded.
/// Quick looks are as many before the bound as it leaves room for.
pub(crate) const LOOKS_AT_THE_MOST: usize = 6;

/// Whether a wait on the looks for the process a command started has looked enough, after `looks` looks
/// whose last two agree or not (`agree`: they found the same child in front, or none): once its bound has
/// run out (`over`), and it has made [`LOOKS_AT_THE_LEAST`], when the last two agree, or when it has made
/// [`LOOKS_AT_THE_MOST`] whatever they found. A restore and `agent start` both end their looks by it.
pub(crate) fn looked_enough(looks: usize, over: impl FnOnce() -> bool, agree: bool) -> bool {
    looks >= LOOKS_AT_THE_LEAST && (agree || looks >= LOOKS_AT_THE_MOST) && over()
}

/// For each of `shells` (the processes tmux started in some panes), its newest child ([`newest_of`]) of
/// those in the foreground of its terminal: in the process group that terminal gives its input to, as
/// `ps` says it of the shell (`tpgid`). A helper a shell's preexec hook runs in the background, or a
/// job the user left there, is not in front; the command the shell runs in front of it is. Two calls
/// for all of them, `pgrep -P` and `ps`. A shell with no child in front has no entry.
///
/// A `0` among `shells` is a pane with no process and is left out of both looks: `pgrep -P 0` lists the
/// children of the kernel's own process 0 (launchd, on macOS).
pub(crate) fn newest_children_in_front(shells: &[u32]) -> std::collections::HashMap<u32, u32> {
    newest_children_by(shells, children_of, procs_of)
}

/// [`newest_children_in_front`] with its two looks given, neither of which is made for a pane with no
/// process.
fn newest_children_by(
    shells: &[u32],
    children_of: impl FnOnce(&[u32]) -> Option<Vec<u32>>,
    procs_of: impl FnOnce(&[u32]) -> Vec<Proc>,
) -> std::collections::HashMap<u32, u32> {
    let shells: Vec<u32> = shells.iter().copied().filter(|&shell| shell != 0).collect();
    if shells.is_empty() {
        return std::collections::HashMap::new();
    }
    let Some(children) = children_of(&shells) else {
        return std::collections::HashMap::new();
    };
    let procs = procs_of(&[&shells[..], &children].concat());
    shells
        .iter()
        .filter_map(|&shell| newest_in_front(&procs, shell).map(|child| (shell, child)))
        .collect()
}

/// [`newest_children_in_front`] for one `shell`, from what `ps` said of it and of its children.
fn newest_in_front(procs: &[Proc], shell: u32) -> Option<u32> {
    let foreground = procs.iter().find(|proc| proc.pid == shell)?.tpgid;
    if foreground <= 0 {
        return None;
    }
    newest_of(
        procs
            .iter()
            .filter(|proc| proc.ppid == shell && proc.pgid == foreground),
    )
}

/// Whether the process that asks descends from the job in front of the terminal of the pane whose own
/// process is `pane_pid`: its parent, or one of that one's ancestors, is in the process group the
/// pane's terminal gives its input to ([`descends_from_the_job_in_front`]). The hook a live agent runs
/// does (the agent is in front of its pane, and the hook is its child); the hook of an agent that has
/// died does not: launchd or init took it in, and the pane's shell has the terminal back. One `ps` of
/// the whole process table; `false` when it cannot be read.
pub(crate) fn caller_descends_from_the_job_in_front_of(pane_pid: u32) -> bool {
    let procs = printed_whatever_the_status("ps", &["-A", "-o", PS_FIELDS])
        .map(|out| parse_procs(&out))
        .unwrap_or_default();
    descends_from_the_job_in_front(&procs, std::os::unix::process::parent_id(), pane_pid)
}

/// [`caller_descends_from_the_job_in_front_of`], from the process table `procs`: whether `start` or one of
/// its ancestors is in the group in front of the terminal of `pane_pid` (the `tpgid` `ps` gives the
/// pane's own process). A walk that comes to launchd or init (pid 1), to a process `ps` did not list, or
/// past a depth no process tree has, has found none.
fn descends_from_the_job_in_front(procs: &[Proc], start: u32, pane_pid: u32) -> bool {
    // A pane with no process (`pane_pid` 0) has no terminal's job in front: process 0 is the kernel's, and
    // `ps -A` lists it on macOS.
    if pane_pid == 0 {
        return false;
    }
    let Some(foreground) = procs
        .iter()
        .find(|proc| proc.pid == pane_pid)
        .map(|proc| proc.tpgid)
        .filter(|group| *group > 0)
    else {
        return false;
    };
    let mut pid = start;
    for _ in 0..64 {
        if pid <= 1 {
            return false;
        }
        let Some(proc) = procs.iter().find(|proc| proc.pid == pid) else {
            return false;
        };
        if proc.pgid == foreground {
            return true;
        }
        pid = proc.ppid;
    }
    false
}

/// The children of `parents`, as `pgrep -P` lists them, `None` for none (or no answer).
fn children_of(parents: &[u32]) -> Option<Vec<u32>> {
    let list = parents
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let children: Vec<u32> = printed_by("pgrep", &["-P", &list])?
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect();
    (!children.is_empty()).then_some(children)
}

/// What `ps` says of `pids` ([`PS_FIELDS`]), the rows of those that are there.
fn procs_of(pids: &[u32]) -> Vec<Proc> {
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // Some `ps` exit with an error when one of the pids has gone, and print the others all the same.
    printed_whatever_the_status("ps", &["-o", PS_FIELDS, "-p", &list])
        .map(|out| parse_procs(&out))
        .unwrap_or_default()
}

/// The fields of a process [`parse_procs`] reads, as `ps -o` takes them.
const PS_FIELDS: &str = "pid=,ppid=,pgid=,tpgid=,etime=";

/// One process, as `ps -o pid=,ppid=,pgid=,tpgid=,etime=` prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// Its process group.
    pgid: i64,
    /// The foreground process group of its terminal: `0` (macOS) or `-1` (Linux) for none.
    tpgid: i64,
    /// How long it has run, in whole seconds.
    age: u64,
}

/// The rows of `ps -o pid=,ppid=,pgid=,tpgid=,etime=` that read as a process, in the order printed.
fn parse_procs(ps_output: &str) -> Vec<Proc> {
    ps_output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let proc = Proc {
                pid: fields.next()?.parse().ok()?,
                ppid: fields.next()?.parse().ok()?,
                pgid: fields.next()?.parse().ok()?,
                tpgid: fields.next()?.parse().ok()?,
                age: elapsed_seconds(fields.next()?)?,
            };
            fields.next().is_none().then_some(proc)
        })
        .collect()
}

/// `ps`'s `etime`, `[[dd-]hh:]mm:ss`, in seconds.
fn elapsed_seconds(etime: &str) -> Option<u64> {
    let (days, clock) = match etime.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, etime),
    };
    let mut seconds = 0u64;
    let mut parts = 0;
    for part in clock.split(':') {
        seconds = seconds * 60 + part.parse::<u64>().ok()?;
        parts += 1;
    }
    (2..=3).contains(&parts).then_some(days * 86_400 + seconds)
}

/// Of `procs`, the pid of the one started last: the least time run. `etime` counts whole seconds, so of
/// those started in the same second the highest pid is taken, which no wrap of the pids within one
/// second makes wrong.
fn newest_of<'a>(procs: impl Iterator<Item = &'a Proc>) -> Option<u32> {
    procs
        .min_by_key(|proc| (proc.age, std::cmp::Reverse(proc.pid)))
        .map(|proc| proc.pid)
}

/// What `program` prints, whatever it exits with, when it runs at all: `ps -p <a>,<b>` exits with an
/// error on some systems when one of the two has gone, and prints the row of the other all the same.
fn printed_whatever_the_status(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What `program` prints, when it runs and succeeds.
pub(crate) fn printed_by(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`pane_shell_is_idle`] from what `ps -o pgid=,tpgid=,comm= -p <pid>` printed and, asked only
/// when the pane's process leads the terminal's foreground group, the members of that group, every
/// process in it whoever asks. `None` for output that says nothing: a process with no terminal
/// (its foreground group is `0` or `-1`), a group that lists nobody, or text that is not what `ps`
/// prints.
fn idle_shell(
    pid: u32,
    ps_output: &str,
    group_members: impl FnOnce(u32) -> Option<Vec<u32>>,
) -> Option<bool> {
    let (leader, rest) = split_field(ps_output.lines().next()?)?;
    let (foreground, name) = split_field(rest)?;
    let (leader, foreground): (i64, i64) = (leader.parse().ok()?, foreground.parse().ok()?);
    let name = name.trim();
    if leader <= 0 || foreground <= 0 || name.is_empty() {
        return None;
    }
    // Something else has the terminal: a command the shell runs, or a group of its own.
    if leader != i64::from(pid) || foreground != leader {
        return Some(false);
    }
    let members = group_members(pid)?;
    if members.is_empty() {
        return None;
    }
    if members.iter().any(|&member| member != pid) {
        return Some(false);
    }
    Some(state::is_shell(name))
}

/// The first whitespace-separated word of `text`, and what follows it.
fn split_field(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// The clock `@agent_final_state` is written in ([`store::FINAL_OPTION`]).
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Test helpers shared with other modules' tests: a real shell on a terminal of its own.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::process::Command;

    /// `script` running `/bin/sh -c line` on a pseudo-terminal, the one place that knows how the two
    /// spell it: BSD `script -q /dev/null command args...` on macOS, util-linux
    /// `script -q -e -c "command" /dev/null` elsewhere (the line single-quoted for the shell it runs
    /// the command in, which `exec` replaces by `/bin/sh` whatever `$SHELL` is). The caller sets
    /// the environment and the three streams, and starts it.
    pub(crate) fn script_command(line: &str) -> Command {
        let mut script = Command::new("script");
        if cfg!(target_os = "macos") {
            script.args(["-q", "/dev/null", "/bin/sh", "-c", line]);
        } else {
            let quoted = line.replace('\'', "'\\''");
            script.args([
                "-q",
                "-e",
                "-c",
                &format!("exec /bin/sh -c '{quoted}'"),
                "/dev/null",
            ]);
        }
        script
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timing_is_herdrs_300ms_500ms_100ms_5s() {
        // herdr v0.9.1: TICK_IDENTIFIED, TICK_UNIDENTIFIED, AGENT_PENDING_IDLE_RECHECK and
        // PROCESS_RECHECK_IDENTIFIED. taw adopts them.
        let timing = Timing::default();
        assert_eq!(timing.identified, Duration::from_millis(300));
        assert_eq!(timing.unidentified, Duration::from_millis(500));
        assert_eq!(timing.hold_recheck, Duration::from_millis(100));
        assert_eq!(timing.identity_recheck, Duration::from_secs(5));
    }

    #[test]
    fn a_quiet_pane_whose_job_could_not_be_read_is_read_again() {
        // `node` in front, and neither its leader nor its job could be read: it may be running an
        // agent, so the pane is looked at again although it prints nothing.
        let timing = Timing::default();
        let container = container::Probe::new(timing.identity_recheck);
        let mut job = job::Probe::new(timing.identity_recheck);
        let base = Instant::now();
        let quiet = |job: &job::Probe| {
            next_read(
                base + Duration::from_secs(1),
                Some(base),
                &timing,
                None,
                false,
                None,
                read_while_quiet(false, &container, job),
            )
        };
        assert_eq!(
            quiet(&job),
            None,
            "a fresh pane that prints nothing is not read"
        );
        let agent = job.agent("node", base, || None, |_| None, |_, _| true);
        assert_eq!(agent, None);
        assert!(job.wants_recheck());
        assert!(
            quiet(&job).is_some(),
            "a job that could not be read is not looked at again"
        );
    }

    /// `ms` after `base`: the scheduler tests' clock.
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_reader_says_the_pipe_is_closed_though_another_sender_keeps_the_channel_open() {
        // The nudge holds a sender of its own for as long as the process lives, so the channel is
        // never closed by the reader ending: the loop ends on the word, or never.
        let (tx, rx) = mpsc::channel();
        let _the_nudges = tx.clone();
        pipe_reader(Box::new(std::io::Cursor::new(b"abc".to_vec())), tx);
        let next = || rx.recv_timeout(Duration::from_secs(5));
        assert!(matches!(next(), Ok(Wake::Output(bytes)) if bytes == b"abc"));
        assert!(matches!(next(), Ok(Wake::Closed)));
    }

    #[test]
    fn the_first_read_is_due_at_once() {
        // A watcher attached to a pane that is already running an agent must classify it
        // without waiting for it to print anything.
        let base = Instant::now();
        let now = at(base, 40);
        for agent_present in [false, true] {
            assert_eq!(
                next_read(
                    now,
                    None,
                    &Timing::default(),
                    None,
                    false,
                    None,
                    agent_present
                ),
                Some(now)
            );
        }
    }

    #[test]
    fn ticks_every_300ms_with_an_agent_and_reads_only_after_output() {
        let base = Instant::now();
        let timing = Timing::default();
        let last = Some(at(base, 0));
        let due = |now: u64, since: Option<u64>| {
            next_read(
                at(base, now),
                last,
                &timing,
                since.map(|ms| at(base, ms)),
                false,
                None,
                true,
            )
        };
        // Output at 50 is read on the tick at 300: not 400 ms after the last byte.
        assert_eq!(due(50, Some(50)), Some(at(base, 300)));
        // Continuous output does not push the read back: it still falls on 300 however
        // late the loop looks, because the tick belongs to the first byte since the read.
        assert_eq!(due(299, Some(50)), Some(at(base, 300)));
        assert_eq!(due(320, Some(50)), Some(at(base, 300)));
        // Output landing exactly on a tick is read on that tick; just after it, the next.
        assert_eq!(due(300, Some(300)), Some(at(base, 300)));
        assert_eq!(due(301, Some(301)), Some(at(base, 600)));
        assert_eq!(due(1000, Some(1000)), Some(at(base, 1200)));
        // Without output nothing on the grid is owed; only the identity recheck.
        assert_eq!(due(50, None), Some(at(base, 5000)));
        assert_eq!(due(4000, None), Some(at(base, 5000)));
    }

    #[test]
    fn a_hold_rechecks_every_100ms_without_output() {
        let base = Instant::now();
        let timing = Timing::default();
        for since in [None, Some(at(base, 20))] {
            assert_eq!(
                next_read(
                    at(base, 30),
                    Some(at(base, 0)),
                    &timing,
                    since,
                    true,
                    None,
                    true
                ),
                Some(at(base, 100)),
                "output or not (since {since:?})"
            );
        }
        // The recheck is measured from the read before it, not from the hold's start.
        assert_eq!(
            next_read(
                at(base, 1210),
                Some(at(base, 1200)),
                &timing,
                None,
                true,
                None,
                true
            ),
            Some(at(base, 1300))
        );
    }

    #[test]
    fn a_quiet_agent_pane_reads_nothing_until_output_then_at_the_next_tick() {
        let base = Instant::now();
        let timing = Timing::default();
        let last = Some(at(base, 0));
        // Quiet: no tick at 300, 600 or 900 is owed anything.
        assert_eq!(
            next_read(at(base, 500), last, &timing, None, false, None, true),
            Some(at(base, 5000))
        );
        // Output at 1000 is read at 1200, the next tick of the 300 ms grid from 0.
        assert_eq!(
            next_read(
                at(base, 1000),
                last,
                &timing,
                Some(at(base, 1000)),
                false,
                None,
                true
            ),
            Some(at(base, 1200))
        );
    }

    #[test]
    fn identity_recheck_every_5s_on_a_quiet_agent_pane() {
        let base = Instant::now();
        let timing = Timing::default();
        for last in [1500, 6500, 11500] {
            assert_eq!(
                next_read(
                    at(base, last + 10),
                    Some(at(base, last)),
                    &timing,
                    None,
                    false,
                    None,
                    true
                ),
                Some(at(base, last + 5000))
            );
        }
    }

    #[test]
    fn an_agentless_pane_reads_at_most_every_500ms_and_only_after_output() {
        let base = Instant::now();
        let timing = Timing::default();
        let due = |now: u64, last: u64, since: Option<u64>| {
            next_read(
                at(base, now),
                Some(at(base, last)),
                &timing,
                since.map(|ms| at(base, ms)),
                false,
                None,
                false,
            )
        };
        // No output: nothing at all, not even a recheck. An agentless pane costs nothing.
        assert_eq!(due(100, 0, None), None);
        assert_eq!(due(60_000, 0, None), None);
        // Output: the next tick of a 500 ms grid.
        assert_eq!(due(100, 0, Some(100)), Some(at(base, 500)));
        assert_eq!(due(700, 0, Some(700)), Some(at(base, 1000)));
        // Output straight after a read still waits out the 500 ms.
        assert_eq!(due(510, 500, Some(510)), Some(at(base, 1000)));
    }

    #[test]
    fn a_read_that_missed_the_typed_line_owes_the_next_tick() {
        let mut misses = typed_line::Misses::default();
        assert_eq!(forced_read(None, &misses), None);
        let line = typed_line::TypedLine::at(8, 3, 40, 0, "claude");
        let _ = misses.read(&line, "\n\n\n\n\n\n", 0, false);
        assert_eq!(forced_read(None, &misses), Some(Forced::NextTick));
        // What the arbiter owes comes first.
        let grace_end = Instant::now();
        assert_eq!(
            forced_read(Some(Forced::From(grace_end)), &misses),
            Some(Forced::From(grace_end))
        );
    }

    #[test]
    fn forced_read_after_grace_or_exit_ignores_output() {
        let base = Instant::now();
        let timing = Timing::default();
        let due = |last: u64, forced: Forced| {
            next_read(
                at(base, last + 10),
                Some(at(base, last)),
                &timing,
                None,
                false,
                Some(forced),
                true,
            )
        };
        // The tick after the grace, or a pending exit's clear: the next tick, quiet or not.
        assert_eq!(due(1200, Forced::NextTick), Some(at(base, 1500)));
        // The grace's end: the first tick at or after it, not the identity recheck.
        assert_eq!(
            due(2700, Forced::From(at(base, 3000))),
            Some(at(base, 3000))
        );
        assert_eq!(
            due(2700, Forced::From(at(base, 3050))),
            Some(at(base, 3300))
        );
        assert_eq!(due(0, Forced::From(at(base, 3000))), Some(at(base, 3000)));
        // Output owed an earlier tick is read on it: forcing never delays a read.
        assert_eq!(
            next_read(
                at(base, 400),
                Some(at(base, 300)),
                &timing,
                Some(at(base, 400)),
                false,
                Some(Forced::From(at(base, 3000))),
                true
            ),
            Some(at(base, 600))
        );
    }

    #[test]
    fn delay_for_selects_the_seconds_field_matching_the_notification_class() {
        let toast = crate::config::Toast {
            delay_seconds: 2.0,
            done_delay_seconds: 7.0,
            ..Default::default()
        };
        assert_eq!(
            delay_for(notify::Class::Request, &toast),
            Duration::from_secs(2)
        );
        assert_eq!(
            delay_for(notify::Class::Done, &toast),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn every_display_column_folds_the_characters_that_break_the_parse() {
        // If a column is ever added without the wrapper, the positional parse becomes a
        // misread waiting for a user to set an option with a tab in it — or, with a
        // newline, an answer that stops being one line at all. The one exception is the
        // last column, the terminals' focus, which expands to nothing a user can set.
        let folded = DISPLAY_FORMAT
            .strip_suffix(crate::notify::terminal_focus_format!())
            .expect("the terminal-focus column comes last");
        assert!(folded.ends_with('\t'));
        assert_eq!(
            folded.matches("#{").count(),
            DISPLAY_FIELDS - 1,
            "one `#{{...}}` per folded column: {DISPLAY_FORMAT:?}"
        );
        assert_eq!(
            folded.matches("#{s/\n/ /;s/\r/ /;s/\t/ /:").count(),
            DISPLAY_FIELDS - 1,
            "every column must fold newline, carriage return and tab: {DISPLAY_FORMAT:?}"
        );
        // tmux ends the last modifier at the first `:`, so no pattern may contain one.
        assert!(!DISPLAY_FORMAT.contains("[[:"));
    }

    /// What the process table says of a group whose members are `pids`.
    fn members(pids: &[u32]) -> Option<Vec<u32>> {
        Some(pids.to_vec())
    }

    #[test]
    fn a_shell_that_leads_the_terminal_and_is_alone_in_its_group_is_idle() {
        // What `ps -o pgid=,tpgid=,comm=` prints, on macOS and on Linux: two numbers and the
        // process's name, padded, and for a login shell with its `-` or as a path.
        for ps in [
            "40532 40532 zsh\n",
            "  40532   40532 /bin/zsh\n",
            "40532 40532 -zsh\n",
            "40532 40532 /usr/bin/FISH\n",
            "40532 40532 bash.exe\n",
            "40532 40532 elvish\n",
        ] {
            assert_eq!(
                idle_shell(40532, ps, |_| members(&[40532])),
                Some(true),
                "{ps:?}"
            );
        }
    }

    #[test]
    fn a_shell_that_shares_its_group_or_does_not_lead_the_terminal_is_not_idle() {
        // `sh -c 'agent; exec zsh'`: the agent is in the shell's group while it runs.
        assert_eq!(
            idle_shell(40532, "40532 40532 sh\n", |_| members(&[40532, 40540])),
            Some(false)
        );
        assert_eq!(
            idle_shell(40532, "40532 40532 sh\n", |_| members(&[40540, 40532])),
            Some(false)
        );
        // A command, a script or a shell inside the shell has the terminal in a group of its own,
        // and the members of the group are not even asked about.
        assert_eq!(
            idle_shell(40532, "40532 40556 zsh\n", |_| panic!(
                "asked about a group"
            )),
            Some(false)
        );
        // The pane's process is not the leader of its own group.
        assert_eq!(
            idle_shell(40532, "40500 40500 zsh\n", |_| panic!(
                "asked about a group"
            )),
            Some(false)
        );
    }

    #[test]
    fn a_process_that_is_not_a_shell_is_not_an_idle_shell() {
        for name in [
            "python3",
            "node",
            "/opt/homebrew/bin/Python",
            "claude",
            "vim",
        ] {
            assert_eq!(
                idle_shell(40532, &format!("40532 40532 {name}\n"), |_| members(&[
                    40532
                ])),
                Some(false),
                "{name}"
            );
        }
    }

    #[test]
    fn a_process_that_cannot_be_read_is_not_said_to_be_idle_or_busy() {
        // No terminal to hold (`ps` prints 0 on macOS and -1 on Linux), a group that lists
        // nobody (the process is gone), and output that is not what the commands print.
        for ps in [
            "",
            "\n",
            "40532\n",
            "40532 40532\n",
            "40532 0 zsh\n",
            "40532 -1 zsh\n",
            "0 0 zsh\n",
            "-1 -1 zsh\n",
            "x y zsh\n",
        ] {
            assert_eq!(idle_shell(40532, ps, |_| members(&[40532])), None, "{ps:?}");
        }
        assert_eq!(idle_shell(40532, "40532 40532 zsh\n", |_| None), None);
        assert_eq!(
            idle_shell(40532, "40532 40532 zsh\n", |_| Some(Vec::new())),
            None
        );
    }

    /// A process is in front of the pane's terminal when its process group is the group the pane's own
    /// process says that terminal gives its input to.
    #[test]
    fn a_process_is_in_front_when_its_group_is_the_foreground_group_of_the_panes_terminal() {
        // The pane's own process 40532 owns the terminal, whose foreground group is 40540.
        let ps = "40532 40532 40540\n40540 40540 40540\n";
        assert!(in_front_of_the_panes_terminal(ps, 40540, 40532));
        // The rows come in any order, and a member of the group that does not lead it is in front too.
        assert!(in_front_of_the_panes_terminal(
            "40541 40540 40540\n40532 40532 40540\n",
            40541,
            40532
        ));
        // The pane's shell has the terminal back (a job suspended with C-z), or the job runs in the
        // background: the pid's group is not the foreground group.
        assert!(!in_front_of_the_panes_terminal(
            "40532 40532 40532\n40540 40540 40532\n",
            40540,
            40532
        ));
    }

    /// A pid the system gave to a program in front of ANOTHER terminal leads the foreground group of that
    /// terminal, and that is no evidence for this one: the pane's own terminal is the one asked.
    #[test]
    fn a_process_in_front_of_another_terminal_is_not_in_front_of_the_pane() {
        let ps = "40532 40532 40532\n51000 51000 51000\n";
        assert!(!in_front_of_the_panes_terminal(ps, 51000, 40532));
        // Asked of its own terminal, as it was, it would have been in front.
        assert!(in_front_of_the_panes_terminal(ps, 51000, 51000));
    }

    /// No such process, no pane's process, no terminal (`ps` prints 0 on macOS and -1 on Linux), and what
    /// is not rows of three numbers: nothing is in front.
    #[test]
    fn a_process_that_cannot_be_read_is_not_in_front_of_the_pane() {
        for ps in [
            "",
            "\n",
            "40540 40540 40532\n",
            "40532 40532 40532\n",
            "40532 40532 0\n40540 40540 0\n",
            "40532 40532 -1\n40540 40540 -1\n",
            "40532 40532 40540\n40540 0 40540\n",
            "40532 40532 40540\n40540 -1 40540\n",
            "40532 40532 40540\n40540 40540\n",
            "x y z\n",
        ] {
            assert!(!in_front_of_the_panes_terminal(ps, 40540, 40532), "{ps:?}");
        }
        // The pid is the pane's own process: one row, asked twice.
        assert!(in_front_of_the_panes_terminal(
            "40532 40532 40532\n",
            40532,
            40532
        ));
    }

    /// The newest child is the one started last, whatever its pid: once pids have wrapped, a job the
    /// shell left in the background an hour ago can hold a higher pid than the agent it has just started.
    #[test]
    fn the_newest_child_is_the_one_started_last_whatever_its_pid() {
        // The shell 500 has a job from an hour ago, 98000, and the agent it started two seconds ago, 1200.
        let ps = "98000   500 98000  1200   01:00:00\n 1200   500  1200  1200      00:02\n";
        assert_eq!(newest_of(parse_procs(ps).iter()), Some(1200));
        // In the other order, the same.
        let ps = " 1200   500  1200  1200      00:02\n98000   500 98000  1200   01:00:00\n";
        assert_eq!(newest_of(parse_procs(ps).iter()), Some(1200));
        // Two started in the same second (`etime` counts whole seconds): the higher pid.
        let ps = "1201 500 1201 1201 00:02\n1200 500 1200 1201 00:02\n";
        assert_eq!(newest_of(parse_procs(ps).iter()), Some(1201));
        assert_eq!(newest_of(parse_procs("").iter()), None);
    }

    #[test]
    fn the_time_a_process_has_run_reads_in_every_shape_ps_prints_it() {
        assert_eq!(elapsed_seconds("00:05"), Some(5));
        assert_eq!(elapsed_seconds("01:02"), Some(62));
        assert_eq!(elapsed_seconds("01:02:03"), Some(3723));
        assert_eq!(
            elapsed_seconds("2-03:04:05"),
            Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)
        );
        for not_a_time in ["", "5", "x:y", "1:2:3:4", "-01:02", "a-01:02", "01:-2"] {
            assert_eq!(elapsed_seconds(not_a_time), None, "{not_a_time:?}");
        }
    }

    #[test]
    fn a_row_that_is_not_a_process_is_left_out() {
        let ps = "1200 500 1200 1200 00:02\nx\n1201 500 1201 -1 00:03 more\n\n1202 500 1202 0 1-00:00:00\n";
        let pids: Vec<u32> = parse_procs(ps).iter().map(|proc| proc.pid).collect();
        assert_eq!(pids, [1200, 1202]);
    }

    /// The child of a pane's shell in front of its terminal is in the group the shell says that terminal
    /// gives its input to: a helper in the background, or a job the user left there, is not, however new.
    #[test]
    fn the_newest_child_in_front_of_the_terminal_is_the_one_in_its_foreground_group() {
        // The shell 500 runs the agent 1300 in front; a helper 1400 runs in the background, newer, and an
        // old job 900 as well.
        let ps = "500 400 500 1300 01:00\n1300 500 1300 1300 00:01\n1400 500 1400 1300 00:00\n900 500 900 1300 00:50\n";
        assert_eq!(newest_in_front(&parse_procs(ps), 500), Some(1300));
        // Two in front, one after the other's start: the newer.
        let ps = "500 400 500 1300 01:00\n1300 500 1300 1300 00:09\n1301 500 1300 1300 00:01\n";
        assert_eq!(newest_in_front(&parse_procs(ps), 500), Some(1301));
        // The shell in front, and a child in the shell's own group (a trap's helper, run without a job
        // of its own): in front.
        let ps = "500 400 500 500 01:00\n1300 500 500 500 00:01\n";
        assert_eq!(newest_in_front(&parse_procs(ps), 500), Some(1300));
        for ps in [
            // The group in front is no child's.
            "500 400 500 1300 01:00\n900 500 900 1300 00:50\n",
            // No terminal: macOS prints 0, Linux -1.
            "500 400 500 0 01:00\n1300 500 1300 1300 00:01\n",
            "500 400 500 -1 01:00\n1300 500 1300 1300 00:01\n",
            // No row for the shell.
            "1300 500 1300 1300 00:01\n",
            // Another shell's child.
            "500 400 500 1300 01:00\n1300 600 1300 1300 00:01\n",
        ] {
            assert_eq!(newest_in_front(&parse_procs(ps), 500), None, "{ps:?}");
        }
    }

    /// The hook a live agent runs descends from the job in front of the agent's pane; the hook of an agent
    /// that has died does not, as launchd took it in and the shell has the terminal back.
    #[test]
    fn the_hook_of_the_agent_in_front_descends_from_the_job_in_front_and_an_orphaned_one_does_not()
    {
        // The pane's shell 500 gives its terminal to the agent's job 1300, whose hook 1400 runs the report.
        let ps = "1 0 1 0 3-00:00:00\n500 400 500 1300 01:00\n1300 500 1300 1300 00:30\n1400 1300 1300 1300 00:00\n";
        assert!(descends_from_the_job_in_front(&parse_procs(ps), 1400, 500));
        // A hook in a group of its own, under a shell in another: the agent above them is in front.
        let ps = "500 400 500 1300 01:00\n1300 500 1300 1300 00:30\n1400 1300 1400 0 00:00\n1450 1400 1450 0 00:00\n";
        assert!(descends_from_the_job_in_front(&parse_procs(ps), 1450, 500));
        // The agent is the second process of a pipeline (`tee log | claude`): in the job in front, though
        // the job's leader is no ancestor of its hook.
        let ps = "500 400 500 1300 01:00\n1300 500 1300 1300 00:30\n1301 500 1300 1300 00:30\n1400 1301 1300 1300 00:00\n";
        assert!(descends_from_the_job_in_front(&parse_procs(ps), 1400, 500));
        // The agent died: launchd took its hook in, and the shell has the terminal back.
        let ps = "1 0 1 0 3-00:00:00\n500 400 500 500 01:00\n1400 1 1300 500 00:00\n";
        assert!(!descends_from_the_job_in_front(&parse_procs(ps), 1400, 500));
        // A report typed at the pane's own prompt: its parent is the shell, which is not in the job in front
        // while the report runs.
        let ps = "500 400 500 1500 01:00\n1500 500 1500 1500 00:00\n";
        assert!(!descends_from_the_job_in_front(&parse_procs(ps), 500, 500));
        // A report from another pane's job.
        let ps = "500 400 500 1300 01:00\n1300 500 1300 1300 00:30\n600 400 600 2300 01:00\n2300 600 2300 2300 00:10\n";
        assert!(!descends_from_the_job_in_front(&parse_procs(ps), 2300, 500));
        for ps in [
            // No row for the pane's process.
            "1300 500 1300 1300 00:30\n1400 1300 1300 1300 00:00\n",
            // Its terminal has no group in front (macOS prints 0, Linux -1).
            "500 400 500 0 01:00\n1300 500 1300 1300 00:30\n1400 1300 1300 1300 00:00\n",
            "500 400 500 -1 01:00\n1300 500 1300 1300 00:30\n1400 1300 1300 1300 00:00\n",
            // No row for the reporter's parent.
            "500 400 500 1300 01:00\n1300 500 1300 1300 00:30\n",
        ] {
            assert!(
                !descends_from_the_job_in_front(&parse_procs(ps), 1400, 500),
                "{ps:?}"
            );
        }
        // A loop in what `ps` printed ends the walk.
        let ps = "500 400 500 1300 01:00\n1400 1401 1400 0 00:00\n1401 1400 1401 0 00:00\n";
        assert!(!descends_from_the_job_in_front(&parse_procs(ps), 1400, 500));
    }

    /// A pid that names no process has no child in front of its terminal, and is in front of nothing.
    #[test]
    fn the_process_table_finds_nothing_in_front_for_a_process_that_is_not_there() {
        assert!(newest_children_in_front(&[u32::MAX - 1]).is_empty());
        assert!(!process_in_front(u32::MAX - 1, std::process::id()));
    }

    /// A wait on the looks for the process a command started ends at its bound only after three looks, and
    /// past it only once the last two agree, or after six.
    #[test]
    fn a_wait_on_the_looks_ends_after_three_once_two_agree_and_after_six_whatever_they_found() {
        assert!(!looked_enough(2, || true, true), "two looks");
        assert!(looked_enough(3, || true, true));
        assert!(!looked_enough(3, || true, false), "the last two disagree");
        assert!(!looked_enough(5, || true, false), "the last two disagree");
        assert!(looked_enough(6, || true, false));
        assert!(
            !looked_enough(9, || false, true),
            "the bound has not run out"
        );
    }

    /// Only a pid the system says no process has is dead: this process is alive, so is one this
    /// process may not signal (pid 1, another user's, as a profile that became `sudo ...` is), and
    /// so, on the safe side, is a number no process can have. A child that has ended and been
    /// reaped is dead.
    #[test]
    fn a_process_is_dead_only_when_the_system_says_there_is_no_such_process() {
        assert!(process_alive(std::process::id()));
        assert!(
            process_alive(1),
            "a process that may not be signalled is there"
        );
        assert!(process_alive(0));
        assert!(process_alive(u32::MAX));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!process_alive(pid), "{pid}");
    }

    /// A pane with no process (pid 0, a pane kept after its program ended on tmux 3.8) has no process to
    /// look at: none of the process table's readers asks `ps` or `pgrep` about process 0 (the kernel's own,
    /// whose children `pgrep -P 0` lists as launchd), or takes it for a pane's shell.
    #[test]
    fn a_pane_with_no_process_is_never_looked_up_in_the_process_table() {
        let boom = |_: u32| -> Option<String> { panic!("ps was asked about process 0") };
        assert_eq!(shell_idle_by(0, boom), None);
        assert!(!in_front_by(0, 7, |_| panic!("ps was asked for pid 0")));
        assert!(!in_front_by(7, 0, |_| panic!(
            "ps was asked for pane pid 0"
        )));
        assert!(in_front_by(7, 8, |list| {
            assert_eq!(list, "7,8");
            Some("7 7 7\n8 8 7\n".to_string())
        }));
        let found = newest_children_by(
            &[0],
            |_| panic!("pgrep -P was asked for 0"),
            |_| panic!("ps was asked for 0"),
        );
        assert!(found.is_empty());
        // A live pane next to a dead one is still looked up, without the 0.
        let found = newest_children_by(
            &[0, 40],
            |parents| {
                assert_eq!(parents, [40]);
                Some(vec![41])
            },
            |pids| {
                assert_eq!(pids, [40, 41]);
                parse_procs("40 1 40 41 01:00\n41 40 41 41 00:05\n")
            },
        );
        assert_eq!(found.get(&40), Some(&41));
        assert_eq!(found.len(), 1);
        // Process 0 is in `ps -A` on macOS (kernel_task): it is no pane's process with a job in front.
        let procs = parse_procs("0 0 0 9 01:00\n600 1 9 0 00:01\n");
        assert!(!descends_from_the_job_in_front(&procs, 600, 0));
    }

    #[test]
    fn the_process_table_answers_for_a_real_process() {
        // This process has no terminal of its own under `cargo test`, or is not a shell: it is
        // never said to be an idle shell. And a process that is gone cannot be told.
        assert_ne!(pane_shell_is_idle(std::process::id()), Some(true));
        assert_eq!(pane_shell_is_idle(u32::MAX - 1), None);
    }

    /// The rule for agent names reads a shell that cannot be told as not idle, so its pane keeps
    /// the name: a process that is gone, and a pane with no process (a dead one), which is never
    /// asked about.
    #[test]
    fn a_shell_that_cannot_be_told_is_not_known_idle() {
        let mut pane = crate::rail::tests_support::pane_with_state(None);
        pane.pane_pid = u32::MAX - 1;
        assert!(!pane_shell_known_idle(&pane));
        pane.pane_pid = 0;
        assert!(!pane_shell_known_idle(&pane));
    }

    /// Where [`the_pane_shell_check_run_from_inside_its_own_group_sees_the_whole_group`] has the probe
    /// write what it saw; the probe is that test run again, inside the terminal.
    const PROBE_OUT: &str = "TAW_IDLE_PROBE_OUT";

    /// Not a test of its own: the part of the one below that runs inside the terminal, as a process in
    /// the group of the shell that leads it. It asks [`pane_shell_is_idle`] about its parent, that
    /// shell, the way the hook of an agent (`taw pane report-agent-session`) runs inside the group of
    /// the pane's shell, and writes the answer to the file the test named.
    #[test]
    fn idle_probe_inside_the_group() {
        let Some(out) = std::env::var_os(PROBE_OUT) else {
            return;
        };
        let shell = std::os::unix::process::parent_id();
        std::fs::write(out, format!("{:?}", pane_shell_is_idle(shell))).unwrap();
    }

    /// On macOS `pgrep -g` without `-a` leaves out every ancestor of `pgrep`, so asked from inside the
    /// group it was asked about, it lists nobody for a shell that has one child, which reads as "cannot
    /// say" and not as "the shell is not alone". Elsewhere plain `pgrep -g` lists the whole group. A
    /// shell on a terminal of its own (`script`) runs a probe in its group, and the probe is told the
    /// shell is busy, not that nothing can be said.
    #[test]
    fn the_pane_shell_check_run_from_inside_its_own_group_sees_the_whole_group() {
        if std::env::var_os(PROBE_OUT).is_some() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("taw-idle-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("answer");
        let exe = std::env::current_exe().unwrap();
        // `; :` keeps the shell from replacing itself with the probe, so the probe's parent is the shell.
        let line = format!(
            "'{}' --exact watch::tests::idle_probe_inside_the_group --test-threads=1; :",
            exe.display()
        );
        let mut script = tests_support::script_command(&line);
        let status = script
            .env(PROBE_OUT, &out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let seen = std::fs::read_to_string(&out);
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(seen) = seen else {
            eprintln!("skipped: no `script` to give a shell a terminal ({status:?})");
            return;
        };
        assert_eq!(seen, "Some(false)");
    }

    #[test]
    fn backoff_delay_grows_then_caps_at_thirty_seconds() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(16));
        assert_eq!(backoff_delay(6), Duration::from_secs(30));
        // A watcher lives for days: the counter must not overflow into a short delay.
        assert_eq!(backoff_delay(64), Duration::from_secs(30));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(30));
        // Defensive: a zero count is not reachable from `pump`, but must not panic.
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn a_missing_panes_display_expansion_is_recognised_as_no_pane() {
        // What tmux 3.6b actually answers for `-t %999`: the literal tabs of the format,
        // with every column expanded to nothing. Not an empty string.
        let missing = "\t".repeat(DISPLAY_FIELDS - 1);
        assert_eq!(missing.len(), DISPLAY_FIELDS - 1);
        assert!(
            !missing.is_empty(),
            "the old `is_empty` check was dead code"
        );
        assert!(describes_no_pane(&missing));
        // Degenerate answers count too (a one-column format, or no output at all).
        assert!(describes_no_pane(""));
        assert!(describes_no_pane("   \t \t  "));
        // A live pane always fills several columns, including for a dead process.
        let live = [
            "zsh",
            "studio.local",
            "1",
            "0",
            "",
            "",
            "",
            "",
            "",
            "one",
            "0",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "0",
            "0",
            "",
            "",
            "",
            "",
            "3.6b:1",
        ]
        .join("\t");
        assert_eq!(live.split('\t').count(), DISPLAY_FIELDS);
        assert!(!describes_no_pane(&live));
        // Even the barest live pane: no command name, no session name, just the numbers.
        let bare = [
            "", "", "0", "0", "", "", "", "", "", "", "0", "", "", "", "", "", "", "", "", "", "0",
            "0", "",
        ]
        .join("\t");
        assert!(!describes_no_pane(&bare));
    }

    #[test]
    fn only_a_missing_pane_or_server_retires_the_watcher() {
        let gone = |text: &str| is_gone(&anyhow::anyhow!(text.to_string()));
        assert!(gone(
            "`tmux display-message` failed (exit status: 1): can't find pane: %9"
        ));
        assert!(gone("no server running on /private/tmp/tmux-501/default"));
        assert!(gone(
            "error connecting to /private/tmp/tmux-501/taw (No such file or directory)"
        ));
        // Transient: retrying these is right.
        assert!(!gone("`tmux display-message` failed: too many files open"));
        assert!(!gone(
            "failed to run `tmux`: Resource temporarily unavailable"
        ));
        assert!(!gone("can't find window: @9"));
        assert!(!gone(""));
        // Matched case-insensitively, through a context chain.
        let chained = anyhow::anyhow!("CAN'T FIND PANE: %9").context("reading pane %9");
        assert!(is_gone(&chained));
    }

    #[test]
    fn pipe_command_words_survive_the_shell_and_tmuxs_format_expansion() {
        assert_eq!(
            pipe_command_word("/usr/local/bin/taw"),
            "/usr/local/bin/taw"
        );
        assert_eq!(
            pipe_command_word("/opt/my taw/bin/taw"),
            "'/opt/my taw/bin/taw'"
        );
        // tmux would otherwise expand `#{pane_id}` / `#S` inside the command.
        assert_eq!(
            pipe_command_word("/opt/c#{pane_id}/taw"),
            "'/opt/c##{pane_id}/taw'"
        );
        // ... and strftime would otherwise eat the `%` of every pane id.
        assert_eq!(pipe_command_word("%12"), "'%%12'");
        assert_eq!(pipe_command_word("/opt/50%/taw"), "'/opt/50%%/taw'");
    }

    #[test]
    fn watch_run_command_pins_a_named_socket_ahead_of_the_command() {
        let t = Tmux {
            bin: std::path::PathBuf::from("/usr/bin/tmux"),
            socket: Some(Socket::Name("taw-e2e-7".to_string())),
        };
        let cmd = watch_run_command(&t, Path::new("/opt/taw"), "%3").unwrap();
        // The pin comes first, as a shell env assignment, so `from_env` in the child reads
        // it ahead of any inherited `$TMUX`.
        assert!(
            cmd.starts_with("TAW_TMUX_SOCKET_NAME=taw-e2e-7 "),
            "no socket pin: {cmd}"
        );
        // ... and the pane id is still escaped against tmux's `%`-eating strftime pass.
        assert!(cmd.ends_with("/opt/taw watch run '%%3'"), "{cmd}");
    }

    #[test]
    fn watch_run_command_pins_and_escapes_a_path_socket() {
        let t = Tmux {
            bin: std::path::PathBuf::from("tmux"),
            socket: Some(Socket::Path(std::path::PathBuf::from("/tmp/c#x/taw.sock"))),
        };
        let cmd = watch_run_command(&t, Path::new("/opt/taw"), "%1").unwrap();
        assert!(
            cmd.starts_with("TAW_TMUX_SOCKET_PATH="),
            "no path pin: {cmd}"
        );
        // The `#` in the socket path is doubled so tmux's format pass leaves it intact.
        assert!(cmd.contains("c##x"), "unescaped '#' in the pin: {cmd}");
    }

    #[test]
    fn watch_run_command_resolves_and_pins_a_default_socket_path() {
        // The `None` (default socket) branch resolves `#{socket_path}` against a *stub*
        // tmux, so it is exercised without ever touching a real default server.
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("stub-tmux");
        std::fs::write(
            &stub,
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = '#{socket_path}' ]; then\n    printf '%s\\n' /run/tmux/sentinel.sock\n    exit 0\n  fi\ndone\nexit 0\n",
        )
        .unwrap();
        {
            crate::test_exec::make_executable(&stub);
        }
        let t = Tmux {
            bin: stub,
            socket: None,
        };
        let cmd = watch_run_command(&t, Path::new("/opt/taw"), "%1").unwrap();
        assert!(
            cmd.starts_with("TAW_TMUX_SOCKET_PATH=/run/tmux/sentinel.sock "),
            "default socket not pinned to its resolved path: {cmd}"
        );
    }

    #[test]
    fn parses_a_display_line_into_an_observation() {
        let line = [
            "claude",                // pane_current_command
            "\u{2733} fake task",    // pane_title
            "1",                     // window_active
            "2",                     // session_attached
            "",                      // @agent_kind_hint
            "manifest",              // @agent_source
            "working",               // @agent_state
            "1",                     // @agent_seen
            "claude",                // @agent_kind
            "main",                  // session_name
            "3",                     // window_index
            "Reviewer",              // @agent_display
            "reviewer",              // @agent_name
            "live_turn_working",     // @agent_rule
            "4",                     // @agent_wizard_reload
            "claude",                // @agent_reported_kind
            "conv-1",                // @agent_session_id
            "/s/conv-1.jsonl",       // @agent_session_path
            "1790800000123456789",   // @agent_turn
            "1",                     // @agent_acquiring
            "3 120 8 10 claude --x", // @agent_typed_line
            "130",                   // history_size
            "0",                     // alternate_on
            "1790800000123456790",   // @agent_report_stamp
            "",                      // @agent_container
            "",                      // @agent_sandbox
            "",                      // @agent_sandbox_pid
            "3.6b:01",               // terminal focus: one of two clients' terminals has it
        ]
        .join("\t");
        let o = Observation::parse(&line, "screen text".to_string(), "4;1;40", None).unwrap();
        assert_eq!(o.report_stamp, "1790800000123456790");
        assert_eq!(o.observed.command, "claude");
        assert_eq!(o.observed.title, "\u{2733} fake task");
        assert!(o.observed.window_active);
        assert_eq!(o.observed.session_attached, 2);
        assert_eq!(o.observed.terminal_focus, Some(true));
        assert_eq!(o.observed.kind_hint, None);
        assert_eq!(o.observed.prev.source.as_deref(), Some("manifest"));
        assert_eq!(o.observed.prev.state, Some(AgentState::Working));
        assert!(o.observed.prev.seen);
        assert_eq!(o.observed.prev.kind.as_deref(), Some("claude"));
        assert_eq!(o.observed.prev.rule.as_deref(), Some("live_turn_working"));
        // Read so a kind change can tell whether there is a name to release.
        assert_eq!(o.observed.prev.name.as_deref(), Some("reviewer"));
        // Read so an agent that named itself is not judged by the process in front of it.
        assert_eq!(o.observed.prev.reported_kind.as_deref(), Some("claude"));
        // Read so a replacement drops the session only while it is still the one this read saw.
        assert_eq!(o.observed.prev.session_id.as_deref(), Some("conv-1"));
        // Read so a new session, by its id or by its path, ends the completion the pane had.
        assert_eq!(
            o.observed.prev.session_path.as_deref(),
            Some("/s/conv-1.jsonl")
        );
        // Read so a turn that began and ended between two reads is told from one that did not.
        assert_eq!(o.observed.turn.as_deref(), Some("1790800000123456789"));
        // Read so an agent `taw agent start` launched, or a watcher before this one took, is still
        // being taken: here a mark an earlier taw wrote, which does not say since when.
        assert_eq!(o.observed.acquiring, Some(store::Acquired::Unknown));
        // Read so the screen a replacement is decided on is the rows below the line its command was
        // typed on, wherever the screen has scrolled that line to.
        assert_eq!(
            o.typed_line
                .as_ref()
                .map(typed_line::TypedLine::value)
                .as_deref(),
            Some("3 120 8 10 claude --x")
        );
        assert_eq!(o.history, 130);
        assert!(!o.alternate);
        assert_eq!(o.observed.screen, "screen text");
        assert_eq!(o.observed.osc_progress, "4;1;40");
        assert!(o.observed.foreground());
        assert_eq!(o.session_name, "main");
        assert_eq!(o.window_index, 3);
        assert_eq!(o.label, "Reviewer");
        // The hot-reload token rides along with the rest — see `Watcher::apply_reload`.
        assert_eq!(o.reload_token, "4");
    }

    #[test]
    fn a_notifications_read_when_its_delay_is_up_is_the_state_and_who_holds_it() {
        let (state, now) = due_read("done\tclaude\tconv-2\t/s/conv-2.jsonl");
        assert_eq!(state.as_deref(), Some("done"));
        assert_eq!(
            now,
            state::Earned {
                kind: Some("claude".to_string()),
                session_id: Some("conv-2".to_string()),
                session_path: Some("/s/conv-2.jsonl".to_string()),
            }
        );
        // An unset option expands to nothing, which is none; a pane that is gone answers tabs.
        let (state, now) = due_read("idle\tpi\t\t/s/new.jsonl");
        assert_eq!(state.as_deref(), Some("idle"));
        assert_eq!(now.session_id, None);
        assert_eq!(now.session_path.as_deref(), Some("/s/new.jsonl"));
        let (state, now) = due_read("\t\t\t");
        assert_eq!((state, now), (None, state::Earned::default()));
        // The format reads the four in that order, each folded.
        let at = |option: &str| {
            DUE_FORMAT
                .find(&format!(":{option}}}"))
                .unwrap_or_else(|| panic!("{option} is not read"))
        };
        assert!(at("@agent_state") < at("@agent_kind"));
        assert!(at("@agent_kind") < at("@agent_session_id"));
        assert!(at("@agent_session_id") < at("@agent_session_path"));
    }

    #[test]
    fn parses_a_display_line_for_a_pane_with_no_agent() {
        let line = [
            "zsh", "", "0", "0", "", "", "", "", "", "one", "0", "", "", "", "", "", "", "", "",
            "", "", "0", "0", "", "", "", "", "",
        ]
        .join("\t");
        let o = Observation::parse(&line, String::new(), "", None).unwrap();
        assert!(o.observed.prev.kind.is_none());
        assert_eq!(o.container_mark, None);
        assert_eq!(o.sandbox, None);
        assert_eq!(o.sandbox_pid, None);
        assert_eq!(o.observed.turn, None);
        assert!(o.observed.prev.reported_kind.is_none());
        assert!(o.observed.prev.state.is_none());
        assert!(o.observed.prev.rule.is_none());
        assert!(!o.observed.prev.seen);
        assert!(!o.observed.foreground());
        assert_eq!(o.label, "");
        // An unset global option expands to nothing, which is a token like any other.
        assert_eq!(o.reload_token, "");
        assert_eq!(o.typed_line, None);
        assert_eq!(o.report_stamp, "");
    }

    #[test]
    fn a_pane_whose_terminal_has_lost_focus_is_not_in_front_of_the_user() {
        // The active window of an attached session, and every terminal attached to it has
        // reported losing focus: the user is in another app, which herdr does not count
        // as looking. Unknown focus — `focus-events` off, or a tmux this was not verified
        // on — counts as looking, as herdr's does.
        let line = |focus: &str| {
            [
                "claude", "", "1", "1", "", "manifest", "working", "0", "claude", "one", "0", "",
                "", "", "", "", "", "", "", "", "", "0", "0", "", "", "", "", focus,
            ]
            .join("\t")
        };
        let parsed = |focus: &str| {
            Observation::parse(&line(focus), String::new(), "", None)
                .unwrap()
                .observed
        };
        assert_eq!(parsed("3.6b:0").terminal_focus, Some(false));
        assert!(!parsed("3.6b:0").foreground());
        assert!(
            !parsed("3.6b:00").foreground(),
            "two clients, both unfocused"
        );
        for known_or_not in ["3.6b:1", "3.6b:10", "", "3.6b:", "3.5a:0"] {
            assert!(parsed(known_or_not).foreground(), "{known_or_not:?}");
        }
    }

    #[test]
    fn the_label_falls_back_from_display_to_name_to_kind() {
        let with_name = [
            "claude", "", "0", "0", "", "", "", "", "claude", "s", "0", "", "rev", "", "", "", "",
            "", "", "", "", "", "0", "", "", "", "", "0",
        ]
        .join("\t");
        assert_eq!(
            Observation::parse(&with_name, String::new(), "", None)
                .unwrap()
                .label,
            "rev"
        );
        let bare = [
            "claude", "", "0", "0", "", "", "", "", "claude", "s", "0", "", "", "", "", "", "", "",
            "", "", "", "", "0", "", "", "", "", "0",
        ]
        .join("\t");
        assert_eq!(
            Observation::parse(&bare, String::new(), "", None)
                .unwrap()
                .label,
            "claude"
        );
    }

    /// A steady pane, as the loop actually sees it: an `Observation` parsed from a real
    /// `DISPLAY_FORMAT` line, not a hand-built `AgentRecord`.
    ///
    /// This is the regression `Decision::RuleChanged` invited. `@agent_rule` was not one of
    /// the columns, so `prev.rule` parsed as `None` for ever — and `decide` then answered
    /// `RuleChanged` on *every* evaluation of any pane whose screen matched a rule, one
    /// `set-option` per pane per tick, with the `Keep` arm unreachable in production. The
    /// unit tests in `watch::state` could not catch it: they build `prev` themselves.
    #[test]
    fn an_unchanged_rule_on_a_steady_pane_is_kept_rather_than_rewritten_every_tick() {
        const IDLE: &str = include_str!("../../tests/fixtures/screens/claude-idle.txt");
        let line = [
            "claude",          // pane_current_command
            "",                // pane_title
            "0",               // window_active
            "0",               // session_attached
            "",                // @agent_kind_hint
            "manifest",        // @agent_source
            "idle",            // @agent_state
            "1",               // @agent_seen
            "claude",          // @agent_kind
            "main",            // session_name
            "0",               // window_index
            "",                // @agent_display
            "",                // @agent_name
            "live_prompt_box", // @agent_rule — what the idle screen below matches
            "",                // @agent_wizard_reload
            "",                // @agent_reported_kind
            "",                // @agent_session_id
            "",                // @agent_session_path
            "",                // @agent_turn
            "",                // @agent_acquiring
            "",                // @agent_typed_line
            "0",               // history_size
            "0",               // alternate_on
            "",                // @agent_report_stamp
            "",                // @agent_container
            "",                // @agent_sandbox
            "",                // @agent_sandbox_pid
            "",                // terminal focus: `focus-events` off
        ]
        .join("\t");
        let o = Observation::parse(&line, IDLE.to_string(), "", None).unwrap();
        assert_eq!(
            o.observed.prev.rule.as_deref(),
            Some("live_prompt_box"),
            "the rule already on the pane has to reach the decision, or it cannot be \
             compared with the one that matches now"
        );

        let store = ManifestStore::new(
            std::path::PathBuf::from("/nonexistent/taw-test-override"),
            std::path::PathBuf::from("/nonexistent/taw-test-remote"),
        );
        let decision = decide(
            &store,
            &mut HashMap::new(),
            &o.observed,
            false,
            Instant::now(),
            &mut state::Arbiter::default(),
        );
        assert_eq!(
            decision,
            Decision::Keep,
            "nothing moved, so nothing may be written"
        );
    }

    /// The display line of a pane that holds a Codex, as the loop reads it: `command` is what
    /// tmux says runs in it (`node` for npm's, which only the `hint` then names), `title` what
    /// tmux says its title is, and the record is the one a watcher left at rest.
    fn codex_at_rest(command: &str, hint: &str, title: &str, state: &str) -> String {
        [
            command,    // pane_current_command
            title,      // pane_title
            "0",        // window_active
            "0",        // session_attached
            hint,       // @agent_kind_hint
            "manifest", // @agent_source
            state,      // @agent_state
            "1",        // @agent_seen
            "codex",    // @agent_kind
            "main",     // session_name
            "0",        // window_index
            "",         // @agent_display
            "",         // @agent_name
            "",         // @agent_rule
            "",         // @agent_wizard_reload
            "",         // @agent_reported_kind
            "",         // @agent_session_id
            "",         // @agent_session_path
            "",         // @agent_turn
            "",         // @agent_acquiring
            "",         // @agent_typed_line
            "0",        // history_size
            "0",        // alternate_on
            "",         // @agent_report_stamp
            "",         // @agent_container
            "",         // @agent_sandbox
            "",         // @agent_sandbox_pid
            "",         // terminal focus
        ]
        .join("\t")
    }

    /// What tmux titles a pane nobody titled, for the tests below: any host name will do.
    const HOST: &str = "studio.local";

    #[test]
    fn a_pane_tmux_titled_itself_is_observed_with_no_title() {
        let line = codex_at_rest("codex", "", HOST, "idle");
        let o = Observation::parse(&line, String::new(), "", Some(HOST)).unwrap();
        assert_eq!(o.observed.title, "", "the host name is no evidence");

        // A program that set a title: the rules read it, whatever the host is called.
        let line = codex_at_rest("codex", "", "project", "idle");
        let o = Observation::parse(&line, String::new(), "", Some(HOST)).unwrap();
        assert_eq!(o.observed.title, "project");
        // With no host name to compare with, a title is a title.
        let line = codex_at_rest("codex", "", HOST, "idle");
        let o = Observation::parse(&line, String::new(), "", None).unwrap();
        assert_eq!(o.observed.title, HOST);
    }

    /// herdr's panes start with no title, and its Codex manifest reads a Codex idle through a
    /// title, any title that is not a spinner: a Codex that never set one is read from the
    /// screen. In tmux every pane starts with a title, the host name, and a read that took it
    /// for Codex's would call every such Codex at rest, whatever its screen shows.
    #[test]
    fn a_codex_that_never_set_a_title_is_read_from_its_screen_as_herdrs_is() {
        let store = ManifestStore::new(
            std::path::PathBuf::from("/nonexistent/taw-test-override"),
            std::path::PathBuf::from("/nonexistent/taw-test-remote"),
        );
        // A frame no screen rule reads: nothing is drawn.
        let read = |line: &str| {
            let o = Observation::parse(line, "\n".to_string(), "", Some(HOST)).unwrap();
            decide(
                &store,
                &mut HashMap::new(),
                &o.observed,
                false,
                Instant::now(),
                &mut state::Arbiter::default(),
            )
        };

        // A Codex tmux names by its process, at rest: with no title nothing reads it, and
        // the known-agent fallback leaves it idle, with no rule. With a title of its own,
        // herdr's rule reads it idle.
        assert_eq!(
            read(&codex_at_rest("codex", "", HOST, "idle")),
            Decision::Keep,
            "tmux's own title is no title"
        );
        assert_eq!(
            read(&codex_at_rest("codex", "", "project", "idle")),
            Decision::RuleChanged {
                rule: Some("osc_title_idle".to_string())
            }
        );

        // A Codex only its launch hint names: with no title and a blank frame nothing says it
        // is there, so it stays unknown. With a title of its own it is idle.
        assert_eq!(
            read(&codex_at_rest("node", "codex", HOST, "unknown")),
            Decision::Keep,
            "tmux's own title is no title"
        );
        assert!(
            matches!(
                read(&codex_at_rest("node", "codex", "project", "unknown")),
                Decision::Transition {
                    to: AgentState::Idle,
                    ..
                }
            ),
            "a title a program set is read"
        );
    }

    /// A write of the reader's that is too long for one tmux command is a refused report, which the
    /// read goes on from; a write that landed, one that did not, and any other failure stay as they were.
    #[test]
    fn a_reader_write_too_long_for_one_tmux_command_is_a_refused_report_not_a_failed_read() {
        let too_long = anyhow::Error::new(store::TooLong { bytes: 20_413 })
            .context("writing the sandboxed agent's session for pane '%1'");
        assert_eq!(refused_if_too_long(Err(too_long)).unwrap(), None);
        assert_eq!(refused_if_too_long(Ok(true)).unwrap(), Some(true));
        assert_eq!(refused_if_too_long(Ok(false)).unwrap(), Some(false));
        assert!(refused_if_too_long(Err(anyhow::anyhow!("tmux is gone"))).is_err());
    }

    #[test]
    fn a_display_line_carries_the_container_mark_the_profile_mark_and_its_process() {
        let mut fields = vec![""; DISPLAY_FIELDS];
        fields[0] = "docker";
        fields[DISPLAY_FIELDS - 4] = "docker";
        fields[DISPLAY_FIELDS - 3] = "default";
        fields[DISPLAY_FIELDS - 2] = "4242";
        fields[DISPLAY_FIELDS - 1] = "3.6b:0";
        let observation = Observation::parse(&fields.join("\t"), String::new(), "", None).unwrap();
        assert_eq!(observation.container_mark.as_deref(), Some("docker"));
        assert_eq!(observation.sandbox.as_deref(), Some("default"));
        assert_eq!(observation.sandbox_pid, Some(4242));
        assert_eq!(observation.sandbox_pid_text, "4242");
        assert_eq!(
            observation.observed.terminal_focus,
            Some(false),
            "the focus is still the last column"
        );

        // A process that is no pid is none, and its text is what the mark is taken off by.
        fields[DISPLAY_FIELDS - 2] = "not-a-pid";
        let observation = Observation::parse(&fields.join("\t"), String::new(), "", None).unwrap();
        assert_eq!(observation.sandbox_pid, None);
        assert_eq!(observation.sandbox_pid_text, "not-a-pid");
    }

    /// A look that could not see into the runtime in front of the pane keeps the agent the look
    /// before found in that runtime, and nothing else: not once it has been given up on, not for
    /// another runtime or a program that is none, not when it saw in and found no agent, and not
    /// with nothing found before.
    #[test]
    fn a_look_that_could_not_see_into_the_runtime_keeps_the_agent_last_found_in_it() {
        let claude = detect::ContainerAgent {
            runtime: "docker",
            kind: "claude".to_string(),
        };
        assert!(keeps_the_last_container(
            Some(&claude),
            "docker",
            true,
            false
        ));
        assert!(keeps_the_last_container(
            Some(&claude),
            "com.docker.cli",
            true,
            false
        ));
        assert!(!keeps_the_last_container(
            Some(&claude),
            "docker",
            true,
            true
        ));
        assert!(!keeps_the_last_container(
            Some(&claude),
            "podman",
            true,
            false
        ));
        assert!(!keeps_the_last_container(Some(&claude), "vim", true, false));
        assert!(!keeps_the_last_container(
            Some(&claude),
            "docker",
            false,
            false
        ));
        assert!(!keeps_the_last_container(None, "docker", true, false));
    }

    #[test]
    fn of_two_reads_owed_whatever_the_pane_prints_the_earlier_is_owed() {
        let base = Instant::now();
        assert_eq!(earliest(None, None), None);
        assert_eq!(
            earliest(None, Some(Forced::From(at(base, 900)))),
            Some(Forced::From(at(base, 900)))
        );
        assert_eq!(
            earliest(Some(Forced::From(at(base, 900))), None),
            Some(Forced::From(at(base, 900)))
        );
        assert_eq!(
            earliest(
                Some(Forced::From(at(base, 900))),
                Some(Forced::From(at(base, 400)))
            ),
            Some(Forced::From(at(base, 400)))
        );
        assert_eq!(
            earliest(Some(Forced::From(at(base, 100))), Some(Forced::NextTick)),
            Some(Forced::NextTick)
        );
        assert_eq!(
            earliest(Some(Forced::NextTick), Some(Forced::From(at(base, 100)))),
            Some(Forced::NextTick)
        );
    }

    #[test]
    fn a_display_line_with_the_wrong_field_count_is_an_error_not_a_misread() {
        let short = ["claude", "", "0"].join("\t");
        let err = Observation::parse(&short, String::new(), "", None).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("expected {DISPLAY_FIELDS}")),
            "unexpected error: {err}"
        );
    }
}
