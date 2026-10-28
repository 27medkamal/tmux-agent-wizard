//! An agent that runs inside a container started from a pane, end to end through the built
//! `taw` binary and a real watcher on a throwaway tmux server.
//!
//! What a user's `box` leaves in the pane is a `docker` process whose arguments name
//! Claude Code, and the screen Claude draws inside the container. tmux names the process
//! `docker`, so nothing but the arguments says there is an agent at all. No real Docker is
//! ever run. The fake is a small program of this suite's own, compiled once with `rustc`
//! and copied to the name of the runtime: a real executable, so `#{pane_current_command}`
//! says `docker` (a shell script would say `sh`), that takes any arguments at all and waits
//! until a file exists. The commands are typed into the pane's shell, each argument quoted,
//! so the process has exactly the argument list the test names, as it does when a shell
//! function runs `docker`. The screens are written to the pane's tty by the test, so the
//! process in front and the screen drawn are two things, as they are with a container.

mod common;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{Server, ServerOptions, wait_for, write_script};

/// Ceiling for "the watcher noticed and wrote it". A new agent reads `unknown` for 3 s and
/// one more tick, and `cargo test` runs these files in parallel.
const SETTLE: Duration = Duration::from_secs(25);

const CONFIG: &str = "\
[update]
manifest_check = false

[ui.toast]
delivery = \"off\"

[ui.sound]
enabled = false
";

/// The fake runtime: it takes what arguments it is given, and runs until the file its
/// environment names exists.
const FAKE_RUNTIME: &str = r#"
use std::path::Path;
use std::time::Duration;

fn main() {
    let stop = std::env::var_os("FAKE_RUNTIME_STOP").expect("FAKE_RUNTIME_STOP names the file that stops it");
    while !Path::new(&stop).exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
}
"#;

/// The script a shell function such as a user's `box` gives `bash -lc`: one argument,
/// with a shell's separators, quotes and `$@` in it. The shape only, the real one is much
/// longer.
const BOX_SCRIPT: &str = "/usr/local/bin/sandbox-firewall.sh || true; mise install; eval \"$(mise activate bash)\"; claude --effort medium --dangerously-skip-permissions \"$@\"";

/// The same script over several lines, as a function's body is written.
const BOX_SCRIPT_LINES: &str = "
  /usr/local/bin/sandbox-firewall.sh || true
  mise install
  eval \"$(mise activate bash)\"
  claude --effort medium --dangerously-skip-permissions \"$@\"
";

/// The same again as a person comments and wraps it: a comment with an apostrophe in it, and
/// a line that goes on after a backslash right before claude.
const BOX_SCRIPT_COMMENTED: &str = "
  # Firewall first; it's optional here
  /usr/local/bin/sandbox-firewall.sh || true
  mise install
  eval \"$(mise activate bash)\"
  true && \\
    claude --effort medium --dangerously-skip-permissions \"$@\"
";

/// A user's `box` as its process has it, after the program's name.
fn box_run() -> Vec<&'static str> {
    box_with(&[], BOX_SCRIPT)
}

/// The same with `flags` before the entrypoint and `script` run by the shell.
fn box_with(flags: &[&'static str], script: &'static str) -> Vec<&'static str> {
    let mut args = vec![
        "run",
        "--rm",
        "-it",
        "-v",
        "/a/project:/a/project",
        "-v",
        "/h/.claude:/h/.claude",
        "-e",
        "CLAUDE_CONFIG_DIR=/h/.claude",
    ];
    args.extend_from_slice(flags);
    args.extend(["--entrypoint", "bash", "mise-sandbox", "-lc", script, "_"]);
    args
}

/// A second run, of Codex: what `docker run a claude; docker run b codex` follows the first
/// with.
fn codex() -> Vec<&'static str> {
    vec![
        "run",
        "--rm",
        "-it",
        "-v",
        "/src:/src",
        "dev-image",
        "codex",
    ]
}

/// Claude Code at its prompt in `--dangerously-skip-permissions` mode, as a user sees it.
const IDLE: &str = "\
\u{23fa} Done. The tests pass and the change is committed.
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
\u{276f}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)
";

