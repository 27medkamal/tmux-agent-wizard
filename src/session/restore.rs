// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.
//! Recreating a saved session snapshot onto a live tmux server, resuming agents into their
//! native conversations where possible.

use std::collections::HashSet;

use crate::config::Config;
use crate::paths::Paths;
use crate::tmux::Tmux;

use super::history::{self, History};
use super::prompt;
use super::snapshot::{self, PaneSnap, SessionSnap};

/// The outcome of [`apply`]: which saved sessions were recreated, which already existed
/// and were left untouched, which panes had their agent resumed, which panes were meant to
/// and had nothing typed into them (their shell never came to its prompt, or the pane was
/// closed first), and (always filled in, but the only field filled in on a `dry_run`) the
/// human-readable plan.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The snapshot this restore read, and the tmux server's start it was judged against. `None`
    /// only in a report that was made by hand.
    pub snapshot: Option<Used>,
    pub created: Vec<String>,
    /// The windows a restore made again in a session that was open, each as
    /// `window <index> '<name>' of '<session>'` (the saved window's own index: tmux gives the new one
    /// the next free index). Empty on a `--dry-run`, as `created` is.
    pub added: Vec<String>,
    /// Saved sessions that are open and had nothing to add to them.
    pub skipped: Vec<String>,
    /// Whether this restore started the tmux server, there being none to restore into.
    pub started_server: bool,
    /// How to attach to the first session a restore made when it started the server.
    pub attach: Option<String>,
    pub resumed: Vec<String>,
    pub not_resumed: Vec<String>,
    pub plan: Vec<String>,
    /// On a `--dry-run`, the line each pane that would be resumed would be typed, in the order of the
    /// plan. Empty in a restore that makes anything: what it typed is in the panes.
    pub would_type: Vec<WouldType>,
}

/// One line a dry run would type: the pane, by the session's name and the window's and pane's saved
/// numbers (no pane exists yet), and the exact line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WouldType {
    pub session: String,
    pub window: u32,
    pub pane: u32,
    pub line: String,
}

/// The snapshot a restore read: its file (`<name>.json`, or a copy in `session-snapshots/`), the time
/// its layout was taken (`taken_unix`, as the file says), and when the tmux server started (`None`
/// when there was no server to ask).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Used {
    pub file: std::path::PathBuf,
    pub taken_unix: u64,
    pub server_start: Option<u64>,
}

/// What a pane that tmux did not open in its saved directory carries (`@agent_restore_error`):
/// herdr's `restore_error` for a pane whose saved directory is gone (`src/app/agent_resume.rs` and
/// `src/persist/restore.rs` at v0.9.3), word for word.
const UNAVAILABLE: &str =
    "Saved directory is unavailable. Restore the directory and restart this session.";

/// Recreates every session in the saved snapshot `name` that is missing from the live
/// server (`created`), and, in a saved session that is open, each window that holds an agent
/// whose conversation runs nowhere in the server (`added`, at the end of that session; nothing
/// already open is changed). `skipped` lists the open sessions that had nothing to add. With no
/// tmux server running, the first session is made by starting one (`started_server`). On
/// `dry_run`, only `Report::plan` is filled in and nothing is executed.
///
/// Every session is made before any agent is resumed, and each agent's command is typed only
/// once its pane's shell is at its prompt (see [`prompt`]): a shell that is still starting
/// would hand what is typed to whatever reads the terminal first. A restore that fails on a
/// session types what it has planned for the sessions it made, and then gives the error back.
///
/// Autosave is held off while it runs ([`super::RESTORING_OPTION`]), and one save is made when
/// it is done, if one was asked for.
pub fn apply(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    name: &str,
    dry_run: bool,
) -> anyhow::Result<Report> {
    apply_with(t, cfg, paths, name, dry_run, &prompt::Timing::default())
}

/// [`apply`] with the waits for the panes' prompts run on `wait` instead of the defaults.
pub fn apply_with(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    name: &str,
    dry_run: bool,
    wait: &prompt::Timing,
) -> anyhow::Result<Report> {
    let snap = snapshot::load(paths, name)?;
    let open = Open::read(t)?;
    let used = Used {
        file: snapshot::snapshot_path(paths, name),
        taken_unix: snap.taken_unix,
        server_start: t.server_start_unix(),
    };
    apply_snapshot(t, cfg, paths, name, snap, used, open, None, dry_run, wait)
}

/// A restore with no `--name`: the layout `taw session save` and the autosave keep as `default`
/// (`default.json`), or, when it has nothing to make (every session open, and no window of an
/// open session to add), the newest layout of `default`
/// taken before this tmux server started ([`snapshot::look_back`]): a new server's first autosave
/// replaces `default.json` with its own layout, and the layout of the server that is gone is
/// then only in the copies. That one layout and no older one: when it has nothing to make there
/// is nothing to restore, and a restore never goes further back. A server whose start is not known
/// (there is none running, or it did not answer) restores `default.json` alone. `--name` never comes
/// here: it restores exactly the file it names.
///
/// The report names the snapshot it used, and its plan begins with a line that says which, with
/// both times.
pub fn apply_default(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    dry_run: bool,
) -> anyhow::Result<Report> {
    apply_default_with(t, cfg, paths, dry_run, &prompt::Timing::default())
}

/// [`apply_default`] with the waits for the panes' prompts run on `wait` instead of the defaults.
pub fn apply_default_with(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    dry_run: bool,
    wait: &prompt::Timing,
) -> anyhow::Result<Report> {
    let name = super::AUTOSAVE_SNAPSHOT_NAME;
    let open = Open::read(t)?;
    let start = t.server_start_unix();
    let taw = taw_word(t);
    let plan = |saved: &PaneSnap| to_resume(cfg, paths, &taw, saved);
    let (snap, used) = plain_source(paths, name, start, &open, &plan)?;
    let heading = saved_line(
        dry_run,
        has_a_session_to_make(&snap, &open, &plan),
        used.taken_unix,
        start,
        super::unix_now(),
        local_offset,
    );
    apply_snapshot(
        t,
        cfg,
        paths,
        name,
        snap,
        used,
        open,
        Some(heading),
        dry_run,
        wait,
    )
}

/// What a restore would resume in `saved` ([`plan_with`]), nothing at all with
/// `resume_agents_on_restore` off: the planning that decides which windows of an open session are added.
/// Only whether there is a plan, and its key and session, are used, so no transcript is read for the effort.
fn to_resume(
    cfg: &Config,
    paths: &Paths,
    taw: &str,
    saved: &PaneSnap,
) -> Result<Option<Planned>, String> {
    if cfg.session.resume_agents_on_restore {
        plan_with(cfg, paths, taw, saved, Read::No)
    } else {
        Ok(None)
    }
}

/// How a typed `taw sandbox` line names taw ([`crate::sandbox::typed::taw_for`]).
fn taw_word(t: &Tmux) -> String {
    crate::sandbox::typed::taw_for(
        t,
        None,
        &std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("taw")),
    )
}

/// What a restore finds on the server: the open sessions, and the agents running in any pane of it.
/// With no server running (`no_server`), nothing is open and nothing runs.
struct Open {
    sessions: HashSet<String>,
    running: Vec<Running>,
    /// For each open session, the number the next window made at its end gets (`new-window -a` after
    /// the last one): one more than its highest.
    next_window: std::collections::HashMap<String, u32>,
    no_server: bool,
}

/// An agent conversation that runs in an open pane: what a restore would resume it by ([`running_of`]),
/// and the pane.
struct Running {
    kind: String,
    session_id: Option<String>,
    session_path: Option<String>,
    /// The key the restore resumes it by ([`Planned::key`]): the kind with the session id or path, or, for
    /// an agent that reported its own resume command, that command. `None` for a pane the restore would
    /// resume nothing in.
    key: Option<String>,
    pane: String,
}

/// The conversation a pane holds, as `saved` (the pane as a save takes it, [`snapshot::saved_pane`]: the
/// kept resume of a sandboxed pane and the agent a restore kept in a pane it left alone included) says, by
/// the one planning a restore resumes by ([`plan_outside`]): its key, whichever way the pane is resumed. A
/// pane named by its hint alone (a restore typed the command and the watcher has not yet identified the
/// agent) counts as that agent: the kind the hint is another name of (`agent start --kind cursor-agent`
/// leaves `cursor-agent`, which is Cursor), or the hint as it is when no bundled manifest knows it. `None`
/// for a pane that holds no agent.
fn running_of(saved: &PaneSnap, pane: String) -> Option<Running> {
    let kind = saved.agent_kind.clone().or_else(|| {
        saved
            .agent_kind_hint
            .as_deref()
            .map(|hint| crate::detect::identify("", Some(hint)).unwrap_or_else(|| hint.to_string()))
    })?;
    let named = PaneSnap {
        agent_kind: Some(kind.clone()),
        ..saved.clone()
    };
    // Only the key is wanted, so no transcript is read for the effort.
    let key = plan_outside(&named, Read::No).ok().flatten().map(|p| p.key);
    Some(Running {
        kind,
        session_id: saved.agent_session_id.clone(),
        session_path: saved.agent_session_path.clone(),
        key,
        pane,
    })
}

impl Open {
    /// Asks the server. tmux saying there is no server is not an error: a restore then starts one.
    fn read(t: &Tmux) -> anyhow::Result<Open> {
        let sessions = match t.list_sessions() {
            Ok(sessions) => sessions
                .into_iter()
                .map(|s| s.session_name)
                .collect::<HashSet<_>>(),
            Err(err) if crate::tmux::no_server(&err) => {
                return Ok(Open {
                    sessions: HashSet::new(),
                    running: Vec::new(),
                    next_window: std::collections::HashMap::new(),
                    no_server: true,
                });
            }
            Err(err) => return Err(err),
        };
        let panes = t.list_panes()?;
        let mut next_window: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for pane in &panes {
            let next = next_window.entry(pane.session_name.clone()).or_insert(0);
            *next = (*next).max(pane.window_index.saturating_add(1));
        }
        let running = panes
            .iter()
            .filter_map(|pane| running_of(&snapshot::saved_pane(t, pane), pane.pane_id.clone()))
            .collect();
        Ok(Open {
            sessions,
            running,
            next_window,
            no_server: false,
        })
    }

    /// The pane that already runs the conversation `planned` would resume, if one does: an open pane
    /// that a restore would resume by the same key, or of the same agent kind that holds the same session
    /// id (or, for an agent resumed by its session file, the same path).
    fn runs(&self, planned: &Planned) -> Option<&str> {
        runs_in(&self.running, planned)
    }
}

fn runs_in<'a>(running: &'a [Running], planned: &Planned) -> Option<&'a str> {
    running
        .iter()
        .find(|r| {
            r.key.as_deref() == Some(planned.key.as_str())
                || (r.kind == planned.kind
                    && ((planned.session_id.is_some() && planned.session_id == r.session_id)
                        || (planned.session_path.is_some()
                            && planned.session_path == r.session_path)))
        })
        .map(|r| r.pane.as_str())
}

/// For each session of `snap`, the positions (in `session.windows`) of the windows a restore adds to
/// it, in order; empty for a session that is not open, which is made whole, and for an open one with
/// nothing to add.
///
/// A window of an open session is added when it holds an agent pane that a restore would resume (the
/// planning of [`plan_with`]: a saved agent with nothing to resume from does not count, and nothing
/// is resumed at all with `resume_agents_on_restore` off, which `plan` says by planning nothing) and whose conversation runs in no pane of
/// the server. Each conversation is resumed once ([`Restore::claim`]): one that a session made whole,
/// or a window added before this one, has already takes a later window out. Plain shell windows are
/// never added.
fn windows_to_add(
    snap: &snapshot::Snapshot,
    open: &Open,
    plan: &dyn Fn(&PaneSnap) -> Result<Option<Planned>, String>,
) -> Vec<Vec<usize>> {
    let mut taken: HashSet<String> = HashSet::new();
    snap.sessions
        .iter()
        .map(|session| {
            let is_open = open.sessions.contains(&session.name);
            let mut add = Vec::new();
            for (position, window) in session.windows.iter().enumerate() {
                let mut missing = false;
                for pane in &window.panes {
                    let Ok(Some(planned)) = plan(pane) else {
                        continue;
                    };
                    let first = taken.insert(planned.key.clone());
                    if is_open && first && open.runs(&planned).is_none() {
                        missing = true;
                    }
                }
                if is_open && missing {
                    add.push(position);
                }
            }
            add
        })
        .collect()
}

/// Whether `snap` has something a restore would make: a session that is not open, or a window of an
/// open one that [`windows_to_add`] adds.
fn has_a_session_to_make(
    snap: &snapshot::Snapshot,
    open: &Open,
    plan: &dyn Fn(&PaneSnap) -> Result<Option<Planned>, String>,
) -> bool {
    snap.sessions
        .iter()
        .any(|s| !open.sessions.contains(&s.name))
        || windows_to_add(snap, open, plan)
            .iter()
            .any(|add| !add.is_empty())
}

/// The layout a plain restore reads, and where it came from: `default.json` while it has something
/// to make; otherwise, when the server's start is known, the newest layout taken before it
/// when that has something to make; otherwise `default.json` again, with nothing to make, which says
/// so. With no `default.json` at all, the look-back alone, or the error a missing file gives.
fn plain_source(
    paths: &Paths,
    name: &str,
    start: Option<u64>,
    open: &Open,
    plan: &dyn Fn(&PaneSnap) -> Result<Option<Planned>, String>,
) -> anyhow::Result<(snapshot::Snapshot, Used)> {
    let used = |file: std::path::PathBuf, snap: &snapshot::Snapshot| Used {
        file,
        taken_unix: snap.taken_unix,
        server_start: start,
    };
    let default = snapshot::load(paths, name);
    let absent = default.as_ref().is_err_and(|err| {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        })
    });
    let default_is_enough = default
        .as_ref()
        .is_ok_and(|snap| has_a_session_to_make(snap, open, plan));
    if let Some(start) = start
        && !default_is_enough
        && (default.is_ok() || absent)
        && let Some(found) = snapshot::look_back(paths, name, start)
        && let Ok(snap) = snapshot::load_file(&found.path, name)
        && (absent || has_a_session_to_make(&snap, open, plan))
    {
        let used = used(found.path, &snap);
        return Ok((snap, used));
    }
    let snap = default?;
    let file = snapshot::snapshot_path(paths, name);
    let used = used(file, &snap);
    Ok((snap, used))
}

/// The first line of a plain restore's plan: which layout it restores (or would, or finds
/// nothing to restore in), with the times it was taken and the server started. `offset` gives the
/// local time's distance from UTC, in seconds, at a unix time.
fn saved_line(
    dry_run: bool,
    something_to_make: bool,
    taken_unix: u64,
    server_start: Option<u64>,
    now: u64,
    offset: impl Fn(u64) -> i64,
) -> String {
    let at = |unix: u64, relative_to: u64| clock_text(unix, offset(unix), relative_to, &offset);
    let taken = at(taken_unix, server_start.unwrap_or(now));
    let when = match server_start {
        Some(start) if taken_unix < start => {
            format!(
                "saved at {taken}, before this tmux server started at {}",
                at(start, now)
            )
        }
        Some(start) => format!(
            "saved at {taken}, after this tmux server started at {}",
            at(start, now)
        ),
        None => format!("saved at {taken}"),
    };
    if !something_to_make {
        let join = if server_start.is_some() { ", " } else { " " };
        return format!("nothing to restore: the layout {when}{join}has every session open");
    }
    let verb = if dry_run { "would restore" } else { "restored" };
    format!("{verb} the layout {when}")
}

/// `HH:MM` for a unix time, with the date (`YYYY-MM-DD HH:MM`) when it is not on the day of
/// `relative_to`, both read at their own local offsets (`offset_of`).
fn clock_text(unix: u64, offset: i64, relative_to: u64, offset_of: &impl Fn(u64) -> i64) -> String {
    let (date, hour, minute) = civil(unix, offset);
    let (other, _, _) = civil(relative_to, offset_of(relative_to));
    if date == other {
        format!("{hour:02}:{minute:02}")
    } else {
        format!("{} {hour:02}:{minute:02}", date_text(date))
    }
}

/// The calendar day, hour and minute of a unix time at `offset` seconds from UTC. The day is
/// `(year, month, day)` (Howard Hinnant's `civil_from_days`).
fn civil(unix: u64, offset: i64) -> ((i64, u32, u32), u32, u32) {
    let local = i64::try_from(unix)
        .unwrap_or(i64::MAX / 2)
        .saturating_add(offset);
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        (year, month, day),
        (seconds / 3_600) as u32,
        (seconds % 3_600 / 60) as u32,
    )
}

fn date_text((year, month, day): (i64, u32, u32)) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

/// The local time zone's distance from UTC at `unix`, in seconds; 0 when the system cannot say.
pub(crate) fn local_offset(unix: u64) -> i64 {
    let Ok(time) = libc::time_t::try_from(unix) else {
        return 0;
    };
    // SAFETY: `localtime_r` reads the time and fills in the `tm` it is given, which is a plain
    // struct of integers (and a pointer to a zone name it only writes), zeroed as the C docs allow.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            0
        } else {
            // `c_long`: 64 bits where this runs, and not on every platform libc has.
            #[allow(clippy::useless_conversion)]
            i64::from(tm.tm_gmtoff)
        }
    }
}

/// [`apply`] and [`apply_default`] once the snapshot is chosen: `open` is what the server holds, and
/// `heading`, when there is one, goes first in the plan.
#[allow(clippy::too_many_arguments)]
fn apply_snapshot(
    t: &Tmux,
    cfg: &Config,
    paths: &Paths,
    name: &str,
    snap: snapshot::Snapshot,
    used: Used,
    open: Open,
    heading: Option<String>,
    dry_run: bool,
    wait: &prompt::Timing,
) -> anyhow::Result<Report> {
    // Only read when the feature is on: with it off, a history file left over from when
    // it was on must have no effect at all.
    let history = cfg
        .experimental
        .pane_history
        .then(|| history::load(paths, name))
        .flatten()
        // Only onto the layout they were taken with: a snapshot recovered by hand never gets
        // another layout's screens (herdr's rule, a missing fingerprint included).
        .and_then(|loaded| history::for_layout(loaded, &snap, name));
    if history.is_some() && !dry_run {
        // A restore never `cat`s a file an earlier one left behind: the layout it
        // described may no longer be this one's.
        history::clear_replays(paths, name);
    }
    let restore = Restore {
        cfg,
        paths,
        name,
        history: history.as_ref(),
        wait: *wait,
        resumed_any: std::cell::Cell::new(false),
        resumed: std::cell::RefCell::new(std::collections::HashMap::new()),
        running: &open.running,
        taw: taw_word(t),
    };
    let additions = {
        let plan = |saved: &PaneSnap| to_resume(cfg, paths, &restore.taw, saved);
        windows_to_add(&snap, &open, &plan)
    };

    let mut report = Report {
        snapshot: Some(used),
        ..Report::default()
    };
    report.plan.extend(heading);
    let mut resumes = Vec::new();
    // Autosave is held off from here to the end: the hooks that fire it on every new window and
    // split fire all through a restore, and what they would save is a half-made layout. With no
    // server there is nothing to set it on yet: the first session's own tmux call sets it, as it
    // starts the server.
    let mut restoring = (!dry_run && !open.no_server).then(|| Restoring::begin(t));
    let mut hold = (!dry_run && open.no_server)
        .then(|| super::restoring_mark(std::process::id(), super::unix_now()));
    if dry_run && open.no_server && !snap.sessions.is_empty() {
        report.plan.insert(0, WOULD_START.to_string());
    }

    for (index, session) in snap.sessions.iter().enumerate() {
        if open.sessions.contains(&session.name) {
            let add = &additions[index];
            if add.is_empty() {
                report.skipped.push(session.name.clone());
                report
                    .plan
                    .push(format!("skip '{}': session already exists", session.name));
                continue;
            }
            let added = SessionSnap {
                windows: add.iter().map(|&i| session.windows[i].clone()).collect(),
                ..session.clone()
            };
            let next = open.next_window.get(&session.name).copied().unwrap_or(0);
            for (k, window) in added.windows.iter().enumerate() {
                report.plan.push(format!(
                    "{} saved window {} to open session '{}' as window {}",
                    if dry_run { "would add" } else { "add" },
                    window_label(window),
                    session.name,
                    next + k as u32
                ));
            }
            if dry_run {
                let would = would_type(&restore, &added, &mut report.plan);
                report.would_type.extend(would);
                continue;
            }
            let made = Making::Windows {
                positions: add,
                next,
            };
            if let Err(err) =
                create_session(t, &restore, index, &added, made, &mut report, &mut resumes)
            {
                return Err(typed_before_failing(
                    t,
                    &restore,
                    &mut resumes,
                    &mut report,
                    err,
                ));
            }
            continue;
        }

        plan_session(&mut report.plan, session);
        if restore.history.is_some() {
            report.plan.push(format!(
                "  replay the saved screen into each pane of '{}' \
                 ([experimental] pane_history)",
                session.name
            ));
        }

        if dry_run {
            let would = would_type(&restore, session, &mut report.plan);
            report.would_type.extend(would);
            continue;
        }

        let everything: Vec<usize> = (0..session.windows.len()).collect();
        let made = Making::Session {
            hold: hold.as_deref(),
            positions: &everything,
        };
        let result = create_session(t, &restore, index, session, made, &mut report, &mut resumes);
        if hold.take().is_some() && t.server_start_unix().is_some() {
            // The call that made the session started the server and set the hold with it.
            restoring = Some(Restoring::held(t));
            report.started_server = true;
            report.plan.insert(0, STARTED.to_string());
            report.attach = Some(attach_line(&session.name));
        }
        if let Err(err) = result {
            return Err(typed_before_failing(
                t,
                &restore,
                &mut resumes,
                &mut report,
                err,
            ));
        }
    }

    resume_agents(t, &restore, resumes, &mut report)?;

    // Let go before the one save a restore makes, which autosave would otherwise hold off. A
    // restore that left a pane alone does not make it: that pane's agent is in the snapshot
    // being restored and in no pane, and a save would be the end of it.
    drop(restoring);
    if (!report.created.is_empty() || !report.added.is_empty()) && report.not_resumed.is_empty() {
        save_what_was_made(t, cfg, paths);
    }

    Ok(report)
}

/// What the plan's first line says when a restore started the tmux server.
const STARTED: &str = "started a tmux server (none was running), with your own tmux config";

/// And what a dry run says it would do.
const WOULD_START: &str = "would start a tmux server (none is running), with your own tmux config";

/// How to attach to the session a restore made first when it started the server. Quoted: a word
/// that begins with `=` is read by zsh as a command's path.
fn attach_line(first: &str) -> String {
    let target = format!("={}", crate::tmux::session_name(first));
    format!("tmux attach -t {}", crate::sandbox::typed::word(&target))
}

/// The sessions made so far keep the agents they were to have: what is planned is typed, and then
/// the error is given back. Left till the end of a restore that does not get there, those commands
/// would be lost without a word.
fn typed_before_failing(
    t: &Tmux,
    restore: &Restore<'_>,
    resumes: &mut Vec<Resume>,
    report: &mut Report,
    err: anyhow::Error,
) -> anyhow::Error {
    let planned = report.plan.len();
    if let Err(typing) = resume_agents(t, restore, std::mem::take(resumes), report) {
        log::warn!("typing the resumes planned before the restore failed: {typing:#}");
    }
    // The report goes with the restore, and the error is all that is given back: the panes
    // that were left alone, with what to run in each, are said there.
    saying_what_was_left_alone(err, &report.plan[planned..])
}

/// `err` with the lines of the plan that say which panes a restore that failed left alone, one to a
/// line after its own message, so that the commands that were not typed are not lost with the report
/// that would have held them. The error that is returned is the same one as before, with a layer
/// around it: `cli::envelope::code_for` looks through the layers for its code.
fn saying_what_was_left_alone(err: anyhow::Error, lines: &[String]) -> anyhow::Error {
    if lines.is_empty() {
        return err;
    }
    let message = format!("{err}\n{}", lines.join("\n"));
    err.context(message)
}

/// Holds [`super::RESTORING_OPTION`] set for as long as it lives, so that it is cleared on every
/// way out of a restore that runs to the end of it: an error and a panic included. A process that
/// is killed does not get that far, and what it set names it ([`super::restoring_mark`]), so that
/// autosave can tell that it has gone.
struct Restoring<'a> {
    t: &'a Tmux,
}

impl<'a> Restoring<'a> {
    fn begin(t: &'a Tmux) -> Restoring<'a> {
        let mark = super::restoring_mark(std::process::id(), super::unix_now());
        if let Err(err) = t.set_global_option(super::RESTORING_OPTION, &mark) {
            log::warn!("could not hold autosave off while restoring: {err:#}");
        }
        Restoring { t }
    }

    /// The hold is set already (by the call that started the server): only its release is left.
    fn held(t: &'a Tmux) -> Restoring<'a> {
        Restoring { t }
    }
}

impl Drop for Restoring<'_> {
    fn drop(&mut self) {
        if let Err(err) = self.t.unset_global_option(super::RESTORING_OPTION) {
            log::warn!("could not let autosave go after restoring: {err:#}");
        }
    }
}

/// The one autosave a restore makes, once it is done: the layout it made, with the agents it
/// resumed in it, in the snapshot autosave keeps. It is the one its hooks asked for all
/// through, which were held off, and no other: with none asked for (no hooks installed, and
/// nothing a watcher reported) the snapshot is left as it is, so that restoring one snapshot
/// does not rewrite another that nobody asked to be touched. A save that fails is logged and
/// no more: the restore made what was asked of it.
fn save_what_was_made(t: &Tmux, cfg: &Config, paths: &Paths) {
    if let Err(err) = super::autosave_if_due(t, cfg, paths, std::time::Duration::ZERO) {
        log::warn!("saving the layout a restore made failed: {err:#}");
    }
}

/// Everything [`create_session`] needs beyond the session it is rebuilding.
struct Restore<'a> {
    cfg: &'a Config,
    paths: &'a Paths,
    /// The snapshot's name — where the pane-history replay files are written.
    name: &'a str,
    /// The saved screens, when `[experimental] pane_history` is on and there are any.
    history: Option<&'a History>,
    /// How long each pane's shell gets to come to its prompt, and how it is looked at.
    wait: prompt::Timing,
    /// Whether [`resume_agents`] has already resumed a pane during this `apply()` call, so
    /// [session] startup_per_agent_delay_ms` paces every resume *across* the whole
    /// restore (not only the ones that land in the same saved session), the same way
    /// herdr's global resume scheduler does (`46a5091838`, #4487, #4102 in v0.9.2's
    /// changelog). A `Cell` rather than a `&mut` thread-through: `create_session` already
    /// takes `restore` by shared reference, and this is the one piece of state every call
    /// to it shares.
    resumed_any: std::cell::Cell<bool>,
    /// What this restore resumes, each once, across every session it makes: the key of each session
    /// it resumes ([`Planned::key`]), and the pane it resumes it in. A snapshot can hold one session in
    /// two panes (a pane split while the agent ran, a layout saved twice over), and herdr resumes each
    /// session in one pane of a restore.
    resumed: std::cell::RefCell<std::collections::HashMap<String, String>>,
    /// The agents that run in the server's panes now: a window added to an open session resumes none of
    /// these ([`Open::runs`]).
    running: &'a [Running],
    /// How a typed `taw sandbox` line names taw (`sandbox::typed::taw_word`), worked out once.
    taw: String,
}

