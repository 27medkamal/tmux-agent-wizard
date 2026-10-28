//! `taw sandbox`: an agent in a container, in the pane it is typed in, and the profiles that put it
//! there (README "Agents in a sandbox"). The start is for a person at a shell, as `taw open` is: it
//! becomes the profile when it works, so it prints nothing of its own, and says why on stderr, in
//! one line, when it does not. `new` and `list` print the usual envelope.

use std::os::unix::process::CommandExt;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::envelope::CliError;
use super::{Ctx, Sub};
use crate::sandbox::profile::{self, Missing, NewError};
use crate::sandbox::{report, start};
use crate::store;

/// `taw sandbox [<agent>] [--profile <name>] [-- <the agent's own arguments>]`, or one of its two
/// words of its own.
#[derive(Args, Debug)]
#[command(
    args_conflicts_with_subcommands = true,
    subcommand_precedence_over_arg = true
)]
pub struct SandboxArgs {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    /// The agent to run, as `taw manifest list` names it (claude when left out)
    #[arg(value_name = "AGENT", value_parser = agent_name)]
    pub agent: Option<String>,
    /// The profile to run it with: one of yours, or default. Left out: the agent's line in
    /// [sandbox.agents], else [sandbox] profile, else default
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// The agent's own arguments, after --
    #[arg(last = true, value_name = "AGENT_ARG")]
    pub agent_args: Vec<String>,
}

/// An agent's name as `taw sandbox` takes it: anything but its own two words, which a flag typed
/// before them would otherwise let through as an agent's.
fn agent_name(value: &str) -> Result<String, String> {
    if matches!(value, "new" | "list") {
        Err(format!(
            "{value} is a word of taw sandbox's own, and comes right after it: taw sandbox {value}"
        ))
    } else {
        Ok(value.to_string())
    }
}

/// The flags other commands place an agent with. `taw sandbox` has none: it runs in the pane it is
/// typed in.
const PLACEMENT_FLAGS: [&str; 6] = [
    "--split",
    "--window",
    "--session",
    "--worktree",
    "--here",
    "--no-focus",
];

/// What the refusal of a placement flag says in place of clap's own tip, which is to pass it after
/// `--`, where it would go to the agent.
const PLACEMENT_TIP: &str = "taw sandbox runs the agent in the pane it is typed in: make the place first (prefix % or prefix \" for a pane beside this one, prefix c for a window, the popup's n or w for a session or a worktree), then type taw sandbox there";

/// clap's refusal of an argument `taw sandbox`'s start does not know, with [`PLACEMENT_TIP`] for its
/// tip when the argument is a placement flag; any other error as it is.
pub(crate) fn placement_tip(mut err: clap::Error) -> clap::Error {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    // clap names `--split=right` as `--split`.
    let placement = match err.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(arg)) => PLACEMENT_FLAGS.contains(&arg.as_str()),
        _ => false,
    };
    // The usage line clap draws is the start's, which shows what was typed (`--profile <NAME>`,
    // `<AGENT>`), unless it is one of the two words'.
    let start = match err.get(ContextKind::Usage) {
        Some(ContextValue::StyledStr(usage)) => {
            let usage = usage.to_string();
            usage.contains("taw sandbox")
                && !usage.contains("taw sandbox new")
                && !usage.contains("taw sandbox list")
        }
        _ => false,
    };
    if err.kind() == ErrorKind::UnknownArgument && placement && start {
        err.insert(
            ContextKind::Suggested,
            ContextValue::StyledStrs(vec![PLACEMENT_TIP.into()]),
        );
    }
    err
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Copy a profile, taw's default unless named, into your profiles folder, executable, and
    /// print its path
    New {
        /// The new profile's name: a letter, then letters, digits, - and _, ending in a letter or
        /// digit. default makes your own copy of taw's, which then runs in its place
        #[arg(value_name = "NAME")]
        name: String,
        /// The profile to copy instead of taw's default
        #[arg(long, value_name = "PROFILE")]
        from: Option<String>,
    },
    /// The profiles taw can run: name, file, and the agents the config gives each
    List,
}

