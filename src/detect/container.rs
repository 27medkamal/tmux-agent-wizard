//! An agent that runs in a container started from a pane.
//!
//! `docker run ... claude` leaves the container runtime in front of the pane, so tmux names
//! `docker`, and a name that is no agent's finds no agent. The agent is in the runtime's
//! arguments. This reads them: which word is the program the container runs, and not a
//! mount, a container's name, the value of a flag or an argument that only says the word. It
//! is the one place identity looks past the name of the foreground command.
//! `watch::container` takes the arguments off the process, once per process, and hands them
//! here in one of two forms, each with an entry point of its own:
//!
//! * [`identify_argv`] takes the exact list, one string for each argument, as the process
//!   has it. The words are the real ones, whatever they hold: a value with a space or a `;`
//!   in it is one word, and only the script a shell is given with `-c` is read as shell
//!   text.
//! * [`identify_ps`] takes the line `ps -o args=` prints, for when the list cannot be had.
//!   The arguments are joined by single spaces, so a value with a space shifts every word
//!   after it, an empty argument is gone and a newline is printed as `\012`. The words are
//!   the line split on white space, and what follows a shell's `-c` is joined again and read
//!   as the script it was. It reads a picture of the arguments, and is no better than that.
//!
//! The two are never run one after the other over the same process: what the exact list
//! says is not a command (`bash -lc 'cat /usr/bin/claude'`) would be one to the other.
//!
//! Each rule is a mistake this refuses to make:
//!
//! * Only `run` and `exec` start a program. `docker logs claude` and
//!   `docker build -t claude .` name no agent.
//! * The agent is the program the container runs: the first word after the image (after the
//!   container's name for `exec`), or the value of `--entrypoint`. It is never the image
//!   (`docker run -it claude` runs an image), a container's or a service's name, the value of
//!   a flag (`--name claude`, `--hostname claude`, `-e AGENT=claude`, `-v ~/.claude:/root/.claude`),
//!   part of a path, or an argument of another program (`echo claude`,
//!   `npm install claude-code`). `sbx run claude` has no image, and the word after `run` is
//!   the agent, as it is after the first word `sandbox` of `docker sandbox run claude`.
//! * An entrypoint runs itself, and what follows the image is its arguments:
//!   `--entrypoint which img claude` runs `which`. Only one that hands them on is followed
//!   by a command: a word that only prefixes one (below), a shell, or a script by its name
//!   (one that ends in `.sh` or holds `entrypoint`).
//! * The image is found by skipping the options before it and the values of those that take
//!   one ([`VALUE_OPTIONS`]). An option that list lacks makes its value the image and the
//!   real image the program: that hides an agent, or shows one only where the image is
//!   called like it.
//! * A word is the agent when the command rule would take it for one
//!   ([`super::identity::identify`]): the last component of a path, `claude-code`,
//!   `cursor-agent`, and never `.claude`.
//! * What a shell runs with `-c` counts by its commands: the first word of each simple
//!   command, at the start of the script and after `;`, `&&`, `||`, `|`, `&`, a parenthesis,
//!   a newline, `then`, `do`, `else` and `{`. `echo "claude"` is not Claude. A shell's own
//!   words are read the way it reads them: options (`-o`, `-O`, `--rcfile` and `--init-file`
//!   take a value), then the script, and what follows the script is its `$0` and `$1`, not
//!   commands. `bash script.sh -c claude` runs a file. The script is found wherever its
//!   shell and its `-c` stand after the verb, so an option the list lacks cannot hide it.
//! * A comment is not a command, and what is in it is not shell syntax: `# it's optional;
//!   claude` runs nothing. A backslash before a newline joins the two lines, so
//!   `true && \` and `claude` on the next line run Claude.
//! * A command substitution, `$( )` or a pair of backticks, is commands of its own in the
//!   middle of a word, and the word goes on after it: `echo $(date) claude` runs `echo`.
//! * A word that only prefixes a command passes the position on: variable assignments, and
//!   `exec`, `env`, `command`, `nohup`, `nice`, `time`, `sudo`, `tini`, `dumb-init`,
//!   `runuser` with the options they take, `gosu` and `su-exec` with their user, and
//!   `mise x` and `mise exec` with their tools up to `--`. So
//!   `tini -- sudo -u node env FOO=1 claude` runs Claude and `sudo -u claude nginx` does
//!   not. `command -v claude` (and `-pv`) looks a program up, it does not run it. `npx` is
//!   not one: its argument is a package, not the agent.
//!
//! The arguments can carry credentials (`-e TOKEN=...`), so nothing here keeps them. They
//! are searched, and what comes out is one agent id.

use super::identity::{self, basename, normalize};

/// The programs that start a process in a container, as tmux names them, and what each is
/// recorded as. A pane whose foreground command is one of them may be running an agent that
/// is not the foreground process itself.
pub(crate) const RUNTIMES: &[(&str, &str)] = &[
    ("docker", "docker"),
    // What Docker Desktop for Mac may call its CLI.
    ("com.docker.cli", "docker"),
    ("podman", "podman"),
    ("nerdctl", "nerdctl"),
    ("container", "container"),
    ("sbx", "sbx"),
];

/// The shells that run a script given with `-c`.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ash", "ksh", "fish"];

/// A word that only prefixes the command it is given: the position of the program passes on
/// to what follows it, and what it takes on its own.
struct Prefix {
    name: &'static str,
    shape: Shape,
}

/// What a prefix takes before the command.
enum Shape {
    /// Nothing: a shell keyword.
    Keyword,
    /// Options of its own, and the ones among them that take the next word as their value:
    /// `sudo -u node -E claude`.
    Options(&'static [&'static str]),
    /// So many words that are no options, a user's name: `gosu node claude`.
    Words(usize),
    /// One of these subcommands, then everything up to a bare `--`, the tools it is given
    /// and its options: `mise x node@22 -- claude`. Without the `--` there is no command,
    /// and without one of the subcommands the word is a program like any other.
    Subcommand(&'static [&'static str]),
}

const PREFIXES: &[Prefix] = &[
    Prefix {
        name: "exec",
        shape: Shape::Options(&["-a"]),
    },
    Prefix {
        name: "env",
        shape: Shape::Options(&["-u", "--unset", "-C", "--chdir", "-S", "--split-string"]),
    },
    Prefix {
        name: "command",
        shape: Shape::Options(&[]),
    },
    Prefix {
        name: "nohup",
        shape: Shape::Options(&[]),
    },
    Prefix {
        name: "nice",
        shape: Shape::Options(&["-n", "--adjustment"]),
    },
    Prefix {
        name: "time",
        shape: Shape::Options(&["-f", "--format", "-o", "--output"]),
    },
    Prefix {
        name: "sudo",
        shape: Shape::Options(&[
            "-u",
            "--user",
            "-g",
            "--group",
            "-C",
            "-h",
            "--host",
            "-p",
            "--prompt",
            "-r",
            "--role",
            "-t",
            "--type",
            "-T",
            "-D",
            "--chdir",
            "-R",
            "--chroot",
            "-U",
            "--other-user",
        ]),
    },
    Prefix {
        name: "tini",
        shape: Shape::Options(&["-p", "-e", "-o"]),
    },
    Prefix {
        name: "dumb-init",
        shape: Shape::Options(&["-r", "--rewrite"]),
    },
    // Switching to the user the agent runs as.
    Prefix {
        name: "gosu",
        shape: Shape::Words(1),
    },
    Prefix {
        name: "su-exec",
        shape: Shape::Words(1),
    },
    Prefix {
        name: "runuser",
        shape: Shape::Options(&["-u", "--user", "-g", "--group", "-G", "--supp-group"]),
    },
    // `mise x` and `mise exec` run a command with the tools they are given.
    Prefix {
        name: "mise",
        shape: Shape::Subcommand(&["x", "exec"]),
    },
    // What a shell puts in front of a command inside a script.
    Prefix {
        name: "!",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "{",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "if",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "then",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "else",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "elif",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "do",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "while",
        shape: Shape::Keyword,
    },
    Prefix {
        name: "until",
        shape: Shape::Keyword,
    },
];

/// The options of `run` and `exec` that take their value as the next word, and hold a name, a
/// path or a setting, never the program: Docker's, and the ones Podman, nerdctl and Apple's
/// `container` add to them. `--entrypoint` is not among them, its value is the program the
/// container runs and [`launch`] reads it.
const VALUE_OPTIONS: &[&str] = &[
    "-a",
    "-c",
    "-e",
    "-h",
    "-l",
    "-m",
    "-p",
    "-u",
    "-v",
    "-w",
    "-H",
    "--add-host",
    "--annotation",
    "--arch",
    "--attach",
    "--authfile",
    "--blkio-weight",
    "--blkio-weight-device",
    "--cap-add",
    "--cap-drop",
    "--cgroup-conf",
    "--cgroup-parent",
    "--cgroupns",
    "--cgroups",
    "--cidfile",
    "--config",
    "--context",
    "--cpu-period",
    "--cpu-quota",
    "--cpu-rt-period",
    "--cpu-rt-runtime",
    "--cpu-shares",
    "--cpus",
    "--cpuset-cpus",
    "--cpuset-mems",
    "--detach-keys",
    "--device",
    "--device-cgroup-rule",
    "--device-read-bps",
    "--device-read-iops",
    "--device-write-bps",
    "--device-write-iops",
    "--dns",
    "--dns-option",
    "--dns-search",
    "--domainname",
    "--env",
    "--env-file",
    "--expose",
    "--gidmap",
    "--gpus",
    "--group-add",
    "--health-cmd",
    "--health-interval",
    "--health-retries",
    "--health-start-interval",
    "--health-start-period",
    "--health-timeout",
    "--host",
    "--hostname",
    "--hostuser",
    "--init-path",
    "--ip",
    "--ip6",
    "--ipc",
    "--isolation",
    "--kernel-memory",
    "--label",
    "--label-file",
    "--link",
    "--link-local-ip",
    "--log-driver",
    "--log-level",
    "--log-opt",
    "--mac-address",
    "--memory",
    "--memory-reservation",
    "--memory-swap",
    "--memory-swappiness",
    "--mount",
    "--name",
    "--net",
    "--network",
    "--network-alias",
    "--oom-score-adj",
    "--os",
    "--pid",
    "--pids-limit",
    "--platform",
    "--pod",
    "--pod-id-file",
    "--publish",
    "--pull",
    "--restart",
    "--runtime",
    "--sdnotify",
    "--secret",
    "--security-opt",
    "--shm-size",
    "--stop-signal",
    "--stop-timeout",
    "--storage-opt",
    "--sysctl",
    "--systemd",
    "--tmpfs",
    "--tz",
    "--uidmap",
    "--ulimit",
    "--umask",
    "--unsetenv",
    "--user",
    "--userns",
    "--uts",
    "--variant",
    "--volume",
    "--volume-driver",
    "--volumes-from",
    "--workdir",
];

/// The short flags that take no value, and the ones that do. A cluster such as `-itv` is
/// flags of the first kind ending in one of the second, and takes the next word as `-v` does.
const SHORT_FLAGS: &str = "itdPq";
const SHORT_VALUE_FLAGS: &str = "acehlmpuvw";