/// The same Claude mid-turn.
const WORKING: &str = "\
\u{23fa} Bash(cd /a/project && ./run-tests.sh)
  \u{23bf}  Running\u{2026}
\u{273b} Verifying\u{2026} (12s \u{b7} \u{2193} 1.2k tokens \u{b7} esc to interrupt)
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
\u{276f}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt
";

/// The fake runtime, compiled: once for the tests of this file, and once for as long as its
/// source stays what it is. Built beside the other test output under a name that holds the
/// source's hash, and moved into place whole, so two runs at once never see half a file.
fn fake_runtime_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fake-container-runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let mut hasher = DefaultHasher::new();
        FAKE_RUNTIME.hash(&mut hasher);
        let binary = dir.join(format!("runtime-{:016x}", hasher.finish()));
        if !binary.exists() {
            let pid = std::process::id();
            let source = dir.join(format!("runtime-{pid}.rs"));
            let partial = dir.join(format!("runtime-{pid}.partial"));
            std::fs::write(&source, FAKE_RUNTIME).unwrap();
            // The compiler that is building this test, or the one on the path.
            let rustc = Path::new(env!("CARGO")).with_file_name("rustc");
            let rustc = if rustc.exists() {
                rustc
            } else {
                PathBuf::from("rustc")
            };
            let status = Command::new(rustc)
                .args(["--edition", "2021", "-o"])
                .arg(&partial)
                .arg(&source)
                .status()
                .expect("running rustc for the fake container runtime");
            assert!(status.success(), "rustc could not build the fake runtime");
            std::fs::rename(&partial, &binary).unwrap();
            let _ = std::fs::remove_file(&source);
        }
        binary
    })
}

/// Puts a fake runtime called `name` in `dir`: a copy of the compiled program, made before
/// anything runs it (an executable rewritten in place after it has run is killed on its next
/// exec by macOS's code-signing cache).
fn fake_runtime(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(fake_runtime_binary(), dir.join(name)).unwrap();
    crate::common::make_executable(dir.join(name));
}

/// A server whose panes have the fake runtimes in `bin_dir` first on their `PATH`, with the
/// watcher logging every decision.
fn server_with(bin_dir: PathBuf) -> Server {
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![("TAW_LOG".to_string(), "debug".to_string())],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    server
}

/// A server, and the directory for its stop files, with a fake runtime called each of `names`.
fn fixture(names: &[&str]) -> (tempfile::TempDir, Server) {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    for name in names {
        fake_runtime(&bin_dir, name);
    }
    let server = server_with(bin_dir);
    (tmp, server)
}

/// `word` quoted so that a shell reads it as that one argument, whatever it holds.
fn quoted(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// A runtime that has been started in a pane, and the way to stop it.
struct Started {
    stop: PathBuf,
}

impl Started {
    fn stop(&self) {
        std::fs::write(&self.stop, "").unwrap();
    }
}

/// The line that starts the runtime `runtime` with exactly `args`, and the runtime it started.
/// `stop_name` names the file whose appearance ends it.
fn command(tmp: &Path, stop_name: &str, runtime: &str, args: &[&str]) -> (String, Started) {
    let stop = tmp.join(stop_name);
    let args: Vec<String> = args.iter().map(|arg| quoted(arg)).collect();
    let line = format!(
        "FAKE_RUNTIME_STOP={} {runtime} {}",
        quoted(&stop.display().to_string()),
        args.join(" ")
    );
    (line, Started { stop })
}

/// Types one line into the pane's shell, once the shell is at its prompt: typed sooner, the line is cut at the
/// terminal's line buffer. Every caller types into a fresh pane, with no program in front of it.
fn type_line(server: &Server, pane: &str, line: &str) {
    server.wait_for_prompt(pane);
    server.tmux_ok(&["send-keys", "-t", pane, "-l", line]);
    server.tmux_ok(&["send-keys", "-t", pane, "Enter"]);
}

/// Waits until `name` is what tmux calls the pane's foreground command.
fn wait_for_command(server: &Server, pane: &str, name: &str) {
    assert!(
        wait_for(
            || server.display(pane, "#{pane_current_command}") == name,
            SETTLE
        ),
        "{name} never took the pane (it runs {:?})",
        server.display(pane, "#{pane_current_command}")
    );
}

/// Types `runtime` with `args` into the pane, as a user's shell would run `docker run ...`,
/// and waits until it is the pane's foreground command.
fn run_in_front(
    server: &Server,
    tmp: &Path,
    pane: &str,
    stop_name: &str,
    runtime: &str,
    args: &[&str],
) -> Started {
    let (line, started) = command(tmp, stop_name, runtime, args);
    type_line(server, pane, &line);
    wait_for_command(server, pane, runtime);
    started
}

/// What Claude draws, written to the pane's tty: a title and a whole screen.
fn draw(server: &Server, pane: &str, title: &str, screen: &str) {
    let tty = server.display(pane, "#{pane_tty}");
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty)
        .unwrap_or_else(|err| panic!("opening the pane's tty {tty}: {err}"));
    write!(
        out,
        "\x1b]2;{title}\x07\x1b[2J\x1b[H{}",
        screen.replace('\n', "\r\n")
    )
    .unwrap();
}

