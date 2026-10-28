//! What `taw sandbox` gives a profile: it runs the profile in the
//! agent's pane, in the project directory, as `<profile> <the agent's command> [<its
//! arguments>...]`, with five variables added to the pane's own environment. What follows the
//! profile's name is exactly the command line taw would have run on this machine.

use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;
use crate::paths::Paths;
use crate::sandbox::{profile, report};

/// The agent `taw sandbox` starts when it is given none.
pub const DEFAULT_AGENT: &str = "claude";

/// The five variables, in a fixed order.
pub const VARIABLES: [&str; 5] = [
    "TAW_SANDBOX_DIR",
    "TAW_SANDBOX_AGENT",
    "TAW_SANDBOX_REPORT",
    "TAW_SANDBOX_PROFILE",
    "TAW_SANDBOX_PANE",
];

/// What `taw sandbox` was asked for.
pub struct Request<'a> {
    pub agent: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub args: &'a [String],
}

/// Where it was asked: the pane it was typed in, when that is in tmux, whether it is in tmux at all,
/// and the directory, which is the project's.
pub struct Here {
    pub pane: Option<String>,
    /// `$TMUX` says a server, with or without a pane: a popup's command and a hook's `run-shell` have
    /// it and no `$TMUX_PANE`.
    pub tmux: bool,
    pub dir: PathBuf,
}

impl Here {
    /// This process's own surroundings: in a pane when both `$TMUX` and `$TMUX_PANE` say one.
    pub fn from_env() -> std::io::Result<Here> {
        let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Ok(Here {
            pane: set("TMUX").and(set("TMUX_PANE")),
            tmux: set("TMUX").is_some(),
            dir: std::env::current_dir()?,
        })
    }
}

/// Why `taw sandbox` starts nothing, in the words it says after `taw sandbox: `.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("taw knows no agent {} (taw manifest list names them)", called(.0))]
    NoSuchAgent(String),
    #[error(transparent)]
    Profile(#[from] profile::Missing),
    /// No `--profile`, and a config that does not say for sure which profile you chose.
    #[error(
        "{0}, so taw cannot tell which profile you chose: fix it (taw config check says what is wrong), or name one with --profile"
    )]
    Config(crate::config::SandboxDoubt),
    /// `TMUX_PANE` is not a pane id as tmux gives one: nothing is made of it.
    #[error("TMUX_PANE is {0:?}, which is no tmux pane (a % and digits), so nothing was started")]
    NotAPane(String),
    /// Not typed at the prompt of the pane `TMUX_PANE` names: an agent's tool call, a job sent to the
    /// background, a terminal that took a pane's environment along. Marked, that pane would lose the
    /// session of the agent in front of it.
    #[error(
        "not typed at the prompt of pane {pane} ({why}), so nothing was started: type it at the prompt of the pane it should take over (agents and scripts start one with taw agent start --sandbox)"
    )]
    NotAtThePrompt { pane: String, why: &'static str },
    /// The state folder is not a full path (an empty or relative `TAW_STATE_DIR`): found from wherever
    /// taw runs, which is the project, it would hold taw's own profile and the pane's report folder.
    #[error(
        "TAW_STATE_DIR is not a full path, which would put taw's own profile and the pane's report folder in this folder, so nothing was started"
    )]
    StateNotAFullPath,
    /// tmux knows no pane by the id `TMUX_PANE` gives.
    #[error("tmux knows no pane {0}, so nothing was started")]
    NoSuchPane(String),
    /// The server's name or the pane's is no plain name of a folder, which the report folder needs.
    #[error(
        "taw keeps no report folder for pane {pane:?} of the tmux server {server:?}, so nothing was started"
    )]
    NoReportFolder { server: String, pane: String },
}

/// An agent's name as the sentence says it: an empty one is "with an empty name", never a hole.
fn called(name: &str) -> String {
    if name.is_empty() {
        "with an empty name".to_string()
    } else {
        format!("called {name}")
    }
}

/// Whether `pane` is a pane id as tmux gives one, `%` and ASCII digits: the only form `TMUX_PANE` is
/// taken in. It names the pane's report folder, which taw empties, and the pane taw marks, and
/// anything can set it: a project's mise.toml or .envrc, a script.
pub fn is_pane_id(pane: &str) -> bool {
    pane.strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Why this process is not at the prompt of the pane whose terminal is `tty`, or `None` when it is:
/// its input is a terminal, that pane's own, and its process group is the one in front of it. A line
/// typed at the prompt is that, and so is a script it runs; an agent's tool call, a job sent to the
/// background and a terminal that took a pane's environment along are not.
pub fn not_at_the_prompt_of(tty: &std::path::Path) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: isatty, tcgetpgrp and getpgrp only read the state of this process and of its standard
    // input, which they are handed by number.
    if unsafe { libc::isatty(0) } != 1 {
        return Some("its input is no terminal");
    }
    let input = std::fs::metadata("/dev/stdin").map(|meta| meta.rdev());
    let pane = std::fs::metadata(tty).map(|meta| meta.rdev());
    match (input, pane) {
        (Ok(input), Ok(pane)) if input == pane => {}
        _ => return Some("its terminal is not that pane's"),
    }
    if unsafe { libc::tcgetpgrp(0) } != unsafe { libc::getpgrp() } {
        return Some("it is not the job in front of that pane");
    }
    None
}

