//! taw's own sandbox profile, `default`, run as the program it is: a fake runtime first on `PATH`
//! records what it is asked (`tests/fixtures/sandbox`), so every argument the profile gives the
//! runtime can be read back. No real runtime runs. Everything the profile touches is under one
//! temp dir: its home, the project, the agent's settings, its git identity.

mod common;

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::fake_runtime::FakeRuntime;

/// A home, a project, the profile and a fake runtime, under one temp dir. Every path is the
/// physical one (`/private/var/...` on macOS), as `pwd -P` and git give them.
struct Rig {
    _root: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
    profile: PathBuf,
    tools: PathBuf,
    fake: FakeRuntime,
}

impl Rig {
    fn new() -> Rig {
        let root = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let project = base.join("work").join("api");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude.json"), "{}\n").unwrap();
        std::fs::create_dir_all(&project).unwrap();
        for dir in ["xdg-config", "xdg-state", "xdg-cache", "tmp"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        std::fs::write(
            base.join("gitconfig"),
            "[user]\n\tname = Sam Doe\n\temail = sam@example.com\n",
        )
        .unwrap();
        let profile = base.join("profiles").join("default");
        std::fs::create_dir_all(profile.parent().unwrap()).unwrap();
        std::fs::write(&profile, taw::sandbox::DEFAULT_PROFILE).unwrap();
        crate::common::make_executable(&profile);
        let tools = base.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let fake = FakeRuntime::install(&base);
        Rig {
            _root: root,
            base,
            home,
            project,
            profile,
            tools,
            fake,
        }
    }

    /// A program called `name` among the host's tools, which prints `out` and ends.
    fn tool(&self, name: &str, out: &str) {
        let path = self.tools.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{out}'\n")).unwrap();
        crate::common::make_executable(&path);
    }

    /// The profile with one recipe line uncommented, as a person does in their copy.
    fn with_recipe(&self, line: &str) -> PathBuf {
        let text =
            taw::sandbox::DEFAULT_PROFILE.replace(&format!("#   {line}\n"), &format!("  {line}\n"));
        assert_ne!(
            text,
            taw::sandbox::DEFAULT_PROFILE,
            "no recipe line `{line}`"
        );
        let path = self.base.join("profiles").join(format!(
            "recipe-{}",
            self.base.join("profiles").read_dir().unwrap().count()
        ));
        std::fs::write(&path, text).unwrap();
        crate::common::make_executable(&path);
        path
    }

    /// Runs `profile` with `bash` and `args`, its environment what taw gives a profile in a pane
    /// of the agent named `agent`, then `extra`. The environment is cleared first, so nothing of
    /// the developer's reaches the profile: `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the other
    /// agents' folder variables stay unset, and the profile's defaults under the scratch home are
    /// what it reads (a test that sets one points it under the scratch home); `PATH` holds the
    /// fake runtime, the rig's own tools and the system's folders, never the developer's, so the
    /// version check never runs a real agent.
    fn run_profile(
        &self,
        bash: &str,
        profile: &Path,
        args: &[&str],
        extra: &[(&str, &str)],
    ) -> Output {
        ended(self.command(bash, profile, args, extra))
    }

    /// The command [`Rig::run_profile`] runs, for a test that starts it and does not wait.
    fn command(
        &self,
        bash: &str,
        profile: &Path,
        args: &[&str],
        extra: &[(&str, &str)],
    ) -> Command {
        let agent = match args.first().copied() {
            Some("cursor-agent") => "cursor",
            Some(other) => other,
            None => "",
        };
        let mut cmd = Command::new(bash);
        cmd.arg(profile)
            .args(args)
            .current_dir(&self.project)
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.fake.bin.display(),
                    self.tools.display()
                ),
            )
            .env("HOME", &self.home)
            .env("TERM", "xterm-256color")
            .env("LANG", "en_AU.UTF-8")
            .env("TZ", "UTC")
            .env("HISTFILE", "/dev/null")
            .env("XDG_CONFIG_HOME", self.base.join("xdg-config"))
            .env("XDG_STATE_HOME", self.base.join("xdg-state"))
            .env("XDG_CACHE_HOME", self.base.join("xdg-cache"))
            .env("TMPDIR", self.base.join("tmp"))
            .env("GIT_CONFIG_GLOBAL", self.base.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TAW_SANDBOX_DIR", &self.project)
            .env("TAW_SANDBOX_AGENT", agent)
            .env("TAW_SANDBOX_PROFILE", "default");
        for (name, value) in self.fake.env() {
            cmd.env(name, value);
        }
        for (name, value) in extra {
            cmd.env(name, value);
        }
        cmd
    }

    /// The profile, under macOS's own bash (3.2) where there is one: the strictest that runs it.
    fn run(&self, args: &[&str]) -> Output {
        self.run_profile("/bin/bash", &self.profile, args, &[])
    }

    fn run_with(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.run_profile("/bin/bash", &self.profile, args, extra)
    }

    /// The one `run` the fake was asked, its program first.
    fn the_run(&self) -> Vec<String> {
        let runs = self.fake.runs();
        assert_eq!(runs.len(), 1, "one run, got {runs:?}");
        runs.into_iter().next().unwrap()
    }
}

/// Runs `cmd` to its end and gives what it said, as `Command::output` does, but waits for the
/// profile first and reads its two pipes beside that wait. `output` reads the pipes to their end
/// before it waits, so a watcher that kept them (one without its redirections) would find the
/// profile's pid still there, a zombie nobody has waited for, and would never end: the test would
/// hang instead of failing on what the watcher said.
fn ended(mut cmd: Command) -> Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let read_all = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut said = Vec::new();
            pipe.read_to_end(&mut said).unwrap();
            said
        })
    };
    let stdout = read_all(Box::new(child.stdout.take().unwrap()));
    let stderr = read_all(Box::new(child.stderr.take().unwrap()));
    let status = child.wait().unwrap();
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether `words` holds `part`, in that order and side by side.
fn holds(words: &[String], part: &[&str]) -> bool {
    words
        .windows(part.len())
        .any(|window| window.iter().zip(part).all(|(a, b)| a == b))
}

