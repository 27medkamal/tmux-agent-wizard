// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! The watcher's decision function: what one observation of a pane means.
//!
//! [`decide`] is the whole of the watcher's judgement, and it is pure apart from loading
//! manifests (cached per agent kind): observation in, [`Decision`] out. It never touches
//! tmux — `watch::run` reads the observation from tmux and applies the decision back to
//! it. Keeping the two apart is what makes the transition table testable without a tmux
//! server.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::detect::{self, LoadedManifest, ManifestStore, Snapshot};
use crate::notify::{self, Class};
use crate::store::{self, Acquired, AgentRecord, AgentState};

/// herdr's `AGENT_PENDING_IDLE_CONFIRMATIONS`: how many further plain-idle reads release
/// a held `working -> idle`.
pub const IDLE_HOLD_CONFIRMATIONS: u8 = 3;
/// herdr's `AGENT_PENDING_IDLE_CAP`: the longest a `working -> idle` is held, however few
/// confirmations it has had.
pub const IDLE_HOLD_CAP: Duration = Duration::from_millis(700);
/// herdr's `AGENT_STARTUP_GRACE_WINDOW`: how long a newly identified agent reads
/// `unknown` before its screen is believed — plus one more tick.
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);
/// herdr's `AGENT_MISS_CONFIRMATION_ATTEMPTS`: the miss that clears an agent another
/// foreground process has been in front of — five are survived.
pub const IDENTITY_MISSES: u8 = 6;
/// herdr's `PROCESS_RECHECK_IDENTIFIED`: while another process stays in front of the
/// agent, one more miss is counted this often — the default of the scheduler's
/// `identity_recheck`, which [`Arbiter::new`] is given.
pub const IDENTITY_RECHECK: Duration = Duration::from_secs(5);
/// herdr's `SELF_REPORTED_AGENT_SHELL_RECHECK`: how often the pane's shell is asked about while
/// an agent that reports for itself, and that no process names, is in the pane.
pub const SELF_REPORTED_AGENT_SHELL_RECHECK: Duration = Duration::from_secs(1);
/// The names of a pane's shell, herdr's own list (`is_pane_shell_process_name`,
/// `src/platform/mod.rs`): what `#{pane_current_command}` is when the shell has the terminal,
/// and what the pane's own process is called when it is a shell at its prompt ([`is_shell`] says
/// how a name is matched). A shell that is not named here is not known to be back, so it is
/// judged as any other command is: an agent taw cannot identify keeps its record (a hook-owned
/// one until `taw pane release-agent`), and any other loses it on the sixth miss.
pub const SHELLS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "csh",
    "tcsh",
    "elvish",
    "xonsh",
    "nu",
    "pwsh",
    "powershell",
    "cmd",
];
/// The rule an exit's `idle` is written with: nothing on screen explains it, the process
/// table does.
const PROCESS_EXITED_RULE: &str = "process_exited";

/// What the watcher carries from one read to the next to arbitrate them as herdr's pane
/// detection task does. It lives in the watcher's memory, as herdr's lives in its task;
/// tmux still holds everything the popup, the rail and the CLI read.
#[derive(Debug)]
pub struct Arbiter {
    /// How often the watcher rechecks a quiet agent pane (`Timing::identity_recheck`),
    /// which is how often a process kept in front of the agent is counted another miss.
    identity_recheck: Duration,
    /// The agent this watcher has identified in the pane, `None` before it has.
    identity: Option<String>,
    hold: Option<Hold>,
    grace: Grace,
    /// An exit's `idle` has been written; the next read clears the record (herdr's
    /// `pending_foreground_shell_clear`).
    exit_pending: bool,
    /// Reads that found another process in front of the agent (herdr's
    /// `consecutive_misses`), and when the last one was counted.
    misses: u8,
    last_miss: Option<Instant>,
    /// The foreground command at the last read: a change of it is a miss at once.
    last_command: String,
    /// What the last agent to leave the pane left on it. Kept when everything else about
    /// that agent is forgotten: it is about the pane, and the next occupant must not
    /// read it as its own.
    evidence: Evidence,
    /// What is known of the record itself, which an agent's own report puts on the pane: whether
    /// a process has ever named its agent. Kept when the agent is forgotten (`reset`), and lost
    /// with the record.
    reported: Reported,
    /// The agent is starting: from the read that identified it at a real start (a relaunch, or a
    /// first identification or a replacement while the pane's record holds no state, `unknown`, or
    /// another agent's) until the first read on which a rule of its kind matches, any state's, an
    /// overlay's too. Until then it has drawn nothing its rules know, and for a kind whose rules know
    /// its idle screen, rows below the line its command was typed on that none of them matches (an
    /// install's progress) are no idle of its own (step 5). A watcher started again identifies anew
    /// an agent that drew long ago, and opens no window for it.
    starting: bool,
    /// What the last read left the pane's state as (see [`Taken`]). Kept when the agent is
    /// forgotten: it says what the watcher has announced of the pane, which is about the pane.
    taken: Taken,
    /// herdr's `agent_process_acquisition_pending` (step 11 of [`decide`]): the agent was taken
    /// just now and has not been seen working since, so its first `idle` is no completion; and
    /// when it was taken, which `@agent_acquiring` keeps for a watcher started again.
    acquiring: Option<Acquired>,
    /// The last read found the pane's record empty and the pane marked as being taken (`taw agent
    /// start` or a restore has just started an agent in it), and set aside what was on the pane then
    /// (step 10). Once per start: a later read of the same start keeps the title tmux reports since.
    start_evidence_dropped: bool,
    /// The session the last read found the pane's agent in, `None` before the first read and once
    /// the record has gone: a read that finds another is the agent's session changing.
    session: Option<Session>,
    /// herdr's completion reset (step 12 of [`decide`]) happened at the last read: the agent's
    /// session changed, or another agent took the pane. Taken by the caller, which marks the pane
    /// seen ([`Update::finish`]).
    reset: bool,
    /// What the last read leaves `@agent_acquiring` as, when that is not what the pane holds: the
    /// mark the caller writes ([`Update::finish`]).
    mark: Option<MarkWrite>,
}

/// A write of `@agent_acquiring` (`store::ACQUIRING_OPTION`) that a read leaves to its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkWrite {
    /// The agent is being taken, since the moment the mark says.
    Set(Acquired),
    /// It is not being taken any more.
    Unset,
}

/// The session of the agent a pane records, as a read finds it: `@agent_session_id` and
/// `@agent_session_path`, which the agent's own hook reports, and the agent they were reported
/// for (`@agent_kind`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Session {
    kind: Option<String>,
    id: Option<String>,
    path: Option<String>,
}

impl Session {
    fn of(record: &AgentRecord) -> Session {
        Session {
            kind: record.kind.clone(),
            id: record.session_id.clone(),
            path: record.session_path.clone(),
        }
    }

    /// What `now`, the session a read finds, is to this one, the session the last read found:
    /// herdr's `session_ref_changed`, and its rebinding where the agent had a session before.
    fn change_to(&self, now: &Session) -> SessionChange {
        // Another agent, or none: what goes with a change of agent is the business of the read
        // that finds the new one, not of its session.
        if now.kind.is_none()
            || now.kind != self.kind
            || (now.id == self.id && now.path == self.path)
        {
            return SessionChange::None;
        }
        if self.id.is_some() || self.path.is_some() {
            SessionChange::Replaced
        } else {
            SessionChange::First
        }
    }
}

/// How the session of a pane's agent changed between two reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionChange {
    /// The same session, or another agent.
    None,
    /// The agent reported a session where the last read found none.
    First,
    /// The agent's session is another than the one the last read found: Claude's `/clear` and
    /// `/resume`, Pi's `/new`, a restore's resume. herdr's rebinding, which takes the agent
    /// again (step 11).
    Replaced,
}

impl SessionChange {
    /// Whether the session changed at all: herdr's completion reset (step 12).
    fn changed(self) -> bool {
        self != SessionChange::None
    }
}

/// What a read changes of what the arbiter carries to the next one, kept from before it: the read
/// that retries one whose decision could not be written has to find the same changes again, as
/// changes (see [`Arbiter::restore`]).
#[derive(Clone, Debug)]
pub struct Checkpoint {
    taken: Taken,
    acquiring: Option<Acquired>,
    session: Option<Session>,
}

/// The state the last read of a pane left it in, and the agent it recorded then, once its change
/// has been announced as the status event, the `agent.*` hooks and the toast it earns.
///
/// A change read off the screen is one because the read finds another state than tmux holds. A
/// report is written to tmux by the report itself, before any read, so what a read finds there is
/// always the newest, and whether it is a change can only be told from what the last read left.
/// That is what this is, for a pane the screen decides as for one a hook reports for: a hook that
/// takes the pane over finds the state the screen had left it in, and the agent, which is how a
/// different agent reporting the state the pane is in is told from the same one again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Taken {
    /// No read yet. The first takes the pane as it finds it, as a watcher restarting on a pane it
    /// already classified does (no startup grace): what it finds was announced by the watcher
    /// before it, if by any, and is no change of its own.
    #[default]
    Unread,
    /// The last read found no agent, or a record with no state. A report that comes after it is a
    /// change from nothing: a pane gaining an agent.
    Nothing,
    /// What the last read left the pane as.
    State {
        state: AgentState,
        /// The agent the pane recorded at that read, when it recorded one.
        kind: Option<String>,
        /// The stamp of the agent's last report of work or a wait that the read saw
        /// (`@agent_turn`). A report of work moves it, so a read that finds it moved, and the state
        /// the one it took in, knows that a turn began and ended between the two.
        turn: Option<String>,
    },
}

impl Taken {
    /// The pane left in `state` with `kind` recorded, and no turn stamp on it.
    pub fn left(state: AgentState, kind: Option<&str>) -> Taken {
        Taken::State {
            state,
            kind: kind.map(str::to_string),
            turn: None,
        }
    }

    /// The pane left in `state` by the read of `o`, which saw `kind` recorded and the turn stamp `o`
    /// carries.
    fn after(state: AgentState, kind: Option<&str>, o: &Observed) -> Taken {
        Taken::State {
            state,
            kind: kind.map(str::to_string),
            turn: o.turn.clone(),
        }
    }

    /// The pane as the read of `o` leaves it when it changes nothing.
    fn as_read(o: &Observed) -> Taken {
        o.prev.state.map_or(Taken::Nothing, |state| {
            Taken::after(state, o.prev.kind.as_deref(), o)
        })
    }
}

/// Whether taw can identify the agent a pane records, and how often the shell of one it cannot
/// is asked about.
///
/// herdr keeps an agent it cannot identify by its process until the pane's shell is idle
/// (`src/pane.rs`, the self-reported agent) and judges every other by its own misses and exits,
/// so the arbiter remembers which this is: whether the process in front of the pane has named
/// the kind the record holds at any read while that record stands, before its agent reported
/// itself or after. The record clearing, or holding another kind, forgets it.
#[derive(Debug, Default)]
struct Reported {
    /// The kind a process has named in this pane while its record stands.
    named: Option<String>,
    /// When the pane's shell was last asked about ([`Arbiter::wants_shell_probe`]).
    asked: Option<Instant>,
    /// A question that was wanted was held back by the once-a-second limit, and is owed a read
    /// when that is up.
    owed: bool,
}

/// An agent's OSC evidence after the pane has changed away from it — to another agent or
/// to none. herdr drops it then (`clear_osc_evidence_for_agent_transition`, `src/pane.rs`),
/// so a successor never takes its predecessor's title or progress for its own; a first
/// acquisition keeps what its own process printed before it was recognised. tmux keeps a
/// pane's title across processes, so here the title the agent left is set aside and reads
/// as no title at all until tmux reports another.
#[derive(Debug, Default)]
struct Evidence {
    /// The pane's title when its agent left.
    stale_title: Option<String>,
    /// The byte stream's `osc_progress` is the departed agent's: the watcher clears it
    /// after this read ([`Arbiter::take_progress_reset`]), and until it has, reads see
    /// none.
    progress_reset: bool,
}

impl Default for Arbiter {
    fn default() -> Self {
        Arbiter::new(IDENTITY_RECHECK)
    }
}

/// A read owed whether or not the pane printed anything since the last one (see
/// [`Arbiter::forced_read`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forced {
    /// On the next tick: the read after the startup grace ends, or a pending exit's
    /// clear.
    NextTick,
    /// On the first tick at or after this instant: the end of the startup grace.
    From(Instant),
}

/// herdr's startup grace (`agent_startup_grace_until`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Grace {
    #[default]
    Off,
    /// A new agent's first frames say nothing yet: nothing is written before this.
    Until(Instant),
    /// The window's extra tick has been read; the read after it writes what it sees.
    LastTick,
}

/// A `working -> idle` waiting for confirmation: herdr's `PendingIdleConfirmation`.
#[derive(Clone, Copy, Debug)]
struct Hold {
    started: Instant,
    confirmations: u8,
}

impl Arbiter {
    /// The arbiter of a watcher that has identified nothing yet, and rechecks a quiet
    /// agent pane every `identity_recheck`.
    pub fn new(identity_recheck: Duration) -> Arbiter {
        Arbiter {
            identity_recheck,
            identity: None,
            hold: None,
            grace: Grace::Off,
            exit_pending: false,
            misses: 0,
            last_miss: None,
            last_command: String::new(),
            evidence: Evidence::default(),
            reported: Reported::default(),
            starting: false,
            taken: Taken::default(),
            acquiring: None,
            start_evidence_dropped: false,
            session: None,
            reset: false,
            mark: None,
        }
    }

    /// An arbiter that has been watching `kind` in this pane for a while: nothing on
    /// hold, no grace running. What tests of one rule give every other rule.
    #[cfg(test)]
    pub(crate) fn settled(kind: &str) -> Arbiter {
        Arbiter {
            identity: Some(kind.to_string()),
            last_command: kind.to_string(),
            ..Arbiter::default()
        }
    }

    /// The session the last read found in `record`, as a read that left the pane so remembers it.
    #[cfg(test)]
    pub(crate) fn remember_session(&mut self, record: &AgentRecord) {
        self.session = record.kind.is_some().then(|| Session::of(record));
    }

    /// Whether a `working -> idle` verdict is on hold, which the scheduler rechecks
    /// every `hold_recheck` whether or not the pane printed anything.
    pub fn hold_active(&self) -> bool {
        self.hold.is_some()
    }

    /// Whether a new agent's startup grace is running: until it ends, nothing on screen
    /// is believed, and no read captures one (see [`wants_screen`]).
    pub fn in_startup_grace(&self) -> bool {
        matches!(self.grace, Grace::Until(_))
    }

    /// Whether the byte stream's OSC progress was printed by an agent that has since left
    /// the pane, and must be cleared now. `true` once per departure.
    pub fn take_progress_reset(&mut self) -> bool {
        std::mem::take(&mut self.evidence.progress_reset)
    }

    /// Forgets the agent and everything carried for it, except what it left on the pane, what
    /// is known of the record, and how often the watcher reads it.
    fn reset(&mut self) {
        let evidence = std::mem::take(&mut self.evidence);
        let reported = std::mem::take(&mut self.reported);
        let taken = std::mem::take(&mut self.taken);
        *self = Arbiter {
            evidence,
            reported,
            taken,
            ..Arbiter::new(self.identity_recheck)
        };
    }

    /// What this watcher has taken in of the state a hook reports (see [`Taken`]).
    pub fn taken(&self) -> Taken {
        self.taken.clone()
    }

    /// Whether the pane's agent is still being taken (herdr's `agent_process_acquisition_pending`):
    /// it was taken just now, as a new agent, one `taw agent start` launched or one whose session
    /// another replaced, and has not been seen working since.
    pub fn acquiring(&self) -> bool {
        self.acquiring.is_some()
    }

    /// A read found the agent working, or found that it reported work since the last read: the
    /// acquisition is over, and its next `idle` ends a turn.
    fn worked(&mut self) {
        self.acquiring = None;
    }

    /// A read found the agent at its prompt. Returns whether that is the first `idle` of an
    /// acquisition, which ends it and is no completion (herdr's
    /// `finish_agent_process_acquisition`): the agent became ready, it did not finish anything.
    fn first_idle(&mut self) -> bool {
        self.acquiring.take().is_some()
    }

    /// Whether the turn stamp `turn` (`@agent_turn`) has moved since the agent being taken was taken,
    /// as its mark says: a report of work or of a wait has come since. A mark that does not say when
    /// it was taken says nothing of it.
    fn reported_since_taken(&self, turn: Option<&str>) -> bool {
        matches!(&self.acquiring, Some(Acquired::At(at)) if at.as_deref() != turn)
    }

    /// Whether herdr's completion reset happened at the last read (step 12 of [`decide`]): its
    /// caller marks the pane seen ([`Update::finish`]). `true` once per reset.
    pub fn take_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset)
    }

    /// What the last read leaves the pane's `@agent_acquiring` mark as, when that is not what the
    /// pane holds (step 11 of [`decide`]). `Some` once per change.
    pub fn take_mark(&mut self) -> Option<MarkWrite> {
        self.mark.take()
    }

    /// Puts back what [`Arbiter::taken`] said before a read whose decision could not be written:
    /// the change it found was not announced, and the read that retries it must find it again.
    pub fn retake(&mut self, taken: Taken) {
        self.taken = taken;
    }

    /// What a read is about to change of what this arbiter carries ([`Checkpoint`]).
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            taken: self.taken.clone(),
            acquiring: self.acquiring.clone(),
            session: self.session.clone(),
        }
    }

    /// Puts back what `checkpoint` held, before a read whose decision could not be written: the
    /// change it found was not announced, the agent it took is still being taken, and the session
    /// it saw is still news to the read that retries it.
    pub fn restore(&mut self, checkpoint: Checkpoint) {
        self.taken = checkpoint.taken;
        self.acquiring = checkpoint.acquiring;
        self.session = checkpoint.session;
        self.reset = false;
        self.mark = None;
    }

    /// What the foreground command at this read says of the pane's record: `named` is the agent
    /// it names (never the hint), and `recorded` the kind tmux holds. A read that names the
    /// agent makes it one taw can identify, for as long as its record stands: any other kind
    /// than the one recorded, or none, forgets it.
    fn note_command(&mut self, named: Option<&str>, recorded: Option<&str>) {
        match named {
            Some(kind) => self.reported.named = Some(kind.to_string()),
            None if self.reported.named.as_deref() != recorded => self.reported.named = None,
            None => {}
        }
    }

    /// Whether this read is to find out if the pane's own shell is idle
    /// ([`Observed::pane_shell_idle`]). herdr asks it (`report_self_reported_agent_shell_return`)
    /// of an agent that reports for itself and that no process names, at most once a second
    /// ([`SELF_REPORTED_AGENT_SHELL_RECHECK`]). A question held back by that limit is owed a read
    /// when it is up ([`Arbiter::forced_read`]), so the shell's return is noticed about as soon
    /// as asking every second would.
    pub fn wants_shell_probe(&mut self, o: &Observed, now: Instant) -> bool {
        if !self_reported(o, self) {
            self.reported.asked = None;
            self.reported.owed = false;
            return false;
        }
        let waiting = self.reported.asked.is_some_and(|at| {
            now.saturating_duration_since(at) < SELF_REPORTED_AGENT_SHELL_RECHECK
        });
        self.reported.owed = waiting;
        if !waiting {
            self.reported.asked = Some(now);
        }
        !waiting
    }

    /// The pane has changed away from an identified agent while showing `title`: that
    /// title and the agent's progress are not its successor's evidence.
    fn drop_evidence(&mut self, title: &str) {
        self.evidence = Evidence {
            stale_title: Some(title.to_string()),
            progress_reset: true,
        };
    }

    /// A read owed whether or not the pane prints anything: a pending exit's clear, the
    /// end of the startup grace, and the tick after it, which writes the new agent's
    /// first real state.
    pub fn forced_read(&self) -> Option<Forced> {
        if self.exit_pending {
            return Some(Forced::NextTick);
        }
        match self.grace {
            Grace::Off => match self.reported.asked {
                Some(at) if self.reported.owed => {
                    Some(Forced::From(at + SELF_REPORTED_AGENT_SHELL_RECHECK))
                }
                _ => None,
            },
            Grace::Until(until) => Some(Forced::From(until)),
            Grace::LastTick => Some(Forced::NextTick),
        }
    }

    /// herdr's `should_hold_working_to_idle`: whether this read's `working -> idle` must
    /// wait. `plain` is whether the read qualifies at all — a plain idle after `working`
    /// from the same agent. The first such read starts the hold; each further one is a
    /// confirmation, and the third, or any read [`IDLE_HOLD_CAP`] after the first,
    /// releases it. A read that does not qualify ends the hold.
    fn hold_working_to_idle(&mut self, plain: bool, now: Instant) -> bool {
        if !plain {
            self.hold = None;
            return false;
        }
        let Some(hold) = &mut self.hold else {
            self.hold = Some(Hold {
                started: now,
                confirmations: 0,
            });
            return true;
        };
        if now.saturating_duration_since(hold.started) >= IDLE_HOLD_CAP {
            self.hold = None;
            return false;
        }
        hold.confirmations = hold.confirmations.saturating_add(1);
        if hold.confirmations >= IDLE_HOLD_CONFIRMATIONS {
            self.hold = None;
            return false;
        }
        true
    }
}

/// One observation of a pane: what tmux says about it right now (`command`, `title`,
/// focus), the `@agent_*` options already on it (`prev`), and the two detection inputs
/// the engine needs (`screen` from `capture-pane`, `osc_progress` from the byte stream).
#[derive(Debug, Clone)]
pub struct Observed {
    pub command: String,
    pub title: String,
    pub window_active: bool,
    pub session_attached: u32,
    /// Whether the terminals attached to the pane's session have focus, when tmux can say
    /// (`notify::terminal_focus`): herdr's `outer_terminal_focus`.
    pub terminal_focus: Option<bool>,
    pub kind_hint: Option<String>,
    /// The agent found in the arguments of the container runtime in front of the pane, when
    /// `command` is one (`watch::container` reads them).
    pub container: Option<detect::ContainerAgent>,
    /// The agent the job in front of the pane names, when `command` is a shell or a runtime that
    /// runs a script (`node /path/to/bin/codex`, `sh /path/to/codex`): `watch::job` reads it.
    pub job_agent: Option<String>,
    pub prev: AgentRecord,
    pub screen: String,
    pub osc_progress: String,
    /// Whether the pane's own shell, the process tmux started in it, is idle: at its prompt with
    /// nothing running in it (herdr's `pane_shell_is_idle`). A script that runs the agent is not
    /// it, though tmux names the script's interpreter as the pane's command all the same, and
    /// nor is a shell started inside the pane. Asked only of an agent that reports for itself
    /// and that no process names ([`Arbiter::wants_shell_probe`]); `None` when it was not asked,
    /// or could not be told, and the record is then kept.
    pub pane_shell_idle: Option<bool>,
    /// `@agent_turn`: the stamp of the agent's last report of work or a wait, when it has made one
    /// (`store::TURN_OPTION`).
    pub turn: Option<String>,
    /// `@agent_acquiring`: the pane records its agent as being taken (`store::ACQUIRING_OPTION`),
    /// which `taw agent start`, a session restore or an earlier read marked, and since when (step 11
    /// of [`decide`]); `None` when it does not.
    pub acquiring: Option<Acquired>,
    /// Whether this read has no screen to decide on: the pane records the line `taw agent start` typed
    /// the agent's command on, and the read did not find it, which one read can do while the shell has
    /// still to echo the command or is redrawing it (`typed_line::Misses`). What the pane says is kept
    /// until the next read looks again.
    pub screen_unread: bool,
    /// Whether `screen` is only the rows below the line `taw agent start` typed the agent's command on,
    /// that line being on the screen still (`typed_line`): what the new agent has drawn so far, which is
    /// nothing while those rows are blank.
    pub below_typed_line: bool,
    /// Whether the process of the agent the pane records is alive behind something else, stopped
    /// with Ctrl+Z or run on in the background: herdr v0.9.4's suspend. The pane keeps its agent,
    /// at `idle` with the rule [`store::SUSPENDED_RULE`], until the process is in front again or has
    /// gone. The watcher asks the process table only for an agent whose process it recorded.
    pub suspended: bool,
}

impl Observed {
    /// The agent the pane's own process names: its command, or, when that is a container
    /// runtime, the agent its arguments name, or, when it is a shell or a runtime, the agent the
    /// job in front of the pane is. The hint never counts here.
    pub fn process_identity(&self) -> Option<String> {
        detect::identify(&self.command, None)
            .or_else(|| self.container.as_ref().map(|found| found.kind.clone()))
            .or_else(|| self.job_agent.clone())
    }

    /// The agent the pane holds: the one its kind hint names, else the one its process does.
    pub fn identity(&self) -> Option<String> {
        self.kind_hint
            .as_deref()
            .and_then(|hint| detect::identify("", Some(hint)))
            .or_else(|| self.process_identity())
    }

    /// Whether a user could be looking at this pane right now: its window is the active
    /// window of a session with at least one client attached, and that client's terminal
    /// has not reported losing focus (`notify::in_view`). The inverse of
    /// `notify::is_background`, and the gate for `working -> done` promotion.
    pub fn foreground(&self) -> bool {
        notify::in_view(
            self.window_active,
            self.session_attached,
            self.terminal_focus,
        )
    }
}

/// What the watcher should do about an [`Observed`] pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No agent occupies the pane. `clear` is true when one used to, so the `@agent_*`
    /// options are stale and must go.
    NoAgent { clear: bool },
    /// Nothing to write: the state is unchanged, or a `skip_state_update` rule matched.
    Keep,
    /// An integration hook owns this pane's state (`@agent_source` is `hook:<id>`), so
    /// the manifest must not write state — only `@agent_kind`, and only when identity is
    /// known. `kind` is `None` when the pane's command names no agent we recognise, which
    /// is common for hook-reported panes: the hook knows what it is, we may not. It owns
    /// the pane until `pane release-agent`, or until its shell is back, which ends the record
    /// as an exit does (`shell_returned`).
    HookAuthoritative { kind: Option<String> },
    /// A different agent occupies the pane, in the same state the last one was in. Only
    /// `@agent_kind` is stale — this is not a status change, and must not be reported as
    /// one (no notification, no `previous_status == agent_status` event).
    KindChanged { kind: String },
    /// The state is unchanged but the *evidence* for it moved: a different manifest rule
    /// matches now, or none does where one did. Only `@agent_rule` is stale.
    ///
    /// `@agent_rule` is what `agent get`, the popup and `watch status` show as the reason
    /// a pane is in the state it is in, and it is not written on a transition only. An
    /// agent identified before it had drawn anything resolved to the engine's default
    /// `idle` with *no* matched rule, and every later look at the now-drawn screen found
    /// the same state — so the empty rule was never refreshed and the pane reported `idle`
    /// with no explanation for as long as it stayed idle, which for an agent waiting at
    /// its prompt is most of a session.
    ///
    /// Like `KindChanged`, this is not a status change: no notification, no event, and
    /// `@agent_state_since` keeps saying when the *state* last moved.
    RuleChanged { rule: Option<String> },
    /// The agent the pane records is suspended ([`Observed::suspended`]): written `idle` with the
    /// rule [`store::SUSPENDED_RULE`], its kind, name and session kept, and announced with no
    /// notification. A pane its hook reports for keeps the hook as its source (`hook`).
    Suspended {
        kind: String,
        from: Option<AgentState>,
        hook: bool,
        detail: String,
    },
    /// A state transition to write.
    Transition {
        kind: String,
        from: Option<AgentState>,
        to: AgentState,
        rule: Option<String>,
        detail: String,
        /// Whether the pane was in the foreground at the moment of the transition, which
        /// [`seen_written`] turns into `@agent_seen`, or is marked seen by it, as a `done`
        /// that a new session releases is (step 12 of [`decide`]).
        seen: bool,
        /// The `idle` is the agent becoming ready once taw has taken it, and not a turn ending
        /// (step 11 of [`decide`]): no completion, so no done notification, whatever it follows.
        ready: bool,
    },
    /// An agent that reports its own state reported another since the last read: a change of state
    /// like any other, announced by the same steps (the status event and the `agent.*` event, the
    /// notification its class earns, the rail, the autosave), once.
    ///
    /// The state is on the pane already, written by the report with the hook as its source, so the
    /// watcher writes no state of its own over it, except where the completion rule has another
    /// word for it: an `idle` that ended a turn behind the user's back is `done` (`to`), written
    /// over the report (`reported`) with the hook still the source. When `from` is `to` there is
    /// no change to announce, and what is written is only what the watcher wrote before: a report
    /// of `idle` that repeats itself over an unseen `done` leaves it `done`.
    Reported {
        kind: String,
        from: Option<AgentState>,
        /// A state the agent went through between the last read and this one, announced between
        /// `from` and `to`: the turn that began and ended inside one tick, which the state alone
        /// does not show (`working` and `idle` reported back to back leave the pane as it was).
        via: Option<AgentState>,
        to: AgentState,
        /// The state the report left on the pane.
        reported: AgentState,
        /// The agent the pane had before this one, when another took it: its name goes with it,
        /// and when the state did not change too, the status event says so (`kind_changed`), as it
        /// does for an agent a screen read finds in the pane's place.
        replaced: Option<String>,
        detail: String,
        /// Whether the pane was in the foreground when the change was read, or is marked seen by
        /// the change, as for `Transition`.
        seen: bool,
        /// The reported `idle` is the agent becoming ready, as for `Transition`.
        ready: bool,
    },
}

/// Reads one observation and says what it means.
///
/// The order is herdr's, and it matters:
///
/// 1. **Identity** comes from the process name and `@agent_kind_hint` only, never from
///    the screen: a transcript must not be able to impersonate an agent. When the process
///    is a container runtime, it is the agent its arguments name (`Observed::container`).
/// 2. **Hooks win**, and win *first*. An `@agent_source` of `hook:<id>` means an agent is
///    reporting its own state; the manifest stays out of it until `pane release-agent`,
///    and so does the "is there even an agent here?" question — a hook-reported pane
///    whose process name we do not recognise must keep its state, not lose it. Its
///    shell coming back is what ends it (herdr's `ReportedAgentShellReturned`): the same
///    exit as any agent's, step 8. Any shell in front of it is that for an agent a process
///    has named, and only the pane's own shell at its prompt for one none has (see step 9).
/// 3. **Manifest rules** decide the state, with `skip_state_update` keeping the previous
///    one (overlays such as Claude's transcript viewer hide the real state rather than
///    replacing it).
/// 4. **`done` is herdr's unseen idle, written as a state of its own:** a completion —
///    `idle` after `working` or `blocked` — while nobody is looking at the pane is
///    `done`, whatever a glance earlier in the turn left in `@agent_seen` (herdr's
///    completion rule), and stays `done` until `taw seen` marks the pane looked at.
///    Everything downstream — the rail, the notification classes, the popup — reads
///    that distinction.
/// 5. **A hint with nothing behind it is `unknown`** (but never mid-turn). `@agent_kind_hint` outlives any one
///    occupant by design (herdr never clears it, and neither does `store::clear_agent`),
///    so a pane whose agent has exited still resolves to an identity — and the `NoAgent`
///    clear at step 1 is unreachable for it. Left alone, such a pane kept reporting
///    whatever state its agent was in when it left, for ever. When the identity comes
///    *only* from the hint, the foreground command is not an agent executable, and no
///    manifest rule matches the screen, the honest answer is `unknown` with no rule: we
///    are told a claude belongs here and can see nothing that says what it is doing.
///    Not while the previous state is `working`, though — a mid-redraw capture matches
///    nothing too, and one observation cannot tell it from an agent that has exited.
/// 6. **A plain idle after `working` is held** (herdr's pending idle confirmation): a
///    frame with neither `visible_idle` nor `visible_blocker` is written only once three
///    more reads agree, or 700 ms after the first. `arb` carries the hold between reads;
///    `now` is when this read happened. Every other change is written on the read that
///    sees it.
/// 7. **A new agent reads `unknown` for 3 s and one more tick** (herdr's startup grace):
///    one this watcher has not identified here before, while tmux holds no state for it —
///    none at all, or another agent's. A watcher restarting on a pane it already
///    classified gets no grace.
/// 8. **An agent that exits mid-turn writes its `idle` before its record is cleared**
///    (herdr's process-exit report): see `shell_returned`.
/// 9. **Another process in front of the agent is a miss, not an exit** (herdr's miss
///    confirmations): the agent is kept, and judged by its own rules, through five
///    misses (one on each change of foreground command, one per identity recheck while
///    it stays) and cleared on the sixth. Not for an agent that reports for itself and that
///    no process has ever named while its record stands (its hook reported its kind,
///    `@agent_reported_kind`, or its state): herdr keeps an agent it cannot identify by its
///    process until the pane's shell is idle, so no miss is counted and it stays, its session
///    id with it, until the pane's own shell is at its prompt with nothing running in it.
/// 10. **What an agent leaves on the pane is not its successor's** (herdr's OSC evidence
///     drop): once the pane changes away from an identified agent, to another or to
///     none, the title it left reads as no title until tmux reports another, and its
///     `9;4` progress is forgotten ([`Arbiter::take_progress_reset`]). A first
///     acquisition keeps what its own process printed, but not one `taw agent start` or a
///     restore started: the first read that finds the record empty and the pane marked as being
///     taken (`@agent_acquiring`) sets aside what is on the pane, once per start, whatever the
///     watcher knew before (a watcher started again since knows of no agent that left). So is
///     the title the last agent of the same kind left, which the next one may set again (Codex:
///     the folder's name): a kind whose idle rules read only the title reads idle by the
///     fallback once it has drawn, until tmux reports a title of its own.
/// 11. **An agent taw has just taken finishes nothing by becoming ready** (herdr's
///     acquisition guard, #4457, v0.9.2): an agent the watcher takes as a new one (its startup
///     grace starts, step 7), one the pane marks as being taken (`@agent_acquiring`, which
///     `taw agent start` sets for the agent it launches, herdr's managed launch, and a restore
///     for the agent each resume starts), and one whose session another has replaced is being
///     taken until a read finds it working, or finds that it reported work. Its first `idle`
///     before that ends the acquisition and is no completion, whatever it follows (`ready`): a
///     trust question at startup, `blocked` and then `idle`, is an agent becoming ready, not a
///     turn ending, and earns no `done` and no done notification. An exit is a completion all
///     the same (step 8). The pane holds the mark while it lasts, so a watcher started again
///     finds it, with the turn stamp the agent was taken at (`store::Acquired`): a watcher's
///     first read of the pane takes a reported `working` for work when the stamp has moved since,
///     and for work the agent was taken over when it has not.
/// 12. **A new session, or another agent, ends the completion the pane had** (herdr's
///     completion reset, #4457, v0.9.2): a read that finds the agent's session id or path
///     changed since the last read (Claude's `/clear` and `/resume`, Pi's `/new`, a restore's
///     resume), or another agent in the pane, marks the pane seen whatever its state
///     ([`Arbiter::take_reset`]), and finds a `done` the idle it is once looked at, as `taw
///     seen` makes it: written `idle`, and no completion of the new session's. A notification
///     that was waiting for its delay is dropped when it comes due, as the agent or the session
///     it was earned in is not the pane's any more ([`Pending`]).
pub fn decide(
    store: &ManifestStore,
    cache: &mut HashMap<String, LoadedManifest>,
    o: &Observed,
    agent_exited: bool,
    now: Instant,
    arb: &mut Arbiter,
) -> Decision {
    // Every read's command is remembered, whatever the read decides: a change of it is a
    // miss at once (step 9).
    let command_changed = arb.last_command != o.command;
    // Any other title than the one a departed agent left is its successor's own (step 10).
    if arb
        .evidence
        .stale_title
        .as_deref()
        .is_some_and(|stale| stale != o.title)
    {
        arb.evidence.stale_title = None;
    }
    // The agent's session against the one the last read found (step 12). One that another
    // replaced takes the agent again (step 11): what the pane showed of the last session is
    // no work of this one's, and only work this one does ends the acquisition.
    let session = Session::of(&o.prev);
    let change = arb
        .session
        .as_ref()
        .map_or(SessionChange::None, |last| last.change_to(&session));
    // An agent the pane records as being taken is (step 11), since the moment the mark says: `taw
    // agent start` and a restore mark the one they launch, and a watcher started again finds the
    // mark its predecessor left. A session another replaced takes the agent again from now, so that
    // only a report of work that comes after it ends the acquisition (herdr's rebinding).
    if o.acquiring.is_some() && arb.acquiring != o.acquiring {
        arb.acquiring.clone_from(&o.acquiring);
    }
    if change == SessionChange::Replaced {
        arb.acquiring = Some(Acquired::At(o.turn.clone()));
    }
    let before = arb.identity.clone();
    let moved = Moved {
        command: command_changed,
        session: change.changed(),
    };
    let decision = arbitrate(store, cache, o, agent_exited, now, arb, moved);
    if before.is_some() && arb.identity != before {
        arb.drop_evidence(&o.title);
    }
    // What was known of a record goes with it.
    if matches!(decision, Decision::NoAgent { .. }) {
        arb.reported = Reported::default();
    }
    arb.session = (!matches!(decision, Decision::NoAgent { .. })).then_some(session);
    // herdr's completion reset (step 12): the session changed, or another agent took the pane.
    arb.reset = change.changed() || another_agent(&o.prev, &decision);
    // The pane holds whether its agent is being taken, and since when (step 11), so that it outlives
    // this watcher.
    arb.mark = (arb.acquiring != o.acquiring).then(|| match &arb.acquiring {
        Some(at) => MarkWrite::Set(at.clone()),
        None => MarkWrite::Unset,
    });
    // What the pane holds once this read's decision is written. A read of a pane a hook reports for
    // has taken that in itself (`reported_change`).
    match &decision {
        Decision::NoAgent { .. } => arb.taken = Taken::Nothing,
        Decision::Transition { kind, to, .. } => arb.taken = Taken::after(*to, Some(kind), o),
        Decision::KindChanged { kind } => {
            arb.taken = o
                .prev
                .state
                .map_or(Taken::Nothing, |state| Taken::after(state, Some(kind), o));
        }
        Decision::Keep | Decision::RuleChanged { .. } => arb.taken = Taken::as_read(o),
        Decision::Suspended { kind, .. } => {
            arb.taken = Taken::after(AgentState::Idle, Some(kind), o);
        }
        Decision::HookAuthoritative { .. } | Decision::Reported { .. } => {}
    }
    arb.last_command.clone_from(&o.command);
    decision
}

