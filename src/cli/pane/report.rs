// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! `taw pane report-agent | report-agent-session | release-agent | report-metadata` —
//! the verbs an integration hook or a plugin uses to tell `taw` what an agent is doing.
//! Each writes the `@agent_*` pane options; nothing here
//! computes state, it only records what the caller reported.

use clap::ValueEnum;
use serde_json::{Value, json};

use crate::cli::common::{self, parse_kv};
use crate::cli::envelope::CliError;
use crate::cli::{Ctx, Sub};
use crate::store::{self, AgentState};

/// herdr's `report-agent --state`. `done` is absent on purpose: it is not a state an
/// integration can report, it is the watcher's name for a completion nobody was looking
/// at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReportState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl ReportState {
    fn state(self) -> AgentState {
        match self {
            ReportState::Idle => AgentState::Idle,
            ReportState::Working => AgentState::Working,
            ReportState::Blocked => AgentState::Blocked,
            ReportState::Unknown => AgentState::Unknown,
        }
    }
}

/// `--source <id>` is recorded as `@agent_source = hook:<id>`, which the watcher treats
/// as authoritative until [`release_agent`] clears it: hook-reported states are
/// authoritative until released.
fn hook_source(source: &str) -> String {
    format!("hook:{source}")
}

/// How many times a report is decided again, when another report has landed between its read of the
/// order of reports and its write, before it gives up.
const ORDER_ATTEMPTS: usize = 20;

/// The most bytes a `--source` may hold. It is kept in the pane's options, and among the last sources'
/// numbers, each byte that is not a plain one written as three ([`store::ReportSeqs`]), which a report
/// writes in one tmux command with the rest of what it writes.
const SOURCE_BYTES: usize = 128;

/// A `--source` that can be kept: at most [`SOURCE_BYTES`] bytes, or `invalid_argument` before anything
/// of the report is written.
fn checked_source(source: &str) -> anyhow::Result<()> {
    if source.len() > SOURCE_BYTES {
        return Err(CliError::invalid_argument(format!(
            "--source may hold at most {SOURCE_BYTES} bytes, and this one holds {}",
            source.len()
        )));
    }
    Ok(())
}

// Follows herdr's src/terminal/state.rs, `is_unsequenced_opencode_selection`.
// Differences: taw's source for OpenCode's plugins, `integration:opencode`, where herdr's is
// `herdr:opencode`.
/// Whether a report is OpenCode's TUI saying which session the pane shows, which it sends with no
/// `--seq` while its other reports have one: herdr takes it whatever the source sent before, and records
/// no number for it.
fn unsequenced_selection(
    source: &str,
    agent: &str,
    session_start_source: Option<&str>,
    seq: Option<u64>,
) -> bool {
    (source, agent, session_start_source, seq)
        == ("integration:opencode", "opencode", Some("select"), None)
}

/// The order a report of `source` numbered `seq` would be taken in ([`store::Order`]), read from the
/// pane: `None` when it is not newer than what the source sent before, and is to be ignored (herdr's
/// `hook_report_is_newer`). `always` takes it whatever was sent before, and records nothing of it.
fn order_of(
    t: &crate::tmux::Tmux,
    pane_id: &str,
    source: &str,
    seq: Option<u64>,
    always: bool,
) -> anyhow::Result<Option<store::Order>> {
    let read = store::read_report_seqs(t, pane_id).map_err(|e| CliError::tmux(format!("{e:#}")))?;
    if always {
        return Ok(Some(store::Order {
            next: read.clone(),
            read,
        }));
    }
    let seqs = store::ReportSeqs::parse(&read);
    if !seqs.newer(source, seq) {
        log::debug!(
            "ignoring a report from {source:?} numbered {seq:?} for {pane_id}: it last sent {:?}",
            seqs.last(source)
        );
        return Ok(None);
    }
    Ok(Some(store::Order {
        next: seqs.after(source, seq).value(),
        read,
    }))
}

