//! `taw hooks list | test | log`: what `[hooks]` does, and how to see it.
//!
//! `exec` is not for people. It is what taw starts for each hook an event fires: it runs the
//! command under its time limit, with the event's JSON on standard input, and writes a failure
//! to the hooks log. See [`crate::hooks::run`] for why there is a process for it.

use std::io::Read;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Value, json};

use super::envelope::{CliError, UsageRefusal};
use super::{Ctx, Sub, common};
use crate::hooks::events::{self, EVENTS, VETOES};
use crate::hooks::{context, log, run, sample};
use crate::plugin::PluginCtx;
use crate::session::create::{Streams, hook_limit};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print every event name a hook can take, when it fires, and the commands hooked to it
    List,
    /// Run the hooks of one event against a sample whose pane, window and session ids name
    /// nothing on any tmux server, or with --pane a real pane's, and show what each one did
    Test {
        /// An event name from `taw hooks list`
        event: String,
        /// Tell the event about this real pane, with its own ids, session, window and
        /// directory, so that a hook that acts on them acts on that pane
        #[arg(long, value_name = "PANE")]
        pane: Option<String>,
        /// Run an event about something going away (a close, a removal or a process ending) on a
        /// real pane all the same, which --pane refuses without it
        #[arg(long, requires = "pane")]
        force: bool,
        /// Send the notification sample as one about this machine of [machines], so that a hook
        /// is told `TAW_MACHINE` as it is for a toast from there
        #[arg(long, value_name = "NAME", conflicts_with = "pane")]
        machine: Option<String>,
    },
    /// Print the end of the hooks log, where a hook that failed in the background left its
    /// line
    Log {
        /// How many lines to print
        #[arg(long, value_name = "N", default_value_t = 20)]
        lines: usize,
    },
    /// Run one hook under its time limit and log its failure, which is what taw starts for
    /// every hook an event fires
    #[command(hide = true)]
    Exec {
        /// The event, for the log line
        #[arg(long)]
        event: String,
        /// The time limit in seconds; 0 means none
        #[arg(long, allow_negative_numbers = true)]
        limit: f64,
        /// Where a failure is written
        #[arg(long)]
        log: PathBuf,
        /// The command line, run through sh
        #[arg(last = true)]
        command: String,
    },
}

pub(crate) fn run(ctx: &Ctx, cmd: Cmd) -> (Sub, anyhow::Result<Value>) {
    match cmd {
        Cmd::List => ("list", Ok(list(ctx))),
        Cmd::Test {
            event,
            pane,
            force,
            machine,
        } => (
            "test",
            test(ctx, &event, pane.as_deref(), force, machine.as_deref()),
        ),
        Cmd::Log { lines } => ("log", Ok(show_log(ctx, lines))),
        Cmd::Exec {
            event,
            limit,
            log,
            command,
        } => ("exec", exec(&event, limit, &log, &command)),
    }
}

/// Every name in [`EVENTS`] and then [`VETOES`], with what is hooked to it, and after them any
/// name the config gives commands to that taw never fires.
fn list(ctx: &Ctx) -> Value {
    let entry = |kind: &str, event: &events::Event| {
        json!({
            "name": event.name,
            "kind": kind,
            "when": event.when,
            "commands": ctx.cfg.hooks.commands(event.name),
        })
    };
    let hooks: Vec<Value> = EVENTS
        .iter()
        .map(|event| entry("event", event))
        .chain(VETOES.iter().map(|event| entry("veto", event)))
        .collect();
    let unknown: Vec<Value> = ctx
        .cfg
        .hooks
        .configured()
        .filter(|(name, _)| !events::is_known(name))
        .map(|(name, commands)| json!({ "name": name, "commands": commands }))
        .collect();
    json!({
        "timeout_seconds": ctx.cfg.hooks.timeout_seconds,
        "veto_timeout_seconds": ctx.cfg.sessions.hook_timeout_seconds,
        "log": log::path(&ctx.paths),
        "variables": context::VARIABLES,
        "sample": sample::note(),
        "hooks": hooks,
        "unknown": unknown,
    })
}

