//! The `taw` command tree: parse with clap, dispatch to a verb group, print one JSON
//! envelope, return an exit code.
//!
//! Verb names, groups and flags mirror herdr's (`third_party/herdr/reference/cli-help.txt`)
//! so a plugin or agent skill written for herdr keeps working; where a herdr flag has no
//! tmux meaning it is accepted and ignored, or reports `unsupported`.
//!
//! # Shape of a group module
//!
//! Each `cli/<group>.rs` exposes a clap `Cmd` enum and
//! `run(&Ctx, Cmd) -> (Sub, anyhow::Result<Value>)`. Returning the sub-name *alongside*
//! the result — rather than deriving it separately — means the envelope's `id` is known
//! on the failure path too, from one `match` per group.
//!
//! Record shapes come from [`crate::plugin::context`], the single source of truth for
//! herdr-shaped `pane`/`agent`/`workspace`/`tab` records.

pub mod agent;
pub mod app;
pub mod common;
pub mod envelope;
pub mod events;
pub mod hooks;
pub mod image;
pub mod integration;
pub(crate) mod launch;
pub mod layout;
pub mod machine;
pub mod manifest;
pub mod misc;
pub mod notification;
pub mod open;
pub mod pane;
pub mod plugin;
pub mod sandbox;
pub mod server;
pub mod session;
pub(crate) mod status_bar;
pub mod tab;
pub mod terminal;
#[cfg(test)]
pub(crate) mod test_support;
pub mod ui_open;
pub mod view;
pub mod watch;
pub mod workspace;
pub mod worktree;

use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::config::Config;
use crate::paths::Paths;
use crate::tmux::Tmux;

/// A subcommand's name within its group, as it appears in the envelope id
/// (`cli:<group>:<sub>`). Empty for a group that is a single verb — `cli:seen`,
/// `cli:ui`, `cli:skill`. (`taw status` is *not* one of those: it dispatches to
/// `show`/`refresh`, so its ids are `cli:status:show` and `cli:status:refresh`.)
pub type Sub = &'static str;

/// Everything a verb needs, built once in [`main`]. Internal to `cli`: library calls take
/// explicit parameters, never this struct, so nothing below `cli` depends on how the CLI
/// happens to assemble its world.
pub(crate) struct Ctx {
    pub tmux: Tmux,
    pub cfg: Config,
    pub paths: Paths,
    /// Absolute path to this binary — baked into hook scripts, plugin environments and
    /// `pipe-pane` commands, so they re-invoke *this* `taw` rather than whatever is on
    /// `$PATH` at the time.
    pub taw_bin: std::path::PathBuf,
}

/// What a verb produced, and therefore how it is written to stdout.
enum Output {
    /// The usual: fields to merge into `{"id":…,"result":{"type":…}}`.
    Envelope(Value),
    /// Written to stdout verbatim, with no envelope. Only `taw skill` and
    /// `taw config default` do this: both exist to be redirected into a file, and a JSON
    /// wrapper would make every consumer unwrap them first. Documented on the `Skill` and
    /// `Config` variants of [`Group`] so it is visible in `--help`.
    Raw(String),
    /// Nothing at all: what `taw open` prints when it works. It is for people at a shell,
    /// and `--json` is how a script asks for the envelope. `taw sandbox`'s start never gets this
    /// far when it works: it has become the profile.
    Nothing,
    /// The verb wrote what it had to say itself, and the process ends with this status: `taw machine
    /// feed`'s lines, which are not an envelope.
    Exit(i32),
    /// An envelope, and a process status of its own: `taw machine check --json` answers with its
    /// result and ends with 1 when a machine did not connect.
    EnvelopeExit(Value, i32),
}

#[derive(Parser, Debug)]
#[command(
    name = "taw",
    version,
    about = "herdr-style agent awareness for tmux",
    long_about = "Control and inspect coding agents running in tmux panes. Every command \
                  prints one JSON envelope on stdout:\n\
                  \n  {\"id\":\"cli:pane:list\",\"result\":{\"type\":\"pane_list\",\"panes\":[…]}}\
                  \n  {\"id\":\"cli:pane:get\",\"error\":{\"code\":\"not_found\",\"message\":\"…\"}}\
                  \n\nEight commands are exceptions. `taw skill` (markdown) and `taw config \
                  default` (TOML) print their payload raw, because they exist to be \
                  redirected into a file. `taw open` is for people at a shell: it prints \
                  nothing when it works (`--json` prints the envelope) and a line on stderr \
                  when it does not. `taw sandbox` runs an agent in this pane through a \
                  sandbox profile: it becomes the profile when it starts, and says why on \
                  stderr when it does not (its `new` and `list` print the envelope). `taw app` is a screen and prints none. `taw machine feed` \
                  prints JSON lines, for the app to read. `taw machine attach` becomes tmux on \
                  that machine. `taw machine check` prints a line for each machine (`--json` \
                  prints the envelope).\
                  \n\nRun `taw skill` for the agent-facing guide.",
    disable_help_subcommand = true
)]
struct Cli {
    /// Indent the JSON envelope instead of printing it on one line
    #[arg(long, global = true)]
    pretty: bool,
    /// Run the command on a machine from [machines] (taw --machine studio agent list)
    ///
    /// Goes before the command, with `--pretty` in either order. What follows is that machine's own
    /// command line, run there through ssh; its answer comes back as it was given.
    #[arg(long, global = false, value_name = "NAME")]
    machine: Option<String>,
    #[command(subcommand)]
    command: Group,
}

#[derive(Subcommand, Debug)]
enum Group {
    /// Control and inspect agent panes
    Agent {
        #[command(subcommand)]
        cmd: agent::Cmd,
    },
    /// Run an agent in a container in this pane, through a sandbox profile (prints nothing when it
    /// works; new and list print an envelope)
    Sandbox(sandbox::SandboxArgs),
    /// The app: every session of this Mac and of the machines in [machines] as the whole terminal
    /// (inside tmux the popup is the list, and this opens it)
    App {
        /// Open in a session or pane, not on the list: last, a pane of this machine as in %7, or a machine and one of
        /// its ids as in studio:%16
        #[arg(long, value_name = "TARGET", hide = true)]
        go: Option<String>,
        /// Run as the screen under the supervisor that `taw app` starts (not for typing)
        #[arg(long, hide = true)]
        child: bool,
        /// The id of the tmux session (as in $3) the doorway's popup was opened from: when the app ends, the terminal
        /// is put back in tmux on it (not for typing)
        #[arg(long, value_name = "SESSION", hide = true)]
        back: Option<String>,
    },
    /// The machines of [machines]: list, add and remove them, check that they answer (taw --machine NAME
    /// runs a command on one)
    Machine {
        #[command(subcommand)]
        cmd: machine::Cmd,
    },
    /// Paste the image on this machine's clipboard into an agent's pane, here or on a machine
    Image {
        #[command(subcommand)]
        cmd: image::Cmd,
    },
    /// Control terminal panes
    Pane {
        #[command(subcommand)]
        cmd: pane::Cmd,
    },
    /// Export and apply tab layouts
    Layout {
        #[command(subcommand)]
        cmd: layout::Cmd,
    },
    /// Manage workspaces (tmux sessions)
    Workspace {
        #[command(subcommand)]
        cmd: workspace::Cmd,
    },
    /// Manage tabs (tmux windows)
    Tab {
        #[command(subcommand)]
        cmd: tab::Cmd,
    },
    /// Manage Git worktree-backed workspaces
    Worktree {
        #[command(subcommand)]
        cmd: worktree::Cmd,
    },
    /// See and test the commands run on taw's events
    Hooks {
        #[command(subcommand)]
        cmd: hooks::Cmd,
    },
    /// Show notifications
    Notification {
        #[command(subcommand)]
        cmd: notification::Cmd,
    },
    /// Note that a tmux server is up, which the plugin does each time it loads (not for typing)
    #[command(hide = true)]
    Server {
        #[command(subcommand)]
        cmd: server::Cmd,
    },
    /// Save and restore the tmux layout, with agent resume
    Session {
        #[command(subcommand)]
        cmd: session::Cmd,
    },
    /// Manage built-in agent integrations
    Integration {
        #[command(subcommand)]
        cmd: integration::Cmd,
    },
    /// Install and run workflow plugins
    Plugin {
        #[command(subcommand)]
        cmd: plugin::Cmd,
    },
    /// Inspect and update agent detection manifests
    Manifest {
        #[command(subcommand)]
        cmd: manifest::Cmd,
    },
    /// Attach, detach and inspect pane watchers
    Watch {
        #[command(subcommand)]
        cmd: watch::Cmd,
    },
    /// Mark a pane as seen (`done` becomes `idle`)
    Seen {
        #[arg(value_name = "PANE")]
        pane: String,
    },
    /// Show the status rail, or refresh `@agent_wizard_status`
    Status {
        #[command(subcommand)]
        cmd: Option<misc::StatusCmd>,
    },
    /// Switch to, or create, the session for a directory or a zoxide match (tmux-session-wizard's t)
    Open {
        /// A directory (absolute, relative or starting with ~), or words for zoxide to
        /// match. With none, the popup opens on its new-session picker
        #[arg(value_name = "TERMS")]
        terms: Vec<String>,
        /// Print the JSON envelope `taw session open` prints, for scripts. This terminal is
        /// not attached to the session
        #[arg(long)]
        json: bool,
    },
    /// Open the agent popup
    Ui {
        /// The pane the popup opens from (`taw ui open` passes this on, or names its own pane;
        /// `taw ui` defaults to `$TMUX_PANE`)
        #[arg(long, value_name = "PANE", global = true)]
        from: Option<String>,
        /// The launcher swapped the status line for taw's bar; keep its counts fresh.
        /// `taw ui open` sets this when it put the bar up; a `taw ui` typed in a pane, or
        /// one the launcher opened without swapping (`status off`, a failed swap), must
        /// never write `@agent_wizard_bar` on any session
        // Hidden from `taw ui --help`: the launcher's contract with the popup it opens, not
        // a switch for anyone to type.
        #[arg(long, hide = true)]
        bar: bool,
        /// The client this popup was opened on (`taw ui open` fills this in): every switch
        /// the popup makes moves that client, however many share its session. Without it
        /// the client is picked from those attached
        // Hidden from `taw ui --help`, like `--bar`: the launcher's contract with the popup
        // it opens, not a switch for anyone to type.
        #[arg(long, hide = true, value_name = "CLIENT")]
        client: Option<String>,
        /// Start with the new-session picker open, and close the popup on Esc
        #[arg(long, global = true)]
        new_session: bool,
        #[command(subcommand)]
        cmd: Option<UiCmd>,
    },
    /// Manage local configuration (`config default` prints raw TOML, not an envelope)
    Config {
        #[command(subcommand)]
        cmd: misc::ConfigCmd,
    },
    /// Manage the outer terminal title
    Terminal {
        #[command(subcommand)]
        cmd: terminal::Cmd,
    },
    /// Close an open popup
    Popup {
        #[command(subcommand)]
        cmd: PopupCmd,
    },
    /// Print the agent skill, SKILL.md (raw markdown, not a JSON envelope)
    Skill,
}