/// Draws Claude's prompt box and attaches the watcher.
fn watch_claude(server: &Server, pane: &str) {
    draw(server, pane, "\u{2733} Claude Code", IDLE);
    server.ok(&["watch", "attach", pane], "cli:watch:attach", "ok");
}

/// Waits for `@agent_kind` to be `kind`.
fn expect_kind(server: &Server, pane: &str, kind: &str, what: &str) {
    assert!(
        wait_for(|| server.pane_opt(pane, "@agent_kind") == kind, SETTLE),
        "{what}: wanted {kind}, got {:?} (container {:?}, command {:?})\n--- log ---\n{}",
        server.pane_opt(pane, "@agent_kind"),
        server.pane_opt(pane, "@agent_container"),
        server.display(pane, "#{pane_current_command}"),
        watcher_logs(server)
    );
}

/// Waits for `@agent_state` to reach `state` with `@agent_rule` at `rule`.
fn expect(server: &Server, pane: &str, state: &str, rule: &str) {
    assert!(
        wait_for(
            || server.pane_opt(pane, "@agent_state") == state
                && server.pane_opt(pane, "@agent_rule") == rule,
            SETTLE
        ),
        "wanted {state:?} by {rule:?}, got {:?} by {:?}\n--- screen ---\n{}\n--- log ---\n{}",
        server.pane_opt(pane, "@agent_state"),
        server.pane_opt(pane, "@agent_rule"),
        server.capture(pane),
        watcher_logs(server),
    );
}

/// Waits until nothing of an agent is left on the pane.
fn expect_no_agent(server: &Server, pane: &str, what: &str) {
    let options = [
        "@agent_kind",
        "@agent_state",
        "@agent_container",
        "@agent_rule",
    ];
    assert!(
        wait_for(
            || options
                .iter()
                .all(|option| server.pane_opt(pane, option).is_empty()),
            SETTLE
        ),
        "{what}: the record stayed: kind {:?}, state {:?}, container {:?}\n--- log ---\n{}",
        server.pane_opt(pane, "@agent_kind"),
        server.pane_opt(pane, "@agent_state"),
        server.pane_opt(pane, "@agent_container"),
        watcher_logs(server)
    );
}

