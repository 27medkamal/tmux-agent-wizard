// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! The watcher on a fake clock: [`next_read`] and [`decide`] run together over a
//! scripted pane, the way `Watcher::pump` runs them, with no tmux and no real time.
//!
//! A script is a list of [`Row`]s: at `t` ms the pane starts showing `frame`, and
//! `output` says whether it printed anything to get there — which is what makes it
//! eligible for a read at its next tick. [`Sim`] steps its clock to each row and to each
//! read `next_read` asks for, calls `decide` with the frame on screen at that instant,
//! applies the decision's [`state::update`] to an in-memory `@agent_*` record as
//! `Watcher::apply` applies it to tmux's, and keeps what was written and when.
//!
//! Notifications are the watcher's too: a transition's notice waits out its
//! `[ui.toast]` delay (none unless a test sets one), then is delivered only if the
//! record still stands behind it, for the agent and the session it was earned in, and
//! nobody is looking — `Watcher::notify`'s gate.
//!
//! Times are milliseconds from the start of the script. A row, a read and a delivery at
//! the same instant: the row first, so the read sees what the pane shows at that instant,
//! and the delivery last, so it sees what the read wrote.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::state::{self, Arbiter, Decision, Earned, Observed, Pending, Taken, decide};
use super::{Timing, delay_for, next_read};
use crate::config::Toast;
use crate::detect::{ContainerAgent, LoadedManifest, ManifestStore};
use crate::notify::Class;
use crate::store::{Acquired, AgentRecord, AgentState, Session};

/// The token `report-agent-session --session-start-source` records (`store::SESSION_START_TOKEN`),
/// keyed as `store::read` keys `@agent_token_*`, without the prefix.
const SESSION_START_TOKEN: &str = "session_start_source";

/// The frames the scenarios are built from. The verdicts are pinned by
/// `the_frames_read_as_the_scenarios_assume` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Frame {
    /// A turn in progress: Claude Code 2.1.258's spinner line. `working`, visible.
    W,
    /// A half-drawn redraw: one tool line and nothing else, under a working title. A
    /// plain idle, no rule matched.
    H,
    /// The prompt box at rest. `idle`, visible (`live_prompt_box`).
    I,
    /// A permission prompt. `blocked`, visible.
    B,
    /// The transcript viewer overlay: `skip_state_update`.
    T,
    /// `vim` in the foreground (Claude's `ctrl+g`). A plain idle under Claude's rules.
    V,
    /// The shell back at its prompt.
    S,
    /// Codex just started where a Claude was: nothing drawn yet, and the title still the
    /// spinner frame that Claude's turn last set, which Codex's `osc_title_working` would
    /// take for its own turn.
    C,
    /// That Codex once its own turn has set a title: a different spinner frame.
    D,
    /// Codex just started where a Claude at its prompt was: nothing drawn yet, and the title
    /// still the `✳` that Claude's prompt left, which Codex's `osc_title_idle` would
    /// take for its own idle.
    E,
    /// That Codex once it has set a title of its own that is not a spinner.
    G,
    /// The prompt box at rest with a sub-agent still counting in the list under it: the
    /// main agent's turn is over and the work is not. `working`, visible
    /// (`taw_subagent_running`).
    A,
    /// That prompt box once every sub-agent has finished, their rows reading `idle`.
    /// `idle`, visible (`live_prompt_box`).
    F,
    /// A program in front of the pane that no name in the identity table matches, which an agent
    /// that reports its own state runs under: the pane's command is `sleep`, and the screen says
    /// nothing, as the stand-in of a test does. The state comes from the agent's reports.
    P,
    /// A Pi in front of the pane, which its process names, and whose screen says nothing yet: an
    /// agent the watcher takes by its process (herdr's `AgentProcessDetected`) and whose state
    /// then comes from its reports.
    R,
}

impl Frame {
    fn command(self) -> &'static str {
        match self {
            Frame::V => "vim",
            Frame::S => "zsh",
            Frame::P => "sleep",
            Frame::R => "pi",
            Frame::C | Frame::D | Frame::E | Frame::G => "codex",
            _ => "claude",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Frame::I | Frame::B | Frame::A | Frame::F | Frame::P | Frame::E => "\u{2733} fake task",
            Frame::C => "\u{280B} fake task",
            Frame::D => "\u{2819} project",
            Frame::G => "codex",
            Frame::R => "",
            _ => "\u{273B} fake task",
        }
    }

    fn screen(self) -> &'static str {
        match self {
            Frame::W => include_str!("../../tests/fixtures/screens/claude-2.1.258-working-vim.txt"),
            Frame::H => "\u{23FA} Update(src/x.rs)\n",
            Frame::I => include_str!("../../tests/fixtures/screens/claude-idle.txt"),
            Frame::B => include_str!("../../tests/fixtures/screens/claude-permission.txt"),
            Frame::T => "\u{2026} showing detailed transcript\nctrl+o to toggle\n",
            Frame::V => "~\n~\n-- INSERT --\n",
            Frame::S => "$ \n",
            Frame::C | Frame::D | Frame::E | Frame::G | Frame::P | Frame::R => "\n",
            Frame::A => include_str!("../../tests/fixtures/screens/claude-subagents-running.txt"),
            Frame::F => include_str!("../../tests/fixtures/screens/claude-subagents-idle.txt"),
        }
    }
}

/// From `t` ms the pane shows `frame`; `output` is whether it printed to get there. A row that
/// `report`s is something written to the pane from outside the watcher instead, an agent's hook or
/// a `taw` verb: the pane shows what it showed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Row {
    t: u64,
    frame: Frame,
    output: bool,
    /// What was written to the pane.
    report: Option<Said>,
}

/// What is written to the pane from outside the watcher, through the verb that writes it.
#[derive(Clone, Copy, Debug)]
pub(super) enum Said {
    /// `taw pane report-agent`: the state, and the agent that reports it.
    State(AgentState, &'static str),
    /// `taw pane report-agent-session`: the agent, the id and the path of its session, and what
    /// started it (`--session-start-source`).
    Session(
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
    ),
    /// `taw agent start --kind <kind>`: the last agent's record taken off the pane, and the kind
    /// hint set, before the command is typed (herdr's managed launch).
    Start(&'static str),
}

/// `frame` printed at `t`.
pub(super) fn out(t: u64, frame: Frame) -> Row {
    Row {
        t,
        frame,
        output: true,
        report: None,
    }
}

/// The hook of an OpenCode reports `state` at `t`: `taw pane report-agent` writes it to the pane,
/// and nudges the watcher, which is read as output is, without printing any.
pub(super) fn report(t: u64, state: AgentState) -> Row {
    report_as(t, "opencode", state)
}

/// [`report`] by the hook of the agent `agent`.
pub(super) fn report_as(t: u64, agent: &'static str, state: AgentState) -> Row {
    Row {
        t,
        frame: Frame::P,
        output: false,
        report: Some(Said::State(state, agent)),
    }
}

/// The hook of the agent `agent` reports at `t` that its session is `id`, started by `reason`:
/// `taw pane report-agent-session` writes it to the pane, as Claude's `SessionStart` hook does when
/// it starts (`startup`), and again on `/clear` and `/resume`. It nudges no watcher: what the agent
/// prints with it brings the read.
pub(super) fn session_as(
    t: u64,
    agent: &'static str,
    id: &'static str,
    reason: &'static str,
) -> Row {
    Row {
        t,
        frame: Frame::P,
        output: false,
        report: Some(Said::Session(agent, Some(id), None, Some(reason))),
    }
}

/// [`session_as`] for an agent that reports its session by the path of its file, as Pi does, and
/// again on `/new`, `/resume` and `/fork`.
pub(super) fn session_path_as(
    t: u64,
    agent: &'static str,
    path: &'static str,
    reason: &'static str,
) -> Row {
    Row {
        t,
        frame: Frame::P,
        output: false,
        report: Some(Said::Session(agent, None, Some(path), Some(reason))),
    }
}

/// `taw agent start --kind <kind>` at `t` (see [`Said::Start`]).
pub(super) fn start_as(t: u64, kind: &'static str) -> Row {
    Row {
        t,
        frame: Frame::P,
        output: false,
        report: Some(Said::Start(kind)),
    }
}

/// `frame` printed every `step` ms from `from` to `to`, both included: a spinner's
/// redraws.
pub(super) fn every(frame: Frame, from: u64, to: u64, step: u64) -> Vec<Row> {
    (from..=to)
        .step_by(step as usize)
        .map(|t| out(t, frame))
        .collect()
}

/// One write the watcher would make to tmux.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Published {
    /// A transition: the state written, with the rule written beside it.
    State(AgentState, Option<String>),
    /// `RuleChanged`: the evidence alone.
    Rule(Option<String>),
    /// `KindChanged`: the occupant alone.
    Kind(String),
    /// `NoAgent { clear: true }`: every `@agent_*` option dropped.
    Cleared,
}

/// A transition to `state`, explained by `rule`.
pub(super) fn st(state: AgentState, rule: Option<&str>) -> Published {
    Published::State(state, rule.map(str::to_string))
}

pub(super) struct Sim {
    base: Instant,
    now: u64,
    timing: Timing,
    store: ManifestStore,
    cache: HashMap<String, LoadedManifest>,
    record: AgentRecord,
    foreground: bool,
    /// The container runtime the agent runs under, when it does (`in_container`).
    container: Option<&'static str>,
    frame: Frame,
    rows: Vec<Row>,
    next_row: usize,
    arbiter: Arbiter,
    last_read: Option<u64>,
    eligible_since: Option<u64>,
    agent_present: bool,
    /// What the agent's own frames read as in the process table, when it is not the name its
    /// executable has (see [`Sim::under`]).
    agent_command: Option<&'static str>,
    /// `[ui.toast]`: how long a notice waits before it is delivered.
    toast: Toast,
    /// Notices waiting out their delay, with the instant it is up.
    pending: Vec<(u64, Pending)>,
    /// When each read happened.
    pub reads: Vec<u64>,
    /// How many reports of work or a wait the agent has made: the stamp `@agent_turn` holds.
    turns: u64,
    /// `@agent_acquiring`: the pane records its agent as being taken, and since when
    /// (`store::ACQUIRING_OPTION`).
    acquiring: Option<Acquired>,
    /// What each read wrote, and when.
    pub written: Vec<(u64, Published)>,
    /// Each change of state the watcher announced (the status event, and the `agent.*` event of
    /// the state it went into), and when: `(at, from, to)`, with `None` for no agent.
    pub events: Vec<(u64, Option<AgentState>, Option<AgentState>)>,
    /// The notifications delivered, and when.
    pub toasts: Vec<(u64, Class)>,
    /// The `[hooks] notification` commands that would run, and when: a hook runs where the
    /// toast is delivered, and is told the kind of it. Written by the sim's own model of
    /// `notify::announce` (see `Sim::deliver`), not by `announce`.
    pub notification_hooks: Vec<(u64, &'static str)>,
}

impl Sim {
    /// A watcher that last read the pane at t=0 and left `start` on it: the tick grid is
    /// anchored there. Before its first row the pane shows that row's frame.
    pub fn anchored(start: AgentRecord, foreground: bool, rows: Vec<Row>) -> Sim {
        let mut sim = Sim::new(start, foreground, rows);
        sim.agent_present = sim.record.kind.is_some();
        if let Some(kind) = &sim.record.kind {
            sim.arbiter = Arbiter::settled(kind);
        }
        // That read left the pane in the state it is in, which was announced then, and saw the
        // session the agent was in.
        sim.arbiter
            .retake(sim.record.state.map_or(Taken::Nothing, |state| {
                Taken::left(state, sim.record.kind.as_deref())
            }));
        sim.arbiter.remember_session(&sim.record);
        sim.last_read = Some(0);
        sim
    }

    /// A watcher starting at t=0, whose first read is due at once.
    pub fn fresh(start: AgentRecord, foreground: bool, rows: Vec<Row>) -> Sim {
        Sim::new(start, foreground, rows)
    }

    fn new(start: AgentRecord, foreground: bool, rows: Vec<Row>) -> Sim {
        assert!(
            rows.windows(2).all(|pair| pair[0].t <= pair[1].t),
            "script rows must be in time order"
        );
        Sim {
            base: Instant::now(),
            now: 0,
            timing: Timing::default(),
            // Directories that do not exist: every `load` falls through to the bundled
            // manifest plus taw's supplements, the rules `tests/detect_fixtures.rs` pins.
            store: ManifestStore::new(
                PathBuf::from("/nonexistent/taw-test-override"),
                PathBuf::from("/nonexistent/taw-test-remote"),
            ),
            cache: HashMap::new(),
            record: start,
            foreground,
            container: None,
            frame: rows.first().map_or(Frame::S, |row| row.frame),
            rows,
            next_row: 0,
            arbiter: Arbiter::default(),
            last_read: None,
            eligible_since: None,
            agent_present: false,
            agent_command: None,
            toast: Toast {
                delay_seconds: 0.0,
                done_delay_seconds: 0.0,
                ..Toast::default()
            },
            pending: Vec::new(),
            reads: Vec::new(),
            turns: 0,
            acquiring: None,
            written: Vec::new(),
            events: Vec::new(),
            toasts: Vec::new(),
            notification_hooks: Vec::new(),
        }
    }

    /// The same pane with its agent run under `command`, which no name in the identity table
    /// matches: a Claude an npm install runs under `node`. Its frames are what they were, and
    /// what tmux reports as the pane's command while they are on screen is `command`.
    pub fn under(mut self, command: &'static str) -> Sim {
        self.agent_command = Some(command);
        self
    }

    /// The same watcher on an agent that runs in a container: `runtime` is the foreground
    /// command of every frame Claude draws, and its arguments name Claude. The shell's
    /// frame and every other process's are what they were.
    pub fn in_container(mut self, runtime: &'static str) -> Sim {
        self.container = Some(runtime);
        self
    }