/// The start: finds the profile, writes taw's own where it runs from when it is the one, empties
/// the pane's report folder and marks the pane with the profile, then becomes the profile. Returns
/// only when it could not.
pub(crate) fn start(ctx: &Ctx, args: &SandboxArgs) -> anyhow::Result<()> {
    let here = start::Here::from_env().context("could not tell which directory this is")?;
    // Before anything is made of it: TMUX_PANE names the pane's report folder, which is emptied,
    // and the pane that is marked, and anything can set it.
    let server = match &here.pane {
        Some(pane) => Some(this_pane(ctx, pane)?),
        None => None,
    };
    let request = start::Request {
        agent: args.agent.as_deref(),
        profile: args.profile.as_deref(),
        args: &args.agent_args,
    };
    // The profile is the flag's, else the config's, read again here and whole: a config taw cannot
    // read whole says nothing for sure, and the defaults could quietly run a weaker sandbox than the
    // one you chose.
    let loaded;
    let cfg = match &args.profile {
        Some(_) => &ctx.cfg,
        None => {
            loaded =
                crate::config::load_for_sandbox(&ctx.paths).map_err(start::StartError::Config)?;
            &loaded
        }
    };
    let prepared = start::prepare(cfg, &ctx.paths, server.as_deref(), &request, &here)?;
    // Before the pane is marked: a start that stops here leaves no mark behind.
    if prepared.profile.builtin {
        profile::write_builtin(&ctx.paths)
            .context("could not write taw's own profile, so nothing was started")?;
    }
    match (&here.pane, &prepared.report) {
        (Some(pane), Some(folder)) => {
            report::empty(folder).with_context(|| {
                format!(
                    "could not empty {}, so nothing was started",
                    folder.display()
                )
            })?;
            // The profile's mark, and this process's pid: `exec` keeps it through the profile and
            // the runtime, so its end is the profile's, however soon that comes.
            store::mark_sandbox(&ctx.tmux, pane, &prepared.profile.name, std::process::id())
                .context("tmux did not take the pane's mark, so nothing was started")?;
        }
        _ => eprintln!(
            "taw sandbox: {}, so this agent gets no popup row and no restore",
            if here.tmux {
                "not in a tmux pane"
            } else {
                "not in tmux"
            }
        ),
    }
    let err = prepared.command().exec();
    if let Some(pane) = &here.pane {
        let _ = store::forget_sandbox(&ctx.tmux, pane);
    }
    Err(anyhow::anyhow!(
        "could not run the profile {}: {err}",
        profile::shown(&prepared.profile.path)
    ))
}

/// The name of the server the report folders are kept by, for the pane `TMUX_PANE` names, once it is
/// seen to be a pane id in tmux's form, a pane tmux knows, and the pane at whose prompt this was
/// typed: marking another would take the session of the agent in front of it off.
fn this_pane(ctx: &Ctx, pane: &str) -> anyhow::Result<String> {
    if !start::is_pane_id(pane) {
        return Err(start::StartError::NotAPane(pane.to_string()).into());
    }
    let known = report::ask(&ctx.tmux, pane)
        .with_context(|| format!("tmux did not say what pane {pane} is, so nothing was started"))?
        .ok_or_else(|| start::StartError::NoSuchPane(pane.to_string()))?;
    if let Some(why) = start::not_at_the_prompt_of(&known.tty) {
        return Err(start::StartError::NotAtThePrompt {
            pane: pane.to_string(),
            why,
        }
        .into());
    }
    Ok(known.server)
}

/// The line a refused start says: what failed, then why, as `taw open` says its own.
fn line(err: &anyhow::Error) -> String {
    format!("taw sandbox: {}", super::open::why(err))
}

/// `taw sandbox`'s failure for a person: one line on stderr. Returns the exit status, 1.
pub(crate) fn report(err: &anyhow::Error) -> i32 {
    eprintln!("{}", line(err));
    1
}

/// `new` and `list`, which print the usual envelope. The start never comes here: `dispatch` runs it.
pub(crate) fn run(ctx: &Ctx, args: SandboxArgs) -> (Sub, anyhow::Result<Value>) {
    match args.cmd {
        Some(Cmd::New { name, from }) => ("new", new(ctx, &name, from.as_deref())),
        Some(Cmd::List) => ("list", list(ctx)),
        None => unreachable!("the start is dispatched before dispatch_envelope"),
    }
}