#[test]
fn an_agent_with_no_recipe_is_refused_with_its_line_and_nothing_runs() {
    let rig = Rig::new();

    let out = rig.run(&["qodercli", "--yolo"]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: no recipe for qodercli; add one to a copy (taw sandbox new)\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn with_no_agent_it_says_how_it_is_run() {
    let rig = Rig::new();

    let out = rig.run(&[]);

    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: usage: default <agent> [the agent's own arguments]\n"
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn claude_is_the_program_the_container_runs_and_taw_recognises_it() {
    let rig = Rig::new();

    let out = rig.run(&["claude"]);

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    assert_eq!(
        run[..6],
        ["docker", "run", "--rm", "--interactive", "--tty", "--init"]
    );
    assert_eq!(
        run[run.len() - 3..],
        [
            "taw-sandbox-default",
            "claude",
            "--dangerously-skip-permissions"
        ]
    );
    let found = taw::detect::identify_container_argv("docker", &run).expect("taw sees an agent");
    assert_eq!((found.runtime, found.kind.as_str()), ("docker", "claude"));
}

/// The wording rule, and the words to avoid: the profile's comments and lines are
/// read by everyone who copies it.
#[test]
fn the_profile_says_nothing_a_person_may_not_read() {
    const BANNED: [(char, &str); 11] = [
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
    const NEVER_SAY: [&str; 13] = [
        "secure",
        "safe",
        "can't escape",
        "isolated",
        "zero risk",
        "untouched",
        "stops prompt injection",
        "stops data theft",
        "never enter the container",
        "vm-level",
        "verified sandboxed",
        "works with every agent",
        "no performance cost",
    ];
    let text = taw::sandbox::DEFAULT_PROFILE;
    for (mark, what) in BANNED {
        assert!(!text.contains(mark), "the profile holds {what}");
    }
    let lower = text.to_lowercase();
    for words in NEVER_SAY {
        assert!(!lower.contains(words), "the profile says {words:?}");
    }
}

/// A profile's name as an image's: lowercase, and each run of `-` and `_` one `-`, since a runtime
/// refuses a name with `_-` or `___` in it (taw refuses a profile's name that ends in either).
#[test]
fn the_image_is_named_for_the_profile_as_a_runtime_takes_a_name() {
    let rig = Rig::new();

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_PROFILE", "Team__b-_2")]);

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    assert_eq!(run[run.len() - 3], "taw-sandbox-team-b-2", "{run:?}");
    // Only the image's name: the label and what the start inside is told keep it as it is.
    assert!(
        holds(&run, &["--label", "taw.profile=Team__b-_2"]),
        "{run:?}"
    );
    assert!(holds(&run, &["-e", "TAW_SB_PROFILE=Team__b-_2"]), "{run:?}");
}

/// The runtime recordings come from a real machine, and these files are public: none may hold the
/// machine's host name or its user's name. This is the net under the scrub that takes them out.
#[test]
fn the_runtime_shapes_hold_no_name_of_this_machine_or_its_user() {
    let said = |program: &str, args: &[&str]| {
        let out = Command::new(program).args(args).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let host = said("hostname", &[]);
    let short = host.split('.').next().unwrap_or_default().to_string();
    let names: Vec<String> = [host.clone(), short, said("id", &["-un"])]
        .into_iter()
        .filter(|name| name.len() >= 3)
        .collect();
    // The recordings only: the fake's source and the folder's README are no runtime's output.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let recorded = path.extension().is_some_and(|ext| ext == "txt")
            || path.file_name().is_some_and(|name| name == "ps.jsonl");
        if !recorded {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let held = names_held(&text, &names);
        assert!(
            held.is_empty(),
            "{} holds {held:?}, a name of this machine or of its user",
            path.display()
        );
    }
}

/// The words the runtime shapes are made of as the Docker CLI documentation prints them, in lower
/// case: tests/fixtures/sandbox's .txt files and ps.jsonl as they were written from it. Kept here,
/// not read from the files, so that a name a recording brings in is never one of them.
const DOCUMENTED_WORDS: [&str; 115] = [
    "0b",
    "0s",
    "2s",
    "3f2a1b4c5d6e",
    "412b",
    "4s",
    "654gib",
    "8s",
    "aarch64",
    "ago",
    "api",
    "apt",
    "architecture",
    "at",
    "build",
    "building",
    "ca",
    "cannot",
    "certificates",
    "client",
    "com",
    "command",
    "compose",
    "connect",
    "containers",
    "context",
    "cpus",
    "createdat",
    "curl",
    "daemon",
    "db",
    "default",
    "definition",
    "desktop",
    "dir",
    "docker",
    "dockerfile",
    "done",
    "driver",
    "entrypoint",
    "error",
    "exporting",
    "for",
    "from",
    "get",
    "ghcr",
    "git",
    "hours",
    "id",
    "image",
    "images",
    "install",
    "instance",
    "internal",
    "io",
    "iproute2",
    "iptables",
    "is",
    "jdx",
    "labels",
    "library",
    "linux",
    "load",
    "localvolumes",
    "machine",
    "memory",
    "metadata",
    "mise",
    "mounts",
    "name",
    "names",
    "naming",
    "networks",
    "no",
    "operating",
    "ostype",
    "paused",
    "pgdata",
    "podman",
    "ports",
    "postgres",
    "project",
    "recommends",
    "response",
    "run",
    "running",
    "runningfor",
    "sandbox",
    "server",
    "service",
    "sh",
    "size",
    "sock",
    "state",
    "status",
    "stopped",
    "such",
    "system",
    "taw",
    "tcp",
    "the",
    "to",
    "total",
    "transferring",
    "unix",
    "up",
    "update",
    "using",
    "utc",
    "util",
    "var",
    "version",
    "with",
    "working",
    "y",
];

/// The names in `names` that `text` holds as a whole word, in any case, but for a name made only of
/// words the shapes are made of (a machine called docker, or docker-desktop): it cannot be told
/// from the shapes by a word, and is not looked for.
fn names_held<'a>(text: &str, names: &'a [String]) -> Vec<&'a str> {
    let documented = |name: &str| {
        name.to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .all(|part| DOCUMENTED_WORDS.contains(&part))
    };
    names
        .iter()
        .filter(|name| !documented(name))
        .filter(|name| {
            regex::Regex::new(&format!("(?i)\\b{}\\b", regex::escape(name)))
                .unwrap()
                .is_match(text)
        })
        .map(String::as_str)
        .collect()
}

/// A machine named for words the shapes are made of (server, client, desktop, docker, linux, or
/// docker-desktop) is not taken for a leak: its name is in the shapes as Docker prints them. Any
/// other name is still found.
#[test]
fn a_machine_named_for_words_of_the_shapes_is_not_taken_for_a_leak() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox");
    let info = std::fs::read_to_string(dir.join("info.txt")).unwrap();
    let names = [
        "server",
        "client",
        "desktop",
        "docker",
        "linux",
        "docker-desktop",
        "testhost",
    ]
    .map(String::from);

    assert_eq!(names_held(&info, &names), Vec::<&str>::new());
    assert_eq!(
        names_held(&format!("{info} Name: Testhost\n"), &names),
        ["testhost"]
    );
}

/// One recipe row: the agent's command, what installs it inside, its settings folders (under the
/// home), and the words of its switch.
const RECIPES: [(&str, &str, &[&str], &[&str]); 12] = [
    (
        "claude",
        "claude",
        &[".claude", ".claude.json"],
        &["--dangerously-skip-permissions"],
    ),
    (
        "codex",
        "npm:@openai/codex",
        &[".codex"],
        &["--dangerously-bypass-approvals-and-sandbox"],
    ),
    (
        "gemini",
        "npm:@google/gemini-cli",
        &[".gemini"],
        &["--yolo"],
    ),
    (
        "opencode",
        "opencode",
        &[".config/opencode", ".local/share/opencode"],
        &["--auto"],
    ),
    (
        "copilot",
        "npm:@github/copilot",
        &[".copilot"],
        &["--allow-all"],
    ),
    (
        "cursor-agent",
        "installer:https://cursor.com/install",
        &[".cursor"],
        &["--force"],
    ),
    (
        "amp",
        "npm:@sourcegraph/amp",
        &[".config/amp"],
        &["--dangerously-allow-all"],
    ),
    ("qwen", "npm:@qwen-code/qwen-code", &[".qwen"], &["--yolo"]),
    (
        "droid",
        "installer:https://app.factory.ai/cli",
        &[".factory"],
        &["--auto", "high"],
    ),
    (
        "kimi",
        "installer:https://code.kimi.com/kimi-code/install.sh",
        &[".kimi-code"],
        &["--auto"],
    ),
    (
        "pi",
        "npm:@mariozechner/pi-coding-agent",
        &[".pi/agent"],
        &[],
    ),
    (
        "letta",
        "npm:@letta-ai/letta-code",
        &[".letta"],
        &["--yolo"],
    ),
];

#[test]
fn each_recipes_agent_gets_its_install_its_folders_and_its_switch() {
    for (command, install, folders, switch) in RECIPES {
        let rig = Rig::new();

        let out = rig.run(&[command, "--model", "x"]);

        assert!(out.status.success(), "{command}: {}", stderr(&out));
        let run = rig.the_run();
        assert!(
            holds(&run, &["-e", &format!("TAW_SB_INSTALL={install}")]),
            "{command}: {run:?}"
        );
        for folder in folders {
            let path = rig.home.join(folder).display().to_string();
            assert!(
                holds(&run, &["-v", &format!("{path}:{path}")]),
                "{command}: {folder} in {run:?}"
            );
        }
        let at = run
            .iter()
            .position(|word| word == "taw-sandbox-default")
            .unwrap();
        let mut after = vec![command];
        after.extend_from_slice(switch);
        after.extend(["--model", "x"]);
        assert_eq!(run[at + 1..], after[..], "{command}");
    }
}

/// A recipe with no settings folders, as a copy may add one: macOS's own bash (3.2) stops on an
/// empty array under `set -u` unless it is expanded with care. The agent starts with no mount but
/// the project's.
#[test]
fn a_recipe_with_no_settings_folders_runs_under_macos_own_bash() {
    let rig = Rig::new();
    let text = taw::sandbox::DEFAULT_PROFILE.replace(
        "  letta)    install=npm:@letta-ai/letta-code\n",
        "  nofolders) install=npm:nofolders\n            folders=()\n            skip=() ;;\n  letta)    install=npm:@letta-ai/letta-code\n",
    );
    assert_ne!(text, taw::sandbox::DEFAULT_PROFILE);
    let copy = rig.base.join("profiles").join("mine");
    std::fs::write(&copy, text).unwrap();
    crate::common::make_executable(&copy);

    let out = rig.run_profile("/bin/bash", &copy, &["nofolders", "--x"], &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    assert!(
        holds(&run, &["-e", "TAW_SB_INSTALL=npm:nofolders"]),
        "{run:?}"
    );
    let at = run
        .iter()
        .position(|word| word == "taw-sandbox-default")
        .unwrap();
    assert_eq!(run[at + 1..], ["nofolders", "--x"]);
    // The project and the volumes every start has, and no settings folder.
    let project = rig.project.display().to_string();
    let mounts: Vec<&str> = run
        .windows(2)
        .filter(|pair| pair[0] == "-v")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        mounts,
        [
            format!("{project}:{project}"),
            format!(
                "{}:{project}/node_modules",
                volume("node_modules", &rig.project.join("node_modules"))
            ),
            format!(
                "{}:{project}/.venv",
                volume("venv", &rig.project.join(".venv"))
            ),
            "taw-sandbox-mise:/opt/taw/mise".to_string(),
            format!("taw-sandbox-cache:{}/.cache", rig.home.display()),
        ],
        "{run:?}"
    );
}

#[test]
fn the_agents_own_arguments_arrive_after_its_switch_exactly_as_given() {
    let rig = Rig::new();

    let out = rig.run(&["claude", "--resume", "4b1f 2c", "$(touch pwned)", "it's"]);

    assert!(out.status.success(), "{}", stderr(&out));
    // The run's own record, read raw: the watcher's `stop` can land in the log after it.
    let log = std::fs::read_to_string(&rig.fake.log).unwrap();
    let record = log
        .split("\x1e\n")
        .find(|call| call.starts_with("docker\x1frun\x1f") && !call.contains("\x1f-d\x1f"))
        .expect("the run in the log");
    assert!(
        record.ends_with("claude\x1f--dangerously-skip-permissions\x1f--resume\x1f4b1f 2c\x1f$(touch pwned)\x1fit's\x1f"),
        "{record:?}"
    );
    assert!(!rig.project.join("pwned").exists());
}

#[test]
fn ask_first_takes_the_switch_off_and_nothing_else() {
    let rig = Rig::new();
    let asking = rig.with_recipe("ask_first=yes");

    rig.run(&["claude", "--resume", "x"]);
    let out = rig.run_profile("/bin/bash", &asking, &["claude", "--resume", "x"], &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let runs = rig.fake.runs();
    let mut expected = normalised(&runs[0]);
    expected.retain(|word| word != "--dangerously-skip-permissions");
    assert_eq!(normalised(&runs[1]), expected);
}

/// The recipe switches come from the profile's own lines and nothing else: with every recipe line
/// commented, each recipe's name exported in the environment changes neither the run nor what the
/// profile says.
#[test]
fn a_recipe_name_in_the_environment_changes_nothing() {
    // Warm: neither start builds the image, so the two say the same lines.
    let rig = Rig::new().warm();
    let plain = rig.run(&["claude"]);

    let out = rig.run_with(
        &["claude"],
        &[
            ("ask_first", "yes"),
            ("settings", "own"),
            ("block_local_network", "yes"),
            ("open_ports", "8317"),
            ("docker", "full"),
            ("runtime", "podman"),
            ("tools", "node@22 uv"),
            ("pass", "OPENAI_API_KEY"),
        ],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    let runs = rig.fake.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(normalised(&runs[1]), normalised(&runs[0]));
    assert_eq!(stderr(&out), stderr(&plain));
}

#[test]
fn the_agent_inside_is_the_version_this_machine_runs_or_the_latest() {
    let rig = Rig::new();
    rig.tool("claude", "2.1.286 (Claude Code)");
    rig.run(&["claude"]);
    assert!(holds(&rig.the_run(), &["-e", "TAW_SB_VERSION=2.1.286"]));

    let without = Rig::new();
    without.run(&["claude"]);
    assert!(holds(&without.the_run(), &["-e", "TAW_SB_VERSION=latest"]));

    // An agent its own installer puts in takes no version.
    let cursor = Rig::new();
    cursor.tool("cursor-agent", "2026.09.30-abc1234");
    cursor.run(&["cursor-agent"]);
    assert!(holds(&cursor.the_run(), &["-e", "TAW_SB_VERSION=latest"]));
}

#[test]
fn claudes_config_dir_is_its_one_folder_when_it_is_set() {
    let rig = Rig::new();
    let work = rig.home.join(".claude-work");
    std::fs::create_dir_all(&work).unwrap();

    rig.run_with(
        &["claude"],
        &[("CLAUDE_CONFIG_DIR", work.to_str().unwrap())],
    );

    let run = rig.the_run();
    let path = work.display().to_string();
    assert!(holds(&run, &["-v", &format!("{path}:{path}")]), "{run:?}");
    // And Claude inside is told where it is: it reads $HOME/.claude otherwise, which is not there.
    assert!(
        holds(&run, &["-e", &format!("CLAUDE_CONFIG_DIR={path}")]),
        "{run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains(".claude.json")),
        "{run:?}"
    );
}

/// The agents whose settings folder a variable can name, and the variable.
const SETTINGS_VARIABLES: [(&str, &str); 7] = [
    ("claude", "CLAUDE_CONFIG_DIR"),
    ("codex", "CODEX_HOME"),
    ("copilot", "COPILOT_HOME"),
    ("cursor-agent", "CURSOR_CONFIG_DIR"),
    ("qwen", "QWEN_HOME"),
    ("kimi", "KIMI_CODE_HOME"),
    ("pi", "PI_CODING_AGENT_DIR"),
];

/// With the variable set, its folder is mounted and the variable goes in with its value, beside
/// HOME, so that the agent inside finds its settings, its login and taw's hook where they are
/// mounted; unset, it does not go in.
#[test]
fn a_settings_folder_named_by_a_variable_goes_in_with_that_variable_beside_home() {
    for (command, variable) in SETTINGS_VARIABLES {
        let rig = Rig::new();
        let folder = rig.home.join("other settings").join(command);
        let path = folder.display().to_string();

        let out = rig.run_with(&[command], &[(variable, path.as_str())]);

        assert!(out.status.success(), "{command}: {}", stderr(&out));
        let run = rig.the_run();
        let home = format!("HOME={}", rig.home.display());
        let told = format!("{variable}={path}");
        assert!(
            holds(&run, &["-v", &format!("{path}:{path}")]),
            "{command}: {run:?}"
        );
        assert!(
            holds(&run, &["-e", &home, "-e", &told]),
            "{command}: {run:?}"
        );
        assert!(folder.is_dir(), "{command}: {path} was not made");

        let unset = Rig::new();
        unset.run(&[command]);
        let run = unset.the_run();
        let named = format!("{variable}=");
        assert!(
            !run.iter().any(|word| word.starts_with(&named)),
            "{command}: {run:?}"
        );
    }
}

/// The two accounts: a copy of the default whose first line sets CLAUDE_CONFIG_DIR, not
/// exported, shares that account's folder and tells Claude inside where it is.
#[test]
fn a_copy_that_sets_claudes_config_dir_on_its_first_line_starts_that_account() {
    let rig = Rig::new();
    let text = taw::sandbox::DEFAULT_PROFILE.replacen(
        "#!/usr/bin/env bash\n",
        "#!/usr/bin/env bash\nCLAUDE_CONFIG_DIR=~/.claude-work\n",
        1,
    );
    assert_ne!(text, taw::sandbox::DEFAULT_PROFILE);
    let work = rig.base.join("profiles").join("work");
    std::fs::write(&work, text).unwrap();
    crate::common::make_executable(&work);

    let out = rig.run_profile("/bin/bash", &work, &["claude"], &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    let path = rig.home.join(".claude-work").display().to_string();
    assert!(holds(&run, &["-v", &format!("{path}:{path}")]), "{run:?}");
    assert!(
        holds(&run, &["-e", &format!("CLAUDE_CONFIG_DIR={path}")]),
        "{run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains(".claude.json")),
        "{run:?}"
    );
}

#[test]
fn a_settings_folder_that_is_not_there_is_made_and_a_settings_file_starts_empty() {
    let rig = Rig::new();
    std::fs::remove_file(rig.home.join(".claude.json")).unwrap();

    rig.run(&["codex"]);
    rig.run(&["claude"]);

    assert!(rig.home.join(".codex").is_dir());
    assert_eq!(
        std::fs::read_to_string(rig.home.join(".claude.json")).unwrap(),
        "{}\n"
    );
}

/// The run's arguments with the container's name, which holds a random id, put as `<name>`.
fn normalised(run: &[String]) -> Vec<String> {
    let mut words = run.to_vec();
    if let Some(at) = words.iter().position(|word| word == "--name") {
        words[at + 1] = "<name>".to_string();
    }
    words
}

#[test]
fn the_container_is_named_for_the_project_and_labelled_with_the_profile_and_the_pane() {
    let rig = Rig::new();

    rig.run_with(&["claude"], &[("TAW_SANDBOX_PANE", "%31")]);

    let run = rig.the_run();
    let at = run.iter().position(|word| word == "--name").unwrap();
    let name = &run[at + 1];
    assert!(
        name.starts_with("taw-api-") && name.len() == "taw-api-".len() + 6,
        "{name}"
    );
    assert!(
        name["taw-api-".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()),
        "{name}"
    );
    assert!(holds(&run, &["--label", "taw.profile=default"]));
    assert!(holds(&run, &["--label", "taw.pane=%31"]));
}

#[test]
fn a_project_folder_whose_name_a_container_cannot_take_is_spelled_with_dashes() {
    let rig = Rig::new();
    let odd = rig.base.join("work").join("my api (old)");
    std::fs::create_dir_all(&odd).unwrap();

    rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", odd.to_str().unwrap())]);

    let run = rig.the_run();
    let name = &run[run.iter().position(|word| word == "--name").unwrap() + 1];
    assert!(name.starts_with("taw-my-api--old--"), "{name}");
}

/// The project is shared read-write, so the home folder, a folder that holds it and / are no
/// project: each is refused with its line before anything runs or is made. A folder inside the
/// home is a project like any other.
#[test]
fn the_home_folder_a_folder_above_it_and_the_root_are_refused_as_a_project() {
    let rig = Rig::new();
    let line = |shown: &str, what: &str| {
        format!(
            "taw sandbox default: {shown} {what}, not a project, and the sandbox would share all of it; cd into a project first\n"
        )
    };
    let base = rig.base.display().to_string();
    let cases = [
        (rig.home.clone(), line("~", "is your home folder")),
        (rig.base.clone(), line(&base, "holds your home folder")),
        (PathBuf::from("/"), line("/", "holds your home folder")),
    ];
    for (folder, said) in cases {
        let out = rig.run_with(&["codex"], &[("TAW_SANDBOX_DIR", folder.to_str().unwrap())]);

        assert_eq!(
            out.status.code(),
            Some(1),
            "{}: {}",
            folder.display(),
            stderr(&out)
        );
        assert_eq!(stderr(&out), said);
        assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
        assert!(
            !rig.home.join(".codex").exists(),
            "{} made a settings folder",
            folder.display()
        );
    }

    let inside = rig.home.join("work").join("api");
    std::fs::create_dir_all(&inside).unwrap();
    let out = rig.run_with(&["codex"], &[("TAW_SANDBOX_DIR", inside.to_str().unwrap())]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(rig.fake.runs().len(), 1);
}

/// A project folder, or a checkout's top, that is or holds a folder whose files run on this machine or
/// reach into it is refused before anything runs or is made, in print mode too, with its line: tmux's
/// socket folder (from TMUX, and outside tmux from TMUX_TMPDIR), taw's config folder, taw's state
/// folder (TAW_SANDBOX_REPORT's root, else TAW_STATE_DIR), ~/.config as a checkout's top, and
/// ~/.local and its share folder. A folder inside ~/.config or ~/.local is a project like any other;
/// one inside taw's folders or tmux's is not (the next test).
#[test]
fn a_project_that_is_or_holds_a_folder_that_runs_here_is_refused() {
    let rig = Rig::new();
    let refused = |what: String| {
        format!(
            "taw sandbox default: {what}, and the sandbox would share it; open a project inside it, or make a profile of your own that shares only what it needs\n"
        )
    };
    let work = rig.base.join("work");
    let made = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        dir.display().to_string()
    };
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let socket = made(&work.join("tmux-sock"));
    let outside = made(&work.join(format!("tmux-{}", uid.trim())));
    let config = made(&rig.project.join(".taw"));
    let reported = rig.project.join("st/sandbox/panes/s/%1");
    made(&reported);
    let state = made(&rig.project.join("st2"));
    let local = made(&rig.home.join(".local"));
    made(&rig.home.join(".local/share"));
    let work_shown = work.display().to_string();
    let project = rig.project.display().to_string();
    // Each case: the project folder, what is set for it, and the line it is refused with.
    type Case = (String, Vec<(&'static str, String)>, String);
    let cases: Vec<Case> = vec![
        (
            work_shown.clone(),
            vec![("TMUX", format!("{socket}/default,4242,0"))],
            refused(format!(
                "{work_shown} holds tmux's socket folder ({socket})"
            )),
        ),
        (
            work_shown.clone(),
            vec![("TMUX_TMPDIR", work_shown.clone())],
            refused(format!(
                "{work_shown} holds tmux's socket folder ({outside})"
            )),
        ),
        (
            project.clone(),
            vec![("TAW_CONFIG_DIR", config.clone())],
            refused(format!("{project} holds taw's config folder ({config})")),
        ),
        (
            project.clone(),
            vec![("TAW_SANDBOX_REPORT", reported.display().to_string())],
            refused(format!(
                "{project} holds taw's state folder ({}/st)",
                rig.project.display()
            )),
        ),
        (
            project.clone(),
            vec![("TAW_STATE_DIR", state.clone())],
            refused(format!("{project} holds taw's state folder ({state})")),
        ),
        (
            local.clone(),
            vec![],
            refused("~/.local is the folder of your own programs and their data".to_string()),
        ),
        (
            rig.home.join(".local/share").display().to_string(),
            vec![],
            refused("~/.local/share is the folder of your programs' data".to_string()),
        ),
    ];
    for (folder, env, said) in &cases {
        let mut extra: Vec<(&str, &str)> = vec![
            ("TAW_SANDBOX_DIR", folder.as_str()),
            ("TAW_SANDBOX_PRINT", "1"),
        ];
        extra.extend(env.iter().map(|(name, value)| (*name, value.as_str())));

        let out = rig.run_with(&["claude"], &extra);

        assert_eq!(
            out.status.code(),
            Some(1),
            "{folder} {env:?}: {}",
            stderr(&out)
        );
        assert_eq!(stderr(&out), *said, "{folder} {env:?}");
        assert!(out.stdout.is_empty(), "{folder} {env:?}: printed a command");
    }

    // ~/.config as a checkout's top: a dotfiles checkout, opened from a folder in it.
    let config_home = rig.home.join(".config");
    let nvim = config_home.join("nvim");
    std::fs::create_dir_all(&nvim).unwrap();
    git(&config_home, &["init", "-q"]);
    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_DIR", nvim.to_str().unwrap()),
            ("TAW_SANDBOX_PRINT", "1"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: this folder is in a git checkout of ~/.config, which is the folder of your programs' settings, and the sandbox would share it; open a project with a git checkout of its own, or make a profile of your own that shares only what it needs\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());

    // A folder inside one of them, with no checkout of its own around it, is a project.
    std::fs::remove_dir_all(config_home.join(".git")).unwrap();
    for inside in [nvim.clone(), rig.home.join(".local/src/app")] {
        std::fs::create_dir_all(&inside).unwrap();
        let out = rig.run_with(
            &["claude"],
            &[
                ("TAW_SANDBOX_DIR", inside.to_str().unwrap()),
                ("TAW_SANDBOX_PRINT", "1"),
            ],
        );
        assert!(
            out.status.success(),
            "{}: {}",
            inside.display(),
            stderr(&out)
        );
    }
}

/// A project folder, or a checkout's top, that is taw's config or state folder or tmux's socket
/// folder, or is inside one, is refused in print mode too, with a line that says what the sandbox
/// could do from there and offers no project inside it: your profiles folder (from TAW_CONFIG_DIR,
/// through a link to it, and the default under XDG_CONFIG_HOME), taw's own profile and the panes'
/// report folders (TAW_STATE_DIR, and TAW_SANDBOX_REPORT's root), a folder in tmux's socket folder
/// (from TMUX, and outside tmux from TMUX_TMPDIR). GIT_DIR and GIT_WORK_TREE that point into your
/// profiles folder from a project elsewhere point at nothing: the checkout is found from the folder.
/// A folder beside them with a name that starts the same is a project, and a report folder of
/// another shape than taw's names no state folder.
#[test]
fn a_project_inside_taws_folders_or_tmuxs_is_refused() {
    let rig = Rig::new();
    let made = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        dir.display().to_string()
    };
    let refused = |folder: &str, what: &str| {
        format!("taw sandbox default: {folder} {what}; open a project outside it\n")
    };
    const PROFILES: &str = "the sandbox could change your profiles and the hooks taw runs here";
    const STATE: &str =
        "the sandbox could change the profile taw runs here and every pane's report";
    const PANES: &str = "from the sandbox anything could type into every pane you have";
    let config = made(&rig.base.join("cfg"));
    let profiles = made(&rig.base.join("cfg/sandbox"));
    let config_link = rig.base.join("cfg-link");
    std::os::unix::fs::symlink(rig.base.join("cfg"), &config_link).unwrap();
    let default_config = made(&rig.base.join("xdg-config/tmux-agent-wizard"));
    let default_profiles = made(&rig.base.join("xdg-config/tmux-agent-wizard/sandbox"));
    let state = made(&rig.base.join("st"));
    let taws_own = made(&rig.base.join("st/sandbox"));
    let reported = rig.base.join("st2/sandbox/panes/s/%1");
    made(&reported);
    let panes = made(&rig.base.join("st2/sandbox/panes"));
    let sockets = made(&rig.base.join("sock"));
    let in_sockets = made(&rig.base.join("sock/sub"));
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let outside = made(&rig.base.join(format!("tt/tmux-{}", uid.trim())));
    let in_outside = made(&rig.base.join(format!("tt/tmux-{}/sub", uid.trim())));
    let tmpdir = rig.base.join("tt").display().to_string();
    // Each case: the project folder, what is set for it, and the line it is refused with.
    type Case = (String, Vec<(&'static str, String)>, String);
    let cases: Vec<Case> = vec![
        (
            profiles.clone(),
            vec![("TAW_CONFIG_DIR", config.clone())],
            refused(
                &profiles,
                &format!("is in taw's config folder ({config}), and {PROFILES}"),
            ),
        ),
        (
            profiles.clone(),
            vec![("TAW_CONFIG_DIR", config_link.display().to_string())],
            refused(
                &profiles,
                &format!(
                    "is in taw's config folder ({}), and {PROFILES}",
                    config_link.display()
                ),
            ),
        ),
        (
            default_profiles.clone(),
            vec![],
            refused(
                &default_profiles,
                &format!("is in taw's config folder ({default_config}), and {PROFILES}"),
            ),
        ),
        (
            config.clone(),
            vec![("TAW_CONFIG_DIR", config.clone())],
            refused(&config, &format!("is taw's config folder, and {PROFILES}")),
        ),
        (
            taws_own.clone(),
            vec![("TAW_STATE_DIR", state.clone())],
            refused(
                &taws_own,
                &format!("is in taw's state folder ({state}), and {STATE}"),
            ),
        ),
        (
            state.clone(),
            vec![("TAW_STATE_DIR", state.clone())],
            refused(&state, &format!("is taw's state folder, and {STATE}")),
        ),
        (
            panes.clone(),
            vec![("TAW_SANDBOX_REPORT", reported.display().to_string())],
            refused(
                &panes,
                &format!(
                    "is in taw's state folder ({}), and {STATE}",
                    rig.base.join("st2").display()
                ),
            ),
        ),
        (
            in_sockets.clone(),
            vec![("TMUX", format!("{sockets}/default,4242,0"))],
            refused(
                &in_sockets,
                &format!("is in tmux's socket folder ({sockets}), and {PANES}"),
            ),
        ),
        (
            sockets.clone(),
            vec![("TMUX", format!("{sockets}/default,4242,0"))],
            refused(&sockets, &format!("is tmux's socket folder, and {PANES}")),
        ),
        (
            in_outside.clone(),
            vec![("TMUX_TMPDIR", tmpdir.clone())],
            refused(
                &in_outside,
                &format!("is in tmux's socket folder ({outside}), and {PANES}"),
            ),
        ),
    ];
    for (folder, env, said) in &cases {
        let mut extra: Vec<(&str, &str)> = vec![
            ("TAW_SANDBOX_DIR", folder.as_str()),
            ("TAW_SANDBOX_PRINT", "1"),
        ];
        extra.extend(env.iter().map(|(name, value)| (*name, value.as_str())));

        let out = rig.run_with(&["claude"], &extra);

        assert_eq!(
            out.status.code(),
            Some(1),
            "{folder} {env:?}: {}",
            stderr(&out)
        );
        assert_eq!(stderr(&out), *said, "{folder} {env:?}");
        assert!(out.stdout.is_empty(), "{folder} {env:?}: printed a command");
    }

    // GIT_DIR and GIT_WORK_TREE that point into your profiles folder, from a project elsewhere that
    // is no checkout: the profile finds the checkout from the folder, as taw's own git does, and
    // the project runs, with nothing of the profiles folder shared.
    git(&rig.base.join("cfg/sandbox"), &["init", "-q"]);
    let project = rig.project.display().to_string();
    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_DIR", project.as_str()),
            ("TAW_SANDBOX_PRINT", "1"),
            ("TAW_CONFIG_DIR", config.as_str()),
            ("GIT_DIR", &format!("{profiles}/.git")),
            ("GIT_WORK_TREE", profiles.as_str()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(!printed.contains(profiles.as_str()), "{printed}");
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());

    // A report folder of another shape than taw's names no state folder: four folders up from this
    // one hold the project, and it runs.
    let elsewhere = rig.base.join("r/%1");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_DIR", project.as_str()),
            ("TAW_SANDBOX_PRINT", "1"),
            ("TAW_SANDBOX_REPORT", elsewhere.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // Beside them, with a name that starts the same, is a project.
    for beside in [
        rig.base.join("cfg-old/sandbox"),
        rig.base.join("xdg-config/tmux-agent-wizard-old"),
        rig.base.join("sockets"),
    ] {
        std::fs::create_dir_all(&beside).unwrap();
        let out = rig.run_with(
            &["claude"],
            &[
                ("TAW_SANDBOX_DIR", beside.to_str().unwrap()),
                ("TAW_SANDBOX_PRINT", "1"),
                ("TAW_CONFIG_DIR", config.as_str()),
                ("TMUX", format!("{sockets}/default,4242,0").as_str()),
            ],
        );
        assert!(
            out.status.success(),
            "{}: {}",
            beside.display(),
            stderr(&out)
        );
    }
}

/// The home folder is refused however it is spelled, since folders are compared and not their
/// names: through a link to it, and where the disk has them, in another case (a disk that does not
/// tell case apart) and by the data volume's own path (/System/Volumes/Data/...), for the home and
/// for a folder that holds it.
#[test]
fn the_home_folder_is_refused_however_it_is_spelled() {
    let rig = Rig::new();
    let refused = |what: &str| {
        format!(
            "taw sandbox default: {what}, not a project, and the sandbox would share all of it; cd into a project first\n"
        )
    };

    // HOME through a link, the project the folder it links to.
    let link = rig.base.join("link-home");
    std::os::unix::fs::symlink(&rig.home, &link).unwrap();
    let out = rig.run_with(
        &["claude"],
        &[
            ("HOME", link.to_str().unwrap()),
            ("TAW_SANDBOX_DIR", rig.home.to_str().unwrap()),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out), refused("~ is your home folder"));

    let mut spellings = Vec::new();
    let other_case = rig.base.join("HOME");
    if other_case.exists() {
        spellings.push((other_case, "~ is your home folder".to_string()));
    }
    let data = Path::new("/System/Volumes/Data");
    let on_data = |path: &Path| data.join(path.strip_prefix("/").unwrap());
    if on_data(&rig.home).exists() {
        spellings.push((on_data(&rig.home), "~ is your home folder".to_string()));
        let above = on_data(&rig.base);
        spellings.push((
            above.clone(),
            format!("{} holds your home folder", above.display()),
        ));
    }
    for (spelled, what) in spellings {
        let out = rig.run_with(
            &["claude"],
            &[("TAW_SANDBOX_DIR", spelled.to_str().unwrap())],
        );

        assert_eq!(
            out.status.code(),
            Some(1),
            "{}: {}",
            spelled.display(),
            stderr(&out)
        );
        assert_eq!(stderr(&out), refused(&what), "{}", spelled.display());
    }
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// A project folder that is not there, a pane whose folder was taken away say: the profile says
/// so in a line of its own, not bash's cd error.
#[test]
fn a_project_folder_that_is_not_there_is_said_in_the_profiles_own_line() {
    let rig = Rig::new();
    let gone = rig.base.join("work").join("gone");

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", gone.to_str().unwrap())]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        format!(
            "taw sandbox default: the project folder {} is not there; cd into a project first\n",
            gone.display()
        )
    );
    assert!(rig.fake.calls().is_empty());
}

#[test]
fn run_by_hand_it_has_no_pane_label_and_no_report_folder() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    let run = rig.the_run();
    assert!(
        !run.iter().any(|word| word.starts_with("taw.pane=")),
        "{run:?}"
    );
    assert!(!run.iter().any(|word| word.contains("/run/taw")), "{run:?}");
}

#[test]
fn the_report_folder_is_mounted_and_named_inside_when_taw_gives_one() {
    let rig = Rig::new();
    let report = rig
        .base
        .join("state")
        .join("panes")
        .join("default")
        .join("%31");
    std::fs::create_dir_all(&report).unwrap();

    rig.run_with(
        &["claude"],
        &[("TAW_SANDBOX_REPORT", report.to_str().unwrap())],
    );

    let run = rig.the_run();
    assert!(
        holds(
            &run,
            &[
                "-v",
                &format!("{}:/run/taw", report.display()),
                "-e",
                "TAW_REPORT_FILE=/run/taw/session.json",
                "-e",
                "TAW_REPORT_AGENT=claude"
            ]
        ),
        "{run:?}"
    );
}

#[test]
fn the_report_names_the_agents_kind_beside_the_file_and_neither_without_a_report_folder() {
    // The kind as TAW_SANDBOX_AGENT gives it: cursor for cursor-agent, so only that agent's own
    // hook writes the report.
    for (args, kind) in [
        (vec!["claude"], "claude"),
        (vec!["codex"], "codex"),
        (vec!["opencode"], "opencode"),
        (vec!["cursor-agent"], "cursor"),
    ] {
        let rig = Rig::new();
        let report = rig
            .base
            .join("state")
            .join("panes")
            .join("default")
            .join("%31");
        std::fs::create_dir_all(&report).unwrap();

        rig.run_with(&args, &[("TAW_SANDBOX_REPORT", report.to_str().unwrap())]);

        let run = rig.the_run();
        assert!(
            holds(
                &run,
                &[
                    "-e",
                    "TAW_REPORT_FILE=/run/taw/session.json",
                    "-e",
                    &format!("TAW_REPORT_AGENT={kind}")
                ]
            ),
            "{kind}: {run:?}"
        );

        let bare = Rig::new();
        bare.run(&args);
        let run = bare.the_run();
        assert!(
            !run.iter().any(|word| word.contains("TAW_REPORT_")),
            "{kind} with no report folder: {run:?}"
        );
    }
}

#[test]
fn the_container_starts_with_no_capability_but_the_five_its_start_needs_and_gains_none() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    let run = rig.the_run();
    assert!(
        holds(
            &run,
            &[
                "--cap-drop",
                "ALL",
                "--cap-add",
                "NET_ADMIN",
                "--cap-add",
                "SETUID",
                "--cap-add",
                "SETGID",
                "--cap-add",
                "CHOWN",
                "--cap-add",
                "SETPCAP",
                "--security-opt",
                "no-new-privileges",
            ]
        ),
        "{run:?}"
    );
    assert!(
        holds(
            &run,
            &["--detach-keys", "ctrl-],ctrl-],ctrl-]", "--shm-size", "1g"]
        ),
        "{run:?}"
    );
}

#[test]
fn what_is_passed_in_is_the_terminal_the_language_the_time_zone_your_git_name_and_the_names_you_list()
 {
    let rig = Rig::new();
    let listing = rig.with_recipe("pass=\"OPENAI_API_KEY GH_TOKEN\"");

    rig.run(&["claude"]);
    rig.run_profile(
        "/bin/bash",
        &listing,
        &["claude"],
        &[("OPENAI_API_KEY", "sk-test")],
    );

    let runs = rig.fake.runs();
    let run = runs[1].clone();
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
    for part in [
        vec!["-e".to_string(), format!("HOME={}", rig.home.display())],
        vec![
            "-e".into(),
            "TERM".into(),
            "-e".into(),
            "COLORTERM".into(),
            "-e".into(),
            "LANG".into(),
            "-e".into(),
            "TZ=UTC".into(),
        ],
        vec![
            "-e".into(),
            "GIT_AUTHOR_NAME=Sam Doe".into(),
            "-e".into(),
            "GIT_COMMITTER_NAME=Sam Doe".into(),
        ],
        vec![
            "-e".into(),
            "GIT_AUTHOR_EMAIL=sam@example.com".into(),
            "-e".into(),
            "GIT_COMMITTER_EMAIL=sam@example.com".into(),
        ],
        vec![
            "-e".into(),
            "OPENAI_API_KEY".into(),
            "-e".into(),
            "GH_TOKEN".into(),
        ],
        vec![
            "-e".into(),
            "TAW_SB_PROFILE=default".into(),
            "-e".into(),
            format!("TAW_SB_UID={}", uid.trim()),
            "-e".into(),
            format!("TAW_SB_GID={}", gid.trim()),
        ],
    ] {
        let part: Vec<&str> = part.iter().map(String::as_str).collect();
        assert!(holds(&run, &part), "{part:?} in {run:?}");
    }
    // A value is never written: the runtime takes it from the environment by its name.
    assert!(!run.iter().any(|word| word.contains("sk-test")), "{run:?}");
    // The names go in after your git email, and nothing else of the run changes.
    let mut expected = normalised(&runs[0]);
    let at = expected
        .iter()
        .position(|word| word == "TAW_SB_PROFILE=default")
        .unwrap()
        - 1;
    expected.splice(
        at..at,
        ["-e", "OPENAI_API_KEY", "-e", "GH_TOKEN"].map(String::from),
    );
    assert_eq!(normalised(&run), expected);
}

/// With no TZ of its own, the time zone is the one /etc/localtime names, which macOS and most
/// Linux systems keep as a link into the zoneinfo folder; with none there, no TZ goes in.
#[test]
fn with_no_tz_the_time_zone_is_the_one_etc_localtime_names() {
    let rig = Rig::new();
    let zone = std::fs::read_link("/etc/localtime").ok().map(|target| {
        let target = target.display().to_string();
        target
            .rsplit("/zoneinfo/")
            .next()
            .unwrap_or_default()
            .to_string()
    });

    rig.run_with(&["claude"], &[("TZ", "")]);

    let run = rig.the_run();
    match zone {
        Some(zone) => assert!(holds(&run, &["-e", &format!("TZ={zone}")]), "{run:?}"),
        None => assert!(!run.iter().any(|word| word.starts_with("TZ=")), "{run:?}"),
    }
}

#[test]
fn pass_takes_names_and_a_value_written_there_stops_the_start_without_showing_it() {
    let rig = Rig::new();
    let wrong = rig.with_recipe("pass=\"OPENAI_API_KEY GH_TOKEN\"");
    let text = std::fs::read_to_string(&wrong)
        .unwrap()
        .replace("GH_TOKEN\"", "GH_TOKEN=ghp_secret\"");
    std::fs::write(&wrong, text).unwrap();

    let out = rig.run_profile("/bin/bash", &wrong, &["claude"], &[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: pass takes the names of variables, not their values; fix its line in this profile\n"
    );
    assert!(rig.fake.runs().is_empty());
}

/// A copy of the profile whose pass line reads `pass="<names>"`.
fn with_pass(rig: &Rig, names: &str) -> PathBuf {
    let listing = rig.with_recipe("pass=\"OPENAI_API_KEY GH_TOKEN\"");
    let text = std::fs::read_to_string(&listing).unwrap().replace(
        "  pass=\"OPENAI_API_KEY GH_TOKEN\"\n",
        &format!("  pass=\"{names}\"\n"),
    );
    std::fs::write(&listing, text).unwrap();
    listing
}

/// pass takes names, never a pattern: bash matched one against the project's files (GH_* with a
/// file GH_TOKEN_FROM_THE_REPO in the project passed that in), so a pattern stops the start with
/// a line of its own, whether it matches a file there or not.
#[test]
fn a_pattern_in_pass_is_refused_and_never_matched_against_the_projects_files() {
    let rig = Rig::new();
    std::fs::write(rig.project.join("GH_TOKEN_FROM_THE_REPO"), "").unwrap();
    for pattern in ["GH_*", "OPENAI_API_KE?", "[A-Z]*_KEY"] {
        let listing = with_pass(&rig, pattern);

        let out = rig.run_profile("/bin/bash", &listing, &["claude"], &[]);

        assert_eq!(out.status.code(), Some(1), "{pattern}: {}", stderr(&out));
        assert_eq!(
            stderr(&out),
            "taw sandbox default: pass takes the names of variables, not patterns; write each name out in its line in this profile\n",
            "{pattern}"
        );
        assert!(rig.fake.runs().is_empty(), "{pattern}");
    }
}

#[test]
fn tmux_never_reaches_the_container() {
    let rig = Rig::new();

    rig.run_with(
        &["claude"],
        &[
            ("TMUX", "/private/tmp/tmux-501/default,4242,0"),
            ("TMUX_PANE", "%9"),
            ("TAW_SANDBOX_PANE", "%31"),
        ],
    );

    let run = rig.the_run();
    for word in &run {
        assert!(!word.contains("TMUX"), "{word} in {run:?}");
        assert!(!word.contains("tmux-501"), "{word} in {run:?}");
        assert!(!word.contains("%9"), "{word} in {run:?}");
    }
}

/// Nor can pass hand tmux in: TMUX and the TMUX_ variables are refused with a line of their own.
#[test]
fn pass_cannot_name_tmux_or_a_tmux_variable() {
    let rig = Rig::new();
    for name in ["TMUX", "TMUX_PANE", "TMUX_TMPDIR"] {
        let listing = with_pass(&rig, &format!("OPENAI_API_KEY {name}"));

        let out = rig.run_profile(
            "/bin/bash",
            &listing,
            &["claude"],
            &[
                ("TMUX", "/private/tmp/tmux-501/default,4242,0"),
                ("TMUX_PANE", "%9"),
                ("TMUX_TMPDIR", "/private/tmp"),
            ],
        );

        assert_eq!(out.status.code(), Some(1), "{name}: {}", stderr(&out));
        assert_eq!(
            stderr(&out),
            format!(
                "taw sandbox default: pass names {name}, and tmux's variables never go in: whoever holds its socket can type into every pane you have\n"
            )
        );
        assert!(rig.fake.runs().is_empty(), "{name}");
    }
}

#[test]
fn podman_runs_the_same_with_your_user_kept() {
    let rig = Rig::new();
    let podman_profile = rig.with_recipe("runtime=podman");

    rig.run(&["claude"]);
    rig.run_profile("/bin/bash", &podman_profile, &["claude"], &[]);

    let runs = rig.fake.runs();
    let mut expected = normalised(&runs[0]);
    expected[0] = "podman".to_string();
    let at = expected
        .iter()
        .position(|word| word == "--security-opt")
        .unwrap()
        + 2;
    expected.splice(at..at, ["--userns".to_string(), "keep-id".to_string()]);
    assert_eq!(normalised(&runs[1]), expected);
}

#[test]
fn tools_are_handed_to_the_start_inside_as_mise_spells_them() {
    let rig = Rig::new();
    let with_tools = rig.with_recipe("tools=\"node@22 uv\"");

    rig.run(&["claude"]);
    rig.run_profile("/bin/bash", &with_tools, &["claude"], &[]);

    // The tools go in, and nothing else of the run changes.
    let runs = rig.fake.runs();
    let mut expected = normalised(&runs[0]);
    let at = expected
        .iter()
        .position(|word| word == "TAW_SB_TOOLS=")
        .unwrap();
    expected[at] = "TAW_SB_TOOLS=node@22 uv".to_string();
    assert_eq!(normalised(&runs[1]), expected);
}

#[test]
fn a_runtime_this_profile_does_not_know_is_refused() {
    let rig = Rig::new();
    let other = rig.with_recipe("runtime=podman");
    let text = std::fs::read_to_string(&other)
        .unwrap()
        .replace("  runtime=podman\n", "  runtime=nerdctl\n");
    std::fs::write(&other, text).unwrap();

    let out = rig.run_profile("/bin/bash", &other, &["claude"], &[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: runtime=nerdctl is not one this profile knows (docker or podman)\n"
    );
}

impl Rig {
    /// The rig after one start, as a machine that has started a sandbox before: the image is
    /// built. That start's calls go to a log of their own, its watcher's
    /// later `stop` included, so `calls`, `runs` and `the_run` see only what the test starts.
    fn warm(self) -> Rig {
        let aside = self.base.join("fake-runtime-warm.log");
        let out = self.run_with(
            &["claude"],
            &[("FAKE_RUNTIME_LOG", aside.to_str().unwrap())],
        );
        assert!(out.status.success(), "warming the rig: {}", stderr(&out));
        self
    }
}

#[test]
fn the_first_lines_say_where_it_runs_what_it_reaches_and_how_it_asks() {
    let rig = Rig::new().warm();
    let home_project = rig.home.join("work").join("api");
    std::fs::create_dir_all(&home_project).unwrap();

    let out = rig.run_with(
        &["claude"],
        &[("TAW_SANDBOX_DIR", home_project.to_str().unwrap())],
    );

    assert_eq!(
        stderr(&out),
        "taw sandbox default: claude in docker, in ~/work/api\n\
         \x20 reaches the internet and your local network, not this machine; no docker inside\n\
         \x20 uses your own claude settings; runs commands without asking\n"
    );

    // The rig's own project is outside its home, so it is shown whole.
    let asking = rig.with_recipe("ask_first=yes");
    let out = rig.run_profile("/bin/bash", &asking, &["codex"], &[]);
    assert_eq!(
        stderr(&out),
        format!(
            "taw sandbox default: codex in docker, in {}\n\
             \x20 reaches the internet and your local network, not this machine; no docker inside\n\
             \x20 uses your own codex settings; asks before each command\n",
            rig.project.display()
        )
    );
}

/// The version check gives the agent no input: one whose --version reads it would wait for the
/// pane's, or take what is typed there.
#[test]
fn the_version_check_gives_the_agent_no_input() {
    use std::os::unix::process::CommandExt;

    let rig = Rig::new();
    let claude = rig.tools.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nread -r line\nprintf '%s\\n' '2.1.286 (Claude Code)'\n",
    )
    .unwrap();
    crate::common::make_executable(&claude);
    let mut cmd = rig.command("/bin/bash", &rig.profile, &["claude"], &[]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = cmd.spawn().unwrap();
    // A pane nobody types in: open, and nothing comes.
    let _input = child.stdin.take();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
            panic!("the version check waited for the pane's input");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(holds(&rig.the_run(), &["-e", "TAW_SB_VERSION=2.1.286"]));
}

/// ask_first takes off a switch that is there: pi has none and never asks, so with the recipe line
/// it still runs commands without asking, and its first lines say so.
#[test]
fn ask_first_on_an_agent_that_never_asks_still_says_it_runs_without_asking() {
    let rig = Rig::new().warm();
    let asking = rig.with_recipe("ask_first=yes");

    let out = rig.run_profile("/bin/bash", &asking, &["pi"], &[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).ends_with("  uses your own pi settings; runs commands without asking\n"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn print_mode_prints_the_command_and_runs_nothing() {
    let rig = Rig::new();

    let out = rig.run_with(
        &["claude", "--resume", "a b"],
        &[("TAW_SANDBOX_PRINT", "1")],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    // One line, the command, whose words hold newlines (TAW_SB_OWN's) written as $'\n'.
    assert_eq!(out.stdout.iter().filter(|byte| **byte == b'\n').count(), 1);
    let printed = printed_command(&rig, &out);
    assert_eq!(
        printed[..6],
        ["docker", "run", "--rm", "--interactive", "--tty", "--init"]
    );
    assert_eq!(
        printed[printed.len() - 4..],
        [
            "claude",
            "--dangerously-skip-permissions",
            "--resume",
            "a b"
        ]
    );
}

/// A settings folder or file that cannot be made, in a home that cannot be written: the start ends
/// with the profile's own line, and runs nothing.
#[test]
fn a_settings_folder_that_cannot_be_made_ends_the_start_with_the_profiles_own_line() {
    // Root may write anywhere, so there is nothing to refuse it.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    /// The home made read-only for as long as it lives, and writable again after, so that the rig
    /// can take its folder away even when the test fails.
    struct ReadOnly<'a>(&'a Path);
    impl Drop for ReadOnly<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let rig = Rig::new();
    std::fs::remove_file(rig.home.join(".claude.json")).unwrap();
    std::fs::set_permissions(&rig.home, std::fs::Permissions::from_mode(0o555)).unwrap();
    let _writable_after = ReadOnly(&rig.home);

    for (command, missing) in [("codex", ".codex"), ("claude", ".claude.json")] {
        let out = rig.run(&[command]);

        let path = rig.home.join(missing);
        assert_eq!(out.status.code(), Some(1), "{command}: {}", stderr(&out));
        assert!(
            stderr(&out).ends_with(&format!(
                "taw sandbox default: {} could not be made for {command}'s settings, so the agent was not started\n",
                path.display()
            )),
            "{command}: {}",
            stderr(&out)
        );
        assert!(
            !stderr(&out).contains("Permission denied"),
            "{command}: {}",
            stderr(&out)
        );
        assert!(!path.exists());
    }
    assert!(rig.fake.runs().is_empty(), "{:?}", rig.fake.runs());
}

/// The words a real shell reads from `line`: `shell` runs a script that prints each word of the
/// line, with a scratch home (so that a `~` the line left bare would show) and its folder `in` to
/// run in (where a `$(...)` it ran would leave its file).
fn read_back(shell: &str, line: &[u8], home: &Path, place: &Path) -> Vec<String> {
    let script = place.join("read-back");
    let mut text = b"printf '%s\\0' ".to_vec();
    text.extend_from_slice(line);
    text.push(b'\n');
    std::fs::write(&script, text).unwrap();
    let mut cmd = Command::new(shell);
    if shell.ends_with("zsh") {
        cmd.arg("-f");
    }
    let out = cmd
        .arg(&script)
        .current_dir(place)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("ZDOTDIR", home)
        .env("HISTFILE", "/dev/null")
        .env("LANG", "en_AU.UTF-8")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{shell} could not read {}: {}",
        String::from_utf8_lossy(line),
        stderr(&out)
    );
    let mut words: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .map(str::to_string)
        .collect();
    // Each word ends with a NUL, an empty one too: what follows the last is no word.
    words.pop();
    words
}

/// The one `run` the fake was asked, its program first, read from its log as it is: `calls` drops
/// an empty word, and this keeps it.
fn raw_run(rig: &Rig) -> Vec<String> {
    let log = std::fs::read_to_string(&rig.fake.log).unwrap();
    let runs: Vec<&str> = log
        .split("\x1e\n")
        .filter(|call| call.starts_with("docker\x1frun\x1f") && !call.contains("\x1f-d\x1f"))
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let mut words: Vec<String> = runs[0].split('\x1f').map(str::to_string).collect();
    // Each word ends with \x1f, an empty one too: what follows the last is no word.
    words.pop();
    words
}

/// The command print mode printed, as macOS's own bash reads it: its first line alone, so that a
/// command spread over several lines (a newline in a word printed as it is) cannot be read.
fn printed_command(rig: &Rig, out: &Output) -> Vec<String> {
    let place = rig.base.join("reading");
    std::fs::create_dir_all(&place).unwrap();
    let line = out.stdout.split(|byte| *byte == b'\n').next().unwrap();
    read_back("/bin/bash", line, &rig.home, &place)
}

/// Print mode prints a line that bash 3.2, bash 5 and zsh all read back as the very run a start
/// makes, whichever bash prints it: a word starting with ~ or =, an assignment with ~ in it, a
/// four-byte character, quotes, an empty argument, a space and a $(...) included.
#[test]
fn print_mode_prints_a_line_every_shell_reads_back_as_the_run() {
    let rig = Rig::new();
    let args = [
        "claude",
        "~",
        "~/x",
        "x=~/y",
        "=x",
        "\u{1f4e6}",
        "it's",
        "a'b'c",
        "'",
        "",
        "a b",
        "$(touch pwned)",
    ];
    let mut bashes = vec!["/bin/bash".to_string()];
    if let Ok(found) = which::which("bash") {
        let same = std::fs::canonicalize(&found).ok() == std::fs::canonicalize("/bin/bash").ok();
        if !same {
            bashes.push(found.display().to_string());
        }
    }
    let mut readers = bashes.clone();
    readers.extend(
        which::which("zsh")
            .ok()
            .map(|zsh| zsh.display().to_string()),
    );
    rig.run(&args);
    let started = normalised(&raw_run(&rig));
    let place = rig.base.join("reading");
    std::fs::create_dir_all(&place).unwrap();

    for printer in &bashes {
        let out = rig.run_profile(printer, &rig.profile, &args, &[("TAW_SANDBOX_PRINT", "1")]);
        assert!(out.status.success(), "{printer}: {}", stderr(&out));
        let line = out.stdout.trim_ascii_end();
        for reader in &readers {
            let read = read_back(reader, line, &rig.home, &place);
            assert_eq!(
                normalised(&read),
                started,
                "printed by {printer}, read by {reader}: {}",
                String::from_utf8_lossy(line)
            );
        }
    }
    assert!(
        !place.join("pwned").exists(),
        "a shell ran the $(...) it was printed"
    );
}

/// Print mode runs nothing and makes nothing: a settings folder that is not there yet stays
/// missing, its mount is printed all the same, and what is printed is what a start then runs.
#[test]
fn print_mode_makes_no_missing_settings_folder_and_prints_what_a_start_runs() {
    let rig = Rig::new();
    let codex = rig.home.join(".codex");

    let out = rig.run_with(&["codex"], &[("TAW_SANDBOX_PRINT", "1")]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!codex.exists(), "print mode made {}", codex.display());
    let printed = printed_command(&rig, &out);
    let mount = format!("{}:{}", codex.display(), codex.display());
    assert!(holds(&printed, &["-v", &mount]), "{printed:?}");
    rig.run(&["codex"]);
    assert_eq!(normalised(&printed), normalised(&raw_run(&rig)));
    assert!(codex.is_dir());
}

/// The same for a settings file: print mode leaves it missing, and a start makes it empty.
#[test]
fn print_mode_makes_no_missing_settings_file_and_prints_what_a_start_runs() {
    let rig = Rig::new();
    let file = rig.home.join(".claude.json");
    std::fs::remove_file(&file).unwrap();

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_PRINT", "1")]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!file.exists(), "print mode made {}", file.display());
    let printed = printed_command(&rig, &out);
    let mount = format!("{}:{}", file.display(), file.display());
    assert!(holds(&printed, &["-v", &mount]), "{printed:?}");
    rig.run(&["claude"]);
    assert_eq!(normalised(&printed), normalised(&raw_run(&rig)));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}\n");
}

#[test]
fn a_runtime_that_does_not_answer_is_waited_for_said_and_given_up_on_at_the_bound() {
    let rig = Rig::new();
    rig.fake.down(true);
    let began = std::time::Instant::now();

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_WAIT_SECONDS", "2")]);

    assert_eq!(out.status.code(), Some(1));
    assert!(began.elapsed() >= std::time::Duration::from_secs(2));
    let said = stderr(&out);
    assert!(said.contains("taw sandbox default: waiting for docker to answer (start Docker Desktop, OrbStack or Colima)\n"), "{said}");
    assert!(said.ends_with("taw sandbox default: docker did not answer in 2 seconds, so the agent was not started\n"), "{said}");
    assert!(rig.fake.runs().is_empty());
}

/// With no runtime on PATH the profile says so at once, where it waited the whole bound for an
/// answer and suggested starting Docker Desktop, and it makes nothing.
#[test]
fn a_runtime_that_is_not_installed_is_said_at_once_and_nothing_is_made() {
    let rig = Rig::new();
    let without = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", rig.tools.display());
    let began = std::time::Instant::now();

    let out = rig.run_with(&["codex"], &[("PATH", without.as_str())]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(
        said.ends_with(
            "taw sandbox default: docker is not installed (no docker on the PATH), so the agent was not started\n"
        ),
        "{said}"
    );
    assert!(!said.contains("waiting for docker"), "{said}");
    // At once: the old wait took the whole 60 seconds, and a loaded machine can add a few.
    assert!(began.elapsed() < std::time::Duration::from_secs(30));
    assert!(!rig.home.join(".codex").exists());
}

/// The `info` calls the fake was asked.
fn infos(rig: &Rig) -> usize {
    rig.fake
        .calls()
        .iter()
        .filter(|call| call[1..] == ["info"])
        .count()
}

/// The bound is in seconds, however long each try takes: a runtime whose every `info` takes 2
/// seconds to fail made a 3 second bound take 11, with 4 tries.
#[test]
fn the_bound_is_seconds_however_long_each_try_takes() {
    let rig = Rig::new();
    rig.fake.down(true);

    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_WAIT_SECONDS", "3"),
            ("FAKE_RUNTIME_INFO_DELAY_MS", "2000"),
        ],
    );

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).ends_with(
            "taw sandbox default: docker did not answer in 3 seconds, so the agent was not started\n"
        ),
        "{}",
        stderr(&out)
    );
    assert!(infos(&rig) <= 2, "{:?}", rig.fake.calls());
}

/// The clock never ends the wait before its bound. bash's SECONDS counts whole seconds of the
/// wall clock, so a deadline of SECONDS + bound could come up to a second early: a 2 second wait
/// whose first second ran out at once ended after one try. Two stand-ins put the wait where that
/// shows every time: `id`, the last program the profile runs before its wait, waits for the middle
/// of a second, and `sleep` lasts a second or more and ends just after a whole second. The tries
/// then span the 2 seconds. The rig is warmed first, so that the fake's first run on this machine,
/// which macOS checks and slows, is not the wait's.
#[test]
fn the_wait_is_never_cut_short_by_the_clocks_whole_seconds() {
    let rig = Rig::new().warm();
    let _ = std::fs::remove_file(rig.fake.state.join("info-at"));
    for (name, body) in [
        (
            "id",
            "# id, in the middle of a second.\nmy $now = time;\nmy $into = $now - floor($now);\nsleep(($into < 0.5 ? 0.5 : 1.5) - $into);\nexec('/usr/bin/id', @ARGV) or die;\n",
        ),
        (
            "sleep",
            "# A second or more, ending just after a whole second.\nmy $now = time;\nsleep(ceil($now + 1) + 0.001 - $now);\n",
        ),
    ] {
        let path = rig.tools.join(name);
        let script = format!(
            "#!/usr/bin/perl\nuse POSIX qw(ceil floor);\nuse Time::HiRes qw(time sleep);\n{body}"
        );
        std::fs::write(&path, script).unwrap();
        crate::common::make_executable(&path);
    }
    rig.fake.down(true);

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_WAIT_SECONDS", "2")]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let at = rig.fake.info_times();
    assert!(at.len() >= 2, "{at:?}");
    let span = at[at.len() - 1] - at[0];
    assert!(
        span >= 2000,
        "the tries spanned {span} ms of a 2 second wait: {at:?}"
    );
}

/// The wait is 60 seconds unless TAW_SANDBOX_WAIT_SECONDS says otherwise, a try a second: with a
/// stand-in `sleep` that returns at once, the profile tries 61 times and says 60 seconds.
#[test]
fn the_wait_is_sixty_seconds_unless_it_is_set() {
    let rig = Rig::new();
    let sleep = rig.tools.join("sleep");
    std::fs::write(&sleep, "#!/bin/sh\nexit 0\n").unwrap();
    crate::common::make_executable(&sleep);
    rig.fake.down(true);

    let out = rig.run(&["claude"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).ends_with(
            "taw sandbox default: docker did not answer in 60 seconds, so the agent was not started\n"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(infos(&rig), 61);
}

#[test]
fn a_runtime_that_answers_after_a_while_starts_the_agent() {
    let rig = Rig::new();
    // Down the first two times it is asked: the profile says it waits once, asks again in vain,
    // and starts the agent on its third try, however long each try takes.
    rig.fake.down_for(2);

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_WAIT_SECONDS", "10")]);

    assert!(out.status.success(), "{}", stderr(&out));
    let asked = rig
        .fake
        .calls()
        .iter()
        .filter(|call| call[1..] == ["info"])
        .count();
    assert_eq!(asked, 3, "{:?}", rig.fake.calls());
    assert_eq!(
        stderr(&out).matches("waiting for docker to answer").count(),
        1
    );
    assert_eq!(rig.fake.runs().len(), 1);
}

/// How long the watcher's stop may take to come in: it looks once a second, and on a loaded machine
/// (this one has run at a load of 900) the processes it starts can take several more.
const STOP_WITHIN: u64 = 15;

/// Waits up to `seconds` for the fake to have been asked `stop` for the container of the run.
fn stopped_within(rig: &Rig, seconds: u64) -> bool {
    let name = {
        let run = rig.the_run();
        run[run.iter().position(|word| word == "--name").unwrap() + 1].clone()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        if rig
            .fake
            .calls()
            .iter()
            .any(|call| call[1..] == ["stop", "-t", "2", name.as_str()])
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Starts `profile` with `args` as a pane does, holding its run until the test ends it with
/// `end_the_run`, and waits until the fake has been asked that run.
fn start_and_hold(
    rig: &Rig,
    profile: &Path,
    args: &[&str],
    extra: &[(&str, &str)],
) -> std::process::Child {
    let stop = rig.fake.stop_file();
    let mut extra = extra.to_vec();
    extra.push(("FAKE_RUNTIME_STOP", stop.to_str().unwrap()));
    let mut cmd = rig.command("/bin/bash", profile, args, &extra);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().unwrap();
    // A while on a loaded machine: this is the start, not what is tested.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while rig.fake.runs().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        rig.fake.runs().len(),
        1,
        "the profile never handed over to the run"
    );
    child
}

/// Ends the run [`start_and_hold`] holds, as an agent that exits does, and waits for its stop.
fn end_the_run(rig: &Rig, mut child: std::process::Child) {
    std::fs::write(rig.fake.stop_file(), "").unwrap();
    let _ = child.wait();
    assert!(stopped_within(rig, STOP_WITHIN), "{:?}", rig.fake.calls());
}

/// The daemon each verb was sent to, as the fake saw it: (DOCKER_CONTEXT, CONTAINER_CONNECTION).
fn daemon_of(rig: &Rig, verb: &str) -> Vec<(String, String)> {
    rig.fake
        .daemons()
        .into_iter()
        .filter(|(called, _, _)| called == verb)
        .map(|(_, context, connection)| (context, connection))
        .collect()
}

/// The Docker context of the start goes with its run and its watcher's stop: switched while the
/// agent runs (`docker context use`), it would send the stop to another daemon.
#[test]
fn the_run_and_its_stop_go_to_the_docker_context_of_the_start() {
    let rig = Rig::new();
    let child = start_and_hold(&rig, &rig.profile, &["claude"], &[]);
    std::fs::write(rig.fake.state.join("context"), "colima\n").unwrap();
    end_the_run(&rig, child);

    let at_start = || ("desktop-linux".to_string(), String::new());
    assert_eq!(daemon_of(&rig, "run"), [at_start()]);
    assert_eq!(daemon_of(&rig, "stop"), [at_start()]);

    // A DOCKER_HOST of yours already names the daemon for both, and is left as it is.
    let hosted = Rig::new();
    let child = start_and_hold(
        &hosted,
        &hosted.profile,
        &["claude"],
        &[("DOCKER_HOST", "unix:///nowhere/docker.sock")],
    );
    end_the_run(&hosted, child);
    assert_eq!(daemon_of(&hosted, "context"), []);
    assert_eq!(daemon_of(&hosted, "run"), [(String::new(), String::new())]);
}

/// The same for Podman, by its default connection.
#[test]
fn the_run_and_its_stop_go_to_podmans_connection_of_the_start() {
    let rig = Rig::new();
    let podman = rig.with_recipe("runtime=podman");
    let child = start_and_hold(&rig, &podman, &["claude"], &[]);
    std::fs::write(rig.fake.state.join("connection"), "other\n").unwrap();
    end_the_run(&rig, child);

    let at_start = || (String::new(), "podman-machine-default".to_string());
    assert_eq!(daemon_of(&rig, "run"), [at_start()]);
    assert_eq!(daemon_of(&rig, "stop"), [at_start()]);

    let named = Rig::new();
    let podman = named.with_recipe("runtime=podman");
    let child = start_and_hold(
        &named,
        &podman,
        &["claude"],
        &[("CONTAINER_CONNECTION", "work")],
    );
    end_the_run(&named, child);
    assert_eq!(daemon_of(&named, "system"), []);
    assert_eq!(
        daemon_of(&named, "stop"),
        [(String::new(), "work".to_string())]
    );
}

#[test]
fn a_closed_pane_stops_the_container_within_seconds() {
    use std::os::unix::process::CommandExt;

    let rig = Rig::new();
    let stop = rig.fake.stop_file();
    // A `sleep` first on the profile's PATH that notes how long it is asked for: the watcher's
    // look once a second, which the stop's bound alone would let slip to several.
    let sleeps = rig.base.join("sleeps");
    let sleep = rig.tools.join("sleep");
    std::fs::write(
        &sleep,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec /bin/sleep \"$@\"\n",
            sleeps.display()
        ),
    )
    .unwrap();
    crate::common::make_executable(&sleep);
    let mut child = {
        let mut cmd = Command::new("/bin/bash");
        cmd.arg(&rig.profile)
            .arg("claude")
            .current_dir(&rig.project)
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    rig.tools.display(),
                    rig.fake.bin.display()
                ),
            )
            .env("HOME", &rig.home)
            .env("HISTFILE", "/dev/null")
            .env("XDG_CONFIG_HOME", rig.base.join("xdg-config"))
            .env("XDG_STATE_HOME", rig.base.join("xdg-state"))
            .env("XDG_CACHE_HOME", rig.base.join("xdg-cache"))
            .env("TMPDIR", rig.base.join("tmp"))
            .env("GIT_CONFIG_GLOBAL", rig.base.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TAW_SANDBOX_PROFILE", "default")
            .env("TAW_SANDBOX_AGENT", "claude")
            .env("FAKE_RUNTIME_STOP", &stop)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // A group of its own, as the job in front of a pane is: the profile, which becomes
            // docker run, and its watcher.
            .process_group(0);
        // The hang-up at its default, as tmux gives it to the program in a pane, whatever the
        // runner of this test ignores: under nohup every process it starts ignores one, so
        // docker run would outlive the hang-up below. Between fork and exec, so signal(2) only.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
                Ok(())
            });
        }
        for (name, value) in rig.fake.env() {
            cmd.env(name, value);
        }
        cmd.spawn().unwrap()
    };
    // A while on a loaded machine: this is the start, not what is tested.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while rig.fake.runs().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        rig.fake.runs().len(),
        1,
        "the profile never handed over to docker run"
    );

    // What a closed pane, x in the popup or a killed tmux does: a hang-up to the whole group in
    // front, the watcher included, not to docker run alone.
    Command::new("kill")
        .args(["-HUP", "--", &format!("-{}", child.id())])
        .status()
        .unwrap();
    let group = -(child.id() as libc::pid_t);
    // docker run ends on it. One that outlived it would wait for its stop file for ever, and so
    // would this test: it fails instead.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("docker run outlived the hang-up of its pane");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    assert!(stopped_within(&rig, STOP_WITHIN), "{:?}", rig.fake.calls());
    // And the watcher ends with its stop: nothing of the group in front of the pane is left.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(STOP_WITHIN);
    while unsafe { libc::kill(group, 0) } == 0 {
        if std::time::Instant::now() > deadline {
            unsafe { libc::kill(group, libc::SIGKILL) };
            panic!("the watcher was still running after its stop");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // It looked once a second.
    let asked = std::fs::read_to_string(&sleeps).unwrap_or_default();
    assert!(!asked.is_empty(), "the watcher never slept");
    assert!(
        asked.lines().all(|seconds| seconds == "1"),
        "the watcher slept {asked:?}"
    );
}

#[test]
fn an_agent_that_ends_by_itself_is_followed_by_the_same_stop_which_changes_nothing() {
    let rig = Rig::new();

    let out = rig.run(&["claude"]);

    assert!(out.status.success());
    assert!(stopped_within(&rig, STOP_WITHIN), "{:?}", rig.fake.calls());
}

#[test]
fn the_watcher_says_nothing_in_the_pane() {
    let rig = Rig::new().warm();

    let out = rig.run(&["claude"]);
    // The watcher has done its work once its stop is in: whatever it would say is said by then.
    assert!(stopped_within(&rig, STOP_WITHIN), "{:?}", rig.fake.calls());

    assert_eq!(stderr(&out).lines().count(), 3, "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
}

#[test]
fn every_bash_here_runs_it_the_same() {
    use std::io::Write;

    let mut bashes = vec!["/bin/bash".to_string()];
    if let Ok(found) = which::which("bash") {
        // Two names of one file (/usr/bin/bash and /bin/bash where /usr is merged) are one bash.
        let same =
            |a: &Path, b: &Path| std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok();
        if !same(&found, Path::new("/bin/bash")) {
            bashes.push(found.display().to_string());
        }
    }
    if bashes.len() == 1 {
        // Said in the run's output, which a passing test's own output is not: there was nothing
        // to compare the one bash with.
        let _ = writeln!(
            std::io::stderr(),
            "every_bash_here_runs_it_the_same: /bin/bash is the only bash here, so it ran the profile with no other to compare"
        );
    }
    let rig = Rig::new();
    for bash in &bashes {
        let out = rig.run_profile(
            bash,
            &rig.profile,
            &["codex", "resume", "x"],
            &[("TAW_SANDBOX_PANE", "%3")],
        );
        assert!(out.status.success(), "{bash}: {}", stderr(&out));
    }
    let runs: Vec<Vec<String>> = rig.fake.runs().iter().map(|run| normalised(run)).collect();
    assert_eq!(runs.len(), bashes.len());
    assert!(runs.windows(2).all(|pair| pair[0] == pair[1]), "{runs:?}");
}

/// The profile with `from` changed to `to`, as a person edits their copy.
fn edited(rig: &Rig, from: &str, to: &str) -> PathBuf {
    let text = taw::sandbox::DEFAULT_PROFILE.replacen(from, to, 1);
    assert_ne!(
        text,
        taw::sandbox::DEFAULT_PROFILE,
        "no {from:?} in the profile"
    );
    let path = rig.base.join("profiles").join("edited");
    std::fs::write(&path, text).unwrap();
    crate::common::make_executable(&path);
    path
}

/// What is left in the rig's temp folder, the profile's TMPDIR: nothing, once a start is over.
fn left_in_tmp(rig: &Rig) -> Vec<String> {
    std::fs::read_dir(rig.base.join("tmp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_first_start_builds_the_image_once_and_says_so() {
    let rig = Rig::new();

    let first = rig.run(&["claude"]);
    let second = rig.run(&["claude"]);

    // The line after the three first lines, and the profile's name not in front of it.
    assert_eq!(
        stderr(&first).lines().nth(3),
        Some("building the image taw-sandbox-default (the first start only, a minute or two)"),
        "{}",
        stderr(&first)
    );
    assert!(
        !stderr(&second).contains("building the image"),
        "{}",
        stderr(&second)
    );
    let builds: Vec<_> = rig
        .fake
        .calls()
        .into_iter()
        .filter(|call| call[1] == "build")
        .collect();
    assert_eq!(builds.len(), 1, "{builds:?}");
    assert!(
        holds(&builds[0], &["-t", "taw-sandbox-default"]),
        "{:?}",
        builds[0]
    );
    let recipe = builds[0]
        .iter()
        .find_map(|word| word.strip_prefix("taw.recipe="))
        .unwrap();
    assert_eq!(
        rig.fake.image("taw-sandbox-default").as_deref(),
        Some(recipe)
    );
    let dockerfile = rig.fake.built("Dockerfile");
    assert!(
        dockerfile.starts_with("FROM ghcr.io/jdx/mise:2026.4.6\n"),
        "{dockerfile}"
    );
    assert!(
        dockerfile.contains("ENTRYPOINT [\"/usr/local/bin/taw-sandbox-start\"]"),
        "{dockerfile}"
    );
    assert!(
        rig.fake
            .built("taw-sandbox-start")
            .starts_with("#!/bin/sh\n")
    );
    assert_eq!(rig.fake.runs().len(), 2);
    // Built where it then runs: by the daemon the start holds (the context Docker had at its start).
    assert_eq!(
        daemon_of(&rig, "build"),
        [("desktop-linux".to_string(), String::new())]
    );
    // And the folder it was built from is taken away.
    assert_eq!(left_in_tmp(&rig), Vec::<String>::new());
}

#[test]
fn an_edited_recipe_builds_the_image_again() {
    let rig = Rig::new();
    rig.run(&["claude"]);
    let changed = edited(
        &rig,
        "util-linux ca-certificates curl",
        "util-linux ca-certificates curl jq",
    );

    let out = rig.run_profile("/bin/bash", &changed, &["claude"], &[]);

    assert_eq!(
        stderr(&out).lines().nth(3),
        Some("building the image taw-sandbox-default again: its recipe in this profile changed"),
        "{}",
        stderr(&out)
    );
    assert!(rig.fake.built("Dockerfile").contains("curl jq"));
}

/// The start script is the image's too: a copy whose start script is edited builds it again.
#[test]
fn an_edited_start_script_builds_the_image_again() {
    let rig = Rig::new();
    rig.run(&["claude"]);
    let changed = edited(&rig, "\nset -eu\n", "\nset -eu\n# edited\n");

    let out = rig.run_profile("/bin/bash", &changed, &["claude"], &[]);

    assert_eq!(
        stderr(&out).lines().nth(3),
        Some("building the image taw-sandbox-default again: its recipe in this profile changed"),
        "{}",
        stderr(&out)
    );
    assert!(rig.fake.built("taw-sandbox-start").contains("\n# edited\n"));
}

#[test]
fn an_image_that_does_not_build_starts_nothing() {
    let rig = Rig::new();

    let out = rig.run_with(&["claude"], &[("FAKE_RUNTIME_BUILD_STATUS", "1")]);

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).ends_with(
            "taw sandbox default: the image taw-sandbox-default did not build, so the agent was not started\n"
        ),
        "{}",
        stderr(&out)
    );
    assert!(rig.fake.runs().is_empty());
    assert_eq!(left_in_tmp(&rig), Vec::<String>::new());
}

/// The image is this machine's own build: the run never pulls it, so a runtime never looks for
/// it in a registry (Podman, given a short name, may ask which one).
#[test]
fn the_run_never_pulls_its_image() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    let run = rig.the_run();
    assert!(
        holds(&run, &["--init", "--pull", "never", "--name"]),
        "{run:?}"
    );
}

/// Runs git in `dir` with nothing of the developer's: no environment but a scratch home and the
/// system's folders, no system or global config.
fn git(dir: &Path, args: &[&str]) {
    let out = taw::git::command()
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
}

/// The volume the profile names for a package folder at `path`: `taw-<kind>-<cksum of the path>`.
fn volume(kind: &str, path: &Path) -> String {
    use std::io::Write;

    let mut cksum = Command::new("cksum")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    cksum
        .stdin
        .take()
        .unwrap()
        .write_all(path.as_os_str().as_encoded_bytes())
        .unwrap();
    let out = cksum.wait_with_output().unwrap();
    let sum = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    format!("taw-{kind}-{sum}")
}

#[test]
fn a_checkout_is_mounted_whole_with_its_git_hooks_and_config_read_only() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let sub = rig.project.join("src");
    std::fs::create_dir_all(&sub).unwrap();

    rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", sub.to_str().unwrap())]);

    let run = rig.the_run();
    let p = rig.project.display().to_string();
    assert!(
        holds(
            &run,
            &[
                "--workdir",
                &sub.display().to_string(),
                "-v",
                &format!("{p}:{p}"),
                "-v",
                &format!("{p}/.git/hooks:{p}/.git/hooks:ro"),
                "-v",
                &format!("{p}/.git/config:{p}/.git/config:ro"),
            ]
        ),
        "{run:?}"
    );
}

#[test]
fn a_worktree_mounts_the_repositorys_git_with_its_hooks_and_config_read_only() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    git(
        &rig.project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "start",
        ],
    );
    let tree = rig.base.join("work").join("api--feat-x");
    git(
        &rig.project,
        &[
            "worktree",
            "add",
            "-q",
            tree.to_str().unwrap(),
            "-b",
            "feat/x",
        ],
    );

    rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", tree.to_str().unwrap())]);

    let run = rig.the_run();
    let (t, g) = (
        tree.display().to_string(),
        rig.project.join(".git").display().to_string(),
    );
    assert!(
        holds(
            &run,
            &[
                "--workdir",
                &t,
                "-v",
                &format!("{t}:{t}"),
                "-v",
                &format!("{g}:{g}"),
                "-v",
                &format!("{g}/hooks:{g}/hooks:ro"),
                "-v",
                &format!("{g}/config:{g}/config:ro"),
            ]
        ),
        "{run:?}"
    );
}

/// A checkout whose repository is kept outside it (git init --separate-git-dir; a submodule's is
/// in its superproject's .git) has it mounted at its own path too, its hooks and config read-only:
/// without it, git fails inside.
#[test]
fn a_checkout_whose_repository_is_kept_elsewhere_mounts_it_with_its_hooks_and_config_read_only() {
    let rig = Rig::new();
    let repository = rig.base.join("repositories").join("api.git");
    std::fs::create_dir_all(repository.parent().unwrap()).unwrap();
    git(
        &rig.project,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            repository.to_str().unwrap(),
        ],
    );

    rig.run(&["claude"]);

    let run = rig.the_run();
    let (p, g) = (
        rig.project.display().to_string(),
        repository.display().to_string(),
    );
    assert!(
        holds(
            &run,
            &[
                "--workdir",
                &p,
                "-v",
                &format!("{p}:{p}"),
                "-v",
                &format!("{g}:{g}"),
                "-v",
                &format!("{g}/hooks:{g}/hooks:ro"),
                "-v",
                &format!("{g}/config:{g}/config:ro"),
            ]
        ),
        "{run:?}"
    );
}

/// The bashes here: macOS's own (3.2), and the one `PATH` finds when it is another (bash 5).
fn bashes_here() -> Vec<String> {
    let mut bashes = vec!["/bin/bash".to_string()];
    if let Ok(found) = which::which("bash") {
        let same =
            |a: &Path, b: &Path| std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok();
        if !same(&found, Path::new("/bin/bash")) {
            bashes.push(found.display().to_string());
        }
    }
    bashes
}

/// The checkout is the one the folder is in, whatever git's variables of yours point at: a dotfiles
/// setup's GIT_DIR, with or without its GIT_WORK_TREE (your home folder), neither shares that
/// repository, read-write or at all, nor refuses the project for it. The project's own repository
/// is shared with its hooks and config read-only, as it is with none of them set.
#[test]
fn a_checkout_is_found_from_the_folder_whatever_gits_variables_point_at() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let dotfiles = rig.base.join("dotfiles.git");
    std::fs::create_dir_all(&dotfiles).unwrap();
    git(&dotfiles, &["init", "-q", "--bare"]);
    let (d, h, p) = (
        dotfiles.display().to_string(),
        rig.home.display().to_string(),
        rig.project.display().to_string(),
    );
    let setups: [&[(&str, &str)]; 2] = [
        &[("GIT_DIR", d.as_str())],
        &[("GIT_DIR", d.as_str()), ("GIT_WORK_TREE", h.as_str())],
    ];

    let bashes = bashes_here();
    for bash in &bashes {
        for setup in setups {
            let out = rig.run_profile(bash, &rig.profile, &["claude"], setup);
            assert!(out.status.success(), "{bash} {setup:?}: {}", stderr(&out));
        }
    }

    let runs = rig.fake.runs();
    assert_eq!(runs.len(), bashes.len() * setups.len(), "{runs:?}");
    for run in &runs {
        assert!(!run.iter().any(|word| word.contains(&d)), "{run:?}");
        assert!(
            holds(
                run,
                &[
                    "--workdir",
                    &p,
                    "-v",
                    &format!("{p}:{p}"),
                    "-v",
                    &format!("{p}/.git/hooks:{p}/.git/hooks:ro"),
                    "-v",
                    &format!("{p}/.git/config:{p}/.git/config:ro"),
                ]
            ),
            "{run:?}"
        );
    }
}

/// The variables the profile takes out of git's way are the ones taw's own git leaves out of its
/// environment (`taw::git::REDIRECTING_VARIABLES`), all of them and no other, in one `unset` line.
#[test]
fn the_profile_leaves_out_the_variables_taws_own_git_leaves_out() {
    let lines: Vec<&str> = taw::sandbox::DEFAULT_PROFILE
        .lines()
        .filter(|line| line.starts_with("unset GIT_"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let mut unset: Vec<&str> = lines[0].split_whitespace().skip(1).collect();
    let mut expected = taw::git::REDIRECTING_VARIABLES.to_vec();
    unset.sort_unstable();
    expected.sort_unstable();
    assert_eq!(unset, expected);
}

#[test]
fn outside_git_the_folder_is_the_project_and_nothing_of_git_is_mounted() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    let run = rig.the_run();
    assert!(!run.iter().any(|word| word.contains(".git")), "{run:?}");
}

/// git spells a checkout's top as the disk has it, and bash keeps the spelling a folder is given
/// in: a folder given in another case, or by the data volume's own path, is still worked in inside
/// the project's mount, and the hooks and config made read-only are the ones git uses there. A
/// mount at another spelling of them would leave git's own writable inside, on the container's
/// disk, which tells case apart.
#[test]
fn a_folder_spelled_another_way_is_worked_in_inside_the_checkouts_mount() {
    use std::io::Write;

    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let sub = rig.project.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let mut spellings = Vec::new();
    let other_case = rig.base.join("work").join("API").join("Src");
    if other_case.exists() {
        spellings.push(other_case);
    }
    let on_data = Path::new("/System/Volumes/Data").join(sub.strip_prefix("/").unwrap());
    if on_data.exists() {
        spellings.push(on_data);
    }
    if spellings.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "a_folder_spelled_another_way_is_worked_in_inside_the_checkouts_mount: this disk has no other spelling of a folder"
        );
        return;
    }

    let p = rig.project.display().to_string();
    for spelled in spellings {
        rig.run_with(
            &["claude"],
            &[("TAW_SANDBOX_DIR", spelled.to_str().unwrap())],
        );

        let run = rig.fake.runs().pop().unwrap();
        assert!(
            holds(
                &run,
                &[
                    "--workdir",
                    &sub.display().to_string(),
                    "-v",
                    &format!("{p}:{p}"),
                    "-v",
                    &format!("{p}/.git/hooks:{p}/.git/hooks:ro"),
                    "-v",
                    &format!("{p}/.git/config:{p}/.git/config:ro"),
                ]
            ),
            "{}: {run:?}",
            spelled.display()
        );
    }
}

/// The checkout is shared whole, so a folder in a checkout of the home folder, or of a folder that
/// holds it, is refused as the home folder is, before anything runs or is made, however the home is
/// spelled: through a link, in another case, or by the data volume's own path (git spells the top
/// as the disk has it). A project with a checkout of its own in such a home is a project.
#[test]
fn a_folder_in_a_git_checkout_of_the_home_folder_or_above_it_is_refused() {
    let refused = |what: &str| {
        format!(
            "taw sandbox default: this folder is in a git checkout of {what}, and the sandbox would share all of it; cd into a project with a git checkout of its own first\n"
        )
    };
    let spellings_of_the_home = |rig: &Rig| {
        let link = rig.base.join("link-home");
        if !link.exists() {
            std::os::unix::fs::symlink(&rig.home, &link).unwrap();
        }
        let mut homes = vec![rig.home.clone(), link];
        let other_case = rig.base.join("HOME");
        if other_case.exists() {
            homes.push(other_case);
        }
        let on_data = Path::new("/System/Volumes/Data").join(rig.home.strip_prefix("/").unwrap());
        if on_data.exists() {
            homes.push(on_data);
        }
        homes
    };

    let rig = Rig::new();
    git(&rig.home, &["init", "-q"]);
    let notes = rig.home.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    for home in spellings_of_the_home(&rig) {
        let out = rig.run_with(
            &["codex"],
            &[
                ("HOME", home.to_str().unwrap()),
                ("TAW_SANDBOX_DIR", notes.to_str().unwrap()),
            ],
        );

        assert_eq!(
            out.status.code(),
            Some(1),
            "{}: {}",
            home.display(),
            stderr(&out)
        );
        assert_eq!(
            stderr(&out),
            refused("your whole home folder"),
            "{}",
            home.display()
        );
    }
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    assert!(
        !rig.home.join(".codex").exists(),
        "a settings folder was made"
    );
    // A project with a checkout of its own in that home is a project.
    let api = rig.home.join("work").join("api");
    std::fs::create_dir_all(&api).unwrap();
    git(&api, &["init", "-q"]);
    let out = rig.run_with(&["codex"], &[("TAW_SANDBOX_DIR", api.to_str().unwrap())]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(rig.fake.runs().len(), 1);

    // A checkout of the folder that holds the home: the rig's own base.
    let above = Rig::new();
    git(&above.base, &["init", "-q"]);
    for home in spellings_of_the_home(&above) {
        let out = above.run_with(&["codex"], &[("HOME", home.to_str().unwrap())]);

        assert_eq!(
            out.status.code(),
            Some(1),
            "{}: {}",
            home.display(),
            stderr(&out)
        );
        assert_eq!(
            stderr(&out),
            refused(&format!(
                "{}, which holds your home folder",
                above.base.display()
            )),
            "{}",
            home.display()
        );
    }
    assert!(above.fake.calls().is_empty(), "{:?}", above.fake.calls());
}

/// A repository with no hooks folder still has one read-only inside, or the agent could make one,
/// with hooks that run on this machine: the start makes it, empty, right before the agent starts.
/// Print mode makes nothing, and prints what a start then runs, read back by a real shell.
#[test]
fn a_checkout_with_no_hooks_folder_gets_an_empty_one_read_only_inside() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let hooks = rig.project.join(".git").join("hooks");
    std::fs::remove_dir_all(&hooks).unwrap();
    let mount = format!("{0}:{0}:ro", hooks.display());

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_PRINT", "1")]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!hooks.exists(), "print mode made {}", hooks.display());
    let place = rig.base.join("reading");
    std::fs::create_dir_all(&place).unwrap();
    let printed = read_back("/bin/bash", out.stdout.trim_ascii_end(), &rig.home, &place);
    assert!(holds(&printed, &["-v", &mount]), "{printed:?}");

    rig.run(&["claude"]);

    assert_eq!(normalised(&printed), normalised(&raw_run(&rig)));
    assert!(hooks.is_dir(), "the start did not make {}", hooks.display());
    assert_eq!(std::fs::read_dir(&hooks).unwrap().count(), 0);
}

#[test]
fn package_folders_get_volumes_of_their_own_at_the_top_always_and_deeper_where_found() {
    let rig = Rig::new();
    for found in [
        "packages/web/node_modules",
        "services/ml/.venv",
        "node_modules/left-pad/node_modules",
        ".git/node_modules",
    ] {
        std::fs::create_dir_all(rig.project.join(found)).unwrap();
    }

    rig.run(&["claude"]);

    let run = rig.the_run();
    let p = &rig.project;
    for (kind, path) in [
        ("node_modules", p.join("node_modules")),
        ("venv", p.join(".venv")),
        ("node_modules", p.join("packages/web/node_modules")),
        ("venv", p.join("services/ml/.venv")),
    ] {
        let mount = format!("{}:{}", volume(kind, &path), path.display());
        assert!(holds(&run, &["-v", &mount]), "{mount} in {run:?}");
    }
    assert!(
        !run.iter().any(|word| word.contains("left-pad")),
        "inside node_modules: {run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains(".git/node_modules")),
        "inside .git: {run:?}"
    );
    let own = run
        .iter()
        .find_map(|word| word.strip_prefix("TAW_SB_OWN="))
        .unwrap();
    assert_eq!(
        own.lines().collect::<Vec<_>>(),
        [
            p.join("node_modules").display().to_string(),
            p.join(".venv").display().to_string(),
            p.join("packages/web/node_modules").display().to_string(),
            p.join("services/ml/.venv").display().to_string(),
            "/opt/taw/mise".to_string(),
            rig.home.join(".cache").display().to_string(),
        ]
    );
}

#[test]
fn the_caches_are_two_volumes_every_sandbox_shares() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    assert!(holds(
        &rig.the_run(),
        &[
            "-v",
            "taw-sandbox-mise:/opt/taw/mise",
            "-v",
            &format!("taw-sandbox-cache:{}/.cache", rig.home.display()),
        ]
    ));
}