    /// The same watcher with `[ui.toast]`'s two delays, in seconds.
    pub fn with_toast_delays(mut self, request: f64, done: f64) -> Sim {
        self.toast.delay_seconds = request;
        self.toast.done_delay_seconds = done;
        self
    }

    /// The same pane with its agent marked as being taken since `at` (`@agent_acquiring`), as `taw
    /// agent start`, a restore or the watcher before this one left it.
    pub fn marked(mut self, at: Acquired) -> Sim {
        self.acquiring = Some(at);
        self
    }

    /// The same pane with the turn stamp `@agent_turn` its agent's reports had moved to before this
    /// watcher started: `turns` reports of work or of a wait.
    pub fn with_turns(mut self, turns: u64) -> Sim {
        self.turns = turns;
        self
    }

    fn at(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }

    fn ms(&self, instant: Instant) -> u64 {
        instant.saturating_duration_since(self.base).as_millis() as u64
    }

    /// Runs the script up to `until` ms, included.
    pub fn run_until(&mut self, until: u64) -> &mut Sim {
        loop {
            let due = next_read(
                self.at(self.now),
                self.last_read.map(|ms| self.at(ms)),
                &self.timing,
                self.eligible_since.map(|ms| self.at(ms)),
                self.arbiter.hold_active(),
                self.arbiter.forced_read(),
                self.agent_present,
            )
            .map(|due| self.ms(due).max(self.now));
            let row = self.rows.get(self.next_row).map(|row| row.t);
            let delivery = self.pending.iter().map(|(at, _)| *at).min();
            // The earliest of the three; at one instant, rows, then reads, then deliveries.
            let next = [(row, 0), (due, 1), (delivery, 2)]
                .into_iter()
                .filter_map(|(at, order)| at.filter(|at| *at <= until).map(|at| (at, order)))
                .min();
            match next {
                Some((_, 0)) => self.show_row(),
                Some((t, 1)) => self.read(t),
                Some((t, _)) => self.deliver(t),
                None => {
                    self.now = until;
                    return self;
                }
            }
        }
    }

    fn show_row(&mut self) {
        let row = self.rows[self.next_row];
        self.next_row += 1;
        self.now = row.t;
        match row.report {
            Some(Said::State(state, agent)) => return self.report(row.t, state, agent),
            Some(Said::Session(agent, id, path, reason)) => {
                return self.report_session(agent, Session { id, path }, reason);
            }
            Some(Said::Start(kind)) => return self.agent_start(kind),
            None => {}
        }
        self.frame = row.frame;
        if row.output && self.eligible_since.is_none() {
            self.eligible_since = Some(row.t);
        }
    }

    /// `taw pane report-agent`: the state, its source and no rule are written to the pane by the
    /// reporter itself, and the watcher is nudged, which makes the pane eligible for a read at its
    /// next tick as output does.
    fn report(&mut self, t: u64, state: AgentState, agent: &str) {
        self.drop_identity_of_another_agent(agent);
        self.record.kind = Some(agent.to_string());
        // `store::add_turn_stamp`: every report of work moves the stamp, and a report of a wait
        // the pane was not already in.
        if state == AgentState::Working
            || (state == AgentState::Blocked && self.record.state != Some(AgentState::Blocked))
        {
            self.turns += 1;
        }
        self.record.state = Some(state);
        self.record.rule = None;
        self.record.source = Some("hook:sim".to_string());
        self.nudge(t);
    }

    /// `taw pane report-agent-session` (`store::write_session_report`): the agent and its word that
    /// it is there, the session it reported and what started it, written to the pane by the
    /// reporter itself; the state and its source stay as they were. A report of a session is no
    /// nudge: the watcher takes it in at its next read, which what the agent prints brings.
    fn report_session(
        &mut self,
        agent: &str,
        session: Session<'static>,
        reason: Option<&'static str>,
    ) {
        self.drop_identity_of_another_agent(agent);
        self.record.kind = Some(agent.to_string());
        self.record.reported_kind = Some(agent.to_string());
        if let Some(id) = session.id {
            self.record.session_id = Some(id.to_string());
        }
        if let Some(path) = session.path {
            self.record.session_path = Some(path.to_string());
        }
        if let Some(reason) = reason {
            self.record
                .tokens
                .insert(SESSION_START_TOKEN.to_string(), reason.to_string());
        }
    }

    /// What the last agent reported of itself goes with it, in the same write as a report from
    /// another (`store::Commands::drop_identity_of_another_agent`).
    fn drop_identity_of_another_agent(&mut self, agent: &str) {
        if self
            .record
            .kind
            .as_deref()
            .is_some_and(|last| last != agent)
        {
            self.record.session_id = None;
            self.record.session_path = None;
            self.record.reported_kind = None;
            self.record.tokens.remove(SESSION_START_TOKEN);
        }
    }

    /// `taw agent start --kind <kind>` (`cli::agent::forget_the_last_agent`, then the hint): what
    /// the pane records of the last agent is taken off it, the hint and the watcher's pid kept, and
    /// the hint set to `kind`. The command it types is the next frame's business. The verb's own
    /// clear makes no read due: what it types does.
    fn agent_start(&mut self, kind: &str) {
        self.record = AgentRecord {
            watch_pid: self.record.watch_pid,
            kind_hint: Some(kind.to_string()),
            ..AgentRecord::default()
        };
        // `@agent_turn` goes with the record, and the agent about to start is being taken, from a
        // pane with no turn stamp (`store::ACQUIRING_OPTION`, herdr's `begin_managed_agent`).
        self.turns = 0;
        self.acquiring = Some(Acquired::At(None));
    }

    /// `watch::nudge`: the pane is owed a read at its next tick, as for output.
    fn nudge(&mut self, t: u64) {
        if self.eligible_since.is_none() {
            self.eligible_since = Some(t);
        }
    }

    fn read(&mut self, t: u64) {
        self.now = t;
        let (command, container) = match self.container {
            Some(runtime) if self.frame.command() == "claude" => (
                runtime,
                Some(ContainerAgent {
                    runtime,
                    kind: "claude".to_string(),
                }),
            ),
            _ => (
                match (self.agent_command, self.frame.command()) {
                    (Some(command), "claude") => command,
                    (_, command) => command,
                },
                None,
            ),
        };
        let mut o = Observed {
            command: command.to_string(),
            title: self.frame.title().to_string(),
            window_active: self.foreground,
            session_attached: u32::from(self.foreground),
            terminal_focus: None,
            kind_hint: self.record.kind_hint.clone(),
            container,
            job_agent: None,
            prev: self.record.clone(),
            screen: self.frame.screen().to_string(),
            osc_progress: String::new(),
            pane_shell_idle: None,
            turn: (self.turns > 0).then(|| self.turns.to_string()),
            acquiring: self.acquiring.clone(),
            screen_unread: false,
            below_typed_line: false,
            suspended: false,
        };
        let now = self.at(t);
        // The watcher asks `ps` about an agent that reports for itself and that no process
        // names, and only that often. The pane's own shell is idle only when it is what the pane
        // shows: a command, an editor or the agent's interpreter has the terminal otherwise.
        if self.arbiter.wants_shell_probe(&o, now) {
            o.pane_shell_idle = Some(self.frame == Frame::S);
        }
        let decision = decide(
            &self.store,
            &mut self.cache,
            &o,
            false,
            now,
            &mut self.arbiter,
        );
        // `Watcher::pump` clears the stream's progress here; the scripted pane prints none.
        let _ = self.arbiter.take_progress_reset();
        self.apply(t, decision);
        self.reads.push(t);
        self.last_read = Some(t);
        self.eligible_since = None;
    }

    /// `Watcher::apply`: the decision's [`state::update`], made to the record instead of
    /// tmux, and its notice set waiting.
    fn apply(&mut self, t: u64, decision: Decision) {
        let update = state::update(&self.record, &decision).finish(&mut self.arbiter, &self.record);
        // Who the notice is earned for: the agent and the session the pane holds once this is
        // written (`Watcher::apply`).
        let earned = Earned::after(&self.record, &update);
        // What `Watcher::apply` announces: a change of state, however it was decided.
        match &decision {
            Decision::Transition { from, to, .. } => self.events.push((t, *from, Some(*to))),
            // A different agent in the same state is an event too, which says who is there.
            Decision::KindChanged { .. } => {
                self.events.push((t, self.record.state, self.record.state));
            }
            // A turn between two reads is two changes, announced one after the other.
            Decision::Reported {
                from,
                via: Some(via),
                to,
                ..
            } => {
                self.events.push((t, *from, Some(*via)));
                self.events.push((t, Some(*via), Some(*to)));
            }
            Decision::Reported {
                from, to, replaced, ..
            } if *from != Some(*to) || replaced.is_some() => {
                self.events.push((t, *from, Some(*to)));
            }
            Decision::NoAgent { clear: true } => self.events.push((t, self.record.state, None)),
            _ => {}
        }
        if update.clear {
            // `store::clear_agent` keeps the hint, as it keeps `@agent_watch_pid`.
            self.record = AgentRecord {
                kind_hint: self.record.kind_hint.take(),
                ..AgentRecord::default()
            };
            self.acquiring = None;
            self.written.push((t, Published::Cleared));
        }
        match update.acquiring {
            Some(state::MarkWrite::Set(at)) => self.acquiring = Some(at),
            Some(state::MarkWrite::Unset) => self.acquiring = None,
            None => {}
        }
        if let Some((to, rule)) = &update.state {
            self.record.state = Some(*to);
            self.record.rule.clone_from(rule);
            self.record.source = Some("manifest".to_string());
            self.written.push((t, Published::State(*to, rule.clone())));
        }
        if let Some((over, to)) = update.complete
            && self.record.state == Some(over)
        {
            // Over what the report left, with its hook still the source.
            self.record.state = Some(to);
            self.written.push((t, Published::State(to, None)));
        }
        if let Some(rule) = &update.rule {
            self.record.rule.clone_from(rule);
            self.written.push((t, Published::Rule(rule.clone())));
        }
        if let Some(kind) = &update.kind {
            self.record.kind = Some(kind.clone());
            if update.state.is_none() {
                self.written.push((t, Published::Kind(kind.clone())));
            }
        }
        if update.release_name {
            self.record.name = None;
        }
        if update.release_identity {
            self.record.session_id = None;
            self.record.session_path = None;
            self.record.reported_kind = None;
            self.record.tokens.remove(SESSION_START_TOKEN);
        }
        if let Some(seen) = update.seen {
            self.record.seen = seen;
        }
        if let Some(notice) = update.notice {
            let delay = delay_for(notice.class, &self.toast).as_millis() as u64;
            self.pending.push((t + delay, Pending { notice, earned }));
        }
        self.agent_present = !matches!(decision, Decision::NoAgent { .. });
    }

    /// The first notice whose delay is up at `t`: `Watcher::notify`'s thread, re-reading
    /// the record and checking that nobody is looking before it delivers.
    fn deliver(&mut self, t: u64) {
        self.now = t;
        let Some(index) = self.pending.iter().position(|(at, _)| *at == t) else {
            return;
        };
        let (_, pending) = self.pending.remove(index);
        let state = self.record.state.map(AgentState::as_str);
        if pending.due(state, &Earned::of(&self.record), || false) && !self.foreground {
            let class = pending.notice.class;
            self.toasts.push((t, class));
            // `notify::announce` runs the `[hooks] notification` commands beside the toast.
            self.notification_hooks
                .push((t, crate::notify::kind_word(class)));
        }
    }

    /// The record as tmux would hold it now.
    pub fn record(&self) -> &AgentRecord {
        &self.record
    }

    /// `taw agent start --kind <kind>` records the pane's kind hint before it launches
    /// the agent, and the agent's kind and name once it sees it running.
    pub fn set_hint(&mut self, kind: &str) {
        self.record.kind_hint = Some(kind.to_string());
    }

    /// `taw seen`, as `watch::mark_seen` applies it ([`state::marked_seen`]): the pane has
    /// been looked at, and a `done` goes back to `idle` with its rule and source kept. Not
    /// the watcher's write, so not in `written`.
    pub fn mark_seen(&mut self) {
        if let Some(seen) = state::marked_seen(&self.record) {
            self.record = seen;
        }
    }

    /// The user brings the pane's window to the front of an attached session.
    pub fn focus(&mut self) {
        self.foreground = true;
    }

    /// The user goes to another window: nobody is looking at the pane any more.
    pub fn blur(&mut self) {
        self.foreground = false;
    }
}

#[cfg(test)]
mod tests {
    use super::Frame::*;
    use super::*;
    use crate::detect;
    use AgentState::*;

    const FOREGROUND: bool = true;

    /// A claude sitting at its prompt, recorded as the watcher leaves one.
    fn idle_at_prompt() -> AgentRecord {
        AgentRecord {
            kind: Some("claude".to_string()),
            state: Some(Idle),
            rule: Some("live_prompt_box".to_string()),
            source: Some("manifest".to_string()),
            seen: true,
            ..AgentRecord::default()
        }
    }

