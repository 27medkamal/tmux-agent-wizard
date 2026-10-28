// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Typed read/write of the `@agent_*` tmux pane options. tmux is the database: this
//! module never computes agent state, it only translates it to and from pane options.

use crate::tmux::Tmux;
use anyhow::Context;
use std::collections::BTreeMap;
use std::str::FromStr;

/// The lifecycle state of a detected agent occupying a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentState {
    /// The lowercase label used in tmux options, JSON and manifests.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Unknown => "unknown",
        }
    }
}

/// The `@agent_rule` the watcher writes for an agent suspended with Ctrl+Z: its process is alive
/// and stopped (or running in the background) behind the shell, so the pane keeps the agent, its
/// name and its session, at `idle` (herdr v0.9.4's suspend).
pub const SUSPENDED_RULE: &str = "suspended";

/// The word the popup's row draws in its state column for a suspended agent
/// ([`SUSPENDED_RULE`]), where it draws `idle` for one at its prompt.
pub const SUSPENDED_WORD: &str = "suspended";

/// The process of the pane's agent, as the watcher found it in front of the pane: its pid, a space,
/// and the command tmux named it by. A watcher started while the agent is suspended reads it back,
/// and keeps it only if that pid is alive, stopped and named as the agent.
pub const PROCESS_OPTION: &str = "@agent_process";

impl FromStr for AgentState {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(AgentState::Idle),
            "working" => Ok(AgentState::Working),
            "blocked" => Ok(AgentState::Blocked),
            "done" => Ok(AgentState::Done),
            "unknown" => Ok(AgentState::Unknown),
            other => anyhow::bail!("invalid agent state: '{other}'"),
        }
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything `taw` knows about the agent occupying a pane, assembled from `@agent_*`
/// pane options. Every field is `None`/`false`/empty when no agent occupies the pane.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct AgentRecord {
    pub kind: Option<String>,
    pub state: Option<AgentState>,
    pub state_since: Option<u64>,
    pub rule: Option<String>,
    pub source: Option<String>,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
    pub name: Option<String>,
    pub display: Option<String>,
    pub title: Option<String>,
    pub seen: bool,
    pub kind_hint: Option<String>,
    pub watch_pid: Option<u32>,
    /// From `@agent_reported_kind`: the kind the agent's own integration hook reported
    /// (`pane report-agent-session --agent <kind>`). It counts only while it agrees with
    /// [`kind`](Self::kind): the watcher may record another occupant over it.
    pub reported_kind: Option<String>,
    /// From `@agent_container`: the container runtime the watcher saw run the agent, from the
    /// process table alone (`watch::container`). What draws 📦 on the agent's row.
    pub container: Option<String>,
    /// From `@agent_sandbox`: the profile `taw sandbox` started the agent through, when it did. It
    /// names the profile in the preview's title and in a restore; it never draws 📦 by itself.
    pub sandbox: Option<String>,
    /// From `@agent_token_<k>`. Read lazily by `read()` (and by `list_panes` only in the
    /// sense that it always leaves this empty) since tokens are not in `OPTION_FORMATS`.
    pub tokens: BTreeMap<String, String>,
    /// The command the agent reported that resumes its session ([`crate::agent_resume`]), from
    /// `@agent_resume_source`, `@agent_resume_agent` and `@agent_resume_argv`.
    pub resume: Option<crate::agent_resume::ReportedResume>,
    /// From [`RESTORE_ERROR_OPTION`]: why a session restore left the pane as tmux made it (herdr's
    /// `restore_error`). It is the pane's and not its agent's, and goes only with the pane.
    pub restore_error: Option<String>,
    /// From [`RESTORE_SAVED_OPTION`]: the pane as the snapshot a restore read had saved it, as the
    /// restore wrote it, for a save to keep in place of what the pane holds now. As
    /// [`restore_error`](Self::restore_error), it goes only with the pane.
    pub restore_saved: Option<String>,
    /// The flags the pane's Claude was started with that set how its session runs, from
    /// [`crate::claude_flags::OPTION`] (a JSON list of words): what a save keeps and a restore types
    /// after `claude`. Empty for none, and for an option that holds anything else.
    pub claude_flags: Vec<String>,
}

/// `#{...}` format strings for the fixed `@agent_*` pane options, in the exact column
/// order `Tmux::list_panes` appends after its own pane/window/session fields — and the
/// order `parse_record` expects its `fields` slice in.
pub const OPTION_FORMATS: &[&str] = &[
    "#{@agent_kind}",
    "#{@agent_state}",
    "#{@agent_state_since}",
    "#{@agent_rule}",
    "#{@agent_source}",
    "#{@agent_session_id}",
    "#{@agent_session_path}",
    "#{@agent_name}",
    "#{@agent_display}",
    "#{@agent_title}",
    "#{@agent_seen}",
    "#{@agent_kind_hint}",
    "#{@agent_watch_pid}",
    "#{@agent_reported_kind}",
    "#{@agent_container}",
    "#{@agent_sandbox}",
    "#{@agent_resume_source}",
    "#{@agent_resume_agent}",
    "#{@agent_resume_argv}",
    "#{@agent_restore_error}",
    "#{@agent_restore_saved}",
    "#{@agent_claude_flags}",
];

/// Builds an `AgentRecord` from `OPTION_FORMATS.len()` fields in that same order. tmux
/// expands an unset option's `#{...}` format to `""`, which we treat as "not set" — this
/// matches the per-option tables (empty means no agent / no value). `tokens` is
/// always empty here; `@agent_token_*` values are read separately, see `read()`.
pub fn parse_record(fields: &[&str]) -> AgentRecord {
    let get = |i: usize| fields.get(i).copied().filter(|s| !s.is_empty());
    AgentRecord {
        kind: get(0).map(str::to_string),
        state: get(1).and_then(|s| s.parse().ok()),
        state_since: get(2).and_then(|s| s.parse().ok()),
        rule: get(3).map(str::to_string),
        source: get(4).map(str::to_string),
        session_id: get(5).map(str::to_string),
        session_path: get(6).map(str::to_string),
        name: get(7).map(str::to_string),
        display: get(8).map(str::to_string),
        title: get(9).map(str::to_string),
        seen: get(10) == Some("1"),
        kind_hint: get(11).map(str::to_string),
        watch_pid: get(12).and_then(|s| s.parse().ok()),
        reported_kind: get(13).map(str::to_string),
        container: get(14).map(str::to_string),
        sandbox: get(15).map(str::to_string),
        tokens: BTreeMap::new(),
        resume: crate::agent_resume::ReportedResume::from_options(
            get(16).unwrap_or_default(),
            get(17).unwrap_or_default(),
            get(18).unwrap_or_default(),
        ),
        restore_error: get(19).map(str::to_string),
        restore_saved: get(20).map(str::to_string),
        claude_flags: get(21).map(crate::claude_flags::decode).unwrap_or_default(),
    }
}

const TOKEN_PREFIX: &str = "@agent_token_";

/// Reads the full agent record for `pane`: the fixed `OPTION_FORMATS` options in one
/// `display-message` call, plus `@agent_token_*` custom tokens via `show-options -p`.
///
/// Each column is wrapped by `tmux::fold_controls`: these options hold free text a user
/// or an agent can put a literal tab or newline into (`@agent_display`, `@agent_title`, …)
/// while the reply is split on tabs. See `tmux::fold_controls` for the full rationale.
pub fn read(t: &Tmux, pane: &str) -> anyhow::Result<AgentRecord> {
    let format = OPTION_FORMATS
        .iter()
        .map(|f| crate::tmux::fold_controls(f))
        .collect::<Vec<_>>()
        .join("\t");
    let out = t
        .display(pane, &format)
        .with_context(|| format!("reading agent options for pane '{pane}'"))?;
    let fields: Vec<&str> = out.split('\t').collect();
    let mut record = parse_record(&fields);
    record.tokens = read_tokens(t, pane)?;
    Ok(record)
}

fn read_tokens(t: &Tmux, pane: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let out = t
        .run(&["show-options", "-p", "-t", pane])
        .with_context(|| format!("reading @agent_token_* options for pane '{pane}'"))?;
    Ok(crate::tmux::parse_option_lines(&out)
        .into_iter()
        .filter_map(|(name, value)| {
            name.strip_prefix(TOKEN_PREFIX)
                .map(|key| (key.to_string(), value))
        })
        .collect())
}

/// Writes a state transition: `@agent_state`, `@agent_state_since` and `@agent_source`,
/// plus `@agent_rule` (unset when `rule` is `None` — e.g. a hook-reported state has no
/// manifest rule behind it).
///
/// All four land in **one** `tmux` invocation, as `set-option … ; set-option …`. Issued
/// as separate invocations they were separate server commands, so a concurrent reader
/// (the rail, the popup, `agent list`, another `taw` process) could observe a torn
/// transition — a new `@agent_state` next to the *previous* `@agent_rule`,
/// `@agent_source` or `@agent_state_since`. tmux executes the commands of a single
/// invocation before replying, so a reader sees either all four old or all four new.
pub fn write_state(
    t: &Tmux,
    pane: &str,
    state: AgentState,
    rule: Option<&str>,
    source: &str,
    now: u64,
) -> anyhow::Result<()> {
    write_state_with_kind(t, pane, None, state, rule, source, now)
}

/// [`write_state`], and the agent's kind (`@agent_kind`) in the same invocation when `kind` is
/// given: what `taw pane report-agent` writes, since a hook that reports a state also says which
/// agent it is. The kind next to its state and its owner, and never before them: a reader that
/// saw `@agent_kind` first would see an agent that nobody owns, and a watcher that reads a pane
/// with a shell in front takes that for an agent that has exited.
pub fn write_state_with_kind(
    t: &Tmux,
    pane: &str,
    kind: Option<&str>,
    state: AgentState,
    rule: Option<&str>,
    source: &str,
    now: u64,
) -> anyhow::Result<()> {
    let mut commands = Commands::default();
    commands.add_state(
        pane,
        &StateWrite {
            kind,
            state,
            rule,
            source,
            now,
            since: Since::Always,
        },
    );
    commands
        .run(t)
        .map(|_| ())
        .with_context(|| format!("writing agent state for pane '{pane}'"))
}

/// Writes `to` over the state `over` that an agent's own report left on the pane, and only while
/// the pane still holds `over`: the state and its since, in one invocation, with the report's
/// source and kind left as they are. The check is tmux's (`if-shell -F`), a step of the same
/// invocation, because a report that came since this watcher read the pane must not be undone by
/// a write made from what it read: the agent has no screen to correct it from.
pub fn write_state_over(
    t: &Tmux,
    pane: &str,
    over: AgentState,
    to: AgentState,
    now: u64,
) -> anyhow::Result<()> {
    let still = format!("#{{==:#{{@agent_state}},{over}}}");
    let writes = format!(
        "set-option -p -t {pane} @agent_state {to} ; set-option -p -t {pane} @agent_state_since {now}"
    );
    t.run_ok(&["if-shell", "-F", "-t", pane, &still, &writes])
        .with_context(|| format!("writing {to} over the reported {over} for pane '{pane}'"))
}

/// What an agent's own hook leaves on a pane about itself: the session it reported, where that is
/// kept, what started that session (the `session_start_source` token), its word that it was the
/// agent talking (`@agent_reported_kind`), and the command it reported that resumes its session
/// ([`crate::agent_resume::OPTIONS`]), and the flags its Claude was started with
/// ([`crate::claude_flags::OPTION`]). All of it is about that agent, so it goes when another takes
/// the pane (`drop_identity_of_another_agent`, and the watcher's `release_identity`).
pub const IDENTITY_OPTIONS: [&str; 8] = [
    "@agent_session_id",
    "@agent_session_path",
    "@agent_token_session_start_source",
    "@agent_reported_kind",
    crate::agent_resume::SOURCE_OPTION,
    crate::agent_resume::AGENT_OPTION,
    crate::agent_resume::ARGV_OPTION,
    crate::claude_flags::OPTION,
];

/// The token `report-agent-session --session-start-source` records, without its prefix: one of
/// [`IDENTITY_OPTIONS`] once the prefix is put back.
const SESSION_START_TOKEN: &str = "session_start_source";

/// `@agent_turn`: a stamp that `taw pane report-agent` writes, in the same tmux invocation as the
/// rest of the report, each time it reports `working` or `blocked`, and that only such a report
/// moves. The watcher reads the pane at most once a tick, so a turn that begins and ends inside one
/// leaves the state it had taken in: `working` and `idle` back to back change nothing it can see but
/// this. It is a stamp of the moment and not a count: a count would need a read before the write,
/// or tmux doing the arithmetic (`set-option -F` with an `e|+` format), and the one invocation
/// cannot count on either across the tmux versions this runs on. The watcher only asks whether it
/// moved. It belongs to the agent, and goes with it.
pub const TURN_OPTION: &str = "@agent_turn";

/// `@agent_sandbox`: the profile `taw sandbox` started the pane's agent through, set by `taw
/// sandbox` in its own pane before it becomes the profile, and by a restore with the line it types.
/// It is not among the options an agent's record is cleared of ([`CLEARED_ON_AGENT_EXIT`]): while a
/// first start builds an image and installs tools, the profile in front of the pane is a shell
/// script, which the watcher reads as the agent's shell come back, and it clears the agent's record
/// then. The watcher clears it itself at one of its two ends (`sandbox::mark`): the process that set
/// it ([`SANDBOX_PID_OPTION`]) has ended with no container in front, which is the container leaving
/// the pane too, since the profile becomes the runtime; or, for a restore's mark, which has no
/// process, a container that came has left. `taw agent start` takes it off with the last agent's
/// record.
pub const SANDBOX_OPTION: &str = "@agent_sandbox";

/// `@agent_sandbox_pid`: the pid of the `taw sandbox` that set [`SANDBOX_OPTION`], which `exec`
/// keeps through the profile and the runtime it becomes, so its end is the profile's. A restore's
/// mark has none until the line it typed runs.
pub const SANDBOX_PID_OPTION: &str = "@agent_sandbox_pid";

/// `@agent_sandbox_resume`: what a restore resumed a sandboxed agent with ([`SANDBOX_OPTION`]'s
/// pane), as JSON (`session::snapshot::KeptResume`), kept while its profile starts: the watcher
/// takes the agent's kind and session off the pane at its first read, the profile in front reading
/// as the agent's shell come back, and a save in that time still has to resume the conversation. It
/// goes with the mark.
pub const SANDBOX_RESUME_OPTION: &str = "@agent_sandbox_resume";

/// The options of a pane's profile mark, which go together: at the mark's end (the watcher), with
/// the last agent's record (`taw agent start`), and when a start did not happen.
pub const SANDBOX_OPTIONS: &[&str] = &[SANDBOX_OPTION, SANDBOX_PID_OPTION, SANDBOX_RESUME_OPTION];