/// An agent found in the arguments of a container runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerAgent {
    /// The runtime in front of the pane, as it is recorded: `docker`, `podman`, `nerdctl`,
    /// `container` or `sbx`.
    pub runtime: &'static str,
    /// The agent's id, as `detect::identify` names it.
    pub kind: String,
}

impl ContainerAgent {
    /// How the identity was found, as recorded: `container:<runtime>`.
    pub fn source(&self) -> String {
        format!("container:{}", self.runtime)
    }
}

/// The container runtime a pane's foreground command is, if it is one, as it is recorded.
/// Named the way the command rule names a program: the last component of a path, in lower
/// case, without a launcher extension.
pub fn runtime(command: &str) -> Option<&'static str> {
    let name = normalize(basename(command));
    RUNTIMES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, recorded)| *recorded)
}

/// The agent the container runtime `command` starts, given the exact arguments of its
/// process (`argv[0]` is the runtime), or `None` when `command` is no runtime or they start
/// none.
pub fn identify_argv(command: &str, argv: &[String]) -> Option<ContainerAgent> {
    let runtime = runtime(command)?;
    let kind = agent_in(runtime, argv, Form::Exact)?;
    Some(ContainerAgent { runtime, kind })
}

/// [`identify_argv`] for a line as `ps -ww -o args=` prints it, when the exact list could
/// not be had. Read as words (so a quote or a `;` in a flag's value is only a character),
/// and only what follows a shell's `-c` as the script it is. A control character in an
/// argument is printed as its octal escape, and a newline in a script is a separator of
/// commands.
pub fn identify_ps(command: &str, line: &str) -> Option<ContainerAgent> {
    let runtime = runtime(command)?;
    let words: Vec<String> = unescape_ps(line)
        .split([' ', '\t', '\r'])
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    let kind = agent_in(runtime, &words, Form::Printed)?;
    Some(ContainerAgent { runtime, kind })
}

/// What `ps` prints for a control character in an argument: macOS writes the octal escape
/// of a newline (`\012`) and of a tab (`\011`), `^M` for a carriage return, and other
/// systems the character or a `?`. A newline stays in the word it ends, where the script it
/// belongs to reads it as the separator of commands it is. A carriage return right before
/// one goes, since CR LF is one line end (and a backslash before the pair joins the lines);
/// any other is white space.
fn unescape_ps(line: &str) -> String {
    line.replace("^M\\012", "\\012")
        .replace("\\015\\012", "\\012")
        .replace("\\012", "\n")
        .replace("\\015", " ")
        .replace("\\011", " ")
        .replace("^M", " ")
}

/// How the words of the runtime's command came.
#[derive(Clone, Copy)]
enum Form {
    /// The process's own arguments: the script of a shell is the one word after its `-c`.
    Exact,
    /// Printed by `ps`, the arguments joined by spaces: the script is the words after the
    /// `-c`, put back together.
    Printed,
}

/// What one command came to.
enum Scan<'a> {
    Agent(String),
    /// A shell in it was given a script with `-c`: the words from the script on.
    Script(&'a [String]),
    Nothing,
}

fn agent_in(runtime: &str, words: &[String], form: Form) -> Option<String> {
    let launch = launch(runtime, words)?;
    // The entrypoint is the program the container runs, and what follows the image its
    // arguments. Only one that hands them on is followed by a command of its own.
    let with_entrypoint: Vec<String>;
    let mut programs: Vec<&[String]> = Vec::new();
    match launch.entrypoint {
        Some(entrypoint) => {
            with_entrypoint = std::iter::once(entrypoint.to_string())
                .chain(launch.command.iter().cloned())
                .collect();
            programs.push(&with_entrypoint);
            if looks_like_a_script(entrypoint) {
                programs.push(launch.command);
            }
        }
        None => programs.push(launch.command),
    }
    for words in programs {
        match scan(words) {
            Scan::Agent(kind) => return Some(kind),
            Scan::Script(script) => return script_agent(script, form),
            Scan::Nothing => {}
        }
    }
    // An option the list lacks moves where the image is taken to be, and with it the
    // program, but not where a shell and its `-c` stand: a script is read wherever it is.
    script_agent(script_in(launch.after_verb)?, form)
}

/// Whether an entrypoint is a script that will `exec "$@"`, judging by its name: it ends in
/// `.sh` or holds `entrypoint`, in any case.
fn looks_like_a_script(entrypoint: &str) -> bool {
    let name = entrypoint.to_lowercase();
    name.ends_with(".sh") || name.contains("entrypoint")
}

#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Run,
    Exec,
}

/// What a runtime's command line starts.
struct Launch<'a> {
    /// The value of `--entrypoint`, the program the container runs in place of the image's
    /// own.
    entrypoint: Option<&'a str>,
    /// What follows the image (the container, for `exec`): a program and its arguments.
    command: &'a [String],
    /// Every word after the verb, the image and the command with them.
    after_verb: &'a [String],
}

/// The words of the command that runs the runtime, read down to the image: options and their
/// values, a verb, more options, then the image. What follows it is the container's command.
fn launch<'a>(runtime: &str, words: &'a [String]) -> Option<Launch<'a>> {
    // `sbx run claude` and `docker sandbox run claude` have no image: the word after `run`
    // is the agent.
    let mut agent_first = runtime == "sbx";
    let mut verb = None;
    let mut after_verb = words.len();
    let mut entrypoint = None;
    // Whether no word that is no option has been seen yet: `sandbox` is the subcommand
    // only as the first, and `docker compose --profile sandbox run app claude` has it as a
    // profile's name.
    let mut first = true;
    // `words[0]` is the runtime itself.
    let mut next = 1;
    while let Some(word) = words.get(next) {
        next += 1;
        if is_option(word) {
            if verb.is_some() {
                if word == "--entrypoint" {
                    entrypoint = words.get(next).map(String::as_str);
                    next += 1;
                } else if let Some(value) = word.strip_prefix("--entrypoint=") {
                    entrypoint = Some(value);
                }
            }
            if takes_a_word(word) {
                next += 1;
            }
            continue;
        }
        match verb {
            None => {
                match word.as_str() {
                    "run" | "exec" => {
                        verb = Some(if word == "run" { Verb::Run } else { Verb::Exec });
                        after_verb = next;
                    }
                    "sandbox" if first => agent_first = true,
                    _ => {}
                }
                first = false;
            }
            Some(verb) => {
                let from = if agent_first && verb == Verb::Run {
                    next - 1
                } else {
                    next
                };
                return Some(Launch {
                    entrypoint: entrypoint.filter(|value| !value.is_empty()),
                    command: &words[from..],
                    after_verb: &words[after_verb..],
                });
            }
        }
    }
    // A verb and no image: nothing is run, but a script may still be in the options.
    verb.map(|_| Launch {
        entrypoint: entrypoint.filter(|value| !value.is_empty()),
        command: &[],
        after_verb: &words[after_verb..],
    })
}

/// The script of the first shell among `words` that is given one with `-c`, wherever it
/// stands. A shell that is the value of `--entrypoint` has the image between it and its own
/// words, and one whose words hold a file before the `-c` runs that file.
fn script_in(words: &[String]) -> Option<&[String]> {
    let mut next = 0;
    while let Some(word) = words.get(next) {
        next += 1;
        if is_option(word) {
            let entrypoint = if word == "--entrypoint" {
                next += 1;
                words.get(next - 1).map(String::as_str)
            } else {
                word.strip_prefix("--entrypoint=")
            };
            if entrypoint.is_some_and(is_shell)
                && let Some(script) = words
                    .get(next..)
                    .and_then(|rest| shell_script(after_the_image(rest)))
            {
                return Some(script);
            }
            if takes_a_word(word) {
                next += 1;
            }
        } else if is_shell(word)
            && let Some(script) = shell_script(&words[next..])
        {
            return Some(script);
        }
    }
    None
}

/// What follows the first word among `words` that is no option and no option's value.
fn after_the_image(words: &[String]) -> &[String] {
    let mut next = 0;
    while let Some(word) = words.get(next) {
        next += 1;
        if !is_option(word) {
            return &words[next..];
        }
        if takes_a_word(word) {
            next += 1;
        }
    }
    &[]
}

fn is_option(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-')
}

/// Whether the option `word` takes the next word as its value.
fn takes_a_word(option: &str) -> bool {
    VALUE_OPTIONS.contains(&option)
        || option.strip_prefix('-').is_some_and(|flags| {
            flags.len() > 1
                && flags.is_ascii()
                && flags[..flags.len() - 1]
                    .chars()
                    .all(|flag| SHORT_FLAGS.contains(flag))
                && SHORT_VALUE_FLAGS.contains(&flags[flags.len() - 1..])
        })
}

/// The agent a script starts, given the words from the script on: the script and, after it,
/// what it is given as `$0` and `$1`. Exact, the script is the first word; printed by `ps`,
/// it is all of them, put back together.
fn script_agent(from_script: &[String], form: Form) -> Option<String> {
    let joined;
    let text = match form {
        Form::Exact => from_script.first()?.as_str(),
        Form::Printed => {
            joined = from_script.join(" ");
            &joined
        }
    };
    in_script(&commands(text))
}

/// The agent one of a script's commands starts. What the script's own shell reads, the words
/// its quotes were taken off, is an exact list again.
fn in_script(commands: &[Vec<String>]) -> Option<String> {
    commands.iter().find_map(|words| match scan(words) {
        Scan::Agent(kind) => Some(kind),
        Scan::Script(script) => script_agent(script, Form::Exact),
        Scan::Nothing => None,
    })
}

/// Where the program of a command stands: past any variables, and the words that only prefix
/// it with the options they take. `None` when there is none, or when the prefix only looks
/// the program up (`command -v claude`).
fn program_at(words: &[String]) -> Option<usize> {
    let mut at = 0;
    while let Some(word) = words.get(at) {
        if is_assignment(word) {
            at += 1;
            continue;
        }
        let name = normalize(basename(word));
        let Some(prefix) = PREFIXES.iter().find(|prefix| prefix.name == name) else {
            return Some(at);
        };
        at += 1;
        match prefix.shape {
            Shape::Keyword => {}
            // Its options end at the first word that is none, and a bare `--`
            // (`tini -- claude`) is one of them.
            Shape::Options(valued) => {
                while let Some(option) = words.get(at).filter(|word| is_option(word)) {
                    at += 1;
                    // `command -v claude` asks where Claude is, it does not run it.
                    if name == "command" && is_lookup(option) {
                        return None;
                    }
                    if valued.contains(&option.as_str()) {
                        at += 1;
                    }
                }
            }
            Shape::Words(count) => {
                for _ in 0..count {
                    if words.get(at).is_none_or(|word| is_option(word)) {
                        return None;
                    }
                    at += 1;
                }
            }
            Shape::Subcommand(subcommands) => {
                if !words
                    .get(at)
                    .is_some_and(|word| subcommands.contains(&word.as_str()))
                {
                    // `mise install claude`: `mise` is the program.
                    return Some(at - 1);
                }
                at += 1;
                let dashes = words[at..].iter().position(|word| word == "--")?;
                at += dashes + 1;
            }
        }
    }
    None
}

/// What a command starts, read from its program.
fn scan(words: &[String]) -> Scan<'_> {
    let Some(at) = program_at(words) else {
        return Scan::Nothing;
    };
    let program = &words[at];
    if let Some(kind) = identity::identify(program, None) {
        return Scan::Agent(kind);
    }
    if is_shell(program)
        && let Some(script) = shell_script(&words[at + 1..])
    {
        return Scan::Script(script);
    }
    Scan::Nothing
}