    #[test]
    fn the_frames_read_as_the_scenarios_assume() {
        let store = ManifestStore::new(
            PathBuf::from("/nonexistent/taw-test-override"),
            PathBuf::from("/nonexistent/taw-test-remote"),
        );
        let claude = store.load("claude").unwrap();
        let verdict = |frame: Frame| {
            detect::evaluate(
                &claude,
                &detect::Snapshot {
                    content: frame.screen(),
                    osc_title: frame.title(),
                    osc_progress: "",
                },
            )
        };
        let w = verdict(W);
        assert_eq!((w.state, w.visible_working), (Working, true));
        for plain in [H, V] {
            let e = verdict(plain);
            assert_eq!(e.state, Idle, "{plain:?}");
            assert!(!e.visible_idle && !e.visible_blocker, "{plain:?}");
            assert_eq!(e.matched_rule, None, "{plain:?}");
        }
        let i = verdict(I);
        assert_eq!((i.state, i.visible_idle), (Idle, true));
        let b = verdict(B);
        assert_eq!((b.state, b.visible_blocker), (Blocked, true));
        let a = verdict(A);
        assert_eq!((a.state, a.visible_working), (Working, true));
        assert_eq!(
            a.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("taw_subagent_running")
        );
        let f = verdict(F);
        assert_eq!((f.state, f.visible_idle), (Idle, true));
        assert!(verdict(T).skip_state_update);
        assert_eq!(detect::identify(V.command(), None), None);
        assert_eq!(detect::identify(S.command(), None), None);

        // Codex's rules read a spinner frame in the title as its own turn, and any other
        // title as its own idle: the frame Claude's turn left behind reads as Codex working,
        // and the `✳` its prompt left reads as Codex idle, which is the evidence a
        // replacement must not inherit. Without a title the blank screen matches nothing.
        let codex = store.load("codex").unwrap();
        let codex_verdict = |frame: Frame, title: &str| {
            detect::evaluate(
                &codex,
                &detect::Snapshot {
                    content: frame.screen(),
                    osc_title: title,
                    osc_progress: "",
                },
            )
        };
        assert_eq!(
            detect::identify(C.command(), None).as_deref(),
            Some("codex")
        );
        for frame in [C, D] {
            let e = codex_verdict(frame, frame.title());
            let rule = e.matched_rule.as_ref().map(|rule| rule.id.as_str());
            assert_eq!(
                (e.state, rule),
                (Working, Some("osc_title_working")),
                "{frame:?}"
            );
        }
        for frame in [E, G] {
            let e = codex_verdict(frame, frame.title());
            let rule = e.matched_rule.as_ref().map(|rule| rule.id.as_str());
            assert_eq!((e.state, rule), (Idle, Some("osc_title_idle")), "{frame:?}");
            assert!(e.visible_idle, "{frame:?}");
        }
        for frame in [C, E] {
            let untitled = codex_verdict(frame, "");
            assert_eq!((untitled.state, untitled.matched_rule), (Idle, None));
        }
    }

    #[test]
    fn enter_then_half_drawn_then_spinner_reads_working_within_one_tick() {
        // The user replies: Claude echoes, draws half a frame, then spins. The first
        // tick after the spinner appears writes `working` — 150 ms after it here — and a
        // frame caught half-drawn before it never reads as the turn ending.
        let mut rows = vec![out(10, H), out(60, H), out(150, W)];
        rows.extend(every(W, 200, 1000, 50));
        let mut sim = Sim::anchored(idle_at_prompt(), FOREGROUND, rows);
        sim.run_until(1100);
        assert_eq!(sim.reads, [300, 600, 900]);
        assert_eq!(sim.written, [(300, st(Working, Some("live_turn_working")))]);
        // A new turn starts unseen, whoever is watching: `@agent_seen` is about the next
        // `working -> idle` edge.
        assert!(!sim.record().seen);
    }

    #[test]
    fn enter_then_half_drawn_then_spinner_reads_working_within_one_tick_worst_phase() {
        // The spinner lands just after a tick: the read at 300 still sees the half frame,
        // which is idle as before — only its missing evidence is written — and the next
        // tick, 280 ms after the spinner appeared, writes `working`.
        let mut rows = every(H, 10, 310, 50);
        rows.extend(every(W, 320, 1000, 50));
        let mut sim = Sim::anchored(idle_at_prompt(), FOREGROUND, rows);
        sim.run_until(1100);
        assert_eq!(sim.reads, [300, 600, 900]);
        assert_eq!(
            sim.written,
            [
                (300, Published::Rule(None)),
                (600, st(Working, Some("live_turn_working")))
            ]
        );
    }

    #[test]
    fn a_restarted_watcher_reads_at_once_and_anchors_its_grid_there() {
        // The first read at watcher start is due at once, output or not; the tick grid
        // then runs from it, so output at 100 is read at 300.
        let rows = vec![
            Row {
                t: 0,
                frame: I,
                output: false,
                report: None,
            },
            out(100, W),
        ];
        let mut sim = Sim::fresh(idle_at_prompt(), FOREGROUND, rows);
        sim.run_until(400);
        assert_eq!(sim.reads, [0, 300]);
        assert_eq!(sim.written, [(300, st(Working, Some("live_turn_working")))]);
    }

    const BACKGROUND: bool = false;

    /// A claude mid-turn, in the background and unseen: where `done` comes from.
    fn working_unseen() -> AgentRecord {
        AgentRecord {
            kind: Some("claude".to_string()),
            state: Some(Working),
            rule: Some("live_turn_working".to_string()),
            source: Some("manifest".to_string()),
            seen: false,
            ..AgentRecord::default()
        }
    }

    /// The spinner redrawing every 50 ms up to 1100 ms: the turn the scenarios end.
    fn turn() -> Vec<Row> {
        every(W, 50, 1100, 50)
    }

    #[test]
    fn one_non_matching_frame_mid_turn_keeps_working() {
        // One frame no working rule matches — a redraw caught half-way — ended the
        // turn at once, and in the background that made it `done`. It is held now: the
        // recheck 100 ms later sees the spinner back, and nothing is ever written.
        let mut rows = turn();
        rows.push(out(1150, H));
        rows.extend(every(W, 1230, 3000, 50));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(3000);
        assert_eq!(
            sim.reads,
            [300, 600, 900, 1200, 1300, 1600, 1900, 2200, 2500, 2800]
        );
        assert!(sim.written.is_empty(), "{:?}", sim.written);
    }

    #[test]
    fn one_non_matching_frame_mid_turn_keeps_working_two_reads() {
        // Two reads of the half-drawn frame are one confirmation, not a release.
        let mut rows = turn();
        rows.extend(every(H, 1150, 1350, 50));
        rows.push(out(1360, W));
        rows.extend(every(W, 1410, 3000, 50));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(3000);
        assert_eq!(
            sim.reads,
            [
                300, 600, 900, 1200, 1300, 1400, 1700, 2000, 2300, 2600, 2900
            ]
        );
        assert!(sim.written.is_empty(), "{:?}", sim.written);
    }

    #[test]
    fn spinner_then_real_prompt_is_written_at_once() {
        // The prompt box at rest is a visible idle: the turn's end, written on the read
        // that sees it, and `done` in the background.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.written, [(1200, st(Done, Some("live_prompt_box")))]);
        assert!(!sim.record().seen);
    }

    #[test]
    fn spinner_then_real_prompt_is_written_at_once_foreground() {
        // The same finish in front of the user is a plain idle they have seen.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.written, [(1200, st(Idle, Some("live_prompt_box")))]);
        assert!(sim.record().seen, "@agent_seen = 1");
    }

    #[test]
    fn spinner_then_plain_idle_is_written_once_after_three_confirmations() {
        // The other half: a turn that really ends on a frame no rule claims. The first
        // read starts the hold, the rechecks at 1300 and 1400 confirm it, 1500 releases
        // it — `done`, written once. Then nothing printed, so nothing is read until the
        // identity recheck, which finds the same frame.
        let mut rows = turn();
        rows.push(out(1150, H));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(7000);
        assert_eq!(sim.reads, [300, 600, 900, 1200, 1300, 1400, 1500, 6500]);
        assert_eq!(sim.written, [(1500, st(Done, None))]);

        // `taw seen` releases it, and the pane stays idle through the next recheck.
        sim.mark_seen();
        assert_eq!(sim.record().state, Some(Idle));
        assert!(sim.record().seen, "@agent_seen = 1");
        sim.run_until(12_000);
        assert_eq!(sim.reads.last(), Some(&11_500));
        assert_eq!(sim.written, [(1500, st(Done, None))]);
    }

    #[test]
    fn spinner_then_plain_idle_is_written_once_after_three_confirmations_seen_while_held() {
        // The user opens the pane while the idle is on hold: at release it is an idle
        // they are looking at, not `done`.
        let mut rows = turn();
        rows.push(out(1150, H));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(1250);
        assert!(sim.written.is_empty(), "the hold started at 1200");
        sim.focus();
        sim.mark_seen();
        sim.run_until(2000);
        assert_eq!(sim.written, [(1500, st(Idle, None))]);
        assert!(sim.record().seen);
    }

    #[test]
    fn a_blocker_is_written_at_once() {
        // From working.
        let mut rows = turn();
        rows.push(out(1150, B));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(1250);
        assert_eq!(
            sim.written,
            [(1200, st(Blocked, Some("bash_permission_prompt")))]
        );

        // From a hold: the blocker ends it and is written on the recheck that sees it.
        let mut rows = turn();
        rows.push(out(1150, H));
        rows.push(out(1250, B));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(1350);
        assert_eq!(sim.reads[3..], [1200, 1300]);
        assert_eq!(
            sim.written,
            [(1300, st(Blocked, Some("bash_permission_prompt")))]
        );

        // From blocked: the answered prompt's idle is written at once too. (In front of
        // the user; behind their back it is a completion, `done` — see below.)
        let mut rows = every(B, 50, 1400, 50);
        rows.push(out(1450, I));
        let mut sim = Sim::anchored(blocked_unseen(), FOREGROUND, rows);
        sim.run_until(1550);
        assert_eq!(sim.written, [(1500, st(Idle, Some("live_prompt_box")))]);
    }

    /// A claude stopped on a permission prompt, in the background and unseen.
    fn blocked_unseen() -> AgentRecord {
        AgentRecord {
            state: Some(Blocked),
            rule: Some("bash_permission_prompt".to_string()),
            ..working_unseen()
        }
    }

    #[test]
    fn a_turn_glanced_at_mid_turn_and_finished_while_away_is_done() {
        // Herdr's completion rule. The user looks in while the agent works — tmux's
        // focus hooks mark the pane seen — then goes to another window, and the turn
        // ends. Where they are looking when it ends is what counts: `done`, and its toast.
        // The glance earlier does not.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(400);
        sim.focus();
        sim.mark_seen();
        sim.run_until(700);
        sim.blur();
        sim.run_until(7000);
        assert_eq!(sim.reads, [300, 600, 900, 1200, 6200]);
        assert_eq!(sim.written, [(1200, st(Done, Some("live_prompt_box")))]);
        assert_eq!(sim.toasts, [(1200, Class::Done)]);
        // A `done` is unseen, whatever the glance left: the recheck at 6200 keeps it.
        assert!(!sim.record().seen);
    }

    #[test]
    fn a_turn_finished_while_looking_is_idle_and_earns_no_toast() {
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.written, [(1200, st(Idle, Some("live_prompt_box")))]);
        assert!(sim.record().seen, "@agent_seen = 1");
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_done_the_user_comes_to_without_a_focus_hook_is_released_by_the_next_read() {
        // The finish lands behind the user's back; they switch to it at 2000, but the
        // `taw seen` their focus hook ran had landed before the `done` was written, so the
        // mark is gone. The next read — the identity recheck, the pane being quiet — finds
        // them looking at it and writes the idle they see.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.written, [(1200, st(Done, Some("live_prompt_box")))]);
        sim.focus();
        sim.run_until(7000);
        assert_eq!(
            sim.written,
            [
                (1200, st(Done, Some("live_prompt_box"))),
                (6200, st(Idle, Some("live_prompt_box")))
            ]
        );
        assert!(sim.record().seen, "@agent_seen = 1");
    }

    #[test]
    fn a_blocked_agent_back_at_its_prompt_while_away_is_done() {
        // herdr counts `blocked -> idle` as a completion too: behind the user's back it
        // is `done`, with its toast, exactly like the end of a turn.
        let mut rows = every(B, 50, 1400, 50);
        rows.push(out(1450, I));
        let mut sim = Sim::anchored(blocked_unseen(), BACKGROUND, rows);
        sim.run_until(1550);
        assert_eq!(sim.written, [(1500, st(Done, Some("live_prompt_box")))]);
        assert_eq!(sim.toasts, [(1500, Class::Done)]);
        assert!(!sim.record().seen);
    }

    #[test]
    fn a_done_toast_waits_out_its_delay_and_is_delivered_if_nobody_looked() {
        // The default `[ui.toast]`: a request waits 1 s, a done 5 s. A background finish
        // at 1200 is delivered at 6200, when the pane still shows it and nobody is looking.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(6100);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        sim.run_until(7000);
        assert_eq!(sim.toasts, [(6200, Class::Done)]);
    }

