// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! `taw agent …` — herdr's agent verbs over tmux pane options. An "agent" is a pane with
//! `@agent_kind` set; every target here accepts a pane id (public or raw), a tmux target,
//! or a live `@agent_name`.

use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::common::{self, Format, Source};
use super::envelope::CliError;
use super::launch;
use super::{Ctx, Sub};
use crate::detect::{self, ManifestStore, Snapshot};
use crate::plugin::context;
use crate::session::prompt;
use crate::store::{self, AgentRecord, AgentState};
use crate::tmux::{PaneInfo, Tmux};

/// `@agent_name` must match herdr's `[a-z][a-z0-9_-]{0,31}` and be unique among live
/// agents (SKILL.md, "Understand layout, panes, and agents").
const NAME_MAX: usize = 32;

/// herdr's `--state`/`--until` status vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StatusArg {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl StatusArg {
    fn state(self) -> AgentState {
        match self {
            StatusArg::Idle => AgentState::Idle,
            StatusArg::Working => AgentState::Working,
            StatusArg::Blocked => AgentState::Blocked,
            StatusArg::Done => AgentState::Done,
            StatusArg::Unknown => AgentState::Unknown,
        }
    }
}

/// The states a flagless `agent wait` / `agent prompt --wait` settles on — herdr's
/// documented default ("matches idle, done, or blocked").
const SETTLED: &[AgentState] = &[AgentState::Idle, AgentState::Done, AgentState::Blocked];

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List agents
    List,
    /// Show an agent
    Get { target: String },
    /// Read agent terminal output
    Read {
        target: String,
        /// Terminal snapshot source (default: recent)
        #[arg(long, value_enum)]
        source: Option<Source>,
        #[arg(long = "lines", value_name = "N")]
        lines: Option<u32>,
        #[arg(long, value_enum)]
        format: Option<Format>,
        #[arg(long)]
        ansi: bool,
    },
    /// Send key presses to an agent
    SendKeys {
        target: String,
        #[arg(required = true, value_name = "KEY")]
        keys: Vec<String>,
    },
    /// Submit a prompt to an agent
    Prompt {
        target: String,
        text: String,
        /// Wait for the first matching state observed after submission
        #[arg(long)]
        wait: bool,
        /// State to match after --wait; repeat for more than one state
        #[arg(long = "until", value_name = "STATUS", value_enum)]
        until: Vec<StatusArg>,
        /// Fail after this many milliseconds
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
    },
    /// Rename an agent
    Rename {
        target: String,
        name: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Focus an agent
    Focus { target: String },
    /// Wait until an agent reaches one of the requested states
    Wait {
        target: String,
        /// State to match; repeat for more than one state. `--until` is herdr's spelling
        /// of the same flag and is accepted as an alias.
        #[arg(long = "state", alias = "until", value_name = "STATUS", value_enum)]
        state: Vec<StatusArg>,
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
    },
    /// Attach directly to an agent terminal
    Attach {
        target: String,
        /// Accepted for herdr parity; tmux has no takeover semantics (a pane has one
        /// process, and every attached client sees the same panes).
        #[arg(long)]
        takeover: bool,
    },
    /// Start a supported interactive agent in an existing pane
    Start {
        name: String,
        /// Supported agent kind and canonical executable
        #[arg(long, value_name = "KIND")]
        kind: String,
        /// Existing pane at an interactive shell prompt
        #[arg(long, value_name = "ID")]
        pane: String,
        /// Wait for interactive readiness (default: 30000; max: 300000)
        #[arg(long, value_name = "MS")]
        timeout: Option<u64>,
        /// Start it in a container, through a sandbox profile, by typing taw sandbox into the pane
        /// (the wait's default is then 300000)
        #[arg(long)]
        sandbox: bool,
        /// The sandbox profile to start it with (with --sandbox; left out, the one the config picks)
        #[arg(long, value_name = "NAME", requires = "sandbox")]
        profile: Option<String>,
        #[arg(last = true, value_name = "AGENT_ARG")]
        agent_args: Vec<String>,
    },
    /// Explain agent detection state
    Explain {
        target: Option<String>,
        /// Explain a snapshot read from a file instead of a live pane
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,
        /// Which manifest to evaluate against (default: the pane's `@agent_kind`)
        #[arg(long, value_name = "LABEL")]
        agent: Option<String>,
        /// Accepted for herdr parity; `taw` always prints JSON.
        #[arg(long)]
        json: bool,
        /// Accepted for herdr parity; `taw` always prints JSON.
        #[arg(long, value_enum)]
        format: Option<Format>,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Set or clear the agent-list projection the popup shows
    View {
        #[command(subcommand)]
        cmd: ViewCmd,
    },
}

/// herdr's `agent.view.set` / `agent.view.clear`. The projection is stored in a global
/// tmux option so the popup — a separate process — can read it; see [`super::view`].
#[derive(Subcommand, Debug)]
pub enum ViewCmd {
    /// Project the agent list
    Set {
        /// Who is asking. Required (herdr's contract), and what `view clear --source`
        /// matches against. Comes from --params instead when that is given.
        #[arg(
            long,
            value_name = "ID",
            required_unless_present = "params",
            conflicts_with = "params"
        )]
        source: Option<String>,
        /// Shown alongside the projected list
        #[arg(long, value_name = "TEXT", conflicts_with = "params")]
        label: Option<String>,
        /// herdr's `AgentViewFilter` as JSON: an `all`/`any`/`not` combinator, or an
        /// `eq`/`in`/`exists` test on a field
        #[arg(long, value_name = "JSON", conflicts_with = "params")]
        filter: Option<String>,
        /// herdr's `AgentViewSort` list as JSON (`[{"field":…,"order":"asc"|"desc"}]`),
        /// or one such object. Replaces the agents panel's own order while the view
        /// lasts.
        #[arg(long, value_name = "JSON", conflicts_with = "params")]
        sort: Option<String>,
        /// The whole herdr `agent.view.set` params object as JSON
        /// (`{"source","label","filter","sort"}`), for a caller that already has one.
        /// Mutually exclusive with the flags above.
        #[arg(long, value_name = "JSON")]
        params: Option<String>,
    },
    /// Remove the projection
    Clear {
        /// Only clear a view this source set
        #[arg(long, value_name = "ID")]
        source: Option<String>,
    },
}

/// Dispatches one `agent` subcommand, returning the envelope's sub-name alongside the
/// result so the id is known even when the command fails.
pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List => ("list", list(ctx)),
        Cmd::Get { target } => ("get", get(ctx, &target)),
        Cmd::Read {
            target,
            source,
            lines,
            format,
            ansi,
        } => (
            "read",
            read(
                ctx,
                &target,
                source,
                lines,
                ansi || format == Some(Format::Ansi),
            ),
        ),
        Cmd::SendKeys { target, keys } => ("send-keys", send_keys(ctx, &target, &keys)),
        Cmd::Prompt {
            target,
            text,
            wait,
            until,
            timeout,
        } => ("prompt", prompt(ctx, &target, &text, wait, &until, timeout)),
        Cmd::Rename {
            target,
            name,
            clear,
        } => ("rename", rename(ctx, &target, name.as_deref(), clear)),
        Cmd::Focus { target } => ("focus", focus(ctx, &target)),
        Cmd::Wait {
            target,
            state,
            timeout,
        } => ("wait", wait(ctx, &target, &state, timeout)),
        // herdr attaches a terminal stream; in tmux the equivalent of "attach to this
        // agent" is to focus its pane — every attached client then sees it.
        Cmd::Attach { target, .. } => ("attach", focus(ctx, &target)),
        Cmd::Start {
            name,
            kind,
            pane,
            timeout,
            sandbox,
            profile,
            agent_args,
        } => (
            "start",
            start(
                ctx,
                &name,
                &kind,
                &pane,
                timeout,
                &agent_args,
                sandbox.then_some(SandboxStart {
                    profile: profile.as_deref(),
                }),
            ),
        ),
        Cmd::Explain {
            target,
            file,
            agent,
            ..
        } => (
            "explain",
            explain(ctx, target.as_deref(), file.as_deref(), agent.as_deref()),
        ),
        Cmd::View { cmd } => match cmd {
            ViewCmd::Set {
                source,
                label,
                filter,
                sort,
                params,
            } => (
                super::view::SET,
                super::view::set(
                    ctx,
                    source.as_deref(),
                    label.as_deref(),
                    filter.as_deref(),
                    sort.as_deref(),
                    params.as_deref(),
                ),
            ),
            ViewCmd::Clear { source } => (
                super::view::CLEAR,
                super::view::clear(ctx, source.as_deref()),
            ),
        },
    }
}

/// Every pane with an agent in it (`@agent_kind` set), in `list-panes -a` order.
fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    let agents: Vec<Value> = common::list_panes(&ctx.tmux)?
        .iter()
        .filter(|p| p.agent.kind.is_some())
        .map(context::agent_json)
        .collect();
    Ok(json!({ "agents": agents }))
}

/// Resolves an agent target to its pane, requiring that an agent actually occupies it —
/// `pane get` shows an agent-less pane, `agent get` reports `not_found`.
fn agent_pane(t: &Tmux, target: &str) -> anyhow::Result<PaneInfo> {
    let pane = common::pane_info(t, target)?;
    if pane.agent.kind.is_none() {
        return Err(CliError::not_found(format!("no agent in pane '{target}'")));
    }
    Ok(pane)
}

fn get(ctx: &Ctx, target: &str) -> anyhow::Result<Value> {
    let pane = agent_pane(&ctx.tmux, target)?;
    Ok(json!({ "agent": context::agent_json(&pane) }))
}

fn read(
    ctx: &Ctx,
    target: &str,
    source: Option<Source>,
    lines: Option<u32>,
    ansi: bool,
) -> anyhow::Result<Value> {
    let pane = agent_pane(&ctx.tmux, target)?;
    let source = source.unwrap_or_default();
    let text = common::read_pane(&ctx.tmux, &pane.pane_id, source, lines, ansi)?;
    Ok(json!({
        "pane_id": context::agent_json(&pane)["pane_id"],
        "text": text,
    }))
}

/// A verb that types into an agent's pane, which refuses in different words for each (herdr's own).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Typing {
    Prompt,
    Keys,
}

/// herdr's `agent_not_found` (`agent_target_error_body`, `src/app/agents.rs` at v0.9.3): the target
/// resolves to no pane.
fn target_not_found(target: &str) -> anyhow::Error {
    CliError::agent_not_found(format!("agent target {target} not found"))
}

/// herdr's `agent_not_ready` for a pane that hosts no agent (`agent_not_ready`, `src/app/api/agents.rs`
/// at v0.9.3: `queue_agent_prompt` at line 157, `handle_agent_send_keys` at 353 and 359).
fn not_an_active_agent(target: &str) -> anyhow::Error {
    CliError::agent_not_ready(format!("agent {target} is not an active named agent"))
}

/// herdr's `agent_not_ready` for an agent that has gone from its pane: a prompt says it is no longer
/// the pane's foreground process (`queue_agent_prompt`, lines 165 to 170, for `runtime_hosts_agent`
/// false), and keys say it is not an active named agent (`handle_agent_send_keys`, line 359).
fn agent_has_gone_from_its_pane(target: &str, verb: Typing) -> anyhow::Error {
    match verb {
        Typing::Prompt => CliError::agent_not_ready(format!(
            "agent {target} is no longer the pane foreground process"
        )),
        Typing::Keys => not_an_active_agent(target),
    }
}

/// `err`, or [`target_not_found`] when it says the target is not there.
fn unresolved_as_herdr_says(err: anyhow::Error, target: &str) -> anyhow::Error {
    match err.downcast_ref::<CliError>() {
        Some(cli) if cli.code == "not_found" => target_not_found(target),
        _ => err,
    }
}

/// The pane of the agent `target` names, and a look at it, for a verb that types into the pane
/// (`agent prompt` and `agent send-keys`). Refused before anything is typed, as herdr does
/// (`queue_agent_prompt` and `handle_agent_send_keys`, `src/app/api/agents.rs` at v0.9.3, which
/// resolve the agent first), since what is typed at a pane with no agent in it lands at the shell's
/// prompt, and a shell may run it as a command:
///
/// * a target that resolves to no pane is `agent_not_found`;
/// * a pane that hosts no agent is `agent_not_ready`, "is not an active named agent";
/// * an agent that has gone from its pane ([`pane_hosts_agent`], for herdr's `runtime_hosts_agent`) is
///   `agent_not_ready` too, in the words of the verb;
/// * a pane that takes no input (in copy mode, or with its input off) is `shell_not_ready`, as `agent
///   start` says of one: tmux gives the keys to the mode, which drops them with nobody attached, and
///   sends none to a pane whose input is off, so nothing would reach the agent. The pane is left in its
///   mode, where a person may be reading the scrollback.
///
/// `agent_not_running` is for the waits, as in herdr (`src/api/wait.rs`).
fn typing_target(t: &Tmux, target: &str, verb: Typing) -> anyhow::Result<(PaneInfo, Look)> {
    let pane = common::pane_info(t, target).map_err(|err| unresolved_as_herdr_says(err, target))?;
    if pane.agent.kind.is_none() {
        return Err(not_an_active_agent(target));
    }
    let first = look(t, &pane.pane_id).map_err(|err| unresolved_as_herdr_says(err, target))?;
    if first.record.kind.is_none() {
        return Err(not_an_active_agent(target));
    }
    if !pane_hosts_agent(&first) {
        return Err(agent_has_gone_from_its_pane(target, verb));
    }
    if let Some(why) = &first.no_input {
        return Err(CliError::shell_not_ready(why.refusal(target)));
    }
    Ok((pane, first))
}

fn send_keys(ctx: &Ctx, target: &str, keys: &[String]) -> anyhow::Result<Value> {
    let (pane, first) = typing_target(&ctx.tmux, target, Typing::Keys)?;
    let (sent, typed) =
        common::send_keys_if_in_front(&ctx.tmux, &pane.pane_id, keys, &first.command)?;
    match typed {
        common::Sent::Typed => Ok(json!({ "keys": sent })),
        common::Sent::HeldBack { .. } => Err(agent_has_gone_from_its_pane(target, Typing::Keys)),
        common::Sent::NoInput { why, .. } => Err(CliError::shell_not_ready(why.refusal(target))),
    }
}

/// Follows herdr's src/api/wait.rs (`AGENT_PROMPT_EFFECT_TIMEOUT_MS`, the
/// same in v0.9.1 and master). How long a prompt sent to an agent that is not working has to show
/// it working or blocked before `agent prompt --wait` calls it stalled.
const PROMPT_GATE_MS: u64 = 5_000;

/// What a pane records of an agent, as far as telling what a prompt did to it goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Seen {
    state: Option<AgentState>,
    /// `@agent_state_since`, in whole seconds: two states written in one second share it.
    since: Option<u64>,
    /// `@agent_turn`: the stamp of the agent's last report of work or a wait, which moves with each
    /// one and with nothing else, so it shows a turn that began and ended between two looks.
    turn: String,
}