/// `taw popup close` — the only popup verb herdr has.
#[derive(Subcommand, Debug)]
pub enum PopupCmd {
    /// Close the popup on every attached client
    Close,
}

/// `taw ui open` — the only subcommand of `ui`: computes popup geometry that leaves
/// the status line visible and runs `display-popup`. Bare `taw ui` (no subcommand)
/// still draws the ratatui popup directly, via `misc::ui`.
///
/// `taw ui --from <pane>` is how the two halves meet. `display-popup -E` does not give
/// its command the invoking pane's environment, so the popup cannot read `$TMUX_PANE`
/// for itself; `ui open` runs from the key binding's `run-shell`, where tmux still
/// resolves the current client's active pane, and passes it down. The flag lives on the
/// group rather than on `open`, because it is bare `taw ui` — the popup — that needs it.
/// `--client` is the same hand-over for the client: `ui open` names the client it opens the
/// popup on, and the popup moves that one whenever it switches to something.
#[derive(Subcommand, Debug)]
pub enum UiCmd {
    /// Size and open the popup, leaving the status line visible
    Open,
}

/// The `result.type` for each verb's envelope. `result.type` is what a plugin switches
/// on, so the names are listed together here rather than scattered through the group
/// modules.
///
/// Looked up as `(group, sub)`; a `sub` of `""` matches any subcommand of that group,
/// which is how `status` and `status refresh` share one type.
const RESULT_TYPES: &[(&str, &str, &str)] = &[
    // agent
    ("agent", "list", "agent_list"),
    ("agent", "get", "agent"),
    ("agent", "read", "pane_output"),
    ("agent", "rename", "agent"),
    ("agent", "wait", "agent"),
    ("agent", "start", "agent"),
    ("agent", "explain", "agent_explain"),
    // pane
    ("pane", "list", "pane_list"),
    ("pane", "current", "pane"),
    ("pane", "get", "pane"),
    ("pane", "layout", "pane_layout"),
    ("pane", "process-info", "pane_process_info"),
    ("pane", "neighbor", "pane"),
    ("pane", "edges", "pane_edges"),
    ("pane", "focus", "pane"),
    ("pane", "resize", "pane"),
    ("pane", "zoom", "pane_zoom"),
    ("pane", "read", "pane_output"),
    ("pane", "rename", "pane"),
    ("pane", "split", "pane_split"),
    ("pane", "move", "pane_moved"),
    ("pane", "wait-output", "pane_output_match"),
    // layout
    ("layout", "export", "layout_export"),
    ("layout", "apply", "layout_apply"),
    ("layout", "set-split-ratio", "layout_split_ratio_set"),
    // workspace
    ("workspace", "list", "workspace_list"),
    ("workspace", "create", "workspace_created"),
    ("workspace", "get", "workspace"),
    ("workspace", "rename", "workspace"),
    ("workspace", "id", "workspace_id"),
    // tab
    ("tab", "list", "tab_list"),
    ("tab", "create", "tab_created"),
    ("tab", "get", "tab"),
    ("tab", "rename", "tab"),
    // worktree
    ("worktree", "list", "worktree_list"),
    ("worktree", "create", "worktree_created"),
    ("worktree", "open", "worktree_opened"),
    ("worktree", "remove", "worktree_removed"),
    // server
    ("server", "started", "server_started"),
    // session
    ("session", "list", "session_list"),
    ("session", "save", "session_saved"),
    ("session", "restore", "session_restored"),
    ("session", "autosave", "session_autosave"),
    ("session", "snapshot", "session_snapshot"),
    ("session", "open", "session_open"),
    ("open", "", "session_open"),
    // sandbox
    ("sandbox", "new", "sandbox_profile"),
    ("sandbox", "list", "sandbox_list"),
    // image
    ("image", "paste", "image_pasted"),
    ("image", "receive", "image_received"),
    // machine
    ("machine", "list", "machine_list"),
    ("machine", "check", "machine_check"),
    ("machine", "add", "machine_added"),
    ("machine", "remove", "machine_removed"),
    // integration
    ("integration", "install", "integration_installed"),
    ("integration", "uninstall", "integration_uninstalled"),
    ("integration", "status", "integration_status"),
    // plugin
    ("plugin", "install", "plugin_installed"),
    ("plugin", "update", "plugin_update"),
    ("plugin", "link", "plugin_linked"),
    ("plugin", "list", "plugin_list"),
    ("plugin", "config-dir", "plugin_config_dir"),
    ("plugin", "log", "plugin_log"),
    ("plugin", "action-list", "plugin_action_list"),
    ("plugin", "action-invoke", "plugin_action_result"),
    ("plugin", "pane-open", "plugin_pane_opened"),
    ("plugin", "startup", "plugin_startup"),
    ("plugin", "event", "plugin_event"),
    // manifest
    ("manifest", "list", "manifest_list"),
    ("manifest", "update", "manifest_update"),
    ("manifest", "show", "manifest"),
    ("manifest", "reload", "agent_manifest_reload"),
    // terminal
    ("terminal", "title-set", "client_window_title"),
    ("terminal", "title-clear", "client_window_title"),
    // agent view
    ("agent", "view-set", "agent_view"),
    ("agent", "view-clear", "agent_view"),
    // hooks
    ("hooks", "list", "hooks_list"),
    ("hooks", "test", "hooks_test"),
    ("hooks", "log", "hooks_log"),
    // watch / status / config
    ("watch", "status", "watch_status"),
    ("status", "", "status"),
    ("ui", "open", "ui_open"),
    ("config", "check", "config_check"),
    ("config", "reload", "config_reload"),
    ("config", "path", "config_path"),
    // No rows for `skill` or `config default`: both print raw and never build an
    // envelope, so a `result.type` for them would only be dead weight.
];

/// The `result.type` for a verb: its own entry, else the group-wide `""` entry, else
/// `"ok"` — the shape for every verb whose result is simply "it worked".
fn result_type(group: &str, sub: Sub) -> &'static str {
    RESULT_TYPES
        .iter()
        .find(|(g, s, _)| *g == group && *s == sub)
        .or_else(|| {
            RESULT_TYPES
                .iter()
                .find(|(g, s, _)| *g == group && s.is_empty())
        })
        .map(|(_, _, t)| *t)
        .unwrap_or("ok")
}