/// Takes a report of `source` numbered `seq` in the order of its source's reports: `write` makes it
/// over the [`store::Order`] read from the pane, and when another report has landed in between, the
/// report is decided again over what that one left. `Ok(None)` when it is not newer than what the
/// source sent before, and nothing is written.
fn in_order<T>(
    t: &crate::tmux::Tmux,
    pane_id: &str,
    source: &str,
    seq: Option<u64>,
    always: bool,
    mut write: impl FnMut(&store::Order) -> anyhow::Result<store::Written<T>>,
) -> anyhow::Result<Option<T>> {
    for _ in 0..ORDER_ATTEMPTS {
        let Some(order) = order_of(t, pane_id, source, seq, always)? else {
            return Ok(None);
        };
        if let store::Written::Taken(taken) = write(&order)? {
            return Ok(Some(taken));
        }
    }
    Err(CliError::tmux(format!(
        "{} of pane '{pane_id}' changed under {ORDER_ATTEMPTS} attempts to write the report",
        store::REPORT_SEQ_OPTION
    )))
}

/// Takes `@agent_pid` off a pane whose recorded pid is dead, before a report is written for it, when the
/// report comes from the job in front of the pane. Such a report is what a live agent says, so the pane's
/// pid is not the reporter's: it is the pid `taw agent start` (or a restore) recorded for an agent that
/// has since exited, and it outlives that agent along with the pane's hint. Left on the pane, it said
/// that an agent the user started by hand afterwards, and whose hook is reporting, had gone: a wait for
/// it ended with `agent_not_running` two seconds in, and the verbs that type refused it. A pid that is
/// alive, or none, is left as it is.
///
/// A report from anywhere else leaves the dead pid too. The hook of the agent that died can report after
/// it: it was the agent's child, and launchd or init has taken it in. With its pid taken off, the pane
/// of an agent only its hint names would say nothing of the exit any more, and the watcher read the dead
/// agent's last frame for good (`working`, when it died mid-turn). So the reporter has to descend from
/// the job in front of the pane's terminal ([`crate::watch::caller_descends_from_the_job_in_front_of`]),
/// which the hook of a live agent does and an orphaned one does not.
///
/// What is read, found dead and unset are three steps, and another writer can land between them: `taw
/// agent start` recording the pid of the agent it has just started would be taken off for the dead
/// one. The unset is one tmux invocation that asks, in tmux, whether the record still holds the pid that
/// was found dead ([`unset_while_it_holds`]).
fn forget_a_dead_pid(t: &crate::tmux::Tmux, pane_id: &str) {
    let Ok(read) = t.display(pane_id, "#{@agent_pid}\t#{pane_pid}") else {
        return;
    };
    let (recorded, pane_pid) = read.split_once('\t').unwrap_or((read.as_str(), ""));
    let Ok(pid) = recorded.trim().parse::<u32>() else {
        return;
    };
    if crate::watch::process_alive(pid) {
        return;
    }
    let Ok(pane_pid) = pane_pid.trim().parse::<u32>() else {
        return;
    };
    if !crate::watch::caller_descends_from_the_job_in_front_of(pane_pid) {
        log::debug!(
            "leaving the dead pid {pid} on {pane_id}: the report does not come from the job in front of it"
        );
        return;
    }
    let unset = unset_while_it_holds(pane_id, pid);
    let args: Vec<&str> = unset.iter().map(String::as_str).collect();
    if let Err(err) = t.run_ok(&args) {
        log::debug!("taking the dead pid {pid} off {pane_id}: {err:#}");
    }
}

/// The tmux command that unsets `@agent_pid` of `pane_id` only while it holds `pid`: `if-shell -F` on the
/// record being that pid, with the unset as its command, so that the question and the unset are one
/// step of one invocation.
fn unset_while_it_holds(pane_id: &str, pid: u32) -> Vec<String> {
    vec![
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        format!("#{{==:#{{@agent_pid}},{pid}}}"),
        format!(
            "set-option -p -u -t {} @agent_pid",
            crate::tmux::command_word(pane_id)
        ),
    ]
}