    #[test]
    fn a_done_toast_is_dropped_when_the_agent_moves_on_within_its_delay() {
        // Claude ends a turn and picks up again a moment later: by the time the delay is
        // up the pane is working, so the finish is not news any more.
        let mut rows = turn();
        rows.push(out(1150, I));
        rows.extend(every(W, 3000, 7000, 50));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.written,
            [
                (1200, st(Done, Some("live_prompt_box"))),
                (3000, st(Working, Some("live_turn_working")))
            ]
        );
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_done_toast_is_dropped_when_the_user_opens_the_pane_within_its_delay() {
        // tmux's focus hooks run `taw seen`, which ends the `done`: nothing to tell them.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(3000);
        sim.focus();
        sim.mark_seen();
        sim.run_until(8000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_permission_prompt_rejected_in_front_of_the_user_earns_no_done_toast_when_they_leave() {
        // Herdr's completion rule for toasts: whether a finish is news is decided when
        // it happens. The user presses Esc on the permission prompt they are looking at —
        // `blocked -> idle`, seen — and goes to another window a second later, well inside
        // the done delay. Nothing they did not see happened, so nothing is toasted.
        let mut rows = every(B, 50, 1400, 50);
        rows.push(out(1450, I));
        let mut sim = Sim::anchored(blocked_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(2500);
        assert_eq!(sim.written, [(1500, st(Idle, Some("live_prompt_box")))]);
        sim.blur();
        sim.run_until(8000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        assert_eq!(
            sim.record().state,
            Some(Idle),
            "it is still the idle they saw"
        );
    }

    #[test]
    fn a_turn_finished_in_front_of_the_user_earns_no_done_toast_when_they_leave() {
        // The same for a turn's end: seen as it happened, so leaving straight after does
        // not make it news.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(1300);
        assert_eq!(sim.written, [(1200, st(Idle, Some("live_prompt_box")))]);
        sim.blur();
        sim.run_until(8000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    /// The sub-agent rule's own explanation, written beside a working state that has not
    /// moved: what `agent explain` and `@agent_rule` say once the turn's spinner is gone.
    fn subagents_rule() -> Published {
        Published::Rule(Some("taw_subagent_running".to_string()))
    }

    #[test]
    fn a_turn_that_ends_while_a_sub_agent_runs_is_not_done_and_earns_no_toast() {
        // The main agent's turn ends at 1150, the prompt box is back, and the sub-agent
        // is still counting under it, its time redrawn every second. herdr's rules read
        // that prompt box as a turn's end, and behind the user's back it was `done` and a
        // toast with the work unfinished. The pane stays `working`: the state does not
        // move, only the rule that explains it, and nothing is announced.
        let mut rows = turn();
        rows.extend(every(A, 1150, 30_000, 1000));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(30_000);
        assert_eq!(sim.written, [(1200, subagents_rule())]);
        assert_eq!(sim.record().state, Some(Working));
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn the_last_sub_agent_finishing_ends_the_turn_and_it_is_done_while_the_user_is_away() {
        // The sub-agent's row goes to `idle` at 20 000: the next read sees the prompt box
        // at rest with nothing counting, a visible idle, written at once and `done` for an
        // user who is away. Its toast is delivered after the done delay, five seconds on.
        let mut rows = turn();
        rows.extend(every(A, 1150, 19_150, 1000));
        rows.push(out(20_000, F));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(30_000);
        assert_eq!(
            sim.written,
            [
                (1200, subagents_rule()),
                (20_100, st(Done, Some("live_prompt_box")))
            ]
        );
        assert_eq!(sim.toasts, [(25_100, Class::Done)]);
        assert!(!sim.record().seen);
    }

    #[test]
    fn the_last_sub_agent_finishing_in_front_of_the_user_is_an_idle_they_saw() {
        // The same end with the user looking at the pane: `idle`, seen, and no toast.
        let mut rows = turn();
        rows.extend(every(A, 1150, 19_150, 1000));
        rows.push(out(20_000, F));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(30_000);
        assert_eq!(
            sim.written,
            [
                (1200, subagents_rule()),
                (20_100, st(Idle, Some("live_prompt_box")))
            ]
        );
        assert!(sim.record().seen, "@agent_seen = 1");
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_request_toast_waits_out_its_one_second() {
        let mut rows = turn();
        rows.push(out(1150, B));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(3000);
        assert_eq!(sim.toasts, [(2200, Class::Request)]);
    }

    // --- the notification hook runs wherever a toast is delivered ----------------------
    //
    // The sim does not call `notify::announce`: it writes `notification_hooks` beside its own
    // toast (`Sim::deliver`), as its model of what `announce` does. So these tests exercise the
    // model, that a hook has the toast's delay, its kind and its silence while the user looks.
    // What pins `announce` itself, the code that really runs the hook where a toast is
    // delivered, are the real-watcher tests in `tests/tmux_watch.rs` (the notification hook runs
    // when a toast would, and not while the user is looking at the pane).

    #[test]
    fn the_notification_hook_runs_with_a_done_toast_when_its_delay_is_up_and_says_done() {
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(6100);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );
        sim.run_until(7000);
        assert_eq!(sim.notification_hooks, [(6200, "done")]);
        assert_eq!(sim.toasts, [(6200, Class::Done)], "the same moment");
    }

    #[test]
    fn the_notification_hook_runs_with_a_request_toast_after_one_second_and_says_request() {
        let mut rows = turn();
        rows.push(out(1150, B));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(2100);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );
        sim.run_until(3000);
        assert_eq!(sim.notification_hooks, [(2200, "request")]);
    }

    #[test]
    fn the_notification_hook_never_runs_while_the_user_looks() {
        // A request is decided when its delay is up: the pane is in front of them.
        let mut rows = turn();
        rows.push(out(1150, B));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(4000);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );

        // A finish they watched is not news, and leaving straight after does not make it so.
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(1400);
        sim.blur();
        sim.run_until(8000);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );
    }

    #[test]
    fn the_notification_hook_does_not_run_for_a_finish_the_agent_moved_on_from() {
        let mut rows = turn();
        rows.push(out(1150, I));
        rows.extend(every(W, 3000, 7000, 50));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );
    }

    #[test]
    fn the_notification_hook_does_not_run_when_the_user_opens_the_pane_within_the_delay() {
        let mut rows = turn();
        rows.push(out(1150, I));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(3000);
        sim.focus();
        sim.mark_seen();
        sim.run_until(8000);
        assert!(
            sim.notification_hooks.is_empty(),
            "{:?}",
            sim.notification_hooks
        );
    }

    #[test]
    fn unknown_then_idle_is_not_a_completion() {
        // An agent becoming readable is not a turn ending (herdr v0.9.2, #4457, dropped
        // `unknown -> idle` from its completions): `idle`, no toast, in the background too.
        let unknown = AgentRecord {
            state: Some(Unknown),
            rule: None,
            ..working_unseen()
        };
        let mut sim = Sim::anchored(unknown, BACKGROUND, every(I, 50, 400, 50));
        sim.run_until(700);
        assert_eq!(sim.written, [(300, st(Idle, Some("live_prompt_box")))]);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn everything_else_is_written_on_the_read_that_sees_it() {
        // Only a plain idle after `working` waits. A finished agent starting its
        // next turn, and an idle one stopping on a permission prompt, are written on the
        // first read after the pane shows them.
        let finished = AgentRecord {
            state: Some(Done),
            rule: Some("live_prompt_box".to_string()),
            ..working_unseen()
        };
        let mut rows = every(I, 50, 1100, 50);
        rows.push(out(1150, W));
        let mut sim = Sim::anchored(finished, BACKGROUND, rows);
        sim.run_until(1250);
        assert_eq!(
            sim.written,
            [(1200, st(Working, Some("live_turn_working")))]
        );

        let mut rows = every(I, 50, 1100, 50);
        rows.push(out(1150, B));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows);
        sim.run_until(1250);
        assert_eq!(
            sim.written,
            [(1200, st(Blocked, Some("bash_permission_prompt")))]
        );
    }

    #[test]
    fn a_transcript_viewer_frame_keeps_working_and_ends_the_hold() {
        // The overlay keeps the state it hides and clears the hold, so the plain idle
        // after it needs its three confirmations again: `done` at 1900, not at 1600.
        let mut rows = turn();
        rows.push(out(1150, H));
        rows.push(out(1250, T));
        rows.push(out(1350, H));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(2500);
        assert_eq!(
            sim.reads,
            [300, 600, 900, 1200, 1300, 1600, 1700, 1800, 1900]
        );
        assert_eq!(sim.written, [(1900, st(Done, None))]);
    }

    /// `@agent_kind` with no state, as `taw agent start` leaves a pane: it writes the kind
    /// itself, before the watcher has looked.
    fn just_started() -> AgentRecord {
        AgentRecord {
            kind: Some("claude".to_string()),
            ..AgentRecord::default()
        }
    }

    #[test]
    fn a_new_agent_reads_unknown_for_three_seconds_and_one_tick() {
        // Whatever a new agent's first frames look like, it is `unknown` until 3 s
        // and one more tick have passed; the read at 3000 is that tick.
        let start = AgentRecord {
            kind_hint: Some("claude".to_string()),
            ..just_started()
        };
        let mut sim = Sim::fresh(start, BACKGROUND, every(I, 0, 3600, 200));
        sim.run_until(3400);
        let ticks: Vec<u64> = (0..=3300).step_by(300).collect();
        assert_eq!(sim.reads, ticks);
        assert_eq!(
            sim.written,
            [
                (0, st(Unknown, None)),
                (3300, st(Idle, Some("live_prompt_box")))
            ]
        );
    }

    #[test]
    fn a_new_agent_reads_unknown_for_three_seconds_and_one_tick_on_a_quiet_pane() {
        // Nothing printed after the first frame: the grace's end and the tick after it
        // are read anyway, or the pane would sit at `unknown` until the identity recheck.
        let mut sim = Sim::fresh(just_started(), BACKGROUND, vec![out(0, I)]);
        sim.run_until(4000);
        assert_eq!(sim.reads, [0, 3000, 3300]);
        assert_eq!(
            sim.written,
            [
                (0, st(Unknown, None)),
                (3300, st(Idle, Some("live_prompt_box")))
            ]
        );
    }

    #[test]
    fn a_new_agent_reads_unknown_for_three_seconds_and_one_tick_restart_on_a_classified_pane_has_no_grace()
     {
        // A watcher restarting on a pane it already classified keeps what tmux says.
        let mut sim = Sim::fresh(working_unseen(), BACKGROUND, every(W, 0, 900, 50));
        sim.run_until(1000);
        assert_eq!(sim.reads, [0, 300, 600, 900]);
        assert!(sim.written.is_empty(), "{:?}", sim.written);
    }

    #[test]
    fn a_new_agent_reads_unknown_for_three_seconds_and_one_tick_exit_during_grace() {
        // The shell back inside the window: the grace is over, and a pane that never got
        // past `unknown` is cleared at once. No hint, so the command is the identity.
        let mut rows = every(I, 0, 800, 200);
        rows.push(out(1000, S));
        let mut sim = Sim::fresh(just_started(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.reads, [0, 300, 600, 900, 1200]);
        assert_eq!(
            sim.written,
            [(0, st(Unknown, None)), (1200, Published::Cleared)]
        );
    }

    #[test]
    fn an_agent_that_exits_mid_turn_writes_done_then_clears() {
        // The shell back at its prompt while the agent was working: the turn's end
        // is written first — `done`, in the background — and the record is cleared on the
        // next tick, a read owed whether or not anything printed.
        let mut rows = turn();
        rows.push(out(1150, S));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.reads, [300, 600, 900, 1200, 1500]);
        assert_eq!(
            sim.written,
            [
                (1200, st(Done, Some("process_exited"))),
                (1500, Published::Cleared)
            ]
        );
    }

    #[test]
    fn an_agent_that_exits_mid_turn_writes_done_then_clears_from_idle_clears_at_once() {
        let mut rows = every(I, 50, 1100, 50);
        rows.push(out(1150, S));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.written, [(1200, Published::Cleared)]);
    }

    #[test]
    fn an_agent_that_exits_mid_turn_writes_done_then_clears_during_a_hold() {
        // A plain idle on hold, then the shell: the exit is the turn's end, written on the
        // recheck that sees it, and the hold goes with it.
        let mut rows = turn();
        rows.push(out(1150, H));
        rows.push(out(1250, S));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(2000);
        assert_eq!(sim.reads, [300, 600, 900, 1200, 1300, 1600]);
        assert_eq!(
            sim.written,
            [
                (1300, st(Done, Some("process_exited"))),
                (1600, Published::Cleared)
            ]
        );
    }

    #[test]
    fn an_agent_that_exits_mid_turn_writes_done_then_clears_in_the_foreground() {
        // In front of the user the same exit is an idle they have seen.
        let mut rows = turn();
        rows.push(out(1150, S));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows);
        sim.run_until(1400);
        assert_eq!(sim.written, [(1200, st(Idle, Some("process_exited")))]);
        assert!(sim.record().seen);
    }

    #[test]
    fn an_agent_that_exits_mid_turn_is_toasted_done_after_its_record_is_cleared() {
        // herdr writes the exit's idle first "so notifications and wait-agent callers
        // observe completion before the pane becomes unknown". The record is cleared a
        // tick later, long before the done delay is up: by then an empty `@agent_state`
        // is what the exit left, and the done it wrote still stands.
        let mut rows = turn();
        rows.push(out(1150, S));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.written,
            [
                (1200, st(Done, Some("process_exited"))),
                (1500, Published::Cleared)
            ]
        );
        assert_eq!(sim.toasts, [(6200, Class::Done)]);
    }