/// `cli:<group>:<sub>`, or `cli:<group>` for a single-verb group.
fn envelope_id(group: &str, sub: Sub) -> String {
    if sub.is_empty() {
        format!("cli:{group}")
    } else {
        format!("cli:{group}:{sub}")
    }
}

/// Sends `log` output to stderr at the level `$TAW_LOG` names (`error`, `warn`, `info`,
/// `debug`, `trace`; unset or unrecognised means `warn`).
///
/// stderr, never stdout: stdout carries exactly one JSON envelope, and a log line mixed
/// into it would break every caller that parses it. It can be called again: a second logger is
/// refused and left alone.
pub(crate) fn init_logging() {
    let level = match std::env::var("TAW_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => log::LevelFilter::Error,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Warn,
    };
    let config = simplelog::ConfigBuilder::new().build();
    // A terminal logger when stderr is a tty (colour, alignment), plain otherwise.
    if simplelog::TermLogger::init(
        level,
        config.clone(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )
    .is_err()
    {
        let _ = simplelog::SimpleLogger::init(level, config);
    }
}

/// Builds the context every verb runs against.
///
/// A `config.toml` that fails to parse warns on stderr (when `warn` says to) and falls back to
/// defaults rather than failing the command: a broken config must not make `taw` unusable, least
/// of all `taw config check`, which exists to explain what is broken.
fn build_ctx(warn: bool) -> Ctx {
    let paths = Paths::from_env();
    let cfg = match crate::config::load(&paths) {
        Ok(cfg) => cfg,
        Err(_) if !warn => Config::default(),
        Err(err) => {
            eprintln!(
                "taw: warning: using default settings, config {} could not be loaded: {err:#}\n\
                 taw: run `taw config check` for details",
                paths.config_file.display()
            );
            Config::default()
        }
    };
    let taw_bin = std::env::current_exe().unwrap_or_else(|err| {
        log::warn!("could not resolve this binary's path ({err}); falling back to 'taw'");
        std::path::PathBuf::from("taw")
    });
    Ctx {
        tmux: Tmux::from_env(),
        cfg,
        paths,
        taw_bin,
    }
}

/// Exit code for a reported error. herdr's contract: runtime errors are JSON with status
/// 1; clap's own usage errors exit 2 (clap does that itself).
const EXIT_ERROR: i32 = 1;

/// Exit code for a verb that refuses what it was asked ([`envelope::UsageRefusal`]): 2, like a
/// usage error.
const EXIT_USAGE: i32 = 2;

/// Parses `argv` as taw's command line: clap's parse, with `taw sandbox`'s own tip when a placement
/// flag is refused ([`sandbox::placement_tip`]).
fn parse_from<I, T>(argv: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(argv).map_err(sandbox::placement_tip)
}

/// Parses argv, runs one verb, prints its envelope, and returns the process exit code.
pub fn main() -> i32 {
    // `--machine` sends the rest of the line to another machine, whose own parser reads it: it never
    // reaches this one's (a word that is not this machine's verb is not an error here).
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let words: Option<Vec<String>> = argv
        .iter()
        .skip(1)
        .map(|word| word.to_str().map(str::to_string))
        .collect();
    if let Some(lead) = words.as_deref().and_then(crate::machine::forward::leading) {
        return crate::machine::forward::run(&lead.machine, lead.pretty, &lead.rest);
    }
    // A word that is not UTF-8 cannot be read above, so a line for a machine that has one is refused
    // here, in its own words: clap would say the machine has no name.
    if words.is_none() {
        let lossy: Vec<String> = argv
            .iter()
            .skip(1)
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        if crate::machine::forward::names_machine(&lossy) {
            eprintln!("taw: --machine runs words of UTF-8 only");
            return EXIT_USAGE;
        }
    }

    let cli = parse_from(argv).unwrap_or_else(|err| err.exit());
    // Only a `--machine` with an empty name (`--machine=` or `--machine ''`) gets here with the option
    // set: the other lines that name one were sent or refused above, and clap refuses the rest.
    if cli.machine.is_some() {
        eprintln!("taw: --machine needs the name of a machine (taw --machine studio agent list)");
        return EXIT_USAGE;
    }

    // `watch run` is the one verb that must not log to stderr: it is spawned by
    // `pipe-pane`, so its stderr is wherever tmux left it and a log line could land in a
    // user's terminal. It gets a per-pane *file* logger instead.
    let watched_pane = match &cli.command {
        Group::Watch {
            cmd: watch::Cmd::Run { pane },
        } => Some(pane.clone()),
        _ => None,
    };

    // `taw sandbox`'s start with no `--profile` says itself, in its one line, that the config did
    // not load, and starts nothing. `taw config check` says it itself too, in its answer: a warning
    // that tells you to run it would be telling you to run the command just run.
    let says_it_itself = matches!(
        &cli.command,
        Group::Sandbox(args) if args.cmd.is_none() && args.profile.is_none()
    ) || matches!(
        &cli.command,
        Group::Config {
            cmd: misc::ConfigCmd::Check
        }
    );
    let ctx = build_ctx(!says_it_itself);

    match &watched_pane {
        Some(pane) => {
            if let Err(err) = crate::watch::init_logging(&ctx.paths, pane) {
                // The watcher is still useful without a log file, so carry on rather than
                // taking down this pane's watcher over it.
                eprintln!("taw: warning: could not open the watcher log for {pane}: {err:#}");
            }
        }
        None => init_logging(),
    }

    // `taw open` without `--json`, and `taw sandbox`'s start, report a failure the way a person
    // reads it, on stderr.
    let plain: Option<fn(&anyhow::Error) -> i32> = match &cli.command {
        Group::Open { json: false, .. } => Some(open::report),
        Group::Sandbox(args) if args.cmd.is_none() => Some(sandbox::report),
        Group::Machine {
            cmd:
                machine::Cmd::Feed { .. }
                | machine::Cmd::Attach { .. }
                | machine::Cmd::Check { json: false, .. },
        } => Some(machine::report),
        Group::App { .. } => Some(app::report),
        _ => None,
    };

    // The client a paste's screen was drawn on, which hears of a failure the screen has gone too soon to toast.
    let shown_on = match &cli.command {
        Group::Image {
            cmd: image::Cmd::Paste { client, .. },
        } => client.clone(),
        _ => None,
    };

    let (group, sub, result) = dispatch(&ctx, cli.command);

    // A refusal is told as a usage error is: on stderr, with no envelope.
    if let Err(err) = &result
        && let Some(refusal) = err.downcast_ref::<envelope::UsageRefusal>()
    {
        eprintln!("{refusal}");
        return EXIT_USAGE;
    }

    let id = envelope_id(group, sub);
    let value = match result {
        // Raw payloads bypass the envelope entirely — and `--pretty`, which has nothing
        // to format. One trailing newline, so the output is a well-formed text file
        // whether or not the payload already ended in one.
        Ok(Output::Raw(text)) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            return 0;
        }
        Ok(Output::Nothing) => return 0,
        Ok(Output::Exit(code)) => return code,
        Ok(Output::EnvelopeExit(fields, code)) => {
            let value = envelope::success(&id, result_type(group, sub), fields);
            let _ = say(&envelope::render(&value, cli.pretty));
            return code;
        }
        Ok(Output::Envelope(fields)) => envelope::success(&id, result_type(group, sub), fields),
        Err(err) if let Some(report) = plain => return report(&err),
        Err(err) => {
            let code = envelope::code_for(&err);
            // The full chain goes to the log; the envelope carries the top-level message,
            // which is the one written for a human or a plugin to read.
            log::debug!("{id} failed ({code}): {err:#}");
            envelope::failure(&id, code, &format!("{err}"))
        }
    };
    let failed = value.get("error").is_some();
    let heard = say(&envelope::render(&value, cli.pretty));
    if !heard
        && let Some(client) = &shown_on
        && let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(|message| message.as_str())
    {
        crate::image::show_on_client(&ctx.tmux, client, message);
    }
    if failed { EXIT_ERROR } else { 0 }
}

/// Writes the answer as one line on standard output. A reader that has gone (`taw image paste` run by a popup that
/// has closed) is not an error: `println!` would panic on the broken pipe, and the verb's work is done by then.
/// Whether the line was written.
fn say(line: &str) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").is_ok() && out.flush().is_ok()
}

