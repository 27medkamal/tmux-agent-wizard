// Derived from herdr (https://github.com/herdrdev/herdr), Apache-2.0, modified for taw; see THIRD_PARTY_LICENSES.md.

//! Which agent, if any, the processes in front of a pane are.
//!
//! [`identify`](super::identity::identify) names an agent by a program's name, and the name tmux gives a
//! pane's command is the name of its foreground process. An agent is often not that program: an npm
//! package's is `node /path/to/bin/codex`, a script's is `sh /path/to/pi`, a Python one's is
//! `python3.12 /path/to/hermes`, and what tmux calls those is `node`, `sh` or `python3.12`. herdr reads
//! the arguments of the processes in the terminal's foreground job to see through them
//! (`identify_agent_in_job`), and a verb that types into a pane asks it whether the agent the pane
//! records is the one in front of it (`runtime_hosts_agent`), so that what is typed never reaches a
//! shell, a pager or an editor the user started after the agent had gone. A positive identification
//! is what it asks for: a process that names no agent is no agent, and a shell at its prompt is not.
//!
//! Nothing here reads the system. The processes, with their names and arguments, are what the caller
//! gives, and the one thing that does touch it is the symlink resolution of a path in an argument.

use std::borrow::Cow;

use super::container;
use super::identity::{basename, identify as identify_name, normalize};

/// One process of a [`Job`], as the process table has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    /// What the table calls the program, without the directories before it.
    pub name: String,
    /// Its arguments, one string each, `argv[0]` first, when they could be read exactly.
    pub argv: Option<Vec<String>>,
    /// The line `ps` prints for them, joined by spaces, when the list could not be had.
    pub line: Option<String>,
}

/// The processes of a terminal's foreground process group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Job {
    /// The group's id, which is its leader's pid.
    pub group: u32,
    pub processes: Vec<Process>,
}

/// The agent the job is, as an id the way [`identify`](super::identity::identify) names one, or `None`
/// when none of its processes is one.
///
/// The group's leader is the one tmux names the pane's command by, so it is asked first, and an
/// agent it is wins. Otherwise the process that best says so does: one whose arguments name the
/// agent (`node /path/to/bin/codex`) over one a program's own name does, over a shell or a
/// runtime, which is what herdr's `process_priority` ranks.
pub fn identify(job: &Job) -> Option<String> {
    identified(job).map(|(agent, _)| agent)
}

/// [`identify`], with the pid of the process that names the agent.
pub fn identify_process(job: &Job) -> Option<(String, u32)> {
    identified(job).map(|(agent, process)| (agent, process.pid))
}

/// [`identify_process`] with no container runtime among the processes asked: what the watcher names
/// the job in front of a pane by. The watcher reads a runtime's arguments only while the runtime is
/// in front of the pane, where it marks the pane with the runtime too (`src/watch/container.rs`); a
/// runtime that a shell of its own keeps behind it (a sandbox profile that does not `exec` it) leaves
/// that shell in front, which names no agent, so that no agent is taken without its container.
pub fn identify_process_outside_containers(job: &Job) -> Option<(String, u32)> {
    let outside = Job {
        group: job.group,
        processes: job
            .processes
            .iter()
            .filter(|process| container::runtime(&process.name).is_none())
            .cloned()
            .collect(),
    };
    identify_process(&outside)
}

/// The process of `job` that [`identify`] names Claude by, and so the one whose arguments are Claude's
/// own: a native `claude`, `node .../@anthropic-ai/claude-code/cli.js`, or a script that is it. `None`
/// when the job is not Claude, and when what names it is a container runtime, whose arguments are the
/// runtime's and not the agent's.
pub fn claude_process(job: &Job) -> Option<&Process> {
    let (agent, process) = identified(job)?;
    (agent == "claude" && container::runtime(&process.name).is_none()).then_some(process)
}

/// [`identify`], with the process that named the agent.
fn identified(job: &Job) -> Option<(String, &Process)> {
    if let Some(leader) = job
        .processes
        .iter()
        .find(|process| process.pid == job.group)
        && let Some((agent, _)) = agent_of(leader)
    {
        return Some((agent, leader));
    }
    let mut best: Option<(u8, String, &Process)> = None;
    for process in &job.processes {
        let Some((agent, score)) = agent_of(process) else {
            continue;
        };
        match &best {
            Some((best_score, ..)) if *best_score >= score => {}
            _ => best = Some((score, agent, process)),
        }
    }
    best.map(|(_, agent, process)| (agent, process))
}

/// The agent `process` is, with how well it says so ([`process_priority`]).
fn agent_of(process: &Process) -> Option<(String, u8)> {
    if let Some(agent) = container_agent(process) {
        return Some((agent, 2));
    }
    let candidate = normalized_process_name(process);
    let agent = agent_named(&candidate)?;
    // A Letta that runs one prompt, or a server, is not the interactive agent.
    if agent == "letta" && !is_interactive_letta_process(process) {
        return None;
    }
    Some((agent, process_priority(process, &candidate)))
}

/// The agent a container runtime in front of the pane starts, from the runtime's arguments.
fn container_agent(process: &Process) -> Option<String> {
    container::runtime(&process.name)?;
    let found = match (&process.argv, &process.line) {
        (Some(argv), _) => container::identify_argv(&process.name, argv),
        (None, Some(line)) => container::identify_ps(&process.name, line),
        (None, None) => None,
    };
    found.map(|agent| agent.kind)
}

/// The id of the agent `name` is, by [`identify_name`], or by the table of the agents herdr names that
/// have no manifest here.
fn agent_named(name: &str) -> Option<String> {
    identify_name(name, None).or_else(|| {
        match normalize(basename(name)).as_str() {
            "omp" => Some("omp"),
            "mastracode" | "mastra-code" | "mastra code" => Some("mastracode"),
            _ => None,
        }
        .map(str::to_string)
    })
}

/// The arguments of `process` as words: the exact list, or the line `ps` printed split at white space.
fn args(process: &Process) -> Option<Cow<'_, [String]>> {
    match (&process.argv, &process.line) {
        (Some(argv), _) => Some(Cow::Borrowed(argv.as_slice())),
        (None, Some(line)) => Some(Cow::Owned(
            line.split_whitespace().map(str::to_string).collect(),
        )),
        (None, None) => None,
    }
}