/// Whether the agent shows that the prompt sent when it was `before` did something to it: it is
/// working, or blocked on something it was not blocked on before (a state it has entered since, as
/// the time it entered it says), or an agent's own report of work or a wait has moved the turn
/// stamp, which a turn that has already ended again leaves as the one thing that says it was.
///
/// A stamp that has gone, with the agent that wrote it, is not a turn that began.
///
/// What was there before the prompt is not the prompt's doing: the `blocked` an agent was in when
/// the text was sent to it (taw sends it, as answering a dialog is often what the caller wants), an
/// `idle` or a `done` it was at rest in, and the `done` that `taw seen` turns back into `idle` are
/// none of them activity, and neither is anything else that is not working or blocked.
fn activity_since(before: &Seen, now: &Seen) -> bool {
    let at_work = now.state == Some(AgentState::Working);
    let blocked_anew = now.state == Some(AgentState::Blocked)
        && (before.state != Some(AgentState::Blocked) || now.since != before.since);
    // A stamp that is there and is not the one that was: the stamp going is the agent going, which
    // is the watcher taking its record off the pane, and no turn.
    let began_a_turn = !now.turn.is_empty() && now.turn != before.turn;
    at_work || blocked_anew || began_a_turn
}

/// Who an agent is, as far as its pane tells, which a wait keeps from the look that began it: another
/// agent in the pane is not the one it waits for, and herdr ends the wait there
/// (`agent_wait_identity_matches`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Identity {
    kind: Option<String>,
    /// The session its integration reported, when it has.
    session_id: Option<String>,
    /// The pid `taw agent start` recorded for what it started (`@agent_pid`), when it did.
    pid: Option<u32>,
}

impl Identity {
    /// Whether the agent `now` in the pane is another than `self`: another kind, another pid where
    /// both say one, or, where no pid shows it is the same process, another session where both say
    /// one. An agent that says what it had not said before (its session, once it has started one) is
    /// the same agent, and so is one that is in another session of the same process: Claude's
    /// `/clear` starts a new session in the process it is in, and `/resume` switches to another,
    /// which herdr, comparing the pane, the name and the kind (`agent_wait_identity_matches`), does
    /// not take for another agent either.
    fn is_replaced_by(&self, now: &Identity) -> bool {
        fn differ<T: PartialEq>(was: &Option<T>, now: &Option<T>) -> bool {
            matches!((was, now), (Some(was), Some(now)) if was != now)
        }
        if self.kind != now.kind || differ(&self.pid, &now.pid) {
            return true;
        }
        let same_process = matches!((self.pid, now.pid), (Some(was), Some(now)) if was == now);
        !same_process && differ(&self.session_id, &now.session_id)
    }
}

/// One look at an agent's pane: what it records of the agent, what [`activity_since`] reads of it, who
/// the agent is, where it runs and what the last agent left with, all read in one tmux call.
///
/// The state and the turn stamp are written by one call, as an agent's report is ([`store`]), and so
/// they have to be read by one. Read by two (the listing, and then the stamp), a look can pair the
/// state from before a report with the stamp the report moved: an `idle` that is the one the agent
/// was at rest in, with a turn that has begun, which `agent prompt --wait` took for an agent that
/// had worked and was back at rest.
struct Look {
    record: AgentRecord,
    seen: Seen,
    identity: Identity,
    /// The pane's own process, and the command in front of it, for telling whether the agent still
    /// runs ([`process_gone`]).
    pane_pid: u32,
    command: String,
    /// A container runtime names the agent, as `@agent_container` says.
    in_container: bool,
    /// The status the last agent of the pane left with ([`store::FINAL_OPTION`]).
    left: Option<store::Final>,
    /// Why the pane takes no input, when it takes none ([`prompt::NoInput`]).
    no_input: Option<prompt::NoInput>,
}

fn look(t: &Tmux, pane_id: &str) -> anyhow::Result<Look> {
    let mut columns = vec!["#{pane_id}".to_string()];
    columns.extend(store::OPTION_FORMATS.iter().map(|f| f.to_string()));
    columns.push(format!("#{{{}}}", store::TURN_OPTION));
    columns.push("#{@agent_pid}".to_string());
    columns.push(format!("#{{{}}}", store::FINAL_OPTION));
    columns.push("#{pane_pid}".to_string());
    columns.push("#{pane_current_command}".to_string());
    columns.push("#{pane_in_mode}".to_string());
    columns.push("#{pane_mode}".to_string());
    columns.push("#{pane_input_off}".to_string());
    let format = columns
        .iter()
        .map(|column| crate::tmux::fold_controls(column))
        .collect::<Vec<_>>()
        .join("\t");
    let out = t.display(pane_id, &format).map_err(|err| {
        if prompt::is_gone(&err) {
            CliError::not_found(format!("pane '{pane_id}' was closed"))
        } else {
            err
        }
    })?;
    look_of(&out).ok_or_else(|| CliError::not_found(format!("pane '{pane_id}' was closed")))
}

/// The [`Look`] tmux's answer to [`look`]'s columns says, or `None` for the line of empty fields
/// `display-message` answers with for a pane that is not there. Whether the agent runs in a container
/// is the record's own `@agent_container` column.
fn look_of(out: &str) -> Option<Look> {
    let fields: Vec<&str> = out.split('\t').collect();
    if fields.first().is_none_or(|id| id.is_empty()) {
        return None;
    }
    let record = store::parse_record(&fields[1..]);
    let after = store::OPTION_FORMATS.len() + 1;
    let column = |offset: usize| fields.get(after + offset).map_or("", |value| value.trim());
    let seen = Seen {
        state: record.state,
        since: record.state_since,
        turn: column(0).to_string(),
    };
    let identity = Identity {
        kind: record.kind.clone(),
        session_id: record.session_id.clone(),
        pid: column(1).parse().ok(),
    };
    let in_container = record.container.is_some();
    Some(Look {
        record,
        seen,
        identity,
        pane_pid: column(3).parse().unwrap_or(0),
        command: column(4).to_string(),
        in_container,
        left: store::Final::parse(column(2)),
        no_input: prompt::NoInput::of(column(5), column(6), column(7)),
    })
}

/// What the wait for a prompt to show its effect came to.
enum Activity {
    /// The agent showed it, at this look.
    Seen(Box<AgentRecord>),
    /// It did not within the time given, and was in this state at the last look.
    Absent(Option<AgentState>),
}

/// Looks at the agent every [`common::POLL_INTERVAL`] for up to `wait_ms` until it shows that the
/// prompt sent when it was `before` did something to it ([`activity_since`]). A look is always
/// made, so a wait of nothing still asks once. An agent that has left, or been replaced, has done
/// nothing with the prompt: the wait ends with `agent_not_running`.
fn await_activity(
    t: &Tmux,
    waiting: &mut Waiting,
    before: &Seen,
    wait_ms: u64,
) -> anyhow::Result<Activity> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    loop {
        let Seeing { look, left } = presence(t, waiting)?;
        if left.is_some() {
            return Err(agent_gone_message(&waiting.target));
        }
        if activity_since(before, &look.seen) {
            return Ok(Activity::Seen(Box::new(look.record)));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(Activity::Absent(look.seen.state));
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }
}

/// What `agent prompt --wait` says when the agent was not seen working or blocked within the gate.
/// The prompt may well have been delivered, as the agent may be slow, or show nothing the watcher
/// reads, so the message says to read the agent before sending it again.
fn stalled_message(target: &str, state: Option<AgentState>) -> String {
    format!(
        "agent '{target}' was not seen working or blocked within {} seconds of the prompt; its \
         state is {}. The prompt may still have been delivered: read the agent with `taw agent \
         read {target}` before sending it again.",
        PROMPT_GATE_MS / 1000,
        state.unwrap_or(AgentState::Unknown).as_str()
    )
}

/// Follows herdr's src/api/wait.rs (`prompt_agent`). Differences: taw polls
/// tmux where herdr watches an event stream, so a turn that began and ended between two looks is
/// told by the turn stamp of an agent that reports for itself, and the text is sent to an agent that
/// is blocked, where herdr refuses it with `agent_blocked` (see PARITY, "Known divergences").
///
/// herdr: send the text, then Enter. With `--wait`, the agent must then be seen working or blocked
/// within [`PROMPT_GATE_MS`], or the verb fails with `agent_prompt_stalled` (`timeout` when
/// `--timeout` is the shorter), and after that the wait is for the end of the turn: the first of
/// `until` (default [`SETTLED`]) within what is left of `--timeout`, which is one budget for all of
/// it, the sending included. An agent that is working when the prompt is sent has no gate, and the
/// end of the turn it is in satisfies the wait.
fn prompt(
    ctx: &Ctx,
    target: &str,
    text: &str,
    wait: bool,
    until: &[StatusArg],
    timeout: Option<u64>,
) -> anyhow::Result<Value> {
    let began = std::time::Instant::now();
    let left = || timeout.map(|total| remaining_budget(total, began.elapsed()));
    let (pane, first) = typing_target(&ctx.tmux, target, Typing::Prompt)?;
    let before = first.seen.clone();
    let mut waiting = Waiting::new(target, pane, &first);
    // Sent only while the command in front of the pane is still the one the check saw, and the pane
    // takes input: an agent that exits in between leaves its shell, and the text would land at its
    // prompt; a pane put in copy mode in between gives it to the mode.
    let sent =
        match common::send_text_if_in_front(&ctx.tmux, &waiting.pane_id, text, &first.command)? {
            common::Sent::Typed => {
                match common::send_enter_if_in_front(&ctx.tmux, &waiting.pane_id, &first.command)? {
                    // The text went, and its Enter did not.
                    common::Sent::NoInput { why, .. } => common::Sent::NoInput { why, typed: true },
                    sent => sent,
                }
            }
            sent => sent,
        };
    match sent {
        common::Sent::Typed => {}
        common::Sent::HeldBack { .. } => {
            return Err(agent_has_gone_from_its_pane(target, Typing::Prompt));
        }
        // What was typed before the pane stopped taking input is left on the agent's line.
        common::Sent::NoInput { why, typed } => {
            return Err(CliError::shell_not_ready(if typed {
                why.refusal_after_typing(target, "the prompt")
            } else {
                why.refusal(target)
            }));
        }
    }
    if !wait {
        return Ok(common::ok());
    }
    let wanted = wanted_states(until);

    if before.state != Some(AgentState::Working) {
        let (gate_ms, stalls) = match left() {
            Some(ms) if ms <= PROMPT_GATE_MS => (ms, false),
            _ => (PROMPT_GATE_MS, true),
        };
        match await_activity(&ctx.tmux, &mut waiting, &before, gate_ms)? {
            // What it was seen doing may be what was asked for, and then there is nothing to wait for.
            Activity::Seen(record) if record.state.is_some_and(|s| wanted.contains(&s)) => {
                // The state it was seen in is the answer, whatever it has gone on to since.
                let mut info = common::pane_info(&ctx.tmux, &waiting.pane_id)?;
                info.agent = *record;
                return Ok(json!({ "agent": context::agent_json(&info) }));
            }
            Activity::Seen(_) => {}
            Activity::Absent(state) if stalls => {
                return Err(CliError::agent_prompt_stalled(stalled_message(
                    target, state,
                )));
            }
            Activity::Absent(_) => {
                return Err(CliError::timeout(format!(
                    "timed out after {gate_ms}ms waiting for agent '{target}' to be seen working \
                     or blocked after the prompt"
                )));
            }
        }
    }

    let settled = wait_for_state(&ctx.tmux, &mut waiting, &wanted, left())?;
    Ok(json!({ "agent": context::agent_json(&settled) }))
}

fn wanted_states(requested: &[StatusArg]) -> Vec<AgentState> {
    if requested.is_empty() {
        SETTLED.to_vec()
    } else {
        requested.iter().map(|s| s.state()).collect()
    }
}

/// How long a wait keeps looking, once it finds the process recorded for its agent gone while the
/// record still stands, for what the watcher writes of the exit: the end of the turn, as an exit's
/// completion, and the status the agent left with. The watcher reads the pane when the shell prints
/// its prompt, which is at once; with no watcher nothing will write, and the wait ends when this is up.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a wait asks the process table whether the process of the agent it is for has gone: each
/// asking spawns programs, and the wait looks at the pane four times a second.
const PROCESS_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// An agent a wait is for: where it is, how the pane held it when the wait began, who it is, and
/// what the wait has found of it since.
struct Waiting {
    pane_id: String,
    /// What the caller called the agent, for what is said of it.
    target: String,
    /// The agent as the wait found it: what an answer about the status it left with is made of, as
    /// herdr answers with the agent it began with and the status it was released with.
    initial: PaneInfo,
    identity: Identity,
    /// The millisecond the wait began: a status left before it is the last agent's, not this one's.
    began: u64,
    /// When a look first found the agent's process gone while its record stood.
    process_gone_since: Option<std::time::Instant>,
    /// When the process table was last asked whether the agent's process had gone, and what it said.
    process_asked: Option<(std::time::Instant, bool)>,
}

impl Waiting {
    fn new(target: &str, initial: PaneInfo, first: &Look) -> Waiting {
        let began = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        Waiting {
            pane_id: initial.pane_id.clone(),
            target: target.to_string(),
            identity: first.identity.clone(),
            initial,
            began,
            process_gone_since: None,
            process_asked: None,
        }
    }

    /// Whether the agent's process has gone, as `ask` says, which is a few programs spawned: a wait
    /// polls four times a second, so it is asked once for each [`PROCESS_CHECK`] and the answer is
    /// kept in between.
    fn process_has_gone(&mut self, now: std::time::Instant, ask: impl FnOnce() -> bool) -> bool {
        if let Some((at, gone)) = self.process_asked
            && now.saturating_duration_since(at) < PROCESS_CHECK
        {
            return gone;
        }
        let gone = ask();
        self.process_asked = Some((now, gone));
        gone
    }

    /// The agent as the wait began, with the status it left with: what a wait that wanted that
    /// status answers.
    fn left_as(&self, state: AgentState) -> PaneInfo {
        let mut info = self.initial.clone();
        info.agent.state = Some(state);
        info
    }
}

/// Why an agent is not in its pane any more, to a wait for it.
#[derive(Debug, PartialEq, Eq)]
enum Left {
    /// Another agent has taken the pane.
    Replaced,
    /// It has gone, and left this status since the wait began, if it left one.
    Gone(Option<AgentState>),
}

/// One look at the agent a wait is for, and whether the agent is still it.
struct Seeing {
    look: Look,
    left: Option<Left>,
}

/// Looks at the pane of the agent `waiting` is for. A pane that is closed is an agent that is not
/// running, as in herdr.
fn presence(t: &Tmux, waiting: &mut Waiting) -> anyhow::Result<Seeing> {
    let look =
        look(t, &waiting.pane_id).map_err(|err| not_running_if_gone(err, &waiting.target))?;
    let left = left_of(
        waiting,
        &look,
        || process_gone(&look),
        std::time::Instant::now(),
    );
    Ok(Seeing { look, left })
}