/// `pane report-agent`: the reported lifecycle state, its source, and optionally the
/// session identity that came with it.
///
/// `@agent_rule` is cleared: a hook-reported state has no manifest rule behind it, and
/// leaving the last manifest rule attached would misattribute it.
///
/// An agent other than the one the pane records takes the pane: what the last one reported of
/// itself (its session id and path, its word that it was there) goes, so a state-only report
/// from the new agent leaves none of the old agent's session behind it.
///
/// The pane's watcher announces the change this reports (the status event, the `agent.*` event,
/// the toast): it is nudged once the state is written, so it reads the pane though the pane prints
/// nothing. The watcher is the one that does that; this stays a short-lived writer.
///
/// A report is taken in the order of its source's reports, by `--seq` (`seq`), as herdr takes them: one
/// whose number is not higher than the last one the source sent, or with no number from a source that
/// has sent one, is ignored, and the reply says so (`in_order`). The check and the write are one tmux
/// call ([`in_order`]).
///
/// `resume` is the command after `--` that resumes the agent's session (herdr 0.9.2): checked before
/// anything is written ([`checked_resume`]), and kept with the report, which takes the pane for its
/// source and agent, so its reporter holds the pane and may set it. A command another source or agent
/// reported goes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn report_agent(
    ctx: &Ctx,
    pane: &str,
    source: &str,
    agent: &str,
    state: ReportState,
    seq: Option<u64>,
    session_id: Option<&str>,
    session_path: Option<&str>,
    resume: Option<&[String]>,
) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    // Before the first write, not beside the write it guards: rejecting the call after
    // `@agent_kind` and the state had already landed left the pane half-updated behind an
    // `invalid_argument` — a caller told its report failed, with part of it applied.
    checked_source(source)?;
    let resume = checked_resume(source, agent, resume)?;
    let session = checked_session(session_id, session_path)?;
    forget_a_dead_pid(&ctx.tmux, &pane_id);

    // The agent, its state, its owner and its session in one call: a watcher that read between
    // separate calls would see an agent that nobody owns (see `store::write_report`).
    let report = store::Report {
        kind: agent,
        state: state.state(),
        source: &hook_source(source),
        session,
        now: common::now_unix(),
        resume: resume.as_ref(),
        reporter: source,
    };
    let taken = in_order(&ctx.tmux, &pane_id, source, seq, false, |order| {
        store::write_report(&ctx.tmux, &pane_id, &report, order).map_err(|e| {
            write_failed(
                e,
                report.resume.is_some(),
                format!("writing agent state for pane '{pane}'"),
            )
        })
    })?;
    // The pane may print nothing, and the watcher reads it when it prints, or on its recheck: it is
    // told that the state is there to be read.
    if let Some(Some(pid)) = taken {
        crate::watch::nudge::send(pid);
    }

    Ok(json!({
        "pane_id": crate::tmux::ids::pane_id(&session_of(ctx, &pane_id)?, &pane_id),
        "agent_status": state.state().as_str(),
        "in_order": taken.is_some(),
    }))
}

/// `pane report-agent-session`: session identity only. Deliberately does **not** touch
/// `@agent_state` — herdr's `SessionStart` hook fires before the agent has done anything,
/// and overwriting a live state with a guess would fight the watcher.
///
/// It does record that the agent itself said what the pane holds (`@agent_reported_kind`).
/// The watcher keeps an agent so reported, when no process in front of the pane has named
/// it, until the pane's own shell is idle, as herdr keeps an agent it cannot identify: no
/// command in front of it is a reason to forget it. That keeps the session id of an agent
/// whose command taw cannot read (a Letta installed under `node`) for `session restore`. One
/// a process does name (a Claude) follows the ordinary exit and miss rules instead.
///
/// The kind, the word that it was the agent talking and the session are written in one tmux
/// call, so a watcher never reads the agent without the word that keeps it, or without the
/// session that came with it; and, as for `report-agent`, another agent than the one the pane
/// records takes away what that one reported of itself.
///
/// It is taken in the order of its source's reports, as `report-agent` is, except OpenCode's TUI
/// saying which session the pane shows, which carries no number ([`unsequenced_selection`]).
///
/// `resume` is the command after `--` that resumes the session, checked before anything is written. A
/// session report does not take the pane, so only a reporter that holds it may set the command
/// ([`holding`]); from any other, the rest of the report is applied and the verb answers
/// `resume_not_accepted`, as herdr's does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn report_agent_session(
    ctx: &Ctx,
    pane: &str,
    source: &str,
    agent: &str,
    seq: Option<u64>,
    session_id: Option<&str>,
    session_path: Option<&str>,
    session_start_source: Option<&str>,
    resume: Option<&[String]>,
) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    checked_source(source)?;
    let resume = checked_resume(source, agent, resume)?;
    let session = checked_session(session_id, session_path)?;
    forget_a_dead_pid(&ctx.tmux, &pane_id);
    let flags = (agent == "claude")
        .then(|| claude_flags_in_front(&ctx.tmux, &pane_id))
        .flatten();
    let always = unsequenced_selection(source, agent, session_start_source, seq);
    let taken = in_order(&ctx.tmux, &pane_id, source, seq, always, |order| {
        let offer = match &resume {
            Some(resume) => Some(store::Offer {
                resume,
                holding: holding(&ctx.tmux, &pane_id, source, agent)?,
            }),
            None => None,
        };
        store::write_session_report(
            &ctx.tmux,
            &pane_id,
            agent,
            &session,
            session_start_source,
            flags.as_deref(),
            order,
            offer.as_ref(),
        )
        .map_err(|e| {
            write_failed(
                e,
                resume.is_some(),
                format!("writing the session of pane '{pane}'"),
            )
        })
    })?;
    if resume.is_some() && taken == Some(false) {
        return Err(CliError::resume_not_accepted(format!(
            "the resume command of {source:?} was not kept: it does not hold pane '{pane}' for \
             {agent:?}; report the agent's state with `taw pane report-agent` first"
        )));
    }
    Ok(json!({
        "pane_id": crate::tmux::ids::pane_id(&session_of(ctx, &pane_id)?, &pane_id),
        "agent_session_id": session_id,
        "in_order": taken.is_some(),
    }))
}