    #[test]
    fn an_agent_that_exits_mid_turn_in_front_of_the_user_is_not_toasted_when_they_leave() {
        // Seen as it happened (the exit's idle, in front of them), so not news.
        let mut rows = turn();
        rows.push(out(1150, S));
        let mut sim = Sim::anchored(working_unseen(), FOREGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(1400);
        sim.blur();
        sim.run_until(7000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn an_agent_started_again_before_the_exits_toast_takes_its_place() {
        // The user starts the agent again at 2000: by the time the done delay is up the
        // pane holds the new agent's startup `unknown`, and the old run's end is not news.
        let mut rows = turn();
        rows.push(out(1150, S));
        rows.extend(every(I, 2000, 7000, 200));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.written[..3],
            [
                (1200, st(Done, Some("process_exited"))),
                (1500, Published::Cleared),
                (2000, st(Unknown, None)),
            ]
        );
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn identity_survives_five_misses_and_clears_on_the_sixth() {
        // Claude's ctrl+g opens vim in front of it. A process that is neither the
        // agent nor its shell is a miss, not an exit: herdr keeps the agent through five
        // and judges the screen by the agent's rules, which read vim as a plain idle — so
        // the turn ends through the hold. One miss per identity recheck; the sixth clears.
        let mut rows = turn();
        rows.push(out(1150, V));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(27_000);
        assert_eq!(
            sim.reads,
            [
                300, 600, 900, 1200, 1300, 1400, 1500, 6500, 11_500, 16_500, 21_500, 26_500
            ]
        );
        assert_eq!(
            sim.written,
            [(1500, st(Done, None)), (26_500, Published::Cleared)]
        );
    }

    #[test]
    fn an_identified_agent_that_named_itself_follows_the_ordinary_miss_rule_behind_vim() {
        // The same ctrl+g, on a pane whose agent also said what it is through its own hook
        // (`@agent_reported_kind`). taw names Claude by its process, so the word changes
        // nothing: herdr keeps only an agent it cannot identify however long another program
        // stays in front of it, and judges the rest by their misses. The turn ends through the
        // hold, and the sixth miss clears the record, as it does for a Claude nothing reported.
        let named = AgentRecord {
            reported_kind: Some("claude".to_string()),
            ..working_unseen()
        };
        let mut rows = turn();
        rows.push(out(1150, V));
        let mut sim = Sim::anchored(named, BACKGROUND, rows);
        sim.run_until(27_000);
        assert_eq!(
            sim.written,
            [(1500, st(Done, None)), (26_500, Published::Cleared)]
        );
        assert_eq!(sim.record().kind, None);
    }

    #[test]
    fn an_agent_no_process_names_outlasts_an_hour_behind_vim_and_ends_with_the_panes_shell() {
        // The same ctrl+g, on a Claude that an npm install runs under `node`: nothing in the
        // process table names it, so taw cannot identify it, and its hook's word is all there
        // is of it. herdr keeps such an agent until the pane's own shell is idle, so no miss is
        // counted: the turn ends through the hold as before, an hour of 5 s rechecks clears
        // nothing, and the shell coming back is what clears the pane.
        let named = AgentRecord {
            reported_kind: Some("claude".to_string()),
            ..working_unseen()
        };
        let mut rows = turn();
        rows.push(out(1150, V));
        rows.push(out(3_600_000, S));
        let mut sim = Sim::anchored(named, BACKGROUND, rows).under("node");
        sim.run_until(3_599_999);
        assert_eq!(sim.written, [(1500, st(Done, None))]);
        assert_eq!(sim.record().kind.as_deref(), Some("claude"));
        // The hold's seven reads, then one every 5 s: hundreds of rechecks, each one a miss
        // under the ordinary rule, and none of them cleared it.
        let rechecks = &sim.reads[7..];
        assert!(rechecks.len() > 700, "{} rechecks", rechecks.len());
        assert!(rechecks.windows(2).all(|pair| pair[1] - pair[0] == 5000));
        sim.run_until(3_601_000);
        assert_eq!(sim.written.len(), 2, "{:?}", sim.written);
        let (at, published) = &sim.written[1];
        assert_eq!(*published, Published::Cleared);
        assert!(
            (3_600_000..=3_600_500).contains(at),
            "cleared at {at}, not by the shell that arrived at 3 600 000"
        );
        assert_eq!(sim.record().kind, None);
    }

    #[test]
    fn a_hook_reported_agent_is_released_when_its_shell_returns_and_not_before() {
        // A pane whose state its own hook reports: the watcher writes nothing for it while
        // the agent runs, whatever is in front of it for however long (vim, from ctrl+g,
        // for ten minutes), and no miss is counted. Its shell coming back is the sign that
        // it has exited: the turn's end is written first, and that write hands the pane to
        // the manifest, so the read after it finds a pane no hook owns and clears it.
        let hooked = AgentRecord {
            source: Some("hook:claude-code".to_string()),
            ..working_unseen()
        };
        let mut rows = turn();
        rows.push(out(1150, V));
        rows.push(out(600_000, S));
        let mut sim = Sim::anchored(hooked, BACKGROUND, rows);
        sim.run_until(599_999);
        assert!(sim.written.is_empty(), "{:?}", sim.written);
        assert_eq!(sim.record().source.as_deref(), Some("hook:claude-code"));
        assert_eq!(sim.record().state, Some(Working), "the hook's, untouched");

        sim.run_until(601_000);
        assert_eq!(sim.written.len(), 2, "{:?}", sim.written);
        let (ended, published) = &sim.written[0];
        assert_eq!(*published, st(Done, Some("process_exited")));
        let (cleared, published) = &sim.written[1];
        assert_eq!(*published, Published::Cleared);
        assert!(
            (600_000..=600_500).contains(ended) && cleared > ended && *cleared <= 601_000,
            "the end at {ended}, the clear at {cleared}, for a shell that arrived at 600 000"
        );
        assert_eq!(sim.record().kind, None);
        assert_eq!(sim.record().source, None);
    }

    #[test]
    fn identity_survives_five_misses_and_clears_on_the_sixth_while_vim_redraws() {
        // Read every tick while vim prints, the agent still loses one miss per 5 s, not
        // one per read: misses at 1200, 6300, 11 400, 16 500 and 21 600, the sixth at
        // 26 700, each on the first tick 5 s after the last.
        let mut rows = turn();
        rows.extend(every(V, 1150, 30_000, 50));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(30_000);
        assert_eq!(
            sim.written,
            [(1500, st(Done, None)), (26_700, Published::Cleared)]
        );
    }

    #[test]
    fn identity_survives_five_misses_and_clears_on_the_sixth_agent_returns() {
        // Vim closes after the third miss and the spinner is back: the agent is written
        // working again, and its misses start from nothing — the next spell in vim is
        // cleared on its own sixth miss, not on the ninth overall.
        let mut rows = turn();
        rows.push(out(1150, V));
        rows.push(out(12_000, W));
        rows.push(out(14_000, V));
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(40_000);
        assert_eq!(
            sim.written,
            [
                (1500, st(Done, None)),
                (12_100, st(Working, Some("live_turn_working"))),
                (14_500, st(Done, None)),
                (39_500, Published::Cleared)
            ]
        );
    }

    #[test]
    fn a_replacement_agent_reads_no_title_until_it_sets_its_own() {
        // Herdr drops a replaced agent's OSC evidence: Claude's turn is cut off with
        // its last spinner frame as the pane's title; Codex takes the pane and has set none
        // of its own. Codex's `osc_title_working` would read Claude's frame as Codex's own
        // turn, so the title reads as none until tmux reports another: after the startup
        // grace the blank screen is a plain idle, and only Codex's own spinner title gives
        // it `working`.
        let rows = vec![out(0, C), out(5000, D)];
        let mut sim = Sim::anchored(working_unseen(), BACKGROUND, rows);
        sim.run_until(5500);
        assert_eq!(sim.reads, [300, 3300, 3600, 5100]);
        assert_eq!(
            sim.written,
            [
                (300, st(Unknown, None)),
                (3600, st(Idle, None)),
                (5100, st(Working, Some("osc_title_working"))),
            ]
        );
    }

    #[test]
    fn a_replacement_agent_reads_no_idle_title_until_it_sets_its_own() {
        // Herdr drops a replaced agent's OSC evidence: Claude, at its prompt, leaves
        // `✳ fake task` as the pane's title; Codex takes the pane and has set none of its
        // own. Codex's `osc_title_idle` would read Claude's title as Codex's idle, so the
        // title reads as none until tmux reports another: after the startup grace the blank
        // screen is a plain idle, and only Codex's own title gives it `osc_title_idle`.
        let mut rows = every(I, 50, 900, 50);
        rows.push(out(1000, E));
        rows.push(out(5000, G));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows);
        sim.run_until(5500);
        assert_eq!(sim.reads, [300, 600, 900, 1200, 4200, 4500, 5100]);
        assert_eq!(
            sim.written,
            [
                (1200, st(Unknown, None)),
                (4500, st(Idle, None)),
                (5100, Published::Rule(Some("osc_title_idle".to_string()))),
            ]
        );
    }

    #[test]
    fn a_replacement_agent_does_not_inherit_the_name() {
        // Herdr's name owner: Claude named `reviewer` gives the pane up to Codex, and
        // the name goes with Claude: it is the handle `taw agent prompt reviewer` types
        // into, and Codex was never given it.
        let named = AgentRecord {
            name: Some("reviewer".to_string()),
            ..idle_at_prompt()
        };
        let mut rows = every(I, 50, 900, 50);
        rows.push(out(1000, C));
        let mut sim = Sim::anchored(named, BACKGROUND, rows);
        sim.run_until(1100);
        assert_eq!(sim.record().name.as_deref(), Some("reviewer"));
        sim.run_until(1300);
        assert_eq!(sim.written, [(1200, st(Unknown, None))]);
        assert_eq!(sim.record().kind.as_deref(), Some("codex"));
        assert_eq!(sim.record().name, None);
    }

    #[test]
    fn a_name_taw_agent_start_is_writing_is_left_to_it() {
        // `taw agent start --kind codex --name coder` in Claude's pane records the hint,
        // launches Codex, then writes Codex's kind and name. A read landing between the
        // hint and the name sees Codex take over from Claude — and must not remove the
        // name, or it could remove `coder` the moment after `agent start` wrote it.
        let named = AgentRecord {
            name: Some("reviewer".to_string()),
            ..idle_at_prompt()
        };
        let mut rows = every(I, 50, 900, 50);
        rows.push(out(1000, C));
        let mut sim = Sim::anchored(named, BACKGROUND, rows);
        sim.run_until(1000);
        sim.set_hint("codex");
        sim.run_until(1300);
        assert_eq!(sim.record().kind.as_deref(), Some("codex"));
        assert_eq!(sim.record().name.as_deref(), Some("reviewer"));
    }

    #[test]
    fn a_quiet_pane_is_read_only_by_the_identity_recheck() {
        // Nothing printed since the read at 0: no tick is owed anything until the 5 s
        // recheck, which finds the prompt it left and writes nothing.
        let mut sim = Sim::anchored(
            idle_at_prompt(),
            FOREGROUND,
            vec![Row {
                t: 0,
                frame: I,
                output: false,
                report: None,
            }],
        );
        sim.run_until(12_000);
        assert_eq!(sim.reads, [5000, 10_000]);
        assert!(sim.written.is_empty(), "{:?}", sim.written);
    }

    /// The same script run by a watcher on a pane whose agent runs on the host and by one on
    /// a pane whose agent runs in a container, with the done toast's usual delay. What the
    /// user sees of the two must be the same.
    fn host_and_container(
        make: fn(AgentRecord, bool, Vec<Row>) -> Sim,
        start: AgentRecord,
        foreground: bool,
        rows: Vec<Row>,
        until: u64,
    ) -> [Sim; 2] {
        let mut host = make(start.clone(), foreground, rows.clone()).with_toast_delays(1.0, 5.0);
        let mut boxed = make(start, foreground, rows)
            .in_container("docker")
            .with_toast_delays(1.0, 5.0);
        host.run_until(until);
        boxed.run_until(until);
        [host, boxed]
    }

    #[test]
    fn an_agent_in_a_container_goes_through_the_states_it_has_on_the_host() {
        // A container run by a shell function: docker in front of the pane, Claude in the container. Its
        // startup grace, its turn, and the finish they were not there to see are the same
        // states, at the same instants, with the same toast as for a Claude on the host.
        let mut rows = vec![out(0, I)];
        rows.extend(every(W, 4000, 6000, 50));
        rows.push(out(6150, I));
        let [host, boxed] =
            host_and_container(Sim::fresh, AgentRecord::default(), BACKGROUND, rows, 12_000);
        assert_eq!(
            boxed.written,
            [
                (0, st(Unknown, None)),
                (3300, st(Idle, Some("live_prompt_box"))),
                (4200, st(Working, Some("live_turn_working"))),
                (6300, st(Done, Some("live_prompt_box")))
            ]
        );
        assert_eq!(boxed.toasts, [(11_300, Class::Done)]);
        assert_eq!(boxed.reads, host.reads);
        assert_eq!(boxed.written, host.written);
        assert_eq!(boxed.toasts, host.toasts);
        assert_eq!(boxed.record().kind.as_deref(), Some("claude"));
        assert_eq!(boxed.record().state, host.record().state);
    }

    #[test]
    fn an_agent_in_a_container_that_exits_mid_turn_ends_as_one_on_the_host_does() {
        // A turn 40 s long, and the container stops in the middle of it: the shell is back,
        // the turn's end is written first, and the record is cleared on the next tick.
        // Docker in front of the pane for the whole turn is not a reason to let go of the
        // agent, as another process in front of one would be after five misses.
        let mut rows = every(W, 50, 40_000, 50);
        rows.push(out(40_050, S));
        let [host, boxed] =
            host_and_container(Sim::anchored, working_unseen(), BACKGROUND, rows, 50_000);
        assert_eq!(
            boxed.written,
            [
                (40_200, st(Done, Some("process_exited"))),
                (40_500, Published::Cleared)
            ]
        );
        assert_eq!(boxed.written, host.written);
        assert_eq!(boxed.toasts, host.toasts);
        assert_eq!(boxed.toasts, [(45_200, Class::Done)]);
        assert_eq!(boxed.record().kind, None);
    }

    #[test]
    fn an_agent_in_a_container_that_asks_for_permission_late_in_a_turn_is_blocked_and_toasted() {
        let mut rows = every(W, 50, 40_000, 50);
        rows.push(out(40_050, B));
        let [host, boxed] =
            host_and_container(Sim::anchored, working_unseen(), BACKGROUND, rows, 45_000);
        assert_eq!(
            boxed.written,
            [(40_200, st(Blocked, Some("bash_permission_prompt")))]
        );
        assert_eq!(boxed.toasts, [(41_200, Class::Request)]);
        assert_eq!(boxed.written, host.written);
        assert_eq!(boxed.toasts, host.toasts);
    }

    // --- an agent that reports its own state ---------------------------------------------------
    //
    // Its reports are written to the pane by `taw pane report-agent`, which nudges the watcher, and
    // the watcher announces each change of state as it does one it reads off the screen: the same
    // states, at the same ticks, with the same toasts. What differs is what writes the state: the
    // agent, in the moment it reports, and the watcher only where the completion rule has another
    // word for it (`done`).

    /// An OpenCode at rest, reporting its own state, and the watcher's last read of it at t=0.
    fn hooked_idle() -> AgentRecord {
        AgentRecord {
            kind: Some("opencode".to_string()),
            state: Some(Idle),
            source: Some("hook:sim".to_string()),
            seen: true,
            ..AgentRecord::default()
        }
    }

    /// `rows` for a pane its agent reports for, which shows a program that prints nothing.
    fn reports(reports: Vec<Row>) -> Vec<Row> {
        let mut rows = vec![out(0, P)];
        rows.extend(reports);
        rows
    }

    /// The same turn read off the screen and reported by the agent: `screen` are the frames a Claude
    /// draws, and `reported` are what an agent that reports for itself says as they happen.
    fn screen_and_report(
        foreground: bool,
        screen: Vec<Row>,
        reported: Vec<Row>,
        until: u64,
    ) -> [Sim; 2] {
        let mut read =
            Sim::anchored(idle_at_prompt(), foreground, screen).with_toast_delays(1.0, 5.0);
        let mut told =
            Sim::anchored(hooked_idle(), foreground, reports(reported)).with_toast_delays(1.0, 5.0);
        read.run_until(until);
        told.run_until(until);
        [read, told]
    }

    #[test]
    fn a_turn_an_agent_reports_is_announced_at_the_same_ticks_with_the_same_toast_as_one_read_off_the_screen()
     {
        // The spinner turns from 50 ms and the prompt is back at 1150: a Claude says so on its
        // screen, an OpenCode in its reports, and the user is away. `working` is seen at the tick
        // after it starts, `done` at the tick after it ends, and its toast after the done delay.
        let mut screen = turn();
        screen.push(out(1150, I));
        let [read, told] = screen_and_report(
            BACKGROUND,
            screen,
            vec![report(50, Working), report(1150, Idle)],
            7000,
        );
        let events = [
            (300, Some(Idle), Some(Working)),
            (1200, Some(Working), Some(Done)),
        ];
        assert_eq!(read.events, events);
        assert_eq!(told.events, events);
        assert_eq!(told.toasts, [(6200, Class::Done)]);
        assert_eq!(told.toasts, read.toasts);
        assert_eq!(told.notification_hooks, read.notification_hooks);
        // What the pane holds: the watcher's `done` over the agent's `idle`, unseen, with the hook
        // still the user. The watcher wrote nothing else: the `working` is the agent's own.
        assert_eq!(told.record().state, Some(Done));
        assert_eq!(told.record().source.as_deref(), Some("hook:sim"));
        assert!(!told.record().seen, "@agent_seen = 0");
        assert_eq!(told.written, [(1200, st(Done, None))]);
        assert_eq!(told.record().state, read.record().state);
    }

    #[test]
    fn a_turn_an_agent_reports_in_front_of_the_user_is_an_idle_they_saw_and_earns_no_toast() {
        let mut screen = turn();
        screen.push(out(1150, I));
        let [read, told] = screen_and_report(
            FOREGROUND,
            screen,
            vec![report(50, Working), report(1150, Idle)],
            7000,
        );
        let events = [
            (300, Some(Idle), Some(Working)),
            (1200, Some(Working), Some(Idle)),
        ];
        assert_eq!(read.events, events);
        assert_eq!(told.events, events);
        assert!(told.toasts.is_empty(), "{:?}", told.toasts);
        assert!(told.notification_hooks.is_empty());
        assert_eq!(told.record().state, Some(Idle));
        assert!(told.record().seen, "@agent_seen = 1");
        assert!(
            told.written.is_empty(),
            "the agent's own idle stands: {:?}",
            told.written
        );
    }

    #[test]
    fn an_agent_that_reports_blocked_is_announced_and_toasted_a_request_as_one_that_shows_it() {
        let mut screen = turn();
        screen.push(out(1150, B));
        let [read, told] = screen_and_report(
            BACKGROUND,
            screen,
            vec![report(50, Working), report(1150, Blocked)],
            4000,
        );
        let events = [
            (300, Some(Idle), Some(Working)),
            (1200, Some(Working), Some(Blocked)),
        ];
        assert_eq!(read.events, events);
        assert_eq!(told.events, events);
        assert_eq!(told.toasts, [(2200, Class::Request)]);
        assert_eq!(told.toasts, read.toasts);
        assert_eq!(told.notification_hooks, [(2200, "request")]);
        assert!(told.written.is_empty(), "{:?}", told.written);
    }

    #[test]
    fn a_report_repeated_is_announced_once_and_an_idle_repeated_over_done_leaves_it_done() {
        // OpenCode reports `working` for each tool it runs, and `idle` from `session.status` and
        // again from `session.idle`. The watcher reads some of them and announces each change once;
        // the second `idle` lands on the `done` it wrote, and it puts the `done` back.
        let mut rows = vec![report(50, Working)];
        rows.extend((350..=950).step_by(300).map(|t| report(t, Working)));
        rows.extend([report(1150, Idle), report(1250, Idle), report(2000, Idle)]);
        let mut sim =
            Sim::anchored(hooked_idle(), BACKGROUND, reports(rows)).with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (1200, Some(Working), Some(Done))
            ]
        );
        assert_eq!(
            sim.written,
            [
                (1200, st(Done, None)),
                (1500, st(Done, None)),
                (2100, st(Done, None))
            ]
        );
        assert_eq!(sim.record().state, Some(Done), "unseen, still");
        assert_eq!(sim.toasts, [(6200, Class::Done)], "one finish, one toast");
    }