/// Whether `look` shows that the agent `waiting` is for has left, or that another has taken its
/// place: the record is gone (and the status the watcher left with it, when it is newer than the
/// wait); another kind, session or pid is in the pane; or its process is dead while the record
/// stands, which waits for [`EXIT_GRACE`] for the watcher to write the exit, and for the status it
/// leaves, before it is an agent that has left with none.
fn left_of(
    waiting: &mut Waiting,
    look: &Look,
    process_gone: impl FnOnce() -> bool,
    now: std::time::Instant,
) -> Option<Left> {
    let status = look
        .left
        .filter(|left| left.at >= waiting.began)
        .map(|left| left.state);
    if look.record.kind.is_none() {
        return Some(Left::Gone(status));
    }
    if waiting.identity.is_replaced_by(&look.identity) {
        return Some(Left::Replaced);
    }
    if !waiting.process_has_gone(now, process_gone) {
        waiting.process_gone_since = None;
        return None;
    }
    if status.is_some() {
        return Some(Left::Gone(status));
    }
    let since = *waiting.process_gone_since.get_or_insert(now);
    (now.saturating_duration_since(since) >= EXIT_GRACE).then_some(Left::Gone(None))
}

/// Whether only the pane's kind hint names the agent it records: no process in front names an agent
/// and no container runtime does (what `taw agent start` leaves for an agent run by a script or a
/// wrapper, which no process name gives away).
fn hint_only(look: &Look) -> bool {
    look.record.kind_hint.is_some()
        && !look.in_container
        && detect::identify(&look.command, None).is_none()
}

/// Whether the agent of a pane whose record stands has gone, as a wait can tell from outside:
///
/// * For an agent only the pane's kind hint names ([`hint_only`]), the process recorded for it is dead
///   or, when none is, the pane's own shell is idle. The watcher takes the same view of such a pane
///   (`hint_only_agent_exited`).
/// * For any other pane, the pane's own shell is idle (at its prompt with nothing running in it) and
///   nothing in front identifies as the agent ([`agent_in_front`], the identification the verbs that
///   type ask for). An agent a process names, or that reports for itself, has its record taken off by
///   the watcher when it goes, and with no watcher an agent the user started by hand leaves it
///   standing: the shell back is what says it has gone, as the watcher says it for an agent that
///   reports for itself (`shell_returned`).
///
/// A wait asks this much and no more: ending a wait for an agent that is there would be the worse
/// mistake, so a pane that cannot be told (`ps` or `pgrep` has no answer) holds its agent.
fn process_gone(look: &Look) -> bool {
    gone_from_its_pane(
        look,
        crate::watch::process_alive,
        || crate::watch::pane_shell_is_idle(look.pane_pid),
        || agent_in_front(look),
    )
}

/// [`process_gone`] with the questions of the process table (`alive`: whether a pid names a live
/// process; `shell_idle`: whether the pane's own shell is idle; `in_front`: which agent the job in
/// front is) as arguments.
fn gone_from_its_pane(
    look: &Look,
    alive: impl FnOnce(u32) -> bool,
    shell_idle: impl FnOnce() -> Option<bool>,
    in_front: impl FnOnce() -> Option<String>,
) -> bool {
    if hint_only(look) {
        return match look.identity.pid {
            Some(pid) => !alive(pid),
            None => shell_idle() == Some(true),
        };
    }
    shell_idle() == Some(true) && in_front().as_deref() != look.record.kind.as_deref()
}

/// Whether the pane's live foreground job is still the agent the pane records, which a verb that types
/// asks before it types: herdr's `runtime_hosts_agent` (`src/app/agents.rs` at v0.9.3), which asks of
/// every pane that the job identifies as the agent, by its process or by its hint.
///
/// * A process in front that names an agent is that agent or another, and the pane hosts it only if
///   it is this one.
/// * An agent only its hint names has the pid `taw agent start` recorded for it, or a session restore
///   (what stands for the process the hint stands for), and the pane hosts it while that process is in
///   the foreground of the pane's terminal ([`crate::watch::process_in_front`], which asks the pane's
///   own terminal, not the pid's). That the pid is alive says nothing of what is in front: an agent
///   suspended with C-z lives, with the shell back, or a shell the user started in front of it, and a
///   pid left on the pane that the system has given to a program of another terminal leads that
///   terminal's foreground, not this one's.
/// * Any other pane, and that one when its pid does not stand, hosts the agent only if the job in
///   front positively identifies as it, as herdr's `identify_agent_in_job` does
///   ([`detect::job::identify`]): by its process's name or by its arguments, so that `node
///   /path/to/bin/codex` is codex. A shell at its prompt, a shell the user started inside the pane
///   (`sh`, `ssh`, `nix develop`, `sudo -s`), a pager, an editor or `cat` identifies as no agent, and
///   what is typed at one would land in it, or run: an agent that has exited, with no watcher to take
///   its record off, leaves exactly that, whether the user started it by hand, `taw agent start` did,
///   or a restore resumed it. A job that cannot be read names nothing, so it hosts nothing.
fn pane_hosts_agent(look: &Look) -> bool {
    hosts_the_agent(
        look,
        |pid| crate::watch::process_in_front(pid, look.pane_pid),
        || agent_in_front(look),
    )
}

/// The agent the processes in front of the pane are, by their names and arguments ([`detect::job`]).
fn agent_in_front(look: &Look) -> Option<String> {
    crate::watch::foreground_job(look.pane_pid).and_then(|job| detect::job::identify(&job))
}

/// [`pane_hosts_agent`] with the questions of the process table (`pid_in_front`: whether a pid names a
/// process in the foreground of the pane's terminal; `in_front`: which agent the job in front is) as
/// arguments.
fn hosts_the_agent(
    look: &Look,
    pid_in_front: impl FnOnce(u32) -> bool,
    in_front: impl FnOnce() -> Option<String>,
) -> bool {
    let kind = look.record.kind.as_deref();
    if !look.in_container
        && let Some(named) = detect::identify(&look.command, None)
    {
        return kind == Some(named.as_str());
    }
    if hint_only(look)
        && let Some(pid) = look.identity.pid
        && pid_in_front(pid)
    {
        return true;
    }
    in_front().is_some_and(|agent| kind == Some(agent.as_str()))
}

/// Polls the agent every 250 ms until its state is one of `wanted`, or it has left with a status
/// that is one (herdr's `wait_for_resolved_agent` answers a wait with the final status an agent is
/// released with when the wait was for it). An agent that leaves any other way, or is replaced by
/// another, ends the wait with `agent_not_running`.
fn wait_for_state(
    t: &Tmux,
    waiting: &mut Waiting,
    wanted: &[AgentState],
    timeout: Option<u64>,
) -> anyhow::Result<PaneInfo> {
    let labels: Vec<&str> = wanted.iter().map(|s| s.as_str()).collect();
    let what = format!(
        "agent '{}' to reach {}",
        waiting.target,
        labels.join(" or ")
    );
    common::poll_until(timeout, &what, || {
        let Seeing { look, left } = presence(t, waiting)?;
        if left == Some(Left::Replaced) {
            return Err(agent_gone_message(&waiting.target));
        }
        // What the record says is the answer while it is the agent's own, whoever has since gone.
        if look.record.kind.is_some()
            && let Some(state) = look.record.state
            && wanted.contains(&state)
        {
            let mut info = common::pane_info(t, &waiting.pane_id)
                .map_err(|err| not_running_if_gone(err, &waiting.target))?;
            info.agent = look.record;
            return Ok(Some(info));
        }
        match left {
            None => Ok(None),
            Some(Left::Gone(Some(state))) if wanted.contains(&state) => {
                Ok(Some(waiting.left_as(state)))
            }
            Some(_) => Err(agent_gone_message(&waiting.target)),
        }
    })
}

/// `agent_not_running`: what a wait for an agent ends with when the agent is no longer in its pane,
/// or another has taken it, as herdr's does (`agent_wait_not_running`, from
/// `wait_for_resolved_agent`), unless it left with a status the wait was for. An agent that left has
/// had its record taken off the pane, and no state it could reach is worth waiting for.
fn agent_gone_message(target: &str) -> anyhow::Error {
    CliError::agent_not_running(format!(
        "agent '{target}' is no longer running in the target pane"
    ))
}

/// `err`, or [`agent_gone_message`] when it is a pane that is not there: a wait for an agent whose
/// pane was closed is a wait for an agent that is not running.
fn not_running_if_gone(err: anyhow::Error, target: &str) -> anyhow::Error {
    match err.downcast_ref::<CliError>() {
        Some(cli) if cli.code == "not_found" => agent_gone_message(target),
        _ => err,
    }
}

fn wait(
    ctx: &Ctx,
    target: &str,
    state: &[StatusArg],
    timeout: Option<u64>,
) -> anyhow::Result<Value> {
    let pane = agent_pane(&ctx.tmux, target)?;
    let first = look(&ctx.tmux, &pane.pane_id)?;
    let mut waiting = Waiting::new(target, pane, &first);
    let wanted = wanted_states(state);
    let settled = wait_for_state(&ctx.tmux, &mut waiting, &wanted, timeout)?;
    Ok(json!({ "agent": context::agent_json(&settled) }))
}

/// Validates herdr's `@agent_name` grammar: `[a-z][a-z0-9_-]{0,31}`.
fn validate_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && name.len() <= NAME_MAX;
    if ok {
        Ok(())
    } else {
        Err(CliError::invalid_argument(format!(
            "invalid agent name '{name}': must match [a-z][a-z0-9_-]{{0,31}}"
        )))
    }
}

/// The pane of a live agent called `name`, other than `own`, when there is one. Names are unique
/// among *live* agents: another pane may hold a stale name only if its agent is gone, and
/// `store::clear_agent` clears `@agent_name` when that happens, which only a watcher does. With none,
/// the record of an agent that has exited stays, so a pane whose own shell is idle (`shell_idle`: at
/// its prompt with nothing running in it) holds no live agent, whatever it records. A dead pane (one
/// kept after its program ended, `remain-on-exit`) runs nothing at all, so it holds none either
/// ([`PaneInfo::holds_its_agent_name`], which the lookup of a pane by agent name follows too).
fn pane_holding_name<'a>(
    panes: &'a [PaneInfo],
    name: &str,
    own: &str,
    shell_idle: impl Fn(&PaneInfo) -> bool,
) -> Option<&'a PaneInfo> {
    panes.iter().find(|p| {
        p.pane_id != own
            && p.agent.name.as_deref() == Some(name)
            && p.holds_its_agent_name(&shell_idle)
    })
}

/// `invalid_argument` when another pane's live agent already has `name` (`agent rename` and
/// `agent start` both name an agent, and both refuse one that is taken).
fn ensure_name_is_free(ctx: &Ctx, name: &str, own_pane: &str) -> anyhow::Result<()> {
    let panes = common::list_panes(&ctx.tmux)?;
    match pane_holding_name(&panes, name, own_pane, crate::watch::pane_shell_known_idle) {
        Some(other) => Err(CliError::invalid_argument(format!(
            "agent name '{name}' is already used by pane {}",
            crate::tmux::ids::pane_id(&other.session_id, &other.pane_id)
        ))),
        None => Ok(()),
    }
}

fn rename(ctx: &Ctx, target: &str, name: Option<&str>, clear: bool) -> anyhow::Result<Value> {
    let pane = agent_pane(&ctx.tmux, target)?;
    if clear {
        ctx.tmux
            .unset_pane_option(&pane.pane_id, "@agent_name")
            .map_err(|e| CliError::tmux(format!("clearing @agent_name: {e:#}")))?;
        return Ok(
            json!({ "agent": context::agent_json(&common::pane_info(&ctx.tmux, &pane.pane_id)?) }),
        );
    }
    let name = name.ok_or_else(|| {
        CliError::invalid_argument("agent rename needs a NAME, or --clear to remove one")
    })?;
    validate_name(name)?;
    ensure_name_is_free(ctx, name, &pane.pane_id)?;
    store::set_text_option(&ctx.tmux, &pane.pane_id, "@agent_name", name)
        .map_err(|e| CliError::tmux(format!("setting @agent_name: {e:#}")))?;
    Ok(json!({ "agent": context::agent_json(&common::pane_info(&ctx.tmux, &pane.pane_id)?) }))
}

/// Focuses an agent's pane: switch any attached client to its session, select its window,
/// select the pane ([`common::focus_pane`], which `pane move --focus` shares).
///
/// `@agent_seen` is deliberately *not* written here. tmux fires `after-select-pane` /
/// `after-select-window` for these very commands, and the plugin script binds those to
/// `taw seen <pane>` — so seen-ness is recorded by the same path a human's keypress takes,
/// with the watcher's `done -> idle` demotion applied once, in one place.
pub(crate) fn focus(ctx: &Ctx, target: &str) -> anyhow::Result<Value> {
    let pane = common::pane_info(&ctx.tmux, target)?;
    common::focus_pane(&ctx.tmux, &pane, target)?;
    Ok(json!({ "pane_id": crate::tmux::ids::pane_id(&pane.session_id, &pane.pane_id) }))
}