/// What `process` runs, as a name [`agent_named`] can read: the agent a runtime or a shell is
/// wrapping, the process's own name when that names an agent, or an agent its first argument is.
fn normalized_process_name(process: &Process) -> String {
    let effective = process.name.as_str();
    let lower_effective = effective.to_lowercase();
    let argv = args(process);
    let argv = argv.as_deref();

    if is_generic_runtime_or_shell(&lower_effective)
        && let Some(wrapped) = wrapped_agent_name_from_runtime_argv(&lower_effective, argv)
    {
        return wrapped;
    }

    if agent_named(effective).is_some() {
        return effective.to_string();
    }

    // A process whose own name says nothing (a thread's name, a version number) that is a Node or
    // Bun program: only the agents that are always that are read this way.
    if let Some(runtime) = argv.and_then(|argv| argv.first())
        && matches!(normalize(basename(runtime)).as_str(), "node" | "bun")
        && let Some(wrapped) = wrapped_agent_name_from_runtime_argv(runtime, argv)
        && matches!(
            agent_named(&wrapped).as_deref(),
            Some("qwen" | "cline" | "letta")
        )
    {
        return wrapped;
    }

    if let Some(wrapped) = argv0_agent_name(argv) {
        return wrapped;
    }

    effective.to_string()
}

fn wrapped_agent_name_from_runtime_argv(runtime: &str, argv: Option<&[String]>) -> Option<String> {
    let argv = argv?;
    let runtime_name = normalize(basename(runtime));

    match runtime_name.as_str() {
        "node" | "bun" => {
            script_arg_agent_name(argv, &["-e", "--eval", "-p", "--print"], &[], false)
        }
        name if is_python_runtime(name) => hermes_installer_agent_name(argv)
            .or_else(|| script_arg_agent_name(argv, &["-c"], &["-m"], false)),
        "sh" | "bash" | "zsh" | "fish" => script_arg_agent_name(argv, &["-c"], &[], true),
        _ => None,
    }
}

/// `hermes` when the arguments are the bootstrap that the installer's Python launcher runs Hermes by:
/// `python -I -c <source>`, where the source is the launcher's own, and only the installation root in
/// it may differ. A Hermes started that way is a Python process whose arguments hold the
/// program as text, which `script_arg_agent_name` takes for no script. Two shapes are known: the short
/// bootstrap, which must match to the byte apart from its root (both of its root literals agree), and
/// the longer one with the recovery helpers in front, which is told by its entry point instead. Source
/// that merely quotes it, a root with a quote or an escape in it, and the launcher's own helper modes
/// (`--run-module`, `--print-runtime-command`) are no Hermes.
///
/// Ported from herdr src/detect/mod.rs@5608de0608 (Apache-2.0), `hermes_installer_agent_name` and its
/// constants (e35f3937b0, #4911, and 43b36b68ca, #5518). Modifications: the agent is named by its id,
/// `hermes`, in place of `agent_label(Agent::Hermes)`.
fn hermes_installer_agent_name(argv: &[String]) -> Option<String> {
    let [_, isolation, command, code, args @ ..] = argv else {
        return None;
    };
    if isolation != "-I"
        || command != "-c"
        || args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "--run-module" | "--print-runtime-command"))
    {
        return None;
    }

    let code = code.strip_prefix("import os, re, sys\n")?;
    let (expanded, code) = if code.starts_with("import ") {
        (true, code.split_once('\n')?.1)
    } else {
        (false, code)
    };
    let rest = code.strip_prefix(HERMES_INSTALLER_PREFIX)?;
    let (root, rest) = rest.split_once("')\n")?;
    if root.is_empty() || root.contains(['\'', '\\', '\n', '\r']) {
        return None;
    }
    if expanded {
        // Recovery helpers change independently of the interactive entry point.
        return (rest.ends_with(HERMES_INSTALLER_ENTRY)
            && rest.lines().any(|line| {
                matches!(
                    line,
                    "import hermes_bootstrap" | "    import hermes_bootstrap"
                )
            }))
        .then(|| "hermes".to_string());
    }
    let rest = rest
        .strip_prefix(HERMES_INSTALLER_MIDDLE)?
        .strip_prefix(root)?;
    (rest == HERMES_INSTALLER_SUFFIX).then(|| "hermes".to_string())
}

const HERMES_INSTALLER_ENTRY: &str = r"from hermes_cli.main import main
sys.argv[0] = re.sub(r'(-script\.pyw|\.exe)?$', '', sys.argv[0])
sys.exit(main())
";

// Short installer source captured in #4910. Only the installation root varies.
const HERMES_INSTALLER_PREFIX: &str = "os.environ.pop('PYTHONHOME', None)
os.environ.pop('PYTHONPATH', None)
sys.path.insert(0, '";
const HERMES_INSTALLER_MIDDLE: &str =
    "if sys.argv[1:2] == ['--print-runtime-command']: sys.dont_write_bytecode = True
from hermes_constants import get_default_hermes_root
os.environ['HERMES_HOME'] = os.environ.get('HERMES_HOME') or str(get_default_hermes_root())
if sys.argv[1:2] == ['--print-runtime-command']:
    from pathlib import Path
    from hermes_cli._launchers import print_runtime_command
    print_runtime_command(Path('";
const HERMES_INSTALLER_SUFFIX: &str = r"'), sys.argv[2:])
    sys.exit(0)
import hermes_bootstrap
if sys.argv[1:2] == ['--run-module']:
    import runpy
    if len(sys.argv) < 3: sys.exit('hermes: --run-module needs a module')
    module = sys.argv.pop(2)
    del sys.argv[1]
    runpy.run_module(module, run_name='__main__', alter_sys=True)
    sys.exit(0)
from hermes_cli.main import main
sys.argv[0] = re.sub(r'(-script\.pyw|\.exe)?$', '', sys.argv[0])
sys.exit(main())
";

/// The agent the script a runtime was given is: its first argument that is not an option, unless an
/// option says the program is in the arguments (`-e`, `-c`) or a module (`-m`), and then it is none.
/// For a shell (`shell`), an argument that starts with `+` is an option too, and the options of
/// [`shell_option_takes_value`] take the word after them.
fn script_arg_agent_name(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
    shell: bool,
) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args
                .next()
                .and_then(|token| agent_name_from_path_token(token));
        }

        if flag_matches(arg, eval_flags) || flag_matches(arg, module_flags) {
            return None;
        }

        if arg.starts_with('-') || (shell && arg.starts_with('+')) {
            if option_takes_value(arg) || (shell && shell_option_takes_value(arg)) {
                let _ = args.next();
            }
            continue;
        }

        return agent_name_from_path_token(arg);
    }

    None
}