/// The watcher's own log, for a failure message and for the checks that need to know it has
/// looked at the pane.
fn watcher_logs(server: &Server) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(server.state_dir().join("logs")) {
        for entry in entries.flatten() {
            out.push_str(&format!("== {}\n", entry.path().display()));
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    out
}

#[test]
fn a_claude_in_docker_is_recognised_and_read_from_its_screen() {
    let (tmp, server) = fixture(&["docker"]);
    let pane = server.pane();

    // The box: `docker` in front of the pane, Claude drawing its prompt box.
    let docker = run_in_front(&server, tmp.path(), &pane, "stop", "docker", &box_run());
    draw(&server, &pane, "\u{2733} Claude Code", IDLE);
    assert!(
        wait_for(
            || server.capture(&pane).contains("bypass permissions on"),
            SETTLE
        ),
        "the screen was never drawn:\n{}",
        server.capture(&pane)
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind"),
        "",
        "nothing has looked at the pane yet"
    );
    server.ok(&["watch", "attach", &pane], "cli:watch:attach", "ok");

    // Claude is named by the container's arguments, and its prompt box is idle.
    expect_kind(&server, &pane, "claude", "docker's arguments name Claude");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");
    expect(&server, &pane, "idle", "live_prompt_box");
    assert_eq!(server.pane_opt(&pane, "@agent_source"), "manifest");
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind_hint"),
        "",
        "identity came from the arguments, not from a hint"
    );

    // `agent explain` reads the pane the way the watcher does.
    let explained = server.ok(
        &["agent", "explain", &pane],
        "cli:agent:explain",
        "agent_explain",
    );
    assert_eq!(explained["state"], "idle", "{explained}");
    assert_eq!(
        explained["matched_rule"]["id"], "live_prompt_box",
        "{explained}"
    );

    // A turn.
    draw(&server, &pane, "\u{273b} Claude Code", WORKING);
    expect(&server, &pane, "working", "live_turn_working");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");

    // Back at the prompt with nobody looking at the pane: the work is finished, and a
    // finish behind the user's back is `done`, not `idle`.
    draw(&server, &pane, "\u{2733} Claude Code", IDLE);
    expect(&server, &pane, "done", "live_prompt_box");

    // The container stops, docker exits, and the pane's shell is back: nothing of the agent
    // is left on the pane, the marker least of all.
    docker.stop();
    expect_no_agent(&server, &pane, "the container stopped");
    assert_ne!(
        server.display(&pane, "#{pane_current_command}"),
        "docker",
        "the shell is back"
    );
}

/// The verbs that type find the agent in a container as the watcher does, by the arguments of the
/// runtime in front of the pane: a Claude in a docker gets what it is sent while the container runs, and
/// nothing is typed once docker has stopped and the shell is back with the agent's record standing (no
/// watcher is attached here to take it off).
#[test]
fn a_claude_in_docker_gets_its_prompt_while_it_runs_and_nothing_once_it_has_stopped() {
    let (tmp, server) = fixture(&["docker"]);
    let pane = server.pane();
    let docker = run_in_front(&server, tmp.path(), &pane, "stop", "docker", &box_run());
    let (code, envelope) = server.taw(&[
        "pane",
        "report-agent",
        &pane,
        "--source",
        "test",
        "--agent",
        "claude",
        "--state",
        "idle",
    ]);
    assert_eq!(code, 0, "{envelope}");
    let (code, envelope) = server.taw(&["agent", "rename", &pane, "rev"]);
    assert_eq!(code, 0, "{envelope}");

    let (code, envelope) = server.taw(&["agent", "prompt", "rev", "typed-while-it-runs"]);

    assert_eq!(code, 0, "{envelope}");
    assert!(
        wait_for(
            || server.capture(&pane).contains("typed-while-it-runs"),
            SETTLE
        ),
        "the prompt never reached the pane:\n{}",
        server.capture(&pane)
    );

    docker.stop();
    assert!(
        wait_for(
            || server.display(&pane, "#{pane_current_command}") != "docker",
            SETTLE
        ),
        "docker never ended"
    );
    let (code, envelope) = server.taw(&["agent", "prompt", "rev", "typed-after-it-stopped"]);
    assert_eq!(code, 1, "{envelope}");
    assert_eq!(
        envelope["error"]["code"].as_str(),
        Some("agent_not_ready"),
        "{envelope}"
    );
    assert!(
        !wait_for(
            || server.capture(&pane).contains("typed-after-it-stopped"),
            Duration::from_secs(2)
        ),
        "the prompt was typed into the shell:\n{}",
        server.capture(&pane)
    );
}