/// Routes one parsed command, returning `(group, sub, output)`.
///
/// The two raw-output verbs, `taw open` and `taw sandbox`'s start are picked off first; everything
/// else goes through [`dispatch_envelope`], whose result is wrapped as [`Output::Envelope`].
fn dispatch(ctx: &Ctx, command: Group) -> (&'static str, Sub, anyhow::Result<Output>) {
    match &command {
        Group::Skill => return ("skill", "", misc::skill().map(Output::Raw)),
        Group::Config {
            cmd: misc::ConfigCmd::Default,
        } => {
            return ("config", "default", misc::config_default().map(Output::Raw));
        }
        Group::Config {
            cmd: misc::ConfigCmd::Check,
        } => {
            let output = misc::check_with_status(ctx)
                .map(|(fields, code)| Output::EnvelopeExit(fields, code));
            return ("config", "check", output);
        }
        Group::Open { terms, json } => {
            let output = open::run(ctx, terms, *json).map(|printed| match printed {
                Some(fields) => Output::Envelope(fields),
                None => Output::Nothing,
            });
            return ("open", "", output);
        }
        Group::Sandbox(args) if args.cmd.is_none() => {
            return (
                "sandbox",
                "",
                sandbox::start(ctx, args).map(|()| Output::Nothing),
            );
        }
        Group::App { go, child, back } => {
            return (
                "app",
                "",
                app::run(ctx, go.as_deref(), *child, back.as_deref()),
            );
        }
        Group::Machine { cmd } => {
            if let Some((sub, output)) = machine::direct(ctx, cmd) {
                return ("machine", sub, output);
            }
        }
        Group::Plugin {
            cmd: plugin::Cmd::Update { plugins, yes },
        } => {
            let output = plugin::update(ctx, plugins, *yes)
                .map(|(fields, code)| Output::EnvelopeExit(fields, code));
            return ("plugin", "update", output);
        }
        _ => {}
    }
    let (group, sub, result) = dispatch_envelope(ctx, command);
    (group, sub, result.map(Output::Envelope))
}