/// The options of a shell, on top of [`option_takes_value`], that take the word after them as their
/// value: bash's `-O` and `+O` name a `shopt` option, and `+o` names a `set -o` one as `-o` does, so
/// `bash -O extglob <dir>/bin/claude` runs claude. taw's own fix: herdr reads the option's value as the
/// script. Only a shell's: Python's `-O` takes no value.
fn shell_option_takes_value(arg: &str) -> bool {
    matches!(arg, "-O" | "+O" | "+o")
}

fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags
        .iter()
        .any(|flag| arg == *flag || short_flag_payload(arg, flag) || long_flag_value(arg, flag))
}

fn short_flag_payload(arg: &str, flag: &str) -> bool {
    flag.starts_with('-')
        && !flag.starts_with("--")
        && arg.starts_with(flag)
        && arg.len() > flag.len()
}

fn long_flag_value(arg: &str, flag: &str) -> bool {
    flag.starts_with("--")
        && arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

/// The options of the runtimes and shells that take the argument after them as their value, which is
/// then not the script. `--rcfile` and `--init-file` (bash's startup file) are taw's own additions:
/// `bash --rcfile <dir>/bin/claude` runs no script called claude.
fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-r" | "--require"
            | "--loader"
            | "--import"
            | "--experimental-loader"
            | "--inspect-port"
            | "-W"
            | "-X"
            | "-S"
            | "-L"
            | "-o"
            | "--rcfile"
            | "--init-file"
    )
}

fn argv0_agent_name(argv: Option<&[String]>) -> Option<String> {
    agent_name_from_path_token(argv?.first()?)
}

/// The agent a path in an argument names: its last component, a path inside one of the packages an
/// agent is installed as, or what a symlink there points at.
fn agent_name_from_path_token(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '"' | '\''));
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }

    agent_named(basename(trimmed))
        .or_else(|| agent_name_from_known_package_path(trimmed))
        .or_else(|| resolved_agent_name_from_path_token(trimmed))
}

fn agent_name_from_known_package_path(path: &str) -> Option<String> {
    let raw_components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect();
    let ends_with = |suffix: &[&str]| {
        raw_components.len() >= suffix.len()
            && raw_components[raw_components.len() - suffix.len()..]
                .iter()
                .zip(suffix)
                .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
    };
    if ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) || ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "bundle",
        "cli.js",
    ]) {
        return Some("pi".to_string());
    }
    if ends_with(&[
        "node_modules",
        "@oh-my-pi",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) {
        return Some("omp".to_string());
    }
    if ends_with(&[
        "node_modules",
        "@moonshot-ai",
        "kimi-code",
        "dist",
        "main.mjs",
    ]) {
        return Some("kimi".to_string());
    }
    // Not herdr's, taw's own: what `node` runs for an npm, pnpm or bun install of Claude Code or Gemini
    // CLI whose shim names the package's entry and not the agent.
    if ends_with(&["node_modules", "@anthropic-ai", "claude-code", "cli.js"]) {
        return Some("claude".to_string());
    }
    if ends_with(&["node_modules", "@google", "gemini-cli", "dist", "index.js"]) {
        return Some("gemini".to_string());
    }

    let components: Vec<String> = raw_components.into_iter().map(normalize).collect();
    for window in components.windows(5) {
        if window == ["node_modules", "@qwen-code", "qwen-code", "dist", "index"] {
            return Some("qwen".to_string());
        }
    }
    for window in components.windows(4) {
        if window == ["node_modules", "mastracode", "dist", "cli"] {
            return Some("mastracode".to_string());
        }
        if window == ["node_modules", "@letta-ai", "letta-code", "letta"] {
            return Some("letta".to_string());
        }
    }
    None
}

/// The index of the Letta entrypoint in `argv`: `letta` itself, or the script a Node or Bun runtime
/// was given.
fn letta_entrypoint_index(argv: &[String]) -> Option<usize> {
    let is_letta = |arg: &str| agent_name_from_path_token(arg).as_deref() == Some("letta");
    if argv.first().is_some_and(|arg| is_letta(arg)) {
        return Some(0);
    }

    let runtime = argv.first().map(|arg| normalize(basename(arg)))?;
    if !matches!(runtime.as_str(), "node" | "bun") {
        return None;
    }

    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            return argv
                .get(index + 1)
                .is_some_and(|arg| is_letta(arg))
                .then_some(index + 1);
        }
        if flag_matches(arg, &["-e", "--eval", "-p", "--print"]) {
            return None;
        }
        if arg.starts_with('-') {
            index += if option_takes_value(arg) { 2 } else { 1 };
            continue;
        }
        return is_letta(arg).then_some(index);
    }
    None
}

fn letta_first_arg_after_backend_selection(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--backend" {
            let _ = args.next();
            continue;
        }
        if arg.starts_with("--backend=") {
            continue;
        }
        return Some(arg);
    }
    None
}

/// Whether a Letta process is the interactive agent: one that was given a prompt, asked to print, to
/// serve or to describe itself is not what a pane that records Letta is running.
fn is_interactive_letta_process(process: &Process) -> bool {
    let parsed;
    let argv: &[String] = if let Some(argv) = process.argv.as_deref() {
        argv
    } else {
        parsed = process
            .line
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(|arg| arg.trim_matches(|ch| matches!(ch, '\'' | '"')).to_string())
            .collect::<Vec<_>>();
        if parsed.is_empty() {
            return true;
        }
        &parsed
    };

    let cli_args = letta_entrypoint_index(argv)
        .map(|index| &argv[index + 1..])
        .unwrap_or(argv);

    if cli_args.iter().any(|arg| {
        let option = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        matches!(
            option,
            "-p" | "--print"
                | "--prompt"
                | "--json"
                | "--stream-json"
                | "--run"
                | "--disable-memory-guard"
                | "--output-format"
                | "--input-format"
                | "--include-partial-messages"
                | "--from-agent"
                | "--environment"
                | "--env"
                | "--pre-load-skills"
                | "--tags"
                | "--ephemeral"
                | "--stateless"
                | "--max-turns"
                | "--memfs-startup"
                | "-h"
                | "--help"
                | "-v"
                | "--version"
                | "--info"
                | "--update"
                | "--upgrade"
        )
    }) {
        return false;
    }

    letta_first_arg_after_backend_selection(cli_args).is_none_or(|arg| arg.starts_with('-'))
}