/// What moved since the last read, which [`decide`] tells [`arbitrate`].
#[derive(Clone, Copy, Debug)]
struct Moved {
    /// The foreground command: a miss at once (step 9).
    command: bool,
    /// The agent's session (step 12).
    session: bool,
}

/// [`decide`], less the command bookkeeping around it.
fn arbitrate(
    store: &ManifestStore,
    cache: &mut HashMap<String, LoadedManifest>,
    o: &Observed,
    agent_exited: bool,
    now: Instant,
    arb: &mut Arbiter,
    moved: Moved,
) -> Decision {
    // Hooks are tested *before* identity, and this order is load-bearing: a
    // hook-reported agent is often something we cannot recognise from the outside (a
    // wrapper script, `node`, an editor extension's helper). Asking "is this an agent?"
    // first would answer "no" and clear the state the agent itself reported.
    // The hint wins, which is what `detect::identify` does and what step 1 above says:
    // `taw agent start --kind claude` is a statement about the pane, and it must not be
    // overruled by whatever the foreground process happens to be called.
    let identity = o.identity();
    // Resolved *separately* and used only to ask "did anything but the hint say so?" —
    // the `is_none()` test that step 5 turns into `unknown`. Never as a fallback: making
    // it one inverted the precedence above. What the pane's process names counts, and so
    // does the agent a container runtime's arguments name: both are evidence an agent is
    // there, which the hint alone is not.
    let from_command = o.process_identity();
    arb.note_command(from_command.as_deref(), o.prev.kind.as_deref());
    if hook_owned(o) {
        if o.suspended
            && let Some(kind) = o.prev.kind.clone()
        {
            return suspended(o, arb, kind);
        }
        // An agent that reports for itself is not judged by the process in front of it:
        // whatever it is, it is no evidence about the agent. Its shell coming back is (herdr's
        // `ReportedAgentShellReturned`), and ends the record as an exit does.
        if shell_is_back(o, arb) {
            return match o.prev.kind.as_deref() {
                Some(kind) => shell_returned(o, arb, kind),
                // Only the hook's source is left, and nothing else would ever clear it.
                None => {
                    arb.reset();
                    Decision::NoAgent { clear: true }
                }
            };
        }
        // The hook reports the agent's state itself: nothing is left to confirm, and no
        // grace is owed a read — one left running would force a read every tick. The agent
        // is still the one it was, and whether it is still being taken (step 11) is about it.
        let acquiring = arb.acquiring.take();
        arb.reset();
        arb.acquiring = acquiring;
        arb.identity.clone_from(&identity);
        let decision = reported_change(o, arb, identity, moved.session);
        // Back in front after a suspend: the hook says nothing of that, so the rule goes here.
        if matches!(
            decision,
            Decision::Keep | Decision::HookAuthoritative { .. }
        ) && o.prev.rule.as_deref() == Some(store::SUSPENDED_RULE)
        {
            return Decision::RuleChanged { rule: None };
        }
        return decision;
    }

    // Only reachable with a non-`hook:` source, so no `NoAgent` clear can ever wipe a
    // record whose state a hook reported.
    let kind = match &identity {
        Some(kind) => {
            // An agent in the foreground: whatever exit was pending did not happen here,
            // and it has missed nothing.
            arb.exit_pending = false;
            arb.misses = 0;
            arb.last_miss = None;
            kind.clone()
        }
        None => match agent_gone(o, now, arb, moved.command) {
            Gone::Decided(decision) => return decision,
            Gone::Kept(kind) => kind,
        },
    };
    // Nothing but the hint names the agent (steps 5 and the exited-agent check below). An
    // agent kept through its misses is not that: it was identified, and something else is
    // in front of it now.
    let hint_only = identity.is_some() && from_command.is_none();
    // A hinted agent with no hook, stopped behind the shell: the hint keeps its identity, and the
    // shell's screen says nothing of it.
    if hint_only && o.suspended && o.prev.kind.as_deref() == Some(kind.as_str()) {
        return suspended(o, arb, kind);
    }

    // A new agent (herdr's startup grace): the watcher has not seen this one here before,
    // and tmux holds no state for it either — no state at all, as `taw agent start` leaves
    // a pane, or another agent's. Its first frames say nothing yet, so the pane is
    // `unknown` for 3 s and one more tick. A watcher restarting on a pane it already
    // classified finds `prev` agreeing and gets none.
    //
    // So is the next agent of a hint-started pane that has taken its place. The hint outlives the
    // agent it started, so this arbiter still holds that agent's identity when `taw agent start`
    // has taken the pane's record off it to type the command for the next, and the next is of the
    // same kind as the last when it is started again. An identity held for a record that is not
    // there any more is of an agent that has left, and what is in front of the pane is a new one:
    // until it draws, the screen is the last agent's, and its rules read that as well as any.
    //
    // A next agent that a process already names at the first read after the record went (a binary
    // of the agent's name, or a script the job under the shell names) is the same agent to the
    // arbiter, and has no grace: the verb marks it as being taken (step 11), and the screen it is
    // read on is the rows below the line its command was typed on. But the title and the progress
    // still on the pane are the last agent's all the same, and are not its own (step 10): a `✳`
    // title left there would read as the new agent's idle before it has drawn anything.
    let record_taken_off = arb.identity.as_deref() == Some(kind.as_str())
        && o.prev.kind.is_none()
        && o.prev.state.is_none();
    let replacement = hint_only && record_taken_off;
    // A hint names its kind by any of the kind's names (`agent start --kind kiro-cli` is Kiro).
    let relaunched = record_taken_off
        && o.kind_hint
            .as_deref()
            .and_then(|hint| detect::identify("", Some(hint)))
            .as_deref()
            == Some(kind.as_str());
    let newly_identified = replacement || arb.identity.as_deref() != Some(kind.as_str());
    // A pane whose record is empty and that is marked as being taken: `taw agent start` (or a
    // restore) has just taken the record off and typed the command. Nothing on the pane from before
    // is the new agent's, whatever this arbiter holds: a watcher started again since has a fresh
    // one, which knows of no agent that left, and at a first start what is there is the shell's.
    // Once per start, at the first read that finds it so: a later read of the same start (the record
    // still empty after a read with no screen) keeps the title tmux has reported since.
    let being_started = o.prev.kind.is_none() && o.prev.state.is_none() && o.acquiring.is_some();
    let start_read = being_started && !arb.start_evidence_dropped;
    arb.start_evidence_dropped = being_started;
    if ((newly_identified || relaunched) && arb.identity.is_some()) || start_read {
        // A replacement: even a first read of it that is evaluated at once — `taw agent
        // start` has already recorded it, so there is no grace — must not see what the
        // last agent left (step 10).
        arb.drop_evidence(&o.title);
    }
    // A real start: a relaunch, or an agent this arbiter has not identified here while the pane's
    // record holds nothing of it that it drew (no state, `unknown`, or another agent's). It is
    // starting until a rule of its kind matches (step 5). A watcher started again has a fresh
    // arbiter, so an agent that drew long ago is identified anew, but the record holds what it
    // drew: no window, and a frame no rule knows reads as the fallback, so a `done` stays.
    if relaunched
        || (newly_identified
            && (matches!(o.prev.state, None | Some(AgentState::Unknown))
                || o.prev.kind.as_deref() != Some(kind.as_str())))
    {
        arb.starting = true;
    }
    arb.identity = Some(kind.clone());
    if newly_identified && (o.prev.state.is_none() || o.prev.kind.as_deref() != Some(kind.as_str()))
    {
        arb.hold = None;
        arb.grace = Grace::Until(now + STARTUP_GRACE);
        // A new agent is being taken until it is seen working (step 11), from now.
        arb.acquiring = Some(Acquired::At(o.turn.clone()));
        return if o.prev.state == Some(AgentState::Unknown) {
            Decision::KindChanged { kind }
        } else {
            Decision::Transition {
                kind,
                from: o.prev.state,
                to: AgentState::Unknown,
                rule: None,
                detail: o.title.trim().to_string(),
                seen: o.foreground(),
                ready: false,
            }
        };
    }
    match arb.grace {
        Grace::Until(until) => {
            // Inside the window nothing is written, and the first read at or after its
            // end is kept too: herdr skips that tick as well.
            if now >= until {
                arb.grace = Grace::LastTick;
            }
            arb.hold = None;
            return Decision::Keep;
        }
        Grace::LastTick => arb.grace = Grace::Off,
        Grace::Off => {}
    }

    // The agent this hint named has exited: the caller sets `agent_exited` only once the pid
    // recorded for it is dead. No frame says anything of a process that is not there, whatever the
    // rules of its kind make of the last one it left, and there is no live turn left for the
    // exception below (a capture caught mid-redraw) to protect.
    if agent_exited && hint_only {
        return hinted_agent_exited(o, arb, kind);
    }

    // The agent's session changed while its pane was `done` (step 12): that completion goes, as
    // when the pane is looked at, before anything on screen is read. What the screen says is read
    // at the next read, which a new session's own output brings.
    if moved.session && o.prev.state == Some(AgentState::Done) {
        return released(o, kind);
    }

    // A read with no screen to decide on (the line its command was typed on was not on it, once): what
    // the pane says stays as it is until the next read, which is owed at once, looks again. Read as an
    // empty screen, it would turn a `blocked` that is still on screen into `unknown` and back, with a
    // second request.
    if o.screen_unread {
        return Decision::Keep;
    }

    if !cache.contains_key(&kind) {
        match store.load(&kind) {
            Ok(manifest) => {
                cache.insert(kind.clone(), manifest);
            }
            Err(err) => {
                log::warn!("no usable manifest for agent '{kind}': {err:#}");
                // Nothing can confirm a hold without rules, and a hold left standing
                // would have the pane re-read every 100 ms for as long as this lasts.
                arb.hold = None;
                return Decision::Keep;
            }
        }
    }
    let manifest = &cache[&kind];

    // What a departed agent left on the pane is not this one's (step 10): the title it
    // left reads as none until tmux reports another, and its progress is gone.
    let title = if arb.evidence.stale_title.as_deref() == Some(o.title.as_str()) {
        ""
    } else {
        o.title.as_str()
    };
    let snapshot = Snapshot {
        content: &o.screen,
        osc_title: title,
        osc_progress: if arb.evidence.progress_reset {
            ""
        } else {
            &o.osc_progress
        },
    };
    let evaluation = detect::evaluate(manifest, &snapshot);
    // A rule of the agent's kind matched, an overlay's too: it has drawn, and is not starting
    // any more (step 5).
    let starting = arb.starting && evaluation.matched_rule.is_none();
    arb.starting = starting;
    if evaluation.skip_state_update {
        // An overlay hides the real state rather than replacing it: keep that state, and
        // (herdr, `src/pane.rs:3051-3054`) end any hold, so a plain idle after the overlay
        // needs its confirmations again.
        arb.hold = None;
        return Decision::Keep;
    }

    let rule = evaluation.matched_rule.as_ref().map(|rule| rule.id.clone());
    // Nothing but the hint says an agent is here, and nothing on screen says what it is
    // doing: `unknown`, not the state whoever was here last happened to leave behind. See
    // step 5 above. `promote_done` makes nothing of it — `done` is a claim about a turn
    // that finished, which is exactly what is not known here.
    //
    // Except while the previous state is `working`. A capture that lands mid-redraw
    // matches nothing either, and it is indistinguishable *in one observation* from an
    // agent that has gone — so letting it overwrite a live turn destroyed the
    // `working -> idle` edge that `promote_done` turns into `done` and the whole
    // notification path hangs on: the user was never told the agent had finished. A pane
    // that really has lost its agent still reaches `unknown`, one evaluation later, when
    // the previous state is no longer `working`. (An agent whose process is known to have
    // exited never gets here: [`hinted_agent_exited`] ends its turn at once.)
    //
    // So is an agent read below the line its command was typed on that has drawn nothing there,
    // whatever names it (a process does too once the job in front of the pane is read, a script
    // under `sh`, or a binary of the agent's name): a blank screen is no idle of the new agent's,
    // only a start that has not drawn. And while it is starting, until a rule of its kind has
    // matched, so is what it printed there that no rule knows (an install's progress before the
    // agent starts), for a kind whose rules know its idle screen: it has not drawn the idle they
    // know. A kind with no idle rule that reads the screen (none at all, or only the title's or the
    // progress's) shows its idle screen only as the fallback, a frame no rule knows, so for it that
    // is its idle. Nor once the agent has drawn: a frame no rule knows then (a menu, a redraw caught
    // half-way) reads as the fallback says, as any agent's does, so a `done` stays.
    let below_the_line = o.below_typed_line
        && (o.screen.trim().is_empty() || (starting && manifest.has_screen_idle_rule()));
    let read = if (hint_only || below_the_line)
        && rule.is_none()
        && o.prev.state != Some(AgentState::Working)
    {
        AgentState::Unknown
    } else {
        evaluation.state
    };
    // `working -> idle` on a frame with neither a visible idle nor a visible blocker is
    // held (herdr's `should_hold_working_to_idle`, on the raw verdict before `done`): such a
    // frame is as often a redraw caught half-way as a turn's end. It is written once three
    // more reads agree, or 700 ms after the first; anything else ends the hold. A visible
    // idle — the prompt box at rest, the `✳` title — or a blocker is written at once.
    let plain_idle_after_working = o.prev.state == Some(AgentState::Working)
        && evaluation.state == AgentState::Idle
        && !evaluation.visible_idle
        && !evaluation.visible_blocker
        && o.prev.kind.as_deref() == Some(kind.as_str())
        && !agent_exited;
    if arb.hold_working_to_idle(plain_idle_after_working, now) {
        return Decision::Keep;
    }
    // The agent being taken (step 11): a working read ends that, and the first idle before one
    // is the agent becoming ready, which no completion rule makes `done` of.
    let ready = read == AgentState::Idle && arb.first_idle();
    if read == AgentState::Working {
        arb.worked();
    }
    let to = if ready { read } else { promote_done(read, o) };
    if o.prev.state == Some(to) {
        // The state did not move. If the *occupant* did, that is a kind update and
        // nothing more: reporting it as a transition would re-fire the notification for
        // whatever state the pane is sitting in.
        if o.prev.kind.as_deref() != Some(kind.as_str()) {
            return Decision::KindChanged { kind };
        }
        // The occupant did not move either, but the evidence still might have: the rule
        // that explains this state is only ever written on a transition, so without this
        // a first `idle` recorded before the agent drew anything keeps its empty rule for
        // the whole idle period. A kind change takes the branch above and refreshes the
        // rule on the next look, when `prev.kind` agrees again.
        return if o.prev.rule == rule {
            Decision::Keep
        } else {
            Decision::RuleChanged { rule }
        };
    }

    Decision::Transition {
        kind,
        from: o.prev.state,
        to,
        rule,
        detail: detail(&evaluation, &snapshot, title),
        seen: o.foreground(),
        ready,
    }
}

/// A hint-only pane whose agent's process has exited (the pid recorded for it is dead): what an exit
/// comes to for an agent the pane's command does not name, an agent `taw agent start` started that
/// runs under a script or a wrapper. A turn that was under way ends as the turn of any agent ends
/// when its process does ([`shell_returned`]: an `idle` with the rule `process_exited`, `done`
/// while nobody is looking), and the read after it, like any read of an agent that was at rest,
/// finds nothing behind the hint: `unknown` with no rule and the kind kept, as the pane of an agent
/// that has left reads while its hint stays (step 5 of [`decide`]).
fn hinted_agent_exited(o: &Observed, arb: &mut Arbiter, kind: String) -> Decision {
    if matches!(
        o.prev.state,
        Some(AgentState::Working | AgentState::Blocked)
    ) {
        return shell_returned(o, arb, &kind);
    }
    arb.hold = None;
    // The agent being taken has gone with its process: the next one is taken when it comes.
    arb.acquiring = None;
    if o.prev.state == Some(AgentState::Unknown) {
        if o.prev.kind.as_deref() != Some(kind.as_str()) {
            return Decision::KindChanged { kind };
        }
        return if o.prev.rule.is_none() {
            Decision::Keep
        } else {
            Decision::RuleChanged { rule: None }
        };
    }
    Decision::Transition {
        kind,
        from: o.prev.state,
        to: AgentState::Unknown,
        rule: None,
        detail: o.title.trim().to_string(),
        seen: o.foreground(),
        ready: false,
    }
}

/// herdr's completion reset (step 12 of [`decide`]) for a pane whose state the screen decides: the
/// `done` of an agent whose session changed goes back to the `idle` it is once the pane is looked
/// at, as `taw seen` makes it. The rule that explained it stands, the pane is marked seen, and the
/// change is announced as the status it is (`done` to `idle`), with no notification: an idle is no
/// news. A notice the `done` earned is dropped when its delay is up ([`Pending`]).
fn released(o: &Observed, kind: String) -> Decision {
    Decision::Transition {
        kind,
        from: Some(AgentState::Done),
        to: AgentState::Idle,
        rule: o.prev.rule.clone(),
        detail: o.title.trim().to_string(),
        seen: true,
        ready: false,
    }
}

/// Whether `decision` has another agent in the pane than the one its record `prev` held: herdr's
/// change of agent label, which resets the pane's completion as a new session does (step 12 of
/// [`decide`]). A pane that held no agent has no completion to reset.
fn another_agent(prev: &AgentRecord, decision: &Decision) -> bool {
    let kind = match decision {
        Decision::Reported { replaced, .. } => return replaced.is_some(),
        Decision::Transition { kind, .. }
        | Decision::KindChanged { kind }
        | Decision::HookAuthoritative { kind: Some(kind) } => kind,
        _ => return false,
    };
    prev.kind.as_deref().is_some_and(|last| last != kind)
}

/// What a read whose command and hint name no agent comes to.
enum Gone {
    /// The record is cleared, or an exit's `idle` is written first.
    Decided(Decision),
    /// Another process is in front of the recorded agent and has not yet been missed six
    /// times, or the agent reports for itself and no process has named it: still that agent,
    /// and its rules still read the screen.
    Kept(String),
}

/// No agent names itself in the pane any more: neither its command nor a hint does.
///
/// * **The shell is back**: herdr's `ReportProcessExit`, `shell_returned`, which a
///   hook-reported pane's shell returning takes too. Any shell in front of the agent is that,
///   the pane's own or one started inside it (`nix develop`, a `bash` typed at the prompt),
///   except for an agent that reports for itself and that no process has named, below.
/// * **Another process is in front** (`vim` from Claude's `ctrl+g`, say) — herdr's
///   `AGENT_MISS_CONFIRMATION_ATTEMPTS`. That is a miss, not an exit: one on each change
///   of command and one per identity recheck (5 s by default) while it stays, and the
///   agent is kept until the sixth, which clears it. A reported exit is cleared whatever
///   is in front.
/// * **The agent reports for itself and no process has named it while its record stands**: its
///   own integration hook reported this kind (`@agent_reported_kind`, written by `pane
///   report-agent-session`), or its state (`@agent_source`, in [`arbitrate`]). herdr keeps such
///   an agent, one it cannot identify by its process, until the pane's shell is idle
///   (`report_self_reported_agent_shell_return`, with `available_pane_shell_from_job`, `src/pane.rs`
///   and `src/platform/mod.rs`, master `0d5d6f1`), so no miss is counted, and neither does a shell
///   inside the pane end it: only the pane's own, at its prompt with nothing running in it
///   ([`Observed::pane_shell_idle`]). The pane keeps its kind and its session id until then,
///   whatever its command is called. That is the Letta an npm install runs under `node`, an
///   agent run by a wrapper script, and any agent that no name in the identity table matches.
///   Which agents herdr treats so is not in what taw is ported from, so this is taw's own
///   reading: every agent whose hook reports for it and that the identity table does not name.
/// * **No agent was recorded**: nothing to keep, and only a stray state to clear.
fn agent_gone(o: &Observed, now: Instant, arb: &mut Arbiter, command_changed: bool) -> Gone {
    let Some(kind) = o.prev.kind.clone() else {
        arb.reset();
        return Gone::Decided(Decision::NoAgent {
            clear: o.prev.state.is_some(),
        });
    };
    if o.suspended {
        return Gone::Decided(suspended(o, arb, kind));
    }
    if shell_is_back(o, arb) {
        return Gone::Decided(shell_returned(o, arb, &kind));
    }
    if arb.exit_pending {
        // herdr's `ClearAgent`: the agent is gone, and a process started after it is not
        // a miss of it.
        arb.reset();
        return Gone::Decided(Decision::NoAgent { clear: true });
    }
    if self_reported(o, arb) {
        // It said what it is, and nothing has recorded another occupant since: it is here
        // until its shell is back (above), and a command that names no agent is not evidence
        // against it. It has missed nothing, so the count starts from nothing if the record
        // is ever judged by it.
        arb.misses = 0;
        arb.last_miss = None;
        arb.identity = Some(kind.clone());
        return Gone::Kept(kind);
    }
    let recheck_due = arb
        .last_miss
        .is_none_or(|at| now.saturating_duration_since(at) >= arb.identity_recheck);
    if command_changed || recheck_due {
        arb.misses = arb.misses.saturating_add(1);
        arb.last_miss = Some(now);
    }
    if arb.misses >= IDENTITY_MISSES {
        arb.reset();
        return Gone::Decided(Decision::NoAgent { clear: true });
    }
    // herdr keeps its current agent through the misses, and so does the watcher: the
    // agent coming back is not a new one, and gets no grace.
    arb.identity = Some(kind.clone());
    Gone::Kept(kind)
}

/// The agent the pane records is suspended ([`Observed::suspended`]): herdr v0.9.4 keeps its name,
/// its session and its hook authority while the exact process stays alive behind the shell. It is
/// the same agent, so nothing it was is reset: not its identity (no startup grace when it is in
/// front again), nor whether it is still being taken. It misses nothing while it is stopped. Its
/// state is `idle`, with [`store::SUSPENDED_RULE`] for the reason, which a turn under way ends in
/// with no notification: nothing finished.
fn suspended(o: &Observed, arb: &mut Arbiter, kind: String) -> Decision {
    arb.hold = None;
    arb.exit_pending = false;
    arb.misses = 0;
    arb.last_miss = None;
    arb.identity = Some(kind.clone());
    if o.prev.state == Some(AgentState::Idle)
        && o.prev.rule.as_deref() == Some(store::SUSPENDED_RULE)
        && o.prev.kind.as_deref() == Some(kind.as_str())
    {
        return Decision::Keep;
    }
    Decision::Suspended {
        kind,
        from: o.prev.state,
        hook: hook_owned(o),
        detail: o.title.trim().to_string(),
    }
}

/// The pane's shell is back in front of the agent the pane records, so the agent has exited:
/// herdr's `ReportProcessExit`, and for an agent that reports for itself its
/// `ReportedAgentShellReturned`. One rule for both, so that whatever ends an agent's record
/// ends it the same way.
///
/// If the agent was `working` or `blocked`, the turn's end is written first: an `idle` with
/// rule `process_exited`, `done` in the background as any finished turn is, "so notifications
/// and wait-agent callers observe completion before the pane becomes unknown" (herdr
/// `src/pane.rs:470-474`). Written by the watcher, that state names `manifest` as its source,
/// which is what hands a pane back from its hook. The next read, owed whether or not anything
/// printed, finds tmux holding that `idle` and clears the record, as it does at once from any
/// other state. A write that failed leaves tmux at `working`, so the retried read writes the
/// exit again.
fn shell_returned(o: &Observed, arb: &mut Arbiter, kind: &str) -> Decision {
    // The turn that was under way is the one the watcher last announced, when the agent reported
    // its own state: its last report, `idle` from an `opencode run` that ended with its process, may
    // be what this read finds first, and the turn it ended was announced as started and never as
    // ended. That end is the exit's, written and announced here as any exit's is, and once.
    let announced = match &arb.taken {
        Taken::State { state, .. } => Some(*state),
        Taken::Unread | Taken::Nothing => None,
    };
    arb.reset();
    let under_way = |state: Option<AgentState>| {
        matches!(state, Some(AgentState::Working | AgentState::Blocked))
    };
    let from = if under_way(o.prev.state) || !under_way(announced) {
        o.prev.state
    } else {
        announced
    };
    if under_way(from) {
        arb.identity = Some(kind.to_string());
        arb.exit_pending = true;
        return Decision::Transition {
            kind: kind.to_string(),
            from,
            to: completion(AgentState::Idle, from, o.prev.seen, o.foreground()),
            rule: Some(PROCESS_EXITED_RULE.to_string()),
            detail: o.title.trim().to_string(),
            seen: o.foreground(),
            // An exit is a completion even of an agent still being taken (herdr's
            // `finish_agent_process_acquisition` lets one through after a process exit).
            ready: false,
        };
    }
    Decision::NoAgent { clear: true }
}

/// What a read of a pane whose agent reports its own state comes to: nothing new, or the change it
/// reported since the last read, which is announced as any change of state is.
///
/// The state is on the pane already, written by the report, so a change is what the read finds
/// other than what the last read took in ([`Taken`]). A change read off the screen goes through the
/// completion rule, and so does this one: an `idle` after `working` or `blocked` is `done` while
/// nobody is looking, decided now, and is written over the report with the hook still its source.
///
/// Three things are not changes. The same state again: OpenCode reports `working` for every tool it
/// runs and `idle` from two of its events. A `done` turned back into `idle` by `taw seen` (the
/// mark is `@agent_seen`), which nobody reported. And an `idle` reported over a `done` nobody has
/// seen, which leaves it `done`, as herdr's unseen idle stays unseen: what is written back over it
/// is the watcher's own, not announced again.
///
/// Except where the session changed since the last read (`session_changed`), or another agent took
/// the pane: the `done` the pane had is then the idle it is once looked at (step 12 of [`decide`]),
/// marked seen, and what the agent says now follows that idle. And an agent being taken (step 11)
/// ends that when it reports work, a report of working or a turn between two reads, and its first
/// `idle` before that is no completion.
fn reported_change(
    o: &Observed,
    arb: &mut Arbiter,
    identity: Option<String>,
    session_changed: bool,
) -> Decision {
    let nothing = Decision::HookAuthoritative {
        kind: identity.clone(),
    };
    let Some(record) = o.prev.state else {
        // A source and no state: nothing to take in, and what is reported next is news.
        arb.taken = Taken::Nothing;
        return nothing;
    };
    // Who the change is announced for: the process in front of the pane when it names an agent,
    // as the kind is recorded, else the agent that reported.
    let kind = identity.or_else(|| o.prev.kind.clone());
    let taken = std::mem::replace(&mut arb.taken, Taken::after(record, kind.as_deref(), o));
    let Some(kind) = kind else {
        return nothing;
    };
    let (last, was, stamp) = match taken {
        Taken::Unread => {
            // This watcher's first read of the pane (one started again, or attached while the agent
            // works): no read of its own came before to tell a report from, so the mark of an agent
            // being taken tells it instead (step 11). A `working` reported over a turn stamp that has
            // moved since the agent was taken is work, which ends the acquisition, as herdr's report
            // of working does; over the stamp it was taken at, it is work it was taken over (herdr's
            // rebinding), and a mark that does not say when keeps it as well.
            if record == AgentState::Working && arb.reported_since_taken(o.turn.as_deref()) {
                arb.worked();
            }
            return nothing;
        }
        Taken::Nothing => (None, None, None),
        Taken::State { state, kind, turn } => (Some(state), kind, turn),
    };
    // Another agent has taken the pane: the status event says so even when the state is the one
    // the pane was in, and the name the last agent was given goes with it.
    let replaced = was.filter(|was| *was != kind);
    // `done` is the watcher's own word for an `idle` nobody saw: what the agent says is `idle`.
    let said = if record == AgentState::Done {
        AgentState::Idle
    } else {
        record
    };
    // A `done` that `taw seen` turned back into `idle` since the last read: nobody reported that,
    // and the state the pane was in is the `idle` it is now.
    let from = if last == Some(AgentState::Done) && record == AgentState::Idle && o.prev.seen {
        Some(AgentState::Idle)
    } else {
        last
    };
    // A new session, or another agent (step 12): the `done` the pane had is the idle it is once
    // looked at, and that is what the agent's word follows.
    let released = (session_changed || replaced.is_some()) && from == Some(AgentState::Done);
    let basis = if released {
        Some(AgentState::Idle)
    } else {
        from
    };
    let to = completion(said, basis, o.prev.seen, o.foreground());
    // A turn that began and ended since the last read: the agent reported work, which moved the
    // stamp, and what it says now is the state the pane was in, so there is no change in the state
    // for this read to see. Not while a turn is under way, which reports work all the way through;
    // and not with nothing taken in, which is a change from nothing whatever the stamp says (there
    // is no state then that this one could equal).
    let under_way = matches!(basis, Some(AgentState::Working | AgentState::Blocked));
    let worked = o.turn.is_some() && o.turn != stamp;
    let via = (worked && basis == Some(to) && !under_way).then_some(AgentState::Working);
    // Work reported since the last read ends an acquisition (step 11): that turn, or a report of
    // working this read takes in. A `working` the pane held before this read is not news of work.
    if via.is_some() || (said == AgentState::Working && (basis != Some(said) || worked)) {
        arb.worked();
    }
    // Its end is a completion like any other: behind the user's back it is `done`, decided now. The
    // first `idle` of an agent being taken is not: the agent became ready.
    let ready = via.is_none() && said == AgentState::Idle && arb.first_idle();
    let to = match via {
        Some(work) => completion(said, Some(work), o.prev.seen, o.foreground()),
        None if ready => said,
        None => to,
    };
    arb.taken = Taken::after(to, Some(&kind), o);
    if from == Some(to) && via.is_none() && to == record && replaced.is_none() {
        return nothing;
    }
    Decision::Reported {
        kind,
        from,
        via,
        to,
        reported: record,
        replaced,
        detail: o.title.trim().to_string(),
        // A released `done` that is still at rest is marked seen, as looking at it would have.
        seen: o.foreground() || (released && to == AgentState::Idle),
        ready,
    }
}

/// Whether the shell is back in front of the agent the pane records: for an agent that reports
/// for itself and that no process has named ([`self_reported`]), the pane's own shell is idle
/// ([`Observed::pane_shell_idle`], what herdr's `pane_shell_is_idle` says), and only that. A
/// script that runs the agent reads as a shell to tmux for as long as it runs, and so does a
/// shell started inside the pane: neither is the shell coming back. For any other agent the
/// command in front is one of [`SHELLS`], and the job it leads names no agent: a wrapper
/// script that does not replace itself (`bash ~/bin/claude-wrap`) keeps the pane's command
/// `bash` for as long as the agent it runs is there ([`Observed::job_agent`]).
fn shell_is_back(o: &Observed, arb: &Arbiter) -> bool {
    if self_reported(o, arb) {
        o.pane_shell_idle == Some(true)
    } else {
        is_shell(&o.command) && o.job_agent.is_none()
    }
}