fn is_shell(word: &str) -> bool {
    SHELLS.contains(&normalize(basename(word)).as_str())
}

/// The words from the script a shell is given with `-c`, `args` being what follows the
/// shell's own name. The shell reads its options first: a cluster of short flags such as
/// `-lc`, of which `c` says the next word that is no option is a script, `o` and `O` that a
/// name follows (`-o pipefail`), and `--rcfile` and `--init-file` that a file does. The first
/// word that is none of them is the script, or, when there was no `-c`, the file to run, and
/// what follows it is its arguments.
fn shell_script(args: &[String]) -> Option<&[String]> {
    let mut command = false;
    let mut next = 0;
    while let Some(word) = args.get(next) {
        if word == "--" {
            next += 1;
            break;
        }
        let Some(flags) = word
            .strip_prefix(['-', '+'])
            .filter(|flags| !flags.is_empty())
        else {
            break;
        };
        next += 1;
        if let Some(long) = flags.strip_prefix('-') {
            if matches!(long, "rcfile" | "init-file") {
                next += 1;
            }
            continue;
        }
        for (at, flag) in flags.char_indices() {
            match flag {
                'c' => command = true,
                // The rest of the cluster is the name, or when there is none the next word.
                'o' | 'O' => {
                    if flags[at + 1..].is_empty() {
                        next += 1;
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    args.get(next..).filter(|rest| command && !rest.is_empty())
}

/// An option that asks where a program is: `-v` and `-V`, alone or in a cluster (`-pv`).
fn is_lookup(option: &str) -> bool {
    option.strip_prefix('-').is_some_and(|flags| {
        flags.chars().all(|flag| flag.is_ascii_alphabetic()) && flags.contains(['v', 'V'])
    })
}

/// `NAME=value`, the variable a command line may set for the program it runs.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// A shell line, read the way a shell reads it and no more strictly: its simple commands,
/// each as its words with the quotes taken off. A quote hides what is inside it, spaces and
/// separators too, and one that never closes runs to the end. Commands end at `;`, `&`, `|`,
/// a newline and a parenthesis. A word that begins with `#` is a comment and runs to the end
/// of the line, and a backslash before a newline joins the line to the next. A command
/// substitution, `$( )` or a pair of backticks, is a list of commands of its own in the
/// middle of a word: they come first, and the word goes on after it, with `$()` where the
/// substitution stood.
fn commands(text: &str) -> Vec<Vec<String>> {
    /// A command as far as it has been read.
    #[derive(Default)]
    struct Command {
        words: Vec<String>,
        word: String,
        /// A word has begun, so a quoted nothing (`''`) is a word too.
        started: bool,
    }

    /// What is open around the command being read.
    #[derive(PartialEq)]
    enum Open {
        /// A parenthesis: its commands are commands of their own, and what follows the
        /// closing one starts afresh.
        Group,
        /// `$( )`.
        Substitution,
        /// A pair of backticks.
        Backticks,
    }

    #[derive(Default)]
    struct Lexer {
        done: Vec<Vec<String>>,
        current: Command,
        /// What is open, with the command that was being read when it opened.
        open: Vec<(Open, Command)>,
    }

    impl Lexer {
        fn end_word(&mut self) {
            let command = &mut self.current;
            if command.started {
                command.words.push(std::mem::take(&mut command.word));
                command.started = false;
            }
        }

        fn end_command(&mut self) {
            self.end_word();
            if !self.current.words.is_empty() {
                self.done.push(std::mem::take(&mut self.current.words));
            }
        }

        fn open(&mut self, kind: Open) {
            if kind == Open::Group {
                self.end_command();
            }
            let outer = std::mem::take(&mut self.current);
            self.open.push((kind, outer));
        }

        /// Ends what was opened last: the command inside is done, and the one around it
        /// goes on, with a word for what was substituted.
        fn close(&mut self) {
            self.end_command();
            if let Some((kind, outer)) = self.open.pop() {
                self.current = outer;
                if kind != Open::Group {
                    self.current.word.push_str("$()");
                    self.current.started = true;
                }
            }
        }

        fn is_open(&self, kind: Open) -> bool {
            self.open.last().is_some_and(|(open, _)| *open == kind)
        }
    }

    let mut lexer = Lexer::default();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                lexer.current.started = true;
                for quoted in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    lexer.current.word.push(quoted);
                }
            }
            '"' => {
                lexer.current.started = true;
                while let Some(quoted) = chars.next() {
                    match quoted {
                        '"' => break,
                        // Inside double quotes a backslash before a newline joins the lines,
                        '\\' if chars.peek() == Some(&'\n') => {
                            chars.next();
                        }
                        // and otherwise only escapes these four.
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            lexer.current.word.extend(chars.next());
                        }
                        _ => lexer.current.word.push(quoted),
                    }
                }
            }
            '\\' => match chars.peek() {
                // A backslash before a newline joins two lines: both go, and the word or the
                // command around them goes on. A carriage return in between goes too, for a
                // script whose lines end in CR LF.
                Some('\n') => {
                    chars.next();
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                _ => {
                    if let Some(escaped) = chars.next() {
                        lexer.current.started = true;
                        lexer.current.word.push(escaped);
                    }
                }
            },
            // A comment, where a word could begin, runs to the end of the line: what is in it
            // (a quote, a `;`) is not shell syntax. The newline that ends it still ends the
            // command, and a backslash at its end does not carry it on.
            '#' if !lexer.current.started => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                if chars.peek() == Some(&'(') {
                    // `$(( ))` is arithmetic and runs nothing: on to the parenthesis that
                    // closes it.
                    let mut depth = 1;
                    for inner in chars.by_ref() {
                        match inner {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        if depth == 0 {
                            break;
                        }
                    }
                    lexer.current.word.push_str("$(())");
                    lexer.current.started = true;
                } else {
                    lexer.open(Open::Substitution);
                }
            }
            '`' => {
                if lexer.is_open(Open::Backticks) {
                    lexer.close();
                } else {
                    lexer.open(Open::Backticks);
                }
            }
            '(' => lexer.open(Open::Group),
            // One that closes no group or substitution ends a command, as the one that
            // ends a `case` pattern does.
            ')' => {
                if lexer.is_open(Open::Group) || lexer.is_open(Open::Substitution) {
                    lexer.close();
                } else {
                    lexer.end_command();
                }
            }
            '\n' | ';' | '&' | '|' => lexer.end_command(),
            c if c.is_whitespace() => lexer.end_word(),
            c => {
                lexer.current.started = true;
                lexer.current.word.push(c);
            }
        }
    }
    while !lexer.open.is_empty() {
        lexer.close();
    }
    lexer.end_command();
    lexer.done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::bundled_ids;

    /// A shell function that runs Claude Code in a Docker container, as
    /// it is typed: the shape only, the real command is much longer.
    const BOXED: &str = r#"docker run --rm -it -v /a/project:/a/project -v /h/.claude:/h/.claude -e CLAUDE_CONFIG_DIR=/h/.claude --entrypoint bash dev-image -lc '/usr/local/bin/firewall.sh || true; mise install; eval "$(mise activate bash)"; claude --effort medium --dangerously-skip-permissions "$@"' _"#;

    /// The same process as `ps -ww -o args=` prints it: the arguments joined by spaces, with
    /// the quotes the shell took off gone, so the script is no longer one word.
    const BOXED_FROM_PS: &str = r#"docker run --rm -it -v /a/project:/a/project -v /h/.claude:/h/.claude -e CLAUDE_CONFIG_DIR=/h/.claude --entrypoint bash dev-image -lc /usr/local/bin/firewall.sh || true; mise install; eval "$(mise activate bash)"; claude --effort medium --dangerously-skip-permissions "$@" _"#;

    /// The words a shell makes of a typed line: the argument list of the process it starts.
    fn argv(line: &str) -> Vec<String> {
        shell_words::split(line).unwrap_or_else(|err| panic!("{line}: {err}"))
    }

    fn agent(command: &str, line: &str) -> Option<String> {
        identify_argv(command, &argv(line)).map(|found| found.kind)
    }

    /// What the exact reader makes of a line as it is typed.
    fn docker(line: &str) -> Option<String> {
        agent("docker", line)
    }

    /// The line `ps` prints for a process with these arguments: joined by single spaces, a
    /// newline as `\012`, a tab as `\011` and a carriage return as `^M`, whatever quoting the
    /// shell had taken off.
    fn printed(argv: &[String]) -> String {
        argv.join(" ")
            .replace('\n', "\\012")
            .replace('\t', "\\011")
            .replace('\r', "^M")
    }

    /// What the fallback reader makes of a line as `ps` prints it.
    fn ps(line: &str) -> Option<String> {
        identify_ps("docker", line).map(|found| found.kind)
    }

    /// What the fallback reader makes of the line `ps` prints for a process started by a typed line.
    fn ps_of(line: &str) -> Option<String> {
        ps(&printed(&argv(line)))
    }

    /// What both readers make of one process: the exact list, and the line `ps` prints for it.
    fn both(argv: &[&str]) -> [Option<String>; 2] {
        let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
        [
            identify_argv("docker", &argv).map(|found| found.kind),
            ps(&printed(&argv)),
        ]
    }

    /// `argv` for a container whose shell is given `script`, with the argument after it a
    /// script's `$0`.
    fn script_argv(script: &str) -> Vec<&str> {
        vec![
            "docker", "run", "--rm", "-it", "img", "bash", "-lc", script, "_",
        ]
    }

    #[test]
    fn a_boxed_claude_reads_as_claude() {
        assert_eq!(docker(BOXED).as_deref(), Some("claude"));
        assert_eq!(ps(BOXED_FROM_PS).as_deref(), Some("claude"));
        // `ps` prints the exact list, and only that, so one is the other's picture.
        assert_eq!(printed(&argv(BOXED)), BOXED_FROM_PS);
        let found = identify_argv("docker", &argv(BOXED)).unwrap();
        assert_eq!((found.runtime, found.kind.as_str()), ("docker", "claude"));
        assert_eq!(found.source(), "container:docker");
        let found = identify_ps("docker", BOXED_FROM_PS).unwrap();
        assert_eq!(found.source(), "container:docker");
    }

    #[test]
    fn podman_running_codex_reads_codex() {
        assert_eq!(
            agent(
                "podman",
                "podman run --rm -it -v /src:/src ghcr.io/acme/dev-image codex --model o3"
            )
            .as_deref(),
            Some("codex")
        );
        // The image, tagged or not, is never the agent: the word after it is.
        assert_eq!(
            agent("podman", "podman run -it acme/dev-image:latest codex").as_deref(),
            Some("codex")
        );
        let found = identify_argv("podman", &argv("podman run -it dev codex")).unwrap();
        assert_eq!(found.source(), "container:podman");
    }

    #[test]
    fn sbx_and_docker_sandbox_read_the_agent_they_are_told_to_run() {
        assert_eq!(agent("sbx", "sbx run claude").as_deref(), Some("claude"));
        assert_eq!(
            agent("sbx", "sbx run claude /home/me/project").as_deref(),
            Some("claude")
        );
        assert_eq!(
            agent("sbx", "sbx run --name review codex .").as_deref(),
            Some("codex")
        );
        assert_eq!(
            docker("docker sandbox run claude").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn exec_and_compose_read_the_command_they_run() {
        assert_eq!(
            docker("docker exec -it web claude --resume").as_deref(),
            Some("claude")
        );
        assert_eq!(
            docker("docker exec -it -w /work web sh -c 'exec codex'").as_deref(),
            Some("codex")
        );
        assert_eq!(
            docker("docker compose run --rm dev claude").as_deref(),
            Some("claude")
        );
        assert_eq!(
            docker("docker compose -f dev.yml exec -T dev bash -lc claude").as_deref(),
            Some("claude")
        );
        assert_eq!(
            docker("docker container run -it img gemini").as_deref(),
            Some("gemini")
        );
    }

    #[test]
    fn nerdctl_and_apples_container_read_the_same_way() {
        assert_eq!(
            agent("nerdctl", "nerdctl run -it --rm img gemini").as_deref(),
            Some("gemini")
        );
        assert_eq!(
            agent(
                "container",
                "container run -it --rm --volume /a:/a ubuntu bash -lc 'cd /work && opencode'"
            )
            .as_deref(),
            Some("opencode")
        );
    }

    #[test]
    fn an_entrypoint_or_a_path_names_the_agent() {
        for args in [
            "docker run --entrypoint claude img",
            "docker run --entrypoint=claude img",
            "docker run --entrypoint /usr/local/bin/claude img",
            "docker run -it img /usr/bin/claude",
            "docker run -it img /opt/bin/CLAUDE.exe",
            // An entrypoint that is a wrapper is handed the command, and the command is read.
            "docker run --entrypoint /entrypoint.sh img claude --continue",
            // An empty entrypoint is the image's own: the command is the program.
            "docker run --entrypoint '' img claude",
        ] {
            assert_eq!(docker(args).as_deref(), Some("claude"), "{args}");
        }
    }

    #[test]
    fn every_name_the_command_rule_knows_reads_here_too() {
        for id in bundled_ids() {
            assert_eq!(
                docker(&format!("docker run --rm -it img {id} --flag")).as_deref(),
                Some(*id),
                "{id}"
            );
        }
        for (name, id) in [
            ("claude-code", "claude"),
            ("Claude", "claude"),
            ("cursor-agent", "cursor"),
            ("kiro-cli", "kiro"),
            ("antigravity", "agy"),
            (".cline", "cline"),
        ] {
            assert_eq!(
                docker(&format!("docker run img {name}")).as_deref(),
                Some(id),
                "{name}"
            );
        }
    }

    #[test]
    fn a_script_reads_its_command_words() {
        let cases: &[(&str, Option<&str>)] = &[
            ("claude", Some("claude")),
            ("claude --continue", Some("claude")),
            ("cd /work && claude --continue", Some("claude")),
            ("FOO=1 BAR=2 claude", Some("claude")),
            ("exec claude \"$@\"", Some("claude")),
            ("sudo true; nohup /usr/local/bin/codex", Some("codex")),
            ("mise install; time gemini", Some("gemini")),
            ("if true; then claude; fi", Some("claude")),
            ("cd /w && exec claude --resume x", Some("claude")),
            ("while true; do claude; done", Some("claude")),
            ("{ claude; }", Some("claude")),
            ("true || claude", Some("claude")),
            ("true | claude", Some("claude")),
            ("(cd /w && claude)", Some("claude")),
            ("echo $(claude)", Some("claude")),
            ("echo done", None),
        ];
        for (script, expected) in cases {
            assert_eq!(
                both(&script_argv(script)),
                [expected.map(str::to_string), expected.map(str::to_string)],
                "{script}"
            );
        }
    }

    #[test]
    fn words_that_only_prefix_a_command_pass_its_position_on() {
        // What the prefix takes on its own, options and their values, is not the program.
        for args in [
            "docker run img tini -- claude",
            "docker run img tini -g -- /usr/local/bin/claude --continue",
            "docker run img dumb-init claude",
            "docker run img dumb-init --single-child claude",
            "docker run img dumb-init --rewrite 2:3 claude",
            "docker run img env FOO=1 BAR=2 claude",
            "docker run img env -i FOO=1 claude",
            "docker run img env -u PATH -C /work claude",
            "docker run img env --unset PATH claude",
            "docker run img nohup claude",
            "docker run img nice claude",
            "docker run img nice -n 10 claude",
            "docker run img nice -10 claude",
            "docker run img sudo claude",
            "docker run img sudo -u node -E claude",
            "docker run img sudo -u node -- claude",
            "docker run img time claude",
            "docker run img time -p claude",
            "docker run img command claude",
            "docker run img exec -a agent claude",
            "docker run img /usr/bin/env claude",
            "docker run img /sbin/tini -- claude",
            "docker run img tini -- sudo -u node env FOO=1 nice -n 5 nohup claude",
            "docker run --entrypoint tini img -- claude",
            "docker run --entrypoint /usr/bin/env img FOO=1 claude",
            "docker exec -it web sudo -u node claude",
        ] {
            assert_eq!(docker(args).as_deref(), Some("claude"), "{args}");
        }
        for args in [
            // The value of a prefix's option is a name, whatever it is called.
            "docker run img sudo -u claude nginx",
            "docker run img env -u claude nginx",
            "docker run img nice -n claude nginx",
            "docker run img dumb-init --rewrite claude nginx",
            "docker run img env FOO=claude nginx",
            // A prefix with nothing to run, or an argument of what it runs.
            "docker run img sudo",
            "docker run img env",
            "docker run img tini -- nginx claude",
            "docker run img sudo nginx --user claude",
            "docker run img nohup cat /usr/bin/claude",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
    }

    #[test]
    fn a_user_switch_and_a_tool_manager_pass_the_position_on() {
        // What Dockerfiles put in front of the agent: `gosu` and `su-exec` (the user is the
        // word after them), `runuser -u`, and `mise x -- ` with the tools it is given.
        for (args, expected) in [
            ("docker run img gosu node claude", "claude"),
            ("docker run img gosu node:node claude --continue", "claude"),
            ("docker run img su-exec app codex", "codex"),
            ("docker run img runuser -u app -- claude", "claude"),
            ("docker run img runuser -u app -g staff claude", "claude"),
            ("docker run img mise x -- claude", "claude"),
            ("docker run img mise exec node@22 -- codex", "codex"),
            (
                "docker run img mise x node@22 python@3.12 -- claude",
                "claude",
            ),
            ("docker run img mise x -j 4 -- claude", "claude"),
            ("docker run --entrypoint gosu img node claude", "claude"),
            ("docker run img tini -- gosu app mise x -- claude", "claude"),
            ("docker run img gosu app sh -c 'exec claude'", "claude"),
        ] {
            assert_eq!(docker(args).as_deref(), Some(expected), "{args}");
        }
        for (script, expected) in [
            ("mise exec node@22 -- codex", "codex"),
            ("gosu app claude", "claude"),
            ("cd /w && su-exec app codex", "codex"),
        ] {
            let found = Some(expected.to_string());
            assert_eq!(both(&script_argv(script)), [found.clone(), found]);
        }
        for args in [
            // `npx` runs a package, and a package is not the agent.
            "docker run img npx some-tool",
            "docker run img npx @anthropic-ai/claude-code",
            // The user is a name, whatever it is called.
            "docker run img gosu claude nginx",
            "docker run img su-exec claude nginx",
            "docker run img runuser -u claude -- nginx",
            // Arguments of what they run.
            "docker run img gosu node nginx claude",
            "docker run img mise exec node@22 -- nginx claude",
            // Nothing to run.
            "docker run img gosu",
            "docker run img gosu --help claude",
            "docker run img mise exec node@22",
            "docker run img mise x claude",
            // Other things `mise` does with words after a `--`.
            "docker run img mise install claude",
            "docker run img mise run test -- claude",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
    }

    #[test]
    fn a_lookup_is_not_a_run() {
        // `command -v claude` asks whether Claude is installed, and a script that installs
        // it if not runs `npm` for it.
        for script in [
            "command -v claude",
            "command -v claude >/dev/null && echo yes",
            "if ! command -v claude; then npm install -g @anthropic-ai/claude-code; fi",
            "command -V codex",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script}");
        }
    }

    #[test]
    fn command_with_any_option_that_looks_a_program_up_looks_it_up() {
        // `-p` searches a default path and `-v` says where the program is: together, in
        // either order and either case, it is still a lookup.
        for script in [
            "command -pv claude",
            "command -vp claude",
            "command -pV claude",
            "command -p -v claude",
            "command -Vp codex",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script}");
        }
        for script in ["command -p claude", "command -- claude", "command claude"] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script}"
            );
        }
    }

    #[test]
    fn a_shell_inside_the_script_is_read_too() {
        assert_eq!(
            docker("docker run img sh -c \"bash -c 'codex'\"").as_deref(),
            Some("codex")
        );
        assert_eq!(
            docker("docker run img bash --login -ic claude").as_deref(),
            Some("claude")
        );
        assert_eq!(
            docker("docker run --entrypoint /bin/zsh img -c 'claude'").as_deref(),
            Some("claude")
        );
        // What the inner shell is given after its script is its `$0`, not more script.
        assert_eq!(docker("docker run img sh -c \"bash -c '' claude\""), None);
        // As `ps` prints it, the quotes the outer shell needed are gone.
        assert_eq!(
            ps("docker run img sh -c bash -c 'codex'").as_deref(),
            Some("codex")
        );
        assert_eq!(ps("docker run img sh -c bash -c '' claude"), None);
        assert_eq!(
            ps("docker run img bash --login -ic claude").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn plain_container_commands_name_no_agent() {
        for args in [
            "docker run -it nginx",
            "docker run --rm -it ubuntu bash",
            "docker run -d --name web -p 8080:80 nginx:1.27",
            "docker ps",
            "docker ps -a --format {{.Names}}",
            "docker images",
            "docker",
            "docker run",
            "docker exec -it web bash",
            "docker exec -it db psql -U postgres -c 'select 1'",
            "docker logs -f web",
            "docker compose up",
            "docker build -t myapp .",
            "",
            "   ",
        ] {
            assert_eq!(docker(args), None, "{args:?}");
            assert_eq!(ps_of(args), None, "{args:?}");
        }
        assert_eq!(agent("podman", "podman run -d redis"), None);
        assert_eq!(agent("nerdctl", "nerdctl compose up -d"), None);
        assert_eq!(agent("sbx", "sbx ls"), None);
        assert_eq!(agent("container", "container ls"), None);
    }

    #[test]
    fn a_path_or_a_variable_that_names_claude_is_not_the_agent() {
        for args in [
            "docker run --rm -it -v /h/.claude:/h/.claude -e CLAUDE_CONFIG_DIR=/h/.claude nginx",
            "docker run -w /h/.claude nginx",
            "docker run --env-file /h/.claude/env nginx",
            "docker run --mount type=bind,source=/h/.claude,target=/h/.claude nginx",
            "docker run -e CLAUDE_CODE_OAUTH_TOKEN nginx",
            "docker run -v/h/.claude:/h/.claude nginx",
            "docker run --volume=/h/.claude:/h/.claude nginx",
        ] {
            assert_eq!(docker(args), None, "{args}");
            assert_eq!(ps_of(args), None, "{args}");
        }
    }

    #[test]
    fn a_script_is_found_wherever_the_shell_and_its_flag_stand() {
        // A real container command has a much longer list of options than the table names. One it
        // lacks shifts where the image is taken to be, and with it where the program is,
        // but not where the shell and its -c stand: the script is read all the same.
        let args = BOXED.replacen("docker run", "docker run --future-flag value", 1);
        assert_eq!(docker(&args).as_deref(), Some("claude"), "{args}");
        let args = BOXED_FROM_PS.replacen("docker run", "docker run --future-flag value", 1);
        assert_eq!(ps(&args).as_deref(), Some("claude"), "{args}");
        for args in [
            "docker run --future-flag value --entrypoint bash img -lc claude",
            "docker run --future-flag value --entrypoint sh img -c 'exec codex'",
            "docker run --future-flag value img bash -lc claude",
            "docker exec --future-flag value web bash -lc claude",
            "docker run --future-flag value -v /a:/a --entrypoint /bin/zsh img -c claude",
        ] {
            assert!(docker(args).is_some(), "{args}");
            assert!(ps_of(args).is_some(), "{args}");
        }
        // What the script only mentions is still not the agent.
        for args in [
            "docker run --future-flag value --entrypoint bash img -lc 'echo claude'",
            "docker run --future-flag value img bash -lc echo claude",
            "docker run --future-flag value img sh -c 'ls claude'",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
    }

    #[test]
    fn a_flags_value_is_never_the_agent() {
        // The sandbox design's requirement: the agent is a command word, never the value of
        // a flag. The four it names first, then the same values in every spelling.
        for args in [
            "docker run --name claude nginx",
            "docker run --hostname claude nginx",
            "docker run -e AGENT=claude nginx",
            "docker run -v ~/.claude:/root/.claude nginx",
            "docker run --name=claude nginx",
            "docker run --hostname=claude nginx",
            "docker run -h claude nginx",
            "docker run --env AGENT=claude nginx",
            "docker run --env=AGENT=claude nginx",
            "docker run -eAGENT=claude nginx",
            "docker run -e AGENT=/usr/local/bin/claude nginx",
            "docker run --volume ~/.claude:/root/.claude nginx",
            "docker run --volume=~/.claude:/root/.claude nginx",
            "docker run -v~/.claude:/root/.claude nginx",
            "docker run -v ~/claude:/root/claude nginx",
            "docker run --mount type=bind,src=~/.claude,dst=/root/.claude nginx",
            "docker run --label agent=claude nginx",
            "docker run -l claude nginx",
            "docker run --user claude nginx",
            "docker run -u claude nginx",
            "docker run --network claude nginx",
            "docker run -w /home/claude nginx",
            "docker run --workdir /home/claude nginx",
            "docker run --env-file ~/.claude/env nginx",
            "docker run --add-host claude:10.0.0.1 nginx",
            "docker run --cap-add claude nginx",
            "docker run --entrypoint= --name claude nginx",
            // The same flags on exec, which enters a container and runs a command in it.
            "docker exec -e AGENT=claude web bash",
            "docker exec --user claude web bash",
            "docker exec -w /home/claude web bash",
        ] {
            assert_eq!(docker(args), None, "{args}");
            assert_eq!(ps_of(args), None, "{args}");
        }
        // The other runtimes take the same flags.
        for runtime in ["podman", "nerdctl", "container"] {
            for flags in [
                "--name claude",
                "--hostname claude",
                "-e AGENT=claude",
                "-v ~/.claude:/root/.claude",
            ] {
                let args = format!("{runtime} run {flags} nginx");
                assert_eq!(agent(runtime, &args), None, "{args}");
            }
        }
        // And a real agent after such flags is still the one found.
        assert_eq!(
            docker("docker run --name claude --hostname claude -e AGENT=claude -v ~/.claude:/root/.claude img claude")
                .as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn a_flag_and_its_value_come_before_the_image_whichever_flag_it_is() {
        // Every option `run` takes a value for, and the value being called like the agent: had
        // the list lacked the flag, the value would be the image and `img` the program.
        for flag in [
            "-a",
            "-c",
            "-e",
            "-h",
            "-l",
            "-m",
            "-p",
            "-u",
            "-v",
            "-w",
            "-H",
            "--add-host",
            "--annotation",
            "--arch",
            "--attach",
            "--authfile",
            "--blkio-weight",
            "--blkio-weight-device",
            "--cap-add",
            "--cap-drop",
            "--cgroup-conf",
            "--cgroup-parent",
            "--cgroupns",
            "--cgroups",
            "--cidfile",
            "--config",
            "--context",
            "--cpu-period",
            "--cpu-quota",
            "--cpu-rt-period",
            "--cpu-rt-runtime",
            "--cpu-shares",
            "--cpus",
            "--cpuset-cpus",
            "--cpuset-mems",
            "--detach-keys",
            "--device",
            "--device-cgroup-rule",
            "--device-read-bps",
            "--device-read-iops",
            "--device-write-bps",
            "--device-write-iops",
            "--dns",
            "--dns-option",
            "--dns-search",
            "--domainname",
            "--env",
            "--env-file",
            "--expose",
            "--gidmap",
            "--gpus",
            "--group-add",
            "--health-cmd",
            "--health-interval",
            "--health-retries",
            "--health-start-interval",
            "--health-start-period",
            "--health-timeout",
            "--host",
            "--hostname",
            "--hostuser",
            "--init-path",
            "--ip",
            "--ip6",
            "--ipc",
            "--isolation",
            "--kernel-memory",
            "--label",
            "--label-file",
            "--link",
            "--link-local-ip",
            "--log-driver",
            "--log-level",
            "--log-opt",
            "--mac-address",
            "--memory",
            "--memory-reservation",
            "--memory-swap",
            "--memory-swappiness",
            "--mount",
            "--name",
            "--net",
            "--network",
            "--network-alias",
            "--oom-score-adj",
            "--os",
            "--pid",
            "--pids-limit",
            "--platform",
            "--pod",
            "--pod-id-file",
            "--publish",
            "--pull",
            "--restart",
            "--runtime",
            "--sdnotify",
            "--secret",
            "--security-opt",
            "--shm-size",
            "--stop-signal",
            "--stop-timeout",
            "--storage-opt",
            "--sysctl",
            "--systemd",
            "--tmpfs",
            "--tz",
            "--uidmap",
            "--ulimit",
            "--umask",
            "--unsetenv",
            "--user",
            "--userns",
            "--uts",
            "--variant",
            "--volume",
            "--volume-driver",
            "--volumes-from",
            "--workdir",
        ] {
            assert_eq!(
                both(&["docker", "run", flag, "claude", "img", "claude"]),
                [Some("claude".to_string()), Some("claude".to_string())],
                "{flag}"
            );
        }
        // With every one of them at once, the way a long `docker run` has them.
        let long = "docker run --rm -it -d --init --privileged -P --read-only --label a=b \
            --cap-add SYS_PTRACE --network host --user 1000:1000 --add-host h:1.2.3.4 \
            --env-file /a/env --platform linux/amd64 --mount type=bind,src=/a,dst=/a \
            -w /a -p 8080:80 -e A=b -v /a:/a --memory 2g --cpus 2 --name box img claude -p hi";
        assert_eq!(docker(long).as_deref(), Some("claude"));
        assert_eq!(ps(long).as_deref(), Some("claude"));
    }

    #[test]
    fn only_the_program_the_container_runs_is_the_agent() {
        for args in [
            // An image called like an agent, or a service.
            "docker run -it claude",
            "docker run --rm codex",
            "docker run -it ghcr.io/anthropics/claude-code",
            "docker run -d --name web pi",
            "docker compose run --rm claude",
            // An argument of another program.
            "docker run img echo claude",
            "docker run img cat /usr/bin/claude",
            "docker run img which claude",
            "docker run img npm install -g @anthropic-ai/claude-code",
            "docker run img git clone https://github.com/acme/claude",
            "docker exec -it web ls /usr/bin/claude",
            "docker exec -it web pkill claude",
            "docker run img cat /etc/codex",
            "docker compose run app pytest tests/codex",
            "docker compose exec app pytest tests/codex",
        ] {
            assert_eq!(docker(args), None, "{args}");
            assert_eq!(ps_of(args), None, "{args}");
        }
        // The program is what counts, after the variables and the words that only prefix it.
        for args in [
            "docker run img claude",
            "docker run img env FOO=1 claude --continue",
            "docker run img nohup /usr/local/bin/claude",
            "docker exec -it web claude",
        ] {
            assert_eq!(docker(args).as_deref(), Some("claude"), "{args}");
            assert_eq!(ps_of(args).as_deref(), Some("claude"), "{args}");
        }
    }

    #[test]
    fn a_script_that_only_mentions_the_agent_is_not_running_it() {
        for script in [
            "echo \"claude\"",
            "echo claude",
            "echo \"a; claude\"",
            "ls claude",
            "which claude",
            "printf \"%s\" claude",
            "cat /usr/bin/claude",
            "true # claude",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script}");
        }
    }

    #[test]
    fn a_project_or_a_container_named_like_an_agent_is_not_the_agent() {
        // The project the container mounts is called `hermes`, and the mount and the
        // working directory both end in it.
        assert_eq!(
            docker(
                "docker run --rm -it -v /home/me/hermes:/home/me/hermes -w /home/me/hermes nginx"
            ),
            None
        );
        // The real agent is what is found, not the directory.
        assert_eq!(
            docker(
                "docker run --rm -it -v /home/me/hermes:/home/me/hermes -w /home/me/hermes img bash -lc claude"
            )
            .as_deref(),
            Some("claude")
        );
        for args in [
            "docker run -d --name claude redis",
            "docker run -d --hostname codex -h gemini redis",
            "docker run -d --network pi --label amp redis",
            "docker run -d --volumes-from droid redis",
            // A container to enter is not the program run in it.
            "docker exec -it claude bash",
            "docker compose exec claude bash",
            // Commands that are not a run at all.
            "docker logs -f claude",
            "docker rm codex",
            "docker stop pi",
            "docker build -t claude .",
            "docker pull ghcr.io/anthropics/claude-code",
            "docker inspect codex",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
    }

    #[test]
    fn what_follows_an_option_no_list_knows_is_still_not_the_agent() {
        // The list of options that hold a name or a path is not the runtime's whole grammar.
        // The value of one it lacks is the first word that is no option, so it is taken for
        // the image, which is never the agent.
        for args in [
            "docker run --future-mount /home/me/pi:/home/me/pi nginx",
            "docker run --future-env X=/usr/bin/claude nginx",
            "docker run --future-env=X=/usr/bin/claude nginx",
            "docker run --future-flag claude nginx",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
        // `-c` of `run` is a cpu weight and of the runtime a context: not a script, and what
        // follows it is skipped whether or not a shell has come before.
        assert_eq!(docker("docker run -c claude nginx"), None);
        assert_eq!(
            docker("docker run -c 512 img claude").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn a_cluster_of_short_flags_ending_in_one_that_takes_a_value_takes_it() {
        // `-itv /a:/b` is `-i -t -v /a:/b`.
        assert_eq!(docker("docker run -itw /work/pi nginx"), None);
        assert_eq!(docker("docker run -itv /a/pi:/a/pi nginx"), None);
        assert_eq!(docker("docker run -itu claude nginx"), None);
        assert_eq!(
            docker("docker run -itv /a:/a img claude").as_deref(),
            Some("claude")
        );
        // A cluster that only holds flags with no value takes nothing.
        assert_eq!(
            docker("docker run -dit img claude").as_deref(),
            Some("claude")
        );
        assert_eq!(
            docker("docker run --rm -it -P img codex").as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn only_a_container_runtime_is_read() {
        assert_eq!(agent("kubectl", "kubectl exec -it pod -- claude"), None);
        assert_eq!(agent("zsh", "zsh -c claude"), None);
        assert_eq!(agent("claude", "claude --resume"), None);
        assert_eq!(
            agent("docker-compose", "docker-compose run dev claude"),
            None
        );
        assert_eq!(agent("dockerd", "dockerd claude"), None);
        assert_eq!(agent("", "docker run img claude"), None);
        assert_eq!(identify_ps("zsh", "zsh -c claude"), None);
        assert_eq!(identify_ps("", "docker run img claude"), None);
    }

    #[test]
    fn a_runtime_is_named_the_way_the_command_rule_names_a_program() {
        for command in [
            "docker",
            "/usr/local/bin/docker",
            "Docker",
            "docker.exe",
            r"C:\Program Files\Docker\docker.exe",
        ] {
            assert_eq!(runtime(command), Some("docker"), "{command}");
        }
        for (command, expected) in [
            ("podman", "podman"),
            ("nerdctl", "nerdctl"),
            ("container", "container"),
            ("/usr/local/bin/container", "container"),
            ("sbx", "sbx"),
        ] {
            assert_eq!(runtime(command), Some(expected), "{command}");
        }
        for command in [
            "",
            "zsh",
            "docker-compose",
            "dockerd",
            "containerd",
            "sbx-helper",
            "kubectl",
            "claude",
            "com.docker",
            "docker.cli",
        ] {
            assert_eq!(runtime(command), None, "{command}");
        }
    }

    #[test]
    fn docker_desktops_own_name_for_its_cli_is_docker() {
        // Docker Desktop for Mac may show its CLI to tmux as `com.docker.cli`, and the mark
        // it leaves on the pane says `docker`.
        for command in [
            "com.docker.cli",
            "/Applications/Docker.app/Contents/Resources/bin/com.docker.cli",
            "Com.Docker.Cli",
        ] {
            assert_eq!(runtime(command), Some("docker"), "{command}");
        }
        let found = identify_argv("com.docker.cli", &argv("docker run -it img claude")).unwrap();
        assert_eq!((found.runtime, found.kind.as_str()), ("docker", "claude"));
        assert_eq!(found.source(), "container:docker");
        let found = identify_ps("com.docker.cli", "docker run -it img codex").unwrap();
        assert_eq!((found.runtime, found.kind.as_str()), ("docker", "codex"));
    }

    #[test]
    fn unbalanced_quotes_and_odd_input_are_read_leniently_and_never_panic() {
        // What the shell in the container is given can be anything, so anything is read: a
        // quote that never closes, a substitution or a parenthesis left open, nothing at all.
        for script in [
            "echo it's claude",
            "echo \"claude",
            "echo it's claude",
            "\\",
            "$(",
            "$((",
            "`",
            "(((((",
            ")))))",
            "$(claude",
            "echo `claude",
            "; ; |",
            "",
            "\u{feff}claude \u{1f600}",
            // A comment or a line end at the very end, or cut short.
            "#",
            "# \\",
            "\\\r",
            "\\\r\n",
            "\\\n",
            "a \\\n",
            "\"\\",
            "\"a\\\n",
            "echo a #",
            "#\n#\n#",
            "$(#",
            "$(# claude\n)",
            "`# claude\n`",
            "(# claude\n",
        ] {
            let _ = both(&script_argv(script));
        }
        for args in [
            "docker run \"",
            "docker run '",
            "docker run - img",
            "docker run img sh -c",
            "docker run img sh -c ''",
            "docker run --entrypoint",
            "\u{feff}docker run img \u{1f600} claude",
        ] {
            let words: Vec<String> = args.split_whitespace().map(str::to_string).collect();
            let _ = identify_argv("docker", &words);
            let _ = identify_ps("docker", args);
        }
        // A quote never closed reads to the end, so what it hid is an argument of `echo`.
        assert_eq!(both(&script_argv("echo it's claude")), [None, None]);
        // A quote that never closes runs to the end, and what follows the flag is still a script.
        // Printed by `ps` the script's `$0` is glued to it, and is inside the quote too.
        assert_eq!(
            identify_argv("docker", &argv("docker run img bash -lc \"'claude\" _"))
                .map(|found| found.kind)
                .as_deref(),
            Some("claude")
        );
        // A substitution never closed is still a list of commands, and one of them runs.
        assert_eq!(
            both(&script_argv("echo $(claude")),
            [Some("claude".to_string()), Some("claude".to_string())]
        );
    }

    /// A small generator for the tests below, with no dependency and the same inputs on every
    /// run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        /// Up to `max` of `pieces`, put together.
        fn text(&mut self, pieces: &[&str], max: usize) -> String {
            let count = self.below(max + 1);
            (0..count)
                .map(|_| pieces[self.below(pieces.len())])
                .collect()
        }
    }

    /// What a script might be made of, the quoting and the separators first.
    const PIECES: &[&str] = &[
        "#",
        "\\",
        "\n",
        "\r",
        "'",
        "\"",
        "$",
        "(",
        ")",
        ";",
        "&",
        "|",
        "`",
        " ",
        "\t",
        "claude",
        "echo",
        "true",
        "a",
        "x#y",
        "$(",
        "${#v}",
        "&&",
        "||",
        "\\\n",
        "\\\r\n",
        "# ",
        "codex",
        "exec",
        "--flag",
        "\u{1f600}",
    ];

    #[test]
    fn a_script_made_of_anything_is_read_and_never_panics() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..5_000 {
            let _ = both(&script_argv(&rng.text(PIECES, 24)));
        }
    }

    #[test]
    fn a_comment_holds_nothing_whatever_is_in_it() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for _ in 0..5_000 {
            let text = rng.text(PIECES, 16).replace('\n', "");
            // A comment line before claude never hides it, and a comment alone names nothing.
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(&format!("#{text}\nclaude"))),
                [found.clone(), found],
                "{text:?}"
            );
            for script in [
                format!("#{text}"),
                format!("true #{text}"),
                format!("echo hi # {text}"),
            ] {
                assert_eq!(both(&script_argv(&script)), [None, None], "{script:?}");
            }
        }
    }

    #[test]
    fn a_line_joined_by_a_backslash_reads_as_the_line_it_makes() {
        // What goes before and after is calm: no quote, comment or substitution, and nothing
        // that a shell's own option would be taken for in the line `ps` prints.
        let calm: Vec<&str> = PIECES
            .iter()
            .copied()
            .filter(|piece| !piece.contains(['#', '\n', '\r', '\'', '"', '\\', '`', '$', '-']))
            .collect();
        let mut rng = Rng(0xdead_beef_cafe_f00d);
        for _ in 0..5_000 {
            let (before, after) = (rng.text(&calm, 6), rng.text(&calm, 6));
            assert_eq!(
                both(&script_argv(&format!("{before} \\\n{after}"))),
                both(&script_argv(&format!("{before} {after}"))),
                "{before:?} and {after:?}"
            );
        }
    }

    #[test]
    fn the_shell_reading_splits_words_and_commands_the_way_a_shell_does() {
        let words = |text: &str| commands(text);
        assert_eq!(words("a b  c"), [["a", "b", "c"]]);
        assert_eq!(words("a 'b c' \"d e\" f\\ g"), [["a", "b c", "d e", "f g"]]);
        assert_eq!(words("a\tb\r\nc"), [vec!["a", "b"], vec!["c"]]);
        assert_eq!(
            words("a; b && c || d | e & f"),
            [
                vec!["a"],
                vec!["b"],
                vec!["c"],
                vec!["d"],
                vec!["e"],
                vec!["f"]
            ]
        );
        // Quotes hide separators, and the quoting is the shell's: a single quote keeps a
        // backslash, a double quote drops it only before `\`, `\"`, `$` and a backtick.
        assert_eq!(words("'a; b' \"c | d\""), [["a; b", "c | d"]]);
        assert_eq!(
            words(r#"'a\b' "a\b" "a\"b" "a\$b""#),
            [[r"a\b", r"a\b", r#"a"b"#, "a$b"]]
        );
        // An empty quoted word is still a word; adjacent quotes join into one.
        assert_eq!(words("a '' b"), [["a", "", "b"]]);
        assert_eq!(words("a'b'\"c\"d"), [["abcd"]]);
        // A subshell is a list of commands of its own, and what follows it starts afresh.
        assert_eq!(
            words("(cd /x && claude) codex"),
            [vec!["cd", "/x"], vec!["claude"], vec!["codex"]]
        );
        assert!(words("").is_empty());
        assert!(words("  ;  ; ").is_empty());
    }

    #[test]
    fn a_substitution_is_commands_of_its_own_in_the_middle_of_a_word() {
        let words = |text: &str| commands(text);
        // The commands inside come first, and the word the substitution stands in goes on
        // after it: `date` is one command, and `echo <date> claude` another.
        assert_eq!(
            words("echo $(date) claude"),
            [vec!["date"], vec!["echo", "$()", "claude"]]
        );
        assert_eq!(
            words("echo `date` codex"),
            [vec!["date"], vec!["echo", "$()", "codex"]]
        );
        assert_eq!(
            words("cd $(dirname $0)/claude && nginx"),
            [
                vec!["dirname", "$0"],
                vec!["cd", "$()/claude"],
                vec!["nginx"]
            ]
        );
        // They nest, and a separator inside ends the commands inside only.
        assert_eq!(
            words("echo $(a $(b) c; d) e"),
            [
                vec!["b"],
                vec!["a", "$()", "c"],
                vec!["d"],
                vec!["echo", "$()", "e"]
            ]
        );
        assert_eq!(
            words("echo `a; b` c"),
            [vec!["a"], vec!["b"], vec!["echo", "$()", "c"]]
        );
        // A backslash keeps a backtick from closing one.
        assert_eq!(
            words(r"echo `a \` b` c"),
            [vec!["a", "`", "b"], vec!["echo", "$()", "c"]]
        );
        // Arithmetic runs no commands.
        assert_eq!(
            words("echo $((1 + (2 * 3))) claude"),
            [["echo", "$(())", "claude"]]
        );
        // A word the substitution starts, or is glued to, is one word.
        assert_eq!(words("x=$(a) claude"), [vec!["a"], vec!["x=$()", "claude"]]);
        // What a quote holds is text, and so is a substitution in it.
        assert_eq!(
            words("echo '$(a) b' \"$(c) d\""),
            [["echo", "$(a) b", "$(c) d"]]
        );
        // A substitution never closed, a `)` that closes nothing, and a `` ` `` alone.
        assert_eq!(
            words("echo $(a; b"),
            [vec!["a"], vec!["b"], vec!["echo", "$()"]]
        );
        assert_eq!(words("a) b"), [vec!["a"], vec!["b"]]);
        assert_eq!(
            words("case x in a) claude ;; esac"),
            [vec!["case", "x", "in", "a"], vec!["claude"], vec!["esac"]]
        );
        assert_eq!(words("echo `a"), [vec!["a"], vec!["echo", "$()"]]);
    }

    #[test]
    fn a_substitution_does_not_start_a_command_after_it() {
        // `)` and the second backtick end what was substituted, not the command around it:
        // what follows is still an argument of `echo`, `cd` or `date`.
        for script in [
            "echo $(date) claude",
            "cd $(dirname $0)/claude && nginx",
            "echo `date` codex",
            "echo $(a $(b) c) claude",
            "echo $((1 + 2)) claude",
            "echo \"$(date) claude\"",
            "ls $(pwd) /usr/bin/claude",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script}");
        }
        // What runs inside one is a command, and so is what follows the whole line.
        for script in [
            "echo $(claude)",
            "echo $(date; claude)",
            "echo `claude`",
            "echo $(date) && claude",
            "x=$(date) claude",
            "FOO=`date` claude",
            "cd $(dirname $0) && claude",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script}"
            );
        }
    }

    #[test]
    fn a_script_over_several_lines_is_read() {
        // A shell function's script, as the process has it: newlines in the one argument.
        let script = "\n  /usr/local/bin/firewall.sh || true\n  mise install\n  \
            eval \"$(mise activate bash)\"\n  claude --effort medium --dangerously-skip-permissions \"$@\"\n";
        let argv = [
            "docker",
            "run",
            "--rm",
            "-it",
            "--entrypoint",
            "bash",
            "dev-image",
            "-lc",
            script,
            "_",
        ];
        assert_eq!(
            both(&argv),
            [Some("claude".to_string()), Some("claude".to_string())]
        );
        // Tabs and a carriage return between the lines, the way an editor leaves them.
        for script in [
            "true\n\tclaude\n",
            "true\r\nclaude\r\n",
            "\tclaude",
            "a\n\n\nclaude",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script:?}"
            );
        }
        // The line after the first is a command of its own, not an argument of it.
        assert_eq!(both(&script_argv("echo hi\necho claude")), [None, None]);
        assert_eq!(
            both(&script_argv("cd /w\nclaude")).map(|kind| kind.is_some()),
            [true, true]
        );
    }

    #[test]
    fn a_comment_runs_to_the_end_of_its_line_and_holds_no_quote_or_command() {
        // A word that begins with `#` is a comment: what is in it, a quote or a `;`, is not
        // shell syntax, and the newline that ends it still ends the command.
        for script in [
            "# Firewall first; it's optional here\nclaude --effort medium",
            "# say \"hi\nclaude",
            "/usr/local/bin/firewall.sh || true\n# it's optional\nmise install\nclaude",
            "  # indented, and it's a comment\n  claude",
            "true # it's fine\nclaude",
            "true; # a `tick\nclaude",
            "claude # start it; it's Claude",
            "true && # it's a pause\nclaude",
            // A comment does not go on to the next line for a backslash at its end.
            "# note \\\nclaude",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script:?}"
            );
        }
        // What a comment says is not a command, whatever would separate it.
        for script in [
            "# a && claude",
            "true # note; claude",
            "true; # claude",
            "echo hi # && claude",
            "true && # claude\nnginx",
            "# claude\necho done",
            "echo hi #claude | tee",
            "# it's not claude",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script:?}");
        }
    }

    #[test]
    fn a_hash_that_does_not_begin_a_word_is_no_comment() {
        // Only a `#` where a word could begin is one: in the middle of a word, after `$`, in
        // `${#var}`, in quotes, escaped, or after a substitution, it is a character.
        for script in [
            "a#b; claude",
            "echo a#b; claude",
            "echo $#; claude",
            "echo ${#PATH}; claude",
            "echo '# no'; claude",
            "echo \"# no\"; claude",
            "echo \\#; claude",
            "echo ''#x; claude",
            "echo $(date)#x; claude",
            "cd /w#x && claude",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script:?}"
            );
        }
        assert_eq!(both(&script_argv("echo a#b claude")), [None, None]);
    }

    #[test]
    fn a_backslash_before_a_newline_joins_the_lines() {
        // The line goes on: what follows is read as if it stood where the backslash was.
        for script in [
            "true && \\\nclaude --effort medium",
            "true && \\\n  claude --effort medium",
            "/usr/local/bin/firewall.sh || true; \\\nclaude",
            "true &&\\\nclaude",
            "echo hi\\\n && claude",
            "cla\\\nude",
            "\\\nclaude",
            "echo \"a\\\nb\" && claude",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script:?}"
            );
        }
        // What follows the joined line is still what it was: an argument of the command
        // before it, and a single quote keeps the backslash and the newline as they are.
        for script in [
            "echo a \\\nclaude",
            "echo a \\\n  claude --flag",
            "true \\\n && echo claude",
            "echo 'a\\\nb' claude",
        ] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script:?}");
        }
    }

    #[test]
    fn a_line_that_ends_in_a_carriage_return_and_a_newline_is_one_line_end() {
        // A script written with CR LF line ends: the carriage return is not part of the
        // command before it, and a backslash before the pair joins the lines.
        for script in [
            "true\r\nclaude\r\n",
            "true && \\\r\nclaude",
            "true &&\\\r\n  claude --effort medium\r\n",
            "# it's a note\r\nclaude\r\n",
        ] {
            let found = Some("claude".to_string());
            assert_eq!(
                both(&script_argv(script)),
                [found.clone(), found],
                "{script:?}"
            );
        }
        for script in ["echo a \\\r\nclaude", "echo hi\r\n# claude\r\n"] {
            assert_eq!(both(&script_argv(script)), [None, None], "{script:?}");
        }
    }

    #[test]
    fn a_comment_and_a_line_end_are_read_the_way_a_shell_reads_them() {
        let words = |text: &str| commands(text);
        assert_eq!(words("a # b; c\nd"), [vec!["a"], vec!["d"]]);
        assert_eq!(words("a #b 'c\nd"), [vec!["a"], vec!["d"]]);
        assert_eq!(words("#a\n#b\nc"), [["c"]]);
        assert_eq!(words("a;#b\nc"), [vec!["a"], vec!["c"]]);
        assert_eq!(words("(#a\nb)"), [["b"]]);
        assert_eq!(words("a#b c"), [["a#b", "c"]]);
        assert_eq!(words("a '#b' \"#c\" \\#d"), [["a", "#b", "#c", "#d"]]);
        assert_eq!(words("a \\\nb"), [["a", "b"]]);
        assert_eq!(words("a\\\nb"), [["ab"]]);
        assert_eq!(words("a \\\n\\\nb"), [["a", "b"]]);
        assert_eq!(words("\"a\\\nb\""), [["ab"]]);
        assert_eq!(words("'a\\\nb'"), [["a\\\nb"]]);
        // A carriage return before the newline goes with it, for a script whose lines end
        // in CR LF.
        assert_eq!(words("a \\\r\nb"), [["a", "b"]]);
        assert_eq!(words("a\\\r\nb"), [["ab"]]);
        // A backslash at the very end has nothing to join, and a comment at the very end
        // nothing to end at.
        assert_eq!(words("a \\"), [["a"]]);
        assert_eq!(words("a #b"), [["a"]]);
    }

    #[test]
    fn a_shell_metacharacter_in_a_flags_value_is_only_a_character() {
        // The exact list keeps a value whole, so nothing in it can end the runtime's command
        // or swallow the rest of the line: the ones a shell would take for syntax included.
        let script = "/usr/local/bin/firewall.sh || true; mise install; claude --effort medium";
        for value in [
            "URL=http://x?a=1&b=2",
            "DATABASE_URL=postgres://u:p@h/db?sslmode=disable&pool=5",
            "X=a;b",
            "X=a|b",
            "X=a||b",
            "X=a&&b",
            "X=a(b",
            "X=a)b",
            "PS1=(sandbox)$",
            "X=a`b",
            "X=`date`",
            "X=$(date)",
            "X=it's",
            "X=say \"hi",
            "X=a\\b",
            "X=a<b>c",
            "X=a#b",
            "X=a\nb",
            "X=hello world",
            "X=hello\tworld",
            "X=",
            "X=\"",
            "X=$((",
        ] {
            // A run of the agent directly, and the shape of a shell function that runs the agent in a container.
            let direct = ["docker", "run", "-e", value, "img", "claude"];
            let boxed = [
                "docker",
                "run",
                "--rm",
                "-it",
                "-e",
                value,
                "--entrypoint",
                "bash",
                "img",
                "-lc",
                script,
                "_",
            ];
            for argv in [&direct[..], &boxed[..]] {
                let words: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
                assert_eq!(
                    identify_argv("docker", &words)
                        .map(|found| found.kind)
                        .as_deref(),
                    Some("claude"),
                    "{value:?} in {argv:?}"
                );
            }
        }
        // And in a flag that is not `-e`, and one that is welded to its value.
        for argv in [
            &[
                "docker",
                "run",
                "--health-cmd",
                "pg_isready || exit 1",
                "img",
                "claude",
            ][..],
            &[
                "docker", "run", "--label", "a=b&c", "--name", "x;y", "img", "claude",
            ][..],
            &["docker", "run", "-e=a&b", "img", "claude"][..],
            &["docker", "run", "--env=X=a;b", "img", "claude"][..],
        ] {
            let words: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
            assert_eq!(
                identify_argv("docker", &words)
                    .map(|found| found.kind)
                    .as_deref(),
                Some("claude"),
                "{argv:?}"
            );
        }
        // Nor does one make an agent of what is not: `claude` in a value stays a value.
        let words: Vec<String> = ["docker", "run", "-e", "X=a;claude", "img", "nginx"]
            .iter()
            .map(|word| word.to_string())
            .collect();
        assert_eq!(identify_argv("docker", &words), None);
    }

    #[test]
    fn a_value_with_a_space_is_one_word() {
        // The line `ps` prints cannot say it, but the list can.
        for line in [
            "docker run -e 'MSG=hello world' img claude",
            "docker run --label 'a b=c d' --name 'my box' img claude",
            "docker run -v '/a b:/a b' -w '/a b' img claude",
            "docker run --entrypoint bash -e 'X=a b' img -lc claude",
            "docker run -e 'X=a b' --entrypoint bash img -lc 'echo hi; claude'",
            "docker exec -e 'MSG=hello world' web claude",
        ] {
            assert_eq!(docker(line).as_deref(), Some("claude"), "{line}");
        }
        // And what is inside one is not the program or the image.
        for line in [
            "docker run -e 'X=a b' img nginx claude",
            "docker run --name 'claude code' nginx",
            "docker run -e 'MSG=hello claude' nginx",
            "docker run img 'claude code'",
        ] {
            assert_eq!(docker(line), None, "{line}");
        }
    }

    #[test]
    fn after_the_script_come_its_arguments_and_they_are_not_commands() {
        // `bash -lc SCRIPT ARG0 ARG1`: `_` is the script's `$0`, and no words after it are
        // read as a command.
        for argv in [
            &["docker", "run", "img", "bash", "-lc", "echo hi", "claude"][..],
            &[
                "docker", "run", "img", "bash", "-c", "true", "claude", "codex",
            ][..],
            &["docker", "run", "img", "sh", "-c", "exit 0", "_", "claude"][..],
        ] {
            let words: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
            assert_eq!(identify_argv("docker", &words), None, "{argv:?}");
        }
    }

    #[test]
    fn an_entrypoint_that_only_runs_something_is_not_handed_the_command_to_run() {
        // `which`, `cat`, `ls`, `echo` and `true` run themselves, and what follows the
        // image is their arguments.
        for args in [
            "docker run --rm --entrypoint which img claude",
            "docker run --rm --entrypoint cat img claude",
            "docker run --rm --entrypoint ls img claude",
            "docker run --rm --entrypoint echo img claude",
            "docker run --rm --entrypoint /bin/true img claude",
            "docker run --rm --entrypoint=echo img claude",
            "docker run --entrypoint /usr/bin/which img codex",
            "docker run --entrypoint python3 img claude",
            "docker run --entrypoint node img /usr/bin/claude",
            // A script by its name is handed the command, but the command has to be one.
            "docker run --entrypoint /entrypoint.sh img nginx claude",
            "docker run --entrypoint docker-entrypoint.sh img echo claude",
        ] {
            assert_eq!(docker(args), None, "{args}");
            assert_eq!(ps_of(args), None, "{args}");
        }
        // One that hands its arguments on: a prefix, a shell, and a script by its name.
        for (args, expected) in [
            (
                "docker run --entrypoint /entrypoint.sh img claude",
                "claude",
            ),
            (
                "docker run --entrypoint docker-entrypoint.sh img claude --continue",
                "claude",
            ),
            (
                "docker run --entrypoint /usr/local/bin/docker-entrypoint img codex",
                "codex",
            ),
            ("docker run --entrypoint ./run.sh img claude", "claude"),
            (
                "docker run --entrypoint=/opt/entrypoint img claude",
                "claude",
            ),
            (
                "docker run --entrypoint /ENTRYPOINT.SH img claude",
                "claude",
            ),
            ("docker run --entrypoint env img claude", "claude"),
            ("docker run --entrypoint tini img -- claude", "claude"),
            ("docker run --entrypoint gosu img node claude", "claude"),
            ("docker run --entrypoint bash img -lc claude", "claude"),
            ("docker run --entrypoint sh img -c 'exec claude'", "claude"),
            ("docker run --entrypoint claude img", "claude"),
        ] {
            assert_eq!(docker(args).as_deref(), Some(expected), "{args}");
        }
    }

    #[test]
    fn a_file_a_shell_runs_is_not_a_script() {
        // `bash FILE -c claude` runs FILE, and `-c claude` are its arguments.
        for args in [
            "docker run img bash /entrypoint.sh -c claude",
            "docker run img sh /entrypoint.sh -c claude",
            "docker run --entrypoint bash img /entrypoint.sh -c claude",
            "docker run img bash script.sh -lc claude",
            "docker exec web sh run.sh -c codex",
            "docker run --future-flag value img bash script.sh -c claude",
            "docker run --future-flag value --entrypoint bash img script.sh -c claude",
            // `--` ends the shell's options, and what follows it is the file, or the script
            // even when it looks like an option.
            "docker run img bash -- /entrypoint.sh -c claude",
            "docker run img bash -c -- -c claude",
        ] {
            assert_eq!(docker(args), None, "{args}");
            assert_eq!(ps_of(args), None, "{args}");
        }
        // What comes before the file is the shell's options, and its `-c` still counts when it
        // comes among them, or the option takes a value that is a bare word.
        for args in [
            "docker run img bash -l -c claude",
            "docker run img bash -o pipefail -c claude",
            "docker run img bash -eo pipefail -c claude",
            "docker run img bash -O extglob -lc claude",
            "docker run img bash +o history -c claude",
            "docker run img bash --rcfile /etc/rc -c claude",
            "docker run img bash --init-file /etc/rc -ic claude",
            "docker run img bash --norc --noprofile -c claude",
            "docker run img bash -c -l claude",
            "docker run img bash -c -- claude",
            "docker run img bash -l -c -- claude",
            "docker run --entrypoint bash img -o pipefail -lc claude",
            "docker run --future-flag value --entrypoint bash img -o pipefail -lc claude",
        ] {
            assert_eq!(docker(args).as_deref(), Some("claude"), "{args}");
            assert_eq!(ps_of(args).as_deref(), Some("claude"), "{args}");
        }
    }

    #[test]
    fn sandbox_before_the_verb_is_the_agent_first_form_only_as_the_first_word() {
        // `docker sandbox run claude` has no image, but a word `sandbox` that is the value of
        // an option, a profile or a project of the compose command is not that form.
        for args in [
            "docker compose --profile sandbox run app claude",
            "docker compose -p sandbox run --rm app claude",
            "docker --context sandbox run img claude",
            "docker compose --project-name sandbox exec app claude",
        ] {
            assert_eq!(docker(args).as_deref(), Some("claude"), "{args}");
            assert_eq!(ps_of(args).as_deref(), Some("claude"), "{args}");
        }
        for args in [
            "docker compose --profile sandbox run app nginx claude",
            "docker compose --profile sandbox run --rm claude",
        ] {
            assert_eq!(docker(args), None, "{args}");
        }
        // The form itself, with options in front of it and after it.
        for args in [
            "docker sandbox run claude",
            "docker --debug sandbox run claude",
            "docker -H tcp://host:2375 sandbox run codex",
            "docker sandbox run --name work claude .",
        ] {
            assert!(docker(args).is_some(), "{args}");
        }
    }
}