/// The agent a symlink in an argument points at, for the path of one (`agent` -> `cursor-agent`).
fn resolved_agent_name_from_path_token(token: &str) -> Option<String> {
    let path = std::path::Path::new(token);
    if path.components().count() < 2 {
        return None;
    }

    let resolved = std::fs::canonicalize(path).ok()?;
    let basename = resolved.file_name()?.to_str()?;
    agent_named(basename)
}

/// How well `process` says it is the agent `normalized_name` names: 3 for one the arguments gave away
/// (its name is not the program's), 2 for a program that is called that, 1 for a shell or a runtime.
fn process_priority(process: &Process, normalized_name: &str) -> u8 {
    let lower_name = normalized_name.to_lowercase();
    if lower_name != process.name.to_lowercase() {
        return 3;
    }
    if !is_generic_runtime_or_shell(&lower_name) {
        return 2;
    }
    1
}

/// Whether `name` is a shell or a runtime (Node, Bun, Python) that runs scripts: a program whose name
/// says nothing of the agent it may be running, so that its arguments have to be read for it.
pub(crate) fn is_generic_runtime_or_shell(name: &str) -> bool {
    let name = normalize(basename(name));
    is_python_runtime(&name)
        || matches!(
            name.as_str(),
            "sh" | "bash" | "zsh" | "fish" | "tmux" | "node" | "bun"
        )
}