/// A start, ready to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub profile: profile::Profile,
    /// The agent's id, as taw names it (`cursor` for `cursor-agent`).
    pub agent: String,
    /// What follows the profile's name: the agent's command, then its own arguments.
    pub command: Vec<String>,
    /// The variables added, in that order: five in a pane, three outside tmux.
    pub env: Vec<(String, String)>,
    pub dir: PathBuf,
    /// The pane's report folder, in a pane.
    pub report: Option<PathBuf>,
}

/// The agent's id for a name a person types: an id (`codex`), or another name of it
/// (`cursor-agent`, an alias a manifest declares).
pub fn agent_id(name: &str) -> Option<String> {
    crate::detect::identify(name, Some(name))
}

/// What `taw sandbox` runs for `req` here: the agent, the profile that runs it, and what the profile
/// is given. `server` is the tmux server's name for the report folder ([`report::ask`]), given in a
/// pane. A pane that is not in tmux's form ([`is_pane_id`]) is refused first, and so is a server's
/// name or a pane's that is no folder's.
pub fn prepare(
    cfg: &Config,
    paths: &Paths,
    server: Option<&str>,
    req: &Request<'_>,
    here: &Here,
) -> Result<Prepared, StartError> {
    if let Some(pane) = &here.pane
        && !is_pane_id(pane)
    {
        return Err(StartError::NotAPane(pane.clone()));
    }
    if !paths.state_dir.is_absolute() {
        return Err(StartError::StateNotAFullPath);
    }
    let named = req.agent.unwrap_or(DEFAULT_AGENT);
    let agent = agent_id(named).ok_or_else(|| StartError::NoSuchAgent(named.to_string()))?;
    let profile = profile::find(paths, profile::pick(&cfg.sandbox, &agent, req.profile))?;
    let mut command = vec![crate::detect::executable(&agent)];
    command.extend(req.args.iter().cloned());
    let report = match (&here.pane, server) {
        (Some(pane), Some(server)) => Some(report::folder(paths, server, pane).ok_or_else(
            || StartError::NoReportFolder {
                server: server.to_string(),
                pane: pane.clone(),
            },
        )?),
        _ => None,
    };
    let mut env = vec![
        (
            "TAW_SANDBOX_DIR".to_string(),
            here.dir.display().to_string(),
        ),
        ("TAW_SANDBOX_AGENT".to_string(), agent.clone()),
    ];
    if let Some(report) = &report {
        env.push((
            "TAW_SANDBOX_REPORT".to_string(),
            report.display().to_string(),
        ));
    }
    env.push(("TAW_SANDBOX_PROFILE".to_string(), profile.name.clone()));
    if let Some(pane) = &here.pane {
        env.push(("TAW_SANDBOX_PANE".to_string(), pane.clone()));
    }
    Ok(Prepared {
        profile,
        agent,
        command,
        env,
        dir: here.dir.clone(),
        report,
    })
}