/// Routes one parsed command to its group, returning `(group, sub, result)`.
fn dispatch_envelope(ctx: &Ctx, command: Group) -> (&'static str, Sub, anyhow::Result<Value>) {
    match command {
        Group::Agent { cmd } => {
            let (sub, result) = agent::run(ctx, cmd);
            ("agent", sub, result)
        }
        Group::Sandbox(args) => {
            let (sub, result) = sandbox::run(ctx, args);
            ("sandbox", sub, result)
        }
        Group::Machine { cmd } => {
            let (sub, result) = machine::run(ctx, cmd);
            ("machine", sub, result)
        }
        Group::Image { cmd } => {
            let (sub, result) = image::run(ctx, cmd);
            ("image", sub, result)
        }
        Group::Pane { cmd } => {
            let (sub, result) = pane::run(ctx, cmd);
            ("pane", sub, result)
        }
        Group::Layout { cmd } => {
            let (sub, result) = layout::run(ctx, cmd);
            ("layout", sub, result)
        }
        Group::Workspace { cmd } => {
            let (sub, result) = workspace::run(ctx, cmd);
            ("workspace", sub, result)
        }
        Group::Tab { cmd } => {
            let (sub, result) = tab::run(ctx, cmd);
            ("tab", sub, result)
        }
        Group::Worktree { cmd } => {
            let (sub, result) = worktree::run(ctx, cmd);
            ("worktree", sub, result)
        }
        Group::Hooks { cmd } => {
            let (sub, result) = hooks::run(ctx, cmd);
            ("hooks", sub, result)
        }
        Group::Notification { cmd } => {
            let (sub, result) = notification::run(ctx, cmd);
            ("notification", sub, result)
        }
        Group::Server { cmd } => {
            let (sub, result) = server::run(ctx, cmd);
            ("server", sub, result)
        }
        Group::Session { cmd } => {
            let (sub, result) = session::run(ctx, cmd);
            ("session", sub, result)
        }
        Group::Integration { cmd } => {
            let (sub, result) = integration::run(ctx, cmd);
            ("integration", sub, result)
        }
        Group::Plugin { cmd } => {
            let (sub, result) = plugin::run(ctx, cmd);
            ("plugin", sub, result)
        }
        Group::Manifest { cmd } => {
            let (sub, result) = manifest::run(ctx, cmd);
            ("manifest", sub, result)
        }
        Group::Watch { cmd } => {
            let (sub, result) = watch::run(ctx, cmd);
            ("watch", sub, result)
        }
        Group::Seen { pane } => ("seen", "", watch::seen(ctx, &pane)),
        Group::Status { cmd } => {
            let (sub, result) = misc::status(ctx, cmd);
            ("status", sub, result)
        }
        Group::Terminal { cmd } => {
            let (sub, result) = terminal::run(ctx, cmd);
            ("terminal", sub, result)
        }
        Group::Popup { cmd } => match cmd {
            PopupCmd::Close => ("popup", "close", terminal::popup_close(ctx)),
        },
        Group::Ui {
            cmd,
            from,
            client,
            bar,
            new_session,
        } => match cmd {
            Some(UiCmd::Open) => (
                "ui",
                "open",
                ui_open::open(ctx, from.as_deref(), new_session),
            ),
            None => (
                "ui",
                "",
                misc::ui(ctx, from.as_deref(), client.as_deref(), bar, new_session),
            ),
        },
        Group::Config { cmd } => {
            let (sub, result) = misc::config(ctx, cmd);
            ("config", sub, result)
        }
        // Handled by `dispatch` before this, since it prints raw (and `open` prints
        // nothing, or its own line of error). (`config default` is too, but its group arm
        // above still has to cover the variant, so `misc::config` keeps an unreachable
        // branch for it — noted there.)
        Group::Skill | Group::Open { .. } | Group::App { .. } => {
            unreachable!("raw-output verbs are dispatched before dispatch_envelope")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_clap_tree_is_internally_consistent() {
        // Catches duplicate flags, bad value names and conflicting arg ids at test time
        // rather than at a user's first invocation.
        Cli::command().debug_assert();
    }

    /// The usage words of `--go` and its hidden help call the form for a pane of this computer the same
    /// thing: "this machine" (a constant and an attribute cannot call `this_machine_word()`).
    #[test]
    fn the_go_help_and_the_go_usage_words_say_this_machine() {
        let form = "a pane of this machine as in %7";
        let command = Cli::command();
        let app = command.find_subcommand("app").expect("the app command");
        let go = app
            .get_arguments()
            .find(|a| a.get_id() == "go")
            .expect("the --go flag");
        let help = go.get_help().expect("its help").to_string();
        assert!(help.contains(form), "the help of --go says: {help}");
        assert!(
            crate::machine::words::GO_USAGE.contains(form),
            "the usage words say: {}",
            crate::machine::words::GO_USAGE
        );
        assert!(!help.contains("this Mac"), "the help of --go says: {help}");
    }

    fn group_names() -> Vec<String> {
        Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .collect()
    }

    /// What is kept out of anything a person reads: an em-dash, an en-dash, an arrow or a
    /// curly quote, and the look-alikes for them. Spelled as escapes, so this file passes its
    /// own scan.
    const MARKS: [(char, &str); 11] = [
        ('\u{2014}', "an em-dash"),
        ('\u{2013}', "an en-dash"),
        ('\u{2192}', "an arrow"),
        ('\u{2018}', "a curly quote"),
        ('\u{2019}', "a curly quote"),
        ('\u{201c}', "a curly quote"),
        ('\u{201d}', "a curly quote"),
        ('\u{2015}', "a horizontal bar"),
        ('\u{2212}', "a minus sign"),
        ('\u{21d2}', "a double arrow"),
        ('\u{279c}', "an arrow"),
    ];

    /// Every help string in the clap tree, with the command path it belongs to.
    fn help_texts(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
        let path = format!("{path} {}", cmd.get_name());
        let about = cmd.get_about().into_iter().chain(cmd.get_long_about());
        for text in about {
            out.push((path.clone(), text.to_string()));
        }
        for arg in cmd.get_arguments() {
            for text in arg.get_help().into_iter().chain(arg.get_long_help()) {
                out.push((format!("{path} --{}", arg.get_id()), text.to_string()));
            }
        }
        for sub in cmd.get_subcommands() {
            help_texts(sub, &path, out);
        }
    }

    /// A bracketed code such as "(Q4)" or "(P18)".
    fn has_bracketed_code(text: &str) -> bool {
        text.match_indices('(').any(|(i, _)| {
            let mut rest = text[i + 1..].chars();
            matches!(rest.next(), Some('P' | 'Q'))
                && rest.next().is_some_and(|c| c.is_ascii_digit())
        })
    }

    /// Markdown emphasis, `*word*`, which a terminal prints as literal asterisks.
    fn has_markdown_emphasis(text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        (0..chars.len()).any(|i| {
            chars[i] == '*'
                && chars.get(i + 1).is_some_and(|c| c.is_alphanumeric())
                && (i + 2..chars.len()).any(|j| chars[j] == '*' && chars[j - 1].is_alphanumeric())
        })
    }

    /// `taw session autosave --help` is what a person reads: no bracketed code such as "(Q4)",
    /// and no "*closing*", which a terminal prints as literal asterisks.
    #[test]
    fn session_autosave_help_reads_as_plain_prose() {
        let cli = Cli::command();
        let autosave = cli
            .find_subcommand("session")
            .and_then(|session| session.find_subcommand("autosave"))
            .expect("taw session autosave is registered");
        let mut texts = Vec::new();
        help_texts(autosave, "taw session", &mut texts);
        assert!(
            texts.iter().any(|(path, _)| path.ends_with("--on_close")),
            "the walk must reach --on-close: {texts:?}"
        );
        for (path, text) in &texts {
            assert!(
                !has_bracketed_code(text),
                "{path}: bracketed code in help text: {text}"
            );
            assert!(
                !has_markdown_emphasis(text),
                "{path}: markdown emphasis in help text: {text}"
            );
            assert!(!text.contains('\u{2014}'), "{path}: em-dash: {text}");
        }
    }

    /// `taw hooks --help` is where a person learns what a hook is: plain prose, with none of
    /// the marks the wording rule keeps out of what a user reads. The
    /// verb taw starts for each background hook, `exec`, is hidden from all of it.
    #[test]
    fn hooks_help_reads_as_plain_prose_and_hides_the_verb_taw_starts_for_itself() {
        let cli = Cli::command();
        let hooks = cli
            .find_subcommand("hooks")
            .expect("taw hooks is registered");
        let mut texts = Vec::new();
        help_texts(hooks, "taw", &mut texts);
        assert!(
            texts.iter().any(|(path, _)| path.ends_with("--lines")),
            "the walk must reach hooks log --lines: {texts:?}"
        );
        for (path, text) in &texts {
            assert!(
                !has_bracketed_code(text),
                "{path}: bracketed code in help text: {text}"
            );
            assert!(
                !has_markdown_emphasis(text),
                "{path}: markdown emphasis in help text: {text}"
            );
            for banned in [
                '\u{2014}', '\u{2013}', '\u{2192}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
            ] {
                assert!(
                    !text.contains(banned),
                    "{path}: {banned:?} in help text: {text}"
                );
            }
        }
        let visible: Vec<&str> = hooks
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| sub.get_name())
            .collect();
        assert_eq!(visible, ["list", "test", "log"]);
        assert!(hooks.get_subcommands().any(|sub| sub.get_name() == "exec"));
    }

    #[test]
    fn every_herdr_group_is_registered() {
        let groups = group_names();
        for expected in [
            "layout",
            "terminal",
            "popup",
            "agent",
            "pane",
            "workspace",
            "tab",
            "worktree",
            "notification",
            "session",
            "integration",
            "plugin",
            "manifest",
            "watch",
            "seen",
            "status",
            "ui",
            "config",
            "skill",
        ] {
            assert!(
                groups.contains(&expected.to_string()),
                "group '{expected}' is missing; have {groups:?}"
            );
        }
    }

    /// The verb list, group by group. A missing entry here
    /// is a verb a herdr plugin or agent skill would call and get a clap error for.
    const SPEC_VERBS: &[(&str, &[&str])] = &[
        (
            "agent",
            &[
                "list",
                "get",
                "read",
                "send-keys",
                "prompt",
                "wait",
                "attach",
                "rename",
                "focus",
                "start",
                "explain",
                "view",
            ],
        ),
        (
            "pane",
            &[
                "list",
                "current",
                "get",
                "layout",
                "process-info",
                "neighbor",
                "edges",
                "focus",
                "resize",
                "zoom",
                "read",
                "rename",
                "input",
                "split",
                "swap",
                "move",
                "close",
                "clear",
                "send-text",
                "send-keys",
                "wait-output",
                "run",
                "report-agent",
                "report-agent-session",
                "release-agent",
                "report-metadata",
            ],
        ),
        (
            "workspace",
            &[
                "list",
                "create",
                "get",
                "focus",
                "rename",
                "close",
                "report-metadata",
                "id",
                "template",
            ],
        ),
        (
            "tab",
            &["list", "create", "get", "focus", "rename", "close"],
        ),
        ("worktree", &["list", "create", "open", "remove"]),
        ("notification", &["show", "open-target"]),
        (
            "session",
            &[
                "list", "save", "restore", "delete", "autosave", "snapshot", "open",
            ],
        ),
        ("integration", &["install", "uninstall", "status"]),
        (
            "plugin",
            &[
                "install",
                "uninstall",
                "enable",
                "disable",
                "link",
                "unlink",
                "list",
                "config-dir",
                "log",
                "action",
                "pane",
                "startup",
            ],
        ),
        ("manifest", &["list", "update", "show", "reload"]),
        (
            "watch",
            &["attach", "attach-all", "detach", "run", "status"],
        ),
        (
            "config",
            &["check", "path", "default", "reload", "reset-keys"],
        ),
        ("terminal", &["title"]),
        ("popup", &["close"]),
        ("ui", &["open"]),
        ("layout", &["export", "apply", "set-split-ratio"]),
    ];

    #[test]
    fn every_verb_in_the_spec_is_reachable() {
        let cmd = Cli::command();
        for (group, verbs) in SPEC_VERBS {
            let sub = cmd
                .get_subcommands()
                .find(|s| s.get_name() == *group)
                .unwrap_or_else(|| panic!("group '{group}' is not registered"));
            let registered: Vec<String> = sub
                .get_subcommands()
                .map(|s| s.get_name().to_string())
                .collect();
            for verb in *verbs {
                assert!(
                    registered.contains(&verb.to_string()),
                    "`taw {group} {verb}` is in the verb list but not registered; have {registered:?}"
                );
            }
        }
    }

    #[test]
    fn plugin_action_and_pane_have_their_nested_verbs() {
        let cmd = Cli::command();
        let plugin = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "plugin")
            .unwrap();
        for (nested, verbs) in [
            ("action", vec!["list", "invoke"]),
            ("pane", vec!["open", "focus", "close"]),
        ] {
            let sub = plugin
                .get_subcommands()
                .find(|s| s.get_name() == nested)
                .unwrap_or_else(|| panic!("`taw plugin {nested}` is missing"));
            let registered: Vec<String> = sub
                .get_subcommands()
                .map(|s| s.get_name().to_string())
                .collect();
            for verb in verbs {
                assert!(
                    registered.contains(&verb.to_string()),
                    "`taw plugin {nested} {verb}` is missing; have {registered:?}"
                );
            }
        }
    }

    #[test]
    fn envelope_ids_follow_the_spec_shape() {
        assert_eq!(envelope_id("pane", "list"), "cli:pane:list");
        assert_eq!(envelope_id("agent", "send-keys"), "cli:agent:send-keys");
        assert_eq!(envelope_id("skill", ""), "cli:skill");
        assert_eq!(envelope_id("seen", ""), "cli:seen");
        // `status` is a group with subcommands, not a single verb: `cli:status` is not
        // an id this CLI ever emits.
        assert_eq!(envelope_id("status", "show"), "cli:status:show");
        assert_eq!(envelope_id("status", "refresh"), "cli:status:refresh");
        // `ui` is a single-verb group with one optional subcommand: bare `ui` keeps
        // `cli:ui`, and `ui open` is `cli:ui:open`.
        assert_eq!(envelope_id("ui", ""), "cli:ui");
        assert_eq!(envelope_id("ui", "open"), "cli:ui:open");
    }

    #[test]
    fn result_types_fall_back_to_the_group_default_then_to_ok() {
        assert_eq!(result_type("pane", "list"), "pane_list");
        assert_eq!(result_type("agent", "explain"), "agent_explain");
        // Group-wide entry: both `status` and `status refresh` report type "status".
        assert_eq!(result_type("status", "show"), "status");
        assert_eq!(result_type("status", "refresh"), "status");
        assert_eq!(result_type("ui", "open"), "ui_open");
        assert_eq!(result_type("ui", ""), "ok");
        // No entry at all: "it worked".
        assert_eq!(result_type("pane", "close"), "ok");
        assert_eq!(result_type("pane", "clear"), "ok");
        assert_eq!(result_type("agent", "focus"), "ok");
        assert_eq!(result_type("nonexistent", "verb"), "ok");
    }

    #[test]
    fn every_result_type_entry_names_a_registered_group() {
        let groups = group_names();
        for (group, _, _) in RESULT_TYPES {
            assert!(
                groups.contains(&group.to_string()),
                "RESULT_TYPES names group '{group}', which is not registered"
            );
        }
    }

    #[test]
    fn parses_a_representative_command_from_each_group() {
        // Each of these would be a clap error at runtime if the tree were wrong.
        for argv in [
            vec!["taw", "pane", "list"],
            vec!["taw", "pane", "split", "w1:p1", "--direction", "right"],
            vec!["taw", "pane", "wait-output", "w1:p1", "--pattern", "x"],
            vec!["taw", "pane", "wait-output", "w1:p1", "--match", "x"],
            vec![
                "taw",
                "pane",
                "report-metadata",
                "%1",
                "--source",
                "s",
                "--title",
                "t",
            ],
            vec!["taw", "agent", "wait", "rev", "--state", "blocked"],
            vec!["taw", "agent", "wait", "rev", "--until", "blocked"],
            vec![
                "taw", "agent", "start", "rev", "--kind", "claude", "--pane", "w1:p1",
            ],
            vec!["taw", "workspace", "create", "--label", "x", "--no-focus"],
            vec!["taw", "tab", "create", "--workspace", "w1"],
            vec!["taw", "worktree", "create", "--branch", "x"],
            vec!["taw", "notification", "show", "hi", "--body", "there"],
            vec!["taw", "session", "restore", "--dry-run"],
            vec!["taw", "session", "open", "/tmp/x"],
            vec!["taw", "integration", "status"],
            vec!["taw", "plugin", "action", "invoke", "p.a"],
            vec!["taw", "manifest", "update", "--if-stale"],
            vec!["taw", "watch", "attach", "%1"],
            vec!["taw", "seen", "%1"],
            vec!["taw", "status", "refresh"],
            vec!["taw", "hooks", "list"],
            vec!["taw", "hooks", "test", "agent.done"],
            vec!["taw", "hooks", "log", "--lines", "5"],
            vec!["taw", "config", "check"],
            vec!["taw", "config", "reload"],
            vec!["taw", "manifest", "reload"],
            vec!["taw", "layout", "export"],
            vec!["taw", "layout", "apply", "w1:t1", "{\"type\":\"pane\"}"],
            vec!["taw", "layout", "set-split-ratio", "w1:p1", "0.5"],
            vec!["taw", "agent", "view", "set", "--source", "s"],
            vec![
                "taw", "agent", "view", "set", "--source", "s", "--label", "l", "--filter", "{}",
                "--sort", "[]",
            ],
            vec!["taw", "agent", "view", "set", "--params", "{}"],
            vec!["taw", "agent", "view", "clear"],
            vec!["taw", "ui"],
            vec!["taw", "ui", "open"],
            vec!["taw", "ui", "--new-session"],
            vec!["taw", "ui", "open", "--new-session"],
            vec!["taw", "open"],
            vec!["taw", "open", "--json", "~/code/app"],
            vec!["taw", "skill"],
            vec!["taw", "--pretty", "pane", "list"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "failed to parse {argv:?}: {:?}",
                Cli::try_parse_from(&argv).err().map(|e| e.to_string())
            );
        }
    }

    #[test]
    fn agent_start_takes_native_args_after_a_double_dash() {
        let cli = Cli::try_parse_from([
            "taw", "agent", "start", "rev", "--kind", "codex", "--pane", "w1:p1", "--", "--model",
            "o3",
        ])
        .unwrap();
        match cli.command {
            Group::Agent {
                cmd: agent::Cmd::Start { agent_args, .. },
            } => assert_eq!(agent_args, vec!["--model", "o3"]),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn agent_start_takes_sandbox_and_a_profile_only_with_it() {
        let cli = Cli::try_parse_from([
            "taw",
            "agent",
            "start",
            "rev",
            "--kind",
            "claude",
            "--pane",
            "w1:p1",
            "--sandbox",
            "--profile",
            "max",
            "--",
            "--model",
            "opus",
        ])
        .unwrap();
        match cli.command {
            Group::Agent {
                cmd:
                    agent::Cmd::Start {
                        sandbox,
                        profile,
                        agent_args,
                        ..
                    },
            } => {
                assert!(sandbox);
                assert_eq!(profile.as_deref(), Some("max"));
                assert_eq!(agent_args, ["--model", "opus"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            Cli::try_parse_from([
                "taw",
                "agent",
                "start",
                "rev",
                "--kind",
                "claude",
                "--pane",
                "w1:p1",
                "--profile",
                "max"
            ])
            .is_err()
        );
    }

    /// `pane report-agent` and `report-agent-session` take the command that resumes the agent's
    /// session after `--`, as herdr's do, flags and all; with no `--` there is none, and so it is with
    /// a `--` that nothing follows, which clap does not tell from no `--` at all (herdr refuses that
    /// one as an empty command).
    #[test]
    fn the_report_verbs_take_a_resume_command_after_a_double_dash() {
        let resume_of = |argv: &[&str]| match Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
            .command
        {
            Group::Pane {
                cmd: pane::Cmd::ReportAgent { resume, .. },
            }
            | Group::Pane {
                cmd: pane::Cmd::ReportAgentSession { resume, .. },
            } => resume,
            other => panic!("{argv:?} parsed as {other:?}"),
        };
        let state = [
            "taw",
            "pane",
            "report-agent",
            "%1",
            "--source",
            "s",
            "--agent",
            "a",
            "--state",
            "idle",
        ];
        let session = [
            "taw",
            "pane",
            "report-agent-session",
            "%1",
            "--source",
            "s",
            "--agent",
            "a",
        ];
        for verb in [&state[..], &session[..]] {
            assert_eq!(resume_of(verb), None);
            let with = [verb, &["--", "a", "--resume", "id", "--model", "x y"]].concat();
            assert_eq!(
                resume_of(&with),
                Some(
                    ["a", "--resume", "id", "--model", "x y"]
                        .map(String::from)
                        .to_vec()
                )
            );
            let flags_after = [verb, &["--", "--resume"]].concat();
            assert_eq!(resume_of(&flags_after), Some(vec!["--resume".to_string()]));
            let empty = [verb, &["--"]].concat();
            assert_eq!(resume_of(&empty), None, "{empty:?}");
        }
    }

    /// `taw open [TERMS]...`: any number of words or paths, and `--json` before or after.
    #[test]
    fn open_takes_any_terms_and_a_json_flag() {
        let cases: [(&[&str], &[&str], bool); 8] = [
            (&["taw", "open"], &[], false),
            (&["taw", "open", "api"], &["api"], false),
            (&["taw", "open", "my", "app"], &["my", "app"], false),
            (&["taw", "open", "../app"], &["../app"], false),
            (&["taw", "open", "~/code/app"], &["~/code/app"], false),
            (&["taw", "open", "--json", "api"], &["api"], true),
            (&["taw", "open", "api", "--json"], &["api"], true),
            (&["taw", "open", "--", "-odd"], &["-odd"], false),
        ];
        for (argv, want_terms, want_json) in cases {
            let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                Group::Open { terms, json } => {
                    assert_eq!(terms, want_terms, "{argv:?}");
                    assert_eq!(json, want_json, "{argv:?}");
                }
                other => panic!("{argv:?} parsed as {other:?}"),
            }
        }
    }

    /// The one line `taw --help` gives the command: what it is, and where it comes from.
    #[test]
    fn open_says_what_it_is_in_one_line() {
        let cli = Cli::command();
        let open = cli.find_subcommand("open").expect("taw open is registered");
        assert_eq!(
            open.get_about().map(ToString::to_string).as_deref(),
            Some(
                "Switch to, or create, the session for a directory or a zoxide match \
                 (tmux-session-wizard's t)"
            )
        );
    }

    /// `taw open --help` and `taw --help` are what a person reads: no bracketed codes, no
    /// markdown emphasis, and none of the marks the wording rule keeps out.
    #[test]
    fn open_help_reads_as_plain_prose() {
        let cli = Cli::command();
        let mut texts = Vec::new();
        help_texts(&cli, "", &mut texts);
        let open: Vec<_> = texts
            .iter()
            .filter(|(path, _)| path.starts_with(" taw open") || path == " taw")
            .collect();
        assert!(
            open.iter().any(|(path, _)| path.ends_with("--json")),
            "the walk must reach --json: {open:?}"
        );
        assert!(
            open.iter().any(|(path, _)| path.ends_with("--terms")),
            "the walk must reach the terms: {open:?}"
        );
        for (path, text) in open {
            assert!(
                !has_bracketed_code(text),
                "{path}: bracketed code in help text: {text}"
            );
            assert!(
                !has_markdown_emphasis(text),
                "{path}: markdown emphasis in help text: {text}"
            );
            for (mark, what) in MARKS {
                assert!(!text.contains(mark), "{path}: {what}: {text}");
            }
        }
    }

    /// Every command of the tree, hidden ones too, with the words a person types to reach it.
    fn every_command(cmd: &clap::Command, path: &str, out: &mut Vec<(String, clap::Command)>) {
        let path = if path.is_empty() {
            cmd.get_name().to_string()
        } else {
            format!("{path} {}", cmd.get_name())
        };
        out.push((path.clone(), cmd.clone()));
        for sub in cmd.get_subcommands() {
            every_command(sub, &path, out);
        }
    }

    /// The help of every command reads as plain prose: none of the marks
    /// above in what `--help` and `-h` print, nor in any string they are made from, and that
    /// for the hidden commands as well, which no `--help` lists. A doc comment on a command,
    /// an option or a value is reached wherever it sits, because the walk is of the tree as
    /// clap builds it and not of a list kept here.
    #[test]
    fn the_help_of_every_command_uses_none_of_the_forbidden_marks() {
        let mut root = Cli::command();
        root.build();
        let mut commands = Vec::new();
        every_command(&root, "", &mut commands);
        // A walk that saw little guards little: the tree has well over a hundred commands,
        // some of them hidden and some of them deep.
        assert!(
            commands.len() > 100,
            "the walk reached {} commands",
            commands.len()
        );
        for named in ["taw hooks exec", "taw agent view set", "taw plugin event"] {
            assert!(
                commands.iter().any(|(path, _)| path == named),
                "the walk did not reach {named}"
            );
        }

        let mut broken = Vec::new();
        for (path, cmd) in &commands {
            for (how, text) in [
                ("--help", cmd.clone().render_long_help().to_string()),
                ("-h", cmd.clone().render_help().to_string()),
            ] {
                for (mark, what) in MARKS {
                    if text.contains(mark) {
                        broken.push(format!("{path} {how}: {what}"));
                    }
                }
            }
        }
        // The strings the help is made from, including an option no help lists.
        let mut raw = Vec::new();
        help_texts(&root, "", &mut raw);
        assert!(
            raw.len() > 400,
            "the walk reached {} help strings",
            raw.len()
        );
        for (path, text) in raw {
            for (mark, what) in MARKS {
                if text.contains(mark) {
                    broken.push(format!("{}: {what} in {text:?}", path.trim()));
                }
            }
        }
        assert!(
            broken.is_empty(),
            "the help of taw says what it may not:\n{}",
            broken.join("\n")
        );
    }

    /// `taw --help` says which commands do not print an envelope, and `taw open` is one.
    #[test]
    fn taw_help_names_open_among_the_commands_without_an_envelope() {
        let cli = Cli::command();
        let long = cli
            .get_long_about()
            .expect("taw has a long help")
            .to_string();
        assert!(!long.contains("Two commands are exceptions"), "{long}");
        assert!(!long.contains("Three commands are exceptions"), "{long}");
        for named in [
            "`taw skill`",
            "`taw config default`",
            "`taw open`",
            "`--json`",
            "`taw sandbox`",
            "`taw app`",
            "`taw machine feed`",
            "`taw machine attach`",
            "`taw machine check`",
        ] {
            assert!(long.contains(named), "{named} is missing from {long}");
        }
    }

    #[test]
    fn sandbox_takes_an_agent_a_profile_and_the_agents_own_arguments_after_a_double_dash() {
        let parse = |argv: &[&str]| match Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
            .command
        {
            Group::Sandbox(args) => (
                args.cmd.is_none(),
                args.agent,
                args.profile,
                args.agent_args,
            ),
            other => panic!("{argv:?} parsed as {other:?}"),
        };
        assert_eq!(parse(&["taw", "sandbox"]), (true, None, None, vec![]));
        assert_eq!(
            parse(&["taw", "sandbox", "codex"]),
            (true, Some("codex".into()), None, vec![])
        );
        assert_eq!(
            parse(&["taw", "sandbox", "claude", "--profile", "max"]),
            (true, Some("claude".into()), Some("max".into()), vec![])
        );
        assert_eq!(
            parse(&[
                "taw",
                "sandbox",
                "--profile",
                "max",
                "claude",
                "--",
                "--model",
                "opus",
                "--profile",
                "x"
            ]),
            (
                true,
                Some("claude".into()),
                Some("max".into()),
                vec![
                    "--model".into(),
                    "opus".into(),
                    "--profile".into(),
                    "x".into()
                ]
            )
        );
        assert_eq!(
            parse(&["taw", "sandbox", "--", "--resume", "4b1f"]),
            (true, None, None, vec!["--resume".into(), "4b1f".into()])
        );
    }

    /// A placement flag is refused as an unknown one, with the usage line, and with a tip that says
    /// where taw sandbox runs and how to make the place first, not clap's own (to pass it after
    /// --, where it would go to the agent). A flag meant for the agent keeps clap's tip, which is
    /// right for it.
    #[test]
    fn sandbox_refuses_every_placement_flag_as_an_unknown_one() {
        for flag in [
            "--split",
            "--split=right",
            "--window",
            "--session",
            "--worktree",
            "--here",
            "--no-focus",
        ] {
            for argv in [
                vec!["taw", "sandbox", flag],
                vec!["taw", "sandbox", "codex", "--profile", "max", flag],
            ] {
                let err = parse_from(&argv).expect_err(flag);
                assert_eq!(
                    err.kind(),
                    clap::error::ErrorKind::UnknownArgument,
                    "{argv:?}"
                );
                let text = err.to_string();
                assert!(text.contains("Usage: taw sandbox"), "{argv:?}: {text}");
                assert!(
                    text.contains(
                        "tip: taw sandbox runs the agent in the pane it is typed in: make the place first (prefix % or prefix \" for a pane beside this one, prefix c for a window, the popup's n or w for a session or a worktree), then type taw sandbox there"
                    ),
                    "{argv:?}: {text}"
                );
                assert!(!text.contains(&format!("-- {flag}")), "{argv:?}: {text}");
                assert_eq!(text.matches("tip:").count(), 1, "{argv:?}: {text}");
            }
        }
        for argv in [
            vec!["taw", "sandbox", "--json"],
            vec!["taw", "sandbox", "--resume"],
            vec!["taw", "sandbox", "new", "mine", "--split"],
            vec!["taw", "agent", "list", "--split"],
        ] {
            let text = parse_from(&argv).expect_err("refused").to_string();
            assert!(!text.contains("make the place first"), "{argv:?}: {text}");
            if argv[1] == "sandbox" {
                assert!(text.contains("use '-- --"), "{argv:?}: {text}");
            }
        }
        assert!(Cli::try_parse_from(["taw", "sandbox", "claude", "codex"]).is_err());
    }

    #[test]
    fn new_and_list_are_taws_own_words_and_never_an_agent() {
        match Cli::try_parse_from(["taw", "sandbox", "new", "mine", "--from", "box"])
            .unwrap()
            .command
        {
            Group::Sandbox(args) => assert!(
                matches!(args.cmd, Some(sandbox::Cmd::New { ref name, ref from }) if name == "mine" && from.as_deref() == Some("box"))
            ),
            other => panic!("{other:?}"),
        }
        match Cli::try_parse_from(["taw", "sandbox", "list"])
            .unwrap()
            .command
        {
            Group::Sandbox(args) => assert!(matches!(args.cmd, Some(sandbox::Cmd::List))),
            other => panic!("{other:?}"),
        }
        // Taw's own words are never an agent's name, even after a flag of the start's.
        for word in ["new", "list"] {
            let err =
                Cli::try_parse_from(["taw", "sandbox", "--profile", "max", word]).expect_err(word);
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{word}"
            );
            assert!(
                err.to_string().contains(&format!(
                    "{word} is a word of taw sandbox's own, and comes right after it: taw sandbox {word}"
                )),
                "{word}: {err}"
            );
        }
        assert_eq!(result_type("sandbox", "new"), "sandbox_profile");
        assert_eq!(result_type("sandbox", "list"), "sandbox_list");
    }

    /// `taw machine attach <target> --holder <id>`: the holder is required and is not among the options, and the
    /// verb is read here and not forwarded (only the option `--machine` is).
    #[test]
    fn machine_attach_takes_a_target_and_a_hidden_holder() {
        match Cli::try_parse_from(["taw", "machine", "attach", "%5", "--holder", "mac-1"])
            .unwrap()
            .command
        {
            Group::Machine {
                cmd:
                    machine::Cmd::Attach {
                        target,
                        new,
                        holder,
                    },
            } => assert_eq!(
                (target.as_deref(), new, holder.as_str()),
                (Some("%5"), false, "mac-1")
            ),
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["taw", "machine", "attach", "%5"]).is_err());
        // `--new` is the other way to say what to attach to: it takes no target, and a target takes no `--new`.
        match Cli::try_parse_from(["taw", "machine", "attach", "--new", "--holder", "mac-1"])
            .unwrap()
            .command
        {
            Group::Machine {
                cmd: machine::Cmd::Attach { target, new, .. },
            } => assert_eq!((target, new), (None, true)),
            other => panic!("{other:?}"),
        }
        assert!(
            Cli::try_parse_from(["taw", "machine", "attach", "--holder", "mac-1"]).is_err(),
            "a target or --new is needed"
        );
        assert!(
            Cli::try_parse_from([
                "taw", "machine", "attach", "%5", "--new", "--holder", "mac-1"
            ])
            .is_err(),
            "--new and a target are exclusive"
        );
        let help = Cli::try_parse_from(["taw", "machine", "attach", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("[TARGET]"), "positive half: {help}");
        // The usage line has to name what is required; the list of options does not describe it.
        let options = help.split("Options:").nth(1).expect("an options list");
        assert!(!options.contains("--holder"), "{help}");
    }

    /// `taw machine list` is the verb group of that name, which this machine's parser reads: only the
    /// option `--machine` is sent over before it. The envelope is `cli:machine:list`, a `machine_list`.
    #[test]
    fn machine_list_is_a_verb_group_and_not_the_machine_option() {
        let words = |line: &[&str]| line.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        match Cli::try_parse_from(["taw", "machine", "list"])
            .unwrap()
            .command
        {
            Group::Machine { cmd } => assert!(matches!(cmd, machine::Cmd::List)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            crate::machine::forward::leading(&words(&["machine", "list"])),
            None
        );
        assert_eq!(
            crate::machine::forward::leading(&words(&["--pretty", "machine", "list"])),
            None
        );
        assert!(
            crate::machine::forward::leading(&words(&["--machine", "studio", "machine", "list"]))
                .is_some()
        );
        assert_eq!(envelope_id("machine", "list"), "cli:machine:list");
        assert_eq!(result_type("machine", "list"), "machine_list");
    }

    /// `taw machine add <name> <ssh-host> [--taw <path>]` and `taw machine remove <name>` are verbs of the
    /// group, with an envelope each.
    #[test]
    fn machine_add_and_remove_are_verbs_with_an_envelope_each() {
        match Cli::try_parse_from(["taw", "machine", "add", "pi", "me@pi", "--taw", "~/bin/taw"])
            .unwrap()
            .command
        {
            Group::Machine {
                cmd:
                    machine::Cmd::Add {
                        name,
                        ssh_host,
                        taw,
                    },
            } => assert_eq!(
                (name.as_str(), ssh_host.as_str(), taw.as_deref()),
                ("pi", "me@pi", Some("~/bin/taw"))
            ),
            other => panic!("{other:?}"),
        }
        match Cli::try_parse_from(["taw", "machine", "remove", "pi"])
            .unwrap()
            .command
        {
            Group::Machine {
                cmd: machine::Cmd::Remove { name },
            } => assert_eq!(name, "pi"),
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["taw", "machine", "add", "pi"]).is_err());
        assert_eq!(envelope_id("machine", "add"), "cli:machine:add");
        assert_eq!(result_type("machine", "add"), "machine_added");
        assert_eq!(envelope_id("machine", "remove"), "cli:machine:remove");
        assert_eq!(result_type("machine", "remove"), "machine_removed");
    }

    /// `taw open --json` is `cli:open`, and its result is a `session_open`, whose fields
    /// are `taw session open`'s.
    #[test]
    fn open_reports_the_result_type_session_open_does() {
        assert_eq!(envelope_id("open", ""), "cli:open");
        assert_eq!(result_type("open", ""), "session_open");
        assert_eq!(result_type("session", "open"), "session_open");
    }

    /// `--new-session` starts the popup on the new-session picker. It is a flag of `ui` and of
    /// `ui open` (which passes it on to the popup it runs), before or after `open`, and off
    /// unless it is given.
    #[test]
    fn new_session_is_a_flag_of_ui_and_of_ui_open() {
        for (argv, on) in [
            (vec!["taw", "ui"], false),
            (vec!["taw", "ui", "open"], false),
            (vec!["taw", "ui", "--new-session"], true),
            (vec!["taw", "ui", "open", "--new-session"], true),
            (vec!["taw", "ui", "--new-session", "open"], true),
            (vec!["taw", "ui", "--from", "%1", "--new-session"], true),
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap();
            match cli.command {
                Group::Ui { new_session, .. } => assert_eq!(new_session, on, "{argv:?}"),
                other => panic!("{argv:?} parsed as {other:?}"),
            }
        }
    }

    /// `--from` is a flag of `ui` and of `ui open`, before `open` or after it, as `--new-session`
    /// is: the launcher passes on the pane it names to the popup it opens.
    #[test]
    fn from_is_a_flag_of_ui_and_of_ui_open() {
        for (argv, open) in [
            (vec!["taw", "ui", "--from", "%9"], false),
            (vec!["taw", "ui", "--from", "%9", "open"], true),
            (vec!["taw", "ui", "open", "--from", "%9"], true),
        ] {
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            match cli.command {
                Group::Ui { from, cmd, .. } => {
                    assert_eq!(from.as_deref(), Some("%9"), "{argv:?}");
                    assert_eq!(matches!(cmd, Some(UiCmd::Open)), open, "{argv:?}");
                }
                other => panic!("{argv:?} parsed as {other:?}"),
            }
        }
    }

    /// The popup is told which client it was opened on with `--client`, which the launcher
    /// puts on the command it runs and nobody types: it parses with the other flags of `ui`,
    /// and `taw ui --help` does not list it.
    #[test]
    fn client_is_a_hidden_flag_of_ui() {
        let cli = Cli::try_parse_from([
            "taw",
            "ui",
            "--from",
            "%5",
            "--client",
            "/dev/ttys021",
            "--bar",
            "--new-session",
        ])
        .unwrap();
        match cli.command {
            Group::Ui {
                client,
                from,
                bar,
                new_session,
                ..
            } => assert_eq!(
                (client.as_deref(), from.as_deref(), bar, new_session),
                (Some("/dev/ttys021"), Some("%5"), true, true)
            ),
            other => panic!("parsed as {other:?}"),
        }
        let none = Cli::try_parse_from(["taw", "ui"]).unwrap();
        assert!(matches!(none.command, Group::Ui { client: None, .. }));

        let help = Cli::command()
            .find_subcommand("ui")
            .expect("taw ui is registered")
            .clone()
            .render_long_help()
            .to_string();
        assert!(!help.contains("--client"), "{help}");
        assert!(help.contains("--new-session"), "{help}");
    }

    /// `taw ui --help`, and `-h`, as a person reads them, whole: what the command is, its one
    /// subcommand, `--from` and its fallback, `--new-session`, and the global `--pretty`. The two
    /// flags the launcher puts on the command it runs, `--bar` and `--client`, are not in it. A
    /// flag added, dropped, shown or reworded changes this text. Rendered the way the binary
    /// prints it, from what clap returns for the flag, since clap picks the layout itself.
    #[test]
    fn taw_ui_help_says_what_the_popup_command_takes() {
        let printed = |flag: &str| -> String {
            let err = Cli::try_parse_from(["taw", "ui", flag]).expect_err("help is not a command");
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "{flag}");
            err.render().to_string()
        };
        let expected = "\
Open the agent popup

Usage: taw ui [OPTIONS] [COMMAND]

Commands:
  open  Size and open the popup, leaving the status line visible

Options:
      --from <PANE>  The pane the popup opens from (`taw ui open` passes this on, or names its own pane; `taw ui` defaults to `$TMUX_PANE`)
      --pretty       Indent the JSON envelope instead of printing it on one line
      --new-session  Start with the new-session picker open, and close the popup on Esc
  -h, --help         Print help
";
        assert_eq!(printed("--help"), expected);
        assert_eq!(printed("-h"), expected);
    }

    #[test]
    fn agent_view_set_needs_a_source_and_refuses_both_input_routes_at_once() {
        // herdr requires `source`; the only way to supply it other than `--source` is
        // inside `--params`, and the two routes are mutually exclusive so a caller cannot
        // half-specify a view.
        assert!(Cli::try_parse_from(["taw", "agent", "view", "set"]).is_err());
        assert!(
            Cli::try_parse_from([
                "taw", "agent", "view", "set", "--params", "{}", "--source", "s"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "taw", "agent", "view", "set", "--params", "{}", "--sort", "[]"
            ])
            .is_err()
        );
    }

    #[test]
    fn a_missing_required_flag_is_a_parse_error_not_a_panic() {
        // `report-agent` needs --source/--agent/--state; clap rejects it (exit 2).
        assert!(Cli::try_parse_from(["taw", "pane", "report-agent", "%1"]).is_err());
        assert!(Cli::try_parse_from(["taw", "agent", "get"]).is_err());
        assert!(Cli::try_parse_from(["taw", "definitely-not-a-group"]).is_err());
        assert!(Cli::try_parse_from(["taw", "pane", "definitely-not-a-verb"]).is_err());
    }
}