/// herdr: set the kind hint, send `<kind> <args>` + Enter, then wait until the expected
/// agent has actually started in that pane.
///
/// The command is typed only once the pane's shell is at its prompt ([`await_prompt`]): a pane
/// that was made a moment ago has a shell that is still starting, and what is typed at it goes
/// to whatever reads the terminal first. It is typed, and entered, only while that shell is still in
/// front of the pane, which tmux is asked in the call that sends: a program started in the pane in
/// between is given neither.
///
/// The hint is no evidence that the agent is there, since it is the verb's own word for what it
/// was asked to start. What is ([`launch`]): the process in front of the pane is named for the
/// agent, or its own integration reported it, or the watcher has read a state for it off the
/// screen. A shell back at its prompt before any of that is the agent not starting, which fails
/// the verb at once ([`CliError::agent_not_started`]) and puts the pane back as it found it; the
/// budget running out is `timeout`.
///
/// The pane's shell is at its prompt with nothing running in it when the command is typed, so
/// whatever the pane records of an agent belongs to one that has left: its report, its state, and
/// the pid the verb recorded for it. That is taken off first ([`forget_the_last_agent`]), as the
/// watcher does when it sees the shell come back, so that what the pane records afterwards was
/// said by what the typed command started.
///
/// With `--sandbox` (`sandbox`), the command is the line a person types to start the agent in its
/// sandbox, `taw sandbox <agent> [--profile <name>] [-- <args>]` ([`crate::sandbox::typed`]). The
/// agent is there when the container runtime in front of the pane runs it, and by nothing else
/// ([`launch::Launch::sandboxed`]); it has no hint, since a hint names its agent whatever runs in the
/// pane, and the pane is marked with the runtime at once. A profile that ends without starting it
/// takes the pane's profile mark with it.
fn start(
    ctx: &Ctx,
    name: &str,
    kind: &str,
    pane: &str,
    timeout: Option<u64>,
    agent_args: &[String],
    sandbox: Option<SandboxStart<'_>>,
) -> anyhow::Result<Value> {
    validate_name(name)?;
    let timeout = start_budget(timeout, sandbox.is_some());
    if timeout > MAX_MS {
        return Err(CliError::invalid_argument(format!(
            "--timeout {timeout} exceeds the maximum of {MAX_MS}ms"
        )));
    }
    // `--timeout` is one budget for the whole verb: the wait for the pane's shell is taken out
    // of it, and the agent has what is left.
    let began = std::time::Instant::now();
    // The agent's id, which `@agent_kind` records: `--kind kiro-cli` is the agent `kiro`.
    let Some(identified_kind) = detect::identify(kind, Some(kind)) else {
        return Err(CliError::invalid_argument(format!(
            "unsupported agent kind '{kind}' (no bundled manifest identifies it)"
        )));
    };
    // A profile that is not there is no reason to wait for the shell or type anything.
    if let Some(sandboxed) = &sandbox {
        ensure_the_profile_is_there(ctx, &identified_kind, sandboxed.profile)?;
    }
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    // Before the wait for the shell, and before anything is typed: a name that is taken is no reason
    // to start an agent.
    ensure_name_is_free(ctx, name, &pane_id)?;
    let shell = await_prompt(&ctx.tmux, &pane_id, pane, timeout)?;
    let command = match &sandbox {
        Some(sandboxed) => {
            let session = common::pane_info(&ctx.tmux, &pane_id)?.session_id;
            let taw = crate::sandbox::typed::taw_for(&ctx.tmux, Some(&session), &ctx.taw_bin);
            crate::sandbox::typed::line(&taw, &identified_kind, sandboxed.profile, agent_args)
        }
        None => {
            let mut command = kind.to_string();
            for arg in agent_args {
                command.push(' ');
                command.push_str(arg);
            }
            command
        }
    };

    // What the pane had of the last agent is taken off, and its hint and pid are kept to be put
    // back if this agent does not start.
    let found = forget_the_last_agent(&ctx.tmux, &pane_id)?;

    // The hint is what `detect::identify` uses to recognise an agent whose process name
    // does not match its manifest id, and, through `detect::identify`, what restore's
    // `running_of` and `agent explain` name the pane's kind by; a restore re-launches from
    // `@agent_kind` or the reported resume, not from the hint. The agent
    // about to start is being taken (herdr's `begin_managed_agent`): its first `idle` is the
    // agent becoming ready, a trust question answered, and no finished turn, until it is seen
    // working (`store::ACQUIRING_OPTION`).
    if sandbox.is_some() {
        // A hint names its agent whatever runs in the pane; a sandboxed agent is named by the
        // container taw sees, and after the container the pane is a shell again.
        ctx.tmux
            .unset_pane_option(&pane_id, "@agent_kind_hint")
            .map_err(|e| CliError::tmux(format!("clearing @agent_kind_hint: {e:#}")))?;
    } else {
        ctx.tmux
            .set_pane_option(&pane_id, "@agent_kind_hint", kind)
            .map_err(|e| CliError::tmux(format!("setting @agent_kind_hint: {e:#}")))?;
    }
    // Follows herdr's src/terminal/state.rs, `begin_managed_agent` (#4457).
    // Differences: the acquisition is a pane option the watcher reads, taken from a pane whose
    // record and turn stamp the forget above has just taken off.
    ctx.tmux
        .set_pane_option(
            &pane_id,
            store::ACQUIRING_OPTION,
            &store::Acquired::At(None).value(),
        )
        .map_err(|e| CliError::tmux(format!("setting {}: {e:#}", store::ACQUIRING_OPTION)))?;

    // Where the command goes: what it starts can draw only below that line, and the pane still shows
    // what the last agent left above it. The watcher reads the rows below it while the line is on the
    // screen (`store::TYPED_LINE_OPTION`). It is recorded once the shell has echoed the command, and
    // Enter is sent after it: a watcher that read the pane between a record and the echo would find the
    // line nowhere on the screen, take it for gone, and read the last agent's frame after the grace.
    let prompt = launch::prompt_of(&ctx.tmux, &pane_id)?;
    // The command goes only while the shell the wait found at its prompt is still in front of the pane, and
    // the pane takes input, as a restore types its own, in the tmux call that sends it: a program started in
    // the pane since that look would be given it, and a mode a person put the pane in, which nothing typed
    // reaches ([`common::send_text_if_in_front`]).
    match common::send_text_if_in_front(&ctx.tmux, &pane_id, &command, &shell)? {
        common::Sent::Typed => {}
        common::Sent::HeldBack { typed } => {
            found.put_back(&ctx.tmux, &pane_id);
            return Err(CliError::shell_not_ready(held_back_message(pane, typed)));
        }
        common::Sent::NoInput { why, typed } => {
            found.put_back(&ctx.tmux, &pane_id);
            return Err(CliError::shell_not_ready(if typed {
                why.refusal_after_typing(pane, "the command")
            } else {
                why.refusal(pane)
            }));
        }
    }
    let echo_bound = launch::ECHO_BOUND.min(std::time::Duration::from_millis(remaining_budget(
        timeout,
        began.elapsed(),
    )));
    let line = match launch::await_echo(&ctx.tmux, &pane_id, &prompt, &command, echo_bound)? {
        launch::Echo::Seen(line) => line,
        // A shell slow to echo: where the command was typed, which the watcher finds once it is echoed.
        // No line at all would have the watcher read the whole screen after the grace, the frame the
        // last agent left included, which is what the line is there to prevent.
        launch::Echo::NotSeen => prompt.line_for(&command),
        // Put in copy mode since its prompt was seen: no Enter follows the text, and nothing of a start
        // is left on the pane but the command, which the verb cannot take off the shell's line without
        // taking the pane out of its mode on the user's behalf.
        launch::Echo::NoInput(why) => {
            found.put_back(&ctx.tmux, &pane_id);
            return Err(CliError::shell_not_ready(
                why.refusal_after_typing(pane, "the command"),
            ));
        }
    };
    // Written as a word of a tmux command, so that a value that ends in `;` (a command that does) is not
    // cut there.
    ctx.tmux
        .set_pane_option(
            &pane_id,
            store::TYPED_LINE_OPTION,
            &store::word(&line.value()),
        )
        .map_err(|e| CliError::tmux(format!("setting {}: {e:#}", store::TYPED_LINE_OPTION)))?;
    // Sent only while the shell is still in front, as the command was, and the pane takes input, in the tmux
    // call that asks: in a pane put in copy mode since the echo was seen, the mode had the Enter, the command
    // stayed on the shell's line, and the verb took the shell idle with it for an agent that did not start.
    // Refused as a mode the echo wait finds is.
    match common::send_enter_if_in_front(&ctx.tmux, &pane_id, &shell)? {
        common::Sent::Typed => {}
        common::Sent::NoInput { why, .. } => {
            found.put_back(&ctx.tmux, &pane_id);
            return Err(CliError::shell_not_ready(
                why.refusal_after_typing(pane, "the command"),
            ));
        }
        common::Sent::HeldBack { .. } => {
            found.put_back(&ctx.tmux, &pane_id);
            return Err(CliError::shell_not_ready(held_back_message(pane, true)));
        }
    }

    let what = format!("agent '{kind}' to become ready in pane '{pane}'");
    let remaining = remaining_budget(timeout, began.elapsed());
    let mut launching = match &sandbox {
        Some(_) => launch::Launch::sandboxed(&identified_kind, prompt.screen, line),
        None => launch::Launch::new(&identified_kind, prompt.screen, line),
    };
    // The pid of the agent's process, which the watcher tells an agent that only its hint names has
    // exited by ([`AgentPid`]).
    let mut agent_pid = AgentPid::default();
    // The container runtime in front of the pane at the last look.
    let mut runtime: Option<&'static str> = None;
    let waited = common::poll_until(Some(remaining), &what, || {
        let look = launch::look_at(&ctx.tmux, &pane_id)?;
        if look.shell_idle == Some(false) {
            agent_pid.look(&ctx.tmux, &pane_id, look.pane_pid, &identified_kind);
        } else {
            agent_pid.looked_at_nothing();
        }
        runtime = detect::container_runtime(&look.command);
        let now = std::time::Instant::now();
        let verdict = launching.judge(now, &look);
        log::debug!(
            "agent start in pane {pane_id}: {}",
            launching.account(now, &look, verdict)
        );
        Ok(match verdict {
            launch::Verdict::Waiting => None,
            verdict => Some(verdict),
        })
    });
    let verdict = match waited {
        Ok(verdict) => verdict,
        // The budget ran out with the agent maybe still coming up: the pid of what came up stays, for the
        // watcher to tell when it exits.
        Err(err) => {
            if err
                .downcast_ref::<CliError>()
                .is_some_and(|cli| cli.code == "timeout")
            {
                agent_pid.settle(&ctx.tmux, &pane_id);
                if sandbox.is_some() {
                    return Err(CliError::timeout(launch::no_agent_seen_message(pane)));
                }
            }
            return Err(err);
        }
    };
    if verdict == launch::Verdict::DidNotStart {
        found.put_back(&ctx.tmux, &pane_id);
        if sandbox.is_some() {
            // The profile ended without starting the agent, and the pane is a shell again: the mark
            // its `taw sandbox` set goes now, as the watcher would take it at its next read.
            let _ = store::forget_sandbox(&ctx.tmux, &pane_id);
        }
        return Err(CliError::agent_not_started(match &sandbox {
            Some(_) => launch::sandbox_not_started_message(kind, pane),
            None => launch::not_started_message(kind, pane),
        }));
    }
    if sandbox.is_some()
        && let Some(runtime) = runtime
    {
        // What the start read off the process table, at once: the envelope says where it runs.
        ctx.tmux
            .set_pane_option(&pane_id, "@agent_container", runtime)
            .map_err(|e| CliError::tmux(format!("setting @agent_container: {e:#}")))?;
    }

    // Only now is the pane genuinely an agent pane, so record its identity, and end what a restore
    // kept of the pane when its saved directory was gone: it has an agent of its own.
    store::record_kind(&ctx.tmux, &pane_id, &identified_kind)
        .map_err(|e| CliError::tmux(format!("setting @agent_kind: {e:#}")))?;
    store::set_text_option(&ctx.tmux, &pane_id, "@agent_name", name)
        .map_err(|e| CliError::tmux(format!("setting @agent_name: {e:#}")))?;

    // `taw agent start --kind codex -- resume <id>` already carries that id, the same as
    // a session-restore resume does: an autosave before Codex's first hook, or a server
    // restart before it ever fires, must still resume it rather than dropping to a plain
    // shell next time.
    //
    // A sandboxed agent's session goes only beside the profile's mark, as a restore keeps the one it
    // resumes (`store::keep_beside_the_mark`), and with the mark it goes: written plain, it could land
    // after the mark had ended, and a restore would type it outside the sandbox.
    if let Some(id) = crate::session::restore::persisted_session_from_launch_args(kind, agent_args)
    {
        if sandbox.is_some() {
            let kept = serde_json::to_string(&crate::session::snapshot::KeptResume {
                kind: identified_kind.clone(),
                session_id: Some(id.to_string()),
                ..Default::default()
            })?;
            if !store::keep_beside_the_mark(&ctx.tmux, &pane_id, &kept)
                .map_err(|e| CliError::tmux(format!("setting @agent_sandbox_resume: {e:#}")))?
            {
                log::debug!(
                    "pane {pane_id}: its sandbox ended before the session {id} could be kept"
                );
            }
        } else {
            store::set_text_option(&ctx.tmux, &pane_id, "@agent_session_id", id)
                .map_err(|e| CliError::tmux(format!("setting @agent_session_id: {e:#}")))?;
        }
    }

    // The launched process's pid (`@agent_pid`) is what lets the watcher tell the agent has
    // exited even when its last screen still matches a rule: otherwise a hint-started pane
    // (its identity comes from `@agent_kind_hint`, which is never cleared) reports a live
    // idle agent for ever after the agent leaves. An agent the job in front names has had it
    // recorded as it waited; one nothing names has it recorded now, once two looks agree on it,
    // for which it is looked at a while more when the wait did not end with two. If that never
    // comes, the watcher simply stays conservative and never auto-clears the identity.
    let spent = began.elapsed();
    agent_pid.settle_within(
        &ctx.tmux,
        &pane_id,
        &identified_kind,
        pid_bound(timeout, spent),
        std::time::Duration::from_millis(remaining_budget(timeout, spent)),
    );

    let pane_info = common::pane_info(&ctx.tmux, &pane_id)?;
    Ok(json!({ "agent": context::agent_json(&pane_info) }))
}

/// A start through a sandbox profile (`agent start --sandbox`), and the profile the caller named, if
/// one.
pub(crate) struct SandboxStart<'a> {
    pub profile: Option<&'a str>,
}

/// herdr's documented default and ceiling for `agent start --timeout`.
const DEFAULT_MS: u64 = 30_000;
const MAX_MS: u64 = 300_000;

/// `--timeout`, or its default: herdr's, and for a sandboxed start, whose first run builds an image
/// and installs tools, the most `agent start` allows.
fn start_budget(timeout: Option<u64>, sandboxed: bool) -> u64 {
    timeout.unwrap_or(if sandboxed { MAX_MS } else { DEFAULT_MS })
}

/// What a pane's options held of its last agent's hint and pid before `agent start` took them for
/// its own, empty for none: what the verb puts back for an agent that did not start.
struct Found {
    hint: String,
    pid: String,
}

impl Found {
    /// Puts back the `@agent_kind_hint` and the `@agent_pid` the pane had, for an agent that did
    /// not start: a pane that holds no agent is not marked as one, and what the watcher records
    /// from the hint goes the way it does for a hint that is gone. The pid is what tells it that
    /// a hint's agent has exited, so a pane that had a hint of an agent that was gone keeps
    /// saying so. No agent is being taken either, and the line its command was typed on is
    /// nobody's. Best-effort, since the verb has an error of its own to report.
    fn put_back(&self, t: &Tmux, pane_id: &str) {
        put_back_option(t, pane_id, "@agent_kind_hint", &self.hint);
        put_back_option(t, pane_id, "@agent_pid", &self.pid);
        put_back_option(t, pane_id, store::ACQUIRING_OPTION, "");
        put_back_option(t, pane_id, store::TYPED_LINE_OPTION, "");
    }
}

fn put_back_option(t: &Tmux, pane_id: &str, name: &str, previous: &str) {
    let put_back = if previous.is_empty() {
        t.unset_pane_option(pane_id, name)
    } else {
        t.set_pane_option(pane_id, name, previous)
    };
    if let Err(err) = put_back {
        log::debug!("putting back {name} on {pane_id}: {err:#}");
    }
}