#[test]
fn a_script_over_several_lines_and_flags_full_of_metacharacters_are_read() {
    // What a real `docker run` has: a script written over several lines, with a comment that
    // has an apostrophe in it and a line that goes on after a backslash, a database URL with
    // an `&` in it, a health check with `||`, a prompt with parentheses and a `$`, a value
    // with a space, an empty argument. Each of them is one argument to the process, and none
    // is shell syntax to the reader. `ps` prints them joined by spaces, so a value with a
    // space and an empty argument are lost to it, which is why the exact list is read.
    let cases: [(&str, Vec<&str>); 7] = [
        (
            "a script over several lines",
            box_with(&[], BOX_SCRIPT_LINES),
        ),
        (
            "a comment with an apostrophe and a line that goes on after a backslash",
            box_with(&[], BOX_SCRIPT_COMMENTED),
        ),
        (
            "a database URL with an ampersand and a health check with an or",
            box_with(
                &[
                    "-e",
                    "DATABASE_URL=postgres://u:p@h/db?sslmode=disable&pool=5",
                    "--health-cmd",
                    "pg_isready||exit 1",
                ],
                BOX_SCRIPT,
            ),
        ),
        (
            "a prompt with parentheses and a dollar, and quotes",
            box_with(
                &[
                    "-e",
                    "PS1=(sandbox) $ ",
                    "-e",
                    "MOTD=it's \"quoted\"; `date`",
                ],
                BOX_SCRIPT_LINES,
            ),
        ),
        (
            "a value with a space",
            box_with(&["-e", "MSG=hello world", "--label", "a b=c d"], BOX_SCRIPT),
        ),
        (
            "a value with a space before the image of a direct run",
            vec!["run", "--rm", "-e", "MSG=hello world", "img", "claude"],
        ),
        (
            "an empty argument before the image of a direct run",
            vec!["run", "--rm", "--entrypoint", "", "img", "claude"],
        ),
    ];
    for (what, args) in cases {
        let (tmp, server) = fixture(&["docker"]);
        let pane = server.pane();
        let docker = run_in_front(&server, tmp.path(), &pane, "stop", "docker", &args);
        watch_claude(&server, &pane);
        expect_kind(&server, &pane, "claude", what);
        assert_eq!(
            server.pane_opt(&pane, "@agent_container"),
            "docker",
            "{what}"
        );
        docker.stop();
        expect_no_agent(&server, &pane, what);
    }
}

#[test]
fn a_docker_that_follows_another_with_no_prompt_between_is_read_again() {
    // `docker run a claude; docker run b codex`, as one line: the shell hands the pane from
    // one docker to the next, and the command is `docker` throughout. Nothing prints, so the
    // watcher next looks at the pane at its identity recheck, finds `docker` in front as
    // before, and has only the pid of the process to tell it is not the same one.
    let (tmp, server) = fixture(&["docker"]);
    let pane = server.pane();
    let (first, first_docker) = command(tmp.path(), "stop-1", "docker", &box_run());
    let (second, second_docker) = command(tmp.path(), "stop-2", "docker", &codex());
    type_line(&server, &pane, &format!("{first}; {second}"));
    wait_for_command(&server, &pane, "docker");
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, "claude", "the first docker");

    // The first container stops, and the second docker starts in its place at once.
    first_docker.stop();
    expect_kind(&server, &pane, "codex", "the second docker");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");

    // It stops too, and the shell is back with nothing to run.
    second_docker.stop();
    expect_no_agent(&server, &pane, "the last container");
}

#[test]
fn a_docker_that_replaces_the_process_in_the_pane_is_read_again() {
    // `respawn-pane -k` puts a new process where the pane's shell was: no shell in between at
    // all, a new first process for the pane, and the command is `docker` before and after.
    let (tmp, server) = fixture(&["docker"]);
    let pane = server.pane();
    run_in_front(&server, tmp.path(), &pane, "stop-1", "docker", &box_run());
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, "claude", "the first docker");

    let shell = server.display(&pane, "#{pane_pid}");
    let bin_dir = tmp.path().join("fake-bin");
    let docker = bin_dir.join("docker").display().to_string();
    let (line, _second) = command(tmp.path(), "stop-2", &docker, &codex());
    server.tmux_ok(&["respawn-pane", "-k", "-t", &pane, &line]);
    assert_ne!(
        server.display(&pane, "#{pane_pid}"),
        shell,
        "the pane has a new first process"
    );
    expect_kind(
        &server,
        &pane,
        "codex",
        "the docker that replaced the first",
    );
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");
}