impl Prepared {
    /// The profile's process: its file, its arguments, the project directory, and the variables,
    /// any of the five this process had taken out first, so a start never passes on a parent's.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.profile.path);
        command.args(&self.command).current_dir(&self.dir);
        for name in VARIABLES {
            command.env_remove(name);
        }
        command.envs(self.env.iter().map(|(name, value)| (name, value)));
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::plugin::tests_support::test_paths(dir.path());
        (dir, paths)
    }

    fn here(pane: Option<&str>) -> Here {
        Here {
            pane: pane.map(str::to_string),
            tmux: pane.is_some(),
            dir: PathBuf::from("/Users/you/work/api"),
        }
    }

    #[test]
    fn typed_alone_it_is_claude_through_taws_own_profile() {
        let (_dir, paths) = rig();

        let got = prepare(
            &Config::default(),
            &paths,
            Some("default"),
            &Request {
                agent: None,
                profile: None,
                args: &[],
            },
            &here(Some("%31")),
        )
        .unwrap();

        assert_eq!(got.agent, "claude");
        assert_eq!(got.command, ["claude"]);
        assert!(got.profile.builtin);
        assert_eq!(got.profile.name, "default");
    }

    #[test]
    fn in_a_pane_the_profile_is_given_the_five_variables_and_the_agents_command_line() {
        let (_dir, paths) = rig();
        let args = ["--resume".to_string(), "4b1f 2c".to_string()];

        let got = prepare(
            &Config::default(),
            &paths,
            Some("default"),
            &Request {
                agent: Some("cursor-agent"),
                profile: None,
                args: &args,
            },
            &here(Some("%31")),
        )
        .unwrap();

        let report = paths.state_dir.join("sandbox/panes/default/%31");
        assert_eq!(got.command, ["cursor-agent", "--resume", "4b1f 2c"]);
        assert_eq!(
            got.env,
            [
                (
                    "TAW_SANDBOX_DIR".to_string(),
                    "/Users/you/work/api".to_string()
                ),
                ("TAW_SANDBOX_AGENT".to_string(), "cursor".to_string()),
                (
                    "TAW_SANDBOX_REPORT".to_string(),
                    report.display().to_string()
                ),
                ("TAW_SANDBOX_PROFILE".to_string(), "default".to_string()),
                ("TAW_SANDBOX_PANE".to_string(), "%31".to_string()),
            ]
        );
        assert_eq!(got.report, Some(report));
    }

    #[test]
    fn outside_tmux_the_profile_has_no_report_folder_and_no_pane() {
        let (_dir, paths) = rig();

        let got = prepare(
            &Config::default(),
            &paths,
            None,
            &Request {
                agent: Some("codex"),
                profile: None,
                args: &[],
            },
            &here(None),
        )
        .unwrap();

        let names: Vec<&str> = got.env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "TAW_SANDBOX_DIR",
                "TAW_SANDBOX_AGENT",
                "TAW_SANDBOX_PROFILE"
            ]
        );
        assert_eq!(got.report, None);
    }

    #[test]
    fn an_agent_taw_does_not_know_and_a_profile_that_is_not_there_say_why() {
        let (_dir, paths) = rig();
        let ask = |agent: Option<&str>, profile: Option<&str>| {
            prepare(
                &Config::default(),
                &paths,
                None,
                &Request {
                    agent,
                    profile,
                    args: &[],
                },
                &here(None),
            )
        };

        assert_eq!(
            ask(Some("claud"), None).unwrap_err().to_string(),
            "taw knows no agent called claud (taw manifest list names them)"
        );
        // An empty name is said as one, never as a hole in the sentence.
        assert_eq!(
            ask(Some(""), None).unwrap_err().to_string(),
            "taw knows no agent with an empty name (taw manifest list names them)"
        );
        assert!(
            ask(None, Some("team"))
                .unwrap_err()
                .to_string()
                .starts_with("no profile called team (yours: default, in ")
        );
    }

    #[test]
    fn the_config_picks_the_profile_for_an_agent_when_none_is_named() {
        let (_dir, paths) = rig();
        let dir = profile::yours_dir(&paths).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("max"), "#!/bin/sh\n").unwrap();
        crate::test_exec::make_executable(dir.join("max"));
        let cfg: Config = toml::from_str("[sandbox.agents]\ncodex = \"max\"\n").unwrap();
        let ask = |agent: Option<&str>| {
            prepare(
                &cfg,
                &paths,
                None,
                &Request {
                    agent,
                    profile: None,
                    args: &[],
                },
                &here(None),
            )
            .unwrap()
        };

        let (codex, claude) = (ask(Some("codex")), ask(None));

        assert_eq!(
            (codex.profile.name.as_str(), claude.profile.name.as_str()),
            ("max", "default")
        );
    }

    #[test]
    fn the_profiles_process_is_its_file_its_arguments_the_project_and_only_these_variables() {
        let (_dir, paths) = rig();
        let got = prepare(
            &Config::default(),
            &paths,
            Some("default"),
            &Request {
                agent: None,
                profile: None,
                args: &[],
            },
            &here(Some("%31")),
        )
        .unwrap();

        let command = got.command();

        assert_eq!(
            command.get_program(),
            profile::builtin_path(&paths).as_os_str()
        );
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["claude"]);
        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("/Users/you/work/api"))
        );
        let envs = envs_of(&command);
        for name in VARIABLES {
            assert!(
                envs.iter().any(|(k, v)| k == name && v.is_some()),
                "{name} in {envs:?}"
            );
        }
    }

    /// Outside tmux the profile is given three of the five, and the two it is not given are taken
    /// out of what this process had: a start from inside a sandbox's shell, or from a terminal that
    /// a pane's shell started, never passes on the report folder and the pane of another.
    #[test]
    fn what_a_start_is_not_given_is_taken_out_of_what_it_had() {
        let (_dir, paths) = rig();
        let got = prepare(
            &Config::default(),
            &paths,
            None,
            &Request {
                agent: None,
                profile: None,
                args: &[],
            },
            &here(None),
        )
        .unwrap();

        let envs = envs_of(&got.command());

        for name in ["TAW_SANDBOX_REPORT", "TAW_SANDBOX_PANE"] {
            assert!(
                envs.iter().any(|(k, v)| k == name && v.is_none()),
                "{name} taken out: {envs:?}"
            );
        }
        for name in [
            "TAW_SANDBOX_DIR",
            "TAW_SANDBOX_AGENT",
            "TAW_SANDBOX_PROFILE",
        ] {
            assert!(
                envs.iter().any(|(k, v)| k == name && v.is_some()),
                "{name} given: {envs:?}"
            );
        }
    }

    /// `TMUX_PANE` is taken in tmux's form only, `%` and digits: anything else is refused before
    /// anything is made of it, a folder of yours written whole or a climb out of taw's state among
    /// them, and so is a server's name that is no folder's.
    #[test]
    fn a_pane_that_is_not_in_tmuxs_form_is_refused_before_anything_is_made_of_it() {
        let (_dir, paths) = rig();
        let ask = |server: &str, pane: &str| {
            prepare(
                &Config::default(),
                &paths,
                Some(server),
                &Request {
                    agent: None,
                    profile: None,
                    args: &[],
                },
                &here(Some(pane)),
            )
        };
        for pane in [
            "/Users/you/Documents",
            "../../../../../..",
            "%31/../..",
            "%31/x",
            "31",
            "%",
            "",
            "%3a",
            "%-1",
            "%31\n",
            "\u{ff05}31",
        ] {
            assert_eq!(
                ask("default", pane).unwrap_err(),
                StartError::NotAPane(pane.to_string()),
                "{pane:?}"
            );
        }
        for server in ["..", ".", "", "a/b", "/x"] {
            assert_eq!(
                ask(server, "%31").unwrap_err(),
                StartError::NoReportFolder {
                    server: server.to_string(),
                    pane: "%31".to_string()
                },
                "{server:?}"
            );
        }
        assert!(ask("taw-e2e-41-3", "%0").is_ok());
        assert!(!paths.state_dir.exists(), "nothing was made");
        assert_eq!(
            StartError::NotAPane("../../x\n".to_string()).to_string(),
            "TMUX_PANE is \"../../x\\n\", which is no tmux pane (a % and digits), so nothing was started"
        );
    }

    /// What a start not typed at its pane's prompt says, in one line: why, and what to do instead.
    /// Which starts those are is the real-pane tests' (tests/e2e_sandbox.rs).
    #[test]
    fn a_start_not_typed_at_the_prompt_says_why_and_what_to_do_instead() {
        assert_eq!(
            StartError::NotAtThePrompt {
                pane: "%3".to_string(),
                why: "its input is no terminal"
            }
            .to_string(),
            "not typed at the prompt of pane %3 (its input is no terminal), so nothing was started: type it at the prompt of the pane it should take over (agents and scripts start one with taw agent start --sandbox)"
        );
    }

    /// A state folder that is not a full path (an empty or relative TAW_STATE_DIR) would be found
    /// from wherever taw runs, which is the project: taw's own profile would be written and run from
    /// it, and the pane's report folder made in it. Refused, in a pane and outside one, before
    /// anything is made.
    #[test]
    fn a_state_folder_that_is_not_a_full_path_is_refused() {
        let (_dir, mut paths) = rig();
        for state in ["", "state", "./x/state"] {
            paths.state_dir = PathBuf::from(state);
            for pane in [Some("%31"), None] {
                let got = prepare(
                    &Config::default(),
                    &paths,
                    pane.map(|_| "default"),
                    &Request {
                        agent: None,
                        profile: None,
                        args: &[],
                    },
                    &here(pane),
                );
                assert_eq!(
                    got.unwrap_err(),
                    StartError::StateNotAFullPath,
                    "{state:?} {pane:?}"
                );
            }
        }
        assert!(!std::path::Path::new("state").exists());
        assert!(!std::path::Path::new("x").exists());
        assert_eq!(
            StartError::StateNotAFullPath.to_string(),
            "TAW_STATE_DIR is not a full path, which would put taw's own profile and the pane's report folder in this folder, so nothing was started"
        );
    }

    #[test]
    fn a_pane_id_is_a_percent_and_ascii_digits() {
        for good in ["%0", "%31", "%123456"] {
            assert!(is_pane_id(good), "{good}");
        }
        for bad in [
            "",
            "%",
            "31",
            "%3a",
            "% 3",
            "%31 ",
            "%\u{0663}",
            "%%3",
            "w0:p3",
        ] {
            assert!(!is_pane_id(bad), "{bad:?}");
        }
    }

    /// What `command` changes of the environment it inherits: each name, and its value, or none
    /// for a name it takes out.
    fn envs_of(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }
}