/// A project path with spaces and a quote is one argument in every mount.
#[test]
fn a_project_path_with_spaces_and_a_quote_stays_one_argument_in_every_mount() {
    let rig = Rig::new();
    let odd = rig.base.join("work").join("it's my api");
    std::fs::create_dir_all(odd.join("node_modules")).unwrap();
    git(&odd, &["init", "-q"]);

    rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", odd.to_str().unwrap())]);

    let run = rig.the_run();
    let p = odd.display().to_string();
    assert!(
        holds(&run, &["--workdir", &p, "-v", &format!("{p}:{p}")]),
        "{run:?}"
    );
    assert!(
        holds(
            &run,
            &["-v", &format!("{p}/.git/config:{p}/.git/config:ro")]
        ),
        "{run:?}"
    );
    let mount = format!(
        "{}:{p}/node_modules",
        volume("node_modules", &odd.join("node_modules"))
    );
    assert!(holds(&run, &["-v", &mount]), "{run:?}");
}

/// A recipe's name exported in the environment, its line left commented, changes nothing: the run
/// and what the profile says are the default's, since the profile's top unsets every recipe name.
fn exported_changes_nothing(name: &str, value: &str) {
    // Warm: neither start builds the image, so the two say the same lines.
    let rig = Rig::new().warm();
    let plain = rig.run(&["claude"]);

    let out = rig.run_with(&["claude"], &[(name, value)]);

    assert!(out.status.success(), "{name}={value}: {}", stderr(&out));
    let runs = rig.fake.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(normalised(&runs[1]), normalised(&runs[0]), "{name}={value}");
    assert_eq!(stderr(&out), stderr(&plain), "{name}={value}");
}