    #[test]
    fn a_done_the_user_opens_is_an_idle_no_report_is_announced_for_and_the_next_turn_is_a_change_from_it()
     {
        let mut sim = Sim::anchored(
            hooked_idle(),
            BACKGROUND,
            reports(vec![
                report(50, Working),
                report(1150, Idle),
                report(8000, Working),
            ]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(2000);
        assert_eq!(sim.record().state, Some(Done));
        // tmux's focus hook: `taw seen` turns the `done` back into `idle`.
        sim.focus();
        sim.mark_seen();
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (1200, Some(Working), Some(Done))
            ],
            "taw seen fired nothing"
        );
        assert!(
            sim.toasts.is_empty(),
            "opened within the delay: {:?}",
            sim.toasts
        );
        sim.run_until(9000);
        assert_eq!(
            sim.events[2..],
            [(8000, Some(Idle), Some(Working))],
            "the next turn changes the idle they saw"
        );
    }

    #[test]
    fn an_agent_that_reports_its_last_state_and_quits_is_announced_done_once_and_toasted() {
        // `opencode run`: the last report is `idle`, and the process ends with it. The watcher reads
        // the report first and the shell's return a tick later (it asks about the shell once a
        // second), and the done toast outlives the record, as any exit mid-turn's does.
        let mut sim = Sim::anchored(
            hooked_idle(),
            BACKGROUND,
            reports(vec![report(50, Working), report(1150, Idle), out(1150, S)]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (1200, Some(Working), Some(Done)),
                (1500, Some(Done), None)
            ]
        );
        assert_eq!(
            sim.record().kind,
            None,
            "the record went with the shell's return"
        );
        assert_eq!(sim.toasts, [(6200, Class::Done)], "after the record went");
    }