impl Restore<'_> {
    /// The shell command a pane should start with, or `None` for the pane's plain shell.
    ///
    /// `Some` means "paint the saved screen back, then become the shell". Suppressed for
    /// a pane that is about to have an agent resumed into it: that agent restores its own
    /// conversation, and a replayed screen would sit above it as a second, stale copy of
    /// the same transcript. (herdr does the same —
    /// `herdr-src/src/persist/restore.rs`, `pane_restore_startup`.) And for a pane whose saved
    /// directory `looked` found gone or not to be opened ([`look`]), which herdr starts nothing in:
    /// an agent's last screen would sit in a pane that tmux opened in the home folder, with no agent
    /// in it.
    fn pane_command(
        &self,
        session_index: usize,
        window_index: usize,
        window: &snapshot::WindowSnap,
        pane: &PaneSnap,
        looked: Looked,
    ) -> Option<String> {
        let history = self.history?;
        if looked != Looked::There || self.will_resume(pane) {
            return None;
        }
        let saved = history::pane_history(history, session_index, window_index, pane.index)?;
        history::replay_command(self.paths, self.name, window.index, pane.index, saved)
    }

    /// Whether [`plan_resumes`] will plan a resume command for this pane, which
    /// [`resume_agents`] then types once the pane's shell is at its prompt, if tmux opens the pane
    /// in its saved directory: the conditions the planning applies, but for that one, which is
    /// known only once tmux has made the pane ([`left_alone`]). A pane whose session another pane
    /// resumes is not painted over either, as herdr's is not: its agent's conversation is the
    /// other pane's.
    fn will_resume(&self, pane: &PaneSnap) -> bool {
        // Only whether there is a plan is wanted, so no transcript is read for the effort.
        self.cfg.session.resume_agents_on_restore
            && matches!(
                plan_with(self.cfg, self.paths, &self.taw, pane, Read::No),
                Ok(Some(_))
            )
    }

    /// The pane of the server that already runs the session `planned` would resume ([`Open::runs`]).
    fn running_in_the_server(&self, planned: &Planned) -> Option<&str> {
        runs_in(self.running, planned)
    }

    /// Takes the session `key` for the pane `pane_id`, unless a pane before it in this restore has: then
    /// that pane, which the session is resumed in.
    fn claim(&self, key: &str, pane_id: &str) -> Option<String> {
        let mut resumed = self.resumed.borrow_mut();
        if let Some(first) = resumed.get(key) {
            return Some(first.clone());
        }
        resumed.insert(key.to_string(), pane_id.to_string());
        None
    }
}

/// The resume command for one native agent integration, or `None` for a kind that has no
/// (or an unsupported) resume path — such a pane restores as a plain shell, never
/// resumed.
///
/// The id is shell-quoted. This string is delivered with `send-keys … Enter`, so it is
/// executed by the pane's shell, not by `exec`: an id carrying `;`, a newline, `$(…)` or a
/// backtick would otherwise run whatever it names. [`valid_session_id`] already rejects
/// those at every write, and this is the second, independent guard — the one that also
/// covers a snapshot written before that check existed.
/// Every command here is herdr's, from its "Native session restore" table — same
/// executable, same flag, same `--flag value` versus `--flag=value` spelling. `kind` is
/// the pane's `@agent_kind`, which is the `--agent` label the integration reported, so
/// Antigravity CLI appears as `agy` (what its hook reports, its detection manifest, and
/// its own executable name) rather than as its `antigravity-cli` install target.
///
/// A kind with no entry restores as a plain shell. That is deliberate for an agent whose
/// resume command taw does not know.
///
/// `session_id` is whatever the pane is resumed from, and goes where the id goes: `pi` and
/// `omp` prefer to report the path of their session file rather than an id, and a pane that
/// reported only that is resumed from the path ([`resume_target`] makes the choice).
///
/// The command is the agent's own ([`crate::detect::executable`]) and the words of
/// [`resume_args`], each written by [`shell_word`], so the line restore types into a plain
/// pane and the arguments it gives `taw sandbox` for a sandboxed one come from the one table.
pub fn resume_command(kind: &str, session_id: &str) -> Option<String> {
    command_of(kind, session_id, &[])
}

/// [`resume_command`] with `flags` between the agent's own command and the resume arguments, each word
/// written by [`shell_word`] as the rest of the line is.
fn command_of(kind: &str, session_id: &str, flags: &[String]) -> Option<String> {
    let args = resume_args(kind, session_id)?;
    let mut words = vec![crate::detect::executable(kind)];
    words.extend(flags.iter().map(|flag| shell_word(flag)));
    words.extend(args.iter().map(|arg| shell_word(arg)));
    Some(words.join(" "))
}

/// [`resume_command`] for the pane's own profile: a Claude pane that ran under another profile
/// (another `CLAUDE_CONFIG_DIR`, as [`profile_of`] reads it from the transcript path) is typed
/// `CLAUDE_CONFIG_DIR=<profile> claude --resume <id>`, the profile written by [`shell_word`] as every
/// word of the line is, and with the flags the pane's Claude was started with
/// ([`crate::claude_flags`]) after the command: `CLAUDE_CONFIG_DIR=<profile> claude --effort max
/// --resume <id>`. The flags are read through [`crate::claude_flags::refiltered`] again, so that a
/// snapshot that holds more than the listed flags types no more than them. With no profile and no
/// flags, and for any agent but Claude (which keeps neither), it is [`resume_command`]'s line.
pub fn resume_line(
    kind: &str,
    session_id: &str,
    profile: Option<&str>,
    flags: &[String],
) -> Option<String> {
    if kind != "claude" {
        return resume_command(kind, session_id);
    }
    let line = command_of(kind, session_id, &crate::claude_flags::refiltered(flags))?;
    Some(match profile {
        Some(profile) => format!("CLAUDE_CONFIG_DIR={} {line}", shell_word(profile)),
        None => line,
    })
}

/// The flags a restore types after `claude` for a saved pane: the ones it saved, as [`resume_line`]
/// reads them, with the effort of the session's last turn in place of the saved one
/// ([`claude_flags_of`]), and none for any other agent.
fn flags_of(kind: &str, saved: &PaneSnap, read: Read) -> Vec<String> {
    if kind == "claude" {
        claude_flags_of(saved, read)
    } else {
        Vec::new()
    }
}

/// The flags a Claude pane comes back with: the saved ones (what its Claude was started with), with
/// `--effort` from the last turn of the session's own transcript when that gives one, since an effort
/// set inside the session (`/effort max`) is on no command line. The saved path is opened only when
/// [`transcript_profile`] accepts its shape, and [`crate::claude_flags::read_turn_effort`] reads
/// nothing but the level word. With no file or no field, the saved flags stand. The result goes
/// through [`crate::claude_flags::refiltered`] once more, as [`resume_line`] reads it, so a list that
/// is already full has no room for an effort it had none of, and the line typed and the flags saved
/// with the pane are the same words. `Read::No` reads no file (the flags are then the saved ones).
fn claude_flags_of(saved: &PaneSnap, read: Read) -> Vec<String> {
    let flags = crate::claude_flags::refiltered(&saved.agent_claude_flags);
    if read == Read::No {
        return flags;
    }
    let level = saved
        .agent_session_path
        .as_deref()
        .filter(|path| transcript_profile(path).is_some())
        .and_then(|path| crate::claude_flags::read_turn_effort(std::path::Path::new(path)));
    crate::claude_flags::refiltered(&crate::claude_flags::with_effort(&flags, level))
}

/// Whether a plan reads the pane's transcript for the effort of its last turn. A plan that wants only
/// the pane's key, or whose flags are thrown away (a sandboxed or container pane), does not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    Yes,
    No,
}

/// The profile a Claude pane ran under, from the transcript path its Claude reported: the folder above
/// `projects/` in `<profile>/projects/<project>/<id>.jsonl`, an absolute path with exactly those two
/// levels under `projects/`. `None` for Claude's default profile (`home`'s `.claude`: typing
/// `CLAUDE_CONFIG_DIR=$HOME/.claude` is not the same as leaving it unset, and a pane that ran on the
/// default comes back on whatever its restored shell has), and for a missing, relative or other-shaped
/// path, or one that holds a control character. The project folder is not looked at: it can differ from
/// the pane's folder (a renamed repository keeps its old transcript folder).
pub fn profile_of(path: &str, home: Option<&std::path::Path>) -> Option<String> {
    let profile = transcript_profile(path)?;
    let default = home.map(|home| home.join(".claude"));
    (default.as_deref() != Some(std::path::Path::new(&profile))).then_some(profile)
}

/// The folder above `projects/` in a transcript path of the shape [`profile_of`] describes, the default
/// profile included: `None` for any other shape. The check that lets a restore open the path
/// ([`claude_flags_of`]).
fn transcript_profile(path: &str) -> Option<String> {
    if path.len() > 4096 || path.chars().any(|c| c.is_control()) {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    // "" (the root), the profile's folders, `projects`, the project and the file.
    let [root, profile @ .., projects, project, file] = parts.as_slice() else {
        return None;
    };
    let named = |part: &&str| !part.is_empty() && *part != "." && *part != "..";
    let transcript = file
        .strip_suffix(".jsonl")
        .is_some_and(|stem| !stem.is_empty());
    if !root.is_empty()
        || profile.is_empty()
        || *projects != "projects"
        || !profile.iter().all(named)
        || !named(project)
        || !transcript
    {
        return None;
    }
    Some(format!("/{}", profile.join("/")))
}

// Follows herdr's src/agent_resume.rs (the Letta arm of plan).
// Differences: the arguments are kept as words, quoted only when a line is written, since the
// two forms place the value in different argv positions.
/// The arguments that resume `kind`'s session `session_id`, after the agent's own command: herdr's
/// "Native session restore" table, the id one of the words as it is. `None` for a kind with no
/// resume, and for Letta's default conversation with no agent id after its marker.
pub fn resume_args(kind: &str, session_id: &str) -> Option<Vec<String>> {
    let id = session_id.to_string();
    Some(match kind {
        "claude" | "cursor" | "devin" | "droid" | "grok" | "hermes" | "qodercli" | "qwen" => {
            vec!["--resume".to_string(), id]
        }
        "codex" => vec!["resume".to_string(), id],
        "copilot" | "omp" => vec![format!("--resume={id}")],
        "kilo" | "kimi" | "opencode" | "pi" => vec!["--session".to_string(), id],
        "mastracode" => vec!["--thread".to_string(), id],
        "agy" => vec!["--conversation".to_string(), id],
        // Letta's default conversation has no id of its own, so its `SessionStart` hook encodes
        // it as `default:<agent_id>` (see `taw-agent-session-letta.sh`, herdr's encoding); the
        // marker with no agent id after it is refused outright, rather than resumed against a
        // blank `--agent`.
        "letta" => match session_id.strip_prefix("default:") {
            Some("") => return None,
            Some(agent) => vec![
                "--conversation".to_string(),
                "default".to_string(),
                "--agent".to_string(),
                agent.to_string(),
            ],
            None => vec!["--conversation".to_string(), id],
        },
        _ => return None,
    })
}

/// One word of a resume command, written for the pane's shell by taw's one rule for a typed word
/// ([`crate::sandbox::typed::word`]): a `--flag=value` word keeps its flag bare and quotes its
/// value (`copilot --resume='a b'`), as herdr spells it; any other word is quoted whole. The rule
/// quotes a word with `=` in it whole, which would type `omp '--resume=o1'` where
/// `omp --resume=o1` was typed before.
fn shell_word(word: &str) -> String {
    match word.split_once('=') {
        Some((flag, value))
            if flag.len() > 2
                && flag.starts_with("--")
                && flag[2..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-') =>
        {
            format!("{flag}={}", crate::sandbox::typed::word(value))
        }
        _ => crate::sandbox::typed::word(word),
    }
}

// Follows herdr's src/agent_resume.rs. Differences: generalised from
// herdr's Agent enum to a plain kind string; only codex's explicit resume launch is recognised,
// and the id is checked too, since it goes straight to a pane option.
/// The session id an explicit `<kind> resume <id>`-style launch already carries, before
/// the agent's own hook has had a chance to report it — so a pane started this way, or
/// resumed by [`resume_agents`] this way, keeps its `@agent_session_id` across an autosave
/// that runs before that hook fires, or a server restart that happens before it ever does.
///
/// Follows herdr's `agent_resume.rs` `persisted_session_from_launch_args`, generalised
/// from herdr's `Agent` enum to a plain kind string, since that is how every other kind
/// check in this crate works. Only Codex's launch syntax is recognised today — its
/// argument shape is exactly `["resume", <id>]`, with the id not starting with `-` (which
/// would be a flag, such as `--last`, not a session id) — but a future kind with a similar
/// explicit-resume syntax can extend the match. [`valid_session_id`] is checked too: this
/// value is about to be written straight to a pane option with no other gate in front of
/// it, unlike a hook's own report, which always goes through `report.rs`'s
/// `check_session_id` first.
pub fn persisted_session_from_launch_args<'a>(
    kind: &str,
    agent_args: &'a [String],
) -> Option<&'a str> {
    let [command, session_id] = agent_args else {
        return None;
    };
    if kind != "codex" || command != "resume" || session_id.starts_with('-') {
        return None;
    }
    valid_session_id(session_id).then_some(session_id.as_str())
}

/// Whether a string is usable as `@agent_session_id`: `^[A-Za-z0-9._:-]{1,128}$`.
///
/// Every native agent's own session ids are UUIDs or similarly plain tokens, so this
/// rejects nothing a real integration produces — while a value that could reach a shell
/// through [`resume_command`], or break a tab-joined listing, cannot be stored at all. `:`
/// is the one addition beyond a plain token: Letta's `default:<agent_id>` encoding for its
/// default conversation (see [`letta_resume_command`]) needs it, and it is not shell-active
/// — `resume_command` quotes whatever it is handed regardless.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

fn plan_session(plan: &mut Vec<String>, session: &SessionSnap) {
    plan.push(format!(
        "create session '{}' (cwd {}) with {} window(s)",
        session.name,
        session.path,
        session.windows.len(),
    ));
    for (i, window) in session.windows.iter().enumerate() {
        let verb = if i == 0 {
            "use first window as"
        } else {
            "create"
        };
        plan.push(format!(
            "  {verb} window {} ({} pane(s), layout {})",
            window_label(window),
            window.panes.len(),
            window.layout,
        ));
    }
    if let Some(active) = session
        .windows
        .iter()
        .find(|w| w.index == session.active_window_index)
    {
        plan.push(format!("  select window {}", window_label(active)));
    }
}

/// What a dry run says of the agents of one session it would make: for each pane it would resume,
/// the exact line it would type (`  window 0 'zsh', pane 1: would type: <line>`), and for each pane
/// with an agent that it would not resume, why. Appended to `plan` in the order of the panes, and
/// returned as data for the JSON. Nothing is read from tmux and nothing is typed. The same decision
/// as a restore's ([`plan_of`], one pane for each session as [`Restore::claim`] has it, and none that
/// already runs in a pane of the server), and the same
/// directory check ([`look`]): a pane tmux would not open in its saved directory is typed nothing.
fn would_type(
    restore: &Restore<'_>,
    session: &SessionSnap,
    plan: &mut Vec<String>,
) -> Vec<WouldType> {
    let mut would = Vec::new();
    if !restore.cfg.session.resume_agents_on_restore {
        return would;
    }
    for window in &session.windows {
        for saved in &window.panes {
            let at = format!("window {}, pane {}", window_label(window), saved.index);
            let planned = match plan_of(restore, saved) {
                Ok(Some(planned)) => planned,
                Ok(None) => continue,
                Err(reason) => {
                    plan.push(format!("  {at}: would not resume: {reason}"));
                    continue;
                }
            };
            let id = format!("session '{}', {at}", session.name);
            // As in a restore, a pane tmux does not open in its saved directory takes no part in which
            // pane resumes a session that two saved.
            match look(&saved.cwd) {
                Looked::There => {}
                looked => {
                    let why = if looked == Looked::Gone {
                        "is gone, so tmux would open it in your home folder"
                    } else {
                        "cannot be opened, so tmux would open it in your home folder"
                    };
                    plan.push(format!(
                        "  {at}: would not type: its saved directory {} {why}; once it can be opened, resume it with: {}",
                        saved.cwd,
                        from_directory(&saved.cwd, &planned.command)
                    ));
                    continue;
                }
            }
            if let Some(pane) = restore.running_in_the_server(&planned) {
                plan.push(format!(
                    "  {at}: would not resume: pane {pane} already runs the same session"
                ));
                continue;
            }
            if let Some(first) = restore.claim(&planned.key, &id) {
                plan.push(format!(
                    "  {at}: would not resume: {first} resumes the same session"
                ));
                continue;
            }
            plan.push(format!("  {at}: would type: {}", planned.command));
            would.push(WouldType {
                session: session.name.clone(),
                window: window.index,
                pane: saved.index,
                line: planned.command,
            });
        }
    }
    would
}

/// How the plan names a saved window: the number it had when the layout was saved, which is
/// what the person saw before the restart, and its name. A name is no way to tell windows
/// apart (two windows called `zsh` are the usual case), and the number a window has on the
/// live server is not the one it had: the restore numbers the windows it makes from the
/// server's base index, and a window closed before the save left a gap in the saved ones.
fn window_label(window: &snapshot::WindowSnap) -> String {
    format!("{} '{}'", window.index, window.name)
}

/// Creates one saved session on the live server: `new-session`, then for each window
/// either renames the session's free first window (index 0 in the loop below — not
/// necessarily tmux window index 0, whatever `base-index` gives a fresh session) or
/// creates a new one, splits it once per additional saved pane, and re-applies the exact
/// saved `#{window_layout}` string. Finally selects the saved active window and, when
/// enabled, works out which agents are to be resumed (see [`plan_resumes`]), for
/// [`resume_agents`] to carry out once every session is made. The agents of the windows made
/// are planned whether or not the whole session is.
///
/// **Every window and pane is addressed by its tmux id** (`@N` / `%N`), captured from the
/// `-P -F` output of the very command that created it. `<session>:<window name>` is not a
/// usable target for this: window names are neither unique nor a target grammar, so two
/// saved windows called `dup` resolved to the same live window and piled both windows'
/// splits onto it, and a name holding a `.` was parsed as `<window>.<pane>` and failed
/// with "can't find pane". Both aborted the restore half-built, and the half that had been
/// built matched nothing that was saved.
fn create_session(
    t: &Tmux,
    restore: &Restore<'_>,
    session_index: usize,
    session: &SessionSnap,
    making: Making<'_>,
    report: &mut Report,
    resumes: &mut Vec<Resume>,
) -> anyhow::Result<()> {
    let adding = matches!(making, Making::Windows { .. });
    let mut built: Vec<Built> = Vec::with_capacity(session.windows.len());
    let made =
        make_windows(t, restore, session_index, session, making, &mut built).and_then(|()| {
            // The windows added to an open session leave the one that is selected as it is.
            if adding {
                Ok(())
            } else {
                select_saved_window(t, session, &built, report)
            }
        });

    // What was made keeps the agents it was to have, whatever stopped the rest of the session:
    // each is typed, or listed with its command. A pane that tmux did not open in its saved
    // directory is typed nothing, whether agents are resumed or not, and is marked so.
    resumes.extend(leave_alone_panes_opened_elsewhere(
        t, restore, session, &built, report,
    ));
    if restore.cfg.session.resume_agents_on_restore {
        resumes.extend(plan_resumes(restore, session, &built, report));
    }

    match made {
        Ok(()) => {
            let (any, seen) = windows_still_there(t, session, &built, report);
            if !any {
                // Everything it made has closed since, and the session with it.
                let every_window: Vec<String> = session.windows.iter().map(window_label).collect();
                report
                    .plan
                    .push(stopped_line(&session.name, &[], &every_window));
            } else if let Making::Windows { next, .. } = making {
                // Each window by the number tmux gave it, as it was found: one that closed since is not
                // said to be added, and the plan says so.
                for (k, (window, seen)) in session.windows.iter().zip(seen).enumerate() {
                    let number = match seen {
                        Seen::At(number) => number,
                        Seen::Unknown => next + k as u32,
                        Seen::Closed => continue,
                    };
                    report.added.push(added_text(window, &session.name, number));
                }
            } else {
                report.created.push(session.name.clone());
            }
        }
        // A pane or window closed under the restore: the shell of a pane can end at any moment,
        // and tmux takes the pane (and its window, and its session) with it. What was made of
        // this session stays, and the sessions after it are still made.
        Err(err) if prompt::is_gone(&err) => {
            let (made, not_made) = made_and_not(session, &built);
            report
                .plan
                .push(stopped_line(&session.name, &made, &not_made));
        }
        Err(err) => return Err(err),
    }
    Ok(())
}

/// What `added` holds for a window added to the open session `session`, which is window `number` of it
/// now.
fn added_text(window: &snapshot::WindowSnap, session: &str, number: u32) -> String {
    format!(
        "saved window {} as window {number} of '{session}'",
        window_label(window)
    )
}

/// What [`create_session`] makes.
#[derive(Clone, Copy)]
enum Making<'a> {
    /// A session that is not open, whole: `new-session` for its first window, and `hold`, when there is
    /// no server yet and so nothing to hold autosave off on, the value that same tmux call sets
    /// [`super::RESTORING_OPTION`] to. `positions` are the windows' places in the saved session.
    Session {
        hold: Option<&'a str>,
        positions: &'a [usize],
    },
    /// Windows of a session that is open, each made after its last window with `new-window -a`: the
    /// positions of those windows in the saved session, which `session` of [`create_session`] holds only
    /// some of, and the number the first of them is expected to get (one more than the session's
    /// highest, the next for each after it).
    Windows { positions: &'a [usize], next: u32 },
}

/// Creates the session (when it is a new one) and each of its windows and panes, in `built` as each
/// is made, and re-applies each window's saved layout.
fn make_windows(
    t: &Tmux,
    restore: &Restore<'_>,
    session_index: usize,
    session: &SessionSnap,
    making: Making<'_>,
    built: &mut Vec<Built>,
) -> anyhow::Result<()> {
    let (fresh, positions, hold) = match making {
        Making::Session { hold, positions } => (true, positions, hold),
        Making::Windows { positions, .. } => (false, positions, None),
    };
    // `new-session -c` seeds window 0 / pane 0's cwd — that pane's own saved `cwd` (its
    // live `#{pane_current_path}` at snapshot time) is the right value here, exactly like
    // every other window's first pane uses its own saved `cwd` below: `#{session_path}`
    // (`session.path`) is frozen at session-creation time and never reflects a `cd` the
    // user made afterward, so falling back to it here would restore window 0 at whatever
    // directory the session merely *started* in, not where its first pane actually was.
    let first_pane = session.windows.first().and_then(|w| w.panes.first());
    let mut window_id = String::new();
    let mut first_made = None;
    if fresh {
        let first_cwd = first_pane
            .map(|p| p.cwd.as_str())
            .unwrap_or(session.path.as_str());
        // What is where each pane is to open is looked at once, before tmux is asked to open it there:
        // that, and where tmux was asked to open it as tmux read it, decide whether it is left alone
        // ([`left_alone`]).
        let first_looked = look(first_cwd);
        // The command the session's very first pane starts with: the pane-history replay
        // where there is one, else nothing at all (tmux then uses `default-shell`, which is
        // what every pane got before this feature existed).
        let first_command = session.windows.first().and_then(|window| {
            window.panes.first().and_then(|pane| {
                restore.pane_command(session_index, positions[0], window, pane, first_looked)
            })
        });
        let mut new_session: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            crate::tmux::new_name(&session.name),
            "-c".into(),
            crate::tmux::start_directory(first_cwd),
            "-x".into(),
            "200".into(),
            "-y".into(),
            "50".into(),
            "-P".into(),
            "-F".into(),
            NEW_WINDOW_FORMAT.into(),
        ];
        // The shell command goes last: tmux takes it as the trailing operand.
        if let Some(command) = &first_command {
            new_session.push(command.clone());
        }
        // The server this starts has no hold on autosave yet: the same call sets it, so that no hook of
        // the user's config that fires on the first window sees a restore that is not marked.
        if let Some(mark) = hold {
            new_session
                .extend([";", "set-option", "-g", super::RESTORING_OPTION, mark].map(String::from));
        }
        let borrowed: Vec<&str> = new_session.iter().map(String::as_str).collect();
        let created = t.run(&borrowed)?;
        let (id, pane_id, start) = parse_ids(&created, "new-session")?;
        window_id = id;
        first_made = Some(BuiltPane::new(pane_id, first_pane, first_looked, &start));
    }

    // `built` holds the live ids of each window this call creates and of its panes, in the saved
    // windows' order and each window's own saved pane order: what everything below addresses
    // tmux by. Each is put in as soon as it is made, so that whatever stops the rest, what was
    // made is known: its agents are still typed, and the plan says which windows were made.
    for (i, window) in session.windows.iter().enumerate() {
        if i == 0 && fresh {
            built.push(Built {
                window_id: window_id.clone(),
                panes: first_made.iter().cloned().collect(),
                complete: false,
            });
            // The `--` is for a window saved under a name that starts with a dash.
            t.run_ok(&[
                "rename-window",
                "-t",
                &window_id,
                "--",
                &crate::tmux::new_name(&window.name),
            ])?;
        } else {
            // A new session is found by the name it was made with, its next window at the next free
            // index. An open one by exactly its own name (`=`: a bare name is matched as a prefix), and
            // its window after the last one (`-a` at `{end}`), never at a number a closed window left
            // free between the user's windows.
            let session_target = if fresh {
                format!("{}:", session.name)
            } else {
                format!("={}:{{end}}", session.name)
            };
            let first_cwd = window
                .panes
                .first()
                .map(|p| p.cwd.as_str())
                .unwrap_or(session.path.as_str());
            let looked = look(first_cwd);
            let mut new_window: Vec<String> = vec!["new-window".into(), "-d".into()];
            if !fresh {
                new_window.push("-a".into());
            }
            new_window.extend([
                "-t".into(),
                session_target,
                "-n".into(),
                crate::tmux::new_name(&window.name),
                "-c".into(),
                crate::tmux::start_directory(first_cwd),
                "-P".into(),
                "-F".into(),
                NEW_WINDOW_FORMAT.into(),
            ]);
            if let Some(command) = window.panes.first().and_then(|pane| {
                restore.pane_command(session_index, positions[i], window, pane, looked)
            }) {
                new_window.push(command);
            }
            let borrowed: Vec<&str> = new_window.iter().map(String::as_str).collect();
            let created = t.run(&borrowed)?;
            let (id, pane_id, start) = parse_ids(&created, "new-window")?;
            window_id = id;
            built.push(Built {
                window_id: window_id.clone(),
                panes: vec![BuiltPane::new(
                    pane_id,
                    window.panes.first(),
                    looked,
                    &start,
                )],
                complete: false,
            });
        }

        for pane in window.panes.iter().skip(1) {
            // Split the *previously created* pane, never the window's active pane. tmux
            // inserts a new pane immediately after its split target, so splitting the
            // last-made pane each time keeps the window's pane list in creation order.
            // That matters because `select-layout` assigns panes to the saved layout's
            // cells *positionally* — in pane-list order, ignoring the pane-id numbers baked
            // into the layout string (verified against tmux 3.6b) — and the snapshot stores
            // panes in `pane_index` order, which is tmux's own spatial (layout-leaf) order.
            // A list in creation order therefore lines up cell-for-cell with the saved
            // layout, landing each pane's saved cwd (and resumed agent) in the cell it was
            // captured from. Splitting the active pane instead reversed part of that order
            // and permuted the cwds across panes.
            let previous = built
                .last()
                .and_then(|made| made.panes.last())
                .map(|pane| pane.id.clone())
                .expect("a window made always holds its first pane");
            let looked = look(&pane.cwd);
            let mut split_args: Vec<String> = vec![
                "split-window".into(),
                "-d".into(),
                "-t".into(),
                previous,
                "-c".into(),
                crate::tmux::start_directory(&pane.cwd),
                "-P".into(),
                "-F".into(),
                NEW_PANE_FORMAT.into(),
            ];
            if let Some(command) =
                restore.pane_command(session_index, positions[i], window, pane, looked)
            {
                split_args.push(command);
            }
            let borrowed: Vec<&str> = split_args.iter().map(String::as_str).collect();
            let split = t.run(&borrowed)?;
            // Validated like `new-session`/`new-window`'s reply: an empty answer would
            // become `-t ""` on the very next command, which tmux resolves to the
            // *current* pane — so a split whose id did not come back would silently send
            // the rest of this window's work somewhere else entirely.
            let (split, start) = parse_pane(&split, "split-window")?;
            if let Some(made) = built.last_mut() {
                made.panes
                    .push(BuiltPane::new(split, Some(pane), looked, &start));
            }
        }
        t.run_ok(&["select-layout", "-t", &window_id, &window.layout])?;
        if let Some(made) = built.last_mut() {
            made.complete = true;
        }
    }
    Ok(())
}