#[test]
fn docker_running_something_else_stays_a_plain_program() {
    // Each of these is a docker in front of the pane with the screen of a Claude prompt box
    // behind it, and none runs an agent: a listing, a web server whose mount and name are
    // called like agents, a log followed, the values of flags, an image called claude, an
    // argument of another program, a script that only says the word, a comment that says
    // it, and a value that holds a `;` and the word. The screen alone never makes an agent.
    let cases: [Vec<&str>; 11] = [
        vec!["ps", "-a"],
        vec![
            "run",
            "-d",
            "--name",
            "claude",
            "-v",
            "/home/you/hermes:/home/you/hermes",
            "-w",
            "/home/you/hermes",
            "nginx",
        ],
        vec!["logs", "-f", "claude"],
        vec![
            "run",
            "--rm",
            "-it",
            "-v",
            "/h/.claude:/h/.claude",
            "-e",
            "CLAUDE_CONFIG_DIR=/h/.claude",
            "ubuntu",
            "bash",
            "-lc",
            "echo claude",
        ],
        vec![
            "run",
            "--hostname",
            "claude",
            "--name",
            "claude",
            "-e",
            "AGENT=claude",
            "-v",
            "~/.claude:/root/.claude",
            "nginx",
        ],
        vec!["run", "-it", "claude"],
        vec!["run", "--rm", "ubuntu", "echo", "claude"],
        vec!["run", "-e", "X=a;claude", "-e", "MSG=hello claude", "nginx"],
        vec!["run", "--rm", "--entrypoint", "which", "img", "claude"],
        vec![
            "run",
            "--rm",
            "ubuntu",
            "bash",
            "/entrypoint.sh",
            "-c",
            "claude",
        ],
        vec![
            "run",
            "--rm",
            "ubuntu",
            "bash",
            "-lc",
            "true # note; claude",
        ],
    ];
    for args in cases {
        let (tmp, server) = fixture(&["docker"]);
        let pane = server.pane();
        run_in_front(&server, tmp.path(), &pane, "stop", "docker", &args);
        watch_claude(&server, &pane);

        // The watcher has looked at the pane and decided there is no agent in it.
        assert!(
            wait_for(
                || watcher_logs(&server).contains("decision: NoAgent"),
                SETTLE
            ),
            "the watcher never decided on {args:?}\n--- log ---\n{}",
            watcher_logs(&server)
        );
        assert_eq!(server.pane_opt(&pane, "@agent_kind"), "", "{args:?}");
        assert_eq!(server.pane_opt(&pane, "@agent_state"), "", "{args:?}");
        assert_eq!(server.pane_opt(&pane, "@agent_container"), "", "{args:?}");
    }
}

/// `runtime` in front of the pane with `args`: the pane takes `kind`, and records which
/// runtime it runs under.
fn recognises(runtime: &str, args: &[&str], kind: &str) {
    let (tmp, server) = fixture(&[runtime]);
    let pane = server.pane();
    run_in_front(&server, tmp.path(), &pane, "stop", runtime, args);
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, kind, &format!("{runtime} {args:?}"));
    assert_eq!(server.pane_opt(&pane, "@agent_container"), runtime);
}

#[test]
fn podman_running_codex_is_recognised() {
    recognises(
        "podman",
        &[
            "run",
            "--rm",
            "-it",
            "-v",
            "/src:/src",
            "dev-image",
            "codex",
        ],
        "codex",
    );
}

#[test]
fn nerdctl_running_gemini_is_recognised() {
    recognises(
        "nerdctl",
        &["run", "-it", "--rm", "img", "gemini", "--yolo"],
        "gemini",
    );
}

#[test]
fn apples_container_running_opencode_is_recognised() {
    recognises(
        "container",
        &[
            "run",
            "-it",
            "--rm",
            "--volume",
            "/a:/a",
            "ubuntu",
            "bash",
            "-lc",
            "cd /a && opencode",
        ],
        "opencode",
    );
}

#[test]
fn sbx_running_claude_is_recognised() {
    recognises("sbx", &["run", "claude", "/home/you/project"], "claude");
}