#[cfg(test)]
mod ps_tests {
    use super::*;

    const BOXED_FROM_PS: &str = r#"docker run --rm -it -v /a/project:/a/project -v /h/.claude:/h/.claude -e CLAUDE_CONFIG_DIR=/h/.claude --entrypoint bash dev-image -lc /usr/local/bin/firewall.sh || true; mise install; eval "$(mise activate bash)"; claude --effort medium --dangerously-skip-permissions "$@" _"#;

    fn ps(line: &str) -> Option<String> {
        identify_ps("docker", line).map(|found| found.kind)
    }

    #[test]
    fn a_box_as_ps_prints_it() {
        assert_eq!(ps(BOXED_FROM_PS).as_deref(), Some("claude"));
        let with_future = BOXED_FROM_PS.replacen("docker run", "docker run --future-flag value", 1);
        assert_eq!(ps(&with_future).as_deref(), Some("claude"));
    }

    #[test]
    fn ps_line_shapes_the_typed_reader_gets_wrong() {
        // What `ps` prints, checked against what the process really ran.
        let cases: &[(&str, Option<&str>)] = &[
            // a flag's value with a shell metacharacter before the script
            (
                "docker run --rm -it -e URL=http://x?a=1&b=2 --entrypoint bash img -lc /fw.sh || true; claude _",
                Some("claude"),
            ),
            (
                "docker run --rm -it -e X=a;b --entrypoint bash img -lc /fw.sh || true; claude _",
                Some("claude"),
            ),
            (
                "docker run --rm -it -e X=a(b --entrypoint bash img -lc claude",
                Some("claude"),
            ),
            (
                "docker run --rm -it -e X=a`b --entrypoint bash img -lc claude",
                Some("claude"),
            ),
            (
                "docker run --rm -it -e X=it's --entrypoint bash img -lc claude",
                Some("claude"),
            ),
            (
                "docker run --rm -it -e X=say \"hi --entrypoint bash img -lc claude",
                Some("claude"),
            ),
            // a script across lines: macOS prints \012 for a newline and \011 for a tab
            (
                "docker run --rm -it --entrypoint bash img -lc \\012  /fw.sh || true\\012  mise install\\012  claude --effort medium\\012 _",
                Some("claude"),
            ),
            (
                "docker run --rm -it img bash -c cd /w\\012claude",
                Some("claude"),
            ),
            (
                "docker run --rm -it img bash -c \\011claude",
                Some("claude"),
            ),
            (
                "docker run --rm -it img bash -c echo hi\\012echo claude",
                None,
            ),
            // a value with a newline in it is one word, and does not end the runtime's command
            (
                "docker run --rm -it -e X=a\\012b img claude",
                Some("claude"),
            ),
            // still not agents
            ("docker run -e X=a;b img nginx claude", None),
            ("docker run img echo claude", None),
            ("docker run --name claude nginx", None),
        ];
        for (args, expected) in cases {
            assert_eq!(ps(args).as_deref(), *expected, "{args}");
        }
    }