/// Selects the saved active window, by its own id.
fn select_saved_window(
    t: &Tmux,
    session: &SessionSnap,
    built: &[Built],
    report: &mut Report,
) -> anyhow::Result<()> {
    let Some((window, saved)) = built
        .iter()
        .zip(&session.windows)
        .find(|(_, saved)| saved.index == session.active_window_index)
    else {
        return Ok(());
    };
    // The window by its own id, not by a pane's: a pane's shell can end at any moment, the
    // first one's included, and tmux takes the pane away with it, while the window stays
    // as long as another pane of it does. Nothing else names a window this long after its
    // first pane was made, so a pane that is gone shows here first: `select-window -t %7`
    // saying "can't find pane" once ended a whole restore, half built.
    match t.run_ok(&["select-window", "-t", &window.window_id]) {
        Ok(()) => Ok(()),
        Err(err) if prompt::is_gone(&err) => {
            report.plan.push(not_selected_line(&window_label(saved)));
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// The saved windows that were made whole, and those that were not, each as [`window_label`]
/// names it.
fn made_and_not(session: &SessionSnap, built: &[Built]) -> (Vec<String>, Vec<String>) {
    let (mut made, mut not_made) = (Vec::new(), Vec::new());
    for (i, window) in session.windows.iter().enumerate() {
        if built.get(i).is_some_and(|built| built.complete) {
            made.push(window_label(window));
        } else {
            not_made.push(window_label(window));
        }
    }
    (made, not_made)
}

/// Says in the plan which windows came back with fewer panes than were saved, or not at all,
/// and returns whether any window of the session is still there, with where each of the windows made
/// was found ([`Seen`]).
///
/// A pane whose shell ended while the session was being made is not in its window any more, and
/// a restore that went on without it would otherwise say nothing of the hole.
///
/// Best effort: a server that cannot be asked has said nothing either, and the session is
/// taken to be there.
fn windows_still_there(
    t: &Tmux,
    session: &SessionSnap,
    built: &[Built],
    report: &mut Report,
) -> (bool, Vec<Seen>) {
    let Ok(live) = t.list_windows() else {
        return (true, built.iter().map(|_| Seen::Unknown).collect());
    };
    let mut any = built.is_empty();
    let mut seen = Vec::with_capacity(built.len());
    for (saved, window) in session.windows.iter().zip(built) {
        match live.iter().find(|w| w.window_id == window.window_id) {
            None => {
                report.plan.push(window_closed_line(&window_label(saved)));
                seen.push(Seen::Closed);
            }
            Some(live) => {
                any = true;
                seen.push(Seen::At(live.window_index));
                if (live.window_panes as usize) < saved.panes.len() {
                    report.plan.push(window_short_line(
                        &window_label(saved),
                        live.window_panes,
                        saved.panes.len(),
                    ));
                }
            }
        }
    }
    (any, seen)
}

/// Where a window [`create_session`] made was found once the session was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// Not there any more.
    Closed,
    /// The server could not be asked.
    Unknown,
    /// There, with this number in its session.
    At(u32),
}

/// What the plan says of a session that was given up on because a pane or window of it closed
/// while it was being made, and which of its windows were made and which not. The windows are
/// named as [`window_label`] does.
fn stopped_line(session: &str, made: &[String], not_made: &[String]) -> String {
    format!(
        "stopped restoring session '{session}': a pane or window of it closed while it was being \
         made; windows made: {}; not made: {}",
        window_list(made),
        window_list(not_made)
    )
}

/// Windows, each already named by [`window_label`], for a plan line: `none` for no window.
fn window_list(labels: &[String]) -> String {
    if labels.is_empty() {
        return "none".to_string();
    }
    labels.join(", ")
}

/// What the plan says of a window (named by [`window_label`]) that closed before it could be
/// selected.
fn not_selected_line(window: &str) -> String {
    format!("could not select window {window}: it closed while the session was being restored")
}

/// What the plan says of a window (named by [`window_label`]) that is not there any more.
fn window_closed_line(window: &str) -> String {
    format!("window {window} was closed while the session was being restored")
}

/// What the plan says of a window (named by [`window_label`]) that came back with fewer panes
/// than were saved.
fn window_short_line(window: &str, live: impl std::fmt::Display, saved: usize) -> String {
    format!(
        "window {window} has {live} of its {saved} panes: the rest closed while the session was being restored"
    )
}

/// A window [`create_session`] made: its id, and its panes in the saved order.
struct Built {
    window_id: String,
    /// The panes made so far: all of the window's, once it is `complete`.
    panes: Vec<BuiltPane>,
    /// Whether every pane of the window was made and its layout applied.
    complete: bool,
}

/// A pane [`create_session`] made: its id, and why it is left alone, when tmux did not open it in
/// the directory it was saved in ([`left_alone`]). Decided once, as the pane is made: the planning of
/// resumes and the panes left alone go by it, and look at nothing again.
#[derive(Clone, Debug)]
struct BuiltPane {
    id: String,
    left: Option<Left>,
}

impl BuiltPane {
    /// The pane `id`, made for `saved` once `looked` was what was found where it was saved, and
    /// which tmux was asked to open in `start` as tmux read it.
    fn new(id: String, saved: Option<&PaneSnap>, looked: Looked, start: &str) -> BuiltPane {
        BuiltPane {
            left: saved.and_then(|saved| left_alone(&saved.cwd, looked, start)),
            id,
        }
    }
}

/// `#{window_id}`, the id of the pane that comes with it, and the directory tmux was asked to open
/// that pane in as tmux read it (`#{pane_start_path}`), tab-joined: what `new-session -P -F` and
/// `new-window -P -F` are asked for.
const NEW_WINDOW_FORMAT: &str = "#{window_id}\t#{pane_id}\t#{pane_start_path}";

/// The id of the pane `split-window -P -F` makes, and the directory tmux was asked to open it in, as
/// [`NEW_WINDOW_FORMAT`] has them.
const NEW_PANE_FORMAT: &str = "#{pane_id}\t#{pane_start_path}";

/// The window id, the pane id and the directory of a reply to [`NEW_WINDOW_FORMAT`], each as
/// [`parse_pane`] takes them.
fn parse_ids(reply: &str, command: &str) -> anyhow::Result<(String, String, String)> {
    let mut columns = reply.splitn(3, '\t');
    let (Some(window), Some(pane), Some(start)) = (columns.next(), columns.next(), columns.next())
    else {
        anyhow::bail!(
            "{command} -P did not answer '<window id>\\t<pane id>\\t<directory>': {reply:?}"
        );
    };
    let (window, pane) = (window.trim(), pane.trim());
    anyhow::ensure!(
        !window.is_empty() && !pane.is_empty(),
        "{command} -P answered an empty id: {reply:?}"
    );
    Ok((window.to_string(), pane.to_string(), start.to_string()))
}

/// The pane id and the directory of a reply to [`NEW_PANE_FORMAT`]. An empty id is an error rather
/// than a value: every id here goes straight into a `-t` argument, and tmux reads an empty target as
/// the *current* pane. The directory is all that follows the id, as tmux gave it, a space at its end
/// included, which a folder's name can have; a tmux that does not have the format gives it empty.
fn parse_pane(reply: &str, command: &str) -> anyhow::Result<(String, String)> {
    let (id, start) = reply.split_once('\t').ok_or_else(|| {
        anyhow::anyhow!("{command} -P did not answer '<pane id>\\t<directory>': {reply:?}")
    })?;
    let id = id.trim();
    anyhow::ensure!(
        !id.is_empty(),
        "{command} -P answered an empty id: {reply:?}"
    );
    Ok((id.to_string(), start.to_string()))
}

/// The agent kinds whose resume flag takes a **session file path** rather than an id.
///
/// herdr's own "Native session restore" table has these two reporting a path
/// (`pi --session <path>`, `omp --resume=<path>`), and their integrations save it as
/// `@agent_session_path`. Both also accept an id, so a pane that reported one resumes by
/// id through the ordinary route; this list is only about which panes may fall back to
/// the path when there is no usable id.
///
/// An allowlist rather than "try the path for anything": `claude --resume <path>` would
/// type a command claude rejects, so guessing would turn a pane that restores as a
/// clean shell into one holding a failed command.
const PATH_RESUME_KINDS: &[&str] = &["pi", "omp"];

/// Which saved value a pane should be resumed from, if any.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeBy<'a> {
    /// `@agent_session_id`, the ordinary case.
    Id(&'a str),
    /// `@agent_session_path`, for a kind in [`PATH_RESUME_KINDS`] that saved no usable id.
    Path(&'a str),
}

/// Decides what a saved pane is resumed from: its session id, its session path, or
/// nothing.
///
/// `Err(reason)` means something *was* saved but cannot be used, and the reason goes in
/// the report — a pane like that restores as a plain shell rather than silently, because
/// "my agent did not come back" deserves an explanation.
///
/// The id is preferred whenever it is valid, including for `pi` and `omp`: an id already
/// resumes today, and this must not change a pane that already works. The path is a
/// fallback for the panes that have nothing else.
///
/// Kept as a pure function so the choice is testable without a tmux server, and
/// independent of which arms [`resume_command`] happens to carry.
pub fn resume_target<'a>(kind: &str, saved: &'a PaneSnap) -> Result<Option<ResumeBy<'a>>, String> {
    let id = saved
        .agent_session_id
        .as_deref()
        .filter(|id| !id.is_empty());
    let path = saved
        .agent_session_path
        .as_deref()
        .filter(|path| !path.is_empty());

    if let Some(id) = id {
        if valid_session_id(id) {
            return Ok(Some(ResumeBy::Id(id)));
        }
        // A snapshot can predate `valid_session_id`, or have been written by a hook that
        // never went through it.
        return Err(format!(
            "saved {kind} session id {id:?} is not a valid agent session id"
        ));
    }

    let Some(path) = path else {
        return Ok(None);
    };
    if !PATH_RESUME_KINDS.contains(&kind) {
        return Ok(None);
    }
    if !valid_session_path(path) {
        return Err(format!(
            "saved {kind} session path {path:?} is not a usable session path"
        ));
    }
    Ok(Some(ResumeBy::Path(path)))
}

/// Whether a string is usable as `@agent_session_path`: an absolute path, at most 4096
/// bytes, holding no control character and none of the shell's active punctuation.
///
/// This is a *read-side* guard and it is load-bearing, because unlike `@agent_session_id`
/// the path is written with **no validation at all** (`pane report-agent-session
/// --agent-session-path` sets the option exactly as given). A snapshot can therefore hold
/// whatever a hook put there.
///
/// What the rules are for:
///
/// * **Absolute.** A relative path would resolve against whatever directory the restored
///   pane happens to start in, which is not necessarily the one it was saved from — so it
///   would name a different file, or none.
/// * **No control characters.** The resume command is delivered with `send-keys … Enter`,
///   so an embedded newline would submit the line early and run whatever followed it as a
///   second command. This is the one rule shell quoting cannot cover.
/// * **No shell-active punctuation.** [`resume_command`] shell-quotes the value, which
///   already makes these inert; this is the second, independent guard, exactly as
///   [`valid_session_id`]'s charset is for ids. Spaces are deliberately *allowed* —
///   `~/Library/Application Support/…` is an ordinary place for an agent to keep a session
///   file, and quoting handles it.
pub fn valid_session_path(path: &str) -> bool {
    const FORBIDDEN: &[char] = &[
        '$', '`', ';', '|', '&', '<', '>', '(', ')', '*', '?', '\\', '\'', '"',
    ];
    !path.is_empty()
        && path.len() <= 4096
        && path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || c == '\0' || FORBIDDEN.contains(&c))
}

/// How a saved pane is resumed, if it is: what it is resumed from, and the command that does it.
///
/// `Err(reason)` is a pane that has something saved and cannot be resumed from it, and what the
/// plan says of it. The one decision [`plan_resumes`] and [`Restore::will_resume`] share, so that
/// a pane an agent is resumed into is the pane a saved screen is not painted over.
fn resume_of<'a>(
    kind: &str,
    saved: &'a PaneSnap,
    home: Option<&std::path::Path>,
    read: Read,
) -> Result<Option<(ResumeBy<'a>, String, Vec<String>)>, String> {
    let Some(target) = resume_target(kind, saved)? else {
        return Ok(None);
    };
    // `resume_command` shell-quotes whichever value it is handed, so a path goes in the same
    // slot an id would (`pi --session <value>`).
    let (ResumeBy::Id(value) | ResumeBy::Path(value)) = target;
    // A Claude pane comes back under the profile its transcript is in (`profile_of`).
    let profile = saved
        .agent_session_path
        .as_deref()
        .and_then(|path| profile_of(path, home));
    // The one place the flags are worked out: the command is typed with them and the plan saves them.
    let flags = flags_of(kind, saved, read);
    match resume_line(kind, value, profile.as_deref(), &flags) {
        Some(command) => Ok(Some((target, command, flags))),
        None => Err(format!("no resume command is known for {kind}")),
    }
}

// Follows herdr's src/persist/restore.rs, `pane_restore_startup` and
// `saved_reported_resume`, and src/agent_resume.rs@v0.9.3, the keys of `plan` and `dedupe_key`.
// Differences: the command is typed into the pane's shell as taw's own resume commands are; taw's
// own key is the kind with the session id or path, where herdr's has the integration's source, which
// a snapshot here does not keep.
/// How a saved pane is resumed, if it is: the command the agent reported for itself, when the pane
/// saved one the checks let through, which wins over taw's own resume command for its kind; else taw's
/// own ([`resume_of`]). `Err(reason)` is a pane that saved a session it cannot be resumed from.
///
/// The planning for a pane whose agent ran in no container; [`plan_of`] decides for every
/// pane.
fn plan_outside(saved: &PaneSnap, read: Read) -> Result<Option<Planned>, String> {
    plan_outside_at(saved, dirs::home_dir().as_deref(), read)
}

/// [`plan_outside`] with `home`, the home that Claude's default profile is in, given (the one the
/// restore runs with).
fn plan_outside_at(
    saved: &PaneSnap,
    home: Option<&std::path::Path>,
    read: Read,
) -> Result<Option<Planned>, String> {
    if let Some(resume) = &saved.agent_resume
        && crate::agent_resume::validate_argv(&resume.argv).is_ok()
    {
        return Ok(Some(Planned {
            kind: resume.agent.clone(),
            command: resume.line(),
            session_id: saved
                .agent_session_id
                .clone()
                .filter(|id| valid_session_id(id)),
            session_path: saved
                .agent_session_path
                .clone()
                .filter(|path| valid_session_path(path)),
            key: resume.key(&saved.cwd),
            reported: Some(resume.clone()),
            sandbox: None,
            claude_flags: Vec::new(),
        }));
    }
    let Some(kind) = saved.agent_kind.as_deref() else {
        return Ok(None);
    };
    let Some((target, command, claude_flags)) = resume_of(kind, saved, home, read)? else {
        return Ok(None);
    };
    // A Claude pane resumed by id keeps its saved transcript path with the session, as the reported
    // resume does: the path is all that carries the profile ([`profile_of`]), so a restore that left it
    // off would save the pane without it, and a second crash would bring the pane back logged out.
    let kept_path = (kind == "claude")
        .then(|| saved.agent_session_path.clone())
        .flatten()
        .filter(|path| valid_session_path(path));
    let (session_id, session_path, key) = match target {
        ResumeBy::Id(id) => (
            Some(id.to_string()),
            kept_path,
            format!("{kind}\u{0}id\u{0}{id}"),
        ),
        ResumeBy::Path(path) => (
            None,
            Some(path.to_string()),
            format!("{kind}\u{0}path\u{0}{path}"),
        ),
    };
    Ok(Some(Planned {
        kind: kind.to_string(),
        command,
        session_id,
        session_path,
        key,
        reported: None,
        sandbox: None,
        claude_flags,
    }))
}

/// The one decision [`plan_resumes`] and [`Restore::will_resume`] share. A pane whose agent ran in a
/// sandbox comes back through its profile, with `taw sandbox` and the agent's resume arguments (the
/// command it reported, else taw's own), or not at all; one whose agent ran in a container taw
/// did not start is not resumed; any other as [`plan_outside`] says. `Err(reason)` is
/// what the plan says of a pane that is not.
fn plan_of(restore: &Restore<'_>, saved: &PaneSnap) -> Result<Option<Planned>, String> {
    plan_with(restore.cfg, restore.paths, &restore.taw, saved, Read::Yes)
}

/// [`plan_of`] with what it reads of a restore given: the config, the paths, and how a typed line
/// names taw. The planning of the windows a restore adds to an open session runs before there is a
/// [`Restore`]. `wanted` is `Read::No` for a caller that uses only whether there is a plan, or its key.
fn plan_with(
    cfg: &Config,
    paths: &Paths,
    taw: &str,
    saved: &PaneSnap,
    wanted: Read,
) -> Result<Option<Planned>, String> {
    // The flags are read from the transcript only when the caller uses them and the pane's plan keeps
    // them: a sandboxed or container pane's are thrown away.
    let read = if wanted == Read::Yes
        && saved.agent_sandbox.is_none()
        && saved.agent_container.is_none()
    {
        Read::Yes
    } else {
        Read::No
    };
    let planned = plan_outside(saved, read);
    match (
        saved.agent_sandbox.as_deref(),
        saved.agent_container.as_deref(),
    ) {
        (Some(name), _) => match planned? {
            None => Ok(None),
            Some(planned) => through_profile(cfg, paths, taw, name, planned).map(Some),
        },
        (None, Some(runtime)) => match planned? {
            None => Ok(None),
            Some(planned) => Err(format!(
                "its {} ran in a {runtime} container taw did not start, so it is not started again outside it",
                planned.kind
            )),
        },
        // Every other pane: the planning for a pane outside any container, unchanged.
        (None, None) => planned,
    }
}

/// `planned`, what the planning makes of a pane outside a sandbox, as its agent comes back through the profile `name`:
/// `taw sandbox` and the agent's resume arguments, the words of the command it reported after the
/// agent's own command, else taw's own ([`resume_args`]); `--profile` only when the config would pick
/// another. `Err(reason)` when the profile is not there to run, or the reported command does not start
/// the agent: a sandboxed agent starts only through its profile. A profile that is not there is gone;
/// one that is there and would fail, or that taw cannot look for, is said in the words `taw sandbox`
/// says it in (`profile::Missing`), which name what to fix.
fn through_profile(
    cfg: &Config,
    paths: &Paths,
    taw: &str,
    name: &str,
    planned: Planned,
) -> Result<Planned, String> {
    use crate::sandbox::profile::{self, Missing};
    match profile::find(paths, name) {
        Ok(_) => {}
        Err(Missing::NoSuch { .. }) => return Err(format!("its sandbox profile {name} is gone")),
        Err(Missing::NotExecutable { path, .. }) => {
            return Err(format!(
                "its sandbox profile {name} is not executable (chmod +x {path})"
            ));
        }
        Err(other) => return Err(other.to_string()),
    }
    let kind = planned.kind.as_str();
    let args = match &planned.reported {
        // Its words after the agent's own command go to `taw sandbox` after `--`.
        Some(reported) => match reported.argv.split_first() {
            Some((first, rest)) if *first == crate::detect::executable(kind) => rest.to_vec(),
            _ => {
                return Err(format!(
                    "its reported resume command does not start {kind}, and a sandboxed agent starts only through its profile"
                ));
            }
        },
        None => planned
            .session_id
            .as_deref()
            .or(planned.session_path.as_deref())
            .and_then(|value| resume_args(kind, value))
            .ok_or_else(|| format!("no resume command is known for {kind}"))?,
    };
    let flag = (profile::pick(&cfg.sandbox, kind, None) != name).then_some(name);
    let command = crate::sandbox::typed::line(taw, kind, flag, &args);
    Ok(Planned {
        command,
        sandbox: Some(name.to_string()),
        // The agent's flags are the container's, as its profile is.
        claude_flags: Vec::new(),
        ..planned
    })
}

/// What [`plan_of`] plans for a saved pane.
struct Planned {
    /// What the command resumes, and the pane's `@agent_kind_hint` once it is typed.
    kind: String,
    command: String,
    /// The session id and path the pane gets back once the command is typed, when it saved them.
    session_id: Option<String>,
    session_path: Option<String>,
    /// What a restore resumes once ([`Restore::claim`]).
    key: String,
    /// The command the agent reported for itself, when it is that command that is typed.
    reported: Option<crate::agent_resume::ReportedResume>,
    /// The profile its agent comes back through, for a sandboxed one.
    sandbox: Option<String>,
    /// The flags its Claude was started with that the line carries, which the pane gets back once it
    /// is typed, so that the next save keeps them (none for a sandboxed pane and any other agent).
    claude_flags: Vec<String>,
}

/// A pane whose saved agent is to be resumed: what is typed into it once its shell is at its
/// prompt, and what the pane is marked with when it is.
struct Resume {
    pane_id: String,
    /// The `@agent_kind_hint` the pane gets, and what the command was made for.
    kind: String,
    /// The `@agent_name` it gets, when one was saved.
    agent_name: Option<String>,
    /// The `@agent_session_id` it gets, when the command was made from an id, or the agent reported a
    /// command of its own and the pane saved one.
    session_id: Option<String>,
    /// The `@agent_session_path` it gets, as the id.
    session_path: Option<String>,
    command: String,
    /// The command the agent reported, which the pane gets back once it is typed, so that the next save
    /// keeps it.
    reported: Option<crate::agent_resume::ReportedResume>,
    /// The profile its agent comes back through, for a sandboxed one.
    sandbox: Option<String>,
    /// The flags the line carries, put back on the pane once it is typed ([`Planned::claude_flags`]).
    claude_flags: Vec<String>,
    /// The wait for the pane's shell, which runs from when the pane was made.
    waiter: prompt::Waiter,
    /// Why the pane is left alone, when tmux did not open it in its saved directory ([`left_alone`]):
    /// it is not waited for, the command is not typed, and [`resume_agents`] says so, with the command
    /// to run from that directory.
    left: Option<Left>,
}

/// For every saved pane that carries a command its agent reported, or an agent kind and something
/// to resume from, works out the command that resumes it ([`plan_of`]), in the pane that was created
/// for it.
///
/// Panes are matched to their live counterpart by the ids [`create_session`] captured as
/// it built them, so a duplicate or dotted window name cannot send an agent's resume
/// command into another window's pane. [`resume_target`] decides whether the pane resumes
/// from its saved session id or (for `pi`/`omp`) its saved session path, and anything it
/// refuses gets a line in the report instead of resuming. A session an earlier pane of this
/// restore resumes is not resumed again: the plan says which pane has it. Nor is a session that a pane of
/// the server runs already, in a window added to an open session or in a session made whole.
///
/// Nothing is typed here: [`resume_agents`] does that, once every session is made and each
/// pane's shell is at its prompt. A pane that tmux did not open in its saved directory, which was
/// decided as it was made, is not planned here ([`leave_alone_panes_opened_elsewhere`]).
fn plan_resumes(
    restore: &Restore<'_>,
    session: &SessionSnap,
    built: &[Built],
    report: &mut Report,
) -> Vec<Resume> {
    // The panes were all made within the last moment: each has its bound from now.
    let made = std::time::Instant::now();
    let mut resumes = Vec::new();
    for (window, built) in session.windows.iter().zip(built) {
        for (saved, pane) in window.panes.iter().zip(&built.panes) {
            if pane.left.is_some() {
                continue;
            }
            let pane_id = &pane.id;
            let planned = match plan_of(restore, saved) {
                Ok(Some(planned)) => planned,
                Ok(None) => continue,
                Err(reason) => {
                    report
                        .plan
                        .push(format!("skip resume in pane {pane_id}: {reason}"));
                    continue;
                }
            };
            // Nothing is resumed that runs in a pane of the server already, in a window added to an open
            // session or in a session made whole.
            if let Some(running) = restore.running_in_the_server(&planned) {
                report.plan.push(format!(
                    "skip resume in pane {pane_id}: pane {running} already runs the same session"
                ));
                continue;
            }
            if let Some(first) = restore.claim(&planned.key, pane_id) {
                report.plan.push(format!(
                    "skip resume in pane {pane_id}: pane {first} resumes the same session"
                ));
                continue;
            }
            resumes.push(Resume {
                pane_id: pane_id.clone(),
                kind: planned.kind,
                agent_name: saved.agent_name.clone(),
                session_id: planned.session_id,
                session_path: planned.session_path,
                command: planned.command,
                reported: planned.reported,
                sandbox: planned.sandbox,
                claude_flags: planned.claude_flags,
                waiter: prompt::Waiter::new(pane_id, made, restore.wait),
                left: None,
            });
        }
    }
    resumes
}

// Follows herdr's src/persist/restore.rs, `restore_tab`'s unavailable pane and
// `unavailable_restored_terminal`, and src/app/agent_resume.rs@v0.9.3, `start_pending_agent_resume`'s
// missing directory. Differences: tmux has made the pane, in the home folder, by the time its
// directory is found gone, where herdr starts no shell: nothing is typed into it, and the plan says so,
// with the command to run once the directory is back. A pane whose directory is there and that tmux
// opened somewhere else all the same, which herdr does not have, is left alone as a gone one is.
/// The panes of `built` that tmux did not open in their saved directory ([`left_alone`]). tmux opens a
/// pane whose saved directory is gone, or cannot be opened, in the home folder and says nothing, and one
/// whose directory it read as another in that other, and taw cannot stop it; but nothing is typed into
/// such a pane, so that an agent's conversation is not resumed in a directory that is not its own. Each
/// is marked with herdr's error ([`UNAVAILABLE`], `@agent_restore_error`), whether agents are resumed or
/// not, and the plan says which pane, which directory, and where tmux opened it. A pane with an agent to
/// resume is returned, for [`resume_agents`] to say so with the command to run from that directory, in
/// the plan and in `not_resumed`, with the other panes it leaves alone. As in herdr, such a pane takes no
/// part in which pane resumes a session that two saved ([`Restore::claim`]).
fn leave_alone_panes_opened_elsewhere(
    t: &Tmux,
    restore: &Restore<'_>,
    session: &SessionSnap,
    built: &[Built],
    report: &mut Report,
) -> Vec<Resume> {
    let made = std::time::Instant::now();
    let mut left = Vec::new();
    for (window, built) in session.windows.iter().zip(built) {
        for (saved, pane) in window.panes.iter().zip(&built.panes) {
            let Some(why) = &pane.left else {
                continue;
            };
            mark_unavailable(t, &pane.id, saved);
            let planned = restore
                .cfg
                .session
                .resume_agents_on_restore
                .then(|| plan_of(restore, saved).ok().flatten())
                .flatten();
            let Some(planned) = planned else {
                report.plan.push(why.line(&pane.id, None));
                continue;
            };
            left.push(Resume {
                pane_id: pane.id.clone(),
                kind: planned.kind,
                agent_name: saved.agent_name.clone(),
                session_id: planned.session_id,
                session_path: planned.session_path,
                command: planned.command,
                reported: planned.reported,
                sandbox: planned.sandbox,
                claude_flags: planned.claude_flags,
                waiter: prompt::Waiter::new(&pane.id, made, restore.wait),
                left: Some(why.clone()),
            });
        }
    }
    left
}

/// What a restore finds where a pane was saved, looked at once before tmux is asked to open the pane
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Looked {
    /// A directory that can be opened, or no directory saved to look for.
    There,
    /// No directory: nothing is there, or something that is not a directory (it was removed or
    /// renamed, or its disk is not mounted), which herdr asks of every pane it restores.
    Gone,
    /// A directory that cannot be opened: there is no permission to go into it or into a folder above
    /// it. tmux opens a pane in your home folder for it, as for a gone one.
    Shut,
}

/// What is at `dir` for a pane to open in ([`Looked`]). A pane saved with no directory (tmux could not
/// read where its process was) has none to look for, and opens where tmux puts it, as it did.
fn look(dir: &str) -> Looked {
    if dir.is_empty() {
        return Looked::There;
    }
    if !std::path::Path::new(dir).is_dir() {
        return Looked::Gone;
    }
    let Ok(path) = std::ffi::CString::new(dir) else {
        return Looked::Gone;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call, which only reads it.
    if unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0 {
        Looked::There
    } else {
        Looked::Shut
    }
}

/// Why a restore leaves a pane alone: tmux did not open it in the directory it was saved in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Left {
    /// The directory the pane was saved in.
    saved: String,
    /// Whether that directory is gone, rather than there and not opened.
    gone: bool,
    /// Where tmux opened the pane instead: the directory it was asked for as tmux read it, when that is
    /// not the saved one and can be opened, else (`None`) your home folder.
    opened_in: Option<String>,
}

