//! `taw --machine <name> <verb>`: runs the verb on that machine, through ssh, and gives its answer back
//! as it was given.
//!
//! The command line is looked at before clap sees it ([`leading`]): what follows the machine is the
//! other machine's command line, and this machine's own parser has no business with it. The words go
//! over as they were typed, each one quoted for the shell on the other end ([`super::ssh::remote_line`]),
//! so nothing in them is read by a shell on the way. A connection that fails is never retried and
//! never answered from this machine.

use std::io::{Read, Write};
use std::process::Stdio;

use crate::cli::envelope::{self, CODES};
use crate::paths::Paths;

use super::Machine;
use super::config::{MachinesDoubt, config_file_word, load_machines};
use super::ssh::{self, Mode, Ssh};

/// The envelope id of what goes wrong on the way to a machine, before any verb has run there.
const ID: &str = "cli:machine";

/// What the leading options of a command line name.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Leading {
    pub(crate) machine: String,
    pub(crate) pretty: bool,
    /// The command line that goes to the machine: the first word that is not a `--` option, and what
    /// follows it.
    pub(crate) rest: Vec<String>,
}

/// The options before the first word that does not start with `--`, when they name a machine and a verb
/// follows. `args` is the command line without the program's name. `None` leaves the line to clap: no
/// `--machine`, one with no name or an empty one, two of them, or nothing to run (`--help`, `-V` and a
/// missing verb are this machine's own parser's to answer).
pub(crate) fn leading(args: &[String]) -> Option<Leading> {
    let mut machine: Option<String> = None;
    let mut pretty = false;
    let mut i = 0;
    while let Some(word) = args.get(i).filter(|word| word.starts_with("--")) {
        match word.as_str() {
            "--pretty" => pretty = true,
            "--machine" => {
                let name = args.get(i + 1).filter(|name| !name.starts_with('-'))?;
                if machine.replace(name.clone()).is_some() {
                    return None;
                }
                i += 1;
            }
            other => match other.strip_prefix("--machine=") {
                Some(name) => {
                    if machine.replace(name.to_string()).is_some() {
                        return None;
                    }
                }
                // An option of the other machine's or of this one's own: clap decides.
                None => return None,
            },
        }
        i += 1;
    }
    let machine = machine.filter(|name| !name.is_empty())?;
    let rest = &args[i..];
    if rest.first().is_none_or(|word| word.starts_with('-')) {
        return None;
    }
    Some(Leading {
        machine,
        pretty,
        rest: rest.to_vec(),
    })
}

/// Whether the leading options of `args` (a lossy view of the command line, without the program's name)
/// include `--machine`, as `--machine <word>`, `--machine=<word>` or with an empty name, whatever
/// follows: the line is meant for another machine, whether or not [`leading`] can read it.
pub(crate) fn names_machine(args: &[String]) -> bool {
    for word in args {
        match word.as_str() {
            "--machine" => return true,
            "--pretty" => {}
            other if other.starts_with("--machine=") => return true,
            _ => return false,
        }
    }
    false
}

/// The words of a verb that belongs to this machine, for `--machine <name>` and the command line `rest`;
/// none for one that goes over.
///
/// The verb is read from the first two words that do not start with `-`, so that an option between
/// them (`agent --pretty attach`) does not hide it.
pub(crate) fn refused_verb(name: &str, rest: &[String]) -> Option<String> {
    let mut plain = rest
        .iter()
        .map(String::as_str)
        .filter(|word| !word.starts_with('-'));
    let first = plain.next()?;
    let second = plain.next();
    let words = match (first, second) {
        ("ui", _) => "the popup is that machine's own; use the app: taw app".to_string(),
        ("app", _) => format!("run taw app here; it lists {name}"),
        ("open", _) if !rest.iter().any(|word| word == "--json") => {
            "needs --json; go in from the app: taw app".to_string()
        }
        ("agent", Some("attach")) => {
            format!("use the app: taw app --go {name}:<target>")
        }
        ("machine", _) => format!("taw machine runs here: taw machine check {name}"),
        _ => return None,
    };
    let verb = match (first, second) {
        ("agent", Some("attach")) => "agent attach",
        _ => first,
    };
    Some(format!("taw --machine {name} {verb}: {words}"))
}