/// Fires the sample for `event` at its hooks, one after another, waiting for each, and reports
/// them. The hooks run for real, in a directory taw makes for the purpose under the state
/// directory and takes away again, with the time limit they would have: the background limit
/// for an event, and the create hooks' limit for the two that can refuse. The sample's ids name
/// nothing on any server and `TAW_TEST` is `1` ([`sample`]).
///
/// With `pane` the event is told about that real pane instead: its ids, its session, its window
/// and its directory, which are the hook's own, so a hook that acts on them acts on it. The
/// output says so. Nothing is made for such a test, so nothing is taken away.
///
/// An event about something going away ([`events::goes_away`]) is refused with a pane unless
/// `force`: the hook would close, remove or respawn something that is really there. The refusal
/// comes before anything is looked up or run ([`UsageRefusal`]).
fn test(
    ctx: &Ctx,
    event: &str,
    pane: Option<&str>,
    force: bool,
    machine: Option<&str>,
) -> anyhow::Result<Value> {
    if !events::is_known(event) {
        return Err(CliError::invalid_argument(format!(
            "'{event}' is not an event a hook can take; run `taw hooks list` for the names"
        )));
    }
    if let Some(name) = machine {
        check_machine(ctx, event, name)?;
    }
    if let Some(target) = pane {
        if events::goes_away(event) && !force {
            return Err(UsageRefusal(sample::forced_message(event, target)).into());
        }
        return test_about(ctx, event, &real_story(ctx, target)?, None);
    }
    // A directory of its own for this run, so that two tests at once do not take each other's
    // away.
    let tests = ctx.paths.state_dir.join("hooks-test");
    let root = tests.join(std::process::id().to_string());
    let result = test_in(ctx, event, &root, machine);
    let _ = std::fs::remove_dir_all(&root);
    // Only when nothing else is testing: a directory that is not empty stays.
    let _ = std::fs::remove_dir(&tests);
    result
}

/// What `--machine` may be: the name of a machine of `[machines]`, and only for the event a machine's toast
/// fires. An unknown name is refused in the words `taw --machine` uses, before any hook runs.
fn check_machine(ctx: &Ctx, event: &str, name: &str) -> anyhow::Result<()> {
    let machines = match crate::machine::config::load_machines(&ctx.paths) {
        Ok(machines) => machines,
        Err(doubt) => return Err(CliError::invalid_argument(doubt.to_string())),
    };
    if !machines.iter().any(|machine| machine.name == name) {
        return Err(UsageRefusal(crate::machine::forward::unknown_name(
            name, &machines, &ctx.paths,
        ))
        .into());
    }
    if event != events::NOTIFICATION {
        return Err(CliError::invalid_argument(format!(
            "--machine tells a hook of a notification about a machine: it takes the event {}, not {event}",
            events::NOTIFICATION
        )));
    }
    Ok(())
}

fn test_in(ctx: &Ctx, event: &str, root: &Path, machine: Option<&str>) -> anyhow::Result<Value> {
    let (session_dir, checkout) = sample::dirs(root);
    for dir in [&session_dir, &checkout] {
        std::fs::create_dir_all(dir)?;
    }
    test_about(ctx, event, &sample::Story::invented(root), machine)
}

/// The story of the pane `target` names, in any way another verb takes one, with its session and
/// its window as tmux lists them. A pane that is not there is `not_found`, before any hook runs.
fn real_story(ctx: &Ctx, target: &str) -> anyhow::Result<sample::Story> {
    let pane = common::pane_info(&ctx.tmux, target)?;
    let session = common::session_info(&ctx.tmux, &pane.session_id)?;
    let window = common::list_windows(&ctx.tmux)?
        .into_iter()
        .find(|window| window.window_id == pane.window_id)
        .ok_or_else(|| CliError::not_found(format!("pane '{target}' not found")))?;
    Ok(sample::Story::of(pane, session, window))
}