/// What `agent start` takes off a pane with the last agent's record, beside it: the pid it
/// recorded for that agent, and the profile mark of one that ran in a sandbox, every option of it.
fn forgotten() -> Vec<&'static str> {
    std::iter::once("@agent_pid")
        .chain(store::SANDBOX_OPTIONS.iter().copied())
        .collect()
}

/// Takes what the pane records of an agent that has left off it, before the command for the next
/// is typed: what the watcher does when it sees the shell come back ([`store::clear_agent`]),
/// and what that leaves ([`forgotten`]), the pid `agent start` recorded for the agent and the
/// profile mark of one that ran in a sandbox, all in one tmux invocation
/// ([`store::forget_agent_and`]), so that a watcher that reads the pane in the middle of
/// it cannot write a kind or a state back and leave the next agent with no grace. Keeps the hint
/// and the watcher's pid. What the pane had of the hint and the pid is returned, for the verb to
/// put back should the agent not start.
///
/// The record is nothing of the agent about to start. Left, it counted for it: the report of a
/// Claude that has gone said a Claude had started when something else ran and died, and the
/// pid of the last agent made the watcher take a new one of the same kind for the one that had
/// gone, so that it was never read. The clear records no status, as the watcher's does: what the
/// pane held is the last agent's, recorded when it left, and recorded again here, at the moment the
/// next start begins, it would be taken for a status the next agent left.
fn forget_the_last_agent(t: &Tmux, pane_id: &str) -> anyhow::Result<Found> {
    let found = Found {
        hint: t
            .display(pane_id, "#{@agent_kind_hint}")
            .unwrap_or_default(),
        pid: t.display(pane_id, "#{@agent_pid}").unwrap_or_default(),
    };
    store::forget_agent_and(t, pane_id, &forgotten())
        .map_err(|e| CliError::tmux(format!("clearing what the last agent left: {e:#}")))?;
    Ok(found)
}

/// Waits until the shell of `pane_id` is at its prompt, which it is given [`prompt::BOUND`] to
/// come to, or `timeout_ms` when that is less: `--timeout` is the most the caller wants to wait
/// for anything here. `asked_for` is the pane as the caller named it. Returns what tmux named in
/// front of the pane at the look that found it there, the shell's own name
/// ([`prompt::Waiter::command`]), for a start that types only while it is still in front.
///
/// A pane already at its prompt is looked at once. One whose shell has not come to it when the
/// wait runs out has nothing typed into it, and neither is the pane marked.
fn await_prompt(
    t: &Tmux,
    pane_id: &str,
    asked_for: &str,
    timeout_ms: u64,
) -> anyhow::Result<String> {
    let timing = prompt::Timing {
        bound: prompt::BOUND.min(std::time::Duration::from_millis(timeout_ms)),
        ..prompt::Timing::default()
    };
    let mut waiter = prompt::Waiter::new(pane_id, std::time::Instant::now(), timing);
    let outcome = loop {
        if let Some(outcome) = waiter.look(t)? {
            break outcome;
        }
        std::thread::sleep(timing.poll);
    };
    match outcome {
        prompt::Outcome::Ready => Ok(waiter.command().to_string()),
        prompt::Outcome::TimedOut => Err(CliError::shell_not_ready(not_ready_message(
            asked_for,
            timing.bound,
        ))),
        prompt::Outcome::Gone => Err(CliError::not_found(closed_message(asked_for))),
        prompt::Outcome::NoInput(why) => Err(CliError::shell_not_ready(why.refusal(asked_for))),
    }
}

/// Refuses a sandboxed start of `agent` whose profile its own `taw sandbox` would refuse, before the
/// wait for the shell and before anything is typed: `profile` when the caller named one, else the one
/// the config picks, read whole as `taw sandbox` reads it, so that a config it cannot read whole is
/// refused here too, in its words ([`crate::sandbox::start::StartError::Config`]). A profile that is
/// not there is `not_found`, and one the system would not run, `invalid_argument`, as `taw sandbox`
/// says them.
fn ensure_the_profile_is_there(
    ctx: &Ctx,
    agent: &str,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    let picked = match profile {
        Some(profile) => profile.to_string(),
        None => {
            let cfg = crate::config::load_for_sandbox(&ctx.paths)
                .map_err(crate::sandbox::start::StartError::Config)?;
            crate::sandbox::profile::pick(&cfg.sandbox, agent, None).to_string()
        }
    };
    crate::sandbox::profile::find(&ctx.paths, &picked)?;
    Ok(())
}

/// What `agent start` says when another program had come in front of the pane's shell since the wait
/// found it at its prompt: before the command was typed (`typed` false), and so nothing was, or after some
/// of it (a piece of a long command) or before its Enter, with what was typed left on the shell's line.
fn held_back_message(asked_for: &str, typed: bool) -> String {
    if typed {
        format!(
            "pane {asked_for} had another program in front of its shell when the command was to be \
             run, and the command typed into it may be left on its line; clear the line and start the \
             agent again once the shell is at its prompt."
        )
    } else {
        format!(
            "pane {asked_for} had another program in front of its shell when the command was to be \
             typed, so nothing was typed. Start the agent again once the shell is at its prompt."
        )
    }
}

/// What is left of a `--timeout` of `total_ms` once `spent` of it has gone.
fn remaining_budget(total_ms: u64, spent: std::time::Duration) -> u64 {
    total_ms.saturating_sub(u64::try_from(spent.as_millis()).unwrap_or(u64::MAX))
}

/// What `agent start` says when the shell was not at its prompt when the wait ran out.
fn not_ready_message(asked_for: &str, waited: std::time::Duration) -> String {
    format!(
        "the shell in pane {asked_for} did not come to its prompt within {}, so nothing was \
         typed. Start the agent again once the shell is ready.",
        wait_said(waited)
    )
}

/// A wait as a person says it: a whole number of seconds in seconds, one of them in the
/// singular, and anything else in the milliseconds `--timeout` is given in.
fn wait_said(waited: std::time::Duration) -> String {
    let millis = waited.as_millis();
    if !millis.is_multiple_of(1000) {
        return format!("{millis} milliseconds");
    }
    match millis / 1000 {
        1 => "1 second".to_string(),
        seconds => format!("{seconds} seconds"),
    }
}

/// What `agent start` says when the pane was closed while it waited for the shell.
fn closed_message(asked_for: &str) -> String {
    format!("pane {asked_for} was closed before its shell came to its prompt, so nothing was typed")
}

/// How long `agent start` looks, once the agent is there and nothing names it, for two looks to agree on
/// the child in front of the pane ([`AgentPid`]): three looks at the least, however long one takes, and
/// past it until two agree or until the sixth ([`crate::watch::looked_enough`]), all of them begun before
/// `--timeout` runs out.
const AGENT_PID_BOUND: std::time::Duration = std::time::Duration::from_millis(1500);

/// How often it looks then.
const AGENT_PID_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long `agent start` looks for two looks to agree on the agent's process once the agent is there,
/// with `spent` of a `--timeout` of `timeout_ms` gone: [`AGENT_PID_BOUND`], or what is left of the budget
/// when that is less, since `--timeout` is one budget for the whole verb. With nothing left, no look is
/// begun, and the last two looks of the wait decide alone.
fn pid_bound(timeout_ms: u64, spent: std::time::Duration) -> std::time::Duration {
    AGENT_PID_BOUND.min(std::time::Duration::from_millis(remaining_budget(
        timeout_ms, spent,
    )))
}

/// What `agent start` records as `@agent_pid`: the pid of the agent's process, by which the watcher tells
/// that an agent only its hint names has exited, and which the verbs that type stand on for it while it is
/// in front of the pane. Best-effort: with none recorded, the watcher never takes the agent for gone.
///
/// It is the newest child of the pane's shell in front of the pane's terminal, and the first such child is
/// not always the agent: a shell that runs a helper before every command (the preexec hooks of atuin,
/// starship and bash-preexec, a DEBUG trap) or a key binding that runs a program starts that first, and it
/// is gone a moment later. Recorded, it was the agent's exit to the watcher, whose first read of the new
/// agent took the line the command was typed on for nobody's. So a child is recorded while the verb waits
/// only once the job in front of the pane names the agent, by its processes' names or arguments, as the
/// verbs that type ask it to ([`detect::job::identify`]); and for an agent nothing names, once the wait is
/// over, the child its last two looks both found in front, as a restore records one: a helper has ended by
/// then.
#[derive(Debug, Default)]
struct AgentPid {
    /// The pid recorded.
    recorded: Option<u32>,
    /// The pane's shell, as the last look found it.
    shell: Option<u32>,
    /// The child in front at the last look.
    last: Option<u32>,
    /// The child in front at the look before the last.
    previous: Option<u32>,
    /// How many looks there have been.
    looks: usize,
    /// The child the last two looks both found in front.
    agreed: Option<u32>,
}

impl AgentPid {
    /// One look found `in_front` the newest child of the pane's shell in front of its terminal, and a job
    /// there that `named` says names the agent, with that child in it: the pid to record now, when one is
    /// to be.
    fn found(&mut self, in_front: Option<u32>, named: bool) -> Option<u32> {
        self.agreed = in_front.filter(|child| self.last == Some(*child));
        self.previous = std::mem::replace(&mut self.last, in_front);
        self.looks += 1;
        in_front.filter(|child| named && self.recorded != Some(*child))
    }

    /// A look that found the pane's own shell in front, or could not tell: no child is in front, and none
    /// agrees with the next look.
    fn looked_at_nothing(&mut self) {
        self.previous = self.last.take();
        self.agreed = None;
        self.looks += 1;
    }

    /// Whether the last two looks found the same: the same child in front, or none.
    fn last_two_agree(&self) -> bool {
        self.looks >= 2 && self.previous == self.last
    }

    /// The pid to record once the wait is over, when nothing has been: the child the last two looks agreed
    /// on.
    fn settled(&self) -> Option<u32> {
        self.agreed.filter(|_| self.recorded.is_none())
    }

    /// Looks at what is in front of `pane_id`, whose shell is `shell`, and records the pid of the child
    /// there when the job in front names the agent `kind`.
    fn look(&mut self, t: &Tmux, pane_id: &str, shell: u32, kind: &str) {
        self.shell = Some(shell);
        let job = crate::watch::foreground_job(shell);
        let in_front = crate::watch::newest_children_in_front(&[shell])
            .get(&shell)
            .copied();
        if let Some(pid) = self.found(in_front, names_the_agent(job.as_ref(), in_front, kind)) {
            self.record(t, pane_id, pid);
        }
    }

    /// Records the child the last two looks agreed on, when nothing has been recorded.
    fn settle(&mut self, t: &Tmux, pane_id: &str) {
        if let Some(pid) = self.settled() {
            self.record(t, pane_id, pid);
        }
    }

    /// [`AgentPid::settle`], looking at the pane again every [`AGENT_PID_POLL`] for up to `bound` while
    /// nothing is recorded and the last two looks have not agreed: for the wait that ended with a look
    /// that saw something new, or saw nothing. The bound ends it only once it has looked three times, and
    /// past the bound it looks on while the last two looks disagree, until the sixth
    /// ([`crate::watch::looked_enough`]): on a busy machine one look can take longer than the bound. And
    /// none is begun once `budget`, what is left of `--timeout`, has run out: the verb's one budget is never
    /// spent on them, and when it ends, the last two looks decide alone, the pid recorded only if they agree.
    fn settle_within(
        &mut self,
        t: &Tmux,
        pane_id: &str,
        kind: &str,
        bound: std::time::Duration,
        budget: std::time::Duration,
    ) {
        let now = std::time::Instant::now();
        let (deadline, end) = (now + bound, now + budget);
        self.settle_with(
            |pid| pid.settle(t, pane_id),
            |pid| {
                let shell = pid
                    .shell
                    .or_else(|| t.display(pane_id, "#{pane_pid}").ok()?.trim().parse().ok());
                let Some(shell) = shell else {
                    return false;
                };
                pid.look(t, pane_id, shell, kind);
                true
            },
            || std::time::Instant::now() >= deadline,
            || std::time::Instant::now() >= end,
            || std::thread::sleep(AGENT_PID_POLL),
        );
    }

    /// [`AgentPid::settle_within`] with its settle, its look (`false` when the pane cannot be looked at),
    /// its bound (`over`), its budget (`spent`) and its pause given.
    fn settle_with(
        &mut self,
        mut settle: impl FnMut(&mut AgentPid),
        mut look: impl FnMut(&mut AgentPid) -> bool,
        mut over: impl FnMut() -> bool,
        mut spent: impl FnMut() -> bool,
        mut pause: impl FnMut(),
    ) {
        let mut looks = 0;
        loop {
            settle(self);
            if self.recorded.is_some()
                || crate::watch::looked_enough(looks, &mut over, self.last_two_agree())
                || spent()
            {
                return;
            }
            pause();
            if spent() || !look(self) {
                return;
            }
            looks += 1;
        }
    }

    fn record(&mut self, t: &Tmux, pane_id: &str, pid: u32) {
        match t.set_pane_option(pane_id, "@agent_pid", &pid.to_string()) {
            Ok(()) => self.recorded = Some(pid),
            Err(err) => log::debug!("recording @agent_pid on {pane_id}: {err:#}"),
        }
    }
}

/// Whether `job`, the job in front of a pane, names the agent `kind` ([`detect::job::identify`]), with
/// `in_front`, the child of the pane's shell found in front of its terminal, one of its processes: the
/// child is then the agent's, and not a helper that was in front when the job was read and gone by the
/// time the children were, or the other way about.
fn names_the_agent(job: Option<&detect::job::Job>, in_front: Option<u32>, kind: &str) -> bool {
    job.is_some_and(|job| {
        detect::job::identify(job).as_deref() == Some(kind)
            && in_front.is_some_and(|child| job.processes.iter().any(|p| p.pid == child))
    })
}