    #[test]
    fn a_comment_and_a_backslash_line_end_are_read_in_the_line_ps_prints() {
        // ps prints a newline as \012: a comment ends there, and a backslash before it joins
        // the lines.
        for (line, expected) in [
            (
                r"docker run img bash -lc # Firewall first; it's optional here\012claude --effort medium _",
                Some("claude"),
            ),
            (
                r"docker run img bash -lc echo hi # it's fine\012claude _",
                Some("claude"),
            ),
            (
                r"docker run img bash -lc true && \\012claude --effort medium _",
                Some("claude"),
            ),
            (
                r"docker run img bash -lc true && \\012  claude _",
                Some("claude"),
            ),
            (r"docker run img bash -lc # a && claude _", None),
            (r"docker run img bash -lc true # note; claude _", None),
            (r"docker run img bash -lc echo a \\012claude _", None),
        ] {
            assert_eq!(ps(line).as_deref(), expected, "{line}");
        }
    }

    #[test]
    fn a_control_character_ps_prints_raw_or_escaped_is_white_space() {
        // macOS escapes a newline and a tab as octal, and prints a carriage return as `^M`;
        // other systems print them as they are.
        for line in [
            "docker run img bash -c\tclaude",
            "docker run img bash -c\rclaude",
            "docker run img bash -c \\015claude",
            "docker run img bash -c \\011claude",
            "docker run img bash -c ^Mclaude",
            "docker run img bash -c true;^Mclaude",
            // White space, not nothing: it is what makes `exec` and `claude` two words.
            "docker run img bash -c exec^Mclaude",
            "docker run img bash -c exec\\015claude",
        ] {
            assert_eq!(ps(line).as_deref(), Some("claude"), "{line:?}");
        }
        // A carriage return and a newline together are one line end, and a backslash before
        // them joins the lines.
        for (line, expected) in [
            (r"docker run img bash -c true^M\012claude", Some("claude")),
            (
                r"docker run img bash -c true && \^M\012claude",
                Some("claude"),
            ),
            (
                r"docker run img bash -c true && \\015\012claude",
                Some("claude"),
            ),
            (r"docker run img bash -c echo a \^M\012claude", None),
            (r"docker run img bash -c echo a \\015\012claude", None),
        ] {
            assert_eq!(ps(line).as_deref(), expected, "{line}");
        }
    }

    #[test]
    fn what_the_line_cannot_say_is_missed_and_never_guessed() {
        // A value with a space is two words in the line, so what follows it is shifted: the
        // second half of the value is taken for the image. Only the exact list is right.
        assert_eq!(ps("docker run -e MSG=hello world img claude"), None);
        // An empty argument leaves no trace at all.
        assert_eq!(ps("docker run --entrypoint  img claude"), None);
    }

    #[test]
    fn ps_is_read_as_words_and_only_the_script_is_shell_text() {
        // The runtime's own words are not shell syntax: a quote in front is a character.
        assert_eq!(
            ps("docker run -e X=' img claude").as_deref(),
            Some("claude")
        );
        // A script that only mentions the agent, printed, is not running it.
        assert_eq!(
            ps("docker run --rm -it img bash -lc cat /usr/bin/claude _"),
            None
        );
        assert_eq!(ps("docker run img sh -c ls claude"), None);
    }
}