/// The agent in the pane says for itself that it is there, either way an integration can:
/// a hook owns the pane's state, or its hook named the agent (`@agent_reported_kind`) and
/// nothing has recorded another occupant since.
fn reports_for_itself(o: &Observed) -> bool {
    hook_owned(o) || (o.prev.kind.is_some() && o.prev.reported_kind == o.prev.kind)
}

/// Whether the process in front of the pane has named the agent the pane records, at this read
/// or at an earlier one while that record has stood: an agent taw can identify by its process.
/// A container runtime names the agent its arguments do ([`Observed::process_identity`]), so an
/// agent in a container is one too, and follows the ordinary exit and miss rules.
fn identified(o: &Observed, arb: &Arbiter) -> bool {
    let Some(kind) = o.prev.kind.as_deref() else {
        return false;
    };
    arb.reported.named.as_deref() == Some(kind) || o.process_identity().as_deref() == Some(kind)
}

/// An agent that reports for itself and that taw cannot identify by its process: herdr's
/// self-reported agent, which it keeps until the pane's shell is idle. The rest, named by their
/// process, follow the ordinary exit and miss rules whatever their hook reported.
fn self_reported(o: &Observed, arb: &Arbiter) -> bool {
    reports_for_itself(o) && !identified(o, arb)
}

/// Whether `command` is one of [`SHELLS`], as herdr matches a shell's name
/// (`is_pane_shell_process_name`): the last component of a path, without a login shell's
/// leading `-` or a trailing `.exe`, lower-cased.
pub(super) fn is_shell(command: &str) -> bool {
    let name = command.trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name = name.trim_start_matches('-').trim_end_matches(".exe");
    SHELLS.contains(&name.to_ascii_lowercase().as_str())
}

/// Whether [`decide`] will look at `o.screen`, so the watcher knows whether this read
/// needs a `capture-pane` at all: only when an agent is identified, no hook owns the
/// state, and no startup grace is running — inside the window nothing on screen is
/// believed (herdr skips the screen read there too), and the display alone still sees
/// the agent exit or give way to another. Every other answer comes from the display line
/// alone.
pub fn wants_screen(o: &Observed, arb: &Arbiter) -> bool {
    if hook_owned(o) || arb.in_startup_grace() || reads_as_suspended(o) {
        return false;
    }
    // An agent names itself, or another process is in front of one the pane records and
    // the agent's rules still read the screen (step 9).
    o.identity().is_some() || (o.prev.kind.is_some() && !is_shell(&o.command))
}

/// Whether [`decide`] keeps the pane's agent as suspended without a look at the screen: its process
/// is stopped behind the pane (`o.suspended`), and nothing but the hint, if anything, names an agent
/// in front of it.
fn reads_as_suspended(o: &Observed) -> bool {
    o.suspended
        && o.prev.kind.is_some()
        && o.process_identity().is_none()
        && o.identity()
            .is_none_or(|kind| o.prev.kind.as_deref() == Some(kind.as_str()))
}

/// herdr's name owner (`reconcile_agent_name_owner`, `src/terminal/state.rs`): whether
/// `@agent_name`, given to the `prev_kind` agent, goes when the watcher records `kind`
/// over it without a clear in between.
///
/// A name belongs to the agent it was given to, so a different kind takes it away —
/// except the kind the pane was set up for, its `@agent_kind_hint` (herdr's managed
/// agent). `taw agent start` records that hint before it launches the agent and writes
/// the agent's own name once it runs, so a read that lands between the two leaves the
/// name to it.
pub fn name_leaves_with(prev_kind: Option<&str>, kind: &str, kind_hint: Option<&str>) -> bool {
    let managed = kind_hint.and_then(|hint| detect::identify("", Some(hint)));
    prev_kind.is_some_and(|prev| prev != kind) && managed.as_deref() != Some(kind)
}

/// An integration hook reported this pane's state (`@agent_source` is `hook:<id>`), and
/// owns it until `pane release-agent`.
pub(crate) fn hook_owned(o: &Observed) -> bool {
    o.prev
        .source
        .as_deref()
        .is_some_and(|source| source.starts_with("hook:"))
}

/// A hook-owned record ends when the shell is back, which the job probe's answer decides: a prompt
/// it saw before is asked about again at the next read, since a script under the shell's own name may
/// lead now. A pane with no hook-owned record never asks again, so a shell at its prompt costs no
/// `ps` per read.
pub(crate) fn forget_prompt_of_hook_owned(job: &mut super::job::Probe, o: &Observed) {
    if hook_owned(o) {
        job.forget_prompt();
    }
}

/// herdr's completion rule (`src/app/actions.rs`, master `0d5d6f1`): `idle` after
/// `working` or `blocked` is the agent's work ending, and whether the user saw it end is
/// decided then, by where they are looking at that moment. Behind their back it is
/// `done`, which stays until the pane has been seen (`taw seen`, via tmux's focus hooks
/// — see `watch::mark_seen`) or a read finds the user looking at it; a glance at the pane
/// earlier in the turn does not count. The read matters when a finish lands as the user
/// switches to the pane: `taw seen` then ran while it was still `working`, and the
/// `done` write undid its mark. `idle` after anything else — `unknown`, an agent
/// becoming readable rather than finishing (herdr #4457) — and every other evaluated
/// state pass through untouched.
fn promote_done(evaluated: AgentState, o: &Observed) -> AgentState {
    completion(evaluated, o.prev.state, o.prev.seen, o.foreground())
}

/// [`promote_done`] for a state that follows `from` (the one the pane was in before it), where
/// `seen` is `@agent_seen` and `foreground` whether the user is looking at the pane now. The state
/// tmux holds is not always `from`: an agent that reports its own state has written the new one
/// itself by the time the watcher reads it, and `from` is then the last state the watcher took in.
fn completion(
    evaluated: AgentState,
    from: Option<AgentState>,
    seen: bool,
    foreground: bool,
) -> AgentState {
    if evaluated != AgentState::Idle {
        return evaluated;
    }
    match from {
        Some(AgentState::Working | AgentState::Blocked) if !foreground => AgentState::Done,
        Some(AgentState::Done) if !seen && !foreground => AgentState::Done,
        _ => evaluated,
    }
}

/// What a transition to `to` writes to `@agent_seen`, if anything, given whether the user
/// is looking at the pane as it happens (`foreground`, [`Observed::foreground`]).
///
/// A new turn starts unseen, whoever is watching: the flag is about how the turn ends.
/// A completion decides it (herdr sets `seen` at the completion): the `idle` the user is
/// looking at is seen, and a `done` is not — whatever a glance earlier in the turn left.
/// Left as it was, that glance would read back as "seen" on the next read and turn the
/// `done` straight back into `idle`.
pub fn seen_written(to: AgentState, foreground: bool) -> Option<bool> {
    match to {
        AgentState::Working | AgentState::Done => Some(false),
        AgentState::Idle if foreground => Some(true),
        _ => None,
    }
}

/// What a [`Decision`] writes to the pane's `@agent_*` options, and the notification it
/// earns, given the record tmux holds before it. The one mapping from a decision to its
/// effect on the record: the watcher makes these writes to tmux, and the fake-clock
/// harness makes them to its own record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// Every option an occupant owns is dropped (`store::clear_agent`).
    pub clear: bool,
    /// `@agent_state` and the rule written with it (`store::write_state`).
    pub state: Option<(AgentState, Option<String>)>,
    /// A state a hook reported, and what the completion rule makes of it: `(reported, written)`,
    /// written over it with the hook still the source, and only while the pane still holds what
    /// was reported (`store::write_state_over`), for a newer report must not be undone.
    pub complete: Option<(AgentState, AgentState)>,
    /// `@agent_rule` alone: the state stands, the evidence for it moved.
    pub rule: Option<Option<String>>,
    /// `@agent_kind`, when the pane records a different agent now.
    pub kind: Option<String>,
    /// `@agent_name` goes: it was the last agent's (see [`name_leaves_with`]).
    pub release_name: bool,
    /// What the last agent's own hook reported goes: its session id and path, and its word
    /// that it was there (`@agent_reported_kind`). They were about that agent, and another has
    /// taken the pane.
    pub release_identity: bool,
    /// `@agent_seen` (see [`seen_written`]).
    pub seen: Option<bool>,
    /// `@agent_acquiring` set or taken off: whether the agent is being taken, or since when, changed
    /// at this read (step 11 of [`decide`], [`Update::finish`]).
    pub acquiring: Option<MarkWrite>,
    /// The notification the change earns (see [`notice`]).
    pub notice: Option<Notice>,
}

/// The writes `decision` makes to a pane whose record is `prev` (see [`Update`]).
pub fn update(prev: &AgentRecord, decision: &Decision) -> Update {
    // The kind is written only when it changed, and the name goes with the agent it was
    // given to.
    let recorded = |kind: &str| Update {
        kind: (prev.kind.as_deref() != Some(kind)).then(|| kind.to_string()),
        release_name: prev.name.is_some()
            && name_leaves_with(prev.kind.as_deref(), kind, prev.kind_hint.as_deref()),
        release_identity: prev.kind.as_deref().is_some_and(|last| last != kind),
        ..Update::default()
    };
    match decision {
        Decision::NoAgent { clear } => Update {
            clear: *clear,
            ..Update::default()
        },
        // A hook that owns a pane whose command names no agent we know keeps its own
        // `@agent_kind`: it knows what it is, and overwriting that with nothing would be a
        // downgrade.
        Decision::Keep | Decision::HookAuthoritative { kind: None } => Update::default(),
        Decision::HookAuthoritative { kind: Some(kind) } | Decision::KindChanged { kind } => {
            recorded(kind)
        }
        Decision::RuleChanged { rule } => Update {
            rule: Some(rule.clone()),
            ..Update::default()
        },
        Decision::Reported {
            kind,
            from,
            via,
            to,
            reported,
            replaced,
            seen,
            ready,
            ..
        } => {
            let changed = *from != Some(*to) || via.is_some();
            let base = recorded(kind);
            Update {
                // A name belongs to the agent it was given to: the agent that has gone took it.
                release_name: base.release_name
                    || (prev.name.is_some()
                        && replaced.as_deref().is_some_and(|last| {
                            name_leaves_with(Some(last), kind, prev.kind_hint.as_deref())
                        })),
                complete: (to != reported).then_some((*reported, *to)),
                // Only a change is announced and marks the pane seen or not: what is written
                // back over a report that repeated itself is not one.
                seen: if changed {
                    seen_written(*to, *seen)
                } else {
                    None
                },
                // An agent that reports for itself often ends with its last report (`opencode
                // run`), and its shell's return then clears the record inside the done delay: the
                // finish is news all the same, as an exit's is. An agent becoming ready finished
                // nothing (step 11 of [`decide`]).
                notice: if changed && !ready {
                    // The step that ends the change: from the state the agent went through, if it did.
                    notice(via.or(*from), *to, *seen, None).map(|notice| Notice {
                        exit: notice.class == Class::Done,
                        ..notice
                    })
                } else {
                    None
                },
                ..base
            }
        }
        // The agent is kept, at `idle` with the suspend for its reason: a pane its hook reports
        // for keeps the hook as the source of its state. A suspend finishes nothing, so no
        // notification, and it is no new turn, so the pane's seen mark stays.
        Decision::Suspended {
            kind, from, hook, ..
        } => {
            let idle = AgentState::Idle;
            let over = from.filter(|from| *hook && *from != idle);
            Update {
                state: (over.is_none() && *from != Some(idle))
                    .then(|| (idle, Some(store::SUSPENDED_RULE.to_string()))),
                complete: over.map(|from| (from, idle)),
                rule: Some(Some(store::SUSPENDED_RULE.to_string())),
                ..recorded(kind)
            }
        }
        Decision::Transition {
            kind,
            from,
            to,
            rule,
            seen,
            ready,
            ..
        } => Update {
            state: Some((*to, rule.clone())),
            // A new turn starts unseen; a completion is seen exactly when the user is
            // looking at the pane as it happens, and a `done` is not.
            seen: seen_written(*to, *seen),
            // An agent becoming ready finished nothing (step 11 of [`decide`]).
            notice: if *ready {
                None
            } else {
                notice(*from, *to, *seen, rule.as_deref())
            },
            ..recorded(kind)
        },
    }
}

impl Update {
    /// The update with what the read that decided it leaves to be written besides, which `arb`
    /// carries: herdr's completion reset, which marks the pane seen ([`Update::reset`], step 12 of
    /// [`decide`]), and the `@agent_acquiring` mark when it changed (step 11). A clear takes the
    /// mark off with the rest of the record. `prev` is the record the update is written over.
    pub fn finish(mut self, arb: &mut Arbiter, prev: &AgentRecord) -> Update {
        if arb.take_reset() {
            self = self.reset(prev);
        }
        let mark = arb.take_mark();
        if !self.clear {
            self.acquiring = mark;
        }
        self
    }

    /// The update after herdr's completion reset at the same read (step 12 of [`decide`],
    /// [`Arbiter::take_reset`]): the pane is marked seen, whatever its state, unless the change
    /// this read found decides that itself (a turn starting, or a `done`), or there is no agent to
    /// mark. `prev` is the record the update is written over.
    pub fn reset(mut self, prev: &AgentRecord) -> Update {
        let agent = prev.kind.is_some() || prev.state.is_some();
        if agent && !self.clear && self.seen.is_none() && !prev.seen {
            self.seen = Some(true);
        }
        self
    }
}

/// Whether the state a read writes is the one its agent leaves with: the end of a turn that its
/// process exiting ends (the first step of the two-step exit, herdr's), or any state written by a
/// read that found the process of an agent only its hint names gone. The watcher writes it only
/// while no report has landed in the pane since the read (`store::write_exit`).
pub fn leaves(rule: Option<&str>, exited: bool) -> bool {
    rule == Some(PROCESS_EXITED_RULE) || exited
}

/// The status an agent leaves its pane with ([`crate::store::FINAL_OPTION`]), from the write of the
/// read that finds it gone: `to` is what is written over `from`, `rule` the rule written with it, and
/// `exited` whether the pid recorded for the agent is dead.
///
/// An exit's completion is the status it is written as (`idle` or `done`), whoever names the agent.
/// An agent whose pane keeps only its hint, and was at rest when its process went, is written
/// `unknown` over the status it had, and leaves with that: the pane says nothing of it, and a wait
/// for the agent is told what it was. Every other write is that of an agent that is still there.
pub fn left_as(
    from: Option<AgentState>,
    to: AgentState,
    rule: Option<&str>,
    exited: bool,
) -> Option<AgentState> {
    let status = if rule == Some(PROCESS_EXITED_RULE) {
        to
    } else if exited && to == AgentState::Unknown {
        from?
    } else {
        return None;
    };
    (status != AgentState::Unknown).then_some(status)
}

/// The record after `taw seen` marks the pane looked at, or `None` when there is no agent
/// to mark: `@agent_seen = 1`, and a `done` goes back to `idle` with its rule and source
/// kept — `done` only says "finished while you were not looking". `watch::mark_seen`
/// writes what changed, and the fake-clock harness keeps the record.
pub fn marked_seen(record: &AgentRecord) -> Option<AgentRecord> {
    if record.kind.is_none() && record.state.is_none() {
        return None;
    }
    let mut seen = record.clone();
    seen.seen = true;
    if seen.state == Some(AgentState::Done) {
        seen.state = Some(AgentState::Idle);
    }
    Some(seen)
}

/// Which notification class a transition earns, if any: arriving at `blocked` is a
/// request for the user, and a completion — `idle`/`done` after `working` or `blocked`,
/// herdr's `is_background_completion_transition` (`src/app/actions.rs`, master
/// `0d5d6f1`) — is work finishing.
pub fn notification_class(from: Option<AgentState>, to: AgentState) -> Option<Class> {
    match (from, to) {
        (_, AgentState::Blocked) => Some(Class::Request),
        (Some(AgentState::Working | AgentState::Blocked), AgentState::Idle | AgentState::Done) => {
            Some(Class::Done)
        }
        _ => None,
    }
}

/// A notification a transition earns: delivered once its delay is up, if the pane still
/// shows what earned it and nobody is looking at it then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub class: Class,
    /// The state that earned it, which `@agent_state` must still hold when the delay is up.
    pub state: AgentState,
    /// It is an exit's completion (rule `process_exited`), or the finish an agent reported for
    /// itself: the record is cleared a tick after the exit is written, and an agent whose last
    /// report ends its run is cleared when its shell is back, so by the time the delay is up an
    /// empty `@agent_state` is what the end left behind.
    pub exit: bool,
}

impl Notice {
    /// Whether `@agent_state` read when the delay is up (`None` when it is unset) still
    /// stands behind the notification: the same state, or for an exit, nothing at all. A
    /// new agent started in the pane in between reads `unknown`, and the old run's end is
    /// no longer news.
    ///
    /// The pane of an agent a process does not name (one `taw agent start` started, under a script or
    /// a wrapper) is not cleared when the agent exits, as its hint stays: it reads `unknown` a tick
    /// after the exit's `done`. That stands for an exit too, while the pid recorded for the agent is
    /// dead (`process_exited`, asked only when it is needed): the `unknown` of a new agent has no
    /// such pid, or one that lives.
    pub fn stands(&self, current: Option<&str>, process_exited: impl FnOnce() -> bool) -> bool {
        match current.filter(|value| !value.is_empty()) {
            Some(value) if value == self.state.as_str() => true,
            Some(value) if value == AgentState::Unknown.as_str() => self.exit && process_exited(),
            Some(_) => false,
            None => self.exit,
        }
    }
}

/// Who a notification was earned for: the agent the pane recorded once the change that earned it
/// was written, and that agent's session (`@agent_session_id`, `@agent_session_path`).
///
/// herdr drops a pane's pending notification when the pane's agent or the agent's session changes
/// (its completion reset, step 12 of [`decide`]). Here a notification waits out its delay on a
/// thread of its own, so it is dropped when it comes due if the pane holds another agent or
/// another session by then, whether or not the watcher has read the pane since.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Earned {
    pub kind: Option<String>,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
}

impl Earned {
    /// The agent and the session `record` holds.
    pub fn of(record: &AgentRecord) -> Earned {
        Earned {
            kind: record.kind.clone(),
            session_id: record.session_id.clone(),
            session_path: record.session_path.clone(),
        }
    }

    /// What the pane holds of its agent and session once `update` is written over `prev`: the
    /// kind it records, and the session, which goes when another agent takes the pane
    /// ([`Update::release_identity`]).
    pub fn after(prev: &AgentRecord, update: &Update) -> Earned {
        let kept = !update.release_identity;
        Earned {
            kind: update.kind.clone().or_else(|| prev.kind.clone()),
            session_id: prev.session_id.clone().filter(|_| kept),
            session_path: prev.session_path.clone().filter(|_| kept),
        }
    }
}

/// A notification waiting out its delay, with who earned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub notice: Notice,
    pub earned: Earned,
}

impl Pending {
    /// Whether the notification is delivered when its delay is up, from what the pane holds then:
    /// its state (`state`, `None` when unset) still [stands](Notice::stands) behind it, and the pane
    /// still holds the agent and the session it was earned for (`now`). An exit's notice that stands
    /// over a record cleared behind it (no state, or the `unknown` a hint keeps) is not held to the
    /// second: that record went as the agent ended.
    pub fn due(
        &self,
        state: Option<&str>,
        now: &Earned,
        process_exited: impl FnOnce() -> bool,
    ) -> bool {
        if !self.notice.stands(state, process_exited) {
            return false;
        }
        let cleared = state
            .filter(|value| !value.is_empty())
            .is_none_or(|value| value == AgentState::Unknown.as_str());
        (self.notice.exit && cleared) || self.earned == *now
    }
}

/// The notification the transition `from -> to` earns, if any (see [`notification_class`]),
/// given whether the user is looking at the pane as it happens (`seen`) and the rule
/// written with it.
///
/// Whether work finishing is news is decided when it finishes, as herdr decides it
/// (`notification_toast_for_effective_state_change`, `src/app/actions.rs`): a completion
/// the user watched earns no done notification, whatever they do while its delay runs —
/// leaving straight after does not make it news. A request is still gated only when its
/// delay is up. An agent that exits mid-turn is a completion like any other (herdr writes
/// its idle "so notifications ... observe completion"), and its notice outlives the clear.
pub fn notice(
    from: Option<AgentState>,
    to: AgentState,
    seen: bool,
    rule: Option<&str>,
) -> Option<Notice> {
    let class = notification_class(from, to)?;
    if class == Class::Done && seen {
        return None;
    }
    Some(Notice {
        class,
        state: to,
        exit: rule == Some(PROCESS_EXITED_RULE),
    })
}

/// The notification body for a transition: the first line of the region the matched rule
/// looked at (the permission prompt, the spinner line, ...) that says something, falling
/// back to the pane title when no rule matched or its region is empty. A region that holds
/// only an agent's input caret or box lines (a done Claude's idle rule looks at its empty
/// input box) says nothing: the detail is then the last line above the input box, which is
/// what the agent said, or nothing.
fn detail(evaluation: &detect::Evaluation, snapshot: &Snapshot, title: &str) -> String {
    let Some(text) = evaluation
        .matched_rule
        .as_ref()
        .and_then(|rule| detect::region_text(&rule.region, snapshot))
        .filter(|text| !text.trim().is_empty())
    else {
        return title.trim().to_string();
    };
    first_text_line(text)
        .or_else(|| {
            detect::region_text("last_non_empty_above_prompt_box", snapshot)
                .and_then(first_text_line)
        })
        .unwrap_or_default()
}

/// True for a line that is only an input caret or box and rule glyphs (and spaces): the
/// chrome around an agent's input box, which tells a reader nothing.
fn is_chrome_line(line: &str) -> bool {
    const CARETS: &[char] = &['\u{276f}', '>', '\u{203a}'];
    line.chars()
        .all(|c| c.is_whitespace() || CARETS.contains(&c) || ('\u{2500}'..='\u{257f}').contains(&c))
}