// Follows herdr's src/terminal/state.rs, `TerminalState::restore_error`, as
// src/persist/restore.rs (`unavailable_restored_terminal`) and src/app/agent_resume.rs set it.
// Differences: kept in a pane option, which only closing the pane takes off; tmux has made the pane,
// in the home folder, by the time a restore finds its saved directory gone, where herdr starts nothing.
/// `@agent_restore_error`: why a session restore left the pane as tmux made it, herdr's
/// `restore_error`: its saved directory was gone, so tmux opened it in the home folder, and the restore
/// typed nothing into it (`session::restore`). `taw pane get` and `pane list` show it. It is the pane's
/// and not its agent's, so nothing that clears an agent's record takes it off: it goes with the pane,
/// or with [`RESTORE_SAVED_OPTION`] once an agent of the pane's own is recorded in it ([`RESTORE_OPTIONS`]).
pub const RESTORE_ERROR_OPTION: &str = "@agent_restore_error";

// Follows herdr's src/persist/restore.rs, what `unavailable_restored_terminal`
// keeps of a pane it could not restore: its saved directory, label, agent and session, and the command
// its agent reported. Differences: kept as the saved pane's own JSON in a pane option, which a save
// reads in place of the pane, since tmux has made the pane, in the home folder.
/// `@agent_restore_saved`: the pane as the snapshot a restore read had saved it (its JSON, as
/// `session::snapshot::PaneSnap` writes it), set with [`RESTORE_ERROR_OPTION`] on a pane whose saved
/// directory was gone. Every save keeps that pane as it was saved, its directory, its agent and session
/// and the command its agent reported, and not as tmux and the pane have it now, as herdr keeps such a
/// pane's saved directory and agent session (`session::snapshot::take`), until the pane is closed or an
/// agent of its own is recorded in it ([`RESTORE_OPTIONS`]).
pub const RESTORE_SAVED_OPTION: &str = "@agent_restore_saved";

/// What a restore marks a pane whose saved directory is gone with, which goes together: when the pane
/// is closed, or as soon as an agent of the pane's own is recorded in it, in the tmux invocation that
/// records it (the watcher naming it, `taw agent start` once it is there, and every report of an
/// agent's state or session; [`record_kind`], `Commands::end_restore_keep`). From then on the pane is
/// saved as it is, with that agent: its conversation would otherwise be written over, at every save, by
/// the one the pane was saved with, and lost to the next restore.
pub const RESTORE_OPTIONS: [&str; 2] = [RESTORE_ERROR_OPTION, RESTORE_SAVED_OPTION];

// Follows herdr's src/terminal/state.rs, `agent_process_acquisition_pending`
// (#4457). Differences: kept in a pane option rather than in memory, with the turn stamp the agent
// was taken at, so that it outlives a watcher.
/// `@agent_acquiring`: set while the pane's agent is being taken, herdr's
/// `agent_process_acquisition_pending` (#4457). taw took it just now, and has not seen it working
/// since, so its first `idle` is the agent becoming ready and not a turn ending: a trust question
/// at startup answered later is no finished turn. `taw agent start` sets it as it takes the last
/// agent's record off the pane (herdr's `begin_managed_agent`), and so does a session restore for
/// the agent each resume starts; the watcher sets it for an agent it takes as a new one or whose
/// session another replaced, and takes it off once the agent works, or is first idle. It is kept
/// in tmux rather than in the watcher, so a watcher started again keeps it, as herdr keeps it
/// across a live handoff, and it says when the agent was taken ([`Acquired`]). It belongs to the
/// agent, and goes with it.
pub const ACQUIRING_OPTION: &str = "@agent_acquiring";

/// `@agent_typed_line`: where `taw agent start` typed the agent's command, as `<row> <history> <column>
/// [right] <length> <command> <start>` (`watch::typed_line::TypedLine`): the row of the screen the command's
/// last row was on, the size of the pane's history then, the column it was typed at, whether the prompt
/// row showed a right prompt, the command, and what its row starts with. The pane still shows what its last
/// occupant left, and what the command started can have drawn only below that line, so the watcher reads
/// those rows only while the line is on the screen, and takes the option off once it is not (the agent's
/// drawing has taken the whole screen). It belongs to the agent, and goes with it.
pub const TYPED_LINE_OPTION: &str = "@agent_typed_line";

/// When the pane's agent was taken, as [`ACQUIRING_OPTION`] holds it: by the turn stamp the pane had
/// then ([`TURN_OPTION`]), which a report of work moves, so that a watcher reading the pane for the
/// first time can tell work reported since the agent was taken from work it was taken over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acquired {
    /// Taken when the pane's turn stamp was this one, or when it had none (`None`): `taken <stamp>`,
    /// or `taken`.
    At(Option<String>),
    /// Taken at a moment the mark does not say: the bare `1` that taw wrote before the mark held the
    /// stamp, or a value no taw writes.
    Unknown,
}

impl Acquired {
    /// What [`ACQUIRING_OPTION`] holding `value` says, `None` for nothing (the agent is not being
    /// taken). An empty option and an unset one read alike in a format, so even "no stamp" is a
    /// word, `taken`.
    pub fn parse(value: &str) -> Option<Acquired> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        Some(match value.strip_prefix("taken") {
            Some("") => Acquired::At(None),
            Some(rest) if rest.starts_with(' ') && !rest.trim().is_empty() => {
                Acquired::At(Some(rest.trim().to_string()))
            }
            _ => Acquired::Unknown,
        })
    }

    /// The value [`ACQUIRING_OPTION`] is written with. A mark whose moment is unknown is written as
    /// taw wrote it before, `1`.
    pub fn value(&self) -> String {
        match self {
            Acquired::At(Some(stamp)) => format!("taken {stamp}"),
            Acquired::At(None) => "taken".to_string(),
            Acquired::Unknown => "1".to_string(),
        }
    }
}

/// A stamp for [`TURN_OPTION`]: the nanoseconds since the epoch, which no two reports share.
fn turn_stamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
        .to_string()
}

/// `@agent_final_state`: the status the last agent of a pane left it with, and the millisecond it went,
/// as `<status> <milliseconds>`. A wait for an agent that has gone answers with it when it is a status
/// the wait was for, and with `agent_not_running` when it is not, as herdr's `wait_for_resolved_agent`
/// answers with the final status an agent is released with. The watcher writes it as it takes the
/// record off the pane, and with the exit of an agent whose pane keeps its hint (which has no record
/// taken off). It is the last agent's and not the next one's, so it is not among the options an
/// occupant owns ([`CLEARED_ON_AGENT_EXIT`]): the next agent to leave overwrites it, and a wait
/// reads it only when it is newer than the wait. The time is in milliseconds because a status left a
/// moment before a wait began is in the second the wait began in, and whole seconds cannot tell it
/// from one left after.
pub const FINAL_OPTION: &str = "@agent_final_state";

/// The status an agent left its pane with, and the millisecond it left ([`FINAL_OPTION`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Final {
    pub state: AgentState,
    pub at: u64,
}

impl Final {
    /// What `@agent_final_state` holds, `None` for nothing, or for what is not a status and a time.
    pub fn parse(text: &str) -> Option<Final> {
        let (state, at) = text.trim().split_once(' ')?;
        Some(Final {
            state: state.parse().ok()?,
            at: at.trim().parse().ok()?,
        })
    }
}

/// The millisecond now, for a record written without a clock of its caller's.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The session an agent reported, as far as its report went: either part may be missing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Session<'a> {
    pub id: Option<&'a str>,
    pub path: Option<&'a str>,
}

/// What `taw pane report-agent` records: the agent, its state, its owner (`source`, the
/// `hook:<id>` the watcher leaves the state to) and the session that came with them, and the
/// command that resumes that session when the report carried one.
#[derive(Clone, Copy, Debug)]
pub struct Report<'a> {
    pub kind: &'a str,
    pub state: AgentState,
    pub source: &'a str,
    pub session: Session<'a>,
    pub now: u64,
    /// The command the agent reported that resumes its session, with the source (as the reporter
    /// named it, `--source`) and the agent it is kept under.
    pub resume: Option<&'a crate::agent_resume::ReportedResume>,
    /// The reporter's own `--source`, which a resume command is kept under: another source's
    /// command goes when this one takes the pane.
    pub reporter: &'a str,
}

// Follows herdr's src/terminal/state.rs, `hook_report_sequences`,
// `hook_report_is_newer` and `accept_hook_report`. Differences: kept in a pane option, as text, and
// taken with the agent's record when it leaves the pane.
/// `@agent_report_seq`: the last `--seq` each source sent for the pane's agent, as herdr keeps one for
/// each source of each pane, so that a report whose number is not higher than the last one its source
/// sent is ignored, and a report with no number is taken only from a source that has sent none (herdr's
/// `hook_report_is_newer`). It holds a `<source>=<seq>` word for each source ([`ReportSeqs`]). It
/// belongs to the agent and goes with it, as herdr forgets a source's numbers when the agent's process
/// exits: a new agent's reporter starts again.
pub const REPORT_SEQ_OPTION: &str = "@agent_report_seq";

// Follows herdr's src/terminal/state.rs, `clear_self_reported_agent` and its
// `observed_at`. Differences: herdr's clear asks whether the agent's claim is newer than when the
// shell was seen idle; here a stamp every report writes says whether one landed since the read.
/// `@agent_report_stamp`: a stamp of the moment the last report for the pane's agent was taken (the
/// nanoseconds since the epoch), which `taw pane report-agent` and `report-agent-session` write in the
/// call that takes the report. The watcher takes the record of an agent that has left off the pane only
/// while the pane holds the stamp its read of the pane saw ([`clear_agent`]): an agent that claimed the
/// pane after that read, with the command that resumes its session, is not cleared with the one that
/// left. It belongs to the agent, and goes with it.
pub const REPORT_STAMP_OPTION: &str = "@agent_report_stamp";

/// The most sources [`REPORT_SEQ_OPTION`] keeps the numbers of, the last ones to send one.
const REPORT_SOURCES_KEPT: usize = 32;

/// The most bytes [`REPORT_SEQ_OPTION`] holds once written: its text goes twice into the command that
/// writes a report (what the report read, and what it writes), beside a resume command, and tmux takes
/// at most about 16 KiB in one command. Twice this, with the longest resume command the checks let
/// through (8 KiB of plain words), and the rest of a report, fit in [`TMUX_COMMAND_ROOM`]. The oldest
/// sources' numbers go first when a report would take it past this.
const REPORT_SEQS_BUDGET: usize = 2 * 1024;

/// The last `--seq` of each source that has sent one for the pane's agent ([`REPORT_SEQ_OPTION`]), the
/// last source to send one last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportSeqs(Vec<(String, u64)>);

impl ReportSeqs {
    /// What [`REPORT_SEQ_OPTION`] holding `text` says: a `<source>=<seq>` word for each source, the
    /// source written as [`seq_source`] writes it. A word that is not one says nothing, and is not
    /// written back.
    pub fn parse(text: &str) -> ReportSeqs {
        let mut seqs = ReportSeqs::default();
        for entry in text.split_whitespace() {
            let Some((source, seq)) = entry.rsplit_once('=') else {
                continue;
            };
            let (Some(source), Ok(seq)) = (from_seq_source(source), seq.parse::<u64>()) else {
                continue;
            };
            seqs = seqs.after(&source, Some(seq));
        }
        seqs
    }

    /// The last number `source` sent.
    pub fn last(&self, source: &str) -> Option<u64> {
        self.0
            .iter()
            .find(|(sent, _)| sent == source)
            .map(|(_, seq)| *seq)
    }

    /// Whether a report of `source` numbered `seq` is newer than what the source sent before: a number
    /// higher than its last one, or no number from a source that has sent none (herdr's
    /// `hook_report_is_newer`).
    pub fn newer(&self, source: &str, seq: Option<u64>) -> bool {
        match (seq, self.last(source)) {
            (Some(seq), Some(last)) => seq > last,
            (Some(_), None) | (None, None) => true,
            (None, Some(_)) => false,
        }
    }

    /// The numbers once a report of `source` numbered `seq` is taken: `seq` is its last, and the
    /// others are as they were, but for the oldest ones that go when there are more than
    /// [`REPORT_SOURCES_KEPT`] or their text would take more than [`REPORT_SEQS_BUDGET`]. The report's
    /// own number is kept, whatever its length. A report with no number changes nothing.
    pub fn after(&self, source: &str, seq: Option<u64>) -> ReportSeqs {
        let Some(seq) = seq else {
            return self.clone();
        };
        let mut seqs: Vec<(String, u64)> = self
            .0
            .iter()
            .filter(|(sent, _)| sent != source)
            .cloned()
            .collect();
        seqs.push((source.to_string(), seq));
        let over = seqs.len().saturating_sub(REPORT_SOURCES_KEPT);
        let mut kept = ReportSeqs(seqs.split_off(over));
        while kept.0.len() > 1 && kept.value().len() > REPORT_SEQS_BUDGET {
            kept.0.remove(0);
        }
        kept
    }

    /// The text [`REPORT_SEQ_OPTION`] is written with.
    pub fn value(&self) -> String {
        self.0
            .iter()
            .map(|(source, seq)| format!("{}={seq}", seq_source(source)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A source as [`REPORT_SEQ_OPTION`] holds it: every byte that is not a letter, a digit, `_`, `.`, `:`
/// or `-` as `%` and two hex digits, so a source is one word, with no `=` in it, and nothing a tmux
/// format or command line reads for a meaning, whatever it is called.
fn seq_source(source: &str) -> String {
    let mut written = String::with_capacity(source.len());
    for byte in source.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-') {
            written.push(byte as char);
        } else {
            written.push_str(&format!("%{byte:02X}"));
        }
    }
    written
}

/// The source [`seq_source`] wrote as `written`, `None` for what it never writes.
fn from_seq_source(written: &str) -> Option<String> {
    let bytes = written.as_bytes();
    let mut source = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = written.get(at + 1..at + 3)?;
            source.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            source.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(source).ok().filter(|s| !s.is_empty())
}

/// What a report is written over, in [`REPORT_SEQ_OPTION`]: the text the option held when the report
/// read it, and the text it holds once the report is taken. The report lands only while the pane still
/// holds `read`, which tmux is asked in the invocation that writes it (`if-shell -F`): another report
/// can land between the read and the write, and the report is then decided again over what that one
/// left. The numbers themselves are compared by taw, not by tmux, whose format arithmetic is in floating
/// point and cannot tell two 19-digit nanosecond stamps apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub read: String,
    pub next: String,
}

/// What a write over an [`Order`] did: took the report, with what the write read back as it did, or
/// found that the pane no longer held what the report had read, and wrote nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written<T> {
    Taken(T),
    Changed,
}

/// What [`REPORT_SEQ_OPTION`] of `pane` holds, as text: what an [`Order`] is read from.
pub fn read_report_seqs(t: &Tmux, pane: &str) -> anyhow::Result<String> {
    t.display(pane, &format!("#{{{REPORT_SEQ_OPTION}}}"))
        .with_context(|| format!("reading {REPORT_SEQ_OPTION} of pane '{pane}'"))
}

/// The most bytes of words tmux takes in one command, with an end to each: its client packs them into
/// one message of 16 KiB, and the socket and the client's own words go in it too.
const TMUX_COMMAND_ROOM: usize = 15 * 1024;