// Follows herdr's src/terminal/state.rs, `can_record_reported_resume`.
// Differences: the pane's hook authority is its `@agent_source` and `@agent_kind`, asked of tmux;
// herdr's agent detected by its process is the agent the process in front of the pane names, as the
// verbs that type ask it (`detect::identify`, `detect::job::identify`).
/// Whether the reporter `source` holds `pane_id` for `agent`, and may set the command that resumes the
/// agent's session there: its `hook:<source>` is the pane's source, for that agent; or no hook holds the
/// pane, and the agent in front of it, named by its process, is `agent`. A late report of an agent that
/// has exited finds its shell in front, which names no agent.
fn holding(
    t: &crate::tmux::Tmux,
    pane_id: &str,
    source: &str,
    agent: &str,
) -> anyhow::Result<store::Holding> {
    let (held, hooked) = store::read_holding(t, pane_id, source, agent)
        .map_err(|e| CliError::tmux(format!("{e:#}")))?;
    Ok(if held {
        store::Holding::Holds
    } else if hooked {
        store::Holding::AnotherHook
    } else {
        store::Holding::NoHook(agent_in_front(t, pane_id).as_deref() == Some(agent))
    })
}

/// The flags the pane's Claude was started with that set how its session runs
/// ([`crate::claude_flags`]), read from the exact arguments of the Claude process in the job in front of
/// the pane: `Some` of the list (empty to clear what the pane holds) when the process was found and its
/// arguments were read, `None` when they were not (the job could not be read, Claude is not in it, or its
/// arguments could not be read), which leaves what the pane holds.
/// Only the listed flags leave this function; nothing of the arguments is logged.
fn claude_flags_in_front(t: &crate::tmux::Tmux, pane_id: &str) -> Option<Vec<String>> {
    let pane_pid = t.display(pane_id, "#{pane_pid}").ok()?;
    let job = crate::watch::foreground_job(pane_pid.trim().parse().ok()?)?;
    flags_of_job(&job)
}

/// [`claude_flags_in_front`] for a job that was read.
fn flags_of_job(job: &crate::detect::job::Job) -> Option<Vec<String>> {
    let process = crate::detect::job::claude_process(job)?;
    // Arguments that could not be read say nothing of the flags, so they must not clear the pane's.
    let argv = process.argv.as_deref()?;
    Some(crate::claude_flags::kept(Some(argv)))
}