#[test]
fn settings_in_the_environment_changes_nothing() {
    exported_changes_nothing("settings", "own");
}

#[test]
fn settings_own_gives_claude_a_set_of_its_own_with_yours_read_only_and_your_conversations_shared() {
    // Warm: the start's last line is then the first lines' last, not the image's build.
    let rig = Rig::new().warm();
    let claude = rig.home.join(".claude");
    std::fs::write(claude.join("settings.json"), "{}").unwrap();
    std::fs::write(claude.join("CLAUDE.md"), "# mine").unwrap();
    std::fs::create_dir_all(claude.join("skills")).unwrap();
    let own = rig.with_recipe("settings=own");
    let state = rig.base.join("taw-state");

    let out = rig.run_profile(
        "/bin/bash",
        &own,
        &["claude"],
        &[("TAW_STATE_DIR", state.to_str().unwrap())],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    let set = state.join("sandbox/own/default/claude");
    let (c, h) = (claude.display().to_string(), rig.home.display().to_string());
    assert!(
        holds(
            &run,
            &[
                "-v",
                &format!("{}:{c}", set.join("claude").display()),
                "-v",
                &format!("{c}/settings.json:{c}/settings.json:ro"),
                "-v",
                &format!("{c}/CLAUDE.md:{c}/CLAUDE.md:ro"),
                "-v",
                &format!("{c}/skills:{c}/skills:ro"),
                "-v",
                &format!("{c}/projects:{c}/projects"),
                "-v",
                &format!("{}:{h}/.claude.json", set.join("claude.json").display()),
            ]
        ),
        "{run:?}"
    );
    assert!(
        !holds(&run, &["-v", &format!("{c}:{c}")]),
        "your own folder is not shared: {run:?}"
    );
    assert!(
        !holds(&run, &["-v", &format!("{h}/.claude.json:{h}/.claude.json")]),
        "your own login is not shared: {run:?}"
    );
    assert!(
        stderr(&out).ends_with(
            "  uses a separate set of claude settings, yours read-only inside; runs commands without asking\n"
        ),
        "{}",
        stderr(&out)
    );
    // Made by the start: the set's folder, its login file, and your conversations' folder.
    assert!(set.join("claude").is_dir());
    assert_eq!(
        std::fs::read_to_string(set.join("claude.json")).unwrap(),
        "{}\n"
    );
    assert!(claude.join("projects").is_dir());
}

#[test]
fn settings_own_gives_another_agent_folders_of_its_own_and_nothing_of_yours() {
    let rig = Rig::new().warm();
    let own = rig.with_recipe("settings=own");
    let state = rig.base.join("taw-state");

    let out = rig.run_profile(
        "/bin/bash",
        &own,
        &["opencode"],
        &[("TAW_STATE_DIR", state.to_str().unwrap())],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    let set = state.join("sandbox/own/default/opencode");
    for (name, folder) in [
        (".config_opencode", ".config/opencode"),
        (".local_share_opencode", ".local/share/opencode"),
    ] {
        let yours = rig.home.join(folder).display().to_string();
        let mount = format!("{}:{yours}", set.join(name).display());
        assert!(holds(&run, &["-v", &mount]), "{mount} in {run:?}");
        assert!(
            !holds(&run, &["-v", &format!("{yours}:{yours}")]),
            "{run:?}"
        );
        assert!(set.join(name).is_dir(), "{name} was not made");
    }
    // Nothing of yours is in it, and its first lines say so.
    assert!(
        stderr(&out).ends_with(
            "  uses a separate set of opencode settings, none of yours inside; runs commands without asking\n"
        ),
        "{}",
        stderr(&out)
    );
}

/// With settings=own the set is mounted where the agent's variable names its folder, when it names
/// one, and the variable goes in beside HOME, as it does for shared settings; your folder is not
/// shared.
#[test]
fn settings_own_follows_the_variable_naming_the_agents_folder() {
    for (command, variable) in SETTINGS_VARIABLES {
        let rig = Rig::new();
        let own = rig.with_recipe("settings=own");
        let state = rig.base.join("taw-state");
        let path = rig
            .home
            .join("other settings")
            .join(command)
            .display()
            .to_string();

        let out = rig.run_profile(
            "/bin/bash",
            &own,
            &[command],
            &[
                (variable, path.as_str()),
                ("TAW_STATE_DIR", state.to_str().unwrap()),
            ],
        );

        assert!(out.status.success(), "{command}: {}", stderr(&out));
        let run = rig.the_run();
        let agent = if command == "cursor-agent" {
            "cursor"
        } else {
            command
        };
        let set = state.join("sandbox/own/default").join(agent);
        let mine = if command == "claude" {
            set.join("claude")
        } else {
            set.join(format!("other settings_{command}"))
        };
        assert!(
            holds(&run, &["-v", &format!("{}:{path}", mine.display())]),
            "{command}: {run:?}"
        );
        assert!(
            holds(
                &run,
                &[
                    "-e",
                    &format!("HOME={}", rig.home.display()),
                    "-e",
                    &format!("{variable}={path}")
                ]
            ),
            "{command}: {run:?}"
        );
        assert!(
            !holds(&run, &["-v", &format!("{path}:{path}")]),
            "{command}: your folder is not shared: {run:?}"
        );
        if command == "claude" {
            // Claude keeps its login inside the folder the variable names.
            assert!(
                !run.iter().any(|word| word.contains(".claude.json")),
                "{run:?}"
            );
        }
    }
}

/// The separate set is in taw's state folder, found as taw finds it: TAW_STATE_DIR, else
/// XDG_STATE_HOME's tmux-agent-wizard when that is a full path (taw ignores one that is not),
/// else ~/.local/state's.
#[test]
fn the_separate_set_is_in_taws_state_folder_as_taw_finds_it() {
    let rig = Rig::new();
    let own = rig.with_recipe("settings=own");
    let named = rig.base.join("taw-state");
    let cases = [
        (
            vec![("TAW_STATE_DIR", named.to_str().unwrap())],
            named.clone(),
        ),
        (vec![], rig.base.join("xdg-state/tmux-agent-wizard")),
        (
            vec![("XDG_STATE_HOME", "relative/state")],
            rig.home.join(".local/state/tmux-agent-wizard"),
        ),
    ];
    let codex = rig.home.join(".codex").display().to_string();
    for (extra, state) in cases {
        let out = rig.run_profile("/bin/bash", &own, &["codex"], &extra);

        assert!(out.status.success(), "{extra:?}: {}", stderr(&out));
        let run = rig.fake.runs().pop().unwrap();
        let set = state.join("sandbox/own/default/codex/.codex");
        assert!(
            holds(&run, &["-v", &format!("{}:{codex}", set.display())]),
            "{extra:?}: {run:?}"
        );
        assert!(set.is_dir(), "{extra:?}: {} was not made", set.display());
    }
    assert!(
        !rig.project.join("relative").exists(),
        "a state folder was made in the project"
    );
}

/// Print mode makes nothing for settings=own either: no set and no conversations folder. It prints
/// what a start then runs, read back by a real shell.
#[test]
fn print_mode_makes_no_separate_set_and_prints_what_a_start_runs() {
    let rig = Rig::new();
    let own = rig.with_recipe("settings=own");
    let state = rig.base.join("taw-state");
    let projects = rig.home.join(".claude").join("projects");
    let extra = [("TAW_STATE_DIR", state.to_str().unwrap())];
    let mut printing = extra.to_vec();
    printing.push(("TAW_SANDBOX_PRINT", "1"));

    let out = rig.run_profile("/bin/bash", &own, &["claude"], &printing);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!state.exists(), "print mode made {}", state.display());
    assert!(!projects.exists(), "print mode made {}", projects.display());
    let place = rig.base.join("reading");
    std::fs::create_dir_all(&place).unwrap();
    let printed = read_back("/bin/bash", out.stdout.trim_ascii_end(), &rig.home, &place);
    let set = state.join("sandbox/own/default/claude/claude");
    let mount = format!("{}:{}", set.display(), rig.home.join(".claude").display());
    assert!(holds(&printed, &["-v", &mount]), "{printed:?}");

    rig.run_profile("/bin/bash", &own, &["claude"], &extra);

    assert_eq!(normalised(&printed), normalised(&raw_run(&rig)));
    assert!(set.is_dir() && projects.is_dir());
}

/// A separate set that cannot be made, its state folder in a folder that cannot be written: the
/// start ends with the profile's own line, as for shared settings, and runs nothing.
#[test]
fn a_separate_set_that_cannot_be_made_ends_the_start_with_the_profiles_own_line() {
    // Root may write anywhere, so there is nothing to refuse it.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    /// The folder made read-only for as long as it lives, and writable again after, so that the
    /// rig can take its folder away even when the test fails.
    struct ReadOnly<'a>(&'a Path);
    impl Drop for ReadOnly<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let rig = Rig::new();
    let own = rig.with_recipe("settings=own");
    let locked = rig.base.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let _writable_after = ReadOnly(&locked);
    let state = locked.join("taw-state");

    let out = rig.run_profile(
        "/bin/bash",
        &own,
        &["codex"],
        &[("TAW_STATE_DIR", state.to_str().unwrap())],
    );

    let set = state.join("sandbox/own/default/codex/.codex");
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).ends_with(&format!(
            "taw sandbox default: {} could not be made for codex's settings, so the agent was not started\n",
            set.display()
        )),
        "{}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("Permission denied"),
        "{}",
        stderr(&out)
    );
    assert!(rig.fake.runs().is_empty(), "{:?}", rig.fake.runs());
}