#[test]
fn docker_desktops_cli_is_recognised_and_recorded_as_docker() {
    // Docker Desktop for Mac installs `docker` as a link to a program called `com.docker.cli`,
    // and tmux names a process by the program: the process is started as `docker`, and the
    // pane's command is `com.docker.cli`.
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    fake_runtime(&bin_dir, "com.docker.cli");
    std::os::unix::fs::symlink("com.docker.cli", bin_dir.join("docker")).unwrap();
    let server = server_with(bin_dir);
    let pane = server.pane();
    let (line, _docker) = command(tmp.path(), "stop", "docker", &box_run());
    type_line(&server, &pane, &line);
    wait_for_command(&server, &pane, "com.docker.cli");
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, "claude", "Docker Desktop's CLI");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");
}

#[test]
fn the_marker_leaves_with_the_runtime_while_the_agent_is_kept_through_its_misses() {
    // The container stops and a program that is no shell and no agent takes the pane at
    // once: the runtime is not in front any more, so the pane no longer says it runs under
    // one, but the agent stays as a record does through the reads that miss it (herdr's
    // confirmations), and goes when the shell is back.
    let (tmp, server) = fixture(&["docker", "vim"]);
    let pane = server.pane();
    let (first, docker) = command(tmp.path(), "stop-docker", "docker", &box_run());
    let (second, vim) = command(tmp.path(), "stop-vim", "vim", &[]);
    type_line(&server, &pane, &format!("{first}; {second}"));
    wait_for_command(&server, &pane, "docker");
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, "claude", "the docker");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");

    docker.stop();
    wait_for_command(&server, &pane, "vim");
    assert!(
        wait_for(
            || server.pane_opt(&pane, "@agent_container").is_empty(),
            SETTLE
        ),
        "the marker stayed after the runtime left (kind {:?})\n--- log ---\n{}",
        server.pane_opt(&pane, "@agent_kind"),
        watcher_logs(&server)
    );
    assert_eq!(
        server.pane_opt(&pane, "@agent_kind"),
        "claude",
        "the agent is kept through the misses, only the marker went"
    );
    assert!(!server.pane_opt(&pane, "@agent_state").is_empty());

    // The shell is back: the agent goes too.
    vim.stop();
    expect_no_agent(&server, &pane, "the shell is back");
}

#[test]
fn the_marker_is_written_before_the_kind() {
    // A reader that sees the agent (the popup and the CLI take `@agent_kind` for one) must
    // see where it runs. tmux runs a hook right after each `set-option`, so the hook records
    // both options as each write left them, in order, and the first record that holds a kind
    // holds the marker too.
    let (tmp, server) = fixture(&["docker"]);
    let pane = server.pane();
    server.tmux_ok(&[
        "set-hook",
        "-g",
        "after-set-option",
        "set-option -F -ga @writes '[#{@agent_container}|#{@agent_kind}]'",
    ]);
    run_in_front(&server, tmp.path(), &pane, "stop", "docker", &box_run());
    watch_claude(&server, &pane);
    expect_kind(&server, &pane, "claude", "the docker");
    let writes = server.global_opt("@writes");
    let first_with_kind = writes
        .split(']')
        .filter_map(|record| record.strip_prefix('['))
        .find(|record| !record.ends_with('|'));
    assert_eq!(
        first_with_kind,
        Some("docker|claude"),
        "the first write that held a kind did not hold the marker: {writes}"
    );
    assert!(writes.contains("[docker|]"), "{writes}");
}

/// What the watcher's log says when it gives up on reading a runtime's arguments.
const GIVING_UP: &str = "could not read what docker was started with";