    #[test]
    fn a_report_and_the_shells_return_found_by_the_same_read_are_one_done_and_one_toast() {
        // The same, where the read that finds the last report finds the pane's shell back too: the
        // turn was announced as started and is owed its end, which is written with the exit.
        let mut sim = Sim::anchored(
            hooked_idle(),
            BACKGROUND,
            reports(vec![report(50, Working), report(6500, Idle), out(6500, S)]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(13_000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (6500, Some(Working), Some(Done)),
                (6800, Some(Done), None)
            ]
        );
        assert_eq!(sim.written[0], (6500, st(Done, Some("process_exited"))));
        assert_eq!(sim.toasts, [(11_500, Class::Done)]);
        assert_eq!(sim.record().kind, None);
    }

    #[test]
    fn an_agent_that_takes_a_pane_and_reports_the_state_it_is_in_fires_the_status_event_and_the_name_goes()
     {
        // A Claude is working in the pane, named `reviewer`, and an OpenCode reports `working` there
        // from its own hook. The state is what was announced; the occupant is not, and the event
        // says who is there, with no agent event and no toast, as for an agent a screen read finds.
        let start = AgentRecord {
            name: Some("reviewer".to_string()),
            reported_kind: Some("claude".to_string()),
            ..working_unseen()
        };
        let mut sim = Sim::anchored(
            start,
            BACKGROUND,
            reports(vec![report_as(50, "opencode", Working)]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(1000);
        assert_eq!(sim.events, [(300, Some(Working), Some(Working))]);
        assert_eq!(sim.record().kind.as_deref(), Some("opencode"));
        assert_eq!(sim.record().name, None, "the name was the Claude's");
        assert_eq!(sim.record().state, Some(Working));
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        // Reporting again is the same agent again: nothing.
        let mut sim = Sim::anchored(
            AgentRecord {
                kind: Some("opencode".to_string()),
                source: Some("hook:sim".to_string()),
                ..working_unseen()
            },
            BACKGROUND,
            reports(vec![report_as(50, "opencode", Working)]),
        );
        sim.run_until(1000);
        assert!(sim.events.is_empty(), "{:?}", sim.events);
    }

    #[test]
    fn a_turn_shorter_than_a_tick_is_announced_as_its_start_and_its_end_and_toasted_once() {
        // `working` at 50 ms and `idle` at 60: both are on the pane before the watcher's read at 300,
        // which finds the state it had taken in (idle) and the stamp moved. herdr applies each report;
        // the watcher announces the start and the end from the one read, and the done toast follows
        // its delay.
        let mut sim = Sim::anchored(
            hooked_idle(),
            BACKGROUND,
            reports(vec![report(50, Working), report(60, Idle)]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (300, Some(Working), Some(Done))
            ]
        );
        assert_eq!(sim.written, [(300, st(Done, None))]);
        assert_eq!(sim.toasts, [(5300, Class::Done)]);
        assert_eq!(sim.notification_hooks, [(5300, "done")]);
        assert!(!sim.record().seen, "@agent_seen = 0");

        // In front of the user it is an idle they saw, and earns no toast.
        let mut sim = Sim::anchored(
            hooked_idle(),
            FOREGROUND,
            reports(vec![report(50, Working), report(60, Idle)]),
        )
        .with_toast_delays(1.0, 5.0);
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (300, Some(Idle), Some(Working)),
                (300, Some(Working), Some(Idle))
            ]
        );
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        assert!(sim.record().seen, "@agent_seen = 1");
    }

    #[test]
    fn the_first_report_on_a_pane_is_read_though_the_pane_printed_nothing() {
        // A watcher that has read an agentless pane reads nothing more until it prints, and a quiet
        // pane prints nothing: the nudge the report sends is what makes it read.
        let rows = vec![out(0, P), report(2000, Working), report(3000, Blocked)];
        let mut sim =
            Sim::fresh(AgentRecord::default(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(6000);
        // On the grid of a pane with no agent at first (500 ms), and of one with an agent once it
        // has one (300 ms): nothing is read in between, and a read owed to no output is none.
        assert_eq!(sim.reads, [0, 2000, 3200], "nothing is read in between");
        assert_eq!(
            sim.events,
            [
                (2000, None, Some(Working)),
                (3200, Some(Working), Some(Blocked))
            ]
        );
        assert_eq!(sim.toasts, [(4200, Class::Request)]);
    }

    // Follows herdr's src/app/actions.rs, its eight `completion_guard_*`
    // tests (lines 3095 to 3300, the completion guards of #4457, released in v0.9.2), and the two
    // headless tests of the same commit (28360107c1, `src/server/headless/tests/mod.rs`).
    // Differences: on the fake clock, with the frames and the reports of a pane where herdr sends
    // state events to its app; herdr is told of a new agent (`AgentProcessDetected`) and of a
    // managed launch (`begin_managed_agent`), which the watcher finds by a read that takes a new
    // agent or a pane `taw agent start` marked; herdr's unseen completion is the `done` taw writes,
    // so where herdr asserts that a pane is unseen these assert that it is `done`, and where it
    // asserts a pending notification these assert the notice waiting out its delay
    // (`Sim::pending`), which is dropped when it comes due where herdr's reset takes it away at
    // once; herdr's `completion_seq` has no counterpart. The managed launch test leaves out
    // herdr's `managed_agent_interactive_ready()`, which is `taw agent start`'s own wait here. The
    // session replacement test starts from the state the pane holds, where herdr's starts from a
    // hook authority at `idle` over a detected state that the replacement drops, and its loop over
    // the reasons that start a session comes to one case here, as taw takes every session report
    // whatever its reason.

    /// A Claude the watcher has long been watching at `unknown`, as herdr's terminal starts.
    fn unknown_claude() -> AgentRecord {
        AgentRecord {
            state: Some(Unknown),
            rule: None,
            seen: true,
            ..idle_at_prompt()
        }
    }

    /// The frame a Claude draws in `state`.
    fn drawn(state: AgentState) -> Frame {
        match state {
            Working => W,
            Blocked => B,
            Idle => I,
            other => panic!("no frame reads {other:?}"),
        }
    }

    /// herdr's `assert_completion_guard_sequence`: a pane in the background, its toasts not
    /// delayed, whose Claude the watcher takes as a new one when `acquired` (a read that finds it
    /// and gives it its startup grace, herdr's `AgentProcessDetected`), and has long watched at
    /// `unknown` when not, goes through `states` one second apart, each read off its screen
    /// (`unknown`, the state herdr's terminal starts in, is the one it is already in). Whether it
    /// ends in a completion: the last `idle` written `done`, the change into it announced, and its
    /// toast delivered; no completion is an `idle` and no done toast.
    fn assert_completion_guard_sequence(
        acquired: bool,
        states: &[AgentState],
        expect_completion: bool,
    ) {
        let shown: Vec<AgentState> = states.iter().copied().filter(|s| *s != Unknown).collect();
        // A new agent's first frame is on screen when it is taken, and is believed only once its
        // grace is over (the read at 3300); every later one a second after the one before.
        let at = |i: usize| if acquired { 3000 } else { 0 } + 1000 * i as u64;
        let rows: Vec<Row> = shown
            .iter()
            .enumerate()
            .map(|(i, state)| out(if acquired && i == 0 { 0 } else { at(i) }, drawn(*state)))
            .collect();
        let end = at(shown.len()) + 1000;
        let mut sim = if acquired {
            Sim::fresh(just_started(), BACKGROUND, rows)
        } else {
            Sim::anchored(unknown_claude(), BACKGROUND, rows)
        };
        sim.run_until(end);
        let finished = sim
            .toasts
            .iter()
            .filter(|(_, class)| *class == Class::Done)
            .count();
        assert_eq!(
            finished,
            usize::from(expect_completion),
            "completion notification: {states:?}, toasts {:?}",
            sim.toasts
        );
        let last = if expect_completion { Done } else { Idle };
        assert_eq!(
            sim.record().state,
            Some(last),
            "unseen completion: {states:?}"
        );
        assert_eq!(
            sim.events.last().map(|(_, _, to)| *to),
            Some(Some(last)),
            "the last change announced: {states:?}, {:?}",
            sim.events
        );
        if expect_completion {
            assert!(!sim.record().seen, "a done is unseen: {states:?}");
        }
    }

    #[test]
    fn completion_guard_unknown_to_idle_is_not_completed_work() {
        assert_completion_guard_sequence(false, &[Unknown, Idle], false);
    }

    #[test]
    fn completion_guard_first_work_finishes_without_prior_idle() {
        assert_completion_guard_sequence(true, &[Working, Idle], true);
    }

    #[test]
    fn completion_guard_first_work_can_pause_for_permission() {
        assert_completion_guard_sequence(true, &[Working, Blocked, Idle], true);
    }

    #[test]
    fn completion_guard_startup_trust_is_not_completed_work() {
        // The user's case: a Claude started in a background session stops at its folder trust
        // question, and is answered later. It is waiting for them, which is a request; that it is
        // ready afterwards is no finished turn.
        assert_completion_guard_sequence(true, &[Blocked, Idle], false);
    }

    #[test]
    fn completion_guard_managed_launch_readiness_does_not_swallow_work() {
        // `taw agent start` in the pane of a Claude the watcher has long watched at its prompt: the
        // record is taken off and the agent marked as being taken (herdr's `begin_managed_agent`),
        // then `claude` typed. The new process is a Claude too, the same agent to the arbiter, so no
        // startup grace takes it as a new one: the mark is what makes it one being taken, and its
        // first `idle` after a trust question is no completion, and the first after real work is.
        // herdr also asserts `managed_agent_interactive_ready()`, the end of its managed launch;
        // taw's is `taw agent start`'s own wait for the agent to come up (`cli::launch`), which has
        // nothing on the fake clock to run, and which tests/e2e_agent_start.rs covers.
        for first in [Blocked, Working] {
            let rows = vec![
                out(0, I),
                start_as(500, "claude"),
                out(600, drawn(first)),
                out(5000, I),
            ];
            let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows);
            sim.run_until(8000);
            let finished = sim
                .toasts
                .iter()
                .filter(|(_, class)| *class == Class::Done)
                .count();
            assert_eq!(
                finished == 1,
                first == Working,
                "{first:?}: {:?}",
                sim.toasts
            );
            assert_eq!(
                sim.record().state == Some(Done),
                first == Working,
                "{first:?}: {:?}",
                sim.record().state
            );
            assert_eq!(sim.record().kind_hint.as_deref(), Some("claude"));
        }
    }

    #[test]
    fn completion_guard_same_state_agent_replacement_clears_old_work() {
        // One agent works and finishes behind the user's back: `done`, and a toast waiting out
        // its five seconds. Another agent then reports the same `idle` from the pane: the old
        // agent's finish is not the new one's, so the pane is `idle`, seen, and the toast never
        // comes.
        let rows = reports(vec![
            report_as(500, "opencode", Working),
            report_as(2000, "opencode", Idle),
            report_as(4000, "kimi", Idle),
        ]);
        let mut sim =
            Sim::anchored(AgentRecord::default(), BACKGROUND, rows).with_toast_delays(5.0, 5.0);
        sim.run_until(3000);
        assert_eq!(sim.record().state, Some(Done));
        assert!(!sim.record().seen);
        assert_eq!(sim.pending.len(), 1, "the done toast waits out its delay");
        // The read the new agent's report brings, as herdr's handling of the report itself.
        sim.run_until(4500);
        assert_eq!(sim.record().kind.as_deref(), Some("kimi"));
        assert_eq!(
            sim.record().state,
            Some(Idle),
            "no completion of the new agent's"
        );
        assert!(sim.record().seen);
        assert_eq!(
            sim.events.last(),
            Some(&(4100, Some(Done), Some(Idle))),
            "the reset is announced: {:?}",
            sim.events
        );
        // herdr takes the pending notification away with the reset. Here it waits out its delay
        // on its own and is dropped when it comes due, as the pane holds another agent by then.
        assert_eq!(sim.pending.len(), 1, "waiting, until its delay is up");
        sim.run_until(12_000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        assert_eq!(sim.record().state, Some(Idle));
    }

    #[test]
    fn completion_guard_idle_session_replacement_clears_seen_and_pending_delivery() {
        // A Claude whose `SessionStart` hook reported its session finishes a turn behind the
        // user's back: `done`, and its toast waiting. Then `/clear`, and the hook reports the new
        // session: that `done` is the old session's, so the pane goes back to `idle`, announced,
        // seen, and the toast never comes.
        let mut rows = vec![session_as(0, "claude", "old-session", "startup")];
        rows.extend(every(W, 500, 1500, 50));
        rows.push(out(1600, I));
        // `/clear` redraws the prompt, which brings the read.
        rows.push(session_as(3000, "claude", "new-session", "clear"));
        rows.push(out(3050, I));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows).with_toast_delays(5.0, 5.0);
        sim.run_until(2500);
        assert_eq!(sim.record().state, Some(Done));
        assert!(!sim.record().seen);
        assert_eq!(sim.pending.len(), 1, "the done toast waits out its delay");
        sim.run_until(3500);
        assert!(
            sim.events
                .iter()
                .any(|(t, from, to)| *t >= 3000 && *from == Some(Done) && *to == Some(Idle)),
            "session replacement must publish attention reset: {:?}",
            sim.events
        );
        assert_eq!(sim.record().session_id.as_deref(), Some("new-session"));
        assert_eq!(sim.record().state, Some(Idle));
        assert!(sim.record().seen);
        // herdr takes the pending notification away with the reset. Here it waits out its delay
        // on its own and is dropped when it comes due, as the pane holds another session by then.
        assert_eq!(sim.pending.len(), 1, "waiting, until its delay is up");
        sim.run_until(12_000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn completion_guard_normal_turn_still_finishes() {
        assert_completion_guard_sequence(true, &[Idle, Working, Idle], true);
    }

    #[test]
    fn completion_guard_api_startup_blocker_respects_suppression() {
        // A Pi the watcher takes by its process, whose hook reports a question (`blocked`), then
        // that it is ready (`idle`): the question is a request, and nothing finished.
        let rows = vec![
            out(0, R),
            report_as(1000, "pi", Blocked),
            report_as(2500, "pi", Idle),
        ];
        let mut sim = Sim::fresh(AgentRecord::default(), BACKGROUND, rows);
        sim.run_until(6000);
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert!(classes.contains(&Class::Request), "{:?}", sim.toasts);
        assert!(!classes.contains(&Class::Done), "startup: {:?}", sim.toasts);
        assert_eq!(sim.record().state, Some(Idle));
    }

    #[test]
    fn completion_guard_api_session_replacement_does_not_notify_finished() {
        // A Pi that reports for itself, in whatever state its last report left, starts a new session
        // (`/new`, `/resume`, `/fork`) and reports that it is idle: the agent is taken again, that
        // idle is no completion, and the pane is seen. The first real turn of the new session still
        // finishes, once. herdr starts from a hook authority at `idle` over a detected state that
        // the replacement drops, so that the state it shows next is that detected one; the pane here
        // holds one state, which is the agent's last report, so it starts from that state. The loop
        // over the reasons is herdr's: herdr lets only some reasons replace a session of a given
        // source, where taw takes every session report whatever its reason, so here every reason
        // comes to the same.
        for state in [Unknown, Working, Blocked] {
            for reason in ["new", "resume", "fork"] {
                let start = AgentRecord {
                    kind: Some("pi".to_string()),
                    state: Some(state),
                    source: Some("hook:sim".to_string()),
                    session_path: Some("/work/completion-old.jsonl".to_string()),
                    seen: false,
                    ..AgentRecord::default()
                };
                let rows = vec![
                    out(0, R),
                    session_path_as(1000, "pi", "/work/completion-new.jsonl", reason),
                    report_as(2000, "pi", Idle),
                    report_as(4000, "pi", Working),
                    report_as(6000, "pi", Idle),
                ];
                let mut sim = Sim::anchored(start, BACKGROUND, rows);
                sim.run_until(3000);
                assert_eq!(sim.record().state, Some(Idle), "{state:?} {reason}");
                assert_eq!(
                    sim.record().session_path.as_deref(),
                    Some("/work/completion-new.jsonl")
                );
                assert!(
                    sim.toasts.is_empty(),
                    "{state:?} {reason}: {:?}",
                    sim.toasts
                );
                assert!(sim.record().seen, "{state:?} {reason}");
                sim.run_until(9000);
                assert_eq!(sim.record().state, Some(Done), "{state:?} {reason}");
                let finished = sim
                    .toasts
                    .iter()
                    .filter(|(_, class)| *class == Class::Done)
                    .count();
                assert_eq!(
                    finished, 1,
                    "the first real turn after {reason} must finish ({state:?})"
                );
            }
        }
    }

    // What taw adds around herdr's guards.

    #[test]
    fn the_pane_holds_that_its_agent_is_being_taken_until_it_is_ready() {
        // A new Claude asks its trust question: the pane records it as being taken from the read
        // that takes it, and the mark goes once the agent is ready.
        let rows = vec![out(0, B), out(5000, I)];
        let mut sim = Sim::fresh(just_started(), BACKGROUND, rows);
        sim.run_until(100);
        assert_eq!(
            sim.acquiring,
            Some(Acquired::At(None)),
            "@agent_acquiring = taken"
        );
        sim.run_until(4000);
        assert!(
            sim.acquiring.is_some(),
            "a question is not the agent being ready"
        );
        sim.run_until(6000);
        assert!(sim.acquiring.is_none(), "@agent_acquiring taken off");
        assert_eq!(sim.record().state, Some(Idle));
        // And once it works, for an agent that works at once.
        let rows = vec![out(0, W), out(5000, I)];
        let mut sim = Sim::fresh(just_started(), BACKGROUND, rows);
        sim.run_until(4000);
        assert!(sim.acquiring.is_none());
        sim.run_until(6000);
        assert_eq!(sim.record().state, Some(Done));
    }

    #[test]
    fn a_watcher_started_again_keeps_an_agent_its_predecessor_was_taking() {
        // A Claude sits at its trust question in a background session when its watcher is started
        // again (`taw watch attach-all --restart` after an update). The new watcher takes the pane
        // as it finds it, and the mark the old one left says the agent is still being taken: the
        // question answered is no finished turn.
        let asking = AgentRecord {
            kind: Some("claude".to_string()),
            state: Some(Blocked),
            rule: Some("live_blocked_form".to_string()),
            source: Some("manifest".to_string()),
            seen: false,
            ..AgentRecord::default()
        };
        let rows = vec![out(0, B), out(2000, I)];
        let mut sim =
            Sim::fresh(asking.clone(), BACKGROUND, rows.clone()).marked(Acquired::At(None));
        sim.run_until(4000);
        assert_eq!(sim.record().state, Some(Idle));
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        assert!(sim.acquiring.is_none());
        // With no mark the watcher has nothing to say the agent was being taken, and an idle
        // after a question is a finished turn, as it was before the mark.
        let mut sim = Sim::fresh(asking, BACKGROUND, rows);
        sim.run_until(4000);
        assert_eq!(sim.record().state, Some(Done));
    }

    #[test]
    fn an_agent_started_whose_hook_reports_before_the_watcher_reads_is_taken_all_the_same() {
        // `taw agent start` launches a Pi in place of one that had finished, and the new Pi's
        // hook reports its question before the watcher has read the pane since: the record the
        // verb took off is not there for the read to see, but the mark the verb set is.
        let start = AgentRecord {
            kind: Some("pi".to_string()),
            state: Some(Idle),
            source: Some("hook:sim".to_string()),
            seen: true,
            ..AgentRecord::default()
        };
        let rows = vec![
            out(0, R),
            start_as(500, "pi"),
            report_as(520, "pi", Blocked),
            report_as(2500, "pi", Idle),
        ];
        let mut sim = Sim::anchored(start, BACKGROUND, rows);
        sim.run_until(6000);
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Request], "{:?}", sim.toasts);
        assert_eq!(sim.record().state, Some(Idle));
    }

    #[test]
    fn a_first_turn_whose_work_and_question_land_inside_one_tick_ends_as_a_ready_agent_does() {
        // Pinned, as the cautious side of what a turn stamp cannot tell: a Pi the watcher takes by
        // its process reports work at 1000 and a question at 1050, inside one read and its startup
        // grace. The read finds `blocked` and a stamp both reports moved; one of them was work, but
        // the stamp says only that a report came, so the agent is still being taken, and the
        // question answered with no later report of work ends as a ready agent does: `idle`, with
        // the request and no done notification (PARITY, "Known divergences").
        let rows = vec![
            out(0, R),
            report_as(1000, "pi", Working),
            report_as(1050, "pi", Blocked),
            report_as(3000, "pi", Idle),
        ];
        let mut sim = Sim::fresh(AgentRecord::default(), BACKGROUND, rows);
        sim.run_until(7000);
        assert_eq!(
            sim.events,
            [
                (0, None, Some(Unknown)),
                (1200, Some(Unknown), Some(Blocked)),
                (3000, Some(Blocked), Some(Idle))
            ]
        );
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Request], "{:?}", sim.toasts);
        assert_eq!(sim.record().state, Some(Idle));
    }