/// Runs the hooks of `event` against the sample about `story` and reports them.
fn test_about(
    ctx: &Ctx,
    event: &str,
    story: &sample::Story,
    machine: Option<&str>,
) -> anyhow::Result<Value> {
    let built = sample::built_of(event, story)
        .ok_or_else(|| anyhow::anyhow!("no sample for the event '{event}'"))?;
    let built = match machine {
        Some(name) => built.about_machine(name),
        None => built,
    };

    let veto = VETOES.iter().any(|candidate| candidate.name == event);
    let limit = hook_limit(if veto {
        ctx.cfg.sessions.hook_timeout_seconds
    } else {
        ctx.cfg.hooks.timeout_seconds
    });
    let home = dirs::home_dir().unwrap_or_default();
    let dir = run::hook_dir(&built.env, &home);
    // What a real run gives a hook: the environment tmux gives a new pane of the session the event
    // is about (only a real pane's has one, so a test without `--pane` has tmux's global
    // environment alone) and `TMUX` beside the `TAW_*` variables. Only the `TAW_*` ones are
    // printed, below: the rest may hold secrets.
    let env = crate::hooks::environment(
        &PluginCtx {
            tmux: &ctx.tmux,
            cfg: &ctx.cfg,
            paths: &ctx.paths,
            taw_bin: &ctx.taw_bin,
            server_env: None,
        },
        &built.env,
    );
    let commands = ctx.cfg.hooks.commands(event);
    // Read as a real run reads it: a hook that can refuse is answered when its shell ends.
    let streams = if veto {
        Streams::SHOWN_VETO
    } else {
        Streams::SHOWN
    };
    let hooks: Vec<Value> = commands
        .iter()
        .map(|command| {
            let outcome = run::run(
                command,
                &home,
                &env,
                built.json.as_bytes(),
                dir.as_deref(),
                limit,
                streams,
            );
            report(command, dir.as_deref(), &outcome)
        })
        .collect();
    let env: serde_json::Map<String, Value> = built
        .env
        .iter()
        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
        .collect();
    Ok(json!({
        "event": event,
        "kind": if veto { "veto" } else { "event" },
        "limit_seconds": limit.map(|limit| limit.as_secs_f64()),
        "count": hooks.len(),
        "note": commands.is_empty().then(|| format!(
            "no hooks are configured for {event}; add one under [hooks] in {}",
            ctx.paths.config_file.display()
        )),
        "sample": if story.real { sample::real_note(story) } else { sample::note() },
        "real_pane": story.real.then(|| story.pane.pane_id.clone()),
        "env": env,
        "input": serde_json::from_str::<Value>(&built.json)?,
        "hooks": hooks,
    }))
}

/// One hook's result: what ran, where, and how it ended.
fn report(command: &str, dir: Option<&Path>, outcome: &run::Outcome) -> Value {
    let status = match outcome {
        run::Outcome::Ended { status, .. } => status.code(),
        _ => None,
    };
    json!({
        "command": command,
        "dir": dir,
        "ok": outcome.failure().is_none(),
        "status": status,
        "timed_out": matches!(outcome, run::Outcome::TimedOut(_)),
        "failure": outcome.failure(),
        "stdout": outcome.stdout_shown(),
        "stderr": outcome.stderr_shown(),
    })
}

fn show_log(ctx: &Ctx, lines: usize) -> Value {
    let path = log::path(&ctx.paths);
    let lines = log::tail(&path, lines);
    json!({ "path": path, "count": lines.len(), "lines": lines })
}

fn exec(event: &str, limit: f64, log: &Path, command: &str) -> anyhow::Result<Value> {
    let mut input = Vec::new();
    // A caller that went away before it wrote leaves less than the whole JSON: the hook
    // still runs, with what there is.
    let _ = std::io::stdin().read_to_end(&mut input);
    let home = dirs::home_dir().unwrap_or_default();
    let outcome = run::supervise(log, event, command, &home, &input, hook_limit(limit));
    Ok(json!({ "event": event, "failed": outcome.failure() }))
}