impl Left {
    /// What the plan says of the pane `pane_id`, left alone: why, and where tmux opened it. With the
    /// `command` its agent was to be resumed with, nothing was typed, and the line says to run the
    /// command from the saved directory, once it is back when it is gone.
    fn line(&self, pane_id: &str, command: Option<&str>) -> String {
        let (dir, opened_in) = (
            &self.saved,
            self.opened_in.as_deref().unwrap_or("your home folder"),
        );
        if self.gone {
            let opened = format!(
                "pane {pane_id}: saved directory {dir} is gone, so tmux opened it in {opened_in}"
            );
            match command {
                None => opened,
                Some(command) => format!(
                    "{opened} and nothing was typed; once the directory is back, resume it with: {}",
                    from_directory(dir, command)
                ),
            }
        } else {
            let opened = format!(
                "pane {pane_id}: tmux opened it in {opened_in} and not in its saved directory {dir}"
            );
            match command {
                None => opened,
                Some(command) => format!(
                    "{opened}, so nothing was typed; resume it with: {}",
                    from_directory(dir, command)
                ),
            }
        }
    }
}

/// Why a restore leaves alone the pane saved in `saved`, if it does: tmux did not open the pane there.
/// `looked` is what was found there before tmux was asked to open it ([`look`]), and `start` is where
/// tmux was asked to open it as tmux read the request (`#{pane_start_path}`): a directory tmux reads as
/// another (a `#` it takes for a format) differs, and is empty from a tmux that does not have the
/// format, which leaves `looked` to decide alone. A pane saved with no directory is never left alone: it
/// opens where tmux puts it, as it did.
fn left_alone(saved: &str, looked: Looked, start: &str) -> Option<Left> {
    if saved.is_empty() {
        return None;
    }
    let opened_in = if !start.is_empty() && start != saved {
        (look(start) == Looked::There).then(|| start.to_string())
    } else if looked == Looked::There {
        return None;
    } else {
        None
    };
    Some(Left {
        saved: saved.to_string(),
        gone: looked == Looked::Gone,
        opened_in,
    })
}

/// Marks `pane_id`, which tmux did not open in its saved directory, with herdr's error ([`UNAVAILABLE`])
/// and with what was saved of it (`@agent_restore_saved`), which every save keeps in its place until the
/// pane is closed or an agent of its own is recorded in it (`snapshot::take`, `store::RESTORE_OPTIONS`),
/// so that the first autosave after the restore does not forget its directory and its agent's
/// conversation. What is kept is the most of it ([`kept_versions`]) that one tmux command
/// can carry. A pane closed since is left so, and anything else tmux says is logged: the restore goes on,
/// nothing is typed into the pane either way, and the plan says so.
fn mark_unavailable(t: &Tmux, pane_id: &str, saved: &PaneSnap) {
    for kept in kept_versions(saved) {
        match crate::store::mark_restore_error(t, pane_id, UNAVAILABLE, kept.as_deref()) {
            Ok(()) => return,
            Err(err) if err.chain().any(|cause| cause.is::<crate::store::TooLong>()) => {
                log::debug!("pane {pane_id} keeps less of what was saved of it: {err:#}");
            }
            Err(err) => {
                if !prompt::is_gone(&err) {
                    log::warn!(
                        "marking pane {pane_id}, which was not opened in its saved directory: {err:#}"
                    );
                }
                return;
            }
        }
    }
}

/// What a pane left alone can keep of what was saved of it, as the text a save reads back (the pane's
/// JSON), in the order a restore tries them: all of it; then, for a pane whose agent reported its own
/// resume command, all but that command, the one part of a saved pane that the checks let run to
/// kilobytes (8 KiB of words, which JSON can double); then nothing, for a pane whose saved directory or
/// session is itself too long for one tmux command, which no tmux or agent writes. Each version that
/// keeps the session keeps the saved profile and runtime beside it: a later restore comes back
/// through the profile, or not at all, whatever the pane ran in between. The pane is not marked with
/// the profile itself: nothing runs in it.
fn kept_versions(saved: &PaneSnap) -> Vec<Option<String>> {
    let mut versions = vec![serde_json::to_string(saved).ok()];
    if saved.agent_resume.is_some() {
        let without = PaneSnap {
            agent_resume: None,
            ..saved.clone()
        };
        versions.push(serde_json::to_string(&without).ok());
    }
    versions.push(None);
    versions
}

/// The line that runs `command` from `dir`, for a pane that is somewhere else: `cd` and the directory,
/// quoted as the words of a reported command are ([`crate::agent_resume::command_line`]), then the
/// command.
fn from_directory(dir: &str, command: &str) -> String {
    format!(
        "{} && {command}",
        crate::agent_resume::command_line(&["cd".to_string(), dir.to_string()])
    )
}

/// Types each resume command into its pane, once the pane's shell is at its prompt.
///
/// What is typed at a shell that is still starting goes to whatever reads the terminal first:
/// mise asked "Trust them? Yes No All" in a restored pane, and the `a` in `claude` answered
/// `All` (see [`prompt`]). So every pane is looked at first, and the command is typed as soon
/// as its shell is ready, not when the slowest shell is: the panes are looked at in turn until
/// each has been typed into or given up on, and a pane whose shell has not come to its prompt
/// within [`prompt::BOUND`] of being made is left alone and said so, in plain words, in the
/// plan, with the command to run by hand. All the panes of every session wait together, each
/// from when it was made, so panes that never get there cost one bound between them, not one
/// each.
///
/// A pane closed before its turn is the same: nothing is typed, and the plan says so. Neither
/// is marked as holding an agent: nothing was started in it. Nor is a pane that tmux did not open
/// in its saved directory, which is not waited for: the plan says so first, with the command to run
/// from that directory ([`leave_alone_panes_opened_elsewhere`]).
///
/// Once every command is typed, the pid of what each started is recorded ([`record_agent_pids`]).
fn resume_agents(
    t: &Tmux,
    restore: &Restore<'_>,
    pending: Vec<Resume>,
    report: &mut Report,
) -> anyhow::Result<()> {
    let first = report.resumed.len();
    let (left, mut pending): (Vec<Resume>, Vec<Resume>) = pending
        .into_iter()
        .partition(|resume| resume.left.is_some());
    for resume in left {
        if let Some(why) = &resume.left {
            report
                .plan
                .push(why.line(&resume.pane_id, Some(&resume.command)));
        }
        report.not_resumed.push(resume.pane_id);
    }
    // The sandboxed resumes typed, for the check that each line started.
    let mut sandboxed = Vec::new();
    while !pending.is_empty() {
        let mut waiting = Vec::with_capacity(pending.len());
        for mut resume in pending {
            match resume.waiter.look(t)? {
                None => waiting.push(resume),
                Some(prompt::Outcome::Ready) => {
                    type_resume(t, restore, &resume, report)?;
                    if resume.sandbox.is_some() && report.resumed.last() == Some(&resume.pane_id) {
                        sandboxed.push(resume);
                    }
                }
                Some(prompt::Outcome::TimedOut) => {
                    report
                        .plan
                        .push(not_ready_line(&resume.pane_id, &resume.command));
                    report.not_resumed.push(resume.pane_id);
                }
                Some(prompt::Outcome::Gone) => {
                    report
                        .plan
                        .push(gone_line(&resume.pane_id, &resume.command));
                    report.not_resumed.push(resume.pane_id);
                }
                Some(prompt::Outcome::NoInput(why)) => {
                    report
                        .plan
                        .push(no_input_line(&resume.pane_id, &why, false, &resume.command));
                    report.not_resumed.push(resume.pane_id);
                }
            }
        }
        pending = waiting;
        if !pending.is_empty() {
            std::thread::sleep(restore.wait.poll);
        }
    }
    let started = record_agent_pids(t, restore, &report.resumed[first..]);
    for resume in sandboxed {
        if !started.contains(&resume.pane_id) {
            take_off_a_start_that_did_not_happen(t, &resume, report);
        }
    }
    Ok(())
}

/// Takes a restore's marks off the pane of `resume`, a sandboxed one whose typed `taw sandbox` line
/// started nothing the looks of [`record_agent_pids`] found, while its shell is in front and the pane
/// holds the restore's profile mark with no process (`store::forget_a_restored_start`): the line was
/// refused before it marked the pane, as `taw agent start --sandbox` takes its mark off when its start
/// did not happen. The plan says so, with the command, and the pane is not resumed. Best-effort: a pane
/// closed since, or anything tmux says, leaves it as it is.
fn take_off_a_start_that_did_not_happen(t: &Tmux, resume: &Resume, report: &mut Report) {
    let Some(profile) = &resume.sandbox else {
        return;
    };
    let pane_id = &resume.pane_id;
    match crate::store::forget_a_restored_start(t, pane_id, profile, resume.waiter.command()) {
        Ok(true) => {
            report.resumed.retain(|pane| pane != pane_id);
            report
                .plan
                .push(not_started_line(pane_id, &resume.kind, &resume.command));
            report.not_resumed.push(pane_id.clone());
        }
        Ok(false) => {}
        Err(err) => log::debug!("looking at whether {pane_id} started its sandbox: {err:#}"),
    }
}

/// How long a restore looks, once every command is typed, for the process each of them started: what
/// a shell runs before the command (the helper of a preexec hook) has ended by then, and the command
/// itself has started, for all of them together.
const AGENT_PID_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// Records `@agent_pid` for each pane in `panes`, which a restore typed the resume command of: the process
/// the command started, as `taw agent start` records the pid of the agent it starts. A pane whose agent a
/// restore resumed is named by its hint alone when the agent's process is a wrapper or an npm install,
/// and the pid is what says that process is in front: with none, an agent that had exited left a pane
/// that nothing could tell from one whose user had started a shell, a pager or `cat`, and what `taw
/// agent prompt` types would reach it.
///
/// The process is the newest child of the pane's shell that is in front of the pane's terminal
/// ([`crate::watch::newest_children_in_front`]), and the first such child is not always it: a shell that
/// runs a helper before every command (atuin's and starship's preexec hooks, bash-preexec, a DEBUG trap)
/// starts the helper first, and the helper is gone a moment later. So the panes are looked at together,
/// every [`Timing::poll`](prompt::Timing), until [`AGENT_PID_BOUND`] (the restore's own bound, when that
/// is shorter) has run out from the last command typed, three times at the least however long a look
/// takes, and past the bound until the last two looks agree or until the sixth
/// ([`crate::watch::looked_enough`]), and each records the child that the last two looks both found. A
/// pane that is gone, or has no such child by then, is left as it is. The panes are looked at together,
/// one `pgrep` and one `ps` a look for them all, so however many agents a restore resumes, a busy machine
/// lengthens it by its looks up to the sixth at the most, once. Gives back the panes a child was recorded
/// for.
fn record_agent_pids(t: &Tmux, restore: &Restore<'_>, panes: &[String]) -> HashSet<String> {
    let shells: Vec<(&str, u32)> = panes
        .iter()
        .filter_map(|pane| {
            let shell = t.display(pane, "#{pane_pid}").ok()?.trim().parse().ok()?;
            Some((pane.as_str(), shell))
        })
        .collect();
    if shells.is_empty() {
        return HashSet::new();
    }
    let pids: Vec<u32> = shells.iter().map(|(_, shell)| *shell).collect();
    let deadline = std::time::Instant::now() + AGENT_PID_BOUND.min(restore.wait.bound);
    let steady = steady_children(
        || crate::watch::newest_children_in_front(&pids),
        || std::time::Instant::now() >= deadline,
        || std::thread::sleep(restore.wait.poll),
    );
    let mut found = HashSet::new();
    for (pane, shell) in &shells {
        let Some(pid) = steady.get(shell) else {
            continue;
        };
        found.insert(pane.to_string());
        if let Err(err) = t.set_pane_option(pane, "@agent_pid", &pid.to_string()) {
            log::debug!("recording @agent_pid on {pane}: {err:#}");
        }
    }
    found
}

/// The child of each shell that the last two looks both found ([`agreed`]): `look` is one look at the
/// children in front of them all, made again after `pause` until `over` says the bound has run out,
/// [`crate::watch::LOOKS_AT_THE_LEAST`] times at the least, and past it while the last two looks disagree
/// for any shell, until look [`crate::watch::LOOKS_AT_THE_MOST`] ([`crate::watch::looked_enough`]). A child the last look found and the one before did not (a helper
/// the shell ran, caught on its way, or the agent's process in place of one) is not taken for any.
fn steady_children(
    mut look: impl FnMut() -> std::collections::HashMap<u32, u32>,
    mut over: impl FnMut() -> bool,
    mut pause: impl FnMut(),
) -> std::collections::HashMap<u32, u32> {
    let mut last = std::collections::HashMap::new();
    let mut looks = 0;
    loop {
        let found = look();
        looks += 1;
        let steady = agreed(&last, &found);
        let agree = found == last;
        last = found;
        if crate::watch::looked_enough(looks, &mut over, agree) {
            return steady;
        }
        pause();
    }
}

/// Of the children one look `found` for each shell, those the look before it, `last`, found too.
fn agreed(
    last: &std::collections::HashMap<u32, u32>,
    found: &std::collections::HashMap<u32, u32>,
) -> std::collections::HashMap<u32, u32> {
    found
        .iter()
        .filter(|(shell, child)| last.get(*shell) == Some(*child))
        .map(|(shell, child)| (*shell, *child))
        .collect()
}

// Follows herdr's src/app/agent_resume.rs, commit 46a5091838 (#4487,
// #4102 in v0.9.2's changelog).
// Differences: behaviour only. herdr's async scheduler starts the first resume at once and
// then one per startup_per_agent_delay_ms; restore here is one blocking call, so the same
// spacing is a plain sleep before every resume after the first.
/// Sets `@agent_kind_hint` (and `@agent_name`, when saved) on a pane whose shell is at its
/// prompt and types its resume command into it.
///
/// Successive resumes, across every session `apply()` recreates and not only this one, are
/// spaced `[session] startup_per_agent_delay_ms` apart ([`Restore::resumed_any`]), ported
/// from herdr v0.9.2's resume scheduler, `46a5091838` (#4487, #4102 in v0.9.2's changelog): several agents launching in
/// the very same instant can overload whatever they call out to.
///
/// The command is typed as text (`send-keys -l`), so that a command of one word that tmux reads as
/// the name of a key (`Up`, `Home`, `F1`) is typed and not pressed, and then Enter, each only while
/// the shell the wait found at its prompt is still in front of the pane and the pane takes input
/// ([`send_resume`]): the wait looked at the pane before the spacing, and a program started in it
/// since would be given the command and the Enter. A command held back so is not typed, or not run:
/// the pane is left as it is, with the marks of an agent taken off it again, and the plan says which
/// pane it is and the command to run there.
///
/// A sandboxed resume sets no hint, and no kind, session or reported command of its own: those are
/// in the kept resume, written with the profile's mark in one tmux command ([`mark_sandbox_resume`]).
fn type_resume(
    t: &Tmux,
    restore: &Restore<'_>,
    resume: &Resume,
    report: &mut Report,
) -> anyhow::Result<()> {
    let delay_ms = restore.cfg.session.startup_per_agent_delay_ms;
    if delay_ms > 0 && restore.resumed_any.replace(true) {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(delay_ms)));
    }

    let pane_id = &resume.pane_id;
    let marks = (|| {
        // A sandboxed agent is named by the container taw sees, never by a hint: a hint names its
        // agent whatever runs in the pane, so after the container ended the pane would still read
        // as that agent.
        if resume.sandbox.is_none() {
            t.set_pane_option(pane_id, "@agent_kind_hint", &resume.kind)?;
        }
        if let Some(agent_name) = &resume.agent_name {
            crate::store::set_text_option(t, pane_id, "@agent_name", agent_name)?;
        }
        // Follows herdr's src/terminal/state.rs, the acquisition a restored
        // agent's process starts (#4457). Differences: marked before the command is typed, as
        // `taw agent start` marks the agent it launches: its first `idle` is no finished turn until it
        // is seen working (`store::ACQUIRING_OPTION`), from a new pane, which has no turn stamp.
        t.set_pane_option(
            pane_id,
            crate::store::ACQUIRING_OPTION,
            &crate::store::Acquired::At(None).value(),
        )
    })();
    let sent =
        marks.and_then(|()| send_resume(t, pane_id, &resume.command, resume.waiter.command()));
    let left_alone = match sent {
        Ok(None) => None,
        // Closed since its shell was seen at its prompt, or while it was marked. A send to a pane that
        // is gone is held back as well: tmux runs the `if-shell` of a guarded send for a pane it cannot
        // find, and finds no shell in front of it.
        Ok(Some(_)) | Err(_) if pane_gone(t, pane_id) => Some(gone_line(pane_id, &resume.command)),
        Err(err) if prompt::is_gone(&err) => Some(gone_line(pane_id, &resume.command)),
        Ok(Some(line)) => {
            unmark(t, pane_id);
            Some(line)
        }
        Err(err) => return Err(err),
    };
    if let Some(line) = left_alone {
        report.plan.push(line);
        report.not_resumed.push(pane_id.clone());
        return Ok(());
    }
    // The pane holds the agent now that its command is typed, as `agent start` marks it the
    // moment it has identified the agent. Until the watcher does, a snapshot taken from here
    // (a hook's autosave, or the one a restore makes when it is done) would otherwise hold a
    // pane with a hint and a name and no agent, and the next restore would not resume it.
    //
    // It holds what it was resumed from as well: the id, or, for `pi` and `omp` resumed by the
    // path of their session file, the path. The snapshot the restore read holds it, but a
    // snapshot is taken of the panes as they are, and a pane that does not hold it is saved
    // without it: the next restore would find neither an id nor a path, and resume nothing.
    let marked = (|| {
        // A sandboxed agent's kind, session and reported command go on the pane only inside the kept
        // resume, in the one tmux command that marks the pane with its profile: what a save finds
        // beside the mark, and what goes with it, whoever ends it. Written plain, the typed `taw
        // sandbox` could mark the pane and its mark end (a profile the system will not run, one that
        // ends at once and the watcher sees go) before they landed, and the pane would hold the
        // session with no profile beside it, which a save keeps and a restore types outside. The mark has no process (`@agent_sandbox_pid`), so the watcher keeps it until the
        // typed `taw sandbox` marks the pane again with its own.
        if let Some(profile) = &resume.sandbox {
            let kept = crate::session::snapshot::KeptResume {
                kind: resume.kind.clone(),
                session_id: resume.session_id.clone(),
                session_path: resume.session_path.clone(),
                agent_resume: resume.reported.clone(),
            };
            return mark_sandbox_resume(t, pane_id, profile, kept);
        }
        t.set_pane_option(pane_id, "@agent_kind", &resume.kind)?;
        if let Some(id) = &resume.session_id {
            t.set_pane_option(pane_id, "@agent_session_id", id)?;
        }
        if let Some(path) = &resume.session_path {
            t.set_pane_option(pane_id, "@agent_session_path", path)?;
        }
        if let Some(reported) = &resume.reported {
            crate::store::set_resume(t, pane_id, reported)?;
        }
        if !resume.claude_flags.is_empty() {
            t.set_pane_option(
                pane_id,
                crate::claude_flags::OPTION,
                &crate::claude_flags::encode(&resume.claude_flags),
            )?;
        }
        Ok(())
    })();
    match marked {
        Ok(()) => {}
        // Closed already, with the command typed: nothing is left to carry an agent.
        Err(err) if prompt::is_gone(&err) => {}
        Err(err) => return Err(err),
    }
    report.resumed.push(pane_id.clone());
    Ok(())
}

/// Marks `pane_id`, where a restore types a sandboxed agent's line, with its `profile` and with `whole`,
/// what it is resumed with (`store::SANDBOX_RESUME_OPTION`), in one tmux invocation: the one
/// place a sandboxed resume's kind, session and reported command are written, read by a save only
/// beside the mark and taken off with it. All of it, else, for a command the agent
/// reported that is too long for one tmux command beside the rest, all but that command, as a pane
/// left alone keeps what it can ([`kept_versions`]); else the profile alone.
fn mark_sandbox_resume(
    t: &Tmux,
    pane_id: &str,
    profile: &str,
    whole: crate::session::snapshot::KeptResume,
) -> anyhow::Result<()> {
    let mut versions = vec![whole.clone()];
    if whole.agent_resume.is_some() {
        versions.push(crate::session::snapshot::KeptResume {
            agent_resume: None,
            ..whole
        });
    }
    for kept in versions {
        let kept = serde_json::to_string(&kept)?;
        match crate::store::mark_restored_sandbox(t, pane_id, profile, &kept) {
            Ok(()) => return Ok(()),
            Err(err) if err.chain().any(|cause| cause.is::<crate::store::TooLong>()) => {
                log::debug!("pane {pane_id} keeps less of what its restore resumed: {err:#}");
            }
            Err(err) => return Err(err),
        }
    }
    t.set_pane_option(pane_id, crate::store::SANDBOX_OPTION, profile)
}

/// Types `command` into `pane_id`, and then Enter, through the guarded send `taw agent prompt` types with
/// ([`crate::cli::common::send_text_if_in_front`]): each only while the command in front of the pane is
/// still `seen`, the shell's own as the wait found it at its prompt, and the pane takes input, which
/// tmux is asked in the invocation that sends. `None` once both are sent; else what the plan says of the
/// pane, which was left alone: nothing was typed, or the command was typed and not run.
fn send_resume(
    t: &Tmux,
    pane_id: &str,
    command: &str,
    seen: &str,
) -> anyhow::Result<Option<String>> {
    use crate::cli::common::{Sent, send_enter_if_in_front, send_text_if_in_front};
    match send_text_if_in_front(t, pane_id, command, seen)? {
        Sent::Typed => {}
        Sent::HeldBack { typed } => return Ok(Some(held_back_line(pane_id, typed, command))),
        Sent::NoInput { why, typed } => {
            return Ok(Some(no_input_line(pane_id, &why, typed, command)));
        }
    }
    Ok(match send_enter_if_in_front(t, pane_id, seen)? {
        Sent::Typed => None,
        Sent::HeldBack { .. } => Some(held_back_line(pane_id, true, command)),
        Sent::NoInput { why, .. } => Some(no_input_line(pane_id, &why, true, command)),
    })
}

/// Whether `pane_id` is gone: what a send that failed is asked, since the guarded send gives tmux's
/// answer in the words of its error alone.
fn pane_gone(t: &Tmux, pane_id: &str) -> bool {
    match t.display(pane_id, "#{pane_id}") {
        Ok(id) => id.trim().is_empty(),
        Err(err) => prompt::is_gone(&err),
    }
}

/// Takes off `pane_id` the marks of an agent [`type_resume`] put on it before its command, which was not
/// run: nothing was started in it. In one tmux invocation, and best-effort: the plan says what was left.
fn unmark(t: &Tmux, pane_id: &str) {
    let mut args = Vec::new();
    for option in [
        "@agent_kind_hint",
        "@agent_name",
        crate::store::ACQUIRING_OPTION,
    ] {
        if !args.is_empty() {
            args.push(";");
        }
        args.extend(["set-option", "-u", "-p", "-t", pane_id, option]);
    }
    if let Err(err) = t.run_ok(&args) {
        log::debug!("taking the marks of an agent off {pane_id}: {err:#}");
    }
}

/// What the plan says of a pane whose shell had another program in front of it when its command was to
/// be typed (`typed` false), and so was typed nothing, or after some of it (a piece of a long command) or
/// when its Enter was to be pressed (`typed` true).
fn held_back_line(pane_id: &str, typed: bool, command: &str) -> String {
    if typed {
        format!(
            "pane {pane_id} had another program in front of its shell when its command was to be run, \
             and the command typed into it may be left on its line; resume it with: {command}"
        )
    } else {
        format!(
            "pane {pane_id} had another program in front of its shell when its command was to be typed, \
             so nothing was typed; resume it with: {command}"
        )
    }
}

/// What the plan says of a sandboxed pane whose typed `taw sandbox` line started nothing: its shell is
/// back at its prompt, as `taw agent start --sandbox` says of a start that did not happen.
fn not_started_line(pane_id: &str, kind: &str, command: &str) -> String {
    format!(
        "{kind} did not start in its sandbox in pane {pane_id}: its shell is back at its prompt \
         (taw pane read {pane_id} shows what the profile said); resume it with: {command}"
    )
}

/// What the plan says of a pane whose shell never came to its prompt, and so was typed nothing.
fn not_ready_line(pane_id: &str, command: &str) -> String {
    format!("pane {pane_id} did not reach its prompt; resume it with: {command}")
}

/// What the plan says of a pane that was closed before its command could be typed.
fn gone_line(pane_id: &str, command: &str) -> String {
    format!("pane {pane_id} was closed before it reached its prompt; resume it with: {command}")
}