    #[test]
    fn a_first_turn_an_agent_reports_finishes_without_a_prior_idle() {
        // `completion_guard_first_work_finishes_without_prior_idle` for an agent that reports for
        // itself: a Pi the watcher takes by its process reports work at once, and then its end.
        let rows = vec![
            out(0, R),
            report_as(1000, "pi", Working),
            report_as(2500, "pi", Idle),
        ];
        let mut sim = Sim::fresh(AgentRecord::default(), BACKGROUND, rows);
        sim.run_until(6000);
        assert_eq!(sim.record().state, Some(Done));
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Done], "{:?}", sim.toasts);
    }

    #[test]
    fn another_agent_taking_a_pane_that_waits_on_the_user_marks_it_seen() {
        // herdr's change of agent label resets the pane as a new session does: a Claude stopped at
        // a permission prompt nobody has looked at gives way to a Codex, and the pane is marked
        // seen as the new agent is taken.
        let rows = vec![out(0, B), out(500, C)];
        let mut sim = Sim::anchored(blocked_unseen(), BACKGROUND, rows);
        sim.run_until(1500);
        assert_eq!(sim.record().kind.as_deref(), Some("codex"));
        assert_eq!(sim.record().state, Some(Unknown));
        assert!(sim.record().seen, "@agent_seen = 1");
    }

    #[test]
    fn an_agent_relaunched_in_place_of_one_the_watcher_still_holds_is_taken_all_the_same() {
        // `taw agent start` takes the record of a Claude at its prompt off the pane and types
        // `claude` again: to the arbiter it is the same agent, so there is no startup grace, but
        // the verb marks the agent it launches as being taken. Its trust question, then its
        // prompt: a request, and no done.
        let rows = vec![
            out(0, I),
            start_as(500, "claude"),
            out(700, B),
            out(3000, I),
        ];
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows);
        sim.run_until(6000);
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Request], "{:?}", sim.toasts);
        assert_eq!(sim.record().state, Some(Idle));
        assert!(
            sim.written
                .contains(&(900, st(Blocked, Some("bash_permission_prompt")))),
            "read at once, with no grace: {:?}",
            sim.written
        );
    }

    #[test]
    fn an_agent_that_exits_while_it_is_being_taken_is_done_all_the_same() {
        // A new Claude asks its trust question in the background and is refused: it exits, and
        // the shell is back. An exit ends what was under way as any exit does, being taken or not
        // (herdr lets a completion through after a process exit).
        let rows = vec![out(0, B), out(5000, S)];
        let mut sim = Sim::fresh(just_started(), BACKGROUND, rows);
        sim.run_until(8000);
        assert!(
            sim.written
                .iter()
                .any(|(_, write)| *write == st(Done, Some("process_exited"))),
            "{:?}",
            sim.written
        );
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Request, Class::Done], "{:?}", sim.toasts);
    }

    #[test]
    fn a_done_toast_waiting_its_delay_is_dropped_when_the_session_changes_though_the_next_turn_is_done_too()
     {
        // A turn ends behind the user's back, and its toast waits out its five seconds. Before
        // they are up, something sends `/clear` and a prompt (an orchestrator, `taw agent prompt`),
        // and that turn ends `done` too. The first toast was earned by a session the pane does not
        // hold any more, so only the second is delivered.
        let mut rows = vec![session_as(0, "claude", "old-session", "startup")];
        rows.extend(every(W, 500, 1500, 50));
        rows.push(out(1600, I));
        rows.push(session_as(2500, "claude", "new-session", "clear"));
        rows.extend(every(W, 3000, 4000, 50));
        rows.push(out(4100, I));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(15_000);
        assert_eq!(sim.record().state, Some(Done));
        let finished: Vec<u64> = sim
            .toasts
            .iter()
            .filter(|(_, class)| *class == Class::Done)
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(finished.len(), 1, "{:?}", sim.toasts);
        assert!(finished[0] > 9000, "the new session's: {finished:?}");
    }

    #[test]
    fn a_done_pi_that_starts_a_new_session_is_idle_and_its_toast_never_comes() {
        // Pi reports its own turn and finishes it behind the user's back; then `/new`, and its
        // hook reports the new session's file. The `done` was the old session's.
        let start = AgentRecord {
            kind: Some("pi".to_string()),
            state: Some(Idle),
            source: Some("hook:sim".to_string()),
            session_path: Some("/work/old.jsonl".to_string()),
            seen: true,
            ..AgentRecord::default()
        };
        let rows = vec![
            out(0, R),
            report_as(500, "pi", Working),
            report_as(1500, "pi", Idle),
            session_path_as(3000, "pi", "/work/new.jsonl", "new"),
            // `/new` redraws the screen, which brings the read.
            out(3050, R),
        ];
        let mut sim = Sim::anchored(start, BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(2000);
        assert_eq!(sim.record().state, Some(Done));
        sim.run_until(12_000);
        assert_eq!(sim.record().state, Some(Idle));
        assert!(sim.record().seen);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
        assert_eq!(
            sim.record().source.as_deref(),
            Some("hook:sim"),
            "the pane is still its hook's"
        );
    }

    #[test]
    fn a_first_session_reported_while_done_turns_it_back_to_idle_at_the_read_that_finds_it() {
        // A Claude whose hook had reported no session finishes a turn behind the user's back:
        // `done`, and its toast waiting. Then its first `SessionStart` report comes, on `/clear`
        // (the integration installed since the agent started). herdr's session changes from none
        // to one, which resets the pane as a new session does, though nothing is taken again: the
        // read that finds it writes the idle the pane is once looked at, whatever the screen says.
        let mut rows = every(W, 500, 1500, 50);
        rows.push(out(1600, I));
        rows.push(session_as(3000, "claude", "first", "clear"));
        rows.push(out(3050, I));
        let mut sim = Sim::anchored(idle_at_prompt(), BACKGROUND, rows).with_toast_delays(5.0, 5.0);
        sim.run_until(2500);
        assert_eq!(sim.record().state, Some(Done));
        sim.run_until(3500);
        assert_eq!(sim.record().state, Some(Idle));
        assert!(sim.record().seen);
        assert_eq!(sim.events.last(), Some(&(3300, Some(Done), Some(Idle))));
        sim.run_until(12_000);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_reported_finish_of_an_old_session_is_not_toasted_beside_the_new_sessions() {
        // Pi finishes a turn behind the user's back, and its toast waits out its five seconds.
        // Before they are up it starts a new session and finishes a turn there too: the pane is
        // `done` again when the first toast comes due, but that was the old session's finish, and
        // only the new session's is delivered.
        let start = AgentRecord {
            kind: Some("pi".to_string()),
            state: Some(Idle),
            source: Some("hook:sim".to_string()),
            session_path: Some("/work/old.jsonl".to_string()),
            seen: true,
            ..AgentRecord::default()
        };
        let rows = vec![
            out(0, R),
            report_as(500, "pi", Working),
            report_as(1500, "pi", Idle),
            session_path_as(2000, "pi", "/work/new.jsonl", "new"),
            out(2050, R),
            report_as(2500, "pi", Working),
            report_as(3500, "pi", Idle),
        ];
        let mut sim = Sim::anchored(start, BACKGROUND, rows).with_toast_delays(1.0, 5.0);
        sim.run_until(12_000);
        assert_eq!(sim.record().state, Some(Done));
        let finished: Vec<u64> = sim
            .toasts
            .iter()
            .filter(|(_, class)| *class == Class::Done)
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(finished.len(), 1, "{:?}", sim.toasts);
        assert!(finished[0] > 8000, "the new session's: {finished:?}");
    }

    #[test]
    fn a_session_the_agent_reports_once_it_is_ready_takes_nothing_from_its_next_turn() {
        // Claude asks its trust question before its hooks run, so its `SessionStart` report comes
        // once it is answered: the agent's first session is no new session to take it again (herdr
        // takes an agent again only where its session replaced another), and the turn after it still
        // finishes. That turn is one whose work no read sees, only its question and the answer, so
        // nothing but the acquisition having ended makes its end a completion.
        let rows = vec![
            out(0, B),
            out(5000, I),
            session_as(5200, "claude", "first", "startup"),
            out(5300, B),
            out(6000, I),
        ];
        let mut sim = Sim::fresh(just_started(), BACKGROUND, rows);
        sim.run_until(5500);
        assert_eq!(sim.record().session_id.as_deref(), Some("first"));
        assert_eq!(sim.record().state, Some(Blocked));
        assert!(
            !sim.arbiter.acquiring() && sim.acquiring.is_none(),
            "the read that took in the first session took nothing again"
        );
        sim.run_until(8000);
        assert_eq!(sim.record().state, Some(Done));
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(
            classes,
            [Class::Request, Class::Request, Class::Done],
            "{:?}",
            sim.toasts
        );
    }

    /// A Pi that `taw agent start` launched reports work before any watcher has read its pane, and a
    /// watcher's first read of it finds that report under the mark the verb left: the watcher started
    /// again (`taw watch attach-all --restart`) between the report and the old watcher's next read,
    /// or one attached while the agent works. Then the turn ends.
    fn first_read_of_a_marked_pane_at_work(at: Acquired) -> Sim {
        let rows = vec![report_as(0, "pi", Working), report_as(2000, "pi", Idle)];
        let mut sim = Sim::fresh(AgentRecord::default(), BACKGROUND, rows).marked(at);
        sim.run_until(6000);
        sim
    }

    #[test]
    fn a_session_replaced_while_the_agent_works_keeps_it_taken_for_a_watcher_started_again() {
        // A Pi at work starts a new session before its turn ends (herdr's rebinding): it is taken
        // again from that moment, and the mark holds the turn stamp of that moment, so that the work
        // shown before it is no news of work. A watcher started again then (`taw watch attach-all
        // --restart`) reads the same `working` under the same stamp, keeps the agent being taken,
        // and the end of that turn is no completion; the next turn, reported after, is.
        let start = AgentRecord {
            kind: Some("pi".to_string()),
            state: Some(Idle),
            source: Some("hook:sim".to_string()),
            session_path: Some("/work/old.jsonl".to_string()),
            seen: true,
            ..AgentRecord::default()
        };
        let rows = vec![
            out(0, R),
            report_as(500, "pi", Working),
            session_path_as(1500, "pi", "/work/new.jsonl", "new"),
            out(1550, R),
        ];
        let mut before = Sim::anchored(start, BACKGROUND, rows);
        before.run_until(2000);
        assert_eq!(before.record().state, Some(Working));
        assert_eq!(
            before.acquiring,
            Some(Acquired::At(Some("1".to_string()))),
            "taken again, at the stamp of the rebinding"
        );
        // The watcher started again: what tmux holds, and nothing of the old watcher's memory.
        let rows = vec![
            out(0, R),
            report_as(2000, "pi", Idle),
            report_as(4000, "pi", Working),
            report_as(6000, "pi", Idle),
        ];
        let mut after = Sim::fresh(before.record().clone(), BACKGROUND, rows)
            .marked(before.acquiring.clone().unwrap())
            .with_turns(1);
        after.run_until(3000);
        assert_eq!(after.record().state, Some(Idle));
        assert!(after.toasts.is_empty(), "{:?}", after.toasts);
        after.run_until(9000);
        assert_eq!(after.record().state, Some(Done));
        let classes: Vec<Class> = after.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Done], "{:?}", after.toasts);
    }

    #[test]
    fn a_first_read_that_finds_work_reported_since_the_agent_was_taken_lets_its_turn_finish() {
        // The verb marked the agent `taken`, with no turn stamp on the pane, and the report of work
        // moved the stamp since: the work is real, so its end is a completion, as herdr's report of
        // working ends its acquisition whenever it comes.
        let sim = first_read_of_a_marked_pane_at_work(Acquired::At(None));
        assert_eq!(sim.record().state, Some(Done), "{:?}", sim.events);
        let classes: Vec<Class> = sim.toasts.iter().map(|(_, class)| *class).collect();
        assert_eq!(classes, [Class::Done], "{:?}", sim.toasts);
        assert!(sim.acquiring.is_none(), "@agent_acquiring taken off");
    }

    #[test]
    fn a_first_read_that_finds_the_work_the_agent_was_taken_over_keeps_it_being_taken() {
        // The mark was taken at the stamp the pane still holds (a session replaced while the agent
        // worked, herdr's rebinding, then a watcher started again): that `working` is no news of work,
        // and only a report that comes after it ends the acquisition, so the idle is no completion.
        let sim = first_read_of_a_marked_pane_at_work(Acquired::At(Some("1".to_string())));
        assert_eq!(sim.record().state, Some(Idle), "{:?}", sim.events);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }

    #[test]
    fn a_first_read_under_a_mark_that_does_not_say_when_keeps_the_agent_being_taken() {
        // The bare `1` an earlier taw wrote says the agent is being taken and not since when: the
        // read cannot tell work from what it was taken over, and keeps it, as before.
        let sim = first_read_of_a_marked_pane_at_work(Acquired::Unknown);
        assert_eq!(sim.record().state, Some(Idle), "{:?}", sim.events);
        assert!(sim.toasts.is_empty(), "{:?}", sim.toasts);
    }
}