/// The agent the process in front of `pane_id` is, by the name tmux gives its command or by the
/// processes of its foreground job, their names and arguments: `None` for a shell, any other program,
/// or what cannot be read.
fn agent_in_front(t: &crate::tmux::Tmux, pane_id: &str) -> Option<String> {
    let read = t
        .display(pane_id, "#{pane_current_command}\t#{pane_pid}")
        .ok()?;
    let (command, pane_pid) = read.split_once('\t')?;
    if let Some(named) = crate::detect::identify(command, None) {
        return Some(named);
    }
    let job = crate::watch::foreground_job(pane_pid.trim().parse().ok()?)?;
    crate::detect::job::identify(&job)
}

/// What a write of a report that failed answers: `invalid_resume_argv` for one that carries a resume
/// command and is too long for one tmux command (8 KiB of quotes and backslashes is twice that once
/// quoted for tmux), with nothing of it written; `tmux_error` for anything else, as tmux itself refuses
/// a command too long.
fn write_failed(err: anyhow::Error, resume: bool, what: String) -> anyhow::Error {
    match err.downcast_ref::<store::TooLong>() {
        Some(too_long) if resume => CliError::invalid_resume_argv(format!(
            "the resume command is too long to keep with the report: {too_long}"
        )),
        _ => CliError::tmux(format!("{what}: {err:#}")),
    }
}

/// The resume command a report carries after `--`, checked before anything of the report is written:
/// one that cannot be kept answers `invalid_resume_argv`, and none of the report is applied.
fn checked_resume(
    source: &str,
    agent: &str,
    argv: Option<&[String]>,
) -> anyhow::Result<Option<crate::agent_resume::ReportedResume>> {
    let Some(argv) = argv else {
        return Ok(None);
    };
    crate::agent_resume::validate_argv(argv).map_err(CliError::invalid_resume_argv)?;
    Ok(Some(crate::agent_resume::ReportedResume {
        source: source.to_string(),
        agent: agent.to_string(),
        argv: argv.to_vec(),
    }))
}

/// `pane release-agent`: gives up hook authority over a pane by clearing
/// `@agent_source`, so the watcher's manifest evaluation takes over again.
///
/// Only clears when the pane's source is *this* caller's `hook:<source>`. A hook must not
/// be able to release an authority it never held (another integration's, or the
/// watcher's own `manifest`) — that would silently hand the pane to whoever asked last.
/// `released` reports which happened.
///
/// A release is a report like the others, taken in the order of its source's reports by `--seq`, as
/// herdr takes it: one out of order is not made (`in_order` false). Whether the source holds the pane,
/// and the order, are asked of tmux in the call that clears it. The command the source reported for
/// `agent` that resumes its session goes with the release.
pub(crate) fn release_agent(
    ctx: &Ctx,
    pane: &str,
    source: &str,
    agent: &str,
    seq: Option<u64>,
) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    checked_source(source)?;
    let expected = hook_source(source);
    for _ in 0..ORDER_ATTEMPTS {
        let current = store::read(&ctx.tmux, &pane_id).map_err(|e| {
            CliError::tmux(format!("reading agent options for pane '{pane}': {e:#}"))
        })?;
        let order = order_of(&ctx.tmux, &pane_id, source, seq, false)?;
        let (Some(order), true) = (&order, current.source.as_deref() == Some(expected.as_str()))
        else {
            return Ok(json!({
                "released": false,
                "source": current.source,
                "in_order": order.is_some(),
            }));
        };
        let released = store::release_source(&ctx.tmux, &pane_id, source, agent, order)
            .map_err(|e| CliError::tmux(format!("clearing @agent_source: {e:#}")))?;
        if released == store::Written::Taken(()) {
            return Ok(json!({
                "released": true,
                "source": current.source,
                "in_order": true,
            }));
        }
    }
    Err(CliError::tmux(format!(
        "{} of pane '{pane}' changed under {ORDER_ATTEMPTS} attempts to release it",
        store::REPORT_SEQ_OPTION
    )))
}

/// `pane report-metadata`: display-only overrides. Every flag is independent, and each
/// `--clear-*` form removes the corresponding option.
///
/// `--seq`, `--ttl-ms` and `--applies-to-source` are accepted for herdr parity and
/// ignored: tmux options have no expiry and no per-source layering, so a later report
/// simply replaces an earlier one.
pub struct Metadata<'a> {
    pub title: Option<&'a str>,
    pub clear_title: bool,
    pub display_agent: Option<&'a str>,
    pub clear_display_agent: bool,
    pub state_labels: &'a [String],
    pub clear_state_labels: bool,
    pub tokens: &'a [String],
    pub clear_tokens: &'a [String],
}