/// What the plan says of a pane that takes no input, so nothing was typed into it: one a person put in
/// copy mode, say, while the restore waited for its shell. Or nothing more, once the command was typed
/// (`typed`), which may be left on its line.
fn no_input_line(pane_id: &str, why: &prompt::NoInput, typed: bool, command: &str) -> String {
    if typed {
        format!(
            "{}, and the command typed into it may be left on its line; resume it with: {command}",
            why.of_pane(pane_id)
        )
    } else {
        format!("{}; resume it with: {command}", why.of_pane(pane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_that_still_changes_at_the_sixth_look_is_recorded_for_no_pane() {
        // Two shells: the first's child is found at every look; the second's is another at every look,
        // and the bound runs out at the third. The looks go on while the second's disagree, to six.
        let made = std::cell::Cell::new(0_u32);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                assert!(made.get() <= 6, "a seventh look");
                std::collections::HashMap::from([(500, 1200), (600, 1300 + made.get())])
            },
            || made.get() >= 3,
            || {},
        );
        assert_eq!(made.get(), 6);
        assert_eq!(steady, std::collections::HashMap::from([(500, 1200)]));
        // A bound that has run out at once still has three looks made, the last two of which can agree.
        let made = std::cell::Cell::new(0);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                std::collections::HashMap::from([(500, 1200)])
            },
            || true,
            || {},
        );
        assert_eq!(made.get(), 3);
        assert_eq!(steady, std::collections::HashMap::from([(500, 1200)]));
    }

    /// Looks so slow that the bound has run out after the first, which caught the helper, then nothing
    /// while the agent came up, then the agent: three that never agree. A fourth finds the agent again, and
    /// the agent is recorded.
    #[test]
    fn a_helper_then_nothing_then_the_agent_twice_is_the_agent() {
        const HELPER: u32 = 1199;
        const AGENT: u32 = 1200;
        let mut looks = vec![
            std::collections::HashMap::from([(500, HELPER)]),
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(500, AGENT)]),
            std::collections::HashMap::from([(500, AGENT)]),
        ]
        .into_iter();
        let made = std::cell::Cell::new(0);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                looks.next().expect("a fifth look")
            },
            || true,
            || {},
        );
        assert_eq!(made.get(), 4);
        assert_eq!(steady, std::collections::HashMap::from([(500, AGENT)]));
    }

    /// Looks that never agree, past the bound: six are made, and nothing is recorded.
    #[test]
    fn six_looks_that_never_agree_record_nothing() {
        let made = std::cell::Cell::new(0_u32);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                assert!(made.get() <= 6, "a seventh look");
                std::collections::HashMap::from([(500, 1200 + made.get())])
            },
            || true,
            || {},
        );
        assert_eq!(made.get(), 6);
        assert!(steady.is_empty(), "{steady:?}");
    }

    /// Looks so slow that the bound has run out after the first: it caught the helper the shell ran before
    /// the command, the second the agent, and two that disagree record nothing. The third finds the agent
    /// again, and the agent is recorded.
    #[test]
    fn looks_that_outlast_the_bound_are_made_three_times() {
        const HELPER: u32 = 1199;
        const AGENT: u32 = 1200;
        let mut looks = vec![
            std::collections::HashMap::from([(500, HELPER)]),
            std::collections::HashMap::from([(500, AGENT)]),
            std::collections::HashMap::from([(500, AGENT)]),
        ]
        .into_iter();
        let made = std::cell::Cell::new(0);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                looks.next().unwrap_or_default()
            },
            || true,
            || {},
        );
        assert_eq!(made.get(), 3);
        assert_eq!(steady, std::collections::HashMap::from([(500, AGENT)]));
        // Quick looks: the bound, not the floor, ends them, and the floor adds none.
        let made = std::cell::Cell::new(0);
        let steady = steady_children(
            || {
                made.set(made.get() + 1);
                std::collections::HashMap::from([(500, AGENT)])
            },
            || made.get() >= 7,
            || {},
        );
        assert_eq!(made.get(), 7);
        assert_eq!(steady, std::collections::HashMap::from([(500, AGENT)]));
    }

    #[test]
    fn a_pane_that_takes_no_input_is_named_in_the_plan_with_its_resume_command() {
        let copy_mode = prompt::NoInput::Mode("copy mode".to_string());
        assert_eq!(
            no_input_line("%5", &copy_mode, false, "claude --resume abc"),
            "pane %5 is in copy mode; resume it with: claude --resume abc"
        );
        assert_eq!(
            no_input_line("%5", &copy_mode, true, "claude --resume abc"),
            "pane %5 is in copy mode, and the command typed into it may be left on its line; resume \
             it with: claude --resume abc"
        );
    }

    /// A pid is recorded for a pane only when two looks in a row found the same child in front: the
    /// helper a shell runs before the command, found once on its way out, is not one.
    #[test]
    fn only_a_child_two_looks_in_a_row_found_is_agreed_on() {
        let last = std::collections::HashMap::from([(500, 1200), (600, 1300), (800, 1600)]);
        let found = std::collections::HashMap::from([(500, 1400), (600, 1300), (700, 1500)]);
        assert_eq!(
            agreed(&last, &found),
            std::collections::HashMap::from([(600, 1300)])
        );
        assert!(agreed(&std::collections::HashMap::new(), &found).is_empty());
    }

    #[test]
    fn what_the_plan_says_of_a_pane_left_alone_names_it_and_the_command_to_run() {
        assert_eq!(
            not_ready_line("%12", "claude --resume abc"),
            "pane %12 did not reach its prompt; resume it with: claude --resume abc"
        );
        assert_eq!(
            gone_line("%12", "claude --resume abc"),
            "pane %12 was closed before it reached its prompt; resume it with: claude --resume abc"
        );
        assert_eq!(
            held_back_line("%12", false, "claude --resume abc"),
            "pane %12 had another program in front of its shell when its command was to be typed, so \
             nothing was typed; resume it with: claude --resume abc"
        );
        assert_eq!(
            held_back_line("%12", true, "claude --resume abc"),
            "pane %12 had another program in front of its shell when its command was to be run, and \
             the command typed into it may be left on its line; resume it with: claude --resume abc"
        );
    }

    /// Everything the plan says here is in a person's words, with none of the marks kept out
    /// of anything a user reads: an em-dash, an en-dash, an arrow or a curly quote.
    #[test]
    fn the_lines_about_what_closed_during_a_restore_hold_no_dash_arrow_or_curly_quote() {
        let dup = window_label(&saved_window(0, "dup", 1));
        let dotted = window_label(&saved_window(3, "with.dot", 1));
        let mut all = vec![
            not_ready_line("%7", "claude --resume abc"),
            gone_line("%7", "claude --resume abc"),
            held_back_line("%7", false, "claude --resume abc"),
            held_back_line("%7", true, "claude --resume abc"),
            no_input_line("%7", &prompt::NoInput::Off, true, "claude --resume abc"),
            stopped_line("alpha", std::slice::from_ref(&dup), &[dup.clone(), dotted]),
            stopped_line("alpha", &[], std::slice::from_ref(&dup)),
            not_selected_line(&dup),
            window_closed_line(&dup),
            window_short_line(&dup, 1, 2),
            UNAVAILABLE.to_string(),
        ];
        for gone in [true, false] {
            for opened_in in [None, Some("/home/me/it's there".to_string())] {
                let left = Left {
                    saved: "/home/me/it's here".to_string(),
                    gone,
                    opened_in,
                };
                all.push(left.line("%7", None));
                all.push(left.line("%7", Some("claude --resume abc")));
            }
        }
        let session = SessionSnap {
            name: "alpha".to_string(),
            path: "/repo".to_string(),
            active_window_index: 1,
            windows: vec![saved_window(0, "dup", 2), saved_window(1, "dup", 1)],
        };
        plan_session(&mut all, &session);
        for line in all {
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(!line.contains(banned), "{line}");
            }
        }
    }

    #[test]
    fn an_empty_id_from_a_p_f_reply_is_an_error_not_a_target() {
        // tmux reads `-t ""` as the current pane, so an id that did not come back must
        // stop the restore rather than redirect the rest of it.
        assert!(parse_pane("", "split-window").is_err());
        assert!(parse_pane("  \t/home/me", "split-window").is_err());
        assert!(parse_pane("%7", "split-window").is_err());
        assert_eq!(
            parse_pane(" %7\t/home/me", "split-window").unwrap(),
            ("%7".to_string(), "/home/me".to_string())
        );
        assert!(parse_ids("", "new-window").is_err());
        assert!(parse_ids("@1\t\t/home/me", "new-window").is_err());
        assert!(parse_ids("\t%1\t/home/me", "new-window").is_err());
        assert!(parse_ids("@1\t%1", "new-window").is_err());
        assert_eq!(
            parse_ids("@1\t%1\t/home/me", "new-window").unwrap(),
            ("@1".to_string(), "%1".to_string(), "/home/me".to_string())
        );
    }

    /// The directory of a `-P -F` reply is all that follows the pane's id, as tmux gave it: a folder's
    /// name can end in a space or hold a tab, and a tmux that does not have `#{pane_start_path}` gives
    /// it empty.
    #[test]
    fn the_directory_of_a_p_f_reply_is_all_that_follows_the_id() {
        for start in ["/home/me/trailing ", "/home/me/a\tb", ""] {
            assert_eq!(
                parse_pane(&format!("%7\t{start}"), "split-window").unwrap(),
                ("%7".to_string(), start.to_string())
            );
            assert_eq!(
                parse_ids(&format!("@1\t%7\t{start}"), "new-window")
                    .unwrap()
                    .2,
                start
            );
        }
    }

    #[test]
    fn valid_session_id_accepts_plain_tokens_and_rejects_anything_a_shell_would_read() {
        // "default:agent-123" is Letta's encoding for its default conversation
        // (`resume_command`'s `letta` arm decodes it) and is the one accepted id with a
        // `:` in it — every other kind's id is a plain token.
        for good in ["abc", "a-b_c.1", "0", "default:agent-123", &"x".repeat(128)] {
            assert!(valid_session_id(good), "{good:?} should be accepted");
        }
        for bad in [
            "",
            &"x".repeat(129),
            "a b",
            "a;b",
            "a\nb",
            "a$(id)b",
            "a`id`b",
            "a|b",
            "a&b",
            "a/b",
            "a'b",
            "\u{e9}",
        ] {
            assert!(!valid_session_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn resume_command_letta_encodes_the_default_conversation_and_refuses_an_empty_one() {
        // Follows herdr's `agent_resume.rs` `plan` tests for the `("herdr:letta",
        // "letta", ...)` arm.
        assert_eq!(
            resume_command("letta", "conversation-123"),
            Some("letta --conversation conversation-123".to_string())
        );
        assert_eq!(
            resume_command("letta", "default:agent-123"),
            Some("letta --conversation default --agent agent-123".to_string())
        );
        // An id that only encodes the "default" marker with no agent id is refused
        // outright, not resumed against a blank `--agent`.
        assert_eq!(resume_command("letta", "default:"), None);
    }

    #[test]
    fn persisted_session_from_launch_args_recognises_exactly_codexs_explicit_resume() {
        // Follows herdr's `agent_resume.rs` `persisted_session_from_launch_args` tests
        // (`codex_noncanonical_resume_launch_has_no_explicit_session`), generalised from
        // herdr's `Agent` enum to a plain kind string.
        assert_eq!(
            persisted_session_from_launch_args(
                "codex",
                &["resume".to_string(), "codex-session".to_string()]
            ),
            Some("codex-session")
        );
        // `--last` (or anything else starting with `-`) is a flag, not a session id.
        assert_eq!(
            persisted_session_from_launch_args(
                "codex",
                &["resume".to_string(), "--last".to_string()]
            ),
            None
        );
        // More than the exact ["resume", id] shape: not recognised, whatever it contains.
        assert_eq!(
            persisted_session_from_launch_args(
                "codex",
                &[
                    "resume".to_string(),
                    "not-a-session".to_string(),
                    "--last".to_string()
                ]
            ),
            None
        );
        assert_eq!(
            persisted_session_from_launch_args(
                "codex",
                &[
                    "--remote".to_string(),
                    "ws://example.test".to_string(),
                    "resume".to_string(),
                    "remote-session".to_string(),
                ]
            ),
            None
        );
        // Only codex's launch syntax means this; another kind's identical args are not a
        // persisted session.
        assert_eq!(
            persisted_session_from_launch_args(
                "claude",
                &["resume".to_string(), "codex-session".to_string()]
            ),
            None
        );
        // A syntactically-fine id that is not a *valid* session id is still refused: this
        // value is about to be written straight to `@agent_session_id` with no other gate.
        assert_eq!(
            persisted_session_from_launch_args(
                "codex",
                &["resume".to_string(), "has a space".to_string()]
            ),
            None
        );
    }

    #[test]
    fn resume_command_quotes_the_session_id_so_a_shell_cannot_read_it_as_syntax() {
        // Belt to the write-side check's braces: a snapshot taken before that check
        // existed can still hold anything, and this string is executed by the pane's
        // shell, not by `exec`.
        let cmd = resume_command("claude", "abc; rm -rf /").unwrap();
        assert_eq!(cmd, "claude --resume 'abc; rm -rf /'");
        assert!(
            !cmd.ends_with("rm -rf /"),
            "the id must not end the command's first word: {cmd}"
        );
        assert_eq!(
            resume_command("codex", "a'b").unwrap(),
            "codex resume 'a'\\''b'"
        );
        // A plain id is left exactly as it was, so nothing an agent really emits changes.
        assert_eq!(
            resume_command("opencode", "01JD-abc.9").unwrap(),
            "opencode --session 01JD-abc.9"
        );
    }

    #[test]
    fn resume_command_maps_known_agent_kinds_and_none_for_the_rest() {
        assert_eq!(
            resume_command("claude", "abc"),
            Some("claude --resume abc".to_string())
        );
        assert_eq!(
            resume_command("codex", "abc"),
            Some("codex resume abc".to_string())
        );
        assert_eq!(
            resume_command("opencode", "abc"),
            Some("opencode --session abc".to_string())
        );
        assert_eq!(resume_command("gemini", "abc"), None);
        assert_eq!(resume_command("", "abc"), None);
    }

    #[test]
    fn a_resume_command_is_the_agents_command_and_its_resume_arguments_whatever_the_id_holds() {
        for kind in [
            "claude",
            "codex",
            "copilot",
            "cursor",
            "devin",
            "droid",
            "grok",
            "hermes",
            "qodercli",
            "qwen",
            "omp",
            "kilo",
            "kimi",
            "opencode",
            "pi",
            "mastracode",
            "agy",
            "letta",
        ] {
            for id in ["4b1f2c", "a b", "it's", "default:agent-7"] {
                let args = resume_args(kind, id).unwrap_or_else(|| panic!("{kind}"));
                let command = resume_command(kind, id).unwrap();
                let mut words = vec![crate::detect::executable(kind)];
                words.extend(args);
                assert_eq!(
                    shell_words::split(&command).unwrap(),
                    words,
                    "{kind} {id:?}"
                );
            }
        }
        assert_eq!(
            resume_args("claude", "4b1f2c").unwrap(),
            ["--resume", "4b1f2c"]
        );
        assert_eq!(
            resume_args("codex", "4b1f2c").unwrap(),
            ["resume", "4b1f2c"]
        );
        assert_eq!(
            resume_args("opencode", "4b1f2c").unwrap(),
            ["--session", "4b1f2c"]
        );
        assert_eq!(
            resume_args("letta", "default:agent-7").unwrap(),
            ["--conversation", "default", "--agent", "agent-7"]
        );
        assert_eq!(resume_args("letta", "default:"), None);
        assert_eq!(resume_args("gemini", "4b1f2c"), None);
        // herdr's spelling stays: a `--flag=value` word keeps its flag bare and quotes its value.
        assert_eq!(resume_command("omp", "o1").unwrap(), "omp --resume=o1");
        assert_eq!(
            resume_command("copilot", "a b").unwrap(),
            "copilot --resume='a b'"
        );
        // Every command is the one taw typed before the table, byte for byte.
        for (kind, command) in [
            ("claude", "claude --resume o1"),
            ("codex", "codex resume o1"),
            ("copilot", "copilot --resume=o1"),
            ("cursor", "cursor-agent --resume o1"),
            ("devin", "devin --resume o1"),
            ("droid", "droid --resume o1"),
            ("grok", "grok --resume o1"),
            ("hermes", "hermes --resume o1"),
            ("qodercli", "qodercli --resume o1"),
            ("qwen", "qwen --resume o1"),
            ("omp", "omp --resume=o1"),
            ("kilo", "kilo --session o1"),
            ("kimi", "kimi --session o1"),
            ("opencode", "opencode --session o1"),
            ("pi", "pi --session o1"),
            ("mastracode", "mastracode --thread o1"),
            ("agy", "agy --conversation o1"),
            ("letta", "letta --conversation o1"),
        ] {
            assert_eq!(
                resume_command(kind, "o1").as_deref(),
                Some(command),
                "{kind}"
            );
        }
    }

    /// A saved pane in `/`, a directory every machine has: a restore types nothing into a pane whose
    /// saved directory is gone.
    fn pane_with(id: Option<&str>, path: Option<&str>) -> PaneSnap {
        PaneSnap {
            index: 0,
            cwd: "/".to_string(),
            active: true,
            agent_kind: None,
            agent_session_id: id.map(str::to_string),
            agent_session_path: path.map(str::to_string),
            agent_name: None,
            agent_kind_hint: None,
            agent_resume: None,
            agent_sandbox: None,
            agent_container: None,
            agent_claude_flags: Vec::new(),
        }
    }

    #[test]
    fn a_valid_session_id_is_always_preferred_including_for_the_path_kinds() {
        // An id already resumes today; adding the path fallback must not change a pane
        // that already works.
        let both = pane_with(Some("abc123"), Some("/home/me/.pi/sessions/x.json"));
        for kind in ["claude", "codex", "pi", "omp"] {
            assert_eq!(
                resume_target(kind, &both).unwrap(),
                Some(ResumeBy::Id("abc123")),
                "{kind} should resume by id when it has a valid one"
            );
        }
    }

    #[test]
    fn a_path_kind_with_no_id_resumes_from_its_saved_session_path() {
        // The case this exists for: the pi and omp integrations report a session *path*,
        // which is saved as @agent_session_path. A path kind is resumed from it; without
        // that the pane would restore as a plain shell.
        let path_only = pane_with(None, Some("/home/me/.pi/sessions/2026-01.json"));
        for kind in PATH_RESUME_KINDS {
            assert_eq!(
                resume_target(kind, &path_only).unwrap(),
                Some(ResumeBy::Path("/home/me/.pi/sessions/2026-01.json")),
                "{kind} should fall back to its saved path"
            );
        }
        // A path with a space in it is fine: `resume_command` shell-quotes the value, and
        // ~/Library/Application Support is an ordinary place to keep a session file.
        let spaced = pane_with(
            None,
            Some("/Users/me/Library/Application Support/omp/session.json"),
        );
        assert_eq!(
            resume_target("omp", &spaced).unwrap(),
            Some(ResumeBy::Path(
                "/Users/me/Library/Application Support/omp/session.json"
            ))
        );
    }

    #[test]
    fn a_kind_whose_resume_flag_wants_an_id_never_falls_back_to_a_path() {
        // `claude --resume <path>` would type a command claude rejects, leaving a failed
        // command in a pane that restores as a clean shell.
        let path_only = pane_with(None, Some("/home/me/.claude/sessions/x.json"));
        for kind in ["claude", "codex", "opencode", "gemini"] {
            assert_eq!(
                resume_target(kind, &path_only).unwrap(),
                None,
                "{kind} must not resume from a path"
            );
        }
    }

    #[test]
    fn a_pane_with_nothing_saved_resumes_from_nothing_and_is_not_an_error() {
        assert_eq!(resume_target("pi", &pane_with(None, None)).unwrap(), None);
        // Empty strings are what an unset tmux option expands to, so they are absence.
        assert_eq!(
            resume_target("pi", &pane_with(Some(""), Some(""))).unwrap(),
            None
        );
        assert_eq!(
            resume_target("pi", &pane_with(Some(""), Some("/tmp/s.json"))).unwrap(),
            Some(ResumeBy::Path("/tmp/s.json"))
        );
    }

    #[test]
    fn a_hostile_or_relative_session_path_is_refused_with_a_reason() {
        // `@agent_session_path` is written with no validation at all, so this read-side
        // check is the only thing between a hook's value and a `send-keys` line.
        for bad in [
            "relative/path.json",
            "~/.pi/session.json",
            "/tmp/a;rm -rf /",
            "/tmp/a$(id)",
            "/tmp/a`id`",
            "/tmp/a|tee /tmp/x",
            "/tmp/a&&id",
            "/tmp/a>/tmp/x",
            "/tmp/a\nrm -rf /",
            "/tmp/a\rid",
            "/tmp/a'b",
            "/tmp/a\"b",
            "/tmp/a\\b",
            "/tmp/a*b",
        ] {
            let saved = pane_with(None, Some(bad));
            let err = resume_target("pi", &saved)
                .expect_err(&format!("{bad:?} should have been refused"));
            assert!(err.contains("not a usable session path"), "{bad:?}: {err}");
        }
        // And the length bound.
        let long = format!("/{}", "x".repeat(4096));
        assert!(resume_target("pi", &pane_with(None, Some(&long))).is_err());
    }

    #[test]
    fn an_invalid_session_id_is_refused_rather_than_falling_through_to_the_path() {
        // A snapshot can predate `valid_session_id`. The pane must be reported, not
        // quietly resumed from a path the agent was not asked about.
        let saved = pane_with(Some("abc; rm -rf /"), Some("/tmp/s.json"));
        let err = resume_target("pi", &saved).unwrap_err();
        assert!(err.contains("not a valid agent session id"), "{err}");
    }

    #[test]
    fn valid_session_path_accepts_what_a_real_agent_writes() {
        for good in [
            "/home/me/.pi/sessions/01JD-abc.json",
            "/Users/me/Library/Application Support/omp/s.json",
            "/tmp/a.b-c_d.json",
            "/",
        ] {
            assert!(valid_session_path(good), "{good} should be accepted");
        }
        for bad in ["", "x", "./x", "/a\tb", "/a\u{0}b"] {
            assert!(!valid_session_path(bad), "{bad:?} should be rejected");
        }
        assert!(valid_session_path(&format!("/{}", "x".repeat(4095))));
        assert!(!valid_session_path(&format!("/{}", "x".repeat(4096))));
    }

    /// A saved window with `panes` panes, none of them holding an agent.
    fn saved_window(index: u32, name: &str, panes: usize) -> snapshot::WindowSnap {
        snapshot::WindowSnap {
            index,
            name: name.to_string(),
            layout: format!("layout-{index}"),
            active: false,
            panes: (0..panes).map(|_| pane_with(None, None)).collect(),
        }
    }

    #[test]
    fn plan_session_names_the_session_and_every_window_by_its_saved_number_and_its_name() {
        // Two windows called `zsh`, and a gap where a window was closed before the save: the
        // number is the one the window had, and not its place in the list.
        let session = SessionSnap {
            name: "alpha".to_string(),
            path: "/repo".to_string(),
            active_window_index: 3,
            windows: vec![
                saved_window(0, "zsh", 2),
                saved_window(1, "zsh", 1),
                saved_window(3, "nvim", 1),
            ],
        };
        let mut plan = Vec::new();
        plan_session(&mut plan, &session);
        assert_eq!(
            plan,
            [
                "create session 'alpha' (cwd /repo) with 3 window(s)",
                "  use first window as window 0 'zsh' (2 pane(s), layout layout-0)",
                "  create window 1 'zsh' (1 pane(s), layout layout-1)",
                "  create window 3 'nvim' (1 pane(s), layout layout-3)",
                "  select window 3 'nvim'",
            ]
        );
    }

    #[test]
    fn the_lines_about_a_window_name_it_by_its_saved_number_and_its_name() {
        let first = window_label(&saved_window(0, "zsh", 1));
        let second = window_label(&saved_window(1, "zsh", 1));
        let fourth = window_label(&saved_window(3, "nvim", 1));
        assert_eq!(first, "0 'zsh'");
        assert_eq!(
            stopped_line("alpha", &[first], &[second.clone(), fourth]),
            "stopped restoring session 'alpha': a pane or window of it closed while it was being \
             made; windows made: 0 'zsh'; not made: 1 'zsh', 3 'nvim'"
        );
        assert_eq!(
            stopped_line("alpha", &[], std::slice::from_ref(&second)),
            "stopped restoring session 'alpha': a pane or window of it closed while it was being \
             made; windows made: none; not made: 1 'zsh'"
        );
        assert_eq!(
            not_selected_line(&second),
            "could not select window 1 'zsh': it closed while the session was being restored"
        );
        assert_eq!(
            window_closed_line(&second),
            "window 1 'zsh' was closed while the session was being restored"
        );
        assert_eq!(
            window_short_line(&second, 1, 2),
            "window 1 'zsh' has 1 of its 2 panes: the rest closed while the session was being restored"
        );
    }

    /// A pane made as `id`, which tmux opened in its saved directory.
    fn made_pane(id: &str) -> BuiltPane {
        BuiltPane {
            id: id.to_string(),
            left: None,
        }
    }

    #[test]
    fn the_windows_made_and_not_made_are_named_by_their_saved_numbers_and_not_their_places() {
        let session = SessionSnap {
            name: "alpha".to_string(),
            path: "/repo".to_string(),
            active_window_index: 0,
            windows: vec![
                saved_window(0, "zsh", 1),
                saved_window(1, "zsh", 2),
                saved_window(3, "nvim", 1),
            ],
        };
        // The first window was made whole, the second was being made when the session was given
        // up on, and the third was never begun.
        let built = [
            Built {
                window_id: "@1".to_string(),
                panes: vec![made_pane("%1")],
                complete: true,
            },
            Built {
                window_id: "@2".to_string(),
                panes: vec![made_pane("%2")],
                complete: false,
            },
        ];
        let (made, not_made) = made_and_not(&session, &built);
        assert_eq!(made, ["0 'zsh'"]);
        assert_eq!(not_made, ["1 'zsh'", "3 'nvim'"]);

        // A window made after a gap is named by its saved number too: the windows that were made
        // are named by the number they had as the ones that were not are.
        let session = SessionSnap {
            windows: vec![
                saved_window(0, "zsh", 1),
                saved_window(2, "zsh", 1),
                saved_window(3, "nvim", 1),
            ],
            ..session
        };
        let built = [
            Built {
                window_id: "@1".to_string(),
                panes: vec![made_pane("%1")],
                complete: true,
            },
            Built {
                window_id: "@2".to_string(),
                panes: vec![made_pane("%2")],
                complete: true,
            },
        ];
        let (made, not_made) = made_and_not(&session, &built);
        assert_eq!(made, ["0 'zsh'", "2 'zsh'"]);
        assert_eq!(not_made, ["3 'nvim'"]);
    }

    #[test]
    fn the_error_of_a_restore_that_left_panes_alone_says_so_after_its_own_message_and_keeps_its_code()
     {
        let tmux_said =
            "`tmux select-layout -t @4 nope` failed (exit status: 1): invalid layout: nope";
        let lines = [
            not_ready_line("%3", "claude --resume abc"),
            gone_line("%5", "codex resume def"),
        ];

        let err = saying_what_was_left_alone(anyhow::anyhow!(tmux_said), &lines);
        assert_eq!(
            err.to_string(),
            format!("{tmux_said}\n{}\n{}", lines[0], lines[1])
        );
        assert_eq!(crate::cli::envelope::code_for(&err), "tmux_error");

        // With nothing left alone it is the error it was.
        assert_eq!(
            saying_what_was_left_alone(anyhow::anyhow!(tmux_said), &[]).to_string(),
            tmux_said
        );
    }

    /// A restore with nothing to replay and the waits it has by default: what the planning of
    /// resumes needs, which touches no tmux.
    fn restore_of<'a>(cfg: &'a Config, paths: &'a Paths) -> Restore<'a> {
        Restore {
            cfg,
            paths,
            name: "default",
            history: None,
            wait: prompt::Timing::default(),
            resumed_any: std::cell::Cell::new(false),
            resumed: std::cell::RefCell::new(std::collections::HashMap::new()),
            running: &[],
            taw: "taw".to_string(),
        }
    }

    fn saved_agent(kind: &str, id: Option<&str>, path: Option<&str>) -> PaneSnap {
        PaneSnap {
            agent_kind: Some(kind.to_string()),
            ..pane_with(id, path)
        }
    }

    #[test]
    fn a_resume_carries_what_it_was_made_from_so_the_pane_can_be_marked_with_it() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        let session = SessionSnap {
            name: "alpha".to_string(),
            path: "/repo".to_string(),
            active_window_index: 0,
            windows: vec![snapshot::WindowSnap {
                index: 0,
                name: "agents".to_string(),
                layout: "layout".to_string(),
                active: true,
                panes: vec![
                    saved_agent("claude", Some("abc"), None),
                    saved_agent("pi", None, Some("/home/me/.pi/s.json")),
                    // An id is preferred where there is one, whatever else was saved.
                    saved_agent("omp", Some("o1"), Some("/home/me/.omp/s.json")),
                ],
            }],
        };
        let built = [Built {
            window_id: "@1".to_string(),
            panes: ["%1", "%2", "%3"].map(made_pane).to_vec(),
            complete: true,
        }];
        let mut report = Report::default();

        let resumes = plan_resumes(&restore, &session, &built, &mut report);

        let planned: Vec<_> = resumes
            .iter()
            .map(|r| {
                (
                    r.pane_id.as_str(),
                    r.command.as_str(),
                    r.session_id.as_deref(),
                    r.session_path.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            planned,
            [
                ("%1", "claude --resume abc", Some("abc"), None),
                (
                    "%2",
                    "pi --session /home/me/.pi/s.json",
                    None,
                    Some("/home/me/.pi/s.json")
                ),
                ("%3", "omp --resume=o1", Some("o1"), None),
            ]
        );
        assert!(report.plan.is_empty(), "{:?}", report.plan);
    }

    /// A pane in `cwd` whose agent reported `argv` as the command that resumes its session.
    fn reported(cwd: &str, source: &str, agent: &str, argv: &[&str]) -> PaneSnap {
        PaneSnap {
            cwd: cwd.to_string(),
            agent_kind: Some(agent.to_string()),
            agent_resume: Some(crate::agent_resume::ReportedResume {
                source: source.to_string(),
                agent: agent.to_string(),
                argv: argv.iter().map(|word| word.to_string()).collect(),
            }),
            ..pane_with(None, None)
        }
    }

    /// One saved window holding `panes`, each made as `%1`, `%2`, and so on, by a tmux that read each
    /// pane's saved directory as it is.
    fn planned_for(restore: &Restore<'_>, panes: Vec<PaneSnap>) -> (Vec<Resume>, Report) {
        let starts: Vec<String> = panes.iter().map(|pane| pane.cwd.clone()).collect();
        planned_opened_in(restore, panes, &starts)
    }

    /// [`planned_for`], with each pane made by a tmux that read its directory as the one in its place in
    /// `starts` (`#{pane_start_path}`).
    fn planned_opened_in(
        restore: &Restore<'_>,
        panes: Vec<PaneSnap>,
        starts: &[String],
    ) -> (Vec<Resume>, Report) {
        let built = [Built {
            window_id: "@1".to_string(),
            panes: panes
                .iter()
                .zip(starts)
                .enumerate()
                .map(|(n, (pane, start))| {
                    BuiltPane::new(format!("%{}", n + 1), Some(pane), look(&pane.cwd), start)
                })
                .collect(),
            complete: true,
        }];
        let session = SessionSnap {
            name: "alpha".to_string(),
            path: "/repo".to_string(),
            active_window_index: 0,
            windows: vec![snapshot::WindowSnap {
                index: 0,
                name: "agents".to_string(),
                layout: "layout".to_string(),
                active: true,
                panes,
            }],
        };
        let mut report = Report::default();
        let resumes = plan_resumes(restore, &session, &built, &mut report);
        (resumes, report)
    }

    // Follows herdr's src/persist/restore.rs,
    // `pane_restore_startup_prefers_reported_resume_argv`. Differences: through the planning of a
    // restore, which types the command into the pane's shell; the saved screen a resumed pane does not
    // get is `will_resume`'s.
    #[test]
    fn pane_restore_startup_prefers_reported_resume_argv() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        let projects = tempfile::tempdir().unwrap();
        let [a, b] = ["project-a", "project-b"].map(|name| {
            let dir = projects.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            dir.display().to_string()
        });
        let pi = PaneSnap {
            agent_session_path: Some("/home/me/.pi/pi-session.jsonl".to_string()),
            ..reported(
                &a,
                "integration:pi",
                "pi",
                &["pi", "--continue", "--model", "m"],
            )
        };
        assert!(restore.will_resume(&pi));
        let custom = |cwd| {
            reported(
                cwd,
                "prime-agent",
                "prime-agent",
                &["prime-agent", "--continue"],
            )
        };

        let (resumes, report) = planned_for(&restore, vec![pi, custom(&a), custom(&b), custom(&a)]);

        let planned: Vec<_> = resumes
            .iter()
            .map(|r| (r.pane_id.as_str(), r.kind.as_str(), r.command.as_str()))
            .collect();
        assert_eq!(
            planned,
            [
                ("%1", "pi", "pi --continue --model m"),
                ("%2", "prime-agent", "prime-agent --continue"),
                ("%3", "prime-agent", "prime-agent --continue"),
            ]
        );
        // The pane keeps what it saved, the session path with the command it reported.
        assert_eq!(
            resumes[0].session_path.as_deref(),
            Some("/home/me/.pi/pi-session.jsonl")
        );
        assert_eq!(
            resumes[0].reported.as_ref().map(|r| r.source.as_str()),
            Some("integration:pi")
        );
        // The same command in the same directory is one session, resumed in the first pane only.
        assert_eq!(
            report.plan,
            ["skip resume in pane %4: pane %2 resumes the same session"]
        );

        let mut off = Config::default();
        off.session.resume_agents_on_restore = false;
        let disabled = restore_of(&off, &paths);
        assert!(!disabled.will_resume(&custom(&a)));
    }

    /// A command a snapshot holds that the checks refuse (a path, an apostrophe) is not typed: the pane
    /// is resumed with taw's own command for its kind, from the session it saved, as herdr falls back
    /// to its own.
    #[test]
    fn a_saved_command_the_checks_refuse_falls_back_to_taws_own_resume() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        for argv in [
            &["/usr/bin/claude", "--resume", "abc"][..],
            &["claude", "it's"],
        ] {
            let pane = PaneSnap {
                agent_session_id: Some("abc".to_string()),
                ..reported("/", "integration:claude", "claude", argv)
            };
            let restore = restore_of(&cfg, &paths);
            let (resumes, report) = planned_for(&restore, vec![pane]);
            assert_eq!(resumes.len(), 1, "{argv:?}");
            assert_eq!(resumes[0].command, "claude --resume abc", "{argv:?}");
            assert!(resumes[0].reported.is_none());
            assert!(report.plan.is_empty(), "{:?}", report.plan);
        }
        // With no session saved either, nothing is resumed.
        let (resumes, _) = planned_for(
            &restore,
            vec![reported("/", "s", "prime-agent", &["/opt/prime-agent"])],
        );
        assert!(resumes.is_empty());
    }

    /// A restore resumes each of taw's own sessions in one pane as well: the same agent session saved in
    /// two panes is resumed in the first, and the plan says why the second is not.
    #[test]
    fn a_session_saved_in_two_panes_is_resumed_in_the_first() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        let (resumes, report) = planned_for(
            &restore,
            vec![
                saved_agent("claude", Some("abc"), None),
                saved_agent("claude", Some("abc"), None),
                saved_agent("claude", Some("def"), None),
                saved_agent("codex", Some("abc"), None),
            ],
        );
        let panes: Vec<&str> = resumes.iter().map(|r| r.pane_id.as_str()).collect();
        assert_eq!(panes, ["%1", "%3", "%4"]);
        assert_eq!(
            report.plan,
            ["skip resume in pane %2: pane %1 resumes the same session"]
        );
    }

    /// `will_resume` is what keeps a saved screen from being painted over a pane an agent is about
    /// to be resumed into, so it has to say yes for exactly the panes the planning resumes.
    #[test]
    fn a_pane_is_not_painted_over_when_it_is_to_be_resumed_from_its_id_or_its_path() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        let resumed = [
            saved_agent("claude", Some("abc"), None),
            saved_agent("pi", Some("p1"), None),
            // Nothing but the path of its session file, which is how `pi` and `omp` report.
            saved_agent("pi", None, Some("/home/me/.pi/s.json")),
            saved_agent("omp", None, Some("/home/me/.omp/s.json")),
        ];
        for pane in &resumed {
            assert!(
                restore.will_resume(pane),
                "{pane:?} is resumed, and painted"
            );
        }
        let not_resumed = [
            // No agent at all, an agent with nothing to resume from, and one taw has no command for.
            pane_with(Some("abc"), None),
            saved_agent("claude", None, None),
            saved_agent("gemini", Some("abc"), None),
            // A path is no id to a kind whose flag wants one, and a hostile one is refused.
            saved_agent("claude", None, Some("/home/me/.claude/s.json")),
            saved_agent("pi", None, Some("/tmp/s.json; touch /tmp/pwned")),
            saved_agent("pi", Some("a b"), Some("/home/me/.pi/s.json")),
        ];
        for pane in &not_resumed {
            assert!(
                !restore.will_resume(pane),
                "{pane:?} is not resumed, and is not to lose its screen"
            );
        }

        let mut off = Config::default();
        off.session.resume_agents_on_restore = false;
        let restore = restore_of(&off, &paths);
        assert!(!restore.will_resume(&resumed[0]));
    }

    /// What the plan says of a pane left alone names the pane, the directory and where tmux opened it,
    /// and, for an agent that was to be resumed, the command to run from that directory: the directory
    /// quoted as the words of a reported command are, since the pane is somewhere else.
    #[test]
    fn what_the_plan_says_of_a_pane_not_opened_in_its_saved_directory_names_where_tmux_opened_it() {
        let gone = |saved: &str| Left {
            saved: saved.to_string(),
            gone: true,
            opened_in: None,
        };
        assert_eq!(
            gone("/home/me/project").line("%12", None),
            "pane %12: saved directory /home/me/project is gone, so tmux opened it in your home folder"
        );
        assert_eq!(
            gone("/home/me/project").line("%12", Some("claude --resume abc")),
            "pane %12: saved directory /home/me/project is gone, so tmux opened it in your home \
             folder and nothing was typed; once the directory is back, resume it with: cd \
             /home/me/project && claude --resume abc"
        );
        assert_eq!(
            gone("/home/me/my project's").line("%3", Some("codex resume def")),
            "pane %3: saved directory /home/me/my project's is gone, so tmux opened it in your home \
             folder and nothing was typed; once the directory is back, resume it with: cd \
             '/home/me/my project'\\''s' && codex resume def"
        );

        let elsewhere = |opened_in: Option<&str>| Left {
            saved: "/home/me/C#Sharp".to_string(),
            gone: false,
            opened_in: opened_in.map(str::to_string),
        };
        assert_eq!(
            elsewhere(Some("/home/me/Cworkharp")).line("%4", None),
            "pane %4: tmux opened it in /home/me/Cworkharp and not in its saved directory \
             /home/me/C#Sharp"
        );
        assert_eq!(
            elsewhere(None).line("%4", Some("claude --resume abc")),
            "pane %4: tmux opened it in your home folder and not in its saved directory \
             /home/me/C#Sharp, so nothing was typed; resume it with: cd '/home/me/C#Sharp' && \
             claude --resume abc"
        );
    }

    /// A saved directory is gone when there is no directory where it was: nothing there, or a file. It
    /// cannot be opened when there is no permission to go into it. A pane saved with no directory has
    /// none to look for.
    #[test]
    fn a_saved_directory_is_gone_when_no_directory_is_where_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, b"").unwrap();
        let there = tmp.path().display().to_string();
        let missing = tmp.path().join("missing").display().to_string();
        let file = file.display().to_string();
        assert_eq!(look(&there), Looked::There);
        assert_eq!(look(""), Looked::There);
        assert_eq!(look(&missing), Looked::Gone);
        assert_eq!(look(&file), Looked::Gone);
        assert_eq!(look("/home/me/a\0b"), Looked::Gone);

        let shut = Shut::new(&tmp.path().join("shut"));
        if std::fs::read_dir(&shut.0).is_ok() {
            eprintln!("skipped: this user opens a folder with no permissions (root)");
            return;
        }
        assert_eq!(look(&shut.0.display().to_string()), Looked::Shut);
        let below = shut.0.join("below");
        assert_eq!(look(&below.display().to_string()), Looked::Gone);
    }

    /// A folder made with no permissions at all, which is given them back once the test is done, so that
    /// its temporary directory can be removed.
    struct Shut(std::path::PathBuf);

    impl Shut {
        fn new(dir: &std::path::Path) -> Shut {
            std::fs::create_dir_all(dir).unwrap();
            let shut = Shut(dir.to_path_buf());
            set_mode(dir, 0o000);
            shut
        }
    }

    impl Drop for Shut {
        fn drop(&mut self) {
            set_mode(&self.0, 0o755);
        }
    }

    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A pane is left alone when tmux did not open it in its saved directory: one that is gone or cannot
    /// be opened, which tmux opens in the home folder, and one that tmux read as another directory (a
    /// `#` it took for a format), which tmux opens in that one when it can. Where tmux read what it was
    /// asked for as it was, or a tmux that does not have `#{pane_start_path}` gave nothing, what was
    /// found there decides alone; and a pane saved with no directory opens where tmux puts it.
    #[test]
    fn a_pane_is_left_alone_when_tmux_did_not_open_it_in_its_saved_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let [saved, other] = ["C#Sharp", "Cworkharp"].map(|name| {
            let dir = tmp.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            dir.display().to_string()
        });
        let missing = tmp.path().join("Cmissingharp").display().to_string();
        let left = |gone, opened_in: Option<&str>| {
            Some(Left {
                saved: saved.clone(),
                gone,
                opened_in: opened_in.map(str::to_string),
            })
        };

        assert_eq!(left_alone(&saved, Looked::There, &saved), None);
        assert_eq!(left_alone(&saved, Looked::There, ""), None);
        assert_eq!(left_alone("", Looked::Gone, "/elsewhere"), None);
        assert_eq!(
            left_alone(&saved, Looked::There, &other),
            left(false, Some(&other))
        );
        assert_eq!(
            left_alone(&saved, Looked::There, &missing),
            left(false, None)
        );
        for start in [saved.as_str(), ""] {
            assert_eq!(left_alone(&saved, Looked::Gone, start), left(true, None));
            assert_eq!(left_alone(&saved, Looked::Shut, start), left(false, None));
        }
        assert_eq!(
            left_alone(&saved, Looked::Gone, &other),
            left(true, Some(&other))
        );
    }

    /// A pane that tmux did not open in its saved directory is planned no resume, and it takes no
    /// session that another pane saved too: the pane after it resumes that session. That was decided
    /// as the pane was made, and the planning looks at nothing again.
    #[test]
    fn a_pane_left_alone_is_planned_no_resume_and_takes_no_session() {
        let cfg = Config::default();
        let paths = Paths::from_env();
        let restore = restore_of(&cfg, &paths);
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone").display().to_string();
        let there = tmp.path().display().to_string();
        let in_dir = |cwd: &str, id: &str| PaneSnap {
            cwd: cwd.to_string(),
            ..saved_agent("claude", Some(id), None)
        };
        let reported_in_gone = reported(&gone, "prime-agent", "prime-agent", &["prime-agent"]);

        let (resumes, report) = planned_opened_in(
            &restore,
            vec![
                in_dir(&gone, "abc"),
                reported_in_gone,
                // A directory that is there, which tmux read as one that is not.
                in_dir(&there, "abc"),
                in_dir(&there, "abc"),
            ],
            &[gone.clone(), gone.clone(), gone.clone(), there.clone()],
        );

        let planned: Vec<_> = resumes
            .iter()
            .map(|r| (r.pane_id.as_str(), r.command.as_str()))
            .collect();
        assert_eq!(planned, [("%4", "claude --resume abc")]);
        assert!(report.plan.is_empty(), "{:?}", report.plan);
    }

    /// With `[experimental] pane_history`, a pane whose saved directory is gone or cannot be opened is
    /// given no saved screen, as herdr starts nothing in it; a pane whose directory is there is.
    #[test]
    fn a_pane_whose_saved_directory_is_gone_is_given_no_saved_screen() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(&tmp.path().join("taw"));
        let screen = history::PaneHistory {
            ansi: "the last screen".to_string(),
            lines: 1,
        };
        let saved = History {
            version: 1,
            layout_fingerprint: None,
            sessions: vec![history::SessionHistory {
                windows: vec![history::WindowHistory {
                    panes: std::collections::HashMap::from([(0, screen)]),
                }],
            }],
        };
        let restore = Restore {
            history: Some(&saved),
            ..restore_of(&cfg, &paths)
        };
        let window = saved_window(0, "zsh", 1);
        let pane = PaneSnap {
            cwd: tmp.path().display().to_string(),
            ..pane_with(None, None)
        };

        for looked in [Looked::Gone, Looked::Shut] {
            assert_eq!(restore.pane_command(0, 0, &window, &pane, looked), None);
        }
        assert!(
            restore
                .pane_command(0, 0, &window, &pane, Looked::There)
                .is_some_and(|command| command.starts_with("cat "))
        );
    }

    /// The screens a pane is given are the ones taken with the snapshot being restored: with the
    /// fingerprint of its layout they paint, with another layout's or none they do not.
    #[test]
    fn a_pane_is_given_saved_screens_only_when_they_were_taken_with_this_layout() {
        let cfg = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(&tmp.path().join("taw"));
        let window = saved_window(0, "zsh", 1);
        let pane = PaneSnap {
            cwd: tmp.path().display().to_string(),
            ..pane_with(None, None)
        };
        let snap = snapshot::Snapshot {
            version: 1,
            taken_unix: 1,
            skipped_panes: 0,
            sessions: vec![SessionSnap {
                name: "one".to_string(),
                path: "/".to_string(),
                active_window_index: 0,
                windows: vec![snapshot::WindowSnap {
                    panes: vec![pane.clone()],
                    ..window.clone()
                }],
            }],
        };
        let screens = |fingerprint: Option<String>| History {
            version: 1,
            layout_fingerprint: fingerprint,
            sessions: vec![history::SessionHistory {
                windows: vec![history::WindowHistory {
                    panes: std::collections::HashMap::from([(
                        0,
                        history::PaneHistory {
                            ansi: "the last screen".to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        };
        let given = |saved: History| {
            let kept = history::for_layout(saved, &snap, "default");
            let restore = Restore {
                history: kept.as_ref(),
                ..restore_of(&cfg, &paths)
            };
            restore.pane_command(0, 0, &window, &pane, Looked::There)
        };

        let ours = snapshot::layout_fingerprint(&snap);
        assert!(given(screens(ours)).is_some_and(|command| command.starts_with("cat ")));
        assert_eq!(given(screens(Some("another layout".to_string()))), None);
        assert_eq!(given(screens(None)), None);
    }

    /// A `Paths` rooted in `dir`, for a test that writes what a restore writes.
    fn snapshot_test_paths(dir: &std::path::Path) -> Paths {
        let plugins_dir = dir.join("plugins");
        Paths {
            config_dir: dir.join("config"),
            config_file: dir.join("config").join("config.toml"),
            state_dir: dir.join("state"),
            override_dir: dir.join("config").join("agent-detection"),
            remote_manifest_dir: dir.join("state").join("agent-detection").join("remote"),
            sessions_dir: dir.join("state").join("sessions"),
            plugin_state_dir: plugins_dir.join("state"),
            plugins_dir,
            logs_dir: dir.join("state").join("logs"),
            manifest_marker: dir.join("state").join("agent-detection").join("last-check"),
        }
    }

    /// What a pane whose saved directory is gone keeps of itself: the whole of what was saved, else all
    /// but the command its agent reported, else nothing, each the text a save reads back.
    #[test]
    fn a_pane_whose_saved_directory_is_gone_keeps_the_most_of_itself_one_tmux_command_carries() {
        let plain = PaneSnap {
            cwd: "/home/me/gone".to_string(),
            agent_name: Some("keeper".to_string()),
            ..saved_agent("claude", Some("abc"), Some("/home/me/.claude/abc.jsonl"))
        };
        let versions = kept_versions(&plain);
        assert_eq!(versions.len(), 2, "{versions:?}");
        let whole: PaneSnap = serde_json::from_str(versions[0].as_deref().unwrap()).unwrap();
        assert_eq!(whole, plain);
        assert_eq!(versions[1], None);

        let with_command = reported(
            "/home/me/gone",
            "prime-agent",
            "prime-agent",
            &["prime-agent", "--resume", "a"],
        );
        let versions = kept_versions(&with_command);
        assert_eq!(versions.len(), 3, "{versions:?}");
        let whole: PaneSnap = serde_json::from_str(versions[0].as_deref().unwrap()).unwrap();
        assert_eq!(whole, with_command);
        let without: PaneSnap = serde_json::from_str(versions[1].as_deref().unwrap()).unwrap();
        assert_eq!(
            without,
            PaneSnap {
                agent_resume: None,
                ..with_command.clone()
            }
        );
        assert_eq!(versions[2], None);
    }

    fn sandboxed(kind: &str, id: &str, profile: Option<&str>, container: Option<&str>) -> PaneSnap {
        PaneSnap {
            agent_sandbox: profile.map(str::to_string),
            agent_container: container.map(str::to_string),
            agent_claude_flags: Vec::new(),
            ..saved_agent(kind, Some(id), None)
        }
    }

    fn one_window(panes: Vec<PaneSnap>) -> (SessionSnap, [Built; 1]) {
        let ids: Vec<BuiltPane> = (1..=panes.len())
            .map(|n| made_pane(&format!("%{n}")))
            .collect();
        (
            SessionSnap {
                name: "alpha".to_string(),
                path: "/repo".to_string(),
                active_window_index: 0,
                windows: vec![snapshot::WindowSnap {
                    index: 0,
                    name: "agents".to_string(),
                    layout: "layout".to_string(),
                    active: true,
                    panes,
                }],
            },
            [Built {
                window_id: "@1".to_string(),
                panes: ids,
                complete: true,
            }],
        )
    }

    fn your_profile(paths: &Paths, name: &str, mode: u32) {
        let dir = crate::sandbox::profile::yours_dir(paths).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable_with_mode(dir.join(name), mode);
    }

    #[test]
    fn a_sandboxed_pane_plans_taw_sandbox_with_the_agents_resume_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let (session, built) = one_window(vec![
            sandboxed("claude", "4b1f2c", Some("default"), Some("docker")),
            sandboxed("codex", "c0dex", Some("default"), Some("docker")),
        ]);
        let mut report = Report::default();

        let resumes = plan_resumes(&restore, &session, &built, &mut report);

        let planned: Vec<_> = resumes
            .iter()
            .map(|r| (r.command.as_str(), r.sandbox.as_deref()))
            .collect();
        assert_eq!(
            planned,
            [
                ("taw sandbox claude -- --resume 4b1f2c", Some("default")),
                ("taw sandbox codex -- resume c0dex", Some("default")),
            ]
        );
        assert!(report.plan.is_empty(), "{:?}", report.plan);
        assert!(restore.will_resume(&session.windows[0].panes[0]));
    }

    #[test]
    fn the_profile_is_named_only_when_the_config_would_pick_another() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        your_profile(&paths, "max", 0o755);
        let (session, built) = one_window(vec![sandboxed(
            "claude",
            "4b1f2c",
            Some("max"),
            Some("docker"),
        )]);

        let plain = Config::default();
        let resumes = plan_resumes(
            &restore_of(&plain, &paths),
            &session,
            &built,
            &mut Report::default(),
        );
        assert_eq!(
            resumes[0].command,
            "taw sandbox claude --profile max -- --resume 4b1f2c"
        );

        let picking: Config = toml::from_str("[sandbox]\nprofile = \"max\"\n").unwrap();
        let resumes = plan_resumes(
            &restore_of(&picking, &paths),
            &session,
            &built,
            &mut Report::default(),
        );
        assert_eq!(resumes[0].command, "taw sandbox claude -- --resume 4b1f2c");
    }

    #[test]
    fn a_profile_that_is_gone_plans_a_skip_with_its_reason_and_never_a_plain_resume() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        your_profile(&paths, "draft", 0o644);
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let (session, built) = one_window(vec![
            sandboxed("claude", "4b1f2c", Some("team"), Some("docker")),
            sandboxed("claude", "4b1f2c", Some("draft"), None),
        ]);
        let mut report = Report::default();

        let resumes = plan_resumes(&restore, &session, &built, &mut report);

        assert!(
            resumes.is_empty(),
            "{:?}",
            resumes.iter().map(|r| &r.command).collect::<Vec<_>>()
        );
        let draft = crate::sandbox::profile::yours_dir(&paths)
            .unwrap()
            .join("draft");
        assert_eq!(
            report.plan,
            [
                "skip resume in pane %1: its sandbox profile team is gone".to_string(),
                format!(
                    "skip resume in pane %2: its sandbox profile draft is not executable (chmod +x {})",
                    draft.display()
                ),
            ]
        );
        assert!(!restore.will_resume(&session.windows[0].panes[0]));
    }

    /// What the plan says of a pane saved with a Claude sandboxed through `profile`, for `paths`.
    fn skipped_through(paths: &Paths, profile: &str) -> Vec<String> {
        let cfg = Config::default();
        let restore = restore_of(&cfg, paths);
        let (session, built) = one_window(vec![sandboxed(
            "claude",
            "4b1f2c",
            Some(profile),
            Some("docker"),
        )]);
        let mut report = Report::default();
        let resumes = plan_resumes(&restore, &session, &built, &mut report);
        assert!(
            resumes.is_empty(),
            "{:?}",
            resumes.iter().map(|r| &r.command).collect::<Vec<_>>()
        );
        assert!(!restore.will_resume(&session.windows[0].panes[0]));
        report.plan
    }

    /// Your profiles folder, made.
    fn profiles_folder(paths: &Paths) -> std::path::PathBuf {
        let dir = crate::sandbox::profile::yours_dir(paths).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A profile that is not there is gone; a profile that is there and fails is
    /// not, and what `taw sandbox` says of it is what it says.
    #[test]
    fn a_profile_that_is_not_there_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        profiles_folder(&paths);
        assert_eq!(
            skipped_through(&paths, "team"),
            ["skip resume in pane %1: its sandbox profile team is gone"]
        );
    }

    #[test]
    fn a_saved_name_that_is_no_profiles_name_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        assert_eq!(
            skipped_through(&paths, "../team"),
            [
                "skip resume in pane %1: ../team is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)"
            ]
        );
    }

    #[test]
    fn a_profile_whose_interpreter_is_not_there_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let file = profiles_folder(&paths).join("team");
        std::fs::write(&file, "#!/no/such/sh\n").unwrap();
        crate::test_exec::make_executable(&file);
        assert_eq!(
            skipped_through(&paths, "team"),
            [format!(
                "skip resume in pane %1: the #! line of the profile team names /no/such/sh, which is no program you can run (head -1 {})",
                file.display()
            )]
        );
    }

    #[test]
    fn a_profile_whose_interpreter_is_not_a_full_path_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let file = profiles_folder(&paths).join("team");
        std::fs::write(&file, "#!bin/sh\n").unwrap();
        crate::test_exec::make_executable(&file);
        assert_eq!(
            skipped_through(&paths, "team"),
            [format!(
                "skip resume in pane %1: the #! line of the profile team names bin/sh, which is not a full path, so the system would take it from the project's folder (head -1 {})",
                file.display()
            )]
        );
    }

    #[test]
    fn a_profile_that_is_a_link_to_a_file_that_is_not_there_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let link = profiles_folder(&paths).join("team");
        std::os::unix::fs::symlink(dir.path().join("moved-away"), &link).unwrap();
        assert_eq!(
            skipped_through(&paths, "team"),
            [format!(
                "skip resume in pane %1: the profile team is a link to a file that is not there (ls -l {})",
                link.display()
            )]
        );
    }

    #[test]
    fn a_profile_that_is_no_file_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let folder = profiles_folder(&paths).join("team");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(
            skipped_through(&paths, "team"),
            [format!(
                "skip resume in pane %1: the profile team is not a file (ls -ld {})",
                folder.display()
            )]
        );
    }

    #[test]
    fn a_profile_taw_cannot_look_at_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let link = profiles_folder(&paths).join("team");
        std::os::unix::fs::symlink(&link, &link).unwrap();
        let plan = skipped_through(&paths, "team");
        let start = "skip resume in pane %1: taw cannot look at the profile team: ";
        let end = format!(" (ls -ld {})", link.display());
        assert!(
            plan.len() == 1 && plan[0].starts_with(start) && plan[0].ends_with(&end),
            "{plan:?}"
        );
    }

    #[test]
    fn a_profiles_folder_that_is_a_link_to_a_folder_that_is_not_there_is_skipped_in_taw_sandboxs_words()
     {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let folder = crate::sandbox::profile::yours_dir(&paths).unwrap();
        std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(dir.path().join("unstowed"), &folder).unwrap();
        assert_eq!(
            skipped_through(&paths, "team"),
            [format!(
                "skip resume in pane %1: your profiles folder {} is a link to a folder that is not there (ls -ld {})",
                folder.display(),
                folder.display()
            )]
        );
    }

    #[test]
    fn a_config_folder_that_is_not_a_full_path_is_skipped_in_taw_sandboxs_words() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: std::path::PathBuf::from("relative/taw"),
            ..crate::plugin::tests_support::test_paths(dir.path())
        };
        assert_eq!(
            skipped_through(&paths, "team"),
            [
                "skip resume in pane %1: no profile called team (TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours)"
            ]
        );
    }

    /// A command the agent reported too long to keep beside the rest in one tmux command: the restore
    /// keeps all but it, which still resumes the conversation by taw's own command, and the profile.
    #[test]
    fn a_sandboxed_resume_too_long_to_keep_whole_keeps_all_but_the_reported_command() {
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
        // As much as the checks let through, of what JSON writes twice over.
        let quotes = "\"".repeat(4000);
        let argv: Vec<&str> = vec!["claude", "--resume", "c-2", &quotes, &quotes];
        let reported = reported("/", "integration:claude", "claude", &argv)
            .agent_resume
            .unwrap();
        crate::agent_resume::validate_argv(&reported.argv).unwrap();
        let whole = snapshot::KeptResume {
            kind: "claude".to_string(),
            session_id: Some("c-2".to_string()),
            session_path: None,
            agent_resume: Some(reported),
        };

        mark_sandbox_resume(&t, "%3", "default", whole.clone()).unwrap();

        let calls = std::fs::read_to_string(&log).unwrap();
        let words: Vec<&str> = calls.lines().collect();
        let at = words
            .iter()
            .position(|word| *word == crate::store::SANDBOX_RESUME_OPTION)
            .unwrap_or_else(|| panic!("{calls}"));
        let kept: snapshot::KeptResume = serde_json::from_str(words[at + 1]).unwrap();
        assert_eq!(
            kept,
            snapshot::KeptResume {
                agent_resume: None,
                ..whole
            }
        );
        assert!(words.contains(&crate::store::SANDBOX_OPTION), "{calls}");
        assert_eq!(
            words.iter().filter(|word| **word == "set-option").count(),
            2,
            "one call, the whole refused before tmux was asked: {calls}"
        );
    }

    /// An agent that ran in a container taw did not start has no profile to come back through,
    /// and is never started again outside one, from its session or from the command it reported.
    #[test]
    fn an_agent_in_a_container_taw_did_not_start_is_never_resumed_outside_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let by_report = PaneSnap {
            agent_container: Some("podman".to_string()),
            agent_claude_flags: Vec::new(),
            ..reported(
                "/",
                "integration:claude",
                "claude",
                &["claude", "--resume", "c-2"],
            )
        };
        let (session, built) = one_window(vec![
            sandboxed("claude", "4b1f2c", None, Some("docker")),
            by_report,
        ]);
        let mut report = Report::default();

        let resumes = plan_resumes(&restore, &session, &built, &mut report);

        assert!(resumes.is_empty());
        assert_eq!(
            report.plan,
            [
                "skip resume in pane %1: its claude ran in a docker container taw did not start, so it is not started again outside it",
                "skip resume in pane %2: its claude ran in a podman container taw did not start, so it is not started again outside it",
            ]
        );
        assert!(!restore.will_resume(&session.windows[0].panes[0]));
        assert!(!restore.will_resume(&session.windows[0].panes[1]));
    }

    /// A command the agent reported wins over taw's own for a sandboxed pane, turned into the
    /// sandbox line; one whose first word is not the agent's command is never typed.
    #[test]
    fn a_reported_resume_command_becomes_the_sandbox_line_or_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        // Built from JSON, in the shape a save writes (herdr's `PaneAgentResumeSnapshot`).
        let reported = |argv: &[&str]| -> PaneSnap {
            serde_json::from_value(serde_json::json!({
                "index": 0, "cwd": "/repo", "active": true, "agent_kind": "kiro", "agent_session_id": "k-1",
                "agent_session_path": null, "agent_name": null, "agent_kind_hint": null,
                "agent_sandbox": "default", "agent_container": "docker",
                "agent_resume": {"source": "hook:kiro", "agent": "kiro", "argv": argv},
            }))
            .unwrap()
        };
        let (session, built) = one_window(vec![
            reported(&["kiro-cli", "chat", "--resume", "k-1"]),
            reported(&["sh", "-c", "kiro-cli chat --resume k-1"]),
        ]);
        let mut report = Report::default();

        let resumes = plan_resumes(&restore, &session, &built, &mut report);

        let typed: Vec<&str> = resumes.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(typed, ["taw sandbox kiro -- chat --resume k-1"]);
        assert_eq!(
            report.plan,
            [
                "skip resume in pane %2: its reported resume command does not start kiro, and a sandboxed agent starts only through its profile"
            ]
        );
        assert!(resumes[0].reported.is_some());
    }

    /// A saved session path with a space in it stays one argument of the line.
    #[test]
    fn a_session_path_with_a_space_is_one_argument_of_the_sandbox_line() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let spaced = "/home/agent/Application Support/omp/s.json";
        let saved = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            agent_container: Some("docker".to_string()),
            agent_claude_flags: Vec::new(),
            ..saved_agent("omp", None, Some(spaced))
        };
        let (session, built) = one_window(vec![saved]);

        let resumes = plan_resumes(&restore, &session, &built, &mut Report::default());

        let words = shell_words::split(&resumes[0].command).unwrap();
        assert_eq!(
            words,
            [
                "taw",
                "sandbox",
                "omp",
                "--",
                format!("--resume={spaced}").as_str()
            ]
        );
        assert_eq!(resumes[0].session_path.as_deref(), Some(spaced));
    }

    // -----------------------------------------------------------------------------------
    // A plain restore after a tmux server change: which layout, and what it says

    /// A snapshot of the sessions `names`, taken at `taken_unix`.
    fn layout_of(names: &[&str], taken_unix: u64) -> snapshot::Snapshot {
        snapshot::Snapshot {
            version: 2,
            taken_unix,
            skipped_panes: 0,
            sessions: names
                .iter()
                .map(|name| snapshot::SessionSnap {
                    name: name.to_string(),
                    path: "/repo".to_string(),
                    active_window_index: 0,
                    windows: vec![snapshot::WindowSnap {
                        index: 0,
                        name: "w".to_string(),
                        layout: "abcd,80x24,0,0,0".to_string(),
                        active: true,
                        panes: Vec::new(),
                    }],
                })
                .collect(),
        }
    }

    /// `default.json` holding `names` taken at `taken`, and copies holding the rest, each named so
    /// that file order is the order they were made in.
    fn saved_layouts(paths: &Paths, default: Option<(&[&str], u64)>, copies: &[(&[&str], u64)]) {
        let dir = paths.sessions_dir.join("session-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some((names, taken)) = default {
            std::fs::write(
                paths.sessions_dir.join("default.json"),
                serde_json::to_vec(&layout_of(names, taken)).unwrap(),
            )
            .unwrap();
        }
        for (n, (names, taken)) in copies.iter().enumerate() {
            std::fs::write(
                dir.join(format!("session-default-{:039}-1-0.json", n + 1)),
                serde_json::to_vec(&layout_of(names, *taken)).unwrap(),
            )
            .unwrap();
        }
    }

    fn open(names: &[&str]) -> Open {
        Open {
            sessions: names.iter().map(|n| n.to_string()).collect(),
            running: Vec::new(),
            next_window: std::collections::HashMap::new(),
            no_server: false,
        }
    }

    /// The planning of a layout with no agent in it: nothing to resume anywhere.
    fn no_plan(_: &PaneSnap) -> Result<Option<Planned>, String> {
        Ok(None)
    }

    fn names_of(snap: &snapshot::Snapshot) -> Vec<&str> {
        snap.sessions.iter().map(|s| s.name.as_str()).collect()
    }

    /// The new server's autosave replaced `default.json` with its one session: the old server's
    /// layout, the newest copy from before the start, is what a plain restore reads.
    #[test]
    fn a_plain_restore_reads_the_old_servers_layout_when_default_has_nothing_to_make() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(
            &paths,
            Some((&["fresh"], 2000)),
            &[(&["a"], 100), (&["a", "b", "c"], 900), (&["fresh"], 2000)],
        );
        let (snap, used) =
            plain_source(&paths, "default", Some(1000), &open(&["fresh"]), &no_plan).unwrap();
        assert_eq!(names_of(&snap), ["a", "b", "c"]);
        assert_eq!(used.taken_unix, 900);
        assert_eq!(used.server_start, Some(1000));
        assert!(used.file.to_string_lossy().contains("session-snapshots"));
    }

    /// `default.json` with a session that is not open is read as it always was, though a copy from
    /// before the server started has more.
    #[test]
    fn a_plain_restore_reads_default_while_it_has_a_session_to_make() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(
            &paths,
            Some((&["fresh", "closed"], 2000)),
            &[(&["a", "b", "c"], 900)],
        );
        let (snap, used) =
            plain_source(&paths, "default", Some(1000), &open(&["fresh"]), &no_plan).unwrap();
        assert_eq!(names_of(&snap), ["fresh", "closed"]);
        assert_eq!(used.file, paths.sessions_dir.join("default.json"));
    }

    /// `default.json` taken before the server started is the look-back itself: one file, read as it
    /// always was.
    #[test]
    fn default_taken_before_the_server_started_is_the_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, Some((&["a", "b"], 900)), &[(&["x"], 100)]);
        let (snap, used) =
            plain_source(&paths, "default", Some(1000), &open(&["x"]), &no_plan).unwrap();
        assert_eq!(names_of(&snap), ["a", "b"]);
        assert_eq!(used.file, paths.sessions_dir.join("default.json"));
    }

    /// One layout back and no further: the newest layout from before the start has every session
    /// open, so there is nothing to restore, though an older copy has a session that is not open.
    #[test]
    fn a_plain_restore_looks_back_one_layout_only() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(
            &paths,
            Some((&["fresh"], 2000)),
            &[(&["old", "long-closed"], 100), (&["a", "b"], 900)],
        );
        let (snap, used) = plain_source(
            &paths,
            "default",
            Some(1000),
            &open(&["fresh", "a", "b"]),
            &no_plan,
        )
        .unwrap();
        // Nothing to make anywhere that is looked at: default.json's own layout says so.
        assert_eq!(names_of(&snap), ["fresh"]);
        assert!(!has_a_session_to_make(
            &snap,
            &open(&["fresh", "a", "b"]),
            &no_plan
        ));
        assert_eq!(used.file, paths.sessions_dir.join("default.json"));
    }

    /// With no server start known, `default.json` alone, as before; with no `default.json` and
    /// nothing from before the start, the error a missing file gives.
    #[test]
    fn a_plain_restore_with_no_server_start_reads_default_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, Some((&["fresh"], 2000)), &[(&["a", "b"], 900)]);
        let (snap, used) =
            plain_source(&paths, "default", None, &open(&["fresh"]), &no_plan).unwrap();
        assert_eq!(names_of(&snap), ["fresh"]);
        assert_eq!(used.server_start, None);

        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, None, &[(&["fresh"], 2000)]);
        let err = plain_source(&paths, "default", Some(1000), &open(&[]), &no_plan).unwrap_err();
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)),
            "{err:#}"
        );
    }

    /// With no `default.json` at all, the copy from before the start is the layout, even with its
    /// sessions all open, which then says there is nothing to restore.
    #[test]
    fn a_plain_restore_with_no_default_file_reads_the_copy_from_before_the_start() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, None, &[(&["a", "b"], 900)]);
        let (snap, used) =
            plain_source(&paths, "default", Some(1000), &open(&["a", "b"]), &no_plan).unwrap();
        assert_eq!(names_of(&snap), ["a", "b"]);
        assert_eq!(used.taken_unix, 900);
    }

    /// A `default.json` a newer taw wrote is an error, as it was, and no older layout is read in its
    /// place.
    #[test]
    fn a_default_file_from_a_newer_taw_is_still_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, None, &[(&["a"], 900)]);
        let newer = snapshot::Snapshot {
            version: 99,
            ..layout_of(&["z"], 2000)
        };
        std::fs::write(
            paths.sessions_dir.join("default.json"),
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        let err = plain_source(&paths, "default", Some(1000), &open(&[]), &no_plan).unwrap_err();
        assert!(format!("{err:#}").contains("newer taw"), "{err:#}");
    }

    /// A `default.json` taw cannot read is an error, as it was: an unreadable file is worth seeing,
    /// and a valid copy from before the start is not used in its place (it can be restored by hand).
    #[test]
    fn an_unreadable_default_file_is_the_read_error_and_no_copy_is_used_instead() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        saved_layouts(&paths, None, &[(&["a", "b"], 900)]);
        std::fs::write(paths.sessions_dir.join("default.json"), "{ not json").unwrap();
        let err = plain_source(&paths, "default", Some(1000), &open(&["fresh"]), &no_plan)
            .map(|(snap, used)| (names_of(&snap).join(","), used.file))
            .unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("parsing") && said.contains("default.json"),
            "{said}"
        );
    }

    /// 2025-03-14 16:23:00 and 16:41:35 UTC, which a zone 10 hours ahead (UTC+10) reads as the
    /// next morning.
    const SAVED: u64 = 1_741_969_380;
    const STARTED: u64 = 1_741_970_495;

    #[test]
    fn the_first_line_of_a_plain_restore_says_which_layout_with_both_times() {
        let offset = |_| 0;
        // Saved and started on one day, and now that day too.
        let now = STARTED + 60;
        assert_eq!(
            saved_line(false, true, SAVED, Some(STARTED), now, offset),
            "restored the layout saved at 16:23, before this tmux server started at 16:41"
        );
        assert_eq!(
            saved_line(true, true, SAVED, Some(STARTED), now, offset),
            "would restore the layout saved at 16:23, before this tmux server started at 16:41"
        );
        assert_eq!(
            saved_line(false, false, SAVED, Some(STARTED), now, offset),
            "nothing to restore: the layout saved at 16:23, before this tmux server started at \
             16:41, has every session open"
        );
        assert_eq!(
            saved_line(false, true, STARTED + 30, Some(STARTED), now, offset),
            "restored the layout saved at 16:42, after this tmux server started at 16:41"
        );
        assert_eq!(
            saved_line(false, true, SAVED, None, now, offset),
            "restored the layout saved at 16:23"
        );
        assert_eq!(
            saved_line(false, false, SAVED, None, now, offset),
            "nothing to restore: the layout saved at 16:23 has every session open"
        );
    }

    /// The day is said when it is not the one it is compared with: the layout's against the
    /// server's start, the start's against today.
    #[test]
    fn a_time_names_its_day_when_it_is_not_the_day_it_is_compared_with() {
        let offset = |_| 0;
        let a_day = 86_400;
        let now = STARTED + 2 * a_day;
        assert_eq!(
            saved_line(false, true, SAVED - a_day, Some(STARTED), now, offset),
            "restored the layout saved at 2025-03-13 16:23, before this tmux server started at \
             2025-03-14 16:41"
        );
        // The same instants in a zone ten hours ahead: past midnight, so the next day.
        let ahead = |_| 10 * 3_600;
        assert_eq!(
            saved_line(false, true, SAVED, Some(STARTED), STARTED + 60, ahead),
            "restored the layout saved at 02:23, before this tmux server started at 02:41"
        );
        assert_eq!(
            saved_line(false, true, SAVED, Some(STARTED), STARTED + 60, |at| {
                if at == SAVED { 0 } else { 10 * 3_600 }
            }),
            "restored the layout saved at 2025-03-14 16:23, before this tmux server started at 02:41"
        );
    }

    fn home() -> Option<&'static std::path::Path> {
        Some(std::path::Path::new("/home/me"))
    }

    #[test]
    fn the_profile_is_the_folder_above_projects_in_an_absolute_transcript_path() {
        assert_eq!(
            profile_of("/home/me/.claude-alt/projects/-repo-api/abc.jsonl", home()).as_deref(),
            Some("/home/me/.claude-alt")
        );
        // A profile can be deeper than the home, and a space in it is kept as it is.
        assert_eq!(
            profile_of("/srv/my agents/team/projects/p/abc.jsonl", home()).as_deref(),
            Some("/srv/my agents/team")
        );
    }

    #[test]
    fn the_default_profile_has_none() {
        assert_eq!(
            profile_of("/home/me/.claude/projects/-repo/abc.jsonl", home()),
            None
        );
        // A trailing slash on the home is still the home.
        assert_eq!(
            profile_of(
                "/home/me/.claude/projects/-repo/abc.jsonl",
                Some(std::path::Path::new("/home/me/"))
            ),
            None
        );
        // Another home's default is a profile of its own, and so is a folder that only starts like it.
        assert_eq!(
            profile_of("/home/you/.claude/projects/-repo/abc.jsonl", home()).as_deref(),
            Some("/home/you/.claude")
        );
        assert_eq!(
            profile_of("/home/me/.claude-alt/projects/-repo/abc.jsonl", home()).as_deref(),
            Some("/home/me/.claude-alt")
        );
        // With no home to compare with, nothing is the default.
        assert_eq!(
            profile_of("/home/me/.claude/projects/-repo/abc.jsonl", None).as_deref(),
            Some("/home/me/.claude")
        );
    }

    #[test]
    fn a_path_of_another_shape_gives_no_profile() {
        for path in [
            "",
            "projects/p/abc.jsonl",
            "team/projects/p/abc.jsonl",
            "/projects/p/abc.jsonl",
            "/x/projects/abc.jsonl",
            "/x/projects/p/q/abc.jsonl",
            "/x/other/p/abc.jsonl",
            "/x/projects/p/abc.json",
            "/x/projects/p/.jsonl",
            "/x/projects/p/",
            "/x/projects//abc.jsonl",
            "/x//projects/p/abc.jsonl",
            "/x/../projects/p/abc.jsonl",
            "/x/./projects/p/abc.jsonl",
            "/x/projects/../abc.jsonl",
            "/x/projects/p/abc.jsonl\nrm -rf /",
        ] {
            assert_eq!(profile_of(path, home()), None, "{path:?}");
        }
    }

    #[test]
    fn a_path_with_a_control_character_or_over_4096_bytes_gives_no_profile() {
        // Each of these is a well-shaped transcript path in every other way, so the only thing that
        // refuses it is the check for a control character or the length.
        let good = "/x/projects/p/abc.jsonl";
        assert_eq!(profile_of(good, home()).as_deref(), Some("/x"));
        for path in [
            "/x\ny/projects/p/abc.jsonl",
            "/x\ty/projects/p/abc.jsonl",
            "/x\u{7f}y/projects/p/abc.jsonl",
            "/x/projects/p\n/abc.jsonl",
            "/x/projects/p/ab\0c.jsonl",
        ] {
            assert_eq!(profile_of(path, home()), None, "{path:?}");
        }
        let long = format!("/{}/projects/p/abc.jsonl", "a".repeat(4100));
        assert_eq!(profile_of(&long, home()), None);
        let fits = format!("/{}/projects/p/abc.jsonl", "a".repeat(4000));
        assert!(profile_of(&fits, home()).is_some());
    }

    #[test]
    fn the_project_folder_plays_no_part_in_the_profile() {
        // A renamed repository keeps its old transcript folder: the pane's folder is not looked at.
        let pane = PaneSnap {
            cwd: "/work/renamed-api".to_string(),
            ..saved_agent(
                "claude",
                Some("abc"),
                Some("/home/me/.claude-work/projects/-work-old-api/abc.jsonl"),
            )
        };
        let planned = plan_outside_at(&pane, home(), Read::Yes).unwrap().unwrap();
        assert_eq!(
            planned.command,
            "CLAUDE_CONFIG_DIR=/home/me/.claude-work claude --resume abc"
        );
    }

    #[test]
    fn the_line_has_the_profile_before_the_command_quoted_as_the_rest_of_the_line() {
        assert_eq!(
            resume_line("claude", "abc", Some("/home/me/.claude-alt"), &[]).unwrap(),
            "CLAUDE_CONFIG_DIR=/home/me/.claude-alt claude --resume abc"
        );
        assert_eq!(
            resume_line("claude", "abc", Some("/srv/my agents/it's"), &[]).unwrap(),
            "CLAUDE_CONFIG_DIR='/srv/my agents/it'\\''s' claude --resume abc"
        );
        assert_eq!(
            resume_line("claude", "abc", None, &[]).unwrap(),
            "claude --resume abc"
        );
        assert_eq!(
            resume_line("claude", "abc", None, &[]),
            resume_command("claude", "abc")
        );
        // Claude only: another agent's line is what it was, whatever profile it is handed.
        for kind in ["codex", "opencode", "pi", "cursor"] {
            assert_eq!(
                resume_line(kind, "abc", Some("/home/me/.x"), &[]),
                resume_command(kind, "abc"),
                "{kind}"
            );
        }
        assert_eq!(resume_line("gemini", "abc", Some("/home/me/.x"), &[]), None);
    }

    fn flags(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn the_line_has_the_flags_after_the_command_and_before_the_resume_each_word_quoted() {
        let kept = flags(&["--effort", "max", "--model", "x"]);
        assert_eq!(
            resume_line("claude", "abc", Some("/home/me/.claude-work"), &kept).unwrap(),
            "CLAUDE_CONFIG_DIR=/home/me/.claude-work claude --effort max --model x --resume abc"
        );
        assert_eq!(
            resume_line("claude", "abc", None, &kept).unwrap(),
            "claude --effort max --model x --resume abc"
        );
        assert_eq!(
            resume_line(
                "claude",
                "abc",
                None,
                &flags(&["--model", "it's a model", "--dangerously-skip-permissions"])
            )
            .unwrap(),
            "claude --model 'it'\\''s a model' --dangerously-skip-permissions --resume abc"
        );
        // A pane saved with none gets the line it got before.
        assert_eq!(
            resume_line("claude", "abc", None, &[]).unwrap(),
            "claude --resume abc"
        );
    }

    #[test]
    fn a_saved_list_types_no_more_than_the_listed_flags_and_only_for_claude() {
        let more = flags(&[
            "--resume",
            "other",
            "--model",
            "x",
            "-p",
            "--add-dir",
            "/etc",
        ]);
        assert_eq!(
            resume_line("claude", "abc", None, &more).unwrap(),
            "claude --model x --resume abc"
        );
        let kept = flags(&["--effort", "max"]);
        for kind in ["codex", "opencode", "pi", "cursor"] {
            assert_eq!(
                resume_line(kind, "abc", Some("/home/me/.x"), &kept),
                resume_command(kind, "abc"),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_claude_pane_is_planned_with_its_flags_in_the_line_and_for_the_pane_to_keep() {
        let pane = PaneSnap {
            agent_claude_flags: flags(&["--effort", "max", "--model", "x"]),
            ..saved_agent(
                "claude",
                Some("abc"),
                Some("/home/me/.claude-work/projects/-work-api/abc.jsonl"),
            )
        };
        let planned = plan_outside_at(&pane, home(), Read::Yes).unwrap().unwrap();
        assert_eq!(
            planned.command,
            "CLAUDE_CONFIG_DIR=/home/me/.claude-work claude --effort max --model x --resume abc"
        );
        assert_eq!(
            planned.claude_flags,
            flags(&["--effort", "max", "--model", "x"])
        );

        // Another agent with flags saved by hand keeps none, and types none.
        let codex = PaneSnap {
            agent_claude_flags: flags(&["--effort", "max"]),
            ..saved_agent("codex", Some("abc"), None)
        };
        let planned = plan_outside_at(&codex, home(), Read::Yes).unwrap().unwrap();
        assert_eq!(planned.command, "codex resume abc");
        assert!(planned.claude_flags.is_empty());
    }

    #[test]
    fn the_line_a_claude_pane_is_planned_is_the_profile_it_ran_under_and_the_bare_line_for_the_default()
     {
        let under = |path: &str| {
            plan_outside_at(
                &saved_agent("claude", Some("abc"), Some(path)),
                home(),
                Read::Yes,
            )
            .unwrap()
            .unwrap()
            .command
        };
        assert_eq!(
            under("/home/me/.claude-alt/projects/-repo/abc.jsonl"),
            "CLAUDE_CONFIG_DIR=/home/me/.claude-alt claude --resume abc"
        );
        assert_eq!(
            under("/home/me/.claude/projects/-repo/abc.jsonl"),
            "claude --resume abc"
        );
        assert_eq!(
            under("relative/projects/-repo/abc.jsonl"),
            "claude --resume abc"
        );
        let none = plan_outside_at(&saved_agent("claude", Some("abc"), None), home(), Read::Yes)
            .unwrap()
            .unwrap();
        assert_eq!(none.command, "claude --resume abc");
        // Another agent with a path of that shape is not given a Claude profile.
        let codex = plan_outside_at(
            &saved_agent(
                "codex",
                Some("abc"),
                Some("/home/me/.codex-x/projects/-repo/abc.jsonl"),
            ),
            home(),
            Read::Yes,
        )
        .unwrap()
        .unwrap();
        assert_eq!(codex.command, "codex resume abc");
    }

    #[test]
    fn a_claude_pane_resumed_by_id_gets_its_transcript_path_back_with_the_session() {
        // The path is all that carries the profile: a pane put back without it is saved without it,
        // and a second crash would type no `CLAUDE_CONFIG_DIR=`.
        let transcript = "/home/me/.claude-alt/projects/-repo/abc.jsonl";
        let plan = |pane: PaneSnap| plan_outside_at(&pane, home(), Read::Yes).unwrap().unwrap();
        let bare = plan(saved_agent("claude", Some("abc"), Some(transcript)));
        assert_eq!(bare.session_id.as_deref(), Some("abc"));
        assert_eq!(bare.session_path.as_deref(), Some(transcript));
        let flagged = plan(PaneSnap {
            agent_claude_flags: vec!["--effort".to_string(), "max".to_string()],
            ..saved_agent("claude", Some("abc"), Some(transcript))
        });
        assert_eq!(flagged.session_path.as_deref(), Some(transcript));
        // What the saved pane puts back, read again, is the same line: a second crash is the first.
        let again = PaneSnap {
            agent_session_path: flagged.session_path.clone(),
            agent_claude_flags: flagged.claude_flags.clone(),
            ..saved_agent("claude", flagged.session_id.as_deref(), None)
        };
        assert_eq!(plan(again).command, flagged.command);
        assert_eq!(
            flagged.command,
            "CLAUDE_CONFIG_DIR=/home/me/.claude-alt claude --effort max --resume abc"
        );
        // A path the checks refuse is not put back, and a pane with none has none.
        let refused = plan(saved_agent("claude", Some("abc"), Some("relative/x.jsonl")));
        assert_eq!(refused.session_path, None);
        assert_eq!(
            plan(saved_agent("claude", Some("abc"), None)).session_path,
            None
        );
        // Another agent's path is not carried (only Claude's profile is read from it).
        let codex = plan(saved_agent("codex", Some("abc"), Some(transcript)));
        assert_eq!(codex.session_path, None);
    }

    /// A file under `dir` holding `text`, as a path.
    fn file_holding(dir: &tempfile::TempDir, rel: &str, text: &str) -> String {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_string()
    }

    /// A transcript whose earlier turn ran at `low` and whose last record is `tail`.
    fn transcript_with(dir: &tempfile::TempDir, rel: &str, tail: &str) -> String {
        let text = format!("{}\n{tail}\n", turn("low"));
        file_holding(dir, rel, &text)
    }

    fn turn(level: &str) -> String {
        format!("{{\"type\":\"assistant\",\"effort\":\"{level}\",\"perTurnEffort\":\"{level}\"}}")
    }

    #[test]
    fn a_claude_pane_comes_back_at_the_effort_its_last_turn_ran_at() {
        let dir = tempfile::tempdir().unwrap();
        let home = Some(dir.path());
        let path = transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let plan = |pane: &PaneSnap| plan_outside_at(pane, home, Read::Yes).unwrap().unwrap();

        // The saved `--effort low` is replaced, once, in place.
        let started = PaneSnap {
            agent_claude_flags: flags(&["--effort", "low", "--model", "x"]),
            ..saved_agent("claude", Some("abc"), Some(&path))
        };
        let planned = plan(&started);
        assert_eq!(
            planned.command,
            "claude --effort max --model x --resume abc"
        );
        assert_eq!(planned.command.matches("--effort").count(), 1);
        // The flags the plan saves with the pane carry it too: the three agree.
        assert_eq!(
            planned.claude_flags,
            flags(&["--effort", "max", "--model", "x"])
        );

        // None saved: appended, after the flags there are.
        let high = transcript_with(&dir, ".claude/projects/p/def.jsonl", &turn("high"));
        let bare = PaneSnap {
            agent_claude_flags: flags(&["--model", "x"]),
            ..saved_agent("claude", Some("def"), Some(&high))
        };
        let planned = plan(&bare);
        assert_eq!(
            planned.command,
            "claude --model x --effort high --resume def"
        );
        assert_eq!(
            planned.claude_flags,
            flags(&["--model", "x", "--effort", "high"])
        );

        // Saved as the pane was started, a pane with no flags at all gets only the effort.
        let none = saved_agent("claude", Some("def"), Some(&high));
        assert_eq!(plan(&none).command, "claude --effort high --resume def");
    }

    #[test]
    fn a_restore_without_a_level_to_read_types_the_saved_line() {
        let dir = tempfile::tempdir().unwrap();
        let home = Some(dir.path());
        let line = |pane: &PaneSnap| {
            plan_outside_at(pane, home, Read::Yes)
                .unwrap()
                .unwrap()
                .command
        };
        let saved = flags(&["--effort", "low"]);
        let pane = |path: &str| PaneSnap {
            agent_claude_flags: saved.clone(),
            ..saved_agent("claude", Some("abc"), Some(path))
        };
        let as_before = "claude --effort low --resume abc";

        let missing = dir.path().join(".claude/projects/p/none.jsonl");
        assert_eq!(line(&pane(missing.to_str().unwrap())), as_before);
        // A file that has no turn field, and one whose word is not a level.
        let nothing = file_holding(
            &dir,
            ".claude/projects/p/a.jsonl",
            "{\"type\":\"user\",\"effort\":\"max\"}\n",
        );
        assert_eq!(line(&pane(&nothing)), as_before);
        let unknown = file_holding(
            &dir,
            ".claude/projects/p/b.jsonl",
            &format!("{}\n", turn("ultra")),
        );
        assert_eq!(line(&pane(&unknown)), as_before);
        // A pane with no path saved.
        let unsaved = PaneSnap {
            agent_claude_flags: saved.clone(),
            ..saved_agent("claude", Some("abc"), None)
        };
        assert_eq!(line(&unsaved), as_before);
    }

    #[test]
    fn a_pane_of_another_kind_types_its_saved_line_whatever_its_transcript_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let codex = PaneSnap {
            agent_claude_flags: flags(&["--effort", "low"]),
            ..saved_agent("codex", Some("abc"), Some(&path))
        };
        let planned = plan_outside_at(&codex, Some(dir.path()), Read::Yes)
            .unwrap()
            .unwrap();
        assert_eq!(planned.command, "codex resume abc");
        assert!(planned.claude_flags.is_empty());
    }

    #[test]
    fn a_full_flag_list_has_no_room_for_an_effort_it_had_none_of_and_the_three_agree() {
        let dir = tempfile::tempdir().unwrap();
        let home = Some(dir.path());
        let path = transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let plan = |saved: Vec<String>| {
            let pane = PaneSnap {
                agent_claude_flags: saved,
                ..saved_agent("claude", Some("abc"), Some(&path))
            };
            plan_outside_at(&pane, home, Read::Yes).unwrap().unwrap()
        };
        // Sixteen words, no `--effort`: the pair would be the 17th and 18th word, so it is dropped
        // from the line and from the flags the pane is saved with, as any other word past the cap.
        let sixteen: Vec<String> = ["--model", "x"]
            .repeat(8)
            .iter()
            .map(|w| w.to_string())
            .collect();
        assert_eq!(sixteen.len(), 16);
        let planned = plan(sixteen.clone());
        assert_eq!(planned.claude_flags, sixteen);
        let typed = format!("claude {} --resume abc", sixteen.join(" "));
        assert_eq!(planned.command, typed);
        assert!(!planned.command.contains("--effort"));
        // Fifteen words: the same, since the pair does not fit either.
        let fifteen = vec!["--dangerously-skip-permissions".to_string(); 15];
        let planned = plan(fifteen.clone());
        assert_eq!(planned.claude_flags, fifteen);
        assert!(!planned.command.contains("--effort"));
        // Fourteen words: the pair is the 15th and 16th, and both agree on it.
        let fourteen = vec!["--dangerously-skip-permissions".to_string(); 14];
        let planned = plan(fourteen);
        assert_eq!(planned.claude_flags.len(), 16);
        assert!(planned.command.ends_with("--effort max --resume abc"));
        // A full list that already holds an `--effort` has it replaced in place: no new word.
        let mut full = vec!["--dangerously-skip-permissions".to_string(); 14];
        full.extend(["--effort".to_string(), "low".to_string()]);
        let planned = plan(full);
        assert_eq!(planned.claude_flags.len(), 16);
        assert!(planned.command.ends_with("--effort max --resume abc"));
    }

    #[test]
    fn a_plan_reads_its_transcript_once_and_only_where_the_flags_are_used() {
        use crate::claude_flags::reads_so_far;
        let dir = tempfile::tempdir().unwrap();
        let home = Some(dir.path());
        let path = transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let claude = saved_agent("claude", Some("abc"), Some(&path));
        let reads = |work: &dyn Fn()| {
            let before = reads_so_far();
            work();
            reads_so_far() - before
        };

        // One plan, one read: the command and the saved flags come from it.
        assert_eq!(
            reads(&|| {
                let planned = plan_outside_at(&claude, home, Read::Yes).unwrap().unwrap();
                assert_eq!(planned.command, "claude --effort max --resume abc");
                assert_eq!(planned.claude_flags, flags(&["--effort", "max"]));
            }),
            1
        );
        // The pane's key alone needs none: `running_of` and a plan that is told not to.
        assert_eq!(
            reads(&|| {
                let running = running_of(&claude, "%1".to_string()).unwrap();
                assert_eq!(running.key.as_deref(), Some("claude\u{0}id\u{0}abc"));
            }),
            0
        );
        assert_eq!(
            reads(&|| {
                plan_outside_at(&claude, home, Read::No).unwrap().unwrap();
            }),
            0
        );
        // Another agent's flags are none, so its transcript is never opened.
        let codex = saved_agent("codex", Some("abc"), Some(&path));
        assert_eq!(
            reads(&|| {
                plan_outside_at(&codex, home, Read::Yes).unwrap().unwrap();
            }),
            0
        );
        // A reported resume is planned before any read.
        let by_report = PaneSnap {
            agent_session_path: Some(path.clone()),
            ..reported(
                "/",
                "integration:claude",
                "claude",
                &["claude", "--resume", "c-2"],
            )
        };
        assert_eq!(
            reads(&|| {
                plan_outside_at(&by_report, home, Read::Yes)
                    .unwrap()
                    .unwrap();
            }),
            0
        );

        // The decision a restore makes: a plain Claude pane reads once, and a sandboxed pane, a
        // container pane and a reported resume (each of which throws the flags away) read none,
        // whether or not the plan then errors (no such profile here).
        let work = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(work.path());
        let cfg = Config::default();
        let plan = |pane: &PaneSnap| {
            let _ = plan_with(&cfg, &paths, "taw", pane, Read::Yes);
        };
        assert_eq!(reads(&|| plan(&claude)), 1);
        let sandboxed = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            ..claude.clone()
        };
        assert_eq!(reads(&|| plan(&sandboxed)), 0);
        let container = PaneSnap {
            agent_container: Some("docker".to_string()),
            ..claude.clone()
        };
        assert_eq!(reads(&|| plan(&container)), 0);
        let both = PaneSnap {
            agent_sandbox: Some("default".to_string()),
            agent_container: Some("docker".to_string()),
            ..claude.clone()
        };
        assert_eq!(reads(&|| plan(&both)), 0);
        assert_eq!(reads(&|| plan(&by_report)), 0);

        // The two that want only whether there is a plan, or its key (`will_resume`, and `to_resume`
        // behind the windows a restore adds) read none, on the fixture whose plan reads once.
        assert_eq!(reads(&|| plan(&claude)), 1);
        let restore = restore_of(&cfg, &paths);
        assert_eq!(
            reads(&|| assert!(restore.will_resume(&claude))),
            0,
            "will_resume read a transcript"
        );
        assert_eq!(
            reads(&|| {
                let planned = to_resume(&cfg, &paths, "taw", &claude).unwrap().unwrap();
                assert_eq!(planned.key, "claude\u{0}id\u{0}abc");
            }),
            0,
            "to_resume read a transcript"
        );
    }

    #[test]
    fn a_plan_that_is_not_told_to_read_says_the_same_of_a_pane_as_one_that_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let work = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(work.path());
        let cfg = Config::default();
        let panes = [
            saved_agent("claude", Some("abc"), Some(&path)),
            saved_agent("claude", Some("abc"), None),
            saved_agent("claude", None, None),
            saved_agent("codex", Some("abc"), Some(&path)),
            saved_agent("gemini", Some("abc"), None),
            pane_with(Some("abc"), None),
        ];
        for pane in &panes {
            let yes = plan_with(&cfg, &paths, "taw", pane, Read::Yes);
            let no = plan_with(&cfg, &paths, "taw", pane, Read::No);
            assert_eq!(
                yes.as_ref().map(|p| p.as_ref().map(|p| &p.key)),
                no.as_ref().map(|p| p.as_ref().map(|p| &p.key)),
                "{pane:?}"
            );
            assert_eq!(yes.is_err(), no.is_err(), "{pane:?}");
        }
        // The fixture is a pane that is planned at all, so the comparison is not vacuous.
        let claude = &panes[0];
        assert!(
            plan_with(&cfg, &paths, "taw", claude, Read::No)
                .unwrap()
                .is_some()
        );
        assert!(restore_of(&cfg, &paths).will_resume(claude));
    }

    #[test]
    fn a_path_outside_the_transcript_shape_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = Some(dir.path());
        let line = |path: &str| {
            let pane = PaneSnap {
                agent_claude_flags: flags(&["--effort", "low"]),
                ..saved_agent("claude", Some("abc"), Some(path))
            };
            plan_outside_at(&pane, home, Read::Yes)
                .unwrap()
                .unwrap()
                .command
        };
        // Files that carry a level and sit where a transcript does not: no `projects` folder, a
        // `..` in the way, the wrong suffix, a relative path.
        let elsewhere = transcript_with(&dir, "x/other/p/abc.jsonl", &turn("max"));
        let dotdot = format!(
            "{}/.claude/projects/../projects/p/abc.jsonl",
            dir.path().display()
        );
        transcript_with(&dir, ".claude/projects/p/abc.jsonl", &turn("max"));
        let suffix = transcript_with(&dir, ".claude/projects/p/abc.txt", &turn("max"));
        let deep = transcript_with(&dir, ".claude/projects/p/q/abc.jsonl", &turn("max"));
        for path in [
            "/etc/hosts",
            elsewhere.as_str(),
            dotdot.as_str(),
            suffix.as_str(),
            deep.as_str(),
            "projects/p/abc.jsonl",
        ] {
            assert!(
                !line(path).contains("--effort max"),
                "{path:?} was read: {}",
                line(path)
            );
            assert!(line(path).contains("--effort low"), "{path:?}");
        }
        // The same file at the right place is read: the refusals above are about the path.
        let right = format!("{}/.claude/projects/p/abc.jsonl", dir.path().display());
        assert!(line(&right).contains("--effort max"));
    }

    #[test]
    fn a_sandboxed_claude_pane_is_not_given_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let pane = PaneSnap {
            agent_session_path: Some("/srv/p/projects/-repo/4b1f2c.jsonl".to_string()),
            ..sandboxed("claude", "4b1f2c", Some("default"), Some("docker"))
        };
        let (session, built) = one_window(vec![pane]);
        let resumes = plan_resumes(&restore, &session, &built, &mut Report::default());
        assert_eq!(resumes[0].command, "taw sandbox claude -- --resume 4b1f2c");
    }

    #[test]
    fn the_dry_run_lists_the_line_each_pane_would_be_typed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let here = dir.path().display().to_string();
        let pane = |index: u32, kind: &str, id: &str, path: Option<&str>| PaneSnap {
            index,
            cwd: here.clone(),
            ..saved_agent(kind, Some(id), path)
        };
        let (session, _) = one_window(vec![
            pane(
                0,
                "claude",
                "abc",
                Some("/srv/team/projects/-repo/abc.jsonl"),
            ),
            pane(1, "codex", "c0dex", None),
            PaneSnap {
                index: 2,
                cwd: here.clone(),
                ..pane_with(None, None)
            },
            // The same session in a second pane is resumed in the first only.
            pane(
                3,
                "claude",
                "abc",
                Some("/srv/team/projects/-repo/abc.jsonl"),
            ),
        ]);
        let mut plan = Vec::new();
        let would = would_type(&restore, &session, &mut plan);
        assert_eq!(
            plan,
            [
                "  window 0 'agents', pane 0: would type: CLAUDE_CONFIG_DIR=/srv/team claude --resume abc",
                "  window 0 'agents', pane 1: would type: codex resume c0dex",
                "  window 0 'agents', pane 3: would not resume: session 'alpha', window 0 'agents', pane 0 resumes the same session",
            ]
        );
        let lines: Vec<_> = would
            .iter()
            .map(|w| (w.window, w.pane, w.line.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (0, 0, "CLAUDE_CONFIG_DIR=/srv/team claude --resume abc"),
                (0, 1, "codex resume c0dex"),
            ]
        );
    }

    #[test]
    fn the_dry_run_says_why_a_pane_would_not_be_typed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let restore = restore_of(&cfg, &paths);
        let gone = dir.path().join("gone").display().to_string();
        let (session, _) = one_window(vec![
            PaneSnap {
                cwd: gone.clone(),
                ..saved_agent("claude", Some("abc"), None)
            },
            PaneSnap {
                index: 1,
                cwd: dir.path().display().to_string(),
                ..saved_agent("claude", Some("bad id;"), None)
            },
        ]);
        let mut plan = Vec::new();
        let would = would_type(&restore, &session, &mut plan);
        assert!(would.is_empty(), "{plan:?}");
        assert_eq!(
            plan,
            [
                format!(
                    "  window 0 'agents', pane 0: would not type: its saved directory {gone} is gone, so tmux would open it in your home folder; once it can be opened, resume it with: {}",
                    from_directory(&gone, "claude --resume abc")
                ),
                "  window 0 'agents', pane 1: would not resume: saved claude session id \"bad id;\" is not a valid agent session id".to_string(),
            ]
        );
    }

    #[test]
    fn the_dry_run_lists_nothing_when_agents_are_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let mut off = Config::default();
        off.session.resume_agents_on_restore = false;
        let restore = restore_of(&off, &paths);
        let (session, _) = one_window(vec![saved_agent("claude", Some("abc"), None)]);
        let mut plan = Vec::new();
        assert!(would_type(&restore, &session, &mut plan).is_empty());
        assert!(plan.is_empty());
    }

    fn window_of(index: u32, panes: Vec<PaneSnap>) -> snapshot::WindowSnap {
        snapshot::WindowSnap {
            index,
            name: format!("w{index}"),
            layout: "abcd,80x24,0,0,0".to_string(),
            active: false,
            panes,
        }
    }

    fn session_of(name: &str, windows: Vec<snapshot::WindowSnap>) -> SessionSnap {
        SessionSnap {
            name: name.to_string(),
            path: "/repo".to_string(),
            active_window_index: 0,
            windows,
        }
    }

    fn layout_with(sessions: Vec<SessionSnap>) -> snapshot::Snapshot {
        snapshot::Snapshot {
            version: 2,
            taken_unix: 900,
            skipped_panes: 0,
            sessions,
        }
    }

    fn open_running(sessions: &[&str], running: &[(&str, &str)]) -> Open {
        Open {
            sessions: sessions.iter().map(|n| n.to_string()).collect(),
            running: running
                .iter()
                .enumerate()
                .map(|(n, (kind, id))| Running {
                    kind: kind.to_string(),
                    session_id: Some(id.to_string()),
                    session_path: None,
                    key: Some(format!("{kind}\u{0}id\u{0}{id}")),
                    pane: format!("%{n}"),
                })
                .collect(),
            next_window: std::collections::HashMap::new(),
            no_server: false,
        }
    }

    fn what_a_restore_would_resume(saved: &PaneSnap) -> Result<Option<Planned>, String> {
        plan_outside(saved, Read::Yes)
    }

    /// The crash case in miniature: a saved session that is open gets back the windows with an agent
    /// whose conversation runs nowhere, and nothing else: not its plain shell window, not an agent that
    /// runs in another pane, not an agent with no session to resume from.
    #[test]
    fn an_open_session_gets_the_windows_whose_agent_conversation_runs_nowhere() {
        let layout = layout_with(vec![session_of(
            "dotfiles",
            vec![
                window_of(0, vec![pane_with(None, None)]),
                window_of(1, vec![saved_agent("claude", Some("aaaaaaaa"), None)]),
                window_of(2, vec![saved_agent("claude", Some("bbbbbbbb"), None)]),
                window_of(3, vec![saved_agent("claude", None, None)]),
                window_of(
                    4,
                    vec![
                        pane_with(None, None),
                        saved_agent("codex", Some("c0dex"), None),
                    ],
                ),
            ],
        )]);
        // bbbbbbbb runs in a pane of another session: its window is not added.
        let open = open_running(&["dotfiles", "other"], &[("claude", "bbbbbbbb")]);
        let added = windows_to_add(&layout, &open, &what_a_restore_would_resume);
        assert_eq!(added, vec![vec![1, 4]]);
    }

    /// Running means the same kind and the same id: another agent's pane with that id is no proof.
    #[test]
    fn a_pane_of_another_kind_with_the_same_id_does_not_count_as_running() {
        let layout = layout_with(vec![session_of(
            "s",
            vec![window_of(
                1,
                vec![saved_agent("claude", Some("same"), None)],
            )],
        )]);
        let other_kind = open_running(&["s"], &[("codex", "same")]);
        assert_eq!(
            windows_to_add(&layout, &other_kind, &what_a_restore_would_resume),
            vec![vec![0]]
        );
        let same_kind = open_running(&["s"], &[("claude", "same")]);
        assert_eq!(
            windows_to_add(&layout, &same_kind, &what_a_restore_would_resume),
            vec![Vec::<usize>::new()]
        );
    }

    /// A session that is not open is made whole as before (nothing is "added" to it), and a
    /// conversation that a session made whole, or an earlier added window, resumes is not resumed
    /// again in a later window.
    #[test]
    fn a_missing_session_is_made_whole_and_each_conversation_is_resumed_once() {
        let layout = layout_with(vec![
            session_of(
                "gone",
                vec![window_of(0, vec![saved_agent("claude", Some("x1"), None)])],
            ),
            session_of(
                "open",
                vec![
                    window_of(0, vec![saved_agent("claude", Some("x1"), None)]),
                    window_of(1, vec![saved_agent("claude", Some("y1"), None)]),
                    window_of(2, vec![saved_agent("claude", Some("y1"), None)]),
                ],
            ),
        ]);
        let open = open_running(&["open"], &[]);
        let added = windows_to_add(&layout, &open, &what_a_restore_would_resume);
        assert_eq!(added, vec![Vec::<usize>::new(), vec![1]]);
    }

    /// An open session with nothing to add is not something to make: the plain restore then looks
    /// back, as it does when every session is open. With a window to add it is, and does not.
    #[test]
    fn an_open_session_with_a_window_to_add_is_something_to_make() {
        let layout = layout_with(vec![session_of(
            "s",
            vec![window_of(1, vec![saved_agent("claude", Some("a1"), None)])],
        )]);
        let plan = what_a_restore_would_resume;
        assert!(has_a_session_to_make(
            &layout,
            &open_running(&["s"], &[]),
            &plan
        ));
        assert!(!has_a_session_to_make(
            &layout,
            &open_running(&["s"], &[("claude", "a1")]),
            &plan
        ));
        assert!(has_a_session_to_make(
            &layout,
            &open_running(&[], &[]),
            &plan
        ));
    }

    /// After a crash, a new server whose own layout holds only running panes looks back to the old
    /// server's layout, and that layout's agent windows are added to the open session of the same
    /// name.
    #[test]
    fn a_plain_restore_looks_back_when_the_open_sessions_lack_only_agent_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = snapshot_test_paths(tmp.path());
        let dir = paths.sessions_dir.join("session-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let fresh = layout_with(vec![session_of(
            "dotfiles",
            vec![window_of(0, vec![saved_agent("claude", Some("own"), None)])],
        )]);
        let old = layout_with(vec![session_of(
            "dotfiles",
            vec![
                window_of(0, vec![pane_with(None, None)]),
                window_of(1, vec![saved_agent("claude", Some("own"), None)]),
                window_of(2, vec![saved_agent("claude", Some("aaaaaaaa"), None)]),
            ],
        )]);
        std::fs::write(
            paths.sessions_dir.join("default.json"),
            serde_json::to_vec(&snapshot::Snapshot {
                taken_unix: 2000,
                ..fresh
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("session-default-{:039}-1-0.json", 1)),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let open = open_running(&["dotfiles"], &[("claude", "own")]);
        let (snap, used) = plain_source(
            &paths,
            "default",
            Some(1000),
            &open,
            &what_a_restore_would_resume,
        )
        .unwrap();
        assert_eq!(used.taken_unix, 900, "the old layout was read");
        assert_eq!(snap.sessions[0].windows.len(), 3);
        assert_eq!(
            windows_to_add(&snap, &open, &what_a_restore_would_resume),
            vec![vec![2]]
        );
    }

    /// With resuming off nothing is resumed anywhere, so no window of an open session is added for it.
    #[test]
    fn no_window_is_added_with_resume_agents_on_restore_off() {
        let mut cfg = Config::default();
        cfg.session.resume_agents_on_restore = false;
        let paths = Paths::from_env();
        let saved = saved_agent("claude", Some("a1"), None);
        assert_eq!(
            to_resume(&cfg, &paths, "taw", &saved).map(|p| p.is_none()),
            Ok(true)
        );
        let layout = layout_with(vec![session_of("s", vec![window_of(1, vec![saved])])]);
        let off = |p: &PaneSnap| to_resume(&cfg, &paths, "taw", p);
        assert_eq!(
            windows_to_add(&layout, &open_running(&["s"], &[]), &off),
            vec![Vec::<usize>::new()]
        );
    }

    /// An open server holding `running`, with `sessions` open.
    fn open_with(sessions: &[&str], running: Vec<Running>) -> Open {
        Open {
            sessions: sessions.iter().map(|n| n.to_string()).collect(),
            running,
            next_window: std::collections::HashMap::new(),
            no_server: false,
        }
    }

    /// An agent that reports its own resume command and has no session id is running where a pane holds
    /// that command (a restore marks the pane it resumed so, with the command and no id), so a second
    /// restore adds no second copy; another command, or the same one in another directory, is another
    /// conversation.
    #[test]
    fn a_reported_resume_with_no_session_id_is_running_where_a_pane_holds_the_same_command() {
        let saved = reported(
            "/work",
            "herdr",
            "prime-agent",
            &["prime-agent", "--continue"],
        );
        let layout = layout_with(vec![session_of(
            "s",
            vec![window_of(1, vec![saved.clone()])],
        )]);
        let pane = |saved: &PaneSnap| running_of(saved, "%9".to_string()).unwrap();
        let add = |running: Running| {
            windows_to_add(
                &layout,
                &open_with(&["s"], vec![running]),
                &what_a_restore_would_resume,
            )
        };
        assert_eq!(add(pane(&saved)), vec![Vec::<usize>::new()]);
        let other_command = reported("/work", "herdr", "prime-agent", &["prime-agent", "--other"]);
        assert_eq!(add(pane(&other_command)), vec![vec![0]]);
        let other_directory = reported(
            "/else",
            "herdr",
            "prime-agent",
            &["prime-agent", "--continue"],
        );
        assert_eq!(add(pane(&other_directory)), vec![vec![0]]);
        // Nothing is running in a server with no pane that holds it.
        assert_eq!(
            windows_to_add(
                &layout,
                &open_with(&["s"], Vec::new()),
                &what_a_restore_would_resume
            ),
            vec![vec![0]]
        );
    }

    /// A sandboxed pane a restore resumed, whose agent has reported no id yet, holds its conversation in
    /// the kept resume alone: that is running too, in the identity a restore resumes it by.
    #[test]
    fn a_sandbox_pane_that_holds_its_conversation_in_the_kept_resume_is_running_it() {
        let layout = layout_with(vec![session_of(
            "s",
            vec![window_of(1, vec![saved_agent("claude", Some("x1"), None)])],
        )]);
        let marked = PaneSnap {
            agent_kind: None,
            agent_sandbox: Some("default".to_string()),
            ..pane_with(None, None)
        };
        let kept = serde_json::to_string(&snapshot::KeptResume {
            kind: "claude".to_string(),
            session_id: Some("x1".to_string()),
            ..snapshot::KeptResume::default()
        })
        .unwrap();
        // The pane as a save takes it: the profile's mark with the kept resume.
        let taken = snapshot::with_kept_resume(marked.clone(), Some(&kept));
        let running =
            running_of(&taken, "%4".to_string()).expect("the kept resume is a conversation");
        assert_eq!(
            windows_to_add(
                &layout,
                &open_with(&["s"], vec![running]),
                &what_a_restore_would_resume
            ),
            vec![Vec::<usize>::new()]
        );
        // Without the kept resume the mark alone holds no conversation: the pane is not running anything.
        assert!(running_of(&marked, "%4".to_string()).is_none());
        // A kept resume of another conversation is no proof.
        let other = snapshot::with_kept_resume(
            marked,
            Some(
                &serde_json::to_string(&snapshot::KeptResume {
                    kind: "claude".to_string(),
                    session_id: Some("y2".to_string()),
                    ..snapshot::KeptResume::default()
                })
                .unwrap(),
            ),
        );
        assert_eq!(
            windows_to_add(
                &layout,
                &open_with(&["s"], vec![running_of(&other, "%4".to_string()).unwrap()]),
                &what_a_restore_would_resume
            ),
            vec![vec![0]]
        );
    }

    /// A pane that only has the hint of an agent (the restore typed the command, the watcher has not yet
    /// named the agent) holds that agent's conversation as well.
    #[test]
    fn a_pane_named_by_its_hint_alone_is_running_its_session() {
        let hinted = PaneSnap {
            agent_kind: None,
            agent_kind_hint: Some("claude".to_string()),
            ..pane_with(Some("x1"), None)
        };
        let running = running_of(&hinted, "%2".to_string()).unwrap();
        assert_eq!(running.kind, "claude");
        assert_eq!(running.key.as_deref(), Some("claude\u{0}id\u{0}x1"));
        assert!(running_of(&pane_with(Some("x1"), None), "%2".to_string()).is_none());
    }

    /// A hint that is another name of the agent's kind (`agent start --kind cursor-agent` leaves `cursor-agent`)
    /// names the same conversation as the kind's id does: the same kind and the same key.
    #[test]
    fn a_pane_hinted_by_another_name_of_its_kind_runs_the_same_session() {
        let hinted = |hint: &str| PaneSnap {
            agent_kind: None,
            agent_kind_hint: Some(hint.to_string()),
            ..pane_with(Some("x1"), None)
        };
        let by_id = running_of(&hinted("cursor"), "%2".to_string()).unwrap();
        let by_alias = running_of(&hinted("cursor-agent"), "%2".to_string()).unwrap();
        assert!(by_id.key.is_some(), "the id plans a key");
        assert_eq!(by_alias.kind, by_id.kind);
        assert_eq!(by_alias.key, by_id.key);
        // A hint no bundled manifest knows (a label an agent reported) still names the pane by it.
        let unknown = running_of(&hinted("prime-agent"), "%2".to_string()).unwrap();
        assert_eq!(unknown.kind, "prime-agent");
    }

    /// A pane of a session made whole whose conversation already runs in an open pane is made and not
    /// resumed, in the words an added window's pane gets, in a restore and in its dry run.
    #[test]
    fn a_session_made_whole_does_not_resume_a_conversation_that_runs_in_an_open_pane() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        let cfg = Config::default();
        let running = vec![Running {
            kind: "claude".to_string(),
            session_id: Some("abc".to_string()),
            session_path: None,
            key: Some("claude\u{0}id\u{0}abc".to_string()),
            pane: "%7".to_string(),
        }];
        let restore = Restore {
            running: &running,
            ..restore_of(&cfg, &paths)
        };
        let here = dir.path().display().to_string();
        let (session, built) = one_window(vec![
            PaneSnap {
                cwd: here.clone(),
                ..saved_agent("claude", Some("abc"), None)
            },
            PaneSnap {
                index: 1,
                cwd: here,
                ..saved_agent("codex", Some("c0dex"), None)
            },
        ]);
        let mut report = Report::default();
        let resumes = plan_resumes(&restore, &session, &built, &mut report);
        assert_eq!(
            resumes
                .iter()
                .map(|r| r.command.as_str())
                .collect::<Vec<_>>(),
            ["codex resume c0dex"]
        );
        assert_eq!(
            report.plan,
            ["skip resume in pane %1: pane %7 already runs the same session"]
        );
        // A dry run is another restore, with claims of its own.
        let dry = Restore {
            running: &running,
            ..restore_of(&cfg, &paths)
        };
        let mut plan = Vec::new();
        let would = would_type(&dry, &session, &mut plan);
        assert_eq!(would.len(), 1);
        assert_eq!(
            plan[0],
            "  window 0 'agents', pane 0: would not resume: pane %7 already runs the same session"
        );
    }

    /// The attach line quotes the target, which zsh would otherwise read as a command's path.
    #[test]
    fn the_attach_line_quotes_the_target_and_names_the_session_tmux_made() {
        assert_eq!(attach_line("dotfiles"), "tmux attach -t '=dotfiles'");
        assert_eq!(attach_line("a.b"), "tmux attach -t '=a_b'");
    }
}