#[test]
fn a_settings_line_this_profile_does_not_know_is_refused() {
    let rig = Rig::new();
    let other = edited(&rig, "#   settings=own\n", "  settings=mine\n");

    let out = rig.run_profile("/bin/bash", &other, &["claude"], &[]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: settings=mine is not one of this profile's recipes (own)\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

/// Commits once in the repository at `dir`, as t, staged or not.
fn commit(dir: &Path, message: &str) {
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            message,
        ],
    );
}

/// A repository at `dir` with one commit.
fn repository_with_a_commit(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    commit(dir, "start");
}

/// The rig's project as a checkout with a submodule at vendor/lib, which has one of its own, inner:
/// their repositories are .git/modules/vendor/lib and .git/modules/vendor/lib/modules/inner, which
/// it gives back.
fn with_nested_submodules(rig: &Rig) -> (PathBuf, PathBuf) {
    let inner = rig.base.join("upstream").join("inner");
    let lib = rig.base.join("upstream").join("lib");
    repository_with_a_commit(&inner);
    repository_with_a_commit(&lib);
    let file = ["-c", "protocol.file.allow=always"];
    git(
        &lib,
        &[
            &file[..],
            &["submodule", "add", "-q", inner.to_str().unwrap(), "inner"],
        ]
        .concat(),
    );
    commit(&lib, "inner");
    repository_with_a_commit(&rig.project);
    git(
        &rig.project,
        &[
            &file[..],
            &[
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        ]
        .concat(),
    );
    git(
        &rig.project,
        &[
            &file[..],
            &["submodule", "update", "-q", "--init", "--recursive"],
        ]
        .concat(),
    );
    let modules = rig.project.join(".git").join("modules");
    let found = (
        modules.join("vendor/lib"),
        modules.join("vendor/lib/modules/inner"),
    );
    assert!(found.1.join("HEAD").is_file(), "no nested submodule");
    found
}

/// Every repository the checkout holds has its hooks folder and config read-only, as the top's
/// has: each submodule's in .git/modules, and a nested one's (a hook written there runs at the
/// next git command in that submodule outside, and shows in no diff). A repository is known by
/// what makes one, so a folder of refs that happens to hold files called HEAD and config is no
/// repository, and a link is never followed.
#[test]
fn every_repository_the_checkout_holds_has_its_hooks_and_config_read_only() {
    let rig = Rig::new();
    let (lib, inner) = with_nested_submodules(&rig);
    // A remote's refs: HEAD, and a branch called config.
    let refs = lib.join("refs").join("remotes").join("origin");
    std::fs::create_dir_all(&refs).unwrap();
    std::fs::write(refs.join("HEAD"), "ref: refs/remotes/origin/main\n").unwrap();
    std::fs::write(refs.join("config"), "ref: refs/remotes/origin/main\n").unwrap();
    // A clone that stopped half way: a config and objects, and no HEAD.
    let half = rig.project.join(".git/modules/half");
    std::fs::create_dir_all(half.join("objects")).unwrap();
    std::fs::write(half.join("config"), "[core]\n").unwrap();
    // A link in .git/modules to a repository elsewhere.
    let elsewhere = rig.base.join("elsewhere");
    repository_with_a_commit(&elsewhere);
    std::os::unix::fs::symlink(
        elsewhere.join(".git"),
        rig.project.join(".git/modules/elsewhere"),
    )
    .unwrap();

    rig.run(&["claude"]);

    let run = rig.the_run();
    for repository in [rig.project.join(".git"), lib, inner] {
        for part in ["hooks", "config"] {
            let path = repository.join(part).display().to_string();
            // Once: a runtime refuses a second mount at one place.
            let mount = format!("{path}:{path}:ro");
            assert_eq!(
                run.iter().filter(|word| **word == mount).count(),
                1,
                "{mount} in {run:?}"
            );
        }
    }
    assert!(
        !run.iter().any(|word| word.contains("/refs/")),
        "a folder of refs taken for a repository: {run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains("/half/")),
        "a folder with no HEAD taken for a repository: {run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains("elsewhere")),
        "a link followed: {run:?}"
    );
    assert!(!refs.join("hooks").exists());
}

/// A submodule's repository with no hooks folder gets an empty one read-only inside, made by the
/// start and never by print mode, whose line a real shell reads back as what the start runs.
#[test]
fn a_submodule_with_no_hooks_folder_gets_an_empty_one_read_only_inside() {
    let rig = Rig::new();
    let (lib, _) = with_nested_submodules(&rig);
    let hooks = lib.join("hooks");
    std::fs::remove_dir_all(&hooks).unwrap();
    let mount = format!("{0}:{0}:ro", hooks.display());

    let out = rig.run_with(&["claude"], &[("TAW_SANDBOX_PRINT", "1")]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!hooks.exists(), "print mode made {}", hooks.display());
    let place = rig.base.join("reading");
    std::fs::create_dir_all(&place).unwrap();
    let printed = read_back("/bin/bash", out.stdout.trim_ascii_end(), &rig.home, &place);
    assert!(holds(&printed, &["-v", &mount]), "{printed:?}");

    rig.run(&["claude"]);

    assert_eq!(normalised(&printed), normalised(&raw_run(&rig)));
    assert!(hooks.is_dir(), "the start did not make {}", hooks.display());
    assert_eq!(std::fs::read_dir(&hooks).unwrap().count(), 0);
}

/// config.worktree, which git reads beside config when worktreeConfig is on, is read-only wherever
/// it is: the repository's own and a worktree's, from the checkout and from the worktree.
#[test]
fn a_config_worktree_is_read_only_wherever_it_is() {
    let rig = Rig::new();
    repository_with_a_commit(&rig.project);
    let tree = rig.base.join("work").join("api--feat-x");
    git(
        &rig.project,
        &[
            "worktree",
            "add",
            "-q",
            tree.to_str().unwrap(),
            "-b",
            "feat/x",
        ],
    );
    let git_dir = rig.project.join(".git");
    let files = [
        git_dir.join("config.worktree"),
        git_dir.join("worktrees/api--feat-x/config.worktree"),
    ];
    for file in &files {
        std::fs::write(file, "[core]\n\tsparseCheckout = false\n").unwrap();
    }

    for dir in [&rig.project, &tree] {
        rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", dir.to_str().unwrap())]);

        let run = rig.fake.runs().pop().unwrap();
        for file in &files {
            let path = file.display().to_string();
            assert!(
                holds(&run, &["-v", &format!("{path}:{path}:ro")]),
                "{}: {path} in {run:?}",
                dir.display()
            );
        }
    }
}

/// The host tools that say this machine's addresses: `uname` and the one it implies.
fn addresses(rig: &Rig, system: &str) {
    rig.tool("uname", system);
    let ifconfig = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\n\tinet6 ::1 prefixlen 128\n\tinet6 fe80::1%lo0 prefixlen 64 scopeid 0x1\nen0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tinet 192.168.1.20 netmask 0xffffff00 broadcast 192.168.1.255\n\tinet6 fd12:3456::1 prefixlen 64";
    let ip = "1: lo    inet 127.0.0.1/8 scope host lo\n1: lo    inet6 ::1/128 scope host\n2: eth0    inet 192.168.1.20/24 brd 192.168.1.255 scope global eth0\n2: eth0    inet6 fd12:3456::1/64 scope global\n3: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0";
    for (name, out) in [("ifconfig", ifconfig), ("ip", ip)] {
        let path = rig.tools.join(name);
        std::fs::write(&path, format!("#!/bin/sh\ncat <<'OUT'\n{out}\nOUT\n")).unwrap();
        crate::common::make_executable(&path);
    }
}

#[test]
fn every_address_of_this_machine_is_handed_in_and_loopback_is_not() {
    let mac = Rig::new();
    addresses(&mac, "Darwin");
    mac.run(&["claude"]);
    assert!(
        holds(
            &mac.the_run(),
            &["-e", "TAW_SB_BLOCK=192.168.1.20 fd12:3456::1 fe80::1"]
        ),
        "{:?}",
        mac.the_run()
    );

    let linux = Rig::new();
    addresses(&linux, "Linux");
    linux.run(&["claude"]);
    assert!(
        holds(
            &linux.the_run(),
            &["-e", "TAW_SB_BLOCK=172.17.0.1 192.168.1.20 fd12:3456::1"]
        ),
        "{:?}",
        linux.the_run()
    );
}

/// A Linux machine without `ip` hands in no address of its own, and the start goes on.
#[test]
fn a_machine_without_the_tool_that_lists_its_addresses_still_starts() {
    let rig = Rig::new();
    rig.tool("uname", "Linux");
    // What a missing program gives the shell: status 127. The rig's tools come before the
    // system's folders on its PATH, so a real `ip` on a Linux machine is never reached.
    let ip = rig.tools.join("ip");
    std::fs::write(&ip, "#!/bin/sh\nexit 127\n").unwrap();
    crate::common::make_executable(&ip);

    let out = rig.run(&["claude"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        holds(&rig.the_run(), &["-e", "TAW_SB_BLOCK="]),
        "{:?}",
        rig.the_run()
    );
}

#[test]
fn open_ports_are_handed_in_and_said() {
    let rig = Rig::new();
    let open = rig.with_recipe("open_ports=\"8317\"");
    // Two, however they are spaced, said one after the other.
    let two = edited(
        &rig,
        "#   open_ports=\"8317\"",
        "  open_ports=\"8317  9000\"",
    );

    let out = rig.run_profile("/bin/bash", &open, &["claude"], &[]);
    let both = rig.run_profile("/bin/bash", &two, &["claude"], &[]);

    assert!(
        holds(&rig.fake.runs()[0], &["-e", "TAW_SB_OPEN_PORTS=8317"]),
        "{:?}",
        rig.fake.runs()
    );
    assert!(
        stderr(&out).contains(
            "  reaches the internet and your local network, not this machine (but its ports 8317); no docker inside\n"
        ),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&both).contains("not this machine (but its ports 8317, 9000); no docker inside\n"),
        "{}",
        stderr(&both)
    );
}

/// open_ports is port numbers, written out: a word, or a pattern (which would be matched against
/// the project's files, here one called 8080), stops the start with its line.
#[test]
fn open_ports_takes_port_numbers_only() {
    let rig = Rig::new();
    std::fs::write(rig.project.join("8080"), "").unwrap();
    for ports in ["8317 db", "80*"] {
        let open = edited(
            &rig,
            "#   open_ports=\"8317\"",
            &format!("  open_ports=\"{ports}\""),
        );

        let out = rig.run_profile("/bin/bash", &open, &["claude"], &[]);

        assert_eq!(out.status.code(), Some(1), "{ports}: {}", stderr(&out));
        assert_eq!(
            stderr(&out),
            "taw sandbox default: open_ports takes port numbers; fix its line in this profile\n",
            "{ports}"
        );
    }
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn open_ports_in_the_environment_changes_nothing() {
    exported_changes_nothing("open_ports", "8317");
}

#[test]
fn block_local_network_is_handed_in_and_said() {
    let rig = Rig::new();
    let blocking = rig.with_recipe("block_local_network=yes");

    let out = rig.run_profile("/bin/bash", &blocking, &["claude"], &[]);

    assert!(holds(&rig.the_run(), &["-e", "TAW_SB_BLOCK_LOCAL=yes"]));
    assert!(
        stderr(&out).contains(
            "  reaches the internet, not your local network or this machine; no docker inside\n"
        ),
        "{}",
        stderr(&out)
    );
}

/// The start script blocks the local network only for yes, so any other word on the line would
/// have the first lines say one thing and the rules do another: it is refused.
#[test]
fn a_block_local_network_line_this_profile_does_not_know_is_refused() {
    let rig = Rig::new();
    let other = edited(
        &rig,
        "#   block_local_network=yes\n",
        "  block_local_network=true\n",
    );

    let out = rig.run_profile("/bin/bash", &other, &["claude"], &[]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: block_local_network=true is not one of this profile's recipes (yes)\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn block_local_network_in_the_environment_changes_nothing() {
    exported_changes_nothing("block_local_network", "yes");
}

/// The project's Compose services running, as the fake runtime tells it from the runtime shapes: the
/// network and the subnet those shapes name, read from them, so that a recording whose names are
/// its own is no failure.
fn compose_running(rig: &Rig) -> (String, String) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox");
    let line = std::fs::read_to_string(fixtures.join("ps.jsonl")).unwrap();
    std::fs::write(
        rig.fake.state.join("ps.jsonl"),
        line.replace("{project}", &rig.project.display().to_string()),
    )
    .unwrap();
    let subnets = std::fs::read_to_string(fixtures.join("network-subnets.txt")).unwrap();
    std::fs::write(rig.fake.state.join("network-subnets.txt"), &subnets).unwrap();
    let network = line
        .split("\"Networks\":\"")
        .nth(1)
        .and_then(|rest| rest.split(['"', ',']).next())
        .unwrap();
    (network.to_string(), subnets.trim().to_string())
}

/// Started in a folder of the checkout, the project's services are found by the checkout's top,
/// where Compose ran.
#[test]
fn the_projects_compose_network_is_joined_and_its_subnets_are_the_sandboxs_own() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let src = rig.project.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let (network, subnet) = compose_running(&rig);

    rig.run_with(&["claude"], &[("TAW_SANDBOX_DIR", src.to_str().unwrap())]);

    let run = rig.the_run();
    assert!(holds(&run, &["--network", &network]), "{run:?}");
    assert!(
        holds(&run, &["-e", &format!("TAW_SB_ALLOW={subnet}")]),
        "{run:?}"
    );
    assert!(rig.fake.calls().iter().any(|call| holds(
        call,
        &[
            "ps",
            "--filter",
            &format!(
                "label=com.docker.compose.project.working_dir={}",
                rig.project.display()
            )
        ]
    )));
}

/// From a folder of the checkout too: the Compose file is the project's, at its top.
#[test]
fn print_mode_asks_the_runtime_nothing_and_says_a_start_joins_compose() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let src = rig.project.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(rig.project.join("compose.yaml"), "services: {}\n").unwrap();

    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_PRINT", "1"),
            ("TAW_SANDBOX_DIR", src.to_str().unwrap()),
        ],
    );

    assert!(rig.fake.calls().is_empty());
    let printed = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        printed.lines().nth(1),
        Some("# a start also joins the network of the project's Compose services, when they run"),
        "{printed}"
    );
}