/// `@agent_token_state_label_<status>` holds a `--state-label` override. There is
/// no dedicated option for these, so they live in the token namespace,
/// where plugins and the TUI already look.
const STATE_LABEL_PREFIX: &str = "state_label_";

pub(crate) fn report_metadata(ctx: &Ctx, pane: &str, meta: Metadata<'_>) -> anyhow::Result<Value> {
    let pane_id = common::resolve_pane(&ctx.tmux, pane)?;
    let t = &ctx.tmux;
    let mut changed: Vec<String> = Vec::new();

    if meta.clear_title {
        t.unset_pane_option(&pane_id, "@agent_title")
            .map_err(|e| CliError::tmux(format!("clearing @agent_title: {e:#}")))?;
        changed.push("title".into());
    } else if let Some(title) = meta.title {
        store::set_text_option(t, &pane_id, "@agent_title", title)
            .map_err(|e| CliError::tmux(format!("setting @agent_title: {e:#}")))?;
        changed.push("title".into());
    }

    if meta.clear_display_agent {
        t.unset_pane_option(&pane_id, "@agent_display")
            .map_err(|e| CliError::tmux(format!("clearing @agent_display: {e:#}")))?;
        changed.push("display_agent".into());
    } else if let Some(display) = meta.display_agent {
        store::set_text_option(t, &pane_id, "@agent_display", display)
            .map_err(|e| CliError::tmux(format!("setting @agent_display: {e:#}")))?;
        changed.push("display_agent".into());
    }

    if meta.clear_state_labels {
        for (key, _) in store::read(t, &pane_id)
            .map_err(|e| CliError::tmux(format!("reading tokens: {e:#}")))?
            .tokens
            .iter()
            .filter(|(k, _)| k.starts_with(STATE_LABEL_PREFIX))
        {
            t.unset_pane_option(&pane_id, &format!("@agent_token_{key}"))
                .map_err(|e| CliError::tmux(format!("clearing state label '{key}': {e:#}")))?;
        }
        changed.push("state_labels".into());
    }
    for label in meta.state_labels {
        let (status, text) = parse_kv(label)?;
        // Only the five real states can be relabelled; a typo would otherwise sit in the
        // token namespace forever, silently doing nothing.
        status.parse::<AgentState>().map_err(|_| {
            CliError::invalid_argument(format!(
                "--state-label '{label}': '{status}' is not an agent state"
            ))
        })?;
        set_token(
            ctx,
            &pane_id,
            &format!("{STATE_LABEL_PREFIX}{status}"),
            &text,
        )?;
        changed.push(format!("state_label:{status}"));
    }

    for token in meta.tokens {
        let (key, value) = parse_kv(token)?;
        validate_token_key(&key)?;
        set_token(ctx, &pane_id, &key, &value)?;
        changed.push(format!("token:{key}"));
    }
    for key in meta.clear_tokens {
        validate_token_key(key)?;
        t.unset_pane_option(&pane_id, &format!("@agent_token_{key}"))
            .map_err(|e| CliError::tmux(format!("clearing token '{key}': {e:#}")))?;
        changed.push(format!("token:{key}"));
    }

    // The pane record changed, which is what herdr's `pane.updated` means. Emitted only
    // when something actually changed, so a no-op call is not an event.
    if !changed.is_empty()
        && let Ok(pane) = common::pane_info(t, &pane_id)
    {
        crate::cli::events::pane_updated(ctx, &pane);
    }

    Ok(json!({ "changed": changed }))
}

/// A token key becomes part of a tmux option name (`@agent_token_<key>`), so it must be
/// a plain identifier — anything else would either be rejected by tmux or, worse, land
/// under a different option than intended.
fn validate_token_key(key: &str) -> anyhow::Result<()> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(CliError::invalid_argument(format!(
            "invalid token name '{key}': use letters, digits, '_' or '-'"
        )))
    }
}