fn first_text_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_chrome_line(line))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const IDLE: &str = include_str!("../../tests/fixtures/screens/claude-idle.txt");
    const WORKING: &str = include_str!("../../tests/fixtures/screens/claude-2.1.227-working.txt");
    const BLOCKED: &str = include_str!("../../tests/fixtures/screens/claude-permission.txt");
    /// Matches claude.toml's `model_picker_menu` (priority 900, `skip_state_update`) and
    /// nothing above it: no prompt box, no spinner, no horizontal rule.
    const MODEL_PICKER: &str = "Select model\nEnter to set as default\nEsc to cancel\n";

    /// A store pointing at directories that do not exist, so every `load` falls through
    /// to the bundled manifest — the same rules `tests/detect_fixtures.rs` pins.
    fn bundled_only() -> ManifestStore {
        ManifestStore::new(
            PathBuf::from("/nonexistent/taw-test-override"),
            PathBuf::from("/nonexistent/taw-test-remote"),
        )
    }

    fn prev(kind: Option<&str>, state: Option<AgentState>, seen: bool) -> AgentRecord {
        AgentRecord {
            kind: kind.map(str::to_string),
            state,
            seen,
            ..Default::default()
        }
    }

    fn prev_with_rule(
        kind: Option<&str>,
        state: Option<AgentState>,
        rule: Option<&str>,
    ) -> AgentRecord {
        AgentRecord {
            rule: rule.map(str::to_string),
            ..prev(kind, state, false)
        }
    }

    fn observed(command: &str, screen: &str, prev: AgentRecord) -> Observed {
        Observed {
            command: command.to_string(),
            title: "\u{2733} fake task".to_string(),
            window_active: false,
            session_attached: 0,
            terminal_focus: None,
            kind_hint: None,
            container: None,
            job_agent: None,
            prev,
            screen: screen.to_string(),
            osc_progress: String::new(),
            pane_shell_idle: None,
            turn: None,
            acquiring: None,
            screen_unread: false,
            below_typed_line: false,
            suspended: false,
        }
    }

    /// One read by a watcher that already knows whichever agent the pane names, so the
    /// rules that span reads stay out of the way of the one under test.
    fn decide_on(o: &Observed) -> Decision {
        decide(
            &bundled_only(),
            &mut HashMap::new(),
            o,
            false,
            Instant::now(),
            &mut settled_for(o),
        )
    }

    /// The arbiter of a watcher that has been watching `o`'s agent for a while.
    fn settled_for(o: &Observed) -> Arbiter {
        match o.identity() {
            Some(kind) => Arbiter::settled(&kind),
            None => Arbiter::default(),
        }
    }

    /// A read `ms` after `base`, arbitrated by `arb` — for the rules that span reads.
    fn decide_at(o: &Observed, base: Instant, ms: u64, arb: &mut Arbiter) -> Decision {
        decide(
            &bundled_only(),
            &mut HashMap::new(),
            o,
            false,
            base + Duration::from_millis(ms),
            arb,
        )
    }

    /// A claude mid-turn, in the background and unseen: the record the hold starts from.
    fn working_turn() -> AgentRecord {
        prev_with_rule(
            Some("claude"),
            Some(AgentState::Working),
            Some("live_turn_working"),
        )
    }

    /// One tool line and nothing else: a redraw caught half-way.
    const HALF_DRAWN: &str = "\u{23FA} Update(src/x.rs)\n";

    /// `screen` read mid-turn: under the turn's own `✻ …` title, which no rule matches,
    /// a screen that matches nothing either is a plain idle.
    fn mid_turn(screen: &str) -> Observed {
        let mut o = observed("claude", screen, working_turn());
        o.title = "\u{273B} fake task".to_string();
        o
    }

    #[test]
    fn a_read_with_no_screen_to_decide_on_keeps_what_the_pane_says() {
        // A claude only its hint names, blocked, read once with the line its command was typed on
        // missing from the screen: an empty screen would read `unknown`, and the next read `blocked`
        // again, with a second request.
        let mut o = observed(
            "sh",
            "",
            prev(Some("claude"), Some(AgentState::Blocked), false),
        );
        o.kind_hint = Some("claude".to_string());
        o.screen_unread = true;
        assert_eq!(decide_on(&o), Decision::Keep);
        o.screen_unread = false;
        assert_ne!(decide_on(&o), Decision::Keep, "an empty screen is read");
        // A new agent's first read begins its grace all the same, which reads no screen either.
        let mut fresh = observed("sh", "", AgentRecord::default());
        fresh.kind_hint = Some("claude".to_string());
        fresh.screen_unread = true;
        let mut arb = Arbiter::default();
        assert!(matches!(
            decide(
                &bundled_only(),
                &mut HashMap::new(),
                &fresh,
                false,
                Instant::now(),
                &mut arb
            ),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
        assert!(arb.in_startup_grace());
    }

    #[test]
    fn an_agent_started_again_by_another_name_of_its_kind_is_relaunched_all_the_same() {
        // `agent start --kind kiro-cli` writes that name as the pane's hint, and the agent it starts
        // is Kiro, named at once by its process: a relaunch as much as one by the id `kiro`. What it
        // prints before it draws is no idle of its own.
        let update = "Downloading kiro-cli update\n  12% of 40 MB\n";
        let mut o = observed("kiro-cli", update, AgentRecord::default());
        o.title = String::new();
        o.kind_hint = Some("kiro-cli".to_string());
        o.below_typed_line = true;
        let mut arb = Arbiter::settled("kiro");
        let base = Instant::now();
        decide_at(&o, base, 0, &mut arb);
        o.prev = prev(Some("kiro"), Some(AgentState::Unknown), false);
        assert_eq!(decide_at(&o, base, 300, &mut arb), Decision::Keep);
    }

    #[test]
    fn an_install_printed_below_the_typed_line_is_unknown_when_the_job_in_front_names_the_agent() {
        // `agent start` typed `claude`, and what runs is a script under `sh` that the job in front of
        // the pane names, and that prints an install's progress before the agent it fetches starts, as
        // `npx` does. No rule of Claude's knows those rows: no idle of the new agent's.
        let install = "Need to install the following packages:\n  @anthropic-ai/claude-code@2.1.300\n\
                       added 3 packages in 2s\n";
        // The agent is starting: the read before this one identified it, the record taken off.
        for (command, job) in [("sh", Some("claude")), ("claude", None)] {
            let mut o = observed(command, install, AgentRecord::default());
            o.title = String::new();
            o.kind_hint = Some("claude".to_string());
            o.job_agent = job.map(str::to_string);
            o.below_typed_line = true;
            let mut arb = Arbiter::settled("claude");
            let base = Instant::now();
            decide_at(&o, base, 0, &mut arb);
            o.prev = prev(Some("claude"), Some(AgentState::Unknown), false);
            assert_eq!(
                decide_at(&o, base, 300, &mut arb),
                Decision::Keep,
                "{command}"
            );
            // The same rows read whole, with no line on the screen, are the agent's to read.
            o.below_typed_line = false;
            assert!(
                matches!(
                    decide_on(&o),
                    Decision::Transition {
                        to: AgentState::Idle,
                        ..
                    }
                ),
                "{command}: {:?}",
                decide_on(&o)
            );
        }
    }

    #[test]
    fn a_blank_screen_below_the_typed_line_is_unknown_when_the_job_in_front_names_the_agent() {
        // `agent start` typed `claude` at a zsh prompt, and the claude it started is a script under `sh`,
        // which tmux names `bash`: the job in front of the pane names it, as well as its hint. It has
        // drawn nothing below the line yet, after its grace: no idle of its own, whatever the last agent
        // left above the line.
        let mut o = observed("bash", "\n\n", AgentRecord::default());
        o.title = String::new();
        o.kind_hint = Some("claude".to_string());
        o.job_agent = Some("claude".to_string());
        o.below_typed_line = true;
        assert!(
            matches!(
                decide_on(&o),
                Decision::Transition {
                    to: AgentState::Unknown,
                    ..
                }
            ),
            "{:?}",
            decide_on(&o)
        );
        // The same blank screen read whole, with no line on it, is that agent's to read: its rules say idle.
        o.below_typed_line = false;
        assert!(
            matches!(
                decide_on(&o),
                Decision::Transition {
                    to: AgentState::Idle,
                    ..
                }
            ),
            "{:?}",
            decide_on(&o)
        );
        // Something drawn below the line is read with the agent's rules: a frame one of them knows.
        o.below_typed_line = true;
        o.screen = IDLE.to_string();
        assert!(!matches!(
            decide_on(&o),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn a_plain_idle_after_working_is_held_until_three_more_reads_confirm_it() {
        // herdr's `pending_idle_holds_working_to_plain_idle_until_confirmed`: the first
        // plain-idle read starts the hold and writes nothing, two more confirm it, and
        // the third confirmation releases it as the ordinary transition.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let o = mid_turn(HALF_DRAWN);
        for ms in [0, 100, 200] {
            assert_eq!(
                decide_at(&o, base, ms, &mut arb),
                Decision::Keep,
                "at {ms} ms"
            );
            assert!(arb.hold_active(), "at {ms} ms");
        }
        match decide_at(&o, base, 300, &mut arb) {
            Decision::Transition { from, to, rule, .. } => {
                assert_eq!(from, Some(AgentState::Working));
                assert_eq!(to, AgentState::Done, "background and unseen");
                assert_eq!(rule, None);
            }
            other => panic!("the third confirmation must release the hold, got {other:?}"),
        }
        assert!(!arb.hold_active());
    }

    #[test]
    fn spinner_then_plain_idle_is_written_once_after_three_confirmations_cap_releases_at_700ms() {
        // Reads too far apart to confirm three times: 700 ms after the first, the cap
        // releases it with one confirmation.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let o = mid_turn(HALF_DRAWN);
        assert_eq!(decide_at(&o, base, 0, &mut arb), Decision::Keep);
        assert_eq!(decide_at(&o, base, 400, &mut arb), Decision::Keep);
        assert!(
            matches!(
                decide_at(&o, base, 700, &mut arb),
                Decision::Transition {
                    to: AgentState::Done,
                    ..
                }
            ),
            "the cap releases the hold 700 ms after it started"
        );
        assert!(!arb.hold_active());
    }

    #[test]
    fn a_visible_idle_after_working_is_written_at_once() {
        // herdr's `visible_idle_bypasses_plain_idle_hold`: the prompt box at rest is the
        // turn's end, not a redraw caught half-way.
        let mut arb = Arbiter::settled("claude");
        let o = observed("claude", IDLE, working_turn());
        match decide_at(&o, Instant::now(), 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
            }
            other => panic!("a visible idle is not held, got {other:?}"),
        }
        assert!(!arb.hold_active());
    }

    #[test]
    fn a_blank_frame_under_an_idle_title_is_a_visible_idle_and_written_at_once() {
        // `osc_title_idle` carries `visible_idle`, so a blank screen under a `✳ …` title
        // is an idle Claude said it was in — herdr does not hold it either.
        let mut arb = Arbiter::settled("claude");
        let o = observed("claude", "\n\n", working_turn());
        match decide_at(&o, Instant::now(), 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("osc_title_idle"));
            }
            other => panic!("a visible idle is not held, got {other:?}"),
        }
    }

    #[test]
    fn a_working_frame_ends_the_hold_and_the_next_plain_idle_starts_a_new_one() {
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let blank = mid_turn(HALF_DRAWN);
        let mut spinner = observed("claude", WORKING, working_turn());
        spinner.title = "\u{273B} fake task".to_string();
        assert_eq!(decide_at(&blank, base, 0, &mut arb), Decision::Keep);
        assert_eq!(decide_at(&spinner, base, 100, &mut arb), Decision::Keep);
        assert!(!arb.hold_active(), "the redraw finished: the turn goes on");
        // Three fresh confirmations are needed, and the cap counts from 200, not 0.
        for ms in [200, 300, 400] {
            assert_eq!(
                decide_at(&blank, base, ms, &mut arb),
                Decision::Keep,
                "at {ms} ms"
            );
        }
        assert!(matches!(
            decide_at(&blank, base, 500, &mut arb),
            Decision::Transition {
                to: AgentState::Done,
                ..
            }
        ));
    }

    #[test]
    fn a_skip_state_update_frame_keeps_working_and_ends_the_hold() {
        // An overlay hides the real state rather than replacing it; herdr clears the hold
        // on such a frame (`src/pane.rs:3051-3054`), so the next plain idle starts again.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        assert_eq!(
            decide_at(&mid_turn(HALF_DRAWN), base, 0, &mut arb),
            Decision::Keep
        );
        let picker = observed("claude", MODEL_PICKER, working_turn());
        assert_eq!(decide_at(&picker, base, 100, &mut arb), Decision::Keep);
        assert!(!arb.hold_active());
    }

    #[test]
    fn a_hook_source_ends_the_hold() {
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        assert_eq!(
            decide_at(&mid_turn(HALF_DRAWN), base, 0, &mut arb),
            Decision::Keep
        );
        let mut hooked = mid_turn(HALF_DRAWN);
        hooked.prev.source = Some("hook:claude-code".to_string());
        assert!(matches!(
            decide_at(&hooked, base, 100, &mut arb),
            Decision::HookAuthoritative { .. }
        ));
        assert!(!arb.hold_active());
    }

    #[test]
    fn an_exited_agents_idle_is_not_held() {
        // A dead start pid settles what the hold exists to find out: the turn is over.
        let mut o = mid_turn(HALF_DRAWN);
        o.command = "bash".to_string();
        o.kind_hint = Some("claude".to_string());
        let mut arb = Arbiter::settled("claude");
        let decision = decide(
            &bundled_only(),
            &mut HashMap::new(),
            &o,
            true,
            Instant::now(),
            &mut arb,
        );
        assert!(
            matches!(
                decision,
                Decision::Transition {
                    to: AgentState::Done,
                    ..
                }
            ),
            "got {decision:?}"
        );
        assert!(!arb.hold_active());
    }

    #[test]
    fn a_pane_that_loses_its_agent_ends_the_hold() {
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        assert_eq!(
            decide_at(&mid_turn(HALF_DRAWN), base, 0, &mut arb),
            Decision::Keep
        );
        let mut gone = mid_turn(HALF_DRAWN);
        gone.command = "zsh".to_string();
        let _ = decide_at(&gone, base, 100, &mut arb);
        assert!(!arb.hold_active());
    }

    #[test]
    fn a_plain_idle_from_a_different_agent_is_not_held() {
        // The hold confirms that *this* turn ended; a new occupant's first frame is not
        // evidence about it.
        let mut arb = Arbiter::settled("claude");
        let mut o = mid_turn(HALF_DRAWN);
        o.prev.kind = Some("codex".to_string());
        assert!(matches!(
            decide_at(&o, Instant::now(), 0, &mut arb),
            Decision::Transition { .. }
        ));
        assert!(!arb.hold_active());
    }

    #[test]
    fn only_a_pane_whose_manifest_can_decide_has_its_screen_captured() {
        // Every read of a pane whose screen cannot matter would otherwise cost a
        // `capture-pane` for nothing: an agentless pane printing output is read up to
        // twice a second.
        let wants = |o: &Observed| wants_screen(o, &settled_for(o));
        assert!(!wants(&observed("zsh", "", AgentRecord::default())));
        assert!(wants(&observed("claude", "", AgentRecord::default())));
        let mut hinted = observed("bash", "", AgentRecord::default());
        hinted.kind_hint = Some("claude".to_string());
        assert!(wants(&hinted), "the hint names an agent");
        // A hook owns the state, so the manifest is never consulted.
        let mut hooked = prev(Some("claude"), Some(AgentState::Working), false);
        hooked.source = Some("hook:claude-code".to_string());
        assert!(!wants(&observed("claude", "", hooked)));
        // The shell is back and the record is stale: cleared from the display alone.
        assert!(!wants(&observed(
            "zsh",
            "",
            prev(Some("claude"), Some(AgentState::Idle), false)
        )));
        // Another process in front of a recorded agent: the agent's rules read its
        // screen until the misses run out.
        assert!(wants(&observed(
            "vim",
            "",
            prev(Some("claude"), Some(AgentState::Working), false)
        )));
    }

    #[test]
    fn a_new_agents_screen_is_not_captured_until_its_grace_is_over() {
        // Inside the startup grace nothing on screen is believed, so no read of it needs
        // one; the read after the window's extra tick does.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let first = observed("claude", IDLE, AgentRecord::default());
        assert!(
            wants_screen(&first, &arb),
            "nothing has started a grace yet"
        );
        let _ = decide_at(&first, base, 0, &mut arb);
        let unknown = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        for ms in [300, 2700] {
            assert!(!wants_screen(&unknown, &arb), "at {ms} ms");
            // What the screen shows makes no difference to the read.
            for screen in [IDLE, WORKING, BLOCKED, ""] {
                let mut o = unknown.clone();
                o.screen = screen.to_string();
                let mut probe = Arbiter {
                    grace: arb.grace,
                    ..Arbiter::settled("claude")
                };
                assert_eq!(decide_at(&o, base, ms, &mut probe), Decision::Keep);
            }
            let _ = decide_at(&unknown, base, ms, &mut arb);
        }
        // The read at the window's end is kept too, and so screenless; the one after it
        // writes what the screen shows.
        assert!(!wants_screen(&unknown, &arb));
        assert_eq!(decide_at(&unknown, base, 3000, &mut arb), Decision::Keep);
        assert!(wants_screen(&unknown, &arb), "the grace is over");
    }

    #[test]
    fn a_pane_with_no_agent_identity_and_a_stale_record_asks_for_a_clear() {
        let o = observed(
            "zsh",
            IDLE,
            prev(Some("claude"), Some(AgentState::Idle), false),
        );
        assert_eq!(decide_on(&o), Decision::NoAgent { clear: true });
    }

    #[test]
    fn a_pane_that_never_had_an_agent_needs_nothing_cleared() {
        let o = observed("zsh", "$ ls\n", AgentRecord::default());
        assert_eq!(decide_on(&o), Decision::NoAgent { clear: false });
    }

    #[test]
    fn identity_can_come_from_the_kind_hint_when_the_command_is_not_an_agent() {
        // The pane holds what the watcher wrote when it first took the hint for an agent: a kind
        // that nothing has read yet. (With no record at all, a watcher that already holds the
        // identity is looking at a new agent, which has a grace of its own.)
        let mut o = observed(
            "bash",
            IDLE,
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        o.kind_hint = Some("claude".to_string());
        match decide_on(&o) {
            Decision::Transition { kind, to, .. } => {
                assert_eq!(kind, "claude");
                assert_eq!(to, AgentState::Idle);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn the_hint_names_the_agent_even_when_the_command_names_a_different_one() {
        // `taw agent start --kind claude` is a statement about the pane; a foreground
        // process that happens to be a *different* known agent does not overrule it.
        // `detect::identify` puts the hint first, and `decide` must agree with it.
        let mut o = observed(
            "codex",
            IDLE,
            prev(Some("codex"), Some(AgentState::Idle), true),
        );
        o.kind_hint = Some("claude".to_string());
        match decide_on(&o) {
            Decision::KindChanged { kind } => assert_eq!(kind, "claude"),
            other => panic!("expected the hint's kind, got {other:?}"),
        }

        // And with no hint, the command is what identifies the pane — the same call,
        // reading the other argument.
        let plain = observed(
            "codex",
            IDLE,
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        match decide_on(&plain) {
            Decision::KindChanged { kind } => assert_eq!(kind, "codex"),
            other => panic!("expected the command's kind, got {other:?}"),
        }
    }

    #[test]
    fn a_hint_with_nothing_behind_it_reports_unknown_rather_than_a_stale_state() {
        // The agent this hint named has exited. `@agent_kind_hint` outlives it by design,
        // so identity still resolves and the pane can never reach `NoAgent`, so it would
        // sit at whatever state it was left in for ever. The shell prompt on screen
        // matches no rule. (`idle` rather than `working` as the previous state: a live
        // turn is deliberately never interrupted by `unknown` — see the test above.)
        let mut o = observed(
            "bash",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        o.kind_hint = Some("claude".to_string());
        o.title = String::new();
        match decide_on(&o) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, Some(AgentState::Idle));
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None, "no rule can explain a state nothing showed");
            }
            other => panic!("expected a transition to unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_transient_unmatched_screen_must_not_swallow_a_working_turns_done_edge() {
        // A capture that lands mid-redraw matches nothing. On a hint-only pane that used
        // to mean `unknown` — which overwrote a live `working` and destroyed the
        // `working -> done` edge the entire notification path hangs on: once the state
        // was `unknown`, the next evaluation of the finished screen was a plain `idle`
        // and the user was never told the agent had finished.
        //
        // Such a frame is a plain idle, so it is held as herdr holds it: nothing is
        // written until three more reads confirm it, and then it is the turn's `done`.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut o = observed(
            "bash",
            "\n\n",
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        o.kind_hint = Some("claude".to_string());
        o.title = String::new();
        for ms in [0, 100, 200] {
            assert_eq!(
                decide_at(&o, base, ms, &mut arb),
                Decision::Keep,
                "held at {ms} ms"
            );
        }
        match decide_at(&o, base, 300, &mut arb) {
            Decision::Transition { to, .. } => assert_eq!(
                to,
                AgentState::Done,
                "a working turn ends as `done` in the background, never as `unknown`"
            ),
            other => panic!("expected a transition to done, got {other:?}"),
        }

        // One evaluation later the state is no longer `working`, so a pane that really
        // has lost its agent does reach `unknown` — just not by interrupting a turn.
        let mut later = observed(
            "bash",
            "\n\n",
            prev(Some("claude"), Some(AgentState::Done), false),
        );
        later.kind_hint = Some("claude".to_string());
        later.title = String::new();
        match decide_on(&later) {
            Decision::Transition { to, .. } => assert_eq!(to, AgentState::Unknown),
            other => panic!("expected a transition to unknown, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_pane_stays_unknown_without_rewriting_anything() {
        let mut o = observed(
            "bash",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        o.kind_hint = Some("claude".to_string());
        o.title = String::new();
        assert_eq!(decide_on(&o), Decision::Keep);
    }

    #[test]
    fn a_hint_backed_by_a_matching_rule_is_the_state_that_rule_names() {
        // The other half of the hint's rule: the hint is not what makes this `unknown`, the
        // absence of evidence is. A drawn screen still reports what it shows.
        let mut o = observed(
            "bash",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        o.kind_hint = Some("claude".to_string());
        match decide_on(&o) {
            Decision::Transition { to, rule, .. } => {
                // Background and unseen, so the finished turn promotes to `done`.
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    /// The pane of an agent run in a container: tmux names `docker`, and the agent is the one the
    /// runtime's arguments name (`watch::container` reads them).
    fn in_container(mut o: Observed, kind: &str) -> Observed {
        o.command = "docker".to_string();
        o.container = Some(detect::ContainerAgent {
            runtime: "docker",
            kind: kind.to_string(),
        });
        o
    }

    #[test]
    fn an_agent_in_a_container_is_read_by_its_own_rules() {
        // The screen is what Claude draws, so the states and the rules are the ones it has
        // on the host. Nothing is recorded for the pane yet: without the container the pane
        // holds no agent at all, as `docker` is no agent's name.
        let read = |screen: &str, prev: AgentRecord| {
            decide_at(
                &in_container(observed("docker", screen, prev), "claude"),
                Instant::now(),
                0,
                &mut Arbiter::settled("claude"),
            )
        };
        match read(IDLE, AgentRecord::default()) {
            Decision::Transition { kind, to, rule, .. } => {
                assert_eq!((kind.as_str(), to), ("claude", AgentState::Idle));
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
            }
            other => panic!("expected idle, got {other:?}"),
        }
        let at_rest = prev_with_rule(
            Some("claude"),
            Some(AgentState::Idle),
            Some("live_prompt_box"),
        );
        assert_eq!(read(IDLE, at_rest.clone()), Decision::Keep);
        match read(WORKING, at_rest.clone()) {
            Decision::Transition { kind, to, rule, .. } => {
                assert_eq!((kind.as_str(), to), ("claude", AgentState::Working));
                assert_eq!(rule.as_deref(), Some("live_turn_working"));
            }
            other => panic!("expected working, got {other:?}"),
        }
        match read(BLOCKED, at_rest) {
            Decision::Transition { kind, to, .. } => {
                assert_eq!((kind.as_str(), to), ("claude", AgentState::Blocked));
            }
            other => panic!("expected blocked, got {other:?}"),
        }
    }

    #[test]
    fn a_container_runtime_with_no_agent_in_it_holds_no_agent() {
        // `docker ps`, or a run of nginx: the runtime is in front and names nothing.
        let o = observed("docker", "CONTAINER ID   IMAGE\n", AgentRecord::default());
        assert_eq!(decide_on(&o), Decision::NoAgent { clear: false });
    }

    #[test]
    fn a_container_agent_read_off_the_runtime_is_not_a_hint_only_pane() {
        // With only the hint behind it, a pane whose screen matches no rule is `unknown`:
        // nothing says what the agent is doing. The arguments of the runtime in front are
        // evidence that an agent is there, as its own command would be.
        let hinted = |container: bool| {
            let mut o = observed(
                "docker",
                "\n\n",
                prev(Some("claude"), Some(AgentState::Idle), true),
            );
            o.kind_hint = Some("claude".to_string());
            o.title = String::new();
            if container {
                in_container(o, "claude")
            } else {
                o
            }
        };
        assert!(matches!(
            decide_on(&hinted(false)),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
        assert_eq!(decide_on(&hinted(true)), Decision::Keep);
    }

    #[test]
    fn the_hint_names_the_agent_before_the_container_does() {
        let mut o = in_container(observed("docker", IDLE, AgentRecord::default()), "claude");
        assert_eq!(o.identity().as_deref(), Some("claude"));
        assert_eq!(o.process_identity().as_deref(), Some("claude"));
        o.kind_hint = Some("codex".to_string());
        assert_eq!(o.identity().as_deref(), Some("codex"));
        assert_eq!(
            o.process_identity().as_deref(),
            Some("claude"),
            "the hint is not the process"
        );
        // A command that names an agent itself comes before a container's.
        o.command = "gemini".to_string();
        o.kind_hint = None;
        assert_eq!(o.identity().as_deref(), Some("gemini"));
        // And a runtime with nothing found in it names nobody.
        o.command = "docker".to_string();
        o.container = None;
        assert_eq!(o.identity(), None);
        assert_eq!(o.process_identity(), None);
    }

    #[test]
    fn an_agent_the_job_in_front_names_is_the_panes_process_identity() {
        // `node /path/to/bin/codex`: tmux names `node`, which names nothing; the job read names Codex.
        let mut o = observed("node", IDLE, AgentRecord::default());
        assert_eq!(o.process_identity(), None);
        o.job_agent = Some("codex".to_string());
        assert_eq!(o.process_identity().as_deref(), Some("codex"));
        assert_eq!(o.identity().as_deref(), Some("codex"));
        // The kind hint still comes first.
        o.kind_hint = Some("claude".to_string());
        assert_eq!(o.identity().as_deref(), Some("claude"));
        assert_eq!(o.process_identity().as_deref(), Some("codex"));
    }

    #[test]
    fn a_container_agents_screen_is_captured_like_any_agents() {
        let wants = |o: &Observed| wants_screen(o, &settled_for(o));
        assert!(!wants(&observed("docker", "", AgentRecord::default())));
        assert!(wants(&in_container(
            observed("docker", "", AgentRecord::default()),
            "claude"
        )));
    }

    /// Codex at rest as its bottom pane draws it: the reply, the turn's rule, the empty
    /// composer and the footer.
    const CODEX_AT_REST: &str = "• All three tests pass now.\n\n─ Worked for 1m 16s ─\n\n› Ask Codex to do anything\n\n  gpt-5.6-sol default · /work\n";
    /// The same Codex mid-turn: its status line above the composer.
    const CODEX_WORKING: &str = "• Working (12s • esc to interrupt)\n\n› Ask Codex to do anything\n\n  gpt-5.6-sol default · /work\n";

    /// An npm-installed Codex: tmux reports `node`, and nothing but the hint `taw agent
    /// start --kind codex` set says it is Codex. Its title is the project's name, which is
    /// what Codex leaves there while no spinner shows.
    fn hinted_codex(screen: &str, prev: AgentRecord) -> Observed {
        let mut o = observed("node", screen, prev);
        o.kind_hint = Some("codex".to_string());
        o.title = "project".to_string();
        o
    }

    /// The title a Codex's pane can have at rest, and the rule that reads the pane idle under
    /// it. A title that is not a spinner is herdr's `osc_title_idle`, whatever is on screen.
    /// No title, as it is for the first reads after another agent handed the pane over and
    /// the watcher dropped the title that agent left, is only taw's rule on the composer.
    const CODEX_TITLES: [(&str, &str); 2] =
        [("project", "osc_title_idle"), ("", "taw_composer_idle")];

    #[test]
    fn a_codex_at_its_composer_reads_idle_with_the_hint_and_without_it() {
        // herdr's Codex manifest (2026.10.01.1) reads a Codex idle through its title, any
        // title that is not a spinner or "Action Required". Where the title says nothing,
        // taw adds a low-priority rule on the composer: a Codex that rests at its empty
        // composer is idle, whether tmux names its process (`codex`) or only the hint does
        // (`node`, which is what npm's Codex runs as), and never `unknown`.
        for (title, wanted_rule) in CODEX_TITLES {
            for (name, mut o) in [
                (
                    "named by its process",
                    observed(
                        "codex",
                        CODEX_AT_REST,
                        prev(Some("codex"), Some(AgentState::Unknown), false),
                    ),
                ),
                (
                    "named by the hint alone",
                    hinted_codex(
                        CODEX_AT_REST,
                        prev(Some("codex"), Some(AgentState::Unknown), false),
                    ),
                ),
            ] {
                o.title = title.to_string();
                match decide_on(&o) {
                    Decision::Transition {
                        kind,
                        from,
                        to,
                        rule,
                        ..
                    } => {
                        assert_eq!(kind, "codex", "{name}, title {title:?}");
                        assert_eq!(from, Some(AgentState::Unknown), "{name}, title {title:?}");
                        assert_eq!(to, AgentState::Idle, "{name}, title {title:?}");
                        assert_eq!(
                            rule.as_deref(),
                            Some(wanted_rule),
                            "{name}, title {title:?}"
                        );
                    }
                    other => panic!(
                        "{name}, title {title:?}: a Codex at its composer is idle, got {other:?}"
                    ),
                }
            }
        }
    }

    #[test]
    fn a_hint_only_codex_stays_idle_at_its_composer() {
        // Read again while it rests, it changes nothing: step 5 no longer finds it without
        // a rule and sends it to `unknown`.
        for (title, wanted_rule) in CODEX_TITLES {
            let mut o = hinted_codex(
                CODEX_AT_REST,
                prev_with_rule(Some("codex"), Some(AgentState::Idle), Some(wanted_rule)),
            );
            o.title = title.to_string();
            assert_eq!(decide_on(&o), Decision::Keep, "title {title:?}");
        }
    }

    #[test]
    fn a_hint_only_codex_changes_rule_when_its_title_comes_or_goes() {
        // The state is the same under either rule, so only the rule moves: herdr's title
        // rule wins the tie once Codex has set a title, and taw's composer rule is the
        // verdict again when the title is dropped.
        let titled = hinted_codex(
            CODEX_AT_REST,
            prev_with_rule(
                Some("codex"),
                Some(AgentState::Idle),
                Some("taw_composer_idle"),
            ),
        );
        assert_eq!(
            decide_on(&titled),
            Decision::RuleChanged {
                rule: Some("osc_title_idle".to_string())
            }
        );
        let mut cleared = hinted_codex(
            CODEX_AT_REST,
            prev_with_rule(
                Some("codex"),
                Some(AgentState::Idle),
                Some("osc_title_idle"),
            ),
        );
        cleared.title = String::new();
        assert_eq!(
            decide_on(&cleared),
            Decision::RuleChanged {
                rule: Some("taw_composer_idle".to_string())
            }
        );
    }

    #[test]
    fn a_hint_only_codex_ends_a_turn_at_once_when_its_composer_returns() {
        // The composer is a visible idle, as Claude's prompt box is, and so is herdr's
        // title: the end of the turn is written on the read that sees it, and behind the
        // user's back it is `done`.
        for (title, wanted_rule) in CODEX_TITLES {
            let mut arb = Arbiter::settled("codex");
            let working = prev_with_rule(
                Some("codex"),
                Some(AgentState::Working),
                Some("screen_working_fallback"),
            );
            let mut mid_turn = hinted_codex(CODEX_WORKING, working.clone());
            mid_turn.title = title.to_string();
            assert_eq!(
                decide_at(&mid_turn, Instant::now(), 0, &mut arb),
                Decision::Keep,
                "the status line is still there: still working, title {title:?}"
            );
            let mut finished = hinted_codex(CODEX_AT_REST, working);
            finished.title = title.to_string();
            match decide_at(&finished, Instant::now(), 100, &mut arb) {
                Decision::Transition { from, to, rule, .. } => {
                    assert_eq!(from, Some(AgentState::Working), "title {title:?}");
                    assert_eq!(to, AgentState::Done, "title {title:?}");
                    assert_eq!(rule.as_deref(), Some(wanted_rule), "title {title:?}");
                }
                other => panic!("a visible idle is not held, title {title:?}: got {other:?}"),
            }
            assert!(!arb.hold_active(), "title {title:?}");
        }
    }

    #[test]
    fn a_hint_only_codex_with_no_composer_on_screen_is_still_unknown() {
        // The rule is evidence that Codex is there, which the hint alone is not: a shell
        // prompt where Codex was is still a hint with nothing behind it.
        let mut o = hinted_codex("$ \n", prev(Some("codex"), Some(AgentState::Idle), true));
        o.title = String::new();
        match decide_on(&o) {
            Decision::Transition { kind, to, rule, .. } => {
                assert_eq!(kind, "codex");
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None, "no rule can explain a state nothing showed");
            }
            other => panic!("expected a transition to unknown, got {other:?}"),
        }
    }

    #[test]
    fn a_hint_only_codex_with_a_title_needs_no_composer_to_read_idle() {
        // herdr's title rule takes any title that is not a spinner as Codex's own, with no
        // composer on screen, as Claude's `✳` title is taken. Only the exit of the agent's
        // process settles a Codex that has left (the next test), not what its pane draws.
        let o = hinted_codex("$ \n", prev(Some("codex"), Some(AgentState::Idle), true));
        assert_eq!(
            decide_on(&o),
            Decision::RuleChanged {
                rule: Some("osc_title_idle".to_string())
            }
        );
    }

    #[test]
    fn a_hint_only_codex_that_has_exited_is_unknown_though_its_composer_is_still_drawn() {
        // The dead pid settles what its last screen cannot: the composer of an agent that
        // has left is stale, and neither idle rule may bring it back as an idle Codex, the
        // title's included, which the shell leaves up.
        for (title, wanted_rule) in CODEX_TITLES {
            let mut o = hinted_codex(
                CODEX_AT_REST,
                prev_with_rule(Some("codex"), Some(AgentState::Idle), Some(wanted_rule)),
            );
            o.title = title.to_string();
            assert_eq!(
                decide_on(&o),
                Decision::Keep,
                "precondition: while it is alive that screen is an idle Codex, title {title:?}"
            );
            match decide(
                &bundled_only(),
                &mut HashMap::new(),
                &o,
                true,
                Instant::now(),
                &mut Arbiter::settled("codex"),
            ) {
                Decision::Transition { kind, to, rule, .. } => {
                    assert_eq!(kind, "codex", "the hint's identity is kept into unknown");
                    assert_eq!(to, AgentState::Unknown, "title {title:?}");
                    assert_eq!(rule, None, "no rule can explain a stale screen");
                }
                other => panic!(
                    "an exited hint-only Codex must go unknown, title {title:?}: got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn a_process_identified_agent_on_a_blank_screen_is_not_downgraded_to_unknown() {
        // Identity from the *command* means the agent really is running, whatever its
        // screen happens to show this instant. Only a hint with nothing behind it is
        // unknown; a running claude that has drawn nothing yet is idle, as before.
        let mut o = observed("claude", "\n\n", AgentRecord::default());
        o.title = String::new();
        match decide_on(&o) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule, None);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_process_identified_agent_that_exits_mid_turn_writes_its_idle_before_the_clear() {
        // herdr's `ReportProcessExit`: the shell back in the foreground while the agent
        // was working first publishes the turn's end — `done` here, in the background —
        // "so notifications and wait-agent callers observe completion before the pane
        // becomes unknown". The next read clears the record, and with no hint nothing
        // claims the pane any more, which is why this is not `unknown`.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let o = observed(
            "zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        match decide_at(&o, base, 0, &mut arb) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, Some(AgentState::Working));
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("the exit's idle comes first, got {other:?}"),
        }
        assert_eq!(
            arb.forced_read(),
            Some(Forced::NextTick),
            "the clear is owed the next tick, output or not"
        );
        let after = observed(
            "zsh",
            "$ \n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Done),
                Some("process_exited"),
            ),
        );
        assert_eq!(
            decide_at(&after, base, 300, &mut arb),
            Decision::NoAgent { clear: true }
        );
        assert_eq!(arb.forced_read(), None);
    }

    #[test]
    fn a_blocked_agent_that_exits_writes_its_completion_before_the_clear() {
        let mut arb = Arbiter::settled("claude");
        let o = observed(
            "-zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Blocked), false),
        );
        match decide_at(&o, Instant::now(), 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                // `idle` after `blocked` is a completion (herdr), so behind the user's
                // back the exit is `done`, as the end of a turn would be.
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("a login shell is a shell too, got {other:?}"),
        }
    }

    #[test]
    fn an_idle_agent_that_exits_is_cleared_at_once() {
        // Nothing was in progress, so there is no end to report first.
        for state in [AgentState::Idle, AgentState::Done, AgentState::Unknown] {
            let o = observed("bash", "$ \n", prev(Some("claude"), Some(state), true));
            assert_eq!(
                decide_at(&o, Instant::now(), 0, &mut Arbiter::settled("claude")),
                Decision::NoAgent { clear: true },
                "from {state}"
            );
        }
    }

    #[test]
    fn an_agent_back_before_the_clear_is_kept() {
        // The exit's idle is written; the next read finds the agent in the foreground
        // again, so there is nothing to clear and no clear is owed any more.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let exited = observed(
            "zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Working), true),
        );
        let _ = decide_at(&exited, base, 0, &mut arb);
        let back = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("process_exited"),
            ),
        );
        assert_eq!(
            decide_at(&back, base, 300, &mut arb),
            Decision::RuleChanged {
                rule: Some("live_prompt_box".to_string())
            }
        );
        assert_eq!(arb.forced_read(), None);
    }

    #[test]
    fn a_hook_reported_pane_keeps_the_manifest_from_writing_state() {
        let mut record = prev(Some("claude"), Some(AgentState::Working), false);
        record.source = Some("hook:claude-code".to_string());
        // The screen says idle; the hook says working. The hook wins.
        let o = observed("claude", IDLE, record);
        assert_eq!(
            decide_on(&o),
            Decision::HookAuthoritative {
                kind: Some("claude".to_string())
            }
        );
    }

    #[test]
    fn a_hook_reported_pane_is_never_cleared_for_want_of_a_recognisable_command() {
        // The whole point of a hook is that the agent reports itself. `node`, a wrapper
        // script, an editor helper — none of them are in the identity table, and none of
        // them may cost the pane the state its own hook wrote.
        for command in ["node", "python3", "my-agent-wrapper", ""] {
            let mut record = prev(Some("cline"), Some(AgentState::Working), false);
            record.source = Some("hook:cline".to_string());
            let o = observed(command, "$ \n", record);
            assert_eq!(
                decide_on(&o),
                Decision::HookAuthoritative { kind: None },
                "command {command:?} must not clear a hook-reported pane"
            );
        }
    }

    #[test]
    fn a_released_hook_source_goes_back_to_being_clearable() {
        // `pane release-agent` resets `@agent_source`; only then may the pane be cleared,
        // and by the rules every other pane follows. The shell back: cleared at once.
        let mut record = prev(Some("cline"), Some(AgentState::Idle), false);
        record.source = Some("manifest".to_string());
        assert_eq!(
            decide_on(&observed("sh", "$ \n", record.clone())),
            Decision::NoAgent { clear: true }
        );
        // Another process in front of it (`node`, the command a hook-reported agent often
        // runs under): one miss per identity recheck, and the record goes on the sixth.
        let base = Instant::now();
        let mut arb = Arbiter::settled("cline");
        let o = observed("node", "$ \n", record);
        for miss in 0..5 {
            assert_ne!(
                decide_at(&o, base, miss * 5000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {} must not clear the pane",
                miss + 1
            );
        }
        assert_eq!(
            decide_at(&o, base, 25_000, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    /// A pane whose state `kind`'s own hook reports (`@agent_source` is `hook:<kind>`).
    fn hooked(kind: &str, state: AgentState) -> AgentRecord {
        let mut record = prev(Some(kind), Some(state), false);
        record.source = Some(format!("hook:{kind}"));
        record
    }

    /// `o` with the pane's own shell said to be idle, as the watcher's `ps` says it.
    fn at_the_panes_prompt(mut o: Observed) -> Observed {
        o.pane_shell_idle = Some(true);
        o
    }

    #[test]
    fn a_hook_reported_agent_is_released_when_its_shell_is_back() {
        // herdr's `ReportedAgentShellReturned`: an agent that reports for itself and that
        // taw cannot identify is not judged by the process in front of it, so the pane's own
        // shell coming back to its prompt is the one sign that it has exited, whatever tmux
        // names the command. From any state that is not a turn in progress the record goes at
        // once, hook source and all.
        for command in ["zsh", "-zsh", "bash", "sh", "/bin/fish", "python3", ""] {
            for state in [AgentState::Idle, AgentState::Done, AgentState::Unknown] {
                let o = at_the_panes_prompt(observed(command, "$ \n", hooked("kimi", state)));
                assert_eq!(
                    decide_on(&o),
                    Decision::NoAgent { clear: true },
                    "{command} after {state}"
                );
            }
        }
    }

    #[test]
    fn a_hook_reported_turn_the_shell_cuts_short_writes_its_end_before_the_release() {
        // As for any exit (herdr's `ReportProcessExit`): the turn's end is written first, so
        // that a notification and a `wait` see the completion before the pane becomes empty.
        // Written by the watcher, that idle names `manifest` as the source, which is what
        // hands the pane back from its hook, and the next read, owed whether or not
        // anything printed, clears it.
        for from in [AgentState::Working, AgentState::Blocked] {
            let base = Instant::now();
            let mut arb = Arbiter::default();
            let o = at_the_panes_prompt(observed("zsh", "$ \n", hooked("kimi", from)));
            match decide_at(&o, base, 0, &mut arb) {
                Decision::Transition {
                    kind,
                    from: was,
                    to,
                    rule,
                    ..
                } => {
                    assert_eq!(kind, "kimi");
                    assert_eq!(was, Some(from));
                    assert_eq!(to, AgentState::Done, "in the background, as any finish is");
                    assert_eq!(rule.as_deref(), Some("process_exited"));
                }
                other => panic!("from {from}: the end comes first, got {other:?}"),
            }
            assert_eq!(arb.forced_read(), Some(Forced::NextTick));

            let mut written = hooked("kimi", AgentState::Done);
            written.source = Some("manifest".to_string());
            written.rule = Some("process_exited".to_string());
            let after = observed("zsh", "$ \n", written);
            assert_eq!(
                decide_at(&after, base, 300, &mut arb),
                Decision::NoAgent { clear: true }
            );
            assert_eq!(arb.forced_read(), None);
        }
    }

    #[test]
    fn a_hook_reported_agent_a_shell_wrapper_runs_is_kept_while_the_job_names_it() {
        // `bash ~/bin/kimi-wrap`, which does not replace itself: tmux names the pane `bash` for as
        // long as the agent runs, and the job read names the agent behind it. That shell is not the
        // shell coming back, so the record stands.
        for state in [AgentState::Idle, AgentState::Working, AgentState::Done] {
            let mut o = observed("bash", "$ \n", hooked("kimi", state));
            o.pane_shell_idle = Some(false);
            o.job_agent = Some("kimi".to_string());
            let mut arb = Arbiter::default();
            let base = Instant::now();
            for ms in [0, 1_000, 2_000] {
                let decision = decide_at(&o, base, ms, &mut arb);
                assert!(
                    matches!(decision, Decision::HookAuthoritative { .. }),
                    "{state} at {ms} ms: {decision:?}"
                );
            }
            // Its wrapper gone and the pane's own shell back, the record goes.
            o.job_agent = None;
            o.pane_shell_idle = Some(true);
            assert!(
                !matches!(
                    decide_at(&o, base, 3_000, &mut arb),
                    Decision::HookAuthoritative { .. }
                ),
                "{state}: the shell back did not end the record"
            );
        }
    }

    #[test]
    fn only_a_hook_owned_pane_asks_again_when_its_shell_was_at_the_prompt() {
        let at = Instant::now();
        // The pane's shell leads, at its prompt: the first look asks, and no job is read.
        let at_prompt = |source: Option<&str>| {
            let mut probe = super::super::job::Probe::new(Duration::from_secs(5));
            let named = probe.agent("bash", at, || Some((100, 100)), |_| None, |_, _| true);
            assert_eq!(named, None);
            let record = AgentRecord {
                source: source.map(str::to_string),
                ..AgentRecord::default()
            };
            let o = observed("bash", "$ \n", record);
            forget_prompt_of_hook_owned(&mut probe, &o);
            // A look a second on: whether the leader is asked again.
            let mut asked = 0;
            probe.agent(
                "bash",
                at + Duration::from_secs(1),
                || {
                    asked += 1;
                    Some((100, 100))
                },
                |_| None,
                |_, _| true,
            );
            asked
        };
        assert_eq!(at_prompt(Some("hook:kimi")), 1, "a hook-owned pane");
        assert_eq!(at_prompt(None), 0, "a pane with no record");
        assert_eq!(
            at_prompt(Some("detected")),
            0,
            "a pane the watcher detected"
        );
    }

    #[test]
    fn a_hook_owned_pane_that_records_no_agent_is_released_with_its_shell() {
        // Only a source is left of it, which nothing else would ever clear.
        let record = AgentRecord {
            source: Some("hook:kimi".to_string()),
            ..AgentRecord::default()
        };
        let o = at_the_panes_prompt(observed("zsh", "$ \n", record));
        assert_eq!(decide_on(&o), Decision::NoAgent { clear: true });
    }

    // --- an agent that reports its own state is announced as any other ---------------------------
    //
    // A report is written to tmux by the report itself, so a read finds the new state already on the
    // pane and `decide` tells a change from what the last read took in (`Arbiter::taken`).

    /// A pane whose agent (a `kimi`) reports its own state, with a program in front of it that no name
    /// in the identity table matches, as the pane of an agent run by a wrapper has: `state` is what
    /// the record holds, `seen` is `@agent_seen`, and the user is in front of the pane or not.
    fn reported_pane(state: AgentState, seen: bool, looking: bool) -> Observed {
        let mut record = hooked("kimi", state);
        record.seen = seen;
        let mut o = observed("python3", "$ \n", record);
        o.window_active = looking;
        o.session_attached = u32::from(looking);
        o
    }

    /// `decide` on `o` at `ms`, as the watcher that has `arb` does.
    fn read(arb: &mut Arbiter, base: Instant, ms: u64, o: &Observed) -> Decision {
        decide_at(o, base, ms, arb)
    }

    /// A watcher that has read a pane its agent reports `state` for once, and taken that in.
    fn watching(state: AgentState) -> (Arbiter, Instant) {
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let first = read(&mut arb, base, 0, &reported_pane(state, false, false));
        assert_eq!(first, Decision::HookAuthoritative { kind: None });
        assert_eq!(arb.taken(), Taken::left(state, Some("kimi")));
        (arb, base)
    }

    fn reported(
        from: Option<AgentState>,
        to: AgentState,
        reported: AgentState,
        seen: bool,
    ) -> Decision {
        Decision::Reported {
            kind: "kimi".to_string(),
            from,
            via: None,
            to,
            reported,
            replaced: None,
            detail: "\u{2733} fake task".to_string(),
            seen,
            ready: false,
        }
    }

    #[test]
    fn the_first_read_of_a_pane_its_agent_reports_for_takes_the_record_as_it_finds_it() {
        // As a watcher restarting on a pane it already classified gets no startup grace: what it
        // finds was announced by the watcher before it, if by any, and is no change of its own.
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
            AgentState::Done,
            AgentState::Unknown,
        ] {
            let mut arb = Arbiter::default();
            assert_eq!(arb.taken(), Taken::Unread);
            let o = reported_pane(state, false, false);
            assert_eq!(
                read(&mut arb, Instant::now(), 0, &o),
                Decision::HookAuthoritative { kind: None },
                "{state}"
            );
            assert_eq!(arb.taken(), Taken::left(state, Some("kimi")), "{state}");
        }
    }

    #[test]
    fn a_report_that_ends_a_turn_behind_the_users_back_is_a_completion_written_as_done() {
        let (mut arb, base) = watching(AgentState::Working);
        let idle = reported_pane(AgentState::Idle, false, false);
        let decision = read(&mut arb, base, 300, &idle);
        // `done` is the watcher's own word for it: written over the report, which said `idle`.
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Working),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
        let update = update(&idle.prev, &decision);
        assert_eq!(update.complete, Some((AgentState::Idle, AgentState::Done)));
        assert_eq!(
            update.state, None,
            "the report's own state is not rewritten"
        );
        // A new turn starts unseen and so does a `done`: what `@agent_seen` says is what a
        // completion decides, and the glance earlier in the turn does not count.
        assert_eq!(update.seen, Some(false));
        // The finish is news when the record is gone by the time its delay is up: an agent that
        // reports for itself often ends with its last report, as `opencode run` does.
        assert_eq!(
            update.notice,
            Some(Notice {
                class: Class::Done,
                state: AgentState::Done,
                exit: true
            })
        );
        // The next read finds the `done` the watcher wrote: nothing new, and the same again.
        let done = reported_pane(AgentState::Done, false, false);
        for ms in [600, 900] {
            assert_eq!(
                read(&mut arb, base, ms, &done),
                Decision::HookAuthoritative { kind: None }
            );
        }
        assert_eq!(arb.taken(), Taken::left(AgentState::Done, Some("kimi")));
    }

    #[test]
    fn a_report_that_ends_a_turn_in_front_of_the_user_is_an_idle_they_saw() {
        let (mut arb, base) = watching(AgentState::Working);
        let idle = reported_pane(AgentState::Idle, false, true);
        let decision = read(&mut arb, base, 300, &idle);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Working),
                AgentState::Idle,
                AgentState::Idle,
                true
            )
        );
        let update = update(&idle.prev, &decision);
        assert_eq!(update.complete, None, "nothing to write over the report");
        assert_eq!(update.seen, Some(true), "@agent_seen = 1");
        assert_eq!(update.notice, None, "a finish they watched is not news");
    }

    #[test]
    fn a_blocked_agent_back_at_its_prompt_while_the_user_is_away_is_done_too() {
        let (mut arb, base) = watching(AgentState::Blocked);
        let idle = reported_pane(AgentState::Idle, false, false);
        let decision = read(&mut arb, base, 300, &idle);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Blocked),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
        assert_eq!(
            update(&idle.prev, &decision)
                .notice
                .map(|notice| notice.class),
            Some(Class::Done)
        );
    }

    #[test]
    fn a_report_of_working_or_blocked_is_announced_where_it_is_written() {
        let (mut arb, base) = watching(AgentState::Idle);
        let working = reported_pane(AgentState::Working, true, false);
        let decision = read(&mut arb, base, 300, &working);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Idle),
                AgentState::Working,
                AgentState::Working,
                false
            )
        );
        let writes = update(&working.prev, &decision);
        assert_eq!(writes.complete, None);
        assert_eq!(writes.seen, Some(false), "a new turn starts unseen");
        assert_eq!(writes.notice, None, "a turn starting is no toast");

        // Blocked is a request, whoever is looking when its delay is up.
        let blocked = reported_pane(AgentState::Blocked, false, false);
        let decision = read(&mut arb, base, 600, &blocked);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Working),
                AgentState::Blocked,
                AgentState::Blocked,
                false
            )
        );
        // A request is moot once the agent is gone, so it does not outlive the record.
        assert_eq!(
            update(&blocked.prev, &decision).notice,
            Some(Notice {
                class: Class::Request,
                state: AgentState::Blocked,
                exit: false
            })
        );
    }

    #[test]
    fn the_same_report_again_is_announced_nothing() {
        // OpenCode's plugin reports `working` on every tool it runs, `idle` from two of its events,
        // and `blocked` for each permission it asks: a repeat is not a change, whatever state it is
        // and whoever is looking, and a report of the state the pane holds fires nothing.
        for state in [
            AgentState::Working,
            AgentState::Blocked,
            AgentState::Idle,
            AgentState::Unknown,
        ] {
            for looking in [false, true] {
                let (mut arb, base) = watching(state);
                let again = reported_pane(state, false, looking);
                for ms in [300, 600, 900] {
                    assert_eq!(
                        read(&mut arb, base, ms, &again),
                        Decision::HookAuthoritative { kind: None },
                        "{state}, looking {looking}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_idle_reported_again_over_a_done_nobody_has_seen_leaves_it_done() {
        // `session.status idle` and `session.idle` both report `idle`: the second lands on the `done`
        // the watcher wrote for the first, and herdr's unseen idle stays unseen. The watcher puts
        // the `done` back, and announces nothing: it is what it announced already.
        let (mut arb, base) = watching(AgentState::Working);
        let idle = reported_pane(AgentState::Idle, false, false);
        read(&mut arb, base, 300, &idle);
        let again = reported_pane(AgentState::Idle, false, false);
        let decision = read(&mut arb, base, 600, &again);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Done),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
        let update = update(&again.prev, &decision);
        assert_eq!(update.complete, Some((AgentState::Idle, AgentState::Done)));
        assert_eq!(update.seen, None);
        assert_eq!(update.notice, None);
        // And it stays put: the pane holds `done` again.
        assert_eq!(
            read(
                &mut arb,
                base,
                900,
                &reported_pane(AgentState::Done, false, false)
            ),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn taw_seen_turning_a_done_back_into_idle_is_no_report_and_fires_nothing() {
        // `taw seen` writes `idle` and `@agent_seen = 1` over the watcher's `done`, as it does on a
        // pane the screen decides: the screen's read finds an idle that did not move. Here the
        // read finds the state other than the one it took in, and what says nobody reported it is
        // the seen mark.
        let (mut arb, base) = watching(AgentState::Working);
        read(
            &mut arb,
            base,
            300,
            &reported_pane(AgentState::Idle, false, false),
        );
        let looked = reported_pane(AgentState::Idle, true, true);
        assert_eq!(
            read(&mut arb, base, 600, &looked),
            Decision::HookAuthoritative { kind: None }
        );
        assert_eq!(arb.taken(), Taken::left(AgentState::Idle, Some("kimi")));
        // The next turn is a change from the idle they saw.
        let working = reported_pane(AgentState::Working, true, true);
        assert_eq!(
            read(&mut arb, base, 900, &working),
            reported(
                Some(AgentState::Idle),
                AgentState::Working,
                AgentState::Working,
                true
            )
        );
    }

    #[test]
    fn a_done_the_user_comes_to_is_released_by_the_next_read_and_announced_as_the_idle_they_see() {
        // As on a pane the screen decides (`a_done_the_user_comes_to_without_a_focus_hook...`):
        // the focus hook's `taw seen` landed before the `done` was written, so its mark is gone.
        // The read that finds them looking writes the idle they see, which is a change of state.
        let (mut arb, base) = watching(AgentState::Working);
        read(
            &mut arb,
            base,
            300,
            &reported_pane(AgentState::Idle, false, false),
        );
        let looking = reported_pane(AgentState::Done, false, true);
        let decision = read(&mut arb, base, 600, &looking);
        assert_eq!(
            decision,
            reported(
                Some(AgentState::Done),
                AgentState::Idle,
                AgentState::Done,
                true
            )
        );
        let update = update(&looking.prev, &decision);
        assert_eq!(update.complete, Some((AgentState::Done, AgentState::Idle)));
        assert_eq!(update.seen, Some(true));
        assert_eq!(update.notice, None);
    }

    #[test]
    fn the_first_report_on_a_pane_with_no_agent_is_a_change_from_nothing() {
        let base = Instant::now();
        let mut arb = Arbiter::default();
        // The watcher reads the pane at its prompt, and then the agent starts and reports.
        let shell = observed("zsh", "$ \n", AgentRecord::default());
        assert_eq!(
            read(&mut arb, base, 0, &shell),
            Decision::NoAgent { clear: false }
        );
        assert_eq!(arb.taken(), Taken::Nothing);
        let working = reported_pane(AgentState::Working, false, false);
        assert_eq!(
            read(&mut arb, base, 500, &working),
            reported(None, AgentState::Working, AgentState::Working, false)
        );
        // `agent_event` turns a `None` before into a change into the state.
        assert_eq!(arb.taken(), Taken::left(AgentState::Working, Some("kimi")));
    }

    #[test]
    fn a_hook_that_takes_the_pane_over_from_the_screen_changes_the_state_the_screen_left() {
        // OpenCode's process is named, so the screen decides its first reads; its plugin's first
        // report makes the pane its hook's. What that report changes is the state the screen had
        // left the pane in, which was announced as it was read.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut screen = observed("kimi", "", prev(Some("kimi"), Some(AgentState::Idle), true));
        screen.prev.source = Some("manifest".to_string());
        read(&mut arb, base, 0, &screen);
        assert_eq!(arb.taken(), Taken::left(AgentState::Idle, Some("kimi")));
        let working = reported_pane(AgentState::Working, true, false);
        assert_eq!(
            read(&mut arb, base, 300, &working),
            reported(
                Some(AgentState::Idle),
                AgentState::Working,
                AgentState::Working,
                false
            )
        );
        // The hook reporting the state the screen had said is no change.
        let mut arb = Arbiter::default();
        read(&mut arb, base, 0, &screen);
        assert_eq!(
            read(
                &mut arb,
                base,
                300,
                &reported_pane(AgentState::Idle, true, false)
            ),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn a_hook_that_gives_the_pane_back_and_reports_again_is_a_change_from_where_the_screen_left_it()
    {
        let (mut arb, base) = watching(AgentState::Idle);
        // `pane release-agent`: the source is the manifest's again, and the screen decides.
        let mut released = reported_pane(AgentState::Idle, true, false);
        released.prev.source = Some("manifest".to_string());
        read(&mut arb, base, 300, &released);
        assert_eq!(arb.taken(), Taken::left(AgentState::Idle, Some("kimi")));
        let working = reported_pane(AgentState::Working, false, false);
        assert_eq!(
            read(&mut arb, base, 600, &working),
            reported(
                Some(AgentState::Idle),
                AgentState::Working,
                AgentState::Working,
                false
            )
        );
    }

    #[test]
    fn a_hook_owned_pane_with_no_state_or_no_agent_to_announce_under_announces_nothing() {
        let base = Instant::now();
        // A source and no state: nothing to take in, and the next state is a change from nothing.
        let mut arb = Arbiter::default();
        let mut sourceless = reported_pane(AgentState::Idle, false, false);
        sourceless.prev.state = None;
        assert_eq!(
            read(&mut arb, base, 0, &sourceless),
            Decision::HookAuthoritative { kind: None }
        );
        assert_eq!(arb.taken(), Taken::Nothing);
        // A state and no kind to announce it for: taken in, and nothing said.
        let mut arb = Arbiter::default();
        let mut kindless = reported_pane(AgentState::Working, false, false);
        kindless.prev.kind = None;
        assert_eq!(
            read(&mut arb, base, 0, &kindless),
            Decision::HookAuthoritative { kind: None }
        );
        kindless.prev.state = Some(AgentState::Idle);
        assert_eq!(
            read(&mut arb, base, 300, &kindless),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn a_change_is_announced_for_the_agent_a_process_names_when_one_does() {
        // The record says `kimi` and the program in front of the pane is a `claude`: the pane's
        // process is the better witness of who is there, and it is what `HookAuthoritative` has
        // always recorded over the hook's kind, with the last agent's session going with it.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut o = reported_pane(AgentState::Working, false, false);
        o.command = "claude".to_string();
        read(&mut arb, base, 0, &o);
        o.prev.state = Some(AgentState::Idle);
        let decision = read(&mut arb, base, 300, &o);
        match &decision {
            Decision::Reported { kind, to, .. } => {
                assert_eq!(kind, "claude");
                assert_eq!(*to, AgentState::Done);
            }
            other => panic!("{other:?}"),
        }
        let update = update(&o.prev, &decision);
        assert_eq!(update.kind.as_deref(), Some("claude"));
        assert!(update.release_identity);
    }

    /// [`reported_pane`] for the agent `kind` that is not the `kimi` it makes.
    fn reported_pane_of(kind: &str, state: AgentState, seen: bool, looking: bool) -> Observed {
        let mut o = reported_pane(state, seen, looking);
        o.prev.kind = Some(kind.to_string());
        o.prev.source = Some(format!("hook:{kind}"));
        o
    }

    #[test]
    fn a_different_agent_reporting_the_state_the_pane_is_in_fires_the_status_event_and_takes_the_name()
     {
        // A pane a `kimi` works in, and a `pi` that takes it and reports `working`: the state is
        // the one the pane was announced in, and the occupant is not. As for an agent a screen read
        // finds in the pane's place, the status event says who is there (`kind_changed`), no agent
        // event and no toast follow, and the name the last agent was given goes with it.
        let (mut arb, base) = watching(AgentState::Working);
        let mut pi = reported_pane_of("pi", AgentState::Working, false, false);
        pi.prev.name = Some("reviewer".to_string());
        let decision = read(&mut arb, base, 300, &pi);
        assert_eq!(
            decision,
            Decision::Reported {
                kind: "pi".to_string(),
                from: Some(AgentState::Working),
                via: None,
                to: AgentState::Working,
                reported: AgentState::Working,
                replaced: Some("kimi".to_string()),
                detail: "\u{2733} fake task".to_string(),
                seen: false,
                ready: false,
            }
        );
        let writes = update(&pi.prev, &decision);
        assert!(writes.release_name, "the name was the last agent's");
        assert_eq!(writes.notice, None);
        assert_eq!(writes.seen, None);
        assert_eq!(writes.complete, None);
        // It is the pane's agent now, and the same report again is nothing.
        assert_eq!(arb.taken(), Taken::left(AgentState::Working, Some("pi")));
        assert_eq!(
            read(&mut arb, base, 600, &pi),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn a_different_agent_that_reports_another_state_is_a_change_and_takes_the_name_too() {
        let (mut arb, base) = watching(AgentState::Working);
        let mut pi = reported_pane_of("pi", AgentState::Idle, false, false);
        pi.prev.name = Some("reviewer".to_string());
        let decision = read(&mut arb, base, 300, &pi);
        assert_eq!(
            decision,
            Decision::Reported {
                kind: "pi".to_string(),
                from: Some(AgentState::Working),
                via: None,
                to: AgentState::Done,
                reported: AgentState::Idle,
                replaced: Some("kimi".to_string()),
                detail: "\u{2733} fake task".to_string(),
                seen: false,
                ready: false,
            }
        );
        assert!(update(&pi.prev, &decision).release_name);
    }

    #[test]
    fn the_name_stays_with_the_agent_the_pane_was_set_up_for() {
        // `@agent_kind_hint` names the agent `taw agent start` launched, and the name it writes is
        // that agent's own: a read that lands between the hint and the name must not take it.
        let (mut arb, base) = watching(AgentState::Working);
        let mut opencode = reported_pane_of("opencode", AgentState::Working, false, false);
        opencode.prev.name = Some("coder".to_string());
        opencode.prev.kind_hint = Some("opencode".to_string());
        opencode.kind_hint = Some("opencode".to_string());
        let decision = read(&mut arb, base, 300, &opencode);
        assert!(matches!(
            decision,
            Decision::Reported {
                replaced: Some(_),
                ..
            }
        ));
        assert!(!update(&opencode.prev, &decision).release_name);
    }

    #[test]
    fn a_screen_read_leaves_the_kind_it_recorded_for_a_hook_that_takes_the_pane() {
        // What a screen read leaves is the state and the agent it recorded, so an agent that reports
        // where a Claude was working is another agent, and one that says it is the same is not.
        let mut arb = Arbiter::settled("claude");
        let claude = observed("claude", WORKING, working_turn());
        assert_eq!(
            decide_at(&claude, Instant::now(), 0, &mut arb),
            Decision::Keep
        );
        assert_eq!(
            arb.taken(),
            Taken::left(AgentState::Working, Some("claude"))
        );
        let opencode = reported_pane_of("opencode", AgentState::Working, false, false);
        assert!(matches!(
            read(&mut arb, Instant::now(), 300, &opencode),
            Decision::Reported { replaced: Some(was), .. } if was == "claude"
        ));
    }

    /// [`reported_pane`] with the stamp of the agent's last report of work or a wait on it.
    fn stamped_pane(state: AgentState, seen: bool, looking: bool, stamp: &str) -> Observed {
        let mut o = reported_pane(state, seen, looking);
        o.turn = Some(stamp.to_string());
        o
    }

    /// [`reported`] for a turn that began and ended between two reads: `via` the `working` it went
    /// through.
    fn reported_via_work(
        from: Option<AgentState>,
        to: AgentState,
        reported: AgentState,
        seen: bool,
    ) -> Decision {
        Decision::Reported {
            kind: "kimi".to_string(),
            from,
            via: Some(AgentState::Working),
            to,
            reported,
            replaced: None,
            detail: "\u{2733} fake task".to_string(),
            seen,
            ready: false,
        }
    }

    #[test]
    fn a_turn_that_began_and_ended_between_two_reads_is_announced_as_its_start_and_its_end() {
        // `working` and `idle` reported back to back leave the state the watcher had taken in, and
        // move the stamp: herdr, which applies each report, announces both, and so does this.
        let (mut arb, base) = watching(AgentState::Idle);
        let idle = stamped_pane(AgentState::Idle, false, false, "100");
        let decision = read(&mut arb, base, 300, &idle);
        assert_eq!(
            decision,
            reported_via_work(
                Some(AgentState::Idle),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
        let writes = update(&idle.prev, &decision);
        // The end is a completion behind the user's back, written over the agent's `idle`.
        assert_eq!(writes.complete, Some((AgentState::Idle, AgentState::Done)));
        assert_eq!(writes.seen, Some(false));
        assert_eq!(
            writes.notice,
            Some(Notice {
                class: Class::Done,
                state: AgentState::Done,
                exit: true
            })
        );
        // Announced once: the `done` the watcher wrote, under the same stamp, is nothing new.
        let done = stamped_pane(AgentState::Done, false, false, "100");
        for ms in [600, 900] {
            assert_eq!(
                read(&mut arb, base, ms, &done),
                Decision::HookAuthoritative { kind: None }
            );
        }
        assert_eq!(
            arb.taken(),
            Taken::State {
                state: AgentState::Done,
                kind: Some("kimi".to_string()),
                turn: Some("100".to_string())
            }
        );
    }

    #[test]
    fn a_turn_that_began_and_ended_between_two_reads_in_front_of_the_user_is_an_idle_they_saw() {
        let (mut arb, base) = watching(AgentState::Idle);
        let idle = stamped_pane(AgentState::Idle, true, true, "100");
        let decision = read(&mut arb, base, 300, &idle);
        assert_eq!(
            decision,
            reported_via_work(
                Some(AgentState::Idle),
                AgentState::Idle,
                AgentState::Idle,
                true
            )
        );
        let writes = update(&idle.prev, &decision);
        assert_eq!(writes.complete, None);
        assert_eq!(writes.seen, Some(true), "@agent_seen = 1");
        assert_eq!(writes.notice, None, "a finish they watched is not news");
    }

    #[test]
    fn a_turn_under_way_is_not_a_missed_one_however_often_it_reports_work() {
        // OpenCode reports `working` for every tool it runs: the stamp moves all the turn long, and
        // the state the watcher has taken in is `working` throughout. Only a turn that is not under
        // way at the last read can have begun and ended since.
        let (mut arb, base) = watching(AgentState::Working);
        for (ms, stamp) in [(300, "1"), (600, "2"), (900, "3")] {
            let working = stamped_pane(AgentState::Working, false, false, stamp);
            assert_eq!(
                read(&mut arb, base, ms, &working),
                Decision::HookAuthoritative { kind: None },
                "stamp {stamp}"
            );
        }
        // And its end is the ordinary completion, with no start to announce before it.
        let idle = stamped_pane(AgentState::Idle, false, false, "3");
        assert_eq!(
            read(&mut arb, base, 1200, &idle),
            reported(
                Some(AgentState::Working),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
    }

    #[test]
    fn a_short_turn_after_a_done_nobody_has_seen_is_a_new_turn_and_its_end() {
        // The agent finished (`done`, announced) and, before the next read, worked and finished
        // again: the `idle` it reported last lands on the `done`, and the stamp says there was work.
        let (mut arb, base) = watching(AgentState::Working);
        let first = stamped_pane(AgentState::Idle, false, false, "1");
        read(&mut arb, base, 300, &first);
        let again = stamped_pane(AgentState::Idle, false, false, "2");
        let decision = read(&mut arb, base, 600, &again);
        assert_eq!(
            decision,
            reported_via_work(
                Some(AgentState::Done),
                AgentState::Done,
                AgentState::Idle,
                false
            )
        );
        let writes = update(&again.prev, &decision);
        assert_eq!(writes.complete, Some((AgentState::Idle, AgentState::Done)));
        assert_eq!(
            writes.notice.map(|notice| notice.class),
            Some(Class::Done),
            "a finish behind the user's back is news each time"
        );
    }

    #[test]
    fn with_nothing_taken_in_a_stamp_is_no_missed_turn_and_the_first_read_takes_it_as_found() {
        let base = Instant::now();
        // The watcher read a pane with no agent, and then the agent reported `idle` after work.
        let mut arb = Arbiter::default();
        read(
            &mut arb,
            base,
            0,
            &observed("zsh", "$ \n", AgentRecord::default()),
        );
        assert_eq!(
            read(
                &mut arb,
                base,
                300,
                &stamped_pane(AgentState::Idle, false, false, "9")
            ),
            reported(None, AgentState::Idle, AgentState::Idle, false)
        );
        // A watcher's first read takes the stamp with the state, as found.
        let mut arb = Arbiter::default();
        let idle = stamped_pane(AgentState::Idle, false, false, "9");
        assert_eq!(
            read(&mut arb, base, 0, &idle),
            Decision::HookAuthoritative { kind: None }
        );
        assert_eq!(
            read(&mut arb, base, 300, &idle),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn a_stamp_a_screen_read_saw_is_no_turn_for_the_hook_that_takes_the_pane_over() {
        // A pane the screen decided is read with the stamp an earlier hook left on it, and the read
        // takes that in: the hook reporting the same state under the same stamp has done nothing.
        let mut arb = Arbiter::default();
        let mut screen = stamped_pane(AgentState::Idle, true, false, "5");
        screen.prev.source = Some("manifest".to_string());
        read(&mut arb, Instant::now(), 0, &screen);
        assert_eq!(
            arb.taken(),
            Taken::State {
                state: AgentState::Idle,
                kind: Some("kimi".to_string()),
                turn: Some("5".to_string())
            }
        );
        let hook = stamped_pane(AgentState::Idle, true, false, "5");
        assert_eq!(
            read(&mut arb, Instant::now(), 300, &hook),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn a_change_whose_write_failed_is_found_again_by_the_read_that_retries_it() {
        let (mut arb, base) = watching(AgentState::Working);
        let before = arb.taken();
        let idle = reported_pane(AgentState::Idle, false, false);
        assert!(matches!(
            read(&mut arb, base, 300, &idle),
            Decision::Reported { .. }
        ));
        // The watcher could not write it, and puts back what it had taken in.
        arb.retake(before);
        assert!(matches!(
            read(&mut arb, base, 600, &idle),
            Decision::Reported { .. }
        ));
    }

    #[test]
    fn a_turn_a_report_ended_that_the_shell_returning_finds_unannounced_is_the_turns_end() {
        // `opencode run`: the last thing the plugin reports is `idle`, and the process exits at
        // once, so the read that finds the report finds the pane's shell back too. The turn was
        // announced as started and never as ended, and its end is owed the same announcement as any
        // exit mid-turn: written first, `done` in the background, before the record goes.
        for (looking, end) in [(false, AgentState::Done), (true, AgentState::Idle)] {
            let (mut arb, base) = watching(AgentState::Working);
            let mut idle = reported_pane(AgentState::Idle, false, looking);
            idle.pane_shell_idle = Some(true);
            match read(&mut arb, base, 300, &idle) {
                Decision::Transition {
                    kind,
                    from,
                    to,
                    rule,
                    seen,
                    ..
                } => {
                    assert_eq!(kind, "kimi");
                    assert_eq!(from, Some(AgentState::Working), "what was announced last");
                    assert_eq!(to, end);
                    assert_eq!(rule.as_deref(), Some("process_exited"));
                    assert_eq!(seen, looking);
                }
                other => panic!("looking {looking}: {other:?}"),
            }
            assert_eq!(arb.forced_read(), Some(Forced::NextTick));
        }
    }

    #[test]
    fn an_end_the_watcher_announced_already_leaves_only_the_clear_when_the_shell_returns() {
        for state in [AgentState::Idle, AgentState::Done] {
            let (mut arb, base) = watching(state);
            let mut back = reported_pane(state, false, false);
            back.pane_shell_idle = Some(true);
            assert_eq!(
                read(&mut arb, base, 300, &back),
                Decision::NoAgent { clear: true },
                "{state}"
            );
        }
        // A turn announced as ended by a report, the `done` written, and then the shell.
        let (mut arb, base) = watching(AgentState::Working);
        read(
            &mut arb,
            base,
            300,
            &reported_pane(AgentState::Idle, false, false),
        );
        let mut back = reported_pane(AgentState::Done, false, false);
        back.pane_shell_idle = Some(true);
        assert_eq!(
            read(&mut arb, base, 600, &back),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn a_shell_that_is_not_the_panes_own_does_not_release_a_hook_reported_agent() {
        // A script that runs the agent is a shell as tmux names it (its interpreter), and so
        // is a shell started inside the pane: neither is the pane's shell back at its prompt,
        // and the agent is still running behind it. Nor is an answer that could not be had.
        for command in ["bash", "sh", "-zsh"] {
            for idle in [Some(false), None] {
                let mut o = observed(command, "$ \n", hooked("kimi", AgentState::Working));
                o.pane_shell_idle = idle;
                assert_eq!(
                    decide_on(&o),
                    Decision::HookAuthoritative { kind: None },
                    "{command} with the shell idle {idle:?}"
                );
            }
        }
        // Once the pane's own shell is idle, it is back.
        let o = at_the_panes_prompt(observed("bash", "$ \n", hooked("kimi", AgentState::Idle)));
        assert_eq!(decide_on(&o), Decision::NoAgent { clear: true });
    }

    #[test]
    fn only_the_answer_about_the_panes_shell_ends_an_agent_no_process_names() {
        // Whatever tmux names the command: a shell it names is not the pane's, and the pane's
        // shell can be idle under a name that is not a shell's at all (the process table says
        // so, and it is asked by pid). herdr asks it whatever the command is.
        for command in ["bash", "node", "python3", "vim", ""] {
            let unasked = observed(command, "$ \n", hooked("kimi", AgentState::Idle));
            assert_eq!(
                decide_on(&unasked),
                Decision::HookAuthoritative { kind: None },
                "{command:?}"
            );
            assert_eq!(
                decide_on(&at_the_panes_prompt(unasked)),
                Decision::NoAgent { clear: true },
                "{command:?}"
            );
        }
    }

    #[test]
    fn a_shell_taw_does_not_know_is_not_taken_for_an_idle_one() {
        // The list of shells is what says a process is a shell (`SHELLS`): the watcher asks the
        // process table, which answers `Some(false)` for a name that is not on it, so a pane
        // whose own shell is another one keeps a hook-reported agent until `pane
        // release-agent` says otherwise.
        assert!(SHELLS.contains(&"xonsh") && SHELLS.contains(&"elvish"));
        for name in ["python3", "node", "vim", "odd-shell", ""] {
            assert!(!is_shell(name), "{name:?}");
        }
        for name in [
            "sh",
            "bash",
            "dash",
            "zsh",
            "fish",
            "ksh",
            "mksh",
            "csh",
            "tcsh",
            "elvish",
            "xonsh",
            "nu",
            "pwsh",
            "powershell",
            "cmd",
            "-zsh",
            "/bin/bash",
            "C:\\Windows\\cmd.exe",
            "PowerShell.exe",
            "/usr/local/bin/-fish",
        ] {
            assert!(is_shell(name), "{name:?}");
        }
    }

    #[test]
    fn the_pane_is_asked_about_only_for_an_agent_that_reports_for_itself_and_no_process_names() {
        let base = Instant::now();
        let asks = |o: &Observed| Arbiter::default().wants_shell_probe(o, base);
        let owned = |command, record| observed(command, "", record);
        // A state a hook reports, or a kind its hook named: the agent says it is there, and
        // whatever tmux names the command, the pane's shell is what would end it.
        for command in ["zsh", "-bash", "node", "python3", ""] {
            assert!(
                asks(&owned(command, hooked("kimi", AgentState::Idle))),
                "{command:?}"
            );
        }
        let mut named = prev(Some("letta"), Some(AgentState::Idle), true);
        named.source = Some("manifest".to_string());
        named.reported_kind = Some("letta".to_string());
        assert!(asks(&owned("node", named.clone())));
        // Not once a process names the agent: its command is that, and it follows the
        // ordinary rules.
        assert!(!asks(&owned("kimi", hooked("kimi", AgentState::Idle))));
        assert!(!asks(&owned("letta", named.clone())));
        // Not once another agent is recorded over the one that named itself.
        named.kind = Some("claude".to_string());
        assert!(!asks(&owned("node", named)));
        // Nor for a pane nothing reports for, which is judged by its command alone.
        let mut manifest = prev(Some("claude"), Some(AgentState::Idle), true);
        manifest.source = Some("manifest".to_string());
        assert!(!asks(&owned("node", manifest)));
        assert!(!asks(&owned(
            "zsh",
            prev(Some("claude"), Some(AgentState::Idle), true)
        )));
        // Nor one that has been named at an earlier read, whatever is in front now.
        let mut arb = Arbiter::default();
        let record = hooked("kimi", AgentState::Idle);
        let _ = decide_at(&observed("kimi", "", record.clone()), base, 0, &mut arb);
        assert!(!arb.wants_shell_probe(&observed("node", "", record), base));
    }

    #[test]
    fn the_pane_is_asked_at_most_once_a_second_and_owed_a_read_when_it_may_be_asked_again() {
        // herdr's `SELF_REPORTED_AGENT_SHELL_RECHECK`: a question held back is not dropped, or
        // the shell's return, which prints a prompt and then nothing, would wait for the next
        // recheck of a quiet pane: 5 s. The read that is owed is on the first tick when the
        // second is up.
        let base = Instant::now();
        let ms = |ms| base + Duration::from_millis(ms);
        let mut arb = Arbiter::default();
        let o = observed("node", "", hooked("kimi", AgentState::Idle));
        assert!(arb.wants_shell_probe(&o, ms(0)));
        assert_eq!(arb.forced_read(), None, "asked, nothing held back");
        assert!(!arb.wants_shell_probe(&o, ms(300)));
        assert_eq!(arb.forced_read(), Some(Forced::From(ms(1000))));
        assert!(!arb.wants_shell_probe(&o, ms(999)));
        assert_eq!(arb.forced_read(), Some(Forced::From(ms(1000))));
        assert!(arb.wants_shell_probe(&o, ms(1000)));
        assert_eq!(arb.forced_read(), None, "owed no more once it is asked");
        assert!(
            arb.wants_shell_probe(&o, ms(2500)),
            "a quiet pane, a second on"
        );
        // The pane stops being one that is asked about: nothing is owed, and the limit starts
        // over with the next.
        assert!(!arb.wants_shell_probe(&o, ms(2600)));
        assert!(arb.forced_read().is_some());
        let plain = observed(
            "node",
            "",
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        assert!(!arb.wants_shell_probe(&plain, ms(2700)));
        assert_eq!(arb.forced_read(), None);
        assert!(arb.wants_shell_probe(&o, ms(2800)));
    }

    #[test]
    fn a_shell_that_is_not_the_panes_own_does_not_end_an_agent_that_named_itself() {
        // The same rule for the agent whose hook only named it (`report-agent-session`): a
        // script that runs it is not its shell coming back, and its session id stays.
        let mut record = prev(Some("letta"), Some(AgentState::Idle), true);
        record.source = Some("manifest".to_string());
        record.reported_kind = Some("letta".to_string());
        let mut o = observed("bash", "$ \n", record);
        o.pane_shell_idle = Some(false);
        let base = Instant::now();
        let mut arb = Arbiter::settled("letta");
        for read in 0..10 {
            assert!(
                !matches!(
                    decide_at(&o, base, read * 5000, &mut arb),
                    Decision::NoAgent { clear: true }
                ),
                "read {read}"
            );
        }
        // Its own shell is idle again: the agent is gone.
        o.pane_shell_idle = Some(true);
        assert_eq!(
            decide_at(&o, base, 50_000, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn a_new_foreground_command_is_a_miss_at_once() {
        // herdr probes the process on a foreground change as well as every 5 s: each
        // change of command is a miss of its own, however close together.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let record = prev_with_rule(
            Some("claude"),
            Some(AgentState::Idle),
            Some("osc_title_idle"),
        );
        for (n, command) in ["vim", "less", "vim", "less", "vim"]
            .into_iter()
            .enumerate()
        {
            let o = observed(command, "~\n", record.clone());
            assert_eq!(
                decide_at(&o, base, n as u64 * 100, &mut arb),
                Decision::Keep,
                "miss {}",
                n + 1
            );
        }
        let sixth = observed("less", "~\n", record);
        assert_eq!(
            decide_at(&sixth, base, 500, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn misses_are_counted_at_the_recheck_interval_the_watcher_runs() {
        // The scheduler's identity recheck and the miss interval are one number (herdr's
        // `PROCESS_RECHECK_IDENTIFIED`): a watcher rechecking every second counts a miss
        // every second, and the sixth clears at 5 s rather than 25 s.
        let base = Instant::now();
        let mut arb = Arbiter {
            identity_recheck: Duration::from_secs(1),
            ..Arbiter::settled("claude")
        };
        // An exit forgets the agent, not how often the watcher reads.
        let exited = observed(
            "zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        assert_eq!(
            decide_at(&exited, base, 0, &mut arb),
            Decision::NoAgent { clear: true }
        );
        assert_eq!(arb.identity_recheck, Duration::from_secs(1));
        arb = Arbiter {
            identity_recheck: arb.identity_recheck,
            ..Arbiter::settled("claude")
        };
        let vim = observed(
            "vim",
            "~\n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("osc_title_idle"),
            ),
        );
        for second in 1..=5 {
            assert_ne!(
                decide_at(&vim, base, second * 1000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {second}"
            );
        }
        assert_eq!(
            decide_at(&vim, base, 6000, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn an_exit_owed_its_clear_is_cleared_whatever_is_in_front() {
        // herdr clears a reported exit on the next probe whatever it finds — the agent is
        // gone, and a process started after it is not a miss of it.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let exited = observed(
            "zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        assert!(matches!(
            decide_at(&exited, base, 0, &mut arb),
            Decision::Transition {
                to: AgentState::Done,
                ..
            }
        ));
        let vim = observed(
            "vim",
            "~\n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Done),
                Some("process_exited"),
            ),
        );
        assert_eq!(
            decide_at(&vim, base, 300, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    /// What an agent's own hook leaves on a pane (`pane report-agent-session`): its kind,
    /// the session it reported, and the mark that the agent itself said so. No state, and no
    /// `hook:` source, which the hook does not claim.
    fn named_itself(kind: &str, state: Option<AgentState>) -> AgentRecord {
        AgentRecord {
            reported_kind: Some(kind.to_string()),
            session_id: Some("conv-1".to_string()),
            ..prev(Some(kind), state, false)
        }
    }

    /// Letta's composer at rest, as the fake in `tests/e2e_letta_session.rs` draws it.
    const LETTA_COMPOSER: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\n\u{203A} Try \"debug this error\"\n\u{2500}\u{2500}\u{2500}\u{2500}\nTutor \u{B7} No model selected\n";

    #[test]
    fn an_agent_that_named_itself_is_kept_however_long_another_command_stays_in_front() {
        // npm's Letta runs under `node`: nothing about the command says Letta, and the
        // ordinary rule clears such a pane on the sixth miss, taking the session id its own
        // hook reported. The agent said what it is, so a command that does not is no reason
        // to forget it: an hour of rechecks, 5 s apart, clears nothing.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut o = observed(
            "node",
            LETTA_COMPOSER,
            named_itself("letta", Some(AgentState::Idle)),
        );
        o.title = "Tutor".to_string();
        for read in 0..720 {
            let decision = decide_at(&o, base, read * 5000, &mut arb);
            assert!(
                !matches!(decision, Decision::NoAgent { .. }),
                "read {read} cleared the pane: {decision:?}"
            );
        }
        assert_eq!(arb.identity.as_deref(), Some("letta"));
        // And it is read as the agent it said it was: a capture, then the agent's rules.
        assert!(wants_screen(&o, &arb));
        assert_eq!(
            decide_at(&o, base, 3_600_000, &mut arb),
            Decision::RuleChanged {
                rule: Some("composer_idle".to_string())
            }
        );
    }

    #[test]
    fn an_agent_that_named_itself_is_kept_through_a_command_that_changes_every_read() {
        // A change of the foreground command is a miss at once, however close together:
        // an agent that named itself is not judged by that either.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        for (n, command) in ["node", "git", "node", "npm", "node", "make", "node", "rg"]
            .into_iter()
            .enumerate()
        {
            let o = observed(
                command,
                LETTA_COMPOSER,
                named_itself("letta", Some(AgentState::Idle)),
            );
            let decision = decide_at(&o, base, n as u64 * 100, &mut arb);
            assert!(
                !matches!(decision, Decision::NoAgent { .. }),
                "read {n} ({command}) cleared the pane: {decision:?}"
            );
        }
    }

    #[test]
    fn an_agent_that_named_itself_is_cleared_when_its_shell_is_back() {
        // What ends it is what ends any agent: the pane's shell in front. From a rest, at
        // once.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let node = observed(
            "node",
            LETTA_COMPOSER,
            named_itself("letta", Some(AgentState::Idle)),
        );
        assert_ne!(
            decide_at(&node, base, 0, &mut arb),
            Decision::NoAgent { clear: true }
        );
        for shell in ["zsh", "-zsh", "bash", "sh", "fish"] {
            let back = at_the_panes_prompt(observed(
                shell,
                "$ \n",
                named_itself("letta", Some(AgentState::Idle)),
            ));
            assert_eq!(
                decide_at(&back, base, 5000, &mut Arbiter::default()),
                Decision::NoAgent { clear: true },
                "{shell}"
            );
        }
    }

    #[test]
    fn an_agent_that_named_itself_writes_its_turns_end_before_the_shell_clears_it() {
        // Mid-turn when it exits: the turn's end is written first, as for every agent, and
        // the read after it clears the record whatever is in front by then.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let working = observed(
            "node",
            LETTA_COMPOSER,
            named_itself("letta", Some(AgentState::Working)),
        );
        let _ = decide_at(&working, base, 0, &mut arb);
        let back = at_the_panes_prompt(observed(
            "zsh",
            "$ \n",
            named_itself("letta", Some(AgentState::Working)),
        ));
        assert!(matches!(
            decide_at(&back, base, 300, &mut arb),
            Decision::Transition {
                to: AgentState::Done,
                ..
            }
        ));
        let mut ended = named_itself("letta", Some(AgentState::Done));
        ended.rule = Some("process_exited".to_string());
        let vim = observed("vim", "~\n", ended);
        assert_eq!(
            decide_at(&vim, base, 600, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn a_report_is_about_the_kind_it_named_and_nothing_a_later_occupant_is() {
        // The watcher recorded another agent over the one whose hook spoke, so the report
        // says nothing about this occupant: an ordinary agent, cleared on the sixth miss.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut record = named_itself("letta", Some(AgentState::Idle));
        record.kind = Some("claude".to_string());
        let o = observed("node", "~\n", record);
        for miss in 0..5 {
            assert_ne!(
                decide_at(&o, base, miss * 5000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {} must not clear the pane",
                miss + 1
            );
        }
        assert_eq!(
            decide_at(&o, base, 25_000, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn a_kind_nobody_reported_is_still_judged_by_the_misses() {
        // A session id and a kind on the pane are not the mark: the agent has to have said
        // so itself (a restore writes the id, and a hand can write the kind).
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut record = prev(Some("letta"), Some(AgentState::Idle), false);
        record.session_id = Some("conv-1".to_string());
        let o = observed("node", LETTA_COMPOSER, record);
        for miss in 0..5 {
            assert_ne!(
                decide_at(&o, base, miss * 5000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {} must not clear the pane",
                miss + 1
            );
        }
        assert_eq!(
            decide_at(&o, base, 25_000, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn an_agent_taw_names_by_its_process_is_ended_by_any_shell_in_front_of_it() {
        // Claude's own hook reported its kind, and the process in front of the pane has named
        // Claude at a read, so taw can identify it: the shell that comes back ends it, the
        // pane's own or one the user started inside the pane, as it ended every agent before a
        // hook reported a kind. Only an agent taw cannot identify is kept until the pane's own
        // shell is back.
        for shell in ["bash", "zsh", "sh", "-bash"] {
            let base = Instant::now();
            let mut arb = Arbiter::default();
            let running = observed(
                "claude",
                IDLE,
                named_itself("claude", Some(AgentState::Idle)),
            );
            let _ = decide_at(&running, base, 0, &mut arb);
            let mut inside = observed(
                shell,
                "$ \n",
                named_itself("claude", Some(AgentState::Idle)),
            );
            inside.pane_shell_idle = Some(false);
            assert_eq!(
                decide_at(&inside, base, 300, &mut arb),
                Decision::NoAgent { clear: true },
                "{shell}"
            );
        }
        // Mid-turn, its end is written first, as for any agent that exits.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let running = observed(
            "claude",
            WORKING,
            named_itself("claude", Some(AgentState::Working)),
        );
        let _ = decide_at(&running, base, 0, &mut arb);
        let mut inside = observed(
            "bash",
            "$ \n",
            named_itself("claude", Some(AgentState::Working)),
        );
        inside.pane_shell_idle = Some(false);
        assert!(matches!(
            decide_at(&inside, base, 300, &mut arb),
            Decision::Transition {
                to: AgentState::Done,
                ..
            }
        ));
    }

    #[test]
    fn an_agent_taw_names_by_its_process_is_cleared_on_the_sixth_miss_whatever_its_hook_reported() {
        // `claude; npm run dev`: the program after Claude is a miss of it, one on the change
        // of command and one per recheck, and the sixth clears the record and the session id
        // with it. The hook's word that it was Claude does not keep it for as long as the
        // program runs, which is only for an agent taw cannot identify.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let running = observed(
            "claude",
            IDLE,
            named_itself("claude", Some(AgentState::Idle)),
        );
        let _ = decide_at(&running, base, 0, &mut arb);
        let dev = observed(
            "npm",
            "> dev\n",
            named_itself("claude", Some(AgentState::Idle)),
        );
        for miss in 0..5 {
            assert_ne!(
                decide_at(&dev, base, 100 + miss * 5000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {} must not clear the pane",
                miss + 1
            );
        }
        assert_eq!(
            decide_at(&dev, base, 25_100, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    /// Whether the record is kept behind a shell the user started, the last `reads` being what
    /// the watcher saw of the pane before it: each is a command and the record on the pane
    /// with it. The shell in front is not the pane's own.
    fn kept_behind_a_nested_shell(reads: Vec<(&str, AgentRecord)>, record: AgentRecord) -> bool {
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut at = 0;
        for (command, record) in reads {
            let _ = decide_at(
                &observed(command, LETTA_COMPOSER, record),
                base,
                at,
                &mut arb,
            );
            at += 300;
        }
        let mut inside = observed("bash", "$ \n", record);
        inside.pane_shell_idle = Some(false);
        !matches!(
            decide_at(&inside, base, at, &mut arb),
            Decision::NoAgent { clear: true }
        )
    }

    #[test]
    fn whether_a_process_has_named_the_agent_is_remembered_while_its_record_stands() {
        let claude = || named_itself("claude", Some(AgentState::Idle));
        let letta = || named_itself("letta", Some(AgentState::Idle));
        // Named at a read before the hook reported anything, or after it: taw can identify it,
        // and a shell in front of it ends it.
        assert!(!kept_behind_a_nested_shell(
            vec![("claude", prev(Some("claude"), Some(AgentState::Idle), true))],
            claude()
        ));
        assert!(!kept_behind_a_nested_shell(
            vec![("claude", claude())],
            claude()
        ));
        // Named by no process at any read: kept until the pane's own shell is back.
        assert!(kept_behind_a_nested_shell(vec![("node", letta())], letta()));
        assert!(kept_behind_a_nested_shell(vec![], letta()));
        // Forgotten when the record clears: a Claude that runs under `node`, reporting for the
        // pane a native Claude has just left, is not one taw can identify, even when the next
        // read of the pane is the first to see the new record.
        assert!(kept_behind_a_nested_shell(
            vec![("claude", claude()), ("zsh", claude()), ("node", claude()),],
            claude()
        ));
        // Forgotten when the kind changes: the Letta that reports for the pane a Claude was
        // recorded in has never been named by a process, and neither has the Claude that
        // reports for it after that.
        assert!(kept_behind_a_nested_shell(
            vec![("claude", claude()), ("node", letta())],
            letta()
        ));
        assert!(kept_behind_a_nested_shell(
            vec![("claude", claude()), ("node", letta()), ("node", claude())],
            claude()
        ));
    }

    #[test]
    fn a_hook_reported_agent_taw_names_by_its_process_is_released_by_any_shell_in_front_of_it() {
        // The same split for a pane whose state a hook reports: named by its process, the
        // ordinary rule (a shell in front ends it), and never named, the shell has to be the
        // pane's own.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let running = observed("kimi", "", hooked("kimi", AgentState::Idle));
        let _ = decide_at(&running, base, 0, &mut arb);
        let mut inside = observed("bash", "$ \n", hooked("kimi", AgentState::Idle));
        inside.pane_shell_idle = Some(false);
        assert_eq!(
            decide_at(&inside, base, 300, &mut arb),
            Decision::NoAgent { clear: true }
        );
        let mut unnamed = observed("bash", "$ \n", hooked("kimi", AgentState::Idle));
        unnamed.pane_shell_idle = Some(false);
        assert_eq!(
            decide_at(&unnamed, base, 600, &mut Arbiter::default()),
            Decision::HookAuthoritative { kind: None }
        );
    }

    #[test]
    fn an_agent_in_a_container_is_one_a_process_names_so_its_shell_is_not_asked_about() {
        // A container pane: `docker` in front, Claude named by the runtime's arguments, and its own
        // hook having reported the kind. The arguments name it as its command would, so it is
        // an agent taw can identify and nothing needs the pane's shell to be idle, where the
        // same record under `node` is one it cannot, and asks.
        let now = Instant::now();
        let record = || named_itself("claude", Some(AgentState::Idle));
        let boxed = in_container(observed("docker", IDLE, record()), "claude");
        assert!(!Arbiter::default().wants_shell_probe(&boxed, now));
        let under_node = observed("node", IDLE, record());
        assert!(Arbiter::default().wants_shell_probe(&under_node, now));
    }

    #[test]
    fn an_agent_in_a_container_is_cleared_on_the_sixth_miss_whatever_its_hook_reported() {
        // The container stops and `npm run dev` takes the pane: a miss on the change of
        // command and one per recheck, and the sixth clears the record and the session id with
        // it. The hook's word that it was Claude keeps only an agent no process has named, and
        // the runtime's arguments named this one.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let running = in_container(
            observed(
                "docker",
                IDLE,
                named_itself("claude", Some(AgentState::Idle)),
            ),
            "claude",
        );
        let _ = decide_at(&running, base, 0, &mut arb);
        let dev = observed(
            "npm",
            "> dev\n",
            named_itself("claude", Some(AgentState::Idle)),
        );
        for miss in 0..5 {
            assert_ne!(
                decide_at(&dev, base, 100 + miss * 5000, &mut arb),
                Decision::NoAgent { clear: true },
                "miss {} must not clear the pane",
                miss + 1
            );
        }
        assert_eq!(
            decide_at(&dev, base, 25_100, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn an_agent_in_a_container_is_ended_by_any_shell_in_front_of_it_whatever_reported_it() {
        // The runtime names it, so the shell that comes back ends it, the pane's own or one the
        // user started inside the pane, as it ends an agent run from a prompt. Its own hook's
        // report of the kind, or of the state, changes none of that.
        for record in [
            || named_itself("claude", Some(AgentState::Idle)),
            || hooked("claude", AgentState::Idle),
        ] {
            for shell in ["bash", "zsh", "sh"] {
                let base = Instant::now();
                let mut arb = Arbiter::default();
                let running = in_container(observed("docker", IDLE, record()), "claude");
                let _ = decide_at(&running, base, 0, &mut arb);
                let mut inside = observed(shell, "$ \n", record());
                inside.pane_shell_idle = Some(false);
                assert_eq!(
                    decide_at(&inside, base, 300, &mut arb),
                    Decision::NoAgent { clear: true },
                    "{shell}"
                );
            }
        }
        // Mid-turn, its end is written first, as for any agent that exits.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let running = in_container(
            observed(
                "docker",
                WORKING,
                named_itself("claude", Some(AgentState::Working)),
            ),
            "claude",
        );
        let _ = decide_at(&running, base, 0, &mut arb);
        let mut inside = observed(
            "bash",
            "$ \n",
            named_itself("claude", Some(AgentState::Working)),
        );
        inside.pane_shell_idle = Some(false);
        assert!(matches!(
            decide_at(&inside, base, 300, &mut arb),
            Decision::Transition {
                to: AgentState::Done,
                ..
            }
        ));
    }

    #[test]
    fn a_process_in_front_of_the_agent_is_judged_by_the_agents_rules() {
        // Kept through its misses, the agent is still the pane's occupant: its manifest
        // reads the screen, and a plain idle mid-turn is held like any other. Not
        // `unknown` — that is for a hint with nothing behind it, not for this.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut vim = observed("vim", "~\n~\n-- INSERT --\n", working_turn());
        vim.title = "\u{273B} fake task".to_string();
        assert_eq!(decide_at(&vim, base, 0, &mut arb), Decision::Keep);
        assert!(arb.hold_active());
        let mut idle = vim.clone();
        idle.prev = prev(Some("claude"), Some(AgentState::Idle), true);
        assert_eq!(
            decide_at(&idle, base, 100, &mut Arbiter::settled("claude")),
            Decision::Keep,
            "an idle agent behind vim stays idle"
        );
    }

    #[test]
    fn a_first_sighting_transitions_from_no_state_at_all() {
        // herdr's startup grace: a newly identified agent is `unknown` for 3 s and one
        // more read, whatever its first frames look like, and only then what its screen
        // says. `unknown` earns no notification.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let o = observed("claude", IDLE, AgentRecord::default());
        match decide_at(&o, base, 0, &mut arb) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, None);
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
        let unknown = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        for ms in [300, 2700, 3000] {
            assert_eq!(
                decide_at(&unknown, base, ms, &mut arb),
                Decision::Keep,
                "inside the grace, or its extra read, at {ms} ms"
            );
        }
        match decide_at(&unknown, base, 3300, &mut arb) {
            Decision::Transition { from, to, rule, .. } => {
                assert_eq!(from, Some(AgentState::Unknown));
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    #[test]
    fn a_different_agent_in_the_pane_starts_a_new_grace() {
        // A replacement is a new agent: its first frames say nothing yet about its turn.
        let mut arb = Arbiter::settled("codex");
        let o = observed(
            "claude",
            IDLE,
            prev(Some("codex"), Some(AgentState::Idle), true),
        );
        match decide_at(&o, Instant::now(), 0, &mut arb) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, Some(AgentState::Idle));
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None);
            }
            other => panic!("expected the grace's unknown, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_started_where_one_that_left_was_is_a_new_agent_with_a_grace_of_its_own() {
        // The pane's record has been taken off it (`taw agent start` does that before it types
        // the command for the next agent), the hint is still there, and the watcher still holds
        // the identity of the agent that left, whose screen is still on the pane. The agent that
        // comes is of the same kind and is a new one: its screen is the last agent's until it
        // draws, and says nothing yet.
        let base = Instant::now();
        let mut arb = Arbiter::settled("codex");
        let o = hinted_codex(CODEX_AT_REST, AgentRecord::default());
        match decide_at(&o, base, 0, &mut arb) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "codex");
                assert_eq!(from, None);
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None);
            }
            other => panic!("expected the grace's unknown, got {other:?}"),
        }
        assert!(arb.in_startup_grace());
        // Inside the window nothing is written, however the screen reads, and after it the
        // screen decides.
        let started = hinted_codex(
            CODEX_AT_REST,
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        assert_eq!(decide_at(&started, base, 1000, &mut arb), Decision::Keep);
        match after_the_grace(&started, base, 0, &mut arb) {
            Decision::Transition { to, .. } => assert_eq!(to, AgentState::Idle),
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    /// What the pane holds once `decision` is written, as the watcher writes it.
    fn write_back(o: &mut Observed, decision: &Decision) {
        match decision {
            Decision::Transition { kind, to, rule, .. } => {
                o.prev.kind = Some(kind.clone());
                o.prev.state = Some(*to);
                o.prev.rule.clone_from(rule);
            }
            Decision::KindChanged { kind } => o.prev.kind = Some(kind.clone()),
            Decision::RuleChanged { rule } => o.prev.rule.clone_from(rule),
            _ => {}
        }
    }

    /// A read of `o` `ms` after `base`, with the rules `store` loads, written back into `o`: the
    /// state and the rule the pane holds after it.
    fn read_with(
        store: &ManifestStore,
        o: &mut Observed,
        base: Instant,
        ms: u64,
        arb: &mut Arbiter,
    ) -> (Option<AgentState>, Option<String>) {
        let decision = decide(
            store,
            &mut HashMap::new(),
            o,
            false,
            base + Duration::from_millis(ms),
            arb,
        );
        write_back(o, &decision);
        (o.prev.state, o.prev.rule.clone())
    }

    /// OpenCode's composer at rest: none of its rules knows it, so it reads idle only as the
    /// fallback.
    const OPENCODE_AT_REST: &str = "\u{2503}  Ask anything...\n\u{2503}\n\u{2503}  Build  big-pickle OpenCode Zen\n\n  tab agents  ctrl+p commands\n";
    /// OpenCode mid-turn: its progress bar (`progress_bar_working`).
    const OPENCODE_WORKING: &str = "\u{2503}  Ask anything...\n\u{2503}\n\u{2503}  Build  big-pickle OpenCode Zen\n\n  \u{25A0}\u{25A0}\u{25A0}\u{25A0}\u{2B1D}\u{2B1D}\u{2B1D}\u{2B1D}  esc interrupt\n";
    /// A model picker over OpenCode's composer: no rule knows it either.
    const OPENCODE_PICKER: &str =
        "Select model\n\n  Recent\n  big-pickle OpenCode Zen\n  gpt-5.6 OpenAI\n";

    #[test]
    fn an_agent_with_no_idle_rule_read_below_the_typed_line_reads_idle_and_keeps_its_done() {
        // OpenCode's rules know its work and its questions, not its rest: it reads idle only as the
        // fallback, a frame no rule knows. `taw agent start` typed its command, and it is read below
        // that line for as long as the line is on screen, on the alternate screen for its whole
        // life, so that fallback is its idle there too: at its first start, and when it is started
        // again in the pane the last one died in and the process table names it at once.
        let store = bundled_only();
        for relaunch in [false, true] {
            let base = Instant::now();
            let mut arb = if relaunch {
                Arbiter::settled("opencode")
            } else {
                Arbiter::default()
            };
            let mut o = observed("opencode", "", AgentRecord::default());
            o.title = String::new();
            o.kind_hint = Some("opencode".to_string());
            o.below_typed_line = true;
            let case = if relaunch { "relaunch" } else { "first start" };
            // Nothing drawn yet: unknown, through a first start's grace.
            assert_eq!(
                read_with(&store, &mut o, base, 0, &mut arb).0,
                Some(AgentState::Unknown),
                "{case}"
            );
            let mut ms = 300;
            if !relaunch {
                for at in [300, 3000] {
                    read_with(&store, &mut o, base, at, &mut arb);
                }
                ms = 3300;
            }
            o.screen = OPENCODE_AT_REST.to_string();
            assert_eq!(
                read_with(&store, &mut o, base, ms, &mut arb),
                (Some(AgentState::Idle), None),
                "{case}: its composer at rest"
            );
            o.screen = OPENCODE_WORKING.to_string();
            assert_eq!(
                read_with(&store, &mut o, base, ms + 300, &mut arb).0,
                Some(AgentState::Working),
                "{case}: a turn"
            );
            // The turn ends: a plain idle after `working`, held 700 ms, and `done` unseen.
            o.screen = OPENCODE_AT_REST.to_string();
            read_with(&store, &mut o, base, ms + 600, &mut arb);
            assert_eq!(
                read_with(&store, &mut o, base, ms + 1300, &mut arb).0,
                Some(AgentState::Done),
                "{case}: the turn's end"
            );
            // A further read of its rest, or of a frame no rule knows, leaves the `done`.
            for (at, screen) in [
                (ms + 1600, OPENCODE_AT_REST),
                (ms + 1900, OPENCODE_PICKER),
                (ms + 2200, OPENCODE_AT_REST),
            ] {
                o.screen = screen.to_string();
                assert_eq!(
                    read_with(&store, &mut o, base, at, &mut arb).0,
                    Some(AgentState::Done),
                    "{case}: the done went at {at} ms"
                );
            }
        }
    }

    #[test]
    fn a_kind_whose_idle_only_its_title_says_never_reads_the_title_the_last_one_left() {
        // An agent of a kind whose idle rules read only its title, started again in the pane the last
        // one died in and named by its process at the first read. The last one left its title on the
        // pane: its idle title, a spinner of a turn, or a question for a person. That title reads as
        // none until tmux reports another, whatever the time: once the new agent has drawn, its frames
        // read idle as the fallback, never working or blocked off the dead agent's title, before
        // STARTUP_GRACE and after it. Codex counts as one without taw's supplement, whose rule on its
        // composer reads its screen.
        let grace = u64::try_from(STARTUP_GRACE.as_millis()).unwrap();
        for (kind, store, screen, titles, own) in [
            (
                "codex",
                bundled_only().with_supplements(false),
                "\u{203A} Ask Codex to do anything\n\n  gpt-5.6-sol default \u{B7} /work\n",
                ["fixO", "\u{280B} fixO", "Action Required"],
                "\u{2819} fixO",
            ),
            (
                "amp",
                bundled_only(),
                "Welcome to Amp\n",
                [
                    "fixO - amp - main",
                    "\u{280B} fixO",
                    "Plugin confirmation needed",
                ],
                "\u{2819} fixO",
            ),
            (
                "hermes",
                bundled_only(),
                "Hermes Agent\n",
                ["\u{2713} fixO", "\u{23F3} fixO", "\u{26A0} fixO"],
                "\u{23F3} its own task",
            ),
        ] {
            for (left, title) in ["idle", "working", "blocking"].into_iter().zip(titles) {
                let base = Instant::now();
                let mut arb = Arbiter::settled(kind);
                let mut o = observed(kind, "", AgentRecord::default());
                o.title = title.to_string();
                o.kind_hint = Some(kind.to_string());
                o.below_typed_line = true;
                assert_eq!(
                    read_with(&store, &mut o, base, 0, &mut arb),
                    (Some(AgentState::Unknown), None),
                    "{kind}, the last one's {left} title: nothing drawn yet"
                );
                o.screen = screen.to_string();
                let reads: Vec<_> = [300, grace - 100, grace, grace + 300, 2 * grace]
                    .into_iter()
                    .map(|ms| (ms, read_with(&store, &mut o, base, ms, &mut arb)))
                    .collect();
                for (ms, (state, _)) in &reads {
                    assert!(
                        !matches!(state, Some(AgentState::Working | AgentState::Blocked)),
                        "{kind}: read {state:?} off the last agent's {left} title {ms} ms on"
                    );
                }
                for (ms, read) in reads {
                    assert_eq!(
                        read,
                        (Some(AgentState::Idle), None),
                        "{kind}, the last one's {left} title: its frame {ms} ms on"
                    );
                }
                // A title tmux reports after the last one's is the new agent's own, and is read.
                o.title = own.to_string();
                assert_eq!(
                    read_with(&store, &mut o, base, 2 * grace + 300, &mut arb),
                    (
                        Some(AgentState::Working),
                        Some("osc_title_working".to_string())
                    ),
                    "{kind}, the last one's {left} title: its own title was not read"
                );
            }
        }
    }

    #[test]
    fn a_watcher_started_again_between_agent_start_and_its_first_read_never_reads_the_last_title() {
        // `agent start` took the record off and marked the pane as being taken, and the watcher was
        // started again before its first read of the new agent: its arbiter is fresh and knows of no
        // agent that left. The title the dead one left is still on the pane: a codex's question (with
        // taw's supplement off) or a claude's `✳`. Before the new agent has drawn, past STARTUP_GRACE,
        // that title reads neither `blocked` nor idle; the first title tmux reports after it is read.
        let grace = u64::try_from(STARTUP_GRACE.as_millis()).unwrap();
        for (kind, store, left, own) in [
            (
                "codex",
                bundled_only().with_supplements(false),
                "Action Required",
                "\u{2819} fixO",
            ),
            (
                "claude",
                bundled_only(),
                "\u{2733} fake task",
                "\u{2810} its own task",
            ),
        ] {
            let base = Instant::now();
            let mut arb = Arbiter::default();
            let mut o = observed(kind, "", AgentRecord::default());
            o.title = left.to_string();
            o.kind_hint = Some(kind.to_string());
            o.acquiring = Some(Acquired::At(None));
            o.below_typed_line = true;
            for ms in [0, 300, grace, grace + 300, 2 * grace] {
                assert_eq!(
                    read_with(&store, &mut o, base, ms, &mut arb),
                    (Some(AgentState::Unknown), None),
                    "{kind}: read the last agent's title {left:?} {ms} ms on"
                );
            }
            o.title = own.to_string();
            assert_eq!(
                read_with(&store, &mut o, base, 2 * grace + 300, &mut arb),
                (
                    Some(AgentState::Working),
                    Some("osc_title_working".to_string())
                ),
                "{kind}: its own title was not read"
            );
        }
        // Once per start: a read that leaves the record empty (one with no screen to decide on) and a
        // next read that finds the new agent's own title reads that title, and does not set it aside
        // as the last agent's.
        let store = bundled_only().with_supplements(false);
        let base = Instant::now();
        let mut arb = Arbiter::settled("codex");
        let mut o = observed("codex", "", AgentRecord::default());
        o.title = "Action Required".to_string();
        o.acquiring = Some(Acquired::At(None));
        o.below_typed_line = true;
        o.screen_unread = true;
        assert_eq!(
            read_with(&store, &mut o, base, 0, &mut arb),
            (None, None),
            "a read with no screen keeps the empty record"
        );
        o.screen_unread = false;
        o.title = "\u{2819} fixO".to_string();
        assert_eq!(
            read_with(&store, &mut o, base, 300, &mut arb),
            (
                Some(AgentState::Working),
                Some("osc_title_working".to_string())
            ),
            "the new agent's own title, read after a read that kept the record empty"
        );
    }

    #[test]
    fn a_first_agent_start_in_a_pane_whose_shell_set_a_title_reads_what_the_agent_draws() {
        // A first `agent start` in a fresh pane, whose shell set a title of its own: that title is set
        // aside with the rest of what was on the pane, and the agent reads what its own drawing gives,
        // then what its own title gives once tmux reports it.
        let store = bundled_only();
        let grace = u64::try_from(STARTUP_GRACE.as_millis()).unwrap();
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut o = observed("claude", "", AgentRecord::default());
        o.title = "me@box: ~/work".to_string();
        o.kind_hint = Some("claude".to_string());
        o.acquiring = Some(Acquired::At(None));
        o.below_typed_line = true;
        for ms in [0, 300, grace] {
            assert_eq!(
                read_with(&store, &mut o, base, ms, &mut arb),
                (Some(AgentState::Unknown), None),
                "{ms} ms on"
            );
        }
        o.screen = IDLE.to_string();
        assert_eq!(
            read_with(&store, &mut o, base, grace + 300, &mut arb),
            (Some(AgentState::Idle), Some("live_prompt_box".to_string())),
            "its prompt box under the shell's title"
        );
        o.title = "\u{2810} its own task".to_string();
        assert_eq!(
            read_with(&store, &mut o, base, grace + 600, &mut arb),
            (
                Some(AgentState::Working),
                Some("osc_title_working".to_string())
            ),
            "its own title"
        );
    }

    #[test]
    fn a_watcher_started_again_keeps_the_done_of_an_agent_that_drew_long_ago() {
        // A watcher started again (a plugin reload, an upgrade, `taw watch attach-all --restart`) has
        // a fresh arbiter, so the agent the pane records is identified anew. But it drew long ago: its
        // turn has ended, the pane says `done`, and the line its command was typed on is still on the
        // screen. A frame no rule knows there (output between a turn's end and its prompt box) reads
        // as the fallback, as it did before the restart, so the `done` stays.
        let store = bundled_only();
        for (command, job) in [("claude", None), ("sh", Some("claude"))] {
            let base = Instant::now();
            let mut arb = Arbiter::default();
            let mut o = observed(
                command,
                "  The tests pass now.\n\n  Summary of the change\n",
                prev(Some("claude"), Some(AgentState::Done), false),
            );
            o.title = String::new();
            o.kind_hint = Some("claude".to_string());
            o.job_agent = job.map(str::to_string);
            o.below_typed_line = true;
            for ms in [0, 300, 3300] {
                assert_eq!(
                    read_with(&store, &mut o, base, ms, &mut arb).0,
                    Some(AgentState::Done),
                    "{command}: the done went at {ms} ms"
                );
            }
        }
    }

    #[test]
    fn an_install_below_the_typed_line_is_unknown_after_a_replacement_or_a_first_identification() {
        // The start window opens on every real start, not only on a relaunch a process names at once:
        // on a replacement (the next agent named by its hint alone at the first read after the record
        // went) and on a first identification. Once its grace is over and its process names it, what it
        // printed below the line its command was typed on that no rule of its kind knows (an install's
        // progress) is no idle of its own, until a rule of its kind matches.
        let store = bundled_only();
        let install = "Need to install the following packages:\n  @anthropic-ai/claude-code@2.1.300\n\
                       added 3 packages in 2s\n";
        for (start, mut arb, first_command) in [
            ("replacement", Arbiter::settled("claude"), "sh"),
            ("first identification", Arbiter::default(), "claude"),
        ] {
            let base = Instant::now();
            let mut o = observed(first_command, "", AgentRecord::default());
            o.title = String::new();
            o.kind_hint = Some("claude".to_string());
            o.below_typed_line = true;
            assert_eq!(
                read_with(&store, &mut o, base, 0, &mut arb),
                (Some(AgentState::Unknown), None),
                "{start}: its grace"
            );
            o.command = "claude".to_string();
            o.screen = install.to_string();
            for ms in [300, 3000, 3300, 3600, 6000] {
                assert_eq!(
                    read_with(&store, &mut o, base, ms, &mut arb),
                    (Some(AgentState::Unknown), None),
                    "{start}: the install read as the agent's own {ms} ms on"
                );
            }
            o.screen = IDLE.to_string();
            assert_eq!(
                read_with(&store, &mut o, base, 6300, &mut arb),
                (Some(AgentState::Idle), Some("live_prompt_box".to_string())),
                "{start}: its prompt box"
            );
        }
    }

    #[test]
    fn a_slow_start_of_a_kind_whose_screen_says_its_idle_never_reads_the_title_the_last_one_left() {
        // Claude started again in the pane the last one died in, named by its process at the first
        // read, fetches itself first: the install's rows below the line its command was typed on
        // for longer than STARTUP_GRACE, and the last Claude's `✳` title still on the pane. Its
        // screen says it is idle once it draws, so the title stays the last one's until tmux
        // reports another, whatever the time: unknown throughout, then idle by its prompt box.
        let store = bundled_only();
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let install = "Need to install the following packages:\n  @anthropic-ai/claude-code@2.1.300\n\
                       added 3 packages in 2s\n";
        let mut o = observed("claude", install, AgentRecord::default());
        o.kind_hint = Some("claude".to_string());
        o.below_typed_line = true;
        for ms in [0, 300, 3000, 3300, 6000, 9000] {
            assert_eq!(
                read_with(&store, &mut o, base, ms, &mut arb),
                (Some(AgentState::Unknown), None),
                "read off the last agent's title or the install {ms} ms on"
            );
        }
        o.screen = IDLE.to_string();
        assert_eq!(
            read_with(&store, &mut o, base, 9300, &mut arb),
            (Some(AgentState::Idle), Some("live_prompt_box".to_string())),
            "its prompt box"
        );
    }

    #[test]
    fn a_frame_no_rule_knows_after_a_turn_reads_as_the_fallback_once_the_agent_has_drawn() {
        // Claude started again where the last one died, read below the line its command was typed
        // on. Once a rule of its own has matched (its prompt box), it has drawn: a frame no rule
        // knows after that (output caught between a turn's end and its prompt box) is read as any
        // agent's is, so the turn's `done` stays.
        let store = bundled_only();
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut o = observed("claude", "", AgentRecord::default());
        o.title = String::new();
        o.kind_hint = Some("claude".to_string());
        o.below_typed_line = true;
        let mut reads = [
            ("", AgentState::Unknown),
            (IDLE, AgentState::Idle),
            (WORKING, AgentState::Working),
            (IDLE, AgentState::Done),
            (
                "  The tests pass now.\n\n  Summary of the change\n",
                AgentState::Done,
            ),
        ]
        .into_iter()
        .zip((0..).step_by(300));
        for ((screen, wanted), ms) in &mut reads {
            o.screen = screen.to_string();
            assert_eq!(
                read_with(&store, &mut o, base, ms, &mut arb).0,
                Some(wanted),
                "{ms} ms: {screen:?}"
            );
        }
    }

    #[test]
    fn the_fact_the_start_window_takes_from_each_bundled_manifest() {
        // Which kinds have an idle rule that reads the screen, counted from the bundled manifests: a
        // refresh that moves a kind fails here and gets a look. The rest read idle only off their
        // title or progress (amp, hermes, codex's own rules), or have no idle rule at all. taw's
        // supplement gives Codex a rule on its composer, which reads the screen.
        let screen = [
            "claude", "cline", "devin", "grok", "kiro", "letta", "maki", "muse", "qwen",
        ];
        let other = [
            "agy", "amp", "codex", "copilot", "cursor", "droid", "gemini", "hermes", "kilo",
            "kimi", "opencode", "pi", "qodercli",
        ];
        let mut ids: Vec<&str> = detect::bundled_ids().to_vec();
        ids.sort_unstable();
        let mut listed: Vec<&str> = screen.iter().chain(&other).copied().collect();
        listed.sort_unstable();
        assert_eq!(ids, listed, "every bundled manifest is listed once");
        let vendored = bundled_only().with_supplements(false);
        let loaded = bundled_only();
        for id in ids {
            let fact = |store: &ManifestStore| store.load(id).unwrap().has_screen_idle_rule();
            assert_eq!(fact(&vendored), screen.contains(&id), "{id}, its own rules");
            assert_eq!(
                fact(&loaded),
                screen.contains(&id) || id == "codex",
                "{id}, with taw's supplement"
            );
        }
    }

    #[test]
    fn a_successor_a_process_names_at_its_first_read_does_not_take_the_title_the_last_agent_left() {
        // The last agent of a hint-started pane has died with its `✳` title on the pane, and
        // `taw agent start` has taken the record off and typed the command for the next. The first
        // read after that can already find the next one in front: a binary called `claude`, or a
        // script that the job under the shell names. It has drawn nothing below the line its
        // command was typed on, and the title is the last agent's: no idle of the new one's.
        for (command, job) in [("claude", None), ("sh", Some("claude"))] {
            let base = Instant::now();
            let mut arb = Arbiter::settled("claude");
            let mut o = observed(command, "", AgentRecord::default());
            o.kind_hint = Some("claude".to_string());
            o.job_agent = job.map(str::to_string);
            o.below_typed_line = true;
            match decide_at(&o, base, 0, &mut arb) {
                Decision::Transition { from, to, rule, .. } => {
                    assert_eq!(
                        (from, to, rule),
                        (None, AgentState::Unknown, None),
                        "{command}: the successor was read off the title the last agent left"
                    );
                }
                other => panic!("{command}: expected unknown, got {other:?}"),
            }
            // The reads after it find the same title, still the last agent's, and still read none.
            let started = {
                let mut o = o.clone();
                o.prev = prev(Some("claude"), Some(AgentState::Unknown), false);
                o
            };
            for ms in [300, 3300, 6000] {
                assert_eq!(
                    decide_at(&started, base, ms, &mut arb),
                    Decision::Keep,
                    "{command}: read off the last agent's title {ms} ms on"
                );
            }
            // A title of its own is read.
            let mut drawn = started.clone();
            drawn.title = "\u{2733} its own task".to_string();
            match decide_at(&drawn, base, 6300, &mut arb) {
                Decision::Transition { to, rule, .. } => {
                    assert_eq!(
                        (to, rule.as_deref()),
                        (AgentState::Idle, Some("osc_title_idle")),
                        "{command}"
                    );
                }
                other => panic!("{command}: its own title was not read: {other:?}"),
            }
        }
    }

    #[test]
    fn only_a_hint_started_agent_whose_record_was_taken_off_is_a_replacement() {
        // A process that names its agent was identified as it came, and a pane that still has
        // its record is the watcher's own to carry on: neither is a new agent with the identity
        // the arbiter holds.
        let named = observed("codex", CODEX_AT_REST, AgentRecord::default());
        match decide_at(&named, Instant::now(), 0, &mut Arbiter::settled("codex")) {
            Decision::Transition { to, .. } => assert_eq!(to, AgentState::Idle),
            other => panic!("a process that names its agent is read at once: got {other:?}"),
        }
        let kept = hinted_codex(
            CODEX_AT_REST,
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        match decide_at(&kept, Instant::now(), 0, &mut Arbiter::settled("codex")) {
            Decision::Transition { to, .. } => assert_eq!(to, AgentState::Idle),
            other => panic!("a pane with its record is read at once: got {other:?}"),
        }
    }

    #[test]
    fn a_different_agent_in_a_pane_already_unknown_is_only_a_kind_update() {
        // Nothing about the pane's status moves, so nothing may be reported as moving.
        let mut arb = Arbiter::settled("codex");
        let o = observed(
            "claude",
            IDLE,
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        assert_eq!(
            decide_at(&o, Instant::now(), 0, &mut arb),
            Decision::KindChanged {
                kind: "claude".to_string()
            }
        );
    }

    #[test]
    fn a_hook_source_ends_the_grace() {
        // A hook reports the state itself. A grace left running would owe a read at its
        // end, and after it at every tick, for as long as the hook held the pane.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let o = observed("claude", IDLE, AgentRecord::default());
        let _ = decide_at(&o, base, 0, &mut arb);
        assert!(
            arb.forced_read().is_some(),
            "precondition: the grace is running"
        );
        let mut hooked = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        hooked.prev.source = Some("hook:claude-code".to_string());
        assert!(matches!(
            decide_at(&hooked, base, 500, &mut arb),
            Decision::HookAuthoritative { .. }
        ));
        assert_eq!(arb.forced_read(), None);
    }

    /// Reads `o` through the startup grace `decide_at` started at `from` ms: the read at
    /// the window's end, which is kept, then the one after it, whose decision is returned.
    fn after_the_grace(o: &Observed, base: Instant, from: u64, arb: &mut Arbiter) -> Decision {
        assert_eq!(decide_at(o, base, from + 3000, arb), Decision::Keep);
        decide_at(o, base, from + 3300, arb)
    }

    /// The title a Claude cut off mid-turn leaves on its pane: the last spinner frame of
    /// its turn. Codex (herdr master, 2026.10.01.1) reads that as its own turn, through
    /// `osc_title_working`, so it is a title a replacement Codex must not inherit. A Claude
    /// at its prompt leaves the `✳` that `observed` gives every pane, which Codex's
    /// `osc_title_idle` reads as its own idle: the other title it must not inherit.
    const SPINNER_LEFT_BEHIND: &str = "\u{280B} fake task";

    /// The spinner frame Codex's own turn sets: a different title, so tmux reports it as
    /// Codex's.
    const CODEX_SPINNER: &str = "\u{2819} project";

    #[test]
    fn a_replacement_agent_does_not_read_the_title_its_predecessor_left() {
        // herdr's `clear_osc_evidence_for_agent_transition`: Claude's turn is cut off
        // with a spinner frame as the pane's title, and Codex takes the pane without
        // changing it. Codex's `osc_title_working` would take that frame for Codex's own
        // turn, so the title reads as none until tmux reports another.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut replaced = observed(
            "codex",
            "\n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Working),
                Some("live_turn_working"),
            ),
        );
        replaced.title = SPINNER_LEFT_BEHIND.to_string();
        assert!(matches!(
            decide_at(&replaced, base, 0, &mut arb),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
        let mut unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        unknown.title = SPINNER_LEFT_BEHIND.to_string();
        match after_the_grace(&unknown, base, 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule, None, "Claude's spinner is not Codex's turn");
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
        // Codex sets a title of its own: that one is its evidence.
        let mut titled = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Idle), false),
        );
        titled.title = CODEX_SPINNER.to_string();
        match decide_at(&titled, base, 3600, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Working);
                assert_eq!(rule.as_deref(), Some("osc_title_working"));
            }
            other => panic!("Codex's own title is read: got {other:?}"),
        }
    }

    #[test]
    fn a_replacement_agent_does_not_read_the_idle_title_its_predecessor_left() {
        // herdr's `clear_osc_evidence_for_agent_transition`: Claude, at its prompt,
        // leaves `✳ fake task` as the pane's title and Codex takes the pane without
        // changing it. Codex's `osc_title_idle` takes any title that is not a spinner for
        // its own idle, so the title reads as none until tmux reports another.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let replaced = observed(
            "codex",
            "\n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("live_prompt_box"),
            ),
        );
        assert!(matches!(
            decide_at(&replaced, base, 0, &mut arb),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
        let unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        match after_the_grace(&unknown, base, 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule, None, "Claude's title is not Codex's idle");
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
        // Codex sets a title of its own: that one is its evidence.
        let mut titled = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Idle), false),
        );
        titled.title = "codex".to_string();
        assert_eq!(
            decide_at(&titled, base, 3600, &mut arb),
            Decision::RuleChanged {
                rule: Some("osc_title_idle".to_string())
            }
        );
    }

    #[test]
    fn a_first_acquisition_keeps_the_idle_title_its_own_process_printed() {
        // Nothing was identified in the pane before, so the title there is this agent's
        // own, printed before the watcher recognised it (herdr v0.9.0, #3326).
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let _ = decide_at(
            &observed("codex", "\n", AgentRecord::default()),
            base,
            0,
            &mut arb,
        );
        let unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        match after_the_grace(&unknown, base, 0, &mut arb) {
            Decision::Transition { rule, .. } => {
                assert_eq!(rule.as_deref(), Some("osc_title_idle"));
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    #[test]
    fn a_first_acquisition_keeps_the_title_its_own_process_printed() {
        // Nothing was identified in the pane before, so the title there is this agent's
        // own, printed before the watcher recognised it (herdr v0.9.0, #3326).
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let mut first = observed("codex", "\n", AgentRecord::default());
        first.title = CODEX_SPINNER.to_string();
        let _ = decide_at(&first, base, 0, &mut arb);
        let mut unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        unknown.title = CODEX_SPINNER.to_string();
        match after_the_grace(&unknown, base, 0, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Working);
                assert_eq!(rule.as_deref(), Some("osc_title_working"));
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    #[test]
    fn a_replacement_agent_does_not_read_the_progress_its_predecessor_left() {
        // Codex's last `9;4;0` is still the byte stream's progress, and `taw agent start`
        // has already recorded Claude as the pane's agent, so there is no grace: this very
        // read is Claude's first evaluation. Claude's `osc_progress_idle` would take
        // Codex's progress for Claude's own idle.
        let mut o = observed(
            "claude",
            "\n\n",
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        o.title = String::new();
        o.osc_progress = "4;0".to_string();
        let mut arb = Arbiter::settled("codex");
        assert_eq!(decide_at(&o, Instant::now(), 0, &mut arb), Decision::Keep);
        // The watcher is told once to clear the byte stream's progress.
        assert!(arb.take_progress_reset());
        assert!(!arb.take_progress_reset());
        // A watcher that had identified nothing takes the same progress as Claude's own.
        let mut first = Arbiter::default();
        assert_eq!(
            decide_at(&o, Instant::now(), 0, &mut first),
            Decision::RuleChanged {
                rule: Some("osc_progress_idle".to_string())
            }
        );
        assert!(!first.take_progress_reset(), "nothing to forget");
    }

    #[test]
    fn an_agent_that_left_the_pane_leaves_no_evidence_for_the_next_one() {
        // herdr drops the evidence whenever the pane changes away from an identified
        // agent, to another or to none. Claude is cut off mid-turn and the shell comes back
        // with the turn's spinner frame still the title, and Codex starts later without
        // setting one: that title is still Claude's.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut shell = observed(
            "zsh",
            "$ \n",
            prev(Some("claude"), Some(AgentState::Idle), true),
        );
        shell.title = SPINNER_LEFT_BEHIND.to_string();
        assert_eq!(
            decide_at(&shell, base, 0, &mut arb),
            Decision::NoAgent { clear: true }
        );
        let mut started = observed("codex", "\n", AgentRecord::default());
        started.title = SPINNER_LEFT_BEHIND.to_string();
        let _ = decide_at(&started, base, 1000, &mut arb);
        let mut unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        unknown.title = SPINNER_LEFT_BEHIND.to_string();
        match after_the_grace(&unknown, base, 1000, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule, None, "the title Claude left is not Codex's");
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    #[test]
    fn an_agent_that_left_the_pane_leaves_no_idle_title_for_the_next_one() {
        // herdr drops the evidence whenever the pane changes away from an identified
        // agent, to another or to none. Claude exits to the shell with its `✳` title still
        // up, and Codex starts later without setting one: that title is still Claude's.
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        assert_eq!(
            decide_at(
                &observed(
                    "zsh",
                    "$ \n",
                    prev(Some("claude"), Some(AgentState::Idle), true)
                ),
                base,
                0,
                &mut arb
            ),
            Decision::NoAgent { clear: true }
        );
        let _ = decide_at(
            &observed("codex", "\n", AgentRecord::default()),
            base,
            1000,
            &mut arb,
        );
        let unknown = observed(
            "codex",
            "\n",
            prev(Some("codex"), Some(AgentState::Unknown), false),
        );
        match after_the_grace(&unknown, base, 1000, &mut arb) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule, None, "the title Claude left is not Codex's");
            }
            other => panic!("the grace is over, so the screen decides: got {other:?}"),
        }
    }

    #[test]
    fn a_name_goes_with_the_agent_it_was_given_to() {
        // herdr's `reconcile_agent_name_owner`: Claude named `reviewer` becomes Codex,
        // and `reviewer` is not Codex's name.
        assert!(name_leaves_with(Some("claude"), "codex", None));
        // The same agent keeps it, and so does the first agent recorded in a pane (a
        // restored session writes the name before the agent is detected).
        assert!(!name_leaves_with(Some("claude"), "claude", None));
        assert!(!name_leaves_with(None, "codex", None));
        // herdr's managed agent: `taw agent start` set the pane up for Codex (its kind
        // hint) and writes Codex's name itself, so the watcher leaves the name to it —
        // even when it reads the pane between the hint and the name. A hint spelled as an
        // alias names its agent all the same.
        assert!(!name_leaves_with(Some("claude"), "codex", Some("codex")));
        assert!(!name_leaves_with(
            Some("codex"),
            "claude",
            Some("claude-code")
        ));
        assert!(name_leaves_with(Some("claude"), "codex", Some("gemini")));
    }

    #[test]
    fn a_watcher_restarted_on_a_classified_pane_has_no_grace() {
        // Its first read is a first sighting for the watcher, not for the pane: tmux says
        // what the agent was doing, and a restart must not blank that out for 3 s.
        let o = observed(
            "claude",
            WORKING,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Working),
                Some("live_turn_working"),
            ),
        );
        let mut arb = Arbiter::default();
        assert_eq!(decide_at(&o, Instant::now(), 0, &mut arb), Decision::Keep);
        // `taw agent start` writes `@agent_kind` itself, before the watcher has looked:
        // a kind with no state is still a new agent.
        let started = observed("claude", IDLE, prev(Some("claude"), None, false));
        assert!(matches!(
            decide_at(&started, Instant::now(), 0, &mut Arbiter::default()),
            Decision::Transition {
                to: AgentState::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn working_to_idle_in_the_background_becomes_done_when_the_pane_is_unseen() {
        let o = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        match decide_on(&o) {
            Decision::Transition { from, to, seen, .. } => {
                assert_eq!(from, Some(AgentState::Working));
                assert_eq!(to, AgentState::Done);
                assert!(!seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn working_to_idle_in_a_visible_pane_stays_idle_and_records_it_as_seen() {
        let mut o = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        o.window_active = true;
        o.session_attached = 1;
        match decide_on(&o) {
            Decision::Transition { to, seen, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert!(seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_turn_that_ends_in_the_background_is_done_even_if_the_pane_was_seen_mid_turn() {
        // herdr's completion rule: whether the user saw a turn end is decided when it
        // ends, by where they are looking then. `@agent_seen = 1` from a glance earlier in
        // the turn does not make a finish behind their back an `idle`.
        let o = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), true),
        );
        match decide_on(&o) {
            Decision::Transition { to, seen, .. } => {
                assert_eq!(to, AgentState::Done);
                assert!(!seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_transition_writes_seen_as_the_completion_rule_decides_it() {
        for foreground in [false, true] {
            // A new turn is unseen whoever is watching, and a `done` is unseen by
            // definition — including after a glance that left `@agent_seen = 1`.
            assert_eq!(seen_written(AgentState::Working, foreground), Some(false));
            assert_eq!(seen_written(AgentState::Done, foreground), Some(false));
            // Blocked and unknown leave the flag as it was: attention tiers read it.
            assert_eq!(seen_written(AgentState::Blocked, foreground), None);
            assert_eq!(seen_written(AgentState::Unknown, foreground), None);
        }
        // An `idle` arrival the user is looking at is seen; one behind their back that is
        // not a completion (an agent becoming readable) leaves the flag alone.
        assert_eq!(seen_written(AgentState::Idle, true), Some(true));
        assert_eq!(seen_written(AgentState::Idle, false), None);
    }

    #[test]
    fn notification_classes_follow_the_spec_table() {
        use AgentState::*;
        assert_eq!(
            notification_class(Some(Working), Blocked),
            Some(Class::Request)
        );
        assert_eq!(notification_class(None, Blocked), Some(Class::Request));
        assert_eq!(notification_class(Some(Working), Idle), Some(Class::Done));
        assert_eq!(notification_class(Some(Working), Done), Some(Class::Done));
        // herdr's completions are `idle` after `working` *or* `blocked`.
        assert_eq!(notification_class(Some(Blocked), Idle), Some(Class::Done));
        assert_eq!(notification_class(Some(Blocked), Done), Some(Class::Done));
        // Not every arrival at idle is work finishing: an agent becoming readable
        // (`unknown`, herdr v0.9.2, #4457) or found at its prompt is not.
        assert_eq!(notification_class(Some(Unknown), Idle), None);
        assert_eq!(notification_class(None, Idle), None);
        assert_eq!(notification_class(Some(Idle), Working), None);
        assert_eq!(notification_class(Some(Done), Idle), None);
        // `unknown` is the watcher admitting it cannot tell, which is never news worth
        // interrupting anyone for — including from `working`, where the turn did not
        // finish, it stopped being observable.
        for from in [None, Some(Idle), Some(Working), Some(Blocked), Some(Done)] {
            assert_eq!(
                notification_class(from, Unknown),
                None,
                "a transition to unknown must not toast (from {from:?})"
            );
        }
    }

    #[test]
    fn a_finish_the_user_watched_earns_no_done_notification() {
        use AgentState::*;
        let done = |state| {
            Some(Notice {
                class: Class::Done,
                state,
                exit: false,
            })
        };
        let rule = Some("live_prompt_box");
        // Behind their back: news, delivered if it still stands when the delay is up.
        assert_eq!(notice(Some(Working), Done, false, rule), done(Done));
        assert_eq!(notice(Some(Blocked), Done, false, rule), done(Done));
        // In front of them, as it happened: not news, whatever happens next.
        assert_eq!(notice(Some(Working), Idle, true, rule), None);
        assert_eq!(notice(Some(Blocked), Idle, true, rule), None);
        // A request is left to the check when its delay is up.
        let request = Some(Notice {
            class: Class::Request,
            state: Blocked,
            exit: false,
        });
        let prompt = Some("bash_permission_prompt");
        assert_eq!(notice(Some(Working), Blocked, true, prompt), request);
        assert_eq!(notice(Some(Working), Blocked, false, prompt), request);
    }

    #[test]
    fn an_exits_done_still_stands_once_its_record_is_cleared() {
        use AgentState::*;
        // The exit's `done` is written, and the record cleared a tick later.
        let exit = notice(Some(Working), Done, false, Some(PROCESS_EXITED_RULE)).unwrap();
        assert!(exit.exit);
        assert!(exit.stands(Some("done"), || false));
        assert!(exit.stands(None, || false), "cleared behind it");
        assert!(exit.stands(Some(""), || false), "cleared behind it");
        // A new agent started in the pane in between: its grace's `unknown`.
        assert!(!exit.stands(Some("unknown"), || false));
        assert!(!exit.stands(Some("working"), || true));
        // The pane of a hint-started agent is not cleared, and reads `unknown` once the exit is
        // written, for as long as the process recorded for the agent is dead.
        assert!(exit.stands(Some("unknown"), || true));
        // In front of the user the exit's idle was seen: nothing to deliver at all.
        assert_eq!(
            notice(Some(Working), Idle, true, Some(PROCESS_EXITED_RULE)),
            None
        );
        // A finish that is not an exit does not survive a clear.
        let finish = notice(Some(Working), Done, false, Some("live_prompt_box")).unwrap();
        assert!(!finish.stands(None, || true));
        assert!(finish.stands(Some("done"), || true));
        assert!(
            !finish.stands(Some("idle"), || true),
            "released by `taw seen`"
        );
        assert!(
            !finish.stands(Some("unknown"), || true),
            "only an exit outlives its state"
        );
    }

    #[test]
    fn a_transition_writes_its_state_its_kind_when_new_and_the_seen_flag_it_redefines() {
        let prev = AgentRecord {
            name: Some("reviewer".to_string()),
            ..prev_with_rule(
                Some("claude"),
                Some(AgentState::Working),
                Some("live_turn_working"),
            )
        };
        let finish = Decision::Transition {
            kind: "claude".to_string(),
            from: Some(AgentState::Working),
            to: AgentState::Done,
            rule: Some("live_prompt_box".to_string()),
            detail: String::new(),
            seen: false,
            ready: false,
        };
        assert_eq!(
            update(&prev, &finish),
            Update {
                state: Some((AgentState::Done, Some("live_prompt_box".to_string()))),
                seen: Some(false),
                notice: Some(Notice {
                    class: Class::Done,
                    state: AgentState::Done,
                    exit: false,
                }),
                ..Update::default()
            },
            "the same agent: its kind and its name stand"
        );
        // Codex taking the pane: the kind is written, and Claude's name goes with Claude.
        let codex = Decision::Transition {
            kind: "codex".to_string(),
            from: Some(AgentState::Working),
            to: AgentState::Unknown,
            rule: None,
            detail: String::new(),
            seen: false,
            ready: false,
        };
        let taken = update(&prev, &codex);
        assert_eq!(taken.kind.as_deref(), Some("codex"));
        assert!(taken.release_name);
        assert_eq!(taken.seen, None, "unknown leaves the flag alone");
        assert_eq!(taken.notice, None, "unknown never toasts");
    }

    #[test]
    fn the_other_decisions_write_only_what_they_are_about() {
        let prev = prev_with_rule(
            Some("claude"),
            Some(AgentState::Idle),
            Some("live_prompt_box"),
        );
        assert_eq!(update(&prev, &Decision::Keep), Update::default());
        assert_eq!(
            update(&prev, &Decision::NoAgent { clear: true }),
            Update {
                clear: true,
                ..Update::default()
            }
        );
        assert_eq!(
            update(&prev, &Decision::NoAgent { clear: false }),
            Update::default()
        );
        assert_eq!(
            update(&prev, &Decision::RuleChanged { rule: None }),
            Update {
                rule: Some(None),
                ..Update::default()
            }
        );
        // Another agent recorded over this one takes what its hook reported away with it.
        let codex = Update {
            kind: Some("codex".to_string()),
            release_identity: true,
            ..Update::default()
        };
        assert_eq!(
            update(
                &prev,
                &Decision::KindChanged {
                    kind: "codex".to_string()
                }
            ),
            codex
        );
        assert_eq!(
            update(
                &prev,
                &Decision::HookAuthoritative {
                    kind: Some("codex".to_string())
                }
            ),
            codex
        );
        // A hook's own kind stands when the watcher knows none, or the same one.
        for kind in [None, Some("claude".to_string())] {
            assert_eq!(
                update(&prev, &Decision::HookAuthoritative { kind }),
                Update::default()
            );
        }
    }

    #[test]
    fn what_an_agents_hook_reported_goes_when_another_agent_takes_the_pane() {
        // The Letta an npm install runs under `node` reports its conversation, and a Claude
        // that starts in the same pane is a different agent: the record follows it, and carries
        // neither the conversation nor the word that Letta was there.
        let letta = named_itself("letta", Some(AgentState::Idle));
        let claude = |decision: Decision| update(&letta, &decision);
        let took = |update: Update| (update.kind.clone(), update.release_identity);
        assert_eq!(
            took(claude(Decision::KindChanged {
                kind: "claude".to_string()
            })),
            (Some("claude".to_string()), true)
        );
        assert_eq!(
            took(claude(Decision::Transition {
                kind: "claude".to_string(),
                from: Some(AgentState::Idle),
                to: AgentState::Unknown,
                rule: None,
                detail: String::new(),
                seen: false,
                ready: false,
            })),
            (Some("claude".to_string()), true)
        );
        // Nothing to release when there was no other agent: a first sighting records a kind,
        // and the same agent read again records nothing.
        let first = update(
            &AgentRecord::default(),
            &Decision::KindChanged {
                kind: "claude".to_string(),
            },
        );
        assert!(!first.release_identity);
        assert_eq!(
            update(
                &letta,
                &Decision::HookAuthoritative {
                    kind: Some("letta".to_string())
                }
            ),
            Update::default()
        );
    }

    #[test]
    fn taw_seen_marks_an_agent_seen_and_ends_its_done() {
        let done = prev_with_rule(
            Some("claude"),
            Some(AgentState::Done),
            Some("live_prompt_box"),
        );
        let seen = marked_seen(&done).unwrap();
        assert!(seen.seen);
        assert_eq!(seen.state, Some(AgentState::Idle));
        assert_eq!(seen.rule.as_deref(), Some("live_prompt_box"), "rule kept");
        let working = prev(Some("claude"), Some(AgentState::Working), false);
        let seen = marked_seen(&working).unwrap();
        assert!(seen.seen);
        assert_eq!(seen.state, Some(AgentState::Working));
        assert!(marked_seen(&AgentRecord::default()).is_none(), "no agent");
    }

    #[test]
    fn blocked_to_idle_is_a_completion_decided_by_where_the_user_is_looking() {
        // herdr's `is_background_completion_transition`: `idle` after `blocked` ends the
        // agent's work as surely as `idle` after `working`. Behind the user's back it is
        // `done`; in front of them, `idle` they have seen.
        let away = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Blocked), true),
        );
        match decide_on(&away) {
            Decision::Transition { from, to, .. } => {
                assert_eq!(from, Some(AgentState::Blocked));
                assert_eq!(to, AgentState::Done);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
        let mut looking = away.clone();
        looking.window_active = true;
        looking.session_attached = 1;
        match decide_on(&looking) {
            Decision::Transition { to, seen, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert!(seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_finish_while_the_users_terminal_has_lost_focus_is_done() {
        // herdr's `seen` at a completion is the active tab *and* an outer terminal not
        // reported unfocused: the pane's window in front of the user does not count while
        // the terminal it is in has lost focus. Focus nobody can tell counts as looking.
        let mut o = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        o.window_active = true;
        o.session_attached = 1;
        o.terminal_focus = Some(false);
        match decide_on(&o) {
            Decision::Transition { to, seen, .. } => {
                assert_eq!(to, AgentState::Done);
                assert!(!seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
        for focus in [Some(true), None] {
            o.terminal_focus = focus;
            match decide_on(&o) {
                Decision::Transition { to, seen, .. } => {
                    assert_eq!(to, AgentState::Idle, "{focus:?}");
                    assert!(seen, "{focus:?}");
                }
                other => panic!("expected a transition, got {other:?}"),
            }
        }

        // The user comes back to the terminal: the read that finds it focused again ends
        // the done in front of them.
        let mut back = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Done),
                Some("live_prompt_box"),
            ),
        );
        back.window_active = true;
        back.session_attached = 1;
        back.terminal_focus = Some(false);
        assert_eq!(decide_on(&back), Decision::Keep, "still in another app");
        back.terminal_focus = Some(true);
        match decide_on(&back) {
            Decision::Transition { from, to, seen, .. } => {
                assert_eq!(from, Some(AgentState::Done));
                assert_eq!(to, AgentState::Idle);
                assert!(seen);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn done_stays_done_until_the_pane_has_been_seen() {
        // Still unseen: the idle screen must not quietly undo `done`. The rule the
        // screen matches is already recorded, so there is nothing at all to write.
        let unseen = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Done),
                Some("live_prompt_box"),
            ),
        );
        assert_eq!(decide_on(&unseen), Decision::Keep);

        // Once seen, the same screen releases it back to idle.
        let seen = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Done), true),
        );
        match decide_on(&seen) {
            Decision::Transition { from, to, .. } => {
                assert_eq!(from, Some(AgentState::Done));
                assert_eq!(to, AgentState::Idle);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_done_the_user_is_looking_at_becomes_an_idle_they_have_seen() {
        // The finish lands as the user switches to the pane: the read that wrote `done`
        // saw it in the background, and `taw seen` from the focus hook ran while it was
        // still `working`, so the `done` write undid its mark. The next read finds them
        // looking at it — which is what ends a `done` — and writes the `idle` they see.
        let mut o = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Done),
                Some("live_prompt_box"),
            ),
        );
        o.window_active = true;
        o.session_attached = 1;
        match decide_on(&o) {
            Decision::Transition { from, to, seen, .. } => {
                assert_eq!(from, Some(AgentState::Done));
                assert_eq!(to, AgentState::Idle);
                assert!(seen, "the idle they are looking at is seen");
            }
            other => panic!("a done in front of the user is released, got {other:?}"),
        }
        let released = update(
            &o.prev,
            &Decision::Transition {
                kind: "claude".to_string(),
                from: Some(AgentState::Done),
                to: AgentState::Idle,
                rule: Some("live_prompt_box".to_string()),
                detail: String::new(),
                seen: true,
                ready: false,
            },
        );
        assert_eq!(released.seen, Some(true));
        assert_eq!(released.notice, None, "done -> idle is not news");
    }

    #[test]
    fn a_skip_state_update_rule_keeps_the_previous_state() {
        let o = observed(
            "claude",
            MODEL_PICKER,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        assert_eq!(decide_on(&o), Decision::Keep);
    }

    #[test]
    fn an_unchanged_state_is_kept() {
        // The recorded rule has to match what the screen evaluates to, or the evidence
        // is what moved and the answer is `RuleChanged` — see the tests for that above.
        let o = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("live_prompt_box"),
            ),
        );
        assert_eq!(decide_on(&o), Decision::Keep);
        let working = observed(
            "claude",
            WORKING,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Working),
                Some("live_turn_working"),
            ),
        );
        assert_eq!(decide_on(&working), Decision::Keep);
    }

    #[test]
    fn an_unchanged_state_whose_evidence_moved_refreshes_the_rule_alone() {
        // The pane was recorded `idle` before the agent drew anything, so no rule was
        // matched. The drawn screen matches `live_prompt_box` and is still idle: the
        // state does not move, but the explanation for it now exists.
        let o = observed(
            "claude",
            IDLE,
            prev_with_rule(Some("claude"), Some(AgentState::Idle), None),
        );
        assert_eq!(
            decide_on(&o),
            Decision::RuleChanged {
                rule: Some("live_prompt_box".to_string())
            }
        );
    }

    #[test]
    fn an_unchanged_state_with_unchanged_evidence_is_still_just_kept() {
        let o = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("live_prompt_box"),
            ),
        );
        assert_eq!(decide_on(&o), Decision::Keep);
    }

    #[test]
    fn evidence_that_disappears_is_refreshed_to_no_rule_rather_than_left_stale() {
        // An empty screen matches nothing in claude.toml and evaluates to a bare idle. A
        // pane sitting at `idle` with a rule recorded from an earlier screen must stop
        // claiming that rule explains what is there now.
        let mut o = observed(
            "claude",
            "\n\n",
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Idle),
                Some("live_prompt_box"),
            ),
        );
        o.title = String::new();
        assert_eq!(decide_on(&o), Decision::RuleChanged { rule: None });
    }

    #[test]
    fn a_changed_kind_takes_precedence_over_refreshing_the_rule() {
        // Both are stale; the kind is the one that is written now. The next look sees
        // `prev.kind` agreeing and refreshes the rule then, so nothing is lost.
        let o = observed(
            "claude",
            IDLE,
            prev_with_rule(Some("codex"), Some(AgentState::Idle), None),
        );
        assert_eq!(
            decide_on(&o),
            Decision::KindChanged {
                kind: "claude".to_string()
            }
        );
    }

    #[test]
    fn a_skip_state_update_rule_does_not_touch_the_evidence_either() {
        // An overlay hides the real state rather than replacing it, so the rule that
        // explains the hidden state must survive the overlay too.
        let o = observed(
            "claude",
            MODEL_PICKER,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Working),
                Some("live_turn_working"),
            ),
        );
        assert_eq!(decide_on(&o), Decision::Keep);
    }

    #[test]
    fn a_changed_kind_with_an_unchanged_state_is_a_kind_update_not_a_transition() {
        let o = observed(
            "claude",
            IDLE,
            prev(Some("codex"), Some(AgentState::Idle), false),
        );
        assert_eq!(
            decide_on(&o),
            Decision::KindChanged {
                kind: "claude".to_string()
            }
        );
    }

    #[test]
    fn a_changed_kind_with_a_changed_state_is_still_a_transition() {
        let o = observed(
            "claude",
            IDLE,
            prev(Some("codex"), Some(AgentState::Working), false),
        );
        match decide_on(&o) {
            Decision::Transition { kind, from, to, .. } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, Some(AgentState::Working));
                // Background and unseen, so the finished turn is `done`.
                assert_eq!(to, AgentState::Done);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_blocked_transition_details_the_first_non_empty_line_of_the_matched_region() {
        let o = observed(
            "claude",
            BLOCKED,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        match decide_on(&o) {
            Decision::Transition { to, detail, .. } => {
                assert_eq!(to, AgentState::Blocked);
                assert_eq!(detail, "\u{23fa} Bash(rm -rf build/)");
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_done_detail_is_the_last_line_claude_said_not_its_empty_input_caret() {
        // A real Claude's idle screen: the reply, a rule, the empty input caret, a rule and
        // the hint line. The idle rule looks at the input box, whose only line is the
        // caret, so the toast said "claude done, e2e-real:0, caret".
        let o = observed(
            "claude",
            IDLE,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        match decide_on(&o) {
            Decision::Transition {
                to, rule, detail, ..
            } => {
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
                assert_eq!(
                    detail,
                    "\u{23fa} Done. The tests pass and the change is committed."
                );
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn a_done_detail_is_empty_when_all_the_screen_holds_is_the_input_box() {
        let rule = "\u{2500}".repeat(70);
        let screen = format!("\n\n{rule}\n\u{276f} \n{rule}\n  ? for shortcuts\n");
        let o = observed(
            "claude",
            &screen,
            prev(Some("claude"), Some(AgentState::Working), false),
        );
        match decide_on(&o) {
            Decision::Transition {
                to, rule, detail, ..
            } => {
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("live_prompt_box"));
                assert_eq!(detail, "");
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn what_is_only_an_input_caret_or_a_box_line_is_not_a_detail() {
        for line in [
            "\u{276f}",
            "  \u{276f} ",
            ">",
            "\u{2502} > \u{2502}",
            "\u{256d}\u{2500}\u{2500}\u{256e}",
            "\u{2570}\u{2500}\u{2500}\u{256f}",
            "\u{2500}\u{2500}\u{2500}",
        ] {
            assert!(is_chrome_line(line), "{line:?} is chrome");
        }
        for line in [
            "\u{276f} fix the bug",
            "\u{23fa} ok",
            "> quoted text",
            "Done",
        ] {
            assert!(!is_chrome_line(line), "{line:?} is text");
        }
    }

    #[test]
    fn a_detail_falls_back_to_the_pane_title_when_no_rule_matched() {
        // An empty screen matches nothing in claude.toml, and the title is not a
        // spinner, so the evaluation is a bare idle with no matched rule.
        let mut o = observed("claude", "\n\n", AgentRecord::default());
        o.title = "  a plain title  ".to_string();
        match decide_on(&o) {
            Decision::Transition { rule, detail, .. } => {
                assert_eq!(rule, None);
                assert_eq!(detail, "a plain title");
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn the_osc_progress_from_the_byte_stream_reaches_the_engine() {
        // `osc_progress_idle` matches `^4;0` in the osc_progress region: an empty screen
        // plus a progress payload still resolves to idle *by that rule*, which proves the
        // byte-stream field is threaded into the snapshot.
        let mut o = observed("claude", "\n\n", AgentRecord::default());
        o.title = String::new();
        o.osc_progress = "4;0".to_string();
        match decide_on(&o) {
            Decision::Transition { rule, to, .. } => {
                assert_eq!(rule.as_deref(), Some("osc_progress_idle"));
                assert_eq!(to, AgentState::Idle);
            }
            other => panic!("expected a transition, got {other:?}"),
        }
    }

    #[test]
    fn the_manifest_cache_is_reused_across_observations() {
        let store = bundled_only();
        let mut cache = HashMap::new();
        let o = observed("claude", IDLE, AgentRecord::default());
        let _ = decide(
            &store,
            &mut cache,
            &o,
            false,
            Instant::now(),
            &mut Arbiter::settled("claude"),
        );
        assert!(cache.contains_key("claude"));
        assert_eq!(cache.len(), 1);
        let _ = decide(
            &store,
            &mut cache,
            &o,
            false,
            Instant::now(),
            &mut Arbiter::settled("claude"),
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn an_exited_hint_only_agent_goes_unknown_even_though_its_screen_still_matches() {
        // The agent this hint named has exited (`agent_exited = true`), but its last screen
        // still carries an idle prompt box that matches a rule. A single look cannot tell a
        // stale screen from a live one — the dead pid can, so the pane must read `unknown`
        // with no rule, not the `idle` its frozen screen would otherwise show. The hint is
        // kept: `decide` never clears it, so the identity (kind) survives into the unknown.
        let mut o = observed(
            "sh",
            IDLE,
            prev(Some("claude"), Some(AgentState::Idle), false),
        );
        o.kind_hint = Some("claude".to_string());
        // Precondition: with the agent still alive, that same screen is a plain `idle`.
        match decide(
            &bundled_only(),
            &mut HashMap::new(),
            &o,
            false,
            Instant::now(),
            &mut Arbiter::settled("claude"),
        ) {
            Decision::Keep | Decision::RuleChanged { .. } => {}
            Decision::Transition { to, .. } => assert_eq!(to, AgentState::Idle),
            other => panic!("a live hint-only agent on an idle screen is idle, got {other:?}"),
        }
        // Once it has exited, the same screen is `unknown` with the rule dropped.
        match decide(
            &bundled_only(),
            &mut HashMap::new(),
            &o,
            true,
            Instant::now(),
            &mut Arbiter::settled("claude"),
        ) {
            Decision::Transition { to, rule, kind, .. } => {
                assert_eq!(to, AgentState::Unknown);
                assert_eq!(rule, None, "no rule can explain a stale screen");
                assert_eq!(kind, "claude", "the hint's identity is kept into unknown");
            }
            other => panic!("an exited hint-only agent must go unknown, got {other:?}"),
        }
    }

    /// A hint-only claude whose recorded process is gone, on `screen`, with the state `prev` holds.
    fn hint_only_exited(screen: &str, state: AgentState, foreground: bool) -> Observed {
        let mut o = observed(
            "sh",
            screen,
            prev_with_rule(Some("claude"), Some(state), Some("live_turn_working")),
        );
        o.kind_hint = Some("claude".to_string());
        o.window_active = foreground;
        o.session_attached = u32::from(foreground);
        o
    }

    /// The read of a hint-only pane whose agent's process is `exited` (or not), by a watcher that
    /// has watched it for a while.
    fn decide_exited(o: &Observed, exited: bool) -> Decision {
        decide(
            &bundled_only(),
            &mut HashMap::new(),
            o,
            exited,
            Instant::now(),
            &mut Arbiter::settled("claude"),
        )
    }

    /// An agent `taw agent start` started that dies mid-turn leaves its last frame on the pane, with
    /// "esc to interrupt" on it, and the rules of its kind read that as a turn under way. A frame
    /// caught mid-redraw is why a hint-only pane is not taken to `unknown` while it is `working`,
    /// and a dead process is no mid-redraw: the pane stayed `working` for good, no `done` was
    /// announced, and everything waiting for the agent waited for ever. The turn ends as the turn of
    /// an agent a process names ends when that process does (`shell_returned`): an `idle` with the
    /// rule `process_exited`, `done` while nobody is looking.
    #[test]
    fn a_hint_only_agent_whose_process_exited_mid_turn_ends_its_turn_as_any_exit_does() {
        let o = hint_only_exited(WORKING, AgentState::Working, false);
        // The agent alive, that frame is the turn under way, and nothing is written.
        assert!(
            matches!(
                decide_exited(&o, false),
                Decision::Keep | Decision::RuleChanged { .. }
            ),
            "precondition: a live agent on that frame is working"
        );
        match decide_exited(&o, true) {
            Decision::Transition {
                kind,
                from,
                to,
                rule,
                ..
            } => {
                assert_eq!(kind, "claude");
                assert_eq!(from, Some(AgentState::Working));
                assert_eq!(to, AgentState::Done, "background and unseen, as any exit's");
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("the exit of a working agent must be written, got {other:?}"),
        }
        // Watched as it ended, it is `idle` and no news, as an exit in front of the user is.
        let watched = hint_only_exited(WORKING, AgentState::Working, true);
        match decide_exited(&watched, true) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Idle);
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("expected the exit's idle, got {other:?}"),
        }
    }

    /// A turn that stops at a question ends with the process as well: what is under way is not only
    /// `working`.
    #[test]
    fn a_hint_only_agent_whose_process_exited_at_a_question_ends_its_turn_too() {
        let o = hint_only_exited(BLOCKED, AgentState::Blocked, false);
        match decide_exited(&o, true) {
            Decision::Transition { from, to, rule, .. } => {
                assert_eq!(from, Some(AgentState::Blocked));
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("the exit of a blocked agent must be written, got {other:?}"),
        }
    }

    /// The last frame of a dead agent says nothing, whatever it shows: an overlay that keeps the
    /// state of a live agent (`skip_state_update`) must not keep that of one that has gone.
    #[test]
    fn what_a_dead_agents_last_frame_shows_does_not_keep_it_working() {
        let o = hint_only_exited(MODEL_PICKER, AgentState::Working, false);
        assert_eq!(
            decide_exited(&o, false),
            Decision::Keep,
            "precondition: for a live agent the overlay keeps the state"
        );
        match decide_exited(&o, true) {
            Decision::Transition { to, rule, .. } => {
                assert_eq!(to, AgentState::Done);
                assert_eq!(rule.as_deref(), Some("process_exited"));
            }
            other => panic!("the exit must be written over an overlay, got {other:?}"),
        }
    }

    /// The exit is written once, and the read after it finds `done` or `idle` over an agent that is
    /// not there: `unknown`, which is what a pane whose agent has left reads while its hint stays.
    #[test]
    fn the_read_after_a_hint_only_exit_finds_the_pane_unknown() {
        for state in [AgentState::Done, AgentState::Idle] {
            let mut o = hint_only_exited(WORKING, state, false);
            o.prev.rule = Some("process_exited".to_string());
            match decide_exited(&o, true) {
                Decision::Transition {
                    kind,
                    from,
                    to,
                    rule,
                    ..
                } => {
                    assert_eq!(kind, "claude", "the hint's identity is kept");
                    assert_eq!(from, Some(state));
                    assert_eq!(to, AgentState::Unknown);
                    assert_eq!(rule, None);
                }
                other => panic!("expected unknown from {state:?}, got {other:?}"),
            }
        }
        // And nothing is written again for a pane that already is.
        let mut unknown = hint_only_exited(WORKING, AgentState::Unknown, false);
        unknown.prev.rule = None;
        assert_eq!(decide_exited(&unknown, true), Decision::Keep);
        // Only the rule of what it left, if the pane still holds one, and the kind the hint names.
        unknown.prev.rule = Some("live_turn_working".to_string());
        assert_eq!(
            decide_exited(&unknown, true),
            Decision::RuleChanged { rule: None }
        );
        unknown.prev.rule = None;
        unknown.prev.kind = Some("codex".to_string());
        assert_eq!(
            decide_exited(&unknown, true),
            Decision::KindChanged {
                kind: "claude".to_string()
            }
        );
    }

    /// A live hint-only agent whose frame was caught mid-redraw keeps its turn: the exception for
    /// `working` is for an agent whose process is there.
    #[test]
    fn a_live_hint_only_agent_on_a_half_drawn_frame_keeps_its_turn() {
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut o = hint_only_exited("\n\n", AgentState::Working, false);
        o.title = String::new();
        for ms in [0, 100, 200] {
            assert_eq!(decide_at(&o, base, ms, &mut arb), Decision::Keep);
        }
    }

    /// A plain idle after `working` is held for three more reads, and an exit that comes while it is
    /// held ends the hold with the rest of what was carried for the agent: nothing is left to confirm.
    #[test]
    fn the_exit_of_an_agent_ends_a_hold_that_was_waiting_on_its_screen() {
        let base = Instant::now();
        let mut arb = Arbiter::settled("claude");
        let mut o = hint_only_exited("\n\n", AgentState::Working, false);
        o.title = String::new();
        assert_eq!(decide_at(&o, base, 0, &mut arb), Decision::Keep);
        assert!(arb.hold_active(), "precondition: the idle is held");
        let _ = decide(
            &bundled_only(),
            &mut HashMap::new(),
            &o,
            true,
            base,
            &mut arb,
        );
        assert!(!arb.hold_active());
        // At rest, too: the hold of a turn that ended is not what an exit has to wait for.
        let mut arb = Arbiter::settled("claude");
        assert_eq!(decide_at(&o, base, 0, &mut arb), Decision::Keep);
        let at_rest = hint_only_exited("\n\n", AgentState::Idle, false);
        let _ = decide(
            &bundled_only(),
            &mut HashMap::new(),
            &at_rest,
            true,
            base,
            &mut arb,
        );
        assert!(!arb.hold_active());
    }

    #[test]
    fn an_exits_completion_is_the_status_the_agent_left_with_whoever_names_it() {
        use AgentState::*;
        let exit = Some(PROCESS_EXITED_RULE);
        assert_eq!(left_as(Some(Working), Done, exit, true), Some(Done));
        assert_eq!(left_as(Some(Working), Idle, exit, true), Some(Idle));
        assert_eq!(left_as(Some(Blocked), Done, exit, false), Some(Done));
        // The read after it, which writes `unknown` over the `done`, leaves with that `done` still.
        assert_eq!(left_as(Some(Done), Unknown, None, true), Some(Done));
    }

    #[test]
    fn an_agent_at_rest_whose_process_went_leaves_with_the_status_it_had() {
        use AgentState::*;
        assert_eq!(left_as(Some(Idle), Unknown, None, true), Some(Idle));
        assert_eq!(left_as(Some(Done), Unknown, None, true), Some(Done));
        // A pane that said nothing of its agent has nothing to leave with.
        assert_eq!(left_as(None, Unknown, None, true), None);
        assert_eq!(left_as(Some(Unknown), Unknown, None, true), None);
    }

    #[test]
    fn a_write_for_an_agent_that_is_still_there_leaves_nothing() {
        use AgentState::*;
        // `unknown` because nothing on the screen says what a live agent is doing.
        assert_eq!(left_as(Some(Idle), Unknown, None, false), None);
        assert_eq!(
            left_as(Some(Working), Idle, Some("live_prompt_box"), false),
            None
        );
        assert_eq!(left_as(Some(Idle), Working, None, true), None);
    }

    // herdr's completion guards (#4457): the pieces the fake-clock scenarios in `watch::sim` put
    // together.

    #[test]
    fn a_session_where_none_was_is_the_first_and_another_in_its_place_takes_the_agent_again() {
        let at = |kind: &str, id: Option<&str>, path: Option<&str>| {
            Session::of(&AgentRecord {
                kind: Some(kind.to_string()),
                session_id: id.map(str::to_string),
                session_path: path.map(str::to_string),
                ..AgentRecord::default()
            })
        };
        let none = at("claude", None, None);
        let old = at("claude", Some("a"), Some("/s/a.jsonl"));
        assert_eq!(none.change_to(&none), SessionChange::None);
        assert_eq!(old.change_to(&old), SessionChange::None);
        // A first report (Claude's `SessionStart` once its trust question is answered).
        assert_eq!(none.change_to(&old), SessionChange::First);
        // `/clear`, `/resume`: another id, and another file with it.
        let new = at("claude", Some("b"), Some("/s/b.jsonl"));
        assert_eq!(old.change_to(&new), SessionChange::Replaced);
        // Pi's `/new`: by the path alone.
        assert_eq!(
            at("pi", None, Some("/s/old.jsonl")).change_to(&at("pi", None, Some("/s/new.jsonl"))),
            SessionChange::Replaced
        );
        // A session that had only an id, reported again with its file: herdr's session reference
        // changes as well, and the agent is taken again.
        assert_eq!(
            at("pi", Some("a"), None).change_to(&at("pi", Some("a"), Some("/s/a.jsonl"))),
            SessionChange::Replaced
        );
        // Another agent's session is the business of the read that finds the agent, not of the
        // session; and a record that went has none.
        assert_eq!(
            old.change_to(&at("codex", Some("c"), None)),
            SessionChange::None
        );
        assert_eq!(
            old.change_to(&Session::of(&AgentRecord::default())),
            SessionChange::None
        );
        assert!(SessionChange::First.changed() && SessionChange::Replaced.changed());
        assert!(!SessionChange::None.changed());
    }

    #[test]
    fn herdrs_completion_reset_marks_the_pane_seen_unless_the_change_it_comes_with_decides() {
        let unseen = prev(Some("claude"), Some(AgentState::Blocked), false);
        // Nothing else written: the pane is marked seen, whatever its state.
        assert_eq!(Update::default().reset(&unseen).seen, Some(true));
        // Seen already: nothing to write.
        let seen = prev(Some("claude"), Some(AgentState::Blocked), true);
        assert_eq!(Update::default().reset(&seen).seen, None);
        // A turn starting, or a `done`, decides the mark itself.
        let starting = Update {
            seen: Some(false),
            ..Update::default()
        };
        assert_eq!(starting.clone().reset(&unseen), starting);
        // The record going, or a pane with no agent, has nothing to mark.
        let clear = Update {
            clear: true,
            ..Update::default()
        };
        assert_eq!(clear.clone().reset(&unseen), clear);
        assert_eq!(Update::default().reset(&AgentRecord::default()).seen, None);
    }

    #[test]
    fn an_agent_becoming_ready_earns_no_done_notification_where_a_finished_turn_would() {
        let before = prev(Some("claude"), Some(AgentState::Blocked), false);
        let ready = |ready: bool| Decision::Transition {
            kind: "claude".to_string(),
            from: Some(AgentState::Blocked),
            to: AgentState::Idle,
            rule: Some("live_prompt_box".to_string()),
            detail: String::new(),
            seen: false,
            ready,
        };
        let became_ready = update(&before, &ready(true));
        assert_eq!(
            became_ready.state,
            Some((AgentState::Idle, Some("live_prompt_box".to_string())))
        );
        assert_eq!(became_ready.notice, None);
        // Without the flag the same change is a completion's, which earns one.
        assert_eq!(
            update(&before, &ready(false))
                .notice
                .map(|notice| notice.class),
            Some(Class::Done)
        );

        // And the same for an agent that reports for itself.
        let reported = |ready: bool| Decision::Reported {
            kind: "pi".to_string(),
            from: Some(AgentState::Blocked),
            via: None,
            to: AgentState::Idle,
            reported: AgentState::Idle,
            replaced: None,
            detail: String::new(),
            seen: false,
            ready,
        };
        let pi = prev(Some("pi"), Some(AgentState::Idle), false);
        assert_eq!(update(&pi, &reported(true)).notice, None);
        assert_eq!(
            update(&pi, &reported(false))
                .notice
                .map(|notice| notice.class),
            Some(Class::Done)
        );
    }

    #[test]
    fn a_waiting_notice_is_delivered_only_to_the_agent_and_session_it_was_earned_for() {
        let earned = Earned {
            kind: Some("claude".to_string()),
            session_id: Some("a".to_string()),
            session_path: Some("/s/a.jsonl".to_string()),
        };
        let pending = |class: Class, state: AgentState, exit: bool| Pending {
            notice: Notice { class, state, exit },
            earned: earned.clone(),
        };
        let done = pending(Class::Done, AgentState::Done, false);
        assert!(done.due(Some("done"), &earned, || false));
        // Another session (`/clear`), or another agent: herdr's completion reset dropped it.
        let cleared = Earned {
            session_id: Some("b".to_string()),
            session_path: Some("/s/b.jsonl".to_string()),
            ..earned.clone()
        };
        assert!(!done.due(Some("done"), &cleared, || false));
        let codex = Earned {
            kind: Some("codex".to_string()),
            ..earned.clone()
        };
        assert!(!done.due(Some("done"), &codex, || false));
        // A first session reported since: a change of session for herdr too.
        let first = Earned {
            kind: Some("claude".to_string()),
            ..Earned::default()
        };
        let before_any = Pending {
            earned: first.clone(),
            ..done.clone()
        };
        assert!(!before_any.due(Some("done"), &earned, || false));
        assert!(before_any.due(Some("done"), &first, || false));
        // A request is held to the same.
        let request = pending(Class::Request, AgentState::Blocked, false);
        assert!(request.due(Some("blocked"), &earned, || false));
        assert!(!request.due(Some("blocked"), &cleared, || false));
        // The state must still stand behind it, as before.
        assert!(!done.due(Some("idle"), &earned, || false));
        // An exit's notice outlives the record it was earned in, which the clear takes away, and
        // the `unknown` a hint keeps once the agent it started has gone (its recorded pid dead).
        let exit = pending(Class::Done, AgentState::Done, true);
        assert!(exit.due(None, &Earned::default(), || false));
        let hinted = Earned {
            kind: Some("claude".to_string()),
            ..Earned::default()
        };
        assert!(exit.due(Some("unknown"), &hinted, || true));
        // While that record still stands it is held to it as any notice is: an agent that reported
        // its own finish and has started a new session since is not that finish's.
        assert!(exit.due(Some("done"), &earned, || false));
        assert!(!exit.due(Some("done"), &cleared, || false));
    }

    #[test]
    fn what_a_notice_is_earned_for_is_the_record_once_the_change_is_written() {
        let record = AgentRecord {
            kind: Some("claude".to_string()),
            session_id: Some("a".to_string()),
            session_path: Some("/s/a.jsonl".to_string()),
            ..AgentRecord::default()
        };
        assert_eq!(
            Earned::after(&record, &Update::default()),
            Earned::of(&record)
        );
        // Another agent took the pane: its kind, and none of the last one's session.
        let taken = Update {
            kind: Some("codex".to_string()),
            release_identity: true,
            ..Update::default()
        };
        assert_eq!(
            Earned::after(&record, &taken),
            Earned {
                kind: Some("codex".to_string()),
                ..Earned::default()
            }
        );
    }

    #[test]
    fn a_read_whose_decision_could_not_be_written_leaves_the_agent_being_taken() {
        // A new Claude's trust question answered: the first idle is the agent becoming ready. If
        // that could not be written, the read that retries it finds the agent still being taken,
        // and the idle still no completion.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let fresh = observed("claude", BLOCKED, prev(None, None, false));
        let _ = decide_at(&fresh, base, 0, &mut arb);
        assert!(arb.acquiring());
        let asking = observed(
            "claude",
            BLOCKED,
            prev(Some("claude"), Some(AgentState::Unknown), false),
        );
        let _ = decide_at(&asking, base, 3000, &mut arb);
        let _ = decide_at(&asking, base, 3300, &mut arb);
        let blocked = observed(
            "claude",
            IDLE,
            prev_with_rule(
                Some("claude"),
                Some(AgentState::Blocked),
                Some("bash_permission_prompt"),
            ),
        );
        let checkpoint = arb.checkpoint();
        let first = decide_at(&blocked, base, 3600, &mut arb);
        assert!(
            matches!(
                first,
                Decision::Transition {
                    to: AgentState::Idle,
                    ready: true,
                    ..
                }
            ),
            "{first:?}"
        );
        assert!(!arb.acquiring(), "the first idle ends it");
        arb.restore(checkpoint);
        assert!(arb.acquiring(), "put back with the write that failed");
        let retried = decide_at(&blocked, base, 3900, &mut arb);
        assert_eq!(retried, first);
    }

    #[test]
    fn a_new_session_while_done_is_the_idle_it_is_once_looked_at_and_takes_the_agent_again() {
        // Claude's `/clear` on a `done` pane: the read finds the new session id, writes the `idle`
        // the pane is once looked at with the rule it had, and the next idle is no completion
        // until the new session has worked.
        let base = Instant::now();
        let done = AgentRecord {
            session_id: Some("old".to_string()),
            rule: Some("live_prompt_box".to_string()),
            ..prev(Some("claude"), Some(AgentState::Done), false)
        };
        let mut arb = Arbiter::settled("claude");
        arb.remember_session(&done);
        let cleared = AgentRecord {
            session_id: Some("new".to_string()),
            ..done.clone()
        };
        let read = decide_at(&observed("claude", IDLE, cleared), base, 0, &mut arb);
        assert_eq!(
            read,
            Decision::Transition {
                kind: "claude".to_string(),
                from: Some(AgentState::Done),
                to: AgentState::Idle,
                rule: Some("live_prompt_box".to_string()),
                detail: "\u{2733} fake task".to_string(),
                seen: true,
                ready: false,
            }
        );
        assert!(arb.take_reset(), "herdr's completion reset");
        assert!(!arb.take_reset(), "once");
        assert!(arb.acquiring(), "the new session's agent is taken again");
    }

    #[test]
    fn a_pane_marked_as_taking_its_agent_arms_a_new_watcher_and_its_first_idle_takes_the_mark_off()
    {
        // A watcher started again while a Claude asks its trust question, on a pane that records
        // the agent as being taken: the new watcher is armed by the mark, writes nothing over it
        // while the question stands, and takes it off with the idle that is no completion.
        let base = Instant::now();
        let mut arb = Arbiter::default();
        let asking_prev = prev_with_rule(
            Some("claude"),
            Some(AgentState::Blocked),
            Some("bash_permission_prompt"),
        );
        let mut asking = observed("claude", BLOCKED, asking_prev.clone());
        asking.acquiring = Some(Acquired::At(None));
        assert_eq!(decide_at(&asking, base, 0, &mut arb), Decision::Keep);
        assert!(arb.acquiring());
        assert_eq!(arb.take_mark(), None, "the pane holds the mark already");
        let mut answered = observed("claude", IDLE, asking_prev);
        answered.acquiring = Some(Acquired::At(None));
        let decision = decide_at(&answered, base, 300, &mut arb);
        assert!(
            matches!(
                decision,
                Decision::Transition {
                    to: AgentState::Idle,
                    ready: true,
                    ..
                }
            ),
            "{decision:?}"
        );
        assert_eq!(
            arb.take_mark(),
            Some(MarkWrite::Unset),
            "@agent_acquiring taken off"
        );
        assert_eq!(arb.take_mark(), None, "once");

        // A new agent the watcher takes itself is marked from the read that takes it.
        let mut arb = Arbiter::default();
        let _ = decide_at(
            &observed("claude", BLOCKED, prev(None, None, false)),
            base,
            0,
            &mut arb,
        );
        assert_eq!(arb.take_mark(), Some(MarkWrite::Set(Acquired::At(None))));
        // And from the turn stamp the pane has then, when it has one.
        let mut arb = Arbiter::default();
        let mut stamped = observed("claude", BLOCKED, prev(None, None, false));
        stamped.turn = Some("17".to_string());
        let _ = decide_at(&stamped, base, 0, &mut arb);
        assert_eq!(
            arb.take_mark(),
            Some(MarkWrite::Set(Acquired::At(Some("17".to_string()))))
        );
    }

    #[test]
    fn the_writes_a_read_leaves_besides_its_decision_are_the_seen_mark_and_the_acquisition_mark() {
        let unseen = prev(Some("claude"), Some(AgentState::Blocked), false);
        let mut arb = Arbiter {
            reset: true,
            mark: Some(MarkWrite::Set(Acquired::At(None))),
            ..Arbiter::default()
        };
        let update = Update::default().finish(&mut arb, &unseen);
        assert_eq!(update.seen, Some(true));
        assert_eq!(update.acquiring, Some(MarkWrite::Set(Acquired::At(None))));
        assert!(!arb.take_reset() && arb.take_mark().is_none(), "taken");
        // A clear takes the mark off with the rest of the record.
        arb.mark = Some(MarkWrite::Unset);
        let clear = Update {
            clear: true,
            ..Update::default()
        }
        .finish(&mut arb, &unseen);
        assert_eq!(clear.acquiring, None);
        assert_eq!(arb.take_mark(), None, "taken all the same");
    }

    const SUSPENDED_SESSION: &str = "5b2e9c1a-3f47-4d8e-a6b0-7c19e2d4f863";

    /// The record of a Claude that reported its session, in `state`, with `rule`.
    fn claude_with_session(state: AgentState, rule: Option<&str>) -> AgentRecord {
        AgentRecord {
            session_id: Some(SUSPENDED_SESSION.to_string()),
            ..prev_with_rule(Some("claude"), Some(state), rule)
        }
    }

    /// A read with the shell in front of the pane, the agent's process stopped behind it.
    fn stopped_behind_the_shell(prev: AgentRecord) -> Observed {
        Observed {
            suspended: true,
            ..observed("bash", "$ ", prev)
        }
    }

    #[test]
    fn a_hinted_agent_with_no_hook_that_is_stopped_reads_as_suspended_without_its_screen() {
        let mut arb = Arbiter::settled("claude");
        let mut o = stopped_behind_the_shell(claude_with_session(AgentState::Working, None));
        o.kind_hint = Some("claude".to_string());
        assert!(!wants_screen(&o, &arb), "no capture while it is suspended");
        let decision = decide_at(&o, Instant::now(), 0, &mut arb);
        assert!(
            matches!(
                &decision,
                Decision::Suspended {
                    from: Some(AgentState::Working),
                    hook: false,
                    ..
                }
            ),
            "{decision:?}"
        );
        assert_eq!(arb.identity.as_deref(), Some("claude"));
        // Not suspended, the same read wants the screen; nor does a suspend hide another agent
        // that names itself in front.
        let mut running = o.clone();
        running.suspended = false;
        assert!(wants_screen(&running, &arb));
        let mut codex = o.clone();
        codex.kind_hint = None;
        codex.command = "codex".to_string();
        assert!(wants_screen(&codex, &arb));
    }

    #[test]
    fn a_stopped_agent_is_kept_at_idle_with_the_suspend_for_its_reason_and_no_notification() {
        let mut arb = Arbiter::settled("claude");
        arb.acquiring = Some(Acquired::At(None));
        let prev = claude_with_session(AgentState::Working, None);
        let o = stopped_behind_the_shell(prev.clone());
        let decision = decide_at(&o, Instant::now(), 0, &mut arb);
        assert_eq!(
            decision,
            Decision::Suspended {
                kind: "claude".to_string(),
                from: Some(AgentState::Working),
                hook: false,
                detail: "\u{2733} fake task".to_string(),
            }
        );
        let update = update(&prev, &decision);
        assert!(!update.clear && !update.release_identity && !update.release_name);
        assert_eq!(
            update.state,
            Some((AgentState::Idle, Some(store::SUSPENDED_RULE.to_string())))
        );
        assert_eq!(update.notice, None, "a suspend finishes nothing");
        assert_eq!(update.seen, None);
        assert_eq!(arb.identity.as_deref(), Some("claude"));
        assert_eq!(arb.acquiring, Some(Acquired::At(None)), "still being taken");

        // The reads while it stays stopped change nothing.
        let kept = stopped_behind_the_shell(claude_with_session(
            AgentState::Idle,
            Some(store::SUSPENDED_RULE),
        ));
        assert_eq!(
            decide_at(&kept, Instant::now(), 50, &mut arb),
            Decision::Keep
        );
    }

    #[test]
    fn a_suspended_agent_in_front_again_is_the_same_agent_with_no_startup_grace() {
        // Long since taken: a new agent would be taken again, from its first idle.
        let mut arb = Arbiter::settled("claude");
        arb.acquiring = None;
        let base = Instant::now();
        let stopped = stopped_behind_the_shell(claude_with_session(AgentState::Idle, None));
        assert!(matches!(
            decide_at(&stopped, base, 0, &mut arb),
            Decision::Suspended { .. }
        ));
        // `fg`: Claude is in front again, its record as the suspend left it.
        let back = observed(
            "claude",
            "",
            claude_with_session(AgentState::Idle, Some(store::SUSPENDED_RULE)),
        );
        let decision = decide_at(&back, base, 100, &mut arb);
        assert!(!arb.in_startup_grace(), "a new agent's grace: {decision:?}");
        assert!(
            matches!(
                &decision,
                Decision::RuleChanged { rule } if rule.as_deref() != Some(store::SUSPENDED_RULE)
            ),
            "{decision:?}"
        );
        assert_eq!(arb.acquiring, None, "no first-idle reset");
        assert!(!update(&back.prev, &decision).release_identity);
    }

    #[test]
    fn an_agent_that_dies_while_stopped_goes_as_any_exit() {
        let mut arb = Arbiter::settled("claude");
        let base = Instant::now();
        let stopped = stopped_behind_the_shell(claude_with_session(AgentState::Idle, None));
        assert!(matches!(
            decide_at(&stopped, base, 0, &mut arb),
            Decision::Suspended { .. }
        ));
        // The process has gone: the shell is in front and nothing is behind it.
        let gone = observed(
            "bash",
            "$ ",
            claude_with_session(AgentState::Idle, Some(store::SUSPENDED_RULE)),
        );
        assert_eq!(
            decide_at(&gone, base, 100, &mut arb),
            Decision::NoAgent { clear: true }
        );
    }

    #[test]
    fn a_suspended_agent_its_hook_reports_for_keeps_the_hook_as_its_source() {
        let mut arb = Arbiter::settled("claude");
        let mut prev = hooked("claude", AgentState::Working);
        prev.session_id = Some(SUSPENDED_SESSION.to_string());
        let o = stopped_behind_the_shell(prev.clone());
        let decision = decide_at(&o, Instant::now(), 0, &mut arb);
        assert!(
            matches!(
                &decision,
                Decision::Suspended {
                    hook: true,
                    from: Some(AgentState::Working),
                    ..
                }
            ),
            "{decision:?}"
        );
        let update = update(&prev, &decision);
        assert_eq!(
            update.state, None,
            "written over the report, not as the manifest's"
        );
        assert_eq!(
            update.complete,
            Some((AgentState::Working, AgentState::Idle))
        );
        assert_eq!(update.rule, Some(Some(store::SUSPENDED_RULE.to_string())));
        assert_eq!(update.notice, None);

        // In front again, the hook still its source: only the reason goes.
        let mut back = hooked("claude", AgentState::Idle);
        back.rule = Some(store::SUSPENDED_RULE.to_string());
        let decision = decide_at(&observed("claude", "", back), Instant::now(), 100, &mut arb);
        assert_eq!(decision, Decision::RuleChanged { rule: None });
    }
}