/// A write too long for one tmux command, which tmux would refuse whole (`command too long`): a value it
/// carries is too long once quoted for the command line of an `if-shell`. Nothing is written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the report is too long for one tmux command once quoted: {bytes} bytes, {TMUX_COMMAND_ROOM} at the most"
)]
pub struct TooLong {
    pub bytes: usize,
}

/// `args`, when they fit in one tmux command.
fn fitting(args: Vec<String>) -> Result<Vec<String>, TooLong> {
    let bytes = args.iter().map(|word| word.len() + 1).sum();
    if bytes > TMUX_COMMAND_ROOM {
        return Err(TooLong { bytes });
    }
    Ok(args)
}

/// What a guarded write prints when it has taken the report, before what its answer expands to.
const TAKEN: &str = "taw-taken";

/// What a guarded write prints when the pane no longer held what the report had read.
const CHANGED: &str = "taw-changed";

/// The arguments of one invocation that runs `commands`, and sets [`REPORT_SEQ_OPTION`] to
/// `order.next`, only while `pane` still holds `order.read` there, and the condition `also` (a format)
/// is true when one is given; and then prints [`TAKEN`] and what the format `answer` expands to. Or,
/// when either does not hold, prints [`CHANGED`] and does nothing. The condition and the commands are
/// one step of one invocation, so no other client's command lands between them.
///
/// Text the option holds that taw does not write (anything but the words of [`ReportSeqs::value`])
/// cannot be compared with as it was read, which can have lost a trailing line break: the write then
/// lands over it whatever it is, and the next one is guarded again.
fn guarded(
    pane: &str,
    order: &Order,
    also: Option<&str>,
    mut commands: Commands,
    answer: &str,
) -> Vec<String> {
    let held = if written_by_taw(&order.read) {
        format!(
            "#{{==:#{{{REPORT_SEQ_OPTION}}},{}}}",
            format_literal(&order.read)
        )
    } else {
        "1".to_string()
    };
    let condition = match also {
        Some(also) => format!("#{{&&:{also},{held}}}"),
        None => held,
    };
    if order.next != order.read {
        if order.next.is_empty() {
            commands.unset(pane, REPORT_SEQ_OPTION);
        } else {
            commands.set(pane, REPORT_SEQ_OPTION, &order.next);
        }
    }
    commands.add(&[
        "display-message",
        "-p",
        "-t",
        pane,
        &format!("{TAKEN} {answer}"),
    ]);
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.to_string(),
        condition,
        commands.line(),
        format!("display-message -p {CHANGED}"),
    ]
}

/// Whether `text` is made of what [`ReportSeqs::value`] writes, and of nothing else.
fn written_by_taw(text: &str) -> bool {
    text.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '=' | '%' | '_' | '.' | ':' | '-'))
}

/// What a guarded write's invocation printed: `Some` of what its answer expanded to when the report
/// was taken, `None` when the pane had changed.
fn taken(printed: &str) -> anyhow::Result<Option<String>> {
    let printed = printed.trim();
    if printed == CHANGED {
        return Ok(None);
    }
    match printed.strip_prefix(TAKEN) {
        Some(answer) => Ok(Some(answer.trim().to_string())),
        None => anyhow::bail!("a write of a report printed {printed:?}"),
    }
}

/// Writes a report of an agent's state in **one** tmux invocation, while the pane holds what the report
/// read of the order of reports ([`Order`]), with the source's number among the writes: what the agent
/// that had the pane reported of itself goes when this is another agent ([`IDENTITY_OPTIONS`]), then the
/// kind, the state with its since, rule and owner, and the session the report carried, which is written
/// over whatever the drop left. A reader sees all of the last agent's record or all of this one's, never
/// this agent beside the last one's session, and never the kind without its owner (see
/// [`write_state_with_kind`]).
///
/// Taken, it gives the pid of the pane's watcher (`@agent_watch_pid`), read in the same invocation,
/// after the write: what the report has to tell that it has something to read (`watch::nudge`). `None`
/// when the pane has no watcher, or what it records is no pid.
pub fn write_report(
    t: &Tmux,
    pane: &str,
    report: &Report<'_>,
    order: &Order,
) -> anyhow::Result<Written<Option<u32>>> {
    let args = fitting(guarded(
        pane,
        order,
        None,
        report_commands(pane, report, &turn_stamp()),
        "#{@agent_watch_pid}",
    ))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let printed = t
        .run(&args)
        .with_context(|| format!("writing the agent's report for pane '{pane}'"))?;
    Ok(match taken(&printed)? {
        Some(watcher) => Written::Taken(watcher.parse().ok()),
        None => Written::Changed,
    })
}

/// The commands of [`write_report`], with `stamp` for [`TURN_OPTION`] where the report moves it.
///
/// What the pane holds is compared before the report is written over it, so the commands that ask
/// (the turn stamp, the since) come before the state: a report that says again what the agent
/// already was moves neither. `@agent_state_since` says when the state last moved, and it is read
/// as that (the age of a wait, the `blocked` an agent is in anew); and the stamp is what shows an
/// agent at work, which a report of the same `blocked` over and over is not.
fn report_commands(pane: &str, report: &Report<'_>, stamp: &str) -> Commands {
    let mut commands = Commands::default();
    commands.drop_identity_of_another_agent(pane, report.kind);
    commands.end_restore_keep(pane);
    commands.add_turn_stamp(pane, report.state, stamp);
    commands.set(pane, REPORT_STAMP_OPTION, stamp);
    commands.add_state(
        pane,
        &StateWrite {
            kind: Some(report.kind),
            state: report.state,
            rule: None,
            source: report.source,
            now: report.now,
            since: Since::WhenTheStateMoves,
        },
    );
    commands.add_session(pane, &report.session);
    // The report takes the pane for its source and agent: a command another source or agent reported
    // is not theirs to be resumed with (herdr's `reconcile_reported_resume`).
    commands.drop_resume_unless_reported_by(pane, report.reporter, report.kind);
    if let Some(resume) = report.resume {
        commands.add_resume(pane, resume);
    }
    commands
}

/// What a session report that carries a resume command does with it: the command, and whether its
/// reporter holds the pane, as the report read it, which only a reporter that does may set a command
/// for ([`Holding`]). The write lands only while the pane still holds what the decision was made on.
#[derive(Clone, Copy, Debug)]
pub struct Offer<'a> {
    pub resume: &'a crate::agent_resume::ReportedResume,
    pub holding: Holding,
}

/// Whether a reporter holds a pane and may set the command that resumes its agent there (herdr's
/// `can_record_reported_resume`), as tmux answered it and taw decided it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holding {
    /// The pane's source is the reporter's `hook:<source>`, for the agent it reports for.
    Holds,
    /// Another hook holds the pane, or this one for another agent.
    AnotherHook,
    /// No hook holds the pane, and its agent in front, named by its process, is the one reported
    /// (`true`) or is not (`false`).
    NoHook(bool),
}

impl Holding {
    /// Whether the reporter may set the command.
    pub fn accepts(self) -> bool {
        matches!(self, Holding::Holds | Holding::NoHook(true))
    }

    /// The tmux format that is true while the pane is still as this was read: what a write that
    /// stands on it is guarded with.
    fn still(self, source: &str, agent: &str) -> String {
        match self {
            Holding::Holds => held_by(source, agent),
            Holding::AnotherHook => {
                format!("#{{&&:{HOOKED},#{{==:{},0}}}}", held_by(source, agent))
            }
            Holding::NoHook(_) => format!("#{{==:{HOOKED},0}}"),
        }
    }
}

/// True (`1`) while a hook holds the pane's state (`@agent_source` is `hook:<id>`).
const HOOKED: &str = "#{m:hook:*,#{@agent_source}}";

/// The tmux format that is true while the pane's source is `hook:<source>`, for `agent`.
fn held_by(source: &str, agent: &str) -> String {
    format!(
        "#{{&&:#{{==:#{{@agent_source}},hook:{}}},#{{==:#{{@agent_kind}},{}}}}}",
        format_literal(source),
        format_literal(agent)
    )
}

/// Whether the reporter `source` holds `pane` for `agent`, and whether any hook does, as tmux answers
/// it now, in one read: the questions [`Holding`] is decided on, besides which agent is in front.
pub fn read_holding(
    t: &Tmux,
    pane: &str,
    source: &str,
    agent: &str,
) -> anyhow::Result<(bool, bool)> {
    let read = t
        .display(pane, &format!("{}\t{HOOKED}", held_by(source, agent)))
        .with_context(|| format!("reading who holds pane '{pane}'"))?;
    let (held, hooked) = read.split_once('\t').unwrap_or((read.as_str(), ""));
    Ok((held.trim() == "1", hooked.trim() == "1"))
}

/// Writes a report of an agent's session in one tmux invocation, as [`write_report`] does for a
/// state, and over an [`Order`] the same way: the agent (`@agent_kind`) and its word that it said so
/// (`@agent_reported_kind`), which keeps it on the pane while nothing names it, next to the session,
/// and what started that session as a token when the report says.
///
/// With `claude_flags`, the flags the pane's Claude was started with ([`crate::claude_flags`]) are
/// written with the session, or taken off for an empty list, so a report that is out of order writes
/// neither. With an [`Offer`], the command it carries is set after the rest when its reporter may set
/// one, and the write lands only while the pane is still as the offer was decided on. Taken, it says
/// whether the command was set.
#[allow(clippy::too_many_arguments)]
pub fn write_session_report(
    t: &Tmux,
    pane: &str,
    kind: &str,
    session: &Session<'_>,
    start_source: Option<&str>,
    claude_flags: Option<&[String]>,
    order: &Order,
    offer: Option<&Offer<'_>>,
) -> anyhow::Result<Written<bool>> {
    let mut commands = Commands::default();
    commands.drop_identity_of_another_agent(pane, kind);
    commands.end_restore_keep(pane);
    commands.set(pane, REPORT_STAMP_OPTION, &turn_stamp());
    commands.set(pane, "@agent_kind", kind);
    commands.set(pane, "@agent_reported_kind", kind);
    commands.add_session(pane, session);
    commands.add_claude_flags(pane, claude_flags);
    if let Some(start) = start_source {
        // No option of its own; recorded as a custom token so it is still
        // visible to plugins and the TUI without inventing a new `@agent_*` key.
        commands.set(
            pane,
            &format!("{TOKEN_PREFIX}{SESSION_START_TOKEN}"),
            &fold_text(start),
        );
    }
    let still = offer.map(|offer| {
        offer
            .holding
            .still(&offer.resume.source, &offer.resume.agent)
    });
    let accepted = offer.filter(|offer| offer.holding.accepts());
    if let Some(offer) = accepted {
        commands.add_resume(pane, offer.resume);
    }
    let args = fitting(guarded(pane, order, still.as_deref(), commands, ""))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let printed = t
        .run(&args)
        .with_context(|| format!("writing the agent's session for pane '{pane}'"))?;
    Ok(match taken(&printed)? {
        Some(_) => Written::Taken(accepted.is_some()),
        None => Written::Changed,
    })
}

/// What the sandbox report reader writes (`sandbox::report`): the session an agent's integration
/// reported from inside the pane's sandbox, as [`write_session_report`] writes one (the agent's word
/// that it was there, `@agent_reported_kind`, the id, the path, what started the session as a token),
/// in the one tmux command that first checks the pane still holds the profile mark `mark` with the
/// process `pid` (its text, empty for none) and the agent `kind` the read found: a fresh `taw sandbox`
/// of the same profile has another process, and the old container's session is not its to carry. A
/// report that comes in as the agent leaves is not the next occupant's to carry either. Says whether
/// it was written.
///
/// No [`Order`] and no [`Offer`]: the watcher is no reporter with a sequence, and a report from inside
/// carries no command that resumes it. No report stamp ([`REPORT_STAMP_OPTION`]) either: the stamp
/// holds the watcher's own writes back when a report from outside lands after its read, and this is
/// the watcher's write, made after the others of its read; set, it would read as an outside claim to
/// the next read's guards. Its session goes with the mark (`end_sandbox`).
pub fn write_sandboxed_session(
    t: &Tmux,
    pane: &str,
    kind: &str,
    mark: &str,
    pid: &str,
    session: &Session<'_>,
    start_source: Option<&str>,
) -> anyhow::Result<bool> {
    let args = sandboxed_session_args(pane, kind, mark, pid, session, start_source)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let printed = t
        .run(&args)
        .with_context(|| format!("writing the sandboxed agent's session for pane '{pane}'"))?;
    Ok(printed.trim() != CHANGED)
}

/// The tmux invocation of [`write_sandboxed_session`], or [`TooLong`] when it would not fit in one
/// command. What a report can bring is bounded by what `sandbox::report` keeps (an id of 128 bytes,
/// a path of 4096 bytes, a source of 32), so a report's own words never come to it.
fn sandboxed_session_args(
    pane: &str,
    kind: &str,
    mark: &str,
    pid: &str,
    session: &Session<'_>,
    start_source: Option<&str>,
) -> Result<Vec<String>, TooLong> {
    let still = format!(
        "#{{&&:{},#{{==:#{{@agent_kind}},{}}}}}",
        held_sandbox(mark, pid),
        format_literal(kind)
    );
    let mut commands = Commands::default();
    commands.set(pane, "@agent_reported_kind", kind);
    commands.add_session(pane, session);
    if let Some(start) = start_source {
        commands.set(
            pane,
            &format!("{TOKEN_PREFIX}{SESSION_START_TOKEN}"),
            &fold_text(start),
        );
    }
    fitting(vec![
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.to_string(),
        still,
        commands.line(),
        format!("display-message -p {CHANGED}"),
    ])
}

/// Gives the pane's state back to its watcher, as `taw pane release-agent` does: takes `@agent_source`
/// off the pane while it is still `hook:<reporter>`, and while the pane holds what the release read of
/// the order of reports ([`Order`]), with the source's number among the writes, in one tmux invocation;
/// and the command the reporter reported for `agent` that resumes its session goes with it, as herdr's
/// release takes the reporter's resume command away (`forget_reported_resume_of`).
pub fn release_source(
    t: &Tmux,
    pane: &str,
    reporter: &str,
    agent: &str,
    order: &Order,
) -> anyhow::Result<Written<()>> {
    let mut commands = Commands::default();
    commands.unset(pane, "@agent_source");
    commands.drop_resume_reported_by(pane, reporter, agent);
    let still = format!(
        "#{{==:#{{@agent_source}},hook:{}}}",
        format_literal(reporter)
    );
    let args = fitting(guarded(pane, order, Some(&still), commands, ""))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let printed = t
        .run(&args)
        .with_context(|| format!("clearing @agent_source of pane '{pane}'"))?;
    Ok(match taken(&printed)? {
        Some(_) => Written::Taken(()),
        None => Written::Changed,
    })
}