/// Writes one `@agent_token_<key>`. The value is folded like every other free-text
/// `@agent_*` option: tokens are whatever a hook script passed to `--token k=v`, and they
/// are read back out of `show-options -p`, whose output is line-oriented — a newline in a
/// value would split one option into two unparseable lines.
fn set_token(ctx: &Ctx, pane_id: &str, key: &str, value: &str) -> anyhow::Result<()> {
    store::set_text_option(&ctx.tmux, pane_id, &format!("@agent_token_{key}"), value)
        .map_err(|e| CliError::tmux(format!("setting token '{key}': {e:#}")))
}

/// Rejects an `@agent_session_id` that cannot be stored, before anything is written.
///
/// The value ends up interpolated into the command `session restore` types into a pane
/// with `send-keys … Enter`, so an id carrying `;` or a newline would execute whatever
/// follows it — at a restore that may be days away, with nothing left to say where it came
/// from. See `session::restore::valid_session_id`.
fn check_session_id(session_id: Option<&str>) -> anyhow::Result<()> {
    match session_id {
        Some(id) if !crate::session::restore::valid_session_id(id) => {
            Err(CliError::invalid_argument(format!(
                "invalid agent session id {id:?}: must match [A-Za-z0-9._:-]{{1,128}}"
            )))
        }
        _ => Ok(()),
    }
}

/// The session a report carries, with its id checked before anything is written. Both verbs get
/// the session they write from here, so a verb cannot write an id that was not checked. A part the
/// report did not carry is `None`, and the writer leaves what is on the pane for it alone rather
/// than clearing it: a hook that reports a state without repeating the session identity must not
/// lose it.
fn checked_session<'a>(
    session_id: Option<&'a str>,
    session_path: Option<&'a str>,
) -> anyhow::Result<store::Session<'a>> {
    check_session_id(session_id)?;
    Ok(store::Session {
        id: session_id,
        path: session_path,
    })
}

/// The tmux session id owning `pane_id`, for building the public pane id in a reply.
fn session_of(ctx: &Ctx, pane_id: &str) -> anyhow::Result<String> {
    Ok(common::pane_info(&ctx.tmux, pane_id)?.session_id)
}