/// Runs the detection engine over a pane (or a file) and reports every rule's outcome —
/// herdr's `agent explain`, and the way to see *why* a pane reads as blocked or working.
fn explain(
    ctx: &Ctx,
    target: Option<&str>,
    file: Option<&std::path::Path>,
    agent: Option<&str>,
) -> anyhow::Result<Value> {
    // `command` is the pane's foreground process name for a live pane, `None` for `--file`
    // (there is no live process to inspect), and `in_container` whether the watcher marked
    // the pane as an agent that runs under a container runtime (`@agent_container`). They
    // drive the same hint-only `unknown` fallback the watcher applies (see below), so `agent
    // explain` never reports `idle` for a pane the watcher has written `unknown`.
    let (content, osc_title, kind, pane_id, command, in_container) = match file {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| {
                CliError::agent_explain_file_read_failed(format!(
                    "reading snapshot {}: {e}",
                    path.display()
                ))
            })?;
            let kind = agent
                .map(str::to_string)
                .ok_or_else(|| CliError::invalid_argument("--file needs --agent <LABEL>"))?;
            (text, String::new(), kind, Value::Null, None, false)
        }
        None => {
            let target = match target {
                Some(t) => t.to_string(),
                None => common::current_pane()?,
            };
            let pane = common::pane_info(&ctx.tmux, &target)?;
            let kind = agent
                .map(str::to_string)
                .or_else(|| pane.agent.kind.clone())
                // The hint is the name `agent start --kind` was given, which can be another name
                // of the agent's kind (`kiro-cli` is Kiro): the manifests know only kind ids.
                .or_else(|| {
                    pane.agent.kind_hint.as_deref().map(|hint| {
                        detect::identify("", Some(hint)).unwrap_or_else(|| hint.to_string())
                    })
                })
                .or_else(|| detect::identify(&pane.pane_current_command, None))
                .ok_or_else(|| {
                    CliError::not_found(format!(
                        "no agent in pane '{target}' and no --agent given: nothing to explain"
                    ))
                })?;
            // What the watcher decides on, it explains: while the line `agent start` typed the
            // agent's command on is on the screen, the rows below it. What says where the line is
            // on the screen is read in the same tmux invocation as the screen, so that it does.
            let (where_the_line_is, screen) =
                common::display_and_capture(&ctx.tmux, &pane.pane_id, &typed_line_format())
                    .map_err(|e| {
                        CliError::tmux(format!("capturing pane '{}': {e:#}", pane.pane_id))
                    })?;
            let content = below_the_typed_line(&where_the_line_is, screen);
            let in_container = pane.agent.container.is_some();
            (
                content,
                // The title a program set, as the watcher reads it: the listing already takes
                // tmux's default, the host name, for none.
                pane.pane_title.clone(),
                kind,
                context::pane_json(&pane)["pane_id"].clone(),
                Some(pane.pane_current_command.clone()),
                in_container,
            )
        }
    };

    let store = ManifestStore::new(
        ctx.paths.override_dir.clone(),
        ctx.paths.remote_manifest_dir.clone(),
    )
    .with_supplements(ctx.cfg.detection.supplements);
    let manifest = store
        .load(&kind)
        .map_err(|e| CliError::not_found(format!("no manifest for agent '{kind}': {e}")))?;
    let evaluation = detect::evaluate(
        &manifest,
        &Snapshot {
            content: &content,
            osc_title: &osc_title,
            // tmux exposes no OSC 9;4 progress payload to a one-shot CLI read; only the
            // watcher, which parses the pane's byte stream, ever sees it.
            osc_progress: "",
        },
    );

    // The watcher's hint-only fallback (`watch::state::decide` step 5): when a pane's
    // foreground process names no agent *and* no manifest rule matched the screen, the
    // honest state is `unknown` — the identity is coming from the kind hint alone and
    // nothing on screen says what the agent is doing. Reporting the engine's default `idle`
    // here made `agent explain` contradict the `@agent_state` the watcher writes. `--file`
    // has no live process, so it keeps the raw evaluation. An agent the arguments of the
    // runtime in front of the pane name is not hint-only, whatever the runtime's own name
    // says: the watcher counts it as its process naming it, and so does this.
    let hint_only_unknown = command
        .as_deref()
        .is_some_and(|c| detect::identify(c, None).is_none())
        && !in_container
        && evaluation.matched_rule.is_none();

    let mut value = serde_json::to_value(&evaluation)
        .map_err(|e| anyhow::anyhow!("serializing evaluation: {e}"))?;
    if let Some(map) = value.as_object_mut() {
        map.insert("pane_id".to_string(), pane_id);
        if hint_only_unknown {
            map.insert(
                "state".to_string(),
                Value::String(AgentState::Unknown.as_str().to_string()),
            );
        }
    }
    Ok(value)
}

/// What says where the line `taw agent start` typed the agent's command on is on a pane's screen: the
/// line (`@agent_typed_line`, folded to one line as the watcher reads it), the size of the history and
/// whether the alternate screen is on, as a `display-message` format.
fn typed_line_format() -> String {
    format!(
        "{}\t#{{history_size}}\t#{{alternate_on}}",
        crate::tmux::fold_controls(&format!("#{{{}}}", store::TYPED_LINE_OPTION))
    )
}

/// The rows of `screen` that the watcher decides on, `read` being [`typed_line_format`] expanded in the
/// same invocation as the screen: while the line `taw agent start` typed the agent's command on is on
/// the screen, those below it, since the pane still shows above it what its last agent left (see
/// `watch::typed_line`); otherwise all of them.
fn below_the_typed_line(read: &str, screen: String) -> String {
    let mut fields = read.split('\t');
    let Some(line) = fields
        .next()
        .and_then(crate::watch::typed_line::TypedLine::parse)
    else {
        return screen;
    };
    let history = fields
        .next()
        .and_then(|history| history.trim().parse().ok())
        .unwrap_or(0);
    let alternate = fields
        .next()
        .is_some_and(|alternate| alternate.trim() == "1");
    match line.below(&screen, history, alternate) {
        crate::watch::typed_line::Below::Rows(rows) => rows.to_string(),
        crate::watch::typed_line::Below::Gone => screen,
    }
}