// Follows herdr's src/persist/restore.rs, the reported agent a restored pane
// starts with. Differences: the agent's word that it is there is `@agent_reported_kind`, which the
// watcher reads as herdr's self-reported agent.
/// Sets the command an agent reported that resumes its session on `pane` ([`crate::agent_resume`]), its
/// three options, and the agent's word that it is there (`@agent_reported_kind`, the agent the command was
/// reported for), in one tmux invocation: what a restore puts back on the pane it typed the command into,
/// so that the next save keeps it. With that word the watcher keeps the agent as one that reports for
/// itself, until the pane's own shell is back at its prompt, as it keeps one whose hook has said so; an
/// agent taw has no other way to name (neither its process nor its hint names one taw knows) would
/// otherwise be taken for gone once it had been missed a few times, with its command, though it ran on.
pub fn set_resume(
    t: &Tmux,
    pane: &str,
    resume: &crate::agent_resume::ReportedResume,
) -> anyhow::Result<()> {
    let mut commands = Commands::default();
    commands.add_resume(pane, resume);
    commands.set(pane, "@agent_reported_kind", &resume.agent);
    commands
        .run(t)
        .map(|_| ())
        .with_context(|| format!("setting the resume command of pane '{pane}'"))
}

/// Records `kind` as the agent of `pane` (`@agent_kind`), as the watcher does when it names the agent in
/// front of the pane and `taw agent start` once the agent it started is there, in one tmux invocation
/// with the end of what a restore kept of the pane ([`RESTORE_OPTIONS`]): the pane has an agent of its
/// own now.
pub fn record_kind(t: &Tmux, pane: &str, kind: &str) -> anyhow::Result<()> {
    let mut commands = Commands::default();
    commands.set(pane, "@agent_kind", kind);
    commands.end_restore_keep(pane);
    commands
        .run(t)
        .map(|_| ())
        .with_context(|| format!("recording the agent of pane '{pane}'"))
}

/// Marks `pane` with the profile a restore types its agent's `taw sandbox` line for ([`SANDBOX_OPTION`],
/// with no process), and with `kept`, what it resumes the agent with as JSON
/// ([`SANDBOX_RESUME_OPTION`]), in one tmux invocation. A `kept` too long for one tmux command is a
/// [`TooLong`], and nothing is written.
pub fn mark_restored_sandbox(
    t: &Tmux,
    pane: &str,
    profile: &str,
    kept: &str,
) -> anyhow::Result<()> {
    let mut commands = Commands::default();
    commands.set(pane, SANDBOX_OPTION, profile);
    commands.set(pane, SANDBOX_RESUME_OPTION, kept);
    let args = fitting(commands.args())?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    t.run_ok(&args)
        .with_context(|| format!("marking pane '{pane}' with the sandbox a restore resumes"))
}

/// Keeps `kept`, what `taw agent start --sandbox` started its agent with as JSON
/// (`session::snapshot::KeptResume`), beside `pane`'s profile mark ([`SANDBOX_RESUME_OPTION`]), in one
/// tmux invocation, only while the pane holds a mark with a process, the one its typed `taw sandbox`
/// set: a session written plain could land after that mark had ended (a container that ends at once,
/// and the watcher sees go), and the pane would hold it with no profile beside it, which a save keeps
/// and a restore types outside the sandbox. Says whether it was kept.
pub fn keep_beside_the_mark(t: &Tmux, pane: &str, kept: &str) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    commands.set(pane, SANDBOX_RESUME_OPTION, kept);
    let held = format!("#{{&&:#{{{SANDBOX_OPTION}}},#{{{SANDBOX_PID_OPTION}}}}}");
    let out = t
        .run(&[
            "if-shell",
            "-F",
            "-t",
            pane,
            &held,
            &commands.line(),
            &format!("display-message -p {CHANGED}"),
        ])
        .with_context(|| format!("keeping what pane '{pane}' was started with beside its mark"))?;
    Ok(out.trim() != CHANGED)
}

/// Marks `pane` as one a session restore left as tmux made it, with why ([`RESTORE_ERROR_OPTION`]):
/// herdr's words for a pane whose saved directory is gone; and with `kept`, the pane as it was saved,
/// for every save to keep in its place ([`RESTORE_SAVED_OPTION`]), when it is given. Both in one tmux
/// invocation. A `kept` too long for one tmux command is a [`TooLong`], and nothing is written.
pub fn mark_restore_error(
    t: &Tmux,
    pane: &str,
    error: &str,
    kept: Option<&str>,
) -> anyhow::Result<()> {
    let mut commands = Commands::default();
    commands.set(pane, RESTORE_ERROR_OPTION, error);
    if let Some(kept) = kept {
        commands.set(pane, RESTORE_SAVED_OPTION, kept);
    }
    let args = fitting(commands.args())?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    t.run_ok(&args)
        .with_context(|| format!("marking why pane '{pane}' was left as tmux made it"))
}

/// Takes the command an agent reported that resumes its session off `pane`, its three options in one tmux
/// invocation: what the watcher does when the agent a pane's hint names exits, which leaves the rest of
/// the record standing. Only while the pane holds the report stamp `seen` ([`REPORT_STAMP_OPTION`]), as
/// the watcher's read that found the agent gone saw it, which tmux is asked in the same invocation: a
/// report that landed since is an agent that claimed the pane after that read, with a command of its own.
/// Says whether the command was taken off.
pub fn drop_resume(t: &Tmux, pane: &str, seen: &str) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    for option in crate::agent_resume::OPTIONS {
        commands.unset(pane, option);
    }
    run_unless_reported_since(t, pane, seen, None, &commands)
        .with_context(|| format!("taking the resume command off pane '{pane}'"))
}

/// Takes [`IDENTITY_OPTIONS`] off `pane`, what the last agent reported of itself, while the format `also`
/// is true and the pane holds the report stamp `seen` ([`REPORT_STAMP_OPTION`]), as the watcher's read
/// that found another agent in the pane saw it, all in one tmux invocation (`watch::release_identity`). A
/// report that landed since that read is the agent that took the pane saying so, with its own session and
/// command, which are not the last agent's to lose (its report took the last one's off itself). Says
/// whether they were taken off.
pub fn drop_identity(t: &Tmux, pane: &str, also: &str, seen: &str) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    for option in IDENTITY_OPTIONS {
        commands.unset(pane, option);
    }
    run_unless_reported_since(t, pane, seen, Some(also), &commands)
        .with_context(|| format!("dropping what the last agent of pane '{pane}' reported"))
}

/// Writes the state the agent of `pane` leaves with, as the watcher does when it finds the agent gone: the
/// status it leaves with ([`FINAL_OPTION`], with the millisecond it left at) when it leaves one, and the
/// state, its rule and its owner, all in one tmux invocation, and only while the pane holds the report
/// stamp `seen` ([`REPORT_STAMP_OPTION`]), as the watcher's read saw it. A report that landed since is an
/// agent that claimed the pane after that read: the watcher's `manifest` written over it would take the
/// pane back from its hook, and the next read would take its record off with the agent that left. Says
/// whether the state was written.
#[allow(clippy::too_many_arguments)]
pub fn write_exit(
    t: &Tmux,
    pane: &str,
    left: Option<(AgentState, u64)>,
    state: AgentState,
    rule: Option<&str>,
    source: &str,
    now: u64,
    seen: &str,
) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    if let Some((status, at)) = left {
        commands.set(pane, FINAL_OPTION, &format!("{status} {at}"));
    }
    commands.add_state(
        pane,
        &StateWrite {
            kind: None,
            state,
            rule,
            source,
            now,
            since: Since::Always,
        },
    );
    run_unless_reported_since(t, pane, seen, None, &commands)
        .with_context(|| format!("writing the state the agent of pane '{pane}' left with"))
}

/// When a state written to a pane moves its `@agent_state_since`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Since {
    /// Every time it is written: a transition the watcher has decided.
    Always,
    /// Only when the pane does not already hold that state: a report that says again what the agent
    /// already was is no change of state.
    WhenTheStateMoves,
}

/// A state to write to a pane, with its owner and when the time moves.
#[derive(Clone, Copy)]
struct StateWrite<'a> {
    kind: Option<&'a str>,
    state: AgentState,
    rule: Option<&'a str>,
    source: &'a str,
    now: u64,
    since: Since,
}

/// Several tmux commands sent as the arguments of one invocation. tmux runs all of them before it
/// answers any other client, so a reader (the watcher, the popup, another `taw`) finds the pane
/// before them or after them and never between. Issued as separate invocations they were separate
/// server commands, and a reader could see half of what was written.
///
/// The commands are kept as their words, and written out as tmux takes them: as the arguments of an
/// invocation ([`Commands::args`]), or as the command line of an `if-shell` ([`Commands::line`]), for
/// a report that is written only while the pane holds what it read ([`Order`]).
#[derive(Default)]
struct Commands(Vec<Vec<String>>);

impl Commands {
    /// One more command, made of `words`.
    fn add(&mut self, words: &[&str]) {
        self.0
            .push(words.iter().map(|text| text.to_string()).collect());
    }

    /// The commands as the arguments of one invocation. `;` is a command separator only as its own
    /// argument (or at the end of one), so the commands are told apart by one, and a word that ends
    /// in a `;` is told from it (see [`word`]); values are otherwise passed as distinct argv entries,
    /// so nothing needs quoting whatever they contain.
    fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for command in &self.0 {
            if !args.is_empty() {
                args.push(";".to_string());
            }
            args.extend(command.iter().map(|text| word(text)));
        }
        args
    }

    /// The commands as one command line that tmux parses from a string, which is what the commands
    /// of `if-shell` are: each word as [`line_word`] writes it, the commands told apart by ` ; `.
    fn line(&self) -> String {
        self.0
            .iter()
            .map(|command| {
                command
                    .iter()
                    .map(|text| line_word(text))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" ; ")
    }

    fn set(&mut self, pane: &str, option: &str, value: &str) {
        self.add(&["set-option", "-p", "-t", pane, option, value]);
    }

    /// tmux has no `unset-option`; `set-option -u` is the removal form.
    fn unset(&mut self, pane: &str, option: &str) {
        self.add(&["set-option", "-u", "-p", "-t", pane, option]);
    }

    /// The four options of a state, and `@agent_kind` first when `kind` is given: the kind next
    /// to the state and its owner, never before them. `since` says when the time moves: the
    /// question of whether the state is a new one is asked of tmux before the state is written.
    fn add_state(&mut self, pane: &str, write: &StateWrite<'_>) {
        let StateWrite {
            kind,
            state,
            rule,
            source,
            now,
            since,
        } = *write;
        if since == Since::WhenTheStateMoves {
            let moves = format!("#{{!=:#{{@agent_state}},{}}}", state.as_str());
            let sets = format!("set-option -p -t {pane} @agent_state_since {now}");
            self.add(&["if-shell", "-F", "-t", pane, &moves, &sets]);
        }
        if let Some(kind) = kind {
            self.set(pane, "@agent_kind", kind);
        }
        self.set(pane, "@agent_state", state.as_str());
        if since == Since::Always {
            self.set(pane, "@agent_state_since", &now.to_string());
        }
        match rule {
            // `@agent_rule` is free text (a manifest author picks the id), and it lands in the
            // same tab-joined listings every other option does, see `fold_text`.
            Some(rule) => self.set(pane, "@agent_rule", &fold_text(rule)),
            None => self.unset(pane, "@agent_rule"),
        }
        self.set(pane, "@agent_source", source);
    }

    /// The stamp of a report of `state`: of work, always, and of a wait (`blocked`) when the pane
    /// was not waiting already, which tmux is asked before the state is written. A report that says
    /// the agent is where it was is nothing it did.
    fn add_turn_stamp(&mut self, pane: &str, state: AgentState, stamp: &str) {
        match state {
            AgentState::Working => self.set(pane, TURN_OPTION, stamp),
            AgentState::Blocked => {
                let anew = format!("#{{!=:#{{@agent_state}},{}}}", state.as_str());
                let sets = format!("set-option -p -t {pane} {TURN_OPTION} {stamp}");
                self.add(&["if-shell", "-F", "-t", pane, &anew, &sets]);
            }
            _ => {}
        }
    }

    /// Records the status the pane holds as the one its agent leaves with ([`FINAL_OPTION`], at
    /// millisecond `now`), taken by tmux from `@agent_state` as the command runs: the state the unsets
    /// after it take off, with no read of it in between that another writer could land a change in. A
    /// pane that holds no state leaves none, and what the last agent left stands.
    fn add_final_from_state(&mut self, pane: &str, now: u64) {
        let copies = format!("set-option -p -F -t {pane} {FINAL_OPTION} '#{{@agent_state}} {now}'");
        self.add(&["if-shell", "-F", "-t", pane, "#{@agent_state}", &copies]);
    }

    /// The session id and path a report carried, each when it did.
    fn add_session(&mut self, pane: &str, session: &Session<'_>) {
        if let Some(id) = session.id {
            self.set(pane, "@agent_session_id", id);
        }
        if let Some(path) = session.path {
            self.set(pane, "@agent_session_path", path);
        }
    }

    /// The flags a Claude was started with that set how its session runs
    /// ([`crate::claude_flags::OPTION`]), when the report read its process: set to the list, or taken
    /// off for an empty one. `None` is a report that could not find the process, which leaves what the
    /// pane holds.
    fn add_claude_flags(&mut self, pane: &str, flags: Option<&[String]>) {
        match flags {
            Some([]) => self.unset(pane, crate::claude_flags::OPTION),
            Some(flags) => self.set(
                pane,
                crate::claude_flags::OPTION,
                &crate::claude_flags::encode(flags),
            ),
            None => {}
        }
    }

    /// Drops [`IDENTITY_OPTIONS`] when the pane records an agent and it is not `kind`: what that
    /// agent reported of itself says nothing about the one taking its place, and a session
    /// restored from it would resume the last agent's conversation under this one. Asked of tmux
    /// itself (`if-shell -F`) so that the answer and the drop are one step of the invocation,
    /// and put before the kind is written, which is what it compares.
    fn drop_identity_of_another_agent(&mut self, pane: &str, kind: &str) {
        let another = format!(
            "#{{&&:#{{!=:#{{@agent_kind}},}},#{{!=:#{{@agent_kind}},{}}}}}",
            format_literal(kind)
        );
        let drops = IDENTITY_OPTIONS
            .map(|option| format!("set-option -p -u -t {pane} {option}"))
            .join(" ; ");
        self.add(&["if-shell", "-F", "-t", pane, &another, &drops]);
    }

    /// Takes off the pane what a restore marked it with because its saved directory was gone
    /// ([`RESTORE_OPTIONS`]), with no question asked: an agent of the pane's own is recorded in it with
    /// these commands, and the pane is saved as it is from now on.
    fn end_restore_keep(&mut self, pane: &str) {
        for option in RESTORE_OPTIONS {
            self.unset(pane, option);
        }
    }

    /// Sets the three options of the command an agent reported that resumes its session
    /// ([`crate::agent_resume::OPTIONS`]): who reported it, for which agent, and the line a restore types.
    fn add_resume(&mut self, pane: &str, resume: &crate::agent_resume::ReportedResume) {
        self.set(pane, crate::agent_resume::SOURCE_OPTION, &resume.source);
        self.set(pane, crate::agent_resume::AGENT_OPTION, &resume.agent);
        self.set(pane, crate::agent_resume::ARGV_OPTION, &resume.line());
    }

    /// Unsets the resume command when another source or another agent than `source` and `agent`
    /// reported it, asked of tmux in the invocation that unsets it.
    fn drop_resume_unless_reported_by(&mut self, pane: &str, source: &str, agent: &str) {
        let another = format!(
            "#{{||:#{{!=:#{{{}}},{}}},#{{!=:#{{{}}},{}}}}}",
            crate::agent_resume::SOURCE_OPTION,
            format_literal(source),
            crate::agent_resume::AGENT_OPTION,
            format_literal(agent)
        );
        self.add(&["if-shell", "-F", "-t", pane, &another, &unset_resume(pane)]);
    }

    /// Unsets the resume command when `source` reported it for `agent`.
    fn drop_resume_reported_by(&mut self, pane: &str, source: &str, agent: &str) {
        let theirs = format!(
            "#{{&&:#{{==:#{{{}}},{}}},#{{==:#{{{}}},{}}}}}",
            crate::agent_resume::SOURCE_OPTION,
            format_literal(source),
            crate::agent_resume::AGENT_OPTION,
            format_literal(agent)
        );
        self.add(&["if-shell", "-F", "-t", pane, &theirs, &unset_resume(pane)]);
    }

    fn run(&self, t: &Tmux) -> anyhow::Result<String> {
        let args = self.args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        t.run(&args)
    }
}