fn new(ctx: &Ctx, name: &str, from: Option<&str>) -> anyhow::Result<Value> {
    let made = profile::new_profile(&ctx.paths, name, from).map_err(|err| match &err {
        NewError::From(Missing::NoSuch { .. }) => CliError::not_found(err.to_string()),
        NewError::Io { .. } => anyhow::Error::new(err),
        NewError::Name(_)
        | NewError::NoFolder
        | NewError::TawsOwnFolder { .. }
        | NewError::Folder(_)
        | NewError::Exists { .. }
        | NewError::From(_) => CliError::invalid_argument(err.to_string()),
    })?;
    Ok(json!({ "name": made.name, "path": made.path, "from": made.from }))
}

fn list(ctx: &Ctx) -> anyhow::Result<Value> {
    Ok(json!({
        "profiles": profile::list(&ctx.paths, &ctx.cfg.sandbox),
        "refused": profile::refused(&ctx.paths),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_start_is_one_line_after_its_name_and_status_1() {
        let err = anyhow::Error::new(crate::sandbox::start::StartError::NoSuchAgent(
            "claud".into(),
        ));
        assert_eq!(
            line(&err),
            "taw sandbox: taw knows no agent called claud (taw manifest list names them)"
        );
        assert_eq!(report(&err), 1);
    }

    /// Every way a start is refused reads as one line after the command's name, and a failure with
    /// a cause says the cause after it, as `taw open` does: the reason tmux gave, not the command
    /// line it ran.
    #[test]
    fn every_refusal_reads_as_one_line_and_a_cause_follows_what_failed() {
        use crate::sandbox::profile::Missing;
        let name = || "max".to_string();
        let path = || "~/.config/tmux-agent-wizard/sandbox/max".to_string();
        for (missing, said) in [
            (
                Missing::BadName("../x".into()),
                "../x is not a profile's name (a letter, then letters, digits, - and _, ending in a letter or digit)",
            ),
            (
                Missing::NoSuch {
                    name: "team".into(),
                    names: "default, max".into(),
                    dir: "~/.config/tmux-agent-wizard/sandbox/".into(),
                },
                "no profile called team (yours: default, max, in ~/.config/tmux-agent-wizard/sandbox/)",
            ),
            (
                Missing::FolderGone {
                    which: "profiles folder",
                    path: "~/.config/tmux-agent-wizard/sandbox".into(),
                },
                "your profiles folder ~/.config/tmux-agent-wizard/sandbox is a link to a folder that is not there (ls -ld ~/.config/tmux-agent-wizard/sandbox)",
            ),
            (
                Missing::NoFolder { name: name() },
                "no profile called max (TAW_CONFIG_DIR is not a full path, so taw reads no profiles of yours)",
            ),
            (
                Missing::NotExecutable {
                    name: name(),
                    path: path(),
                },
                "the profile max is not executable (chmod +x ~/.config/tmux-agent-wizard/sandbox/max)",
            ),
            (
                Missing::BrokenLink {
                    name: name(),
                    path: path(),
                },
                "the profile max is a link to a file that is not there (ls -l ~/.config/tmux-agent-wizard/sandbox/max)",
            ),
            (
                Missing::NotAFile {
                    name: name(),
                    path: path(),
                },
                "the profile max is not a file (ls -ld ~/.config/tmux-agent-wizard/sandbox/max)",
            ),
            (
                Missing::CannotLook {
                    name: name(),
                    path: path(),
                    why: "too many levels of symbolic links".into(),
                },
                "taw cannot look at the profile max: too many levels of symbolic links (ls -ld ~/.config/tmux-agent-wizard/sandbox/max)",
            ),
        ] {
            let err = anyhow::Error::new(crate::sandbox::start::StartError::Profile(missing));
            assert_eq!(line(&err), format!("taw sandbox: {said}"));
            assert!(!line(&err).contains('\n'), "{}", line(&err));
        }

        use std::os::unix::process::ExitStatusExt;
        let failed = anyhow::Error::new(crate::tmux::CommandFailed {
            command: "tmux -u display-message -p #{socket_path}".to_string(),
            status: std::process::ExitStatus::from_raw(256),
            stderr: "no server running on /private/tmp/tmux-501/default\n".to_string(),
        })
        .context("tmux did not answer, so nothing was started");
        assert_eq!(
            line(&failed),
            "taw sandbox: tmux did not answer, so nothing was started: no server running on /private/tmp/tmux-501/default"
        );
    }
}