fn is_python_runtime(name: &str) -> bool {
    name == "python"
        || name.strip_prefix("python").is_some_and(|version| {
            !version.is_empty()
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, name: &str, argv: &[&str]) -> Process {
        Process {
            pid,
            name: name.to_string(),
            argv: Some(argv.iter().map(|arg| (*arg).to_string()).collect()),
            line: Some(argv.join(" ")),
        }
    }

    fn job(group: u32, processes: Vec<Process>) -> Job {
        Job { group, processes }
    }

    fn alone(name: &str, argv: &[&str]) -> Job {
        job(123, vec![process(123, name, argv)])
    }

    #[test]
    fn the_process_that_names_claude_is_found_whether_native_node_or_a_script() {
        let native = job(
            10,
            vec![
                process(11, "bash", &["bash"]),
                process(10, "claude", &["claude", "--effort", "max"]),
            ],
        );
        let node = job(
            20,
            vec![
                process(21, "bash", &["bash"]),
                process(
                    20,
                    "node",
                    &[
                        "node",
                        "/x/node_modules/@anthropic-ai/claude-code/cli.js",
                        "--model",
                        "x",
                    ],
                ),
            ],
        );
        let script = job(
            30,
            vec![
                process(30, "sh", &["/bin/sh", "/opt/bin/claude", "--effort", "max"]),
                process(31, "cat", &["cat"]),
            ],
        );

        assert_eq!(claude_process(&native).map(|p| p.pid), Some(10));
        assert_eq!(claude_process(&node).map(|p| p.pid), Some(20));
        assert_eq!(claude_process(&script).map(|p| p.pid), Some(30));
    }

    #[test]
    fn no_process_is_claudes_for_another_agent_a_shell_or_a_container_runtime() {
        assert_eq!(
            claude_process(&alone("codex", &["codex", "--effort", "x"])),
            None
        );
        assert_eq!(claude_process(&alone("bash", &["bash"])), None);
        // The runtime's arguments are the runtime's: the agent inside has its own.
        let docker = alone(
            "docker",
            &[
                "docker",
                "run",
                "--rm",
                "-it",
                "dev-image",
                "claude",
                "--effort",
                "max",
            ],
        );
        assert_eq!(identify(&docker).as_deref(), Some("claude"));
        assert_eq!(claude_process(&docker), None);
    }

    #[test]
    fn a_wrapped_codex_is_preferred_to_the_shell_beside_it() {
        let job = job(
            123,
            vec![
                process(1, "node", &["node", "/path/to/bin/codex"]),
                process(2, "bash", &["bash"]),
            ],
        );

        assert_eq!(identify(&job).as_deref(), Some("codex"));
    }

    /// herdr's `identify_agent_in_job_detects_opencode2_as_opencode`: OpenCode 2's preview binary.
    #[test]
    fn opencode2_is_opencode() {
        let job = alone("opencode2", &["opencode2", "--standalone"]);

        assert_eq!(identify(&job).as_deref(), Some("opencode"));
    }

    #[test]
    fn a_node_wrapped_qwen_is_qwen() {
        let job = alone("MainThread", &["node", "/home/user/.fnm/bin/qwen"]);

        assert_eq!(identify(&job).as_deref(), Some("qwen"));
    }

    #[test]
    fn clines_native_binaries_are_cline_whatever_the_separators() {
        for (name, executable) in [
            (
                ".cline",
                "/home/user/.npm/lib/node_modules/cline/bin/.cline",
            ),
            (
                "cline",
                "/usr/local/lib/node_modules/@cline/cli-darwin-arm64/bin/cline",
            ),
            (
                "cline.exe",
                r"C:\Users\user\AppData\Roaming\npm\node_modules\@cline\cli-windows-x64\bin\cline.exe",
            ),
        ] {
            let job = alone(name, &[executable, "--tui"]);

            assert_eq!(identify(&job).as_deref(), Some("cline"), "{name}");
        }
    }

    #[test]
    fn a_node_wrapped_cline_is_cline() {
        for (name, argv) in [
            (
                "MainThread",
                vec!["node", "/home/user/.fnm/bin/cline", "--tui"],
            ),
            (
                "node",
                vec!["node", "/usr/local/lib/node_modules/cline/bin/cline"],
            ),
            (
                "node.exe",
                vec![
                    r"C:\Program Files\nodejs\node.exe",
                    r"C:\Users\user\AppData\Roaming\npm\node_modules\cline\bin\cline",
                ],
            ),
        ] {
            assert_eq!(
                identify(&alone(name, &argv)).as_deref(),
                Some("cline"),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn an_unrelated_mention_of_cline_is_not_cline() {
        for argv in [
            vec!["node"],
            vec!["node", "/path/to/other.js", "cline"],
            vec!["node", "-e", "cline"],
            vec!["node", "/path/to/cline-helper"],
            vec!["/path/to/.cline-helper"],
            vec!["/path/to/other", "/path/to/cline"],
        ] {
            assert_eq!(
                identify(&alone("MainThread", &argv)),
                None,
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn interactive_letta_entrypoints_are_letta() {
        for argv in [
            vec!["letta", "--backend", "local"],
            vec![
                "node",
                "/home/user/project/node_modules/.bin/letta",
                "--conversation",
                "conversation-id",
            ],
            vec![
                "node",
                "/home/user/.npm/node_modules/@letta-ai/letta-code/letta.js",
                "--agent",
                "agent-id",
            ],
        ] {
            assert_eq!(
                identify(&alone("MainThread", &argv)).as_deref(),
                Some("letta"),
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn a_letta_that_runs_one_prompt_or_a_server_is_not_the_interactive_agent() {
        for args in [
            vec!["--prompt", "hello"],
            vec!["--output-format", "json"],
            vec!["--input-format=stream-json"],
            vec!["--ephemeral"],
            vec!["--max-turns=1"],
            vec!["server"],
            vec!["--backend", "local", "server"],
            vec!["fix this bug"],
            vec!["agents", "list"],
            vec!["version"],
        ] {
            let mut argv = vec!["node", "/home/user/project/node_modules/.bin/letta"];
            argv.extend(args);

            assert_eq!(
                identify(&alone("MainThread", &argv)),
                None,
                "argv: {argv:?}"
            );
        }

        assert_eq!(
            identify(&alone("node", &["node", "/tmp/server.js", "letta"])),
            None
        );
        assert_eq!(
            identify(&alone(
                "node",
                &["node", "/home/user/src/letta-code/letta/build.js"]
            )),
            None
        );
    }

    #[test]
    fn a_recognized_process_group_leader_is_preferred() {
        let job = job(
            42,
            vec![
                process(42, "claude", &["claude"]),
                process(43, "node", &["node", "/tmp/mcp/bin/codex"]),
            ],
        );

        assert_eq!(identify(&job).as_deref(), Some("claude"));
    }

    #[test]
    fn another_process_names_the_agent_when_the_leader_does_not() {
        let job = job(
            42,
            vec![
                process(42, "bash", &["bash"]),
                process(43, "node", &["node", "/tmp/mcp/bin/codex"]),
            ],
        );

        assert_eq!(identify(&job).as_deref(), Some("codex"));
    }

    #[test]
    fn a_python_versioned_hermes_is_hermes() {
        let job = alone(
            "python3.12",
            &[
                "/nix/store/example/bin/python3.12",
                "/nix/store/example/bin/hermes",
                "--resume",
                "session-id",
            ],
        );

        assert_eq!(identify(&job).as_deref(), Some("hermes"));
    }

    #[test]
    fn a_nix_wrapped_agent_is_named_by_the_first_argument() {
        let codex = alone(
            ".codex-wrapped",
            &["/etc/profiles/per-user/user/bin/codex", "--model", "gpt-5"],
        );
        let claude = alone(
            ".claude-code-wrapped",
            &["/nix/store/example/bin/claude-code"],
        );

        assert_eq!(identify(&codex).as_deref(), Some("codex"));
        assert_eq!(identify(&claude).as_deref(), Some("claude"));
    }

    #[test]
    fn a_shell_wrapped_pi_is_pi() {
        let job = alone("sh", &["/bin/sh", "/tmp/test-bin/pi"]);

        assert_eq!(identify(&job).as_deref(), Some("pi"));
    }

    #[test]
    fn a_bun_wrapped_omp_is_omp_and_its_other_scripts_are_not() {
        for (runtime, script) in [
            ("bun", "/home/can/.bun/bin/omp"),
            (
                "bun.exe",
                r"C:\Users\herdr\AppData\Roaming\npm\node_modules\@oh-my-pi\pi-coding-agent\dist\cli.js",
            ),
        ] {
            let job = alone(runtime, &[runtime, script]);

            assert_eq!(identify(&job).as_deref(), Some("omp"), "script: {script}");
        }

        let other = alone(
            "bun.exe",
            &[
                "bun.exe",
                r"C:\Users\herdr\AppData\Roaming\npm\node_modules\@oh-my-pi\pi-coding-agent\dist\setup.js",
            ],
        );
        assert_eq!(identify(&other), None);
    }

    #[test]
    fn a_package_cli_is_the_agent_the_package_is() {
        for (argv, agent) in [
            (
                vec![
                    "node.exe",
                    r"C:\Users\herdr\AppData\Roaming\npm\node_modules\@earendil-works\pi-coding-agent\dist\cli.js",
                ],
                "pi",
            ),
            (
                vec![
                    "node",
                    "/home/herdr/.local/pi-node/current/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js",
                ],
                "pi",
            ),
            (
                vec![
                    "node.exe",
                    r"C:\Users\herdr\AppData\Roaming\npm\node_modules\mastracode\dist\cli.js",
                ],
                "mastracode",
            ),
            (
                vec![
                    "node",
                    "/srv/repro/node_modules/@moonshot-ai/kimi-code/dist/main.mjs",
                ],
                "kimi",
            ),
        ] {
            assert_eq!(
                identify(&alone("node", &argv)).as_deref(),
                Some(agent),
                "argv: {argv:?}"
            );
        }
    }

    /// A shell's option that takes a file is not the script the shell runs: `bash --rcfile <file>` reads
    /// the file for its startup, and what follows it is the script, if there is one.
    #[test]
    fn the_file_of_a_shells_rcfile_option_is_not_the_script_it_runs() {
        for argv in [
            vec!["bash", "--rcfile", "/home/user/bin/claude"],
            vec!["bash", "--init-file", "/home/user/bin/claude"],
            vec!["bash", "--rcfile", "/home/user/bin/claude", "-i"],
        ] {
            assert_eq!(identify(&alone("bash", &argv)), None, "argv: {argv:?}");
        }
        // The script that follows the option is the script.
        for option in ["--rcfile", "--init-file"] {
            let argv = [
                "bash",
                option,
                "/home/user/.bashrc",
                "/home/user/bin/claude",
            ];
            assert_eq!(
                identify(&alone("bash", &argv)).as_deref(),
                Some("claude"),
                "argv: {argv:?}"
            );
        }
    }

    /// bash's `-O` and `+O` name a `shopt` option and `+o` a `set -o` one, as `-o` does, so the word after
    /// each is no script: `bash -O extglob <dir>/bin/claude` runs claude, and `bash -O <dir>/bin/claude`
    /// runs nothing called claude (bash says the path is an invalid shell option name). taw's own fix:
    /// herdr reads the option's value as the script.
    #[test]
    fn the_option_a_shells_dash_capital_o_names_is_not_the_script_it_runs() {
        for argv in [
            vec!["bash", "-O", "extglob", "/home/user/bin/claude"],
            vec!["bash", "+O", "extglob", "/home/user/bin/claude"],
            vec!["bash", "+o", "posix", "/home/user/bin/claude"],
            vec!["sh", "-O", "nullglob", "/home/user/bin/claude"],
            vec![
                "bash",
                "-O",
                "extglob",
                "-O",
                "globstar",
                "/home/user/bin/claude",
            ],
        ] {
            assert_eq!(
                identify(&alone(argv[0], &argv)).as_deref(),
                Some("claude"),
                "argv: {argv:?}"
            );
        }
        for argv in [
            vec!["bash", "-O", "/home/user/bin/claude"],
            vec!["bash", "+O", "/home/user/bin/claude"],
        ] {
            assert_eq!(identify(&alone("bash", &argv)), None, "argv: {argv:?}");
        }
        // Python's `-O` takes no value: the script is the word after it.
        assert_eq!(
            identify(&alone(
                "python3",
                &["python3", "-O", "/home/user/bin/hermes"]
            ))
            .as_deref(),
            Some("hermes")
        );
    }

    /// A taw supplement to herdr's package paths: the npm and pnpm shim forms of Claude Code and Gemini
    /// CLI, where `node` runs the package's own entry (`@anthropic-ai/claude-code/cli.js`,
    /// `@google/gemini-cli/dist/index.js`) and no argument says `claude` or `gemini`.
    #[test]
    fn the_package_entries_of_claude_code_and_gemini_cli_are_those_agents() {
        for (script, agent) in [
            (
                "/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            (
                "/home/u/.local/share/pnpm/global/5/node_modules/.pnpm/@anthropic-ai+claude-code@2.1.0/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            (
                "/home/u/.bun/install/global/node_modules/@anthropic-ai/claude-code/cli.js",
                "claude",
            ),
            (
                "/usr/local/lib/node_modules/@google/gemini-cli/dist/index.js",
                "gemini",
            ),
            (
                "/Users/u/.npm-global/lib/node_modules/@Google/Gemini-CLI/dist/INDEX.js",
                "gemini",
            ),
        ] {
            for runtime in ["node", "bun"] {
                assert_eq!(
                    identify(&alone(runtime, &[runtime, script])).as_deref(),
                    Some(agent),
                    "{runtime} {script}"
                );
            }
        }
        // What lives in those packages and is not their entry, and what only looks like them.
        for script in [
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/scripts/postinstall.js",
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js/other.js",
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.mjs",
            "/usr/local/lib/node_modules/@anthropic-ai/other/cli.js",
            "/usr/local/lib/node_modules/@other/claude-code/cli.js",
            "/work/claude-code/cli.js",
            "/usr/local/lib/node_modules/@google/gemini-cli/dist/other.js",
            "/usr/local/lib/node_modules/@google/gemini-cli/index.js",
            "/work/gemini-cli/dist/index.js",
        ] {
            assert_eq!(
                identify(&alone("node", &["node", script])),
                None,
                "script: {script}"
            );
        }
    }

    /// herdr's own reading, kept: a script of the user's called for an agent is that agent to herdr,
    /// whatever it does, since it is told by the script's name.
    #[test]
    fn a_users_own_script_named_for_an_agent_is_that_agent_as_herdr_reads_it() {
        for script in [
            "/work/scripts/claude.js",
            "/work/scripts/claude",
            "/work/codex.js",
        ] {
            let agent = if script.contains("codex") {
                "codex"
            } else {
                "claude"
            };
            assert_eq!(
                identify(&alone("node", &["node", script])).as_deref(),
                Some(agent),
                "{script}"
            );
        }
    }

    #[test]
    fn a_script_of_a_package_that_is_not_its_cli_is_not_the_agent() {
        for script in [
            r"C:\Users\herdr\AppData\Roaming\npm\node_modules\@earendil-works\pi-coding-agent\scripts\build.js",
            r"C:\Users\herdr\AppData\Local\pi-node\current\node_modules\@earendil-works\pi-coding-agent\dist\bundle\update.js",
            r"C:\workspace\dist\bundle\cli.js",
            r"C:\workspace\node_modules\other-package\dist\bundle\cli.js",
            r"C:\workspace\node_modules\@earendil-works\pi-coding-agent\dist\cli.exe",
            r"C:\workspace\node_modules\@earendil-works\pi-coding-agent\dist\cli.js\other.js",
            r"C:\workspace\node_modules\@earendil-works\pi-coding-agent\dist\bundle\cli.exe",
            r"C:\workspace\node_modules\@earendil-works\pi-coding-agent\dist\bundle\cli.js\other.js",
        ] {
            assert_eq!(
                identify(&alone("node.exe", &["node.exe", script])),
                None,
                "script: {script}"
            );
        }
    }

    #[test]
    fn opencode_is_found_by_its_package_and_its_argv0() {
        let package = alone(
            "opencode.exe",
            &["/home/user/.local/share/pnpm/global/node_modules/opencode-ai/bin/opencode.exe"],
        );
        let argv0 = alone(
            "MainThread",
            &["/home/user/.local/share/pnpm/global/node_modules/opencode-ai/bin/opencode.exe"],
        );

        assert_eq!(identify(&package).as_deref(), Some("opencode"));
        assert_eq!(identify(&argv0).as_deref(), Some("opencode"));
    }

    #[test]
    fn a_shell_with_only_its_flags_is_no_agent() {
        assert_eq!(
            wrapped_agent_name_from_runtime_argv("bash", Some(&["bash".into(), "-lc".into()])),
            None
        );
    }

    #[test]
    fn an_argument_that_is_code_to_run_is_no_agent_whatever_it_names() {
        for (name, argv) in [
            (
                "python3",
                vec!["python3", "-c", "import time; time.sleep(60)", "/tmp/codex"],
            ),
            (
                "node",
                vec!["node", "-e", "setTimeout(() => {}, 1)", "codex"],
            ),
            ("sh", vec!["sh", "-c", "sleep 60", "codex"]),
            ("python3", vec!["python3", "-m", "http.server", "codex"]),
        ] {
            assert_eq!(identify(&alone(name, &argv)), None, "argv: {argv:?}");
        }
    }

    #[test]
    fn a_python_script_named_for_an_agent_is_that_agent() {
        let job = alone(
            "python3",
            &["python3", "-u", "/tmp/bin/codex", "--model", "x"],
        );

        assert_eq!(identify(&job).as_deref(), Some("codex"));
    }

    #[test]
    fn an_argument_a_runtime_takes_a_value_for_is_skipped() {
        let job = alone(
            "node",
            &["node", "--require", "/tmp/setup.js", "/opt/bin/codex"],
        );

        assert_eq!(identify(&job).as_deref(), Some("codex"));
    }

    #[test]
    fn the_first_word_of_a_printed_line_is_read_as_a_program() {
        let printed = Process {
            pid: 7,
            name: "MainThread".to_string(),
            argv: None,
            line: Some("/nix/store/example/bin/ghcs --resume".to_string()),
        };

        assert_eq!(identify(&job(7, vec![printed])).as_deref(), Some("copilot"));
    }

    #[test]
    fn a_program_whose_name_only_contains_an_agent_is_not_one() {
        for argv in [
            vec!["/tmp/my-codex-helper"],
            vec!["less", "claude"],
            vec!["cat", "/tmp/claude"],
            vec!["vim", "/home/user/claude"],
            vec!["ssh", "codex"],
        ] {
            assert_eq!(identify(&alone(argv[0], &argv)), None, "argv: {argv:?}");
        }
    }

    #[test]
    fn a_shell_at_its_prompt_a_pager_and_an_editor_are_no_agent() {
        for (name, argv) in [
            ("sh", vec!["sh"]),
            ("-zsh", vec!["-zsh"]),
            ("bash", vec!["/bin/bash", "--login"]),
            ("less", vec!["less", "/tmp/notes.txt"]),
            ("cat", vec!["cat"]),
            ("vim", vec!["vim", "/tmp/x"]),
            ("ssh", vec!["ssh", "host"]),
            ("sudo", vec!["sudo", "-s"]),
        ] {
            assert_eq!(identify(&alone(name, &argv)), None, "argv: {argv:?}");
        }
        assert_eq!(identify(&job(1, Vec::new())), None);
    }

    #[test]
    fn a_process_that_cannot_be_read_is_known_only_by_its_name() {
        let named = Process {
            pid: 9,
            name: "claude".to_string(),
            ..Process::default()
        };
        let unnamed = Process {
            pid: 9,
            name: "node".to_string(),
            ..Process::default()
        };

        assert_eq!(identify(&job(9, vec![named])).as_deref(), Some("claude"));
        assert_eq!(identify(&job(9, vec![unnamed])), None);
    }

    #[test]
    fn the_agents_herdr_names_that_have_no_manifest_here_are_named() {
        assert_eq!(identify(&alone("omp", &["omp"])).as_deref(), Some("omp"));
        for name in ["mastracode", "mastra-code", "Mastra Code"] {
            assert_eq!(
                identify(&alone(name, &[name])).as_deref(),
                Some("mastracode"),
                "{name}"
            );
        }
        assert_eq!(identify(&alone("ompx", &["ompx"])), None);
    }

    #[test]
    fn a_container_runtime_is_the_agent_it_starts() {
        let docker = alone(
            "docker",
            &["docker", "run", "--rm", "-it", "dev-image", "codex"],
        );
        let podman_ps = Process {
            pid: 5,
            name: "podman".to_string(),
            argv: None,
            line: Some("podman run --rm -it dev-image claude".to_string()),
        };
        let plain = alone("docker", &["docker", "ps", "-a"]);

        assert_eq!(identify(&docker).as_deref(), Some("codex"));
        assert_eq!(
            identify(&job(5, vec![podman_ps])).as_deref(),
            Some("claude")
        );
        assert_eq!(identify(&plain), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_points_at_an_agent_is_that_agent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cursor-agent");
        let link = dir.path().join("agent");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let argv0 = link.to_string_lossy().into_owned();
        let job = job(
            42,
            vec![process(
                42,
                "MainThread",
                &[&argv0, "--use-system-ca", "/tmp/index.js"],
            )],
        );

        assert_eq!(identify(&job).as_deref(), Some("cursor"));
    }

    // The Hermes installer's Python launcher. The tests below follow herdr's
    // src/detect/mod.rs@5608de0608 (Apache-2.0), e35f3937b0 and 43b36b68ca. Modifications: a
    // fixture's process becomes taw's `Process` (its `cmdline` is the `ps` line), `identify` takes
    // the place of `identify_agent_in_job`, and the agent is `hermes` as an id. The two fixtures
    // under tests/fixtures/ are herdr's, byte for byte: the reporter's capture from #4910 (the
    // short bootstrap) and the capture from #5068 (the longer one).
    fn hermes_installer_capture_from(capture: &str) -> Job {
        let capture: serde_json::Value = serde_json::from_str(capture).unwrap();
        let info = &capture["result"]["process_info"];
        Job {
            group: info["foreground_process_group_id"].as_u64().unwrap() as u32,
            processes: info["foreground_processes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|process| Process {
                    pid: process["pid"].as_u64().unwrap() as u32,
                    name: process["name"].as_str().unwrap().to_string(),
                    argv: Some(serde_json::from_value(process["argv"].clone()).unwrap()),
                    line: Some(process["cmdline"].as_str().unwrap().to_string()),
                })
                .collect(),
        }
    }

    fn hermes_installer_capture() -> Job {
        hermes_installer_capture_from(include_str!(
            "../../tests/fixtures/hermes-installer-process-info-4910.json"
        ))
    }

    fn hermes_newer_installer_capture() -> Job {
        hermes_installer_capture_from(include_str!(
            "../../tests/fixtures/hermes-installer-process-info-5068.json"
        ))
    }

    #[test]
    fn the_hermes_installer_capture_is_hermes() {
        let mut job = hermes_installer_capture();
        assert_eq!(identify(&job).as_deref(), Some("hermes"));
        // The group's leader alone is enough, as it is where the pane's own command is read.
        job.processes.retain(|process| process.pid == job.group);
        assert_eq!(identify(&job).as_deref(), Some("hermes"));
    }

    #[test]
    fn the_hermes_installer_signature_allows_another_install_root() {
        let job = hermes_installer_capture();
        let mut argv = job.processes.last().unwrap().argv.clone().unwrap();
        argv[3] = argv[3].replace(
            "/Users/REDACTED/.hermes/hermes-agent",
            "/opt/another install/hermes-agent",
        );
        assert_eq!(hermes_installer_agent_name(&argv), Some("hermes".into()));
    }

    #[test]
    fn the_hermes_installer_helper_modes_are_no_hermes() {
        for mode in ["--run-module", "--print-runtime-command"] {
            let mut job = hermes_installer_capture();
            let argv = job.processes.last_mut().unwrap().argv.as_mut().unwrap();
            argv.truncate(4);
            argv.extend([mode.into(), "tui_gateway.entry".into()]);
            assert_eq!(identify(&job), None, "{mode}");
        }
    }

    #[test]
    fn the_hermes_installer_signature_rejects_incidental_source_and_unsafe_roots() {
        let job = hermes_installer_capture();
        let mut argv = job.processes.last().unwrap().argv.clone().unwrap();
        let original = argv[3].clone();
        for code in [
            "print('hermes_cli.main')".to_string(),
            format!("'''{original}'''"),
            original.lines().map(|line| format!("# {line}\n")).collect(),
            original.replace("/Users/REDACTED", "/Users/has'quote"),
            original.replace("/Users/REDACTED", r"/Users/has\escape"),
            original.replacen("/Users/REDACTED", "/different/root", 1),
        ] {
            argv[3] = code;
            assert_eq!(hermes_installer_agent_name(&argv), None);
        }
    }

    #[test]
    fn hermes_source_outside_the_installer_invocation_is_no_hermes() {
        for prefix in [
            vec!["python3", "-m", "module"],
            vec!["python3", "--", "script.py"],
            vec!["python3", "script.py"],
            vec!["bash", "-I"],
        ] {
            let mut job = hermes_installer_capture();
            let process = job.processes.last_mut().unwrap();
            let argv = process.argv.as_mut().unwrap();
            let source = argv[3].clone();
            *argv = prefix.iter().map(|arg| (*arg).to_string()).collect();
            argv.extend(["-c".into(), source]);
            process.name = prefix[0].into();
            process.line = Some(argv.join(" "));
            assert_eq!(identify(&job), None, "{prefix:?}");
        }
    }

    #[test]
    fn the_newer_hermes_installer_capture_is_hermes() {
        let mut job = hermes_newer_installer_capture();
        assert_eq!(identify(&job).as_deref(), Some("hermes"));
        let process = job.processes.last_mut().unwrap();
        let argv = process.argv.as_mut().unwrap();
        argv[3] = argv[3]
            .replace(
                "/Users/username/.hermes/hermes-agent",
                "/opt/another install/hermes-agent",
            )
            .replace(
                "import contextlib, hashlib, io, pathlib, shutil, subprocess",
                "import contextlib, hashlib, io, pathlib, shutil, subprocess, warnings",
            );
        argv.push("--resume".into());
        argv.push("session-id".into());
        assert_eq!(identify(&job).as_deref(), Some("hermes"));
    }

    #[test]
    fn the_newer_hermes_installer_rejects_non_entry_source_and_helper_modes() {
        let job = hermes_newer_installer_capture();
        let mut argv = job.processes.last().unwrap().argv.clone().unwrap();
        let original = argv[3].clone();
        for code in [
            "print('hermes_cli.main')".to_string(),
            format!("'''{original}'''"),
            original.lines().map(|line| format!("# {line}\n")).collect(),
            original.replace(
                "from hermes_cli.main import main",
                "from unrelated.main import main",
            ),
            original.replace("sys.exit(main())", "sys.exit(0)"),
            original.replace("import hermes_bootstrap", "import unrelated_bootstrap"),
            original.replace("/Users/username", r"/Users/has\escape"),
            original.replace("/Users/username", "/Users/has'quote"),
        ] {
            argv[3] = code;
            assert_eq!(hermes_installer_agent_name(&argv), None);
        }
        argv[3] = original;
        for mode in ["--run-module", "--print-runtime-command"] {
            argv.truncate(4);
            argv.push(mode.into());
            assert_eq!(hermes_installer_agent_name(&argv), None);
        }
        argv.truncate(4);
        argv[1] = "-m".into();
        assert_eq!(hermes_installer_agent_name(&argv), None);
    }

    // Written for taw: a process whose arguments could not be read exactly has only the line `ps`
    // printed, split at white space, which cuts the bootstrap into words. It is no Hermes, and it
    // is no panic: the launcher's source needs its exact argument list.
    #[test]
    fn a_hermes_installer_known_only_by_its_ps_line_is_not_taken_for_hermes() {
        let mut job = hermes_installer_capture();
        let process = job.processes.last_mut().unwrap();
        process.argv = None;
        assert_eq!(identify(&job), None);
        job.processes.retain(|process| process.pid == job.group);
        assert_eq!(identify(&job), None);
    }
}