/// The command line that unsets the three options of a reported resume command of `pane`.
fn unset_resume(pane: &str) -> String {
    crate::agent_resume::OPTIONS
        .map(|option| format!("set-option -p -u -t {pane} {option}"))
        .join(" ; ")
}

/// A word of a command line that tmux parses from a string ([`Commands::line`]): as it is when it is
/// made of what that parser reads as itself wherever it stands in a word, and as
/// [`crate::tmux::command_word`] writes it, in double quotes with every meaning escaped, when it is
/// not (a format's `#` and braces, a space, a `$`, a `~`, a `;`, an `=`, a quote). A plain word left
/// bare keeps the line readable in a log of the calls; anything else comes out as exactly itself.
fn line_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '%' | '+')
        });
    if plain {
        text.to_string()
    } else {
        crate::tmux::command_word(text)
    }
}

/// A word of a tmux command as tmux reads it from its arguments: one that ends in a `;` ends
/// the command there, unless a backslash comes before the `;`. A path or a label that ends in
/// one is written with the backslash, and tmux takes it back out.
pub(crate) fn word(text: &str) -> String {
    match text.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => text.to_string(),
    }
}

/// `value` as a literal in a tmux format: `#`, `,` and `}` are the characters a format gives a
/// meaning to inside `#{==:...}` and the like, and each is escaped with a `#`, so that a label that
/// has one is compared as typed.
pub(crate) fn format_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '#' | ',' | '}' => {
                literal.push('#');
                literal.push(c);
            }
            c => literal.push(c),
        }
    }
    literal
}

/// The fixed `@agent_*` options cleared when an agent's occupant leaves a pane.
/// `@agent_watch_pid` and `@agent_kind_hint` are deliberately excluded — the watcher's
/// pid and the manual kind hint both outlive any one occupant; `@agent_name` follows the
/// occupant, and so does `@agent_reported_kind`: what one agent's hook said about itself
/// says nothing about the next occupant. So does `@agent_container`, which says which runtime
/// the occupant runs under.
const CLEARED_ON_AGENT_EXIT: &[&str] = &[
    "@agent_kind",
    "@agent_container",
    "@agent_state",
    "@agent_state_since",
    "@agent_rule",
    "@agent_source",
    "@agent_session_id",
    "@agent_session_path",
    "@agent_name",
    "@agent_display",
    "@agent_title",
    "@agent_seen",
    "@agent_reported_kind",
    TURN_OPTION,
    ACQUIRING_OPTION,
    TYPED_LINE_OPTION,
    REPORT_SEQ_OPTION,
    REPORT_STAMP_OPTION,
    crate::agent_resume::SOURCE_OPTION,
    crate::agent_resume::AGENT_OPTION,
    crate::agent_resume::ARGV_OPTION,
    crate::claude_flags::OPTION,
];

/// Clears all agent-occupant state for `pane`: the fixed options above, plus every
/// `@agent_token_*` custom option. Keeps `@agent_watch_pid` and `@agent_kind_hint`. The agent has
/// left: the status the pane holds is recorded as the one it left with ([`FINAL_OPTION`]).
///
/// Only while the pane holds the report stamp `seen`, what the read that found the agent gone saw of
/// [`REPORT_STAMP_OPTION`] (empty for none), asked of tmux in the invocation that clears: a report that
/// landed since is an agent that claimed the pane after that read, and its record stays, for the next
/// read to decide on. Says whether the record was cleared.
pub fn clear_agent(t: &Tmux, pane: &str, seen: &str) -> anyhow::Result<bool> {
    clear(t, pane, &[], true, Some(seen))
}

/// [`clear_agent`] for a record that is not an agent leaving, and the pane options in `also` as well:
/// `taw agent start` takes what the last agent left off the pane to start the next one, and says
/// nothing of the status the pane held. That is the last agent's, and the watcher recorded it with the
/// time it left at when it went; recorded again at the second the next start begins, it would be
/// taken for a status the new agent left.
pub fn forget_agent_and(t: &Tmux, pane: &str, also: &[&str]) -> anyhow::Result<()> {
    clear(t, pane, also, false, None).map(|_| ())
}

/// Marks `pane` with the profile `taw sandbox` is about to become and that process's pid
/// ([`SANDBOX_OPTION`], [`SANDBOX_PID_OPTION`]), and takes off what the pane's last agent reported
/// of its session ([`IDENTITY_OPTIONS`]), in one tmux invocation. `taw sandbox` is typed at the
/// pane's prompt, so a session the pane holds then is the last agent's: kept, a watcher that never
/// read the pane's shell between the two would take it for the sandboxed agent's, and a restore
/// would resume that conversation outside the sandbox.
pub fn mark_sandbox(t: &Tmux, pane: &str, profile: &str, pid: u32) -> anyhow::Result<()> {
    mark_commands(pane, profile, pid).run(t)?;
    Ok(())
}

/// The commands of [`mark_sandbox`], as one command line.
fn mark_commands(pane: &str, profile: &str, pid: u32) -> Commands {
    let mut commands = Commands::default();
    for option in IDENTITY_OPTIONS {
        commands.unset(pane, option);
    }
    commands.set(pane, SANDBOX_OPTION, profile);
    commands.set(pane, SANDBOX_PID_OPTION, &pid.to_string());
    commands
}

/// Marks `pane` with the container runtime that has come to run its agent (`@agent_container`), and
/// takes off what the pane's last agent reported of itself ([`IDENTITY_OPTIONS`]: its session, and
/// the command it reported that resumes it), in one tmux invocation, only while the pane holds the
/// report stamp `seen` that the watcher's read saw ([`REPORT_STAMP_OPTION`]). Says whether it did.
///
/// Nothing in a container reports through tmux: what the pane holds when the container comes was
/// reported from outside, by the agent that ran there before, plain and of the same kind (`claude;
/// docker run ... claude`, a line typed ahead, a shell function that runs docker), whom the watcher, seeing no shell
/// between the two, takes the container's agent for. Kept, a save would keep that session and a
/// restore would resume it outside any container. A report since the read is decided on by the
/// next read, which marks the pane then.
pub fn mark_container(t: &Tmux, pane: &str, runtime: &str, seen: &str) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    for option in IDENTITY_OPTIONS {
        commands.unset(pane, option);
    }
    commands.set(pane, "@agent_container", runtime);
    run_unless_reported_since(t, pane, seen, None, &commands)
        .with_context(|| format!("marking pane '{pane}' with the container its agent runs in"))
}

/// Takes `pane`'s profile mark off, every option of it ([`SANDBOX_OPTIONS`]), and what the pane's
/// agent reported of itself with it ([`IDENTITY_OPTIONS`], as [`end_sandbox`] does), in one tmux
/// invocation: a start that did not happen. `taw sandbox` took the identity off as it marked the
/// pane, so all that can be there since is a session the watcher took from inside the sandbox
/// (`sandbox::report`) of a container `taw agent start` did not see come and go, which the watcher's
/// own end of the mark would have taken with it.
pub fn forget_sandbox(t: &Tmux, pane: &str) -> anyhow::Result<()> {
    forget_commands(pane).run(t)?;
    Ok(())
}

/// Takes `pane`'s profile mark off at one of its ends, every option of it ([`SANDBOX_OPTIONS`]), in one
/// tmux invocation, only while the pane still holds the mark `profile` with the process `pid` (its
/// text, empty for none) that the watcher's read found, and the report stamp `seen` that read saw
/// ([`REPORT_STAMP_OPTION`]): a `taw sandbox` typed since has marked the pane afresh, with a process
/// of its own, and a report since the read is decided on by the next. Says whether it was taken off.
///
/// What the pane's agent reported of itself ([`IDENTITY_OPTIONS`]: its session, what started it, its
/// word, the command that resumes it) goes in the same invocation. The watcher reports a
/// sandboxed agent's session from inside its sandbox (`sandbox::report`) into the record, and the
/// mark's end and the record's clear are decided apart: the mark on its process alone
/// (`sandbox::mark`), the record by the agent's state, which can keep it a read longer (an agent that
/// ended mid-turn: its exit's `idle` first) or through several misses (a program the shell runs after
/// the container). Kept past the mark, a save would hold that session plain, and a restore would
/// resume it outside the sandbox. The kind stays, and resumes nothing alone. The cost: a plain
/// agent's report from outside that lands after the container left and before the read that ends
/// the mark goes with it; one that lands after that read holds the end back, as the stamp says.
pub fn end_sandbox(
    t: &Tmux,
    pane: &str,
    profile: &str,
    pid: &str,
    seen: &str,
) -> anyhow::Result<bool> {
    run_unless_reported_since(
        t,
        pane,
        seen,
        Some(&held_sandbox(profile, pid)),
        &forget_commands(pane),
    )
    .with_context(|| format!("taking the profile mark off pane '{pane}'"))
}

/// Takes off `pane` what a restore marked it with around the `taw sandbox` line it typed, which did not
/// start: its profile mark, which has no process ([`SANDBOX_OPTIONS`]), and the agent's kind, name and
/// pid, the acquisition and what was saved of its session ([`IDENTITY_OPTIONS`]), in one tmux
/// invocation, only while the pane still holds that mark, `profile` with no process, and `shell` is the
/// command in front of it. A mark with no process ends only when a container that came leaves
/// (`sandbox::mark`), so a line refused before it marked the pane (its prompt check, a profile gone
/// since) would leave it for good; and a session kept without the profile would be resumed outside it
/// by the restore after the next save. A `taw sandbox` that has marked the pane with its own process
/// since, or a program in front, leaves it as it is. Says whether it was taken off.
pub fn forget_a_restored_start(
    t: &Tmux,
    pane: &str,
    profile: &str,
    shell: &str,
) -> anyhow::Result<bool> {
    let mut commands = Commands::default();
    for option in IDENTITY_OPTIONS.iter().chain(&[
        "@agent_kind",
        "@agent_kind_hint",
        "@agent_name",
        "@agent_pid",
        ACQUIRING_OPTION,
    ]) {
        commands.unset(pane, option);
    }
    for option in SANDBOX_OPTIONS {
        commands.unset(pane, option);
    }
    let held = format!(
        "#{{&&:{},#{{==:#{{pane_current_command}},{}}}}}",
        held_sandbox(profile, ""),
        format_literal(shell)
    );
    let out = t
        .run(&[
            "if-shell",
            "-F",
            "-t",
            pane,
            &held,
            &commands.line(),
            &format!("display-message -p {CHANGED}"),
        ])
        .with_context(|| format!("taking a restore's marks off pane '{pane}'"))?;
    Ok(out.trim() != CHANGED)
}

/// The tmux format that is true while the pane holds the profile mark `profile` and the process `pid`
/// with it (its text, empty for none).
fn held_sandbox(profile: &str, pid: &str) -> String {
    format!(
        "#{{&&:#{{==:#{{{SANDBOX_OPTION}}},{}}},#{{==:#{{{SANDBOX_PID_OPTION}}},{}}}}}",
        format_literal(profile),
        format_literal(pid)
    )
}

/// The commands of [`forget_sandbox`] and [`end_sandbox`], as one command line: the mark, and what
/// the agent reported of itself with it.
fn forget_commands(pane: &str) -> Commands {
    let mut commands = Commands::default();
    for option in SANDBOX_OPTIONS.iter().chain(&IDENTITY_OPTIONS) {
        commands.unset(pane, option);
    }
    commands
}

/// The clear of both, in one tmux invocation: a watcher that reads the pane between two calls can
/// write a kind or a state back, and tmux runs the commands of one invocation without any other
/// client's between them. The tokens are read first, since a name cannot be unset by a pattern. With a
/// report stamp `seen`, the clear is made only while the pane holds it ([`clear_agent`]).
fn clear(
    t: &Tmux,
    pane: &str,
    also: &[&str],
    leaves: bool,
    seen: Option<&str>,
) -> anyhow::Result<bool> {
    let out = t
        .run(&["show-options", "-p", "-t", pane])
        .with_context(|| format!("listing pane options for '{pane}' to clear tokens"))?;
    let tokens: Vec<String> = crate::tmux::parse_option_lines(&out)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with(TOKEN_PREFIX))
        .collect();
    let commands = clear_commands(pane, also, &tokens, leaves.then(now_millis));
    let Some(seen) = seen else {
        commands.run(t)?;
        return Ok(true);
    };
    run_unless_reported_since(t, pane, seen, None, &commands)
        .with_context(|| format!("clearing the agent record of pane '{pane}'"))
}

/// Runs `commands` in one tmux invocation only while `pane` holds the report stamp `seen`, and the format
/// `also` is true when one is given ([`unless_reported_since`]): whether they ran.
fn run_unless_reported_since(
    t: &Tmux,
    pane: &str,
    seen: &str,
    also: Option<&str>,
    commands: &Commands,
) -> anyhow::Result<bool> {
    let args = unless_reported_since(pane, seen, also, commands);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(t.run(&args)?.trim() != CHANGED)
}

/// The arguments of one invocation that runs `commands` only while `pane` holds the report stamp `seen`
/// ([`REPORT_STAMP_OPTION`]), and the format `also` is true when one is given, and prints [`CHANGED`] when
/// it does not.
fn unless_reported_since(
    pane: &str,
    seen: &str,
    also: Option<&str>,
    commands: &Commands,
) -> Vec<String> {
    let held = format!(
        "#{{==:#{{{REPORT_STAMP_OPTION}}},{}}}",
        format_literal(seen)
    );
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane.to_string(),
        match also {
            Some(also) => format!("#{{&&:{also},{held}}}"),
            None => held,
        },
        commands.line(),
        format!("display-message -p {CHANGED}"),
    ]
}