/// The envelope sub-name for each report verb, so `cli/pane/mod.rs` need not repeat them.
pub const REPORT_AGENT: Sub = "report-agent";
pub const REPORT_AGENT_SESSION: Sub = "report-agent-session";
pub const RELEASE_AGENT: Sub = "release-agent";
pub const REPORT_METADATA: Sub = "report-metadata";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::envelope::code_for;

    /// The unset asks tmux, in the invocation that makes it, whether the record still holds the pid that
    /// was found dead.
    #[test]
    fn the_unset_of_a_dead_pid_is_one_command_that_asks_whether_the_record_still_holds_it() {
        assert_eq!(
            unset_while_it_holds("%7", 4242),
            [
                "if-shell",
                "-F",
                "-t",
                "%7",
                "#{==:#{@agent_pid},4242}",
                "set-option -p -u -t \"%7\" @agent_pid",
            ]
        );
    }

    /// Only OpenCode's TUI choosing the pane's session, with no number, is taken whatever its source
    /// sent before; herdr's `herdr:opencode` is taw's `integration:opencode`.
    #[test]
    fn only_opencodes_unnumbered_choice_of_a_session_skips_the_order_of_reports() {
        let source = "integration:opencode";
        assert!(unsequenced_selection(
            source,
            "opencode",
            Some("select"),
            None
        ));
        assert!(!unsequenced_selection(
            source,
            "opencode",
            Some("select"),
            Some(5)
        ));
        assert!(!unsequenced_selection(
            source,
            "opencode",
            Some("resume"),
            None
        ));
        assert!(!unsequenced_selection(source, "opencode", None, None));
        assert!(!unsequenced_selection(
            "integration:kilo",
            "opencode",
            Some("select"),
            None
        ));
        assert!(!unsequenced_selection(source, "kilo", Some("select"), None));
        assert!(!unsequenced_selection(
            "herdr:opencode",
            "opencode",
            Some("select"),
            None
        ));
    }

    #[test]
    fn a_source_of_at_most_128_bytes_is_kept_and_a_longer_one_refused() {
        assert!(checked_source(&"s".repeat(128)).is_ok());
        assert!(checked_source(&"\u{e9}".repeat(64)).is_ok());
        let err = checked_source(&"s".repeat(129)).unwrap_err();
        assert_eq!(crate::cli::envelope::code_for(&err), "invalid_argument");
        assert_eq!(
            err.to_string(),
            "--source may hold at most 128 bytes, and this one holds 129"
        );
    }

    fn process(
        pid: u32,
        name: &str,
        argv: Option<&[&str]>,
        line: Option<&str>,
    ) -> crate::detect::job::Process {
        crate::detect::job::Process {
            pid,
            name: name.to_string(),
            argv: argv.map(|argv| argv.iter().map(|arg| arg.to_string()).collect()),
            line: line.map(str::to_string),
        }
    }

    fn job(group: u32, processes: Vec<crate::detect::job::Process>) -> crate::detect::job::Job {
        crate::detect::job::Job { group, processes }
    }

    /// A session report keeps the listed flags of the Claude process in front of the pane, and says
    /// "nothing found" or "arguments not read" (`None`, which leaves the pane's flags) apart from
    /// "found, none" (`Some` of an empty list, which clears them).
    #[test]
    fn the_flags_come_from_the_claude_process_of_the_job_and_nothing_found_is_not_none_kept() {
        let claude = job(
            10,
            vec![
                process(11, "cat", Some(&["cat"]), None),
                process(
                    10,
                    "claude",
                    Some(&["claude", "--effort=max", "-p", "a prompt", "--model", "x"]),
                    None,
                ),
            ],
        );
        assert_eq!(
            flags_of_job(&claude),
            Some(
                ["--effort", "max", "--model", "x"]
                    .map(String::from)
                    .to_vec()
            )
        );
        // Found, with arguments that name none of them: an empty list, which clears the option.
        let plain = job(10, vec![process(10, "claude", Some(&["claude"]), None)]);
        assert_eq!(flags_of_job(&plain), Some(Vec::new()));
        // Found, with only the line `ps` printed, or with no arguments at all (they could not be read):
        // nothing is known of the flags, so the pane keeps what it holds.
        let printed = job(
            10,
            vec![process(10, "claude", None, Some("claude --effort max"))],
        );
        assert_eq!(flags_of_job(&printed), None);
        let unread = job(10, vec![process(10, "claude", None, None)]);
        assert_eq!(flags_of_job(&unread), None);
        // Claude is not in the job (a shell, another agent, a container runtime): nothing found.
        assert_eq!(
            flags_of_job(&job(10, vec![process(10, "bash", Some(&["bash"]), None)])),
            None
        );
        let codex = job(
            10,
            vec![process(10, "codex", Some(&["codex", "--model", "x"]), None)],
        );
        assert_eq!(flags_of_job(&codex), None);
        let docker = job(
            10,
            vec![process(
                10,
                "docker",
                Some(&["docker", "run", "-it", "img", "claude", "--model", "x"]),
                None,
            )],
        );
        assert_eq!(flags_of_job(&docker), None);
    }

    #[test]
    fn a_reported_source_becomes_a_hook_authority() {
        assert_eq!(hook_source("test"), "hook:test");
        assert_eq!(hook_source("claude-hooks"), "hook:claude-hooks");
    }

    #[test]
    fn report_states_map_onto_the_store_states_and_exclude_done() {
        assert_eq!(ReportState::Idle.state(), AgentState::Idle);
        assert_eq!(ReportState::Working.state(), AgentState::Working);
        assert_eq!(ReportState::Blocked.state(), AgentState::Blocked);
        assert_eq!(ReportState::Unknown.state(), AgentState::Unknown);
        // `done` is the watcher's promotion, never a reportable state; the clap enum has
        // no variant for it, which this asserts by construction.
        assert_eq!(
            ReportState::value_variants().len(),
            4,
            "report-agent must accept exactly herdr's four states"
        );
    }

    #[test]
    fn accepts_plain_token_names() {
        for key in ["cost", "pr_number", "branch-name", "a1"] {
            assert!(validate_token_key(key).is_ok(), "rejected {key:?}");
        }
    }

    #[test]
    fn rejects_token_names_that_would_not_survive_a_tmux_option_name() {
        for key in ["", "has space", "has.dot", "has/slash", "@at"] {
            let err = validate_token_key(key).unwrap_err();
            assert_eq!(code_for(&err), "invalid_argument", "accepted {key:?}");
        }
    }
}