/// Reads `@agent_state` for a pane — used by `taw status` and the wait loops.
pub fn state_of(t: &Tmux, pane_id: &str) -> anyhow::Result<Option<AgentState>> {
    Ok(store::read(t, pane_id)?.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::envelope::code_for;

    /// A pane `id` whose agent is `kind` (when it has one) and is called `name` (when it is).
    fn pane_of_agent(id: &str, kind: Option<&str>, name: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: id.to_string(),
            session_id: "$0".to_string(),
            session_name: "one".to_string(),
            session_path: String::new(),
            session_attached: 0,
            window_id: "@0".to_string(),
            window_index: 0,
            window_name: "sh".to_string(),
            window_active: true,
            window_layout: String::new(),
            pane_index: 0,
            pane_active: true,
            pane_pid: 1,
            pane_current_command: "sh".to_string(),
            pane_current_path: String::new(),
            pane_title: String::new(),
            pane_width: 80,
            pane_height: 24,
            pane_dead: false,
            alternate_on: false,
            agent: AgentRecord {
                kind: kind.map(str::to_string),
                name: name.map(str::to_string),
                ..AgentRecord::default()
            },
        }
    }

    #[test]
    fn a_name_is_held_by_another_panes_live_agent_and_by_no_one_else() {
        let panes = [
            pane_of_agent("%0", Some("claude"), Some("rev")),
            pane_of_agent("%1", Some("codex"), Some("fix")),
            // A name left on a pane whose agent has gone is nobody's.
            pane_of_agent("%2", None, Some("old")),
            pane_of_agent("%3", None, None),
        ];
        let nothing_idle = |_: &PaneInfo| false;
        assert_eq!(
            pane_holding_name(&panes, "rev", "%1", nothing_idle).map(|p| p.pane_id.as_str()),
            Some("%0")
        );
        // The pane the agent is started in does not clash with itself, as a restart there.
        assert!(pane_holding_name(&panes, "rev", "%0", nothing_idle).is_none());
        assert!(pane_holding_name(&panes, "old", "%1", nothing_idle).is_none());
        assert!(pane_holding_name(&panes, "free", "%1", nothing_idle).is_none());
    }

    /// Nobody takes the record off an agent that has exited when no watcher is attached, and its
    /// name stays: a pane whose own shell is at its prompt holds no live agent, whatever it records.
    #[test]
    fn a_pane_whose_own_shell_is_idle_holds_no_live_agent_and_so_no_name() {
        let panes = [
            pane_of_agent("%0", Some("claude"), Some("rev")),
            pane_of_agent("%1", Some("codex"), Some("fix")),
        ];
        let idle_at = |id: &'static str| move |pane: &PaneInfo| pane.pane_id == id;
        assert!(pane_holding_name(&panes, "rev", "%1", idle_at("%0")).is_none());
        // Another pane's shell being idle says nothing of this one.
        assert_eq!(
            pane_holding_name(&panes, "rev", "%1", idle_at("%1")).map(|p| p.pane_id.as_str()),
            Some("%0")
        );
    }

    /// A pane kept after its program ended runs nothing, and its record stays: it holds no name, even
    /// where nothing can tell whether its shell is idle.
    #[test]
    fn a_dead_pane_holds_no_live_agent_and_so_no_name() {
        let mut dead = pane_of_agent("%0", Some("claude"), Some("rev"));
        dead.pane_dead = true;
        let panes = [dead, pane_of_agent("%1", Some("codex"), Some("fix"))];
        let nothing_idle = |_: &PaneInfo| false;
        assert!(pane_holding_name(&panes, "rev", "%1", nothing_idle).is_none());
        // The live pane next to it still holds its own.
        assert_eq!(
            pane_holding_name(&panes, "fix", "%0", nothing_idle).map(|p| p.pane_id.as_str()),
            Some("%1")
        );
    }

    #[test]
    fn accepts_herdr_agent_names() {
        for name in ["a", "reviewer", "agent-1", "a_b-2", &"a".repeat(32)] {
            assert!(validate_name(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn rejects_names_outside_herdr_grammar() {
        for name in [
            "",
            "Reviewer",      // uppercase
            "1agent",        // must start with a letter
            "-agent",        // must start with a letter
            "has space",     // no spaces
            "has.dot",       // no dots
            &"a".repeat(33), // too long
        ] {
            let err = validate_name(name).unwrap_err();
            assert_eq!(code_for(&err), "invalid_argument", "accepted {name:?}");
        }
    }

    #[test]
    fn the_wait_for_the_shell_is_taken_out_of_the_timeout_and_the_agent_has_the_rest() {
        let ms = std::time::Duration::from_millis;
        assert_eq!(remaining_budget(30_000, ms(0)), 30_000);
        assert_eq!(remaining_budget(30_000, ms(14_500)), 15_500);
        assert_eq!(remaining_budget(30_000, ms(30_000)), 0);
        assert_eq!(
            remaining_budget(30_000, ms(40_000)),
            0,
            "never more than was given, and no underflow"
        );
        assert_eq!(remaining_budget(0, ms(1)), 0);
    }

    #[test]
    fn what_a_start_says_of_a_program_that_took_the_shells_place_names_the_pane() {
        assert_eq!(
            held_back_message("w1:p3", false),
            "pane w1:p3 had another program in front of its shell when the command was to be typed, \
             so nothing was typed. Start the agent again once the shell is at its prompt."
        );
        assert_eq!(
            held_back_message("w1:p3", true),
            "pane w1:p3 had another program in front of its shell when the command was to be run, and \
             the command typed into it may be left on its line; clear the line and start the agent \
             again once the shell is at its prompt."
        );
        for typed in [false, true] {
            let message = held_back_message("%3", typed);
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(!message.contains(banned), "{message}");
            }
        }
    }

    #[test]
    fn a_sandboxed_start_waits_up_to_the_most_agent_start_allows_and_an_ordinary_one_as_before() {
        assert_eq!(start_budget(None, true), 300_000);
        assert_eq!(start_budget(None, false), 30_000);
        assert_eq!(start_budget(Some(5_000), true), 5_000);
        assert_eq!(start_budget(Some(5_000), false), 5_000);
    }

    /// The looks for the agent's pid once it is there are taken out of the one budget `--timeout` gives the
    /// verb: what is left of it, when that is less than they are given. A start near its deadline ran
    /// past it by as much as they are given.
    #[test]
    fn the_looks_for_the_agents_pid_once_it_is_there_are_taken_out_of_the_budget() {
        let ms = std::time::Duration::from_millis;
        assert_eq!(pid_bound(30_000, ms(1_000)), AGENT_PID_BOUND);
        assert_eq!(pid_bound(2_000, ms(1_900)), ms(100));
        assert_eq!(pid_bound(2_000, ms(2_000)), ms(0));
        assert_eq!(pid_bound(2_000, ms(3_000)), ms(0));
    }

    /// A helper the shell runs before the command (a preexec hook's) is in front of the pane first, and
    /// names no agent: it is never recorded, however long it stays. The agent is, at the first look whose
    /// job names it, and again only if the child named changes.
    #[test]
    fn a_child_is_recorded_while_the_wait_runs_only_when_the_job_in_front_names_the_agent() {
        const HELPER: u32 = 700;
        const AGENT: u32 = 701;
        let mut pid = AgentPid::default();
        assert_eq!(pid.found(Some(HELPER), false), None);
        assert_eq!(pid.found(Some(HELPER), false), None);
        assert_eq!(pid.found(Some(HELPER), false), None);
        assert_eq!(pid.found(Some(AGENT), true), Some(AGENT));
        pid.recorded = Some(AGENT);
        assert_eq!(pid.found(Some(AGENT), true), None);
        assert_eq!(pid.settled(), None, "the agent named is recorded already");
        // A wrapper named for the agent that hands over to another process of its job.
        assert_eq!(pid.found(Some(AGENT + 1), true), Some(AGENT + 1));
    }

    /// The job in front names the agent with the child found in front one of its processes: the child of a
    /// job read a moment before or after it is no evidence of it.
    #[test]
    fn a_child_is_the_agents_only_as_a_process_of_a_job_that_names_the_agent() {
        use crate::detect::job::{Job, Process};
        let process = |pid: u32, name: &str| Process {
            pid,
            name: name.to_string(),
            argv: Some(vec![name.to_string()]),
            line: None,
        };
        let claude = Job {
            group: 701,
            processes: vec![process(701, "claude")],
        };
        assert!(names_the_agent(Some(&claude), Some(701), "claude"));
        assert!(!names_the_agent(Some(&claude), Some(700), "claude"));
        assert!(!names_the_agent(Some(&claude), None, "claude"));
        assert!(!names_the_agent(Some(&claude), Some(701), "codex"));
        assert!(!names_the_agent(None, Some(701), "claude"));
        let helper = Job {
            group: 700,
            processes: vec![process(700, "sleep")],
        };
        assert!(!names_the_agent(Some(&helper), Some(700), "claude"));
    }

    /// An agent nothing names has, once the wait is over, the child the last two looks both found: not a
    /// helper that was in front before it, and nothing when the child changed at the last look, or the
    /// shell was in front.
    #[test]
    fn an_agent_nothing_names_is_the_child_the_last_two_looks_agree_on() {
        const HELPER: u32 = 700;
        const AGENT: u32 = 701;
        let mut pid = AgentPid::default();
        assert_eq!(pid.found(Some(HELPER), false), None);
        assert_eq!(pid.found(Some(HELPER), false), None);
        assert_eq!(pid.settled(), Some(HELPER), "a wait that ended here");
        assert_eq!(pid.found(Some(AGENT), false), None);
        assert_eq!(pid.settled(), None, "the child changed at the last look");
        assert_eq!(pid.found(Some(AGENT), false), None);
        assert_eq!(pid.settled(), Some(AGENT));
        // The shell in front, or no answer, between two looks at the same child: no two looks agree.
        pid.looked_at_nothing();
        assert_eq!(pid.settled(), None);
        assert_eq!(pid.found(Some(AGENT), false), None);
        assert_eq!(pid.settled(), None);
        assert_eq!(pid.found(None, false), None);
        assert_eq!(pid.settled(), None);
    }

    /// Records what the last two looks agreed on, as [`AgentPid::settle`] does without a pane.
    fn settle_here(pid: &mut AgentPid) {
        if let Some(child) = pid.settled() {
            pid.recorded = Some(child);
        }
    }

    /// Past its bound, `agent start` looks on while its last two looks disagree: a helper, then nothing
    /// while the agent comes up, then the agent twice, is the agent.
    #[test]
    fn the_last_looks_go_on_past_the_bound_until_two_agree() {
        const HELPER: u32 = 700;
        const AGENT: u32 = 701;
        let mut seen = vec![Some(HELPER), None, Some(AGENT), Some(AGENT)].into_iter();
        let mut pid = AgentPid::default();
        pid.settle_with(
            settle_here,
            |pid| {
                pid.found(seen.next().expect("a fifth look"), false);
                true
            },
            || true,
            || false,
            || {},
        );
        assert_eq!(pid.recorded, Some(AGENT));
        assert_eq!(pid.looks, 4);
    }

    /// Six last looks that never agree record nothing, and no seventh is made.
    #[test]
    fn six_last_looks_that_never_agree_record_nothing() {
        let mut made = 0_u32;
        let mut pid = AgentPid::default();
        pid.settle_with(
            settle_here,
            |pid| {
                made += 1;
                assert!(made <= 6, "a seventh look");
                pid.found(Some(800 + made), false);
                true
            },
            || true,
            || false,
            || {},
        );
        assert_eq!(made, 6);
        assert_eq!(pid.recorded, None);
    }

    /// `--timeout` ends the last looks whatever the floor and the cap: no look is begun once it has run
    /// out, and the last two looks then decide alone, the pid recorded only if they agree.
    #[test]
    fn the_last_looks_end_with_the_budget_whatever_the_floor_or_the_cap() {
        // Looks that never agree, with the budget gone after the second: two are made, and nothing is
        // recorded.
        let made = std::cell::Cell::new(0_u32);
        let mut pid = AgentPid::default();
        pid.settle_with(
            settle_here,
            |pid| {
                made.set(made.get() + 1);
                pid.found(Some(800 + made.get()), false);
                true
            },
            || true,
            || made.get() >= 2,
            || {},
        );
        assert_eq!(made.get(), 2);
        assert_eq!(pid.recorded, None);
        // The helper, then the agent twice, with the budget gone after the third: the last two agree.
        let mut seen = vec![Some(700), Some(701), Some(701)].into_iter();
        let made = std::cell::Cell::new(0_u32);
        let mut pid = AgentPid::default();
        pid.settle_with(
            settle_here,
            |pid| {
                made.set(made.get() + 1);
                pid.found(seen.next().expect("a fourth look"), false);
                true
            },
            || true,
            || made.get() >= 3,
            || {},
        );
        assert_eq!(pid.recorded, Some(701));
        // A budget gone before the first: no look, and what the wait's last two looks agreed on.
        let mut pid = AgentPid::default();
        pid.found(Some(702), false);
        pid.found(Some(702), false);
        pid.settle_with(
            settle_here,
            |_| panic!("a look past the budget"),
            || true,
            || true,
            || {},
        );
        assert_eq!(pid.recorded, Some(702));
    }

    #[test]
    fn what_agent_start_says_of_a_shell_that_was_not_ready_names_the_pane_and_the_wait() {
        assert_eq!(
            not_ready_message("%3", std::time::Duration::from_secs(15)),
            "the shell in pane %3 did not come to its prompt within 15 seconds, so nothing was \
             typed. Start the agent again once the shell is ready."
        );
        // A whole number of seconds is said in seconds, one of them in the singular; anything
        // else in the milliseconds `--timeout` is given in.
        for (waited, said) in [
            (std::time::Duration::from_secs(1), "within 1 second,"),
            (std::time::Duration::from_secs(2), "within 2 seconds,"),
            (
                std::time::Duration::from_millis(1500),
                "within 1500 milliseconds,",
            ),
            (
                std::time::Duration::from_millis(250),
                "within 250 milliseconds,",
            ),
            (
                std::time::Duration::from_millis(9999),
                "within 9999 milliseconds,",
            ),
        ] {
            let message = not_ready_message("w0:p1", waited);
            assert!(message.contains(said), "{waited:?}: {message}");
        }
        assert_eq!(
            closed_message("%3"),
            "pane %3 was closed before its shell came to its prompt, so nothing was typed"
        );
    }

    /// Everything it says is in a person's words, with none of the marks kept out of anything
    /// a user reads: an em-dash, an en-dash, an arrow or a curly quote.
    #[test]
    fn what_agent_start_says_of_a_shell_that_was_not_ready_holds_no_dash_arrow_or_curly_quote() {
        for text in [
            not_ready_message("%3", std::time::Duration::from_secs(15)),
            closed_message("%3"),
        ] {
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(!text.contains(banned), "{text}");
            }
        }
    }

    #[test]
    fn a_start_takes_the_last_agents_profile_mark_off_with_its_record() {
        assert!(forgotten().contains(&"@agent_pid"));
        for option in store::SANDBOX_OPTIONS {
            assert!(forgotten().contains(option), "{option}");
        }
    }

    fn seen(state: Option<AgentState>, since: u64, turn: &str) -> Seen {
        Seen {
            state,
            since: Some(since),
            turn: turn.to_string(),
        }
    }

    #[test]
    fn the_gate_is_herdrs_five_seconds() {
        assert_eq!(PROMPT_GATE_MS, 5_000);
    }

    #[test]
    fn an_agent_at_work_after_the_prompt_is_activity() {
        let before = seen(Some(AgentState::Idle), 100, "");
        assert!(activity_since(
            &before,
            &seen(Some(AgentState::Working), 100, "")
        ));
        // Whatever it was at rest in, and at whatever second.
        for resting in [
            Some(AgentState::Idle),
            Some(AgentState::Done),
            Some(AgentState::Unknown),
            None,
        ] {
            assert!(activity_since(
                &seen(resting, 100, ""),
                &seen(Some(AgentState::Working), 130, "")
            ));
        }
    }

    #[test]
    fn nothing_that_is_not_working_or_blocked_is_activity() {
        let before = seen(Some(AgentState::Idle), 100, "");
        for now in [
            seen(Some(AgentState::Idle), 100, ""),
            // The same state written again, or at a later second: `taw seen` turns `done` back into
            // `idle`, and a watcher rewrites what it read.
            seen(Some(AgentState::Idle), 150, ""),
            seen(Some(AgentState::Done), 150, ""),
            seen(Some(AgentState::Unknown), 150, ""),
            seen(None, 0, ""),
        ] {
            assert!(!activity_since(&before, &now), "{now:?}");
        }
    }

    #[test]
    fn a_blocked_state_the_agent_was_already_in_is_not_activity_but_a_new_one_is() {
        let blocked = seen(Some(AgentState::Blocked), 100, "t1");
        // Still the same dialog: the same state, written at the same second, and no new report.
        assert!(!activity_since(&blocked, &blocked.clone()));
        // Blocked after something else, or again at a later second, or after a report.
        assert!(activity_since(
            &seen(Some(AgentState::Idle), 100, ""),
            &seen(Some(AgentState::Blocked), 100, "")
        ));
        assert!(activity_since(
            &blocked,
            &seen(Some(AgentState::Blocked), 101, "t1")
        ));
        assert!(activity_since(
            &blocked,
            &seen(Some(AgentState::Blocked), 100, "t2")
        ));
    }

    #[test]
    fn a_moved_turn_stamp_is_a_turn_even_when_the_agent_is_at_rest_again() {
        let before = seen(Some(AgentState::Idle), 100, "");
        // A report of work moved the stamp, and a report of `idle` followed before anything looked.
        assert!(activity_since(
            &before,
            &seen(Some(AgentState::Idle), 100, "1790000000000000000")
        ));
        // A stamp the agent already had says nothing new.
        let stamped = seen(Some(AgentState::Idle), 100, "1790000000000000000");
        assert!(!activity_since(&stamped, &stamped.clone()));
    }

    #[test]
    fn a_turn_stamp_that_has_gone_is_the_agent_going_and_no_turn() {
        // The watcher takes the record off the pane when the agent exits, the stamp with it.
        let before = seen(Some(AgentState::Idle), 100, "1790000000000000000");
        for now in [
            seen(None, 0, ""),
            seen(Some(AgentState::Idle), 100, ""),
            seen(Some(AgentState::Unknown), 150, ""),
        ] {
            assert!(!activity_since(&before, &now), "{now:?}");
        }
        // A stamp that is there, and is another, is a turn; a first stamp is.
        assert!(activity_since(
            &before,
            &seen(Some(AgentState::Idle), 100, "1790000000000000001")
        ));
        assert!(activity_since(
            &seen(None, 0, ""),
            &seen(Some(AgentState::Idle), 100, "1790000000000000001")
        ));
    }

    #[test]
    fn a_pane_that_is_gone_is_an_agent_that_is_not_running_and_any_other_error_is_left_alone() {
        let gone = not_running_if_gone(CliError::not_found("pane '%3' was closed"), "rev");
        assert_eq!(code_for(&gone), "agent_not_running");
        assert!(gone.to_string().contains("'rev'"), "{gone}");
        let other = not_running_if_gone(CliError::tmux("no server running"), "rev");
        assert_eq!(code_for(&other), "tmux_error");
        assert_eq!(code_for(&agent_gone_message("rev")), "agent_not_running");
    }

    /// A look at a pane whose record is `kind`'s (when it has one) with the kind hint `claude`, and the
    /// shell in front: the pane of an agent `taw agent start` started under a wrapper.
    fn a_look(kind: Option<&str>) -> Look {
        Look {
            record: AgentRecord {
                kind: kind.map(str::to_string),
                kind_hint: Some("claude".to_string()),
                ..AgentRecord::default()
            },
            seen: Seen::default(),
            identity: Identity {
                kind: kind.map(str::to_string),
                ..Identity::default()
            },
            pane_pid: 1,
            command: "sh".to_string(),
            in_container: false,
            left: None,
            no_input: None,
        }
    }

    /// What a look reads of tmux's answer: the record, whether the agent runs in a container from the
    /// record's own `@agent_container` column, and each column after the record where it is.
    #[test]
    fn a_look_reads_the_record_and_each_column_after_it() {
        let container = store::OPTION_FORMATS
            .iter()
            .position(|format| *format == "#{@agent_container}")
            .unwrap();
        let answer = |runtime: &'static str| {
            let mut record = vec![""; store::OPTION_FORMATS.len()];
            record[0] = "claude";
            record[container] = runtime;
            let mut fields = vec!["%3"];
            fields.extend(record);
            fields.extend([
                "1700000000000000000",
                "4242",
                "idle 1700000000123",
                "777",
                "docker",
                "1",
                "copy-mode",
                "0",
            ]);
            fields.join("\t")
        };

        let look = look_of(&answer("docker")).unwrap();
        assert!(look.in_container);
        assert_eq!(look.record.kind.as_deref(), Some("claude"));
        assert_eq!(look.seen.turn, "1700000000000000000");
        assert_eq!(look.identity.pid, Some(4242));
        assert_eq!(look.left, store::Final::parse("idle 1700000000123"));
        assert_eq!(look.pane_pid, 777);
        assert_eq!(look.command, "docker");
        assert_eq!(
            look.no_input,
            Some(prompt::NoInput::Mode("copy mode".to_string()))
        );
        assert!(!look_of(&answer("")).unwrap().in_container);
        assert!(look_of(&vec![""; 30].join("\t")).is_none());
    }

    /// A wait for the claude of pane `%1`, begun at millisecond `began`.
    fn waiting_for_claude(began: u64) -> Waiting {
        Waiting {
            pane_id: "%1".to_string(),
            target: "rev".to_string(),
            initial: pane_of_agent("%1", Some("claude"), Some("rev")),
            identity: Identity {
                kind: Some("claude".to_string()),
                ..Identity::default()
            },
            began,
            process_gone_since: None,
            process_asked: None,
        }
    }

    fn left_with(state: AgentState, at: u64) -> Option<store::Final> {
        Some(store::Final { state, at })
    }

    /// Another agent in the pane is not the one a wait is for (herdr's
    /// `agent_wait_identity_matches`): another kind, or another pid where both say one, or, where no
    /// pid shows it is the same process, another session. What the agent says that it had not said
    /// before is still the agent.
    #[test]
    fn another_kind_pid_or_session_is_another_agent_and_saying_more_is_not() {
        // No pid: the session is what says.
        let claude = Identity {
            kind: Some("claude".to_string()),
            session_id: Some("s1".to_string()),
            pid: None,
        };
        assert!(!claude.is_replaced_by(&claude.clone()));
        let other = |was: &Identity, change: fn(&mut Identity)| {
            let mut now = was.clone();
            change(&mut now);
            was.is_replaced_by(&now)
        };
        assert!(other(&claude, |now| now.kind = Some("codex".to_string())));
        assert!(other(&claude, |now| now.session_id = Some("s2".to_string())));
        assert!(
            !other(&claude, |now| now.pid = Some(11)),
            "a pid that was not recorded before is saying more"
        );
        // The session is told when the agent has one at each look: one that has none yet, or none
        // any more, is no other agent.
        assert!(!other(&claude, |now| now.session_id = None));
        // A recorded pid is the process, and a pid that is another is another agent.
        let recorded = Identity {
            pid: Some(10),
            ..claude.clone()
        };
        assert!(other(&recorded, |now| now.pid = Some(11)));
        assert!(!other(&recorded, |now| now.pid = None));
        // The same process in another session is the same agent: `/clear` and `/resume`.
        assert!(
            !other(&recorded, |now| now.session_id = Some("s2".to_string())),
            "the recorded pid says it is the process it was"
        );
        // And a session that differs with a pid that does not show it is the same process is another.
        assert!(other(&recorded, |now| {
            now.pid = None;
            now.session_id = Some("s2".to_string());
        }));
        assert!(other(&recorded, |now| {
            now.pid = Some(11);
            now.session_id = Some("s2".to_string());
        }));
        let fresh = Identity {
            kind: Some("claude".to_string()),
            ..Identity::default()
        };
        let reported = Identity {
            session_id: Some("s1".to_string()),
            pid: Some(10),
            ..fresh.clone()
        };
        assert!(
            !fresh.is_replaced_by(&reported),
            "its session, once it has one"
        );
    }

    #[test]
    fn a_record_that_is_gone_is_an_agent_that_left_and_the_status_it_left_is_the_waits_if_newer() {
        let mut waiting = waiting_for_claude(100);
        let mut look = a_look(None);
        let now = std::time::Instant::now();
        assert_eq!(
            left_of(&mut waiting, &look, || false, now),
            Some(Left::Gone(None))
        );
        look.left = left_with(AgentState::Done, 100);
        assert_eq!(
            left_of(&mut waiting, &look, || false, now),
            Some(Left::Gone(Some(AgentState::Done)))
        );
        // A status from before the wait is the last agent's.
        look.left = left_with(AgentState::Done, 99);
        assert_eq!(
            left_of(&mut waiting, &look, || false, now),
            Some(Left::Gone(None))
        );
    }

    /// The time a status was left at and the time a wait began are in milliseconds: a status left a
    /// moment before the wait began is in the second the wait began in, and is the last agent's.
    #[test]
    fn a_status_left_a_moment_before_the_wait_in_the_same_second_is_the_last_agents() {
        let mut waiting = waiting_for_claude(1_790_897_487_500);
        let mut look = a_look(None);
        let now = std::time::Instant::now();
        look.left = left_with(AgentState::Done, 1_790_897_487_100);
        assert_eq!(
            left_of(&mut waiting, &look, || false, now),
            Some(Left::Gone(None))
        );
        look.left = left_with(AgentState::Done, 1_790_897_487_900);
        assert_eq!(
            left_of(&mut waiting, &look, || false, now),
            Some(Left::Gone(Some(AgentState::Done)))
        );
    }

    #[test]
    fn another_agent_in_the_pane_is_not_waited_for_and_the_same_one_is() {
        let mut waiting = waiting_for_claude(100);
        let now = std::time::Instant::now();
        assert_eq!(
            left_of(&mut waiting, &a_look(Some("claude")), || false, now),
            None
        );
        assert_eq!(
            left_of(&mut waiting, &a_look(Some("codex")), || false, now),
            Some(Left::Replaced)
        );
        // Whatever the pane says of the last agent has no bearing on the one that took its place.
        let mut look = a_look(Some("codex"));
        look.left = left_with(AgentState::Idle, 150);
        assert_eq!(
            left_of(&mut waiting, &look, || true, now),
            Some(Left::Replaced)
        );
    }

    /// The process of an agent a hint names is dead while its record stands: the watcher is about to
    /// write the exit, with the status it leaves, and a wait that is owed it keeps looking for as
    /// long as [`EXIT_GRACE`]. When the watcher has written it the wait has it at once, and with
    /// none the agent has left with nothing.
    #[test]
    fn a_dead_process_under_a_standing_record_waits_for_the_watcher_for_the_grace() {
        let mut waiting = waiting_for_claude(100);
        let look = a_look(Some("claude"));
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        assert_eq!(left_of(&mut waiting, &look, || true, at(0)), None);
        let almost = EXIT_GRACE.as_millis() as u64 - 1;
        assert_eq!(left_of(&mut waiting, &look, || true, at(almost)), None);
        assert_eq!(
            left_of(&mut waiting, &look, || true, at(almost + 1)),
            Some(Left::Gone(None))
        );
        // The status the watcher wrote ends it at once.
        let mut waiting = waiting_for_claude(100);
        let mut written = a_look(Some("claude"));
        written.left = left_with(AgentState::Done, 101);
        assert_eq!(
            left_of(&mut waiting, &written, || true, at(0)),
            Some(Left::Gone(Some(AgentState::Done)))
        );
        // A process that is alive again, or still, is no agent that has left, and the grace starts anew.
        let mut waiting = waiting_for_claude(100);
        assert_eq!(left_of(&mut waiting, &look, || true, at(0)), None);
        assert_eq!(left_of(&mut waiting, &look, || false, at(1000)), None);
        assert_eq!(left_of(&mut waiting, &look, || true, at(2500)), None);
        assert_eq!(
            left_of(&mut waiting, &look, || true, at(2500 + almost + 1)),
            Some(Left::Gone(None))
        );
    }

    /// An agent only the pane's kind hint names is told gone by the pid `agent start` recorded for it,
    /// or by the pane's own shell when it recorded none, and the job in front is not read.
    #[test]
    fn an_agent_only_a_hint_names_is_gone_when_its_recorded_pid_is_dead_or_its_shell_is_idle() {
        let in_front =
            || -> Option<String> { unreachable!("a hint names the agent: nothing is read") };
        let idle = || -> Option<bool> { unreachable!("the pid said, so the shell is not asked") };
        let mut look = a_look(Some("claude"));
        look.identity.pid = Some(4242);
        assert!(gone_from_its_pane(&look, |_| false, idle, in_front));
        assert!(!gone_from_its_pane(&look, |_| true, idle, in_front));
        // With no pid recorded the pane's own shell says, and a pane that cannot be told keeps it.
        look.identity.pid = None;
        let alive = |_: u32| -> bool { unreachable!("no pid was recorded") };
        assert!(gone_from_its_pane(&look, alive, || Some(true), in_front));
        assert!(!gone_from_its_pane(&look, alive, || Some(false), in_front));
        assert!(!gone_from_its_pane(&look, alive, || None, in_front));
    }

    /// Any other pane, an agent a process names or one the user started by hand that reports for
    /// itself, is told gone by its own shell: idle, with nothing in front that identifies as the agent.
    #[test]
    fn any_other_pane_is_told_gone_by_an_idle_shell_with_nothing_in_front_that_names_the_agent() {
        let alive = |_: u32| -> bool { unreachable!("no hint, so no recorded pid is asked about") };
        let claude = || Some("claude".to_string());
        let mut look = a_look(Some("claude"));
        look.record.kind_hint = None;
        look.identity.pid = Some(4242);
        assert!(gone_from_its_pane(&look, alive, || Some(true), || None));
        assert!(
            gone_from_its_pane(&look, alive, || Some(true), || Some("codex".to_string())),
            "another agent is not this one"
        );
        assert!(
            !gone_from_its_pane(&look, alive, || Some(true), claude),
            "the pane's own process is the agent, a script named for it"
        );
        let in_front =
            || -> Option<String> { unreachable!("the shell is not idle: nothing is read") };
        assert!(!gone_from_its_pane(&look, alive, || Some(false), in_front));
        assert!(!gone_from_its_pane(&look, alive, || None, in_front));
        // A process in front that names the agent, and a container runtime that runs it.
        look.command = "claude".to_string();
        assert!(!gone_from_its_pane(&look, alive, || Some(false), in_front));
        look.command = "docker".to_string();
        look.in_container = true;
        assert!(!gone_from_its_pane(&look, alive, || Some(false), in_front));
    }

    /// A wait looks at the pane four times a second and each question of the process table spawns
    /// programs, so it is asked once for each [`PROCESS_CHECK`], and what it said is kept in between.
    #[test]
    fn a_wait_asks_the_process_table_once_for_each_check_and_keeps_the_answer_between() {
        let mut waiting = waiting_for_claude(100);
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + std::time::Duration::from_millis(ms);
        let asked = std::cell::Cell::new(0);
        let mut ask = |now: std::time::Instant, answer: bool| {
            waiting.process_has_gone(now, || {
                asked.set(asked.get() + 1);
                answer
            })
        };
        assert!(!ask(at(0), false));
        assert!(!ask(at(250), true), "the answer of 0 ms is kept");
        assert!(!ask(at(999), true));
        assert_eq!(asked.get(), 1);
        assert!(ask(at(1000), true), "a second on, it is asked again");
        assert!(ask(at(1250), false), "and kept");
        assert_eq!(asked.get(), 2);
    }

    /// What herdr says to a verb that types, word for word (`src/app/agents.rs` and
    /// `src/app/api/agents.rs` at v0.9.3), and not the code a wait ends with.
    #[test]
    fn a_verb_that_types_refuses_in_herdrs_codes_and_words() {
        let said = |err: anyhow::Error| (code_for(&err).to_string(), err.to_string());
        assert_eq!(
            said(target_not_found("rev")),
            (
                "agent_not_found".to_string(),
                "agent target rev not found".to_string()
            )
        );
        assert_eq!(
            said(not_an_active_agent("rev")),
            (
                "agent_not_ready".to_string(),
                "agent rev is not an active named agent".to_string()
            )
        );
        assert_eq!(
            said(agent_has_gone_from_its_pane("rev", Typing::Prompt)),
            (
                "agent_not_ready".to_string(),
                "agent rev is no longer the pane foreground process".to_string()
            )
        );
        assert_eq!(
            said(agent_has_gone_from_its_pane("rev", Typing::Keys)),
            (
                "agent_not_ready".to_string(),
                "agent rev is not an active named agent".to_string()
            )
        );
    }

    #[test]
    fn only_a_target_that_is_not_found_is_taken_for_agent_not_found() {
        let said = |err: anyhow::Error| code_for(&unresolved_as_herdr_says(err, "rev")).to_string();
        assert_eq!(
            said(CliError::not_found("pane 'rev' not found")),
            "agent_not_found"
        );
        assert_eq!(said(CliError::tmux("no server running")), "tmux_error");
    }

    /// herdr's `runtime_hosts_agent`, asked of every pane: the foreground job must still be the agent.
    #[test]
    fn a_process_in_front_that_names_an_agent_is_it_or_another_and_nothing_else_is_asked() {
        let pid_in_front =
            |_: u32| -> bool { unreachable!("a pid was asked after a process named an agent") };
        let in_front = || -> Option<String> {
            unreachable!("the job was read after a process named an agent")
        };
        let mut look = a_look(Some("claude"));
        look.command = "claude".to_string();
        assert!(hosts_the_agent(&look, pid_in_front, in_front));
        look.command = "codex".to_string();
        assert!(
            !hosts_the_agent(&look, pid_in_front, in_front),
            "another agent is in front"
        );
    }

    /// A container runtime names the agent by its arguments, which the job in front says and the pane's
    /// command does not.
    #[test]
    fn a_container_runtime_in_front_hosts_the_agent_it_is_running() {
        let mut look = a_look(Some("claude"));
        look.command = "docker".to_string();
        look.in_container = true;
        let claude = || Some("claude".to_string());
        assert!(hosts_the_agent(&look, |_| false, claude));
        assert!(!hosts_the_agent(&look, |_| false, || None));
        assert!(!hosts_the_agent(
            &look,
            |_| false,
            || Some("codex".to_string())
        ));
    }

    /// A pane no hint names (an agent the user started by hand, which reported itself) hosts its agent
    /// only when the job in front positively identifies as it: whatever else is in front, a shell at its
    /// prompt, a shell the user started in the pane, a pager, an editor or `cat`, is no agent, and the
    /// pid says nothing.
    #[test]
    fn a_pane_no_hint_names_hosts_the_agent_only_when_the_job_in_front_identifies_as_it() {
        let mut look = a_look(Some("claude"));
        look.record.kind_hint = None;
        look.identity.pid = Some(4242);
        let pid_in_front =
            |_: u32| -> bool { unreachable!("no hint, so no recorded pid is asked about") };
        assert!(hosts_the_agent(&look, pid_in_front, || Some(
            "claude".to_string()
        )));
        assert!(
            !hosts_the_agent(&look, pid_in_front, || Some("codex".to_string())),
            "another agent is in front"
        );
        assert!(
            !hosts_the_agent(&look, pid_in_front, || None),
            "nothing in front is an agent, or the job cannot be read"
        );
    }

    /// The pid `taw agent start` or a restore recorded for an agent only the pane's hint names stands for
    /// it while its process is in front of the terminal, and for nothing else: not once it has gone, not
    /// suspended (a process that lives, with the shell or another job in front), not as a job in the
    /// background. Then the job in front is asked, as it is for any pane, so that an agent that is
    /// there and that a process names is not refused for a pid that is stale.
    #[test]
    fn the_pid_recorded_for_an_agent_only_a_hint_names_stands_for_it_only_while_it_is_in_front() {
        let mut look = a_look(Some("claude"));
        look.identity.pid = Some(4242);
        let unread = || -> Option<String> { unreachable!("the pid stood for the agent") };
        assert!(hosts_the_agent(&look, |pid| pid == 4242, unread));
        // Not in front (gone, suspended, in the background): nothing says the agent is there.
        assert!(!hosts_the_agent(&look, |_| false, || None));
        assert!(
            !hosts_the_agent(&look, |_| false, || Some("sh".to_string())),
            "the user's own shell is in front"
        );
        // What is in front names the agent, though the pid does not stand: it is there.
        assert!(hosts_the_agent(
            &look,
            |_| false,
            || Some("claude".to_string())
        ));
        assert!(!hosts_the_agent(
            &look,
            |_| false,
            || Some("codex".to_string())
        ));
    }

    /// With no pid recorded (a restore that found no process to record, or an older start), the hint
    /// stands for nothing: the job in front is asked, as for any pane. The pane's own shell not
    /// being idle would not do, which a nested shell, a pager or `cat` satisfies.
    #[test]
    fn with_no_pid_recorded_a_hint_does_not_stand_for_the_agent() {
        let look = a_look(Some("claude"));
        assert_eq!(look.identity.pid, None);
        let pid_in_front = |_: u32| -> bool { unreachable!("no pid was recorded") };
        assert!(!hosts_the_agent(&look, pid_in_front, || None));
        assert!(!hosts_the_agent(&look, pid_in_front, || Some(
            "codex".to_string()
        )));
        assert!(hosts_the_agent(&look, pid_in_front, || Some(
            "claude".to_string()
        )));
    }

    #[test]
    fn what_agent_prompt_says_of_a_stalled_prompt_names_the_agent_and_says_it_may_have_arrived() {
        let message = stalled_message("reviewer", Some(AgentState::Idle));
        assert_eq!(
            message,
            "agent 'reviewer' was not seen working or blocked within 5 seconds of the prompt; its \
             state is idle. The prompt may still have been delivered: read the agent with `taw \
             agent read reviewer` before sending it again."
        );
        // An agent with no state yet reads as `unknown`.
        assert!(stalled_message("a", None).contains("its state is unknown."));
        for banned in [
            '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        ] {
            assert!(!message.contains(banned), "{message}");
        }
    }

    #[test]
    fn a_flagless_wait_settles_on_idle_done_or_blocked() {
        assert_eq!(wanted_states(&[]), SETTLED.to_vec());
        assert_eq!(
            wanted_states(&[StatusArg::Working]),
            vec![AgentState::Working]
        );
        assert_eq!(
            wanted_states(&[StatusArg::Idle, StatusArg::Unknown]),
            vec![AgentState::Idle, AgentState::Unknown]
        );
    }

    #[test]
    fn status_args_map_onto_the_store_states() {
        for (arg, state) in [
            (StatusArg::Idle, AgentState::Idle),
            (StatusArg::Working, AgentState::Working),
            (StatusArg::Blocked, AgentState::Blocked),
            (StatusArg::Done, AgentState::Done),
            (StatusArg::Unknown, AgentState::Unknown),
        ] {
            assert_eq!(arg.state(), state);
        }
    }
}