/// The test plan: for an ordinary checkout the recorded arguments are, argument for argument,
/// the expected list.
#[test]
fn an_ordinary_checkouts_arguments_are_argument_for_argument_the_expected_list() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    addresses(&rig, "Darwin");
    rig.tool("claude", "2.1.286 (Claude Code)");
    let report = rig.base.join("panes").join("%31");
    std::fs::create_dir_all(&report).unwrap();

    let out = rig.run_with(
        &["claude"],
        &[
            ("TAW_SANDBOX_REPORT", report.to_str().unwrap()),
            ("TAW_SANDBOX_PANE", "%31"),
        ],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    let (p, h) = (
        rig.project.display().to_string(),
        rig.home.display().to_string(),
    );
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
    let expected: Vec<String> = [
        "docker",
        "run",
        "--rm",
        "--interactive",
        "--tty",
        "--init",
        "--pull",
        "never",
        "--name",
        "<name>",
        "--label",
        "taw.profile=default",
        "--label",
        "taw.pane=%31",
        "--detach-keys",
        "ctrl-],ctrl-],ctrl-]",
        "--shm-size",
        "1g",
        "--cap-drop",
        "ALL",
        "--cap-add",
        "NET_ADMIN",
        "--cap-add",
        "SETUID",
        "--cap-add",
        "SETGID",
        "--cap-add",
        "CHOWN",
        "--cap-add",
        "SETPCAP",
        "--security-opt",
        "no-new-privileges",
        "--workdir",
        &p,
        "-v",
        &format!("{p}:{p}"),
        "-v",
        &format!("{p}/.git/hooks:{p}/.git/hooks:ro"),
        "-v",
        &format!("{p}/.git/config:{p}/.git/config:ro"),
        "-v",
        &format!(
            "{}:{p}/node_modules",
            volume("node_modules", &rig.project.join("node_modules"))
        ),
        "-v",
        &format!("{}:{p}/.venv", volume("venv", &rig.project.join(".venv"))),
        "-v",
        "taw-sandbox-mise:/opt/taw/mise",
        "-v",
        &format!("taw-sandbox-cache:{h}/.cache"),
        "-v",
        &format!("{h}/.claude:{h}/.claude"),
        "-v",
        &format!("{h}/.claude.json:{h}/.claude.json"),
        "-e",
        "TAW_SB_INSTALL=claude",
        "-e",
        "TAW_SB_VERSION=2.1.286",
        "-e",
        "TAW_SB_BLOCK=192.168.1.20 fd12:3456::1 fe80::1",
        "-e",
        "TAW_SB_OPEN_PORTS=",
        "-e",
        "TAW_SB_BLOCK_LOCAL=no",
        "-e",
        &format!("HOME={h}"),
        "-e",
        "TERM",
        "-e",
        "COLORTERM",
        "-e",
        "LANG",
        "-e",
        "TZ=UTC",
        "-e",
        "GIT_AUTHOR_NAME=Sam Doe",
        "-e",
        "GIT_COMMITTER_NAME=Sam Doe",
        "-e",
        "GIT_AUTHOR_EMAIL=sam@example.com",
        "-e",
        "GIT_COMMITTER_EMAIL=sam@example.com",
        "-e",
        "TAW_SB_PROFILE=default",
        "-e",
        &format!("TAW_SB_UID={}", uid.trim()),
        "-e",
        &format!("TAW_SB_GID={}", gid.trim()),
        "-e",
        "TAW_SB_TOOLS=",
        "-e",
        &format!("TAW_SB_MOUNTS={h}/.cache\n{h}/.claude\n{h}/.claude.json"),
        "-e",
        &format!("TAW_SB_OWN={p}/node_modules\n{p}/.venv\n/opt/taw/mise\n{h}/.cache"),
        "-v",
        &format!("{}:/run/taw", report.display()),
        "-e",
        "TAW_REPORT_FILE=/run/taw/session.json",
        "-e",
        "TAW_REPORT_AGENT=claude",
        "-e",
        "TAW_SB_ALLOW=",
        "taw-sandbox-default",
        "claude",
        "--dangerously-skip-permissions",
    ]
    .iter()
    .map(|word| word.to_string())
    .collect();
    assert_eq!(normalised(&rig.the_run()), expected);
}

#[test]
fn with_no_docker_recipe_the_sandbox_has_no_socket_and_no_proxy() {
    let rig = Rig::new();

    rig.run(&["claude"]);

    let run = rig.the_run();
    assert!(
        !run.iter()
            .any(|word| word.contains("docker.sock") || word.starts_with("DOCKER_HOST")),
        "{run:?}"
    );
    assert!(holds(&run, &["-e", "TAW_SB_TOOLS="]), "{run:?}");
    assert!(
        !run.iter().any(|word| word.starts_with("TAW_SB_DOCKER")),
        "{run:?}"
    );
    assert!(
        !rig.fake
            .calls()
            .iter()
            .any(|call| holds(call, &["run", "-d"]))
    );
}