/// The commands that take [`CLEARED_ON_AGENT_EXIT`], the options in `also` and the `tokens` off
/// `pane`: when `left_at` says the agent left, at which millisecond, the status the pane holds is
/// recorded as the one it left with first ([`FINAL_OPTION`]), and then one `set-option -u` for each,
/// as one command line.
fn clear_commands(pane: &str, also: &[&str], tokens: &[String], left_at: Option<u64>) -> Commands {
    let mut commands = Commands::default();
    if let Some(now) = left_at {
        commands.add_final_from_state(pane, now);
    }
    for name in CLEARED_ON_AGENT_EXIT
        .iter()
        .copied()
        .chain(also.iter().copied())
        .chain(tokens.iter().map(String::as_str))
    {
        commands.unset(pane, name);
    }
    commands
}

/// Sets `@agent_seen`: `1` once the pane has been looked at since its agent last
/// completed, `0` from the start of a turn and on `done` (see
/// `watch::state::seen_written`).
pub fn set_seen(t: &Tmux, pane: &str, seen: bool) -> anyhow::Result<()> {
    t.set_pane_option(pane, "@agent_seen", if seen { "1" } else { "0" })
}

/// Replaces every control character in a free-text option value with a space.
///
/// The reading side already folds these server-side ([`crate::tmux::fold_controls`]), so
/// this is the second of two independent guards on the same hazard — and it is the one
/// that keeps the *stored* value honest. `@agent_display`, `@agent_name`, `@agent_title`
/// and `@agent_rule` are free text arriving from a hook script, a resumed snapshot or a
/// keystroke, and anything that renders them without going through a folded format — the
/// popup, a plugin payload, a notification body, `show-options` output — would otherwise
/// get a value with a line break in the middle of it.
///
/// Folded to a space rather than stripped: a tab already reads as a space in a terminal,
/// and folding leaves the value's word boundaries where its writer put them.
pub fn fold_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `set-option -p` for one of the four free-text `@agent_*` options, [`fold_text`] first.
/// Every write of those goes through here, so there is one rule for what can end up in
/// them rather than one per call site. The value is the last word of the command, written as
/// [`word`] writes one, so a value that ends in `;` (a label, a title, a token) is kept whole
/// and not cut there.
pub fn set_text_option(t: &Tmux, pane: &str, name: &str, value: &str) -> anyhow::Result<()> {
    t.set_pane_option(pane, name, &word(&fold_text(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The most a report can bring (an id of 128 bytes, a path of 4096 bytes that is all `~`, which
    /// quotes to two, a source of 32) beside the longest names a mark and a kind can have fits one tmux
    /// command with room to spare (about 9 of the 15 KiB), so a report taken from inside never comes to
    /// [`TooLong`]; the check itself still refuses what does not fit.
    #[test]
    fn a_report_at_its_bounds_fits_one_tmux_command_and_a_longer_mark_does_not() {
        let id = "a".repeat(128);
        let path = format!("/{}", "~".repeat(4095));
        let source = "s".repeat(32);
        let mark = "#".repeat(255);
        let session = Session {
            id: Some(&id),
            path: Some(&path),
        };
        let args = sandboxed_session_args(
            "%12345",
            "claude",
            &mark,
            "4294967295",
            &session,
            Some(&source),
        )
        .unwrap();
        let bytes: usize = args.iter().map(|word| word.len() + 1).sum();
        assert!(bytes < TMUX_COMMAND_ROOM * 2 / 3, "{bytes}");

        let long_mark = "#".repeat(TMUX_COMMAND_ROOM);
        assert!(sandboxed_session_args("%1", "claude", &long_mark, "", &session, None).is_err());
    }

    #[test]
    fn agent_state_round_trips_through_lowercase_strings() {
        for (state, text) in [
            (AgentState::Idle, "idle"),
            (AgentState::Working, "working"),
            (AgentState::Blocked, "blocked"),
            (AgentState::Done, "done"),
            (AgentState::Unknown, "unknown"),
        ] {
            assert_eq!(state.to_string(), text);
            assert_eq!(text.parse::<AgentState>().unwrap(), state);
        }
        assert!("bogus".parse::<AgentState>().is_err());
    }

    #[test]
    fn option_formats_is_in_the_documented_column_order() {
        let expected: &[&str] = &[
            "#{@agent_kind}",
            "#{@agent_state}",
            "#{@agent_state_since}",
            "#{@agent_rule}",
            "#{@agent_source}",
            "#{@agent_session_id}",
            "#{@agent_session_path}",
            "#{@agent_name}",
            "#{@agent_display}",
            "#{@agent_title}",
            "#{@agent_seen}",
            "#{@agent_kind_hint}",
            "#{@agent_watch_pid}",
            "#{@agent_reported_kind}",
            "#{@agent_container}",
            "#{@agent_sandbox}",
            "#{@agent_resume_source}",
            "#{@agent_resume_agent}",
            "#{@agent_resume_argv}",
            "#{@agent_restore_error}",
            "#{@agent_restore_saved}",
            "#{@agent_claude_flags}",
        ];
        assert_eq!(OPTION_FORMATS, expected);
    }

    #[test]
    fn parse_record_reads_fields_in_option_formats_order() {
        let fields = [
            "claude",
            "working",
            "1700000000",
            "live_turn_working",
            "manifest",
            "sess-abc",
            "/repo",
            "reviewer",
            "Reviewer",
            "claude: reviewer",
            "1",
            "manual-hint",
            "4242",
            "letta",
            "docker",
            "default",
            "integration:claude",
            "claude",
            "claude --resume sess-abc --model 'opus 4'",
            "Saved directory is unavailable.",
            "{\"cwd\":\"/gone\"}",
            "[\"--effort\",\"max\",\"--model\",\"x\"]",
        ];
        assert_eq!(fields.len(), OPTION_FORMATS.len());
        let record = parse_record(&fields);
        assert_eq!(record.kind.as_deref(), Some("claude"));
        assert_eq!(record.state, Some(AgentState::Working));
        assert_eq!(record.state_since, Some(1_700_000_000));
        assert_eq!(record.rule.as_deref(), Some("live_turn_working"));
        assert_eq!(record.source.as_deref(), Some("manifest"));
        assert_eq!(record.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(record.session_path.as_deref(), Some("/repo"));
        assert_eq!(record.name.as_deref(), Some("reviewer"));
        assert_eq!(record.display.as_deref(), Some("Reviewer"));
        assert_eq!(record.title.as_deref(), Some("claude: reviewer"));
        assert!(record.seen);
        assert_eq!(record.kind_hint.as_deref(), Some("manual-hint"));
        assert_eq!(record.watch_pid, Some(4242));
        assert_eq!(record.reported_kind.as_deref(), Some("letta"));
        assert_eq!(record.container.as_deref(), Some("docker"));
        assert_eq!(record.sandbox.as_deref(), Some("default"));
        assert!(record.tokens.is_empty());
        assert_eq!(
            record.resume,
            Some(crate::agent_resume::ReportedResume {
                source: "integration:claude".to_string(),
                agent: "claude".to_string(),
                argv: ["claude", "--resume", "sess-abc", "--model", "opus 4"]
                    .map(String::from)
                    .to_vec(),
            })
        );
        assert_eq!(
            record.restore_error.as_deref(),
            Some("Saved directory is unavailable.")
        );
        assert_eq!(record.restore_saved.as_deref(), Some("{\"cwd\":\"/gone\"}"));
        assert_eq!(record.claude_flags, ["--effort", "max", "--model", "x"]);
    }

    #[test]
    fn parse_record_treats_empty_fields_as_no_agent() {
        let fields = [""; 22];
        assert_eq!(fields.len(), OPTION_FORMATS.len());
        let record = parse_record(&fields);
        assert!(record.kind.is_none());
        assert!(record.state.is_none());
        assert!(record.state_since.is_none());
        assert!(record.rule.is_none());
        assert!(record.source.is_none());
        assert!(record.session_id.is_none());
        assert!(record.session_path.is_none());
        assert!(record.name.is_none());
        assert!(record.display.is_none());
        assert!(record.title.is_none());
        assert!(!record.seen);
        assert!(record.kind_hint.is_none());
        assert!(record.watch_pid.is_none());
        assert!(record.reported_kind.is_none());
        assert!(record.container.is_none());
        assert!(record.sandbox.is_none());
        assert!(record.resume.is_none());
        assert!(record.restore_error.is_none());
        assert!(record.restore_saved.is_none());
        assert!(record.claude_flags.is_empty());
    }

    #[test]
    fn an_option_that_holds_anything_but_the_listed_flags_reads_as_those_only() {
        let mut fields = vec![""; OPTION_FORMATS.len()];
        fields[21] = "not json";
        assert!(parse_record(&fields).claude_flags.is_empty());
        fields[21] = r#"["--resume","abc","--model","m"]"#;
        assert_eq!(parse_record(&fields).claude_flags, ["--model", "m"]);
        // A record read from a pane listing too short to have the column has none.
        assert!(parse_record(&fields[..21]).claude_flags.is_empty());
    }

    #[test]
    fn parse_record_ignores_an_unparseable_state_rather_than_panicking() {
        let mut fields = [""; 22];
        fields[1] = "sleeping"; // not a valid AgentState
        let record = parse_record(&fields);
        assert_eq!(record.state, None);
    }

    #[test]
    fn the_record_reads_the_runtime_and_the_profile_its_agent_runs_under() {
        let mut fields = vec![""; OPTION_FORMATS.len()];
        fields[0] = "claude";
        fields[14] = "docker";
        fields[15] = "default";
        let record = parse_record(&fields);
        assert_eq!(record.container.as_deref(), Some("docker"));
        assert_eq!(record.sandbox.as_deref(), Some("default"));
        assert_eq!(OPTION_FORMATS[14], "#{@agent_container}");
        assert_eq!(OPTION_FORMATS[15], "#{@agent_sandbox}");
    }

    /// docs/ARCHITECTURE.md's table of a pane's options has the columns of [`OPTION_FORMATS`] in their
    /// order, and the sentence under it says how many there are and which of its rows are not columns.
    #[test]
    fn the_architecture_table_has_the_records_columns_in_their_order() {
        let doc =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/ARCHITECTURE.md"))
                .unwrap();
        let section = doc
            .split("### Per pane (`set-option -pt <pane>`)")
            .nth(1)
            .and_then(|rest| rest.split("\n### ").next())
            .expect("the table of a pane's options");
        let named = |text: &str| -> Vec<String> {
            text.split('`')
                .skip(1)
                .step_by(2)
                .filter(|word| word.starts_with("@agent_"))
                .map(str::to_string)
                .collect()
        };
        let rows: Vec<String> = section
            .lines()
            .filter(|line| line.starts_with("| `@agent_"))
            .flat_map(|line| named(line).into_iter().take(1))
            .collect();
        let sentence = section.split_whitespace().collect::<Vec<_>>().join(" ");
        let (count, except) = sentence
            .split("the code and this table are the same ")
            .nth(1)
            .and_then(|rest| rest.split_once(" options, in the same order, except "))
            .expect("the sentence under the table");
        let except = named(
            except
                .split(", which are not columns of it")
                .next()
                .unwrap(),
        );

        assert_eq!(
            count,
            OPTION_FORMATS.len().to_string(),
            "the sentence under the table says {count} options"
        );
        assert!(
            except.iter().all(|option| rows.contains(option)),
            "{except:?} {rows:?}"
        );
        let columns: Vec<String> = rows
            .iter()
            .filter(|option| !except.contains(option))
            .map(|option| format!("#{{{option}}}"))
            .collect();
        assert_eq!(columns, OPTION_FORMATS);
    }

    /// Recording an agent in a pane ends what a restore kept of it because its saved directory was
    /// gone, with no question asked, in the tmux invocation that records it: every report of a state
    /// or a session, and `record_kind`, which the watcher and `taw agent start` record an agent with.
    /// The pane has an agent of its own, and is saved as it is from then on.
    #[test]
    fn recording_an_agent_in_a_pane_ends_what_a_restore_kept_of_it() {
        let commands = report_of(AgentState::Idle);
        for option in RESTORE_OPTIONS {
            let at = at(&commands, option);
            assert_eq!(
                commands[at - 5..at],
                ["set-option", "-u", "-p", "-t", "%3"],
                "{option}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\necho '{TAKEN}'\n",
                log.display()
            ),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let t = Tmux {
            bin,
            socket: Some(crate::tmux::Socket::Name("s".to_string())),
        };
        let calls = || -> Vec<String> {
            let calls = std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            std::fs::remove_file(&log).unwrap();
            calls
        };

        record_kind(&t, "%3", "claude").unwrap();
        assert_eq!(
            calls(),
            [
                "-L",
                "s",
                "-u",
                "set-option",
                "-p",
                "-t",
                "%3",
                "@agent_kind",
                "claude",
                ";",
                "set-option",
                "-u",
                "-p",
                "-t",
                "%3",
                RESTORE_ERROR_OPTION,
                ";",
                "set-option",
                "-u",
                "-p",
                "-t",
                "%3",
                RESTORE_SAVED_OPTION,
            ]
        );

        let order = Order {
            read: String::new(),
            next: String::new(),
        };
        let session = Session {
            id: Some("conv-new"),
            path: None,
        };
        write_session_report(&t, "%3", "claude", &session, None, None, &order, None).unwrap();
        let line = calls()
            .into_iter()
            .find(|word| word.contains("@agent_session_id"))
            .expect("the commands of the session report");
        for option in RESTORE_OPTIONS {
            assert!(
                line.contains(&format!("set-option -u -p -t %3 {option}")),
                "{option} is not taken off: {line}"
            );
        }
    }

    /// A restore marks a pane whose saved directory is gone with herdr's error and the pane as it was
    /// saved, in one tmux invocation; what is kept of it too long for one tmux command is refused before
    /// tmux is asked, and nothing is written. Neither is an option an agent's record is cleared of.
    #[test]
    fn a_restore_marks_a_pane_left_as_tmux_made_it_in_one_call_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display()),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let t = Tmux {
            bin,
            socket: Some(crate::tmux::Socket::Name("s".to_string())),
        };

        let too_long = "x".repeat(TMUX_COMMAND_ROOM);
        let err = mark_restore_error(&t, "%3", "why", Some(&too_long)).unwrap_err();
        assert!(err.chain().any(|cause| cause.is::<TooLong>()), "{err:#}");
        assert!(!log.exists(), "tmux was asked");

        mark_restore_error(&t, "%3", "why", Some("{\"cwd\":\"/gone\"}")).unwrap();
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            calls,
            [
                "-L",
                "s",
                "-u",
                "set-option",
                "-p",
                "-t",
                "%3",
                RESTORE_ERROR_OPTION,
                "why",
                ";",
                "set-option",
                "-p",
                "-t",
                "%3",
                RESTORE_SAVED_OPTION,
                "{\"cwd\":\"/gone\"}",
            ]
        );
        for option in [RESTORE_ERROR_OPTION, RESTORE_SAVED_OPTION] {
            assert!(!CLEARED_ON_AGENT_EXIT.contains(&option), "{option}");
            assert!(!IDENTITY_OPTIONS.contains(&option), "{option}");
        }
    }

    #[test]
    fn the_profile_mark_is_not_cleared_with_the_agents_record() {
        for option in SANDBOX_OPTIONS {
            assert!(!CLEARED_ON_AGENT_EXIT.contains(option), "{option}");
        }
        assert_eq!(
            SANDBOX_OPTIONS,
            [SANDBOX_OPTION, SANDBOX_PID_OPTION, SANDBOX_RESUME_OPTION]
        );
        assert!(CLEARED_ON_AGENT_EXIT.contains(&"@agent_container"));
    }

    #[test]
    fn a_format_literal_escapes_what_a_format_gives_a_meaning_to() {
        assert_eq!(format_literal("opencode"), "opencode");
        assert_eq!(format_literal("a:b,c}d#{e}"), "a:b#,c#}d##{e#}");
        assert_eq!(format_literal(""), "");
    }

    /// The flags a Claude was started with belong to that agent: they go when it leaves the pane
    /// (the watcher's clear) and when another agent takes it (the drops of what the last one reported).
    #[test]
    fn the_claude_flags_go_with_the_agent_that_left_and_with_the_one_that_was_replaced() {
        assert!(CLEARED_ON_AGENT_EXIT.contains(&crate::claude_flags::OPTION));
        assert!(IDENTITY_OPTIONS.contains(&crate::claude_flags::OPTION));
        assert!(OPTION_FORMATS.contains(&"#{@agent_claude_flags}"));
    }

    #[test]
    fn a_word_that_ends_in_a_semicolon_is_told_from_a_command_separator() {
        assert_eq!(word("opencode"), "opencode");
        assert_eq!(word("/tmp/a;"), "/tmp/a\\;");
        assert_eq!(word(";"), "\\;");
        assert_eq!(word("a;b"), "a;b");
    }

    #[test]
    fn commands_are_told_apart_by_a_separator_of_their_own_and_nothing_else() {
        let mut commands = Commands::default();
        commands.set("%3", "@agent_kind", "opencode");
        commands.unset("%3", "@agent_rule");
        commands.add(&["display-message", "-p", "-t", "%3", "#{@agent_watch_pid}"]);
        assert_eq!(
            commands.args(),
            [
                "set-option",
                "-p",
                "-t",
                "%3",
                "@agent_kind",
                "opencode",
                ";",
                "set-option",
                "-u",
                "-p",
                "-t",
                "%3",
                "@agent_rule",
                ";",
                "display-message",
                "-p",
                "-t",
                "%3",
                "#{@agent_watch_pid}",
            ]
        );
    }

    /// The drop is asked of tmux: the pane records an agent, and it is not this one. What it drops
    /// is what an agent's own hook leaves, its resume command included, and the kind it compares is
    /// escaped.
    #[test]
    fn the_drop_asks_whether_the_pane_records_another_agent_and_drops_what_the_last_one_reported() {
        let mut commands = Commands::default();
        commands.drop_identity_of_another_agent("%3", "a,b");
        assert_eq!(commands.args()[..4], ["if-shell", "-F", "-t", "%3"]);
        assert_eq!(
            commands.args()[4],
            "#{&&:#{!=:#{@agent_kind},},#{!=:#{@agent_kind},a#,b}}"
        );
        assert_eq!(
            commands.args()[5],
            "set-option -p -u -t %3 @agent_session_id ; set-option -p -u -t %3 \
             @agent_session_path ; set-option -p -u -t %3 @agent_token_session_start_source ; \
             set-option -p -u -t %3 @agent_reported_kind ; set-option -p -u -t %3 \
             @agent_resume_source ; set-option -p -u -t %3 @agent_resume_agent ; \
             set-option -p -u -t %3 @agent_resume_argv ; set-option -p -u -t %3 \
             @agent_claude_flags"
        );
        assert_eq!(commands.args().len(), 6);
    }

    #[test]
    fn the_mark_of_an_agent_being_taken_says_the_turn_stamp_it_was_taken_at() {
        // What taw writes, read back.
        for at in [
            Acquired::At(None),
            Acquired::At(Some("1790800000123456789".to_string())),
        ] {
            assert_eq!(Acquired::parse(&at.value()), Some(at.clone()), "{at:?}");
        }
        assert_eq!(Acquired::At(None).value(), "taken");
        assert_eq!(Acquired::At(Some("17".to_string())).value(), "taken 17");
        // Nothing is no mark: an unset option and an empty one read alike in a format.
        assert_eq!(Acquired::parse(""), None);
        assert_eq!(Acquired::parse("  "), None);
        // The bare `1` an earlier taw wrote, and what no taw writes, say the agent is being taken
        // and not since when; such a mark is written back as it was.
        for value in ["1", "takenx", "yes"] {
            assert_eq!(Acquired::parse(value), Some(Acquired::Unknown), "{value:?}");
        }
        // White space around a value is none of it.
        assert_eq!(Acquired::parse("taken "), Some(Acquired::At(None)));
        assert_eq!(Acquired::Unknown.value(), "1");
    }

    #[test]
    fn clearing_an_agent_is_one_command_line_that_unsets_every_option_it_owns() {
        let commands = clear_commands(
            "%3",
            &["@agent_pid"],
            &["@agent_token_session_start_source".to_string()],
            Some(1_700_000_000_000),
        );
        let args = commands.args();
        let line = &args[args.iter().position(|word| word == ";").unwrap() + 1..];
        // After the record of what the agent left with, one command for each option, told apart by
        // one separator each, and nothing but unsets.
        let options: Vec<&str> = CLEARED_ON_AGENT_EXIT
            .iter()
            .copied()
            .chain(["@agent_pid", "@agent_token_session_start_source"])
            .collect();
        assert_eq!(
            line.iter().filter(|word| *word == ";").count(),
            options.len() - 1
        );
        for option in &options {
            let at = line
                .iter()
                .position(|word| word == option)
                .unwrap_or_else(|| panic!("{option} is not unset: {line:?}"));
            assert_eq!(
                line[at - 5..at],
                ["set-option", "-u", "-p", "-t", "%3"],
                "{option}"
            );
        }
        // What outlives any one occupant stays.
        for kept in ["@agent_watch_pid", "@agent_kind_hint"] {
            assert!(!line.iter().any(|word| word == kept), "{kept} was cleared");
        }
    }

    fn report_of(state: AgentState) -> Vec<String> {
        report_commands(
            "%3",
            &Report {
                kind: "claude",
                state,
                source: "hook:x",
                session: Session::default(),
                now: 100,
                resume: None,
                reporter: "x",
            },
            "STAMP",
        )
        .args()
    }

    /// The position of the first word of `commands` that is `word`.
    fn at(commands: &[String], word: &str) -> usize {
        commands
            .iter()
            .position(|w| w == word)
            .unwrap_or_else(|| panic!("{word} is not in {commands:?}"))
    }

    #[test]
    fn a_report_of_a_wait_stamps_the_turn_only_when_the_pane_was_not_waiting_already() {
        let commands = report_of(AgentState::Blocked);
        let anew = at(&commands, "#{!=:#{@agent_state},blocked}");
        assert_eq!(commands[anew - 4..anew], ["if-shell", "-F", "-t", "%3"]);
        assert_eq!(commands[anew + 1], "set-option -p -t %3 @agent_turn STAMP");
        // The time moves on the same question, and both are asked before the state is written.
        let since = commands
            .iter()
            .rposition(|w| w == "#{!=:#{@agent_state},blocked}")
            .unwrap();
        assert_ne!(since, anew);
        assert_eq!(
            commands[since + 1],
            "set-option -p -t %3 @agent_state_since 100"
        );
        let written = at(&commands, "@agent_state");
        assert!(anew < written && since < written, "{commands:?}");
        // And nothing sets the time on its own.
        assert!(
            !commands
                .windows(2)
                .any(|pair| pair[0] == "@agent_state_since"),
            "{commands:?}"
        );
    }

    #[test]
    fn a_report_of_work_stamps_the_turn_whatever_the_pane_held() {
        let commands = report_of(AgentState::Working);
        let at_stamp = at(&commands, "@agent_turn");
        assert_eq!(
            commands[at_stamp - 4..at_stamp],
            ["set-option", "-p", "-t", "%3"]
        );
        assert_eq!(commands[at_stamp + 1], "STAMP");
        assert!(
            !commands.iter().any(|w| w.contains("@agent_turn STAMP")),
            "the stamp of work is not asked about: {commands:?}"
        );
    }

    #[test]
    fn a_report_of_rest_stamps_nothing() {
        for state in [AgentState::Idle, AgentState::Done, AgentState::Unknown] {
            let commands = report_of(state);
            assert!(
                !commands.iter().any(|w| w.contains("@agent_turn")),
                "{state}: {commands:?}"
            );
        }
    }

    /// What an agent leaves with is read from the pane by tmux as the record is taken off it, in the
    /// one command line, and first: a copy made after the unsets would copy nothing, and one made by a
    /// read of its own could copy what another writer had changed since.
    #[test]
    fn clearing_an_agent_first_records_the_status_it_left_with_in_the_same_command_line() {
        let commands = clear_commands("%3", &[], &[], Some(1_700_000_000_123));
        // Asked of tmux whether the pane holds a state at all: with none, nothing is left, and the
        // last agent's status stands.
        assert_eq!(
            commands.args()[..6],
            [
                "if-shell",
                "-F",
                "-t",
                "%3",
                "#{@agent_state}",
                "set-option -p -F -t %3 @agent_final_state '#{@agent_state} 1700000000123'"
            ]
        );
        assert_eq!(commands.args()[6], ";");
        assert_eq!(
            commands.args()[7..12],
            ["set-option", "-u", "-p", "-t", "%3"]
        );
        // And the record of it is not among what goes with the agent.
        assert!(!CLEARED_ON_AGENT_EXIT.contains(&FINAL_OPTION));
        assert!(
            !commands
                .args()
                .windows(6)
                .any(|w| w[..5] == ["set-option", "-u", "-p", "-t", "%3"] && w[5] == FINAL_OPTION)
        );
    }

    /// A clear that is not an agent leaving, `taw agent start`'s, takes the record off and records
    /// nothing of the status: the one the pane holds is the last agent's, and was recorded when it left.
    #[test]
    fn clearing_a_record_that_is_not_an_agent_leaving_records_no_status() {
        let commands = clear_commands("%3", &["@agent_pid"], &[], None);
        assert!(
            !commands
                .args()
                .iter()
                .any(|word| word.contains(FINAL_OPTION) || word == "if-shell"),
            "{:?}",
            commands.args()
        );
        // The record and the options asked for go all the same.
        assert_eq!(commands.args()[..5], ["set-option", "-u", "-p", "-t", "%3"]);
        assert!(commands.args().iter().any(|word| word == "@agent_pid"));
        assert!(commands.args().iter().any(|word| word == "@agent_kind"));
    }

    #[test]
    fn the_status_an_agent_left_with_is_read_back_as_the_status_and_the_millisecond() {
        assert_eq!(
            Final::parse("done 1700000000123"),
            Some(Final {
                state: AgentState::Done,
                at: 1_700_000_000_123
            })
        );
        assert_eq!(
            Final::parse(" idle 5 \n").map(|left| left.state),
            Some(AgentState::Idle)
        );
        // A pane that held no state when it was cleared left nothing to answer with.
        for text in [
            "",
            " 1700000000",
            "done",
            "done soon",
            "sleeping 5",
            "5 done",
        ] {
            assert_eq!(Final::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_token_that_says_what_started_a_session_is_one_of_the_options_that_go_with_its_agent() {
        assert!(
            IDENTITY_OPTIONS.contains(&format!("{TOKEN_PREFIX}{SESSION_START_TOKEN}").as_str())
        );
    }

    // Follows herdr's src/terminal/state.rs, `hook_report_is_newer`, whose rule
    // its tests `stale_hook_report_sequence_is_ignored_for_same_source`,
    // `unsequenced_hook_report_is_ignored_after_source_uses_sequence` and
    // `same_sequence_from_different_sources_is_independent` hold. Differences: the rule alone, over
    // the numbers a pane option keeps.
    #[test]
    fn a_report_is_newer_when_its_number_is_higher_than_its_sources_last_or_it_has_none_and_the_source_sent_none()
     {
        let seqs = ReportSeqs::parse("herdr:pi=20");
        assert!(seqs.newer("herdr:pi", Some(21)));
        assert!(!seqs.newer("herdr:pi", Some(20)), "the same number again");
        assert!(!seqs.newer("herdr:pi", Some(19)));
        assert!(
            !seqs.newer("herdr:pi", None),
            "no number from a source that sent one"
        );
        // Another source's numbers are its own.
        assert!(seqs.newer("custom:pi", Some(19)));
        assert!(seqs.newer("custom:pi", None));
        assert!(seqs.newer("custom:pi", Some(0)));
        // A pane that holds none takes anything.
        assert!(ReportSeqs::default().newer("herdr:pi", None));
        assert!(ReportSeqs::default().newer("herdr:pi", Some(1)));
        // Nanoseconds since the epoch, one apart, are two numbers.
        let stamp = ReportSeqs::parse("s=1790800000123456789");
        assert!(stamp.newer("s", Some(1_790_800_000_123_456_790)));
        assert!(!stamp.newer("s", Some(1_790_800_000_123_456_789)));
    }

    #[test]
    fn the_numbers_after_a_report_are_its_number_for_its_source_and_the_others_as_they_were() {
        let seqs = ReportSeqs::parse("a=1 b=2");
        assert_eq!(seqs.after("a", Some(5)).value(), "b=2 a=5");
        assert_eq!(seqs.after("c", Some(3)).value(), "a=1 b=2 c=3");
        assert_eq!(
            seqs.after("a", None),
            seqs,
            "a report with no number records one"
        );
        assert_eq!(ReportSeqs::default().value(), "");
        // Read back as it was written.
        let written = seqs.after("c", Some(u64::MAX));
        assert_eq!(ReportSeqs::parse(&written.value()), written);
        assert_eq!(written.last("c"), Some(u64::MAX));
        assert_eq!(written.last("d"), None);
    }

    #[test]
    fn the_numbers_of_the_last_sources_to_send_one_are_kept() {
        let mut seqs = ReportSeqs::default();
        for n in 0..=REPORT_SOURCES_KEPT {
            seqs = seqs.after(&format!("s{n}"), Some(n as u64));
        }
        assert_eq!(seqs.last("s0"), None, "the first to send one was kept");
        for n in 1..=REPORT_SOURCES_KEPT {
            assert_eq!(seqs.last(&format!("s{n}")), Some(n as u64));
        }
        // One that sends again is the last to have sent one.
        let again = seqs.after("s1", Some(100)).after("new", Some(1));
        assert_eq!(again.last("s1"), Some(100));
        assert_eq!(again.last("s2"), None);
    }

    /// Sources with long names fill the text sooner than the count: the oldest go until it fits, and
    /// the last to send one is kept whatever its length.
    #[test]
    fn the_numbers_kept_fit_in_their_budget_the_oldest_going_first() {
        let mut seqs = ReportSeqs::default();
        let name = |n: usize| format!("{n:02}{}", "\u{e9}".repeat(63));
        for n in 0..12 {
            seqs = seqs.after(&name(n), Some(n as u64));
            assert!(
                seqs.value().len() <= REPORT_SEQS_BUDGET,
                "{n}: {}",
                seqs.value().len()
            );
        }
        assert_eq!(seqs.last(&name(11)), Some(11));
        assert_eq!(seqs.last(&name(0)), None, "the oldest was kept");
        // About 383 bytes each: five fit.
        assert_eq!(
            (0..12).filter(|n| seqs.last(&name(*n)).is_some()).count(),
            5
        );
        let alone = ReportSeqs::default().after(&"x".repeat(5000), Some(1));
        assert_eq!(alone.last(&"x".repeat(5000)), Some(1));
    }

    /// A source is any text a reporter picks; written in the option it is one word, made of what neither
    /// a format nor a command line reads for a meaning, and it reads back as itself.
    #[test]
    fn a_source_of_any_name_is_kept_as_one_plain_word_and_read_back_as_itself() {
        assert_eq!(seq_source("integration:claude"), "integration:claude");
        for source in [
            "my agent",
            "a=b",
            "100%",
            "#{pane_id}",
            "a,b}c",
            "semi;colon",
            "quote\"'`",
            "$HOME ~",
            "tab\tand\nline",
            "caf\u{e9} \u{1f600}",
        ] {
            let written = seq_source(source);
            assert!(
                written
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '%')),
                "{source:?} was written as {written:?}"
            );
            assert_eq!(from_seq_source(&written).as_deref(), Some(source));
            let seqs = ReportSeqs::default().after(source, Some(7));
            assert_eq!(ReportSeqs::parse(&seqs.value()).last(source), Some(7));
        }
    }

    #[test]
    fn a_word_of_the_option_that_is_not_a_source_and_its_number_says_nothing() {
        let seqs = ReportSeqs::parse("junk a= =5 b=x %zz=3 %4=4 ok=5 ok=6 c=-1");
        assert_eq!(seqs.value(), "ok=6");
    }

    /// The write of a report is one `if-shell` that asks tmux whether the pane still holds the text of
    /// the order the report read, and writes the report, the next text and what it answers, or says that
    /// the pane changed.
    #[test]
    fn a_guarded_write_asks_for_what_the_report_read_and_sets_the_next_order_with_it() {
        let mut commands = Commands::default();
        commands.set("%3", "@agent_kind", "claude");
        let args = guarded(
            "%3",
            &Order {
                read: "a=1 b%2Cc=2".to_string(),
                next: "a=1 b%2Cc=3".to_string(),
            },
            None,
            commands,
            "#{@agent_watch_pid}",
        );
        assert_eq!(args[..4], ["if-shell", "-F", "-t", "%3"]);
        assert_eq!(args[4], "#{==:#{@agent_report_seq},a=1 b%2Cc=2}");
        assert_eq!(
            args[5],
            "set-option -p -t %3 @agent_kind claude ; \
             set-option -p -t %3 @agent_report_seq \"a=1 b%2Cc=3\" ; \
             display-message -p -t %3 \"taw-taken #{@agent_watch_pid}\""
        );
        assert_eq!(args[6], "display-message -p taw-changed");
        assert_eq!(args.len(), 7);
    }

    #[test]
    fn a_guarded_write_sets_no_order_that_does_not_change_and_unsets_one_that_goes() {
        let order = |read: &str, next: &str| Order {
            read: read.to_string(),
            next: next.to_string(),
        };
        let same = guarded("%3", &order("a=1", "a=1"), None, Commands::default(), "");
        assert!(!same[5].contains("@agent_report_seq"), "{}", same[5]);
        let gone = guarded("%3", &order("junk", ""), None, Commands::default(), "");
        assert!(
            gone[5].starts_with("set-option -u -p -t %3 @agent_report_seq ; "),
            "{}",
            gone[5]
        );
    }

    /// What taw does not write cannot be asked about as it was read (a line break at its end is lost
    /// on the way), so a write over it is made whatever it is, and the next one is guarded again.
    #[test]
    fn a_guarded_write_over_text_taw_does_not_write_is_made_whatever_it_holds() {
        for read in ["a=1\n", "a=1\tb=2", "#{x}", "\"quoted\""] {
            let args = guarded(
                "%3",
                &Order {
                    read: read.to_string(),
                    next: "a=2".to_string(),
                },
                None,
                Commands::default(),
                "",
            );
            assert_eq!(args[4], "1", "{read:?}");
        }
        // A condition of the caller's is still asked.
        let args = guarded(
            "%3",
            &Order {
                read: "a=1".to_string(),
                next: "a=2".to_string(),
            },
            Some("#{==:#{@agent_source},hook:a}"),
            Commands::default(),
            "",
        );
        assert_eq!(
            args[4],
            "#{&&:#{==:#{@agent_source},hook:a},#{==:#{@agent_report_seq},a=1}}"
        );
    }

    #[test]
    fn what_a_guarded_write_printed_says_whether_it_took_the_report() {
        assert_eq!(taken("taw-taken 4242").unwrap().as_deref(), Some("4242"));
        assert_eq!(taken("taw-taken \n").unwrap().as_deref(), Some(""));
        assert_eq!(taken("taw-changed\n").unwrap(), None);
        assert!(taken("").is_err());
        assert!(taken("something else").is_err());
    }

    /// A command line of `if-shell` writes a plain word as it is, so a log of the calls reads as the
    /// arguments did, and anything a command line reads for a meaning in double quotes, escaped.
    #[test]
    fn a_word_of_a_command_line_is_bare_when_it_is_plain_and_quoted_when_it_is_not() {
        for plain in [
            "set-option",
            "-p",
            "%3",
            "@agent_kind",
            "hook:integration:opencode",
            "/work/conv-1.json",
            "a+b",
        ] {
            assert_eq!(line_word(plain), plain);
        }
        for text in [
            "", "a b", "a;", "#{x}", "$HOME", "~", "a=b", "\"", "'", "x\ny",
        ] {
            assert_eq!(line_word(text), crate::tmux::command_word(text), "{text:?}");
        }
    }

    /// The watcher's clear of a record is one `if-shell` on the pane holding the report stamp its read
    /// saw, with the clear as its command line; a stamp it read empty is asked as empty.
    #[test]
    fn the_watchers_clear_lands_only_while_the_pane_holds_the_report_stamp_it_read() {
        let commands = clear_commands("%3", &[], &[], Some(5));
        let args = unless_reported_since("%3", "1790800000123456789", None, &commands);
        assert_eq!(args[..4], ["if-shell", "-F", "-t", "%3"]);
        assert_eq!(args[4], "#{==:#{@agent_report_stamp},1790800000123456789}");
        assert_eq!(args[5], commands.line());
        assert!(
            args[5].contains("set-option -u -p -t %3 @agent_kind ;"),
            "{}",
            args[5]
        );
        assert_eq!(args[6], "display-message -p taw-changed");
        assert_eq!(
            unless_reported_since("%3", "", None, &commands)[4],
            "#{==:#{@agent_report_stamp},}"
        );
        // A condition of the caller's own as well, both asked in the one `if-shell`.
        assert_eq!(
            unless_reported_since("%3", "17", Some("#{==:#{@agent_session_id},}"), &commands)[4],
            "#{&&:#{==:#{@agent_session_id},},#{==:#{@agent_report_stamp},17}}"
        );
    }

    /// Every report stamps the pane, a session report too, before the writes it guards.
    #[test]
    fn every_report_writes_its_stamp() {
        let commands = report_of(AgentState::Idle);
        let at = at(&commands, REPORT_STAMP_OPTION);
        assert_eq!(commands[at - 4..at], ["set-option", "-p", "-t", "%3"]);
        assert_eq!(commands[at + 1], "STAMP");
        assert!(CLEARED_ON_AGENT_EXIT.contains(&REPORT_STAMP_OPTION));
    }

    fn resume_of(source: &str, agent: &str, argv: &[&str]) -> crate::agent_resume::ReportedResume {
        crate::agent_resume::ReportedResume {
            source: source.to_string(),
            agent: agent.to_string(),
            argv: argv.iter().map(|word| word.to_string()).collect(),
        }
    }

    /// A report takes the pane for its source and agent: a command another source or agent reported
    /// goes, asked of tmux, and the report's own is set after that, the three options at once.
    #[test]
    fn a_report_drops_another_reporters_command_and_sets_its_own() {
        let resume = resume_of("a,b", "prime", &["prime", "--resume", "x y"]);
        let commands = report_commands(
            "%3",
            &Report {
                kind: "prime",
                state: AgentState::Idle,
                source: "hook:a,b",
                session: Session::default(),
                now: 1,
                resume: Some(&resume),
                reporter: "a,b",
            },
            "STAMP",
        )
        .args();
        let drop = at(
            &commands,
            "#{||:#{!=:#{@agent_resume_source},a#,b},#{!=:#{@agent_resume_agent},prime}}",
        );
        assert_eq!(commands[drop - 4..drop], ["if-shell", "-F", "-t", "%3"]);
        assert_eq!(
            commands[drop + 1],
            "set-option -p -u -t %3 @agent_resume_source ; set-option -p -u -t %3 \
             @agent_resume_agent ; set-option -p -u -t %3 @agent_resume_argv"
        );
        let set = at(&commands, "@agent_resume_argv");
        assert!(drop < set, "{commands:?}");
        assert_eq!(commands[set + 1], "prime --resume 'x y'");
        assert_eq!(commands[at(&commands, "@agent_resume_source") + 1], "a,b");
        assert_eq!(commands[at(&commands, "@agent_resume_agent") + 1], "prime");

        // A report that carries none drops another's and sets nothing.
        let none = report_of(AgentState::Idle);
        assert!(
            none.iter()
                .any(|word| word.contains("@agent_resume_source},x"))
        );
        assert!(!none.iter().any(|word| word == "@agent_resume_argv"));
    }

    /// Whether a reporter holds a pane, as tmux is asked it, and the guard each answer writes under.
    #[test]
    fn who_holds_a_pane_is_asked_of_tmux_and_guards_the_write_that_stands_on_it() {
        assert_eq!(
            held_by("a,b", "x}"),
            "#{&&:#{==:#{@agent_source},hook:a#,b},#{==:#{@agent_kind},x#}}}"
        );
        assert_eq!(Holding::Holds.still("s", "a"), held_by("s", "a"));
        assert_eq!(
            Holding::AnotherHook.still("s", "a"),
            format!("#{{&&:{HOOKED},#{{==:{},0}}}}", held_by("s", "a"))
        );
        assert_eq!(
            Holding::NoHook(true).still("s", "a"),
            format!("#{{==:{HOOKED},0}}")
        );
        assert_eq!(
            Holding::NoHook(false).still("s", "a"),
            Holding::NoHook(true).still("s", "a")
        );
        assert!(Holding::Holds.accepts());
        assert!(Holding::NoHook(true).accepts());
        assert!(!Holding::NoHook(false).accepts());
        assert!(!Holding::AnotherHook.accepts());
    }

    #[test]
    fn the_numbers_a_source_sent_go_with_the_agent() {
        assert!(CLEARED_ON_AGENT_EXIT.contains(&REPORT_SEQ_OPTION));
        assert!(!IDENTITY_OPTIONS.contains(&REPORT_SEQ_OPTION));
    }

    /// `taw sandbox` marks its pane in one command line: what the pane's last agent reported of its
    /// session goes, the command it reported that resumes it among it, and the profile and the pid
    /// of the process about to become it are set. The start is typed at the pane's prompt, so that
    /// session is never the sandboxed agent's, and a restore must not resume it outside the sandbox.
    #[test]
    fn the_sandbox_mark_takes_the_last_agents_session_off_and_sets_the_profile_and_the_pid() {
        let commands = mark_commands("%3", "box", 4242);

        let mut expected: Vec<String> = Vec::new();
        for option in IDENTITY_OPTIONS {
            expected.extend(["set-option", "-u", "-p", "-t", "%3", option, ";"].map(String::from));
        }
        expected.extend(
            [
                "set-option",
                "-p",
                "-t",
                "%3",
                SANDBOX_OPTION,
                "box",
                ";",
                "set-option",
                "-p",
                "-t",
                "%3",
                SANDBOX_PID_OPTION,
                "4242",
            ]
            .map(String::from),
        );
        assert_eq!(commands.args(), expected);
        // What a restore kept of the pane stays: a save takes its directory from it, and no session
        // while the pane has the container mark.
        for option in RESTORE_OPTIONS {
            assert!(
                !commands.args().iter().any(|word| word == option),
                "{option}"
            );
        }
        for option in [
            "@agent_session_id",
            "@agent_session_path",
            "@agent_token_session_start_source",
            "@agent_reported_kind",
            "@agent_resume_source",
            "@agent_resume_agent",
            "@agent_resume_argv",
        ] {
            assert!(
                commands.args().iter().any(|word| word == option),
                "{option}"
            );
        }
    }

    /// What `taw agent start --sandbox` keeps beside the mark is kept only while the pane holds a mark
    /// with a process, the one its typed `taw sandbox` set: a mark with no process is a restore's,
    /// and its kept resume is the restore's own. tmux decides it in the one command, so the words
    /// handed to it name both options.
    #[test]
    fn a_start_keeps_its_session_only_beside_a_mark_with_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls");
        let bin = dir.path().join("tmux");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n", log.display()),
        )
        .unwrap();
        crate::test_exec::make_executable(&bin);
        let t = Tmux {
            bin,
            socket: Some(crate::tmux::Socket::Name("s".to_string())),
        };

        assert!(keep_beside_the_mark(&t, "%3", "{\"kind\":\"codex\"}").unwrap());

        let calls = std::fs::read_to_string(&log).unwrap();
        let words: Vec<&str> = calls.lines().collect();
        let at = words
            .iter()
            .position(|word| *word == "if-shell")
            .unwrap_or_else(|| panic!("{calls}"));
        assert_eq!(words[at + 1], "-F", "{calls}");
        assert_eq!(
            words[at + 4],
            format!("#{{&&:#{{{SANDBOX_OPTION}}},#{{{SANDBOX_PID_OPTION}}}}}"),
            "{calls}"
        );
        assert!(words[at + 5].contains(SANDBOX_RESUME_OPTION), "{calls}");
    }

    /// A start that did not happen takes every option of the mark off, in one command line, and
    /// nothing a restore kept of the pane.
    #[test]
    fn forgetting_the_sandbox_mark_takes_every_option_of_it_off_and_the_agents_identity() {
        let commands = forget_commands("%3");
        for option in RESTORE_OPTIONS {
            assert!(
                !commands.args().iter().any(|word| word == option),
                "{option}"
            );
        }

        let mut expected: Vec<String> = Vec::new();
        for (n, option) in SANDBOX_OPTIONS.iter().chain(&IDENTITY_OPTIONS).enumerate() {
            if n > 0 {
                expected.push(";".to_string());
            }
            expected.extend(["set-option", "-u", "-p", "-t", "%3", option].map(String::from));
        }
        assert_eq!(commands.args(), expected);
    }
}