/// What a person is told for a name that `[machines]` does not have.
pub(crate) fn unknown_name(name: &str, machines: &[Machine], paths: &Paths) -> String {
    if machines.is_empty() {
        format!(
            "taw: no machine called {name}: [machines] in {} lists none",
            config_file_word(paths)
        )
    } else {
        let yours: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
        format!(
            "taw: no machine called {name} in [machines] (yours: {})",
            yours.join(", ")
        )
    }
}

/// The envelope for a failure on the way to a machine.
fn failure(code: &str, message: &str, pretty: bool) -> String {
    debug_assert!(CODES.contains(&code), "undeclared error code {code:?}");
    envelope::render(&envelope::failure(ID, code, message), pretty)
}

/// Runs `rest` on the machine called `name` and returns the process's exit status: what the far
/// `taw` exits with, 1 for a failure on the way (told as an envelope on stdout) and 2 for a command
/// line it refuses (told on stderr).
pub fn run(name: &str, pretty: bool, rest: &[String]) -> i32 {
    // `main` returns from here, before it starts logging: without this a warning on the way (a control
    // folder that cannot be made) would go nowhere.
    crate::cli::init_logging();
    let paths = Paths::from_env();
    let machines = match load_machines(&paths) {
        Ok(machines) => machines,
        Err(doubt @ (MachinesDoubt::NotLoaded { .. } | MachinesDoubt::Dropped { .. })) => {
            println!(
                "{}",
                failure("invalid_argument", &doubt.to_string(), pretty)
            );
            return 1;
        }
    };
    // `main` prints a refusal bare for a verb that raised it; this returns before `main`'s mapping
    // runs, so the words carry their own `taw: ` and are printed here.
    let Some(machine) = machines.iter().find(|m| m.name == name) else {
        eprintln!("{}", unknown_name(name, &machines, &paths));
        return 2;
    };
    if let Some(words) = refused_verb(name, rest) {
        eprintln!("taw: {words}");
        return 2;
    }

    let mut args: Vec<&str> = Vec::new();
    if pretty {
        args.push("--pretty");
    }
    args.extend(rest.iter().map(String::as_str));
    let mut cmd = ssh::command(&Ssh::from_env(), &paths, machine, Mode::Command, &args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("{name}: ssh could not be run ({err})");
            println!("{}", failure("machine_unreachable", &message, pretty));
            return 1;
        }
    };
    // Read to the end before waiting: a full pipe would stop the child, and the end of the pipe is the
    // end of the child (a connection kept open for the next command has no hold on it).
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let status = child.wait();
    let code = status.as_ref().ok().and_then(|status| status.code());
    let unreachable = |why: &str| {
        println!(
            "{}",
            failure("machine_unreachable", &format!("{name}: {why}"), pretty)
        );
        1
    };
    match code {
        Some(255) => {
            let line = ssh::last_line(&stderr);
            if line.is_empty() {
                unreachable("ssh exited 255 without saying why")
            } else {
                unreachable(&ssh::shown(&line))
            }
        }
        Some(127) if String::from_utf8_lossy(&stderr).contains("not found") => {
            unreachable(&super::words::no_taw())
        }
        Some(code) => {
            let _ = std::io::stderr().write_all(&stderr);
            code
        }
        None => unreachable("ssh was stopped before it answered"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn the_leading_options_in_any_order_name_the_machine_and_the_rest() {
        let want = |pretty| {
            Some(Leading {
                machine: "studio".to_string(),
                pretty,
                rest: words("agent list"),
            })
        };
        assert_eq!(leading(&words("--machine studio agent list")), want(false));
        assert_eq!(leading(&words("--machine=studio agent list")), want(false));
        assert_eq!(
            leading(&words("--pretty --machine studio agent list")),
            want(true)
        );
        assert_eq!(
            leading(&words("--machine studio --pretty agent list")),
            want(true)
        );
        assert_eq!(
            leading(&words("--pretty --machine=studio agent list")),
            want(true)
        );
    }

    #[test]
    fn a_line_that_names_a_machine_is_known_by_its_leading_options_alone() {
        for line in [
            "--machine studio agent list",
            "--machine=studio agent list",
            "--pretty --machine studio agent list",
            "--machine= agent list",
            "--machine",
            "--machine --pretty agent list",
        ] {
            assert!(names_machine(&words(line)), "{line}");
        }
        assert!(names_machine(&["--machine".to_string(), String::new()]));
        for line in [
            "",
            "agent list",
            "--pretty agent list",
            "agent list --machine studio",
            "--other --machine studio agent list",
            "--machines=studio agent list",
        ] {
            assert!(!names_machine(&words(line)), "{line}");
        }
    }

    #[test]
    fn what_follows_the_verb_is_the_other_machines_and_never_read_here() {
        let got = leading(&words("--machine studio agent list --machine pi --pretty")).unwrap();
        assert_eq!(got.rest, words("agent list --machine pi --pretty"));
        assert!(!got.pretty);
    }

    #[test]
    fn a_line_clap_should_answer_is_left_to_it() {
        for line in [
            "agent list",
            "--pretty agent list",
            "--machine",
            "--machine --pretty agent list",
            "--machine studio",
            "--machine studio --help",
            "--machine studio -h",
            "--machine studio --machine pi agent list",
            "--machine=studio --machine=pi agent list",
            "--machine= agent list",
            "--machine=studio",
            "--other --machine studio agent list",
        ] {
            assert_eq!(leading(&words(line)), None, "{line}");
        }
        // An empty name is never a word of `words`.
        assert_eq!(
            leading(&["--machine".to_string(), String::new(), "agent".to_string()]),
            None
        );
    }

    #[test]
    fn the_refused_verbs_have_their_words_and_the_others_none() {
        let refused = |line: &str| refused_verb("studio", &words(line));
        assert_eq!(
            refused("ui").unwrap(),
            "taw --machine studio ui: the popup is that machine's own; use the app: taw app"
        );
        assert_eq!(
            refused("app").unwrap(),
            "taw --machine studio app: run taw app here; it lists studio"
        );
        assert_eq!(
            refused("open w0").unwrap(),
            "taw --machine studio open: needs --json; go in from the app: taw app"
        );
        assert_eq!(
            refused("agent attach %3").unwrap(),
            "taw --machine studio agent attach: use the app: taw app --go studio:<target>"
        );
        assert_eq!(
            refused("machine check x").unwrap(),
            "taw --machine studio machine: taw machine runs here: taw machine check studio"
        );
        for line in [
            "open --json w0",
            "open w0 --json",
            "agent list",
            "agent get %3",
            "workspace list",
            "uis",
            "pane attach",
        ] {
            assert_eq!(refused(line), None, "{line}");
        }
        assert_eq!(refused_verb("studio", &[]), None);
    }

    #[test]
    fn an_option_between_the_words_of_a_verb_does_not_hide_it() {
        let refused = |line: &str| refused_verb("studio", &words(line));
        let attach = "taw --machine studio agent attach: use the app: taw app --go studio:<target>";
        for line in [
            "agent --pretty attach %3",
            "agent attach --pretty %3",
            "agent --pretty --json attach %3",
        ] {
            assert_eq!(refused(line).as_deref(), Some(attach), "{line}");
        }
        assert!(refused("--pretty ui").unwrap().contains("the popup"));
        // The positive half: the same options on a verb that goes over.
        for line in [
            "agent --pretty list",
            "agent list --pretty",
            "--pretty agent get %3",
        ] {
            assert_eq!(refused(line), None, "{line}");
        }
    }

    #[test]
    fn the_unknown_name_says_the_names_there_are_or_where_it_looked() {
        let paths = Paths::rooted_at("/c".into(), "/s".into());
        let machine = |name: &str| Machine {
            name: name.to_string(),
            ssh: name.to_string(),
            taw: None,
        };
        assert_eq!(
            unknown_name("nope", &[machine("studio"), machine("pi")], &paths),
            "taw: no machine called nope in [machines] (yours: studio, pi)"
        );
        assert_eq!(
            unknown_name("nope", &[], &paths),
            "taw: no machine called nope: [machines] in /c/config.toml lists none"
        );
    }
}