#[test]
fn docker_full_hands_over_the_socket_and_says_what_that_means() {
    let rig = Rig::new();
    let full = rig.with_recipe("docker=full");

    let out = rig.run_profile("/bin/bash", &full, &["claude"], &[]);

    let run = rig.the_run();
    assert!(
        holds(&run, &["-v", "/var/run/docker.sock:/var/run/docker.sock"]),
        "{run:?}"
    );
    assert!(
        holds(&run, &["-e", "TAW_SB_TOOLS=docker-cli@29.9.1"]),
        "{run:?}"
    );
    assert!(holds(&run, &["-e", "TAW_SB_DOCKER=full"]), "{run:?}");
    assert!(
        stderr(&out).contains("; all of docker inside, which is this whole machine\n"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn docker_read_only_starts_a_proxy_on_a_network_only_the_two_share() {
    let rig = Rig::new();
    let read_only = rig.with_recipe("docker=read-only");

    let out = rig.run_profile("/bin/bash", &read_only, &["claude"], &[]);

    let run = rig.the_run();
    let name = run[run.iter().position(|word| word == "--name").unwrap() + 1].clone();
    let proxy = format!("{name}-docker");
    let calls = rig.fake.calls();
    assert!(
        calls
            .iter()
            .any(|call| call[1..] == ["network", "create", proxy.as_str()]),
        "{calls:?}"
    );
    assert!(
        calls.iter().any(|call| call[1..]
            == [
                "run",
                "-d",
                "--rm",
                "--name",
                proxy.as_str(),
                "--network",
                proxy.as_str(),
                "--label",
                "taw.profile=default",
                "-v",
                "/var/run/docker.sock:/var/run/docker.sock:ro",
                "-e",
                "CONTAINERS=1",
                "-e",
                "IMAGES=1",
                "-e",
                "INFO=1",
                "-e",
                "NETWORKS=1",
                "-e",
                "VOLUMES=1",
                "-e",
                "POST=0",
                "docker.io/tecnativa/docker-socket-proxy:0.3.0",
            ]),
        "{calls:?}"
    );
    assert!(holds(&run, &["--network", &proxy]), "{run:?}");
    assert!(
        holds(&run, &["-e", &format!("DOCKER_HOST=tcp://{proxy}:2375")]),
        "{run:?}"
    );
    assert!(
        holds(&run, &["-e", "TAW_SB_TOOLS=docker-cli@29.9.1"]),
        "{run:?}"
    );
    assert!(
        !run.iter().any(|word| word.contains("docker.sock")),
        "the sandbox never has the socket: {run:?}"
    );
    assert!(
        !run.iter().any(|word| word.starts_with("TAW_SB_DOCKER")),
        "{run:?}"
    );
    assert!(
        stderr(&out).contains("; docker inside, read-only\n"),
        "{}",
        stderr(&out)
    );
}

/// Waits up to [`STOP_WITHIN`] seconds for the fake to have been asked to remove the proxy `proxy`
/// and its network.
fn proxy_removed(rig: &Rig, proxy: &str) -> bool {
    let removed = |calls: &[Vec<String>]| {
        calls.iter().any(|call| call[1..] == ["rm", "-f", proxy])
            && calls
                .iter()
                .any(|call| call[1..] == ["network", "rm", proxy])
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(STOP_WITHIN);
    while !removed(&rig.fake.calls()) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    removed(&rig.fake.calls())
}

#[test]
fn the_watcher_removes_the_proxy_and_its_network_when_the_sandbox_ends() {
    let rig = Rig::new();
    let read_only = rig.with_recipe("docker=read-only");

    rig.run_profile("/bin/bash", &read_only, &["claude"], &[]);

    let run = rig.the_run();
    let proxy = format!(
        "{}-docker",
        run[run.iter().position(|word| word == "--name").unwrap() + 1]
    );
    assert!(proxy_removed(&rig, &proxy), "{:?}", rig.fake.calls());
}

/// With the project's Compose services running, the proxy keeps a network only it and the sandbox
/// are on, and the sandbox is on Compose's as well: both are its own, for block_local_network.
#[test]
fn with_compose_running_the_proxy_keeps_its_own_network_and_the_sandbox_joins_both() {
    let rig = Rig::new();
    let (network, subnet) = compose_running(&rig);
    let read_only = rig.with_recipe("docker=read-only");

    rig.run_profile("/bin/bash", &read_only, &["claude"], &[]);

    let run = rig.the_run();
    let proxy = format!(
        "{}-docker",
        run[run.iter().position(|word| word == "--name").unwrap() + 1]
    );
    let started: Vec<Vec<String>> = rig
        .fake
        .calls()
        .into_iter()
        .filter(|call| holds(call, &["run", "-d"]))
        .collect();
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(
        holds(&started[0], &["--network", &proxy]),
        "{:?}",
        started[0]
    );
    assert!(
        !started[0].contains(&network),
        "the proxy is never on Compose's network: {:?}",
        started[0]
    );
    assert!(
        holds(&run, &["--network", &network]) && holds(&run, &["--network", &proxy]),
        "{run:?}"
    );
    // The fake answers every network with the same subnet: one comes from each.
    assert!(
        holds(&run, &["-e", &format!("TAW_SB_ALLOW={subnet} {subnet}")]),
        "{run:?}"
    );
}

#[test]
fn an_image_that_does_not_build_starts_no_proxy() {
    let rig = Rig::new();
    let read_only = rig.with_recipe("docker=read-only");

    let out = rig.run_profile(
        "/bin/bash",
        &read_only,
        &["claude"],
        &[("FAKE_RUNTIME_BUILD_STATUS", "1")],
    );

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let calls = rig.fake.calls();
    assert!(
        !calls
            .iter()
            .any(|call| holds(call, &["network", "create"]) || holds(call, &["run", "-d"])),
        "{calls:?}"
    );
}

/// The watcher runs before the proxy starts: a proxy that does not start takes the profile down with
/// a line, and its network is taken away all the same.
#[test]
fn a_proxy_that_does_not_start_starts_nothing_and_its_network_is_taken_away() {
    let rig = Rig::new();
    let read_only = rig.with_recipe("docker=read-only");

    let out = rig.run_profile(
        "/bin/bash",
        &read_only,
        &["claude"],
        &[("FAKE_RUNTIME_DETACH_STATUS", "125")],
    );

    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).ends_with(
            "taw sandbox default: the read-only Docker proxy did not start, so the agent was not started\n"
        ),
        "{}",
        stderr(&out)
    );
    assert!(rig.fake.runs().is_empty());
    let created = rig
        .fake
        .calls()
        .into_iter()
        .find(|call| call.len() > 3 && call[1] == "network" && call[2] == "create")
        .expect("the proxy's network was made");
    assert!(proxy_removed(&rig, &created[3]), "{:?}", rig.fake.calls());
}

/// Print mode asks the runtime nothing, so it starts no proxy: it says, on a second line, that a
/// start runs one.
#[test]
fn print_mode_starts_no_proxy_and_says_a_start_runs_one() {
    let rig = Rig::new();
    let read_only = rig.with_recipe("docker=read-only");

    let out = rig.run_profile(
        "/bin/bash",
        &read_only,
        &["claude"],
        &[("TAW_SANDBOX_PRINT", "1")],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
    let printed = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        printed.lines().nth(1),
        Some(
            "# a start also runs a read-only Docker proxy, docker.io/tecnativa/docker-socket-proxy:0.3.0, on a network only the two share"
        ),
        "{printed}"
    );
}

#[test]
fn a_docker_line_this_profile_does_not_know_is_refused() {
    let rig = Rig::new();
    let other = edited(&rig, "#   docker=full\n", "  docker=yes\n");

    let out = rig.run_profile("/bin/bash", &other, &["claude"], &[]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: docker=yes is not one of this profile's recipes (read-only or full)\n"
    );
    assert!(rig.fake.calls().is_empty(), "{:?}", rig.fake.calls());
}

#[test]
fn docker_in_the_environment_changes_nothing() {
    exported_changes_nothing("docker", "full");
}

/// The test plan: each recipe line changes exactly its own arguments. Every line runs in the one rig,
/// so that the project, its package folders' volumes and the home are those of the first run.
#[test]
fn each_recipe_line_changes_exactly_its_own_arguments() {
    type Change = Box<dyn Fn(&[String]) -> Vec<String>>;
    let rig = Rig::new();
    rig.run(&["claude"]);
    let base = normalised(&rig.the_run());
    let without = |words: &[String], drop: &[&str]| -> Vec<String> {
        let mut out = words.to_vec();
        if let Some(at) = out
            .windows(drop.len())
            .position(|w| w.iter().zip(drop).all(|(a, b)| a == b))
        {
            out.drain(at..at + drop.len());
        }
        out
    };
    let replaced = |words: &[String], from: &str, to: &str| -> Vec<String> {
        words
            .iter()
            .map(|word| {
                if word == from {
                    to.to_string()
                } else {
                    word.clone()
                }
            })
            .collect()
    };
    let cases: Vec<(&str, Change)> = vec![
        (
            "ask_first=yes",
            Box::new(move |b| without(b, &["--dangerously-skip-permissions"])),
        ),
        (
            "block_local_network=yes",
            Box::new(move |b| replaced(b, "TAW_SB_BLOCK_LOCAL=no", "TAW_SB_BLOCK_LOCAL=yes")),
        ),
        (
            "open_ports=\"8317\"",
            Box::new(move |b| replaced(b, "TAW_SB_OPEN_PORTS=", "TAW_SB_OPEN_PORTS=8317")),
        ),
        (
            "tools=\"node@22 uv\"",
            Box::new(move |b| replaced(b, "TAW_SB_TOOLS=", "TAW_SB_TOOLS=node@22 uv")),
        ),
        (
            "docker=full",
            Box::new(move |b| {
                let mut out = replaced(b, "TAW_SB_TOOLS=", "TAW_SB_TOOLS=docker-cli@29.9.1");
                let at = out
                    .iter()
                    .position(|w| w.starts_with("TAW_SB_OWN="))
                    .unwrap()
                    + 1;
                out.splice(
                    at..at,
                    [
                        "-v".to_string(),
                        "/var/run/docker.sock:/var/run/docker.sock".to_string(),
                        "-e".to_string(),
                        "TAW_SB_DOCKER=full".to_string(),
                    ],
                );
                out
            }),
        ),
        (
            "runtime=podman",
            Box::new(|b| {
                let mut out = b.to_vec();
                out[0] = "podman".to_string();
                let at = out.iter().position(|w| w == "--security-opt").unwrap() + 2;
                out.splice(at..at, ["--userns".to_string(), "keep-id".to_string()]);
                out
            }),
        ),
    ];
    for (line, expected) in cases {
        let profile = rig.with_recipe(line);
        let out = rig.run_profile("/bin/bash", &profile, &["claude"], &[]);
        assert!(out.status.success(), "{line}: {}", stderr(&out));
        let last = rig.fake.runs().pop().expect("the run of this line");
        assert_eq!(normalised(&last), expected(&base), "{line}");
    }
}

/// The start script, as the first build put it in the image, with fakes for the programs only a
/// container has. Every fake writes its arguments, one call to a line, to `calls`.
struct Inside {
    _rig: Rig,
    dir: PathBuf,
    script: PathBuf,
    calls: PathBuf,
}

impl Inside {
    fn new() -> Inside {
        let rig = Rig::new();
        rig.run(&["claude"]);
        let dir = rig.base.join("inside");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = dir.join("taw-sandbox-start");
        std::fs::write(&script, rig.fake.built("taw-sandbox-start")).unwrap();
        crate::common::make_executable(&script);
        let calls = dir.join("calls");
        let fake = |name: &str, body: &str| {
            let path = bin.join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> \"$INSIDE_CALLS\"\n{body}\n"),
            )
            .unwrap();
            crate::common::make_executable(&path);
        };
        fake("iptables", "exit \"${FAKE_IPTABLES_STATUS:-0}\"");
        fake("ip6tables", "exit \"${FAKE_IPTABLES_STATUS:-0}\"");
        fake(
            "getent",
            "printf '192.168.65.254  STREAM host.docker.internal\\n192.168.65.254  DGRAM\\n'",
        );
        fake("ip", "printf 'default via 172.17.0.1 dev eth0\\n'");
        // Root's chown in a container: a test only reads what it was asked.
        fake("chown", "");
        // It runs the start script again with the shell under test, as you.
        fake(
            "setpriv",
            "while [ \"$1\" != -- ]; do shift; done; shift; exec \"$INSIDE_SH\" \"$@\"",
        );
        // The group of the Docker socket, as `stat -c %g <file>` prints it: the number a test names.
        fake("stat", "printf '%s\\n' \"${FAKE_SOCKET_GID:-0}\"");
        // An agent's own installer, as `curl -fsSL <its address>` gives it: the script a test names.
        fake("curl", "cat \"${FAKE_INSTALLER:-/dev/null}\"");
        // The agent mise starts writes down the TAW_SB_ variables it was given, and, asked to, sends
        // itself an interrupt: one that it ignores, it lives through, and says so. Its install of
        // the project's tools says a line, as mise does.
        fake(
            "mise",
            "case \"$1\" in where) exit \"${FAKE_MISE_WHERE:-0}\" ;; which) exit \"${FAKE_MISE_WHICH:-0}\" ;; install) echo 'mise put in the project tools' >&2 ;; exec) env | sed -n 's/^\\(TAW_SB_[A-Z_]*\\)=.*/\\1/p' | sort > \"$INSIDE_ENV\"; printf 'PATH=%s\\nMISE_CACHE_DIR=%s\\nMISE_CONFIG_DIR=%s\\nMISE_DATA_DIR=%s\\n' \"$PATH\" \"${MISE_CACHE_DIR:-}\" \"${MISE_CONFIG_DIR:-}\" \"${MISE_DATA_DIR:-}\" > \"$INSIDE_MISE\"; [ -z \"${FAKE_AGENT_INTERRUPT:-}\" ] || { kill -INT $$; echo survived >> \"$INSIDE_CALLS\"; }; exit \"${FAKE_AGENT_STATUS:-0}\" ;; esac",
        );
        std::fs::write(dir.join("resolv.conf"), "nameserver 192.168.65.7\n").unwrap();
        std::fs::write(
            dir.join("if_inet6"),
            "00000000000000000000000000000001 01 80 10 80       lo\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("memory.events"),
            "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n",
        )
        .unwrap();
        Inside {
            _rig: rig,
            dir,
            script,
            calls,
        }
    }

    /// The start script for Claude, its switch and `--resume x`, with `extra` added.
    fn run(&self, extra: &[(&str, &str)]) -> Output {
        self.run_agent(
            &["claude", "--dangerously-skip-permissions", "--resume", "x"],
            extra,
        )
    }

    /// The start script as the image runs it, with `args` (the agent's command, its switch, its
    /// arguments), the variables the profile passes, and `extra` after them, under this machine's
    /// own sh.
    fn run_agent(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.run_under("/bin/sh", args, extra)
    }

    /// [`Inside::run_agent`] under `shell`. Its environment is cleared first, with a scratch home
    /// and no history.
    fn run_under(&self, shell: &str, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
        let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
        let home = self.dir.join("home");
        let own = [
            self.dir.join("vol/node_modules"),
            self.dir.join("vol/mise"),
            home.join(".cache"),
        ];
        let mut cmd = Command::new(shell);
        cmd.arg(&self.script)
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("INSIDE_SH", shell)
            // The fakes first; `chown` is in /usr/sbin on macOS.
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.dir.join("bin").display()
                ),
            )
            .env("HOME", &home)
            .env("HISTFILE", "/dev/null")
            .env("XDG_CONFIG_HOME", self.dir.join("xdg-config"))
            .env("XDG_STATE_HOME", self.dir.join("xdg-state"))
            .env("INSIDE_CALLS", &self.calls)
            .env("INSIDE_ENV", self.dir.join("env"))
            .env("INSIDE_MISE", self.dir.join("mise-env"))
            .env("TAW_SB_PROFILE", "default")
            .env("TAW_SB_UID", uid.trim())
            .env("TAW_SB_GID", gid.trim())
            .env("TAW_SB_INSTALL", "claude")
            .env("TAW_SB_VERSION", "2.1.286")
            .env("TAW_SB_TOOLS", "")
            .env("TAW_SB_BLOCK", "192.168.1.20")
            .env("TAW_SB_OPEN_PORTS", "8317")
            .env("TAW_SB_BLOCK_LOCAL", "no")
            .env("TAW_SB_ALLOW", "")
            .env("TAW_SB_MOUNTS", home.join(".cache"))
            .env(
                "TAW_SB_OWN",
                own.iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .env("TAW_SB_RESOLV", self.dir.join("resolv.conf"))
            .env("TAW_SB_IF_INET6", self.dir.join("if_inet6"))
            .env("TAW_SB_MEMORY_EVENTS", self.dir.join("memory.events"))
            .env("TAW_SB_MISE_DIR", self.dir.join("vol/mise"));
        for (name, value) in extra {
            cmd.env(name, value);
        }
        ended(cmd)
    }

    fn calls(&self, program: &str) -> Vec<String> {
        std::fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                line.strip_prefix(&format!("{program} "))
                    .map(str::to_string)
            })
            .collect()
    }
}

#[test]
fn the_rules_refuse_this_machine_and_let_its_open_ports_and_the_name_servers_through() {
    let inside = Inside::new();

    let out = inside.run(&[]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        inside.calls("iptables"),
        [
            "-A OUTPUT -d 192.168.65.7 -p udp --dport 53 -j ACCEPT",
            "-A OUTPUT -d 192.168.65.7 -p tcp --dport 53 -j ACCEPT",
            "-A OUTPUT -d 192.168.1.20 -p tcp --dport 8317 -j ACCEPT",
            "-A OUTPUT -d 192.168.1.20 -j REJECT",
            "-A OUTPUT -d 192.168.65.254 -p tcp --dport 8317 -j ACCEPT",
            "-A OUTPUT -d 192.168.65.254 -j REJECT",
            "-A OUTPUT -d 172.17.0.1 -p tcp --dport 8317 -j ACCEPT",
            "-A OUTPUT -d 172.17.0.1 -j REJECT",
        ]
    );
    assert!(
        inside.calls("ip6tables").is_empty(),
        "a container with no IPv6 needs no IPv6 rules"
    );
}

#[test]
fn an_ipv6_address_is_refused_with_ip6tables_when_the_container_has_ipv6() {
    let inside = Inside::new();
    std::fs::write(
        inside.dir.join("if_inet6"),
        "fd000000000000000000000000000002 02 40 00 80     eth0\n00000000000000000000000000000001 01 80 10 80       lo\n",
    )
    .unwrap();

    inside.run(&[
        ("TAW_SB_BLOCK", "192.168.1.20 fd12:3456::1"),
        ("TAW_SB_OPEN_PORTS", ""),
    ]);

    assert!(
        inside
            .calls("ip6tables")
            .contains(&"-A OUTPUT -d fd12:3456::1 -j REJECT".to_string()),
        "{:?}",
        inside.calls("ip6tables")
    );
}

#[test]
fn block_local_network_refuses_the_local_ranges_after_the_sandboxs_own_networks() {
    let inside = Inside::new();

    inside.run(&[
        ("TAW_SB_BLOCK_LOCAL", "yes"),
        ("TAW_SB_ALLOW", "172.18.0.0/16"),
        ("TAW_SB_OPEN_PORTS", ""),
    ]);

    let rules = inside.calls("iptables");
    let tail: Vec<&str> = rules
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "-A OUTPUT -d 172.18.0.0/16 -j ACCEPT",
            "-A OUTPUT -d 10.0.0.0/8 -j REJECT",
            "-A OUTPUT -d 172.16.0.0/12 -j REJECT",
            "-A OUTPUT -d 192.168.0.0/16 -j REJECT",
            "-A OUTPUT -d 169.254.0.0/16 -j REJECT",
        ]
    );
    // The local IPv6 ranges need IPv6, which this container has not.
    assert!(
        inside.calls("ip6tables").is_empty(),
        "{:?}",
        inside.calls("ip6tables")
    );
}

#[test]
fn rules_that_cannot_be_set_stop_the_agent_from_starting() {
    let inside = Inside::new();

    let out = inside.run(&[("FAKE_IPTABLES_STATUS", "1")]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "taw sandbox default: the network rules could not be set, so the agent does not start\n"
    );
    assert!(inside.calls("setpriv").is_empty() && inside.calls("mise").is_empty());
}

#[test]
fn it_becomes_you_with_no_capabilities_and_hands_you_the_volumes_folders_first() {
    let inside = Inside::new();

    inside.run(&[]);

    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
    assert_eq!(
        inside.calls("setpriv"),
        [format!(
            "--reuid {} --regid {} --clear-groups --inh-caps=-all --bounding-set=-all --no-new-privs -- {} --as-you claude --dangerously-skip-permissions --resume x",
            uid.trim(),
            gid.trim(),
            inside.script.display()
        )]
    );
    for folder in ["vol/node_modules", "vol/mise", "home/.cache"] {
        assert!(inside.dir.join(folder).is_dir(), "{folder}");
    }
}

#[test]
fn the_agent_starts_through_mise_at_the_version_asked_with_its_arguments_unchanged() {
    let inside = Inside::new();

    let first = inside.run(&[("FAKE_MISE_WHERE", "1")]);
    let again = inside.run(&[]);

    // The line as it shows: no name in front.
    assert!(
        stderr(&first).lines().any(|line| line
            == "installing claude 2.1.286 and the tools in mise.toml (once for each version)"),
        "{}",
        stderr(&first)
    );
    assert!(!stderr(&again).contains("installing"), "{}", stderr(&again));
    let execs: Vec<String> = inside
        .calls("mise")
        .into_iter()
        .filter(|call| call.starts_with("exec"))
        .collect();
    assert_eq!(
        execs[0],
        "exec --yes claude@2.1.286 -- claude --dangerously-skip-permissions --resume x"
    );
}

/// With no version known, mise's latest: the line says so in words.
#[test]
fn an_agent_with_no_version_known_is_installed_at_the_latest() {
    let inside = Inside::new();

    let out = inside.run_agent(
        &["codex", "--dangerously-bypass-approvals-and-sandbox"],
        &[
            ("TAW_SB_INSTALL", "npm:@openai/codex"),
            ("TAW_SB_VERSION", "latest"),
            ("FAKE_MISE_WHERE", "1"),
        ],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "installing the latest codex and the tools in mise.toml (once for each version)\n"
    );
    let execs: Vec<String> = inside
        .calls("mise")
        .into_iter()
        .filter(|call| call.starts_with("exec"))
        .collect();
    assert_eq!(
        execs,
        ["exec --yes npm:@openai/codex@latest -- codex --dangerously-bypass-approvals-and-sandbox"]
    );
}