/// A server like [`fixture`]'s, with a `ps` first on the watcher's `PATH` that refuses to
/// answer while the file the returned path names exists, and is the real one otherwise: the
/// calls the watcher makes to find who is in front of the pane fail and stop failing as the
/// test says. The tmux server's environment is what every watcher it starts inherits.
fn fixture_with_a_ps_that_can_be_broken() -> (tempfile::TempDir, Server, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("fake-bin");
    fake_runtime(&bin_dir, "docker");
    let broken = tmp.path().join("ps-broken");
    let ps_dir = tmp.path().join("fake-ps");
    write_script(
        &ps_dir.join("ps"),
        &format!(
            "[ -e '{}' ] && exit 1\nfor ps in /bin/ps /usr/bin/ps; do\n  [ -x \"$ps\" ] && exec \"$ps\" \"$@\"\ndone\nexit 1\n",
            broken.display()
        ),
    );
    let path = format!(
        "{}:{}",
        ps_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let server = Server::start_with(ServerOptions {
        path_prefix: Some(bin_dir),
        env: vec![
            ("TAW_LOG".to_string(), "debug".to_string()),
            ("PATH".to_string(), path),
            // Since the system read, only the ps fallback can fail the way this `ps` breaks it.
            ("TAW_FOREGROUND_BY_PS".to_string(), "1".to_string()),
        ],
        ..ServerOptions::default()
    });
    server.write_config(CONFIG);
    (tmp, server, broken)
}

#[test]
fn a_runtime_that_could_not_be_looked_into_is_looked_at_again_though_the_pane_prints_nothing() {
    // The first look finds `ps` unwilling, so the watcher cannot say who is in front of the
    // pane, and nothing prints after that: a watcher that looked at a pane only when it
    // printed would never come back to it. It comes back at its identity recheck.
    let (tmp, server, broken) = fixture_with_a_ps_that_can_be_broken();
    let pane = server.pane();
    std::fs::write(&broken, "").unwrap();
    run_in_front(&server, tmp.path(), &pane, "stop", "docker", &box_run());
    watch_claude(&server, &pane);
    assert!(
        wait_for(
            || watcher_logs(&server).contains("decision: NoAgent"),
            SETTLE
        ),
        "the watcher never looked at the pane\n--- log ---\n{}",
        watcher_logs(&server)
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");

    // `ps` answers again, and nothing is drawn from here on.
    std::fs::remove_file(&broken).unwrap();
    expect_kind(
        &server,
        &pane,
        "claude",
        "the recheck reads the runtime once ps answers",
    );
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");
}

#[test]
fn a_runtime_that_cannot_be_read_is_given_up_on_in_the_log_once_and_tried_again_later() {
    // `ps` refuses every call, so the watcher can never say who is in front. The pane keeps
    // printing, so it is looked at at every tick, and the process in front is asked for at
    // most once a second: five tries take four seconds, not one and a half. The fifth says so
    // in the log, once, without the arguments, and the runtime is tried again at the
    // recheck, which finds the agent when `ps` answers again.
    let (tmp, server, broken) = fixture_with_a_ps_that_can_be_broken();
    let pane = server.pane();
    std::fs::write(&broken, "").unwrap();
    // A token among the arguments, which no log may hold.
    let args = box_with(&["-e", "API_TOKEN=sk-secret-4f9d"], BOX_SCRIPT);
    run_in_front(&server, tmp.path(), &pane, "stop", "docker", &args);
    let gave_up = |server: &Server| watcher_logs(server).matches(GIVING_UP).count();

    let started = Instant::now();
    watch_claude(&server, &pane);
    while gave_up(&server) == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the watcher never gave up on the runtime\n--- log ---\n{}",
            watcher_logs(&server)
        );
        draw(&server, &pane, "\u{2733} Claude Code", IDLE);
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        started.elapsed() >= Duration::from_millis(3500),
        "five tries a second apart at the least, and it gave up after {:?}",
        started.elapsed()
    );
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");

    // Said once, though the watcher goes on looking at the pane, and tries the runtime again
    // at the recheck, which still finds `ps` unwilling.
    for _ in 0..26 {
        draw(&server, &pane, "\u{2733} Claude Code", IDLE);
        std::thread::sleep(Duration::from_millis(250));
    }
    assert_eq!(gave_up(&server), 1, "{}", watcher_logs(&server));
    assert_eq!(server.pane_opt(&pane, "@agent_kind"), "");

    // `ps` answers again and nothing is drawn: the next try reads the arguments.
    std::fs::remove_file(&broken).unwrap();
    expect_kind(&server, &pane, "claude", "tried again at the recheck");
    assert_eq!(server.pane_opt(&pane, "@agent_container"), "docker");
    assert_eq!(gave_up(&server), 1);
    assert!(
        !watcher_logs(&server).contains("sk-secret-4f9d"),
        "the log holds the arguments:\n{}",
        watcher_logs(&server)
    );
}