/// Cursor, Droid and Kimi come with installers of their own: installed once, as you, into the volume
/// every sandbox shares, where the next start finds the agent.
#[test]
fn an_agent_with_its_own_installer_is_installed_once_into_the_shared_volume() {
    let inside = Inside::new();
    let installer = inside.dir.join("installer.sh");
    std::fs::write(
        &installer,
        "mkdir -p \"$HOME/.local/bin\"\nprintf '#!/bin/sh\\n' > \"$HOME/.local/bin/cursor-agent\"\nchmod +x \"$HOME/.local/bin/cursor-agent\"\n",
    )
    .unwrap();
    let cursor = [
        ("TAW_SB_INSTALL", "installer:https://cursor.com/install"),
        ("TAW_SB_VERSION", "latest"),
        ("FAKE_INSTALLER", installer.to_str().unwrap()),
    ];

    let first = inside.run_agent(&["cursor-agent", "--force"], &cursor);
    let again = inside.run_agent(&["cursor-agent", "--force"], &cursor);

    assert!(
        first.status.success() && again.status.success(),
        "{}{}",
        stderr(&first),
        stderr(&again)
    );
    assert_eq!(
        stderr(&first),
        "installing cursor-agent with its own installer (the first start only)\n"
    );
    assert_eq!(stderr(&again), "");
    assert_eq!(
        inside.calls("curl"),
        ["-fsSL https://cursor.com/install"],
        "installed once"
    );
    assert!(
        inside
            .dir
            .join("vol/mise/agents/.local/bin/cursor-agent")
            .exists()
    );
    let execs: Vec<String> = inside
        .calls("mise")
        .into_iter()
        .filter(|call| call.starts_with("exec"))
        .collect();
    assert_eq!(
        execs,
        [
            "exec --yes -- cursor-agent --force",
            "exec --yes -- cursor-agent --force"
        ]
    );
}

/// The project's mise.toml is trusted inside the sandbox and installed, after the line that says
/// what is being installed.
#[test]
fn the_projects_mise_toml_is_trusted_inside_and_installed() {
    let inside = Inside::new();
    std::fs::write(inside.dir.join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();

    let out = inside.run(&[("FAKE_MISE_WHERE", "1")]);

    let calls = inside.calls("mise");
    assert!(calls.contains(&"trust --yes .".to_string()), "{calls:?}");
    assert!(calls.contains(&"install --yes".to_string()), "{calls:?}");
    assert_eq!(
        stderr(&out),
        "installing claude 2.1.286 and the tools in mise.toml (once for each version)\n\
         mise put in the project tools\n"
    );
}

#[test]
fn an_out_of_memory_kill_is_said_in_those_words() {
    let inside = Inside::new();
    std::fs::write(
        inside.dir.join("memory.events"),
        "low 0\nhigh 0\nmax 4\noom 1\noom_kill 1\n",
    )
    .unwrap();
    let status: i32 = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sandbox/oom-status.txt"),
    )
    .unwrap()
    .trim()
    .parse()
    .unwrap();

    let out = inside.run(&[("FAKE_AGENT_STATUS", &status.to_string())]);

    assert_eq!(out.status.code(), Some(status));
    assert!(
        stderr(&out).ends_with(
            "taw sandbox default: claude was killed for lack of memory: give the runtime's VM more (Docker Desktop: Settings, Resources, Memory)\n"
        ),
        "{}",
        stderr(&out)
    );
}

/// Ctrl-C is the agent's: the start script lets it through with its own meaning (the agent here
/// ends on it), never ignored, and waits on to say how the agent ended.
#[test]
fn an_interrupt_reaches_the_agent_as_an_interrupt() {
    let inside = Inside::new();

    let out = inside.run(&[("FAKE_AGENT_INTERRUPT", "1")]);

    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    assert!(
        !std::fs::read_to_string(&inside.calls)
            .unwrap()
            .contains("survived"),
        "the agent ignored it"
    );
}

#[test]
fn the_profiles_own_variables_are_gone_before_the_agent_starts() {
    let inside = Inside::new();

    inside.run(&[]);

    assert_eq!(std::fs::read_to_string(inside.dir.join("env")).unwrap(), "");
}

/// The base image sets mise up for root: a config that installs node and python in every sandbox,
/// with its cache in root's folder and its shims in front. The recipe takes the config out.
#[test]
fn the_image_takes_the_base_images_mise_config_out() {
    let rig = Rig::new();
    rig.run(&["claude"]);

    let dockerfile = rig.fake.built("Dockerfile");

    let lines: Vec<&str> = dockerfile.lines().collect();
    let chmod = lines
        .iter()
        .position(|line| *line == "RUN chmod 755 /usr/local/bin/taw-sandbox-start")
        .expect("the chmod line");
    assert_eq!(lines[chmod + 1], "RUN rm -f /mise/config.toml", "{lines:?}");
}

/// As you, mise keeps its cache in your home and its config in the shared volume, and the base
/// image's shims, which are root's, are out of the way wherever PATH has them.
#[test]
fn as_you_mise_has_a_cache_and_a_config_of_its_own_and_no_root_shims() {
    let inside = Inside::new();
    let bin = inside.dir.join("bin").display().to_string();
    let rest = format!("{bin}:/usr/bin:/bin:/usr/sbin:/sbin");
    let cases = [
        ("front", format!("/mise/shims:{rest}")),
        (
            "middle",
            format!("{bin}:/mise/shims:/usr/bin:/bin:/usr/sbin:/sbin"),
        ),
        ("end", format!("{rest}:/mise/shims")),
        ("twice", format!("/mise/shims:{rest}:/mise/shims")),
        ("never", rest.clone()),
    ];
    // The image's /bin/sh is dash: where this machine has it, the script runs under it too.
    let shells: Vec<&str> = ["/bin/sh", "/bin/dash"]
        .into_iter()
        .filter(|shell| Path::new(shell).exists())
        .collect();
    for ((place, path), shell) in cases
        .iter()
        .flat_map(|case| shells.iter().map(move |shell| (case, shell)))
    {
        let out = inside.run_under(
            shell,
            &["claude", "--dangerously-skip-permissions", "--resume", "x"],
            &[("PATH", path)],
        );
        let place = format!("{place} under {shell}");

        assert!(out.status.success(), "{place}: {}", stderr(&out));
        let seen = std::fs::read_to_string(inside.dir.join("mise-env")).unwrap();
        let home = inside.dir.join("home");
        let vol = inside.dir.join("vol/mise");
        let line = |name: &str| {
            seen.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(
            line("MISE_CACHE_DIR"),
            format!("{}/.cache/mise", home.display()),
            "{place}"
        );
        assert_eq!(
            line("MISE_CONFIG_DIR"),
            format!("{}/config", vol.display()),
            "{place}"
        );
        assert_eq!(line("MISE_DATA_DIR"), vol.display().to_string(), "{place}");
        let path = line("PATH");
        assert!(
            !path.split(':').any(|dir| dir == "/mise/shims"),
            "{place}: {path}"
        );
        assert!(path.ends_with(&rest), "{place}: {path}");
    }
}

/// An agent mise installs with npm needs node. The project's own node wins: the global one is
/// installed only when mise finds no npm, and only for an npm agent.
#[test]
fn an_npm_agent_with_no_npm_gets_node_once_and_one_with_npm_or_another_install_does_not() {
    let uses = |inside: &Inside| -> Vec<String> {
        inside
            .calls("mise")
            .into_iter()
            .filter(|call| call.starts_with("use"))
            .collect()
    };
    let codex = [
        ("TAW_SB_INSTALL", "npm:@openai/codex"),
        ("TAW_SB_VERSION", "latest"),
    ];

    let none = Inside::new();
    let out = none.run_agent(&["codex"], &[codex[0], codex[1], ("FAKE_MISE_WHICH", "1")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(uses(&none), ["use -g node@lts"]);
    assert!(
        stderr(&out)
            .lines()
            .any(|line| line == "installing node for codex (once)"),
        "{}",
        stderr(&out)
    );
    let asked: Vec<String> = none
        .calls("mise")
        .into_iter()
        .filter(|call| call.starts_with("which") || call.starts_with("use"))
        .collect();
    assert_eq!(
        asked,
        ["which npm", "use -g node@lts"],
        "asked before it installs"
    );

    let has = Inside::new();
    let out = has.run_agent(&["codex"], &codex);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(uses(&has).is_empty(), "{:?}", uses(&has));
    assert!(!stderr(&out).contains("node"), "{}", stderr(&out));

    let other = Inside::new();
    let out = other.run(&[("FAKE_MISE_WHICH", "1")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(uses(&other).is_empty(), "{:?}", uses(&other));
    assert!(
        !other
            .calls("mise")
            .iter()
            .any(|call| call.starts_with("which")),
        "no question asked for an agent that is not npm"
    );
}

/// docker=full: the socket is root's group inside, so the agent keeps that one group; any other mode
/// keeps none, and so does a full with no socket to read.
#[test]
fn docker_full_keeps_the_sockets_group_for_the_agent_and_every_other_mode_none() {
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
    let cases: [(&str, Option<&str>, bool, &str); 6] = [
        ("full, group 0", Some("full"), true, "0"),
        ("full, group 998", Some("full"), true, "998"),
        ("read-only", Some("read-only"), true, "998"),
        ("no docker", None, true, "998"),
        ("full, no socket", Some("full"), false, "998"),
        ("full, a group that is no number", Some("full"), true, "x y"),
    ];
    for (label, mode, socket, group) in cases {
        let inside = Inside::new();
        let sock = inside.dir.join("docker.sock");
        if socket {
            std::fs::write(&sock, "").unwrap();
        }
        let sock = sock.display().to_string();
        let mut extra = vec![
            ("TAW_SB_DOCKER_SOCK", sock.as_str()),
            ("FAKE_SOCKET_GID", group),
        ];
        if let Some(mode) = mode {
            extra.push(("TAW_SB_DOCKER", mode));
        }

        let out = inside.run(&extra);

        assert!(out.status.success(), "{label}: {}", stderr(&out));
        let groups = if mode == Some("full") && socket && group != "x y" {
            format!("--groups {group}")
        } else {
            "--clear-groups".to_string()
        };
        assert_eq!(
            inside.calls("setpriv"),
            [format!(
                "--reuid {} --regid {} {groups} --inh-caps=-all --bounding-set=-all --no-new-privs -- {} --as-you claude --dangerously-skip-permissions --resume x",
                uid.trim(),
                gid.trim(),
                inside.script.display()
            )],
            "{label}"
        );
        assert_eq!(
            std::fs::read_to_string(inside.dir.join("env")).unwrap(),
            "",
            "{label}: the mode is gone before the agent starts"
        );
    }
}

/// The image's /bin/sh is dash, Debian's: where this machine has dash, the start script does under
/// it what it does under this machine's own sh, from the rules to the agent's start, and an
/// interrupt still reaches the agent.
#[test]
fn the_start_script_runs_the_same_under_dash() {
    use std::io::Write;

    if !Path::new("/bin/dash").exists() {
        let _ = writeln!(
            std::io::stderr(),
            "the_start_script_runs_the_same_under_dash: this machine has no /bin/dash to run it with"
        );
        return;
    }
    let mut seen = Vec::new();
    for shell in ["/bin/sh", "/bin/dash"] {
        let inside = Inside::new();
        std::fs::write(
            inside.dir.join("if_inet6"),
            "fd000000000000000000000000000002 02 40 00 80     eth0\n",
        )
        .unwrap();
        std::fs::write(inside.dir.join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let home = inside.dir.join("home");
        let mount = home.join(".local/share/opencode");
        std::fs::create_dir_all(&mount).unwrap();
        let mounts = format!("{}\n{}", home.join(".cache").display(), mount.display());
        let args = [
            "claude",
            "--dangerously-skip-permissions",
            "--resume",
            "x y",
        ];

        let out = inside.run_under(
            shell,
            &args,
            &[
                ("TAW_SB_BLOCK", "192.168.1.20 fd12:3456::1"),
                ("TAW_SB_BLOCK_LOCAL", "yes"),
                ("TAW_SB_ALLOW", "172.18.0.0/16"),
                ("TAW_SB_TOOLS", "node@22 uv"),
                ("TAW_SB_MOUNTS", &mounts),
                ("FAKE_MISE_WHERE", "1"),
            ],
        );
        let interrupted = inside.run_under(shell, &args, &[("FAKE_AGENT_INTERRUPT", "1")]);

        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        assert_eq!(
            interrupted.status.code(),
            Some(130),
            "{shell}: {}",
            stderr(&interrupted)
        );
        let calls = std::fs::read_to_string(&inside.calls)
            .unwrap()
            .replace(&inside.dir.display().to_string(), "<inside>");
        assert!(
            calls.contains("mise exec --yes claude@2.1.286 node@22 uv -- claude"),
            "{shell}: {calls}"
        );
        seen.push((calls, stderr(&out)));
    }
    assert_eq!(seen[0], seen[1], "/bin/sh, then /bin/dash");
}

/// What part 2 of the profile adds runs the same under every bash here, macOS's 3.2 and bash 5: a
/// start in a folder of a checkout with a deeper package folder and Compose running, with the
/// recipe lines settings=own, block_local_network, open_ports and docker=read-only uncommented.
/// The container's name, random, and the proxy's, made from it, are put as `<name>`.
#[test]
fn every_bash_here_runs_every_recipe_the_same() {
    // Warm: neither start builds the image.
    let rig = Rig::new().warm();
    git(&rig.project, &["init", "-q"]);
    let src = rig.project.join("src");
    std::fs::create_dir_all(src.join("web").join("node_modules")).unwrap();
    compose_running(&rig);
    let mut text = taw::sandbox::DEFAULT_PROFILE.to_string();
    for line in [
        "settings=own",
        "block_local_network=yes",
        "open_ports=\"8317\"",
        "docker=read-only",
    ] {
        let before = text.clone();
        text = text.replace(&format!("#   {line}\n"), &format!("  {line}\n"));
        assert_ne!(text, before, "no recipe line {line}");
    }
    let profile = rig.base.join("profiles").join("every-recipe");
    std::fs::write(&profile, text).unwrap();
    crate::common::make_executable(&profile);
    let mut bashes = vec!["/bin/bash".to_string()];
    if let Ok(found) = which::which("bash") {
        let same = std::fs::canonicalize(&found).ok() == std::fs::canonicalize("/bin/bash").ok();
        if !same {
            bashes.push(found.display().to_string());
        }
    }

    let mut seen = Vec::new();
    for bash in &bashes {
        let log = rig.base.join(format!("calls-{}", seen.len()));
        let out = rig.run_profile(
            bash,
            &profile,
            &["claude", "--resume", "x"],
            &[
                ("TAW_SANDBOX_DIR", src.to_str().unwrap()),
                (
                    "TAW_STATE_DIR",
                    rig.base.join("taw-state").to_str().unwrap(),
                ),
                ("FAKE_RUNTIME_LOG", log.to_str().unwrap()),
            ],
        );
        assert!(out.status.success(), "{bash}: {}", stderr(&out));
        let raw = std::fs::read_to_string(&log).unwrap();
        let run = raw
            .split("\x1e\n")
            .find(|call| call.starts_with("docker\x1frun\x1f") && !call.contains("\x1f-d\x1f"))
            .expect("the run")
            .to_string();
        let words: Vec<&str> = run.split('\x1f').collect();
        let name = words[words.iter().position(|word| *word == "--name").unwrap() + 1];
        // Every call up to the run, the proxy's included; the watcher's come after it.
        let upto: String = raw
            .split_inclusive("\x1e\n")
            .take_while(|call| !call.starts_with("docker\x1frun\x1f--rm"))
            .collect();
        seen.push((
            format!("{upto}{run}").replace(name, "<name>"),
            stderr(&out).replace(name, "<name>"),
        ));
    }
    assert!(
        seen.windows(2).all(|pair| pair[0] == pair[1]),
        "{bashes:?}: {seen:#?}"
    );
    assert!(
        seen[0].0.contains("\x1f--network\x1f<name>-docker\x1f"),
        "{:?}",
        seen[0]
    );
}

/// A CDPATH of yours, exported (`~/projects:.`, say), never moves where the profile looks for the
/// checkout's repository: bash's cd searches it for a folder such as .git, would find another
/// checkout's first, and says where it went.
#[test]
fn a_cdpath_of_yours_changes_nothing_in_a_checkout() {
    let rig = Rig::new();
    git(&rig.project, &["init", "-q"]);
    let src = rig.project.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let elsewhere = rig.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    git(&elsewhere, &["init", "-q"]);
    let cdpath = format!("{}:.", elsewhere.display());

    for dir in [&rig.project, &src] {
        let out = rig.run_with(
            &["claude"],
            &[
                ("CDPATH", cdpath.as_str()),
                ("TAW_SANDBOX_DIR", dir.to_str().unwrap()),
            ],
        );

        assert!(out.status.success(), "{}: {}", dir.display(), stderr(&out));
        let run = rig.fake.runs().pop().unwrap();
        let p = rig.project.display().to_string();
        assert!(
            holds(
                &run,
                &[
                    "-v",
                    &format!("{p}/.git/hooks:{p}/.git/hooks:ro"),
                    "-v",
                    &format!("{p}/.git/config:{p}/.git/config:ro"),
                ]
            ),
            "{}: {run:?}",
            dir.display()
        );
        assert!(
            !run.iter().any(|word| word.contains("elsewhere")),
            "{}: {run:?}",
            dir.display()
        );
    }
}

/// The mount points under your home go in, one to a line, in the run's order, for the start script:
/// the project's and its git parts' when the project is in your home, the volumes', and the
/// settings folders'; nothing mounted elsewhere (mise's volume, the report folder).
#[test]
fn the_mount_points_under_your_home_go_in_for_the_start_script() {
    let rig = Rig::new();
    let project = rig.home.join("work").join("api");
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q"]);
    let report = rig.base.join("panes").join("%31");
    std::fs::create_dir_all(&report).unwrap();

    let out = rig.run_with(
        &["opencode"],
        &[
            ("TAW_SANDBOX_DIR", project.to_str().unwrap()),
            ("TAW_SANDBOX_REPORT", report.to_str().unwrap()),
        ],
    );

    assert!(out.status.success(), "{}", stderr(&out));
    let run = rig.the_run();
    let mounts = run
        .iter()
        .find_map(|word| word.strip_prefix("TAW_SB_MOUNTS="))
        .expect("TAW_SB_MOUNTS");
    let (p, h) = (
        project.display().to_string(),
        rig.home.display().to_string(),
    );
    assert_eq!(
        mounts.lines().collect::<Vec<_>>(),
        [
            p.clone(),
            format!("{p}/.git/hooks"),
            format!("{p}/.git/config"),
            format!("{p}/node_modules"),
            format!("{p}/.venv"),
            format!("{h}/.cache"),
            format!("{h}/.config/opencode"),
            format!("{h}/.local/share/opencode"),
        ]
    );
}

/// Docker makes the folders between HOME and a mount point under it as root (~/.local and
/// ~/.local/share for opencode's ~/.local/share/opencode), where the agent could then write nothing
/// else (opencode's state, mise's trust in ~/.local/state). While still root, the start script hands
/// each to you, alone and not what is in it, as it hands HOME and the volumes: never a mount point,
/// nor a folder inside one (the project's .git), and nothing at HOME or above.
#[test]
fn the_folders_docker_makes_between_your_home_and_a_mount_are_handed_to_you() {
    let inside = Inside::new();
    let home = inside.dir.join("home");
    let project = home.join("work").join("api");
    let mounts = [
        project.clone(),
        project.join(".git/hooks"),
        home.join(".cache"),
        home.join(".config/opencode"),
        home.join(".local/share/opencode"),
    ];
    for mount in &mounts {
        std::fs::create_dir_all(mount).unwrap();
    }
    // A blank line among them names no folder.
    let listed = mounts
        .iter()
        .map(|mount| mount.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .replacen('\n', "\n\n", 1);

    let out = inside.run(&[("TAW_SB_MOUNTS", listed.as_str())]);

    assert!(out.status.success(), "{}", stderr(&out));
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let gid = String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout).unwrap();
    let mut handed = inside.calls("chown");
    handed.sort();
    handed.dedup();
    let mut expected: Vec<String> = [
        home.clone(),
        inside.dir.join("vol/node_modules"),
        inside.dir.join("vol/mise"),
        home.join(".cache"),
        home.join("work"),
        home.join(".config"),
        home.join(".local"),
        home.join(".local/share"),
    ]
    .iter()
    .map(|folder| format!("{}:{} {}", uid.trim(), gid.trim(), folder.display()))
    .collect();
    expected.sort();
    assert_eq!(handed, expected);
}

/// No git that these tests run reaches the repository that git's variables of whoever runs them point
/// at (git runs the command of a `rebase -x` with `GIT_DIR` set): the tests of this file that make a
/// repository, run again with each of the variables pointing into a decoy, pass and leave it byte for
/// byte as it was (tests/common/git_decoy.rs). Their git, and the profile they run, have an
/// environment of their own, which this shows holds.
#[test]
fn the_tests_that_make_a_repository_leave_the_one_gits_variables_point_at_as_it_was() {
    common::git_decoy::rerun(
        &[
            "checkout",
            "worktree",
            "a_project_",
            "a_folder_",
            "compose",
            "every_bash",
            "mount_points",
            "pulls_its_image",
            "settings_line",
            "submodule",
        ],
        "leave_the_one_gits_variables_point_at",
    );
}
